//! Length-prefixed MessagePack codec for tokio I/O.
//!
//! Framing: `[4 bytes: payload length, big-endian u32][N bytes: MessagePack payload]`
//!
//! See CONTRACT_BROKER.md §Wire Protocol §Framing.

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use super::protocol::{MAX_PAYLOAD_SIZE, Message, RawEnvelope};

/// Codec error type.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("payload too large: {0} bytes (max {MAX_PAYLOAD_SIZE})")]
    PayloadTooLarge(usize),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("MessagePack encode error: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("MessagePack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

/// Length-prefixed MessagePack codec.
///
/// Encodes [`Message`] values as length-prefixed MessagePack frames.
/// Decodes length-prefixed frames back into [`Message`] values.
///
/// Enforces the 16 MiB maximum payload size from CONTRACT_BROKER.md.
///
/// Used by client code (PTY wrappers, hotkey clients) for simple
/// send/receive. The broker itself uses [`FrameCodec`] + [`decode_frame`]
/// for two-phase decode with unknown-type fallback.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct LengthPrefixedCodec {
    /// Length of the current frame being read, if the header has been consumed.
    pending_len: Option<usize>,
}

#[allow(dead_code)]
impl LengthPrefixedCodec {
    pub fn new() -> Self {
        Self { pending_len: None }
    }
}

impl Decoder for LengthPrefixedCodec {
    type Item = Message;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // Read the 4-byte length header if we haven't yet.
        let payload_len = match self.pending_len {
            Some(len) => len,
            None => {
                if src.len() < 4 {
                    return Ok(None); // Need more data for the header.
                }
                let len = src.get_u32() as usize;
                if len > MAX_PAYLOAD_SIZE {
                    return Err(CodecError::PayloadTooLarge(len));
                }
                self.pending_len = Some(len);
                len
            }
        };

        // Wait for the full payload.
        if src.len() < payload_len {
            // Reserve space for the remaining bytes to avoid repeated
            // small allocations.
            src.reserve(payload_len - src.len());
            return Ok(None);
        }

        // Extract and decode the payload.
        let payload = src.split_to(payload_len);
        self.pending_len = None;

        let msg: Message = rmp_serde::from_slice(&payload)?;
        Ok(Some(msg))
    }
}

impl Encoder<Message> for LengthPrefixedCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let payload = rmp_serde::to_vec_named(&item)?;

        if payload.len() > MAX_PAYLOAD_SIZE {
            return Err(CodecError::PayloadTooLarge(payload.len()));
        }

        dst.reserve(4 + payload.len());
        dst.put_u32(payload.len() as u32);
        dst.extend_from_slice(&payload);
        Ok(())
    }
}

/// Frame-level codec — handles only length-prefixed framing.
///
/// Returns raw `BytesMut` payloads without deserializing. Used by the
/// connection layer for two-phase decode: try [`Message`], then fall
/// back to [`RawEnvelope`] for unknown-type error responses.
#[derive(Debug, Default)]
pub struct FrameCodec {
    /// Length of the current frame being read, if the header has been consumed.
    pending_len: Option<usize>,
}

impl FrameCodec {
    pub fn new() -> Self {
        Self { pending_len: None }
    }
}

impl Decoder for FrameCodec {
    type Item = BytesMut;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let payload_len = match self.pending_len {
            Some(len) => len,
            None => {
                if src.len() < 4 {
                    return Ok(None);
                }
                let len = src.get_u32() as usize;
                if len > MAX_PAYLOAD_SIZE {
                    return Err(CodecError::PayloadTooLarge(len));
                }
                self.pending_len = Some(len);
                len
            }
        };

        if src.len() < payload_len {
            src.reserve(payload_len - src.len());
            return Ok(None);
        }

        let payload = src.split_to(payload_len);
        self.pending_len = None;
        Ok(Some(payload))
    }
}

impl Encoder<Message> for FrameCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let payload = rmp_serde::to_vec_named(&item)?;
        if payload.len() > MAX_PAYLOAD_SIZE {
            return Err(CodecError::PayloadTooLarge(payload.len()));
        }
        dst.reserve(4 + payload.len());
        dst.put_u32(payload.len() as u32);
        dst.extend_from_slice(&payload);
        Ok(())
    }
}

/// Result of attempting to decode a raw frame into a protocol message.
#[derive(Debug)]
pub enum DecodeResult {
    /// Successfully decoded a known message variant.
    Ok(Message),
    /// Unknown type — extracted envelope for error response echoing.
    UnknownType(RawEnvelope),
    /// Completely malformed — could not even extract `{type, id}`.
    Malformed(rmp_serde::decode::Error),
}

/// Attempt two-phase decode of a raw frame.
///
/// 1. Try to deserialize as [`Message`] (known variant).
/// 2. On failure, try [`RawEnvelope`] to extract `{type, id}`.
/// 3. If both fail, return [`DecodeResult::Malformed`].
pub fn decode_frame(payload: &[u8]) -> DecodeResult {
    match rmp_serde::from_slice::<Message>(payload) {
        Ok(msg) => DecodeResult::Ok(msg),
        Err(_) => match rmp_serde::from_slice::<RawEnvelope>(payload) {
            Ok(envelope) => DecodeResult::UnknownType(envelope),
            Err(e) => DecodeResult::Malformed(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::*;

    fn encode_message(msg: &Message) -> BytesMut {
        let mut codec = LengthPrefixedCodec::new();
        let mut buf = BytesMut::new();
        codec.encode(msg.clone(), &mut buf).unwrap();
        buf
    }

    fn decode_message(buf: &mut BytesMut) -> Option<Message> {
        let mut codec = LengthPrefixedCodec::new();
        codec.decode(buf).unwrap()
    }

    #[test]
    fn round_trip_through_codec() {
        let msg = Message::Hello {
            id: 0,
            version: PROTOCOL_VERSION,
            role: Role::Wrapper,
        };

        let mut buf = encode_message(&msg);
        let decoded = decode_message(&mut buf).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn round_trip_all_variants() {
        let messages = vec![
            Message::Hello {
                id: 0,
                version: 1,
                role: Role::Client,
            },
            Message::HelloAck {
                id: 0,
                status: Status::Ok,
                error: None,
            },
            Message::Register {
                id: 1,
                session: "s1".into(),
                pid: 42,
                pattern: "generic".into(),
            },
            Message::Deregister {
                id: 2,
                session: "s1".into(),
            },
            Message::TurnCompleted {
                id: 3,
                session: "s1".into(),
                content: b"turn content".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            Message::Capture {
                id: 4,
                session: "s1".into(),
            },
            Message::Paste {
                id: 5,
                session: "s1".into(),
            },
            Message::Inject {
                id: 0,
                content: b"inject bytes".to_vec(),
            },
            Message::ListSessions { id: 6 },
            Message::Response {
                id: 1,
                status: Status::Ok,
                error: None,
                size: Some(100),
                sessions: None,
                turn_id: None,
            },
        ];

        for msg in &messages {
            let mut buf = encode_message(msg);
            let decoded = decode_message(&mut buf).unwrap();
            assert_eq!(&decoded, msg, "round-trip failed for {msg:?}");
        }
    }

    #[test]
    fn partial_header_returns_none() {
        let mut codec = LengthPrefixedCodec::new();
        // Only 2 bytes of the 4-byte header.
        let mut buf = BytesMut::from(&[0u8, 0][..]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn partial_payload_returns_none() {
        let msg = Message::ListSessions { id: 1 };
        let mut full = encode_message(&msg);

        // Take only the header + half the payload.
        let half = full.len() / 2;
        let mut partial = full.split_to(half);

        let mut codec = LengthPrefixedCodec::new();
        assert!(codec.decode(&mut partial).unwrap().is_none());

        // Feed the rest.
        partial.extend_from_slice(&full);
        let decoded = codec.decode(&mut partial).unwrap().unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn multiple_messages_in_buffer() {
        let msg1 = Message::ListSessions { id: 1 };
        let msg2 = Message::Capture {
            id: 2,
            session: "s1".into(),
        };

        let mut buf = BytesMut::new();
        let mut codec = LengthPrefixedCodec::new();
        codec.encode(msg1.clone(), &mut buf).unwrap();
        codec.encode(msg2.clone(), &mut buf).unwrap();

        let mut codec = LengthPrefixedCodec::new();
        let decoded1 = codec.decode(&mut buf).unwrap().unwrap();
        let decoded2 = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(decoded1, msg1);
        assert_eq!(decoded2, msg2);
    }

    #[test]
    fn binary_content_fidelity() {
        let content: Vec<u8> = (0..=255).collect();
        let msg = Message::TurnCompleted {
            id: 1,
            session: "s".into(),
            content: content.clone(),
            interrupted: true,
            timestamp: 1000,
        };

        let mut buf = encode_message(&msg);
        let decoded = decode_message(&mut buf).unwrap();
        match decoded {
            Message::TurnCompleted {
                content: decoded_content,
                ..
            } => {
                assert_eq!(decoded_content, content);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn payload_too_large_on_decode() {
        let mut buf = BytesMut::new();
        // Write a length header claiming 17 MiB.
        buf.put_u32((17 * 1024 * 1024) as u32);
        buf.extend_from_slice(&[0u8; 100]); // some dummy data

        let mut codec = LengthPrefixedCodec::new();
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::PayloadTooLarge(_)));
    }

    #[test]
    fn empty_buffer_returns_none() {
        let mut codec = LengthPrefixedCodec::new();
        let mut buf = BytesMut::new();
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn frame_length_header_is_big_endian() {
        let msg = Message::ListSessions { id: 0 };
        let buf = encode_message(&msg);

        // Read the first 4 bytes as big-endian u32.
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        // The remaining bytes should be exactly that length.
        assert_eq!(buf.len() - 4, len);
    }
}

//! Wire protocol message types for broker IPC.
//!
//! All messages are MessagePack-encoded maps with at minimum `type` and `id`
//! fields. See CONTRACT_BROKER.md §Wire Protocol.

use serde::{Deserialize, Serialize};

/// All wire protocol messages.
///
/// Serialized as a tagged union on the `type` field via MessagePack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Message {
    // -- Handshake --
    #[serde(rename = "hello")]
    Hello { id: u32, version: u32, role: Role },

    #[serde(rename = "hello_ack")]
    HelloAck {
        id: u32,
        status: Status,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    // -- Session management --
    #[serde(rename = "register")]
    Register {
        id: u32,
        session: String,
        pid: u32,
        pattern: String,
    },

    #[serde(rename = "deregister")]
    Deregister { id: u32, session: String },

    // -- Turn storage --
    #[serde(rename = "turn_completed")]
    TurnCompleted {
        id: u32,
        session: String,
        #[serde(with = "serde_bytes")]
        content: Vec<u8>,
        interrupted: bool,
    },

    // -- Capture / Paste --
    #[serde(rename = "capture")]
    Capture { id: u32, session: String },

    #[serde(rename = "paste")]
    Paste { id: u32, session: String },

    // -- Unsolicited commands (broker → wrapper) --
    #[serde(rename = "inject")]
    Inject {
        id: u32,
        #[serde(with = "serde_bytes")]
        content: Vec<u8>,
    },

    // -- Query --
    #[serde(rename = "list_sessions")]
    ListSessions { id: u32 },

    // -- Generic response --
    #[serde(rename = "response")]
    Response {
        id: u32,
        status: Status,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sessions: Option<Vec<SessionDescriptor>>,
    },
}

/// Client role in the handshake.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Wrapper,
    Client,
}

/// Response status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Error,
}

/// Session descriptor returned in list_sessions responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionDescriptor {
    pub session: String,
    pub pid: u32,
    pub has_turn: bool,
}

/// Protocol version for v0.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum payload size (16 MiB).
pub const MAX_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;

/// Minimal envelope for extracting `{type, id}` from unknown messages.
///
/// Used by the broker as a fallback when [`Message`] deserialization
/// fails (e.g., unknown `type` tag). Allows the broker to echo the
/// request `id` in the error response per CONTRACT_BROKER.md §129.
#[derive(Debug, Deserialize)]
pub struct RawEnvelope {
    /// Consumed by serde for structural matching; not read by broker code.
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub msg_type: String,
    pub id: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: &Message) -> Message {
        let encoded = rmp_serde::to_vec_named(msg).unwrap();
        rmp_serde::from_slice(&encoded).unwrap()
    }

    #[test]
    fn hello_round_trip() {
        let msg = Message::Hello {
            id: 0,
            version: PROTOCOL_VERSION,
            role: Role::Wrapper,
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn hello_ack_ok_round_trip() {
        let msg = Message::HelloAck {
            id: 0,
            status: Status::Ok,
            error: None,
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn hello_ack_error_round_trip() {
        let msg = Message::HelloAck {
            id: 0,
            status: Status::Error,
            error: Some("version_mismatch".into()),
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn register_round_trip() {
        let msg = Message::Register {
            id: 1,
            session: "abc-123".into(),
            pid: 4567,
            pattern: "generic".into(),
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn deregister_round_trip() {
        let msg = Message::Deregister {
            id: 2,
            session: "abc-123".into(),
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn turn_completed_round_trip() {
        let msg = Message::TurnCompleted {
            id: 3,
            session: "abc-123".into(),
            content: b"hello world\nline 2\n".to_vec(),
            interrupted: false,
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn turn_completed_binary_fidelity() {
        // Ensure binary content survives round-trip without corruption.
        let binary_content: Vec<u8> = (0..=255).collect();
        let msg = Message::TurnCompleted {
            id: 4,
            session: "test".into(),
            content: binary_content.clone(),
            interrupted: true,
        };
        let decoded = round_trip(&msg);
        match decoded {
            Message::TurnCompleted { content, .. } => {
                assert_eq!(content, binary_content);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn capture_round_trip() {
        let msg = Message::Capture {
            id: 5,
            session: "abc-123".into(),
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn paste_round_trip() {
        let msg = Message::Paste {
            id: 6,
            session: "abc-123".into(),
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn inject_round_trip() {
        let msg = Message::Inject {
            id: 0,
            content: b"injected data".to_vec(),
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn list_sessions_round_trip() {
        let msg = Message::ListSessions { id: 7 };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn response_ok_round_trip() {
        let msg = Message::Response {
            id: 1,
            status: Status::Ok,
            error: None,
            size: None,
            sessions: None,
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn response_with_size_round_trip() {
        let msg = Message::Response {
            id: 5,
            status: Status::Ok,
            error: None,
            size: Some(1024),
            sessions: None,
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn response_with_sessions_round_trip() {
        let msg = Message::Response {
            id: 7,
            status: Status::Ok,
            error: None,
            size: None,
            sessions: Some(vec![
                SessionDescriptor {
                    session: "s1".into(),
                    pid: 100,
                    has_turn: true,
                },
                SessionDescriptor {
                    session: "s2".into(),
                    pid: 200,
                    has_turn: false,
                },
            ]),
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn response_error_round_trip() {
        let msg = Message::Response {
            id: 1,
            status: Status::Error,
            error: Some("session_not_found".into()),
            size: None,
            sessions: None,
        };
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn role_serialization() {
        let wrapper = rmp_serde::to_vec_named(&Role::Wrapper).unwrap();
        let decoded: Role = rmp_serde::from_slice(&wrapper).unwrap();
        assert_eq!(decoded, Role::Wrapper);

        let client = rmp_serde::to_vec_named(&Role::Client).unwrap();
        let decoded: Role = rmp_serde::from_slice(&client).unwrap();
        assert_eq!(decoded, Role::Client);
    }

    #[test]
    fn status_serialization() {
        let ok = rmp_serde::to_vec_named(&Status::Ok).unwrap();
        let decoded: Status = rmp_serde::from_slice(&ok).unwrap();
        assert_eq!(decoded, Status::Ok);

        let err = rmp_serde::to_vec_named(&Status::Error).unwrap();
        let decoded: Status = rmp_serde::from_slice(&err).unwrap();
        assert_eq!(decoded, Status::Error);
    }
}

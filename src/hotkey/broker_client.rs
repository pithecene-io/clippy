//! Broker client for the hotkey client.
//!
//! Connects to the broker daemon as `Role::Client`, performs the
//! handshake, and provides methods for list_sessions, capture, and
//! paste operations. See CONTRACT_HOTKEY.md §184–192,
//! CONTRACT_BROKER.md §Wire Protocol.

use std::path::PathBuf;

use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use crate::ipc::codec::LengthPrefixedCodec;
use crate::ipc::protocol::{Message, PROTOCOL_VERSION, Role, SessionDescriptor, Status};

use super::HotkeyError;

/// Broker client for the hotkey client.
///
/// Simpler than the PTY wrapper's client — no split sink/stream needed
/// because actions are serialized (CONTRACT_HOTKEY.md §199). Uses
/// simple request-response: `send()` then `next()`.
pub struct BrokerClient {
    framed: Framed<UnixStream, LengthPrefixedCodec>,
    next_id: u32,
}

impl BrokerClient {
    /// Connect to the broker and perform the handshake.
    ///
    /// Returns `Err` if the broker is unreachable or the handshake fails.
    /// The hotkey client MUST exit on failure — it does not operate
    /// independently of the broker (CONTRACT_HOTKEY.md §191-192).
    pub async fn connect() -> Result<Self, HotkeyError> {
        let socket_path = resolve_socket_path()?;

        let stream = UnixStream::connect(&socket_path)
            .await
            .map_err(|e| HotkeyError::Broker(format!("connect failed: {e}")))?;
        let mut framed = Framed::new(stream, LengthPrefixedCodec::new());

        // Handshake: Hello → HelloAck.
        framed
            .send(Message::Hello {
                id: 0,
                version: PROTOCOL_VERSION,
                role: Role::Client,
            })
            .await
            .map_err(|e| HotkeyError::Broker(format!("send hello: {e}")))?;

        match framed.next().await {
            Some(Ok(Message::HelloAck {
                status: Status::Ok, ..
            })) => {}
            Some(Ok(Message::HelloAck {
                status: Status::Error,
                error,
                ..
            })) => {
                return Err(HotkeyError::Broker(format!(
                    "handshake rejected: {}",
                    error.unwrap_or_default()
                )));
            }
            other => {
                return Err(HotkeyError::Broker(format!(
                    "unexpected handshake response: {other:?}"
                )));
            }
        }

        Ok(Self {
            framed,
            next_id: 1, // 0 = Hello
        })
    }

    /// List all active sessions.
    ///
    /// Returns session descriptors including PID and has_turn flag.
    pub async fn list_sessions(&mut self) -> Result<Vec<SessionDescriptor>, HotkeyError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::ListSessions { id })
            .await
            .map_err(|e| HotkeyError::Broker(format!("send list_sessions: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok,
                sessions: Some(sessions),
                ..
            })) => Ok(sessions),
            Some(Ok(Message::Response {
                status: Status::Ok,
                sessions: None,
                ..
            })) => Ok(Vec::new()),
            Some(Ok(Message::Response { error, .. })) => Err(HotkeyError::Broker(format!(
                "list_sessions failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(HotkeyError::Broker(format!(
                "unexpected list_sessions response: {other:?}"
            ))),
        }
    }

    /// Capture the latest turn from a session into the relay buffer.
    ///
    /// Returns the byte size of the captured content on success.
    pub async fn capture(&mut self, session: &str) -> Result<u32, HotkeyError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::Capture {
                id,
                session: session.to_string(),
            })
            .await
            .map_err(|e| HotkeyError::Broker(format!("send capture: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok,
                size: Some(size),
                ..
            })) => Ok(size),
            Some(Ok(Message::Response { error, .. })) => Err(HotkeyError::Broker(format!(
                "capture failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(HotkeyError::Broker(format!(
                "unexpected capture response: {other:?}"
            ))),
        }
    }

    /// Deliver relay buffer content to the clipboard sink.
    pub async fn deliver_clipboard(&mut self) -> Result<(), HotkeyError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::Deliver {
                id,
                sink: "clipboard".into(),
                session: None,
                path: None,
            })
            .await
            .map_err(|e| HotkeyError::Broker(format!("send deliver_clipboard: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok, ..
            })) => Ok(()),
            Some(Ok(Message::Response { error, .. })) => Err(HotkeyError::Broker(format!(
                "deliver_clipboard failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(HotkeyError::Broker(format!(
                "unexpected deliver_clipboard response: {other:?}"
            ))),
        }
    }

    /// Paste relay buffer content to a session (inject into its PTY).
    pub async fn paste(&mut self, session: &str) -> Result<(), HotkeyError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::Paste {
                id,
                session: session.to_string(),
            })
            .await
            .map_err(|e| HotkeyError::Broker(format!("send paste: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok, ..
            })) => Ok(()),
            Some(Ok(Message::Response { error, .. })) => Err(HotkeyError::Broker(format!(
                "paste failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(HotkeyError::Broker(format!(
                "unexpected paste response: {other:?}"
            ))),
        }
    }
}

/// Resolve the broker socket path from `$XDG_RUNTIME_DIR`.
fn resolve_socket_path() -> Result<PathBuf, HotkeyError> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| HotkeyError::Broker("$XDG_RUNTIME_DIR not set".into()))?;
    Ok(PathBuf::from(runtime_dir)
        .join("clippy")
        .join("broker.sock"))
}

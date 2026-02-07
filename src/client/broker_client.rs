//! Broker client for the CLI client.
//!
//! Connects to the broker daemon as `Role::Client`, performs the
//! handshake, and provides methods for all v0 and v1 operations.
//! Follows the same pattern as `hotkey::broker_client`.

use std::path::PathBuf;

use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use crate::ipc::codec::LengthPrefixedCodec;
use crate::ipc::protocol::{
    Message, PROTOCOL_VERSION, Role, SessionDescriptor, Status, TurnDescriptor,
};

use super::ClientError;

/// Result of a capture or capture-by-id operation.
pub struct CaptureResult {
    pub turn_id: String,
    pub size: u32,
}

/// Result of a get-turn operation.
pub struct GetTurnResult {
    pub content: Vec<u8>,
    pub timestamp: u64,
    pub byte_length: u32,
    pub interrupted: bool,
    pub truncated: bool,
}

/// Broker client for one-shot CLI commands.
///
/// Simpler than the PTY wrapper's client — no split sink/stream needed
/// because each CLI invocation performs a single request-response cycle.
pub struct BrokerClient {
    framed: Framed<UnixStream, LengthPrefixedCodec>,
    next_id: u32,
}

impl BrokerClient {
    /// Connect to the broker and perform the handshake.
    pub async fn connect() -> Result<Self, ClientError> {
        let socket_path = resolve_socket_path()?;

        let stream = UnixStream::connect(&socket_path)
            .await
            .map_err(|e| ClientError::Broker(format!("connect failed: {e}")))?;
        let mut framed = Framed::new(stream, LengthPrefixedCodec::new());

        // Handshake: Hello → HelloAck.
        framed
            .send(Message::Hello {
                id: 0,
                version: PROTOCOL_VERSION,
                role: Role::Client,
            })
            .await
            .map_err(|e| ClientError::Broker(format!("send hello: {e}")))?;

        match framed.next().await {
            Some(Ok(Message::HelloAck {
                status: Status::Ok, ..
            })) => {}
            Some(Ok(Message::HelloAck {
                status: Status::Error,
                error,
                ..
            })) => {
                return Err(ClientError::Broker(format!(
                    "handshake rejected: {}",
                    error.unwrap_or_default()
                )));
            }
            other => {
                return Err(ClientError::Broker(format!(
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
    pub async fn list_sessions(&mut self) -> Result<Vec<SessionDescriptor>, ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::ListSessions { id })
            .await
            .map_err(|e| ClientError::Broker(format!("send list_sessions: {e}")))?;

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
            Some(Ok(Message::Response { error, .. })) => Err(ClientError::Broker(format!(
                "list_sessions failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(ClientError::Broker(format!(
                "unexpected list_sessions response: {other:?}"
            ))),
        }
    }

    /// Capture the latest turn from a session into the relay buffer.
    pub async fn capture(&mut self, session: &str) -> Result<CaptureResult, ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::Capture {
                id,
                session: session.to_string(),
            })
            .await
            .map_err(|e| ClientError::Broker(format!("send capture: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok,
                turn_id: Some(turn_id),
                size: Some(size),
                ..
            })) => Ok(CaptureResult { turn_id, size }),
            Some(Ok(Message::Response { error, .. })) => Err(ClientError::Broker(format!(
                "capture failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(ClientError::Broker(format!(
                "unexpected capture response: {other:?}"
            ))),
        }
    }

    /// Paste relay buffer content to a session (inject into its PTY).
    pub async fn paste(&mut self, session: &str) -> Result<(), ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::Paste {
                id,
                session: session.to_string(),
            })
            .await
            .map_err(|e| ClientError::Broker(format!("send paste: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok, ..
            })) => Ok(()),
            Some(Ok(Message::Response { error, .. })) => Err(ClientError::Broker(format!(
                "paste failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(ClientError::Broker(format!(
                "unexpected paste response: {other:?}"
            ))),
        }
    }

    /// Get a turn's content and metadata by ID.
    pub async fn get_turn(&mut self, turn_id: &str) -> Result<GetTurnResult, ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::GetTurn {
                id,
                turn_id: turn_id.to_string(),
            })
            .await
            .map_err(|e| ClientError::Broker(format!("send get_turn: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok,
                content: Some(content),
                timestamp: Some(timestamp),
                byte_length: Some(byte_length),
                interrupted: Some(interrupted),
                truncated: Some(truncated),
                ..
            })) => Ok(GetTurnResult {
                content,
                timestamp,
                byte_length,
                interrupted,
                truncated,
            }),
            Some(Ok(Message::Response { error, .. })) => Err(ClientError::Broker(format!(
                "get_turn failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(ClientError::Broker(format!(
                "unexpected get_turn response: {other:?}"
            ))),
        }
    }

    /// List recent turns for a session.
    pub async fn list_turns(
        &mut self,
        session: &str,
        limit: Option<u32>,
    ) -> Result<Vec<TurnDescriptor>, ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::ListTurns {
                id,
                session: session.to_string(),
                limit,
            })
            .await
            .map_err(|e| ClientError::Broker(format!("send list_turns: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok,
                turns: Some(turns),
                ..
            })) => Ok(turns),
            Some(Ok(Message::Response {
                status: Status::Ok,
                turns: None,
                ..
            })) => Ok(Vec::new()),
            Some(Ok(Message::Response { error, .. })) => Err(ClientError::Broker(format!(
                "list_turns failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(ClientError::Broker(format!(
                "unexpected list_turns response: {other:?}"
            ))),
        }
    }

    /// Capture a specific turn by ID into the relay buffer.
    pub async fn capture_by_id(&mut self, turn_id: &str) -> Result<CaptureResult, ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::CaptureByID {
                id,
                turn_id: turn_id.to_string(),
            })
            .await
            .map_err(|e| ClientError::Broker(format!("send capture_by_id: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok,
                turn_id: Some(turn_id),
                size: Some(size),
                ..
            })) => Ok(CaptureResult { turn_id, size }),
            Some(Ok(Message::Response { error, .. })) => Err(ClientError::Broker(format!(
                "capture_by_id failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(ClientError::Broker(format!(
                "unexpected capture_by_id response: {other:?}"
            ))),
        }
    }

    /// Deliver relay buffer content to a sink.
    pub async fn deliver(
        &mut self,
        sink: &str,
        session: Option<String>,
        path: Option<String>,
    ) -> Result<(), ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        self.framed
            .send(Message::Deliver {
                id,
                sink: sink.to_string(),
                session,
                path,
            })
            .await
            .map_err(|e| ClientError::Broker(format!("send deliver: {e}")))?;

        match self.framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok, ..
            })) => Ok(()),
            Some(Ok(Message::Response { error, .. })) => Err(ClientError::Broker(format!(
                "deliver failed: {}",
                error.unwrap_or_default()
            ))),
            other => Err(ClientError::Broker(format!(
                "unexpected deliver response: {other:?}"
            ))),
        }
    }
}

/// Resolve the broker socket path from `$XDG_RUNTIME_DIR`.
fn resolve_socket_path() -> Result<PathBuf, ClientError> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| ClientError::Broker("$XDG_RUNTIME_DIR not set".into()))?;
    Ok(PathBuf::from(runtime_dir)
        .join("clippy")
        .join("broker.sock"))
}

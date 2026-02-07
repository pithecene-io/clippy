//! Broker client — connect, handshake, register, send turns, handle inject.
//!
//! The PTY wrapper acts as a broker client with `Role::Wrapper`.
//! Connection is optional — the wrapper runs standalone if the broker
//! is unreachable. See CONTRACT_PTY.md §104–123, CONTRACT_BROKER.md
//! §Wire Protocol.

use std::path::PathBuf;

use futures::stream::SplitSink;
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use crate::ipc::codec::LengthPrefixedCodec;
use crate::ipc::protocol::{Message, PROTOCOL_VERSION, Role, Status};
use crate::turn::Turn;

use super::PtyError;

/// Broker client for the PTY wrapper.
///
/// Splits the framed connection into separate sink/stream halves so
/// that `stream.next()` can be polled in the `select!` loop while
/// `sink.send()` is called from the output handler.
pub struct BrokerClient {
    sink: SplitSink<Framed<UnixStream, LengthPrefixedCodec>, Message>,
    stream: SplitStream<Framed<UnixStream, LengthPrefixedCodec>>,
    next_id: u32,
    session_id: String,
}

impl BrokerClient {
    /// Connect to the broker, perform handshake, and register the session.
    ///
    /// Returns `Err` if the broker is unreachable, handshake fails, or
    /// registration fails. The caller should log the error and continue
    /// in standalone mode.
    pub async fn connect(session_id: &str, pid: u32, pattern: &str) -> Result<Self, PtyError> {
        // Resolve socket path.
        let socket_path = resolve_socket_path()?;

        // Connect.
        let stream = UnixStream::connect(&socket_path)
            .await
            .map_err(|e| PtyError::Broker(format!("connect failed: {e}")))?;
        let mut framed = Framed::new(stream, LengthPrefixedCodec::new());

        // Handshake: Hello → HelloAck.
        framed
            .send(Message::Hello {
                id: 0,
                version: PROTOCOL_VERSION,
                role: Role::Wrapper,
            })
            .await
            .map_err(|e| PtyError::Broker(format!("send hello: {e}")))?;

        match framed.next().await {
            Some(Ok(Message::HelloAck {
                status: Status::Ok, ..
            })) => {}
            Some(Ok(Message::HelloAck {
                status: Status::Error,
                error,
                ..
            })) => {
                return Err(PtyError::Broker(format!(
                    "handshake rejected: {}",
                    error.unwrap_or_default()
                )));
            }
            other => {
                return Err(PtyError::Broker(format!(
                    "unexpected handshake response: {other:?}"
                )));
            }
        }

        // Register session.
        framed
            .send(Message::Register {
                id: 1,
                session: session_id.to_string(),
                pid,
                pattern: pattern.to_string(),
            })
            .await
            .map_err(|e| PtyError::Broker(format!("send register: {e}")))?;

        match framed.next().await {
            Some(Ok(Message::Response {
                status: Status::Ok, ..
            })) => {}
            Some(Ok(Message::Response { error, .. })) => {
                return Err(PtyError::Broker(format!(
                    "register failed: {}",
                    error.unwrap_or_default()
                )));
            }
            other => {
                return Err(PtyError::Broker(format!(
                    "unexpected register response: {other:?}"
                )));
            }
        }

        let (sink, stream) = framed.split();

        Ok(Self {
            sink,
            stream,
            next_id: 2, // 0=Hello, 1=Register
            session_id: session_id.to_string(),
        })
    }

    /// Send a completed turn to the broker (fire-and-forget).
    ///
    /// Writes the `TurnCompleted` message to the sink and returns
    /// immediately without waiting for the response ack. The ack
    /// arrives on the stream and is handled in the select! broker
    /// arm (as an ignored `Response`).
    ///
    /// This avoids blocking the I/O loop (CONTRACT_PTY.md §46, §49)
    /// and prevents inject messages from being dropped during the
    /// ack wait.
    pub async fn send_turn(&mut self, turn: &Turn) -> Result<(), PtyError> {
        let id = self.next_id;
        self.next_id += 1;

        self.sink
            .send(Message::TurnCompleted {
                id,
                session: self.session_id.clone(),
                content: turn.content.clone(),
                interrupted: turn.interrupted,
            })
            .await
            .map_err(|e| PtyError::Broker(format!("send turn: {e}")))
    }

    /// Send deregister and close the connection.
    ///
    /// Best-effort — errors are logged but not propagated since we're
    /// shutting down anyway.
    pub async fn deregister(&mut self) {
        let id = self.next_id;
        self.next_id += 1;

        if let Err(e) = self
            .sink
            .send(Message::Deregister {
                id,
                session: self.session_id.clone(),
            })
            .await
        {
            tracing::debug!(error = %e, "deregister send failed");
        }
        // Don't wait for response — we're exiting.
    }

    /// Get a mutable reference to the stream half for `select!` polling.
    ///
    /// The caller polls `stream_mut().next()` to receive unsolicited
    /// `Inject` messages from the broker.
    pub fn stream_mut(&mut self) -> &mut SplitStream<Framed<UnixStream, LengthPrefixedCodec>> {
        &mut self.stream
    }
}

/// Resolve the broker socket path from `$XDG_RUNTIME_DIR`.
fn resolve_socket_path() -> Result<PathBuf, PtyError> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| PtyError::Broker("$XDG_RUNTIME_DIR not set".into()))?;
    Ok(PathBuf::from(runtime_dir)
        .join("clippy")
        .join("broker.sock"))
}

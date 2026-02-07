//! Per-connection task — framed I/O, handshake, command forwarding.
//!
//! Each client connection spawns a tokio task that:
//! 1. Wraps the socket in a length-prefixed MessagePack codec.
//! 2. Reads the first message (must be `Hello`) and forwards it to
//!    the broker loop for handshake validation.
//! 3. Enters a select loop: forward requests to the broker loop,
//!    receive inject commands for unsolicited delivery.
//! 4. On disconnect, notifies the broker loop for cleanup.
//!
//! See CONTRACT_BROKER.md §Wire Protocol, §Handshake.

use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::Framed;

use crate::ipc::codec::{CodecError, DecodeResult, FrameCodec, decode_frame};
use crate::ipc::protocol::{Message, Status};

use super::state::ConnectionId;

/// Command sent from a connection task to the broker loop.
#[derive(Debug)]
pub struct BrokerCommand {
    pub request: Message,
    pub response_tx: oneshot::Sender<Message>,
    pub connection_id: ConnectionId,
}

/// Notification sent when a connection closes.
#[derive(Debug)]
pub struct DisconnectNotice {
    pub connection_id: ConnectionId,
}

/// Connection-level errors.
#[derive(Debug, thiserror::Error)]
enum ConnectionError {
    #[error("unexpected EOF during handshake")]
    HandshakeEof,
    #[error("first message must be Hello")]
    NotHello,
    #[error("malformed frame: {0}")]
    MalformedFrame(#[from] rmp_serde::decode::Error),
    #[error("codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("broker loop closed")]
    BrokerGone,
    #[error("response channel closed")]
    ResponseDropped,
}

/// Spawn a connection handler task.
///
/// The task runs until the client disconnects or a protocol error
/// occurs. On exit, a [`DisconnectNotice`] is sent to the broker loop.
pub fn spawn_connection(
    stream: UnixStream,
    conn_id: ConnectionId,
    cmd_tx: mpsc::UnboundedSender<BrokerCommand>,
    inject_rx: mpsc::UnboundedReceiver<Message>,
    disconnect_tx: mpsc::UnboundedSender<DisconnectNotice>,
) {
    tokio::spawn(async move {
        if let Err(e) = handle_connection(stream, conn_id, cmd_tx, inject_rx).await {
            tracing::debug!(?conn_id, error = %e, "connection closed");
        }
        // Always notify broker of disconnect for cleanup.
        let _ = disconnect_tx.send(DisconnectNotice {
            connection_id: conn_id,
        });
    });
}

async fn handle_connection(
    stream: UnixStream,
    conn_id: ConnectionId,
    cmd_tx: mpsc::UnboundedSender<BrokerCommand>,
    mut inject_rx: mpsc::UnboundedReceiver<Message>,
) -> Result<(), ConnectionError> {
    let mut framed = Framed::new(stream, FrameCodec::new());

    // -- Handshake: first message must be Hello --
    let first_frame = framed
        .next()
        .await
        .ok_or(ConnectionError::HandshakeEof)?
        .map_err(ConnectionError::Codec)?;

    let first_msg = match decode_frame(&first_frame) {
        DecodeResult::Ok(msg @ Message::Hello { .. }) => msg,
        DecodeResult::Ok(_non_hello) => {
            // First message is a valid message but not Hello — reject.
            // We can't echo a meaningful id without parsing, but the
            // client violated the protocol so we close immediately.
            return Err(ConnectionError::NotHello);
        }
        DecodeResult::UnknownType(_envelope) => {
            // Unknown type as first message — not a Hello.
            return Err(ConnectionError::NotHello);
        }
        DecodeResult::Malformed(e) => {
            return Err(ConnectionError::MalformedFrame(e));
        }
    };

    let response = send_command(&cmd_tx, first_msg, conn_id).await?;
    let is_error = is_error_hello_ack(&response);
    framed
        .send(response)
        .await
        .map_err(ConnectionError::Codec)?;

    if is_error {
        // Version mismatch or other handshake failure — close connection.
        return Ok(());
    }

    // -- Main loop: requests + inject delivery --
    loop {
        tokio::select! {
            frame = framed.next() => {
                let raw = match frame {
                    Some(Ok(raw)) => raw,
                    Some(Err(e)) => return Err(ConnectionError::Codec(e)),
                    None => return Ok(()), // Clean disconnect.
                };
                match decode_frame(&raw) {
                    DecodeResult::Ok(msg) => {
                        let response = send_command(&cmd_tx, msg, conn_id).await?;
                        framed.send(response).await.map_err(ConnectionError::Codec)?;
                    }
                    DecodeResult::UnknownType(envelope) => {
                        // Unknown message type — send error with echoed id,
                        // keep connection open per CONTRACT_BROKER.md §129.
                        let response = Message::Response {
                            id: envelope.id,
                            status: Status::Error,
                            error: Some("unknown_type".into()),
                            size: None,
                            sessions: None,
                            turn_id: None,
                            content: None,
                            timestamp: None,
                            byte_length: None,
                            interrupted: None,
                            truncated: None,
                            turns: None,
                        };
                        framed.send(response).await.map_err(ConnectionError::Codec)?;
                    }
                    DecodeResult::Malformed(e) => {
                        // Completely unrecoverable — can't extract id.
                        return Err(ConnectionError::MalformedFrame(e));
                    }
                }
            }
            inject = inject_rx.recv() => {
                match inject {
                    Some(msg) => {
                        framed.send(msg).await.map_err(ConnectionError::Codec)?;
                    }
                    None => {
                        // Broker loop dropped our inject sender — shutting down.
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Send a command to the broker loop and wait for the response.
async fn send_command(
    cmd_tx: &mpsc::UnboundedSender<BrokerCommand>,
    request: Message,
    conn_id: ConnectionId,
) -> Result<Message, ConnectionError> {
    let (response_tx, response_rx) = oneshot::channel();
    cmd_tx
        .send(BrokerCommand {
            request,
            response_tx,
            connection_id: conn_id,
        })
        .map_err(|_| ConnectionError::BrokerGone)?;
    response_rx
        .await
        .map_err(|_| ConnectionError::ResponseDropped)
}

fn is_error_hello_ack(msg: &Message) -> bool {
    matches!(
        msg,
        Message::HelloAck {
            status: Status::Error,
            ..
        }
    )
}

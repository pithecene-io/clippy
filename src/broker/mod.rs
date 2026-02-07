//! Broker daemon — session management, turn storage, capture/paste.
//!
//! The broker is the central coordinator for clippy v0. It listens on
//! a Unix domain socket and manages session state, turn storage, and
//! capture/paste relay between PTY wrappers and hotkey clients.
//!
//! Architecture: channel-based actor. A single broker loop owns all
//! mutable state ([`state::BrokerState`]). Per-connection tasks
//! forward commands via mpsc channels. Inject commands for paste
//! are routed to wrapper connections via per-connection channels.
//!
//! See CONTRACT_BROKER.md.

mod connection;
mod handler;
pub mod registry;
pub mod state;

use std::collections::HashMap;
use std::path::PathBuf;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use connection::{BrokerCommand, DisconnectNotice};
use handler::InjectAction;
use state::{BrokerState, ConnectionId};

use crate::ipc::protocol::Message;

/// Broker startup/runtime errors.
#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("$XDG_RUNTIME_DIR is not set")]
    NoRuntimeDir,
    #[error("broker already running at {0}")]
    AlreadyRunning(PathBuf),
    #[error("failed to create directory {path}: {source}")]
    MkdirFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to bind socket {path}: {source}")]
    BindFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Run the broker daemon until SIGTERM or SIGINT.
///
/// # Errors
///
/// Returns `BrokerError` if `$XDG_RUNTIME_DIR` is unset, socket bind
/// fails, or another broker is already running.
///
/// # Contract compliance
///
/// - Socket at `$XDG_RUNTIME_DIR/clippy/broker.sock` (mode 0700)
/// - Stale socket detection and cleanup
/// - SIGTERM/SIGINT → graceful shutdown, socket file removed
/// - All state in-memory only (lost on exit)
pub async fn run(config: state::RingConfig) -> Result<(), BrokerError> {
    let socket_path = resolve_socket_path()?;
    let listener = bind_socket(&socket_path).await?;

    tracing::info!(path = %socket_path.display(), "broker listening");

    // Channels for connection → broker communication.
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<BrokerCommand>();
    let (disconnect_tx, mut disconnect_rx) = mpsc::unbounded_channel::<DisconnectNotice>();

    // Per-connection inject channels for paste → inject routing.
    let mut inject_senders: HashMap<ConnectionId, mpsc::UnboundedSender<Message>> = HashMap::new();

    let mut state = BrokerState::new(config);

    // Graceful shutdown on SIGTERM or SIGINT.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    loop {
        tokio::select! {
            // -- New connection --
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        accept_connection(
                            stream,
                            &cmd_tx,
                            &disconnect_tx,
                            &mut inject_senders,
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                    }
                }
            }

            // -- Command from connection task --
            Some(cmd) = cmd_rx.recv() => {
                let (response, inject_action) = handler::handle_message(
                    &mut state,
                    cmd.request,
                    cmd.connection_id,
                );
                // Send response to requesting connection.
                let _ = cmd.response_tx.send(response);

                // Route inject to target wrapper if paste succeeded.
                if let Some(inject) = inject_action {
                    dispatch_inject(&inject_senders, inject);
                }
            }

            // -- Connection disconnected --
            Some(notice) = disconnect_rx.recv() => {
                let conn_id = notice.connection_id;
                inject_senders.remove(&conn_id);
                state.remove_connection(conn_id);
                tracing::debug!(?conn_id, "connection cleaned up");
            }

            // -- Shutdown signals --
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM, shutting down");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("received SIGINT, shutting down");
                break;
            }
        }
    }

    // Cleanup: remove socket file.
    drop(listener);
    if let Err(e) = std::fs::remove_file(&socket_path) {
        tracing::warn!(error = %e, path = %socket_path.display(), "failed to remove socket");
    }

    tracing::info!("broker stopped");
    Ok(())
}

/// Accept a new connection — create channels and spawn handler task.
fn accept_connection(
    stream: UnixStream,
    cmd_tx: &mpsc::UnboundedSender<BrokerCommand>,
    disconnect_tx: &mpsc::UnboundedSender<DisconnectNotice>,
    inject_senders: &mut HashMap<ConnectionId, mpsc::UnboundedSender<Message>>,
) {
    let conn_id = ConnectionId::new();
    let (inject_tx, inject_rx) = mpsc::unbounded_channel();
    inject_senders.insert(conn_id, inject_tx);

    connection::spawn_connection(
        stream,
        conn_id,
        cmd_tx.clone(),
        inject_rx,
        disconnect_tx.clone(),
    );

    tracing::debug!(?conn_id, "accepted connection");
}

/// Route an inject command to the target wrapper's connection task.
fn dispatch_inject(
    inject_senders: &HashMap<ConnectionId, mpsc::UnboundedSender<Message>>,
    action: InjectAction,
) {
    if let Some(tx) = inject_senders.get(&action.target_connection) {
        if tx.send(action.message).is_err() {
            tracing::warn!(
                conn_id = ?action.target_connection,
                "inject send failed — wrapper disconnected"
            );
        }
    } else {
        tracing::warn!(
            conn_id = ?action.target_connection,
            "inject target not found"
        );
    }
}

// -- Socket setup --

/// Resolve the broker socket path from `$XDG_RUNTIME_DIR`.
fn resolve_socket_path() -> Result<PathBuf, BrokerError> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").map_err(|_| BrokerError::NoRuntimeDir)?;
    Ok(PathBuf::from(runtime_dir)
        .join("clippy")
        .join("broker.sock"))
}

/// Create the socket directory and bind the Unix listener.
///
/// Handles stale socket detection: if EADDRINUSE, attempts to connect
/// to the existing socket. If the connection succeeds, another broker
/// is running. If it fails, the socket is stale and is removed.
async fn bind_socket(path: &std::path::Path) -> Result<UnixListener, BrokerError> {
    // Ensure parent directory exists with mode 0700.
    let parent = path.parent().expect("socket path has parent");
    if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|e| BrokerError::MkdirFailed {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    // Always validate/set directory permissions to 0700, even if the
    // directory already existed (CONTRACT_BROKER.md §63).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
            BrokerError::MkdirFailed {
                path: parent.to_path_buf(),
                source: e,
            }
        })?;
    }

    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Check if the existing socket is live.
            match UnixStream::connect(path).await {
                Ok(_) => {
                    // Another broker is running.
                    Err(BrokerError::AlreadyRunning(path.to_path_buf()))
                }
                Err(_) => {
                    // Stale socket — remove and retry.
                    tracing::info!(
                        path = %path.display(),
                        "removing stale socket"
                    );
                    std::fs::remove_file(path).map_err(|e| BrokerError::BindFailed {
                        path: path.to_path_buf(),
                        source: e,
                    })?;
                    UnixListener::bind(path).map_err(|e| BrokerError::BindFailed {
                        path: path.to_path_buf(),
                        source: e,
                    })
                }
            }
        }
        Err(e) => Err(BrokerError::BindFailed {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio::net::UnixStream;
    use tokio_util::codec::Framed;

    use crate::ipc::codec::{FrameCodec, LengthPrefixedCodec};
    use crate::ipc::protocol::{Message, PROTOCOL_VERSION, Role, Status};

    /// Start a broker on a temp socket and return the socket path.
    /// The broker runs as a background task and is cancelled on drop.
    async fn start_broker(
        path: &std::path::Path,
    ) -> tokio::task::JoinHandle<Result<(), BrokerError>> {
        let socket_path = path.to_path_buf();
        tokio::spawn(async move {
            let listener = bind_socket(&socket_path).await?;
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<BrokerCommand>();
            let (disconnect_tx, mut disconnect_rx) = mpsc::unbounded_channel::<DisconnectNotice>();
            let mut inject_senders: HashMap<ConnectionId, mpsc::UnboundedSender<Message>> =
                HashMap::new();
            let mut state = BrokerState::new(state::RingConfig::default());

            loop {
                tokio::select! {
                    result = listener.accept() => {
                        if let Ok((stream, _)) = result {
                            accept_connection(
                                stream,
                                &cmd_tx,
                                &disconnect_tx,
                                &mut inject_senders,
                            );
                        }
                    }
                    Some(cmd) = cmd_rx.recv() => {
                        let (response, inject_action) =
                            handler::handle_message(
                                &mut state,
                                cmd.request,
                                cmd.connection_id,
                            );
                        let _ = cmd.response_tx.send(response);
                        if let Some(inject) = inject_action {
                            dispatch_inject(&inject_senders, inject);
                        }
                    }
                    Some(notice) = disconnect_rx.recv() => {
                        inject_senders.remove(&notice.connection_id);
                        state.remove_connection(notice.connection_id);
                    }
                }
            }
        })
    }

    async fn connect(path: &std::path::Path) -> Framed<UnixStream, LengthPrefixedCodec> {
        let stream = UnixStream::connect(path).await.unwrap();
        Framed::new(stream, LengthPrefixedCodec::new())
    }

    async fn send_recv(
        framed: &mut Framed<UnixStream, LengthPrefixedCodec>,
        msg: Message,
    ) -> Message {
        framed.send(msg).await.unwrap();
        framed.next().await.unwrap().unwrap()
    }

    async fn handshake(framed: &mut Framed<UnixStream, LengthPrefixedCodec>, role: Role) {
        let resp = send_recv(
            framed,
            Message::Hello {
                id: 0,
                version: PROTOCOL_VERSION,
                role,
            },
        )
        .await;
        assert!(matches!(
            resp,
            Message::HelloAck {
                status: Status::Ok,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn full_capture_paste_flow() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("broker.sock");
        let _broker = start_broker(&sock).await;

        // Give the broker a moment to start listening.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // -- Wrapper connects and registers --
        let mut wrapper = connect(&sock).await;
        handshake(&mut wrapper, Role::Wrapper).await;

        let resp = send_recv(
            &mut wrapper,
            Message::Register {
                id: 1,
                session: "s1".into(),
                pid: 42,
                pattern: "generic".into(),
            },
        )
        .await;
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));

        // -- Wrapper sends a completed turn --
        let resp = send_recv(
            &mut wrapper,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"hello from agent".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
        )
        .await;
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));

        // -- Client connects and captures --
        let mut client = connect(&sock).await;
        handshake(&mut client, Role::Client).await;

        let resp = send_recv(
            &mut client,
            Message::Capture {
                id: 1,
                session: "s1".into(),
            },
        )
        .await;
        match &resp {
            Message::Response {
                status, size, id, ..
            } => {
                assert_eq!(*status, Status::Ok);
                assert_eq!(*id, 1);
                assert_eq!(*size, Some(16)); // b"hello from agent".len()
            }
            _ => panic!("expected Response, got {resp:?}"),
        }

        // -- Client pastes to the same wrapper session --
        let resp = send_recv(
            &mut client,
            Message::Paste {
                id: 2,
                session: "s1".into(),
            },
        )
        .await;
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));

        // -- Wrapper should receive the inject command --
        let inject = wrapper.next().await.unwrap().unwrap();
        match inject {
            Message::Inject { id, content } => {
                assert_eq!(id, 0); // Unsolicited
                assert_eq!(content, b"hello from agent");
            }
            other => panic!("expected Inject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn version_mismatch_closes_connection() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("broker.sock");
        let _broker = start_broker(&sock).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut conn = connect(&sock).await;
        let resp = send_recv(
            &mut conn,
            Message::Hello {
                id: 0,
                version: 999,
                role: Role::Client,
            },
        )
        .await;

        match resp {
            Message::HelloAck { status, error, .. } => {
                assert_eq!(status, Status::Error);
                assert_eq!(error.as_deref(), Some("version_mismatch"));
            }
            other => panic!("expected HelloAck error, got {other:?}"),
        }

        // Connection should be closed by the server.
        let next = conn.next().await;
        assert!(next.is_none(), "expected connection closed");
    }

    #[tokio::test]
    async fn implicit_deregister_on_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("broker.sock");
        let _broker = start_broker(&sock).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Wrapper connects and registers.
        let mut wrapper = connect(&sock).await;
        handshake(&mut wrapper, Role::Wrapper).await;
        send_recv(
            &mut wrapper,
            Message::Register {
                id: 1,
                session: "s-temp".into(),
                pid: 1,
                pattern: "generic".into(),
            },
        )
        .await;

        // Drop the wrapper — simulates disconnect.
        drop(wrapper);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Client tries to capture the disconnected session.
        let mut client = connect(&sock).await;
        handshake(&mut client, Role::Client).await;
        let resp = send_recv(
            &mut client,
            Message::Capture {
                id: 1,
                session: "s-temp".into(),
            },
        )
        .await;

        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("session_not_found"));
            }
            other => panic!("expected error response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_sessions_query() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("broker.sock");
        let _broker = start_broker(&sock).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Wrapper registers a session with a turn.
        let mut wrapper = connect(&sock).await;
        handshake(&mut wrapper, Role::Wrapper).await;
        send_recv(
            &mut wrapper,
            Message::Register {
                id: 1,
                session: "s1".into(),
                pid: 42,
                pattern: "generic".into(),
            },
        )
        .await;
        send_recv(
            &mut wrapper,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"data".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
        )
        .await;

        // Client queries sessions.
        let mut client = connect(&sock).await;
        handshake(&mut client, Role::Client).await;
        let resp = send_recv(&mut client, Message::ListSessions { id: 1 }).await;

        match resp {
            Message::Response { sessions, .. } => {
                let sessions = sessions.unwrap();
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].session, "s1");
                assert_eq!(sessions[0].pid, 42);
                assert!(sessions[0].has_turn);
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_hello_first_message_closes_connection() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("broker.sock");
        let _broker = start_broker(&sock).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Send Register as first message (not Hello).
        let mut conn = connect(&sock).await;
        conn.send(Message::Register {
            id: 1,
            session: "s1".into(),
            pid: 42,
            pattern: "generic".into(),
        })
        .await
        .unwrap();

        // Connection should be closed without any response.
        let next = conn.next().await;
        assert!(next.is_none(), "expected connection closed, got {next:?}");
    }

    #[tokio::test]
    async fn unknown_type_returns_error_keeps_connection() {
        use bytes::BufMut;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("broker.sock");
        let _broker = start_broker(&sock).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Use FrameCodec for raw frame control.
        let stream = UnixStream::connect(&sock).await.unwrap();
        let mut framed = Framed::new(stream, FrameCodec::new());

        // Handshake normally first.
        let hello = Message::Hello {
            id: 0,
            version: PROTOCOL_VERSION,
            role: Role::Client,
        };
        let hello_bytes = rmp_serde::to_vec_named(&hello).unwrap();
        let mut hello_frame = bytes::BytesMut::new();
        hello_frame.put_u32(hello_bytes.len() as u32);
        hello_frame.extend_from_slice(&hello_bytes);

        // Send hello as raw frame via FrameCodec (which encodes Message).
        framed.send(hello).await.unwrap();
        let ack_raw = framed.next().await.unwrap().unwrap();
        let ack: Message = rmp_serde::from_slice(&ack_raw).unwrap();
        assert!(matches!(
            ack,
            Message::HelloAck {
                status: Status::Ok,
                ..
            }
        ));

        // Send an unknown message type as raw MessagePack.
        // {type: "frobnicate", id: 42}
        #[derive(serde::Serialize)]
        struct FakeMsg {
            #[serde(rename = "type")]
            msg_type: String,
            id: u32,
        }
        let unknown_msg = rmp_serde::to_vec_named(&FakeMsg {
            msg_type: "frobnicate".into(),
            id: 42,
        })
        .unwrap();
        let mut raw_frame = bytes::BytesMut::new();
        raw_frame.put_u32(unknown_msg.len() as u32);
        raw_frame.extend_from_slice(&unknown_msg);

        // We need to send raw bytes, but FrameCodec::Encoder expects Message.
        // Write directly to the underlying stream instead.
        use tokio::io::AsyncWriteExt;
        let stream = framed.into_inner();
        let (mut reader, mut writer) = stream.into_split();
        writer.write_all(&raw_frame).await.unwrap();

        // Read response frame.
        use tokio::io::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await.unwrap();
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        reader.read_exact(&mut resp_buf).await.unwrap();
        let resp: Message = rmp_serde::from_slice(&resp_buf).unwrap();

        match resp {
            Message::Response {
                id, status, error, ..
            } => {
                assert_eq!(id, 42); // Echoed from unknown message
                assert_eq!(status, Status::Error);
                assert_eq!(error.as_deref(), Some("unknown_type"));
            }
            other => panic!("expected error Response, got {other:?}"),
        }

        // Connection should still be open — send a valid message.
        let list_msg = rmp_serde::to_vec_named(&Message::ListSessions { id: 7 }).unwrap();
        let mut list_frame = bytes::BytesMut::new();
        list_frame.put_u32(list_msg.len() as u32);
        list_frame.extend_from_slice(&list_msg);
        writer.write_all(&list_frame).await.unwrap();

        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await.unwrap();
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        reader.read_exact(&mut resp_buf).await.unwrap();
        let resp: Message = rmp_serde::from_slice(&resp_buf).unwrap();

        assert!(
            matches!(
                resp,
                Message::Response {
                    id: 7,
                    status: Status::Ok,
                    ..
                }
            ),
            "expected ok response after unknown_type, got {resp:?}"
        );
    }
}

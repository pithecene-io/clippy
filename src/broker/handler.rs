//! Message dispatch and request handling.
//!
//! Pure logic — no I/O. Each handler takes a mutable reference to
//! [`BrokerState`] and returns a response message plus an optional
//! [`InjectAction`] for paste-triggered inject commands.
//!
//! See CONTRACT_BROKER.md §Request / Response.

use crate::ipc::protocol::{Message, PROTOCOL_VERSION, Role, Status, TurnDescriptor};

use super::state::{BrokerState, ConnectionId};

/// An inject command that the broker loop must send to a wrapper.
///
/// Produced by the paste handler when the relay buffer is successfully
/// read. The broker loop routes this to the target wrapper's inject
/// channel.
#[derive(Debug)]
pub struct InjectAction {
    pub target_connection: ConnectionId,
    pub message: Message,
}

/// Dispatch a request message to the appropriate handler.
///
/// Returns `(response, optional_inject_action)`. The broker loop
/// sends the response back to the requesting connection and, if
/// present, routes the inject action to the target wrapper.
///
/// Enforces:
/// - Role-based access: wrapper-only messages rejected from clients
///   (CONTRACT_BROKER.md §136, §193)
/// - Server-originated variants → `unknown_type` (CONTRACT_BROKER.md §129)
pub fn handle_message(
    state: &mut BrokerState,
    request: Message,
    connection_id: ConnectionId,
) -> (Message, Option<InjectAction>) {
    match request {
        Message::Hello { id, version, role } => {
            let response = handle_hello(state, id, version, role, connection_id);
            (response, None)
        }
        // -- Wrapper-only messages --
        Message::Register {
            id,
            session,
            pid,
            pattern: _,
        } => {
            if !is_wrapper(state, connection_id) {
                return (error_response(id, "unknown_type"), None);
            }
            let response = handle_register(state, id, session, pid, connection_id);
            (response, None)
        }
        Message::Deregister { id, session } => {
            if !is_wrapper(state, connection_id) {
                return (error_response(id, "unknown_type"), None);
            }
            let response = handle_deregister(state, id, &session);
            (response, None)
        }
        Message::TurnCompleted {
            id,
            session,
            content,
            interrupted,
            timestamp,
        } => {
            if !is_wrapper(state, connection_id) {
                return (error_response(id, "unknown_type"), None);
            }
            // v0 wrappers omit timestamp (defaults to 0); fall back to
            // broker receipt time so the ring record is never zero.
            let ts = if timestamp == 0 {
                crate::turn::epoch_millis()
            } else {
                timestamp
            };
            let response = handle_turn_completed(state, id, &session, content, interrupted, ts);
            (response, None)
        }
        // -- Any role --
        Message::Capture { id, session } => {
            let response = handle_capture(state, id, &session);
            (response, None)
        }
        Message::Paste { id, session } => handle_paste(state, id, &session),
        Message::ListSessions { id } => {
            let response = handle_list_sessions(state, id);
            (response, None)
        }
        // -- Turn registry queries (v1, any role) --
        Message::GetTurn { id, turn_id } => {
            let response = handle_get_turn(state, id, &turn_id);
            (response, None)
        }
        Message::ListTurns { id, session, limit } => {
            let response = handle_list_turns(state, id, &session, limit);
            (response, None)
        }
        Message::CaptureByID { id, turn_id } => {
            let response = handle_capture_by_id(state, id, &turn_id);
            (response, None)
        }
        // Server-originated messages should never be sent by clients.
        Message::HelloAck { id, .. }
        | Message::Response { id, .. }
        | Message::Inject { id, .. } => (error_response(id, "unknown_type"), None),
    }
}

// -- Individual handlers --

fn handle_hello(
    state: &mut BrokerState,
    id: u32,
    version: u32,
    role: Role,
    connection_id: ConnectionId,
) -> Message {
    // CONTRACT_BROKER.md §102: hello.id MUST be 0.
    if id != 0 {
        return Message::HelloAck {
            id: 0,
            status: Status::Error,
            error: Some("invalid_hello_id".into()),
        };
    }
    if version != PROTOCOL_VERSION {
        return Message::HelloAck {
            id: 0,
            status: Status::Error,
            error: Some("version_mismatch".into()),
        };
    }
    state.add_connection(connection_id, role);
    // CONTRACT_BROKER.md §111: hello_ack.id MUST be 0.
    Message::HelloAck {
        id: 0,
        status: Status::Ok,
        error: None,
    }
}

fn handle_register(
    state: &mut BrokerState,
    id: u32,
    session: String,
    pid: u32,
    connection_id: ConnectionId,
) -> Message {
    match state.register_session(session, connection_id, pid) {
        Ok(()) => ok_response(id),
        Err(reason) => error_response(id, reason),
    }
}

fn handle_deregister(state: &mut BrokerState, id: u32, session: &str) -> Message {
    state.deregister_session(session);
    ok_response(id)
}

fn handle_turn_completed(
    state: &mut BrokerState,
    id: u32,
    session: &str,
    content: Vec<u8>,
    interrupted: bool,
    timestamp: u64,
) -> Message {
    match state.store_turn(session, content, interrupted, timestamp) {
        Ok(turn_id) => Message::Response {
            id,
            status: Status::Ok,
            error: None,
            size: None,
            sessions: None,
            turn_id: Some(turn_id),
            content: None,
            timestamp: None,
            byte_length: None,
            interrupted: None,
            truncated: None,
            turns: None,
        },
        Err(reason) => error_response(id, reason),
    }
}

fn handle_capture(state: &mut BrokerState, id: u32, session: &str) -> Message {
    match state.capture(session) {
        Ok(result) => Message::Response {
            id,
            status: Status::Ok,
            error: None,
            size: Some(result.size),
            sessions: None,
            turn_id: Some(result.turn_id),
            content: None,
            timestamp: None,
            byte_length: None,
            interrupted: None,
            truncated: None,
            turns: None,
        },
        Err(reason) => error_response(id, reason),
    }
}

fn handle_paste(
    state: &mut BrokerState,
    id: u32,
    session: &str,
) -> (Message, Option<InjectAction>) {
    match state.paste_content(session) {
        Ok((content, target_conn)) => {
            let inject = InjectAction {
                target_connection: target_conn,
                message: Message::Inject { id: 0, content },
            };
            (ok_response(id), Some(inject))
        }
        Err(reason) => (error_response(id, reason), None),
    }
}

fn handle_list_sessions(state: &BrokerState, id: u32) -> Message {
    let sessions = state.list_sessions();
    Message::Response {
        id,
        status: Status::Ok,
        error: None,
        size: None,
        sessions: Some(sessions),
        turn_id: None,
        content: None,
        timestamp: None,
        byte_length: None,
        interrupted: None,
        truncated: None,
        turns: None,
    }
}

fn handle_get_turn(state: &BrokerState, id: u32, turn_id: &str) -> Message {
    match state.get_turn(turn_id) {
        Ok(record) => Message::Response {
            id,
            status: Status::Ok,
            error: None,
            size: None,
            sessions: None,
            turn_id: Some(record.turn_id.clone()),
            content: Some(record.content.clone()),
            timestamp: Some(record.timestamp),
            byte_length: Some(record.byte_length),
            interrupted: Some(record.interrupted),
            truncated: Some(record.truncated),
            turns: None,
        },
        Err(reason) => error_response(id, reason),
    }
}

fn handle_list_turns(state: &BrokerState, id: u32, session: &str, limit: Option<u32>) -> Message {
    let limit = limit.map(|n| n as usize);
    match state.list_turns(session, limit) {
        Ok(records) => {
            let turns: Vec<TurnDescriptor> = records
                .into_iter()
                .map(|r| TurnDescriptor {
                    turn_id: r.turn_id.clone(),
                    timestamp: r.timestamp,
                    byte_length: r.byte_length,
                    interrupted: r.interrupted,
                    truncated: r.truncated,
                })
                .collect();
            Message::Response {
                id,
                status: Status::Ok,
                error: None,
                size: None,
                sessions: None,
                turn_id: None,
                content: None,
                timestamp: None,
                byte_length: None,
                interrupted: None,
                truncated: None,
                turns: Some(turns),
            }
        }
        Err(reason) => error_response(id, reason),
    }
}

fn handle_capture_by_id(state: &mut BrokerState, id: u32, turn_id: &str) -> Message {
    match state.capture_by_id(turn_id) {
        Ok(result) => Message::Response {
            id,
            status: Status::Ok,
            error: None,
            size: Some(result.size),
            sessions: None,
            turn_id: Some(result.turn_id),
            content: None,
            timestamp: None,
            byte_length: None,
            interrupted: None,
            truncated: None,
            turns: None,
        },
        Err(reason) => error_response(id, reason),
    }
}

// -- Helpers --

fn is_wrapper(state: &BrokerState, connection_id: ConnectionId) -> bool {
    state.connection_role(connection_id) == Some(Role::Wrapper)
}

fn ok_response(id: u32) -> Message {
    Message::Response {
        id,
        status: Status::Ok,
        error: None,
        size: None,
        sessions: None,
        turn_id: None,
        content: None,
        timestamp: None,
        byte_length: None,
        interrupted: None,
        truncated: None,
        turns: None,
    }
}

fn error_response(id: u32, reason: &str) -> Message {
    Message::Response {
        id,
        status: Status::Error,
        error: Some(reason.into()),
        size: None,
        sessions: None,
        turn_id: None,
        content: None,
        timestamp: None,
        byte_length: None,
        interrupted: None,
        truncated: None,
        turns: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (BrokerState, ConnectionId) {
        use crate::broker::state::RingConfig;
        let state = BrokerState::new(RingConfig::default());
        let conn = ConnectionId::new();
        (state, conn)
    }

    fn hello(version: u32) -> Message {
        Message::Hello {
            id: 0,
            version,
            role: Role::Wrapper,
        }
    }

    fn register(id: u32, session: &str, pid: u32) -> Message {
        Message::Register {
            id,
            session: session.into(),
            pid,
            pattern: "generic".into(),
        }
    }

    // -- Hello --

    #[test]
    fn hello_success() {
        let (mut s, c) = fresh();
        let (resp, inject) = handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        assert!(inject.is_none());
        assert!(matches!(
            resp,
            Message::HelloAck {
                id: 0,
                status: Status::Ok,
                ..
            }
        ));
    }

    #[test]
    fn hello_version_mismatch() {
        let (mut s, c) = fresh();
        let (resp, _) = handle_message(&mut s, hello(999), c);
        match resp {
            Message::HelloAck {
                id, status, error, ..
            } => {
                assert_eq!(id, 0); // Fixed ID per contract
                assert_eq!(status, Status::Error);
                assert_eq!(error.as_deref(), Some("version_mismatch"));
            }
            _ => panic!("expected HelloAck"),
        }
    }

    #[test]
    fn hello_nonzero_id_rejected() {
        let (mut s, c) = fresh();
        let (resp, _) = handle_message(
            &mut s,
            Message::Hello {
                id: 5,
                version: PROTOCOL_VERSION,
                role: Role::Wrapper,
            },
            c,
        );
        match resp {
            Message::HelloAck {
                id, status, error, ..
            } => {
                assert_eq!(id, 0); // Always 0
                assert_eq!(status, Status::Error);
                assert_eq!(error.as_deref(), Some("invalid_hello_id"));
            }
            _ => panic!("expected HelloAck"),
        }
    }

    #[test]
    fn hello_ack_always_id_zero() {
        let (mut s, c) = fresh();
        let (resp, _) = handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        match resp {
            Message::HelloAck { id, .. } => assert_eq!(id, 0),
            _ => panic!("expected HelloAck"),
        }
    }

    // -- Register --

    #[test]
    fn register_success() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        let (resp, _) = handle_message(&mut s, register(1, "s1", 100), c);
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));
    }

    #[test]
    fn register_duplicate() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        let (resp, _) = handle_message(&mut s, register(2, "s1", 200), c);
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("duplicate_session"));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn register_rejected_from_client() {
        let (mut s, c) = fresh();
        // Connect as client role
        handle_message(
            &mut s,
            Message::Hello {
                id: 0,
                version: PROTOCOL_VERSION,
                role: Role::Client,
            },
            c,
        );
        let (resp, _) = handle_message(&mut s, register(1, "s1", 100), c);
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("unknown_type"));
            }
            _ => panic!("expected error Response"),
        }
    }

    // -- Deregister --

    #[test]
    fn deregister_success() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        let (resp, _) = handle_message(
            &mut s,
            Message::Deregister {
                id: 2,
                session: "s1".into(),
            },
            c,
        );
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));
    }

    // -- Turn completed --

    #[test]
    fn turn_completed_success() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        let (resp, _) = handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"output".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));
    }

    #[test]
    fn turn_completed_session_not_found() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        let (resp, _) = handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 1,
                session: "nonexistent".into(),
                content: b"data".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("session_not_found"));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn turn_completed_rejected_from_client() {
        let (mut s, c) = fresh();
        handle_message(
            &mut s,
            Message::Hello {
                id: 0,
                version: PROTOCOL_VERSION,
                role: Role::Client,
            },
            c,
        );
        let (resp, _) = handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 1,
                session: "s1".into(),
                content: b"data".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("unknown_type"));
            }
            _ => panic!("expected error Response"),
        }
    }

    // -- Capture --

    #[test]
    fn capture_success_returns_size() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"12345".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );
        let (resp, _) = handle_message(
            &mut s,
            Message::Capture {
                id: 3,
                session: "s1".into(),
            },
            c,
        );
        match resp {
            Message::Response {
                status, size, id, ..
            } => {
                assert_eq!(status, Status::Ok);
                assert_eq!(id, 3);
                assert_eq!(size, Some(5));
            }
            _ => panic!("expected Response"),
        }
    }

    // -- Paste --

    #[test]
    fn paste_success_produces_inject_action() {
        let (mut s, c1) = fresh();
        let c2 = ConnectionId::new();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c1);
        handle_message(
            &mut s,
            Message::Hello {
                id: 0,
                version: PROTOCOL_VERSION,
                role: Role::Client,
            },
            c2,
        );
        handle_message(&mut s, register(1, "s1", 100), c1);
        handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"turn data".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c1,
        );
        handle_message(
            &mut s,
            Message::Capture {
                id: 3,
                session: "s1".into(),
            },
            c2,
        );

        let (resp, inject) = handle_message(
            &mut s,
            Message::Paste {
                id: 4,
                session: "s1".into(),
            },
            c2,
        );

        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));
        let inject = inject.expect("paste should produce InjectAction");
        assert_eq!(inject.target_connection, c1);
        match inject.message {
            Message::Inject { id, content } => {
                assert_eq!(id, 0);
                assert_eq!(content, b"turn data");
            }
            _ => panic!("expected Inject"),
        }
    }

    #[test]
    fn paste_buffer_empty() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        let (resp, inject) = handle_message(
            &mut s,
            Message::Paste {
                id: 2,
                session: "s1".into(),
            },
            c,
        );
        assert!(inject.is_none());
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("buffer_empty"));
            }
            _ => panic!("expected Response"),
        }
    }

    // -- List sessions --

    #[test]
    fn list_sessions_returns_descriptors() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        let (resp, _) = handle_message(&mut s, Message::ListSessions { id: 2 }, c);
        match resp {
            Message::Response { sessions, .. } => {
                let sessions = sessions.unwrap();
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].session, "s1");
            }
            _ => panic!("expected Response"),
        }
    }

    // -- Unknown type --

    #[test]
    fn server_messages_return_unknown_type() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);

        // HelloAck from client → unknown_type
        let (resp, _) = handle_message(
            &mut s,
            Message::HelloAck {
                id: 1,
                status: Status::Ok,
                error: None,
            },
            c,
        );
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("unknown_type"));
            }
            _ => panic!("expected Response"),
        }

        // Inject from client → unknown_type
        let (resp, _) = handle_message(
            &mut s,
            Message::Inject {
                id: 2,
                content: vec![],
            },
            c,
        );
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("unknown_type"));
            }
            _ => panic!("expected Response"),
        }
    }

    // -- Response ID echoing --

    #[test]
    fn response_echoes_request_id() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);

        let (resp, _) = handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 42,
                session: "s1".into(),
                content: b"data".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );
        match resp {
            Message::Response { id, .. } => assert_eq!(id, 42),
            _ => panic!("expected Response"),
        }
    }

    // -- Turn ID in responses --

    #[test]
    fn turn_completed_response_includes_turn_id() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);

        let (resp, _) = handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"data".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );
        match resp {
            Message::Response {
                status, turn_id, ..
            } => {
                assert_eq!(status, Status::Ok);
                assert_eq!(turn_id, Some("s1:1".into()));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn capture_response_includes_turn_id() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"data".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );

        let (resp, _) = handle_message(
            &mut s,
            Message::Capture {
                id: 3,
                session: "s1".into(),
            },
            c,
        );
        match resp {
            Message::Response {
                status,
                size,
                turn_id,
                ..
            } => {
                assert_eq!(status, Status::Ok);
                assert_eq!(size, Some(4));
                assert_eq!(turn_id, Some("s1:1".into()));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn interrupted_flag_stored_via_handler() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);

        let (resp, _) = handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"data".to_vec(),
                interrupted: true,
                timestamp: 1000,
            },
            c,
        );
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));

        // Verify the interrupted flag was actually stored.
        let turns = s.list_turns("s1", None).unwrap();
        assert!(turns[0].interrupted);
    }

    #[test]
    fn turn_id_increments_across_turns() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);

        let (r1, _) = handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"a".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );
        let (r2, _) = handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 3,
                session: "s1".into(),
                content: b"b".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );

        match (r1, r2) {
            (
                Message::Response {
                    turn_id: Some(t1), ..
                },
                Message::Response {
                    turn_id: Some(t2), ..
                },
            ) => {
                assert_eq!(t1, "s1:1");
                assert_eq!(t2, "s1:2");
            }
            _ => panic!("expected Responses with turn_ids"),
        }
    }

    // -- GetTurn --

    /// Helper: set up a session with a single turn and return state + connection.
    fn setup_with_turn() -> (BrokerState, ConnectionId) {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"hello world".to_vec(),
                interrupted: false,
                timestamp: 5000,
            },
            c,
        );
        (s, c)
    }

    #[test]
    fn get_turn_success() {
        let (mut s, c) = setup_with_turn();
        let (resp, _) = handle_message(
            &mut s,
            Message::GetTurn {
                id: 10,
                turn_id: "s1:1".into(),
            },
            c,
        );
        match resp {
            Message::Response {
                id,
                status,
                turn_id,
                content,
                timestamp,
                byte_length,
                interrupted,
                truncated,
                ..
            } => {
                assert_eq!(id, 10);
                assert_eq!(status, Status::Ok);
                assert_eq!(turn_id, Some("s1:1".into()));
                assert_eq!(content, Some(b"hello world".to_vec()));
                assert_eq!(timestamp, Some(5000));
                assert_eq!(byte_length, Some(11));
                assert_eq!(interrupted, Some(false));
                assert_eq!(truncated, Some(false));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn get_turn_not_found() {
        let (mut s, c) = setup_with_turn();
        let (resp, _) = handle_message(
            &mut s,
            Message::GetTurn {
                id: 10,
                turn_id: "s1:999".into(),
            },
            c,
        );
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("turn_not_found"));
            }
            _ => panic!("expected Response"),
        }
    }

    // -- ListTurns --

    #[test]
    fn list_turns_success() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        for i in 0..3 {
            handle_message(
                &mut s,
                Message::TurnCompleted {
                    id: 2 + i,
                    session: "s1".into(),
                    content: format!("turn-{i}").into_bytes(),
                    interrupted: false,
                    timestamp: 1000 + u64::from(i),
                },
                c,
            );
        }

        let (resp, _) = handle_message(
            &mut s,
            Message::ListTurns {
                id: 10,
                session: "s1".into(),
                limit: None,
            },
            c,
        );
        match resp {
            Message::Response {
                status, turns, id, ..
            } => {
                assert_eq!(id, 10);
                assert_eq!(status, Status::Ok);
                let turns = turns.unwrap();
                assert_eq!(turns.len(), 3);
                // Newest first.
                assert_eq!(turns[0].turn_id, "s1:3");
                assert_eq!(turns[1].turn_id, "s1:2");
                assert_eq!(turns[2].turn_id, "s1:1");
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn list_turns_with_limit() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        for i in 0..5 {
            handle_message(
                &mut s,
                Message::TurnCompleted {
                    id: 2 + i,
                    session: "s1".into(),
                    content: b"x".to_vec(),
                    interrupted: false,
                    timestamp: 1000,
                },
                c,
            );
        }

        let (resp, _) = handle_message(
            &mut s,
            Message::ListTurns {
                id: 10,
                session: "s1".into(),
                limit: Some(2),
            },
            c,
        );
        match resp {
            Message::Response { turns, .. } => {
                assert_eq!(turns.unwrap().len(), 2);
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn list_turns_session_not_found() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        let (resp, _) = handle_message(
            &mut s,
            Message::ListTurns {
                id: 10,
                session: "nonexistent".into(),
                limit: None,
            },
            c,
        );
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("session_not_found"));
            }
            _ => panic!("expected Response"),
        }
    }

    // -- CaptureByID --

    #[test]
    fn capture_by_id_success() {
        let (mut s, c) = fresh();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c);
        handle_message(&mut s, register(1, "s1", 100), c);
        handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"first".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c,
        );
        handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 3,
                session: "s1".into(),
                content: b"second".to_vec(),
                interrupted: false,
                timestamp: 2000,
            },
            c,
        );

        // Capture the first turn, not the latest.
        let (resp, _) = handle_message(
            &mut s,
            Message::CaptureByID {
                id: 10,
                turn_id: "s1:1".into(),
            },
            c,
        );
        match resp {
            Message::Response {
                status,
                size,
                turn_id,
                ..
            } => {
                assert_eq!(status, Status::Ok);
                assert_eq!(size, Some(5)); // b"first".len()
                assert_eq!(turn_id, Some("s1:1".into()));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn capture_by_id_not_found() {
        let (mut s, c) = setup_with_turn();
        let (resp, _) = handle_message(
            &mut s,
            Message::CaptureByID {
                id: 10,
                turn_id: "s1:999".into(),
            },
            c,
        );
        match resp {
            Message::Response { error, .. } => {
                assert_eq!(error.as_deref(), Some("turn_not_found"));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn capture_by_id_then_paste() {
        let (mut s, c1) = fresh();
        let c2 = ConnectionId::new();
        handle_message(&mut s, hello(PROTOCOL_VERSION), c1);
        handle_message(
            &mut s,
            Message::Hello {
                id: 0,
                version: PROTOCOL_VERSION,
                role: Role::Client,
            },
            c2,
        );
        handle_message(&mut s, register(1, "s1", 100), c1);
        handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"first".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            c1,
        );
        handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 3,
                session: "s1".into(),
                content: b"second".to_vec(),
                interrupted: false,
                timestamp: 2000,
            },
            c1,
        );

        // Capture the first turn by ID.
        handle_message(
            &mut s,
            Message::CaptureByID {
                id: 4,
                turn_id: "s1:1".into(),
            },
            c2,
        );

        // Paste — should inject the first turn's content.
        let (resp, inject) = handle_message(
            &mut s,
            Message::Paste {
                id: 5,
                session: "s1".into(),
            },
            c2,
        );
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));
        let inject = inject.expect("paste should produce InjectAction");
        match inject.message {
            Message::Inject { content, .. } => {
                assert_eq!(content, b"first");
            }
            _ => panic!("expected Inject"),
        }
    }

    #[test]
    fn v1_queries_work_from_client_role() {
        let (mut s, c) = fresh();
        // Connect as Client (not Wrapper).
        handle_message(
            &mut s,
            Message::Hello {
                id: 0,
                version: PROTOCOL_VERSION,
                role: Role::Client,
            },
            c,
        );

        // Need a wrapper to register a session.
        let w = ConnectionId::new();
        handle_message(&mut s, hello(PROTOCOL_VERSION), w);
        handle_message(&mut s, register(1, "s1", 100), w);
        handle_message(
            &mut s,
            Message::TurnCompleted {
                id: 2,
                session: "s1".into(),
                content: b"data".to_vec(),
                interrupted: false,
                timestamp: 1000,
            },
            w,
        );

        // Client can use GetTurn.
        let (resp, _) = handle_message(
            &mut s,
            Message::GetTurn {
                id: 10,
                turn_id: "s1:1".into(),
            },
            c,
        );
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));

        // Client can use ListTurns.
        let (resp, _) = handle_message(
            &mut s,
            Message::ListTurns {
                id: 11,
                session: "s1".into(),
                limit: None,
            },
            c,
        );
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));

        // Client can use CaptureByID.
        let (resp, _) = handle_message(
            &mut s,
            Message::CaptureByID {
                id: 12,
                turn_id: "s1:1".into(),
            },
            c,
        );
        assert!(matches!(
            resp,
            Message::Response {
                status: Status::Ok,
                ..
            }
        ));
    }
}

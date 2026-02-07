//! Broker state — session table, relay buffer, connection tracking.
//!
//! All methods are pure state transitions with no I/O. Error strings
//! are machine-readable reasons from CONTRACT_BROKER.md §Error Semantics.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ipc::protocol::SessionDescriptor;

/// Unique identifier for a client connection.
///
/// Monotonically increasing counter. Used to route inject commands
/// to the correct wrapper connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Session entry in the broker's session table.
#[derive(Debug)]
struct SessionEntry {
    connection_id: ConnectionId,
    pid: u32,
    /// Latest completed turn content (raw bytes, no interpretation).
    /// `None` if no turn has been received yet.
    latest_turn: Option<Vec<u8>>,
}

/// Broker state — session table and relay buffer.
///
/// Owned exclusively by the broker loop. No concurrent access.
/// See CONTRACT_BROKER.md §Session Management, §Turn Storage,
/// §Capture / Paste.
#[derive(Debug)]
pub struct BrokerState {
    /// Session table keyed by session ID.
    sessions: HashMap<String, SessionEntry>,
    /// Global single-slot relay buffer. `None` until first capture.
    relay_buffer: Option<Vec<u8>>,
    /// Set of active connection IDs (for liveness checks).
    connections: HashSet<ConnectionId>,
}

impl BrokerState {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            relay_buffer: None,
            connections: HashSet::new(),
        }
    }

    /// Register a new connection.
    pub fn add_connection(&mut self, id: ConnectionId) {
        self.connections.insert(id);
    }

    /// Remove a connection and implicitly deregister any associated session.
    ///
    /// CONTRACT_BROKER.md §Implicit deregister: if a wrapper connection
    /// drops without sending `deregister`, the session is removed.
    pub fn remove_connection(&mut self, id: ConnectionId) {
        self.connections.remove(&id);
        // Find and remove any session owned by this connection.
        self.sessions.retain(|_, entry| entry.connection_id != id);
    }

    /// Register a new session.
    ///
    /// Returns `Err("duplicate_session")` if the session ID is already registered.
    pub fn register_session(
        &mut self,
        session_id: String,
        connection_id: ConnectionId,
        pid: u32,
    ) -> Result<(), &'static str> {
        if self.sessions.contains_key(&session_id) {
            return Err("duplicate_session");
        }
        self.sessions.insert(
            session_id,
            SessionEntry {
                connection_id,
                pid,
                latest_turn: None,
            },
        );
        Ok(())
    }

    /// Deregister a session. Idempotent — returns `Ok(())` even if
    /// the session was already removed.
    ///
    /// CONTRACT_BROKER.md §Deregister: relay buffer is NOT cleared
    /// (content was already captured).
    pub fn deregister_session(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    /// Store a completed turn for a session, replacing any previous turn.
    ///
    /// CONTRACT_BROKER.md §Turn Storage: raw bytes, no interpretation.
    /// Returns `Err("session_not_found")` if the session doesn't exist.
    pub fn store_turn(&mut self, session_id: &str, content: Vec<u8>) -> Result<(), &'static str> {
        let entry = self
            .sessions
            .get_mut(session_id)
            .ok_or("session_not_found")?;
        entry.latest_turn = Some(content);
        Ok(())
    }

    /// Capture: copy a session's latest turn into the relay buffer.
    ///
    /// Returns the byte size of the captured content on success.
    /// The session's turn buffer is NOT cleared.
    /// The relay buffer is overwritten (previous content replaced).
    pub fn capture(&mut self, session_id: &str) -> Result<u32, &'static str> {
        let entry = self.sessions.get(session_id).ok_or("session_not_found")?;
        let content = entry.latest_turn.as_ref().ok_or("no_turn")?;
        let size = content.len() as u32;
        self.relay_buffer = Some(content.clone());
        Ok(size)
    }

    /// Read relay buffer content and resolve the target wrapper connection.
    ///
    /// Returns `(content, target_connection_id)` on success.
    /// Does NOT clear the relay buffer (same content can be pasted
    /// multiple times per CONTRACT_BROKER.md §Relay buffer persistence).
    pub fn paste_content(&self, session_id: &str) -> Result<(Vec<u8>, ConnectionId), &'static str> {
        let content = self.relay_buffer.as_ref().ok_or("buffer_empty")?.clone();
        let entry = self.sessions.get(session_id).ok_or("session_not_found")?;
        if !self.connections.contains(&entry.connection_id) {
            return Err("session_disconnected");
        }
        Ok((content, entry.connection_id))
    }

    /// List all active sessions.
    ///
    /// Returns a descriptor for each session including whether it
    /// has a completed turn.
    pub fn list_sessions(&self) -> Vec<SessionDescriptor> {
        self.sessions
            .iter()
            .map(|(id, entry)| SessionDescriptor {
                session: id.clone(),
                pid: entry.pid,
                has_turn: entry.latest_turn.is_some(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> BrokerState {
        BrokerState::new()
    }

    fn conn() -> ConnectionId {
        ConnectionId::new()
    }

    // -- Connection tracking --

    #[test]
    fn add_and_remove_connection() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        assert!(s.connections.contains(&c));
        s.remove_connection(c);
        assert!(!s.connections.contains(&c));
    }

    // -- Registration --

    #[test]
    fn register_session_success() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        assert!(s.register_session("s1".into(), c, 100).is_ok());
        assert_eq!(s.sessions.len(), 1);
    }

    #[test]
    fn register_duplicate_session() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        assert_eq!(
            s.register_session("s1".into(), c, 200),
            Err("duplicate_session")
        );
    }

    #[test]
    fn deregister_session_removes_entry() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        s.deregister_session("s1");
        assert!(s.sessions.is_empty());
    }

    #[test]
    fn deregister_nonexistent_is_ok() {
        let mut s = state();
        // No panic, no error.
        s.deregister_session("nonexistent");
    }

    // -- Implicit deregister --

    #[test]
    fn remove_connection_deregisters_session() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        s.remove_connection(c);
        assert!(s.sessions.is_empty());
    }

    #[test]
    fn remove_connection_leaves_other_sessions() {
        let mut s = state();
        let c1 = conn();
        let c2 = conn();
        s.add_connection(c1);
        s.add_connection(c2);
        s.register_session("s1".into(), c1, 100).unwrap();
        s.register_session("s2".into(), c2, 200).unwrap();
        s.remove_connection(c1);
        assert_eq!(s.sessions.len(), 1);
        assert!(s.sessions.contains_key("s2"));
    }

    // -- Turn storage --

    #[test]
    fn store_turn_success() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        assert!(s.store_turn("s1", b"turn content".to_vec()).is_ok());
    }

    #[test]
    fn store_turn_replaces_previous() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"first".to_vec()).unwrap();
        s.store_turn("s1", b"second".to_vec()).unwrap();
        assert_eq!(
            s.sessions["s1"].latest_turn.as_deref(),
            Some(b"second".as_slice())
        );
    }

    #[test]
    fn store_turn_session_not_found() {
        let mut s = state();
        assert_eq!(
            s.store_turn("nonexistent", b"data".to_vec()),
            Err("session_not_found")
        );
    }

    // -- Capture --

    #[test]
    fn capture_success() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"turn data".to_vec()).unwrap();
        let size = s.capture("s1").unwrap();
        assert_eq!(size, 9);
        assert_eq!(s.relay_buffer.as_deref(), Some(b"turn data".as_slice()));
    }

    #[test]
    fn capture_session_not_found() {
        let mut s = state();
        assert_eq!(s.capture("nonexistent"), Err("session_not_found"));
    }

    #[test]
    fn capture_no_turn() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        assert_eq!(s.capture("s1"), Err("no_turn"));
    }

    #[test]
    fn capture_does_not_clear_session_turn() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"turn data".to_vec()).unwrap();
        s.capture("s1").unwrap();
        // Session's turn is still there.
        assert!(s.sessions["s1"].latest_turn.is_some());
    }

    #[test]
    fn capture_overwrites_relay_buffer() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"first".to_vec()).unwrap();
        s.capture("s1").unwrap();
        s.store_turn("s1", b"second".to_vec()).unwrap();
        s.capture("s1").unwrap();
        assert_eq!(s.relay_buffer.as_deref(), Some(b"second".as_slice()));
    }

    // -- Paste --

    #[test]
    fn paste_success() {
        let mut s = state();
        let c1 = conn();
        let c2 = conn();
        s.add_connection(c1);
        s.add_connection(c2);
        s.register_session("s1".into(), c1, 100).unwrap();
        s.register_session("s2".into(), c2, 200).unwrap();
        s.store_turn("s1", b"turn data".to_vec()).unwrap();
        s.capture("s1").unwrap();

        let (content, target) = s.paste_content("s2").unwrap();
        assert_eq!(content, b"turn data");
        assert_eq!(target, c2);
    }

    #[test]
    fn paste_buffer_empty() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        assert_eq!(s.paste_content("s1"), Err("buffer_empty"));
    }

    #[test]
    fn paste_session_not_found() {
        let mut s = state();
        s.relay_buffer = Some(b"data".to_vec());
        assert_eq!(s.paste_content("nonexistent"), Err("session_not_found"));
    }

    #[test]
    fn paste_session_disconnected() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"turn data".to_vec()).unwrap();
        s.capture("s1").unwrap();
        // Simulate disconnect without deregister.
        s.connections.remove(&c);
        assert_eq!(s.paste_content("s1"), Err("session_disconnected"));
    }

    #[test]
    fn paste_does_not_clear_relay_buffer() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"data".to_vec()).unwrap();
        s.capture("s1").unwrap();
        s.paste_content("s1").unwrap();
        // Relay buffer still has content.
        assert!(s.relay_buffer.is_some());
    }

    // -- List sessions --

    #[test]
    fn list_sessions_empty() {
        let s = state();
        assert!(s.list_sessions().is_empty());
    }

    #[test]
    fn list_sessions_populated() {
        let mut s = state();
        let c = conn();
        s.add_connection(c);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"data".to_vec()).unwrap();

        let c2 = conn();
        s.add_connection(c2);
        s.register_session("s2".into(), c2, 200).unwrap();

        let mut list = s.list_sessions();
        list.sort_by(|a, b| a.session.cmp(&b.session));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session, "s1");
        assert_eq!(list[0].pid, 100);
        assert!(list[0].has_turn);
        assert_eq!(list[1].session, "s2");
        assert_eq!(list[1].pid, 200);
        assert!(!list[1].has_turn);
    }
}

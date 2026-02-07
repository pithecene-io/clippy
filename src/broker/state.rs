//! Broker state — session table, relay buffer, connection tracking.
//!
//! All methods are pure state transitions with no I/O. Error strings
//! are machine-readable reasons from CONTRACT_BROKER.md §Error Semantics
//! and CONTRACT_REGISTRY.md.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ipc::protocol::{Role, SessionDescriptor};

use super::registry::{TurnRecord, TurnRingBuffer};

/// Configuration for per-session turn ring buffers.
#[derive(Debug, Clone)]
pub struct RingConfig {
    /// Maximum number of turns retained per session.
    pub depth: usize,
    /// Maximum byte size per turn (content is truncated beyond this).
    pub max_turn_bytes: usize,
}

impl Default for RingConfig {
    fn default() -> Self {
        Self {
            depth: 32,
            max_turn_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Turn metadata passed to sinks per CONTRACT_REGISTRY.md §266.
///
/// All fields are public and part of the sink interface contract.
/// v1 sinks do not consume these, but the interface must carry them.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SinkMetadata {
    pub turn_id: String,
    pub timestamp: u64,
    pub byte_length: u32,
    pub interrupted: bool,
    pub truncated: bool,
}

/// Relay buffer entry — captured turn content with metadata.
#[derive(Debug)]
struct RelayEntry {
    content: Vec<u8>,
    metadata: SinkMetadata,
}

/// Result of a capture operation.
#[derive(Debug, PartialEq, Eq)]
pub struct CaptureResult {
    pub size: u32,
    pub turn_id: String,
}

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
    /// Per-session ring buffer of completed turns.
    ring: TurnRingBuffer,
}

/// Broker state — session table and relay buffer.
///
/// Owned exclusively by the broker loop. No concurrent access.
/// See CONTRACT_BROKER.md §Session Management, §Turn Storage,
/// §Capture / Paste, and CONTRACT_REGISTRY.md.
#[derive(Debug)]
pub struct BrokerState {
    /// Session table keyed by session ID.
    sessions: HashMap<String, SessionEntry>,
    /// Global single-slot relay buffer. `None` until first capture.
    relay_buffer: Option<RelayEntry>,
    /// Active connections keyed by ID, storing their role.
    connections: HashMap<ConnectionId, Role>,
    /// Ring buffer configuration applied to new sessions.
    ring_config: RingConfig,
}

impl BrokerState {
    pub fn new(config: RingConfig) -> Self {
        Self {
            sessions: HashMap::new(),
            relay_buffer: None,
            connections: HashMap::new(),
            ring_config: config,
        }
    }

    /// Register a new connection with its role.
    pub fn add_connection(&mut self, id: ConnectionId, role: Role) {
        self.connections.insert(id, role);
    }

    /// Get the role for a connection, if it exists.
    pub fn connection_role(&self, id: ConnectionId) -> Option<Role> {
        self.connections.get(&id).copied()
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
        let ring = TurnRingBuffer::new(
            session_id.clone(),
            self.ring_config.depth,
            self.ring_config.max_turn_bytes,
        );
        self.sessions.insert(
            session_id,
            SessionEntry {
                connection_id,
                pid,
                ring,
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

    /// Store a completed turn for a session.
    ///
    /// Pushes into the per-session ring buffer. Returns the assigned
    /// turn ID on success.
    ///
    /// CONTRACT_BROKER.md §Turn Storage: raw bytes, no interpretation.
    /// CONTRACT_REGISTRY.md: turn IDs, metadata, ring eviction.
    /// `timestamp` is the detection-time Unix epoch millis from the wrapper.
    pub fn store_turn(
        &mut self,
        session_id: &str,
        content: Vec<u8>,
        interrupted: bool,
        timestamp: u64,
    ) -> Result<String, &'static str> {
        let entry = self
            .sessions
            .get_mut(session_id)
            .ok_or("session_not_found")?;
        let record = entry.ring.push(content, interrupted, timestamp);
        Ok(record.turn_id.clone())
    }

    /// Capture: copy a session's latest turn into the relay buffer.
    ///
    /// Returns a [`CaptureResult`] with the byte size and turn ID.
    /// The session's turn is NOT cleared.
    /// The relay buffer is overwritten (previous content replaced).
    pub fn capture(&mut self, session_id: &str) -> Result<CaptureResult, &'static str> {
        let entry = self.sessions.get(session_id).ok_or("session_not_found")?;
        let head = entry.ring.head().ok_or("no_turn")?;
        let size = head.content.len() as u32;
        let turn_id = head.turn_id.clone();
        self.relay_buffer = Some(RelayEntry {
            content: head.content.clone(),
            metadata: SinkMetadata {
                turn_id: turn_id.clone(),
                timestamp: head.timestamp,
                byte_length: head.byte_length,
                interrupted: head.interrupted,
                truncated: head.truncated,
            },
        });
        Ok(CaptureResult { size, turn_id })
    }

    /// Read relay buffer content and resolve the target wrapper connection.
    ///
    /// Returns `(content, target_connection_id)` on success.
    /// Does NOT clear the relay buffer (same content can be pasted
    /// multiple times per CONTRACT_BROKER.md §Relay buffer persistence).
    pub fn paste_content(&self, session_id: &str) -> Result<(Vec<u8>, ConnectionId), &'static str> {
        let relay = self.relay_buffer.as_ref().ok_or("buffer_empty")?;
        let content = relay.content.clone();
        let entry = self.sessions.get(session_id).ok_or("session_not_found")?;
        if !self.connections.contains_key(&entry.connection_id) {
            return Err("session_disconnected");
        }
        Ok((content, entry.connection_id))
    }

    /// Read a clone of the relay buffer content and metadata, if present.
    ///
    /// Used by non-inject sinks (clipboard, file) that need the
    /// content and metadata without session routing. Returns `None`
    /// if no turn has been captured yet.
    ///
    /// CONTRACT_REGISTRY.md §266: sinks receive `(content, metadata)`.
    pub fn relay_content(&self) -> Option<(Vec<u8>, SinkMetadata)> {
        self.relay_buffer
            .as_ref()
            .map(|r| (r.content.clone(), r.metadata.clone()))
    }

    /// List all active sessions.
    ///
    /// Returns a descriptor for each session including whether it
    /// has a completed turn. Backward compatible with v0.
    pub fn list_sessions(&self) -> Vec<SessionDescriptor> {
        self.sessions
            .iter()
            .map(|(id, entry)| SessionDescriptor {
                session: id.clone(),
                pid: entry.pid,
                has_turn: !entry.ring.is_empty(),
            })
            .collect()
    }

    /// Look up a specific turn by its ID.
    ///
    /// Turn IDs have the format `<session_id>:<seq>`. The session ID
    /// is extracted by splitting on the first `:`.
    pub fn get_turn(&self, turn_id: &str) -> Result<&TurnRecord, &'static str> {
        let session_id = turn_id
            .split_once(':')
            .map(|(s, _)| s)
            .ok_or("turn_not_found")?;
        let entry = self.sessions.get(session_id).ok_or("turn_not_found")?;
        entry.ring.get(turn_id).ok_or("turn_not_found")
    }

    /// List turn descriptors for a session, newest first.
    pub fn list_turns(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<&TurnRecord>, &'static str> {
        let entry = self.sessions.get(session_id).ok_or("session_not_found")?;
        Ok(entry.ring.iter_newest_first(limit).collect())
    }

    /// Capture a specific turn by ID into the relay buffer.
    ///
    /// Like [`capture`](Self::capture) but resolves a specific turn
    /// from the ring instead of the head.
    pub fn capture_by_id(&mut self, turn_id: &str) -> Result<CaptureResult, &'static str> {
        let session_id = turn_id
            .split_once(':')
            .map(|(s, _)| s)
            .ok_or("turn_not_found")?;
        let entry = self.sessions.get(session_id).ok_or("turn_not_found")?;
        let record = entry.ring.get(turn_id).ok_or("turn_not_found")?;
        let size = record.content.len() as u32;
        let turn_id = record.turn_id.clone();
        self.relay_buffer = Some(RelayEntry {
            content: record.content.clone(),
            metadata: SinkMetadata {
                turn_id: turn_id.clone(),
                timestamp: record.timestamp,
                byte_length: record.byte_length,
                interrupted: record.interrupted,
                truncated: record.truncated,
            },
        });
        Ok(CaptureResult { size, turn_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> BrokerState {
        BrokerState::new(RingConfig::default())
    }

    fn conn() -> ConnectionId {
        ConnectionId::new()
    }

    // -- Connection tracking --

    #[test]
    fn add_and_remove_connection() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        assert!(s.connections.contains_key(&c));
        s.remove_connection(c);
        assert!(!s.connections.contains_key(&c));
    }

    #[test]
    fn connection_role_tracking() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Client);
        assert_eq!(s.connection_role(c), Some(Role::Client));
        s.remove_connection(c);
        assert_eq!(s.connection_role(c), None);
    }

    // -- Registration --

    #[test]
    fn register_session_success() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        assert!(s.register_session("s1".into(), c, 100).is_ok());
        assert_eq!(s.sessions.len(), 1);
    }

    #[test]
    fn register_duplicate_session() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
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
        s.add_connection(c, Role::Wrapper);
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
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.remove_connection(c);
        assert!(s.sessions.is_empty());
    }

    #[test]
    fn remove_connection_leaves_other_sessions() {
        let mut s = state();
        let c1 = conn();
        let c2 = conn();
        s.add_connection(c1, Role::Wrapper);
        s.add_connection(c2, Role::Wrapper);
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
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        let turn_id = s
            .store_turn("s1", b"turn content".to_vec(), false, 1000)
            .unwrap();
        assert_eq!(turn_id, "s1:1");
    }

    #[test]
    fn store_turn_returns_turn_id() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        let t1 = s.store_turn("s1", b"first".to_vec(), false, 1000).unwrap();
        let t2 = s.store_turn("s1", b"second".to_vec(), false, 1000).unwrap();
        assert_eq!(t1, "s1:1");
        assert_eq!(t2, "s1:2");
    }

    #[test]
    fn store_turn_head_is_latest() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"first".to_vec(), false, 1000).unwrap();
        s.store_turn("s1", b"second".to_vec(), false, 1000).unwrap();
        let head = s.sessions["s1"].ring.head().unwrap();
        assert_eq!(head.content, b"second");
    }

    #[test]
    fn store_turn_stores_interrupted() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"data".to_vec(), true, 1000).unwrap();
        let head = s.sessions["s1"].ring.head().unwrap();
        assert!(head.interrupted);
    }

    #[test]
    fn store_turn_session_not_found() {
        let mut s = state();
        assert_eq!(
            s.store_turn("nonexistent", b"data".to_vec(), false, 1000),
            Err("session_not_found")
        );
    }

    // -- Capture --

    #[test]
    fn capture_success() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"turn data".to_vec(), false, 1000)
            .unwrap();
        let result = s.capture("s1").unwrap();
        assert_eq!(result.size, 9);
        assert_eq!(result.turn_id, "s1:1");
        assert_eq!(
            s.relay_buffer.as_ref().unwrap().content,
            b"turn data".to_vec()
        );
    }

    #[test]
    fn capture_returns_turn_id() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"a".to_vec(), false, 1000).unwrap();
        s.store_turn("s1", b"b".to_vec(), false, 1000).unwrap();
        let result = s.capture("s1").unwrap();
        // Captures the head (latest = seq 2).
        assert_eq!(result.turn_id, "s1:2");
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
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        assert_eq!(s.capture("s1"), Err("no_turn"));
    }

    #[test]
    fn capture_does_not_clear_session_turn() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"turn data".to_vec(), false, 1000)
            .unwrap();
        s.capture("s1").unwrap();
        // Session's ring still has the turn.
        assert!(!s.sessions["s1"].ring.is_empty());
    }

    #[test]
    fn capture_overwrites_relay_buffer() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"first".to_vec(), false, 1000).unwrap();
        s.capture("s1").unwrap();
        s.store_turn("s1", b"second".to_vec(), false, 1000).unwrap();
        s.capture("s1").unwrap();
        assert_eq!(s.relay_buffer.as_ref().unwrap().content, b"second".to_vec());
    }

    // -- Paste --

    #[test]
    fn paste_success() {
        let mut s = state();
        let c1 = conn();
        let c2 = conn();
        s.add_connection(c1, Role::Wrapper);
        s.add_connection(c2, Role::Wrapper);
        s.register_session("s1".into(), c1, 100).unwrap();
        s.register_session("s2".into(), c2, 200).unwrap();
        s.store_turn("s1", b"turn data".to_vec(), false, 1000)
            .unwrap();
        s.capture("s1").unwrap();

        let (content, target) = s.paste_content("s2").unwrap();
        assert_eq!(content, b"turn data");
        assert_eq!(target, c2);
    }

    #[test]
    fn paste_buffer_empty() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        assert_eq!(s.paste_content("s1"), Err("buffer_empty"));
    }

    #[test]
    fn paste_session_not_found() {
        let mut s = state();
        s.relay_buffer = Some(RelayEntry {
            content: b"data".to_vec(),
            metadata: SinkMetadata {
                turn_id: "x:1".into(),
                timestamp: 1000,
                byte_length: 4,
                interrupted: false,
                truncated: false,
            },
        });
        assert_eq!(s.paste_content("nonexistent"), Err("session_not_found"));
    }

    #[test]
    fn paste_session_disconnected() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"turn data".to_vec(), false, 1000)
            .unwrap();
        s.capture("s1").unwrap();
        // Simulate disconnect without deregister.
        s.connections.remove(&c);
        assert_eq!(s.paste_content("s1"), Err("session_disconnected"));
    }

    #[test]
    fn paste_does_not_clear_relay_buffer() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"data".to_vec(), false, 1000).unwrap();
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
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"data".to_vec(), false, 1000).unwrap();

        let c2 = conn();
        s.add_connection(c2, Role::Wrapper);
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

    // -- Get turn --

    #[test]
    fn get_turn_hit() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"data".to_vec(), false, 1000).unwrap();
        let record = s.get_turn("s1:1").unwrap();
        assert_eq!(record.content, b"data");
    }

    #[test]
    fn get_turn_miss() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        assert_eq!(s.get_turn("s1:99"), Err("turn_not_found"));
    }

    #[test]
    fn get_turn_bad_format() {
        let s = state();
        assert_eq!(s.get_turn("no_colon"), Err("turn_not_found"));
    }

    #[test]
    fn get_turn_wrong_session() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"data".to_vec(), false, 1000).unwrap();
        assert_eq!(s.get_turn("s2:1"), Err("turn_not_found"));
    }

    // -- List turns --

    #[test]
    fn list_turns_ordering() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"a".to_vec(), false, 1000).unwrap();
        s.store_turn("s1", b"b".to_vec(), false, 1000).unwrap();
        s.store_turn("s1", b"c".to_vec(), false, 1000).unwrap();
        let turns = s.list_turns("s1", None).unwrap();
        let ids: Vec<&str> = turns.iter().map(|t| t.turn_id.as_str()).collect();
        assert_eq!(ids, vec!["s1:3", "s1:2", "s1:1"]);
    }

    #[test]
    fn list_turns_with_limit() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        for _ in 0..5 {
            s.store_turn("s1", b"x".to_vec(), false, 1000).unwrap();
        }
        let turns = s.list_turns("s1", Some(2)).unwrap();
        assert_eq!(turns.len(), 2);
    }

    #[test]
    fn list_turns_session_not_found() {
        let s = state();
        assert_eq!(s.list_turns("nonexistent", None), Err("session_not_found"));
    }

    // -- Capture by ID --

    #[test]
    fn capture_by_id_hit() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"first".to_vec(), false, 1000).unwrap();
        s.store_turn("s1", b"second".to_vec(), false, 1000).unwrap();
        // Capture the first turn, not the head.
        let result = s.capture_by_id("s1:1").unwrap();
        assert_eq!(result.turn_id, "s1:1");
        assert_eq!(result.size, 5);
        assert_eq!(s.relay_buffer.as_ref().unwrap().content, b"first".to_vec());
    }

    #[test]
    fn capture_by_id_miss() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        assert_eq!(s.capture_by_id("s1:99"), Err("turn_not_found"));
    }

    #[test]
    fn capture_by_id_wrong_session() {
        let mut s = state();
        assert_eq!(s.capture_by_id("nonexistent:1"), Err("turn_not_found"));
    }

    // -- Relay stores metadata --

    #[test]
    fn relay_buffer_stores_metadata() {
        let mut s = state();
        let c = conn();
        s.add_connection(c, Role::Wrapper);
        s.register_session("s1".into(), c, 100).unwrap();
        s.store_turn("s1", b"data".to_vec(), true, 5000).unwrap();
        s.capture("s1").unwrap();

        let (content, metadata) = s.relay_content().unwrap();
        assert_eq!(content, b"data");
        assert_eq!(metadata.turn_id, "s1:1");
        assert_eq!(metadata.timestamp, 5000);
        assert_eq!(metadata.byte_length, 4);
        assert!(metadata.interrupted);
        assert!(!metadata.truncated);
    }
}

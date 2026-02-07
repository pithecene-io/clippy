//! Turn registry — per-session ring buffer of metadata-bearing turn records.
//!
//! Replaces the single-slot `latest_turn: Option<Vec<u8>>` per session
//! with a bounded ring buffer of [`TurnRecord`] entries carrying stable
//! turn IDs and metadata. See CONTRACT_REGISTRY.md.

use std::collections::VecDeque;

/// A single completed turn stored in the ring buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecord {
    /// Stable turn identifier: `<session_id>:<seq>`.
    pub turn_id: String,
    /// Raw turn content (bytes, no interpretation).
    pub content: Vec<u8>,
    /// Unix epoch milliseconds when the turn was stored.
    pub timestamp: u64,
    /// Length of the original content in bytes (before truncation).
    pub byte_length: u32,
    /// Whether the turn was interrupted (signal-terminated).
    pub interrupted: bool,
    /// Whether the content was truncated to fit `max_turn_bytes`.
    pub truncated: bool,
}

/// Per-session ring buffer of completed turns.
///
/// Backed by a `VecDeque` with newest turns at the front.
/// When capacity is reached, the oldest turn is silently evicted.
#[derive(Debug)]
pub struct TurnRingBuffer {
    entries: VecDeque<TurnRecord>,
    capacity: usize,
    max_turn_bytes: usize,
    next_seq: u64,
    session_id: String,
}

impl TurnRingBuffer {
    /// Create a new ring buffer for the given session.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0. The ring buffer must hold at least one turn.
    pub fn new(session_id: String, capacity: usize, max_turn_bytes: usize) -> Self {
        assert!(capacity >= 1, "ring buffer capacity must be >= 1");
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
            max_turn_bytes,
            next_seq: 1,
            session_id,
        }
    }

    /// Push a new turn into the ring buffer.
    ///
    /// Assigns a monotonically increasing turn ID, truncates content
    /// if it exceeds `max_turn_bytes`, and evicts the oldest turn if
    /// the buffer is at capacity.
    ///
    /// `timestamp` is the detection-time Unix epoch millis, set by the
    /// wrapper when the turn was completed (CONTRACT_REGISTRY.md §73).
    ///
    /// Returns a reference to the newly inserted record.
    pub fn push(&mut self, mut content: Vec<u8>, interrupted: bool, timestamp: u64) -> &TurnRecord {
        let turn_id = format!("{}:{}", self.session_id, self.next_seq);
        self.next_seq += 1;

        let byte_length = content.len() as u32;
        let truncated = content.len() > self.max_turn_bytes;
        if truncated {
            content.truncate(self.max_turn_bytes);
        }

        let record = TurnRecord {
            turn_id,
            content,
            timestamp,
            byte_length,
            interrupted,
            truncated,
        };

        if self.entries.len() == self.capacity {
            self.entries.pop_back();
        }

        self.entries.push_front(record);
        &self.entries[0]
    }

    /// Get the most recent turn (ring head), or `None` if empty.
    pub fn head(&self) -> Option<&TurnRecord> {
        self.entries.front()
    }

    /// Look up a turn by its ID. Linear scan (capacity is small).
    #[allow(dead_code)]
    pub fn get(&self, turn_id: &str) -> Option<&TurnRecord> {
        self.entries.iter().find(|r| r.turn_id == turn_id)
    }

    /// Iterate turns newest-first, with an optional limit.
    #[allow(dead_code)]
    pub fn iter_newest_first(&self, limit: Option<usize>) -> impl Iterator<Item = &TurnRecord> {
        self.entries.iter().take(limit.unwrap_or(usize::MAX))
    }

    /// Number of turns currently stored.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ring buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(capacity: usize) -> TurnRingBuffer {
        TurnRingBuffer::new("test-session".into(), capacity, 4 * 1024 * 1024)
    }

    #[test]
    fn push_and_read_head() {
        let mut r = ring(4);
        r.push(b"hello".to_vec(), false, 1000);
        let head = r.head().unwrap();
        assert_eq!(head.content, b"hello");
        assert!(!head.interrupted);
        assert!(!head.truncated);
        assert_eq!(head.byte_length, 5);
    }

    #[test]
    fn turn_id_format() {
        let mut r = ring(4);
        r.push(b"a".to_vec(), false, 1000);
        assert_eq!(r.head().unwrap().turn_id, "test-session:1");
        r.push(b"b".to_vec(), false, 1000);
        assert_eq!(r.head().unwrap().turn_id, "test-session:2");
    }

    #[test]
    fn sequence_monotonically_increasing() {
        let mut r = ring(8);
        for i in 1..=5 {
            r.push(format!("turn-{i}").into_bytes(), false, 1000);
            assert_eq!(r.head().unwrap().turn_id, format!("test-session:{i}"));
        }
    }

    #[test]
    fn ring_eviction_at_capacity() {
        let mut r = ring(3);
        r.push(b"a".to_vec(), false, 1000); // seq 1
        r.push(b"b".to_vec(), false, 1000); // seq 2
        r.push(b"c".to_vec(), false, 1000); // seq 3
        assert_eq!(r.len(), 3);

        r.push(b"d".to_vec(), false, 1000); // seq 4 — evicts seq 1
        assert_eq!(r.len(), 3);
        assert!(r.get("test-session:1").is_none(), "seq 1 should be evicted");
        assert!(r.get("test-session:2").is_some());
        assert!(r.get("test-session:4").is_some());
    }

    #[test]
    fn truncation_at_max_turn_bytes() {
        let mut r = TurnRingBuffer::new("s".into(), 4, 10);
        let content = vec![0u8; 20];
        r.push(content, false, 1000);
        let head = r.head().unwrap();
        assert!(head.truncated);
        assert_eq!(head.content.len(), 10);
        assert_eq!(head.byte_length, 20); // Original length preserved
    }

    #[test]
    fn no_truncation_within_limit() {
        let mut r = TurnRingBuffer::new("s".into(), 4, 100);
        r.push(vec![0u8; 50], false, 1000);
        let head = r.head().unwrap();
        assert!(!head.truncated);
        assert_eq!(head.content.len(), 50);
        assert_eq!(head.byte_length, 50);
    }

    #[test]
    fn get_hit_and_miss() {
        let mut r = ring(4);
        r.push(b"data".to_vec(), false, 1000);
        assert!(r.get("test-session:1").is_some());
        assert!(r.get("test-session:999").is_none());
        assert!(r.get("other-session:1").is_none());
    }

    #[test]
    fn iter_newest_first_ordering() {
        let mut r = ring(4);
        r.push(b"first".to_vec(), false, 1000);
        r.push(b"second".to_vec(), false, 1000);
        r.push(b"third".to_vec(), false, 1000);

        let ids: Vec<&str> = r
            .iter_newest_first(None)
            .map(|t| t.turn_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["test-session:3", "test-session:2", "test-session:1"]
        );
    }

    #[test]
    fn iter_newest_first_with_limit() {
        let mut r = ring(8);
        for _ in 0..5 {
            r.push(b"x".to_vec(), false, 1000);
        }
        let count = r.iter_newest_first(Some(2)).count();
        assert_eq!(count, 2);
    }

    #[test]
    fn empty_ring_head_is_none() {
        let r = ring(4);
        assert!(r.head().is_none());
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn timestamp_preserved_from_caller() {
        let mut r = ring(4);
        r.push(b"data".to_vec(), false, 1700000000000);
        assert_eq!(r.head().unwrap().timestamp, 1700000000000);
    }

    #[test]
    fn interrupted_flag_stored() {
        let mut r = ring(4);
        r.push(b"data".to_vec(), true, 1000);
        assert!(r.head().unwrap().interrupted);
    }

    #[test]
    fn metadata_correctness() {
        let mut r = ring(4);
        r.push(b"hello world".to_vec(), true, 42000);
        let head = r.head().unwrap();
        assert_eq!(head.byte_length, 11);
        assert!(head.interrupted);
        assert!(!head.truncated);
        assert_eq!(head.timestamp, 42000);
        assert_eq!(head.turn_id, "test-session:1");
    }

    #[test]
    fn sequence_continues_after_eviction() {
        let mut r = ring(2);
        r.push(b"a".to_vec(), false, 1000); // seq 1
        r.push(b"b".to_vec(), false, 1000); // seq 2
        r.push(b"c".to_vec(), false, 1000); // seq 3 — evicts seq 1
        assert_eq!(r.head().unwrap().turn_id, "test-session:3");
        // Sequence never resets
        r.push(b"d".to_vec(), false, 1000); // seq 4
        assert_eq!(r.head().unwrap().turn_id, "test-session:4");
    }

    #[test]
    fn capacity_one_ring() {
        let mut r = ring(1);
        r.push(b"first".to_vec(), false, 1000);
        assert_eq!(r.len(), 1);
        r.push(b"second".to_vec(), false, 1000);
        assert_eq!(r.len(), 1);
        assert_eq!(r.head().unwrap().content, b"second");
        assert!(r.get("test-session:1").is_none());
    }

    #[test]
    #[should_panic(expected = "ring buffer capacity must be >= 1")]
    fn capacity_zero_panics() {
        TurnRingBuffer::new("s".into(), 0, 4096);
    }
}

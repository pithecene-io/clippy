//! Turn detection — prompt-pattern matching and turn extraction.
//!
//! See CONTRACT_TURN.md.
//!
//! The [`TurnDetector`] is a state machine that consumes agent output
//! byte-by-byte, detects prompt patterns (after ANSI stripping), and
//! emits [`TurnEvent`]s when turn boundaries are found.

pub mod ansi;
pub mod presets;

use std::time::{SystemTime, UNIX_EPOCH};

use ansi::AnsiStripper;
use regex::Regex;

/// Errors that can occur when constructing a [`TurnDetector`].
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error(
        "prompt pattern contains literal newline — multi-line patterns are not supported in v0"
    )]
    MultiLinePattern,
    #[error("invalid regex pattern: {0}")]
    InvalidPattern(#[from] regex::Error),
}

/// A completed turn — the agent output between user input and the
/// next prompt.
#[derive(Debug, Clone)]
pub struct Turn {
    /// Raw bytes of the turn content (ANSI sequences preserved).
    pub content: Vec<u8>,
    /// Whether the turn was interrupted (e.g., Ctrl+C).
    pub interrupted: bool,
    /// Unix epoch milliseconds when the turn was detected.
    pub timestamp: u64,
}

/// Current time as Unix epoch milliseconds.
pub(crate) fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as u64
}

/// Events emitted by the turn detector.
#[derive(Debug)]
pub enum TurnEvent {
    /// The agent's initial prompt was detected — the session is ready
    /// for user input. No turn is produced.
    SessionReady,
    /// A complete turn was detected.
    TurnCompleted(Turn),
}

/// Internal state of the turn detector state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectorState {
    /// Waiting for the agent to show its first prompt.
    AwaitingFirstPrompt,
    /// A prompt was shown — waiting for the user to submit input.
    AwaitingUserInput,
    /// User has submitted input — accumulating agent output until the
    /// next prompt.
    AccumulatingOutput,
}

/// Prompt-pattern turn detector.
///
/// Feed agent output via [`feed_output`] and user input notifications
/// via [`notify_user_input`]. The detector emits [`TurnEvent`]s when
/// turn boundaries are found.
///
/// # Contract compliance
///
/// - Prompt matching is per-line, after ANSI stripping.
/// - First prompt → `SessionReady` (no turn).
/// - Consecutive prompts without output → no empty turns.
/// - Interrupted turns are marked.
/// - ANSI sequences are preserved in turn content.
#[derive(Debug)]
pub struct TurnDetector {
    pattern: Regex,
    state: DetectorState,
    stripper: AnsiStripper,

    /// Accumulates the current line for prompt detection (ANSI-stripped).
    line_buf: Vec<u8>,

    /// Accumulates raw output bytes for the current turn content.
    /// Lines are moved here once complete (newline encountered).
    /// The current partial line is in `raw_line_buf`.
    content_buf: Vec<u8>,

    /// Accumulates raw bytes for the current (possibly incomplete) line.
    raw_line_buf: Vec<u8>,

    /// Whether the current turn was interrupted.
    interrupted: bool,
}

impl TurnDetector {
    /// Create a new turn detector with the given prompt pattern.
    ///
    /// If `pattern` matches a known preset name, the preset regex is
    /// used. Otherwise, `pattern` is compiled as a custom regex.
    ///
    /// Returns an error if the pattern contains literal newlines or
    /// is not a valid regex.
    pub fn new(pattern: &str) -> Result<Self, TurnError> {
        // Resolve preset or use as custom regex.
        let pattern_str = presets::preset_pattern(pattern).unwrap_or(pattern);

        // Reject multi-line patterns (CONTRACT_TURN.md §Matching rules).
        if pattern_str.contains('\n') {
            return Err(TurnError::MultiLinePattern);
        }

        let regex = Regex::new(pattern_str)?;

        Ok(Self {
            pattern: regex,
            state: DetectorState::AwaitingFirstPrompt,
            stripper: AnsiStripper::new(),
            line_buf: Vec::new(),
            content_buf: Vec::new(),
            raw_line_buf: Vec::new(),
            interrupted: false,
        })
    }

    /// Feed agent output bytes to the detector.
    ///
    /// Returns any events produced by processing this chunk. Output
    /// is processed byte-by-byte: lines are assembled and checked
    /// against the prompt pattern after ANSI stripping.
    ///
    /// **Echo-stripping**: The caller (PTY wrapper) is responsible for
    /// excluding echoed user input before feeding data here.
    /// CONTRACT_TURN.md §126–136 requires turn content to exclude
    /// echoed input, but the mechanism is an upstream concern.
    pub fn feed_output(&mut self, data: &[u8]) -> Vec<TurnEvent> {
        let mut events = Vec::new();

        for &byte in data {
            self.raw_line_buf.push(byte);

            // Strip ANSI for prompt detection.
            let stripped = self.stripper.strip(&[byte]);
            self.line_buf.extend_from_slice(&stripped);

            if byte == b'\n' {
                // Line complete — check for prompt match.
                self.process_line(&mut events);
            }
        }

        events
    }

    /// Notify the detector that the user has submitted input.
    ///
    /// Transitions from `AwaitingUserInput` to `AccumulatingOutput`.
    /// No-op in other states.
    pub fn notify_user_input(&mut self) {
        if self.state == DetectorState::AwaitingUserInput {
            self.state = DetectorState::AccumulatingOutput;
            self.content_buf.clear();
            self.interrupted = false;
        }
    }

    /// Notify the detector that the user interrupted the agent (e.g., Ctrl+C).
    ///
    /// Sets the interrupted flag on the current turn. Only meaningful
    /// in `AccumulatingOutput` state.
    pub fn notify_interrupt(&mut self) {
        if self.state == DetectorState::AccumulatingOutput {
            self.interrupted = true;
        }
    }

    /// Check the current line against the prompt pattern and handle
    /// state transitions.
    fn process_line(&mut self, events: &mut Vec<TurnEvent>) {
        // Trim trailing newline/carriage return — we use newlines as
        // line delimiters, but they shouldn't be part of the match input.
        let line = &self.line_buf;
        let trimmed = match line.last() {
            Some(b'\n') => {
                let end = line.len() - 1;
                if end > 0 && line[end - 1] == b'\r' {
                    &line[..end - 1]
                } else {
                    &line[..end]
                }
            }
            _ => line.as_slice(),
        };
        let line_str = String::from_utf8_lossy(trimmed);
        let is_prompt = self.pattern.is_match(&line_str);

        if is_prompt {
            match self.state {
                DetectorState::AwaitingFirstPrompt => {
                    events.push(TurnEvent::SessionReady);
                    self.state = DetectorState::AwaitingUserInput;
                }
                DetectorState::AwaitingUserInput => {
                    // Consecutive prompt without intervening output.
                    // No empty turn produced (CONTRACT_TURN.md §Matching rules).
                }
                DetectorState::AccumulatingOutput => {
                    // Turn boundary — emit completed turn.
                    // Content is everything accumulated so far, excluding
                    // the prompt line itself.
                    let content = std::mem::take(&mut self.content_buf);

                    if !content.is_empty() {
                        events.push(TurnEvent::TurnCompleted(Turn {
                            content,
                            interrupted: self.interrupted,
                            timestamp: epoch_millis(),
                        }));
                    }
                    // Even if content was empty (e.g., only whitespace
                    // was accumulated), transition to awaiting input.
                    self.interrupted = false;
                    self.state = DetectorState::AwaitingUserInput;
                }
            }
        } else if self.state == DetectorState::AccumulatingOutput {
            // Non-prompt line during output accumulation — append to
            // turn content (raw bytes, ANSI preserved).
            self.content_buf.extend_from_slice(&self.raw_line_buf);
        }

        self.line_buf.clear();
        self.raw_line_buf.clear();
    }

    /// Check for a prompt match on a partial (unterminated) line.
    ///
    /// Some agents emit a prompt without a trailing newline. This
    /// method allows the PTY wrapper to flush the line buffer when
    /// idle (e.g., after a read timeout with no new data).
    pub fn flush_line(&mut self) -> Vec<TurnEvent> {
        if self.line_buf.is_empty() {
            return Vec::new();
        }

        let mut events = Vec::new();
        self.process_line(&mut events);
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector(pattern: &str) -> TurnDetector {
        TurnDetector::new(pattern).unwrap()
    }

    // -- Construction --

    #[test]
    fn preset_names_resolve() {
        assert!(TurnDetector::new("claude").is_ok());
        assert!(TurnDetector::new("aider").is_ok());
        assert!(TurnDetector::new("generic").is_ok());
    }

    #[test]
    fn custom_regex_accepted() {
        assert!(TurnDetector::new(r"my-prompt>").is_ok());
    }

    #[test]
    fn multiline_pattern_rejected() {
        let err = TurnDetector::new("foo\nbar").unwrap_err();
        assert!(matches!(err, TurnError::MultiLinePattern));
    }

    #[test]
    fn invalid_regex_rejected() {
        let err = TurnDetector::new(r"(unclosed").unwrap_err();
        assert!(matches!(err, TurnError::InvalidPattern(_)));
    }

    // -- State transitions --

    #[test]
    fn first_prompt_emits_session_ready() {
        let mut d = detector(r"^> $");
        let events = d.feed_output(b"> \n");

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TurnEvent::SessionReady));
    }

    #[test]
    fn no_turn_before_user_input() {
        let mut d = detector(r"^> $");
        // First prompt → session ready
        let events = d.feed_output(b"> \n");
        assert!(matches!(events[0], TurnEvent::SessionReady));

        // Second prompt without user input → no turn
        let events = d.feed_output(b"> \n");
        assert!(events.is_empty());
    }

    #[test]
    fn basic_turn_detection() {
        let mut d = detector(r"^> $");

        // First prompt → session ready
        d.feed_output(b"> \n");

        // User types something
        d.notify_user_input();

        // Agent produces output + next prompt
        let events = d.feed_output(b"hello world\n> \n");

        assert_eq!(events.len(), 1);
        match &events[0] {
            TurnEvent::TurnCompleted(turn) => {
                assert_eq!(turn.content, b"hello world\n");
                assert!(!turn.interrupted);
            }
            _ => panic!("expected TurnCompleted"),
        }
    }

    #[test]
    fn multi_line_output_turn() {
        let mut d = detector(r"^> $");
        d.feed_output(b"> \n");
        d.notify_user_input();

        let events = d.feed_output(b"line 1\nline 2\nline 3\n> \n");

        assert_eq!(events.len(), 1);
        match &events[0] {
            TurnEvent::TurnCompleted(turn) => {
                assert_eq!(turn.content, b"line 1\nline 2\nline 3\n");
            }
            _ => panic!("expected TurnCompleted"),
        }
    }

    #[test]
    fn interrupted_turn_flagged() {
        let mut d = detector(r"^> $");
        d.feed_output(b"> \n");
        d.notify_user_input();

        d.feed_output(b"partial out");
        d.notify_interrupt();

        let events = d.feed_output(b"put\n> \n");

        assert_eq!(events.len(), 1);
        match &events[0] {
            TurnEvent::TurnCompleted(turn) => {
                assert!(turn.interrupted);
            }
            _ => panic!("expected TurnCompleted"),
        }
    }

    #[test]
    fn consecutive_prompts_no_empty_turns() {
        let mut d = detector(r"^> $");
        d.feed_output(b"> \n");
        d.notify_user_input();

        // Agent shows output then multiple prompts
        let events = d.feed_output(b"output\n> \n> \n> \n");

        // Only one turn completed (from the first prompt after output)
        let turn_count = events
            .iter()
            .filter(|e| matches!(e, TurnEvent::TurnCompleted(_)))
            .count();
        assert_eq!(turn_count, 1);
    }

    #[test]
    fn ansi_stripped_for_detection() {
        // Prompt wrapped in color codes
        let mut d = detector(r"^> $");
        let events = d.feed_output(b"\x1b[32m> \x1b[0m\n");

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TurnEvent::SessionReady));
    }

    #[test]
    fn ansi_preserved_in_content() {
        let mut d = detector(r"^> $");
        d.feed_output(b"> \n");
        d.notify_user_input();

        let colored = b"\x1b[31mred text\x1b[0m\n> \n";
        let events = d.feed_output(colored);

        assert_eq!(events.len(), 1);
        match &events[0] {
            TurnEvent::TurnCompleted(turn) => {
                // Content preserves ANSI escapes
                assert_eq!(turn.content, b"\x1b[31mred text\x1b[0m\n");
            }
            _ => panic!("expected TurnCompleted"),
        }
    }

    #[test]
    fn multiple_turns_in_sequence() {
        let mut d = detector(r"^> $");
        d.feed_output(b"> \n");

        // Turn 1
        d.notify_user_input();
        let events = d.feed_output(b"output 1\n> \n");
        assert_eq!(events.len(), 1);

        // Turn 2
        d.notify_user_input();
        let events = d.feed_output(b"output 2\n> \n");
        assert_eq!(events.len(), 1);
        match &events[0] {
            TurnEvent::TurnCompleted(turn) => {
                assert_eq!(turn.content, b"output 2\n");
            }
            _ => panic!("expected TurnCompleted"),
        }
    }

    #[test]
    fn chunked_output() {
        let mut d = detector(r"^> $");
        d.feed_output(b"> \n");
        d.notify_user_input();

        // Output arrives in small chunks
        let mut all_events = Vec::new();
        all_events.extend(d.feed_output(b"hel"));
        all_events.extend(d.feed_output(b"lo wor"));
        all_events.extend(d.feed_output(b"ld\n"));
        all_events.extend(d.feed_output(b"> \n"));

        let turns: Vec<_> = all_events
            .iter()
            .filter(|e| matches!(e, TurnEvent::TurnCompleted(_)))
            .collect();
        assert_eq!(turns.len(), 1);
    }

    #[test]
    fn flush_detects_unterminated_prompt() {
        let mut d = detector(r"^> $");

        // Prompt without trailing newline
        d.feed_output(b"> ");
        let events = d.flush_line();

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TurnEvent::SessionReady));
    }

    #[test]
    fn no_turn_without_output() {
        let mut d = detector(r"^> $");
        d.feed_output(b"> \n");
        d.notify_user_input();

        // Immediate prompt with no agent output between
        let events = d.feed_output(b"> \n");

        // No turn (empty content suppressed)
        let turn_count = events
            .iter()
            .filter(|e| matches!(e, TurnEvent::TurnCompleted(_)))
            .count();
        assert_eq!(turn_count, 0);
    }

    #[test]
    fn prompt_match_anywhere_in_line() {
        // Pattern that matches anywhere (not anchored)
        let mut d = detector(r"PROMPT>");
        let events = d.feed_output(b"some prefix PROMPT> and suffix\n");

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TurnEvent::SessionReady));
    }

    #[test]
    fn notify_user_input_noop_in_wrong_state() {
        let mut d = detector(r"^> $");

        // Before first prompt — noop
        d.notify_user_input();
        assert_eq!(d.state, DetectorState::AwaitingFirstPrompt);

        // After first prompt — transitions
        d.feed_output(b"> \n");
        d.notify_user_input();
        assert_eq!(d.state, DetectorState::AccumulatingOutput);

        // During accumulation — noop (stays accumulating)
        d.notify_user_input();
        assert_eq!(d.state, DetectorState::AccumulatingOutput);
    }

    #[test]
    fn notify_interrupt_noop_outside_accumulating() {
        let mut d = detector(r"^> $");

        // Before first prompt — noop, no panic
        d.notify_interrupt();

        d.feed_output(b"> \n");

        // Awaiting input — noop, no panic
        d.notify_interrupt();
    }
}

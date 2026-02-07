//! Output formatting for CLI client commands.
//!
//! Design: human-readable tables and status lines. For `get-turn`,
//! metadata goes to stderr and raw content to stdout so that piping
//! works naturally (`clippyctl client get-turn s1:5 | less`).

use std::io::{self, Write};

use crate::ipc::protocol::{SessionDescriptor, TurnDescriptor};

use super::broker_client::{CaptureResult, GetTurnResult};

/// Print session descriptors as a table to stdout.
pub fn print_sessions(sessions: &[SessionDescriptor]) {
    if sessions.is_empty() {
        println!("No active sessions");
        return;
    }

    println!("{:<40} {:>8} HAS_TURN", "SESSION", "PID");
    println!("{}", "-".repeat(60));
    for s in sessions {
        println!(
            "{:<40} {:>8} {}",
            s.session,
            s.pid,
            if s.has_turn { "yes" } else { "no" }
        );
    }
}

/// Print turn descriptors as a table to stdout.
pub fn print_turns(turns: &[TurnDescriptor]) {
    if turns.is_empty() {
        println!("No turns in history");
        return;
    }

    println!("{:<24} {:>10} {:>16} FLAGS", "TURN_ID", "SIZE", "TIMESTAMP");
    println!("{}", "-".repeat(70));
    for t in turns {
        println!(
            "{:<24} {:>10} {:>16} {}",
            t.turn_id,
            t.byte_length,
            t.timestamp,
            format_flags(t.interrupted, t.truncated),
        );
    }
}

/// Print turn content and metadata.
///
/// Metadata header goes to stderr, raw content to stdout. With
/// `metadata_only`, everything goes to stdout and content is omitted.
pub fn print_turn(
    turn_id: &str,
    result: &GetTurnResult,
    metadata_only: bool,
) -> Result<(), io::Error> {
    let flags = format_flags(result.interrupted, result.truncated);

    if metadata_only {
        println!("Turn:      {turn_id}");
        println!("Size:      {} bytes", result.byte_length);
        println!("Timestamp: {}", result.timestamp);
        println!("Flags:     {flags}");
    } else {
        eprintln!("Turn:      {turn_id}");
        eprintln!("Size:      {} bytes", result.byte_length);
        eprintln!("Timestamp: {}", result.timestamp);
        eprintln!("Flags:     {flags}");
        eprintln!("---");
        let mut stdout = io::stdout().lock();
        stdout.write_all(&result.content)?;
    }

    Ok(())
}

/// Print capture/capture-by-id result.
pub fn print_capture(result: &CaptureResult) {
    println!("Captured {} ({} bytes)", result.turn_id, result.size);
}

/// Print paste success.
pub fn print_paste(session: &str) {
    println!("Pasted to session {session}");
}

/// Print deliver success.
pub fn print_deliver(sink: &str) {
    println!("Delivered to {sink} sink");
}

/// Format interrupted/truncated flags as a comma-separated string.
fn format_flags(interrupted: bool, truncated: bool) -> String {
    let mut flags = Vec::new();
    if interrupted {
        flags.push("interrupted");
    }
    if truncated {
        flags.push("truncated");
    }
    if flags.is_empty() {
        "-".to_string()
    } else {
        flags.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_flags_none() {
        assert_eq!(format_flags(false, false), "-");
    }

    #[test]
    fn format_flags_interrupted() {
        assert_eq!(format_flags(true, false), "interrupted");
    }

    #[test]
    fn format_flags_truncated() {
        assert_eq!(format_flags(false, true), "truncated");
    }

    #[test]
    fn format_flags_both() {
        assert_eq!(format_flags(true, true), "interrupted,truncated");
    }
}

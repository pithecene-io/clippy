//! ANSI escape sequence stripper.
//!
//! Implements a state machine that removes ANSI escape sequences from
//! a byte stream while preserving all other content. Used for prompt
//! detection only — turn content retains ANSI sequences verbatim.

/// Internal parser states for the ANSI stripping state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Normal text — pass bytes through.
    Ground,
    /// Saw ESC (0x1B), waiting for next byte to classify the sequence.
    Escape,
    /// Inside a CSI sequence (ESC [ ...). Consuming until final byte.
    Csi,
    /// Inside an nF escape sequence (ESC + intermediate bytes 0x20-0x2F
    /// + final byte 0x30-0x7E). E.g. ESC ( B (designate G0 charset).
    EscapeIntermediate,
    /// Inside an OSC sequence (ESC ] ...). Consuming until BEL or ST.
    Osc,
    /// Inside OSC, saw ESC — expecting '\' for ST (String Terminator).
    OscEscape,
}

/// Strips ANSI escape sequences from a byte slice.
///
/// Returns a new `Vec<u8>` containing only the visible text content.
/// This is a stateless convenience wrapper — each call processes a
/// complete buffer independently.
#[allow(dead_code)]
pub fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut stripper = AnsiStripper::new();
    stripper.strip(input)
}

/// Stateful ANSI escape sequence stripper.
///
/// Maintains parser state across calls to [`strip`] so that escape
/// sequences split across chunk boundaries are handled correctly.
#[derive(Debug)]
pub struct AnsiStripper {
    state: State,
}

impl AnsiStripper {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
        }
    }

    /// Strip ANSI escape sequences from `input`, returning visible text.
    ///
    /// State is preserved between calls — a sequence that starts in one
    /// chunk and ends in the next is handled correctly.
    pub fn strip(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(input.len());

        for &byte in input {
            match self.state {
                State::Ground => {
                    if byte == 0x1B {
                        self.state = State::Escape;
                    } else {
                        output.push(byte);
                    }
                }
                State::Escape => match byte {
                    b'[' => self.state = State::Csi,
                    b']' => self.state = State::Osc,
                    // Two-character sequences: ESC followed by a single
                    // byte in the 0x40..0x5F range (C1 control shorthand)
                    // or common sequences like ESC ( B, ESC ) 0, etc.
                    // For simplicity, consume one byte after ESC for
                    // sequences that aren't CSI or OSC.
                    0x20..=0x2F => {
                        // Intermediate byte — start of an nF escape
                        // sequence (e.g. ESC ( B for charset select).
                        // Consume intermediate bytes then a final byte.
                        self.state = State::EscapeIntermediate;
                    }
                    _ => {
                        // Single-character escape sequence (e.g., ESC M,
                        // ESC 7, ESC 8, ESC =, ESC >, etc.)
                        self.state = State::Ground;
                    }
                },
                State::Csi => {
                    // CSI sequences: ESC [ (parameter bytes 0x30-0x3F)*
                    //                      (intermediate bytes 0x20-0x2F)*
                    //                      (final byte 0x40-0x7E)
                    if (0x40..=0x7E).contains(&byte) {
                        self.state = State::Ground;
                    }
                    // Otherwise consume parameter/intermediate bytes.
                }
                State::EscapeIntermediate => {
                    // nF sequences: ESC (intermediate 0x20-0x2F)+
                    //               (final 0x30-0x7E)
                    // Consume additional intermediate bytes; transition
                    // to Ground on the final byte.
                    if (0x20..=0x2F).contains(&byte) {
                        // More intermediate bytes — stay.
                    } else {
                        // Final byte (or unexpected) — sequence done.
                        self.state = State::Ground;
                    }
                }
                State::Osc => {
                    // OSC sequences end with BEL (0x07) or ST (ESC \).
                    if byte == 0x07 {
                        self.state = State::Ground;
                    } else if byte == 0x1B {
                        self.state = State::OscEscape;
                    }
                    // Otherwise consume OSC content.
                }
                State::OscEscape => {
                    // Expecting '\' to complete ST (String Terminator).
                    if byte == b'\\' {
                        self.state = State::Ground;
                    } else {
                        // Malformed — treat as new escape sequence.
                        self.state = State::Escape;
                        // Re-process this byte as if we just saw ESC.
                        match byte {
                            b'[' => self.state = State::Csi,
                            b']' => self.state = State::Osc,
                            _ => self.state = State::Ground,
                        }
                    }
                }
            }
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(strip_ansi(b"hello world"), b"hello world");
    }

    #[test]
    fn empty_input() {
        assert_eq!(strip_ansi(b""), b"");
    }

    #[test]
    fn sgr_colors_stripped() {
        // ESC[31m hello ESC[0m
        let input = b"\x1b[31mhello\x1b[0m";
        assert_eq!(strip_ansi(input), b"hello");
    }

    #[test]
    fn multiple_sgr_sequences() {
        let input = b"\x1b[1;32mbold green\x1b[0m normal \x1b[4munderline\x1b[0m";
        assert_eq!(strip_ansi(input), b"bold green normal underline");
    }

    #[test]
    fn csi_cursor_movement() {
        // ESC[2J (clear screen) + ESC[H (cursor home)
        let input = b"\x1b[2J\x1b[Hprompt> ";
        assert_eq!(strip_ansi(input), b"prompt> ");
    }

    #[test]
    fn osc_title_with_bel() {
        // ESC]0;window title BEL
        let input = b"\x1b]0;my terminal\x07prompt> ";
        assert_eq!(strip_ansi(input), b"prompt> ");
    }

    #[test]
    fn osc_title_with_st() {
        // ESC]0;window title ESC\
        let input = b"\x1b]0;my terminal\x1b\\prompt> ";
        assert_eq!(strip_ansi(input), b"prompt> ");
    }

    #[test]
    fn single_char_escape() {
        // ESC M (reverse index) + text
        let input = b"\x1bMhello";
        assert_eq!(strip_ansi(input), b"hello");
    }

    #[test]
    fn split_across_chunks() {
        let mut stripper = AnsiStripper::new();

        // ESC[31m split across two chunks
        let out1 = stripper.strip(b"before\x1b[3");
        let out2 = stripper.strip(b"1mafter");

        let mut combined = out1;
        combined.extend(out2);
        assert_eq!(combined, b"beforeafter");
    }

    #[test]
    fn newlines_preserved() {
        let input = b"\x1b[32mline1\nline2\x1b[0m\nline3";
        assert_eq!(strip_ansi(input), b"line1\nline2\nline3");
    }

    #[test]
    fn bare_esc_at_end() {
        let mut stripper = AnsiStripper::new();
        let out = stripper.strip(b"hello\x1b");
        assert_eq!(out, b"hello");
        // Next chunk completes the sequence
        let out2 = stripper.strip(b"[0mworld");
        assert_eq!(out2, b"world");
    }

    #[test]
    fn interleaved_text_and_escapes() {
        let input = b"a\x1b[1mb\x1b[2mc\x1b[3md";
        assert_eq!(strip_ansi(input), b"abcd");
    }

    #[test]
    fn nf_charset_select_stripped() {
        // ESC ( B — designate G0 charset as ASCII
        let input = b"\x1b(Bhello";
        assert_eq!(strip_ansi(input), b"hello");
    }

    #[test]
    fn nf_charset_select_g1() {
        // ESC ) 0 — designate G1 charset as line drawing
        let input = b"\x1b)0world";
        assert_eq!(strip_ansi(input), b"world");
    }

    #[test]
    fn nf_sequence_does_not_leak_final_byte() {
        // ESC ( B should consume all three bytes; 'B' must not leak.
        let input = b"before\x1b(Bafter";
        assert_eq!(strip_ansi(input), b"beforeafter");
    }

    #[test]
    fn nf_multiple_intermediate_bytes() {
        // Hypothetical nF sequence with two intermediate bytes + final.
        let input = b"\x1b #8visible";
        assert_eq!(strip_ansi(input), b"visible");
    }

    #[test]
    fn nf_split_across_chunks() {
        let mut stripper = AnsiStripper::new();
        let out1 = stripper.strip(b"before\x1b(");
        let out2 = stripper.strip(b"Bafter");

        let mut combined = out1;
        combined.extend(out2);
        assert_eq!(combined, b"beforeafter");
    }
}

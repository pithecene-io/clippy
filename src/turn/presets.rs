//! Prompt-pattern presets for common agents.
//!
//! Exact patterns are placeholders until validated against real agent
//! output. Custom `--pattern` works as an escape hatch.
//!
//! See CONTRACT_TURN.md §Presets.

/// Returns the regex pattern string for a named preset, or `None` if
/// the name is not a recognized preset (in which case it should be
/// treated as a custom regex).
pub fn preset_pattern(name: &str) -> Option<&'static str> {
    match name {
        "claude" => Some(CLAUDE),
        "aider" => Some(AIDER),
        "generic" => Some(GENERIC),
        _ => None,
    }
}

/// Claude Code CLI prompt pattern.
///
/// Placeholder — needs validation against real Claude Code output.
/// Matches lines ending with a `>` followed by optional whitespace,
/// typical of the Claude Code interactive prompt.
const CLAUDE: &str = r"(?:^|\n)\s*>\s*$";

/// Aider CLI prompt pattern.
///
/// Placeholder — needs validation against real Aider output.
/// Matches the aider prompt which typically looks like `aider> ` or
/// contains the repo name.
const AIDER: &str = r"(?:^|\n)[\w/.-]*>\s*$";

/// Generic shell-style prompt pattern.
///
/// Matches common `$ ` or `> ` style prompts at the end of a line.
/// Deliberately broad — intended as a fallback.
const GENERIC: &str = r"[>$#%]\s*$";

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn known_presets_resolve() {
        assert!(preset_pattern("claude").is_some());
        assert!(preset_pattern("aider").is_some());
        assert!(preset_pattern("generic").is_some());
    }

    #[test]
    fn unknown_names_return_none() {
        assert!(preset_pattern("unknown").is_none());
        assert!(preset_pattern("").is_none());
    }

    #[test]
    fn preset_patterns_are_valid_regex() {
        for name in &["claude", "aider", "generic"] {
            let pattern = preset_pattern(name).unwrap();
            Regex::new(pattern).unwrap_or_else(|e| {
                panic!("preset '{name}' has invalid regex: {e}");
            });
        }
    }

    #[test]
    fn generic_matches_common_prompts() {
        let re = Regex::new(preset_pattern("generic").unwrap()).unwrap();
        assert!(re.is_match("$ "));
        assert!(re.is_match("> "));
        assert!(re.is_match("# "));
        assert!(re.is_match("user@host:~$ "));
    }

    #[test]
    fn generic_does_not_match_plain_text() {
        let re = Regex::new(preset_pattern("generic").unwrap()).unwrap();
        assert!(!re.is_match("hello world"));
        assert!(!re.is_match("no prompt here"));
    }
}

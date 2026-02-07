//! Key binding parser — "Super+Shift+C" → (modifier mask, keycode).
//!
//! Parses user-provided key binding strings into X11 modifier masks
//! and keycodes. See CONTRACT_HOTKEY.md §84–90.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, Keysym, ModMask, Setup};

use super::HotkeyError;

/// A parsed key binding ready for X11 grab registration.
#[derive(Debug, Clone)]
pub struct Binding {
    /// X11 modifier mask (e.g., Shift | Mod4).
    pub modifiers: u16,
    /// X11 keycode for the key.
    pub keycode: u8,
    /// X11 keysym for display/logging.
    #[allow(dead_code)]
    pub keysym: Keysym,
    /// Original user-provided string.
    pub raw: String,
}

/// Parse a key binding specification string into an X11 binding.
///
/// Format: `Modifier[+Modifier...]+Key`
///
/// Modifiers: `Shift`, `Control`/`Ctrl`, `Alt`, `Super`
/// Keys: single letter (A-Z), digit (0-9), F1-F12, or named key
/// (space, return, escape, tab, backspace).
///
/// At least one modifier is required (CONTRACT_HOTKEY.md §89-90).
pub fn parse_binding(
    spec: &str,
    conn: &impl Connection,
    setup: &Setup,
) -> Result<Binding, HotkeyError> {
    let parts: Vec<&str> = spec.split('+').map(str::trim).collect();

    if parts.is_empty() {
        return Err(HotkeyError::InvalidBinding("empty binding".into()));
    }
    if parts.len() < 2 {
        return Err(HotkeyError::InvalidBinding(format!(
            "bare key without modifier: {spec:?}"
        )));
    }

    // Last part is the key name, everything before is modifiers.
    let (modifier_parts, key_name) = parts.split_at(parts.len() - 1);
    let key_name = key_name[0];

    // Parse modifiers.
    let mut modifiers: u16 = 0;
    for &m in modifier_parts {
        let mask = parse_modifier(m)
            .ok_or_else(|| HotkeyError::InvalidBinding(format!("unknown modifier: {m:?}")))?;
        modifiers |= mask;
    }

    if modifiers == 0 {
        return Err(HotkeyError::InvalidBinding(format!(
            "no valid modifiers in: {spec:?}"
        )));
    }

    // Parse key name → keysym.
    let keysym = key_name_to_keysym(key_name)
        .ok_or_else(|| HotkeyError::InvalidBinding(format!("unknown key: {key_name:?}")))?;

    // Resolve keysym → keycode via keyboard mapping.
    let keycode = keysym_to_keycode(conn, setup, keysym).ok_or_else(|| {
        HotkeyError::InvalidBinding(format!(
            "keysym 0x{keysym:04x} not found in keyboard mapping"
        ))
    })?;

    Ok(Binding {
        modifiers,
        keycode,
        keysym,
        raw: spec.to_string(),
    })
}

/// Parse a modifier name to its X11 modifier mask bits.
fn parse_modifier(name: &str) -> Option<u16> {
    match name.to_ascii_lowercase().as_str() {
        "shift" => Some(ModMask::SHIFT.into()),
        "control" | "ctrl" => Some(ModMask::CONTROL.into()),
        "alt" | "mod1" => Some(u16::from(ModMask::M1)),
        "super" | "mod4" => Some(u16::from(ModMask::M4)),
        _ => None,
    }
}

/// Map a key name to an X11 keysym.
///
/// Supports single ASCII letters (A-Z), digits (0-9), function keys
/// (F1-F12), and common named keys.
fn key_name_to_keysym(name: &str) -> Option<Keysym> {
    // Single ASCII letter → lowercase keysym.
    if name.len() == 1 {
        let ch = name.chars().next()?;
        if ch.is_ascii_alphabetic() {
            return Some(ch.to_ascii_lowercase() as Keysym);
        }
        if ch.is_ascii_digit() {
            return Some(ch as Keysym);
        }
    }

    // Function keys.
    if let Some(rest) = name.strip_prefix('F').or_else(|| name.strip_prefix('f'))
        && let Ok(n) = rest.parse::<u32>()
        && (1..=12).contains(&n)
    {
        // XK_F1 = 0xffbe, XK_F2 = 0xffbf, ...
        return Some(0xffbe + n - 1);
    }

    // Named keys (case-insensitive).
    match name.to_ascii_lowercase().as_str() {
        "space" => Some(0x0020),
        "return" | "enter" => Some(0xff0d),
        "escape" | "esc" => Some(0xff1b),
        "tab" => Some(0xff09),
        "backspace" => Some(0xff08),
        "delete" => Some(0xffff),
        "insert" => Some(0xff63),
        "home" => Some(0xff50),
        "end" => Some(0xff57),
        "page_up" | "pageup" | "prior" => Some(0xff55),
        "page_down" | "pagedown" | "next" => Some(0xff56),
        "up" => Some(0xff52),
        "down" => Some(0xff54),
        "left" => Some(0xff51),
        "right" => Some(0xff53),
        _ => None,
    }
}

/// Resolve a keysym to a keycode using the server's keyboard mapping.
///
/// Returns the first matching keycode, or `None` if the keysym is
/// not present in any keycode's keysym list.
fn keysym_to_keycode(conn: &impl Connection, setup: &Setup, keysym: Keysym) -> Option<u8> {
    let min_keycode = setup.min_keycode;
    let max_keycode = setup.max_keycode;
    let count = max_keycode - min_keycode + 1;

    let reply = xproto::get_keyboard_mapping(conn, min_keycode, count)
        .ok()?
        .reply()
        .ok()?;

    let syms_per_code = reply.keysyms_per_keycode as usize;
    if syms_per_code == 0 {
        return None;
    }

    for i in 0..count as usize {
        let base = i * syms_per_code;
        for j in 0..syms_per_code {
            if reply.keysyms.get(base + j) == Some(&keysym) {
                return Some(min_keycode + i as u8);
            }
        }
    }

    None
}

/// Check if a key event matches a binding.
///
/// Masks out CapsLock (LockMask) and NumLock from the event state
/// before comparing, so hotkeys fire regardless of lock key state.
///
/// `numlock_mask` is the dynamically detected modifier bit for NumLock
/// (usually Mod2 / 0x0010, but may differ per X11 server configuration).
pub fn event_matches_binding(
    event_keycode: u8,
    event_state: u16,
    binding: &Binding,
    numlock_mask: u16,
) -> bool {
    // Mask out lock bits: CapsLock = LOCK (bit 1), NumLock = detected bit.
    let lock_mask: u16 = u16::from(ModMask::LOCK) | numlock_mask;
    let clean_state = event_state & !lock_mask;
    // Also mask out mouse button bits (bits 8-12) — only compare modifier bits.
    let modifier_mask: u16 = 0x00ff;
    let clean_mods = clean_state & modifier_mask;

    event_keycode == binding.keycode && clean_mods == binding.modifiers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modifier_shift() {
        assert_eq!(parse_modifier("Shift"), Some(u16::from(ModMask::SHIFT)));
        assert_eq!(parse_modifier("shift"), Some(u16::from(ModMask::SHIFT)));
    }

    #[test]
    fn parse_modifier_control() {
        let expected = u16::from(ModMask::CONTROL);
        assert_eq!(parse_modifier("Control"), Some(expected));
        assert_eq!(parse_modifier("Ctrl"), Some(expected));
        assert_eq!(parse_modifier("ctrl"), Some(expected));
    }

    #[test]
    fn parse_modifier_alt() {
        assert_eq!(parse_modifier("Alt"), Some(u16::from(ModMask::M1)));
        assert_eq!(parse_modifier("alt"), Some(u16::from(ModMask::M1)));
    }

    #[test]
    fn parse_modifier_super() {
        assert_eq!(parse_modifier("Super"), Some(u16::from(ModMask::M4)));
        assert_eq!(parse_modifier("super"), Some(u16::from(ModMask::M4)));
    }

    #[test]
    fn parse_modifier_unknown_returns_none() {
        assert_eq!(parse_modifier("Meta"), None);
        assert_eq!(parse_modifier(""), None);
        assert_eq!(parse_modifier("Hyper"), None);
    }

    #[test]
    fn key_name_letters() {
        assert_eq!(key_name_to_keysym("C"), Some(0x63)); // 'c'
        assert_eq!(key_name_to_keysym("V"), Some(0x76)); // 'v'
        assert_eq!(key_name_to_keysym("a"), Some(0x61)); // 'a'
        assert_eq!(key_name_to_keysym("Z"), Some(0x7a)); // 'z'
    }

    #[test]
    fn key_name_digits() {
        assert_eq!(key_name_to_keysym("0"), Some(0x30));
        assert_eq!(key_name_to_keysym("9"), Some(0x39));
    }

    #[test]
    fn key_name_function_keys() {
        assert_eq!(key_name_to_keysym("F1"), Some(0xffbe));
        assert_eq!(key_name_to_keysym("F12"), Some(0xffc9));
        assert_eq!(key_name_to_keysym("f5"), Some(0xffc2));
    }

    #[test]
    fn key_name_invalid_function_key() {
        assert_eq!(key_name_to_keysym("F0"), None);
        assert_eq!(key_name_to_keysym("F13"), None);
    }

    #[test]
    fn key_name_named_keys() {
        assert_eq!(key_name_to_keysym("space"), Some(0x0020));
        assert_eq!(key_name_to_keysym("Return"), Some(0xff0d));
        assert_eq!(key_name_to_keysym("Enter"), Some(0xff0d));
        assert_eq!(key_name_to_keysym("Escape"), Some(0xff1b));
        assert_eq!(key_name_to_keysym("Tab"), Some(0xff09));
        assert_eq!(key_name_to_keysym("BackSpace"), Some(0xff08));
    }

    #[test]
    fn key_name_unknown() {
        assert_eq!(key_name_to_keysym(""), None);
        assert_eq!(key_name_to_keysym("FooBar"), None);
    }

    #[test]
    fn event_matches_basic() {
        let binding = Binding {
            modifiers: u16::from(ModMask::M4) | u16::from(ModMask::SHIFT),
            keycode: 54,
            keysym: 0x63,
            raw: "Super+Shift+C".into(),
        };

        let numlock = u16::from(ModMask::M2); // Standard Mod2 for test

        // Exact match.
        assert!(event_matches_binding(
            54,
            binding.modifiers,
            &binding,
            numlock
        ));

        // With CapsLock set.
        let with_caps = binding.modifiers | u16::from(ModMask::LOCK);
        assert!(event_matches_binding(54, with_caps, &binding, numlock));

        // With NumLock (Mod2) set.
        let with_num = binding.modifiers | numlock;
        assert!(event_matches_binding(54, with_num, &binding, numlock));

        // With both locks set.
        let with_both = binding.modifiers | u16::from(ModMask::LOCK) | numlock;
        assert!(event_matches_binding(54, with_both, &binding, numlock));
    }

    #[test]
    fn event_no_match_wrong_keycode() {
        let binding = Binding {
            modifiers: u16::from(ModMask::M4) | u16::from(ModMask::SHIFT),
            keycode: 54,
            keysym: 0x63,
            raw: "Super+Shift+C".into(),
        };
        assert!(!event_matches_binding(
            55,
            binding.modifiers,
            &binding,
            u16::from(ModMask::M2)
        ));
    }

    #[test]
    fn event_no_match_wrong_modifiers() {
        let binding = Binding {
            modifiers: u16::from(ModMask::M4) | u16::from(ModMask::SHIFT),
            keycode: 54,
            keysym: 0x63,
            raw: "Super+Shift+C".into(),
        };
        // Only Super, missing Shift.
        assert!(!event_matches_binding(
            54,
            u16::from(ModMask::M4),
            &binding,
            u16::from(ModMask::M2)
        ));
    }

    #[test]
    fn event_matches_custom_numlock_mask() {
        let binding = Binding {
            modifiers: u16::from(ModMask::M4) | u16::from(ModMask::SHIFT),
            keycode: 54,
            keysym: 0x63,
            raw: "Super+Shift+C".into(),
        };

        // NumLock mapped to Mod3 (bit 5 = 0x0020) instead of standard Mod2.
        let custom_numlock: u16 = u16::from(ModMask::M3);
        let with_custom_num = binding.modifiers | custom_numlock;
        assert!(event_matches_binding(
            54,
            with_custom_num,
            &binding,
            custom_numlock
        ));

        // Standard Mod2 should NOT be masked when numlock is Mod3.
        let with_mod2 = binding.modifiers | u16::from(ModMask::M2);
        assert!(!event_matches_binding(
            54,
            with_mod2,
            &binding,
            custom_numlock
        ));
    }

    #[test]
    fn event_ignores_mouse_buttons() {
        let binding = Binding {
            modifiers: u16::from(ModMask::M4) | u16::from(ModMask::SHIFT),
            keycode: 54,
            keysym: 0x63,
            raw: "Super+Shift+C".into(),
        };
        // Mouse button 1 set in state (bit 8 = 0x100).
        let with_button = binding.modifiers | 0x100;
        assert!(event_matches_binding(
            54,
            with_button,
            &binding,
            u16::from(ModMask::M2)
        ));
    }
}

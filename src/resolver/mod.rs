//! Resolver abstraction — pluggable platform adapters.
//!
//! Extracts all platform-specific behavior (focus detection, key grabs,
//! clipboard access) into composable sub-interfaces. Platform adapters
//! implement one or more traits; the system composes them at startup.
//!
//! See CONTRACT_RESOLVER.md.

// Trait definitions and types — no consumers until X11 adapters land.
#![allow(dead_code, unused_imports)]

pub mod clipboard;
pub mod hotkey;
pub mod session;
pub mod x11;

pub use clipboard::ClipboardProvider;
pub use hotkey::{HotkeyEvent, HotkeyProvider, HotkeyRegistration, KeyBinding};
pub use session::SessionResolver;

/// Errors returned by resolver adapters.
#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    /// Session resolution failed (e.g. X11 property query error,
    /// ambiguous focus match).
    #[error("session: {0}")]
    Session(String),

    /// Hotkey registration or event delivery failed (e.g. grab conflict,
    /// display connection lost).
    #[error("hotkey: {0}")]
    Hotkey(String),

    /// Clipboard operation failed (e.g. xclip not found, pipe error).
    #[error("clipboard: {0}")]
    Clipboard(String),
}

/// A composed set of platform adapters.
///
/// Constructed at startup and passed to the hotkey client (and eventually
/// the broker's clipboard sink). Only one adapter per sub-interface is
/// active at runtime.
pub struct ResolverSet {
    /// Resolves which clippy session has focus.
    pub session: Box<dyn SessionResolver>,

    /// Registers global hotkeys and delivers events.
    pub hotkey: Box<dyn HotkeyProvider>,

    /// Reads and writes the system clipboard.
    pub clipboard: Box<dyn ClipboardProvider>,
}

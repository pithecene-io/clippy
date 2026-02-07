//! X11 platform adapters — reference implementation for the resolver
//! abstraction.
//!
//! Wraps existing X11 code from `hotkey/x11.rs`, `hotkey/keybinding.rs`,
//! and `hotkey/focus.rs` behind the resolver traits. All three adapters
//! share an `Arc<RustConnection>` created by `connect()`.
//!
//! See CONTRACT_RESOLVER.md §Reference Adapter: X11.

pub mod clipboard;
pub mod hotkey;
pub mod session;

use std::sync::Arc;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, Atom, Window};
use x11rb::rust_connection::RustConnection;

use super::ResolverError;

/// Shared X11 connection state used by all three adapters.
///
/// Created once via `X11Shared::connect()`, then cloned (via `Arc`)
/// into each adapter.
pub struct X11Shared {
    /// Shared X11 connection.
    pub conn: Arc<RustConnection>,
    /// Screen number (from `RustConnection::connect`).
    #[allow(dead_code)]
    pub screen_num: usize,
    /// Root window of the default screen.
    pub root: Window,
    /// `_NET_ACTIVE_WINDOW` atom (for focus queries).
    pub net_active_window: Atom,
    /// `_NET_WM_PID` atom (for focus queries).
    pub net_wm_pid: Atom,
    /// Dynamically detected NumLock modifier mask.
    pub numlock_mask: u16,
}

impl X11Shared {
    /// Connect to the X11 display and intern required atoms.
    ///
    /// This is the single connection point — both `X11SessionResolver`
    /// and `X11HotkeyProvider` receive `Arc` clones from the same
    /// instance.
    pub fn connect() -> Result<Self, ResolverError> {
        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|e| ResolverError::Session(format!("X11 connect failed: {e}")))?;

        let root = conn.setup().roots[screen_num].root;

        let net_active_window = xproto::intern_atom(&conn, false, b"_NET_ACTIVE_WINDOW")
            .map_err(|e| ResolverError::Session(format!("intern_atom: {e}")))?
            .reply()
            .map_err(|e| ResolverError::Session(format!("intern_atom reply: {e}")))?
            .atom;

        let net_wm_pid = xproto::intern_atom(&conn, false, b"_NET_WM_PID")
            .map_err(|e| ResolverError::Session(format!("intern_atom: {e}")))?
            .reply()
            .map_err(|e| ResolverError::Session(format!("intern_atom reply: {e}")))?
            .atom;

        let numlock_mask = crate::hotkey::x11::detect_numlock_mask(&conn);
        tracing::debug!(
            numlock_mask = format_args!("0x{numlock_mask:04x}"),
            "X11Shared: detected NumLock modifier"
        );

        Ok(Self {
            conn: Arc::new(conn),
            screen_num,
            root,
            net_active_window,
            net_wm_pid,
            numlock_mask,
        })
    }
}

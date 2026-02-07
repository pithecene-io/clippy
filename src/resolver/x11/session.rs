//! X11 session resolver — focus detection via `_NET_ACTIVE_WINDOW` and
//! `_NET_WM_PID`.
//!
//! Wraps the existing focus detection logic from `hotkey/x11.rs` and
//! `hotkey/focus.rs` behind the `SessionResolver` trait.
//!
//! See CONTRACT_RESOLVER.md §X11Resolver.

use std::sync::Arc;

use x11rb::protocol::xproto::{self, Atom, Window};
use x11rb::rust_connection::RustConnection;

use crate::hotkey::focus;
use crate::ipc::protocol::SessionDescriptor;
use crate::resolver::{ResolverError, SessionResolver};

/// X11 implementation of `SessionResolver`.
///
/// Queries `_NET_ACTIVE_WINDOW` → `_NET_WM_PID` → process tree walk
/// to determine which clippy session owns the focused window.
pub struct X11SessionResolver {
    conn: Arc<RustConnection>,
    root: Window,
    net_active_window: Atom,
    net_wm_pid: Atom,
}

impl X11SessionResolver {
    /// Create from shared X11 connection state.
    pub fn new(shared: &super::X11Shared) -> Self {
        Self {
            conn: Arc::clone(&shared.conn),
            root: shared.root,
            net_active_window: shared.net_active_window,
            net_wm_pid: shared.net_wm_pid,
        }
    }

    /// Query the active window's PID via X11 properties.
    ///
    /// Same algorithm as `X11Context::get_active_window_pid()`.
    fn get_active_window_pid(&self) -> Result<Option<u32>, ResolverError> {
        let reply = xproto::get_property(
            &*self.conn,
            false,
            self.root,
            self.net_active_window,
            xproto::AtomEnum::WINDOW,
            0,
            1,
        )
        .map_err(|e| ResolverError::Session(format!("get_property _NET_ACTIVE_WINDOW: {e}")))?
        .reply()
        .map_err(|e| ResolverError::Session(format!("get_property reply: {e}")))?;

        if reply.format != 32 || reply.value.len() < 4 {
            return Ok(None);
        }

        let window_id = u32::from_ne_bytes([
            reply.value[0],
            reply.value[1],
            reply.value[2],
            reply.value[3],
        ]);

        if window_id == 0 {
            return Ok(None);
        }

        let reply = xproto::get_property(
            &*self.conn,
            false,
            window_id,
            self.net_wm_pid,
            xproto::AtomEnum::CARDINAL,
            0,
            1,
        )
        .map_err(|e| ResolverError::Session(format!("get_property _NET_WM_PID: {e}")))?
        .reply()
        .map_err(|e| ResolverError::Session(format!("get_property reply: {e}")))?;

        if reply.format != 32 || reply.value.len() < 4 {
            return Ok(None);
        }

        let pid = u32::from_ne_bytes([
            reply.value[0],
            reply.value[1],
            reply.value[2],
            reply.value[3],
        ]);

        Ok(Some(pid))
    }
}

impl SessionResolver for X11SessionResolver {
    fn focused_session(
        &self,
        sessions: &[SessionDescriptor],
    ) -> Result<Option<String>, ResolverError> {
        let window_pid = match self.get_active_window_pid()? {
            Some(pid) => pid,
            None => return Ok(None),
        };

        match focus::resolve_session(window_pid, sessions) {
            Ok(session_id) => Ok(Some(session_id)),
            Err(focus::FocusError::NoSession) => Ok(None),
            Err(focus::FocusError::Ambiguous(ids)) => Err(ResolverError::Session(format!(
                "ambiguous — multiple sessions match: {}",
                ids.join(", ")
            ))),
        }
    }
}

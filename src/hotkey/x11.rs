//! X11 integration — connection, key grabs, focus queries, event thread.
//!
//! Wraps `x11rb::rust_connection::RustConnection` for hotkey registration,
//! active window detection, and a polling event thread that feeds key
//! events to the main async loop. See CONTRACT_HOTKEY.md §132–156.

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{self, Atom, GrabMode, Keysym, ModMask, Window};
use x11rb::rust_connection::RustConnection;

use super::HotkeyError;
use super::keybinding::Binding;

/// CapsLock modifier bit (always LockMask, bit 1).
const LOCK_MASK: u16 = 0x0002;

/// XK_Num_Lock keysym for dynamic modifier detection.
const XK_NUM_LOCK: Keysym = 0xff7f;

/// Pre-interned X11 atoms for property queries.
struct Atoms {
    net_active_window: Atom,
    net_wm_pid: Atom,
}

/// X11 connection context for the hotkey client.
pub struct X11Context {
    conn: Arc<RustConnection>,
    screen_num: usize,
    root: Window,
    atoms: Atoms,
    numlock_mask: u16,
}

impl X11Context {
    /// Connect to the X11 display and intern required atoms.
    pub fn connect() -> Result<Self, HotkeyError> {
        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|e| HotkeyError::X11(format!("connect failed: {e}")))?;

        let root = conn.setup().roots[screen_num].root;

        // Intern atoms for focus detection.
        let net_active_window = xproto::intern_atom(&conn, false, b"_NET_ACTIVE_WINDOW")
            .map_err(|e| HotkeyError::X11(format!("intern_atom: {e}")))?
            .reply()
            .map_err(|e| HotkeyError::X11(format!("intern_atom reply: {e}")))?
            .atom;

        let net_wm_pid = xproto::intern_atom(&conn, false, b"_NET_WM_PID")
            .map_err(|e| HotkeyError::X11(format!("intern_atom: {e}")))?
            .reply()
            .map_err(|e| HotkeyError::X11(format!("intern_atom reply: {e}")))?
            .atom;

        // Detect which modifier bit corresponds to NumLock.
        let numlock_mask = detect_numlock_mask(&conn);
        tracing::debug!(
            numlock_mask = format_args!("0x{numlock_mask:04x}"),
            "detected NumLock modifier"
        );

        Ok(Self {
            conn: Arc::new(conn),
            screen_num,
            root,
            atoms: Atoms {
                net_active_window,
                net_wm_pid,
            },
            numlock_mask,
        })
    }

    /// Register a global key grab on the root window.
    ///
    /// Registers 4 grabs per binding (with/without NumLock/CapsLock).
    /// Returns `Ok(true)` on success, `Ok(false)` if the grab failed
    /// (another application holds it), `Err` on connection error.
    ///
    /// CONTRACT_HOTKEY.md §143-150: log conflict, continue with
    /// whatever bindings succeeded.
    pub fn grab_key(&self, binding: &Binding) -> Result<bool, HotkeyError> {
        let mut all_ok = true;
        let lock_masks = self.lock_masks();

        for &lock_mask in &lock_masks {
            let mods = ModMask::from(binding.modifiers | lock_mask);

            let cookie = xproto::grab_key(
                &*self.conn,
                true, // owner_events
                self.root,
                mods,
                binding.keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(|e| HotkeyError::X11(format!("grab_key send: {e}")))?;

            // Check for error reply (grab conflict).
            if let Err(e) = cookie.check() {
                tracing::warn!(
                    binding = %binding.raw,
                    lock_mask,
                    error = %e,
                    "XGrabKey failed — binding may conflict with another application"
                );
                all_ok = false;
            }
        }

        Ok(all_ok)
    }

    /// Unregister a global key grab from the root window.
    ///
    /// Ungrabs all 4 lock-mask variants. Best-effort — errors are logged.
    pub fn ungrab_key(&self, binding: &Binding) {
        let lock_masks = self.lock_masks();
        for &lock_mask in &lock_masks {
            let mods = ModMask::from(binding.modifiers | lock_mask);

            if let Err(e) = xproto::ungrab_key(&*self.conn, binding.keycode, self.root, mods) {
                tracing::debug!(
                    binding = %binding.raw,
                    error = %e,
                    "XUngrabKey failed"
                );
            }
        }

        // Flush ungrab requests.
        if let Err(e) = self.conn.flush() {
            tracing::debug!(error = %e, "flush after ungrab failed");
        }
    }

    /// Query the active (focused) window's PID.
    ///
    /// 1. Read `_NET_ACTIVE_WINDOW` on root → window XID.
    /// 2. Read `_NET_WM_PID` on that window → PID.
    ///
    /// Returns `None` if either property is missing (e.g., focused
    /// window doesn't set `_NET_WM_PID`).
    pub fn get_active_window_pid(&self) -> Result<Option<u32>, HotkeyError> {
        // Step 1: Get the active window XID.
        let reply = xproto::get_property(
            &*self.conn,
            false,
            self.root,
            self.atoms.net_active_window,
            xproto::AtomEnum::WINDOW,
            0,
            1, // We need one 32-bit value.
        )
        .map_err(|e| HotkeyError::X11(format!("get_property _NET_ACTIVE_WINDOW: {e}")))?
        .reply()
        .map_err(|e| HotkeyError::X11(format!("get_property reply: {e}")))?;

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

        // Step 2: Get the PID of the active window.
        let reply = xproto::get_property(
            &*self.conn,
            false,
            window_id,
            self.atoms.net_wm_pid,
            xproto::AtomEnum::CARDINAL,
            0,
            1,
        )
        .map_err(|e| HotkeyError::X11(format!("get_property _NET_WM_PID: {e}")))?
        .reply()
        .map_err(|e| HotkeyError::X11(format!("get_property reply: {e}")))?;

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

    /// Get a shared reference to the X11 connection.
    pub fn conn(&self) -> &Arc<RustConnection> {
        &self.conn
    }

    /// Get the X11 Setup (for keybinding resolution).
    pub fn setup(&self) -> &x11rb::protocol::xproto::Setup {
        self.conn.setup()
    }

    /// Get the screen number.
    pub fn screen_num(&self) -> usize {
        self.screen_num
    }

    /// Get the dynamically detected NumLock modifier mask.
    pub fn numlock_mask(&self) -> u16 {
        self.numlock_mask
    }

    /// Compute lock mask combinations for grab registration.
    ///
    /// Returns 4 masks: [0, CapsLock, NumLock, CapsLock|NumLock].
    fn lock_masks(&self) -> [u16; 4] {
        [
            0,
            LOCK_MASK,
            self.numlock_mask,
            LOCK_MASK | self.numlock_mask,
        ]
    }
}

/// Detect which modifier bit corresponds to NumLock by querying the
/// X11 modifier mapping and keyboard mapping.
///
/// Falls back to Mod2 (0x0010) if detection fails — this is the most
/// common mapping and matches xmodmap defaults.
fn detect_numlock_mask(conn: &RustConnection) -> u16 {
    const FALLBACK: u16 = 0x0010; // Mod2Mask

    let mod_reply = match xproto::get_modifier_mapping(conn) {
        Ok(cookie) => match cookie.reply() {
            Ok(r) => r,
            Err(_) => return FALLBACK,
        },
        Err(_) => return FALLBACK,
    };

    let keycodes_per_mod = mod_reply.keycodes_per_modifier() as usize;
    if keycodes_per_mod == 0 {
        return FALLBACK;
    }

    // Resolve XK_Num_Lock → set of keycodes via keyboard mapping.
    let setup = conn.setup();
    let min_kc = setup.min_keycode;
    let max_kc = setup.max_keycode;
    let count = max_kc - min_kc + 1;

    let kb_reply = match xproto::get_keyboard_mapping(conn, min_kc, count) {
        Ok(cookie) => match cookie.reply() {
            Ok(r) => r,
            Err(_) => return FALLBACK,
        },
        Err(_) => return FALLBACK,
    };

    let syms_per_code = kb_reply.keysyms_per_keycode as usize;
    if syms_per_code == 0 {
        return FALLBACK;
    }

    // Collect keycodes that produce XK_Num_Lock.
    let mut numlock_keycodes: Vec<u8> = Vec::new();
    for i in 0..count as usize {
        let base = i * syms_per_code;
        for j in 0..syms_per_code {
            if kb_reply.keysyms.get(base + j) == Some(&XK_NUM_LOCK) {
                numlock_keycodes.push(min_kc + i as u8);
                break;
            }
        }
    }

    // Scan modifier map: 8 rows × keycodes_per_modifier.
    // Row 0 = Shift, 1 = Lock, 2 = Control, 3 = Mod1, ..., 7 = Mod5.
    // Modifier mask bit for row i = 1 << i.
    for modifier_idx in 0..8usize {
        let row_start = modifier_idx * keycodes_per_mod;
        for k in 0..keycodes_per_mod {
            if let Some(&keycode) = mod_reply.keycodes.get(row_start + k)
                && keycode != 0
                && numlock_keycodes.contains(&keycode)
            {
                return 1u16 << modifier_idx;
            }
        }
    }

    // NumLock not found in modifier map.
    FALLBACK
}

/// Spawn a dedicated thread that polls the X11 connection for events.
///
/// Uses `nix::poll()` on the X11 connection fd with a 100ms timeout.
/// When readable, drains all available events via `poll_for_event()`.
/// Checks the `stop` flag each iteration for clean shutdown.
///
/// Returns the receiver channel and the thread join handle.
pub fn spawn_event_thread(
    conn: Arc<RustConnection>,
    stop: Arc<AtomicBool>,
) -> (tokio::sync::mpsc::UnboundedReceiver<Event>, JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    let handle = std::thread::Builder::new()
        .name("x11-events".into())
        .spawn(move || {
            let raw_fd = conn.stream().as_raw_fd();

            while !stop.load(Ordering::Relaxed) {
                // SAFETY: raw_fd is the X11 connection fd, valid while conn is alive.
                let borrowed = unsafe { BorrowedFd::borrow_raw(raw_fd) };
                let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];

                match poll(&mut fds, PollTimeout::from(100u16)) {
                    Ok(0) => continue, // Timeout — check stop flag.
                    Ok(_) => {
                        // Drain all available events.
                        loop {
                            match conn.poll_for_event() {
                                Ok(Some(event)) => {
                                    if tx.send(event).is_err() {
                                        // Receiver dropped — shut down.
                                        return;
                                    }
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    tracing::error!(error = %e, "X11 connection error");
                                    return;
                                }
                            }
                        }
                    }
                    Err(nix::Error::EINTR) => continue,
                    Err(e) => {
                        tracing::error!(error = %e, "poll error on X11 fd");
                        return;
                    }
                }
            }
        })
        .expect("failed to spawn x11 event thread");

    (rx, handle)
}

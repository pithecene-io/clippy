//! X11 hotkey provider — global key grabs via `XGrabKey` and event thread.
//!
//! Wraps existing key grab and event thread logic from `hotkey/x11.rs`
//! and `hotkey/keybinding.rs` behind the `HotkeyProvider` trait.
//!
//! See CONTRACT_RESOLVER.md §X11Hotkey.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{self, GrabMode, ModMask, Window};
use x11rb::rust_connection::RustConnection;

use crate::hotkey::keybinding::{self, Binding};
use crate::hotkey::x11 as x11_impl;
use crate::resolver::ResolverError;
use crate::resolver::hotkey::{HotkeyEvent, HotkeyRegistration, KeyBinding};

/// X11 implementation of `HotkeyProvider`.
///
/// Grabs keys on the root window with NumLock/CapsLock masking, spawns
/// an X11 event thread, and bridges raw `KeyPress` events to
/// `HotkeyEvent` values on the returned channel.
pub struct X11HotkeyProvider {
    conn: Arc<RustConnection>,
    screen_num: usize,
    root: Window,
    numlock_mask: u16,

    /// Parsed bindings held for ungrab on shutdown.
    bindings: Vec<Binding>,
    /// Stop flag shared with the X11 event thread.
    stop: Option<Arc<AtomicBool>>,
    /// X11 event thread join handle.
    event_thread: Option<JoinHandle<()>>,
    /// Classification bridge join handle.
    bridge_thread: Option<JoinHandle<()>>,
}

impl X11HotkeyProvider {
    /// Create from shared X11 connection state.
    pub fn new(shared: &super::X11Shared) -> Self {
        Self {
            conn: Arc::clone(&shared.conn),
            screen_num: shared.screen_num,
            root: shared.root,
            numlock_mask: shared.numlock_mask,
            bindings: Vec::new(),
            stop: None,
            event_thread: None,
            bridge_thread: None,
        }
    }

    /// Grab a single binding on the root window with lock-mask variants.
    ///
    /// Same logic as `X11Context::grab_key()`.
    fn grab_key(&self, binding: &Binding) -> Result<bool, ResolverError> {
        let mut all_ok = true;
        let lock_masks = self.lock_masks();

        for &lock_mask in &lock_masks {
            let mods = ModMask::from(binding.modifiers | lock_mask);

            let cookie = xproto::grab_key(
                &*self.conn,
                true,
                self.root,
                mods,
                binding.keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(|e| ResolverError::Hotkey(format!("grab_key send: {e}")))?;

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

    /// Ungrab a single binding from the root window.
    ///
    /// Same logic as `X11Context::ungrab_key()`.
    fn ungrab_key(&self, binding: &Binding) {
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
        if let Err(e) = self.conn.flush() {
            tracing::debug!(error = %e, "flush after ungrab failed");
        }
    }

    /// Lock-mask combinations for grab registration.
    fn lock_masks(&self) -> [u16; 4] {
        [
            0,
            x11_impl::LOCK_MASK,
            self.numlock_mask,
            x11_impl::LOCK_MASK | self.numlock_mask,
        ]
    }
}

impl crate::resolver::HotkeyProvider for X11HotkeyProvider {
    fn register(
        &mut self,
        capture: &KeyBinding,
        paste: &KeyBinding,
        clipboard: Option<&KeyBinding>,
    ) -> Result<HotkeyRegistration, ResolverError> {
        // 1. Parse bindings.
        let capture_binding =
            keybinding::parse_binding(&capture.spec, &*self.conn, self.conn.setup())
                .map_err(|e| ResolverError::Hotkey(format!("parse capture binding: {e}")))?;

        let paste_binding = keybinding::parse_binding(&paste.spec, &*self.conn, self.conn.setup())
            .map_err(|e| ResolverError::Hotkey(format!("parse paste binding: {e}")))?;

        tracing::info!(
            capture = %capture_binding.raw,
            capture_keycode = capture_binding.keycode,
            paste = %paste_binding.raw,
            paste_keycode = paste_binding.keycode,
            "bindings parsed"
        );

        // 2. Grab keys.
        let mut bindings_ok = 0u32;

        match self.grab_key(&capture_binding) {
            Ok(true) => {
                bindings_ok += 1;
                tracing::info!(binding = %capture_binding.raw, "capture hotkey grabbed");
            }
            Ok(false) => {
                eprintln!(
                    "warning: capture hotkey {} could not be grabbed (conflict)",
                    capture_binding.raw
                );
            }
            Err(e) => {
                tracing::error!(binding = %capture_binding.raw, error = %e, "grab failed");
            }
        }

        match self.grab_key(&paste_binding) {
            Ok(true) => {
                bindings_ok += 1;
                tracing::info!(binding = %paste_binding.raw, "paste hotkey grabbed");
            }
            Ok(false) => {
                eprintln!(
                    "warning: paste hotkey {} could not be grabbed (conflict)",
                    paste_binding.raw
                );
            }
            Err(e) => {
                tracing::error!(binding = %paste_binding.raw, error = %e, "grab failed");
            }
        }

        let clipboard_binding = match clipboard {
            Some(key) => {
                let binding = keybinding::parse_binding(&key.spec, &*self.conn, self.conn.setup())
                    .map_err(|e| ResolverError::Hotkey(format!("parse clipboard binding: {e}")))?;

                match self.grab_key(&binding) {
                    Ok(true) => {
                        bindings_ok += 1;
                        tracing::info!(binding = %binding.raw, "clipboard hotkey grabbed");
                    }
                    Ok(false) => {
                        eprintln!(
                            "warning: clipboard hotkey {} could not be grabbed (conflict)",
                            binding.raw
                        );
                    }
                    Err(e) => {
                        tracing::error!(binding = %binding.raw, error = %e, "grab failed");
                    }
                }
                Some(binding)
            }
            None => None,
        };

        // Store bindings for ungrab on shutdown.
        self.bindings.push(capture_binding.clone());
        self.bindings.push(paste_binding.clone());
        if let Some(ref b) = clipboard_binding {
            self.bindings.push(b.clone());
        }

        // 3. Spawn X11 event thread.
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(Arc::clone(&stop));

        let (mut raw_rx, x11_thread) =
            x11_impl::spawn_event_thread(Arc::clone(&self.conn), Arc::clone(&stop));
        self.event_thread = Some(x11_thread);

        // 4. Spawn classification bridge thread.
        //
        // Reads raw X11 Events, classifies KeyPress via
        // event_matches_binding(), and sends HotkeyEvent on the
        // returned channel.
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let numlock_mask = self.numlock_mask;

        // Clone bindings for the bridge thread.
        let cap = capture_binding;
        let pst = paste_binding;
        let clip = clipboard_binding;

        let bridge = std::thread::Builder::new()
            .name("x11-hotkey-bridge".into())
            .spawn(move || {
                while let Some(event) = raw_rx.blocking_recv() {
                    if let Some(hotkey_event) =
                        classify_event(&event, &cap, &pst, clip.as_ref(), numlock_mask)
                        && event_tx.send(hotkey_event).is_err()
                    {
                        // Receiver dropped — shut down.
                        return;
                    }
                }
            })
            .expect("failed to spawn x11-hotkey-bridge thread");
        self.bridge_thread = Some(bridge);

        Ok(HotkeyRegistration {
            events: event_rx,
            bindings_ok,
        })
    }

    fn unregister(&mut self) {
        // 1. Signal stop to the X11 event thread.
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }

        // 2. Ungrab all bindings.
        for binding in &self.bindings {
            self.ungrab_key(binding);
        }
        self.bindings.clear();

        // 3. Join event thread (exits within 100ms due to poll timeout).
        if let Some(handle) = self.event_thread.take()
            && let Err(e) = handle.join()
        {
            tracing::warn!("X11 event thread panicked: {e:?}");
        }

        // 4. Join bridge thread (exits when raw_rx closes).
        if let Some(handle) = self.bridge_thread.take()
            && let Err(e) = handle.join()
        {
            tracing::warn!("X11 hotkey bridge thread panicked: {e:?}");
        }
    }
}

/// Classify a raw X11 event as a `HotkeyEvent`, or `None` if not a
/// registered hotkey.
///
/// Same logic as `hotkey::classify_event()` but returns `HotkeyEvent`
/// instead of `Action`.
fn classify_event(
    event: &Event,
    capture_binding: &Binding,
    paste_binding: &Binding,
    clipboard_binding: Option<&Binding>,
    numlock_mask: u16,
) -> Option<HotkeyEvent> {
    let key_event = match event {
        Event::KeyPress(e) => e,
        _ => return None,
    };

    let keycode = key_event.detail;
    let state = u16::from(key_event.state);

    if keybinding::event_matches_binding(keycode, state, capture_binding, numlock_mask) {
        Some(HotkeyEvent::Capture)
    } else if keybinding::event_matches_binding(keycode, state, paste_binding, numlock_mask) {
        Some(HotkeyEvent::Paste)
    } else if let Some(binding) = clipboard_binding {
        if keybinding::event_matches_binding(keycode, state, binding, numlock_mask) {
            Some(HotkeyEvent::Clipboard)
        } else {
            None
        }
    } else {
        None
    }
}

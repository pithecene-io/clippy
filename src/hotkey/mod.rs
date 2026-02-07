//! Hotkey client — global key bindings and focus-based dispatch.
//!
//! Registers global X11 hotkeys, detects which clippy session has focus,
//! and sends capture/paste requests to the broker daemon.
//! See CONTRACT_HOTKEY.md.

mod broker_client;
mod focus;
mod keybinding;
mod x11;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal as tokio_signal};
use x11rb::protocol::Event;

use broker_client::BrokerClient;
use keybinding::{Binding, event_matches_binding, parse_binding};
use x11::X11Context;

/// Hotkey client errors.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    #[error("broker: {0}")]
    Broker(String),
    #[error("X11: {0}")]
    X11(String),
    #[error("invalid key binding: {0}")]
    InvalidBinding(String),
    #[error("no bindings succeeded")]
    NoBindings,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Actions triggered by hotkeys.
#[derive(Debug, Clone, Copy)]
enum Action {
    Capture,
    Paste,
}

/// Run the hotkey client.
///
/// This is the main entry point called from `main.rs` for the `hotkey`
/// command. Runs until a signal is received or the broker disconnects.
///
/// # Contract compliance
///
/// - Broker connection required at startup (§191-192)
/// - X11 global key grabs via XGrabKey (§136)
/// - NumLock/CapsLock masking (§138-139)
/// - Focus detection: _NET_ACTIVE_WINDOW → _NET_WM_PID → /proc (§99-114)
/// - Action serialization (§199)
/// - Ungrab on shutdown (§154-155)
/// - Broker disconnect → ungrab + exit non-zero (§213-218)
pub async fn run(capture_key: String, paste_key: String) -> Result<(), HotkeyError> {
    // 1. Connect to broker — fail hard if unreachable.
    let mut broker = BrokerClient::connect().await?;
    tracing::info!("connected to broker");

    // 2. Connect to X11 display.
    let x11 = X11Context::connect()?;
    tracing::info!(screen = x11.screen_num(), "connected to X11 display");

    // 3. Parse key bindings.
    let capture_binding = parse_binding(&capture_key, &**x11.conn(), x11.setup())?;
    let paste_binding = parse_binding(&paste_key, &**x11.conn(), x11.setup())?;

    tracing::info!(
        capture = %capture_binding.raw,
        capture_keycode = capture_binding.keycode,
        paste = %paste_binding.raw,
        paste_keycode = paste_binding.keycode,
        "bindings parsed"
    );

    // 4. Grab keys with NumLock/CapsLock masking.
    let mut bindings_ok = 0u32;

    match x11.grab_key(&capture_binding) {
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

    match x11.grab_key(&paste_binding) {
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

    // CONTRACT_HOTKEY.md §149-150: if no bindings succeed, exit.
    if bindings_ok == 0 {
        return Err(HotkeyError::NoBindings);
    }

    // 5. Start X11 event thread.
    let stop = Arc::new(AtomicBool::new(false));
    let (mut event_rx, x11_thread) =
        x11::spawn_event_thread(Arc::clone(x11.conn()), Arc::clone(&stop));

    // 6. Install signal handlers.
    let mut sig_term = tokio_signal(SignalKind::terminate())?;
    let mut sig_int = tokio_signal(SignalKind::interrupt())?;

    tracing::info!("hotkey client running — press Ctrl+C to stop");

    // 7. Main select! loop.
    let mut broker_disconnected = false;
    let mut x11_thread_died = false;

    // Periodic broker health check — detect disconnect while idle
    // (CONTRACT_HOTKEY.md §213-218).
    let mut health_check = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    );

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                let Some(event) = event else {
                    // Channel closed — X11 event thread exited.
                    tracing::error!("X11 event thread died — shutting down");
                    eprintln!("error: X11 event thread exited unexpectedly");
                    x11_thread_died = true;
                    break;
                };

                if let Some(action) = classify_event(&event, &capture_binding, &paste_binding, x11.numlock_mask())
                    && let Err(e) = dispatch_action(action, &x11, &mut broker).await
                {
                    // Check if this is a broker disconnect.
                    if is_broker_error(&e) {
                        tracing::error!(error = %e, "broker disconnected — shutting down");
                        eprintln!("error: broker disconnected");
                        broker_disconnected = true;
                        break;
                    }
                    // Non-fatal action error — log and continue.
                    tracing::warn!(error = %e, "action failed");
                    eprintln!("error: {e}");
                }
            }

            _ = health_check.tick() => {
                // Periodic broker liveness probe — detect disconnect
                // while idle (CONTRACT_HOTKEY.md §213-218).
                if let Err(e) = broker.list_sessions().await {
                    if is_broker_error(&e) {
                        tracing::error!(error = %e, "broker health check failed — shutting down");
                        eprintln!("error: broker disconnected");
                        broker_disconnected = true;
                        break;
                    }
                    tracing::warn!(error = %e, "broker health check returned error");
                }
            }

            _ = sig_term.recv() => {
                tracing::info!("received SIGTERM, shutting down");
                break;
            }

            _ = sig_int.recv() => {
                tracing::info!("received SIGINT, shutting down");
                break;
            }
        }
    }

    // 8. Cleanup.
    // Stop X11 event thread.
    stop.store(true, Ordering::Relaxed);

    // Ungrab all registered hotkeys (CONTRACT_HOTKEY.md §154-155).
    x11.ungrab_key(&capture_binding);
    x11.ungrab_key(&paste_binding);

    // Wait for X11 thread to exit (will exit within 100ms due to poll timeout).
    if let Err(e) = x11_thread.join() {
        tracing::warn!("X11 event thread panicked: {e:?}");
    }

    tracing::info!("hotkey client stopped");

    if broker_disconnected || x11_thread_died {
        // CONTRACT_HOTKEY.md §215: exit with non-zero on broker disconnect.
        // Also exit non-zero if X11 thread died — client is non-functional.
        std::process::exit(1);
    }

    Ok(())
}

/// Classify an X11 event as a hotkey action, or `None` if not a hotkey.
fn classify_event(
    event: &Event,
    capture_binding: &Binding,
    paste_binding: &Binding,
    numlock_mask: u16,
) -> Option<Action> {
    // We only care about KeyPress events.
    let key_event = match event {
        Event::KeyPress(e) => e,
        _ => return None,
    };

    let keycode = key_event.detail;
    let state = u16::from(key_event.state);

    if event_matches_binding(keycode, state, capture_binding, numlock_mask) {
        Some(Action::Capture)
    } else if event_matches_binding(keycode, state, paste_binding, numlock_mask) {
        Some(Action::Paste)
    } else {
        None
    }
}

/// Dispatch an action: resolve focused session, send request to broker.
async fn dispatch_action(
    action: Action,
    x11: &X11Context,
    broker: &mut BrokerClient,
) -> Result<(), HotkeyError> {
    // 1. Query active window PID from X11.
    let window_pid = x11
        .get_active_window_pid()?
        .ok_or_else(|| HotkeyError::X11("no PID on focused window".into()))?;

    // 2. List sessions from broker.
    let sessions = broker.list_sessions().await?;

    if sessions.is_empty() {
        eprintln!("no active clippy sessions");
        return Ok(());
    }

    // 3. Resolve focused session via process tree walk.
    let session_id = focus::resolve_session(window_pid, &sessions)
        .map_err(|e| HotkeyError::X11(format!("focus resolution: {e}")))?;

    // 4. Send action to broker.
    match action {
        Action::Capture => {
            let size = broker.capture(&session_id).await?;
            tracing::info!(session = %session_id, size, "captured");
            eprintln!("captured {size} bytes from session {session_id}");
        }
        Action::Paste => {
            broker.paste(&session_id).await?;
            tracing::info!(session = %session_id, "pasted");
            eprintln!("pasted to session {session_id}");
        }
    }

    Ok(())
}

/// Check if an error indicates a broker connection failure.
fn is_broker_error(e: &HotkeyError) -> bool {
    matches!(e, HotkeyError::Broker(_))
}

//! Hotkey client — global key bindings and focus-based dispatch.
//!
//! Platform-agnostic hotkey event loop. Delegates key registration to
//! a `HotkeyProvider` and focus detection to a `SessionResolver`,
//! both supplied via resolver trait objects.
//! See CONTRACT_HOTKEY.md, CONTRACT_RESOLVER.md.

mod broker_client;
pub(crate) mod focus;
pub(crate) mod keybinding;
pub(crate) mod x11;

use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal as tokio_signal};

use broker_client::BrokerClient;

use crate::resolver::{HotkeyEvent, HotkeyProvider, KeyBinding, ResolverError, SessionResolver};

/// Hotkey client errors.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    #[error("broker: {0}")]
    Broker(String),
    // Dead in v2 (X11Context replaced by resolver adapters) — cleaned up in PR 4.
    #[allow(dead_code)]
    #[error("X11: {0}")]
    X11(String),
    // Dead in v2 (binding parsing moved into HotkeyProvider) — cleaned up in PR 4.
    #[allow(dead_code)]
    #[error("invalid key binding: {0}")]
    InvalidBinding(String),
    #[error("resolver: {0}")]
    Resolver(#[from] ResolverError),
    #[error("no bindings succeeded")]
    NoBindings,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Run the hotkey client.
///
/// This is the main entry point called from `main.rs` for the `hotkey`
/// command. Runs until a signal is received or the broker disconnects.
///
/// The hotkey client is platform-agnostic — all platform-specific
/// behavior is delegated to the resolver trait objects.
///
/// # Contract compliance
///
/// - Broker connection required at startup (§191-192)
/// - Key registration via HotkeyProvider (CONTRACT_RESOLVER.md)
/// - Focus detection via SessionResolver (CONTRACT_RESOLVER.md)
/// - Action serialization (§199)
/// - Ungrab on shutdown (§154-155)
/// - Broker disconnect → ungrab + exit non-zero (§213-218)
pub async fn run(
    capture_key: String,
    paste_key: String,
    clipboard_key: Option<String>,
    session_resolver: &dyn SessionResolver,
    hotkey_provider: &mut dyn HotkeyProvider,
) -> Result<(), HotkeyError> {
    // 1. Connect to broker — fail hard if unreachable.
    let mut broker = BrokerClient::connect().await?;
    tracing::info!("connected to broker");

    // 2. Register hotkeys via provider.
    let capture_binding = KeyBinding { spec: capture_key };
    let paste_binding = KeyBinding { spec: paste_key };
    let clipboard_binding = clipboard_key.map(|key| KeyBinding { spec: key });

    let registration =
        hotkey_provider.register(&capture_binding, &paste_binding, clipboard_binding.as_ref())?;

    // CONTRACT_HOTKEY.md §149-150: if no bindings succeed, exit.
    if registration.bindings_ok == 0 {
        hotkey_provider.unregister();
        return Err(HotkeyError::NoBindings);
    }

    // 3. Install signal handlers.
    let mut sig_term = tokio_signal(SignalKind::terminate())?;
    let mut sig_int = tokio_signal(SignalKind::interrupt())?;

    tracing::info!("hotkey client running — press Ctrl+C to stop");

    // 4. Main select! loop.
    let mut broker_disconnected = false;
    let mut event_thread_died = false;
    let mut event_rx = registration.events;

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
                    // Channel closed — event thread exited.
                    tracing::error!("hotkey event thread died — shutting down");
                    eprintln!("error: hotkey event thread exited unexpectedly");
                    event_thread_died = true;
                    break;
                };

                if let Err(e) = dispatch_action(event, session_resolver, &mut broker).await {
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

    // 5. Cleanup — release all key grabs (CONTRACT_HOTKEY.md §154-155).
    hotkey_provider.unregister();

    tracing::info!("hotkey client stopped");

    if broker_disconnected || event_thread_died {
        // CONTRACT_HOTKEY.md §215: exit with non-zero on broker disconnect.
        // Also exit non-zero if event thread died — client is non-functional.
        std::process::exit(1);
    }

    Ok(())
}

/// Dispatch a hotkey event: resolve focused session, send request to broker.
async fn dispatch_action(
    event: HotkeyEvent,
    session_resolver: &dyn SessionResolver,
    broker: &mut BrokerClient,
) -> Result<(), HotkeyError> {
    // 1. List sessions from broker.
    let sessions = broker.list_sessions().await?;

    if sessions.is_empty() {
        eprintln!("no active clippy sessions");
        return Ok(());
    }

    // 2. Resolve focused session via resolver.
    let session_id = session_resolver
        .focused_session(&sessions)?
        .ok_or_else(|| ResolverError::Session("no clippy session in focused window".into()))?;

    // 3. Send action to broker.
    match event {
        HotkeyEvent::Capture => {
            let size = broker.capture(&session_id).await?;
            tracing::info!(session = %session_id, size, "captured");
            eprintln!("captured {size} bytes from session {session_id}");
        }
        HotkeyEvent::Paste => {
            broker.paste(&session_id).await?;
            tracing::info!(session = %session_id, "pasted");
            eprintln!("pasted to session {session_id}");
        }
        HotkeyEvent::Clipboard => {
            let size = broker.capture(&session_id).await?;
            broker.deliver_clipboard().await?;
            tracing::info!(session = %session_id, size, "captured to clipboard");
            eprintln!("captured {size} bytes to clipboard from session {session_id}");
        }
    }

    Ok(())
}

/// Check if an error indicates a broker connection failure.
fn is_broker_error(e: &HotkeyError) -> bool {
    matches!(e, HotkeyError::Broker(_))
}

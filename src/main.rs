mod broker;
mod cli;
mod client;
mod hotkey;
mod ipc;
mod pty;
mod resolver;
mod turn;

use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Wrap { pattern, command } => match pty::run_session(pattern, command).await {
            Ok(code) => std::process::exit(code),
            Err(e) => {
                tracing::error!(error = %e, "wrap failed");
                eprintln!("clippyctl wrap: {e}");
                std::process::exit(1);
            }
        },
        Command::Broker {
            ring_depth,
            max_turn_size,
        } => {
            let depth = usize::try_from(ring_depth).unwrap_or_else(|_| {
                eprintln!("clippyctl broker: --ring-depth value too large for this platform");
                std::process::exit(1);
            });
            let config = broker::state::RingConfig {
                depth,
                max_turn_bytes: max_turn_size,
            };
            if let Err(e) = broker::run(config).await {
                tracing::error!(error = %e, "broker failed");
                eprintln!("clippyctl broker: {e}");
                std::process::exit(1);
            }
        }
        Command::Hotkey {
            capture_key,
            paste_key,
            clipboard_key,
        } => {
            // Construct X11 resolver adapters.
            let shared = match resolver::x11::X11Shared::connect() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "X11 connect failed");
                    eprintln!("clippyctl hotkey: {e}");
                    std::process::exit(1);
                }
            };
            let session_resolver = resolver::x11::session::X11SessionResolver::new(&shared);
            let mut hotkey_provider = resolver::x11::hotkey::X11HotkeyProvider::new(&shared);

            if let Err(e) = hotkey::run(
                capture_key,
                paste_key,
                clipboard_key,
                &session_resolver,
                &mut hotkey_provider,
            )
            .await
            {
                tracing::error!(error = %e, "hotkey failed");
                eprintln!("clippyctl hotkey: {e}");
                std::process::exit(1);
            }
        }
        Command::Client { action } => {
            if let Err(e) = client::run(action).await {
                tracing::error!(error = %e, "client failed");
                eprintln!("clippyctl client: {e}");
                std::process::exit(1);
            }
        }
    }
}

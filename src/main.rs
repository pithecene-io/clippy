mod broker;
mod cli;
mod hotkey;
mod ipc;
mod pty;
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
                eprintln!("clippyd wrap: {e}");
                std::process::exit(1);
            }
        },
        Command::Broker {
            ring_depth,
            max_turn_size,
        } => {
            let config = broker::state::RingConfig {
                depth: ring_depth as usize,
                max_turn_bytes: max_turn_size,
            };
            if let Err(e) = broker::run(config).await {
                tracing::error!(error = %e, "broker failed");
                eprintln!("clippyd broker: {e}");
                std::process::exit(1);
            }
        }
        Command::Hotkey {
            capture_key,
            paste_key,
        } => {
            if let Err(e) = hotkey::run(capture_key, paste_key).await {
                tracing::error!(error = %e, "hotkey failed");
                eprintln!("clippyd hotkey: {e}");
                std::process::exit(1);
            }
        }
    }
}

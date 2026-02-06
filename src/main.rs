mod broker;
mod cli;
mod hotkey;
#[allow(unused)]
mod ipc;
mod pty;
#[allow(unused)]
mod turn;

use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Wrap { pattern, command } => {
            tracing::info!(?pattern, ?command, "wrap: not yet implemented");
            eprintln!("clippyd wrap: not yet implemented");
            std::process::exit(1);
        }
        Command::Broker => {
            tracing::info!("broker: not yet implemented");
            eprintln!("clippyd broker: not yet implemented");
            std::process::exit(1);
        }
        Command::Hotkey {
            capture_key,
            paste_key,
        } => {
            tracing::info!(?capture_key, ?paste_key, "hotkey: not yet implemented");
            eprintln!("clippyd hotkey: not yet implemented");
            std::process::exit(1);
        }
    }
}

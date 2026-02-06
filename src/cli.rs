use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "clippyd", about = "Keyboard-driven agent turn relay")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the PTY wrapper around an agent process
    Wrap {
        /// Prompt pattern preset or custom regex
        #[arg(long, default_value = "generic")]
        pattern: String,

        /// Command to run
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },

    /// Run the broker daemon
    Broker,

    /// Run the hotkey client
    Hotkey {
        /// Capture hotkey binding
        #[arg(long, default_value = "Super+Shift+C")]
        capture_key: String,

        /// Paste hotkey binding
        #[arg(long, default_value = "Super+Shift+V")]
        paste_key: String,
    },
}

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "clippyctl", about = "Keyboard-driven agent turn relay")]
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
    Broker {
        /// Maximum number of turns retained per session (minimum 1)
        #[arg(long, default_value = "32", value_parser = clap::value_parser!(u64).range(1..))]
        ring_depth: u64,

        /// Maximum byte size per turn (content truncated beyond this)
        #[arg(long, default_value = "4194304")]
        max_turn_size: usize,
    },

    /// Run the hotkey client
    Hotkey {
        /// Capture hotkey binding
        #[arg(long, default_value = "Super+Shift+C")]
        capture_key: String,

        /// Paste hotkey binding
        #[arg(long, default_value = "Super+Shift+V")]
        paste_key: String,

        /// Clipboard-deliver hotkey binding (capture + copy to clipboard)
        #[arg(long)]
        clipboard_key: Option<String>,
    },

    /// CLI client for broker operations
    Client {
        #[command(subcommand)]
        action: ClientAction,
    },
}

#[derive(Subcommand)]
pub enum ClientAction {
    /// List all active sessions
    #[command(name = "list-sessions")]
    ListSessions,

    /// List turns for a session
    #[command(name = "list-turns")]
    ListTurns {
        /// Session ID to query
        session: String,

        /// Maximum number of turns to return
        #[arg(long)]
        limit: Option<u32>,
    },

    /// Get turn content and metadata by ID
    #[command(name = "get-turn")]
    GetTurn {
        /// Turn ID (format: session_id:seq)
        turn_id: String,

        /// Show only metadata, omit content
        #[arg(long)]
        metadata_only: bool,
    },

    /// Capture latest turn from session to relay buffer
    Capture {
        /// Session ID
        session: String,
    },

    /// Capture specific turn by ID to relay buffer
    #[command(name = "capture-by-id")]
    CaptureByID {
        /// Turn ID (format: session_id:seq)
        turn_id: String,
    },

    /// Paste relay buffer content to session
    Paste {
        /// Target session ID
        session: String,
    },

    /// Deliver relay buffer to a sink
    Deliver {
        /// Sink name: clipboard, file, or inject
        sink: String,

        /// Target session ID (required for inject sink)
        #[arg(long)]
        session: Option<String>,

        /// File path (required for file sink)
        #[arg(long)]
        path: Option<String>,
    },
}

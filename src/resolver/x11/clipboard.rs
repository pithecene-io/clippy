//! X11 clipboard provider — read/write via `xclip`.
//!
//! Wraps `xclip -selection clipboard` for clipboard access. Same
//! mechanism as `broker/sink.rs::deliver_clipboard()` but synchronous
//! (`std::process::Command`) to satisfy the `ClipboardProvider` trait.
//!
//! See CONTRACT_RESOLVER.md §X11Clipboard.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::resolver::{ClipboardProvider, ResolverError};

/// X11 implementation of `ClipboardProvider` via `xclip`.
// Wired into broker sink in PR 4.
#[allow(dead_code)]
pub struct X11ClipboardProvider;

#[allow(dead_code)]
impl X11ClipboardProvider {
    /// Create a new X11 clipboard provider.
    pub fn new() -> Self {
        Self
    }
}

impl ClipboardProvider for X11ClipboardProvider {
    fn write(&self, content: &[u8]) -> Result<(), ResolverError> {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| ResolverError::Clipboard(format!("failed to spawn xclip: {e}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(content)
                .map_err(|e| ResolverError::Clipboard(format!("failed to write to xclip: {e}")))?;
            // Drop stdin to close the pipe so xclip can finish.
        }

        let status = child
            .wait()
            .map_err(|e| ResolverError::Clipboard(format!("failed to wait for xclip: {e}")))?;

        if status.success() {
            Ok(())
        } else {
            Err(ResolverError::Clipboard(format!(
                "xclip exited with status {status}"
            )))
        }
    }

    fn read(&self) -> Result<Vec<u8>, ResolverError> {
        let output = Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| ResolverError::Clipboard(format!("failed to spawn xclip -o: {e}")))?;

        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(ResolverError::Clipboard(format!(
                "xclip -o exited with status {}",
                output.status
            )))
        }
    }
}

//! Terminal management — raw mode, restoration, window size.
//!
//! The [`TerminalGuard`] uses RAII to guarantee terminal restoration
//! on exit, even on panic. See CONTRACT_PTY.md §86–101.

use std::os::fd::{BorrowedFd, RawFd};

use nix::libc;
use nix::sys::termios::{self, SetArg, Termios};

use super::PtyError;

/// RAII guard that restores terminal settings on drop.
///
/// Created by [`enter_raw_mode`](TerminalGuard::enter_raw_mode),
/// which captures the current settings, enters raw mode, and sets
/// stdin to non-blocking.
///
/// The guard provides explicit [`restore`](TerminalGuard::restore) and
/// [`reenter_raw`](TerminalGuard::reenter_raw) methods for SIGTSTP/
/// SIGCONT handling. The [`Drop`] impl is the safety net for all other
/// exit paths.
pub struct TerminalGuard {
    original: Termios,
    fd: RawFd,
    /// Original fcntl flags for stdin, restored on drop.
    original_flags: nix::fcntl::OFlag,
}

impl TerminalGuard {
    /// Capture current terminal settings, enter raw mode, and set
    /// stdin to non-blocking.
    ///
    /// Raw mode disables line buffering, local echo, and signal
    /// generation by the terminal driver — keystrokes are forwarded
    /// immediately to the PTY wrapper.
    pub fn enter_raw_mode() -> Result<Self, PtyError> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: STDIN_FILENO is valid for the process lifetime.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };

        // Capture original settings.
        let original = termios::tcgetattr(borrowed).map_err(PtyError::Terminal)?;

        // Capture original fcntl flags (for non-blocking restore).
        let original_flags = nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_GETFL)
            .map(nix::fcntl::OFlag::from_bits_truncate)
            .map_err(PtyError::Terminal)?;

        // Apply raw mode.
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(borrowed, SetArg::TCSANOW, &raw).map_err(PtyError::Terminal)?;

        // Set stdin to non-blocking for AsyncFd integration.
        nix::fcntl::fcntl(
            borrowed,
            nix::fcntl::FcntlArg::F_SETFL(original_flags | nix::fcntl::OFlag::O_NONBLOCK),
        )
        .map_err(PtyError::Terminal)?;

        Ok(Self {
            original,
            fd,
            original_flags,
        })
    }

    /// Explicitly restore terminal settings.
    ///
    /// Used during SIGTSTP handling — restore before suspending so
    /// the user's shell works while the wrapper is stopped.
    pub fn restore(&self) -> Result<(), PtyError> {
        // SAFETY: self.fd is STDIN_FILENO, valid for process lifetime.
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        // Restore original fcntl flags (removes non-blocking).
        nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_SETFL(self.original_flags))
            .map_err(PtyError::Terminal)?;
        // Restore original termios settings.
        termios::tcsetattr(borrowed, SetArg::TCSANOW, &self.original)
            .map_err(PtyError::Terminal)?;
        Ok(())
    }

    /// Re-enter raw mode after being resumed (SIGCONT).
    ///
    /// Re-applies raw mode and non-blocking settings that were
    /// cleared by [`restore`](TerminalGuard::restore).
    pub fn reenter_raw(&self) -> Result<(), PtyError> {
        // SAFETY: self.fd is STDIN_FILENO, valid for process lifetime.
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let mut raw = self.original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(borrowed, SetArg::TCSANOW, &raw).map_err(PtyError::Terminal)?;

        // Re-set non-blocking.
        nix::fcntl::fcntl(
            borrowed,
            nix::fcntl::FcntlArg::F_SETFL(self.original_flags | nix::fcntl::OFlag::O_NONBLOCK),
        )
        .map_err(PtyError::Terminal)?;

        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // SAFETY: self.fd is STDIN_FILENO, valid for process lifetime.
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        // Restore non-blocking flags first.
        let _ = nix::fcntl::fcntl(borrowed, nix::fcntl::FcntlArg::F_SETFL(self.original_flags));
        // Restore terminal settings.
        if let Err(e) = termios::tcsetattr(borrowed, SetArg::TCSANOW, &self.original) {
            eprintln!("WARNING: failed to restore terminal: {e}");
        }
    }
}

/// Read the current terminal window size.
///
/// Returns the dimensions for initializing the child PTY.
pub fn get_terminal_size() -> Result<nix::pty::Winsize, PtyError> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };

    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } < 0 {
        return Err(PtyError::Terminal(nix::Error::last()));
    }

    Ok(nix::pty::Winsize {
        ws_row: ws.ws_row,
        ws_col: ws.ws_col,
        ws_xpixel: ws.ws_xpixel,
        ws_ypixel: ws.ws_ypixel,
    })
}

/// Set the PTY window size from the user's current terminal.
///
/// Called on SIGWINCH. Reads the new size from the user's terminal
/// and sets it on the PTY master via `ioctl(TIOCSWINSZ)`.
/// The kernel automatically delivers SIGWINCH to the child.
pub fn propagate_window_size(pty_master_fd: RawFd) -> Result<(), PtyError> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };

    // Read from user's terminal.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } < 0 {
        return Err(PtyError::Terminal(nix::Error::last()));
    }

    // Set on PTY master — kernel delivers SIGWINCH to child.
    if unsafe { libc::ioctl(pty_master_fd, libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(PtyError::Terminal(nix::Error::last()));
    }

    tracing::debug!(rows = ws.ws_row, cols = ws.ws_col, "window resized");
    Ok(())
}

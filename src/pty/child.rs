//! Child process spawning — PTY allocation, fork, exec.
//!
//! Isolates the `unsafe` fork/exec code from the rest of the wrapper.
//! See CONTRACT_PTY.md §76–83 (PTY allocation), §144–178 (lifecycle).

use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};

use nix::libc;

use nix::pty::{Winsize, openpty};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, execvp, fork, setsid};

use super::PtyError;

/// A spawned child process with its PTY master fd.
pub struct ChildProcess {
    /// Child process PID.
    pub pid: Pid,
    /// Master side of the PTY pair (non-blocking).
    pub master: OwnedFd,
}

/// Spawn a child process on a new PTY.
///
/// Allocates a PTY pair, forks, sets up the slave as the child's
/// controlling terminal, and execs the command. The master fd is
/// returned in non-blocking mode for async I/O.
///
/// # Safety
///
/// Uses `fork()` internally. Only async-signal-safe operations are
/// performed between fork and exec/exit in the child branch.
pub fn spawn_child(command: &[String], winsize: &Winsize) -> Result<ChildProcess, PtyError> {
    if command.is_empty() {
        return Err(PtyError::Exec("empty command".into()));
    }

    // Allocate PTY pair with initial dimensions matching user's terminal.
    let pty = openpty(Some(winsize), None).map_err(PtyError::PtyAlloc)?;
    let master = pty.master;
    let slave = pty.slave;

    // Set master to non-blocking for tokio AsyncFd.
    nix::fcntl::fcntl(
        &master,
        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
    )
    .map_err(PtyError::PtyAlloc)?;

    // Prepare C strings for exec before forking (heap allocation).
    let c_args: Vec<CString> = command
        .iter()
        .filter_map(|s| CString::new(s.as_bytes()).ok())
        .collect();
    if c_args.is_empty() {
        return Err(PtyError::Exec("command contains null bytes".into()));
    }

    // SAFETY: Between fork() and exec()/_exit(), only async-signal-safe
    // functions are called. All heap allocation happens before fork.
    match unsafe { fork() }.map_err(PtyError::Fork)? {
        ForkResult::Parent { child } => {
            // Parent: close slave (child owns it), return master.
            drop(slave);
            Ok(ChildProcess { pid: child, master })
        }
        ForkResult::Child => {
            // -- Child branch: async-signal-safe only --

            // Close master (parent owns it).
            drop(master);

            // Create new session (detach from parent's controlling terminal).
            if setsid().is_err() {
                unsafe { libc::_exit(1) };
            }

            // Set slave as controlling terminal.
            let slave_fd = slave.as_raw_fd();
            if unsafe { libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) } < 0 {
                unsafe { libc::_exit(1) };
            }

            // Redirect stdin/stdout/stderr to slave.
            // Use libc::dup2 directly — async-signal-safe, and nix::dup2
            // changed signature in 0.30 (takes &mut OwnedFd).
            if unsafe { libc::dup2(slave_fd, 0) } < 0
                || unsafe { libc::dup2(slave_fd, 1) } < 0
                || unsafe { libc::dup2(slave_fd, 2) } < 0
            {
                unsafe { libc::_exit(1) };
            }

            // Close the original slave fd if it's not 0, 1, or 2.
            if slave_fd > 2 {
                drop(slave);
            } else {
                // Leak the fd — it's now stdin/stdout/stderr.
                std::mem::forget(slave);
            }

            // Close all other inherited fds (best-effort).
            for fd in 3..1024 {
                unsafe { libc::close(fd) };
            }

            // Exec the command — replaces process image.
            let _ = execvp(&c_args[0], &c_args);

            // If exec failed, exit with 127 (command not found convention).
            unsafe { libc::_exit(127) };
        }
    }
}

/// Wait for the child process to exit and return its exit code.
///
/// For signal-terminated children, returns 128 + signal number
/// per standard convention.
///
/// This should be called after the I/O loop exits (PTY EOF or
/// graceful shutdown). Uses blocking `waitpid` — the child has
/// already exited or is about to.
pub fn wait_for_exit(pid: Pid) -> Result<i32, PtyError> {
    loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)).map_err(PtyError::Signal)? {
            WaitStatus::Exited(_, code) => return Ok(code),
            WaitStatus::Signaled(_, sig, _) => return Ok(128 + sig as i32),
            WaitStatus::StillAlive => {
                // Child still running — brief sleep then retry.
                // This path is rare (PTY EOF usually means child exited).
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                // Other states (stopped, continued) — keep waiting.
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

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
#[derive(Debug)]
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

    // Validate and prepare C strings before any resource allocation.
    // Reject arguments containing NUL bytes rather than silently
    // dropping them (which would mutate the effective argv).
    let c_args: Vec<CString> = command
        .iter()
        .map(|s| {
            CString::new(s.as_bytes())
                .map_err(|_| PtyError::Exec(format!("argument contains null byte: {s:?}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

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
/// Default window size for tests.
#[cfg(test)]
fn test_winsize() -> Winsize {
    Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_command_rejected() {
        let ws = test_winsize();
        let err = spawn_child(&[], &ws).unwrap_err();
        assert!(
            matches!(err, PtyError::Exec(ref msg) if msg.contains("empty command")),
            "expected Exec error, got: {err}"
        );
    }

    #[test]
    fn nul_byte_in_argument_rejected() {
        let ws = test_winsize();
        let cmd = vec!["echo".into(), "hello\0world".into()];
        let err = spawn_child(&cmd, &ws).unwrap_err();
        assert!(
            matches!(err, PtyError::Exec(ref msg) if msg.contains("null byte")),
            "expected Exec error about null byte, got: {err}"
        );
    }

    #[test]
    fn nul_byte_in_first_argument_rejected() {
        let ws = test_winsize();
        let cmd = vec!["\0bad".into()];
        let err = spawn_child(&cmd, &ws).unwrap_err();
        assert!(
            matches!(err, PtyError::Exec(ref msg) if msg.contains("null byte")),
            "expected Exec error about null byte, got: {err}"
        );
    }

    #[test]
    fn spawn_true_exits_zero() {
        let ws = test_winsize();
        let child = spawn_child(&["true".into()], &ws).unwrap();
        let code = wait_for_exit(child.pid).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn spawn_false_exits_nonzero() {
        let ws = test_winsize();
        let child = spawn_child(&["false".into()], &ws).unwrap();
        let code = wait_for_exit(child.pid).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn nonexistent_command_exits_127() {
        let ws = test_winsize();
        let child = spawn_child(&["__clippy_nonexistent_cmd_12345__".into()], &ws).unwrap();
        let code = wait_for_exit(child.pid).unwrap();
        assert_eq!(code, 127);
    }

    #[test]
    fn spawn_preserves_arguments() {
        // Spawn a command that writes its argument count to the PTY.
        // Use `sh -c 'echo $#' -- a b c` → should output "3".
        let ws = test_winsize();
        let child = spawn_child(
            &[
                "sh".into(),
                "-c".into(),
                "echo $#".into(),
                "--".into(),
                "a".into(),
                "b".into(),
                "c".into(),
            ],
            &ws,
        )
        .unwrap();

        // Read output from PTY master.
        let mut buf = [0u8; 256];
        let mut output = Vec::new();
        loop {
            match nix::unistd::read(&child.master, &mut buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(nix::Error::EAGAIN) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    // Check if child exited.
                    if let Ok(WaitStatus::Exited(..)) =
                        waitpid(child.pid, Some(WaitPidFlag::WNOHANG))
                    {
                        // Drain remaining.
                        while let Ok(n) = nix::unistd::read(&child.master, &mut buf) {
                            if n == 0 {
                                break;
                            }
                            output.extend_from_slice(&buf[..n]);
                        }
                        break;
                    }
                }
                Err(nix::Error::EIO) => break, // PTY closed.
                Err(e) => panic!("read error: {e}"),
            }
        }

        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains('3'),
            "expected output to contain '3', got: {text:?}"
        );
    }
}

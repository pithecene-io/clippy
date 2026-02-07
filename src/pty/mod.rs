//! PTY wrapper — transparent I/O mediation and turn detection.
//!
//! Wraps a child process in a pseudoterminal, mediates I/O between
//! the user's terminal and the child, feeds output to the turn
//! detector, and relays completed turns to the broker daemon.
//!
//! See CONTRACT_PTY.md.

mod broker_client;
mod child;
mod terminal;

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

use nix::libc;

use futures::StreamExt;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tokio::io::unix::AsyncFd;
use tokio::signal::unix::{SignalKind, signal as tokio_signal};
use tokio::time;

use broker_client::BrokerClient;
use child::{spawn_child, wait_for_exit};
use terminal::{TerminalGuard, get_terminal_size, propagate_window_size};

use crate::turn::{TurnDetector, TurnError, TurnEvent};

/// PTY wrapper errors.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("PTY allocation failed: {0}")]
    PtyAlloc(nix::Error),
    #[error("fork failed: {0}")]
    Fork(nix::Error),
    #[error("exec failed: {0}")]
    Exec(String),
    #[error("terminal error: {0}")]
    Terminal(nix::Error),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("turn detector error: {0}")]
    TurnDetector(#[from] TurnError),
    #[error("broker: {0}")]
    Broker(String),
    #[error("signal error: {0}")]
    Signal(nix::Error),
}

/// Run a PTY-wrapped session for the given command with turn detection.
///
/// This is the main entry point called from `main.rs` for the `wrap`
/// command. Returns the child's exit code on success.
///
/// # Contract compliance
///
/// - I/O transparency: raw bytes forwarded unmodified (§42–73)
/// - PTY allocation per session with matching dimensions (§76–83)
/// - Raw mode with RAII restore (§86–101)
/// - Turn detection in-process, non-blocking (§104–123)
/// - Broker optional — standalone if unreachable (§155–158)
/// - Signal forwarding per full table (§182–198) incl. SIGTSTP/SIGCONT
/// - SIGWINCH → TIOCSWINSZ, not forwarded (§200–211)
/// - Late registration with local turn buffer (§119, §155–158)
/// - Exit with child's code (§169–178)
pub async fn run_session(pattern: String, command: Vec<String>) -> Result<i32, PtyError> {
    // Generate session ID.
    let session_id = uuid::Uuid::new_v4().to_string();

    // Initialize turn detector (fail early on invalid pattern).
    let mut turn_detector = TurnDetector::new(&pattern)?;

    // Install signal handlers BEFORE entering raw mode.
    let mut sig_int = tokio_signal(SignalKind::interrupt())?;
    let mut sig_term = tokio_signal(SignalKind::terminate())?;
    let mut sig_hup = tokio_signal(SignalKind::hangup())?;
    let mut sig_quit = tokio_signal(SignalKind::quit())?;
    let mut sig_winch = tokio_signal(SignalKind::window_change())?;
    let mut sig_tstp = tokio_signal(SignalKind::from_raw(libc::SIGTSTP))?;
    let mut sig_cont = tokio_signal(SignalKind::from_raw(libc::SIGCONT))?;

    // Get terminal dimensions for the child PTY.
    let winsize = get_terminal_size()?;

    // Spawn child process with PTY.
    let child_result = spawn_child(&command, &winsize)?;
    let child_pid = child_result.pid;
    let master_fd = child_result.master.as_raw_fd();

    tracing::info!(
        session = %session_id,
        pid = child_pid.as_raw(),
        command = ?command,
        "session started"
    );

    // Enter raw mode (RAII guard ensures restore on any exit path).
    let terminal_guard = TerminalGuard::enter_raw_mode()?;

    // Attempt to connect to broker (optional — standalone if unreachable).
    let mut broker_client =
        match BrokerClient::connect(&session_id, child_pid.as_raw() as u32, &pattern).await {
            Ok(client) => {
                tracing::info!("connected to broker");
                Some(client)
            }
            Err(e) => {
                tracing::warn!(error = %e, "broker unavailable — running standalone");
                None
            }
        };

    // Wrap PTY master in AsyncFd for tokio integration.
    // We need to keep `child_result.master` alive (owns the fd).
    let pty_async = AsyncFd::new(child_result.master)?;

    // Non-owning stdin wrapper (don't close on drop).
    let stdin_async = AsyncFd::new(StdinFd)?;

    // Latest completed turn buffer — retained locally for late
    // registration when broker is unreachable (CONTRACT_PTY.md §119).
    let mut latest_turn: Option<crate::turn::Turn> = None;

    // -- Main I/O loop --
    let mut stdin_buf = [0u8; 8192];
    let mut pty_buf = [0u8; 8192];

    let loop_result: Result<(), PtyError> = loop {
        // Pending turns to send after select! (avoids borrow conflicts).
        // Vec instead of Option: a single read chunk can emit multiple turns.
        let mut pending_turns: Vec<crate::turn::Turn> = Vec::new();

        tokio::select! {
            // -- User stdin → PTY master --
            guard = stdin_async.readable() => {
                let mut guard = guard?;
                match guard.try_io(|_| {
                    nix_read(libc::STDIN_FILENO, &mut stdin_buf)
                }) {
                    Ok(Ok(0)) => {
                        // stdin EOF — unusual in raw mode but handle gracefully.
                        break Ok(());
                    }
                    Ok(Ok(n)) => {
                        // Forward to PTY master unmodified.
                        nix_write_all(master_fd, &stdin_buf[..n])?;

                        // Detect Enter key → notify turn detector.
                        if stdin_buf[..n].iter().any(|&b| b == b'\r' || b == b'\n') {
                            turn_detector.notify_user_input();
                        }
                    }
                    Ok(Err(e)) => break Err(e.into()),
                    Err(_would_block) => {} // Spurious wakeup.
                }
            }

            // -- PTY master → stdout + turn detector --
            guard = pty_async.readable() => {
                let mut guard = guard?;
                match guard.try_io(|inner| {
                    nix_read(inner.as_raw_fd(), &mut pty_buf)
                }) {
                    Ok(Ok(0)) => {
                        // PTY EOF — child exited.
                        break Ok(());
                    }
                    Ok(Ok(n)) => {
                        // Forward to stdout unmodified.
                        nix_write_all(libc::STDOUT_FILENO, &pty_buf[..n])?;

                        // Feed to turn detector.
                        let events = turn_detector.feed_output(&pty_buf[..n]);
                        for event in events {
                            match event {
                                TurnEvent::SessionReady => {
                                    tracing::info!("session ready — first prompt detected");
                                }
                                TurnEvent::TurnCompleted(turn) => {
                                    tracing::debug!(
                                        len = turn.content.len(),
                                        interrupted = turn.interrupted,
                                        "turn completed"
                                    );
                                    pending_turns.push(turn);
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => break Err(e.into()),
                    Err(_would_block) => {} // Spurious wakeup.
                }
            }

            // -- Broker inject messages --
            msg = async {
                match broker_client.as_mut() {
                    Some(b) => b.stream_mut().next().await,
                    None => std::future::pending().await,
                }
            } => {
                match msg {
                    Some(Ok(crate::ipc::protocol::Message::Inject { content, .. })) => {
                        // Write injected bytes to PTY master input.
                        tracing::debug!(len = content.len(), "inject received");
                        nix_write_all(master_fd, &content)?;
                    }
                    Some(Ok(crate::ipc::protocol::Message::Response { .. })) => {
                        // Ack to a previous request — ignore.
                    }
                    Some(Ok(other)) => {
                        tracing::warn!(?other, "unexpected broker message");
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "broker codec error — disconnecting");
                        broker_client = None;
                    }
                    None => {
                        tracing::warn!("broker disconnected");
                        broker_client = None;
                    }
                }
            }

            // -- Signal handlers --
            _ = sig_int.recv() => {
                turn_detector.notify_interrupt();
                forward_signal(child_pid, Signal::SIGINT)?;
            }

            _ = sig_term.recv() => {
                forward_signal(child_pid, Signal::SIGTERM)?;
                break Ok(());
            }

            _ = sig_hup.recv() => {
                forward_signal(child_pid, Signal::SIGHUP)?;
            }

            _ = sig_quit.recv() => {
                forward_signal(child_pid, Signal::SIGQUIT)?;
            }

            _ = sig_winch.recv() => {
                if let Err(e) = propagate_window_size(master_fd) {
                    tracing::warn!(error = %e, "SIGWINCH handling failed");
                }
            }

            _ = sig_tstp.recv() => {
                // CONTRACT_PTY.md §190: forward to child, suspend wrapper.
                forward_signal(child_pid, Signal::SIGTSTP)?;
                // Restore terminal before suspending so the user's shell
                // works while we're stopped.
                if let Err(e) = terminal_guard.restore() {
                    tracing::warn!(error = %e, "terminal restore before SIGTSTP failed");
                }
                // Raise SIGSTOP on self — tokio consumed SIGTSTP, so we
                // use SIGSTOP to actually suspend the process.
                signal::kill(Pid::this(), Signal::SIGSTOP).map_err(PtyError::Signal)?;
                // Execution resumes here after SIGCONT — re-enter raw mode.
                if let Err(e) = terminal_guard.reenter_raw() {
                    tracing::warn!(error = %e, "terminal re-raw after resume failed");
                }
            }

            _ = sig_cont.recv() => {
                // CONTRACT_PTY.md §191: forward to child, resume wrapper.
                forward_signal(child_pid, Signal::SIGCONT)?;
            }
        }

        // Send pending turns (outside select! to avoid borrow conflicts).
        // All broker I/O is bounded by a timeout so it cannot stall the
        // main I/O loop (CONTRACT_PTY.md §46, §49).
        if !pending_turns.is_empty() {
            // Always update the local latest-turn buffer (for late registration).
            latest_turn = pending_turns.last().cloned();

            const BROKER_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

            if let Some(ref mut broker) = broker_client {
                for turn in &pending_turns {
                    match time::timeout(BROKER_IO_TIMEOUT, broker.send_turn(turn)).await {
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "failed to send turn to broker");
                        }
                        Err(_elapsed) => {
                            tracing::warn!("broker send timed out — skipping");
                            break;
                        }
                        Ok(Ok(())) => {}
                    }
                }
            } else if let Some(ref buffered) = latest_turn {
                // Broker disconnected — attempt late registration.
                // CONTRACT_PTY.md §119: retain latest turn and send on
                // successful registration.
                let reconnect = async {
                    let mut client =
                        BrokerClient::connect(&session_id, child_pid.as_raw() as u32, &pattern)
                            .await?;
                    client.send_turn(buffered).await?;
                    Ok::<_, PtyError>(client)
                };
                match time::timeout(BROKER_IO_TIMEOUT, reconnect).await {
                    Ok(Ok(client)) => {
                        tracing::info!("late registration — connected to broker");
                        broker_client = Some(client);
                    }
                    Ok(Err(_)) | Err(_) => {
                        // Unreachable or timed out — will retry on next turn.
                    }
                }
            }
        }
    };

    // -- Post-loop cleanup --

    // Flush any unterminated prompt.
    const SHUTDOWN_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
    let events = turn_detector.flush_line();
    for event in events {
        if let TurnEvent::TurnCompleted(turn) = event {
            latest_turn = Some(turn.clone());
            if let Some(ref mut broker) = broker_client {
                let _ = time::timeout(SHUTDOWN_IO_TIMEOUT, broker.send_turn(&turn)).await;
            }
        }
    }

    // Suppress unused warning when broker is never connected.
    drop(latest_turn);

    // Deregister from broker.
    if let Some(ref mut broker) = broker_client {
        let _ = time::timeout(SHUTDOWN_IO_TIMEOUT, broker.deregister()).await;
    }

    // Wait for child exit.
    let exit_code = wait_for_exit(child_pid)?;

    // Terminal guard drops here → restores terminal.
    drop(terminal_guard);

    tracing::info!(exit_code, "session ended");

    if let Err(e) = loop_result {
        tracing::warn!(error = %e, "I/O loop error");
    }

    Ok(exit_code)
}

// -- Helpers --

/// Forward a signal to the child's process group.
fn forward_signal(child_pid: Pid, sig: Signal) -> Result<(), PtyError> {
    // Negative PID → send to process group.
    signal::kill(Pid::from_raw(-child_pid.as_raw()), sig).map_err(PtyError::Signal)
}

/// Non-owning wrapper for stdin fd — used with `AsyncFd`.
///
/// Does NOT close the fd on drop (stdin must remain open).
struct StdinFd;

impl AsRawFd for StdinFd {
    fn as_raw_fd(&self) -> RawFd {
        libc::STDIN_FILENO
    }
}

/// Read from a raw fd, converting nix errors to `io::Error`.
fn nix_read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: fd is STDIN_FILENO or PTY master, valid for session lifetime.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    nix::unistd::read(borrowed, buf).map_err(|e| io::Error::from_raw_os_error(e as i32))
}

/// Write all bytes to a raw fd.
fn nix_write_all(fd: RawFd, mut data: &[u8]) -> Result<(), PtyError> {
    // SAFETY: fd is PTY master or STDOUT_FILENO, valid for session lifetime.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    while !data.is_empty() {
        match nix::unistd::write(borrowed, data) {
            Ok(n) => data = &data[n..],
            Err(nix::Error::EINTR) => continue,
            Err(nix::Error::EAGAIN) => {
                // Non-blocking fd is full — brief yield then retry.
                std::thread::yield_now();
            }
            Err(e) => return Err(PtyError::Io(io::Error::from_raw_os_error(e as i32))),
        }
    }
    Ok(())
}

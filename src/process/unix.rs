use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::bail;
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tracing::{debug, warn};

use crate::Result;

use super::Exit;

/// A handle to a running child process.
///
/// The `std::process::Child` itself is moved into, and owned by, a dedicated
/// waiter thread that performs a blocking `wait()` (i.e. `waitpid(2)`) and
/// reports the exit status back over `exit_rx`. That thread is the single
/// reaper of the child, so nothing else may call `wait`/`try_wait`/`waitpid`
/// on this pid.
///
/// This lets the main loop block on `exit_rx` with a short timeout: it wakes
/// essentially immediately when the child exits, rather than only noticing on
/// the next poll, while still returning at the poll cadence to tick the
/// progress bar and check for a timeout or Ctrl-C interrupt.
pub(super) struct ChildHandle {
    pid: u32,
    exit_rx: Receiver<io::Result<ExitStatus>>,
}

/// Spawn `command`, handing the child over to a waiter thread that blocks on
/// `wait()` and reports the result back through a channel.
pub(super) fn spawn(mut command: Command) -> io::Result<ChildHandle> {
    let mut child = command.spawn()?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("cargo-mutants child waiter".to_owned())
        .spawn(move || {
            // Ignore the error: it just means the receiver (and `Process`) was
            // dropped before the child exited; the `wait()` above still reaps it.
            let _ = tx.send(child.wait());
        })
        .expect("failed to spawn child-waiter thread");
    Ok(ChildHandle { pid, exit_rx: rx })
}

impl ChildHandle {
    pub(super) fn id(&self) -> u32 {
        self.pid
    }

    /// Wait for the child to exit, for up to `timeout`.
    ///
    /// Returns `Ok(None)` if the child is still running when `timeout` elapses;
    /// returns as soon as the child exits if that happens sooner.
    pub(super) fn wait_for_exit(&mut self, timeout: Duration) -> Result<Option<Exit>> {
        match self.exit_rx.recv_timeout(timeout) {
            Ok(result) => Ok(Some(result?.into())),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("child waiter thread exited without reporting a status")
            }
        }
    }

    /// Terminate the child, and block until the waiter thread confirms it's gone.
    pub(super) fn terminate(&mut self) -> Result<()> {
        terminate_child(self.pid)?;
        match self.exit_rx.recv() {
            Ok(Ok(exit)) => debug!("terminated child exit status {exit:?}"),
            Ok(Err(err)) => debug!(?err, "Failed to wait for child after termination"),
            Err(_) => debug!("child waiter thread exited without reporting a status"),
        }
        Ok(())
    }
}

#[mutants::skip] // hard to exercise the ESRCH edge case
fn terminate_child(pid: u32) -> Result<()> {
    let pid = Pid::from_raw(pid.try_into().unwrap());
    match killpg(pid, Signal::SIGTERM) {
        Ok(()) => Ok(()),
        Err(Errno::ESRCH) => {
            Ok(()) // Probably already gone
        }
        Err(Errno::EPERM) if cfg!(target_os = "macos") => {
            Ok(()) // If the process no longer exists then macos can return EPERM (maybe?)
        }
        Err(errno) => {
            // TODO: Maybe strerror?
            let message = format!("failed to terminate child: error {errno}");
            warn!("{}", message);
            bail!(message);
        }
    }
}

#[mutants::skip]
pub(super) fn configure_command(command: &mut Command) {
    command.process_group(0);
}

impl From<ExitStatus> for Exit {
    fn from(status: ExitStatus) -> Self {
        if let Some(code) = status.code() {
            if code == 0 {
                Exit::Success
            } else {
                Exit::Failure(code)
            }
        } else if let Some(signal) = status.signal() {
            Exit::Signalled(signal)
        } else {
            Exit::Other
        }
    }
}

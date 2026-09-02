use std::io;
use std::process::{Child, Command, ExitStatus};
use std::thread::sleep;
use std::time::Duration;

use anyhow::Context;
use tracing::debug;

use crate::Result;

use super::Exit;

/// A handle to a running child process.
///
/// Unlike the Unix implementation (see `process::unix`), this polls
/// `try_wait` rather than blocking on the child's exit: the standard library
/// doesn't offer a wait-with-timeout primitive for `Child` on Windows, and
/// implementing one would mean calling into the Win32 API directly (e.g.
/// `WaitForSingleObject` on the child's raw handle) rather than using only
/// existing dependencies. So on Windows a normal child exit is still noticed
/// with up to one poll interval of latency, same as before this change.
pub(super) struct ChildHandle {
    child: Child,
}

pub(super) fn spawn(mut command: Command) -> io::Result<ChildHandle> {
    Ok(ChildHandle {
        child: command.spawn()?,
    })
}

impl ChildHandle {
    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    /// Wait for the child to exit, for up to `timeout`.
    ///
    /// Returns `Ok(None)` if the child is still running when `timeout` elapses.
    pub(super) fn wait_for_exit(&mut self, timeout: Duration) -> Result<Option<Exit>> {
        if let Some(status) = self.child.try_wait()? {
            return Ok(Some(status.into()));
        }
        sleep(timeout);
        Ok(self.child.try_wait()?.map(Into::into))
    }

    /// Terminate the child, and block until it's gone.
    pub(super) fn terminate(&mut self) -> Result<()> {
        terminate_child(&mut self.child)?;
        match self.child.wait() {
            Err(err) => debug!(?err, "Failed to wait for child after termination"),
            Ok(exit) => debug!("terminated child exit status {exit:?}"),
        }
        Ok(())
    }
}

#[mutants::skip] // hard to exercise the ESRCH edge case
fn terminate_child(child: &mut Child) -> Result<()> {
    child.kill().context("Kill child")
}

#[mutants::skip]
pub(super) fn configure_command(_command: &mut Command) {}

impl From<ExitStatus> for Exit {
    fn from(status: ExitStatus) -> Self {
        if let Some(code) = status.code() {
            if code == 0 {
                Exit::Success
            } else {
                Exit::Failure(code)
            }
        } else {
            Exit::Other
        }
    }
}

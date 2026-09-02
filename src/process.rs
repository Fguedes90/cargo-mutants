// Copyright 2021-2024 Martin Pool

//! Manage a subprocess, with polling, timeouts, termination, and so on.
//!
//! On Unix, the subprocess runs as its own process group, so that any
//! grandchild processes are also signalled if it's interrupted.

#![warn(clippy::pedantic)]
#![allow(clippy::redundant_else)]

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use camino::Utf8Path;
use serde::Serialize;
use tracing::{Level, debug, span};

use crate::Result;
use crate::console::Console;
use crate::interrupt::check_interrupted;
use crate::output::ScenarioOutput;

/// How frequently to check for a timeout or Ctrl-C interrupt, and to tick the
/// progress bar, while a subprocess is running.
///
/// This is *not* the latency with which a normal child exit is noticed: the
/// platform child handle wakes (on Unix, essentially immediately; see
/// `process::unix`) as soon as the child exits, rather than waiting for the
/// next poll.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::{ChildHandle, configure_command, spawn as spawn_child};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::{ChildHandle, configure_command, spawn as spawn_child};

pub struct Process {
    child: ChildHandle,
    start: Instant,
    timeout: Option<Duration>,
}

impl Process {
    /// Run a subprocess to completion, watching for interrupts, with a timeout, while
    /// ticking the progress bar.
    pub fn run(
        argv: &[String],
        env: &[(String, String)],
        cwd: &Utf8Path,
        timeout: Option<Duration>,
        jobserver: Option<&jobserver::Client>,
        scenario_output: &mut ScenarioOutput,
        console: &Console,
    ) -> Result<Exit> {
        let mut child = Process::start(argv, env, cwd, timeout, jobserver, scenario_output)?;
        let process_status = loop {
            if let Some(exit_status) = child.wait_step(WAIT_POLL_INTERVAL)? {
                break exit_status;
            }
            console.tick();
        };
        scenario_output.message(&format!("result: {process_status:?}"))?;
        Ok(process_status)
    }

    /// Launch a process, and return an object representing the child.
    pub fn start(
        argv: &[String],
        env: &[(String, String)],
        cwd: &Utf8Path,
        timeout: Option<Duration>,
        jobserver: Option<&jobserver::Client>,
        scenario_output: &mut ScenarioOutput,
    ) -> Result<Process> {
        let start = Instant::now();
        let quoted_argv = quote_argv(argv);
        scenario_output.message(&quoted_argv)?;
        debug!(%quoted_argv, "start process");
        let os_env = env.iter().map(|(k, v)| (OsStr::new(k), OsStr::new(v)));
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .envs(os_env)
            .stdin(Stdio::null())
            .stdout(scenario_output.open_log_append()?)
            .stderr(scenario_output.open_log_append()?)
            .current_dir(cwd);
        if let Some(js) = jobserver {
            js.configure(&mut command);
        }
        configure_command(&mut command);
        let child =
            spawn_child(command).with_context(|| format!("failed to spawn {}", argv.join(" ")))?;
        Ok(Process {
            child,
            start,
            timeout,
        })
    }

    /// Check the timeout and Ctrl-C interrupt, and otherwise wait for the child to
    /// exit, for up to `poll_interval`.
    ///
    /// Returns `Ok(None)` if `poll_interval` elapses with the child still running, so
    /// that the caller can tick the progress bar and loop again. If the child exits
    /// before `poll_interval` elapses this returns as soon as that's observed, rather
    /// than waiting out the rest of the interval.
    #[mutants::skip] // It's hard to avoid timeouts if this never works...
    fn wait_step(&mut self, poll_interval: Duration) -> Result<Option<Exit>> {
        if self.timeout.is_some_and(|t| self.start.elapsed() > t) {
            debug!("timeout, terminating child process...",);
            self.terminate()?;
            Ok(Some(Exit::Timeout))
        } else if let Err(e) = check_interrupted() {
            debug!("interrupted, terminating child process...");
            self.terminate()?;
            Err(e)
        } else {
            self.child.wait_for_exit(poll_interval)
        }
    }

    /// Terminate the subprocess, initially gently and then harshly.
    ///
    /// Blocks until the subprocess is terminated and then returns the exit status.
    ///
    /// The status might not be `Timeout` if this raced with a normal exit.
    #[mutants::skip] // would leak processes from tests if skipped
    fn terminate(&mut self) -> Result<()> {
        let _span = span!(Level::DEBUG, "terminate_child", pid = self.child.id()).entered();
        debug!("terminating child process");
        self.child.terminate()
    }
}

/// The result of running a single child process.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Exit {
    /// Exited with status 0.
    Success,
    /// Exited with status non-0.
    Failure(i32),
    /// Exceeded its timeout, and killed.
    Timeout,
    /// Killed by some signal.
    #[cfg(unix)]
    Signalled(i32),
    /// Unknown or unexpected situation.
    Other,
}

impl Exit {
    pub fn is_success(self) -> bool {
        self == Exit::Success
    }

    pub fn is_timeout(self) -> bool {
        self == Exit::Timeout
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Exit::Failure(_))
    }
}

/// Quote an argv slice in Unix shell style.
///
/// This isn't guaranteed to match the interpretation of a shell or to be safe.
/// It's just for debug logs.
fn quote_argv<S: AsRef<str>, I: IntoIterator<Item = S>>(argv: I) -> String {
    let mut r = String::new();
    for s in argv {
        if !r.is_empty() {
            r.push(' ');
        }
        for c in s.as_ref().chars() {
            match c {
                '\t' => r.push_str(r"\t"),
                '\n' => r.push_str(r"\n"),
                '\r' => r.push_str(r"\r"),
                ' ' | '\\' | '\'' | '"' => {
                    r.push('\\');
                    r.push(c);
                }
                _ => r.push(c),
            }
        }
    }
    r
}

#[cfg(test)]
mod test {
    use std::cmp::min;
    use std::time::{Duration, Instant};

    use camino::Utf8Path;
    use tempfile::tempdir;

    use super::{Exit, Process, quote_argv};
    use crate::console::Console;
    use crate::output::OutputDir;
    use crate::scenario::Scenario;

    #[test]
    fn shell_quoting() {
        assert_eq!(quote_argv(["foo".to_string()]), "foo");
        assert_eq!(
            quote_argv(["foo bar", r"\blah\x", r#""quoted""#]),
            r#"foo\ bar \\blah\\x \"quoted\""#
        );
        assert_eq!(quote_argv([""]), "");
        assert_eq!(
            quote_argv(["with whitespace", "\r\n\t\t"]),
            r"with\ whitespace \r\n\t\t"
        );
    }

    /// A near-instant subprocess exit is observed with much less latency than the
    /// `WAIT_POLL_INTERVAL` poll interval, because the waiter thread wakes the main
    /// loop as soon as the child exits rather than only on the next poll.
    ///
    /// The fastest of several runs is the one that matters: a single run also
    /// measures whatever else the machine was doing, but polling would delay
    /// *every* run to the next tick, so the minimum can only be small if the
    /// exit is really observed as it happens.
    #[cfg(unix)]
    #[test]
    fn a_near_instant_subprocess_is_observed_as_finished_well_under_the_old_poll_interval() {
        let temp_dir = tempdir().unwrap();
        let cwd = Utf8Path::from_path(temp_dir.path()).unwrap();
        let mut output_dir = OutputDir::new(cwd).unwrap();
        let console = Console::new();

        let mut fastest = Duration::MAX;
        for _i in 0..5 {
            let mut scenario_output = output_dir.start_scenario(&Scenario::Baseline).unwrap();
            let start = Instant::now();
            let exit = Process::run(
                &["true".to_owned()],
                &[],
                cwd,
                None,
                None,
                &mut scenario_output,
                &console,
            )
            .unwrap();
            let elapsed = start.elapsed();
            assert!(exit.is_success());
            fastest = min(fastest, elapsed);
        }
        assert!(
            fastest < Duration::from_millis(20),
            "expected a near-instant subprocess exit to be observed in well under \
             the old 50ms poll interval, but the fastest of 5 runs took {fastest:?}",
        );
    }

    /// A subprocess that runs past its timeout is killed and reported as `Exit::Timeout`,
    /// long before it would otherwise have finished on its own.
    #[test]
    fn a_subprocess_exceeding_its_timeout_is_killed_and_reported_as_timeout() {
        let temp_dir = tempdir().unwrap();
        let cwd = Utf8Path::from_path(temp_dir.path()).unwrap();
        let mut output_dir = OutputDir::new(cwd).unwrap();
        let mut scenario_output = output_dir.start_scenario(&Scenario::Baseline).unwrap();
        let console = Console::new();
        let argv: Vec<String> = if cfg!(unix) {
            vec!["sleep".to_owned(), "5".to_owned()]
        } else {
            vec![
                "cmd".to_owned(),
                "/C".to_owned(),
                "ping -n 6 127.0.0.1 >nul".to_owned(),
            ]
        };

        let start = Instant::now();
        let exit = Process::run(
            &argv,
            &[],
            cwd,
            Some(Duration::from_millis(200)),
            None,
            &mut scenario_output,
            &console,
        )
        .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(exit, Exit::Timeout);
        assert!(
            elapsed < Duration::from_secs(3),
            "expected the timeout to fire well before the subprocess would finish on \
             its own, but it took {elapsed:?}",
        );
    }
}

// Copyright 2021-2025 Martin Pool

//! Successively apply mutations to the source code and run cargo to check,
//! build, and test them.

#![warn(clippy::pedantic)]

use std::cmp::{max, min};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::panic::resume_unwind;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use std::{thread, vec};

use anyhow::bail;
use camino::Utf8Path;
use itertools::Itertools;
use jiff::Timestamp;
use tracing::{debug, debug_span, error, trace, warn};

use crate::artifacts::{Fingerprint, fingerprint_build_artifacts, test_executables};
use crate::coverage::{LineCoverage, LlvmTools, instrumented_env, read_coverage};
use crate::outcome::SkipReason;
use crate::{
    BaselineStrategy, BuildDir, Console, Context, Mutant, Options, Phase, Result, Scenario,
    ScenarioOutcome, cargo::encoded_rustflags, cargo::run_cargo, options::TestPackages,
    outcome::LabOutcome, output::OutputDir, package::Package, package::PackageSelection,
    timeouts::Timeouts, workspace::Workspace,
};

/// Run all possible mutation experiments.
///
/// This is called after all filtering is complete, so all the mutants here will be tested
/// or checked.
///
/// Before testing the mutants, the lab checks that the source tree passes its tests with no
/// mutations applied.
pub fn test_mutants(
    mut mutants: Vec<Mutant>,
    workspace: &Workspace,
    output_dir: OutputDir,
    options: &Options,
    console: &Console,
) -> Result<LabOutcome> {
    let start_time = Instant::now();
    if options.detect_equivalent_mutants && options.check_only {
        warn!(
            "--detect-equivalent-mutants has no effect together with --check: \
             there is no test phase to skip"
        );
    }
    let llvm_tools = coverage_tools(options)?;
    console.set_debug_log(output_dir.open_debug_log()?);
    if options.shuffle {
        fastrand::shuffle(&mut mutants);
    }
    output_dir.write_mutants_list(&mutants)?;
    console.discovered_mutants(&mutants);
    if mutants.is_empty() {
        warn!("No mutants found under the active filters");
        return Ok(LabOutcome::new(Timestamp::now()));
    }
    let output_mutex = Mutex::new(output_dir);
    let baseline_build_dir = BuildDir::for_baseline(workspace, options, console)?;
    let jobserver = options
        .jobserver
        .then(|| {
            let n_tasks = options.jobserver_tasks.unwrap_or_else(num_cpus::get);
            debug!(n_tasks, "starting jobserver");
            jobserver::Client::new(n_tasks)
        })
        .transpose()
        .context("Start jobserver")?;
    let tests_for_mutant = TestsForMutant::new(options, workspace);
    let lab = Lab {
        output_mutex,
        jobserver,
        tests_for_mutant,
        options,
        console,
        baseline_fingerprint: OnceLock::new(),
        mutant_fingerprints: Mutex::new(HashMap::new()),
        llvm_tools,
        coverage: OnceLock::new(),
    };
    let timeouts = match options.baseline {
        BaselineStrategy::Run => {
            let outcome = lab.run_baseline(&baseline_build_dir, &mutants)?;
            if outcome.success() {
                Timeouts::from_baseline(&outcome, options)
            } else {
                error!(
                    "cargo {phase} failed in an unmutated tree, so no mutants were tested",
                    phase = outcome
                        .last_phase()
                        .expect("the baseline ran at least one phase"),
                );
                return lab
                    .output_mutex
                    .into_inner()
                    .expect("lock output_dir")
                    .finish();
            }
        }
        BaselineStrategy::Skip => Timeouts::without_baseline(options),
    };
    debug!(?timeouts);

    let build_dir_0 = Mutex::new(Some(baseline_build_dir));
    // Create n threads, each dedicated to one build directory. Each of them tries to take a
    // scenario to test off the queue, and then exits when there are no more left.
    console.start_testing_mutants(mutants.len());
    let n_threads = max(1, min(options.jobs.unwrap_or(1), mutants.len()));
    let work_queue = &Mutex::new(mutants.into_iter());
    thread::scope(|scope| -> crate::Result<()> {
        let mut threads = Vec::new();
        for _i_thread in 0..n_threads {
            threads.push(scope.spawn(|| -> crate::Result<()> {
                trace!(thread_id = ?thread::current().id(), "start thread");
                // First thread to start can use the baseline's build dir;
                // others need to copy a new one
                let build_dir_0 = build_dir_0.lock().expect("lock build dir 0").take(); // separate for lock
                let build_dir = &if let Some(d) = build_dir_0 {
                    d
                } else {
                    BuildDir::copy_from(workspace.root(), options, console)?
                };
                lab.run_queue(build_dir, timeouts, work_queue)
            }));
        }
        join_threads(threads)
    })?;

    let output_dir = lab
        .output_mutex
        .into_inner()
        .expect("final unlock mutants queue");
    console.lab_finished(&output_dir.lab_outcome, start_time, options);
    let lab_outcome = output_dir.finish()?;
    if lab_outcome.total_mutants == 0 {
        // This should be unreachable as we also bail out before copying
        // the tree if no mutants are generated.
        warn!("No mutants were generated");
    } else if lab_outcome.unviable == lab_outcome.total_mutants {
        warn!(
            "No mutants were viable: perhaps there is a problem with building in a scratch directory. Look in mutants.out/log/* for more information."
        );
    }
    Ok(lab_outcome)
}

/// Find the tools needed to measure coverage, if `--skip-uncovered` asked for
/// it, after checking that the run can measure it at all.
///
/// This happens before anything is built, so that a missing toolchain
/// component or a contradictory pair of options fails at once rather than
/// after a baseline build.
fn coverage_tools(options: &Options) -> Result<Option<LlvmTools>> {
    if !options.skip_uncovered {
        return Ok(None);
    }
    if matches!(options.baseline, BaselineStrategy::Skip) {
        bail!(
            "--skip-uncovered measures coverage during the baseline test run, \
             so it can't be used with --baseline=skip"
        );
    }
    if options.check_only {
        bail!(
            "--skip-uncovered measures coverage during the baseline test run, \
             and --check runs no tests"
        );
    }
    Ok(Some(LlvmTools::find()?))
}

#[mutants::skip] // it's a little hard to observe that the threads were collected?
fn join_threads(threads: Vec<thread::ScopedJoinHandle<'_, Result<()>>>) -> Result<()> {
    // The errors potentially returned from `join` are a special `std::thread::Result`
    // that does not implement error, indicating that the thread panicked.
    // Probably the most useful thing is to `resume_unwind` it.
    // Inside that, there's an actual Mutants error indicating a non-panic error.
    // Most likely, this would be "interrupted" but it might be some IO error
    // etc. In that case, print them all and return the first.
    let errors = threads
        .into_iter()
        .filter_map(|thread| match thread.join() {
            Err(panic) => resume_unwind(panic),
            Ok(Ok(())) => None,
            Ok(Err(err)) => {
                // To avoid console spam don't print "interrupted" errors for each thread,
                // since that should have been printed by check_interrupted but do return them.
                if err.to_string() != "interrupted" {
                    error!("Worker thread failed: {:?}", err);
                }
                Some(err)
            }
        })
        .collect_vec();
    if let Some(first_err) = errors.into_iter().next() {
        Err(first_err)
    } else {
        Ok(())
    }
}

/// Common context across all scenarios, threads, and build dirs.
struct Lab<'a> {
    output_mutex: Mutex<OutputDir>,
    jobserver: Option<jobserver::Client>,
    tests_for_mutant: TestsForMutant,
    options: &'a Options,
    console: &'a Console,
    /// The fingerprint of the baseline build's test artifacts, set once after
    /// the baseline succeeds, if `--detect-equivalent-mutants` is on and the
    /// baseline was actually built (i.e. not `--baseline=skip`).
    ///
    /// Read-only from every worker thread once set, so a `OnceLock` avoids
    /// lock contention on the hot path of every mutant's build.
    baseline_fingerprint: OnceLock<Fingerprint>,
    /// Fingerprints of every mutant build seen so far, mapping to the name of
    /// the first mutant that produced it. Used to detect mutants that are
    /// redundant with an earlier one, even without a baseline.
    mutant_fingerprints: Mutex<HashMap<Fingerprint, String>>,
    /// The LLVM tools that read coverage counters, present only if
    /// `--skip-uncovered` asked for coverage to be measured.
    llvm_tools: Option<LlvmTools>,
    /// Which source lines the baseline test run executed, set once after the
    /// baseline succeeds if `--skip-uncovered` is on.
    ///
    /// Read-only from every worker thread once set.
    coverage: OnceLock<LineCoverage>,
}

impl Lab<'_> {
    /// Run the baseline scenario, which is the same as running `cargo test` on the unmutated
    /// tree.
    ///
    /// If it fails, return None, indicating that no further testing should be done.
    ///
    /// If it succeeds, return the timeouts to be used for the other scenarios.
    fn run_baseline(&self, build_dir: &BuildDir, mutants: &[Mutant]) -> Result<ScenarioOutcome> {
        let all_mutated_packages: Vec<Arc<Package>> = mutants
            .iter()
            .map(|m| Arc::clone(&m.source_file.package))
            .sorted_by_key(|p| p.name.clone())
            .unique()
            .collect_vec();
        // If coverage is wanted, the baseline is the run that measures it:
        // it's the only run of the unmutated tree, and instrumenting the
        // mutants instead would measure the wrong thing. The cost is that
        // baseline timings, and so the timeouts derived from them, include
        // the instrumentation overhead; that errs towards longer timeouts.
        let profraw_dir = build_dir.path().join("target").join("mutants-coverage");
        let extra_env = if self.llvm_tools.is_some() {
            instrumented_env(&profraw_dir, encoded_rustflags(self.options).as_deref())?
        } else {
            Vec::new()
        };
        let outcome = self.make_worker(build_dir, &extra_env).run_one_scenario(
            &Scenario::Baseline,
            &PackageSelection::Explicit(all_mutated_packages),
            Timeouts::for_baseline(self.options),
        )?;
        if !outcome.success() {
            return Ok(outcome);
        }
        if self.options.detect_equivalent_mutants {
            if let Some(fingerprint) = fingerprint_build_artifacts(build_dir, self.options) {
                self.baseline_fingerprint
                    .set(fingerprint)
                    .expect("baseline fingerprint set only once");
            } else {
                debug!(
                    "could not fingerprint baseline build artifacts; \
                     equivalent-mutant detection against the baseline is disabled for this run"
                );
            }
        }
        if let Some(tools) = &self.llvm_tools {
            let binaries = test_executables(build_dir, self.options);
            let coverage = read_coverage(tools, &profraw_dir, &binaries, build_dir.path())
                .context("measure coverage of the baseline test run")?;
            debug!(
                covered_files = coverage.covered_files(),
                "measured baseline coverage"
            );
            self.coverage
                .set(coverage)
                .expect("baseline coverage set only once");
        }
        Ok(outcome)
    }

    /// Run until the input queue is empty.
    ///
    /// The queue, inside a mutex, can be consumed by multiple threads.
    fn run_queue(
        &self,
        build_dir: &BuildDir,
        timeouts: Timeouts,
        work_queue: &Mutex<vec::IntoIter<Mutant>>,
    ) -> Result<()> {
        self.make_worker(build_dir, &[])
            .run_queue(work_queue, timeouts)
    }

    fn make_worker<'a>(
        &'a self,
        build_dir: &'a BuildDir,
        extra_env: &'a [(String, String)],
    ) -> Worker<'a> {
        Worker {
            build_dir,
            output_mutex: &self.output_mutex,
            jobserver: self.jobserver.as_ref(),
            tests_for_mutant: &self.tests_for_mutant,
            options: self.options,
            console: self.console,
            baseline_fingerprint: &self.baseline_fingerprint,
            mutant_fingerprints: &self.mutant_fingerprints,
            coverage: &self.coverage,
            extra_env,
        }
    }
}

/// A worker owns one build directory and runs a single thread of testing.
///
/// It consumes jobs from an input queue and runs them until the queue is empty,
/// appending output to the output directory.
struct Worker<'a> {
    build_dir: &'a BuildDir,
    output_mutex: &'a Mutex<OutputDir>,
    jobserver: Option<&'a jobserver::Client>,
    tests_for_mutant: &'a TestsForMutant,
    options: &'a Options,
    console: &'a Console,
    baseline_fingerprint: &'a OnceLock<Fingerprint>,
    mutant_fingerprints: &'a Mutex<HashMap<Fingerprint, String>>,
    /// The lines the baseline test run executed, if `--skip-uncovered` asked
    /// for coverage; empty until the baseline has run.
    coverage: &'a OnceLock<LineCoverage>,
    /// Environment variables to add to every cargo invocation this worker
    /// makes, after everything cargo.rs sets itself.
    extra_env: &'a [(String, String)],
}

impl Worker<'_> {
    /// Run until the input queue is empty.
    fn run_queue(
        mut self,
        work_queue: &Mutex<vec::IntoIter<Mutant>>,
        timeouts: Timeouts,
    ) -> Result<()> {
        let _span = debug_span!("worker thread", build_dir = ?self.build_dir.path()).entered();
        loop {
            // Not a `for` statement so that we don't hold the lock
            // for the whole iteration.
            let Some(mutant) = work_queue.lock().expect("Lock pending work queue").next() else {
                return Ok(());
            };
            let _span = debug_span!("mutant", name = mutant.name(false)).entered();
            let test_packages = match self.tests_for_mutant {
                TestsForMutant::Workspace => PackageSelection::All,
                TestsForMutant::Mutated => {
                    PackageSelection::Explicit(vec![mutant.source_file.package.clone()])
                }
                TestsForMutant::Explicit(packages) => PackageSelection::Explicit(packages.clone()),
            };
            self.run_one_scenario(&Scenario::Mutant(mutant), &test_packages, timeouts)?;
        }
    }

    fn run_one_scenario(
        &mut self,
        scenario: &Scenario,
        test_packages: &PackageSelection,
        timeouts: Timeouts,
    ) -> Result<ScenarioOutcome> {
        let mut scenario_output = self
            .output_mutex
            .lock()
            .expect("lock output_dir to start scenario")
            .start_scenario(scenario)?;
        let dir = self.build_dir.path();
        self.console
            .scenario_started(dir, scenario, scenario_output.open_log_read()?);
        debug!(?test_packages);

        let mut outcome = ScenarioOutcome::new(&scenario_output, scenario.clone());
        if let Some(mutant) = scenario.mutant() {
            // The diff was already computed when `mutants.json` was written.
            scenario_output.write_diff(mutant.cached_diff())?;
            if let Some(message) = self.uncovered_message(mutant) {
                debug!(mutant = mutant.full_name(), "uncovered mutant");
                scenario_output.message(&message)?;
                outcome.set_skip_reason(SkipReason::Uncovered);
                return self.finish_scenario(dir, scenario, outcome);
            }
            mutant.apply(self.build_dir, &mutant.mutated_code())?;
        }
        let extra_env: Vec<(&str, &str)> = self
            .extra_env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();

        for &phase in self.options.phases() {
            self.console.scenario_phase_started(dir, phase);
            let timeout = match phase {
                Phase::Test => timeouts.test,
                Phase::Build | Phase::Check => timeouts.build,
            };
            match run_cargo(
                self.build_dir,
                self.jobserver,
                test_packages,
                phase,
                timeout,
                &mut scenario_output,
                self.options,
                &extra_env,
                self.console,
            ) {
                Ok(phase_result) => {
                    let success = phase_result.is_success(); // so we can move it away
                    outcome.add_phase_result(phase_result);
                    self.console.scenario_phase_finished(dir, phase);
                    if !success {
                        break;
                    }
                    if phase == Phase::Build
                        && self.options.detect_equivalent_mutants
                        && let Some(mutant) = scenario.mutant()
                        && let Some(equivalent_to) = self.equivalent_build(mutant.full_name())
                    {
                        let message = format!(
                            "build artifacts are identical to {equivalent_to}; \
                             skipping the test phase (--detect-equivalent-mutants)"
                        );
                        debug!(mutant = mutant.full_name(), %equivalent_to, "equivalent mutant");
                        scenario_output.message(&message)?;
                        outcome.set_skip_reason(SkipReason::Equivalent);
                        break;
                    }
                }
                Err(err) => {
                    error!(?err, ?phase, "scenario execution internal error");
                    // Some unexpected internal error that stops the program.
                    if let Some(mutant) = scenario.mutant() {
                        mutant.revert(self.build_dir)?;
                    }
                    return Err(err);
                }
            }
        }
        if let Some(mutant) = scenario.mutant() {
            mutant.revert(self.build_dir)?;
        }
        self.finish_scenario(dir, scenario, outcome)
    }

    /// Record a finished scenario's outcome in the output directory and on the
    /// console.
    fn finish_scenario(
        &self,
        dir: &Utf8Path,
        scenario: &Scenario,
        outcome: ScenarioOutcome,
    ) -> Result<ScenarioOutcome> {
        self.output_mutex
            .lock()
            .expect("lock output dir to add outcome")
            .add_scenario_outcome(&outcome)?;
        debug!(outcome = ?outcome.summary());
        self.console
            .scenario_finished(dir, scenario, &outcome, self.options);
        Ok(outcome)
    }

    /// If coverage was measured and no test executed the line this mutant
    /// changes, describe that, for the log and the scenario's output file.
    ///
    /// Returns `None` when coverage was not measured at all, so a run without
    /// `--skip-uncovered` never skips anything.
    fn uncovered_message(&self, mutant: &Mutant) -> Option<String> {
        let coverage = self.coverage.get()?;
        if mutant.const_eval {
            // A compile-time-evaluated position (a const/static initializer,
            // a const fn body, an array length, or a const generic argument)
            // has no coverage counter: `-Cinstrument-coverage` only
            // instruments code the compiled program executes at runtime,
            // and the compiler evaluates these positions itself while
            // building, not by running the instrumented binary. A test can
            // still catch a mutant here (e.g. by asserting on the resulting
            // constant), so treating it as uncovered would be wrong: build
            // and test it as usual.
            debug!(
                mutant = mutant.full_name(),
                "mutant is in a compile-time-evaluated position; never skipping for coverage"
            );
            return None;
        }
        let file = &mutant.source_file.tree_relative_path;
        let line = mutant.span.start.line;
        if coverage.covers(file, line) {
            None
        } else {
            Some(format!(
                "no test executed {file}:{line}, so no test can catch this mutant; \
                 skipping the build and test phases (--skip-uncovered)"
            ))
        }
    }

    /// Check whether the artifacts just built at `self.build_dir` are
    /// indistinguishable from the baseline or an earlier mutant.
    ///
    /// If so, returns a description of what they match, for logging.
    /// Otherwise, if this build's fingerprint could be computed, records it
    /// as the first mutant to produce it (so a later, identical mutant can
    /// be found redundant with this one), and returns `None`.
    ///
    /// Returns `None` without recording anything if the artifacts can't be
    /// fingerprinted (see [`fingerprint_build_artifacts`]): a mutant must
    /// never be treated as equivalent just because we couldn't tell.
    fn equivalent_build(&self, mutant_name: &str) -> Option<String> {
        let fingerprint = fingerprint_build_artifacts(self.build_dir, self.options)?;
        if self.baseline_fingerprint.get() == Some(&fingerprint) {
            return Some("the unmutated baseline".to_owned());
        }
        let mut fingerprints = self
            .mutant_fingerprints
            .lock()
            .expect("lock mutant fingerprint map");
        match fingerprints.entry(fingerprint) {
            Entry::Occupied(entry) => Some(format!("mutant {:?}", entry.get())),
            Entry::Vacant(entry) => {
                entry.insert(mutant_name.to_owned());
                None
            }
        }
    }
}

/// Which packages to test
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestsForMutant {
    /// Test all packages in the workspace
    Workspace,
    /// Test only the package that was mutated
    Mutated,
    /// Test specific packages
    Explicit(Vec<Arc<Package>>),
}

impl TestsForMutant {
    fn new(options: &Options, workspace: &Workspace) -> Self {
        match options.test_package {
            TestPackages::Workspace => TestsForMutant::Workspace,
            TestPackages::Mutated => TestsForMutant::Mutated,
            TestPackages::Named(ref package_names) => {
                TestsForMutant::Explicit(workspace.packages_by_name(package_names))
            }
        }
    }
}

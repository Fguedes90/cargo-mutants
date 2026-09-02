// Copyright 2021-2025 Martin Pool

//! Run Cargo as a subprocess, including timeouts and propagating signals.

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::env;
use std::iter::once;
use std::time::{Duration, Instant};

use nextest_metadata::NextestExitCode;
use tracing::{debug, debug_span, warn};

use crate::Result;
use crate::build_dir::BuildDir;
use crate::console::Console;
use crate::interrupt::check_interrupted;
use crate::options::{Options, TestTool};
use crate::outcome::{Phase, PhaseResult};
use crate::output::ScenarioOutput;
use crate::package::PackageSelection;
use crate::process::{Exit, Process};

// Allowed nextest codes (those will be considered a mutation caught / ignored without a warning)
const NEXTEST_ALLOWED_CODES: &[i32] = &[
    NextestExitCode::NO_TESTS_RUN,
    NextestExitCode::TEST_RUN_FAILED,
    NextestExitCode::BUILD_FAILED,
];

/// Run cargo build, check, or test.
///
/// `extra_env` is appended to the child's environment after every other
/// variable this function sets, so a later duplicate key (e.g. a caller
/// overriding `CARGO_ENCODED_RUSTFLAGS`) wins.
#[allow(clippy::too_many_arguments)] // I agree it's a lot but I'm not sure wrapping in a struct would be better.
pub fn run_cargo(
    build_dir: &BuildDir,
    jobserver: Option<&jobserver::Client>,
    packages: &PackageSelection,
    phase: Phase,
    timeout: Option<Duration>,
    scenario_output: &mut ScenarioOutput,
    options: &Options,
    extra_env: &[(&str, &str)],
    console: &Console,
) -> Result<PhaseResult> {
    let _span = debug_span!("run", ?phase).entered();
    let start = Instant::now();
    let argv = cargo_argv(packages, phase, options);
    let mut env = vec![
        // The tests might use Insta <https://insta.rs>, and we don't want it to write
        // updates to the source tree, and we *certainly* don't want it to write
        // updates and then let the test pass.
        ("INSTA_UPDATE".to_owned(), "no".to_owned()),
        ("INSTA_FORCE_PASS".to_owned(), "0".to_owned()),
    ];
    if let Some(encoded_rustflags) = encoded_rustflags(options) {
        debug!(?encoded_rustflags);
        env.push(("CARGO_ENCODED_RUSTFLAGS".to_owned(), encoded_rustflags));
    }
    env.extend(
        extra_env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    );
    let process_status = Process::run(
        &argv,
        &env,
        build_dir.path(),
        timeout,
        jobserver,
        scenario_output,
        console,
    )?;
    check_interrupted()?;
    debug!(?process_status, elapsed = ?start.elapsed());
    if let Exit::Failure(code) = process_status
        && argv[1] == "nextest"
        && !NEXTEST_ALLOWED_CODES.contains(&code)
    {
        // Nextest returns detailed exit codes. I think we should still treat any non-zero result as
        // just an error, but we can at least warn if it's unexpected.
        warn!(%code, "nextest process exited with unexpected code (allowed: {NEXTEST_ALLOWED_CODES:?})");
    }
    Ok(PhaseResult {
        phase,
        duration: start.elapsed(),
        process_status,
        argv,
    })
}

/// Return the name of the cargo binary.
pub fn cargo_bin() -> String {
    // When run as a Cargo subcommand, which is the usual/intended case,
    // $CARGO tells us the right way to call back into it, so that we get
    // the matching toolchain etc.
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// Make up the argv for a cargo check/build/test invocation, including argv[0] as the
/// cargo binary itself.
// (This is split out so it's easier to test.)
pub(crate) fn cargo_argv(
    packages: &PackageSelection,
    phase: Phase,
    options: &Options,
) -> Vec<String> {
    let mut cargo_args = vec![cargo_bin()];
    match phase {
        Phase::Test => match &options.test_tool() {
            TestTool::Cargo => {
                cargo_args.push("test".to_string());
                if options.detect_equivalent_mutants || options.skip_uncovered {
                    // Both `--detect-equivalent-mutants` and
                    // `--skip-uncovered` infer a mutant's outcome from a
                    // signal that doc tests never contribute to:
                    // equivalent-mutant detection compares the compiled
                    // test executables byte for byte (see `artifacts.rs`),
                    // which only covers the binaries built by `cargo test
                    // --no-run`; `--skip-uncovered` reads coverage counters
                    // from `-Cinstrument-coverage` via RUSTFLAGS, which
                    // rustdoc doesn't honour. Either way, a function
                    // exercised only by a doc test looks exactly like dead
                    // code to the signal being used, so a mutant there
                    // could be wrongly judged equivalent/uncovered even
                    // though a doc test would actually have caught it. Fix
                    // that by restricting the run to the same targets the
                    // signal actually covers, so the signal is a sound
                    // proxy for everything this run can catch.
                    // `--lib --bins --tests` is the narrowest selection
                    // that still runs everything a plain `cargo test`
                    // would (lib tests, binary tests, and integration
                    // tests), while excluding doc tests; `--all-targets`
                    // also excludes doc tests but additionally pulls in
                    // benches and examples that a plain `cargo test`
                    // wouldn't run, so it's not the narrowest choice.
                    cargo_args.push("--lib".to_string());
                    cargo_args.push("--bins".to_string());
                    cargo_args.push("--tests".to_string());
                }
            }
            TestTool::Nextest => {
                cargo_args.push("nextest".to_string());
                cargo_args.push("run".to_string());
            }
        },
        Phase::Build => {
            match &options.test_tool() {
                TestTool::Cargo => {
                    // These invocations default to the test profile, and might
                    // have other differences? Generally we want to do everything
                    // to make the tests build, but not actually run them.
                    // See <https://github.com/sourcefrog/cargo-mutants/issues/237>.
                    cargo_args.push("test".to_string());
                    cargo_args.push("--no-run".to_string());
                }
                TestTool::Nextest => {
                    cargo_args.push("nextest".to_string());
                    cargo_args.push("run".to_string());
                    cargo_args.push("--no-run".to_string());
                }
            }
        }
        Phase::Check => {
            cargo_args.push("check".to_string());
            cargo_args.push("--tests".to_string());
        }
    }
    if let Some(profile) = &options.profile {
        match options.test_tool() {
            TestTool::Cargo => {
                cargo_args.push(format!("--profile={profile}"));
            }
            TestTool::Nextest => {
                cargo_args.push(format!("--cargo-profile={profile}"));
            }
        }
    }
    cargo_args.push("--verbose".to_string());
    match packages {
        PackageSelection::All => {
            cargo_args.push("--workspace".to_string());
        }
        PackageSelection::Explicit(packages) => {
            cargo_args.extend(
                packages
                    .iter()
                    .map(|p| format!("--package={}", p.version_qualified_name())),
            );
        }
    }
    if options.no_default_features {
        cargo_args.push("--no-default-features".to_owned());
    }
    if options.all_features {
        cargo_args.push("--all-features".to_owned());
    }
    // N.B. it can make sense to have --all-features and also explicit features from non-default packages.
    cargo_args.extend(options.features.iter().map(|f| format!("--features={f}")));
    cargo_args.extend(options.additional_cargo_args.iter().cloned());
    if phase == Phase::Test {
        cargo_args.extend(options.additional_cargo_test_args.iter().cloned());
    }
    cargo_args
}

/// Return adjusted `CARGO_ENCODED_RUSTFLAGS`, including any changes to cap-lints.
///
/// It seems we have to set this in the environment because Cargo doesn't expose
/// a way to pass it in as an option from all commands?
///
/// This does not currently read config files; it's too complicated.
///
/// See <https://doc.rust-lang.org/cargo/reference/environment-variables.html>
/// <https://doc.rust-lang.org/rustc/lints/levels.html#capping-lints>
pub(crate) fn encoded_rustflags(options: &Options) -> Option<String> {
    let separator = "\x1f";
    let mut extra_args: Vec<&str> = Vec::new();
    if options.cap_lints {
        extra_args.push("--cap-lints=warn");
    }
    if options.detect_equivalent_mutants {
        // Equivalent-mutant detection (Trivial Compiler Equivalence) compares
        // build artifacts byte for byte. Embedded debug info records source
        // line numbers, which necessarily move when a mutation is applied
        // even if the generated code is identical, so debug info must be
        // turned off for the comparison to be meaningful. See `artifacts.rs`.
        extra_args.push("-Cdebuginfo=0");
    }
    if extra_args.is_empty() {
        return None;
    }
    if let Ok(encoded) = env::var("CARGO_ENCODED_RUSTFLAGS") {
        if encoded.is_empty() {
            Some(extra_args.join(separator))
        } else {
            Some(
                once(encoded.as_str())
                    .chain(extra_args)
                    .collect::<Vec<&str>>()
                    .join(separator),
            )
        }
    } else if let Ok(rustflags) = env::var("RUSTFLAGS") {
        if rustflags.is_empty() {
            Some(extra_args.join(separator))
        } else {
            Some(
                rustflags
                    .split(' ')
                    .filter(|s| !s.is_empty())
                    .chain(extra_args)
                    .collect::<Vec<&str>>()
                    .join(separator),
            )
        }
    } else {
        Some(extra_args.join(separator))
    }
}

#[cfg(test)]
mod test {
    use clap::Parser;
    use pretty_assertions::assert_eq;
    use rusty_fork::rusty_fork_test;

    use crate::{
        Args,
        test_util::{single_threaded_remove_env_var, single_threaded_set_env_var},
    };

    use super::*;

    #[test]
    fn generate_cargo_args_for_baseline_with_default_options() {
        let options = Options::default();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            ["check", "--tests", "--verbose", "--workspace"]
        );
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Build, &options)[1..],
            ["test", "--no-run", "--verbose", "--workspace"]
        );
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Test, &options)[1..],
            ["test", "--verbose", "--workspace"]
        );
    }

    #[test]
    fn generate_cargo_args_with_additional_cargo_test_args_and_package() {
        let mut options = Options::default();
        options
            .additional_cargo_test_args
            .extend(["--lib", "--no-fail-fast"].iter().map(ToString::to_string));
        assert_eq!(
            cargo_argv(
                &PackageSelection::one(
                    "cargo-mutants-testdata-something",
                    "0.1.0",
                    "",
                    "src/lib.rs"
                ),
                Phase::Check,
                &options
            )[1..],
            [
                "check",
                "--tests",
                "--verbose",
                "--package=cargo-mutants-testdata-something@0.1.0",
            ]
        );
    }

    #[test]
    fn generate_cargo_args_with_additional_cargo_args_and_test_args() {
        let mut options = Options::default();
        options
            .additional_cargo_test_args
            .extend(["--lib", "--no-fail-fast"].iter().map(|&s| s.to_string()));
        options
            .additional_cargo_args
            .extend(["--release".to_owned()]);
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            ["check", "--tests", "--verbose", "--workspace", "--release"]
        );
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Build, &options)[1..],
            ["test", "--no-run", "--verbose", "--workspace", "--release"]
        );
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Test, &options)[1..],
            [
                "test",
                "--verbose",
                "--workspace",
                "--release",
                "--lib",
                "--no-fail-fast"
            ]
        );
    }

    #[test]
    fn no_default_features_args_passed_to_cargo() {
        let args = Args::try_parse_from(["mutants", "--no-default-features"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            [
                "check",
                "--tests",
                "--verbose",
                "--workspace",
                "--no-default-features"
            ]
        );
    }

    #[test]
    fn all_features_args_passed_to_cargo() {
        let args = Args::try_parse_from(["mutants", "--all-features"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            [
                "check",
                "--tests",
                "--verbose",
                "--workspace",
                "--all-features"
            ]
        );
    }

    #[test]
    fn cap_lints_passed_to_cargo() {
        let args = Args::try_parse_from(["mutants", "--cap-lints=true"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            ["check", "--tests", "--verbose", "--workspace",]
        );
    }

    #[test]
    fn feature_args_passed_to_cargo() {
        let args = Args::try_parse_from(
            ["mutants", "--features", "foo", "--features", "bar,baz"].as_slice(),
        )
        .unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            [
                "check",
                "--tests",
                "--verbose",
                "--workspace",
                "--features=foo",
                "--features=bar,baz"
            ]
        );
    }

    #[test]
    fn profile_arg_passed_to_cargo() {
        let args = Args::try_parse_from(["mutants", "--profile", "mutants"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Check, &options)[1..],
            [
                "check",
                "--tests",
                "--profile=mutants",
                "--verbose",
                "--workspace",
            ]
        );
    }

    #[test]
    fn nextest_gets_special_cargo_profile_option() {
        let args = Args::try_parse_from(
            ["mutants", "--test-tool=nextest", "--profile", "mutants"].as_slice(),
        )
        .unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Build, &options)[1..],
            [
                "nextest",
                "run",
                "--no-run",
                "--cargo-profile=mutants",
                "--verbose",
                "--workspace",
            ]
        );
    }

    #[test]
    fn detect_equivalent_mutants_excludes_doc_tests_from_the_cargo_test_invocation() {
        let args =
            Args::try_parse_from(["mutants", "--detect-equivalent-mutants"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Test, &options)[1..],
            [
                "test",
                "--lib",
                "--bins",
                "--tests",
                "--verbose",
                "--workspace"
            ]
        );
    }

    #[test]
    fn skip_uncovered_excludes_doc_tests_from_the_cargo_test_invocation() {
        let args = Args::try_parse_from(["mutants", "--skip-uncovered"].as_slice()).unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Test, &options)[1..],
            [
                "test",
                "--lib",
                "--bins",
                "--tests",
                "--verbose",
                "--workspace"
            ]
        );
    }

    #[test]
    fn without_detect_equivalent_mutants_or_skip_uncovered_doc_tests_are_not_excluded() {
        let options = Options::default();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Test, &options)[1..],
            ["test", "--verbose", "--workspace"]
        );
    }

    #[test]
    fn detect_equivalent_mutants_does_not_change_the_nextest_invocation() {
        let args = Args::try_parse_from(
            [
                "mutants",
                "--test-tool=nextest",
                "--detect-equivalent-mutants",
            ]
            .as_slice(),
        )
        .unwrap();
        let options = Options::from_args(&args).unwrap();
        assert_eq!(
            cargo_argv(&PackageSelection::All, Phase::Test, &options)[1..],
            ["nextest", "run", "--verbose", "--workspace"]
        );
    }

    rusty_fork_test! {
        #[test]
        fn rustflags_without_cap_lints_and_no_environment_variables() {
            single_threaded_remove_env_var("RUSTFLAGS");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            assert_eq!(
                encoded_rustflags(&Options {
                    ..Default::default()
                }),
                None
            );
        }
        #[test]
        fn rustflags_with_cap_lints_and_no_environment_variables() {
            single_threaded_remove_env_var("RUSTFLAGS");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            assert_eq!(
                encoded_rustflags(&Options {
                    cap_lints: true,
                    ..Default::default()
                }),
                Some("--cap-lints=warn".into())
            );
        }

        // Don't generate an empty argument if the encoded rustflags is empty.
        #[test]
        fn rustflags_with_empty_encoded_rustflags() {
            single_threaded_set_env_var("CARGO_ENCODED_RUSTFLAGS", "");
            assert_eq!(
                encoded_rustflags(&Options {
                    cap_lints: true,
                    ..Default::default()
                }).unwrap(),
                "--cap-lints=warn"
            );
        }

        #[test]
        fn rustflags_added_to_existing_encoded_rustflags() {
            single_threaded_set_env_var("RUSTFLAGS", "--something\x1f--else");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            let options = Options {
                cap_lints: true,
                ..Default::default()
            };
            assert_eq!(encoded_rustflags(&options).unwrap(), "--something\x1f--else\x1f--cap-lints=warn");
        }

        #[test]
        fn rustflags_added_to_existing_rustflags() {
            single_threaded_set_env_var("RUSTFLAGS", "-Dwarnings");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            assert_eq!(encoded_rustflags(&Options {
                cap_lints: true,
                ..Default::default()
            }).unwrap(), "-Dwarnings\x1f--cap-lints=warn");
        }

        #[test]
        fn detect_equivalent_mutants_adds_debuginfo_0_rustflag() {
            single_threaded_remove_env_var("RUSTFLAGS");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            assert_eq!(
                encoded_rustflags(&Options {
                    detect_equivalent_mutants: true,
                    ..Default::default()
                }),
                Some("-Cdebuginfo=0".into())
            );
        }

        #[test]
        fn debuginfo_flag_is_absent_when_detect_equivalent_mutants_is_off() {
            single_threaded_remove_env_var("RUSTFLAGS");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            assert_eq!(
                encoded_rustflags(&Options {
                    detect_equivalent_mutants: false,
                    ..Default::default()
                }),
                None
            );
        }

        #[test]
        fn detect_equivalent_mutants_and_cap_lints_are_both_present() {
            single_threaded_remove_env_var("RUSTFLAGS");
            single_threaded_remove_env_var("CARGO_ENCODED_RUSTFLAGS");
            assert_eq!(
                encoded_rustflags(&Options {
                    cap_lints: true,
                    detect_equivalent_mutants: true,
                    ..Default::default()
                }),
                Some("--cap-lints=warn\x1f-Cdebuginfo=0".into())
            );
        }
    }
}

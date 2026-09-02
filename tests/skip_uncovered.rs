// Copyright 2026 Martin Pool

//! Integration tests for `--skip-uncovered`.

use predicates::prelude::*;

mod integration_util;
mod util;
use integration_util::run;
use util::{OUTER_TIMEOUT, copy_of_testdata, outcome_json_counts};

/// With `--skip-uncovered`, mutants in a function that no test ever calls
/// are reported `uncovered` and listed in `mutants.out/uncovered.txt`,
/// mutants in a function that a test does call are still built and caught,
/// and mutants in a `const` initializer that a test asserts on are also
/// caught, not skipped, even though the initializer runs at compile time
/// and so never trips a coverage counter. Because there's an uncovered
/// mutant, the exit code is `FoundProblems`, exactly as it would be for a
/// missed mutant.
#[test]
fn skip_uncovered_reports_dead_code_as_uncovered_and_still_catches_covered_and_const_mutants() {
    let tmp_src_dir = copy_of_testdata("skip_uncovered");
    run()
        .arg("mutants")
        .args(["--skip-uncovered", "--no-times", "--no-shuffle"])
        .arg("-d")
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .code(2) // exit_code::FoundProblems
        .stdout(
            predicate::str::contains("7 caught")
                .and(predicate::str::contains("5 uncovered"))
                .and(predicate::str::contains("UNCOVERED src/dead.rs"))
                .and(predicate::str::contains("src/const_item.rs").not()),
        );

    let counts = outcome_json_counts(&tmp_src_dir);
    assert_eq!(counts["total_mutants"], 12);
    assert_eq!(counts["caught"], 7);
    assert_eq!(counts["uncovered"], 5);
    assert_eq!(counts["missed"], 0);

    let uncovered_txt =
        std::fs::read_to_string(tmp_src_dir.path().join("mutants.out/uncovered.txt"))
            .expect("read uncovered.txt");
    assert_eq!(uncovered_txt.lines().count(), 5);
    assert!(
        uncovered_txt.lines().all(|line| line.contains("dead.rs")),
        "every uncovered mutant should be in dead.rs, not const_item.rs or covered.rs: {uncovered_txt}"
    );
}

/// Without the flag, the same tree behaves exactly as it always did: the
/// dead-code mutants are just ordinary missed mutants (nothing exercises
/// them, so the tests still pass either way), and `uncovered.txt` is empty.
#[test]
fn without_skip_uncovered_the_same_tree_reports_missed_as_usual() {
    let tmp_src_dir = copy_of_testdata("skip_uncovered");
    run()
        .arg("mutants")
        .args(["--no-times", "--no-shuffle"])
        .arg("-d")
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .code(2) // exit_code::FoundProblems
        .stdout(predicate::str::contains("7 caught").and(predicate::str::contains("5 missed")));

    let counts = outcome_json_counts(&tmp_src_dir);
    assert_eq!(counts["missed"], 5);
    assert_eq!(counts["uncovered"], 0);
    assert_eq!(counts["caught"], 7);

    // The list file always exists (like caught.txt/missed.txt/etc.), but is
    // empty when the feature is off.
    let uncovered_txt =
        std::fs::read_to_string(tmp_src_dir.path().join("mutants.out/uncovered.txt"))
            .expect("read uncovered.txt");
    assert_eq!(uncovered_txt, "");
}

/// `--skip-uncovered` measures coverage during the baseline test run, so it
/// can't be combined with `--baseline=skip`: there would be no baseline run
/// to measure coverage from. cargo-mutants rejects the combination up front
/// with a clear message, before building anything.
#[test]
fn skip_uncovered_is_rejected_with_baseline_skip() {
    let tmp_src_dir = copy_of_testdata("skip_uncovered");
    run()
        .arg("mutants")
        .args(["--skip-uncovered", "--baseline=skip", "--no-times"])
        .arg("-d")
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("--skip-uncovered")
                .and(predicate::str::contains("--baseline=skip")),
        );
}

/// `--skip-uncovered` measures coverage during the baseline test run, so it
/// can't be combined with `--check`, which never runs tests at all: there
/// would be nothing to measure coverage from. cargo-mutants rejects the
/// combination up front with a clear message.
#[test]
fn skip_uncovered_is_rejected_with_check() {
    let tmp_src_dir = copy_of_testdata("skip_uncovered");
    run()
        .arg("mutants")
        .args(["--skip-uncovered", "--check", "--no-times"])
        .arg("-d")
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("--skip-uncovered").and(predicate::str::contains("--check")),
        );
}

// Copyright 2026 Martin Pool

//! Integration tests for `--detect-equivalent-mutants`.

use predicates::prelude::*;

mod integration_util;
mod util;
use integration_util::run;
use util::{OUTER_TIMEOUT, copy_of_testdata, outcome_json, outcome_json_counts};

/// With `--detect-equivalent-mutants`, every mutant in the `equivalent_mutants`
/// testdata tree is recognized as equivalent to the baseline (its mutated
/// function is never called, so the mutation can't change the compiled test
/// binary), and none of them count as a found problem.
#[test]
fn equivalent_mutants_are_detected_and_excluded_from_missed() {
    let tmp_src_dir = copy_of_testdata("equivalent_mutants");
    run()
        .arg("mutants")
        .args(["--detect-equivalent-mutants", "--no-times", "--no-shuffle"])
        .arg("-d")
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("equivalent").and(predicate::str::contains("5 equivalent")),
        );

    let counts = outcome_json_counts(&tmp_src_dir);
    assert_eq!(counts["total_mutants"], 5);
    assert_eq!(counts["equivalent"], 5);
    assert_eq!(counts["missed"], 0);
    assert_eq!(counts["caught"], 0);

    let equivalent_txt =
        std::fs::read_to_string(tmp_src_dir.path().join("mutants.out/equivalent.txt"))
            .expect("read equivalent.txt");
    assert_eq!(equivalent_txt.lines().count(), 5);
    assert!(equivalent_txt.contains("never_called"));
}

/// Without the flag, the same mutants are just reported as ordinary missed
/// mutants (nothing exercises the mutated function, so the tests still pass):
/// the flag is what turns this into a `FoundProblems` exit code, not into
/// success, and it's inert on `--list`.
#[test]
fn without_the_flag_the_same_mutants_are_reported_missed_as_usual() {
    let tmp_src_dir = copy_of_testdata("equivalent_mutants");
    run()
        .arg("mutants")
        .args(["--no-times", "--no-shuffle"])
        .arg("-d")
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .code(2) // exit_code::FoundProblems
        .stdout(predicate::str::contains("5 missed"));

    let counts = outcome_json_counts(&tmp_src_dir);
    assert_eq!(counts["missed"], 5);
    assert_eq!(counts["equivalent"], 0);

    // The list file always exists (like caught.txt/missed.txt/etc.), but is
    // empty when the feature is off.
    let equivalent_txt =
        std::fs::read_to_string(tmp_src_dir.path().join("mutants.out/equivalent.txt"))
            .expect("read equivalent.txt");
    assert_eq!(equivalent_txt, "");
}

/// `--list --json` output doesn't depend on `--detect-equivalent-mutants` at
/// all: it never builds or compares anything, so the flag must be exactly
/// inert there.
#[test]
fn detect_equivalent_mutants_is_inert_for_list_json() {
    let tmp_src_dir = copy_of_testdata("equivalent_mutants");
    let without_flag = run()
        .args(["mutants", "--list", "--json", "-d"])
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let with_flag = run()
        .args([
            "mutants",
            "--list",
            "--json",
            "--detect-equivalent-mutants",
            "-d",
        ])
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(without_flag, with_flag);
}

/// Without a baseline build to compare against (`--baseline=skip`), the first
/// mutant of a given build-artifact "shape" can't be recognized as anything
/// yet and is actually tested (and, since nothing exercises it, missed); but
/// every later mutant whose build matches it is still recognized as
/// equivalent, and the log names the earlier mutant it matches.
#[test]
fn redundant_mutants_are_detected_against_an_earlier_mutant_without_a_baseline() {
    let tmp_src_dir = copy_of_testdata("equivalent_mutants");
    run()
        .arg("mutants")
        .args([
            "--detect-equivalent-mutants",
            "--baseline=skip",
            "--no-times",
            "--no-shuffle",
            "--timeout=20",
        ])
        .arg("-d")
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .code(2) // exit_code::FoundProblems, from the one missed mutant
        .stdout(predicate::str::contains("1 missed").and(predicate::str::contains("4 equivalent")));

    let counts = outcome_json_counts(&tmp_src_dir);
    assert_eq!(counts["missed"], 1);
    assert_eq!(counts["equivalent"], 4);

    let log_dir = tmp_src_dir.path().join("mutants.out/log");
    let matched_an_earlier_mutant = std::fs::read_dir(&log_dir)
        .expect("read log dir")
        .filter_map(Result::ok)
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
        .any(|log| log.contains("build artifacts are identical to mutant"));
    assert!(
        matched_an_earlier_mutant,
        "expected at least one log to record which earlier mutant it matched"
    );
}

/// A mutation of code exercised only by a doc test (`double_string` in
/// `testdata/well_tested`, whose only test is the `///` example above its
/// declaration) is not falsely reported `equivalent`. Before doc tests were
/// excluded from the `cargo test` invocation this feature runs, one of
/// `double_string`'s sibling mutants could get lucky: its test phase
/// actually ran (and its doc test caught it), while a build-identical
/// sibling's test phase was skipped as "redundant" with it and reported
/// `equivalent` -- a false claim, since the doc test that caught the first
/// one would have caught the second too, had it been allowed to run. Now
/// that doc tests never run in this mode, nothing in the run can catch
/// `double_string`'s mutants at all, so the first one is honestly reported
/// `missed` (a mutant with no test coverage in this run), not
/// `equivalent` (a mutant no test could *ever* catch).
#[test]
fn a_mutant_that_only_a_doc_test_would_catch_is_not_reported_equivalent() {
    let tmp_src_dir = copy_of_testdata("well_tested");
    run()
        .arg("mutants")
        .args([
            "--detect-equivalent-mutants",
            "--no-times",
            "--no-shuffle",
            "--file",
            "src/simple_fns.rs",
        ])
        .arg("-d")
        .arg(tmp_src_dir.path())
        .timeout(OUTER_TIMEOUT)
        .assert()
        .code(2); // exit_code::FoundProblems: the doc-test-only mutation is missed

    let outcomes = outcome_json(&tmp_src_dir);
    let double_string_outcomes: Vec<&serde_json::Value> = outcomes["outcomes"]
        .as_array()
        .expect("outcomes is an array")
        .iter()
        .filter(|o| {
            o["scenario"]["Mutant"]["name"]
                .as_str()
                .is_some_and(|name| name.contains("double_string"))
        })
        .collect();
    assert_eq!(
        double_string_outcomes.len(),
        2,
        "expected both double_string mutants in the outcomes"
    );
    assert!(
        double_string_outcomes
            .iter()
            .any(|o| o["summary"] == "MissedMutant"),
        "expected at least one double_string mutant to be honestly reported missed \
         (no test in this run exercises it, since doc tests are excluded): {double_string_outcomes:#?}"
    );
    for outcome in &double_string_outcomes {
        for phase_result in outcome["phase_results"]
            .as_array()
            .expect("phase_results array")
        {
            if phase_result["phase"] == "Test" {
                let argv: Vec<&str> = phase_result["argv"]
                    .as_array()
                    .expect("argv array")
                    .iter()
                    .map(|v| v.as_str().expect("argv entries are strings"))
                    .collect();
                assert!(
                    argv.contains(&"--lib")
                        && argv.contains(&"--bins")
                        && argv.contains(&"--tests"),
                    "test phase for a double_string mutant should exclude doc tests: {argv:?}"
                );
            }
        }
    }
}

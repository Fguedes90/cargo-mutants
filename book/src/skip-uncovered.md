# Skipping uncovered mutants

`--skip-uncovered` avoids building and testing mutants in code that the
test suite never runs at all: if no test executes a line, no test can
possibly catch a mutation there, so the outcome is known before spending a
build and a test run on it.

To find out which lines are executed, cargo-mutants instruments the
**baseline** test run (the one unmutated run it makes before testing any
mutant) with `-Cinstrument-coverage`, and reads the resulting coverage
counters with `llvm-profdata` and `llvm-cov`. Mutants placed on a line that
the baseline run never executed are reported `uncovered` instead of
`missed`, and skip both the build and the test phase.

## Needs the `llvm-tools` rustup component

Reading coverage counters requires `llvm-profdata` and `llvm-cov`, which
ship as part of rustup's `llvm-tools` component, not with `cargo` itself.
cargo-mutants checks for them before building anything, so a missing
component fails immediately with:

```
llvm-profdata not found: --skip-uncovered needs the llvm-tools component, so run `rustup component add llvm-tools`
```

## Baseline timings include instrumentation overhead

Because the baseline run is also the coverage-measuring run, it carries the
overhead of `-Cinstrument-coverage`. cargo-mutants derives its default
timeouts from how long the baseline took, so with `--skip-uncovered` those
timeouts (and the reported baseline duration) are a little longer than an
uninstrumented run of the same tests would take. This errs on the side of
generous timeouts, not tight ones.

## `uncovered` counts as a found problem

An uncovered mutant is not a false positive to be filtered out the way
[`--detect-equivalent-mutants`](equivalent-mutants.md) filters equivalent
mutants: it's a real gap in test coverage, exactly like a missed mutant, so
it's still something you should look at. `uncovered` mutants are counted
separately in the summary line, listed in `mutants.out/uncovered.txt`
(alongside `caught.txt`/`missed.txt`/etc.), and count toward the
[`FoundProblems`](exit-codes.md) exit code just as missed mutants do.

## Doc tests are not run in this mode

`-Cinstrument-coverage` reaches code compiled by `rustc`, but not code
compiled and run separately by `rustdoc` for doc tests: doc-test execution
is invisible to coverage instrumentation. If cargo-mutants ran doc tests
anyway while measuring coverage, a line exercised only by a doc test would
look uncovered even though a test does in fact catch mutations there --
a false positive.

To keep the `uncovered` verdict trustworthy ("no test in this run executed
this line" being exactly true), cargo-mutants excludes doc tests from the
test run whenever `--skip-uncovered` is set, by passing `--lib --bins
--tests` to `cargo test`. The consequence is that, in this mode, doc tests
don't run at all: a function exercised only by a doc test is reported
`uncovered`, not caught.

For example, `testdata/well_tested`'s `double_string` function is only
exercised by a doc test:

```rust
/// Return `s` repeated twice.
///
/// ```
/// assert_eq!(cargo_mutants_testdata_well_tested::simple_fns::double_string("cat"), "catcat");
/// ```
pub fn double_string(s: &str) -> String {
    let mut r = s.to_owned();
    r.push_str(s);
    r
}
```

Running `cargo mutants --skip-uncovered` on that tree reports both of
`double_string`'s mutants as `uncovered`, even though a plain `cargo
mutants` run (which does run the doc test) catches both of them. If you
rely on doc tests to exercise some code path, expect it to show up as
`uncovered` here; that's a known blind spot of this mode, not a bug.

## Compile-time-evaluated code is always tested

Some code runs during compilation, not during the test run, so it never
increments a coverage counter even when a test does depend on its result:
`const` and `static` initializers, `const fn` bodies, array-length
expressions, and const generic arguments. A test asserting on such a
constant catches a mutation of it, so treating those lines as uncovered
would be simply wrong.

cargo-mutants marks mutants in those positions during discovery and never
skips them for coverage: they are built and tested as usual. For example,
the three mutants in `testdata/well_tested`'s `src/static_item.rs:1`
initializer are reported `caught` with `--skip-uncovered`, exactly as they
are without it.

## Incompatible with `--baseline=skip` and `--check`

`--skip-uncovered` measures coverage during the baseline test run, so it
can't be combined with:

* [`--baseline=skip`](baseline.md), which skips the baseline entirely —
  there would be no run to measure coverage from.
* `--check`, which only runs `cargo check` and never runs tests — there
  would be nothing to measure coverage from either.

Both combinations are rejected up front with an error, before any building
starts.

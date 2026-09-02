# Detecting equivalent mutants

An "equivalent mutant" is a mutation that changes the source code but cannot
possibly be caught by any test, because it makes no difference to the
compiled program: the mutated code either isn't reachable from anything the
test suite calls, or it changes a value in a way that no observable behavior
depends on. Equivalent mutants are not a real gap in your tests, but by
default cargo-mutants can't tell them apart from genuinely missed mutants,
so they show up as false-positive `MISSED` results.

`--detect-equivalent-mutants` finds some equivalent mutants automatically,
using a technique called [Trivial Compiler
Equivalence](https://dl.acm.org/doi/10.1109/ICSE.2015.103) (Papadakis, Jia,
Harman & Le Traon, ICSE 2015): after building a mutant, cargo-mutants
compares its compiled test binary, byte for byte, against the binary built
for the unmutated baseline, and also against the binaries already built for
other mutants in this run.

- If a mutant's binary is identical to the baseline's, that mutant is
  **equivalent**: no test could distinguish it from unmutated code, so its
  test phase is skipped entirely.
- If a mutant's binary is identical to an earlier mutant's binary (but not
  the baseline's), it is **redundant** with that earlier mutant: whatever
  the earlier mutant's outcome turns out to be, this one will be identical,
  so its test phase is skipped too. cargo-mutants still runs the tests for
  the first mutant in each such group.

Mutants recognized this way are reported as `equivalent`, counted
separately in the summary line, listed in `mutants.out/equivalent.txt`
(alongside the existing `caught.txt`/`missed.txt`/etc.), and are **excluded
from the found-problems count**: they are neither `caught` nor `missed`,
exactly as Trivial Compiler Equivalence removes equivalent mutants from the
denominator of the mutation score.

## It forces debug info off

Byte-for-byte comparison only works if two builds of identical (or
behaviorally identical) source produce identical bytes. In practice, with
Rust's default debug build, they don't: the debug info embedded in the
binary records source line numbers, and mutating a line moves that
metadata even when it doesn't change the generated code. So whenever
`--detect-equivalent-mutants` is set, cargo-mutants adds `-Cdebuginfo=0` to
the build (on top of any `RUSTFLAGS` you've configured). This is
unconditional and can't be turned off separately: without it, the
comparison would never match and the option would silently find nothing.

One effect of this is that backtraces from a crashing test, if you inspect
them by hand outside cargo-mutants, will have less source-line detail
during a `--detect-equivalent-mutants` run.

## How much this actually saves

The yield depends heavily on your code and your build profile. Measured on
`testdata/well_tested` in this repository (101 mutants, one machine, one
run — treat this as illustrative, not a guarantee for your project):

- At the default (unoptimized) profile, 0 mutants were found equivalent to
  the baseline directly, and 1 was found redundant with an earlier mutant
  — 1% of the test phases skipped. (Two further mutants of a `static`
  initializer were found equivalent to the baseline directly, for 3
  `equivalent` mutants in total out of 101.)
- Built with `-Copt-level=3`, the same 3 mutants were found equivalent or
  redundant as at the default profile, plus 14 more that only collapse
  onto identical generated code once the optimizer runs — 17 out of 101,
  about 17% of the test phases skipped.

Do not expect the ~28% equivalent-mutant rate reported in the original
Trivial Compiler Equivalence paper (which studied C/C++ code with a
different compiler and mutation set): for the kinds of mutations
cargo-mutants generates in Rust, byte-identical *redundant* mutants (two
different mutations that happen to compile to the same code) are more
common than mutants that are byte-identical to the unmutated *baseline*.
Optimizing builds (`--profile` set to something built with a higher
`opt-level`) increases the yield because the optimizer collapses more
distinct mutations onto the same generated code.

## Interaction with `--baseline=skip`

If you also pass [`--baseline=skip`](baseline.md), there is no baseline
build to compare against, so no mutant can be recognized as equivalent to
the baseline. Redundant-mutant detection between mutants still works: the
first mutant in each group of identically-compiling mutants is tested as
normal, and later ones in the same group are skipped as equivalent to it.

## Interaction with `--check`

`--check` only runs `cargo check`, never `cargo test`, so there is no test
phase for `--detect-equivalent-mutants` to skip. Passing both options
together has no effect beyond the wasted comparison work; cargo-mutants
prints a warning if you do.

## Doc tests aren't run in this mode

The fingerprint compares only the artifacts from `cargo test --no-run`
(the unit/integration test binaries built for the *build* phase). Doc
tests are compiled and run separately from source, so they never
contribute to that fingerprint: a mutant in code reachable *only* from a
doc test would look exactly as unreachable to the fingerprint as code
with no test coverage at all. Left alone, that's unsound, not just
imprecise: whether a mutant's test phase gets skipped depends on which
other mutant happened to build to the same bytes, so one mutant in a
"redundant" group could get its test phase actually run (and be honestly
caught by a doc test) while a build-identical sibling's test phase gets
skipped and is reported `equivalent` -- a false claim that no test could
ever have caught it, when a doc test demonstrably could and did, on its
sibling.

So, when the test tool is `cargo` (the default), `--detect-equivalent-mutants`
also runs `cargo test` with `--lib --bins --tests`, which excludes doc
tests. This makes the fingerprint sound again: doc tests contribute to
neither the signal being compared nor the actual test run it stands in
for, so nothing this mode reports can be contradicted by a doc test that
it silently skipped. The user-visible consequence is that a mutant only a
doc test would catch is now reported `missed` -- the same as any other
mutant with no test coverage in this run -- instead of `equivalent`. This
is more conservative but honest: it costs you the ability to rely on doc
tests while `--detect-equivalent-mutants` is set, but it never claims a
mutant is uncatchable when a test that ran (or, unmodified, would have
run) could catch it.

(`cargo nextest` doesn't run doc tests at all, so nothing changes for the
nextest test tool.)

## A residual limitation: tests that read the source tree

Excluding doc tests closes the gap between what's fingerprinted and what's
run for tests that only differ in *compiled* behavior. It can't help with
a test that distinguishes two build-identical binaries by reading
something other than the binary at run time -- for example a `trybuild`
test that recompiles a fixture `.rs` file, an `insta` snapshot of source
text, or a test that opens and inspects its own source file. Such a test
can catch a mutation whose compiled test binary is byte-identical to the
baseline or to another mutant, and `--detect-equivalent-mutants` has no
way to see that: it never inspects source or non-binary files, only the
compiled artifacts. If your test suite relies on this pattern, treat
`equivalent`/redundant-skipped results with the same caution as any other
`missed` result, and consider excluding the affected functions from
equivalent-mutant detection (or from mutation testing) rather than trusting
the `equivalent` label there.

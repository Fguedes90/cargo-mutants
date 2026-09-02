A tree demonstrating `--detect-equivalent-mutants` (Trivial Compiler
Equivalence): comparing a mutant's compiled test artifacts against the
baseline and against earlier mutants, byte for byte, to find mutants that no
test could possibly catch.

Everything below was confirmed empirically (not assumed), by hand-building
this tree with `RUSTFLAGS=-Cdebuginfo=0 cargo test --no-run` and comparing
the resulting test binaries, and then by running the real
`--detect-equivalent-mutants` CLI against it.

`unreachable::never_called` is a private function that nothing in this
crate ever calls, not even a test. Rust never generates code for it in the
test binary (there's no other crate that could call it), so every mutation
of its body produces a byte-identical test binary:

* Run normally (`cargo mutants --detect-equivalent-mutants`), every one of
  its mutants matches the *baseline* fingerprint directly and is reported
  `equivalent`.
* Run with `--baseline=skip` (so there's no baseline fingerprint to compare
  against), the first mutant of `never_called` doesn't match anything yet
  and is actually tested (it's `missed`, since nothing exercises it); every
  later mutant of the same dead code matches *that first mutant's*
  fingerprint and is reported `equivalent` (redundant) with it by name. This
  is the code path used when there's no baseline build to compare against.

### A pitfall found while building this tree, not reproduced here

An earlier version of this tree used a function only exercised by a doc
test (mirroring `testdata/well_tested`'s `double_string`), to demonstrate
two differently-mutated sibling replacements building to identical bytes.
That turned out to be a **false positive**, not a valid demonstration: doc
tests are not part of the `cargo test --no-run` build that
`--detect-equivalent-mutants` fingerprints (they're only compiled and run
by the full `cargo test` invocation, in the *test* phase, not the *build*
phase). A doc-test-only function looks exactly as "dead" to the fingerprint
as `never_called` above, so its mutants got reported equivalent/redundant
even when a doc test would actually have caught one of them -- confirmed
by re-running that mutant with `--baseline=skip`, which forced its test
phase to run and caught it.

That false positive is now fixed: when the test tool is `cargo`,
`--detect-equivalent-mutants` excludes doc tests from the actual `cargo
test` invocation, so a doc-test-only function has no test coverage in the
run at all and its mutants are honestly reported `missed`, not
`equivalent`. See "Doc tests aren't run in this mode" in
`book/src/equivalent-mutants.md`.

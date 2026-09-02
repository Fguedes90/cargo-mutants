A tree demonstrating `--skip-uncovered`: mutants in a line that no test
executes are reported `uncovered` and skip the build and test phases,
instead of being built and (mis)reported as `missed`.

Everything below was confirmed empirically (not assumed), by running the
real `cargo-mutants` binary against this tree, both without any flag and
with `--skip-uncovered`.

* `covered::add_one` is called directly by a unit test. Coverage
  instrumentation records a hit on its body, so none of its mutants are
  skipped: they are built, tested, and caught as usual, with or without
  `--skip-uncovered`.

* `dead::never_called` is a private-in-effect function that nothing in this
  crate ever calls, not even a test. No coverage counter for its body is
  ever incremented, so with `--skip-uncovered` every one of its mutants is
  reported `uncovered` without being built. Without the flag, the same
  mutants are reported `missed` (nothing exercises the function, so the
  unmutated behavior looks just as correct as the mutated one).

* `const_item::ANSWER` is a `const` initializer expression (`40 + 2`),
  checked by a test that asserts on its value. The initializer is evaluated
  by the compiler at compile time, so `-Cinstrument-coverage` never emits a
  counter for it — there's no "running" the expression as instrumented
  code. Even so, its mutants are genuinely catchable: `answer_is_forty_two`
  fails if the value changes. This is the regression case for the
  compile-time-evaluation gate that `--skip-uncovered` needs: without it,
  these mutants would be misreported `uncovered` even though a test in this
  very run does catch them.

Confirmed by running, from the root of a copy of this tree with
`Cargo_test.toml` renamed to `Cargo.toml`:

```
cargo mutants --no-times --no-shuffle
```

which reports 12 mutants tested: 7 caught (`covered::add_one` and
`const_item::ANSWER`), 5 missed (`dead::never_called`); and:

```
cargo mutants --skip-uncovered --no-times --no-shuffle
```

which reports the same 12 mutants: 7 caught (`covered::add_one` and
`const_item::ANSWER`), 5 uncovered (`dead::never_called`), and lists
exactly those 5 in `mutants.out/uncovered.txt`.

/// Not called anywhere in this crate, including from any test: the
/// coverage instrumentation never records a hit on its body, so with
/// `--skip-uncovered` every mutant generated here should be reported
/// `uncovered` instead of being built and tested.
#[allow(dead_code)]
pub fn never_called(x: i32) -> i32 {
    x + 1
}

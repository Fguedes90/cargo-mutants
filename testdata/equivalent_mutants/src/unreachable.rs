/// Not called anywhere in this crate, including from any test or doc test.
///
/// Rust never generates code for a private, unreferenced function in a
/// final test binary (there's no other crate that could possibly call it),
/// so every mutation of its body produces a byte-identical test binary to
/// the unmutated baseline. Every mutant generated here should be reported
/// `equivalent` by `--detect-equivalent-mutants`.
#[allow(dead_code)]
fn never_called(x: i32) -> i32 {
    x + 1
}

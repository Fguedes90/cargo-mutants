/// A `const` initializer expression, checked by a test below. It's
/// evaluated by the compiler at compile time, so no coverage counter is
/// ever emitted for the initializer: `-Cinstrument-coverage` only
/// instruments code that runs, and this expression never "runs" as
/// instrumented code at all. Its mutants must still be built and tested
/// (and caught by `answer_is_forty_two` below), not skipped as uncovered.
pub const ANSWER: i32 = 40 + 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_is_forty_two() {
        assert_eq!(ANSWER, 42);
    }
}

/// Exercised directly by a unit test below, so the coverage instrumentation
/// records a hit on every line of its body: none of its mutants should be
/// skipped as uncovered, and the test should catch every one of them.
pub fn add_one(x: i32) -> i32 {
    x + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_one_adds_one() {
        assert_eq!(add_one(1), 2);
    }
}

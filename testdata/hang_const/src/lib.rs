const fn should_stop_const() -> bool {
    true
}

/// If `should_stop_const` is mutated to return false, then this const block
/// will hang and block compilation.
///
/// Mutants inside the initializer are skipped: replacing the `if` condition
/// with `true` leaves `VAL` at 1, so no test can tell the difference, and this
/// tree exists only to exercise the mutation of the const fn itself.
#[mutants::skip]
pub const VAL: i32 = loop {
    if should_stop_const() {
        break 1;
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_const() {
        assert_eq!(VAL, 1);
    }
}

use cargo_mutants_testdata_relative_dependency::double_factorial;

#[test]
fn double_factorial_zero_is_2() {
    assert_eq!(double_factorial(0), 2);
}

#[test]
fn double_factorial_one_is_2() {
    assert_eq!(double_factorial(1), 2);
}

#[test]
fn double_factorial_two_is_4() {
    assert_eq!(double_factorial(2), 4);
}

#[test]
fn double_factorial_of_negative_is_0() {
    assert_eq!(double_factorial(-1), 0);
}

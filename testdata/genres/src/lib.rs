//! A tree where every executable mutation genre added for if conditions, enum
//! values, bool literals, statement deletion and return values lands, and where
//! the tests kill all of them.
//!
//! There is deliberately no `while` loop and no mutable loop counter, so that no
//! mutant can hang.

pub fn classify(n: i32) -> &'static str {
    if n > 10 { "big" } else { "small" }
}

#[derive(Debug, PartialEq)]
pub enum Colour {
    Red,
    Green,
}

pub fn colour_of(flag: bool) -> Colour {
    if flag { Colour::Red } else { Colour::Green }
}

pub fn is_enabled(override_off: bool) -> bool {
    let enabled = true;
    enabled && !override_off
}

pub fn push_twice(v: &mut Vec<u32>) {
    v.push(1);
    v.push(2);
}

pub fn early(n: i32) -> i32 {
    if n < 0 {
        return 0;
    }
    n * 2 + 1
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn classify_boundary() {
        assert_eq!(classify(11), "big");
        assert_eq!(classify(10), "small");
        assert_eq!(classify(-1), "small");
    }

    #[test]
    fn colours() {
        assert_eq!(colour_of(true), Colour::Red);
        assert_eq!(colour_of(false), Colour::Green);
    }

    #[test]
    fn enabled_unless_overridden() {
        assert_eq!(is_enabled(false), true);
        assert_eq!(is_enabled(true), false);
    }

    #[test]
    fn pushes_both() {
        let mut v = Vec::new();
        push_twice(&mut v);
        assert_eq!(v, [1, 2]);
    }

    #[test]
    fn early_returns_zero_for_negative() {
        assert_eq!(early(-5), 0);
        assert_eq!(early(0), 1);
        assert_eq!(early(3), 7);
    }
}

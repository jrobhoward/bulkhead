#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case(1, "p000001")]
#[case(42, "p000042")]
#[case(999_999, "p999999")]
fn key____post_number____fixed_width_with_the_prefix(#[case] n: u64, #[case] expected: &str) {
    let found = key(n);

    assert_eq!(found, expected);
}

#[test]
fn key____consecutive_numbers____sort_in_order() {
    let mut keys = vec![key(10), key(9), key(100)];

    keys.sort();

    assert_eq!(keys, [key(9), key(10), key(100)]);
}

#[rstest]
#[case("said hello there", Command::Said("hello there"))]
#[case("said ", Command::Said(""))]
#[case("clear", Command::Clear)]
#[case("recent", Command::Recent)]
#[case("hello", Command::Unknown)]
fn command____text____what_it_asks(#[case] text: &str, #[case] expected: Command<'_>) {
    let found = command(text);

    assert_eq!(found, expected);
}

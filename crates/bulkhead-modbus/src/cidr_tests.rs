#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::{contains, parse};

#[rstest]
#[case("10.0.0.0/24", "10.0.0.7", true)]
#[case("10.0.0.0/24", "10.0.1.7", false)]
#[case("127.0.0.0/8", "127.4.5.6", true)]
#[case("0.0.0.0/0", "192.0.2.1", true)]
#[case("::1/128", "::1", true)]
#[case("fd00::/8", "fd12::1", true)]
#[case("10.0.0.0/8", "::1", false)]
fn contains____address____inside_or_not(#[case] net: &str, #[case] ip: &str, #[case] inside: bool) {
    assert_eq!(contains(parse(net).unwrap(), ip.parse().unwrap()), inside);
}

#[rstest]
#[case("10.0.0.0")]
#[case("10.0.0.0/33")]
#[case("nope/8")]
fn parse____not_a_network____none(#[case] entry: &str) {
    assert_eq!(parse(entry), None);
}

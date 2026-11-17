#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case::half("0.5", 500)]
#[case::two("2", 2000)]
#[case::two_as_decimal("2.0", 2000)]
#[case::a_quarter_more("1.25", 1250)]
#[case::smallest("0.01", 10)]
fn millicores____cores____thousandths(#[case] cores: &str, #[case] millis: u32) {
    let parsed: Millicores = serde_json::from_str(cores).unwrap();

    assert_eq!(parsed.millis(), millis);
}

#[rstest]
#[case::negative("-1")]
#[case::negative_decimal("-0.5")]
#[case::four_decimals("0.0001")]
#[case::text("\"half\"")]
fn millicores____not_a_count_of_cores____refused(#[case] cores: &str) {
    assert!(serde_json::from_str::<Millicores>(cores).is_err());
}

#[rstest]
#[case::half(500, "0.5")]
#[case::two(2000, "2")]
#[case::a_quarter_more(1250, "1.25")]
fn millicores____written____as_cores(#[case] millis: u32, #[case] text: &str) {
    let m = Millicores(millis);

    assert_eq!(m.to_string(), text);
    assert_eq!(serde_json::to_string(&m).unwrap(), text);
    assert_eq!(serde_json::from_str::<Millicores>(text).unwrap(), m);
}

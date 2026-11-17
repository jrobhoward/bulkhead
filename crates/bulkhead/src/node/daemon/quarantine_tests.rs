#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

const PERIOD: Duration = Duration::from_secs(600);

#[rstest]
#[case::policy("policy(refusals)", true)]
#[case::rate("policy(rate)", true)]
#[case::protocol("protocol: frame of 20000000 bytes is over 16777216", true)]
#[case::panic("panic: index out of bounds", false)]
#[case::memory("memory_limit", false)]
fn misbehaved____a_crash____policy_and_protocol_only(#[case] why: &str, #[case] counted: bool) {
    assert_eq!(
        misbehaved(&ExitReason::Crashed(why.into())).is_some(),
        counted
    );
}

#[rstest]
#[case::killed(ExitReason::Killed)]
#[case::timeout(ExitReason::Timeout)]
#[case::normal(ExitReason::Normal)]
fn misbehaved____not_a_crash____not_counted(#[case] reason: ExitReason) {
    assert_eq!(misbehaved(&reason), None);
}

#[test]
fn strike____the_third_within_the_period____quarantines_once() {
    let (mut s, t) = (Strikes::default(), Instant::now());

    let got: Vec<bool> = (0..4).map(|_| s.strike("a", t, 3, PERIOD)).collect();

    assert_eq!(got, [false, false, true, false]);
}

#[test]
fn strike____spread_past_the_period____never() {
    let (mut s, t) = (Strikes::default(), Instant::now());

    let got = (0..10u64).any(|i| s.strike("a", t + Duration::from_secs(301 * i), 3, PERIOD));

    assert!(!got);
}

#[test]
fn strike____apps_apart____counted_apart() {
    let (mut s, t) = (Strikes::default(), Instant::now());

    let a = [s.strike("a", t, 2, PERIOD), s.strike("b", t, 2, PERIOD)];

    assert_eq!(a, [false, false]);
}

#[test]
fn strike____quarantine_after_zero____never() {
    let (mut s, t) = (Strikes::default(), Instant::now());

    let got = (0..100).any(|_| s.strike("a", t, 0, PERIOD));

    assert!(!got);
}

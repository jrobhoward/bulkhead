#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn written(key: &str, outcome: Outcome, at: Instant) -> Written {
    Written {
        key: key.into(),
        outcome,
        at,
    }
}

#[test]
fn outcome____replies____sorted() {
    assert_eq!(outcome(Some("ok")), Outcome::Acked);
    assert_eq!(
        outcome(Some("err:unavailable: no primary with a lease")),
        Outcome::Refused
    );
    assert_eq!(outcome(Some("err:unknown")), Outcome::Unknown);
    assert_eq!(outcome(None), Outcome::Unknown);
}

#[test]
fn ledger____acked_between____only_acknowledged_in_the_window() {
    let t0 = Instant::now();
    let mut ledger = Ledger::default();
    ledger.absorb(vec![
        written("w1", Outcome::Acked, t0),
        written("w2", Outcome::Acked, t0 + Duration::from_secs(5)),
        written("w3", Outcome::Unknown, t0 + Duration::from_secs(6)),
        written("w4", Outcome::Acked, t0 + Duration::from_secs(10)),
    ]);

    let window = ledger.acked_between(t0 + Duration::from_secs(1), t0 + Duration::from_secs(10));
    let since = ledger.acked_since(t0 + Duration::from_secs(6));

    assert_eq!(window, BTreeSet::from(["w2".to_string()]));
    assert_eq!(since, BTreeSet::from(["w4".to_string()]));
    assert_eq!(ledger.next, 4);
}

#[test]
fn phantoms____a_refused_key_held____reported() {
    let t0 = Instant::now();
    let mut ledger = Ledger::default();
    ledger.absorb(vec![
        written("w1", Outcome::Acked, t0),
        written("w2", Outcome::Refused, t0),
    ]);
    let held = BTreeSet::from(["w1".to_string(), "w2".to_string()]);

    assert_eq!(phantoms(&ledger, &held), ["w2"]);
}

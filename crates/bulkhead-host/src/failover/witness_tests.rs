#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

/// A request from node 2 whose last confirmation of node 1's lease was 5 s
/// ago, longer than the 3.5 s hold.
fn request(witness: &mut Witness) -> Option<FailoverMsg> {
    answer(witness, 2, request_msg())
}

fn request_msg() -> FailoverMsg {
    FailoverMsg::Request {
        seq: 5,
        resume: false,
        epoch: 1,
        confirmed_ago_ms: 5_000,
    }
}

/// The witness's answer to `msg` from `from`, applying a grant at once (as
/// with nothing to keep on disk).
fn answer(witness: &mut Witness, from: u32, msg: FailoverMsg) -> Option<FailoverMsg> {
    match witness.handle(from, msg)? {
        Decision::Answer(reply) => Some(reply),
        Decision::Grant(grant) => Some(witness.apply(grant)),
        Decision::Record(record) => Some(witness.apply_record(record)),
        Decision::GrantThenState(grant) => {
            witness.apply(grant);
            Some(witness.dr_state())
        }
    }
}

fn config(resume: Option<(u64, u32)>) -> WitnessConfig {
    let mut cfg = WitnessConfig::new([1, 2], 1);
    cfg.resume = resume.map(|(epoch, promoted)| WitnessState::new(epoch, promoted, false));
    cfg
}

#[tokio::test(start_paused = true)]
async fn Witness____fresh_and_partner_long_unconfirmed____grants() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let answer = request(&mut witness);

    assert_eq!(
        answer,
        Some(FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____resumed____waits_a_lease_from_the_restart() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(Some((1, 1))));

    let at_restart = request(&mut witness);
    tokio::time::advance(Duration::from_millis(3_501)).await;
    let after_hold = request(&mut witness);

    assert_eq!(at_restart, Some(FailoverMsg::NotYet { wait_ms: 3_501 }));
    assert_eq!(
        after_hold,
        Some(FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____grant_decided_not_yet_kept____state_unchanged_and_the_pair_unanswered() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let decided = witness.handle(2, request_msg());
    let state_while_keeping = (witness.epoch, witness.promoted);
    let renewal = witness.handle(1, FailoverMsg::Renew { seq: 7, epoch: 1 });
    let second = witness.handle(2, request_msg());
    let Some(Decision::Grant(grant)) = decided else {
        panic!("{decided:?}");
    };
    let granted = witness.apply(grant);
    let old_primary_after = answer(&mut witness, 1, FailoverMsg::Renew { seq: 8, epoch: 1 });

    assert_eq!(
        grant,
        Grant {
            to: 2,
            seq: 5,
            epoch: 2,
            promoted: 2,
            outdated: false,
        }
    );
    assert_eq!(state_while_keeping, (1, 1));
    assert_eq!(renewal, None);
    assert_eq!(second, None);
    assert_eq!(
        granted,
        FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        }
    );
    assert_eq!((witness.epoch, witness.promoted), (2, 2));
    assert_eq!(old_primary_after, Some(FailoverMsg::Fenced { epoch: 2 }));
}

#[tokio::test(start_paused = true)]
async fn Witness____grant_not_kept____abandoned_and_asked_again() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let first = witness.handle(2, request_msg());
    witness.abandon();
    let again = request(&mut witness);

    assert!(matches!(first, Some(Decision::Grant(_))), "{first:?}");
    assert_eq!(
        again,
        Some(FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        })
    );
}

fn record_msg(seq: u64, epoch: u64, outdated: bool) -> FailoverMsg {
    FailoverMsg::Record {
        seq,
        epoch,
        outdated,
    }
}

#[tokio::test(start_paused = true)]
async fn Witness____partner_recorded_out_of_date____not_promoted_until_cleared() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let set = witness.handle(1, record_msg(3, 1, true));
    let Some(Decision::Record(record)) = set else {
        panic!("{set:?}");
    };
    let kept = record.state;
    let recorded = witness.apply_record(record);
    let while_outdated = request(&mut witness);
    let cleared = answer(&mut witness, 1, record_msg(4, 1, false));
    let after_clearing = request(&mut witness);

    assert_eq!(kept, WitnessState::new(1, 1, true));
    assert_eq!(
        recorded,
        FailoverMsg::Recorded {
            seq: 3,
            outdated: true
        }
    );
    assert_eq!(while_outdated, Some(FailoverMsg::Refused { epoch: 1 }));
    assert_eq!(
        cleared,
        Some(FailoverMsg::Recorded {
            seq: 4,
            outdated: false
        })
    );
    assert_eq!(
        after_clearing,
        Some(FailoverMsg::Granted {
            seq: 5,
            epoch: 2,
            outdated: false
        })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____record_from_the_partner_or_a_stale_epoch____fenced_and_unchanged() {
    let mut witness = Witness::start(config(Some((2, 1))));

    let from_partner = answer(&mut witness, 2, record_msg(1, 2, true));
    let stale = answer(&mut witness, 1, record_msg(2, 1, true));

    assert_eq!(from_partner, Some(FailoverMsg::Fenced { epoch: 2 }));
    assert_eq!(stale, Some(FailoverMsg::Fenced { epoch: 2 }));
    assert!(!witness.outdated);
}

#[tokio::test(start_paused = true)]
async fn Witness____record_already_as_asked____answered_without_keeping() {
    let mut witness = Witness::start(config(None));

    let decision = witness.handle(1, record_msg(6, 1, false));

    assert_eq!(
        decision,
        Some(Decision::Answer(FailoverMsg::Recorded {
            seq: 6,
            outdated: false
        }))
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____resumed_with_the_record____resume_keeps_it_and_the_partner_is_refused() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut cfg = WitnessConfig::new([1, 2], 1);
    cfg.resume = Some(WitnessState::new(2, 1, true));
    let mut witness = Witness::start(cfg);
    tokio::time::advance(Duration::from_secs(4)).await;

    let resumed = answer(
        &mut witness,
        1,
        FailoverMsg::Request {
            seq: 9,
            resume: true,
            epoch: 2,
            confirmed_ago_ms: 5_000,
        },
    );
    let partner = answer(
        &mut witness,
        2,
        FailoverMsg::Request {
            seq: 1,
            resume: false,
            epoch: 2,
            confirmed_ago_ms: 5_000,
        },
    );

    assert_eq!(
        resumed,
        Some(FailoverMsg::Granted {
            seq: 9,
            epoch: 2,
            outdated: true
        })
    );
    assert_eq!(partner, Some(FailoverMsg::Refused { epoch: 2 }));
}

/// A request from node 2 that confirmed node 1's lease just now, as a
/// standby that hears its partner yield asks.
fn fresh_request(seq: u64) -> FailoverMsg {
    FailoverMsg::Request {
        seq,
        resume: false,
        epoch: 1,
        confirmed_ago_ms: 0,
    }
}

#[tokio::test(start_paused = true)]
async fn Witness____primary_released_its_lease____partner_granted_at_once() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));
    answer(&mut witness, 1, FailoverMsg::Renew { seq: 1, epoch: 1 });

    let before = answer(&mut witness, 2, fresh_request(5));
    let released = answer(&mut witness, 1, FailoverMsg::Release { epoch: 1 });
    let after = answer(&mut witness, 2, fresh_request(6));

    assert_eq!(before, Some(FailoverMsg::NotYet { wait_ms: 3_501 }));
    assert_eq!(released, None);
    assert_eq!(
        after,
        Some(FailoverMsg::Granted {
            seq: 6,
            epoch: 2,
            outdated: false
        })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____release_from_the_partner_or_a_stale_epoch____ignored() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(Some((3, 1))));

    answer(&mut witness, 2, FailoverMsg::Release { epoch: 3 });
    answer(&mut witness, 1, FailoverMsg::Release { epoch: 2 });
    let asked = answer(
        &mut witness,
        2,
        FailoverMsg::Request {
            seq: 5,
            resume: false,
            epoch: 3,
            confirmed_ago_ms: 0,
        },
    );

    assert!(!witness.released);
    assert!(
        matches!(asked, Some(FailoverMsg::NotYet { .. })),
        "{asked:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____released_then_resumed____the_grant_clears_the_release() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));
    answer(&mut witness, 1, FailoverMsg::Release { epoch: 1 });

    let resumed = answer(
        &mut witness,
        1,
        FailoverMsg::Request {
            seq: 7,
            resume: true,
            epoch: 1,
            confirmed_ago_ms: 5_000,
        },
    );
    let partner = answer(&mut witness, 2, fresh_request(8));

    assert_eq!(
        resumed,
        Some(FailoverMsg::Granted {
            seq: 7,
            epoch: 1,
            outdated: false
        })
    );
    assert!(
        matches!(partner, Some(FailoverMsg::NotYet { .. })),
        "{partner:?}"
    );
}

// ------------------------------------------------- DR activation (DR4)

fn renew(witness: &mut Witness, from: u32, epoch: u64) -> Option<FailoverMsg> {
    answer(witness, from, FailoverMsg::Renew { seq: 9, epoch })
}

#[tokio::test(start_paused = true)]
async fn Witness____fenced____waits_out_the_last_renewal_and_answers_nothing_granted() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));
    assert_eq!(
        renew(&mut witness, 1, 1),
        Some(FailoverMsg::LeaseOk { seq: 9 })
    );
    tokio::time::advance(Duration::from_secs(1)).await;

    let gone = witness.fence(Instant::now()).unwrap();

    // Lease (3 s) and margin (0.5 s) from the renewal, a second ago.
    assert_eq!(gone - Instant::now(), Duration::from_millis(2_500));
    assert_eq!(witness.fence(Instant::now()), Err(ActivateRefused::Fencing));
}

#[tokio::test(start_paused = true)]
async fn Witness____fenced_never_renewed____gone_now() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));

    let gone = witness.fence(Instant::now()).unwrap();

    assert_eq!(gone, Instant::now());
}

#[tokio::test(start_paused = true)]
async fn Witness____restarted_then_fenced____waits_a_lease_from_the_restart() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(Some((4, 1))));

    let gone = witness.fence(Instant::now()).unwrap();

    assert_eq!(gone - Instant::now(), Duration::from_millis(3_500));
}

#[tokio::test(start_paused = true)]
async fn Witness____activated____new_epoch_and_the_pair_is_stale() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));
    witness.fence(Instant::now()).unwrap();

    let grant = witness.activate(3).unwrap();
    witness.apply(grant);

    assert_eq!((witness.epoch, witness.promoted), (2, 3));
    assert!(witness.activated());
    assert_eq!(
        renew(&mut witness, 1, 1),
        Some(FailoverMsg::DrActive { epoch: 2, dr: 3 })
    );
    assert_eq!(
        answer(
            &mut witness,
            1,
            FailoverMsg::Request {
                seq: 1,
                resume: true,
                epoch: 1,
                confirmed_ago_ms: 60_000
            }
        ),
        Some(FailoverMsg::DrActive { epoch: 2, dr: 3 })
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(
        request(&mut witness),
        Some(FailoverMsg::DrActive { epoch: 2, dr: 3 })
    );
    assert_eq!(
        witness.fence(Instant::now()),
        Err(ActivateRefused::Active(2))
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____a_pair_node_heard_while_fenced____refused_then_abandoned() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));
    witness.fence(Instant::now()).unwrap();

    let renewed = renew(&mut witness, 1, 1);
    let requested = request(&mut witness);
    let activated = witness.activate(3);

    assert_eq!(renewed, Some(FailoverMsg::Fenced { epoch: 1 }));
    assert_eq!(requested, Some(FailoverMsg::Refused { epoch: 1 }));
    assert_eq!(activated, Err(ActivateRefused::Heard(2)));
    // Fencing ends with it: the pair is answered as before.
    assert_eq!(
        renew(&mut witness, 1, 1),
        Some(FailoverMsg::LeaseOk { seq: 9 })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____activation_not_kept____fencing_ends() {
    tokio::time::advance(Duration::from_secs(10)).await;
    let mut witness = Witness::start(config(None));
    witness.fence(Instant::now()).unwrap();
    let _grant = witness.activate(3).unwrap();

    witness.abandon();

    assert_eq!((witness.epoch, witness.promoted), (1, 1));
    assert_eq!(
        renew(&mut witness, 1, 1),
        Some(FailoverMsg::LeaseOk { seq: 9 })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____resumed_activated____stays_active() {
    let mut witness = Witness::start(config(Some((5, 3))));

    assert!(witness.activated());
    assert_eq!(
        renew(&mut witness, 2, 4),
        Some(FailoverMsg::DrActive { epoch: 5, dr: 3 })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____handed_back____node_resumes_at_a_new_epoch() {
    let mut witness = Witness::start(config(Some((5, 3))));

    let grant = witness.hand_back(2).unwrap();
    witness.apply(grant);
    tokio::time::advance(Duration::from_secs(10)).await;
    let resumed = answer(
        &mut witness,
        2,
        FailoverMsg::Request {
            seq: 4,
            resume: true,
            epoch: 5,
            confirmed_ago_ms: 60_000,
        },
    );
    let other = answer(
        &mut witness,
        1,
        FailoverMsg::Request {
            seq: 1,
            resume: true,
            epoch: 5,
            confirmed_ago_ms: 60_000,
        },
    );

    assert!(!witness.activated());
    assert_eq!(
        resumed,
        Some(FailoverMsg::Granted {
            seq: 4,
            epoch: 6,
            outdated: false
        })
    );
    assert_eq!(other, Some(FailoverMsg::Refused { epoch: 6 }));
}

#[tokio::test(start_paused = true)]
async fn Witness____hand_back_not_active_or_outside_the_pair____refused() {
    let mut active = Witness::start(config(Some((5, 3))));
    let mut plain = Witness::start(config(None));

    assert!(active.hand_back(7).is_err());
    assert!(plain.hand_back(2).is_err());
}

/// A witness for pair [1, 2] that takes DR asks from node 4, a DR node
/// apart (DR4b).
fn apart() -> Witness {
    let mut cfg = config(None);
    cfg.dr = Some(4);
    Witness::start(cfg)
}

fn dr_ask(witness: &mut Witness, from: u32, intent: DrIntent) -> Option<FailoverMsg> {
    answer(witness, from, FailoverMsg::DrAsk { intent })
}

fn dr_state(epoch: u64, promoted: u32, fencing: bool, abandoned: Option<u32>) -> FailoverMsg {
    FailoverMsg::DrState {
        epoch,
        promoted,
        fencing,
        abandoned,
    }
}

#[tokio::test(start_paused = true)]
async fn Witness____dr_ask_from_a_node_it_does_not_name____ignored() {
    let mut witness = apart();
    let mut plain = Witness::start(config(None));

    let stranger = dr_ask(&mut witness, 5, DrIntent::Fence { attempt: 1 });
    let pair_node = dr_ask(&mut witness, 2, DrIntent::Fence { attempt: 1 });
    let unnamed = dr_ask(&mut plain, 4, DrIntent::Fence { attempt: 1 });

    assert_eq!(stranger, None);
    assert_eq!(pair_node, None);
    assert_eq!(unnamed, None);
    assert!(witness.fencing.is_none());
}

#[tokio::test(start_paused = true)]
async fn Witness____dr_apart_fences____promoted_on_its_first_ask_once_the_lease_is_gone() {
    let mut witness = apart();
    renew(&mut witness, 1, 1);

    let fencing = dr_ask(&mut witness, 4, DrIntent::Fence { attempt: 1 });
    tokio::time::advance(Duration::from_millis(3_400)).await;
    let early = dr_ask(&mut witness, 4, DrIntent::State);
    tokio::time::advance(Duration::from_millis(200)).await;
    let granted = dr_ask(&mut witness, 4, DrIntent::State);
    let again = dr_ask(&mut witness, 4, DrIntent::Fence { attempt: 1 });

    assert_eq!(fencing, Some(dr_state(1, 1, true, None)));
    assert_eq!(early, Some(dr_state(1, 1, true, None)));
    assert_eq!(granted, Some(dr_state(2, 4, false, None)));
    // Asked again: what is done is only answered.
    assert_eq!(again, Some(dr_state(2, 4, false, None)));
    assert_eq!(
        renew(&mut witness, 1, 1),
        Some(FailoverMsg::DrActive { epoch: 2, dr: 4 })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____pair_node_heard_while_the_dr_apart_fences____abandoned_and_said_so() {
    let mut witness = apart();
    renew(&mut witness, 1, 1);
    dr_ask(&mut witness, 4, DrIntent::Fence { attempt: 1 });
    tokio::time::advance(Duration::from_millis(100)).await;
    renew(&mut witness, 2, 1);
    tokio::time::advance(Duration::from_secs(4)).await;

    let abandoned = dr_ask(&mut witness, 4, DrIntent::Fence { attempt: 1 });
    renew(&mut witness, 1, 1);
    // The same activation asked again (still in flight when it was
    // abandoned): no new fence.
    let stale = dr_ask(&mut witness, 4, DrIntent::Fence { attempt: 1 });
    let fenced_again = dr_ask(&mut witness, 4, DrIntent::Fence { attempt: 2 });

    assert_eq!(abandoned, Some(dr_state(1, 1, false, Some(2))));
    assert_eq!(stale, Some(dr_state(1, 1, false, Some(2))));
    assert_eq!(fenced_again, Some(dr_state(1, 1, true, None)));
}

#[tokio::test(start_paused = true)]
async fn Witness____dr_apart_hands_back____pair_node_promoted_and_asked_again_only_answered() {
    let mut witness = apart();
    dr_ask(&mut witness, 4, DrIntent::Fence { attempt: 1 });
    dr_ask(&mut witness, 4, DrIntent::State);
    let before = dr_ask(&mut witness, 4, DrIntent::State);

    let handed = dr_ask(&mut witness, 4, DrIntent::HandBack { to: 2 });
    let again = dr_ask(&mut witness, 4, DrIntent::HandBack { to: 1 });
    tokio::time::advance(Duration::from_secs(5)).await;
    let resumed = answer(
        &mut witness,
        2,
        FailoverMsg::Request {
            seq: 9,
            resume: true,
            epoch: 1,
            confirmed_ago_ms: 5_000,
        },
    );

    assert_eq!(before, Some(dr_state(2, 4, false, None)));
    assert_eq!(handed, Some(dr_state(3, 2, false, None)));
    assert_eq!(again, Some(dr_state(3, 2, false, None)));
    assert_eq!(
        resumed,
        Some(FailoverMsg::Granted {
            seq: 9,
            epoch: 3,
            outdated: false
        })
    );
}

#[tokio::test(start_paused = true)]
async fn Witness____hand_back_asked_before_the_dr_apart_is_promoted____nothing_done() {
    let mut witness = apart();

    let early = dr_ask(&mut witness, 4, DrIntent::HandBack { to: 2 });

    assert_eq!(early, Some(dr_state(1, 1, false, None)));
}

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use redb::ReadableDatabase;
use tokio::time::Instant;

use super::*;
use crate::replication::log::{Command, Entry, SPACE};
use crate::replication::ship::Shipper;
use crate::services::keyvalue::storage::{Disk, Op};

fn set(epoch: u64, key: &str) -> Entry {
    Entry {
        epoch,
        cmd: Command::Write {
            bucket: "points".into(),
            quota: None,
            op: Op::Set(key.into(), b"v".to_vec()),
        },
    }
}

fn empty_log() -> Log {
    Log::open(Arc::new(Disk::in_memory())).unwrap()
}

/// A pair node's log: `last` entries of epoch 1, committed up to `commit`.
fn log(last: u64, commit: u64) -> Log {
    let mut log = empty_log();
    let entries: Vec<Entry> = (1..=last).map(|i| set(1, &format!("k{i}"))).collect();
    log.append(1, &entries).unwrap();
    log.commit_to(commit).unwrap();
    log
}

fn tuning() -> DrTuning {
    DrTuning {
        pair: [1, 2],
        poll: Duration::from_secs(1),
        batch_bytes: 1 << 20,
        rpo_target: Duration::from_secs(300),
        log_bytes: 64 << 20,
        log_age: Duration::from_secs(3600),
    }
}

/// A pair node as the DR node sees it: its log, its standing, whether it
/// answers.
struct Node {
    log: Log,
    standby: bool,
    up: bool,
    shipper: Shipper,
}

impl Node {
    fn new(log: Log, standby: bool) -> Self {
        Self {
            log,
            standby,
            up: true,
            shipper: Shipper::new(Instant::now()),
        }
    }
}

/// Deliver the DR node's sends to `nodes` and their answers back, until
/// nothing more is sent; the asks each node got, in order.
fn exchange(
    dr: &mut DrReplica,
    nodes: &mut [Node; 2],
    mut effects: Vec<DrEffect>,
    now: Instant,
) -> Vec<(u32, DrMsg)> {
    let mut asked = Vec::new();
    while !effects.is_empty() {
        let mut next = Vec::new();
        for e in effects {
            let DrEffect::Send { to, msg } = e else {
                continue;
            };
            asked.push((to, msg.clone()));
            let n = &mut nodes[usize::try_from(to - 1).unwrap()];
            if !n.up {
                continue;
            }
            if let Some(answer) = n.shipper.answer(&n.log, n.standby, msg, now).unwrap() {
                next.extend(dr.answer(to, answer, now).unwrap());
            }
        }
        effects = next;
    }
    asked
}

fn keys(log: &Log) -> Vec<String> {
    let txn = log.disk().db().begin_read().unwrap();
    let dumps = log.disk().export(&txn, SPACE).unwrap();
    dumps
        .iter()
        .flat_map(|d| d.entries.iter().map(|e| e.key.clone()))
        .collect()
}

#[test]
fn round____pair_has_committed_entries____copy_takes_them_and_none_uncommitted() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    let mut nodes = [Node::new(log(5, 5), false), Node::new(log(5, 4), true)];

    let effects = dr.tick(now).unwrap();
    exchange(&mut dr, &mut nodes, effects, now);

    assert_eq!(dr.log.commit(), 5);
    assert_eq!(dr.log.last(), 5);
    assert_eq!(keys(&dr.log).len(), 5);
}

#[test]
fn round____standby_within_a_batch_of_the_best____pulled_from_and_primary_probed() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    let mut nodes = [Node::new(log(8, 8), false), Node::new(log(8, 7), true)];
    let effects = dr.tick(now).unwrap();
    exchange(&mut dr, &mut nodes, effects, now);
    let later = now + Duration::from_secs(2);

    let effects = dr.tick(later).unwrap();
    let asked = exchange(&mut dr, &mut nodes, effects, later);

    let batch_to = |node: u32| {
        asked.iter().any(|(to, m)| {
            *to == node && matches!(m, DrMsg::Ask { max_bytes, .. } if *max_bytes > 0)
        })
    };
    assert!(batch_to(2), "{asked:?}");
    assert!(!batch_to(1), "{asked:?}");
    assert_eq!(dr.status(later).source, 2);
    // The first round, before the standby was heard, pulled from the
    // primary.
    assert_eq!(dr.log.commit(), 8);
}

#[test]
fn round____standby_far_behind____pulled_from_the_primary() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    let mut nodes = [
        Node::new(log(600, 600), false),
        Node::new(log(10, 10), true),
    ];

    let effects = dr.tick(now).unwrap();
    exchange(&mut dr, &mut nodes, effects, now);

    assert_eq!(dr.log.commit(), 600);
    assert_eq!(dr.status(now).source, 1);
}

#[test]
fn tick____pulled_node_silent_twice____pulls_from_the_other() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    let mut nodes = [Node::new(log(4, 4), false), Node::new(log(4, 4), true)];
    nodes[0].up = false;
    nodes[1].up = false;
    let effects = dr.tick(now).unwrap();
    exchange(&mut dr, &mut nodes, effects, now);
    nodes[1].up = true;

    let mut at = now;
    for _ in 0..3 {
        at += Duration::from_secs(5);
        let effects = dr.tick(at).unwrap();
        exchange(&mut dr, &mut nodes, effects, at);
    }

    assert_eq!(dr.status(at).source, 2);
    assert_eq!(dr.log.commit(), 4);
}

#[test]
fn round____entries_compacted_on_the_pair____caught_up_from_a_snapshot() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    let mut primary = log(30, 30);
    primary.compact(25).unwrap();
    let mut standby = log(30, 30);
    standby.compact(25).unwrap();
    let mut nodes = [Node::new(primary, false), Node::new(standby, true)];

    let effects = dr.tick(now).unwrap();
    exchange(&mut dr, &mut nodes, effects, now);

    assert_eq!(dr.log.commit(), 30);
    assert_eq!(dr.log.base(), 30);
    assert_eq!(keys(&dr.log).len(), 30);
    assert!(!dr.status(now).snapshotting);
}

#[test]
fn answer____chunk_of_another_snapshot____started_over_from_its_first() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    let mut out = Vec::new();
    dr.ask_snapshot(0, 0, now, &mut out);
    let chunk = |index, chunk, last| DrMsg::Snapshot {
        index,
        index_epoch: 1,
        chunk,
        last,
        buckets: Vec::new(),
    };
    dr.answer(1, chunk(10, 0, false), now).unwrap();

    let stale = dr.answer(1, chunk(12, 1, true), now).unwrap();
    let restarted = dr.answer(1, chunk(12, 0, false), now).unwrap();

    assert!(stale.is_empty());
    assert_eq!(
        restarted,
        [DrEffect::Send {
            to: 1,
            msg: DrMsg::AskSnapshot {
                index: 12,
                chunk: 1
            }
        }]
    );
}

#[test]
fn answer____mismatch_from_both____stopped() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    let mismatch = DrMsg::Mismatch {
        commit: 3,
        after: 0,
    };

    dr.answer(1, mismatch.clone(), now).unwrap();
    let one = dr.status(now);
    dr.answer(2, mismatch, now).unwrap();

    assert_eq!((one.source, one.stopped), (2, false));
    assert!(dr.status(now).stopped);
    assert!(dr.tick(now + Duration::from_secs(5)).unwrap().is_empty());
}

#[test]
fn lag____pair_unreachable_then_back____grows_then_resets() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    let mut nodes = [Node::new(log(3, 3), false), Node::new(log(3, 3), true)];
    let effects = dr.tick(now).unwrap();
    exchange(&mut dr, &mut nodes, effects, now);
    let caught_up = dr.lag(now);
    nodes[0].up = false;
    nodes[1].up = false;

    let later = now + Duration::from_secs(400);
    let effects = dr.tick(later).unwrap();
    exchange(&mut dr, &mut nodes, effects, later);
    let cut = dr.lag(later);
    nodes[0].up = true;
    nodes[1].up = true;
    let back = later + Duration::from_secs(10);
    // Back, each on a new session.
    let mut effects = dr.reconnected(0, back);
    effects.extend(dr.reconnected(1, back));
    effects.extend(dr.tick(back).unwrap());
    exchange(&mut dr, &mut nodes, effects, back);

    assert_eq!(caught_up, Duration::ZERO);
    assert_eq!(cut, Duration::from_secs(400));
    assert!(dr.warned);
    assert_eq!(dr.lag(back), Duration::ZERO);
}

#[test]
fn new____entries_past_the_commit_index_on_disk____applied() {
    let mut on_disk = empty_log();
    on_disk.append(1, &[set(1, "a"), set(1, "b")]).unwrap();

    let dr = DrReplica::new(tuning(), on_disk, Instant::now()).unwrap();

    assert_eq!(dr.log.commit(), 2);
}

#[test]
fn answer____from_a_node_not_of_the_pair____ignored() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();

    let out = dr
        .answer(
            9,
            DrMsg::Entries {
                standby: true,
                commit: 1,
                after: 0,
                entries: vec![set(1, "x")],
            },
            now,
        )
        .unwrap();

    assert!(out.is_empty());
    assert_eq!(dr.log.commit(), 0);
}

// --------------------------------------------- activation (DR4)

#[test]
fn write____not_leading____unavailable_and_nothing_written() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();

    let (answer, out) = dr.write("points".into(), None, Op::Set("k".into(), b"v".to_vec()));

    assert_eq!(answer, Err(Refusal::Unavailable));
    assert!(out.is_empty());
    assert_eq!(dr.log.last(), 0);
}

#[test]
fn lead____after_the_copy____noop_of_the_new_epoch_then_writes_commit_at_once() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    let mut nodes = [Node::new(log(3, 3), false), Node::new(log(3, 3), true)];
    let effects = dr.tick(now).unwrap();
    exchange(&mut dr, &mut nodes, effects, now);

    dr.lead(7).unwrap();
    let (answer, out) = dr.write("points".into(), None, Op::Set("new".into(), b"v".to_vec()));

    assert!(answer.is_ok(), "{answer:?}");
    assert!(matches!(&out[..], [DrEffect::Notify { bucket, .. }] if bucket == "points"));
    assert_eq!(dr.log.commit(), 5);
    assert_eq!(dr.log.epoch_at(4), Some(7));
    assert_eq!(keys(&dr.log).len(), 4);
    assert_eq!(dr.status(now).leading, Some(7));
    assert_eq!(dr.lag(now + Duration::from_secs(600)), Duration::ZERO);
}

#[test]
fn lead____again_after_a_restart____no_second_noop() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    dr.lead(7).unwrap();

    dr.lead(7).unwrap();

    assert_eq!(dr.log.last(), 1);
}

#[test]
fn leading____the_pair_answers_and_ticks____nothing_taken_or_asked() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    dr.lead(7).unwrap();

    let ticked = dr.tick(now + Duration::from_secs(5)).unwrap();
    let answered = dr
        .answer(
            1,
            DrMsg::Entries {
                standby: false,
                commit: 9,
                after: 1,
                entries: vec![set(1, "stale")],
            },
            now,
        )
        .unwrap();

    assert!(ticked.is_empty());
    assert!(answered.is_empty());
    assert_eq!(dr.log.last(), 1);
}

#[test]
fn leading____both_pair_nodes_take_a_snapshot_at_once____neither_starts_the_other_over() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    dr.lead(7).unwrap();
    // Three values of 600 KiB: a snapshot of more than one chunk.
    for key in ["a", "b", "c"] {
        let (answer, _) = dr.write(
            "points".into(),
            None,
            Op::Set(key.into(), vec![0; 600 << 10]),
        );
        answer.unwrap();
    }
    let ask = |index, chunk| DrMsg::AskSnapshot { index, chunk };
    let first = |out: Vec<DrEffect>| match out.into_iter().next() {
        Some(DrEffect::Send {
            msg: DrMsg::Snapshot { index, chunk, .. },
            ..
        }) => (index, chunk),
        other => panic!("not a snapshot chunk: {other:?}"),
    };

    let (index_1, _) = first(dr.answer(1, ask(0, 0), now).unwrap());
    // A write between the two, then node 2's snapshot of its own.
    let (answer, _) = dr.write("points".into(), None, Op::Set("d".into(), b"v".to_vec()));
    answer.unwrap();
    let (index_2, _) = first(dr.answer(2, ask(0, 0), now).unwrap());
    let next_1 = first(dr.answer(1, ask(index_1, 1), now).unwrap());
    let next_2 = first(dr.answer(2, ask(index_2, 1), now).unwrap());

    assert_ne!(index_1, index_2);
    assert_eq!(next_1, (index_1, 1));
    assert_eq!(next_2, (index_2, 1));
}

/// A probe from the DR node holding nothing.
fn probe() -> DrMsg {
    DrMsg::Ask {
        after: 0,
        after_epoch: 0,
        max_bytes: 0,
        rpo_target_ms: 300_000,
    }
}

/// An answer to a probe: `commit` entries committed, none sent.
fn probed(commit: u64) -> DrMsg {
    DrMsg::Entries {
        standby: false,
        commit,
        after: 0,
        entries: Vec::new(),
    }
}

#[test]
fn reconnected____taking_a_snapshot_from_that_node____probe_at_once_then_the_chunk_once_answered() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    dr.answer(1, probed(0), now).unwrap();
    let mut out = Vec::new();
    dr.ask_snapshot(0, 0, now, &mut out);
    dr.answer(
        1,
        DrMsg::Snapshot {
            index: 10,
            index_epoch: 1,
            chunk: 0,
            last: false,
            buckets: Vec::new(),
        },
        now,
    )
    .unwrap();

    let other = dr.reconnected(1, now);
    let probed_again = dr.reconnected(0, now);
    let asked = dr.answer(1, probed(10), now).unwrap();
    // Nothing more until the backstop.
    let waited = dr.tick(now + Duration::from_secs(10)).unwrap();

    assert!(other.is_empty());
    assert_eq!(
        probed_again,
        [DrEffect::Send {
            to: 1,
            msg: probe()
        }]
    );
    assert_eq!(
        asked,
        [DrEffect::Send {
            to: 1,
            msg: DrMsg::AskSnapshot {
                index: 10,
                chunk: 1
            }
        }]
    );
    assert!(waited.is_empty(), "{waited:?}");
}

#[test]
fn round____not_heard_on_this_session____probes_only_then_a_batch_once_answered() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();

    let first = dr.tick(now).unwrap();
    let batch = dr.answer(1, probed(5), now).unwrap();

    assert_eq!(
        first,
        [
            DrEffect::Send {
                to: 1,
                msg: probe()
            },
            DrEffect::Send {
                to: 2,
                msg: probe()
            }
        ]
    );
    assert!(
        matches!(&batch[..], [DrEffect::Send { to: 1, msg: DrMsg::Ask { max_bytes, .. } }] if *max_bytes > 0),
        "{batch:?}"
    );
}

#[test]
fn tick____batch_overdue_on_a_session_that_delivers____asked_again_only_after_the_chunk_timeout() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    dr.tick(now).unwrap();
    dr.answer(1, probed(5), now).unwrap();
    let is_batch_to_1 = |e: &DrEffect| matches!(e, DrEffect::Send { to: 1, msg: DrMsg::Ask { max_bytes, .. } } if *max_bytes > 0);

    // A batch on its way over a slow link is not asked for again (DR3b).
    let soon = dr.tick(now + Duration::from_secs(10)).unwrap();
    let late = dr.tick(now + CHUNK_TIMEOUT).unwrap();

    assert!(!soon.iter().any(is_batch_to_1), "{soon:?}");
    assert!(late.iter().any(is_batch_to_1), "{late:?}");
}

#[test]
fn reconnected____batch_asked_of_that_node____probe_at_once() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    dr.tick(now).unwrap();
    dr.answer(1, probed(5), now).unwrap();
    let later = now + Duration::from_secs(3);

    let again = dr.reconnected(0, later);

    assert!(
        again.contains(&DrEffect::Send {
            to: 1,
            msg: probe()
        }),
        "{again:?}"
    );
}

#[test]
fn answer____an_ask_from_a_reseeding_pair_node____not_taken_as_its_stream_delivering() {
    let now = Instant::now();
    let mut dr = DrReplica::new(tuning(), empty_log(), now).unwrap();
    dr.tick(now).unwrap();

    let ignored = dr.answer(1, probe(), now).unwrap();
    let later = dr.tick(now + Duration::from_secs(5)).unwrap();

    assert!(ignored.is_empty());
    // Still probes only, asked again after the short timeout: a batch
    // asked of a reseeding node would go unanswered until the backstop.
    assert!(
        later.iter().all(|e| matches!(
            e,
            DrEffect::Send {
                msg: DrMsg::Ask { max_bytes: 0, .. },
                ..
            }
        )),
        "{later:?}"
    );
    assert!(!later.is_empty());
}

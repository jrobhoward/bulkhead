#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::sync::Arc;

use redb::ReadableDatabase;

use super::*;
use crate::replication::log::{Command, Entry, SPACE};
use crate::replication::ship::{CHUNK_TIMEOUT, Shipper};
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

fn log_of(entries: &[Entry], commit: u64) -> Log {
    let mut log = Log::open(Arc::new(Disk::in_memory())).unwrap();
    if !entries.is_empty() {
        log.append(1, entries).unwrap();
        log.commit_to(commit).unwrap();
    }
    log
}

fn keys(log: &Log) -> Vec<String> {
    let txn = log.disk().db().begin_read().unwrap();
    let dumps = log.disk().export(&txn, SPACE).unwrap();
    let mut keys: Vec<String> = dumps
        .iter()
        .flat_map(|d| d.entries.iter().map(|e| e.key.clone()))
        .collect();
    keys.sort();
    keys
}

/// The probe's answer: the DR node holds nothing yet.
fn probe_answer() -> DrMsg {
    DrMsg::Entries {
        standby: false,
        commit: 0,
        after: 0,
        entries: Vec::new(),
    }
}

fn probe() -> ReseedEffect {
    ReseedEffect::Send(DrMsg::Ask {
        after: 0,
        after_epoch: 0,
        max_bytes: 0,
        rpo_target_ms: u64::MAX,
    })
}

/// Deliver the reseed's asks to the DR node's shipper over `dr_log`, and
/// its answers back, until nothing more is sent.
fn exchange(reseed: &mut Reseed, log: &mut Log, dr_log: &Log, mut effects: Vec<ReseedEffect>) {
    let mut shipper = Shipper::new(Instant::now());
    let now = Instant::now();
    let mut rounds = 0;
    while let Some(effect) = effects.pop() {
        rounds += 1;
        assert!(rounds < 1000, "no end to the exchange");
        let ReseedEffect::Send(ask) = effect else {
            continue;
        };
        if let Some(answer) = shipper.answer(dr_log, false, ask, now).unwrap() {
            effects.extend(reseed.answer(log, answer, now).unwrap());
        }
    }
}

/// The DR node's log: what it copied from the pair (three entries of
/// epoch 1), then its own epoch 2 (a no-op and a write).
fn dr_log() -> Log {
    let entries = [
        set(1, "k1"),
        set(1, "k2"),
        set(1, "k3"),
        Entry {
            epoch: 2,
            cmd: Command::Noop,
        },
        set(2, "d1"),
    ];
    log_of(&entries, 5)
}

#[test]
fn reseed____stale_copy_ahead_of_the_dr_node____becomes_exactly_its_copy() {
    // Site 1 went on past what the DR node copied: k4 to k6 committed.
    let ahead: Vec<Entry> = (1..=6).map(|i| set(1, &format!("k{i}"))).collect();
    let mut log = log_of(&ahead, 6);
    let dr = dr_log();
    let now = Instant::now();
    let mut reseed = Reseed::new(3, now);

    let first = reseed.tick(&log, now).unwrap();
    exchange(&mut reseed, &mut log, &dr, first);

    assert_eq!(keys(&log), ["d1", "k1", "k2", "k3"]);
    assert_eq!(log.commit(), 5);
    assert_eq!(log.last(), 5);
    assert_eq!(log.last_epoch(), 2);
    assert!(reseed.caught_up(&log));
}

#[test]
fn reseed____dr_node_writes_more____entries_after_the_snapshot_taken() {
    let mut log = log_of(&[], 0);
    let mut dr = dr_log();
    let now = Instant::now();
    let mut reseed = Reseed::new(3, now);
    let first = reseed.tick(&log, now).unwrap();
    exchange(&mut reseed, &mut log, &dr, first);
    dr.append(6, &[set(2, "d2"), set(2, "d3")]).unwrap();
    dr.commit_to(7).unwrap();

    let later = now + Duration::from_secs(1);
    let asked = reseed.tick(&log, later).unwrap();
    exchange(&mut reseed, &mut log, &dr, asked);

    assert_eq!(keys(&log), ["d1", "d2", "d3", "k1", "k2", "k3"]);
    assert_eq!(log.commit(), 7);
    assert_eq!(log.base(), 5, "only the first catch-up was a snapshot");
}

#[test]
fn reseed____behind_or_mismatch____starts_over_from_a_snapshot() {
    let mut log = log_of(&[], 0);
    let now = Instant::now();
    let mut reseed = Reseed::new(3, now);
    let _ = reseed.tick(&log, now).unwrap();
    let _ = reseed.answer(&mut log, probe_answer(), now).unwrap();
    let dr = dr_log();
    let snapshot = Shipper::new(now)
        .answer(&dr, false, DrMsg::AskSnapshot { index: 0, chunk: 0 }, now)
        .unwrap()
        .unwrap();
    let _ = reseed.answer(&mut log, snapshot, now).unwrap();

    let out = reseed
        .answer(
            &mut log,
            DrMsg::Mismatch {
                commit: 9,
                after: 5,
            },
            now,
        )
        .unwrap();

    assert_eq!(
        out,
        [ReseedEffect::Send(DrMsg::AskSnapshot {
            index: 0,
            chunk: 0
        })]
    );
    assert!(!reseed.caught_up(&log));
}

#[test]
fn tick____unanswered_on_this_session____only_a_probe_asked_again_soon() {
    let log = log_of(&[], 0);
    let now = Instant::now();
    let mut reseed = Reseed::new(3, now);

    let first = reseed.tick(&log, now).unwrap();
    let soon = reseed.tick(&log, now + Duration::from_millis(500)).unwrap();
    let again = reseed.tick(&log, now + TIMEOUT).unwrap();

    assert_eq!(first, [probe()]);
    assert!(soon.is_empty());
    assert_eq!(again, [probe()]);
}

#[test]
fn tick____snapshot_chunk_overdue____asked_again_only_after_the_chunk_timeout() {
    let mut log = log_of(&[], 0);
    let now = Instant::now();
    let mut reseed = Reseed::new(3, now);
    let _ = reseed.tick(&log, now).unwrap();
    let asked = reseed.answer(&mut log, probe_answer(), now).unwrap();

    // A chunk on its way over a slow link is not asked for again (a second
    // copy would only queue behind it).
    let soon = reseed.tick(&log, now + TIMEOUT).unwrap();
    let late = reseed.tick(&log, now + CHUNK_TIMEOUT).unwrap();

    let chunk = ReseedEffect::Send(DrMsg::AskSnapshot { index: 0, chunk: 0 });
    assert_eq!(asked, [chunk]);
    assert!(soon.is_empty());
    assert_eq!(
        late,
        [ReseedEffect::Send(DrMsg::AskSnapshot {
            index: 0,
            chunk: 0
        })]
    );
}

#[test]
fn reconnected____chunk_asked____probe_again_at_the_next_tick() {
    let mut log = log_of(&[], 0);
    let now = Instant::now();
    let mut reseed = Reseed::new(3, now);
    let _ = reseed.tick(&log, now).unwrap();
    let _ = reseed.answer(&mut log, probe_answer(), now).unwrap();
    let later = now + Duration::from_secs(1);

    reseed.reconnected(later);
    let again = reseed.tick(&log, later).unwrap();

    assert_eq!(again, [probe()]);
}

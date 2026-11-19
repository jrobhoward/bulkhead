#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::replication::log::Command;
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

/// A log of `last` entries of epoch 1, committed up to `commit`.
fn log(last: u64, commit: u64) -> Log {
    let mut log = Log::open(Arc::new(Disk::in_memory())).unwrap();
    let entries: Vec<Entry> = (1..=last).map(|i| set(1, &format!("k{i}"))).collect();
    log.append(1, &entries).unwrap();
    log.commit_to(commit).unwrap();
    log
}

fn ask(after: u64, after_epoch: u64, max_bytes: u64) -> DrMsg {
    DrMsg::Ask {
        after,
        after_epoch,
        max_bytes,
        rpo_target_ms: 300_000,
    }
}

#[test]
fn ask____entries_past_the_commit_index____only_committed_ones_shipped() {
    let log = log(5, 3);
    let mut shipper = Shipper::new(Instant::now());

    let answer = shipper.answer(&log, true, ask(1, 1, 1 << 20), Instant::now());

    assert_eq!(
        answer.unwrap(),
        Some(DrMsg::Entries {
            standby: true,
            commit: 3,
            after: 1,
            entries: vec![set(1, "k2"), set(1, "k3")],
        })
    );
}

#[test]
fn ask____more_than_max_bytes____cut_to_about_that() {
    let log = log(10, 10);
    let mut shipper = Shipper::new(Instant::now());
    let one = u64::try_from(set(1, "k1").size()).unwrap();

    let answer = shipper.answer(&log, false, ask(0, 0, 3 * one), Instant::now());

    let Some(DrMsg::Entries { entries, .. }) = answer.unwrap() else {
        panic!("not entries");
    };
    assert_eq!(entries.len(), 3);
}

#[test]
fn ask____a_probe____the_commit_index_alone() {
    let log = log(5, 5);
    let mut shipper = Shipper::new(Instant::now());

    let answer = shipper.answer(&log, false, ask(2, 1, 0), Instant::now());

    assert_eq!(
        answer.unwrap(),
        Some(DrMsg::Entries {
            standby: false,
            commit: 5,
            after: 2,
            entries: Vec::new(),
        })
    );
}

#[test]
fn ask____after_compacted_entries____behind() {
    let mut log = log(5, 5);
    log.compact(4).unwrap();
    let mut shipper = Shipper::new(Instant::now());

    let answer = shipper.answer(&log, true, ask(2, 1, 1 << 20), Instant::now());

    assert_eq!(
        answer.unwrap(),
        Some(DrMsg::Behind {
            standby: true,
            commit: 5,
            after: 2,
        })
    );
}

#[test]
fn ask____entry_of_another_epoch____mismatch() {
    let log = log(5, 5);
    let mut shipper = Shipper::new(Instant::now());

    let answer = shipper.answer(&log, true, ask(3, 2, 1 << 20), Instant::now());

    assert_eq!(
        answer.unwrap(),
        Some(DrMsg::Mismatch {
            commit: 5,
            after: 3
        })
    );
}

#[test]
fn ask____dr_node_ahead_of_this_node____nothing_and_not_checked() {
    let log = log(3, 2);
    let mut shipper = Shipper::new(Instant::now());

    let answer = shipper.answer(&log, true, ask(4, 7, 1 << 20), Instant::now());

    assert_eq!(
        answer.unwrap(),
        Some(DrMsg::Entries {
            standby: true,
            commit: 2,
            after: 4,
            entries: Vec::new(),
        })
    );
}

#[test]
fn lag____until_an_ask_finds_it_caught_up____grows_then_zero() {
    let log = log(5, 5);
    let start = Instant::now();
    let mut shipper = Shipper::new(start);
    let later = start + Duration::from_secs(10);

    shipper.answer(&log, true, ask(2, 1, 0), later).unwrap();
    let behind = shipper.lag(later);
    shipper.answer(&log, true, ask(5, 1, 0), later).unwrap();
    let caught_up = shipper.lag(later);

    assert_eq!(behind, Duration::from_secs(10));
    assert_eq!(caught_up, Duration::ZERO);
}

#[test]
fn ask_snapshot____chunks_of_the_one_kept____then_a_new_one_from_chunk_0() {
    let mut log = log(20, 20);
    log.compact(20).unwrap();
    let mut shipper = Shipper::new(Instant::now());
    let now = Instant::now();

    let first = shipper
        .answer(&log, true, DrMsg::AskSnapshot { index: 0, chunk: 0 }, now)
        .unwrap();
    let again = shipper
        .answer(
            &log,
            true,
            DrMsg::AskSnapshot {
                index: 20,
                chunk: 0,
            },
            now,
        )
        .unwrap();
    let stale = shipper
        .answer(&log, true, DrMsg::AskSnapshot { index: 9, chunk: 3 }, now)
        .unwrap();

    let Some(DrMsg::Snapshot {
        index,
        index_epoch,
        chunk,
        last,
        buckets,
    }) = first
    else {
        panic!("not a snapshot");
    };
    assert_eq!((index, index_epoch, chunk, last), (20, 1, 0, true));
    assert_eq!(buckets[0].entries.len(), 20);
    assert!(matches!(again, Some(DrMsg::Snapshot { chunk: 0, .. })));
    assert!(matches!(
        stale,
        Some(DrMsg::Snapshot {
            index: 20,
            chunk: 0,
            ..
        })
    ));
}

#[test]
fn answer____a_pair_node_s_own_answer____ignored() {
    let log = log(1, 1);
    let mut shipper = Shipper::new(Instant::now());

    let answer = shipper.answer(
        &log,
        true,
        DrMsg::Mismatch {
            commit: 1,
            after: 0,
        },
        Instant::now(),
    );

    assert_eq!(answer.unwrap(), None);
}

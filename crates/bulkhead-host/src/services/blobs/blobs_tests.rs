#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Blob, EXPIRED, FREED, Table};
use crate::metrics::AppStats;
use crate::pid::Pid;

fn blob(app: &str, size: u64, shared: &[&str], expires: Option<Instant>) -> Blob {
    let fd = std::fs::File::open("/dev/null").unwrap().into();
    Blob {
        size,
        app: app.into(),
        maker: Pid {
            node: 1,
            id: 1,
            incarnation: 1,
        },
        sealed: true,
        expires,
        shared: shared.iter().map(|s| s.to_string()).collect(),
        released: BTreeSet::new(),
        fd,
        stats: Arc::new(AppStats::default()),
    }
}

fn table(blobs: Vec<Blob>) -> Table {
    let mut t = Table::default();
    for b in blobs {
        t.next += 1;
        *t.bytes.entry(b.app.clone()).or_insert(0) += b.size;
        t.blobs.insert(t.next, b);
    }
    t
}

#[test]
fn Blob____released_by_its_app_alone____not_done_while_shared_apps_hold_it() {
    let mut b = blob("w", 1, &["r1", "r2"], None);

    b.released.insert("w".into());
    let writer_only = b.done();
    b.released.insert("r1".into());
    let one_reader = b.done();
    b.released.insert("r2".into());

    assert!(!writer_only);
    assert!(!one_reader);
    assert!(b.done());
}

#[test]
fn Table____sweep____frees_the_expired_and_counts_their_bytes_off() {
    let now = Instant::now();
    let mut t = table(vec![
        blob("w", 10, &[], Some(now - Duration::from_millis(1))),
        blob("w", 20, &[], Some(now + Duration::from_secs(60))),
        blob("w", 40, &[], None),
    ]);

    t.sweep(now);

    assert_eq!(t.blobs.len(), 2);
    assert_eq!(t.bytes.get("w"), Some(&60));
}

#[test]
fn Table____free_the_last____forgets_the_app() {
    let mut t = table(vec![blob("w", 10, &[], None)]);

    t.free(1, FREED);
    t.free(1, EXPIRED);

    assert!(t.blobs.is_empty());
    assert_eq!(t.bytes.get("w"), None);
}

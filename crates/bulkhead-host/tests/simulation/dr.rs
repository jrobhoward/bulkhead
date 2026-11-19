//! Shipping the replicated log to a disaster-recovery node (ROADMAP Phase
//! 8, DR2): an HA pair (nodes 1 and 2) with a witness (node 3) that also
//! keeps the DR copy, or a DR node of its own (node 4), over a WAN with
//! delay and loss. Against spike S38's properties for shipping: the copy
//! holds committed writes only, in the pair's order (`DrPrefix`,
//! `DrNoConflict`), it converges once writes stop, and the pair never
//! waits for it.
//!
//! The DR node configures the pair's bucket too, only so that a test
//! instance there can read its copy; it takes no writes.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bulkhead_bundle::manifest::Durability;
use bulkhead_host::failover::{Arbiter, FailoverConfig, FailoverHandle, Standing, WitnessConfig};
use bulkhead_host::transport::memory::Faults;
use bulkhead_host::{DrConfig, InstanceHandle, RuntimeConfig, SharedBucket};

use crate::common::ask;
use crate::harness::{Sim, pass, simulate_with};

const LEASE: Duration = Duration::from_millis(1000);
const MARGIN: Duration = Duration::from_millis(200);
const BUCKET: &str = "points";

/// A data directory and the replicated bucket on every node.
fn configure(root: &Path) -> impl Fn(u32, &mut RuntimeConfig) + '_ {
    move |node, config| {
        config.data_dir = Some(root.join(node.to_string()));
        config.buckets = BTreeMap::from([(
            BUCKET.to_string(),
            SharedBucket {
                durability: Durability::Replicated,
                quota: None,
                read_concurrency: false,
                publish: None,
            },
        )]);
    }
}

/// The WAN between site 1 (the pair) and `dr`: delay, jitter and loss
/// each way.
fn lossy_wan(sim: &Sim, dr: u32) {
    let wan = Faults {
        delay: Duration::from_millis(50),
        jitter: Duration::from_millis(20),
        drop: 0.05,
    };
    for n in 1..=2 {
        sim.mesh.set_faults(n, dr, wan);
        sim.mesh.set_faults(dr, n, wan);
    }
}

/// Serve the witness on node 3, start failover on 1 and 2 naming `dr` as
/// their DR node (log bounds `log_bytes`), and the DR copy on `dr`.
fn start(sim: &Sim, dr: u32, log_bytes: u64) -> Vec<FailoverHandle> {
    let mut witness = WitnessConfig::new([1, 2], 1);
    witness.lease = LEASE;
    witness.margin = MARGIN;
    sim.node(3).rt.serve_witness(witness).unwrap();
    let mut copy = DrConfig::new([1, 2]);
    copy.poll = Duration::from_millis(200);
    copy.rpo_target = Duration::from_secs(5);
    sim.node(dr).rt.start_dr(copy).unwrap();
    (1..=2)
        .map(|node| {
            let mut cfg = FailoverConfig::new(3 - node, Arbiter::Witness(3));
            cfg.lease = LEASE;
            cfg.margin = MARGIN;
            cfg.dr = Some(dr);
            cfg.replication_log_bytes = log_bytes;
            sim.node(node).rt.start_failover(cfg).unwrap()
        })
        .collect()
}

/// The pair node primary with its lease, once it committed its no-op.
async fn primary(sim: &Sim) -> u32 {
    for _ in 0..200 {
        for node in 1..=2 {
            if let Some(s) = sim.node(node).rt.replication_status()
                && s.standing == Standing::Primary
                && s.commit > 0
                && s.commit == s.last
            {
                return node;
            }
        }
        pass(50).await;
    }
    panic!("no primary");
}

/// Wait until the DR copy holds node `p`'s commit index.
async fn caught_up(sim: &Sim, dr: u32, p: u32) {
    for _ in 0..400 {
        let pair = sim.node(p).rt.replication_status().unwrap().commit;
        if sim.node(dr).rt.dr_status().unwrap().commit == pair {
            return;
        }
        pass(50).await;
    }
    panic!(
        "the DR copy did not catch up: {:?} against {:?}",
        sim.node(dr).rt.dr_status(),
        sim.node(p).rt.replication_status()
    );
}

async fn set(kv: &InstanceHandle, key: &str) -> String {
    ask(kv, &format!("set:{BUCKET}:{key}:v")).await
}

async fn keys(sim: &Sim, node: u32) -> String {
    ask(&sim.node(node).spawn_kv(BUCKET), &format!("keys:{BUCKET}")).await
}

/// The keys `k0` .. `k{n-1}`, sorted as the bucket lists them.
fn written(n: usize) -> String {
    let mut k: Vec<String> = (0..n).map(|i| format!("k{i:02}")).collect();
    k.sort();
    k.join(",")
}

/// While writes go on over a lossy WAN, the copy only ever holds a prefix
/// of what the primary committed; once they stop it holds all of it.
#[test]
fn dr____writes_over_a_lossy_wan____copy_a_prefix_then_every_write() {
    let root = tempfile::tempdir().unwrap();
    let (acked, prefixes, copy, on_primary, lag) =
        simulate_with(201, 3, configure(root.path()), |sim| async move {
            lossy_wan(&sim, 3);
            let _handles = start(&sim, 3, 64 << 20);
            let p = primary(&sim).await;
            let kv = sim.node(p).spawn_kv(BUCKET);
            let mut acked = Vec::new();
            let mut prefixes = Vec::new();
            for i in 0..30 {
                acked.push(set(&kv, &format!("k{i:02}")).await);
                if i % 5 == 4 {
                    let held = sim.node(3).rt.dr_status().unwrap().commit;
                    let committed = sim.node(p).rt.replication_status().unwrap().commit;
                    prefixes.push(held <= committed);
                }
            }
            caught_up(&sim, 3, p).await;
            pass(500).await;
            let lag = sim.node(3).rt.dr_status().unwrap().lag;
            (
                acked,
                prefixes,
                keys(&sim, 3).await,
                keys(&sim, p).await,
                lag,
            )
        });

    assert!(acked.iter().all(|r| r == "ok"), "{acked:?}");
    assert!(prefixes.iter().all(|&p| p), "{prefixes:?}");
    assert_eq!(on_primary, written(30));
    assert_eq!(copy, on_primary);
    assert!(lag < Duration::from_secs(1), "{lag:?}");
}

/// With the WAN cut, the pair writes as fast as before and the copy's lag
/// grows past its target; healed, the copy catches up and the lag drops.
#[test]
fn dr____wan_cut____pair_does_not_wait_and_lag_grows_then_drops() {
    let root = tempfile::tempdir().unwrap();
    let (cut_writes, lag_cut, pair_lag, copy, on_primary, lag_after) =
        simulate_with(202, 3, configure(root.path()), |sim| async move {
            let _handles = start(&sim, 3, 64 << 20);
            let p = primary(&sim).await;
            let kv = sim.node(p).spawn_kv(BUCKET);
            set(&kv, "k00").await;
            caught_up(&sim, 3, p).await;

            for n in 1..=2 {
                sim.mesh.partition(n, 3);
            }
            let started = tokio::time::Instant::now();
            for i in 1..20 {
                assert_eq!(set(&kv, &format!("k{i:02}")).await, "ok");
            }
            let cut_writes = started.elapsed();
            pass(8_000).await;
            let lag_cut = sim.node(3).rt.dr_status().unwrap().lag;
            let pair_lag = sim.node(p).rt.dr_lag().unwrap();
            for n in 1..=2 {
                sim.mesh.heal(n, 3);
            }
            caught_up(&sim, 3, p).await;
            pass(500).await;
            let lag_after = sim.node(3).rt.dr_status().unwrap().lag;
            (
                cut_writes,
                lag_cut,
                pair_lag,
                keys(&sim, 3).await,
                keys(&sim, p).await,
                lag_after,
            )
        });

    assert!(cut_writes < Duration::from_secs(1), "{cut_writes:?}");
    assert!(lag_cut > Duration::from_secs(8), "{lag_cut:?}");
    assert!(pair_lag > Duration::from_secs(8), "{pair_lag:?}");
    assert_eq!(on_primary, written(20));
    assert_eq!(copy, on_primary);
    assert!(lag_after < Duration::from_secs(1), "{lag_after:?}");
}

/// Cut off while the pair compacts past what it holds, the copy catches
/// up from a snapshot.
#[test]
fn dr____behind_the_pair_s_retention____caught_up_from_a_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let (base, copy, on_primary) =
        simulate_with(203, 3, configure(root.path()), |sim| async move {
            let _handles = start(&sim, 3, 256);
            let p = primary(&sim).await;
            let kv = sim.node(p).spawn_kv(BUCKET);
            set(&kv, "k00").await;
            caught_up(&sim, 3, p).await;
            for n in 1..=2 {
                sim.mesh.partition(n, 3);
            }
            for i in 1..40 {
                assert_eq!(set(&kv, &format!("k{i:02}")).await, "ok");
            }
            pass(2_000).await;
            for n in 1..=2 {
                sim.mesh.heal(n, 3);
            }
            caught_up(&sim, 3, p).await;
            let base = sim.node(3).rt.dr_status().unwrap().base;
            (base, keys(&sim, 3).await, keys(&sim, p).await)
        });

    assert!(base > 1, "the copy's base: {base}");
    assert_eq!(on_primary, written(40));
    assert_eq!(copy, on_primary);
}

/// The primary is lost: the standby takes over with every acknowledged
/// write, and the copy goes on from it, the writes after the failover
/// included.
#[test]
fn dr____primary_lost____copy_follows_the_new_primary() {
    let root = tempfile::tempdir().unwrap();
    let (before, after, copy, on_new) =
        simulate_with(204, 3, configure(root.path()), |sim| async move {
            lossy_wan(&sim, 3);
            let _handles = start(&sim, 3, 64 << 20);
            let p = primary(&sim).await;
            let kv = sim.node(p).spawn_kv(BUCKET);
            for i in 0..10 {
                assert_eq!(set(&kv, &format!("k{i:02}")).await, "ok");
            }
            for other in (1..=3).filter(|&o| o != p) {
                sim.mesh.partition(p, other);
            }
            let s = 3 - p;
            let mut before = 0;
            for _ in 0..200 {
                pass(50).await;
                if primary_of(&sim, s) {
                    before = sim.node(3).rt.dr_status().unwrap().commit;
                    break;
                }
            }
            let kv_s = sim.node(s).spawn_kv(BUCKET);
            for i in 10..20 {
                assert_eq!(set(&kv_s, &format!("k{i:02}")).await, "ok");
            }
            caught_up(&sim, 3, s).await;
            let after = sim.node(3).rt.dr_status().unwrap();
            (before, after, keys(&sim, 3).await, keys(&sim, s).await)
        });

    assert!(before > 0);
    assert!(after.commit > before);
    assert_eq!(on_new, written(20));
    assert_eq!(copy, on_new);
}

fn primary_of(sim: &Sim, node: u32) -> bool {
    sim.node(node)
        .rt
        .replication_status()
        .is_some_and(|s| s.standing == Standing::Primary && s.ready)
}

/// A DR node apart from the witness ships alike.
#[test]
fn dr____node_of_its_own____copy_holds_every_write() {
    let root = tempfile::tempdir().unwrap();
    let (copy, on_primary) = simulate_with(205, 4, configure(root.path()), |sim| async move {
        lossy_wan(&sim, 4);
        let _handles = start(&sim, 4, 64 << 20);
        let p = primary(&sim).await;
        let kv = sim.node(p).spawn_kv(BUCKET);
        for i in 0..15 {
            assert_eq!(set(&kv, &format!("k{i:02}")).await, "ok");
        }
        caught_up(&sim, 4, p).await;
        (keys(&sim, 4).await, keys(&sim, p).await)
    });

    assert_eq!(on_primary, written(15));
    assert_eq!(copy, on_primary);
}

/// The node's own DR messages (ROADMAP DR3) pass both ways between the DR
/// node and a pair node, and not to a node outside them.
#[test]
fn dr_deploys____between_dr_node_and_pair____both_ways_and_no_other() {
    let root = tempfile::tempdir().unwrap();
    let (to_pair, to_dr, to_other, again) =
        simulate_with(206, 4, configure(root.path()), |sim| async move {
            let _handles = start(&sim, 3, 64 << 20);
            primary(&sim).await;
            let (dr_tx, mut dr_rx) = sim.node(3).rt.dr_deploys().unwrap();
            let (pair_tx, mut pair_rx) = sim.node(1).rt.dr_deploys().unwrap();
            let again = sim.node(1).rt.dr_deploys().is_none();
            assert!(dr_tx.send(1, b"ask".to_vec()));
            let to_pair = pair_rx.recv().await.unwrap();
            assert!(pair_tx.send(3, b"answer".to_vec()));
            let to_dr = dr_rx.recv().await.unwrap();
            let to_other = dr_tx.send(4, b"ask".to_vec());
            (to_pair, to_dr, to_other, again)
        });

    assert_eq!(to_pair, (3, b"ask".to_vec()));
    assert_eq!(to_dr, (1, b"answer".to_vec()));
    assert!(!to_other);
    assert!(again);
}

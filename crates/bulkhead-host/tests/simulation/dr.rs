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
/// each way. The loss is of whole frames on a live session, which QUIC's
/// retransmission rules out on a real link: an ask lost so waits for the
/// backstop (`ship::CHUNK_TIMEOUT`, 60 s), so the waits below allow
/// minutes of the paused clock.
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

/// As [`start`], with the DR node on node 4 apart from the witness on
/// node 3 (ROADMAP DR4b): the witness names it, and it follows the witness.
fn start_apart(sim: &Sim) -> Vec<FailoverHandle> {
    let mut witness = WitnessConfig::new([1, 2], 1);
    witness.lease = LEASE;
    witness.margin = MARGIN;
    witness.dr = Some(4);
    sim.node(3).rt.serve_witness(witness).unwrap();
    let mut copy = DrConfig::new([1, 2]);
    copy.poll = Duration::from_millis(200);
    copy.rpo_target = Duration::from_secs(5);
    copy.witness = Some(3);
    sim.node(4).rt.start_dr(copy).unwrap();
    (1..=2)
        .map(|node| {
            let mut cfg = FailoverConfig::new(3 - node, Arbiter::Witness(3));
            cfg.lease = LEASE;
            cfg.margin = MARGIN;
            cfg.dr = Some(4);
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
    for _ in 0..4_000 {
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
            caught_up(&sim, 3, p).await;
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

/// Site 1 lost (each pair node cut off from everything) over a lossy WAN:
/// the witness, as DR node, is activated (ROADMAP DR4); it keeps exactly
/// its copy (S38's `NoLostWriteDr`) and takes writes of its own. A DR
/// node apart from the witness refuses (DR4b).
#[test]
fn dr____site_1_lost_then_activated____copy_kept_and_writes_taken() {
    use bulkhead_host::failover::ActivateRefused;
    let root = tempfile::tempdir().unwrap();
    let (copy_before, epoch, after, written_here, apart) =
        simulate_with(206, 4, configure(root.path()), |sim| async move {
            lossy_wan(&sim, 3);
            let _handles = start(&sim, 3, 64 << 20);
            let mut own = DrConfig::new([1, 2]);
            own.poll = Duration::from_millis(200);
            sim.node(4).rt.start_dr(own).unwrap();
            let p = primary(&sim).await;
            let kv = sim.node(p).spawn_kv(BUCKET);
            for i in 0..10 {
                assert_eq!(set(&kv, &format!("k{i:02}")).await, "ok");
            }
            caught_up(&sim, 3, p).await;
            for (a, b) in [(1, 2), (1, 3), (2, 3), (1, 4), (2, 4)] {
                sim.mesh.partition(a, b);
            }
            let copy_before = keys(&sim, 3).await;

            let epoch = sim.node(3).rt.dr_activate().await.unwrap();
            let kv3 = sim.node(3).spawn_kv(BUCKET);
            let written_here = set(&kv3, "site2").await;
            let after = keys(&sim, 3).await;
            let apart = sim.node(4).rt.dr_activate().await.unwrap_err();
            (copy_before, epoch, after, written_here, apart)
        });

    assert_eq!(copy_before, written(10));
    assert!(epoch >= 2, "{epoch}");
    assert_eq!(written_here, "ok");
    assert_eq!(after, format!("{},site2", written(10)));
    assert!(matches!(apart, ActivateRefused::NotHere(_)), "{apart:?}");
}

/// Failback over a lossy WAN (ROADMAP DR4): after an activation and a
/// write at site 2, site 1 comes back; its nodes are stale (reads of the
/// replicated bucket refused, S38's `StaleInert`), reseed from the DR
/// node, and once both have, it hands back to node 1, which then holds
/// exactly the DR node's copy and leads again.
#[test]
fn dr____site_1_back_after_activation____reseeded_and_handed_back() {
    let root = tempfile::tempdir().unwrap();
    let (stale_read, epoch, on_1, on_dr) =
        simulate_with(207, 3, configure(root.path()), |sim| async move {
            lossy_wan(&sim, 3);
            let _handles = start(&sim, 3, 64 << 20);
            let p = primary(&sim).await;
            let kv = sim.node(p).spawn_kv(BUCKET);
            for i in 0..10 {
                assert_eq!(set(&kv, &format!("k{i:02}")).await, "ok");
            }
            caught_up(&sim, 3, p).await;
            for (a, b) in [(1, 2), (1, 3), (2, 3)] {
                sim.mesh.partition(a, b);
            }
            sim.node(3).rt.dr_activate().await.unwrap();
            let kv3 = sim.node(3).spawn_kv(BUCKET);
            assert_eq!(set(&kv3, "site2").await, "ok");
            sim.mesh.heal_all();
            lossy_wan(&sim, 3);
            let mut stale_read = String::new();
            for _ in 0..4_000 {
                pass(50).await;
                let reseeded = (1..=2).all(|n| {
                    sim.node(n)
                        .rt
                        .replication_status()
                        .is_some_and(|s| s.stale == Some(3) && s.reseeded)
                });
                if stale_read.is_empty()
                    && sim
                        .node(1)
                        .rt
                        .replication_status()
                        .is_some_and(|s| s.stale.is_some())
                {
                    stale_read = keys(&sim, 1).await;
                }
                if reseeded {
                    break;
                }
            }
            let epoch = sim
                .node(3)
                .rt
                .dr_failback(1, Duration::from_secs(180))
                .await
                .unwrap();
            for _ in 0..400 {
                if primary_of(&sim, 1) {
                    break;
                }
                pass(50).await;
            }
            (stale_read, epoch, keys(&sim, 1).await, keys(&sim, 3).await)
        });

    assert_eq!(stale_read, "err:unavailable");
    assert!(epoch >= 3, "{epoch}");
    assert_eq!(on_1, format!("{},site2", written(10)));
    assert_eq!(on_dr, on_1);
}

/// A DR node apart from the witness (ROADMAP DR4b), every link to site 2
/// lossy, its own to the witness too: activated through the witness, it
/// takes writes; site 1 back, both its nodes reseed from it and it hands
/// back through the witness to node 1, which then holds exactly its copy
/// and leads, and the DR node no longer does.
#[test]
fn dr____node_of_its_own____activated_and_handed_back_through_the_witness() {
    let root = tempfile::tempdir().unwrap();
    let (activated, epoch, on_1, on_dr, after) =
        simulate_with(208, 4, configure(root.path()), |sim| async move {
            lossy_wan(&sim, 4);
            let wan = Faults {
                delay: Duration::from_millis(30),
                jitter: Duration::from_millis(10),
                drop: 0.05,
            };
            sim.mesh.set_faults(3, 4, wan);
            sim.mesh.set_faults(4, 3, wan);
            let _handles = start_apart(&sim);
            let p = primary(&sim).await;
            let kv = sim.node(p).spawn_kv(BUCKET);
            for i in 0..10 {
                assert_eq!(set(&kv, &format!("k{i:02}")).await, "ok");
            }
            caught_up(&sim, 4, p).await;
            // Site 1 lost: cut off from everything.
            for (a, b) in [(1, 2), (1, 3), (1, 4), (2, 3), (2, 4)] {
                sim.mesh.partition(a, b);
            }
            let activated = sim.node(4).rt.dr_activate().await.unwrap();
            let kv4 = sim.node(4).spawn_kv(BUCKET);
            assert_eq!(set(&kv4, "site2").await, "ok");
            sim.mesh.heal_all();
            lossy_wan(&sim, 4);
            sim.mesh.set_faults(3, 4, wan);
            sim.mesh.set_faults(4, 3, wan);
            for _ in 0..4_000 {
                pass(50).await;
                if (1..=2).all(|n| {
                    sim.node(n)
                        .rt
                        .replication_status()
                        .is_some_and(|s| s.stale == Some(4) && s.reseeded)
                }) {
                    break;
                }
            }
            let epoch = sim
                .node(4)
                .rt
                .dr_failback(1, Duration::from_secs(180))
                .await
                .unwrap();
            for _ in 0..400 {
                if primary_of(&sim, 1) {
                    break;
                }
                pass(50).await;
            }
            let after = sim.node(4).rt.dr_active();
            (
                activated,
                epoch,
                keys(&sim, 1).await,
                keys(&sim, 4).await,
                after,
            )
        });

    assert!(activated >= 2, "{activated}");
    assert_eq!(epoch, activated + 1);
    assert_eq!(on_1, format!("{},site2", written(10)));
    assert_eq!(on_dr, on_1);
    assert_eq!(after, None);
}

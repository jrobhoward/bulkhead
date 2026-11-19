#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

/// A witness: it runs no apps, so no resolver of host names to build for
/// tests of the change lock alone.
async fn node(dir: &tempfile::TempDir) -> Node {
    let toml = format!(
        "data_dir = {:?}\nmax_instances = 8\n[cluster]\nrole = \"witness\"\n",
        dir.path().join("data")
    );
    Node::open(NodeConfig::from_toml_str(&toml).unwrap(), LogRing::new(16))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads____change_in_progress____answer_without_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(&dir).await;
    let change = node.change.lock().await;

    let list = node.list();
    let status = node.status("absent");
    let logs = node.logs("absent", 10);

    drop(change);
    assert!(list.is_empty());
    assert!(matches!(status, Err(NodeError::NotFound(_))));
    assert!(matches!(logs, Err(NodeError::NotFound(_))));
    node.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changes____one_in_progress____the_next_waits_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let node = node(&dir).await;
    let change = node.change.lock().await;

    let stop =
        tokio::time::timeout(std::time::Duration::from_millis(200), node.stop("absent")).await;

    drop(change);
    assert!(stop.is_err(), "stop ran during another change");
    assert!(matches!(
        node.stop("absent").await,
        Err(NodeError::NotFound(_))
    ));
    node.shutdown().await;
}

#[test]
fn dr_metrics____pair_node_and_dr_node____lag_and_the_copy_s_index() {
    let pair = dr_metrics(Some(Duration::from_millis(2500)), None);
    let dr = dr_metrics(Some(Duration::from_secs(1)), Some(77));
    let none = dr_metrics(None, None);

    assert!(pair.contains("\nbulkhead_dr_lag_seconds 2.500\n"), "{pair}");
    assert!(!pair.contains("commit_index"), "{pair}");
    assert!(dr.contains("\nbulkhead_dr_commit_index 77\n"), "{dr}");
    assert!(
        dr.contains("# TYPE bulkhead_dr_lag_seconds gauge\n"),
        "{dr}"
    );
    assert!(none.is_empty());
}

#[test]
fn replication_metrics____primary____indexes_lag_and_alone() {
    let primary = replication_metrics(40, 42, Some(30), true);
    let standby = replication_metrics(40, 40, None, false);

    assert!(
        primary.contains("\nbulkhead_replication_commit_index 40\n"),
        "{primary}"
    );
    assert!(
        primary.contains("\nbulkhead_replication_partner_lag_entries 12\n"),
        "{primary}"
    );
    assert!(
        primary.contains("\nbulkhead_replication_alone 1\n"),
        "{primary}"
    );
    assert!(
        primary.contains("# TYPE bulkhead_replication_last_index gauge\n"),
        "{primary}"
    );
    assert!(!standby.contains("lag"), "{standby}");
}

#[test]
fn cluster_metrics____two_peers____queued_bytes_and_refused_per_node() {
    let mut a = bulkhead_host::transport::LaneStats::default();
    a.queued_bytes = 512;
    a.refused = 3;
    let b = bulkhead_host::transport::LaneStats::default();

    let text = cluster_metrics(&[(2, a), (3, b)]);

    assert!(
        text.contains("bulkhead_cluster_send_queue_bytes{node=\"2\"} 512\n"),
        "{text}"
    );
    assert!(
        text.contains("bulkhead_cluster_send_refused_total{node=\"2\"} 3\n"),
        "{text}"
    );
    assert!(
        text.contains("bulkhead_cluster_send_refused_total{node=\"3\"} 0\n"),
        "{text}"
    );
    assert!(
        text.contains("# TYPE bulkhead_cluster_send_queue_bytes gauge\n"),
        "{text}"
    );
}

#[test]
fn dr_apps_metrics____held_and_not_ready____both_gauges() {
    let text = dr_apps_metrics(3, 1);

    assert!(text.contains("\nbulkhead_dr_apps 3\n"), "{text}");
    assert!(text.contains("\nbulkhead_dr_apps_not_ready 1\n"), "{text}");
    assert!(text.contains("# TYPE bulkhead_dr_apps_not_ready gauge"));
}

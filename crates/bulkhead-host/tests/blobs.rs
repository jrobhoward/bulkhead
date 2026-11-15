//! Blobs between apps (`bulkhead:blobs`, ROADMAP BL1), through the native
//! fixture's `blob-*` commands: a writer fills a blob the node made, the
//! node seals it, apps it is shared with open it read-only; the node frees
//! it once every app released it, at its expiry, or (unsealed) when its
//! instance ends.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use bulkhead_bundle::manifest::NativeProgram;
use bulkhead_host::{AppDeployment, Apps, InstanceHandle, NativeCode, Runtime, RuntimeConfig};
use common::{app_manifest, ask, native_config, native_fixtures};
use serde_json::json;

const MIB: u64 = 1 << 20;

/// A runtime with `writer` (4 MiB of blobs), `reader`, `outsider` (which
/// accepts messages from no one the writer is) and `plain` (no blobs).
struct Node {
    rt: Runtime,
    apps: Apps,
}

fn manifest(blobs: Option<serde_json::Value>, acl: serde_json::Value) -> serde_json::Value {
    let mut capabilities = json!({});
    if let Some(b) = blobs {
        capabilities["bulkhead:blobs"] = b;
    }
    json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "messaging_acl": acl,
        "capabilities": capabilities,
        "supervision": { "intensity": 10, "backoff": { "initial_ms": 1, "max_ms": 20 } },
        "limits": { "callback_deadline_ms": 5000, "memory_max": "64MiB" }
    })
}

async fn node() -> Node {
    let mut blobs = bulkhead_host::Blobs::default();
    blobs.max_blob = 2 * MIB;
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native: native_config(),
        blobs,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let code = rt.load_native(NativeCode {
        path: native_fixtures().1.clone(),
        kind: NativeProgram::Executable,
    });
    let deploy = |name: &str, spec: serde_json::Value| {
        AppDeployment::new(app_manifest(name, spec), code.clone())
    };
    let apps = rt
        .start_apps(vec![
            deploy(
                "writer",
                manifest(Some(json!({ "max_bytes": "4MiB" })), json!({})),
            ),
            deploy("reader", manifest(Some(json!({})), json!({}))),
            deploy(
                "outsider",
                manifest(Some(json!({})), json!({ "accept_from": ["reader"] })),
            ),
            deploy("plain", manifest(None, json!({}))),
        ])
        .await
        .unwrap();
    Node { rt, apps }
}

impl Node {
    fn one(&self, app: &str) -> InstanceHandle {
        self.apps.app(app).unwrap().instances().remove(0)
    }

    async fn ask(&self, app: &str, command: &str) -> String {
        ask(&self.one(app), command).await
    }

    /// A blob of `size` bytes from `writer`, filled with `text` and sealed;
    /// its id.
    async fn sealed(&self, size: u64, text: &str, ttl_ms: u64) -> String {
        let id = self.ask("writer", &format!("blob-create {size}")).await;
        assert!(id.parse::<u64>().is_ok(), "{id}");
        assert_eq!(
            self.ask("writer", &format!("blob-fill {id} {text}")).await,
            "ok"
        );
        assert_eq!(
            self.ask("writer", &format!("blob-seal {id} {ttl_ms}"))
                .await,
            "ok"
        );
        id
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob____written_sealed_shared____read_by_the_other_app() {
    let n = node().await;
    let id = n.sealed(MIB, "hello", 0).await;

    let shared = n.ask("writer", &format!("blob-share {id} reader")).await;
    let read = n.ask("reader", &format!("blob-open {id}")).await;

    assert_eq!(shared, "ok");
    assert_eq!(read, format!("{MIB} hellohellohelloh"));
    assert_eq!(n.rt.blob_usage("writer"), (1, MIB));
    let metrics = n.rt.render_metrics();
    for line in [
        "bulkhead_blobs_bytes{app=\"writer\"} 1048576",
        "bulkhead_blobs_total{app=\"writer\",outcome=\"created\"} 1",
        "bulkhead_blobs_total{app=\"writer\",outcome=\"sealed\"} 1",
    ] {
        assert!(metrics.contains(line), "{line} in {metrics}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob____unshared_outside_the_acl_or_ungranted____denied() {
    let n = node().await;
    let unsealed = n.ask("writer", "blob-create 4096").await;
    let id = n.sealed(4096, "x", 0).await;

    let before_seal = n
        .ask("writer", &format!("blob-share {unsealed} reader"))
        .await;
    let unshared = n.ask("reader", &format!("blob-open {id}")).await;
    let outside = n.ask("writer", &format!("blob-share {id} outsider")).await;
    let not_running = n.ask("writer", &format!("blob-share {id} nobody")).await;
    let ungranted = n.ask("plain", "blob-create 4096").await;
    let seal_by_other = n.ask("reader", &format!("blob-seal {unsealed}")).await;

    assert_eq!(before_seal, "err:NotSealed");
    assert_eq!(unshared, "err:Denied");
    assert_eq!(outside, "err:Denied");
    assert_eq!(not_running, "err:Denied");
    assert_eq!(ungranted, "err:Denied");
    assert_eq!(seal_by_other, "err:Denied");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob____over_the_quota_or_size____refused() {
    let n = node().await;

    let too_large = n
        .ask("writer", &format!("blob-create {}", 2 * MIB + 1))
        .await;
    let empty = n.ask("writer", "blob-create 0").await;
    let first = n.ask("writer", &format!("blob-create {}", 2 * MIB)).await;
    let second = n.ask("writer", &format!("blob-create {}", 2 * MIB)).await;
    let over = n.ask("writer", "blob-create 1").await;

    assert_eq!(too_large, "err:TooLarge");
    assert_eq!(empty, "err:TooLarge");
    assert!(first.parse::<u64>().is_ok(), "{first}");
    assert!(second.parse::<u64>().is_ok(), "{second}");
    assert_eq!(over, "err:OverQuota");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob____writable_mapping_kept____not_sealed_until_removed() {
    let n = node().await;
    let id = n.ask("writer", "blob-create 4096").await;
    n.ask("writer", &format!("blob-fill {id} y")).await;
    n.ask("writer", &format!("blob-map {id}")).await;

    let mapped = n.ask("writer", &format!("blob-seal {id}")).await;
    n.ask("writer", &format!("blob-unmap {id}")).await;
    let unmapped = n.ask("writer", &format!("blob-seal {id}")).await;

    assert_eq!(mapped, "err:Mapped");
    assert_eq!(unmapped, "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob____sealed____neither_side_can_change_it() {
    let n = node().await;
    let id = n.ask("writer", "blob-create 4096").await;
    n.ask("writer", &format!("blob-fill {id} z")).await;
    n.ask("writer", &format!("blob-seal {id}")).await;
    n.ask("writer", &format!("blob-share {id} reader")).await;
    n.ask("reader", &format!("blob-open {id}")).await;

    let writer = n.ask("writer", &format!("blob-try {id}")).await;
    let reader = n.ask("reader", &format!("blob-try {id}")).await;
    let read = n.ask("reader", &format!("blob-reread {id}")).await;

    for tried in [&writer, &reader] {
        assert!(
            tried.split_whitespace().all(|kv| !kv.ends_with("=ok")),
            "{tried}"
        );
    }
    assert_eq!(read, "zzzzzzzzzzzzzzzz");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob____released_by_every_app____freed_and_an_open_descriptor_reads_on() {
    let n = node().await;
    let id = n.sealed(4096, "kept", 0).await;
    n.ask("writer", &format!("blob-share {id} reader")).await;
    n.ask("reader", &format!("blob-open {id}")).await;

    n.ask("writer", &format!("blob-release {id}")).await;
    let after_writer = n.rt.blob_usage("writer");
    n.ask("reader", &format!("blob-release {id}")).await;
    let after_both = n.rt.blob_usage("writer");
    let reread = n.ask("reader", &format!("blob-reread {id}")).await;
    let reopen = n.ask("reader", &format!("blob-open {id}")).await;

    assert_eq!(after_writer, (1, 4096), "the reader has not released it");
    assert_eq!(after_both, (0, 0));
    assert_eq!(reread, "keptkeptkeptkept");
    assert_eq!(reopen, "err:NotFound");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob____past_its_time____freed() {
    let n = node().await;
    let id = n.sealed(4096, "t", 200).await;
    n.ask("writer", &format!("blob-share {id} reader")).await;

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let usage = n.rt.blob_usage("writer");
    let open = n.ask("reader", &format!("blob-open {id}")).await;

    assert_eq!(usage, (0, 0), "swept without a request");
    assert_eq!(open, "err:NotFound");
    assert!(
        n.rt.render_metrics()
            .contains("bulkhead_blobs_total{app=\"writer\",outcome=\"expired\"} 1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob____unsealed_when_its_instance_ends____freed() {
    let n = node().await;
    let sealed = n.sealed(4096, "s", 0).await;
    n.ask("writer", "blob-create 8192").await;
    let before = n.rt.blob_usage("writer");

    n.one("writer").kill();
    let mut after = before;
    for _ in 0..100 {
        after = n.rt.blob_usage("writer");
        if after != before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    n.ask("writer", &format!("blob-share {sealed} reader"))
        .await;
    let open = n.ask("reader", &format!("blob-open {sealed}")).await;

    assert_eq!(before, (2, 4096 + 8192));
    assert_eq!(after, (1, 4096), "the sealed one outlives its maker");
    assert_eq!(open, "4096 ssssssssssssssss");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob____from_a_sender_channel____made_and_read() {
    let n = node().await;

    let id = n.ask("writer", "sender-blob 4096 abc").await;
    let shared = n.ask("writer", &format!("blob-share {id} reader")).await;
    let read = n.ask("reader", &format!("blob-open {id}")).await;

    assert!(id.parse::<u64>().is_ok(), "{id}");
    assert_eq!(shared, "ok");
    assert_eq!(read, "4096 abcabcabcabcabca");
}

//! The app tutorial (`examples/app-tutorial`): each chapter's script run
//! straight through (`AUTO=1`), its own `expect`s the checks; and the
//! chapter 5 apps on a node started by the test, the worked example of
//! chapter 6.
//!
//! The scripts build into `target/fixtures/app-tutorial`, debug, with
//! `bash` (skipped without it). Linux and FreeBSD only, where native apps
//! run.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use bulkhead::node::Node;
use rstest::rstest;
use tempfile::TempDir;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Where the scripts and this test build: not `target/`, which the
/// running `cargo test` holds.
fn target() -> PathBuf {
    root().join("target/fixtures/app-tutorial")
}

fn has_bash() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[rstest]
#[case("01-node.sh")]
#[case("02-room.sh")]
#[case("03-room.sh")]
#[case("04-chat.sh")]
#[case("05-chat.sh")]
#[case("06-test.sh")]
#[case("07-pair.sh")]
#[case("08-dr.sh")]
fn chapter____run_straight_through____every_expectation_met(#[case] script: &str) {
    if !has_bash() {
        eprintln!("no bash: {script} skipped");
        return;
    }
    let demo = TempDir::new().unwrap();

    let out = Command::new("bash")
        .arg(root().join("examples/app-tutorial").join(script))
        .env("AUTO", "1")
        .env("DEMO_DIR", demo.path())
        .env("CARGO_TARGET_DIR", target())
        // Chapters 7 and 8's cluster ports.
        .env("PORT_1", common::ports::free_port().unwrap().to_string())
        .env("PORT_2", common::ports::free_port().unwrap().to_string())
        .env("PORT_3", common::ports::free_port().unwrap().to_string())
        .output()
        .unwrap();

    let logs: String = ["node.log", "node-1.log", "node-2.log", "node-3.log"]
        .iter()
        .filter_map(|f| {
            let text = std::fs::read_to_string(demo.path().join(f)).ok()?;
            Some(format!("--- {f}\n{text}\n"))
        })
        .collect();
    assert!(
        out.status.success(),
        "{script} failed\n--- stdout\n{}\n--- stderr\n{}\n{logs}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// The `bulkhead` CLI against the node at `socket`: (ok, stdout, stderr).
async fn bulkhead(socket: &Path, args: &[&str]) -> (bool, String, String) {
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_bulkhead"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .await
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Build the chapter 5 apps; their programs, `room` then `history`.
fn build_chapter_5() -> (PathBuf, PathBuf) {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ok = Command::new(cargo)
        .current_dir(root())
        .args([
            "build",
            "-p",
            "tut-room-5",
            "-p",
            "tut-history-5",
            "--target-dir",
        ])
        .arg(target())
        .status()
        .unwrap()
        .success();
    assert!(ok, "building the chapter 5 apps failed");
    let debug = target().join("debug");
    (debug.join("tut-room-5"), debug.join("tut-history-5"))
}

/// A node on `toml`, serving its admin API until `stop` is sent.
struct Running {
    node: Arc<Node>,
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn open(toml: &str, socket: &Path) -> Self {
        let node = Arc::new(
            Node::open(common::node_config(toml), common::ring())
                .await
                .unwrap(),
        );
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn({
            let node = node.clone();
            async move {
                let _ = bulkhead::node::serve(node, async {
                    let _ = stopped.await;
                })
                .await;
            }
        });
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Self { node, stop, server }
    }

    async fn close(self) {
        self.node.shutdown().await;
        let _ = self.stop.send(());
        let _ = self.server.await;
    }
}

/// Ask `bulkhead call room <text>` until the answer is `want`, for up to
/// five seconds; the last answer.
async fn call_until(socket: &Path, text: &str, want: &str) -> String {
    let mut got = String::new();
    for _ in 0..100 {
        got = bulkhead(socket, &["call", "room", text]).await.1;
        if got == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    got
}

/// Chapter 6's worked example: a node in a temporary directory with the
/// `posts` bucket and a policy granting both apps, the apps bundled and
/// deployed with the CLI, then a post seen through `room`'s watch and kept
/// by `history` across a restart of its instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn room_and_history____a_post____kept_in_the_bucket_and_seen_by_the_watch() {
    let (room, history) = build_chapter_5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("a.sock");
    let (ok, signer, err) = bulkhead(
        &socket,
        &["keygen", "--no-password", s(&dir.path().join("k"))],
    )
    .await;
    assert!(ok, "{err}");
    let policy = dir.path().join("policy.toml");
    std::fs::write(
        &policy,
        "[apps.room]\ngrant = [\"bulkhead:logging\", \"bulkhead:metrics\", \"bulkhead:groups\", \"bulkhead:keyvalue\"]\n\
         buckets = [\"posts\"]\n\
         [apps.history]\ngrant = [\"bulkhead:logging\", \"bulkhead:groups\", \"bulkhead:keyvalue\"]\n\
         buckets = [\"posts\"]\n",
    )
    .unwrap();
    let toml = format!(
        "data_dir = {:?}\nadmin_socket = {socket:?}\ntrusted_keys = [{signer:?}]\npolicy = {policy:?}\n\
         [buckets.posts]\ndurability = \"disk\"\n",
        dir.path().join("data")
    );
    let running = Running::open(&toml, &socket).await;
    let tutorial = root().join("examples/app-tutorial");
    let key = dir.path().join("k.key");
    for (name, manifest, program) in [
        (
            "history",
            tutorial.join("05-history/bulkhead.toml"),
            &history,
        ),
        ("room", tutorial.join("05-room/bulkhead.toml"), &room),
    ] {
        let bundle = dir.path().join(format!("{name}.rbp"));
        let (ok, _, err) = bulkhead(
            &socket,
            &[
                "bundle",
                s(&manifest),
                s(program),
                "--key",
                s(&key),
                "--out",
                s(&bundle),
            ],
        )
        .await;
        assert!(ok, "bundle {name}: {err}");
        let (ok, _, err) = bulkhead(&socket, &["deploy", s(&bundle)]).await;
        assert!(ok, "deploy {name}: {err}");
    }

    let posted = bulkhead(&socket, &["call", "room", "post hello"]).await.1;
    let seen = call_until(&socket, "seen", "1").await;
    let client = bulkhead::api::Client::new(&socket);
    let history_pid = common::ps(&client)
        .await
        .into_iter()
        .find(|i| i.app == "history")
        .unwrap()
        .pid;
    let killed = bulkhead(&socket, &["kill", &history_pid]).await.0;
    let recent = call_until(&socket, "recent", "hello").await;

    running.close().await;
    assert_eq!(posted, "posted to 1");
    assert_eq!(seen, "1");
    assert!(killed);
    assert_eq!(recent, "hello");
}

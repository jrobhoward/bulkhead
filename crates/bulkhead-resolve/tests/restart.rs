//! The driver's process killed during a lookup: the runtime's supervisor
//! starts it again, and the runtime asks again, so the lookup is
//! answered. A test binary of its own: the driver it kills is the only
//! one this process runs.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::process::Command;
use std::time::Duration;

use common::Dns;

/// This process's children running `bulkhead-resolve`: its path on Linux,
/// its name after the run-time linker's arguments on FreeBSD
/// (`ld-elf.so.1 -f FD -- bulkhead-resolve`).
fn drivers() -> Vec<u32> {
    let out = Command::new("pgrep")
        .args([
            "-P",
            &std::process::id().to_string(),
            "-f",
            "/bulkhead-resolve$|-- bulkhead-resolve$",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn driver____killed_during_a_lookup____restarted_and_the_lookup_answered() {
    let dns = Dns::start(&[("svc.test", &["192.0.2.10", "2001:db8::10"])]).await;
    let s = common::serve(&[dns.addr], "", "").await;
    // Started: a lookup has an answer.
    s.rt.resolve("warm.test", 80, Duration::from_secs(10))
        .await
        .unwrap_err();
    let first = drivers();
    assert_eq!(first.len(), 1, "{first:?}");
    dns.delay(Duration::from_millis(500));

    let rt = s.rt.clone();
    let lookup =
        tokio::spawn(async move { rt.resolve("svc.test", 80, Duration::from_secs(10)).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let killed = Command::new("kill")
        .args(["-KILL", &first[0].to_string()])
        .status()
        .unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(15), lookup)
        .await
        .unwrap()
        .unwrap();
    let now = drivers();

    assert!(killed.success());
    assert_eq!(answered.unwrap().len(), 2);
    assert!(now.iter().all(|p| *p != first[0]), "{now:?}");
}

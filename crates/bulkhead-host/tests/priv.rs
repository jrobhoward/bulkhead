//! Native instances started by the privileged helper, `bulkhead-priv`
//! (ROADMAP PS1): each app under a user of its own, in its cgroup,
//! namespaces and minimal root on Linux, its jail and `rctl` rules on
//! FreeBSD (ROADMAP FB2).
//!
//! Root only: each test starts a helper of its own and does nothing
//! without `BULKHEAD_PRIV_TESTS=1` and root (on the test machines, after
//! building as an ordinary user).

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bulkhead_bundle::manifest::{Capability, NativeProgram};
use bulkhead_host::{
    ExitReason, InstanceSpec, LoadedApp, NativeCode, NativeConfig, Runtime, RuntimeConfig,
};
use common::privhelper::{self, Started};
use common::{LONG, ask, native_fixtures, text};

fn runtime(started: &Started) -> Runtime {
    runtime_with(started, |_| {})
}

fn runtime_with(started: &Started, f: impl FnOnce(&mut NativeConfig)) -> Runtime {
    let mut native = NativeConfig::default();
    f(&mut native);
    native.runner = Some(started.runner.clone());
    native.helper = Some(started.helper.clone());
    Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        ..RuntimeConfig::default()
    })
    .unwrap()
}

/// The native fixture, copied where the helper may share it.
fn program(started: &Started, rt: &Runtime) -> LoadedApp {
    let dir = privhelper::dir_owned(&started.dir.path().join("prog"), started.node_uid, 0o755);
    let path = privhelper::copy_owned(&native_fixtures().1, &dir, started.node_uid);
    rt.load_native(NativeCode {
        path,
        kind: NativeProgram::Executable,
    })
}

fn spec(app: &str) -> InstanceSpec {
    InstanceSpec {
        memory_max: 64 << 20,
        callback_deadline: Duration::from_millis(500),
        init_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(1),
        ..InstanceSpec::new(app, "1.0.0")
    }
}

/// The helper, started for one test, or `None` (and a note) when the
/// helper's tests do not run here.
fn helper(name: &str) -> Option<Started> {
    if !privhelper::enabled() {
        privhelper::skip(name);
        return None;
    }
    Some(privhelper::start(&native_fixtures().0))
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____two_apps____each_under_a_user_of_its_own() {
    let Some(started) = helper("helper____two_apps") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let a = rt.spawn(&app, spec("alpha")).unwrap();
    let a2 = rt.spawn(&app, spec("alpha")).unwrap();
    let b = rt.spawn(&app, spec("beta")).unwrap();

    let ids = (
        ask(&a, "ids").await,
        ask(&a2, "ids").await,
        ask(&b, "ids").await,
    );
    let echo = ask(&a, "echo hello").await;
    a.shutdown(ExitReason::Shutdown);
    let reason = a.wait().await;

    // Which app starts first, and gets the first user, is a race.
    let users = [started.first_uid, started.first_uid + 1].map(|u| format!("{u} {u}"));
    assert!(users.contains(&ids.0), "{ids:?}");
    assert_eq!(ids.0, ids.1, "an app's instances share its user");
    assert!(users.contains(&ids.2) && ids.2 != ids.0, "{ids:?}");
    assert_eq!(echo, "hello");
    assert_eq!(reason, ExitReason::Shutdown);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn helper____minimal_root____the_system_hidden() {
    let Some(started) = helper("helper____minimal_root") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();

    let passwd = ask(&i, "read /etc/passwd").await;
    let pid = ask(&i, "os-pid").await;
    let node = ask(
        &i,
        &format!("read {}", started.dir.path().join("priv.conf").display()),
    )
    .await;

    assert!(passwd.starts_with("err:"), "{passwd}");
    assert_eq!(pid, "1", "the instance's PID namespace");
    assert!(node.starts_with("err:"), "{node}");
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____spins_past_deadline____killed_with_timeout() {
    let Some(started) = helper("helper____spins_past_deadline") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();
    let _ = ask(&i, "echo up").await;
    let at = Instant::now();

    let _ = i.call(text("spin"), LONG).await;
    let reason = i.wait().await;

    assert_eq!(reason, ExitReason::Timeout);
    assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____allocates_over_memory_max____crashed() {
    let Some(started) = helper("helper____allocates_over_memory_max") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();

    let small = ask(&i, "alloc 8").await;
    let _ = i.call(text("alloc 256"), LONG).await;
    let reason = i.wait().await;

    assert_eq!(small, "ok 8");
    if cfg!(target_os = "linux") {
        assert_eq!(reason, ExitReason::Crashed("memory_limit".into()));
    } else {
        assert!(matches!(reason, ExitReason::Crashed(_)), "{reason:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____volumes____written_where_writable_read_where_not() {
    let Some(started) = helper("helper____volumes") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let uid = started.node_uid;
    let base = started.dir.path();
    let rw = privhelper::dir_owned(&base.join("rw"), uid, 0o700);
    let ro = privhelper::dir_owned(&base.join("ro"), uid, 0o700);
    let table = ro.join("table.csv");
    std::fs::write(&table, b"a,b\n").unwrap();
    std::os::unix::fs::chown(&table, Some(uid), None).unwrap();
    let preopen = |host: &PathBuf, guest: &str, writable| bulkhead_host::Preopen {
        host: host.clone(),
        guest: guest.into(),
        writable,
    };
    let spec = InstanceSpec {
        granted: [Capability::Filesystem].into(),
        preopens: vec![preopen(&rw, "/data", true), preopen(&ro, "/ref", false)],
        ..spec("alpha")
    };
    // As the node does when it installs an app (ROADMAP PS2): once.
    rt.share_with_app("alpha", &rw, true).await.unwrap();
    rt.share_with_app("alpha", &ro, false).await.unwrap();
    let i = rt.spawn(&app, spec).unwrap();

    let wrote = ask(&i, "vwrite /data/new.txt").await;
    let read = ask(&i, "vcat /ref/table.csv").await;
    let write_ro = ask(&i, "vwrite /ref/table.csv").await;

    assert_eq!(wrote, "ok");
    assert_eq!(read, "a,b\n");
    assert!(write_ro.starts_with("err:"), "{write_ro}");
    let made = std::fs::metadata(rw.join("new.txt")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(made.uid(), started.first_uid, "the app's file is the app's");
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____killed____ends_promptly() {
    let Some(started) = helper("helper____killed") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();
    let pid: i32 = ask(&i, "os-pid").await.parse().unwrap_or(0);

    i.kill();
    let reason = tokio::time::timeout(Duration::from_secs(5), i.wait()).await;

    assert!(pid > 0);
    assert!(reason.is_ok(), "the kill reached the app");
}

/// A directory the node wrote, shared with `app` as the node shares an
/// app's `priv/` files, holding `secret.txt`.
async fn shared_dir(started: &Started, rt: &Runtime, name: &str, app: &str) -> PathBuf {
    let uid = started.node_uid;
    let dir = privhelper::dir_owned(&started.dir.path().join(name), uid, 0o700);
    let file = dir.join("secret.txt");
    std::fs::write(&file, b"alpha's").unwrap();
    std::os::unix::fs::chown(&file, Some(uid), None).unwrap();
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    rt.share_with_app(app, &dir, false).await.unwrap();
    dir
}

fn reading(app: &str, dir: &std::path::Path) -> InstanceSpec {
    InstanceSpec {
        granted: [Capability::Filesystem].into(),
        preopens: vec![bulkhead_host::Preopen {
            host: dir.to_path_buf(),
            guest: "/ref".into(),
            writable: false,
        }],
        ..spec(app)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn share____one_apps_files____another_apps_user_reads_nothing() {
    let Some(started) = helper("share____one_apps_files") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let dir = shared_dir(&started, &rt, "alpha-priv", "alpha").await;
    let alpha = rt.spawn(&app, reading("alpha", &dir)).unwrap();
    let beta = rt.spawn(&app, reading("beta", &dir)).unwrap();

    let own = ask(&alpha, "vcat /ref/secret.txt").await;
    let other = ask(&beta, "vcat /ref/secret.txt").await;

    assert_eq!(own, "alpha's");
    assert!(other.starts_with("err:"), "{other}");
}

#[tokio::test(flavor = "multi_thread")]
async fn share____then_starts____a_start_shares_nothing_again() {
    use std::os::unix::fs::MetadataExt;
    let Some(started) = helper("share____then_starts") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let dir = shared_dir(&started, &rt, "once", "alpha").await;
    let first = rt.spawn(&app, reading("alpha", &dir)).unwrap();
    ask(&first, "echo up").await;
    // Taken back from the app: were a start to share again, it would undo
    // this.
    let file = dir.join("secret.txt");
    std::os::unix::fs::chown(&file, None, Some(0)).unwrap();

    let second = rt.spawn(&app, reading("alpha", &dir)).unwrap();
    let read = ask(&second, "vcat /ref/secret.txt").await;

    assert_eq!(std::fs::metadata(&file).unwrap().gid(), 0);
    assert!(read.starts_with("err:"), "{read}");
}

#[tokio::test(flavor = "multi_thread")]
async fn share____a_volume____becomes_the_apps_and_sharing_again_changes_nothing() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Some(started) = helper("share____a_volume") else {
        return;
    };
    let rt = runtime(&started);
    let vol = privhelper::dir_owned(&started.dir.path().join("vol"), started.node_uid, 0o755);

    rt.share_with_app("alpha", &vol, true).await.unwrap();
    rt.share_with_app("alpha", &vol, true).await.unwrap();

    let meta = std::fs::metadata(&vol).unwrap();
    assert_eq!(meta.uid(), started.first_uid);
    assert_eq!(meta.gid(), started.first_uid);
    assert_eq!(meta.permissions().mode() & 0o7777, 0o770);
}

/// [`spec`] with time for `forks`, which waits 200 ms after forking.
fn patient(app: &str) -> InstanceSpec {
    InstanceSpec {
        callback_deadline: Duration::from_secs(5),
        ..spec(app)
    }
}

/// `key=value`'s value in `answer`.
fn field<'a>(answer: &'a str, key: &str) -> &'a str {
    answer
        .split(' ')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or("")
}

/// What `program ARGS` prints, trimmed.
#[cfg(target_os = "freebsd")]
fn output(program: &str, args: &[&str]) -> String {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[cfg(target_os = "freebsd")]
#[tokio::test(flavor = "multi_thread")]
async fn helper____jails____one_per_instance_showing_only_its_directories() {
    let Some(started) = helper("helper____jails") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let a = rt.spawn(&app, spec("alpha")).unwrap();
    let a2 = rt.spawn(&app, spec("alpha")).unwrap();
    let b = rt.spawn(&app, spec("beta")).unwrap();

    let mut jails = Vec::new();
    for i in [&a, &a2, &b] {
        let pid = ask(i, "os-pid").await;
        jails.push(output("/bin/ps", &["-o", "jid=", "-p", &pid]));
    }
    let root = output("/usr/sbin/jls", &["-j", &jails[0], "path"]);
    let mut top: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    top.sort();
    let name = output("/usr/sbin/jls", &["-j", &jails[0], "host.hostname"]);

    assert!(jails.iter().all(|j| !j.is_empty() && j != "0"), "{jails:?}");
    assert!(
        jails[0] != jails[1] && jails[1] != jails[2] && jails[0] != jails[2],
        "{jails:?}"
    );
    assert!(
        root.starts_with(started.dir.path().to_str().unwrap()),
        "{root}"
    );
    // The test's directories are under /tmp, so they show inside the
    // instance's private /tmp.
    assert_eq!(top, ["dev", "lib", "libexec", "tmp", "usr"]);
    assert_eq!(name, "bulkhead-app");
}

// FreeBSD: the jail's removal. Linux ends them with `cgroup.kill`, and
// the child's pid there is the instance's PID namespace's, which the test
// cannot look up by number.
#[cfg(target_os = "freebsd")]
#[tokio::test(flavor = "multi_thread")]
async fn helper____forked_child____ends_with_its_instance() {
    let Some(started) = helper("helper____forked_child") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, patient("alpha")).unwrap();
    let forked = ask(&i, "forks 1 0").await;
    let child: i32 = field(&forked, "last").parse().unwrap_or(0);

    i.kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), i.wait()).await;
    let pid = rustix::process::Pid::from_raw(child).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while rustix::process::test_kill_process(pid).is_ok() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(field(&forked, "alive"), "1", "{forked}");
    assert!(
        rustix::process::test_kill_process(pid).is_err(),
        "child {child} outlived its instance"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____processes_and_threads____held_at_pids_max() {
    let Some(started) = helper("helper____processes_and_threads") else {
        return;
    };
    let rt = runtime_with(&started, |n| n.pids_max = 24);
    let app = program(&started, &rt);
    let forking = rt.spawn(&app, patient("alpha")).unwrap();
    let threading = rt.spawn(&app, patient("alpha")).unwrap();

    let forks = ask(&forking, "forks 40 0").await;
    let threads = ask(&threading, "threads 40").await;

    eprintln!("forks: {forks}\nthreads: {threads}");
    let made = |a: &str| field(a, "made").parse::<u32>().unwrap_or(99);
    assert!(made(&forks) < 24, "{forks}");
    assert!(made(&threads) < 24, "{threads}");
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____processes_together_over_memory_max____refused() {
    let Some(started) = helper("helper____processes_together") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, patient("alpha")).unwrap();

    // Each child under memory_max (64 MiB), all four together over it.
    let forks = ask(&i, "forks 4 24").await;

    eprintln!("forks: {forks}");
    let alive = field(&forks, "alive").parse::<u32>().unwrap_or(99);
    assert!(alive < 4, "{forks}");
}

/// A driver app under the helper (ROADMAP DA1, PS2), as the HTTP ingress
/// runs on a node with one: under a user of its own, it accepts on the
/// listening socket the node passes and reads the file its profile gives
/// it, which the node's user owns and others may not read, as an
/// operator's key file is, once the node has shared it
/// (`share_driver_files`, at its start and before a reload); a file
/// beside it stays out of reach. A key rewritten in place is read as the
/// new one (one replaced by rename is not, until the driver restarts:
/// Landlock's rule and the FreeBSD runner's descriptor hold the old file).
#[tokio::test(flavor = "multi_thread")]
async fn helper____driver____accepts_on_its_listener_and_reads_its_file() {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    let Some(started) = helper("helper____driver") else {
        return;
    };
    let keys = privhelper::dir_owned(&started.dir.path().join("keys"), started.node_uid, 0o750);
    let file = keys.join("web.key");
    let beside = keys.join("other.key");
    for (path, body) in [(&file, "the driver's key"), (&beside, "not given")] {
        std::fs::write(path, body).unwrap();
        std::os::unix::fs::chown(path, Some(started.node_uid), Some(started.node_uid)).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    let mut native = NativeConfig::default();
    native.runner = Some(started.runner.clone());
    native.helper = Some(started.helper.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        drivers: std::collections::BTreeMap::from([(
            "drv".to_string(),
            bulkhead_host::DriverProfile {
                listeners: vec![bulkhead_host::DriverListener {
                    name: "main".into(),
                    socket: std::sync::Arc::new(socket),
                }],
                files: vec![file.clone()],
                ..bulkhead_host::DriverProfile::default()
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let app = program(&started, &rt);
    let unshared = rt.share_driver_files().await;
    let d = rt.spawn(&app, spec("drv")).unwrap();

    let ids = ask(&d, "ids").await;
    let names = ask(&d, "listeners").await;
    let client = std::thread::spawn(move || {
        let mut s = std::net::TcpStream::connect(address).unwrap();
        s.set_read_timeout(Some(LONG)).unwrap();
        let mut got = String::new();
        s.read_to_string(&mut got).unwrap();
        got
    });
    let peer = ask(&d, "accept main").await;
    let greeted = client.join().unwrap();
    let given = ask(&d, "vcat /driver/web.key").await;
    let other = ask(&d, &format!("read {}", beside.display())).await;
    std::fs::write(&file, "the renewed key").unwrap();
    let reshared = rt.share_driver_files().await;
    let renewed = ask(&d, "vcat /driver/web.key").await;

    let user = started.first_uid;
    assert_eq!(ids, format!("{user} {user}"), "a user of its own");
    assert_eq!(names, "main");
    assert!(!peer.starts_with("err:"), "{peer}");
    assert_eq!(greeted, "hello\n");
    assert!(unshared.is_empty(), "{unshared:?}");
    assert_eq!(given, "the driver's key");
    assert!(other.starts_with("err:"), "{other}");
    assert!(reshared.is_empty(), "{reshared:?}");
    assert_eq!(renewed, "the renewed key");
    d.kill();
}

/// `bulkhead-resolve`, built where the node's tests build it (as the
/// ordinary user, before a root run with `CARGO=/bin/true`).
fn resolve_program() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = root.join("target/fixtures/native");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = std::process::Command::new(cargo)
        .current_dir(&root)
        .args(["build", "-p", "bulkhead-resolve", "--target-dir"])
        .arg(&target)
        .status()
        .unwrap();
    assert!(status.success(), "building bulkhead-resolve failed");
    target.join("debug/bulkhead-resolve")
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____resolver____answers_from_hosts_and_reaches_its_nameserver_through_the_node() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(started) = helper("helper____resolver") else {
        return;
    };
    // A nameserver that accepts and hangs up: the driver's stream to it
    // is the node's to open, from outside the instance's fences.
    let nameserver = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let ns = nameserver.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = accepted.clone();
    std::thread::spawn(move || {
        for stream in nameserver.incoming() {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    let mut native = NativeConfig::default();
    native.runner = Some(started.runner.clone());
    native.helper = Some(started.helper.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        resolver: bulkhead_host::Resolver::App("bulkhead-resolve".into()),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let dir = privhelper::dir_owned(&started.dir.path().join("prog"), started.node_uid, 0o755);
    let code = rt.load_native(NativeCode {
        path: privhelper::copy_owned(&resolve_program(), &dir, started.node_uid),
        kind: NativeProgram::Executable,
    });
    // As the node's manifest for it (`node/daemon/resolve.rs`).
    let spec = serde_json::json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "lifecycle": { "start_args": {
            "nameservers": [ns.to_string()],
            "resolv_conf": "options timeout:1 attempts:1\n",
            "hosts": "10.1.2.3 db.internal\n",
        } },
        "limits": { "memory_max": "64MiB" },
        "messaging_acl": { "accept_from": ["bulkhead-resolve"], "may_call": ["bulkhead-resolve"] },
        "capabilities": { "bulkhead:sockets": {
            "connect": [{ "cidr": format!("{}/32", ns.ip()), "ports": ns.port().to_string() }],
        } },
    });
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    apps.add(bulkhead_host::AppDeployment::new(
        common::app_manifest("bulkhead-resolve", spec),
        code,
    ))
    .await
    .unwrap();

    let hosted = rt.resolve("db.internal", 5432, LONG).await;
    let elsewhere = rt.resolve("svc.test", 80, LONG).await;

    assert_eq!(hosted, Ok(vec!["10.1.2.3:5432".parse().unwrap()]));
    assert!(
        matches!(elsewhere, Err(bulkhead_host::ResolveError::Temporary(_))),
        "{elsewhere:?}"
    );
    assert!(
        accepted.load(Ordering::SeqCst) >= 1,
        "no stream reached the nameserver"
    );
    apps.stop().await;
}

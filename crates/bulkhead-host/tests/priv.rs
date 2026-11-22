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

#[tokio::test(flavor = "multi_thread")]
async fn helper____bind_a_listed_port____a_listener_that_accepts() {
    let Some(started) = helper("helper____bind_a_listed_port") else {
        return;
    };
    let addr: std::net::SocketAddr = "127.0.0.1:81".parse().unwrap();

    let listener = started.helper.bind(addr).unwrap();
    let client = std::net::TcpStream::connect(addr).unwrap();
    let (_, peer) = listener.accept().unwrap();

    assert_eq!(listener.local_addr().unwrap(), addr);
    assert_eq!(peer, client.local_addr().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn helper____bind_an_unlisted_port____refused_with_the_helpers_reason() {
    let Some(started) = helper("helper____bind_an_unlisted_port") else {
        return;
    };

    let refused = started
        .helper
        .bind("127.0.0.1:82".parse().unwrap())
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("port 82 is not one the helper binds"),
        "{refused}"
    );
}

/// A line of `/proc/<pid>/status`, its value trimmed.
#[cfg(target_os = "linux")]
fn proc_status(pid: u32, key: &str) -> String {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn helper____seccomp____its_filter_and_the_runners_on_an_instance() {
    let Some(started) = helper("helper____seccomp") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();
    let echo = ask(&i, "echo hello").await;

    let helper = (
        proc_status(started.pid(), "Seccomp"),
        proc_status(started.pid(), "Seccomp_filters"),
    );
    let children =
        std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", started.pid())).unwrap();
    let instances: Vec<String> = children
        .split_whitespace()
        .map(|pid| proc_status(pid.parse().unwrap(), "Seccomp_filters"))
        .collect();

    assert_eq!(echo, "hello");
    // Mode 2, filtering: the helper's one filter.
    assert_eq!(helper, ("2".to_string(), "1".to_string()));
    // The helper's, inherited, and the runner's three.
    assert_eq!(instances, ["4"]);
}

/// The app `loose`, which the test helper's configuration lists as
/// unconfined (ROADMAP AP4), with the node's grant.
fn unconfined(app: &str) -> InstanceSpec {
    let mut s = spec(app);
    s.granted.insert(Capability::Unconfined);
    s
}

/// An unconfined instance: no runner fences (Linux: the helper's filter
/// alone; FreeBSD: out of capability mode), the host's network and `/etc`,
/// and still its own user and PID namespace or jail.
#[tokio::test(flavor = "multi_thread")]
async fn helper____unconfined_app____own_sockets_and_etc_under_its_user() {
    let Some(started) = helper("helper____unconfined_app") else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, unconfined("loose")).unwrap();

    let connect = ask(&i, &format!("connect {address}")).await;
    let hosts = ask(&i, "read /etc/hosts").await;
    let ids = ask(&i, "ids").await;
    let node = ask(
        &i,
        &format!("read {}", started.dir.path().join("priv.conf").display()),
    )
    .await;

    assert_eq!(connect, "ok", "the host's network");
    assert_eq!(hosts, "ok", "the host's /etc, read-only");
    let uid = started.first_uid;
    assert_eq!(ids, format!("{uid} {uid}"), "its own user");
    assert!(node.starts_with("err:"), "{node}");
    #[cfg(target_os = "linux")]
    {
        let shadow = ask(&i, "read /etc/shadow").await;
        let pid = ask(&i, "os-pid").await;
        let children =
            std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", started.pid())).unwrap();
        let filters: Vec<String> = children
            .split_whitespace()
            .map(|pid| proc_status(pid.parse().unwrap(), "Seccomp_filters"))
            .collect();
        assert!(shadow.starts_with("err:"), "{shadow}");
        assert_eq!(pid, "1", "its PID namespace");
        assert_eq!(filters, ["1"], "the helper's filter alone");
    }
}

/// The node's grant alone does not unconfine an app: the helper refuses
/// one its own configuration does not list.
#[tokio::test(flavor = "multi_thread")]
async fn helper____unconfined_app_not_listed____refused_by_the_helper() {
    let Some(started) = helper("helper____unconfined_app_not_listed") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);

    let i = rt.spawn(&app, unconfined("alpha")).unwrap();
    let reason = i.wait().await;

    let ExitReason::Crashed(why) = &reason else {
        panic!("{reason:?}");
    };
    assert!(
        why.contains("not unconfined in the helper's configuration"),
        "{why}"
    );
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

/// The helper's first child: the one instance a test started.
fn instance_pid(started: &Started) -> u32 {
    let out = output("pgrep", &["-P", &started.pid().to_string()]);
    out.split_whitespace().next().unwrap().parse().unwrap()
}

/// No CPU limit reaches an instance (ROADMAP PS6b): on Linux its cgroup
/// has no `cpu` controller, on FreeBSD its jail no `pcpu` rule.
#[tokio::test(flavor = "multi_thread")]
async fn helper____an_instance____no_cpu_controller_or_rule() {
    let Some(started) = helper("helper____no_cpu") else {
        return;
    };
    let rt = runtime(&started);
    let app = program(&started, &rt);
    let i = rt.spawn(&app, spec("alpha")).unwrap();
    let _ = ask(&i, "echo up").await;
    let pid = instance_pid(&started);

    #[cfg(target_os = "linux")]
    {
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        let path = cgroup.lines().find_map(|l| l.strip_prefix("0::")).unwrap();
        let leaf = std::path::Path::new("/sys/fs/cgroup").join(path.trim().trim_start_matches('/'));
        let parent =
            std::fs::read_to_string(leaf.parent().unwrap().join("cgroup.subtree_control")).unwrap();
        assert!(leaf.join("memory.max").exists(), "{}", leaf.display());
        assert!(!leaf.join("cpu.max").exists(), "{}", leaf.display());
        assert!(!parent.split_whitespace().any(|c| c == "cpu"), "{parent}");
    }
    #[cfg(target_os = "freebsd")]
    {
        let rules = output("rctl", &[]);
        assert!(rules.contains("vmemoryuse"), "{rules}");
        assert!(!rules.contains("pcpu"), "{rules}");
    }
    let _ = pid;
    i.kill();
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
    // Room in its address space for the threads: on FreeBSD each new thread
    // may take a jemalloc arena, whose mapping the jail's `vmemoryuse`
    // (64 MiB by default) refuses, and jemalloc then faults in the thread's
    // start (SIGSEGV) before `nthr`, the limit this tests, is reached.
    let threading = rt
        .spawn(
            &app,
            InstanceSpec {
                memory_max: 512 << 20,
                ..patient("alpha")
            },
        )
        .unwrap();

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
    let second = keys.join("web.pem");
    for (path, body) in [
        (&file, "the driver's key"),
        (&beside, "not given"),
        (&second, "the driver's certificate"),
    ] {
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
                // Two files in one directory: bound once.
                files: vec![file.clone(), second.clone()],
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
    let certificate = ask(&d, "vcat /driver/web.pem").await;
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
    assert_eq!(certificate, "the driver's certificate");
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

#[tokio::test(flavor = "multi_thread")]
async fn helper____admin_and_metrics_driver____streams_and_metrics_reach_it_under_its_user() {
    let Some(started) = helper("helper____admin_and_metrics_driver") else {
        return;
    };
    let mut native = NativeConfig::default();
    native.runner = Some(started.runner.clone());
    native.helper = Some(started.helper.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        drivers: std::collections::BTreeMap::from([(
            "drv".to_string(),
            bulkhead_host::DriverProfile {
                admin: true,
                metrics: true,
                ..bulkhead_host::DriverProfile::default()
            },
        )]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    // The node's side, played here: an admin stream answers what it got.
    let streams: bulkhead_host::AdminStreams = std::sync::Arc::new(|mut stream| {
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut got = String::new();
            stream.read_to_string(&mut got).unwrap();
            stream.write_all(format!("admin:{got}").as_bytes()).unwrap();
        });
    });
    let text: bulkhead_host::MetricsText = std::sync::Arc::new(|| "node_metric 1\n".to_string());
    rt.set_admin_streams(&streams);
    rt.set_metrics_text(&text);
    let app = program(&started, &rt);
    let d = rt.spawn(&app, spec("drv")).unwrap();

    let ids = ask(&d, "ids").await;
    let answered = ask(&d, "admin-stream GET /v1/apps").await;
    let metrics = ask(&d, "metrics-text").await;

    let user = started.first_uid;
    assert_eq!(ids, format!("{user} {user}"), "a user of its own");
    assert_eq!(answered, "admin:GET /v1/apps");
    assert_eq!(metrics, "node_metric 1\n");
    d.kill();
}

/// A datasource's driver under the helper (ROADMAP DA3): a user of its
/// own, its password file shared with it and read under `/driver/`, its
/// one connection opened by the node, and an app's statements forwarded
/// to it.
#[tokio::test(flavor = "multi_thread")]
async fn helper____sql_driver____reads_its_password_and_logs_in_over_the_nodes_stream() {
    use std::os::unix::fs::PermissionsExt;
    let Some(started) = helper("helper____sql_driver") else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let Some(pg) = common::pg::Scratch::start_with(&common::pg::Options {
        tcp: true,
        password: Some("s3cret"),
        ..common::pg::Options::default()
    }) else {
        return;
    };
    let keys = privhelper::dir_owned(&started.dir.path().join("keys"), started.node_uid, 0o750);
    let file = keys.join("main.password");
    std::fs::write(&file, "s3cret\n").unwrap();
    std::os::unix::fs::chown(&file, Some(started.node_uid), Some(started.node_uid)).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    let mut ds = bulkhead_host::SqlDatasource::new("127.0.0.1", "postgres", "bulkhead");
    ds.port = pg.port();
    ds.password = Some(bulkhead_host::SecretSource::File(file));
    let datasources = std::collections::BTreeMap::from([("main".to_string(), ds)]);
    let mut native = NativeConfig::default();
    native.runner = Some(started.runner.clone());
    native.helper = Some(started.helper.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        datasources: datasources.clone(),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let unshared = rt.share_driver_files().await;
    let dir = privhelper::dir_owned(&started.dir.path().join("sql"), started.node_uid, 0o755);
    let program_path = privhelper::copy_owned(common::sql_program(), &dir, started.node_uid);
    let apps = common::sql_drivers_from(&rt, &datasources, &program_path).await;
    let app = program(&started, &rt);
    let mut s = spec("a");
    s.callback_deadline = LONG;
    s.granted.insert(Capability::Sql);
    s.services.sql.datasources = std::collections::BTreeMap::from([("main".to_string(), 1)]);
    let a = rt.spawn(&app, s).unwrap();

    let user = ask(&a, "sql-q main SELECT current_user::text AS u").await;

    assert!(unshared.is_empty(), "{unshared:?}");
    assert_eq!(
        user,
        "u=bulkhead",
        "the driver: {:?}",
        apps.app("bulkhead-sql-main").map(|d| d.status())
    );
    a.kill();
    apps.stop().await;
}

/// The users of the processes running the program `name`: one of its
/// arguments ends in it (on FreeBSD the runner executes the program
/// through the run-time linker, which comes first).
fn users_of(name: &str) -> Vec<u32> {
    let out = std::process::Command::new("ps")
        .args(["axww", "-o", "uid=", "-o", "args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (uid, args) = line.trim().split_once(char::is_whitespace)?;
            args.split_whitespace()
                .any(|arg| arg.rsplit('/').next() == Some(name))
                .then(|| uid.parse().ok())?
        })
        .collect()
}

/// An app's egress driver under the helper (ROADMAP DA4): a user of its
/// own, neither the node's nor the app's, its connection to the app's
/// allowed server opened by the node, and the app's request answered
/// through it.
#[cfg(feature = "egress")]
#[tokio::test(flavor = "multi_thread")]
async fn helper____egress_driver____under_its_own_user_reaches_the_apps_server_through_the_node() {
    use std::io::{Read, Write};
    let Some(started) = helper("helper____egress_driver") else {
        return;
    };
    // A server that answers each connection's first request and hangs up.
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = server.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in server.incoming().flatten() {
            let mut got = Vec::new();
            let mut buf = [0u8; 1024];
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
        }
    });
    let mut native = NativeConfig::default();
    native.runner = Some(started.runner.clone());
    native.helper = Some(started.helper.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let dir = privhelper::dir_owned(&started.dir.path().join("prog"), started.node_uid, 0o755);
    let driver = rt.load_native(NativeCode {
        path: privhelper::copy_owned(common::egress_program(), &dir, started.node_uid),
        kind: NativeProgram::Executable,
    });
    let app = rt.load_native(NativeCode {
        path: privhelper::copy_owned(common::native_sdk(), &dir, started.node_uid),
        kind: NativeProgram::Executable,
    });
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    apps.add(common::egress_driver(&rt, "a", driver))
        .await
        .unwrap();
    let mut s = spec("a");
    s.callback_deadline = LONG;
    s.granted.insert(Capability::HttpOutgoing);
    s.services.egress.allow = vec![format!("http://{address}")];
    let a = rt.spawn(&app, s).unwrap();

    let answered = ask(&a, &format!("fetch GET http://{address}/hello")).await;
    let outside = ask(
        &a,
        &format!("fetch GET http://127.0.0.1:{}/", address.port() + 1),
    )
    .await;
    let drivers = users_of("bulkhead-egress");
    let apps_users = users_of("bulkhead-fixture-sdk");

    assert_eq!(answered, "200 hi");
    assert_eq!(outside, "err:denied");
    assert_eq!(drivers.len(), 1, "{drivers:?}");
    assert_eq!(apps_users.len(), 1, "{apps_users:?}");
    let (driver_user, app_user) = (drivers[0], apps_users[0]);
    assert!(driver_user >= started.first_uid, "{driver_user}");
    assert!(app_user >= started.first_uid, "{app_user}");
    assert_ne!(driver_user, app_user, "the driver has a user of its own");
    a.kill();
    apps.stop().await;
}

/// The `shmem` bytes charged to app `app`'s cgroup under this test
/// process's helpers (`<cgroup>/apps/<app>`, which counts its instances).
#[cfg(target_os = "linux")]
fn app_shmem(app: &str) -> u64 {
    let ours = format!("bulkhead-test-{}-", std::process::id());
    std::fs::read_dir("/sys/fs/cgroup")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(&ours))
        .filter_map(|e| {
            std::fs::read_to_string(e.path().join("apps").join(app).join("memory.stat")).ok()
        })
        .filter_map(|stat| {
            stat.lines()
                .find_map(|l| l.strip_prefix("shmem "))
                .map(|v| v.trim().parse::<u64>().unwrap())
        })
        .sum()
}

/// A blob between two apps under the helper (ROADMAP BL1): each app under
/// a user of its own, the reader's descriptor working with no file
/// permissions involved, neither able to change it once sealed, and on
/// Linux the memory charged to the writer's cgroup, not the reader's.
#[tokio::test(flavor = "multi_thread")]
async fn helper____blob____written_by_one_user_read_by_another() {
    let Some(started) = helper("helper____blob") else {
        return;
    };
    let rt = runtime(&started);
    let code = program(&started, &rt);
    let manifest = |name: &str| {
        common::app_manifest(
            name,
            serde_json::json!({
                "kind": "native-process",
                "native": { "program": "executable" },
                "capabilities": { "bulkhead:blobs": {} },
                "limits": { "callback_deadline_ms": 5000, "memory_max": "64MiB" },
            }),
        )
    };
    let apps = rt
        .start_apps(vec![
            bulkhead_host::AppDeployment::new(manifest("writer"), code.clone()),
            bulkhead_host::AppDeployment::new(manifest("reader"), code),
        ])
        .await
        .unwrap();
    let w = apps.app("writer").unwrap().instances().remove(0);
    let r = apps.app("reader").unwrap().instances().remove(0);
    let size = 4 << 20;

    let id = ask(&w, &format!("blob-create {size}")).await;
    let filled = ask(&w, &format!("blob-fill {id} ab")).await;
    let sealed = ask(&w, &format!("blob-seal {id}")).await;
    let shared = ask(&w, &format!("blob-share {id} reader")).await;
    let read = ask(&r, &format!("blob-open {id}")).await;
    let tried = ask(&r, &format!("blob-try {id}")).await;
    let users = (ask(&w, "ids").await, ask(&r, "ids").await);
    #[cfg(target_os = "linux")]
    let charged = (app_shmem("writer"), app_shmem("reader"));

    assert_eq!(
        (filled.as_str(), sealed.as_str(), shared.as_str()),
        ("ok", "ok", "ok"),
        "blob {id}"
    );
    assert_eq!(read, format!("{size} abababababababab"));
    assert!(
        tried.split_whitespace().all(|kv| !kv.ends_with("=ok")),
        "{tried}"
    );
    assert_ne!(users.0, users.1, "a user of its own each");
    #[cfg(target_os = "linux")]
    {
        assert!(charged.0 >= size, "the writer's cgroup: {charged:?}");
        assert!(charged.1 < size / 4, "the reader's cgroup: {charged:?}");
    }
    apps.stop().await;
}

/// A Modbus TCP read of `count` holding registers from 0, as a master
/// sends it (MBAP header, unit 1, function 3).
fn modbus_read(count: u16) -> Vec<u8> {
    let mut req = vec![0, 1, 0, 0, 0, 6, 1, 3, 0, 0];
    req.extend(count.to_be_bytes());
    req
}

/// Answer each read of holding registers on `listener` with `values`, as
/// a device does; nothing else is served.
fn fake_modbus_device(listener: std::net::TcpListener, values: Vec<u16>) {
    use std::io::{Read, Write};
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let values = values.clone();
            std::thread::spawn(move || {
                let mut req = [0u8; 12];
                while stream.read_exact(&mut req).is_ok() {
                    let count = usize::from(u16::from_be_bytes([req[10], req[11]]));
                    let data: Vec<u8> = values
                        .iter()
                        .cycle()
                        .take(count)
                        .flat_map(|v| v.to_be_bytes())
                        .collect();
                    let len = u16::try_from(3 + data.len()).unwrap();
                    let mut reply = vec![req[0], req[1], 0, 0];
                    reply.extend(len.to_be_bytes());
                    reply.extend([req[6], 3, u8::try_from(data.len()).unwrap()]);
                    reply.extend(data);
                    if stream.write_all(&reply).is_err() {
                        break;
                    }
                }
            });
        }
    });
}

/// The Modbus drivers under the helper (ROADMAP DA5): the devices' driver
/// under a user of its own polls a device over the connection the node
/// opens and answers an app's read through the node; the server's driver,
/// under another, answers a master on the listener the node bound.
#[tokio::test(flavor = "multi_thread")]
async fn helper____modbus_drivers____poll_a_device_and_serve_a_master_under_their_users() {
    use bulkhead_host::services::modbus::{MODBUS_DEVICES_APP, MODBUS_SERVER_APP};
    use std::io::{Read, Write};
    let Some(started) = helper("helper____modbus_drivers") else {
        return;
    };
    let device_socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let device = device_socket.local_addr().unwrap();
    fake_modbus_device(device_socket, vec![7, 8]);
    let master_socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let master = master_socket.local_addr().unwrap();
    let mut modbus = bulkhead_host::ModbusDrivers::default();
    modbus.devices.insert("pump-1".into());
    modbus.server = true;
    modbus.timeout = Duration::from_secs(5);
    let mut native = NativeConfig::default();
    native.runner = Some(started.runner.clone());
    native.helper = Some(started.helper.clone());
    let profile = |p: bulkhead_host::DriverProfile| bulkhead_host::DriverProfile {
        calls: vec!["scada".into()],
        delivers: true,
        ..p
    };
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        modbus: Some(modbus),
        drivers: std::collections::BTreeMap::from([
            (
                MODBUS_DEVICES_APP.to_string(),
                profile(bulkhead_host::DriverProfile {
                    endpoints: vec![bulkhead_host::DriverEndpoint::Tcp {
                        host: device.ip().to_string(),
                        port: device.port(),
                        timeout: Duration::from_secs(1),
                    }],
                    ..Default::default()
                }),
            ),
            (
                MODBUS_SERVER_APP.to_string(),
                profile(bulkhead_host::DriverProfile {
                    listeners: vec![bulkhead_host::DriverListener {
                        name: "modbus".into(),
                        socket: std::sync::Arc::new(master_socket),
                    }],
                    ..Default::default()
                }),
            ),
        ]),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let dir = privhelper::dir_owned(&started.dir.path().join("prog"), started.node_uid, 0o755);
    let load = |path: &PathBuf| {
        rt.load_native(NativeCode {
            path: privhelper::copy_owned(path, &dir, started.node_uid),
            kind: NativeProgram::Executable,
        })
    };
    let (devices_program, server_program) = common::modbus_programs();
    let driver = |name: &str, args: serde_json::Value| {
        common::app_manifest(
            name,
            serde_json::json!({
                "kind": "native-process",
                "native": { "program": "executable" },
                "lifecycle": { "start_args": args },
                "limits": { "memory_max": "64MiB" },
                "messaging_acl": { "accept_from": [name], "may_call": [name] },
            }),
        )
    };
    let scada = common::app_manifest(
        "scada",
        serde_json::json!({
            "kind": "native-process",
            "native": { "program": "executable" },
            "capabilities": { "extensions": { "bulkhead-modbus:modbus": { "read": ["pump-1"] } } },
            "limits": { "callback_deadline_ms": 5000, "memory_max": "64MiB" },
        }),
    );
    let apps = rt
        .start_apps(vec![
            bulkhead_host::AppDeployment::new(scada, program(&started, &rt)),
            bulkhead_host::AppDeployment::new(
                driver(
                    MODBUS_DEVICES_APP,
                    serde_json::json!({ "devices": { "pump-1": {
                        "address": device.to_string(), "poll_ms": 50, "deliver_to": "scada",
                        "holding": [{ "start": 0, "count": 2 }],
                    } } }),
                ),
                load(devices_program),
            ),
            bulkhead_host::AppDeployment::new(
                driver(
                    MODBUS_SERVER_APP,
                    serde_json::json!({ "server": { "listen": master.to_string(), "holding": 4 } }),
                ),
                load(server_program),
            ),
        ])
        .await
        .unwrap();
    let a = apps.app("scada").unwrap().instances().remove(0);

    let read = ask(
        &a,
        r#"ext bulkhead-modbus:modbus client#read-holding ["pump-1", 0, 2]"#,
    )
    .await;
    let mut polled = String::new();
    for _ in 0..100 {
        polled = ask(&a, "infos").await;
        if polled.contains("pump-1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let served = tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(master).unwrap();
        stream.write_all(&modbus_read(2)).unwrap();
        let mut reply = [0u8; 13];
        stream.read_exact(&mut reply).unwrap();
        reply
    })
    .await
    .unwrap();
    let users = (
        users_of("bulkhead-modbus"),
        users_of("bulkhead-modbus-server"),
    );

    assert_eq!(read, r#"{"Ok":[7,8]}"#);
    assert!(polled.contains("pump-1"), "{polled}");
    assert_eq!(&served[7..9], &[3, 4], "function 3, four bytes: {served:?}");
    assert_eq!(users.0.len(), 1, "{users:?}");
    assert_eq!(users.1.len(), 1, "{users:?}");
    assert!(users.0[0] >= started.first_uid && users.1[0] >= started.first_uid);
    assert_ne!(users.0[0], users.1[0], "a user of its own each");
    apps.stop().await;
}

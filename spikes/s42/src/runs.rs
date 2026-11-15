//! What S42 runs: the checks (pass, fail or a note each) and the cost
//! table.

use std::path::PathBuf;
use std::time::Instant;

use rustix::fs::SealFlags;

use crate::node::{self, App, Start, for_app, produce, seal, spawn};
use crate::wire;

/// Command-line options: `--runner PATH`, `--users A,B`, `--cgroup DIR`,
/// `--sizes N,N,...`, `--count N` (a scale for the iterations).
pub struct Opts {
    pub runner: Option<PathBuf>,
    pub users: Option<(u32, u32)>,
    pub cgroup: Option<PathBuf>,
    pub sizes: Vec<usize>,
    pub count: usize,
}

pub fn opts(args: &[String]) -> Opts {
    let mut o = Opts {
        runner: None,
        users: None,
        cgroup: None,
        sizes: vec![64 << 10, 1 << 20, 20 << 20],
        count: 1,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut v = || it.next().expect("a value").clone();
        match a.as_str() {
            "--runner" => o.runner = Some(v().into()),
            "--users" => {
                let s = v();
                let (a, b) = s.split_once(',').expect("--users A,B");
                o.users = Some((a.parse().unwrap(), b.parse().unwrap()));
            }
            "--cgroup" => o.cgroup = Some(v().into()),
            "--sizes" => o.sizes = v().split(',').map(|s| s.parse().unwrap()).collect(),
            "--count" => o.count = v().parse().unwrap(),
            other => panic!("unknown argument {other}"),
        }
    }
    o
}

fn report(ok: bool, what: &str, detail: &str) {
    println!("{} {what}: {detail}", if ok { "PASS" } else { "FAIL" });
}

fn note(what: &str, detail: &str) {
    println!("NOTE {what}: {detail}");
}

fn errno_name(e: u64) -> String {
    match e as i32 {
        0 => "ok (allowed)".into(),
        libc::EPERM => "EPERM".into(),
        #[cfg(target_os = "freebsd")]
        libc::ENOTCAPABLE => "ENOTCAPABLE".into(),
        e => format!("errno {e}"),
    }
}

/// Every field of a `TRY` report refused.
fn all_refused(report: &str) -> bool {
    report.split_whitespace().all(|kv| !kv.ends_with("=ok"))
}

/// `shmem` bytes in a cgroup's `memory.stat`.
fn shmem(cg: &std::path::Path) -> u64 {
    std::fs::read_to_string(cg.join("memory.stat"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("shmem ")
                    .map(|v| v.trim().parse().unwrap_or(0))
            })
        })
        .unwrap_or(0)
}

/// System-wide `Shmem` in `/proc/meminfo`, in KiB (Linux).
fn meminfo_shmem() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("Shmem:")
                    .and_then(|v| v.split_whitespace().next()?.parse().ok())
            })
        })
        .unwrap_or(0)
}

/// What `rctl -u process:<pid>` says of memory (FreeBSD, root).
#[cfg(target_os = "freebsd")]
fn rctl_memory(pid: u32) -> String {
    // racct updates a process's resident memory once a second.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let out = std::process::Command::new("rctl")
        .args(["-h", "-u", &format!("process:{pid}")])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| l.starts_with("memoryuse") || l.starts_with("vmemoryuse"))
            .collect::<Vec<_>>()
            .join(" "),
        Err(e) => format!("rctl: {e}"),
    }
}

pub fn checks(args: &[String]) {
    let o = opts(args);
    let cg_parent = o.cgroup.clone();
    let (cg_w, cg_r) = match &cg_parent {
        Some(p) => {
            let _ = std::fs::write(p.join("cgroup.subtree_control"), "+memory");
            let (w, r) = (p.join("writer"), p.join("reader"));
            std::fs::create_dir_all(&w).unwrap();
            std::fs::create_dir_all(&r).unwrap();
            (Some(w), Some(r))
        }
        None => (None, None),
    };
    let start = |user: Option<u32>, cg: Option<PathBuf>| Start {
        runner: o.runner.clone(),
        user,
        cgroup: cg,
    };
    let mut w = spawn(&start(o.users.map(|u| u.0), cg_w.clone()));
    let mut r = spawn(&start(o.users.map(|u| u.1), cg_r.clone()));
    w.send(wire::WHO, 0, 0, &[]);
    r.send(wire::WHO, 0, 0, &[]);
    println!(
        "writer {}, reader {}, through {}",
        w.text(),
        r.text(),
        o.runner
            .as_ref()
            .map_or("no runner".into(), |p| p.display().to_string())
    );
    let n = 1 << 20;
    let expected = wire::sum(&wire::pattern(n));
    let mut id = 0;

    for (label, how) in [("w1 mmap", 0), ("w2 pwrite", wire::MODE_PWRITE)] {
        id += 1;
        let (fd, _) = produce(&mut w, id, n, how);
        let sealed = seal(&fd);
        r.pass(id, &for_app(&fd, false));
        r.send(wire::BLOB_READ, id, 0, &[]);
        let (sum, _) = r.ok();
        report(
            sealed.is_ok() && sum == expected,
            &format!("{label}: filled, sealed, read by the other app"),
            &format!("seal {sealed:?}, sum matches {}", sum == expected),
        );
    }

    // Sealed against size first, against writes after filling.
    id += 1;
    let (fd, probe) = produce(&mut w, id, n, wire::MODE_PWRITE | wire::KEEP | wire::PROBE);
    report(
        probe != 0,
        "the writer cannot resize a granted blob",
        &format!("ftruncate before filling: {}", errno_name(probe)),
    );
    seal(&fd).expect("seal");
    r.pass(id, &for_app(&fd, false));
    r.send(wire::BLOB_READ, id, wire::KEEP, &[]);
    r.ok();
    w.send(wire::TRY, id, 0, &[]);
    let tw = w.text();
    report(
        all_refused(&tw),
        "the writer cannot change a sealed blob",
        &tw,
    );
    r.send(wire::TRY, id, 0, &[]);
    let tr = r.text();
    report(
        all_refused(&tr),
        "a reader cannot change a sealed blob",
        &tr,
    );
    for app in [&mut w, &mut r] {
        app.send(wire::DROP, id, 0, &[]);
        app.ok();
    }
    drop(fd);

    // A writer that keeps a writable mapping.
    id += 1;
    let (fd, _) = produce(&mut w, id, n, wire::KEEP);
    match seal(&fd) {
        Err(e) => report(
            true,
            "a writable mapping kept by the writer blocks the write seal",
            &format!("refused: {e:?}"),
        ),
        Ok(()) => {
            r.pass(id, &for_app(&fd, false));
            r.send(wire::BLOB_READ, id, wire::KEEP, &[]);
            r.ok();
            w.send(wire::POKE, id, 0, &[]);
            w.ok();
            r.send(wire::RESUM, id, 0, &[]);
            let (_, first) = r.ok();
            report(
                first != 0xAA,
                "a writable mapping kept by the writer blocks the write seal",
                &format!(
                    "the seal was allowed; a write through the kept mapping {} the reader",
                    if first == 0xAA {
                        "REACHED"
                    } else {
                        "did not reach"
                    }
                ),
            );
            r.send(wire::DROP, id, 0, &[]);
            r.ok();
        }
    }
    w.send(wire::DROP, id, 0, &[]);
    w.ok();
    drop(fd);

    #[cfg(target_os = "linux")]
    {
        id += 1;
        let (fd, _) = produce(&mut w, id, n, wire::KEEP);
        let fw = rustix::fs::fcntl_add_seals(&fd, SealFlags::FUTURE_WRITE);
        w.send(wire::POKE, id, 0, &[]);
        w.ok();
        let mut first = [0u8; 1];
        let _ = rustix::io::pread(&fd, &mut first, 0);
        note(
            "F_SEAL_FUTURE_WRITE with a kept writable mapping",
            &format!(
                "the seal {fw:?}; a write through the writer's kept mapping after it {}, so it cannot make a blob immutable",
                if first[0] == 0xAA {
                    "landed"
                } else {
                    "did not land"
                }
            ),
        );
        w.send(wire::DROP, id, 0, &[]);
        w.ok();
        drop(fd);
    }

    // What the node can revoke: nothing an app holds.
    id += 1;
    let big = 20 << 20;
    let big_sum = wire::sum(&wire::pattern(big));
    let base_sys = meminfo_shmem();
    let stage = |label: &str| {
        let mut s = format!("{label}:");
        if let (Some(p), Some(cw), Some(cr)) = (&cg_parent, &cg_w, &cg_r) {
            s += &format!(
                " shmem writer {} KiB, reader {} KiB, parent {} KiB;",
                shmem(cw) >> 10,
                shmem(cr) >> 10,
                shmem(p) >> 10
            );
        }
        #[cfg(target_os = "linux")]
        {
            s += &format!(
                " system Shmem {:+} KiB",
                meminfo_shmem() as i64 - base_sys as i64
            );
        }
        s
    };
    let (fd, _) = produce(&mut w, id, big, wire::KEEP);
    let after_fill = stage("filled (writer's mapping kept)");
    #[cfg(target_os = "freebsd")]
    let rctl_w = rctl_memory(w.pid());
    w.send(wire::DROP, id, 0, &[]);
    w.ok();
    seal(&fd).expect("seal");
    r.pass(id, &for_app(&fd, false));
    r.send(wire::BLOB_READ, id, wire::KEEP, &[]);
    let (s1, _) = r.ok();
    let after_read = stage("read (reader's mapping kept, writer's dropped)");
    #[cfg(target_os = "freebsd")]
    let rctl_r = rctl_memory(r.pid());
    let wpid = w.pid();
    w.exit();
    if let Some(cw) = &cg_w {
        let _ = std::fs::remove_dir(cw);
    }
    let after_exit = stage("writer exited, its cgroup removed");
    drop(fd);
    r.send(wire::RESUM, id, 0, &[]);
    let (s2, _) = r.ok();
    let after_node = stage("node's descriptor closed, reader still mapped");
    report(
        s1 == big_sum && s2 == big_sum,
        "a reader keeps a blob the node let go of (the node cannot revoke it)",
        &format!("20 MiB read before and after: sums match {}", s2 == big_sum),
    );
    r.send(wire::DROP, id, 0, &[]);
    r.ok();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let after_drop = stage("reader dropped it");
    for s in [after_fill, after_read, after_exit, after_node, after_drop] {
        note("memory", &s);
    }
    #[cfg(target_os = "freebsd")]
    {
        note("rctl, writer with its mapping", &rctl_w);
        note("rctl, reader with its mapping", &rctl_r);
    }
    let _ = wpid;
    r.exit();
    if let Some(p) = &cg_parent {
        let _ = std::fs::remove_dir(p.join("reader"));
    }
}

/// p50 and p99 in µs.
fn percentiles(mut v: Vec<f64>) -> (f64, f64) {
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    (at(0.5), at(0.99))
}

pub fn bench(args: &[String]) {
    let o = opts(args);
    let start = Start {
        runner: o.runner.clone(),
        ..Start::default()
    };
    let mut w = spawn(&start);
    let mut r = spawn(&start);
    let mut id = 0u64;
    println!("| size | path | p50 µs | p99 µs | CPU µs (node + writer + reader) | MB/s |");
    println!("| --- | --- | --- | --- | --- | --- |");
    // (label, (writer's, reader's) ways, pages made ahead)
    let paths: [(&str, Option<(u64, u64)>, bool); 6] = [
        ("ring", None, false),
        ("blob: mmap, mmap", Some((0, 0)), false),
        ("blob: pwrite, mmap", Some((wire::MODE_PWRITE, 0)), false),
        (
            "blob: pwrite, pread",
            Some((wire::MODE_PWRITE, wire::COPY)),
            false,
        ),
        ("blob: mmap, mmap; pages ahead", Some((0, 0)), true),
        (
            "blob: pwrite, pread; pages ahead",
            Some((wire::MODE_PWRITE, wire::COPY)),
            true,
        ),
    ];
    for &n in &o.sizes {
        let expected = wire::sum(&wire::pattern(n));
        let iters = ((256usize << 20) / n).clamp(30, 3000) * o.count;
        for (label, how, ready) in paths {
            node::READY.store(ready, std::sync::atomic::Ordering::Relaxed);
            let mut one = |w: &mut App, r: &mut App| match how {
                None => {
                    w.send(wire::RING_SEND, n as u64, 0, &[]);
                    let f = w.recv();
                    assert_eq!(f.kind, wire::DATA);
                    r.send(wire::DATA, 0, 0, &f.payload);
                    w.give_back(f);
                    let (s, _) = r.ok();
                    assert_eq!(s, expected);
                }
                Some((hw, hr)) => {
                    id += 1;
                    let (fd, _) = produce(w, id, n, hw);
                    seal(&fd).expect("seal");
                    r.pass(id, &for_app(&fd, false));
                    r.send(wire::BLOB_READ, id, hr, &[]);
                    let (s, _) = r.ok();
                    assert_eq!(s, expected);
                }
            };
            for _ in 0..3 {
                one(&mut w, &mut r);
            }
            let cpu0 =
                node::cpu_ns(std::process::id()) + node::cpu_ns(w.pid()) + node::cpu_ns(r.pid());
            node::READY_NS.store(0, std::sync::atomic::Ordering::Relaxed);
            let t0 = Instant::now();
            let mut lat = Vec::with_capacity(iters);
            for _ in 0..iters {
                let t = Instant::now();
                one(&mut w, &mut r);
                lat.push(t.elapsed().as_secs_f64() * 1e6);
            }
            let wall = t0.elapsed().as_secs_f64();
            let cpu =
                node::cpu_ns(std::process::id()) + node::cpu_ns(w.pid()) + node::cpu_ns(r.pid())
                    - cpu0;
            // Pages made ahead are off the request's path: their time is
            // taken out of each request (evenly) and shown apart.
            let ahead = node::READY_NS.load(std::sync::atomic::Ordering::Relaxed) as f64
                / iters as f64
                / 1e3;
            let lat = lat.into_iter().map(|l| l - ahead).collect();
            let (p50, p99) = percentiles(lat);
            let size = if n >= 1 << 20 {
                format!("{} MiB", n >> 20)
            } else {
                format!("{} KiB", n >> 10)
            };
            let ahead_note = if ready {
                format!(" (+{ahead:.0} ahead)")
            } else {
                String::new()
            };
            println!(
                "| {size} | {label} | {p50:.0} | {p99:.0} | {:.0}{ahead_note} | {:.0} |",
                cpu as f64 / iters as f64 / 1e3 - ahead,
                (n * iters) as f64 / (wall - ahead * iters as f64 / 1e6) / 1e6
            );
        }
    }
    w.exit();
    r.exit();
}

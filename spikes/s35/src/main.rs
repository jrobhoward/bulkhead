//! S35: ZFS datasets as app volumes (ROADMAP, "ZFS volumes"). See README.md.
//!
//! ```text
//! s35 req PARENT ZFS create VOL QUOTA | quota VOL QUOTA | snap VOL TAG
//!             | rollback VOL TAG | unsnap VOL TAG | destroy VOL | list
//! s35 bench PARENT ZFS N          times of each request, N rounds
//! s35 fill DIR [UID]              write 1 MiB pieces until refused (as UID)
//! s35 landlock DIR [UID]          fill under a Landlock rule on DIR alone (Linux)
//! s35 fenced trap|log -- CMD ...  exec CMD under the helper's fence (Linux)
//! ```

mod fill;
#[cfg(target_os = "linux")]
mod fence;
mod zfs;

use std::time::Duration;

use zfs::{Config, Req};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = match a.as_slice() {
        ["req", parent, zfs, rest @ ..] => req(&cfg(parent, zfs), rest),
        ["bench", parent, zfs, n] => {
            bench(&cfg(parent, zfs), n.parse().unwrap());
            0
        }
        ["fill", dir] => fill::fill(dir, None),
        ["fill", dir, uid] => fill::fill(dir, Some(uid.parse().unwrap())),
        #[cfg(target_os = "linux")]
        ["landlock", dir, rest @ ..] => fence::landlock_fill(dir, rest.first().map(|u| u.parse().unwrap())),
        #[cfg(target_os = "linux")]
        ["fenced", mode, "--", cmd @ ..] => fence::exec_fenced(mode, cmd),
        _ => {
            eprintln!("usage: see src/main.rs");
            2
        }
    };
    std::process::exit(code);
}

fn cfg(parent: &str, zfs: &str) -> Config {
    Config { parent: parent.into(), max_quota: 50 << 30, zfs: zfs.into() }
}

fn parse_req(w: &[&str]) -> Result<Req, String> {
    let s = |i: usize| w.get(i).map(|s| (*s).to_string()).ok_or("missing argument");
    let n = |i: usize| -> Result<u64, String> { s(i)?.parse().map_err(|_| "a number".into()) };
    Ok(match w.first().copied() {
        Some("create") => Req::Create { volume: s(1)?, refquota: n(2)?, reservation: 0 },
        Some("quota") => Req::SetQuota { volume: s(1)?, refquota: n(2)? },
        Some("snap") => Req::Snapshot { volume: s(1)?, tag: s(2)? },
        Some("rollback") => Req::Rollback { volume: s(1)?, tag: s(2)? },
        Some("unsnap") => Req::DestroySnapshot { volume: s(1)?, tag: s(2)? },
        Some("destroy") => Req::Destroy { volume: s(1)? },
        Some("list") => Req::List,
        other => return Err(format!("unknown request {other:?}")),
    })
}

fn req(cfg: &Config, w: &[&str]) -> i32 {
    let req = match parse_req(w) {
        Ok(r) => r,
        Err(e) => {
            println!("refused: {e}");
            return 1;
        }
    };
    match zfs::request(cfg, &req) {
        Err(e) => {
            println!("refused: {e}");
            1
        }
        Ok(ran) => {
            let ms = ran.took.as_secs_f64() * 1e3;
            if ran.ok {
                println!("ok {ms:.1} ms");
                if matches!(req, Req::List) {
                    match zfs::parse_list(&cfg.parent, &ran.stdout) {
                        Ok(rows) => rows.iter().for_each(|r| println!("  {r:?}")),
                        Err(e) => println!("  unparsed: {e}\n{}", ran.stdout),
                    }
                }
                0
            } else {
                println!("failed {ms:.1} ms: {}", ran.stderr);
                1
            }
        }
    }
}

/// Each request's time over `n` rounds on fresh volumes `s35-bench-<i>`.
fn bench(cfg: &Config, n: usize) {
    let ops = ["create", "quota", "snap", "rollback", "unsnap", "list", "destroy"];
    let mut times: Vec<Vec<Duration>> = vec![Vec::new(); ops.len() + 1];
    for i in 0..n {
        let v = format!("s35-bench-{i}");
        let reqs = [
            Req::Create { volume: v.clone(), refquota: 64 << 20, reservation: 0 },
            Req::SetQuota { volume: v.clone(), refquota: 128 << 20 },
            Req::Snapshot { volume: v.clone(), tag: "t".into() },
            Req::Rollback { volume: v.clone(), tag: "t".into() },
            Req::DestroySnapshot { volume: v.clone(), tag: "t".into() },
            Req::List,
            Req::Destroy { volume: v.clone() },
        ];
        for (k, r) in reqs.iter().enumerate() {
            let ran = zfs::request(cfg, r).unwrap();
            assert!(ran.ok, "{:?}: {}", r, ran.stderr);
            times[k].push(ran.took);
        }
        // The floor: starting a process that does nothing.
        let t = std::time::Instant::now();
        let _ = std::process::Command::new("/bin/true").status();
        times[ops.len()].push(t.elapsed());
    }
    println!("| request | median ms | p90 ms | max ms |");
    println!("| --- | --- | --- | --- |");
    for (k, name) in ops.iter().chain(["/bin/true"].iter()).enumerate() {
        let t = &mut times[k];
        t.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let p = |q: f64| t[((t.len() as f64 - 1.0) * q).round() as usize];
        println!("| {name} | {:.1} | {:.1} | {:.1} |", ms(p(0.5)), ms(p(0.9)), ms(t[t.len() - 1]));
    }
}

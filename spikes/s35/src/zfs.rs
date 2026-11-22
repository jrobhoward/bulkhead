//! The helper's ZFS requests as S35 proposes them: what the node may ask,
//! the checks the helper makes against its own configuration, the `zfs`
//! command line each becomes, and `zfs list -H -p`'s output parsed.
//!
//! The node never names a dataset: it names a volume, and the helper puts
//! it under the one parent its configuration gives. Nothing the node sends
//! reaches the command line but a checked volume name, a checked snapshot
//! tag and numbers.

use std::process::Command;
use std::time::{Duration, Instant};

/// The helper's configuration for ZFS (from its own file, not the node).
#[derive(Debug, Clone)]
pub struct Config {
    /// The parent dataset; volumes are its children, one level down.
    pub parent: String,
    /// The largest `refquota` a volume may have.
    pub max_quota: u64,
    /// The `zfs` program, by absolute path.
    pub zfs: String,
}

/// What the node may ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Req {
    /// Make a volume's dataset with a quota (and optionally a reservation).
    Create { volume: String, refquota: u64, reservation: u64 },
    /// Change its quota.
    SetQuota { volume: String, refquota: u64 },
    /// Snapshot it (`<volume>@<tag>`).
    Snapshot { volume: String, tag: String },
    /// Roll it back to its latest snapshot, which must be `tag`.
    Rollback { volume: String, tag: String },
    /// Remove a snapshot.
    DestroySnapshot { volume: String, tag: String },
    /// Remove the dataset and its snapshots (never clones).
    Destroy { volume: String },
    /// The parent's children and their snapshots.
    List,
}

/// The properties every volume is made with: nothing on it executes,
/// honours set-user-id bits or opens device nodes, whatever the parent
/// says. FreeBSD accepts `devices` too (its device nodes live in devfs
/// alone, so it changes nothing there).
pub fn fixed_properties() -> &'static [&'static str] {
    &["exec=off", "setuid=off", "devices=off", "canmount=on"]
}

/// A volume name: what an app's manifest and node config call it, used
/// as the dataset's last component. Lower-case letters, digits and `-`,
/// 1 to 64, not starting with `-`. ZFS allows more (`_ . :` and spaces);
/// the helper allows less, so no name can be read as an option, a path
/// component, a snapshot (`@`) or a bookmark (`#`).
pub fn check_name(what: &str, s: &str) -> Result<(), String> {
    let ok = (1..=64).contains(&s.len())
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok { Ok(()) } else { Err(format!("{what} {s:?}: [a-z0-9-]{{1,64}}, not starting with -")) }
}

/// The `zfs` arguments for `req`, after the helper's checks.
pub fn argv(cfg: &Config, req: &Req) -> Result<Vec<String>, String> {
    let ds = |v: &str| -> Result<String, String> {
        check_name("volume", v)?;
        Ok(format!("{}/{v}", cfg.parent))
    };
    let snap = |v: &str, t: &str| -> Result<String, String> {
        check_name("tag", t)?;
        Ok(format!("{}@{t}", ds(v)?))
    };
    let quota = |q: u64| -> Result<String, String> {
        if q == 0 || q > cfg.max_quota {
            return Err(format!("refquota {q}: 1 to {}", cfg.max_quota));
        }
        Ok(format!("refquota={q}"))
    };
    let mut a: Vec<String> = Vec::new();
    match req {
        Req::Create { volume, refquota, reservation } => {
            a.push("create".into());
            for p in fixed_properties() {
                a.extend(["-o".into(), (*p).into()]);
            }
            a.extend(["-o".into(), quota(*refquota)?]);
            if *reservation > 0 {
                if reservation > refquota {
                    return Err("reservation over the quota".into());
                }
                a.extend(["-o".into(), format!("refreservation={reservation}")]);
            }
            a.push(ds(volume)?);
        }
        Req::SetQuota { volume, refquota } => {
            a.extend(["set".into(), quota(*refquota)?, ds(volume)?]);
        }
        Req::Snapshot { volume, tag } => a.extend(["snapshot".into(), snap(volume, tag)?]),
        // Without `-r`: only the latest snapshot, so a rollback never
        // destroys snapshots taken after it.
        Req::Rollback { volume, tag } => a.extend(["rollback".into(), snap(volume, tag)?]),
        Req::DestroySnapshot { volume, tag } => a.extend(["destroy".into(), snap(volume, tag)?]),
        // `-r` takes its snapshots; `-R` (clones elsewhere) never.
        Req::Destroy { volume } => a.extend(["destroy".into(), "-r".into(), ds(volume)?]),
        Req::List => a.extend(
            [
                "list", "-H", "-p", "-r", "-t", "filesystem,snapshot", "-o",
                "name,used,referenced,refquota,refreservation,mountpoint,mounted",
            ]
            .map(String::from),
        ),
    }
    if matches!(req, Req::List) {
        a.push(cfg.parent.clone());
    }
    Ok(a)
}

/// One row of [`Req::List`]'s output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The volume (`None` for the parent itself).
    pub volume: Option<String>,
    /// The snapshot tag, for a snapshot.
    pub tag: Option<String>,
    pub used: u64,
    pub referenced: u64,
    /// 0 for none (`-p` prints `0`; snapshots print `-`, read as 0).
    pub refquota: u64,
    pub refreservation: u64,
    /// `None` for a snapshot, or a dataset not mounted (`none`, `-`).
    pub mountpoint: Option<String>,
    pub mounted: bool,
}

/// Parse [`Req::List`]'s output: tab-separated, numbers in bytes.
pub fn parse_list(parent: &str, out: &str) -> Result<Vec<Row>, String> {
    let num = |s: &str| -> Result<u64, String> {
        if s == "-" || s == "none" { Ok(0) } else { s.parse().map_err(|_| format!("number {s:?}")) }
    };
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [name, used, refd, quota, resv, mp, mounted] = f[..] else {
                return Err(format!("{} fields in {line:?}", f.len()));
            };
            let (ds, tag) = match name.split_once('@') {
                Some((d, t)) => (d, Some(t.to_string())),
                None => (name, None),
            };
            let volume = if ds == parent {
                None
            } else {
                let v = ds
                    .strip_prefix(parent)
                    .and_then(|r| r.strip_prefix('/'))
                    .ok_or_else(|| format!("{ds} is not under {parent}"))?;
                Some(v.to_string())
            };
            Ok(Row {
                volume,
                tag,
                used: num(used)?,
                referenced: num(refd)?,
                refquota: num(quota)?,
                refreservation: num(resv)?,
                mountpoint: (mp != "-" && mp != "none" && mp != "legacy").then(|| mp.to_string()),
                mounted: mounted == "yes",
            })
        })
        .collect()
}

/// The outcome of running `zfs`.
#[derive(Debug)]
pub struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    pub took: Duration,
}

/// Run `zfs` with `args`, with an empty environment (the helper's is
/// root's: nothing in it should reach the command) and a fixed `PATH`.
pub fn run(cfg: &Config, args: &[String]) -> Ran {
    let t = Instant::now();
    let out = Command::new(&cfg.zfs)
        .args(args)
        .env_clear()
        .env("PATH", "/sbin:/bin:/usr/sbin:/usr/bin")
        .output();
    let took = t.elapsed();
    match out {
        Ok(o) => Ran {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
            took,
        },
        Err(e) => Ran { ok: false, stdout: String::new(), stderr: e.to_string(), took },
    }
}

/// [`argv`] then [`run`].
pub fn request(cfg: &Config, req: &Req) -> Result<Ran, String> {
    Ok(run(cfg, &argv(cfg, req)?))
}

#[cfg(test)]
#[path = "zfs_tests.rs"]
mod zfs_tests;

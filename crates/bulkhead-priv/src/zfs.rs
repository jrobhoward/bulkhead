//! ZFS volumes (ROADMAP ZV1, spike S35): each app's volume a dataset at
//! `PARENT/APP/VOLUME` under the one parent the helper's configuration
//! names, with a quota (`refquota`, so snapshots are not counted against
//! the app), made and changed through the `zfs` command.
//!
//! The command, not libzfs: `zfs ... -H -p` is meant for scripts and the
//! same on Linux and FreeBSD (S35), needs nothing at build time, and is
//! permissively usable where libzfs is CDDL. A request costs 9 to 34 ms on
//! the Raspberry Pi 5 (S35), at a deploy, an upgrade or a destroy, never
//! per message; the helper's loop waits for it.
//!
//! The node names an app and a volume, both checked as names
//! (`bulkhead_priv_wire::is_volume_name`), and a quota checked against
//! the configuration's ceiling; nothing else of its reaches the command
//! line. The command runs with an empty environment and a fixed `PATH`.

use std::path::Path;
use std::process::Command;

/// The properties every volume is made with, whatever its parent says:
/// nothing on it executes, honours set-user-id bits or opens device nodes
/// (FreeBSD accepts `devices` too; its device nodes live in devfs alone).
pub const FIXED: [&str; 4] = ["exec=off", "setuid=off", "devices=off", "canmount=on"];

/// One filesystem under the parent, from `zfs list -H -p`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Its full name.
    pub name: String,
    /// Bytes used, snapshots included.
    pub used: u64,
    /// Bytes it refers to, which `refquota` counts.
    pub referenced: u64,
    /// Its quota in bytes; 0 for none.
    pub refquota: u64,
    /// Where it mounts; `None` for `none`, `legacy` or `-`.
    pub mountpoint: Option<String>,
    /// Whether it is mounted.
    pub mounted: bool,
}

/// The arguments listing `dataset` and every filesystem under it.
#[must_use]
pub fn list_args(dataset: &str) -> Vec<String> {
    [
        "list",
        "-H",
        "-p",
        "-r",
        "-t",
        "filesystem",
        "-o",
        "name,used,referenced,refquota,mountpoint,mounted",
        dataset,
    ]
    .map(String::from)
    .to_vec()
}

/// [`list_args`]'s output: tab-separated, sizes in bytes.
///
/// # Errors
///
/// A line without six fields, or a size that is not a number.
pub fn parse_list(out: &str) -> Result<Vec<Row>, String> {
    let num = |s: &str| -> Result<u64, String> {
        if s == "-" || s == "none" {
            Ok(0)
        } else {
            s.parse()
                .map_err(|_| format!("zfs list: {s:?} is not a size"))
        }
    };
    out.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [name, used, referenced, refquota, mountpoint, mounted] = f[..] else {
                return Err(format!("zfs list: {} fields in {line:?}", f.len()));
            };
            Ok(Row {
                name: name.to_string(),
                used: num(used)?,
                referenced: num(referenced)?,
                refquota: num(refquota)?,
                mountpoint: (!matches!(mountpoint, "-" | "none" | "legacy"))
                    .then(|| mountpoint.to_string()),
                mounted: mounted == "yes",
            })
        })
        .collect()
}

/// What making sure of a volume takes, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Make the app's dataset, which holds its volumes and mounts nothing.
    CreateApp,
    /// Make the volume with the fixed properties and its quota.
    Create,
    /// Change its quota.
    SetQuota,
    /// Mount it (after a reboot, or a destroy that failed half way).
    Mount,
}

/// The steps that bring `volume` (its row, if it exists) to `refquota`,
/// mounted, where `app_exists` says whether the app's dataset does.
#[must_use]
pub fn ensure_steps(app_exists: bool, volume: Option<&Row>, refquota: u64) -> Vec<Step> {
    let Some(row) = volume else {
        let mut steps = Vec::new();
        if !app_exists {
            steps.push(Step::CreateApp);
        }
        steps.push(Step::Create);
        return steps;
    };
    let mut steps = Vec::new();
    if row.refquota != refquota {
        steps.push(Step::SetQuota);
    }
    if !row.mounted {
        steps.push(Step::Mount);
    }
    steps
}

/// The arguments for `step` on `app`'s `volume` under `parent`.
#[must_use]
pub fn step_args(step: Step, parent: &str, app: &str, volume: &str, refquota: u64) -> Vec<String> {
    let app_ds = format!("{parent}/{app}");
    let vol_ds = format!("{app_ds}/{volume}");
    let o = |p: &str| ["-o".to_string(), p.to_string()];
    match step {
        Step::CreateApp => {
            let mut a = vec!["create".to_string()];
            a.extend(o("canmount=off"));
            a.push(app_ds);
            a
        }
        Step::Create => {
            let mut a = vec!["create".to_string()];
            for p in FIXED {
                a.extend(o(p));
            }
            a.extend(o(&format!("refquota={refquota}")));
            a.push(vol_ds);
            a
        }
        Step::SetQuota => vec!["set".into(), format!("refquota={refquota}"), vol_ds],
        Step::Mount => vec!["mount".into(), vol_ds],
    }
}

/// The arguments removing a volume and its snapshots (`-r`; never `-R`,
/// which would follow clones elsewhere).
#[must_use]
pub fn destroy_args(dataset: &str) -> Vec<String> {
    vec!["destroy".into(), "-r".into(), dataset.into()]
}

/// The volumes in `rows` (from [`list_args`] on `parent`): the
/// filesystems two levels below it, as app and volume.
#[must_use]
pub fn volumes<'a>(parent: &str, rows: &'a [Row]) -> Vec<(&'a str, &'a str, &'a Row)> {
    rows.iter()
        .filter_map(|r| {
            let rest = r.name.strip_prefix(parent)?.strip_prefix('/')?;
            let (app, volume) = rest.split_once('/')?;
            (!volume.contains('/')).then_some((app, volume, r))
        })
        .collect()
}

/// Every filesystem under `parent`, `parent` included.
///
/// # Errors
///
/// The command failed, or its output did not parse.
pub fn list(program: &Path, parent: &str) -> Result<Vec<Row>, String> {
    parse_list(&run(program, &list_args(parent))?)
}

/// Run `program` with `args`; its standard output, or its standard error
/// (or why it did not run) as the failure.
///
/// # Errors
///
/// The command failed or could not start.
pub fn run(program: &Path, args: &[String]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/sbin:/bin:/usr/sbin:/usr/bin")
        .output()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("zfs {}: {}", args[0], err.trim()))
    }
}

#[cfg(test)]
#[path = "zfs_tests.rs"]
mod zfs_tests;

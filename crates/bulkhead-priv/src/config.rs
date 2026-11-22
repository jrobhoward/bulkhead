//! The helper's configuration: its own file, owned by root, never the
//! node's. Every check the helper makes comes from here.
//!
//! One `key = value` per line, `#` to the end of a line a comment, lists
//! comma-separated, sizes in bytes or with `KiB`, `MiB`, `GiB` or `TiB`:
//!
//! ```text
//! node = /usr/local/bin/bulkhead                    # the node's executable
//! node_args = node --config /etc/bulkhead/node.toml # its arguments, split at spaces
//! node_uid = 990                                    # the node's user and group (numbers)
//! node_gid = 990
//! runner = /usr/local/libexec/bulkhead-runner       # the only program an instance starts as
//! uids = 60000-60999                                # users for apps, one per app
//! state = /var/lib/bulkhead-priv                    # the helper's own state (0700, root)
//! binds = /var/lib/bulkhead, /srv/bulkhead          # what an instance's directories may be under
//! memory_max = 8GiB                                 # ceilings on what a spawn may ask
//! pids_max = 4096
//! ports = 80, 443                                   # ports below 1024 the node may have bound
//! cgroup = /sys/fs/cgroup/system.slice/bulkhead.service  # Linux: a cgroup delegated to the helper
//! tmp_size = 16MiB                                  # each instance's private /tmp
//! devfs_ruleset = 4290                              # FreeBSD: the devfs ruleset the helper defines for jails
//! zfs_parent = tank/bulkhead/volumes                # ZFS volumes (ROADMAP ZV1): their parent dataset
//! zfs_max_quota = 50GiB                             # the largest quota a volume may have
//! zfs = /usr/sbin/zfs                               # the zfs command (default /usr/sbin/zfs, /sbin/zfs on FreeBSD)
//! node_watchdog_s = 30                              # kill a node silent this long (ROADMAP B6); 0: never
//! ```

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

/// The helper's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The node's executable.
    pub node: PathBuf,
    /// Its arguments.
    pub node_args: Vec<String>,
    /// The user the node runs as: not root, not an app's.
    pub node_uid: u32,
    /// The node's group.
    pub node_gid: u32,
    /// `bulkhead-runner`: the program every instance starts as.
    pub runner: PathBuf,
    /// The users apps run as, one per app (their group the same number).
    pub uids: RangeInclusive<u32>,
    /// The helper's own state: which user belongs to which app, and (on
    /// Linux) the instances' root mount points.
    pub state: PathBuf,
    /// The directories an instance's directories may be under.
    pub binds: Vec<PathBuf>,
    /// The most memory a spawn may ask for one instance.
    pub memory_max: u64,
    /// The most processes and threads a spawn may ask for one instance.
    pub pids_max: u32,
    /// The ports the node may ask the helper to bind.
    pub ports: Vec<u16>,
    /// Linux: the cgroup v2 directory delegated to the helper; required
    /// there.
    pub cgroup: Option<PathBuf>,
    /// The size of each instance's private `/tmp`.
    pub tmp_size: u64,
    /// FreeBSD: the number of the devfs ruleset the helper defines at its
    /// start for its jails' `/dev` (`null`, `zero`, `random`, `urandom`,
    /// `pvclock`); any ruleset of that number is replaced.
    pub devfs_ruleset: u16,
    /// The apps whose instances may run unconfined (ROADMAP AP4): with the
    /// host's network and `/etc`, the runner's fences left off. Root's
    /// consent beside the node's; empty by default.
    pub unconfined: Vec<String>,
    /// ZFS volumes (ROADMAP ZV1), with `zfs_parent`; none without it.
    pub zfs: Option<ZfsConfig>,
    /// How long the node may go without telling the helper it is alive
    /// (`Request::Alive`) before the helper kills it, which ends its
    /// instances, and exits for the service manager to start again
    /// (ROADMAP B6). `None` (`node_watchdog_s = 0`): never. 30 s by
    /// default; at least twice `bulkhead_priv_wire::ALIVE_EVERY_S`.
    pub node_watchdog: Option<std::time::Duration>,
}

/// Where the helper makes apps' ZFS volumes, and their ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZfsConfig {
    /// The parent dataset: each app's volumes are at `PARENT/APP/VOLUME`,
    /// and nothing outside it is touched.
    pub parent: String,
    /// The largest quota (`refquota`) a volume may have, in bytes.
    pub max_quota: u64,
    /// The `zfs` command, by absolute path.
    pub program: PathBuf,
}

/// The `zfs` command's usual place on this system.
pub const ZFS_PROGRAM: &str = if cfg!(target_os = "freebsd") {
    "/sbin/zfs"
} else {
    "/usr/sbin/zfs"
};

/// Whether `name` is a dataset name the helper accepts as its parent: a
/// pool and children separated by `/`, each of letters, digits, `_`, `-`,
/// `.` and `:` and starting with a letter or digit; so no snapshot (`@`),
/// bookmark (`#`), space or option.
#[must_use]
pub fn is_dataset_name(name: &str) -> bool {
    name.len() <= 200
        && name.split('/').all(|part| {
            let b = part.as_bytes();
            !b.is_empty()
                && b[0].is_ascii_alphanumeric()
                && b.iter()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-.:".contains(c))
        })
}

impl Config {
    /// The configuration in `text`.
    ///
    /// # Errors
    ///
    /// A line that is not `key = value`, an unknown or repeated key, a
    /// missing one, or a value out of its bounds.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut kv = std::collections::BTreeMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("line {}: not key = value", n + 1))?;
            let k = k.trim();
            if !KEYS.contains(&k) {
                return Err(format!("line {}: unknown key {k}", n + 1));
            }
            if kv.insert(k.to_string(), v.trim().to_string()).is_some() {
                return Err(format!("line {}: {k} given twice", n + 1));
            }
        }
        let get = |k: &str| kv.get(k).ok_or_else(|| format!("{k} is missing"));
        let num = |k: &str| -> Result<u32, String> {
            get(k)?.parse().map_err(|_| format!("{k} is not a number"))
        };
        let path = |k: &str| absolute(k, get(k)?);
        let uids = {
            let v = get("uids")?;
            let (lo, hi) = v
                .split_once('-')
                .ok_or("uids is not a range like 60000-60999")?;
            let lo: u32 = lo.trim().parse().map_err(|_| "uids is not a range")?;
            let hi: u32 = hi.trim().parse().map_err(|_| "uids is not a range")?;
            if lo == 0 || lo > hi {
                return Err("uids must be a range above 0, low to high".into());
            }
            lo..=hi
        };
        let node_uid = num("node_uid")?;
        let node_gid = num("node_gid")?;
        if node_uid == 0 || node_gid == 0 {
            return Err("the node may not run as root".into());
        }
        if uids.contains(&node_uid) || uids.contains(&node_gid) {
            return Err("the node's user or group is inside uids".into());
        }
        let binds = list(get("binds")?)
            .map(|b| absolute("binds", b))
            .collect::<Result<Vec<_>, _>>()?;
        if binds.iter().any(|b| b == Path::new("/")) {
            return Err("binds may not include /".into());
        }
        let ports = match kv.get("ports") {
            Some(v) => list(v)
                .map(|p| p.parse().map_err(|_| format!("ports: {p} is not a port")))
                .collect::<Result<Vec<u16>, _>>()?,
            None => Vec::new(),
        };
        let cgroup = kv
            .get("cgroup")
            .map(|v| absolute("cgroup", v))
            .transpose()?;
        if let Some(c) = &cgroup
            && !c.starts_with("/sys/fs/cgroup/")
        {
            return Err("cgroup must be under /sys/fs/cgroup".into());
        }
        let unconfined = match kv.get("unconfined") {
            Some(v) => list(v)
                .map(|a| {
                    bulkhead_priv_wire::is_app_name(a)
                        .then(|| a.to_string())
                        .ok_or_else(|| format!("unconfined: {a} is not an app name"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        let zfs = match kv.get("zfs_parent") {
            Some(parent) => {
                if !is_dataset_name(parent) {
                    return Err(format!("zfs_parent: {parent} is not a dataset name"));
                }
                let max_quota = size(get("zfs_max_quota")?)
                    .filter(|q| *q > 0)
                    .ok_or("zfs_max_quota is not a size above 0")?;
                let program = match kv.get("zfs") {
                    Some(v) => absolute("zfs", v)?,
                    None => PathBuf::from(ZFS_PROGRAM),
                };
                Some(ZfsConfig {
                    parent: parent.clone(),
                    max_quota,
                    program,
                })
            }
            None if kv.contains_key("zfs_max_quota") || kv.contains_key("zfs") => {
                return Err("zfs_max_quota and zfs need zfs_parent".into());
            }
            None => None,
        };
        let node_watchdog = match kv.get("node_watchdog_s") {
            None => Some(30),
            Some(v) => match v.parse::<u64>() {
                Ok(0) => None,
                Ok(s) if s >= 2 * bulkhead_priv_wire::ALIVE_EVERY_S => Some(s),
                _ => {
                    return Err(format!(
                        "node_watchdog_s is not 0 or a number of seconds of at least {}",
                        2 * bulkhead_priv_wire::ALIVE_EVERY_S
                    ));
                }
            },
        }
        .map(std::time::Duration::from_secs);
        let pids_max = num("pids_max")?;
        let memory_max = size(get("memory_max")?).ok_or("memory_max is not a size")?;
        if pids_max == 0 || memory_max == 0 {
            return Err("memory_max and pids_max must be above 0".into());
        }
        Ok(Self {
            node: path("node")?,
            node_args: get("node_args")?
                .split_whitespace()
                .map(String::from)
                .collect(),
            node_uid,
            node_gid,
            runner: path("runner")?,
            uids,
            state: path("state")?,
            binds,
            memory_max,
            pids_max,
            ports,
            cgroup,
            tmp_size: match kv.get("tmp_size") {
                Some(v) => size(v).ok_or("tmp_size is not a size")?,
                None => 16 << 20,
            },
            devfs_ruleset: match kv.get("devfs_ruleset") {
                // 0 is no ruleset; 1 to 4 are the system's (devfs.rules).
                Some(v) => v
                    .parse()
                    .ok()
                    .filter(|n| *n > 4)
                    .ok_or("devfs_ruleset is not a number above 4")?,
                None => 4290,
            },
            unconfined,
            zfs,
            node_watchdog,
        })
    }

    /// Read and parse the file at `path`, which must be root's and
    /// writable by no one else.
    ///
    /// # Errors
    ///
    /// It cannot be read, belongs to another user, is writable by its
    /// group or others, or [`Config::parse`] refuses it.
    pub fn load(path: &Path) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;

        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err(format!(
                "{}: must be owned by root and writable by root only",
                path.display()
            ));
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

const KEYS: [&str; 19] = [
    "node",
    "node_args",
    "node_uid",
    "node_gid",
    "runner",
    "uids",
    "state",
    "binds",
    "memory_max",
    "pids_max",
    "ports",
    "cgroup",
    "tmp_size",
    "devfs_ruleset",
    "unconfined",
    "zfs_parent",
    "zfs_max_quota",
    "zfs",
    "node_watchdog_s",
];

fn list(v: &str) -> impl Iterator<Item = &str> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty())
}

fn absolute(key: &str, v: &str) -> Result<PathBuf, String> {
    crate::check::normal_absolute(v).ok_or_else(|| {
        format!("{key}: {v} is not an absolute path without `.`, `..` or empty parts")
    })
}

/// Bytes in `v`: a number, or one with `KiB`, `MiB`, `GiB` or `TiB`.
#[must_use]
pub fn size(v: &str) -> Option<u64> {
    let v = v.trim();
    let (digits, shift) = [("TiB", 40), ("GiB", 30), ("MiB", 20), ("KiB", 10)]
        .iter()
        .find_map(|(unit, shift)| v.strip_suffix(unit).map(|d| (d.trim(), *shift)))
        .unwrap_or((v, 0));
    let n: u64 = digits.parse().ok()?;
    n.checked_mul(1 << shift)
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;

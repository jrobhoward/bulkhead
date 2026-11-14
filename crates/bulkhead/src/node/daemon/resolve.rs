//! The node's resolver of host names (ROADMAP DA9): the driver
//! `bulkhead-resolve`, a native app of the node's own (`RESOLVE_APP`)
//! started on every member, so no DNS reply is parsed in the node.
//!
//! The node reads `resolv.conf` and the hosts file by path (a file
//! replaced by rename, as DHCP clients and NetworkManager do, is seen) and
//! gives their text to the driver as init arguments. It parses only
//! `resolv.conf`'s `nameserver` lines, or takes `[resolver] nameservers`,
//! for the one network right the driver gets: TCP to those addresses
//! (`bulkhead:sockets` connect rules). The files are read again every
//! `reload_s`, on SIGHUP and on `bulkhead reload-tls`; on a change the
//! driver restarts with them, since an instance's rules are fixed.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bulkhead_bundle::manifest::{AppManifest, NativeProgram};
use bulkhead_host::{AppDeployment, Apps, NativeCode, Runtime};

use super::builtin;
use crate::node::config::{
    ClusterRole, MAX_NAMESERVERS, NodeConfig, RESOLVE_APP, ResolverBinding, nameserver,
};
use crate::node::error::NodeError;

/// The largest `resolv.conf` or hosts file read.
const MAX_FILE: u64 = 1 << 20;

/// The streams the driver may hold open at once, across its nameservers.
const MAX_STREAMS: u32 = 16;

/// What the driver runs with: the files' text and the nameservers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    resolv_conf: String,
    hosts: String,
    nameservers: Vec<SocketAddr>,
}

impl Snapshot {
    /// Both files as they are now, and the nameservers: `[resolver]
    /// nameservers`, else `resolv.conf`'s.
    fn read(binding: &ResolverBinding) -> Result<Self, String> {
        let resolv_conf = read_text(&binding.resolv_conf, binding.nameservers.is_empty())?;
        let hosts = read_text(&binding.hosts, false)?;
        let nameservers: Vec<SocketAddr> = if binding.nameservers.is_empty() {
            nameservers_of(&resolv_conf)
        } else {
            binding
                .nameservers
                .iter()
                .filter_map(|n| nameserver(n))
                .collect()
        };
        if nameservers.is_empty() {
            return Err(format!(
                "{} names no nameserver, and [resolver] nameservers is unset",
                binding.resolv_conf.display()
            ));
        }
        Ok(Self {
            resolv_conf,
            hosts,
            nameservers,
        })
    }

    /// The driver's init arguments (`bulkhead_resolve::config::Config`).
    fn args(&self) -> serde_json::Value {
        serde_json::json!({
            "nameservers": self.nameservers.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "resolv_conf": self.resolv_conf,
            "hosts": self.hosts,
        })
    }
}

/// `path`'s text, at most [`MAX_FILE`] bytes; a missing file is empty
/// unless `required`.
fn read_text(path: &Path, required: bool) -> Result<String, String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(String::new());
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(format!("{}: over {MAX_FILE} bytes", path.display()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `resolv.conf`'s nameservers, as libc reads them: a line whose first
/// word is `nameserver`, an address after it, on port 53, the first
/// [`MAX_NAMESERVERS`]. An address with a scope (`fe80::1%eth0`) is
/// skipped: the driver's connect rules name addresses alone.
fn nameservers_of(resolv_conf: &str) -> Vec<SocketAddr> {
    resolv_conf
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            if words.next()? != "nameserver" {
                return None;
            }
            words.next()?.parse::<IpAddr>().ok()
        })
        .map(|ip| SocketAddr::new(ip, 53))
        .take(MAX_NAMESERVERS)
        .collect()
}

/// The driver's manifest: one instance; TCP to each nameserver and
/// nothing else; messages from itself alone (its worker hands back to its
/// callbacks by casting to its own instance); restarts that never give
/// up; its counters as `bulkhead_resolve_*`.
fn manifest(snapshot: &Snapshot) -> Result<AppManifest, NodeError> {
    let connect: Vec<serde_json::Value> = snapshot
        .nameservers
        .iter()
        .map(|addr| {
            let bits = if addr.is_ipv4() { 32 } else { 128 };
            serde_json::json!({
                "cidr": format!("{}/{bits}", addr.ip()),
                "ports": addr.port().to_string(),
            })
        })
        .collect();
    let spec = serde_json::json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": snapshot.args() },
        "supervision": builtin::supervision(),
        "limits": { "memory_max": "64MiB" },
        "messaging_acl": { "accept_from": [RESOLVE_APP], "may_call": [RESOLVE_APP] },
        "capabilities": {
            "bulkhead:sockets": { "connect": connect, "max_sockets": MAX_STREAMS },
            "bulkhead:metrics": { "prefix": "bulkhead_resolve" },
        },
    });
    builtin::manifest(RESOLVE_APP, "The node's resolver of host names", &spec)
}

/// The resolver driver, on a member; nothing on a witness, which runs no
/// apps and resolves no names.
#[derive(Debug, Clone, Default)]
pub(super) struct Resolver {
    inner: Option<Arc<Inner>>,
}

#[derive(Debug)]
struct Inner {
    rt: Runtime,
    apps: Apps,
    program: PathBuf,
    binding: ResolverBinding,
    /// What the running driver was started with.
    current: tokio::sync::Mutex<Snapshot>,
}

impl Inner {
    async fn start(&self, snapshot: &Snapshot) -> Result<(), NodeError> {
        let code = self.rt.load_native(NativeCode {
            path: self.program.clone(),
            kind: NativeProgram::Executable,
        });
        builtin::start(
            &self.rt,
            &self.apps,
            AppDeployment::new(manifest(snapshot)?, code),
        )
        .await
    }
}

impl Resolver {
    /// Start the driver on a member.
    ///
    /// # Errors
    ///
    /// Its program is missing, the files name no nameserver, or it did
    /// not start.
    pub(super) async fn open(
        config: &NodeConfig,
        rt: &Runtime,
        apps: &Apps,
    ) -> Result<Self, NodeError> {
        if config.cluster.role == ClusterRole::Witness {
            return Ok(Self::default());
        }
        let program = builtin::program(config, config.native.resolve.as_ref(), RESOLVE_APP);
        if !program.is_file() {
            return Err(NodeError::Invalid(format!(
                "the node resolves host names through {RESOLVE_APP}, and {} is not it \
                 (install it beside bulkhead-runner, or set [native] resolve)",
                program.display()
            )));
        }
        let snapshot = Snapshot::read(&config.resolver)
            .map_err(|e| NodeError::Invalid(format!("[resolver]: {e}")))?;
        let inner = Inner {
            rt: rt.clone(),
            apps: apps.clone(),
            program,
            binding: config.resolver.clone(),
            current: tokio::sync::Mutex::new(snapshot.clone()),
        };
        inner.start(&snapshot).await?;
        tracing::info!(
            nameservers = snapshot.nameservers.len(),
            "resolver of host names started"
        );
        Ok(Self {
            inner: Some(Arc::new(inner)),
        })
    }

    /// Read the files again and restart the driver if they changed:
    /// whether it restarted.
    ///
    /// # Errors
    ///
    /// The files could not be read or name no nameserver (the driver keeps
    /// running with the old), or the new driver did not start (tried again
    /// at the next reload).
    pub(super) async fn reload(&self) -> Result<bool, String> {
        let Some(inner) = &self.inner else {
            return Ok(false);
        };
        let next = Snapshot::read(&inner.binding)?;
        let mut current = inner.current.lock().await;
        if *current == next {
            return Ok(false);
        }
        // Lookups in flight end with it; the runtime asks the new one.
        inner.apps.take_out(RESOLVE_APP).await;
        inner.start(&next).await.map_err(|e| e.to_string())?;
        *current = next;
        Ok(true)
    }

    /// Reload every `reload_s` until `stop`.
    pub(super) fn watch(&self, mut stop: tokio::sync::watch::Receiver<bool>) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner.binding.reload_s == 0 {
            return;
        }
        let every = Duration::from_secs(inner.binding.reload_s);
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.wait_for(|s| *s) => return,
                    () = tokio::time::sleep(every) => {}
                }
                match this.reload().await {
                    Ok(true) => {
                        tracing::info!("resolver restarted with changed resolv.conf or hosts")
                    }
                    Ok(false) => {}
                    Err(e) => tracing::error!(error = %e, "resolver files not reloaded"),
                }
            }
        });
    }
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;

//! Volumes (ROADMAP ZV1): where each volume a manifest names comes from,
//! a directory of node config's `[volumes]` or, with `[volumes.zfs]`, the
//! app's own ZFS dataset the privileged helper makes; and the operator's
//! `bulkhead volume list` and `destroy`; and the volumes' use as metrics
//! (ROADMAP ZV2), kept from the helper's list and refreshed every
//! [`REFRESH`] and after each change, since a scrape cannot wait on it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bulkhead_priv_wire::VolumeRow;

use bulkhead_bundle::manifest::AppManifest;

use serde_json::json;

use super::Node;
use crate::api::{Done, VolumeInfo};
use crate::node::config::{NodeConfig, Volumes};
use crate::node::error::NodeError;

/// How often the kept ZFS volumes are read again from the helper (each
/// read runs `zfs list`, about 9 ms on a Raspberry Pi 5).
pub(crate) const REFRESH: Duration = Duration::from_secs(30);

/// The ZFS volumes as the helper last listed them, for metrics.
pub(crate) type Kept = Arc<Mutex<Vec<VolumeRow>>>;

/// Read the volumes from the helper into `kept`; a failure keeps the last
/// list and is logged.
pub(crate) async fn refresh(rt: &bulkhead_host::Runtime, kept: &Kept) {
    match rt.volumes().await {
        Ok((_, rows)) => *kept.lock().unwrap_or_else(PoisonError::into_inner) = rows,
        Err(e) => tracing::warn!(error = %e, "ZFS volumes not listed for metrics"),
    }
}

/// With `[volumes.zfs]`, keep `kept` fresh every [`REFRESH`] until `stop`.
pub(crate) fn start_refresh(
    config: &NodeConfig,
    rt: &bulkhead_host::Runtime,
    kept: &Kept,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    if config.volumes.zfs.is_none() {
        return;
    }
    let (rt, kept) = (rt.clone(), kept.clone());
    tokio::spawn(async move {
        loop {
            refresh(&rt, &kept).await;
            tokio::select! {
                _ = stop.wait_for(|s| *s) => return,
                () = tokio::time::sleep(REFRESH) => {}
            }
        }
    });
}

/// A metric family: its name, its help, and its value from a row.
type Family = (&'static str, &'static str, fn(&VolumeRow) -> u64);

/// The kept volumes in the Prometheus text format: what each refers to
/// (what its quota counts), what it uses with its snapshots, and its
/// quota, labelled by app and volume. Nothing without a volume.
pub(crate) fn metrics(kept: &Kept) -> String {
    let rows = kept.lock().unwrap_or_else(PoisonError::into_inner);
    if rows.is_empty() {
        return String::new();
    }
    let mut text = String::new();
    let families: [Family; 3] = [
        (
            "bulkhead_volume_referenced_bytes",
            "Bytes a ZFS volume refers to, which its quota counts.",
            |r| r.referenced,
        ),
        (
            "bulkhead_volume_used_bytes",
            "Bytes a ZFS volume uses, its snapshots included.",
            |r| r.used,
        ),
        (
            "bulkhead_volume_quota_bytes",
            "A ZFS volume's quota in bytes.",
            |r| r.refquota,
        ),
    ];
    for (name, help, value) in families {
        let _ = writeln!(text, "# HELP {name} {help}\n# TYPE {name} gauge");
        for r in rows.iter() {
            let _ = writeln!(
                text,
                "{name}{{app=\"{}\",volume=\"{}\"}} {}",
                r.app,
                r.volume,
                value(r)
            );
        }
    }
    text
}

/// Where a volume comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// A host directory from `[volumes]`.
    Dir(PathBuf),
    /// The app's own ZFS dataset, with this quota in bytes.
    Zfs {
        /// Its `refquota`.
        refquota: u64,
    },
}

/// Each volume `manifest`'s preopens name, and where it comes from on a
/// node with `volumes`: a directory takes no quota (the operator's to set
/// on its filesystem); a ZFS volume needs one, at most `max_quota`.
///
/// # Errors
///
/// A quota on a directory, a ZFS volume without one or over the ceiling,
/// a volume the node has nowhere to put, or one named twice with
/// different quotas.
pub(crate) fn sources(
    volumes: &Volumes,
    manifest: &AppManifest,
) -> Result<BTreeMap<String, Source>, String> {
    let mut out = BTreeMap::new();
    let Some(fs) = &manifest.spec.capabilities.filesystem else {
        return Ok(out);
    };
    for (guest, preopen) in &fs.preopens {
        let name = &preopen.volume;
        let quota = preopen.quota.map(|q| q.bytes());
        let source = match (volumes.dirs.get(name), &volumes.zfs, quota) {
            (Some(_), _, Some(_)) => {
                return Err(format!(
                    "volume '{name}' (for {guest}) is a directory, and Bulkhead enforces no \
                     quota on one: the operator sets a filesystem quota on it instead"
                ));
            }
            (Some(dir), _, None) => Source::Dir(dir.clone()),
            (None, Some(zfs), Some(q)) if q <= zfs.max_quota.bytes() => Source::Zfs { refquota: q },
            (None, Some(zfs), Some(_)) => {
                return Err(format!(
                    "volume '{name}' (for {guest}): its quota is over the node's \
                     [volumes.zfs] max_quota {}",
                    zfs.max_quota
                ));
            }
            (None, Some(_), None) => {
                return Err(format!(
                    "volume '{name}' (for {guest}) is the app's own ZFS volume, and needs a \
                     quota"
                ));
            }
            (None, None, _) => {
                return Err(format!("volume '{name}' is not configured on this node"));
            }
        };
        if let Some(earlier) = out.insert(name.clone(), source.clone())
            && earlier != source
        {
            return Err(format!(
                "volume '{name}' is named twice with different quotas"
            ));
        }
    }
    Ok(out)
}

/// With `[volumes.zfs]`, that the privileged helper keeps ZFS volumes
/// under the same parent as node config says: a check of two files the
/// operator wrote, so a mistake in either shows at the start, not at the
/// first deploy. The helper's own configuration is the one that counts.
///
/// # Errors
///
/// The helper keeps no ZFS volumes, or under another parent.
pub(crate) async fn check_parent(
    config: &NodeConfig,
    rt: &bulkhead_host::Runtime,
) -> Result<(), NodeError> {
    let Some(zfs) = &config.volumes.zfs else {
        return Ok(());
    };
    let (parent, rows) = rt
        .volumes()
        .await
        .map_err(|e| NodeError::Invalid(format!("[volumes.zfs]: {e}")))?;
    if parent != zfs.parent {
        return Err(NodeError::Invalid(format!(
            "[volumes.zfs] parent is {}, and the privileged helper's zfs_parent is {parent}",
            zfs.parent
        )));
    }
    tracing::info!(
        parent,
        volumes = rows.len(),
        "ZFS volumes through the privileged helper"
    );
    Ok(())
}

impl Node {
    /// The node's volumes: `[volumes]`'s directories, and with
    /// `[volumes.zfs]` every app's ZFS volume the helper keeps.
    ///
    /// # Errors
    ///
    /// The helper refused or has gone.
    pub async fn volumes(&self) -> Result<Vec<VolumeInfo>, NodeError> {
        let mut out: Vec<VolumeInfo> = self
            .config
            .volumes
            .dirs
            .iter()
            .map(|(name, dir)| VolumeInfo {
                name: name.clone(),
                kind: "directory".into(),
                app: None,
                path: dir.display().to_string(),
                used: None,
                referenced: None,
                quota: None,
                mounted: true,
                deployed: None,
                snapshot: None,
            })
            .collect();
        if self.config.volumes.zfs.is_none() {
            return Ok(out);
        }
        let (_, rows) = self.rt.volumes().await.map_err(NodeError::Internal)?;
        rows.clone_into(
            &mut self
                .volume_rows
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let deployed = |app: &str| self.st().desired.apps.contains_key(app);
        out.extend(rows.into_iter().map(|r| VolumeInfo {
            deployed: Some(deployed(&r.app)),
            name: r.volume,
            kind: "zfs".into(),
            app: Some(r.app),
            path: r.mountpoint,
            used: Some(r.used),
            referenced: Some(r.referenced),
            quota: Some(r.refquota),
            mounted: r.mounted,
            snapshot: r.snapshot,
        }));
        Ok(out)
    }

    /// Before `app` moves from version `from` to `to`, snapshot each of its
    /// ZFS volumes as `@upgrade` (ROADMAP ZV3), replacing the last
    /// upgrade's, so an operator can roll the volume back if the new
    /// version wrote what it should not have. Nothing without
    /// `[volumes.zfs]` or a volume.
    ///
    /// # Errors
    ///
    /// [`NodeError::Refused`] for a volume the helper could not snapshot:
    /// the upgrade does not go ahead.
    pub(super) async fn snapshot_volumes(
        &self,
        app: &str,
        from: &str,
        to: &str,
    ) -> Result<(), NodeError> {
        if self.config.volumes.zfs.is_none() {
            return Ok(());
        }
        let (_, rows) = self.rt.volumes().await.map_err(NodeError::Internal)?;
        let mine: Vec<String> = rows
            .into_iter()
            .filter(|r| r.app == app)
            .map(|r| r.volume)
            .collect();
        if mine.is_empty() {
            return Ok(());
        }
        for volume in &mine {
            self.rt.snapshot_volume(app, volume).await.map_err(|e| {
                NodeError::Refused(format!(
                    "volume '{volume}' not snapshot before the upgrade: {e}"
                ))
            })?;
        }
        self.audit.record(
            "volume_snapshot",
            json!({ "app": app, "volumes": mine, "from": from, "to": to }),
        );
        refresh(&self.rt, &self.volume_rows).await;
        Ok(())
    }

    /// Roll `app`'s ZFS volume `volume` back to the snapshot its last
    /// upgrade took (ROADMAP ZV3), losing what was written since. Only
    /// while the app is stopped or undeployed; the helper refuses too while
    /// any of its instances runs.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] without `[volumes.zfs]`;
    /// [`NodeError::Conflict`] while the app runs, or the helper refused
    /// (no such volume or snapshot, a newer snapshot of the operator's).
    pub async fn rollback_volume(&self, app: &str, volume: &str) -> Result<Done, NodeError> {
        if self.config.volumes.zfs.is_none() {
            return Err(NodeError::Invalid(
                "this node has no ZFS volumes ([volumes.zfs])".into(),
            ));
        }
        let _change = self.change.lock().await;
        if self.st().desired.apps.get(app).is_some_and(|a| a.running) {
            return Err(NodeError::Conflict(format!(
                "{app} is running; stop it first (bulkhead stop {app}): a rollback changes the \
                 files under its instances"
            )));
        }
        self.rt
            .rollback_volume(app, volume)
            .await
            .map_err(NodeError::Conflict)?;
        self.audit.record(
            "volume_rolled_back",
            json!({ "app": app, "volume": volume }),
        );
        refresh(&self.rt, &self.volume_rows).await;
        Ok(Done {
            message: format!("{app}'s volume {volume} rolled back to its upgrade snapshot"),
        })
    }

    /// Remove `app`'s ZFS volume `volume` and what it holds, once the app
    /// is not deployed (an undeploy keeps its volumes); the helper refuses
    /// too while any of its instances runs.
    ///
    /// # Errors
    ///
    /// [`NodeError::Invalid`] without `[volumes.zfs]`;
    /// [`NodeError::Conflict`] while the app is deployed, or the helper
    /// refused (an instance still running, the dataset busy, no such
    /// volume).
    pub async fn destroy_volume(&self, app: &str, volume: &str) -> Result<Done, NodeError> {
        if self.config.volumes.zfs.is_none() {
            return Err(NodeError::Invalid(
                "this node has no ZFS volumes ([volumes.zfs]); a directory volume is the \
                 operator's to remove"
                    .into(),
            ));
        }
        let _change = self.change.lock().await;
        if self.st().desired.apps.contains_key(app) {
            return Err(NodeError::Conflict(format!(
                "{app} is deployed; undeploy it first (bulkhead remove {app}), which keeps \
                 its volumes"
            )));
        }
        self.rt
            .destroy_volume(app, volume)
            .await
            .map_err(NodeError::Conflict)?;
        self.audit
            .record("volume_destroyed", json!({ "app": app, "volume": volume }));
        refresh(&self.rt, &self.volume_rows).await;
        Ok(Done {
            message: format!("{app}'s volume {volume} destroyed"),
        })
    }
}

#[cfg(test)]
#[path = "volumes_tests.rs"]
mod volumes_tests;

//! Volumes (ROADMAP ZV1): where each volume a manifest names comes from,
//! a directory of node config's `[volumes]` or, with `[volumes.zfs]`, the
//! app's own ZFS dataset the privileged helper makes; and the operator's
//! `bulkhead volume list` and `destroy`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bulkhead_bundle::manifest::AppManifest;

use serde_json::json;

use super::Node;
use crate::api::{Done, VolumeInfo};
use crate::node::config::{NodeConfig, Volumes};
use crate::node::error::NodeError;

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
                quota: None,
                mounted: true,
                deployed: None,
            })
            .collect();
        if self.config.volumes.zfs.is_none() {
            return Ok(out);
        }
        let (_, rows) = self.rt.volumes().await.map_err(NodeError::Internal)?;
        let deployed = |app: &str| self.st().desired.apps.contains_key(app);
        out.extend(rows.into_iter().map(|r| VolumeInfo {
            deployed: Some(deployed(&r.app)),
            name: r.volume,
            kind: "zfs".into(),
            app: Some(r.app),
            path: r.mountpoint,
            used: Some(r.used),
            quota: Some(r.refquota),
            mounted: r.mounted,
        }));
        Ok(out)
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
        Ok(Done {
            message: format!("{app}'s volume {volume} destroyed"),
        })
    }
}

#[cfg(test)]
#[path = "volumes_tests.rs"]
mod volumes_tests;

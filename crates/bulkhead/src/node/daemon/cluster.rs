//! The cluster transport, failover and the witness.

use std::fmt::Write as _;
use std::time::Duration;

use crate::api::{Done, DrInfo, FailoverInfo, MemberInfo, ReleaseInfo, ReplicationInfo};
use bulkhead_host::DrConfig;
use bulkhead_host::failover::{Arbiter, FailoverConfig, SwitchoverRefused, WitnessConfig};
use bulkhead_host::transport::quic::{NodeIdentity, QuicConfig, QuicTransport};
use bulkhead_host::transport::wire;
use bulkhead_host::{Finalized, Role};
use serde_json::json;

use super::Node;
use crate::node::config::{ArbiterKind, BucketScope};
use crate::node::error::NodeError;
use crate::node::gate;

impl Node {
    /// The cluster's members as this node knows them, itself included.
    #[must_use]
    pub fn members(&self) -> Vec<MemberInfo> {
        let me = self.rt.node();
        self.rt
            .members()
            .into_iter()
            .map(|m| MemberInfo {
                reachable: m.node == me || self.rt.peer_status(m.node).is_some_and(|s| s.reachable),
                node: m.node,
                creation: m.creation,
                address: m.address,
                role: match m.role {
                    Role::Witness => "witness".into(),
                    _ => "member".into(),
                },
            })
            .collect()
    }

    /// Start the QUIC transport on `cluster.address`, join the seeds, and
    /// watch the identity files.
    pub(super) fn start_cluster(&self) -> Result<(), NodeError> {
        let cluster = &self.config.cluster;
        let (Some(address), Some(key), Some(peers)) =
            (&cluster.address, &cluster.key, &cluster.peers)
        else {
            return Ok(());
        };
        let bind = address
            .parse()
            .map_err(|e| NodeError::Invalid(format!("cluster.address: {e}")))?;
        let quic = QuicTransport::start(
            &self.rt,
            QuicConfig {
                bind,
                identity: NodeIdentity {
                    key: key.clone(),
                    peers: peers.clone(),
                },
                idle_timeout: Duration::from_millis(cluster.node_timeout_ms.max(1) * 2),
                send_buffer_bytes: cluster.send_buffer_bytes,
            },
        )
        .map_err(|e| NodeError::Invalid(e.to_string()))?;
        tracing::info!(%bind, "cluster transport listening");
        for seed in &cluster.seeds {
            let Ok(seed) = seed.parse() else {
                continue;
            };
            let quic = quic.clone();
            tokio::spawn(async move {
                match quic.join_seed(seed).await {
                    Ok(node) => tracing::info!(%seed, node, "joined through seed"),
                    Err(e) => tracing::warn!(error = %e, "seed not reached yet; retrying"),
                }
            });
        }
        if cluster.identity_reload_s > 0 {
            let (quic, every) = (quic.clone(), Duration::from_secs(cluster.identity_reload_s));
            let mut stop = self.stop.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.wait_for(|s| *s) => return,
                        () = tokio::time::sleep(every) => {}
                    }
                    match quic.reload_if_changed() {
                        Ok(true) => tracing::info!("cluster key or peers reloaded"),
                        Ok(false) => {}
                        Err(e) => {
                            tracing::error!(error = %e, "cluster identity reload failed; keeping the old")
                        }
                    }
                }
            });
        }
        *self
            .quic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(quic);
        Ok(())
    }

    /// Start `[failover]` at the epoch kept on disk and follow its
    /// leadership, or serve `[witness]` from its kept state.
    pub(super) fn start_failover(&self) -> Result<(), NodeError> {
        if let Some(w) = &self.config.witness {
            let mut cfg = WitnessConfig::new(w.pair, w.first);
            cfg.lease = Duration::from_millis(w.lease_ms);
            cfg.margin = Duration::from_millis(w.margin_ms);
            cfg.resume = self.store.load_witness()?;
            self.rt
                .serve_witness_kept(cfg, Some(self.store.witness_persist()))
                .map_err(|e| NodeError::Invalid(format!("witness: {e}")))?;
            tracing::info!(pair = ?w.pair, "serving as witness");
        }
        if let Some(d) = &self.config.dr {
            let mut cfg = DrConfig::new(d.pair);
            cfg.poll = Duration::from_millis(d.poll_ms);
            cfg.batch_bytes = d.batch_bytes.bytes();
            cfg.rpo_target = Duration::from_secs(d.rpo_target_s);
            cfg.log_bytes = d.log_max.bytes();
            cfg.log_age = Duration::from_secs(d.log_age_s);
            self.rt
                .start_dr(cfg)
                .map_err(|e| NodeError::Invalid(format!("dr: {e}")))?;
            tracing::info!(pair = ?d.pair, "keeping a DR copy of the pair's replicated log");
        }
        let (Some(f), Some(gate)) = (&self.config.failover, &self.gate) else {
            return Ok(());
        };
        let arbiter = match (f.arbiter, f.witness) {
            (ArbiterKind::Witness, Some(w)) => Arbiter::Witness(w),
            (ArbiterKind::Operator, _) => Arbiter::Operator,
            _ => Arbiter::Paths,
        };
        #[cfg(unix)]
        if let Some(serial) = &f.serial {
            use crate::node::serial_path::{self, Line, PATH};
            let line = Line {
                device: serial.device.clone(),
                baud: serial.baud,
            };
            let path = serial_path::start(
                self.rt.clone(),
                self.config.node,
                f.partner,
                line,
                self.stop.subscribe(),
            );
            self.rt.add_heartbeat_path(PATH, path);
        }
        let epoch = self.store.load_epoch()?;
        let mut cfg = FailoverConfig::new(f.partner, arbiter);
        cfg.lease = Duration::from_millis(f.lease_ms);
        cfg.margin = Duration::from_millis(f.margin_ms);
        cfg.epoch = epoch;
        cfg.replication_log_bytes = f.replication_log_max.bytes();
        cfg.replication_log_age = Duration::from_secs(f.replication_log_age_s);
        cfg.replication = f.replication;
        cfg.dr = f.dr;
        let handle = self
            .rt
            .start_failover(cfg)
            .map_err(|e| NodeError::Invalid(format!("failover: {e}")))?;
        tracing::info!(partner = f.partner, epoch, "failover started");
        #[cfg(feature = "http")]
        {
            // A singleton runs on the partner while this node is its
            // standby and hears it.
            let (gate, rt, partner) = (gate.clone(), self.rt.clone(), f.partner);
            let leadership = handle.subscribe();
            let elsewhere: bulkhead_host::Elsewhere = std::sync::Arc::new(move |app| {
                (leadership.borrow().standing == bulkhead_host::failover::Standing::Standby
                    && gate.holds(app)
                    && rt.peer_status(partner).is_some_and(|s| s.reachable))
                .then_some(partner)
            });
            // The runtime holds it weakly; the node keeps it for its life.
            self.rt.set_elsewhere(&elsewhere);
            let _ = self.elsewhere.set(super::KeptElsewhere(elsewhere));
        }
        tokio::spawn(gate::follow(
            gate.clone(),
            self.rt.clone(),
            self.store.epoch_saver(),
            epoch,
            handle.subscribe(),
            self.stop.subscribe(),
        ));
        let mut changes = handle.subscribe();
        let (generation, mut stop) = (self.leadership.clone(), self.stop.subscribe());
        let events = self.events.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = changes.changed() => if changed.is_err() { return },
                    _ = stop.wait_for(|s| *s) => return,
                }
                generation.send_modify(|g| *g += 1);
                let now = changes.borrow_and_update().clone();
                events.emit(
                    "failover",
                    "leadership",
                    json!({
                        "role": gate::role_name(now.standing),
                        "epoch": now.epoch,
                        "may_run": now.may_run,
                        "awaiting_confirmation": now.awaiting_confirmation,
                        "generation": *generation.borrow(),
                    }),
                );
            }
        });
        let _ = self.failover.set(handle);
        Ok(())
    }

    /// This node's failover standing.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`.
    pub fn failover(&self) -> Result<FailoverInfo, NodeError> {
        let (Some(f), Some(handle)) = (&self.config.failover, self.failover.get()) else {
            return Err(NodeError::NotFound(
                "this node has no [failover] in its config".into(),
            ));
        };
        let now = handle.leadership();
        let singletons = self.gate.as_ref().map(|g| g.names()).unwrap_or_default();
        let status = self.rt.peer_status(f.partner);
        Ok(FailoverInfo {
            partner: f.partner,
            arbiter: match f.arbiter {
                ArbiterKind::Witness => format!("witness {}", f.witness.unwrap_or_default()),
                ArbiterKind::Paths => "paths".into(),
                ArbiterKind::Operator => "operator".into(),
            },
            role: gate::role_name(now.standing).into(),
            epoch: now.epoch,
            may_run: now.may_run,
            awaiting_confirmation: now.awaiting_confirmation,
            partner_reachable: status.as_ref().is_some_and(|s| s.reachable),
            paths: status
                .map(|s| s.paths.into_iter().collect())
                .unwrap_or_default(),
            singletons,
            generation: *self.leadership.borrow(),
            partner_outdated: now.partner_outdated,
            replication: self.rt.replication_status().map(|s| ReplicationInfo {
                buckets: self
                    .config
                    .buckets
                    .iter()
                    .filter(|(_, b)| b.scope == BucketScope::Cluster)
                    .map(|(name, _)| name.clone())
                    .collect(),
                commit: s.commit,
                last: s.last,
                base: s.base,
                log_bytes: s.log_bytes,
                partner_matched: s.partner_matched,
                alone: s.alone,
                dr_lag_ms: self.rt.dr_lag().map(millis),
                stale: s.stale,
                reseeded: s.reseeded,
            }),
        })
    }

    /// This node's copy of its pair's replicated log, as the DR node.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[dr]`.
    pub fn dr(&self) -> Result<DrInfo, NodeError> {
        let (Some(d), Some(s)) = (&self.config.dr, self.rt.dr_status()) else {
            return Err(NodeError::NotFound(
                "this node has no [dr] in its config".into(),
            ));
        };
        Ok(DrInfo {
            pair: d.pair,
            source: s.source,
            commit: s.commit,
            base: s.base,
            log_bytes: s.log_bytes,
            lag_ms: millis(s.lag),
            rpo_target_ms: d.rpo_target_s.saturating_mul(1000),
            snapshotting: s.snapshotting,
            stopped: s.stopped,
            deploys: Some(self.held_info()),
            active_epoch: self.rt.dr_active(),
            reseeded: s.leading.map(|_| s.reseeded),
        })
    }

    /// Activate this node, the DR node and the pair's witness (ROADMAP
    /// DR4), once the operator confirms that site 1 is down or fenced:
    /// the witness waits out the pair's lease, grants this node a new
    /// epoch, the copy leads from what it holds, and the pair's apps start
    /// here.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[dr]`; [`NodeError::Refused`]
    /// without the confirmation, on a DR node that is not the witness, or
    /// while it does not hold the primary's deployments (none yet, still
    /// fetching them, or refused) unless `allow_missing_apps`;
    /// [`NodeError::Conflict`] when active already, fencing, or a pair node
    /// was heard while fencing.
    pub async fn dr_activate(
        &self,
        site_1_down: bool,
        allow_missing_apps: bool,
    ) -> Result<Done, NodeError> {
        use bulkhead_host::failover::ActivateRefused;
        let Some(status) = self.config.dr.as_ref().and(self.rt.dr_status()) else {
            return Err(NodeError::NotFound(
                "this node has no [dr] in its config".into(),
            ));
        };
        if self.config.witness.is_none() {
            return Err(NodeError::Refused(
                "only a DR node that is also the pair's witness can be activated (ROADMAP DR4b)"
                    .into(),
            ));
        }
        if !site_1_down {
            return Err(NodeError::Refused(format!(
                "activation needs your confirmation that site 1 is down or fenced and stays so \
                 (--site-1-down): a site-1 pair that still hears itself would go on running its \
                 singletons and writes beside this node. This copy holds entry {}, {} s behind \
                 the pair; writes site 1 committed after it are lost",
                status.commit,
                status.lag.as_secs(),
            )));
        }
        let short = self.held_short();
        if let Some(why) = &short
            && !allow_missing_apps
        {
            return Err(NodeError::Refused(format!(
                "{why}. Wait for it, or activate with --allow-missing-apps to run only what \
                 it holds"
            )));
        }
        self.audit.record(
            "dr_activate_asked",
            json!({
                "commit": status.commit,
                "lag_ms": millis(status.lag),
                "apps_short": short,
            }),
        );
        let epoch = self.rt.dr_activate().await.map_err(|e| match e {
            ActivateRefused::NotHere(why) => NodeError::Refused(why),
            ActivateRefused::NotKept(why) => NodeError::Internal(why),
            other => NodeError::Conflict(other.to_string()),
        })?;
        self.audit.record(
            "dr_activate",
            json!({ "epoch": epoch, "commit": status.commit, "lag_ms": millis(status.lag) }),
        );
        self.restore().await;
        let deployed: Vec<String> = self.st().desired.apps.keys().cloned().collect();
        let started = deployed.iter().filter(|name| self.running(name)).count();
        let mut message = format!(
            "activated at epoch {epoch}: this node leads from entry {} (writes site 1 \
             committed after it are lost) and runs {started} of {} apps; site 1's nodes are \
             fenced until failback",
            status.commit,
            deployed.len(),
        );
        if let Some(why) = short {
            let _ = write!(message, ". Warning: {why}");
        }
        for (app, why) in self.held_not_ready() {
            let _ = write!(message, ". Warning: {app} would not start here: {why}");
        }
        Ok(Done { message })
    }

    /// Hand back from this node, the active DR node, to pair node `to`
    /// (ROADMAP DR4's failback): its apps stop, its copy takes no more
    /// writes, both pair nodes must have reseeded up to its last entry
    /// within `wait`, then the witness promotes `to` at a new epoch and
    /// this node holds the pair's apps and pulls its copy again. On a
    /// refusal after the apps stopped, they start again.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[dr]`; [`NodeError::Conflict`]
    /// when not active or a pair node has not caught up;
    /// [`NodeError::Invalid`] for a node not of the pair.
    pub async fn dr_failback(&self, to: u32, wait: Duration) -> Result<Done, NodeError> {
        use bulkhead_host::failover::FailbackRefused;
        let Some(d) = &self.config.dr else {
            return Err(NodeError::NotFound(
                "this node has no [dr] in its config".into(),
            ));
        };
        if self.rt.dr_active().is_none() {
            return Err(NodeError::Conflict(
                "this DR node is not active: there is nothing to hand back".into(),
            ));
        }
        if !d.pair.contains(&to) {
            return Err(NodeError::Invalid(format!(
                "node {to} is not of the pair {:?}",
                d.pair
            )));
        }
        self.audit.record("dr_failback_asked", json!({ "to": to }));
        {
            // Nothing may write while the pair catches up.
            let _change = self.change.lock().await;
            let order = self.st().desired.order.clone();
            for name in order.iter().rev() {
                if self.running(name) {
                    self.stop_running(name).await?;
                    self.ingress.remove_routes(name);
                }
            }
        }
        match self.rt.dr_failback(to, wait).await {
            Ok(epoch) => {
                self.audit
                    .record("dr_failback", json!({ "to": to, "epoch": epoch }));
                Ok(Done {
                    message: format!(
                        "handed back to node {to} at epoch {epoch}: it resumes as primary when it \
                         next asks the witness, its partner rejoining it as standby; this node \
                         holds the pair's apps again and pulls its copy from the pair. \
                         Deployments made here while active stay here until the pair's replace \
                         them"
                    ),
                })
            }
            Err(e) => {
                self.restore().await;
                Err(match e {
                    FailbackRefused::NotOfPair(..) => NodeError::Invalid(e.to_string()),
                    FailbackRefused::NotKept(why) => NodeError::Internal(why),
                    other => NodeError::Conflict(other.to_string()),
                })
            }
        }
    }

    /// This node's failover standing once its leadership has changed since
    /// `after` (a [`FailoverInfo::generation`]), or after `wait` without a
    /// change: a long poll, for `bulkhead failover --watch`.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`.
    pub async fn failover_after(
        &self,
        after: u64,
        wait: Duration,
    ) -> Result<FailoverInfo, NodeError> {
        self.failover()?;
        let mut generation = self.leadership.subscribe();
        let _ = tokio::time::timeout(wait, generation.wait_for(|g| *g > after)).await;
        self.failover()
    }

    /// The versions this node speaks and keeps, and each connected
    /// member's.
    ///
    /// # Errors
    ///
    /// [`NodeError::Internal`] if the pair's version cannot be read.
    pub async fn release(&self) -> Result<ReleaseInfo, NodeError> {
        let formats = self
            .rt
            .formats_version()
            .await
            .map_err(|e| NodeError::Internal(e.to_string()))?;
        let me = self.rt.node();
        let peers = self
            .rt
            .members()
            .into_iter()
            .filter(|m| m.node != me)
            .filter_map(|m| Some((m.node, self.rt.peer_version(m.node)?)))
            .collect();
        Ok(ReleaseInfo {
            formats,
            min_version: wire::MIN_VERSION,
            max_version: wire::VERSION,
            peers,
        })
    }

    /// Resolves with the pair's version once replication has applied one
    /// newer than this build (`bulkhead_host::Runtime::formats_outrun`): the
    /// node was rolled back while cut off from a release's last step, and
    /// should stop. Its next start is refused.
    pub async fn formats_outrun(&self) -> u16 {
        let mut outrun = self.rt.formats_outrun();
        loop {
            if let Some(version) = *outrun.borrow_and_update() {
                return version;
            }
            if outrun.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// The last step of a release across the pair: raise the pair's
    /// version for what is kept to this build's, once the partner and the
    /// witness are connected at it. Neither node can then be rolled back
    /// to a build before it.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`; [`NodeError::Conflict`]
    /// when it is refused (no replication, a node not at this version, a
    /// record that does not commit).
    pub async fn finalize_release(&self) -> Result<Done, NodeError> {
        let Some(f) = &self.config.failover else {
            return Err(NodeError::NotFound(
                "this node has no [failover] in its config".into(),
            ));
        };
        // The DR node holds a copy too, so it must read what is raised.
        let mut peers: Vec<u32> = std::iter::once(f.partner).chain(f.witness).collect();
        if let Some(dr) = f.dr.filter(|d| !peers.contains(d)) {
            peers.push(dr);
        }
        let done = self
            .rt
            .finalize_formats(&peers)
            .await
            .map_err(|e| NodeError::Conflict(format!("finalize refused: {e}")))?;
        let message = match done {
            Finalized::Raised(version) => {
                self.audit
                    .record("release_finalized", json!({ "formats": version }));
                format!(
                    "the pair now keeps version {version}'s formats; neither node starts a build older than version {version}"
                )
            }
            Finalized::Already(version) => {
                format!("the pair keeps version {version}'s formats already")
            }
            _ => "the pair's version is unchanged".into(),
        };
        Ok(Done { message })
    }

    /// A planned switchover: this primary stops its
    /// singletons gracefully, while its lease holds, and hands over; its
    /// standby takes over within about a lease, with no node timeout, and
    /// this node rejoins as the new primary's standby. Refused, with nothing
    /// stopped, unless this node is a primary with a lease, its arbiter is a
    /// witness and its partner is heard as a confirming standby.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`; [`NodeError::Conflict`]
    /// when the switchover is refused.
    pub async fn switchover(&self) -> Result<Done, NodeError> {
        let (Some(handle), Some(gate)) = (self.failover.get(), &self.gate) else {
            return Err(NodeError::NotFound(
                "this node has no [failover] in its config".into(),
            ));
        };
        let refused =
            |e: SwitchoverRefused| NodeError::Conflict(format!("switchover refused: {e}"));
        handle.check_switchover().await.map_err(refused)?;
        let epoch = handle.leadership().epoch;
        gate.hand_over().await;
        if let Err(e) = handle.switchover().await {
            // Changed between the check and now: run again what was stopped.
            gate.restore(handle.leadership().may_run).await;
            return Err(refused(e));
        }
        self.audit
            .record("failover_switchover", json!({ "epoch": epoch }));
        Ok(Done {
            message: "handing over: the singletons stopped here, and the partner takes over \
                      once the witness grants it (about a lease); this node then rejoins as its \
                      standby"
                .into(),
        })
    }

    /// An operator confirms that the partner is gone, so the standby may
    /// promote itself (`bulkhead_host::failover::FailoverHandle::confirm`).
    /// Taken only while the node awaits a confirmation: with
    /// `arbiter = "operator"`, or a witness that cannot be reached. Safe
    /// only if the partner is down or cut off.
    ///
    /// # Errors
    ///
    /// [`NodeError::NotFound`] without `[failover]`.
    pub fn confirm_failover(&self) -> Result<Done, NodeError> {
        let handle = self.failover.get().ok_or_else(|| {
            NodeError::NotFound("this node has no [failover] in its config".into())
        })?;
        let now = handle.leadership();
        handle.confirm();
        self.audit.record(
            "failover_confirm",
            json!({ "epoch": now.epoch, "awaiting": now.awaiting_confirmation }),
        );
        Ok(Done {
            message: if now.awaiting_confirmation {
                "confirmed: this node is promoted once the partner's lease has run out".into()
            } else {
                "ignored: this node was not waiting for a confirmation (with a witness, one is \
                 needed only while the witness cannot be reached)"
                    .into()
            },
        })
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

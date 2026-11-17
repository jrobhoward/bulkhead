//! Quarantine (ROADMAP PS5): an app whose instances keep misbehaving is
//! stopped and kept stopped, across node restarts, until an operator
//! starts it again.
//!
//! Misbehaving is an exit the node caused for cause: the misbehaviour
//! policy's kills (`crashed: policy(refusals)`, `crashed: policy(rate)`)
//! and protocol breaks (`crashed: protocol: ...`, a frame the node
//! refused). Each is an audit record (`misbehaved`). At node config's
//! `[limits] quarantine_after` of them within `quarantine_period_ms`, the
//! node stops the app and marks it quarantined in its desired state, with
//! an audit record (`quarantine`); `bulkhead start` clears it. Ordinary
//! crashes stay under the supervisor's restart intensity.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bulkhead_host::{ExitReason, InstanceEvent};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use super::Node;

/// Whether an exit reason is misbehaviour.
pub(super) fn misbehaved(reason: &ExitReason) -> Option<&str> {
    match reason {
        ExitReason::Crashed(why) if why.starts_with("policy(") || why.starts_with("protocol:") => {
            Some(why)
        }
        _ => None,
    }
}

/// Recent misbehaviour per app.
#[derive(Debug, Default)]
pub(super) struct Strikes {
    by_app: HashMap<String, VecDeque<Instant>>,
}

impl Strikes {
    /// Count one strike for `app` at `now`; whether that makes `after`
    /// within `period`, which clears its count.
    pub(super) fn strike(&mut self, app: &str, now: Instant, after: u32, period: Duration) -> bool {
        if after == 0 {
            return false;
        }
        let strikes = self.by_app.entry(app.to_string()).or_default();
        while strikes
            .front()
            .is_some_and(|at| now.duration_since(*at) >= period)
        {
            strikes.pop_front();
        }
        strikes.push_back(now);
        if strikes.len() >= after as usize {
            strikes.clear();
            return true;
        }
        false
    }
}

impl Node {
    /// Watch instance exits for misbehaviour and quarantine apps that
    /// reach the limit, until the node stops.
    pub fn start_quarantine(self: &Arc<Self>) {
        let mut exits = self.rt.instance_events();
        let mut stop = self.stop.subscribe();
        let node = Arc::downgrade(self);
        let after = self.config.limits.quarantine_after;
        let period = Duration::from_millis(self.config.limits.quarantine_period_ms);
        tokio::spawn(async move {
            let mut strikes = Strikes::default();
            loop {
                let event = tokio::select! {
                    _ = stop.wait_for(|s| *s) => return,
                    got = exits.recv() => got,
                };
                let (pid, app, why) = match &event {
                    Ok(InstanceEvent::Exited { pid, app, reason }) => match misbehaved(reason) {
                        Some(why) => (*pid, app.clone(), why.to_string()),
                        None => continue,
                    },
                    Ok(_) => continue,
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "quarantine fell behind instance exits");
                        continue;
                    }
                    Err(RecvError::Closed) => return,
                };
                let Some(node) = node.upgrade() else { return };
                node.audit.record(
                    "misbehaved",
                    json!({ "app": app, "pid": pid.to_string(), "reason": why }),
                );
                if strikes.strike(&app, Instant::now(), after, period) {
                    node.quarantine(&app, &why, after, period).await;
                }
            }
        });
    }

    /// Stop `app` and mark it quarantined: it stays stopped, across node
    /// restarts, until `bulkhead start`.
    async fn quarantine(&self, app: &str, why: &str, after: u32, period: Duration) {
        let _change = self.change.lock().await;
        let running = self.st().desired.apps.get(app).is_some_and(|a| a.running);
        if !running {
            return;
        }
        let reason = format!("{why}; {after} kills within {} s", period.as_secs());
        if let Err(e) = self.stop_running(app).await {
            tracing::error!(app, error = %e, "cannot stop the app to quarantine it");
            self.audit.record(
                "quarantine_refused",
                json!({ "app": app, "reason": reason, "error": e.to_string() }),
            );
            return;
        }
        self.ingress.remove_routes(app);
        {
            let mut st = self.st();
            if let Some(entry) = st.desired.apps.get_mut(app) {
                entry.running = false;
                entry.quarantined = Some(reason.clone());
            }
        }
        if let Err(e) = self.save_desired() {
            tracing::error!(app, error = %e, "cannot save the quarantine");
        }
        tracing::warn!(
            app,
            reason,
            "app quarantined; `bulkhead start` runs it again"
        );
        self.audit
            .record("quarantine", json!({ "app": app, "reason": reason }));
    }
}

#[cfg(test)]
#[path = "quarantine_tests.rs"]
mod tests;

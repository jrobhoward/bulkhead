//! A disaster-recovery node apart from the witness (ROADMAP DR4b): it
//! follows what the witness has kept.
//!
//! The witness is the one authority: its kept epoch and promoted node
//! decide who runs, as for the pair. This node only asks. Every poll it
//! sends the witness its intent ([`DrIntent`]: only the state, a fence for
//! an activation, or a hand-back), and the witness answers with its state
//! after acting on it ([`FailoverMsg::DrState`]). The copy leads once an
//! answer says the witness promoted this node, and stops leading once one
//! says it promoted another. So a grant whose answer was lost, or a
//! restart, recovers by itself at the next answer, and nothing leads here
//! without the witness having kept a grant naming this node. While it
//! hands back the copy is frozen, and stays so until the witness says it
//! promoted the pair node: never on a timeout. `docs/tla/DrFailover.tla`
//! with `DrApart` is this protocol (`RecvFromD`, `RecvAtD`).

use std::sync::{Arc, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use super::{ActivateRefused, DrIntent, FailbackRefused, FailoverMsg};
use crate::runtime::RuntimeInner;
use crate::transport::{self, Frame};

/// How long an activation or a hand-back waits for the witness's word
/// before the operator is answered; this node goes on following it.
const CONFIRM_WAIT: Duration = Duration::from_secs(25);

/// What the witness last said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Heard {
    epoch: u64,
    promoted: u32,
    fencing: bool,
    abandoned: Option<u32>,
}

/// Where this node stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct View {
    intent: DrIntent,
    /// The epoch the witness promoted this node at, once its copy leads.
    leading: Option<u64>,
    heard: Option<Heard>,
    /// Answers heard so far, so a wait counts only answers after it began.
    answers: u64,
}

/// A DR node's following of the witness `witness`.
#[derive(Debug)]
pub(crate) struct Follow {
    pub(crate) witness: u32,
    view: watch::Sender<View>,
    active: watch::Sender<Option<u64>>,
    tx: mpsc::UnboundedSender<Heard>,
}

impl Follow {
    /// An answer from the witness.
    pub(super) fn heard(&self, msg: &FailoverMsg) {
        if let FailoverMsg::DrState {
            epoch,
            promoted,
            fencing,
            abandoned,
        } = *msg
        {
            let _ = self.tx.send(Heard {
                epoch,
                promoted,
                fencing,
                abandoned,
            });
        }
    }

    /// The epoch the witness promoted this node at, while its copy leads.
    pub(crate) fn leading(&self) -> Option<u64> {
        self.view.borrow().leading
    }

    /// The same, as it changes.
    pub(crate) fn watch_active(&self) -> watch::Receiver<Option<u64>> {
        self.active.subscribe()
    }

    fn set_intent(&self, intent: DrIntent) {
        self.view.send_modify(|v| v.intent = intent);
    }
}

/// Start following `witness`, asking every `poll`.
pub(crate) fn start(rt: &Arc<RuntimeInner>, witness: u32, poll: Duration) {
    let (tx, rx) = mpsc::unbounded_channel();
    let follow = Arc::new(Follow {
        witness,
        view: watch::Sender::new(View::default()),
        active: watch::Sender::new(None),
        tx,
    });
    *rt.failover
        .follow
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(follow.clone());
    tokio::spawn(run(Arc::downgrade(rt), follow, rx, poll));
}

async fn run(
    rt: Weak<RuntimeInner>,
    follow: Arc<Follow>,
    mut rx: mpsc::UnboundedReceiver<Heard>,
    poll: Duration,
) {
    let mut ticker = tokio::time::interval(poll);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let heard = tokio::select! {
            h = rx.recv() => match h {
                Some(h) => Some(h),
                None => return,
            },
            _ = ticker.tick() => None,
        };
        let Some(rt) = rt.upgrade() else { return };
        match heard {
            Some(h) => reconcile(&rt, &follow, h).await,
            None => ask(&rt, &follow),
        }
    }
}

fn ask(rt: &RuntimeInner, follow: &Follow) {
    let intent = follow.view.borrow().intent;
    // Lost if the witness is out of reach now; asked again next poll.
    transport::send(
        rt,
        follow.witness,
        Frame::Failover(FailoverMsg::DrAsk { intent }),
    );
}

/// Follow the witness's answer `h`.
async fn reconcile(rt: &RuntimeInner, follow: &Follow, h: Heard) {
    let me = rt.node();
    let view = *follow.view.borrow();
    let handing_back = matches!(view.intent, DrIntent::HandBack { .. });
    let mut leading = view.leading;
    let mut intent = view.intent;
    if h.promoted == me {
        // Promoted: lead, unless leading already, or frozen handing back
        // and not yet handed back.
        if leading.is_none() && !handing_back {
            match crate::replication::dr_lead(rt, h.epoch).await {
                Ok(()) => {
                    tracing::warn!(
                        epoch = h.epoch,
                        witness = follow.witness,
                        "DR activation: the witness promoted this node; it leads at site 2"
                    );
                    leading = Some(h.epoch);
                    intent = DrIntent::State;
                }
                Err(e) => tracing::error!(error = %e, "DR activation: the copy does not lead"),
            }
        }
    } else {
        if leading.is_some() {
            // The witness promoted another: handed back, whether or not
            // this node heard the answer to its hand-back.
            crate::replication::dr_release(rt, true);
            tracing::warn!(
                to = h.promoted,
                epoch = h.epoch,
                "DR failback: the witness promoted node {}; this node pulls from the pair again",
                h.promoted
            );
            leading = None;
        }
        let fencing_asked = matches!(intent, DrIntent::Fence { .. });
        if handing_back || (fencing_asked && !h.fencing && h.abandoned.is_some()) {
            intent = DrIntent::State;
        }
    }
    follow.view.send_modify(|v| {
        v.leading = leading;
        v.intent = intent;
        v.heard = Some(h);
        v.answers += 1;
    });
    follow.active.send_if_modified(|a| {
        let changed = *a != leading;
        *a = leading;
        changed
    });
}

/// Activate: ask the witness to fence and promote this node, and wait for
/// its word. The new epoch.
pub(crate) async fn activate(rt: &RuntimeInner, follow: &Follow) -> Result<u64, ActivateRefused> {
    if let Some(epoch) = follow.leading() {
        return Err(ActivateRefused::Active(epoch));
    }
    let mut rx = follow.view.subscribe();
    let since = rx.borrow_and_update().answers;
    let attempt = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    follow.set_intent(DrIntent::Fence { attempt });
    ask(rt, follow);
    let deadline = Instant::now() + CONFIRM_WAIT;
    let mut fenced = false;
    loop {
        let view = *rx.borrow_and_update();
        if let Some(epoch) = view.leading {
            return Ok(epoch);
        }
        if let Some(h) = view.heard.filter(|_| view.answers > since) {
            fenced |= h.fencing;
            if fenced
                && !h.fencing
                && h.promoted != rt.node()
                && let Some(node) = h.abandoned
            {
                return Err(ActivateRefused::Heard(node));
            }
        }
        if tokio::time::timeout_at(deadline, rx.changed())
            .await
            .map_or(true, |r| r.is_err())
        {
            // Stop asking for a fence; a grant already decided still
            // reaches this node at its next answer.
            follow.view.send_modify(|v| {
                if matches!(v.intent, DrIntent::Fence { .. }) {
                    v.intent = DrIntent::State;
                }
            });
            return Err(ActivateRefused::Unconfirmed(
                follow.witness,
                CONFIRM_WAIT.as_secs(),
            ));
        }
    }
}

/// Hand back to `to`, the copy frozen and both pair nodes caught up: ask
/// the witness until it says it promoted `to`. On no word in time the copy
/// stays frozen, and this node goes on asking. The new epoch.
pub(crate) async fn hand_back(
    rt: &RuntimeInner,
    follow: &Follow,
    to: u32,
) -> Result<u64, FailbackRefused> {
    let mut rx = follow.view.subscribe();
    rx.borrow_and_update();
    follow.set_intent(DrIntent::HandBack { to });
    ask(rt, follow);
    let deadline = Instant::now() + CONFIRM_WAIT;
    loop {
        let view = *rx.borrow_and_update();
        if view.leading.is_none() {
            return Ok(view.heard.map_or(0, |h| h.epoch));
        }
        if tokio::time::timeout_at(deadline, rx.changed())
            .await
            .map_or(true, |r| r.is_err())
        {
            return Err(FailbackRefused::Unconfirmed(
                follow.witness,
                CONFIRM_WAIT.as_secs(),
            ));
        }
    }
}

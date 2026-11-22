//! Trace context across calls (ROADMAP O1): what a traced callback sends
//! joins its trace by itself. The node holds the trace of the message an
//! instance's callback handles (`InstanceState::trace`), set when it hands
//! the message over and cleared at the callback's end; every message the
//! callback sends then gets a child span of it, unless the app set a trace
//! of its own. Replies need none: they go back to the caller, whose own
//! trace stands. An app's other threads and drivers set traces themselves.

use bulkhead_native::protocol::{Envelope, GroupsRequest, Request, Trace};

/// Give `msg` a child span of `current`, unless it carries a trace already.
pub(crate) fn stamp(msg: &mut Envelope, current: Option<Trace>) {
    if msg.trace.is_none()
        && let Some(trace) = current
    {
        msg.trace = Some(Box::new(trace.child()));
    }
}

/// [`stamp`] the message `request` sends, if it sends one: a call, a cast
/// (by pid or app name, waiting or not), a group publish.
pub(crate) fn stamp_request(request: &mut Request, current: Option<Trace>) {
    if current.is_none() {
        return;
    }
    match request {
        Request::Cast { msg, .. }
        | Request::CastApp { msg, .. }
        | Request::CastWait { msg, .. }
        | Request::Call { msg, .. }
        | Request::CallApp { msg, .. }
        | Request::Groups(GroupsRequest::Publish { msg, .. }) => stamp(msg, current),
        _ => {}
    }
}

#[cfg(test)]
#[path = "trace_tests.rs"]
mod trace_tests;

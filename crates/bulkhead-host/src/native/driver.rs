//! The node's own drivers' requests (ROADMAP DA7): a stream into the
//! admin API, and the metrics as text, each for a driver whose profile
//! allows it ([`crate::DriverProfile`]'s `admin`, `metrics`), from a
//! callback or a sender channel alike.

use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;

use bulkhead_native::protocol::{Answer, FdRef, NetError, Request};

use crate::runtime::RuntimeInner;

/// What a driver may ask for here.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Allowed {
    pub(crate) admin: bool,
    pub(crate) metrics: bool,
}

/// Answer `request`, one of [`Request::AdminStream`] and
/// [`Request::MetricsText`], passing an admin stream with `pass`. Refused
/// (`denied`) without the profile's flag.
pub(crate) fn answer(
    rt: &Arc<RuntimeInner>,
    allowed: Allowed,
    request: &Request,
    pass: impl FnOnce(BorrowedFd<'_>) -> Result<FdRef, String>,
) -> Answer {
    match request {
        Request::AdminStream => Answer::AdminStream(if allowed.admin {
            rt.admin_stream()
                .and_then(|stream| pass(stream.as_fd()))
                .map_err(NetError::Other)
        } else {
            Err(NetError::Denied)
        }),
        Request::MetricsText => Answer::MetricsText(if allowed.metrics {
            Ok(rt.metrics_text())
        } else {
            Err(NetError::Denied)
        }),
        other => Answer::MetricsText(Err(NetError::Other(format!(
            "not a driver's request: {other:?}"
        )))),
    }
}

//! The node's own drivers' requests: a stream into the admin API and the
//! metrics as text (ROADMAP DA7), and a connection to the driver's one
//! endpoint (DA3), each for a driver whose profile allows it
//! ([`crate::DriverProfile`]'s `admin`, `metrics`, `connect`), from a
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

/// A connection to `endpoint`, which the node opens for the driver: a
/// host name resolved through the runtime's resolver each time, the
/// addresses tried in turn within the timeout. The stream is passed in
/// blocking mode, as a new socket is; the node keeps no copy.
///
/// # Errors
///
/// `denied` without an endpoint; otherwise why it could not connect.
pub(crate) async fn connect(
    rt: &Arc<RuntimeInner>,
    endpoint: Option<&crate::config::DriverEndpoint>,
) -> Result<std::os::fd::OwnedFd, NetError> {
    use crate::config::DriverEndpoint;
    match endpoint {
        None => Err(NetError::Denied),
        Some(DriverEndpoint::Tcp {
            host,
            port,
            timeout,
        }) => {
            let deadline = tokio::time::Instant::now() + *timeout;
            let addrs = rt
                .services
                .names()
                .resolve(host, *port, *timeout)
                .await
                .map_err(|e| NetError::Other(e.to_string()))?;
            let mut last = NetError::Other(format!("{host} has no address"));
            for addr in addrs {
                match tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect(addr)).await
                {
                    Ok(Ok(stream)) => {
                        let _ = stream.set_nodelay(true);
                        let stream = stream
                            .into_std()
                            .and_then(|s| s.set_nonblocking(false).map(|()| s))
                            .map_err(|e| NetError::Other(e.to_string()))?;
                        return Ok(stream.into());
                    }
                    Ok(Err(e)) => last = NetError::Other(format!("{addr}: {e}")),
                    Err(_) => return Err(NetError::Other(format!("{host}: connect timed out"))),
                }
            }
            Err(last)
        }
        Some(DriverEndpoint::Unix(path)) => tokio::net::UnixStream::connect(path)
            .await
            .and_then(tokio::net::UnixStream::into_std)
            .and_then(|s| s.set_nonblocking(false).map(|()| s))
            .map(Into::into)
            .map_err(|e| NetError::Other(format!("{}: {e}", path.display()))),
    }
}

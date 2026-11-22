//! The node's own drivers' requests: a stream into the admin API and the
//! metrics as text (ROADMAP DA7), a connection to the driver's one
//! endpoint (DA3) and connections where an app's HTTP requests may go
//! (DA4), each for a driver whose profile allows it
//! ([`crate::DriverProfile`]'s `admin`, `metrics`, `connect`, `egress`),
//! from a callback or a sender channel alike.

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
            first_to_accept(host, addrs, deadline).await
        }
        Some(DriverEndpoint::Unix(path)) => tokio::net::UnixStream::connect(path)
            .await
            .and_then(tokio::net::UnixStream::into_std)
            .and_then(|s| s.set_nonblocking(false).map(|()| s))
            .map(Into::into)
            .map_err(|e| NetError::Other(format!("{}: {e}", path.display()))),
    }
}

/// A stream to the first of `addrs` that accepts before `deadline`, in
/// blocking mode with `TCP_NODELAY`, as a new socket is passed.
async fn first_to_accept(
    host: &str,
    addrs: Vec<std::net::SocketAddr>,
    deadline: tokio::time::Instant,
) -> Result<std::os::fd::OwnedFd, NetError> {
    let mut last = NetError::Other(format!("{host} has no address"));
    for addr in addrs {
        match tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect(addr)).await {
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

/// A connection to `host` and `port` for `served`, the app an egress
/// driver sends HTTP requests for (ROADMAP DA4): only where one of that
/// app's running instances may send a request
/// (`services::egress::Shared::reach`), a name resolved through the
/// runtime's resolver each time and its loopback and link-local addresses
/// dropped unless node config allows them, within egress's connect
/// timeout. The node keeps no copy.
///
/// # Errors
///
/// `denied` for a driver that serves no app, or a destination its app may
/// not reach, or a name resolving only to refused addresses; otherwise why
/// it could not connect.
pub(crate) async fn connect_to(
    rt: &Arc<RuntimeInner>,
    served: Option<&str>,
    endpoints: &[crate::config::DriverEndpoint],
    host: &str,
    port: u16,
) -> Result<std::os::fd::OwnedFd, NetError> {
    // A driver given endpoints (the Modbus devices, ROADMAP DA5) reaches
    // those alone.
    let listed = endpoints.iter().find(|e| {
        matches!(e, crate::config::DriverEndpoint::Tcp { host: h, port: p, .. } if h == host && *p == port)
    });
    if let Some(endpoint) = listed {
        return connect(rt, Some(endpoint)).await;
    }
    #[cfg(feature = "egress")]
    {
        use crate::services::egress::Reach;
        let Some(app) = served else {
            return Err(NetError::Denied);
        };
        let egress = rt.services.egress();
        let Some(reach) = egress.reach(app, host, port) else {
            tracing::debug!(%app, %host, port, "egress driver's connect denied");
            return Err(NetError::Denied);
        };
        let timeout = egress.config().connect_timeout;
        let deadline = tokio::time::Instant::now() + timeout;
        // Through a proxy (ROADMAP DA4b) the node tunnels to the allowed
        // destination itself; the proxy resolves its name.
        if let Some((proxy_host, proxy_port)) = egress.proxy() {
            let addrs = rt
                .services
                .names()
                .resolve(proxy_host, *proxy_port, timeout)
                .await
                .map_err(|e| NetError::Other(format!("the egress proxy: {e}")))?;
            let stream = first_to_accept(proxy_host, addrs, deadline).await?;
            return super::proxy::tunnel(stream, host, port, deadline).await;
        }
        let addrs = match reach {
            Reach::Address(ip) => vec![std::net::SocketAddr::new(ip, port)],
            Reach::Name => {
                let addrs: Vec<_> = rt
                    .services
                    .names()
                    .resolve(host, port, timeout)
                    .await
                    .map_err(|e| NetError::Other(e.to_string()))?
                    .into_iter()
                    .filter(|a| !egress.refused(a.ip()))
                    .collect();
                if addrs.is_empty() {
                    return Err(NetError::Denied);
                }
                addrs
            }
        };
        first_to_accept(host, addrs, deadline).await
    }
    #[cfg(not(feature = "egress"))]
    {
        let _ = (rt, served, host, port, endpoints);
        Err(NetError::Denied)
    }
}

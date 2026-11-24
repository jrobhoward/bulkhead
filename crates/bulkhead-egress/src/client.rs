//! The pool every request of the app goes through: hyper-util's client,
//! HTTP/1.1, TLS through rustls on ring against the roots the node gave,
//! over connections the node opens (`Request::ConnectTo`), asked for on
//! the worker's sender channel by a [`Connector`].
//!
//! The driver opens no socket itself. A destination the node refuses
//! (a name resolving only to loopback or link-local addresses) is the
//! app's `prohibited`, as before ROADMAP DA4.

use std::future::Future;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bulkhead_native::protocol::{self as p, HttpError, HttpRequest, HttpResponse};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::HeaderName;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Sleep;

use crate::config::Config;

/// Why a request failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No response, or no body chunk, in time.
    #[error("no response or body chunk in time")]
    ConnectionReadTimeout,
    /// The connection did not open in time.
    #[error("the connection timed out")]
    ConnectionTimeout,
    /// The server refused the connection.
    #[error("the connection was refused")]
    ConnectionRefused,
    /// The connection ended early.
    #[error("the connection ended")]
    ConnectionTerminated,
    /// The node refused the destination: its name resolves only to
    /// loopback or link-local addresses.
    #[error("the name resolves only to loopback or link-local addresses")]
    DestinationIpProhibited,
    /// The destination could not be reached.
    #[error("the destination could not be reached: {0}")]
    DestinationUnavailable(String),
    /// The server's certificate was not accepted.
    #[error("the certificate was not accepted")]
    TlsCertificate,
    /// The server sent a TLS alert.
    #[error("TLS alert: {0}")]
    TlsAlertReceived(String),
    /// TLS failed otherwise.
    #[error("TLS failed")]
    TlsProtocol,
    /// The server broke HTTP.
    #[error("HTTP protocol error")]
    HttpProtocol,
    /// The response body is over the cap.
    #[error("the response body is too large")]
    HttpResponseBodySize,
}

impl From<hyper::Error> for Error {
    fn from(_: hyper::Error) -> Self {
        Self::HttpProtocol
    }
}

/// A request or response body.
pub type HttpBody = http_body_util::combinators::UnsyncBoxBody<Bytes, Error>;

/// A connection the worker asks the node for: the host and port, and
/// where the stream, or why there is none, goes.
pub type Wanted = (String, u16, oneshot::Sender<Result<OwnedFd, p::NetError>>);

/// The node refused the destination.
#[derive(Debug, thiserror::Error)]
#[error("the node refused the destination")]
struct Refused;

/// hyper's connector: each new connection a stream the node opened, asked
/// for through the worker.
#[derive(Debug, Clone)]
pub struct Connector {
    wanted: mpsc::UnboundedSender<Wanted>,
    timeout: Duration,
}

impl Connector {
    /// A connector sending its requests to `wanted`, each answered within
    /// `timeout`.
    #[must_use]
    pub fn new(wanted: mpsc::UnboundedSender<Wanted>, timeout: Duration) -> Self {
        Self { wanted, timeout }
    }
}

impl tower_service::Service<hyper::Uri> for Connector {
    type Response = TokioIo<tokio::net::TcpStream>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: hyper::Uri) -> Self::Future {
        let (wanted, timeout) = (self.wanted.clone(), self.timeout);
        Box::pin(async move {
            let host = uri
                .host()
                .map(|h| h.trim_start_matches('[').trim_end_matches(']').to_string())
                .ok_or_else(|| std::io::Error::other("no host"))?;
            let port = uri
                .port_u16()
                .unwrap_or(if uri.scheme_str() == Some("https") {
                    443
                } else {
                    80
                });
            let (tx, rx) = oneshot::channel();
            wanted
                .send((host, port, tx))
                .map_err(|_| std::io::Error::other("the driver's sender stopped"))?;
            let got = tokio::time::timeout(timeout, rx)
                .await
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
                .map_err(|_| std::io::Error::other("the driver's sender stopped"))?;
            let fd = got.map_err(|e| match e {
                p::NetError::Denied => std::io::Error::other(Refused),
                p::NetError::Other(why) if why.ends_with("timed out") => {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, why)
                }
                p::NetError::Other(why) => std::io::Error::other(why),
                other => std::io::Error::other(format!("{other:?}")),
            })?;
            let stream = std::net::TcpStream::from(fd);
            stream.set_nonblocking(true)?;
            Ok(TokioIo::new(tokio::net::TcpStream::from_std(stream)?))
        })
    }
}

/// The pool, and what bounds each response.
pub struct Pool {
    client: Client<HttpsConnector<Connector>, HttpBody>,
    max_body: u64,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("max_body", &self.max_body)
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// A pool as `config` describes it, connecting through `connector`,
    /// presenting `identity` to a server that asks for a client certificate
    /// (ROADMAP DA4b).
    ///
    /// # Errors
    ///
    /// The roots are not certificates, the identity's key does not fit, or
    /// TLS cannot be set up.
    pub fn new(
        config: &Config,
        connector: Connector,
        identity: Option<crate::config::Identity>,
    ) -> Result<Self, String> {
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_root_certificates(config.roots()?);
        let tls = match identity {
            Some((chain, key)) => builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| format!("client certificate: {e}"))?,
            None => builder.with_no_client_auth(),
        };
        let https = HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(connector);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(config.pool_idle_timeout())
            .pool_max_idle_per_host(config.pool_max_idle_per_host)
            .pool_timer(TokioTimer::new())
            .timer(TokioTimer::new())
            .build(https);
        Ok(Self {
            client,
            max_body: config.max_response_body,
        })
    }

    /// Send `request` and read its whole response: its headers within
    /// `first_byte`, each body chunk within `between` of the last, the
    /// body up to the pool's cap. Hop-by-hop headers and `host` are
    /// neither sent nor answered.
    ///
    /// # Errors
    ///
    /// The request is not valid HTTP ([`HttpError::Invalid`]), or why it
    /// failed.
    pub async fn send(
        &self,
        request: HttpRequest,
        first_byte: Duration,
        between: Duration,
    ) -> Result<HttpResponse, HttpError> {
        let request = build(request)?;
        let read = async {
            let response = tokio::time::timeout(first_byte, self.client.request(request))
                .await
                .map_err(|_| Error::ConnectionReadTimeout)?
                .map_err(|e| classify(&e))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter(|(n, _)| !forbidden(n))
                .map(|(n, v)| (n.as_str().to_string(), p::Bytes(v.as_bytes().to_vec())))
                .collect();
            // One buffer for the declared length (within `max_body`), so a
            // large body is copied once (ROADMAP PF1).
            let declared = response
                .headers()
                .get(hyper::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
                .map_or(0, |n| n.min(self.max_body));
            let mut body = Timed {
                inner: response.into_body(),
                between,
                sleep: Box::pin(tokio::time::sleep(between)),
            };
            let mut bytes = Vec::with_capacity(usize::try_from(declared).unwrap_or(0));
            while let Some(frame) = body.frame().await {
                if let Ok(data) = frame?.into_data() {
                    if (bytes.len() + data.len()) as u64 > self.max_body {
                        return Err(Error::HttpResponseBodySize);
                    }
                    bytes.extend_from_slice(&data);
                }
            }
            Ok(HttpResponse {
                status,
                headers,
                body: p::Bytes(bytes),
            })
        };
        read.await.map_err(|e| self.app_error(&e))
    }

    /// `e` as the app's error.
    fn app_error(&self, e: &Error) -> HttpError {
        match e {
            Error::DestinationIpProhibited => HttpError::Prohibited,
            Error::ConnectionReadTimeout | Error::ConnectionTimeout => HttpError::Timeout,
            Error::ConnectionRefused
            | Error::DestinationUnavailable(_)
            | Error::ConnectionTerminated => HttpError::Unreachable(e.to_string()),
            Error::TlsProtocol | Error::TlsCertificate | Error::TlsAlertReceived(_) => {
                HttpError::Tls(format!("{e:?}"))
            }
            Error::HttpResponseBodySize => {
                HttpError::Other(format!("response body over {} bytes", self.max_body))
            }
            Error::HttpProtocol => HttpError::Other(format!("{e:?}")),
        }
    }
}

/// The app's request as hyper's, refusing forbidden headers.
fn build(request: HttpRequest) -> Result<hyper::Request<HttpBody>, HttpError> {
    let invalid =
        |what: &str, e: &dyn std::fmt::Display| HttpError::Invalid(format!("{what}: {e}"));
    let method =
        hyper::Method::from_bytes(request.method.as_bytes()).map_err(|e| invalid("method", &e))?;
    let uri: hyper::Uri = request.url.parse().map_err(|e| invalid("url", &e))?;
    let mut builder = hyper::Request::builder().method(method).uri(uri);
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| invalid("header", &e))?;
        if forbidden(&name) {
            return Err(HttpError::Invalid(format!("header {name} is forbidden")));
        }
        let value =
            hyper::header::HeaderValue::from_bytes(&value.0).map_err(|e| invalid("header", &e))?;
        builder = builder.header(name, value);
    }
    let body: HttpBody = http_body_util::Full::new(Bytes::from(request.body.0))
        .map_err(|never| match never {})
        .boxed_unsync();
    builder.body(body).map_err(|e| invalid("request", &e))
}

/// Whether `name` is a header an app may not set or see (hop-by-hop
/// headers and `host`).
#[must_use]
pub fn forbidden(name: &HeaderName) -> bool {
    use hyper::header;
    matches!(
        *name,
        header::CONNECTION
            | header::PROXY_AUTHENTICATE
            | header::PROXY_AUTHORIZATION
            | header::TRANSFER_ENCODING
            | header::UPGRADE
            | header::HOST
    ) || matches!(
        name.as_str(),
        "keep-alive" | "proxy-connection" | "http2-settings"
    )
}

/// The error for a client error, from what caused it.
fn classify(e: &hyper_util::client::legacy::Error) -> Error {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cause {
        // `io::Error::source` skips the error it wraps (often another
        // `io::Error`), so look inside each.
        let mut wrapped = vec![c];
        while let Some(inner) = wrapped
            .last()
            .and_then(|w| w.downcast_ref::<std::io::Error>())
            .and_then(|io| io.get_ref())
        {
            wrapped.push(inner);
        }
        for err in wrapped {
            if err.downcast_ref::<Refused>().is_some() {
                return Error::DestinationIpProhibited;
            }
            if let Some(tls) = err.downcast_ref::<rustls::Error>() {
                return match tls {
                    rustls::Error::InvalidCertificate(_) => Error::TlsCertificate,
                    rustls::Error::AlertReceived(_) => Error::TlsAlertReceived(tls.to_string()),
                    _ => Error::TlsProtocol,
                };
            }
            if let Some(io) = err.downcast_ref::<std::io::Error>() {
                match io.kind() {
                    std::io::ErrorKind::ConnectionRefused => return Error::ConnectionRefused,
                    std::io::ErrorKind::TimedOut => return Error::ConnectionTimeout,
                    std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof => return Error::ConnectionTerminated,
                    _ => {}
                }
            }
        }
        cause = c.source();
    }
    if e.is_connect() {
        Error::DestinationUnavailable(deepest(e))
    } else {
        Error::HttpProtocol
    }
}

/// The innermost cause's message.
fn deepest(e: &(dyn std::error::Error + 'static)) -> String {
    let mut last = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        last = c.to_string();
        cause = c.source();
    }
    last
}

/// A response body that fails when a chunk is late.
struct Timed {
    inner: Incoming,
    between: Duration,
    sleep: Pin<Box<Sleep>>,
}

impl Body for Timed {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                let next = tokio::time::Instant::now() + this.between;
                this.sleep.as_mut().reset(next);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Error::from(e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Some(Err(Error::ConnectionReadTimeout))),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

//! One request: matched to a route, its body read within the listener's
//! limits, sent as a call to the app that owns the route, and the app's
//! reply turned into the response. The node's ingress did this until
//! ROADMAP DA2; the statuses and limits are its.
//!
//! Failures map to statuses: no route 404, a body over the route's
//! `max_body` (or the app's `message_max_bytes`) 413, a body not received
//! within the listener's `body_timeout` 408, an unreadable body 400, the
//! app not running or overloaded 503 (overloaded with `Retry-After`), no
//! reply within the route's timeout 504, refused by the app's messaging
//! rules 403, the instance exiting before replying, a malformed reply or
//! one over `max_response` 502.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use bulkhead_native::protocol::{CallError, Envelope};
use bulkhead_native::sender_async::AsyncSender;
use bulkhead_sdk::http::{ClientCert, Request, Response, Routes};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{
    CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName, HeaderValue, LOCATION, RETRY_AFTER,
};
use hyper::{StatusCode, Uri};
use rustls_pki_types::CertificateDer;

use crate::config::{Listener, Standby};
use crate::tls::IngressTls;

/// A response.
pub type Reply = hyper::Response<Full<Bytes>>;

/// What every worker thread shares: the route table and the counters.
#[derive(Debug, Default)]
pub struct Shared {
    /// The node's routes, from the host event `routes`.
    pub routes: RwLock<Routes>,
    /// What the driver has served.
    pub counters: Counters,
}

/// What the driver has served, added to the app's metrics now and then.
#[derive(Debug, Default)]
pub struct Counters {
    /// Requests answered.
    pub requests: AtomicU64,
    /// Of those, answered with a 5xx status.
    pub server_errors: AtomicU64,
    /// Of those, matched no route.
    pub not_found: AtomicU64,
    /// Of those, redirected to the partner.
    pub redirected: AtomicU64,
    /// Of those, forwarded to the partner.
    pub forwarded: AtomicU64,
    /// Connections closed on accept, over `max_connections`.
    pub refused: AtomicU64,
}

/// A listener as a worker serves it.
#[derive(Debug)]
pub struct Listening {
    /// Its configuration.
    pub config: Listener,
    /// Its certificates, for HTTPS.
    pub tls: Option<IngressTls>,
}

impl Listening {
    /// `https` with certificates, `http` without.
    #[must_use]
    pub fn scheme(&self) -> &'static str {
        if self.tls.is_some() { "https" } else { "http" }
    }
}

/// The client certificate a handshake verified, from its peer
/// certificates (leaf first), for the app (ROADMAP DA2b).
#[must_use]
pub fn client_cert(peer: Option<&[CertificateDer<'_>]>) -> Option<Arc<ClientCert>> {
    let leaf = peer?.first()?;
    let digest = ring::digest::digest(&ring::digest::SHA256, leaf.as_ref());
    let sha256 = digest
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        });
    Some(Arc::new(ClientCert {
        sha256,
        der: leaf.to_vec(),
    }))
}

/// Answer `req`, counting it; `client` is the connection's verified client
/// certificate, if any.
pub async fn serve(
    req: hyper::Request<Incoming>,
    listening: &Listening,
    shared: &Shared,
    sender: &AsyncSender,
    client: Option<&ClientCert>,
) -> Reply {
    let reply = handle(req, listening, shared, sender, client).await;
    let c = &shared.counters;
    c.requests.fetch_add(1, Ordering::Relaxed);
    let code = reply.status();
    if code.is_server_error() {
        c.server_errors.fetch_add(1, Ordering::Relaxed);
    }
    if code == StatusCode::NOT_FOUND {
        c.not_found.fetch_add(1, Ordering::Relaxed);
    }
    if code == StatusCode::TEMPORARY_REDIRECT {
        c.redirected.fetch_add(1, Ordering::Relaxed);
    }
    reply
}

async fn handle(
    req: hyper::Request<Incoming>,
    listening: &Listening,
    shared: &Shared,
    sender: &AsyncSender,
    client: Option<&ClientCert>,
) -> Reply {
    let host = host_of(req.headers().get(HOST), req.uri());
    let path = req.uri().path().to_string();
    let picked = shared
        .routes
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pick(host.as_deref(), &path)
        .cloned();
    let Some(route) = picked else {
        return status(StatusCode::NOT_FOUND, "no route");
    };
    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > route.max_body) {
        return status(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
    }
    let (parts, body) = req.into_parts();
    let body = match read_body(body, route.max_body, listening).await {
        Ok(b) => b,
        Err(reply) => return *reply,
    };
    let key = route
        .hash_header
        .as_deref()
        .and_then(|h| parts.headers.get(h))
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    let request = Request {
        scheme: listening.scheme().to_string(),
        method: parts.method.as_str().to_string(),
        path,
        query: parts.uri.query().map(str::to_string),
        headers: parts
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect(),
        body: body.to_vec(),
        key,
        client_cert: client.cloned(),
    };
    let timeout_ms = u32::try_from(route.timeout_ms).unwrap_or(u32::MAX);
    let msg: Envelope = request.to_envelope().into();
    // Kept for the partner only where a listener forwards.
    let again = matches!(listening.config.standby, Standby::Forward).then(|| msg.clone());
    let mut result = sender.call_app(route.app.as_str(), msg, timeout_ms).await;
    if matches!(result, Err(CallError::Noproc)) {
        // Not running here: where it runs now, asked of the node at once,
        // as the node's ingress asked (ROADMAP DA2).
        match (&listening.config.standby, again) {
            (Standby::Redirect { partner_url }, _) => {
                if sender.elsewhere(route.app.as_str()).await.is_some() {
                    return redirect(partner_url, &parts.uri);
                }
            }
            (Standby::Forward, Some(msg)) => {
                result = sender
                    .call_app_partner(route.app.as_str(), msg, timeout_ms)
                    .await;
                if !matches!(result, Err(CallError::Noproc)) {
                    shared.counters.forwarded.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }
    from_call(result, listening.config.max_response)
}

/// The body, whole, within `max_body` and the listener's body timeout; a
/// request with no body arms no timer.
async fn read_body(
    body: Incoming,
    max_body: u64,
    listening: &Listening,
) -> Result<Bytes, Box<Reply>> {
    if hyper::body::Body::is_end_stream(&body) {
        return Ok(Bytes::new());
    }
    let limit = usize::try_from(max_body).unwrap_or(usize::MAX);
    let read = async {
        Limited::new(body, limit)
            .collect()
            .await
            .map(http_body_util::Collected::to_bytes)
    };
    match tokio::time::timeout(listening.config.body_timeout(), read).await {
        Err(_) => Err(Box::new(status(
            StatusCode::REQUEST_TIMEOUT,
            "body not received in time",
        ))),
        Ok(Ok(b)) => Ok(b),
        Ok(Err(e))
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some() =>
        {
            Err(Box::new(status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body too large",
            )))
        }
        Ok(Err(_)) => Err(Box::new(status(StatusCode::BAD_REQUEST, "unreadable body"))),
    }
}

/// The request's host, lowercase and without its port: the `Host` header,
/// or the request target's authority.
fn host_of(header: Option<&HeaderValue>, uri: &Uri) -> Option<String> {
    header
        .and_then(|h| h.to_str().ok())
        .or_else(|| uri.host())
        .map(|h| h.split(':').next().unwrap_or(h).to_ascii_lowercase())
}

/// A plain-text response.
fn status(code: StatusCode, text: &str) -> Reply {
    let mut r = hyper::Response::new(Full::new(Bytes::from(format!("{text}\n"))));
    *r.status_mut() = code;
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    r
}

/// `307` to `uri`'s path and query under `base`; 503 if that is not a
/// valid header value.
fn redirect(base: &str, uri: &Uri) -> Reply {
    let rest = uri.path_and_query().map_or("/", |pq| pq.as_str());
    let to = format!("{base}{rest}");
    let Ok(location) = HeaderValue::from_str(&to) else {
        return status(StatusCode::SERVICE_UNAVAILABLE, "app not running");
    };
    let mut r = status(
        StatusCode::TEMPORARY_REDIRECT,
        &format!("runs on the partner node: {to}"),
    );
    r.headers_mut().insert(LOCATION, location);
    r
}

/// The response a call's result becomes.
fn from_call(result: Result<Envelope, CallError>, max_response: u64) -> Reply {
    match result {
        Ok(reply) if u64::try_from(reply.body.len()).unwrap_or(u64::MAX) > max_response => {
            status(StatusCode::BAD_GATEWAY, "reply too large")
        }
        Ok(reply) => match Response::from_reply(&reply.into()) {
            Some(r) => response(r),
            None => status(StatusCode::BAD_GATEWAY, "malformed response from the app"),
        },
        Err(CallError::Overloaded) => {
            let mut r = status(StatusCode::SERVICE_UNAVAILABLE, "overloaded");
            r.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("1"));
            r
        }
        Err(CallError::Noproc | CallError::Noconnection) => {
            status(StatusCode::SERVICE_UNAVAILABLE, "app not running")
        }
        Err(CallError::Timeout) => status(StatusCode::GATEWAY_TIMEOUT, "no reply in time"),
        Err(CallError::TooLarge) => status(StatusCode::PAYLOAD_TOO_LARGE, "body too large"),
        Err(CallError::AccessDenied) => status(StatusCode::FORBIDDEN, "refused"),
        Err(CallError::Exited(_)) => status(StatusCode::BAD_GATEWAY, "the app failed"),
    }
}

/// An app's response as hyper's; 502 for a status or header HTTP does
/// not allow.
fn response(r: Response) -> Reply {
    let Ok(code) = StatusCode::from_u16(r.status) else {
        return status(StatusCode::BAD_GATEWAY, "malformed response from the app");
    };
    let mut out = hyper::Response::new(Full::new(Bytes::from(r.body)));
    *out.status_mut() = code;
    for (name, value) in &r.headers {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            return status(StatusCode::BAD_GATEWAY, "malformed response from the app");
        };
        out.headers_mut().append(name, value);
    }
    out
}

/// The status the driver answers for an app's reply `body` in the
/// response format (`bulkhead_sdk::http::RESPONSE_SCHEMA`): the app's,
/// or 502. For the fuzz target `http_reply`.
#[cfg(fuzzing)]
#[doc(hidden)]
#[must_use]
pub fn fuzz_reply(body: &[u8]) -> u16 {
    let msg = bulkhead_sdk::Envelope {
        content_type: "application/cbor".into(),
        schema: Some(bulkhead_sdk::http::RESPONSE_SCHEMA.into()),
        body: body.to_vec(),
    };
    Response::from_reply(&msg)
        .map_or(StatusCode::BAD_GATEWAY, |r| response(r).status())
        .as_u16()
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod serve_tests;

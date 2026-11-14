//! The node's metrics endpoint as a driver app (ROADMAP DA7): Prometheus's
//! `GET /metrics`, unauthenticated by design, served outside the node.
//!
//! The node binds `[metrics] address` and passes the listener (`metrics`);
//! the driver accepts, serves HTTP/1.1 with hyper, and asks the node for
//! the text on each scrape (`Request::MetricsText`, which its profile's
//! `metrics` flag allows and nothing else). It reads no files, calls no
//! app and holds no secret: a bug in its parser reaches the metrics, which
//! are public anyway.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bulkhead_native::sender::Sender;
use bulkhead_native::sender_async::AsyncSender;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

/// The name of the listener the node passes.
pub const LISTENER: &str = "metrics";

/// The driver's init arguments (JSON), every key optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Connections open at once; more are closed on accept.
    pub max_connections: usize,
    /// How long a client may take to send a request's headers.
    pub header_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_connections: 64,
            header_timeout_ms: 10_000,
        }
    }
}

impl Config {
    /// The node's arguments; empty for the defaults.
    ///
    /// # Errors
    ///
    /// Not this format, or no connection allowed.
    pub fn from_args(args: &[u8]) -> Result<Self, String> {
        if args.is_empty() {
            return Ok(Self::default());
        }
        let config: Self =
            serde_json::from_slice(args).map_err(|e| format!("init arguments: {e}"))?;
        if config.max_connections == 0 {
            return Err("init arguments: max_connections must be at least 1".into());
        }
        Ok(config)
    }
}

/// The metrics driver.
#[derive(Debug)]
pub struct Metrics;

impl App for Metrics {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let config = Config::from_args(args)?;
        let socket = bulkhead_native::driver::listeners()
            .into_iter()
            .find(|(name, _)| name == LISTENER)
            .map(|(_, fd)| std::net::TcpListener::from(fd))
            .ok_or_else(|| format!("no listener named {LISTENER} passed"))?;
        let sender = bulkhead_native::app::Ctx::current()
            .sender()
            .map_err(|e| format!("sender channel: {e:?}"))?;
        std::thread::Builder::new()
            .name("metrics".into())
            .spawn(move || worker(sender, socket, &config))
            .map_err(|e| format!("worker thread: {e}"))?;
        Ok(Self)
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, _msg: Envelope) -> Reply {
        Reply::Now(Envelope::text("bulkhead-metrics"))
    }
}

/// The worker thread: accepts and serves on a runtime of its own. A
/// sender that cannot become async ends the process; the node restarts it.
fn worker(sender: Sender, socket: std::net::TcpListener, config: &Config) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            bulkhead_sdk::error!("worker runtime: {e}");
            std::process::exit(70);
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let sender = match AsyncSender::new(sender) {
            Ok(s) => s,
            Err(e) => {
                bulkhead_sdk::error!("worker sender: {e}");
                std::process::exit(70);
            }
        };
        tokio::task::spawn_local(sender.clone().pump());
        let listener = match socket
            .set_nonblocking(true)
            .and_then(|()| tokio::net::TcpListener::from_std(socket))
        {
            Ok(l) => l,
            Err(e) => {
                bulkhead_sdk::error!("listener: {e}");
                std::process::exit(70);
            }
        };
        let slots = Arc::new(Semaphore::new(config.max_connections));
        let header_timeout = Duration::from_millis(config.header_timeout_ms.max(1));
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    bulkhead_sdk::warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let sender = sender.clone();
            tokio::task::spawn_local(async move {
                let service = hyper::service::service_fn(move |req| {
                    let sender = sender.clone();
                    async move { Ok::<_, Infallible>(answer(&req, &sender).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(header_timeout)
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
                drop(permit);
            });
        }
    });
}

/// `GET /metrics` (or `/`): the node's text; anything else 404.
async fn answer<B>(req: &Request<B>, sender: &AsyncSender) -> Response<Full<Bytes>> {
    if req.method() != Method::GET || !matches!(req.uri().path(), "/metrics" | "/") {
        return status(StatusCode::NOT_FOUND, "GET /metrics\n");
    }
    match sender.metrics_text().await {
        Ok(text) => {
            let mut r = Response::new(Full::new(Bytes::from(text)));
            r.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_static("text/plain; version=0.0.4"),
            );
            r
        }
        Err(e) => {
            bulkhead_sdk::warn!("metrics from the node: {e:?}");
            status(StatusCode::SERVICE_UNAVAILABLE, "the node did not answer\n")
        }
    }
}

fn status(code: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *r.status_mut() = code;
    r
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;

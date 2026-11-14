//! The driver app: its worker threads, its host events, its metrics.
//!
//! - `threads` worker threads, each a tokio current-thread runtime with
//!   a sender channel of its own (`bulkhead_native::sender_async`): a task
//!   per connection, served by hyper's HTTP/1.1 server, each request a call
//!   to the app owning its route, awaited (spike S41).
//! - Worker 0 accepts on every listener and hands each connection to the
//!   workers in turn: the runner's seccomp filter allows `accept4` on the
//!   descriptors the node passed only, so a duplicate cannot accept.
//! - The callback thread takes the host event `routes` (the node's route
//!   table) and the node's call to reload certificates ([`RELOAD_SCHEMA`]).
//!   A request for an app that does not run here is redirected or
//!   forwarded as its listener says, the node asked at once where the app
//!   runs (`serve`).
//! - Worker 0 adds the counters to the app's metrics once a second, not
//!   one frame per request.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::TcpListener as StdListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use bulkhead_native::protocol::MetricKind;
use bulkhead_native::sender::Sender;
use bulkhead_native::sender_async::AsyncSender;
use bulkhead_sdk::http::{ROUTES_EVENT, Routes};
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Info, Reply};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::Config;
use crate::serve::{Listening, Shared, serve};
use crate::tls::{IngressTls, SdkFiles};

/// The schema of the node's call asking the driver to reload every
/// listener's certificates; answered with [`Reloaded`] as JSON.
pub const RELOAD_SCHEMA: &str = "bulkhead/ingress-reload";

/// How often worker 0 adds the counters to the app's metrics.
const METRICS_EVERY: Duration = Duration::from_secs(1);

/// The answer to a reload: how many listeners have certificates, and each
/// that failed (it keeps its old certificates).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reloaded {
    /// Listeners serving HTTPS.
    pub certificates: usize,
    /// `<listener>: <why>` for each that failed.
    pub failed: Vec<String>,
}

/// The ingress driver.
#[derive(Debug)]
pub struct Ingress {
    shared: Arc<Shared>,
    listening: Arc<Vec<Arc<Listening>>>,
}

/// A connection handed from worker 0 to a worker.
#[derive(Debug)]
struct Handed {
    stream: std::net::TcpStream,
    /// Its listener, by index.
    listener: usize,
    /// Its place under the listener's `max_connections`.
    permit: OwnedSemaphorePermit,
}

/// What worker 0 holds beside its own connections.
struct Accepting {
    sockets: Vec<StdListener>,
    inboxes: Vec<UnboundedSender<Handed>>,
    metrics: Option<MetricIds>,
}

/// The app's metrics for the counters, registered before the sender
/// became async.
#[derive(Debug, Clone, Copy)]
struct MetricIds {
    requests: u32,
    server_errors: u32,
    not_found: u32,
    redirected: u32,
    forwarded: u32,
    refused: u32,
}

impl MetricIds {
    /// Register the counters on `sender`; `None` when the app may not
    /// (`bulkhead:metrics` not granted).
    fn register(sender: &mut Sender) -> Option<Self> {
        let mut counter = |name: &str, help: &str| {
            sender
                .register_metric(MetricKind::Counter, name, help, &[], &[])
                .map_err(|e| bulkhead_sdk::warn!("no metric {name}: {e:?}"))
                .ok()
        };
        Some(Self {
            requests: counter("requests_total", "HTTP requests answered.")?,
            server_errors: counter(
                "server_errors_total",
                "HTTP requests answered with a 5xx status.",
            )?,
            not_found: counter("not_found_total", "HTTP requests that matched no route.")?,
            redirected: counter(
                "redirected_total",
                "HTTP requests redirected to the failover partner.",
            )?,
            forwarded: counter(
                "forwarded_total",
                "HTTP requests forwarded to the failover partner.",
            )?,
            refused: counter(
                "refused_connections_total",
                "Connections closed on accept, over max_connections.",
            )?,
        })
    }
}

impl App for Ingress {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let config = Config::from_args(args)?;
        let mut passed: HashMap<String, _> =
            bulkhead_native::driver::listeners().into_iter().collect();
        let mut sockets = Vec::new();
        let mut listening = Vec::new();
        for l in &config.listeners {
            let fd = passed
                .remove(&l.name)
                .ok_or_else(|| format!("listener {}: no socket passed by that name", l.name))?;
            sockets.push(StdListener::from(fd));
            let tls = if l.tls.is_empty() {
                None
            } else {
                Some(
                    IngressTls::load(l.tls.clone(), SdkFiles)
                        .map_err(|e| format!("listener {}: {e}", l.name))?,
                )
            };
            listening.push(Arc::new(Listening {
                config: l.clone(),
                tls,
            }));
        }
        let shared = Arc::new(Shared::default());
        let listening = Arc::new(listening);
        let mut workers = Vec::new();
        let mut inboxes = Vec::new();
        for _ in 0..config.threads {
            let sender = bulkhead_native::app::Ctx::current()
                .sender()
                .map_err(|e| format!("sender channel: {e:?}"))?;
            let (tx, rx) = unbounded_channel();
            inboxes.push(tx);
            workers.push((sender, rx));
        }
        let mut accepting = workers.first_mut().map(|(sender, _)| Accepting {
            sockets,
            inboxes,
            metrics: MetricIds::register(sender),
        });
        for (sender, inbox) in workers {
            let (shared, listening, accepting) =
                (shared.clone(), listening.clone(), accepting.take());
            std::thread::Builder::new()
                .name("ingress".into())
                .spawn(move || worker(sender, inbox, &shared, &listening, accepting))
                .map_err(|e| format!("worker thread: {e}"))?;
        }
        Ok(Self { shared, listening })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        if msg.schema.as_deref() != Some(RELOAD_SCHEMA) {
            return Reply::Now(Envelope::text("bulkhead-ingress"));
        }
        let reloaded = reload(&self.listening);
        match Envelope::json(&reloaded) {
            Ok(env) => Reply::Now(env),
            Err(e) => Reply::Now(Envelope::text(e.to_string())),
        }
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        let Info::Event { name, msg } = info else {
            return;
        };
        if name == ROUTES_EVENT {
            match Routes::from_event(&msg.body) {
                Ok(routes) => {
                    *self
                        .shared
                        .routes
                        .write()
                        .unwrap_or_else(PoisonError::into_inner) = routes;
                }
                Err(e) => bulkhead_sdk::error!("routes event: {e}"),
            }
        }
    }
}

/// Reload every listener's certificates from their files.
fn reload(listening: &[Arc<Listening>]) -> Reloaded {
    let mut certificates = 0;
    let mut failed = Vec::new();
    for l in listening {
        if let Some(tls) = &l.tls {
            certificates += 1;
            if let Err(e) = tls.reload() {
                failed.push(format!("{}: {e}", l.config.name));
            }
        }
    }
    Reloaded {
        certificates,
        failed,
    }
}

/// One worker thread: its runtime, its sender, its connections; on worker
/// 0 the accepts, the metrics and the certificate timers too. A sender
/// that cannot become async ends the process, and the node restarts it.
fn worker(
    sender: Sender,
    mut inbox: UnboundedReceiver<Handed>,
    shared: &Arc<Shared>,
    listening: &Arc<Vec<Arc<Listening>>>,
    accepting: Option<Accepting>,
) {
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
        if let Some(a) = accepting {
            start_accepting(a, shared, listening, &sender);
        }
        while let Some(handed) = inbox.recv().await {
            let (shared, listening, sender) = (shared.clone(), listening.clone(), sender.clone());
            tokio::task::spawn_local(async move {
                connection(handed, &listening, &shared, &sender).await;
            });
        }
    });
}

/// Worker 0's tasks beside its connections.
fn start_accepting(
    a: Accepting,
    shared: &Arc<Shared>,
    listening: &Arc<Vec<Arc<Listening>>>,
    sender: &AsyncSender,
) {
    let next = Arc::new(AtomicU64::new(0));
    for (i, socket) in a.sockets.into_iter().enumerate() {
        let slots = Arc::new(Semaphore::new(listening[i].config.max_connections));
        let listener = match socket
            .set_nonblocking(true)
            .and_then(|()| TcpListener::from_std(socket))
        {
            Ok(l) => l,
            Err(e) => {
                bulkhead_sdk::error!("listener {}: {e}", listening[i].config.name);
                std::process::exit(70);
            }
        };
        tokio::task::spawn_local(accept(
            listener,
            i,
            slots,
            a.inboxes.clone(),
            next.clone(),
            shared.clone(),
        ));
    }
    if let Some(ids) = a.metrics {
        tokio::task::spawn_local(flush_metrics(ids, shared.clone(), sender.clone()));
    }
    for l in listening.iter() {
        if let Some(tls) = &l.tls
            && l.config.tls_reload_s > 0
        {
            let every = Duration::from_secs(l.config.tls_reload_s);
            tokio::task::spawn_local(watch_certificates(
                l.config.name.clone(),
                tls.clone(),
                every,
            ));
        }
    }
}

/// Accept on listener `index` and hand each connection to the next
/// worker in turn; over `max_connections`, close it and count it.
async fn accept(
    listener: TcpListener,
    index: usize,
    slots: Arc<Semaphore>,
    inboxes: Vec<UnboundedSender<Handed>>,
    next: Arc<AtomicU64>,
    shared: Arc<Shared>,
) {
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
            shared.counters.refused.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let Ok(stream) = stream.into_std() else {
            continue;
        };
        let turn = next.fetch_add(1, Ordering::Relaxed);
        let i = usize::try_from(turn).unwrap_or(0) % inboxes.len().max(1);
        let _ = inboxes[i].send(Handed {
            stream,
            listener: index,
            permit,
        });
    }
}

/// Serve one connection: the TLS handshake within the header timeout,
/// then HTTP/1.1 until it closes.
async fn connection(
    handed: Handed,
    listening: &[Arc<Listening>],
    shared: &Arc<Shared>,
    sender: &AsyncSender,
) {
    let Handed {
        stream,
        listener,
        permit,
    } = handed;
    let l = listening[listener].clone();
    let stream = match stream
        .set_nonblocking(true)
        .and_then(|()| tokio::net::TcpStream::from_std(stream))
    {
        Ok(s) => s,
        Err(_) => return,
    };
    match &l.tls {
        None => http1(stream, l.clone(), shared.clone(), sender.clone()).await,
        Some(tls) => {
            let handshake = tls.acceptor().accept(stream);
            match tokio::time::timeout(l.config.header_timeout(), handshake).await {
                Ok(Ok(s)) => http1(s, l.clone(), shared.clone(), sender.clone()).await,
                Ok(Err(e)) => bulkhead_sdk::debug!("TLS handshake failed: {e}"),
                Err(_) => bulkhead_sdk::debug!("TLS handshake timed out"),
            }
        }
    }
    drop(permit);
}

async fn http1<IO>(io: IO, l: Arc<Listening>, shared: Arc<Shared>, sender: AsyncSender)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
{
    let header_timeout = l.config.header_timeout();
    let service = hyper::service::service_fn(move |req| {
        let (l, shared, sender) = (l.clone(), shared.clone(), sender.clone());
        async move { Ok::<_, Infallible>(serve(req, &l, &shared, &sender).await) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(header_timeout)
        .serve_connection(TokioIo::new(io), service)
        .await;
}

/// Add what the counters gained to the app's metrics, once a second.
async fn flush_metrics(ids: MetricIds, shared: Arc<Shared>, sender: AsyncSender) {
    let c = &shared.counters;
    let pairs = [
        (&c.requests, ids.requests),
        (&c.server_errors, ids.server_errors),
        (&c.not_found, ids.not_found),
        (&c.redirected, ids.redirected),
        (&c.forwarded, ids.forwarded),
        (&c.refused, ids.refused),
    ];
    let mut every = tokio::time::interval(METRICS_EVERY);
    loop {
        every.tick().await;
        for (counter, id) in pairs {
            let delta = counter.swap(0, Ordering::Relaxed);
            if delta > 0 {
                #[allow(clippy::cast_precision_loss)] // Counts in a second.
                sender.metric_add(id, delta as f64);
            }
        }
    }
}

/// Reload `tls` whenever its files change, checking every `every`.
async fn watch_certificates(name: String, tls: IngressTls, every: Duration) {
    let mut ticks = tokio::time::interval(every);
    ticks.tick().await;
    loop {
        ticks.tick().await;
        match tls.reload_if_changed() {
            Ok(true) => bulkhead_sdk::info!("listener {name}: TLS certificates reloaded"),
            Ok(false) => {}
            Err(e) => {
                bulkhead_sdk::error!(
                    "listener {name}: TLS reload failed; keeping the old certificates: {e}"
                );
            }
        }
    }
}

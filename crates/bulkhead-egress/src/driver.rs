//! The driver app: the callback thread, which takes the node's calls, and
//! worker threads, which send them (`threads`, 4 by default; ROADMAP
//! DA4b).
//!
//! - The node calls with [`EGRESS_SCHEMA`], the body an [`EgressRequest`]
//!   in CBOR; the callback thread leaves the call waiting and hands the
//!   request to the worker with the fewest in flight, the first of those
//!   ([`least_busy`]): one at a time, every request goes to worker 0 and
//!   reuses its connections; under load they spread.
//! - Each worker runs on a tokio current-thread runtime with a sender
//!   channel of its own, and answers each of its calls itself
//!   (`Request::Reply`), with `Result<HttpResponse, HttpError>` in CBOR.
//!   Each worker has a pool of its own, so a connection is used on the
//!   runtime that drives it (a pool shared by SQL's workers made requests
//!   cross threads twice; BASELINE.md, 2026-11-23): `pool_max_idle_per_host`
//!   is per worker. Worker 0 asks the node for each new connection
//!   (`Request::ConnectTo`), and counts them.
//!
//! The node has checked the request against the app's grant, `allow`,
//! `methods` and limits, and holds its permit while the driver works.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bulkhead_native::protocol::{HttpError, MetricKind};
use bulkhead_native::sender::Sender;
use bulkhead_native::sender_async::AsyncSender;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};
use tokio::sync::mpsc;

use crate::client::{Connector, Pool, Wanted};
use crate::config::Config;
use crate::request::{Answer, EGRESS_SCHEMA, EgressRequest};

/// A request for the worker, and the node's call to answer.
#[derive(Debug)]
struct Job {
    call: u64,
    request: EgressRequest,
}

/// The egress driver's callback side: each worker's queue and its
/// requests in flight.
#[derive(Debug)]
pub struct Egress {
    workers: Vec<(mpsc::UnboundedSender<Job>, Arc<AtomicUsize>)>,
    /// Whether a traced request carries `traceparent` (ROADMAP O1).
    traceparent: bool,
}

/// The first of the workers with the fewest requests in flight, by their
/// counts; 0 for none.
pub fn least_busy(in_flight: impl IntoIterator<Item = usize>) -> usize {
    in_flight
        .into_iter()
        .enumerate()
        .min_by_key(|&(i, n)| (n, i))
        .map_or(0, |(i, _)| i)
}

/// The counters, registered before the sender became async.
#[derive(Debug, Clone, Copy)]
struct Metrics {
    connections: u32,
}

impl Metrics {
    fn register(sender: &mut Sender, app: &str) -> Option<Self> {
        let labels = [("for", app)];
        sender
            .register_metric(
                MetricKind::Counter,
                "connections_total",
                "Connections the node opened for the app's requests.",
                &labels,
                &[],
            )
            .map_err(|e| bulkhead_sdk::warn!("no metric connections_total: {e:?}"))
            .ok()
            .map(|connections| Self { connections })
    }
}

/// What a worker thread starts with.
struct Start {
    sender: Sender,
    jobs: mpsc::UnboundedReceiver<Job>,
    in_flight: Arc<AtomicUsize>,
    pool: Pool,
    /// Worker 0's: the connections the pool wants, and their counter.
    connects: Option<(mpsc::UnboundedReceiver<Wanted>, Option<Metrics>)>,
}

impl App for Egress {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let config = Config::from_args(args)?;
        let config_traceparent = config.traceparent;
        // Refused here rather than on the first request.
        config.roots()?;
        // The app's client certificate (ROADMAP DA4b), from the files the
        // node bound into this root.
        let identity = config.identity(bulkhead_sdk::files::read)?;
        let (wanted, wanted_rx) = mpsc::unbounded_channel::<Wanted>();
        let mut wanted_rx = Some(wanted_rx);
        let mut starts = Vec::with_capacity(config.threads);
        let mut workers = Vec::with_capacity(config.threads);
        for _ in 0..config.threads {
            let pool = Pool::new(
                &config,
                Connector::new(wanted.clone(), config.connect_timeout()),
                identity
                    .as_ref()
                    .map(|(chain, key)| (chain.clone(), key.clone_key())),
            )
            .map_err(|e| format!("egress for {}: {e}", config.app))?;
            let mut sender = bulkhead_native::app::Ctx::current()
                .sender()
                .map_err(|e| format!("sender channel: {e:?}"))?;
            let connects = wanted_rx
                .take()
                .map(|rx| (rx, Metrics::register(&mut sender, &config.app)));
            let (jobs, jobs_rx) = mpsc::unbounded_channel();
            let in_flight = Arc::new(AtomicUsize::new(0));
            workers.push((jobs, in_flight.clone()));
            starts.push(Start {
                sender,
                jobs: jobs_rx,
                in_flight,
                pool,
                connects,
            });
        }
        for start in starts {
            std::thread::Builder::new()
                .name("egress".into())
                .spawn(move || worker(start))
                .map_err(|e| format!("worker thread: {e}"))?;
        }
        Ok(Self {
            workers,
            traceparent: config_traceparent,
        })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Reply {
        if msg.schema.as_deref() != Some(EGRESS_SCHEMA) {
            return Reply::Now(Envelope::text("bulkhead-egress"));
        }
        let answer = match msg.decode::<EgressRequest>() {
            Ok(mut request) => {
                if self.traceparent
                    && let Some(trace) = msg.trace.as_deref()
                {
                    add_traceparent(&mut request.request, trace);
                }
                let i = least_busy(self.workers.iter().map(|(_, n)| n.load(Ordering::Relaxed)));
                let (queue, in_flight) = &self.workers[i];
                in_flight.fetch_add(1, Ordering::Relaxed);
                if queue
                    .send(Job {
                        call: call.0,
                        request,
                    })
                    .is_ok()
                {
                    return Reply::Later;
                }
                Err(HttpError::Other("the driver's worker stopped".into()))
            }
            Err(e) => Err(HttpError::Invalid(format!("request: {e}"))),
        };
        Reply::Now(answer_envelope(&answer))
    }
}

/// Give `request` the W3C `traceparent` header of `trace` (ROADMAP O1),
/// unless the app set one of its own.
pub fn add_traceparent(
    request: &mut bulkhead_native::protocol::HttpRequest,
    trace: &bulkhead_native::protocol::Trace,
) {
    if request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("traceparent"))
    {
        return;
    }
    request.headers.push((
        "traceparent".to_string(),
        bulkhead_native::protocol::Bytes(trace.traceparent().into_bytes()),
    ));
}

/// `answer` as the driver's answer to a call.
#[must_use]
pub fn answer_envelope(answer: &Answer) -> Envelope {
    match Envelope::cbor(answer) {
        Ok(env) => env.with_schema(EGRESS_SCHEMA),
        Err(e) => Envelope::text(e.to_string()),
    }
}

/// A worker thread: its share of the requests on its own runtime. A sender
/// that cannot become async ends the process, and the node restarts it.
fn worker(start: Start) {
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
    local.block_on(&runtime, run(start));
}

async fn run(start: Start) {
    let Start {
        sender,
        mut jobs,
        in_flight,
        pool,
        connects,
    } = start;
    let sender = match AsyncSender::new(sender) {
        Ok(s) => s,
        Err(e) => {
            bulkhead_sdk::error!("worker sender: {e}");
            std::process::exit(70);
        }
    };
    tokio::task::spawn_local(sender.clone().pump());
    if let Some((mut wanted_rx, metrics)) = connects {
        // Connections the pool wants, asked of the node.
        let sender = sender.clone();
        tokio::task::spawn_local(async move {
            while let Some((host, port, reply)) = wanted_rx.recv().await {
                let sender = sender.clone();
                tokio::task::spawn_local(async move {
                    let got = sender.connect_to(&host, port).await;
                    if let (Ok(_), Some(m)) = (&got, metrics) {
                        sender.metric_add(m.connections, 1.0);
                    }
                    let _ = reply.send(got);
                });
            }
        });
    }
    let pool = std::rc::Rc::new(pool);
    while let Some(job) = jobs.recv().await {
        let (sender, pool, in_flight) = (sender.clone(), pool.clone(), in_flight.clone());
        tokio::task::spawn_local(async move {
            let EgressRequest {
                request,
                first_byte_ms,
                between_ms,
            } = job.request;
            let answer = pool
                .send(
                    request,
                    Duration::from_millis(first_byte_ms),
                    Duration::from_millis(between_ms),
                )
                .await;
            // Before the answer, so the node's next request sees it.
            in_flight.fetch_sub(1, Ordering::Relaxed);
            sender
                .reply(job.call, answer_envelope(&answer).into())
                .await;
        });
    }
}

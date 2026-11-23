//! The driver app: the callback thread, which takes the node's requests,
//! and worker threads, which run them (`threads`, 4 by default, no more
//! than the pool's connections; ROADMAP DA3b).
//!
//! - The node calls with [`SQL_SCHEMA`], the body the protocol's
//!   `SqlRequest` in CBOR; the callback thread leaves the call waiting and
//!   hands the request to a worker: of those whose pool has a connection
//!   free or room for one (all, if none has), the one with the fewest
//!   requests in flight, the first of those, so one at a time every
//!   request goes to worker 0 and under load they spread; a
//!   transaction's statements to the worker that began it, which holds its
//!   connection (a transaction's number names its worker, [`tx_worker`]). A cast
//!   with that schema is run with no answer: the node's rollback of a
//!   transaction whose instance exited.
//! - Each worker runs on a tokio current-thread runtime with a sender
//!   channel of its own, and answers each of its calls itself
//!   (`Request::Reply`), with `Result<SqlAnswer, SqlError>` in CBOR. Each
//!   worker has a pool of its own, a share of `pool_size`, so a connection
//!   is used on the runtime that drives it: one pool shared by the workers
//!   made most statements cross threads twice, and cost more than one
//!   thread did (BASELINE.md, 2026-11-23). Worker 0 asks the node for each
//!   new connection's stream (`Request::Connect`) and adds the workers'
//!   counts to the app's metrics once a second, as the metrics a sender
//!   registers are its own.
//!
//! The node has checked the app's grant and holds its permit; the driver
//! is told no app, and ignores the request's datasource, being one
//! datasource's alone. Transactions are numbered here, and the node maps
//! its instances' numbers to these.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bulkhead_native::protocol::{self as p, MetricKind, SqlAnswer, SqlError, SqlRequest};
use bulkhead_native::sender::Sender;
use bulkhead_native::sender_async::AsyncSender;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};
use tokio::sync::{mpsc, oneshot};

use crate::config::{Config, Password, Tls, trim_password};
use crate::pool::{self, Conn, Opened, Pool};
use crate::statements;

/// The schema of the node's requests and the driver's answers.
pub const SQL_SCHEMA: &str = "bulkhead/sql";

/// What the driver answers a request.
pub type Answer = Result<SqlAnswer, SqlError>;

/// A request for the worker, and the node's call to answer, if any.
#[derive(Debug)]
struct Job {
    call: Option<u64>,
    request: SqlRequest,
}

/// The SQL driver's callback side: each worker's queue, pool and requests
/// in flight.
#[derive(Debug)]
pub struct Sql {
    workers: Vec<Handle>,
}

/// A worker, as the callback thread sees it.
#[derive(Debug)]
struct Handle {
    jobs: mpsc::UnboundedSender<Job>,
    pool: Pool,
    in_flight: Arc<AtomicUsize>,
}

/// Worker `i`'s share of `pool_size` connections among `workers`.
#[must_use]
pub fn pool_share(pool_size: usize, workers: usize, i: usize) -> usize {
    let workers = workers.max(1);
    pool_size / workers + usize::from(i < pool_size % workers)
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

/// Whether `pool` has a connection free, or room to open one.
fn has_room(pool: &Pool) -> bool {
    let s = pool.status();
    s.available > 0 || s.size < s.max_size
}

/// How often worker 0 adds the counts to the app's metrics.
const METRICS_EVERY: Duration = Duration::from_secs(1);

/// The worker that began transaction `tx`, of `workers`: its number is
/// `n * workers + worker`.
#[must_use]
pub fn tx_worker(tx: u32, workers: usize) -> usize {
    usize::try_from(tx).unwrap_or(0) % workers.max(1)
}

/// Transaction number `n` of `worker`, of `workers`, and the `n` after it:
/// numbers stay below `u32::MAX`, so [`tx_worker`] holds when `n` wraps.
fn tx_number(n: u32, worker: u32, workers: u32) -> (u32, u32) {
    let span = u32::MAX / workers.max(1);
    let n = n % span;
    (n * workers + worker, (n + 1) % span)
}

/// What the workers counted since worker 0 last added it to the metrics.
#[derive(Debug, Default)]
struct Counts {
    statements: AtomicU64,
    failed: AtomicU64,
    connections: AtomicU64,
}

/// The counters, registered on worker 0's sender before it became async.
#[derive(Debug, Clone, Copy)]
struct Metrics {
    statements: u32,
    failed: u32,
    connections: u32,
}

impl Metrics {
    fn register(sender: &mut Sender, datasource: &str) -> Option<Self> {
        let labels = [("datasource", datasource)];
        let mut counter = |name: &str, help: &str| {
            sender
                .register_metric(MetricKind::Counter, name, help, &labels, &[])
                .map_err(|e| bulkhead_sdk::warn!("no metric {name}: {e:?}"))
                .ok()
        };
        Some(Self {
            statements: counter(
                "requests_total",
                "Requests run for the node: statements, begins and ends.",
            )?,
            failed: counter("requests_failed_total", "Requests that failed.")?,
            connections: counter(
                "connections_total",
                "Connections to the database the node opened for the pool.",
            )?,
        })
    }
}

/// What a worker thread starts with.
struct Start {
    worker: u32,
    workers: u32,
    sender: Sender,
    jobs: mpsc::UnboundedReceiver<Job>,
    in_flight: Arc<AtomicUsize>,
    pool: Pool,
    max_result_bytes: u64,
    counts: Arc<Counts>,
    first: Option<First>,
}

/// What worker 0 does besides its requests.
struct First {
    connects: mpsc::UnboundedReceiver<oneshot::Sender<Opened>>,
    metrics: Option<Metrics>,
}

/// A file under `/driver/`, for `what`.
fn read_file(path: &str, what: &str) -> Result<Vec<u8>, String> {
    bulkhead_sdk::files::read(path).map_err(|e| format!("{what} {path}: {}", e.kind()))
}

impl App for Sql {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let config = Config::from_args(args)?;
        let secret = match &config.password {
            None => pool::Secret::None,
            // Read now, so a missing file refuses the start, and again for
            // each new connection (ROADMAP DA3b).
            Some(Password::File(path)) => {
                let first = trim_password(&read_file(path, "password file")?).to_vec();
                let path = path.clone();
                pool::Secret::File {
                    read: Box::new(move || {
                        bulkhead_sdk::files::read(&path).map(|b| trim_password(&b).to_vec())
                    }),
                    last: std::sync::Mutex::new(first),
                }
            }
            Some(Password::Value(value)) => pool::Secret::Fixed(value.clone().into_bytes()),
        };
        let ca = match &config.tls {
            Tls::Disable => None,
            Tls::Require { ca_file } => Some(read_file(ca_file, "CA file")?),
        };
        let (connector, connects) = mpsc::unbounded_channel();
        let secret = Arc::new(secret);
        let n = config.workers();
        let workers_u32 = u32::try_from(n).map_err(|e| e.to_string())?;
        let counts = Arc::new(Counts::default());
        let mut connects = Some(connects);
        let mut queues = Vec::with_capacity(n);
        let mut starts = Vec::with_capacity(n);
        for worker in 0..workers_u32 {
            let size = pool_share(config.pool_size, n, worker as usize);
            let pool = pool::build(
                &config,
                size,
                secret.clone(),
                ca.as_deref(),
                connector.clone(),
            )
            .map_err(|e| format!("datasource {}: {e}", config.datasource))?;
            let mut sender = bulkhead_native::app::Ctx::current()
                .sender()
                .map_err(|e| format!("sender channel: {e:?}"))?;
            let first = connects.take().map(|connects| First {
                connects,
                metrics: Metrics::register(&mut sender, &config.datasource),
            });
            let (jobs, jobs_rx) = mpsc::unbounded_channel();
            let in_flight = Arc::new(AtomicUsize::new(0));
            queues.push(Handle {
                jobs,
                pool: pool.clone(),
                in_flight: in_flight.clone(),
            });
            starts.push(Start {
                worker,
                workers: workers_u32,
                sender,
                jobs: jobs_rx,
                in_flight,
                pool,
                max_result_bytes: config.max_result_bytes,
                counts: counts.clone(),
                first,
            });
        }
        for start in starts {
            std::thread::Builder::new()
                .name("sql".into())
                .spawn(move || worker(start))
                .map_err(|e| format!("worker thread: {e}"))?;
        }
        Ok(Self { workers: queues })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Reply {
        if msg.schema.as_deref() != Some(SQL_SCHEMA) {
            return Reply::Now(Envelope::text("bulkhead-sql"));
        }
        let answer = match msg.decode::<SqlRequest>() {
            Ok(request) => {
                if self.hand(Some(call.0), request) {
                    return Reply::Later;
                }
                Err(SqlError::Unavailable("the driver's worker stopped".into()))
            }
            Err(e) => Err(SqlError::Unavailable(format!("request: {e}"))),
        };
        Reply::Now(answer_envelope(&answer))
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        if msg.schema.as_deref() != Some(SQL_SCHEMA) {
            return;
        }
        if let Ok(request) = msg.decode::<SqlRequest>() {
            self.hand(None, request);
        }
    }
}

impl Sql {
    /// Hand `request` to its worker: a transaction's to the worker that
    /// began it; the rest to the least busy of the workers whose pool has
    /// room, or of all if none has (it waits there, up to the acquire
    /// timeout). False if that worker stopped.
    fn hand(&mut self, call: Option<u64>, request: SqlRequest) -> bool {
        let n = self.workers.len();
        let worker = match &request {
            SqlRequest::TxQuery { tx, .. }
            | SqlRequest::TxExecute { tx, .. }
            | SqlRequest::TxEnd { tx, .. } => tx_worker(*tx, n),
            _ => {
                let busy = |room: bool| {
                    self.workers.iter().map(move |w| {
                        if room && !has_room(&w.pool) {
                            usize::MAX
                        } else {
                            w.in_flight.load(Ordering::Relaxed)
                        }
                    })
                };
                let with_room = least_busy(busy(true));
                if has_room(&self.workers[with_room].pool) {
                    with_room
                } else {
                    least_busy(busy(false))
                }
            }
        };
        let w = &self.workers[worker];
        w.in_flight.fetch_add(1, Ordering::Relaxed);
        w.jobs.send(Job { call, request }).is_ok()
    }
}

/// `answer` as the driver's answer to a call.
#[must_use]
pub fn answer_envelope(answer: &Answer) -> Envelope {
    match Envelope::cbor(answer) {
        Ok(env) => env.with_schema(SQL_SCHEMA),
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

/// A worker's open transactions, by number.
struct State {
    pool: Pool,
    max_result_bytes: u64,
    txs: RefCell<HashMap<u32, Rc<Conn>>>,
    next_tx: Cell<u32>,
    worker: u32,
    workers: u32,
}

fn ended() -> SqlError {
    SqlError::Unavailable("the transaction has ended".into())
}

impl State {
    fn tx(&self, tx: u32) -> Result<Rc<Conn>, SqlError> {
        self.txs.borrow().get(&tx).cloned().ok_or_else(ended)
    }

    async fn run(&self, request: SqlRequest) -> Answer {
        match request {
            SqlRequest::Query {
                statement, params, ..
            } => {
                let conn = pool::get(&self.pool).await?;
                statements::query(&conn, &statement, params, self.max_result_bytes)
                    .await
                    .map(SqlAnswer::Rows)
            }
            SqlRequest::Execute {
                statement, params, ..
            } => {
                let conn = pool::get(&self.pool).await?;
                statements::execute(&conn, &statement, params)
                    .await
                    .map(SqlAnswer::Changed)
            }
            SqlRequest::Begin(_) => {
                let conn = pool::get(&self.pool).await?;
                statements::simple(&conn, "BEGIN").await?;
                let mut txs = self.txs.borrow_mut();
                let mut n = self.next_tx.get();
                let id = loop {
                    let (id, after) = tx_number(n, self.worker, self.workers);
                    n = after;
                    if !txs.contains_key(&id) {
                        break id;
                    }
                };
                self.next_tx.set(n);
                txs.insert(id, Rc::new(conn));
                Ok(SqlAnswer::Begun(id))
            }
            SqlRequest::TxQuery {
                tx,
                statement,
                params,
            } => {
                let conn = self.tx(tx)?;
                statements::query(&conn, &statement, params, self.max_result_bytes)
                    .await
                    .map(SqlAnswer::Rows)
            }
            SqlRequest::TxExecute {
                tx,
                statement,
                params,
            } => {
                let conn = self.tx(tx)?;
                statements::execute(&conn, &statement, params)
                    .await
                    .map(SqlAnswer::Changed)
            }
            SqlRequest::TxEnd { tx, commit } => {
                let conn = self.txs.borrow_mut().remove(&tx).ok_or_else(ended)?;
                // Back to the pool when the last statement on it is done;
                // the pool's reset rolls back whatever this leaves open.
                statements::simple(&conn, if commit { "COMMIT" } else { "ROLLBACK" }).await?;
                Ok(SqlAnswer::Ended)
            }
        }
    }
}

async fn run(start: Start) {
    let Start {
        worker,
        workers,
        sender,
        mut jobs,
        in_flight,
        pool,
        max_result_bytes,
        counts,
        first,
    } = start;
    let sender = match AsyncSender::new(sender) {
        Ok(s) => s,
        Err(e) => {
            bulkhead_sdk::error!("worker sender: {e}");
            std::process::exit(70);
        }
    };
    tokio::task::spawn_local(sender.clone().pump());
    if let Some(first) = first {
        start_first(first, &sender, &counts);
    }
    let state = Rc::new(State {
        pool,
        max_result_bytes,
        txs: RefCell::default(),
        next_tx: Cell::new(0),
        worker,
        workers,
    });
    while let Some(job) = jobs.recv().await {
        let (sender, state, counts) = (sender.clone(), state.clone(), counts.clone());
        let in_flight = in_flight.clone();
        tokio::task::spawn_local(async move {
            let answer = state.run(job.request).await;
            // Before the answer, so the node's next request sees it.
            in_flight.fetch_sub(1, Ordering::Relaxed);
            counts.statements.fetch_add(1, Ordering::Relaxed);
            if answer.is_err() {
                counts.failed.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(call) = job.call {
                sender.reply(call, answer_envelope(&answer).into()).await;
            }
        });
    }
}

/// Worker 0's tasks beside its requests: the streams the pool wants, asked
/// of the node, and the counts added to the metrics.
fn start_first(first: First, sender: &AsyncSender, counts: &Arc<Counts>) {
    let First {
        mut connects,
        metrics,
    } = first;
    {
        let (sender, counts) = (sender.clone(), counts.clone());
        tokio::task::spawn_local(async move {
            while let Some(reply) = connects.recv().await {
                let (sender, counts) = (sender.clone(), counts.clone());
                tokio::task::spawn_local(async move {
                    let got = sender.connect().await.map_err(|e| net_error(&e));
                    if got.is_ok() {
                        counts.connections.fetch_add(1, Ordering::Relaxed);
                    }
                    let _ = reply.send(got);
                });
            }
        });
    }
    if let Some(m) = metrics {
        tokio::task::spawn_local(flush_metrics(m, counts.clone(), sender.clone()));
    }
}

/// Add what the workers counted to the app's metrics, once a second.
async fn flush_metrics(ids: Metrics, counts: Arc<Counts>, sender: AsyncSender) {
    let pairs = [
        (&counts.statements, ids.statements),
        (&counts.failed, ids.failed),
        (&counts.connections, ids.connections),
    ];
    let mut every = tokio::time::interval(METRICS_EVERY);
    loop {
        every.tick().await;
        for (count, id) in pairs {
            let delta = count.swap(0, Ordering::Relaxed);
            if delta > 0 {
                #[allow(clippy::cast_precision_loss)] // Counts in a second.
                sender.metric_add(id, delta as f64);
            }
        }
    }
}

/// Why the node could not connect, for the app's `unavailable`.
fn net_error(e: &p::NetError) -> String {
    match e {
        p::NetError::Denied => "the node connects this driver nowhere".into(),
        p::NetError::Other(why) => why.clone(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;

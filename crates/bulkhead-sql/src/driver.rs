//! The driver app: the callback thread, which takes the node's requests,
//! and one worker thread, which runs them.
//!
//! - The node calls with [`SQL_SCHEMA`], the body the protocol's
//!   `SqlRequest` in CBOR; the callback thread leaves the call waiting and
//!   hands the request to the worker. A cast with that schema is run with
//!   no answer: the node's rollback of a transaction whose instance
//!   exited.
//! - The worker runs the pool on a tokio current-thread runtime with a
//!   sender channel of its own. It asks the node for each new connection's
//!   stream there (`Request::Connect`) and answers each call itself
//!   (`Request::Reply`), with `Result<SqlAnswer, SqlError>` in CBOR.
//!
//! The node has checked the app's grant and holds its permit; the driver
//! is told no app, and ignores the request's datasource, being one
//! datasource's alone. Transactions are numbered here, and the node maps
//! its instances' numbers to these.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

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

/// The SQL driver's callback side.
#[derive(Debug)]
pub struct Sql {
    jobs: mpsc::UnboundedSender<Job>,
}

/// The counters, registered before the sender became async.
#[derive(Debug, Clone, Copy)]
struct Metrics {
    statements: u32,
    failed: u32,
    connections: u32,
}

impl Metrics {
    fn register(sender: &mut Sender) -> Option<Self> {
        let mut counter = |name: &str, help: &str| {
            sender
                .register_metric(MetricKind::Counter, name, help, &[], &[])
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

/// What the worker thread starts with.
struct Start {
    sender: Sender,
    jobs: mpsc::UnboundedReceiver<Job>,
    connects: mpsc::UnboundedReceiver<oneshot::Sender<Opened>>,
    pool: Pool,
    max_result_bytes: u64,
    metrics: Option<Metrics>,
}

/// A file under `/driver/`, for `what`.
fn read_file(path: &str, what: &str) -> Result<Vec<u8>, String> {
    bulkhead_sdk::files::read(path).map_err(|e| format!("{what} {path}: {}", e.kind()))
}

impl App for Sql {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let config = Config::from_args(args)?;
        let password = match &config.password {
            None => None,
            Some(Password::File(path)) => {
                Some(trim_password(&read_file(path, "password file")?).to_vec())
            }
            Some(Password::Value(value)) => Some(value.clone().into_bytes()),
        };
        let ca = match &config.tls {
            Tls::Disable => None,
            Tls::Require { ca_file } => Some(read_file(ca_file, "CA file")?),
        };
        let (connector, connects) = mpsc::unbounded_channel();
        let pool = pool::build(&config, password, ca.as_deref(), connector)
            .map_err(|e| format!("datasource {}: {e}", config.datasource))?;
        let mut sender = bulkhead_native::app::Ctx::current()
            .sender()
            .map_err(|e| format!("sender channel: {e:?}"))?;
        let metrics = Metrics::register(&mut sender);
        let (jobs, jobs_rx) = mpsc::unbounded_channel();
        let start = Start {
            sender,
            jobs: jobs_rx,
            connects,
            pool,
            max_result_bytes: config.max_result_bytes,
            metrics,
        };
        std::thread::Builder::new()
            .name("sql".into())
            .spawn(move || worker(start))
            .map_err(|e| format!("worker thread: {e}"))?;
        Ok(Self { jobs })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Reply {
        if msg.schema.as_deref() != Some(SQL_SCHEMA) {
            return Reply::Now(Envelope::text("bulkhead-sql"));
        }
        let answer = match msg.decode::<SqlRequest>() {
            Ok(request) => {
                if self
                    .jobs
                    .send(Job {
                        call: Some(call.0),
                        request,
                    })
                    .is_ok()
                {
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
            let _ = self.jobs.send(Job {
                call: None,
                request,
            });
        }
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

/// The worker thread: the pool on its own runtime. A sender that cannot
/// become async ends the process, and the node restarts it.
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

/// The open transactions, by number.
struct State {
    pool: Pool,
    max_result_bytes: u64,
    txs: RefCell<HashMap<u32, Rc<Conn>>>,
    next_tx: Cell<u32>,
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
                let mut id = self.next_tx.get();
                while txs.contains_key(&id) {
                    id = id.wrapping_add(1);
                }
                self.next_tx.set(id.wrapping_add(1));
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
        sender,
        mut jobs,
        mut connects,
        pool,
        max_result_bytes,
        metrics,
    } = start;
    let sender = match AsyncSender::new(sender) {
        Ok(s) => s,
        Err(e) => {
            bulkhead_sdk::error!("worker sender: {e}");
            std::process::exit(70);
        }
    };
    tokio::task::spawn_local(sender.clone().pump());
    {
        // Streams the pool wants, asked of the node.
        let sender = sender.clone();
        tokio::task::spawn_local(async move {
            while let Some(reply) = connects.recv().await {
                let sender = sender.clone();
                tokio::task::spawn_local(async move {
                    let got = sender.connect().await.map_err(|e| net_error(&e));
                    if let (Ok(_), Some(m)) = (&got, metrics) {
                        sender.metric_add(m.connections, 1.0);
                    }
                    let _ = reply.send(got);
                });
            }
        });
    }
    let state = Rc::new(State {
        pool,
        max_result_bytes,
        txs: RefCell::default(),
        next_tx: Cell::new(0),
    });
    while let Some(job) = jobs.recv().await {
        let (sender, state) = (sender.clone(), state.clone());
        tokio::task::spawn_local(async move {
            let answer = state.run(job.request).await;
            if let Some(m) = metrics {
                sender.metric_add(m.statements, 1.0);
                if answer.is_err() {
                    sender.metric_add(m.failed, 1.0);
                }
            }
            if let Some(call) = job.call {
                sender.reply(call, answer_envelope(&answer).into()).await;
            }
        });
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

//! The driver app: the callback thread, which answers the node, and one
//! worker thread, which resolves.
//!
//! - The node calls with [`RESOLVE_SCHEMA`]; the callback thread leaves
//!   the call waiting and hands the name to the worker.
//! - The worker runs hickory on a tokio current-thread runtime with a
//!   sender channel of its own, and answers the node's call itself on it
//!   (`Request::Reply`, ROADMAP AP1b). Only the callback thread may ask
//!   the node for a socket, so the worker hands back by casting to its own
//!   instance for that: [`CONNECT_SCHEMA`] when hickory wants a stream to a
//!   nameserver, [`CLOSED_SCHEMA`] when hickory is done with one. Its
//!   manifest accepts messages from itself only (`accept_from`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use bulkhead_native::protocol::{self as p, MetricKind};
use bulkhead_native::sender::Sender;
use bulkhead_native::sender_async::AsyncSender;
use bulkhead_sdk::net::TcpStream;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::proto::op::Query;
use hickory_resolver::proto::rr::{Name, RecordType};
use hickory_resolver::{Hosts, Resolver};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::lookup::{self, Answer, Failure, RESOLVE_SCHEMA, Request};
use crate::provider::{NodeStreams, OneStream, Opened, Wanted};

/// A stream to a nameserver wanted, cast to its own instance.
pub const CONNECT_SCHEMA: &str = "bulkhead/resolve-connect";
/// A stream done with, cast to its own instance.
pub const CLOSED_SCHEMA: &str = "bulkhead/resolve-closed";

/// hickory's request `id` for a stream to `addr`.
#[derive(Debug, Serialize, Deserialize)]
struct Connect {
    id: u64,
    addr: SocketAddr,
}

/// The stream kept as `key` is done with.
#[derive(Debug, Serialize, Deserialize)]
struct Closed {
    key: u64,
}

/// A name for the worker to look up, for the node's call `call`.
#[derive(Debug)]
struct Job {
    call: u64,
    host: String,
}

/// The resolver driver's callback side.
#[derive(Debug)]
pub struct Resolve {
    jobs: mpsc::UnboundedSender<Job>,
    opened: mpsc::UnboundedSender<(u64, io::Result<Opened>)>,
    /// The streams the node opened, by key: the node counts each until
    /// it is dropped here.
    streams: HashMap<u64, TcpStream>,
    next_key: u64,
}

/// The counters, registered before the sender became async.
#[derive(Debug, Clone, Copy)]
struct Metrics {
    lookups: u32,
    failed: u32,
    streams: u32,
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
            lookups: counter("lookups_total", "Host names looked up for the node.")?,
            failed: counter(
                "lookups_failed_total",
                "Lookups that found no address: no such name, or no nameserver answered.",
            )?,
            streams: counter(
                "streams_total",
                "TCP streams to nameservers the node opened.",
            )?,
        })
    }
}

/// What the worker thread starts with.
struct Start {
    sender: Sender,
    me: p::Pid,
    jobs: mpsc::UnboundedReceiver<Job>,
    opened: mpsc::UnboundedReceiver<(u64, io::Result<Opened>)>,
    config: ResolverConfig,
    options: ResolverOpts,
    hosts: Hosts,
    metrics: Option<Metrics>,
}

impl App for Resolve {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let config = Config::from_args(args)?;
        let (resolver, options) = config.resolver()?;
        let hosts = config.hosts()?;
        let mut sender = bulkhead_native::app::Ctx::current()
            .sender()
            .map_err(|e| format!("sender channel: {e:?}"))?;
        let me = sender.self_pid();
        let metrics = Metrics::register(&mut sender);
        let (jobs, jobs_rx) = mpsc::unbounded_channel();
        let (opened, opened_rx) = mpsc::unbounded_channel();
        let start = Start {
            sender,
            me,
            jobs: jobs_rx,
            opened: opened_rx,
            config: resolver,
            options,
            hosts,
            metrics,
        };
        std::thread::Builder::new()
            .name("resolve".into())
            .spawn(move || worker(start))
            .map_err(|e| format!("worker thread: {e}"))?;
        Ok(Self {
            jobs,
            opened,
            streams: HashMap::new(),
            next_key: 0,
        })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Reply {
        if msg.schema.as_deref() != Some(RESOLVE_SCHEMA) {
            return Reply::Now(Envelope::text("bulkhead-resolve"));
        }
        let answer = match msg.decode::<Request>() {
            Ok(request) => {
                if self
                    .jobs
                    .send(Job {
                        call: call.0,
                        host: request.host,
                    })
                    .is_ok()
                {
                    return Reply::Later;
                }
                Answer::failed(Failure::Temporary, "the resolver's worker stopped".into())
            }
            Err(e) => Answer::failed(Failure::Invalid, format!("request: {e}")),
        };
        Reply::Now(answer_envelope(&answer))
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        match msg.schema.as_deref() {
            Some(CONNECT_SCHEMA) => {
                if let Ok(c) = msg.decode::<Connect>() {
                    let opened = self.connect(c.addr);
                    let _ = self.opened.send((c.id, opened));
                }
            }
            Some(CLOSED_SCHEMA) => {
                if let Ok(c) = msg.decode::<Closed>() {
                    self.streams.remove(&c.key);
                }
            }
            _ => {}
        }
    }
}

impl Resolve {
    /// A stream to `addr` from the node, kept here, and a duplicate for
    /// hickory.
    fn connect(&mut self, addr: SocketAddr) -> io::Result<Opened> {
        let stream = TcpStream::connect(addr).map_err(|e| io::Error::other(e.to_string()))?;
        let clone = stream
            .try_clone_std()
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.next_key += 1;
        let key = self.next_key;
        self.streams.insert(key, stream);
        Ok(Opened { key, stream: clone })
    }
}

fn answer_envelope(answer: &Answer) -> Envelope {
    match Envelope::json(answer) {
        Ok(env) => env.with_schema(RESOLVE_SCHEMA),
        Err(e) => Envelope::text(e.to_string()),
    }
}

fn cast_body<T: Serialize>(schema: &str, value: &T) -> p::Envelope {
    p::Envelope {
        content_type: "application/json".into(),
        schema: Some(schema.into()),
        body: serde_json::to_vec(value).unwrap_or_default(),
    }
}

/// The worker thread: hickory on its own runtime. A sender that cannot
/// become async, or a resolver that cannot be built, ends the process,
/// and the node restarts it.
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
        me,
        mut jobs,
        mut opened,
        config,
        options,
        hosts,
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
    let (wanted_tx, mut wanted) = mpsc::unbounded_channel::<Wanted>();
    let (closed_tx, mut closed) = mpsc::unbounded_channel::<u64>();
    let mut builder = Resolver::builder_with_config(
        config,
        OneStream::new(NodeStreams::new(wanted_tx, closed_tx)),
    );
    *builder.options_mut() = options;
    let mut resolver = match builder.build() {
        Ok(r) => r,
        Err(e) => {
            bulkhead_sdk::error!("resolver: {e}");
            std::process::exit(70);
        }
    };
    let hosts = Arc::new(hosts);
    resolver.set_hosts(hosts.clone());
    let resolver = Rc::new(resolver);

    // Streams hickory wants: asked of the callback thread by id, answered
    // through `opened`.
    let asked: Rc<RefCell<HashMap<u64, oneshot::Sender<io::Result<Opened>>>>> = Rc::default();
    {
        let (sender, asked) = (sender.clone(), asked.clone());
        tokio::task::spawn_local(async move {
            let mut next = 0u64;
            while let Some(w) = wanted.recv().await {
                next += 1;
                asked.borrow_mut().insert(next, w.reply);
                let cast = sender.cast(
                    me,
                    cast_body(
                        CONNECT_SCHEMA,
                        &Connect {
                            id: next,
                            addr: w.addr,
                        },
                    ),
                );
                if let Err(e) = cast.await {
                    if let Some(reply) = asked.borrow_mut().remove(&next) {
                        let _ = reply.send(Err(io::Error::other(format!("{e:?}"))));
                    }
                } else if let Some(m) = metrics {
                    sender.metric_add(m.streams, 1.0);
                }
            }
        });
    }
    {
        let asked = asked.clone();
        tokio::task::spawn_local(async move {
            while let Some((id, result)) = opened.recv().await {
                if let Some(reply) = asked.borrow_mut().remove(&id) {
                    let _ = reply.send(result);
                }
            }
        });
    }
    {
        let sender = sender.clone();
        tokio::task::spawn_local(async move {
            while let Some(key) = closed.recv().await {
                let _ = sender
                    .cast(me, cast_body(CLOSED_SCHEMA, &Closed { key }))
                    .await;
            }
        });
    }
    while let Some(job) = jobs.recv().await {
        let (sender, resolver, hosts) = (sender.clone(), resolver.clone(), hosts.clone());
        tokio::task::spawn_local(async move {
            let answer = resolve(&resolver, &hosts, &job.host).await;
            if let Some(m) = metrics {
                sender.metric_add(m.lookups, 1.0);
                if answer.failure.is_some() {
                    sender.metric_add(m.failed, 1.0);
                }
            }
            sender
                .reply(job.call, answer_envelope(&answer).into())
                .await;
        });
    }
}

/// `host`'s addresses: the hosts file's if it names the host, as libc's
/// `files` source answers before DNS; otherwise through hickory.
async fn resolve(resolver: &Resolver<OneStream>, hosts: &Hosts, host: &str) -> Answer {
    let name = match lookup::name(host) {
        Ok(name) => name,
        Err(e) => return Answer::failed(Failure::Invalid, e),
    };
    if let Some(found) = from_hosts(hosts, &name) {
        return Answer::found(found);
    }
    match resolver.lookup_ip(name).await {
        Ok(found) => Answer::found(found.iter().collect()),
        Err(e) => Answer::failed(lookup::failure(&e), e.to_string()),
    }
}

/// `name`'s addresses in the hosts file, IPv4 and IPv6, if it names it.
fn from_hosts(hosts: &Hosts, name: &Name) -> Option<Vec<std::net::IpAddr>> {
    let mut fqdn = name.clone();
    fqdn.set_fqdn(true);
    let found: Vec<std::net::IpAddr> = [RecordType::A, RecordType::AAAA]
        .into_iter()
        .filter_map(|t| hosts.lookup_static_host(&Query::query(fqdn.clone(), t)))
        .flat_map(|lookup| {
            lookup
                .answers()
                .iter()
                .filter_map(|r| r.data.ip_addr())
                .collect::<Vec<_>>()
        })
        .collect();
    (!found.is_empty()).then_some(found)
}

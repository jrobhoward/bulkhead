//! The driver apps: [`Devices`] (`bulkhead-modbus`, and
//! `bulkhead-modbus-primary` for `primary_only` devices) and [`Server`]
//! (`bulkhead-modbus-server`). Each takes its part of the node config's
//! `[extensions."bulkhead-modbus:modbus"]` as its start arguments (JSON),
//! runs Modbus on a worker thread (a tokio current-thread runtime with a
//! sender channel of its own), casts updates to `deliver_to` apps
//! (`Request::CastApp`), and answers the node's forwarded calls
//! ([`MODBUS_SCHEMA`]: the CBOR pair `(function, args)`, answered with the
//! function's result).

use std::collections::BTreeMap;
use std::sync::Arc;

use bulkhead_native::protocol::Envelope as WireEnvelope;
use bulkhead_native::sender::Sender;
use bulkhead_native::sender_async::AsyncSender;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::points::Update;
use crate::server::Served;

/// The schema of the node's calls and the drivers' answers.
pub const MODBUS_SCHEMA: &str = "bulkhead/modbus";

/// A forwarded call for the worker: the call to answer, the function and
/// its arguments.
struct Job {
    call: u64,
    function: String,
    args: Vec<u8>,
}

/// A forwarded call's function and arguments, if `msg` is one.
fn forwarded(msg: &Envelope) -> Option<(String, Vec<u8>)> {
    if msg.schema.as_deref() != Some(MODBUS_SCHEMA) {
        return None;
    }
    let (function, args): (String, ciborium::Value) =
        ciborium::from_reader(msg.body.as_slice()).ok()?;
    match args {
        ciborium::Value::Bytes(b) => Some((function, b)),
        _ => None,
    }
}

/// `result` (CBOR) as the answer to a forwarded call.
fn answer(result: Vec<u8>) -> WireEnvelope {
    let mut env = WireEnvelope::new("application/cbor", result);
    env.schema = Some(MODBUS_SCHEMA.to_string());
    env
}

/// The configuration in the start arguments.
fn config(args: &[u8]) -> Result<Config, String> {
    let value: serde_json::Value = if args.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(args).map_err(|e| format!("start arguments: {e}"))?
    };
    Config::from_json(&value)
}

/// A sender channel for the worker thread.
fn sender() -> Result<Sender, String> {
    bulkhead_native::app::Ctx::current()
        .sender()
        .map_err(|e| format!("sender channel: {e:?}"))
}

/// Run `body` on a new worker thread: a current-thread tokio runtime and a
/// `LocalSet`, the sender made async there. A worker that cannot start
/// ends the process, and the node restarts it.
fn worker<F, Fut>(name: &str, sender: Sender, body: F) -> Result<(), String>
where
    F: FnOnce(AsyncSender) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
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
                body(sender).await;
            });
        })
        .map(|_| ())
        .map_err(|e| format!("worker thread: {e}"))
}

/// The devices' driver.
#[derive(Debug)]
pub struct Devices {
    jobs: mpsc::UnboundedSender<Job>,
}

impl App for Devices {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let config = config(args)?;
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
        worker("modbus", sender()?, move |sender| async move {
            let devices: BTreeMap<_, _> = config
                .devices
                .into_iter()
                .map(|(name, device)| {
                    let handle = crate::device::spawn(name.clone(), device, sender.clone());
                    (name, handle)
                })
                .collect();
            let devices = std::rc::Rc::new(devices);
            while let Some(job) = queue.recv().await {
                let (sender, devices) = (sender.clone(), devices.clone());
                tokio::task::spawn_local(async move {
                    let result = crate::calls::client(&devices, &job.function, &job.args).await;
                    sender.reply(job.call, answer(result)).await;
                });
            }
        })?;
        Ok(Self { jobs })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, call: CallRef, msg: Envelope) -> Reply {
        let Some((function, args)) = forwarded(&msg) else {
            return Reply::Now(Envelope::text("bulkhead-modbus"));
        };
        let job = Job {
            call: call.0,
            function,
            args,
        };
        match self.jobs.send(job) {
            Ok(()) => Reply::Later,
            Err(_) => Reply::Now(Envelope::text("the driver's worker stopped")),
        }
    }
}

/// The server's driver.
#[derive(Debug)]
pub struct Server {
    served: Arc<Served>,
}

/// The name of the listening socket the node passes the server.
pub const LISTENER: &str = "modbus";

impl App for Server {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let config = config(args)?;
        let server = config.server.ok_or("no server in the start arguments")?;
        let listener = bulkhead_native::driver::listeners()
            .into_iter()
            .find(|(name, _)| name == LISTENER)
            .map(|(_, fd)| std::net::TcpListener::from(fd))
            .ok_or_else(|| format!("no listening socket named {LISTENER}"))?;
        let served = Arc::new(Served::new(&server));
        let tables = served.clone();
        worker("modbus-server", sender()?, move |sender| async move {
            let (updates, mut written) = mpsc::unbounded_channel::<(String, Update)>();
            tokio::task::spawn_local(async move {
                while let Some((app, update)) = written.recv().await {
                    let msg = WireEnvelope::new("application/cbor", update.to_cbor());
                    let _ = sender.cast_app(&app, msg).await;
                }
            });
            if let Err(e) = crate::server::serve(listener, server, tables, updates).await {
                bulkhead_sdk::error!("{e}");
                std::process::exit(70);
            }
        })?;
        Ok(Self { served })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        match forwarded(&msg) {
            Some((function, args)) => {
                let result = crate::calls::server(&self.served, &function, &args);
                Reply::Now(Envelope::from(answer(result)))
            }
            None => Reply::Now(Envelope::text("bulkhead-modbus-server")),
        }
    }
}

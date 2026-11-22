//! One device: a task that owns its connection, polls it, and runs guests'
//! requests between polls.
//!
//! The connection is opened on first use and dropped on any transport
//! error or timeout, so the next poll or request reconnects. A poll reads
//! every configured range; if any read fails the device counts as offline
//! (one `offline` message per outage), and the next good poll sends every
//! point. A failed delivery (the app is not running, or its mailbox is
//! full) also makes the next poll send every point, so an app that starts
//! late or drops an update catches up.
//!
//! The connection is the node's to open (`Request::ConnectTo`, to the
//! device's configured address alone), passed to the driver, and Modbus
//! runs over it here. `primary_only` devices are polled by the driver the
//! node runs as a singleton (`bulkhead-modbus-primary`), so only while it
//! holds the failover lease; this module polls whatever it is given.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use bulkhead_native::protocol::Envelope;
use bulkhead_native::sender_async::AsyncSender;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{MissedTickBehavior, timeout};
use tokio_modbus::client::{Context, Reader, Writer};
use tokio_modbus::prelude::Slave;

use crate::config::DeviceConfig;
use crate::points::{
    Kind, Scan, Table, TagValue, Update, changes, covered, record_bits, record_words, tag_changes,
};

/// A request to a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    ReadWords(Table, u16, u16),
    ReadBits(Table, u16, u16),
    WriteWords(u16, Vec<u16>),
    WriteBits(u16, Vec<bool>),
}

/// A request's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    Words(Vec<u16>),
    Bits(Vec<bool>),
    Done,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeviceError {
    /// Not connected, timed out, or the connection failed.
    Offline(String),
    /// The device answered with this exception code.
    Exception(u8),
}

type Job = (Request, oneshot::Sender<Result<Reply, DeviceError>>);

/// Sends requests to a device's task. Cheap to clone.
#[derive(Debug, Clone)]
pub(crate) struct DeviceHandle {
    jobs: mpsc::Sender<Job>,
}

impl DeviceHandle {
    /// Run `request` on the device, after any poll or request in progress.
    pub(crate) async fn request(&self, request: Request) -> Result<Reply, DeviceError> {
        let (reply, answer) = oneshot::channel();
        let gone = || DeviceError::Offline("the node is stopping".into());
        self.jobs.send((request, reply)).await.map_err(|_| gone())?;
        answer.await.map_err(|_| gone())?
    }
}

/// A device that answers each request with `answer`, and records the
/// requests in `seen`: for tests of what the guest interface sends.
#[cfg(test)]
pub(crate) fn answering(
    answer: impl Fn(&Request) -> Result<Reply, DeviceError> + Send + 'static,
    seen: std::sync::Arc<std::sync::Mutex<Vec<Request>>>,
) -> DeviceHandle {
    let (jobs, mut queue) = mpsc::channel::<Job>(64);
    tokio::spawn(async move {
        while let Some((request, reply)) = queue.recv().await {
            let result = answer(&request);
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            let _ = reply.send(result);
        }
    });
    DeviceHandle { jobs }
}

/// Start the task for device `name` on the current `LocalSet`, asking the
/// node through `sender`.
pub(crate) fn spawn(name: String, config: DeviceConfig, sender: AsyncSender) -> DeviceHandle {
    let (jobs, queue) = mpsc::channel(64);
    let device = Device {
        name,
        sender,
        ctx: None,
        covered: covered(&config.tags),
        config,
        last: Scan::new(),
        tags_sent: BTreeMap::new(),
        online: None,
        resend: false,
        last_full: Instant::now(),
    };
    tokio::task::spawn_local(device.run(queue));
    DeviceHandle { jobs }
}

struct Device {
    name: String,
    config: DeviceConfig,
    sender: AsyncSender,
    ctx: Option<Context>,
    /// The last scan delivered.
    last: Scan,
    /// The registers and bits the tags cover: sent as tags, not points.
    covered: BTreeSet<(Table, u16)>,
    /// Each tag's value as last sent, for its deadband.
    tags_sent: BTreeMap<String, TagValue>,
    /// Whether the last poll succeeded; `None` before the first.
    online: Option<bool>,
    /// The next poll sends every point (a delivery failed).
    resend: bool,
    last_full: Instant,
}

impl Device {
    async fn run(mut self, mut queue: mpsc::Receiver<Job>) {
        let polls = self.config.deliver_to.is_some()
            && [
                &self.config.holding,
                &self.config.input,
                &self.config.coils,
                &self.config.discrete,
            ]
            .iter()
            .any(|r| !r.is_empty());
        let mut tick = tokio::time::interval(self.config.poll());
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                job = queue.recv() => {
                    let Some((request, reply)) = job else { break };
                    let result = self.request(request).await;
                    let _ = reply.send(result);
                }
                _ = tick.tick(), if polls => self.poll().await,
            }
        }
    }

    async fn connected(&mut self) -> Result<&mut Context, DeviceError> {
        if self.ctx.is_none() {
            let address = self.config.address;
            let host = address.ip().to_string();
            let connect = self.sender.connect_to(&host, address.port());
            let fd = timeout(self.config.timeout(), connect)
                .await
                .map_err(|_| DeviceError::Offline("connect timed out".into()))?
                .map_err(|e| DeviceError::Offline(format!("connect: {e:?}")))?;
            let stream = std::net::TcpStream::from(fd);
            stream
                .set_nonblocking(true)
                .map_err(|e| DeviceError::Offline(format!("connect: {e}")))?;
            let stream = tokio::net::TcpStream::from_std(stream)
                .map_err(|e| DeviceError::Offline(format!("connect: {e}")))?;
            self.ctx = Some(tokio_modbus::client::tcp::attach_slave(
                stream,
                Slave(self.config.unit),
            ));
        }
        self.ctx
            .as_mut()
            .ok_or_else(|| DeviceError::Offline("not connected".into()))
    }

    async fn request(&mut self, request: Request) -> Result<Reply, DeviceError> {
        let limit = self.config.timeout();
        let ctx = self.connected().await?;
        let result = match request {
            Request::ReadWords(table, address, count) => {
                let read = async {
                    match table {
                        Table::Input => ctx.read_input_registers(address, count).await,
                        _ => ctx.read_holding_registers(address, count).await,
                    }
                };
                answer(timeout(limit, read).await, Reply::Words)
            }
            Request::ReadBits(table, address, count) => {
                let read = async {
                    match table {
                        Table::Discrete => ctx.read_discrete_inputs(address, count).await,
                        _ => ctx.read_coils(address, count).await,
                    }
                };
                answer(timeout(limit, read).await, Reply::Bits)
            }
            Request::WriteWords(address, words) => answer(
                timeout(limit, ctx.write_multiple_registers(address, &words)).await,
                |()| Reply::Done,
            ),
            Request::WriteBits(address, bits) => answer(
                timeout(limit, ctx.write_multiple_coils(address, &bits)).await,
                |()| Reply::Done,
            ),
        };
        if matches!(result, Err(DeviceError::Offline(_))) {
            // Reconnect next time.
            self.ctx = None;
        }
        result
    }

    /// Read every configured range into one scan.
    async fn scan(&mut self) -> Result<Scan, DeviceError> {
        let mut scan = Scan::new();
        let words = [
            (Table::Holding, self.config.holding.clone()),
            (Table::Input, self.config.input.clone()),
        ];
        for (table, ranges) in words {
            for r in ranges {
                if let Reply::Words(w) = self
                    .request(Request::ReadWords(table, r.start, r.count))
                    .await?
                {
                    record_words(&mut scan, table, r.start, &w);
                }
            }
        }
        let bits = [
            (Table::Coil, self.config.coils.clone()),
            (Table::Discrete, self.config.discrete.clone()),
        ];
        for (table, ranges) in bits {
            for r in ranges {
                if let Reply::Bits(b) = self
                    .request(Request::ReadBits(table, r.start, r.count))
                    .await?
                {
                    // A device may pad bits to a whole byte.
                    let b = &b[..b.len().min(usize::from(r.count))];
                    record_bits(&mut scan, table, r.start, b);
                }
            }
        }
        Ok(scan)
    }

    async fn poll(&mut self) {
        let Some(app) = self.config.deliver_to.clone() else {
            return;
        };
        let scan = match self.scan().await {
            Ok(scan) => scan,
            Err(e) => {
                if self.online != Some(false) {
                    let error = match e {
                        DeviceError::Offline(why) => why,
                        DeviceError::Exception(code) => format!("exception {code}"),
                    };
                    bulkhead_sdk::warn!("modbus device {} offline: {error}", self.name);
                    let update = Update {
                        device: self.name.clone(),
                        kind: Kind::Offline,
                        full: false,
                        points: Vec::new(),
                        error: Some(error),
                        tags: Vec::new(),
                    };
                    // An offline notice that cannot be delivered is
                    // retried on the next failed poll.
                    if self.deliver(&app, &update).await {
                        self.online = Some(false);
                    }
                }
                self.last.clear();
                self.tags_sent.clear();
                return;
            }
        };
        let integrity = self
            .config
            .full_every_ms
            .is_some_and(|ms| self.last_full.elapsed() >= Duration::from_millis(ms));
        let full = self.online != Some(true) || self.resend || integrity;
        let mut points = changes(&self.last, &scan, full);
        points.retain(|p| !self.covered.contains(&(p.table, p.address)));
        let tags = tag_changes(&self.config.tags, &mut self.tags_sent, &scan, full);
        if self.online == Some(false) {
            bulkhead_sdk::info!("modbus device {} online", self.name);
        }
        self.online = Some(true);
        self.last = scan;
        if full {
            self.last_full = Instant::now();
        }
        if points.is_empty() && tags.is_empty() && !full {
            return;
        }
        let update = Update {
            device: self.name.clone(),
            kind: Kind::Points,
            full,
            points,
            error: None,
            tags,
        };
        self.resend = !self.deliver(&app, &update).await;
    }

    /// Cast `update` to `app`; whether it was taken.
    async fn deliver(&self, app: &str, update: &Update) -> bool {
        let msg = Envelope::new("application/cbor", update.to_cbor());
        self.sender.cast_app(app, msg).await.is_ok()
    }
}

/// Turn a timed request's result into a reply.
fn answer<T>(
    result: Result<tokio_modbus::Result<T>, tokio::time::error::Elapsed>,
    reply: impl FnOnce(T) -> Reply,
) -> Result<Reply, DeviceError> {
    match result {
        Err(_) => Err(DeviceError::Offline("timed out".into())),
        Ok(Err(e)) => Err(DeviceError::Offline(e.to_string())),
        Ok(Ok(Err(code))) => Err(DeviceError::Exception(code.into())),
        Ok(Ok(Ok(value))) => Ok(reply(value)),
    }
}

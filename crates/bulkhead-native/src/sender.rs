//! A channel of its own for one of the app's threads (ROADMAP AP1, spike
//! S39): a [`Sender`] asks the node from any thread, at any time, not only
//! from the callback thread during a callback.
//!
//! The callback thread opens one with [`Ctx::sender`](crate::app::Ctx::sender),
//! and a thread holding one opens more with [`Sender::try_clone`]. Each is
//! its own pair of rings with its own door and bell, used by one thread at
//! a time (`Send`, not `Sync`), so a request costs what a callback's does
//! and no thread waits on another's. The node serves each sender with a
//! task of its own, as the instance (its pid, its messaging ACLs, its
//! grants), never behind the callback that is running.
//!
//! A sender asks for messaging (casts and calls), keyvalue (not watches)
//! and metrics. Calls may be in flight together: [`Sender::start_call`]
//! sends one and [`Sender::next_answer`] takes the answers as they come,
//! up to 64 on one sender before the node reads no more of its requests.
//! Metric updates go at once, one frame each, with no answer. Dropping a
//! sender closes its end of the bell, which ends the node's task for it
//! and gives its place under the app's `senders_max` back.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::marker::PhantomData;

use crate::app::{Ctx, take_fd};
use crate::channel::{AppEnd, FromNode, Spin, ToNode};
use crate::hot::{KvFrame, KvOp, KvReply};
use crate::protocol::{
    self, Answer, BlobAnswer, BlobError, BlobRequest, Bytes, CallError, CastError, Envelope,
    FromApp, KvAnswer, KvError, KvRequest, MetricError, MetricKind, MetricUpdate, NetError, Pid,
    Request, SenderChannel, SenderError, ToApp,
};

/// One thread's channel to the node.
#[derive(Debug)]
pub struct Sender {
    pub(crate) end: AppEnd,
    next_id: u64,
    /// Bucket handles the node gave this sender, by bucket name.
    handles: HashMap<String, u32>,
    /// Calls started and not yet taken by [`Sender::next_answer`], and the
    /// answers that came for them while a request waited.
    started: HashMap<u64, Option<Result<Envelope, CallError>>>,
    out: Vec<u8>,
    inbuf: Vec<u8>,
    /// Frames written only in part, the first from `queue_at`, while
    /// writes do not block ([`Sender::set_queued`]).
    queue: VecDeque<Vec<u8>>,
    queue_at: usize,
    /// Writes queue what does not fit instead of waiting for room.
    queued: bool,
    /// Not `Sync`: one thread at a time.
    _one_thread: PhantomData<std::cell::Cell<()>>,
}

/// What came from the node.
enum Got {
    Frame(ToApp),
    /// A hot keyvalue answer, in `inbuf`.
    Hot,
}

impl Sender {
    /// The sender the node made, from the answer naming its descriptors.
    pub(crate) fn from_channel(passed: SenderChannel) -> Result<Self, SenderError> {
        let other = |e: io::Error| SenderError::Other(format!("sender channel: {e}"));
        let memfd = take_fd(passed.memfd).map_err(other)?;
        let door = take_fd(passed.door).map_err(other)?;
        let bell = take_fd(passed.bell).map_err(other)?;
        let end = AppEnd::sender(memfd, bell, door, Spin::sender_from_env()).map_err(other)?;
        Ok(Self {
            end,
            next_id: 0,
            handles: HashMap::new(),
            started: HashMap::new(),
            out: Vec::new(),
            inbuf: Vec::new(),
            queue: VecDeque::new(),
            queue_at: 0,
            queued: false,
            _one_thread: PhantomData,
        })
    }

    /// A sender on `end`, for tests that play the node.
    #[cfg(test)]
    pub(crate) fn for_tests(end: AppEnd) -> Self {
        Self {
            end,
            next_id: 0,
            handles: HashMap::new(),
            started: HashMap::new(),
            out: Vec::new(),
            inbuf: Vec::new(),
            queue: VecDeque::new(),
            queue_at: 0,
            queued: false,
            _one_thread: PhantomData,
        }
    }

    /// Another sender, for another thread, asked of the node through this
    /// one.
    ///
    /// # Errors
    ///
    /// [`SenderError::LimitReached`] at the app's `senders_max`, or why
    /// the channel could not be made.
    pub fn try_clone(&mut self) -> Result<Self, SenderError> {
        match self.request(Request::OpenSender) {
            Answer::Sender(result) => Self::from_channel(result?),
            other => unexpected(&other),
        }
    }

    /// A stream into the node's admin API (ROADMAP DA7), for a driver
    /// whose profile has `admin`: each request on it must carry an
    /// operator's signature.
    ///
    /// # Errors
    ///
    /// `denied` without the profile's flag, or why the node could not
    /// make or pass it.
    pub fn admin_stream(&mut self) -> Result<std::os::fd::OwnedFd, NetError> {
        match self.request(Request::AdminStream) {
            Answer::AdminStream(result) => {
                take_fd(result?).map_err(|e| NetError::Other(e.to_string()))
            }
            other => unexpected(&other),
        }
    }

    /// A connection to the endpoint the driver's profile names (ROADMAP
    /// DA3), which the node opened.
    ///
    /// # Errors
    ///
    /// `denied` without an endpoint, or why the node could not connect.
    pub fn connect(&mut self) -> Result<std::os::fd::OwnedFd, NetError> {
        match self.request(Request::Connect) {
            Answer::Connected(result) => {
                take_fd(result?).map_err(|e| NetError::Other(e.to_string()))
            }
            other => unexpected(&other),
        }
    }

    /// A connection to `host` and `port` for the app an egress driver
    /// serves (ROADMAP DA4), which the node opened.
    ///
    /// # Errors
    ///
    /// `denied` for a destination that app may not reach or a driver that
    /// serves none, or why the node could not connect.
    pub fn connect_to(&mut self, host: &str, port: u16) -> Result<std::os::fd::OwnedFd, NetError> {
        let request = Request::ConnectTo {
            host: host.to_string(),
            port,
        };
        match self.request(request) {
            Answer::Connected(result) => {
                take_fd(result?).map_err(|e| NetError::Other(e.to_string()))
            }
            other => unexpected(&other),
        }
    }

    /// A blob request (`bulkhead:blobs`, ROADMAP BL1). A descriptor the
    /// answer names ([`BlobAnswer::Created`], [`BlobAnswer::Opened`]) is
    /// taken with `app::take_fd`.
    ///
    /// # Errors
    ///
    /// As [`BlobError`] describes.
    pub fn blob(&mut self, request: BlobRequest) -> Result<BlobAnswer, BlobError> {
        match self.request(Request::Blob(request)) {
            Answer::Blob(result) => result,
            other => unexpected(&other),
        }
    }

    /// The node's metrics in the Prometheus text format (ROADMAP DA7), for
    /// a driver whose profile has `metrics`.
    ///
    /// # Errors
    ///
    /// `denied` without the profile's flag.
    pub fn metrics_text(&mut self) -> Result<String, NetError> {
        match self.request(Request::MetricsText) {
            Answer::MetricsText(result) => result,
            other => unexpected(&other),
        }
    }

    /// Answer the call `call_ref` a callback of this instance left
    /// waiting (`Reply::Later`), from this thread (ROADMAP AP1b). A second
    /// answer, or one after the caller gave up, is dropped.
    pub fn reply(&mut self, call_ref: u64, msg: Envelope) {
        match self.request(Request::Reply { call_ref, msg }) {
            Answer::Unit => {}
            other => unexpected(&other),
        }
    }

    /// This instance's pid.
    pub fn self_pid(&mut self) -> Pid {
        match self.request(Request::SelfPid) {
            Answer::Pid(pid) => pid,
            other => unexpected(&other),
        }
    }

    /// Send `msg` to `to` without waiting; dropped if `to` is gone.
    pub fn cast(&mut self, to: Pid, msg: Envelope) {
        let _ = self.try_cast(to, msg);
    }

    /// Cast to the app called `app` on this node, the node picking the
    /// instance by its routing (ROADMAP DA5), saying whether the message
    /// was queued (`noproc` when the app does not run here).
    ///
    /// # Errors
    ///
    /// The [`CastError`] saying why the message was dropped.
    pub fn try_cast_app(&mut self, app: &str, msg: Envelope) -> Result<(), CastError> {
        match self.request(Request::CastApp {
            app: app.to_string(),
            msg,
        }) {
            Answer::Unit => Ok(()),
            Answer::CastFailed(e) => Err(e),
            other => unexpected(&other),
        }
    }

    /// [`Sender::cast`], saying whether the message was queued.
    ///
    /// # Errors
    ///
    /// The [`CastError`] saying why the message was dropped.
    pub fn try_cast(&mut self, to: Pid, msg: Envelope) -> Result<(), CastError> {
        match self.request(Request::Cast { to, msg }) {
            Answer::Unit => Ok(()),
            Answer::CastFailed(e) => Err(e),
            other => unexpected(&other),
        }
    }

    /// [`Sender::try_cast`] that waits up to `timeout_ms` for room if
    /// `to`'s mailbox is full. Only this sender waits: the callbacks and
    /// the app's other senders go on.
    ///
    /// # Errors
    ///
    /// The [`CastError`] saying why the message was not queued.
    pub fn cast_timeout(
        &mut self,
        to: Pid,
        msg: Envelope,
        timeout_ms: u32,
    ) -> Result<(), CastError> {
        match self.request(Request::CastWait {
            to,
            msg,
            timeout_ms,
        }) {
            Answer::Unit => Ok(()),
            Answer::CastFailed(e) => Err(e),
            other => unexpected(&other),
        }
    }

    /// Call `to` and wait up to `timeout_ms` for its reply. A call to this
    /// instance's own pid is answered by its callbacks, on the callback
    /// thread.
    ///
    /// # Errors
    ///
    /// The [`CallError`] if there is no reply.
    pub fn call(&mut self, to: Pid, msg: Envelope, timeout_ms: u32) -> Result<Envelope, CallError> {
        match self.request(Request::Call {
            to,
            msg,
            timeout_ms,
        }) {
            Answer::Called(result) => result,
            other => unexpected(&other),
        }
    }

    /// Call the app called `app` on this node, as
    /// [`Ctx::call_app`](crate::app::Ctx::call_app) does.
    ///
    /// # Errors
    ///
    /// The [`CallError`] if there is no reply.
    pub fn call_app(
        &mut self,
        app: impl Into<String>,
        msg: Envelope,
        timeout_ms: u32,
    ) -> Result<Envelope, CallError> {
        match self.request(Request::CallApp {
            app: app.into(),
            msg,
            timeout_ms,
            partner: false,
        }) {
            Answer::Called(result) => result,
            other => unexpected(&other),
        }
    }

    /// Start a call to the app called `app` without waiting for its
    /// reply: its id, for [`Sender::next_answer`].
    pub fn start_call_app(
        &mut self,
        app: impl Into<String>,
        msg: Envelope,
        timeout_ms: u32,
    ) -> u64 {
        let (id, refused) = self.send_request_checked(Request::CallApp {
            app: app.into(),
            msg,
            timeout_ms,
            partner: false,
        });
        self.started.insert(id, called(refused));
        id
    }

    /// Start a call without waiting for its reply: its id, for
    /// [`Sender::next_answer`].
    pub fn start_call(&mut self, to: Pid, msg: Envelope, timeout_ms: u32) -> u64 {
        let (id, refused) = self.send_request_checked(Request::Call {
            to,
            msg,
            timeout_ms,
        });
        self.started.insert(id, called(refused));
        id
    }

    /// Calls started and not yet answered by [`Sender::next_answer`].
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.started.len()
    }

    /// The next started call to be answered, in the order the answers
    /// come, waiting for one; `None` when no call is in flight.
    pub fn next_answer(&mut self) -> Option<(u64, Result<Envelope, CallError>)> {
        if self.started.is_empty() {
            return None;
        }
        let ready = self
            .started
            .iter()
            .find_map(|(id, answer)| answer.is_some().then_some(*id));
        if let Some(id) = ready {
            let answer = self.started.remove(&id).flatten()?;
            return Some((id, answer));
        }
        // Nothing else waits on this sender while it is here, so the next
        // frame answers a started call.
        match self.read() {
            Got::Frame(ToApp::Answer {
                id,
                answer: Answer::Called(result),
            }) if self.started.contains_key(&id) => {
                self.started.remove(&id);
                Some((id, result))
            }
            Got::Frame(other) => unexpected_frame(&other),
            Got::Hot => protocol_error("a hot frame with no request waiting"),
        }
    }

    /// One keyvalue request as the protocol has it. Watches are the
    /// callback thread's: a sender's `watch` is refused.
    ///
    /// # Errors
    ///
    /// The [`KvError`] the node answers.
    pub fn kv(&mut self, request: KvRequest) -> Result<KvAnswer, KvError> {
        let hot = match &request {
            KvRequest::Get { bucket, key } => Some((bucket, KvOp::Get, key, &[][..])),
            KvRequest::Set { bucket, key, value } => Some((bucket, KvOp::Set, key, &value.0[..])),
            KvRequest::Delete { bucket, key } => Some((bucket, KvOp::Delete, key, &[][..])),
            KvRequest::Exists { bucket, key } => Some((bucket, KvOp::Exists, key, &[][..])),
            _ => None,
        };
        if let Some((bucket, op, key, value)) = hot {
            return self.kv_op(bucket, op, key, value);
        }
        match self.request(Request::Kv(request)) {
            Answer::Kv(result) => result,
            other => unexpected(&other),
        }
    }

    /// A `get`, `set`, `delete` or `exists`: one hot frame on the
    /// bucket's handle (opened on first use), or the CBOR request for a key
    /// or value too long for the hot layout. Reads go to the node, not to a
    /// published bucket's snapshot.
    ///
    /// # Errors
    ///
    /// The [`KvError`] the node answers.
    pub fn kv_op(
        &mut self,
        bucket: &str,
        op: KvOp,
        key: &str,
        value: &[u8],
    ) -> Result<KvAnswer, KvError> {
        let handle = self.handle(bucket)?;
        self.next_id += 1;
        let id = self.next_id;
        let frame = KvFrame {
            id,
            bucket: handle,
            op,
            key,
            value,
        };
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        let written = crate::hot::write_kv_request(&mut out, &frame).is_some();
        if written {
            self.write(&out);
        }
        self.out = out;
        if !written {
            return match self.request(Request::Kv(cold_request(bucket, op, key, value))) {
                Answer::Kv(result) => result,
                other => unexpected(&other),
            };
        }
        loop {
            match self.read() {
                Got::Hot => {
                    return match crate::hot::parse_kv_answer(&self.inbuf) {
                        Ok((got, reply)) if got == id => Ok(match reply {
                            KvReply::Value(v) => KvAnswer::Value(v.map(Bytes::from)),
                            KvReply::Unit => KvAnswer::Unit,
                            KvReply::Exists(e) => KvAnswer::Exists(e),
                        }),
                        Ok((got, _)) => protocol_error(&format!("expected answer {id}, got {got}")),
                        Err(e) => protocol_error(&e.to_string()),
                    };
                }
                Got::Frame(ToApp::Answer {
                    id: got,
                    answer: Answer::Kv(result),
                }) if got == id => return result,
                Got::Frame(frame) => self.early(frame),
            }
        }
    }

    /// Register a metric (`bulkhead:metrics`) for this sender's updates:
    /// the same series as the callbacks' by name, with an id of this
    /// sender's own.
    ///
    /// # Errors
    ///
    /// As [`Ctx::register_metric`].
    pub fn register_metric(
        &mut self,
        kind: MetricKind,
        name: impl Into<String>,
        help: impl Into<String>,
        labels: &[(&str, &str)],
        buckets: &[f64],
    ) -> Result<u32, MetricError> {
        match self.request(Request::MetricRegister {
            kind,
            name: name.into(),
            help: help.into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            buckets: buckets.to_vec(),
        }) {
            Answer::Metric(result) => result,
            other => unexpected(&other),
        }
    }

    /// Add to a counter or gauge this sender registered. Sent at once, with
    /// no answer; the node logs one it refuses.
    pub fn metric_add(&mut self, metric: u32, delta: f64) {
        self.send(&FromApp::Metrics(vec![MetricUpdate::Add(metric, delta)]));
    }

    /// Set a gauge, as [`Sender::metric_add`] sends.
    pub fn metric_set(&mut self, metric: u32, value: f64) {
        self.send(&FromApp::Metrics(vec![MetricUpdate::Set(metric, value)]));
    }

    /// Record a histogram observation, as [`Sender::metric_add`] sends.
    pub fn metric_observe(&mut self, metric: u32, value: f64) {
        self.send(&FromApp::Metrics(vec![MetricUpdate::Observe(
            metric, value,
        )]));
    }

    /// Send `request` without waiting for an answer: for tests of what
    /// the node refuses on a sender channel only.
    #[doc(hidden)]
    pub fn send_for_tests(&mut self, request: Request) {
        self.send_request(request);
    }

    fn handle(&mut self, bucket: &str) -> Result<u32, KvError> {
        if let Some(h) = self.handles.get(bucket) {
            return Ok(*h);
        }
        match self.request(Request::KvOpen(bucket.to_string())) {
            Answer::KvOpened(Ok(h)) => {
                self.handles.insert(bucket.to_string(), h);
                Ok(h)
            }
            Answer::KvOpened(Err(e)) => Err(e),
            other => unexpected(&other),
        }
    }

    /// Ask and wait for the answer; answers to started calls that come
    /// first are kept for [`Sender::next_answer`].
    fn request(&mut self, request: Request) -> Answer {
        let (id, refused) = self.send_request_checked(request);
        if let Some(answer) = refused {
            return answer;
        }
        loop {
            match self.read() {
                Got::Frame(ToApp::Answer { id: got, answer }) if got == id => return answer,
                Got::Frame(frame) => self.early(frame),
                Got::Hot => protocol_error("a hot frame with no hot request waiting"),
            }
        }
    }

    /// Keep a started call's answer that came while another request
    /// waited; anything else breaks the protocol.
    fn early(&mut self, frame: ToApp) {
        match frame {
            ToApp::Answer {
                id,
                answer: Answer::Called(result),
            } if self.started.contains_key(&id) => {
                self.started.insert(id, Some(result));
            }
            other => unexpected_frame(&other),
        }
    }

    pub(crate) fn send_request(&mut self, request: Request) -> u64 {
        self.send_request_checked(request).0
    }

    /// Send `request`, or, for one whose frame is over the limit, keep it
    /// and give the answer it gets instead ([`protocol::too_large`]).
    pub(crate) fn send_request_checked(&mut self, request: Request) -> (u64, Option<Answer>) {
        self.next_id += 1;
        let id = self.next_id;
        let frame = FromApp::Request { id, request };
        if let Err(len) = self.try_send(&frame, true) {
            if let FromApp::Request { request, .. } = &frame
                && let Some(answer) = protocol::too_large(request, len)
            {
                return (id, Some(answer));
            }
            // No error to give: sent, and the node ends the instance.
            self.send(&frame);
        }
        (id, None)
    }

    fn send(&mut self, frame: &FromApp) {
        let _ = self.try_send(frame, false);
    }

    fn try_send(&mut self, frame: &FromApp, refuse_over_limit: bool) -> Result<(), usize> {
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        let mut result = Ok(());
        if crate::hot::write_from_app(&mut out, frame).is_some() {
            self.write(&out);
        } else {
            let bytes = protocol::encode_sized(frame, frame.size_hint());
            if refuse_over_limit && protocol::over_limit(&bytes) {
                result = Err(bytes.len() - 4);
            } else {
                self.write_owned(bytes);
            }
        }
        self.out = out;
        result
    }

    /// Write `bytes`, a buffer the caller keeps: blocking until there is
    /// room, or queued in a copy of what does not fit.
    fn write(&mut self, bytes: &[u8]) {
        if !self.queued {
            self.write_blocking(bytes);
        } else if !self.queue.is_empty() {
            self.queue.push_back(bytes.to_vec());
        } else {
            let written = self.try_write(bytes);
            if written < bytes.len() {
                self.queue.push_back(bytes[written..].to_vec());
                self.queue_at = 0;
            }
        }
    }

    /// Write `bytes`, an encoded frame: as [`Sender::write`], what does
    /// not fit queued in the same buffer.
    fn write_owned(&mut self, bytes: Vec<u8>) {
        if !self.queued {
            self.write_blocking(&bytes);
        } else if !self.queue.is_empty() {
            self.queue.push_back(bytes);
        } else {
            let written = self.try_write(&bytes);
            if written < bytes.len() {
                self.queue.push_back(bytes);
                self.queue_at = written;
            }
        }
    }

    fn try_write(&self, bytes: &[u8]) -> usize {
        self.end.try_write_to_node(bytes).unwrap_or_else(|_| {
            // The node broke the ring; it ends this process.
            gone()
        })
    }

    fn write_blocking(&self, bytes: &[u8]) {
        if ToNode(&self.end).write_all(bytes).is_err() {
            // The node broke the ring; it ends this process.
            gone();
        }
    }

    /// Have writes queue what does not fit in the ring instead of waiting
    /// for room, for a thread that must not block (`sender_async`, ROADMAP
    /// PF1): [`Sender::flush_queue`] writes it as room comes. Only where
    /// the node wakes a writer through the door
    /// ([`AppEnd::door_wakes_writer`]); whether it did.
    pub(crate) fn set_queued(&mut self) -> bool {
        self.queued = self.end.door_wakes_writer();
        self.queued
    }

    /// Write what is queued, as far as it fits; whether anything is left.
    pub(crate) fn flush_queue(&mut self) -> bool {
        while let Some(front) = self.queue.front() {
            let written = self.try_write(&front[self.queue_at..]);
            if written == 0 {
                return true;
            }
            self.queue_at += written;
            if self.queue_at == front.len() {
                self.queue.pop_front();
                self.queue_at = 0;
            }
        }
        false
    }

    /// The next frame from the node; its end means the node stopped this
    /// instance, and the process exits.
    fn read(&mut self) -> Got {
        let mut prefix = [0u8; 4];
        match FromNode(&self.end).read_exact(&mut prefix) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => gone(),
            Err(e) => protocol_error(&e.to_string()),
        }
        let len = protocol::frame_len(prefix).unwrap_or_else(|e| protocol_error(&e.to_string()));
        self.inbuf.resize(len, 0);
        if let Err(e) = FromNode(&self.end).read_exact(&mut self.inbuf) {
            protocol_error(&e.to_string());
        }
        if crate::hot::is_hot(&self.inbuf) {
            return match crate::hot::parse_to_app(&self.inbuf) {
                Ok(crate::hot::ToAppHot::Kv(..)) => Got::Hot,
                Ok(crate::hot::ToAppHot::Frame(frame)) => Got::Frame(frame),
                Err(e) => protocol_error(&e.to_string()),
            };
        }
        match protocol::decode::<ToApp>(&self.inbuf) {
            Ok(frame) => Got::Frame(frame),
            Err(e) => protocol_error(&e.to_string()),
        }
    }
}

impl Ctx {
    /// A sender channel for one of the app's threads (ROADMAP AP1): hand
    /// it to the thread, which asks the node through it at any time.
    ///
    /// # Errors
    ///
    /// [`SenderError::LimitReached`] at the app's `senders_max`, or why
    /// the channel could not be made.
    pub fn sender(&mut self) -> Result<Sender, SenderError> {
        match crate::app::request(Request::OpenSender) {
            Answer::Sender(result) => Sender::from_channel(result?),
            other => unexpected(&other),
        }
    }
}

/// A hot keyvalue operation as the CBOR request it stands for.
fn cold_request(bucket: &str, op: KvOp, key: &str, value: &[u8]) -> KvRequest {
    let (bucket, key) = (bucket.to_string(), key.to_string());
    match op {
        KvOp::Get => KvRequest::Get { bucket, key },
        KvOp::Set => KvRequest::Set {
            bucket,
            key,
            value: Bytes::from(value),
        },
        KvOp::Delete => KvRequest::Delete { bucket, key },
        KvOp::Exists => KvRequest::Exists { bucket, key },
    }
}

fn unexpected(answer: &Answer) -> ! {
    protocol_error(&format!("unexpected answer {answer:?}"))
}

/// A started call's answer, if it is one given without the node (a frame
/// over the limit).
fn called(refused: Option<Answer>) -> Option<Result<Envelope, CallError>> {
    match refused {
        Some(Answer::Called(result)) => Some(result),
        _ => None,
    }
}

pub(crate) fn unexpected_frame(frame: &ToApp) -> ! {
    protocol_error(&format!("unexpected frame on a sender: {frame:?}"))
}

pub(crate) fn protocol_error(why: &str) -> ! {
    crate::app::protocol_error(&format!("sender: {why}"))
}

pub(crate) fn gone() -> ! {
    crate::app::gone()
}

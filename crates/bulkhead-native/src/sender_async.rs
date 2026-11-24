//! A sender channel for tokio (spike S41, ROADMAP DA2): calls in flight
//! from many tasks of one thread, each awaited, with the thread's runtime
//! free between answers.
//!
//! An [`AsyncSender`] takes a [`Sender`] for good: its door becomes
//! non-blocking and its blocking requests are not used again. Calls start
//! at once (the frame is written while the caller is polled) and their
//! answers are read by [`AsyncSender::pump`], a task the caller spawns
//! with `tokio::task::spawn_local` on the same thread. With calls in
//! flight and nothing to read, the pump spins for the sender's budget
//! (`sender_spin_us`), yielding to the thread's other tasks, then says it
//! sleeps and waits for the door to become readable in tokio's reactor,
//! as [`crate::channel::Spin::block_until`] waits on it in a blocking
//! read.
//!
//! A frame that does not fit in the ring is not waited for: what does not
//! fit is queued, and the pump writes it as the node makes room, waking on
//! the door for room as for answers (ROADMAP PF1). A 1 MiB request through
//! a 64 KiB ring otherwise held the thread, and every connection on it,
//! until the node had read it. Against a node that does not wake writers
//! through the door, writes block as before.
//!
//! One thread only (`!Send`): a current-thread runtime, a `LocalSet`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;

use crate::channel::Spin;
use crate::protocol::{self, Answer, CallError, CastError, Envelope, Pid, Request, ToApp};
use crate::sender::{Sender, gone, protocol_error, unexpected_frame};

/// The room `partial` starts with: a ring's worth.
const CHUNK: usize = 64 << 10;
/// The longest `partial` kept between answers: room for one carrying a
/// 1 MiB body.
const KEPT_PARTIAL: usize = 2 << 20;

/// A sender channel whose calls are awaited (spike S41).
#[derive(Debug, Clone)]
pub struct AsyncSender {
    core: Rc<Core>,
}

/// The door's descriptor, for tokio's reactor; owned by the sender.
#[derive(Debug)]
struct Door(RawFd);

impl AsRawFd for Door {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[derive(Debug)]
struct Core {
    /// Dropped before `state`, which closes the descriptor it names.
    door: AsyncFd<Door>,
    state: RefCell<State>,
    /// A call started: wakes the pump while nothing is in flight.
    work: Notify,
    spin: Duration,
}

#[derive(Debug)]
struct State {
    sender: Sender,
    /// Bytes read from the ring from `start` on, not decoded yet: read
    /// into its spare capacity, never zeroed first (zeroing a 64 KiB chunk
    /// at each look cost a sixth of a driver's CPU on a Raspberry Pi 5,
    /// S41).
    partial: Vec<u8>,
    start: usize,
    /// Calls in flight by id.
    slots: HashMap<u64, Slot>,
}

#[derive(Debug)]
enum Slot {
    Waiting(Option<Waker>),
    Done(Answer),
    /// Its future was dropped; the answer is thrown away.
    Abandoned,
}

impl AsyncSender {
    /// Take `sender` for awaited calls.
    ///
    /// # Errors
    ///
    /// The door could not be made non-blocking or registered with the
    /// current runtime's reactor (call within a runtime).
    pub fn new(mut sender: Sender) -> io::Result<Self> {
        sender.set_queued();
        let door = sender.end.door();
        rustix::fs::fcntl_setfl(door, rustix::fs::OFlags::NONBLOCK)?;
        // SAFETY: the door is the sender's open descriptor, and the sender
        // moves into the same `Core`, which drops the `AsyncFd` first
        // (field order): it stays open and the same for the `AsyncFd`'s
        // life.
        let door = unsafe {
            AsyncFd::register_with_interest(Door(door.as_raw_fd()), tokio::io::Interest::READABLE)
        }?;
        Ok(Self {
            core: Rc::new(Core {
                door,
                state: RefCell::new(State {
                    sender,
                    partial: Vec::with_capacity(CHUNK),
                    start: 0,
                    slots: HashMap::new(),
                }),
                work: Notify::new(),
                spin: Spin::sender_from_env().0,
            }),
        })
    }

    /// Call the app called `app`, as [`Sender::call_app`].
    pub fn call_app(&self, app: impl Into<String>, msg: Envelope, timeout_ms: u32) -> Called {
        self.start(Request::CallApp {
            app: app.into(),
            msg,
            timeout_ms,
            partner: false,
        })
    }

    /// Call the app called `app` on the failover partner, where it runs
    /// now (`Request::CallApp` with `partner`): only for a driver whose
    /// profile forwards (ROADMAP DA2).
    pub fn call_app_partner(
        &self,
        app: impl Into<String>,
        msg: Envelope,
        timeout_ms: u32,
    ) -> Called {
        self.start(Request::CallApp {
            app: app.into(),
            msg,
            timeout_ms,
            partner: true,
        })
    }

    /// Call `to`, as [`Sender::call`].
    pub fn call(&self, to: Pid, msg: Envelope, timeout_ms: u32) -> Called {
        self.start(Request::Call {
            to,
            msg,
            timeout_ms,
        })
    }

    /// Send `msg` to `to` (`Request::Cast`), as [`Sender::try_cast`]: the
    /// future says whether it was queued. A thread's way to wake its own
    /// instance's callbacks, by casting to the instance's pid (ROADMAP
    /// DA9).
    pub fn cast(&self, to: Pid, msg: Envelope) -> Casted {
        Casted(self.ask(Request::Cast { to, msg }))
    }

    /// Cast to the app called `app` on this node, as
    /// [`Sender::try_cast_app`].
    pub fn cast_app(&self, app: &str, msg: Envelope) -> Casted {
        Casted(self.ask(Request::CastApp {
            app: app.to_string(),
            msg,
        }))
    }

    /// A stream into the node's admin API (ROADMAP DA7), as
    /// [`Sender::admin_stream`].
    ///
    /// # Errors
    ///
    /// As [`Sender::admin_stream`].
    pub async fn admin_stream(&self) -> Result<std::os::fd::OwnedFd, crate::protocol::NetError> {
        use crate::protocol::NetError;
        match Answered(self.ask(Request::AdminStream)).await {
            Answer::AdminStream(result) => {
                crate::app::take_fd(result?).map_err(|e| NetError::Other(e.to_string()))
            }
            other => protocol_error(&format!("AdminStream answered with {other:?}")),
        }
    }

    /// A connection to the driver's endpoint (ROADMAP DA3), as
    /// [`Sender::connect`].
    ///
    /// # Errors
    ///
    /// As [`Sender::connect`].
    pub async fn connect(&self) -> Result<std::os::fd::OwnedFd, crate::protocol::NetError> {
        use crate::protocol::NetError;
        match Answered(self.ask(Request::Connect)).await {
            Answer::Connected(result) => {
                crate::app::take_fd(result?).map_err(|e| NetError::Other(e.to_string()))
            }
            other => protocol_error(&format!("Connect answered with {other:?}")),
        }
    }

    /// A connection to `host` and `port` for the app an egress driver
    /// serves (ROADMAP DA4), as [`Sender::connect_to`].
    ///
    /// # Errors
    ///
    /// As [`Sender::connect_to`].
    pub async fn connect_to(
        &self,
        host: &str,
        port: u16,
    ) -> Result<std::os::fd::OwnedFd, crate::protocol::NetError> {
        use crate::protocol::NetError;
        let request = Request::ConnectTo {
            host: host.to_string(),
            port,
        };
        match Answered(self.ask(request)).await {
            Answer::Connected(result) => {
                crate::app::take_fd(result?).map_err(|e| NetError::Other(e.to_string()))
            }
            other => protocol_error(&format!("ConnectTo answered with {other:?}")),
        }
    }

    /// The node's metrics as text (ROADMAP DA7), as
    /// [`Sender::metrics_text`].
    ///
    /// # Errors
    ///
    /// As [`Sender::metrics_text`].
    pub async fn metrics_text(&self) -> Result<String, crate::protocol::NetError> {
        match Answered(self.ask(Request::MetricsText)).await {
            Answer::MetricsText(result) => result,
            other => protocol_error(&format!("MetricsText answered with {other:?}")),
        }
    }

    /// Answer the call `call_ref` a callback of this instance left
    /// waiting, as [`Sender::reply`]: sent at once, the node's
    /// acknowledgement awaited.
    pub async fn reply(&self, call_ref: u64, msg: Envelope) {
        match Answered(self.ask(Request::Reply { call_ref, msg })).await {
            Answer::Unit => {}
            other => protocol_error(&format!("Reply answered with {other:?}")),
        }
    }

    /// Where the app called `app`, which does not run here, runs now, if
    /// on the failover partner (`Request::Elsewhere`): for a driver given
    /// the route table (ROADMAP DA2).
    pub fn elsewhere(&self, app: impl Into<String>) -> Located {
        Located(self.ask(Request::Elsewhere { app: app.into() }))
    }

    fn start(&self, request: Request) -> Called {
        Called(self.ask(request))
    }

    fn ask(&self, request: Request) -> Asked {
        let id = {
            let mut state = self.core.state.borrow_mut();
            let (id, refused) = state.sender.send_request_checked(request);
            state
                .slots
                .insert(id, refused.map_or(Slot::Waiting(None), Slot::Done));
            id
        };
        self.core.work.notify_one();
        Asked {
            core: self.core.clone(),
            id,
        }
    }

    /// Add `delta` to a metric the sender registered before it became
    /// async ([`Sender::register_metric`]); a frame with no answer.
    pub fn metric_add(&self, metric: u32, delta: f64) {
        self.core
            .state
            .borrow_mut()
            .sender
            .metric_add(metric, delta);
        // Queued behind a large frame, it goes when the pump writes.
        self.core.work.notify_one();
    }

    /// Calls in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.core.state.borrow().slots.len()
    }

    /// Read answers as they come and wake their callers; runs until the
    /// node ends this process. Spawn it once, on this thread.
    pub async fn pump(self) {
        let core = self.core;
        loop {
            let (waiting, queued) = {
                let mut state = core.state.borrow_mut();
                let queued = state.sender.flush_queue();
                state.drain();
                (state.unanswered(), queued)
            };
            if !waiting && !queued {
                core.work.notified().await;
                continue;
            }
            if !core.spin.is_zero() && spin(&core, queued).await {
                continue;
            }
            if !arm(&core, waiting, queued) {
                continue;
            }
            match core.door.readable().await {
                Ok(mut ready) => {
                    let mut byte = [0u8; 64];
                    loop {
                        match rustix::io::read(core.door.get_ref().fd(), &mut byte) {
                            Ok(0) => gone(),
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    ready.clear_ready();
                }
                Err(e) => protocol_error(&format!("door: {e}")),
            }
            let state = core.state.borrow();
            state.sender.end.disarm_reader();
            state.sender.end.disarm_writer();
        }
    }
}

/// Say the pump sleeps until the door wakes it: for an answer while one
/// is `waiting`, for room while frames are `queued`. `false`, and nothing
/// said, when there is something to do already.
fn arm(core: &Core, waiting: bool, queued: bool) -> bool {
    let state = core.state.borrow();
    let end = &state.sender.end;
    if waiting && !end.arm_reader() {
        return false;
    }
    if queued && !end.arm_writer() {
        end.disarm_reader();
        return false;
    }
    true
}

/// The bytes still to come of the frame `have` starts, once its length
/// prefix is in; `None` before it, or for a length the frame check will
/// refuse.
fn frame_room(have: &[u8]) -> Option<usize> {
    let prefix = <[u8; 4]>::try_from(have.get(..4)?).ok()?;
    let len = protocol::frame_len(prefix).ok()?;
    (4 + len).checked_sub(have.len())
}

/// Spin for the budget, yielding to the thread's other tasks; whether the
/// node wrote meanwhile, or made room for what is `queued`.
async fn spin(core: &Core, queued: bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < core.spin {
        tokio::task::yield_now().await;
        let state = core.state.borrow();
        if state.sender.end.is_readable() || queued && state.sender.end.has_room_to_node() {
            return true;
        }
    }
    false
}

impl Door {
    fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        // SAFETY: the sender that owns the descriptor outlives the door
        // (`Core` drops `door` first) and does not close it before.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.0) }
    }
}

impl State {
    /// Whether a call waits for its answer: one answered but not yet
    /// taken by its caller does not, or the pump would spin and then sleep
    /// on the door just as the next call starts (S41).
    fn unanswered(&self) -> bool {
        self.slots.values().any(|s| !matches!(s, Slot::Done(_)))
    }

    /// Read and decode everything the node has written, filling slots.
    fn drain(&mut self) {
        loop {
            while let Some(frame) = self.take_frame() {
                self.answer(frame);
            }
            if self.start > 0 && self.start == self.partial.len() {
                self.partial.clear();
                self.start = 0;
                // A large answer's buffer is not kept for the thread's life.
                if self.partial.capacity() > KEPT_PARTIAL {
                    self.partial = Vec::with_capacity(CHUNK);
                }
            } else if self.start > 0 && self.start * 2 >= self.partial.len() {
                self.partial.drain(..self.start);
                self.start = 0;
            }
            // A large frame's whole length at once, once its prefix is in.
            if let Some(want) = frame_room(&self.partial[self.start..]) {
                self.partial.reserve(want);
            }
            match self.sender.end.try_read_into_from_node(&mut self.partial) {
                Ok(Some(0)) => gone(),
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(e) => protocol_error(&e.to_string()),
            }
        }
    }

    /// The next whole frame from `partial`, decoded.
    fn take_frame(&mut self) -> Option<ToApp> {
        let rest = &self.partial[self.start..];
        let prefix: [u8; 4] = rest.get(..4)?.try_into().ok()?;
        let len = protocol::frame_len(prefix).unwrap_or_else(|e| protocol_error(&e.to_string()));
        let body = rest.get(4..4 + len)?;
        let frame = if crate::hot::is_hot(body) {
            match crate::hot::parse_to_app(body) {
                Ok(crate::hot::ToAppHot::Frame(frame)) => frame,
                Ok(crate::hot::ToAppHot::Kv(..)) => {
                    protocol_error("a hot keyvalue answer on an async sender")
                }
                Err(e) => protocol_error(&e.to_string()),
            }
        } else {
            protocol::decode::<ToApp>(body).unwrap_or_else(|e| protocol_error(&e.to_string()))
        };
        self.start += 4 + len;
        Some(frame)
    }

    fn answer(&mut self, frame: ToApp) {
        let ToApp::Answer { id, answer } = frame else {
            unexpected_frame(&frame)
        };
        match self.slots.remove(&id) {
            Some(Slot::Waiting(waker)) => {
                self.slots.insert(id, Slot::Done(answer));
                if let Some(w) = waker {
                    w.wake();
                }
            }
            Some(Slot::Abandoned) => {}
            Some(done @ Slot::Done(_)) => {
                self.slots.insert(id, done);
                protocol_error("a call answered twice");
            }
            None => protocol_error("an answer to no call"),
        }
    }
}

/// A request's answer, awaited; dropped first, the answer is thrown
/// away when it comes.
#[derive(Debug)]
struct Asked {
    core: Rc<Core>,
    id: u64,
}

impl Asked {
    fn poll_answer(&self, cx: &Context<'_>) -> Poll<Answer> {
        let mut state = self.core.state.borrow_mut();
        match state.slots.remove(&self.id) {
            Some(Slot::Done(answer)) => Poll::Ready(answer),
            Some(Slot::Waiting(_)) | None => {
                state
                    .slots
                    .insert(self.id, Slot::Waiting(Some(cx.waker().clone())));
                Poll::Pending
            }
            Some(Slot::Abandoned) => Poll::Pending,
        }
    }
}

impl Drop for Asked {
    fn drop(&mut self) {
        let mut state = self.core.state.borrow_mut();
        if let Some(Slot::Waiting(_)) = state.slots.get(&self.id) {
            state.slots.insert(self.id, Slot::Abandoned);
        }
    }
}

/// A request's answer as it came, awaited.
struct Answered(Asked);

impl Future for Answered {
    type Output = Answer;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Answer> {
        self.0.poll_answer(cx)
    }
}

/// A call's answer, awaited.
#[derive(Debug)]
#[must_use = "a call's answer is read by awaiting it"]
pub struct Called(Asked);

impl Called {
    /// The call's id on the channel.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.0.id
    }
}

impl Future for Called {
    type Output = Result<Envelope, CallError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_answer(cx).map(|answer| match answer {
            Answer::Called(result) => result,
            other => protocol_error(&format!("a call answered with {other:?}")),
        })
    }
}

/// Whether a cast was queued, awaited ([`AsyncSender::cast`]).
#[derive(Debug)]
#[must_use = "dropped, the cast is still sent and its answer thrown away"]
pub struct Casted(Asked);

impl Future for Casted {
    type Output = Result<(), CastError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_answer(cx).map(|answer| match answer {
            Answer::Unit => Ok(()),
            Answer::CastFailed(e) => Err(e),
            other => protocol_error(&format!("a cast answered with {other:?}")),
        })
    }
}

/// Where an app runs, awaited ([`AsyncSender::elsewhere`]).
#[derive(Debug)]
#[must_use = "the answer is read by awaiting it"]
pub struct Located(Asked);

impl Future for Located {
    type Output = Option<u32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_answer(cx).map(|answer| match answer {
            Answer::Elsewhere(node) => node,
            other => protocol_error(&format!("Elsewhere answered with {other:?}")),
        })
    }
}

#[cfg(test)]
#[path = "sender_async_tests.rs"]
mod sender_async_tests;

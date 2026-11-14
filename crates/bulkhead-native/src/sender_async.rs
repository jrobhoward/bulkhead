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
use crate::protocol::{self, Answer, CallError, Envelope, Pid, Request, ToApp};
use crate::sender::{Sender, gone, protocol_error, unexpected_frame};

/// The bytes read from the ring at a time.
const CHUNK: usize = 64 << 10;

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
    /// Bytes read from the ring from `start` on, not decoded yet.
    partial: Vec<u8>,
    /// Where the ring is read into, zeroed once: growing `partial` by a
    /// zeroed chunk at each look cost a 64 KiB `memset` per look, a
    /// sixth of a driver's CPU on a Raspberry Pi 5 (S41).
    chunk: Box<[u8]>,
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
    pub fn new(sender: Sender) -> io::Result<Self> {
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
                    chunk: vec![0; CHUNK].into_boxed_slice(),
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
            let id = state.sender.send_request(request);
            state.slots.insert(id, Slot::Waiting(None));
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
            let waiting = {
                let mut state = core.state.borrow_mut();
                state.drain();
                state.unanswered()
            };
            if !waiting {
                core.work.notified().await;
                continue;
            }
            if !core.spin.is_zero() && spin(&core).await {
                continue;
            }
            if !core.state.borrow().sender.end.arm_reader() {
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
            core.state.borrow().sender.end.disarm_reader();
        }
    }
}

/// Spin for the budget, yielding to the thread's other tasks; whether the
/// node wrote meanwhile.
async fn spin(core: &Core) -> bool {
    let start = Instant::now();
    while start.elapsed() < core.spin {
        tokio::task::yield_now().await;
        if core.state.borrow().sender.end.is_readable() {
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
            if self.start > 0 && self.start * 2 >= self.partial.len() {
                self.partial.drain(..self.start);
                self.start = 0;
            }
            match self.sender.end.try_read_from_node(&mut self.chunk) {
                Ok(Some(0)) => gone(),
                Ok(Some(n)) => self.partial.extend_from_slice(&self.chunk[..n]),
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

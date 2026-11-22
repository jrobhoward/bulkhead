//! Chapter 5: the room of chapter 4, also watching the `posts` bucket
//! `history` writes, and logging each new post.
//!
//! - `post <text>`, `recent` and `clear` as in chapter 4.
//! - `seen`: how many posts this instance has seen arrive in the bucket.
//!
//! The watch's notices arrive in `handle_info`. After each, the room saves
//! the bucket's revision in a bucket of its own (`cursor`), and a new
//! instance resumes the watch after it: a post written while no instance
//! ran is still seen, and none is seen twice.

use std::time::Duration;

use bulkhead_sdk::kv::Bucket;
use bulkhead_sdk::metrics::Counter;
use bulkhead_sdk::watch::{self, Change, WatchError};
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Info, Reply, groups, info, warn};

/// The group posts are published to.
pub const CHAT: &str = "chat";
/// The app that keeps the posts.
pub const HISTORY: &str = "history";
/// The shared bucket `history` keeps them in.
pub const POSTS: &str = "posts";
/// Every post's key starts with this.
pub const PREFIX: &str = "p";
/// The room's own bucket, and the key it keeps its place in `posts` under.
const CURSOR: &str = "cursor";
const REVISION: &str = "revision";
const TIMEOUT: Duration = Duration::from_secs(2);

/// What a call to the room asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    /// Post this text.
    Post(&'a str),
    /// The last posts, from `history`.
    Recent,
    /// Tell `history` to delete them.
    Clear,
    /// How many new posts this instance has seen.
    Seen,
    /// Anything else.
    Unknown,
}

/// Read a call's text.
#[must_use]
pub fn command(text: &str) -> Command<'_> {
    match text {
        "recent" => Command::Recent,
        "clear" => Command::Clear,
        "seen" => Command::Seen,
        _ => text
            .strip_prefix("post ")
            .map_or(Command::Unknown, Command::Post),
    }
}

/// The log line for a change to key `key` of `posts`, whose value is now
/// `value` (`None`: deleted); `None` for keys that are not posts.
#[must_use]
pub fn describe(key: &str, value: Option<&[u8]>) -> Option<String> {
    if !key.starts_with(PREFIX) {
        return None;
    }
    Some(match value {
        Some(text) => format!("new post {key}: {}", String::from_utf8_lossy(text)),
        None => format!("post {key} removed"),
    })
}

struct Room {
    posted: Counter,
    cursor: Bucket,
    seen: u64,
}

impl Room {
    /// Watch `posts` after the revision a previous instance saved, or from
    /// now on the first start (or when the bucket has compacted past it).
    fn watch(cursor: &Bucket) -> Result<(), String> {
        let after = cursor
            .get_value::<u64>(REVISION)
            .map_err(|e| e.to_string())?;
        let started = match after.map(|rev| watch::resume(POSTS, PREFIX, true, rev)) {
            Some(Ok(started)) => started,
            Some(Err(WatchError::TooOld(oldest))) => {
                warn!("posts before revision {oldest} are gone; watching from now");
                watch::watch_prefix(POSTS, PREFIX).map_err(|e| e.to_string())?
            }
            Some(Err(e)) => return Err(e.to_string()),
            None => watch::watch_prefix(POSTS, PREFIX).map_err(|e| e.to_string())?,
        };
        info!(
            "watching {POSTS} after revision {}",
            after.unwrap_or(started.revision)
        );
        Ok(())
    }

    fn post(&self, text: &str) -> String {
        self.posted.inc();
        match groups::publish(CHAT, Envelope::text(format!("said {text}"))) {
            Ok(reached) => format!("posted to {reached}"),
            Err(e) => format!("not published: {e}"),
        }
    }

    fn recent(ctx: &mut Ctx) -> String {
        match ctx.call_app(HISTORY, Envelope::text("recent"), TIMEOUT) {
            Ok(answer) => answer.as_text().unwrap_or("").to_string(),
            Err(e) => format!("history: {e}"),
        }
    }

    fn clear(ctx: &mut Ctx) -> String {
        match ctx.whereis(HISTORY) {
            Some(pid) => {
                ctx.cast(pid, Envelope::text("clear"));
                "clear sent".to_string()
            }
            None => "history is not running".to_string(),
        }
    }
}

impl App for Room {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        let posted = Counter::new("posts_total", "Posts made in the room", &[])
            .map_err(|e| format!("metric: {e}"))?;
        let cursor = Bucket::open(CURSOR).map_err(|e| format!("{CURSOR}: {e}"))?;
        Self::watch(&cursor)?;
        Ok(Self {
            posted,
            cursor,
            seen: 0,
        })
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let answer = match command(msg.as_text().unwrap_or("")) {
            Command::Post(text) => self.post(text),
            Command::Recent => Self::recent(ctx),
            Command::Clear => Self::clear(ctx),
            Command::Seen => self.seen.to_string(),
            Command::Unknown => "say `post <text>`, `recent`, `clear` or `seen`".to_string(),
        };
        Reply::Now(Envelope::text(answer))
    }

    fn handle_info(&mut self, _ctx: &mut Ctx, info: Info) {
        let Some(change) = Change::from_info(&info) else {
            return;
        };
        let value = change.current().ok().flatten().map(|e| e.value);
        if let Some(line) = describe(&change.key, value.as_deref()) {
            self.seen += 1;
            info!("{line}");
        }
        if let Err(e) = self.cursor.set_value(REVISION, &change.revision) {
            warn!("{CURSOR}: {e}");
        }
    }
}

bulkhead_sdk::app!(Room);

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;

//! Chapter 5: `history` keeps the posts in the node's `posts` bucket, so
//! they outlive its instances (and the node's restarts: the bucket is on
//! disk).
//!
//! - Each post the room publishes (`said <text>`) is stored under the next
//!   key, `p000001`, `p000002`, ..., numbered by the bucket's `seq`
//!   counter.
//! - A cast `clear` deletes them.
//! - A call `recent` answers the last ten, oldest first.

use bulkhead_sdk::kv::Bucket;
use bulkhead_sdk::watch;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply, groups, info, warn};

/// The group the room publishes to.
pub const CHAT: &str = "chat";
/// The shared bucket the posts are kept in.
pub const POSTS: &str = "posts";
/// Every post's key starts with this; the counter's does not.
pub const PREFIX: &str = "p";
/// The counter that numbers the posts.
const SEQ: &str = "seq";
/// How many posts `recent` answers.
pub const KEEP: u32 = 10;

/// The key post number `n` is stored under: fixed width, so the bucket's
/// key order is the posts' order.
#[must_use]
pub fn key(n: u64) -> String {
    format!("{PREFIX}{n:06}")
}

/// What a message to `history` asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    /// Keep a post.
    Said(&'a str),
    /// Delete every post.
    Clear,
    /// Answer the last posts.
    Recent,
    /// Anything else.
    Unknown,
}

/// Read a message's text.
#[must_use]
pub fn command(text: &str) -> Command<'_> {
    match text {
        "clear" => Command::Clear,
        "recent" => Command::Recent,
        _ => text
            .strip_prefix("said ")
            .map_or(Command::Unknown, Command::Said),
    }
}

struct History {
    posts: Bucket,
}

impl History {
    fn keep(&self, text: &str) -> Result<(), String> {
        let n = self.posts.increment(SEQ, 1).map_err(|e| e.to_string())?;
        self.posts
            .set(&key(n), text.as_bytes())
            .map_err(|e| e.to_string())
    }

    fn clear(&self) -> Result<usize, String> {
        let keys: Vec<String> = self
            .posts
            .keys()
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|k| k.starts_with(PREFIX))
            .collect();
        self.posts.delete_many(&keys).map_err(|e| e.to_string())?;
        Ok(keys.len())
    }

    fn recent() -> Result<String, String> {
        let page = watch::scan(POSTS)
            .prefix(PREFIX)
            .reverse()
            .limit(KEEP)
            .page(None)
            .map_err(|e| e.to_string())?;
        let mut posts: Vec<String> = page
            .entries
            .into_iter()
            .map(|e| String::from_utf8_lossy(&e.value).into_owned())
            .collect();
        posts.reverse();
        Ok(posts.join("\n"))
    }
}

impl App for History {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        let posts = Bucket::open(POSTS).map_err(|e| format!("{POSTS}: {e}"))?;
        groups::join(CHAT).map_err(|e| format!("join {CHAT}: {e}"))?;
        Ok(Self { posts })
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        let done = match command(msg.as_text().unwrap_or("")) {
            Command::Said(text) => self.keep(text),
            Command::Clear => self.clear().map(|n| info!("cleared {n} posts")),
            Command::Recent | Command::Unknown => Ok(()),
        };
        if let Err(e) = done {
            warn!("{POSTS}: {e}");
        }
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let answer = match command(msg.as_text().unwrap_or("")) {
            Command::Recent => Self::recent().unwrap_or_else(|e| format!("error: {e}")),
            _ => "say `recent`".to_string(),
        };
        Reply::Now(Envelope::text(answer))
    }
}

bulkhead_sdk::app!(History);

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;

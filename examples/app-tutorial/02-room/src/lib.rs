//! Chapter 2: a chat room that keeps its posts in memory.
//!
//! - `post <text>`: add a post; the reply is its number, `#1`, `#2`, ...
//! - `recent`: the posts so far, one per line.
//!
//! An instance's memory is its own: when it restarts, the posts are gone.
//! Chapter 5 keeps them in a bucket instead.

use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};

/// The app's state: one value per instance, made by `init`.
struct Room {
    posts: Vec<String>,
}

impl App for Room {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self { posts: Vec::new() })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let answer = if let Some(post) = text.strip_prefix("post ") {
            self.posts.push(post.to_string());
            format!("#{}", self.posts.len())
        } else if text == "recent" {
            self.posts.join("\n")
        } else {
            "say `post <text>` or `recent`".to_string()
        };
        Reply::Now(Envelope::text(answer))
    }
}

bulkhead_sdk::app!(Room);

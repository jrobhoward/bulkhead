//! Chapter 3: the room of chapter 2, logging what it does and counting
//! posts in a metric.
//!
//! - `post <text>`: add a post; the reply is its number.
//! - `recent`: the posts so far, one per line.
//!
//! Records go to the node's log ring (`bulkhead logs room`); the counter is
//! exported as `room_posts_total` (`bulkhead metrics`).

use bulkhead_sdk::metrics::Counter;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply, debug, info, warn};

struct Room {
    posts: Vec<String>,
    /// Registered once, in `init`; each post adds one.
    posted: Counter,
}

impl App for Room {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        let posted = Counter::new("posts_total", "Posts made in the room", &[])
            .map_err(|e| format!("metric: {e}"))?;
        info!("room open");
        Ok(Self {
            posts: Vec::new(),
            posted,
        })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let answer = if let Some(post) = text.strip_prefix("post ") {
            self.posts.push(post.to_string());
            self.posted.inc();
            info!("post #{}: {post}", self.posts.len());
            format!("#{}", self.posts.len())
        } else if text == "recent" {
            debug!("recent: {} posts", self.posts.len());
            self.posts.join("\n")
        } else {
            warn!("not understood: {text:?}");
            "say `post <text>` or `recent`".to_string()
        };
        Reply::Now(Envelope::text(answer))
    }
}

bulkhead_sdk::app!(Room);

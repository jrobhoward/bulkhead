//! Chapter 4: the room talks to a second app, `history`, three ways.
//!
//! - `post <text>`: publish the post to the process group `chat`, which
//!   `history`'s instances join (a cast to every member); the reply is the
//!   post's number and how many members the publish reached.
//! - `recent`: call `history` by app name and return its answer.
//! - `clear`: cast `clear` to an instance of `history`; no answer comes
//!   back, so the reply only says it was sent.
//!
//! What the room keeps in memory is now only its post count.

use std::time::Duration;

use bulkhead_sdk::metrics::Counter;
use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply, groups, info, warn};

/// The group posts are published to.
pub const CHAT: &str = "chat";
/// The app that keeps the posts.
pub const HISTORY: &str = "history";
/// How long `recent` waits for `history`.
const TIMEOUT: Duration = Duration::from_secs(2);

struct Room {
    posts: u64,
    posted: Counter,
}

impl Room {
    fn post(&mut self, text: &str) -> String {
        self.posts += 1;
        self.posted.inc();
        match groups::publish(CHAT, Envelope::text(format!("said {text}"))) {
            Ok(reached) => {
                info!("post #{}: {text} (to {reached})", self.posts);
                format!("#{} to {reached}", self.posts)
            }
            Err(e) => {
                warn!("publish: {e}");
                format!("#{} not published: {e}", self.posts)
            }
        }
    }

    fn recent(ctx: &mut Ctx) -> String {
        match ctx.call_app(HISTORY, Envelope::text("recent"), TIMEOUT) {
            Ok(answer) => answer.as_text().unwrap_or("").to_string(),
            Err(e) => format!("history: {e}"),
        }
    }

    fn clear(ctx: &mut Ctx) -> String {
        // An app's name finds one of its instances when no instance has
        // registered the name itself.
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
        Ok(Self { posts: 0, posted })
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let answer = if let Some(post) = text.strip_prefix("post ") {
            self.post(post)
        } else if text == "recent" {
            Self::recent(ctx)
        } else if text == "clear" {
            Self::clear(ctx)
        } else {
            "say `post <text>`, `recent` or `clear`".to_string()
        };
        Reply::Now(Envelope::text(answer))
    }
}

bulkhead_sdk::app!(Room);

//! Chapter 4: `history` keeps the room's last posts.
//!
//! - It joins the process group `chat` in `init`; each post the room
//!   publishes arrives as a cast, `said <text>`.
//! - A cast `clear` empties it.
//! - A call `recent` answers the last posts, one per line.
//!
//! The posts are in this instance's memory: a restart loses them
//! (chapter 5 moves them into a bucket).

use std::collections::VecDeque;

use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply, groups, info};

/// The group the room publishes to.
pub const CHAT: &str = "chat";
/// How many posts `recent` keeps.
pub const KEEP: usize = 10;

struct History {
    posts: VecDeque<String>,
}

impl App for History {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        groups::join(CHAT).map_err(|e| format!("join {CHAT}: {e}"))?;
        Ok(Self {
            posts: VecDeque::new(),
        })
    }

    fn handle_cast(&mut self, _ctx: &mut Ctx, msg: Envelope) {
        let text = msg.as_text().unwrap_or("");
        if let Some(post) = text.strip_prefix("said ") {
            if self.posts.len() == KEEP {
                self.posts.pop_front();
            }
            self.posts.push_back(post.to_string());
        } else if text == "clear" {
            info!("cleared {} posts", self.posts.len());
            self.posts.clear();
        }
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let answer = match msg.as_text() {
            Some("recent") => Vec::from(self.posts.clone()).join("\n"),
            _ => "say `recent`".to_string(),
        };
        Reply::Now(Envelope::text(answer))
    }
}

bulkhead_sdk::app!(History);

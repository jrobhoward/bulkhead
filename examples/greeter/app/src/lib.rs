//! An app that calls the greeter, an app of its own, through the
//! greeter's client module (`example_greeter_service::client`).
//!
//! Commands: `hello:<name>` (the greeting) and `count` (greetings the
//! greeter instance has made). Errors reply `err:<error>`.

use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};
use example_greeter_service::client as greeter;

#[derive(Default)]
struct Greeted;

impl App for Greeted {
    fn init(_ctx: &mut Ctx, _args: &[u8]) -> Result<Self, String> {
        Ok(Self)
    }

    fn handle_call(&mut self, ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let text = msg.as_text().unwrap_or("");
        let reply = match text.split_once(':') {
            Some(("hello", name)) => {
                greeter::hello(ctx, name).unwrap_or_else(|e| format!("err:{e}"))
            }
            _ if text == "count" => {
                greeter::count(ctx).map_or_else(|e| format!("err:{e}"), |n| n.to_string())
            }
            _ => "commands: hello:<name>, count".into(),
        };
        Reply::Now(Envelope::text(reply))
    }
}

bulkhead_sdk::app!(Greeted);

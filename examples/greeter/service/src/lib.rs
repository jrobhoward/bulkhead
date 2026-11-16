//! The greeter: a service of its own as an ordinary app (ROADMAP DA8
//! replaced host extensions with this). It answers [`client::Request`]s
//! with the salutation its `start_args` give, and counts its greetings.
//! Other apps call it through [`client`]; its manifest's
//! `messaging_acl.accept_from` names the apps it serves.

use bulkhead_sdk::{App, CallRef, Ctx, Envelope, Reply};
use serde::Deserialize;

pub mod client;

use client::{Request, Response, SCHEMA};

#[derive(Deserialize)]
struct Args {
    #[serde(default = "default_salutation")]
    salutation: String,
}

fn default_salutation() -> String {
    "Hello".into()
}

/// One greeter instance.
struct Greeter {
    salutation: String,
    greetings: u64,
}

impl Greeter {
    fn answer(&mut self, request: Request) -> Response {
        match request {
            Request::Hello { name } => {
                self.greetings += 1;
                Response::Greeting(format!("{}, {name}", self.salutation))
            }
            Request::Count => Response::Count(self.greetings),
        }
    }
}

impl App for Greeter {
    fn init(_ctx: &mut Ctx, args: &[u8]) -> Result<Self, String> {
        let args: Args = if args.is_empty() {
            Args {
                salutation: default_salutation(),
            }
        } else {
            serde_json::from_slice(args).map_err(|e| format!("start_args: {e}"))?
        };
        Ok(Self {
            salutation: args.salutation,
            greetings: 0,
        })
    }

    fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
        let answer = msg
            .decode::<Request>()
            .map_err(|e| e.to_string())
            .and_then(|request| Envelope::cbor(&self.answer(request)).map_err(|e| e.to_string()));
        Reply::Now(match answer {
            Ok(env) => env.with_schema(SCHEMA),
            Err(why) => Envelope::text(format!("expected a greeter request: {why}")),
        })
    }
}

bulkhead_sdk::app!(Greeter);

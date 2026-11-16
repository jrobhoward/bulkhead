//! The greeter's client: the messages it answers and a typed function for
//! each, for apps that call it. An app depends on this crate for this
//! module alone; the node runs the greeter itself as an app of its own.
//!
//! Each function calls the app `greeter` by name (`Ctx::call_app`), so the
//! node picks the instance and checks the messaging ACLs: the caller's
//! `may_call` must list `greeter`, and the greeter's `accept_from` the
//! caller.

use std::time::Duration;

use bulkhead_sdk::{CallError, Ctx, Envelope};
use serde::{Deserialize, Serialize};

/// The app the functions call.
pub const APP: &str = "greeter";

/// The schema of every message to and from the greeter.
pub const SCHEMA: &str = "example.greeter.v1";

/// How long a call waits for the greeter.
const TIMEOUT: Duration = Duration::from_secs(5);

/// What the greeter is asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Greet `name`.
    Hello {
        /// Who to greet.
        name: String,
    },
    /// How many greetings this greeter instance has made.
    Count,
}

/// What the greeter answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// A greeting.
    Greeting(String),
    /// A count of greetings.
    Count(u64),
}

/// Why a call to the greeter failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The greeter refused this app: its `accept_from` does not list it,
    /// or this app's `may_call` does not list the greeter.
    AccessDenied,
    /// No greeter runs on this node.
    NotRunning,
    /// Anything else: a timeout, a full mailbox, an answer not understood.
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccessDenied => f.write_str("access-denied"),
            Self::NotRunning => f.write_str("the greeter is not running"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

fn ask(ctx: &mut Ctx, request: &Request) -> Result<Response, Error> {
    let msg = Envelope::cbor(request)
        .map_err(|e| Error::Other(e.to_string()))?
        .with_schema(SCHEMA);
    let reply = ctx.call_app(APP, msg, TIMEOUT).map_err(|e| match e {
        CallError::AccessDenied => Error::AccessDenied,
        CallError::Noproc => Error::NotRunning,
        other => Error::Other(other.to_string()),
    })?;
    reply
        .decode::<Response>()
        .map_err(|e| Error::Other(format!("not a greeter's answer: {e}")))
}

/// Greet `name` with the greeter's salutation.
///
/// # Errors
///
/// [`Error::AccessDenied`] if the ACLs keep this app from the greeter,
/// [`Error::NotRunning`] if none runs, [`Error::Other`] otherwise.
pub fn hello(ctx: &mut Ctx, name: &str) -> Result<String, Error> {
    match ask(ctx, &Request::Hello { name: name.into() })? {
        Response::Greeting(text) => Ok(text),
        other => Err(Error::Other(format!("unexpected answer {other:?}"))),
    }
}

/// How many greetings the greeter instance asked has made.
///
/// # Errors
///
/// As [`hello`].
pub fn count(ctx: &mut Ctx) -> Result<u64, Error> {
    match ask(ctx, &Request::Count)? {
        Response::Count(n) => Ok(n),
        other => Err(Error::Other(format!("unexpected answer {other:?}"))),
    }
}

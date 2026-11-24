//! HTTP requests from the node's ingress (cargo feature `http`).
//!
//! The node terminates HTTP and sends each request on the app's routes
//! (manifest `capabilities."bulkhead:http-routes".routes`) to the app
//! as a call. In `handle_call`, [`Request::from_envelope`] recognises one;
//! answer with a [`Response`], now or later with `Ctx::reply`. Any other
//! reply is sent as `200` with its content type and body.
//!
//! ```ignore
//! use bulkhead_sdk::http::{Request, Response};
//!
//! fn handle_call(&mut self, _ctx: &mut Ctx, _call: CallRef, msg: Envelope) -> Reply {
//!     if let Some(req) = Request::from_envelope(&msg) {
//!         return Reply::Now(match (req.method.as_str(), req.path.as_str()) {
//!             ("GET", "/hello") => Response::text(200, "hello"),
//!             _ => Response::text(404, "not here"),
//!         }.into());
//!     }
//!     // ... other calls
//! }
//! ```

use ciborium::Value;
use serde::Serialize;

use crate::envelope::{Envelope, MessageError};

/// The `schema` of a request envelope.
pub const REQUEST_SCHEMA: &str = "bulkhead/http-request";
/// The `schema` of a response envelope.
pub const RESPONSE_SCHEMA: &str = "bulkhead/http-response";

/// An HTTP request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    /// `http` or `https`: how the client reached the node.
    pub scheme: String,
    /// `GET`, `POST`, ...
    pub method: String,
    /// The path, without the query.
    pub path: String,
    /// The query string, without `?`.
    pub query: Option<String>,
    /// Headers in order, names lowercase.
    pub headers: Vec<(String, String)>,
    /// The whole body.
    pub body: Vec<u8>,
    /// The value of the route's `hash_header`, if it names one.
    pub key: Option<String>,
    /// The client's certificate, verified by the ingress against the
    /// listener's `client_ca` (ROADMAP DA2b); `None` without one.
    pub client_cert: Option<ClientCert>,
}

/// A client certificate the ingress verified: its leaf, which an app that
/// needs its subject parses itself (the ingress parses no more of it than
/// the verification does).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientCert {
    /// The SHA-256 of `der`, lowercase hex: the certificate's fingerprint.
    pub sha256: String,
    /// The leaf certificate, DER.
    pub der: Vec<u8>,
}

fn field<'a>(map: &'a [(Value, Value)], name: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_text() == Some(name))
        .map(|(_, v)| v)
}

fn pairs(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some((
                        p.first()?.as_text()?.to_string(),
                        p.get(1)?.as_text()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Request {
    /// The request `msg` carries, if it is one.
    #[must_use]
    pub fn from_envelope(msg: &Envelope) -> Option<Self> {
        if msg.schema.as_deref() != Some(REQUEST_SCHEMA) {
            return None;
        }
        let mut value = decode(&msg.body)?;
        let body = take_bytes(&mut value, "body");
        let map = value.as_map()?;
        let get_text = |name| {
            field(map, name)
                .and_then(Value::as_text)
                .map(str::to_string)
        };
        Some(Self {
            scheme: get_text("scheme").unwrap_or_else(|| "http".into()),
            method: get_text("method")?,
            path: get_text("path")?,
            query: get_text("query"),
            headers: pairs(field(map, "headers")),
            body,
            key: get_text("key"),
            client_cert: field(map, "client_cert")
                .and_then(Value::as_map)
                .and_then(|c| {
                    Some(ClientCert {
                        sha256: field(c, "sha256")?.as_text()?.to_string(),
                        der: field(c, "der")?.as_bytes()?.clone(),
                    })
                }),
        })
    }

    /// The first header named `name` (any case).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as UTF-8 text.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.body).ok()
    }

    /// The query parameter `name`, undecoded.
    #[must_use]
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .as_deref()?
            .split('&')
            .filter_map(|kv| kv.split_once('=').or(Some((kv, ""))))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }

    /// The request as an envelope, as the ingress sends it: a CBOR map,
    /// written from the request without copying its body first (ROADMAP
    /// PF1).
    #[must_use]
    pub fn to_envelope(&self) -> Envelope {
        let wire = RequestWire {
            scheme: &self.scheme,
            method: &self.method,
            path: &self.path,
            query: self.query.as_deref(),
            headers: &self.headers,
            body: &self.body,
            key: self.key.as_deref(),
            client_cert: self.client_cert.as_ref().map(|c| CertWire {
                sha256: &c.sha256,
                der: &c.der,
            }),
        };
        encode(&wire, self.body.len(), REQUEST_SCHEMA)
    }
}

/// A request as its envelope carries it: the keys and their order are the
/// format (GUEST_FORMATS.md, "HTTP").
#[derive(Serialize)]
struct RequestWire<'a> {
    scheme: &'a str,
    method: &'a str,
    path: &'a str,
    query: Option<&'a str>,
    headers: &'a [(String, String)],
    #[serde(serialize_with = "byte_string")]
    body: &'a [u8],
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_cert: Option<CertWire<'a>>,
}

#[derive(Serialize)]
struct CertWire<'a> {
    sha256: &'a str,
    #[serde(serialize_with = "byte_string")]
    der: &'a [u8],
}

/// A response as its envelope carries it.
#[derive(Serialize)]
struct ResponseWire<'a> {
    status: u16,
    headers: &'a [(String, String)],
    #[serde(serialize_with = "byte_string")]
    body: &'a [u8],
}

/// Bytes as a CBOR byte string.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde's `serialize_with` passes a reference.
fn byte_string<S: serde::Serializer>(v: &&[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_bytes(v)
}

/// `value` in CBOR as an envelope of `schema`, in a buffer made for its
/// `body` bytes and the rest.
fn encode<T: Serialize>(value: &T, body: usize, schema: &str) -> Envelope {
    let mut out = Vec::with_capacity(body + 512);
    // Writing to a Vec cannot fail, and these types always serialize.
    let _ = ciborium::into_writer(value, &mut out);
    Envelope::new("application/cbor", out).with_schema(schema)
}

/// A request's or response's CBOR.
fn decode(body: &[u8]) -> Option<Value> {
    ciborium::from_reader(body).ok()
}

/// The byte string at `name` in the map `value`, moved out of it (empty
/// for none).
fn take_bytes(value: &mut Value, name: &str) -> Vec<u8> {
    let Value::Map(map) = value else {
        return Vec::new();
    };
    map.iter_mut()
        .find(|(k, _)| k.as_text() == Some(name))
        .and_then(|(_, v)| match v {
            Value::Bytes(b) => Some(std::mem::take(b)),
            _ => None,
        })
        .unwrap_or_default()
}

/// An HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// Headers in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    /// A response with `status` and no body.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A `text/plain` response.
    #[must_use]
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status)
            .header("content-type", "text/plain; charset=utf-8")
            .body(body.into().into_bytes())
    }

    /// An `application/json` response.
    ///
    /// # Errors
    ///
    /// If `value` does not serialise.
    pub fn json<T: Serialize + ?Sized>(status: u16, value: &T) -> Result<Self, MessageError> {
        let body = serde_json::to_vec(value).map_err(|e| MessageError(e.to_string()))?;
        Ok(Self::new(status)
            .header("content-type", "application/json")
            .body(body))
    }

    /// Add a header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// The response as the envelope the node expects.
    #[must_use]
    pub fn to_envelope(&self) -> Envelope {
        let wire = ResponseWire {
            status: self.status,
            headers: &self.headers,
            body: &self.body,
        };
        encode(&wire, self.body.len(), RESPONSE_SCHEMA)
    }

    /// The response an envelope carries, if it is one (for tests).
    #[must_use]
    pub fn from_envelope(msg: &Envelope) -> Option<Self> {
        if msg.schema.as_deref() != Some(RESPONSE_SCHEMA) {
            return None;
        }
        let mut value = decode(&msg.body)?;
        let body = take_bytes(&mut value, "body");
        let map = value.as_map()?;
        Some(Self {
            status: field(map, "status")
                .and_then(Value::as_integer)
                .and_then(|i| u16::try_from(i).ok())?,
            headers: pairs(field(map, "headers")),
            body,
        })
    }
}

impl Response {
    /// The HTTP response an app's reply to a request becomes, as the
    /// node's ingress makes it, for a driver app serving HTTP (ROADMAP
    /// DA2): a reply in the response format (`RESPONSE_SCHEMA`) gives its
    /// status (200 when it has none), headers and body; any other reply is
    /// `200` with the envelope's content type and its body. `None` for a
    /// response that does not decode, a status outside 100 to 999, or a
    /// header that is not a pair of texts: the ingress answers `502`
    /// then. Header names and values are not checked against HTTP's
    /// rules here; the caller's HTTP library does that.
    #[must_use]
    pub fn from_reply(msg: &Envelope) -> Option<Self> {
        if msg.schema.as_deref() != Some(RESPONSE_SCHEMA) {
            return Some(
                Self::new(200)
                    .header("content-type", msg.content_type.clone())
                    .body(msg.body.clone()),
            );
        }
        Self::from_reply_format(&msg.body)
    }

    /// [`Response::from_reply`], taking the reply: a body not in the
    /// response format is moved, not copied (ROADMAP PF1).
    #[must_use]
    pub fn from_reply_owned(msg: Envelope) -> Option<Self> {
        if msg.schema.as_deref() != Some(RESPONSE_SCHEMA) {
            return Some(
                Self::new(200)
                    .header("content-type", msg.content_type)
                    .body(msg.body),
            );
        }
        Self::from_reply_format(&msg.body)
    }

    /// A reply in the response format, its body moved out of the decoded
    /// map.
    fn from_reply_format(body: &[u8]) -> Option<Self> {
        let mut value = decode(body)?;
        let body = take_bytes(&mut value, "body");
        let map = value.as_map()?;
        let status = field(map, "status")
            .and_then(Value::as_integer)
            .and_then(|i| u16::try_from(i).ok())
            .unwrap_or(200);
        if !(100..=999).contains(&status) {
            return None;
        }
        let mut headers = Vec::new();
        if let Some(list) = field(map, "headers").and_then(Value::as_array) {
            for pair in list {
                let pair = pair.as_array()?;
                headers.push((
                    pair.first()?.as_text()?.to_string(),
                    pair.get(1)?.as_text()?.to_string(),
                ));
            }
        }
        Some(Self {
            status,
            headers,
            body,
        })
    }
}

/// The host event that carries the node's HTTP route table to a driver
/// app whose profile asks for it (`[drivers.<app>] routes = true`).
pub const ROUTES_EVENT: &str = "routes";

/// One route of the node's table, as the `routes` event has it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Route {
    /// The app that owns it, to call by name.
    pub app: String,
    /// Host to match, without port, any case; `None` matches any host.
    pub host: Option<String>,
    /// Path prefix, matched on segment boundaries.
    pub path_prefix: String,
    /// A header whose value becomes the request's `key`.
    pub hash_header: Option<String>,
    /// Largest body accepted, in bytes.
    pub max_body: u64,
    /// Time allowed for the reply, in milliseconds.
    pub timeout_ms: u64,
}

impl Route {
    /// Whether a request for `host` (without port) and `path` matches, as
    /// the node's ingress matches it.
    #[must_use]
    pub fn matches(&self, host: Option<&str>, path: &str) -> bool {
        let host_ok = match (&self.host, host) {
            (None, _) => true,
            (Some(want), Some(got)) => want.eq_ignore_ascii_case(got),
            (Some(_), None) => false,
        };
        let Some(rest) = path.strip_prefix(self.path_prefix.as_str()) else {
            return false;
        };
        host_ok && (self.path_prefix.ends_with('/') || rest.is_empty() || rest.starts_with('/'))
    }
}

/// The node's route table, from the `routes` event ([`ROUTES_EVENT`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct Routes {
    /// Every app's routes, in app order.
    pub routes: Vec<Route>,
}

impl Routes {
    /// The table a `routes` event's body holds.
    ///
    /// # Errors
    ///
    /// The body is not the event's JSON.
    pub fn from_event(body: &[u8]) -> Result<Self, MessageError> {
        serde_json::from_slice(body).map_err(|e| MessageError(e.to_string()))
    }

    /// The route a request for `host` (without port) and `path` goes to,
    /// as the node's ingress picks it: the longest prefix, then a route
    /// naming the host.
    #[must_use]
    pub fn pick(&self, host: Option<&str>, path: &str) -> Option<&Route> {
        self.routes
            .iter()
            .filter(|r| r.matches(host, path))
            .max_by_key(|r| (r.path_prefix.len(), r.host.is_some()))
    }
}

impl From<Response> for Envelope {
    fn from(r: Response) -> Self {
        r.to_envelope()
    }
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod http_tests;

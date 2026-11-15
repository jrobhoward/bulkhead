//! The node's call to an egress driver and its answer: schema
//! [`EGRESS_SCHEMA`], an [`EgressRequest`] in CBOR, answered with
//! `Result<HttpResponse, HttpError>` in CBOR (the protocol's types, as an
//! app's request and answer are). The node's side
//! (`bulkhead_host::services::egress`) keeps a copy of [`EgressRequest`]
//! with the same fields.

use bulkhead_native::protocol::{HttpError, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};

/// The schema of the node's calls and the driver's answers.
pub const EGRESS_SCHEMA: &str = "bulkhead/egress";

/// What the driver answers a call.
pub type Answer = Result<HttpResponse, HttpError>;

/// One request the node checked, with the timeouts it allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressRequest {
    /// The app's request; its `timeout_ms` is already folded into the two
    /// timeouts below.
    pub request: HttpRequest,
    /// The longest wait for the response's headers.
    pub first_byte_ms: u64,
    /// The longest wait between two chunks of its body.
    pub between_ms: u64,
}

//! Calls to the node's built-in packages, by name: since ROADMAP DA8 the
//! one package is Modbus's (`bulkhead-modbus:modbus`), whose client is
//! `bulkhead-modbus-client`. Any other package answers
//! [`Error::NoSuchExtension`]; a service of an app's own is an app called
//! with `Ctx::call_app` (`examples/greeter`).
//!
//! [`call`] names the package and the function (by convention
//! `interface#function`, `client#read-holding`), and carries the arguments
//! and the result in CBOR (an array of the parameters in order; the result
//! as serde encodes it). The app declares the package in its manifest and
//! the operator grants it.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Why an extension call was not made. The extension's own errors (a
/// denied grant, an unknown device) are in its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The node runs no extension serving this package.
    NoSuchExtension,
    /// The extension serves no such function.
    NoSuchFunction(String),
    /// The extension cannot read the arguments.
    BadArguments(String),
    /// The arguments would not encode, or the result does not decode as
    /// the type asked for.
    Encoding(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchExtension => f.write_str("the node runs no such extension"),
            Self::NoSuchFunction(name) => write!(f, "the extension has no function {name}"),
            Self::BadArguments(why) => write!(f, "bad arguments to {why}"),
            Self::Encoding(why) => write!(f, "encoding: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// Call `function` of the extension serving `package` with `args` (by
/// convention a tuple of the parameters), decoding its result as `R` (by
/// convention `Result<T, E>`). Only during a callback, as every request.
///
/// # Errors
///
/// As [`Error`] lists.
pub fn call<A: Serialize, R: DeserializeOwned>(
    package: &str,
    function: &str,
    args: &A,
) -> Result<R, Error> {
    use bulkhead_native::protocol::ExtensionError;
    let mut encoded = Vec::new();
    ciborium::into_writer(args, &mut encoded).map_err(|e| Error::Encoding(e.to_string()))?;
    let result = bulkhead_native::app::Ctx::current()
        .extension(package, function, encoded)
        .map_err(|e| match e {
            ExtensionError::NoSuchExtension => Error::NoSuchExtension,
            ExtensionError::NoSuchFunction(name) => Error::NoSuchFunction(name),
            ExtensionError::BadArguments(why) => Error::BadArguments(why),
        })?;
    ciborium::from_reader(result.as_slice()).map_err(|e| Error::Encoding(e.to_string()))
}

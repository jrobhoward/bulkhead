//! The init arguments the node writes for one datasource's driver (JSON):
//!
//! ```json
//! {"datasource": "main", "host": "db.internal", "transport": "tcp",
//!  "dbname": "shop", "user": "shop",
//!  "password": {"file": "/driver/main.password"},
//!  "tls": {"require": {"ca_file": "/driver/main-ca.pem"}},
//!  "pool_size": 10, "connect_timeout_ms": 5000, "acquire_timeout_ms": 5000,
//!  "statement_timeout_ms": 30000, "max_result_bytes": 16777216}
//! ```
//!
//! The node connects each stream itself (`Request::Connect`) to the
//! datasource's address, so the host here only names the server for TLS
//! and for messages. Files are paths under `/driver/`, which the node
//! lets the driver read; a password from the node's environment comes as
//! `{"value": "..."}` instead.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How the node's streams reach the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// TCP.
    Tcp,
    /// The server's Unix socket.
    Unix,
}

/// Where the password comes from.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Password {
    /// A file under `/driver/`, its trailing newline not part of it.
    File(String),
    /// The password itself: a node secret from its environment.
    Value(String),
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Value(_) => f.write_str("Value(..)"),
        }
    }
}

/// TLS to the server.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Tls {
    /// None.
    #[default]
    Disable,
    /// Required, the server's certificate checked against `ca_file`.
    Require {
        /// The CA certificates (PEM), a path under `/driver/`.
        ca_file: String,
    },
}

/// The driver's init arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The datasource's name, for messages.
    pub datasource: String,
    /// The server's host name, for TLS (and messages).
    pub host: String,
    /// TCP or the Unix socket.
    pub transport: Transport,
    /// The database.
    pub dbname: String,
    /// The user.
    pub user: String,
    /// The password, if the server asks for one.
    #[serde(default)]
    pub password: Option<Password>,
    /// TLS.
    #[serde(default)]
    pub tls: Tls,
    /// Most connections open at once.
    pub pool_size: usize,
    /// Time to open a connection: the node's connect, TLS and login.
    pub connect_timeout_ms: u64,
    /// Time a statement waits for a free connection.
    pub acquire_timeout_ms: u64,
    /// The server cancels a statement that runs longer.
    pub statement_timeout_ms: u64,
    /// The largest result answered, in bytes.
    pub max_result_bytes: u64,
}

impl Config {
    /// The arguments the node wrote.
    ///
    /// # Errors
    ///
    /// Not this format, a pool of no connections, or TLS over a Unix
    /// socket (Postgres serves none there).
    pub fn from_args(args: &[u8]) -> Result<Self, String> {
        let config: Self =
            serde_json::from_slice(args).map_err(|e| format!("init arguments: {e}"))?;
        if config.pool_size == 0 {
            return Err("init arguments: pool_size is 0".into());
        }
        if config.transport == Transport::Unix && config.tls != Tls::Disable {
            return Err("init arguments: TLS over a Unix socket".into());
        }
        Ok(config)
    }

    /// [`Config::connect_timeout_ms`].
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    /// [`Config::acquire_timeout_ms`].
    #[must_use]
    pub fn acquire_timeout(&self) -> Duration {
        Duration::from_millis(self.acquire_timeout_ms)
    }
}

/// A password file's contents without its trailing newline.
#[must_use]
pub fn trim_password(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|b| *b != b'\n' && *b != b'\r')
        .map_or(0, |i| i + 1);
    &bytes[..end]
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

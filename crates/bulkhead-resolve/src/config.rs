//! The init arguments the node writes, and hickory's configuration from
//! them: the nameservers the node lets the driver reach, over TCP only,
//! with the search domains and options of the system's `resolv.conf`.

use std::net::SocketAddr;

use hickory_resolver::Hosts;
use hickory_resolver::config::{
    ConnectionConfig, NameServerConfig, ResolveHosts, ResolverConfig, ResolverOpts,
};
use serde::{Deserialize, Serialize};

/// The most nameservers the node passes.
pub const MAX_NAMESERVERS: usize = 8;

/// The driver's init arguments (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The nameservers, with their ports: `resolv.conf`'s on port 53, or
    /// node config's `[resolver] nameservers`. The node's connect rules
    /// allow these and nothing else.
    pub nameservers: Vec<SocketAddr>,
    /// The contents of `resolv.conf`: its search domains, `ndots`,
    /// `timeout` and `attempts` (its nameservers are the list above).
    #[serde(default)]
    pub resolv_conf: String,
    /// The contents of `hosts`.
    #[serde(default)]
    pub hosts: String,
}

impl Config {
    /// The arguments the node wrote.
    ///
    /// # Errors
    ///
    /// Not this format, no nameserver, or more than [`MAX_NAMESERVERS`].
    pub fn from_args(args: &[u8]) -> Result<Self, String> {
        let config: Self =
            serde_json::from_slice(args).map_err(|e| format!("init arguments: {e}"))?;
        if config.nameservers.is_empty() {
            return Err("init arguments: no nameservers".into());
        }
        if config.nameservers.len() > MAX_NAMESERVERS {
            return Err(format!(
                "init arguments: {} nameservers, at most {MAX_NAMESERVERS}",
                config.nameservers.len()
            ));
        }
        Ok(config)
    }

    /// hickory's configuration and options: each nameserver over TCP alone,
    /// on its port; `resolv.conf`'s domain, search list and options; the
    /// hosts file never read from disk ([`Config::hosts`] instead).
    ///
    /// # Errors
    ///
    /// `resolv.conf` names a search domain hickory cannot read.
    pub fn resolver(&self) -> Result<(ResolverConfig, ResolverOpts), String> {
        // hickory refuses a file without nameservers; the node's list
        // replaces the file's in any case.
        let text = format!("{}\nnameserver 127.0.0.1\n", self.resolv_conf);
        let (parsed, mut options) = hickory_resolver::system_conf::parse_resolv_conf(text)
            .map_err(|e| format!("resolv.conf: {e}"))?;
        let servers = self
            .nameservers
            .iter()
            .map(|addr| {
                let mut tcp = ConnectionConfig::tcp();
                tcp.port = addr.port();
                let mut server = NameServerConfig::udp_and_tcp(addr.ip());
                server.connections = vec![tcp];
                server
            })
            .collect();
        options.use_hosts_file = ResolveHosts::Never;
        options.try_tcp_on_error = false;
        let config =
            ResolverConfig::from_parts(parsed.domain().cloned(), parsed.search().to_vec(), servers);
        Ok((config, options))
    }

    /// The hosts file's names.
    ///
    /// # Errors
    ///
    /// None in practice: hickory skips lines it cannot read.
    pub fn hosts(&self) -> Result<Hosts, String> {
        let mut hosts = Hosts::default();
        hosts
            .read_hosts_conf(self.hosts.as_bytes())
            .map_err(|e| format!("hosts: {e}"))?;
        Ok(hosts)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

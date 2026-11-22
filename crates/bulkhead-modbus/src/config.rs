//! The node configuration: `[extensions."bulkhead-modbus:modbus"]`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use serde::Deserialize;

/// Most registers one read request carries (Modbus application protocol,
/// 6.3 and 6.4).
pub(crate) const MAX_READ_REGISTERS: u16 = 125;
/// Most coils or discrete inputs one read request carries.
pub(crate) const MAX_READ_BITS: u16 = 2000;
/// Most registers one write request carries.
pub(crate) const MAX_WRITE_REGISTERS: usize = 123;
/// Most coils one write request carries.
pub(crate) const MAX_WRITE_BITS: usize = 1968;

/// The extension's node configuration.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Devices the node polls, by name.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceConfig>,
    /// A Modbus TCP server the node runs, if any.
    #[serde(default)]
    pub server: Option<ServerConfig>,
}

/// One device: `[extensions."bulkhead-modbus:modbus".devices.<name>]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    /// `ip:port`.
    pub address: SocketAddr,
    /// The unit (slave) id.
    #[serde(default = "default_unit")]
    pub unit: u8,
    /// Time between polls.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// Time allowed for connecting and for each request.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Send every point, changed or not, this often (an integrity scan).
    /// Unset sends every point only after a connect or a failed delivery.
    #[serde(default)]
    pub full_every_ms: Option<u64>,
    /// The app updates are cast to. Unset polls nothing; guests can still
    /// read and write the device.
    #[serde(default)]
    pub deliver_to: Option<String>,
    /// Poll only while this node runs singletons: on an HA pair, the
    /// primary while its lease holds, so one node polls and a `cluster`
    /// bucket carries the table to the other. The standby closes its
    /// connection; guests' reads and writes still reach the device.
    #[serde(default)]
    pub primary_only: bool,
    /// Holding registers polled.
    #[serde(default)]
    pub holding: Vec<Range>,
    /// Input registers polled.
    #[serde(default)]
    pub input: Vec<Range>,
    /// Coils polled.
    #[serde(default)]
    pub coils: Vec<Range>,
    /// Discrete inputs polled.
    #[serde(default)]
    pub discrete: Vec<Range>,
    /// Typed values read from the polled registers and bits (ROADMAP
    /// DA5b), by name. A register a tag covers is delivered as the tag
    /// only, not as a raw point.
    #[serde(default)]
    pub tags: BTreeMap<String, Tag>,
}

/// A typed value: `[extensions."bulkhead-modbus:modbus".devices.<d>.tags.<name>]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tag {
    /// The table it is read from.
    pub table: crate::points::Table,
    /// Its first register, or its bit's address.
    pub address: u16,
    /// How its registers are read; `u16` for registers and `bool` for
    /// coils and discrete inputs when unset.
    #[serde(default, rename = "type")]
    pub kind: Option<TagType>,
    /// The byte order of a 32- or 64-bit value's registers.
    #[serde(default)]
    pub order: Option<WordOrder>,
    /// With `type = "bool"` on a register: which of its bits, 0 to 15.
    #[serde(default)]
    pub bit: Option<u8>,
    /// The value is `raw * scale + offset`.
    #[serde(default = "one")]
    pub scale: f64,
    /// Added after `scale`.
    #[serde(default)]
    pub offset: f64,
    /// A new value is sent only when it differs from the last one sent by
    /// at least this; 0 sends every change.
    #[serde(default)]
    pub deadband: f64,
}

/// How a tag's registers are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagType {
    /// One register, unsigned.
    U16,
    /// One register, two's complement.
    I16,
    /// Two registers, unsigned.
    U32,
    /// Two registers, two's complement.
    I32,
    /// Two registers, IEEE 754.
    F32,
    /// Four registers, unsigned.
    U64,
    /// Four registers, two's complement.
    I64,
    /// Four registers, IEEE 754.
    F64,
    /// A coil, a discrete input, or one bit of a register.
    Bool,
}

impl TagType {
    /// How many registers a value takes.
    #[must_use]
    pub fn registers(self) -> u16 {
        match self {
            Self::U16 | Self::I16 | Self::Bool => 1,
            Self::U32 | Self::I32 | Self::F32 => 2,
            Self::U64 | Self::I64 | Self::F64 => 4,
        }
    }
}

/// The byte order of a value across registers, naming its bytes `a` (the
/// most significant) to `d` for 32 bits; 64-bit values pair their bytes
/// alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WordOrder {
    /// Big-endian: the first register holds the most significant word, high
    /// byte first (Modbus's own order).
    #[default]
    Abcd,
    /// Word-swapped: the first register holds the least significant word.
    Cdab,
    /// Byte-swapped within each register, words big-endian.
    Badc,
    /// Little-endian: words and bytes reversed.
    Dcba,
}

impl Tag {
    /// Its type, with the default for its table.
    #[must_use]
    pub fn kind(&self) -> TagType {
        self.kind.unwrap_or(if self.table.is_bits() {
            TagType::Bool
        } else {
            TagType::U16
        })
    }

    /// The registers (or the bit) it reads.
    #[must_use]
    pub fn span(&self) -> std::ops::Range<u32> {
        let start = u32::from(self.address);
        start..start + u32::from(self.kind().registers())
    }

    /// What is wrong with it, if anything.
    fn problem(&self, device: &DeviceConfig) -> Option<String> {
        let kind = self.kind();
        let bits = self.table.is_bits();
        if bits && (kind != TagType::Bool || self.bit.is_some()) {
            return Some("a coil or discrete input is a bool, without bit".into());
        }
        if self.bit.is_some_and(|b| b > 15) {
            return Some("bit must be 0 to 15".into());
        }
        if self.bit.is_some() && kind != TagType::Bool {
            return Some("bit goes with type = \"bool\"".into());
        }
        if !bits && kind == TagType::Bool && self.bit.is_none() {
            return Some("a bool in a register names its bit".into());
        }
        if self.order.is_some() && kind.registers() < 2 {
            return Some("order is for 32- and 64-bit types".into());
        }
        if !self.scale.is_finite() || self.scale == 0.0 {
            return Some("scale must be a finite number other than 0".into());
        }
        if !self.offset.is_finite() {
            return Some("offset must be a finite number".into());
        }
        if !self.deadband.is_finite() || self.deadband < 0.0 {
            return Some("deadband must be a finite number of at least 0".into());
        }
        let ranges = match self.table {
            crate::points::Table::Holding => &device.holding,
            crate::points::Table::Input => &device.input,
            crate::points::Table::Coil => &device.coils,
            crate::points::Table::Discrete => &device.discrete,
        };
        let span = self.span();
        let polled = ranges.iter().any(|r| {
            let start = u32::from(r.start);
            start <= span.start && span.end <= start + u32::from(r.count)
        });
        if !polled {
            return Some(format!(
                "{:?} {} to {} is not inside one of the device's polled ranges",
                self.table,
                span.start,
                span.end - 1
            ));
        }
        None
    }
}

fn one() -> f64 {
    1.0
}

/// A tag's name: lowercase letters, digits, `_` and `-`, 1 to 64 of them.
fn is_tag_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The first problem with a device's tags.
fn tags_problem(name: &str, device: &DeviceConfig) -> Option<String> {
    let mut words: BTreeMap<(crate::points::Table, u32), (&str, bool)> = BTreeMap::new();
    for (tag_name, tag) in &device.tags {
        let at = |why: String| Some(format!("devices.{name}.tags.{tag_name}: {why}"));
        if !is_tag_name(tag_name) {
            return at("a name of lowercase letters, digits, _ and -".into());
        }
        if let Some(why) = tag.problem(device) {
            return at(why);
        }
        // Several bit tags may share a register; nothing else shares.
        let bit = tag.bit.is_some();
        for word in tag.span() {
            if let Some((other, other_bit)) = words.insert((tag.table, word), (tag_name, bit))
                && !(bit && other_bit)
            {
                return at(format!("shares {:?} {word} with tag {other}", tag.table));
            }
        }
    }
    None
}

/// A run of addresses: `{ start = 0, count = 10 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    /// First address.
    pub start: u16,
    /// How many.
    pub count: u16,
}

/// The server: `[extensions."bulkhead-modbus:modbus".server]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// `ip:port` to listen on.
    pub listen: SocketAddr,
    /// The `device` name its write messages carry.
    #[serde(default = "default_server_name")]
    pub name: String,
    /// The app masters' writes are cast to. Unset keeps them in the tables
    /// only.
    #[serde(default)]
    pub deliver_to: Option<String>,
    /// Holding registers served, from address 0.
    #[serde(default)]
    pub holding: u16,
    /// Input registers served.
    #[serde(default)]
    pub input: u16,
    /// Coils served.
    #[serde(default)]
    pub coils: u16,
    /// Discrete inputs served.
    #[serde(default)]
    pub discrete: u16,
    /// Masters allowed to connect: addresses (`10.0.0.5`) or networks
    /// (`10.0.0.0/24`). Modbus has no authentication of its own, so the
    /// default is loopback only; list the SCADA network to serve it.
    #[serde(default = "default_allow")]
    pub allow: Vec<String>,
    /// Most masters connected at once; more are closed on accept.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

fn default_max_connections() -> usize {
    32
}

fn default_allow() -> Vec<String> {
    vec!["127.0.0.0/8".into(), "::1/128".into()]
}

impl ServerConfig {
    /// The `allow` list as networks. Entries are checked by
    /// [`Config::from_json`], so none is dropped here.
    #[must_use]
    pub fn allowed(&self) -> Vec<(std::net::IpAddr, u8)> {
        self.allow.iter().filter_map(|a| network(a)).collect()
    }
}

/// An address or a network, as a network.
fn network(entry: &str) -> Option<(std::net::IpAddr, u8)> {
    match entry.parse::<std::net::IpAddr>() {
        Ok(ip) => Some((ip, if ip.is_ipv4() { 32 } else { 128 })),
        Err(_) => crate::cidr::parse(entry),
    }
}

fn default_unit() -> u8 {
    1
}

fn default_poll_ms() -> u64 {
    1_000
}

fn default_timeout_ms() -> u64 {
    1_000
}

fn default_server_name() -> String {
    "server".to_string()
}

impl DeviceConfig {
    /// Time between polls.
    #[must_use]
    pub fn poll(&self) -> Duration {
        Duration::from_millis(self.poll_ms.max(1))
    }

    /// Time allowed per request.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.max(1))
    }
}

impl Config {
    /// Parse the configuration the node hands the extension (null when it
    /// has none).
    ///
    /// # Errors
    ///
    /// A message naming the field at fault.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let config: Self = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        for (name, device) in &config.devices {
            let ranges = [
                ("holding", &device.holding, MAX_READ_REGISTERS),
                ("input", &device.input, MAX_READ_REGISTERS),
                ("coils", &device.coils, MAX_READ_BITS),
                ("discrete", &device.discrete, MAX_READ_BITS),
            ];
            for (table, list, max) in ranges {
                for r in list {
                    if r.count == 0 || r.count > max {
                        return Err(format!(
                            "devices.{name}.{table}: count {} must be 1 to {max}",
                            r.count
                        ));
                    }
                    if u32::from(r.start) + u32::from(r.count) > 65_536 {
                        return Err(format!(
                            "devices.{name}.{table}: start {} + count {} runs past address 65535",
                            r.start, r.count
                        ));
                    }
                }
            }
            if let Some(why) = tags_problem(name, device) {
                return Err(why);
            }
            if device.deliver_to.is_none() && ranges.iter().any(|(_, list, _)| !list.is_empty()) {
                return Err(format!(
                    "devices.{name}: polls registers but names no deliver_to app"
                ));
            }
        }
        if let Some(server) = &config.server
            && let Some(bad) = server.allow.iter().find(|a| network(a).is_none())
        {
            return Err(format!(
                "server.allow: '{bad}' is not an address or a network (10.0.0.0/24)"
            ));
        }
        if config
            .server
            .as_ref()
            .is_some_and(|s| s.max_connections == 0)
        {
            return Err("server.max_connections must be more than 0".into());
        }
        Ok(config)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;

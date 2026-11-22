//! Point values, what changed between scans, and the messages apps get.
//!
//! An update is one CBOR cast per device and scan, carrying only the points
//! that changed (every point when `full`), so a pool routed with
//! `hash:device` keeps each device on one instance and pays one message per
//! scan, not per point:
//!
//! ```text
//! { "device": "pump-1", "kind": "points", "full": false,
//!   "points": [ { "table": "holding", "address": 3, "value": 17 } ] }
//! { "device": "pump-1", "kind": "offline", "error": "connection refused" }
//! { "device": "server", "kind": "written",
//!   "points": [ { "table": "holding", "address": 0, "value": 5 } ] }
//! ```
//!
//! Coils and discrete inputs carry 0 or 1.
//!
//! A device's tags (ROADMAP DA5b) are its typed values: each read from its
//! registers or bit, with its type, word order, scale and offset, and sent
//! in `tags` when it moved by at least its deadband since the value last
//! sent. A register a tag covers is not sent as a raw point:
//!
//! ```text
//! { "device": "pump-1", "kind": "points", "full": false, "points": [],
//!   "tags": [ { "name": "flow", "value": 12.5 } ] }
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::config::{Tag, TagType, WordOrder};

/// A Modbus table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Table {
    /// Holding registers (read-write words).
    Holding,
    /// Input registers (read-only words).
    Input,
    /// Coils (read-write bits).
    Coil,
    /// Discrete inputs (read-only bits).
    Discrete,
}

impl Table {
    /// Whether it holds bits (coils, discrete inputs), not registers.
    #[must_use]
    pub fn is_bits(self) -> bool {
        matches!(self, Self::Coil | Self::Discrete)
    }
}

/// One point's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// Its table.
    pub table: Table,
    /// Its address.
    pub address: u16,
    /// Its value; 0 or 1 for coils and discrete inputs.
    pub value: u16,
}

/// What kind of message an [`Update`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A scan's points.
    Points,
    /// The device stopped answering; `error` says why. Sent once per
    /// outage; the next `points` after it is `full`.
    Offline,
    /// A master wrote to the node's server.
    Written,
}

/// A message to an app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Update {
    /// The device's name (the server's `name` for `written`): the routing
    /// key.
    pub device: String,
    /// What this is.
    pub kind: Kind,
    /// Whether `points` holds every polled point, not only changes.
    #[serde(default)]
    pub full: bool,
    /// The points.
    #[serde(default)]
    pub points: Vec<Point>,
    /// Why the device is offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The device's tags that changed (every tag when `full`); absent for a
    /// device without tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<TagPoint>,
}

/// A tag's value: an integer for an integer type without scale or offset,
/// a float otherwise, or a bool.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TagValue {
    /// A bool tag.
    Bool(bool),
    /// An unsigned integer.
    Unsigned(u64),
    /// A signed integer.
    Signed(i64),
    /// A float, or a scaled integer.
    Float(f64),
}

impl TagValue {
    #[allow(clippy::cast_precision_loss)]
    fn as_f64(self) -> f64 {
        match self {
            Self::Bool(b) => f64::from(u8::from(b)),
            Self::Unsigned(n) => n as f64,
            Self::Signed(n) => n as f64,
            Self::Float(x) => x,
        }
    }

    /// Whether `self` is far enough from `last` to send.
    fn moved_from(self, last: Self, deadband: f64) -> bool {
        match (self, last) {
            (Self::Bool(a), Self::Bool(b)) => a != b,
            (Self::Float(a), Self::Float(b)) if a.is_nan() || b.is_nan() => {
                a.is_nan() != b.is_nan()
            }
            _ if deadband > 0.0 => (self.as_f64() - last.as_f64()).abs() >= deadband,
            _ => self != last,
        }
    }
}

/// One tag's value in an update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagPoint {
    /// The tag's name.
    pub name: String,
    /// Its value.
    pub value: TagValue,
}

impl Update {
    /// The CBOR body.
    #[must_use]
    pub fn to_cbor(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // Writing plain data to a Vec cannot fail.
        let _ = ciborium::into_writer(self, &mut out);
        out
    }
}

/// One scan's values, by table and address.
pub type Scan = BTreeMap<(Table, u16), u16>;

/// The points in `now` that differ from `before`, or all of them when
/// `full`.
#[must_use]
pub fn changes(before: &Scan, now: &Scan, full: bool) -> Vec<Point> {
    now.iter()
        .filter(|(at, value)| full || before.get(at) != Some(value))
        .map(|(&(table, address), &value)| Point {
            table,
            address,
            value,
        })
        .collect()
}

/// The registers and bits a device's tags cover, which go out as tags only.
#[must_use]
pub fn covered(tags: &BTreeMap<String, Tag>) -> BTreeSet<(Table, u16)> {
    tags.values()
        .flat_map(|t| {
            t.span()
                .filter_map(move |a| Some((t.table, u16::try_from(a).ok()?)))
        })
        .collect()
}

/// The bytes of `words` in big-endian order, from the registers as `order`
/// lays them out.
fn bytes(words: &[u16], order: WordOrder) -> Vec<u8> {
    let swap_words = matches!(order, WordOrder::Cdab | WordOrder::Dcba);
    let swap_bytes = matches!(order, WordOrder::Badc | WordOrder::Dcba);
    let mut ordered: Vec<u16> = words.to_vec();
    if swap_words {
        ordered.reverse();
    }
    ordered
        .into_iter()
        .flat_map(|w| {
            let [hi, lo] = w.to_be_bytes();
            if swap_bytes { [lo, hi] } else { [hi, lo] }
        })
        .collect()
}

/// `tag`'s value in `scan`, or `None` while a register it needs is missing.
#[must_use]
pub fn decode(tag: &Tag, scan: &Scan) -> Option<TagValue> {
    let kind = tag.kind();
    let words: Vec<u16> = tag
        .span()
        .map(|a| scan.get(&(tag.table, u16::try_from(a).ok()?)).copied())
        .collect::<Option<_>>()?;
    if kind == TagType::Bool {
        let w = words[0];
        let on = match tag.bit {
            Some(bit) => (w >> bit) & 1 == 1,
            None => w != 0,
        };
        return Some(TagValue::Bool(on));
    }
    let b = bytes(&words, tag.order.unwrap_or_default());
    let raw = match kind {
        TagType::U16 => TagValue::Unsigned(u64::from(words[0])),
        TagType::I16 => TagValue::Signed(i64::from(i16::from_be_bytes([b[0], b[1]]))),
        TagType::U32 => TagValue::Unsigned(u64::from(u32::from_be_bytes(b[..4].try_into().ok()?))),
        TagType::I32 => TagValue::Signed(i64::from(i32::from_be_bytes(b[..4].try_into().ok()?))),
        TagType::F32 => TagValue::Float(f64::from(f32::from_be_bytes(b[..4].try_into().ok()?))),
        TagType::U64 => TagValue::Unsigned(u64::from_be_bytes(b[..8].try_into().ok()?)),
        TagType::I64 => TagValue::Signed(i64::from_be_bytes(b[..8].try_into().ok()?)),
        TagType::F64 => TagValue::Float(f64::from_be_bytes(b[..8].try_into().ok()?)),
        TagType::Bool => return None,
    };
    #[allow(clippy::float_cmp)]
    let plain = tag.scale == 1.0 && tag.offset == 0.0;
    Some(if plain {
        raw
    } else {
        TagValue::Float(raw.as_f64() * tag.scale + tag.offset)
    })
}

/// The tags whose value in `now` moved by at least their deadband from the
/// value last sent (every tag when `full`), recording them in `last_sent`.
pub fn tag_changes(
    tags: &BTreeMap<String, Tag>,
    last_sent: &mut BTreeMap<String, TagValue>,
    now: &Scan,
    full: bool,
) -> Vec<TagPoint> {
    let mut out = Vec::new();
    for (name, tag) in tags {
        let Some(value) = decode(tag, now) else {
            continue;
        };
        let send = full
            || last_sent
                .get(name)
                .is_none_or(|last| value.moved_from(*last, tag.deadband));
        if send {
            last_sent.insert(name.clone(), value);
            out.push(TagPoint {
                name: name.clone(),
                value,
            });
        }
    }
    out
}

/// Record words read from `start` into `scan`.
pub(crate) fn record_words(scan: &mut Scan, table: Table, start: u16, words: &[u16]) {
    for (address, value) in (start..=u16::MAX).zip(words) {
        scan.insert((table, address), *value);
    }
}

/// Record bits read from `start` into `scan`, as 0 or 1.
pub(crate) fn record_bits(scan: &mut Scan, table: Table, start: u16, bits: &[bool]) {
    for (address, value) in (start..=u16::MAX).zip(bits) {
        scan.insert((table, address), u16::from(*value));
    }
}

#[cfg(test)]
#[path = "points_tests.rs"]
mod points_tests;

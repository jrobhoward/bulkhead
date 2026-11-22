//! The manifest's socket rules (`bulkhead:sockets`): `connect` and `bind`,
//! each a list of `{ cidr, ports }`, and the node's address guard:
//! loopback, link-local and unspecified destinations are reached only
//! through a rule inside that range, so `0.0.0.0/0` does not reach the
//! node itself (SECURITY.md, "App sockets").

use std::net::IpAddr;

use serde::Deserialize;

/// A rules file, in the manifest's syntax.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Rules {
    #[serde(default)]
    pub connect: Vec<RawRule>,
    #[serde(default)]
    pub bind: Vec<RawRule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawRule {
    pub cidr: String,
    pub ports: Option<String>,
}

/// An address block, IPv4 or IPv6, as network-order bytes and a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Net {
    V4([u8; 4], u8),
    V6([u8; 16], u8),
}

/// One rule: a block and an inclusive port range (`None`: any port).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub net: Net,
    pub ports: Option<(u16, u16)>,
}

/// The ranges the guard keeps out of broader rules.
pub const SPECIAL: [Net; 6] = [
    Net::V4([127, 0, 0, 0], 8),
    Net::V4([169, 254, 0, 0], 16),
    Net::V4([0, 0, 0, 0], 8),
    Net::V6([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], 128),
    Net::V6([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 10),
    Net::V6([0; 16], 128),
];

impl Rules {
    pub fn read(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        toml::from_str(&text).map_err(|e| format!("{path}: {e}"))
    }

    pub fn connect(&self) -> Result<Vec<Rule>, String> {
        self.connect.iter().map(RawRule::parse).collect()
    }

    pub fn bind(&self) -> Result<Vec<Rule>, String> {
        self.bind.iter().map(RawRule::parse).collect()
    }
}

impl RawRule {
    pub fn parse(&self) -> Result<Rule, String> {
        Ok(Rule {
            net: Net::parse(&self.cidr)?,
            ports: self.ports.as_deref().map(parse_ports).transpose()?,
        })
    }
}

fn parse_ports(s: &str) -> Result<(u16, u16), String> {
    let bad = || format!("bad ports {s:?}");
    let (lo, hi) = s.split_once('-').unwrap_or((s, s));
    let lo: u16 = lo.trim().parse().map_err(|_| bad())?;
    let hi: u16 = hi.trim().parse().map_err(|_| bad())?;
    if lo > hi {
        return Err(bad());
    }
    Ok((lo, hi))
}

impl Net {
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("bad cidr {s:?}");
        let (addr, len) = s.split_once('/').ok_or_else(bad)?;
        let len: u8 = len.parse().map_err(|_| bad())?;
        match addr.parse::<IpAddr>().map_err(|_| bad())? {
            IpAddr::V4(a) if len <= 32 => {
                Ok(Self::V4(mask(&a.octets(), len).try_into().unwrap(), len))
            }
            IpAddr::V6(a) if len <= 128 => {
                Ok(Self::V6(mask(&a.octets(), len).try_into().unwrap(), len))
            }
            _ => Err(bad()),
        }
    }

    pub fn is_v4(self) -> bool {
        matches!(self, Self::V4(..))
    }

    pub fn len(self) -> u8 {
        match self {
            Self::V4(_, l) | Self::V6(_, l) => l,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::V4(b, _) => b,
            Self::V6(b, _) => b,
        }
    }

    /// The prefix's mask, as bytes in network order.
    pub fn mask_bytes(self) -> Vec<u8> {
        let n = self.bytes().len();
        (0..n)
            .map(|i| {
                let bits = (i32::from(self.len()) - 8 * i as i32).clamp(0, 8);
                if bits == 0 { 0 } else { 0xffu8 << (8 - bits) }
            })
            .collect()
    }

    /// Whether `other` lies wholly inside this block.
    pub fn contains(self, other: Self) -> bool {
        self.is_v4() == other.is_v4()
            && other.len() >= self.len()
            && mask(other.bytes(), self.len()) == self.bytes()
    }

    pub fn contains_addr(self, addr: IpAddr) -> bool {
        self.contains(Self::of(addr))
    }

    /// `addr` as a block of one address.
    pub fn of(addr: IpAddr) -> Self {
        match addr {
            IpAddr::V4(a) => Self::V4(a.octets(), 32),
            IpAddr::V6(a) => Self::V6(a.octets(), 128),
        }
    }

    /// As text, `a.b.c.d/n` or `x::y/n`.
    pub fn text(self) -> String {
        match self {
            Self::V4(b, l) => format!("{}/{l}", std::net::Ipv4Addr::from(b)),
            Self::V6(b, l) => format!("{}/{l}", std::net::Ipv6Addr::from(b)),
        }
    }
}

/// `bytes` with every bit past the first `len` cleared.
fn mask(bytes: &[u8], len: u8) -> Vec<u8> {
    bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let bits = (i32::from(len) - 8 * i as i32).clamp(0, 8);
            if bits == 0 {
                0
            } else {
                b & (0xffu8 << (8 - bits))
            }
        })
        .collect()
}

impl Rule {
    /// The special ranges this rule must not reach: those it covers
    /// without lying inside.
    pub fn guarded(self) -> Vec<Net> {
        SPECIAL
            .into_iter()
            .filter(|s| self.net.contains(*s) && !s.contains(self.net))
            .collect()
    }

    pub fn port_ok(self, port: u16) -> bool {
        self.ports.is_none_or(|(lo, hi)| (lo..=hi).contains(&port))
    }
}

/// Whether `rules` allow `addr:port`; `guard` for connect and send rules.
/// An IPv4-mapped IPv6 address is checked as its IPv4 address, as the
/// kernel sends it. The reference the generated fences are tested against.
pub fn allows(rules: &[Rule], addr: IpAddr, port: u16, guard: bool) -> bool {
    let addr = match addr {
        IpAddr::V6(a) => a.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        v4 => v4,
    };
    rules.iter().any(|r| {
        r.net.contains_addr(addr)
            && r.port_ok(port)
            && (!guard || !r.guarded().iter().any(|s| s.contains_addr(addr)))
    })
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod rules_tests;

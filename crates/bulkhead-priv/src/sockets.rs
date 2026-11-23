//! Own sockets (ROADMAP AP2, spike S40): an app's own TCP and UDP sockets
//! held by the kernel to the rules in the helper's configuration
//! (`own_connect.<app>`, `own_bind.<app>`), the same `{ cidr, ports }`
//! rules as the manifest's `bulkhead:sockets`, with the node's address
//! guard: loopback, link-local and unspecified destinations are reached
//! only through a rule inside that range.
//!
//! On Linux, cgroup BPF programs ([`programs`]) the helper attaches to
//! the app's cgroup at start, before its own seccomp filter refuses
//! `bpf`: one `BPF_PROG_TYPE_CGROUP_SOCK_ADDR` program per hook, plain
//! instructions with no helper calls, returning 1 to allow the call and 0
//! to refuse it (`EPERM`). On FreeBSD, ipfw rules matching the instance's
//! jail ([`ipfw`]).
//!
//! The programs read `struct bpf_sock_addr`: `user_ip4` at 4,
//! `user_ip6[4]` at 8 and `user_port` at 24, each 32 bits in network
//! byte order. A 32-bit load gives the bytes in host order, so a program
//! compares against `u32::from_ne_bytes` of the rule's bytes, and turns
//! the port into host order with `be16`, on little-endian machines (the
//! systems Bulkhead runs on).

use std::fmt;
use std::net::IpAddr;

/// An address block, IPv4 or IPv6: network-order bytes, host bits clear,
/// and a prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Net {
    /// An IPv4 block.
    V4([u8; 4], u8),
    /// An IPv6 block.
    V6([u8; 16], u8),
}

/// One rule: a block and an inclusive port range (`None`: any port).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rule {
    /// The block.
    pub net: Net,
    /// The ports, inclusive.
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

/// IPv4-mapped IPv6 addresses, which travel as IPv4.
const MAPPED: Net = Net::V6([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0], 96);

/// `bytes` with every bit past the first `len` cleared.
fn masked(bytes: &[u8], len: u8) -> Vec<u8> {
    bytes
        .iter()
        .zip(mask(bytes.len(), len))
        .map(|(b, m)| b & m)
        .collect()
}

/// A prefix of `len` bits as `n` mask bytes.
fn mask(n: usize, len: u8) -> Vec<u8> {
    (0..n)
        .map(|i| {
            let bits = (usize::from(len).saturating_sub(8 * i)).min(8);
            if bits == 0 { 0 } else { 0xffu8 << (8 - bits) }
        })
        .collect()
}

impl Net {
    /// Parse `addr/len`, clearing host bits.
    ///
    /// # Errors
    ///
    /// Not a block of either family.
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("{s} is not an address block such as 10.0.0.0/8");
        let (addr, len) = s.split_once('/').ok_or_else(bad)?;
        let len: u8 = len.parse().map_err(|_| bad())?;
        match addr.parse::<IpAddr>().map_err(|_| bad())? {
            IpAddr::V4(a) if len <= 32 => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&masked(&a.octets(), len));
                Ok(Self::V4(b, len))
            }
            IpAddr::V6(a) if len <= 128 => {
                let mut b = [0u8; 16];
                b.copy_from_slice(&masked(&a.octets(), len));
                Ok(Self::V6(b, len))
            }
            _ => Err(bad()),
        }
    }

    /// Whether this is an IPv4 block.
    #[must_use]
    pub const fn is_v4(self) -> bool {
        matches!(self, Self::V4(..))
    }

    /// The prefix length.
    #[must_use]
    pub const fn prefix(self) -> u8 {
        match self {
            Self::V4(_, l) | Self::V6(_, l) => l,
        }
    }

    /// The block's bytes, network order.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::V4(b, _) => b,
            Self::V6(b, _) => b,
        }
    }

    /// Whether `other` lies wholly inside this block.
    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        self.is_v4() == other.is_v4()
            && other.prefix() >= self.prefix()
            && masked(other.bytes(), self.prefix()) == self.bytes()
    }

    /// Whether `addr` is in this block.
    #[must_use]
    pub fn contains_addr(self, addr: IpAddr) -> bool {
        let one = match addr {
            IpAddr::V4(a) => Self::V4(a.octets(), 32),
            IpAddr::V6(a) => Self::V6(a.octets(), 128),
        };
        self.contains(one)
    }
}

impl fmt::Display for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::V4(b, l) => write!(f, "{}/{l}", std::net::Ipv4Addr::from(b)),
            Self::V6(b, l) => write!(f, "{}/{l}", std::net::Ipv6Addr::from(b)),
        }
    }
}

impl Rule {
    /// Parse the configuration's form: `cidr`, `cidr:port` or
    /// `cidr:lo-hi` (`::1/128:53`: the port follows the prefix).
    ///
    /// # Errors
    ///
    /// A bad block or ports.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let slash = s.find('/').ok_or_else(|| format!("{s}: no /prefix"))?;
        let (net, ports) = match s[slash..].find(':') {
            Some(colon) => (&s[..slash + colon], Some(&s[slash + colon + 1..])),
            None => (s, None),
        };
        let ports = ports
            .map(|p| {
                let bad = || format!("{s}: bad ports {p}");
                let (lo, hi) = p.split_once('-').unwrap_or((p, p));
                let lo: u16 = lo.parse().map_err(|_| bad())?;
                let hi: u16 = hi.parse().map_err(|_| bad())?;
                if lo > hi { Err(bad()) } else { Ok((lo, hi)) }
            })
            .transpose()?;
        Ok(Self {
            net: Net::parse(net)?,
            ports,
        })
    }

    /// The special ranges this rule must not reach: those it covers
    /// without lying inside.
    #[must_use]
    pub fn guarded(self) -> Vec<Net> {
        SPECIAL
            .into_iter()
            .filter(|s| self.net.contains(*s) && !s.contains(self.net))
            .collect()
    }

    /// Whether `port` is in the rule's ports.
    #[must_use]
    pub fn port_ok(self, port: u16) -> bool {
        self.ports.is_none_or(|(lo, hi)| (lo..=hi).contains(&port))
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ports {
            None => write!(f, "{}", self.net),
            Some((lo, hi)) if lo == hi => write!(f, "{}:{lo}", self.net),
            Some((lo, hi)) => write!(f, "{}:{lo}-{hi}", self.net),
        }
    }
}

/// Which use of an address a program checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    /// A connect or a send: the `connect` rules, with the guard.
    Connect,
    /// A bind: the `bind` rules; the unspecified address with port 0 (the
    /// bind before a connect, or a client's UDP socket) needs no rule, as
    /// for sockets the node makes.
    Bind,
}

/// Whether `rules` allow `addr:port` for `hook`. An IPv4-mapped address
/// is checked as its IPv4 address, as the kernel sends it. The reference
/// the generated fences are tested against.
#[must_use]
pub fn allows(rules: &[Rule], addr: IpAddr, port: u16, hook: Hook) -> bool {
    if hook == Hook::Bind && addr.is_unspecified() && port == 0 {
        return true;
    }
    let guard = hook == Hook::Connect;
    let addr = match addr {
        IpAddr::V6(a) => a.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        v4 @ IpAddr::V4(_) => v4,
    };
    rules.iter().any(|r| {
        r.net.contains_addr(addr)
            && r.port_ok(port)
            && (!guard || !r.guarded().iter().any(|s| s.contains_addr(addr)))
    })
}

/// One BPF instruction, as the kernel lays it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    /// The opcode.
    pub code: u8,
    /// The destination register.
    pub dst: u8,
    /// The source register.
    pub src: u8,
    /// The offset.
    pub off: i16,
    /// The immediate.
    pub imm: i32,
}

impl Insn {
    /// As the kernel reads it, on a little-endian machine.
    #[must_use]
    pub fn encode(self) -> u64 {
        u64::from(self.code)
            | u64::from((self.dst & 0xf) | ((self.src & 0xf) << 4)) << 8
            | u64::from(self.off.cast_unsigned()) << 16
            | u64::from(self.imm.cast_unsigned()) << 32
    }
}

/// The opcodes the generator emits.
pub mod op {
    /// `BPF_LDX | BPF_MEM | BPF_W`.
    pub const LDX_W: u8 = 0x61;
    /// `BPF_ALU64 | BPF_MOV | BPF_X`.
    pub const MOV64_X: u8 = 0xbf;
    /// `BPF_ALU64 | BPF_MOV | BPF_K`.
    pub const MOV64_K: u8 = 0xb7;
    /// `BPF_ALU | BPF_MOV | BPF_X`.
    pub const MOV32_X: u8 = 0xbc;
    /// `BPF_ALU | BPF_AND | BPF_K`.
    pub const AND32_K: u8 = 0x54;
    /// `BPF_ALU | BPF_END | BPF_TO_BE`, with immediate 16.
    pub const BE16: u8 = 0xdc;
    /// `BPF_JMP | BPF_JA`.
    pub const JA: u8 = 0x05;
    /// `BPF_JMP32 | BPF_JNE | BPF_K`.
    pub const JNE32_K: u8 = 0x56;
    /// `BPF_JMP32 | BPF_JGT | BPF_K`.
    pub const JGT32_K: u8 = 0x26;
    /// `BPF_JMP32 | BPF_JLT | BPF_K`.
    pub const JLT32_K: u8 = 0xa6;
    /// `BPF_JMP | BPF_EXIT`.
    pub const EXIT: u8 = 0x95;
}

const OFF_IP4: i16 = 4;
const OFF_IP6: i16 = 8;
const OFF_PORT: i16 = 24;
/// Registers: r1 the context, r2 the IPv4 address or r2 to r5 the IPv6
/// words, r6 the port in host order, r7 scratch.
const CTX: u8 = 1;
const PORT: u8 = 6;
const SCRATCH: u8 = 7;

/// Which address family a program sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `*4` hooks.
    V4,
    /// `*6` hooks.
    V6,
}

type Label = usize;

/// Instructions with jumps to labels, resolved by [`Asm::finish`].
#[derive(Default)]
struct Asm {
    insns: Vec<Insn>,
    fixups: Vec<(usize, Label)>,
    labels: Vec<Option<usize>>,
}

impl Asm {
    fn label(&mut self) -> Label {
        self.labels.push(None);
        self.labels.len() - 1
    }

    fn here(&mut self, l: Label) {
        if let Some(slot) = self.labels.get_mut(l) {
            *slot = Some(self.insns.len());
        }
    }

    fn op(&mut self, code: u8, dst: u8, src: u8, off: i16, imm: i32) {
        self.insns.push(Insn {
            code,
            dst,
            src,
            off,
            imm,
        });
    }

    fn jump(&mut self, code: u8, dst: u8, imm: i32, to: Label) {
        self.fixups.push((self.insns.len(), to));
        self.op(code, dst, 0, 0, imm);
    }

    fn reached(&self, l: Label) -> bool {
        self.fixups.iter().any(|(_, to)| *to == l)
    }

    fn finish(mut self) -> Result<Vec<Insn>, String> {
        for (at, l) in self.fixups {
            let target = self
                .labels
                .get(l)
                .copied()
                .flatten()
                .ok_or("a jump to a label never placed")?;
            let off =
                i64::try_from(target).unwrap_or(i64::MAX) - i64::try_from(at).unwrap_or(0) - 1;
            let off = i16::try_from(off).map_err(|_| "a jump too long for BPF")?;
            if let Some(insn) = self.insns.get_mut(at) {
                insn.off = off;
            }
        }
        Ok(self.insns)
    }

    /// Unless `(reg & mask) == want`, jump to `miss`; nothing for an empty
    /// mask.
    fn masked(&mut self, reg: u8, (mask, want): (u32, u32), miss: Label) {
        match mask {
            0 => {}
            u32::MAX => self.jump(op::JNE32_K, reg, want.cast_signed(), miss),
            _ => {
                self.op(op::MOV32_X, SCRATCH, reg, 0, 0);
                self.op(op::AND32_K, SCRATCH, 0, 0, mask.cast_signed());
                self.jump(op::JNE32_K, SCRATCH, want.cast_signed(), miss);
            }
        }
    }
}

/// A block's 32-bit words as a program compares them: `(mask, want)`, in
/// the byte order a load gives.
fn words(net: Net) -> Vec<(u32, u32)> {
    let m = mask(net.bytes().len(), net.prefix());
    net.bytes()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(m.as_chunks::<4>().0)
        .map(|(b, m)| {
            let m = u32::from_ne_bytes(*m);
            (m, u32::from_ne_bytes(*b) & m)
        })
        .collect()
}

/// The program for `family` and `hook` allowing what `rules` allow, as
/// [`allows`] does. An IPv6 program checks IPv4 rules against
/// IPv4-mapped addresses, and keeps IPv6 rules off them.
///
/// # Errors
///
/// A program too long for BPF's 16-bit jumps (thousands of rules).
pub fn program(rules: &[Rule], family: Family, hook: Hook) -> Result<Vec<Insn>, String> {
    let guard = hook == Hook::Connect;
    let mut a = Asm::default();
    let allow = a.label();
    let deny = a.label();
    a.op(op::MOV64_X, CTX, 1, 0, 0);
    match family {
        Family::V4 => a.op(op::LDX_W, 2, CTX, OFF_IP4, 0),
        Family::V6 => {
            for i in 0..4u8 {
                a.op(op::LDX_W, 2 + i, CTX, OFF_IP6 + 4 * i16::from(i), 0);
            }
        }
    }
    a.op(op::LDX_W, PORT, CTX, OFF_PORT, 0);
    a.op(op::BE16, PORT, 0, 0, 16);
    if hook == Hook::Bind {
        // The unspecified address with port 0: allowed without a rule.
        let ruled = a.label();
        let words = if family == Family::V4 { 1 } else { 4 };
        for reg in 2..2 + words {
            a.jump(op::JNE32_K, reg, 0, ruled);
        }
        a.jump(op::JNE32_K, PORT, 0, ruled);
        a.jump(op::JA, 0, 0, allow);
        a.here(ruled);
    }
    for r in rules {
        let next = a.label();
        let regs: Vec<u8> = match (family, r.net.is_v4()) {
            (Family::V4, true) => vec![2],
            (Family::V4, false) => continue,
            (Family::V6, false) => vec![2, 3, 4, 5],
            (Family::V6, true) => {
                // The mapped prefix, then the IPv4 address in the last word.
                let mapped = u32::from_ne_bytes([0, 0, 0xff, 0xff]);
                a.jump(op::JNE32_K, 2, 0, next);
                a.jump(op::JNE32_K, 3, 0, next);
                a.jump(op::JNE32_K, 4, mapped.cast_signed(), next);
                vec![5]
            }
        };
        for (reg, w) in regs.iter().zip(words(r.net)) {
            a.masked(*reg, w, next);
        }
        let mut skipped = if guard { r.guarded() } else { Vec::new() };
        if !r.net.is_v4() && r.net.contains(MAPPED) && !MAPPED.contains(r.net) {
            skipped.push(MAPPED);
        }
        for special in skipped {
            // Inside `special`: this rule does not apply.
            let outside = a.label();
            for (reg, w) in regs.iter().zip(words(special)) {
                a.masked(*reg, w, outside);
            }
            a.jump(op::JA, 0, 0, next);
            a.here(outside);
        }
        match r.ports {
            None => {}
            Some((lo, hi)) if lo == hi => a.jump(op::JNE32_K, PORT, i32::from(lo), next),
            Some((lo, hi)) => {
                a.jump(op::JLT32_K, PORT, i32::from(lo), next);
                a.jump(op::JGT32_K, PORT, i32::from(hi), next);
            }
        }
        a.jump(op::JA, 0, 0, allow);
        a.here(next);
    }
    a.jump(op::JA, 0, 0, deny);
    // The verifier refuses unreachable instructions: no allow block when
    // no rule of this family reaches it.
    if a.reached(allow) {
        a.here(allow);
        a.op(op::MOV64_K, 0, 0, 0, 1);
        a.op(op::EXIT, 0, 0, 0, 0);
    }
    a.here(deny);
    a.op(op::MOV64_K, 0, 0, 0, 0);
    a.op(op::EXIT, 0, 0, 0, 0);
    a.finish()
}

/// The cgroup hooks an own-sockets app's programs attach to, as the
/// kernel numbers them (`enum bpf_attach_type`).
pub mod hook {
    /// `BPF_CGROUP_INET4_BIND`.
    pub const INET4_BIND: u32 = 8;
    /// `BPF_CGROUP_INET6_BIND`.
    pub const INET6_BIND: u32 = 9;
    /// `BPF_CGROUP_INET4_CONNECT`.
    pub const INET4_CONNECT: u32 = 10;
    /// `BPF_CGROUP_INET6_CONNECT`.
    pub const INET6_CONNECT: u32 = 11;
    /// `BPF_CGROUP_UDP4_SENDMSG`.
    pub const UDP4_SENDMSG: u32 = 14;
    /// `BPF_CGROUP_UDP6_SENDMSG`.
    pub const UDP6_SENDMSG: u32 = 15;
    /// All six.
    pub const ALL: [u32; 6] = [
        INET4_CONNECT,
        INET6_CONNECT,
        UDP4_SENDMSG,
        UDP6_SENDMSG,
        INET4_BIND,
        INET6_BIND,
    ];
}

/// A program with its hook and its name.
pub type Hooked = (u32, &'static str, Vec<Insn>);

/// One program for each of [`hook::ALL`]: connect and unconnected sends
/// by `connect` with the guard, binds by `bind`.
///
/// # Errors
///
/// As [`program`].
pub fn programs(connect: &[Rule], bind: &[Rule]) -> Result<Vec<Hooked>, String> {
    Ok(vec![
        (
            hook::INET4_CONNECT,
            "own_connect4",
            program(connect, Family::V4, Hook::Connect)?,
        ),
        (
            hook::INET6_CONNECT,
            "own_connect6",
            program(connect, Family::V6, Hook::Connect)?,
        ),
        (
            hook::UDP4_SENDMSG,
            "own_sendmsg4",
            program(connect, Family::V4, Hook::Connect)?,
        ),
        (
            hook::UDP6_SENDMSG,
            "own_sendmsg6",
            program(connect, Family::V6, Hook::Connect)?,
        ),
        (
            hook::INET4_BIND,
            "own_bind4",
            program(bind, Family::V4, Hook::Bind)?,
        ),
        (
            hook::INET6_BIND,
            "own_bind6",
            program(bind, Family::V6, Hook::Bind)?,
        ),
    ])
}

/// `connect`, ordered for a first-match filter: rules inside a guarded
/// range, the guarded ranges to block, the other rules.
fn first_match(connect: &[Rule]) -> (Vec<Rule>, Vec<Net>, Vec<Rule>) {
    let (inside, broad): (Vec<Rule>, Vec<Rule>) = connect
        .iter()
        .partition(|r| SPECIAL.iter().any(|s| s.contains(r.net)));
    let mut blocks: Vec<Net> = Vec::new();
    for s in broad.iter().flat_map(|r| r.guarded()) {
        if !blocks.contains(&s) {
            blocks.push(s);
        }
    }
    (inside, blocks, broad)
}

/// The ipfw rules for an own-sockets instance in jail `jid`, numbered from
/// `first`, as arguments to `ipfw -q add`: what the jail's sockets may
/// send. ipfw stops at the first match, so the guard is order: rules
/// inside a guarded range, a deny of each guarded range a broader rule
/// covers, the broader rules, then a deny of the rest. Replies from the
/// `bind` rules' ports pass.
#[must_use]
pub fn ipfw(jid: i32, first: u32, connect: &[Rule], bind: &[Rule]) -> Vec<Vec<String>> {
    let port = |p: Option<(u16, u16)>| match p {
        None => Vec::new(),
        Some((lo, hi)) if lo == hi => vec![lo.to_string()],
        Some((lo, hi)) => vec![format!("{lo}-{hi}")],
    };
    let mut lines: Vec<Vec<String>> = Vec::new();
    let mut add = |verb: &str, from_port: Vec<String>, to: String, to_port: Vec<String>| {
        for proto in ["tcp", "udp"] {
            let mut l = vec![
                verb.to_string(),
                proto.to_string(),
                "from".into(),
                "any".into(),
            ];
            l.extend(from_port.iter().cloned());
            l.extend(["to".to_string(), to.clone()]);
            l.extend(to_port.iter().cloned());
            l.extend(["out".to_string(), "jail".to_string(), jid.to_string()]);
            lines.push(l);
        }
    };
    for b in bind {
        add("allow", port(b.ports), "any".into(), Vec::new());
    }
    let (inside, blocks, broad) = first_match(connect);
    for r in &inside {
        add("allow", Vec::new(), r.net.to_string(), port(r.ports));
    }
    for s in &blocks {
        add("deny", Vec::new(), s.to_string(), Vec::new());
    }
    for r in &broad {
        add("allow", Vec::new(), r.net.to_string(), port(r.ports));
    }
    add("deny", Vec::new(), "any".into(), Vec::new());
    lines
        .into_iter()
        .zip(first..)
        .map(|(mut l, n)| {
            l.insert(0, n.to_string());
            l
        })
        .collect()
}

/// The first `len` free rule numbers in `range`, given the blocks in
/// `used` (start and length): where an instance's ipfw rules go.
#[must_use]
pub fn free_block(
    range: &std::ops::RangeInclusive<u32>,
    used: &[(u32, u32)],
    len: u32,
) -> Option<u32> {
    let mut taken: Vec<(u32, u32)> = used.to_vec();
    taken.sort_unstable();
    let mut start = *range.start();
    for (at, n) in taken {
        if at.saturating_sub(start) >= len {
            break;
        }
        start = start.max(at.saturating_add(n));
    }
    let last = start.checked_add(len)?.checked_sub(1)?;
    (len > 0 && last <= *range.end()).then_some(start)
}

/// The rule numbers in `range` that `ipfw list` printed in `listing` (a
/// number at the start of each line): rules a helper that was killed left.
#[must_use]
pub fn listed_rules(listing: &str, range: &std::ops::RangeInclusive<u32>) -> Vec<u32> {
    let mut numbers: Vec<u32> = listing
        .lines()
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .filter(|n| range.contains(n))
        .collect();
    numbers.dedup();
    numbers
}

#[cfg(test)]
#[path = "sockets_tests.rs"]
mod sockets_tests;

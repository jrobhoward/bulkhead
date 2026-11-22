//! The rules as packet-filter rules: nftables on Linux (matching the app's
//! cgroup), pf and ipfw on FreeBSD (matching the jail's address, or its
//! id). Each filters what the app sends: packets to a `connect` rule's
//! block and ports, and replies from a `bind` rule's ports; the rest is
//! dropped, so `connect` and `sendto` fail at once.
//!
//! pf and ipfw stop at the first match, so the guard becomes order: rules
//! inside a guarded range, then a block of each guarded range a broader
//! rule covers, then the broader rules.

use crate::rules::{Net, Rule};

fn family(net: Net) -> &'static str {
    if net.is_v4() { "ip" } else { "ip6" }
}

/// The nftables table for `app`'s cgroup `s40/apps/<app>`.
pub fn nft(app: &str, connect: &[Rule], bind: &[Rule]) -> String {
    let mut chain = Vec::new();
    for b in bind {
        let ports = b.ports.map_or(String::new(), |(lo, hi)| {
            format!(" meta l4proto {{ tcp, udp }} th sport {lo}-{hi}")
        });
        chain.push(format!(
            "{} saddr {}{ports} accept",
            family(b.net),
            b.net.text()
        ));
    }
    for r in connect {
        let f = family(r.net);
        let guard: String = r
            .guarded()
            .iter()
            .map(|s| format!(" {f} daddr != {}", s.text()))
            .collect();
        let ports = r.ports.map_or(String::new(), |(lo, hi)| {
            format!(" meta l4proto {{ tcp, udp }} th dport {lo}-{hi}")
        });
        chain.push(format!("{f} daddr {}{guard}{ports} accept", r.net.text()));
    }
    // A dropped SYN does not fail `connect`: the kernel retransmits until
    // the caller gives up. A reset fails it at once (`ECONNREFUSED`); a
    // dropped datagram fails `sendto` with `EPERM`.
    chain.push("meta l4proto tcp reject with tcp reset".into());
    chain.push("drop".into());
    let body: String = chain.iter().map(|l| format!("    {l}\n")).collect();
    format!(
        "table inet s40 {{\n  chain out {{\n    type filter hook output priority 0; policy accept;\n    \
         socket cgroupv2 level 3 \"s40/apps/{app}\" jump app\n  }}\n  chain app {{\n{body}  }}\n}}\n"
    )
}

/// `connect`, ordered for a first-match filter: rules inside a guarded
/// range, the guarded ranges to block, the other rules.
fn first_match(connect: &[Rule]) -> (Vec<Rule>, Vec<Net>, Vec<Rule>) {
    let (inside, broad): (Vec<Rule>, Vec<Rule>) = connect
        .iter()
        .partition(|r| crate::rules::SPECIAL.iter().any(|s| s.contains(r.net)));
    let mut blocks: Vec<Net> = Vec::new();
    for s in broad.iter().flat_map(|r| r.guarded()) {
        if !blocks.contains(&s) {
            blocks.push(s);
        }
    }
    (inside, blocks, broad)
}

/// pf rules for anchor `s40/<app>`: what the jail at `jail` (IPv4) may
/// send. IPv6 rules are left out: the pf variant's jail has no IPv6.
pub fn pf(jail: &str, connect: &[Rule], bind: &[Rule]) -> String {
    let v4 = |rules: &[Rule]| {
        rules
            .iter()
            .copied()
            .filter(|r| r.net.is_v4())
            .collect::<Vec<_>>()
    };
    let port = |p: Option<(u16, u16)>| match p {
        None => String::new(),
        Some((lo, hi)) if lo == hi => format!(" port {lo}"),
        Some((lo, hi)) => format!(" port {lo}:{hi}"),
    };
    let pass = |r: &Rule| match r.ports {
        None => format!("pass out quick from {jail} to {}\n", r.net.text()),
        Some(_) => format!(
            "pass out quick proto {{ tcp udp }} from {jail} to {}{}\n",
            r.net.text(),
            port(r.ports)
        ),
    };
    let mut out = String::new();
    for b in v4(bind) {
        out += &format!(
            "pass out quick proto {{ tcp udp }} from {jail}{}\n",
            port(b.ports)
        );
    }
    let (inside, blocks, broad) = first_match(&v4(connect));
    inside.iter().for_each(|r| out += &pass(r));
    for s in blocks.iter().filter(|s| s.is_v4()) {
        out += &format!("block drop out quick from {jail} to {}\n", s.text());
    }
    broad.iter().for_each(|r| out += &pass(r));
    out += &format!("block drop out quick from {jail}\n");
    out
}

/// ipfw commands (arguments to `ipfw -q add`) for jail `jid`, numbered
/// from `first`: what the jail's sockets may send.
pub fn ipfw(jid: u32, first: u32, connect: &[Rule], bind: &[Rule]) -> Vec<String> {
    let port = |p: Option<(u16, u16)>| match p {
        None => String::new(),
        Some((lo, hi)) if lo == hi => format!(" {lo}"),
        Some((lo, hi)) => format!(" {lo}-{hi}"),
    };
    let mut lines = Vec::new();
    let mut add = |verb: &str, rest: String| {
        for proto in ["tcp", "udp"] {
            lines.push(format!("{verb} {proto} {rest} out jail {jid}"));
        }
    };
    for b in bind {
        add("allow", format!("from any{} to any", port(b.ports)));
    }
    let (inside, blocks, broad) = first_match(connect);
    for r in &inside {
        add(
            "allow",
            format!("from any to {}{}", r.net.text(), port(r.ports)),
        );
    }
    for s in &blocks {
        add("deny", format!("from any to {}", s.text()));
    }
    for r in &broad {
        add(
            "allow",
            format!("from any to {}{}", r.net.text(), port(r.ports)),
        );
    }
    add("deny", "from any to any".into());
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| format!("{} {l}", first + i as u32))
        .collect()
}

#[cfg(test)]
#[path = "firewall_tests.rs"]
mod firewall_tests;

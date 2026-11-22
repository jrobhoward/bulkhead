#![allow(non_snake_case)]

use std::net::IpAddr;

use super::*;
use crate::rules::{RawRule, allows};

fn rules(spec: &[(&str, Option<&str>)]) -> Vec<Rule> {
    spec.iter()
        .map(|(c, p)| {
            RawRule {
                cidr: (*c).into(),
                ports: p.map(Into::into),
            }
            .parse()
            .unwrap()
        })
        .collect()
}

fn probe_rules() -> Vec<Rule> {
    rules(&[
        ("127.0.0.1/32", Some("7001")),
        ("127.0.0.1/32", Some("9000-9009")),
        ("::1/128", Some("7001")),
        ("0.0.0.0/0", Some("7100")),
        ("10.20.0.0/12", None),
        ("2001:db8::/33", Some("443")),
        ("::/0", Some("8443")),
    ])
}

const ADDRS: &[&str] = &[
    "127.0.0.1",
    "127.0.0.2",
    "127.255.0.1",
    "10.9.9.9",
    "10.16.0.1",
    "10.31.255.255",
    "10.32.0.0",
    "169.254.169.254",
    "0.0.0.0",
    "0.1.2.3",
    "192.168.1.1",
    "::1",
    "::",
    "fe80::1",
    "febf::1",
    "fec0::1",
    "2001:db8::1",
    "2001:db8:7fff::1",
    "2001:db8:8000::1",
    "::ffff:127.0.0.1",
    "::ffff:127.0.0.2",
    "::ffff:10.20.0.5",
    "::ffff:169.254.169.254",
    "::ffff:1.2.3.4",
    "2a00::1",
];

const PORTS: &[u16] = &[
    0, 443, 7001, 7002, 7100, 8443, 8999, 9000, 9005, 9009, 9010, 65535,
];

fn family(addr: IpAddr) -> Family {
    if addr.is_ipv4() {
        Family::V4
    } else {
        Family::V6
    }
}

#[test]
fn program____every_address_and_port____agrees_with_the_rules() {
    let rules = probe_rules();
    for guard in [true, false] {
        let v4 = program(&rules, Family::V4, guard);
        let v6 = program(&rules, Family::V6, guard);
        for a in ADDRS {
            let addr: IpAddr = a.parse().unwrap();
            for port in PORTS {
                let prog = if family(addr) == Family::V4 { &v4 } else { &v6 };

                let got = run(prog, &context(addr, *port));

                let want = allows(&rules, addr, *port, guard);
                assert_eq!(got == 1, want, "{a}:{port} guard={guard}");
            }
        }
    }
}

/// Every instruction reachable from the first, as the verifier requires.
fn all_reachable(prog: &[Insn]) -> bool {
    let mut seen = vec![false; prog.len()];
    let mut todo = vec![0usize];
    while let Some(pc) = todo.pop() {
        if pc >= prog.len() || seen[pc] {
            continue;
        }
        seen[pc] = true;
        let i = prog[pc];
        let target = (pc as isize + 1 + isize::from(i.off)) as usize;
        match i.code {
            0x95 => {}
            0x05 => todo.push(target),
            c if c & 0x07 == 0x06 => todo.extend([pc + 1, target]),
            _ => todo.push(pc + 1),
        }
    }
    seen.iter().all(|s| *s)
}

#[test]
fn program____no_rules_of_its_family____refuses_everything_all_reachable() {
    let v4_only = rules(&[("127.0.0.1/32", Some("7001"))]);
    let v6_only = rules(&[("::1/128", Some("7001"))]);

    let none = program(&[], Family::V6, true);
    let v4_of_v6 = program(&v6_only, Family::V4, true);

    assert_eq!(run(&none, &context("::1".parse().unwrap(), 1)), 0);
    assert_eq!(
        run(&v4_of_v6, &context("127.0.0.1".parse().unwrap(), 7001)),
        0
    );
    for prog in [
        none,
        v4_of_v6,
        program(&v4_only, Family::V6, true),
        program(&probe_rules(), Family::V6, true),
    ] {
        assert!(all_reachable(&prog), "{prog:?}");
    }
}

#[test]
fn program____64_rules____well_under_the_verifiers_limit() {
    let many: Vec<Rule> = (0..64)
        .map(|i| {
            RawRule {
                cidr: format!("10.{i}.0.0/24"),
                ports: Some("8000-8100".into()),
            }
            .parse()
            .unwrap()
        })
        .collect();

    let v4 = program(&many, Family::V4, true);
    let v6 = program(&many, Family::V6, true);

    // 4096 instructions is the unprivileged limit; root's is a million.
    assert!(v4.len() < 512, "{}", v4.len());
    assert!(v6.len() < 1024, "{}", v6.len());
    assert_eq!(run(&v4, &context("10.63.0.9".parse().unwrap(), 8050)), 1);
    assert_eq!(run(&v4, &context("10.64.0.9".parse().unwrap(), 8050)), 0);
}

#[test]
#[ignore = "prints the sizes the README quotes"]
fn program____sizes() {
    let many: Vec<Rule> = (0..64)
        .map(|i| {
            RawRule {
                cidr: format!("10.{i}.0.0/24"),
                ports: Some("8000-8100".into()),
            }
            .parse()
            .unwrap()
        })
        .collect();
    println!(
        "empty v4 {}, probe v4 {} v6 {}, 64 rules v4 {} v6 {}",
        program(&[], Family::V4, true).len(),
        program(&probe_rules(), Family::V4, true).len(),
        program(&probe_rules(), Family::V6, true).len(),
        program(&many, Family::V4, true).len(),
        program(&many, Family::V6, true).len()
    );
}

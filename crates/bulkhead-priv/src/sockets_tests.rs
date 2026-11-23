#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::net::IpAddr;

use rstest::rstest;

use super::*;

fn rules(spec: &[&str]) -> Vec<Rule> {
    spec.iter().map(|s| Rule::parse(s).unwrap()).collect()
}

fn probe_rules() -> Vec<Rule> {
    rules(&[
        "127.0.0.1/32:7001",
        "127.0.0.1/32:9000-9009",
        "::1/128:7001",
        "0.0.0.0/0:7100",
        "10.20.0.0/12",
        "2001:db8::/33:443",
        "::/0:8443",
    ])
}

/// A `bpf_sock_addr` context for `addr:port`, as the kernel fills it.
fn context(addr: IpAddr, port: u16) -> [u8; 64] {
    let mut ctx = [0u8; 64];
    match addr {
        IpAddr::V4(a) => ctx[4..8].copy_from_slice(&a.octets()),
        IpAddr::V6(a) => ctx[8..24].copy_from_slice(&a.octets()),
    }
    ctx[24..26].copy_from_slice(&port.to_be_bytes());
    ctx
}

/// Run `prog` on `ctx` as the kernel would, for the instructions the
/// generator emits; its return value.
fn run(prog: &[Insn], ctx: &[u8]) -> u64 {
    run_on(prog, ctx, &[])
}

/// [`run`], with `packet` behind `bpf_skb_load_bytes` and a stack at r10.
fn run_on(prog: &[Insn], ctx: &[u8], packet: &[u8]) -> u64 {
    const CTX_AT: u64 = 0x1000_0000;
    const STACK_AT: u64 = 0x2000_0000;
    let mut stack = [0u8; 512];
    let mut r = [0u64; 11];
    r[1] = CTX_AT;
    r[10] = STACK_AT + 512;
    let mut pc = 0usize;
    loop {
        let i = prog[pc];
        let (d, s) = (usize::from(i.dst), usize::from(i.src));
        let imm32 = i.imm.cast_unsigned();
        let imm64 = i64::from(i.imm).cast_unsigned();
        let mut jump = |cond: bool| {
            if cond {
                pc = pc.checked_add_signed(isize::from(i.off)).unwrap();
            }
        };
        let load = |stack: &[u8], addr: u64, n: usize| -> u64 {
            let (mem, at): (&[u8], u64) = if addr >= STACK_AT {
                (stack, addr - STACK_AT)
            } else {
                (ctx, addr - CTX_AT)
            };
            let at = usize::try_from(at).unwrap();
            let mut b = [0u8; 8];
            b[..n].copy_from_slice(&mem[at..at + n]);
            u64::from_ne_bytes(b)
        };
        let at = r[s].wrapping_add_signed(i64::from(i.off));
        match i.code {
            op::LDX_W => r[d] = load(&stack, at, 4),
            op::LDX_H => r[d] = load(&stack, at, 2),
            op::LDX_B => r[d] = load(&stack, at, 1),
            op::MOV64_X => r[d] = r[s],
            op::MOV64_K => r[d] = imm64,
            op::MOV32_X => r[d] = r[s] & 0xffff_ffff,
            op::AND32_K => r[d] = (r[d] & 0xffff_ffff) & u64::from(imm32),
            op::ADD64_K => r[d] = r[d].wrapping_add(imm64),
            op::AND64_K => r[d] &= imm64,
            op::LSH64_K => r[d] <<= imm32,
            op::BE16 => r[d] = u64::from(u16::try_from(r[d] & 0xffff).unwrap().to_be()),
            op::CALL => {
                assert_eq!(i.imm, 26, "only bpf_skb_load_bytes");
                let (off, to, len) = (
                    usize::try_from(r[2]).unwrap(),
                    usize::try_from(r[3] - STACK_AT).unwrap(),
                    usize::try_from(r[4]).unwrap(),
                );
                r[0] = match packet.get(off..off + len) {
                    Some(bytes) => {
                        stack[to..to + len].copy_from_slice(bytes);
                        0
                    }
                    None => (-14i64).cast_unsigned(),
                };
                // The helper's arguments are not kept.
                r[1..6].fill(0xdead);
            }
            op::JA => jump(true),
            op::JNE64_K => jump(r[d] != imm64),
            op::JEQ32_K => jump((r[d] & 0xffff_ffff) == u64::from(imm32)),
            op::JNE32_K => jump((r[d] & 0xffff_ffff) != u64::from(imm32)),
            op::JGT32_K => jump((r[d] & 0xffff_ffff) > u64::from(imm32)),
            op::JLT32_K => jump((r[d] & 0xffff_ffff) < u64::from(imm32)),
            op::EXIT => return r[0],
            other => panic!("opcode {other:#x} not interpreted"),
        }
        pc += 1;
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
        // Only a jump's offset is a target: a load's may be negative.
        let target = || (pc + 1).checked_add_signed(isize::from(i.off)).unwrap();
        match i.code {
            op::EXIT => {}
            op::JA => todo.push(target()),
            op::CALL => todo.push(pc + 1),
            c if c & 0x07 == 0x05 || c & 0x07 == 0x06 => todo.extend([pc + 1, target()]),
            _ => todo.push(pc + 1),
        }
    }
    seen.iter().all(|s| *s)
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

#[test]
fn program____every_address_and_port____agrees_with_the_rules() {
    let rules = probe_rules();
    for hook in [Hook::Connect, Hook::Bind] {
        let v4 = program(&rules, Family::V4, hook).unwrap();
        let v6 = program(&rules, Family::V6, hook).unwrap();
        for a in ADDRS {
            let addr: IpAddr = a.parse().unwrap();
            for port in PORTS {
                let prog = if addr.is_ipv4() { &v4 } else { &v6 };

                let got = run(prog, &context(addr, *port));

                assert_eq!(
                    got == 1,
                    allows(&rules, addr, *port, hook),
                    "{a}:{port} {hook:?}"
                );
            }
        }
    }
}

#[test]
fn program____no_rules_of_its_family____refuses_everything_all_reachable() {
    let v6_only = rules(&["::1/128:7001"]);

    let none = program(&[], Family::V6, Hook::Connect).unwrap();
    let v4_of_v6 = program(&v6_only, Family::V4, Hook::Connect).unwrap();
    let bind_none = program(&[], Family::V4, Hook::Bind).unwrap();

    assert_eq!(run(&none, &context("::1".parse().unwrap(), 1)), 0);
    assert_eq!(
        run(&v4_of_v6, &context("127.0.0.1".parse().unwrap(), 7001)),
        0
    );
    assert_eq!(
        run(&bind_none, &context("0.0.0.0".parse().unwrap(), 0)),
        1,
        "the bind before a connect"
    );
    assert_eq!(run(&bind_none, &context("0.0.0.0".parse().unwrap(), 80)), 0);
    assert_eq!(
        run(&bind_none, &context("127.0.0.1".parse().unwrap(), 0)),
        0
    );
    for (connect, bind) in [(probe_rules(), Vec::new()), (Vec::new(), v6_only.clone())] {
        for (_, _, name, prog) in programs(&connect, &bind, false).unwrap() {
            assert!(all_reachable(&prog), "{name}: {prog:?}");
        }
    }
}

#[test]
fn program____64_rules____well_under_the_verifiers_limit() {
    let many: Vec<Rule> = (0..64)
        .map(|i| Rule::parse(&format!("10.{i}.0.0/24:8000-8100")).unwrap())
        .collect();

    let v4 = program(&many, Family::V4, Hook::Connect).unwrap();
    let v6 = program(&many, Family::V6, Hook::Connect).unwrap();

    assert!(v4.len() < 512, "{}", v4.len());
    assert!(v6.len() < 1024, "{}", v6.len());
    assert_eq!(run(&v4, &context("10.63.0.9".parse().unwrap(), 8050)), 1);
    assert_eq!(run(&v4, &context("10.64.0.9".parse().unwrap(), 8050)), 0);
}

#[rstest]
#[case("10.20.0.0/16:1883", "10.20.0.0/16:1883")]
#[case("10.20.1.2/16", "10.20.0.0/16")]
#[case(" ::1/128:53 ", "::1/128:53")]
#[case("0.0.0.0/0:9000-9009", "0.0.0.0/0:9000-9009")]
#[case("fe80::/10:8000-8000", "fe80::/10:8000")]
fn Rule____parse_then_display____canonical(#[case] text: &str, #[case] canonical: &str) {
    let rule = Rule::parse(text).unwrap();

    assert_eq!(rule.to_string(), canonical);
    assert_eq!(Rule::parse(canonical).unwrap(), rule);
}

#[rstest]
#[case("10.0.0.0")]
#[case("10.0.0.0/33")]
#[case("10.0.0.0/8:9-1")]
#[case("10.0.0.0/8:http")]
#[case("host.example/8")]
fn Rule____bad____refused(#[case] text: &str) {
    assert!(Rule::parse(text).is_err());
}

#[test]
fn ipfw____a_rule_inside_a_guarded_range____before_its_block_then_broad_then_deny() {
    let connect = rules(&["0.0.0.0/0:7100", "127.0.0.1/32:7001"]);
    let bind = rules(&["0.0.0.0/0:9000-9009"]);

    let lines: Vec<String> = ipfw(7, 30000, &connect, &bind, false)
        .iter()
        .map(|l| l.join(" "))
        .collect();

    let at = |s: &str| {
        lines
            .iter()
            .position(|l| l.contains(s))
            .unwrap_or_else(|| panic!("{s}: {lines:?}"))
    };
    assert_eq!(
        lines[0],
        "30000 allow tcp from any 9000-9009 to any out jail 7"
    );
    assert!(at("to 127.0.0.1/32 7001") < at("deny tcp from any to 127.0.0.0/8"));
    assert!(at("deny udp from any to 169.254.0.0/16") < at("to 0.0.0.0/0 7100"));
    let out_end = at("deny udp from any to any out jail 7");
    assert!(lines[..out_end].iter().all(|l| l.contains(" out jail ")));
    let numbers: Vec<usize> = lines
        .iter()
        .map(|l| l.split(' ').next().unwrap().parse().unwrap())
        .collect();
    assert_eq!(
        numbers,
        (30000..30000 + lines.len()).collect::<Vec<_>>(),
        "numbered in order"
    );
}

#[rstest]
#[case(&[], 4, Some(100))]
#[case(&[(100, 4)], 4, Some(104))]
#[case(&[(104, 4)], 4, Some(100))]
#[case(&[(100, 2), (106, 4)], 4, Some(102))]
#[case(&[(100, 2), (105, 3)], 4, None)]
#[case(&[(100, 8)], 4, None)]
#[case(&[], 9, None)]
#[case(&[], 0, None)]
fn free_block____used_blocks____first_gap_that_fits(
    #[case] used: &[(u32, u32)],
    #[case] len: u32,
    #[case] want: Option<u32>,
) {
    assert_eq!(free_block(&(100..=107), used, len), want);
}

#[test]
fn listed_rules____ipfw_list____numbers_in_range_once() {
    let listing = "00100 allow ip from any to any\n30000 allow tcp from any to 10.0.0.0/8 1883 out jail 3\n30000 allow udp from any to any\n30001 deny tcp from any to any out jail 3\n65535 allow ip from any to any\n";

    assert_eq!(listed_rules(listing, &(30000..=39999)), [30000, 30001]);
}

/// An `__sk_buff` whose `protocol` is the Ethernet type `ethertype`.
fn skb(ethertype: u16) -> [u8; 96] {
    let mut ctx = [0u8; 96];
    let be = u32::from(u16::from_ne_bytes(ethertype.to_be_bytes()));
    ctx[16..20].copy_from_slice(&be.to_ne_bytes());
    ctx
}

/// An IPv4 packet carrying `proto` from `src:sport`, its header `ihl`
/// words long, and how the ingress program should see it.
fn v4(proto: u8, src: &str, sport: u16, ihl: u8) -> (Vec<u8>, Inbound) {
    let src: std::net::Ipv4Addr = src.parse().unwrap();
    let mut p = vec![0u8; usize::from(ihl) * 4 + 8];
    p[0] = 0x40 | ihl;
    p[9] = proto;
    p[12..16].copy_from_slice(&src.octets());
    let at = usize::from(ihl) * 4;
    p[at..at + 2].copy_from_slice(&sport.to_be_bytes());
    let seen = if proto == 17 {
        Inbound::Udp(IpAddr::V4(src), sport)
    } else {
        Inbound::NotUdp
    };
    (p, seen)
}

/// An IPv6 packet whose next header is `next`, from `src:sport`.
fn v6(next: u8, src: &str, sport: u16) -> (Vec<u8>, Inbound) {
    let src: std::net::Ipv6Addr = src.parse().unwrap();
    let mut p = vec![0u8; 48];
    p[0] = 0x60;
    p[6] = next;
    p[8..24].copy_from_slice(&src.octets());
    p[40..42].copy_from_slice(&sport.to_be_bytes());
    let seen = match next {
        17 => Inbound::Udp(IpAddr::V6(src), sport),
        6 => Inbound::NotUdp,
        _ => Inbound::V6Extension,
    };
    (p, seen)
}

#[test]
fn ingress____every_packet____agrees_with_delivers() {
    let connect = probe_rules();
    let mut cases: Vec<(u16, Vec<u8>, Inbound)> = vec![
        (0x0806, vec![0u8; 28], Inbound::NotIp),
        (0x0800, vec![0x45; 10], Inbound::Truncated),
        (
            0x0800,
            v4(17, "127.0.0.1", 7001, 5).0[..20].to_vec(),
            Inbound::Truncated,
        ),
        (0x86dd, vec![0x60; 30], Inbound::Truncated),
        (
            0x86dd,
            v6(17, "::1", 7001).0[..40].to_vec(),
            Inbound::Truncated,
        ),
    ];
    for a in ADDRS.iter().filter(|a| !a.contains(':')) {
        for port in PORTS {
            for (proto, ihl) in [(17, 5), (17, 7), (6, 5), (1, 5)] {
                let (p, seen) = v4(proto, a, *port, ihl);
                cases.push((0x0800, p, seen));
            }
        }
    }
    for a in ADDRS.iter().filter(|a| a.contains(':')) {
        for port in PORTS {
            for next in [17, 6, 0, 44] {
                let (p, seen) = v6(next, a, *port);
                cases.push((0x86dd, p, seen));
            }
        }
    }
    for listen in [false, true] {
        let prog = ingress(&connect, listen).unwrap();
        assert!(all_reachable(&prog), "{prog:?}");
        for (ethertype, packet, seen) in &cases {
            let got = run_on(&prog, &skb(*ethertype), packet);

            assert_eq!(
                got == 1,
                delivers(&connect, listen, *seen),
                "{seen:?} listen={listen}"
            );
        }
    }
}

#[test]
fn ipfw____in_rules____udp_from_connect_unless_listen() {
    let connect = rules(&["127.0.0.1/32:7001", "0.0.0.0/0:7100"]);
    let bind = rules(&["0.0.0.0/0:9000-9009"]);
    let text = |listen| -> Vec<String> {
        ipfw(7, 1, &connect, &bind, listen)
            .iter()
            .map(|l| l[1..].join(" "))
            .collect()
    };

    let quiet = text(false);
    let listening = text(true);

    let at = |s: &str| {
        quiet
            .iter()
            .position(|l| l == s)
            .unwrap_or_else(|| panic!("{s}: {quiet:?}"))
    };
    let udp_in = at("allow udp from 127.0.0.1/32 7001 to any in jail 7");
    let guarded = at("deny udp from 127.0.0.0/8 to any in jail 7");
    assert!(udp_in < guarded);
    assert!(guarded < at("allow udp from 0.0.0.0/0 7100 to any in jail 7"));
    assert_eq!(quiet.last().unwrap(), "deny udp from any to any in jail 7");
    assert!(
        !listening.iter().any(|l| l.contains(" in ")),
        "{listening:?}"
    );
    assert!(!quiet.iter().chain(&listening).any(|l| l.contains("setup")));
}

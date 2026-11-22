#![allow(non_snake_case)]

use super::*;
use crate::rules::RawRule;

fn rule(cidr: &str, ports: Option<&str>) -> Rule {
    RawRule {
        cidr: cidr.into(),
        ports: ports.map(Into::into),
    }
    .parse()
    .unwrap()
}

#[test]
fn nft____broad_rule____guarded_ranges_excluded_inside_rule_not() {
    let text = nft(
        "a",
        &[
            rule("127.0.0.1/32", Some("7001")),
            rule("0.0.0.0/0", Some("7100")),
        ],
        &[rule("0.0.0.0/0", Some("9000-9009"))],
    );

    assert!(
        text.contains("socket cgroupv2 level 3 \"s40/apps/a\" jump app"),
        "{text}"
    );
    assert!(
        text.contains("ip daddr 127.0.0.1/32 meta l4proto { tcp, udp } th dport 7001-7001 accept")
    );
    assert!(text.contains(
        "ip daddr 0.0.0.0/0 ip daddr != 127.0.0.0/8 ip daddr != 169.254.0.0/16 ip daddr != 0.0.0.0/8"
    ));
    assert!(
        text.contains("ip saddr 0.0.0.0/0 meta l4proto { tcp, udp } th sport 9000-9009 accept")
    );
    assert!(text.trim_end().ends_with("drop\n  }\n}"));
}

#[test]
fn pf_and_ipfw____inside_rule_before_blocks_before_broad() {
    let connect = [
        rule("0.0.0.0/0", Some("7100")),
        rule("127.0.0.1/32", Some("7001")),
    ];

    let pf = pf("10.40.0.2", &connect, &[]);
    let fw = ipfw(7, 1000, &connect, &[]);

    let at = |t: &str, s: &str| t.find(s).unwrap_or_else(|| panic!("{s} in {t}"));
    assert!(
        at(&pf, "to 127.0.0.1/32 port 7001")
            < at(&pf, "block drop out quick from 10.40.0.2 to 127.0.0.0/8")
    );
    assert!(at(&pf, "to 127.0.0.0/8") < at(&pf, "to 0.0.0.0/0 port 7100"));
    assert_eq!(
        fw[0],
        "1000 allow tcp from any to 127.0.0.1/32 7001 out jail 7"
    );
    assert!(
        fw.iter()
            .any(|l| l.ends_with("deny udp from any to 169.254.0.0/16 out jail 7"))
    );
    assert!(
        fw.last()
            .unwrap()
            .ends_with("deny udp from any to any out jail 7")
    );
}

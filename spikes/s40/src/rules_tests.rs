#![allow(non_snake_case)]

use super::*;

fn rule(cidr: &str, ports: Option<&str>) -> Rule {
    RawRule {
        cidr: cidr.into(),
        ports: ports.map(Into::into),
    }
    .parse()
    .unwrap()
}

#[test]
fn parse____host_bits____masked_and_ranges_read() {
    let r = rule("10.1.2.3/16", Some("8000-8100"));

    assert_eq!(r.net, Net::V4([10, 1, 0, 0], 16));
    assert_eq!(r.ports, Some((8000, 8100)));
    assert!(
        RawRule {
            cidr: "10.0.0.0/33".into(),
            ports: None
        }
        .parse()
        .is_err()
    );
    assert!(
        RawRule {
            cidr: "10.0.0.0/8".into(),
            ports: Some("9-1".into())
        }
        .parse()
        .is_err()
    );
}

#[test]
fn guarded____broad_rule____specials_kept_out_inside_rule_reaches_them() {
    let any = rule("0.0.0.0/0", Some("7100"));
    let lo = rule("127.0.0.1/32", Some("7001"));

    assert_eq!(any.guarded().len(), 3);
    assert!(lo.guarded().is_empty());
    assert!(!allows(&[any], "127.0.0.1".parse().unwrap(), 7100, true));
    assert!(allows(&[any], "127.0.0.1".parse().unwrap(), 7100, false));
    assert!(allows(&[any], "10.9.9.9".parse().unwrap(), 7100, true));
    assert!(allows(&[lo], "127.0.0.1".parse().unwrap(), 7001, true));
    assert!(!allows(&[lo], "127.0.0.2".parse().unwrap(), 7001, true));
}

#[test]
fn allows____mapped_address____checked_as_ipv4() {
    let lo = rule("127.0.0.1/32", Some("7001"));

    assert!(allows(
        &[lo],
        "::ffff:127.0.0.1".parse().unwrap(),
        7001,
        true
    ));
    assert!(!allows(
        &[lo],
        "::ffff:127.0.0.1".parse().unwrap(),
        7002,
        true
    ));
    assert!(!allows(&[lo], "::1".parse().unwrap(), 7001, true));
}

#[test]
fn mask_bytes____odd_prefix____partial_byte() {
    assert_eq!(
        Net::parse("fe80::/10").unwrap().mask_bytes()[..3],
        [0xff, 0xc0, 0]
    );
    assert_eq!(
        Net::parse("10.0.0.0/12").unwrap().mask_bytes(),
        vec![0xff, 0xf0, 0, 0]
    );
}

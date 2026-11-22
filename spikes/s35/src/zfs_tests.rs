use super::*;

fn cfg() -> Config {
    Config { parent: "tank/bulkhead/vol".into(), max_quota: 50 << 30, zfs: "/sbin/zfs".into() }
}

#[test]
fn names_refused() {
    for bad in ["", "-o", "a/b", "a@b", "a#b", "A", "a b", "..", "a.b", &"x".repeat(65)] {
        let req = Req::Destroy { volume: bad.into() };
        assert!(argv(&cfg(), &req).is_err(), "{bad:?}");
    }
    let req = Req::Snapshot { volume: "orders".into(), tag: "x@y".into() };
    assert!(argv(&cfg(), &req).is_err());
}

#[test]
fn quotas_checked() {
    let over = Req::Create { volume: "a".into(), refquota: (50 << 30) + 1, reservation: 0 };
    let zero = Req::SetQuota { volume: "a".into(), refquota: 0 };
    let resv = Req::Create { volume: "a".into(), refquota: 10, reservation: 11 };
    assert!(argv(&cfg(), &over).is_err());
    assert!(argv(&cfg(), &zero).is_err());
    assert!(argv(&cfg(), &resv).is_err());
}

#[test]
fn create_line() {
    let req = Req::Create { volume: "orders-data".into(), refquota: 10 << 30, reservation: 0 };
    let a = argv(&cfg(), &req).unwrap();
    assert_eq!(a.first().unwrap(), "create");
    assert!(a.contains(&"exec=off".to_string()));
    assert!(a.contains(&format!("refquota={}", 10u64 << 30)));
    assert_eq!(a.last().unwrap(), "tank/bulkhead/vol/orders-data");
}

/// Captured on the Pi (OpenZFS 2.4.1, Linux) and the FreeBSD VM (2.4.0):
/// the same columns and forms on both.
#[test]
fn list_parsed() {
    let out = "tank/bulkhead/vol\t294912\t98304\t0\t0\t/srv/vol\tyes\n\
               tank/bulkhead/vol/orders\t65536\t32768\t67108864\t0\t/srv/vol/orders\tyes\n\
               tank/bulkhead/vol/orders@pre-2\t0\t32768\t-\t-\t-\t-\n";
    let rows = parse_list("tank/bulkhead/vol", out).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].volume, None);
    assert_eq!(rows[1].volume.as_deref(), Some("orders"));
    assert_eq!(rows[1].refquota, 64 << 20);
    assert!(rows[1].mounted);
    assert_eq!(rows[2].tag.as_deref(), Some("pre-2"));
    assert_eq!(rows[2].mountpoint, None);
    assert!(parse_list("tank/other", out).is_err());
}

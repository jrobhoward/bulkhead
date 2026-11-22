#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

/// As OpenZFS 2.4 prints it on Linux and FreeBSD (spike S35).
const LISTED: &str = "tank/v\t294912\t98304\t0\t/srv/v\tno\n\
                      tank/v/orders\t196608\t98304\t0\t/srv/v/orders\tno\n\
                      tank/v/orders/data\t98304\t65536\t67108864\t/srv/v/orders/data\tyes\n\
                      tank/v/orders/logs\t98304\t98304\t1048576\t/srv/v/orders/logs\tno\n";

fn row(refquota: u64, mounted: bool) -> Row {
    Row {
        name: "tank/v/a/b".into(),
        used: 1,
        referenced: 1,
        refquota,
        mountpoint: Some("/srv/v/a/b".into()),
        mounted,
    }
}

#[test]
fn parse_list____zfs_output____rows() {
    let rows = parse_list(LISTED).unwrap();

    assert_eq!(rows.len(), 4);
    assert_eq!(rows[2].name, "tank/v/orders/data");
    assert_eq!(rows[2].refquota, 64 << 20);
    assert_eq!(rows[2].referenced, 65536);
    assert!(rows[2].mounted);
    assert!(!rows[3].mounted);
}

#[test]
fn parse_list____a_short_line_or_a_bad_size____refused() {
    assert!(parse_list("tank/v\t1\t0\n").is_err());
    assert!(parse_list("tank/v\tlots\t1\t0\t/srv\tyes\n").is_err());
}

#[test]
fn parse_list____no_mountpoint____none() {
    let rows = parse_list("tank/v\t1\t1\t0\tnone\tno\n").unwrap();

    assert_eq!(rows[0].mountpoint, None);
}

#[test]
fn volumes____rows_under_the_parent____two_levels_down_only() {
    let rows = parse_list(LISTED).unwrap();

    let vols: Vec<(&str, &str)> = volumes("tank/v", &rows)
        .into_iter()
        .map(|(a, v, _)| (a, v))
        .collect();

    assert_eq!(vols, [("orders", "data"), ("orders", "logs")]);
}

#[test]
fn ensure_steps____nothing_yet____the_app_then_the_volume() {
    assert_eq!(
        ensure_steps(false, None, 1),
        [Step::CreateApp, Step::Create]
    );
    assert_eq!(ensure_steps(true, None, 1), [Step::Create]);
}

#[test]
fn ensure_steps____there_mounted_at_its_quota____nothing() {
    assert!(ensure_steps(true, Some(&row(5, true)), 5).is_empty());
}

#[test]
fn ensure_steps____another_quota_and_unmounted____set_then_mount() {
    assert_eq!(
        ensure_steps(true, Some(&row(5, false)), 6),
        [Step::SetQuota, Step::Mount]
    );
}

#[test]
fn step_args____create____the_fixed_properties_and_the_quota() {
    let a = step_args(Step::Create, "tank/v", "orders", "data", 1 << 30);

    assert_eq!(
        a,
        [
            "create",
            "-o",
            "exec=off",
            "-o",
            "setuid=off",
            "-o",
            "devices=off",
            "-o",
            "canmount=on",
            "-o",
            "refquota=1073741824",
            "tank/v/orders/data"
        ]
    );
}

#[test]
fn step_args____the_others____on_the_volume_or_its_app() {
    assert_eq!(
        step_args(Step::CreateApp, "tank/v", "orders", "data", 1),
        ["create", "-o", "canmount=off", "tank/v/orders"]
    );
    assert_eq!(
        step_args(Step::SetQuota, "tank/v", "orders", "data", 7),
        ["set", "refquota=7", "tank/v/orders/data"]
    );
    assert_eq!(
        step_args(Step::Mount, "tank/v", "orders", "data", 7),
        ["mount", "tank/v/orders/data"]
    );
    assert_eq!(destroy_args("tank/v/o/d"), ["destroy", "-r", "tank/v/o/d"]);
}

#[test]
fn run____a_failing_program____its_error() {
    let err = run(
        Path::new("/bin/sh"),
        &["-c".into(), "echo nope >&2; exit 1".into()],
    )
    .unwrap_err();

    assert!(err.contains("nope"), "{err}");
}

#[test]
fn run____an_empty_environment____nothing_inherited() {
    let out = run(
        Path::new("/bin/sh"),
        &["-c".into(), "echo \"${HOME:-unset}\"".into()],
    )
    .unwrap();

    assert_eq!(out.trim(), "unset");
}

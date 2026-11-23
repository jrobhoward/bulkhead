#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

const GOOD: &str = "
node = /usr/local/bin/bulkhead            # the node
node_args = node --config /etc/bulkhead/node.toml
node_uid = 990
node_gid = 990
runner = /usr/local/libexec/bulkhead-runner
uids = 60000-60999
state = /var/lib/bulkhead-priv
binds = /var/lib/bulkhead, /srv/bulkhead
memory_max = 8GiB
pids_max = 4096
ports = 80, 443
cgroup = /sys/fs/cgroup/system.slice/bulkhead.service
";

#[test]
fn parse____a_full_file____every_value() {
    let c = Config::parse(GOOD).unwrap();

    assert_eq!(c.node_args, ["node", "--config", "/etc/bulkhead/node.toml"]);
    assert_eq!(c.uids, 60000..=60999);
    assert_eq!(c.binds.len(), 2);
    assert_eq!(c.memory_max, 8 << 30);
    assert_eq!(c.ports, [80, 443]);
    assert_eq!(c.tmp_size, 16 << 20);
    assert!(c.cgroup.is_some());
    assert_eq!(c.devfs_ruleset, 4290);
    assert!(c.unconfined.is_empty());
}

#[test]
fn parse____unconfined____the_listed_apps() {
    let c = Config::parse(&format!("{GOOD}unconfined = kafka-bridge, amqp\n")).unwrap();

    assert_eq!(c.unconfined, ["kafka-bridge", "amqp"]);
}

#[test]
fn parse____zfs_parent____the_volumes_settings_with_the_default_program() {
    let c = Config::parse(&format!(
        "{GOOD}zfs_parent = tank/bulkhead/volumes\nzfs_max_quota = 50GiB\n"
    ))
    .unwrap();

    let zfs = c.zfs.unwrap();
    assert_eq!(zfs.parent, "tank/bulkhead/volumes");
    assert_eq!(zfs.max_quota, 50 << 30);
    assert_eq!(zfs.program, std::path::Path::new(ZFS_PROGRAM));
    assert!(Config::parse(GOOD).unwrap().zfs.is_none());
}

#[rstest]
#[case("zfs_parent = tank/v@snap\nzfs_max_quota = 1GiB", "not a dataset name")]
#[case("zfs_parent = -o/x\nzfs_max_quota = 1GiB", "not a dataset name")]
#[case("zfs_parent = tank//v\nzfs_max_quota = 1GiB", "not a dataset name")]
#[case("zfs_parent = tank/v", "zfs_max_quota is missing")]
#[case("zfs_parent = tank/v\nzfs_max_quota = 0", "above 0")]
#[case(
    "zfs_parent = tank/v\nzfs_max_quota = 1GiB\nzfs = zfs",
    "absolute path"
)]
#[case("zfs_max_quota = 1GiB", "need zfs_parent")]
fn parse____bad_zfs_settings____refused_saying_why(#[case] lines: &str, #[case] says: &str) {
    let err = Config::parse(&format!("{GOOD}{lines}\n")).unwrap_err();

    assert!(err.contains(says), "{err}");
}

#[rstest]
#[case("tank", true)]
#[case("tank/bulkhead/volumes", true)]
#[case("zroot/a_b-c.d:e", true)]
#[case("tank/v@s", false)]
#[case("tank/v#b", false)]
#[case("tank/a b", false)]
#[case("/tank", false)]
#[case("tank/", false)]
#[case("tank/-o", false)]
fn is_dataset_name____a_name____accepted_when_a_dataset_path(#[case] name: &str, #[case] ok: bool) {
    assert_eq!(is_dataset_name(name), ok);
}

#[rstest]
#[case("node_uid = 990", "node_uid = 0", "root")]
#[case("node_uid = 990", "node_uid = 60001", "inside uids")]
#[case("uids = 60000-60999", "uids = 0-10", "above 0")]
#[case("uids = 60000-60999", "uids = 10-5", "low to high")]
#[case(
    "binds = /var/lib/bulkhead, /srv/bulkhead",
    "binds = /",
    "may not include /"
)]
#[case(
    "binds = /var/lib/bulkhead, /srv/bulkhead",
    "binds = /var/../etc",
    "absolute path"
)]
#[case("ports = 80, 443", "devfs_ruleset = 4", "above 4")]
#[case(
    "runner = /usr/local/libexec/bulkhead-runner",
    "runner = bulkhead-runner",
    "absolute path"
)]
#[case(
    "cgroup = /sys/fs/cgroup/system.slice/bulkhead.service",
    "cgroup = /tmp/cg",
    "under /sys/fs/cgroup"
)]
#[case("pids_max = 4096", "pids_max = 0", "above 0")]
#[case("memory_max = 8GiB", "memory_max = lots", "not a size")]
#[case("ports = 80, 443", "unconfined = ../etc", "not an app name")]
fn parse____a_bad_value____refused_saying_why(
    #[case] from: &str,
    #[case] to: &str,
    #[case] says: &str,
) {
    let text = GOOD.replace(from, to);

    let err = Config::parse(&text).unwrap_err();

    assert!(err.contains(says), "{err}");
}

#[test]
fn parse____unknown_key____refused() {
    let err = Config::parse(&format!("{GOOD}\nfences = none\n")).unwrap_err();

    assert!(err.contains("unknown key fences"), "{err}");
}

#[test]
fn parse____cpu_max_removed____refused_as_unknown() {
    let err = Config::parse(&format!("{GOOD}\ncpu_max = 2\n")).unwrap_err();

    assert!(err.contains("unknown key cpu_max"), "{err}");
}

#[test]
fn parse____a_key_twice____refused() {
    let err = Config::parse(&format!("{GOOD}\npids_max = 1\n")).unwrap_err();

    assert!(err.contains("twice"), "{err}");
}

#[test]
fn parse____a_key_missing____refused() {
    let err = Config::parse(&GOOD.replace("runner =", "# runner =")).unwrap_err();

    assert!(err.contains("runner is missing"), "{err}");
}

#[rstest]
#[case("4096", Some(4096))]
#[case("64KiB", Some(64 << 10))]
#[case("64 MiB", Some(64 << 20))]
#[case("2GiB", Some(2 << 30))]
#[case("1TiB", Some(1 << 40))]
#[case("1.5GiB", None)]
#[case("", None)]
#[case("18446744073709551615GiB", None)]
fn size____text____bytes(#[case] text: &str, #[case] bytes: Option<u64>) {
    assert_eq!(size(text), bytes);
}

#[test]
fn load____writable_by_others____refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("priv.conf");
    std::fs::write(&path, GOOD).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

    let err = Config::load(&path).unwrap_err();

    assert!(err.contains("owned by root"), "{err}");
}

#[rstest]
#[case("", Some(30))]
#[case("node_watchdog_s = 0\n", None)]
#[case("node_watchdog_s = 60\n", Some(60))]
fn parse____node_watchdog_s____the_default_off_or_given(
    #[case] line: &str,
    #[case] secs: Option<u64>,
) {
    let c = Config::parse(&format!("{GOOD}{line}")).unwrap();

    assert_eq!(c.node_watchdog, secs.map(std::time::Duration::from_secs));
}

#[test]
fn parse____node_watchdog_s_shorter_than_two_pings____refused() {
    let err = Config::parse(&format!("{GOOD}node_watchdog_s = 5\n")).unwrap_err();

    assert!(err.contains("at least 10"), "{err}");
}

/// FreeBSD needs `ipfw_rules` with own sockets; given here for both.
const OWN: &str = "
own_sockets = mqtt-bridge, sink
own_connect.mqtt-bridge = 10.20.0.0/16:1883, 127.0.0.53/32:53, ::1/128
own_bind.mqtt-bridge = 0.0.0.0/0:9000-9009
own_connect.sink =
own_listen = mqtt-bridge
ipfw_rules = 30000-39999
";

#[test]
fn parse____own_sockets____each_apps_rules() {
    let c = Config::parse(&format!("{GOOD}{OWN}")).unwrap();

    let bridge = &c.own_sockets["mqtt-bridge"];
    let sink = &c.own_sockets["sink"];
    let shown: Vec<String> = bridge.connect.iter().map(ToString::to_string).collect();
    assert_eq!(shown, ["10.20.0.0/16:1883", "127.0.0.53/32:53", "::1/128"]);
    assert_eq!(bridge.bind.len(), 1);
    assert!(bridge.listen);
    assert!(sink.connect.is_empty() && sink.bind.is_empty() && !sink.listen);
    let ipfw = c.ipfw.unwrap();
    assert_eq!(ipfw.rules, 30000..=39999);
    assert_eq!(ipfw.program, PathBuf::from("/sbin/ipfw"));
}

/// `ipfw_rules` beside an own-sockets case, as FreeBSD needs.
const RULES: &str = "ipfw_rules = 30000-39999\n";

#[rstest]
#[case(RULES, "own_sockets = a\n", "own_connect.a is missing")]
#[case(
    RULES,
    "own_sockets = a\nown_connect.a =\nown_bind.b = 0.0.0.0/0\n",
    "b has own-sockets keys"
)]
#[case(
    RULES,
    "own_sockets = a\nown_connect.a =\nown_listen = b\n",
    "b has own-sockets keys"
)]
#[case(
    RULES,
    "own_sockets = a\nown_connect.a = 10.0.0.0/33\n",
    "own_connect.a"
)]
#[case(
    RULES,
    "own_sockets = a\nown_connect.a =\nunconfined = a\n",
    "unconfined and in own_sockets"
)]
#[case(RULES, "own_sockets = ../a\n", "not an app name")]
#[case("", "ipfw_rules = 0-10\n", "ipfw_rules")]
#[case("", "ipfw_rules = 30000-70000\n", "ipfw_rules")]
#[case("", "ipfw = /sbin/ipfw\n", "ipfw needs ipfw_rules")]
fn parse____bad_own_sockets____refused(
    #[case] rules: &str,
    #[case] extra: &str,
    #[case] message: &str,
) {
    let err = Config::parse(&format!("{GOOD}{rules}{extra}")).unwrap_err();

    assert!(err.contains(message), "{err}");
}

#[cfg(target_os = "linux")]
#[test]
fn parse____own_sockets_without_cgroup____refused_on_linux() {
    let no_cgroup = GOOD.replace(
        "cgroup = /sys/fs/cgroup/system.slice/bulkhead.service\n",
        "",
    );

    let err = Config::parse(&format!("{no_cgroup}{OWN}")).unwrap_err();

    assert!(err.contains("needs cgroup"), "{err}");
}

#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

fn binding(dir: &Path, resolv_conf: &str, nameservers: &[&str]) -> ResolverBinding {
    std::fs::write(dir.join("resolv.conf"), resolv_conf).unwrap();
    std::fs::write(dir.join("hosts"), "10.1.2.3 db.internal\n").unwrap();
    ResolverBinding {
        resolv_conf: dir.join("resolv.conf"),
        hosts: dir.join("hosts"),
        nameservers: nameservers.iter().map(ToString::to_string).collect(),
        reload_s: 5,
    }
}

#[test]
fn nameservers_of____a_resolv_conf____its_nameserver_lines_on_port_53() {
    let text = "# comment\nnameserver 127.0.0.53\nsearch corp.example\n\
                nameserver ::1\nnameserver fe80::1%eth0\n; nameserver 10.9.9.9\nnameserver not-an-address\n";

    let found = nameservers_of(text);

    assert_eq!(
        found,
        vec![
            "127.0.0.53:53".parse::<SocketAddr>().unwrap(),
            "[::1]:53".parse().unwrap()
        ]
    );
}

#[test]
fn nameservers_of____more_than_eight____the_first_eight() {
    let text: String = (1..=10)
        .map(|i| format!("nameserver 10.0.0.{i}\n"))
        .collect();

    assert_eq!(nameservers_of(&text).len(), MAX_NAMESERVERS);
}

#[test]
fn read____nameservers_set____they_replace_resolv_conf_s() {
    let dir = tempfile::tempdir().unwrap();
    let b = binding(
        dir.path(),
        "nameserver 10.0.0.1\n",
        &["127.0.0.1:5353", "10.0.0.2"],
    );

    let snapshot = Snapshot::read(&b).unwrap();

    assert_eq!(
        snapshot.nameservers,
        vec![
            "127.0.0.1:5353".parse::<SocketAddr>().unwrap(),
            "10.0.0.2:53".parse().unwrap()
        ]
    );
    assert_eq!(snapshot.hosts, "10.1.2.3 db.internal\n");
}

#[test]
fn read____no_nameserver_anywhere____refused() {
    let dir = tempfile::tempdir().unwrap();
    let b = binding(dir.path(), "search corp.example\n", &[]);

    let refused = Snapshot::read(&b).unwrap_err();

    assert!(refused.contains("names no nameserver"), "{refused}");
}

#[test]
fn read____hosts_missing____empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = binding(dir.path(), "nameserver 10.0.0.1\n", &[]);
    b.hosts = dir.path().join("absent");

    assert_eq!(Snapshot::read(&b).unwrap().hosts, "");
}

#[test]
fn read____resolv_conf_missing_with_nameservers_set____read() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = binding(dir.path(), "", &["10.0.0.1"]);
    b.resolv_conf = dir.path().join("absent");

    assert_eq!(Snapshot::read(&b).unwrap().resolv_conf, "");
}

#[test]
fn read____resolv_conf_missing____refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = binding(dir.path(), "", &[]);
    b.resolv_conf = dir.path().join("absent");

    assert!(Snapshot::read(&b).is_err());
}

#[test]
fn read____file_replaced_by_rename____the_new_text_read() {
    let dir = tempfile::tempdir().unwrap();
    let b = binding(dir.path(), "nameserver 10.0.0.1\n", &[]);
    let before = Snapshot::read(&b).unwrap();
    std::fs::write(dir.path().join("new"), "nameserver 10.0.0.2\n").unwrap();
    std::fs::rename(dir.path().join("new"), &b.resolv_conf).unwrap();

    let after = Snapshot::read(&b).unwrap();

    assert_ne!(before, after);
    assert_eq!(after.nameservers, vec!["10.0.0.2:53".parse().unwrap()]);
}

#[test]
fn manifest____nameservers____tcp_to_each_alone_and_messages_from_itself() {
    let snapshot = Snapshot {
        resolv_conf: String::new(),
        hosts: String::new(),
        nameservers: vec![
            "127.0.0.53:53".parse().unwrap(),
            "[::1]:5353".parse().unwrap(),
        ],
    };

    let spec = manifest(&snapshot).unwrap().spec;

    let sockets = spec.capabilities.sockets.as_ref().unwrap();
    let rules: Vec<(String, Option<String>)> = sockets
        .connect
        .iter()
        .map(|r| (r.cidr.clone(), r.ports.map(|p| p.to_string())))
        .collect();
    assert_eq!(
        rules,
        vec![
            ("127.0.0.53/32".to_string(), Some("53".to_string())),
            ("::1/128".to_string(), Some("5353".to_string())),
        ]
    );
    assert!(sockets.bind.is_empty());
    assert!(!sockets.listen);
    let acl = spec.messaging_acl.as_ref().unwrap();
    assert_eq!(acl.accept_from, vec![RESOLVE_APP.to_string()]);
    assert_eq!(acl.may_call, vec![RESOLVE_APP.to_string()]);
}

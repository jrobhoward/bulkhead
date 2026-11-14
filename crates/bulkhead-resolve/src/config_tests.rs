#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::time::Duration;

use hickory_resolver::config::ProtocolConfig;

use super::*;

fn args(nameservers: &[&str], resolv_conf: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "nameservers": nameservers,
        "resolv_conf": resolv_conf,
        "hosts": "127.0.0.1 localhost\n10.1.2.3 db.internal db\n",
    }))
    .unwrap()
}

#[test]
fn resolver____nameservers_with_ports____each_over_tcp_alone_on_its_port() {
    let config = Config::from_args(&args(&["127.0.0.53:53", "[::1]:5353"], "")).unwrap();

    let (resolver, _) = config.resolver().unwrap();

    let servers: Vec<(String, Vec<(u16, bool)>)> = resolver
        .name_servers()
        .iter()
        .map(|s| {
            (
                s.ip.to_string(),
                s.connections
                    .iter()
                    .map(|c| (c.port, matches!(c.protocol, ProtocolConfig::Tcp)))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        servers,
        vec![
            ("127.0.0.53".to_string(), vec![(53, true)]),
            ("::1".to_string(), vec![(5353, true)]),
        ]
    );
}

#[test]
fn resolver____resolv_conf_search_and_options____taken_and_its_nameservers_ignored() {
    let config = Config::from_args(&args(
        &["10.0.0.1:53"],
        "nameserver 192.0.2.9\nsearch corp.example lab.example\noptions ndots:2 timeout:3 attempts:4\n",
    ))
    .unwrap();

    let (resolver, options) = config.resolver().unwrap();

    let search: Vec<String> = resolver.search().iter().map(ToString::to_string).collect();
    assert_eq!(search, vec!["corp.example", "lab.example"]);
    assert_eq!(resolver.name_servers().len(), 1);
    assert_eq!(options.ndots, 2);
    assert_eq!(options.timeout, Duration::from_secs(3));
    assert_eq!(options.attempts, 4);
    assert!(matches!(options.use_hosts_file, ResolveHosts::Never));
}

#[test]
fn resolver____empty_resolv_conf____hickory_defaults() {
    let config = Config::from_args(&args(&["10.0.0.1:53"], "")).unwrap();

    let (resolver, options) = config.resolver().unwrap();

    assert!(resolver.search().is_empty());
    assert_eq!(options.ndots, 1);
}

#[test]
fn from_args____no_nameservers____refused() {
    let refused = Config::from_args(&args(&[], "")).unwrap_err();

    assert!(refused.contains("no nameservers"), "{refused}");
}

#[test]
fn from_args____too_many_nameservers____refused() {
    let many: Vec<String> = (0..9).map(|i| format!("10.0.0.{i}:53")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();

    let refused = Config::from_args(&args(&many, "")).unwrap_err();

    assert!(refused.contains("at most 8"), "{refused}");
}

#[test]
fn from_args____unknown_key____refused() {
    let refused = Config::from_args(br#"{"nameservers": ["10.0.0.1:53"], "extra": 1}"#);

    assert!(refused.is_err());
}

#[test]
fn hosts____a_hosts_file____its_names_known() {
    use hickory_resolver::proto::op::Query;
    use hickory_resolver::proto::rr::{Name, RecordType};
    let config = Config::from_args(&args(&["10.0.0.1:53"], "")).unwrap();

    let hosts = config.hosts().unwrap();

    let query = Query::query(Name::from_ascii("db.internal.").unwrap(), RecordType::A);
    let found = hosts.lookup_static_host(&query).unwrap();
    let addrs: Vec<String> = found.answers().iter().map(|r| r.data.to_string()).collect();
    assert_eq!(addrs, vec!["10.1.2.3"]);
}

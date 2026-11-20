#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn SqlDatasource____a_socket_directory_or_a_host____unix_socket_or_tcp_endpoint() {
    let mut socket = SqlDatasource::new("/run/postgresql", "app", "app");
    socket.port = 5433;
    let tcp = SqlDatasource::new("db.internal", "app", "app");

    assert_eq!(
        socket.endpoint(),
        DriverEndpoint::Unix(PathBuf::from("/run/postgresql/.s.PGSQL.5433"))
    );
    assert_eq!(
        tcp.endpoint(),
        DriverEndpoint::Tcp {
            host: "db.internal".into(),
            port: 5432,
            timeout: Duration::from_secs(5),
        }
    );
}

#[test]
fn SqlDatasource____password_and_ca_files____in_the_profile_and_under_driver() {
    let mut ds = SqlDatasource::new("db.internal", "shop", "shop");
    ds.password = Some(SecretSource::File("/etc/bulkhead/pg.password".into()));
    ds.tls = SqlTls::Require {
        ca_file: "/etc/bulkhead/pg-ca.pem".into(),
    };

    let profile = ds.driver_profile();
    let args = ds.driver_args("main").unwrap();

    assert_eq!(
        profile.files,
        vec![
            PathBuf::from("/etc/bulkhead/pg.password"),
            PathBuf::from("/etc/bulkhead/pg-ca.pem")
        ]
    );
    assert_eq!(profile.connect, Some(ds.endpoint()));
    assert!(profile.listeners.is_empty() && profile.calls.is_empty());
    assert_eq!(args["password"]["file"], "/driver/pg.password");
    assert_eq!(args["tls"]["require"]["ca_file"], "/driver/pg-ca.pem");
    assert_eq!(args["transport"], "tcp");
    assert_eq!(args["datasource"], "main");
}

#[test]
fn SqlDatasource____files_with_one_name____refused() {
    let mut ds = SqlDatasource::new("db.internal", "shop", "shop");
    ds.password = Some(SecretSource::File("/a/secret.pem".into()));
    ds.tls = SqlTls::Require {
        ca_file: "/b/secret.pem".into(),
    };

    let got = ds.driver_args("main").unwrap_err();

    assert!(got.contains("same file name"), "{got}");
}

#[test]
fn SqlDatasource____password_from_an_unset_variable____refused_naming_it() {
    let mut ds = SqlDatasource::new("/run/postgresql", "shop", "shop");
    ds.password = Some(SecretSource::Env("BULKHEAD_TEST_UNSET_PG_PASSWORD".into()));

    let got = ds.driver_args("main").unwrap_err();

    assert!(got.contains("BULKHEAD_TEST_UNSET_PG_PASSWORD"), "{got}");
}

#[test]
fn SqlDatasource____call_timeout____past_the_drivers_own_bound() {
    let ds = SqlDatasource::new("db", "app", "app");

    let bound = ds.acquire_timeout + ds.connect_timeout * 2 + ds.statement_timeout;

    assert!(ds.call_timeout() > bound);
}

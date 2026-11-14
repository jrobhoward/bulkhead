#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

const WAIT: Duration = Duration::from_secs(1);

fn table() -> Names {
    Names::new(Resolver::Table(BTreeMap::from([(
        "db.internal".to_string(),
        vec!["10.1.2.3".parse().unwrap(), "::1".parse().unwrap()],
    )])))
}

#[rstest]
#[case("10.0.0.1", "10.0.0.1:80")]
#[case("::1", "[::1]:80")]
#[case("[::1]", "[::1]:80")]
#[tokio::test]
async fn resolve____an_address_as_text____read_here_whatever_the_resolver(
    #[case] host: &str,
    #[case] expected: &str,
) {
    let names = Names::new(Resolver::None);

    let found = names.resolve(host, 80, WAIT).await.unwrap();

    assert_eq!(found, vec![expected.parse::<SocketAddr>().unwrap()]);
}

#[tokio::test]
async fn resolve____a_name_in_the_table____its_addresses_on_the_port() {
    let found = table().resolve("DB.Internal", 5432, WAIT).await.unwrap();

    assert_eq!(
        found,
        vec![
            "10.1.2.3:5432".parse::<SocketAddr>().unwrap(),
            "[::1]:5432".parse().unwrap()
        ]
    );
}

#[tokio::test]
async fn resolve____a_name_not_in_the_table____not_found() {
    let got = table().resolve("other.internal", 80, WAIT).await;

    assert_eq!(
        got,
        Err(ResolveError::NotFound("other.internal".to_string()))
    );
}

#[tokio::test]
async fn resolve____no_resolver____refused() {
    let got = Names::new(Resolver::None)
        .resolve("db.internal", 80, WAIT)
        .await;

    assert!(matches!(got, Err(ResolveError::Refused(_))), "{got:?}");
}

#[tokio::test]
async fn resolve____the_driver_never_runs____temporary_once_the_time_is_up() {
    // Not attached to a runtime: as a runtime stopping.
    let names = Names::new(Resolver::App("bulkhead-resolve".into()));

    let got = names.resolve("db.internal", 80, WAIT).await;

    assert!(matches!(got, Err(ResolveError::Temporary(_))), "{got:?}");
}

#[test]
fn is_resolver____the_driver_and_another_app____only_the_driver() {
    let names = Names::new(Resolver::App("bulkhead-resolve".into()));

    assert!(names.is_resolver("bulkhead-resolve"));
    assert!(!names.is_resolver("shop"));
    assert!(!table().is_resolver("bulkhead-resolve"));
}

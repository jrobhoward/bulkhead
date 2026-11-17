#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use bulkhead_native::protocol::{self as p, Bytes, FdRef};
use rstest::rstest;

use super::*;

const LIMITS: Limits = Limits {
    refusals: 3,
    period: Duration::from_secs(10),
    per_sec: 0,
};

#[test]
fn count____refusals_up_to_the_limit____none_over() {
    let (mut c, t) = (Counts::default(), Instant::now());

    let over: Vec<_> = (0..3)
        .map(|_| c.count(LIMITS, Some(Refusal::AccessDenied), t))
        .collect();

    assert_eq!(over, [None, None, None]);
}

#[test]
fn count____one_refusal_past_the_limit____over_once() {
    let (mut c, t) = (Counts::default(), Instant::now());
    for _ in 0..3 {
        c.count(LIMITS, Some(Refusal::Acl), t);
    }

    let over = c.count(LIMITS, Some(Refusal::AccessDenied), t);
    let again = c.count(LIMITS, Some(Refusal::AccessDenied), t);

    assert_eq!(over, Some(Over::Refusals));
    assert_eq!(again, None, "counted no more once over");
}

#[test]
fn count____refusals_spread_past_the_period____never_over() {
    let (mut c, t) = (Counts::default(), Instant::now());

    let over: Vec<_> = (0..10u64)
        .map(|i| c.count(LIMITS, Some(Refusal::Acl), t + Duration::from_secs(4 * i)))
        .collect();

    assert!(over.iter().all(Option::is_none), "{over:?}");
}

#[test]
fn count____answers_without_refusal____never_over() {
    let (mut c, t) = (Counts::default(), Instant::now());

    let over = (0..10_000).find_map(|_| c.count(LIMITS, None, t));

    assert_eq!(over, None);
}

#[rstest]
#[case::at_the_rate(5, None)]
#[case::one_past(6, Some(Over::Rate))]
fn count____requests_in_one_second____over_past_the_rate(
    #[case] n: u32,
    #[case] expected: Option<Over>,
) {
    let limits = Limits {
        per_sec: 5,
        ..LIMITS
    };
    let (mut c, t) = (Counts::default(), Instant::now());

    let over = (0..n).find_map(|_| c.count(limits, None, t));

    assert_eq!(over, expected);
}

#[test]
fn count____requests_at_the_rate_every_second____never_over() {
    let limits = Limits {
        per_sec: 5,
        ..LIMITS
    };
    let (mut c, t) = (Counts::default(), Instant::now());

    let over = (0..50u64).find_map(|i| c.count(limits, None, t + Duration::from_millis(200 * i)));

    assert_eq!(over, None);
}

#[rstest]
#[case::call_acl(Answer::Called(Err(p::CallError::AccessDenied)), Some(Refusal::Acl))]
#[case::cast_acl(Answer::CastFailed(p::CastError::AccessDenied), Some(Refusal::Acl))]
#[case::register(
    Answer::Registered(Err(p::RegisterError::AccessDenied)),
    Some(Refusal::AccessDenied)
)]
#[case::kv(Answer::Kv(Err(p::KvError::AccessDenied)), Some(Refusal::AccessDenied))]
#[case::kv_open(
    Answer::KvOpened(Err(p::KvError::AccessDenied)),
    Some(Refusal::AccessDenied)
)]
#[case::groups(
    Answer::Groups(Err(p::GroupsError::AccessDenied)),
    Some(Refusal::AccessDenied)
)]
#[case::config(
    Answer::Config(Err(p::ConfigError::AccessDenied)),
    Some(Refusal::AccessDenied)
)]
#[case::config_all(
    Answer::ConfigAll(Err(p::ConfigError::AccessDenied)),
    Some(Refusal::AccessDenied)
)]
#[case::secret(
    Answer::Secret(Err(p::SecretError::AccessDenied)),
    Some(Refusal::AccessDenied)
)]
#[case::metric(
    Answer::Metric(Err(p::MetricError::AccessDenied)),
    Some(Refusal::AccessDenied)
)]
#[case::sql(
    Answer::Sql(Err(p::SqlError::AccessDenied)),
    Some(Refusal::AccessDenied)
)]
#[case::net(Answer::Net(Err(p::NetError::Denied)), Some(Refusal::AccessDenied))]
#[case::http(Answer::Http(Err(p::HttpError::Denied)), Some(Refusal::AccessDenied))]
#[case::blob(Answer::Blob(Err(p::BlobError::Denied)), Some(Refusal::AccessDenied))]
#[case::admin(
    Answer::AdminStream(Err(p::NetError::Denied)),
    Some(Refusal::AccessDenied)
)]
#[case::metrics_text(
    Answer::MetricsText(Err(p::NetError::Denied)),
    Some(Refusal::AccessDenied)
)]
#[case::connected(
    Answer::Connected(Err(p::NetError::Denied)),
    Some(Refusal::AccessDenied)
)]
#[case::unit(Answer::Unit, None)]
#[case::ref_(Answer::Ref(1), None)]
#[case::call_timeout(Answer::Called(Err(p::CallError::Timeout)), None)]
#[case::cast_overloaded(Answer::CastFailed(p::CastError::Overloaded), None)]
#[case::register_taken(Answer::Registered(Ok(())), None)]
#[case::kv_unavailable(Answer::Kv(Err(p::KvError::Unavailable)), None)]
#[case::groups_limit(Answer::Groups(Err(p::GroupsError::LimitReached)), None)]
#[case::secret_missing(Answer::Secret(Err(p::SecretError::NotFound)), None)]
#[case::sql_timeout(Answer::Sql(Err(p::SqlError::Timeout)), None)]
#[case::net_limited(Answer::Net(Err(p::NetError::Limited)), None)]
#[case::http_timeout(Answer::Http(Err(p::HttpError::Timeout)), None)]
#[case::extension(Answer::Extension(Err(p::ExtensionError::NoSuchExtension)), None)]
#[case::extension_ok(Answer::Extension(Ok(Bytes(Vec::new()))), None)]
#[case::elsewhere(Answer::Elsewhere(None), None)]
#[case::connected_ok(Answer::Connected(Ok(FdRef(1))), None)]
#[case::blob_quota(Answer::Blob(Err(p::BlobError::OverQuota)), None)]
fn refusal____an_answer____its_kind(#[case] answer: Answer, #[case] kind: Option<Refusal>) {
    assert_eq!(refusal(&answer), kind);
}

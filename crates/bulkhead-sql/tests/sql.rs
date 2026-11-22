//! The SQL driver as a node runs it, against a scratch Postgres: the
//! node's requests as calls, every connection a stream the runtime opened
//! (its Unix socket, or TCP to a name it resolved), the password and the
//! CA certificates files under `/driver/`. Skipped without Postgres.

#![cfg(any(target_os = "linux", target_os = "freebsd"))]
#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use bulkhead_native::protocol::{SqlAnswer, SqlError, SqlRequest, SqlValue};
use common::pg::{Options, Scratch};
use serde_json::json;

fn int(i: i64) -> SqlValue {
    SqlValue::Int(i)
}

fn text(s: &str) -> SqlValue {
    SqlValue::Text(s.into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____over_the_unix_socket____statements_with_parameters_run() {
    let Some(pg) = Scratch::start() else { return };
    let s = common::serve(
        &common::args("unix", json!({})),
        Some(common::unix(&pg)),
        &[],
    )
    .await;

    s.execute("CREATE TABLE t (id int8, name text)", vec![])
        .await
        .unwrap();
    let changed = s
        .execute(
            "INSERT INTO t VALUES ($1, $2), ($3, $4)",
            vec![int(1), text("a"), int(2), text("b")],
        )
        .await
        .unwrap();
    let rows = s
        .query(
            "SELECT id, name FROM t WHERE id > $1 ORDER BY id",
            vec![int(0)],
        )
        .await
        .unwrap();

    assert_eq!(changed, 2);
    assert_eq!(rows, vec![vec![int(1), text("a")], vec![int(2), text("b")]]);
    assert!(s.counter("connections_total") >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____tcp_by_name_with_a_password_file____logged_in() {
    let Some(pg) = Scratch::start_with(&Options {
        tcp: true,
        password: Some("s3cret"),
        ..Options::default()
    }) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.password");
    std::fs::write(&file, "s3cret\n").unwrap();
    let s = common::serve(
        &common::args(
            "tcp",
            json!({"password": {"file": "/driver/main.password"}}),
        ),
        Some(common::tcp(&pg)),
        &[&file],
    )
    .await;

    let rows = s.query("SELECT current_user::text", vec![]).await.unwrap();

    assert_eq!(rows, vec![vec![text("bulkhead")]]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____password_file_rewritten_in_place____new_connections_log_in_with_it() {
    let Some(pg) = Scratch::start_with(&Options {
        tcp: true,
        password: Some("s3cret"),
        ..Options::default()
    }) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.password");
    std::fs::write(&file, "s3cret\n").unwrap();
    let s = common::serve(
        &common::args(
            "tcp",
            json!({"password": {"file": "/driver/main.password"}}),
        ),
        Some(common::tcp(&pg)),
        &[&file],
    )
    .await;
    s.query("SELECT 1", vec![]).await.unwrap();

    // The role's password changed, and the file rewritten in place (its
    // inode kept, as the driver's access is to it).
    s.query("ALTER ROLE bulkhead PASSWORD 'n3w'", vec![])
        .await
        .unwrap();
    std::fs::write(&file, "n3w\n").unwrap();
    // Its one connection ended, so the next statement needs a new one.
    let _ = s
        .query("SELECT pg_terminate_backend(pg_backend_pid())", vec![])
        .await;
    let rows = s.query("SELECT current_user::text", vec![]).await;

    assert_eq!(rows.unwrap(), vec![vec![text("bulkhead")]]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____a_wrong_password____refused_by_the_server() {
    let Some(pg) = Scratch::start_with(&Options {
        tcp: true,
        password: Some("s3cret"),
        ..Options::default()
    }) else {
        return;
    };
    let s = common::serve(
        &common::args("tcp", json!({"password": {"value": "wrong"}})),
        Some(common::tcp(&pg)),
        &[],
    )
    .await;

    let got = s.query("SELECT 1", vec![]).await;

    assert!(
        matches!(&got, Err(SqlError::Query { code, .. }) if code == "28P01"),
        "{got:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____tls_required____the_servers_certificate_checked() {
    let made = rcgen::generate_simple_self_signed(vec!["db.test".to_string()]).unwrap();
    let (cert, key) = (made.cert.pem(), made.signing_key.serialize_pem());
    let Some(pg) = Scratch::start_with(&Options {
        tcp: true,
        tls: Some((&cert, &key)),
        ..Options::default()
    }) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("main-ca.pem");
    std::fs::write(&ca, &cert).unwrap();
    let other = rcgen::generate_simple_self_signed(vec!["db.test".to_string()]).unwrap();
    let wrong = dir.path().join("wrong-ca.pem");
    std::fs::write(&wrong, other.cert.pem()).unwrap();
    let tls = |file: &str| json!({"tls": {"require": {"ca_file": format!("/driver/{file}")}}});
    let s = common::serve(
        &common::args("tcp", tls("main-ca.pem")),
        Some(common::tcp(&pg)),
        &[&ca],
    )
    .await;
    let untrusted = common::serve(
        &common::args("tcp", tls("wrong-ca.pem")),
        Some(common::tcp(&pg)),
        &[&wrong],
    )
    .await;

    let rows = s
        .query(
            "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            vec![],
        )
        .await
        .unwrap();
    let refused = untrusted.query("SELECT 1", vec![]).await;

    assert_eq!(rows, vec![vec![SqlValue::Bool(true)]]);
    assert!(
        matches!(&refused, Err(SqlError::Unavailable(_))),
        "{refused:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____transactions____committed_rolled_back_and_ended_once() {
    let Some(pg) = Scratch::start() else { return };
    let s = common::serve(
        &common::args("unix", json!({})),
        Some(common::unix(&pg)),
        &[],
    )
    .await;
    s.execute("CREATE TABLE t (id int8)", vec![]).await.unwrap();
    let begin = || async {
        match s.ask(SqlRequest::Begin("main".into())).await.unwrap() {
            SqlAnswer::Begun(tx) => tx,
            other => panic!("{other:?}"),
        }
    };
    let insert = |tx, id| SqlRequest::TxExecute {
        tx,
        statement: "INSERT INTO t VALUES ($1)".into(),
        params: vec![int(id)],
    };

    let kept = begin().await;
    let dropped = begin().await;
    let cast = begin().await;
    s.ask(insert(kept, 1)).await.unwrap();
    s.ask(insert(dropped, 2)).await.unwrap();
    s.ask(insert(cast, 3)).await.unwrap();
    let inside = s
        .ask(SqlRequest::TxQuery {
            tx: kept,
            statement: "SELECT count(*) FROM t".into(),
            params: vec![],
        })
        .await
        .unwrap();
    s.ask(SqlRequest::TxEnd {
        tx: kept,
        commit: true,
    })
    .await
    .unwrap();
    s.ask(SqlRequest::TxEnd {
        tx: dropped,
        commit: false,
    })
    .await
    .unwrap();
    s.cast(SqlRequest::TxEnd {
        tx: cast,
        commit: false,
    });
    let again = s
        .ask(SqlRequest::TxEnd {
            tx: kept,
            commit: true,
        })
        .await;
    let rows = s
        .query("SELECT id FROM t ORDER BY id", vec![])
        .await
        .unwrap();

    assert!(matches!(inside, SqlAnswer::Rows(r) if r.rows == vec![vec![int(1)]]));
    assert_eq!(rows, vec![vec![int(1)]]);
    assert!(
        matches!(&again, Err(SqlError::Unavailable(m)) if m.contains("ended")),
        "{again:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____the_pool_held_by_a_transaction____limit_reached_then_free() {
    let Some(pg) = Scratch::start() else { return };
    let s = common::serve(
        &common::args("unix", json!({"pool_size": 1, "acquire_timeout_ms": 200})),
        Some(common::unix(&pg)),
        &[],
    )
    .await;
    let SqlAnswer::Begun(tx) = s.ask(SqlRequest::Begin("main".into())).await.unwrap() else {
        panic!("not begun");
    };

    let held = s.query("SELECT 1", vec![]).await;
    s.ask(SqlRequest::TxEnd { tx, commit: false })
        .await
        .unwrap();
    let free = s.query("SELECT 1", vec![]).await;

    assert_eq!(held, Err(SqlError::LimitReached));
    assert_eq!(free, Ok(vec![vec![int(1)]]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____a_connection_back_in_the_pool____its_session_reset() {
    let Some(pg) = Scratch::start() else { return };
    let s = common::serve(
        &common::args("unix", json!({"pool_size": 1})),
        Some(common::unix(&pg)),
        &[],
    )
    .await;

    s.execute("SET search_path TO nowhere", vec![])
        .await
        .unwrap();
    s.execute("BEGIN", vec![]).await.unwrap();
    let path = s.query("SHOW search_path", vec![]).await.unwrap();
    // One transaction left open would give both statements its id.
    let first = s.query("SELECT txid_current()", vec![]).await.unwrap();
    let second = s.query("SELECT txid_current()", vec![]).await.unwrap();

    assert_eq!(path, vec![vec![text("\"$user\", public")]]);
    assert_ne!(first, second);
    // Reset, not replaced: a reset that fails makes the pool open a new
    // connection for each statement.
    assert_eq!(s.counter("connections_total"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____limits____timeout_too_large_and_unsupported() {
    let Some(pg) = Scratch::start() else { return };
    let s = common::serve(
        &common::args(
            "unix",
            json!({"statement_timeout_ms": 200, "max_result_bytes": 1000}),
        ),
        Some(common::unix(&pg)),
        &[],
    )
    .await;

    let slow = s.query("SELECT pg_sleep(5)::text", vec![]).await;
    let large = s.query("SELECT repeat('x', 2000)", vec![]).await;
    let interval = s.query("SELECT interval '1 day' AS d", vec![]).await;
    let bad = s.query("SELECT * FROM absent", vec![]).await;

    assert_eq!(slow, Err(SqlError::Timeout));
    assert_eq!(large, Err(SqlError::TooLarge));
    assert!(
        matches!(&interval, Err(SqlError::Unsupported(m)) if m.contains("interval")),
        "{interval:?}"
    );
    assert!(
        matches!(&bad, Err(SqlError::Query { code, .. }) if code == "42P01"),
        "{bad:?}"
    );
    assert!(s.counter("requests_failed_total") >= 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sql____no_endpoint_in_its_profile____unavailable() {
    let Some(_pg) = Scratch::start() else { return };
    let s = common::serve(&common::args("unix", json!({})), None, &[]).await;

    let got = s.query("SELECT 1", vec![]).await;

    assert!(
        matches!(&got, Err(SqlError::Unavailable(m)) if m.contains("nowhere")),
        "{got:?}"
    );
}

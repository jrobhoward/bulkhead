//! The driver run as the node runs it, on a runtime of the test's own:
//! `bulkhead-sql` built and started as the app `bulkhead-sql-main`, its
//! profile's endpoint the scratch Postgres (`bulkhead-host`'s
//! `tests/common/pg.rs`), its files readable under `/driver/`, and the
//! node's requests sent as calls.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

#[path = "../../../bulkhead-host/tests/common/pg.rs"]
pub mod pg;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use bulkhead_bundle::manifest::{
    AppInfo, AppManifest, AppSpec, NativeProgram, ProgramRef, Provenance,
};
use bulkhead_host::{
    AppDeployment, Apps, DriverEndpoint, DriverProfile, Envelope, NativeCode, Resolver, Runtime,
    RuntimeConfig,
};
use bulkhead_native::protocol::{SqlAnswer, SqlError, SqlRequest, SqlValue};
use serde_json::{Value, json};

/// The driver's app name, as the node gives it.
pub const SQL: &str = "bulkhead-sql-main";

/// The schema of the node's requests.
pub const SCHEMA: &str = "bulkhead/sql";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// `bulkhead-runner` and `bulkhead-sql`, built on first use where the
/// node's tests build them.
pub fn programs() -> &'static (PathBuf, PathBuf) {
    static BUILT: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let native = workspace_root().join("target/fixtures/native");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .current_dir(workspace_root())
            .args(["build", "-p", "bulkhead-runner", "-p", "bulkhead-sql"])
            .arg("--target-dir")
            .arg(&native)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "building the runner and the SQL driver failed"
        );
        (
            native.join("debug/bulkhead-runner"),
            native.join("debug/bulkhead-sql"),
        )
    })
}

/// The init arguments for the scratch server as `bulkhead`, over
/// `transport`, with `extra` fields added or replaced.
pub fn args(transport: &str, extra: Value) -> Value {
    let mut args = json!({
        "datasource": "main",
        "host": "db.test",
        "transport": transport,
        "dbname": "postgres",
        "user": "bulkhead",
        "pool_size": 4,
        "connect_timeout_ms": 5000,
        "acquire_timeout_ms": 2000,
        "statement_timeout_ms": 30000,
        "max_result_bytes": 1 << 20,
    });
    if let (Some(args), Value::Object(extra)) = (args.as_object_mut(), extra) {
        args.extend(extra);
    }
    args
}

/// The node's manifest for the driver, with `args` as its init arguments.
pub fn manifest(args: &Value) -> AppManifest {
    let spec = json!({
        "kind": "native-process",
        "native": { "program": "executable" },
        "instances": { "count": 1 },
        "lifecycle": { "start_args": args },
        "supervision": { "intensity": 100, "backoff": { "initial_ms": 1, "max_ms": 50 } },
        "limits": { "memory_max": "128MiB" },
        "capabilities": {
            "bulkhead:metrics": { "prefix": "bulkhead_sql" },
        },
    });
    AppManifest {
        app: AppInfo {
            name: SQL.into(),
            version: "1.0.0".parse().unwrap(),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        program: ProgramRef {
            path: String::new(),
            checksum: String::new(),
        },
        provenance: Provenance {
            signer_key: String::new(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(&spec).unwrap(),
    }
}

/// The scratch server's Unix socket, as the node's endpoint.
pub fn unix(pg: &pg::Scratch) -> DriverEndpoint {
    DriverEndpoint::Unix(pg.socket_dir().join(format!(".s.PGSQL.{}", pg.port())))
}

/// The scratch server's TCP port, by the name `db.test`.
pub fn tcp(pg: &pg::Scratch) -> DriverEndpoint {
    DriverEndpoint::Tcp {
        host: "db.test".into(),
        port: pg.port(),
        timeout: Duration::from_secs(5),
    }
}

/// A runtime with the driver running.
pub struct Served {
    pub rt: Runtime,
    pub apps: Apps,
}

/// Start the driver with `args`, connected to `endpoint`, reading
/// `files`; `db.test` is loopback. Call from a multi-threaded tokio
/// runtime.
pub async fn serve(args: &Value, endpoint: Option<DriverEndpoint>, files: &[&Path]) -> Served {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (runner, program) = programs();
    let mut native = bulkhead_host::NativeConfig::default();
    native.runner = Some(runner.clone());
    let rt = Runtime::new(RuntimeConfig {
        max_instances: 16,
        native,
        drivers: BTreeMap::from([(
            SQL.to_string(),
            DriverProfile {
                connect: endpoint,
                files: files.iter().map(|f| f.to_path_buf()).collect(),
                ..DriverProfile::default()
            },
        )]),
        resolver: Resolver::Table(BTreeMap::from([(
            "db.test".to_string(),
            vec!["127.0.0.1".parse().unwrap()],
        )])),
        ..RuntimeConfig::default()
    })
    .unwrap();
    let apps = rt.start_apps(Vec::new()).await.unwrap();
    let code = rt.load_native(NativeCode {
        path: program.clone(),
        kind: NativeProgram::Executable,
    });
    apps.add(AppDeployment::new(manifest(args), code))
        .await
        .unwrap();
    Served { rt, apps }
}

fn request(request: &SqlRequest) -> Envelope {
    let mut body = Vec::new();
    ciborium::into_writer(request, &mut body).unwrap();
    Envelope {
        content_type: "application/cbor".into(),
        schema: Some(SCHEMA.into()),
        body,
        trace: None,
    }
}

impl Served {
    /// The driver's answer to `req`.
    pub async fn ask(&self, req: SqlRequest) -> Result<SqlAnswer, SqlError> {
        let app = self.apps.app(SQL).unwrap();
        let answer = app
            .call(request(&req), Duration::from_secs(20))
            .await
            .unwrap();
        assert_eq!(answer.schema.as_deref(), Some(SCHEMA));
        ciborium::from_reader(answer.body.as_slice()).unwrap()
    }

    /// Cast `req`, as the node casts a rollback.
    pub fn cast(&self, req: SqlRequest) {
        self.apps.app(SQL).unwrap().cast(request(&req)).unwrap();
    }

    /// A query's rows.
    pub async fn query(
        &self,
        statement: &str,
        params: Vec<SqlValue>,
    ) -> Result<Vec<Vec<SqlValue>>, SqlError> {
        match self
            .ask(SqlRequest::Query {
                datasource: "main".into(),
                statement: statement.into(),
                params,
            })
            .await?
        {
            SqlAnswer::Rows(rows) => Ok(rows.rows),
            other => panic!("not rows: {other:?}"),
        }
    }

    /// A statement's changed rows.
    pub async fn execute(&self, statement: &str, params: Vec<SqlValue>) -> Result<u64, SqlError> {
        match self
            .ask(SqlRequest::Execute {
                datasource: "main".into(),
                statement: statement.into(),
                params,
            })
            .await?
        {
            SqlAnswer::Changed(n) => Ok(n),
            other => panic!("not changed: {other:?}"),
        }
    }

    /// The driver's counter `bulkhead_sql_<name>` after its next flush
    /// (worker 0 adds the counts once a second); 0 if never reported.
    pub async fn counter(&self, name: &str) -> u64 {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let wanted = format!("bulkhead_sql_{name}{{");
        self.rt
            .render_metrics()
            .lines()
            .filter(|l| l.starts_with(&wanted))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
            .map(|v| v as u64)
            .sum()
    }
}

//! `bulkhead:sql`: SQL datasources the node binds, Postgres.
//!
//! An instance uses only datasources its manifest lists and the operator's
//! policy grants ([`SqlSpec::datasources`], narrowed by
//! [`InstanceSpec::with_granted_datasources`](crate::InstanceSpec::with_granted_datasources)),
//! with the capability granted; the node binds each name
//! ([`SqlDatasource`](crate::SqlDatasource)).
//!
//! The database's client is not here (ROADMAP DA3): each datasource has a
//! driver of its own (`bulkhead-sql`, the app
//! [`SqlDatasource::driver_app`](crate::SqlDatasource::driver_app)), which
//! holds the pool and runs statements over connections the node opens for
//! it. This service checks the request, holds the app's connection cap,
//! and forwards the request as a call (schema [`SQL_SCHEMA`], the
//! protocol's `SqlRequest` in CBOR), answering the driver's
//! `Result<SqlAnswer, SqlError>`.
//!
//! Each app has a connection cap per datasource (its manifest's
//! `max_connections`), shared by its instances: a call or a transaction
//! holds one of the app's permits while it runs. A call waits up to the
//! datasource's acquire timeout for one, then fails with `limit-reached`.
//!
//! An instance's transactions are kept by number in its state, each with
//! the driver instance that began it and that instance's number for it:
//! the instance's next statements go to that driver instance alone, so a
//! driver that restarted answers `ended`, never another transaction. One
//! still open when the instance exits is rolled back by a cast to the
//! driver.
//!
//! Without the `sql` feature the interface is still linked and every call
//! answers `unavailable`.

use std::collections::BTreeMap;
use std::sync::Arc;

use bulkhead_bundle::manifest::{AppSpec, Capability};
use bulkhead_native::protocol as p;

use super::{InstanceInfo, Service, ServiceCtx};
use crate::config::RuntimeConfig;
use crate::error::HostError;

/// The schema of a request to a datasource's driver, and of its answer
/// (`bulkhead_sql::SQL_SCHEMA`).
pub const SQL_SCHEMA: &str = "bulkhead/sql";

/// The SQL service.
pub(crate) struct Sql;

/// What the manifest asks of SQL for one app.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SqlSpec {
    /// Datasources the instance may use, with the app's connection cap
    /// for each: the manifest's list, narrowed to the operator's grant.
    pub datasources: BTreeMap<String, u32>,
}

/// The node's datasources, and the runtime to reach their drivers.
pub(crate) struct Shared {
    #[cfg(feature = "sql")]
    datasources: BTreeMap<String, Arc<run::Datasource>>,
    #[cfg(feature = "sql")]
    rt: std::sync::OnceLock<std::sync::Weak<crate::runtime::RuntimeInner>>,
}

/// SQL state for one instance.
// Without the `sql` feature every call answers before reading it.
#[cfg_attr(not(feature = "sql"), allow(dead_code))]
pub(crate) struct Instance {
    shared: Arc<Shared>,
    app: String,
    datasources: BTreeMap<String, u32>,
    granted: bool,
    /// The open transactions, by the number the instance was given.
    #[cfg(feature = "sql")]
    txs: BTreeMap<u32, run::Transaction>,
    next_tx: u32,
}

impl Service for Sql {
    const CAPABILITY: Capability = Capability::Sql;
    type Spec = SqlSpec;
    type Shared = Shared;
    type Instance = Instance;

    fn spec(manifest: &AppSpec) -> SqlSpec {
        SqlSpec {
            datasources: manifest
                .capabilities
                .sql
                .iter()
                .flat_map(|s| s.datasources.iter())
                .map(|(name, ds)| (name.clone(), ds.max_connections))
                .collect(),
        }
    }

    fn shared(config: &RuntimeConfig) -> Result<Shared, HostError> {
        Ok(Shared::new(config))
    }

    fn instance(shared: &Arc<Shared>, spec: &SqlSpec, info: &InstanceInfo<'_>) -> Instance {
        Instance {
            shared: shared.clone(),
            app: info.handle.app().to_string(),
            datasources: spec.datasources.clone(),
            granted: info.granted,
            #[cfg(feature = "sql")]
            txs: BTreeMap::new(),
            next_tx: 0,
        }
    }
}

impl Shared {
    /// Each datasource, served by its driver.
    pub(crate) fn new(config: &RuntimeConfig) -> Self {
        #[cfg(not(feature = "sql"))]
        let _ = config;
        Self {
            #[cfg(feature = "sql")]
            datasources: config
                .datasources
                .iter()
                .map(|(name, cfg)| (name.clone(), Arc::new(run::Datasource::new(name, cfg))))
                .collect(),
            #[cfg(feature = "sql")]
            rt: std::sync::OnceLock::new(),
        }
    }

    /// Calls to the drivers go through `rt`.
    pub(crate) fn attach(&self, rt: &Arc<crate::runtime::RuntimeInner>) {
        #[cfg(feature = "sql")]
        let _ = self.rt.set(Arc::downgrade(rt));
        #[cfg(not(feature = "sql"))]
        let _ = rt;
    }
}

/// A SQL operation's outcome, on the protocol's error.
type Answer<T> = std::result::Result<T, p::SqlError>;

#[cfg(not(feature = "sql"))]
fn not_built<T>() -> Answer<T> {
    Err(p::SqlError::Unavailable(
        "this node was built without SQL (cargo feature `sql`)".into(),
    ))
}

#[cfg(feature = "sql")]
mod run {
    //! The calls, with the `sql` feature.

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use tokio::sync::{OwnedSemaphorePermit, Semaphore};

    use super::{Answer, Instance, SQL_SCHEMA, ServiceCtx, Shared, p};
    use crate::config::SqlDatasource;
    use crate::instance::InstanceHandle;
    use crate::message::{CallError, Envelope};

    /// How long a request waits between tries while the driver is not
    /// running (restarting).
    const RETRY_EVERY: Duration = Duration::from_millis(100);

    /// One datasource: its driver, its timeouts, and each app's cap.
    pub(super) struct Datasource {
        name: String,
        driver: String,
        acquire_timeout: Duration,
        call_timeout: Duration,
        caps: Mutex<HashMap<String, (u32, Arc<Semaphore>)>>,
    }

    impl Datasource {
        pub(super) fn new(name: &str, cfg: &SqlDatasource) -> Self {
            Self {
                name: name.to_string(),
                driver: SqlDatasource::driver_app(name),
                acquire_timeout: cfg.acquire_timeout,
                call_timeout: cfg.call_timeout(),
                caps: Mutex::default(),
            }
        }

        /// `app`'s cap of `max` connections, shared by its instances. A
        /// new cap (an upgrade changed it) replaces the old for new
        /// permits.
        fn cap(&self, app: &str, max: u32) -> Arc<Semaphore> {
            let mut caps = self.caps.lock().unwrap_or_else(PoisonError::into_inner);
            let new = || {
                (
                    max,
                    Arc::new(Semaphore::new(usize::try_from(max).unwrap_or(1))),
                )
            };
            let entry = caps.entry(app.to_string()).or_insert_with(new);
            if entry.0 != max {
                *entry = new();
            }
            entry.1.clone()
        }

        /// One of `app`'s permits, within the acquire timeout.
        async fn permit(&self, app: &str, max: u32) -> Answer<OwnedSemaphorePermit> {
            let cap = self.cap(app, max);
            match tokio::time::timeout(self.acquire_timeout, cap.acquire_owned()).await {
                Ok(Ok(permit)) => Ok(permit),
                _ => Err(p::SqlError::LimitReached),
            }
        }
    }

    /// An instance's open transaction: the driver instance that began it,
    /// that instance's number for it, and the app's permit.
    pub(super) struct Transaction {
        driver: InstanceHandle,
        tx: u32,
        ds: Arc<Datasource>,
        _permit: OwnedSemaphorePermit,
        open: bool,
    }

    impl Drop for Transaction {
        /// Still open (the instance exited, or the app dropped it): the
        /// driver rolls it back.
        fn drop(&mut self) {
            if self.open
                && let Ok(msg) = envelope(&p::SqlRequest::TxEnd {
                    tx: self.tx,
                    commit: false,
                })
            {
                let _ = self.driver.cast(msg);
            }
        }
    }

    fn envelope(request: &p::SqlRequest) -> Answer<Envelope> {
        let mut body = Vec::new();
        ciborium::into_writer(request, &mut body)
            .map_err(|e| p::SqlError::Unavailable(format!("request: {e}")))?;
        Ok(Envelope {
            content_type: "application/cbor".into(),
            schema: Some(SQL_SCHEMA.into()),
            body,
        })
    }

    fn ended() -> p::SqlError {
        p::SqlError::Unavailable("the transaction has ended".into())
    }

    /// The driver's answer in `reply`.
    fn answer(ds: &Datasource, reply: &Envelope) -> Answer<p::SqlAnswer> {
        if reply.schema.as_deref() != Some(SQL_SCHEMA) {
            return Err(p::SqlError::Unavailable(format!(
                "datasource {}: not an answer from its driver",
                ds.name
            )));
        }
        ciborium::from_reader::<Answer<p::SqlAnswer>, _>(reply.body.as_slice()).map_err(|e| {
            p::SqlError::Unavailable(format!("datasource {}: its driver's answer: {e}", ds.name))
        })?
    }

    impl Shared {
        /// `msg` to the datasource's driver, and the instance that
        /// answered. While the driver is not running (restarting) it is
        /// tried again until the acquire timeout: it ran nowhere.
        async fn ask(
            &self,
            ds: &Datasource,
            msg: Envelope,
        ) -> Answer<(p::SqlAnswer, InstanceHandle)> {
            let deadline = Instant::now() + ds.acquire_timeout;
            loop {
                let Some(rt) = self.rt.get().and_then(std::sync::Weak::upgrade) else {
                    return Err(p::SqlError::Unavailable("the runtime is stopping".into()));
                };
                let target = rt.route_app(&ds.driver, &msg);
                drop(rt);
                if let Some(target) = target {
                    let reply = target.call(msg, ds.call_timeout).await;
                    return Ok((self.reply(ds, reply)?, target));
                }
                if Instant::now() + RETRY_EVERY >= deadline {
                    return Err(p::SqlError::Unavailable(format!(
                        "datasource {}: its driver ({}) is not running",
                        ds.name, ds.driver
                    )));
                }
                tokio::time::sleep(RETRY_EVERY).await;
            }
        }

        /// The answer to a call that reached the driver.
        fn reply(
            &self,
            ds: &Datasource,
            reply: Result<Envelope, CallError>,
        ) -> Answer<p::SqlAnswer> {
            match reply {
                Ok(reply) => answer(ds, &reply),
                Err(CallError::Timeout) => Err(p::SqlError::Unavailable(format!(
                    "datasource {}: no answer from its driver in {:?}",
                    ds.name, ds.call_timeout
                ))),
                Err(e) => Err(p::SqlError::Unavailable(format!(
                    "datasource {}: its driver: {e}",
                    ds.name
                ))),
            }
        }
    }

    impl ServiceCtx<'_, Instance> {
        /// The datasource `name` and the app's cap on it, if the instance
        /// may use it.
        fn datasource(&self, name: &str) -> Answer<(Arc<Datasource>, u32)> {
            let state = &*self.state;
            let Some(cap) = state
                .datasources
                .get(name)
                .copied()
                .filter(|_| state.granted)
            else {
                return Err(p::SqlError::AccessDenied);
            };
            let ds = state
                .shared
                .datasources
                .get(name)
                .cloned()
                .ok_or(p::SqlError::NotFound)?;
            Ok((ds, cap))
        }

        /// [`ServiceCtx::native`]'s work.
        pub(super) async fn native_run(&mut self, request: p::SqlRequest) -> Answer<p::SqlAnswer> {
            let shared = self.state.shared.clone();
            match request {
                p::SqlRequest::Query { ref datasource, .. }
                | p::SqlRequest::Execute { ref datasource, .. } => {
                    let (ds, cap) = self.datasource(datasource)?;
                    let _permit = ds.permit(&self.state.app, cap).await?;
                    Ok(shared.ask(&ds, envelope(&request)?).await?.0)
                }
                p::SqlRequest::Begin(ref datasource) => {
                    let (ds, cap) = self.datasource(datasource)?;
                    let permit = ds.permit(&self.state.app, cap).await?;
                    let (answer, driver) = shared.ask(&ds, envelope(&request)?).await?;
                    let p::SqlAnswer::Begun(tx) = answer else {
                        return Err(p::SqlError::Unavailable(format!(
                            "datasource {}: its driver answered no transaction",
                            ds.name
                        )));
                    };
                    let transaction = Transaction {
                        driver,
                        tx,
                        ds,
                        _permit: permit,
                        open: true,
                    };
                    let state = &mut *self.state;
                    let mut id = state.next_tx;
                    while state.txs.contains_key(&id) {
                        id = id.wrapping_add(1);
                    }
                    state.next_tx = id.wrapping_add(1);
                    state.txs.insert(id, transaction);
                    Ok(p::SqlAnswer::Begun(id))
                }
                p::SqlRequest::TxQuery {
                    tx,
                    statement,
                    params,
                } => {
                    self.in_tx(tx, |tx| p::SqlRequest::TxQuery {
                        tx,
                        statement,
                        params,
                    })
                    .await
                }
                p::SqlRequest::TxExecute {
                    tx,
                    statement,
                    params,
                } => {
                    self.in_tx(tx, |tx| p::SqlRequest::TxExecute {
                        tx,
                        statement,
                        params,
                    })
                    .await
                }
                p::SqlRequest::TxEnd { tx, commit } => {
                    let mut t = self.state.txs.remove(&tx).ok_or_else(ended)?;
                    let msg = envelope(&p::SqlRequest::TxEnd { tx: t.tx, commit })?;
                    let reply = t.driver.call(msg, t.ds.call_timeout).await;
                    // Ended either way: answered, or its driver gone, or a
                    // rollback cast when `t` drops.
                    t.open = reply.is_err();
                    shared.reply(&t.ds, reply)
                }
            }
        }

        /// A statement in the instance's transaction `tx`, sent to the
        /// driver instance that began it, with its number. The transaction
        /// stays in the instance's state while it runs: if the instance is
        /// killed meanwhile, dropping its state rolls it back.
        async fn in_tx(
            &mut self,
            tx: u32,
            make: impl FnOnce(u32) -> p::SqlRequest,
        ) -> Answer<p::SqlAnswer> {
            let shared = self.state.shared.clone();
            let t = self.state.txs.get(&tx).ok_or_else(ended)?;
            let msg = envelope(&make(t.tx))?;
            let (driver, ds) = (t.driver.clone(), t.ds.clone());
            match driver.call(msg, ds.call_timeout).await {
                Err(CallError::Noproc | CallError::Exited(_)) => {
                    // Its driver restarted: the transaction is gone.
                    if let Some(mut t) = self.state.txs.remove(&tx) {
                        t.open = false;
                    }
                    Err(ended())
                }
                reply => shared.reply(&ds, reply),
            }
        }
    }
}

impl ServiceCtx<'_, Instance> {
    /// A native app's SQL request (ROADMAP Z8d): the checks and caps here,
    /// the statement in the datasource's driver.
    pub(crate) async fn native(
        &mut self,
        request: p::SqlRequest,
    ) -> Result<p::SqlAnswer, p::SqlError> {
        #[cfg(not(feature = "sql"))]
        {
            let _ = request;
            not_built()
        }
        #[cfg(feature = "sql")]
        {
            self.native_run(request).await
        }
    }
}

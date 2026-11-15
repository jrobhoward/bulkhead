//! Running a statement on a connection, and what its failure is for the
//! app (`bulkhead:sql`'s `error`).
//!
//! Parameters go as text for the server to convert; columns come back in
//! the binary format and are decoded by [`crate::values`], up to the
//! datasource's result size.

use bulkhead_native::protocol::{SqlError, SqlRows, SqlValue};
use bytes::BytesMut;
use futures_util::StreamExt;
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type, to_sql_checked};

use crate::values::{DecodeError, Value, decode, param_text};

/// The app's error for a client error: the server's own error with its
/// SQLSTATE, a cancelled statement as a timeout, or anything else (the
/// connection) as unavailable.
#[must_use]
pub fn sql_error(e: &tokio_postgres::Error) -> SqlError {
    match e.as_db_error() {
        Some(db) if *db.code() == SqlState::QUERY_CANCELED => SqlError::Timeout,
        Some(db) => SqlError::Query {
            code: db.code().code().to_string(),
            message: db.message().to_string(),
        },
        None => SqlError::Unavailable(e.to_string()),
    }
}

/// A parameter, sent as text for the server to convert.
#[derive(Debug)]
struct Param(Option<Vec<u8>>);

impl ToSql for Param {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match &self.0 {
            None => Ok(IsNull::Yes),
            Some(text) => {
                out.extend_from_slice(text);
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> tokio_postgres::types::Format {
        tokio_postgres::types::Format::Text
    }

    to_sql_checked!();
}

/// A column's raw binary value, decoded afterwards.
struct Raw<'a>(Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(Some(raw)))
    }

    fn from_sql_null(_ty: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self(None))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

fn params(values: Vec<SqlValue>) -> Vec<Param> {
    values
        .into_iter()
        .map(|v| Param(param_text(&Value::from(v))))
        .collect()
}

/// Run a statement that returns rows, up to `max_bytes` of them.
///
/// # Errors
///
/// The statement failed, a column's type is not carried, or the rows are
/// larger than `max_bytes`.
pub async fn query(
    client: &Client,
    statement: &str,
    values: Vec<SqlValue>,
    max_bytes: u64,
) -> Result<SqlRows, SqlError> {
    let stmt = client.prepare(statement).await.map_err(|e| sql_error(&e))?;
    let params = params(values);
    let stream = client
        .query_raw(&stmt, params.iter().map(|p| p as &(dyn ToSql + Sync)))
        .await
        .map_err(|e| sql_error(&e))?;
    let columns: Vec<String> = stmt
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let types: Vec<Type> = stmt.columns().iter().map(|c| c.type_().clone()).collect();
    tokio::pin!(stream);
    let mut rows = Vec::new();
    let mut size: u64 = 0;
    while let Some(row) = stream.next().await {
        let row = row.map_err(|e| sql_error(&e))?;
        let mut values = Vec::with_capacity(types.len());
        for (i, ty) in types.iter().enumerate() {
            let Raw(raw) = row.try_get(i).map_err(|e| sql_error(&e))?;
            let value = decode(ty, raw).map_err(|e| match e {
                DecodeError::Unsupported(t) => SqlError::Unsupported(format!(
                    "column {} is of type {t}; cast it in the statement ({}::text)",
                    columns[i], columns[i]
                )),
                DecodeError::Malformed(m) => {
                    SqlError::Unavailable(format!("bad value from server: {m}"))
                }
            })?;
            size = size.saturating_add(value.size());
            if size > max_bytes {
                return Err(SqlError::TooLarge);
            }
            values.push(value.into());
        }
        rows.push(values);
    }
    Ok(SqlRows { columns, rows })
}

/// Run a statement for its effect: rows changed.
///
/// # Errors
///
/// The statement failed.
pub async fn execute(
    client: &Client,
    statement: &str,
    values: Vec<SqlValue>,
) -> Result<u64, SqlError> {
    let stmt = client.prepare(statement).await.map_err(|e| sql_error(&e))?;
    let params = params(values);
    client
        .execute_raw(&stmt, params.iter().map(|p| p as &(dyn ToSql + Sync)))
        .await
        .map_err(|e| sql_error(&e))
}

/// Run `sql` (no parameters, no rows): `BEGIN`, `COMMIT`, `ROLLBACK`.
///
/// # Errors
///
/// The server refused it, or the connection failed.
pub async fn simple(client: &Client, sql: &str) -> Result<(), SqlError> {
    client.batch_execute(sql).await.map_err(|e| sql_error(&e))
}

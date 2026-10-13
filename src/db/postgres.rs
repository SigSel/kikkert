//! PostgreSQL. Every statement runs inside `BEGIN TRANSACTION READ ONLY`, so
//! the server itself rejects writes, and results come back over the simple
//! query protocol, where every value is already text.

use anyhow::Result;
use futures_util::TryStreamExt;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgRow, PgSslMode};
use sqlx::{
    AssertSqlSafe, Column, ConnectOptions, Executor, Row, SqlSafeStr, Statement, TypeInfo, ValueRef,
};

use super::dialect::{TypeClass, classify};
use super::{
    ConnectionSpec, Fetched, Limits, Outcome, ResultColumn, TlsMode, bytes_display, sqlx_error,
};

/// A connection plus the server process behind it, which is what has to be
/// named to cancel a running statement.
pub struct PgSession {
    conn: PgConnection,
    backend_pid: Option<i64>,
}

/// Asks the server to stop a statement nobody is waiting for any more.
///
/// A dropped future cannot tell the server anything, and Postgres keeps
/// working on an abandoned query until it next touches the socket. So when a
/// statement is abandoned mid-flight, a short-lived second connection cancels
/// it by process id.
struct CancelOnDrop {
    spec: ConnectionSpec,
    backend_pid: Option<i64>,
    finished: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let (false, Some(pid)) = (self.finished, self.backend_pid) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let spec = self.spec.clone();
        runtime.spawn(async move {
            let cancel = async {
                if let Ok(mut conn) = connect(&spec).await {
                    let sql = format!("SELECT pg_cancel_backend({pid})");
                    let _ = sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await;
                }
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), cancel).await;
        });
    }
}

pub async fn open(spec: &ConnectionSpec) -> Result<PgSession> {
    let mut conn = connect(spec).await?;

    // Belt and braces: the session default is read-only too, on top of the
    // explicit read-only transaction around each statement.
    sqlx::raw_sql("SET default_transaction_read_only = on")
        .execute(&mut conn)
        .await
        .map_err(sqlx_error)?;

    let backend_pid = sqlx::raw_sql("SELECT pg_backend_pid()")
        .fetch_optional(&mut conn)
        .await
        .ok()
        .flatten()
        .and_then(|row| row.try_get_raw(0).ok()?.as_str().ok()?.parse().ok());
    Ok(PgSession { conn, backend_pid })
}

async fn connect(spec: &ConnectionSpec) -> Result<PgConnection> {
    let mut options = PgConnectOptions::new_without_pgpass()
        .host(&spec.host)
        .port(spec.port.unwrap_or(5432))
        .application_name("rowboat")
        .ssl_mode(match spec.tls.unwrap_or_default() {
            TlsMode::Disable => PgSslMode::Disable,
            TlsMode::Prefer => PgSslMode::Prefer,
            TlsMode::Require => PgSslMode::Require,
            TlsMode::Verify => PgSslMode::VerifyFull,
        });
    if let Some(username) = &spec.username {
        options = options.username(username);
    }
    if let Some(password) = &spec.password {
        options = options.password(password);
    }
    if let Some(database) = &spec.database {
        options = options.database(database);
    }
    if let Some(ca_file) = &spec.ca_file {
        options = options.ssl_root_cert(ca_file);
    }
    options.connect().await.map_err(sqlx_error)
}

pub async fn fetch(
    session: &mut PgSession,
    spec: &ConnectionSpec,
    sql: &str,
    limits: Limits,
) -> Outcome {
    let mut cancel = CancelOnDrop {
        spec: spec.clone(),
        backend_pid: session.backend_pid,
        finished: false,
    };
    let outcome = run(&mut session.conn, sql, limits).await;
    cancel.finished = true;
    outcome
}

async fn run(conn: &mut PgConnection, sql: &str, limits: Limits) -> Outcome {
    let begin = format!(
        "BEGIN TRANSACTION READ ONLY; SET LOCAL statement_timeout = {}",
        limits.timeout.as_millis()
    );
    if let Err(e) = sqlx::raw_sql(AssertSqlSafe(begin))
        .execute(&mut *conn)
        .await
    {
        return Outcome {
            result: Err(sqlx_error(e)),
            reusable: false,
        };
    }

    let result = read_rows(conn, sql, limits.max_rows).await;

    // A truncated read leaves rows unread on the wire, so that connection is
    // closed instead of drained.
    let truncated = matches!(&result, Ok(fetched) if fetched.truncated);
    let reusable = !truncated && sqlx::raw_sql("ROLLBACK").execute(&mut *conn).await.is_ok();
    Outcome { result, reusable }
}

async fn read_rows(conn: &mut PgConnection, sql: &str, max_rows: usize) -> Result<Fetched> {
    let mut columns: Vec<ResultColumn> = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;
    {
        let mut stream = sqlx::raw_sql(AssertSqlSafe(sql.to_owned())).fetch(&mut *conn);
        while let Some(row) = stream.try_next().await.map_err(sqlx_error)? {
            if columns.is_empty() {
                columns = columns_of(row.columns());
            }
            if rows.len() >= max_rows {
                truncated = true;
                break;
            }
            rows.push(cells_of(&row));
        }
    }
    // An empty result carries no rows to read the header from, so ask.
    if columns.is_empty()
        && let Ok(statement) = (&mut *conn)
            .prepare(AssertSqlSafe(sql.to_owned()).into_sql_str())
            .await
    {
        columns = columns_of(statement.columns());
    }
    Ok(Fetched {
        columns,
        rows,
        truncated,
    })
}

fn columns_of(columns: &[sqlx::postgres::PgColumn]) -> Vec<ResultColumn> {
    columns
        .iter()
        .map(|column| {
            let type_name = column.type_info().name().to_ascii_lowercase();
            ResultColumn {
                name: column.name().to_string(),
                numeric: classify(&type_name) == TypeClass::Numeric,
                type_name: if type_name == "?" {
                    String::new()
                } else {
                    type_name
                },
            }
        })
        .collect()
}

fn cells_of(row: &PgRow) -> Vec<Option<String>> {
    (0..row.len())
        .map(|i| {
            let raw = row.try_get_raw(i).ok()?;
            if raw.is_null() {
                return None;
            }
            let is_bool = row.column(i).type_info().name() == "BOOL";
            match raw.as_str() {
                Ok("t") if is_bool => Some("true".to_string()),
                Ok("f") if is_bool => Some("false".to_string()),
                Ok(text) => Some(text.to_string()),
                Err(_) => Some(bytes_display(raw.as_bytes().unwrap_or_default())),
            }
        })
        .collect()
}

//! MySQL and MariaDB. The session is switched to `TRANSACTION READ ONLY`
//! right after connecting, and the connection is refused if that fails.
//! Results come back over the text protocol.

use anyhow::{Context, Result};
use futures_util::TryStreamExt;
use sqlx::mysql::{MySql, MySqlConnectOptions, MySqlConnection, MySqlRow, MySqlSslMode};
use sqlx::{
    AssertSqlSafe, Column, ConnectOptions, Decode, Executor, Row, SqlSafeStr, Statement, TypeInfo,
    ValueRef,
};

use super::dialect::{TypeClass, classify};
use super::{
    ConnectionSpec, Fetched, Limits, Outcome, ResultColumn, TlsMode, bytes_display, sqlx_error,
};

/// A connection plus its id on the server, which is what has to be named to
/// stop a running statement.
pub struct MySqlSession {
    conn: MySqlConnection,
    connection_id: Option<u64>,
}

/// Stops a statement nobody is waiting for any more, from a short-lived
/// second connection. Without this the server would keep working on it until
/// its own time limit.
struct KillOnDrop {
    spec: ConnectionSpec,
    connection_id: Option<u64>,
    finished: bool,
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let (false, Some(id)) = (self.finished, self.connection_id) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let spec = self.spec.clone();
        runtime.spawn(async move {
            let kill = async {
                if let Ok(mut conn) = connect(&spec).await {
                    let sql = format!("KILL QUERY {id}");
                    let _ = sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await;
                }
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), kill).await;
        });
    }
}

pub async fn open(spec: &ConnectionSpec) -> Result<MySqlSession> {
    let mut conn = connect(spec).await?;

    sqlx::raw_sql("SET SESSION TRANSACTION READ ONLY")
        .execute(&mut conn)
        .await
        .map_err(sqlx_error)
        .context("Could not make the session read-only, so the connection was dropped")?;

    let connection_id = sqlx::raw_sql("SELECT CONNECTION_ID()")
        .fetch_optional(&mut conn)
        .await
        .ok()
        .flatten()
        .and_then(|row| {
            let raw = row.try_get_raw(0).ok()?;
            let bytes = <&[u8] as Decode<MySql>>::decode(raw).ok()?;
            std::str::from_utf8(bytes).ok()?.parse().ok()
        });
    Ok(MySqlSession {
        conn,
        connection_id,
    })
}

async fn connect(spec: &ConnectionSpec) -> Result<MySqlConnection> {
    let mut options = MySqlConnectOptions::new()
        .host(&spec.host)
        .port(spec.port.unwrap_or(3306))
        .ssl_mode(match spec.tls.unwrap_or_default() {
            TlsMode::Disable => MySqlSslMode::Disabled,
            TlsMode::Prefer => MySqlSslMode::Preferred,
            TlsMode::Require => MySqlSslMode::Required,
            TlsMode::Verify => MySqlSslMode::VerifyIdentity,
        })
        // A viewer should see the data the way the server is configured to
        // show it, so leave the session's time zone and SQL mode alone.
        .timezone(None)
        .pipes_as_concat(false)
        .no_engine_substitution(false);
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
        options = options.ssl_ca(ca_file);
    }
    options.connect().await.map_err(sqlx_error)
}

pub async fn fetch(
    session: &mut MySqlSession,
    spec: &ConnectionSpec,
    sql: &str,
    limits: Limits,
) -> Outcome {
    let mut kill = KillOnDrop {
        spec: spec.clone(),
        connection_id: session.connection_id,
        finished: false,
    };
    let outcome = run(&mut session.conn, sql, limits).await;
    kill.finished = true;
    outcome
}

async fn run(conn: &mut MySqlConnection, sql: &str, limits: Limits) -> Outcome {
    // MySQL counts in milliseconds, MariaDB in seconds under another name.
    // Whichever the server does not know simply fails, which is fine.
    let millis = limits.timeout.as_millis();
    let seconds = limits.timeout.as_secs().max(1);
    if sqlx::raw_sql(AssertSqlSafe(format!(
        "SET SESSION max_execution_time = {millis}"
    )))
    .execute(&mut *conn)
    .await
    .is_err()
    {
        let _ = sqlx::raw_sql(AssertSqlSafe(format!(
            "SET SESSION max_statement_time = {seconds}"
        )))
        .execute(&mut *conn)
        .await;
    }

    let result = read_rows(conn, sql, limits.max_rows).await;
    let reusable = !matches!(&result, Ok(fetched) if fetched.truncated);
    Outcome { result, reusable }
}

async fn read_rows(conn: &mut MySqlConnection, sql: &str, max_rows: usize) -> Result<Fetched> {
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

fn columns_of(columns: &[sqlx::mysql::MySqlColumn]) -> Vec<ResultColumn> {
    columns
        .iter()
        .map(|column| {
            let type_name = column.type_info().name().to_ascii_lowercase();
            ResultColumn {
                name: column.name().to_string(),
                numeric: classify(&type_name) == TypeClass::Numeric,
                type_name,
            }
        })
        .collect()
}

fn cells_of(row: &MySqlRow) -> Vec<Option<String>> {
    (0..row.len())
        .map(|i| {
            let raw = row.try_get_raw(i).ok()?;
            if raw.is_null() {
                return None;
            }
            // The text protocol sends every value as its text form; binary
            // columns are the exception and are shown as hex.
            let bytes = <&[u8] as Decode<MySql>>::decode(raw).ok()?;
            Some(bytes_display(bytes))
        })
        .collect()
}

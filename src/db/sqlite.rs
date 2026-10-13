//! SQLite. The file is opened with `SQLITE_OPEN_READONLY`, so the library
//! itself cannot write to it, and `query_only` is switched on as well.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteRow};
use sqlx::{
    AssertSqlSafe, Column, ConnectOptions, Executor, Row, SqlSafeStr, Statement, TypeInfo, ValueRef,
};

use super::dialect::{TypeClass, classify};
use super::{
    ConnectionSpec, Fetched, Limits, Outcome, ResultColumn, bytes_display, gave_up, sqlx_error,
};

pub async fn open(spec: &ConnectionSpec) -> Result<SqliteConnection> {
    let Some(path) = &spec.path else {
        bail!("A SQLite connection needs a path to the database file");
    };
    if !path.is_file() {
        bail!("No SQLite database at {}", path.display());
    }
    SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .create_if_missing(false)
        .pragma("query_only", "ON")
        .connect()
        .await
        .map_err(sqlx_error)
        .with_context(|| format!("Could not open {}", path.display()))
}

/// Flips its flag when dropped, which is how an abandoned query finds out.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub async fn fetch(conn: &mut SqliteConnection, sql: &str, limits: Limits) -> Outcome {
    // SQLite runs the query on a worker thread that nothing can reach from
    // outside, so the query checks in periodically and stops itself once it
    // is out of time or nobody is waiting for it any more.
    let stop = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + limits.timeout;
    match conn.lock_handle().await {
        Ok(mut handle) => {
            let stop = stop.clone();
            handle.set_progress_handler(1_000, move || {
                !stop.load(Ordering::Relaxed) && Instant::now() < deadline
            });
        }
        Err(e) => {
            return Outcome {
                result: Err(sqlx_error(e)),
                reusable: false,
            };
        }
    }
    let _stop_when_abandoned = StopOnDrop(stop);

    let result = read_rows(conn, sql, limits.max_rows).await.map_err(|e| {
        if Instant::now() >= deadline {
            gave_up(limits)
        } else {
            e
        }
    });
    let reusable = !matches!(&result, Ok(fetched) if fetched.truncated);
    Outcome { result, reusable }
}

async fn read_rows(conn: &mut SqliteConnection, sql: &str, max_rows: usize) -> Result<Fetched> {
    let mut columns: Vec<ResultColumn> = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;
    let mut holds_numbers: Vec<Option<bool>> = Vec::new();
    {
        let mut stream = sqlx::raw_sql(AssertSqlSafe(sql.to_owned())).fetch(&mut *conn);
        while let Some(row) = stream.try_next().await.map_err(sqlx_error)? {
            if columns.is_empty() {
                columns = columns_of(row.columns());
                holds_numbers = vec![None; columns.len()];
            }
            if rows.len() >= max_rows {
                truncated = true;
                break;
            }
            rows.push(cells_of(&row, &mut holds_numbers));
        }
    }
    // Declared types are only a suggestion in SQLite, so a column counts as
    // numeric when every value in it actually is a number.
    for (column, holds) in columns.iter_mut().zip(&holds_numbers) {
        if let Some(numbers) = holds {
            column.numeric = *numbers;
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

fn columns_of(columns: &[sqlx::sqlite::SqliteColumn]) -> Vec<ResultColumn> {
    columns
        .iter()
        .map(|column| {
            let type_name = column.type_info().name().to_ascii_lowercase();
            ResultColumn {
                name: column.name().to_string(),
                numeric: classify(&type_name) == TypeClass::Numeric,
                // Expressions have no declared type.
                type_name: if type_name == "null" {
                    String::new()
                } else {
                    type_name
                },
            }
        })
        .collect()
}

fn cells_of(row: &SqliteRow, holds_numbers: &mut [Option<bool>]) -> Vec<Option<String>> {
    (0..row.len())
        .map(|i| {
            let raw = row.try_get_raw(i).ok()?;
            if raw.is_null() {
                return None;
            }
            // SQLite types belong to values, not columns, so ask each value.
            let storage = raw.type_info().name().to_string();
            drop(raw);
            if let Some(holds) = holds_numbers.get_mut(i) {
                let number = matches!(storage.as_str(), "INTEGER" | "REAL");
                *holds = Some(holds.unwrap_or(true) && number);
            }
            match storage.as_str() {
                "INTEGER" => row
                    .try_get_unchecked::<i64, _>(i)
                    .ok()
                    .map(|v| v.to_string()),
                "REAL" => row
                    .try_get_unchecked::<f64, _>(i)
                    .ok()
                    .map(|v| v.to_string()),
                "BLOB" => row
                    .try_get_unchecked::<Vec<u8>, _>(i)
                    .ok()
                    .map(|v| bytes_display(&v)),
                _ => row.try_get_unchecked::<String, _>(i).ok(),
            }
        })
        .collect()
}

//! Database access: one small, read-only surface over four engines.
//!
//! Nothing in here can change data. User-visible SQL goes through
//! [`guard::check`], and each driver additionally puts its session or
//! transaction into read-only mode where the engine has one.

pub mod dialect;
pub mod guard;
pub mod meta;
mod mssql;
mod mysql;
mod postgres;
mod sqlite;

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[serde(alias = "postgresql", alias = "pg")]
    Postgres,
    #[serde(alias = "mariadb")]
    Mysql,
    #[serde(alias = "sqlserver", alias = "sql server")]
    Mssql,
    #[serde(alias = "sqlite3")]
    Sqlite,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Mysql => "mysql",
            Engine::Mssql => "mssql",
            Engine::Sqlite => "sqlite",
        }
    }

    /// Recognises the names people and 1Password use for an engine.
    pub fn parse(text: &str) -> Option<Engine> {
        let t = text
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '_', '-'], "");
        Some(match t.as_str() {
            "postgres" | "postgresql" | "pg" | "pgsql" => Engine::Postgres,
            "mysql" | "mariadb" => Engine::Mysql,
            "mssql" | "sqlserver" | "microsoftsqlserver" | "mssqlserver" | "tds" | "azuresql" => {
                Engine::Mssql
            }
            "sqlite" | "sqlite3" => Engine::Sqlite,
            _ => return None,
        })
    }

    pub fn from_port(port: u16) -> Option<Engine> {
        match port {
            5432 => Some(Engine::Postgres),
            3306 => Some(Engine::Mysql),
            1433 => Some(Engine::Mssql),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Never encrypt.
    Disable,
    /// Encrypt when the server offers it.
    #[default]
    Prefer,
    /// Always encrypt, without checking the server's certificate.
    Require,
    /// Always encrypt and check the certificate.
    Verify,
}

/// Everything needed to open a connection, secrets included. It only ever
/// lives in memory.
#[derive(Clone)]
pub struct ConnectionSpec {
    pub name: String,
    pub engine: Engine,
    pub host: String,
    pub port: Option<u16>,
    /// SQL Server named instance (`host\instance`).
    pub instance: Option<String>,
    pub database: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// SQLite database file.
    pub path: Option<PathBuf>,
    pub tls: Option<TlsMode>,
    pub ca_file: Option<PathBuf>,
    /// SQL Server: ask an availability group for a read-only replica.
    pub read_intent: bool,
}

impl fmt::Debug for ConnectionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionSpec")
            .field("name", &self.name)
            .field("engine", &self.engine)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("instance", &self.instance)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("path", &self.path)
            .field("tls", &self.tls)
            .finish()
    }
}

impl ConnectionSpec {
    /// Where this points, without credentials.
    pub fn target(&self) -> String {
        if let Some(path) = &self.path {
            return path.display().to_string();
        }
        let mut target = self.host.clone();
        if let Some(instance) = &self.instance {
            target.push('\\');
            target.push_str(instance);
        }
        if let Some(port) = self.port {
            target.push_str(&format!(":{port}"));
        }
        if let Some(database) = &self.database {
            target.push('/');
            target.push_str(database);
        }
        target
    }
}

#[derive(Clone, Debug)]
pub struct ResultColumn {
    pub name: String,
    pub type_name: String,
    pub numeric: bool,
}

/// A query result with every value already rendered as text. `None` is NULL.
#[derive(Clone, Debug, Default)]
pub struct ResultSet {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Option<String>>>,
    /// The row cap was reached and the rest was not read.
    pub truncated: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub timeout: Duration,
    pub max_rows: usize,
}

/// What a driver hands back for one statement.
pub(crate) struct Fetched {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
}

/// The result of a statement plus whether its connection is still in a known
/// state and can be used again.
pub(crate) struct Outcome {
    pub result: Result<Fetched>,
    pub reusable: bool,
}

enum Conn {
    Postgres(Box<postgres::PgSession>),
    Mysql(Box<mysql::MySqlSession>),
    Sqlite(Box<sqlx::sqlite::SqliteConnection>),
    Mssql(Box<mssql::Client>),
}

const MAX_IDLE: usize = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// An open, read-only connection to one database.
///
/// Statements may run concurrently; each takes a connection from a small idle
/// pool or opens another. Dropping the future of a running statement cancels
/// it: the connection is closed rather than reused, and the drivers tell the
/// server to stop working on it.
pub struct Session {
    spec: ConnectionSpec,
    lock_timeout: Duration,
    idle: Mutex<Vec<Conn>>,
}

impl Session {
    pub async fn connect(spec: ConnectionSpec, lock_timeout: Duration) -> Result<Arc<Session>> {
        let session = Session {
            spec,
            lock_timeout,
            idle: Mutex::new(Vec::new()),
        };
        let conn = session.open().await?;
        session.give_back(conn);
        Ok(Arc::new(session))
    }

    pub fn engine(&self) -> Engine {
        self.spec.engine
    }

    pub fn spec(&self) -> &ConnectionSpec {
        &self.spec
    }

    /// Runs a statement written by the user or the query builder. It is
    /// refused unless the guard can show it only reads.
    pub async fn query(&self, sql: &str, limits: Limits) -> Result<ResultSet> {
        guard::check(self.engine(), sql)?;
        self.run(sql, limits).await
    }

    /// Runs one of rowboat's own catalog queries from [`meta`].
    pub async fn catalog_query(&self, sql: &str, limits: Limits) -> Result<ResultSet> {
        self.run(sql, limits).await
    }

    async fn open(&self) -> Result<Conn> {
        // Driver futures are large; boxing them keeps the stack shallow.
        let opening = Box::pin(async {
            Ok::<Conn, anyhow::Error>(match self.spec.engine {
                Engine::Postgres => Conn::Postgres(Box::new(postgres::open(&self.spec).await?)),
                Engine::Mysql => Conn::Mysql(Box::new(mysql::open(&self.spec).await?)),
                Engine::Sqlite => Conn::Sqlite(Box::new(sqlite::open(&self.spec).await?)),
                Engine::Mssql => {
                    Conn::Mssql(Box::new(mssql::open(&self.spec, self.lock_timeout).await?))
                }
            })
        });
        match tokio::time::timeout(CONNECT_TIMEOUT, opening).await {
            Ok(conn) => conn,
            Err(_) => Err(anyhow!(
                "Timed out after {}s connecting to {}",
                CONNECT_TIMEOUT.as_secs(),
                self.spec.target()
            )),
        }
    }

    fn take_idle(&self) -> Option<Conn> {
        self.idle.lock().ok()?.pop()
    }

    fn give_back(&self, conn: Conn) {
        if let Ok(mut idle) = self.idle.lock()
            && idle.len() < MAX_IDLE
        {
            idle.push(conn);
        }
    }

    async fn run(&self, sql: &str, limits: Limits) -> Result<ResultSet> {
        let started = Instant::now();
        let mut conn = match self.take_idle() {
            Some(conn) => conn,
            None => self.open().await?,
        };

        // The server-side timeout (where there is one) fires first and leaves
        // the connection usable; this one is the backstop.
        let backstop = limits.timeout + Duration::from_secs(2);
        let fetching: std::pin::Pin<Box<dyn Future<Output = Outcome> + Send + '_>> = match &mut conn
        {
            Conn::Postgres(c) => Box::pin(postgres::fetch(c, &self.spec, sql, limits)),
            Conn::Mysql(c) => Box::pin(mysql::fetch(c, &self.spec, sql, limits)),
            Conn::Sqlite(c) => Box::pin(sqlite::fetch(c, sql, limits)),
            Conn::Mssql(c) => Box::pin(mssql::fetch(c, sql, limits)),
        };
        let outcome = tokio::time::timeout(backstop, fetching).await;

        match outcome {
            Ok(Outcome { result, reusable }) => {
                if reusable {
                    self.give_back(conn);
                }
                let fetched = result.map_err(|e| {
                    // Each server words its own time limit differently.
                    let text = e.to_string().to_lowercase();
                    let timed_out = [
                        "statement timeout",
                        "execution time exceeded",
                        "max_statement_time",
                    ]
                    .iter()
                    .any(|phrase| text.contains(phrase));
                    if timed_out { gave_up(limits) } else { e }
                })?;
                Ok(ResultSet {
                    columns: fetched.columns,
                    rows: fetched.rows,
                    truncated: fetched.truncated,
                    elapsed: started.elapsed(),
                })
            }
            // `conn` is dropped here, which closes it mid-statement.
            Err(_) => Err(gave_up(limits)),
        }
    }
}

pub(crate) fn gave_up(limits: Limits) -> anyhow::Error {
    anyhow!(
        "Gave up after {}s. Narrow the query, or raise query_timeout_secs in the config.",
        limits.timeout.as_secs()
    )
}

/// Renders bytes for display: text when they are text, otherwise hex.
pub(crate) fn bytes_display(bytes: &[u8]) -> String {
    const SHOWN: usize = 64;
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return text.to_string();
    }
    let mut out = String::with_capacity(2 + SHOWN * 2);
    out.push_str("0x");
    for byte in bytes.iter().take(SHOWN) {
        out.push_str(&format!("{byte:02x}"));
    }
    if bytes.len() > SHOWN {
        out.push_str(&format!("… ({} bytes)", bytes.len()));
    }
    out
}

/// Database errors arrive wrapped in driver boilerplate; this keeps the part
/// a person needs.
pub(crate) fn sqlx_error(error: sqlx::Error) -> anyhow::Error {
    match &error {
        sqlx::Error::Database(db) => anyhow!("{}", db.message()),
        _ => anyhow!("{error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_names() {
        assert_eq!(Engine::parse("PostgreSQL"), Some(Engine::Postgres));
        assert_eq!(Engine::parse("Microsoft SQL Server"), Some(Engine::Mssql));
        assert_eq!(Engine::parse("sql-server"), Some(Engine::Mssql));
        assert_eq!(Engine::parse("MariaDB"), Some(Engine::Mysql));
        assert_eq!(Engine::parse("oracle"), None);
    }

    #[test]
    fn bytes_are_shown_as_text_or_hex() {
        assert_eq!(bytes_display(b"hello"), "hello");
        assert_eq!(bytes_display(&[0x00, 0xff]), "0x00ff");
        assert_eq!(bytes_display(&[0x01]), "0x01");
        assert_eq!(bytes_display(b""), "");
        let long = vec![0u8; 100];
        assert!(bytes_display(&long).ends_with("… (100 bytes)"));
    }

    #[test]
    fn debug_output_hides_the_password() {
        let spec = ConnectionSpec {
            name: "x".into(),
            engine: Engine::Postgres,
            host: "h".into(),
            port: None,
            instance: None,
            database: None,
            username: Some("u".into()),
            password: Some("hunter2".into()),
            path: None,
            tls: None,
            ca_file: None,
            read_intent: false,
        };
        assert!(!format!("{spec:?}").contains("hunter2"));
    }
}

//! The config file: which connections exist and where their details live.
//! Secrets are not meant to be in here; point at 1Password instead.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::db::{Limits, TlsMode};

pub const EXAMPLE: &str = r#"# rowboat config
#
# Each [[connection]] is one database. The usual setup keeps everything in
# 1Password: create a "Database" item with type, server, port, database,
# username and password, and point at it here.

[[connection]]
name = "orders-prod"
item = "op://Engineering/Orders Prod DB"

# Anything set here wins over the 1Password item, so one item can serve
# several databases on the same server.
[[connection]]
name = "orders-reporting"
item = "op://Engineering/Orders Prod DB"
database = "reporting"

# Without an item, give the details directly. Any value can be an
# op://vault/item/field reference, resolved when you connect.
[[connection]]
name = "staging"
engine = "postgres"            # postgres | mysql | mssql | sqlite
host = "staging-db.internal"
port = 5432
database = "app"
username = "op://Engineering/Staging DB/username"
password = "op://Engineering/Staging DB/password"
tls = "require"                # disable | prefer | require | verify

# If an item uses unusual field names, say which is which.
[[connection]]
name = "legacy-erp"
item = "op://Engineering/ERP"
engine = "mssql"
fields = { host = "Server URL", database = "Catalog" }

[[connection]]
name = "local-file"
engine = "sqlite"
path = "~/data/app.db"

[settings]
page_size = 200            # rows fetched per page
query_timeout_secs = 30    # a query is abandoned after this long
count_timeout_secs = 5     # the row count shown in the status bar; 0 turns it off
max_rows = 5000            # cap for hand-written SQL without a LIMIT
max_column_width = 40
# op_account = "my.1password.com"   # only needed with several 1Password accounts
"#;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "connection")]
    pub connections: Vec<ConnectionConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub page_size: usize,
    pub query_timeout_secs: u64,
    pub count_timeout_secs: u64,
    pub max_rows: usize,
    pub max_column_width: u16,
    /// The 1Password CLI to run, if it is not simply `op` on the PATH.
    pub op_path: String,
    pub op_account: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            page_size: 200,
            query_timeout_secs: 30,
            count_timeout_secs: 5,
            max_rows: 5000,
            max_column_width: 40,
            op_path: "op".to_string(),
            op_account: None,
        }
    }
}

impl Settings {
    pub fn query_timeout(&self) -> Duration {
        Duration::from_secs(self.query_timeout_secs.max(1))
    }

    /// Limits for a page of a table: the page size is already in the SQL.
    pub fn page_limits(&self) -> Limits {
        Limits {
            timeout: self.query_timeout(),
            max_rows: self.page_size.max(self.max_rows),
        }
    }

    /// Limits for SQL typed by hand, which may have no LIMIT at all.
    pub fn custom_limits(&self) -> Limits {
        Limits {
            timeout: self.query_timeout(),
            max_rows: self.max_rows.max(1),
        }
    }

    pub fn count_limits(&self) -> Option<Limits> {
        (self.count_timeout_secs > 0).then(|| Limits {
            timeout: Duration::from_secs(self.count_timeout_secs),
            max_rows: 1,
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum PortValue {
    Number(u16),
    Text(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionConfig {
    pub name: String,
    /// A 1Password item holding the connection details: `op://Vault/Item`.
    pub item: Option<String>,
    pub engine: Option<String>,
    pub host: Option<String>,
    pub port: Option<PortValue>,
    pub instance: Option<String>,
    pub database: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// SQLite database file.
    pub path: Option<String>,
    pub tls: Option<TlsMode>,
    pub ca_file: Option<String>,
    #[serde(default)]
    pub read_intent: bool,
    /// Which 1Password field holds what, when the item's labels are unusual:
    /// `fields = { host = "Server URL" }`.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

impl ConnectionConfig {
    /// A one-line summary for the connection list. Never contains secrets:
    /// references are shown as references.
    pub fn summary(&self) -> String {
        if let Some(item) = &self.item {
            return match &self.database {
                Some(database) => format!("{item}  ·  {database}"),
                None => item.clone(),
            };
        }
        if let Some(path) = &self.path {
            return path.clone();
        }
        let mut out = self.host.clone().unwrap_or_default();
        if let Some(PortValue::Number(port)) = &self.port {
            out.push_str(&format!(":{port}"));
        }
        if let Some(database) = &self.database {
            out.push('/');
            out.push_str(database);
        }
        out
    }
}

pub const FIELD_KEYS: [&str; 8] = [
    "engine", "host", "port", "instance", "database", "username", "password", "path",
];

pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ROWBOAT_CONFIG") {
        return PathBuf::from(path);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rowboat")
        .join("config.toml")
}

/// Expands a leading `~/`.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Could not read {}", path.display()))?;
    parse(&text).with_context(|| format!("In {}", path.display()))
}

pub fn parse(text: &str) -> Result<Config> {
    let config: Config = toml::from_str(text)?;
    let mut seen = HashSet::new();
    for connection in &config.connections {
        let name = connection.name.trim();
        if name.is_empty() {
            bail!("A connection has an empty name");
        }
        if !seen.insert(name.to_lowercase()) {
            bail!("Two connections are both called \"{name}\"");
        }
        if connection.item.is_none() && connection.host.is_none() && connection.path.is_none() {
            bail!("Connection \"{name}\" needs an item, a host or a path");
        }
        for key in connection.fields.keys() {
            if !FIELD_KEYS.contains(&key.as_str()) {
                bail!(
                    "Connection \"{name}\": \"{key}\" is not something `fields` can map. Use one of: {}",
                    FIELD_KEYS.join(", ")
                );
            }
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_is_valid() {
        let config = parse(EXAMPLE).expect("example config should parse");
        assert_eq!(config.connections.len(), 5);
        assert_eq!(config.settings.page_size, 200);
        assert_eq!(
            config.connections[0].summary(),
            "op://Engineering/Orders Prod DB"
        );
        assert_eq!(
            config.connections[2].summary(),
            "staging-db.internal:5432/app"
        );
    }

    #[test]
    fn an_empty_file_is_an_empty_config() {
        let config = parse("").unwrap();
        assert!(config.connections.is_empty());
        assert_eq!(config.settings.query_timeout_secs, 30);
    }

    #[test]
    fn mistakes_are_reported() {
        let duplicate = "[[connection]]\nname = \"a\"\nhost = \"h\"\n[[connection]]\nname = \"A\"\nhost = \"h\"\n";
        assert!(
            parse(duplicate)
                .unwrap_err()
                .to_string()
                .contains("both called")
        );

        let nowhere = "[[connection]]\nname = \"a\"\n";
        assert!(
            parse(nowhere)
                .unwrap_err()
                .to_string()
                .contains("needs an item")
        );

        let typo = "[[connection]]\nname = \"a\"\nhost = \"h\"\npasword = \"x\"\n";
        assert!(parse(typo).is_err());

        let mapping =
            "[[connection]]\nname = \"a\"\nitem = \"op://v/i\"\nfields = { server = \"x\" }\n";
        assert!(parse(mapping).unwrap_err().to_string().contains("fields"));
    }

    #[test]
    fn a_summary_never_shows_a_resolved_secret() {
        let text = "[[connection]]\nname = \"a\"\nhost = \"h\"\npassword = \"hunter2\"\n";
        let config = parse(text).unwrap();
        assert!(!config.connections[0].summary().contains("hunter2"));
    }
}

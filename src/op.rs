//! 1Password. Connection details are read through the `op` command line tool
//! at the moment of connecting and are never written anywhere.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::process::Command;

use crate::config::{ConnectionConfig, PortValue, Settings, expand_home};
use crate::db::{ConnectionSpec, Engine};

const REFERENCE_PREFIX: &str = "op://";

/// What a connection may take from a 1Password item, with the field names
/// people actually use. Names are compared without case, spaces or
/// punctuation, and earlier names win.
fn synonyms(key: &str) -> &'static [&'static str] {
    match key {
        "engine" => &["databasetype", "type", "engine", "driver", "dbtype"],
        "host" => &[
            "hostname",
            "server",
            "host",
            "serverurl",
            "url",
            "address",
            "serveraddress",
            "endpoint",
            "datasource",
        ],
        "port" => &["port"],
        "instance" => &["instance", "instancename"],
        "database" => &[
            "database",
            "databasename",
            "dbname",
            "db",
            "catalog",
            "initialcatalog",
        ],
        "username" => &["username", "user", "login", "userid", "uid"],
        "password" => &["password", "pwd", "pass"],
        "path" => &["path", "file", "filename"],
        _ => &[],
    }
}

fn normalize(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(Debug, Deserialize)]
struct RawItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    fields: Vec<RawField>,
    #[serde(default)]
    urls: Vec<RawUrl>,
}

#[derive(Debug, Deserialize)]
struct RawField {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawUrl {
    #[serde(default)]
    href: String,
    #[serde(default)]
    primary: bool,
}

/// A 1Password item, reduced to its non-empty fields.
pub struct Item {
    title: String,
    fields: Vec<Field>,
    url: Option<String>,
}

struct Field {
    id: String,
    label: String,
    value: String,
}

impl std::fmt::Debug for Item {
    // Field values are secrets; only the labels are safe to print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Item")
            .field("title", &self.title)
            .field("fields", &self.labels())
            .finish()
    }
}

impl Item {
    pub fn from_json(json: &str) -> Result<Item> {
        let raw: RawItem =
            serde_json::from_str(json).context("1Password returned something unexpected")?;
        let fields = raw
            .fields
            .into_iter()
            .filter_map(|field| {
                let value = match field.value? {
                    serde_json::Value::String(text) => text,
                    serde_json::Value::Null => return None,
                    other => other.to_string(),
                };
                (!value.is_empty()).then_some(Field {
                    id: normalize(&field.id),
                    label: field.label,
                    value,
                })
            })
            .collect();
        let url = raw
            .urls
            .iter()
            .find(|u| u.primary)
            .or(raw.urls.first())
            .map(|u| u.href.clone())
            .filter(|href| !href.is_empty());
        Ok(Item {
            title: raw.title,
            fields,
            url,
        })
    }

    fn labels(&self) -> Vec<&str> {
        self.fields.iter().map(|f| f.label.as_str()).collect()
    }

    /// The field with exactly this label (ignoring case and punctuation).
    fn by_label(&self, label: &str) -> Option<&str> {
        let wanted = normalize(label);
        self.fields
            .iter()
            .find(|f| normalize(&f.label) == wanted || f.id == wanted)
            .map(|f| f.value.as_str())
    }

    /// The field for one of kikkert's keys, found by its usual names.
    fn by_key(&self, key: &str) -> Option<&str> {
        let found = synonyms(key).iter().find_map(|name| {
            self.fields
                .iter()
                .find(|f| f.id == *name || normalize(&f.label) == *name)
                .map(|f| f.value.as_str())
        });
        match (found, key) {
            // Login items keep the address under "website".
            (None, "host") => self.url.as_deref(),
            (found, _) => found,
        }
    }
}

/// Runs the 1Password CLI.
pub struct Op {
    program: String,
    account: Option<String>,
}

impl Op {
    pub fn new(settings: &Settings) -> Op {
        Op {
            program: settings.op_path.clone(),
            account: settings.op_account.clone(),
        }
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let mut command = Command::new(&self.program);
        command.args(args);
        if let Some(account) = &self.account {
            command.args(["--account", account]);
        }
        // The terminal belongs to the UI, so op must not prompt on it. With
        // the desktop app integration it asks through the app instead.
        let output = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => anyhow!(
                    "The 1Password CLI (`{}`) is not installed or not on the PATH. \
                     Install it from https://developer.1password.com/docs/cli/get-started/ \
                     and turn on \"Integrate with 1Password CLI\" in the 1Password app.",
                    self.program
                ),
                _ => anyhow!("Could not run `{}`: {e}", self.program),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("1Password: {}", clean_error(&stderr));
        }
        String::from_utf8(output.stdout).context("1Password returned text that is not UTF-8")
    }

    /// Reads one `op://vault/item/field` reference.
    pub async fn read(&self, reference: &str) -> Result<String> {
        let value = self.run(&["read", "--no-newline", reference]).await?;
        Ok(value)
    }

    /// Fetches a whole item given as `op://Vault/Item`, or as a bare item
    /// name or id to search every vault.
    pub async fn item(&self, reference: &str) -> Result<Item> {
        let json = match parse_item_reference(reference)? {
            (Some(vault), item) => {
                self.run(&["item", "get", &item, "--vault", &vault, "--format", "json"])
                    .await?
            }
            (None, item) => {
                self.run(&["item", "get", &item, "--format", "json"])
                    .await?
            }
        };
        Item::from_json(&json)
    }
}

/// `op` prefixes errors with a tag and a timestamp; keep the message.
fn clean_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("the command failed without saying why");
    let message = match line.strip_prefix("[ERROR]") {
        // "[ERROR] 2024/01/02 03:04:05 message"
        Some(rest) => rest.trim().splitn(3, ' ').nth(2).unwrap_or(rest).trim(),
        None => line,
    };
    message.to_string()
}

fn parse_item_reference(reference: &str) -> Result<(Option<String>, String)> {
    let Some(rest) = reference.strip_prefix(REFERENCE_PREFIX) else {
        return Ok((None, reference.to_string()));
    };
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [vault, item] if !vault.is_empty() && !item.is_empty() => {
            Ok((Some(vault.to_string()), item.to_string()))
        }
        _ => bail!(
            "\"{reference}\" should name an item as op://Vault/Item. \
             To read a single field, put the reference on that setting instead."
        ),
    }
}

/// The pieces that can be packed into a "server" value.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ServerParts {
    pub engine: Option<Engine>,
    pub host: String,
    pub port: Option<u16>,
    pub instance: Option<String>,
    pub database: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Understands the ways people write down a server: a bare host,
/// `host:port`, SQL Server's `host,port` and `host\instance`, Azure's
/// `tcp:host,port`, and full URLs including JDBC ones.
pub fn parse_server(text: &str) -> ServerParts {
    let mut parts = ServerParts::default();
    let mut rest = text.trim();
    rest = rest.strip_prefix("jdbc:").unwrap_or(rest);

    if let Some((scheme, after)) = rest.split_once("://") {
        parts.engine = Engine::parse(scheme);
        rest = after;
    } else if let Some(after) = rest.strip_prefix("tcp:") {
        rest = after;
    }

    // JDBC SQL Server: host:port;databaseName=x;...
    if let Some((head, properties)) = rest.split_once(';') {
        for property in properties.split(';') {
            if let Some((key, value)) = property.split_once('=') {
                match normalize(key).as_str() {
                    "databasename" | "database" | "initialcatalog" => {
                        parts.database = Some(value.to_string())
                    }
                    "instancename" => parts.instance = Some(value.to_string()),
                    _ => {}
                }
            }
        }
        rest = head;
    }

    rest = rest.split(['?', '#']).next().unwrap_or(rest);
    if let Some((credentials, after)) = rest.rsplit_once('@') {
        let (user, password) = match credentials.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (credentials, None),
        };
        if !user.is_empty() {
            parts.username = Some(user.to_string());
        }
        parts.password = password.map(str::to_string);
        rest = after;
    }
    if let Some((before, database)) = rest.split_once('/') {
        if !database.is_empty() {
            parts.database = Some(database.trim_end_matches('/').to_string());
        }
        rest = before;
    }

    let (mut host, port) = if let Some(bracketed) = rest.strip_prefix('[') {
        // [::1]:5432
        match bracketed.split_once(']') {
            Some((host, tail)) => (host, tail.trim_start_matches([':', ','])),
            None => (bracketed, ""),
        }
    } else if let Some((host, port)) = rest.rsplit_once(',') {
        (host, port)
    } else if rest.matches(':').count() == 1 {
        rest.split_once(':').unwrap_or((rest, ""))
    } else {
        (rest, "")
    };
    parts.port = port.trim().parse().ok();

    if let Some((name, instance)) = host.split_once('\\') {
        if !instance.is_empty() {
            parts.instance = Some(instance.to_string());
        }
        host = name;
    }
    parts.host = host.trim().to_string();
    parts
}

/// Works out everything needed to connect: the config file's own values
/// first, then the 1Password item.
pub async fn resolve(connection: &ConnectionConfig, settings: &Settings) -> Result<ConnectionSpec> {
    let op = Op::new(settings);
    let name = connection.name.as_str();
    let item = match &connection.item {
        Some(reference) => Some(op.item(reference).await?),
        None => None,
    };

    // A value written in the config: taken as is, unless it is a reference.
    let direct = async |value: &Option<String>| -> Result<Option<String>> {
        match value {
            Some(v) if v.starts_with(REFERENCE_PREFIX) => Ok(Some(op.read(v).await?)),
            Some(v) => Ok(Some(v.clone())),
            None => Ok(None),
        }
    };
    let from_item = |key: &str| -> Result<Option<String>> {
        let Some(item) = &item else { return Ok(None) };
        match connection.fields.get(key) {
            Some(label) => match item.by_label(label) {
                Some(value) => Ok(Some(value.to_string())),
                None => bail!(
                    "The 1Password item \"{}\" has no field \"{label}\" (mapped to {key}). It has: {}",
                    item.title,
                    item.labels().join(", ")
                ),
            },
            None => Ok(item.by_key(key).map(str::to_string)),
        }
    };
    let pick = async |value: &Option<String>, key: &str| -> Result<Option<String>> {
        match direct(value).await? {
            Some(v) => Ok(Some(v)),
            None => from_item(key),
        }
    };

    let engine_text = pick(&connection.engine, "engine").await?;
    let server = pick(&connection.host, "host").await?;
    let port_text = match &connection.port {
        Some(PortValue::Number(port)) => Some(port.to_string()),
        Some(PortValue::Text(text)) => direct(&Some(text.clone())).await?,
        None => from_item("port")?,
    };
    let instance = pick(&connection.instance, "instance").await?;
    let database = pick(&connection.database, "database").await?;
    let username = pick(&connection.username, "username").await?;
    let password = pick(&connection.password, "password").await?;
    let path = pick(&connection.path, "path").await?;

    let server = server.as_deref().map(parse_server).unwrap_or_default();
    let port = match port_text
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(text) => Some(
            text.parse::<u16>()
                .map_err(|_| anyhow!("Connection \"{name}\": \"{text}\" is not a port number"))?,
        ),
        None => server.port,
    };

    let engine = match engine_text.as_deref() {
        // An engine written in the config must be one we know.
        Some(text) if connection.engine.is_some() => Some(Engine::parse(text).ok_or_else(|| {
            anyhow!("Connection \"{name}\": unknown engine \"{text}\". Use postgres, mysql, mssql or sqlite.")
        })?),
        // One found in the item is a hint; other clues can fill in.
        Some(text) => Engine::parse(text),
        None => None,
    }
    .or(server.engine)
    .or(port.and_then(Engine::from_port))
    .or(path.as_ref().map(|_| Engine::Sqlite));
    let Some(engine) = engine else {
        bail!(
            "Connection \"{name}\": could not tell which database engine this is. \
             Add engine = \"postgres\" (or mysql, mssql, sqlite) to the connection."
        );
    };

    if engine == Engine::Sqlite {
        // A SQLite "server" is just a file.
        let path = path.or_else(|| (!server.host.is_empty()).then(|| server.host.clone()));
        let Some(path) = path else {
            bail!("Connection \"{name}\": a SQLite connection needs a path");
        };
        return Ok(ConnectionSpec {
            name: name.to_string(),
            engine,
            host: String::new(),
            port: None,
            instance: None,
            database: None,
            username: None,
            password: None,
            path: Some(expand_home(&path)),
            tls: None,
            ca_file: None,
            read_intent: false,
        });
    }

    if server.host.is_empty() {
        bail!(
            "Connection \"{name}\": no server address. Set host, or add a \"server\" field to the 1Password item."
        );
    }
    Ok(ConnectionSpec {
        name: name.to_string(),
        engine,
        host: server.host,
        port,
        instance: instance.or(server.instance),
        database: database.or(server.database).filter(|d| !d.is_empty()),
        username: username.or(server.username),
        password: password.or(server.password),
        path: None,
        tls: connection.tls,
        ca_file: connection.ca_file.as_deref().map(expand_home),
        read_intent: connection.read_intent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `op item get --format json` returns for a Database item.
    const DATABASE_ITEM: &str = r#"{
      "id": "abcdefghijklmnopqrstuvwxyz",
      "title": "Orders Prod DB",
      "version": 3,
      "vault": {"id": "v1", "name": "Engineering"},
      "category": "DATABASE",
      "fields": [
        {"id": "notesPlain", "type": "STRING", "purpose": "NOTES", "label": "notesPlain"},
        {"id": "database_type", "type": "MENU", "label": "type", "value": "postgresql"},
        {"id": "hostname", "type": "STRING", "label": "server", "value": "db.example.com"},
        {"id": "port", "type": "STRING", "label": "port", "value": "5433"},
        {"id": "database", "type": "STRING", "label": "database", "value": "orders"},
        {"id": "username", "type": "STRING", "label": "username", "value": "reader"},
        {"id": "password", "type": "CONCEALED", "label": "password", "value": "s3cret"},
        {"id": "sid", "type": "STRING", "label": "SID"},
        {"id": "options", "type": "STRING", "label": "connection options", "value": ""}
      ]
    }"#;

    #[test]
    fn a_database_item_gives_every_field() {
        let item = Item::from_json(DATABASE_ITEM).unwrap();
        assert_eq!(item.by_key("engine"), Some("postgresql"));
        assert_eq!(item.by_key("host"), Some("db.example.com"));
        assert_eq!(item.by_key("port"), Some("5433"));
        assert_eq!(item.by_key("database"), Some("orders"));
        assert_eq!(item.by_key("username"), Some("reader"));
        assert_eq!(item.by_key("password"), Some("s3cret"));
        assert_eq!(item.by_key("instance"), None);
        assert_eq!(
            item.by_label("Connection Options"),
            None,
            "empty fields are absent"
        );
    }

    #[test]
    fn a_login_item_with_custom_fields_works_too() {
        let json = r#"{
          "title": "ERP",
          "category": "LOGIN",
          "urls": [{"primary": true, "href": "sqlserver://erp.internal,1433"}],
          "fields": [
            {"id": "username", "type": "STRING", "purpose": "USERNAME", "label": "username", "value": "viewer"},
            {"id": "password", "type": "CONCEALED", "purpose": "PASSWORD", "label": "password", "value": "pw"},
            {"id": "x1", "section": {"id": "s"}, "type": "STRING", "label": "Database Name", "value": "erp"},
            {"id": "x2", "section": {"id": "s"}, "type": "STRING", "label": "Server URL", "value": "erp2.internal"}
          ]
        }"#;
        let item = Item::from_json(json).unwrap();
        assert_eq!(item.by_key("database"), Some("erp"));
        assert_eq!(item.by_key("host"), Some("erp2.internal"));
        assert_eq!(item.by_label("server url"), Some("erp2.internal"));
        assert_eq!(item.by_label("missing"), None);
    }

    #[test]
    fn the_website_is_the_host_of_last_resort() {
        let json = r#"{"title": "x", "urls": [{"href": "postgres://pg.internal:5432/app"}], "fields": []}"#;
        let item = Item::from_json(json).unwrap();
        assert_eq!(item.by_key("host"), Some("postgres://pg.internal:5432/app"));
    }

    #[test]
    fn debug_output_shows_labels_not_values() {
        let item = Item::from_json(DATABASE_ITEM).unwrap();
        let shown = format!("{item:?}");
        assert!(shown.contains("password"));
        assert!(!shown.contains("s3cret"));
    }

    fn server(text: &str) -> ServerParts {
        parse_server(text)
    }

    #[test]
    fn servers_in_every_notation() {
        assert_eq!(server("db.example.com").host, "db.example.com");

        let s = server("db.example.com:5432");
        assert_eq!((s.host.as_str(), s.port), ("db.example.com", Some(5432)));

        let s = server("tcp:myserver.database.windows.net,1433");
        assert_eq!(
            (s.host.as_str(), s.port),
            ("myserver.database.windows.net", Some(1433))
        );

        let s = server("sqlhost\\REPORTING");
        assert_eq!(
            (s.host.as_str(), s.instance.as_deref(), s.port),
            ("sqlhost", Some("REPORTING"), None)
        );

        let s = server("postgresql://reader:pw@pg.internal:6432/app?sslmode=require");
        assert_eq!(s.engine, Some(Engine::Postgres));
        assert_eq!((s.host.as_str(), s.port), ("pg.internal", Some(6432)));
        assert_eq!(s.database.as_deref(), Some("app"));
        assert_eq!(
            (s.username.as_deref(), s.password.as_deref()),
            (Some("reader"), Some("pw"))
        );

        let s = server("jdbc:sqlserver://erp.internal:1433;databaseName=erp;encrypt=true");
        assert_eq!(s.engine, Some(Engine::Mssql));
        assert_eq!(
            (s.host.as_str(), s.port, s.database.as_deref()),
            ("erp.internal", Some(1433), Some("erp"))
        );

        let s = server("mysql://db.internal/shop");
        assert_eq!(
            (s.engine, s.host.as_str(), s.database.as_deref()),
            (Some(Engine::Mysql), "db.internal", Some("shop"))
        );

        let s = server("[::1]:5432");
        assert_eq!((s.host.as_str(), s.port), ("::1", Some(5432)));

        let s = server("https://example.com/");
        assert_eq!(
            (s.engine, s.host.as_str(), s.database),
            (None, "example.com", None)
        );
    }

    #[test]
    fn item_references() {
        assert_eq!(
            parse_item_reference("op://Engineering/Orders Prod DB").unwrap(),
            (Some("Engineering".into()), "Orders Prod DB".into())
        );
        assert_eq!(
            parse_item_reference("Orders Prod DB").unwrap(),
            (None, "Orders Prod DB".into())
        );
        assert!(parse_item_reference("op://Engineering/Orders/password").is_err());
        assert!(parse_item_reference("op://Engineering").is_err());
    }

    #[test]
    fn op_errors_lose_their_prefix() {
        assert_eq!(
            clean_error(
                "[ERROR] 2024/01/02 03:04:05 \"x\" isn't an item. Specify the item with its UUID, name, or domain.\n"
            ),
            "\"x\" isn't an item. Specify the item with its UUID, name, or domain."
        );
        assert_eq!(clean_error("something else\n"), "something else");
        assert_eq!(clean_error(""), "the command failed without saying why");
    }
}

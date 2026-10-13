//! SQL Server. There is no read-only session to switch on here, so the
//! statement guard carries the weight; each statement additionally runs in a
//! transaction that is always rolled back. A login with only SELECT rights is
//! what makes this airtight.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use tiberius::{
    AuthMethod, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, QueryItem, SqlBrowser,
};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::{ConnectionSpec, Fetched, Limits, Outcome, ResultColumn, TlsMode};

pub type Client = tiberius::Client<Compat<TcpStream>>;

fn config_for(spec: &ConnectionSpec) -> Config {
    let mut config = Config::new();
    config.host(&spec.host);
    if let Some(port) = spec.port {
        config.port(port);
    }
    if let Some(instance) = &spec.instance {
        config.instance_name(instance);
    }
    if let Some(database) = &spec.database {
        config.database(database);
    }
    config.authentication(AuthMethod::sql_server(
        spec.username.as_deref().unwrap_or_default(),
        spec.password.as_deref().unwrap_or_default(),
    ));
    config.application_name("kikkert");
    config.readonly(spec.read_intent);
    match spec.tls.unwrap_or(TlsMode::Require) {
        TlsMode::Disable => config.encryption(EncryptionLevel::NotSupported),
        TlsMode::Prefer => {
            config.encryption(EncryptionLevel::On);
            config.trust_cert();
        }
        TlsMode::Require => {
            config.encryption(EncryptionLevel::Required);
            config.trust_cert();
        }
        TlsMode::Verify => {
            config.encryption(EncryptionLevel::Required);
            if let Some(ca_file) = &spec.ca_file {
                config.trust_cert_ca(ca_file.display());
            }
        }
    }
    config
}

pub async fn open(spec: &ConnectionSpec, lock_timeout: Duration) -> Result<Client> {
    if spec.username.is_none() {
        bail!("SQL Server connections need a username and password (SQL authentication)");
    }
    // A named instance without a port is located through the SQL Browser
    // service; everything else is a plain TCP connection.
    let mut browse = spec.instance.is_some() && spec.port.is_none();
    let mut redirect: Option<(String, u16)> = None;

    // Azure SQL answers the first connection with "go to this node instead".
    for _ in 0..4 {
        let mut config = config_for(spec);
        if let Some((host, port)) = &redirect {
            config.host(host);
            config.port(*port);
        }
        let tcp = if browse {
            TcpStream::connect_named(&config).await.map_err(describe)?
        } else {
            TcpStream::connect(config.get_addr())
                .await
                .with_context(|| format!("Could not reach {}", config.get_addr()))?
        };
        tcp.set_nodelay(true)?;

        match tiberius::Client::connect(config, tcp.compat_write()).await {
            Ok(mut client) => {
                // Never wait on someone else's lock for long, and lose any
                // deadlock: the viewer must not get in a real workload's way.
                let setup = format!(
                    "SET LOCK_TIMEOUT {}; SET DEADLOCK_PRIORITY LOW;",
                    lock_timeout.as_millis()
                );
                run_batch(&mut client, &setup).await?;
                return Ok(client);
            }
            Err(tiberius::error::Error::Routing { host, port }) => {
                redirect = Some((host, port));
                browse = false;
            }
            Err(e) => return Err(describe(e)),
        }
    }
    bail!("The server kept redirecting the connection")
}

async fn run_batch(client: &mut Client, sql: &str) -> Result<()> {
    client
        .simple_query(sql)
        .await
        .map_err(describe)?
        .into_results()
        .await
        .map_err(describe)?;
    Ok(())
}

pub async fn fetch(client: &mut Client, sql: &str, limits: Limits) -> Outcome {
    if let Err(e) = run_batch(client, "BEGIN TRANSACTION").await {
        return Outcome {
            result: Err(e),
            reusable: false,
        };
    }

    let result = read_rows(client, sql, limits.max_rows).await;

    let mut reusable = true;
    if matches!(&result, Ok(fetched) if fetched.truncated) {
        // Tell the server to stop producing rows nobody will read.
        reusable = client.cancel_query().await.is_ok();
    }
    if reusable {
        reusable = run_batch(client, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")
            .await
            .is_ok();
    }
    Outcome { result, reusable }
}

async fn read_rows(client: &mut Client, sql: &str, max_rows: usize) -> Result<Fetched> {
    let mut columns: Vec<ResultColumn> = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;

    let mut stream = client.simple_query(sql).await.map_err(describe)?;
    while let Some(item) = stream.try_next().await.map_err(describe)? {
        match item {
            QueryItem::Metadata(metadata) if metadata.result_index() == 0 => {
                columns = metadata
                    .columns()
                    .iter()
                    .map(|column| {
                        let (type_name, numeric) = describe_type(column.column_type());
                        ResultColumn {
                            name: column.name().to_string(),
                            type_name: type_name.to_string(),
                            numeric,
                        }
                    })
                    .collect();
            }
            QueryItem::Row(row) if row.result_index() == 0 => {
                if rows.len() >= max_rows {
                    truncated = true;
                    break;
                }
                rows.push(row.into_iter().map(cell).collect());
            }
            _ => {}
        }
    }
    Ok(Fetched {
        columns,
        rows,
        truncated,
    })
}

fn describe_type(column_type: ColumnType) -> (&'static str, bool) {
    match column_type {
        ColumnType::Int1 => ("tinyint", true),
        ColumnType::Int2 => ("smallint", true),
        ColumnType::Int4 | ColumnType::Intn => ("int", true),
        ColumnType::Int8 => ("bigint", true),
        ColumnType::Float4 => ("real", true),
        ColumnType::Float8 | ColumnType::Floatn => ("float", true),
        ColumnType::Money | ColumnType::Money4 => ("money", true),
        ColumnType::Decimaln => ("decimal", true),
        ColumnType::Numericn => ("numeric", true),
        ColumnType::Bit | ColumnType::Bitn => ("bit", false),
        ColumnType::Guid => ("uniqueidentifier", false),
        ColumnType::Datetime | ColumnType::Datetimen => ("datetime", false),
        ColumnType::Datetime4 => ("smalldatetime", false),
        ColumnType::Datetime2 => ("datetime2", false),
        ColumnType::DatetimeOffsetn => ("datetimeoffset", false),
        ColumnType::Daten => ("date", false),
        ColumnType::Timen => ("time", false),
        ColumnType::BigVarChar => ("varchar", false),
        ColumnType::BigChar => ("char", false),
        ColumnType::NVarchar => ("nvarchar", false),
        ColumnType::NChar => ("nchar", false),
        ColumnType::Text => ("text", false),
        ColumnType::NText => ("ntext", false),
        ColumnType::BigVarBin => ("varbinary", false),
        ColumnType::BigBinary => ("binary", false),
        ColumnType::Image => ("image", false),
        ColumnType::Xml => ("xml", false),
        ColumnType::Udt => ("udt", false),
        ColumnType::SSVariant => ("sql_variant", false),
        ColumnType::Null => ("", false),
    }
}

fn cell(data: ColumnData<'static>) -> Option<String> {
    match data {
        ColumnData::U8(v) => v.map(|v| v.to_string()),
        ColumnData::I16(v) => v.map(|v| v.to_string()),
        ColumnData::I32(v) => v.map(|v| v.to_string()),
        ColumnData::I64(v) => v.map(|v| v.to_string()),
        ColumnData::F32(v) => v.map(|v| v.to_string()),
        ColumnData::F64(v) => v.map(|v| v.to_string()),
        ColumnData::Bit(v) => v.map(|v| if v { "1" } else { "0" }.to_string()),
        ColumnData::String(v) => v.map(|v| v.into_owned()),
        ColumnData::Guid(v) => v.map(|v| v.to_string().to_uppercase()),
        ColumnData::Binary(v) => v.map(|v| hex(&v)),
        ColumnData::Numeric(v) => v.map(|v| v.to_string()),
        ColumnData::Xml(v) => v.map(|v| v.to_string()),
        ref temporal @ (ColumnData::DateTime(_)
        | ColumnData::SmallDateTime(_)
        | ColumnData::DateTime2(_)) => chrono::NaiveDateTime::from_sql(temporal)
            .ok()
            .flatten()
            .map(|v| v.format("%Y-%m-%d %H:%M:%S%.f").to_string()),
        ref date @ ColumnData::Date(_) => chrono::NaiveDate::from_sql(date)
            .ok()
            .flatten()
            .map(|v| v.format("%Y-%m-%d").to_string()),
        ref time @ ColumnData::Time(_) => chrono::NaiveTime::from_sql(time)
            .ok()
            .flatten()
            .map(|v| v.format("%H:%M:%S%.f").to_string()),
        ref offset @ ColumnData::DateTimeOffset(_) => {
            chrono::DateTime::<chrono::FixedOffset>::from_sql(offset)
                .ok()
                .flatten()
                .map(|v| v.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string())
        }
    }
}

/// SQL Server binary values are always shown the way SSMS shows them.
fn hex(bytes: &[u8]) -> String {
    const SHOWN: usize = 64;
    let mut out = String::from("0x");
    for byte in bytes.iter().take(SHOWN) {
        out.push_str(&format!("{byte:02X}"));
    }
    if bytes.len() > SHOWN {
        out.push_str(&format!("… ({} bytes)", bytes.len()));
    }
    out
}

fn describe(error: tiberius::error::Error) -> anyhow::Error {
    match error {
        tiberius::error::Error::Server(token) => anyhow!("{}", token.message()),
        other => anyhow!("{other}"),
    }
}

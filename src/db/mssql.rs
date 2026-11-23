//! SQL Server. There is no read-only session to switch on here, so the
//! statement guard carries the weight; each statement additionally runs in a
//! transaction that is always rolled back. A login with only SELECT rights is
//! what makes this airtight.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use tiberius::time::time::{Date, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};
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
        | ColumnData::DateTime2(_)) => PrimitiveDateTime::from_sql(temporal)
            .ok()
            .flatten()
            .map(|v| format!("{} {}", format_date(v.date()), format_time(v.time()))),
        ref date @ ColumnData::Date(_) => Date::from_sql(date).ok().flatten().map(format_date),
        ref time @ ColumnData::Time(_) => Time::from_sql(time).ok().flatten().map(format_time),
        ref offset @ ColumnData::DateTimeOffset(_) => {
            OffsetDateTime::from_sql(offset).ok().flatten().map(|v| {
                let (date, time) = (format_date(v.date()), format_time(v.time()));
                format!("{date} {time} {}", format_offset(v.offset()))
            })
        }
    }
}

/// `2024-03-05`
fn format_date(date: Date) -> String {
    let (year, month, day) = (date.year(), u8::from(date.month()), date.day());
    format!("{year:04}-{month:02}-{day:02}")
}

/// `13:45:09`, with the fraction of a second in as many digits as it needs: 3, 6 or 9.
fn format_time(time: Time) -> String {
    let fraction = match time.nanosecond() {
        0 => String::new(),
        n if n % 1_000_000 == 0 => format!(".{:03}", n / 1_000_000),
        n if n % 1_000 == 0 => format!(".{:06}", n / 1_000),
        n => format!(".{n:09}"),
    };
    let (hour, minute, second) = time.as_hms();
    format!("{hour:02}:{minute:02}:{second:02}{fraction}")
}

/// `+01:00`
fn format_offset(offset: UtcOffset) -> String {
    let minutes = offset.whole_minutes();
    let sign = if minutes < 0 { '-' } else { '+' };
    let minutes = minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
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

#[cfg(test)]
mod tests {
    use super::*;
    use tiberius::time::{Date, DateTime, DateTime2, DateTimeOffset, SmallDateTime, Time};

    // SQL Server counts days from 0001-01-01 for date, datetime2 and datetimeoffset, and from
    // 1900-01-01 for datetime and smalldatetime.
    const MAR_5_2024: u32 = 738_949;
    const MAR_5_2024_FROM_1900: i32 = 45_354;
    /// 13:45:09, in seconds after midnight.
    const AFTERNOON: u64 = 49_509;

    /// A time of day: `secs` and then `fraction` in units of 10^-scale seconds.
    fn time(secs: u64, fraction: u64, scale: u8) -> Time {
        Time::new(secs * 10u64.pow(scale.into()) + fraction, scale)
    }

    fn datetime2(days: u32, time: Time) -> ColumnData<'static> {
        ColumnData::DateTime2(Some(DateTime2::new(Date::new(days), time)))
    }

    fn shown(data: ColumnData<'static>) -> String {
        cell(data).expect("a value")
    }

    #[test]
    fn datetime2_shows_as_many_fraction_digits_as_it_needs() {
        let at = |fraction, scale| shown(datetime2(MAR_5_2024, time(AFTERNOON, fraction, scale)));
        assert_eq!(at(0, 0), "2024-03-05 13:45:09");
        assert_eq!(at(0, 7), "2024-03-05 13:45:09");
        assert_eq!(at(120, 3), "2024-03-05 13:45:09.120");
        assert_eq!(at(123_456, 6), "2024-03-05 13:45:09.123456");
        assert_eq!(at(1_234_567, 7), "2024-03-05 13:45:09.123456700");
    }

    #[test]
    fn dates_are_zero_padded() {
        let date = |days| shown(ColumnData::Date(Some(Date::new(days))));
        assert_eq!(date(0), "0001-01-01");
        assert_eq!(date(MAR_5_2024), "2024-03-05");
        assert_eq!(date(3_652_058), "9999-12-31");
    }

    #[test]
    fn times_of_day() {
        let at = |time| shown(ColumnData::Time(Some(time)));
        assert_eq!(at(time(0, 0, 7)), "00:00:00");
        assert_eq!(at(time(AFTERNOON, 5, 1)), "13:45:09.500");
        assert_eq!(at(time(86_399, 9_999_999, 7)), "23:59:59.999999900");
    }

    #[test]
    fn datetime_and_smalldatetime() {
        // datetime counts 1/300 s, so one tick is 3.33 ms.
        let datetime = |days, ticks| shown(ColumnData::DateTime(Some(DateTime::new(days, ticks))));
        let ticks = AFTERNOON as u32 * 300;
        assert_eq!(datetime(MAR_5_2024_FROM_1900, ticks), "2024-03-05 13:45:09");
        assert_eq!(
            datetime(MAR_5_2024_FROM_1900, ticks + 1),
            "2024-03-05 13:45:09.003333333"
        );
        assert_eq!(datetime(-1, 0), "1899-12-31 00:00:00");

        // smalldatetime counts minutes.
        let small = ColumnData::SmallDateTime(Some(SmallDateTime::new(45_354, 13 * 60 + 45)));
        assert_eq!(shown(small), "2024-03-05 13:45:00");
    }

    #[test]
    fn datetimeoffset_shows_the_local_time_and_its_offset() {
        // Stored as the time in UTC and the offset in minutes.
        let at = |days, utc_secs, offset| {
            let utc = DateTime2::new(Date::new(days), time(utc_secs, 0, 7));
            shown(ColumnData::DateTimeOffset(Some(DateTimeOffset::new(
                utc, offset,
            ))))
        };
        assert_eq!(
            at(MAR_5_2024, AFTERNOON - 3_600, 60),
            "2024-03-05 13:45:09 +01:00"
        );
        assert_eq!(at(MAR_5_2024, 3_600, -210), "2024-03-04 21:30:00 -03:30");
        assert_eq!(at(MAR_5_2024, AFTERNOON, 0), "2024-03-05 13:45:09 +00:00");
    }

    #[test]
    fn null_temporals_stay_null() {
        assert_eq!(cell(ColumnData::DateTime2(None)), None);
        assert_eq!(cell(ColumnData::Date(None)), None);
        assert_eq!(cell(ColumnData::Time(None)), None);
        assert_eq!(cell(ColumnData::DateTimeOffset(None)), None);
        assert_eq!(cell(ColumnData::DateTime(None)), None);
    }
}

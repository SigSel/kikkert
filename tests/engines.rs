//! End-to-end checks against real databases. They are skipped unless
//! `scripts/test-dbs.sh up` has been run; use `scripts/test-dbs.sh test`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use kikkert::compare::{Comparison, Snapshot, Status};
use kikkert::db::meta::{self, DbObject, ObjectKind};
use kikkert::db::{ConnectionSpec, Engine, Limits, ResultSet, Session, TlsMode};
use kikkert::query::{ColumnInfo, Dir, Filter, Mode, Op, TableQuery};

const LIMITS: Limits = Limits {
    timeout: Duration::from_secs(10),
    max_rows: 1000,
};

fn spec(engine: Engine) -> Option<ConnectionSpec> {
    std::env::var("KIKKERT_TEST_DBS").ok()?;
    let base = ConnectionSpec {
        name: format!("test-{}", engine.label()),
        engine,
        host: "127.0.0.1".into(),
        port: None,
        instance: None,
        database: None,
        username: None,
        password: None,
        path: None,
        tls: None,
        ca_file: None,
        read_intent: false,
    };
    Some(match engine {
        Engine::Postgres => ConnectionSpec {
            port: Some(55432),
            database: Some("postgres".into()),
            username: Some("postgres".into()),
            password: Some("kikkert".into()),
            ..base
        },
        Engine::Mysql => ConnectionSpec {
            port: Some(53306),
            database: Some("shop".into()),
            username: Some("root".into()),
            password: Some("kikkert".into()),
            ..base
        },
        Engine::Mssql => ConnectionSpec {
            port: Some(51433),
            database: Some("kikkert_test".into()),
            username: Some("sa".into()),
            password: Some("Kikkert!Passw0rd".into()),
            tls: Some(TlsMode::Require),
            ..base
        },
        Engine::Sqlite => ConnectionSpec {
            host: String::new(),
            path: Some(PathBuf::from(std::env::var("KIKKERT_TEST_SQLITE").ok()?)),
            ..base
        },
    })
}

struct Db {
    engine: Engine,
    session: std::sync::Arc<Session>,
    objects: Vec<DbObject>,
}

impl Db {
    async fn open(engine: Engine) -> Option<Db> {
        let Some(spec) = spec(engine) else {
            eprintln!("skipping {engine:?}: run scripts/test-dbs.sh test");
            return None;
        };
        let session = Session::connect(spec, Duration::from_secs(5))
            .await
            .expect("connect");
        let listing = session
            .catalog_query(meta::objects_sql(engine), LIMITS)
            .await
            .expect("list objects");
        Some(Db {
            engine,
            session,
            objects: meta::parse_objects(&listing),
        })
    }

    fn object(&self, name: &str) -> &DbObject {
        let schema = match self.engine {
            Engine::Sqlite => None,
            _ => Some("shop"),
        };
        self.objects
            .iter()
            .find(|o| o.name == name && o.schema.as_deref() == schema)
            .unwrap_or_else(|| panic!("{name} not listed in {:?}", self.objects))
    }

    async fn structure(&self, name: &str) -> Vec<(&'static str, ResultSet)> {
        let mut out = Vec::new();
        for tab in meta::structure_tabs(self.engine, self.object(name)) {
            let result = self
                .session
                .catalog_query(&tab.sql, LIMITS)
                .await
                .unwrap_or_else(|e| panic!("{} tab of {name}: {e}\n{}", tab.title, tab.sql));
            out.push((tab.title, result));
        }
        out
    }

    async fn snapshot(&self, name: &str) -> Snapshot {
        let tabs = self.structure(name).await;
        Snapshot::new(
            &format!("test-{}", self.engine.label()),
            self.engine,
            &self.object(name).display_name(),
            &tabs[0].1,
        )
    }

    async fn table(&self, name: &str, page_size: usize) -> TableQuery {
        let tabs = self.structure(name).await;
        let columns = meta::parse_columns(&tabs[0].1);
        assert!(!columns.is_empty(), "no columns for {name}");
        TableQuery::new(self.object(name).table_ref(), columns, page_size)
    }

    async fn run(&self, query: &TableQuery) -> ResultSet {
        let sql = query.sql(self.engine);
        self.session
            .query(&sql, LIMITS)
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"))
    }

    async fn count(&self, query: &TableQuery) -> String {
        let sql = query.count_sql(self.engine).unwrap();
        let result = self.session.query(&sql, LIMITS).await.expect("count");
        result.rows[0][0].clone().unwrap()
    }

    /// The ids of the rows a filter leaves, in id order.
    async fn ids(&self, column: &str, op: Op, value: &str) -> Vec<String> {
        let mut query = self.table("customers", 50).await;
        query.filters.push(Filter {
            column: column.into(),
            op,
            value: value.into(),
        });
        let result = self.run(&query).await;
        result.rows.iter().map(|r| r[0].clone().unwrap()).collect()
    }
}

fn text(result: &ResultSet) -> String {
    result
        .rows
        .iter()
        .flatten()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

fn names(columns: &[ColumnInfo]) -> Vec<&str> {
    columns.iter().map(|c| c.name.as_str()).collect()
}

async fn exercise(engine: Engine) {
    let Some(db) = Db::open(engine).await else {
        return;
    };

    // --- what exists -------------------------------------------------------
    assert_eq!(db.object("customers").kind, ObjectKind::Table);
    assert_eq!(db.object("active_customers").kind, ObjectKind::View);
    db.object("empty_table");
    // A table whose name needs quoting, in each engine's own quote character.
    let odd_name = if engine == Engine::Mysql {
        "Odd `Name`"
    } else {
        "Odd \"Name\""
    };
    db.object(odd_name);

    // --- structure ---------------------------------------------------------
    let tabs = db.structure("customers").await;
    assert_eq!(tabs[0].0, "Columns");
    let columns = meta::parse_columns(&tabs[0].1);
    assert_eq!(
        names(&columns),
        [
            "id",
            "email",
            "name",
            "active",
            "balance",
            "created_at",
            "note",
            "token",
            "payload",
            "meta"
        ]
    );
    assert!(columns[0].primary_key, "id is the primary key");
    assert!(columns[1..].iter().all(|c| !c.primary_key));
    assert!(!columns[1].type_name.is_empty(), "types are reported");
    let indexes = &tabs.iter().find(|t| t.0 == "Indexes").unwrap().1;
    assert!(text(indexes).contains("customers_email_idx"), "{indexes:?}");

    let order_tabs = db.structure("orders").await;
    let keys = &order_tabs.iter().find(|t| t.0 == "Keys").unwrap().1;
    assert!(
        text(keys).contains("customers"),
        "foreign key shown: {keys:?}"
    );

    let view_tabs = db.structure("active_customers").await;
    let definition = &view_tabs.iter().find(|t| t.0 == "Definition").unwrap().1;
    assert!(
        text(definition).to_lowercase().contains("customers"),
        "{definition:?}"
    );

    // --- comparing structure -----------------------------------------------
    let customers = db.snapshot("customers").await;
    let itself = Comparison::new(customers.clone(), db.snapshot("customers").await);
    assert_eq!(
        itself.count(Status::Same),
        10,
        "a table is the same as itself"
    );
    let other = Comparison::new(customers, db.snapshot("empty_table").await);
    assert_eq!(other.count(Status::Same), 1, "id is in both");
    assert_eq!(other.count(Status::OnlyLeft), 9);
    assert_eq!(other.count(Status::OnlyRight), 1, "label");

    // --- the data, as it should be displayed -------------------------------
    let query = db.table("customers", 50).await;
    let page = db.run(&query).await;
    assert_eq!(page.rows.len(), 6);
    assert_eq!(page.columns.len(), 10);
    assert!(!page.truncated);
    assert!(page.columns[0].numeric, "id is right-aligned");
    assert!(!page.columns[2].numeric);

    let cell = |row: usize, col: usize| page.rows[row][col].as_deref();
    assert_eq!(cell(0, 0), Some("1"));
    assert_eq!(cell(0, 1), Some("ada@example.com"));
    assert_eq!(cell(0, 2), Some("Ada Lovelace"));
    assert_eq!(
        cell(0, 3),
        Some(if engine == Engine::Postgres {
            "true"
        } else {
            "1"
        })
    );
    assert_eq!(
        cell(1, 3),
        Some(if engine == Engine::Postgres {
            "false"
        } else {
            "0"
        })
    );
    assert_eq!(
        cell(0, 4),
        Some(if engine == Engine::Sqlite {
            "1234.5"
        } else {
            "1234.50"
        })
    );
    assert_eq!(cell(0, 5), Some("2024-01-15 09:30:00"));
    assert_eq!(cell(0, 6), None, "NULL stays NULL");
    assert_eq!(
        cell(0, 7).map(str::to_lowercase).as_deref(),
        Some("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")
    );
    assert_eq!(
        cell(0, 8).map(str::to_lowercase).as_deref(),
        Some(if engine == Engine::Postgres {
            "\\xdeadbeef"
        } else {
            "0xdeadbeef"
        })
    );
    assert!(cell(0, 9).unwrap().contains("gold"));
    assert_eq!(cell(1, 2), Some("Bob O'Brien"));
    assert_eq!(
        cell(1, 4),
        Some(if engine == Engine::Sqlite {
            "0"
        } else {
            "0.00"
        })
    );
    assert_eq!(cell(3, 1), None);
    assert_eq!(cell(3, 2), Some("Blåbær Ærlig"));
    assert_eq!(cell(3, 4), Some("-5.25"));
    assert_eq!(cell(3, 6), Some("unicode ✓"));
    assert_eq!(cell(4, 6), Some("back\\slash"));

    // --- searching ---------------------------------------------------------
    assert_eq!(
        db.ids("email", Op::Contains, "ADA").await,
        ["1", "3"],
        "case-insensitive"
    );
    assert_eq!(
        db.ids("note", Op::Contains, "50%_").await,
        ["2"],
        "wildcards are literal"
    );
    assert_eq!(db.ids("note", Op::Contains, "%").await, ["2"]);
    assert_eq!(db.ids("name", Op::Equals, "Bob O'Brien").await, ["2"]);
    assert_eq!(db.ids("name", Op::Contains, "Blåbær").await, ["4"]);
    assert_eq!(db.ids("note", Op::Equals, "back\\slash").await, ["5"]);
    assert_eq!(db.ids("note", Op::EndsWith, "\\slash").await, ["5"]);
    assert_eq!(db.ids("id", Op::Equals, "3").await, ["3"]);
    assert_eq!(
        db.ids("id", Op::Contains, "3").await,
        ["3"],
        "contains on a number"
    );
    assert_eq!(
        db.ids("created_at", Op::StartsWith, "2024-03").await,
        ["3", "4"]
    );
    assert_eq!(db.ids("email", Op::IsNull, "").await, ["4"]);
    assert_eq!(db.ids("email", Op::IsNotNull, "").await.len(), 5);
    assert_eq!(db.ids("balance", Op::Greater, "50").await, ["1", "5"]);
    assert_eq!(db.ids("balance", Op::LessEq, "0").await, ["2", "4"]);
    assert_eq!(
        db.ids("email", Op::NotEquals, "ada@example.com").await,
        ["2", "5", "6"]
    );
    assert_eq!(db.ids("email", Op::StartsWith, "carol").await, ["5"]);
    assert_eq!(
        db.ids("name", Op::Equals, "'; DROP TABLE customers; --")
            .await,
        Vec::<String>::new(),
        "a hostile value is just a value"
    );

    // --- sorting and paging ------------------------------------------------
    let mut sorted = db.table("customers", 50).await;
    sorted.toggle_sort("name", false);
    sorted.toggle_sort("name", false);
    assert_eq!(sorted.sort_of("name"), Some((0, Dir::Desc)));
    assert_eq!(db.run(&sorted).await.rows[0][2].as_deref(), Some("Carol"));

    let mut paged = db.table("customers", 4).await;
    assert_eq!(db.run(&paged).await.rows.len(), 4);
    paged.page = 1;
    let second = db.run(&paged).await;
    assert_eq!(second.rows.len(), 2);
    assert_eq!(second.rows[0][0].as_deref(), Some("5"));

    // --- duplicates and counts ---------------------------------------------
    let mut groups = db.table("customers", 50).await;
    groups.mode = Mode::Groups {
        columns: vec!["email".into()],
        duplicates_only: true,
    };
    let duplicates = db.run(&groups).await;
    assert_eq!(duplicates.rows.len(), 2, "{duplicates:?}");
    assert_eq!(
        duplicates.rows[0],
        [Some("ada@example.com".to_string()), Some("2".to_string())]
    );
    assert_eq!(duplicates.rows[1][0].as_deref(), Some("bob@example.com"));
    groups.mode = Mode::Groups {
        columns: vec!["email".into()],
        duplicates_only: false,
    };
    assert_eq!(
        db.run(&groups).await.rows.len(),
        4,
        "ada, bob, carol and NULL"
    );

    let mut counted = db.table("customers", 50).await;
    assert_eq!(db.count(&counted).await, "6");
    counted.filters.push(Filter {
        column: "email".into(),
        op: Op::Contains,
        value: "ada".into(),
    });
    assert_eq!(db.count(&counted).await, "2");

    // --- edge cases --------------------------------------------------------
    let empty = db.run(&db.table("empty_table", 50).await).await;
    assert!(empty.rows.is_empty());
    assert_eq!(
        empty
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "label"],
        "an empty table still shows its header"
    );

    let odd = db.table(odd_name, 50).await;
    assert_eq!(names(&odd.columns), ["select", "two words"]);
    assert_eq!(db.run(&odd).await.rows[0][1].as_deref(), Some("quoted"));

    // --- read-only, first line: the guard ----------------------------------
    let customers = engine.qualify(db.object("customers").schema.as_deref(), "customers");
    let refused = db
        .session
        .query(&format!("DELETE FROM {customers}"), LIMITS)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("read-only"), "{refused}");

    // --- read-only, second line: the server, with the guard bypassed -------
    // `catalog_query` skips the guard, which is exactly the situation the
    // read-only session exists for.
    let scratch = engine.qualify(db.object("customers").schema.as_deref(), "kikkert_was_here");
    let writes = [
        format!(
            "INSERT INTO {customers} (id, name, created_at) VALUES (99, 'intruder', '2024-01-01 00:00:00')"
        ),
        format!("UPDATE {customers} SET name = 'changed'"),
        format!("DELETE FROM {customers}"),
        format!("CREATE TABLE {scratch} (id int)"),
    ];
    for write in &writes {
        let outcome = db.session.catalog_query(write, LIMITS).await;
        if engine != Engine::Mssql {
            assert!(outcome.is_err(), "{engine:?} let this through: {write}");
        }
    }
    // Whatever each engine said, nothing may have changed.
    let after = db.run(&db.table("customers", 50).await).await;
    assert_eq!(after.rows.len(), 6, "row count unchanged");
    assert_eq!(
        after.rows[0][2].as_deref(),
        Some("Ada Lovelace"),
        "values unchanged"
    );
    let listing = db
        .session
        .catalog_query(meta::objects_sql(engine), LIMITS)
        .await
        .unwrap();
    assert!(
        !meta::parse_objects(&listing)
            .iter()
            .any(|o| o.name == "kikkert_was_here"),
        "no table was created"
    );

    // --- large results are cut off, and the session carries on -------------
    let big = format!(
        "SELECT a.id FROM {customers} a CROSS JOIN {customers} b CROSS JOIN {customers} c CROSS JOIN {customers} d"
    );
    let capped = Limits {
        timeout: Duration::from_secs(10),
        max_rows: 100,
    };
    for _ in 0..5 {
        let result = db.session.query(&big, capped).await.expect("big query");
        assert_eq!(result.rows.len(), 100);
        assert!(result.truncated);
        assert_eq!(db.run(&db.table("orders", 50).await).await.rows.len(), 3);
    }
    let whole = db
        .session
        .query(
            &big,
            Limits {
                max_rows: 5000,
                ..capped
            },
        )
        .await
        .unwrap();
    assert_eq!(whole.rows.len(), 1296);
    assert!(!whole.truncated);

    // --- a query that never finishes is abandoned --------------------------
    let slow = match engine {
        Engine::Postgres => "SELECT pg_sleep(30)".to_string(),
        Engine::Mysql => "SELECT SLEEP(30)".to_string(),
        Engine::Mssql => "WAITFOR DELAY '00:00:30'".to_string(),
        Engine::Sqlite => {
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT count(*) FROM n"
                .to_string()
        }
    };
    let impatient = Limits {
        timeout: Duration::from_secs(1),
        max_rows: 10,
    };
    let started = Instant::now();
    let outcome = db.session.catalog_query(&slow, impatient).await;
    let waited = started.elapsed();
    assert!(waited < Duration::from_secs(6), "gave up after {waited:?}");
    if engine != Engine::Mysql {
        // MySQL ends an interrupted SLEEP without an error.
        assert!(outcome.is_err(), "{engine:?} slow query should fail");
    }
    assert_eq!(
        db.run(&db.table("orders", 50).await).await.rows.len(),
        3,
        "still usable afterwards"
    );

    // --- several statements at once ----------------------------------------
    let query = db.table("customers", 50).await;
    let sql = query.sql(engine);
    let (a, b, c) = tokio::join!(
        db.session.query(&sql, LIMITS),
        db.session.query(&sql, LIMITS),
        db.session.query(&sql, LIMITS)
    );
    for result in [a, b, c] {
        assert_eq!(result.unwrap().rows.len(), 6);
    }
}

#[tokio::test]
async fn postgres() {
    exercise(Engine::Postgres).await;
}

#[tokio::test]
async fn mysql() {
    exercise(Engine::Mysql).await;
}

#[tokio::test]
async fn mssql() {
    exercise(Engine::Mssql).await;
}

#[tokio::test]
async fn sqlite() {
    exercise(Engine::Sqlite).await;
}

/// The same table on two engines: types and defaults that each engine spells
/// its own way compare alike, and only real differences are left.
#[tokio::test]
async fn one_table_compared_across_engines() {
    use Status::{Alike, Different, Same};
    let Some(postgres) = Db::open(Engine::Postgres).await else {
        return;
    };
    let base = postgres.snapshot("customers").await;
    for engine in [Engine::Mysql, Engine::Mssql, Engine::Sqlite] {
        let Some(db) = Db::open(engine).await else {
            continue;
        };
        let comparison = Comparison::new(base.clone(), db.snapshot("customers").await);
        assert_eq!(comparison.rows.len(), 10, "{engine:?}: every column paired");
        let expected = match engine {
            Engine::Mssql => [Alike, Different, Alike, Alike, Alike, Different],
            _ => [Alike, Same, Alike, Alike, Different, Different],
        };
        for (column, status) in ["email", "name", "active", "created_at", "token", "meta"]
            .into_iter()
            .zip(expected)
        {
            let row = comparison
                .rows
                .iter()
                .find(|r| comparison.names(r).0 == Some(column))
                .unwrap();
            assert_eq!(
                row.status(),
                status,
                "{engine:?} {column}: {:?}",
                comparison
                    .attributes
                    .iter()
                    .map(|a| comparison.values(row, a))
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[tokio::test]
async fn a_wrong_password_is_a_clear_error() {
    let Some(mut spec) = spec(Engine::Postgres) else {
        return;
    };
    spec.password = Some("wrong".into());
    let error = Session::connect(spec, Duration::from_secs(5))
        .await
        .err()
        .expect("should not connect")
        .to_string();
    assert!(error.to_lowercase().contains("password"), "{error}");
    assert!(
        !error.contains("wrong"),
        "the password is not echoed: {error}"
    );
}

/// Abandoning a statement (Esc in the interface) has to stop it on the
/// server as well, not just stop waiting for it.
async fn cancelling_stops_the_server(engine: Engine, slow: &str, still_running: &str) {
    let Some(db) = Db::open(engine).await else {
        return;
    };
    let running = async || {
        let result = db
            .session
            .catalog_query(still_running, LIMITS)
            .await
            .expect("activity");
        result.rows[0][0].clone().unwrap()
    };

    let session = db.session.clone();
    let slow = slow.to_string();
    let patient = Limits {
        timeout: Duration::from_secs(60),
        max_rows: 10,
    };
    let task = tokio::spawn(async move { session.catalog_query(&slow, patient).await });

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(running().await, "1", "the slow statement should be running");

    task.abort();
    let mut stopped = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if running().await == "0" {
            stopped = true;
            break;
        }
    }
    assert!(
        stopped,
        "{engine:?} kept running the statement after it was cancelled"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_postgres() {
    cancelling_stops_the_server(
        Engine::Postgres,
        "SELECT pg_sleep(31)",
        "SELECT count(*) FROM pg_stat_activity \
         WHERE state = 'active' AND query = 'SELECT pg_sleep(31)' AND pid <> pg_backend_pid()",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_mysql() {
    cancelling_stops_the_server(
        Engine::Mysql,
        "SELECT SLEEP(31)",
        "SELECT COUNT(*) FROM information_schema.processlist \
         WHERE info = 'SELECT SLEEP(31)' AND id <> CONNECTION_ID()",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_mssql() {
    cancelling_stops_the_server(
        Engine::Mssql,
        "WAITFOR DELAY '00:00:31'",
        "SELECT COUNT(*) FROM sys.dm_exec_requests r \
         CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t \
         WHERE t.text LIKE 'WAITFOR DELAY ''00:00:31''%' AND r.session_id <> @@SPID",
    )
    .await;
}

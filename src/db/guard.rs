//! The read-only gate. Every statement that did not come from kikkert's own
//! catalog queries passes through [`check`] before it is sent anywhere.
//!
//! The rule is an allow-list: exactly one statement, and it must be a query
//! (or an EXPLAIN / SHOW / DESCRIBE of one). Anything the parser cannot
//! understand is refused rather than passed along, because a statement that
//! cannot be read cannot be shown to be harmless.
//!
//! This is one layer of several. The session itself is also read-only where
//! the engine supports it (see the driver modules), and the strongest
//! guarantee of all is a database login that only has SELECT rights.

use std::fmt;
use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, ObjectName, ObjectNamePart, Query, Select, SetExpr, Statement, TableFactor, Visit,
    Visitor,
};
use sqlparser::dialect::{Dialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::keywords::Keyword;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

use super::Engine;

/// Why a statement was not sent to the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(String);

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Not sent: kikkert is read-only. {}", self.0)
    }
}

impl std::error::Error for Refusal {}

fn refuse<T>(why: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(why.into()))
}

fn dialect_for(engine: Engine) -> Box<dyn Dialect> {
    match engine {
        Engine::Postgres => Box::new(PostgreSqlDialect {}),
        Engine::Mysql => Box::new(MySqlDialect {}),
        Engine::Mssql => Box::new(MsSqlDialect {}),
        Engine::Sqlite => Box::new(SQLiteDialect {}),
    }
}

/// Accepts `sql` only if it is a single statement that reads and does
/// nothing else.
pub fn check(engine: Engine, sql: &str) -> Result<(), Refusal> {
    if sql.trim().is_empty() {
        return refuse("There is no statement to run.");
    }
    let dialect = dialect_for(engine);

    // First, independently of the parser's verdict: the statement has to
    // start like a query.
    check_leading_keyword(dialect.as_ref(), sql)?;

    let statements = match Parser::parse_sql(dialect.as_ref(), sql) {
        Ok(statements) => statements,
        Err(e) => {
            return refuse(format!(
                "This statement could not be verified as read-only ({e})."
            ));
        }
    };
    match statements.as_slice() {
        [] => return refuse("There is no statement to run."),
        [_] => {}
        _ => return refuse("Only one statement can be run at a time."),
    }

    let mut walker = Walker { engine };
    match statements[0].visit(&mut walker) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(why) => refuse(why),
    }
}

fn check_leading_keyword(dialect: &dyn Dialect, sql: &str) -> Result<(), Refusal> {
    let tokens = match Tokenizer::new(dialect, sql).tokenize() {
        Ok(tokens) => tokens,
        Err(e) => {
            return refuse(format!(
                "This statement could not be verified as read-only ({e})."
            ));
        }
    };
    let first = tokens
        .iter()
        .find(|t| !matches!(t, Token::Whitespace(_) | Token::SemiColon));
    match first {
        Some(Token::LParen) => Ok(()),
        Some(Token::Word(w))
            if matches!(
                w.keyword,
                Keyword::SELECT
                    | Keyword::WITH
                    | Keyword::VALUES
                    | Keyword::TABLE
                    | Keyword::EXPLAIN
                    | Keyword::SHOW
                    | Keyword::DESCRIBE
                    | Keyword::DESC
            ) =>
        {
            Ok(())
        }
        Some(other) => refuse(format!(
            "Only SELECT queries can be run, and this starts with {}.",
            other.to_string().to_uppercase()
        )),
        None => refuse("There is no statement to run."),
    }
}

fn statement_kind(statement: &Statement) -> String {
    statement
        .to_string()
        .split_whitespace()
        .next()
        .unwrap_or("That")
        .to_uppercase()
}

fn allowed_statement(statement: &Statement) -> bool {
    match statement {
        Statement::Query(_) => true,
        // The explained statement is visited too, so it has to be a query.
        Statement::Explain { .. } | Statement::ExplainTable { .. } => true,
        Statement::ShowTables { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowCollation { .. }
        | Statement::ShowFunctions { .. } => true,
        _ => false,
    }
}

/// Functions that change state, take lasting locks or reach outside the
/// database even though they are called from a SELECT. The read-only session
/// already stops most writes; this list covers what a session cannot.
fn denied_function(engine: Engine, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let (exact, prefixes): (&[&str], &[&str]) = match engine {
        Engine::Postgres => (
            &["set_config", "setval", "nextval", "pg_promote", "pg_notify"],
            &[
                "pg_advisory",
                "pg_try_advisory",
                "pg_terminate",
                "pg_cancel",
                "pg_reload",
                "pg_rotate",
                "pg_switch",
                "pg_create",
                "pg_drop",
                "pg_replication",
                "pg_logical",
                "pg_wal_replay",
                "pg_backup",
                "pg_start_backup",
                "pg_stop_backup",
                "pg_read_",
                "pg_ls_",
                "pg_stat_reset",
                "pg_file_",
                "pg_import",
                "pg_export",
                "lo_",
                "dblink",
            ],
        ),
        Engine::Mysql => (
            &[
                "sleep",
                "benchmark",
                "get_lock",
                "release_lock",
                "release_all_locks",
                "load_file",
                "master_pos_wait",
                "source_pos_wait",
                "sys_exec",
                "sys_eval",
            ],
            &[],
        ),
        Engine::Mssql => (
            &["openrowset", "openquery", "opendatasource", "openxml"],
            &["xp_", "sp_"],
        ),
        Engine::Sqlite => (
            &[
                "load_extension",
                "writefile",
                "readfile",
                "edit",
                "fts3_tokenizer",
            ],
            &[],
        ),
    };
    exact.contains(&name.as_str()) || prefixes.iter().any(|p| name.starts_with(p))
}

fn last_ident(name: &ObjectName) -> Option<&str> {
    match name.0.last()? {
        ObjectNamePart::Identifier(ident) => Some(ident.value.as_str()),
        ObjectNamePart::Function(f) => Some(f.name.value.as_str()),
    }
}

fn body_problem(body: &SetExpr) -> Option<&'static str> {
    match body {
        SetExpr::Insert(_) => Some("INSERT inside a query is not allowed."),
        SetExpr::Update(_) => Some("UPDATE inside a query is not allowed."),
        SetExpr::Delete(_) => Some("DELETE inside a query is not allowed."),
        SetExpr::Merge(_) => Some("MERGE inside a query is not allowed."),
        SetExpr::SetOperation { left, right, .. } => {
            body_problem(left).or_else(|| body_problem(right))
        }
        // Nested queries get their own visit.
        SetExpr::Select(_) | SetExpr::Query(_) | SetExpr::Values(_) | SetExpr::Table(_) => None,
    }
}

struct Walker {
    engine: Engine,
}

impl Walker {
    fn check_callable(&self, name: &ObjectName) -> ControlFlow<String> {
        match last_ident(name) {
            Some(function) if denied_function(self.engine, function) => ControlFlow::Break(
                format!("The function {function} can change state, so it is not allowed."),
            ),
            _ => ControlFlow::Continue(()),
        }
    }
}

impl Visitor for Walker {
    type Break = String;

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<String> {
        if allowed_statement(statement) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(format!(
                "Only SELECT queries can be run; {} statements are not allowed.",
                statement_kind(statement)
            ))
        }
    }

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<String> {
        if !query.locks.is_empty() {
            return ControlFlow::Break(
                "Locking clauses (FOR UPDATE / FOR SHARE) are not allowed.".into(),
            );
        }
        match body_problem(&query.body) {
            Some(problem) => ControlFlow::Break(problem.into()),
            None => ControlFlow::Continue(()),
        }
    }

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<String> {
        if select.into.is_some() {
            return ControlFlow::Break(
                "SELECT ... INTO writes its result somewhere, so it is not allowed.".into(),
            );
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<String> {
        match expr {
            Expr::Function(function) => self.check_callable(&function.name),
            _ => ControlFlow::Continue(()),
        }
    }

    // Table-valued functions show up as relations.
    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<String> {
        self.check_callable(relation)
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<String> {
        match factor {
            TableFactor::Function { name, .. } => self.check_callable(name),
            _ => ControlFlow::Continue(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Engine; 4] = [
        Engine::Postgres,
        Engine::Mysql,
        Engine::Mssql,
        Engine::Sqlite,
    ];

    fn ok(engine: Engine, sql: &str) {
        if let Err(e) = check(engine, sql) {
            panic!("{engine:?} should accept {sql:?}, but: {e}");
        }
    }

    fn refused(engine: Engine, sql: &str) {
        assert!(
            check(engine, sql).is_err(),
            "{engine:?} should refuse {sql:?}"
        );
    }

    #[test]
    fn plain_queries_are_accepted() {
        for engine in ALL {
            ok(engine, "SELECT 1");
            ok(engine, "select * from users where id = 1");
            ok(
                engine,
                "SELECT a, COUNT(*) FROM t GROUP BY a HAVING COUNT(*) > 1 ORDER BY 2 DESC",
            );
            ok(engine, "WITH x AS (SELECT 1 AS a) SELECT * FROM x");
            ok(engine, "SELECT * FROM a UNION ALL SELECT * FROM b");
            ok(engine, "-- a comment\nSELECT 1;");
            ok(engine, "SELECT (SELECT max(id) FROM t) AS m");
        }
        ok(
            Engine::Postgres,
            "SELECT \"a\" FROM \"public\".\"t\" WHERE CAST(\"a\" AS TEXT) ILIKE '%x%' ESCAPE '!' LIMIT 10 OFFSET 0",
        );
        ok(Engine::Postgres, "EXPLAIN SELECT * FROM t");
        ok(Engine::Postgres, "EXPLAIN ANALYZE SELECT * FROM t");
        ok(Engine::Postgres, "SHOW server_version");
        ok(Engine::Mysql, "SELECT `a` FROM `db`.`t` LIMIT 10 OFFSET 0");
        ok(Engine::Mysql, "SHOW TABLES");
        ok(Engine::Mysql, "DESCRIBE users");
        ok(
            Engine::Mssql,
            "SELECT [a] FROM [dbo].[t] ORDER BY (SELECT NULL) OFFSET 0 ROWS FETCH NEXT 10 ROWS ONLY",
        );
        ok(Engine::Mssql, "SELECT TOP 10 * FROM dbo.t WITH (NOLOCK)");
        ok(Engine::Sqlite, "SELECT * FROM pragma_table_info('t')");
    }

    #[test]
    fn writes_are_refused() {
        for engine in ALL {
            refused(engine, "");
            refused(engine, "   ");
            refused(engine, "INSERT INTO t VALUES (1)");
            refused(engine, "UPDATE t SET a = 1");
            refused(engine, "DELETE FROM t");
            refused(engine, "DROP TABLE t");
            refused(engine, "TRUNCATE TABLE t");
            refused(engine, "CREATE TABLE t (a int)");
            refused(engine, "ALTER TABLE t ADD b int");
            refused(engine, "GRANT ALL ON t TO u");
            refused(engine, "COMMIT");
            refused(engine, "BEGIN");
            refused(engine, "this is not sql");
        }
    }

    #[test]
    fn a_second_statement_is_refused() {
        for engine in ALL {
            refused(engine, "SELECT 1; DROP TABLE t");
            refused(engine, "SELECT 1; SELECT 2");
            refused(engine, "SELECT 1 /* x */; DELETE FROM t --");
        }
    }

    #[test]
    fn writes_hidden_inside_queries_are_refused() {
        refused(
            Engine::Postgres,
            "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
        );
        refused(
            Engine::Postgres,
            "WITH u AS (UPDATE t SET a = 1 RETURNING *) SELECT * FROM u",
        );
        refused(
            Engine::Postgres,
            "WITH i AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM i",
        );
        refused(Engine::Postgres, "SELECT * INTO backup FROM t");
        refused(Engine::Mssql, "SELECT * INTO backup FROM t");
        refused(Engine::Mysql, "SELECT * FROM t INTO OUTFILE '/tmp/x'");
        refused(Engine::Postgres, "SELECT * FROM t FOR UPDATE");
        refused(Engine::Mysql, "SELECT * FROM t FOR UPDATE");
        refused(Engine::Postgres, "EXPLAIN ANALYZE DELETE FROM t");
        refused(Engine::Postgres, "EXPLAIN (ANALYZE) UPDATE t SET a = 1");
    }

    #[test]
    fn session_and_procedure_statements_are_refused() {
        refused(Engine::Postgres, "SET default_transaction_read_only = off");
        refused(
            Engine::Postgres,
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        );
        refused(Engine::Postgres, "COPY t TO '/tmp/x'");
        refused(Engine::Postgres, "CALL do_things()");
        refused(Engine::Postgres, "DO $$ BEGIN DELETE FROM t; END $$");
        refused(Engine::Mysql, "SET SESSION TRANSACTION READ WRITE");
        refused(Engine::Mysql, "CALL do_things()");
        refused(Engine::Mysql, "LOCK TABLES t WRITE");
        refused(Engine::Mssql, "EXEC sp_who");
        refused(Engine::Mssql, "EXECUTE('DROP TABLE t')");
        refused(Engine::Mssql, "USE master");
        refused(Engine::Sqlite, "PRAGMA query_only = OFF");
        refused(Engine::Sqlite, "ATTACH DATABASE 'x.db' AS x");
        refused(Engine::Sqlite, "VACUUM");
    }

    #[test]
    fn state_changing_functions_are_refused() {
        refused(
            Engine::Postgres,
            "SELECT set_config('default_transaction_read_only', 'off', false)",
        );
        refused(Engine::Postgres, "SELECT pg_advisory_lock(1)");
        refused(
            Engine::Postgres,
            "SELECT pg_catalog.pg_terminate_backend(123)",
        );
        refused(Engine::Postgres, "SELECT * FROM t WHERE id = nextval('s')");
        refused(Engine::Postgres, "SELECT lo_unlink(1)");
        refused(
            Engine::Postgres,
            "SELECT * FROM dblink('x', 'DELETE FROM t') AS r(a int)",
        );
        refused(Engine::Mysql, "SELECT GET_LOCK('x', 10)");
        refused(Engine::Mysql, "SELECT SLEEP(100)");
        refused(
            Engine::Mssql,
            "SELECT * FROM OPENQUERY(other, 'DELETE FROM t')",
        );
        refused(Engine::Sqlite, "SELECT load_extension('evil.so')");
    }

    #[test]
    fn refusals_explain_themselves() {
        let e = check(Engine::Postgres, "DROP TABLE t")
            .unwrap_err()
            .to_string();
        assert!(e.contains("read-only"), "{e}");
        assert!(e.contains("DROP"), "{e}");
    }
}

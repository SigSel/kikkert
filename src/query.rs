//! Turns "this table, these filters, this order" into SQL. The SQL is always
//! shown to the user and can be edited by hand, so it is written to be read.

use crate::db::Engine;
use crate::db::dialect::{TypeClass, classify};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub primary_key: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Contains,
    Equals,
    StartsWith,
    EndsWith,
    NotEquals,
    NotContains,
    Greater,
    GreaterEq,
    Less,
    LessEq,
    IsNull,
    IsNotNull,
}

impl Op {
    pub const ALL: [Op; 12] = [
        Op::Contains,
        Op::Equals,
        Op::StartsWith,
        Op::EndsWith,
        Op::NotEquals,
        Op::NotContains,
        Op::Greater,
        Op::GreaterEq,
        Op::Less,
        Op::LessEq,
        Op::IsNull,
        Op::IsNotNull,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Op::Contains => "contains",
            Op::Equals => "equals",
            Op::StartsWith => "starts with",
            Op::EndsWith => "ends with",
            Op::NotEquals => "is not",
            Op::NotContains => "does not contain",
            Op::Greater => "greater than",
            Op::GreaterEq => "at least",
            Op::Less => "less than",
            Op::LessEq => "at most",
            Op::IsNull => "is NULL",
            Op::IsNotNull => "is not NULL",
        }
    }

    pub fn needs_value(self) -> bool {
        !matches!(self, Op::IsNull | Op::IsNotNull)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    pub column: String,
    pub op: Op,
    pub value: String,
}

impl Filter {
    /// A short description for lists: `email contains "foo"`.
    pub fn describe(&self) -> String {
        if self.op.needs_value() {
            format!("{} {} \"{}\"", self.column, self.op.label(), self.value)
        } else {
            format!("{} {}", self.column, self.op.label())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Asc,
    Desc,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sort {
    pub column: String,
    pub dir: Dir,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The table's rows.
    Rows,
    /// One row per distinct combination of `columns`, with how often it occurs.
    Groups {
        columns: Vec<String>,
        duplicates_only: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableQuery {
    pub table: TableRef,
    pub columns: Vec<ColumnInfo>,
    pub filters: Vec<Filter>,
    pub sorts: Vec<Sort>,
    pub mode: Mode,
    pub page: usize,
    pub page_size: usize,
}

impl TableQuery {
    pub fn new(table: TableRef, columns: Vec<ColumnInfo>, page_size: usize) -> Self {
        TableQuery {
            table,
            columns,
            filters: Vec::new(),
            sorts: Vec::new(),
            mode: Mode::Rows,
            page: 0,
            page_size: page_size.max(1),
        }
    }

    pub fn offset(&self) -> usize {
        self.page * self.page_size
    }

    pub fn sort_of(&self, column: &str) -> Option<(usize, Dir)> {
        self.sorts
            .iter()
            .position(|s| s.column == column)
            .map(|i| (i, self.sorts[i].dir))
    }

    /// Cycles a column through ascending, descending and unsorted. With
    /// `additional`, the column joins the existing ordering instead of
    /// replacing it.
    pub fn toggle_sort(&mut self, column: &str, additional: bool) {
        let current = self.sort_of(column);
        if !additional {
            self.sorts.retain(|s| s.column == column);
        }
        match current {
            None => self.sorts.push(Sort {
                column: column.to_string(),
                dir: Dir::Asc,
            }),
            Some((_, Dir::Asc)) => {
                if let Some(sort) = self.sorts.iter_mut().find(|s| s.column == column) {
                    sort.dir = Dir::Desc;
                }
            }
            Some((_, Dir::Desc)) => self.sorts.retain(|s| s.column != column),
        }
        self.page = 0;
    }

    fn class_of(&self, engine: Engine, column: &str) -> TypeClass {
        let Some(info) = self.columns.iter().find(|c| c.name == column) else {
            return TypeClass::Other;
        };
        // SQL Server's "timestamp" is a binary row version, not a time.
        if engine == Engine::Mssql
            && matches!(
                info.type_name.to_ascii_lowercase().as_str(),
                "timestamp" | "rowversion"
            )
        {
            return TypeClass::Other;
        }
        classify(&info.type_name)
    }

    /// The SQL for one filter, as it appears after WHERE.
    pub fn predicate(&self, engine: Engine, filter: &Filter) -> String {
        let ident = engine.quote_ident(&filter.column);
        let class = self.class_of(engine, &filter.column);
        let text = engine.as_text(&ident, class);
        let like = engine.like_keyword();
        let value = filter.value.as_str();
        let literal = || engine.value_literal(class, value);
        match filter.op {
            Op::Contains => format!("{text} {like} {}", engine.like_literal("%", value, "%")),
            Op::NotContains => {
                format!("{text} NOT {like} {}", engine.like_literal("%", value, "%"))
            }
            Op::StartsWith => format!("{text} {like} {}", engine.like_literal("", value, "%")),
            Op::EndsWith => format!("{text} {like} {}", engine.like_literal("%", value, "")),
            Op::Equals => format!("{ident} = {}", literal()),
            Op::NotEquals => format!("{ident} <> {}", literal()),
            Op::Greater => format!("{ident} > {}", literal()),
            Op::GreaterEq => format!("{ident} >= {}", literal()),
            Op::Less => format!("{ident} < {}", literal()),
            Op::LessEq => format!("{ident} <= {}", literal()),
            Op::IsNull => format!("{ident} IS NULL"),
            Op::IsNotNull => format!("{ident} IS NOT NULL"),
        }
    }

    fn table_and_filters(&self, engine: Engine) -> String {
        let mut sql = format!(
            "FROM {}",
            engine.qualify(self.table.schema.as_deref(), &self.table.name)
        );
        for (i, filter) in self.filters.iter().enumerate() {
            sql.push_str(if i == 0 { "\nWHERE " } else { "\n  AND " });
            sql.push_str(&self.predicate(engine, filter));
        }
        sql
    }

    /// The query for the current page.
    pub fn sql(&self, engine: Engine) -> String {
        let page = engine.page_clause(self.page_size, self.offset());
        match &self.mode {
            Mode::Rows => {
                let columns: Vec<String> = self
                    .columns
                    .iter()
                    .map(|c| engine.quote_ident(&c.name))
                    .collect();
                let mut order: Vec<String> = self
                    .sorts
                    .iter()
                    .map(|s| format!("{} {}", engine.quote_ident(&s.column), dir_sql(s.dir)))
                    .collect();
                if order.is_empty() {
                    // Without an order, pages are not stable. The primary key
                    // is the one ordering that is always cheap.
                    order = self
                        .columns
                        .iter()
                        .filter(|c| c.primary_key)
                        .map(|c| format!("{} ASC", engine.quote_ident(&c.name)))
                        .collect();
                }
                if order.is_empty() && engine.requires_order_for_paging() {
                    order.push("(SELECT NULL)".to_string());
                }

                let mut sql = format!(
                    "SELECT {}\n{}",
                    select_list(&columns),
                    self.table_and_filters(engine)
                );
                if !order.is_empty() {
                    sql.push_str(&format!("\nORDER BY {}", order.join(", ")));
                }
                sql.push_str(&format!("\n{page}"));
                sql
            }
            Mode::Groups {
                columns,
                duplicates_only,
            } => {
                let idents: Vec<String> = columns.iter().map(|c| engine.quote_ident(c)).collect();
                let count = engine.count_star();
                let list = idents.join(", ");
                let mut sql = format!(
                    "SELECT {list}, {count} AS {}\n{}\nGROUP BY {list}",
                    engine.quote_ident("count"),
                    self.table_and_filters(engine),
                );
                if *duplicates_only {
                    sql.push_str(&format!("\nHAVING {count} > 1"));
                }
                sql.push_str(&format!("\nORDER BY {count} DESC, {list}\n{page}"));
                sql
            }
        }
    }

    /// Counts the rows matching the current filters. Only meaningful for
    /// [`Mode::Rows`].
    pub fn count_sql(&self, engine: Engine) -> Option<String> {
        match self.mode {
            Mode::Rows => Some(format!(
                "SELECT {}\n{}",
                engine.count_star(),
                self.table_and_filters(engine)
            )),
            Mode::Groups { .. } => None,
        }
    }
}

fn dir_sql(dir: Dir) -> &'static str {
    match dir {
        Dir::Asc => "ASC",
        Dir::Desc => "DESC",
    }
}

/// Lays out a long column list over several lines so it stays readable in
/// the SQL editor.
fn select_list(columns: &[String]) -> String {
    const WIDTH: usize = 96;
    let one_line = columns.join(", ");
    if one_line.len() <= WIDTH {
        return one_line;
    }
    let mut out = String::new();
    let mut line = String::new();
    for (i, column) in columns.iter().enumerate() {
        let piece = if i + 1 < columns.len() {
            format!("{column}, ")
        } else {
            column.clone()
        };
        if !line.is_empty() && line.len() + piece.len() > WIDTH {
            out.push_str(line.trim_end());
            out.push_str("\n       ");
            line.clear();
        }
        line.push_str(&piece);
    }
    out.push_str(&line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::guard;

    const ALL: [Engine; 4] = [
        Engine::Postgres,
        Engine::Mysql,
        Engine::Mssql,
        Engine::Sqlite,
    ];

    fn col(name: &str, type_name: &str, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            type_name: type_name.to_string(),
            primary_key: pk,
        }
    }

    fn users() -> TableQuery {
        TableQuery::new(
            TableRef {
                schema: Some("app".to_string()),
                name: "users".to_string(),
            },
            vec![
                col("id", "int", true),
                col("email", "varchar(255)", false),
                col("created_at", "datetime", false),
            ],
            50,
        )
    }

    fn filter(column: &str, op: Op, value: &str) -> Filter {
        Filter {
            column: column.to_string(),
            op,
            value: value.to_string(),
        }
    }

    #[test]
    fn a_fresh_table_lists_every_column_ordered_by_key() {
        assert_eq!(
            users().sql(Engine::Postgres),
            "SELECT \"id\", \"email\", \"created_at\"\nFROM \"app\".\"users\"\nORDER BY \"id\" ASC\nLIMIT 50 OFFSET 0"
        );
        assert_eq!(
            users().sql(Engine::Mssql),
            "SELECT [id], [email], [created_at]\nFROM [app].[users]\nORDER BY [id] ASC\nOFFSET 0 ROWS FETCH NEXT 50 ROWS ONLY"
        );
    }

    #[test]
    fn sql_server_pages_even_without_a_key() {
        let mut q = users();
        q.columns[0].primary_key = false;
        assert!(
            q.sql(Engine::Mssql)
                .contains("ORDER BY (SELECT NULL)\nOFFSET 0 ROWS")
        );
        assert!(!q.sql(Engine::Postgres).contains("ORDER BY"));
    }

    #[test]
    fn filters_become_a_where_clause() {
        let mut q = users();
        q.filters.push(filter("email", Op::Contains, "o'neil_50%"));
        q.filters.push(filter("id", Op::Greater, "10"));
        q.filters.push(filter("created_at", Op::IsNull, ""));
        q.page = 2;
        assert_eq!(
            q.sql(Engine::Postgres),
            "SELECT \"id\", \"email\", \"created_at\"\nFROM \"app\".\"users\"\n\
             WHERE \"email\" ILIKE '%o''neil!_50!%%' ESCAPE '!'\n  AND \"id\" > 10\n  AND \"created_at\" IS NULL\n\
             ORDER BY \"id\" ASC\nLIMIT 50 OFFSET 100"
        );
    }

    #[test]
    fn contains_works_on_columns_that_are_not_text() {
        let q = users();
        let f = filter("id", Op::Contains, "42");
        assert_eq!(
            q.predicate(Engine::Postgres, &f),
            "CAST(\"id\" AS TEXT) ILIKE '%42%' ESCAPE '!'"
        );
        assert_eq!(
            q.predicate(Engine::Mysql, &f),
            "CAST(`id` AS CHAR) LIKE '%42%' ESCAPE '!'"
        );
        assert_eq!(
            q.predicate(Engine::Mssql, &f),
            "CAST([id] AS NVARCHAR(MAX)) LIKE '%42%' ESCAPE '!'"
        );
        let d = filter("created_at", Op::StartsWith, "2024-01");
        assert_eq!(
            q.predicate(Engine::Mssql, &d),
            "CONVERT(NVARCHAR(MAX), [created_at], 121) LIKE '2024-01%' ESCAPE '!'"
        );
    }

    #[test]
    fn a_number_typed_into_a_text_column_stays_a_string() {
        let q = users();
        assert_eq!(
            q.predicate(Engine::Postgres, &filter("email", Op::Equals, "42")),
            "\"email\" = '42'"
        );
        assert_eq!(
            q.predicate(Engine::Postgres, &filter("id", Op::Equals, "42")),
            "\"id\" = 42"
        );
        assert_eq!(
            q.predicate(Engine::Postgres, &filter("id", Op::Equals, "x")),
            "\"id\" = 'x'"
        );
    }

    #[test]
    fn sorting_cycles_and_stacks() {
        let mut q = users();
        q.toggle_sort("email", false);
        assert_eq!(q.sort_of("email"), Some((0, Dir::Asc)));
        q.toggle_sort("email", false);
        assert_eq!(q.sort_of("email"), Some((0, Dir::Desc)));
        q.toggle_sort("created_at", true);
        assert!(
            q.sql(Engine::Sqlite)
                .contains("ORDER BY \"email\" DESC, \"created_at\" ASC")
        );
        q.toggle_sort("id", false);
        assert_eq!(q.sorts.len(), 1);
        q.toggle_sort("id", false);
        q.toggle_sort("id", false);
        assert!(q.sorts.is_empty());
    }

    #[test]
    fn duplicates_group_and_count() {
        let mut q = users();
        q.filters.push(filter("id", Op::Less, "1000"));
        q.mode = Mode::Groups {
            columns: vec!["email".to_string()],
            duplicates_only: true,
        };
        assert_eq!(
            q.sql(Engine::Mysql),
            "SELECT `email`, COUNT(*) AS `count`\nFROM `app`.`users`\nWHERE `id` < 1000\n\
             GROUP BY `email`\nHAVING COUNT(*) > 1\nORDER BY COUNT(*) DESC, `email`\nLIMIT 50 OFFSET 0"
        );
        assert_eq!(q.count_sql(Engine::Mysql), None);
    }

    #[test]
    fn counting_uses_the_same_filters() {
        let mut q = users();
        q.filters
            .push(filter("email", Op::EndsWith, "@example.com"));
        assert_eq!(
            q.count_sql(Engine::Sqlite).unwrap(),
            "SELECT COUNT(*)\nFROM \"app\".\"users\"\nWHERE \"email\" LIKE '%@example.com' ESCAPE '!'"
        );
    }

    #[test]
    fn long_column_lists_wrap() {
        let columns: Vec<ColumnInfo> = (0..40)
            .map(|i| col(&format!("column_number_{i}"), "int", false))
            .collect();
        let q = TableQuery::new(
            TableRef {
                schema: None,
                name: "wide".into(),
            },
            columns,
            10,
        );
        let sql = q.sql(Engine::Sqlite);
        assert!(sql.lines().all(|l| l.len() <= 110), "{sql}");
        assert!(sql.contains("\"column_number_39\"\nFROM"));
    }

    /// Whatever the builder produces has to get through the read-only guard,
    /// including with hostile values typed into a filter.
    #[test]
    fn everything_the_builder_writes_passes_the_guard() {
        let nasty = [
            "plain",
            "o'brien",
            "50%_off!",
            "back\\slash",
            "'; DROP TABLE users; --",
            "blåbær",
            "[x]",
            "\"q\"",
            "`q`",
            "",
        ];
        for engine in ALL {
            for value in nasty {
                for op in Op::ALL {
                    let mut q = users();
                    q.filters.push(filter("email", op, value));
                    q.filters.push(filter("id", op, value));
                    q.filters.push(filter("created_at", op, value));
                    q.toggle_sort("email", false);
                    for sql in [q.sql(engine), q.count_sql(engine).unwrap()] {
                        guard::check(engine, &sql)
                            .unwrap_or_else(|e| panic!("{engine:?} {op:?} {value:?}: {e}\n{sql}"));
                    }
                    q.mode = Mode::Groups {
                        columns: vec!["email".into(), "id".into()],
                        duplicates_only: true,
                    };
                    let sql = q.sql(engine);
                    guard::check(engine, &sql)
                        .unwrap_or_else(|e| panic!("{engine:?} groups {value:?}: {e}\n{sql}"));
                }
            }
        }
    }

    #[test]
    fn odd_identifiers_pass_the_guard_too() {
        for engine in ALL {
            let q = TableQuery::new(
                TableRef {
                    schema: Some("my schema".into()),
                    name: "order \"items\" [v2] `x`".into(),
                },
                vec![col("select", "int", true), col("a b]c\"d`e", "text", false)],
                10,
            );
            let sql = q.sql(engine);
            guard::check(engine, &sql).unwrap_or_else(|e| panic!("{engine:?}: {e}\n{sql}"));
        }
    }
}

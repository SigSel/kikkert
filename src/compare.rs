//! Comparing the columns of two tables: their names, types, nullability,
//! defaults and keys, never their rows. The tables may be in different
//! databases or on different engines, so a type or default that two engines
//! spell differently counts as alike rather than different.

use crate::db::{Engine, ResultSet};

/// One table's columns as its Columns tab lists them, held to compare other
/// tables against.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub connection: String,
    pub engine: Engine,
    pub table: String,
    /// What the catalog says about each column besides its name: type,
    /// nullable, default, key, and whatever else this engine reports.
    pub attributes: Vec<String>,
    pub columns: Vec<Column>,
}

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    /// One per attribute.
    pub values: Vec<Option<String>>,
}

impl Snapshot {
    /// Reads the result of a Columns tab ([`crate::db::meta::structure_tabs`]),
    /// whose first column is the name.
    pub fn new(connection: &str, engine: Engine, table: &str, result: &ResultSet) -> Snapshot {
        Snapshot {
            connection: connection.to_string(),
            engine,
            table: table.to_string(),
            // Some servers give catalog columns upper-case names.
            attributes: result
                .columns
                .iter()
                .skip(1)
                .map(|c| c.name.to_lowercase())
                .collect(),
            columns: result
                .rows
                .iter()
                .filter_map(|row| {
                    Some(Column {
                        name: row.first()?.clone()?,
                        values: row.iter().skip(1).cloned().collect(),
                    })
                })
                .collect(),
        }
    }

    /// Whether this engine says anything about `attribute`.
    pub fn reports(&self, attribute: &str) -> bool {
        self.attributes.iter().any(|a| a == attribute)
    }

    pub fn value(&self, column: usize, attribute: &str) -> Option<&str> {
        let at = self.attributes.iter().position(|a| a == attribute)?;
        self.columns.get(column)?.values.get(at)?.as_deref()
    }
}

/// How two values compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Same,
    /// Spelled differently but meaning the same, such as `int` and `integer`.
    Alike,
    Different,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Same,
    Alike,
    Different,
    OnlyLeft,
    OnlyRight,
}

impl Status {
    pub fn differs(self) -> bool {
        matches!(
            self,
            Status::Different | Status::OnlyLeft | Status::OnlyRight
        )
    }
}

/// One line of a comparison: a column found on both sides, or on one only.
#[derive(Clone, Debug)]
pub struct Row {
    pub left: Option<usize>,
    pub right: Option<usize>,
    /// Names are paired regardless of case, so they can still differ in it.
    pub name: Verdict,
    /// One per attribute of the comparison. `None` where a side does not
    /// report it, and for a column on one side only.
    pub cells: Vec<Option<Verdict>>,
}

impl Row {
    pub fn status(&self) -> Status {
        match (self.left, self.right) {
            (Some(_), None) => Status::OnlyLeft,
            (None, _) => Status::OnlyRight,
            (Some(_), Some(_)) => match self
                .cells
                .iter()
                .flatten()
                .fold(self.name, |a, b| a.max(*b))
            {
                Verdict::Same => Status::Same,
                Verdict::Alike => Status::Alike,
                Verdict::Different => Status::Different,
            },
        }
    }
}

pub struct Comparison {
    /// The table held for comparing.
    pub left: Snapshot,
    /// The table compared with it.
    pub right: Snapshot,
    /// Everything either side reports: the left's in its order, then the
    /// rest of the right's.
    pub attributes: Vec<String>,
    /// In the left's column order, with columns only on the right placed
    /// after the column they follow there.
    pub rows: Vec<Row>,
}

impl Comparison {
    pub fn new(left: Snapshot, right: Snapshot) -> Comparison {
        let mut attributes = left.attributes.clone();
        for attribute in &right.attributes {
            if !attributes.contains(attribute) {
                attributes.push(attribute.clone());
            }
        }

        // Pair the columns by name: exact matches first, then ones that only
        // differ in case.
        let mut partner: Vec<Option<usize>> = vec![None; left.columns.len()];
        let mut taken = vec![false; right.columns.len()];
        for (i, column) in left.columns.iter().enumerate() {
            if let Some(j) = right.columns.iter().position(|c| c.name == column.name) {
                partner[i] = Some(j);
                taken[j] = true;
            }
        }
        for (i, column) in left.columns.iter().enumerate() {
            if partner[i].is_some() {
                continue;
            }
            let name = column.name.to_lowercase();
            if let Some(j) = (0..right.columns.len())
                .find(|&j| !taken[j] && right.columns[j].name.to_lowercase() == name)
            {
                partner[i] = Some(j);
                taken[j] = true;
            }
        }

        let same_engine = left.engine == right.engine;
        let pair = |i: usize, j: usize| Row {
            left: Some(i),
            right: Some(j),
            name: if left.columns[i].name == right.columns[j].name {
                Verdict::Same
            } else {
                Verdict::Alike
            },
            cells: attributes
                .iter()
                .map(|a| {
                    (left.reports(a) && right.reports(a))
                        .then(|| judge(a, left.value(i, a), right.value(j, a), same_engine))
                })
                .collect(),
        };
        let one_side = |left_index: Option<usize>, right_index: Option<usize>| Row {
            left: left_index,
            right: right_index,
            name: Verdict::Different,
            cells: vec![None; attributes.len()],
        };

        let mut rows = Vec::new();
        // Right columns before this one have been placed.
        let mut next = 0;
        for (i, partner) in partner.iter().enumerate() {
            match *partner {
                Some(j) => {
                    while next < j {
                        if !taken[next] {
                            rows.push(one_side(None, Some(next)));
                        }
                        next += 1;
                    }
                    next = next.max(j + 1);
                    rows.push(pair(i, j));
                }
                None => rows.push(one_side(Some(i), None)),
            }
        }
        for (j, taken) in taken.iter().enumerate().skip(next) {
            if !taken {
                rows.push(one_side(None, Some(j)));
            }
        }

        Comparison {
            left,
            right,
            attributes,
            rows,
        }
    }

    pub fn count(&self, status: Status) -> usize {
        self.rows.iter().filter(|r| r.status() == status).count()
    }

    /// The values of `attribute` for `row`, on the left and on the right.
    pub fn values(&self, row: &Row, attribute: &str) -> (Option<&str>, Option<&str>) {
        (
            row.left.and_then(|i| self.left.value(i, attribute)),
            row.right.and_then(|j| self.right.value(j, attribute)),
        )
    }

    pub fn names(&self, row: &Row) -> (Option<&str>, Option<&str>) {
        (
            row.left.map(|i| self.left.columns[i].name.as_str()),
            row.right.map(|j| self.right.columns[j].name.as_str()),
        )
    }

    /// Short names for the two sides: the connections when they differ, else
    /// the tables, else it is one table read twice.
    pub fn labels(&self) -> (String, String) {
        let (left, right) = (&self.left, &self.right);
        if left.connection != right.connection {
            (left.connection.clone(), right.connection.clone())
        } else if left.table != right.table {
            (left.table.clone(), right.table.clone())
        } else {
            ("earlier".to_string(), "now".to_string())
        }
    }
}

fn judge(attribute: &str, left: Option<&str>, right: Option<&str>, same_engine: bool) -> Verdict {
    let left = left.map(str::trim).filter(|v| !v.is_empty());
    let right = right.map(str::trim).filter(|v| !v.is_empty());
    // Type names mean the same in any case; SQLite hands back TEXT for text.
    let same = match attribute {
        "type" => left.map(str::to_lowercase) == right.map(str::to_lowercase),
        _ => left == right,
    };
    if same {
        return Verdict::Same;
    }
    let alike = match attribute {
        "type" => left.map(canonical_type) == right.map(canonical_type),
        "default" => canonical_default(left) == canonical_default(right),
        "extra" => canonical_extra(left) == canonical_extra(right),
        // MySQL also marks unique and indexed columns here, which the other
        // engines leave out.
        "key" => !same_engine && (left == Some("PK")) == (right == Some("PK")),
        _ => left.map(str::to_lowercase) == right.map(str::to_lowercase),
    };
    if alike {
        Verdict::Alike
    } else {
        Verdict::Different
    }
}

/// A type name with the differences in spelling between engines taken out:
/// `int`, `int4` and `integer` are all `integer`.
fn canonical_type(type_name: &str) -> String {
    let lower = type_name.trim().to_lowercase();
    // The arguments, wherever the name puts them: Postgres writes
    // timestamp(3) without time zone.
    let (words, args) = match (lower.find('('), lower.find(')')) {
        (Some(open), Some(close)) if open < close => (
            format!("{} {}", &lower[..open], &lower[close + 1..]),
            lower[open + 1..close].replace(' ', ""),
        ),
        _ => (lower.clone(), String::new()),
    };
    let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
    let words = match type_synonym(&words) {
        Some(name) => name.to_string(),
        // As in "int unsigned".
        None => match words.split_once(' ') {
            Some((first, rest)) => format!("{} {rest}", type_synonym(first).unwrap_or(first)),
            None => words,
        },
    };
    // MySQL writes boolean as tinyint(1).
    if words == "tinyint" && args == "1" {
        return "boolean".to_string();
    }
    // The display width in MySQL's int(11) changes nothing about the values.
    let integer = ["tinyint", "smallint", "mediumint", "integer", "bigint"]
        .iter()
        .any(|t| words.split(' ').next() == Some(*t));
    if args.is_empty() || integer {
        words
    } else {
        format!("{words}({args})")
    }
}

fn type_synonym(words: &str) -> Option<&'static str> {
    Some(match words {
        "int" | "int4" => "integer",
        "int8" => "bigint",
        "int2" => "smallint",
        "character varying" => "varchar",
        "character" | "bpchar" => "char",
        "decimal" | "dec" => "numeric",
        "double precision" | "float8" => "double",
        "float4" => "real",
        "bool" => "boolean",
        // SQL Server's boolean. Elsewhere bit takes a length, which keeps it
        // apart.
        "bit" => "boolean",
        "timestamp without time zone" | "datetime" | "datetime2" => "timestamp",
        "timestamp with time zone" | "datetimeoffset" => "timestamptz",
        "time without time zone" => "time",
        "time with time zone" => "timetz",
        "uniqueidentifier" => "uuid",
        _ => return None,
    })
}

/// A default with what engines wrap around it taken off: SQL Server's
/// parentheses, Postgres's casts, the quotes around a string. `None` is no
/// default.
fn canonical_default(value: Option<&str>) -> Option<String> {
    let mut v = value?.trim();
    loop {
        let before = v;
        if let Some(inner) = v.strip_prefix('(').and_then(|v| v.strip_suffix(')'))
            && balanced(inner)
        {
            v = inner.trim();
        }
        if let Some((value, cast)) = v.rsplit_once("::")
            && balanced(value)
            && cast
                .chars()
                .all(|c| c.is_alphanumeric() || " _(),[]\"".contains(c))
        {
            v = value.trim();
        }
        if v == before {
            break;
        }
    }
    let v = v
        .strip_prefix('N')
        .filter(|v| v.starts_with('\''))
        .unwrap_or(v);
    let v = match v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        Some(text) => text.replace("''", "'"),
        // MySQL gives a string default without its quotes.
        None => v.to_string(),
    }
    .to_lowercase();
    Some(match v.as_str() {
        "null" => return None,
        "true" | "b'1'" => "1".to_string(),
        "false" | "b'0'" => "0".to_string(),
        "now()"
        | "current_timestamp"
        | "current_timestamp()"
        | "getdate()"
        | "sysdatetime()"
        | "localtimestamp"
        | "localtimestamp()" => "current_timestamp".to_string(),
        _ => v,
    })
}

fn balanced(text: &str) -> bool {
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0
}

/// MySQL's and SQL Server's notes on a column. MySQL's DEFAULT_GENERATED
/// only says the default is an expression, which the default itself shows.
fn canonical_extra(value: Option<&str>) -> String {
    value
        .unwrap_or("")
        .to_lowercase()
        .replace("default_generated", "")
        .replace("auto_increment", "identity")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ResultColumn;

    type Spec<'a> = (&'a str, &'a str, &'a str, Option<&'a str>, &'a str);

    fn snapshot(connection: &str, engine: Engine, columns: &[Spec]) -> Snapshot {
        Snapshot {
            connection: connection.into(),
            engine,
            table: "shop.customers".into(),
            attributes: ["type", "nullable", "default", "key"]
                .map(String::from)
                .to_vec(),
            columns: columns
                .iter()
                .map(|(name, type_name, nullable, default, key)| Column {
                    name: name.to_string(),
                    values: vec![
                        Some(type_name.to_string()),
                        Some(nullable.to_string()),
                        default.map(String::from),
                        Some(key.to_string()),
                    ],
                })
                .collect(),
        }
    }

    fn plain(names: &[&str]) -> Snapshot {
        let specs: Vec<Spec> = names
            .iter()
            .map(|n| (*n, "text", "YES", None, ""))
            .collect();
        snapshot("a", Engine::Postgres, &specs)
    }

    fn names(comparison: &Comparison) -> Vec<(String, Status)> {
        comparison
            .rows
            .iter()
            .map(|row| {
                let (left, right) = comparison.names(row);
                (left.or(right).unwrap().to_string(), row.status())
            })
            .collect()
    }

    fn type_of(left: &str, right: &str) -> Verdict {
        judge("type", Some(left), Some(right), false)
    }

    fn default_of(left: Option<&str>, right: Option<&str>) -> Verdict {
        judge("default", left, right, false)
    }

    #[test]
    fn a_table_matches_itself() {
        let table = snapshot(
            "a",
            Engine::Postgres,
            &[
                ("id", "integer", "NO", None, "PK"),
                (
                    "email",
                    "character varying(255)",
                    "YES",
                    Some("'x'::character varying"),
                    "",
                ),
            ],
        );
        let comparison = Comparison::new(table.clone(), table);
        assert_eq!(comparison.count(Status::Same), 2);
        assert!(
            comparison
                .rows
                .iter()
                .all(|r| r.cells.iter().all(|c| *c == Some(Verdict::Same)))
        );
    }

    #[test]
    fn added_and_removed_columns_keep_their_place() {
        let comparison = Comparison::new(
            plain(&["id", "email", "gone", "name"]),
            plain(&["id", "added", "email", "name", "last"]),
        );
        assert_eq!(
            names(&comparison),
            [
                ("id".to_string(), Status::Same),
                ("added".to_string(), Status::OnlyRight),
                ("email".to_string(), Status::Same),
                ("gone".to_string(), Status::OnlyLeft),
                ("name".to_string(), Status::Same),
                ("last".to_string(), Status::OnlyRight),
            ]
        );
    }

    #[test]
    fn reordered_columns_are_still_paired() {
        let comparison = Comparison::new(plain(&["a", "b", "c"]), plain(&["c", "x", "b", "a"]));
        let statuses: Vec<Status> = comparison.rows.iter().map(Row::status).collect();
        assert_eq!(comparison.rows.len(), 4);
        assert_eq!(comparison.count(Status::Same), 3);
        assert_eq!(
            statuses.iter().filter(|s| **s == Status::OnlyRight).count(),
            1
        );
    }

    #[test]
    fn names_that_differ_only_in_case_are_paired() {
        let comparison = Comparison::new(
            plain(&["CustomerId", "id", "ID"]),
            plain(&["customerid", "ID"]),
        );
        assert_eq!(
            names(&comparison),
            [
                ("CustomerId".to_string(), Status::Alike),
                ("id".to_string(), Status::OnlyLeft),
                ("ID".to_string(), Status::Same),
            ],
            "an exact match wins over one that differs in case"
        );
    }

    #[test]
    fn differences_are_found_attribute_by_attribute() {
        let comparison = Comparison::new(
            snapshot(
                "prod",
                Engine::Postgres,
                &[("total", "numeric(10,2)", "NO", Some("0"), "")],
            ),
            snapshot(
                "staging",
                Engine::Postgres,
                &[("total", "numeric(12,2)", "YES", Some("0"), "")],
            ),
        );
        let row = &comparison.rows[0];
        assert_eq!(row.status(), Status::Different);
        assert_eq!(
            row.cells,
            [
                Some(Verdict::Different),
                Some(Verdict::Different),
                Some(Verdict::Same),
                Some(Verdict::Same)
            ]
        );
        assert_eq!(
            comparison.values(row, "type"),
            (Some("numeric(10,2)"), Some("numeric(12,2)"))
        );
        assert_eq!(
            comparison.labels(),
            ("prod".to_string(), "staging".to_string())
        );
    }

    #[test]
    fn what_only_one_engine_reports_is_not_a_difference() {
        let mut left = plain(&["id"]);
        left.attributes.push("comment".into());
        left.columns[0].values.push(Some("the key".into()));
        let comparison = Comparison::new(left, plain(&["id"]));
        assert_eq!(
            comparison.attributes,
            ["type", "nullable", "default", "key", "comment"]
        );
        assert_eq!(comparison.rows[0].cells[4], None);
        assert_eq!(comparison.rows[0].status(), Status::Same);
    }

    #[test]
    fn types_spelled_differently_are_alike() {
        for (left, right) in [
            ("integer", "int"),
            ("int4", "integer"),
            ("character varying(255)", "varchar(255)"),
            ("numeric(10,2)", "decimal(10, 2)"),
            ("timestamp without time zone", "datetime"),
            ("timestamp without time zone", "datetime2"),
            ("timestamp(3) without time zone", "datetime(3)"),
            ("timestamp with time zone", "datetimeoffset"),
            ("boolean", "tinyint(1)"),
            ("boolean", "bit"),
            ("int(11)", "int"),
            ("int(10) unsigned", "int unsigned"),
            ("uuid", "uniqueidentifier"),
            ("double precision", "double"),
        ] {
            assert_eq!(type_of(left, right), Verdict::Alike, "{left} / {right}");
        }
        for (left, right) in [
            ("varchar(255)", "varchar(100)"),
            ("integer", "bigint"),
            ("int unsigned", "int"),
            ("text", "varchar(255)"),
            ("varchar(255)", "nvarchar(255)"),
            ("jsonb", "json"),
            ("timestamp with time zone", "timestamp without time zone"),
            ("numeric(10,2)", "numeric(12,2)"),
            ("bit(1)", "bit"),
            ("uuid", "char(36)"),
        ] {
            assert_eq!(type_of(left, right), Verdict::Different, "{left} / {right}");
        }
        assert_eq!(type_of("text", " text "), Verdict::Same);
        assert_eq!(type_of("TEXT", "text"), Verdict::Same);
    }

    #[test]
    fn defaults_spelled_differently_are_alike() {
        for (left, right) in [
            (Some("true"), Some("1")),
            (Some("((1))"), Some("1")),
            (Some("'x'::character varying"), Some("x")),
            (Some("('x')"), Some("'x'")),
            (Some("N'blå'"), Some("'blå'")),
            (Some("now()"), Some("CURRENT_TIMESTAMP")),
            (Some("(getdate())"), Some("now()")),
            (Some("'{}'::jsonb"), Some("'{}'")),
            (None, Some("NULL")),
        ] {
            assert_eq!(
                default_of(left, right),
                Verdict::Alike,
                "{left:?} / {right:?}"
            );
        }
        for (left, right) in [
            (Some("nextval('t_id_seq'::regclass)"), None),
            (Some("0"), Some("1")),
            (Some("'a'"), Some("'b'")),
            (Some("(1) + (2)"), Some("1) + (2")),
        ] {
            assert_eq!(
                default_of(left, right),
                Verdict::Different,
                "{left:?} / {right:?}"
            );
        }
        assert_eq!(
            canonical_default(Some("nextval('t_id_seq'::regclass)")).as_deref(),
            Some("nextval('t_id_seq'::regclass)"),
            "a cast inside a call stays"
        );
        assert_eq!(default_of(None, Some("")), Verdict::Same);
    }

    #[test]
    fn keys_and_extras() {
        assert_eq!(
            judge("key", Some("PK"), Some(""), false),
            Verdict::Different
        );
        assert_eq!(judge("key", Some("MUL"), Some(""), false), Verdict::Alike);
        assert_eq!(
            judge("key", Some("MUL"), Some(""), true),
            Verdict::Different,
            "on the same engine an index is a difference"
        );
        assert_eq!(
            judge("extra", Some("auto_increment"), Some("identity"), false),
            Verdict::Alike
        );
        assert_eq!(
            judge("extra", Some("DEFAULT_GENERATED"), Some(""), false),
            Verdict::Alike
        );
        assert_eq!(
            judge(
                "extra",
                Some("DEFAULT_GENERATED on update CURRENT_TIMESTAMP"),
                None,
                false
            ),
            Verdict::Different
        );
        assert_eq!(
            judge("comment", Some("Key"), Some("key"), false),
            Verdict::Alike
        );
    }

    #[test]
    fn sides_are_named_by_what_tells_them_apart() {
        let mut other = plain(&["id"]);
        other.table = "shop.clients".into();
        let comparison = Comparison::new(plain(&["id"]), other);
        assert_eq!(
            comparison.labels(),
            ("shop.customers".to_string(), "shop.clients".to_string())
        );
        let comparison = Comparison::new(plain(&["id"]), plain(&["id"]));
        assert_eq!(
            comparison.labels(),
            ("earlier".to_string(), "now".to_string())
        );
    }

    #[test]
    fn a_snapshot_reads_a_columns_tab() {
        let column = |name: &str| ResultColumn {
            name: name.into(),
            type_name: String::new(),
            numeric: false,
        };
        let result = ResultSet {
            columns: ["name", "type", "nullable", "default", "key", "EXTRA"]
                .map(column)
                .to_vec(),
            rows: vec![
                vec![
                    Some("id".into()),
                    Some("int".into()),
                    Some("NO".into()),
                    None,
                    Some("PK".into()),
                    Some("auto_increment".into()),
                ],
                vec![None, None, None, None, None, None],
            ],
            ..ResultSet::default()
        };
        let snapshot = Snapshot::new("demo", Engine::Mysql, "shop.customers", &result);
        assert_eq!(
            snapshot.attributes,
            ["type", "nullable", "default", "key", "extra"]
        );
        assert_eq!(
            snapshot.columns.len(),
            1,
            "a row without a name is left out"
        );
        assert_eq!(snapshot.value(0, "extra"), Some("auto_increment"));
        assert_eq!(snapshot.value(0, "default"), None);
        assert_eq!(snapshot.value(0, "comment"), None);
        assert!(!snapshot.reports("comment"));
    }
}

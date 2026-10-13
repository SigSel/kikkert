//! The small differences between SQL dialects that the query builder needs:
//! quoting, literals, pagination and how to treat a column as text.

use super::Engine;

/// A coarse view of a column's type, enough to decide how to compare it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeClass {
    Numeric,
    Text,
    Temporal,
    Other,
}

/// Classifies a type name as reported by a catalog or a driver.
pub fn classify(type_name: &str) -> TypeClass {
    let t = type_name.trim().to_ascii_lowercase();
    let word: String = t
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();

    const NUMERIC: &[&str] = &[
        "int",
        "bigint",
        "smallint",
        "tinyint",
        "mediumint",
        "integer",
        "serial",
        "bigserial",
        "smallserial",
        "numeric",
        "decimal",
        "dec",
        "float",
        "double",
        "real",
        "money",
        "smallmoney",
        "number",
        "oid",
    ];
    if word != "interval"
        && (NUMERIC.contains(&word.as_str())
            || word.starts_with("int")
            || word.starts_with("float")
            || word.starts_with("uint"))
    {
        return TypeClass::Numeric;
    }
    if t.contains("char")
        || t.contains("text")
        || t.contains("clob")
        || t.starts_with("enum")
        || t.starts_with("set(")
        || matches!(word.as_str(), "citext" | "name" | "string" | "sysname")
    {
        return TypeClass::Text;
    }
    if t.contains("date") || t.contains("time") {
        return TypeClass::Temporal;
    }
    TypeClass::Other
}

/// True for values that can be written into SQL as a bare number.
pub fn looks_numeric(value: &str) -> bool {
    let v = value.trim();
    let digits = v.strip_prefix(['-', '+']).unwrap_or(v);
    if digits.is_empty() {
        return false;
    }
    let (mantissa, exponent) = match digits.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (digits, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all_digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() && fraction.is_empty() {
        return false;
    }
    if !all_digits(whole) || !all_digits(fraction) {
        return false;
    }
    match exponent {
        None => true,
        Some(e) => {
            let e = e.strip_prefix(['-', '+']).unwrap_or(e);
            !e.is_empty() && all_digits(e)
        }
    }
}

impl Engine {
    /// Quotes an identifier so any table or column name is safe to embed.
    pub fn quote_ident(self, name: &str) -> String {
        match self {
            Engine::Postgres | Engine::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
            Engine::Mysql => format!("`{}`", name.replace('`', "``")),
            Engine::Mssql => format!("[{}]", name.replace(']', "]]")),
        }
    }

    /// `schema.table`, quoted.
    pub fn qualify(self, schema: Option<&str>, name: &str) -> String {
        match schema {
            Some(s) if !s.is_empty() => {
                format!("{}.{}", self.quote_ident(s), self.quote_ident(name))
            }
            _ => self.quote_ident(name),
        }
    }

    /// Quotes a string literal. Values come from the keyboard and are shown
    /// to the user as part of the generated SQL, so they are embedded rather
    /// than bound.
    pub fn quote_literal(self, value: &str) -> String {
        // No engine accepts a NUL inside a literal.
        let value: String = value.chars().filter(|c| *c != '\0').collect();
        let doubled = value.replace('\'', "''");
        match self {
            // E'' strings behave the same whatever standard_conforming_strings is.
            Engine::Postgres if value.contains('\\') => {
                format!("E'{}'", doubled.replace('\\', "\\\\"))
            }
            Engine::Postgres | Engine::Sqlite => format!("'{doubled}'"),
            // Doubling backslashes is correct with and without NO_BACKSLASH_ESCAPES
            // as far as staying inside the literal goes.
            Engine::Mysql => format!("'{}'", doubled.replace('\\', "\\\\")),
            Engine::Mssql if !value.is_ascii() => format!("N'{doubled}'"),
            Engine::Mssql => format!("'{doubled}'"),
        }
    }

    /// A literal for comparing against a column of the given class: numbers
    /// stay bare when both the column and the value are numeric.
    pub fn value_literal(self, class: TypeClass, value: &str) -> String {
        let bare = match class {
            TypeClass::Numeric => true,
            // SQLite columns often have no declared type at all.
            TypeClass::Other => self == Engine::Sqlite,
            TypeClass::Text | TypeClass::Temporal => false,
        };
        if bare && looks_numeric(value) {
            value.trim().to_string()
        } else {
            self.quote_literal(value)
        }
    }

    /// A `LIKE` right-hand side matching `value` literally, with the given
    /// wildcards around it. `!` is the escape character because, unlike a
    /// backslash, it means nothing special inside any dialect's literals.
    pub fn like_literal(self, prefix: &str, value: &str, suffix: &str) -> String {
        let mut escaped = String::with_capacity(value.len() + 2);
        for c in value.chars() {
            let special = matches!(c, '!' | '%' | '_') || (self == Engine::Mssql && c == '[');
            if special {
                escaped.push('!');
            }
            escaped.push(c);
        }
        format!(
            "{} ESCAPE '!'",
            self.quote_literal(&format!("{prefix}{escaped}{suffix}"))
        )
    }

    /// The case-insensitive pattern match operator.
    pub fn like_keyword(self) -> &'static str {
        match self {
            Engine::Postgres => "ILIKE",
            _ => "LIKE",
        }
    }

    /// An expression that renders any column as text, so "contains" works on
    /// numbers, dates and ids too.
    pub fn as_text(self, quoted_ident: &str, class: TypeClass) -> String {
        if class == TypeClass::Text {
            return quoted_ident.to_string();
        }
        match self {
            Engine::Postgres | Engine::Sqlite => format!("CAST({quoted_ident} AS TEXT)"),
            Engine::Mysql => format!("CAST({quoted_ident} AS CHAR)"),
            // Style 121 gives yyyy-mm-dd hh:mi:ss, matching how dates are shown.
            Engine::Mssql if class == TypeClass::Temporal => {
                format!("CONVERT(NVARCHAR(MAX), {quoted_ident}, 121)")
            }
            Engine::Mssql => format!("CAST({quoted_ident} AS NVARCHAR(MAX))"),
        }
    }

    /// The clause that limits a query to one page.
    pub fn page_clause(self, limit: usize, offset: usize) -> String {
        match self {
            Engine::Mssql => format!("OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"),
            _ => format!("LIMIT {limit} OFFSET {offset}"),
        }
    }

    /// SQL Server only pages an ordered query, so it needs a placeholder
    /// ordering when the user has not asked for one.
    pub fn requires_order_for_paging(self) -> bool {
        self == Engine::Mssql
    }

    pub fn count_star(self) -> &'static str {
        match self {
            Engine::Mssql => "COUNT_BIG(*)",
            _ => "COUNT(*)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_escaped() {
        assert_eq!(Engine::Postgres.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(Engine::Mysql.quote_ident("a`b"), "`a``b`");
        assert_eq!(Engine::Mssql.quote_ident("a]b"), "[a]]b]");
        assert_eq!(Engine::Mssql.qualify(Some("dbo"), "t"), "[dbo].[t]");
        assert_eq!(Engine::Sqlite.qualify(None, "t"), "\"t\"");
    }

    #[test]
    fn literals_cannot_break_out() {
        assert_eq!(Engine::Sqlite.quote_literal("o'brien"), "'o''brien'");
        assert_eq!(Engine::Postgres.quote_literal("a\\'b"), "E'a\\\\''b'");
        assert_eq!(Engine::Mysql.quote_literal("a\\'b"), "'a\\\\''b'");
        assert_eq!(Engine::Mssql.quote_literal("blåbær"), "N'blåbær'");
        assert_eq!(Engine::Postgres.quote_literal("a\0b"), "'ab'");
    }

    #[test]
    fn like_wildcards_are_literal() {
        assert_eq!(
            Engine::Postgres.like_literal("%", "50%_off!", "%"),
            "'%50!%!_off!!%' ESCAPE '!'"
        );
        assert_eq!(
            Engine::Mssql.like_literal("", "[x]", "%"),
            "'![x]%' ESCAPE '!'"
        );
    }

    #[test]
    fn type_classes() {
        for t in [
            "int4",
            "INT",
            "bigint unsigned",
            "numeric(10,2)",
            "decimal",
            "float8",
            "money",
        ] {
            assert_eq!(classify(t), TypeClass::Numeric, "{t}");
        }
        for t in [
            "varchar(255)",
            "character varying",
            "TEXT",
            "nvarchar",
            "enum('a','b')",
            "citext",
        ] {
            assert_eq!(classify(t), TypeClass::Text, "{t}");
        }
        for t in [
            "timestamp with time zone",
            "datetime2",
            "date",
            "TIMESTAMPTZ",
        ] {
            assert_eq!(classify(t), TypeClass::Temporal, "{t}");
        }
        for t in ["interval", "point", "uuid", "jsonb", "bytea", "", "boolean"] {
            assert_ne!(classify(t), TypeClass::Numeric, "{t}");
            assert_ne!(classify(t), TypeClass::Text, "{t}");
        }
    }

    #[test]
    fn numeric_detection() {
        for v in ["1", "-1", "+3.5", ".5", "5.", "1e9", "1.5E-3", " 42 "] {
            assert!(looks_numeric(v), "{v}");
        }
        for v in [
            "",
            "-",
            ".",
            "1e",
            "1,5",
            "0x10",
            "1 2",
            "abc",
            "1; DROP TABLE x",
        ] {
            assert!(!looks_numeric(v), "{v}");
        }
    }

    #[test]
    fn numbers_stay_bare_only_for_numeric_columns() {
        assert_eq!(
            Engine::Postgres.value_literal(TypeClass::Numeric, "42"),
            "42"
        );
        assert_eq!(
            Engine::Postgres.value_literal(TypeClass::Text, "42"),
            "'42'"
        );
        assert_eq!(
            Engine::Postgres.value_literal(TypeClass::Numeric, "4 2"),
            "'4 2'"
        );
        assert_eq!(Engine::Sqlite.value_literal(TypeClass::Other, "42"), "42");
        assert_eq!(Engine::Mssql.value_literal(TypeClass::Other, "42"), "'42'");
    }
}

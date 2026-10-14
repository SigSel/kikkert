//! Syntax colouring for SQL and JSON.
//!
//! A small lexer rather than sqlparser's tokenizer: the editor shows
//! half-typed statements, and colouring only needs to tell keywords, names,
//! strings, numbers and comments apart. Strings and comments may run over
//! several lines; one that is not finished runs to the end of the text.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Plain,
    Keyword,
    Function,
    /// A quoted identifier.
    Name,
    /// A JSON object key.
    Key,
    String,
    Number,
    Comment,
    Punct,
}

fn style(kind: Kind) -> Style {
    let t = theme();
    match kind {
        Kind::Plain | Kind::Name => Style::new(),
        Kind::Keyword => Style::new().fg(t.keyword),
        Kind::Function => Style::new().fg(t.function),
        Kind::Key => Style::new().fg(t.accent),
        Kind::String => Style::new().fg(t.string),
        Kind::Number => Style::new().fg(t.number),
        Kind::Comment => Style::new().fg(t.muted).add_modifier(Modifier::ITALIC),
        Kind::Punct => Style::new().fg(t.subtle),
    }
}

/// One coloured line per line of `text`.
pub fn sql(text: &str) -> Vec<Line<'static>> {
    lines(sql_tokens(text))
}

/// One coloured line per line of `text`, which should be JSON.
pub fn json(text: &str) -> Vec<Line<'static>> {
    lines(json_tokens(text))
}

fn lines(tokens: Vec<Vec<(Kind, &str)>>) -> Vec<Line<'static>> {
    tokens
        .into_iter()
        .map(|line| {
            Line::from(
                line.into_iter()
                    .map(|(kind, piece)| Span::styled(piece.to_string(), style(kind)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// Cuts `text` into pieces with `next`, which says what the text starts with
/// and how many bytes of it that is, and sorts the pieces into lines.
fn tokens(text: &str, next: impl Fn(&str) -> (Kind, usize)) -> Vec<Vec<(Kind, &str)>> {
    let mut lines = vec![Vec::new()];
    let mut at = 0;
    while at < text.len() {
        let (kind, len) = next(&text[at..]);
        for (i, part) in text[at..at + len].split('\n').enumerate() {
            if i > 0 {
                lines.push(Vec::new());
            }
            if !part.is_empty() {
                lines.last_mut().expect("never empty").push((kind, part));
            }
        }
        at += len;
    }
    lines
}

const KEYWORDS: &str = "ALL ALTER AND ANALYZE ANY APPLY AS ASC BETWEEN BIGINT BY CASE CAST CHAR CREATE CROSS \
     DATE DECIMAL DELETE DESC DESCRIBE DISTINCT DROP ELSE END ESCAPE EXCEPT EXISTS EXPLAIN \
     FALSE FETCH FILTER FIRST FOR FROM FULL GROUP HAVING ILIKE IN INNER INSERT INT INTEGER \
     INTERSECT INTERVAL INTO IS JOIN LAST LATERAL LEFT LIKE LIMIT NATURAL NEXT NOT NULL NULLS \
     NUMERIC NVARCHAR OFFSET ON ONLY OR ORDER OUTER OVER PARTITION RECURSIVE RIGHT ROW ROWS \
     SELECT SET SHOW SIMILAR SOME TABLE TEXT THEN TIMESTAMP TOP TRUE UNION UPDATE USING \
     VALUES VARCHAR VIEW WHEN WHERE WINDOW WITH WITHIN";

fn sql_tokens(text: &str) -> Vec<Vec<(Kind, &str)>> {
    tokens(text, |rest| {
        let c = rest.chars().next().expect("not empty");
        let after = &rest[c.len_utf8()..];
        if rest.starts_with("--") {
            (Kind::Comment, rest.find('\n').unwrap_or(rest.len()))
        } else if let Some(comment) = rest.strip_prefix("/*") {
            let end = comment.find("*/").map_or(rest.len(), |end| end + 4);
            (Kind::Comment, end)
        } else if c == '\'' {
            (Kind::String, quoted(rest, '\''))
        } else if c == '"' || c == '`' {
            (Kind::Name, quoted(rest, c))
        } else if c == '[' && after.starts_with(|c: char| !c.is_ascii_digit()) {
            // SQL Server's [name]; [1] is more likely an array subscript.
            (Kind::Name, quoted(rest, ']'))
        } else if c.is_ascii_digit()
            || (c == '.' && after.starts_with(|c: char| c.is_ascii_digit()))
        {
            (Kind::Number, number(rest))
        } else if c.is_alphabetic() || c == '_' {
            let len = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(rest.len());
            let word = &rest[..len];
            if len == 1 && after.starts_with('\'') && "EeNnXx".contains(word) {
                // E'…', N'…' and X'…' are strings with a prefix.
                (Kind::String, 1 + quoted(after, '\''))
            } else if KEYWORDS
                .split_ascii_whitespace()
                .any(|k| k.eq_ignore_ascii_case(word))
            {
                (Kind::Keyword, len)
            } else if rest[len..].starts_with('(') {
                (Kind::Function, len)
            } else {
                (Kind::Plain, len)
            }
        } else if c.is_whitespace() {
            let len = rest
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(rest.len());
            (Kind::Plain, len)
        } else {
            (Kind::Punct, c.len_utf8())
        }
    })
}

fn json_tokens(text: &str) -> Vec<Vec<(Kind, &str)>> {
    tokens(text, |rest| {
        let c = rest.chars().next().expect("not empty");
        let after = &rest[c.len_utf8()..];
        if c == '"' {
            let len = json_string(rest);
            let kind = if rest[len..].trim_start().starts_with(':') {
                Kind::Key
            } else {
                Kind::String
            };
            (kind, len)
        } else if c.is_ascii_digit()
            || (c == '-' && after.starts_with(|c: char| c.is_ascii_digit()))
        {
            (Kind::Number, 1 + number(after))
        } else if c.is_alphabetic() {
            let len = rest
                .find(|c: char| !c.is_alphanumeric())
                .unwrap_or(rest.len());
            let kind = match &rest[..len] {
                "true" | "false" | "null" => Kind::Keyword,
                _ => Kind::Plain,
            };
            (kind, len)
        } else if c.is_whitespace() {
            let len = rest
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(rest.len());
            (Kind::Plain, len)
        } else {
            (Kind::Punct, c.len_utf8())
        }
    })
}

/// The length of a quoted run that starts `text`, up to and including
/// `close`. A doubled `close` stands for itself.
fn quoted(text: &str, close: char) -> usize {
    let mut chars = text.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        if c == close {
            if chars.peek().is_some_and(|&(_, next)| next == close) {
                chars.next();
                continue;
            }
            return i + c.len_utf8();
        }
    }
    text.len()
}

/// The length of the JSON string that starts `text`, quotes included.
fn json_string(text: &str) -> usize {
    let mut chars = text.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return i + 1,
            _ => {}
        }
    }
    text.len()
}

/// The length of the number that starts `text`: digits, a decimal point, an
/// exponent.
fn number(text: &str) -> usize {
    let bytes = text.as_bytes();
    let (mut len, mut dot, mut exponent) = (0, false, false);
    while len < bytes.len() {
        match bytes[len] {
            b'0'..=b'9' => {}
            b'.' if !dot && !exponent => dot = true,
            b'e' | b'E' if !exponent && len > 0 => {
                exponent = true;
                if matches!(bytes.get(len + 1), Some(b'+' | b'-')) {
                    len += 1;
                }
            }
            _ => break,
        }
        len += 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The non-blank pieces of each line, as (kind, text).
    fn pieces(tokens: Vec<Vec<(Kind, &str)>>) -> Vec<Vec<(Kind, &str)>> {
        tokens
            .into_iter()
            .map(|line| {
                line.into_iter()
                    .filter(|(_, piece)| !piece.trim().is_empty())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn generated_sql() {
        use Kind::*;
        let sql = "SELECT \"id\", COUNT(*) AS [count]\nFROM `app`.\"users\"\n\
                   WHERE \"email\" ILIKE E'%o''neil%' AND n >= 1.5e3\nLIMIT 50";
        assert_eq!(
            pieces(sql_tokens(sql)),
            vec![
                vec![
                    (Keyword, "SELECT"),
                    (Name, "\"id\""),
                    (Punct, ","),
                    (Function, "COUNT"),
                    (Punct, "("),
                    (Punct, "*"),
                    (Punct, ")"),
                    (Keyword, "AS"),
                    (Name, "[count]"),
                ],
                vec![
                    (Keyword, "FROM"),
                    (Name, "`app`"),
                    (Punct, "."),
                    (Name, "\"users\""),
                ],
                vec![
                    (Keyword, "WHERE"),
                    (Name, "\"email\""),
                    (Keyword, "ILIKE"),
                    (String, "E'%o''neil%'"),
                    (Keyword, "AND"),
                    (Plain, "n"),
                    (Punct, ">"),
                    (Punct, "="),
                    (Number, "1.5e3"),
                ],
                vec![(Keyword, "LIMIT"), (Number, "50")],
            ]
        );
    }

    #[test]
    fn keywords_are_found_in_any_case_but_not_inside_names() {
        use Kind::*;
        assert_eq!(
            pieces(sql_tokens("select from_date, t1 from x")),
            vec![vec![
                (Keyword, "select"),
                (Plain, "from_date"),
                (Punct, ","),
                (Plain, "t1"),
                (Keyword, "from"),
                (Plain, "x"),
            ]]
        );
    }

    #[test]
    fn strings_and_comments_run_over_lines() {
        use Kind::*;
        assert_eq!(
            pieces(sql_tokens("/* a\nb */ SELECT 'x\ny' -- end\n1")),
            vec![
                vec![(Comment, "/* a")],
                vec![(Comment, "b */"), (Keyword, "SELECT"), (String, "'x"),],
                vec![(String, "y'"), (Comment, "-- end")],
                vec![(Number, "1")],
            ]
        );
    }

    #[test]
    fn unfinished_text_runs_to_the_end() {
        use Kind::*;
        assert_eq!(
            pieces(sql_tokens("SELECT 'abc")),
            vec![vec![(Keyword, "SELECT"), (String, "'abc")]]
        );
        assert_eq!(pieces(sql_tokens("\"na")), vec![vec![(Name, "\"na")]]);
        assert_eq!(pieces(sql_tokens("/* x")), vec![vec![(Comment, "/* x")]]);
        assert_eq!(
            pieces(sql_tokens("a[")),
            vec![vec![(Plain, "a"), (Punct, "[")]]
        );
    }

    #[test]
    fn every_line_is_kept() {
        assert_eq!(sql("").len(), 1);
        assert_eq!(sql("a\n\nb\n").len(), 4);
        assert_eq!(json("{\n}").len(), 2);
    }

    #[test]
    fn json_keys_and_values() {
        use Kind::*;
        let text = "{\n  \"a\\\"b\": [-1.5, true, null],\n  \"c\": \"d:e\"\n}";
        assert_eq!(
            pieces(json_tokens(text)),
            vec![
                vec![(Punct, "{")],
                vec![
                    (Key, "\"a\\\"b\""),
                    (Punct, ":"),
                    (Punct, "["),
                    (Number, "-1.5"),
                    (Punct, ","),
                    (Keyword, "true"),
                    (Punct, ","),
                    (Keyword, "null"),
                    (Punct, "]"),
                    (Punct, ","),
                ],
                vec![(Key, "\"c\""), (Punct, ":"), (String, "\"d:e\"")],
                vec![(Punct, "}")],
            ]
        );
    }
}

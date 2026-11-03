//! Comparing two rows of a result, column by column: which columns hold
//! different values, and where two values that differ part ways.

use std::ops::Range;

use crate::db::ResultColumn;

/// One row, copied out of a result to compare another row with. It is a
/// copy, so it survives paging, sorting and going back.
#[derive(Clone, Debug)]
pub struct RowSnapshot {
    /// Where the row came from: a table, or hand-written SQL.
    pub source: String,
    /// What tells the row apart: its primary key, or else its position.
    pub label: String,
    pub columns: Vec<ResultColumn>,
    pub values: Vec<Option<String>>,
    /// Indexes of the columns of the table's primary key. Empty when there
    /// is none, or the row came from hand-written SQL.
    pub key: Vec<usize>,
}

impl RowSnapshot {
    /// Whether a result with these columns has rows to compare this one with.
    pub fn fits(&self, columns: &[ResultColumn]) -> bool {
        self.columns.len() == columns.len()
            && self
                .columns
                .iter()
                .zip(columns)
                .all(|(a, b)| a.name == b.name)
    }

    /// Whether `values`, a row of a result this one [fits](Self::fits), is
    /// this row. Rows that are the same in every column cannot be told apart.
    pub fn is(&self, values: &[Option<String>]) -> bool {
        self.values == values
    }

    /// The value in column `i`; `None` for NULL.
    pub fn value(&self, i: usize) -> Option<&str> {
        self.values.get(i)?.as_deref()
    }

    pub fn is_key(&self, i: usize) -> bool {
        self.key.contains(&i)
    }
}

pub struct RowComparison {
    /// The row marked first.
    pub left: RowSnapshot,
    /// The row compared with it.
    pub right: RowSnapshot,
    /// One per column: whether the two rows hold different values there.
    /// NULL and an empty string differ. The primary key is left out: it
    /// differs between any two rows, and the rows are named by it already.
    pub differs: Vec<bool>,
}

impl RowComparison {
    /// `right` should come from a result `left` [fits](RowSnapshot::fits).
    pub fn new(left: RowSnapshot, right: RowSnapshot) -> RowComparison {
        let differs = (0..left.columns.len())
            .map(|i| !left.is_key(i) && left.values.get(i) != right.values.get(i))
            .collect();
        RowComparison {
            left,
            right,
            differs,
        }
    }

    pub fn differences(&self) -> usize {
        self.differs.iter().filter(|d| **d).count()
    }
}

/// Where two values part ways: in each, the byte range left over once the
/// start and end they share are taken off. Equal values give empty ranges.
pub fn changed(left: &str, right: &str) -> (Range<usize>, Range<usize>) {
    let start: usize = left
        .chars()
        .zip(right.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    // The end is looked for after the start, so the two cannot overlap.
    let end: usize = left[start..]
        .chars()
        .rev()
        .zip(right[start..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .sum();
    (start..left.len() - end, start..right.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, values: &[Option<&str>]) -> RowSnapshot {
        RowSnapshot {
            source: "customers".into(),
            label: label.into(),
            columns: ["id", "email", "name", "note"]
                .iter()
                .take(values.len())
                .map(|name| ResultColumn {
                    name: name.to_string(),
                    type_name: "text".into(),
                    numeric: false,
                })
                .collect(),
            values: values.iter().map(|v| v.map(String::from)).collect(),
            key: Vec::new(),
        }
    }

    fn pieces<'a>(text: &'a str, range: &Range<usize>) -> (&'a str, &'a str, &'a str) {
        (
            &text[..range.start],
            &text[range.clone()],
            &text[range.end..],
        )
    }

    #[test]
    fn columns_that_differ_are_found() {
        let comparison = RowComparison::new(
            row(
                "id 1",
                &[Some("1"), Some("ada@example.com"), Some("Ada"), None],
            ),
            row(
                "id 3",
                &[Some("3"), Some("ada@example.com"), Some("Ada"), None],
            ),
        );
        assert_eq!(comparison.differs, [true, false, false, false]);
        assert_eq!(comparison.differences(), 1);
    }

    #[test]
    fn the_primary_key_is_not_a_difference() {
        let mut left = row("id 1", &[Some("1"), Some("ada@example.com"), Some("Ada")]);
        let mut right = row("id 3", &[Some("3"), Some("ada@example.com"), Some("Ada D")]);
        left.key = vec![0];
        right.key = vec![0];
        let comparison = RowComparison::new(left, right);
        assert_eq!(comparison.differs, [false, false, true]);
        assert_eq!(comparison.differences(), 1);
    }

    #[test]
    fn null_and_empty_differ() {
        let comparison = RowComparison::new(row("a", &[None]), row("b", &[Some("")]));
        assert_eq!(comparison.differs, [true]);
    }

    #[test]
    fn a_row_is_recognised_by_its_values() {
        let marked = row("id 1", &[Some("1"), Some("ada@example.com")]);
        let other = row("id 3", &[Some("3"), Some("ada@example.com")]);
        assert!(marked.fits(&other.columns));
        assert!(marked.is(&marked.values));
        assert!(!marked.is(&other.values));

        let wider = row("id 1", &[Some("1"), Some("ada@example.com"), Some("Ada")]);
        assert!(
            !marked.fits(&wider.columns),
            "other columns, another result"
        );
        let mut renamed = other.clone();
        renamed.columns[1].name = "mail".into();
        assert!(!marked.fits(&renamed.columns));
    }

    #[test]
    fn the_part_that_changed_is_picked_out() {
        let (left, right) = ("Main Street 1", "Main St. 1");
        let (l, r) = changed(left, right);
        assert_eq!(pieces(left, &l), ("Main St", "reet", " 1"));
        assert_eq!(pieces(right, &r), ("Main St", ".", " 1"));
    }

    #[test]
    fn invisible_changes_still_have_a_place() {
        let (l, r) = changed("bob@example.com", "bob@example.com ");
        assert_eq!((l, r), (15..15, 15..16), "a trailing space");
        let (l, r) = changed("Oslo", "oslo");
        assert_eq!((l, r), (0..1, 0..1), "case");
    }

    #[test]
    fn equal_and_unrelated_values() {
        assert_eq!(changed("same", "same"), (4..4, 4..4));
        assert_eq!(changed("", ""), (0..0, 0..0));
        assert_eq!(changed("abc", "xyz"), (0..3, 0..3));
        assert_eq!(changed("", "new"), (0..0, 0..3));
    }

    #[test]
    fn repeated_letters_do_not_overlap() {
        // "aa" is the start of both; the end must not reuse it.
        assert_eq!(changed("aa", "aaa"), (2..2, 2..3));
        assert_eq!(changed("aaa", "aa"), (2..3, 2..2));
    }

    #[test]
    fn ranges_fall_on_character_boundaries() {
        let (left, right) = ("Blåbær", "Blåber");
        let (l, r) = changed(left, right);
        assert_eq!(pieces(left, &l), ("Blåb", "æ", "r"));
        assert_eq!(pieces(right, &r), ("Blåb", "e", "r"));
    }
}

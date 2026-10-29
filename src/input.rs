//! Text entry: a one-line input for filters and a small multi-line editor for
//! SQL. Cursor positions are counted in characters, not bytes.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(i, _)| i)
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Where the word before `cursor` starts.
fn word_start(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut at = cursor.min(chars.len());
    while at > 0 && !chars[at - 1].is_alphanumeric() {
        at -= 1;
    }
    while at > 0 && chars[at - 1].is_alphanumeric() {
        at -= 1;
    }
    at
}

/// Where the word after `cursor` ends.
fn word_end(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut at = cursor.min(chars.len());
    while at < chars.len() && !chars[at].is_alphanumeric() {
        at += 1;
    }
    while at < chars.len() && chars[at].is_alphanumeric() {
        at += 1;
    }
    at
}

fn plain(key: &KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
}

/// A single line of editable text.
#[derive(Clone, Debug, Default)]
pub struct Input {
    text: String,
    cursor: usize,
}

impl Input {
    pub fn new(text: &str) -> Input {
        Input {
            text: text.to_string(),
            cursor: char_len(text),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn set(&mut self, text: &str) {
        *self = Input::new(text);
    }

    pub fn insert_str(&mut self, text: &str) {
        // A pasted newline would be invisible in a one-line field, and one at
        // the end, copied along with a line, would turn into a stray space.
        let clean: String = text
            .trim_end_matches(['\r', '\n'])
            .chars()
            .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        let at = byte_index(&self.text, self.cursor);
        self.text.insert_str(at, &clean);
        self.cursor += char_len(&clean);
    }

    fn delete_range(&mut self, from: usize, to: usize) {
        let (start, end) = (byte_index(&self.text, from), byte_index(&self.text, to));
        self.text.replace_range(start..end, "");
        self.cursor = from;
    }

    /// Applies an editing key. Returns whether the key was used.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let len = char_len(&self.text);
        match key.code {
            KeyCode::Char('a') if ctrl(&key) => self.cursor = 0,
            KeyCode::Char('e') if ctrl(&key) => self.cursor = len,
            KeyCode::Char('u') if ctrl(&key) => self.delete_range(0, self.cursor),
            KeyCode::Char('k') if ctrl(&key) => self.delete_range(self.cursor, len),
            KeyCode::Char('w') if ctrl(&key) => {
                self.delete_range(word_start(&self.text, self.cursor), self.cursor)
            }
            KeyCode::Char(c) if plain(&key) => {
                let mut buffer = [0u8; 4];
                self.insert_str(c.encode_utf8(&mut buffer));
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.delete_range(self.cursor - 1, self.cursor)
            }
            KeyCode::Delete if self.cursor < len => self.delete_range(self.cursor, self.cursor + 1),
            KeyCode::Left if ctrl(&key) => self.cursor = word_start(&self.text, self.cursor),
            KeyCode::Right if ctrl(&key) => self.cursor = word_end(&self.text, self.cursor),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

/// A small multi-line text editor.
#[derive(Clone, Debug)]
pub struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl Editor {
    pub fn new(text: &str) -> Editor {
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let row = lines.len() - 1;
        let col = char_len(&lines[row]);
        Editor { lines, row, col }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// (row, column) of the cursor, in characters.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    fn line_len(&self) -> usize {
        char_len(&self.lines[self.row])
    }

    pub fn insert_str(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '\n' => self.newline(),
                '\r' => {}
                '\t' => self.insert_plain("  "),
                c if c.is_control() => {}
                c => {
                    let mut buffer = [0u8; 4];
                    self.insert_plain(c.encode_utf8(&mut buffer));
                }
            }
        }
    }

    fn insert_plain(&mut self, text: &str) {
        let at = byte_index(&self.lines[self.row], self.col);
        self.lines[self.row].insert_str(at, text);
        self.col += char_len(text);
    }

    fn newline(&mut self) {
        let at = byte_index(&self.lines[self.row], self.col);
        let rest = self.lines[self.row].split_off(at);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let (start, end) = (
                byte_index(&self.lines[self.row], self.col - 1),
                byte_index(&self.lines[self.row], self.col),
            );
            self.lines[self.row].replace_range(start..end, "");
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&line);
        }
    }

    fn delete(&mut self) {
        if self.col < self.line_len() {
            let (start, end) = (
                byte_index(&self.lines[self.row], self.col),
                byte_index(&self.lines[self.row], self.col + 1),
            );
            self.lines[self.row].replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn move_rows(&mut self, delta: isize) {
        let last = self.lines.len() - 1;
        self.row = self.row.saturating_add_signed(delta).min(last);
        self.col = self.col.min(self.line_len());
    }

    /// Applies an editing key. Returns whether the key was used.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('a') if ctrl(&key) => self.col = 0,
            KeyCode::Char('e') if ctrl(&key) => self.col = self.line_len(),
            KeyCode::Char('u') if ctrl(&key) => {
                let at = byte_index(&self.lines[self.row], self.col);
                self.lines[self.row].replace_range(..at, "");
                self.col = 0;
            }
            KeyCode::Char('k') if ctrl(&key) => {
                let at = byte_index(&self.lines[self.row], self.col);
                self.lines[self.row].truncate(at);
            }
            KeyCode::Char('w') if ctrl(&key) => {
                let from = word_start(&self.lines[self.row], self.col);
                let (start, end) = (
                    byte_index(&self.lines[self.row], from),
                    byte_index(&self.lines[self.row], self.col),
                );
                self.lines[self.row].replace_range(start..end, "");
                self.col = from;
            }
            KeyCode::Char(c) if plain(&key) => {
                let mut buffer = [0u8; 4];
                self.insert_plain(c.encode_utf8(&mut buffer));
            }
            KeyCode::Enter => self.newline(),
            KeyCode::Tab => self.insert_plain("  "),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if ctrl(&key) => self.col = word_start(&self.lines[self.row], self.col),
            KeyCode::Right if ctrl(&key) => self.col = word_end(&self.lines[self.row], self.col),
            KeyCode::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.line_len();
                }
            }
            KeyCode::Right => {
                if self.col < self.line_len() {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up => self.move_rows(-1),
            KeyCode::Down => self.move_rows(1),
            KeyCode::PageUp => self.move_rows(-10),
            KeyCode::PageDown => self.move_rows(10),
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.line_len(),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_into(input: &mut Input, text: &str) {
        for c in text.chars() {
            input.handle(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_and_deleting() {
        let mut input = Input::default();
        type_into(&mut input, "blåbær");
        assert_eq!((input.text(), input.cursor()), ("blåbær", 6));
        input.handle(key(KeyCode::Backspace));
        input.handle(key(KeyCode::Home));
        input.handle(key(KeyCode::Delete));
        assert_eq!(input.text(), "låbæ");
        input.handle(key(KeyCode::Right));
        type_into(&mut input, "X");
        assert_eq!(input.text(), "lXåbæ");
    }

    #[test]
    fn line_editing_shortcuts() {
        let mut input = Input::new("select * from users");
        input.handle(ctrl_key('w'));
        assert_eq!(input.text(), "select * from ");
        input.handle(ctrl_key('a'));
        input.handle(ctrl_key('k'));
        assert!(input.is_empty());
        input.set("abc def");
        input.handle(key(KeyCode::Left));
        input.handle(ctrl_key('u'));
        assert_eq!((input.text(), input.cursor()), ("f", 0));
    }

    #[test]
    fn keys_that_are_not_text_are_left_alone() {
        let mut input = Input::new("x");
        assert!(!input.handle(key(KeyCode::Enter)));
        assert!(!input.handle(key(KeyCode::Esc)));
        assert!(!input.handle(key(KeyCode::Up)));
        assert!(!input.handle(ctrl_key('r')));
        assert_eq!(input.text(), "x");
    }

    #[test]
    fn pasting_into_one_line_flattens_it() {
        let mut input = Input::default();
        input.insert_str("a\nb\tc\u{7}");
        assert_eq!(input.text(), "a b c");
        input.clear();
        input.insert_str("1042\r\n");
        assert_eq!(input.text(), "1042");
    }

    #[test]
    fn editor_splits_and_joins_lines() {
        let mut editor = Editor::new("SELECT 1");
        assert_eq!(editor.cursor(), (0, 8));
        editor.handle(key(KeyCode::Enter));
        editor.insert_str("FROM t");
        assert_eq!(editor.text(), "SELECT 1\nFROM t");
        editor.handle(key(KeyCode::Home));
        editor.handle(key(KeyCode::Backspace));
        assert_eq!(editor.text(), "SELECT 1FROM t");
        assert_eq!(editor.cursor(), (0, 8));
        editor.handle(key(KeyCode::Enter));
        editor.handle(key(KeyCode::Up));
        editor.handle(key(KeyCode::End));
        editor.handle(key(KeyCode::Delete));
        assert_eq!(editor.text(), "SELECT 1FROM t");
    }

    #[test]
    fn editor_pastes_several_lines() {
        let mut editor = Editor::new("");
        editor.insert_str("SELECT *\r\nFROM t\n\tWHERE a = 1");
        assert_eq!(editor.lines(), ["SELECT *", "FROM t", "  WHERE a = 1"]);
        assert_eq!(editor.cursor(), (2, 13));
    }

    #[test]
    fn editor_cursor_stays_inside_shorter_lines() {
        let mut editor = Editor::new("a long first line\nxy");
        editor.handle(key(KeyCode::Up));
        editor.handle(key(KeyCode::End));
        editor.handle(key(KeyCode::Down));
        assert_eq!(editor.cursor(), (1, 2));
        editor.handle(key(KeyCode::PageUp));
        assert_eq!(editor.cursor(), (0, 2));
    }
}

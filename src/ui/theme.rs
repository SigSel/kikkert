//! Colours.
//!
//! kikkert brings its own palette instead of using the terminal's sixteen
//! named colours, because themes remap those freely: in some, "dark gray" is
//! as bright as the text and "black" is a light gray, so dimmed text and the
//! selection stop standing out. Terminals that take 24-bit colour get the
//! palette as designed, others the nearest of the standard 256, which themes
//! leave alone. It is made for a dark background.

use std::sync::OnceLock;

use ratatui::style::Color;

use crate::db::Engine;

pub struct Theme {
    /// Text on a raised background, where the terminal's own may not read.
    pub text: Color,
    /// Quieter than the text but meant to be read: punctuation, details.
    pub subtle: Color,
    /// Secondary text: labels, counts, key hints, NULL.
    pub muted: Color,
    /// Borders and rules, which should recede.
    pub faint: Color,
    /// Raised background: the selected line, the header row, input fields.
    pub surface: Color,
    /// A step above `surface`.
    pub raised: Color,
    /// Behind popups.
    pub popup: Color,
    pub accent: Color,
    /// Text on `accent`.
    pub on_accent: Color,
    pub good: Color,
    pub bad: Color,
    pub warn: Color,
    pub keyword: Color,
    pub function: Color,
    pub string: Color,
    pub number: Color,
}

impl Theme {
    /// A colour per engine, so connections are told apart at a glance.
    pub fn engine(&self, engine: Engine) -> Color {
        match engine {
            Engine::Postgres => self.accent,
            Engine::Mysql => self.number,
            Engine::Mssql => self.keyword,
            Engine::Sqlite => self.function,
        }
    }
}

static TRUECOLOR: Theme = Theme {
    text: Color::Rgb(0xcd, 0xd6, 0xf4),
    subtle: Color::Rgb(0xa6, 0xad, 0xc8),
    muted: Color::Rgb(0x7f, 0x84, 0x9c),
    faint: Color::Rgb(0x45, 0x47, 0x5a),
    surface: Color::Rgb(0x31, 0x32, 0x44),
    raised: Color::Rgb(0x45, 0x47, 0x5a),
    popup: Color::Rgb(0x1e, 0x1e, 0x2e),
    accent: Color::Rgb(0x89, 0xb4, 0xfa),
    on_accent: Color::Rgb(0x11, 0x11, 0x1b),
    good: Color::Rgb(0xa6, 0xe3, 0xa1),
    bad: Color::Rgb(0xf3, 0x8b, 0xa8),
    warn: Color::Rgb(0xf9, 0xe2, 0xaf),
    keyword: Color::Rgb(0xcb, 0xa6, 0xf7),
    function: Color::Rgb(0x94, 0xe2, 0xd5),
    string: Color::Rgb(0xa6, 0xe3, 0xa1),
    number: Color::Rgb(0xfa, 0xb3, 0x87),
};

/// The same palette, each colour the nearest of the standard 256.
static XTERM: Theme = Theme {
    text: Color::Indexed(189),
    subtle: Color::Indexed(146),
    muted: Color::Indexed(103),
    faint: Color::Indexed(239),
    surface: Color::Indexed(237),
    raised: Color::Indexed(239),
    popup: Color::Indexed(235),
    accent: Color::Indexed(111),
    on_accent: Color::Indexed(233),
    good: Color::Indexed(151),
    bad: Color::Indexed(211),
    warn: Color::Indexed(223),
    keyword: Color::Indexed(183),
    function: Color::Indexed(116),
    string: Color::Indexed(151),
    number: Color::Indexed(216),
};

pub fn theme() -> &'static Theme {
    static THEME: OnceLock<&'static Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        if truecolor(|name| std::env::var(name).ok()) {
            &TRUECOLOR
        } else {
            &XTERM
        }
    })
}

/// Whether the terminal says it takes 24-bit colour.
fn truecolor(env: impl Fn(&str) -> Option<String>) -> bool {
    let var = |name| env(name).unwrap_or_default().to_ascii_lowercase();
    matches!(var("COLORTERM").as_str(), "truecolor" | "24bit")
        || ["kitty", "ghostty", "wezterm", "alacritty", "-direct"]
            .iter()
            .any(|name| var("TERM").contains(name))
        || matches!(
            var("TERM_PROGRAM").as_str(),
            "iterm.app" | "wezterm" | "ghostty" | "vscode"
        )
        || env("WT_SESSION").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(vars: &[(&str, &str)]) -> bool {
        truecolor(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn truecolor_is_taken_from_the_environment() {
        assert!(with(&[("COLORTERM", "truecolor")]));
        assert!(with(&[("COLORTERM", "24bit"), ("TERM", "screen")]));
        assert!(with(&[("TERM", "xterm-kitty")]));
        assert!(with(&[("TERM", "xterm-direct")]));
        assert!(with(&[("TERM_PROGRAM", "iTerm.app")]));
        assert!(with(&[("WT_SESSION", "")]));
    }

    #[test]
    fn otherwise_the_256_colours_are_used() {
        assert!(!with(&[]));
        assert!(!with(&[("TERM", "xterm-256color")]));
        assert!(!with(&[("TERM", "tmux-256color"), ("COLORTERM", "")]));
        assert!(!with(&[("TERM_PROGRAM", "Apple_Terminal")]));
    }
}

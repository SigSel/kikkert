//! Drawing. Everything on screen is derived from [`App`] each frame; the only
//! things written back are scroll offsets that depend on the window size.

mod syntax;
mod theme;

use std::borrow::Cow;
use std::collections::BTreeSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    App, CompareView, FilterDialog, FilterField, Focus, Grid, Overlay, Pane, Record, Screen, Total,
    Workspace,
};
use crate::compare::{Comparison, Row, Snapshot, Status, Verdict};
use crate::db::Engine;
use crate::db::meta::ObjectKind;
use crate::input::{Editor, Input};
use crate::query::{Dir, Filter, Mode, Op};
use theme::theme;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A key and what it does, as shown in the status bar and on popups.
type Hints = &'static [(&'static str, &'static str)];

fn muted() -> Style {
    Style::new().fg(theme().muted)
}

fn faint() -> Style {
    Style::new().fg(theme().faint)
}

fn subtle() -> Style {
    Style::new().fg(theme().subtle)
}

fn accent() -> Style {
    Style::new().fg(theme().accent)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn good() -> Style {
    Style::new().fg(theme().good)
}

fn bad() -> Style {
    Style::new().fg(theme().bad)
}

fn null() -> Style {
    muted().add_modifier(Modifier::ITALIC)
}

/// The cursor where it matters most: the grid cell, the chosen option.
fn cursor() -> Style {
    let t = theme();
    Style::new()
        .fg(t.on_accent)
        .bg(t.accent)
        .add_modifier(Modifier::BOLD)
}

/// The selected line of a list; it lights up when the list has focus.
fn highlight(focused: bool) -> Style {
    let t = theme();
    let style = Style::new().bg(t.surface).fg(t.text);
    if focused {
        style.fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// A rounded box whose border lights up when it has focus.
fn panel<'a>(focused: bool) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused { accent() } else { faint() })
}

fn panel_title(title: &str, focused: bool) -> Span<'static> {
    let style = if focused {
        accent().add_modifier(Modifier::BOLD)
    } else {
        subtle()
    };
    Span::styled(format!(" {title} "), style)
}

fn hint_spans(hints: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (key, action)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        if !key.is_empty() {
            spans.push(Span::styled(key.to_string(), accent()));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(action.to_string(), muted()));
    }
    spans
}

/// Hints for the bottom border of a box.
fn hint_title(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(hint_spans(hints));
    spans.push(Span::raw(" "));
    Line::from(spans).right_aligned()
}

// ---- text helpers ---------------------------------------------------------

pub fn text_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Makes a value safe to put in one terminal cell row: line breaks and
/// other control characters become visible stand-ins.
fn clean(value: &str) -> Cow<'_, str> {
    if !value.chars().any(char::is_control) {
        return Cow::Borrowed(value);
    }
    Cow::Owned(
        value
            .chars()
            .filter(|c| *c != '\r')
            .map(|c| match c {
                '\n' => '⏎',
                '\t' => ' ',
                c if c.is_control() => '·',
                c => c,
            })
            .collect(),
    )
}

/// The on-screen width of a value, without measuring more than `cap` of it.
pub fn cell_width(value: &str, cap: usize) -> usize {
    let mut width = 0;
    for c in value.chars() {
        if c == '\r' {
            continue;
        }
        width += if c.is_control() {
            1
        } else {
            c.width().unwrap_or(0)
        };
        if width >= cap {
            return cap;
        }
    }
    width
}

/// Clips or pads `text` to exactly `width` cells.
fn fit(text: &str, width: usize, right_align: bool) -> String {
    let text = clean(text);
    let total = text_width(&text);
    if total <= width {
        let padding = " ".repeat(width - total);
        return if right_align {
            format!("{padding}{text}")
        } else {
            format!("{text}{padding}")
        };
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    used += 1;
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Lines in the middle of `area`, each centred.
fn draw_centered(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    let at = centered(area, area.width, lines.len() as u16);
    frame.render_widget(
        Paragraph::new(lines).centered().wrap(Wrap { trim: true }),
        at,
    );
}

/// The part of a line from char `skip`, at most `take` chars of it.
fn crop(line: &Line<'static>, skip: usize, take: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut at = 0;
    for span in &line.spans {
        let len = span.content.chars().count();
        let start = skip.saturating_sub(at).min(len);
        let end = (skip + take).saturating_sub(at).min(len);
        if start < end {
            let piece: String = span.content.chars().skip(start).take(end - start).collect();
            spans.push(Span::styled(piece, span.style));
        }
        at += len;
    }
    Line::from(spans)
}

/// Draws a one-line input and, when focused, puts the cursor in it.
fn draw_input(frame: &mut Frame, area: Rect, input: &Input, focused: bool, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let room = area.width as usize;
    let chars: Vec<char> = input.text().chars().collect();
    let cursor = input.cursor().min(chars.len());
    // Scroll so the cursor stays in view.
    let mut start = 0;
    while chars[start..cursor]
        .iter()
        .map(|c| c.width().unwrap_or(0))
        .sum::<usize>()
        >= room
    {
        start += 1;
    }
    let shown: String = chars[start..].iter().collect();
    frame.render_widget(Paragraph::new(shown).style(style), area);
    if focused {
        let offset: usize = chars[start..cursor]
            .iter()
            .map(|c| c.width().unwrap_or(0))
            .sum();
        frame.set_cursor_position(Position::new(area.x + offset as u16, area.y));
    }
}

/// A scroll thumb drawn over a border, when there is more than fits.
/// `track` is one cell wide (vertical) or one cell high (horizontal).
fn scrollbar(frame: &mut Frame, track: Rect, offset: usize, shown: usize, total: usize, lit: bool) {
    if total <= shown || shown == 0 {
        return;
    }
    let vertical = track.width == 1;
    let length = if vertical { track.height } else { track.width } as usize;
    if length == 0 {
        return;
    }
    let thumb = (length * shown / total).clamp(1, length);
    let start = (length - thumb) * offset.min(total - shown) / (total - shown);
    // Brighter than any border, so it shows on a lit one too.
    let style = Style::new().fg(if lit { theme().text } else { theme().muted });
    let buffer = frame.buffer_mut();
    for i in start..start + thumb {
        let (x, y, symbol) = if vertical {
            (track.x, track.y + i as u16, "┃")
        } else {
            (track.x + i as u16, track.y, "━")
        };
        buffer[(x, y)].set_symbol(symbol).set_style(style);
    }
}

/// Fades what is behind a popup, so the popup is what the eye lands on.
fn scrim(frame: &mut Frame, area: Rect) {
    frame
        .buffer_mut()
        .set_style(area, Style::reset().fg(theme().faint));
}

// ---- the frame ------------------------------------------------------------

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [title, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_title(frame, title, app);
    match app.screen {
        Screen::Picker => draw_picker(frame, body, app),
        Screen::Workspace => draw_workspace(frame, body, app),
    }
    draw_status(frame, status, app);

    if !matches!(app.overlay, Overlay::None | Overlay::Editor(_)) {
        scrim(frame, body);
    }
    let ws = app.workspaces.get(app.current);
    match &mut app.overlay {
        Overlay::Help { scroll } => draw_help(frame, body, scroll),
        Overlay::Filter(dialog) => {
            if let Some(ws) = ws {
                draw_filter(frame, body, dialog, ws);
            }
        }
        Overlay::Filters { selected } => {
            if let Some(ws) = ws {
                draw_filters(frame, body, *selected, ws);
            }
        }
        Overlay::Record(record) => {
            if let Some(grid) = ws.and_then(Workspace::grid) {
                draw_record(frame, body, record, grid);
            }
        }
        Overlay::Compare(view) => draw_compare(frame, body, view),
        Overlay::None | Overlay::Editor(_) => {}
    }
}

fn draw_title(frame: &mut Frame, area: Rect, app: &App) {
    let t = theme();
    let mut spans = vec![Span::styled(
        " kikkert ",
        Style::new()
            .fg(t.on_accent)
            .bg(t.accent)
            .add_modifier(Modifier::BOLD),
    )];
    // The badge keeps its place however long the connection details are.
    // It also names the table held for comparing, which is not tied to any
    // one connection.
    let mut badge = Vec::new();
    if let Some(held) = &app.held {
        badge.extend([
            Span::styled(" ⇄ ", accent()),
            Span::styled(
                format!("{} ", fit(&held.table, 28, false).trim_end()),
                subtle(),
            ),
        ]);
    }
    badge.extend([
        Span::styled(" ● ", good()),
        Span::styled("read-only ", subtle()),
    ]);
    let badge = Line::from(badge).style(Style::new().bg(t.surface));
    let width = (badge.width() as u16).min(area.width);
    let [text_area, _, badge_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(width),
    ])
    .areas(area);
    // A tab for each open connection. A busy one out of sight spins.
    let on_screen = (app.screen == Screen::Workspace).then_some(app.current);
    for (i, ws) in app.workspaces.iter().enumerate() {
        let name = &ws.session.spec().name;
        spans.push(Span::raw(" "));
        if on_screen == Some(i) {
            spans.push(Span::styled(
                format!(" {name} "),
                Style::new()
                    .bg(t.surface)
                    .fg(t.text)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(format!(" {name} "), muted()));
            if ws.busy().is_some() {
                spans.push(Span::styled(
                    format!("{} ", SPINNER[app.tick % SPINNER.len()]),
                    accent(),
                ));
            }
        }
    }
    if let Some(ws) = on_screen.and_then(|i| app.workspaces.get(i)) {
        let spec = ws.session.spec();
        spans.extend([
            Span::raw("  "),
            Span::styled(spec.engine.label(), Style::new().fg(t.engine(spec.engine))),
            Span::styled(" · ", faint()),
        ]);
        let used: usize = spans.iter().map(Span::width).sum();
        let room = (text_area.width as usize).saturating_sub(used);
        let target = fit(&spec.target(), room, false);
        spans.push(Span::styled(target.trim_end().to_string(), muted()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), text_area);
    frame.render_widget(Paragraph::new(badge), badge_area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let left: Line = if let Some(job) = app.busy() {
        let seconds = job.started.elapsed().as_secs();
        let waited = if seconds >= 2 {
            format!(" {seconds}s")
        } else {
            String::new()
        };
        let mut spans = vec![
            Span::styled(format!(" {} ", SPINNER[app.tick % SPINNER.len()]), accent()),
            Span::raw(format!("{}…", job.label)),
            Span::styled(waited, muted()),
            Span::raw("   "),
        ];
        spans.extend(hint_spans(&[("Esc", "cancel")]));
        Line::from(spans)
    } else if let Some(toast) = &app.toast {
        let (mark, style) = if toast.error {
            ("✗", bad())
        } else {
            ("✓", good())
        };
        Line::from(vec![
            Span::styled(format!(" {mark} "), style.add_modifier(Modifier::BOLD)),
            Span::styled(clean(&toast.text).into_owned(), style),
        ])
    } else {
        let mut spans = vec![Span::raw(" ")];
        for (i, part) in position_text(app).into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ·  ", faint()));
            }
            spans.push(Span::styled(part, subtle()));
        }
        Line::from(spans)
    };

    // Key hints go on the right when there is room next to the status text.
    // The least important go first, and help stays to the last.
    let room = area.width as usize;
    let width = |hints: &[(&str, &str)]| Line::from(hint_spans(hints)).width();
    let mut shown = hints(app).to_vec();
    let help = shown.last().is_some_and(|(key, _)| *key == "?");
    while !shown.is_empty() && left.width() + width(&shown) + 3 > room {
        let drop = if help && shown.len() > 1 {
            shown.len() - 2
        } else {
            shown.len() - 1
        };
        shown.remove(drop);
    }
    let hint_width = width(&shown) as u16;
    let [left_area, right_area] = if shown.is_empty() {
        [area, Rect { width: 0, ..area }]
    } else {
        Layout::horizontal([Constraint::Min(1), Constraint::Length(hint_width + 1)]).areas(area)
    };
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(Line::from(hint_spans(&shown))), right_area);
}

fn hints(app: &App) -> Hints {
    match (&app.overlay, app.screen, app.ws()) {
        (Overlay::Editor(_), ..) => &[("F5", "run"), ("Ctrl-L", "reset"), ("Esc", "close")],
        (Overlay::None, Screen::Picker, Some(_)) => &[
            ("Enter", "connect"),
            ("Esc", "back"),
            ("/", "filter"),
            ("r", "reload"),
            ("?", "help"),
            ("Q", "quit"),
        ],
        (Overlay::None, Screen::Picker, None) => &[
            ("Enter", "connect"),
            ("/", "filter"),
            ("r", "reload"),
            ("?", "help"),
            ("q", "quit"),
        ],
        (Overlay::None, Screen::Workspace, Some(ws)) => match ws.focus {
            Focus::Sidebar if ws.side.filtering => {
                &[("", "type to filter"), ("Enter", "open"), ("Esc", "clear")]
            }
            Focus::Sidebar => &[
                ("Enter", "open"),
                ("/", "filter"),
                ("Tab", "data"),
                ("e", "SQL"),
                ("c", "connect"),
                ("v", "compare"),
                ("?", "help"),
            ],
            Focus::Main if ws.tab != 0 => &[
                ("1", "data"),
                ("Enter", "row"),
                ("y", "copy"),
                ("?", "help"),
            ],
            Focus::Main if ws.custom.is_some() => &[
                ("e", "edit SQL"),
                ("Backspace", "table"),
                ("Enter", "row"),
                ("?", "help"),
            ],
            Focus::Main if ws.groups().is_some() => &[
                ("Enter", "show rows"),
                ("Backspace", "back"),
                ("]", "next"),
                ("?", "help"),
            ],
            Focus::Main => &[
                ("/", "filter"),
                ("s", "sort"),
                ("d", "duplicates"),
                ("Enter", "row"),
                ("e", "SQL"),
                ("v", "compare"),
                ("?", "help"),
            ],
        },
        _ => &[],
    }
}

fn position_text(app: &App) -> Vec<String> {
    let Some(ws) = app.ws() else {
        return Vec::new();
    };
    if app.screen != Screen::Workspace {
        return Vec::new();
    }
    let Some(grid) = ws.grid() else {
        return Vec::new();
    };
    let rows = grid.rows();
    let mut parts: Vec<String> = Vec::new();

    if ws.tab != 0 {
        parts.push(format!("{rows} rows"));
    } else if ws.custom.is_some() {
        if grid.result.truncated {
            parts.push(format!(
                "first {} rows (max_rows limit reached)",
                thousands(rows as u64)
            ));
        } else {
            parts.push(format!("{} rows", thousands(rows as u64)));
        }
    } else if let Some(table) = &ws.table {
        let offset = table.query.offset();
        let page = table.query.page + 1;
        match &table.query.mode {
            Mode::Groups { .. } => parts.push(format!("{rows} values, page {page}")),
            Mode::Rows if rows == 0 => parts.push(match ws.total {
                Total::Known(0) | Total::Unknown | Total::Unavailable | Total::Counting
                    if offset == 0 =>
                {
                    "no rows".to_string()
                }
                _ => format!("no rows on page {page}"),
            }),
            Mode::Rows => {
                let range = format!(
                    "rows {}–{}",
                    thousands(offset as u64 + 1),
                    thousands((offset + rows) as u64)
                );
                parts.push(match ws.total {
                    Total::Known(total) => format!("{range} of {}", thousands(total)),
                    Total::Counting => format!("{range} of …"),
                    Total::Unknown | Total::Unavailable => range,
                });
            }
        }
        if !table.query.filters.is_empty() {
            let count = table.query.filters.len();
            parts.push(format!(
                "{count} filter{}",
                if count == 1 { "" } else { "s" }
            ));
        }
    }

    let elapsed = grid.result.elapsed;
    parts.push(if elapsed.as_millis() >= 1000 {
        format!("{:.1} s", elapsed.as_secs_f32())
    } else {
        format!("{} ms", elapsed.as_millis())
    });
    if let Some(column) = grid.column() {
        let mut cursor = format!("{}/{} {}", grid.col + 1, grid.cols(), column.name);
        if !column.type_name.is_empty() {
            cursor.push_str(&format!(" ({})", column.type_name));
        }
        parts.push(cursor);
    }
    parts
}

// ---- connection picker ----------------------------------------------------

fn draw_picker(frame: &mut Frame, area: Rect, app: &App) {
    let t = theme();
    let shown = app.picker_visible();
    let picker = &app.picker;

    if app.cfg.connections.is_empty() {
        draw_welcome(frame, area, app);
        return;
    }

    // The card, and above it the name when there is room for both.
    let width = 86.min(area.width);
    let room = width.saturating_sub(4).max(10) as usize;
    let error_lines = picker
        .error
        .as_ref()
        .map_or(0, |e| (text_width(e) + 2).div_ceil(room) as u16 + 1);
    let filter_line = u16::from(picker.filtering || !picker.filter.is_empty());
    let card_height = shown.len().max(1) as u16 + filter_line + error_lines + 4;
    let hero = if area.height >= card_height + 6 { 3 } else { 0 };
    let at = centered(area, width, card_height + hero);
    let [hero_area, card_area] =
        Layout::vertical([Constraint::Length(hero), Constraint::Min(3)]).areas(at);

    if hero > 0 {
        let name: String = "kikkert".chars().flat_map(|c| [c, ' ']).collect();
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    name.trim_end().to_string(),
                    accent().add_modifier(Modifier::BOLD),
                ),
                Line::styled("a read-only window on your databases", muted()),
            ])
            .centered(),
            hero_area,
        );
    }

    let count = if shown.len() == app.cfg.connections.len() {
        format!(" {} ", shown.len())
    } else {
        format!(" {} of {} ", shown.len(), app.cfg.connections.len())
    };
    let title = match app.ws() {
        Some(ws) if picker.replace => format!("Replace {}", ws.session.spec().name),
        _ => "Connections".to_string(),
    };
    let block = panel(true)
        .title(panel_title(&title, true))
        .title(Line::styled(count, muted()).right_aligned())
        .padding(Padding::uniform(1));
    let inner = block.inner(card_area);
    frame.render_widget(block, card_area);

    let [filter_area, list_area, error_area] = Layout::vertical([
        Constraint::Length(filter_line),
        Constraint::Min(1),
        Constraint::Length(error_lines),
    ])
    .areas(inner);

    if filter_line == 1 {
        let [slash, field] =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(filter_area);
        frame.render_widget(Paragraph::new("/").style(accent()), slash);
        draw_input(frame, field, &picker.filter, picker.filtering, Style::new());
    }

    let name_width = shown
        .iter()
        .map(|&i| text_width(&app.cfg.connections[i].name))
        .max()
        .unwrap_or(0)
        .clamp(8, 32);
    let visible_rows = list_area.height as usize;
    let top = picker
        .selected
        .saturating_sub(visible_rows.saturating_sub(1));
    let list_width = list_area.width as usize;
    let lines: Vec<Line> = shown
        .iter()
        .enumerate()
        .skip(top)
        .take(visible_rows)
        .map(|(row, &i)| {
            let connection = &app.cfg.connections[i];
            let engine = connection.engine.as_deref().unwrap_or("");
            let engine_style = Engine::parse(engine)
                .map_or_else(muted, |engine| Style::new().fg(t.engine(engine)));
            let mut summary_width = list_width.saturating_sub(name_width + 15);
            let status = if app.workspace_of(&connection.name).is_some() {
                const CONNECTED: &str = "● connected  ";
                summary_width = summary_width.saturating_sub(text_width(CONNECTED));
                Span::styled(CONNECTED, good())
            } else {
                Span::raw("")
            };
            let is_selected = row == picker.selected;
            let (bar, name_style, summary_style, line_style) = if is_selected {
                (
                    Span::styled("▌", accent()),
                    highlight(true),
                    subtle(),
                    Style::new().bg(t.surface),
                )
            } else {
                (Span::raw(" "), Style::new(), muted(), Style::new())
            };
            Line::from(vec![
                bar,
                Span::styled(
                    format!(" {}  ", fit(&connection.name, name_width, false)),
                    name_style,
                ),
                Span::styled(format!("{}  ", fit(engine, 9, false)), engine_style),
                status,
                Span::styled(
                    fit(&connection.summary(), summary_width, false),
                    summary_style,
                ),
            ])
            .style(line_style)
        })
        .collect();
    if lines.is_empty() {
        frame.render_widget(
            Paragraph::new("  Nothing matches the filter").style(null()),
            list_area,
        );
    } else {
        frame.render_widget(Paragraph::new(lines), list_area);
    }

    if let Some(error) = &picker.error {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("✗ ", bad().add_modifier(Modifier::BOLD)),
                Span::styled(error.clone(), bad()),
            ]))
            .wrap(Wrap { trim: true }),
            Rect {
                y: error_area.y + 1,
                height: error_area.height.saturating_sub(1),
                ..error_area
            },
        );
    }
}

/// The first run: no connections yet, so say how to add one.
fn draw_welcome(frame: &mut Frame, area: Rect, app: &App) {
    let t = theme();
    let width = 86.min(area.width);
    let room = width.saturating_sub(6) as usize;
    let mut lines = vec![
        Line::styled("No connections yet", bold()),
        Line::default(),
        Line::from(vec![
            Span::raw("Create "),
            Span::styled(app.cfg_path.display().to_string(), accent()),
            Span::raw(" with one block per database:"),
        ]),
        Line::default(),
    ];
    let code = Style::new().bg(t.surface).fg(t.text);
    for line in [
        "",
        "[[connection]]",
        "name = \"orders-prod\"",
        "item = \"op://Engineering/Orders Prod DB\"",
        "",
    ] {
        let mut spans = vec![Span::styled("  ", code)];
        match line.split_once(" = ") {
            Some((key, value)) => spans.extend([
                Span::styled(key, code.fg(t.accent)),
                Span::styled(" = ", code.fg(t.subtle)),
                Span::styled(value, code.fg(t.string)),
            ]),
            None => spans.push(Span::styled(line, code.fg(t.keyword))),
        }
        let used: usize = spans.iter().map(Span::width).sum();
        spans.push(Span::styled(" ".repeat(room.saturating_sub(used)), code));
        lines.push(Line::from(spans));
    }
    lines.extend([
        Line::default(),
        Line::from("The item is a 1Password \"Database\" item with type, server, port, database,"),
        Line::from(vec![
            Span::raw("username and password. Then press "),
            Span::styled("r", accent()),
            Span::raw(" to reload."),
        ]),
        Line::default(),
        Line::from(vec![
            Span::styled("kikkert --example-config", accent()),
            Span::styled(" prints a fuller example.", muted()),
        ]),
    ]);
    if let Some(error) = &app.picker.error {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("✗ ", bad().add_modifier(Modifier::BOLD)),
            Span::styled(error.clone(), bad()),
        ]));
    }
    // Room for the border, the padding and a wrapped config path.
    let height = lines.len() as u16 + 6;
    let block = panel(true)
        .title(panel_title("Welcome to kikkert", true))
        .padding(Padding::symmetric(2, 1));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        centered(area, width, height),
    );
}

// ---- workspace ------------------------------------------------------------

fn draw_workspace(frame: &mut Frame, area: Rect, app: &mut App) {
    let tick = app.tick;
    let Some(ws) = app.workspaces.get_mut(app.current) else {
        return;
    };
    let side_width = (area.width / 4)
        .clamp(20, 40)
        .min(area.width.saturating_sub(24));
    let [side, main] =
        Layout::horizontal([Constraint::Length(side_width), Constraint::Min(10)]).areas(area);

    draw_sidebar(frame, side, ws, matches!(app.overlay, Overlay::None));

    // The SQL on top: read-only normally, an editor while editing.
    let top_height = match &app.overlay {
        Overlay::Editor(editor) => {
            (editor.lines().len() as u16 + 2).clamp(8, (main.height * 3 / 5).max(8))
        }
        _ => (ws.sql.lines().count() as u16 + 2).clamp(3, 9),
    }
    .min(main.height.saturating_sub(4));
    let [sql_area, pane_area] =
        Layout::vertical([Constraint::Length(top_height), Constraint::Min(1)]).areas(main);

    match &app.overlay {
        Overlay::Editor(editor) => draw_editor(frame, sql_area, editor),
        _ => draw_sql(frame, sql_area, ws),
    }

    // The data and structure tabs share one box, with the tabs on its edge.
    // What the box shows goes in the corner if the tabs leave room for it.
    let focused = ws.focus == Focus::Main && matches!(app.overlay, Overlay::None);
    let tabs = tab_line(ws, focused);
    let context = context_line(ws);
    let fits = tabs.width() + context.width() + 4 <= pane_area.width as usize;
    let mut block = panel(focused).title(tabs);
    if fits {
        block = block.title(context.right_aligned());
    }
    let inner = block.inner(pane_area);
    frame.render_widget(block, pane_area);
    let text_area = inner.inner(Margin::new(1, 0));

    let (sorts, marked) = match (&ws.table, ws.builder_active() && ws.groups().is_none()) {
        (Some(table), true) => (
            table
                .query
                .sorts
                .iter()
                .map(|s| (s.column.clone(), s.dir))
                .collect::<Vec<_>>(),
            table.marked.clone(),
        ),
        _ => (Vec::new(), BTreeSet::new()),
    };
    let empty_hint: Vec<Line<'static>> = if ws.objects.is_empty() {
        vec![
            Line::styled("This login cannot see any tables here.", subtle()),
            Line::default(),
            Line::from(vec![
                Span::styled("Press ", muted()),
                Span::styled("e", accent()),
                Span::styled(" to write SQL.", muted()),
            ]),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::styled("Pick a table on the left and press ", subtle()),
                Span::styled("Enter", accent()),
            ]),
            Line::default(),
            Line::from(vec![
                Span::styled("t", accent()),
                Span::styled(" finds a table  ·  ", muted()),
                Span::styled("e", accent()),
                Span::styled(" writes SQL  ·  ", muted()),
                Span::styled("?", accent()),
                Span::styled(" lists every key", muted()),
            ]),
        ]
    };
    match ws.pane_mut() {
        Pane::Grid(grid) => draw_grid(frame, pane_area, inner, grid, focused, &sorts, &marked),
        Pane::Loading => draw_centered(
            frame,
            text_area,
            vec![Line::from(vec![
                Span::styled(format!("{} ", SPINNER[tick % SPINNER.len()]), accent()),
                Span::styled("Loading…", muted()),
            ])],
        ),
        Pane::Empty => draw_centered(frame, text_area, empty_hint),
        Pane::Error(error) => {
            let mut text = Text::from(vec![
                Line::default(),
                Line::from(vec![
                    Span::styled("✗ ", bad().add_modifier(Modifier::BOLD)),
                    Span::styled("Could not run this", bad().add_modifier(Modifier::BOLD)),
                ]),
                Line::default(),
            ]);
            for line in error.lines() {
                text.push_line(Line::from(line.to_string()));
            }
            text.push_line(Line::default());
            text.push_line(Line::from(hint_spans(&[
                ("e", "edits the SQL"),
                ("F", "lists the filters"),
                ("Backspace", "goes back"),
            ])));
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), text_area);
        }
    }
}

/// The tabs, for the top edge of the data box.
fn tab_line(ws: &Workspace, focused: bool) -> Line<'static> {
    let t = theme();
    let titles = ws.tab_titles();
    if titles.len() == 1 {
        return Line::from(panel_title(titles[0], focused));
    }
    let mut spans = vec![Span::raw(" ")];
    for (i, title) in titles.iter().enumerate() {
        let (number, name) = if i == ws.tab {
            let tab = Style::new().bg(t.surface);
            let name = if focused {
                tab.fg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                tab.fg(t.text)
            };
            (tab.fg(t.muted), name)
        } else {
            (faint(), muted())
        };
        spans.push(Span::styled(format!(" {} ", i + 1), number));
        spans.push(Span::styled(format!("{title} "), name));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// What the data box shows, for its top right corner.
fn context_line(ws: &Workspace) -> Line<'static> {
    let special = |text: String| Line::styled(format!(" {text} "), accent());
    if ws.custom.is_some() {
        return special("hand-written SQL".to_string());
    }
    let Some(table) = &ws.table else {
        return Line::default();
    };
    match &table.query.mode {
        Mode::Rows => Line::from(vec![
            Span::styled(format!(" {}", table.object.display_name()), subtle()),
            Span::styled(format!(" · {} ", table.object.kind_label), muted()),
        ]),
        Mode::Groups {
            columns,
            duplicates_only: true,
        } => special(format!("duplicates of {}", columns.join(", "))),
        Mode::Groups { columns, .. } => special(format!("values of {}", columns.join(", "))),
    }
}

fn draw_sidebar(frame: &mut Frame, area: Rect, ws: &mut Workspace, active: bool) {
    let t = theme();
    let visible = ws.visible();
    let focused = active && ws.focus == Focus::Sidebar;
    let count = if visible.len() == ws.objects.len() {
        format!(" {} ", ws.objects.len())
    } else {
        format!(" {} of {} ", visible.len(), ws.objects.len())
    };
    let block = panel(focused)
        .title(panel_title("Tables", focused))
        .title(Line::styled(count, muted()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let filter_line = u16::from(ws.side.filtering || !ws.side.filter.is_empty());
    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(filter_line), Constraint::Min(0)]).areas(inner);
    if filter_line == 1 {
        let [slash, field] =
            Layout::horizontal([Constraint::Length(3), Constraint::Min(1)]).areas(filter_area);
        frame.render_widget(Paragraph::new(" /").style(accent()), slash);
        draw_input(
            frame,
            field,
            &ws.side.filter,
            focused && ws.side.filtering,
            Style::new(),
        );
    }

    ws.side.selected = ws.side.selected.min(visible.len().saturating_sub(1));
    let width = list_area.width as usize;
    let open = ws.table.as_ref().map(|t| t.object.display_name());

    // Schema names are headings, so the selected object's line number is
    // not simply its index.
    let mut lines: Vec<Line> = Vec::new();
    let mut selected_line = 0;
    let mut schema: Option<&Option<String>> = None;
    for (position, &index) in visible.iter().enumerate() {
        let object = &ws.objects[index];
        if schema != Some(&object.schema) {
            schema = Some(&object.schema);
            if let Some(name) = &object.schema {
                lines.push(Line::styled(
                    fit(&format!(" {name}"), width, false),
                    muted().add_modifier(Modifier::BOLD),
                ));
            }
        }
        let is_open = open.as_deref() == Some(object.display_name().as_str());
        let is_selected = position == ws.side.selected;
        let glyph = if object.kind == ObjectKind::View {
            Span::styled("◇", Style::new().fg(t.keyword))
        } else {
            Span::raw(" ")
        };
        let name = fit(&object.name, width.saturating_sub(3), false);
        let line = if is_selected {
            selected_line = lines.len();
            let bar = if focused { "▌" } else { " " };
            Line::from(vec![
                Span::styled(bar, accent()),
                glyph,
                Span::styled(format!(" {name}"), highlight(focused)),
            ])
            .style(Style::new().bg(t.surface))
        } else {
            let style = if is_open {
                accent().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Line::from(vec![
                Span::raw(" "),
                glyph,
                Span::styled(format!(" {name}"), style),
            ])
        };
        lines.push(line);
    }

    let height = list_area.height as usize;
    if selected_line < ws.side.offset {
        // Show the schema heading above the first object too.
        ws.side.offset = selected_line.saturating_sub(1);
    } else if height > 0 && selected_line >= ws.side.offset + height {
        ws.side.offset = selected_line + 1 - height;
    }
    ws.side.offset = ws.side.offset.min(lines.len().saturating_sub(height));

    if lines.is_empty() {
        let message = if ws.objects.is_empty() {
            "  no tables"
        } else {
            "  no match"
        };
        frame.render_widget(Paragraph::new(message).style(null()), list_area);
    } else {
        let total = lines.len();
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(ws.side.offset)
            .take(height)
            .collect();
        frame.render_widget(Paragraph::new(shown), list_area);
        let track = Rect {
            x: area.right().saturating_sub(1),
            width: 1,
            ..list_area
        };
        scrollbar(frame, track, ws.side.offset, height, total, focused);
    }
}

fn draw_sql(frame: &mut Frame, area: Rect, ws: &Workspace) {
    let block = panel(false)
        .title(panel_title("SQL", false))
        .title_bottom(hint_title(&[("e", "edit")]))
        .padding(Padding::horizontal(1));
    let text = if ws.sql.is_empty() {
        Paragraph::new("Nothing has run yet").style(null())
    } else {
        Paragraph::new(syntax::sql(&ws.sql)).wrap(Wrap { trim: false })
    };
    frame.render_widget(text.block(block), area);
}

fn draw_editor(frame: &mut Frame, area: Rect, editor: &Editor) {
    let t = theme();
    let (row, col) = editor.cursor();
    let block = panel(true)
        .title(panel_title("SQL editor", true))
        .title_bottom(
            Line::styled(format!(" Ln {}, Col {} ", row + 1, col + 1), muted()).right_aligned(),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 8 || inner.height == 0 {
        return;
    }

    const GUTTER: u16 = 5;
    let height = inner.height as usize;
    let room = (inner.width - GUTTER) as usize;
    let top = (row + 1).saturating_sub(height);
    let left = (col + 1).saturating_sub(room);

    // The line being edited sits on a raised band.
    frame.buffer_mut().set_style(
        Rect {
            y: inner.y + (row - top) as u16,
            height: 1,
            ..inner
        },
        Style::new().bg(t.surface),
    );
    let lines: Vec<Line> = syntax::sql(&editor.text())
        .iter()
        .enumerate()
        .skip(top)
        .take(height)
        .map(|(i, line)| {
            let number = if i == row { accent() } else { faint() };
            let mut spans = vec![Span::styled(format!("{:>4} ", i + 1), number)];
            spans.extend(crop(line, left, room).spans);
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);

    let before: String = editor.lines()[row]
        .chars()
        .skip(left)
        .take(col - left)
        .collect();
    frame.set_cursor_position(Position::new(
        inner.x + GUTTER + text_width(&before) as u16,
        inner.y + (row - top) as u16,
    ));
}

/// The rows of a result. `area` is inside `frame_area`'s border, where the
/// scroll thumbs go.
fn draw_grid(
    frame: &mut Frame,
    frame_area: Rect,
    area: Rect,
    grid: &mut Grid,
    focused: bool,
    sorts: &[(String, Dir)],
    marked: &BTreeSet<String>,
) {
    let t = theme();
    if area.height < 2 || area.width < 6 {
        return;
    }
    let rows = grid.rows();
    let cols = grid.cols();
    if cols == 0 {
        draw_centered(
            frame,
            area,
            vec![Line::styled(
                "The statement ran and returned nothing to show.",
                muted(),
            )],
        );
        return;
    }
    let body = (area.height - 1) as usize;
    let room = area.width as usize;
    grid.page_rows = body;
    grid.row = grid.row.min(rows.saturating_sub(1));
    grid.col = grid.col.min(cols - 1);

    if grid.row < grid.row_off {
        grid.row_off = grid.row;
    } else if grid.row >= grid.row_off + body {
        grid.row_off = grid.row + 1 - body;
    }
    grid.row_off = grid.row_off.min(rows.saturating_sub(body));

    // Columns are " value │" wide; scroll until the cursor column fits.
    let width_of = |i: usize| grid.widths[i].min(room.saturating_sub(3)).max(1);
    grid.col_off = grid.col_off.min(grid.col);
    loop {
        let needed: usize = (grid.col_off..=grid.col).map(|i| width_of(i) + 3).sum();
        if needed <= room + 2 || grid.col_off >= grid.col {
            break;
        }
        grid.col_off += 1;
    }

    let buffer = frame.buffer_mut();
    // The header on a band of its own, and the cursor row raised across
    // the whole width, past the last column.
    buffer.set_style(
        Rect { height: 1, ..area },
        Style::new().bg(t.surface).fg(t.subtle),
    );
    if rows > 0 && grid.row >= grid.row_off && grid.row - grid.row_off < body {
        buffer.set_style(
            Rect {
                y: area.y + 1 + (grid.row - grid.row_off) as u16,
                height: 1,
                ..area
            },
            Style::new().bg(t.surface).fg(t.text),
        );
    }

    let mut x = area.x;
    let right_edge = area.x + area.width;
    let mut last_shown = grid.col_off;
    for c in grid.col_off..cols {
        if x >= right_edge {
            break;
        }
        let column = &grid.result.columns[c];
        let left = (right_edge - x) as usize;
        if left < 4 {
            break;
        }
        // A column cut off by the edge is clipped with an ellipsis.
        let width = width_of(c).min(left - 2);
        if width == width_of(c) {
            last_shown = c;
        }

        // Header: name, a mark for Space-selected columns, the sort arrow.
        let mut name = column.name.clone();
        if marked.contains(&column.name) {
            name = format!("● {name}");
        }
        if let Some(position) = sorts.iter().position(|(sorted, _)| *sorted == column.name) {
            let arrow = if sorts[position].1 == Dir::Asc {
                "↑"
            } else {
                "↓"
            };
            name = if sorts.len() > 1 {
                format!("{name} {arrow}{}", position + 1)
            } else {
                format!("{name} {arrow}")
            };
        }
        let header_style = if c == grid.col && focused {
            accent().add_modifier(Modifier::BOLD)
        } else if marked.contains(&column.name) {
            Style::new().fg(t.warn).add_modifier(Modifier::BOLD)
        } else if sorts.iter().any(|(sorted, _)| *sorted == column.name) {
            Style::new().fg(t.text).add_modifier(Modifier::BOLD)
        } else {
            bold()
        };
        buffer.set_stringn(
            x,
            area.y,
            format!(" {}", fit(&name, width, column.numeric)),
            left,
            header_style,
        );

        for line in 0..body {
            let r = grid.row_off + line;
            let y = area.y + 1 + line as u16;
            if r >= rows {
                break;
            }
            let value = grid.result.rows[r].get(c).and_then(|v| v.as_deref());
            let on_cell = r == grid.row && c == grid.col;
            let style = if on_cell && focused {
                cursor()
            } else if on_cell {
                Style::new().bg(t.raised).fg(t.text)
            } else if value.is_none() {
                null()
            } else {
                Style::new()
            };
            let text = fit(
                value.unwrap_or("NULL"),
                width,
                column.numeric && value.is_some(),
            );
            buffer.set_stringn(x, y, format!(" {text} "), left, style);
            if left > width + 2 {
                buffer.set_stringn(x + width as u16 + 2, y, "│", 1, faint());
            }
        }
        if left > width + 2 {
            buffer.set_stringn(x + width as u16 + 2, area.y, "│", 1, faint());
        }
        x += width as u16 + 3;
    }

    if rows == 0 {
        draw_centered(
            frame,
            Rect {
                y: area.y + 1,
                height: area.height - 1,
                ..area
            },
            vec![Line::styled("No rows", null())],
        );
    }

    // Thumbs on the right and bottom edges show where the view is.
    let right = Rect {
        x: frame_area.right().saturating_sub(1),
        y: area.y + 1,
        width: 1,
        height: area.height - 1,
    };
    scrollbar(frame, right, grid.row_off, body, rows, focused);
    let bottom = Rect {
        y: frame_area.bottom().saturating_sub(1),
        height: 1,
        ..area
    };
    let shown = last_shown + 1 - grid.col_off;
    scrollbar(frame, bottom, grid.col_off, shown, cols, focused);
}

// ---- overlays -------------------------------------------------------------

fn popup(frame: &mut Frame, area: Rect, title: &str, hints: &[(&str, &str)]) -> Rect {
    let t = theme();
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(accent())
        .style(Style::new().bg(t.popup).fg(t.text))
        .title(panel_title(title, true))
        .title_bottom(hint_title(hints))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn draw_filter(frame: &mut Frame, area: Rect, dialog: &FilterDialog, ws: &Workspace) {
    let t = theme();
    let Some(table) = &ws.table else { return };
    let engine = ws.session.engine();
    let columns = &table.query.columns;
    let matches = dialog.matches(columns);
    let at = centered(area, 74, 18);
    let inner = popup(
        frame,
        at,
        "Filter",
        &[("Tab", "next field"), ("Enter", "apply"), ("Esc", "cancel")],
    );
    if inner.height < 7 {
        return;
    }
    let [_, column_area, op_area, value_area, _, detail_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    let field = Style::new().bg(t.surface).fg(t.text);
    let row = |frame: &mut Frame, area: Rect, label: &str, focused: bool| -> Rect {
        let [label_area, field_area] =
            Layout::horizontal([Constraint::Length(10), Constraint::Min(1)]).areas(area);
        let (pointer, style) = if focused {
            ("› ", accent().add_modifier(Modifier::BOLD))
        } else {
            ("  ", muted())
        };
        frame.render_widget(
            Paragraph::new(format!("{pointer}{label}")).style(style),
            label_area,
        );
        field_area
    };

    let focus = dialog.focus;
    let area = row(frame, column_area, "Column", focus == FilterField::Column);
    frame.render_widget(Paragraph::new("").style(field), area);
    draw_input(
        frame,
        area.inner(Margin::new(1, 0)),
        &dialog.column,
        focus == FilterField::Column,
        field,
    );

    let area = row(frame, op_area, "Match", focus == FilterField::Operator);
    let op_style = if focus == FilterField::Operator {
        cursor()
    } else {
        field
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {} ", dialog.op().label()), op_style),
            Span::raw("  "),
            Span::styled("↑↓", accent()),
            Span::styled(" changes", muted()),
        ])),
        area,
    );

    let area = row(frame, value_area, "Value", focus == FilterField::Value);
    if dialog.op().needs_value() {
        frame.render_widget(Paragraph::new("").style(field), area);
        draw_input(
            frame,
            area.inner(Margin::new(1, 0)),
            &dialog.value,
            focus == FilterField::Value,
            field,
        );
    } else {
        frame.render_widget(Paragraph::new("not needed").style(null()), area);
    }

    match focus {
        FilterField::Column => {
            let pick = dialog.pick.min(matches.len().saturating_sub(1));
            let height = detail_area.height as usize;
            let top = (pick + 1).saturating_sub(height);
            let name_width = matches
                .iter()
                .map(|c| text_width(&c.name))
                .max()
                .unwrap_or(0)
                .min(40);
            let lines: Vec<Line> = matches
                .iter()
                .enumerate()
                .skip(top)
                .take(height)
                .map(|(i, column)| {
                    let name = fit(&column.name, name_width, false);
                    let key = if column.primary_key { " PK" } else { "" };
                    let text = fit(
                        &format!("  {name}  {}{key}", column.type_name),
                        detail_area.width as usize,
                        false,
                    );
                    if i == pick {
                        Line::styled(text, highlight(true))
                    } else {
                        Line::from(vec![
                            Span::raw(format!("  {name}  ")),
                            Span::styled(column.type_name.clone(), muted()),
                            Span::styled(key, Style::new().fg(t.warn)),
                        ])
                    }
                })
                .collect();
            if lines.is_empty() {
                frame.render_widget(
                    Paragraph::new("  No column matches").style(null()),
                    detail_area,
                );
            } else {
                frame.render_widget(Paragraph::new(lines), detail_area);
            }
        }
        FilterField::Operator => {
            let lines: Vec<Line> = Op::ALL
                .iter()
                .map(|op| {
                    let text = format!("  {} ", op.label());
                    if *op == dialog.op() {
                        Line::styled(text, highlight(true))
                    } else {
                        Line::styled(text, subtle())
                    }
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), detail_area);
        }
        FilterField::Value => {
            // Show what is about to be added to the query.
            let preview = dialog.chosen(columns).map(|column| {
                table.query.predicate(
                    engine,
                    &Filter {
                        column: column.name.clone(),
                        op: dialog.op(),
                        value: dialog.value.text().to_string(),
                    },
                )
            });
            let mut lines = vec![Line::styled("  Adds to the query", muted())];
            if let Some(sql) = preview {
                let keyword = if table.query.filters.is_empty() {
                    "WHERE"
                } else {
                    "AND"
                };
                lines.push(Line::default());
                for line in syntax::sql(&format!("{keyword} {sql}")) {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                }
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                detail_area,
            );
        }
    }
}

fn draw_filters(frame: &mut Frame, area: Rect, selected_index: usize, ws: &Workspace) {
    let t = theme();
    let Some(table) = &ws.table else { return };
    let filters = &table.query.filters;
    let at = centered(area, 74, (filters.len() as u16 + 4).clamp(7, 20));
    let inner = popup(
        frame,
        at,
        "Filters",
        &[
            ("d", "remove"),
            ("c", "clear all"),
            ("a", "add"),
            ("Esc", "close"),
        ],
    );
    if filters.is_empty() {
        draw_centered(
            frame,
            inner,
            vec![Line::from(vec![
                Span::styled("No filters. Press ", muted()),
                Span::styled("/", accent()),
                Span::styled(" on the data to add one.", muted()),
            ])],
        );
        return;
    }
    let width = inner.width as usize;
    let lines: Vec<Line> = filters
        .iter()
        .enumerate()
        .map(|(i, filter)| {
            let selected = i == selected_index.min(filters.len() - 1);
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(filter.column.clone(), bold()),
                Span::styled(
                    format!(" {}", filter.op.label()),
                    Style::new().fg(t.keyword),
                ),
            ];
            if filter.op.needs_value() {
                spans.push(Span::styled(
                    format!(" \"{}\"", clean(&filter.value)),
                    Style::new().fg(t.string),
                ));
            }
            let used: usize = spans.iter().map(Span::width).sum();
            if used > width {
                // Too long to colour piece by piece; clip it whole.
                return Line::styled(fit(&format!(" {}", filter.describe()), width, false), {
                    if selected {
                        highlight(true)
                    } else {
                        Style::new()
                    }
                });
            }
            spans.push(Span::raw(" ".repeat(width - used)));
            let line = Line::from(spans);
            if selected {
                line.style(Style::new().bg(t.surface))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One row shown vertically: every column on its own line, with the full
/// value of the selected one below.
fn draw_record(frame: &mut Frame, area: Rect, record: &mut Record, grid: &Grid) {
    let t = theme();
    let Some(row) = grid.result.rows.get(grid.row) else {
        return;
    };
    let width = area.width.saturating_sub(6).min(120);
    let height = area.height.saturating_sub(2);
    let at = centered(area, width, height);
    let title = format!("Row {} of {} shown", grid.row + 1, grid.rows());
    let inner = popup(
        frame,
        at,
        &title,
        &[
            ("↑↓", "field"),
            ("←→", "row"),
            ("y", "copy"),
            ("=", "filter"),
            ("Esc", "close"),
        ],
    );
    if inner.height < 4 {
        return;
    }
    let cols = grid.cols();
    record.field = record.field.min(cols.saturating_sub(1));
    let list_height = (cols as u16).min(inner.height * 3 / 5).max(1);
    let [list_area, rule_area, value_area] = Layout::vertical([
        Constraint::Length(list_height),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    let shown = list_area.height as usize;
    if record.field < record.offset {
        record.offset = record.field;
    } else if record.field >= record.offset + shown {
        record.offset = record.field + 1 - shown;
    }
    let name_width = grid
        .result
        .columns
        .iter()
        .map(|c| text_width(&c.name))
        .max()
        .unwrap_or(0)
        .min(32);
    let value_width = (list_area.width as usize).saturating_sub(name_width + 4);
    let lines: Vec<Line> = grid
        .result
        .columns
        .iter()
        .enumerate()
        .skip(record.offset)
        .take(shown)
        .map(|(i, column)| {
            let value = row.get(i).and_then(|v| v.as_deref());
            let name = fit(&column.name, name_width, false);
            let text = fit(value.unwrap_or("NULL"), value_width, false);
            let value_style = if value.is_none() {
                null()
            } else {
                Style::new()
            };
            if i == record.field {
                Line::from(vec![
                    Span::styled(format!(" {name}  "), highlight(true)),
                    Span::styled(format!("{text} "), value_style.fg(t.text)),
                ])
                .style(Style::new().bg(t.surface))
            } else {
                Line::from(vec![
                    Span::styled(format!(" {name}  "), muted()),
                    Span::styled(text, value_style),
                ])
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list_area);
    let track = Rect {
        x: at.right().saturating_sub(1),
        width: 1,
        ..list_area
    };
    scrollbar(frame, track, record.offset, shown, cols, true);

    let column = &grid.result.columns[record.field];
    let value = row.get(record.field).and_then(|v| v.as_deref());
    let mut heading = vec![
        Span::styled("── ", faint()),
        Span::styled(column.name.clone(), accent().add_modifier(Modifier::BOLD)),
    ];
    if !column.type_name.is_empty() {
        heading.push(Span::styled(format!("  {}", column.type_name), muted()));
    }
    if let Some(value) = value {
        let count = value.chars().count();
        heading.push(Span::styled(
            format!(
                "  {} char{}",
                thousands(count as u64),
                if count == 1 { "" } else { "s" }
            ),
            muted(),
        ));
    }
    let used: usize = heading.iter().map(Span::width).sum::<usize>() + 1;
    heading.push(Span::styled(
        format!(
            " {}",
            "─".repeat((rule_area.width as usize).saturating_sub(used))
        ),
        faint(),
    ));
    frame.render_widget(Paragraph::new(Line::from(heading)), rule_area);

    let value_area = Rect {
        y: value_area.y + 1,
        height: value_area.height.saturating_sub(1),
        ..value_area
    };
    let body = match value {
        None => Paragraph::new("NULL").style(null()),
        Some(value) => Paragraph::new(pretty(value)).wrap(Wrap { trim: false }),
    };
    frame.render_widget(body.scroll((record.scroll, 0)), value_area);
}

/// How a line of a comparison is marked, in the list and in the summary.
fn status_mark(status: Status) -> (&'static str, Style) {
    match status {
        Status::Same => ("=", faint()),
        Status::Alike => ("≈", subtle()),
        Status::Different => ("≠", Style::new().fg(theme().warn)),
        Status::OnlyLeft => ("−", bad()),
        Status::OnlyRight => ("+", good()),
    }
}

/// One attribute of one line of a comparison: the value when both sides
/// agree, both values when they do not.
fn compare_cell(comparison: &Comparison, row: &Row, k: usize) -> (String, Style) {
    let (left, right) = comparison.values(row, &comparison.attributes[k]);
    let shown = |v: Option<&str>| {
        v.filter(|v| !v.trim().is_empty())
            .unwrap_or("–")
            .to_string()
    };
    let both = |join: &str| format!("{} {join} {}", shown(left), shown(right));
    match (row.status(), row.cells[k]) {
        (Status::OnlyLeft, _) => (left.unwrap_or("").to_string(), bad()),
        (Status::OnlyRight, _) => (right.unwrap_or("").to_string(), good()),
        (_, Some(Verdict::Same)) => (left.unwrap_or("").to_string(), Style::new()),
        (_, Some(Verdict::Alike)) => (both("≈"), subtle()),
        (_, Some(Verdict::Different)) => (both("→"), Style::new().fg(theme().warn)),
        // Only one side reports this.
        (_, None) => (left.or(right).unwrap_or("").to_string(), muted()),
    }
}

fn compare_name(comparison: &Comparison, row: &Row) -> (String, Style) {
    match (comparison.names(row), row.status()) {
        ((Some(left), _), Status::OnlyLeft) => (left.to_string(), bad()),
        ((_, Some(right)), Status::OnlyRight) => (right.to_string(), good()),
        ((Some(left), Some(right)), _) if row.name == Verdict::Alike => {
            (format!("{left} ≈ {right}"), Style::new())
        }
        ((left, right), _) => (left.or(right).unwrap_or("").to_string(), Style::new()),
    }
}

/// Narrows the widest of `widths` until they add up to `room`, or all are
/// down to `min`.
fn squeeze(widths: &mut [usize], room: usize, min: usize) {
    while widths.iter().sum::<usize>() > room {
        let Some((i, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > min)
            .max_by_key(|(_, w)| **w)
        else {
            return;
        };
        widths[i] -= 1;
    }
}

/// Two tables' columns: a line per column, and below them both sides of the
/// one under the cursor in full.
fn draw_compare(frame: &mut Frame, area: Rect, view: &mut CompareView) {
    let t = theme();
    let shown = view.shown();
    let comparison = &view.comparison;
    let attributes = &comparison.attributes;
    let (left_label, right_label) = comparison.labels();

    // Which table is which.
    let side = |s: &Snapshot| {
        vec![
            Span::styled(s.connection.clone(), Style::new().fg(t.engine(s.engine))),
            Span::styled(" · ", faint()),
            Span::styled(s.table.clone(), bold()),
        ]
    };
    let mut heading = side(&comparison.left);
    heading.push(Span::styled("  →  ", accent()));
    heading.extend(side(&comparison.right));
    let heading = Line::from(heading);

    // How many of each.
    let mut summary = Vec::new();
    if !comparison.rows.iter().any(|r| r.status().differs()) {
        summary.push(Span::styled(
            "✓ No differences   ",
            good().add_modifier(Modifier::BOLD),
        ));
    }
    for (status, what) in [
        (Status::Different, "differ".to_string()),
        (Status::OnlyLeft, format!("only in {left_label}")),
        (Status::OnlyRight, format!("only in {right_label}")),
        (Status::Alike, "alike".to_string()),
        (Status::Same, "the same".to_string()),
    ] {
        let count = comparison.count(status);
        if count == 0 {
            continue;
        }
        let (mark, style) = status_mark(status);
        summary.extend([
            Span::styled(format!("{mark} "), style),
            Span::raw(format!("{count} ")),
            Span::styled(format!("{what}   "), muted()),
        ]);
    }
    let summary = Line::from(summary);

    // The list: a mark, the name, then each attribute. The widths come from
    // every line, so hiding the ones that match leaves the columns in place.
    let names: Vec<(String, Style)> = comparison
        .rows
        .iter()
        .map(|row| compare_name(comparison, row))
        .collect();
    let cells: Vec<Vec<(String, Style)>> = comparison
        .rows
        .iter()
        .map(|row| {
            (0..attributes.len())
                .map(|k| compare_cell(comparison, row, k))
                .collect()
        })
        .collect();
    // An attribute that is empty on every line is only shown below.
    let listed: Vec<usize> = (0..attributes.len())
        .filter(|&k| cells.iter().any(|row| !row[k].0.is_empty()))
        .collect();
    let mut widths = vec![
        names
            .iter()
            .map(|(name, _)| text_width(name))
            .chain([text_width("column")])
            .max()
            .unwrap_or(0)
            .min(36),
    ];
    for &k in &listed {
        widths.push(
            cells
                .iter()
                .map(|row| text_width(&row[k].0))
                .chain([text_width(&attributes[k])])
                .max()
                .unwrap_or(0)
                .min(40),
        );
    }
    // Three for the bar and the mark, two between columns.
    let gaps = 3 + 2 * listed.len();

    // The box is as big as what it shows, within the screen.
    let detail_height = attributes.len() as u16 + 3;
    let content_width = (gaps + widths.iter().sum::<usize>())
        .max(heading.width())
        .max(summary.width())
        .max(72) as u16;
    let content_height = 4 + comparison.rows.len() as u16 + 1 + detail_height;
    let at = centered(
        area,
        (content_width + 4).min(area.width.saturating_sub(6)),
        (content_height + 2)
            .max(16)
            .min(area.height.saturating_sub(2)),
    );
    let toggle = if view.differences_only {
        ("d", "all columns")
    } else {
        ("d", "only differences")
    };
    let inner = popup(
        frame,
        at,
        "Compare columns",
        &[("↑↓", "column"), toggle, ("Esc", "close")],
    );
    if inner.height < 8 || inner.width < 30 {
        return;
    }
    let room = inner.width as usize;
    squeeze(&mut widths, room.saturating_sub(gaps), 4);

    let detail_height = detail_height.min(inner.height / 2);
    let [heading_area, summary_area, _, list_area, _, detail_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(detail_height),
    ])
    .areas(inner);

    let heading = if heading.width() <= room {
        heading
    } else {
        Line::raw(fit(&heading.to_string(), room, false))
    };
    frame.render_widget(Paragraph::new(heading), heading_area);
    frame.render_widget(Paragraph::new(summary), summary_area);

    let body = list_area.height.saturating_sub(1) as usize;
    view.page_rows = body;
    view.selected = view.selected.min(shown.len().saturating_sub(1));
    if view.selected < view.offset {
        view.offset = view.selected;
    } else if body > 0 && view.selected >= view.offset + body {
        view.offset = view.selected + 1 - body;
    }
    view.offset = view.offset.min(shown.len().saturating_sub(body));

    let mut header = vec![Span::raw(format!("   {}", fit("column", widths[0], false)))];
    for (n, &k) in listed.iter().enumerate() {
        header.push(Span::raw(format!(
            "  {}",
            fit(&attributes[k], widths[n + 1], false)
        )));
    }
    let mut lines = vec![Line::from(header).style(Style::new().bg(t.surface).fg(t.subtle))];
    for (line, &i) in shown.iter().enumerate().skip(view.offset).take(body) {
        let selected = line == view.selected;
        let (mark, mark_style) = status_mark(comparison.rows[i].status());
        let (name, name_style) = &names[i];
        let mut spans = vec![
            Span::styled(if selected { "▌" } else { " " }, accent()),
            Span::styled(format!("{mark} "), mark_style),
            Span::styled(
                fit(name, widths[0], false),
                if selected {
                    highlight(true).patch(*name_style)
                } else {
                    *name_style
                },
            ),
        ];
        for (n, &k) in listed.iter().enumerate() {
            let (text, style) = &cells[i][k];
            spans.push(Span::styled(
                format!("  {}", fit(text, widths[n + 1], false)),
                *style,
            ));
        }
        let mut line = Line::from(spans);
        if selected {
            line = line.style(Style::new().bg(t.surface).fg(t.text));
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), list_area);
    if shown.is_empty() {
        draw_centered(
            frame,
            Rect {
                y: list_area.y + 1,
                height: list_area.height.saturating_sub(1),
                ..list_area
            },
            vec![Line::from(vec![
                Span::styled("No differences. ", muted()),
                Span::styled("d", accent()),
                Span::styled(" shows every column.", muted()),
            ])],
        );
    }
    let track = Rect {
        x: at.right().saturating_sub(1),
        y: list_area.y + 1,
        width: 1,
        height: list_area.height.saturating_sub(1),
    };
    scrollbar(frame, track, view.offset, body, shown.len(), true);

    // Both sides of the column under the cursor, in full.
    let Some(row) = shown.get(view.selected).map(|&i| &comparison.rows[i]) else {
        return;
    };
    let status = row.status();
    let (name, _) = compare_name(comparison, row);
    let verdict = match status {
        Status::Same => "the same".to_string(),
        Status::Alike => "alike: spelled differently, meaning the same".to_string(),
        Status::Different => "differs".to_string(),
        Status::OnlyLeft => format!("only in {left_label}"),
        Status::OnlyRight => format!("only in {right_label}"),
    };
    let mut rule = vec![
        Span::styled("── ", faint()),
        Span::styled(name, accent().add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {verdict}"), status_mark(status).1),
    ];
    let used = Line::from(rule.clone()).width() + 1;
    rule.push(Span::styled(
        format!(" {}", "─".repeat(room.saturating_sub(used))),
        faint(),
    ));

    let label_width = attributes
        .iter()
        .map(|a| text_width(a))
        .chain([text_width("position")])
        .max()
        .unwrap_or(0)
        + 2;
    let value_width = room.saturating_sub(label_width + 4) / 2;
    let pair = |label: &str, left: (String, Style), right: (String, Style)| {
        Line::from(vec![
            Span::styled(format!("  {}", fit(label, label_width, false)), muted()),
            Span::styled(format!("{}  ", fit(&left.0, value_width, false)), left.1),
            Span::styled(fit(&right.0, value_width, false), right.1),
        ])
    };
    let value = |v: Option<&str>, style: Style| match v.filter(|v| !v.trim().is_empty()) {
        Some(v) => (v.to_string(), style),
        None => ("–".to_string(), muted()),
    };
    let side_label = |s: &Snapshot, label: String| (label, bold().fg(t.engine(s.engine)));
    let mut detail = vec![
        Line::from(rule),
        pair(
            "",
            side_label(&comparison.left, left_label),
            side_label(&comparison.right, right_label),
        ),
        pair(
            "position",
            value(
                row.left.map(|i| (i + 1).to_string()).as_deref(),
                Style::new(),
            ),
            value(
                row.right.map(|j| (j + 1).to_string()).as_deref(),
                Style::new(),
            ),
        ),
    ];
    for (k, attribute) in attributes.iter().enumerate() {
        let (left, right) = comparison.values(row, attribute);
        let style = match row.cells[k] {
            Some(Verdict::Different) => Style::new().fg(t.warn),
            Some(Verdict::Alike) => subtle(),
            _ => Style::new(),
        };
        detail.push(pair(attribute, value(left, style), value(right, style)));
    }
    frame.render_widget(Paragraph::new(detail), detail_area);
}

/// JSON reads better indented and coloured; everything else is shown as it
/// is.
fn pretty(value: &str) -> Text<'static> {
    let trimmed = value.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(value)
        && let Ok(text) = serde_json::to_string_pretty(&json)
    {
        return Text::from(syntax::json(&text));
    }
    Text::raw(value.replace('\r', "").replace('\t', "    "))
}

fn draw_help(frame: &mut Frame, area: Rect, scroll: &mut u16) {
    const HELP: &[(&str, &[(&str, &str)])] = &[
        (
            "Everywhere",
            &[
                ("?", "this help"),
                ("Tab", "switch between the table list and the data"),
                ("t", "find a table by name"),
                ("e", "edit and run SQL"),
                ("1 … 5", "Data, Columns, Indexes, Keys, Definition"),
                ("Esc", "cancel a running query, close a dialog"),
            ],
        ),
        (
            "Connections",
            &[
                ("c", "open another; Esc comes back"),
                ("C", "open another in place of this one"),
                ("r  R", "next and previous open connection"),
                ("q", "close this connection"),
                ("Q", "quit"),
            ],
        ),
        (
            "Moving around",
            &[
                ("↑↓←→  h j k l", "move"),
                ("g  G", "first and last row"),
                ("0  $", "first and last column"),
                ("PgUp  PgDn", "a screen at a time"),
                ("]  [", "next and previous page"),
                ("Enter", "show the whole row"),
                ("y  Y", "copy the cell, copy the row"),
                ("F5  Ctrl-R", "run again"),
            ],
        ),
        (
            "Search and sort",
            &[
                ("/", "filter: choose a column, how to match, a value"),
                (
                    "=  !",
                    "keep, or drop, rows with the value under the cursor",
                ),
                ("F", "list and remove filters"),
                ("X", "clear all filters"),
                ("s", "sort by this column: ascending, descending, off"),
                ("S", "add this column as a further sort key"),
            ],
        ),
        (
            "Duplicates",
            &[
                ("d", "values of this column that occur more than once"),
                ("D", "every value of this column and how often it occurs"),
                ("Space", "mark columns, to check a combination with d"),
                ("Enter", "show the rows behind a line"),
                ("Backspace", "go back"),
            ],
        ),
        (
            "Compare tables",
            &[
                ("v", "hold this table's columns; on another, compare"),
                ("V", "hold this table's columns instead"),
                ("d", "in a comparison: only the differences"),
            ],
        ),
        (
            "SQL editor",
            &[
                ("F5  Ctrl-R", "run"),
                ("Ctrl-L", "reset to the SQL kikkert generated"),
                ("Esc", "close"),
            ],
        ),
    ];

    let section = |(title, keys): &(&'static str, &'static [(&'static str, &'static str)])| -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled(*title, accent().add_modifier(Modifier::BOLD)),
        ];
        for (key, what) in *keys {
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", fit(key, 14, false)), accent()),
                Span::styled(*what, Style::new()),
            ]));
        }
        lines.push(Line::default());
        lines
    };
    let note = Line::from(vec![
        Span::styled("● ", good()),
        Span::styled(
            "kikkert only reads. Statements that could change anything are refused.",
            muted(),
        ),
    ]);

    // Two columns when the window is wide enough, otherwise one long one.
    const COLUMN: u16 = 72;
    let wide = area.width >= COLUMN * 2 + 6;
    let (left, right): (Vec<Line>, Vec<Line>) = if wide {
        (
            HELP[..3].iter().flat_map(section).collect(),
            HELP[3..].iter().flat_map(section).chain([note]).collect(),
        )
    } else {
        (
            HELP.iter().flat_map(section).chain([note]).collect(),
            Vec::new(),
        )
    };

    let wanted = left.len().max(right.len()) as u16;
    let width = if wide { COLUMN * 2 + 4 } else { COLUMN + 4 };
    let at = centered(area, width, wanted + 3);
    let room = at.height.saturating_sub(3);
    *scroll = (*scroll).min(wanted.saturating_sub(room));
    let hints: Hints = if wanted > room {
        &[("↑↓", "scroll"), ("", "any other key closes")]
    } else {
        &[("", "any key closes")]
    };
    let inner = popup(frame, at, "Keys", hints);
    let inner = Rect {
        y: inner.y + 1,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    if wide {
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Length(COLUMN), Constraint::Min(1)]).areas(inner);
        frame.render_widget(Paragraph::new(left).scroll((*scroll, 0)), left_area);
        frame.render_widget(Paragraph::new(right).scroll((*scroll, 0)), right_area);
    } else {
        frame.render_widget(Paragraph::new(left).scroll((*scroll, 0)), inner);
    }
    let track = Rect {
        x: at.right().saturating_sub(1),
        width: 1,
        ..inner
    };
    scrollbar(
        frame,
        track,
        *scroll as usize,
        room as usize,
        wanted as usize,
        true,
    );
}

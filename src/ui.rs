//! Drawing. Everything on screen is derived from [`App`] each frame; the only
//! things written back are scroll offsets that depend on the window size.

use std::borrow::Cow;
use std::collections::BTreeSet;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    App, FilterDialog, FilterField, Focus, Grid, Overlay, Pane, Record, Screen, Total, Workspace,
};
use crate::db::meta::ObjectKind;
use crate::input::{Editor, Input};
use crate::query::{Dir, Filter, Mode, Op};

const ACCENT: Color = Color::Cyan;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn accent() -> Style {
    Style::new().fg(ACCENT)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn selected() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD)
}

fn border(focused: bool) -> Style {
    if focused { accent() } else { dim() }
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

    match &mut app.overlay {
        Overlay::Help { scroll } => draw_help(frame, body, scroll),
        Overlay::Filter(dialog) => {
            if let Some(ws) = &app.ws {
                draw_filter(frame, body, dialog, ws);
            }
        }
        Overlay::Filters { selected } => {
            if let Some(ws) = &app.ws {
                draw_filters(frame, body, *selected, ws);
            }
        }
        Overlay::Record(record) => {
            if let Some(grid) = app.ws.as_ref().and_then(Workspace::grid) {
                draw_record(frame, body, record, grid);
            }
        }
        Overlay::None | Overlay::Editor(_) => {}
    }
}

fn draw_title(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(" rowboat ", selected())];
    if let (Screen::Workspace, Some(ws)) = (app.screen, &app.ws) {
        let spec = ws.session.spec();
        spans.push(Span::styled(format!("  {}", spec.name), bold()));
        spans.push(Span::styled(
            format!("  {}  {}", spec.engine.label(), spec.target()),
            dim(),
        ));
    }
    // The badge keeps its place however long the connection details are.
    let badge = " READ-ONLY ";
    let width = (badge.len() as u16).min(area.width);
    let [text_area, _, badge_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(width),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), text_area);
    frame.render_widget(
        Paragraph::new(badge).style(Style::new().fg(Color::Black).bg(Color::Green)),
        badge_area,
    );
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let left: Line = if let Some(job) = app.busy() {
        let seconds = job.started.elapsed().as_secs();
        let waited = if seconds >= 2 {
            format!(" {seconds}s")
        } else {
            String::new()
        };
        Line::from(vec![
            Span::styled(format!(" {} ", SPINNER[app.tick % SPINNER.len()]), accent()),
            Span::raw(format!("{}…{waited}", job.label)),
            Span::styled("   Esc cancels", dim()),
        ])
    } else if let Some(toast) = &app.toast {
        let style = if toast.error {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Green)
        };
        Line::from(Span::styled(format!(" {}", clean(&toast.text)), style))
    } else {
        Line::from(Span::raw(format!(" {}", position_text(app))))
    };

    // Key hints go on the right when there is room next to the status text;
    // in a narrow window the status wins and help is one key away.
    let room = area.width as usize;
    let hints = [hint_text(app), "? help"]
        .into_iter()
        .find(|hints| !hints.is_empty() && left.width() + text_width(hints) + 3 <= room)
        .unwrap_or("");
    let hint_width = text_width(hints) as u16;
    let [left_area, right_area] = if hints.is_empty() {
        [area, Rect { width: 0, ..area }]
    } else {
        Layout::horizontal([Constraint::Min(1), Constraint::Length(hint_width + 1)]).areas(area)
    };
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(hints).style(dim()), right_area);
}

fn hint_text(app: &App) -> &'static str {
    match (&app.overlay, app.screen, &app.ws) {
        (Overlay::Editor(_), ..) => "F5 run  Ctrl-L reset  Esc close",
        (Overlay::Help { .. }, ..) => "",
        (Overlay::None, Screen::Picker, _) => "Enter connect  / filter  r reload  ? help  q quit",
        (Overlay::None, Screen::Workspace, Some(ws)) => match ws.focus {
            Focus::Sidebar if ws.side.filtering => "type to filter  Enter open  Esc clear",
            Focus::Sidebar => "Enter open  / filter  Tab data  e SQL  ? help",
            Focus::Main if ws.tab != 0 => "1 data  Enter row  y copy  ? help",
            Focus::Main if ws.custom.is_some() => "e edit SQL  Backspace table  Enter row  ? help",
            Focus::Main if ws.groups().is_some() => {
                "Enter show rows  Backspace back  ] next  ? help"
            }
            Focus::Main => "/ filter  s sort  d duplicates  Enter row  e SQL  ? help",
        },
        _ => "",
    }
}

fn position_text(app: &App) -> String {
    let Some(ws) = &app.ws else {
        return String::new();
    };
    if app.screen != Screen::Workspace {
        return String::new();
    }
    let Some(grid) = ws.grid() else {
        return String::new();
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
    parts.join("  ·  ")
}

// ---- connection picker ----------------------------------------------------

fn draw_picker(frame: &mut Frame, area: Rect, app: &App) {
    let shown = app.picker_visible();
    let picker = &app.picker;

    if app.cfg.connections.is_empty() {
        let mut lines = vec![
            Line::from(Span::styled("No connections yet", bold())),
            Line::default(),
            Line::from(format!(
                "Create {} with one block per database:",
                app.cfg_path.display()
            )),
            Line::default(),
        ];
        for line in [
            "[[connection]]",
            "name = \"orders-prod\"",
            "item = \"op://Engineering/Orders Prod DB\"",
        ] {
            lines.push(Line::from(Span::styled(format!("    {line}"), accent())));
        }
        lines.push(Line::default());
        lines.push(Line::from(
            "The item is a 1Password \"Database\" item with type, server, port, database,",
        ));
        lines.push(Line::from("username and password. Then press r to reload."));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "rowboat --example-config prints a fuller example.",
            dim(),
        )));
        if let Some(error) = &picker.error {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::new().fg(Color::Red),
            )));
        }
        // Room for the border, the padding and a wrapped config path.
        let height = lines.len() as u16 + 6;
        let block = Block::bordered()
            .title(" Welcome aboard ")
            .border_style(accent());
        let at = centered(area, 86, height);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block.padding(ratatui::widgets::Padding::uniform(1)))
                .wrap(Wrap { trim: false }),
            at,
        );
        return;
    }

    let error_lines = picker.error.as_ref().map_or(0, |e| {
        let width = 86usize.min(area.width as usize).saturating_sub(4).max(10);
        (text_width(e) / width + 2) as u16
    });
    let filter_line = u16::from(picker.filtering || !picker.filter.is_empty());
    let height = (shown.len() as u16 + filter_line + error_lines + 2).clamp(5, area.height);
    let at = centered(area, 86, height);
    let block = Block::bordered()
        .title(format!(" Connections ({}) ", shown.len()))
        .border_style(accent());
    let inner = block.inner(at);
    frame.render_widget(block, at);

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
    let lines: Vec<Line> = shown
        .iter()
        .enumerate()
        .skip(top)
        .take(visible_rows)
        .map(|(row, &i)| {
            let connection = &app.cfg.connections[i];
            let engine = connection.engine.as_deref().unwrap_or("");
            let text = format!(
                " {}  {}  {}",
                fit(&connection.name, name_width, false),
                fit(engine, 9, false),
                connection.summary()
            );
            if row == picker.selected {
                Line::from(Span::styled(
                    fit(&text, list_area.width as usize, false),
                    selected(),
                ))
            } else {
                Line::from(vec![
                    Span::raw(format!(" {}  ", fit(&connection.name, name_width, false))),
                    Span::styled(
                        format!("{}  {}", fit(engine, 9, false), connection.summary()),
                        dim(),
                    ),
                ])
            }
        })
        .collect();
    if lines.is_empty() {
        frame.render_widget(
            Paragraph::new(" Nothing matches the filter").style(dim()),
            list_area,
        );
    } else {
        frame.render_widget(Paragraph::new(lines), list_area);
    }

    if let Some(error) = &picker.error {
        // Inset, so wrapped lines line up with the first.
        let inset = Rect {
            x: error_area.x + 1,
            y: error_area.y + 1,
            width: error_area.width.saturating_sub(2),
            height: error_area.height.saturating_sub(1),
        };
        frame.render_widget(
            Paragraph::new(error.clone())
                .style(Style::new().fg(Color::Red))
                .wrap(Wrap { trim: true }),
            inset,
        );
    }
}

// ---- workspace ------------------------------------------------------------

fn draw_workspace(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(ws) = &mut app.ws else { return };
    let side_width = (area.width / 4)
        .clamp(20, 40)
        .min(area.width.saturating_sub(24));
    let [side, main] =
        Layout::horizontal([Constraint::Length(side_width), Constraint::Min(10)]).areas(area);

    let editing = matches!(app.overlay, Overlay::Editor(_));
    draw_sidebar(
        frame,
        side,
        ws,
        !editing && matches!(app.overlay, Overlay::None),
    );

    // The SQL on top: read-only normally, an editor while editing.
    let top_height = match &app.overlay {
        Overlay::Editor(editor) => {
            (editor.lines().len() as u16 + 2).clamp(8, (main.height * 3 / 5).max(8))
        }
        _ => (ws.sql.lines().count() as u16 + 2).clamp(3, 9),
    }
    .min(main.height.saturating_sub(4));
    let [sql_area, tabs_area, pane_area] = Layout::vertical([
        Constraint::Length(top_height),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(main);

    match &app.overlay {
        Overlay::Editor(editor) => draw_editor(frame, sql_area, editor),
        _ => draw_sql(frame, sql_area, ws),
    }
    draw_tabs(frame, tabs_area, ws);

    let focused = ws.focus == Focus::Main && matches!(app.overlay, Overlay::None);
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
    let empty_hint = if ws.objects.is_empty() {
        "This login cannot see any tables here.\n\nPress e to write SQL."
    } else {
        "Pick a table on the left and press Enter.\n\nt finds a table by name, e opens the SQL editor, ? lists every key."
    };
    match ws.pane_mut() {
        Pane::Grid(grid) => draw_grid(frame, pane_area, grid, focused, &sorts, &marked),
        Pane::Loading => {
            frame.render_widget(Paragraph::new("\n  Loading…").style(dim()), pane_area)
        }
        Pane::Empty => frame.render_widget(
            Paragraph::new(format!("\n{empty_hint}"))
                .style(dim())
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            pane_area,
        ),
        Pane::Error(error) => {
            let lines = vec![
                Line::default(),
                Line::from(Span::styled(" Could not run this", bold().fg(Color::Red))),
                Line::default(),
            ];
            let mut text = ratatui::text::Text::from(lines);
            for line in error.lines() {
                text.push_line(Line::from(format!(" {line}")));
            }
            text.push_line(Line::default());
            text.push_line(Line::from(Span::styled(
                " e edits the SQL, F lists the filters, Backspace goes back.",
                dim(),
            )));
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), pane_area);
        }
    }
}

fn draw_sidebar(frame: &mut Frame, area: Rect, ws: &mut Workspace, active: bool) {
    let visible = ws.visible();
    let focused = active && ws.focus == Focus::Sidebar;
    let title = if visible.len() == ws.objects.len() {
        format!(" Tables ({}) ", ws.objects.len())
    } else {
        format!(" Tables ({}/{}) ", visible.len(), ws.objects.len())
    };
    let block = Block::bordered().title(title).border_style(border(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let filter_line = u16::from(ws.side.filtering || !ws.side.filter.is_empty());
    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(filter_line), Constraint::Min(0)]).areas(inner);
    if filter_line == 1 {
        let [slash, field] =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).areas(filter_area);
        frame.render_widget(Paragraph::new("/").style(accent()), slash);
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
                lines.push(Line::from(Span::styled(
                    fit(name, width, false),
                    dim().add_modifier(Modifier::BOLD),
                )));
            }
        }
        let marker = if object.kind == ObjectKind::View {
            "◇"
        } else {
            " "
        };
        let text = fit(&format!(" {marker} {}", object.name), width, false);
        let is_open = open.as_deref() == Some(object.display_name().as_str());
        let style = if position == ws.side.selected {
            selected_line = lines.len();
            if focused {
                selected()
            } else {
                Style::new().add_modifier(Modifier::REVERSED)
            }
        } else if is_open {
            accent().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        lines.push(Line::from(Span::styled(text, style)));
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
            " (no tables)"
        } else {
            " (no match)"
        };
        frame.render_widget(Paragraph::new(message).style(dim()), list_area);
    } else {
        let shown: Vec<Line> = lines
            .into_iter()
            .skip(ws.side.offset)
            .take(height)
            .collect();
        frame.render_widget(Paragraph::new(shown), list_area);
    }
}

fn draw_sql(frame: &mut Frame, area: Rect, ws: &Workspace) {
    let block = Block::bordered()
        .title(" SQL ")
        .title_bottom(Line::from(" e to edit ").right_aligned())
        .border_style(dim());
    let text = if ws.sql.is_empty() {
        Paragraph::new("Nothing has run yet").style(dim())
    } else {
        Paragraph::new(ws.sql.clone()).wrap(Wrap { trim: false })
    };
    frame.render_widget(text.block(block), area);
}

fn draw_editor(frame: &mut Frame, area: Rect, editor: &Editor) {
    let block = Block::bordered()
        .title(" SQL editor ")
        .title_bottom(
            Line::from(" F5 or Ctrl-R runs · Ctrl-L resets · Esc closes ").right_aligned(),
        )
        .border_style(accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 8 || inner.height == 0 {
        return;
    }

    const GUTTER: u16 = 4;
    let (row, col) = editor.cursor();
    let height = inner.height as usize;
    let room = (inner.width - GUTTER) as usize;
    let top = (row + 1).saturating_sub(height);
    let left = (col + 1).saturating_sub(room);

    let lines: Vec<Line> = editor
        .lines()
        .iter()
        .enumerate()
        .skip(top)
        .take(height)
        .map(|(i, line)| {
            let shown: String = line.chars().skip(left).take(room).collect();
            Line::from(vec![
                Span::styled(format!("{:>3} ", i + 1), dim()),
                Span::raw(shown),
            ])
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

fn draw_tabs(frame: &mut Frame, area: Rect, ws: &Workspace) {
    let mut spans = vec![Span::raw(" ")];
    for (i, title) in ws.tab_titles().iter().enumerate() {
        let label = format!(" {} {title} ", i + 1);
        spans.push(if i == ws.tab {
            Span::styled(label, selected())
        } else {
            Span::styled(label, dim())
        });
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    let context = if ws.custom.is_some() {
        "hand-written SQL".to_string()
    } else {
        match &ws.table {
            Some(table) => match &table.query.mode {
                Mode::Rows => format!(
                    "{} · {}",
                    table.object.display_name(),
                    table.object.kind_label
                ),
                Mode::Groups {
                    columns,
                    duplicates_only: true,
                } => {
                    format!("duplicates of {}", columns.join(", "))
                }
                Mode::Groups { columns, .. } => format!("values of {}", columns.join(", ")),
            },
            None => String::new(),
        }
    };
    frame.render_widget(
        Paragraph::new(format!("{context} "))
            .style(accent())
            .alignment(Alignment::Right),
        area,
    );
}

fn draw_grid(
    frame: &mut Frame,
    area: Rect,
    grid: &mut Grid,
    focused: bool,
    sorts: &[(String, Dir)],
    marked: &BTreeSet<String>,
) {
    if area.height < 2 || area.width < 6 {
        return;
    }
    let rows = grid.rows();
    let cols = grid.cols();
    if cols == 0 {
        frame.render_widget(
            Paragraph::new("\n  The statement ran and returned nothing to show.").style(dim()),
            area,
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
    let mut x = area.x;
    let right_edge = area.x + area.width;
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
            Style::new()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
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
            let on_row = r == grid.row;
            let on_cell = on_row && c == grid.col;
            let mut style = match value {
                None => dim().add_modifier(Modifier::ITALIC),
                Some(_) => Style::new(),
            };
            if on_cell && focused {
                style = selected();
            } else if on_cell {
                style = style.add_modifier(Modifier::REVERSED);
            } else if on_row {
                style = style.bg(Color::Indexed(238)).fg(match value {
                    None => Color::Gray,
                    Some(_) => Color::White,
                });
            }
            let text = fit(
                value.unwrap_or("NULL"),
                width,
                column.numeric && value.is_some(),
            );
            buffer.set_stringn(x, y, format!(" {text} "), left, style);
            if left > width + 2 {
                buffer.set_stringn(x + width as u16 + 2, y, "│", 1, dim());
            }
        }
        if left > width + 2 {
            buffer.set_stringn(x + width as u16 + 2, area.y, "│", 1, dim());
        }
        x += width as u16 + 3;
    }

    if rows == 0 {
        frame.render_widget(
            Paragraph::new("  No rows.").style(dim()),
            Rect {
                y: area.y + 1,
                height: 1,
                ..area
            },
        );
    }
}

// ---- overlays -------------------------------------------------------------

fn popup(frame: &mut Frame, area: Rect, title: &str, hint: &str) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(format!(" {title} "))
        .title_bottom(Line::from(format!(" {hint} ")).right_aligned())
        .border_style(accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

fn draw_filter(frame: &mut Frame, area: Rect, dialog: &FilterDialog, ws: &Workspace) {
    let Some(table) = &ws.table else { return };
    let engine = ws.session.engine();
    let columns = &table.query.columns;
    let matches = dialog.matches(columns);
    let at = centered(area, 72, 17);
    let inner = popup(
        frame,
        at,
        "Filter",
        "Tab next field · Enter apply · Esc cancel",
    );
    if inner.height < 6 {
        return;
    }
    let [column_area, op_area, value_area, _, detail_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);

    let row = |frame: &mut Frame, area: Rect, label: &str, focused: bool| -> Rect {
        let [label_area, field] =
            Layout::horizontal([Constraint::Length(11), Constraint::Min(1)]).areas(area);
        let style = if focused {
            accent().add_modifier(Modifier::BOLD)
        } else {
            dim()
        };
        let pointer = if focused { "›" } else { " " };
        frame.render_widget(
            Paragraph::new(format!("{pointer}{label}")).style(style),
            label_area,
        );
        field
    };

    let field = row(
        frame,
        column_area,
        " Column",
        dialog.focus == FilterField::Column,
    );
    draw_input(
        frame,
        field,
        &dialog.column,
        dialog.focus == FilterField::Column,
        Style::new(),
    );

    let field = row(
        frame,
        op_area,
        " Match",
        dialog.focus == FilterField::Operator,
    );
    let op_style = if dialog.focus == FilterField::Operator {
        selected()
    } else {
        Style::new()
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {} ", dialog.op().label()), op_style),
            Span::styled("   ↑↓ changes", dim()),
        ])),
        field,
    );

    let field = row(
        frame,
        value_area,
        " Value",
        dialog.focus == FilterField::Value,
    );
    if dialog.op().needs_value() {
        draw_input(
            frame,
            field,
            &dialog.value,
            dialog.focus == FilterField::Value,
            Style::new(),
        );
    } else {
        frame.render_widget(Paragraph::new("(not needed)").style(dim()), field);
    }

    match dialog.focus {
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
                    if i == pick {
                        Line::from(Span::styled(
                            fit(
                                &format!(" {name}  {}{key}", column.type_name),
                                detail_area.width as usize,
                                false,
                            ),
                            selected(),
                        ))
                    } else {
                        Line::from(vec![
                            Span::raw(format!(" {name}  ")),
                            Span::styled(format!("{}{key}", column.type_name), dim()),
                        ])
                    }
                })
                .collect();
            if lines.is_empty() {
                frame.render_widget(
                    Paragraph::new(" No column matches").style(dim()),
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
                    if *op == dialog.op() {
                        Line::from(Span::styled(format!(" {} ", op.label()), selected()))
                    } else {
                        Line::from(Span::raw(format!(" {} ", op.label())))
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
            let mut lines = vec![Line::from(Span::styled(" Adds to the query:", dim()))];
            if let Some(sql) = preview {
                let keyword = if table.query.filters.is_empty() {
                    "WHERE"
                } else {
                    "AND"
                };
                lines.push(Line::from(Span::styled(
                    format!(" {keyword} {sql}"),
                    accent(),
                )));
            }
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }),
                detail_area,
            );
        }
    }
}

fn draw_filters(frame: &mut Frame, area: Rect, selected_index: usize, ws: &Workspace) {
    let Some(table) = &ws.table else { return };
    let filters = &table.query.filters;
    let at = centered(area, 72, (filters.len() as u16 + 4).clamp(6, 20));
    let inner = popup(
        frame,
        at,
        "Filters",
        "d remove · c clear all · a add · Esc close",
    );
    if filters.is_empty() {
        frame.render_widget(
            Paragraph::new("\n No filters. Press / on the data to add one.").style(dim()),
            inner,
        );
        return;
    }
    let lines: Vec<Line> = filters
        .iter()
        .enumerate()
        .map(|(i, filter)| {
            let text = fit(
                &format!(" {}", filter.describe()),
                inner.width as usize,
                false,
            );
            if i == selected_index.min(filters.len() - 1) {
                Line::from(Span::styled(text, selected()))
            } else {
                Line::from(text)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// One row shown vertically: every column on its own line, with the full
/// value of the selected one below.
fn draw_record(frame: &mut Frame, area: Rect, record: &mut Record, grid: &Grid) {
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
        "↑↓ field · ←→ row · y copy · = filter · Esc close",
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
            if i == record.field {
                Line::from(Span::styled(format!(" {name}  {text} "), selected()))
            } else {
                Line::from(vec![
                    Span::styled(format!(" {name}  "), accent()),
                    Span::styled(
                        text,
                        if value.is_none() {
                            dim().add_modifier(Modifier::ITALIC)
                        } else {
                            Style::new()
                        },
                    ),
                ])
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list_area);

    let column = &grid.result.columns[record.field];
    let value = row.get(record.field).and_then(|v| v.as_deref());
    let mut heading = format!("─ {} ", column.name);
    if !column.type_name.is_empty() {
        heading.push_str(&format!("({}) ", column.type_name));
    }
    if let Some(value) = value {
        let count = value.chars().count();
        heading.push_str(&format!(
            "· {} char{} ",
            thousands(count as u64),
            if count == 1 { "" } else { "s" }
        ));
    }
    let rule = format!(
        "{heading}{}",
        "─".repeat((rule_area.width as usize).saturating_sub(text_width(&heading)))
    );
    frame.render_widget(Paragraph::new(rule).style(dim()), rule_area);

    let body = match value {
        None => Paragraph::new("NULL").style(dim().add_modifier(Modifier::ITALIC)),
        Some(value) => Paragraph::new(pretty(value)).wrap(Wrap { trim: false }),
    };
    frame.render_widget(body.scroll((record.scroll, 0)), value_area);
}

/// JSON reads better indented; everything else is shown as it is.
fn pretty(value: &str) -> String {
    let trimmed = value.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(value)
        && let Ok(text) = serde_json::to_string_pretty(&json)
    {
        return text;
    }
    value.replace('\r', "").replace('\t', "    ")
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
                ("C", "back to the connection list"),
                ("q", "quit"),
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
                ("r", "run again"),
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
            "SQL editor",
            &[
                ("F5  Ctrl-R", "run"),
                ("Ctrl-L", "reset to the SQL rowboat generated"),
                ("Esc", "close"),
            ],
        ),
    ];

    let section = |(title, keys): &(&'static str, &'static [(&'static str, &'static str)])| -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(Span::styled(format!(" {title}"), bold()))];
        for (key, what) in *keys {
            lines.push(Line::from(vec![
                Span::styled(format!("   {} ", fit(key, 14, false)), accent()),
                Span::raw(*what),
            ]));
        }
        lines.push(Line::default());
        lines
    };
    let note = Line::from(Span::styled(
        " rowboat only reads. Statements that could change anything are refused.",
        dim(),
    ));

    // Two columns when the window is wide enough, otherwise one long one.
    const COLUMN: u16 = 72;
    let wide = area.width >= COLUMN * 2 + 4;
    let (left, right): (Vec<Line>, Vec<Line>) = if wide {
        (
            HELP[..2].iter().flat_map(section).collect(),
            HELP[2..].iter().flat_map(section).chain([note]).collect(),
        )
    } else {
        (
            HELP.iter().flat_map(section).chain([note]).collect(),
            Vec::new(),
        )
    };

    let wanted = left.len().max(right.len()) as u16;
    let width = if wide { COLUMN * 2 + 2 } else { COLUMN + 2 };
    let at = centered(area, width, wanted + 2);
    let room = at.height.saturating_sub(2);
    *scroll = (*scroll).min(wanted.saturating_sub(room));
    let hint = if wanted > room {
        "↑↓ scroll · any other key closes"
    } else {
        "any key closes"
    };
    let inner = popup(frame, at, "Keys", hint);
    if wide {
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Length(COLUMN), Constraint::Min(1)]).areas(inner);
        frame.render_widget(Paragraph::new(left).scroll((*scroll, 0)), left_area);
        frame.render_widget(Paragraph::new(right).scroll((*scroll, 0)), right_area);
    } else {
        frame.render_widget(Paragraph::new(left).scroll((*scroll, 0)), inner);
    }
}

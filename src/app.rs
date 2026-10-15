//! Application state and key handling. Drawing lives in [`crate::ui`].
//!
//! Anything slow (1Password, connecting, queries) runs as a background job
//! and reports back through a channel, so the interface never blocks and a
//! running job can always be cancelled with Esc.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

use crate::clipboard;
use crate::config::{self, Config};
use crate::db::meta::{self, DbObject, StructureTab};
use crate::db::{ResultColumn, ResultSet, Session};
use crate::input::{Editor, Input};
use crate::op;
use crate::query::{ColumnInfo, Filter, Mode, Op, TableQuery};
use crate::ui;

pub enum Msg {
    Term(Event),
    Progress(u64, String),
    Connected(u64, Result<Connected, String>),
    Done(u64, Output),
}

/// What a workspace's background job hands back. Errors are already text
/// for display.
pub enum Output {
    Opened(Result<Box<Opened>, String>),
    Page(Result<ResultSet, String>),
    Count(Result<u64, String>),
    Structure(usize, Result<ResultSet, String>),
}

pub struct Connected {
    session: Arc<Session>,
    objects: Vec<DbObject>,
}

pub struct Opened {
    object: DbObject,
    tabs: Vec<StructureTab>,
    columns: ResultSet,
    query: TableQuery,
    sql: String,
    page: Result<ResultSet, String>,
}

/// A running background job. Dropping it cancels the work, which closes its
/// connection mid-statement.
pub struct Job {
    id: u64,
    handle: AbortHandle,
    pub label: String,
    pub started: Instant,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Clone, Copy)]
enum Slot {
    /// Opening a table, loading a page. One at a time.
    Main,
    /// The row count in the status bar.
    Count,
    /// Structure tabs.
    Aux,
}

/// A workspace's jobs, one per [`Slot`].
#[derive(Default)]
struct Jobs {
    main: Option<Job>,
    count: Option<Job>,
    aux: Option<Job>,
}

impl Jobs {
    fn slot(&mut self, slot: Slot) -> &mut Option<Job> {
        match slot {
            Slot::Main => &mut self.main,
            Slot::Count => &mut self.count,
            Slot::Aux => &mut self.aux,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Picker,
    Workspace,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Main,
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    until: Instant,
}

#[derive(Default)]
pub struct Picker {
    pub filter: Input,
    pub filtering: bool,
    pub selected: usize,
    pub error: Option<String>,
    /// The connection picked takes the place of the one on screen.
    pub replace: bool,
}

/// A result laid out as a grid, with a cell cursor.
pub struct Grid {
    pub result: ResultSet,
    pub row: usize,
    pub col: usize,
    pub row_off: usize,
    pub col_off: usize,
    pub widths: Vec<usize>,
    /// How many rows fit on screen; the renderer keeps this current.
    pub page_rows: usize,
}

impl Grid {
    pub fn new(result: ResultSet, max_width: usize) -> Grid {
        const SAMPLE: usize = 300;
        let max_width = max_width.max(4);
        let widths = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, column)| {
                let header = ui::text_width(&column.name) + 2;
                let widest = result
                    .rows
                    .iter()
                    .take(SAMPLE)
                    .map(|row| match row.get(i) {
                        Some(Some(value)) => ui::cell_width(value, max_width),
                        _ => 4,
                    })
                    .max()
                    .unwrap_or(0);
                header.max(widest).clamp(3, max_width.max(header.min(60)))
            })
            .collect();
        Grid {
            result,
            row: 0,
            col: 0,
            row_off: 0,
            col_off: 0,
            widths,
            page_rows: 20,
        }
    }

    pub fn rows(&self) -> usize {
        self.result.rows.len()
    }

    pub fn cols(&self) -> usize {
        self.result.columns.len()
    }

    pub fn column(&self) -> Option<&ResultColumn> {
        self.result.columns.get(self.col)
    }

    /// The value under the cursor: `None` when there is no cell, `Some(None)`
    /// for NULL.
    pub fn value(&self) -> Option<&Option<String>> {
        self.result.rows.get(self.row)?.get(self.col)
    }

    fn step_rows(&mut self, delta: isize) {
        let last = self.rows().saturating_sub(1);
        self.row = self.row.saturating_add_signed(delta).min(last);
    }

    fn step_cols(&mut self, delta: isize) {
        let last = self.cols().saturating_sub(1);
        self.col = self.col.saturating_add_signed(delta).min(last);
    }

    /// Cursor movement. Returns whether the key was a movement key.
    pub fn navigate(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.page_rows.max(2) as isize;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.step_rows(1),
            KeyCode::Up | KeyCode::Char('k') => self.step_rows(-1),
            KeyCode::Left | KeyCode::Char('h') => self.step_cols(-1),
            KeyCode::Right | KeyCode::Char('l') => self.step_cols(1),
            KeyCode::PageDown => self.step_rows(page - 1),
            KeyCode::PageUp => self.step_rows(1 - page),
            KeyCode::Char('d') if ctrl => self.step_rows(page / 2),
            KeyCode::Char('u') if ctrl => self.step_rows(-(page / 2)),
            KeyCode::Home | KeyCode::Char('g') => self.row = 0,
            KeyCode::End | KeyCode::Char('G') => self.row = self.rows().saturating_sub(1),
            KeyCode::Char('0') => self.col = 0,
            KeyCode::Char('$') => self.col = self.cols().saturating_sub(1),
            _ => return false,
        }
        true
    }
}

pub enum Pane {
    Empty,
    Loading,
    Error(String),
    Grid(Grid),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Total {
    Unknown,
    Counting,
    Known(u64),
    Unavailable,
}

#[derive(Default)]
pub struct Sidebar {
    pub filter: Input,
    pub filtering: bool,
    pub selected: usize,
    pub offset: usize,
}

/// The table currently open, with its query and structure tabs.
pub struct TableCtx {
    pub object: DbObject,
    pub query: TableQuery,
    pub tabs: Vec<StructureTab>,
    pub panes: Vec<Pane>,
    /// Columns picked with Space, for multi-column duplicate checks.
    pub marked: BTreeSet<String>,
}

pub struct Workspace {
    pub session: Arc<Session>,
    pub objects: Vec<DbObject>,
    pub side: Sidebar,
    pub focus: Focus,
    pub table: Option<TableCtx>,
    /// Hand-written SQL currently shown in the Data tab, if any.
    pub custom: Option<String>,
    /// 0 is Data; the rest index `TableCtx::tabs`, offset by one.
    pub tab: usize,
    pub data: Pane,
    /// The SQL behind the Data tab.
    pub sql: String,
    pub total: Total,
    /// Earlier states of the query, for Backspace.
    pub back: Vec<TableQuery>,
    draft: Option<String>,
    jobs: Jobs,
}

impl Workspace {
    /// The job the user is waiting on, if any.
    pub fn busy(&self) -> Option<&Job> {
        self.jobs.main.as_ref().or(self.jobs.aux.as_ref())
    }

    /// Indexes into `objects` that match the sidebar filter.
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.side.filter.text().to_lowercase();
        self.objects
            .iter()
            .enumerate()
            .filter(|(_, object)| {
                needle.is_empty() || object.display_name().to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn tab_titles(&self) -> Vec<&'static str> {
        let mut titles = vec!["Data"];
        if let Some(table) = &self.table {
            titles.extend(table.tabs.iter().map(|t| t.title));
        }
        titles
    }

    pub fn pane(&self) -> &Pane {
        match (self.tab, &self.table) {
            (0, _) | (_, None) => &self.data,
            (n, Some(table)) => table.panes.get(n - 1).unwrap_or(&self.data),
        }
    }

    pub fn pane_mut(&mut self) -> &mut Pane {
        match (self.tab, &mut self.table) {
            (n, Some(table)) if n > 0 && n <= table.panes.len() => &mut table.panes[n - 1],
            _ => &mut self.data,
        }
    }

    pub fn grid(&self) -> Option<&Grid> {
        match self.pane() {
            Pane::Grid(grid) => Some(grid),
            _ => None,
        }
    }

    fn grid_mut(&mut self) -> Option<&mut Grid> {
        match self.pane_mut() {
            Pane::Grid(grid) => Some(grid),
            _ => None,
        }
    }

    /// True when the Data tab shows the open table through the query
    /// builder, as opposed to hand-written SQL.
    pub fn builder_active(&self) -> bool {
        self.tab == 0 && self.table.is_some() && self.custom.is_none()
    }

    pub fn groups(&self) -> Option<&[String]> {
        match &self.table.as_ref()?.query.mode {
            Mode::Groups { columns, .. } if self.custom.is_none() => Some(columns),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FilterField {
    Column,
    Operator,
    Value,
}

pub struct FilterDialog {
    pub column: Input,
    pub op: usize,
    pub value: Input,
    pub focus: FilterField,
    pub pick: usize,
}

impl FilterDialog {
    pub fn op(&self) -> Op {
        Op::ALL[self.op % Op::ALL.len()]
    }

    /// Columns matching what has been typed, best match first.
    pub fn matches<'a>(&self, columns: &'a [ColumnInfo]) -> Vec<&'a ColumnInfo> {
        let needle = self.column.text().trim().to_lowercase();
        let mut found: Vec<&ColumnInfo> = columns
            .iter()
            .filter(|c| needle.is_empty() || c.name.to_lowercase().contains(&needle))
            .collect();
        // A name that starts with the text beats one that merely contains it.
        found.sort_by_key(|c| {
            let name = c.name.to_lowercase();
            (name != needle, !name.starts_with(&needle))
        });
        found
    }

    pub fn chosen<'a>(&self, columns: &'a [ColumnInfo]) -> Option<&'a ColumnInfo> {
        let matches = self.matches(columns);
        matches
            .get(self.pick.min(matches.len().saturating_sub(1)))
            .copied()
    }
}

pub struct Record {
    pub field: usize,
    pub offset: usize,
    pub scroll: u16,
}

pub enum Overlay {
    None,
    Help { scroll: u16 },
    Filter(FilterDialog),
    Filters { selected: usize },
    Record(Record),
    Editor(Editor),
}

pub struct App {
    pub cfg: Arc<Config>,
    pub cfg_path: PathBuf,
    pub screen: Screen,
    pub picker: Picker,
    /// One per open connection, in the order they were opened.
    pub workspaces: Vec<Workspace>,
    /// The workspace on screen, or the one Esc on the picker goes back to.
    pub current: usize,
    pub overlay: Overlay,
    pub toast: Option<Toast>,
    pub quit: bool,
    pub tick: usize,
    connecting: Option<Job>,
    tx: UnboundedSender<Msg>,
    next_id: u64,
}

fn is_ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
}

/// F5 or Ctrl-R: run the SQL in the editor, or run the data view again.
fn is_run(key: &KeyEvent) -> bool {
    key.code == KeyCode::F(5) || (is_ctrl(key) && key.code == KeyCode::Char('r'))
}

impl App {
    pub fn new(
        cfg: Config,
        cfg_path: PathBuf,
        cfg_error: Option<String>,
        tx: UnboundedSender<Msg>,
    ) -> App {
        App {
            cfg: Arc::new(cfg),
            cfg_path,
            screen: Screen::Picker,
            picker: Picker {
                error: cfg_error,
                ..Picker::default()
            },
            workspaces: Vec::new(),
            current: 0,
            overlay: Overlay::None,
            toast: None,
            quit: false,
            tick: 0,
            connecting: None,
            tx,
            next_id: 0,
        }
    }

    pub fn ws(&self) -> Option<&Workspace> {
        self.workspaces.get(self.current)
    }

    fn ws_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(self.current)
    }

    /// The workspace of the open connection called `name`, if any.
    pub fn workspace_of(&self, name: &str) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|ws| ws.session.spec().name.eq_ignore_ascii_case(name))
    }

    /// The job the user is waiting on, if any.
    pub fn busy(&self) -> Option<&Job> {
        match self.screen {
            Screen::Picker => self.connecting.as_ref(),
            Screen::Workspace => self.ws().and_then(Workspace::busy),
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        if self
            .toast
            .as_ref()
            .is_some_and(|t| Instant::now() >= t.until)
        {
            self.toast = None;
        }
    }

    fn say(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            error: false,
            until: Instant::now() + Duration::from_secs(4),
        });
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            error: true,
            until: Instant::now() + Duration::from_secs(6),
        });
    }

    // ---- background jobs --------------------------------------------------

    /// Runs the future `make` builds in the background, and sends the message
    /// it ends with.
    fn start<F, Fut>(&mut self, label: impl Into<String>, make: F) -> Job
    where
        F: FnOnce(u64, UnboundedSender<Msg>) -> Fut,
        Fut: Future<Output = Msg> + Send + 'static,
    {
        self.next_id += 1;
        let id = self.next_id;
        let work = make(id, self.tx.clone());
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let _ = tx.send(work.await);
        })
        .abort_handle();
        Job {
            id,
            handle,
            label: label.into(),
            started: Instant::now(),
        }
    }

    /// Starts a job for workspace `ws`, cancelling the one in its slot.
    fn spawn<Fut>(&mut self, ws: usize, slot: Slot, label: impl Into<String>, work: Fut)
    where
        Fut: Future<Output = Output> + Send + 'static,
    {
        let job = self.start(label, |id, _| async move { Msg::Done(id, work.await) });
        if let Some(ws) = self.workspaces.get_mut(ws) {
            *ws.jobs.slot(slot) = Some(job);
        }
    }

    fn abort(slot: &mut Option<Job>) -> bool {
        slot.take().is_some()
    }

    fn take_job(slot: &mut Option<Job>, id: u64) -> bool {
        if slot.as_ref().is_some_and(|job| job.id == id) {
            *slot = None;
            true
        } else {
            false
        }
    }

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Term(Event::Key(key)) if key.kind != KeyEventKind::Release => self.on_key(key),
            Msg::Term(Event::Paste(text)) => self.on_paste(&text),
            Msg::Term(_) => {}
            Msg::Progress(id, label) => {
                if let Some(job) = self.connecting.as_mut().filter(|job| job.id == id) {
                    job.label = label;
                }
            }
            // A result from a job that was replaced or cancelled is ignored.
            Msg::Connected(id, result) => {
                if Self::take_job(&mut self.connecting, id) {
                    self.on_connected(result);
                }
            }
            Msg::Done(id, output) => self.on_done(id, output),
        }
    }

    fn on_connected(&mut self, result: Result<Connected, String>) {
        let connected = match result {
            Ok(connected) => connected,
            Err(error) => {
                self.picker.error = Some(error);
                return;
            }
        };
        self.picker.error = None;
        let ws = Workspace {
            session: connected.session,
            objects: connected.objects,
            side: Sidebar::default(),
            focus: Focus::Sidebar,
            table: None,
            custom: None,
            tab: 0,
            data: Pane::Empty,
            sql: String::new(),
            total: Total::Unavailable,
            back: Vec::new(),
            draft: None,
            jobs: Jobs::default(),
        };
        // A replacement goes next to the one it replaces, which then closes.
        let at = if self.picker.replace {
            (self.current + 1).min(self.workspaces.len())
        } else {
            self.workspaces.len()
        };
        self.workspaces.insert(at, ws);
        self.arrive(at);
    }

    /// Shows workspace `i` once a connection has been picked, closing the
    /// one it replaces.
    fn arrive(&mut self, i: usize) {
        let previous = self.current;
        self.current = i;
        self.screen = Screen::Workspace;
        if std::mem::take(&mut self.picker.replace)
            && previous != i
            && previous < self.workspaces.len()
        {
            let closed = self.workspaces.remove(previous);
            if previous < i {
                self.current -= 1;
            }
            self.say(format!("Closed {}", closed.session.spec().name));
        }
    }

    fn on_done(&mut self, id: u64, output: Output) {
        let slot = match &output {
            Output::Opened(_) | Output::Page(_) => Slot::Main,
            Output::Count(_) => Slot::Count,
            Output::Structure(..) => Slot::Aux,
        };
        // The result goes to the workspace that started the job, which need
        // not be the one on screen. One that was replaced or cancelled, or
        // whose connection was closed, is ignored.
        let Some(i) = self
            .workspaces
            .iter_mut()
            .position(|ws| Self::take_job(ws.jobs.slot(slot), id))
        else {
            return;
        };
        match output {
            Output::Opened(Ok(opened)) => self.on_opened(i, *opened),
            Output::Opened(Err(error)) => {
                let ws = &mut self.workspaces[i];
                Self::abort(&mut ws.jobs.count);
                Self::abort(&mut ws.jobs.aux);
                ws.table = None;
                ws.custom = None;
                ws.tab = 0;
                ws.sql.clear();
                ws.back.clear();
                ws.total = Total::Unavailable;
                ws.data = Pane::Error(error);
            }
            Output::Page(result) => self.on_page(i, result),
            Output::Count(result) => {
                self.workspaces[i].total = match result {
                    Ok(count) => Total::Known(count),
                    Err(_) => Total::Unavailable,
                };
            }
            Output::Structure(index, result) => {
                let width = self.cfg.settings.max_column_width as usize;
                if let Some(table) = self.workspaces[i].table.as_mut()
                    && let Some(pane) = table.panes.get_mut(index)
                {
                    *pane = match result {
                        // Definitions are long; let them use the room.
                        Ok(result) => Pane::Grid(Grid::new(result, width.max(120))),
                        Err(error) => Pane::Error(error),
                    };
                }
            }
        }
    }

    // ---- connecting -------------------------------------------------------

    pub fn connect_by_name(&mut self, name: &str) {
        match self
            .cfg
            .connections
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
        {
            Some(index) => self.connect(index),
            None => {
                self.picker.error = Some(format!(
                    "There is no connection called \"{name}\" in {}",
                    self.cfg_path.display()
                ))
            }
        }
    }

    fn connect(&mut self, index: usize) {
        let Some(connection) = self.cfg.connections.get(index).cloned() else {
            return;
        };
        // One that is open already is switched to, not opened twice.
        if let Some(open) = self.workspace_of(&connection.name) {
            Self::abort(&mut self.connecting);
            self.arrive(open);
            return;
        }
        let cfg = self.cfg.clone();
        self.picker.error = None;
        let label = format!("Connecting to {}", connection.name);
        let job = self.start(label, move |id, tx| async move {
            let result: anyhow::Result<Connected> = async {
                let uses_op = connection.item.is_some()
                    || [
                        &connection.host,
                        &connection.username,
                        &connection.password,
                        &connection.database,
                    ]
                    .iter()
                    .any(|v| v.as_deref().is_some_and(|v| v.starts_with("op://")));
                if uses_op {
                    let _ = tx.send(Msg::Progress(id, "Waiting for 1Password".into()));
                }
                let spec = op::resolve(&connection, &cfg.settings).await?;
                let _ = tx.send(Msg::Progress(
                    id,
                    format!("Connecting to {}", spec.target()),
                ));
                let session = Session::connect(spec, cfg.settings.query_timeout()).await?;
                let _ = tx.send(Msg::Progress(id, "Loading tables".into()));
                let listing = session
                    .catalog_query(
                        meta::objects_sql(session.engine()),
                        cfg.settings.custom_limits(),
                    )
                    .await?;
                Ok(Connected {
                    objects: meta::parse_objects(&listing),
                    session,
                })
            }
            .await;
            Msg::Connected(id, result.map_err(|e| format!("{e:#}")))
        });
        self.connecting = Some(job);
    }

    /// Shows the connection list. What is open stays open, unless `replace`
    /// says the connection picked takes the place of the one on screen.
    fn show_picker(&mut self, replace: bool) {
        self.picker.filtering = false;
        self.picker.filter.clear();
        self.picker.error = None;
        self.picker.replace = replace && self.ws().is_some();
        self.screen = Screen::Picker;
    }

    /// Goes back from the connection list to the open connections, giving up
    /// on any connection still being made.
    fn leave_picker(&mut self) {
        Self::abort(&mut self.connecting);
        self.picker.replace = false;
        self.screen = Screen::Workspace;
    }

    /// Closes the connection on screen, and stops whatever it was running.
    fn close(&mut self) {
        if self.current >= self.workspaces.len() {
            return;
        }
        let closed = self.workspaces.remove(self.current);
        self.current = self.current.min(self.workspaces.len().saturating_sub(1));
        if self.workspaces.is_empty() {
            self.show_picker(false);
        }
        self.say(format!("Closed {}", closed.session.spec().name));
    }

    /// Moves to the next or previous open connection.
    fn cycle(&mut self, forward: bool) {
        let count = self.workspaces.len();
        if count < 2 {
            self.say("This is the only open connection: c opens another");
            return;
        }
        self.current = (self.current + if forward { 1 } else { count - 1 }) % count;
    }

    fn reload_config(&mut self) {
        match config::load(&self.cfg_path) {
            Ok(cfg) => {
                let count = cfg.connections.len();
                self.cfg = Arc::new(cfg);
                self.picker.error = None;
                self.picker.selected = 0;
                self.say(format!("Config reloaded: {count} connections"));
            }
            Err(error) => self.picker.error = Some(format!("{error:#}")),
        }
    }

    // ---- tables and queries -----------------------------------------------

    fn open_selected(&mut self) {
        let Some(ws) = self.ws_mut() else { return };
        let visible = ws.visible();
        let Some(&index) = visible.get(ws.side.selected) else {
            return;
        };
        ws.side.filtering = false;
        let object = ws.objects[index].clone();
        let session = ws.session.clone();
        let cfg = self.cfg.clone();
        let label = format!("Opening {}", object.display_name());
        self.spawn(self.current, Slot::Main, label, async move {
            let engine = session.engine();
            let result: anyhow::Result<Opened> = async {
                let tabs = meta::structure_tabs(engine, &object);
                let columns = session
                    .catalog_query(&tabs[0].sql, cfg.settings.custom_limits())
                    .await?;
                let infos = meta::parse_columns(&columns);
                if infos.is_empty() {
                    anyhow::bail!(
                        "{} has no columns you can see. It may be gone, or this login may lack access.",
                        object.display_name()
                    );
                }
                let query = TableQuery::new(object.table_ref(), infos, cfg.settings.page_size);
                let sql = query.sql(engine);
                let page = session
                    .query(&sql, cfg.settings.page_limits())
                    .await
                    .map_err(|e| format!("{e:#}"));
                Ok(Opened {
                    object,
                    tabs,
                    columns,
                    query,
                    sql,
                    page,
                })
            }
            .await;
            Output::Opened(result.map(Box::new).map_err(|e| format!("{e:#}")))
        });
    }

    fn on_opened(&mut self, i: usize, opened: Opened) {
        let width = self.cfg.settings.max_column_width as usize;
        let Some(ws) = self.workspaces.get_mut(i) else {
            return;
        };
        Self::abort(&mut ws.jobs.count);
        Self::abort(&mut ws.jobs.aux);
        let mut panes: Vec<Pane> = opened.tabs.iter().map(|_| Pane::Empty).collect();
        panes[0] = Pane::Grid(Grid::new(opened.columns, width));
        ws.table = Some(TableCtx {
            object: opened.object,
            query: opened.query,
            tabs: opened.tabs,
            panes,
            marked: BTreeSet::new(),
        });
        ws.custom = None;
        ws.tab = 0;
        ws.sql = opened.sql;
        ws.back.clear();
        ws.total = Total::Unknown;
        ws.focus = Focus::Main;
        ws.data = Pane::Empty;
        self.on_page(i, opened.page);
    }

    fn on_page(&mut self, i: usize, result: Result<ResultSet, String>) {
        let width = self.cfg.settings.max_column_width as usize;
        let Some(ws) = self.workspaces.get_mut(i) else {
            return;
        };
        match result {
            Ok(result) => {
                let mut grid = Grid::new(result, width);
                // Keep the cursor on the same column across pages and sorts.
                if let Pane::Grid(old) = &ws.data
                    && old.cols() == grid.cols()
                {
                    grid.col = old.col;
                    grid.col_off = old.col_off;
                }
                let rows = grid.rows();
                ws.data = Pane::Grid(grid);

                if ws.custom.is_some() || ws.groups().is_some() {
                    ws.total = Total::Unavailable;
                } else if let Some(table) = &ws.table
                    && ws.total == Total::Unknown
                {
                    if table.query.page == 0 && rows < table.query.page_size {
                        // The first page is not full, so it is everything.
                        ws.total = Total::Known(rows as u64);
                    } else {
                        self.start_count(i);
                    }
                }
            }
            Err(error) => ws.data = Pane::Error(error),
        }
    }

    fn start_count(&mut self, i: usize) {
        let limits = self.cfg.settings.count_limits();
        let Some(ws) = self.workspaces.get_mut(i) else {
            return;
        };
        let Some(limits) = limits else {
            ws.total = Total::Unavailable;
            return;
        };
        let Some(sql) = ws
            .table
            .as_ref()
            .and_then(|t| t.query.count_sql(ws.session.engine()))
        else {
            return;
        };
        ws.total = Total::Counting;
        let session = ws.session.clone();
        self.spawn(i, Slot::Count, "Counting rows", async move {
            let result = session.query(&sql, limits).await;
            Output::Count(match result {
                Ok(result) => result
                    .rows
                    .first()
                    .and_then(|row| row.first().cloned().flatten())
                    .and_then(|text| text.parse().ok())
                    .ok_or_else(|| "no count".to_string()),
                Err(e) => Err(format!("{e:#}")),
            })
        });
    }

    /// Runs the open table's query as it now stands.
    fn run_table(&mut self) {
        let limits = self.cfg.settings.page_limits();
        let Some(ws) = self.ws_mut() else { return };
        let Some(table) = &ws.table else { return };
        let sql = table.query.sql(ws.session.engine());
        ws.custom = None;
        ws.tab = 0;
        ws.sql = sql.clone();
        let session = ws.session.clone();
        self.spawn(self.current, Slot::Main, "Running query", async move {
            Output::Page(
                session
                    .query(&sql, limits)
                    .await
                    .map_err(|e| format!("{e:#}")),
            )
        });
    }

    fn run_custom(&mut self, sql: String) {
        if sql.trim().is_empty() {
            self.warn("There is no SQL to run");
            return;
        }
        let limits = self.cfg.settings.custom_limits();
        let Some(ws) = self.ws_mut() else { return };
        Self::abort(&mut ws.jobs.count);
        ws.custom = Some(sql.clone());
        ws.tab = 0;
        ws.sql = sql.clone();
        ws.total = Total::Unavailable;
        let session = ws.session.clone();
        self.spawn(self.current, Slot::Main, "Running SQL", async move {
            Output::Page(
                session
                    .query(&sql, limits)
                    .await
                    .map_err(|e| format!("{e:#}")),
            )
        });
    }

    /// Changes the open table's query and reruns it. `refilter` says the set
    /// of matching rows changed, so the row count is stale.
    fn change_query(&mut self, refilter: bool, change: impl FnOnce(&mut TableQuery)) {
        let Some(ws) = self.ws_mut() else { return };
        let Some(table) = &mut ws.table else { return };
        if refilter {
            table.query.page = 0;
            ws.total = Total::Unknown;
            Self::abort(&mut ws.jobs.count);
        }
        change(&mut table.query);
        self.run_table();
    }

    fn add_filter(&mut self, filter: Filter) {
        let description = filter.describe();
        self.change_query(true, |query| query.filters.push(filter));
        self.say(format!("Filter added: {description}"));
    }

    fn switch_tab(&mut self, index: usize) {
        let limits = self.cfg.settings.custom_limits();
        let Some(ws) = self.ws_mut() else { return };
        if index >= ws.tab_titles().len() {
            return;
        }
        ws.tab = index;
        ws.focus = Focus::Main;
        if index == 0 {
            return;
        }
        let Some(table) = &mut ws.table else { return };
        let pane = &mut table.panes[index - 1];
        if matches!(pane, Pane::Grid(_) | Pane::Error(_)) {
            return;
        }
        *pane = Pane::Loading;
        let sql = table.tabs[index - 1].sql.clone();
        let title = table.tabs[index - 1].title;
        let session = ws.session.clone();
        self.spawn(
            self.current,
            Slot::Aux,
            format!("Loading {}", title.to_lowercase()),
            async move {
                let result = session.catalog_query(&sql, limits).await;
                Output::Structure(index - 1, result.map_err(|e| format!("{e:#}")))
            },
        );
    }

    fn reload(&mut self) {
        let Some(ws) = self.ws_mut() else { return };
        if ws.tab > 0 {
            let tab = ws.tab;
            if let Some(table) = &mut ws.table {
                table.panes[tab - 1] = Pane::Empty;
            }
            self.switch_tab(tab);
        } else if let Some(sql) = ws.custom.clone() {
            self.run_custom(sql);
        } else if ws.table.is_some() {
            ws.total = Total::Unknown;
            self.run_table();
        }
    }

    fn turn_page(&mut self, forward: bool) {
        let Some(ws) = self.ws() else { return };
        if ws.custom.is_some() {
            self.say("Hand-written SQL is not paged; everything it returned is here");
            return;
        }
        let Some(table) = &ws.table else { return };
        let page = table.query.page;
        if !forward {
            if page == 0 {
                self.say("This is the first page");
                return;
            }
            self.change_query(false, |q| q.page -= 1);
            return;
        }
        let shown = ws.grid().map_or(0, Grid::rows);
        let past_end = match ws.total {
            Total::Known(total) => (table.query.offset() + shown) as u64 >= total,
            _ => shown < table.query.page_size,
        };
        if past_end {
            self.say("This is the last page");
        } else {
            self.change_query(false, |q| q.page += 1);
        }
    }

    fn go_back(&mut self) {
        let Some(ws) = self.ws_mut() else { return };
        if ws.custom.is_some() {
            if ws.table.is_some() {
                ws.total = Total::Unknown;
                self.run_table();
            } else {
                self.say("No table is open to go back to");
            }
            return;
        }
        match ws.back.pop() {
            Some(previous) => self.change_query(true, |query| *query = previous),
            None => self.say("Nothing to go back to"),
        }
    }

    fn remember(&mut self) {
        if let Some(ws) = self.ws_mut()
            && let Some(table) = &ws.table
        {
            ws.back.push(table.query.clone());
        }
    }

    /// Shows how often each value (or combination of marked columns) occurs.
    fn group_by(&mut self, duplicates_only: bool) {
        let Some(ws) = self.ws() else { return };
        let (Some(table), Some(grid)) = (&ws.table, ws.grid()) else {
            return;
        };
        let columns: Vec<String> = if table.marked.is_empty() {
            grid.column().map(|c| c.name.clone()).into_iter().collect()
        } else {
            // In table order, which reads better than alphabetical.
            table
                .query
                .columns
                .iter()
                .filter(|c| table.marked.contains(&c.name))
                .map(|c| c.name.clone())
                .collect()
        };
        if columns.is_empty() {
            return;
        }
        let what = columns.join(", ");
        self.remember();
        self.change_query(true, |query| {
            query.mode = Mode::Groups {
                columns,
                duplicates_only,
            };
        });
        self.say(if duplicates_only {
            format!("Duplicates of {what}. Enter shows the rows, Backspace goes back")
        } else {
            format!("Values of {what}. Enter shows the rows, Backspace goes back")
        });
    }

    /// From a duplicates row, shows the rows that share those values.
    fn drill_down(&mut self) {
        let Some(ws) = self.ws() else { return };
        let (Some(columns), Some(grid)) = (ws.groups(), ws.grid()) else {
            return;
        };
        let Some(row) = grid.result.rows.get(grid.row) else {
            return;
        };
        let filters: Vec<Filter> = columns
            .iter()
            .zip(row)
            .map(|(column, value)| match value {
                Some(value) => Filter {
                    column: column.clone(),
                    op: Op::Equals,
                    value: value.clone(),
                },
                None => Filter {
                    column: column.clone(),
                    op: Op::IsNull,
                    value: String::new(),
                },
            })
            .collect();
        self.remember();
        self.change_query(true, |query| {
            query.mode = Mode::Rows;
            query.filters.extend(filters);
        });
    }

    fn quick_filter(&mut self, op: Op) {
        let Some(ws) = self.ws() else { return };
        let Some(grid) = ws.grid() else { return };
        let (Some(column), Some(value)) = (grid.column(), grid.value()) else {
            return;
        };
        let filter = match (value, op) {
            (Some(value), _) => Filter {
                column: column.name.clone(),
                op,
                value: value.clone(),
            },
            (None, Op::Equals) => Filter {
                column: column.name.clone(),
                op: Op::IsNull,
                value: String::new(),
            },
            (None, _) => Filter {
                column: column.name.clone(),
                op: Op::IsNotNull,
                value: String::new(),
            },
        };
        self.add_filter(filter);
    }

    fn yank(&mut self, whole_row: bool) {
        let Some(grid) = self.ws().and_then(Workspace::grid) else {
            return;
        };
        let Some(row) = grid.result.rows.get(grid.row) else {
            return;
        };
        let (text, what) = if whole_row {
            let cells: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or("")).collect();
            (cells.join("\t"), "row")
        } else {
            match row.get(grid.col) {
                Some(Some(value)) => (value.clone(), "cell"),
                _ => (String::new(), "cell"),
            }
        };
        match clipboard::copy(&text) {
            Ok(how) => self.say(format!("Copied {what} ({how})")),
            Err(error) => self.warn(format!("Could not copy: {error}")),
        }
    }

    fn open_editor(&mut self) {
        let Some(ws) = self.ws_mut() else { return };
        let text = ws
            .draft
            .clone()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| ws.sql.clone());
        ws.tab = 0;
        self.overlay = Overlay::Editor(Editor::new(&text));
    }

    fn open_filter(&mut self) {
        let Some(ws) = self.ws() else { return };
        // Start on the column under the cursor, ready for a value.
        let column = ws.grid().and_then(Grid::column).map(|c| c.name.clone());
        let known = ws.table.as_ref().is_some_and(|t| {
            column
                .as_ref()
                .is_some_and(|c| t.query.columns.iter().any(|i| &i.name == c))
        });
        self.overlay = Overlay::Filter(FilterDialog {
            column: Input::new(if known {
                column.as_deref().unwrap_or("")
            } else {
                ""
            }),
            op: 0,
            value: Input::default(),
            focus: if known {
                FilterField::Value
            } else {
                FilterField::Column
            },
            pick: 0,
        });
    }

    // ---- keys -------------------------------------------------------------

    fn on_paste(&mut self, text: &str) {
        match &mut self.overlay {
            Overlay::Editor(editor) => editor.insert_str(text),
            Overlay::Filter(dialog) => match dialog.focus {
                FilterField::Column => dialog.column.insert_str(text),
                FilterField::Value => dialog.value.insert_str(text),
                FilterField::Operator => {}
            },
            Overlay::None => match self.screen {
                Screen::Picker if self.picker.filtering => self.picker.filter.insert_str(text),
                Screen::Picker => {}
                Screen::Workspace => {
                    if let Some(ws) = self.workspaces.get_mut(self.current)
                        && ws.side.filtering
                    {
                        ws.side.filter.insert_str(text);
                        ws.side.selected = 0;
                    }
                }
            },
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if is_ctrl(&key) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::None => match self.screen {
                Screen::Picker => self.picker_key(key),
                Screen::Workspace => self.workspace_key(key),
            },
            // Help scrolls if it has to; any other key closes it.
            Overlay::Help { scroll } => {
                let scroll = match key.code {
                    KeyCode::Down | KeyCode::Char('j') => scroll.saturating_add(1),
                    KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
                    KeyCode::PageDown => scroll.saturating_add(10),
                    KeyCode::PageUp => scroll.saturating_sub(10),
                    _ => return,
                };
                self.overlay = Overlay::Help { scroll };
            }
            Overlay::Filter(dialog) => self.filter_key(dialog, key),
            Overlay::Filters { selected } => self.filters_key(selected, key),
            Overlay::Record(record) => self.record_key(record, key),
            Overlay::Editor(editor) => self.editor_key(editor, key),
        }
    }

    fn cancel_job(&mut self) -> bool {
        let job = match self.screen {
            Screen::Picker => &mut self.connecting,
            Screen::Workspace => match self.workspaces.get_mut(self.current) {
                Some(ws) => &mut ws.jobs.main,
                None => return false,
            },
        };
        if Self::abort(job) {
            self.say("Cancelled");
            true
        } else {
            false
        }
    }

    fn picker_key(&mut self, key: KeyEvent) {
        let shown = self.picker_visible();
        let last = shown.len().saturating_sub(1);
        if self.picker.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.picker.filtering = false;
                    self.picker.filter.clear();
                    self.picker.selected = 0;
                }
                KeyCode::Enter => {
                    self.picker.filtering = false;
                    if let Some(&index) = shown.get(self.picker.selected) {
                        self.connect(index);
                    }
                }
                KeyCode::Down => self.picker.selected = (self.picker.selected + 1).min(last),
                KeyCode::Up => self.picker.selected = self.picker.selected.saturating_sub(1),
                _ => {
                    if self.picker.filter.handle(key) {
                        self.picker.selected = 0;
                    }
                }
            }
            return;
        }
        match key.code {
            KeyCode::Esc if self.cancel_job() => {}
            KeyCode::Esc if self.picker.filter.is_empty() && !self.workspaces.is_empty() => {
                self.leave_picker()
            }
            KeyCode::Esc => {
                self.picker.filter.clear();
                self.picker.selected = 0;
            }
            // q closes what is on screen: this list, back to the open
            // connections, or kikkert itself when none are open.
            KeyCode::Char('q') if !self.workspaces.is_empty() => self.leave_picker(),
            KeyCode::Char('q' | 'Q') => self.quit = true,
            KeyCode::Char('?') => self.overlay = Overlay::Help { scroll: 0 },
            KeyCode::Char('/') => self.picker.filtering = true,
            KeyCode::Char('r') => self.reload_config(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.picker.selected = (self.picker.selected + 1).min(last)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.picker.selected = self.picker.selected.saturating_sub(1)
            }
            KeyCode::Home | KeyCode::Char('g') => self.picker.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.picker.selected = last,
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(&index) = shown.get(self.picker.selected) {
                    self.connect(index);
                }
            }
            _ => {}
        }
    }

    /// Indexes of the connections matching the picker's filter.
    pub fn picker_visible(&self) -> Vec<usize> {
        let needle = self.picker.filter.text().to_lowercase();
        self.cfg
            .connections
            .iter()
            .enumerate()
            .filter(|(_, c)| needle.is_empty() || c.name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    }

    fn workspace_key(&mut self, key: KeyEvent) {
        let typing = self
            .ws()
            .is_some_and(|ws| ws.focus == Focus::Sidebar && ws.side.filtering);
        if key.code == KeyCode::Esc && !typing && self.cancel_job() {
            return;
        }
        let Some(ws) = self.ws_mut() else { return };

        if ws.focus == Focus::Sidebar && ws.side.filtering {
            let last = ws.visible().len().saturating_sub(1);
            match key.code {
                KeyCode::Esc => {
                    ws.side.filtering = false;
                    ws.side.filter.clear();
                    ws.side.selected = 0;
                }
                KeyCode::Enter => self.open_selected(),
                KeyCode::Down => ws.side.selected = (ws.side.selected + 1).min(last),
                KeyCode::Up => ws.side.selected = ws.side.selected.saturating_sub(1),
                KeyCode::Tab => ws.side.filtering = false,
                _ => {
                    if ws.side.filter.handle(key) {
                        ws.side.selected = 0;
                    }
                }
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.close(),
            KeyCode::Char('Q') => self.quit = true,
            KeyCode::Char('?') => self.overlay = Overlay::Help { scroll: 0 },
            KeyCode::Char('c') => self.show_picker(false),
            KeyCode::Char('C') => self.show_picker(true),
            KeyCode::Char('r') if !is_ctrl(&key) => self.cycle(true),
            KeyCode::Char('R') => self.cycle(false),
            KeyCode::Char('e') => self.open_editor(),
            KeyCode::Char('t') => {
                ws.focus = Focus::Sidebar;
                ws.side.filtering = true;
                ws.side.filter.clear();
                ws.side.selected = 0;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                ws.focus = match ws.focus {
                    Focus::Sidebar => Focus::Main,
                    Focus::Main => Focus::Sidebar,
                }
            }
            KeyCode::Char(c @ '1'..='9') => self.switch_tab(c as usize - '1' as usize),
            _ => match ws.focus {
                Focus::Sidebar => self.sidebar_key(key),
                Focus::Main => self.main_key(key),
            },
        }
    }

    fn sidebar_key(&mut self, key: KeyEvent) {
        let Some(ws) = self.ws_mut() else { return };
        let last = ws.visible().len().saturating_sub(1);
        let side = &mut ws.side;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => side.selected = (side.selected + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => side.selected = side.selected.saturating_sub(1),
            KeyCode::PageDown => side.selected = (side.selected + 15).min(last),
            KeyCode::PageUp => side.selected = side.selected.saturating_sub(15),
            KeyCode::Home | KeyCode::Char('g') => side.selected = 0,
            KeyCode::End | KeyCode::Char('G') => side.selected = last,
            KeyCode::Char('/') => side.filtering = true,
            KeyCode::Esc => {
                side.filter.clear();
                side.selected = 0;
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_selected(),
            _ => {}
        }
    }

    fn main_key(&mut self, key: KeyEvent) {
        let Some(ws) = self.ws_mut() else { return };
        if let Some(grid) = ws.grid_mut()
            && grid.navigate(key)
        {
            return;
        }

        match key.code {
            KeyCode::Enter if ws.tab == 0 && ws.groups().is_some() => self.drill_down(),
            KeyCode::Enter => {
                if let Some(grid) = ws.grid()
                    && grid.rows() > 0
                {
                    self.overlay = Overlay::Record(Record {
                        field: grid.col,
                        offset: 0,
                        scroll: 0,
                    });
                }
            }
            KeyCode::Char('y') => self.yank(false),
            KeyCode::Char('Y') => self.yank(true),
            _ if is_run(&key) => self.reload(),
            KeyCode::Backspace if ws.tab == 0 => self.go_back(),
            KeyCode::Char(']') | KeyCode::Char('n') if ws.tab == 0 => self.turn_page(true),
            KeyCode::Char('[') | KeyCode::Char('p') if ws.tab == 0 => self.turn_page(false),
            KeyCode::Char('/' | 'f' | '=' | '!' | 'F' | 'X' | 's' | 'S' | ' ' | 'd' | 'D') => {
                self.builder_key(key)
            }
            _ => {}
        }
    }

    /// Keys that reshape the query. They only make sense while the query
    /// builder owns the Data tab.
    fn builder_key(&mut self, key: KeyEvent) {
        let Some(ws) = self.ws_mut() else { return };
        if ws.tab != 0 {
            self.say("Press 1 to go to the data first");
            return;
        }
        if ws.custom.is_some() {
            self.say("This is hand-written SQL: e edits it, Backspace returns to the table");
            return;
        }
        if ws.table.is_none() {
            self.say("Open a table first: t jumps to the table list");
            return;
        }
        let in_groups = ws.groups().is_some();
        let column = ws.grid().and_then(Grid::column).map(|c| c.name.clone());

        match key.code {
            KeyCode::Char('F') => self.overlay = Overlay::Filters { selected: 0 },
            KeyCode::Char('X') => {
                self.change_query(true, |q| q.filters.clear());
                self.say("All filters cleared");
            }
            _ if in_groups => {
                self.say("Enter shows the rows behind this line; Backspace goes back to the table")
            }
            KeyCode::Char('/' | 'f') => self.open_filter(),
            KeyCode::Char('=') => self.quick_filter(Op::Equals),
            KeyCode::Char('!') => self.quick_filter(Op::NotEquals),
            KeyCode::Char('d') => self.group_by(true),
            KeyCode::Char('D') => self.group_by(false),
            KeyCode::Char('s' | 'S') => {
                if let Some(column) = column {
                    let additional = key.code == KeyCode::Char('S');
                    self.change_query(false, |q| q.toggle_sort(&column, additional));
                }
            }
            KeyCode::Char(' ') => {
                if let (Some(column), Some(table)) = (column, &mut ws.table)
                    && !table.marked.remove(&column)
                {
                    table.marked.insert(column);
                }
            }
            _ => {}
        }
    }

    fn filter_key(&mut self, mut dialog: FilterDialog, key: KeyEvent) {
        let columns: Vec<ColumnInfo> = self
            .ws()
            .and_then(|ws| ws.table.as_ref())
            .map(|t| t.query.columns.clone())
            .unwrap_or_default();
        let matches = dialog.matches(&columns).len();
        let ops = Op::ALL.len();

        let next = |focus: FilterField, forward: bool| match (focus, forward) {
            (FilterField::Column, true) | (FilterField::Value, false) => FilterField::Operator,
            (FilterField::Operator, true) | (FilterField::Column, false) => FilterField::Value,
            (FilterField::Value, true) | (FilterField::Operator, false) => FilterField::Column,
        };
        // Leaving the column field settles on the highlighted match.
        let settle = |dialog: &mut FilterDialog| match dialog.chosen(&columns) {
            Some(column) => {
                let name = column.name.clone();
                dialog.column.set(&name);
                dialog.pick = 0;
                true
            }
            None => false,
        };

        match key.code {
            KeyCode::Esc => return,
            KeyCode::Tab | KeyCode::BackTab => {
                let forward = key.code == KeyCode::Tab;
                if dialog.focus != FilterField::Column || settle(&mut dialog) {
                    dialog.focus = next(dialog.focus, forward);
                }
            }
            KeyCode::Enter => {
                if !settle(&mut dialog) {
                    self.warn("No column matches that");
                } else if dialog.focus == FilterField::Column
                    || (dialog.focus == FilterField::Operator && dialog.op().needs_value())
                {
                    // Not ready to apply yet: go on to the value.
                    dialog.focus = FilterField::Value;
                } else {
                    self.add_filter(Filter {
                        column: dialog.column.text().to_string(),
                        op: dialog.op(),
                        value: dialog.value.text().to_string(),
                    });
                    return;
                }
            }
            _ => match dialog.focus {
                FilterField::Column => match key.code {
                    KeyCode::Down => dialog.pick = (dialog.pick + 1).min(matches.saturating_sub(1)),
                    KeyCode::Up => dialog.pick = dialog.pick.saturating_sub(1),
                    _ => {
                        if dialog.column.handle(key) {
                            dialog.pick = 0;
                        }
                    }
                },
                FilterField::Operator => match key.code {
                    KeyCode::Down | KeyCode::Right | KeyCode::Char('j' | 'l') => {
                        dialog.op = (dialog.op + 1) % ops
                    }
                    KeyCode::Up | KeyCode::Left | KeyCode::Char('k' | 'h') => {
                        dialog.op = (dialog.op + ops - 1) % ops
                    }
                    _ => {}
                },
                FilterField::Value => match key.code {
                    // The operator is one step away while typing a value.
                    KeyCode::Down => dialog.op = (dialog.op + 1) % ops,
                    KeyCode::Up => dialog.op = (dialog.op + ops - 1) % ops,
                    _ => {
                        dialog.value.handle(key);
                    }
                },
            },
        }
        self.overlay = Overlay::Filter(dialog);
    }

    fn filters_key(&mut self, mut selected: usize, key: KeyEvent) {
        let count = self
            .ws()
            .and_then(|ws| ws.table.as_ref())
            .map_or(0, |t| t.query.filters.len());
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'F') | KeyCode::Enter => return,
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Char('a' | '/') => {
                self.open_filter();
                return;
            }
            KeyCode::Char('c' | 'X') if count > 0 => {
                self.change_query(true, |q| q.filters.clear());
                return;
            }
            KeyCode::Char('d' | 'x') | KeyCode::Delete | KeyCode::Backspace if count > 0 => {
                self.change_query(true, |q| {
                    q.filters.remove(selected.min(q.filters.len() - 1));
                });
                if count == 1 {
                    return;
                }
                selected = selected.min(count - 2);
            }
            _ => {}
        }
        self.overlay = Overlay::Filters { selected };
    }

    fn record_key(&mut self, mut record: Record, key: KeyEvent) {
        let Some(grid) = self.ws().and_then(Workspace::grid) else {
            return;
        };
        let last = grid.cols().saturating_sub(1);
        let rows = grid.rows();
        let shown_field = record.field;
        let mut row_step: isize = 0;
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => return,
            KeyCode::Down | KeyCode::Char('j') => record.field = (record.field + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => record.field = record.field.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => record.field = 0,
            KeyCode::End | KeyCode::Char('G') => record.field = last,
            KeyCode::PageDown => record.scroll = record.scroll.saturating_add(5),
            KeyCode::PageUp => record.scroll = record.scroll.saturating_sub(5),
            // Step through neighbouring rows without leaving the view.
            KeyCode::Right | KeyCode::Char('l') => row_step = 1,
            KeyCode::Left | KeyCode::Char('h') => row_step = -1,
            KeyCode::Char('y') => {
                self.sync_cursor(record.field, 0);
                self.yank(false);
            }
            KeyCode::Char('=') => {
                self.sync_cursor(record.field, 0);
                if self
                    .ws()
                    .is_some_and(|ws| ws.builder_active() && ws.groups().is_none())
                {
                    self.quick_filter(Op::Equals);
                    return;
                }
                self.say("Filtering by value works on a table's data");
            }
            _ => {}
        }
        if record.field != shown_field || row_step != 0 {
            record.scroll = 0;
        }
        if rows > 0 {
            self.sync_cursor(record.field, row_step);
        }
        self.overlay = Overlay::Record(record);
    }

    /// Keeps the grid cursor on what the row view is showing.
    fn sync_cursor(&mut self, field: usize, row_step: isize) {
        if let Some(grid) = self.ws_mut().and_then(Workspace::grid_mut) {
            grid.col = field.min(grid.cols().saturating_sub(1));
            grid.step_rows(row_step);
        }
    }

    fn editor_key(&mut self, mut editor: Editor, key: KeyEvent) {
        let run = is_run(&key)
            || (key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Enter);
        if run {
            let sql = editor.text();
            if let Some(ws) = self.ws_mut() {
                ws.draft = Some(sql.clone());
            }
            self.run_custom(sql);
            self.overlay = Overlay::Editor(editor);
            return;
        }
        match key.code {
            KeyCode::Esc => {
                if !self.cancel_job()
                    && let Some(ws) = self.ws_mut()
                {
                    ws.draft = Some(editor.text());
                    ws.focus = Focus::Main;
                    return;
                }
            }
            KeyCode::Char('l') if is_ctrl(&key) => {
                // Back to what the query builder would run.
                if let Some(ws) = self.ws()
                    && let Some(table) = &ws.table
                {
                    editor = Editor::new(&table.query.sql(ws.session.engine()));
                }
            }
            _ => {
                editor.handle(key);
            }
        }
        self.overlay = Overlay::Editor(editor);
    }
}

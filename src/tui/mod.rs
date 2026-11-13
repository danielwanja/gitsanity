//! Terminal UI: state, input handling and the event loop. Drawing is in `ui`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use crate::git::Progress;
use crate::query::{self, Filter, Summary};
use crate::store::{self, RepoInfo, Stage, Store};
use crate::time::{Bounds, Clock, Preset, Window};

pub mod ui;
pub mod widgets;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Activity,
    Contributors,
    Files,
    Commits,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Overview, Tab::Activity, Tab::Contributors, Tab::Files, Tab::Commits];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Activity => "Activity",
            Tab::Contributors => "Contributors",
            Tab::Files => "Files",
            Tab::Commits => "Commits",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing in the commit search box.
    Search,
    /// Choosing a contributor to filter by.
    PickAuthor,
    Help,
}

#[derive(Clone, Copy)]
pub enum Click {
    Tab(Tab),
    Preset(Preset),
    Shift(i32),
}

pub struct App {
    pub repo_name: String,
    pub repo_path: PathBuf,
    pub ref_name: String,
    pub clock: Clock,
    pub now: i64,
    pub store: Option<Arc<Store>>,
    pub progress: Arc<Progress>,
    pub from_cache: bool,
    pub error: Option<String>,
    pub started: Instant,
    pub ready_after: Option<Duration>,

    pub tab: Tab,
    pub mode: Mode,
    pub bounds: Bounds,
    pub window: Option<Window>,
    pub filter: Filter,
    pub search: String,
    pub picker: String,
    pub picker_sel: usize,
    /// Selected row per tab.
    pub sel: [usize; 5],
    pub summary: Option<Summary>,
    pub summary_ms: f64,
    chart_width: usize,
    dirty: bool,
    /// Click targets recorded by the last draw.
    pub hits: Vec<(Rect, Click)>,
    pub quit: bool,
}

pub enum Update {
    Store(Arc<Store>, bool),
    Failed(String),
}

impl App {
    pub fn new(info: &RepoInfo, clock: Clock, now: i64, progress: Arc<Progress>) -> App {
        App {
            repo_name: info.workdir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            repo_path: info.workdir.clone(),
            ref_name: info.ref_name.clone(),
            clock,
            now,
            store: None,
            progress,
            from_cache: false,
            error: None,
            started: Instant::now(),
            ready_after: None,
            tab: Tab::Overview,
            mode: Mode::Normal,
            bounds: Bounds { first_day: 0, last_day: 0 },
            window: None,
            filter: Filter::default(),
            search: String::new(),
            picker: String::new(),
            picker_sel: 0,
            sel: [0; 5],
            summary: None,
            summary_ms: 0.0,
            chart_width: 120,
            dirty: true,
            hits: Vec::new(),
            quit: false,
        }
    }

    pub fn set_store(&mut self, store: Arc<Store>, from_cache: bool) {
        self.bounds = query::bounds(&store, &self.clock, self.now);
        let preset = self.window.map_or(Preset::All, |w| w.preset);
        if self.window.is_none() || preset == Preset::All {
            self.window = Some(Window::ending(&self.clock, preset, self.bounds.last_day, self.bounds));
        }
        if store.stage == Stage::Lines && self.ready_after.is_none() {
            self.ready_after = Some(self.started.elapsed());
        }
        self.from_cache = from_cache;
        self.store = Some(store);
        self.dirty = true;
    }

    pub fn set_preset(&mut self, preset: Preset) {
        // Keep looking at the same end date when zooming in or out.
        let anchor = match self.window {
            Some(w) if w.preset != Preset::All => w.last_day.min(self.bounds.last_day),
            _ => self.bounds.last_day,
        };
        self.window = Some(Window::ending(&self.clock, preset, anchor, self.bounds));
        self.dirty = true;
    }

    pub fn shift(&mut self, dir: i32) {
        if let Some(next) = self.window.and_then(|w| w.shifted(&self.clock, dir, self.bounds)) {
            self.window = Some(next);
            self.dirty = true;
        }
    }

    fn zoom(&mut self, dir: i32) {
        let Some(w) = self.window else { return };
        let at = Preset::ALL.iter().position(|p| *p == w.preset).unwrap_or(0) as i32;
        let next = (at + dir).clamp(0, Preset::ALL.len() as i32 - 1) as usize;
        self.set_preset(Preset::ALL[next]);
    }

    /// Recompute the summary if the window, filter, search or data changed.
    pub fn refresh(&mut self, chart_width: usize) {
        if chart_width != self.chart_width {
            self.chart_width = chart_width;
            self.dirty = true;
        }
        if !self.dirty {
            return;
        }
        let (Some(store), Some(w)) = (&self.store, &self.window) else { return };
        let t = Instant::now();
        let summary = query::summarize(store, &self.clock, w, self.bounds, &self.filter, &self.search, chart_width);
        self.summary_ms = t.elapsed().as_secs_f64() * 1e3;
        self.summary = Some(summary);
        self.dirty = false;
        for tab in Tab::ALL {
            let i = tab as usize;
            self.sel[i] = self.sel[i].min(self.rows(tab).saturating_sub(1));
        }
    }

    pub fn rows(&self, tab: Tab) -> usize {
        let Some(s) = &self.summary else { return 0 };
        match tab {
            Tab::Contributors => s.authors.len(),
            Tab::Files => s.files.len(),
            Tab::Commits => s.commits.len(),
            _ => 0,
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let rows = self.rows(self.tab);
        let i = self.tab as usize;
        self.sel[i] = (self.sel[i] as i64 + delta).clamp(0, rows.saturating_sub(1) as i64) as usize;
    }

    /// Contributors offered by the picker: those active in the window, filtered by what was typed.
    pub fn picker_rows(&self) -> Vec<u32> {
        let (Some(store), Some(s)) = (&self.store, &self.summary) else { return Vec::new() };
        let needle = self.picker.to_lowercase();
        s.authors
            .iter()
            .map(|a| a.id)
            .filter(|id| {
                let p = &store.people[*id as usize];
                needle.is_empty() || p.name.to_lowercase().contains(&needle) || p.email.to_lowercase().contains(&needle)
            })
            .take(200)
            .collect()
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match self.mode {
            Mode::Help => self.mode = Mode::Normal,
            Mode::Search => match key.code {
                KeyCode::Esc => {
                    self.search.clear();
                    self.mode = Mode::Normal;
                    self.dirty = true;
                }
                KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.search.pop();
                    self.dirty = true;
                }
                KeyCode::Char(c) => {
                    self.search.push(c);
                    self.sel[Tab::Commits as usize] = 0;
                    self.dirty = true;
                }
                _ => {}
            },
            Mode::PickAuthor => match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => {
                    if let Some(id) = self.picker_rows().get(self.picker_sel) {
                        self.filter.author = Some(*id);
                        self.dirty = true;
                    }
                    self.mode = Mode::Normal;
                }
                KeyCode::Up => self.picker_sel = self.picker_sel.saturating_sub(1),
                KeyCode::Down => self.picker_sel += 1,
                KeyCode::Backspace => {
                    self.picker.pop();
                    self.picker_sel = 0;
                }
                KeyCode::Char(c) => {
                    self.picker.push(c);
                    self.picker_sel = 0;
                }
                _ => {}
            },
            Mode::Normal => self.on_normal_key(key),
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char(c @ '1'..='5') => self.tab = Tab::ALL[c as usize - '1' as usize],
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab as usize + 1) % Tab::ALL.len()],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab as usize + Tab::ALL.len() - 1) % Tab::ALL.len()],
            KeyCode::Left | KeyCode::Char('h' | '[') => self.shift(-1),
            KeyCode::Right | KeyCode::Char('l' | ']') => self.shift(1),
            KeyCode::Char('-') => self.zoom(-1),
            KeyCode::Char('+' | '=') => self.zoom(1),
            KeyCode::Char('0') => self.set_preset(Preset::All),
            KeyCode::Char('t') => {
                // Jump back to the most recent period.
                if let Some(w) = self.window {
                    self.window = Some(Window::ending(&self.clock, w.preset, self.bounds.last_day, self.bounds));
                    self.dirty = true;
                }
            }
            KeyCode::Char('d') => self.set_preset(Preset::D1),
            KeyCode::Char('w') => self.set_preset(Preset::D5),
            KeyCode::Char('m') => self.set_preset(Preset::M1),
            KeyCode::Char('s') => self.set_preset(Preset::M6),
            KeyCode::Char('y') => self.set_preset(Preset::Ytd),
            KeyCode::Char('Y') => self.set_preset(Preset::Y1),
            KeyCode::Char('f') => self.set_preset(Preset::Y5),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.move_sel(15),
            KeyCode::PageUp => self.move_sel(-15),
            KeyCode::Home | KeyCode::Char('g') => self.move_sel(i64::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_sel(i64::MAX / 2),
            KeyCode::Char('/') => {
                self.tab = Tab::Commits;
                self.mode = Mode::Search;
            }
            KeyCode::Char('a') => {
                self.picker.clear();
                self.picker_sel = 0;
                self.mode = Mode::PickAuthor;
            }
            KeyCode::Char('x') | KeyCode::Esc => {
                if self.filter.author.take().is_some() || !self.search.is_empty() {
                    self.search.clear();
                    self.dirty = true;
                }
            }
            KeyCode::Enter if self.tab == Tab::Contributors => {
                let picked = self.summary.as_ref().and_then(|s| s.authors.get(self.sel[Tab::Contributors as usize])).map(|a| a.id);
                if picked.is_some() {
                    // Enter on the filtered contributor clears the filter again.
                    self.filter.author = if self.filter.author == picked { None } else { picked };
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    fn on_click(&mut self, x: u16, y: u16) {
        let hit = self.hits.iter().find(|(r, _)| r.contains((x, y).into())).map(|(_, c)| *c);
        match hit {
            Some(Click::Tab(t)) => self.tab = t,
            Some(Click::Preset(p)) => self.set_preset(p),
            Some(Click::Shift(d)) => self.shift(d),
            None => {}
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Load from cache or index in the background, sending each stage as it lands.
pub fn spawn_loader(info: RepoInfo, progress: Arc<Progress>, reindex: bool) -> Receiver<Update> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        if !reindex {
            if let Some(cached) = store::load_cache(&info) {
                progress.phase.store(crate::git::PHASE_DONE, Ordering::Relaxed);
                let _ = tx.send(Update::Store(Arc::new(cached), true));
                return;
            }
        }
        match store::index(&info, &progress, true, |s| {
            let _ = tx.send(Update::Store(s, false));
        }) {
            Ok((store, _)) => {
                if !progress.cancel.load(Ordering::Relaxed) {
                    let _ = store::save_cache(&info, &store);
                }
            }
            Err(e) => {
                let _ = tx.send(Update::Failed(format!("{e:#}")));
            }
        }
    });
    rx
}

pub fn run(info: RepoInfo, clock: Clock, reindex: bool) -> Result<()> {
    let progress = Arc::new(Progress::default());
    let mut app = App::new(&info, clock, unix_now(), progress.clone());
    let rx = spawn_loader(info, progress.clone(), reindex);

    let mut terminal = ratatui::init();
    let _ = ratatui::crossterm::execute!(std::io::stdout(), event::EnableMouseCapture);
    let result = (|| -> Result<()> {
        while !app.quit {
            while let Ok(update) = rx.try_recv() {
                match update {
                    Update::Store(s, cached) => app.set_store(s, cached),
                    Update::Failed(e) => app.error = Some(e),
                }
            }
            terminal.draw(|f| ui::draw(f, &mut app))?;
            // Redraw promptly while indexing so progress stays live.
            let idle = if progress.phase.load(Ordering::Relaxed) == crate::git::PHASE_DONE { 250 } else { 80 };
            if event::poll(Duration::from_millis(idle))? {
                match event::read()? {
                    Event::Key(key) => app.on_key(key),
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => app.on_click(m.column, m.row),
                        MouseEventKind::ScrollDown => app.move_sel(3),
                        MouseEventKind::ScrollUp => app.move_sel(-3),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    progress.cancel.store(true, Ordering::Relaxed);
    let _ = ratatui::crossterm::execute!(std::io::stdout(), event::DisableMouseCapture);
    ratatui::restore();
    result
}

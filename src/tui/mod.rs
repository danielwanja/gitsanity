//! Terminal UI: state, input handling and the event loop. Drawing is in `ui`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::git::Progress;
use crate::graph::Graph;
use crate::query::{self, Filter, Summary};
use crate::store::{self, RefKind, RefRow, RepoInfo, Stage, Store};
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
    Lens,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Overview,
        Tab::Activity,
        Tab::Contributors,
        Tab::Files,
        Tab::Commits,
        Tab::Lens,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Activity => "Activity",
            Tab::Contributors => "Contributors",
            Tab::Files => "Files",
            Tab::Commits => "Commits",
            Tab::Lens => "Lens",
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
    /// Typing in the Lens ref filter.
    RefSearch,
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
    pub sel: [usize; 6],
    // -- Lens
    pub refs: Vec<RefRow>,
    /// None shows every kind.
    pub ref_kind: Option<RefKind>,
    pub ref_filter: String,
    /// Only refs untouched for 90 days or more.
    pub stale_only: bool,
    pub ref_sel: usize,
    /// Keyboard focus in Lens: the graph (true) or the ref list.
    pub graph_focus: bool,
    pub graph: Option<Graph>,
    /// Full name of a ref the user asked to switch to; the event loop acts on it.
    pub switch_to: Option<String>,
    pub summary: Option<Summary>,
    pub summary_ms: f64,
    /// The selected contributor's own summary for the window (Contributors view).
    pub detail: Option<(u32, Summary)>,
    /// Message body of the selected commit, read from the repository on demand.
    pub body: Option<(u32, String)>,
    repo: gix::Repository,
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
            repo_name: info
                .workdir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
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
            bounds: Bounds {
                first_day: 0,
                last_day: 0,
            },
            window: None,
            filter: Filter::default(),
            search: String::new(),
            picker: String::new(),
            picker_sel: 0,
            sel: [0; 6],
            refs: store::list_refs(&info.repo.to_thread_local()),
            ref_kind: None,
            ref_filter: String::new(),
            stale_only: false,
            ref_sel: 0,
            graph_focus: true,
            graph: None,
            switch_to: None,
            summary: None,
            summary_ms: 0.0,
            detail: None,
            body: None,
            repo: info.repo.to_thread_local(),
            chart_width: 120,
            dirty: true,
            hits: Vec::new(),
            quit: false,
        }
    }

    /// Point the app at another ref of the same repository; data follows from the loader.
    pub fn rebind(&mut self, info: &RepoInfo, progress: Arc<Progress>) {
        self.ref_name = info.ref_name.clone();
        self.repo = info.repo.to_thread_local();
        self.progress = progress;
        self.store = None;
        self.summary = None;
        self.detail = None;
        self.body = None;
        self.graph = None;
        self.error = None;
        self.started = Instant::now();
        self.ready_after = None;
        self.sel = [0; 6];
        self.dirty = true;
    }

    /// Re-read branches and tags (they move when someone commits, pulls or tags).
    pub fn reload_refs(&mut self) {
        self.refs = store::list_refs(&self.repo);
    }

    /// Refs shown in Lens after the kind, name and staleness filters.
    pub fn visible_refs(&self) -> Vec<usize> {
        let needle = self.ref_filter.to_lowercase();
        (0..self.refs.len())
            .filter(|i| {
                let r = &self.refs[*i];
                self.ref_kind.is_none_or(|k| k == r.kind)
                    && (needle.is_empty() || r.name.to_lowercase().contains(&needle))
                    && (!self.stale_only || self.now - r.time >= 90 * 86_400)
            })
            .collect()
    }

    /// Build the commit graph the first time Lens is shown for this history.
    pub fn ensure_graph(&mut self) {
        let Some(store) = &self.store else { return };
        if self.graph.as_ref().is_none_or(|g| g.len() != store.len()) {
            self.graph = Some(Graph::new(store));
            self.sel[Tab::Lens as usize] = 0;
            self.follow_window();
        }
    }

    /// Scroll the graph to the newest commit inside the time window.
    fn follow_window(&mut self) {
        let (Some(store), Some(w), Some(g)) = (&self.store, &self.window, &self.graph) else {
            return;
        };
        if let Some(newest) = store.range(w.start, w.end).next_back() {
            self.sel[Tab::Lens as usize] = g.pos[newest] as usize;
        }
    }

    pub fn set_store(&mut self, store: Arc<Store>, from_cache: bool) {
        // Later stages of the same history keep the graph; a different history drops it.
        if self
            .store
            .as_ref()
            .is_some_and(|old| old.head != store.head)
        {
            self.graph = None;
            self.body = None;
        }
        self.bounds = query::bounds(&store, &self.clock, self.now);
        let preset = self.window.map_or(Preset::All, |w| w.preset);
        if self.window.is_none() || preset == Preset::All {
            self.window = Some(Window::ending(
                &self.clock,
                preset,
                self.bounds.last_day,
                self.bounds,
            ));
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
        self.follow_window();
    }

    pub fn shift(&mut self, dir: i32) {
        if let Some(next) = self
            .window
            .and_then(|w| w.shifted(&self.clock, dir, self.bounds))
        {
            self.window = Some(next);
            self.dirty = true;
            self.follow_window();
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
        let (Some(store), Some(w)) = (&self.store, &self.window) else {
            return;
        };
        let t = Instant::now();
        let summary = query::summarize(
            store,
            &self.clock,
            w,
            self.bounds,
            &self.filter,
            &self.search,
            chart_width,
        );
        self.summary_ms = t.elapsed().as_secs_f64() * 1e3;
        self.summary = Some(summary);
        self.detail = None;
        self.dirty = false;
        for tab in Tab::ALL {
            let i = tab as usize;
            self.sel[i] = self.sel[i].min(self.rows(tab).saturating_sub(1));
        }
    }

    /// Summarize the window again for just the selected contributor.
    pub fn load_detail(&mut self, chart_width: usize) {
        let (Some(store), Some(w), Some(s)) = (&self.store, &self.window, &self.summary) else {
            return;
        };
        let Some(id) = s
            .authors
            .get(self.sel[Tab::Contributors as usize])
            .map(|a| a.id)
        else {
            return;
        };
        if self.detail.as_ref().is_some_and(|(have, _)| *have == id) {
            return;
        }
        let filter = Filter {
            author: Some(id),
            path: self.filter.path.clone(),
        };
        let detail = query::summarize(store, &self.clock, w, self.bounds, &filter, "", chart_width);
        self.detail = Some((id, detail));
    }

    /// Read the selected commit's message body.
    pub fn load_body(&mut self) {
        let (Some(store), Some(s)) = (&self.store, &self.summary) else {
            return;
        };
        let selected = match self.tab {
            Tab::Lens => self
                .graph
                .as_ref()
                .and_then(|g| g.order.get(self.sel[Tab::Lens as usize]))
                .copied(),
            _ => s.commits.get(self.sel[Tab::Commits as usize]).copied(),
        };
        let Some(ci) = selected else { return };
        if self.body.as_ref().is_some_and(|(have, _)| *have == ci) {
            return;
        }
        use gix::objs::Find;
        let mut buf = Vec::new();
        let id = gix::ObjectId::from(store.oid[ci as usize]);
        let text = match self.repo.objects.try_find(&id, &mut buf) {
            Ok(Some(data)) => crate::git::message_body(data.data),
            _ => String::new(),
        };
        self.body = Some((ci, text));
    }

    pub fn rows(&self, tab: Tab) -> usize {
        let Some(s) = &self.summary else { return 0 };
        match tab {
            Tab::Contributors => s.authors.len(),
            Tab::Files => s.files.len(),
            Tab::Commits => s.commits.len(),
            Tab::Lens if self.graph_focus => self.graph.as_ref().map_or(0, Graph::len),
            Tab::Lens => self.visible_refs().len(),
            _ => 0,
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let rows = self.rows(self.tab);
        let last = rows.saturating_sub(1) as i64;
        if self.tab == Tab::Lens && !self.graph_focus {
            self.ref_sel = (self.ref_sel as i64).saturating_add(delta).clamp(0, last) as usize;
            return;
        }
        let i = self.tab as usize;
        self.sel[i] = (self.sel[i] as i64).saturating_add(delta).clamp(0, last) as usize;
    }

    /// Contributors offered by the picker: those active in the window, filtered by what was typed.
    pub fn picker_rows(&self) -> Vec<u32> {
        let (Some(store), Some(s)) = (&self.store, &self.summary) else {
            return Vec::new();
        };
        let needle = self.picker.to_lowercase();
        s.authors
            .iter()
            .map(|a| a.id)
            .filter(|id| {
                let p = &store.people[*id as usize];
                needle.is_empty()
                    || p.name.to_lowercase().contains(&needle)
                    || p.email.to_lowercase().contains(&needle)
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
            Mode::RefSearch => match key.code {
                KeyCode::Esc => {
                    self.ref_filter.clear();
                    self.mode = Mode::Normal;
                }
                KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.ref_filter.pop();
                }
                KeyCode::Char(c) => {
                    self.ref_filter.push(c);
                    self.ref_sel = 0;
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

    /// Keys that only mean something in one view. True when the key was used.
    fn on_view_key(&mut self, key: KeyEvent) -> bool {
        match (self.tab, key.code) {
            (Tab::Lens, KeyCode::Char('p')) => self.graph_focus = !self.graph_focus,
            (Tab::Lens, KeyCode::Char('r')) => {
                self.ref_kind = match self.ref_kind {
                    None => Some(RefKind::Branch),
                    Some(RefKind::Branch) => Some(RefKind::Tag),
                    Some(RefKind::Tag) => Some(RefKind::Remote),
                    Some(RefKind::Remote) => None,
                };
                self.ref_sel = 0;
            }
            (Tab::Lens, KeyCode::Char('S')) => {
                self.stale_only = !self.stale_only;
                self.ref_sel = 0;
            }
            (Tab::Lens, KeyCode::Char('/')) => {
                self.graph_focus = false;
                self.mode = Mode::RefSearch;
            }
            (Tab::Lens, KeyCode::Enter) if !self.graph_focus => {
                // Analyse the selected ref instead of the current one.
                if let Some(i) = self.visible_refs().get(self.ref_sel) {
                    self.switch_to = Some(self.refs[*i].full_name.clone());
                }
            }
            (Tab::Lens, KeyCode::Enter) => {
                // Jump from the graph to the ref list and back.
                self.graph_focus = false;
            }
            (Tab::Files, KeyCode::Enter) => {
                // Narrow every view to the selected file.
                let picked = self
                    .summary
                    .as_ref()
                    .and_then(|s| s.files.get(self.sel[Tab::Files as usize]))
                    .map(|f| f.id);
                if let (Some(id), Some(store)) = (picked, &self.store) {
                    self.filter.path = Some(store.paths[id as usize].clone());
                    self.dirty = true;
                }
            }
            (Tab::Files, KeyCode::Backspace | KeyCode::Char('u')) => {
                // Widen the path filter one level: file -> its directory -> parent -> off.
                self.filter.path = match self.filter.path.take() {
                    Some(path) => {
                        let trimmed = path.trim_end_matches('/');
                        trimmed.rfind('/').map(|i| trimmed[..=i].to_owned())
                    }
                    None => {
                        // Nothing set yet: start from the selected file's directory.
                        let picked = self
                            .summary
                            .as_ref()
                            .and_then(|s| s.files.get(self.sel[Tab::Files as usize]))
                            .map(|f| f.id);
                        match (picked, &self.store) {
                            (Some(id), Some(store)) => {
                                let path = &store.paths[id as usize];
                                path.rfind('/').map(|i| path[..=i].to_owned())
                            }
                            _ => None,
                        }
                    }
                };
                self.sel[Tab::Files as usize] = 0;
                self.dirty = true;
            }
            _ => return false,
        }
        true
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        if self.on_view_key(key) {
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char(c @ '1'..='6') => self.tab = Tab::ALL[c as usize - '1' as usize],
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab as usize + 1) % Tab::ALL.len()],
            KeyCode::BackTab => {
                self.tab = Tab::ALL[(self.tab as usize + Tab::ALL.len() - 1) % Tab::ALL.len()]
            }
            KeyCode::Left | KeyCode::Char('h' | '[') => self.shift(-1),
            KeyCode::Right | KeyCode::Char('l' | ']') => self.shift(1),
            KeyCode::Char('-') => self.zoom(-1),
            KeyCode::Char('+' | '=') => self.zoom(1),
            KeyCode::Char('0') => self.set_preset(Preset::All),
            KeyCode::Char('t') => {
                // Jump back to the most recent period.
                if let Some(w) = self.window {
                    self.window = Some(Window::ending(
                        &self.clock,
                        w.preset,
                        self.bounds.last_day,
                        self.bounds,
                    ));
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
                self.filter = Filter::default();
                self.search.clear();
                self.ref_filter.clear();
                self.dirty = true;
            }
            KeyCode::Enter if self.tab == Tab::Contributors => {
                let picked = self
                    .summary
                    .as_ref()
                    .and_then(|s| s.authors.get(self.sel[Tab::Contributors as usize]))
                    .map(|a| a.id);
                if picked.is_some() {
                    // Enter on the filtered contributor clears the filter again.
                    self.filter.author = if self.filter.author == picked {
                        None
                    } else {
                        picked
                    };
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    fn on_click(&mut self, x: u16, y: u16) {
        let hit = self
            .hits
            .iter()
            .find(|(r, _)| r.contains((x, y).into()))
            .map(|(_, c)| *c);
        match hit {
            Some(Click::Tab(t)) => self.tab = t,
            Some(Click::Preset(p)) => self.set_preset(p),
            Some(Click::Shift(d)) => self.shift(d),
            None => {}
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Load from cache or index in the background, sending each stage as it lands.
pub fn spawn_loader(info: RepoInfo, progress: Arc<Progress>, reindex: bool) -> Receiver<Update> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        if !reindex && let Some(cached) = store::load_cache(&info) {
            progress
                .phase
                .store(crate::git::PHASE_DONE, Ordering::Relaxed);
            let _ = tx.send(Update::Store(Arc::new(cached), true));
            return;
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

pub fn run(mut info: RepoInfo, clock: Clock, reindex: bool) -> Result<()> {
    let mut progress = Arc::new(Progress::default());
    let mut app = App::new(&info, clock, unix_now(), progress.clone());
    let mut rx = spawn_loader(info.clone(), progress.clone(), reindex);
    let mut last_check = Instant::now();

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

            // The user picked another ref in Lens: analyse that one instead.
            if let Some(name) = app.switch_to.take() {
                match store::open(&info.workdir, Some(&name)) {
                    Ok(next) => {
                        progress.cancel.store(true, Ordering::Relaxed);
                        progress = Arc::new(Progress::default());
                        app.rebind(&next, progress.clone());
                        rx = spawn_loader(next.clone(), progress.clone(), false);
                        info = next;
                    }
                    Err(e) => app.error = Some(format!("{e:#}")),
                }
            }

            // Live refresh: when the ref moves (commit, pull, rebase), read the
            // history again in the background and keep showing the old data meanwhile.
            let idle = progress.phase.load(Ordering::Relaxed) == crate::git::PHASE_DONE;
            if idle && last_check.elapsed() >= Duration::from_secs(2) {
                last_check = Instant::now();
                app.now = unix_now();
                if let Some(tip) = info.current_tip().filter(|tip| *tip != info.head) {
                    info.head = tip;
                    progress = Arc::new(Progress::default());
                    app.progress = progress.clone();
                    app.started = Instant::now();
                    app.ready_after = None;
                    app.reload_refs();
                    rx = spawn_loader(info.clone(), progress.clone(), false);
                }
            }

            terminal.draw(|f| ui::draw(f, &mut app))?;
            // Redraw promptly while indexing so progress stays live.
            if event::poll(Duration::from_millis(if idle { 250 } else { 80 }))? {
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

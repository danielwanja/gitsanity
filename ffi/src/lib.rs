//! Swift bindings for the Gitsanity engine.
//!
//! One rule shapes this interface: the big columns stay on this side and
//! Swift gets small answers. A summary carries totals, series and the top of
//! each list; the long lists (contributors, files, commits, graph rows) are
//! fetched a page at a time. All date arithmetic happens here, in the time
//! zone the caller names, so every client buckets days the same way.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gitsanity::git::{self, FLAG_BINARY, KIND_ADD, KIND_DEL, KIND_REN, Progress};
use gitsanity::graph::Graph;
use gitsanity::query::{self, Filter};
use gitsanity::store::{self, COMMIT_TYPES, RefKind, RepoInfo, Stage, Store};
use gitsanity::time::{Bounds, Bucket, Buckets, Clock, Preset, Window};

uniffi::setup_scaffolding!();

#[derive(Debug, uniffi::Error)]
#[uniffi(flat_error)]
pub enum EngineError {
    Failed(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let EngineError::Failed(message) = self;
        f.write_str(message)
    }
}

impl std::error::Error for EngineError {}

pub(crate) fn failed(e: impl std::fmt::Display) -> EngineError {
    EngineError::Failed(format!("{e:#}"))
}

type Res<T> = Result<T, EngineError>;

// ------------------------------------------------------------------ records

#[derive(uniffi::Record)]
pub struct RepoSummary {
    pub name: String,
    pub path: String,
    pub ref_name: String,
    pub head: String,
}

/// How far along loading is. `stage` says what the data can already answer:
/// 0 nothing yet, 1 commits, 2 plus files, 3 plus line counts.
#[derive(uniffi::Record)]
pub struct ProgressSnapshot {
    pub stage: u8,
    /// 0 reading commits, 1 finding changed files, 2 counting lines, 3 done.
    pub phase: u8,
    pub commits: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub lines_done: u64,
    pub lines_total: u64,
}

#[derive(uniffi::Record)]
pub struct IndexReport {
    pub commits: u64,
    pub from_cache: bool,
    pub seconds: f64,
}

/// A range of whole local days. `start..end` is the same range in epoch seconds.
#[derive(uniffi::Record, Clone)]
pub struct WindowInfo {
    /// `1D 5D 1M 6M YTD 1Y 5Y All`
    pub preset: String,
    pub first_day: i64,
    pub last_day: i64,
    pub start: i64,
    pub end: i64,
    /// `Oct 13 – Nov 12, 2026`
    pub label: String,
    pub days: i64,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

/// What to summarise: a window, optional filters, and how to bucket it.
#[derive(uniffi::Record, Clone)]
pub struct Query {
    pub preset: String,
    pub first_day: i64,
    pub last_day: i64,
    /// Only this contributor (an id from [`Contributor`]).
    pub author: Option<u32>,
    /// Only this file (`src/a.rs`) or folder (`src/`).
    pub path: Option<String>,
    /// Matches subject, author, sha prefix or `YYYY-MM-DD`.
    pub search: String,
    /// The finest bucket that fits this many points is used...
    pub max_buckets: u32,
    /// ...unless a size is forced: hour, day, week, month, quarter, year.
    pub bucket: Option<String>,
    /// IANA zone such as `America/Denver`.
    pub time_zone: String,
}

#[derive(uniffi::Record, Clone, Copy)]
pub struct Totals {
    pub commits: u64,
    pub contributors: u64,
    pub files: u64,
    pub added: u64,
    pub removed: u64,
}

#[derive(uniffi::Record)]
pub struct BucketPoint {
    /// Epoch seconds of the bucket's local start.
    pub start: i64,
    pub label: String,
    pub commits: u64,
    pub added: u64,
    pub removed: u64,
}

/// A directory or file type.
#[derive(uniffi::Record)]
pub struct NamedValue {
    pub name: String,
    pub lines: u64,
    pub touches: u64,
    pub files: u32,
}

#[derive(uniffi::Record)]
pub struct TypeCount {
    pub label: String,
    pub count: u64,
    pub lines: u64,
}

#[derive(uniffi::Record)]
pub struct Contributor {
    pub id: u32,
    pub name: String,
    pub email: String,
    pub is_bot: bool,
    pub commits: u32,
    pub added: u64,
    pub removed: u64,
    pub first: i64,
    pub last: i64,
}

#[derive(uniffi::Record)]
pub struct FileStat {
    pub id: u32,
    pub path: String,
    pub touches: u32,
    pub added: u64,
    pub removed: u64,
    pub last: i64,
}

#[derive(uniffi::Record)]
pub struct CommitRow {
    /// Stable handle for [`Repo::commit`].
    pub index: u32,
    pub sha: String,
    /// Author time, epoch seconds.
    pub time: i64,
    pub author: String,
    pub author_id: u32,
    pub subject: String,
    pub added: u32,
    pub removed: u32,
    pub files: u32,
    pub is_merge: bool,
    /// feat, fix, docs, ..., merge, revert, other
    pub kind: String,
}

#[derive(uniffi::Record)]
pub struct Summary {
    pub window: WindowInfo,
    pub stage: u8,
    pub totals: Totals,
    /// The window just before this one; None for "All".
    pub previous: Option<Totals>,
    pub merges: u64,
    /// Local day number (days since 1970-01-01) of the busiest day, by lines.
    pub busiest_day: Option<i64>,
    pub busiest_day_lines: u64,
    pub bucket: String,
    pub series: Vec<BucketPoint>,
    /// Commits per local day, starting at `window.first_day`.
    pub daily: Vec<u32>,
    /// Commits by weekday (Mon first) and hour: 7 × 24 values, row by row.
    pub rhythm: Vec<u32>,
    pub commit_types: Vec<TypeCount>,
    /// added, modified, deleted, renamed, binary
    pub change_kinds: Vec<TypeCount>,
    pub contributors_total: u32,
    pub files_listed: u32,
    pub commits_total: u32,
    pub top_contributors: Vec<Contributor>,
    pub top_files: Vec<FileStat>,
    pub directories: Vec<NamedValue>,
    pub file_types: Vec<NamedValue>,
    pub largest_commits: Vec<CommitRow>,
    pub recent_commits: Vec<CommitRow>,
    pub query_ms: f64,
}

#[derive(uniffi::Record)]
pub struct ChangedFile {
    pub path: String,
    /// A, M, D or R
    pub kind: String,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
}

#[derive(uniffi::Record)]
pub struct CommitDetail {
    pub row: CommitRow,
    pub oid: String,
    pub author_email: String,
    pub committer: String,
    pub commit_time: i64,
    /// The author's own UTC offset, for "on the author's clock".
    pub author_offset_minutes: i32,
    pub parents: u32,
    pub body: String,
    pub files: Vec<ChangedFile>,
    pub files_total: u32,
}

#[derive(uniffi::Record)]
pub struct RefInfo {
    /// branch, tag or remote
    pub kind: String,
    pub name: String,
    pub full_name: String,
    pub sha: String,
    pub time: i64,
    pub is_current: bool,
}

#[derive(uniffi::Record)]
pub struct GraphCell {
    pub glyph: String,
    pub color: u8,
}

#[derive(uniffi::Record)]
pub struct GraphRow {
    pub commit: CommitRow,
    /// Two cells per lane.
    pub cells: Vec<GraphCell>,
    /// Branches and tags that point at this commit.
    pub labels: Vec<String>,
}

// ------------------------------------------------------------------- handle

struct Cached {
    key: String,
    summary: Arc<query::Summary>,
    window: Window,
}

#[derive(uniffi::Object)]
pub struct Repo {
    pub(crate) info: RwLock<RepoInfo>,
    progress: RwLock<Arc<Progress>>,
    pub(crate) store: RwLock<Option<Arc<Store>>>,
    cached: Mutex<Option<Cached>>,
    graph: Mutex<Option<Graph>>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn preset(name: &str) -> Res<Preset> {
    Preset::ALL
        .into_iter()
        .find(|p| p.label().eq_ignore_ascii_case(name))
        .ok_or_else(|| failed(format!("no range named {name}")))
}

fn bucket(name: &str) -> Res<Bucket> {
    [
        Bucket::Hour,
        Bucket::Day,
        Bucket::Week,
        Bucket::Month,
        Bucket::Quarter,
        Bucket::Year,
    ]
    .into_iter()
    .find(|b| b.label().eq_ignore_ascii_case(name))
    .ok_or_else(|| failed(format!("no bucket size named {name}")))
}

fn window_info(clock: &Clock, w: &Window, bounds: Bounds) -> WindowInfo {
    WindowInfo {
        preset: w.preset.label().to_owned(),
        first_day: w.first_day,
        last_day: w.last_day,
        start: w.start,
        end: w.end,
        label: w.label(),
        days: w.days(),
        can_go_back: w.shifted(clock, -1, bounds).is_some(),
        can_go_forward: w.shifted(clock, 1, bounds).is_some(),
    }
}

fn totals(t: &query::Totals) -> Totals {
    Totals {
        commits: t.commits,
        contributors: t.authors,
        files: t.files,
        added: t.added,
        removed: t.removed,
    }
}

pub(crate) fn commit_row(store: &Store, i: usize) -> CommitRow {
    CommitRow {
        index: i as u32,
        sha: hex(&store.oid[i][..4]),
        time: store.time[i],
        author: store.people[store.author[i] as usize].name.clone(),
        author_id: store.author[i],
        subject: store.subject(i).to_owned(),
        added: store.c_add[i],
        removed: store.c_del[i],
        files: store.changes_of(i).len() as u32,
        is_merge: store.is_merge(i),
        kind: COMMIT_TYPES[store.ctype[i] as usize].to_owned(),
    }
}

fn contributor(store: &Store, a: &query::AuthorRow) -> Contributor {
    let p = &store.people[a.id as usize];
    Contributor {
        id: a.id,
        name: p.name.clone(),
        email: p.email.clone(),
        is_bot: p.is_bot,
        commits: a.commits,
        added: a.added,
        removed: a.removed,
        first: a.first,
        last: a.last,
    }
}

fn file_stat(store: &Store, f: &query::FileRow) -> FileStat {
    FileStat {
        id: f.id,
        path: store.paths[f.id as usize].clone(),
        touches: f.touches,
        added: f.added,
        removed: f.removed,
        last: f.last,
    }
}

fn named(rows: &[query::NamedRow], limit: usize) -> Vec<NamedValue> {
    rows.iter()
        .take(limit)
        .map(|r| NamedValue {
            name: r.name.clone(),
            lines: r.churn,
            touches: r.touches,
            files: r.files,
        })
        .collect()
}

fn page<T>(items: &[T], offset: u32, limit: u32) -> &[T] {
    let start = (offset as usize).min(items.len());
    &items[start..(start + limit as usize).min(items.len())]
}

impl Repo {
    pub(crate) fn loaded(&self) -> Res<Arc<Store>> {
        self.store
            .read()
            .expect("poisoned")
            .clone()
            .ok_or_else(|| failed("nothing loaded yet"))
    }

    pub(crate) fn frame(&self, store: &Store, q: &Query) -> Res<(Clock, Bounds, Window)> {
        let clock = Clock::named(&q.time_zone).map_err(failed)?;
        let bounds = query::bounds(store, &clock, now());
        let window = Window::of(&clock, preset(&q.preset)?, q.first_day, q.last_day);
        Ok((clock, bounds, window))
    }

    pub(crate) fn filter(q: &Query) -> Filter {
        Filter {
            author: q.author,
            path: q.path.clone(),
        }
    }

    /// The engine-side summary for a query, computed once and reused by the
    /// paging calls that follow it.
    fn summarized(&self, q: &Query) -> Res<(Arc<Store>, Arc<query::Summary>, Window)> {
        let store = self.loaded()?;
        let key = format!(
            "{:p}/{:?}/{}/{}/{}/{:?}/{:?}/{}/{}/{:?}/{}",
            Arc::as_ptr(&store),
            store.stage,
            q.preset,
            q.first_day,
            q.last_day,
            q.author,
            q.path,
            q.search,
            q.max_buckets,
            q.bucket,
            q.time_zone
        );
        let mut cached = self.cached.lock().expect("poisoned");
        if let Some(c) = cached.as_ref().filter(|c| c.key == key) {
            return Ok((store, c.summary.clone(), c.window));
        }
        let (clock, bounds, window) = self.frame(&store, q)?;
        let buckets = match &q.bucket {
            Some(name) => Buckets::build(&clock, &window, bucket(name)?),
            None => Buckets::fit(&clock, &window, q.max_buckets as usize),
        };
        let summary = Arc::new(query::summarize_with(
            &store,
            &clock,
            &window,
            bounds,
            &Self::filter(q),
            &q.search,
            buckets,
        ));
        *cached = Some(Cached {
            key,
            summary: summary.clone(),
            window,
        });
        Ok((store, summary, window))
    }
}

#[uniffi::export]
impl Repo {
    /// Open the repository containing `path`, looking at `reference` (a branch
    /// or tag) or HEAD. The index is cached in `cache_dir`.
    #[uniffi::constructor]
    pub fn open(
        path: String,
        reference: Option<String>,
        cache_dir: Option<String>,
    ) -> Res<Arc<Self>> {
        let mut info =
            store::open(std::path::Path::new(&path), reference.as_deref()).map_err(failed)?;
        if let Some(dir) = cache_dir {
            info = info.with_cache_dir(dir.into());
        }
        Ok(Arc::new(Repo {
            info: RwLock::new(info),
            progress: RwLock::new(Arc::new(Progress::default())),
            store: RwLock::new(None),
            cached: Mutex::new(None),
            graph: Mutex::new(None),
        }))
    }

    pub fn info(&self) -> RepoSummary {
        let info = self.info.read().expect("poisoned");
        RepoSummary {
            name: info
                .workdir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: info.workdir.to_string_lossy().into_owned(),
            ref_name: info.ref_name.clone(),
            head: hex(info.head.as_bytes()),
        }
    }

    /// Load the history: from the cache when the ref has not moved, otherwise
    /// by reading the repository. Blocks until everything is known, so call it
    /// off the main thread; summaries can be asked for as soon as
    /// [`Repo::progress`] reports a stage.
    pub fn index(&self, reindex: bool) -> Res<IndexReport> {
        let started = Instant::now();
        // Pick up a moved ref, and start this run with fresh counters.
        let info = {
            let mut info = self.info.write().expect("poisoned");
            if let Some(tip) = info.current_tip() {
                info.head = tip;
            }
            info.clone()
        };
        let progress = Arc::new(Progress::default());
        *self.progress.write().expect("poisoned") = progress.clone();

        if !reindex && let Some(cached) = store::load_cache(&info) {
            let commits = cached.len() as u64;
            *self.store.write().expect("poisoned") = Some(Arc::new(cached));
            progress.phase.store(git::PHASE_DONE, Ordering::Relaxed);
            return Ok(IndexReport {
                commits,
                from_cache: true,
                seconds: started.elapsed().as_secs_f64(),
            });
        }
        let (done, _) = store::index(&info, &progress, true, |stage| {
            *self.store.write().expect("poisoned") = Some(stage);
        })
        .map_err(failed)?;
        let commits = done.len() as u64;
        if !progress.cancel.load(Ordering::Relaxed) {
            store::save_cache(&info, &done).map_err(failed)?;
        }
        drop(done);
        store::release_memory();
        Ok(IndexReport {
            commits,
            from_cache: false,
            seconds: started.elapsed().as_secs_f64(),
        })
    }

    /// Stop a running [`Repo::index`] as soon as it can.
    pub fn cancel(&self) {
        self.progress
            .read()
            .expect("poisoned")
            .cancel
            .store(true, Ordering::Relaxed);
    }

    pub fn progress(&self) -> ProgressSnapshot {
        let p = self.progress.read().expect("poisoned").clone();
        let stage = match self
            .store
            .read()
            .expect("poisoned")
            .as_deref()
            .map(|s| s.stage)
        {
            None => 0,
            Some(Stage::Commits) => 1,
            Some(Stage::Files) => 2,
            Some(Stage::Lines) => 3,
        };
        ProgressSnapshot {
            stage,
            phase: p.phase.load(Ordering::Relaxed),
            commits: p.commits.load(Ordering::Relaxed),
            files_done: p.diffed.load(Ordering::Relaxed),
            files_total: p.diff_total.load(Ordering::Relaxed),
            lines_done: p.lined.load(Ordering::Relaxed),
            lines_total: p.line_total.load(Ordering::Relaxed),
        }
    }

    /// True once someone has committed, pulled or rebased since the last index.
    pub fn tip_moved(&self) -> bool {
        let info = self.info.read().expect("poisoned");
        info.current_tip().is_some_and(|tip| tip != info.head)
    }

    /// The window for a preset ending on `anchor_day` (default: the day of the
    /// newest commit, or today if that is earlier).
    pub fn window(
        &self,
        preset_name: String,
        anchor_day: Option<i64>,
        time_zone: String,
    ) -> Res<WindowInfo> {
        let store = self.loaded()?;
        let clock = Clock::named(&time_zone).map_err(failed)?;
        let bounds = query::bounds(&store, &clock, now());
        let anchor = anchor_day.unwrap_or(bounds.last_day).min(bounds.last_day);
        let w = Window::ending(&clock, preset(&preset_name)?, anchor, bounds);
        Ok(window_info(&clock, &w, bounds))
    }

    /// The neighbouring window (`direction` -1 earlier, +1 later), or None at
    /// the edge of the repository's history.
    pub fn shift(
        &self,
        window: WindowInfo,
        direction: i32,
        time_zone: String,
    ) -> Res<Option<WindowInfo>> {
        let store = self.loaded()?;
        let clock = Clock::named(&time_zone).map_err(failed)?;
        let bounds = query::bounds(&store, &clock, now());
        let w = Window::of(
            &clock,
            preset(&window.preset)?,
            window.first_day,
            window.last_day,
        );
        Ok(w.shifted(&clock, direction.signum(), bounds)
            .map(|next| window_info(&clock, &next, bounds)))
    }

    pub fn summary(&self, query: Query) -> Res<Summary> {
        let started = Instant::now();
        let (store, s, window) = self.summarized(&query)?;
        let clock = Clock::named(&query.time_zone).map_err(failed)?;
        let bounds = query::bounds(&store, &clock, now());
        let stage = match store.stage {
            Stage::Commits => 1,
            Stage::Files => 2,
            Stage::Lines => 3,
        };
        let type_counts = |rows: &[(&'static str, u64, u64)]| {
            rows.iter()
                .map(|(label, count, lines)| TypeCount {
                    label: (*label).to_owned(),
                    count: *count,
                    lines: *lines,
                })
                .collect()
        };
        Ok(Summary {
            window: window_info(&clock, &window, bounds),
            stage,
            totals: totals(&s.totals),
            previous: s.previous.as_ref().map(totals),
            merges: s.merges,
            busiest_day: s.biggest_day.map(|d| d.0),
            busiest_day_lines: s.biggest_day.map_or(0, |d| d.1),
            bucket: s.buckets.bucket.label().to_owned(),
            series: (0..s.buckets.len())
                .map(|i| BucketPoint {
                    start: s.buckets.edges[i],
                    label: s.buckets.label(&clock, i),
                    commits: s.b_commits[i],
                    added: s.b_added[i],
                    removed: s.b_removed[i],
                })
                .collect(),
            daily: s.daily.clone(),
            rhythm: s.rhythm.iter().flatten().copied().collect(),
            commit_types: type_counts(&s.ctypes),
            change_kinds: type_counts(&s.kinds),
            contributors_total: s.authors.len() as u32,
            files_listed: s.files.len() as u32,
            commits_total: s.commits.len() as u32,
            top_contributors: s
                .authors
                .iter()
                .take(25)
                .map(|a| contributor(&store, a))
                .collect(),
            top_files: s
                .files
                .iter()
                .take(25)
                .map(|f| file_stat(&store, f))
                .collect(),
            directories: named(&s.dirs, 40),
            file_types: named(&s.langs, 40),
            largest_commits: s
                .largest
                .iter()
                .take(30)
                .map(|c| commit_row(&store, *c as usize))
                .collect(),
            recent_commits: s
                .commits
                .iter()
                .take(12)
                .map(|c| commit_row(&store, *c as usize))
                .collect(),
            query_ms: started.elapsed().as_secs_f64() * 1e3,
        })
    }

    /// Contributors active in the query, most commits first.
    pub fn contributors(&self, query: Query, offset: u32, limit: u32) -> Res<Vec<Contributor>> {
        let (store, s, _) = self.summarized(&query)?;
        Ok(page(&s.authors, offset, limit)
            .iter()
            .map(|a| contributor(&store, a))
            .collect())
    }

    /// Files touched in the query, most changed first (the engine keeps the top 2,000).
    pub fn files(&self, query: Query, offset: u32, limit: u32) -> Res<Vec<FileStat>> {
        let (store, s, _) = self.summarized(&query)?;
        Ok(page(&s.files, offset, limit)
            .iter()
            .map(|f| file_stat(&store, f))
            .collect())
    }

    /// Commits matching the query, newest first.
    pub fn commits(&self, query: Query, offset: u32, limit: u32) -> Res<Vec<CommitRow>> {
        let (store, s, _) = self.summarized(&query)?;
        Ok(page(&s.commits, offset, limit)
            .iter()
            .map(|c| commit_row(&store, *c as usize))
            .collect())
    }

    /// Everything about one commit, including its message body and files.
    pub fn commit(&self, index: u32) -> Res<CommitDetail> {
        let store = self.loaded()?;
        let i = index as usize;
        if i >= store.len() {
            return Err(failed("no such commit"));
        }
        let changes = store.changes_of(i);
        let files_total = changes.len() as u32;
        let files = changes
            .take(500)
            .map(|c| {
                let kind = store.ch_kind[c];
                ChangedFile {
                    path: store.paths[store.ch_file[c] as usize].clone(),
                    kind: match kind & !FLAG_BINARY {
                        KIND_ADD => "A",
                        KIND_DEL => "D",
                        KIND_REN => "R",
                        _ => "M",
                    }
                    .to_owned(),
                    added: store.ch_add[c],
                    removed: store.ch_del[c],
                    binary: kind & FLAG_BINARY != 0,
                }
            })
            .collect();
        let body = self
            .info
            .read()
            .expect("poisoned")
            .commit_body(&store.oid[i]);
        Ok(CommitDetail {
            row: commit_row(&store, i),
            oid: hex(&store.oid[i]),
            author_email: store.people[store.author[i] as usize].email.clone(),
            committer: store.people[store.committer[i] as usize].name.clone(),
            commit_time: store.commit_time[i],
            author_offset_minutes: i32::from(store.tz[i]),
            parents: store.parents_of(i).len() as u32,
            body,
            files,
            files_total,
        })
    }

    /// Branches, tags and remote branches, most recently active first.
    pub fn refs(&self) -> Vec<RefInfo> {
        let info = self.info.read().expect("poisoned");
        store::list_refs(&info.repo.to_thread_local())
            .into_iter()
            .map(|r| RefInfo {
                kind: match r.kind {
                    RefKind::Branch => "branch",
                    RefKind::Tag => "tag",
                    RefKind::Remote => "remote",
                }
                .to_owned(),
                is_current: r.name == info.ref_name,
                sha: hex(&r.oid[..4]),
                time: r.time,
                name: r.name,
                full_name: r.full_name,
            })
            .collect()
    }

    /// Analyse another branch or tag (its full name from [`Repo::refs`]), or
    /// HEAD when None. Call [`Repo::index`] afterwards.
    pub fn switch_ref(&self, full_name: Option<String>) -> Res<()> {
        let mut info = self.info.write().expect("poisoned");
        let mut next = store::open(&info.workdir, full_name.as_deref()).map_err(failed)?;
        next.cache_dir = info.cache_dir.clone();
        *info = next;
        *self.store.write().expect("poisoned") = None;
        *self.cached.lock().expect("poisoned") = None;
        *self.graph.lock().expect("poisoned") = None;
        Ok(())
    }

    /// Number of rows in the commit graph (every commit of the history).
    pub fn graph_len(&self) -> Res<u32> {
        Ok(self.loaded()?.len() as u32)
    }

    /// A page of the commit graph, newest first, in git's topological order.
    pub fn graph_rows(&self, start: u32, count: u32) -> Res<Vec<GraphRow>> {
        let store = self.loaded()?;
        let mut guard = self.graph.lock().expect("poisoned");
        if guard.as_ref().is_none_or(|g| g.len() != store.len()) {
            *guard = Some(Graph::new(&store));
        }
        let graph = guard.as_mut().expect("just built");
        let rows = graph.rows(&store, start as usize, count as usize);
        // Refs that sit on a visible commit become its labels.
        let info = self.info.read().expect("poisoned");
        let refs = store::list_refs(&info.repo.to_thread_local());
        let labelled: Vec<(usize, &str)> = refs
            .iter()
            .filter_map(|r| {
                store
                    .find(&r.oid, r.author_time)
                    .map(|i| (i, r.name.as_str()))
            })
            .collect();
        Ok(rows
            .into_iter()
            .map(|row| {
                let i = row.commit as usize;
                GraphRow {
                    commit: commit_row(&store, i),
                    cells: row
                        .cells
                        .into_iter()
                        .map(|(g, color)| GraphCell {
                            glyph: g.to_string(),
                            color,
                        })
                        .collect(),
                    labels: labelled
                        .iter()
                        .filter(|l| l.0 == i)
                        .map(|l| l.1.to_owned())
                        .collect(),
                }
            })
            .collect())
    }

    /// Graph row of the newest commit inside a window, so the graph can follow the time range.
    pub fn graph_row_for(&self, window: WindowInfo) -> Res<Option<u32>> {
        let store = self.loaded()?;
        let mut guard = self.graph.lock().expect("poisoned");
        if guard.as_ref().is_none_or(|g| g.len() != store.len()) {
            *guard = Some(Graph::new(&store));
        }
        let graph = guard.as_ref().expect("just built");
        Ok(store
            .range(window.start, window.end)
            .next_back()
            .map(|newest| graph.pos[newest]))
    }
}

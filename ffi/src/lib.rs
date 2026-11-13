//! Swift bindings for the Gitsanity engine.
//!
//! Spike scope: open a repository, index it (or load the cache), report
//! progress, and answer the Overview's questions. The rule for everything that
//! follows: the big columns stay on this side; Swift gets small answers.

use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use gitsanity::git::Progress;
use gitsanity::query::{self, Filter};
use gitsanity::store::{self, RepoInfo, Stage, Store};
use gitsanity::time::{Clock, Preset, Window, civil_from_days, month_name};

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

fn failed(e: impl std::fmt::Display) -> EngineError {
    EngineError::Failed(format!("{e:#}"))
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

#[derive(uniffi::Record)]
pub struct NamedCount {
    pub name: String,
    pub count: u64,
}

/// What the Overview screen shows for one time range.
#[derive(uniffi::Record)]
pub struct Overview {
    /// `Apr 16, 2005 – Feb 3, 2026`, in the viewer's time zone.
    pub range_label: String,
    pub commits: u64,
    pub contributors: u64,
    pub files: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
    /// `Apr 16, 2005`, or empty when no lines are known yet.
    pub busiest_day: String,
    pub busiest_day_lines: u64,
    /// Bucket size of the series: hour, day, week, month, quarter or year.
    pub bucket: String,
    pub commits_per_bucket: Vec<u64>,
    pub top_contributors: Vec<NamedCount>,
    pub top_files: Vec<NamedCount>,
    /// Time the engine spent computing this answer.
    pub query_ms: f64,
}

#[derive(uniffi::Object)]
pub struct Repo {
    info: RepoInfo,
    progress: Arc<Progress>,
    store: RwLock<Option<Arc<Store>>>,
}

#[uniffi::export]
impl Repo {
    /// Open the repository containing `path`, looking at `reference` (a branch
    /// or tag name) or HEAD.
    #[uniffi::constructor]
    pub fn open(path: String, reference: Option<String>) -> Result<Arc<Self>, EngineError> {
        let info =
            store::open(std::path::Path::new(&path), reference.as_deref()).map_err(failed)?;
        Ok(Arc::new(Repo {
            info,
            progress: Arc::new(Progress::default()),
            store: RwLock::new(None),
        }))
    }

    pub fn name(&self) -> String {
        self.info
            .workdir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn ref_name(&self) -> String {
        self.info.ref_name.clone()
    }

    /// Load the history: from the cache when HEAD has not moved, otherwise by
    /// reading the repository. Blocks until everything is known; call it off
    /// the main thread and poll [`Repo::progress`] / [`Repo::overview`] meanwhile.
    pub fn index(&self, reindex: bool) -> Result<IndexReport, EngineError> {
        let started = Instant::now();
        if !reindex && let Some(cached) = store::load_cache(&self.info) {
            let commits = cached.len() as u64;
            *self.store.write().expect("poisoned") = Some(Arc::new(cached));
            self.progress
                .phase
                .store(gitsanity::git::PHASE_DONE, Ordering::Relaxed);
            return Ok(IndexReport {
                commits,
                from_cache: true,
                seconds: started.elapsed().as_secs_f64(),
            });
        }
        let (done, _) = store::index(&self.info, &self.progress, true, |stage| {
            *self.store.write().expect("poisoned") = Some(stage);
        })
        .map_err(failed)?;
        store::save_cache(&self.info, &done).map_err(failed)?;
        Ok(IndexReport {
            commits: done.len() as u64,
            from_cache: false,
            seconds: started.elapsed().as_secs_f64(),
        })
    }

    pub fn progress(&self) -> ProgressSnapshot {
        let p = &self.progress;
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

    /// The Overview for a preset range (`1D 5D 1M 6M YTD 1Y 5Y All`) ending at
    /// the newest commit, with every day and bucket taken in `time_zone`
    /// (an IANA name such as `America/Denver`).
    pub fn overview(
        &self,
        preset: String,
        time_zone: String,
        max_buckets: u32,
    ) -> Result<Overview, EngineError> {
        let store = self
            .store
            .read()
            .expect("poisoned")
            .clone()
            .ok_or_else(|| failed("nothing loaded yet"))?;
        let clock = Clock::named(&time_zone).map_err(failed)?;
        let preset = Preset::ALL
            .into_iter()
            .find(|p| p.label().eq_ignore_ascii_case(&preset))
            .ok_or_else(|| failed(format!("no range named {preset}")))?;
        let started = Instant::now();
        let now = store.time.last().copied().unwrap_or(0);
        let bounds = query::bounds(&store, &clock, now);
        let window = Window::ending(&clock, preset, bounds.last_day, bounds);
        let s = query::summarize(
            &store,
            &clock,
            &window,
            bounds,
            &Filter::default(),
            "",
            max_buckets as usize,
        );
        let (busiest_day, busiest_day_lines) = match s.biggest_day {
            Some((day, lines)) => {
                let (y, m, d) = civil_from_days(day);
                (format!("{} {d}, {y}", month_name(m)), lines)
            }
            None => (String::new(), 0),
        };
        Ok(Overview {
            range_label: window.label(),
            commits: s.totals.commits,
            contributors: s.totals.authors,
            files: s.totals.files,
            lines_added: s.totals.added,
            lines_removed: s.totals.removed,
            busiest_day,
            busiest_day_lines,
            bucket: s.buckets.bucket.label().to_owned(),
            commits_per_bucket: s.b_commits.clone(),
            top_contributors: s
                .authors
                .iter()
                .take(10)
                .map(|a| NamedCount {
                    name: store.people[a.id as usize].name.clone(),
                    count: u64::from(a.commits),
                })
                .collect(),
            top_files: s
                .files
                .iter()
                .take(10)
                .map(|f| NamedCount {
                    name: store.paths[f.id as usize].clone(),
                    count: f.added + f.removed,
                })
                .collect(),
            query_ms: started.elapsed().as_secs_f64() * 1e3,
        })
    }
}

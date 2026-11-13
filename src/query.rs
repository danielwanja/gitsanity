//! Dashboard queries: one pass over the commits (and their file changes) that
//! fall inside the window. No rollup tables: the scan is fast enough to redo
//! on every window or filter change.

use std::collections::HashMap;

use crate::git::{FLAG_BINARY, KIND_ADD, KIND_DEL, KIND_MOD, KIND_REN};
use crate::store::{COMMIT_TYPES, Stage, Store};
use crate::time::{Bounds, Buckets, Clock, Window};

#[derive(Clone, Default, PartialEq)]
pub struct Filter {
    pub author: Option<u32>,
    /// One file (`src/a.rs`) or everything under a directory (`src/`).
    pub path: Option<String>,
}

/// For a path filter, which files it selects.
fn file_mask(store: &Store, filter: &Filter) -> Option<Vec<bool>> {
    let path = filter.path.as_deref()?;
    let dir = path.ends_with('/');
    Some(
        store
            .paths
            .iter()
            .map(|p| if dir { p.starts_with(path) } else { p == path })
            .collect(),
    )
}

/// Lines a commit added and removed, counting only files the mask selects.
/// None when the mask leaves the commit with nothing.
fn commit_lines(store: &Store, i: usize, mask: Option<&[bool]>) -> Option<(u64, u64)> {
    let Some(mask) = mask else {
        return Some((u64::from(store.c_add[i]), u64::from(store.c_del[i])));
    };
    let (mut added, mut removed, mut any) = (0, 0, false);
    for c in store.changes_of(i) {
        if mask[store.ch_file[c] as usize] {
            any = true;
            added += u64::from(store.ch_add[c]);
            removed += u64::from(store.ch_del[c]);
        }
    }
    any.then_some((added, removed))
}

#[derive(Clone, Copy, Default)]
pub struct Totals {
    pub commits: u64,
    pub authors: u64,
    pub files: u64,
    pub added: u64,
    pub removed: u64,
}

impl Totals {
    pub fn churn(&self) -> u64 {
        self.added + self.removed
    }
}

#[derive(Clone)]
pub struct AuthorRow {
    pub id: u32,
    pub commits: u32,
    pub added: u64,
    pub removed: u64,
    pub first: i64,
    pub last: i64,
}

#[derive(Clone)]
pub struct FileRow {
    pub id: u32,
    pub touches: u32,
    pub added: u64,
    pub removed: u64,
    pub last: i64,
}

#[derive(Clone)]
pub struct NamedRow {
    pub name: String,
    pub touches: u64,
    pub churn: u64,
    pub files: u32,
}

pub struct Summary {
    pub totals: Totals,
    /// Same figures for the window just before this one (None for "All").
    pub previous: Option<Totals>,
    pub merges: u64,
    /// (local day, lines changed) of the busiest day.
    pub biggest_day: Option<(i64, u64)>,
    pub buckets: Buckets,
    pub b_commits: Vec<u64>,
    pub b_added: Vec<u64>,
    pub b_removed: Vec<u64>,
    /// Commits per local day, `first_day..=last_day`.
    pub daily: Vec<u32>,
    pub authors: Vec<AuthorRow>,
    pub files: Vec<FileRow>,
    pub dirs: Vec<NamedRow>,
    pub langs: Vec<NamedRow>,
    /// (label, commits, churn) per commit type, biggest first.
    pub ctypes: Vec<(&'static str, u64, u64)>,
    /// (label, events, churn) for added / modified / deleted / renamed / binary.
    pub kinds: Vec<(&'static str, u64, u64)>,
    /// Commits by [weekday Mon..Sun][hour], viewer's local time.
    pub rhythm: [[u32; 24]; 7],
    /// Matching commits, newest first.
    pub commits: Vec<u32>,
    /// The matching commits that changed the most lines, biggest first.
    pub largest: Vec<u32>,
}

/// Local-day span of the repository, never reaching past today.
pub fn bounds(store: &Store, clock: &Clock, now: i64) -> Bounds {
    let first = store.time.first().copied().unwrap_or(now);
    let last = store
        .time
        .last()
        .copied()
        .unwrap_or(now)
        .min(now)
        .max(first);
    Bounds {
        first_day: clock.day(first),
        last_day: clock.day(last),
    }
}

fn totals(store: &Store, w: &Window, filter: &Filter) -> Totals {
    let range = store.range(w.start, w.end);
    let mut t = Totals::default();
    let mask = file_mask(store, filter);
    let mut author_seen = vec![false; store.people.len()];
    let mut file_seen = vec![false; store.paths.len()];
    for i in range {
        if filter.author.is_some_and(|a| a != store.author[i]) {
            continue;
        }
        let Some((added, removed)) = commit_lines(store, i, mask.as_deref()) else {
            continue;
        };
        t.commits += 1;
        t.added += added;
        t.removed += removed;
        if !std::mem::replace(&mut author_seen[store.author[i] as usize], true) {
            t.authors += 1;
        }
        for c in store.changes_of(i) {
            let file = store.ch_file[c] as usize;
            if mask.as_ref().is_none_or(|m| m[file])
                && !std::mem::replace(&mut file_seen[file], true)
            {
                t.files += 1;
            }
        }
    }
    t
}

/// Group key for the directory table: the first two path components.
fn dir_group(dir: &str) -> &str {
    match dir.match_indices('/').nth(1) {
        Some((i, _)) => &dir[..i],
        None => dir,
    }
}

pub fn summarize(
    store: &Store,
    clock: &Clock,
    w: &Window,
    bounds: Bounds,
    filter: &Filter,
    search: &str,
    max_buckets: usize,
) -> Summary {
    let buckets = Buckets::fit(clock, w, max_buckets);
    summarize_with(store, clock, w, bounds, filter, search, buckets)
}

/// [`summarize`] with the bucket size chosen by the caller (the Mac app's
/// code-frequency chart is always weekly, for instance).
pub fn summarize_with(
    store: &Store,
    clock: &Clock,
    w: &Window,
    bounds: Bounds,
    filter: &Filter,
    search: &str,
    buckets: Buckets,
) -> Summary {
    let range = store.range(w.start, w.end);
    let nb = buckets.len();
    let mut s = Summary {
        totals: Totals::default(),
        previous: w
            .shifted(clock, -1, bounds)
            .map(|p| totals(store, &p, filter)),
        merges: 0,
        biggest_day: None,
        b_commits: vec![0; nb],
        b_added: vec![0; nb],
        b_removed: vec![0; nb],
        daily: vec![0; w.days().max(0) as usize],
        buckets,
        authors: Vec::new(),
        files: Vec::new(),
        dirs: Vec::new(),
        langs: Vec::new(),
        ctypes: Vec::new(),
        kinds: Vec::new(),
        rhythm: [[0; 24]; 7],
        commits: Vec::new(),
        largest: Vec::new(),
    };
    let mut day_churn = vec![0u64; s.daily.len()];
    let mut authors: Vec<AuthorRow> = Vec::new();
    let mut author_slot = vec![u32::MAX; store.people.len()];
    let mut files: Vec<FileRow> = Vec::new();
    let mut file_slot = vec![u32::MAX; store.paths.len()];
    let mut ctypes = [(0u64, 0u64); COMMIT_TYPES.len()];
    let mut kinds = [(0u64, 0u64); 5];
    let needle = search.to_lowercase();
    let mask = file_mask(store, filter);
    let mut bucket = 0usize;

    for i in range {
        let author = store.author[i];
        if filter.author.is_some_and(|a| a != author) {
            continue;
        }
        let t = store.time[i];
        let Some((added, removed)) = commit_lines(store, i, mask.as_deref()) else {
            continue;
        };
        s.totals.commits += 1;
        s.totals.added += added;
        s.totals.removed += removed;
        s.merges += u64::from(store.is_merge(i));

        // Commits are time-ordered, so the bucket cursor only moves forward.
        while bucket + 1 < nb && t >= s.buckets.edges[bucket + 1] {
            bucket += 1;
        }
        s.b_commits[bucket] += 1;
        s.b_added[bucket] += added;
        s.b_removed[bucket] += removed;

        let day = (clock.day(t) - w.first_day).clamp(0, s.daily.len() as i64 - 1) as usize;
        s.daily[day] += 1;
        day_churn[day] += added + removed;
        let (weekday, hour) = clock.weekday_hour(t);
        s.rhythm[weekday][hour] += 1;

        let ct = &mut ctypes[store.ctype[i] as usize];
        ct.0 += 1;
        ct.1 += added + removed;

        let slot = &mut author_slot[author as usize];
        if *slot == u32::MAX {
            *slot = authors.len() as u32;
            authors.push(AuthorRow {
                id: author,
                commits: 0,
                added: 0,
                removed: 0,
                first: t,
                last: t,
            });
        }
        let a = &mut authors[*slot as usize];
        a.commits += 1;
        a.added += added;
        a.removed += removed;
        a.last = t;

        for c in store.changes_of(i) {
            let file = store.ch_file[c];
            if mask.as_ref().is_some_and(|m| !m[file as usize]) {
                continue;
            }
            let (fa, fr) = (u64::from(store.ch_add[c]), u64::from(store.ch_del[c]));
            let slot = &mut file_slot[file as usize];
            if *slot == u32::MAX {
                *slot = files.len() as u32;
                files.push(FileRow {
                    id: file,
                    touches: 0,
                    added: 0,
                    removed: 0,
                    last: t,
                });
            }
            let f = &mut files[*slot as usize];
            f.touches += 1;
            f.added += fa;
            f.removed += fr;
            f.last = t;
            let kind = store.ch_kind[c];
            let k = if kind & FLAG_BINARY != 0 {
                4
            } else {
                match kind {
                    KIND_ADD => 0,
                    KIND_MOD => 1,
                    KIND_DEL => 2,
                    KIND_REN => 3,
                    _ => 1,
                }
            };
            kinds[k].0 += 1;
            kinds[k].1 += fa + fr;
        }

        if needle.is_empty() || matches(store, clock, i, &needle) {
            s.commits.push(i as u32);
        }
    }
    s.commits.reverse();
    let churn = |c: &u32| u64::from(store.c_add[*c as usize]) + u64::from(store.c_del[*c as usize]);
    let mut largest: Vec<u32> = s.commits.iter().copied().filter(|c| churn(c) > 0).collect();
    if largest.len() > 100 {
        largest.select_nth_unstable_by_key(100, |c| std::cmp::Reverse(churn(c)));
        largest.truncate(100);
    }
    largest.sort_unstable_by_key(|c| (std::cmp::Reverse(churn(c)), *c));
    s.largest = largest;
    s.totals.authors = authors.len() as u64;
    s.totals.files = files.len() as u64;
    s.biggest_day = day_churn
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| **c)
        .filter(|(_, c)| **c > 0)
        .map(|(d, c)| (w.first_day + d as i64, *c));

    // Directories and languages roll up from the per-file rows.
    let mut dirs: HashMap<&str, NamedRow> = HashMap::new();
    let mut langs: HashMap<u16, NamedRow> = HashMap::new();
    for f in &files {
        let churn = f.added + f.removed;
        let group = dir_group(&store.dirs[store.f_dir[f.id as usize] as usize]);
        let d = dirs.entry(group).or_insert_with(|| NamedRow {
            name: if group.is_empty() {
                "(root)".to_owned()
            } else {
                group.to_owned()
            },
            touches: 0,
            churn: 0,
            files: 0,
        });
        d.touches += u64::from(f.touches);
        d.churn += churn;
        d.files += 1;
        let lang = store.f_lang[f.id as usize];
        let l = langs.entry(lang).or_insert_with(|| NamedRow {
            name: store.langs[lang as usize].clone(),
            touches: 0,
            churn: 0,
            files: 0,
        });
        l.touches += u64::from(f.touches);
        l.churn += churn;
        l.files += 1;
    }
    let by_lines = store.stage == Stage::Lines;
    let weight = |churn: u64, touches: u64| {
        if by_lines {
            (churn, touches)
        } else {
            (touches, churn)
        }
    };
    s.dirs = dirs.into_values().collect();
    s.dirs.sort_unstable_by(|a, b| {
        weight(b.churn, b.touches)
            .cmp(&weight(a.churn, a.touches))
            .then(a.name.cmp(&b.name))
    });
    s.langs = langs.into_values().collect();
    s.langs.sort_unstable_by(|a, b| {
        weight(b.churn, b.touches)
            .cmp(&weight(a.churn, a.touches))
            .then(a.name.cmp(&b.name))
    });
    files.sort_unstable_by(|a, b| {
        weight(b.added + b.removed, u64::from(b.touches))
            .cmp(&weight(a.added + a.removed, u64::from(a.touches)))
            .then(a.id.cmp(&b.id))
    });
    files.truncate(2000);
    s.files = files;
    authors.sort_unstable_by(|a, b| b.commits.cmp(&a.commits).then(a.id.cmp(&b.id)));
    s.authors = authors;
    s.ctypes = COMMIT_TYPES
        .iter()
        .zip(ctypes)
        .filter(|(_, c)| c.0 > 0)
        .map(|(n, c)| (*n, c.0, c.1))
        .collect();
    s.ctypes.sort_by_key(|c| std::cmp::Reverse(c.1));
    s.kinds = ["added", "modified", "deleted", "renamed", "binary"]
        .into_iter()
        .zip(kinds)
        .map(|(n, k)| (n, k.0, k.1))
        .collect();
    s
}

/// Search matches the subject, author name or email, a SHA prefix, or a local
/// date typed as `YYYY-MM-DD` (or any prefix of it, like `2026-02`).
fn matches(store: &Store, clock: &Clock, i: usize, needle: &str) -> bool {
    let p = &store.people[store.author[i] as usize];
    contains_ci(store.subject(i), needle)
        || contains_ci(&p.name, needle)
        || contains_ci(&p.email, needle)
        || (needle.len() >= 4
            && needle.bytes().all(|b| b.is_ascii_hexdigit())
            && store.hex_oid(i).starts_with(needle))
        || (needle.len() >= 4
            && needle.as_bytes()[..4].iter().all(u8::is_ascii_digit)
            && clock.date(store.time[i]).starts_with(needle))
}

fn contains_ci(hay: &str, needle_lower: &str) -> bool {
    if needle_lower.len() > hay.len() {
        return false;
    }
    let (h, n) = (hay.as_bytes(), needle_lower.as_bytes());
    h.windows(n.len())
        .any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

// ------------------------------------------------------------ extra views
//
// Second-pass queries for screens that need more than the summary. They are
// separate so the common case (one pass) stays as cheap as it is.

/// (file, events, lines changed by those events)
pub type FileEvents = (u32, u32, u64);

pub struct Extras {
    /// Commits per bucket for the most active contributors: (person, series).
    pub stream: Vec<(u32, Vec<u32>)>,
    pub top_added: Vec<FileEvents>,
    pub top_deleted: Vec<FileEvents>,
    pub top_renamed: Vec<FileEvents>,
    /// File types with vendored and generated files left out.
    pub langs_core: Vec<NamedRow>,
    /// Commits attributed to each file type (the type a commit changed most),
    /// most commits first.
    pub attributed: Vec<(String, u64)>,
}

/// Third-party code checked into the repository.
pub fn is_vendored(path: &str) -> bool {
    ["vendor/", "third_party/"]
        .iter()
        .any(|dir| path.starts_with(dir) || path.contains(&format!("/{dir}")))
}

/// Files written by a tool rather than a person.
pub fn is_generated(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("generated/")
        || lower.contains("/generated/")
        || lower.contains(".gen")
        || lower.contains("autogen")
}

pub fn extras(
    store: &Store,
    w: &Window,
    filter: &Filter,
    s: &Summary,
    stream_size: usize,
) -> Extras {
    let mask = file_mask(store, filter);
    let nb = s.buckets.len();
    let people: Vec<u32> = s.authors.iter().take(stream_size).map(|a| a.id).collect();
    let mut stream_slot = vec![u32::MAX; store.people.len()];
    for (k, id) in people.iter().enumerate() {
        stream_slot[*id as usize] = k as u32;
    }
    let mut stream = vec![vec![0u32; nb]; people.len()];

    let files = store.paths.len();
    let mut events = vec![[0u32; 3]; files];
    let mut event_lines = vec![[0u64; 3]; files];
    // 0 not looked at yet, 1 counts as the project's own code, 2 vendored or generated
    let mut class = vec![0u8; files];
    let mut seen = vec![false; files];
    let mut core: HashMap<u16, NamedRow> = HashMap::new();
    let mut attributed = vec![0u64; store.langs.len()];
    let mut per_type: Vec<(u16, u64, u32)> = Vec::new();
    let mut bucket = 0usize;

    for i in store.range(w.start, w.end) {
        let author = store.author[i];
        if filter.author.is_some_and(|a| a != author)
            || commit_lines(store, i, mask.as_deref()).is_none()
        {
            continue;
        }
        while bucket + 1 < nb && store.time[i] >= s.buckets.edges[bucket + 1] {
            bucket += 1;
        }
        if stream_slot[author as usize] != u32::MAX {
            stream[stream_slot[author as usize] as usize][bucket] += 1;
        }
        per_type.clear();
        for c in store.changes_of(i) {
            let file = store.ch_file[c] as usize;
            if mask.as_ref().is_some_and(|m| !m[file]) {
                continue;
            }
            let lines = u64::from(store.ch_add[c]) + u64::from(store.ch_del[c]);
            let kind = match store.ch_kind[c] & !FLAG_BINARY {
                KIND_ADD => Some(0),
                KIND_DEL => Some(1),
                KIND_REN => Some(2),
                _ => None,
            };
            if let Some(k) = kind {
                events[file][k] += 1;
                event_lines[file][k] += lines;
            }
            let lang = store.f_lang[file];
            match per_type.iter_mut().find(|t| t.0 == lang) {
                Some(t) => {
                    t.1 += lines;
                    t.2 += 1;
                }
                None => per_type.push((lang, lines, 1)),
            }
            if class[file] == 0 {
                let path = &store.paths[file];
                class[file] = if is_vendored(path) || is_generated(path) {
                    2
                } else {
                    1
                };
            }
            if class[file] == 1 {
                let row = core.entry(lang).or_insert_with(|| NamedRow {
                    name: store.langs[lang as usize].clone(),
                    touches: 0,
                    churn: 0,
                    files: 0,
                });
                row.touches += 1;
                row.churn += lines;
                if !std::mem::replace(&mut seen[file], true) {
                    row.files += 1;
                }
            }
        }
        // A commit belongs to the type it changed most (lines, then files touched).
        if let Some(best) = per_type
            .iter()
            .max_by_key(|t| (t.1, t.2, std::cmp::Reverse(t.0)))
        {
            attributed[best.0 as usize] += 1;
        }
    }

    let top = |k: usize| -> Vec<FileEvents> {
        let mut rows: Vec<FileEvents> = (0..files)
            .filter(|f| events[*f][k] > 0)
            .map(|f| (f as u32, events[f][k], event_lines[f][k]))
            .collect();
        rows.sort_unstable_by_key(|r| (std::cmp::Reverse(r.1), std::cmp::Reverse(r.2), r.0));
        rows.truncate(20);
        rows
    };
    let mut langs_core: Vec<NamedRow> = core.into_values().collect();
    langs_core.sort_unstable_by(|a, b| {
        (b.churn, b.touches)
            .cmp(&(a.churn, a.touches))
            .then(a.name.cmp(&b.name))
    });
    let mut attributed: Vec<(String, u64)> = attributed
        .into_iter()
        .enumerate()
        .filter(|(_, n)| *n > 0)
        .map(|(lang, n)| (store.langs[lang].clone(), n))
        .collect();
    attributed.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Extras {
        stream: people.into_iter().zip(stream).collect(),
        top_added: top(0),
        top_deleted: top(1),
        top_renamed: top(2),
        langs_core,
        attributed,
    }
}

pub struct StaleRow {
    pub file: u32,
    pub churn: u64,
    pub last: i64,
    pub last_author: u32,
}

pub const STALENESS_BINS: [&str; 7] = [
    "0–7d",
    "8–30d",
    "31–90d",
    "91–180d",
    "181–365d",
    "1–2y",
    "2y+",
];

pub struct FileHealth {
    /// Files that exist at the end of the window, by time since their last
    /// change (see [`STALENESS_BINS`]).
    pub staleness: [u32; 7],
    pub alive: u32,
    /// Most-changed files nobody has touched for 90 days or more.
    pub stale: Vec<StaleRow>,
    /// Files first added inside the window, newest first: (file, time).
    pub new_files: Vec<(u32, i64)>,
}

/// File lifecycle as of the end of the window. Unlike a summary this reads the
/// history from its very first commit, because "when was this last touched"
/// does not depend on where the window starts.
pub fn file_health(store: &Store, w: &Window) -> FileHealth {
    let range = store.range(w.start, w.end);
    let files = store.paths.len();
    let mut last = vec![0i64; files];
    let mut last_author = vec![0u32; files];
    let mut churn = vec![0u64; files];
    let mut alive = vec![false; files];
    let mut new_files = Vec::new();
    let mut renames = store.ren_from.iter().peekable();
    for i in 0..range.end {
        let t = store.time[i];
        for c in store.changes_of(i) {
            let file = store.ch_file[c] as usize;
            last[file] = t;
            last_author[file] = store.author[i];
            churn[file] += u64::from(store.ch_add[c]) + u64::from(store.ch_del[c]);
            match store.ch_kind[c] & !FLAG_BINARY {
                KIND_DEL => alive[file] = false,
                KIND_ADD => {
                    alive[file] = true;
                    if i >= range.start {
                        new_files.push((file as u32, t));
                    }
                }
                KIND_REN => {
                    alive[file] = true;
                    while renames.peek().is_some_and(|r| (r.0 as usize) < c) {
                        renames.next();
                    }
                    if let Some((_, from)) = renames.peek().filter(|r| r.0 as usize == c) {
                        alive[*from as usize] = false;
                    }
                }
                _ => alive[file] = true,
            }
        }
    }
    let mut health = FileHealth {
        staleness: [0; 7],
        alive: 0,
        stale: Vec::new(),
        new_files,
    };
    for file in (0..files).filter(|f| alive[*f]) {
        health.alive += 1;
        let days = (w.end - last[file]).max(0) / 86_400;
        let bin = match days {
            0..=7 => 0,
            8..=30 => 1,
            31..=90 => 2,
            91..=180 => 3,
            181..=365 => 4,
            366..=730 => 5,
            _ => 6,
        };
        health.staleness[bin] += 1;
        if days >= 90 {
            health.stale.push(StaleRow {
                file: file as u32,
                churn: churn[file],
                last: last[file],
                last_author: last_author[file],
            });
        }
    }
    health
        .stale
        .sort_unstable_by_key(|r| (std::cmp::Reverse(r.churn), r.file));
    health.stale.truncate(200);
    health
        .new_files
        .sort_unstable_by_key(|n| (std::cmp::Reverse(n.1), n.0));
    health.new_files.truncate(500);
    health
}

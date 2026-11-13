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
    let last = store.time.last().copied().unwrap_or(now).min(now).max(first);
    Bounds { first_day: clock.day(first), last_day: clock.day(last) }
}

fn totals(store: &Store, w: &Window, filter: &Filter) -> Totals {
    let range = store.range(w.start, w.end);
    let mut t = Totals::default();
    let mut author_seen = vec![false; store.people.len()];
    let mut file_seen = vec![false; store.paths.len()];
    for i in range {
        if filter.author.is_some_and(|a| a != store.author[i]) {
            continue;
        }
        t.commits += 1;
        t.added += u64::from(store.c_add[i]);
        t.removed += u64::from(store.c_del[i]);
        if !std::mem::replace(&mut author_seen[store.author[i] as usize], true) {
            t.authors += 1;
        }
        for c in store.changes_of(i) {
            if !std::mem::replace(&mut file_seen[store.ch_file[c] as usize], true) {
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
    let range = store.range(w.start, w.end);
    let buckets = Buckets::fit(clock, w, max_buckets);
    let nb = buckets.len();
    let mut s = Summary {
        totals: Totals::default(),
        previous: w.shifted(clock, -1, bounds).map(|p| totals(store, &p, filter)),
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
    let mut bucket = 0usize;

    for i in range {
        let author = store.author[i];
        if filter.author.is_some_and(|a| a != author) {
            continue;
        }
        let t = store.time[i];
        let (added, removed) = (u64::from(store.c_add[i]), u64::from(store.c_del[i]));
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
            authors.push(AuthorRow { id: author, commits: 0, added: 0, removed: 0, first: t, last: t });
        }
        let a = &mut authors[*slot as usize];
        a.commits += 1;
        a.added += added;
        a.removed += removed;
        a.last = t;

        for c in store.changes_of(i) {
            let file = store.ch_file[c];
            let (fa, fr) = (u64::from(store.ch_add[c]), u64::from(store.ch_del[c]));
            let slot = &mut file_slot[file as usize];
            if *slot == u32::MAX {
                *slot = files.len() as u32;
                files.push(FileRow { id: file, touches: 0, added: 0, removed: 0, last: t });
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
            name: if group.is_empty() { "(root)".to_owned() } else { group.to_owned() },
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
    let weight = |churn: u64, touches: u64| if by_lines { (churn, touches) } else { (touches, churn) };
    s.dirs = dirs.into_values().collect();
    s.dirs.sort_unstable_by(|a, b| weight(b.churn, b.touches).cmp(&weight(a.churn, a.touches)).then(a.name.cmp(&b.name)));
    s.langs = langs.into_values().collect();
    s.langs
        .sort_unstable_by(|a, b| weight(b.churn, b.touches).cmp(&weight(a.churn, a.touches)).then(a.name.cmp(&b.name)));
    files.sort_unstable_by(|a, b| {
        weight(b.added + b.removed, u64::from(b.touches))
            .cmp(&weight(a.added + a.removed, u64::from(a.touches)))
            .then(a.id.cmp(&b.id))
    });
    files.truncate(2000);
    s.files = files;
    authors.sort_unstable_by(|a, b| b.commits.cmp(&a.commits).then(a.id.cmp(&b.id)));
    s.authors = authors;
    s.ctypes = COMMIT_TYPES.iter().zip(ctypes).filter(|(_, c)| c.0 > 0).map(|(n, c)| (*n, c.0, c.1)).collect();
    s.ctypes.sort_by(|a, b| b.1.cmp(&a.1));
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
        || (needle.len() >= 4 && needle.bytes().all(|b| b.is_ascii_hexdigit()) && store.hex_oid(i).starts_with(needle))
        || (needle.len() >= 4 && needle.as_bytes()[..4].iter().all(u8::is_ascii_digit) && clock.date(store.time[i]).starts_with(needle))
}

fn contains_ci(hay: &str, needle_lower: &str) -> bool {
    if needle_lower.len() > hay.len() {
        return false;
    }
    let (h, n) = (hay.as_bytes(), needle_lower.as_bytes());
    h.windows(n.len()).any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

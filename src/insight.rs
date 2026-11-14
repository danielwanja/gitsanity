//! Questions that need more than counting inside a window: who knows which
//! code, which files move together, how the group of contributors changes,
//! and which commits lie between two releases.

use std::collections::HashMap;

use rayon::prelude::*;

use crate::git::{FLAG_BINARY, KIND_DEL, KIND_REN};
use crate::query::{Filter, commit_lines, dir_group, file_mask, is_generated, is_vendored};
use crate::store::{NO_PARENT, Stage, Store};
use crate::time::{Buckets, Window};

/// Commits that change more files than this say little about which files
/// belong together (tree-wide renames, licence headers, reformatting).
pub const BULK_COMMIT_FILES: usize = 50;

// ---------------------------------------------------------------- ownership

/// One file that exists at the end of the window, with its whole history.
#[derive(Clone)]
pub struct OwnedFile {
    /// The file under its current name.
    pub file: u32,
    /// Lines in the file now: everything added minus everything removed.
    pub size: u64,
    /// Lines changed over the file's history, by everyone.
    pub lines: u64,
    pub authors: u32,
    /// Whoever changed the most lines.
    pub owner: u32,
    pub owner_lines: u64,
    /// The owner's latest commit anywhere in the repository.
    pub owner_last: i64,
    /// Changes inside the window.
    pub touches: u32,
    pub churn: u64,
}

impl OwnedFile {
    /// One person wrote at least four fifths of the changes.
    pub fn single_owner(&self) -> bool {
        self.owner_lines * 5 >= self.lines * 4
    }
}

#[derive(Clone)]
pub struct OwnedDir {
    pub name: String,
    pub lines: u64,
    pub authors: u32,
    pub owner: u32,
    pub owner_lines: u64,
    pub owner_last: i64,
    pub files: u32,
    /// Files in it that one person wrote at least four fifths of.
    pub single_owner: u32,
    /// Lines changed inside the window.
    pub churn: u64,
}

pub struct Ownership {
    /// Most changed in the window first.
    pub files: Vec<OwnedFile>,
    /// Largest history first.
    pub dirs: Vec<OwnedDir>,
    /// (person, lines changed) over the history of those files, most first.
    pub people: Vec<(u32, u64)>,
    /// Each person's latest commit anywhere in the repository, by person id.
    pub last_commit: Vec<i64>,
    pub lines: u64,
    /// False while line counts are not known yet: ownership is then by number
    /// of changes instead of lines.
    pub by_lines: bool,
}

/// Who wrote what, as of the end of the window.
///
/// Like file health this reads history from the first commit: knowledge of a
/// file does not start where the window does. Only the `touches` and `churn`
/// of a file are limited to the window. A file renamed without changes keeps
/// its history. Vendored and generated files are left out. With an author
/// filter, only the files that person owns are reported.
///
/// Someone who committed under several addresses is one owner here: people
/// are matched by name, and reported under the first of their identities.
pub fn ownership(store: &Store, w: &Window, filter: &Filter) -> Ownership {
    let range = store.range(w.start, w.end);
    let n = store.paths.len();
    let by_lines = store.stage == Stage::Lines;
    let mut result = Ownership {
        files: Vec::new(),
        dirs: Vec::new(),
        people: Vec::new(),
        last_commit: Vec::new(),
        lines: 0,
        by_lines,
    };
    if store.ch_start.is_empty() {
        return result;
    }

    // A path and the history it carries are two things: after `git mv a b`,
    // b continues a's history, and a new file called a starts from nothing.
    let mut entity: Vec<u32> = (0..n as u32).collect();
    let mut name: Vec<u32> = (0..n as u32).collect();
    let mut alive = vec![false; n];
    let mut size = vec![0i64; n];
    let mut touches = vec![0u32; n];
    let mut churn = vec![0u64; n];
    let mut person_last = vec![0i64; store.people.len()];
    let same_person = people_by_name(store);
    let mut work: Vec<(u32, u32, u32)> = Vec::with_capacity(store.ch_start[range.end] as usize);
    let mut renames = store.ren_from.iter().peekable();

    for i in 0..range.end {
        let author = same_person[store.author[i] as usize];
        person_last[author as usize] = person_last[author as usize].max(store.time[i]);
        let in_window = i >= range.start && filter.commits.as_ref().is_none_or(|set| set[i]);
        for c in store.changes_of(i) {
            let file = store.ch_file[c] as usize;
            let kind = store.ch_kind[c] & !FLAG_BINARY;
            if kind == KIND_REN {
                while renames.peek().is_some_and(|r| (r.0 as usize) < c) {
                    renames.next();
                }
                if let Some((_, from)) = renames.peek().filter(|r| r.0 as usize == c) {
                    let from = *from as usize;
                    let moved = entity[from];
                    entity[file] = moved;
                    name[moved as usize] = file as u32;
                    entity[from] = name.len() as u32;
                    name.push(from as u32);
                    alive.push(false);
                    size.push(0);
                    touches.push(0);
                    churn.push(0);
                }
            }
            let e = entity[file] as usize;
            let (added, removed) = (store.ch_add[c], store.ch_del[c]);
            alive[e] = kind != KIND_DEL;
            size[e] += i64::from(added) - i64::from(removed);
            let weight = if by_lines {
                (added + removed).max(1)
            } else {
                1
            };
            work.push((e as u32, author, weight));
            if in_window {
                touches[e] += 1;
                churn[e] += u64::from(added) + u64::from(removed);
            }
        }
    }

    work.par_sort_unstable();
    let mask = file_mask(store, filter);
    let mut people = vec![0u64; store.people.len()];
    let mut dir_slot: HashMap<&str, u32> = HashMap::new();
    let mut dir_work: Vec<(u32, u32, u64)> = Vec::new();
    let mut run: Vec<(u32, u64)> = Vec::new();
    let mut k = 0;
    while k < work.len() {
        let e = work[k].0 as usize;
        run.clear();
        while k < work.len() && work[k].0 as usize == e {
            let author = work[k].1;
            let mut lines = 0u64;
            while k < work.len() && work[k].0 as usize == e && work[k].1 == author {
                lines += u64::from(work[k].2);
                k += 1;
            }
            run.push((author, lines));
        }
        let path_id = name[e] as usize;
        let path = &store.paths[path_id];
        if !alive[e]
            || mask.as_ref().is_some_and(|m| !m[path_id])
            || is_vendored(path)
            || is_generated(path)
        {
            continue;
        }
        // Ties go to the lower id, so the answer does not depend on thread timing.
        let (owner, owner_lines) = run
            .iter()
            .copied()
            .max_by_key(|(author, lines)| (*lines, std::cmp::Reverse(*author)))
            .expect("a run has at least one change");
        if filter
            .author
            .is_some_and(|a| same_person[a as usize] != owner)
        {
            continue;
        }
        let lines: u64 = run.iter().map(|r| r.1).sum();
        let file = OwnedFile {
            file: path_id as u32,
            size: size[e].max(0) as u64,
            lines,
            authors: run.len() as u32,
            owner,
            owner_lines,
            owner_last: person_last[owner as usize],
            touches: touches[e],
            churn: churn[e],
        };
        let group = dir_group(&store.dirs[store.f_dir[path_id] as usize]);
        let next = dir_slot.len() as u32;
        let slot = *dir_slot.entry(group).or_insert_with(|| {
            result.dirs.push(OwnedDir {
                name: if group.is_empty() {
                    "(root)".to_owned()
                } else {
                    group.to_owned()
                },
                lines: 0,
                authors: 0,
                owner: 0,
                owner_lines: 0,
                owner_last: 0,
                files: 0,
                single_owner: 0,
                churn: 0,
            });
            next
        });
        let dir = &mut result.dirs[slot as usize];
        dir.files += 1;
        dir.single_owner += u32::from(file.single_owner());
        dir.churn += file.churn;
        for (author, lines) in &run {
            people[*author as usize] += lines;
            dir_work.push((slot, *author, *lines));
        }
        result.lines += lines;
        result.files.push(file);
    }

    dir_work.par_sort_unstable();
    let mut k = 0;
    while k < dir_work.len() {
        let slot = dir_work[k].0;
        let dir = &mut result.dirs[slot as usize];
        while k < dir_work.len() && dir_work[k].0 == slot {
            let author = dir_work[k].1;
            let mut lines = 0u64;
            while k < dir_work.len() && dir_work[k].0 == slot && dir_work[k].1 == author {
                lines += dir_work[k].2;
                k += 1;
            }
            dir.lines += lines;
            dir.authors += 1;
            if lines > dir.owner_lines {
                dir.owner = author;
                dir.owner_lines = lines;
            }
        }
        dir.owner_last = person_last[dir.owner as usize];
    }

    result.files.sort_unstable_by_key(|f| {
        (
            std::cmp::Reverse(f.churn),
            std::cmp::Reverse(f.lines),
            f.file,
        )
    });
    result
        .dirs
        .sort_unstable_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.name.cmp(&b.name)));
    result.people = people
        .into_iter()
        .enumerate()
        .filter(|(_, lines)| *lines > 0)
        .map(|(id, lines)| (id as u32, lines))
        .collect();
    result
        .people
        .sort_unstable_by_key(|p| (std::cmp::Reverse(p.1), p.0));
    result.last_commit = person_last;
    result
}

/// For each identity, the first identity with the same name: one person who
/// committed under several addresses.
fn people_by_name(store: &Store) -> Vec<u32> {
    let mut first: HashMap<String, u32> = HashMap::new();
    store
        .people
        .iter()
        .enumerate()
        .map(|(id, person)| {
            let name = person.name.trim().to_lowercase();
            if name.is_empty() {
                id as u32
            } else {
                *first.entry(name).or_insert(id as u32)
            }
        })
        .collect()
}

/// The fewest people who together account for at least half of `people`'s
/// lines: how many would have to leave before most of the knowledge is gone.
pub fn bus_factor(people: &[(u32, u64)]) -> u32 {
    let total: u64 = people.iter().map(|p| p.1).sum();
    let mut covered = 0u64;
    for (k, (_, lines)) in people.iter().enumerate() {
        covered += lines;
        if covered * 2 >= total {
            return k as u32 + 1;
        }
    }
    0
}

// ----------------------------------------------------------------- coupling

pub struct Coupled {
    pub file: u32,
    /// Commits that changed both files.
    pub shared: u32,
    /// Commits that changed this file.
    pub touches: u32,
}

fn counts_for_coupling(store: &Store, filter: &Filter, i: usize) -> bool {
    let changes = store.changes_of(i).len();
    (1..=BULK_COMMIT_FILES).contains(&changes) && !filter.skips(i, store.author[i])
}

/// Files that changed in the same commits as `file` inside the window, most
/// shared commits first, and how many commits changed `file` itself. Commits
/// touching more than [`BULK_COMMIT_FILES`] files are not counted on either side.
pub fn coupled(store: &Store, w: &Window, filter: &Filter, file: u32) -> (u32, Vec<Coupled>) {
    let mut touches = vec![0u32; store.paths.len()];
    let mut shared = vec![0u32; store.paths.len()];
    let mut own = 0u32;
    for i in store.range(w.start, w.end) {
        if !counts_for_coupling(store, filter, i) {
            continue;
        }
        let changes = store.changes_of(i);
        let has = changes.clone().any(|c| store.ch_file[c] == file);
        own += u32::from(has);
        for c in changes {
            let other = store.ch_file[c] as usize;
            touches[other] += 1;
            if has {
                shared[other] += 1;
            }
        }
    }
    let mut rows: Vec<Coupled> = (0..store.paths.len())
        .filter(|f| *f != file as usize && shared[*f] > 0)
        .map(|f| Coupled {
            file: f as u32,
            shared: shared[f],
            touches: touches[f],
        })
        .collect();
    rows.sort_unstable_by_key(|r| (std::cmp::Reverse(r.shared), r.touches, r.file));
    rows.truncate(40);
    (own, rows)
}

pub struct Pair {
    pub a: u32,
    pub b: u32,
    pub shared: u32,
    pub touches_a: u32,
    pub touches_b: u32,
}

impl Pair {
    /// Of the commits that changed either file, the share that changed both.
    pub fn strength(&self) -> f64 {
        f64::from(self.shared) / f64::from(self.touches_a + self.touches_b - self.shared)
    }
}

/// The pairs of files that most reliably change together, among the `hot`
/// most-changed files of the window. A pair needs at least `min_shared`
/// commits in common. Strongest first.
pub fn top_pairs(
    store: &Store,
    w: &Window,
    filter: &Filter,
    hot: usize,
    min_shared: u32,
) -> Vec<Pair> {
    let mask = file_mask(store, filter);
    let range = store.range(w.start, w.end);
    let mut touches = vec![0u32; store.paths.len()];
    for i in range.clone() {
        if counts_for_coupling(store, filter, i) {
            for c in store.changes_of(i) {
                touches[store.ch_file[c] as usize] += 1;
            }
        }
    }
    let mut chosen: Vec<u32> = (0..store.paths.len() as u32)
        .filter(|f| {
            let path = &store.paths[*f as usize];
            touches[*f as usize] >= min_shared.max(1)
                && mask.as_ref().is_none_or(|m| m[*f as usize])
                && !is_vendored(path)
                && !is_generated(path)
        })
        .collect();
    chosen.sort_unstable_by_key(|f| (std::cmp::Reverse(touches[*f as usize]), *f));
    chosen.truncate(hot);
    let n = chosen.len();
    let mut slot = vec![u32::MAX; store.paths.len()];
    for (k, f) in chosen.iter().enumerate() {
        slot[*f as usize] = k as u32;
    }
    let mut together = vec![0u32; n * n];
    let mut present: Vec<u32> = Vec::new();
    for i in range {
        if !counts_for_coupling(store, filter, i) {
            continue;
        }
        present.clear();
        present.extend(
            store
                .changes_of(i)
                .map(|c| slot[store.ch_file[c] as usize])
                .filter(|s| *s != u32::MAX),
        );
        present.sort_unstable();
        present.dedup();
        for (x, a) in present.iter().enumerate() {
            for b in &present[x + 1..] {
                together[*a as usize * n + *b as usize] += 1;
            }
        }
    }
    let mut pairs: Vec<Pair> = Vec::new();
    for a in 0..n {
        for b in a + 1..n {
            let shared = together[a * n + b];
            if shared >= min_shared.max(1) {
                pairs.push(Pair {
                    a: chosen[a],
                    b: chosen[b],
                    shared,
                    touches_a: touches[chosen[a] as usize],
                    touches_b: touches[chosen[b] as usize],
                });
            }
        }
    }
    pairs.sort_unstable_by(|x, y| {
        y.strength()
            .total_cmp(&x.strength())
            .then_with(|| y.shared.cmp(&x.shared))
            .then_with(|| (x.a, x.b).cmp(&(y.a, y.b)))
    });
    pairs.truncate(60);
    pairs
}

// ---------------------------------------------------------------- lifecycle

pub struct Lifecycle {
    /// People with a commit in each bucket.
    pub active: Vec<u32>,
    /// Of those, the ones whose first commit to the repository is in it.
    pub newcomers: Vec<u32>,
    pub people: u32,
    pub new_people: u32,
    /// People active in the window before this one, and how many of them
    /// have no commit in this one. None when there is no window before.
    pub previous_people: Option<u32>,
    pub departed: Option<u32>,
    /// The fewest people who together made four fifths of the commits.
    pub core: u32,
    pub commits: u64,
    /// Commits by the most active person.
    pub top_commits: u64,
}

fn active_in(store: &Store, w: &Window, filter: &Filter, mask: Option<&[bool]>) -> Vec<u32> {
    let mut commits = vec![0u32; store.people.len()];
    for i in store.range(w.start, w.end) {
        let author = store.author[i];
        if !filter.skips(i, author) && commit_lines(store, i, mask).is_some() {
            commits[author as usize] += 1;
        }
    }
    commits
}

/// How the group of contributors changes over the window: who is active per
/// bucket, who is new to the repository, who has left since the window before.
pub fn lifecycle(
    store: &Store,
    w: &Window,
    previous: Option<&Window>,
    filter: &Filter,
    buckets: &Buckets,
) -> Lifecycle {
    let range = store.range(w.start, w.end);
    let nb = buckets.len();
    let mask = file_mask(store, filter);
    let mut first = vec![u32::MAX; store.people.len()];
    for i in 0..range.end {
        let slot = &mut first[store.author[i] as usize];
        if *slot == u32::MAX {
            *slot = i as u32;
        }
    }
    let mut life = Lifecycle {
        active: vec![0; nb],
        newcomers: vec![0; nb],
        people: 0,
        new_people: 0,
        previous_people: None,
        departed: None,
        core: 0,
        commits: 0,
        top_commits: 0,
    };
    let mut commits = vec![0u32; store.people.len()];
    let mut seen_in = vec![0u32; store.people.len()];
    let mut bucket = 0usize;
    for i in range {
        let author = store.author[i];
        if filter.skips(i, author) || commit_lines(store, i, mask.as_deref()).is_none() {
            continue;
        }
        while bucket + 1 < nb && store.time[i] >= buckets.edges[bucket + 1] {
            bucket += 1;
        }
        let a = author as usize;
        commits[a] += 1;
        life.commits += 1;
        if seen_in[a] != bucket as u32 + 1 {
            seen_in[a] = bucket as u32 + 1;
            life.active[bucket] += 1;
        }
        if first[a] == i as u32 {
            life.newcomers[bucket] += 1;
            life.new_people += 1;
        }
    }
    let mut ranked: Vec<u32> = commits.iter().copied().filter(|c| *c > 0).collect();
    ranked.sort_unstable_by_key(|c| std::cmp::Reverse(*c));
    life.people = ranked.len() as u32;
    life.top_commits = ranked.first().map_or(0, |c| u64::from(*c));
    let mut covered = 0u64;
    for (k, c) in ranked.iter().enumerate() {
        covered += u64::from(*c);
        if covered * 5 >= life.commits * 4 {
            life.core = k as u32 + 1;
            break;
        }
    }
    if let Some(previous) = previous {
        let before = active_in(store, previous, filter, mask.as_deref());
        let was = before.iter().filter(|c| **c > 0).count() as u32;
        let gone = before
            .iter()
            .zip(&commits)
            .filter(|(before, now)| **before > 0 && **now == 0)
            .count() as u32;
        life.previous_people = Some(was);
        life.departed = Some(gone);
    }
    life
}

// ----------------------------------------------------------------- releases

/// The commits reachable from `tip` and not from `base`: what `git log
/// base..tip` lists. With no base, everything up to `tip`.
pub fn between(store: &Store, base: Option<usize>, tip: usize) -> Vec<bool> {
    // 0 not visited, 1 part of the range, 2 already in the base
    let mut state = vec![0u8; store.len()];
    let mut stack: Vec<u32> = Vec::new();
    let mut walk = |from: usize, mark: u8, state: &mut Vec<u8>| {
        if state[from] != 0 {
            return;
        }
        state[from] = mark;
        stack.push(from as u32);
        while let Some(i) = stack.pop() {
            let i = i as usize;
            for p in &store.parents[store.par_start[i] as usize..store.par_start[i + 1] as usize] {
                if *p != NO_PARENT && state[*p as usize] == 0 {
                    state[*p as usize] = mark;
                    stack.push(*p);
                }
            }
        }
    };
    if let Some(base) = base {
        walk(base, 2, &mut state);
    }
    walk(tip, 1, &mut state);
    state.into_iter().map(|s| s == 1).collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::git::Progress;
    use crate::store;
    use crate::time::{Clock, Preset};

    fn git(dir: &Path, who: &str, date: &str, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                &format!("user.name={who}"),
                "-c",
                &format!("user.email={who}@example.com"),
            ])
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(args)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git is installed");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn lines(count: usize, tag: &str) -> String {
        (0..count).map(|k| format!("{tag} {k}\n")).collect()
    }

    /// Ada writes most of `core.rs`; Bo adds a little to it and owns `ui.rs`.
    /// `core.rs` and `core_test.rs` always change together. `core.rs` is
    /// renamed to `engine.rs` without changes, and Cy arrives late.
    fn fixture() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let write = |name: &str, body: String| std::fs::write(d.join(name), body).unwrap();
        git(d, "Ada", "", &["init", "-q", "-b", "main"]);

        write("core.rs", lines(40, "a"));
        write("core_test.rs", lines(10, "t"));
        git(d, "Ada", "2025-01-10T10:00:00Z", &["add", "."]);
        git(
            d,
            "Ada",
            "2025-01-10T10:00:00Z",
            &["commit", "-q", "-m", "core"],
        );
        git(d, "Ada", "2025-01-10T10:00:00Z", &["tag", "v1"]);

        write("ui.rs", lines(30, "u"));
        git(d, "Bo", "2025-02-10T10:00:00Z", &["add", "."]);
        git(
            d,
            "Bo",
            "2025-02-10T10:00:00Z",
            &["commit", "-q", "-m", "ui"],
        );

        write("core.rs", lines(40, "a") + &lines(5, "b"));
        write("core_test.rs", lines(10, "t") + &lines(2, "tb"));
        git(
            d,
            "Bo",
            "2025-03-10T10:00:00Z",
            &["commit", "-q", "-am", "core tweak"],
        );

        git(
            d,
            "Ada",
            "2025-04-10T10:00:00Z",
            &["mv", "core.rs", "engine.rs"],
        );
        git(
            d,
            "Ada",
            "2025-04-10T10:00:00Z",
            &["commit", "-q", "-m", "rename"],
        );
        git(d, "Ada", "2025-04-10T10:00:00Z", &["tag", "v2"]);

        write(
            "engine.rs",
            lines(40, "a") + &lines(5, "b") + &lines(15, "c"),
        );
        write(
            "core_test.rs",
            lines(10, "t") + &lines(2, "tb") + &lines(3, "tc"),
        );
        git(
            d,
            "Ada",
            "2025-05-10T10:00:00Z",
            &["commit", "-q", "-am", "more engine"],
        );

        write("notes.md", lines(4, "n"));
        git(d, "Cy", "2025-06-10T10:00:00Z", &["add", "."]);
        git(
            d,
            "Cy",
            "2025-06-10T10:00:00Z",
            &["commit", "-q", "-m", "notes"],
        );

        let info = store::open(d, None).unwrap();
        let (store, _) = store::index(&info, &Progress::default(), true, |_| {}).unwrap();
        ((tmp), (*store).clone())
    }

    fn all(store: &Store) -> (Clock, Window) {
        let clock = Clock::named("UTC").unwrap();
        let bounds = crate::query::bounds(store, &clock, i64::MAX);
        let w = Window::ending(&clock, Preset::All, bounds.last_day, bounds);
        (clock, w)
    }

    fn person(store: &Store, name: &str) -> u32 {
        store.people.iter().position(|p| p.name == name).unwrap() as u32
    }

    fn file(store: &Store, path: &str) -> u32 {
        store.paths.iter().position(|p| p == path).unwrap() as u32
    }

    #[test]
    fn ownership_follows_renames_and_counts_lines() {
        let (_tmp, s) = fixture();
        let (_, w) = all(&s);
        let own = ownership(&s, &w, &Filter::default());
        assert!(own.by_lines);
        let paths: Vec<&str> = own
            .files
            .iter()
            .map(|f| s.paths[f.file as usize].as_str())
            .collect();
        assert!(
            !paths.contains(&"core.rs"),
            "the old name is gone: {paths:?}"
        );
        let engine = own
            .files
            .iter()
            .find(|f| s.paths[f.file as usize] == "engine.rs")
            .unwrap();
        // Ada: 40 at creation + 15 later; Bo: 5. The rename itself changes no lines
        // but counts as one change, by Ada.
        assert_eq!(engine.owner, person(&s, "Ada"));
        assert_eq!(
            (engine.owner_lines, engine.lines, engine.authors),
            (56, 61, 2)
        );
        assert_eq!(engine.size, 60);
        assert_eq!(engine.touches, 4);
        assert!(engine.single_owner());

        let ui = own
            .files
            .iter()
            .find(|f| s.paths[f.file as usize] == "ui.rs")
            .unwrap();
        assert_eq!((ui.owner, ui.authors, ui.size), (person(&s, "Bo"), 1, 30));

        // The whole repository sits in one directory group.
        assert_eq!(own.dirs.len(), 1);
        let root = &own.dirs[0];
        assert_eq!(
            (root.name.as_str(), root.files, root.authors),
            ("(root)", 4, 3)
        );
        assert_eq!(root.owner, person(&s, "Ada"));
        assert_eq!(root.lines, own.lines);
        assert_eq!(own.people[0].0, person(&s, "Ada"));
        assert_eq!(bus_factor(&own.people), 1);
    }

    #[test]
    fn one_person_with_two_addresses_is_one_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, "Ada", "", &["init", "-q", "-b", "main"]);
        std::fs::write(d.join("a.rs"), lines(10, "a")).unwrap();
        git(d, "Ada", "2025-01-10T10:00:00Z", &["add", "."]);
        git(
            d,
            "Ada",
            "2025-01-10T10:00:00Z",
            &["commit", "-q", "-m", "one"],
        );
        std::fs::write(d.join("a.rs"), lines(10, "a") + &lines(6, "b")).unwrap();
        git(
            d,
            "Bo",
            "2025-02-10T10:00:00Z",
            &["commit", "-q", "-am", "two"],
        );
        std::fs::write(
            d.join("a.rs"),
            lines(10, "a") + &lines(6, "b") + &lines(4, "c"),
        )
        .unwrap();
        let work = [
            "-c",
            "user.email=ada@work.example",
            "commit",
            "-q",
            "-am",
            "three",
        ];
        git(d, "Ada", "2025-03-10T10:00:00Z", &work);
        let info = store::open(d, None).unwrap();
        let (s, _) = store::index(&info, &Progress::default(), true, |_| {}).unwrap();
        assert_eq!(s.people.len(), 3, "the store keeps the two addresses apart");

        let (_, w) = all(&s);
        let own = ownership(&s, &w, &Filter::default());
        let file = &own.files[0];
        assert_eq!((file.authors, file.owner_lines, file.lines), (2, 14, 20));
        assert_eq!(s.people[file.owner as usize].name, "Ada");
        assert_eq!(
            file.owner_last, s.time[2],
            "her latest commit, under either address"
        );
        assert_eq!(own.people.len(), 2);
    }

    #[test]
    fn ownership_can_be_limited_to_one_owner_and_one_window() {
        let (_tmp, s) = fixture();
        let (clock, w) = all(&s);
        let bo = Filter {
            author: Some(person(&s, "Bo")),
            ..Default::default()
        };
        let own = ownership(&s, &w, &bo);
        let paths: Vec<&str> = own
            .files
            .iter()
            .map(|f| s.paths[f.file as usize].as_str())
            .collect();
        assert_eq!(paths, ["ui.rs"]);

        // In May alone only two files changed, but ownership still knows the history.
        let may = Window::of(
            &clock,
            Preset::M1,
            clock.day(1_746_057_600),
            clock.day(1_748_649_600),
        );
        let own = ownership(&s, &may, &Filter::default());
        let engine = own
            .files
            .iter()
            .find(|f| s.paths[f.file as usize] == "engine.rs")
            .unwrap();
        assert_eq!((engine.touches, engine.churn, engine.lines), (1, 15, 61));
        assert!(
            own.files
                .iter()
                .all(|f| s.paths[f.file as usize] != "notes.md"),
            "notes.md is from June"
        );
    }

    #[test]
    fn files_that_change_together() {
        let (_tmp, s) = fixture();
        let (_, w) = all(&s);
        // core_test.rs changed three times, each time with core.rs or engine.rs.
        let (own, rows) = coupled(&s, &w, &Filter::default(), file(&s, "core_test.rs"));
        assert_eq!(own, 3);
        let with: Vec<(&str, u32)> = rows
            .iter()
            .map(|r| (s.paths[r.file as usize].as_str(), r.shared))
            .collect();
        assert!(
            with.contains(&("core.rs", 2)) && with.contains(&("engine.rs", 1)),
            "{with:?}"
        );

        let pairs = top_pairs(&s, &w, &Filter::default(), 10, 2);
        assert_eq!(pairs.len(), 1);
        let p = &pairs[0];
        let names = [
            s.paths[p.a as usize].as_str(),
            s.paths[p.b as usize].as_str(),
        ];
        assert!(
            names.contains(&"core.rs") && names.contains(&"core_test.rs"),
            "{names:?}"
        );
        assert_eq!(p.shared, 2);
    }

    #[test]
    fn newcomers_and_people_who_left() {
        let (_tmp, s) = fixture();
        let (clock, _) = all(&s);
        // April to June against January to March.
        let now = Window::of(
            &clock,
            Preset::M1,
            clock.day(1_743_465_600),
            clock.day(1_751_241_600),
        );
        let before = Window::of(
            &clock,
            Preset::M1,
            clock.day(1_735_689_600),
            clock.day(1_743_379_200),
        );
        let buckets = Buckets::fit(&clock, &now, 3);
        let life = lifecycle(&s, &now, Some(&before), &Filter::default(), &buckets);
        assert_eq!(
            (life.people, life.new_people),
            (2, 1),
            "Ada and Cy; Cy is new"
        );
        assert_eq!(
            (life.previous_people, life.departed),
            (Some(2), Some(1)),
            "Bo left"
        );
        assert_eq!(life.commits, 3);
        assert_eq!(life.active.iter().sum::<u32>(), 3);
        assert_eq!(life.newcomers.iter().sum::<u32>(), 1);
        assert_eq!(life.core, 2);
    }

    #[test]
    fn a_release_is_the_commits_between_two_tags() {
        let (_tmp, s) = fixture();
        // Commits are in time order: core, ui, core tweak, rename, more engine, notes.
        let (v1, v2) = (0, 3);
        let range = between(&s, Some(v1), v2);
        assert_eq!(range, [false, true, true, true, false, false]);
        assert_eq!(
            between(&s, None, v1),
            [true, false, false, false, false, false]
        );

        let (clock, w) = all(&s);
        let bounds = crate::query::bounds(&s, &clock, i64::MAX);
        let filter = Filter {
            commits: Some(std::sync::Arc::new(range)),
            ..Default::default()
        };
        let sum = crate::query::summarize(&s, &clock, &w, bounds, &filter, "", 10);
        assert_eq!((sum.totals.commits, sum.totals.authors), (3, 2));
    }
}

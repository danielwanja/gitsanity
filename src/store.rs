//! The whole history as plain columns in memory, commits sorted by time.
//! A time window is then a contiguous index range and every dashboard query
//! is a linear scan over a slice: milliseconds, even for the Linux kernel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};
use gix::ObjectId;
use serde::{Deserialize, Serialize};

use crate::git::{self, DiffChunk, DiffJob, Progress, RawCommit};

pub const NO_PARENT: u32 = u32::MAX;

pub const COMMIT_TYPES: [&str; 13] =
    ["merge", "revert", "feat", "fix", "docs", "refactor", "chore", "test", "build", "ci", "perf", "style", "other"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Stage {
    /// Commit metadata only.
    Commits,
    /// Plus which files each commit touched.
    Files,
    /// Plus added / removed line counts.
    Lines,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Person {
    pub name: String,
    pub email: String,
    pub is_bot: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Store {
    pub version: u32,
    pub head: [u8; 20],
    pub ref_name: String,
    pub stage: Stage,
    /// Commits whose author date was implausible and replaced by the commit date.
    pub repaired_dates: u32,

    // -- commits, ascending by `time`
    pub oid: Vec<[u8; 20]>,
    /// Author time (epoch seconds), the timestamp all analytics use.
    pub time: Vec<i64>,
    pub commit_time: Vec<i64>,
    /// Author's own UTC offset, minutes.
    pub tz: Vec<i16>,
    pub author: Vec<u32>,
    pub committer: Vec<u32>,
    pub ctype: Vec<u8>,
    pub subj_end: Vec<u32>,
    pub subjects: String,
    /// Parents as commit indices; `par_start[i]..par_start[i+1]` into `parents`.
    pub par_start: Vec<u32>,
    pub parents: Vec<u32>,
    /// Lines added / removed per commit (0 until [`Stage::Lines`]).
    pub c_add: Vec<u32>,
    pub c_del: Vec<u32>,

    // -- file changes, grouped by commit: `ch_start[i]..ch_start[i+1]`
    pub ch_start: Vec<u32>,
    pub ch_file: Vec<u32>,
    pub ch_kind: Vec<u8>,
    pub ch_add: Vec<u32>,
    pub ch_del: Vec<u32>,

    // -- dimensions
    pub people: Vec<Person>,
    pub paths: Vec<String>,
    pub f_dir: Vec<u32>,
    pub f_lang: Vec<u16>,
    pub dirs: Vec<String>,
    pub langs: Vec<String>,
}

const VERSION: u32 = 3;

impl Store {
    pub fn len(&self) -> usize {
        self.oid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.oid.is_empty()
    }

    pub fn subject(&self, i: usize) -> &str {
        let start = if i == 0 { 0 } else { self.subj_end[i - 1] as usize };
        &self.subjects[start..self.subj_end[i] as usize]
    }

    pub fn parents_of(&self, i: usize) -> &[u32] {
        &self.parents[self.par_start[i] as usize..self.par_start[i + 1] as usize]
    }

    pub fn is_merge(&self, i: usize) -> bool {
        self.par_start[i + 1] - self.par_start[i] > 1
    }

    pub fn changes_of(&self, i: usize) -> std::ops::Range<usize> {
        if self.ch_start.is_empty() {
            return 0..0;
        }
        self.ch_start[i] as usize..self.ch_start[i + 1] as usize
    }

    pub fn short_oid(&self, i: usize) -> String {
        self.oid[i][..4].iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn hex_oid(&self, i: usize) -> String {
        self.oid[i].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Index range of commits with `start <= time < end`.
    pub fn range(&self, start: i64, end: i64) -> std::ops::Range<usize> {
        self.time.partition_point(|t| *t < start)..self.time.partition_point(|t| *t < end)
    }
}

fn split_ident(ident: &str) -> (&str, &str) {
    match ident.rfind('<') {
        Some(lt) => (ident[..lt].trim(), ident[lt + 1..].trim_end_matches('>').trim()),
        None => (ident.trim(), ""),
    }
}

fn classify(subject: &str, merge: bool) -> u8 {
    if merge {
        return 0;
    }
    let lower = subject.as_bytes().iter().take(12).map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    if lower.starts_with(b"revert") {
        return 1;
    }
    for (i, name) in COMMIT_TYPES.iter().enumerate().skip(2).take(10) {
        if let Some(rest) = lower.strip_prefix(name.as_bytes()) {
            if matches!(rest.first(), Some(b':' | b'(' | b'!')) {
                return i as u8;
            }
        }
    }
    12
}

fn language(ext: &str) -> String {
    let known = match ext {
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "C++",
        "rs" => "Rust",
        "go" => "Go",
        "py" => "Python",
        "rb" | "rake" | "gemspec" => "Ruby",
        "js" | "mjs" | "cjs" | "jsx" => "JavaScript",
        "ts" | "tsx" | "mts" | "cts" => "TypeScript",
        "swift" => "Swift",
        "m" | "mm" => "Objective-C",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "cs" => "C#",
        "ex" | "exs" | "eex" | "heex" => "Elixir",
        "erl" | "hrl" => "Erlang",
        "sh" | "bash" | "zsh" => "Shell",
        "s" | "asm" => "Assembly",
        "md" | "mdx" | "rst" | "txt" | "adoc" => "Docs",
        "json" | "yml" | "yaml" | "toml" | "xml" | "ini" | "cfg" => "Config",
        "html" | "htm" | "erb" => "HTML",
        "css" | "scss" | "sass" | "less" => "CSS",
        "sql" => "SQL",
        "dts" | "dtsi" => "Devicetree",
        "php" => "PHP",
        "lua" => "Lua",
        "zig" => "Zig",
        "" => "Other",
        other => return other.to_ascii_uppercase(),
    };
    known.to_owned()
}

#[derive(Default)]
struct Interner {
    ids: HashMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    fn get(&mut self, s: &str) -> u32 {
        if let Some(id) = self.ids.get(s) {
            return *id;
        }
        let id = self.names.len() as u32;
        self.ids.insert(s.to_owned(), id);
        self.names.push(s.to_owned());
        id
    }
}

/// The result of the commit walk, plus the tree-diff work list derived from it.
pub fn from_commits(mut raw: Vec<RawCommit>, head: ObjectId, ref_name: String) -> (Store, Vec<DiffJob>) {
    // The author date drives every chart, and it is whatever the author's clock
    // said. The kernel has commits "authored" in 1970, 1996 and 2085. A date is
    // treated as broken, and replaced by the commit date, when it is later than
    // its own commit or earlier than the first commit the repository ever
    // recorded, so one bad clock cannot stretch the timeline by decades.
    let first_commit = raw.iter().map(|c| c.commit_time).min().unwrap_or(0);
    let mut repaired = 0;
    for c in &mut raw {
        if c.author_time > c.commit_time + 2 * 86_400 || c.author_time < first_commit - 86_400 {
            c.author_time = c.commit_time;
            repaired += 1;
        }
    }
    raw.sort_unstable_by(|a, b| (a.author_time, a.oid).cmp(&(b.author_time, b.oid)));

    let n = raw.len();
    let index: HashMap<ObjectId, u32> = raw.iter().enumerate().map(|(i, c)| (c.oid, i as u32)).collect();

    // One person per email address (case-insensitive). Their display name is
    // the spelling they used most often, so a one-off typo does not win.
    let mut people: Vec<Person> = Vec::new();
    let mut by_ident: HashMap<String, (u32, u32)> = HashMap::new();
    let mut by_email: HashMap<String, u32> = HashMap::new();
    let mut person = |ident: &str, people: &mut Vec<Person>| -> u32 {
        if let Some((id, uses)) = by_ident.get_mut(ident) {
            *uses += 1;
            return *id;
        }
        let (name, email) = split_ident(ident);
        let key = if email.is_empty() { name.to_lowercase() } else { email.to_lowercase() };
        let id = *by_email.entry(key).or_insert_with(|| {
            let lower = format!("{name} {email}").to_lowercase();
            let is_bot = lower.contains("[bot]") || lower.contains("bot@") || lower.contains("noreply");
            people.push(Person { name: name.to_owned(), email: email.to_owned(), is_bot });
            people.len() as u32 - 1
        });
        by_ident.insert(ident.to_owned(), (id, 1));
        id
    };

    let mut s = Store {
        version: VERSION,
        head: head.as_bytes().try_into().expect("sha1"),
        ref_name,
        stage: Stage::Commits,
        repaired_dates: repaired,
        oid: Vec::with_capacity(n),
        time: Vec::with_capacity(n),
        commit_time: Vec::with_capacity(n),
        tz: Vec::with_capacity(n),
        author: Vec::with_capacity(n),
        committer: Vec::with_capacity(n),
        ctype: Vec::with_capacity(n),
        subj_end: Vec::with_capacity(n),
        subjects: String::new(),
        par_start: Vec::with_capacity(n + 1),
        parents: Vec::with_capacity(n + n / 8),
        c_add: vec![0; n],
        c_del: vec![0; n],
        ch_start: Vec::new(),
        ch_file: Vec::new(),
        ch_kind: Vec::new(),
        ch_add: Vec::new(),
        ch_del: Vec::new(),
        people: Vec::new(),
        paths: Vec::new(),
        f_dir: Vec::new(),
        f_lang: Vec::new(),
        dirs: Vec::new(),
        langs: Vec::new(),
    };
    for c in &raw {
        s.oid.push(c.oid.as_bytes().try_into().expect("sha1"));
        s.time.push(c.author_time);
        s.commit_time.push(c.commit_time);
        s.tz.push(c.author_tz);
        s.author.push(person(&c.author, &mut people));
        s.committer.push(person(&c.committer, &mut people));
        s.ctype.push(classify(&c.subject, c.parents.len() > 1));
        s.subjects.push_str(&c.subject);
        s.subj_end.push(s.subjects.len() as u32);
        s.par_start.push(s.parents.len() as u32);
        s.parents.extend(c.parents.iter().map(|p| index.get(p).copied().unwrap_or(NO_PARENT)));
    }
    // Diff jobs run along first-parent chains (newest first), not in time order:
    // a commit's parent tree is the next job's own tree, so the object cache
    // stays hot. This alone halves the tree-diff time on the Linux kernel.
    let tree = |i: usize| -> [u8; 20] { raw[i].tree.as_bytes().try_into().expect("sha1") };
    let mut jobs = Vec::with_capacity(n);
    let mut queued = vec![false; n];
    for tip in (0..n).rev() {
        let mut i = tip;
        while !queued[i] {
            queued[i] = true;
            let first_parent = raw[i].parents.first().map(|p| index.get(p).copied());
            if raw[i].parents.len() <= 1 {
                match first_parent {
                    None => jobs.push(DiffJob { commit: i as u32, tree: tree(i), parent_tree: None }),
                    Some(Some(p)) => {
                        jobs.push(DiffJob { commit: i as u32, tree: tree(i), parent_tree: Some(tree(p as usize)) })
                    }
                    Some(None) => {} // parent outside a shallow clone: nothing to compare with
                }
            }
            match first_parent {
                Some(Some(p)) => i = p as usize,
                _ => break,
            }
        }
    }
    s.par_start.push(s.parents.len() as u32);
    let mut best = vec![0u32; people.len()];
    let mut idents: Vec<(&String, &(u32, u32))> = by_ident.iter().collect();
    idents.sort_unstable();
    for (ident, (id, uses)) in idents {
        let name = split_ident(ident).0;
        if *uses > best[*id as usize] && !name.is_empty() {
            best[*id as usize] = *uses;
            people[*id as usize].name = name.to_owned();
        }
    }
    s.people = people;
    (s, jobs)
}

/// Blob ids of every change, kept only until line stats are computed.
pub struct ChangeBlobs {
    pub old: Vec<[u8; 20]>,
    pub new: Vec<[u8; 20]>,
    pub kinds: Vec<u8>,
    /// Where each of these (history-ordered) changes sits in the store.
    pub position: Vec<u32>,
}

impl Store {
    /// Fold the tree-diff output in. Chunks arrive in job (history) order; the
    /// store wants changes grouped by commit index.
    pub fn apply_diff(&mut self, chunks: Vec<DiffChunk>) -> ChangeBlobs {
        let total: usize = chunks.iter().map(|c| c.kinds.len()).sum();
        let n = self.len();
        // (commit, chunk, offset in chunk, count, offset overall)
        let mut runs: Vec<(u32, u32, u32, u32, u32)> = Vec::new();
        let mut overall = 0u32;
        for (ci, chunk) in chunks.iter().enumerate() {
            let mut at = 0u32;
            for (commit, count) in &chunk.commits {
                runs.push((*commit, ci as u32, at, *count, overall + at));
                at += count;
            }
            overall += chunk.kinds.len() as u32;
        }
        runs.sort_unstable();

        let mut dirs = Interner::default();
        let mut langs = Interner::default();
        let mut path_ids: HashMap<&[u8], u32> = HashMap::new();
        let mut position = vec![0u32; total];
        self.ch_start = Vec::with_capacity(n + 1);
        self.ch_file = Vec::with_capacity(total);
        self.ch_kind = Vec::with_capacity(total);
        (self.paths, self.f_dir, self.f_lang) = (Vec::new(), Vec::new(), Vec::new());
        for (commit, ci, at, count, overall) in runs {
            let chunk = &chunks[ci as usize];
            while self.ch_start.len() <= commit as usize {
                self.ch_start.push(self.ch_file.len() as u32);
            }
            for k in 0..count {
                let i = (at + k) as usize;
                let raw = chunk.path(i);
                let id = match path_ids.get(raw) {
                    Some(id) => *id,
                    None => {
                        let path = String::from_utf8_lossy(raw).into_owned();
                        let id = self.paths.len() as u32;
                        let dir = path.rfind('/').map(|i| &path[..i]).unwrap_or("");
                        let name = &path[path.rfind('/').map_or(0, |i| i + 1)..];
                        let ext = match name.rfind('.') {
                            Some(dot) if dot > 0 => name[dot + 1..].to_ascii_lowercase(),
                            _ => String::new(),
                        };
                        self.f_dir.push(dirs.get(dir));
                        self.f_lang.push(langs.get(&language(&ext)) as u16);
                        self.paths.push(path);
                        path_ids.insert(raw, id);
                        id
                    }
                };
                position[(overall + k) as usize] = self.ch_file.len() as u32;
                self.ch_file.push(id);
                self.ch_kind.push(chunk.kinds[i]);
            }
        }
        while self.ch_start.len() <= n {
            self.ch_start.push(self.ch_file.len() as u32);
        }
        drop(path_ids);
        let mut blobs = ChangeBlobs {
            old: Vec::with_capacity(total),
            new: Vec::with_capacity(total),
            kinds: Vec::with_capacity(total),
            position,
        };
        for chunk in chunks {
            blobs.old.extend_from_slice(&chunk.old);
            blobs.new.extend_from_slice(&chunk.new);
            blobs.kinds.extend_from_slice(&chunk.kinds);
        }
        self.ch_add = vec![0; total];
        self.ch_del = vec![0; total];
        self.dirs = dirs.names;
        self.langs = langs.names;
        self.stage = Stage::Files;
        blobs
    }

    /// Line counts were computed in history order; file them under their commits.
    pub fn apply_lines(&mut self, blobs: &ChangeBlobs, added: &[u32], removed: &[u32]) {
        for (i, pos) in blobs.position.iter().enumerate() {
            let pos = *pos as usize;
            self.ch_add[pos] = added[i];
            self.ch_del[pos] = removed[i];
            self.ch_kind[pos] = blobs.kinds[i];
        }
        self.finish_lines();
    }

    /// Roll per-change line counts up to their commits.
    fn finish_lines(&mut self) {
        for i in 0..self.len() {
            let r = self.changes_of(i);
            self.c_add[i] = self.ch_add[r.clone()].iter().sum();
            self.c_del[i] = self.ch_del[r].iter().sum();
        }
        self.stage = Stage::Lines;
    }
}

// ------------------------------------------------------------------ indexing

pub struct Timings {
    pub walk: f64,
    pub build: f64,
    pub diff: f64,
    pub merge: f64,
    pub lines: f64,
}

pub struct RepoInfo {
    pub repo: gix::ThreadSafeRepository,
    pub git_dir: PathBuf,
    pub workdir: PathBuf,
    pub head: ObjectId,
    pub ref_name: String,
}

pub fn open(path: &Path) -> Result<RepoInfo> {
    let repo = gix::discover(path).with_context(|| format!("{} is not inside a git repository", path.display()))?;
    let head = repo.head_id().context("repository has no commits yet")?.detach();
    let ref_name = match repo.head_name()? {
        Some(name) => name.shorten().to_string(),
        None => format!("detached at {}", &head.to_hex().to_string()[..8]),
    };
    let git_dir = repo.git_dir().to_owned();
    let workdir = repo.workdir().map(Path::to_owned).unwrap_or_else(|| git_dir.clone());
    Ok(RepoInfo { repo: repo.into_sync(), git_dir, workdir, head, ref_name })
}

/// Run the three passes, handing each finished stage to `publish` so a UI can
/// show commits first, then files, then line counts.
pub fn index(
    info: &RepoInfo,
    progress: &Progress,
    with_lines: bool,
    mut publish: impl FnMut(Arc<Store>),
) -> Result<(Arc<Store>, Timings)> {
    let t = Instant::now();
    let raw = git::walk(&info.repo, &[info.head], progress);
    let walk = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let (store, jobs) = from_commits(raw, info.head, info.ref_name.clone());
    let build = t.elapsed().as_secs_f64();
    publish(Arc::new(store.clone()));
    let mut store = store;

    let t = Instant::now();
    progress.diff_total.store(jobs.len() as u64, Ordering::Relaxed);
    progress.phase.store(git::PHASE_DIFF, Ordering::Relaxed);
    let chunks = git::tree_diff(&info.repo, &jobs, progress);
    let diff = t.elapsed().as_secs_f64();
    drop(jobs);

    let t = Instant::now();
    let mut blobs = store.apply_diff(chunks);
    let merge = t.elapsed().as_secs_f64();

    let t = Instant::now();
    if with_lines {
        progress.line_total.store(blobs.old.len() as u64, Ordering::Relaxed);
        progress.phase.store(git::PHASE_LINES, Ordering::Relaxed);
        publish(Arc::new(store.clone()));
        let (mut added, mut removed) = (vec![0u32; blobs.old.len()], vec![0u32; blobs.old.len()]);
        let ChangeBlobs { old, new, kinds, .. } = &mut blobs;
        git::line_stats(&info.repo, old, new, kinds, &mut added, &mut removed, progress);
        store.apply_lines(&blobs, &added, &removed);
    }
    let lines = t.elapsed().as_secs_f64();
    let store = Arc::new(store);
    progress.phase.store(git::PHASE_DONE, Ordering::Relaxed);
    publish(store.clone());
    Ok((store, Timings { walk, build, diff, merge, lines }))
}

// --------------------------------------------------------------------- cache

fn cache_path(info: &RepoInfo) -> Option<PathBuf> {
    use etcetera::BaseStrategy;
    let base = etcetera::choose_base_strategy().ok()?.cache_dir().join("gitsanity");
    // FNV-1a of the git dir: stable, and no path characters to escape.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in info.git_dir.to_string_lossy().bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let name = info.workdir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Some(base.join(format!("{name}-{h:016x}.gsx")))
}

/// A finished index for exactly this HEAD, if one was saved earlier.
pub fn load_cache(info: &RepoInfo) -> Option<Store> {
    let file = std::fs::File::open(cache_path(info)?).ok()?;
    let reader = zstd::Decoder::new(file).ok()?;
    let store: Store = bincode::deserialize_from(std::io::BufReader::with_capacity(1 << 20, reader)).ok()?;
    (store.version == VERSION && store.head == info.head.as_bytes() && store.stage == Stage::Lines).then_some(store)
}

pub fn save_cache(info: &RepoInfo, store: &Store) -> Result<PathBuf> {
    let path = cache_path(info).context("no cache directory")?;
    std::fs::create_dir_all(path.parent().expect("has parent"))?;
    let tmp = path.with_extension("tmp");
    let mut enc = zstd::Encoder::new(std::fs::File::create(&tmp)?, 1)?;
    enc.multithread(4).ok();
    bincode::serialize_into(std::io::BufWriter::with_capacity(1 << 20, &mut enc), store)?;
    enc.finish()?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::git::{FLAG_BINARY, KIND_ADD, KIND_DEL, KIND_MOD, KIND_REN};
    use crate::query::{self, Filter};
    use crate::time::{Clock, Preset, Window};

    fn git(dir: &Path, date: &str, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com", "-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git is installed");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Five commits, one of them a merge, with a rename, a deletion and a binary file.
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git(d, "", &["init", "-q", "-b", "main"]);
        std::fs::create_dir(d.join("src")).unwrap();
        std::fs::write(d.join("src/a.rs"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(d.join("notes.md"), "hello\n").unwrap();
        git(d, "2026-03-07T23:30:00-07:00", &["add", "."]);
        git(d, "2026-03-07T23:30:00-07:00", &["commit", "-q", "-m", "feat: first"]);

        // 00:30 in Tokyo on Mar 9 is still Mar 8 in Denver (and in UTC).
        std::fs::write(d.join("src/a.rs"), "one\n2\nthree\nfour\n").unwrap();
        std::fs::write(d.join("logo.bin"), [0u8, 1, 2, 3]).unwrap();
        git(d, "2026-03-09T00:30:00+09:00", &["add", "."]);
        git(d, "2026-03-09T00:30:00+09:00", &["commit", "-q", "-m", "fix(parser): second\n\nbody text"]);

        git(d, "", &["checkout", "-q", "-b", "side"]);
        git(d, "2026-03-10T09:00:00-06:00", &["mv", "notes.md", "README.md"]);
        git(d, "2026-03-10T09:00:00-06:00", &["commit", "-q", "-m", "docs: rename notes"]);
        git(d, "", &["checkout", "-q", "main"]);
        git(d, "2026-03-10T10:00:00-06:00", &["rm", "-q", "logo.bin"]);
        git(d, "2026-03-10T10:00:00-06:00", &["commit", "-q", "-m", "remove logo"]);
        git(d, "2026-03-11T12:00:00-06:00", &["merge", "-q", "--no-ff", "side", "-m", "Merge branch 'side'"]);
        tmp
    }

    #[test]
    fn indexes_a_small_repository_end_to_end() {
        let tmp = fixture();
        let info = open(tmp.path()).unwrap();
        assert_eq!(info.ref_name, "main");
        let (s, _) = index(&info, &Progress::default(), true, |_| {}).unwrap();

        assert_eq!(s.len(), 5);
        assert_eq!(s.stage, Stage::Lines);
        assert_eq!(s.people.len(), 1);
        assert_eq!(s.people[0].name, "Ada");
        let subjects: Vec<&str> = (0..5).map(|i| s.subject(i)).collect();
        assert_eq!(subjects, ["feat: first", "fix(parser): second", "docs: rename notes", "remove logo", "Merge branch 'side'"]);
        let types: Vec<&str> = s.ctype.iter().map(|t| COMMIT_TYPES[*t as usize]).collect();
        assert_eq!(types, ["feat", "fix", "docs", "other", "merge"]);
        assert_eq!(s.tz, [-420, 540, -360, -360, -360]);

        let change = |i: usize| -> Vec<(&str, u8, u32, u32)> {
            let mut v: Vec<_> = s
                .changes_of(i)
                .map(|c| (s.paths[s.ch_file[c] as usize].as_str(), s.ch_kind[c], s.ch_add[c], s.ch_del[c]))
                .collect();
            v.sort();
            v
        };
        assert_eq!(change(0), [("notes.md", KIND_ADD, 1, 0), ("src/a.rs", KIND_ADD, 3, 0)]);
        assert_eq!(change(1), [("logo.bin", KIND_ADD | FLAG_BINARY, 0, 0), ("src/a.rs", KIND_MOD, 2, 1)]);
        assert_eq!(change(2), [("README.md", KIND_REN, 0, 0)]);
        assert_eq!(change(3), [("logo.bin", KIND_DEL | FLAG_BINARY, 0, 0)]);
        assert!(change(4).is_empty(), "merges carry no file changes");
        assert_eq!((s.c_add.iter().sum::<u32>(), s.c_del.iter().sum::<u32>()), (6, 1));
        assert_eq!(s.parents_of(4).len(), 2);

        // The store survives serialisation (what the on-disk cache does).
        let bytes = bincode::serialize(&*s).unwrap();
        let back: Store = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.subjects, s.subjects);
        assert_eq!(back.ch_add, s.ch_add);
    }

    #[test]
    fn days_follow_the_viewers_time_zone() {
        let tmp = fixture();
        let info = open(tmp.path()).unwrap();
        let (s, _) = index(&info, &Progress::default(), true, |_| {}).unwrap();
        let now = *s.time.last().unwrap();
        let per_day = |zone: &str| {
            let clock = Clock::named(zone).unwrap();
            let bounds = query::bounds(&s, &clock, now);
            let w = Window::ending(&clock, Preset::All, bounds.last_day, bounds);
            let sum = query::summarize(&s, &clock, &w, bounds, &Filter::default(), "", 100);
            (w.label(), sum.daily, sum.totals.commits)
        };
        // Denver: Mar 7 (23:30), Mar 8 (the commit made in Tokyo, 08:30 local, on the DST day), Mar 10 twice, Mar 11.
        assert_eq!(per_day("America/Denver"), ("Mar 7 – Mar 11, 2026".to_owned(), vec![1, 1, 0, 2, 1], 5));
        // In Tokyo every one of them lands a calendar day later.
        assert_eq!(per_day("Asia/Tokyo"), ("Mar 8 – Mar 12, 2026".to_owned(), vec![1, 1, 0, 2, 1], 5));
        // UTC, which is what the old day_sk rollups used.
        assert_eq!(per_day("UTC"), ("Mar 8 – Mar 11, 2026".to_owned(), vec![2, 0, 2, 1], 5));

        // Searching by local date finds what the user sees on that date.
        let clock = Clock::named("America/Denver").unwrap();
        let bounds = query::bounds(&s, &clock, now);
        let w = Window::ending(&clock, Preset::All, bounds.last_day, bounds);
        let found = query::summarize(&s, &clock, &w, bounds, &Filter::default(), "2026-03-07", 100);
        assert_eq!(found.commits.len(), 1);
        assert_eq!(s.subject(found.commits[0] as usize), "feat: first");
    }
}

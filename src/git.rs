//! History extraction straight from the object database (gitoxide), fanned out
//! over all cores. Three passes, each usable on its own so the UI can fill in
//! progressively:
//!
//! 1. [`walk`]       every commit reachable from the tips (parallel graph walk)
//! 2. [`tree_diff`]  which files each non-merge commit touched (parallel tree diff)
//! 3. [`line_stats`] added / removed lines per file change (parallel blob diff)

use std::cell::RefCell;
use std::cmp::Ordering as Cmp;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::diff::blob::{Algorithm, Diff, InternedInput};
use gix::objs::Find;
use rayon::prelude::*;
use thread_local::ThreadLocal;

/// Shared progress counters, read by the UI while a pass runs.
#[derive(Default)]
pub struct Progress {
    pub phase: AtomicU8,
    pub commits: AtomicU64,
    pub diffed: AtomicU64,
    pub diff_total: AtomicU64,
    pub lined: AtomicU64,
    pub line_total: AtomicU64,
    pub cancel: AtomicBool,
}

pub const PHASE_WALK: u8 = 0;
pub const PHASE_DIFF: u8 = 1;
pub const PHASE_LINES: u8 = 2;
pub const PHASE_DONE: u8 = 3;

pub struct RawCommit {
    pub oid: ObjectId,
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    /// `Name <email>` exactly as recorded.
    pub author: String,
    pub committer: String,
    pub author_time: i64,
    /// Author's UTC offset in minutes.
    pub author_tz: i16,
    pub commit_time: i64,
    pub subject: String,
}

fn parse_sig(line: &[u8]) -> Option<(String, i64, i16)> {
    // `Name <email> 1700000000 +0100`
    let mut it = line.rsplitn(3, |b| *b == b' ');
    let tz = it.next()?;
    let time = it.next()?;
    let ident = it.next().unwrap_or_default();
    let time: i64 = std::str::from_utf8(time).ok()?.parse().ok()?;
    let tz: i32 = std::str::from_utf8(tz).ok()?.parse().unwrap_or(0);
    let minutes = (tz.abs() / 100) * 60 + tz.abs() % 100;
    let minutes = if tz < 0 { -minutes } else { minutes };
    Some((String::from_utf8_lossy(ident).into_owned(), time, minutes.clamp(-1440, 1440) as i16))
}

fn parse_commit(oid: ObjectId, data: &[u8]) -> Option<RawCommit> {
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer = None;
    let mut rest = data;
    loop {
        let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        let line = &rest[..end];
        rest = rest.get(end + 1..).unwrap_or_default();
        if line.is_empty() {
            break;
        }
        if let Some(hex) = line.strip_prefix(b"tree ") {
            tree = ObjectId::from_hex(hex).ok();
        } else if let Some(hex) = line.strip_prefix(b"parent ") {
            parents.push(ObjectId::from_hex(hex).ok()?);
        } else if let Some(sig) = line.strip_prefix(b"author ") {
            author = parse_sig(sig);
        } else if let Some(sig) = line.strip_prefix(b"committer ") {
            committer = parse_sig(sig);
        }
        if rest.is_empty() {
            break;
        }
    }
    // Subject: first paragraph of the message, folded onto one line (git's %s).
    let msg = rest;
    let start = msg.iter().position(|b| *b != b'\n').unwrap_or(msg.len());
    let msg = &msg[start..];
    let end = msg.windows(2).position(|w| w == b"\n\n").unwrap_or(msg.len());
    let subject = String::from_utf8_lossy(&msg[..end]).trim_end().replace('\n', " ");

    let (author, author_time, author_tz) = author.unwrap_or_default();
    let (committer, commit_time, _) = committer.unwrap_or_else(|| (author.clone(), author_time, author_tz));
    Some(RawCommit { oid, tree: tree?, parents, author, committer, author_time, author_tz, commit_time, subject })
}

/// Everything in a commit message after its subject paragraph.
pub fn message_body(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    let Some((_, message)) = text.split_once("\n\n") else { return String::new() };
    match message.trim_start_matches('\n').split_once("\n\n") {
        Some((_, body)) => body.trim().to_owned(),
        None => String::new(),
    }
}

struct Worker {
    repo: gix::Repository,
    buf: Vec<u8>,
    out: Vec<RawCommit>,
}

struct WalkShared<'a> {
    repo: &'a gix::ThreadSafeRepository,
    seen: Vec<Mutex<HashSet<ObjectId>>>,
    workers: ThreadLocal<RefCell<Worker>>,
    progress: &'a Progress,
}

impl WalkShared<'_> {
    /// True the first time an id is offered.
    fn claim(&self, id: &ObjectId) -> bool {
        self.seen[id.as_bytes()[0] as usize].lock().expect("poisoned").insert(*id)
    }

    fn visit<'s>(&'s self, mut id: ObjectId, scope: &rayon::Scope<'s>) {
        let cell = self.workers.get_or(|| {
            RefCell::new(Worker { repo: self.repo.to_thread_local(), buf: Vec::new(), out: Vec::new() })
        });
        // Follow the first-parent chain inline; hand side branches to other cores.
        loop {
            if self.progress.cancel.load(Ordering::Relaxed) {
                return;
            }
            let next = {
                let mut w = cell.borrow_mut();
                let Worker { repo, buf, out } = &mut *w;
                // A missing object is the edge of a shallow clone: stop quietly.
                let Ok(Some(data)) = repo.objects.try_find(&id, buf) else { return };
                if data.kind != gix::object::Kind::Commit {
                    return;
                }
                let Some(commit) = parse_commit(id, data.data) else { return };
                let mut next = None;
                for p in &commit.parents {
                    if self.claim(p) {
                        if next.is_none() {
                            next = Some(*p);
                        } else {
                            let p = *p;
                            scope.spawn(move |s| self.visit(p, s));
                        }
                    }
                }
                out.push(commit);
                next
            };
            self.progress.commits.fetch_add(1, Ordering::Relaxed);
            match next {
                Some(p) => id = p,
                None => return,
            }
        }
    }
}

/// Every commit reachable from `tips`, in no particular order.
pub fn walk(repo: &gix::ThreadSafeRepository, tips: &[ObjectId], progress: &Progress) -> Vec<RawCommit> {
    let mut shared = WalkShared {
        repo,
        seen: (0..256).map(|_| Mutex::new(HashSet::new())).collect(),
        workers: ThreadLocal::new(),
        progress,
    };
    rayon::scope(|s| {
        for tip in tips {
            if shared.claim(tip) {
                let shared = &shared;
                s.spawn(move |s| shared.visit(*tip, s));
            }
        }
    });
    let mut out = Vec::new();
    for w in shared.workers.iter_mut() {
        out.append(&mut w.get_mut().out);
    }
    out
}

// ---------------------------------------------------------------- tree diff

pub const KIND_ADD: u8 = 1;
pub const KIND_MOD: u8 = 2;
pub const KIND_DEL: u8 = 3;
pub const KIND_REN: u8 = 4;
/// Set on top of the kind once the blobs turned out to be binary.
pub const FLAG_BINARY: u8 = 0x80;

const NO_OID: [u8; 20] = [0; 20];
const MODE_TREE: u32 = 0o40000;
const MODE_GITLINK: u32 = 0o160000;

/// What one tree-diff job returns: changes for a run of commits, paths in one arena.
#[derive(Default)]
pub struct DiffChunk {
    /// (commit index, number of changes) for each commit that had any.
    pub commits: Vec<(u32, u32)>,
    pub kinds: Vec<u8>,
    pub old: Vec<[u8; 20]>,
    pub new: Vec<[u8; 20]>,
    /// End offset of each path in `arena`.
    pub path_ends: Vec<u32>,
    pub arena: Vec<u8>,
}

impl DiffChunk {
    pub fn path(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.path_ends[i - 1] as usize };
        &self.arena[start..self.path_ends[i] as usize]
    }
}

struct Entry<'a> {
    mode: u32,
    name: &'a [u8],
    oid: &'a [u8],
}

fn next_entry<'a>(data: &mut &'a [u8]) -> Option<Entry<'a>> {
    let d = *data;
    let sp = d.iter().position(|b| *b == b' ')?;
    let mode = d[..sp].iter().fold(0u32, |m, b| m * 8 + b.wrapping_sub(b'0') as u32);
    let rest = &d[sp + 1..];
    let nul = rest.iter().position(|b| *b == 0)?;
    let oid = rest.get(nul + 1..nul + 21)?;
    *data = &rest[nul + 21..];
    Some(Entry { mode, name: &rest[..nul], oid })
}

/// Git's tree order: directories sort as if their name ended in `/`.
fn cmp_entries(a: &Entry, b: &Entry) -> Cmp {
    let n = a.name.len().min(b.name.len());
    match a.name[..n].cmp(&b.name[..n]) {
        Cmp::Equal => {}
        o => return o,
    }
    let tail = |e: &Entry| e.name.get(n).copied().unwrap_or(if e.mode == MODE_TREE { b'/' } else { 0 });
    tail(a).cmp(&tail(b))
}

fn to_oid(bytes: &[u8]) -> [u8; 20] {
    bytes.try_into().expect("sha1 object id")
}

struct Differ<'r> {
    odb: &'r gix::OdbHandle,
    bufs: Vec<Vec<u8>>,
    path: Vec<u8>,
    out: DiffChunk,
}

impl Differ<'_> {
    fn load(&mut self, id: Option<[u8; 20]>, slot: usize) -> Result<Vec<u8>> {
        if self.bufs.len() <= slot {
            self.bufs.resize_with(slot + 1, Vec::new);
        }
        let mut buf = std::mem::take(&mut self.bufs[slot]);
        buf.clear();
        if let Some(id) = id {
            let id = ObjectId::from(id);
            self.odb.try_find(&id, &mut buf).map_err(|e| anyhow::anyhow!("{e}"))?.context("missing tree")?;
        }
        Ok(buf)
    }

    fn diff(&mut self, a: Option<[u8; 20]>, b: Option<[u8; 20]>, depth: usize) -> Result<()> {
        let abuf = self.load(a, depth * 2)?;
        let bbuf = self.load(b, depth * 2 + 1)?;
        let res = self.merge(&abuf, &bbuf, depth);
        self.bufs[depth * 2] = abuf;
        self.bufs[depth * 2 + 1] = bbuf;
        res
    }

    fn push(&mut self, name: &[u8], kind: u8, old: [u8; 20], new: [u8; 20]) {
        self.out.arena.extend_from_slice(&self.path);
        self.out.arena.extend_from_slice(name);
        self.out.path_ends.push(self.out.arena.len() as u32);
        self.out.kinds.push(kind);
        self.out.old.push(old);
        self.out.new.push(new);
    }

    /// An entry present on one side only.
    fn one_sided(&mut self, e: &Entry, added: bool, depth: usize) -> Result<()> {
        if e.mode == MODE_TREE {
            let len = self.path.len();
            self.path.extend_from_slice(e.name);
            self.path.push(b'/');
            let id = Some(to_oid(e.oid));
            let res = if added { self.diff(None, id, depth + 1) } else { self.diff(id, None, depth + 1) };
            self.path.truncate(len);
            res
        } else {
            if e.mode != MODE_GITLINK {
                if added {
                    self.push(e.name, KIND_ADD, NO_OID, to_oid(e.oid));
                } else {
                    self.push(e.name, KIND_DEL, to_oid(e.oid), NO_OID);
                }
            }
            Ok(())
        }
    }

    fn merge(&mut self, mut a: &[u8], mut b: &[u8], depth: usize) -> Result<()> {
        let mut ea = next_entry(&mut a);
        let mut eb = next_entry(&mut b);
        loop {
            let order = match (&ea, &eb) {
                (None, None) => return Ok(()),
                (Some(_), None) => Cmp::Less,
                (None, Some(_)) => Cmp::Greater,
                (Some(x), Some(y)) => cmp_entries(x, y),
            };
            match order {
                Cmp::Less => {
                    self.one_sided(ea.as_ref().expect("checked"), false, depth)?;
                    ea = next_entry(&mut a);
                }
                Cmp::Greater => {
                    self.one_sided(eb.as_ref().expect("checked"), true, depth)?;
                    eb = next_entry(&mut b);
                }
                Cmp::Equal => {
                    let (x, y) = (ea.as_ref().expect("checked"), eb.as_ref().expect("checked"));
                    if x.oid != y.oid || x.mode != y.mode {
                        if x.mode == MODE_TREE {
                            let len = self.path.len();
                            self.path.extend_from_slice(x.name);
                            self.path.push(b'/');
                            self.diff(Some(to_oid(x.oid)), Some(to_oid(y.oid)), depth + 1)?;
                            self.path.truncate(len);
                        } else if x.mode != MODE_GITLINK && y.mode != MODE_GITLINK {
                            self.push(x.name, KIND_MOD, to_oid(x.oid), to_oid(y.oid));
                        }
                    }
                    ea = next_entry(&mut a);
                    eb = next_entry(&mut b);
                }
            }
        }
    }

    /// Pair a deletion and an addition of the very same blob as a rename.
    /// (Content-similarity renames are not detected yet.)
    fn pair_exact_renames(&mut self, from: usize) {
        if std::env::var_os("GITSANITY_NO_RENAMES").is_some() {
            return;
        }
        let out = &mut self.out;
        let n = out.kinds.len();
        if !(out.kinds[from..].contains(&KIND_DEL) && out.kinds[from..].contains(&KIND_ADD)) {
            return;
        }
        let mut deleted: HashMap<[u8; 20], usize> = HashMap::new();
        for i in from..n {
            if out.kinds[i] == KIND_DEL {
                deleted.entry(out.old[i]).or_insert(i);
            }
        }
        let mut drop = Vec::new();
        for i in from..n {
            if out.kinds[i] == KIND_ADD
                && let Some(d) = deleted.remove(&out.new[i]) {
                    out.kinds[i] = KIND_REN;
                    out.old[i] = out.new[i];
                    drop.push(d);
                }
        }
        if drop.is_empty() {
            return;
        }
        drop.sort_unstable();
        // Rebuild the tail without the dropped deletions.
        let mut arena = Vec::new();
        let mut ends = Vec::new();
        let (mut kinds, mut old, mut new) = (Vec::new(), Vec::new(), Vec::new());
        let base = if from == 0 { 0 } else { out.path_ends[from - 1] as usize };
        for i in from..n {
            if drop.binary_search(&i).is_ok() {
                continue;
            }
            arena.extend_from_slice(out.path(i));
            ends.push((base + arena.len()) as u32);
            kinds.push(out.kinds[i]);
            old.push(out.old[i]);
            new.push(out.new[i]);
        }
        out.arena.truncate(base);
        out.arena.extend_from_slice(&arena);
        out.path_ends.truncate(from);
        out.path_ends.extend(ends);
        out.kinds.truncate(from);
        out.kinds.extend(kinds);
        out.old.truncate(from);
        out.old.extend(old);
        out.new.truncate(from);
        out.new.extend(new);
    }
}

/// One commit to diff: its tree and its first parent's tree (None for a root).
/// Merge commits are skipped by the caller, matching `git log` without `-m`.
pub struct DiffJob {
    pub commit: u32,
    pub tree: [u8; 20],
    pub parent_tree: Option<[u8; 20]>,
}

pub fn tree_diff(repo: &gix::ThreadSafeRepository, jobs: &[DiffJob], progress: &Progress) -> Vec<DiffChunk> {
    jobs.par_chunks(128)
        .map_init(
            || {
                let mut r = repo.to_thread_local();
                r.object_cache_size_if_unset(32 * 1024 * 1024);
                r
            },
            |repo, chunk| {
                let mut d = Differ { odb: &repo.objects, bufs: Vec::new(), path: Vec::new(), out: DiffChunk::default() };
                for job in chunk {
                    if progress.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let from = d.out.kinds.len();
                    d.path.clear();
                    // A tree we cannot read (shallow or damaged history) yields no changes.
                    if d.diff(job.parent_tree, Some(job.tree), 0).is_err() {
                        d.out.kinds.truncate(from);
                        d.out.old.truncate(from);
                        d.out.new.truncate(from);
                        d.out.path_ends.truncate(from);
                        continue;
                    }
                    d.pair_exact_renames(from);
                    let n = d.out.kinds.len() - from;
                    if n > 0 {
                        d.out.commits.push((job.commit, n as u32));
                    }
                }
                progress.diffed.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                d.out
            },
        )
        .collect()
}

// --------------------------------------------------------------- line stats

fn is_binary(d: &[u8]) -> bool {
    d[..d.len().min(8000)].contains(&0)
}

fn count_lines(d: &[u8]) -> u32 {
    let n = d.iter().filter(|b| **b == b'\n').count() as u32;
    if d.last().is_some_and(|b| *b != b'\n') { n + 1 } else { n }
}

/// Fill `added` / `removed` for every change; sets [`FLAG_BINARY`] on `kinds`
/// where a side is binary (those count as 0 / 0, like `git log --numstat`).
pub fn line_stats(
    repo: &gix::ThreadSafeRepository,
    old: &[[u8; 20]],
    new: &[[u8; 20]],
    kinds: &mut [u8],
    added: &mut [u32],
    removed: &mut [u32],
    progress: &Progress,
) {
    const CHUNK: usize = 256;
    (kinds.par_chunks_mut(CHUNK), added.par_chunks_mut(CHUNK), removed.par_chunks_mut(CHUNK))
        .into_par_iter()
        .enumerate()
        .for_each_init(
            || (repo.to_thread_local(), Vec::new(), Vec::new()),
            |(repo, a, b), (ci, (kinds, added, removed))| {
                if progress.cancel.load(Ordering::Relaxed) {
                    return;
                }
                let base = ci * CHUNK;
                for i in 0..kinds.len() {
                    let (o, n) = (old[base + i], new[base + i]);
                    let kind = kinds[i];
                    if kind == KIND_REN {
                        continue; // identical blob: nothing added or removed
                    }
                    a.clear();
                    b.clear();
                    if o != NO_OID && repo.objects.try_find(&ObjectId::from(o), a).ok().flatten().is_none() {
                        continue;
                    }
                    if n != NO_OID && repo.objects.try_find(&ObjectId::from(n), b).ok().flatten().is_none() {
                        continue;
                    }
                    if is_binary(a) || is_binary(b) {
                        kinds[i] |= FLAG_BINARY;
                        continue;
                    }
                    match kind {
                        KIND_ADD => added[i] = count_lines(b),
                        KIND_DEL => removed[i] = count_lines(a),
                        _ => {
                            let input = InternedInput::new(a.as_slice(), b.as_slice());
                            let diff = Diff::compute(Algorithm::Myers, &input);
                            added[i] = diff.count_additions();
                            removed[i] = diff.count_removals();
                        }
                    }
                }
                progress.lined.fetch_add(kinds.len() as u64, Ordering::Relaxed);
            },
        );
}

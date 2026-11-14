//! Ownership and risk, files that change together, movers, the contributor
//! lifecycle and releases.

use std::collections::HashMap;
use std::sync::Arc;

use gitsanity::insight;
use gitsanity::query;
use gitsanity::store::{self, RefKind, Stage};
use gitsanity::time::Buckets;

use crate::{Query, Repo, Res, failed, hex};

/// An owner counts as gone quiet after this many days without a commit.
const INACTIVE_DAYS: i64 = 180;

#[derive(uniffi::Record)]
pub struct OwnedFile {
    pub path: String,
    /// Lines in the file now.
    pub size: u64,
    /// Lines changed over its history, by everyone.
    pub lines: u64,
    pub authors: u32,
    pub owner: String,
    pub owner_id: u32,
    /// The owner's part of `lines`, 0...1.
    pub owner_share: f64,
    /// The owner's latest commit anywhere in the repository.
    pub owner_last: i64,
    pub owner_inactive: bool,
    /// Changes inside the window.
    pub touches: u32,
    pub churn: u64,
    /// Changes in the window times size, as a share of the largest such value.
    pub hotspot: f64,
}

#[derive(uniffi::Record)]
pub struct OwnedDir {
    pub name: String,
    pub lines: u64,
    pub authors: u32,
    pub owner: String,
    pub owner_id: u32,
    pub owner_share: f64,
    pub owner_inactive: bool,
    pub files: u32,
    pub single_owner_files: u32,
    /// Lines changed inside the window.
    pub churn: u64,
}

#[derive(uniffi::Record)]
pub struct OwnerShare {
    pub id: u32,
    pub name: String,
    pub lines: u64,
    pub share: f64,
    pub last: i64,
    pub inactive: bool,
}

#[derive(uniffi::Record)]
pub struct OwnershipReport {
    /// Files that exist at the end of the window (vendored and generated ones left out).
    pub files: u32,
    pub lines: u64,
    /// False while line counts are still being read: shares are then by number of changes.
    pub by_lines: bool,
    /// The fewest people who account for half of all changed lines.
    pub bus_factor: u32,
    /// Files one person wrote at least four fifths of.
    pub single_owner_files: u32,
    /// Of those, the ones whose owner has not committed for `inactive_days`,
    /// and the lines they hold today.
    pub at_risk_files: u32,
    pub at_risk_lines: u64,
    pub inactive_days: u32,
    pub people: Vec<OwnerShare>,
    /// Directories, largest history first.
    pub dirs: Vec<OwnedDir>,
    /// Single-owner files changed in the window, most changed first.
    pub single_owner: Vec<OwnedFile>,
    /// Single-owner files whose owner has gone quiet, largest first.
    pub at_risk: Vec<OwnedFile>,
    /// Large files that keep changing: changes in the window times size.
    pub hotspots: Vec<OwnedFile>,
}

#[derive(uniffi::Record)]
pub struct CoupledFile {
    pub path: String,
    /// Commits that changed both files.
    pub shared: u32,
    /// Commits that changed this file.
    pub touches: u32,
    /// Of the selected file's commits, the share that also changed this one.
    pub share: f64,
}

#[derive(uniffi::Record)]
pub struct Coupling {
    pub path: String,
    /// Commits that changed the selected file (bulk commits left out).
    pub commits: u32,
    pub files: Vec<CoupledFile>,
}

#[derive(uniffi::Record)]
pub struct FilePair {
    pub a: String,
    pub b: String,
    pub shared: u32,
    pub touches_a: u32,
    pub touches_b: u32,
    /// Of the commits that changed either file, the share that changed both.
    pub strength: f64,
    pub same_folder: bool,
}

#[derive(uniffi::Record)]
pub struct Mover {
    pub name: String,
    pub now: u64,
    pub before: u64,
}

#[derive(uniffi::Record)]
pub struct Movers {
    /// Lines changed when true, number of changes while lines are not known yet.
    pub by_lines: bool,
    /// Directories with more activity than in the window before, biggest rise first.
    pub rising: Vec<Mover>,
    pub falling: Vec<Mover>,
}

#[derive(uniffi::Record)]
pub struct LifecycleReport {
    pub labels: Vec<String>,
    /// People with a commit in each bucket.
    pub active: Vec<u32>,
    /// Of those, the ones whose first commit to the repository is in it.
    pub newcomers: Vec<u32>,
    pub people: u32,
    pub new_people: u32,
    /// People active in the window before, and how many of them are absent now.
    pub previous_people: Option<u32>,
    pub departed: Option<u32>,
    /// The fewest people who together made four fifths of the commits.
    pub core: u32,
    pub commits: u64,
    /// Commits by the most active person.
    pub top_commits: u64,
}

#[derive(uniffi::Record)]
pub struct Release {
    pub name: String,
    /// The tagged commit, for [`Query::release_tip`] and `release_base`.
    pub commit: u32,
    pub sha: String,
    /// Author time of the tagged commit.
    pub time: i64,
}

#[uniffi::export]
impl Repo {
    /// Who wrote what as of the end of the query's window, where knowledge
    /// rests on one person, and which large files keep changing.
    pub fn ownership(&self, query: Query, limit: u32) -> Res<OwnershipReport> {
        let store = self.loaded()?;
        if store.stage == Stage::Commits {
            return Err(failed("files are not known yet"));
        }
        let (_, _, window) = self.frame(&store, &query)?;
        let key = format!(
            "{:p}/{:?}/{}/{}/{:?}/{:?}/{:?}/{:?}",
            Arc::as_ptr(&store),
            store.stage,
            window.start,
            window.end,
            query.author,
            query.path,
            query.release_base,
            query.release_tip
        );
        let cached = self
            .owned
            .lock()
            .expect("poisoned")
            .as_ref()
            .filter(|c| c.0 == key)
            .map(|c| c.1.clone());
        let own = match cached {
            Some(own) => own,
            None => {
                let own = Arc::new(insight::ownership(
                    &store,
                    &window,
                    &self.filter(&store, &query)?,
                ));
                *self.owned.lock().expect("poisoned") = Some((key, own.clone()));
                own
            }
        };

        let quiet_before = window.end - INACTIVE_DAYS * 86_400;
        let peak = own
            .files
            .iter()
            .map(|f| u64::from(f.touches) * f.size)
            .max()
            .unwrap_or(0)
            .max(1);
        let file = |f: &insight::OwnedFile| OwnedFile {
            path: store.paths[f.file as usize].clone(),
            size: f.size,
            lines: f.lines,
            authors: f.authors,
            owner: store.people[f.owner as usize].name.clone(),
            owner_id: f.owner,
            owner_share: f.owner_lines as f64 / f.lines.max(1) as f64,
            owner_last: f.owner_last,
            owner_inactive: f.owner_last < quiet_before,
            touches: f.touches,
            churn: f.churn,
            hotspot: (u64::from(f.touches) * f.size) as f64 / peak as f64,
        };
        let limit = limit as usize;
        let single: Vec<&insight::OwnedFile> =
            own.files.iter().filter(|f| f.single_owner()).collect();
        let mut at_risk: Vec<&insight::OwnedFile> = single
            .iter()
            .copied()
            .filter(|f| f.owner_last < quiet_before && f.size > 0)
            .collect();
        let at_risk_lines = at_risk.iter().map(|f| f.size).sum();
        let at_risk_files = at_risk.len() as u32;
        at_risk.sort_unstable_by_key(|f| (std::cmp::Reverse(f.size), f.file));
        let mut hotspots: Vec<&insight::OwnedFile> = own
            .files
            .iter()
            .filter(|f| f.touches > 0 && f.size > 0)
            .collect();
        hotspots
            .sort_unstable_by_key(|f| (std::cmp::Reverse(u64::from(f.touches) * f.size), f.file));

        Ok(OwnershipReport {
            files: own.files.len() as u32,
            lines: own.lines,
            by_lines: own.by_lines,
            bus_factor: insight::bus_factor(&own.people),
            single_owner_files: single.len() as u32,
            at_risk_files,
            at_risk_lines,
            inactive_days: INACTIVE_DAYS as u32,
            people: own
                .people
                .iter()
                .take(12)
                .map(|(id, lines)| {
                    let last = own.last_commit[*id as usize];
                    OwnerShare {
                        id: *id,
                        name: store.people[*id as usize].name.clone(),
                        lines: *lines,
                        share: *lines as f64 / own.lines.max(1) as f64,
                        last,
                        inactive: last < quiet_before,
                    }
                })
                .collect(),
            dirs: own
                .dirs
                .iter()
                .take(80)
                .map(|d| OwnedDir {
                    name: d.name.clone(),
                    lines: d.lines,
                    authors: d.authors,
                    owner: store.people[d.owner as usize].name.clone(),
                    owner_id: d.owner,
                    owner_share: d.owner_lines as f64 / d.lines.max(1) as f64,
                    owner_inactive: d.owner_last < quiet_before,
                    files: d.files,
                    single_owner_files: d.single_owner,
                    churn: d.churn,
                })
                .collect(),
            single_owner: single
                .iter()
                .filter(|f| f.touches > 0)
                .take(limit)
                .map(|f| file(f))
                .collect(),
            at_risk: at_risk.iter().take(limit).map(|f| file(f)).collect(),
            hotspots: hotspots.iter().take(limit).map(|f| file(f)).collect(),
        })
    }

    /// The files that changed in the same commits as `path` inside the query's window.
    pub fn coupled_files(&self, query: Query, path: String) -> Res<Coupling> {
        let store = self.loaded()?;
        let (_, _, window) = self.frame(&store, &query)?;
        let Some(file) = store.paths.iter().position(|p| *p == path) else {
            return Err(failed(format!("no file called {path}")));
        };
        // The file is named outright, so a path filter is not applied on top.
        let mut filter = self.filter(&store, &query)?;
        filter.path = None;
        let (commits, rows) = insight::coupled(&store, &window, &filter, file as u32);
        Ok(Coupling {
            path,
            commits,
            files: rows
                .into_iter()
                .map(|r| CoupledFile {
                    path: store.paths[r.file as usize].clone(),
                    shared: r.shared,
                    touches: r.touches,
                    share: f64::from(r.shared) / f64::from(commits.max(1)),
                })
                .collect(),
        })
    }

    /// The pairs of files that most reliably change together in the query's window.
    pub fn file_pairs(&self, query: Query, limit: u32) -> Res<Vec<FilePair>> {
        let store = self.loaded()?;
        let (_, _, window) = self.frame(&store, &query)?;
        let folder = |file: u32| store.f_dir[file as usize];
        Ok(
            insight::top_pairs(&store, &window, &self.filter(&store, &query)?, 300, 4)
                .into_iter()
                .take(limit as usize)
                .map(|p| FilePair {
                    strength: p.strength(),
                    same_folder: folder(p.a) == folder(p.b),
                    a: store.paths[p.a as usize].clone(),
                    b: store.paths[p.b as usize].clone(),
                    shared: p.shared,
                    touches_a: p.touches_a,
                    touches_b: p.touches_b,
                })
                .collect(),
        )
    }

    /// Directories whose activity changed most against the window before.
    /// None when there is no window before (the whole history, a release).
    pub fn movers(&self, query: Query, limit: u32) -> Res<Option<Movers>> {
        let (store, now, window) = self.summarized(&query)?;
        if query.release_tip.is_some() {
            return Ok(None);
        }
        let (clock, bounds, _) = self.frame(&store, &query)?;
        let Some(previous) = window.shifted(&clock, -1, bounds) else {
            return Ok(None);
        };
        let before = query::summarize_with(
            &store,
            &clock,
            &previous,
            bounds,
            &self.filter(&store, &query)?,
            &query.search,
            Buckets::fit(&clock, &previous, 1),
        );
        let by_lines = store.stage == Stage::Lines;
        let value = |d: &query::NamedRow| if by_lines { d.churn } else { d.touches };
        let mut both: HashMap<&str, (u64, u64)> = HashMap::new();
        for d in &now.dirs {
            both.entry(&d.name).or_default().0 = value(d);
        }
        for d in &before.dirs {
            both.entry(&d.name).or_default().1 = value(d);
        }
        let mut rows: Vec<Mover> = both
            .into_iter()
            .map(|(name, (now, before))| Mover {
                name: name.to_owned(),
                now,
                before,
            })
            .collect();
        let delta = |m: &Mover| m.now as i64 - m.before as i64;
        rows.sort_unstable_by(|a, b| delta(b).cmp(&delta(a)).then_with(|| a.name.cmp(&b.name)));
        let limit = limit as usize;
        let rising: Vec<Mover> = rows
            .iter()
            .filter(|m| delta(m) > 0)
            .take(limit)
            .map(|m| Mover {
                name: m.name.clone(),
                now: m.now,
                before: m.before,
            })
            .collect();
        let falling: Vec<Mover> = rows
            .into_iter()
            .rev()
            .filter(|m| delta(m) < 0)
            .take(limit)
            .collect();
        Ok(Some(Movers {
            by_lines,
            rising,
            falling,
        }))
    }

    /// Active and new contributors per bucket (the summary's buckets), who
    /// left since the window before, and how concentrated the work is.
    pub fn lifecycle(&self, query: Query) -> Res<LifecycleReport> {
        let (store, summary, window) = self.summarized(&query)?;
        let (clock, bounds, _) = self.frame(&store, &query)?;
        let previous = if query.release_tip.is_some() {
            None
        } else {
            window.shifted(&clock, -1, bounds)
        };
        let life = insight::lifecycle(
            &store,
            &window,
            previous.as_ref(),
            &self.filter(&store, &query)?,
            &summary.buckets,
        );
        Ok(LifecycleReport {
            labels: (0..summary.buckets.len())
                .map(|i| summary.buckets.label(&clock, i))
                .collect(),
            active: life.active,
            newcomers: life.newcomers,
            people: life.people,
            new_people: life.new_people,
            previous_people: life.previous_people,
            departed: life.departed,
            core: life.core,
            commits: life.commits,
            top_commits: life.top_commits,
        })
    }

    /// Tags on the history being analysed, newest first. Tags on other
    /// branches are left out: they are not part of what was read.
    pub fn releases(&self) -> Res<Vec<Release>> {
        let store = self.loaded()?;
        let tags: Vec<store::RefRow> = {
            let info = self.info.read().expect("poisoned");
            store::list_refs(&info.repo.to_thread_local())
                .into_iter()
                .filter(|r| matches!(r.kind, RefKind::Tag))
                .collect()
        };
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let index: HashMap<&[u8; 20], u32> = store
            .oid
            .iter()
            .enumerate()
            .map(|(i, oid)| (oid, i as u32))
            .collect();
        let mut releases: Vec<Release> = tags
            .into_iter()
            .filter_map(|tag| {
                let commit = *index.get(&tag.oid)?;
                Some(Release {
                    name: tag.name,
                    commit,
                    sha: hex(&tag.oid[..4]),
                    time: store.time[commit as usize],
                })
            })
            .collect();
        releases.sort_unstable_by(|a, b| b.commit.cmp(&a.commit).then_with(|| a.name.cmp(&b.name)));
        Ok(releases)
    }
}

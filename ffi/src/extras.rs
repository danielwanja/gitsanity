//! Second-pass answers for the Activity, Contributors and Files views.

use gitsanity::query;

use crate::{NamedValue, Query, Repo, Res, failed, named};

#[derive(uniffi::Record)]
pub struct ContributorSeries {
    pub id: u32,
    pub name: String,
    /// Commits per bucket, aligned with the summary's series.
    pub commits: Vec<u32>,
}

#[derive(uniffi::Record)]
pub struct FileEvents {
    pub path: String,
    pub events: u32,
    pub lines: u64,
}

#[derive(uniffi::Record)]
pub struct TypeShare {
    pub name: String,
    pub commits: u64,
}

#[derive(uniffi::Record)]
pub struct ActivityExtras {
    /// The most active contributors over time.
    pub stream: Vec<ContributorSeries>,
    pub top_added: Vec<FileEvents>,
    pub top_deleted: Vec<FileEvents>,
    pub top_renamed: Vec<FileEvents>,
    /// File types with vendored and generated files left out.
    pub file_types_core: Vec<NamedValue>,
    /// Commits attributed to the file type each one changed most.
    pub attributed_commits: Vec<TypeShare>,
}

#[derive(uniffi::Record)]
pub struct StaleFile {
    pub path: String,
    pub lines: u64,
    pub last: i64,
    pub last_author: String,
}

#[derive(uniffi::Record)]
pub struct NewFile {
    pub path: String,
    pub created: i64,
}

#[derive(uniffi::Record)]
pub struct FileHealth {
    /// Files that exist at the end of the window, by time since last change.
    pub staleness: Vec<u32>,
    pub staleness_labels: Vec<String>,
    pub alive: u32,
    /// Most-changed files untouched for 90 days or more.
    pub stale: Vec<StaleFile>,
    /// Files first added inside the window, newest first.
    pub new_files: Vec<NewFile>,
}

#[uniffi::export]
impl Repo {
    /// Contributor stream, most added / deleted / renamed files, and file
    /// types without vendored or generated code, for the same query as a summary.
    pub fn activity_extras(&self, query: Query, stream_size: u32) -> Res<ActivityExtras> {
        let (store, summary, window) = self.summarized(&query)?;
        let extras = query::extras(
            &store,
            &window,
            &Self::filter(&query),
            &summary,
            stream_size as usize,
        );
        let events = |rows: Vec<query::FileEvents>| -> Vec<FileEvents> {
            rows.into_iter()
                .map(|(file, events, lines)| FileEvents {
                    path: store.paths[file as usize].clone(),
                    events,
                    lines,
                })
                .collect()
        };
        Ok(ActivityExtras {
            stream: extras
                .stream
                .into_iter()
                .map(|(id, commits)| ContributorSeries {
                    id,
                    name: store.people[id as usize].name.clone(),
                    commits,
                })
                .collect(),
            top_added: events(extras.top_added),
            top_deleted: events(extras.top_deleted),
            top_renamed: events(extras.top_renamed),
            file_types_core: named(&extras.langs_core, 40),
            attributed_commits: extras
                .attributed
                .into_iter()
                .map(|(name, commits)| TypeShare { name, commits })
                .collect(),
        })
    }

    /// Staleness, stale hotspots and new files as of the end of the query's window.
    pub fn file_health(&self, query: Query) -> Res<FileHealth> {
        let store = self.loaded()?;
        if store.ch_start.is_empty() {
            return Err(failed("files are not known yet"));
        }
        let (_, _, window) = self.frame(&store, &query)?;
        let health = query::file_health(&store, &window);
        Ok(FileHealth {
            staleness: health.staleness.to_vec(),
            staleness_labels: query::STALENESS_BINS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            alive: health.alive,
            stale: health
                .stale
                .into_iter()
                .map(|r| StaleFile {
                    path: store.paths[r.file as usize].clone(),
                    lines: r.churn,
                    last: r.last,
                    last_author: store.people[r.last_author as usize].name.clone(),
                })
                .collect(),
            new_files: health
                .new_files
                .into_iter()
                .map(|(file, created)| NewFile {
                    path: store.paths[file as usize].clone(),
                    created,
                })
                .collect(),
        })
    }
}

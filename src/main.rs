use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod git;
mod query;
mod snapshot;
mod store;
mod time;
mod tui;

#[derive(Parser)]
#[command(name = "gitsanity", version, about = "Git history analytics for the terminal")]
struct Cli {
    /// Repository to open (default: the current directory)
    path: Option<PathBuf>,
    /// Ignore the saved index and read the history again
    #[arg(long)]
    reindex: bool,
    /// Show dates in this IANA time zone instead of the system one (e.g. Europe/Zurich)
    #[arg(long, value_name = "ZONE")]
    tz: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Index a repository and print how long each pass took
    Bench {
        path: PathBuf,
        /// Skip the line-count pass
        #[arg(long)]
        no_lines: bool,
        /// Also write and re-read the on-disk cache
        #[arg(long)]
        cache: bool,
    },
    /// Print `sha files added removed` for every non-merge commit (for checking against git)
    #[command(hide = true)]
    Dump { path: PathBuf },
    /// Render one screen without a terminal (plain text, or HTML with colours)
    Snapshot {
        path: PathBuf,
        /// overview, activity, contributors, files or commits
        #[arg(long, default_value = "overview")]
        tab: String,
        /// 1D 5D 1M 6M YTD 1Y 5Y All
        #[arg(long, default_value = "All")]
        range: String,
        #[arg(long, default_value_t = 160)]
        width: u16,
        #[arg(long, default_value_t = 45)]
        height: u16,
        /// Keys to press before the screenshot, e.g. "jjj" or "?"
        #[arg(long, default_value = "")]
        keys: String,
        /// Write an HTML rendering here instead of printing text
        #[arg(long)]
        html: Option<PathBuf>,
    },
}

fn clock(tz: Option<&str>) -> Result<time::Clock> {
    match tz {
        Some(name) => time::Clock::named(name).with_context(|| format!("unknown time zone {name}")),
        None => Ok(time::Clock::system()),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let clock = clock(cli.tz.as_deref())?;
    match cli.command {
        Some(Command::Bench { path, no_lines, cache }) => bench(&path, !no_lines, cache, &clock),
        Some(Command::Snapshot { path, tab, range, width, height, keys, html }) => {
            snapshot::run(&path, clock, &tab, &range, width, height, &keys, html.as_deref())
        }
        Some(Command::Dump { path }) => {
            use std::io::Write;
            let info = store::open(&path)?;
            let (s, _) = store::index(&info, &git::Progress::default(), true, |_| {})?;
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            for i in (0..s.len()).filter(|i| !s.is_merge(*i)) {
                writeln!(out, "{} {} {} {}", s.hex_oid(i), s.changes_of(i).len(), s.c_add[i], s.c_del[i])?;
            }
            Ok(())
        }
        None => {
            let path = cli.path.unwrap_or_else(|| PathBuf::from("."));
            tui::run(store::open(&path)?, clock, cli.reindex)
        }
    }
}

fn bench(path: &Path, lines: bool, cache: bool, clock: &time::Clock) -> Result<()> {
    let total = Instant::now();
    let info = store::open(path)?;
    let progress = git::Progress::default();
    let mut first_stage = None;
    let (s, t) = store::index(&info, &progress, lines, |_| {
        first_stage.get_or_insert(total.elapsed().as_secs_f64());
    })?;
    let (add, del): (u64, u64) =
        (s.ch_add.iter().map(|v| u64::from(*v)).sum(), s.ch_del.iter().map(|v| u64::from(*v)).sum());
    let binary = s.ch_kind.iter().filter(|k| **k & git::FLAG_BINARY != 0).count();
    let renames = s.ch_kind.iter().filter(|k| **k & 0x7f == git::KIND_REN).count();
    println!("{} @ {}", info.workdir.display(), info.ref_name);
    println!(
        "  {} commits · {} people · {} files · {} changes (+{add} -{del}, {binary} binary, {renames} renames) · {} dates repaired",
        s.len(),
        s.people.len(),
        s.paths.len(),
        s.ch_file.len(),
        s.repaired_dates
    );
    println!(
        "  walk {:.2}s · build {:.2}s · tree diff {:.2}s · merge {:.2}s · lines {:.2}s · total {:.2}s · first screen after {:.2}s",
        t.walk,
        t.build,
        t.diff,
        t.merge,
        t.lines,
        total.elapsed().as_secs_f64(),
        first_stage.unwrap_or(0.0)
    );
    // How long the dashboard takes to recompute for the widest window.
    let now = s.time.last().copied().unwrap_or(0);
    let bounds = query::bounds(&s, clock, now);
    for preset in [time::Preset::All, time::Preset::Y1, time::Preset::M1] {
        let w = time::Window::ending(clock, preset, bounds.last_day, bounds);
        let t = Instant::now();
        let sum = query::summarize(&s, clock, &w, bounds, &query::Filter::default(), "", 150);
        println!(
            "  query {:>3} ({}): {:.1} ms · {} commits · {} files · {} buckets by {}",
            preset.label(),
            w.label(),
            t.elapsed().as_secs_f64() * 1e3,
            sum.totals.commits,
            sum.totals.files,
            sum.buckets.len(),
            sum.buckets.bucket.label()
        );
    }
    if cache {
        let t = Instant::now();
        let file = store::save_cache(&info, &s)?;
        let saved = t.elapsed().as_secs_f64();
        let size = std::fs::metadata(&file)?.len();
        let t = Instant::now();
        let loaded = store::load_cache(&info).map(|s| s.len());
        println!(
            "  cache {:.1} MB · save {saved:.2}s · load {:.2}s ({loaded:?} commits) · {}",
            size as f64 / 1e6,
            t.elapsed().as_secs_f64(),
            file.display()
        );
    }
    let _ = Arc::strong_count(&s);
    Ok(())
}

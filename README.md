# gitsanity

Git history analytics for the terminal. The Rust successor to the Gitsanity
macOS app: open a repository, get commits, contributors, hot files, code
frequency and work rhythm for any time range, without waiting.

![Overview](docs/screenshots/overview.png)

```bash
cargo run --release -- ~/code/some-repo
```

## Status: 2.0 alpha

Working: Overview, Activity, Contributors, Files and Commits views; range
presets with previous / next stepping; contributor filter; commit search;
progressive indexing; on-disk cache.

Not ported from the Swift app yet: Lens (refs and commit graph), staleness
and new-file tables, storage treemap, repo hygiene, report export,
content-similarity rename detection (only exact renames are paired),
mailmap, choosing a ref other than HEAD, incremental re-index.
SHA-256 repositories are not supported.

## How it is fast

History is read straight from the object database with
[gitoxide](https://github.com/GitoxideLabs/gitoxide), on all cores, in three
passes the UI can show one at a time:

1. commits (parallel graph walk)
2. files touched per commit (parallel tree diff)
3. lines added / removed per file (parallel blob diff, Myers, like `git log --numstat`)

The result is a set of plain in-memory columns sorted by time, so a time
window is an index range and every screen is one linear scan. It is saved
compressed under the user cache directory and reloaded when HEAD is unchanged.

Measured on a 14-core Apple Silicon Mac (`gitsanity bench <repo> --cache`):

| Repository | Commits | First screen | Files known | Fully indexed | Reopen from cache |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux kernel | 1,414,234 | 2.1 s | 9.8 s | 50.9 s | 0.33 s |
| swift | 197,206 | 0.3 s | 1.6 s | 10.1 s | |
| VS Code | 145,304 | 0.2 s | 1.0 s | 4.8 s | |
| rails | 97,074 | 0.1 s | 0.4 s | 1.7 s | 0.02 s |

Dashboard recompute on the kernel: 81 ms for all 21 years, 7 ms for one year.

## Dates

Timestamps are stored as epoch seconds. Days, months, weekdays, hours and
every range label are derived in the viewer's time zone at query time
(`--tz Europe/Zurich` overrides the system zone). Windows are whole local days,
half-open, and tile exactly when stepping. An author date that is later than
its own commit, or earlier than the repository's first commit, is treated as a
broken clock and replaced by the commit date. See `src/time.rs` and its tests.

## Develop

```bash
cargo test
cargo run --release -- bench <repo> --cache
cargo run --release -- snapshot <repo> --tab activity --range 1Y            # text
cargo run --release -- snapshot <repo> --tab overview --html overview.html  # with colours
```

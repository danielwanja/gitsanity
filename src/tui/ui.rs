//! Drawing. Pure function of `App`; the only state it writes back is the list
//! of click targets and (through `App::refresh`) the cached summary.

use std::sync::atomic::Ordering;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::widgets::*;
use super::{App, Click, Mode, Tab};
use crate::git::{self, FLAG_BINARY, KIND_ADD, KIND_DEL, KIND_REN};
use crate::query::{Summary, Totals};
use crate::store::{COMMIT_TYPES, Stage, Store};
use crate::time::{self, Bucket, Preset, civil_from_days, month_name};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    app.refresh((area.width as usize).saturating_sub(4).max(10));
    match app.tab {
        Tab::Lens => {
            app.ensure_graph();
            app.load_body();
        }
        Tab::Commits => app.load_body(),
        Tab::Contributors => {
            app.load_detail((area.width as usize * 2 / 5).saturating_sub(6).max(10))
        }
        _ => {}
    }
    app.hits.clear();
    let [header, sub, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_header(f, app, header);
    draw_sub(f, app, sub);
    draw_tabs(f, app, tabs);
    draw_footer(f, app, footer);

    match (&app.store, &app.summary) {
        (Some(store), Some(summary)) => {
            let store = store.clone();
            match app.tab {
                Tab::Overview => overview(f, app, &store, summary, body),
                Tab::Activity => activity(f, app, &store, summary, body),
                Tab::Contributors => contributors(f, app, &store, summary, body),
                Tab::Files => files(f, app, &store, summary, body),
                Tab::Commits => commits(f, app, &store, summary, body),
                Tab::Lens => {}
            }
            if app.tab == Tab::Lens {
                lens(f, app, &store, body);
            }
        }
        _ => loading(f, app, body),
    }
    match app.mode {
        Mode::Help => help(f, area),
        Mode::PickAuthor => picker(f, app, area),
        _ => {}
    }
}

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(FAINT))
        .title(Line::from(format!(" {title} ")).style(Style::new().fg(DIM)))
}

fn dim() -> Style {
    Style::new().fg(DIM)
}

// ------------------------------------------------------------------ chrome

fn draw_header(f: &mut Frame, app: &mut App, area: Rect) {
    let left = Line::from(vec![
        Span::styled(" ◆ ", Style::new().fg(VIOLET)),
        Span::styled("gitsanity", Style::new().fg(VIOLET).bold()),
        Span::raw("   "),
        Span::styled(app.repo_name.clone(), Style::new().bold()),
        Span::styled(format!("  ⎇ {}", app.ref_name), Style::new().fg(CYAN)),
    ]);
    f.render_widget(left, area);

    // Range chips, right-aligned and clickable.
    let current = app.window.map(|w| w.preset);
    let total: usize = Preset::ALL.iter().map(|p| p.label().len() + 3).sum();
    let mut x = area.right().saturating_sub(total as u16 + 1);
    for p in Preset::ALL {
        let text = format!(" {} ", p.label());
        let w = text.len() as u16;
        let style = if current == Some(p) {
            Style::new().fg(Color::Black).bg(VIOLET).bold()
        } else {
            Style::new().fg(DIM)
        };
        if x + w <= area.right() {
            f.buffer_mut().set_string(x, area.y, &text, style);
            app.hits
                .push((Rect::new(x, area.y, w, 1), Click::Preset(p)));
        }
        x += w + 1;
    }
}

fn draw_sub(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(w) = app.window else { return };
    let movable = |dir| w.shifted(&app.clock, dir, app.bounds).is_some();
    let arrow = |on: bool| {
        if on {
            Style::new().fg(VIOLET).bold()
        } else {
            Style::new().fg(FAINT)
        }
    };
    let label = w.label();
    let bucket = app
        .summary
        .as_ref()
        .map(|s| s.buckets.bucket.label())
        .unwrap_or("day");
    let days = w.days();
    let mut spans = vec![
        Span::styled(" ‹ ", arrow(movable(-1))),
        Span::styled(label.clone(), Style::new().bold()),
        Span::styled(" › ", arrow(movable(1))),
        Span::styled(
            format!(
                "  {} day{} · by {bucket} · times in {}",
                fmt_n(days as u64),
                if days == 1 { "" } else { "s" },
                app.clock.name
            ),
            dim(),
        ),
    ];
    app.hits
        .push((Rect::new(area.x, area.y, 3, 1), Click::Shift(-1)));
    app.hits.push((
        Rect::new(area.x + 3 + label.width() as u16, area.y, 3, 1),
        Click::Shift(1),
    ));
    if let (Some(author), Some(store)) = (app.filter.author, &app.store) {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!(" ● {} ", store.people[author as usize].name),
            Style::new().fg(Color::Black).bg(CYAN),
        ));
        spans.push(Span::styled(" x clears", dim()));
    }
    if let Some(path) = &app.filter.path {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!(" ▸ {} ", fit_tail(path, 48)),
            Style::new().fg(Color::Black).bg(GREEN),
        ));
        spans.push(Span::styled(" u widens", dim()));
    }
    if !app.search.is_empty() && app.mode != Mode::Search {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!(" /{} ", app.search),
            Style::new().fg(Color::Black).bg(AMBER),
        ));
    }
    f.render_widget(Line::from(spans), area);
}

fn draw_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    let mut x = area.x + 1;
    let y = area.y + 1;
    for (i, tab) in Tab::ALL.into_iter().enumerate() {
        let text = format!(" {} {} ", i + 1, tab.label());
        let w = text.width() as u16;
        let buf = f.buffer_mut();
        if tab == app.tab {
            buf.set_string(
                x,
                y,
                &text,
                Style::new()
                    .fg(VIOLET)
                    .bold()
                    .add_modifier(Modifier::UNDERLINED),
            );
        } else {
            buf.set_string(x, y, format!(" {} ", i + 1), Style::new().fg(FAINT));
            buf.set_string(x + 3, y, format!("{} ", tab.label()), dim());
        }
        app.hits.push((Rect::new(x, y, w, 1), Click::Tab(tab)));
        x += w + 1;
    }
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hints = match app.mode {
        Mode::Search => " type to search · Enter keep · Esc clear",
        Mode::PickAuthor => " type to filter · ↑↓ choose · Enter select · Esc cancel",
        Mode::RefSearch => " type to filter refs · Enter keep · Esc clear",
        _ => match app.tab {
            Tab::Contributors => {
                " ←→ period · -/+ range · ↑↓ select · Enter filter · a contributor · ? help · q quit"
            }
            Tab::Commits => {
                " ←→ period · -/+ range · ↑↓ select · / search · a contributor · ? help · q quit"
            }
            Tab::Files => {
                " ↑↓ select · Enter filter by file · u up to its folder · x clear · ←→ period · ? help · q quit"
            }
            Tab::Lens => {
                " p pane · ↑↓ select · Enter switch to ref · r kind · S stale · / filter refs · ? help · q quit"
            }
            _ => " ←→ period · -/+ range · 1-6 tabs · / search · a contributor · ? help · q quit",
        },
    };
    let p = &app.progress;
    let spin = SPINNER[(app.started.elapsed().as_millis() / 80) as usize % SPINNER.len()];
    let pct = |done: u64, total: u64| (done * 100).checked_div(total).unwrap_or(0).min(100);
    let meter = |pct: u64| {
        let filled = (pct as usize * 12 / 100).min(12);
        format!("{}{}", "━".repeat(filled), "─".repeat(12 - filled))
    };
    let status: Line = if let Some(e) = &app.error {
        Line::from(Span::styled(format!("✗ {e} "), Style::new().fg(RED)))
    } else {
        match p.phase.load(Ordering::Relaxed) {
            git::PHASE_WALK => Line::from(vec![
                Span::styled(format!("{spin} reading commits "), Style::new().fg(AMBER)),
                Span::styled(
                    format!("{} ", fmt_n(p.commits.load(Ordering::Relaxed))),
                    Style::new().bold(),
                ),
            ]),
            git::PHASE_DIFF => {
                let pc = pct(
                    p.diffed.load(Ordering::Relaxed),
                    p.diff_total.load(Ordering::Relaxed),
                );
                Line::from(vec![
                    Span::styled(
                        format!("{spin} finding changed files "),
                        Style::new().fg(AMBER),
                    ),
                    Span::styled(meter(pc), Style::new().fg(VIOLET)),
                    Span::styled(format!(" {pc:>3}% "), Style::new().bold()),
                ])
            }
            git::PHASE_LINES => {
                let pc = pct(
                    p.lined.load(Ordering::Relaxed),
                    p.line_total.load(Ordering::Relaxed),
                );
                Line::from(vec![
                    Span::styled(format!("{spin} counting lines "), Style::new().fg(AMBER)),
                    Span::styled(meter(pc), Style::new().fg(VIOLET)),
                    Span::styled(format!(" {pc:>3}% "), Style::new().bold()),
                ])
            }
            _ => {
                let commits = app.store.as_ref().map_or(0, |s| s.len());
                let ready = app.ready_after.map(|d| d.as_secs_f64()).unwrap_or(0.0);
                Line::from(vec![
                    Span::styled("● ", Style::new().fg(GREEN)),
                    Span::styled(
                        format!(
                            "{} commits · ready in {ready:.1}s{} · query {:.0}ms ",
                            fmt_n(commits as u64),
                            if app.from_cache { " from cache" } else { "" },
                            app.summary_ms
                        ),
                        dim(),
                    ),
                ])
            }
        }
    };
    let room = (area.width as usize).saturating_sub(status.width() + 2);
    f.render_widget(Line::from(fit(hints, room)).style(dim()), area);
    f.render_widget(status.alignment(Alignment::Right), area);
}

fn loading(f: &mut Frame, app: &App, area: Rect) {
    let n = app.progress.commits.load(Ordering::Relaxed);
    let spin = SPINNER[(app.started.elapsed().as_millis() / 80) as usize % SPINNER.len()];
    let lines = vec![
        Line::from(Span::styled(
            format!("{spin} Reading history"),
            Style::new().fg(VIOLET).bold(),
        )),
        Line::from(""),
        Line::from(Span::styled(format!("{} commits so far", fmt_n(n)), dim())),
        Line::from(Span::styled(
            app.repo_path.display().to_string(),
            Style::new().fg(FAINT),
        )),
    ];
    let [mid] = Layout::vertical([Constraint::Length(4)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), mid);
}

// ---------------------------------------------------------------- overview

fn delta_line(now: u64, previous: Option<u64>, width: usize) -> Line<'static> {
    match previous {
        None => Line::from(Span::styled("whole history", Style::new().fg(FAINT))),
        Some(0) if now == 0 => Line::from(Span::styled("— no change", dim())),
        Some(0) => Line::from(Span::styled("▲ new this period", Style::new().fg(GREEN))),
        Some(prev) => {
            let pct = (now as f64 - prev as f64) / prev as f64 * 100.0;
            let (arrow, color) = if pct >= 0.0 {
                ("▲", GREEN)
            } else {
                ("▼", RED)
            };
            let change = if pct >= 1000.0 {
                format!("{:.1}×", now as f64 / prev as f64)
            } else {
                format!("{:.1}%", pct.abs())
            };
            Line::from(vec![
                Span::styled(format!("{arrow} {change}"), Style::new().fg(color)),
                Span::styled(
                    if width >= 22 {
                        " vs previous period"
                    } else {
                        " vs prev"
                    },
                    dim(),
                ),
            ])
        }
    }
}

fn gradient_spark(values: &[u64], width: usize) -> Line<'static> {
    let s = sparkline(values, width);
    let n = s.chars().count().max(1);
    Line::from(
        s.chars()
            .enumerate()
            .map(|(i, c)| {
                Span::styled(
                    c.to_string(),
                    Style::new().fg(gradient(i as f64 / n as f64)),
                )
            })
            .collect::<Vec<_>>(),
    )
}

fn lines_pending(app: &App, store: &Store) -> Option<String> {
    (store.stage != Stage::Lines).then(|| {
        let p = &app.progress;
        let total = p.line_total.load(Ordering::Relaxed);
        if store.stage == Stage::Files && total > 0 {
            format!(
                "counting… {}%",
                p.lined.load(Ordering::Relaxed) * 100 / total
            )
        } else {
            "counting…".to_owned()
        }
    })
}

fn overview(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    // The chart takes under half of what is left below the cards, capped, so a
    // tall terminal gives its extra rows to the tables instead of stretching bars.
    let below = area.height.saturating_sub(5);
    let chart_h = (below * 45 / 100)
        .min(22)
        .min(below.saturating_sub(10))
        .max(6);
    let [kpis, chart, bottom] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(chart_h),
        Constraint::Min(0),
    ])
    .areas(area);

    let cards: [Rect; 5] = Layout::horizontal([Constraint::Fill(1); 5]).areas(kpis);
    let prev = s.previous;
    let pending = lines_pending(app, store);
    let files_pending = store.stage == Stage::Commits;
    let churn: Vec<u64> = s
        .b_added
        .iter()
        .zip(&s.b_removed)
        .map(|(a, r)| a + r)
        .collect();
    let inner_w = cards[0].width.saturating_sub(4) as usize;
    let big = |text: String| Line::from(Span::styled(text, Style::new().bold()));
    let card = |f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>| {
        f.render_widget(
            Paragraph::new(lines)
                .block(panel(title).padding(ratatui::widgets::Padding::horizontal(1))),
            area,
        );
    };
    let prev_of = |get: fn(&Totals) -> u64| prev.as_ref().map(get);
    card(
        f,
        cards[0],
        "Commits",
        vec![
            big(fmt_n(s.totals.commits)),
            delta_line(s.totals.commits, prev_of(|t| t.commits), inner_w),
            gradient_spark(&s.b_commits, inner_w),
        ],
    );
    card(
        f,
        cards[1],
        "Contributors",
        vec![
            big(fmt_n(s.totals.authors)),
            delta_line(s.totals.authors, prev_of(|t| t.authors), inner_w),
            Line::from(Span::styled(
                s.authors
                    .first()
                    .map(|a| {
                        format!(
                            "top: {}",
                            fit(&store.people[a.id as usize].name, inner_w.saturating_sub(5))
                        )
                    })
                    .unwrap_or_default(),
                dim(),
            )),
        ],
    );
    card(
        f,
        cards[2],
        "Files touched",
        if files_pending {
            vec![Line::from(Span::styled("scanning…", dim()))]
        } else {
            vec![
                big(fmt_n(s.totals.files)),
                delta_line(s.totals.files, prev_of(|t| t.files), inner_w),
                Line::from(Span::styled(
                    format!("{} changes", compact(s.kinds.iter().map(|k| k.1).sum())),
                    dim(),
                )),
            ]
        },
    );
    card(
        f,
        cards[3],
        "Lines changed",
        match &pending {
            Some(p) => vec![Line::from(Span::styled(p.clone(), dim()))],
            None => vec![
                Line::from(vec![
                    Span::styled(
                        format!("+{}", compact(s.totals.added)),
                        Style::new().fg(GREEN).bold(),
                    ),
                    Span::styled(
                        format!("  −{}", compact(s.totals.removed)),
                        Style::new().fg(RED).bold(),
                    ),
                ]),
                delta_line(s.totals.churn(), prev_of(Totals::churn), inner_w),
                gradient_spark(&churn, inner_w),
            ],
        },
    );
    card(
        f,
        cards[4],
        "Busiest day",
        match (&pending, s.biggest_day) {
            (Some(p), _) => vec![Line::from(Span::styled(p.clone(), dim()))],
            (None, None) => vec![Line::from(Span::styled("—", dim()))],
            (None, Some((day, lines))) => {
                let (y, m, d) = civil_from_days(day);
                vec![
                    big(format!("{} {d}, {y}", month_name(m))),
                    Line::from(Span::styled(
                        format!("{} lines changed", fmt_n(lines)),
                        dim(),
                    )),
                    Line::from(Span::styled(
                        format!("{} merges in range", fmt_n(s.merges)),
                        Style::new().fg(FAINT),
                    )),
                ]
            }
        },
    );

    timeline(f, app, s, chart);

    let weeks = calendar_weeks(app, (area.width as usize * 2 / 5).saturating_sub(8) / 2);
    let cal_w = ((weeks * 2 + 8) as u16).max(46).min(area.width / 2);
    let [left, hot, people] = Layout::horizontal([
        Constraint::Length(cal_w),
        Constraint::Fill(1),
        Constraint::Fill(1),
    ])
    .areas(bottom);
    if left.height >= 18 {
        let [cal, dirs] =
            Layout::vertical([Constraint::Length(11), Constraint::Min(0)]).areas(left);
        calendar(f, app, s, cal, weeks);
        directories(f, store, s, dirs);
    } else {
        calendar(f, app, s, left, weeks);
    }

    let by_lines = store.stage == Stage::Lines;
    let block = panel(if by_lines {
        "Hot files · lines changed"
    } else {
        "Hot files · times touched"
    });
    let inner = block.inner(hot);
    f.render_widget(block, hot);
    let value = |r: &crate::query::FileRow| {
        if by_lines {
            r.added + r.removed
        } else {
            u64::from(r.touches)
        }
    };
    let max = s.files.first().map(value).unwrap_or(0);
    for (row, file) in s.files.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + row as u16;
        let v = value(file);
        let buf = f.buffer_mut();
        buf.set_string(
            inner.x + 1,
            y,
            pad_left(&compact(v), 6),
            Style::new().bold(),
        );
        buf.set_string(
            inner.x + 8,
            y,
            hbar(v, max, 8),
            Style::new().fg(gradient(row as f64 / 10.0)),
        );
        let w = (inner.width as usize).saturating_sub(18);
        buf.set_string(
            inner.x + 17,
            y,
            fit_tail(&store.paths[file.id as usize], w),
            Style::new(),
        );
    }

    let block = panel("Top contributors · commits");
    let inner = block.inner(people);
    f.render_widget(block, people);
    let max = s.authors.first().map(|a| u64::from(a.commits)).unwrap_or(0);
    for (row, a) in s.authors.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + row as u16;
        let buf = f.buffer_mut();
        buf.set_string(
            inner.x + 1,
            y,
            pad_left(&compact(u64::from(a.commits)), 6),
            Style::new().bold(),
        );
        buf.set_string(
            inner.x + 8,
            y,
            hbar(u64::from(a.commits), max, 8),
            Style::new().fg(gradient(row as f64 / 10.0)),
        );
        let w = (inner.width as usize).saturating_sub(18);
        buf.set_string(
            inner.x + 17,
            y,
            fit(&store.people[a.id as usize].name, w),
            Style::new(),
        );
    }
}

/// Axis labels under a bar chart: first, last, and as many evenly spaced ones as fit.
fn axis_labels(buf: &mut Buffer, app: &App, s: &Summary, area: Rect) {
    let n = s.buckets.len();
    if n == 0 || area.width < 8 {
        return;
    }
    let same_year = app
        .window
        .is_some_and(|w| civil_from_days(w.first_day).0 == civil_from_days(w.last_day).0);
    let mut next_free = area.x;
    let slots = (area.width as usize / 16).max(2);
    let every = n.div_ceil(slots).max(1);
    for i in (0..n).step_by(every) {
        let mut label = s.buckets.label(&app.clock, i);
        if s.buckets.bucket == Bucket::Hour && app.window.is_some_and(|w| w.days() == 1) {
            label = label.rsplit(' ').next().unwrap_or_default().to_owned();
        }
        if same_year && matches!(s.buckets.bucket, Bucket::Day | Bucket::Week) {
            label.truncate(label.rfind(',').unwrap_or(label.len()));
        }
        let x = area.x + bar_span(i, n, area.width as usize).0 as u16;
        if x >= next_free && x + label.width() as u16 <= area.right() {
            buf.set_string(x, area.y, &label, dim());
            next_free = x + label.width() as u16 + 2;
        }
    }
}

fn timeline(f: &mut Frame, app: &App, s: &Summary, area: Rect) {
    let peak = s.b_commits.iter().copied().max().unwrap_or(0);
    let block = panel(&format!("Commits per {}", s.buckets.bucket.label())).title(
        Line::from(format!(" peak {} ", fmt_n(peak)))
            .style(dim())
            .right_aligned(),
    );
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let bars = Rect {
        height: inner.height - 1,
        ..inner
    };
    let n = s.b_commits.len().max(1);
    bars_up(f.buffer_mut(), bars, &s.b_commits, peak, |i, _| {
        gradient(i as f64 / n as f64)
    });
    axis_labels(
        f.buffer_mut(),
        app,
        s,
        Rect {
            y: inner.bottom() - 1,
            height: 1,
            ..inner
        },
    );
}

/// How many week columns the day grid shows: the window's own weeks, up to `max`.
fn calendar_weeks(app: &App, max: usize) -> usize {
    let Some(w) = app.window else { return 1 };
    (((w.last_day - w.first_day) / 7 + 2) as usize)
        .min(max.clamp(8, 53))
        .max(1)
}

fn directories(f: &mut Frame, store: &Store, s: &Summary, area: Rect) {
    let by_lines = store.stage == Stage::Lines;
    let rows: Vec<_> = s
        .dirs
        .iter()
        .take(60)
        .enumerate()
        .map(|(i, d)| {
            (
                d.name.clone(),
                if by_lines { d.churn } else { d.touches },
                gradient(i as f64 / 12.0),
            )
        })
        .collect();
    bar_table(
        f,
        area,
        if by_lines {
            "Directories · lines changed"
        } else {
            "Directories · times touched"
        },
        &rows,
    );
}

fn calendar(f: &mut Frame, app: &App, s: &Summary, area: Rect, weeks: usize) {
    let Some(w) = app.window else { return };
    // Columns are Monday-first weeks ending with the week of the window's last day.
    let last_monday = w.last_day - (w.last_day + 3).rem_euclid(7);
    let weeks = weeks
        .min(((w.last_day - w.first_day) / 7 + 2) as usize)
        .max(1);
    let first_monday = last_monday - 7 * (weeks as i64 - 1);
    let (ly, lm, ld) = civil_from_days(w.last_day);
    let block = panel(&format!(
        "Commits per day · {weeks} weeks to {} {ld}, {ly}",
        month_name(lm)
    ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 8 {
        return;
    }
    let value = |day: i64| -> Option<u64> {
        (day >= w.first_day && day <= w.last_day)
            .then(|| u64::from(s.daily[(day - w.first_day) as usize]))
    };
    let max = (first_monday..=w.last_day)
        .filter_map(value)
        .max()
        .unwrap_or(0);
    let buf = f.buffer_mut();
    let x0 = inner.x + 5;
    let mut last_month = 0;
    let mut label_free = x0;
    for week in 0..weeks {
        let monday = first_monday + 7 * week as i64;
        let x = x0 + (week * 2) as u16;
        if x + 1 >= inner.right() {
            break;
        }
        let (_, m, _) = civil_from_days(monday + 6);
        if m != last_month && x >= label_free && x + 3 <= inner.right() {
            buf.set_string(x, inner.y, month_name(m), dim());
            label_free = x + 4;
        }
        last_month = m;
        for dow in 0..7 {
            if let Some(v) = value(monday + dow) {
                buf.set_string(
                    x,
                    inner.y + 1 + dow as u16,
                    "■",
                    Style::new().fg(heat(heat_level(v, max))),
                );
            }
        }
    }
    for (dow, name) in ["Mon", "", "Wed", "", "Fri", "", "Sun"].iter().enumerate() {
        buf.set_string(inner.x + 1, inner.y + 1 + dow as u16, name, dim());
    }
    if inner.height >= 9 {
        let y = inner.y + 8;
        buf.set_string(x0, y, "less ", dim());
        for level in 0..5 {
            buf.set_string(
                x0 + 5 + level * 2,
                y,
                "■",
                Style::new().fg(heat(level as usize)),
            );
        }
        buf.set_string(x0 + 16, y, format!("more · busiest {}", fmt_n(max)), dim());
    }
}

// ---------------------------------------------------------------- activity

fn bar_table(f: &mut Frame, area: Rect, title: &str, rows: &[(String, u64, Color)]) {
    let block = panel(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let total: u64 = rows.iter().map(|r| r.1).sum();
    let max = rows.iter().map(|r| r.1).max().unwrap_or(0);
    let name_w = rows
        .iter()
        .map(|r| r.0.width())
        .max()
        .unwrap_or(0)
        .min((inner.width as usize / 2).max(8));
    let bar_w = (inner.width as usize).saturating_sub(name_w + 20);
    for (row, (name, value, color)) in rows.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + row as u16;
        let buf = f.buffer_mut();
        buf.set_string(inner.x + 1, y, pad(name, name_w), Style::new());
        buf.set_string(
            inner.x + 2 + name_w as u16,
            y,
            hbar(*value, max, bar_w),
            Style::new().fg(*color),
        );
        let share = if total == 0 {
            0.0
        } else {
            *value as f64 / total as f64 * 100.0
        };
        let tail = format!("{:>8} {share:>5.1}%", compact(*value));
        buf.set_string(
            inner.right().saturating_sub(tail.len() as u16 + 1),
            y,
            tail,
            dim(),
        );
    }
}

fn activity(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    // A tall terminal gets a third band (largest commits) rather than a taller chart.
    let tall = area.height >= 40;
    let [chart, mid, bottom] = if tall {
        let chart_h = (area.height * 40 / 100).min(24);
        Layout::vertical([
            Constraint::Length(chart_h),
            Constraint::Min(6),
            Constraint::Length(11),
        ])
        .areas(area)
    } else {
        Layout::vertical([
            Constraint::Min(8),
            Constraint::Length(0),
            Constraint::Length(11),
        ])
        .areas(area)
    };
    let by_lines = store.stage == Stage::Lines;
    let (max_a, max_r) = (
        s.b_added.iter().copied().max().unwrap_or(0),
        s.b_removed.iter().copied().max().unwrap_or(0),
    );
    let title = if by_lines {
        format!(
            "Code frequency · lines added ▲ and removed ▼ per {}",
            s.buckets.bucket.label()
        )
    } else {
        format!(
            "Code frequency per {} · {}",
            s.buckets.bucket.label(),
            lines_pending(app, store).unwrap_or_default()
        )
    };
    let block = panel(&title).title(
        Line::from(vec![
            Span::styled(
                format!(" +{}", compact(s.totals.added)),
                Style::new().fg(GREEN),
            ),
            Span::styled(
                format!(" −{} ", compact(s.totals.removed)),
                Style::new().fg(RED),
            ),
        ])
        .right_aligned(),
    );
    let inner = block.inner(chart).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, chart);
    if inner.height >= 4 {
        // Split the height in proportion to the two peaks, then use one scale
        // (lines per row) for both halves so they are directly comparable.
        let plot_h = inner.height - 1;
        let up_h = ((u64::from(plot_h) * max_a.max(1)).div_ceil((max_a + max_r).max(1)) as u16)
            .clamp(1, plot_h - 1);
        let down_h = plot_h - up_h;
        let per_row = (max_a as f64 / f64::from(up_h))
            .max(max_r as f64 / f64::from(down_h))
            .max(1.0);
        let up = Rect {
            height: up_h,
            ..inner
        };
        let down = Rect {
            y: inner.y + up_h,
            height: down_h,
            ..inner
        };
        bars_up(
            f.buffer_mut(),
            up,
            &s.b_added,
            (per_row * f64::from(up_h)).ceil() as u64,
            |_, _| GREEN,
        );
        bars_down(
            f.buffer_mut(),
            down,
            &s.b_removed,
            (per_row * f64::from(down_h)).ceil() as u64,
            RED,
        );
        axis_labels(
            f.buffer_mut(),
            app,
            s,
            Rect {
                y: inner.bottom() - 1,
                height: 1,
                ..inner
            },
        );
    }

    if tall {
        let block = panel(if by_lines {
            "Largest commits · lines changed"
        } else {
            "Largest commits · counting lines…"
        });
        let inner = block.inner(mid);
        f.render_widget(block, mid);
        for (row, ci) in s.largest.iter().take(inner.height as usize).enumerate() {
            commit_row(
                f.buffer_mut(),
                app,
                store,
                *ci as usize,
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                false,
            );
        }
    }

    let [types, kinds, rhythm] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(58),
    ])
    .areas(bottom);
    let palette = [VIOLET, CYAN, GREEN, AMBER, PINK, RED];
    let rows: Vec<_> = s
        .ctypes
        .iter()
        .enumerate()
        .map(|(i, (n, c, _))| ((*n).to_owned(), *c, palette[i % palette.len()]))
        .collect();
    bar_table(f, types, "Commit types · commits", &rows);
    let colors = [GREEN, CYAN, RED, AMBER, DIM];
    let rows: Vec<_> = s
        .kinds
        .iter()
        .zip(colors)
        .filter(|(k, _)| k.1 > 0)
        .map(|((n, e, _), c)| ((*n).to_owned(), *e, c))
        .collect();
    bar_table(f, kinds, "File changes · events", &rows);
    rhythm_panel(f, s, rhythm);
}

fn rhythm_panel(f: &mut Frame, s: &Summary, area: Rect) {
    let block = panel("Work rhythm · commits by weekday × hour, your time");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let max = s.rhythm.iter().flatten().copied().max().unwrap_or(0);
    let buf = f.buffer_mut();
    for hour in (0..24).step_by(3) {
        buf.set_string(inner.x + 5 + hour * 2, inner.y, format!("{hour:02}"), dim());
    }
    for (d, name) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        .iter()
        .enumerate()
    {
        let y = inner.y + 1 + d as u16;
        if y >= inner.bottom() {
            break;
        }
        buf.set_string(inner.x + 1, y, name, dim());
        for h in 0..24u16 {
            let level = heat_level(u64::from(s.rhythm[d][h as usize]), u64::from(max));
            buf.set_string(inner.x + 5 + h * 2, y, "■", Style::new().fg(heat(level)));
        }
    }
}

/// `+12` in green / `−3` in red, dimmed when zero.
fn signed(sign: char, value: u64, color: Color) -> Style {
    let _ = sign;
    if value == 0 {
        Style::new().fg(FAINT)
    } else {
        Style::new().fg(color)
    }
}

/// One commit on one line: sha, local date, author, subject, +added −removed.
fn commit_row(buf: &mut Buffer, app: &App, store: &Store, i: usize, area: Rect, selected: bool) {
    let style = row_style(selected);
    buf.set_style(area, style);
    let with_stats = store.stage == Stage::Lines && !store.is_merge(i);
    let (a, r) = (u64::from(store.c_add[i]), u64::from(store.c_del[i]));
    let stats_w = if with_stats {
        compact(a).len() + compact(r).len() + 4
    } else {
        0
    };
    let mut x = area.x + 1;
    let mut put = |text: String, st: Style| {
        buf.set_string(x, area.y, &text, st.patch(style));
        x += text.width() as u16;
    };
    put(format!("{} ", store.short_oid(i)), Style::new().fg(AMBER));
    put(format!("{} ", app.clock.datetime(store.time[i])), dim());
    put(
        format!("{} ", pad(&store.people[store.author[i] as usize].name, 16)),
        Style::new().fg(CYAN),
    );
    let subject_w = (area.width as usize).saturating_sub(46 + stats_w);
    put(
        pad(store.subject(i), subject_w),
        if store.is_merge(i) {
            dim()
        } else {
            Style::new()
        },
    );
    if with_stats {
        put(format!(" +{}", compact(a)), signed('+', a, GREEN));
        put(format!(" −{}", compact(r)), signed('-', r, RED));
    }
}

// ------------------------------------------------------------------ tables

/// First visible row so the selection stays roughly centred.
fn scroll(sel: usize, rows: usize, height: usize) -> usize {
    sel.saturating_sub(height / 2)
        .min(rows.saturating_sub(height))
}

fn row_style(selected: bool) -> Style {
    if selected {
        Style::new().bg(SELECT_BG)
    } else {
        Style::new()
    }
}

fn contributors(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    let detail = app.detail.as_ref().filter(|_| area.width >= 150);
    let [left, right] = match detail {
        Some(_) => Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(area),
        None => [area, Rect::default()],
    };
    let block = panel(&format!(
        "Contributors · {} active in range",
        fmt_n(s.authors.len() as u64)
    ))
    .title(
        Line::from(" Enter: filter every view ")
            .style(Style::new().fg(FAINT))
            .right_aligned(),
    );
    let inner = block.inner(left);
    f.render_widget(block, left);
    if inner.height < 2 {
        return;
    }
    // Under ~116 columns the share bar shrinks and the "Active" column goes.
    let roomy = inner.width >= 116;
    let bar_w = if roomy { 18 } else { 10 };
    let fixed = if roomy { 96 } else { 67 };
    let name_w = (inner.width as usize).saturating_sub(fixed).clamp(12, 40);
    let header = format!(
        " {:>5}  {}  {:>9}  {} {:>10} {:>10}  {}",
        "#",
        pad("Name", name_w),
        "Commits",
        pad("Share", bar_w),
        "Added",
        "Removed",
        if roomy {
            "First       Last        Active"
        } else {
            "Last"
        }
    );
    f.buffer_mut()
        .set_string(inner.x, inner.y, fit(&header, inner.width as usize), dim());
    let height = inner.height as usize - 1;
    let sel = app.sel[Tab::Contributors as usize];
    let top = scroll(sel, s.authors.len(), height);
    let max = s.authors.first().map(|a| u64::from(a.commits)).unwrap_or(0);
    for (row, a) in s.authors.iter().enumerate().skip(top).take(height) {
        let y = inner.y + 1 + (row - top) as u16;
        let p = &store.people[a.id as usize];
        let style = row_style(row == sel);
        let buf = f.buffer_mut();
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), style);
        let filtered = app.filter.author == Some(a.id);
        let mut x = inner.x;
        let mut put = |text: String, st: Style| {
            buf.set_string(x, y, &text, st.patch(style));
            x += text.width() as u16;
        };
        put(format!(" {:>5}  ", row + 1), dim());
        put(
            pad(
                &format!("{}{}", if filtered { "● " } else { "" }, p.name),
                name_w,
            ),
            if filtered {
                Style::new().fg(CYAN).bold()
            } else {
                Style::new().bold()
            },
        );
        put(
            format!("  {:>9}  ", fmt_n(u64::from(a.commits))),
            Style::new(),
        );
        put(
            pad(&hbar(u64::from(a.commits), max, bar_w), bar_w),
            Style::new().fg(gradient((row - top) as f64 / height.max(1) as f64)),
        );
        put(
            format!(" {:>10}", format!("+{}", compact(a.added))),
            signed('+', a.added, GREEN),
        );
        put(
            format!(" {:>10}", format!("−{}", compact(a.removed))),
            signed('-', a.removed, RED),
        );
        // The narrow layout keeps only the last date; first and last are in the side panel.
        if roomy {
            put(
                format!(
                    "  {}  {}  {}",
                    app.clock.date(a.first),
                    app.clock.date(a.last),
                    time::ago(app.now, a.last)
                ),
                dim(),
            );
        } else {
            put(format!("  {}", app.clock.date(a.last)), dim());
        }
    }
    if let Some((id, d)) = detail {
        contributor_detail(f, app, store, s, d, *id, right);
    }
}

/// Right-hand column of the Contributors view: the selected person's own numbers.
fn contributor_detail(
    f: &mut Frame,
    app: &App,
    store: &Store,
    all: &Summary,
    d: &Summary,
    id: u32,
    area: Rect,
) {
    let [head, chart, files, dirs, rhythm] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Fill(3),
        Constraint::Fill(2),
        Constraint::Length(if area.height >= 50 { 10 } else { 0 }),
    ])
    .areas(area);
    let p = &store.people[id as usize];
    let share = if all.totals.commits == 0 {
        0.0
    } else {
        d.totals.commits as f64 / all.totals.commits as f64 * 100.0
    };
    let active_days = d.daily.iter().filter(|c| **c > 0).count();
    let (first, last) = d
        .authors
        .first()
        .map(|a| (a.first, a.last))
        .unwrap_or((0, 0));
    let lines = vec![
        Line::from(vec![
            Span::styled(p.name.clone(), Style::new().fg(CYAN).bold()),
            Span::styled(
                format!("  {}{}", p.email, if p.is_bot { "  · bot" } else { "" }),
                dim(),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(fmt_n(d.totals.commits), Style::new().bold()),
            Span::styled(format!(" commits · {share:.1}% of the range · "), dim()),
            Span::styled(fmt_n(active_days as u64), Style::new().bold()),
            Span::styled(
                format!(" active day{} · ", if active_days == 1 { "" } else { "s" }),
                dim(),
            ),
            Span::styled(fmt_n(d.totals.files), Style::new().bold()),
            Span::styled(" files", dim()),
        ]),
        Line::from(vec![
            Span::styled(
                format!("+{}", fmt_n(d.totals.added)),
                signed('+', d.totals.added, GREEN),
            ),
            Span::styled(
                format!(" −{}", fmt_n(d.totals.removed)),
                signed('-', d.totals.removed, RED),
            ),
            Span::styled(
                format!(
                    "   first {} · last {}",
                    app.clock.date(first),
                    app.clock.date(last)
                ),
                dim(),
            ),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines)
            .block(panel("Contributor").padding(ratatui::widgets::Padding::horizontal(1))),
        head,
    );

    let peak = d.b_commits.iter().copied().max().unwrap_or(0);
    let block = panel(&format!("Their commits per {}", d.buckets.bucket.label())).title(
        Line::from(format!(" peak {} ", fmt_n(peak)))
            .style(dim())
            .right_aligned(),
    );
    let inner = block.inner(chart).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, chart);
    if inner.height >= 2 {
        let n = d.b_commits.len().max(1);
        bars_up(
            f.buffer_mut(),
            Rect {
                height: inner.height - 1,
                ..inner
            },
            &d.b_commits,
            peak,
            |i, _| gradient(i as f64 / n as f64),
        );
        axis_labels(
            f.buffer_mut(),
            app,
            d,
            Rect {
                y: inner.bottom() - 1,
                height: 1,
                ..inner
            },
        );
    }

    let by_lines = store.stage == Stage::Lines;
    let path_w = (files.width as usize / 2).saturating_sub(2);
    let rows: Vec<_> = d
        .files
        .iter()
        .take(60)
        .enumerate()
        .map(|(i, r)| {
            let v = if by_lines {
                r.added + r.removed
            } else {
                u64::from(r.touches)
            };
            (
                fit_tail(&store.paths[r.id as usize], path_w),
                v,
                gradient(i as f64 / 12.0),
            )
        })
        .collect();
    bar_table(
        f,
        files,
        if by_lines {
            "Their files · lines changed"
        } else {
            "Their files · times touched"
        },
        &rows,
    );
    directories(f, store, d, dirs);
    if rhythm.height > 0 {
        rhythm_panel(f, d, rhythm);
    }
}

fn files(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(area);
    let by_lines = store.stage == Stage::Lines;
    let block = panel(&format!(
        "Hot files · {} touched in range{}",
        fmt_n(s.totals.files),
        if s.files.len() as u64 == s.totals.files {
            String::new()
        } else {
            format!(" · top {}", fmt_n(s.files.len() as u64))
        }
    ));
    let inner = block.inner(left);
    f.render_widget(block, left);
    if inner.height >= 2 {
        let path_w = (inner.width as usize).saturating_sub(66).max(12);
        let header = format!(
            " {:>5}  {}  {:>8} {:>9} {:>9}  {:<12} {}",
            "#",
            pad("Path", path_w),
            "Touches",
            "Added",
            "Removed",
            "Churn",
            "Last change"
        );
        f.buffer_mut()
            .set_string(inner.x, inner.y, fit(&header, inner.width as usize), dim());
        let height = inner.height as usize - 1;
        let sel = app.sel[Tab::Files as usize];
        let top = scroll(sel, s.files.len(), height);
        let value = |r: &crate::query::FileRow| {
            if by_lines {
                r.added + r.removed
            } else {
                u64::from(r.touches)
            }
        };
        let max = s.files.first().map(value).unwrap_or(0);
        for (row, file) in s.files.iter().enumerate().skip(top).take(height) {
            let y = inner.y + 1 + (row - top) as u16;
            let style = row_style(row == sel);
            let buf = f.buffer_mut();
            buf.set_style(Rect::new(inner.x, y, inner.width, 1), style);
            let mut x = inner.x;
            let mut put = |text: String, st: Style| {
                buf.set_string(x, y, &text, st.patch(style));
                x += text.width() as u16;
            };
            put(format!(" {:>5}  ", row + 1), dim());
            let path = fit_tail(&store.paths[file.id as usize], path_w);
            put(
                format!("{path}{}", " ".repeat(path_w.saturating_sub(path.width()))),
                Style::new(),
            );
            put(
                format!("  {:>8}", fmt_n(u64::from(file.touches))),
                Style::new(),
            );
            put(
                format!(" {:>9}", format!("+{}", compact(file.added))),
                signed('+', file.added, GREEN),
            );
            put(
                format!(" {:>9}  ", format!("−{}", compact(file.removed))),
                signed('-', file.removed, RED),
            );
            put(
                pad(&hbar(value(file), max, 12), 12),
                Style::new().fg(gradient((row - top) as f64 / height.max(1) as f64)),
            );
            put(format!(" {}", app.clock.date(file.last)), dim());
        }
    }

    let [dirs, langs] = Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(right);
    let unit = if by_lines {
        "lines changed"
    } else {
        "times touched"
    };
    let value = |r: &crate::query::NamedRow| if by_lines { r.churn } else { r.touches };
    directories(f, store, s, dirs);
    let palette = [VIOLET, CYAN, GREEN, AMBER, PINK, RED];
    let rows: Vec<_> = s
        .langs
        .iter()
        .take(40)
        .enumerate()
        .map(|(i, l)| (l.name.clone(), value(l), palette[i % palette.len()]))
        .collect();
    bar_table(f, langs, &format!("File types · {unit}"), &rows);
}

fn commits(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(area);
    let mut block = panel(&format!("Commits · {}", fmt_n(s.commits.len() as u64)));
    if app.mode == Mode::Search {
        block = block.border_style(Style::new().fg(AMBER)).title(
            Line::from(vec![
                Span::styled(format!(" /{}", app.search), Style::new().fg(AMBER).bold()),
                Span::styled("▏ ", Style::new().fg(AMBER)),
            ])
            .right_aligned(),
        );
    } else {
        block = block.title(
            Line::from(" / searches subject, author, sha or YYYY-MM-DD ")
                .style(Style::new().fg(FAINT))
                .right_aligned(),
        );
    }
    let inner = block.inner(left);
    f.render_widget(block, left);
    let height = inner.height as usize;
    let sel = app.sel[Tab::Commits as usize];
    let top = scroll(sel, s.commits.len(), height);
    for (row, ci) in s.commits.iter().enumerate().skip(top).take(height) {
        let line = Rect::new(inner.x, inner.y + (row - top) as u16, inner.width, 1);
        commit_row(f.buffer_mut(), app, store, *ci as usize, line, row == sel);
    }

    commit_detail(f, app, store, s.commits.get(sel).copied(), right);
}

/// Everything about one commit: ids, people, both clocks, message body, files.
fn commit_detail(f: &mut Frame, app: &App, store: &Store, commit: Option<u32>, area: Rect) {
    let block = panel("Commit");
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, area);
    let Some(ci) = commit.as_ref() else {
        f.render_widget(Paragraph::new("No commits match.").style(dim()), inner);
        return;
    };
    let i = *ci as usize;
    let width = inner.width as usize;
    let author = &store.people[store.author[i] as usize];
    let committer = &store.people[store.committer[i] as usize];
    let t = store.time[i];
    // The same instant on the author's own clock.
    let tz = i64::from(store.tz[i]);
    let own = t + tz * 60;
    let (oy, om, od) = civil_from_days(own.div_euclid(time::DAY));
    let own_clock = format!(
        "{oy:04}-{om:02}-{od:02} {:02}:{:02} {}{:02}{:02}",
        own.rem_euclid(time::DAY) / 3600,
        own.rem_euclid(3600) / 60,
        if tz < 0 { '-' } else { '+' },
        tz.abs() / 60,
        tz.abs() % 60
    );
    let key = |k: &str| Span::styled(format!("{k:<10}"), dim());
    let parents = store.parents_of(i).len();
    let mut lines = vec![
        Line::from(Span::styled(store.hex_oid(i), Style::new().fg(AMBER))),
        Line::from(""),
    ];
    lines.extend(
        wrap(store.subject(i), width)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::new().bold()))),
    );
    lines.extend([
        Line::from(""),
        Line::from(vec![
            key("Author"),
            Span::styled(author.name.clone(), Style::new().fg(CYAN)),
            Span::styled(
                format!(
                    "  {}",
                    fit(
                        &author.email,
                        width.saturating_sub(13 + author.name.width())
                    )
                ),
                dim(),
            ),
        ]),
        Line::from(vec![
            key("Authored"),
            Span::raw(app.clock.datetime(t)),
            Span::styled(format!("  your time · {}", time::ago(app.now, t)), dim()),
        ]),
        Line::from(vec![
            key(""),
            Span::styled(format!("{own_clock}  on the author's clock"), dim()),
        ]),
        Line::from(vec![
            key("Committed"),
            Span::raw(app.clock.datetime(store.commit_time[i])),
            Span::styled(format!("  by {}", committer.name), dim()),
        ]),
        Line::from(vec![
            key("Type"),
            Span::raw(COMMIT_TYPES[store.ctype[i] as usize]),
            Span::styled(
                match parents {
                    0 => "  · root commit".to_owned(),
                    1 => String::new(),
                    n => format!("  · {n} parents (merges carry no file stats)"),
                },
                dim(),
            ),
        ]),
        Line::from(""),
    ]);

    // Message body: at most a third of the panel, so the file list keeps its room.
    if let Some((_, body)) = app
        .body
        .as_ref()
        .filter(|(have, text)| *have == *ci && !text.is_empty())
    {
        let room = (inner.height as usize / 3).max(3);
        let mut wrapped: Vec<String> = Vec::new();
        for para in body.lines() {
            if para.trim().is_empty() {
                wrapped.push(String::new());
            } else {
                wrapped.extend(wrap(para, width));
            }
        }
        let more = wrapped.len().saturating_sub(room);
        lines.extend(
            wrapped
                .into_iter()
                .take(room)
                .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Rgb(190, 197, 207))))),
        );
        if more > 0 {
            lines.push(Line::from(Span::styled(
                format!("… {more} more line{}", if more == 1 { "" } else { "s" }),
                dim(),
            )));
        }
        lines.push(Line::from(""));
    }

    let changes = store.changes_of(i);
    if store.stage == Stage::Commits {
        lines.push(Line::from(Span::styled("Files: scanning…", dim())));
    } else if !changes.is_empty() {
        let (a, r) = (u64::from(store.c_add[i]), u64::from(store.c_del[i]));
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{} file{}  ",
                    fmt_n(changes.len() as u64),
                    if changes.len() == 1 { "" } else { "s" }
                ),
                Style::new().bold(),
            ),
            Span::styled(format!("+{}", fmt_n(a)), signed('+', a, GREEN)),
            Span::styled(format!(" −{}", fmt_n(r)), signed('-', r, RED)),
        ]));
        let room = (inner.height as usize).saturating_sub(lines.len() + 1);
        let path_w = width.saturating_sub(18);
        for c in changes.clone().take(room) {
            let kind = store.ch_kind[c];
            let (tag, color) = match kind & !FLAG_BINARY {
                KIND_ADD => ("A", GREEN),
                KIND_DEL => ("D", RED),
                KIND_REN => ("R", AMBER),
                _ => ("M", CYAN),
            };
            let mut spans = vec![
                Span::styled(format!("{tag} "), Style::new().fg(color).bold()),
                Span::raw(pad(
                    &fit_tail(&store.paths[store.ch_file[c] as usize], path_w),
                    path_w,
                )),
            ];
            if kind & FLAG_BINARY != 0 {
                spans.push(Span::styled("  binary", dim()));
            } else {
                let (fa, fr) = (u64::from(store.ch_add[c]), u64::from(store.ch_del[c]));
                spans.push(Span::styled(format!(" +{fa}"), signed('+', fa, GREEN)));
                spans.push(Span::styled(format!(" −{fr}"), signed('-', fr, RED)));
            }
            lines.push(Line::from(spans));
        }
        if changes.len() > room {
            lines.push(Line::from(Span::styled(
                format!("… and {} more", fmt_n((changes.len() - room) as u64)),
                dim(),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// -------------------------------------------------------------------- lens

const LANE_COLORS: [Color; 6] = [VIOLET, CYAN, GREEN, AMBER, PINK, RED];

fn lens(f: &mut Frame, app: &mut App, store: &Store, area: Rect) {
    let refs_w = (area.width / 3).clamp(36, 58);
    let [left, right] =
        Layout::horizontal([Constraint::Length(refs_w), Constraint::Fill(1)]).areas(area);
    let detail_h = (area.height * 2 / 5).clamp(10, 26);
    let [graph_area, detail] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(detail_h)]).areas(right);
    let focus = |on: bool| {
        if on {
            Style::new().fg(VIOLET)
        } else {
            Style::new().fg(FAINT)
        }
    };

    // ---- references
    let visible = app.visible_refs();
    app.ref_sel = app.ref_sel.min(visible.len().saturating_sub(1));
    let kind = match app.ref_kind {
        None => "all",
        Some(crate::store::RefKind::Branch) => "branches",
        Some(crate::store::RefKind::Tag) => "tags",
        Some(crate::store::RefKind::Remote) => "remotes",
    };
    let mut title = format!("References · {kind} · {}", fmt_n(visible.len() as u64));
    if app.stale_only {
        title.push_str(" · stale 90d+");
    }
    let mut block = panel(&title).border_style(focus(!app.graph_focus));
    if app.mode == Mode::RefSearch || !app.ref_filter.is_empty() {
        let cursor = if app.mode == Mode::RefSearch {
            "▏"
        } else {
            ""
        };
        block = block.title(
            Line::from(format!(" /{}{cursor} ", app.ref_filter))
                .style(Style::new().fg(AMBER).bold())
                .right_aligned(),
        );
    }
    let inner = block.inner(left);
    f.render_widget(block, left);
    let height = inner.height as usize;
    let top = scroll(app.ref_sel, visible.len(), height);
    for (row, ri) in visible.iter().enumerate().skip(top).take(height) {
        let r = &app.refs[*ri];
        let y = inner.y + (row - top) as u16;
        let style = row_style(row == app.ref_sel && !app.graph_focus);
        let buf = f.buffer_mut();
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), style);
        let (tag, color) = match r.kind {
            crate::store::RefKind::Branch => ("⎇", GREEN),
            crate::store::RefKind::Tag => ("◆", AMBER),
            crate::store::RefKind::Remote => ("○", CYAN),
        };
        let current = r.name == app.ref_name;
        let age = time::ago(app.now, r.time);
        let tail = format!("{}  {age}", app.clock.date(r.time));
        let name_w = (inner.width as usize).saturating_sub(tail.width() + 6);
        buf.set_string(inner.x + 1, y, tag, Style::new().fg(color).patch(style));
        let name_style = if current {
            Style::new().fg(color).bold()
        } else {
            Style::new()
        };
        buf.set_string(
            inner.x + 3,
            y,
            pad(
                &format!("{}{}", r.name, if current { "  ← viewing" } else { "" }),
                name_w,
            ),
            name_style.patch(style),
        );
        buf.set_string(
            inner.right().saturating_sub(tail.width() as u16 + 1),
            y,
            tail,
            dim().patch(style),
        );
    }
    if visible.is_empty() {
        f.buffer_mut()
            .set_string(inner.x + 1, inner.y, "No refs match.", dim());
    }

    // ---- commit graph
    let total = app.graph.as_ref().map_or(0, |g| g.len());
    let sel = app.sel[Tab::Lens as usize].min(total.saturating_sub(1));
    let block = panel(&format!(
        "Commit graph · {} · {} commits · whole history",
        app.ref_name,
        fmt_n(total as u64)
    ))
    .border_style(focus(app.graph_focus))
    .title(
        Line::from(format!(
            " {} / {} ",
            fmt_n(sel as u64 + 1),
            fmt_n(total as u64)
        ))
        .style(dim())
        .right_aligned(),
    );
    let inner = block.inner(graph_area);
    f.render_widget(block, graph_area);
    let height = inner.height as usize;
    let top = scroll(sel, total, height);
    let rows = match &mut app.graph {
        Some(g) => g.rows(store, top, height),
        None => Vec::new(),
    };
    // Which refs sit on which of the visible commits.
    let labels: Vec<(usize, &str, Color)> = app
        .refs
        .iter()
        .filter_map(|r| {
            let color = match r.kind {
                crate::store::RefKind::Branch => GREEN,
                crate::store::RefKind::Tag => AMBER,
                crate::store::RefKind::Remote => CYAN,
            };
            store
                .find(&r.oid, r.author_time)
                .map(|i| (i, r.name.as_str(), color))
        })
        .collect();
    let max_lanes = ((inner.width as usize) / 10).clamp(4, 14);
    let lanes_w = rows
        .iter()
        .map(|r| r.cells.len())
        .max()
        .unwrap_or(1)
        .min(max_lanes * 2);
    for (k, row) in rows.iter().enumerate() {
        let i = row.commit as usize;
        let y = inner.y + k as u16;
        let selected = top + k == sel;
        let style = row_style(selected && app.graph_focus);
        let buf = f.buffer_mut();
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), style);
        // When the graph is wider than its column, keep the left lanes and
        // still show this commit's own node at the right edge.
        let overflow = row.cells.len() > lanes_w;
        let shown = if overflow {
            lanes_w.saturating_sub(3)
        } else {
            lanes_w
        };
        let lane_style = |glyph: char, color: u8| {
            let st = Style::new().fg(LANE_COLORS[color as usize % LANE_COLORS.len()]);
            if matches!(glyph, '●' | '◆') {
                st.bold()
            } else {
                st
            }
        };
        for (c, (glyph, color)) in row.cells.iter().take(shown).enumerate() {
            buf.set_string(
                inner.x + 1 + c as u16,
                y,
                glyph.to_string(),
                lane_style(*glyph, *color).patch(style),
            );
        }
        if overflow {
            buf.set_string(inner.x + 1 + shown as u16, y, "…", dim().patch(style));
            if let Some((glyph, color)) = row
                .cells
                .iter()
                .skip(shown)
                .find(|c| matches!(c.0, '●' | '◆'))
            {
                buf.set_string(
                    inner.x + lanes_w as u16,
                    y,
                    glyph.to_string(),
                    lane_style(*glyph, *color).patch(style),
                );
            }
        }
        let mut x = inner.x + 2 + lanes_w as u16;
        let right_w = 34u16; // author + date
        let limit = inner.right().saturating_sub(right_w);
        buf.set_string(
            x,
            y,
            store.short_oid(i),
            Style::new().fg(AMBER).patch(style),
        );
        x += 9;
        for (_, name, color) in labels.iter().filter(|l| l.0 == i).take(3) {
            let chip = format!(" {} ", fit(name, 22));
            if x + chip.width() as u16 + 8 > limit {
                break;
            }
            buf.set_string(x, y, &chip, Style::new().fg(Color::Black).bg(*color));
            x += chip.width() as u16 + 1;
        }
        let subject_w = limit.saturating_sub(x) as usize;
        let subject_style = if store.is_merge(i) {
            dim()
        } else {
            Style::new()
        };
        buf.set_string(
            x,
            y,
            fit(store.subject(i), subject_w.saturating_sub(1)),
            subject_style.patch(style),
        );
        buf.set_string(
            limit,
            y,
            pad(&store.people[store.author[i] as usize].name, 16),
            Style::new().fg(CYAN).patch(style),
        );
        buf.set_string(
            limit + 17,
            y,
            app.clock.datetime(store.time[i]),
            dim().patch(style),
        );
    }

    let selected = app.graph.as_ref().and_then(|g| g.order.get(sel)).copied();
    commit_detail(f, app, store, selected, detail);
}

// ------------------------------------------------------------------ popups

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn help(f: &mut Frame, area: Rect) {
    let rows = [
        ("Views", ""),
        (
            "1 2 3 4 5 6",
            "Overview · Activity · Contributors · Files · Commits · Lens",
        ),
        ("Tab / Shift-Tab", "next / previous view"),
        ("", ""),
        ("Time", ""),
        ("← →  or  h l", "previous / next period"),
        ("- +", "shorter / longer range"),
        ("d w m s y Y f 0", "1D · 5D · 1M · 6M · YTD · 1Y · 5Y · All"),
        ("t", "jump to the most recent period"),
        ("", ""),
        ("Filter", ""),
        (
            "a",
            "pick a contributor (Enter on the Contributors view too)",
        ),
        ("/", "search commits: subject, author, sha, or YYYY-MM-DD"),
        (
            "Enter  (Files)",
            "show only the selected file; u widens to its folder, then up",
        ),
        ("x  or  Esc", "clear every filter and search"),
        ("", ""),
        ("Lens", ""),
        ("p", "move between the ref list and the commit graph"),
        (
            "Enter  (on a ref)",
            "analyse that branch or tag instead of the current one",
        ),
        (
            "r  S  /",
            "ref kind · only stale refs (90+ days) · filter refs by name",
        ),
        ("", ""),
        ("Lists", ""),
        ("↑ ↓  j k", "move"),
        ("PgUp PgDn  g G", "page · top · bottom"),
        ("", ""),
        ("q", "quit"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                Line::from(Span::styled(
                    format!(" {k}"),
                    Style::new().fg(VIOLET).bold(),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("   {k:<18}"), Style::new().fg(AMBER)),
                    Span::raw(*v),
                ])
            }
        })
        .collect();
    let rect = centered(area, 80, lines.len() as u16 + 4);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(VIOLET))
        .title(" Keys ")
        .title_bottom(
            Line::from(" all dates and ranges are in your local time zone · any key closes ")
                .style(dim())
                .centered(),
        )
        .padding(ratatui::widgets::Padding::vertical(1));
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn picker(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.picker_rows();
    app.picker_sel = app.picker_sel.min(rows.len().saturating_sub(1));
    let rect = centered(area, 64, 18);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(CYAN))
        .title(" Filter by contributor ")
        .title_bottom(
            Line::from(format!(" {} match ", rows.len()))
                .style(dim())
                .right_aligned(),
        );
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let Some(store) = &app.store else { return };
    let buf = f.buffer_mut();
    buf.set_string(
        inner.x + 1,
        inner.y,
        format!("› {}▏", app.picker),
        Style::new().fg(CYAN).bold(),
    );
    let height = inner.height.saturating_sub(2) as usize;
    let top = scroll(app.picker_sel, rows.len(), height);
    for (row, id) in rows.iter().enumerate().skip(top).take(height) {
        let y = inner.y + 2 + (row - top) as u16;
        let p = &store.people[*id as usize];
        let style = row_style(row == app.picker_sel);
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), style);
        buf.set_string(
            inner.x + 1,
            y,
            pad(&p.name, 26),
            Style::new().bold().patch(style),
        );
        buf.set_string(
            inner.x + 28,
            y,
            fit(&p.email, (inner.width as usize).saturating_sub(29)),
            dim().patch(style),
        );
    }
}

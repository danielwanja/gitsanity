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
            app.hits.push((Rect::new(x, area.y, w, 1), Click::Preset(p)));
        }
        x += w + 1;
    }
}

fn draw_sub(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(w) = app.window else { return };
    let movable = |dir| w.shifted(&app.clock, dir, app.bounds).is_some();
    let arrow = |on: bool| if on { Style::new().fg(VIOLET).bold() } else { Style::new().fg(FAINT) };
    let label = w.label();
    let bucket = app.summary.as_ref().map(|s| s.buckets.bucket.label()).unwrap_or("day");
    let days = w.days();
    let mut spans = vec![
        Span::styled(" ‹ ", arrow(movable(-1))),
        Span::styled(label.clone(), Style::new().bold()),
        Span::styled(" › ", arrow(movable(1))),
        Span::styled(
            format!("  {} day{} · by {bucket} · times in {}", fmt_n(days as u64), if days == 1 { "" } else { "s" }, app.clock.name),
            dim(),
        ),
    ];
    app.hits.push((Rect::new(area.x, area.y, 3, 1), Click::Shift(-1)));
    app.hits.push((Rect::new(area.x + 3 + label.width() as u16, area.y, 3, 1), Click::Shift(1)));
    if let (Some(author), Some(store)) = (app.filter.author, &app.store) {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(format!(" ● {} ", store.people[author as usize].name), Style::new().fg(Color::Black).bg(CYAN)));
        spans.push(Span::styled(" x clears", dim()));
    }
    if !app.search.is_empty() && app.mode != Mode::Search {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(format!(" /{} ", app.search), Style::new().fg(Color::Black).bg(AMBER)));
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
            buf.set_string(x, y, &text, Style::new().fg(VIOLET).bold().add_modifier(Modifier::UNDERLINED));
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
        _ => match app.tab {
            Tab::Contributors => " ←→ period · -/+ range · ↑↓ select · Enter filter · a contributor · ? help · q quit",
            Tab::Commits => " ←→ period · -/+ range · ↑↓ select · / search · a contributor · ? help · q quit",
            _ => " ←→ period · -/+ range · 1-5 tabs · / search · a contributor · ? help · q quit",
        },
    };
    let p = &app.progress;
    let spin = SPINNER[(app.started.elapsed().as_millis() / 80) as usize % SPINNER.len()];
    let pct = |done: u64, total: u64| if total == 0 { 0 } else { (done * 100 / total).min(100) };
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
                Span::styled(format!("{} ", fmt_n(p.commits.load(Ordering::Relaxed))), Style::new().bold()),
            ]),
            git::PHASE_DIFF => {
                let pc = pct(p.diffed.load(Ordering::Relaxed), p.diff_total.load(Ordering::Relaxed));
                Line::from(vec![
                    Span::styled(format!("{spin} finding changed files "), Style::new().fg(AMBER)),
                    Span::styled(meter(pc), Style::new().fg(VIOLET)),
                    Span::styled(format!(" {pc:>3}% "), Style::new().bold()),
                ])
            }
            git::PHASE_LINES => {
                let pc = pct(p.lined.load(Ordering::Relaxed), p.line_total.load(Ordering::Relaxed));
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
        Line::from(Span::styled(format!("{spin} Reading history"), Style::new().fg(VIOLET).bold())),
        Line::from(""),
        Line::from(Span::styled(format!("{} commits so far", fmt_n(n)), dim())),
        Line::from(Span::styled(app.repo_path.display().to_string(), Style::new().fg(FAINT))),
    ];
    let [mid] = Layout::vertical([Constraint::Length(4)]).flex(ratatui::layout::Flex::Center).areas(area);
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
            let (arrow, color) = if pct >= 0.0 { ("▲", GREEN) } else { ("▼", RED) };
            Line::from(vec![
                Span::styled(format!("{arrow} {:.1}%", pct.abs()), Style::new().fg(color)),
                Span::styled(if width >= 22 { " vs previous period" } else { " vs prev" }, dim()),
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
            .map(|(i, c)| Span::styled(c.to_string(), Style::new().fg(gradient(i as f64 / n as f64))))
            .collect::<Vec<_>>(),
    )
}

fn lines_pending(app: &App, store: &Store) -> Option<String> {
    (store.stage != Stage::Lines).then(|| {
        let p = &app.progress;
        let total = p.line_total.load(Ordering::Relaxed);
        if store.stage == Stage::Files && total > 0 {
            format!("counting… {}%", p.lined.load(Ordering::Relaxed) * 100 / total)
        } else {
            "counting…".to_owned()
        }
    })
}

fn overview(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    let bottom_h = if area.height >= 30 { 12 } else { 10 };
    let [kpis, chart, bottom] =
        Layout::vertical([Constraint::Length(5), Constraint::Min(6), Constraint::Length(bottom_h)]).areas(area);

    let cards: [Rect; 5] = Layout::horizontal([Constraint::Fill(1); 5]).areas(kpis);
    let prev = s.previous;
    let pending = lines_pending(app, store);
    let files_pending = store.stage == Stage::Commits;
    let churn: Vec<u64> = s.b_added.iter().zip(&s.b_removed).map(|(a, r)| a + r).collect();
    let inner_w = cards[0].width.saturating_sub(4) as usize;
    let big = |text: String| Line::from(Span::styled(text, Style::new().bold()));
    let card = |f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>| {
        f.render_widget(Paragraph::new(lines).block(panel(title).padding(ratatui::widgets::Padding::horizontal(1))), area);
    };
    let prev_of = |get: fn(&Totals) -> u64| prev.as_ref().map(get);
    card(f, cards[0], "Commits", vec![
        big(fmt_n(s.totals.commits)),
        delta_line(s.totals.commits, prev_of(|t| t.commits), inner_w),
        gradient_spark(&s.b_commits, inner_w),
    ]);
    card(f, cards[1], "Contributors", vec![
        big(fmt_n(s.totals.authors)),
        delta_line(s.totals.authors, prev_of(|t| t.authors), inner_w),
        Line::from(Span::styled(
            s.authors.first().map(|a| format!("top: {}", fit(&store.people[a.id as usize].name, inner_w.saturating_sub(5)))).unwrap_or_default(),
            dim(),
        )),
    ]);
    card(f, cards[2], "Files touched", if files_pending {
        vec![Line::from(Span::styled("scanning…", dim()))]
    } else {
        vec![
            big(fmt_n(s.totals.files)),
            delta_line(s.totals.files, prev_of(|t| t.files), inner_w),
            Line::from(Span::styled(format!("{} changes", compact(s.kinds.iter().map(|k| k.1).sum())), dim())),
        ]
    });
    card(f, cards[3], "Lines changed", match &pending {
        Some(p) => vec![Line::from(Span::styled(p.clone(), dim()))],
        None => vec![
            Line::from(vec![
                Span::styled(format!("+{}", compact(s.totals.added)), Style::new().fg(GREEN).bold()),
                Span::styled(format!("  −{}", compact(s.totals.removed)), Style::new().fg(RED).bold()),
            ]),
            delta_line(s.totals.churn(), prev_of(Totals::churn), inner_w),
            gradient_spark(&churn, inner_w),
        ],
    });
    card(f, cards[4], "Busiest day", match (&pending, s.biggest_day) {
        (Some(p), _) => vec![Line::from(Span::styled(p.clone(), dim()))],
        (None, None) => vec![Line::from(Span::styled("—", dim()))],
        (None, Some((day, lines))) => {
            let (y, m, d) = civil_from_days(day);
            vec![
                big(format!("{} {d}, {y}", month_name(m))),
                Line::from(Span::styled(format!("{} lines changed", fmt_n(lines)), dim())),
                Line::from(Span::styled(format!("{} merges in range", fmt_n(s.merges)), Style::new().fg(FAINT))),
            ]
        }
    });

    timeline(f, app, s, chart);

    let weeks_fit = ((area.width as usize * 2 / 5).saturating_sub(8) / 2).clamp(8, 53);
    let cal_w = (weeks_fit * 2 + 8) as u16;
    let [cal, hot, people] =
        Layout::horizontal([Constraint::Length(cal_w), Constraint::Fill(1), Constraint::Fill(1)]).areas(bottom);
    calendar(f, app, s, cal, weeks_fit);

    let by_lines = store.stage == Stage::Lines;
    let block = panel(if by_lines { "Hot files · lines changed" } else { "Hot files · times touched" });
    let inner = block.inner(hot);
    f.render_widget(block, hot);
    let value = |r: &crate::query::FileRow| if by_lines { r.added + r.removed } else { u64::from(r.touches) };
    let max = s.files.first().map(value).unwrap_or(0);
    for (row, file) in s.files.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + row as u16;
        let v = value(file);
        let buf = f.buffer_mut();
        buf.set_string(inner.x + 1, y, pad_left(&compact(v), 6), Style::new().bold());
        buf.set_string(inner.x + 8, y, hbar(v, max, 8), Style::new().fg(gradient(row as f64 / 10.0)));
        let w = (inner.width as usize).saturating_sub(18);
        buf.set_string(inner.x + 17, y, fit_tail(&store.paths[file.id as usize], w), Style::new());
    }

    let block = panel("Top contributors · commits");
    let inner = block.inner(people);
    f.render_widget(block, people);
    let max = s.authors.first().map(|a| u64::from(a.commits)).unwrap_or(0);
    for (row, a) in s.authors.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + row as u16;
        let buf = f.buffer_mut();
        buf.set_string(inner.x + 1, y, pad_left(&compact(u64::from(a.commits)), 6), Style::new().bold());
        buf.set_string(inner.x + 8, y, hbar(u64::from(a.commits), max, 8), Style::new().fg(gradient(row as f64 / 10.0)));
        let w = (inner.width as usize).saturating_sub(18);
        buf.set_string(inner.x + 17, y, fit(&store.people[a.id as usize].name, w), Style::new());
    }
}

/// Axis labels under a bar chart: first, last, and as many evenly spaced ones as fit.
fn axis_labels(buf: &mut Buffer, app: &App, s: &Summary, area: Rect) {
    let n = s.buckets.len();
    if n == 0 || area.width < 8 {
        return;
    }
    let mut next_free = area.x;
    let slots = (area.width as usize / 16).max(2);
    let every = n.div_ceil(slots).max(1);
    for i in (0..n).step_by(every) {
        let mut label = s.buckets.label(&app.clock, i);
        if s.buckets.bucket == Bucket::Hour && app.window.is_some_and(|w| w.days() == 1) {
            label = label.rsplit(' ').next().unwrap_or_default().to_owned();
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
    let block = panel(&format!("Commits per {}", s.buckets.bucket.label()))
        .title(Line::from(format!(" peak {} ", fmt_n(peak))).style(dim()).right_aligned());
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let bars = Rect { height: inner.height - 1, ..inner };
    let n = s.b_commits.len().max(1);
    bars_up(f.buffer_mut(), bars, &s.b_commits, peak, |i, _| gradient(i as f64 / n as f64));
    axis_labels(f.buffer_mut(), app, s, Rect { y: inner.bottom() - 1, height: 1, ..inner });
}

fn calendar(f: &mut Frame, app: &App, s: &Summary, area: Rect, weeks: usize) {
    let Some(w) = app.window else { return };
    // Columns are Monday-first weeks ending with the week of the window's last day.
    let last_monday = w.last_day - (w.last_day + 3).rem_euclid(7);
    let weeks = weeks.min(((w.last_day - w.first_day) / 7 + 2) as usize).max(1);
    let first_monday = last_monday - 7 * (weeks as i64 - 1);
    let (ly, lm, ld) = civil_from_days(w.last_day);
    let block = panel(&format!("Commits per day · {weeks} weeks to {} {ld}, {ly}", month_name(lm)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 8 {
        return;
    }
    let value = |day: i64| -> Option<u64> {
        (day >= w.first_day && day <= w.last_day).then(|| u64::from(s.daily[(day - w.first_day) as usize]))
    };
    let max = (first_monday..=w.last_day).filter_map(value).max().unwrap_or(0);
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
                buf.set_string(x, inner.y + 1 + dow as u16, "■", Style::new().fg(heat(heat_level(v, max))));
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
            buf.set_string(x0 + 5 + level * 2, y, "■", Style::new().fg(heat(level as usize)));
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
    let name_w = rows.iter().map(|r| r.0.width()).max().unwrap_or(0).min((inner.width as usize / 3).max(8));
    let bar_w = (inner.width as usize).saturating_sub(name_w + 20);
    for (row, (name, value, color)) in rows.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + row as u16;
        let buf = f.buffer_mut();
        buf.set_string(inner.x + 1, y, pad(name, name_w), Style::new());
        buf.set_string(inner.x + 2 + name_w as u16, y, hbar(*value, max, bar_w), Style::new().fg(*color));
        let share = if total == 0 { 0.0 } else { *value as f64 / total as f64 * 100.0 };
        let tail = format!("{:>8} {share:>5.1}%", compact(*value));
        buf.set_string(inner.right().saturating_sub(tail.len() as u16 + 1), y, tail, dim());
    }
}

fn activity(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    let [chart, bottom] = Layout::vertical([Constraint::Min(8), Constraint::Length(11)]).areas(area);
    let by_lines = store.stage == Stage::Lines;
    let (max_a, max_r) = (s.b_added.iter().copied().max().unwrap_or(0), s.b_removed.iter().copied().max().unwrap_or(0));
    let title = if by_lines {
        format!("Code frequency · lines added ▲ and removed ▼ per {}", s.buckets.bucket.label())
    } else {
        format!("Code frequency per {} · {}", s.buckets.bucket.label(), lines_pending(app, store).unwrap_or_default())
    };
    let block = panel(&title).title(
        Line::from(vec![
            Span::styled(format!(" +{}", compact(s.totals.added)), Style::new().fg(GREEN)),
            Span::styled(format!(" −{} ", compact(s.totals.removed)), Style::new().fg(RED)),
        ])
        .right_aligned(),
    );
    let inner = block.inner(chart).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, chart);
    if inner.height >= 4 {
        // Split the height in proportion to the two peaks, then use one scale
        // (lines per row) for both halves so they are directly comparable.
        let plot_h = inner.height - 1;
        let up_h = ((u64::from(plot_h) * max_a.max(1)).div_ceil((max_a + max_r).max(1)) as u16).clamp(1, plot_h - 1);
        let down_h = plot_h - up_h;
        let per_row = (max_a as f64 / f64::from(up_h)).max(max_r as f64 / f64::from(down_h)).max(1.0);
        let up = Rect { height: up_h, ..inner };
        let down = Rect { y: inner.y + up_h, height: down_h, ..inner };
        bars_up(f.buffer_mut(), up, &s.b_added, (per_row * f64::from(up_h)).ceil() as u64, |_, _| GREEN);
        bars_down(f.buffer_mut(), down, &s.b_removed, (per_row * f64::from(down_h)).ceil() as u64, RED);
        axis_labels(f.buffer_mut(), app, s, Rect { y: inner.bottom() - 1, height: 1, ..inner });
    }

    let [types, kinds, rhythm] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1), Constraint::Length(58)]).areas(bottom);
    let palette = [VIOLET, CYAN, GREEN, AMBER, PINK, RED];
    let rows: Vec<_> =
        s.ctypes.iter().enumerate().map(|(i, (n, c, _))| ((*n).to_owned(), *c, palette[i % palette.len()])).collect();
    bar_table(f, types, "Commit types · commits", &rows);
    let colors = [GREEN, CYAN, RED, AMBER, DIM];
    let rows: Vec<_> = s.kinds.iter().zip(colors).filter(|(k, _)| k.1 > 0).map(|((n, e, _), c)| ((*n).to_owned(), *e, c)).collect();
    bar_table(f, kinds, "File changes · events", &rows);

    let block = panel("Work rhythm · commits by weekday × hour, your time");
    let inner = block.inner(rhythm);
    f.render_widget(block, rhythm);
    let max = s.rhythm.iter().flatten().copied().max().unwrap_or(0);
    let buf = f.buffer_mut();
    for hour in (0..24).step_by(3) {
        buf.set_string(inner.x + 5 + hour * 2, inner.y, format!("{hour:02}"), dim());
    }
    for (d, name) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].iter().enumerate() {
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

// ------------------------------------------------------------------ tables

/// First visible row so the selection stays roughly centred.
fn scroll(sel: usize, rows: usize, height: usize) -> usize {
    sel.saturating_sub(height / 2).min(rows.saturating_sub(height))
}

fn row_style(selected: bool) -> Style {
    if selected { Style::new().bg(SELECT_BG) } else { Style::new() }
}

fn contributors(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    let block = panel(&format!("Contributors · {} active in range", fmt_n(s.authors.len() as u64)))
        .title(Line::from(" Enter filters everything by the selected contributor ").style(Style::new().fg(FAINT)).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let name_w = (inner.width as usize).saturating_sub(96).clamp(16, 40);
    let header = format!(
        " {:>5}  {}  {:>9}  {:<18} {:>10} {:>10}  {:<10}  {:<10}  {:<9}",
        "#",
        pad("Name", name_w),
        "Commits",
        "Share",
        "Added",
        "Removed",
        "First",
        "Last",
        "Active"
    );
    f.buffer_mut().set_string(inner.x, inner.y, fit(&header, inner.width as usize), dim());
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
        put(pad(&format!("{}{}", if filtered { "● " } else { "" }, p.name), name_w), if filtered { Style::new().fg(CYAN).bold() } else { Style::new().bold() });
        put(format!("  {:>9}  ", fmt_n(u64::from(a.commits))), Style::new());
        put(pad(&hbar(u64::from(a.commits), max, 18), 18), Style::new().fg(gradient((row - top) as f64 / height.max(1) as f64)));
        put(format!(" {:>10}", format!("+{}", compact(a.added))), Style::new().fg(GREEN));
        put(format!(" {:>10}", format!("−{}", compact(a.removed))), Style::new().fg(RED));
        put(format!("  {}  {}  ", app.clock.date(a.first), app.clock.date(a.last)), dim());
        put(time::ago(app.now, a.last), dim());
    }
}

fn files(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(area);
    let by_lines = store.stage == Stage::Lines;
    let block = panel(&format!(
        "Hot files · {} touched in range{}",
        fmt_n(s.totals.files),
        if s.files.len() as u64 == s.totals.files { String::new() } else { format!(" · top {}", fmt_n(s.files.len() as u64)) }
    ));
    let inner = block.inner(left);
    f.render_widget(block, left);
    if inner.height >= 2 {
        let path_w = (inner.width as usize).saturating_sub(66).max(12);
        let header = format!(" {:>5}  {}  {:>8} {:>9} {:>9}  {:<12} {}", "#", pad("Path", path_w), "Touches", "Added", "Removed", "Churn", "Last change");
        f.buffer_mut().set_string(inner.x, inner.y, fit(&header, inner.width as usize), dim());
        let height = inner.height as usize - 1;
        let sel = app.sel[Tab::Files as usize];
        let top = scroll(sel, s.files.len(), height);
        let value = |r: &crate::query::FileRow| if by_lines { r.added + r.removed } else { u64::from(r.touches) };
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
            put(format!("{path}{}", " ".repeat(path_w.saturating_sub(path.width()))), Style::new());
            put(format!("  {:>8}", fmt_n(u64::from(file.touches))), Style::new());
            put(format!(" {:>9}", format!("+{}", compact(file.added))), Style::new().fg(GREEN));
            put(format!(" {:>9}  ", format!("−{}", compact(file.removed))), Style::new().fg(RED));
            put(pad(&hbar(value(file), max, 12), 12), Style::new().fg(gradient((row - top) as f64 / height.max(1) as f64)));
            put(format!(" {}", app.clock.date(file.last)), dim());
        }
    }

    let [dirs, langs] = Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(right);
    let unit = if by_lines { "lines changed" } else { "times touched" };
    let value = |r: &crate::query::NamedRow| if by_lines { r.churn } else { r.touches };
    let rows: Vec<_> = s.dirs.iter().take(40).enumerate().map(|(i, d)| (d.name.clone(), value(d), gradient(i as f64 / 12.0))).collect();
    bar_table(f, dirs, &format!("Directories · {unit}"), &rows);
    let palette = [VIOLET, CYAN, GREEN, AMBER, PINK, RED];
    let rows: Vec<_> = s.langs.iter().take(40).enumerate().map(|(i, l)| (l.name.clone(), value(l), palette[i % palette.len()])).collect();
    bar_table(f, langs, &format!("File types · {unit}"), &rows);
}

fn commits(f: &mut Frame, app: &App, store: &Store, s: &Summary, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).areas(area);
    let mut block = panel(&format!("Commits · {}", fmt_n(s.commits.len() as u64)));
    if app.mode == Mode::Search {
        block = block.border_style(Style::new().fg(AMBER)).title(
            Line::from(vec![Span::styled(format!(" /{}", app.search), Style::new().fg(AMBER).bold()), Span::styled("▏ ", Style::new().fg(AMBER))])
                .right_aligned(),
        );
    } else {
        block = block.title(Line::from(" / searches subject, author, sha or YYYY-MM-DD ").style(Style::new().fg(FAINT)).right_aligned());
    }
    let inner = block.inner(left);
    f.render_widget(block, left);
    let height = inner.height as usize;
    let sel = app.sel[Tab::Commits as usize];
    let top = scroll(sel, s.commits.len(), height);
    let by_lines = store.stage == Stage::Lines;
    for (row, ci) in s.commits.iter().enumerate().skip(top).take(height) {
        let i = *ci as usize;
        let y = inner.y + (row - top) as u16;
        let style = row_style(row == sel);
        let buf = f.buffer_mut();
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), style);
        let stats = if by_lines { format!("+{} −{}", compact(u64::from(store.c_add[i])), compact(u64::from(store.c_del[i]))) } else { String::new() };
        let mut x = inner.x + 1;
        let mut put = |text: String, st: Style| {
            buf.set_string(x, y, &text, st.patch(style));
            x += text.width() as u16;
        };
        put(format!("{} ", store.short_oid(i)), Style::new().fg(AMBER));
        put(format!("{} ", app.clock.datetime(store.time[i])), dim());
        put(format!("{} ", pad(&store.people[store.author[i] as usize].name, 16)), Style::new().fg(CYAN));
        let subject_w = (inner.width as usize).saturating_sub(46 + stats.width() + 2);
        put(pad(store.subject(i), subject_w), if store.is_merge(i) { dim() } else { Style::new() });
        if by_lines && !store.is_merge(i) {
            put(format!(" +{}", compact(u64::from(store.c_add[i]))), Style::new().fg(GREEN));
            put(format!(" −{}", compact(u64::from(store.c_del[i]))), Style::new().fg(RED));
        }
    }

    let block = panel("Commit");
    let inner = block.inner(right).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, right);
    let Some(ci) = s.commits.get(sel) else {
        f.render_widget(Paragraph::new("No commits match.").style(dim()), inner);
        return;
    };
    let i = *ci as usize;
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
        Line::from(Span::styled(store.subject(i).to_owned(), Style::new().bold())),
        Line::from(""),
        Line::from(vec![key("Author"), Span::styled(author.name.clone(), Style::new().fg(CYAN)), Span::styled(format!("  {}", author.email), dim())]),
        Line::from(vec![key("Authored"), Span::raw(app.clock.datetime(t)), Span::styled(format!("  your time · {}", time::ago(app.now, t)), dim())]),
        Line::from(vec![key(""), Span::styled(format!("{own_clock}  on the author's clock"), dim())]),
        Line::from(vec![key("Committed"), Span::raw(app.clock.datetime(store.commit_time[i])), Span::styled(format!("  by {}", committer.name), dim())]),
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
    ];
    let changes = store.changes_of(i);
    if store.stage == Stage::Commits {
        lines.push(Line::from(Span::styled("Files: scanning…", dim())));
    } else if !changes.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(format!("{} file{}  ", changes.len(), if changes.len() == 1 { "" } else { "s" }), Style::new().bold()),
            Span::styled(format!("+{}", fmt_n(u64::from(store.c_add[i]))), Style::new().fg(GREEN)),
            Span::styled(format!(" −{}", fmt_n(u64::from(store.c_del[i]))), Style::new().fg(RED)),
        ]));
        let room = (inner.height as usize).saturating_sub(lines.len() + 1);
        let path_w = (inner.width as usize).saturating_sub(18);
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
                Span::raw(pad(&fit_tail(&store.paths[store.ch_file[c] as usize], path_w), path_w)),
            ];
            if kind & FLAG_BINARY != 0 {
                spans.push(Span::styled("  binary", dim()));
            } else {
                spans.push(Span::styled(format!(" +{}", store.ch_add[c]), Style::new().fg(GREEN)));
                spans.push(Span::styled(format!(" −{}", store.ch_del[c]), Style::new().fg(RED)));
            }
            lines.push(Line::from(spans));
        }
        if changes.len() > room {
            lines.push(Line::from(Span::styled(format!("… and {} more", changes.len() - room), dim())));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// ------------------------------------------------------------------ popups

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn help(f: &mut Frame, area: Rect) {
    let rows = [
        ("Views", ""),
        ("1 2 3 4 5", "Overview · Activity · Contributors · Files · Commits"),
        ("Tab / Shift-Tab", "next / previous view"),
        ("", ""),
        ("Time", ""),
        ("← →  or  h l", "previous / next period"),
        ("- +", "shorter / longer range"),
        ("d w m s y Y f 0", "1D · 5D · 1M · 6M · YTD · 1Y · 5Y · All"),
        ("t", "jump to the most recent period"),
        ("", ""),
        ("Filter", ""),
        ("a", "pick a contributor (Enter on the Contributors view too)"),
        ("/", "search commits: subject, author, sha, or YYYY-MM-DD"),
        ("x  or  Esc", "clear filter and search"),
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
                Line::from(Span::styled(format!(" {k}"), Style::new().fg(VIOLET).bold()))
            } else {
                Line::from(vec![Span::styled(format!("   {k:<18}"), Style::new().fg(AMBER)), Span::raw(*v)])
            }
        })
        .collect();
    let rect = centered(area, 80, lines.len() as u16 + 4);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(VIOLET))
        .title(" Keys ")
        .title_bottom(Line::from(" all dates and ranges are in your local time zone · any key closes ").style(dim()).centered())
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
        .title_bottom(Line::from(format!(" {} match ", rows.len())).style(dim()).right_aligned());
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let Some(store) = &app.store else { return };
    let buf = f.buffer_mut();
    buf.set_string(inner.x + 1, inner.y, format!("› {}▏", app.picker), Style::new().fg(CYAN).bold());
    let height = inner.height.saturating_sub(2) as usize;
    let top = scroll(app.picker_sel, rows.len(), height);
    for (row, id) in rows.iter().enumerate().skip(top).take(height) {
        let y = inner.y + 2 + (row - top) as u16;
        let p = &store.people[*id as usize];
        let style = row_style(row == app.picker_sel);
        buf.set_style(Rect::new(inner.x, y, inner.width, 1), style);
        buf.set_string(inner.x + 1, y, pad(&p.name, 26), Style::new().bold().patch(style));
        buf.set_string(inner.x + 28, y, fit(&p.email, (inner.width as usize).saturating_sub(29)), dim().patch(style));
    }
}

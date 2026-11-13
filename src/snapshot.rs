//! Render a screen off-terminal: plain text for tests and docs, HTML (with the
//! real colours) for screenshots.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier};

use crate::git::Progress;
use crate::store;
use crate::time::{Clock, Preset};
use crate::tui::{App, Tab, ui};

#[allow(clippy::too_many_arguments)]
pub fn run(path: &Path, clock: Clock, tab: &str, range: &str, width: u16, height: u16, keys: &str, html: Option<&Path>) -> Result<()> {
    let info = store::open(path)?;
    let progress = Arc::new(Progress::default());
    let (store, cached) = match store::load_cache(&info) {
        Some(s) => (Arc::new(s), true),
        None => {
            let (s, _) = store::index(&info, &progress, true, |_| {})?;
            store::save_cache(&info, &s)?;
            (s, false)
        }
    };
    progress.phase.store(crate::git::PHASE_DONE, std::sync::atomic::Ordering::Relaxed);
    // "Now" is the newest commit, so snapshots do not drift from day to day.
    let now = store.time.last().copied().unwrap_or(0);
    let mut app = App::new(&info, clock, now, progress);
    app.set_store(store, cached);
    app.tab = Tab::ALL.into_iter().find(|t| t.label().eq_ignore_ascii_case(tab)).with_context(|| format!("no tab named {tab}"))?;
    let Some(preset) = Preset::ALL.into_iter().find(|p| p.label().eq_ignore_ascii_case(range)) else { bail!("no range named {range}") };
    app.set_preset(preset);

    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|f| ui::draw(f, &mut app))?;
    for c in keys.chars() {
        let code = match c {
            '\n' => KeyCode::Enter,
            '<' => KeyCode::Left,
            '>' => KeyCode::Right,
            c => KeyCode::Char(c),
        };
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        terminal.draw(|f| ui::draw(f, &mut app))?;
    }
    let buf = terminal.backend().buffer().clone();
    match html {
        Some(out) => std::fs::write(out, to_html(&buf)).with_context(|| format!("writing {}", out.display()))?,
        None => print!("{}", to_text(&buf)),
    }
    Ok(())
}

pub fn to_text(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn css(color: Color, fallback: &str) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
        Color::Black => "#0d1117".into(),
        Color::White => "#ffffff".into(),
        _ => fallback.into(),
    }
}

fn to_html(buf: &Buffer) -> String {
    let (fg0, bg0) = ("#e6edf3", "#0d1117");
    let mut out = format!(
        "<!doctype html><meta charset=utf-8><title>gitsanity</title><body style=\"margin:0;background:{bg0}\">\
         <pre style=\"margin:0;padding:14px 18px;display:inline-block;background:{bg0};color:{fg0};\
         font:14px/17px Menlo,'DejaVu Sans Mono',monospace\">"
    );
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            let (mut fg, mut bg) = (css(cell.fg, fg0), css(cell.bg, bg0));
            if cell.modifier.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let mut style = format!("color:{fg}");
            if bg != bg0 {
                style.push_str(&format!(";background:{bg}"));
            }
            if cell.modifier.contains(Modifier::BOLD) {
                style.push_str(";font-weight:700");
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                style.push_str(";text-decoration:underline;text-underline-offset:3px");
            }
            let sym = cell.symbol().replace('&', "&amp;").replace('<', "&lt;");
            out.push_str(&format!("<span style=\"{style}\">{sym}</span>"));
        }
        out.push('\n');
    }
    out.push_str("</pre></body>");
    out
}

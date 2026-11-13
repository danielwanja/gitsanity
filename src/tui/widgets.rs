//! Small drawing primitives: gradients, bar charts, heat cells, text fitting.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const VIOLET: Color = Color::Rgb(167, 139, 250);
pub const CYAN: Color = Color::Rgb(56, 189, 248);
pub const GREEN: Color = Color::Rgb(74, 222, 128);
pub const RED: Color = Color::Rgb(248, 113, 113);
pub const AMBER: Color = Color::Rgb(251, 191, 36);
pub const PINK: Color = Color::Rgb(244, 114, 182);
pub const DIM: Color = Color::Rgb(125, 133, 144);
pub const FAINT: Color = Color::Rgb(58, 64, 74);
pub const SELECT_BG: Color = Color::Rgb(49, 46, 129);

const STOPS: [(u8, u8, u8); 3] = [(167, 139, 250), (56, 189, 248), (74, 222, 128)];

/// Violet -> cyan -> green, `t` in 0..=1.
pub fn gradient(t: f64) -> Color {
    let t = t.clamp(0.0, 1.0) * (STOPS.len() - 1) as f64;
    let i = (t.floor() as usize).min(STOPS.len() - 2);
    let f = t - i as f64;
    let mix = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * f).round() as u8;
    let (a, b) = (STOPS[i], STOPS[i + 1]);
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// Five-step heat scale; level 0 is "nothing here".
pub fn heat(level: usize) -> Color {
    [
        FAINT,
        Color::Rgb(14, 68, 41),
        Color::Rgb(0, 109, 50),
        Color::Rgb(38, 166, 65),
        Color::Rgb(57, 211, 83),
    ][level.min(4)]
}

/// 0 for zero, otherwise 1..=4 by share of `max` (square-root scaled so a few
/// huge days do not flatten everything else).
pub fn heat_level(value: u64, max: u64) -> usize {
    if value == 0 || max == 0 {
        return 0;
    }
    (((value as f64 / max as f64).sqrt() * 4.0).ceil() as usize).clamp(1, 4)
}

pub fn fmt_n(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1.4M`, `93.9M`, `12.3k`, `842`
pub fn compact(n: u64) -> String {
    match n {
        0..10_000 => fmt_n(n),
        10_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.1}B", n as f64 / 1e9),
    }
}

/// Cut to `width` columns with an ellipsis, keeping the start.
pub fn fit(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Cut to `width` columns keeping the end (file names matter more than roots).
pub fn fit_tail(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_owned();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        tail.push(c);
        used += w;
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

pub fn pad(s: &str, width: usize) -> String {
    let s = fit(s, width);
    let w = s.width();
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

pub fn pad_left(s: &str, width: usize) -> String {
    let w = s.width();
    format!("{}{s}", " ".repeat(width.saturating_sub(w)))
}

const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
const H_EIGHTHS: [&str; 9] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

/// One-row sparkline, resampled to `width` cells.
pub fn sparkline(values: &[u64], width: usize) -> String {
    if values.is_empty() || width == 0 {
        return String::new();
    }
    let cells = resample(values, width);
    let max = cells.iter().copied().max().unwrap_or(0).max(1);
    cells
        .iter()
        .map(|v| {
            if *v == 0 {
                "▁"
            } else {
                EIGHTHS[((*v as f64 / max as f64) * 7.0).round() as usize + 1]
            }
        })
        .collect()
}

/// Sum `values` into exactly `width` cells (or fewer when there are fewer values).
pub fn resample(values: &[u64], width: usize) -> Vec<u64> {
    if values.len() <= width {
        return values.to_vec();
    }
    let mut out = vec![0u64; width];
    for (i, v) in values.iter().enumerate() {
        out[i * width / values.len()] += v;
    }
    out
}

/// A horizontal bar `width` cells wide filled to `value / max`, with eighth precision.
pub fn hbar(value: u64, max: u64, width: usize) -> String {
    if max == 0 || width == 0 {
        return String::new();
    }
    let eighths = ((value as f64 / max as f64) * (width * 8) as f64).round() as usize;
    let eighths = if value > 0 { eighths.max(1) } else { 0 };
    let mut s = "█".repeat(eighths / 8);
    s.push_str(H_EIGHTHS[eighths % 8]);
    s
}

/// Columns `[start, end)` that bar `i` of `n` occupies in a chart `width` wide.
/// Bars always spread over the full width: with room to spare each one keeps a
/// one-column gap to its neighbour, otherwise they sit edge to edge.
pub fn bar_span(i: usize, n: usize, width: usize) -> (usize, usize) {
    let n = n.max(1);
    let (x0, x1) = (i * width / n, (i + 1) * width / n);
    // Same bar width everywhere; only the gaps absorb the rounding.
    if width / n >= 2 {
        (x0, x0 + width / n - 1)
    } else {
        (x0, x1.max(x0 + 1))
    }
}

/// Greedy word wrap to `width` columns.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Vertical bars growing up from the bottom of `area`. Values must already fit
/// (`values.len() <= area.width`). `color(i, t)` gets the bar index and its
/// relative height.
pub fn bars_up(
    buf: &mut Buffer,
    area: Rect,
    values: &[u64],
    max: u64,
    color: impl Fn(usize, f64) -> Color,
) {
    let max = max.max(1);
    for (i, v) in values.iter().enumerate() {
        let (x0, x1) = bar_span(i, values.len(), area.width as usize);
        let t = *v as f64 / max as f64;
        let eighths =
            ((t * f64::from(area.height) * 8.0).round() as usize).max(usize::from(*v > 0));
        let c = color(i, t);
        for row in 0..area.height as usize {
            let fill = eighths.saturating_sub(row * 8).min(8);
            if fill == 0 {
                break;
            }
            let y = area.bottom() - 1 - row as u16;
            // A full cell paints its background too: terminals that add line
            // spacing leave a gap between glyph rows, but never between backgrounds.
            let style = if fill == 8 {
                Style::new().fg(c).bg(c)
            } else {
                Style::new().fg(c)
            };
            for x in x0..x1 {
                let x = area.x + x as u16;
                if x < area.right() {
                    buf.set_string(x, y, EIGHTHS[fill], style);
                }
            }
        }
    }
}

/// Vertical bars hanging down from the top of `area` (drawn as reversed
/// lower blocks, so they stay crisp at eighth precision).
pub fn bars_down(buf: &mut Buffer, area: Rect, values: &[u64], max: u64, color: Color) {
    let max = max.max(1);
    for (i, v) in values.iter().enumerate() {
        let (x0, x1) = bar_span(i, values.len(), area.width as usize);
        let t = *v as f64 / max as f64;
        let eighths =
            ((t * f64::from(area.height) * 8.0).round() as usize).max(usize::from(*v > 0));
        for row in 0..area.height as usize {
            let fill = eighths.saturating_sub(row * 8).min(8);
            if fill == 0 {
                break;
            }
            let y = area.y + row as u16;
            let (glyph, style) = if fill == 8 {
                ("█", Style::new().fg(color).bg(color))
            } else {
                (
                    EIGHTHS[8 - fill],
                    Style::new().fg(color).add_modifier(Modifier::REVERSED),
                )
            };
            for x in x0..x1 {
                let x = area.x + x as u16;
                if x < area.right() {
                    buf.set_string(x, y, glyph, style);
                }
            }
        }
    }
}

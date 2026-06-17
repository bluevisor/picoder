//! Standalone UI helpers: formatting, path utilities, attachment expansion,
//! transcript line rendering, and terminal-width queries.

use super::palette::{self, Palette};
use super::types::{BannerColor, Glyphs, Kind, is_16color_terminal};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub fn perm_name(p: u8) -> &'static str {
    match p {
        crate::agent::PERM_AUTO => "bypass",
        crate::agent::PERM_PLAN => "plan",
        _ => "ask",
    }
}

/// Inset a rect by one column on each side for a small left/right margin.
pub fn pad1(r: Rect) -> Rect {
    Rect { x: r.x + 1, width: r.width.saturating_sub(2), ..r }
}

/// A solid mini progress bar. At normal usage it's the theme `accent`, then
/// escalates to amber → red as context fills, so it both matches the theme and
/// still warns. The empty track is a dimmed shade of the fill's hue, so the bar
/// reads as one element.
pub fn bar(frac: f64, width: usize, accent: Color) -> Vec<Span<'static>> {
    let frac = frac.clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let fill = if frac < 0.8 {
        accent
    } else if frac < 0.95 {
        Color::Rgb(220, 190, 50)
    } else {
        Color::Rgb(220, 70, 70)
    };
    let track = track_color(accent);
    let mut v = Vec::new();
    if filled > 0 {
        v.push(Span::styled(" ".repeat(filled), Style::default().bg(fill)));
    }
    if filled < width {
        v.push(Span::styled(" ".repeat(width - filled), Style::default().bg(track)));
    }
    v
}

/// A dark "slate" tinted toward `accent`'s hue: a neutral floor plus a small
/// proportional tint, so every theme's empty ctx-bar track shares ~the same
/// dark brightness but carries its own color cast.
pub fn track_color(accent: Color) -> Color {
    const FLOOR: f64 = 10.0;
    const TINT: f64 = 20.0;
    let (r, g, b) = palette::color_to_rgb(accent);
    let max = r.max(g).max(b).max(1) as f64;
    let chan = |v: u8| (FLOOR + (v as f64 / max) * TINT).round() as u8;
    Color::Rgb(chan(r), chan(g), chan(b))
}

pub fn humanize(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

pub fn fmt_cost(c: f64) -> String {
    if c < 0.01 {
        format!("${c:.4}")
    } else {
        format!("${c:.2}")
    }
}

fn expand_user(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

/// Strip a `@path` token: drop the `@` and any trailing sentence punctuation
/// so "@calc.py?" resolves to calc.py. Returns None for a bare "@".
fn attach_token(tok: &str) -> Option<&str> {
    let p = tok.strip_prefix('@')?;
    let p = p.trim_end_matches(|c: char| "?.,;:!)]}'\"".contains(c));
    (!p.is_empty()).then_some(p)
}

/// Expand `@path` tokens into appended fenced file contents for the model.
/// Image paths are skipped (handled by `extract_images`). Returns
/// (text_to_send, attached_paths). Display text keeps the raw `@path`.
pub fn expand_attachments(text: &str) -> (String, Vec<String>) {
    let mut attached = Vec::new();
    let mut blocks = String::new();
    for tok in text.split_whitespace() {
        let Some(p) = attach_token(tok) else { continue };
        if crate::tools::is_image_path(p) {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(expand_user(p)) {
            let body = crate::api::truncate(&content, 20000);
            blocks.push_str(&format!("\n\n--- {p} ---\n```\n{body}\n```"));
            attached.push(p.to_string());
        }
    }
    if attached.is_empty() {
        (text.to_string(), attached)
    } else {
        (format!("{text}{blocks}"), attached)
    }
}

/// Resolve `@image.png` tokens to base64 data URIs. Returns (uris, names) so
/// the caller can both attach the images and report what was attached.
pub fn extract_images(text: &str) -> (Vec<String>, Vec<String>) {
    let mut uris = Vec::new();
    let mut names = Vec::new();
    for tok in text.split_whitespace() {
        let Some(p) = attach_token(tok) else { continue };
        if !crate::tools::is_image_path(p) {
            continue;
        }
        if let Ok(uri) = crate::tools::image_data_uri(p) {
            uris.push(uri);
            names.push(p.to_string());
        }
    }
    (uris, names)
}

/// Filesystem completions for a path prefix; dirs get a trailing '/'.
pub fn complete_path(prefix: &str) -> Vec<String> {
    let (dir, base) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("", prefix),
    };
    let search_dir = if dir.is_empty() { expand_user(".") } else { expand_user(dir) };
    let Ok(entries) = std::fs::read_dir(&search_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(base) {
            continue;
        }
        // Hide dotfiles unless the user explicitly typed a leading dot.
        if name.starts_with('.') && !base.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let suffix = if is_dir { "/" } else { "" };
        out.push(format!("{dir}{name}{suffix}"));
    }
    out.sort();
    out
}

pub fn longest_common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut end = first.chars().count();
    for s in &items[1..] {
        let common = first
            .chars()
            .zip(s.chars())
            .take_while(|(a, b)| a == b)
            .count();
        end = end.min(common);
    }
    first.chars().take(end).collect()
}

/// Make a transcript line safe to hand to the terminal. Tool output and model
/// text can carry control characters — a raw ESC (e.g. ANSI colors from a
/// command) starts an escape sequence mid-frame, and a raw tab advances the
/// real cursor further than ratatui's one-cell bookkeeping; both leave stray
/// characters on screen that the diff never cleans up. Tabs become spaces,
/// other control chars are dropped. With `single_width` (ASCII terminals and
/// the framebuffer console, whose font draws every glyph in one cell), chars
/// that aren't single-cell width (emoji, CJK) are replaced with '?' — ratatui
/// books them as two cells, the console draws one, and the diff desyncs.
pub fn clean_text(text: &str, single_width: bool) -> std::borrow::Cow<'_, str> {
    use unicode_width::UnicodeWidthChar;
    let dirty = |c: char| c.is_control() || (single_width && c.width() != Some(1));
    if !text.chars().any(dirty) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\t' {
            out.push_str("    ");
        } else if c.is_control() {
            // drop
        } else if single_width && c.width() != Some(1) {
            out.push('?');
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

#[allow(clippy::too_many_arguments)]
pub fn render_tline(
    out: &mut Vec<Line<'static>>,
    kind: Kind,
    text: &str,
    lead: bool,
    color: Option<BannerColor>,
    width: usize,
    g: Glyphs,
    p: &Palette,
    single_width: bool,
) {
    let text = &*clean_text(text, single_width);
    let (indent, glyph) = layout(kind, g);
    let (mut base, mut glyph_style) = palette::colors(kind, p);
    if let Some(bc) = color {
        let rainbow = if is_16color_terminal() { palette::APPLE_RAINBOW_16 } else { palette::APPLE_RAINBOW };
        let c = match bc {
            BannerColor::Fixed(c) => c,
            BannerColor::Rainbow(i) => palette::banner_row_color(p, &rainbow, i),
            BannerColor::Accent => p.accent,
        };
        base = base.fg(c);
    }
    // Highlight the user's own prompts with a full-width gray band so they
    // stand out when scrolling back through output.
    let bg = if kind == Kind::User {
        Some(p.user_bg)
    } else {
        None
    };
    if let Some(c) = bg {
        base = base.bg(c);
        glyph_style = glyph_style.bg(c);
    }
    let pad_style = bg.map(|c| Style::default().bg(c)).unwrap_or_default();

    let prefix_w = indent + glyph.chars().count();
    let wrap_w = width.saturating_sub(prefix_w).max(1);
    let no_wrap = matches!(kind, Kind::Banner | Kind::BannerDim);
    let wrapped: Vec<String> = if text.is_empty() {
        vec![String::new()]
    } else if no_wrap {
        // Block art must never word-wrap; truncate to the visible width.
        vec![text.chars().take(wrap_w).collect()]
    } else {
        textwrap::wrap(text, wrap_w).into_iter().map(|c| c.into_owned()).collect()
    };
    for (j, piece) in wrapped.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if j == 0 && lead {
            if indent > 0 {
                spans.push(Span::styled(" ".repeat(indent), pad_style));
            }
            if !glyph.is_empty() {
                spans.push(Span::styled(glyph.to_string(), glyph_style));
            }
        } else {
            spans.push(Span::styled(" ".repeat(prefix_w), pad_style));
        }
        let used = prefix_w + piece.chars().count();
        spans.push(Span::styled(piece, base));
        // Extend the gray band to the full width.
        if bg.is_some() && used < width {
            spans.push(Span::styled(" ".repeat(width - used), pad_style));
        }
        out.push(Line::from(spans));
    }
}

/// Indentation and leading glyph per line kind (theme-independent).
pub fn layout(kind: Kind, g: Glyphs) -> (usize, &'static str) {
    match kind {
        Kind::User => (0, g.user),
        Kind::Assistant => (0, g.assistant),
        Kind::Reasoning => (2, ""),
        Kind::Tool => (2, g.tool),
        Kind::ToolResult | Kind::ToolErr => (4, g.result),
        Kind::DiffAdd | Kind::DiffDel | Kind::DiffCtx => (4, ""),
        Kind::Notice => (2, ""),
        Kind::ErrorK => (0, g.error),
        Kind::Code => (2, ""),
        Kind::Heading => (0, g.assistant),
        Kind::Banner | Kind::BannerDim => (0, ""),
    }
}

/// Current terminal width in columns (for sizing the launch banner).
pub fn term_width() -> u16 {
    ratatui::crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80)
}

pub fn setting_max_tool_calls(n: u32) -> String {
    if n == 0 { "auto".into() } else { n.to_string() }
}

pub fn parse_max_tool_calls(s: &str) -> u32 {
    s.parse().unwrap_or(0)
}

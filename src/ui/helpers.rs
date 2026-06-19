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

/// Render a full assistant message with lightweight Markdown formatting.
///
/// The message may span several `\n`-separated lines. We split here (rather
/// than collapsing in `clean_text`, which strips the newlines) so paragraph
/// breaks, headings, and lists survive, and so fenced code blocks — which need
/// to know where they start and end — can be tracked across lines. Per line we
/// apply: ATX headings (`# …`), bullet lists (`-`/`*`/`+`), inline code
/// (`` `…` ``), bold (`**…**`), and italic (`*…*`). Fenced blocks (```` ``` ````)
/// render verbatim in the code color with no inline parsing.
#[allow(clippy::too_many_arguments)]
pub fn render_message(
    out: &mut Vec<Line<'static>>,
    text: &str,
    lead: bool,
    width: usize,
    g: Glyphs,
    p: &Palette,
    single_width: bool,
) {
    let mut in_code = false;
    let mut lead_left = lead;
    for raw in text.split('\n') {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            continue; // the fence marker itself isn't shown
        }
        if in_code {
            render_code_line(out, raw, lead_left, width, g, p, single_width);
        } else {
            render_md_line(out, raw, lead_left, width, g, p, single_width);
        }
        lead_left = false;
    }
}

/// The bullet marker for list items, matched to the glyph set.
fn bullet_glyph(g: Glyphs) -> &'static str {
    if g.rounded { "•" } else { "-" }
}

/// If `line` (already left-trimmed) is an ATX heading (`#`..`######` then a
/// space), return the heading text with the markers stripped.
fn heading_body(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
        Some(line[hashes + 1..].trim_start())
    } else {
        None
    }
}

/// If `line` is a bullet list item (`-`/`*`/`+` then whitespace), return its
/// leading indent and the item text after the marker.
fn bullet_body(line: &str) -> Option<(&str, &str)> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let mut chars = rest.chars();
    match chars.next() {
        Some('-') | Some('*') | Some('+') => {}
        _ => return None,
    }
    match chars.next() {
        Some(' ') | Some('\t') => Some((indent, rest[2..].trim_start())),
        _ => None,
    }
}

/// Render one Markdown line (no fence): detect a heading/bullet prefix, parse
/// inline emphasis, then word-wrap the styled runs under the assistant glyph.
fn render_md_line(
    out: &mut Vec<Line<'static>>,
    text: &str,
    lead: bool,
    width: usize,
    g: Glyphs,
    p: &Palette,
    single_width: bool,
) {
    let text = &*clean_text(text, single_width);
    let (indent, glyph) = layout(Kind::Assistant, g);
    let prefix_w = indent + glyph.chars().count();
    let wrap_w = width.saturating_sub(prefix_w).max(1);
    let (mut base, _) = palette::colors(Kind::Assistant, p);
    let code_style = Style::default().fg(p.code);

    let mut segs: Vec<(String, Style)> = Vec::new();
    let body: String = if let Some(rest) = heading_body(text.trim_start()) {
        base = Style::default().fg(p.heading).add_modifier(Modifier::BOLD);
        rest.to_string()
    } else if let Some((lead_ws, rest)) = bullet_body(text) {
        segs.push((format!("{lead_ws}{} ", bullet_glyph(g)), base));
        rest.to_string()
    } else {
        text.to_string()
    };
    segs.extend(inline_segments(&body, base, code_style));
    if segs.is_empty() {
        segs.push((String::new(), base));
    }
    let lines = wrap_segments(segs, wrap_w);
    emit_lines(out, lead, width, g, p, lines);
}

/// Render a verbatim line inside a fenced code block: code color, no inline
/// parsing, wrapped (not truncated) so nothing is lost.
fn render_code_line(
    out: &mut Vec<Line<'static>>,
    text: &str,
    lead: bool,
    width: usize,
    g: Glyphs,
    p: &Palette,
    single_width: bool,
) {
    let text = &*clean_text(text, single_width);
    let (indent, glyph) = layout(Kind::Assistant, g);
    let wrap_w = width.saturating_sub(indent + glyph.chars().count()).max(1);
    let lines = wrap_segments(vec![(text.to_string(), Style::default().fg(p.code))], wrap_w);
    emit_lines(out, lead, width, g, p, lines);
}

/// Parse inline Markdown into styled runs: `` `code` ``, `**bold**`, `*italic*`.
/// Markers are only treated as emphasis when they enclose non-blank text on the
/// same line (so `2 * 3` and bare backticks stay literal); underscores are left
/// alone to avoid mangling snake_case identifiers.
fn inline_segments(text: &str, base: Style, code_style: Style) -> Vec<(String, Style)> {
    let cs: Vec<char> = text.chars().collect();
    let n = cs.len();
    let mut segs: Vec<(String, Style)> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    let flush = |buf: &mut String, segs: &mut Vec<(String, Style)>| {
        if !buf.is_empty() {
            segs.push((std::mem::take(buf), base));
        }
    };
    while i < n {
        let c = cs[i];
        // Inline code spans the next backtick; everything inside is literal.
        if c == '`' {
            if let Some(close) = (i + 1..n).find(|&j| cs[j] == '`') {
                if close > i + 1 {
                    flush(&mut buf, &mut segs);
                    segs.push((cs[i + 1..close].iter().collect(), code_style));
                    i = close + 1;
                    continue;
                }
            }
        }
        // Bold (`**`) or italic (`*`): require a non-space just inside each end.
        if c == '*' {
            let double = i + 1 < n && cs[i + 1] == '*';
            let mlen = if double { 2 } else { 1 };
            let start = i + mlen;
            if start < n && cs[start] != ' ' {
                let mut j = start;
                let close = loop {
                    if j >= n {
                        break None;
                    }
                    if double {
                        if j + 1 < n && cs[j] == '*' && cs[j + 1] == '*' {
                            break Some(j);
                        }
                    } else if cs[j] == '*' {
                        break Some(j);
                    }
                    j += 1;
                };
                if let Some(close) = close {
                    if close > start && cs[close - 1] != ' ' {
                        flush(&mut buf, &mut segs);
                        let modi = if double { Modifier::BOLD } else { Modifier::ITALIC };
                        segs.push((cs[start..close].iter().collect(), base.add_modifier(modi)));
                        i = close + mlen;
                        continue;
                    }
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut segs);
    segs
}

/// Greedy word-wrap a sequence of styled runs to `wrap_w` display columns,
/// preserving each run's style. Whitespace is collapsed at line breaks; a single
/// word wider than the line is hard-split.
fn wrap_segments(segs: Vec<(String, Style)>, wrap_w: usize) -> Vec<Vec<(String, Style)>> {
    use unicode_width::UnicodeWidthStr;
    // Split each run into whitespace / non-whitespace tokens, keeping its style.
    let mut toks: Vec<(String, Style, bool)> = Vec::new();
    for (text, st) in &segs {
        let mut cur = String::new();
        let mut cur_space: Option<bool> = None;
        for ch in text.chars() {
            let sp = ch == ' ';
            if cur_space == Some(sp) {
                cur.push(ch);
            } else {
                if !cur.is_empty() {
                    toks.push((std::mem::take(&mut cur), *st, cur_space.unwrap()));
                }
                cur.push(ch);
                cur_space = Some(sp);
            }
        }
        if !cur.is_empty() {
            toks.push((cur, *st, cur_space.unwrap()));
        }
    }

    let mut lines: Vec<Vec<(String, Style)>> = Vec::new();
    let mut line: Vec<(String, Style)> = Vec::new();
    let mut w = 0usize;
    let push_word = |line: &mut Vec<(String, Style)>, w: &mut usize, lines: &mut Vec<Vec<(String, Style)>>, text: String, st: Style| {
        let tw = text.width();
        if *w + tw > wrap_w && *w > 0 {
            lines.push(std::mem::take(line));
            *w = 0;
        }
        line.push((text, st));
        *w += tw;
    };
    for (text, st, is_space) in toks {
        if is_space {
            if w == 0 {
                continue; // never start a wrapped line with leading space
            }
            let tw = text.width();
            if w + tw > wrap_w {
                lines.push(std::mem::take(&mut line));
                w = 0;
            } else {
                line.push((text, st));
                w += tw;
            }
        } else if text.width() > wrap_w {
            if w > 0 {
                lines.push(std::mem::take(&mut line));
                w = 0;
            }
            for chunk in hard_split(&text, wrap_w) {
                push_word(&mut line, &mut w, &mut lines, chunk, st);
            }
        } else {
            push_word(&mut line, &mut w, &mut lines, text, st);
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Split a single over-long word into chunks no wider than `w` display columns.
fn hard_split(word: &str, w: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cw = 0usize;
    for ch in word.chars() {
        let chw = ch.width().unwrap_or(0);
        if cw + chw > w && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cw = 0;
        }
        cur.push(ch);
        cw += chw;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Emit wrapped, pre-styled lines under the assistant glyph (glyph on the first
/// line when `lead`, alignment padding on the rest).
fn emit_lines(
    out: &mut Vec<Line<'static>>,
    lead: bool,
    width: usize,
    g: Glyphs,
    p: &Palette,
    lines: Vec<Vec<(String, Style)>>,
) {
    let (indent, glyph) = layout(Kind::Assistant, g);
    let (_, glyph_style) = palette::colors(Kind::Assistant, p);
    let prefix_w = indent + glyph.chars().count();
    let _ = width;
    for (j, segs) in lines.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if j == 0 && lead {
            if indent > 0 {
                spans.push(Span::raw(" ".repeat(indent)));
            }
            if !glyph.is_empty() {
                spans.push(Span::styled(glyph.to_string(), glyph_style));
            }
        } else {
            spans.push(Span::raw(" ".repeat(prefix_w)));
        }
        for (t, st) in segs {
            spans.push(Span::styled(t, st));
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

#[cfg(test)]
mod tests {
    use super::*;

    // Plain (text, modifier-or-fg) view of inline parsing, ignoring the base style.
    fn parse(text: &str) -> Vec<(String, Style)> {
        let base = Style::default();
        let code = Style::default().fg(Color::Cyan);
        inline_segments(text, base, code)
    }

    #[test]
    fn inline_bold_italic_code() {
        let segs = parse("a **b** c *d* `e`");
        let texts: Vec<&str> = segs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, vec!["a ", "b", " c ", "d", " ", "e"]);
        assert!(segs[1].1.add_modifier.contains(Modifier::BOLD));
        assert!(segs[3].1.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(segs[5].1.fg, Some(Color::Cyan));
    }

    #[test]
    fn inline_leaves_literals_and_identifiers_alone() {
        // Lone/spaced asterisks and snake_case underscores are not emphasis.
        assert_eq!(parse("2 * 3").len(), 1);
        assert_eq!(parse("a * b * c").len(), 1);
        assert_eq!(parse("snake_case_name").len(), 1);
        // Unterminated markers stay literal.
        let segs = parse("`unterminated and **also");
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn heading_and_bullet_detection() {
        assert_eq!(heading_body("## Title"), Some("Title"));
        assert_eq!(heading_body("####### too many"), None);
        assert_eq!(heading_body("#nospace"), None);
        assert_eq!(bullet_body("- item"), Some(("", "item")));
        assert_eq!(bullet_body("  * nested"), Some(("  ", "nested")));
        assert_eq!(bullet_body("*emphasis*"), None);
    }

    #[test]
    fn wrap_preserves_styles_and_width() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let segs = vec![("hello ".to_string(), Style::default()), ("world".to_string(), bold)];
        let lines = wrap_segments(segs, 8);
        assert_eq!(lines.len(), 2);
        // The bold run keeps its modifier after wrapping.
        let last = lines.last().unwrap().last().unwrap();
        assert_eq!(last.0, "world");
        assert!(last.1.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn wrap_hard_splits_overlong_word() {
        let segs = vec![("abcdefghij".to_string(), Style::default())];
        let lines = wrap_segments(segs, 4);
        let joined: Vec<String> = lines.iter().map(|l| l.iter().map(|(t, _)| t.as_str()).collect()).collect();
        assert_eq!(joined, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn message_splits_lines_and_hides_code_fence() {
        let p = palette::palette_by_name("Default");
        let mut out = Vec::new();
        render_message(&mut out, "para one\n\n```\ncode\n```\ndone", true, 40, crate::ui::types::GLYPHS_A, &p, true);
        // "para one" + blank + "code" + "done" = 4; the ``` fences are dropped.
        assert_eq!(out.len(), 4);
    }
}

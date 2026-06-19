//! Color palette and theme definitions. Each theme is a `Palette` struct;
//! `palette_by_name` resolves a name to a palette, and `is_theme_name` checks
//! whether a string names a known theme.

use super::types::{CursorKind, Kind};
use ratatui::style::{Color, Modifier, Style};

/// Apple-logo rainbow (top→bottom): green, yellow, orange, red, purple, blue.
pub const APPLE_RAINBOW: [Color; 6] = [
    Color::Rgb(97, 187, 70),
    Color::Rgb(253, 184, 39),
    Color::Rgb(245, 130, 31),
    Color::Rgb(224, 58, 62),
    Color::Rgb(150, 61, 151),
    Color::Rgb(0, 157, 220),
];
/// Per-row brightness factors that turn a theme's banner color into a shaded
/// top→bottom gradient, so the logo reads with depth (darkest row = the drop
/// shadow). Each non-default theme thus gets a distinct shaded ramp of its own
/// color; the default theme uses the Apple rainbow instead.
pub const BANNER_SHADE: [f32; 6] = [1.0, 0.86, 0.72, 0.60, 0.49, 0.40];
/// 16-color approximation for the framebuffer console.
pub const APPLE_RAINBOW_16: [Color; 6] = [
    Color::Green,
    Color::Yellow,
    Color::LightRed,
    Color::Red,
    Color::Magenta,
    Color::Blue,
];

/// A color theme. `mono_banner = Some(c)` draws the logo in one color instead
/// of the Apple rainbow.
#[derive(Clone, Copy)]
pub struct Palette {
    pub name: &'static str,
    pub accent: Color,
    pub assistant: Color,
    pub assistant_glyph: Color,
    pub reasoning: Color,
    pub tool: Color,
    pub tool_result: Color,
    pub notice: Color,
    pub code: Color,
    pub heading: Color,
    pub diff_add: Color,
    pub diff_del: Color,
    pub diff_ctx: Color,
    pub error: Color,
    pub mono_banner: Option<Color>,
    /// UI chrome: borders, rules, separators.
    pub chrome: Color,
    /// Secondary / dimmed text (hints, status values, picker dim).
    pub secondary: Color,
    /// Background for user messages (a subtle band).
    pub user_bg: Color,
    /// App background. `Color::Reset` inherits the terminal's own background;
    /// themed palettes paint their own near-black so the whole TUI matches.
    pub bg: Color,
    /// Theme-specific composer prompt (else the default glyph set's prompt).
    pub prompt: Option<&'static str>,
    pub cursor: CursorKind,
}

pub const DEFAULT_PALETTE: Palette = Palette {
    name: "Default",
    accent: Color::Cyan,
    assistant: Color::Rgb(242, 242, 247),        // systemWhite (slightly warm)
    assistant_glyph: Color::Green,
    reasoning: Color::Rgb(140, 140, 150),          // dim gray, readable on dark bg
    tool: Color::Blue,
    tool_result: Color::Rgb(150, 150, 150),
    notice: Color::Rgb(150, 150, 150),
    code: Color::Yellow,
    heading: Color::Rgb(242, 242, 247),           // near white
    diff_add: Color::Green,
    diff_del: Color::Red,
    diff_ctx: Color::DarkGray,
    error: Color::Red,
    mono_banner: None,
    chrome: Color::DarkGray,
    secondary: Color::Rgb(140, 140, 140),
    user_bg: Color::Rgb(48, 48, 48),
    bg: Color::Reset,                             // inherit the terminal background
    prompt: None,
    cursor: CursorKind::Reverse,
};

// Apple ][ — authentic green-phosphor CRT (P31, Monitor II / Monitor ///).
pub const APPLE2_GREEN: Color = Color::Rgb(51, 255, 51);
pub const APPLE2_PALETTE: Palette = Palette {
    name: "Apple ][",
    accent: APPLE2_GREEN,
    assistant: APPLE2_GREEN,
    assistant_glyph: Color::Rgb(100, 255, 100),
    reasoning: Color::Rgb(0, 130, 0),
    tool: APPLE2_GREEN,
    tool_result: Color::Rgb(0, 160, 0),
    notice: Color::Rgb(0, 155, 0),
    code: Color::Rgb(140, 255, 140),
    heading: APPLE2_GREEN,
    diff_add: Color::Rgb(110, 255, 110),
    diff_del: Color::Rgb(0, 100, 0),
    diff_ctx: Color::Rgb(0, 70, 0),
    error: Color::Rgb(200, 255, 200),
    mono_banner: Some(APPLE2_GREEN),
    chrome: Color::Rgb(0, 120, 0),
    secondary: Color::Rgb(0, 155, 0),
    user_bg: Color::Rgb(0, 30, 0),
    bg: Color::Rgb(0, 12, 0),
    prompt: Some("] "),
    cursor: CursorKind::Block,
};

// MS-DOS — light-gray text on black, C:\> prompt.
pub const MSDOS_PALETTE: Palette = Palette {
    name: "MSDOS",
    accent: Color::White,
    assistant: Color::Gray,
    assistant_glyph: Color::White,
    reasoning: Color::DarkGray,
    tool: Color::Cyan,
    tool_result: Color::Gray,
    notice: Color::Gray,
    code: Color::LightGreen,
    heading: Color::Rgb(242, 242, 247),
    diff_add: Color::Green,
    diff_del: Color::Red,
    diff_ctx: Color::DarkGray,
    error: Color::LightRed,
    mono_banner: Some(Color::White),
    chrome: Color::DarkGray,
    secondary: Color::Gray,
    user_bg: Color::DarkGray,
    bg: Color::Rgb(8, 8, 10),
    prompt: Some("C:\\> "),
    cursor: CursorKind::Block,
};

// macOS — macOS Terminal.app dark mode running fish.
pub const MACOS_BLUE: Color = Color::Rgb(111, 157, 196);
pub const MACOS_GREEN: Color = Color::Rgb(126, 190, 106);
pub const MACOS_FG: Color = Color::Rgb(216, 216, 216);
pub const MACOS_PALETTE: Palette = Palette {
    name: "macOS",
    accent: MACOS_BLUE,
    assistant: MACOS_FG,
    assistant_glyph: MACOS_GREEN,
    reasoning: Color::Rgb(142, 142, 147),
    tool: MACOS_BLUE,
    tool_result: Color::Rgb(174, 174, 178),
    notice: Color::Rgb(152, 152, 157),
    code: MACOS_GREEN,
    heading: MACOS_FG,
    diff_add: MACOS_GREEN,
    diff_del: Color::Rgb(255, 95, 86),
    diff_ctx: Color::Rgb(99, 99, 102),
    error: Color::Rgb(255, 95, 86),
    mono_banner: Some(MACOS_BLUE),
    chrome: Color::Rgb(72, 72, 74),
    secondary: Color::Rgb(152, 152, 157),
    user_bg: Color::Rgb(38, 38, 38),
    bg: Color::Rgb(29, 29, 29),
    prompt: Some("~ "),
    cursor: CursorKind::Reverse,
};

// ── Sun Microsystems ───────────────────────────────────────────────
pub const SUN_PALETTE: Palette = Palette {
    name: "SUN",
    accent: Color::Rgb(255, 183, 0),
    assistant: Color::Rgb(238, 232, 213),
    assistant_glyph: Color::Rgb(255, 183, 0),
    reasoning: Color::Rgb(139, 119, 80),
    tool: Color::Rgb(186, 85, 211),
    tool_result: Color::Rgb(160, 140, 100),
    notice: Color::Rgb(160, 140, 100),
    code: Color::Rgb(255, 200, 80),
    heading: Color::Rgb(238, 232, 213),
    diff_add: Color::Rgb(100, 200, 100),
    diff_del: Color::Rgb(220, 100, 60),
    diff_ctx: Color::Rgb(100, 80, 50),
    error: Color::Rgb(255, 120, 70),
    mono_banner: Some(Color::Rgb(255, 183, 0)),
    chrome: Color::Rgb(100, 80, 50),
    secondary: Color::Rgb(160, 140, 100),
    user_bg: Color::Rgb(40, 30, 15),
    bg: Color::Rgb(22, 16, 8),
    prompt: Some("sun% "),
    cursor: CursorKind::Block,
};

// ── NeXT ────────────────────────────────────────────────────────────
pub const NEXTS_PALETTE: Palette = Palette {
    name: "NeXT",
    accent: Color::White,
    assistant: Color::Rgb(220, 220, 220),
    assistant_glyph: Color::White,
    reasoning: Color::Rgb(100, 100, 100),
    tool: Color::Rgb(180, 180, 180),
    tool_result: Color::Rgb(130, 130, 130),
    notice: Color::Rgb(130, 130, 130),
    code: Color::Rgb(200, 200, 200),
    heading: Color::White,
    diff_add: Color::Rgb(180, 180, 180),
    diff_del: Color::Rgb(80, 80, 80),
    diff_ctx: Color::Rgb(60, 60, 60),
    error: Color::Rgb(255, 255, 255),
    mono_banner: Some(Color::White),
    chrome: Color::Rgb(80, 80, 80),
    secondary: Color::Rgb(120, 120, 120),
    user_bg: Color::Rgb(40, 40, 40),
    bg: Color::Rgb(18, 18, 20),
    prompt: Some("NeXT> "),
    cursor: CursorKind::Reverse,
};

// ── SGI (Silicon Graphics) ─────────────────────────────────────────
pub const SGI_PALETTE: Palette = Palette {
    name: "SGI",
    accent: Color::Rgb(0, 191, 165),
    assistant: Color::Rgb(224, 240, 240),
    assistant_glyph: Color::Rgb(0, 191, 165),
    reasoning: Color::Rgb(80, 100, 120),
    tool: Color::Rgb(100, 140, 220),
    tool_result: Color::Rgb(100, 130, 145),
    notice: Color::Rgb(100, 130, 145),
    code: Color::Rgb(120, 220, 200),
    heading: Color::Rgb(224, 240, 240),
    diff_add: Color::Rgb(60, 200, 140),
    diff_del: Color::Rgb(200, 80, 100),
    diff_ctx: Color::Rgb(50, 70, 90),
    error: Color::Rgb(255, 100, 120),
    mono_banner: Some(Color::Rgb(0, 191, 165)),
    chrome: Color::Rgb(50, 70, 90),
    secondary: Color::Rgb(100, 130, 145),
    user_bg: Color::Rgb(20, 30, 50),
    bg: Color::Rgb(12, 16, 26),
    prompt: Some("irix# "),
    cursor: CursorKind::Block,
};

pub const THEMES: &[&str] = &["Default", "Apple ][", "MSDOS", "macOS", "SUN", "NeXT", "SGI"];

pub fn palette_by_name(name: &str) -> Palette {
    match name {
        "Apple ][" | "apple2" | "apple][" | "appleii" | "apple2e" => APPLE2_PALETTE,
        "MSDOS" | "msdos" | "dos" => MSDOS_PALETTE,
        "macOS" | "macos" | "macintosh" | "mac" => MACOS_PALETTE,
        "SUN" | "sun" | "solaris" | "sunos" => SUN_PALETTE,
        "NeXT" | "next" | "nextstep" => NEXTS_PALETTE,
        "SGI" | "sgi" | "irix" | "indigo" => SGI_PALETTE,
        _ => DEFAULT_PALETTE,
    }
}

/// Whether `name` is a known theme name (used by the CLI to avoid consuming
/// the next argument after --banner as a theme if it's a task description).
pub fn is_theme_name(name: &str) -> bool {
    matches!(name, "Default" | "default" | "Apple ][" | "apple2" | "apple][" | "appleii" | "apple2e" | "MSDOS" | "msdos" | "dos" | "macOS" | "macos" | "macintosh" | "mac" | "SUN" | "sun" | "solaris" | "sunos" | "NeXT" | "next" | "nextstep" | "SGI" | "sgi" | "irix" | "indigo")
}

pub fn ansi_fg(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m"),
        Color::Green => "\x1b[32m".into(),
        Color::Yellow => "\x1b[33m".into(),
        Color::LightRed => "\x1b[91m".into(),
        Color::LightGreen => "\x1b[92m".into(),
        Color::Red => "\x1b[31m".into(),
        Color::Magenta => "\x1b[35m".into(),
        Color::Blue => "\x1b[34m".into(),
        Color::Cyan => "\x1b[36m".into(),
        Color::White => "\x1b[97m".into(),
        Color::Gray => "\x1b[37m".into(),
        Color::DarkGray => "\x1b[90m".into(),
        Color::Black => "\x1b[30m".into(),
        _ => "\x1b[39m".into(),
    }
}

/// Best-effort RGB for a `Color`, so we can shade named theme colors too.
pub fn color_to_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::White => (255, 255, 255),
        Color::Gray => (190, 190, 190),
        Color::DarkGray => (110, 110, 110),
        Color::Green => (0, 200, 0),
        Color::LightGreen => (120, 255, 120),
        Color::Yellow => (220, 220, 0),
        Color::Red => (220, 0, 0),
        Color::LightRed => (255, 90, 90),
        Color::Blue => (0, 90, 220),
        Color::Cyan => (0, 200, 200),
        Color::Magenta => (200, 0, 200),
        _ => (220, 220, 220),
    }
}

/// Darken `c` by the row-`i` shade factor for the banner gradient.
pub fn shade(c: Color, i: usize) -> Color {
    let (r, g, b) = color_to_rgb(c);
    let f = BANNER_SHADE[i.min(BANNER_SHADE.len() - 1)];
    let s = |v: u8| (v as f32 * f).round().clamp(0.0, 255.0) as u8;
    Color::Rgb(s(r), s(g), s(b))
}

/// Color for art row `i`: the Apple rainbow for the default theme, or a shaded
/// top→bottom gradient of the theme's own banner color for any non-default
/// theme. `i` may exceed the ramp on the ASCII shadow row, so it's clamped.
pub fn banner_row_color(p: &Palette, rainbow: &[Color; 6], i: usize) -> Color {
    match p.mono_banner {
        Some(c) => shade(c, i),
        None => rainbow[i % rainbow.len()],
    }
}

/// (text style, glyph style) per line kind, from the active theme palette.
pub fn colors(kind: Kind, p: &Palette) -> (Style, Style) {
    let s = |c: Color| Style::default().fg(c);
    let bold = |c: Color| Style::default().fg(c).add_modifier(Modifier::BOLD);
    match kind {
        Kind::User => (bold(p.accent), bold(p.accent)),
        Kind::Assistant => (s(p.assistant), s(p.assistant_glyph)),
        Kind::Reasoning => (s(p.reasoning), s(p.reasoning)),
        Kind::Tool => (s(p.tool), s(p.tool)),
        Kind::ToolResult => (s(p.tool_result), s(p.reasoning)),
        Kind::ToolErr => (s(p.error), s(p.error)),
        Kind::DiffAdd => (s(p.diff_add), Style::default()),
        Kind::DiffDel => (s(p.diff_del), Style::default()),
        Kind::DiffCtx => (s(p.diff_ctx), s(p.diff_ctx)),
        Kind::Notice => (s(p.notice), s(p.notice)),
        Kind::ErrorK => (s(p.error), s(p.error)),
        Kind::Code => (s(p.code), s(p.code)),
        Kind::Heading => (bold(p.heading), s(p.assistant_glyph)),
        Kind::Banner => (bold(p.accent), s(p.accent)),
        Kind::BannerDim => (s(p.notice), Style::default()),
    }
}

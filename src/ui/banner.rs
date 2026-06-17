//! Banner art: the PICODER logo rendered as block art (Unicode or ASCII),
//! assembled into lines with roles so the TUI and `--banner` flag can
//! render the same layout identically.

use super::palette::{self};
use super::types::{BannerColor, Kind, TLine};

pub const TAGLINE: &str = "a tiny agentic coding CLI";

/// 6x4 block glyphs (P I C O D E R), assembled at runtime so the heavy and
/// ASCII logos stay aligned and adding a letter is a one-line change.
const ART_GLYPHS: [[&str; 6]; 7] = [
    ["####", "#  #", "####", "#   ", "#   ", "#   "], // P
    ["####", " ## ", " ## ", " ## ", " ## ", "####"], // I
    ["####", "#   ", "#   ", "#   ", "#   ", "####"], // C
    ["####", "#  #", "#  #", "#  #", "#  #", "####"], // O
    ["### ", "#  #", "#  #", "#  #", "#  #", "### "], // D
    ["####", "#   ", "### ", "#   ", "#   ", "####"], // E
    ["####", "#  #", "####", "# # ", "#  #", "#  #"], // R
];

/// Heavy Unicode "PICODER" with a diagonal `░` drop-shadow, for wide terminals.
/// Hand-tuned: the shadow can't be derived by simply doubling `ART_GLYPHS`.
const ART_UNICODE: [&str; 6] = [
    "██████  ██████  ██████  ██████  ████    ██████  ██████",
    "██░░██░░  ██░░░░██░░░░░░██░░██░░██░░██  ██░░░░░░██░░██░░",
    "██████░░  ██░░  ██░░    ██░░██░░██░░██░░████    ██████░░",
    "██░░░░░░  ██░░  ██░░    ██░░██░░██░░██░░██░░░░  ████░░░░",
    "██░░    ██████  ██████  ██████░░████  ░░██████  ██░░██",
    "  ░░      ░░░░░░  ░░░░░░  ░░░░░░  ░░░░    ░░░░░░  ░░  ░░",
];

/// Render the glyph table at a row, mapping `#`→`fill`. `double` widens each
/// cell to two columns for the heavy Unicode logo.
fn art_glyphs(fill: char, double: bool) -> Vec<String> {
    (0..6)
        .map(|r| {
            ART_GLYPHS
                .iter()
                .map(|g| {
                    g[r]
                        .chars()
                        .map(|c| {
                            let cell = if c == '#' { fill } else { ' ' };
                            if double { format!("{cell}{cell}") } else { cell.to_string() }
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join(if double { "  " } else { " " })
        })
        .collect()
}

/// Generate a drop-shadow row for the ASCII art, matching the bottom row
/// shifted right by one column. Uses `.` as the shadow character.
fn ascii_shadow_row() -> String {
    let fill = '#';
    let bottom: String = ART_GLYPHS
        .iter()
        .map(|g| {
            g[5]
                .chars()
                .map(|c| if c == '#' { fill } else { ' ' })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<char> = bottom.chars().collect();
    let mut shadow = vec![' '; chars.len()];
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i] == '#' {
            shadow[i + 1] = '.';
        }
    }
    shadow.into_iter().collect()
}

/// Pick the widest PICODER art that fits in `w` columns.
fn banner_art(w: usize, ascii: bool) -> Vec<String> {
    if !ascii && w >= 56 {
        // Unicode art already includes its shadow row (row 5).
        return ART_UNICODE.iter().map(|s| s.to_string()).collect();
    } else if w >= 34 {
        let mut art = art_glyphs(if ascii { '#' } else { '█' }, false);
        // ASCII gets a drop-shadow row; Unicode non-heavy gets none (too narrow).
        if ascii {
            art.push(ascii_shadow_row());
        }
        return art;
    } else {
        vec!["P I C O D E R".to_string()]
    }
}

/// Role of a banner line, so the TUI and the ANSI `--banner` preview color the
/// same layout identically.
#[derive(Clone, Copy)]
pub enum BRole {
    Art(usize), // rainbow/mono block-art row
    Version,    // bold accent
    Tagline,    // dim
    Frame,      // dim panel rule / blank
    Data,       // accent status line
}

pub struct BLine {
    pub text: String,
    pub role: BRole,
}

/// Build the launch banner as structured lines: centered block art, a version
/// + tagline, and a bordered SYSTEM panel wrapping the status lines.
pub fn banner_lines(w: usize, ascii: bool, status: &[String]) -> Vec<BLine> {
    let center = |s: &str| {
        let pad = " ".repeat(w.saturating_sub(s.chars().count()) / 2);
        format!("{pad}{s}")
    };
    let art = banner_art(w, ascii);
    let artw = art.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let art_pad = " ".repeat(w.saturating_sub(artw) / 2);

    let mut out = Vec::new();
    // Breathing room between the box title (working path) and the logo.
    out.push(BLine { text: String::new(), role: BRole::Frame });
    for (i, l) in art.iter().enumerate() {
        out.push(BLine { text: format!("{art_pad}{l}"), role: BRole::Art(i) });
    }
    out.push(BLine { text: String::new(), role: BRole::Frame });
    out.push(BLine { text: center(&format!("picoder v{}", env!("CARGO_PKG_VERSION"))), role: BRole::Version });
    out.push(BLine { text: center(TAGLINE), role: BRole::Tagline });
    out.push(BLine { text: String::new(), role: BRole::Frame });

    let (tl, tr, bl, br, h, vbar) = if ascii {
        ("+", "+", "+", "+", "-", "|")
    } else {
        ("┌", "┐", "└", "┘", "─", "│")
    };
    // Top border: ┌─ SYSTEM ───...──┐
    let head = format!("{tl}{h} SYSTEM ");
    let fill = w.saturating_sub(head.chars().count() + 1); // +1 for right corner
    out.push(BLine { text: format!("{head}{}{tr}", h.repeat(fill)), role: BRole::Frame });
    // Status lines: │ text ... │
    for line in status {
        let pad = w.saturating_sub(line.chars().count() + 3); // 3 = "│ " + "│"
        out.push(BLine { text: format!("{vbar} {line}{}{vbar}", " ".repeat(pad)), role: BRole::Data });
    }
    // Bottom border: └───...──┘
    let bfill = w.saturating_sub(2); // bl + br = 2
    out.push(BLine { text: format!("{bl}{}{br}", h.repeat(bfill)), role: BRole::Frame });
    out
}

/// ANSI-colored banner for the `--banner` flag (a preview of the launch screen).
pub fn banner_ansi(width: u16, ascii: bool, theme: &str, status: &[String]) -> String {
    use super::types::is_16color_terminal;

    let p = palette::palette_by_name(theme);
    let w = (width as usize).saturating_sub(4).max(8);
    let rainbow = if is_16color_terminal() { palette::APPLE_RAINBOW_16 } else { palette::APPLE_RAINBOW };
    let reset = "\x1b[0m";

    let mut out = String::new();
    for bl in banner_lines(w, ascii, status) {
        let prefix = match bl.role {
            BRole::Art(i) => palette::ansi_fg(palette::banner_row_color(&p, &rainbow, i)),
            BRole::Version => format!("\x1b[1m{}", palette::ansi_fg(p.accent)),
            BRole::Tagline => palette::ansi_fg(p.notice),
            BRole::Frame | BRole::Data => palette::ansi_fg(p.accent),
        };
        out.push_str(&format!("{prefix}{}{reset}\n", bl.text));
    }
    out
}

/// Push banner lines into the transcript as `TLine`s.
pub fn push_banner_transcript(
    transcript: &mut Vec<TLine>,
    width: u16,
    ascii: bool,
    theme: &str,
    status: Vec<String>,
) {
    let w = (width as usize).saturating_sub(4).max(8);
    for bl in banner_lines(w, ascii, &status) {
        let color = match bl.role {
            BRole::Art(i) => Some(BannerColor::Rainbow(i)),
            BRole::Version | BRole::Data => Some(BannerColor::Accent),
            BRole::Tagline | BRole::Frame => Some(BannerColor::Fixed(
                palette::palette_by_name(theme).notice,
            )),
        };
        transcript.push(TLine {
            kind: if matches!(bl.role, BRole::Tagline | BRole::Frame) {
                Kind::BannerDim
            } else {
                Kind::Banner
            },
            text: bl.text,
            lead: true,
            color,
        });
    }
}

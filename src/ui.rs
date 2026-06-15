//! Full-screen ratatui interface: a scrolling transcript, a multi-line
//! composer, and a status bar. Runs on the UI thread; the agent runs on a
//! worker thread and feeds this UI through a channel.

use crate::agent::{ApprovalResponse, Handles, UiEvent, WorkerCmd};
use crate::config::{memory_path, Config, ConfigPatch, PROVIDERS};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
    KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const SPIN_U: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPIN_A: [&str; 4] = ["|", "/", "-", "\\"];
const MAX_TRANSCRIPT: usize = 4000;

/// Apple-logo rainbow (top→bottom): green, yellow, orange, red, purple, blue.
const APPLE_RAINBOW: [Color; 6] = [
    Color::Rgb(97, 187, 70),
    Color::Rgb(253, 184, 39),
    Color::Rgb(245, 130, 31),
    Color::Rgb(224, 58, 62),
    Color::Rgb(150, 61, 151),
    Color::Rgb(0, 157, 220),
];
/// 16-color approximation for the framebuffer console.
const APPLE_RAINBOW_16: [Color; 6] = [
    Color::Green,
    Color::Yellow,
    Color::LightRed,
    Color::Red,
    Color::Magenta,
    Color::Blue,
];

#[derive(Clone, Copy, PartialEq)]
enum CursorKind {
    Caret,   // hardware caret
    Reverse, // reverse-video over the char (Claude-style)
    Block,   // a solid ▒/# block (Apple ][ / DOS)
}

/// A color theme. `mono_banner = Some(c)` draws the logo in one color instead
/// of the Apple rainbow.
#[derive(Clone, Copy)]
struct Palette {
    name: &'static str,
    accent: Color,
    assistant: Color,
    assistant_glyph: Color,
    reasoning: Color,
    tool: Color,
    tool_result: Color,
    notice: Color,
    code: Color,
    heading: Color,
    diff_add: Color,
    diff_del: Color,
    diff_ctx: Color,
    error: Color,
    mono_banner: Option<Color>,
    /// Theme-specific composer prompt (else the default glyph set's prompt).
    prompt: Option<&'static str>,
    cursor: CursorKind,
}

const DEFAULT_PALETTE: Palette = Palette {
    name: "default",
    accent: Color::Cyan,
    assistant: Color::White,
    assistant_glyph: Color::Green,
    reasoning: Color::DarkGray,
    tool: Color::Blue,
    tool_result: Color::Rgb(150, 150, 150),
    notice: Color::Rgb(150, 150, 150),
    code: Color::Yellow,
    heading: Color::White,
    diff_add: Color::Green,
    diff_del: Color::Red,
    diff_ctx: Color::DarkGray,
    error: Color::Red,
    mono_banner: None,
    prompt: None,
    cursor: CursorKind::Reverse,
};

// Apple ][ — green phosphor on black.
const APPLE2_GREEN: Color = Color::Rgb(51, 255, 51);
const APPLE2_PALETTE: Palette = Palette {
    name: "apple2",
    accent: APPLE2_GREEN,
    assistant: APPLE2_GREEN,
    assistant_glyph: APPLE2_GREEN,
    reasoning: Color::Rgb(0, 140, 0),
    tool: APPLE2_GREEN,
    tool_result: Color::Rgb(0, 190, 0),
    notice: Color::Rgb(0, 190, 0),
    code: Color::Rgb(140, 255, 140),
    heading: APPLE2_GREEN,
    diff_add: Color::Rgb(140, 255, 140),
    diff_del: Color::Rgb(0, 140, 0),
    diff_ctx: Color::Rgb(0, 110, 0),
    error: Color::Rgb(255, 90, 90),
    // Keep the rainbow Apple logo (period-correct) even on the green theme.
    mono_banner: None,
    prompt: Some("] "),
    cursor: CursorKind::Block,
};

// MS-DOS — light-gray text on black, C:\> prompt.
const MSDOS_PALETTE: Palette = Palette {
    name: "msdos",
    accent: Color::White,
    assistant: Color::Gray,
    assistant_glyph: Color::White,
    reasoning: Color::DarkGray,
    tool: Color::Cyan,
    tool_result: Color::Gray,
    notice: Color::Gray,
    code: Color::LightGreen,
    heading: Color::White,
    diff_add: Color::Green,
    diff_del: Color::Red,
    diff_ctx: Color::DarkGray,
    error: Color::LightRed,
    mono_banner: Some(Color::White),
    prompt: Some("C:\\> "),
    cursor: CursorKind::Block,
};

const THEMES: &[&str] = &["default", "apple2", "msdos"];

fn palette_by_name(name: &str) -> Palette {
    match name {
        "apple2" | "apple][" | "appleii" | "apple2e" => APPLE2_PALETTE,
        "msdos" | "dos" => MSDOS_PALETTE,
        _ => DEFAULT_PALETTE,
    }
}

/// Glyphs vary by terminal: the Pi's framebuffer console (TERM=linux) lacks
/// the fancy Unicode used over SSH, so we fall back to ASCII there.
#[derive(Clone, Copy)]
struct Glyphs {
    user: &'static str,
    assistant: &'static str,
    tool: &'static str,
    result: &'static str,
    error: &'static str,
    prompt: &'static str,
    rounded: bool,
}

const GLYPHS_U: Glyphs = Glyphs {
    user: "❯ ",
    assistant: "● ",
    tool: "⏺ ",
    result: "⎿ ",
    error: "✗ ",
    prompt: "❯ ",
    rounded: true,
};
const GLYPHS_A: Glyphs = Glyphs {
    user: "> ",
    assistant: "* ",
    tool: "* ",
    result: "> ",
    error: "x ",
    prompt: "> ",
    rounded: false,
};

/// Choose a glyph set. ASCII for dumb terminals, or when forced via
/// PICODE_ASCII=1; Unicode otherwise (or forced via PICODE_UNICODE=1).
/// TERM=linux stays Unicode: the framebuffer console renders the glyphs we
/// use at single-cell width (set PICODE_ASCII=1 on consoles whose font
/// doesn't cover them).
pub fn detect_ascii() -> bool {
    if std::env::var("PICODE_UNICODE").is_ok() {
        return false;
    }
    if std::env::var("PICODE_ASCII").is_ok() {
        return true;
    }
    match std::env::var("TERM").as_deref() {
        Ok("dumb") | Ok("vt100") | Ok("") | Err(_) => true,
        _ => false,
    }
}

/// True when the terminal likely has only 16 colors (Linux console without truecolor).
pub fn is_16color_terminal() -> bool {
    // COLORTERM=truecolor or COLORTERM=24bit means true color support.
    matches!(std::env::var("COLORTERM").as_deref(), Ok("truecolor") | Ok("24bit"))
        == false
        && matches!(std::env::var("TERM").as_deref(), Ok("linux") | Ok("dumb") | Ok("vt100"))
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    User,
    Assistant,
    Reasoning,
    Tool,
    ToolResult,
    ToolErr,
    DiffAdd,
    DiffDel,
    DiffCtx,
    Notice,
    ErrorK,
    Code,
    Heading,
    Banner,
    BannerDim,
}

/// Slash commands with the one-line description shown in the `/` palette.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/model", "pick a model from the provider's list"),
    ("/new", "clear conversation and session"),
    ("/config", "settings: provider, model, key, thinking, …"),
    ("/compact", "summarize older turns to free context"),
    ("/reset", "clear conversation context"),
    ("/auto", "toggle bypass-permissions"),
    ("/mcp", "list MCP servers and their tools"),
    ("/memory", "show persistent memory"),
    ("/theme", "open the theme picker"),
    ("/init", "summarize this project into PICODE.md"),
    ("/clear", "clear the screen transcript"),
    ("/help", "show help"),
    ("/exit", "quit picode"),
];

/// Most suggestions shown under the composer for a `/` prefix.
const MAX_SUGGEST: usize = 8;
/// Window within which a second Ctrl+C/Ctrl+D exits (Claude Code style).
const DOUBLE_PRESS_TIMEOUT: Duration = Duration::from_secs(2);

/// Rows of the `/config` panel, in display order.
const SETTING_LABELS: &[&str] = &[
    "provider",
    "base url",
    "model",
    "api key",
    "thinking",
    "permissions",
    "auto-commit",
    "theme",
    "context window",
    "max tool calls",
];

/// Heavy block-letter "PICODE" for capable terminals.
const ART_UNICODE: [&str; 6] = [
    "██████░ ██░ ██████░ ██████░ ██████░ ███████░",
    "██░░░██░██░██░░░░░░██░░░░██░██░░░██░██░░░░░░",
    "██████░░██░██░     ██░   ██░██░  ██░█████░  ",
    "██░░░░░ ██░██░     ██░   ██░██░  ██░██░░░░  ",
    "██░     ██░░██████░░██████░░██████░░███████░",
    "░░░     ░░░ ░░░░░░░ ░░░░░░░ ░░░░░░░ ░░░░░░░░",
];

/// 6x4 ASCII glyphs (P I C O D E), assembled at runtime to guarantee alignment.
const ART_GLYPHS: [[&str; 6]; 6] = [
    ["####", "#  #", "####", "#   ", "#   ", "#   "], // P
    ["####", " ## ", " ## ", " ## ", "####", "####"], // I
    ["####", "#   ", "#   ", "#   ", "####", "####"], // C
    ["####", "#  #", "#  #", "#  #", "####", "####"], // O
    ["### ", "#  #", "#  #", "#  #", "### ", "### "], // D
    ["####", "#   ", "### ", "#   ", "####", "####"], // E
];

fn ascii_art() -> Vec<String> {
    (0..6)
        .map(|r| ART_GLYPHS.iter().map(|g| g[r]).collect::<Vec<_>>().join(" "))
        .collect()
}

/// Pick the widest PICODE art that fits in `w` columns.
fn banner_art(w: usize, ascii: bool) -> Vec<String> {
    if !ascii && w >= 46 {
        ART_UNICODE.iter().map(|s| s.to_string()).collect()
    } else if w >= 31 {
        ascii_art()
    } else {
        vec!["P I C O D E".to_string()]
    }
}

const TAGLINE: &str = "a tiny agentic coding CLI";

/// Role of a banner line, so the TUI and the ANSI `--banner` preview color the
/// same layout identically.
#[derive(Clone, Copy)]
enum BRole {
    Art(usize), // rainbow/mono block-art row
    Version,    // bold accent
    Tagline,    // dim
    Frame,      // dim panel rule / blank
    Data,       // accent status line
}

struct BLine {
    text: String,
    role: BRole,
}

/// Build the launch banner as structured lines: centered block art, a version
/// + tagline, and a bordered SYSTEM panel wrapping the status lines.
fn banner_lines(w: usize, ascii: bool, status: &[String]) -> Vec<BLine> {
    let center = |s: &str| {
        let pad = " ".repeat(w.saturating_sub(s.chars().count()) / 2);
        format!("{pad}{s}")
    };
    let art = banner_art(w, ascii);
    let artw = art.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let art_pad = " ".repeat(w.saturating_sub(artw) / 2);

    let mut out = Vec::new();
    for (i, l) in art.iter().enumerate() {
        out.push(BLine { text: format!("{art_pad}{l}"), role: BRole::Art(i) });
    }
    out.push(BLine { text: String::new(), role: BRole::Frame });
    out.push(BLine { text: center(&format!("picode v{}", env!("CARGO_PKG_VERSION"))), role: BRole::Version });
    out.push(BLine { text: center(TAGLINE), role: BRole::Tagline });
    out.push(BLine { text: String::new(), role: BRole::Frame });

    let (tl, bl, h, vbar) = if ascii { ("+", "+", "-", "|") } else { ("┌", "└", "─", "│") };
    let head = format!("{tl}{h} SYSTEM ");
    let fill = w.saturating_sub(head.chars().count());
    out.push(BLine { text: format!("{head}{}", h.repeat(fill)), role: BRole::Frame });
    for line in status {
        out.push(BLine { text: format!("{vbar} {line}"), role: BRole::Data });
    }
    out.push(BLine { text: format!("{bl}{}", h.repeat(w.saturating_sub(1))), role: BRole::Frame });
    out
}

fn ansi_fg(c: Color) -> String {
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

/// ANSI-colored banner for the `--banner` flag (a preview of the launch screen).
pub fn banner_ansi(width: u16, ascii: bool, theme: &str, status: &[String]) -> String {
    let p = palette_by_name(theme);
    let w = (width as usize).saturating_sub(4).max(8);
    let rainbow = if is_16color_terminal() { APPLE_RAINBOW_16 } else { APPLE_RAINBOW };
    let reset = "\x1b[0m";

    let mut out = String::new();
    for bl in banner_lines(w, ascii, status) {
        let prefix = match bl.role {
            BRole::Art(i) => ansi_fg(p.mono_banner.unwrap_or(rainbow[i % rainbow.len()])),
            BRole::Version => format!("\x1b[1m{}", ansi_fg(p.accent)),
            BRole::Tagline | BRole::Frame => ansi_fg(p.notice),
            BRole::Data => ansi_fg(p.accent),
        };
        out.push_str(&format!("{prefix}{}{reset}\n", bl.text));
    }
    out
}

struct TLine {
    kind: Kind,
    text: String,
    /// First line of a block — shows the glyph; later lines align under it.
    lead: bool,
    /// Optional per-line fg override (used by the rainbow banner).
    color: Option<Color>,
}

enum Mode {
    Idle,
    Busy,
    Approval(String),
    /// Interactive theme picker (`/theme` with no argument). `cursor` is the
    /// highlighted theme index; `prev` is the theme to restore if cancelled.
    ThemeSelect { cursor: usize, prev: String },
    /// Masked sudo password entry, requested by the askpass helper. `prompt` is
    /// the text sudo asked with (e.g. "[sudo] password for user:").
    Password { prompt: String },
    /// The ask_user tool: a visible one-line answer to the agent's question.
    Question { prompt: String },
    /// The `/config` panel. `edit` holds the text buffer while a free-text
    /// row (base url, model, api key, context window) is being edited.
    Settings { cursor: usize, edit: Option<String> },
    /// A generic selection list (state lives in `App::picker`): cursor +
    /// type-to-filter, used by `/model` for the fetched model list.
    Select,
}

/// What committing a `Select` picker entry does.
#[derive(Clone, Copy)]
enum PickAction {
    /// Switch model to the chosen id.
    Model,
}

/// State for `Mode::Select`: a filterable, scrollable list of choices.
struct Picker {
    title: String,
    items: Vec<String>,
    /// Item highlighted as the current value (shown with a marker).
    current: Option<usize>,
    filter: String,
    /// Cursor within the *filtered* view.
    cursor: usize,
    scroll: usize,
    action: PickAction,
}

/// Visible rows of a Select picker before it scrolls.
const PICKER_VISIBLE: usize = 8;

impl Picker {
    /// Indices of items matching the filter (case-insensitive substring).
    fn filtered(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.items.len()).collect();
        }
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| self.items[i].to_lowercase().contains(&f))
            .collect()
    }

    /// Keep the cursor inside the filtered list and the scroll window.
    fn clamp(&mut self, filtered_len: usize) {
        if filtered_len == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = self.cursor.min(filtered_len - 1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + PICKER_VISIBLE {
            self.scroll = self.cursor + 1 - PICKER_VISIBLE;
        }
    }
}

/// Everything the UI needs from config, captured before the worker consumes it.
pub struct UiConfig {
    pub model: String,
    pub theme: String,
    pub ascii: bool,
    pub ctx_limit: u32,
    pub price_in: f64,
    pub price_out: f64,
    pub perm: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Snapshot for the `/config` panel; kept in sync as patches are sent.
    pub settings: Config,
}

pub struct App {
    transcript: Vec<TLine>,
    live: String,
    live_reasoning: String,
    input: String,
    cursor: usize, // char index
    history: Vec<String>,
    hist_idx: usize,
    pending: String,
    /// Messages typed while the agent was busy, sent in order as turns finish.
    queued: Vec<String>,
    /// Highlighted row of the `/` command palette (clamped at use).
    suggest_idx: usize,
    /// State for Mode::Select (the /model list, etc.).
    picker: Option<Picker>,
    mode: Mode,
    /// Buffer + reply channel for an in-flight masked sudo password prompt. Held
    /// in memory only; never pushed to the transcript, history, or session.
    pw_input: String,
    pw_reply: Option<std::sync::mpsc::Sender<Option<String>>>,
    /// Buffer + reply channel for an in-flight ask_user question.
    q_input: String,
    q_reply: Option<std::sync::mpsc::Sender<Option<String>>>,
    follow: bool,
    scroll: usize,
    max_top: usize,
    view_h: usize,
    spinner: usize,
    spin_counter: usize,
    model_info: String,
    cwd: String,
    last_models: Vec<String>,
    should_quit: bool,
    esc_deadline: Option<Instant>,
    /// Time of last Ctrl+C with empty input; used for double-press-to-quit.
    last_ctrl_c: Option<Instant>,
    /// Set by Ctrl+L; makes the event loop clear the backend before the next
    /// draw, forcing a full repaint (recovers from any screen desync).
    force_clear: bool,
    /// Terminal draws every glyph in one cell (ASCII mode, or the Linux
    /// framebuffer console) — wide chars must be replaced before rendering.
    single_width: bool,
    glyphs: Glyphs,
    ascii: bool,
    palette: Palette,
    // Claude-style status data:
    perm: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ctx_limit: u32,
    price_in: f64,
    price_out: f64,
    last_prompt_tokens: u32,
    sess_prompt: u64,
    sess_completion: u64,
    balance: Option<String>,
    settings: Config,
}

impl App {
    pub fn new(cfg: UiConfig, history: Vec<String>) -> App {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "~".into());
        let hist_idx = history.len();
        App {
            transcript: Vec::new(),
            live: String::new(),
            live_reasoning: String::new(),
            input: String::new(),
            cursor: 0,
            history,
            hist_idx,
            pending: String::new(),
            queued: Vec::new(),
            suggest_idx: 0,
            picker: None,
            mode: Mode::Idle,
            pw_input: String::new(),
            pw_reply: None,
            q_input: String::new(),
            q_reply: None,
            follow: true,
            scroll: 0,
            max_top: 0,
            view_h: 0,
            spinner: 0,
            spin_counter: 0,
            model_info: cfg.model,
            cwd,
            last_models: Vec::new(),
            should_quit: false,
            esc_deadline: None,
            last_ctrl_c: None,
            force_clear: false,
            single_width: cfg.ascii
                || matches!(std::env::var("TERM").as_deref(), Ok("linux")),
            glyphs: if cfg.ascii { GLYPHS_A } else { GLYPHS_U },
            ascii: cfg.ascii,
            palette: palette_by_name(&cfg.theme),
            perm: cfg.perm,
            ctx_limit: cfg.ctx_limit.max(1),
            price_in: cfg.price_in,
            price_out: cfg.price_out,
            last_prompt_tokens: 0,
            sess_prompt: 0,
            sess_completion: 0,
            balance: None,
            settings: cfg.settings,
        }
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    fn push(&mut self, kind: Kind, text: impl Into<String>) {
        for (i, ln) in text.into().split('\n').enumerate() {
            self.transcript.push(TLine { kind, text: ln.to_string(), lead: i == 0, color: None });
        }
        self.after_push();
    }

    fn after_push(&mut self) {
        if self.transcript.len() > MAX_TRANSCRIPT {
            let drop = self.transcript.len() - MAX_TRANSCRIPT;
            self.transcript.drain(0..drop);
        }
        self.follow = true;
    }

    /// Push assistant text, classifying lines: ``` fences toggle code blocks,
    /// `#`-prefixed lines are headings, everything else is prose.
    fn push_assistant(&mut self, text: &str) {
        let mut in_code = false;
        let mut first = true;
        for ln in text.split('\n') {
            let trimmed = ln.trim_start();
            if trimmed.starts_with("```") {
                in_code = !in_code;
                self.transcript.push(TLine { kind: Kind::Code, text: ln.to_string(), lead: false, color: None });
                continue;
            }
            let kind = if in_code {
                Kind::Code
            } else if trimmed.starts_with('#') && trimmed.contains("# ") {
                Kind::Heading
            } else {
                Kind::Assistant
            };
            self.transcript.push(TLine { kind, text: ln.to_string(), lead: first, color: None });
            first = false;
        }
        self.after_push();
    }

    fn flush_live(&mut self) {
        if !self.live.is_empty() {
            let live = std::mem::take(&mut self.live);
            self.push_assistant(&live);
        }
        self.live.clear();
        self.live_reasoning.clear();
    }

    pub fn handle_event(&mut self, ev: UiEvent, h: &Handles) {
        match ev {
            UiEvent::Token(t) => {
                self.live.push_str(&t);
                self.follow = true;
            }
            UiEvent::Reasoning(t) => {
                if self.live.is_empty() {
                    self.live_reasoning.push_str(&t);
                    self.follow = true;
                }
            }
            UiEvent::ResetLive => {
                self.live.clear();
                self.live_reasoning.clear();
            }
            UiEvent::AssistantCommit => self.flush_live(),
            UiEvent::ToolStart { name, summary } => {
                self.flush_live();
                let text = if summary.is_empty() {
                    name
                } else {
                    format!("{name}  {summary}")
                };
                self.push(Kind::Tool, text);
            }
            UiEvent::Diff(d) => {
                self.flush_live();
                for ln in d.lines() {
                    let kind = if ln.starts_with('+') {
                        Kind::DiffAdd
                    } else if ln.starts_with('-') {
                        Kind::DiffDel
                    } else {
                        Kind::DiffCtx
                    };
                    self.transcript.push(TLine { kind, text: ln.to_string(), lead: true, color: None });
                }
                self.follow = true;
            }
            UiEvent::ToolResult { ok, preview } => {
                self.flush_live();
                self.push(if ok { Kind::ToolResult } else { Kind::ToolErr }, preview);
            }
            UiEvent::Approval(desc) => {
                self.flush_live();
                self.mode = Mode::Approval(desc);
            }
            UiEvent::PasswordRequest { prompt, reply } => {
                self.flush_live();
                self.pw_input.clear();
                self.pw_reply = Some(reply);
                self.mode = Mode::Password { prompt };
            }
            UiEvent::Question { prompt, reply } => {
                self.flush_live();
                self.q_input.clear();
                self.q_reply = Some(reply);
                self.mode = Mode::Question { prompt };
            }
            UiEvent::ModelList(ids) => {
                self.flush_live();
                self.last_models = ids.clone();
                if ids.is_empty() {
                    self.push(Kind::Notice, "provider returned no models.".to_string());
                    return;
                }
                // Open an interactive picker instead of printing the list.
                let current = ids.iter().position(|id| id == self.model_short());
                let mut p = Picker {
                    title: format!("select model ({})", ids.len()),
                    items: ids,
                    current,
                    filter: String::new(),
                    cursor: current.unwrap_or(0),
                    scroll: 0,
                    action: PickAction::Model,
                };
                let n = p.filtered().len();
                p.clamp(n);
                self.picker = Some(p);
                self.mode = Mode::Select;
            }
            UiEvent::ModelChanged(m) => {
                self.settings.model = m.clone();
                self.model_info = m;
            }
            UiEvent::Usage { prompt, completion } => {
                self.last_prompt_tokens = prompt;
                self.sess_prompt += prompt as u64;
                self.sess_completion += completion as u64;
            }
            UiEvent::Context(tokens) => {
                self.last_prompt_tokens = tokens;
            }
            UiEvent::Balance(b) => {
                self.balance = Some(b);
            }
            UiEvent::Notice(s) => {
                self.flush_live();
                self.push(Kind::Notice, s);
            }
            UiEvent::Error(s) => {
                self.flush_live();
                self.push(Kind::ErrorK, s);
            }
            UiEvent::TurnDone => {
                self.flush_live();
                // Don't clobber an interactive mode a preceding event opened
                // in this same turn (e.g. ModelList → Select picker).
                if matches!(self.mode, Mode::Busy) {
                    self.mode = Mode::Idle;
                }
                // Send the next queued message, if any. Local slash commands
                // resolve immediately (no TurnDone), so keep draining until
                // something goes to the worker or the queue empties.
                while !self.queued.is_empty() && matches!(self.mode, Mode::Idle) {
                    let text = self.queued.remove(0);
                    self.dispatch(text, h);
                }
            }
        }
    }

    fn model_short(&self) -> &str {
        self.model_info.split(':').nth(1).unwrap_or(&self.model_info)
    }

    pub fn on_paste(&mut self, s: String) {
        if matches!(self.mode, Mode::Idle | Mode::Busy) {
            let mut filtered = String::with_capacity(s.len());
            for c in s.chars() {
                if c == '\n' || c == '\r' || c == '\t' {
                    filtered.push(' ');
                } else if !c.is_control() {
                    filtered.push(c);
                }
            }
            self.last_ctrl_c = None;
            let byte = self.byte_at(self.cursor);
            let added = filtered.chars().count();
            self.input.insert_str(byte, &filtered);
            self.cursor += added;
            self.suggest_idx = 0;
        }
    }

    fn insert_char(&mut self, c: char) {
        // Control characters would be written verbatim into the composer cells
        // and desync the terminal (a raw ESC starts an escape sequence).
        if c.is_control() {
            return;
        }
        let byte = self.byte_at(self.cursor);
        self.input.insert(byte, c);
        self.cursor += 1;
        // New text narrows the `/` palette — restart from the top match.
        self.suggest_idx = 0;
    }

    /// Commands matching the typed `/` prefix, for the palette under the
    /// composer. "Most likely" first: ranked by how often each command appears
    /// in composer history, with the curated order breaking ties. Empty unless
    /// idle with a bare command token (no arguments yet).
    fn slash_suggestions(&self) -> Vec<(&'static str, &'static str)> {
        if !matches!(self.mode, Mode::Idle | Mode::Busy)
            || !self.input.starts_with('/')
            || self.input.contains(char::is_whitespace)
        {
            return Vec::new();
        }
        // One prefix allocation per command, not per history entry.
        let uses = |cmd: &str| {
            let with_arg = format!("{cmd} ");
            self.history
                .iter()
                .filter(|h| h.as_str() == cmd || h.starts_with(&with_arg))
                .count()
        };
        let mut scored: Vec<((&'static str, &'static str), usize)> = SLASH_COMMANDS
            .iter()
            .filter(|(c, _)| c.starts_with(self.input.as_str()))
            .map(|&(c, d)| ((c, d), uses(c)))
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1)); // stable: ties keep curated order
        scored.into_iter().map(|(cd, _)| cd).take(MAX_SUGGEST).collect()
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
    }

    fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    pub fn on_key(&mut self, key: KeyEvent, h: &Handles) {
        // Masked sudo password entry captures every key (so a stray Ctrl+P, Tab,
        // etc. never leaks out or triggers a shortcut) — handle it first.
        if matches!(self.mode, Mode::Password { .. }) {
            self.on_key_password(key);
            return;
        }
        // ask_user answers likewise capture every key.
        if matches!(self.mode, Mode::Question { .. }) {
            self.on_key_question(key);
            return;
        }
        // Cycle permission mode in any state. Shift+Tab is the primary binding:
        // it arrives as BackTab on ANSI terminals, or Tab+SHIFT under the Kitty
        // keyboard protocol. But the Linux framebuffer console (TERM=linux)
        // reports Shift+Tab as a bare Tab — its default keymap has no shift
        // binding for the Tab key, so the two are byte-identical and no app can
        // tell them apart. Ctrl+P is a console-safe alias that works there (and
        // everywhere else), since Ctrl-letter chords always come through.
        let shift_tab = key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
        let ctrl_p = matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
            && key.modifiers.contains(KeyModifiers::CONTROL);
        if shift_tab || ctrl_p {
            self.cycle_perm();
            return;
        }
        // Ctrl+L: force a full clear + repaint in any state. The framebuffer
        // console in particular can drift out of sync with ratatui's diff
        // buffer (font/width quirks, kernel messages); this recovers it.
        if matches!(key.code, KeyCode::Char('l') | KeyCode::Char('L'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.force_clear = true;
            self.follow = true;
            return;
        }
        // If ESC arrived recently (within 50 ms) and another key follows,
        // terminals are encoding Alt+key as ESC prefix.  Treat the key as
        // Alt-modified instead of acting on the lone ESC.
        // Only relevant in Idle mode; clear stale deadlines for other modes.
        if !matches!(&self.mode, Mode::Idle) {
            self.esc_deadline = None;
        }
        if let Some(deadline) = self.esc_deadline {
            self.esc_deadline = None;
            if Instant::now() < deadline {
                let mut alt_key = key;
                alt_key.modifiers |= KeyModifiers::ALT;
                match &self.mode {
                    Mode::Approval(_) => self.on_key_approval(alt_key, h),
                    Mode::ThemeSelect { .. } => self.on_key_themeselect(alt_key, h),
                    Mode::Settings { .. } => self.on_key_settings(alt_key, h),
                    Mode::Select => self.on_key_select(alt_key, h),
                    Mode::Busy => self.on_key_busy(alt_key, h),
                    Mode::Password { .. } | Mode::Question { .. } => {}
                    Mode::Idle => self.on_key_idle(alt_key, h),
                }
                return;
            } else {
                // ESC timed out — process it as a standalone Esc first.
                self.do_esc(h);
            }
        }
        match &self.mode {
            Mode::Approval(_) => self.on_key_approval(key, h),
            Mode::ThemeSelect { .. } => self.on_key_themeselect(key, h),
            Mode::Settings { .. } => self.on_key_settings(key, h),
            Mode::Select => self.on_key_select(key, h),
            Mode::Busy => self.on_key_busy(key, h),
            // Password/Question are intercepted at the top of on_key.
            Mode::Password { .. } | Mode::Question { .. } => {}
            Mode::Idle => self.on_key_idle(key, h),
        }
    }

    /// Handle a standalone Esc (not an Alt-prefix).
    fn do_esc(&mut self, h: &Handles) {
        match &self.mode {
            Mode::Approval(_) => {
                let _ = h.appr_tx.send(ApprovalResponse::No);
            }
            Mode::ThemeSelect { prev, .. } => {
                // Cancel the picker, restoring the theme we started with.
                self.palette = palette_by_name(&prev.clone());
                self.mode = Mode::Idle;
            }
            Mode::Busy => h.shared.cancel.store(true, Ordering::Relaxed),
            Mode::Settings { .. } => self.mode = Mode::Idle,
            Mode::Select => {
                self.picker = None;
                self.mode = Mode::Idle;
            }
            // Password/Question Esc is handled in their own key handlers.
            Mode::Password { .. } | Mode::Question { .. } => {}
            Mode::Idle => {
                self.input.clear();
                self.cursor = 0;
            }
        }
    }

    /// Masked sudo password entry. Keystrokes go into `pw_input` (never shown);
    /// Enter sends it to the askpass helper, Esc/Ctrl+C cancel. Either way we
    /// return to the busy view, since the bash command is still running.
    fn on_key_password(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => {
                let pw = std::mem::take(&mut self.pw_input);
                if let Some(tx) = self.pw_reply.take() {
                    let _ = tx.send(Some(pw));
                }
                self.mode = Mode::Busy;
            }
            KeyCode::Esc => self.cancel_password(),
            KeyCode::Char('c') if ctrl => self.cancel_password(),
            KeyCode::Char('u') if ctrl => self.pw_input.clear(),
            KeyCode::Backspace => {
                self.pw_input.pop();
            }
            KeyCode::Char(c) if !ctrl && !alt => self.pw_input.push(caps_char(&key, c)),
            _ => {}
        }
    }

    /// Abort the password prompt: tell the helper there's no password (so sudo
    /// fails cleanly) and drop the buffer.
    fn cancel_password(&mut self) {
        self.pw_input.clear();
        if let Some(tx) = self.pw_reply.take() {
            let _ = tx.send(None);
        }
        self.mode = Mode::Busy;
    }

    /// The ask_user answer line. Enter sends the answer; Esc declines. The
    /// agent's bash-free turn is still running, so we return to the busy view.
    fn on_key_question(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => {
                let ans = std::mem::take(&mut self.q_input);
                self.push(Kind::User, ans.clone());
                if let Some(tx) = self.q_reply.take() {
                    let _ = tx.send(Some(ans));
                }
                self.mode = Mode::Busy;
            }
            KeyCode::Esc => self.cancel_question(),
            KeyCode::Char('c') if ctrl => self.cancel_question(),
            KeyCode::Char('u') if ctrl => self.q_input.clear(),
            KeyCode::Backspace => {
                self.q_input.pop();
            }
            KeyCode::Char(c) if !ctrl && !alt => self.q_input.push(caps_char(&key, c)),
            _ => {}
        }
    }

    fn cancel_question(&mut self) {
        self.q_input.clear();
        if let Some(tx) = self.q_reply.take() {
            let _ = tx.send(None);
        }
        self.mode = Mode::Busy;
    }

    fn cycle_perm(&self) {
        let next = (self.perm.load(Ordering::Relaxed) + 1) % 3;
        self.perm.store(next, Ordering::Relaxed);
    }

    fn perm(&self) -> u8 {
        self.perm.load(Ordering::Relaxed)
    }

    fn on_key_approval(&mut self, key: KeyEvent, h: &Handles) {
        let resp = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalResponse::Yes),
            KeyCode::Char('a') | KeyCode::Char('A') => Some(ApprovalResponse::Always),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ApprovalResponse::No),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                h.shared.cancel.store(true, Ordering::Relaxed);
                Some(ApprovalResponse::No)
            }
            _ => None,
        };
        if let Some(r) = resp {
            if matches!(r, ApprovalResponse::Always) {
                h.shared.perm.store(crate::agent::PERM_AUTO, Ordering::Relaxed);
            }
            if matches!(r, ApprovalResponse::No) {
                self.push(Kind::Notice, "denied".to_string());
            }
            let _ = h.appr_tx.send(r);
            self.mode = Mode::Busy;
        }
    }

    /// Live-preview the theme at `idx` (applies the palette so the whole UI
    /// updates) while keeping the picker open.
    fn preview_theme(&mut self, idx: usize, prev: String) {
        self.palette = palette_by_name(THEMES[idx]);
        self.mode = Mode::ThemeSelect { cursor: idx, prev };
    }

    /// Commit the theme at `idx`: persist it and close the picker.
    fn commit_theme(&mut self, idx: usize) {
        let name = THEMES[idx];
        self.palette = palette_by_name(name);
        crate::config::Config::persist_theme(name);
        self.push(Kind::Notice, format!("theme set to {name}"));
        self.mode = Mode::Idle;
    }

    /// `/config` panel keys. Up/Down move; Enter/←/→ cycle a choice row or
    /// open a text row for editing; while editing, Enter commits, Esc cancels;
    /// Esc otherwise closes the panel (everything was applied as it changed).
    fn on_key_settings(&mut self, key: KeyEvent, h: &Handles) {
        let (cur, editing) = match &self.mode {
            Mode::Settings { cursor, edit } => (*cursor, edit.clone()),
            _ => return,
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(mut buf) = editing {
            match key.code {
                KeyCode::Enter => {
                    self.mode = Mode::Settings { cursor: cur, edit: None };
                    self.commit_setting(cur, buf, h);
                }
                KeyCode::Esc => self.mode = Mode::Settings { cursor: cur, edit: None },
                KeyCode::Char('u') if ctrl => {
                    self.mode = Mode::Settings { cursor: cur, edit: Some(String::new()) };
                }
                KeyCode::Backspace => {
                    buf.pop();
                    self.mode = Mode::Settings { cursor: cur, edit: Some(buf) };
                }
                KeyCode::Char(c) if !ctrl => {
                    buf.push(caps_char(&key, c));
                    self.mode = Mode::Settings { cursor: cur, edit: Some(buf) };
                }
                _ => {}
            }
            return;
        }
        let n = SETTING_LABELS.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode = Mode::Settings { cursor: (cur + n - 1) % n, edit: None };
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode = Mode::Settings { cursor: (cur + 1) % n, edit: None };
            }
            KeyCode::Enter | KeyCode::Right => self.activate_setting(cur, 1, h),
            KeyCode::Left => self.activate_setting(cur, -1, h),
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Idle,
            _ => {}
        }
    }

    /// Act on a `/config` row: cycle choice rows by `dir`, or open text rows
    /// in the edit buffer. Choice changes apply (and persist) immediately.
    fn activate_setting(&mut self, cur: usize, dir: i32, h: &Handles) {
        let cycle = |i: usize, n: usize| ((i as i32 + dir).rem_euclid(n as i32)) as usize;
        let edit_with = |s: String| Mode::Settings { cursor: cur, edit: Some(s) };
        match cur {
            0 => {
                let i = PROVIDERS
                    .iter()
                    .position(|(n, _, _)| *n == self.settings.provider)
                    .map(|i| cycle(i, PROVIDERS.len()))
                    .unwrap_or(0);
                let (p, b, m) = PROVIDERS[i];
                self.settings.provider = p.to_string();
                self.settings.base_url = b.to_string();
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::Provider {
                    provider: p.to_string(),
                    base_url: b.to_string(),
                    model: m.to_string(),
                }));
            }
            1 => self.mode = edit_with(self.settings.base_url.clone()),
            2 => self.mode = edit_with(self.settings.model.clone()),
            3 => self.mode = edit_with(String::new()),
            4 => {
                let v = !self.settings.thinking;
                self.settings.thinking = v;
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::Thinking(v)));
            }
            5 => {
                let next = if dir >= 0 { (self.perm() + 1) % 3 } else { (self.perm() + 2) % 3 };
                self.perm.store(next, Ordering::Relaxed);
                let name = perm_name(next);
                self.settings.permission = name.to_string();
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::Permission(name.to_string())));
            }
            6 => {
                let v = !self.settings.auto_commit;
                self.settings.auto_commit = v;
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::AutoCommit(v)));
            }
            7 => {
                let i = THEMES
                    .iter()
                    .position(|t| *t == self.palette.name)
                    .map(|i| cycle(i, THEMES.len()))
                    .unwrap_or(0);
                self.palette = palette_by_name(THEMES[i]);
                self.settings.theme = THEMES[i].to_string();
                Config::persist_theme(THEMES[i]);
            }
            8 => self.mode = edit_with(self.settings.context_window.to_string()),
            9 => self.mode = edit_with(setting_max_tool_calls(self.settings.max_tool_calls)),
            _ => {}
        }
    }

    /// Commit a text edit from the `/config` panel.
    fn commit_setting(&mut self, cur: usize, val: String, h: &Handles) {
        let val = val.trim().to_string();
        match cur {
            1 => {
                if !val.is_empty() {
                    let v = val.trim_end_matches('/').to_string();
                    self.settings.base_url = v.clone();
                    let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::BaseUrl(v)));
                }
            }
            2 => {
                if !val.is_empty() {
                    // SetModel persists and echoes ModelChanged + a notice.
                    let _ = h.cmd_tx.send(WorkerCmd::SetModel(val));
                }
            }
            3 => {
                if !val.is_empty() {
                    self.settings.api_key = val.clone();
                    self.settings.key_from_env = false;
                    let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::ApiKey(val)));
                }
            }
            8 => match val.parse::<u32>() {
                Ok(n) if n > 0 => {
                    self.settings.context_window = n;
                    self.ctx_limit = n;
                    let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::ContextWindow(n)));
                }
                _ => self.push(Kind::ErrorK, "context window must be a positive integer".to_string()),
            },
            9 => {
                let n = parse_max_tool_calls(&val);
                self.settings.max_tool_calls = n;
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::MaxToolCalls(n)));
            }
            _ => {}
        }
    }

    /// Generic selection list (Mode::Select). Up/Down move, typing filters,
    /// Enter commits the highlighted item, Esc cancels.
    fn on_key_select(&mut self, key: KeyEvent, h: &Handles) {
        let Some(p) = &mut self.picker else {
            self.mode = Mode::Idle;
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let filtered = p.filtered();
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
                self.mode = Mode::Idle;
            }
            KeyCode::Char('c') if ctrl => {
                self.picker = None;
                self.mode = Mode::Idle;
            }
            KeyCode::Enter => {
                let chosen = filtered.get(p.cursor).map(|&i| p.items[i].clone());
                let action = p.action;
                self.picker = None;
                self.mode = Mode::Idle;
                if let Some(id) = chosen {
                    match action {
                        // SetModel persists and echoes ModelChanged + a notice.
                        PickAction::Model => {
                            let _ = h.cmd_tx.send(WorkerCmd::SetModel(id));
                        }
                    }
                }
            }
            KeyCode::Up => {
                let n = filtered.len();
                if n > 0 {
                    p.cursor = (p.cursor + n - 1) % n;
                    p.clamp(n);
                }
            }
            KeyCode::Down => {
                let n = filtered.len();
                if n > 0 {
                    p.cursor = (p.cursor + 1) % n;
                    p.clamp(n);
                }
            }
            KeyCode::PageUp => {
                p.cursor = p.cursor.saturating_sub(PICKER_VISIBLE);
                p.clamp(filtered.len());
            }
            KeyCode::PageDown => {
                p.cursor += PICKER_VISIBLE;
                p.clamp(filtered.len());
            }
            KeyCode::Char('u') if ctrl => {
                p.filter.clear();
                p.cursor = 0;
                p.scroll = 0;
            }
            KeyCode::Backspace => {
                p.filter.pop();
                p.cursor = 0;
                p.scroll = 0;
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                p.filter.push(caps_char(&key, c));
                p.cursor = 0;
                p.scroll = 0;
            }
            _ => {}
        }
    }

    fn on_key_themeselect(&mut self, key: KeyEvent, _h: &Handles) {
        let (cursor, prev) = match &self.mode {
            Mode::ThemeSelect { cursor, prev } => (*cursor, prev.clone()),
            _ => return,
        };
        let n = THEMES.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.preview_theme((cursor + n - 1) % n, prev),
            KeyCode::Down | KeyCode::Char('j') => self.preview_theme((cursor + 1) % n, prev),
            // Number keys select instantly, mirroring the (y)/(n)/(a) hotkeys.
            KeyCode::Char(d) if d.is_ascii_digit() => {
                let i = d.to_digit(10).unwrap_or(0) as usize;
                if (1..=n).contains(&i) {
                    self.commit_theme(i - 1);
                }
            }
            KeyCode::Enter => self.commit_theme(cursor),
            KeyCode::Esc => {
                self.palette = palette_by_name(&prev);
                self.mode = Mode::Idle;
            }
            _ => {}
        }
    }

    /// Busy: the composer stays live so the next message can be typed and
    /// queued with Enter; Esc/Ctrl+C still interrupt the running turn.
    fn on_key_busy(&mut self, key: KeyEvent, h: &Handles) {
        match key.code {
            KeyCode::Enter => self.queue_input(),
            KeyCode::Esc => self.interrupt(h),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.interrupt(h)
            }
            KeyCode::PageUp => self.scroll_up(),
            KeyCode::PageDown => self.scroll_down(),
            _ => {
                self.on_key_edit(key);
            }
        }
    }

    /// Cancel the running turn. Queued messages won't be auto-sent into a turn
    /// the user just killed: the first goes back into the (empty) composer,
    /// and the rest are dropped with a notice — they were recorded in history
    /// at queue time, so Up recalls them.
    fn interrupt(&mut self, h: &Handles) {
        h.shared.cancel.store(true, Ordering::Relaxed);
        if self.queued.is_empty() {
            return;
        }
        let mut q = std::mem::take(&mut self.queued);
        if self.input.is_empty() {
            self.input = q.remove(0);
            self.cursor = self.char_len();
        }
        if !q.is_empty() {
            self.push(
                Kind::Notice,
                format!("({} queued message(s) cleared — recall with Up)", q.len()),
            );
        }
    }

    fn on_key_idle(&mut self, key: KeyEvent, h: &Handles) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // While the `/` palette is open, Up/Down/Tab/Enter act on it.
        let sugg = self.slash_suggestions();
        if !sugg.is_empty() {
            let idx = self.suggest_idx.min(sugg.len() - 1);
            match key.code {
                KeyCode::Down => {
                    self.suggest_idx = (idx + 1) % sugg.len();
                    return;
                }
                KeyCode::Up => {
                    self.suggest_idx = (idx + sugg.len() - 1) % sugg.len();
                    return;
                }
                KeyCode::Tab => {
                    self.input = sugg[idx].0.to_string();
                    self.cursor = self.char_len();
                    return;
                }
                KeyCode::Enter => {
                    self.input = sugg[idx].0.to_string();
                    self.cursor = self.char_len();
                    self.suggest_idx = 0;
                    self.submit(h);
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Enter => self.submit(h),
            KeyCode::Char('c') if ctrl => {
                if self.input.is_empty() {
                    let now = Instant::now();
                    if let Some(t) = self.last_ctrl_c {
                        if now.duration_since(t) < DOUBLE_PRESS_TIMEOUT {
                            self.should_quit = true;
                            return;
                        }
                    }
                    self.last_ctrl_c = Some(now);
                    self.push(
                        Kind::Notice,
                        "Press Ctrl+C again to exit".to_string(),
                    );
                } else {
                    self.last_ctrl_c = None;
                    self.input.clear();
                    self.cursor = 0;
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() {
                    let now = Instant::now();
                    if let Some(t) = self.last_ctrl_c {
                        if now.duration_since(t) < DOUBLE_PRESS_TIMEOUT {
                            self.should_quit = true;
                            return;
                        }
                    }
                    self.last_ctrl_c = Some(now);
                    self.push(
                        Kind::Notice,
                        "Press Ctrl+C again to exit".to_string(),
                    );
                }
            }
            KeyCode::PageUp => self.scroll_up(),
            KeyCode::PageDown => self.scroll_down(),
            KeyCode::Esc => {
                // Wait briefly: terminals often encode Alt+key as ESC prefix.
                self.esc_deadline = Some(Instant::now() + Duration::from_millis(50));
            }
            _ => {
                self.on_key_edit(key);
            }
        }
    }

    /// Composer editing keys shared by Idle and Busy (queueing) modes.
    fn on_key_edit(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Tab => self.complete(),
            KeyCode::Char('u') if ctrl => {
                let byte = self.byte_at(self.cursor);
                self.input.replace_range(0..byte, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => {
                let byte = self.byte_at(self.cursor);
                self.input.truncate(byte);
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.char_len(),
            KeyCode::Char('w') if ctrl => self.delete_word(),
            // macOS Option-as-Meta sends ESC b / ESC f for word motion.
            KeyCode::Char('b') if alt => self.cursor = self.prev_word(),
            KeyCode::Char('f') if alt => self.cursor = self.next_word(),
            // Unhandled Ctrl/Alt chords must not type their letter into the
            // composer (e.g. Ctrl+T would otherwise insert a stray 't').
            KeyCode::Char(c) if !ctrl && !alt => {
                self.last_ctrl_c = None;
                self.insert_char(caps_char(&key, c));
            }
            KeyCode::Backspace if alt => self.delete_word(),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let start = self.byte_at(self.cursor - 1);
                    let end = self.byte_at(self.cursor);
                    self.input.replace_range(start..end, "");
                    self.cursor -= 1;
                }
            }
            KeyCode::Delete if alt => self.delete_word_forward(),
            KeyCode::Delete => {
                if self.cursor < self.char_len() {
                    let start = self.byte_at(self.cursor);
                    let end = self.byte_at(self.cursor + 1);
                    self.input.replace_range(start..end, "");
                }
            }
            // macOS Cmd+Left/Right jump to start/end of line.
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SUPER) => self.cursor = 0,
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SUPER) => {
                self.cursor = self.char_len()
            }
            KeyCode::Left if alt || ctrl => self.cursor = self.prev_word(),
            KeyCode::Right if alt || ctrl => self.cursor = self.next_word(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                if self.cursor < self.char_len() {
                    self.cursor += 1;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.char_len(),
            KeyCode::Up => self.history_prev(),
            KeyCode::Down => self.history_next(),
            _ => {}
        }
    }

    fn prev_word(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn next_word(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let n = chars.len();
        let mut i = self.cursor;
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    fn delete_word(&mut self) {
        let i = self.prev_word();
        let start = self.byte_at(i);
        let end = self.byte_at(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor = i;
    }

    fn delete_word_forward(&mut self) {
        let i = self.next_word();
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(i);
        self.input.replace_range(start..end, "");
    }

    /// Tab completion: slash-commands at the start of the line, otherwise file
    /// paths for the token under the cursor (with or without a leading `@`).
    fn complete(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor;
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let token: String = chars[start..self.cursor].iter().collect();

        let (prefix_kept, search, candidates) = if start == 0 && token.starts_with('/') {
            let s = token.clone();
            let cands: Vec<String> = SLASH_COMMANDS
                .iter()
                .filter(|(c, _)| c.starts_with(&s))
                .map(|(c, _)| c.to_string())
                .collect();
            ("".to_string(), s, cands)
        } else {
            let at = token.starts_with('@');
            let raw = if at { &token[1..] } else { &token[..] };
            // Only path-complete plain tokens that look like paths.
            if !at && !raw.contains('/') && !raw.starts_with('.') && !raw.starts_with('~') {
                return;
            }
            let cands = complete_path(raw);
            ((if at { "@" } else { "" }).to_string(), raw.to_string(), cands)
        };

        if candidates.is_empty() {
            return;
        }
        let common = longest_common_prefix(&candidates);
        let completed = if common.len() > search.len() { common } else { search.clone() };
        // Replace the token region with prefix_kept + completed.
        let start_b = self.byte_at(start);
        let cur_b = self.byte_at(self.cursor);
        let mut replacement = format!("{prefix_kept}{completed}");
        if candidates.len() == 1 {
            // Unique: add a trailing '/' for dirs (already in candidate) or a space.
            if !replacement.ends_with('/') {
                replacement.push(' ');
            }
        }
        self.input.replace_range(start_b..cur_b, &replacement);
        self.cursor = start + replacement.chars().count();
        if candidates.len() > 1 {
            let shown: Vec<String> = candidates.iter().take(20).cloned().collect();
            self.push(Kind::Notice, shown.join("   "));
        }
    }

    fn history_prev(&mut self) {
        if self.hist_idx > 0 {
            if self.hist_idx == self.history.len() {
                self.pending = self.input.clone();
            }
            self.hist_idx -= 1;
            self.input = self.history[self.hist_idx].clone();
            self.cursor = self.char_len();
        }
    }

    fn history_next(&mut self) {
        if self.hist_idx < self.history.len() {
            self.hist_idx += 1;
            self.input = if self.hist_idx == self.history.len() {
                self.pending.clone()
            } else {
                self.history[self.hist_idx].clone()
            };
            self.cursor = self.char_len();
        }
    }

    pub fn mouse_scroll(&mut self, up: bool) {
        if up {
            self.follow = false;
            self.scroll = self.scroll.saturating_sub(3);
        } else {
            self.scroll = (self.scroll + 3).min(self.max_top);
            if self.scroll >= self.max_top {
                self.follow = true;
            }
        }
    }

    fn scroll_up(&mut self) {
        self.follow = false;
        let step = (self.view_h / 2).max(1);
        self.scroll = self.scroll.saturating_sub(step);
    }

    fn scroll_down(&mut self) {
        let step = (self.view_h / 2).max(1);
        self.scroll = (self.scroll + step).min(self.max_top);
        if self.scroll >= self.max_top {
            self.follow = true;
        }
    }

    fn submit(&mut self, h: &Handles) {
        let Some(text) = self.take_input() else { return };
        self.dispatch(text, h);
    }

    /// While the agent is busy, Enter queues the composer text to send after
    /// the current turn finishes.
    fn queue_input(&mut self) {
        let Some(text) = self.take_input() else { return };
        self.queued.push(text);
    }

    /// Pull the trimmed composer text (recording it in history), or None if empty.
    fn take_input(&mut self) -> Option<String> {
        let text = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        if text.is_empty() {
            return None;
        }
        self.history.push(text.clone());
        self.hist_idx = self.history.len();
        Some(text)
    }

    /// Run a slash command or send a user message to the worker.
    fn dispatch(&mut self, text: String, h: &Handles) {
        if let Some(cmd) = text.strip_prefix('/') {
            self.run_command(cmd.trim(), h);
            return;
        }
        self.push(Kind::User, text.clone());
        let (expanded, attached) = expand_attachments(&text);
        let (images, img_names) = extract_images(&text);
        let mut all = attached;
        all.extend(img_names);
        if !all.is_empty() {
            self.push(Kind::Notice, format!("attached: {}", all.join(", ")));
        }
        let _ = h.cmd_tx.send(WorkerCmd::User { text: expanded, images });
        self.mode = Mode::Busy;
    }

    fn run_command(&mut self, cmd: &str, h: &Handles) {
        let (name, arg) = match cmd.split_once(char::is_whitespace) {
            Some((a, b)) => (a, b.trim()),
            None => (cmd, ""),
        };
        match name {
            "exit" | "quit" | "q" => self.should_quit = true,
            "help" | "h" => self.show_help(),
            "clear" => self.transcript.clear(),
            "auto" => {
                let on = self.perm() != crate::agent::PERM_AUTO;
                self.perm.store(
                    if on { crate::agent::PERM_AUTO } else { crate::agent::PERM_ASK },
                    Ordering::Relaxed,
                );
                self.push(Kind::Notice, format!("auto-approve: {}", if on { "on" } else { "off" }));
            }
            "reset" => {
                let _ = h.cmd_tx.send(WorkerCmd::Reset);
            }
            "new" => {
                let _ = h.cmd_tx.send(WorkerCmd::New);
            }
            "compact" => {
                let _ = h.cmd_tx.send(WorkerCmd::Compact);
                self.mode = Mode::Busy;
            }
            "mcp" => {
                let _ = h.cmd_tx.send(WorkerCmd::ListMcp);
                self.mode = Mode::Busy;
            }
            "config" | "settings" => {
                self.mode = Mode::Settings { cursor: 0, edit: None };
            }
            "memory" => {
                let mem = std::fs::read_to_string(memory_path()).unwrap_or_default();
                let mem = mem.trim();
                self.push(Kind::Notice, if mem.is_empty() { "(no memories yet)".into() } else { mem.to_string() });
            }
            "model" | "models" => {
                if arg.is_empty() {
                    self.push(Kind::Notice, "fetching models...".to_string());
                    let _ = h.cmd_tx.send(WorkerCmd::ListModels);
                    self.mode = Mode::Busy;
                } else {
                    let id = match arg.parse::<usize>() {
                        Ok(n) if n >= 1 && n <= self.last_models.len() => self.last_models[n - 1].clone(),
                        _ => arg.to_string(),
                    };
                    let _ = h.cmd_tx.send(WorkerCmd::SetModel(id));
                }
            }
            "theme" => {
                if arg.is_empty() {
                    // Open the interactive picker, highlighting the current theme.
                    let cur = THEMES
                        .iter()
                        .position(|t| *t == self.palette.name)
                        .unwrap_or(0);
                    self.mode = Mode::ThemeSelect {
                        cursor: cur,
                        prev: self.palette.name.to_string(),
                    };
                } else {
                    let name = match arg.parse::<usize>() {
                        Ok(n) if n >= 1 && n <= THEMES.len() => THEMES[n - 1],
                        _ => arg,
                    };
                    let p = palette_by_name(name);
                    self.palette = p;
                    crate::config::Config::persist_theme(p.name);
                    self.push(Kind::Notice, format!("theme set to {}", p.name));
                }
            }
            "init" => {
                let prompt = "Explore the current project directory (list_files, then read key files like README, Cargo.toml, package.json, pyproject.toml, Makefile, etc.) and write a concise PICODE.md summarizing: what this project is, its structure, and how to build/run/test it. Keep it short and accurate.";
                self.push(Kind::User, "/init".to_string());
                let _ = h.cmd_tx.send(WorkerCmd::User { text: prompt.to_string(), images: Vec::new() });
                self.mode = Mode::Busy;
            }
            other => {
                self.push(Kind::ErrorK, format!("unknown command: /{other}  (try /help)"));
            }
        }
    }

    fn show_help(&mut self) {
        let lines = [
            "commands:",
            "  /model [id|n]  open the model picker, or set directly by id/number",
            "  /auto          toggle bypass-permissions (or Shift+Tab / Ctrl+P to cycle modes)",
            "  /reset         clear conversation context",
            "  /new           delete session, fresh start",
            "  /compact       summarize older turns to free context (auto at 80%)",
            "  /config        settings: provider, model, key, thinking, permissions, …",
            "  /mcp           list configured MCP servers and their tools",
            "  /memory        show persistent memory",
            "  /theme [n]     open theme picker, or switch directly (default, apple2, msdos)",
            "  /init          summarize this project into PICODE.md",
            "  /clear         clear the screen transcript",
            "  /help          show this help",
            "  /exit          quit picode",
            "input: @path attaches a file | Tab autocompletes commands & paths",
            "       typing while the agent works queues the message; Enter queues it",
            "keys: Enter send | Shift+Tab or Ctrl+P permission mode | Up/Down history | Alt+Left/Right word | Alt+Bksp/Del word | PgUp/PgDn scroll | Ctrl-L redraw | Ctrl-C twice to quit",
        ];
        for l in lines {
            self.push(Kind::Notice, l.to_string());
        }
    }

    /// A boot-screen banner: a PICODE block-art logo, version + tagline, and a
    /// bordered SYSTEM panel of status lines.
    pub fn banner(&mut self, width: u16, status: Vec<String>) {
        let w = (width as usize).saturating_sub(4).max(8);
        let rainbow = if is_16color_terminal() { APPLE_RAINBOW_16 } else { APPLE_RAINBOW };
        for bl in banner_lines(w, self.ascii, &status) {
            let (kind, color) = match bl.role {
                BRole::Art(i) => (
                    Kind::Banner,
                    Some(self.palette.mono_banner.unwrap_or(rainbow[i % rainbow.len()])),
                ),
                BRole::Version => (Kind::Banner, None),
                BRole::Tagline | BRole::Frame => (Kind::BannerDim, None),
                // Non-bold accent: BannerDim base recolored to the accent.
                BRole::Data => (Kind::BannerDim, Some(self.palette.accent)),
            };
            self.transcript.push(TLine { kind, text: bl.text, lead: false, color });
        }
        self.push_dim(String::new());
        self.after_push();
    }

    fn push_dim(&mut self, text: String) {
        self.transcript.push(TLine { kind: Kind::BannerDim, text, lead: false, color: None });
    }

    pub fn welcome(&mut self) {
        let model = self.model_info.clone();
        self.push(Kind::Notice, format!("picode ({model}) — type a task, or /help for commands"));
    }

    pub fn note(&mut self, s: String) {
        self.push(Kind::Notice, s);
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn busy(&self) -> bool {
        matches!(self.mode, Mode::Busy)
    }

    pub fn tick_spinner(&mut self) -> bool {
        self.spin_counter += 1;
        if self.spin_counter % 3 == 0 {
            self.spinner += 1;
            return true;
        }
        false
    }

    fn spin_frame(&self) -> &'static str {
        if self.ascii {
            SPIN_A[self.spinner % SPIN_A.len()]
        } else {
            SPIN_U[self.spinner % SPIN_U.len()]
        }
    }

    fn border_type(&self) -> BorderType {
        if self.glyphs.rounded {
            BorderType::Rounded
        } else {
            BorderType::Plain
        }
    }

    /// A comfortable mid-gray for secondary text. True RGB over SSH (where ANSI
    /// gray can render as pure white); plain gray on the 16-color console.
    fn dim_text(&self) -> Color {
        if self.ascii {
            Color::Gray
        } else {
            Color::Rgb(140, 140, 140)
        }
    }

    // ----------------------------------------------------------- render -----

    pub fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        let ch = self.composer_rows(area.width);
        let chunks = Layout::vertical([
            Constraint::Min(3),    // output
            Constraint::Length(1), // rule
            Constraint::Length(ch),// composer / busy / approval
            Constraint::Length(1), // rule
            Constraint::Length(1), // status: model · usage · ctx
            Constraint::Length(1), // status: permission mode
        ])
        .split(area);
        self.render_output(f, chunks[0]);
        self.render_rule(f, chunks[1]);
        self.render_inputline(f, pad1(chunks[2]));
        self.render_rule(f, chunks[3]);
        self.render_status1(f, pad1(chunks[4]));
        self.render_status2(f, pad1(chunks[5]));
    }

    /// Display value for one `/config` row.
    fn setting_value(&self, row: usize) -> String {
        let s = &self.settings;
        match row {
            0 => s.provider.clone(),
            1 => s.base_url.clone(),
            2 => self.model_info.clone(),
            3 => {
                if s.api_key.is_empty() {
                    "(not set)".to_string()
                } else {
                    let tail: String = s
                        .api_key
                        .chars()
                        .rev()
                        .take(4)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    // An env key silently overrides whatever is saved or
                    // edited here on the next launch — make that visible.
                    if s.key_from_env {
                        format!("…{tail} (from env — overrides the saved key)")
                    } else {
                        format!("…{tail}")
                    }
                }
            }
            4 => if s.thinking { "on".into() } else { "off".into() },
            5 => perm_name(self.perm()).to_string(),
            6 => if s.auto_commit { "on".into() } else { "off".into() },
            7 => self.palette.name.to_string(),
            8 => s.context_window.to_string(),
            _ => String::new(),
        }
    }

    fn prompt_str(&self) -> &'static str {
        self.palette.prompt.unwrap_or(self.glyphs.prompt)
    }

    fn prompt_w(&self) -> usize {
        self.prompt_str().chars().count()
    }

    /// Rows the composer text needs at this width (same in Idle and Busy).
    fn input_rows(&self, width: u16) -> u16 {
        let w = (width as usize).saturating_sub(2 + self.prompt_w()).max(1);
        let rows = self.char_len() / w + 1;
        rows.clamp(1, 5) as u16
    }

    fn composer_rows(&self, width: u16) -> u16 {
        match &self.mode {
            // Spinner line + queued messages + the live composer + slash palette.
            Mode::Busy => {
                1 + self.queued.len() as u16 + self.input_rows(width)
                    + self.slash_suggestions().len() as u16
            }
            // Description line + the (y)/(n)/(a) line below it.
            Mode::Approval(_) => 2,
            // Header line + one line per theme.
            Mode::ThemeSelect { .. } => THEMES.len() as u16 + 1,
            // Header line + one line per setting.
            Mode::Settings { .. } => SETTING_LABELS.len() as u16 + 1,
            // Header line + the visible window of choices (≥1 for "no matches").
            Mode::Select => {
                let n = self.picker.as_ref().map(|p| p.filtered().len()).unwrap_or(0);
                n.clamp(1, PICKER_VISIBLE) as u16 + 1
            }
            // Prompt line + masked-input line.
            Mode::Password { .. } => 2,
            // Wrapped question + answer line.
            Mode::Question { prompt } => {
                let w = (width as usize).saturating_sub(2).max(1);
                (prompt.chars().count() / w + 1).min(4) as u16 + 1
            }
            Mode::Idle => self.input_rows(width) + self.slash_suggestions().len() as u16,
        }
    }

    fn render_rule(&self, f: &mut Frame, area: Rect) {
        let ch = if self.ascii { "-" } else { "─" };
        let line = ch.repeat(area.width as usize);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(line, Style::default().fg(Color::DarkGray)))),
            area,
        );
    }

    fn render_output(&mut self, f: &mut Frame, area: Rect) {
        let title = Line::from(vec![
            Span::styled(" picode ", Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD)),
        ]);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(Style::default().fg(Color::DarkGray))
            .title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let lines = self.build_display(width);
        let total = lines.len();
        self.view_h = inner.height as usize;
        self.max_top = total.saturating_sub(self.view_h);
        let top = if self.follow { self.max_top } else { self.scroll.min(self.max_top) };
        self.scroll = top;
        let visible: Vec<Line> = lines.into_iter().skip(top).take(self.view_h).collect();
        f.render_widget(Paragraph::new(visible), inner);
    }

    fn build_display(&self, width: usize) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        for t in &self.transcript {
            render_tline(&mut out, t.kind, &t.text, t.lead, t.color, width, self.glyphs, &self.palette, self.single_width);
        }
        if !self.live.is_empty() {
            for (i, ln) in self.live.split('\n').enumerate() {
                render_tline(&mut out, Kind::Assistant, ln, i == 0, None, width, self.glyphs, &self.palette, self.single_width);
            }
        } else if !self.live_reasoning.is_empty() {
            for ln in self.live_reasoning.split('\n') {
                render_tline(&mut out, Kind::Reasoning, ln, true, None, width, self.glyphs, &self.palette, self.single_width);
            }
        }
        out
    }

    fn render_inputline(&self, f: &mut Frame, area: Rect) {
        match &self.mode {
            Mode::Idle => self.render_composer(f, area),
            Mode::Busy => {
                let mut lines = vec![Line::from(vec![
                    Span::styled(format!("{} ", self.spin_frame()), Style::default().fg(self.palette.accent)),
                    Span::styled("working... ", Style::default().fg(self.dim_text())),
                    Span::styled("(Esc to interrupt · Enter queues)", Style::default().fg(self.dim_text())),
                ])];
                let mark = if self.ascii { "->" } else { "↳" };
                for q in &self.queued {
                    lines.push(Line::from(Span::styled(
                        format!("  {mark} queued: {q}"),
                        Style::default().fg(self.dim_text()),
                    )));
                }
                let head_h = (lines.len() as u16).min(area.height);
                f.render_widget(Paragraph::new(lines), Rect { height: head_h, ..area });
                // Live composer below, so the next message can be typed now.
                if area.height > head_h {
                    let comp = Rect {
                        y: area.y + head_h,
                        height: area.height - head_h,
                        ..area
                    };
                    self.render_composer(f, comp);
                }
            }
            Mode::Approval(desc) => {
                // Keep the y/n/a hotkeys at the start of their own line so a long
                // description can't push them off the right edge of the screen.
                let desc_line = Line::from(vec![
                    Span::styled("approve ", Style::default().fg(Color::Yellow)),
                    Span::styled(desc.clone(), Style::default().add_modifier(Modifier::BOLD)),
                ]);
                let opts_line = Line::from(vec![
                    Span::styled("(Y)", Style::default().fg(Color::Green)),
                    Span::raw("es  "),
                    Span::styled("(N)", Style::default().fg(Color::Red)),
                    Span::raw("o  "),
                    Span::styled("(A)", Style::default().fg(self.palette.accent)),
                    Span::raw("lways"),
                ]);
                f.render_widget(Paragraph::new(vec![desc_line, opts_line]), area);
            }
            Mode::ThemeSelect { cursor, .. } => {
                let mut lines: Vec<Line> = Vec::new();
                lines.push(Line::from(vec![
                    Span::styled("select theme ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        "(up/down to preview, Enter to keep, Esc to cancel)",
                        Style::default().fg(self.dim_text()),
                    ),
                ]));
                let marker = if self.ascii { ">" } else { "▸" };
                for (i, t) in THEMES.iter().enumerate() {
                    let sel = i == *cursor;
                    let lead = if sel { marker } else { " " };
                    let style = if sel {
                        Style::default()
                            .fg(self.palette.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(self.dim_text())
                    };
                    lines.push(Line::from(vec![Span::styled(
                        format!("{lead} ({}) {t}", i + 1),
                        style,
                    )]));
                }
                f.render_widget(Paragraph::new(lines), area);
            }
            Mode::Select => {
                let Some(p) = &self.picker else { return };
                let filtered = p.filtered();
                let marker = if self.ascii { ">" } else { "▸" };
                let mut lines: Vec<Line> = Vec::new();
                let mut header = vec![
                    Span::styled(format!("{} ", p.title), Style::default().fg(Color::Yellow)),
                    Span::styled(
                        "(type to filter · Enter select · Esc cancel)",
                        Style::default().fg(self.dim_text()),
                    ),
                ];
                if !p.filter.is_empty() {
                    header.push(Span::styled(
                        format!("  filter: {}", p.filter),
                        Style::default().fg(self.palette.accent),
                    ));
                }
                lines.push(Line::from(header));
                if filtered.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  (no matches — Backspace to widen)",
                        Style::default().fg(self.dim_text()),
                    )));
                }
                let end = (p.scroll + PICKER_VISIBLE).min(filtered.len());
                for (row, &item_idx) in filtered[p.scroll..end].iter().enumerate() {
                    let i = p.scroll + row;
                    let sel = i == p.cursor;
                    let lead = if sel { marker } else { " " };
                    let cur = if Some(item_idx) == p.current { "  (current)" } else { "" };
                    let style = if sel {
                        Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(self.dim_text())
                    };
                    // Scroll hints on the window edges.
                    let more = if row == 0 && p.scroll > 0 {
                        if self.ascii { "  ^ more".to_string() } else { "  ↑ more".to_string() }
                    } else if i == end - 1 && end < filtered.len() {
                        format!("  +{} more", filtered.len() - end)
                    } else {
                        String::new()
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{lead} {}{cur}{more}", p.items[item_idx]),
                        style,
                    )));
                }
                f.render_widget(Paragraph::new(lines), area);
            }
            Mode::Settings { cursor, edit } => {
                let mut lines: Vec<Line> = Vec::new();
                let hint = if self.ascii {
                    "(up/down move - Enter/left/right change - Esc close)"
                } else {
                    "(↑/↓ move · Enter/←/→ change · Esc close)"
                };
                lines.push(Line::from(vec![
                    Span::styled("settings ", Style::default().fg(Color::Yellow)),
                    Span::styled(hint, Style::default().fg(self.dim_text())),
                ]));
                let marker = if self.ascii { ">" } else { "▸" };
                let block = if self.ascii { "#" } else { "▒" };
                for (i, label) in SETTING_LABELS.iter().enumerate() {
                    let sel = i == *cursor;
                    let lead = if sel { marker } else { " " };
                    let label_style = if sel {
                        Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(self.dim_text())
                    };
                    let mut spans = vec![Span::styled(format!("{lead} {label:<15}"), label_style)];
                    match edit {
                        Some(buf) if sel => {
                            // api key edits render masked, like the sudo prompt.
                            let shown = if i == 3 {
                                "•".repeat(buf.chars().count())
                            } else {
                                buf.clone()
                            };
                            spans.push(Span::styled(shown, Style::default().fg(self.palette.accent)));
                            spans.push(Span::styled(block, Style::default().fg(self.palette.accent)));
                        }
                        _ => {
                            spans.push(Span::styled(
                                self.setting_value(i),
                                Style::default().fg(if sel { self.palette.accent } else { self.dim_text() }),
                            ));
                        }
                    }
                    lines.push(Line::from(spans));
                }
                f.render_widget(Paragraph::new(lines), area);
            }
            Mode::Question { prompt } => {
                let block = if self.ascii { "#" } else { "▒" };
                let mut lines: Vec<Line> = textwrap::wrap(prompt, (area.width as usize).max(1))
                    .into_iter()
                    .map(|piece| {
                        Line::from(Span::styled(
                            piece.into_owned(),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ))
                    })
                    .collect();
                // A very long question must not push the answer line (and its
                // cursor) below the area — clip the question, keep the input.
                lines.truncate((area.height as usize).saturating_sub(1));
                lines.push(Line::from(vec![
                    Span::styled(self.prompt_str(), Style::default().fg(self.palette.accent)),
                    Span::raw(self.q_input.clone()),
                    Span::styled(block, Style::default().fg(self.palette.accent)),
                    Span::styled(
                        "  (Enter to answer · Esc to decline)",
                        Style::default().fg(self.dim_text()),
                    ),
                ]));
                f.render_widget(Paragraph::new(lines), area);
            }
            Mode::Password { prompt } => {
                let label = if prompt.is_empty() { "[sudo] password:" } else { prompt.as_str() };
                let mask = if self.ascii { '*' } else { '•' };
                let dots: String = std::iter::repeat(mask).take(self.pw_input.chars().count()).collect();
                let prompt_line = Line::from(vec![Span::styled(
                    label.to_string(),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )]);
                let input_line = Line::from(vec![
                    Span::styled(dots, Style::default().fg(self.palette.accent)),
                    Span::styled(
                        "  (Enter to submit · Esc to cancel · hidden, not stored)",
                        Style::default().fg(self.dim_text()),
                    ),
                ]);
                f.render_widget(Paragraph::new(vec![prompt_line, input_line]), area);
            }
        }
    }

    fn render_composer(&self, f: &mut Frame, inner: Rect) {
        let pw = self.prompt_w();
        let w = (inner.width as usize).saturating_sub(pw).max(1);
        let chars: Vec<char> = self.input.chars().collect();
        let empty = chars.is_empty();
        let total_rows = chars.len() / w + 1;
        let cur_row = self.cursor / w;
        let cur_col = self.cursor % w;
        let max_rows = inner.height as usize;
        let start_row = if cur_row >= max_rows { cur_row - max_rows + 1 } else { 0 };

        let prompt = self.prompt_str();
        let cursor = self.palette.cursor;
        let block = if self.ascii { "#" } else { "▒" };
        let accent = Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD);
        let cont = " ".repeat(pw);

        let mut lines: Vec<Line> = Vec::new();
        for r in start_row..(start_row + max_rows).min(total_rows.max(1)) {
            let prefix = if r == 0 { Span::styled(prompt, accent) } else { Span::raw(cont.clone()) };
            let row: Vec<char> = chars.iter().skip(r * w).take(w).cloned().collect();
            let mut spans = vec![prefix];
            if r == cur_row && cursor != CursorKind::Caret {
                let before: String = row.iter().take(cur_col).collect();
                let after: String = row.iter().skip(cur_col + 1).collect();
                spans.push(Span::raw(before));
                match cursor {
                    CursorKind::Block => spans.push(Span::styled(block, accent)),
                    _ => {
                        let at = row.get(cur_col).map(|c| c.to_string()).unwrap_or_else(|| " ".into());
                        spans.push(Span::styled(at, Style::default().add_modifier(Modifier::REVERSED)));
                    }
                }
                spans.push(Span::raw(after));
            } else {
                spans.push(Span::raw(row.iter().collect::<String>()));
            }
            if empty && r == 0 {
                let hint = if matches!(self.mode, Mode::Busy) {
                    "  type to queue the next message"
                } else {
                    "  describe a task · @file to attach · /help"
                };
                spans.push(Span::styled(hint, Style::default().fg(self.dim_text())));
            }
            lines.push(Line::from(spans));
        }
        // The `/` command palette, under the input.
        let sugg = self.slash_suggestions();
        if !sugg.is_empty() {
            let idx = self.suggest_idx.min(sugg.len() - 1);
            let marker = if self.ascii { ">" } else { "▸" };
            for (i, (name, desc)) in sugg.iter().enumerate() {
                let sel = i == idx;
                let name_style = if sel {
                    Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(self.dim_text())
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} {name:<9} ", if sel { marker } else { " " }),
                        name_style,
                    ),
                    Span::styled(desc.to_string(), Style::default().fg(self.dim_text())),
                ]));
            }
        }
        f.render_widget(Paragraph::new(lines), inner);
        if cursor == CursorKind::Caret {
            let cx = inner.x + pw as u16 + cur_col as u16;
            let cy = inner.y + (cur_row - start_row) as u16;
            f.set_cursor_position(Position::new(cx.min(inner.x + inner.width - 1), cy));
        }
    }

    /// Status line 1: model · session usage/cost · context bar · balance.
    fn render_status1(&self, f: &mut Frame, area: Rect) {
        let gray = Style::default().fg(self.dim_text());
        let sep = || Span::styled(if self.ascii { "  |  " } else { "  │  " }, Style::default().fg(Color::DarkGray));
        let mut spans = vec![Span::styled(
            self.model_info.clone(),
            Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD),
        )];
        if self.settings.thinking {
            spans.push(Span::styled(" think", gray));
        }

        let tokens = self.sess_prompt + self.sess_completion;
        if tokens > 0 {
            let cost = self.sess_prompt as f64 / 1e6 * self.price_in
                + self.sess_completion as f64 / 1e6 * self.price_out;
            spans.push(sep());
            spans.push(Span::styled(format!("{} · {} tok", fmt_cost(cost), humanize(tokens)), gray));
        }

        spans.push(sep());
        spans.push(Span::styled("ctx ", gray));
        let frac = self.last_prompt_tokens as f64 / self.ctx_limit as f64;
        spans.extend(bar(frac, 8));
        spans.push(Span::styled(
            format!(" {}%", (frac.clamp(0.0, 1.0) * 100.0).round() as u32),
            gray,
        ));

        if let Some(b) = &self.balance {
            spans.push(sep());
            spans.push(Span::styled(format!("bal {b}"), gray));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Status line 2: permission mode + hint.
    fn render_status2(&self, f: &mut Frame, area: Rect) {
        let (glyph, text, color) = match self.perm() {
            crate::agent::PERM_AUTO => (
                if self.ascii { ">>" } else { "▶▶" },
                "bypass permissions on",
                Color::Red,
            ),
            crate::agent::PERM_PLAN => (
                if self.ascii { "#" } else { "◆" },
                "plan mode · read-only",
                Color::Cyan,
            ),
            _ => (if self.ascii { "*" } else { "●" }, "ask before edits", Color::Green),
        };
        let spans = vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(text, Style::default().fg(color)),
            Span::styled("  (shift+tab/ctrl+p to cycle)", Style::default().fg(self.dim_text())),
            Span::styled(format!("   {}", self.cwd), Style::default().fg(self.dim_text())),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

/// Apply Caps Lock to a typed character. Under the Kitty keyboard protocol
/// (pushed in setup_terminal) the terminal reports the BASE key — 'a' even
/// with Caps Lock on — plus a CAPS_LOCK state flag, leaving case to the app.
/// Caps alone uppercases letters; Caps+Shift lowercases them, like a real
/// keyboard. Legacy terminals pre-shift the char and set no state flag, so
/// this is a no-op there.
fn caps_char(key: &KeyEvent, c: char) -> char {
    if !key.state.contains(KeyEventState::CAPS_LOCK) || !c.is_alphabetic() {
        return c;
    }
    // Only simple 1:1 case mappings; multi-char expansions (ß → SS) keep the
    // original — which matches real keyboards, where Caps Lock doesn't
    // affect such keys.
    let mapped: Vec<char> = if key.modifiers.contains(KeyModifiers::SHIFT) {
        c.to_lowercase().collect()
    } else {
        c.to_uppercase().collect()
    };
    if mapped.len() == 1 {
        mapped[0]
    } else {
        c
    }
}

fn perm_name(p: u8) -> &'static str {
    match p {
        crate::agent::PERM_AUTO => "bypass",
        crate::agent::PERM_PLAN => "plan",
        _ => "ask",
    }
}

/// Inset a rect by one column on each side for a small left/right margin.
fn pad1(r: Rect) -> Rect {
    Rect { x: r.x + 1, width: r.width.saturating_sub(2), ..r }
}

/// A solid mini progress bar (green→yellow→red as it fills).
fn bar(frac: f64, width: usize) -> Vec<Span<'static>> {
    let frac = frac.clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let fill = if frac < 0.8 {
        Color::Green
    } else if frac < 0.95 {
        Color::Yellow
    } else {
        Color::Red
    };
    let mut v = Vec::new();
    if filled > 0 {
        v.push(Span::styled(" ".repeat(filled), Style::default().bg(fill)));
    }
    if filled < width {
        v.push(Span::styled(" ".repeat(width - filled), Style::default().bg(Color::DarkGray)));
    }
    v
}

fn humanize(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn fmt_cost(c: f64) -> String {
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
fn complete_path(prefix: &str) -> Vec<String> {
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

fn longest_common_prefix(items: &[String]) -> String {
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
fn clean_text(text: &str, single_width: bool) -> std::borrow::Cow<'_, str> {
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
fn render_tline(
    out: &mut Vec<Line<'static>>,
    kind: Kind,
    text: &str,
    lead: bool,
    color: Option<Color>,
    width: usize,
    g: Glyphs,
    p: &Palette,
    single_width: bool,
) {
    let text = &*clean_text(text, single_width);
    let (indent, glyph) = layout(kind, g);
    let (mut base, mut glyph_style) = colors(kind, p);
    if let Some(c) = color {
        base = base.fg(c);
    }
    // Highlight the user's own prompts with a full-width gray band so they
    // stand out when scrolling back through output.
    let bg = if kind == Kind::User {
        Some(if g.rounded { Color::Rgb(48, 48, 48) } else { Color::DarkGray })
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
fn layout(kind: Kind, g: Glyphs) -> (usize, &'static str) {
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

/// (text style, glyph style) per line kind, from the active theme palette.
fn colors(kind: Kind, p: &Palette) -> (Style, Style) {
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

/// Current terminal width in columns (for sizing the launch banner).
pub fn term_width() -> u16 {
    ratatui::crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80)
}

#[cfg(test)]
mod tests {
    use super::clean_text;

    #[test]
    fn clean_text_passes_plain_text_through_borrowed() {
        assert!(matches!(clean_text("hello world", true), std::borrow::Cow::Borrowed(_)));
        assert!(matches!(clean_text("héllo ❯ wörld", false), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn clean_text_strips_escapes_and_expands_tabs() {
        // ANSI color codes from tool output must not reach the terminal.
        assert_eq!(clean_text("\x1b[31mred\x1b[0m", false), "[31mred[0m");
        assert_eq!(clean_text("a\tb", false), "a    b");
        assert_eq!(clean_text("a\rb\x07", false), "ab");
    }

    #[test]
    fn clean_text_ascii_replaces_non_single_width() {
        // Wide chars (emoji/CJK) misrender on the framebuffer console.
        assert_eq!(clean_text("ok 🚀 漢", true), "ok ? ?");
        // Single-width non-ASCII is fine: the console shows a 1-cell fallback.
        assert_eq!(clean_text("héllo", true), "héllo");
        // Unicode terminals keep wide chars as-is.
        assert_eq!(clean_text("🚀\t", false), "🚀    ");
    }
}

/// The UI event loop. Owns the terminal; returns when the user quits.
pub fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    ui_rx: Receiver<UiEvent>,
    h: &Handles,
) -> std::io::Result<()> {
    // Paint once up front: the Pi's local console sends no initial resize
    // event, so without this the screen stays blank until the first keypress.
    terminal.draw(|f| app.render(f))?;
    loop {
        let mut dirty = false;
        while let Ok(ev) = ui_rx.try_recv() {
            app.handle_event(ev, h);
            dirty = true;
        }
        if app.should_quit() {
            break;
        }
        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                // Press and Repeat (held-key autorepeat). The Kitty protocol's
                // REPORT_EVENT_TYPES flag also emits Release events — ignore those.
                Event::Key(k)
                    if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    app.on_key(k, h);
                    dirty = true;
                }
                Event::Paste(s) => {
                    app.on_paste(s);
                    dirty = true;
                }
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => {
                        app.mouse_scroll(true);
                        dirty = true;
                    }
                    MouseEventKind::ScrollDown => {
                        app.mouse_scroll(false);
                        dirty = true;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
        // Flush an expired ESC timeout (no key arrived after ESC).
        if let Some(deadline) = app.esc_deadline {
            if Instant::now() >= deadline {
                app.esc_deadline = None;
                app.do_esc(h);
                dirty = true;
            }
        }
        if app.busy() && app.tick_spinner() {
            dirty = true;
        }
        if app.force_clear {
            app.force_clear = false;
            terminal.clear()?;
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| app.render(f))?;
        }
    }
    Ok(())
}

/// Enter alt-screen + raw mode with bracketed paste; returns a ready Terminal.
pub fn setup_terminal() -> std::io::Result<Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>>
{
    let mut term = ratatui::init();
    let _ = execute!(std::io::stdout(), event::EnableBracketedPaste, event::EnableMouseCapture);
    // Ask the terminal to report modified keys unambiguously (Kitty keyboard
    // protocol). Without this, terminals like Warp drop the Option/Alt modifier
    // on Backspace in full-screen apps and send a bare 0x7f, so Option+Backspace
    // is indistinguishable from a plain Backspace. With DISAMBIGUATE_ESCAPE_CODES
    // it arrives as Alt+Backspace (and Option+Delete as Alt+Delete), which the
    // composer already turns into word-delete. Terminals that don't support it
    // ignore the push, so this is safe everywhere.
    if matches!(
        ratatui::crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    ) {
        // Warp only disambiguates a *modified* Backspace (so Option+Backspace
        // arrives as Alt+Backspace rather than a bare 0x7f) when ALL keys are
        // reported as escape codes — DISAMBIGUATE_ESCAPE_CODES alone is not
        // enough. Push the full flag set; crossterm decodes the resulting CSI-u
        // sequences back to ordinary KeyCodes (see the Shift+Tab and Repeat
        // handling that this enables in the event loop / on_key).
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::all())
        );
    }
    term.clear()?;
    Ok(term)
}

pub fn restore_terminal(console: bool) {
    use ratatui::crossterm::cursor::MoveTo;
    use ratatui::crossterm::terminal::{Clear, ClearType};
    let mut out = std::io::stdout();
    // Undo the Kitty keyboard-protocol push from setup_terminal. Harmless if we
    // never pushed (the terminal just pops an empty stack entry).
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, event::DisableMouseCapture, event::DisableBracketedPaste);
    ratatui::restore();
    // The Linux framebuffer console ignores the alternate screen, so the last
    // TUI frame is left on screen after restore. Wipe it for a clean exit.
    // (Over SSH the alt-screen already restored the shell — don't clear that.)
    if console {
        let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
    }
}

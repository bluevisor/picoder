//! Full-screen ratatui interface: a scrolling transcript, a multi-line
//! composer, and a status bar. Runs on the UI thread; the agent runs on a
//! worker thread and feeds this UI through a channel.

use crate::agent::{ApprovalResponse, Handles, UiEvent, WorkerCmd};
use crate::config::memory_path;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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

/// Choose a glyph set. ASCII for the Linux console / dumb terminals, or when
/// forced via PICODE_ASCII=1; Unicode otherwise (or forced via PICODE_UNICODE=1).
pub fn detect_ascii() -> bool {
    if std::env::var("PICODE_UNICODE").is_ok() {
        return false;
    }
    if std::env::var("PICODE_ASCII").is_ok() {
        return true;
    }
    match std::env::var("TERM").as_deref() {
        Ok("linux") | Ok("dumb") | Ok("vt100") | Ok("") | Err(_) => true,
        _ => false,
    }
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

const SLASH_COMMANDS: &[&str] = &[
    "/model", "/auto", "/reset", "/memory", "/theme", "/init", "/clear", "/help", "/exit",
];

/// Heavy block-letter "PICODE" for capable terminals.
const ART_UNICODE: [&str; 6] = [
    "██████╗ ██╗ ██████╗ ██████╗ ██████╗ ███████╗",
    "██╔══██╗██║██╔════╝██╔═══██╗██╔══██╗██╔════╝",
    "██████╔╝██║██║     ██║   ██║██║  ██║█████╗  ",
    "██╔═══╝ ██║██║     ██║   ██║██║  ██║██╔══╝  ",
    "██║     ██║╚██████╗╚██████╔╝██████╔╝███████╗",
    "╚═╝     ╚═╝ ╚═════╝ ╚═════╝ ╚═════╝ ╚══════╝",
];

/// 5x4 ASCII glyphs (P I C O D E), assembled at runtime to guarantee alignment.
const ART_GLYPHS: [[&str; 5]; 6] = [
    ["####", "#  #", "####", "#   ", "#   "], // P
    ["####", " ## ", " ## ", " ## ", "####"], // I
    ["####", "#   ", "#   ", "#   ", "####"], // C
    ["####", "#  #", "#  #", "#  #", "####"], // O
    ["### ", "#  #", "#  #", "#  #", "### "], // D
    ["####", "#   ", "### ", "#   ", "####"], // E
];

fn ascii_art() -> Vec<String> {
    (0..5)
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
    let mark = if ascii { "=#" } else { "──■" };
    let art = banner_art(w, ascii);
    let artw = art.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let pad = " ".repeat(w.saturating_sub(artw) / 2);
    let rainbow = if ascii { APPLE_RAINBOW_16 } else { APPLE_RAINBOW };
    let reset = "\x1b[0m";
    let acc = ansi_fg(p.accent);

    let mut out = format!("{acc}{mark} PICODE v1.0{reset}\n\n");
    for (i, line) in art.iter().enumerate() {
        let c = p.mono_banner.unwrap_or(rainbow[i % rainbow.len()]);
        out.push_str(&format!("{}{pad}{line}{reset}\n", ansi_fg(c)));
    }
    out.push_str(&format!("\n{acc}{mark} SYSTEM STATUS{reset}\n"));
    for line in status {
        out.push_str(&format!(" {acc}{line}{reset}\n"));
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
    mode: Mode,
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
            mode: Mode::Idle,
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

    pub fn handle_event(&mut self, ev: UiEvent) {
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
            UiEvent::ModelList(ids) => {
                self.flush_live();
                self.last_models = ids.clone();
                self.push(Kind::Notice, format!("available models ({}):", self.model_info));
                for (i, id) in ids.iter().enumerate() {
                    let cur = if id == self.model_short() { "  <- current" } else { "" };
                    self.push(Kind::Notice, format!("  {:>2}) {id}{cur}", i + 1));
                }
                self.push(Kind::Notice, "set with: /model <id>  or  /model <number>".to_string());
            }
            UiEvent::ModelChanged(m) => {
                self.model_info = m;
            }
            UiEvent::Usage { prompt, completion } => {
                self.last_prompt_tokens = prompt;
                self.sess_prompt += prompt as u64;
                self.sess_completion += completion as u64;
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
                self.mode = Mode::Idle;
            }
        }
    }

    fn model_short(&self) -> &str {
        self.model_info.split(':').nth(1).unwrap_or(&self.model_info)
    }

    pub fn on_paste(&mut self, s: String) {
        if let Mode::Idle = self.mode {
            for c in s.chars() {
                if c == '\n' || c == '\r' {
                    self.insert_char(' ');
                } else {
                    self.insert_char(c);
                }
            }
        }
    }

    fn insert_char(&mut self, c: char) {
        let byte = self.byte_at(self.cursor);
        self.input.insert(byte, c);
        self.cursor += 1;
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
        // Shift+Tab cycles permission mode in any state. Under the Kitty
        // keyboard protocol it arrives as Tab+SHIFT rather than BackTab.
        if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.cycle_perm();
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
                    Mode::Busy => self.on_key_busy(alt_key, h),
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
            Mode::Busy => self.on_key_busy(key, h),
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
            Mode::Idle => {
                self.input.clear();
                self.cursor = 0;
            }
        }
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

    fn on_key_busy(&mut self, key: KeyEvent, h: &Handles) {
        match key.code {
            KeyCode::Esc => h.shared.cancel.store(true, Ordering::Relaxed),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                h.shared.cancel.store(true, Ordering::Relaxed)
            }
            KeyCode::PageUp => self.scroll_up(),
            KeyCode::PageDown => self.scroll_down(),
            _ => {}
        }
    }

    fn on_key_idle(&mut self, key: KeyEvent, h: &Handles) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => self.submit(h),
            KeyCode::Tab => self.complete(),
            KeyCode::Char('c') if ctrl => {
                if self.input.is_empty() {
                    self.should_quit = true;
                } else {
                    self.input.clear();
                    self.cursor = 0;
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() {
                    self.should_quit = true;
                }
            }
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
            KeyCode::Char('l') if ctrl => self.follow = true,
            // macOS Option-as-Meta sends ESC b / ESC f for word motion.
            KeyCode::Char('b') if alt => self.cursor = self.prev_word(),
            KeyCode::Char('f') if alt => self.cursor = self.next_word(),
            KeyCode::Char(c) => self.insert_char(c),
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
            KeyCode::PageUp => self.scroll_up(),
            KeyCode::PageDown => self.scroll_down(),
            KeyCode::Esc => {
                // Wait briefly: terminals often encode Alt+key as ESC prefix.
                self.esc_deadline = Some(Instant::now() + Duration::from_millis(50));
            }
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
                .filter(|c| c.starts_with(&s))
                .map(|c| c.to_string())
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
        let text = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        if text.is_empty() {
            return;
        }
        self.history.push(text.clone());
        self.hist_idx = self.history.len();
        if let Some(cmd) = text.strip_prefix('/') {
            self.run_command(cmd.trim(), h);
            return;
        }
        self.push(Kind::User, text.clone());
        let (expanded, attached) = expand_attachments(&text);
        if !attached.is_empty() {
            self.push(Kind::Notice, format!("attached: {}", attached.join(", ")));
        }
        let _ = h.cmd_tx.send(WorkerCmd::User(expanded));
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
                let _ = h.cmd_tx.send(WorkerCmd::User(prompt.to_string()));
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
            "  /model [id|n]  list models, or set by id/number",
            "  /auto          toggle bypass-permissions (or Shift+Tab to cycle modes)",
            "  /reset         clear conversation context",
            "  /memory        show persistent memory",
            "  /theme [n]     open theme picker, or switch directly (default, apple2, msdos)",
            "  /init          summarize this project into PICODE.md",
            "  /clear         clear the screen transcript",
            "  /help          show this help",
            "  /exit          quit picode",
            "input: @path attaches a file | Tab autocompletes commands & paths",
            "keys: Enter send | Shift+Tab permission mode | Up/Down history | Alt+Left/Right word | Alt+Bksp/Del word | PgUp/PgDn scroll | Ctrl-C quit",
        ];
        for l in lines {
            self.push(Kind::Notice, l.to_string());
        }
    }

    /// A boot-screen banner: a PICODE block-art logo and a system-status line.
    pub fn banner(&mut self, width: u16, status: Vec<String>) {
        let w = (width as usize).saturating_sub(4).max(8);
        let mark = if self.ascii { "=#" } else { "──■" };
        let art = banner_art(w, self.ascii);
        let artw = art.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        let pad = " ".repeat(w.saturating_sub(artw) / 2);
        let rainbow = if self.ascii { APPLE_RAINBOW_16 } else { APPLE_RAINBOW };

        self.push_dim(format!("{mark} PICODE v1.0"));
        self.push_dim(String::new());
        for (i, line) in art.iter().enumerate() {
            let color = self.palette.mono_banner.unwrap_or(rainbow[i % rainbow.len()]);
            self.transcript.push(TLine {
                kind: Kind::Banner,
                text: format!("{pad}{line}"),
                lead: false,
                color: Some(color),
            });
        }
        self.push_dim(String::new());
        self.push_dim(format!("{mark} SYSTEM STATUS"));
        for line in status {
            self.transcript.push(TLine {
                kind: Kind::Banner,
                text: format!(" {line}"),
                lead: false,
                color: Some(self.palette.accent),
            });
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

    fn prompt_str(&self) -> &'static str {
        self.palette.prompt.unwrap_or(self.glyphs.prompt)
    }

    fn prompt_w(&self) -> usize {
        self.prompt_str().chars().count()
    }

    fn composer_rows(&self, width: u16) -> u16 {
        match &self.mode {
            Mode::Busy => 1,
            // Description line + the (y)/(n)/(a) line below it.
            Mode::Approval(_) => 2,
            // Header line + one line per theme.
            Mode::ThemeSelect { .. } => THEMES.len() as u16 + 1,
            Mode::Idle => {
                let w = (width as usize).saturating_sub(2 + self.prompt_w()).max(1);
                let rows = self.char_len() / w + 1;
                rows.clamp(1, 5) as u16
            }
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
            render_tline(&mut out, t.kind, &t.text, t.lead, t.color, width, self.glyphs, &self.palette);
        }
        if !self.live.is_empty() {
            for (i, ln) in self.live.split('\n').enumerate() {
                render_tline(&mut out, Kind::Assistant, ln, i == 0, None, width, self.glyphs, &self.palette);
            }
        } else if !self.live_reasoning.is_empty() {
            for ln in self.live_reasoning.split('\n') {
                render_tline(&mut out, Kind::Reasoning, ln, true, None, width, self.glyphs, &self.palette);
            }
        }
        out
    }

    fn render_inputline(&self, f: &mut Frame, area: Rect) {
        match &self.mode {
            Mode::Idle => self.render_composer(f, area),
            Mode::Busy => {
                let line = Line::from(vec![
                    Span::styled(format!("{} ", self.spin_frame()), Style::default().fg(self.palette.accent)),
                    Span::styled("working... ", Style::default().fg(self.dim_text())),
                    Span::styled("(Esc to interrupt)", Style::default().fg(self.dim_text())),
                ]);
                f.render_widget(Paragraph::new(line), area);
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
                spans.push(Span::styled(
                    "  describe a task · @file to attach · /help",
                    Style::default().fg(self.dim_text()),
                ));
            }
            lines.push(Line::from(spans));
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
            Span::styled("  (shift+tab to cycle)", Style::default().fg(self.dim_text())),
            Span::styled(format!("   {}", self.cwd), Style::default().fg(self.dim_text())),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
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

/// Expand `@path` tokens into appended fenced file contents for the model.
/// Returns (text_to_send, attached_paths). Display text keeps the raw `@path`.
pub fn expand_attachments(text: &str) -> (String, Vec<String>) {
    let mut attached = Vec::new();
    let mut blocks = String::new();
    for tok in text.split_whitespace() {
        let Some(p) = tok.strip_prefix('@') else { continue };
        // Drop trailing sentence punctuation so "@calc.py?" resolves to calc.py.
        let p = p.trim_end_matches(|c: char| "?.,;:!)]}'\"".contains(c));
        if p.is_empty() {
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
) {
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
            app.handle_event(ev);
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

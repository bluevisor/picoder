//! UI types: glyphs, line kinds, modes, pickers, and config snapshots.

use ratatui::crossterm::event::KeyEvent;
use ratatui::style::Color;
use std::time::Duration;

pub const SPIN_U: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const SPIN_A: [&str; 4] = ["|", "/", "-", "\\"];
pub const MAX_TRANSCRIPT: usize = 4000;

#[derive(Clone, Copy, PartialEq)]
pub enum CursorKind {
    Caret,   // hardware caret
    Reverse, // reverse-video over the char (Claude-style)
    Block,   // a solid ▒/# block (Apple ][ / DOS)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
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
    #[allow(dead_code)] Code,
    #[allow(dead_code)] Heading,
    #[allow(dead_code)] Banner,
    #[allow(dead_code)] BannerDim,
}

/// Glyphs vary by terminal: the Pi's framebuffer console (TERM=linux) lacks
/// the fancy Unicode used over SSH, so we fall back to ASCII there.
#[derive(Clone, Copy)]
pub struct Glyphs {
    pub user: &'static str,
    pub assistant: &'static str,
    pub tool: &'static str,
    pub result: &'static str,
    pub error: &'static str,
    pub prompt: &'static str,
    pub rounded: bool,
}

pub const GLYPHS_U: Glyphs = Glyphs {
    user: "❯ ",
    assistant: "● ",
    tool: "⏺ ",
    result: "⎿ ",
    error: "✗ ",
    prompt: "❯ ",
    rounded: true,
};
pub const GLYPHS_A: Glyphs = Glyphs {
    user: "> ",
    assistant: "* ",
    tool: "* ",
    result: "> ",
    error: "x ",
    prompt: "> ",
    rounded: false,
};

/// Choose a glyph set. ASCII for dumb terminals, or when forced via
/// PICODER_ASCII=1; Unicode otherwise (or forced via PICODER_UNICODE=1).
/// TERM=linux stays Unicode: the framebuffer console renders the glyphs we
/// use at single-cell width (set PICODER_ASCII=1 on consoles whose font
/// doesn't cover them).
pub fn detect_ascii() -> bool {
    if std::env::var("PICODER_UNICODE").is_ok() {
        return false;
    }
    if std::env::var("PICODER_ASCII").is_ok() {
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

/// Slash commands with the one-line description shown in the `/` palette.
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/model", "pick a model from the provider's list"),
    ("/login", "sign in to a subscription (anthropic, openai, google)"),
    ("/new", "clear conversation and session"),
    ("/config", "settings: provider, model, key, thinking, …"),
    ("/compact", "summarize older turns to free context"),
    ("/reset", "clear conversation context"),
    ("/auto", "toggle bypass-permissions"),
    ("/mcp", "list MCP servers and their tools"),
    ("/memory", "show persistent memory"),
    ("/theme", "open the theme picker"),
    ("/init", "summarize this project into PICODER.md"),
    ("/clear", "clear the screen transcript"),
    ("/help", "show help"),
    ("/exit", "quit picoder"),
];

/// Most suggestions shown under the composer for a `/` prefix.
pub const MAX_SUGGEST: usize = 8;
/// Window within which a second Ctrl+C/Ctrl+D exits (Claude Code style).
pub const DOUBLE_PRESS_TIMEOUT: Duration = Duration::from_secs(2);

/// Rows of the `/config` panel, in display order.
pub const SETTING_LABELS: &[&str] = &[
    "provider",
    "base url",
    "model",
    "api key",
    "auth mode",
    "thinking",
    "permissions",
    "auto-commit",
    "theme",
    "context window",
    "max tool calls",
];

#[derive(Clone, Copy)]
pub enum BannerColor {
    /// A fixed color (no palette reactivity).
    #[allow(dead_code)] Fixed(Color),
    /// Rainbow art row: uses `palette.mono_banner` when set, else
    /// the Apple rainbow at the given index.
    Rainbow(usize),
    /// Uses `palette.accent`.
    Accent,
}

pub struct TLine {
    pub kind: Kind,
    pub text: String,
    /// First line of a block — shows the glyph; later lines align under it.
    pub lead: bool,
    /// Optional per-line fg override that resolves against the current palette.
    /// Banner colors are deferred so theme switches update the logo in real time.
    pub color: Option<BannerColor>,
}

#[derive(Clone, PartialEq)]
pub enum Mode {
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
pub enum PickAction {
    /// Switch model to the chosen id.
    #[allow(dead_code)] Model,
    /// Start the subscription OAuth flow for the chosen provider.
    Login,
}

/// State for `Mode::Select`: a filterable, scrollable list of choices.
pub struct Picker {
    pub title: String,
    pub items: Vec<String>,
    /// Item highlighted as the current value (shown with a marker).
    pub current: Option<usize>,
    pub filter: String,
    /// Cursor within the *filtered* view.
    pub cursor: usize,
    pub scroll: usize,
    pub action: PickAction,
}

/// Visible rows of a Select picker before it scrolls.
pub const PICKER_VISIBLE: usize = 8;

impl Picker {
    /// Indices of items matching the filter (case-insensitive substring).
    pub fn filtered(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.items.len()).collect();
        }
        let f = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| self.items[i].to_lowercase().contains(&f))
            .collect()
    }

    /// Keep the cursor inside the filtered list and the scroll window.
    pub fn clamp(&mut self, filtered_len: usize) {
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
    pub settings: crate::config::Config,
}

/// Apply Caps Lock to a typed character. Under the Kitty keyboard protocol
/// (pushed in setup_terminal) the terminal reports the BASE key — 'a' even
/// with Caps Lock on — plus a CAPS_LOCK state flag, leaving case to the app.
/// Caps alone uppercases letters; Caps+Shift lowercases them, like a real
/// keyboard. Legacy terminals pre-shift the char and set no state flag, so
/// this is a no-op there.
pub fn caps_char(key: &KeyEvent, c: char) -> char {
    use ratatui::crossterm::event::KeyEventState;
    if !key.state.contains(KeyEventState::CAPS_LOCK) || !c.is_alphabetic() {
        return c;
    }
    let mapped: Vec<char> = if key.modifiers.contains(ratatui::crossterm::event::KeyModifiers::SHIFT) {
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

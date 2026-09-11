//! Full-screen ratatui interface: a scrolling transcript, a multi-line
//! composer, and a status bar. Runs on the UI thread; the agent runs on a
//! worker thread and feeds this UI through a channel.

mod banner;
mod helpers;
mod palette;
mod types;

pub use banner::banner_ansi;
pub use helpers::{expand_attachments, extract_images, term_width};
pub use palette::{is_theme_name, THEMES};
pub use types::{detect_ascii, UiConfig};

use crate::agent::{ApprovalResponse, Handles, UiEvent, WorkerCmd};
use crate::config::{Config, ConfigPatch, PROVIDERS};
use crate::money;
use banner::{BRole, banner_lines};
use helpers::{
    bar, complete_path, humanize, longest_common_prefix, pad1,
    perm_name, render_message, render_tline, setting_max_tool_calls,
};
use palette::{Palette, palette_by_name};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
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
use std::collections::HashMap;
use types::{
    caps_char, ctrl_c_or_d, is_16color_terminal, picker_step_for_key, BannerColor, CursorKind,
    Glyphs, Kind, Mode, PickAction, Picker, TLine, DOUBLE_PRESS_TIMEOUT, GLYPHS_A, GLYPHS_U,
    MAX_SUGGEST, MAX_TRANSCRIPT, PICKER_VISIBLE, SETTING_LABELS, SLASH_COMMANDS, SPIN_A, SPIN_U,
};

/// The current working directory as a display string, with `$HOME` collapsed to
/// `~`. Falls back to `picoder` if the cwd can't be read.
fn cwd_label() -> String {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return "picoder".to_string(),
    };
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            if let Ok(rest) = cwd.strip_prefix(&home) {
                return if rest.as_os_str().is_empty() {
                    "~".to_string()
                } else {
                    format!("~/{}", rest.display())
                };
            }
        }
    }
    cwd.display().to_string()
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
    /// Suggested next prompt from the agent, shown as a dimmed hint.
    suggestion: Option<String>,
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
    /// True while the view is pinned above the live end: shows the scroll
    /// affordance in the status bar.
    scrolled_up: bool,
    /// Output arrived while the view was pinned, so the status bar hint can say
    /// "↓ new" instead of the neutral "↓ end".
    new_below: bool,
    scroll: usize,
    max_top: usize,
    view_h: usize,
    spinner: usize,
    spin_counter: usize,
    /// Clock time when the agent last entered Busy; used to show an elapsed timer.
    busy_since: Option<Instant>,
    model_info: String,
    last_models: Vec<String>,
    should_quit: bool,
    esc_deadline: Option<Instant>,
    /// Time of last Ctrl+C with empty input; used for double-press-to-quit.
    last_ctrl_c: Option<Instant>,
    /// Cached slash-command usage counts (command → times used). Rebuilt when
    /// history grows so we don't scan full history per keystroke.
    cmd_uses: HashMap<String, usize>,
    /// Set by Ctrl+L; makes the event loop clear the backend before the next
    /// draw, forcing a full repaint (recovers from any screen desync).
    force_clear: bool,
    /// Terminal draws every glyph in one cell (ASCII mode, or the Linux
    /// framebuffer console) — wide chars must be replaced before rendering.
    single_width: bool,
    glyphs: Glyphs,
    ascii: bool,
    palette: Palette,
    /// Cached rendered lines for the static (non-`live`) transcript. Rebuilding
    /// this for every frame meant re-wrapping thousands of lines per streamed
    /// token — the dominant cost on a single-core Pi. We rebuild only when the
    /// transcript content (`tver`) or terminal `width` actually changes, and
    /// each frame clones just the visible window.
    disp_cache: Vec<Line<'static>>,
    disp_cache_width: usize,
    disp_cache_tver: u64,
    /// Bumped on every transcript-content or palette change to invalidate
    /// `disp_cache`. (`single_width`/`glyphs` are fixed at construction.)
    tver: u64,
    // Claude-style status data:
    perm: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ctx_limit: u32,
    price_in: f64,
    price_out: f64,
    /// Unit the prices above are quoted in; the account balance carries its own.
    price_currency: money::Currency,
    last_prompt_tokens: u32,
    sess_prompt: u64,
    sess_completion: u64,
    balance: Option<money::Balance>,
    /// True once the "balance and prices are in different currencies" notice
    /// has been shown, so the per-turn balance refresh doesn't repeat it.
    balance_units_flagged: bool,
    settings: Config,
    /// Current working directory shown in the output box title, with $HOME
    /// collapsed to `~`. Computed once at startup (picoder never chdirs).
    cwd_label: String,
    /// Cached `(branch, dirty)` for the title's git indicator. `None` outside a
    /// repo. Refreshed on a throttle (see `git_checked_at`) since the agent's
    /// edits/auto-commits change the dirty state mid-session.
    git_head: Option<(String, bool)>,
    git_checked_at: Option<Instant>,
}

/// Lines moved per arrow key / wheel notch. PgUp/PgDn move a full viewport.
const SCROLL_STEP: isize = 3;

/// Whether a blank separator line should precede an entry of kind `next` given
/// the previous entry's kind. The transcript is made of distinct blocks — a
/// turn's prose, its tool activity, and the next prompt — and without a gap they
/// butt together into one wall of text. Only genuinely new blocks get a gap; a
/// notice or an error that follows a tool stays attached to it.
fn needs_sep(prev: Kind, next: Kind) -> bool {
    use Kind::*;
    let tool_like = |k: Kind| matches!(k, Tool | ToolResult | ToolErr | DiffAdd | DiffDel | DiffCtx);
    match next {
        // A new prompt opens a new turn: give it room above (but not above the
        // launch banner, which already pads itself).
        User => !matches!(prev, User | Banner | BannerDim),
        // Separate the prose that precedes tool activity from the tools.
        Tool => matches!(prev, Assistant | ToolResult | ToolErr | DiffAdd | DiffDel | DiffCtx),
        // Separate the assistant's next words (prose or reasoning) from the tool
        // activity that has just scrolled past, and set the answer apart from the
        // reasoning that produced it.
        Assistant => tool_like(prev) || prev == Reasoning,
        Reasoning => tool_like(prev),
        // An agent-level failure is its own block, not a footnote to a tool.
        ErrorK => tool_like(prev) || matches!(prev, Assistant | Reasoning),
        _ => false,
    }
}

/// Resolve a theme name for this terminal. A 16-color console (the Pi's
/// framebuffer, `TERM=linux`) can't display a theme's `Rgb` shades at all — it
/// emits the escape anyway, the console ignores the unknown parameters, and the
/// cell keeps whatever color was last in effect. So those terminals get the
/// theme with every shade snapped to the nearest ANSI color (see
/// `palette::for_16color`); a 256-color or truecolor terminal is left alone.
fn palette_for(theme: &str) -> Palette {
    let p = palette_by_name(theme);
    if is_16color_terminal() {
        palette::for_16color(p)
    } else {
        p
    }
}

impl App {
    pub fn new(cfg: UiConfig, history: Vec<String>) -> App {
        let hist_idx = history.len();
        let mut app = App {
            transcript: Vec::new(),
            live: String::new(),
            live_reasoning: String::new(),
            input: String::new(),
            cursor: 0,
            history,
            hist_idx,
            pending: String::new(),
            queued: Vec::new(),
            suggestion: None,
            suggest_idx: 0,
            picker: None,
            mode: Mode::Idle,
            pw_input: String::new(),
            pw_reply: None,
            q_input: String::new(),
            q_reply: None,
            follow: true,
            scrolled_up: false,
            new_below: false,
            scroll: 0,
            max_top: 0,
            view_h: 0,
            spinner: 0,
            spin_counter: 0,
            busy_since: None,
            model_info: cfg.model,
            last_models: Vec::new(),
            should_quit: false,
            esc_deadline: None,
            last_ctrl_c: None,
            cmd_uses: HashMap::new(),
            force_clear: false,
            single_width: cfg.ascii
                || matches!(std::env::var("TERM").as_deref(), Ok("linux")),
            glyphs: if cfg.ascii { GLYPHS_A } else { GLYPHS_U },
            ascii: cfg.ascii,
            palette: palette_for(&cfg.theme),
            disp_cache: Vec::new(),
            disp_cache_width: usize::MAX,
            disp_cache_tver: u64::MAX,
            tver: 0,
            perm: cfg.perm,
            ctx_limit: cfg.ctx_limit.max(1),
            price_in: cfg.price_in,
            price_out: cfg.price_out,
            price_currency: cfg.price_currency.clone(),
            last_prompt_tokens: 0,
            sess_prompt: 0,
            sess_completion: 0,
            balance: None,
            balance_units_flagged: false,
            settings: cfg.settings,
            cwd_label: cwd_label(),
            git_head: None,
            git_checked_at: None,
        };
        app.rebuild_cmd_uses();
        app
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    fn refresh_git_head(&mut self) {
        // Throttle: only probe git once every 5 seconds within render_output.
        if let Some(t) = self.git_checked_at {
            if t.elapsed() < Duration::from_secs(5) {
                return;
            }
        }
        self.git_checked_at = Some(Instant::now());
        let cwd = match std::env::current_dir() {
            Ok(p) => p,
            Err(_) => return,
        };
        self.git_head = crate::tools::git_head(&cwd);
    }

    fn push(&mut self, kind: Kind, text: impl Into<String>) {
        let text = text.into();
        if text.is_empty() && kind != Kind::Banner && kind != Kind::BannerDim {
            return;
        }
        self.separate(kind);
        self.transcript.push(TLine { kind, text, lead: true, color: None });
        self.after_push();
    }

    /// Insert a blank separator line when `kind` opens a new block (see
    /// `needs_sep`), so turns, prose, and tool activity don't run together into
    /// one unreadable wall of text.
    fn separate(&mut self, kind: Kind) {
        if let Some(last) = self.transcript.last() {
            if needs_sep(last.kind, kind) {
                self.transcript.push(TLine {
                    kind: Kind::Blank,
                    text: String::new(),
                    lead: true,
                    color: None,
                });
            }
        }
    }

    fn dirty(&mut self) {
        self.tver = self.tver.wrapping_add(1);
    }

    fn set_palette(&mut self, p: Palette) {
        self.palette = self.terminal_safe(p);
        self.dirty();
    }

    /// Adapt a palette to what this terminal can actually display, so a theme
    /// switch on the Pi's console doesn't reintroduce colors it can't show.
    fn terminal_safe(&self, p: Palette) -> Palette {
        if is_16color_terminal() {
            palette::for_16color(p)
        } else {
            p
        }
    }

    fn after_push(&mut self) {
        self.dirty();
        if self.transcript.len() > MAX_TRANSCRIPT {
            let excess = self.transcript.len() - MAX_TRANSCRIPT;
            self.transcript.drain(0..excess);
        }
        if self.follow {
            self.scroll = self.max_top;
        } else {
            self.scrolled_up = true;
            self.new_below = true;
        }
    }

    #[allow(dead_code)]
    fn push_assistant(&mut self, text: &str) {
        if let Some(last) = self.transcript.last_mut() {
            if last.kind == Kind::Assistant {
                last.text.push_str(text);
                self.dirty();
                return;
            }
        }
        self.transcript.push(TLine { kind: Kind::Assistant, text: text.to_string(), lead: true, color: None });
        self.dirty();
    }

    fn flush_live(&mut self) {
        if !self.live.is_empty() {
            self.separate(Kind::Assistant);
            self.transcript.push(TLine { kind: Kind::Assistant, text: std::mem::take(&mut self.live), lead: true, color: None });
            self.after_push();
        }
        if !self.live_reasoning.is_empty() {
            self.separate(Kind::Reasoning);
            self.transcript.push(TLine { kind: Kind::Reasoning, text: std::mem::take(&mut self.live_reasoning), lead: true, color: None });
            self.after_push();
        }
    }

    pub fn handle_event(&mut self, ev: UiEvent, h: &Handles) {
        match ev {
            UiEvent::Token(t) => {
                self.live.push_str(&t);
                self.dirty();
            }
            UiEvent::Reasoning(t) => {
                self.live_reasoning.push_str(&t);
                self.dirty();
            }
            UiEvent::ResetLive => {
                self.flush_live();
            }
            UiEvent::AssistantCommit => {
                self.flush_live();
                if let Some(last) = self.transcript.last() {
                    if last.kind == Kind::Assistant && last.text.is_empty() {
                        self.transcript.pop();
                        // Drop the separator that was inserted for the entry we
                        // just removed, so no stray blank is left behind.
                        if self.transcript.last().map(|l| l.kind) == Some(Kind::Blank) {
                            self.transcript.pop();
                        }
                    }
                }
            }
            UiEvent::ToolStart { name, summary } => {
                self.flush_live();
                self.push(Kind::Tool, format!("{name} {summary}"));
            }
            UiEvent::Diff(d) => {
                // Flush any live assistant text before showing a diff preview so
                // the diff doesn't interleave with streaming tokens mid-sentence.
                self.flush_live();
                for (i, ln) in d.lines().enumerate() {
                    let kind = if ln.starts_with('+') {
                        Kind::DiffAdd
                    } else if ln.starts_with('-') {
                        Kind::DiffDel
                    } else {
                        Kind::DiffCtx
                    };
                    self.transcript.push(TLine { kind, text: ln.to_string(), lead: i == 0, color: None });
                }
                self.after_push();
            }
            UiEvent::ToolResult { ok, preview } => {
                self.flush_live();
                if preview.is_empty() {
                    return;
                }
                let kind = if ok { Kind::ToolResult } else { Kind::ToolErr };
                self.push(kind, preview);
            }
            UiEvent::Approval(desc) => {
                // Flush any preceding assistant text before showing the prompt.
                self.flush_live();
                self.mode = Mode::Approval(desc);
            }
            UiEvent::PasswordRequest { prompt, reply } => {
                self.pw_input.clear();
                self.pw_reply = Some(reply);
                self.mode = Mode::Password { prompt };
            }
            UiEvent::Question { prompt, reply } => {
                self.q_input.clear();
                self.q_reply = Some(reply);
                self.mode = Mode::Question { prompt };
            }
            UiEvent::ModelList(ids) => {
                let current = ids.iter().position(|m| *m == self.model_info);
                self.picker = Some(Picker {
                    title: "Pick a model (type to filter)".into(),
                    items: ids.clone(),
                    current,
                    filter: String::new(),
                    cursor: current.unwrap_or(0),
                    scroll: 0,
                    action: PickAction::Model,
                });
                if let Some(p) = self.picker.as_mut() {
                    let len = p.items.len();
                    p.clamp(len); // scroll the current model into view
                }
                self.last_models = ids;
                self.mode = Mode::Select;
            }
            UiEvent::ModelChanged(m) => {
                self.model_info = m;
            }
            UiEvent::Usage { prompt, completion } => {
                self.last_prompt_tokens = prompt;
                self.sess_prompt += prompt as u64;
                self.sess_completion += completion as u64;
            }
            UiEvent::Context(n) => {
                self.last_prompt_tokens = n;
            }
            UiEvent::ContextLimit(n) => {
                self.ctx_limit = n;
            }
            UiEvent::AuthMode(m) => {
                self.settings.auth_mode = m;
            }
            UiEvent::Balance(b) => {
                // Point out a unit mismatch once (the balance is refetched every
                // turn), with the fix, instead of silently printing two
                // currencies on one line.
                if b.currency != self.price_currency && !self.balance_units_flagged {
                    self.balance_units_flagged = true;
                    self.push(
                        Kind::Notice,
                        format!(
                            "balance is in {} but prices are quoted in {} — set \"price_currency\": \"{}\" \
                             and that currency's price list in ~/.config/picoder/config.json (price_in/price_out, \
                             or the `price currency` row in /config) to compare them",
                            b.currency.code, self.price_currency.code, b.currency.code
                        ),
                    );
                }
                self.balance = Some(b);
            }
            UiEvent::Notice(msg) => {
                self.push(Kind::Notice, msg);
            }
            UiEvent::Error(msg) => {
                self.push(Kind::ErrorK, msg);
            }
            UiEvent::TurnDone => {
                self.flush_live();
                // Keep an open picker alive: the worker sends TurnDone right
                // after ModelList, and clear_busy would drop back to Idle.
                if matches!(self.mode, Mode::Select) && self.picker.is_some() {
                    self.busy_since = None;
                } else {
                    self.clear_busy();
                }
                // The worker may reply with Bypass toggled; sync the UI.
                self.perm = h.shared.perm.clone();
                // Dispatch the next queued message, if any.
                if let Some(next) = self.queued.first() {
                    let _ = h.cmd_tx.send(WorkerCmd::User { text: next.clone(), images: vec![] });
                    self.queued.remove(0);
                    self.set_busy();
                }
            }
            UiEvent::Suggestion(s) => {
                self.suggestion = Some(s);
            }
        }
    }

    #[allow(dead_code)]
    fn model_short(&self) -> &str {
        self.model_info.rsplit('/').next().unwrap_or(&self.model_info)
    }

    pub fn on_paste(&mut self, s: String) {
        for c in s.chars() {
            if c == '\n' || c == '\r' {
                // ignore; pasted newlines shouldn't submit
            } else {
                self.insert_char(c);
            }
        }
    }

    fn insert_char(&mut self, c: char) {
        // Never let a control character into the composer buffer. A pasted
        // snippet copied from a colored terminal (or a log file) carries raw
        // ESC sequences; drawn into the frame they desync the terminal from
        // ratatui's cell grid, so the rest of the screen — transcript included —
        // comes out in the wrong colors until a full repaint. Tabs are kept as
        // spaces for the same reason clean_text expands them.
        let c = if c == '\t' {
            ' '
        } else if c.is_control() {
            return;
        } else {
            c
        };
        let pos = self.byte_at(self.cursor);
        self.input.insert(pos, c);
        self.cursor += 1;
    }

    fn slash_suggestions(&self) -> Vec<(&'static str, &'static str)> {
        if self.mode == Mode::Idle && self.input.starts_with('/') && !self.input.contains(' ') {
            let mut scored: Vec<_> = SLASH_COMMANDS
                .iter()
                .filter(|(cmd, _)| cmd.starts_with(&self.input))
                .map(|&(cmd, desc)| {
                    let count = self.cmd_uses.get(cmd).copied().unwrap_or(0);
                    (cmd, desc, count)
                })
                .collect();
            scored.sort_by_key(|(cmd, _, count)| {
                // Exact match first, then prefix matches sorted by usage (desc),
                // then alphabetically.
                (
                    if *cmd == self.input { 0 } else { 1 },
                    std::cmp::Reverse(*count),
                    *cmd,
                )
            });
            scored.truncate(MAX_SUGGEST);
            scored.into_iter().map(|(c, d, _)| (c, d)).collect()
        } else {
            Vec::new()
        }
    }

    fn rebuild_cmd_uses(&mut self) {
        self.cmd_uses.clear();
        for entry in &self.history {
            if let Some(cmd) = entry.split_whitespace().next() {
                if cmd.starts_with('/') {
                    *self.cmd_uses.entry(cmd.to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.input
            .chars()
            .take(char_idx)
            .map(|c| c.len_utf8())
            .sum()
    }

    fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    pub fn on_key(&mut self, key: KeyEvent, h: &Handles) {
        // Cycle the permission mode in any state, except while a masked prompt
        // or an ask_user question is capturing every key. Shift+Tab is the
        // primary binding: it arrives as BackTab on ANSI terminals, or as
        // Tab+SHIFT under the Kitty keyboard protocol (which setup_terminal
        // pushes) — so both spellings must be recognized, and neither may fall
        // through to the composer's Tab autocomplete. The Pi's framebuffer
        // console can't report Shift+Tab at all (its keymap has no shift
        // binding for Tab), so Ctrl+P is the console-safe alias.
        if !matches!(self.mode, Mode::Password { .. } | Mode::Question { .. }) {
            let shift_tab = key.code == KeyCode::BackTab
                || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
            let ctrl_p = matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
                && key.modifiers.contains(KeyModifiers::CONTROL);
            if shift_tab || ctrl_p {
                self.cycle_perm();
                return;
            }
        }
        match self.mode {
            Mode::Password { .. } => self.on_key_password(key),
            Mode::Question { .. } => self.on_key_question(key),
            Mode::Approval(_) => self.on_key_approval(key, h),
            Mode::Settings { .. } => self.on_key_settings(key, h),
            Mode::Select => self.on_key_select(key, h),
            Mode::ThemeSelect { .. } => self.on_key_themeselect(key, h),
            Mode::Busy => self.on_key_busy(key, h),
            Mode::Idle => self.on_key_idle(key, h),
        }
    }

    fn do_esc(&mut self, h: &Handles) {
        match self.mode {
            Mode::Password { .. } => self.cancel_password(),
            Mode::Question { .. } => self.cancel_question(),
            Mode::Approval(_) => {
                let _ = h.appr_tx.send(ApprovalResponse::No);
                self.clear_busy();
            }
            Mode::Settings { .. } | Mode::Select | Mode::ThemeSelect { .. } => {
                self.clear_busy();
                self.picker = None;
            }
            Mode::Busy => self.interrupt(h),
            Mode::Idle => {
                self.busy_since = None;
                self.suggestion = None;
            }
        }
    }

    fn on_key_password(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.cancel_password(),
            KeyCode::Enter => {
                let val = std::mem::take(&mut self.pw_input);
                if let Some(tx) = self.pw_reply.take() {
                    let _ = tx.send(Some(val));
                }
                self.mode = Mode::Idle;
            }
            KeyCode::Backspace => {
                self.pw_input.pop();
            }
            KeyCode::Char(c) => {
                self.pw_input.push(caps_char(&key, c));
            }
            _ => {}
        }
    }

    fn cancel_password(&mut self) {
        if let Some(tx) = self.pw_reply.take() {
            let _ = tx.send(None);
        }
        self.mode = Mode::Idle;
    }

    fn on_key_question(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.cancel_question(),
            KeyCode::Enter => {
                let val = std::mem::take(&mut self.q_input);
                if let Some(tx) = self.q_reply.take() {
                    let _ = tx.send(Some(val));
                }
                self.mode = Mode::Idle;
            }
            KeyCode::Backspace => {
                self.q_input.pop();
            }
            KeyCode::Char(c) => {
                self.q_input.push(caps_char(&key, c));
            }
            _ => {}
        }
    }

    fn cancel_question(&mut self) {
        if let Some(tx) = self.q_reply.take() {
            let _ = tx.send(None);
        }
        self.mode = Mode::Idle;
    }

    fn cycle_perm(&self) {
        let v = self.perm.load(Ordering::Relaxed);
        let next = match v {
            crate::agent::PERM_ASK => crate::agent::PERM_AUTO,
            crate::agent::PERM_AUTO => crate::agent::PERM_PLAN,
            _ => crate::agent::PERM_ASK,
        };
        self.perm.store(next, Ordering::Relaxed);
    }

    fn perm(&self) -> u8 {
        self.perm.load(Ordering::Relaxed)
    }

    fn on_key_approval(&mut self, key: KeyEvent, h: &Handles) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let _ = h.appr_tx.send(ApprovalResponse::Yes);
                self.set_busy();
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                let _ = h.appr_tx.send(ApprovalResponse::No);
                self.set_busy();
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                let _ = h.appr_tx.send(ApprovalResponse::Always);
                self.set_busy();
            }
            KeyCode::Esc => {
                let _ = h.appr_tx.send(ApprovalResponse::No);
                self.clear_busy();
            }
            _ => {}
        }
    }

    fn preview_theme(&mut self, idx: usize, prev: String) {
        self.set_palette(palette_by_name(THEMES[idx]));
        self.mode = Mode::ThemeSelect { cursor: idx, prev };
    }

    /// Commit the theme at `idx`: persist it and close the picker.
    fn commit_theme(&mut self, idx: usize) {
        let name = THEMES[idx];
        self.set_palette(palette_by_name(name));
        crate::config::Config::persist_theme(name);
        self.push(Kind::Notice, format!("theme set to {name}"));
        self.mode = Mode::Idle;
    }

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
                // Swap to the new provider's saved API key.
                self.settings.resolve_key();
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
                // Toggle credential source: API key <-> subscription (OAuth).
                let next = if self.settings.auth_mode == "sub" { "api" } else { "sub" };
                self.settings.auth_mode = next.to_string();
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::AuthMode(next.to_string())));
            }
            5 => {
                let v = !self.settings.thinking;
                self.settings.thinking = v;
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::Thinking(v)));
            }
            6 => {
                let next = if dir >= 0 { (self.perm() + 1) % 3 } else { (self.perm() + 2) % 3 };
                self.perm.store(next, Ordering::Relaxed);
                let name = perm_name(next);
                self.settings.permission = name.to_string();
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::Permission(name.to_string())));
            }
            7 => {
                let v = !self.settings.auto_commit;
                self.settings.auto_commit = v;
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::AutoCommit(v)));
            }
            8 => {
                let i = THEMES
                    .iter()
                    .position(|t| *t == self.palette.name)
                    .map(|i| cycle(i, THEMES.len()))
                    .unwrap_or(0);
                self.set_palette(palette_by_name(THEMES[i]));
                self.settings.theme = THEMES[i].to_string();
                Config::persist_theme(THEMES[i]);
            }
            9 => self.mode = edit_with(self.settings.context_window.to_string()),
            10 => self.mode = edit_with(setting_max_tool_calls(self.settings.max_tool_calls)),
            // Prices below are quoted in this currency; set it to match the
            // account's billing currency (see the balance readout) so the cost
            // and balance figures line up.
            11 => self.mode = edit_with(self.settings.price_currency.clone()),
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
                    self.model_info = val.clone();
                    let _ = h.cmd_tx.send(WorkerCmd::SetModel(val));
                }
            }
            3 => {
                self.settings.api_key = val.clone();
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::ApiKey(val)));
            }
            9 => {
                let v: u32 = val.parse().unwrap_or(self.ctx_limit);
                self.settings.context_window = v;
                self.ctx_limit = v;
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::ContextWindow(v)));
            }
            10 => {
                let v = helpers::parse_max_tool_calls(&val);
                self.settings.max_tool_calls = v;
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::MaxToolCalls(v)));
            }
            11 => {
                let code = money::Currency::parse(&val).code;
                self.settings.price_currency = code.clone();
                // Take effect immediately: the status line re-reads it, so the
                // cost figure switches units as soon as the row is committed.
                self.price_currency = money::Currency::parse(&code);
                let _ = h.cmd_tx.send(WorkerCmd::Patch(ConfigPatch::PriceCurrency(code)));
            }
            _ => {}
        }
    }

    fn on_key_select(&mut self, key: KeyEvent, h: &Handles) {
        let Some(ref mut picker) = self.picker else {
            self.mode = Mode::Idle;
            return;
        };
        // Movement keys (arrows, Ctrl+j/k, PageUp/PageDown) are handled up
        // front rather than as match arms: a shared arm like
        // `KeyCode::Up | KeyCode::Char('k') if ctrl` applies its guard to
        // `Up` too, so plain arrow keys fell through to `_ => {}` and did
        // nothing. Bare j/k stay filter input (the list is type-to-filter).
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(delta) = picker_step_for_key(&key) {
            picker.step(delta);
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Idle;
                self.picker = None;
            }
            KeyCode::Enter => {
                let filtered = picker.filtered();
                if let Some(&idx) = filtered.get(picker.cursor) {
                    let item = picker.items[idx].clone();
                    match picker.action {
                        PickAction::Model => {
                            let _ = h.cmd_tx.send(WorkerCmd::SetModel(item));
                        }
                        PickAction::Login => {
                            let _ = h.cmd_tx.send(WorkerCmd::Login(item));
                        }
                    }
                }
                self.mode = Mode::Idle;
                self.picker = None;
            }
            KeyCode::Backspace => {
                picker.filter.pop();
                let len = picker.filtered().len();
                picker.clamp(len.max(1));
            }
            // Ctrl-modified chars are shortcuts, not filter input.
            KeyCode::Char(c) if !ctrl => {
                picker.filter.push(caps_char(&key, c));
                let len = picker.filtered().len();
                picker.clamp(len.max(1));
            }
            _ => {}
        }
    }

    fn on_key_themeselect(&mut self, key: KeyEvent, _h: &Handles) {
        let (cursor, prev) = match &self.mode {
            Mode::ThemeSelect { cursor, prev } => (*cursor, prev.clone()),
            _ => return,
        };
        match key.code {
            KeyCode::Esc => {
                self.set_palette(palette_by_name(&prev));
                self.mode = Mode::Idle;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let idx = cursor.saturating_sub(1).max(0);
                self.preview_theme(idx, prev);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let idx = (cursor + 1).min(THEMES.len() - 1);
                self.preview_theme(idx, prev);
            }
            KeyCode::Enter => self.commit_theme(cursor),
            _ => {}
        }
    }

    fn on_key_busy(&mut self, key: KeyEvent, h: &Handles) {
        match key.code {
            KeyCode::Esc => self.interrupt(h),
            KeyCode::Char('c') | KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.interrupt(h);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char(caps_char(&key, c));
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let pos = self.byte_at(self.cursor);
                    self.input.remove(pos);
                }
            }
            KeyCode::Enter => self.queue_input(),
            // Scroll the output transcript while the agent runs; the queued
            // composer line stays put.
            KeyCode::Up => self.scroll_lines(-SCROLL_STEP),
            KeyCode::Down => self.scroll_lines(SCROLL_STEP),
            KeyCode::PageUp => self.scroll_page(true),
            KeyCode::PageDown => self.scroll_page(false),
            _ => {}
        }
    }

    fn interrupt(&mut self, h: &Handles) {
        h.shared.cancel.store(true, Ordering::Relaxed);
        // Restore any queued input undone by Esc so the composer content isn't lost.
        if let Some(pending) = self.take_input() {
            if !pending.is_empty() {
                self.queued.insert(0, pending);
            }
        }
        self.clear_busy();
        // Clear the suggestion so the user sees the hint again.
        self.suggestion = None;
    }

    fn on_key_idle(&mut self, key: KeyEvent, h: &Handles) {
        if key.code == KeyCode::Esc || ctrl_c_or_d(&key) {
            // Check for double-press exit (Ctrl+C / Ctrl+D style).
            if self.input.is_empty() {
                let now = Instant::now();
                if let Some(t) = self.last_ctrl_c {
                    if now.duration_since(t) < DOUBLE_PRESS_TIMEOUT {
                        self.should_quit = true;
                        return;
                    }
                }
                self.last_ctrl_c = Some(now);
                return;
            }
        }
        self.last_ctrl_c = None;

        match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if c == '/' && self.input.is_empty() {
                    self.suggest_idx = 0;
                }
                self.insert_char(caps_char(&key, c));
                self.suggest_idx = 0;
            }
            KeyCode::Tab => {
                if !self.input.starts_with('/') || self.input.contains(' ') {
                    // Cycle suggestion
                    if let Some(ref s) = self.suggestion {
                        if !self.input.is_empty() {
                            self.complete();
                            return;
                        }
                        self.input = s.clone();
                        self.cursor = self.char_len();
                        self.suggestion = None;
                        return;
                    }
                    self.complete();
                } else {
                    let sugg = self.slash_suggestions();
                    if !sugg.is_empty() {
                        let idx = self.suggest_idx.min(sugg.len() - 1);
                        self.input = sugg[idx].0.to_string();
                        self.cursor = self.char_len();
                        self.suggest_idx = 0;
                    }
                }
            }
            KeyCode::Backspace => self.on_key_edit(key),
            KeyCode::Delete => self.delete_word_forward(),
            // Modified arrows: Alt/Ctrl+←/→ move by word, Alt/Ctrl+↑/↓ walk the
            // composer history (plain arrows are the transcript scroll below).
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.suggest_idx = 0;
                match key.code {
                    KeyCode::Left => self.cursor = self.prev_word(),
                    KeyCode::Right => self.cursor = self.next_word(),
                    KeyCode::Up => self.history_prev(),
                    KeyCode::Down => self.history_next(),
                    _ => {}
                }
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
            }
            KeyCode::Right => {
                if self.cursor < self.char_len() {
                    self.cursor += 1;
                }
            }
            // Plain arrows roll the transcript output. This is the gesture that
            // works on every terminal — Warp (and any terminal where we skip
            // mouse capture) delivers no scroll-wheel events at all.
            KeyCode::Up => self.scroll_lines(-SCROLL_STEP),
            KeyCode::Down => self.scroll_lines(SCROLL_STEP),
            KeyCode::Enter => {
                self.last_ctrl_c = None;
                self.submit(h);
                return;
            }
            // Scroll the output transcript; the input field is untouched.
            KeyCode::PageUp => self.scroll_page(true),
            KeyCode::PageDown => self.scroll_page(false),
            _ => {}
        }
    }

    fn on_key_edit(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Backspace => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.delete_word();
                } else if self.cursor > 0 {
                    self.cursor -= 1;
                    let pos = self.byte_at(self.cursor);
                    self.input.remove(pos);
                }
            }
            KeyCode::Delete => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.delete_word_forward();
                } else {
                    let pos = self.byte_at(self.cursor);
                    if pos < self.input.len() {
                        self.input.remove(pos);
                    }
                }
            }
            _ => {}
        }
    }

    fn prev_word(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor.min(chars.len());
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
        let mut i = self.cursor;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    fn delete_word(&mut self) {
        let target = self.prev_word();
        let start = self.byte_at(target);
        let end = self.byte_at(self.cursor);
        self.input.drain(start..end);
        self.cursor = target;
    }

    fn delete_word_forward(&mut self) {
        let target = self.next_word();
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(target);
        self.input.drain(start..end);
    }

    fn complete(&mut self) {
        if self.input.starts_with('@') {
            let prefix = &self.input[1..];
            let opts = complete_path(prefix);
            if opts.is_empty() {
                return;
            }
            if opts.len() == 1 {
                self.input = format!("@{}", opts[0]);
                self.cursor = self.char_len();
                return;
            }
            let lcp = longest_common_prefix(&opts);
            if lcp.len() > prefix.len() {
                self.input = format!("@{lcp}");
                self.cursor = self.char_len();
            }
        }
    }

    fn history_prev(&mut self) {
        if self.hist_idx > 0 {
            if self.hist_idx == self.history.len() {
                self.pending = std::mem::take(&mut self.input);
            }
            self.hist_idx -= 1;
            self.input = self.history[self.hist_idx].clone();
            self.cursor = self.char_len();
        }
    }

    fn history_next(&mut self) {
        if self.hist_idx < self.history.len() {
            self.hist_idx += 1;
            if self.hist_idx == self.history.len() {
                self.input = std::mem::take(&mut self.pending);
            } else {
                self.input = self.history[self.hist_idx].clone();
            }
            self.cursor = self.char_len();
        }
    }

    pub fn mouse_scroll(&mut self, up: bool) {
        if up {
            self.scroll_lines(-SCROLL_STEP);
        } else {
            self.scroll_lines(SCROLL_STEP);
        }
    }

    /// Scroll the transcript by `n` lines (negative scrolls up). Scrolling up
    /// pins the view (`follow = false`); coming back to the bottom resumes
    /// following the live end.
    fn scroll_lines(&mut self, n: isize) {
        if n < 0 {
            // Nothing above the fold: don't pin or flash the "more below" hint.
            if self.max_top == 0 {
                return;
            }
            self.follow = false;
            self.scroll = self.scroll.saturating_sub(n.unsigned_abs());
            self.scrolled_up = true;
        } else {
            self.scroll = (self.scroll + n as usize).min(self.max_top);
            if self.scroll >= self.max_top {
                self.follow = true;
                self.scrolled_up = false;
                self.new_below = false;
            }
        }
    }

    /// Scroll a whole screenful (PgUp/PgDn), using the last rendered viewport
    /// height so a page is exactly what the user can see.
    fn scroll_page(&mut self, up: bool) {
        let page = self.view_h.max(1) as isize;
        self.scroll_lines(if up { -page } else { page });
    }

    /// Snap back to the live end, dropping the "[arrow] new" hint. Called when
    /// the user sends or queues a message so the answer they're waiting for is
    /// on screen.
    fn scroll_to_bottom(&mut self) {
        self.follow = true;
        self.scrolled_up = false;
        self.new_below = false;
    }

    fn submit(&mut self, h: &Handles) {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        if text.is_empty() {
            return;
        }
        self.history.push(text.clone());
        self.hist_idx = self.history.len();
        self.pending.clear();
        self.suggestion = None;
        self.rebuild_cmd_uses();
        self.scroll_to_bottom();
        self.dispatch(text, h);
    }

    fn queue_input(&mut self) {
        if let Some(text) = self.take_input() {
            if !text.is_empty() {
                self.queued.push(text);
            }
        }
    }

    fn take_input(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        if text.is_empty() {
            return None;
        }
        self.history.push(text.clone());
        self.hist_idx = self.history.len();
        self.pending.clear();
        self.rebuild_cmd_uses();
        self.scroll_to_bottom();
        Some(text)
    }



    fn set_busy(&mut self) {
        self.mode = Mode::Busy;
        self.busy_since = Some(Instant::now());
    }

    fn clear_busy(&mut self) {
        self.mode = Mode::Idle;
        self.busy_since = None;
    }

    fn dispatch(&mut self, text: String, h: &Handles) {
        if self.mode == Mode::Busy {
            self.queued.push(text);
            return;
        }
        if let Some(cmd) = text.strip_prefix('/') {
            self.run_command(cmd, h);
            return;
        }
        // Echo the prompt into the transcript (with its full-width band) before
        // the turn starts, so the user can see what they asked for while
        // scrolling back — and so the attachments they referenced are named.
        self.push(Kind::User, text.clone());
        let (task_text, attached) = expand_attachments(&text);
        let (images, img_names) = extract_images(&text);
        let mut all = attached;
        all.extend(img_names);
        if !all.is_empty() {
            self.push(Kind::Notice, format!("attached: {}", all.join(", ")));
        }
        self.set_busy();
        let _ = h.cmd_tx.send(WorkerCmd::User { text: task_text, images });
    }

    fn open_login_picker(&mut self) {
        use crate::auth;
        self.picker = Some(Picker {
            title: "Pick a provider to sign in to".into(),
            items: auth::supported().iter().map(|s| s.to_string()).collect(),
            current: None,
            filter: String::new(),
            cursor: 0,
            scroll: 0,
            action: PickAction::Login,
        });
        self.mode = Mode::Select;
    }

    fn run_command(&mut self, cmd: &str, h: &Handles) {
        let (cmd, _arg) = match cmd.split_once(' ') {
            Some((c, a)) => (c, Some(a)),
            None => (cmd, None),
        };
        match cmd {
            "model" => {
                if let Some(arg) = _arg {
                    let _ = h.cmd_tx.send(WorkerCmd::SetModel(arg.to_string()));
                } else {
                    let _ = h.cmd_tx.send(WorkerCmd::ListModels);
                    self.mode = Mode::Select;
                }
            }
            "login" => {
                self.open_login_picker();
            }
            "new" => {
                let _ = h.cmd_tx.send(WorkerCmd::New);
                self.transcript.clear();
                self.dirty();
                self.push(Kind::Notice, "new session — fresh start.".to_string());
            }
            "config" => {
                self.mode = Mode::Settings { cursor: 0, edit: None };
            }
            "compact" => {
                self.set_busy();
                let _ = h.cmd_tx.send(WorkerCmd::Compact);
            }
            "reset" => {
                let _ = h.cmd_tx.send(WorkerCmd::Reset);
                self.transcript.clear();
                self.dirty();
            }
            "auto" => {
                self.cycle_perm();
                self.push(
                    Kind::Notice,
                    format!("permissions: {}", perm_name(self.perm())),
                );
            }
            "mcp" => {
                let _ = h.cmd_tx.send(WorkerCmd::ListMcp);
            }
            "memory" => {
                match crate::tools::load_memory() {
                    Ok(Some(text)) => self.push(Kind::Notice, format!("memory:\n{text}")),
                    Ok(None) => self.push(Kind::Notice, String::from("no persistent memory.")),
                    Err(e) => self.push(Kind::ErrorK, format!("{e}")),
                }
            }
            "theme" => {
                if let Some(name) = _arg {
                    let p = palette_by_name(name);
                    self.set_palette(p);
                    crate::config::Config::persist_theme(p.name);
                    self.push(Kind::Notice, format!("theme set to {}", p.name));
                } else {
                    let current = THEMES
                        .iter()
                        .position(|&n| n == self.palette.name)
                        .unwrap_or(0);
                    self.preview_theme(current, self.palette.name.to_string());
                }
            }
            "init" => {
                self.set_busy();
                let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                let task = format!(
                    "Summarise this codebase into a PICODER.md (or AGENTS.md / CLAUDE.md \
                     if one already exists) that picoder (a coding agent) can read at \
                     startup. Include key architecture, conventions, and safety notes. \
                     Keep it under 300 lines. Use the existing PICODER.md as a starting \
                     point if it exists. Working directory: {}",
                    cwd.display()
                );
                let _ = h.cmd_tx.send(WorkerCmd::User { text: task, images: vec![] });
            }
            "clear" => {
                self.transcript.clear();
                self.dirty();
            }
            "help" => self.show_help(),
            "exit" | "quit" | "q" => self.should_quit = true,
            _ => self.push(Kind::ErrorK, format!("unknown command: /{cmd} (try /help)")),
        }
    }

    fn show_help(&mut self) {
        self.push(Kind::Notice, String::from("commands:"));
        for (name, desc) in SLASH_COMMANDS {
            self.push(Kind::Notice, format!("  {name:<12} {desc}"));
        }
        self.push(Kind::Notice, String::from("  @file       attach a file"));
        self.push(Kind::Notice, "  ↑/↓         scroll the output (3 lines)");
        self.push(Kind::Notice, "  PgUp/PgDn   scroll the output a page");
        self.push(Kind::Notice, "  Ctrl/Alt+↑/↓  browse history");
        self.push(Kind::Notice, String::from("  Tab         autocomplete"));
        self.push(Kind::Notice, String::from("  Shift+Tab   cycle permissions (ask / bypass / plan)"));
        self.push(Kind::Notice, String::from("  Esc         interrupt the agent"));
        self.push(Kind::Notice, String::from("  Ctrl+L      force clear/repaint"));
    }

    pub fn banner(&mut self, width: u16, status: Vec<String>) {
        let w = (width as usize).saturating_sub(4).max(8);
        for bl in banner_lines(w, self.ascii, &status) {
            let (kind, color) = match bl.role {
                BRole::Art(i) => (
                    Kind::Banner,
                    Some(BannerColor::Rainbow(i)),
                ),
                BRole::Version => (Kind::Banner, None),
                BRole::Tagline => (Kind::BannerDim, None),
                BRole::Frame | BRole::Data => (Kind::BannerDim, Some(BannerColor::Accent)),
            };
            self.transcript.push(TLine { kind, text: bl.text, lead: false, color });
        }
        self.push_dim(String::new());
        self.after_push();
    }

    fn push_dim(&mut self, text: String) {
        self.transcript.push(TLine { kind: Kind::BannerDim, text, lead: false, color: None });
        self.dirty();
    }

    pub fn welcome(&mut self) {
        let model = self.model_info.clone();
        self.push(Kind::Notice, format!("picoder ({model}) — theme: {} · type a task, or /help for commands", self.palette.name));
    }

    pub fn note(&mut self, s: String) {
        self.push(Kind::Notice, s);
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn busy(&self) -> bool {
        self.mode == Mode::Busy
    }

    pub fn tick_spinner(&mut self) -> bool {
        self.spin_counter += 1;
        if self.spin_counter % 6 == 0 {
            self.spinner = (self.spinner + 1) % 10;
            true
        } else {
            false
        }
    }

    fn spin_frame(&self) -> &'static str {
        if self.ascii {
            SPIN_A[self.spinner % SPIN_A.len()]
        } else {
            SPIN_U[self.spinner]
        }
    }

    fn working_timer(&self) -> String {
        match self.busy_since {
            Some(t) => {
                let secs = t.elapsed().as_secs();
                if secs < 60 {
                    format!("working ({secs}s)… ")
                } else {
                    let m = secs / 60;
                    let s = secs % 60;
                    format!("working ({m}:{s:02})… ")
                }
            }
            None => "working... ".to_string(),
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
        self.palette.secondary
    }

    // ----------------------------------------------------------- render -----

    pub fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        // Paint the themed background first; spans drawn on top keep their own
        // fg and inherit this bg (Color::Reset on the default theme = no-op,
        // so the terminal's own background shows through).
        if self.palette.bg != Color::Reset {
            f.render_widget(
                Block::default().style(Style::default().bg(self.palette.bg)),
                area,
            );
        }
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
                    if s.key_from_env {
                        format!("…{tail} (from env — overrides the saved key)")
                    } else {
                        format!("…{tail}")
                    }
                }
            }
            4 => {
                if s.auth_mode == "sub" {
                    let signed = if s.oauth.contains_key(&s.provider) { "" } else { " — not signed in, run /login" };
                    format!("subscription{signed}")
                } else {
                    "api key".into()
                }
            }
            5 => if s.thinking { "on".into() } else { "off".into() },
            6 => perm_name(self.perm()).to_string(),
            7 => if s.auto_commit { "on".into() } else { "off".into() },
            8 => self.palette.name.to_string(),
            9 => s.context_window.to_string(),
            10 => setting_max_tool_calls(s.max_tool_calls),
            11 => s.price_currency.clone(),
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
            Mode::Busy => {
                1 + self.queued.len() as u16 + self.input_rows(width)
                    + self.slash_suggestions().len() as u16
            }
            Mode::Approval(_) => 2,
            Mode::ThemeSelect { .. } => THEMES.len() as u16 + 1,
            Mode::Settings { .. } => SETTING_LABELS.len() as u16 + 1,
            Mode::Select => {
                let n = self.picker.as_ref().map(|p| p.filtered().len()).unwrap_or(0);
                n.clamp(1, PICKER_VISIBLE) as u16 + 1
            }
            Mode::Password { .. } => 2,
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
            Paragraph::new(Line::from(Span::styled(line, Style::default().fg(self.palette.chrome)))),
            area,
        );
    }

    fn render_output(&mut self, f: &mut Frame, area: Rect) {
        self.refresh_git_head();
        let mut title_spans = vec![
            Span::styled(format!(" {} ", self.cwd_label), Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD)),
        ];
        if let Some((branch, dirty)) = self.git_head.clone() {
            let dot = if self.ascii || self.single_width { "*" } else { "●" };
            let dot_color = if dirty { self.palette.code } else { self.palette.diff_add };
            title_spans.push(Span::styled(
                format!("{branch} "),
                Style::default().fg(self.palette.secondary),
            ));
            title_spans.push(Span::styled(
                format!("{dot} "),
                Style::default().fg(dot_color),
            ));
        }
        let title = Line::from(title_spans);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(Style::default().fg(self.palette.chrome))
            .title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        self.ensure_display_cache(width);
        let live = self.build_live_lines(width);
        let total = self.disp_cache.len() + live.len();
        self.view_h = inner.height as usize;
        self.max_top = total.saturating_sub(self.view_h);
        let top = if self.follow { self.max_top } else { self.scroll.min(self.max_top) };
        self.scroll = top;
        let mut visible: Vec<Line> = Vec::with_capacity(self.view_h);
        for ln in self.disp_cache.iter().skip(top).take(self.view_h) {
            visible.push(ln.clone());
        }
        let remaining = self.view_h.saturating_sub(visible.len());
        if remaining > 0 {
            let live_skip = top.saturating_sub(self.disp_cache.len());
            for ln in live.into_iter().skip(live_skip).take(remaining) {
                visible.push(ln);
            }
        }
        f.render_widget(Paragraph::new(visible), inner);
    }

    fn ensure_display_cache(&mut self, width: usize) {
        if self.disp_cache_width == width && self.disp_cache_tver == self.tver {
            return;
        }
        let mut out: Vec<Line<'static>> = Vec::new();
        for t in &self.transcript {
            if t.kind == Kind::Assistant {
                render_message(&mut out, &t.text, t.lead, width, self.glyphs, &self.palette, self.single_width);
            } else {
                render_tline(&mut out, t.kind, &t.text, t.lead, t.color, width, self.glyphs, &self.palette, self.single_width);
            }
        }
        self.disp_cache = out;
        self.disp_cache_width = width;
        self.disp_cache_tver = self.tver;
    }

    fn build_live_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        // Reserve the same blank gap the committed entry will get, so streaming
        // text doesn't jump up a line the moment it is committed.
        let lead_kind = if !self.live.is_empty() {
            Some(Kind::Assistant)
        } else if !self.live_reasoning.is_empty() {
            Some(Kind::Reasoning)
        } else {
            None
        };
        if let (Some(k), Some(last)) = (lead_kind, self.transcript.last()) {
            if needs_sep(last.kind, k) {
                out.push(Line::default());
            }
        }
        if !self.live.is_empty() {
            render_message(&mut out, &self.live, true, width, self.glyphs, &self.palette, self.single_width);
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
                    Span::styled(self.working_timer(), Style::default().fg(self.dim_text())),
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
                    lines.push(Line::from(vec![
                        Span::styled(format!("{lead}{t}"), style),
                    ]));
                }
                f.render_widget(Paragraph::new(lines), area);
            }
            Mode::Settings { cursor, .. } => {
                let mut lines: Vec<Line> = Vec::new();
                lines.push(Line::from(vec![
                    Span::styled("settings ", Style::default().fg(Color::Yellow)),
                    Span::styled("(up/down to browse, Enter to change, Esc to close)", Style::default().fg(self.dim_text())),
                ]));
                let marker = if self.ascii { ">" } else { "▸" };
                for (i, label) in SETTING_LABELS.iter().enumerate() {
                    let sel = i == *cursor;
                    let lead = if sel { marker } else { " " };
                    let label_style = if sel {
                        Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(self.dim_text())
                    };
                    let val = self.setting_value(i);
                    lines.push(Line::from(vec![
                        Span::styled(format!("{lead}{label:<16} "), label_style),
                        Span::styled(val, Style::default().fg(self.palette.accent)),
                    ]));
                }
                f.render_widget(Paragraph::new(lines), area);
            }
            Mode::Select => {
                let mut lines: Vec<Line> = Vec::new();
                if let Some(ref picker) = self.picker {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}", picker.title), Style::default().fg(Color::Yellow)),
                        Span::styled(" (type to filter, Enter to select, Esc to cancel)", Style::default().fg(self.dim_text())),
                    ]));
                    let filtered = picker.filtered();
                    if filtered.is_empty() {
                        lines.push(Line::from(Span::styled(
                            "no matches",
                            Style::default().fg(self.dim_text()),
                        )));
                    } else {
                        let marker = if self.ascii { ">" } else { "▸" };
                        let end = (picker.scroll + PICKER_VISIBLE).min(filtered.len());
                        for fi in picker.scroll..end {
                            let idx = filtered[fi];
                            let sel = fi == picker.cursor;
                            let lead = if sel { marker } else { " " };
                            let mark = if Some(idx) == picker.current { " (*)" } else { "" };
                            let style = if sel {
                                Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(self.dim_text())
                            };
                            lines.push(Line::from(vec![
                                Span::styled(format!("{lead}{}{mark}", picker.items[idx]), style),
                            ]));
                        }
                    }
                }
                f.render_widget(Paragraph::new(lines), area);
            }
            Mode::Password { prompt } => {
                let mut lines: Vec<Line> = Vec::new();
                let masked: String = self.pw_input.chars().map(|_| '*').collect();
                lines.push(Line::from(vec![
                    Span::styled(prompt.clone(), Style::default().fg(self.palette.accent)),
                ]));
                lines.push(Line::from(vec![
                    Span::styled(if masked.is_empty() { " " } else { &masked }, Style::default().fg(self.palette.accent)),
                    Span::styled("█", Style::default().add_modifier(Modifier::REVERSED)),
                ]));
                f.render_widget(Paragraph::new(lines), area);
            }
            Mode::Question { prompt } => {
                let mut lines: Vec<Line> = Vec::new();
                lines.push(Line::from(vec![
                    Span::styled(prompt.clone(), Style::default().fg(self.palette.accent)),
                ]));
                let mut line_spans = vec![
                    Span::styled(if self.q_input.is_empty() { " " } else { &self.q_input }, Style::default().fg(self.palette.accent)),
                ];
                if !self.q_input.is_empty() {
                    line_spans.push(Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)));
                }
                lines.push(Line::from(line_spans));
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
            let text_style = Style::default().fg(self.palette.accent);
            if r == cur_row && cursor != CursorKind::Caret {
                let before: String = row.iter().take(cur_col).collect();
                let after: String = row.iter().skip(cur_col + 1).collect();
                spans.push(Span::styled(before, text_style));
                match cursor {
                    CursorKind::Block => spans.push(Span::styled(block, accent)),
                    _ => {
                        let at = row.get(cur_col).map(|c| c.to_string()).unwrap_or_else(|| " ".into());
                        spans.push(Span::styled(at, Style::default().add_modifier(Modifier::REVERSED)));
                    }
                }
                spans.push(Span::styled(after, text_style));
            } else {
                spans.push(Span::styled(row.iter().collect::<String>(), text_style));
            }
            if empty && r == 0 {
                let hint = if matches!(self.mode, Mode::Busy) {
                    "  type to queue the next message".to_string()
                } else if let Some(ref s) = self.suggestion {
                    if self.ascii {
                        format!("  [{}] (Tab)", s)
                    } else {
                        format!("  {}  (Tab to accept)", s)
                    }
                } else {
                    "  describe a task · @file to attach · /help".to_string()
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
        let sep = || Span::styled(if self.ascii { "  |  " } else { "  │  " }, Style::default().fg(self.palette.chrome));
        let mut spans = vec![Span::styled(
            self.model_info.clone(),
            Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD),
        )];
        if self.settings.thinking {
            spans.push(Span::styled(" think", gray));
        }

        let tokens = self.sess_prompt + self.sess_completion;
        // The cost is quoted in the configured price currency; the balance is
        // whatever the provider bills the account in. They can legitimately
        // differ (a CNY account using the USD price list), so when they do both
        // figures get their ISO code and the line never implies they're
        // comparable.
        let aligned = self
            .balance
            .as_ref()
            .map(|b| b.currency == self.price_currency)
            .unwrap_or(true);
        if tokens > 0 {
            let cost = self.sess_prompt as f64 / 1e6 * self.price_in
                + self.sess_completion as f64 / 1e6 * self.price_out;
            let cost = if aligned {
                money::fmt_cost(cost, &self.price_currency)
            } else {
                money::fmt_cost_tagged(cost, &self.price_currency)
            };
            spans.push(sep());
            spans.push(Span::styled(format!("{cost} · {} tok", humanize(tokens)), gray));
        }

        spans.push(sep());
        spans.push(Span::styled("ctx ", gray));
        let frac = self.last_prompt_tokens as f64 / self.ctx_limit as f64;
        spans.extend(bar(frac, 8, self.palette.accent));
        spans.push(Span::styled(
            format!(" {}%", (frac.clamp(0.0, 1.0) * 100.0).round() as u32),
            gray,
        ));

        if let Some(b) = &self.balance {
            spans.push(sep());
            let bal = if aligned { b.render() } else { b.render_tagged() };
            spans.push(Span::styled(format!("bal {bal}"), gray));
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
        let down = if self.ascii { "| v" } else { "↓" };
        // "new" only when something actually arrived; a manual scroll-up just
        // hints that the live end is below. Kept near the front of the line
        // (before the shift+tab hint and version) so a narrow terminal can't
        // clip it away.
        let hint = if !self.scrolled_up {
            None
        } else if self.new_below {
            Some(Span::styled(
                format!("  {down} new"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Some(Span::styled(format!("  {down} end"), Style::default().fg(self.dim_text())))
        };
        let mut spans = vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(text, Style::default().fg(color)),
        ];
        spans.extend(hint);
        spans.push(Span::styled("  (shift+tab/ctrl+p to cycle)", Style::default().fg(self.dim_text())));
        spans.push(Span::styled(format!("   picoder v{}", env!("CARGO_PKG_VERSION")), Style::default().fg(self.dim_text())));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

#[cfg(test)]
mod tests {
    use super::helpers::clean_text;
    use super::palette::palette_by_name;
    use super::{
        palette_for, want_mouse_capture, App, ConfigPatch, Handles, KeyCode, KeyEvent,
        KeyModifiers, Mode, THEMES, UiConfig, UiEvent, WorkerCmd,
    };

    #[test]
    fn clean_text_passes_plain_text_through_borrowed() {
        assert!(matches!(clean_text("hello world", true), std::borrow::Cow::Borrowed(_)));
        assert!(matches!(clean_text("héllo ❯ wörld", false), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn clean_text_strips_escapes_and_expands_tabs() {
        // ANSI color codes from tool output must not reach the terminal — and
        // neither may their parameter bytes: `ls --color` emits `ESC [ 1 ; 31 m`
        // and dropping only the ESC leaves `[1;31m` on screen as literal text.
        assert_eq!(clean_text("\x1b[31mred\x1b[0m", false), "red");
        assert_eq!(clean_text("\x1b[1;31mERROR\x1b[0m: disk full", false), "ERROR: disk full");
        assert_eq!(clean_text("a\tb", false), "a    b");
        assert_eq!(clean_text("a\rb\x07", false), "ab");
    }

    #[test]
    fn clean_text_eats_whole_escape_sequences() {
        // OSC title, two-character charset, and an unterminated trailing ESC.
        assert_eq!(clean_text("\x1b]0;title\x07x", false), "x");
        assert_eq!(clean_text("\x1b]0;t\x1b\\x", false), "x");
        assert_eq!(clean_text("\x1b(Bbold", false), "bold");
        assert_eq!(clean_text("dangling\x1b", false), "dangling");
        // A CSI without its final byte must not eat the rest of the line.
        assert_eq!(clean_text("\x1b[32mok", false), "ok");
    }

    /// A pasted snippet often carries raw escapes (copied out of a colored
    /// terminal or a log file). Drawn into the frame they desync the terminal
    /// from ratatui's cell grid, so every later cell comes out in the wrong
    /// color until a full repaint. Control bytes must never reach the buffer.
    #[test]
    fn pasted_control_characters_never_reach_the_composer() {
        let mut app = App::new(test_ui_config(), Vec::new());
        app.on_paste("safe \x1b[31mred\x1b[0m text\tend".to_string());
        assert_eq!(app.input, "safe [31mred[0m text end");
        assert_eq!(app.cursor, app.input.chars().count());
    }

    /// On a 16-color console the console ignores `ESC[38;2;…m`, so a theme's RGB
    /// shades made text come out in whatever color was last in effect. The
    /// adapted palette must contain no RGB left at all (and must stay usable:
    /// readable text, a visible highlight band).
    #[test]
    fn a_16color_terminal_gets_a_palette_with_no_rgb_left_in_it() {
        use ratatui::style::Color;
        for theme in THEMES {
            let safe = super::palette::for_16color(palette_by_name(theme));
            for (name, c) in [
                ("accent", safe.accent),
                ("assistant", safe.assistant),
                ("assistant_glyph", safe.assistant_glyph),
                ("reasoning", safe.reasoning),
                ("tool", safe.tool),
                ("tool_result", safe.tool_result),
                ("notice", safe.notice),
                ("code", safe.code),
                ("heading", safe.heading),
                ("diff_add", safe.diff_add),
                ("diff_del", safe.diff_del),
                ("diff_ctx", safe.diff_ctx),
                ("error", safe.error),
                ("chrome", safe.chrome),
                ("secondary", safe.secondary),
            ] {
                assert!(
                    !matches!(c, Color::Rgb(..)),
                    "{theme}: {name} is still RGB on a 16-color terminal"
                );
            }
            assert_eq!(safe.user_bg, Color::DarkGray, "{theme}: band stays visible");
            // A truecolor terminal is left completely alone.
            let full = palette_for(theme);
            assert_eq!(full.assistant, palette_by_name(theme).assistant);
        }
        // The default theme's near-white still reads as a bright color, and the
        // dim gray still as a dim one: the snap must not collapse them together.
        let safe = super::palette::for_16color(palette_by_name("Default"));
        assert_eq!(safe.assistant, Color::White);
        assert_eq!(safe.reasoning, Color::DarkGray);
    }

    #[test]
    fn clean_text_ascii_replaces_non_single_width() {
        assert_eq!(clean_text("ok 🚀 漢", true), "ok ? ?");
        assert_eq!(clean_text("héllo", true), "héllo");
        assert_eq!(clean_text("🚀\t", false), "🚀    ");
    }

    /// The permission-mode binding is advertised as "shift+tab/ctrl+p" in the
    /// status bar, so every spelling of it must cycle — in every mode. The
    /// refactor once dropped this to a single `BackTab` arm inside `Idle`, which
    /// silently broke Shift+Tab under the Kitty keyboard protocol (where it
    /// arrives as Tab+SHIFT and fell through to Tab autocomplete) and Ctrl+P
    /// everywhere, while the hint still promised both.
    #[test]
    fn shift_tab_and_ctrl_p_cycle_permissions_in_every_mode() {
        let cases: [(&str, KeyEvent); 3] = [
            ("BackTab (ANSI)", KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
            ("Tab+SHIFT (Kitty)", KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)),
            ("Ctrl+P", KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        ];
        // Idle, Busy (agent running) and an approval prompt all honour it.
        for mode in [Mode::Idle, Mode::Busy, Mode::Approval("edit x".into())] {
            for (name, key) in cases {
                let mut app = App::new(test_ui_config(), Vec::new());
                let (h, _rx) = test_handles();
                app.mode = mode.clone();
                app.on_key(key, &h);
                assert_eq!(
                    app.perm(),
                    crate::agent::PERM_AUTO,
                    "{name} in {mode:?} must cycle ask → bypass"
                );
                assert_eq!(app.input, "", "{name} must not type into the composer");
            }
        }

        // A masked prompt and an ask_user question capture every key, so a Tab
        // or Ctrl+P there must not leak out as a mode change.
        for mode in [Mode::Password { prompt: "pw".into() }, Mode::Question { prompt: "q".into() }] {
            let mut app = App::new(test_ui_config(), Vec::new());
            let (h, _rx) = test_handles();
            app.mode = mode.clone();
            app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL), &h);
            assert_eq!(app.perm(), 0, "{mode:?} must keep the captured key");
        }
    }

    /// A tool result with newlines keeps its line structure. The transcript used
    /// to hand the whole multi-line string to the wrapper, which turned every
    /// newline into a space and reflowed `ls`/stack-trace output into one
    /// garbled paragraph.
    #[test]
    fn multi_line_tool_output_keeps_its_lines() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = App::new(test_ui_config(), Vec::new());
        app.transcript.clear();
        app.push(
            crate::ui::types::Kind::ToolResult,
            "total 4\n-rw-r--r-- 1 a b\nroot -> /private/root".to_string(),
        );
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let screen = draw(&mut app, &mut term);
        assert!(screen.contains("total 4"), "{screen}");
        assert!(screen.contains("-rw-r--r-- 1 a b"), "second line survived:\n{screen}");
        assert!(screen.contains("root -> /private/root"), "third line survived:\n{screen}");
        assert!(
            !screen.contains("total 4 -rw-r--r--"),
            "lines must not be reflowed together:\n{screen}"
        );
    }

    /// The user's own prompt is echoed into the transcript, tagged so it draws
    /// with the full-width band that makes it scannable while scrolling back.
    #[test]
    fn sending_a_message_echoes_the_prompt_into_the_transcript() {
        let (mut app, h) = scrollable_app();
        app.mode = Mode::Idle;
        app.input = "rename the field".to_string();
        app.cursor = app.input.chars().count();
        app.submit(&h);
        let echoed = app
            .transcript
            .iter()
            .find(|l| crate::ui::types::Kind::User == l.kind)
            .expect("the prompt is shown");
        assert_eq!(echoed.text, "rename the field");
        assert!(matches!(app.mode, Mode::Busy), "the turn started");
    }

    /// A `Handles` backed by throwaway channels, plus the receiver so a test
    /// can assert what the UI sent to the worker.
    fn test_handles() -> (Handles, std::sync::mpsc::Receiver<WorkerCmd>) {
        use std::sync::atomic::{AtomicBool, AtomicU8};
        use std::sync::mpsc;
        use std::sync::Arc;

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (appr_tx, _appr_rx) = mpsc::channel();
        let h = Handles {
            join: std::thread::spawn(|| {}),
            cmd_tx,
            appr_tx,
            shared: crate::agent::Shared {
                cancel: Arc::new(AtomicBool::new(false)),
                perm: Arc::new(AtomicU8::new(0)),
            },
        };
        (h, cmd_rx)
    }

    /// A `UiConfig` with test-sized numbers; `price_*` are zeroed so costs are
    /// deterministic unless a test sets them.
    fn test_ui_config() -> UiConfig {
        use std::sync::atomic::AtomicU8;
        use std::sync::Arc;

        UiConfig {
            model: "m0".into(),
            theme: "Default".into(),
            ascii: false,
            ctx_limit: 128_000,
            price_in: 0.0,
            price_out: 0.0,
            price_currency: crate::money::Currency::default(),
            perm: Arc::new(AtomicU8::new(0)),
            settings: crate::config::Config::default(),
        }
    }

    /// The real key path (`App::on_key` → `Mode::Select`) used by `/model`:
    /// a mis-guarded match arm silently swallowed plain Up/Down while the
    /// Ctrl+j/k aliases still worked.
    fn select_app(models: &[&str]) -> (App, Handles, std::sync::mpsc::Receiver<WorkerCmd>) {
        let mut app = App::new(test_ui_config(), Vec::new());
        let (h, cmd_rx) = test_handles();
        app.handle_event(
            UiEvent::ModelList(models.iter().map(|m| m.to_string()).collect()),
            &h,
        );
        assert!(
            matches!(app.mode, Mode::Select),
            "ModelList opens the picker"
        );
        (app, h, cmd_rx)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// An idle app whose transcript is taller than the viewport, so scrolling
    /// has somewhere to go. `max_top`/`view_h` are normally set by the render
    /// pass; a unit test pokes them directly.
    fn scrollable_app() -> (App, Handles) {
        let mut app = App::new(test_ui_config(), vec!["first".into(), "second".into()]);
        app.max_top = 40;
        app.view_h = 10;
        app.scroll = 40; // pinned to the live end
        let (h, _rx) = test_handles();
        (app, h)
    }

    #[test]
    fn arrows_roll_the_transcript_and_only_ctrl_arrows_recall_history() {
        let (mut app, h) = scrollable_app();

        // Plain Up/Down move the view, not the composer history.
        app.on_key(key(KeyCode::Up), &h);
        assert_eq!(app.scroll, 37, "Up rolls the output back a step");
        assert!(!app.follow, "scrolling up pins the view");
        assert!(app.scrolled_up, "the more-below hint shows");
        assert_eq!(app.input, "", "Up must not type history into the composer");

        app.on_key(key(KeyCode::Down), &h);
        assert_eq!(app.scroll, 40);
        assert!(app.follow, "returning to the live end resumes following");
        assert!(!app.scrolled_up);

        app.on_key(key(KeyCode::Up), &h);
        assert!(app.scrolled_up);
        assert!(!app.new_below, "a manual scroll-up has nothing new below");

        // New output while pinned flags the hint as "new" and leaves the
        // user's reading position alone.
        app.handle_event(UiEvent::Token("streaming".into()), &h);
        app.handle_event(UiEvent::AssistantCommit, &h);
        assert!(app.new_below, "arriving output is flagged as new");
        assert!(!app.follow, "...without yanking the view to the bottom");
        assert_eq!(app.scroll, 37, "the reading position is preserved");

        app.on_key(key(KeyCode::Down), &h);
        assert!(app.follow);
        assert!(!app.scrolled_up && !app.new_below, "hint clears at the end");

        // Ctrl+Up is where history recall lives now.
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), &h);
        assert_eq!(app.input, "second", "Ctrl+Up recalls the previous prompt");
        assert_eq!(app.scroll, 40, "history recall doesn't move the view");
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), &h);
        assert_eq!(app.input, "", "Ctrl+Down returns to the empty draft");
    }

    #[test]
    fn scrolling_clamps_at_both_ends_and_never_pins_an_unscrollable_view() {
        let (mut app, h) = scrollable_app();
        for _ in 0..30 {
            app.on_key(key(KeyCode::Down), &h);
        }
        assert_eq!(app.scroll, 40, "clamped at the live end");
        assert!(app.follow);

        for _ in 0..30 {
            app.on_key(key(KeyCode::Up), &h);
        }
        assert_eq!(app.scroll, 0, "clamped at the top");
        assert!(!app.follow);

        // Short transcript: nothing above the fold, so Up is a no-op and we
        // don't tease a scroll that can't happen.
        app.max_top = 0;
        app.follow = true;
        app.scrolled_up = false;
        app.new_below = false;
        app.on_key(key(KeyCode::Up), &h);
        assert_eq!((app.scroll, app.follow, app.scrolled_up), (0, true, false));
    }

    #[test]
    fn page_keys_move_a_full_screenful() {
        let (mut app, h) = scrollable_app();
        app.on_key(key(KeyCode::PageUp), &h);
        assert_eq!(app.scroll, 30, "PgUp moves one viewport (view_h = 10)");
        app.on_key(key(KeyCode::PageDown), &h);
        assert_eq!(app.scroll, 40);
        assert!(app.follow);
    }

    #[test]
    fn sending_a_message_snaps_back_to_the_live_end() {
        let (mut app, h) = scrollable_app();
        app.on_key(key(KeyCode::Up), &h);
        app.on_key(key(KeyCode::Up), &h);
        assert!(!app.follow);

        for c in "hello".chars() {
            app.on_key(key(KeyCode::Char(c)), &h);
        }
        app.on_key(key(KeyCode::Enter), &h);
        assert!(app.follow, "Enter returns to the live end for the reply");
        assert!(!app.scrolled_up, "and drops the new-output hint");
        assert!(!app.new_below);
    }

    #[test]
    fn scrolling_works_while_the_agent_is_busy() {
        let (mut app, h) = scrollable_app();
        app.mode = Mode::Busy;
        app.on_key(key(KeyCode::Up), &h);
        assert_eq!(app.scroll, 37, "queued-input mode scrolls too");
        assert!(!app.follow);
        assert!(matches!(app.mode, Mode::Busy), "still busy, not interrupted");
    }

    #[test]
    fn mouse_capture_defaults_to_on_except_on_warp_and_honours_the_override() {
        assert!(want_mouse_capture("iTerm.app", None));
        assert!(want_mouse_capture("", None));
        assert!(!want_mouse_capture("WarpTerminal", None));

        assert!(want_mouse_capture("WarpTerminal", Some("1")));
        assert!(want_mouse_capture("WarpTerminal", Some("true")));
        assert!(want_mouse_capture("WarpTerminal", Some("ON")));
        assert!(!want_mouse_capture("iTerm.app", Some("0")));
        assert!(!want_mouse_capture("iTerm.app", Some("off")));
        // Unrecognised value: fall back to the terminal-based default.
        assert!(!want_mouse_capture("WarpTerminal", Some("maybe")));
        assert!(want_mouse_capture("iTerm.app", Some("maybe")));
    }

    /// The mismatch notice: fired once, only when the units actually differ,
    /// and worded so the fix is obvious.
    #[test]
    fn a_balance_in_another_currency_is_flagged_once_with_the_fix() {
        let bal = |code: &str| crate::money::Balance {
            amount: "135.70".into(),
            currency: crate::money::Currency::parse(code),
        };
        let (mut app, h) = scrollable_app();
        let notices = |app: &App| {
            app.transcript
                .iter()
                .filter(|l| l.text.contains("balance is in"))
                .count()
        };

        // Aligned (USD prices, USD account): no notice at all.
        app.handle_event(UiEvent::Balance(bal("USD")), &h);
        assert_eq!(notices(&app), 0);
        assert_eq!(app.balance.as_ref().unwrap().render(), "$135.70");

        // Mismatch: one notice naming both currencies and the config key, and
        // the balance still updates on later refreshes without re-noticing.
        app.handle_event(UiEvent::Balance(bal("CNY")), &h);
        assert_eq!(notices(&app), 1);
        let text = app
            .transcript
            .iter()
            .find(|l| l.text.contains("balance is in"))
            .unwrap()
            .text
            .clone();
        assert!(text.contains("CNY") && text.contains("USD"), "{text}");
        assert!(
            text.contains("price_currency"),
            "it says how to fix it: {text}"
        );
        assert_eq!(app.balance.as_ref().unwrap().render(), "¥135.70");

        app.handle_event(UiEvent::Balance(bal("CNY")), &h);
        assert_eq!(notices(&app), 1, "not repeated on every turn");
    }

    /// `/config` row 11 drives the cost unit: committing a code re-labels the
    /// status line live and patches the worker so it also re-reads the balance
    /// with that preference.
    #[test]
    fn the_config_panel_can_switch_the_price_currency() {
        let (mut app, _h, cmd_rx) = select_app(&["m0"]);
        app.mode = Mode::Idle;
        app.settings.price_currency = "USD".into();
        let (h2, cmd_rx2) = test_handles();

        app.commit_setting(11, "cny".into(), &h2);
        assert_eq!(app.price_currency.code, "CNY");
        assert_eq!(app.settings.price_currency, "CNY");
        assert!(
            matches!(cmd_rx2.try_recv(), Ok(WorkerCmd::Patch(ConfigPatch::PriceCurrency(c))) if c == "CNY"),
            "the worker is told so the balance preference follows"
        );
        // An unrecognized code keeps the user's text but prints it as-is, so the
        // line is never silently mislabelled as dollars.
        app.commit_setting(11, "xyz".into(), &h2);
        assert_eq!(app.price_currency.code, "XYZ");
        assert!(app.price_currency.symbol.is_empty());
        assert!(cmd_rx.try_recv().is_err());
    }

    /// The status line's two money readouts, as drawn. This is the bug being
    /// guarded: a hardcoded `$` on the cost while the balance rendered as `¥`.
    #[test]
    fn status_line_money_readouts_never_mix_currencies_silently() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let cny = crate::money::Currency::parse("cny");
        let bal = |code: &str, amount: &str| crate::money::Balance {
            amount: amount.into(),
            currency: crate::money::Currency::parse(code),
        };

        // A CNY-billed account (balance ¥135.70) with the default USD price
        // list: both figures must carry their ISO code, because a `$0.0300`
        // next to `¥135.70` looks comparable and isn't.
        let mut app = App::new(test_ui_config(), Vec::new());
        app.price_in = 0.14;
        app.price_out = 0.28;
        app.sess_prompt = 100_000;
        app.sess_completion = 50_000;
        app.balance = Some(bal("CNY", "135.70"));
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let screen = draw(&mut app, &mut term);
        assert!(
            screen.contains("$0.03 USD"),
            "cost is unit-labelled:\n{screen}"
        );
        assert!(
            screen.contains("bal ¥135.70 CNY"),
            "balance is too:\n{screen}"
        );

        // Set the price currency to CNY (and use DeepSeek's CNY list) and the
        // readouts line up in one currency, with no tags.
        app.price_currency = cny.clone();
        app.price_in = 1.0;
        app.price_out = 2.0;
        let screen = draw(&mut app, &mut term);
        assert!(
            screen.contains("¥0.20 · 150.0k tok"),
            "cost in CNY:\n{screen}"
        );
        assert!(screen.contains("bal ¥135.70"), "balance in CNY:\n{screen}");
        assert!(!screen.contains("USD"), "no leftover USD:\n{screen}");

        // A USD account with the default prices reads plainly, as before: no
        // currency tags, because there is nothing to disambiguate.
        app.price_currency = crate::money::Currency::default();
        app.price_in = 0.14;
        app.price_out = 0.28;
        app.balance = Some(bal("USD", "12.34"));
        let screen = draw(&mut app, &mut term);
        assert!(screen.contains("$0.03 · 150.0k tok"), "cost:\n{screen}");
        assert!(screen.contains("bal $12.34"), "balance:\n{screen}");
        assert!(
            !screen.contains("CNY") && !screen.contains("USD"),
            "untagged when aligned:\n{screen}"
        );

        // Sub-cent sessions keep four decimals so a short turn isn't "$0.00".
        app.sess_prompt = 10_000;
        app.sess_completion = 0;
        let screen = draw(&mut app, &mut term);
        assert!(
            screen.contains("$0.0014 · 10.0k tok"),
            "sub-cent cost:\n{screen}"
        );
    }

    /// The real render path: an overflowing transcript, a small screen, and the
    /// status bar read back from the backend buffer. Guards against the scroll
    /// state being right while nothing visible changes.
    fn draw(app: &mut App, term: &mut ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer();
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_output_actually_moves_and_the_hint_shows_in_the_rendered_screen() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (mut app, h) = scrollable_app();
        app.max_top = 0;
        app.view_h = 0;
        for i in 0..80 {
            app.note(format!("transcript line {i}"));
        }
        let mut term = Terminal::new(TestBackend::new(60, 16)).unwrap();

        let first = draw(&mut app, &mut term); // sets max_top from the real layout
        assert!(app.max_top > 0, "the test transcript overflows the view");
        assert!(first.contains("transcript line 79"), "the live end is visible");
        assert!(!first.contains("end"), "no hint while following");

        app.on_key(key(KeyCode::Up), &h);
        let scrolled = draw(&mut app, &mut term);
        assert!(
            !scrolled.contains("transcript line 79"),
            "the live end scrolled off the bottom"
        );
        assert!(
            scrolled.contains("↓ end"),
            "the status bar advertises the live end below:\n{scrolled}"
        );
        assert!(!scrolled.contains("↓ new"), "nothing new arrived yet");

        // Streaming while pinned keeps the position and flips the hint to "new".
        let before = app.scroll;
        app.handle_event(UiEvent::Token("a fresh reply".into()), &h);
        app.handle_event(UiEvent::AssistantCommit, &h);
        let streaming = draw(&mut app, &mut term);
        assert_eq!(app.scroll, before, "the reading position is preserved");
        assert!(
            streaming.contains("↓ new"),
            "new output is called out:\n{streaming}"
        );

        app.on_key(key(KeyCode::PageDown), &h);
        let bottom = draw(&mut app, &mut term);
        assert!(bottom.contains("a fresh reply"), "PgDn returns to the live end");
        assert!(!bottom.contains("end") && !bottom.contains("new"));
    }

    /// The transcript separates blocks with blank lines so turns, prose, and
    /// tool activity don't run together — but only at real boundaries: a notice
    /// or a tool result that belongs to the preceding block stays attached.
    #[test]
    fn transcript_inserts_blank_lines_between_blocks() {
        use super::needs_sep;
        use crate::ui::types::Kind;

        // A new prompt always opens a fresh, roomy block…
        assert!(needs_sep(Kind::Assistant, Kind::User));
        assert!(needs_sep(Kind::ToolResult, Kind::User));
        // …but two prompts in a row are one block, and the banner pads itself.
        assert!(!needs_sep(Kind::User, Kind::User));
        assert!(!needs_sep(Kind::Banner, Kind::User));

        // Prose → tools, and tools → the reply that follows, both get a gap.
        assert!(needs_sep(Kind::Assistant, Kind::Tool));
        assert!(needs_sep(Kind::ToolResult, Kind::Tool));
        assert!(needs_sep(Kind::ToolResult, Kind::Assistant));
        assert!(needs_sep(Kind::Reasoning, Kind::Assistant));

        // A notice or a result never detaches itself from its block.
        assert!(!needs_sep(Kind::Assistant, Kind::Notice));
        assert!(!needs_sep(Kind::Tool, Kind::ToolResult));
        assert!(!needs_sep(Kind::Assistant, Kind::Assistant));
    }

    /// The whole point of the separator: pushing a prompt after a reply leaves a
    /// blank line in the transcript, and the renderer draws it as an empty row.
    #[test]
    fn committed_blocks_are_separated_by_a_blank_row() {
        let mut app = App::new(test_ui_config(), Vec::new());
        app.push(Kind::Assistant, "first answer");
        app.push(Kind::User, "second question");
        let kinds: Vec<Kind> = app.transcript.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![Kind::Assistant, Kind::Blank, Kind::User],
            "a blank line lands between the turn's answer and the next prompt"
        );
        // The blank row renders as a genuinely empty line, not stray padding.
        app.ensure_display_cache(40);
        let blank = app
            .disp_cache
            .iter()
            .find(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
            .expect("the separator renders as an empty row");
        assert!(blank.spans.iter().all(|s| s.content.is_empty()));
    }

    #[test]
    fn model_picker_moves_with_arrows_and_enters_the_highlighted_model() {
        let (mut app, h, cmd_rx) = select_app(&["m0", "m1", "m2"]);

        app.on_key(key(KeyCode::Down), &h);
        app.on_key(key(KeyCode::Down), &h);
        assert_eq!(app.picker.as_ref().unwrap().cursor, 2, "Down moves down");
        app.on_key(key(KeyCode::Up), &h);
        assert_eq!(app.picker.as_ref().unwrap().cursor, 1, "Up moves back up");

        app.on_key(key(KeyCode::Enter), &h);
        assert!(matches!(app.mode, Mode::Idle), "Enter closes the picker");
        assert!(app.picker.is_none());
        assert!(
            matches!(cmd_rx.try_recv(), Ok(WorkerCmd::SetModel(m)) if m == "m1"),
            "Enter selects the highlighted model"
        );
    }

    #[test]
    fn model_picker_arrows_still_work_above_the_first_row() {
        let (mut app, h, _rx) = select_app(&["m0", "m1"]);
        app.on_key(key(KeyCode::Up), &h);
        assert_eq!(app.picker.as_ref().unwrap().cursor, 0, "clamped at the top");
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT), &h);
        assert_eq!(
            app.picker.as_ref().unwrap().cursor,
            1,
            "shifted arrows work too"
        );
    }

    #[test]
    fn model_picker_types_to_filter_without_stealing_j_k() {
        let (mut app, h, _rx) = select_app(&["deepseek-chat", "gpt-4o", "kimi-k2"]);
        for c in "kimi".chars() {
            app.on_key(key(KeyCode::Char(c)), &h);
        }
        let p = app.picker.as_ref().unwrap();
        assert_eq!(p.filter, "kimi");
        assert_eq!(p.filtered(), vec![2], "'k' filtered instead of moving up");
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

/// Whether to grab the mouse (needed for scroll-wheel events). Capture is on by
/// default, but skipped on Warp: it grabs click-drag text selection and Warp has
/// no modifier to bypass a grabbed mouse, so users couldn't copy anything. Other
/// terminals (iTerm, …) let you hold Option/Fn to select, so capture stays.
///
/// `PICODER_MOUSE` overrides either way: a truthy value (`1`/`true`/`on`/`yes`)
/// forces wheel scrolling on even under Warp, a falsy one (`0`/`false`/`off`/
/// `no`) turns capture off everywhere. Anything else means "decide from the
/// terminal". Scrolling is also on ↑/↓ and PgUp/PgDn, so a terminal without
/// capture still has a way to move through the transcript.
pub fn want_mouse_capture(term_program: &str, override_env: Option<&str>) -> bool {
    match override_env.map(str::to_ascii_lowercase).as_deref() {
        Some("1") | Some("true") | Some("on") | Some("yes") => true,
        Some("0") | Some("false") | Some("off") | Some("no") => false,
        _ => term_program != "WarpTerminal",
    }
}

/// Enter alt-screen + raw mode with bracketed paste; returns a ready Terminal.
pub fn setup_terminal() -> std::io::Result<Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>>
{
    let mut term = ratatui::init();
    let _ = execute!(std::io::stdout(), event::EnableBracketedPaste);
    let mouse = want_mouse_capture(
        &std::env::var("TERM_PROGRAM").unwrap_or_default(),
        std::env::var("PICODER_MOUSE").ok().as_deref(),
    );
    if mouse {
        let _ = execute!(std::io::stdout(), event::EnableMouseCapture);
    }
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
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, event::DisableMouseCapture, event::DisableBracketedPaste);
    ratatui::restore();
    if console {
        let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
    }
}

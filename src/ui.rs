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
use banner::{BRole, banner_lines};
use helpers::{
    bar, complete_path, fmt_cost, humanize, longest_common_prefix, pad1,
    perm_name, render_tline, setting_max_tool_calls,
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
    caps_char, BannerColor, CursorKind, GLYPHS_A, GLYPHS_U, Glyphs, Kind, Mode, PickAction,
    Picker, PICKER_VISIBLE, SETTING_LABELS, SLASH_COMMANDS, SPIN_A, SPIN_U, TLine,
    DOUBLE_PRESS_TIMEOUT, MAX_SUGGEST, MAX_TRANSCRIPT,
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
    /// True when new content was pushed while the user was scrolled up. Shows a
    /// "↓ new" indicator in the status bar.
    scrolled_up: bool,
    scroll: usize,
    max_top: usize,
    view_h: usize,
    spinner: usize,
    spin_counter: usize,
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
    last_prompt_tokens: u32,
    sess_prompt: u64,
    sess_completion: u64,
    balance: Option<String>,
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
            scroll: 0,
            max_top: 0,
            view_h: 0,
            spinner: 0,
            spin_counter: 0,
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
            palette: palette_by_name(&cfg.theme),
            disp_cache: Vec::new(),
            disp_cache_width: usize::MAX,
            disp_cache_tver: u64::MAX,
            tver: 0,
            perm: cfg.perm,
            ctx_limit: cfg.ctx_limit.max(1),
            price_in: cfg.price_in,
            price_out: cfg.price_out,
            last_prompt_tokens: 0,
            sess_prompt: 0,
            sess_completion: 0,
            balance: None,
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
        self.transcript.push(TLine { kind, text, lead: true, color: None });
        self.after_push();
    }

    fn dirty(&mut self) {
        self.tver = self.tver.wrapping_add(1);
    }

    fn set_palette(&mut self, p: Palette) {
        self.palette = p;
        self.dirty();
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
        }
    }

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
            self.transcript.push(TLine { kind: Kind::Assistant, text: std::mem::take(&mut self.live), lead: true, color: None });
            self.after_push();
        }
        if !self.live_reasoning.is_empty() {
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
                self.last_models = ids;
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
                self.mode = Mode::Idle;
                // The worker may reply with Bypass toggled; sync the UI.
                self.perm = h.shared.perm.clone();
                // Dispatch the next queued message, if any.
                if let Some(next) = self.queued.first() {
                    let _ = h.cmd_tx.send(WorkerCmd::User { text: next.clone(), images: vec![] });
                    self.queued.remove(0);
                    self.mode = Mode::Busy;
                }
            }
            UiEvent::Suggestion(s) => {
                self.suggestion = Some(s);
            }
        }
    }

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
                self.mode = Mode::Idle;
            }
            Mode::Settings { .. } | Mode::Select | Mode::ThemeSelect { .. } => {
                self.mode = Mode::Idle;
                self.picker = None;
            }
            Mode::Busy => self.interrupt(h),
            Mode::Idle => {
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
                self.mode = Mode::Busy;
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                let _ = h.appr_tx.send(ApprovalResponse::No);
                self.mode = Mode::Busy;
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                let _ = h.appr_tx.send(ApprovalResponse::Always);
                self.mode = Mode::Busy;
            }
            KeyCode::Esc => {
                let _ = h.appr_tx.send(ApprovalResponse::No);
                self.mode = Mode::Idle;
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
            _ => {}
        }
    }

    fn on_key_select(&mut self, key: KeyEvent, h: &Handles) {
        let Some(ref mut picker) = self.picker else {
            self.mode = Mode::Idle;
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Idle;
                self.picker = None;
            }
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let len = picker.filtered().len();
                if len > 0 {
                    picker.cursor = picker.cursor.saturating_sub(1).max(0);
                    picker.clamp(len);
                }
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let len = picker.filtered().len();
                if len > 0 {
                    picker.cursor = (picker.cursor + 1).min(len - 1);
                    picker.clamp(len);
                }
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
            KeyCode::Char(c) => {
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
        self.mode = Mode::Idle;
        // Clear the suggestion so the user sees the hint again.
        self.suggestion = None;
    }

    fn on_key_idle(&mut self, key: KeyEvent, h: &Handles) {
        if key.code == KeyCode::Esc {
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
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                match key.code {
                    KeyCode::Left => self.cursor = self.prev_word(),
                    KeyCode::Right => self.cursor = self.next_word(),
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
            KeyCode::Up | KeyCode::Down
                if !key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.suggest_idx = 0;
                if key.code == KeyCode::Up {
                    self.history_prev();
                } else {
                    self.history_next();
                }
            }
            KeyCode::Enter => {
                self.last_ctrl_c = None;
                self.submit(h);
                return;
            }
            KeyCode::BackTab => {
                self.cycle_perm();
            }
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
            let mut opts = complete_path(prefix);
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
            self.scroll_up();
        } else {
            self.scroll_down();
        }
    }

    fn scroll_up(&mut self) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(3);
    }

    fn scroll_down(&mut self) {
        self.scroll = (self.scroll + 3).min(self.max_top);
        if self.scroll >= self.max_top {
            self.follow = true;
            self.scrolled_up = false;
        }
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
        Some(text)
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
        self.mode = Mode::Busy;
        let (task_text, images) = self.prepare_message(text);
        let _ = h.cmd_tx.send(WorkerCmd::User { text: task_text, images });
    }

    fn prepare_message(&self, text: String) -> (String, Vec<String>) {
        let (task_text, _attached) = expand_attachments(&text);
        let (images, _img_names) = extract_images(&text);
        (task_text, images)
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
                self.mode = Mode::Busy;
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
                self.mode = Mode::Busy;
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
        self.push(Kind::Notice, "  ↑/↓         browse history".into());
        self.push(Kind::Notice, "  Tab         autocomplete".into());
        self.push(Kind::Notice, "  Shift+Tab   cycle permissions (ask / bypass / plan)".into());
        self.push(Kind::Notice, "  Esc         interrupt the agent".into());
        self.push(Kind::Notice, "  Ctrl+L      force clear/repaint".into());
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
            render_tline(&mut out, t.kind, &t.text, t.lead, t.color, width, self.glyphs, &self.palette, self.single_width);
        }
        self.disp_cache = out;
        self.disp_cache_width = width;
        self.disp_cache_tver = self.tver;
    }

    fn build_live_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
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
        if tokens > 0 {
            let cost = self.sess_prompt as f64 / 1e6 * self.price_in
                + self.sess_completion as f64 / 1e6 * self.price_out;
            spans.push(sep());
            spans.push(Span::styled(format!("{} · {} tok", fmt_cost(cost), humanize(tokens)), gray));
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
        let down = if self.ascii { "| v" } else { "↓" };
        let mut spans = vec![
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(text, Style::default().fg(color)),
            Span::styled("  (shift+tab/ctrl+p to cycle)", Style::default().fg(self.dim_text())),
            Span::styled(format!("   picoder v{}", env!("CARGO_PKG_VERSION")), Style::default().fg(self.dim_text())),
        ];
        if self.scrolled_up {
            spans.push(Span::styled(
                format!("  {down} new"),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

#[cfg(test)]
mod tests {
    use super::helpers::clean_text;

    #[test]
    fn clean_text_passes_plain_text_through_borrowed() {
        assert!(matches!(clean_text("hello world", true), std::borrow::Cow::Borrowed(_)));
        assert!(matches!(clean_text("héllo ❯ wörld", false), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn clean_text_strips_escapes_and_expands_tabs() {
        assert_eq!(clean_text("\x1b[31mred\x1b[0m", false), "[31mred[0m");
        assert_eq!(clean_text("a\tb", false), "a    b");
        assert_eq!(clean_text("a\rb\x07", false), "ab");
    }

    #[test]
    fn clean_text_ascii_replaces_non_single_width() {
        assert_eq!(clean_text("ok 🚀 漢", true), "ok ? ?");
        assert_eq!(clean_text("héllo", true), "héllo");
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

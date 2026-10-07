//! The agent worker: a long-lived thread that owns the conversation and runs
//! the blocking model/tool loop, talking to the UI thread over channels.

use crate::api::{self, AccumCall, Message};
use crate::config::{atomic_write, Config, ConfigPatch};
use crate::hooks::{Hooks, Outcome};
use crate::mcp::Mcp;
use crate::policy::{Decision, Policy};
use crate::system_prompt;
use crate::tools;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Fallback limit used when max_tool_calls is 0 ("auto"). High enough to let
/// the model converge naturally; low enough to catch runaway loops.
const AUTO_STEPS: usize = 500;
const SESSION_START_TAG: &str = "SessionStart hook context:";

/// Permission modes (cycled with Shift+Tab in the UI).
pub const PERM_ASK: u8 = 0; // prompt before each write/edit/bash
pub const PERM_AUTO: u8 = 1; // bypass — auto-approve everything
pub const PERM_PLAN: u8 = 2; // read-only — refuse writes/bash, ask the model to plan

/// Worker → UI.
pub enum UiEvent {
    Token(String),
    Reasoning(String),
    ResetLive,
    AssistantCommit,
    ToolStart { name: String, summary: String },
    Diff(String),
    ToolResult { ok: bool, preview: String },
    /// Ask the user to approve `desc`. `rule` is the permission rule the UI can
    /// offer as "don't ask again for this" (persisted via ApprovalResponse::Rule).
    Approval { desc: String, rule: Option<String> },
    /// sudo (via the askpass helper) needs a password. The UI pops a masked
    /// prompt and sends the result back over `reply` (None = user cancelled).
    PasswordRequest { prompt: String, reply: Sender<Option<String>> },
    /// The ask_user tool: the UI pops a visible input line and sends the
    /// answer back over `reply` (None = user declined).
    Question { prompt: String, reply: Sender<Option<String>> },
    ModelList(Vec<String>),
    /// `/rewind` choices, newest first: "N. prompt preview".
    TurnList(Vec<String>),
    /// After a rewind: the prompt that was undone, to put back in the composer.
    Rewound(String),
    ModelChanged(String),
    Usage { prompt: u32, completion: u32 },
    /// Tokens spent by a background call (compaction summary, follow-up
    /// suggestion): counted toward session cost, but not the ctx bar.
    Spend { prompt: u32, completion: u32 },
    /// Re-estimate of the next prompt's size (after compaction) so the UI's
    /// context bar drops immediately, without touching session totals.
    Context(u32),
    /// Auto-detected model context window (tokens): updates the ctx bar and the
    /// `/config` panel's displayed value live, without the user pinning it.
    ContextLimit(u32),
    /// Auth mode changed by the worker (e.g. after a successful /login), so the
    /// `/config` panel reflects "api" vs "sub" without a round-trip.
    AuthMode(String),
    /// Account balance, with the currency the provider billed it in (see
    /// `money::Balance`), so the status line can't mix units silently.
    Balance(crate::money::Balance),
    Notice(String),
    Error(String),
    TurnDone,
    /// A suggested follow-up prompt generated after the turn completes.
    Suggestion(String),
}

/// UI → Worker control messages (processed between turns).
pub enum WorkerCmd {
    User { text: String, images: Vec<String> },
    Reset,
    /// `/compact [focus]`: summarize older turns, optionally steering the
    /// summary toward what the user wants preserved.
    Compact(Option<String>),
    /// `!cmd` from the composer: run a shell command directly, show its output,
    /// and add both to the conversation so the model can see what the user saw.
    Shell(String),
    /// Add a note to the conversation (as the user) without starting a turn —
    /// e.g. "/undo reverted commit X" so the model's picture of the tree stays true.
    Inject(String),
    /// Permission rules or hooks changed on disk: re-read them.
    ReloadSettings,
    SetModel(String),
    /// Run the OAuth subscription flow and store the token. No UI sends it
    /// since `/login` was hidden; kept for when native adapters exist.
    #[allow(dead_code)]
    Login(String),
    /// A `/config` panel change: apply to the live config and persist.
    Patch(ConfigPatch),
    ListModels,
    ListMcp,
    /// `/rewind`: list this session's prompts for the picker.
    ListTurns,
    /// Rewind to just before turn N (0-based, oldest first): undo picoder's
    /// checkpoints since then and drop that prompt and everything after it.
    Rewind(usize),
    /// `/new`: delete the session file and reset to a clean slate.
    New,
    Quit,
}

#[derive(Clone)]
pub enum ApprovalResponse {
    Yes,
    No,
    Always,
    /// Yes, and persist this allow rule so the same call is never asked again.
    Rule(String),
}

pub struct Shared {
    pub cancel: Arc<AtomicBool>,
    pub perm: Arc<AtomicU8>,
}

pub struct Handles {
    pub join: JoinHandle<()>,
    pub cmd_tx: Sender<WorkerCmd>,
    pub appr_tx: Sender<ApprovalResponse>,
    pub shared: Shared,
}

struct Worker {
    http: ureq::Agent,
    cfg: Config,
    messages: Vec<Message>,
    system_len: usize,
    cancel: Arc<AtomicBool>,
    /// Cancels the in-flight follow-up suggestion call from the previous turn
    /// so a superseded suggestion neither bills nor surfaces stale.
    suggest_cancel: Arc<AtomicBool>,
    perm: Arc<AtomicU8>,
    ui: Sender<UiEvent>,
    appr_rx: Receiver<ApprovalResponse>,
    session: Option<PathBuf>,
    /// Prompt tokens of the last completion, for the auto-compaction trigger.
    last_prompt: u32,
    /// True while a sub-agent is running: suppresses streaming the sub-agent's
    /// tokens as the main reply, and blocks nested `task` calls.
    quiet: bool,
    /// Images queued by view_image, injected as a user message after the
    /// current round of tool results.
    pending_images: Vec<String>,
    /// Launched MCP servers and their tools.
    mcp: Mcp,
    /// Built-in + MCP tool schema, rebuilt once at startup; sent each request.
    tools: Value,
    /// Permission rules (allow/deny) merged from config + project settings.
    policy: Policy,
    /// Lifecycle hooks (PreToolUse, PostToolUse, UserPromptSubmit, Stop).
    hooks: Hooks,
    /// Hashes of this turn's tool calls, newest last — thrashing detection.
    recent_calls: Vec<u64>,
    /// Context window the provider's `/models` advertised, tagged with the
    /// model it was fetched for (filled by a background thread). Preferred
    /// over the built-in table when sizing auto-compaction.
    ctx_detected: Arc<Mutex<Option<(String, u32)>>>,
    /// Where each prompt of this run starts, for `/rewind`.
    turns: Vec<TurnMark>,
}

/// A user prompt `/rewind` can go back to: its index in `messages` and the
/// git HEAD when it was sent (None without a checkpointed repo).
#[derive(Clone)]
struct TurnMark {
    at: usize,
    head: Option<String>,
    prompt: String,
}

pub fn spawn(
    cfg: Config,
    messages: Vec<Message>,
    perm_start: u8,
    session: Option<PathBuf>,
    ui: Sender<UiEvent>,
) -> Handles {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
    let (appr_tx, appr_rx) = mpsc::channel::<ApprovalResponse>();
    let cancel = Arc::new(AtomicBool::new(false));
    let perm = Arc::new(AtomicU8::new(perm_start));
    let shared = Shared { cancel: cancel.clone(), perm: perm.clone() };

    let join = std::thread::spawn(move || {
        // The protected prefix is the leading run of system messages (prompt,
        // memory, project context, git state). Counting by role — rather than
        // taking the startup message count — keeps /reset and /compact correct
        // for resumed sessions, where startup messages span the whole history.
        let mut messages = messages;
        let policy = Policy::load();
        let hooks = Hooks::load();
        if !policy.is_empty() {
            let _ = ui.send(UiEvent::Notice(format!(
                "permission rules: {} allow, {} deny (/permissions)",
                policy.allow.len(),
                policy.deny.len()
            )));
        }
        if !hooks.is_empty() {
            let _ = ui.send(UiEvent::Notice(format!("hooks: {} configured (/hooks)", hooks.hooks.len())));
        }
        if hooks.has("SessionStart") {
            if let Outcome::Continue { context } = hooks.run("SessionStart", "", &serde_json::json!({})) {
                // A resumed session carries the previous launch's copy; replace
                // it rather than stacking one per launch.
                messages.retain(|m| !(m.role == "system" && m.content.starts_with(SESSION_START_TAG)));
                if !context.trim().is_empty() {
                    let at = system_prefix_len(&messages);
                    messages.insert(at, Message::system(format!("{SESSION_START_TAG}\n{context}")));
                }
            }
        }
        let system_len = system_prefix_len(&messages);
        // Launch MCP servers before the loop (can take a moment per server).
        let mcp = if cfg.mcp_servers.is_empty() {
            Mcp::disabled()
        } else {
            let _ = ui.send(UiEvent::Notice(format!(
                "starting {} MCP server(s)…",
                cfg.mcp_servers.len()
            )));
            let mcp = Mcp::launch(&cfg.mcp_servers);
            for s in mcp.status() {
                let msg = format!("mcp {}: {}", s.name, s.detail);
                let _ = ui.send(if s.ok { UiEvent::Notice(msg) } else { UiEvent::Error(msg) });
            }
            mcp
        };
        let tools = api::tools_spec_with(mcp.tools());
        // Estimate prompt tokens from the (possibly resumed) message list so
        // auto-compaction triggers correctly on the first turn rather than
        // starting from zero and never firing.
        let est_prompt = estimate_tokens(&messages);
        let mut w = Worker {
            http: api::agent_http(),
            cfg,
            messages,
            system_len,
            cancel,
            suggest_cancel: Arc::new(AtomicBool::new(true)),
            perm,
            ui,
            appr_rx,
            session,
            last_prompt: est_prompt,
            quiet: false,
            pending_images: Vec::new(),
            mcp,
            tools,
            policy,
            hooks,
            recent_calls: Vec::new(),
            ctx_detected: Arc::new(Mutex::new(None)),
            turns: Vec::new(),
        };
        w.ensure_oauth_fresh(); // keep a resumed subscription login authenticated
        w.refresh_balance(); // initial account balance for the status line
        w.refresh_context_window(); // refine the ctx window from the provider
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                WorkerCmd::Quit => break,
                WorkerCmd::Reset => {
                    let old_len = w.messages.len();
                    w.messages.truncate(w.system_len);
                    w.turns.clear();
                    w.last_prompt = 0;
                    if w.messages.len() != old_len {
                        w.save_session();
                    }
                    let _ = w.ui.send(UiEvent::Notice("context cleared.".into()));
                    let _ = w.ui.send(UiEvent::Context(0));
                }
                WorkerCmd::New => {
                    // Delete the session file so we start fresh.
                    if let Some(ref path) = w.session {
                        let _ = std::fs::remove_file(path);
                    }
                    w.messages.truncate(w.system_len);
                    w.turns.clear();
                    w.last_prompt = 0;
                    let _ = w.ui.send(UiEvent::Notice("new session — fresh start.".into()));
                    let _ = w.ui.send(UiEvent::Context(0));
                }
                WorkerCmd::Compact(focus) => {
                    w.cancel.store(false, Ordering::Relaxed);
                    if w.compact(focus.as_deref()) {
                        w.save_session();
                    }
                    let _ = w.ui.send(UiEvent::TurnDone);
                }
                WorkerCmd::Shell(cmd) => {
                    w.cancel.store(false, Ordering::Relaxed);
                    w.run_shell(&cmd);
                    w.save_session();
                    let _ = w.ui.send(UiEvent::TurnDone);
                }
                WorkerCmd::Inject(note) => {
                    w.messages.push(Message::user(note));
                    w.save_session();
                }
                WorkerCmd::ReloadSettings => {
                    w.policy = Policy::load();
                    w.hooks = Hooks::load();
                    let _ = w.ui.send(UiEvent::Notice(format!(
                        "settings reloaded: {} allow / {} deny rule(s), {} hook(s)",
                        w.policy.allow.len(),
                        w.policy.deny.len(),
                        w.hooks.hooks.len()
                    )));
                }
                WorkerCmd::SetModel(m) => {
                    w.cfg.model = m.clone();
                    w.cfg.persist_model();
                    w.refresh_context_window(); // new model may have a different window
                    let _ = w.ui.send(UiEvent::ModelChanged(m.clone()));
                    let _ = w.ui.send(UiEvent::Notice(format!("model set to {m}")));
                }
                WorkerCmd::Login(name) => {
                    w.login(&name);
                    let _ = w.ui.send(UiEvent::TurnDone);
                }

                WorkerCmd::ListModels => {
                    match api::list_models(&w.http, &w.cfg) {
                        Ok(ids) => {
                            let _ = w.ui.send(UiEvent::ModelList(ids));
                        }
                        Err(e) => {
                            let _ = w.ui.send(UiEvent::Error(format!("could not fetch models: {e}")));
                        }
                    }
                    let _ = w.ui.send(UiEvent::TurnDone);
                }
                WorkerCmd::ListTurns => {
                    let items: Vec<String> = w
                        .turns
                        .iter()
                        .enumerate()
                        .rev()
                        .map(|(i, t)| {
                            let line: String = t.prompt.chars().take(80).collect();
                            let more = if t.prompt.chars().count() > 80 { "…" } else { "" };
                            format!("{}. {line}{more}", i + 1)
                        })
                        .collect();
                    if items.is_empty() {
                        let _ = w.ui.send(UiEvent::Notice("nothing to rewind in this session yet.".into()));
                    }
                    let _ = w.ui.send(UiEvent::TurnList(items));
                    let _ = w.ui.send(UiEvent::TurnDone);
                }
                WorkerCmd::Rewind(n) => {
                    w.rewind(n);
                    let _ = w.ui.send(UiEvent::TurnDone);
                }
                WorkerCmd::ListMcp => {
                    if w.mcp.status().is_empty() {
                        let _ = w.ui.send(UiEvent::Notice(
                            "no MCP servers configured (add \"mcp_servers\" to config.json).".into(),
                        ));
                    } else {
                        for s in w.mcp.status() {
                            let tag = if s.ok { "ok" } else { "FAILED" };
                            let _ = w.ui.send(UiEvent::Notice(format!("mcp {} [{tag}]: {}", s.name, s.detail)));
                        }
                        for t in w.mcp.tools() {
                            let _ = w.ui.send(UiEvent::Notice(format!("  {}", t.full_name)));
                        }
                    }
                    let _ = w.ui.send(UiEvent::TurnDone);
                }
                WorkerCmd::Patch(p) => {
                    w.cfg.apply_patch(&p);
                    Config::persist_patch(&p);
                    // Rebuild the system prompt when auto_commit toggles, so the
                    // model's git-instructions stay in sync with the live config.
                    if matches!(&p, ConfigPatch::AutoCommit(_)) {
                        if !w.messages.is_empty() {
                            w.messages[0] = Message::system(system_prompt(w.cfg.auto_commit));
                        }
                    }
                    // Provider presets also switch the model; reflect it.
                    if let ConfigPatch::Provider { model, provider, .. } = &p {
                        let _ = w.ui.send(UiEvent::ModelChanged(model.clone()));
                        let _ = w.ui.send(UiEvent::Notice(format!("provider set to {provider}")));
                        w.refresh_balance();
                        w.refresh_context_window(); // new provider/model window
                    }
                }
                WorkerCmd::User { text, images } => {
                    w.cancel.store(false, Ordering::Relaxed);
                    // Supersede the previous turn's follow-up suggestion: stop its
                    // call early and mark it stale so a late result is dropped.
                    w.suggest_cancel.store(true, Ordering::Relaxed);
                    w.maybe_auto_compact();
                    // UserPromptSubmit hooks can veto the turn or add context.
                    let mut text = text;
                    let mut vetoed = false;
                    if w.hooks.has("UserPromptSubmit") {
                        match w.hooks.run("UserPromptSubmit", "", &serde_json::json!({ "prompt": text })) {
                            Outcome::Block(reason) => {
                                let _ = w.ui.send(UiEvent::Error(format!("prompt blocked by hook: {reason}")));
                                vetoed = true;
                            }
                            Outcome::Continue { context } if !context.trim().is_empty() => {
                                text.push_str("\n\n[hook context]\n");
                                text.push_str(context.trim());
                            }
                            _ => {}
                        }
                    }
                    if vetoed {
                        let _ = w.ui.send(UiEvent::TurnDone);
                        continue;
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        w.run_turn(text, images);
                        w.run_stop_hooks();
                    }));
                    if result.is_err() {
                        let _ = w.ui.send(UiEvent::Error("internal error in agent turn".into()));
                        // A panic mid tool-round leaves tool_calls without
                        // results; the API would reject every later request.
                        repair_orphans(&mut w.messages);
                    }
                    w.save_session();
                    w.refresh_balance(); // reflect spend after the turn
                    let _ = w.ui.send(UiEvent::TurnDone);
                    // Fire a cheap follow-up suggestion call in the background.
                    // Uses its own cancel flag so a mid-turn Esc in the next turn
                    // doesn't interrupt it.
                    if !w.quiet {
                        let suggest_ui = w.ui.clone();
                        let suggest_http = w.http.clone();
                        let suggest_cfg = w.cfg.clone();
                        // Only send the last few turns (system prefix + ~6 most recent
                        // messages), plus the suggestion prompt as a user message.
                        let mut suggest_msgs: Vec<Message> = w.messages[..w.system_len].to_vec();
                        // Pick a window start no earlier than ~6 messages back, then
                        // skip any leading orphaned `tool` results: an OpenAI-style
                        // `tool` message with no preceding `tool_calls` inside the
                        // window makes the API reject the whole request with a 400
                        // (silently dropped below), so no suggestion would ever show.
                        let n = w.messages.len();
                        let mut start = n.saturating_sub(6).max(w.system_len);
                        while start < n && w.messages[start].role == "tool" {
                            start += 1;
                        }
                        suggest_msgs.extend(w.messages[start..].iter().cloned());
                        suggest_msgs.push(Message::user(
                            "Based on the conversation above, suggest ONE short follow-up \
                             prompt the user might want to ask next. Reply with ONLY the \
                             prompt text, no other words, no quotes, no explanation.",
                        ));
                        // Fresh flag for this turn's suggestion; storing it on the
                        // worker lets the next User command cancel it mid-flight.
                        let cancel_flag = Arc::new(AtomicBool::new(false));
                        w.suggest_cancel = cancel_flag.clone();
                        std::thread::spawn(move || {
                            match api::chat_plain(&suggest_http, &suggest_cfg, &suggest_msgs, &cancel_flag) {
                                Ok((text, usage)) => {
                                    // Billed even when the result arrives too late to show.
                                    if let Some(u) = usage {
                                        let _ = suggest_ui.send(spend(&u));
                                    }
                                    // Drop a result that arrived after a new turn began.
                                    if cancel_flag.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    let text = text.trim().trim_matches('"').trim();
                                    if !text.is_empty() && text.len() < 200 {
                                        let _ = suggest_ui.send(UiEvent::Suggestion(text.to_string()));
                                    }
                                }
                                Err(_) => {} // silently skip on error
                            }
                        });
                    }
                }
            }
        }
    });

    Handles { join, cmd_tx, appr_tx, shared }
}

impl Worker {
    /// Fetch the account balance on a background thread (best-effort).
    fn refresh_balance(&self) {
        let ui = self.ui.clone();
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            let http = api::agent_http();
            if let Some(b) = api::fetch_balance(&http, &cfg) {
                let _ = ui.send(UiEvent::Balance(b));
            }
        });
    }

    /// Set the context window for the current model. The built-in table is
    /// instant; a background call then refines it from the provider's
    /// `/models` metadata for providers that advertise one (see `ctx_limit`).
    /// A user-pinned window is left untouched.
    fn refresh_context_window(&mut self) {
        if self.cfg.context_window_explicit {
            return;
        }
        let table = crate::config::known_context_window(&self.cfg.model);
        self.cfg.context_window = table;
        let _ = self.ui.send(UiEvent::ContextLimit(table));
        let ui = self.ui.clone();
        let cfg = self.cfg.clone();
        let detected = self.ctx_detected.clone();
        std::thread::spawn(move || {
            let http = api::agent_http();
            if let Some(n) = api::context_window(&http, &cfg, &cfg.model) {
                if let Ok(mut d) = detected.lock() {
                    *d = Some((cfg.model.clone(), n));
                }
                if n != table {
                    let _ = ui.send(UiEvent::ContextLimit(n));
                }
            }
        });
    }

    /// Run the OAuth subscription flow for `name` and store the token. Blocks
    /// the worker (a deliberate modal action) while the browser round-trips.
    fn login(&mut self, name: &str) {
        let Some(p) = crate::auth::provider(name) else {
            let _ = self.ui.send(UiEvent::Error(format!(
                "unknown subscription provider '{name}'. Try one of: {}",
                crate::auth::supported().join(", ")
            )));
            return;
        };
        let label = p.label.clone();
        let login = match crate::auth::start(p) {
            Ok(l) => l,
            Err(e) => {
                let _ = self.ui.send(UiEvent::Error(format!("login: {e}")));
                return;
            }
        };
        let _ = self.ui.send(UiEvent::Notice(format!(
            "Opening your browser to sign in to {label}.\nIf it didn't open, visit:\n{}",
            login.url
        )));
        match login.finish() {
            Ok(token) => {
                self.cfg.oauth.insert(name.to_string(), token.clone());
                Config::persist_oauth(name, &token);
                // Logging in is an explicit opt-in to subscription auth.
                self.cfg.auth_mode = "sub".into();
                Config::persist_patch(&ConfigPatch::AuthMode("sub".into()));
                let _ = self.ui.send(UiEvent::AuthMode("sub".into()));
                let exp = if token.expires_at > 0 {
                    format!(
                        " (access token valid ~{}m)",
                        token.expires_at.saturating_sub(crate::config::now_unix()) / 60
                    )
                } else {
                    String::new()
                };
                let _ = self.ui.send(UiEvent::Notice(format!(
                    "Signed in to {label}{exp}. Auth mode set to subscription. \
                     Select the {name} provider in /config to use it."
                )));
            }
            Err(e) => {
                let _ = self.ui.send(UiEvent::Error(format!("login failed: {e}")));
            }
        }
    }

    /// Refresh the active provider's subscription token if it's near expiry, so
    /// a resumed session stays authenticated. Best-effort and synchronous.
    fn ensure_oauth_fresh(&mut self) {
        let name = self.cfg.provider.clone();
        let Some(token) = self.cfg.oauth.get(&name).cloned() else { return };
        if !token.needs_refresh() {
            return;
        }
        let Some(p) = crate::auth::provider(&name) else { return };
        if let Ok(fresh) = crate::auth::refresh(&p, &token) {
            self.cfg.oauth.insert(name.clone(), fresh.clone());
            Config::persist_oauth(&name, &fresh);
        }
    }

    fn save_session(&self) {
        let Some(path) = &self.session else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Strip image payloads: a few photos would balloon the session file to
        // multi-MB rewritten every turn (painful on a Pi Zero). The text
        // around each image survives, so a resumed session stays coherent.
        let slim: Vec<Message> = self
            .messages
            .iter()
            .map(|m| {
                let mut m = m.clone();
                m.images.clear();
                m
            })
            .collect();
        if let Ok(json) = serde_json::to_string(&slim) {
            // Atomic: a power loss mid-rewrite must not corrupt the session that
            // `--continue` depends on — leave the previous turn's file intact.
            let _ = atomic_write(path, json.as_bytes());
        }
    }

    /// The context window auto-compaction sizes against: a user-pinned value,
    /// else what the provider advertised for this model, else the table.
    fn ctx_limit(&self) -> u32 {
        if !self.cfg.context_window_explicit {
            if let Ok(d) = self.ctx_detected.lock() {
                if let Some((model, n)) = d.as_ref() {
                    if *model == self.cfg.model {
                        return *n;
                    }
                }
            }
        }
        self.cfg.context_window
    }

    /// Auto-compact when the last prompt crossed 80% of the context window.
    /// Does not save the session itself — the caller (User handler) saves
    /// after the turn, avoiding a double-write when compaction runs right
    /// before a new turn.
    fn maybe_auto_compact(&mut self) {
        let limit = self.ctx_limit().max(1);
        if self.last_prompt as f64 >= 0.8 * limit as f64 {
            let pct = (self.last_prompt as f64 / limit as f64 * 100.0).round() as u32;
            let _ = self
                .ui
                .send(UiEvent::Notice(format!("context {pct}% full — compacting automatically…")));
            self.compact(None);
        }
    }

    /// Replace older turns with a model-written summary. Keeps the system
    /// prefix (prompt, memory, project context) and the most recent user
    /// exchange verbatim; everything in between is summarized with a plain
    /// (tool-less) completion. Returns true when messages were compacted (the
    /// caller should save the session) and false when there was nothing to do.
    fn compact(&mut self, focus: Option<&str>) -> bool {
        let n = self.messages.len();
        if n <= self.system_len + 2 {
            let _ = self.ui.send(UiEvent::Notice("nothing to compact yet.".into()));
            return false;
        }
        // Keep the latest exchange (from the last user message on) verbatim —
        // unless it IS the whole conversation, then summarize everything.
        let mut tail_start = self.messages[self.system_len..]
            .iter()
            .rposition(|m| m.role == "user")
            .map(|i| i + self.system_len)
            .filter(|&i| i > self.system_len)
            .unwrap_or(n);
        // When the latest exchange is itself what blew the window (one turn
        // with hundreds of tool calls), keeping it verbatim compacts nothing
        // and the next request still fails — summarize it too.
        let limit = self.ctx_limit().max(1) as usize;
        if tail_start < n && estimate_tokens(&self.messages[tail_start..]) as usize > limit / 2 {
            tail_start = n;
        }
        let rendered = render_for_summary(&self.messages[self.system_len..tail_start]);
        // The summary request must itself fit the model: ~4 chars per token,
        // leave room for the reply.
        let rendered = api::truncate(&rendered, (limit * 4).saturating_sub(8_000).max(20_000));
        if rendered.trim().is_empty() {
            let _ = self.ui.send(UiEvent::Notice("nothing to compact yet.".into()));
            return false;
        }
        let _ = self.ui.send(UiEvent::Notice("compacting context…".into()));
        let req = vec![
            Message::system(
                "You compress coding-agent conversations. Write a dense summary that lets the \
                 agent continue seamlessly. Preserve: the user's goals and constraints, decisions \
                 made, files/paths touched and how, key file contents or APIs discovered, command \
                 results that matter, and any unresolved problems or next steps. Use terse \
                 bullet points. No preamble.",
            ),
            Message::user(match focus {
                Some(f) if !f.trim().is_empty() => format!(
                    "Summarize this conversation so far. Pay special attention to, and preserve in \
                     detail: {}\n\n{rendered}",
                    f.trim()
                ),
                _ => format!("Summarize this conversation so far:\n\n{rendered}"),
            }),
        ];
        let res = api::chat_plain(&self.http, &self.cfg, &req, &self.cancel);
        if let Ok((_, Some(u))) = &res {
            let _ = self.ui.send(spend(u));
        }
        match res.map(|(text, _)| text) {
            Ok(summary) if !summary.trim().is_empty() => {
                let mut new = self.messages[..self.system_len].to_vec();
                new.push(Message::user(format!(
                    "[Earlier conversation was compacted. Summary:]\n{}",
                    summary.trim()
                )));
                new.push(Message {
                    role: "assistant".into(),
                    content: "Understood — continuing from that summary.".into(),
                    images: Vec::new(),
                    tool_calls: None,
                    tool_call_id: None,
                });
                // Prompts inside the summary can't be rewound to any more; the
                // kept tail moves up behind the summary.
                let kept_from = new.len();
                self.turns.retain(|t| t.at >= tail_start);
                for t in &mut self.turns {
                    t.at = t.at - tail_start + kept_from;
                }
                new.extend_from_slice(&self.messages[tail_start..]);
                self.messages = new;
                // Rough size estimate so the UI's ctx bar drops right away.
                let est = estimate_tokens(&self.messages);
                self.last_prompt = est;
                let _ = self.ui.send(UiEvent::Context(est));
                let _ = self.ui.send(UiEvent::Notice(format!(
                    "context compacted: {n} → {} messages.",
                    self.messages.len()
                )));
                true
            }
            Ok(_) => {
                let _ = self.ui.send(UiEvent::Error("compaction failed: empty summary.".into()));
                false
            }
            Err(e) => {
                let _ = self.ui.send(UiEvent::Error(format!("compaction failed: {e}")));
                false
            }
        }
    }

    fn run_turn(&mut self, text: String, images: Vec<String>) {
        // Drop images a previous interrupted turn queued but never consumed,
        // so they can't surface mid-way through an unrelated turn.
        self.pending_images.clear();
        self.recent_calls.clear();
        if !self.quiet {
            // The typed line only: attachments and hook context follow a blank line.
            let prompt = text.split("\n\n").next().unwrap_or_default().to_string();
            self.turns.push(TurnMark { at: self.messages.len(), head: self.checkpoint_head(), prompt });
        }
        self.messages.push(Message::user_with_images(text, images));
        self.run_loop();
    }

    /// HEAD of the working-directory repo when edits are checkpointed there.
    fn checkpoint_head(&self) -> Option<String> {
        let cwd = std::env::current_dir().ok()?;
        if !self.cfg.auto_commit || !tools::in_git_repo(&cwd) {
            return None;
        }
        tools::git_run(&cwd, &["rev-parse", "HEAD"], 10).ok().map(|s| s.trim().to_string())
    }

    /// Go back to just before turn `n`: undo picoder's checkpoints since then
    /// (files first: if git refuses, the conversation is left alone too), then
    /// drop that prompt and everything after it and hand it back to the composer.
    fn rewind(&mut self, n: usize) {
        let Some(mark) = self.turns.get(n).cloned() else {
            let _ = self.ui.send(UiEvent::Error("that prompt is no longer in the conversation.".into()));
            return;
        };
        let files = match &mark.head {
            Some(head) => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                match tools::revert_checkpoints_since(&cwd, head) {
                    Ok(0) => "no file changes to undo".to_string(),
                    Ok(k) => format!("reverted {k} checkpoint commit(s)"),
                    Err(e) => {
                        let _ = self.ui.send(UiEvent::Error(format!("rewind stopped, nothing changed: {e}")));
                        return;
                    }
                }
            }
            None => "files untouched (no git checkpoints for that prompt)".to_string(),
        };
        self.messages.truncate(mark.at);
        self.turns.truncate(n);
        let est = estimate_tokens(&self.messages);
        self.last_prompt = est;
        self.save_session();
        let _ = self.ui.send(UiEvent::Context(est));
        let _ = self.ui.send(UiEvent::Notice(format!(
            "rewound to before prompt {}: {files}; conversation trimmed to {} messages.",
            n + 1,
            self.messages.len()
        )));
        let _ = self.ui.send(UiEvent::Rewound(mark.prompt));
    }

    /// Stop hooks run after the model's final reply. Exit 2 means "not done":
    /// the hook's reason goes back to the model and the loop continues, at most
    /// a few times so a hook that can never be satisfied can't spin forever.
    fn run_stop_hooks(&mut self) {
        if !self.hooks.has("Stop") || self.quiet {
            return;
        }
        for _ in 0..3 {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            let last = self.messages.iter().rev().find(|m| m.role == "assistant").map(|m| m.content.clone());
            let payload = serde_json::json!({ "last_assistant_message": last.unwrap_or_default() });
            match self.hooks.run("Stop", "", &payload) {
                Outcome::Block(reason) => {
                    let _ = self.ui.send(UiEvent::Notice(format!("stop hook: {}", crate::api::truncate(&reason, 200))));
                    self.messages.push(Message::user(format!("[Stop hook feedback — address this before finishing]\n{reason}")));
                    self.run_loop();
                }
                Outcome::Continue { context } => {
                    if !context.trim().is_empty() {
                        let _ = self.ui.send(UiEvent::Notice(crate::api::truncate(context.trim(), 400)));
                    }
                    return;
                }
            }
        }
    }

    /// `!cmd`: run a shell command on the user's behalf. Output is shown like a
    /// tool result and recorded in the conversation so the model can build on it.
    fn run_shell(&mut self, cmd: &str) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let _ = self.ui.send(UiEvent::ToolStart { name: "bash".into(), summary: cmd.to_string() });
        let out = crate::tools::bash(cmd, 120, &cwd, &self.cancel);
        let ok = !out.starts_with("ERROR");
        let _ = self.ui.send(UiEvent::ToolResult { ok, preview: preview(&out, 40) });
        self.messages.push(Message::user(format!(
            "[The user ran a shell command directly]\n$ {cmd}\n{}",
            crate::api::truncate(&out, 20000)
        )));
    }

    /// The model/tool loop over `self.messages`. Returns the final assistant
    /// text (the reply with no tool calls). When `self.quiet` is set (inside a
    /// sub-agent) assistant tokens aren't streamed to the UI as the main reply,
    /// but tool activity still shows so the user can follow along and approve.
    fn run_loop(&mut self) -> String {
        let quiet = self.quiet;
        let max = if self.cfg.max_tool_calls == 0 {
            AUTO_STEPS
        } else {
            self.cfg.max_tool_calls as usize
        };
        // A failed or fruitless mid-turn compaction isn't retried every step.
        let mut compact_failed = false;
        for _ in 0..max {
            if self.cancel.load(Ordering::Relaxed) {
                let _ = self.ui.send(UiEvent::Notice("(interrupted)".into()));
                return String::new();
            }
            // A long turn can outgrow the window between user prompts, so check
            // before every request, not only when a new turn starts. Tool results
            // appended since the last response aren't in `last_prompt` yet, hence
            // the estimate. Sub-agents run on their own short-lived history.
            if !quiet && !compact_failed {
                let used = self.last_prompt.max(estimate_tokens(&self.messages));
                if used as f64 >= 0.8 * self.ctx_limit().max(1) as f64 {
                    let _ = self.ui.send(UiEvent::Notice(
                        "context nearly full mid-turn — compacting…".into(),
                    ));
                    // When the whole conversation was summarized it ends on the
                    // assistant's acknowledgement; hand the turn back explicitly.
                    if !self.compact(None) {
                        compact_failed = true;
                    } else {
                        if self.messages.last().map(|m| m.role.as_str()) == Some("assistant") {
                            self.messages.push(Message::user(
                                "[Context was compacted mid-task.] Continue the task from the summary.",
                            ));
                        }
                        // Still over the line: compacting again this turn would
                        // only burn calls (the model re-reads what was summarized).
                        if estimate_tokens(&self.messages) as f64 >= 0.8 * self.ctx_limit().max(1) as f64 {
                            compact_failed = true;
                        }
                    }
                }
            }
            let ui = self.ui.clone();
            let ui2 = self.ui.clone();
            let ui3 = self.ui.clone();
            let res = api::chat_resilient(
                &self.http,
                &self.cfg,
                &self.messages,
                &self.tools,
                &self.cancel,
                move |t| {
                    if !quiet {
                        let _ = ui.send(UiEvent::Token(t.to_string()));
                    }
                },
                move |t| {
                    if !quiet {
                        let _ = ui2.send(UiEvent::Reasoning(t.to_string()));
                    }
                },
                move |m| {
                    let _ = ui3.send(UiEvent::ResetLive);
                    let _ = ui3.send(UiEvent::Notice(m.to_string()));
                },
            );
            let (content, calls, usage) = match res {
                Ok(x) => x,
                Err(e) => {
                    let _ = self.ui.send(UiEvent::Error(e.to_string()));
                    return String::new();
                }
            };
            if let Some(u) = usage {
                self.last_prompt = u.prompt_tokens;
                let _ = self.ui.send(UiEvent::Usage {
                    prompt: u.prompt_tokens,
                    completion: u.total_tokens.saturating_sub(u.prompt_tokens),
                });
            }
            if !quiet {
                let _ = self.ui.send(UiEvent::AssistantCommit);
            }

            // Esc during streaming: chat_stream stops early and returns what
            // had accumulated so far. Half-streamed tool calls must not enter
            // history (their args may be truncated and they'll never get
            // results, which the API rejects on the next request) — keep only
            // the partial text and end the turn.
            if self.cancel.load(Ordering::Relaxed) {
                if !content.is_empty() {
                    self.messages.push(Message {
                        role: "assistant".into(),
                        content,
                        images: Vec::new(),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                let _ = self.ui.send(UiEvent::Notice("(interrupted)".into()));
                return String::new();
            }

            let mut msg = Message {
                role: "assistant".into(),
                content: content.clone(),
                images: Vec::new(),
                tool_calls: None,
                tool_call_id: None,
            };
            if !calls.is_empty() {
                msg.tool_calls = Some(
                    calls
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(|(i, c)| c.into_tool_call(i))
                        .collect(),
                );
            }
            self.messages.push(msg);

            if calls.is_empty() {
                return content;
            }
            // Once the assistant message above is in history, every one of its
            // tool_call_ids must get a tool result or the API rejects the next
            // request — so an Esc here answers the skipped calls instead of
            // returning with the history dangling.
            let mut interrupted = false;
            let mut i = 0;
            while i < calls.len() {
                if interrupted || self.cancel.load(Ordering::Relaxed) {
                    if !interrupted {
                        interrupted = true;
                        let _ = self.ui.send(UiEvent::Notice("(interrupted)".into()));
                    }
                    let c = &calls[i];
                    let id = if c.id.is_empty() { format!("call_{i}") } else { c.id.clone() };
                    self.messages
                        .push(Message::tool(id, "(interrupted by user; tool not run)".into()));
                    i += 1;
                    continue;
                }
                // Consecutive read-only calls go out together; the rest run one
                // at a time, in order (approvals, writes, sub-agents, MCP).
                let run = calls[i..].iter().take_while(|c| parallel_safe(&c.name)).count();
                if run >= 2 {
                    self.handle_parallel(&calls[i..i + run], i);
                    i += run;
                } else {
                    self.handle_call(calls[i].clone(), i);
                    i += 1;
                }
            }
            if interrupted {
                return String::new();
            }
            // view_image queues images to hand to the model on the next call;
            // add them as a user message after the tool results (which must
            // immediately follow their tool calls).
            if !self.pending_images.is_empty() {
                let imgs = std::mem::take(&mut self.pending_images);
                self.messages.push(Message::user_with_images(
                    "(attached image(s) from view_image)",
                    imgs,
                ));
            }
        }
        let _ = self.ui.send(UiEvent::Notice("(stopped: hit max steps)".into()));
        String::new()
    }

    /// Run a delegated task in an isolated sub-agent: a fresh conversation with
    /// filtered tools (no `task` for recursion, no `ask_user` — sub-agents must
    /// be autonomous). Only its final report returns to the parent — the
    /// intermediate steps never enter the parent's context. Wrapped in
    /// catch_unwind so a sub-agent panic cannot poison the parent worker.
    fn run_subagent(&mut self, task: &str) -> String {
        // Build a tool schema for the sub-agent: built-ins minus task/ask_user + MCP.
        let sub_tools = crate::api::tools_spec_subagent(self.mcp.tools());
        let sub_max = if self.cfg.max_tool_calls == 0 {
            AUTO_STEPS
        } else {
            self.cfg.max_tool_calls as usize
        };
        // Swap in a fresh context; restore the parent's afterward.
        let saved_msgs = std::mem::replace(
            &mut self.messages,
            vec![
                Message::system(subagent_prompt(sub_max)),
                Message::user(task.to_string()),
            ],
        );
        let saved_len = self.system_len;
        let saved_prompt = self.last_prompt;
        let saved_tools = std::mem::replace(&mut self.tools, sub_tools);
        let saved_calls = std::mem::take(&mut self.recent_calls);
        let saved_images = std::mem::take(&mut self.pending_images);
        self.system_len = 1;
        self.quiet = true;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.run_loop()
        }));

        // Always restore parent state, even if the sub-agent panicked.
        self.quiet = false;
        self.system_len = saved_len;
        self.last_prompt = saved_prompt;
        self.tools = saved_tools;
        self.messages = saved_msgs;
        self.recent_calls = saved_calls;
        self.pending_images = saved_images;
        let _ = self.ui.send(UiEvent::Context(saved_prompt));

        match result {
            Ok(report) => {
                if report.trim().is_empty() {
                    "(sub-agent returned no report)".to_string()
                } else {
                    report
                }
            }
            Err(_) => {
                let _ = self.ui.send(UiEvent::Error("sub-agent panicked".into()));
                "(sub-agent panicked)".to_string()
            }
        }
    }

    fn handle_call(&mut self, c: AccumCall, idx: usize) {
        let call = Call::new(&c, idx);
        let _ = self.ui.send(UiEvent::ToolStart { name: call.name.clone(), summary: call.summary.clone() });
        let result = match self.settle_early(&call) {
            Some(e) => {
                self.result_event(&e);
                e
            }
            // settle_early returns Some for every unparsable call.
            None => self.run_tool(&call.name, call.args.as_ref().unwrap_or(&Value::Null)),
        };
        self.messages.push(Message::tool(call.id, result));
    }

    /// Run consecutive read-only calls side by side. Everything stateful (the
    /// repeat guard, hooks, deny rules, transcript events, history) stays
    /// sequential and in call order; only the tools' own work overlaps. The
    /// transcript gets each call's start/result pair once the batch is done.
    fn handle_parallel(&mut self, batch: &[AccumCall], base: usize) {
        let _ = self.ui.send(UiEvent::Notice(format!(
            "running {} read-only calls in parallel…",
            batch.len()
        )));
        // A call settled up front (bad JSON, repeat, hook veto, deny rule) has
        // its result already; the rest are run below.
        let mut calls: Vec<(Call, Option<String>)> = Vec::with_capacity(batch.len());
        for (k, c) in batch.iter().enumerate() {
            let call = Call::new(c, base + k);
            let early = self.settle_early(&call).or_else(|| {
                let args = call.args.as_ref().unwrap_or(&Value::Null);
                self.pre_tool(&call.name, args).err()
            });
            calls.push((call, early));
        }
        let http = &self.http;
        let mut ran: Vec<Option<String>> = vec![None; calls.len()];
        // Bounded fan-out: a Pi Zero has one core and 512 MB.
        let todo: Vec<usize> = (0..calls.len()).filter(|&k| calls[k].1.is_none()).collect();
        for chunk in todo.chunks(8) {
            let out: Vec<(usize, String)> = std::thread::scope(|sc| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|&k| {
                        let (call, _) = &calls[k];
                        let args = call.args.as_ref().unwrap_or(&Value::Null);
                        (k, sc.spawn(move || run_readonly(http, &call.name, args)))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|(k, h)| (k, h.join().unwrap_or_else(|_| "ERROR: tool panicked".into())))
                    .collect()
            });
            for (k, r) in out {
                ran[k] = Some(r);
            }
        }
        for ((call, early), r) in calls.into_iter().zip(ran) {
            let _ = self.ui.send(UiEvent::ToolStart { name: call.name.clone(), summary: call.summary.clone() });
            let result = match (early, r) {
                (Some(e), _) => e,
                (None, Some(r)) => {
                    let args = call.args.as_ref().unwrap_or(&Value::Null);
                    self.post_tool(&call.name, args, r)
                }
                (None, None) => "ERROR: tool did not run".to_string(),
            };
            self.result_event(&result);
            self.messages.push(Message::tool(call.id, result));
        }
    }

    /// The result for a call that must not run: arguments that aren't JSON, or
    /// the thrashing guard — the same call with the same arguments three times
    /// in a row can't produce a different result, so break the loop instead of
    /// burning tokens (Codex-style). None means go ahead.
    fn settle_early(&mut self, call: &Call) -> Option<String> {
        let name = &call.name;
        let sig = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            name.hash(&mut h);
            call.raw.hash(&mut h);
            h.finish()
        };
        let n = self.recent_calls.len();
        let repeated = n >= 2 && self.recent_calls[n - 1] == sig && self.recent_calls[n - 2] == sig;
        self.recent_calls.push(sig);
        if self.recent_calls.len() > 32 {
            self.recent_calls.remove(0);
        }
        if repeated && name != "bash_output" && name != "ask_user" {
            return Some(format!(
                "ERROR: `{name}` was just called twice with these exact arguments and the result \
                 will not change. Do not repeat it. Try a different approach (other arguments, \
                 another tool, or inspect why it failed), or explain the blocker to the user."
            ));
        }
        if call.args.is_none() {
            return Some(format!(
                "ERROR: could not parse tool arguments as JSON. Re-issue with valid JSON. Received: {}",
                api::truncate(&call.raw, 300)
            ));
        }
        None
    }

    /// What a permission rule's pattern is matched against for this call.
    fn policy_targets(name: &str, args: &Value) -> Vec<String> {
        let s = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
        match name {
            "bash" => s("command").into_iter().collect(),
            "read_file" | "write_file" | "edit_file" | "list_files" => s("path").into_iter().collect(),
            "multi_edit" => args
                .get("edits")
                .and_then(|e| e.as_array())
                .map(|a| a.iter().filter_map(|e| e.get("path").and_then(|p| p.as_str()).map(str::to_string)).collect())
                .unwrap_or_default(),
            "grep" => s("path").into_iter().collect(),
            "glob" => s("pattern").into_iter().collect(),
            "web_fetch" => s("url").into_iter().collect(),
            "web_search" => s("query").into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn run_tool(&mut self, name: &str, args: &Value) -> String {
        let decision = match self.pre_tool(name, args) {
            Ok(d) => d,
            Err(r) => {
                self.result_event(&r);
                return r;
            }
        };
        let targets = Self::policy_targets(name, args);
        let result = self.run_tool_inner(name, args, &decision, targets.first().map(String::as_str).unwrap_or(""));
        self.post_tool(name, args, result)
    }

    /// PreToolUse hooks (which may veto) and the permission rules. Err is the
    /// refusal to return as the call's result; deny rules are absolute, so
    /// they hold in bypass mode too.
    fn pre_tool(&mut self, name: &str, args: &Value) -> Result<Decision, String> {
        if self.hooks.has("PreToolUse") {
            match self.hooks.run("PreToolUse", name, &serde_json::json!({ "tool_input": args })) {
                Outcome::Block(reason) => return Err(format!("DENIED by PreToolUse hook: {reason}")),
                Outcome::Continue { context } if !context.trim().is_empty() => {
                    let _ = self.ui.send(UiEvent::Notice(format!("hook: {}", crate::api::truncate(context.trim(), 300))));
                }
                _ => {}
            }
        }
        let targets = Self::policy_targets(name, args);
        let target_refs: Vec<&str> = targets.iter().map(String::as_str).collect();
        match self.policy.check(name, &target_refs) {
            Decision::Deny(rule) => Err(format!(
                "DENIED by permission rule `{rule}` (see /permissions). Do not retry this call."
            )),
            d => Ok(d),
        }
    }

    /// PostToolUse hooks can append feedback (e.g. a formatter's diagnostics).
    fn post_tool(&mut self, name: &str, args: &Value, result: String) -> String {
        if self.hooks.has("PostToolUse") {
            let payload = serde_json::json!({ "tool_input": args, "tool_response": crate::api::truncate(&result, 8000) });
            match self.hooks.run("PostToolUse", name, &payload) {
                Outcome::Block(reason) => {
                    return format!("{result}\n\n[PostToolUse hook feedback]\n{reason}");
                }
                Outcome::Continue { context } if !context.trim().is_empty() => {
                    return format!("{result}\n\n[PostToolUse hook]\n{}", context.trim());
                }
                _ => {}
            }
        }
        result
    }

    fn run_tool_inner(&mut self, name: &str, args: &Value, decision: &Decision, target: &str) -> String {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
        // An allow rule stands in for the user's "yes".
        let pre_approved = matches!(decision, Decision::Allow(_));
        // Plan mode: refuse mutating tools and ask the model to plan instead.
        if self.perm.load(Ordering::Relaxed) == PERM_PLAN
            && matches!(name, "bash" | "write_file" | "edit_file" | "multi_edit" | "bash_kill")
        {
            let r = "[plan mode] Not executed. picoder is in read-only plan mode — \
                     describe the change you'd make; the user will switch off plan mode to apply it."
                .to_string();
            self.result_event(&r);
            return r;
        }
        match name {
            n if parallel_safe(n) => {
                let r = run_readonly(&self.http, n, args);
                self.result_event(&r);
                r
            }
            "remember" => {
                let r = tools::remember(s("note"));
                self.result_event(&r);
                r
            }
            "view_image" => {
                let path = s("path");
                let r = match tools::image_data_uri(path) {
                    Ok(uri) => {
                        self.pending_images.push(uri);
                        format!("Loaded image {path} into context.")
                    }
                    Err(e) => e,
                };
                self.result_event(&r);
                r
            }
            "todo" => {
                let r = tools::todo(args.get("items").unwrap_or(&Value::Null));
                self.result_event(&r);
                r
            }
            "task" => {
                // No nested sub-agents: a sub-agent calling task would recurse.
                if self.quiet {
                    let r = "ERROR: a sub-agent cannot spawn another sub-agent.".to_string();
                    self.result_event(&r);
                    return r;
                }
                let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
                if prompt.trim().is_empty() {
                    let r = "ERROR: task needs a 'prompt' describing the work.".to_string();
                    self.result_event(&r);
                    return r;
                }
                let r = self.run_subagent(prompt);
                self.result_event(&r);
                r
            }
            "ask_user" => {
                let (tx, rx) = mpsc::channel();
                let _ = self.ui.send(UiEvent::Question { prompt: s("question").to_string(), reply: tx });
                let r = match self.wait_reply(&rx) {
                    Ok(Some(ans)) if !ans.trim().is_empty() => {
                        format!("User answered: {}", ans.trim())
                    }
                    _ => "(user declined to answer)".to_string(),
                };
                self.result_event(&r);
                r
            }
            "bash" => {
                let cmd = s("command");
                let background = args.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
                let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(120);
                let desc = if background { format!("run in background: {cmd}") } else { format!("run: {cmd}") };
                if !pre_approved && !self.approve(&desc, Some(crate::policy::suggest_rule("bash", cmd))) {
                    let r = "DENIED by user.".to_string();
                    self.result_event(&r);
                    return r;
                }
                let r = if background {
                    tools::bash_background(cmd, &cwd)
                } else {
                    tools::bash(cmd, timeout, &cwd, &self.cancel)
                };
                self.result_event(&r);
                r
            }
            "bash_output" => {
                let r = tools::bash_output(args.get("id").and_then(|v| v.as_u64()).unwrap_or(0));
                self.result_event(&r);
                r
            }
            "bash_kill" => {
                let r = tools::bash_kill(args.get("id").and_then(|v| v.as_u64()).unwrap_or(0));
                self.result_event(&r);
                r
            }
            "write_file" => {
                let path = s("path");
                let content = s("content");
                let (diff, existed) = tools::write_preview(path, content);
                let _ = self.ui.send(UiEvent::Diff(diff));
                let verb = if existed { "overwrite" } else { "create" };
                if !pre_approved
                    && !self.approve(
                        &format!("{verb} {path} ({} bytes)", content.len()),
                        Some(crate::policy::suggest_rule("write_file", path)),
                    )
                {
                    let r = "DENIED by user.".to_string();
                    self.result_event(&r);
                    return r;
                }
                let mut r = tools::write_file(path, content);
                if r.starts_with("OK") {
                    r.push_str(&self.autocommit(&[path], &format!("{verb} {path}")));
                }
                self.result_event(&r);
                r
            }
            "edit_file" => {
                let path = s("path");
                match tools::edit_preview(path, s("old_text"), s("new_text")) {
                    tools::EditPreview::Err(e) => {
                        self.result_event(&e);
                        e
                    }
                    tools::EditPreview::Ok { diff, new_content } => {
                        let _ = self.ui.send(UiEvent::Diff(diff));
                        if !pre_approved
                            && !self.approve(&format!("edit {path}"), Some(crate::policy::suggest_rule("edit_file", path)))
                        {
                            let r = "DENIED by user.".to_string();
                            self.result_event(&r);
                            return r;
                        }
                        let mut r = tools::apply_write(path, &new_content);
                        if r.starts_with("OK") {
                            r.push_str(&self.autocommit(&[path], &format!("edit {path}")));
                        }
                        self.result_event(&r);
                        r
                    }
                }
            }
            "multi_edit" => {
                let edits = parse_edits(args.get("edits"));
                if edits.is_empty() {
                    let r = "ERROR: multi_edit needs an 'edits' array of {path, old_text, new_text}.".to_string();
                    self.result_event(&r);
                    return r;
                }
                match tools::multi_edit_plan(&edits) {
                    Err(e) => {
                        self.result_event(&e);
                        e
                    }
                    Ok(plan) => {
                        let _ = self.ui.send(UiEvent::Diff(plan.diff));
                        let paths: Vec<String> = plan.files.iter().map(|(p, _)| p.clone()).collect();
                        if !pre_approved
                            && !self.approve(
                                &format!("apply {} edits across {} file(s)", edits.len(), paths.len()),
                                Some(crate::policy::suggest_rule("multi_edit", target)),
                            )
                        {
                            let r = "DENIED by user.".to_string();
                            self.result_event(&r);
                            return r;
                        }
                        let mut applied = Vec::new();
                        let mut errs = Vec::new();
                        for (p, content) in &plan.files {
                            let res = tools::apply_write(p, content);
                            if res.starts_with("OK") {
                                applied.push(p.clone());
                            } else {
                                errs.push(res);
                            }
                        }
                        let note = self.autocommit(
                            &applied.iter().map(String::as_str).collect::<Vec<_>>(),
                            &format!("multi_edit: {} file(s)", applied.len()),
                        );
                        let mut r = format!("OK applied edits to {} file(s){note}", applied.len());
                        if !errs.is_empty() {
                            r.push_str(&format!("\n{} failed:\n{}", errs.len(), errs.join("\n")));
                        }
                        self.result_event(&r);
                        r
                    }
                }
            }
            other if self.mcp.handles(other) => {
                // MCP tools can have side effects; gate them like bash unless
                // auto-approve is on. Plan mode can't tell read from write, so
                // it blocks them all.
                if self.perm.load(Ordering::Relaxed) == PERM_PLAN {
                    let r = "[plan mode] Not executed. picoder is in read-only plan mode — \
                             MCP tools may have side effects."
                        .to_string();
                    self.result_event(&r);
                    return r;
                }
                if !pre_approved && !self.approve(&format!("call MCP tool {other}"), Some(other.to_string())) {
                    let r = "DENIED by user.".to_string();
                    self.result_event(&r);
                    return r;
                }
                let cancel = self.cancel.clone();
                let r = self.mcp.call(other, args, &cancel);
                self.result_event(&r);
                r
            }
            other => {
                let e = format!("ERROR: unknown tool {other}");
                self.result_event(&e);
                e
            }
        }
    }

    /// Commit just-edited paths as a checkpoint (when auto_commit is on and we
    /// are in a repo). Returns a short note to append to the tool result.
    fn autocommit(&self, paths: &[&str], summary: &str) -> String {
        if !self.cfg.auto_commit {
            return String::new();
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let owned: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
        tools::git_autocommit(&cwd, &owned, &format!("picoder: {summary}"))
    }

    fn result_event(&self, result: &str) {
        let ok = !result.starts_with("ERROR") && !result.starts_with("DENIED");
        let _ = self.ui.send(UiEvent::ToolResult { ok, preview: preview(result, 12) });
    }

    /// Block on a UI reply channel, but give up when the turn is cancelled
    /// (Esc, or quitting) so the worker can never hang on a prompt nobody will
    /// answer — that used to wedge `join()` at exit.
    fn wait_reply<T>(&self, rx: &Receiver<T>) -> Result<T, ()> {
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(v) => return Ok(v),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.cancel.load(Ordering::Relaxed) {
                        return Err(());
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(()),
            }
        }
    }

    fn approve(&mut self, desc: &str, rule: Option<String>) -> bool {
        if self.perm.load(Ordering::Relaxed) == PERM_AUTO {
            return true;
        }
        let _ = self.ui.send(UiEvent::Approval { desc: desc.to_string(), rule });
        match self.wait_reply(&self.appr_rx) {
            Ok(ApprovalResponse::Yes) => true,
            Ok(ApprovalResponse::Always) => {
                self.perm.store(PERM_AUTO, Ordering::Relaxed);
                true
            }
            Ok(ApprovalResponse::Rule(rule)) => {
                let path = crate::policy::persist_target();
                match crate::policy::persist_rule(&path, "allow", &rule) {
                    Ok(()) => {
                        if let Some(r) = crate::policy::Rule::parse(&rule) {
                            if !self.policy.allow.contains(&r) {
                                self.policy.allow.push(r);
                            }
                        }
                        let _ = self.ui.send(UiEvent::Notice(format!(
                            "won't ask again for {rule} (saved to {})",
                            path.display()
                        )));
                    }
                    Err(e) => {
                        let _ = self.ui.send(UiEvent::Error(format!("could not save rule {rule}: {e}")));
                    }
                }
                true
            }
            _ => false,
        }
    }
}

/// Answer every `tool_call` in the last assistant message that has no `tool`
/// result following it, so the history is valid for the next request. Used
/// after a panic mid tool-round and when loading a session from disk.
pub fn repair_orphans(messages: &mut Vec<Message>) {
    let Some(ai) = messages.iter().rposition(|m| m.role == "assistant") else { return };
    let Some(calls) = messages[ai].tool_calls.clone() else { return };
    let answered: Vec<String> = messages[ai + 1..]
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    // Results must directly follow their call; insert after the last tool result.
    let mut at = ai + 1;
    while at < messages.len() && messages[at].role == "tool" {
        at += 1;
    }
    for c in calls {
        if !answered.contains(&c.id) {
            messages.insert(at, Message::tool(c.id.clone(), "(interrupted: tool did not run)".into()));
            at += 1;
        }
    }
}

/// Render messages as role-tagged plain text for the summarizer. Tool results
/// are clipped hard — the summary only needs their gist, and this keeps the
/// compaction request itself small.
fn render_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        match m.role.as_str() {
            "tool" => {
                out.push_str(&format!("[tool result] {}\n", api::truncate(&m.content, 500)));
            }
            role => {
                if !m.content.trim().is_empty() {
                    out.push_str(&format!("[{role}] {}\n", api::truncate(&m.content, 4000)));
                }
                // Preserve image context: note how many images were attached so the
                // summarizer knows visual context existed (even if it can't see them).
                if !m.images.is_empty() {
                    let count = m.images.len();
                    out.push_str(&format!(
                        "[{} image{} attached: {}]\n",
                        count,
                        if count > 1 { "s" } else { "" },
                        m.images.iter().map(|i| {
                            i.rsplit('/').next().unwrap_or(i).to_string()
                        }).collect::<Vec<_>>().join(", ")
                    ));
                }
                if let Some(calls) = &m.tool_calls {
                    for c in calls {
                        out.push_str(&format!(
                            "[tool call] {}({})\n",
                            c.function.name,
                            api::truncate(&c.function.arguments, 300)
                        ));
                    }
                }
            }
        }
    }
    // Belt and braces: the request must fit in the context window itself.
    api::truncate(&out, 300_000)
}

/// Crude size estimate (~4 chars/token, ~1000 tokens per image) for the
/// post-compaction context bar.
/// One tool call from the model, parsed once.
struct Call {
    id: String,
    name: String,
    /// The arguments as sent (`{}` when empty), for errors and the repeat guard.
    raw: String,
    /// None when `raw` isn't valid JSON.
    args: Option<Value>,
    /// The argument shown next to the tool name in the transcript.
    summary: String,
}

impl Call {
    fn new(c: &AccumCall, idx: usize) -> Call {
        let raw = if c.args.trim().is_empty() { "{}".to_string() } else { c.args.clone() };
        let args: Option<Value> = serde_json::from_str(&raw).ok();
        let summary = args
            .as_ref()
            .and_then(|a| {
                ["command", "path", "pattern", "note", "query", "url", "question", "description"]
                    .iter()
                    .find_map(|k| a.get(*k).and_then(|v| v.as_str()))
            })
            .unwrap_or("")
            .to_string();
        Call {
            id: if c.id.is_empty() { format!("call_{idx}") } else { c.id.clone() },
            name: c.name.clone(),
            raw,
            args,
            summary,
        }
    }
}

/// Read-only tools that need no approval, prompt or worker state, so several
/// from one response can run side by side.
fn parallel_safe(name: &str) -> bool {
    matches!(name, "read_file" | "list_files" | "grep" | "glob" | "web_fetch" | "web_search" | "recall")
}

/// The read-only tools' own work, free of the worker so a batch can run on
/// threads. Also the sequential path for these tools, so both behave alike.
fn run_readonly(http: &ureq::Agent, name: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "read_file" => tools::read_file(
            s("path"),
            args.get("start_line").and_then(|v| v.as_u64()),
            args.get("end_line").and_then(|v| v.as_u64()),
        ),
        "list_files" => tools::list_files(s("path")),
        "grep" => tools::grep(
            s("pattern"),
            s("path"),
            args.get("ignore_case").and_then(|v| v.as_bool()).unwrap_or(false),
        ),
        "glob" => tools::glob_search(s("pattern")),
        "web_fetch" => tools::web_fetch(http, s("url")),
        "web_search" => tools::web_search(http, s("query")),
        "recall" => tools::recall(args.get("query").and_then(|v| v.as_str())),
        _ => format!("ERROR: {name} is not a read-only tool"),
    }
}

/// A background call's token usage as a session-cost event.
fn spend(u: &api::Usage) -> UiEvent {
    UiEvent::Spend {
        prompt: u.prompt_tokens,
        completion: u.total_tokens.saturating_sub(u.prompt_tokens),
    }
}

fn estimate_tokens(messages: &[Message]) -> u32 {
    let chars: usize = messages
        .iter()
        .map(|m| {
            m.content.len()
                + m.images.len() * 4000
                + m.tool_calls
                    .as_ref()
                    .map(|cs| cs.iter().map(|c| c.function.arguments.len() + 20).sum())
                    .unwrap_or(0)
        })
        .sum();
    (chars / 4) as u32
}

/// Length of the leading run of system messages — the conversation prefix
/// that /reset and /compact must preserve.
fn system_prefix_len(messages: &[Message]) -> usize {
    messages.iter().take_while(|m| m.role == "system").count()
}

/// Parse the `edits` argument of multi_edit into typed edit requests.
fn parse_edits(v: Option<&Value>) -> Vec<tools::EditReq> {
    let Some(arr) = v.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let path = e.get("path")?.as_str()?.to_string();
            let old_text = e.get("old_text")?.as_str()?.to_string();
            let new_text = e.get("new_text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Some(tools::EditReq { path, old_text, new_text })
        })
        .collect()
}

/// System prompt for a delegated sub-agent. Its whole job is one task; its
/// final message becomes the report handed back to the parent.
fn subagent_prompt(max: usize) -> String {
    let host = crate::sysinfo::host_descriptor();
    format!(
        "You are a sub-agent of picoder, a terminal coding agent running ON {host}. You were \
delegated a single focused task by the main agent.

Rules:
- Use tools to inspect and change the real filesystem; never invent file contents.
- Work autonomously — your tools are a subset of the main agent's. You cannot \
ask the user questions or spawn further sub-agents.
- You have up to {max} tool-call rounds to complete the task.
- Stay strictly within the delegated task.
- When done, reply with a concise final report (findings, files changed, key results) \
and no tool call. That report is ALL the parent agent sees, so make it self-contained.",
        max = max
    )
}

fn preview(s: &str, maxlines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= maxlines {
        return s.trim_end().to_string();
    }
    let head = lines[..maxlines].join("\n");
    format!("{head}\n… (+{} more lines)", lines.len() - maxlines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_read_only_tools_run_in_parallel() {
        for t in ["read_file", "list_files", "grep", "glob", "web_fetch", "web_search", "recall"] {
            assert!(parallel_safe(t), "{t}");
        }
        // Anything that writes, prompts, recurses or touches worker state
        // must keep running one at a time, in order.
        for t in [
            "bash", "bash_output", "bash_kill", "write_file", "edit_file", "multi_edit",
            "remember", "view_image", "todo", "task", "ask_user", "mcp__fs__read",
        ] {
            assert!(!parallel_safe(t), "{t}");
        }
    }

    #[test]
    fn repair_orphans_answers_unanswered_tool_calls_only() {
        use crate::api::{FunctionCall, ToolCall};
        let call = |id: &str| ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall { name: "bash".into(), arguments: "{}".into() },
        };
        let mut msgs = vec![
            Message::system("s"),
            Message::user("u"),
            Message { role: "assistant".into(), content: String::new(), images: vec![], tool_calls: Some(vec![call("a"), call("b"), call("c")]), tool_call_id: None },
            Message::tool("a".into(), "done".into()),
        ];
        repair_orphans(&mut msgs);
        let ids: Vec<_> = msgs.iter().filter(|m| m.role == "tool").map(|m| m.tool_call_id.clone().unwrap()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(msgs[3].content, "done", "the real result is kept");
        // Idempotent, and a no-op on a healthy history.
        let before = msgs.len();
        repair_orphans(&mut msgs);
        assert_eq!(msgs.len(), before);
    }

    #[test]
    fn system_prefix_counts_leading_system_only() {
        // Fresh session: every startup message is system.
        let fresh = vec![Message::system("a"), Message::system("b")];
        assert_eq!(system_prefix_len(&fresh), 2);
        // Resumed session: the prefix ends at the first user message, even
        // though the whole history was passed in at spawn.
        let resumed = vec![
            Message::system("prompt"),
            Message::system("memory"),
            Message::user("hi"),
            Message::system("not a prefix message"),
        ];
        assert_eq!(system_prefix_len(&resumed), 2);
        assert_eq!(system_prefix_len(&[]), 0);
    }
}

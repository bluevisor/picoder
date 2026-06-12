//! The agent worker: a long-lived thread that owns the conversation and runs
//! the blocking model/tool loop, talking to the UI thread over channels.

use crate::api::{self, AccumCall, Message};
use crate::config::Config;
use crate::tools;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

const MAX_STEPS: usize = 50;

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
    Approval(String),
    /// sudo (via the askpass helper) needs a password. The UI pops a masked
    /// prompt and sends the result back over `reply` (None = user cancelled).
    PasswordRequest { prompt: String, reply: Sender<Option<String>> },
    ModelList(Vec<String>),
    ModelChanged(String),
    Usage { prompt: u32, completion: u32 },
    /// Re-estimate of the next prompt's size (after compaction) so the UI's
    /// context bar drops immediately, without touching session totals.
    Context(u32),
    Balance(String),
    Notice(String),
    Error(String),
    TurnDone,
}

/// UI → Worker control messages (processed between turns).
pub enum WorkerCmd {
    User(String),
    Reset,
    Compact,
    SetModel(String),
    ListModels,
    Quit,
}

#[derive(Clone, Copy)]
pub enum ApprovalResponse {
    Yes,
    No,
    Always,
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
    perm: Arc<AtomicU8>,
    ui: Sender<UiEvent>,
    appr_rx: Receiver<ApprovalResponse>,
    session: Option<PathBuf>,
    /// Prompt tokens of the last completion, for the auto-compaction trigger.
    last_prompt: u32,
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
        let system_len = messages.len();
        let mut w = Worker {
            http: api::agent_http(),
            cfg,
            messages,
            system_len,
            cancel,
            perm,
            ui,
            appr_rx,
            session,
            last_prompt: 0,
        };
        w.refresh_balance(); // initial account balance for the status line
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                WorkerCmd::Quit => break,
                WorkerCmd::Reset => {
                    w.messages.truncate(w.system_len);
                    w.last_prompt = 0;
                    w.save_session();
                    let _ = w.ui.send(UiEvent::Notice("context cleared.".into()));
                    let _ = w.ui.send(UiEvent::Context(0));
                }
                WorkerCmd::Compact => {
                    w.cancel.store(false, Ordering::Relaxed);
                    w.compact();
                    let _ = w.ui.send(UiEvent::TurnDone);
                }
                WorkerCmd::SetModel(m) => {
                    w.cfg.model = m.clone();
                    w.cfg.persist_model();
                    let _ = w.ui.send(UiEvent::ModelChanged(m.clone()));
                    let _ = w.ui.send(UiEvent::Notice(format!("model set to {m}")));
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
                WorkerCmd::User(text) => {
                    w.cancel.store(false, Ordering::Relaxed);
                    w.maybe_auto_compact();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        w.run_turn(text);
                    }));
                    if result.is_err() {
                        let _ = w.ui.send(UiEvent::Error("internal error in agent turn".into()));
                    }
                    w.save_session();
                    w.refresh_balance(); // reflect spend after the turn
                    let _ = w.ui.send(UiEvent::TurnDone);
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

    fn save_session(&self) {
        let Some(path) = &self.session else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string(&self.messages) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Auto-compact when the last prompt crossed 80% of the context window.
    fn maybe_auto_compact(&mut self) {
        let limit = self.cfg.context_window.max(1);
        if self.last_prompt as f64 >= 0.8 * limit as f64 {
            let pct = (self.last_prompt as f64 / limit as f64 * 100.0).round() as u32;
            let _ = self
                .ui
                .send(UiEvent::Notice(format!("context {pct}% full — compacting automatically…")));
            self.compact();
        }
    }

    /// Replace older turns with a model-written summary. Keeps the system
    /// prefix (prompt, memory, project context) and the most recent user
    /// exchange verbatim; everything in between is summarized with a plain
    /// (tool-less) completion.
    fn compact(&mut self) {
        let n = self.messages.len();
        if n <= self.system_len + 2 {
            let _ = self.ui.send(UiEvent::Notice("nothing to compact yet.".into()));
            return;
        }
        // Keep the latest exchange (from the last user message on) verbatim —
        // unless it IS the whole conversation, then summarize everything.
        let tail_start = self.messages[self.system_len..]
            .iter()
            .rposition(|m| m.role == "user")
            .map(|i| i + self.system_len)
            .filter(|&i| i > self.system_len)
            .unwrap_or(n);
        let rendered = render_for_summary(&self.messages[self.system_len..tail_start]);
        if rendered.trim().is_empty() {
            let _ = self.ui.send(UiEvent::Notice("nothing to compact yet.".into()));
            return;
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
            Message::user(format!("Summarize this conversation so far:\n\n{rendered}")),
        ];
        match api::chat_plain(&self.http, &self.cfg, &req, &self.cancel) {
            Ok(summary) if !summary.trim().is_empty() => {
                let mut new = self.messages[..self.system_len].to_vec();
                new.push(Message::user(format!(
                    "[Earlier conversation was compacted. Summary:]\n{}",
                    summary.trim()
                )));
                new.push(Message {
                    role: "assistant".into(),
                    content: "Understood — continuing from that summary.".into(),
                    tool_calls: None,
                    tool_call_id: None,
                });
                new.extend_from_slice(&self.messages[tail_start..]);
                self.messages = new;
                self.save_session();
                // Rough size estimate so the UI's ctx bar drops right away.
                let est = estimate_tokens(&self.messages);
                self.last_prompt = est;
                let _ = self.ui.send(UiEvent::Context(est));
                let _ = self.ui.send(UiEvent::Notice(format!(
                    "context compacted: {n} → {} messages.",
                    self.messages.len()
                )));
            }
            Ok(_) => {
                let _ = self.ui.send(UiEvent::Error("compaction failed: empty summary.".into()));
            }
            Err(e) => {
                let _ = self.ui.send(UiEvent::Error(format!("compaction failed: {e}")));
            }
        }
    }

    fn run_turn(&mut self, text: String) {
        self.messages.push(Message::user(text));
        for _ in 0..MAX_STEPS {
            if self.cancel.load(Ordering::Relaxed) {
                let _ = self.ui.send(UiEvent::Notice("(interrupted)".into()));
                return;
            }
            let ui = self.ui.clone();
            let ui2 = self.ui.clone();
            let ui3 = self.ui.clone();
            let res = api::chat_resilient(
                &self.http,
                &self.cfg,
                &self.messages,
                &self.cancel,
                move |t| {
                    let _ = ui.send(UiEvent::Token(t.to_string()));
                },
                move |t| {
                    let _ = ui2.send(UiEvent::Reasoning(t.to_string()));
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
                    return;
                }
            };
            if let Some(u) = usage {
                self.last_prompt = u.prompt_tokens;
                let _ = self.ui.send(UiEvent::Usage {
                    prompt: u.prompt_tokens,
                    completion: u.total_tokens.saturating_sub(u.prompt_tokens),
                });
            }
            let _ = self.ui.send(UiEvent::AssistantCommit);

            let mut msg = Message {
                role: "assistant".into(),
                content: content.clone(),
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
                return;
            }
            for (i, c) in calls.into_iter().enumerate() {
                if self.cancel.load(Ordering::Relaxed) {
                    let _ = self.ui.send(UiEvent::Notice("(interrupted)".into()));
                    return;
                }
                self.handle_call(c, i);
            }
        }
        let _ = self.ui.send(UiEvent::Notice("(stopped: hit max steps)".into()));
    }

    fn handle_call(&mut self, c: AccumCall, idx: usize) {
        let id = if c.id.is_empty() { format!("call_{idx}") } else { c.id.clone() };
        let name = c.name.clone();
        let raw = if c.args.trim().is_empty() { "{}" } else { &c.args };
        let parsed: Result<Value, _> = serde_json::from_str(raw);
        let summary = parsed
            .as_ref()
            .ok()
            .and_then(|a| {
                ["command", "path", "pattern", "note", "query", "url"]
                    .iter()
                    .find_map(|k| a.get(*k).and_then(|v| v.as_str()))
            })
            .unwrap_or("")
            .to_string();
        let _ = self.ui.send(UiEvent::ToolStart { name: name.clone(), summary });

        let result = match parsed {
            Err(_) => {
                let e = format!(
                    "ERROR: could not parse tool arguments as JSON. Re-issue with valid JSON. Received: {}",
                    api::truncate(raw, 300)
                );
                let _ = self.ui.send(UiEvent::ToolResult { ok: false, preview: e.clone() });
                e
            }
            Ok(args) => self.run_tool(&name, &args),
        };
        self.messages.push(Message::tool(id, result));
    }

    fn run_tool(&self, name: &str, args: &Value) -> String {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
        // Plan mode: refuse mutating tools and ask the model to plan instead.
        if self.perm.load(Ordering::Relaxed) == PERM_PLAN
            && matches!(name, "bash" | "write_file" | "edit_file")
        {
            let r = "[plan mode] Not executed. picode is in read-only plan mode — \
                     describe the change you'd make; the user will switch off plan mode to apply it."
                .to_string();
            self.result_event(&r);
            return r;
        }
        match name {
            "read_file" => {
                let r = tools::read_file(
                    s("path"),
                    args.get("start_line").and_then(|v| v.as_u64()),
                    args.get("end_line").and_then(|v| v.as_u64()),
                );
                self.result_event(&r);
                r
            }
            "list_files" => {
                let r = tools::list_files(s("path"));
                self.result_event(&r);
                r
            }
            "grep" => {
                let r = tools::grep(
                    s("pattern"),
                    s("path"),
                    args.get("ignore_case").and_then(|v| v.as_bool()).unwrap_or(false),
                );
                self.result_event(&r);
                r
            }
            "glob" => {
                let r = tools::glob_search(s("pattern"));
                self.result_event(&r);
                r
            }
            "remember" => {
                let r = tools::remember(s("note"));
                self.result_event(&r);
                r
            }
            "recall" => {
                let q = args.get("query").and_then(|v| v.as_str());
                let r = tools::recall(q);
                self.result_event(&r);
                r
            }
            "web_fetch" => {
                let r = tools::web_fetch(&self.http, s("url"));
                self.result_event(&r);
                r
            }
            "bash" => {
                let cmd = s("command");
                let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(120);
                if !self.approve(&format!("run: {cmd}")) {
                    let r = "DENIED by user.".to_string();
                    self.result_event(&r);
                    return r;
                }
                let r = tools::bash(cmd, timeout, &cwd);
                self.result_event(&r);
                r
            }
            "write_file" => {
                let path = s("path");
                let content = s("content");
                let (diff, existed) = tools::write_preview(path, content);
                let _ = self.ui.send(UiEvent::Diff(diff));
                let verb = if existed { "overwrite" } else { "create" };
                if !self.approve(&format!("{verb} {path} ({} bytes)", content.len())) {
                    let r = "DENIED by user.".to_string();
                    self.result_event(&r);
                    return r;
                }
                let r = tools::write_file(path, content);
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
                        if !self.approve(&format!("edit {path}")) {
                            let r = "DENIED by user.".to_string();
                            self.result_event(&r);
                            return r;
                        }
                        let r = tools::apply_write(path, &new_content);
                        self.result_event(&r);
                        r
                    }
                }
            }
            other => {
                let e = format!("ERROR: unknown tool {other}");
                self.result_event(&e);
                e
            }
        }
    }

    fn result_event(&self, result: &str) {
        let ok = !result.starts_with("ERROR") && !result.starts_with("DENIED");
        let _ = self.ui.send(UiEvent::ToolResult { ok, preview: preview(result, 12) });
    }

    fn approve(&self, desc: &str) -> bool {
        if self.perm.load(Ordering::Relaxed) == PERM_AUTO {
            return true;
        }
        let _ = self.ui.send(UiEvent::Approval(desc.to_string()));
        match self.appr_rx.recv() {
            Ok(ApprovalResponse::Yes) => true,
            Ok(ApprovalResponse::Always) => {
                self.perm.store(PERM_AUTO, Ordering::Relaxed);
                true
            }
            _ => false,
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

/// Crude size estimate (~4 chars/token) for the post-compaction context bar.
fn estimate_tokens(messages: &[Message]) -> u32 {
    let chars: usize = messages
        .iter()
        .map(|m| {
            m.content.len()
                + m.tool_calls
                    .as_ref()
                    .map(|cs| cs.iter().map(|c| c.function.arguments.len() + 20).sum())
                    .unwrap_or(0)
        })
        .sum();
    (chars / 4) as u32
}

fn preview(s: &str, maxlines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= maxlines {
        return s.trim_end().to_string();
    }
    let head = lines[..maxlines].join("\n");
    format!("{head}\n… (+{} more lines)", lines.len() - maxlines)
}

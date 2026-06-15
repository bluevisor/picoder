//! The agent worker: a long-lived thread that owns the conversation and runs
//! the blocking model/tool loop, talking to the UI thread over channels.

use crate::api::{self, AccumCall, Message};
use crate::config::{Config, ConfigPatch};
use crate::mcp::Mcp;
use crate::tools;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

const MAX_STEPS: usize = 100;

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
    /// The ask_user tool: the UI pops a visible input line and sends the
    /// answer back over `reply` (None = user declined).
    Question { prompt: String, reply: Sender<Option<String>> },
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
    User { text: String, images: Vec<String> },
    Reset,
    Compact,
    SetModel(String),
    /// A `/config` panel change: apply to the live config and persist.
    Patch(ConfigPatch),
    ListModels,
    ListMcp,
    /// `/new`: delete the session file and reset to a clean slate.
    New,
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
            quiet: false,
            pending_images: Vec::new(),
            mcp,
            tools,
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
                WorkerCmd::New => {
                    // Delete the session file so we start fresh.
                    if let Some(ref path) = w.session {
                        let _ = std::fs::remove_file(path);
                    }
                    w.messages.truncate(w.system_len);
                    w.last_prompt = 0;
                    let _ = w.ui.send(UiEvent::Notice("new session — fresh start.".into()));
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
                WorkerCmd::Patch(p) => {
                    w.cfg.apply_patch(&p);
                    Config::persist_patch(&p);
                    // Provider presets also switch the model; reflect it.
                    if let ConfigPatch::Provider { model, provider, .. } = &p {
                        let _ = w.ui.send(UiEvent::ModelChanged(model.clone()));
                        let _ = w.ui.send(UiEvent::Notice(format!("provider set to {provider}")));
                        w.refresh_balance();
                    }
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
                WorkerCmd::User { text, images } => {
                    w.cancel.store(false, Ordering::Relaxed);
                    w.maybe_auto_compact();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        w.run_turn(text, images);
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
                    images: Vec::new(),
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

    fn run_turn(&mut self, text: String, images: Vec<String>) {
        // Drop images a previous interrupted turn queued but never consumed,
        // so they can't surface mid-way through an unrelated turn.
        self.pending_images.clear();
        self.messages.push(Message::user_with_images(text, images));
        self.run_loop();
    }

    /// The model/tool loop over `self.messages`. Returns the final assistant
    /// text (the reply with no tool calls). When `self.quiet` is set (inside a
    /// sub-agent) assistant tokens aren't streamed to the UI as the main reply,
    /// but tool activity still shows so the user can follow along and approve.
    fn run_loop(&mut self) -> String {
        let quiet = self.quiet;
        for _ in 0..MAX_STEPS {
            if self.cancel.load(Ordering::Relaxed) {
                let _ = self.ui.send(UiEvent::Notice("(interrupted)".into()));
                return String::new();
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
            for (i, c) in calls.into_iter().enumerate() {
                if interrupted || self.cancel.load(Ordering::Relaxed) {
                    if !interrupted {
                        interrupted = true;
                        let _ = self.ui.send(UiEvent::Notice("(interrupted)".into()));
                    }
                    let id = if c.id.is_empty() { format!("call_{i}") } else { c.id.clone() };
                    self.messages
                        .push(Message::tool(id, "(interrupted by user; tool not run)".into()));
                    continue;
                }
                self.handle_call(c, i);
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
        // Swap in a fresh context; restore the parent's afterward.
        let saved_msgs = std::mem::replace(
            &mut self.messages,
            vec![
                Message::system(subagent_prompt()),
                Message::user(task.to_string()),
            ],
        );
        let saved_len = self.system_len;
        let saved_prompt = self.last_prompt;
        let saved_tools = std::mem::replace(&mut self.tools, sub_tools);
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
        let id = if c.id.is_empty() { format!("call_{idx}") } else { c.id.clone() };
        let name = c.name.clone();
        let raw = if c.args.trim().is_empty() { "{}" } else { &c.args };
        let parsed: Result<Value, _> = serde_json::from_str(raw);
        let summary = parsed
            .as_ref()
            .ok()
            .and_then(|a| {
                ["command", "path", "pattern", "note", "query", "url", "question", "description"]
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

    fn run_tool(&mut self, name: &str, args: &Value) -> String {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
        // Plan mode: refuse mutating tools and ask the model to plan instead.
        if self.perm.load(Ordering::Relaxed) == PERM_PLAN
            && matches!(name, "bash" | "write_file" | "edit_file" | "multi_edit" | "bash_kill")
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
            "web_search" => {
                let r = tools::web_search(&self.http, s("query"));
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
                let r = match rx.recv() {
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
                if !self.approve(&desc) {
                    let r = "DENIED by user.".to_string();
                    self.result_event(&r);
                    return r;
                }
                let r = if background {
                    tools::bash_background(cmd, &cwd)
                } else {
                    tools::bash(cmd, timeout, &cwd)
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
                if !self.approve(&format!("{verb} {path} ({} bytes)", content.len())) {
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
                        if !self.approve(&format!("edit {path}")) {
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
                        if !self.approve(&format!("apply {} edits across {} file(s)", edits.len(), paths.len())) {
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
                    let r = "[plan mode] Not executed. picode is in read-only plan mode — \
                             MCP tools may have side effects."
                        .to_string();
                    self.result_event(&r);
                    return r;
                }
                if !self.approve(&format!("call MCP tool {other}")) {
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
        tools::git_autocommit(&cwd, &owned, &format!("picode: {summary}"))
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

/// Crude size estimate (~4 chars/token, ~1000 tokens per image) for the
/// post-compaction context bar.
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
fn subagent_prompt() -> String {
    let host = crate::sysinfo::host_descriptor();
    format!(
        "You are a sub-agent of picode, a terminal coding agent running ON {host}. You were \
delegated a single focused task by the main agent.

Rules:
- Use tools to inspect and change the real filesystem; never invent file contents.
- Work autonomously — your tools are a subset of the main agent's. You cannot \
ask the user questions or spawn further sub-agents.
- You have up to {max} tool-call rounds to complete the task.
- Stay strictly within the delegated task.
- When done, reply with a concise final report (findings, files changed, key results) \
and no tool call. That report is ALL the parent agent sees, so make it self-contained.",
        max = MAX_STEPS
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

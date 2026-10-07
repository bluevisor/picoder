//! picoder — a tiny full-screen agentic coding CLI for the Raspberry Pi Zero W.
//!
//! Pure static binary (rustls TLS, no system deps). Talks to any
//! OpenAI-compatible chat API; defaults to DeepSeek. Codex/Claude-Code-style
//! agent loop with a ratatui terminal UI.

mod agent;
mod api;
mod askpass;
mod auth;
mod commands;
mod config;
mod diff;
mod hooks;
mod mcp;
mod money;
mod policy;
mod sysinfo;
mod tools;
mod ui;

use agent::{UiEvent, WorkerCmd};
use api::Message;
use config::Config;
use std::io::{IsTerminal, Read, Write};

/// System prompt, built at runtime so picoder honestly describes whatever host
/// it's on (Pi Zero W → Jetson Nano). A host is treated as build-capable when
/// it has ≥4 cores or ≥2GB RAM (so the quad-core Jetson Nano, which can compile
/// its own Rust, isn't told to avoid compiles); smaller hosts get a "keep it
/// light" caution instead.
pub fn system_prompt(auto_commit: bool) -> String {
    let host = sysinfo::host_descriptor();
    let capable = sysinfo::cpu_cores() >= 4 || sysinfo::mem_total_mb().unwrap_or(512) >= 2048;
    let resource_rule = if !capable {
        "- Use bash for git, builds, tests. Keep commands fast and memory-light — this machine is \
tiny. Avoid heavy installs/compiles unless asked."
    } else {
        "- Use bash for git, builds, tests. This machine is reasonably capable (multi-core, GBs of \
RAM + swap), so builds and compiles are fine when useful."
    };
    let git_rule = if auto_commit {
        "\n- Every successful edit is auto-committed to git as a checkpoint. Use git history as a \
clue: `git log`, `git show`, and `git diff` tell you what changed and let you restore a previous \
state with `git revert`/`git checkout` if an edit was wrong. Don't commit manually unless asked."
    } else {
        ""
    };
    format!(
        "You are picoder, a terminal coding agent running ON {host}. You help with \
software tasks using tools.

Rules:
- Use tools to inspect and change the real filesystem; never invent file contents.
- Prefer read_file/list_files/grep/glob before editing. Make minimal, correct edits with \
edit_file; use write_file for new files; use multi_edit to change several files at once.
{resource_rule}{git_rule}
- When the task is complete, reply with a short plain-text summary and no tool call.
- Be concise."
    )
}

const HELP: &str = "picoder — tiny agentic coding CLI (Rust, for the Pi Zero W)

usage:
  picoder                 interactive full-screen TUI
  picoder \"do a thing\"     one-shot task, then exit
  cmd | picoder \"task\"     one-shot with stdin attached as context (or as the task)
  picoder --continue      resume this directory's last session (alias: -c)
  picoder --auto ...      auto-approve tool calls (deny rules still apply); one-shot
                          runs otherwise follow config \"permission\" and decline prompts
  picoder --output FILE   one-shot: also write the final reply to FILE (alias: -o)
  picoder --json ...      one-shot: print one JSON object {result, usage, cost, …}
  picoder --stream-json   one-shot: print JSON-lines events as they happen
  picoder --config        set up provider + API key
  picoder model [id]      list models, or set the model
  picoder --banner        print the launch banner (debug) and exit
  picoder -h | --help     this help
  picoder --version

in the TUI: @path attaches a file · !cmd runs a shell command · Tab autocompletes

config: ~/.config/picoder/config.json  (key also via PICODER_API_KEY, or the
        configured provider's var, e.g. DEEPSEEK_API_KEY / OPENAI_API_KEY)
project: .picoder/settings.json {permissions: {allow, deny}, hooks, auto_bump_version}
         .picoder/commands/*.md  custom slash commands ($ARGUMENTS)
instructions: the first of PICODER.md / AGENTS.md / CLAUDE.md / GEMINI.md in ~/.config/picoder
              and in each directory from the repo root down to the working directory

exit status: 0 ok · 1 the run reported an error · 2 bad usage";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Internal: invoked by sudo as the SUDO_ASKPASS helper. Talks to the running
    // picoder over the given socket and prints the password; never starts the TUI.
    if let Some(pos) = args.iter().position(|a| a == "--askpass") {
        let sock = args.get(pos + 1).cloned().unwrap_or_default();
        let prompt = args.get(pos + 2).cloned().unwrap_or_else(|| "[sudo] password:".into());
        std::process::exit(askpass::run_helper(&sock, &prompt));
    }

    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("picoder {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--config" || a == "--setup") {
        if let Err(e) = config::run_setup() {
            eprintln!("setup failed: {e}");
        }
        return;
    }
    if let Some(pos) = args.iter().position(|a| a == "--banner") {
        let ascii = ui::detect_ascii();
        let cfg = Config::load();
        let status = status_lines(&cfg, ascii);
        // Optional theme name after --banner, else the configured theme.
        // Only consume the next arg if it looks like a theme name (prevents
        // --banner from swallowing e.g. "fix" as a theme name).
        let theme = match args.get(pos + 1) {
            Some(n) if ui::is_theme_name(n) => {
                let name = n.to_string();
                args.remove(pos + 1);
                name
            }
            _ => cfg.theme,
        };
        print!("{}", ui::banner_ansi(ui::term_width(), ascii, &theme, &status));
        return;
    }

    let auto = args.iter().any(|a| a == "--auto");
    let cont = args.iter().any(|a| a == "--continue" || a == "-c");
    let out_mode = if args.iter().any(|a| a == "--stream-json") {
        OutputMode::StreamJson
    } else if args.iter().any(|a| a == "--json" || a == "--output-format=json") {
        OutputMode::Json
    } else {
        OutputMode::Text
    };
    args.retain(|a| {
        !matches!(a.as_str(), "--auto" | "--continue" | "-c" | "--json" | "--stream-json" | "--output-format=json")
    });

    let mut output: Option<String> = None;
    if let Some(pos) = args.iter().position(|a| a == "--output" || a == "-o") {
        if pos + 1 >= args.len() {
            eprintln!("--output needs a file path");
            std::process::exit(2);
        }
        output = Some(args.remove(pos + 1));
        args.remove(pos);
    }

    let mut cfg = Config::load();

    if !args.is_empty() && args[0] == "model" {
        model_subcommand(&cfg, args.get(1).cloned());
        return;
    }

    if cfg.api_key.is_empty() {
        eprintln!("\x1b[33mNo API key configured.\x1b[0m");
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            match config::run_setup() {
                Ok(c) => cfg = c,
                Err(e) => {
                    eprintln!("setup failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        if cfg.api_key.is_empty() {
            eprintln!("Run: picoder --config");
            std::process::exit(1);
        }
    }

    // Piped stdin (`git diff | picoder "review this"`) rides along as context;
    // with no task argument it *is* the task. A terminal stdin is left alone.
    let piped = if std::io::stdin().is_terminal() {
        String::new()
    } else if !args.is_empty() && !stdin_is_pipe() && !stdin_ready(300) {
        // A non-tty stdin that nobody is writing to (a supervisor's idle pipe,
        // a socket) must not wedge a scripted `picoder "task"`: with a task
        // given, only read stdin when data or EOF is already there.
        String::new()
    } else {
        read_stdin_capped(1_000_000)
    };
    let mut task = args.join(" ");
    if !piped.trim().is_empty() {
        if task.trim().is_empty() {
            task = piped;
        } else {
            task = format!("{task}\n\n<stdin>\n{}\n</stdin>", piped.trim_end());
        }
    }

    if !task.trim().is_empty() {
        let (messages, _) = build_context(cont, &cfg);
        // Only persist a one-shot turn when explicitly continuing a session,
        // so a stray `picoder "x"` can't clobber a richer interactive session.
        let session = cont.then(config::session_path);
        let perm = start_perm(&cfg, auto);
        if !run_oneshot(cfg, messages, session, task, output, out_mode, perm) {
            std::process::exit(1);
        }
    } else {
        if output.is_some() {
            eprintln!("--output only applies to one-shot mode (picoder \"task\" -o file)");
            std::process::exit(2);
        }
        if out_mode != OutputMode::Text {
            eprintln!("--json / --stream-json only apply to one-shot mode (picoder \"task\" --json)");
            std::process::exit(2);
        }
        if !std::io::stdin().is_terminal() {
            eprintln!("no task given and stdin is not a terminal — pass a task: picoder \"do a thing\"");
            std::process::exit(2);
        }
        let (messages, notes) = build_context(cont, &cfg);
        run_tui(cfg, messages, notes, auto);
    }
}

/// Build the starting conversation: a resumed session if asked for and present,
/// otherwise a fresh one (system prompt + memory + auto-loaded project file).
/// Returns the messages and human-readable startup notes for the UI.
fn build_context(cont: bool, cfg: &Config) -> (Vec<Message>, Vec<String>) {
    let mut notes = Vec::new();
    if cont {
        if let Some(msgs) = load_session() {
            notes.push(format!("resumed session ({} messages)", msgs.len()));
            return (msgs, notes);
        }
        notes.push("no previous session here — starting fresh".into());
    }
    let mut messages = vec![Message::system(system_prompt(cfg.auto_commit))];
    if let Ok(Some(mem)) = tools::load_memory() {
        messages.push(Message::system(format!(
            "Persistent memory (things you were told to remember):\n{mem}"
        )));
    }
    if let Some((msg, names)) = load_project_context() {
        messages.push(msg);
        notes.push(format!("loaded {}", names.join(", ")));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if let Some(git) = tools::git_context(&cwd) {
        messages.push(Message::system(format!(
            "Current git state of the working directory (use as a clue to recent work):\n{git}"
        )));
        notes.push("loaded git history".into());
    }
    (messages, notes)
}

/// The three SYSTEM STATUS lines for the launch banner: hardware, network, and
/// the active LLM (model · provider · context window).
fn status_lines(cfg: &Config, ascii: bool) -> Vec<String> {
    let sep = if ascii { " - " } else { " · " };
    let ctx = if cfg.context_window >= 1_000_000 {
        let m = cfg.context_window as f64 / 1e6;
        // 1M, not 1.0M; 1.5M keeps the decimal.
        format!("{}M ctx", if m.fract() == 0.0 { format!("{m:.0}") } else { format!("{m:.1}") })
    } else if cfg.context_window >= 1000 {
        format!("{}K ctx", cfg.context_window / 1000)
    } else {
        format!("{} ctx", cfg.context_window)
    };
    let mut lines = vec![
        format!("HW   {}", sysinfo::hardware_line(ascii)),
        format!("NET  {}", sysinfo::network_line(ascii)),
        format!("LLM  {}{sep}{}{sep}{ctx}", cfg.model, cfg.provider),
    ];
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if let Some(git) = tools::git_status_line(&cwd, sep) {
        lines.push(format!("GIT  {git}"));
    }
    lines
}

/// Instruction files, in precedence order within one directory.
const CONTEXT_FILES: [&str; 4] = ["PICODER.md", "AGENTS.md", "CLAUDE.md", "GEMINI.md"];
/// Per-file and overall caps (chars) on loaded instructions; a Pi's model
/// can't spare much context for them.
const CONTEXT_FILE_CAP: usize = 12_000;
const CONTEXT_TOTAL_CAP: usize = 32_000;

/// Instruction files for this session as one system message, plus their
/// display names for the startup notes.
fn load_project_context() -> Option<(Message, Vec<String>)> {
    let cwd = std::env::current_dir().ok()?;
    let found = collect_context(&cwd, &config::config_dir());
    let names: Vec<String> = found.iter().map(|(n, _)| n.clone()).collect();
    let text = match found.as_slice() {
        [] => return None,
        [(name, body)] => format!("Project context from {name}:\n{body}"),
        _ => {
            let parts: Vec<String> = found.iter().map(|(n, b)| format!("--- {n} ---\n{b}")).collect();
            format!(
                "Project instructions, general to specific (later files take precedence):\n\n{}",
                parts.join("\n\n")
            )
        }
    };
    Some((Message::system(text), names))
}

/// (display name, contents) of the instruction files that apply in `cwd`,
/// general to specific: the global one in `global` (picoder's config dir),
/// then one per directory from the repo root down to `cwd` (just `cwd`
/// outside a repo), each the first of `CONTEXT_FILES` present there. The
/// overall cap is spent nearest-first, so a long global file can't crowd out
/// the project's own instructions.
fn collect_context(cwd: &std::path::Path, global: &std::path::Path) -> Vec<(String, String)> {
    let mut dirs: Vec<(std::path::PathBuf, String)> = vec![(cwd.to_path_buf(), String::new())];
    let below_repo_root =
        !cwd.join(".git").exists() && cwd.ancestors().skip(1).any(|d| d.join(".git").exists());
    if below_repo_root {
        for (up, d) in cwd.ancestors().enumerate().skip(1) {
            dirs.push((d.to_path_buf(), "../".repeat(up)));
            if d.join(".git").exists() {
                break;
            }
        }
    }
    if dirs.iter().all(|(d, _)| d != global) {
        dirs.push((global.to_path_buf(), "~/.config/picoder/".into()));
    }
    let mut found = Vec::new();
    let mut left = CONTEXT_TOTAL_CAP;
    for (dir, shown) in dirs {
        if left == 0 {
            break;
        }
        let Some((name, text)) = CONTEXT_FILES.iter().find_map(|n| {
            let text = read_capped(&dir.join(n))?;
            (!text.trim().is_empty()).then(|| (*n, text))
        }) else {
            continue;
        };
        let body = api::truncate(text.trim(), CONTEXT_FILE_CAP.min(left));
        left = left.saturating_sub(body.chars().count());
        found.push((format!("{shown}{name}"), body));
    }
    found.reverse();
    found
}

/// A file's text, read no further than 1 MB; None if it can't be read.
fn read_capped(path: &std::path::Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut s = String::new();
    std::io::BufReader::new(f.take(1_000_000)).read_to_string(&mut s).ok()?;
    Some(s)
}

fn load_session() -> Option<Vec<Message>> {
    let text = std::fs::read_to_string(config::session_path()).ok()?;
    let mut msgs: Vec<Message> = serde_json::from_str(&text).ok()?;
    if msgs.is_empty() {
        None
    } else {
        // A session saved mid tool-round (crash, kill) has tool_calls with no
        // results; answer them so the first request on resume isn't a 400.
        agent::repair_orphans(&mut msgs);
        Some(msgs)
    }
}

/// How one-shot mode reports: streamed text (default), one JSON object at the
/// end (`--json`), or one JSON object per event (`--stream-json`) — the shapes
/// scripts and CI expect from Claude Code / Codex.
#[derive(Clone, Copy, PartialEq)]
enum OutputMode {
    Text,
    Json,
    StreamJson,
}

/// True when stdin is a pipe (`cmd | picoder …`). A pipe's writer will close
/// it, so reading to EOF is safe even when the producer is slow — unlike a
/// supervisor's socket or an inherited tty-less device, which we only read
/// when data is already waiting.
fn stdin_is_pipe() -> bool {
    #[cfg(unix)]
    {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd 0 and a valid, zeroed stat buffer.
        if unsafe { libc::fstat(0, &mut st) } == 0 {
            return (st.st_mode & libc::S_IFMT) == libc::S_IFIFO;
        }
        return false;
    }
    #[allow(unreachable_code)]
    false
}

/// True when stdin has data (or EOF) to read within `timeout_ms`.
fn stdin_ready(timeout_ms: i32) -> bool {
    #[cfg(unix)]
    {
        let mut fds = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        // SAFETY: a valid pollfd array of length 1 and a finite timeout.
        let n = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
        return n > 0 && (fds.revents & (libc::POLLIN | libc::POLLHUP)) != 0;
    }
    #[allow(unreachable_code)]
    true
}

fn read_stdin_capped(cap: u64) -> String {
    let mut s = String::new();
    let _ = std::io::stdin().lock().take(cap).read_to_string(&mut s);
    s
}

/// The permission mode a session starts in: `--auto` forces bypass, else the
/// configured default.
fn start_perm(cfg: &Config, auto: bool) -> u8 {
    if auto {
        return agent::PERM_AUTO;
    }
    match cfg.permission.as_str() {
        "bypass" | "auto" => agent::PERM_AUTO,
        "plan" => agent::PERM_PLAN,
        _ => agent::PERM_ASK,
    }
}

/// Run one task to completion without the TUI. Returns false when the run
/// reported an error (or `--output` couldn't be written), for the exit status.
fn run_oneshot(
    cfg: Config,
    messages: Vec<Message>,
    session: Option<std::path::PathBuf>,
    task: String,
    output: Option<String>,
    mode: OutputMode,
    perm: u8,
) -> bool {
    let started = std::time::Instant::now();
    let model = cfg.model.clone();
    let (price_in, price_out, price_currency) = (cfg.price_in, cfg.price_out, cfg.price_currency.clone());
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let h = agent::spawn(cfg, messages, perm, session, ui_tx);
    let (task_text, attached) = ui::expand_attachments(&task);
    let (images, img_names) = ui::extract_images(&task);
    let mut all = attached;
    all.extend(img_names);
    let text_mode = mode == OutputMode::Text;
    let emit = |v: serde_json::Value| {
        if mode == OutputMode::StreamJson {
            println!("{v}");
        }
    };
    if !all.is_empty() {
        if text_mode {
            eprintln!("\x1b[2mattached: {}\x1b[0m", all.join(", "));
        }
        emit(serde_json::json!({"type": "attached", "files": all}));
    }
    let _ = h.cmd_tx.send(WorkerCmd::User { text: task_text, images });

    let mut out = std::io::stdout();
    let mut at_line_start = true;
    // Track the final assistant message (the last committed non-empty one)
    // so --output can write it to disk after the turn.
    let mut live = String::new();
    let mut final_text = String::new();
    let (mut prompt_tokens, mut completion_tokens, mut tool_calls, mut errors) = (0u64, 0u64, 0u32, Vec::new());
    while let Ok(ev) = ui_rx.recv() {
        match ev {
            UiEvent::Token(t) => {
                if text_mode {
                    print!("{t}");
                    let _ = out.flush();
                }
                emit(serde_json::json!({"type": "token", "text": t}));
                at_line_start = t.ends_with('\n');
                live.push_str(&t);
            }
            UiEvent::ResetLive => {
                // The worker is retrying the request: what streamed so far was
                // a failed attempt and will be re-sent, so it is not a result
                // candidate. Tell JSON consumers to discard it; in text mode a
                // newline at least separates the repeat from the fragment.
                live.clear();
                if text_mode && !at_line_start {
                    println!();
                    at_line_start = true;
                }
                emit(serde_json::json!({"type": "reset"}));
            }
            UiEvent::AssistantCommit => {
                // The reply (or the text before a tool round) finished — it is
                // the latest candidate for the final result.
                if !live.trim().is_empty() {
                    final_text = std::mem::take(&mut live);
                } else {
                    live.clear();
                }
            }
            UiEvent::ToolStart { name, summary } => {
                tool_calls += 1;
                if text_mode {
                    if !at_line_start {
                        println!();
                    }
                    println!("\x1b[34m⏺ {name}\x1b[0m {summary}");
                    at_line_start = true;
                }
                emit(serde_json::json!({"type": "tool_start", "name": name, "summary": summary}));
            }
            UiEvent::ToolResult { ok, preview } => {
                if text_mode {
                    let color = if ok { "\x1b[2m" } else { "\x1b[31m" };
                    for line in preview.lines() {
                        println!("  {color}{line}\x1b[0m");
                    }
                    at_line_start = true;
                }
                emit(serde_json::json!({"type": "tool_result", "ok": ok, "preview": preview}));
            }
            UiEvent::Notice(s) => {
                if text_mode {
                    if !at_line_start {
                        println!();
                    }
                    println!("\x1b[2m{s}\x1b[0m");
                    at_line_start = true;
                }
                emit(serde_json::json!({"type": "notice", "text": s}));
            }
            UiEvent::Error(s) => {
                if text_mode {
                    if !at_line_start {
                        println!();
                    }
                    at_line_start = true;
                }
                // Errors always go to stderr so a JSON consumer's stdout stays clean.
                eprintln!("\x1b[31m{s}\x1b[0m");
                emit(serde_json::json!({"type": "error", "text": s}));
                errors.push(s);
            }
            UiEvent::Usage { prompt, completion } | UiEvent::Spend { prompt, completion } => {
                prompt_tokens += prompt as u64;
                completion_tokens += completion as u64;
            }
            UiEvent::TurnDone => break,
            UiEvent::Reasoning(t) => {
                if text_mode {
                    // Thinking tokens: print dimmed, no newline.
                    print!("\x1b[2m{t}\x1b[0m");
                    let _ = out.flush();
                    at_line_start = false;
                }
                emit(serde_json::json!({"type": "reasoning", "text": t}));
            }
            UiEvent::Diff(d) => {
                if text_mode {
                    if !at_line_start {
                        println!();
                    }
                    println!("\x1b[2m{d}\x1b[0m");
                    at_line_start = true;
                }
                emit(serde_json::json!({"type": "diff", "text": d}));
            }
            UiEvent::Approval { desc, .. } => {
                // Nobody is at a terminal to ask. Bypass runs shouldn't get
                // here; anything else is declined (the model is told and goes
                // on), never silently approved.
                if perm == agent::PERM_AUTO {
                    let _ = h.appr_tx.send(agent::ApprovalResponse::Yes);
                    eprintln!("\x1b[2m[auto-approved: {desc}]\x1b[0m");
                } else {
                    let _ = h.appr_tx.send(agent::ApprovalResponse::No);
                    let note = format!("declined: {desc} (one-shot runs need --auto or \"permission\": \"bypass\")");
                    eprintln!("\x1b[33m[{note}]\x1b[0m");
                    emit(serde_json::json!({"type": "notice", "text": note}));
                }
            }
            UiEvent::Question { prompt, reply } => {
                // In auto mode, answer with an empty decline.
                eprintln!("\x1b[33m[question skipped in one-shot mode] {prompt}\x1b[0m");
                let _ = reply.send(None);
            }
            UiEvent::PasswordRequest { prompt, reply } => {
                eprintln!("\x1b[33m[sudo password requested but not available in one-shot mode] {prompt}\x1b[0m");
                let _ = reply.send(None);
            }
            _ => {}
        }
    }
    if text_mode && !at_line_start {
        println!();
    }
    let cost = prompt_tokens as f64 / 1e6 * price_in + completion_tokens as f64 / 1e6 * price_out;
    let result = final_text.trim_end().to_string();
    if mode != OutputMode::Text {
        let summary = serde_json::json!({
            "type": "result",
            "result": result,
            "model": model,
            "usage": { "prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens },
            "cost": cost,
            "cost_currency": price_currency,
            "tool_calls": tool_calls,
            "duration_ms": started.elapsed().as_millis() as u64,
            "errors": errors,
            "is_error": !errors.is_empty(),
        });
        println!("{summary}");
    }
    let mut ok = errors.is_empty();
    if let Some(path) = output {
        let mut text = result;
        text.push('\n');
        match std::fs::write(&path, text) {
            Ok(()) => eprintln!("\x1b[2mwrote {path}\x1b[0m"),
            Err(e) => {
                eprintln!("\x1b[31mcould not write {path}: {e}\x1b[0m");
                ok = false;
            }
        }
    }
    let _ = h.cmd_tx.send(WorkerCmd::Quit);
    let _ = h.join.join();
    ok
}

fn run_tui(cfg: Config, messages: Vec<Message>, notes: Vec<String>, auto: bool) {
    let console = ui::detect_ascii();
    let status = status_lines(&cfg, console);
    let history = load_history();
    // Capture UI-relevant config before the worker takes ownership of `cfg`.
    let ui_cfg_model = cfg.model.clone();
    let ui_cfg_theme = cfg.theme.clone();
    let ctx_limit = cfg.context_window;
    let price_in = cfg.price_in;
    let price_out = cfg.price_out;
    let price_currency = money::Currency::parse(&cfg.price_currency);
    let settings = cfg.clone();
    let perm_start = start_perm(&cfg, auto);

    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    // Wire up sudo askpass before the worker exists (it mutates process env).
    askpass::setup(ui_tx.clone());
    let h = agent::spawn(cfg, messages, perm_start, Some(config::session_path()), ui_tx);

    let mut terminal = match ui::setup_terminal() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to start terminal: {e}");
            return;
        }
    };
    let mut app = ui::App::new(
        ui::UiConfig {
            model: ui_cfg_model,
            theme: ui_cfg_theme,
            ascii: console,
            ctx_limit,
            price_in,
            price_out,
            price_currency,
            perm: h.shared.perm.clone(),
            settings,
        },
        history,
    );
    app.banner(ui::term_width(), status);
    app.welcome();
    for n in notes {
        app.note(n);
    }

    let result = ui::run(&mut terminal, &mut app, ui_rx, &h);
    ui::restore_terminal(console);
    if let Err(e) = result {
        eprintln!("ui error: {e}");
    }

    save_history(app.history());
    h.shared.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = h.cmd_tx.send(WorkerCmd::Quit);
    let _ = h.join.join();
}

fn model_subcommand(cfg: &Config, arg: Option<String>) {
    let http = api::agent_http();
    if let Some(id) = arg {
        let mut c = cfg.clone();
        c.model = id.clone();
        c.persist_model();
        println!("model set to {id}");
        return;
    }
    if cfg.api_key.is_empty() {
        eprintln!("No API key. Run: picoder --config");
        return;
    }
    match api::list_models(&http, cfg) {
        Ok(ids) if !ids.is_empty() => {
            println!("available models ({}):", cfg.provider);
            for (i, id) in ids.iter().enumerate() {
                let cur = if *id == cfg.model { "  <- current" } else { "" };
                println!("  {:>2}) {id}{cur}", i + 1);
            }
            print!("pick 1-{} or id (blank=keep {}): ", ids.len(), cfg.model);
            let _ = std::io::stdout().flush();
            let mut sel = String::new();
            let _ = std::io::stdin().read_line(&mut sel);
            let sel = sel.trim();
            if sel.is_empty() {
                return;
            }
            let chosen = match sel.parse::<usize>() {
                Ok(n) if n >= 1 && n <= ids.len() => ids[n - 1].clone(),
                _ => sel.to_string(),
            };
            let mut c = cfg.clone();
            c.model = chosen.clone();
            c.persist_model();
            println!("model set to {chosen}");
        }
        Ok(_) => println!("provider returned no models."),
        Err(e) => eprintln!("could not fetch models: {e}"),
    }
}

fn load_history() -> Vec<String> {
    std::fs::read_to_string(config::history_path())
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default()
}

fn save_history(history: &[String]) {
    let start = history.len().saturating_sub(1000);
    let text = history[start..].join("\n");
    let _ = std::fs::create_dir_all(config::config_dir());
    let _ = std::fs::write(config::history_path(), text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("picoder_ctx_{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn instruction_files_stack_from_global_to_the_working_directory() {
        let root = scratch("repo");
        let repo = root.join("repo");
        let deep = repo.join("sub/deep");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&deep).unwrap();
        fs::write(root.join("AGENTS.md"), "above the repo: never read").unwrap();
        fs::write(repo.join("AGENTS.md"), "repo root").unwrap();
        fs::write(repo.join("sub/CLAUDE.md"), "sub").unwrap();
        fs::write(deep.join("PICODER.md"), "deep").unwrap();
        fs::write(deep.join("AGENTS.md"), "shadowed by PICODER.md").unwrap();
        let global = root.join("global");
        fs::create_dir_all(&global).unwrap();
        fs::write(global.join("AGENTS.md"), "global").unwrap();

        let got = collect_context(&deep, &global);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["~/.config/picoder/AGENTS.md", "../../AGENTS.md", "../CLAUDE.md", "PICODER.md"]);
        let bodies: Vec<&str> = got.iter().map(|(_, b)| b.as_str()).collect();
        assert_eq!(bodies, ["global", "repo root", "sub", "deep"]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn outside_a_repo_only_the_working_directory_counts() {
        let root = scratch("plain");
        let cwd = root.join("work");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(root.join("AGENTS.md"), "parent, not a repo").unwrap();
        fs::write(cwd.join("GEMINI.md"), "here").unwrap();
        let got = collect_context(&cwd, &root.join("no-global"));
        assert_eq!(got, vec![("GEMINI.md".to_string(), "here".to_string())]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_size_budget_goes_to_the_nearest_files_first() {
        let root = scratch("budget");
        let cwd = root.join("work");
        let global = root.join("global");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&global).unwrap();
        fs::write(cwd.join("AGENTS.md"), "x".repeat(CONTEXT_FILE_CAP + 500)).unwrap();
        fs::write(global.join("AGENTS.md"), "g".repeat(CONTEXT_TOTAL_CAP)).unwrap();
        let got = collect_context(&cwd, &global);
        let total: usize = got.iter().map(|(_, b)| b.chars().count()).sum();
        assert!(total <= CONTEXT_TOTAL_CAP, "{total}");
        let (near, far) = (&got[1].1, &got[0].1);
        assert!(near.chars().count() <= CONTEXT_FILE_CAP && near.starts_with("xxx"));
        assert!(far.chars().count() < CONTEXT_TOTAL_CAP, "global got only what was left");
        fs::remove_dir_all(&root).ok();
    }
}

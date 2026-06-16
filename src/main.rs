//! picode — a tiny full-screen agentic coding CLI for the Raspberry Pi Zero W.
//!
//! Pure static binary (rustls TLS, no system deps). Talks to any
//! OpenAI-compatible chat API; defaults to DeepSeek. Codex/Claude-Code-style
//! agent loop with a ratatui terminal UI.

mod agent;
mod api;
mod askpass;
mod config;
mod diff;
mod mcp;
mod sysinfo;
mod tools;
mod ui;

use agent::{UiEvent, WorkerCmd};
use api::Message;
use config::Config;
use std::io::{IsTerminal, Read, Write};

/// System prompt, built at runtime so picode honestly describes whatever host
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
        "You are picode, a terminal coding agent running ON {host}. You help with \
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

const HELP: &str = "picode — tiny agentic coding CLI (Rust, for the Pi Zero W)

usage:
  picode                 interactive full-screen TUI
  picode \"do a thing\"     one-shot task, then exit
  picode --continue      resume this directory's last session (alias: -c)
  picode --auto ...      auto-approve tool calls
  picode --output FILE   one-shot: also write the final reply to FILE (alias: -o)
  picode --config        set up provider + API key
  picode model [id]      list models, or set the model
  picode --banner        print the launch banner (debug) and exit
  picode -h | --help     this help
  picode --version

in the TUI: @path attaches a file · Tab autocompletes commands/paths

config: ~/.config/picode/config.json  (key also via PICODE_API_KEY, or the
        configured provider's var, e.g. DEEPSEEK_API_KEY / OPENAI_API_KEY)
auto-loads PICODE.md / AGENTS.md / CLAUDE.md from the working directory";

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    // Internal: invoked by sudo as the SUDO_ASKPASS helper. Talks to the running
    // picode over the given socket and prints the password; never starts the TUI.
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
        println!("picode {}", env!("CARGO_PKG_VERSION"));
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
    args.retain(|a| a != "--auto" && a != "--continue" && a != "-c");

    let mut output: Option<String> = None;
    if let Some(pos) = args.iter().position(|a| a == "--output" || a == "-o") {
        if pos + 1 >= args.len() {
            eprintln!("--output needs a file path");
            return;
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
                    return;
                }
            }
        }
        if cfg.api_key.is_empty() {
            eprintln!("Run: picode --config");
            return;
        }
    }

    if !args.is_empty() {
        let (messages, _) = build_context(cont, &cfg);
        // Only persist a one-shot turn when explicitly continuing a session,
        // so a stray `picode "x"` can't clobber a richer interactive session.
        let session = cont.then(config::session_path);
        run_oneshot(cfg, messages, session, args.join(" "), output);
    } else {
        if output.is_some() {
            eprintln!("--output only applies to one-shot mode (picode \"task\" -o file)");
            return;
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
    if let Some((msg, name)) = load_project_context() {
        messages.push(msg);
        notes.push(format!("loaded {name}"));
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
    let ctx = if cfg.context_window >= 1000 {
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

fn load_project_context() -> Option<(Message, String)> {
    for name in ["PICODE.md", "AGENTS.md", "CLAUDE.md", "GEMINI.md"] {
        let content = match std::fs::File::open(name) {
            Ok(f) => {
                let mut r = std::io::BufReader::new(f.take(1_000_000)); // cap at 1 MB
                let mut s = String::new();
                match r.read_to_string(&mut s) {
                    Ok(_) => s,
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        };
        if !content.trim().is_empty() {
            let body = api::truncate(content.trim(), 12000);
            return Some((
                Message::system(format!("Project context from {name}:\n{body}")),
                name.to_string(),
            ));
        }
    }
    None
}

fn load_session() -> Option<Vec<Message>> {
    let text = std::fs::read_to_string(config::session_path()).ok()?;
    let msgs: Vec<Message> = serde_json::from_str(&text).ok()?;
    if msgs.is_empty() {
        None
    } else {
        Some(msgs)
    }
}

fn run_oneshot(
    cfg: Config,
    messages: Vec<Message>,
    session: Option<std::path::PathBuf>,
    task: String,
    output: Option<String>,
) {
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let h = agent::spawn(cfg, messages, agent::PERM_AUTO, session, ui_tx);
    let (task_text, attached) = ui::expand_attachments(&task);
    let (images, img_names) = ui::extract_images(&task);
    let mut all = attached;
    all.extend(img_names);
    if !all.is_empty() {
        eprintln!("\x1b[2mattached: {}\x1b[0m", all.join(", "));
    }
    let _ = h.cmd_tx.send(WorkerCmd::User { text: task_text, images });

    let mut out = std::io::stdout();
    let mut at_line_start = true;
    // Track the final assistant message (the last committed non-empty one)
    // so --output can write it to disk after the turn.
    let mut live = String::new();
    let mut final_text = String::new();
    while let Ok(ev) = ui_rx.recv() {
        match ev {
            UiEvent::Token(t) => {
                print!("{t}");
                at_line_start = t.ends_with('\n');
                live.push_str(&t);
                let _ = out.flush();
            }
            UiEvent::ResetLive => {
                // Tool calls are starting — capture any text accumulated so far
                // as a candidate for --output, because AssistantCommit will fire
                // after live is cleared and would miss this turn's text.
                if !live.trim().is_empty() {
                    final_text = std::mem::take(&mut live);
                } else {
                    live.clear();
                }
            }
            UiEvent::AssistantCommit => {
                if !live.trim().is_empty() {
                    final_text = std::mem::take(&mut live);
                } else {
                    live.clear();
                }
            }
            UiEvent::ToolStart { name, summary } => {
                if !at_line_start {
                    println!();
                }
                println!("\x1b[34m⏺ {name}\x1b[0m {summary}");
                at_line_start = true;
            }
            UiEvent::ToolResult { ok, preview } => {
                let color = if ok { "\x1b[2m" } else { "\x1b[31m" };
                for line in preview.lines() {
                    println!("  {color}{line}\x1b[0m");
                }
                at_line_start = true;
            }
            UiEvent::Notice(s) => {
                if !at_line_start {
                    println!();
                }
                println!("\x1b[2m{s}\x1b[0m");
                at_line_start = true;
            }
            UiEvent::Error(s) => {
                if !at_line_start {
                    println!();
                }
                eprintln!("\x1b[31m{s}\x1b[0m");
                at_line_start = true;
            }
            UiEvent::TurnDone => break,
            // Reasoning and diffs are shown in one-shot mode now.
            UiEvent::Reasoning(t) => {
                // Thinking tokens: print dimmed, no newline.
                print!("\x1b[2m{t}\x1b[0m");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                at_line_start = false;
            }
            UiEvent::Diff(d) => {
                if !at_line_start { println!(); }
                println!("\x1b[2m{d}\x1b[0m");
                at_line_start = true;
            }
            UiEvent::Approval(d) => {
                // In auto mode we shouldn't get here, but answer yes just in case.
                let _ = h.appr_tx.send(agent::ApprovalResponse::Yes);
                eprintln!("\x1b[2m[auto-approved: {d}]\x1b[0m");
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
    if !at_line_start {
        println!();
    }
    if let Some(path) = output {
        let mut text = final_text.trim_end().to_string();
        text.push('\n');
        match std::fs::write(&path, text) {
            Ok(()) => eprintln!("\x1b[2mwrote {path}\x1b[0m"),
            Err(e) => eprintln!("\x1b[31mcould not write {path}: {e}\x1b[0m"),
        }
    }
    let _ = h.cmd_tx.send(WorkerCmd::Quit);
    let _ = h.join.join();
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
    let settings = cfg.clone();
    let perm_start = if auto {
        agent::PERM_AUTO
    } else {
        match cfg.permission.as_str() {
            "bypass" | "auto" => agent::PERM_AUTO,
            "plan" => agent::PERM_PLAN,
            _ => agent::PERM_ASK,
        }
    };

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
        eprintln!("No API key. Run: picode --config");
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

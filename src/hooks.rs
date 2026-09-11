//! Lifecycle hooks — shell commands run at fixed points of the agent loop,
//! configured the way Claude Code does it so a `.claude/settings.json` hooks
//! block can be pasted into `.picoder/settings.json`:
//!
//! ```json
//! "hooks": {
//!   "PreToolUse":  [{ "matcher": "bash|edit_file", "hooks": [{ "type": "command", "command": "./scripts/guard.sh" }] }],
//!   "PostToolUse": [{ "matcher": "edit_file|write_file|multi_edit", "hooks": [{ "type": "command", "command": "cargo fmt" }] }],
//!   "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "date" }] }],
//!   "Stop": [{ "hooks": [{ "type": "command", "command": "cargo test -q" }] }]
//! }
//! ```
//!
//! The flatter `[{ "matcher": "bash", "command": "…" }]` form is accepted too.
//! Each hook gets a JSON payload on stdin (`hook_event_name`, `tool_name`,
//! `tool_input`, `tool_response`, `prompt`, `cwd`) and speaks through its exit
//! code: `0` continues (stdout is handed to the model as extra context), `2`
//! blocks the action and feeds stderr to the model, anything else is a warning
//! shown to the user. The matcher is a `|`-separated list of tool names or
//! wildcards (`mcp__*`); no matcher means every tool.

use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Hook {
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout: u64,
}

pub const EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "Stop",
    "SessionStart",
];

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Every hook passed; `context` is their combined stdout (may be empty).
    Continue { context: String },
    /// A hook exited 2: the action must not proceed; the reason goes to the model.
    Block(String),
}

#[derive(Clone, Debug, Default)]
pub struct Hooks {
    pub hooks: Vec<Hook>,
}

impl Hooks {
    pub fn load() -> Hooks {
        let mut h = Hooks::default();
        for (_, v) in crate::config::settings_layers() {
            h.absorb(&v);
        }
        h
    }

    pub fn absorb(&mut self, v: &Value) {
        let Some(obj) = v.get("hooks").and_then(|h| h.as_object()) else {
            return;
        };
        for (event, groups) in obj {
            if !EVENTS.contains(&event.as_str()) {
                continue;
            }
            let Some(groups) = groups.as_array() else {
                continue;
            };
            for g in groups {
                let matcher = g
                    .get("matcher")
                    .and_then(|m| m.as_str())
                    .map(str::trim)
                    .filter(|m| !m.is_empty() && *m != "*")
                    .map(String::from);
                let mut push = |h: &Value| {
                    if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                        if h.get("type")
                            .and_then(|t| t.as_str())
                            .map(|t| t != "command")
                            .unwrap_or(false)
                        {
                            return;
                        }
                        self.hooks.push(Hook {
                            event: event.clone(),
                            matcher: matcher.clone(),
                            command: cmd.to_string(),
                            timeout: h.get("timeout").and_then(|t| t.as_u64()).unwrap_or(60),
                        });
                    }
                };
                match g.get("hooks").and_then(|h| h.as_array()) {
                    Some(inner) => inner.iter().for_each(&mut push),
                    None => push(g),
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn has(&self, event: &str) -> bool {
        self.hooks.iter().any(|h| h.event == event)
    }

    pub fn describe(&self) -> Vec<String> {
        if self.hooks.is_empty() {
            return vec![
                "no hooks configured (add a \"hooks\" block to .picoder/settings.json).".into(),
            ];
        }
        self.hooks
            .iter()
            .map(|h| {
                format!(
                    "{:<17} {:<24} {}",
                    h.event,
                    h.matcher.as_deref().unwrap_or("*"),
                    h.command
                )
            })
            .collect()
    }

    fn matching<'a>(&'a self, event: &str, tool: &str) -> impl Iterator<Item = &'a Hook> + 'a {
        let event = event.to_string();
        let tool = tool.to_string();
        self.hooks.iter().filter(move |h| {
            h.event == event
                && match &h.matcher {
                    None => true,
                    // Claude Code spellings (`Bash`, `Edit`) are accepted like in rules.
                    Some(m) => m.split('|').map(str::trim).any(|p| {
                        let p = if p.contains(['*', '?']) { p.to_string() } else { crate::policy::canonical_tool(p) };
                        crate::policy::wild_match(&p, &tool)
                    }),
                }
        })
    }

    /// Run every hook registered for `event` (filtered by `tool` when the event
    /// is tool-scoped) with `payload` on stdin. Stops at the first blocking hook.
    pub fn run(&self, event: &str, tool: &str, payload: &Value) -> Outcome {
        let mut context = String::new();
        let mut payload = payload.clone();
        if let Some(o) = payload.as_object_mut() {
            o.insert("hook_event_name".into(), Value::String(event.into()));
            if !tool.is_empty() {
                o.insert("tool_name".into(), Value::String(tool.into()));
            }
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            o.insert("cwd".into(), Value::String(cwd));
        }
        let stdin = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
        for h in self.matching(event, tool) {
            match run_hook(h, &stdin) {
                HookExit::Ok(out) => {
                    let out = out.trim();
                    if !out.is_empty() {
                        if !context.is_empty() {
                            context.push('\n');
                        }
                        context.push_str(out);
                    }
                }
                HookExit::Block(reason) => {
                    let reason = reason.trim();
                    let reason = if reason.is_empty() {
                        format!("blocked by {event} hook `{}`", h.command)
                    } else {
                        reason.to_string()
                    };
                    return Outcome::Block(reason);
                }
                HookExit::Warn(msg) => {
                    // Non-blocking failure: surface to the model as context so
                    // it can mention it, but don't stop the action.
                    if !context.is_empty() {
                        context.push('\n');
                    }
                    context.push_str(&format!(
                        "[{event} hook `{}` failed: {}]",
                        h.command,
                        msg.trim()
                    ));
                }
            }
        }
        Outcome::Continue { context }
    }
}

enum HookExit {
    Ok(String),
    Block(String),
    Warn(String),
}

fn run_hook(h: &Hook, stdin_data: &str) -> HookExit {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&h.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return HookExit::Warn(format!("could not start: {e}")),
    };
    let pid = child.id();
    if let Some(mut si) = child.stdin.take() {
        let data = stdin_data.to_string();
        // Write on a helper thread: a hook that never reads stdin must not
        // deadlock us on a full pipe.
        std::thread::spawn(move || {
            let _ = si.write_all(data.as_bytes());
        });
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(Duration::from_secs(h.timeout.max(1))) {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            match out.status.code() {
                Some(0) => HookExit::Ok(crate::api::truncate(&stdout, 8000)),
                Some(2) => HookExit::Block(crate::api::truncate(&stderr, 4000)),
                code => HookExit::Warn(format!(
                    "exit {}: {}",
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into()),
                    crate::api::truncate(stderr.trim(), 1000)
                )),
            }
        }
        Ok(Err(e)) => HookExit::Warn(e.to_string()),
        Err(_) => {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{pid}"))
                .status();
            HookExit::Warn(format!("timed out after {}s", h.timeout))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks(json: &str) -> Hooks {
        let mut h = Hooks::default();
        h.absorb(&serde_json::from_str(json).unwrap());
        h
    }

    #[test]
    fn parses_nested_and_flat_shapes_and_ignores_unknown_events() {
        let h = hooks(
            r#"{"hooks":{
                "PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"echo a"}]}],
                "PostToolUse":[{"matcher":"edit_file|write_file","command":"echo b","timeout":5}],
                "Bogus":[{"command":"echo c"}],
                "Stop":[{"hooks":[{"type":"prompt","command":"nope"}]}]
            }}"#,
        );
        assert_eq!(h.hooks.len(), 2);
        let pre = h.hooks.iter().find(|x| x.event == "PreToolUse").unwrap();
        let post = h.hooks.iter().find(|x| x.event == "PostToolUse").unwrap();
        assert_eq!(pre.matcher.as_deref(), Some("bash"));
        assert_eq!((pre.timeout, post.timeout), (60, 5));
        assert!(!h.has("Stop"));
    }

    #[test]
    fn matcher_filters_by_tool_and_wildcards() {
        let h = hooks(
            r#"{"hooks":{"PreToolUse":[
                {"matcher":"bash","command":"echo bash-hook"},
                {"matcher":"mcp__*","command":"echo mcp-hook"},
                {"command":"echo any-hook"}
            ]}}"#,
        );
        let out = h.run("PreToolUse", "bash", &serde_json::json!({}));
        assert_eq!(
            out,
            Outcome::Continue {
                context: "bash-hook\nany-hook".into()
            }
        );
        let claude = hooks(r#"{"hooks":{"PreToolUse":[{"matcher":"Bash|Edit","command":"echo cc"}]}}"#);
        assert_eq!(
            claude.run("PreToolUse", "edit_file", &serde_json::json!({})),
            Outcome::Continue { context: "cc".into() }
        );
        let out = h.run("PreToolUse", "mcp__fs__read", &serde_json::json!({}));
        assert_eq!(
            out,
            Outcome::Continue {
                context: "mcp-hook\nany-hook".into()
            }
        );
        let out = h.run("PostToolUse", "bash", &serde_json::json!({}));
        assert_eq!(
            out,
            Outcome::Continue {
                context: String::new()
            }
        );
    }

    #[test]
    fn exit_two_blocks_with_stderr_and_stdin_carries_the_payload() {
        let h = hooks(
            r#"{"hooks":{"PreToolUse":[
                {"command":"cat >/dev/null; echo ok"},
                {"matcher":"bash","command":"grep -q '\"tool_name\":\"bash\"' && echo 'no rm here' >&2 && exit 2"}
            ]}}"#,
        );
        assert_eq!(
            h.run(
                "PreToolUse",
                "bash",
                &serde_json::json!({"tool_input": {"command": "rm -rf x"}})
            ),
            Outcome::Block("no rm here".into())
        );
    }

    #[test]
    fn other_exit_codes_warn_but_continue() {
        let h = hooks(r#"{"hooks":{"Stop":[{"command":"echo boom >&2; exit 1"}]}}"#);
        match h.run("Stop", "", &serde_json::json!({})) {
            Outcome::Continue { context } => {
                assert!(context.contains("failed: exit 1: boom"), "{context}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_hung_hook_times_out_instead_of_wedging_the_agent() {
        let h = hooks(r#"{"hooks":{"Stop":[{"command":"sleep 30","timeout":1}]}}"#);
        let t = std::time::Instant::now();
        match h.run("Stop", "", &serde_json::json!({})) {
            Outcome::Continue { context } => assert!(context.contains("timed out"), "{context}"),
            other => panic!("{other:?}"),
        }
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}

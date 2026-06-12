//! OpenAI-compatible chat API: streaming completions with tool calls, plus
//! model listing. Uses ureq (blocking) + rustls so it cross-compiles to ARMv6
//! as a single static binary with no system TLS dependency.

use crate::config::Config;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};

pub const MAX_TOOL_OUTPUT: usize = 16000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Message {
        Message { role: "system".into(), content: content.into(), tool_calls: None, tool_call_id: None }
    }
    pub fn user(content: impl Into<String>) -> Message {
        Message { role: "user".into(), content: content.into(), tool_calls: None, tool_call_id: None }
    }
    pub fn tool(id: String, content: String) -> Message {
        Message { role: "tool".into(), content, tool_calls: None, tool_call_id: Some(id) }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// A tool call accumulated from streamed deltas.
#[derive(Clone, Debug, Default)]
pub struct AccumCall {
    pub id: String,
    pub name: String,
    pub args: String,
}

impl AccumCall {
    pub fn args_ok(&self) -> bool {
        let a = if self.args.trim().is_empty() { "{}" } else { &self.args };
        serde_json::from_str::<serde_json::Value>(a).is_ok()
    }
    pub fn into_tool_call(self, fallback_idx: usize) -> ToolCall {
        let id = if self.id.is_empty() { format!("call_{fallback_idx}") } else { self.id };
        let args = if self.args.trim().is_empty() { "{}".into() } else { self.args };
        ToolCall { id, kind: "function".into(), function: FunctionCall { name: self.name, arguments: args } }
    }
}

pub fn agent_http() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(20))
        .user_agent("picode/0.1")
        .build()
}

/// The tool schema advertised to the model.
pub fn tools_spec() -> serde_json::Value {
    serde_json::json!([
        tool("bash", "Run a shell command in the working directory and return stdout, stderr and exit code. Use for git, builds, tests, searching, installing, etc.", serde_json::json!({
            "type":"object",
            "properties":{
                "command":{"type":"string","description":"Shell command to run."},
                "timeout":{"type":"integer","description":"Seconds (default 120)."}
            },
            "required":["command"]
        })),
        tool("read_file","Read a UTF-8 text file, optionally a 1-based inclusive line range.", serde_json::json!({
            "type":"object",
            "properties":{
                "path":{"type":"string"},
                "start_line":{"type":"integer"},
                "end_line":{"type":"integer"}
            },
            "required":["path"]
        })),
        tool("write_file","Create or overwrite a file with the given content. Creates parent dirs.", serde_json::json!({
            "type":"object",
            "properties":{"path":{"type":"string"},"content":{"type":"string"}},
            "required":["path","content"]
        })),
        tool("edit_file","Replace an exact substring in a file. old_text must appear exactly once. Use for surgical edits.", serde_json::json!({
            "type":"object",
            "properties":{"path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"}},
            "required":["path","old_text","new_text"]
        })),
        tool("list_files","List entries in a directory (non-recursive).", serde_json::json!({
            "type":"object",
            "properties":{"path":{"type":"string","description":"default '.'"}}
        })),
        tool("grep","Search file contents for a regex pattern under a path. Returns matching path:line: text. Skips .git/target/node_modules and binaries.", serde_json::json!({
            "type":"object",
            "properties":{
                "pattern":{"type":"string","description":"Rust regex."},
                "path":{"type":"string","description":"file or dir, default '.'"},
                "ignore_case":{"type":"boolean"}
            },
            "required":["pattern"]
        })),
        tool("glob","Find files matching a glob pattern (e.g. src/**/*.rs). Returns relative paths.", serde_json::json!({
            "type":"object",
            "properties":{"pattern":{"type":"string"}},
            "required":["pattern"]
        })),
        tool("remember","Persist a one-line note into long-term memory (~/.config/picode/memory.md), available across sessions.", serde_json::json!({
            "type":"object",
            "properties":{"note":{"type":"string"}},
            "required":["note"]
        })),
        tool("recall","Retrieve memories; with 'query' returns only matching lines, else all.", serde_json::json!({
            "type":"object",
            "properties":{"query":{"type":"string"}}
        })),
        tool("web_fetch","Fetch a URL over HTTP(S) and return its content as readable text (HTML is stripped to text). Use for documentation, APIs, and reference pages.", serde_json::json!({
            "type":"object",
            "properties":{"url":{"type":"string","description":"http:// or https:// URL"}},
            "required":["url"]
        })),
    ])
}

fn tool(name: &str, desc: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type":"function",
        "function":{"name":name,"description":desc,"parameters":params}
    })
}

/// Token usage reported by the API (the final stream chunk).
#[derive(Default, Clone, Copy, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}
#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}
#[derive(Default, Deserialize)]
struct Delta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<DeltaToolCall>>,
}
#[derive(Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: usize,
    id: Option<String>,
    function: Option<DeltaFn>,
}
#[derive(Deserialize)]
struct DeltaFn {
    name: Option<String>,
    arguments: Option<String>,
}

/// One streaming completion. Calls `on_content`/`on_reasoning` as deltas arrive.
/// Returns (assistant_text, tool_calls). `with_tools = false` omits the tool
/// schema (used for internal calls like context compaction).
fn chat_stream(
    http: &ureq::Agent,
    cfg: &Config,
    messages: &[Message],
    with_tools: bool,
    cancel: &AtomicBool,
    mut on_content: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
) -> Result<(String, Vec<AccumCall>, Option<Usage>)> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut body = serde_json::json!({
        "model": cfg.model,
        "messages": messages,
        "temperature": 0.2,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if with_tools {
        body["tools"] = tools_spec();
        body["tool_choice"] = serde_json::json!("auto");
    }
    let resp = http
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());

    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            return Err(anyhow!("HTTP {code}: {}", truncate(&detail, 800)));
        }
        Err(e) => return Err(anyhow!("network error: {e}")),
    };

    let mut content = String::new();
    let mut calls: Vec<AccumCall> = Vec::new();
    let mut usage: Option<Usage> = None;
    let reader = BufReader::new(resp.into_reader());
    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let line = match line {
            Ok(l) => l,
            Err(e) => return Err(anyhow!("stream read error: {e}")),
        };
        let line = line.trim();
        if line.is_empty() || !line.starts_with("data:") {
            continue;
        }
        let data = line[5..].trim();
        if data == "[DONE]" {
            break;
        }
        let chunk: StreamChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(u) = chunk.usage {
            usage = Some(u);
        }
        let Some(choice) = chunk.choices.into_iter().next() else { continue };
        let delta = choice.delta;
        if let Some(r) = delta.reasoning_content {
            if !r.is_empty() {
                on_reasoning(&r);
            }
        }
        if let Some(c) = delta.content {
            if !c.is_empty() {
                on_content(&c);
                content.push_str(&c);
            }
        }
        if let Some(tcs) = delta.tool_calls {
            for tc in tcs {
                if calls.len() <= tc.index {
                    calls.resize(tc.index + 1, AccumCall::default());
                }
                let slot = &mut calls[tc.index];
                if let Some(id) = tc.id {
                    if !id.is_empty() {
                        slot.id = id;
                    }
                }
                if let Some(f) = tc.function {
                    if let Some(n) = f.name {
                        if !n.is_empty() {
                            slot.name = n;
                        }
                    }
                    if let Some(a) = f.arguments {
                        slot.args.push_str(&a);
                    }
                }
            }
        }
    }
    calls.retain(|c| !c.name.is_empty());
    Ok((content, calls, usage))
}

/// chat_stream with retry: a dropped SSE chunk can corrupt streamed tool-call
/// JSON, so we re-request a clean response. Also retries transient net errors.
pub fn chat_resilient(
    http: &ureq::Agent,
    cfg: &Config,
    messages: &[Message],
    cancel: &AtomicBool,
    mut on_content: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
    mut on_retry: impl FnMut(&str),
) -> Result<(String, Vec<AccumCall>, Option<Usage>)> {
    let tries = 3;
    let mut last: Option<(String, Vec<AccumCall>, Option<Usage>)> = None;
    for attempt in 1..=tries {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match chat_stream(http, cfg, messages, true, cancel, &mut on_content, &mut on_reasoning) {
            Err(e) => {
                if attempt == tries {
                    return Err(e);
                }
                on_retry(&format!("network error — retry {attempt}/{}: {e}", tries - 1));
                continue;
            }
            Ok((content, calls, usage)) => {
                let ok = calls.iter().all(|c| c.args_ok());
                if calls.is_empty() || ok {
                    return Ok((content, calls, usage));
                }
                last = Some((content, calls, usage));
                if attempt < tries {
                    on_retry(&format!("malformed tool args — retry {attempt}/{}", tries - 1));
                }
            }
        }
    }
    last.ok_or_else(|| anyhow!("no response"))
}

/// A plain (no tools) completion, with one retry on transient errors. Used for
/// internal calls like summarizing the conversation during compaction.
pub fn chat_plain(
    http: &ureq::Agent,
    cfg: &Config,
    messages: &[Message],
    cancel: &AtomicBool,
) -> Result<String> {
    let mut last_err = anyhow!("no response");
    for _ in 0..2 {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("cancelled"));
        }
        match chat_stream(http, cfg, messages, false, cancel, |_| {}, |_| {}) {
            Ok((content, _, _)) => {
                if cancel.load(Ordering::Relaxed) {
                    return Err(anyhow!("cancelled"));
                }
                return Ok(content);
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// DeepSeek account balance (best-effort; returns None for other providers).
pub fn fetch_balance(http: &ureq::Agent, cfg: &Config) -> Option<String> {
    let url = format!("{}/user/balance", cfg.base_url.trim_end_matches('/'));
    let text = http
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let infos = v.get("balance_infos")?.as_array()?;
    let parse = |info: &serde_json::Value| {
        info.get("total_balance").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok())
    };
    // An account can have several currency buckets ($0 USD next to ¥77 CNY);
    // show the funded one.
    let info = infos
        .iter()
        .max_by(|a, b| {
            parse(a).unwrap_or(0.0).partial_cmp(&parse(b).unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal)
        })?;
    let bal = info.get("total_balance")?.as_str()?;
    let sym = match info.get("currency").and_then(|c| c.as_str()) {
        Some("CNY") => "¥",
        Some("USD") => "$",
        _ => "",
    };
    Some(format!("{sym}{bal}"))
}

/// Fetch the provider's available model ids.
pub fn list_models(http: &ureq::Agent, cfg: &Config) -> Result<Vec<String>> {
    let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .call();
    let text = match resp {
        Ok(r) => r.into_string()?,
        Err(ureq::Error::Status(code, r)) => {
            return Err(anyhow!("HTTP {code}: {}", truncate(&r.into_string().unwrap_or_default(), 400)));
        }
        Err(e) => return Err(anyhow!("network error: {e}")),
    };
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let items = v.get("data").cloned().unwrap_or(v);
    let mut ids: Vec<String> = items
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|x| x.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

pub fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let head: String = s.chars().take(limit.saturating_sub(40)).collect();
    let dropped = s.chars().count() - limit + 40;
    format!("{head}\n... [truncated {dropped} chars] ...")
}

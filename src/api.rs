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
    /// Attached images as `data:image/...;base64,...` URIs. Sent to the API as
    /// OpenAI content parts (see `messages_payload`). Stripped when sessions
    /// are saved — the payloads would balloon the session file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Message {
        Message { role: "system".into(), content: content.into(), images: Vec::new(), tool_calls: None, tool_call_id: None }
    }
    pub fn user(content: impl Into<String>) -> Message {
        Message { role: "user".into(), content: content.into(), images: Vec::new(), tool_calls: None, tool_call_id: None }
    }
    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Message {
        Message { role: "user".into(), content: content.into(), images, tool_calls: None, tool_call_id: None }
    }
    pub fn tool(id: String, content: String) -> Message {
        Message { role: "tool".into(), content, images: Vec::new(), tool_calls: None, tool_call_id: Some(id) }
    }
}

/// Serialize messages for the API request. When `supports_images` is true, a
/// message with images becomes the OpenAI multimodal form (`content` as an array
/// of text + image_url parts); otherwise images are silently dropped and the
/// message serializes as plain text (DeepSeek and Zhipu reject `image_url`).
fn messages_payload(messages: &[Message], supports_images: bool) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|m| {
            if !supports_images || m.images.is_empty() {
                return serde_json::to_value(m).unwrap_or(serde_json::Value::Null);
            }
            let mut parts: Vec<serde_json::Value> = Vec::new();
            if !m.content.is_empty() {
                parts.push(serde_json::json!({"type":"text","text":m.content}));
            }
            for uri in &m.images {
                parts.push(serde_json::json!({
                    "type":"image_url",
                    "image_url":{"url":uri}
                }));
            }
            serde_json::json!({"role": m.role, "content": parts})
        })
        .collect()
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
        .timeout_read(std::time::Duration::from_secs(60))
        .user_agent("picoder/0.1")
        .build()
}

/// The tool schema advertised to the model.
pub fn tools_spec() -> serde_json::Value {
    serde_json::json!([
        tool("bash", "Run a shell command in the working directory and return stdout, stderr and exit code. Use for git, builds, tests, searching, installing, etc. Set background=true for long-running commands (servers, builds): it returns a job id immediately; poll with bash_output.", serde_json::json!({
            "type":"object",
            "properties":{
                "command":{"type":"string","description":"Shell command to run."},
                "timeout":{"type":"integer","description":"Seconds (default 120). Ignored for background jobs."},
                "background":{"type":"boolean","description":"Run detached; returns a job id."}
            },
            "required":["command"]
        })),
        tool("bash_output","Read output a background bash job has produced since the last bash_output call, plus its run status.", serde_json::json!({
            "type":"object",
            "properties":{"id":{"type":"integer","description":"Job id from bash."}},
            "required":["id"]
        })),
        tool("bash_kill","Kill a background bash job (its whole process group).", serde_json::json!({
            "type":"object",
            "properties":{"id":{"type":"integer","description":"Job id from bash."}},
            "required":["id"]
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
        tool("multi_edit","Apply a batch of edits across one or more files in a single approval and commit. Edits apply in order (so several edits to the same file compose); each old_text must be unique in its file at the time it runs. All-or-nothing: if any edit can't be located, none are applied.", serde_json::json!({
            "type":"object",
            "properties":{"edits":{"type":"array","items":{
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "old_text":{"type":"string"},
                    "new_text":{"type":"string"}
                },
                "required":["path","old_text","new_text"]
            }}},
            "required":["edits"]
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
        tool("remember","Persist a one-line note into long-term memory (~/.config/picoder/memory.md), available across sessions.", serde_json::json!({
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
        tool("web_search","Search the web (DuckDuckGo) and return the top results as title, URL and snippet. Follow up with web_fetch to read a promising result.", serde_json::json!({
            "type":"object",
            "properties":{"query":{"type":"string"}},
            "required":["query"]
        })),
        tool("view_image","Load an image file (png/jpg/gif/webp) from disk into the conversation so you can see it. Use for screenshots, diagrams, and mockups.", serde_json::json!({
            "type":"object",
            "properties":{"path":{"type":"string"}},
            "required":["path"]
        })),
        tool("todo","Maintain a visible plan for multi-step tasks. Pass the FULL list every time (it replaces the previous plan; it is shown to the user). Keep items short; mark exactly one item in_progress while working on it.", serde_json::json!({
            "type":"object",
            "properties":{"items":{"type":"array","items":{
                "type":"object",
                "properties":{
                    "text":{"type":"string"},
                    "status":{"type":"string","enum":["pending","in_progress","completed"]}
                },
                "required":["text"]
            }}},
            "required":["items"]
        })),
        tool("ask_user","Ask the user ONE question and wait for their typed answer. Use only when blocked on a decision or missing information you cannot determine yourself.", serde_json::json!({
            "type":"object",
            "properties":{"question":{"type":"string"}},
            "required":["question"]
        })),
        tool("task","Delegate a self-contained sub-task to a fresh sub-agent that has its own context and the same tools. Use to parallelize exploration or keep a big sub-task's intermediate steps out of your context. Give complete instructions; you only get back the sub-agent's final report.", serde_json::json!({
            "type":"object",
            "properties":{
                "description":{"type":"string","description":"Short label for the sub-task (a few words)."},
                "prompt":{"type":"string","description":"Full instructions for the sub-agent, including everything it needs to know."}
            },
            "required":["prompt"]
        })),
    ])
}

fn tool(name: &str, desc: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type":"function",
        "function":{"name":name,"description":desc,"parameters":params}
    })
}

/// The built-in tool schema plus any MCP tools, as one array for the request.
pub fn tools_spec_with(mcp: &[crate::mcp::McpTool]) -> serde_json::Value {
    let mut spec = tools_spec();
    if let Some(arr) = spec.as_array_mut() {
        for t in mcp {
            arr.push(tool(&t.full_name, &t.description, t.schema.clone()));
        }
    }
    spec
}

/// Tool schema for a sub-agent: same as the parent's but without `task` (no
/// recursive sub-agents) and without `ask_user` (sub-agents must work
/// autonomously; the parent handles user interaction).
pub fn tools_spec_subagent(mcp: &[crate::mcp::McpTool]) -> serde_json::Value {
    let mut spec = tools_spec_with(mcp);
    if let Some(arr) = spec.as_array_mut() {
        arr.retain(|t| {
            let name = t.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str());
            !matches!(name, Some("task") | Some("ask_user"))
        });
    }
    spec
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
    tools: Option<&serde_json::Value>,
    cancel: &AtomicBool,
    mut on_content: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
) -> Result<(String, Vec<AccumCall>, Option<Usage>)> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut body = serde_json::json!({
        "model": cfg.model,
        "messages": messages_payload(messages, cfg.supports_images()),
        "temperature": 0.2,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(t) = tools {
        body["tools"] = t.clone();
        body["tool_choice"] = serde_json::json!("auto");
    }
    // DeepSeek-style thinking switch; other providers may not know the field
    // (the /config panel notes it), so it is only sent when enabled.
    if cfg.thinking {
        body["thinking"] = serde_json::json!({"type": "enabled"});
    }
    let resp = http
        .post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.bearer()))
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
#[allow(clippy::too_many_arguments)]
pub fn chat_resilient(
    http: &ureq::Agent,
    cfg: &Config,
    messages: &[Message],
    tools: &serde_json::Value,
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
        match chat_stream(http, cfg, messages, Some(tools), cancel, &mut on_content, &mut on_reasoning) {
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
        match chat_stream(http, cfg, messages, None, cancel, |_| {}, |_| {}) {
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
        .set("Authorization", &format!("Bearer {}", cfg.bearer()))
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

/// Best-effort context window (tokens) for `model` from the provider's
/// `/models` metadata. OpenAI-compatible endpoints usually omit it (DeepSeek
/// returns only id/object/owned_by), but OpenRouter and some others advertise
/// `context_length`. Returns None when the provider doesn't expose it, so the
/// caller can fall back to the built-in table.
pub fn context_window(http: &ureq::Agent, cfg: &Config, model: &str) -> Option<u32> {
    let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
    let text = http
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .set("Authorization", &format!("Bearer {}", cfg.bearer()))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let items = v.get("data").cloned().unwrap_or(v);
    let entry = items
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(|x| x.as_str()) == Some(model))?;
    // Field name varies across providers; accept the common spellings, plus
    // OpenRouter's nested `top_provider.context_length`.
    let n = ["context_length", "context_window", "max_context_length", "max_context_window"]
        .iter()
        .find_map(|k| entry.get(*k).and_then(|x| x.as_u64()))
        .or_else(|| entry.pointer("/top_provider/context_length").and_then(|x| x.as_u64()))?;
    (n > 0).then_some(n as u32)
}

/// Fetch the provider's available model ids.
pub fn list_models(http: &ureq::Agent, cfg: &Config) -> Result<Vec<String>> {
    let url = format!("{}/models", cfg.base_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .set("Authorization", &format!("Bearer {}", cfg.bearer()))
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

//! Native protocol adapters beside chat/completions: Anthropic Messages and
//! OpenAI Responses. picoder's history stays OpenAI-shaped (`api::Message`);
//! each adapter converts it to its wire form, streams the reply back into the
//! same (text, tool calls, usage) result, and hands back the turn in its
//! native form for `Message::native`, so what must round-trip across tool
//! turns does: Anthropic's signed thinking blocks, Responses' encrypted
//! reasoning items.

use crate::api::{for_each_sse, post_json, AccumCall, HttpStatus, LoopGuard, Message, Streamed, Usage};
use crate::config::Config;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

/// `max_tokens` for Messages, which requires one: inside every current
/// Claude model's output cap, and roomy enough for adaptive thinking.
const ANTHROPIC_MAX_TOKENS: u32 = 32_000;

/// Identifies this process. A stored native turn is replayed only within the
/// run that produced it: a resumed session has lost its images (they aren't
/// saved), which edits the history a signed thinking block was bound to.
fn run_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{:x}-{nanos:x}", std::process::id())
    })
}

/// Wrap a turn's native `parts` for `Message::native`.
fn native_turn(api: &str, cfg: &Config, parts: Vec<Value>) -> Value {
    json!({"api": api, "model": cfg.model, "run": run_id(), "parts": parts})
}

/// A stored native turn's parts, if it came from `api` + this model in this run.
fn replayable<'a>(m: &'a Message, api: &str, cfg: &Config) -> Option<&'a Vec<Value>> {
    let n = m.native.as_ref()?;
    (n["api"] == api && n["model"] == cfg.model.as_str() && n["run"] == run_id())
        .then(|| n["parts"].as_array())
        .flatten()
}

/// `data:<media type>;base64,<payload>` → (media type, payload).
fn split_data_uri(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("data:")?;
    let (mt, data) = rest.split_once(";base64,")?;
    Some((mt, data))
}

/// A tool call's arguments as JSON, `{}` when they don't parse.
fn args_json(args: &str) -> Value {
    serde_json::from_str(if args.trim().is_empty() { "{}" } else { args }).unwrap_or_else(|_| json!({}))
}

/// (name, description, JSON schema) of each tool in picoder's OpenAI-shaped spec.
fn tool_parts(spec: &Value) -> Vec<(Value, Value, Value)> {
    spec.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    let f = t.get("function")?;
                    let schema = f.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}));
                    Some((f.get("name")?.clone(), f.get("description").cloned().unwrap_or(json!("")), schema))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The error a mid-stream `error` event stands for, typed by kind so the retry
/// loop retries overload and rate limits but not a bad request.
fn stream_error(kind: &str, message: &str) -> anyhow::Error {
    let code = match kind {
        "overloaded_error" => 529,
        "rate_limit_error" | "rate_limit_exceeded" => 429,
        "api_error" | "server_error" => 500,
        _ => 400,
    };
    HttpStatus { code, retry_after: None, detail: message.to_string() }.into()
}

// ---------------------------------------------------------------- Messages

/// The Messages request body for `messages`: system prompts lifted into
/// `system`, tool results folded into user turns, the current run's
/// assistant turns replayed verbatim (signed thinking included), and cache
/// breakpoints on the system prompt (which caches the tools with it) and the
/// latest turn.
pub(crate) fn anthropic_body(cfg: &Config, messages: &[Message], tools: Option<&Value>) -> Value {
    let mut system = Vec::new();
    let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
    let mut push = |role: &str, mut blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        match turns.last_mut() {
            Some((r, b)) if r == role => b.append(&mut blocks),
            _ => turns.push((role.to_string(), blocks)),
        }
    };
    for m in messages {
        match m.role.as_str() {
            "system" => system.push(m.content.clone()),
            "assistant" => {
                if let Some(parts) = replayable(m, "messages", cfg) {
                    push("assistant", parts.clone());
                    continue;
                }
                let mut b = Vec::new();
                if !m.content.trim().is_empty() {
                    b.push(json!({"type": "text", "text": m.content}));
                }
                for c in m.tool_calls.iter().flatten() {
                    b.push(json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.function.name,
                        "input": args_json(&c.function.arguments),
                    }));
                }
                push("assistant", b);
            }
            "tool" => {
                let out = if m.content.is_empty() { "(no output)" } else { m.content.as_str() };
                push(
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                        "content": out,
                    })],
                );
            }
            _ => {
                let mut b: Vec<Value> = m
                    .images
                    .iter()
                    .filter_map(|u| split_data_uri(u))
                    .map(|(mt, data)| json!({"type": "image", "source": {"type": "base64", "media_type": mt, "data": data}}))
                    .collect();
                if !m.content.trim().is_empty() || b.is_empty() {
                    let text = if m.content.trim().is_empty() { "(empty message)" } else { m.content.as_str() };
                    b.push(json!({"type": "text", "text": text}));
                }
                push("user", b);
            }
        }
    }
    // The first turn must be the user's (a window that starts mid-conversation).
    if turns.first().is_some_and(|(r, _)| r == "assistant") {
        turns.insert(0, ("user".into(), vec![json!({"type": "text", "text": "(conversation continues)"})]));
    }
    // Breakpoint on the latest turn's last cacheable block.
    if let Some(block) = turns
        .last_mut()
        .and_then(|(_, b)| b.iter_mut().rev().find(|b| !matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking"))))
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
    let mut body = json!({
        "model": cfg.model,
        "max_tokens": ANTHROPIC_MAX_TOKENS,
        "stream": true,
        "messages": turns.into_iter().map(|(role, content)| json!({"role": role, "content": content})).collect::<Vec<_>>(),
    });
    if !system.is_empty() {
        body["system"] = json!([{"type": "text", "text": system.join("\n\n"), "cache_control": {"type": "ephemeral"}}]);
    }
    if let Some(spec) = tools {
        let tools: Vec<Value> = tool_parts(spec)
            .into_iter()
            .map(|(name, description, schema)| {
                // Big inputs (file bodies) stream as generated instead of in one
                // burst at the end; picoder validates the JSON itself.
                json!({"name": name, "description": description, "input_schema": schema, "eager_input_streaming": true})
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!({"type": "auto"});
        }
    }
    if let Some(t) = cfg.temperature() {
        body["temperature"] = json!(t);
    }
    match cfg.reasoning.as_str() {
        "auto" => {}
        // Current models can't switch thinking off (`disabled` is a 400 on
        // Opus 5.5 and Sonnet 5.5); the lowest effort is the off switch.
        "off" => body["output_config"] = json!({"effort": "low"}),
        level => {
            body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
            body["output_config"] = json!({"effort": level});
        }
    }
    body
}

/// Folds a Messages event stream into the reply, its tool calls, usage, and
/// the content blocks to replay next turn.
#[derive(Default)]
pub(crate) struct AnthropicAcc {
    blocks: Vec<Value>,
    /// Raw `input_json_delta` text per tool_use block, by block index.
    args: Vec<String>,
    content: String,
    usage: Usage,
    stop_reason: Option<String>,
    guard: LoopGuard,
}

impl AnthropicAcc {
    /// Apply one event. Ok(true) at `message_stop`.
    pub(crate) fn on_event(
        &mut self,
        ev: &Value,
        on_content: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str),
    ) -> Result<bool> {
        let idx = ev["index"].as_u64().unwrap_or(0) as usize;
        match ev["type"].as_str().unwrap_or("") {
            "message_start" => self.read_usage(&ev["message"]["usage"]),
            "content_block_start" => {
                if self.blocks.len() <= idx {
                    self.blocks.resize(idx + 1, Value::Null);
                    self.args.resize(idx + 1, String::new());
                }
                self.blocks[idx] = ev["content_block"].clone();
            }
            "content_block_delta" => {
                let Some(block) = self.blocks.get_mut(idx) else { return Ok(false) };
                let d = &ev["delta"];
                let append = |block: &mut Value, key: &str, piece: &str| {
                    let cur = block[key].as_str().unwrap_or("").to_string();
                    block[key] = json!(cur + piece);
                };
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        append(block, "text", t);
                        on_content(t);
                        self.content.push_str(t);
                        self.guard.feed(t)?;
                    }
                    "thinking_delta" => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        append(block, "thinking", t);
                        on_reasoning(t);
                        self.guard.feed(t)?;
                    }
                    "signature_delta" => append(block, "signature", d["signature"].as_str().unwrap_or("")),
                    "input_json_delta" => self.args[idx].push_str(d["partial_json"].as_str().unwrap_or("")),
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(block) = self.blocks.get_mut(idx) {
                    if block["type"] == "tool_use" {
                        if let Ok(input) = serde_json::from_str::<Value>(&self.args[idx]) {
                            block["input"] = input;
                        }
                    }
                }
            }
            "message_delta" => {
                if let Some(r) = ev["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(r.to_string());
                }
                self.read_usage(&ev["usage"]);
            }
            "message_stop" => return Ok(true),
            "error" => {
                let e = &ev["error"];
                return Err(stream_error(e["type"].as_str().unwrap_or(""), e["message"].as_str().unwrap_or("stream error")));
            }
            _ => {}
        }
        Ok(false)
    }

    /// Usage fields as they arrive (message_start has the input side, each
    /// message_delta a cumulative output count). Input counts exclude the
    /// cache, so the prompt total adds both cache fields back.
    fn read_usage(&mut self, u: &Value) {
        let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).map(|x| x as u32);
        let read = n("cache_read_input_tokens");
        let write = n("cache_creation_input_tokens");
        if let Some(input) = n("input_tokens") {
            self.usage.prompt_tokens = input + read.unwrap_or(0) + write.unwrap_or(0);
        }
        if let Some(r) = read {
            self.usage.cached_tokens = r;
        }
        if let Some(w) = write {
            self.usage.cache_write_tokens = w;
        }
        if let Some(out) = n("output_tokens") {
            self.usage.completion_tokens = out;
        }
    }

    pub(crate) fn finish(mut self, cfg: &Config, on_content: &mut dyn FnMut(&str)) -> Streamed {
        if self.stop_reason.as_deref() == Some("refusal") {
            let note = "\n[The model declined to continue this request (refusal).]";
            on_content(note);
            self.content.push_str(note);
        }
        let calls: Vec<AccumCall> = self
            .blocks
            .iter()
            .zip(&self.args)
            .filter(|(b, _)| b["type"] == "tool_use")
            .map(|(b, args)| AccumCall {
                id: b["id"].as_str().unwrap_or("").to_string(),
                name: b["name"].as_str().unwrap_or("").to_string(),
                args: args.clone(),
            })
            .collect();
        let parts: Vec<Value> = self.blocks.into_iter().filter(|b| !b.is_null()).collect();
        let native = (!parts.is_empty()).then(|| native_turn("messages", cfg, parts));
        (self.content, calls, Some(self.usage), native)
    }
}

pub(crate) fn anthropic_stream(
    http: &ureq::Agent,
    cfg: &Config,
    messages: &[Message],
    tools: Option<&Value>,
    cancel: &AtomicBool,
    mut on_content: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
) -> Result<Streamed> {
    let url = format!("{}/messages", cfg.base_url.trim_end_matches('/'));
    let resp = post_json(http, cfg, &url, &anthropic_body(cfg, messages, tools))?;
    let mut acc = AnthropicAcc::default();
    let mut done = false;
    for_each_sse(resp, cancel, |data| match serde_json::from_str::<Value>(data) {
        Ok(ev) => {
            done = acc.on_event(&ev, &mut on_content, &mut on_reasoning)?;
            Ok(done)
        }
        Err(_) => Ok(false),
    })?;
    if !done && !cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(anyhow!("network error: the response stream ended before it completed"));
    }
    Ok(acc.finish(cfg, &mut on_content))
}

// --------------------------------------------------------------- Responses

/// OpenAI models that reason (and so return encrypted reasoning items).
fn openai_reasoning_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let m = m.rsplit('/').next().unwrap_or(&m);
    ["o1", "o3", "o4", "gpt-5", "codex"].iter().any(|p| m.starts_with(p)) || m.contains("gpt-oss")
}

/// The Responses `reasoning.effort` for a picoder level. "off" is each
/// family's floor: `none` on gpt-5.1 and later, `minimal` on the original
/// gpt-5 models, `low` on the o-series.
fn responses_effort(model: &str, level: &str) -> &'static str {
    match level {
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        _ if model.starts_with('o') => "low",
        _ if model.starts_with("gpt-5") && !model.starts_with("gpt-5.") => "minimal",
        _ => "none",
    }
}

/// A stored output item made ready to send back as input: with `store`
/// false nothing is persisted server-side, so item ids other than a reasoning
/// item's (which carries its own encrypted content) would dangle.
fn as_input_item(item: &Value) -> Value {
    let mut it = item.clone();
    if let Some(o) = it.as_object_mut() {
        o.remove("status");
        if o.get("type").and_then(|t| t.as_str()) != Some("reasoning") {
            o.remove("id");
        }
    }
    it
}

/// The Responses request body: stateless (`store: false`), with encrypted
/// reasoning requested from reasoning models and replayed, and a prompt cache
/// key per conversation.
pub(crate) fn responses_body(cfg: &Config, messages: &[Message], tools: Option<&Value>) -> Value {
    let mut input: Vec<Value> = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "system" => input.push(json!({"role": "system", "content": m.content})),
            "assistant" => {
                if let Some(parts) = replayable(m, "responses", cfg) {
                    input.extend(parts.iter().map(as_input_item));
                    continue;
                }
                if !m.content.trim().is_empty() {
                    input.push(json!({"role": "assistant", "content": m.content}));
                }
                for c in m.tool_calls.iter().flatten() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": c.id,
                        "name": c.function.name,
                        "arguments": c.function.arguments,
                    }));
                }
            }
            "tool" => input.push(json!({
                "type": "function_call_output",
                "call_id": m.tool_call_id.clone().unwrap_or_default(),
                "output": m.content,
            })),
            _ if m.images.is_empty() => input.push(json!({"role": "user", "content": m.content})),
            _ => {
                let mut parts = vec![json!({"type": "input_text", "text": m.content})];
                parts.extend(m.images.iter().map(|u| json!({"type": "input_image", "image_url": u, "detail": "auto"})));
                input.push(json!({"role": "user", "content": parts}));
            }
        }
    }
    let mut body = json!({
        "model": cfg.model,
        "input": input,
        "stream": true,
        "store": false,
        "prompt_cache_key": crate::api::cache_key(messages),
    });
    if let Some(spec) = tools {
        let tools: Vec<Value> = tool_parts(spec)
            .into_iter()
            .map(|(name, description, schema)| {
                json!({"type": "function", "name": name, "description": description, "parameters": schema, "strict": false})
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }
    }
    if let Some(t) = cfg.temperature() {
        body["temperature"] = json!(t);
    }
    let reasoner = openai_reasoning_model(&cfg.model);
    let level = cfg.reasoning.as_str();
    if reasoner || level != "auto" {
        let mut r = json!({});
        if level != "auto" {
            r["effort"] = json!(responses_effort(&cfg.model, level));
        }
        if reasoner {
            r["summary"] = json!("auto");
            // Non-reasoning models reject this include with a 400.
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
        body["reasoning"] = r;
    }
    body
}

/// Folds a Responses event stream into the reply, its tool calls, usage, and
/// the output items to replay next turn.
#[derive(Default)]
pub(crate) struct ResponsesAcc {
    /// Function calls by output index.
    calls: Vec<(u64, AccumCall)>,
    /// Finished output items by output index (from `output_item.done`, the
    /// only place a reasoning item's encrypted content is complete).
    items: Vec<(u64, Value)>,
    content: String,
    usage: Option<Usage>,
    guard: LoopGuard,
}

impl ResponsesAcc {
    fn call_at(&mut self, idx: u64) -> &mut AccumCall {
        if let Some(i) = self.calls.iter().position(|(k, _)| *k == idx) {
            return &mut self.calls[i].1;
        }
        self.calls.push((idx, AccumCall::default()));
        &mut self.calls.last_mut().unwrap().1
    }

    /// Apply one event. Ok(true) once the response is complete.
    pub(crate) fn on_event(
        &mut self,
        ev: &Value,
        on_content: &mut dyn FnMut(&str),
        on_reasoning: &mut dyn FnMut(&str),
    ) -> Result<bool> {
        let idx = ev["output_index"].as_u64().unwrap_or(0);
        let delta = ev["delta"].as_str().unwrap_or("");
        match ev["type"].as_str().unwrap_or("") {
            "response.output_item.added" | "response.output_item.done" => {
                let item = &ev["item"];
                if item["type"] == "function_call" {
                    let c = self.call_at(idx);
                    c.id = item["call_id"].as_str().unwrap_or("").to_string();
                    c.name = item["name"].as_str().unwrap_or("").to_string();
                    // The finished item's arguments are authoritative.
                    if let Some(a) = item["arguments"].as_str().filter(|a| !a.is_empty()) {
                        c.args = a.to_string();
                    }
                }
                if ev["type"] == "response.output_item.done" {
                    self.items.push((idx, item.clone()));
                }
            }
            "response.function_call_arguments.delta" => self.call_at(idx).args.push_str(delta),
            "response.output_text.delta" => {
                on_content(delta);
                self.content.push_str(delta);
                self.guard.feed(delta)?;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                on_reasoning(delta);
                self.guard.feed(delta)?;
            }
            "response.reasoning_summary_part.added" if ev["summary_index"].as_u64().unwrap_or(0) > 0 => {
                on_reasoning("\n\n");
            }
            "response.completed" | "response.incomplete" => {
                let u = &ev["response"]["usage"];
                let n = |p: &str| u.pointer(p).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                self.usage = Some(Usage {
                    prompt_tokens: n("/input_tokens"),
                    completion_tokens: n("/output_tokens"),
                    cached_tokens: n("/input_tokens_details/cached_tokens"),
                    // OpenAI doesn't charge extra to write the cache.
                    cache_write_tokens: 0,
                });
                return Ok(true);
            }
            "response.failed" => {
                let e = &ev["response"]["error"];
                return Err(stream_error(e["code"].as_str().unwrap_or(""), e["message"].as_str().unwrap_or("response failed")));
            }
            "error" => {
                return Err(stream_error(ev["code"].as_str().unwrap_or(""), ev["message"].as_str().unwrap_or("stream error")));
            }
            _ => {}
        }
        Ok(false)
    }

    pub(crate) fn finish(mut self, cfg: &Config) -> Streamed {
        self.calls.sort_by_key(|(k, _)| *k);
        self.items.sort_by_key(|(k, _)| *k);
        let calls: Vec<AccumCall> = self.calls.into_iter().map(|(_, c)| c).filter(|c| !c.name.is_empty()).collect();
        let parts: Vec<Value> = self.items.into_iter().map(|(_, it)| it).collect();
        let native = (!parts.is_empty()).then(|| native_turn("responses", cfg, parts));
        (self.content, calls, self.usage, native)
    }
}

pub(crate) fn responses_stream(
    http: &ureq::Agent,
    cfg: &Config,
    messages: &[Message],
    tools: Option<&Value>,
    cancel: &AtomicBool,
    mut on_content: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
) -> Result<Streamed> {
    let url = format!("{}/responses", cfg.base_url.trim_end_matches('/'));
    let resp = post_json(http, cfg, &url, &responses_body(cfg, messages, tools))?;
    let mut acc = ResponsesAcc::default();
    let mut done = false;
    for_each_sse(resp, cancel, |data| match serde_json::from_str::<Value>(data) {
        Ok(ev) => {
            done = acc.on_event(&ev, &mut on_content, &mut on_reasoning)?;
            Ok(done)
        }
        Err(_) => Ok(false),
    })?;
    if !done && !cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(anyhow!("network error: the response stream ended before it completed"));
    }
    Ok(acc.finish(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{FunctionCall, ToolCall};

    fn cfg(provider: &str, model: &str, reasoning: &str) -> Config {
        let mut c = Config::default();
        c.provider = provider.into();
        c.model = model.into();
        c.reasoning = reasoning.into();
        c
    }

    fn assistant_call(id: &str, args: &str) -> Message {
        let mut m = Message::user("");
        m.role = "assistant".into();
        m.content = "Reading it.".into();
        m.tool_calls = Some(vec![ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall { name: "read_file".into(), arguments: args.into() },
        }]);
        m
    }

    fn spec() -> Value {
        json!([{"type": "function", "function": {"name": "read_file", "description": "Read", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}])
    }

    /// A conversation: two system messages, a prompt with an image, a tool
    /// round trip with an injected image, and the next prompt.
    fn history() -> Vec<Message> {
        vec![
            Message::system("You are picoder."),
            Message::system("Project context."),
            Message::user_with_images("What's in a.rs?", vec!["data:image/png;base64,AAAA".into()]),
            assistant_call("toolu_1", r#"{"path":"a.rs"}"#),
            Message::tool("toolu_1".into(), "fn main() {}".into()),
            Message::user_with_images("(attached image(s) from view_image)", vec!["data:image/jpeg;base64,BBBB".into()]),
        ]
    }

    #[test]
    fn messages_body_folds_history_into_alternating_turns() {
        let c = cfg("anthropic", "claude-opus-5-5", "auto");
        let b = anthropic_body(&c, &history(), Some(&spec()));
        assert_eq!(b["system"][0]["text"], "You are picoder.\n\nProject context.");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral", "caches tools + system");
        let msgs = b["messages"].as_array().unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert_eq!(msgs[0]["content"][0]["source"]["media_type"], "image/png");
        assert_eq!(msgs[1]["content"][1]["input"]["path"], "a.rs", "tool_use input is JSON");
        // Tool results lead the user turn that carries them; the image follows.
        let last = msgs[2]["content"].as_array().unwrap();
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[0]["tool_use_id"], "toolu_1");
        assert_eq!(last[1]["type"], "image");
        assert_eq!(last.last().unwrap()["cache_control"]["type"], "ephemeral", "breakpoint on the latest turn");
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(b["tools"][0]["eager_input_streaming"], true);
        assert!(b.get("thinking").is_none() && b.get("output_config").is_none(), "auto sends nothing");
        assert!(b.get("temperature").is_none());
    }

    #[test]
    fn messages_effort_follows_the_reasoning_setting() {
        let b = anthropic_body(&cfg("anthropic", "claude-opus-5-5", "high"), &history(), None);
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["thinking"]["display"], "summarized");
        assert_eq!(b["output_config"]["effort"], "high");
        assert!(b.get("tools").is_none());
        // Off can't send `disabled` (a 400 on current models): lowest effort.
        let off = anthropic_body(&cfg("anthropic", "claude-opus-5-5", "off"), &history(), None);
        assert!(off.get("thinking").is_none());
        assert_eq!(off["output_config"]["effort"], "low");
    }

    fn run_anthropic(events: &[Value], c: &Config) -> (Streamed, String) {
        let mut acc = AnthropicAcc::default();
        let (mut text, mut reasoning) = (String::new(), String::new());
        for ev in events {
            if acc.on_event(ev, &mut |t| text.push_str(t), &mut |t| reasoning.push_str(t)).unwrap() {
                break;
            }
        }
        (acc.finish(c, &mut |t| text.push_str(t)), reasoning)
    }

    #[test]
    fn messages_stream_yields_text_tools_usage_and_signed_thinking() {
        let c = cfg("anthropic", "claude-opus-5-5", "high");
        let events = vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 100, "cache_read_input_tokens": 900, "cache_creation_input_tokens": 50, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Need the file."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig=="}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Reading."}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "read_file", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "th\":\"b.rs\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "ping"}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
            json!({"type": "message_stop"}),
        ];
        let ((content, calls, usage, native), reasoning) = run_anthropic(&events, &c);
        assert_eq!(content, "Reading.");
        assert_eq!(reasoning, "Need the file.");
        assert_eq!(calls.len(), 1);
        assert_eq!((calls[0].id.as_str(), calls[0].name.as_str(), calls[0].args.as_str()), ("toolu_9", "read_file", r#"{"path":"b.rs"}"#));
        assert_eq!(
            usage.unwrap(),
            Usage { prompt_tokens: 1050, completion_tokens: 42, cached_tokens: 900, cache_write_tokens: 50 }
        );
        let native = native.unwrap();
        let parts = native["parts"].as_array().unwrap();
        assert_eq!(parts[0], json!({"type": "thinking", "thinking": "Need the file.", "signature": "sig=="}));
        assert_eq!(parts[2]["input"], json!({"path": "b.rs"}));

        // Next request in this run replays the turn verbatim, thinking first.
        let mut turn = assistant_call("toolu_9", r#"{"path":"b.rs"}"#);
        turn.native = Some(native.clone());
        let msgs = vec![Message::user("go"), turn.clone(), Message::tool("toolu_9".into(), "ok".into())];
        let b = anthropic_body(&c, &msgs, None);
        assert_eq!(b["messages"][1]["content"][0]["signature"], "sig==");
        // Another model (or a resumed run) gets the reconstruction instead.
        let other = cfg("anthropic", "claude-sonnet-5-5", "high");
        let b = anthropic_body(&other, &msgs, None);
        assert_eq!(b["messages"][1]["content"][0]["type"], "text");
        assert!(!b.to_string().contains("sig=="));
    }

    #[test]
    fn messages_stream_errors_are_typed_for_retry() {
        let mut acc = AnthropicAcc::default();
        let err = acc
            .on_event(&json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}), &mut |_| {}, &mut |_| {})
            .unwrap_err();
        assert_eq!(err.downcast_ref::<HttpStatus>().unwrap().code, 529);
        let ((content, _, _, _), _) = run_anthropic(
            &[
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "message_delta", "delta": {"stop_reason": "refusal"}, "usage": {"output_tokens": 0}}),
                json!({"type": "message_stop"}),
            ],
            &cfg("anthropic", "claude-opus-5-5", "auto"),
        );
        assert!(content.contains("refusal"), "{content}");
    }

    #[test]
    fn responses_body_is_stateless_and_asks_reasoners_for_encrypted_reasoning() {
        let c = cfg("openai", "gpt-5.2", "off");
        let b = responses_body(&c, &history(), Some(&spec()));
        assert_eq!(b["store"], false);
        assert!(b["prompt_cache_key"].as_str().unwrap().starts_with("picoder-"));
        assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(b["reasoning"], json!({"effort": "none", "summary": "auto"}));
        let input = b["input"].as_array().unwrap();
        assert_eq!(input[0], json!({"role": "system", "content": "You are picoder."}));
        assert_eq!(input[2]["content"][1], json!({"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "auto"}));
        assert_eq!(input[3], json!({"role": "assistant", "content": "Reading it."}));
        assert_eq!(input[4]["type"], "function_call");
        assert_eq!(input[4]["call_id"], "toolu_1");
        assert_eq!(input[5], json!({"type": "function_call_output", "call_id": "toolu_1", "output": "fn main() {}"}));
        assert_eq!(b["tools"][0]["parameters"]["type"], "object");
        // A non-reasoning model is never sent the include (it 400s).
        let plain = responses_body(&cfg("openai", "gpt-4o-mini", "auto"), &history(), None);
        assert!(plain.get("include").is_none() && plain.get("reasoning").is_none());
        assert_eq!(responses_effort("gpt-5", "off"), "minimal");
        assert_eq!(responses_effort("o3", "off"), "low");
    }

    #[test]
    fn responses_stream_yields_text_tools_usage_and_replayable_items() {
        let c = cfg("openai", "gpt-5.2", "auto");
        let events = vec![
            json!({"type": "response.created"}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": 0, "delta": "Plan."}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Plan."}], "encrypted_content": "ENC"}}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg_1", "role": "assistant"}}),
            json!({"type": "response.output_text.delta", "output_index": 1, "delta": "Checking."}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "Checking."}]}}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_7", "name": "read_file", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"path\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "\"c.rs\"}"}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_7", "name": "read_file", "arguments": "{\"path\":\"c.rs\"}", "status": "completed"}}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 2000, "input_tokens_details": {"cached_tokens": 1500}, "output_tokens": 60, "total_tokens": 2060}}}),
        ];
        let mut acc = ResponsesAcc::default();
        let (mut text, mut reasoning) = (String::new(), String::new());
        for ev in &events {
            if acc.on_event(ev, &mut |t| text.push_str(t), &mut |t| reasoning.push_str(t)).unwrap() {
                break;
            }
        }
        let (content, calls, usage, native) = acc.finish(&c);
        assert_eq!((content.as_str(), reasoning.as_str()), ("Checking.", "Plan."));
        assert_eq!((calls[0].id.as_str(), calls[0].args.as_str()), ("call_7", r#"{"path":"c.rs"}"#));
        assert_eq!(usage.unwrap(), Usage { prompt_tokens: 2000, completion_tokens: 60, cached_tokens: 1500, cache_write_tokens: 0 });

        let mut turn = assistant_call("call_7", r#"{"path":"c.rs"}"#);
        turn.native = native;
        let b = responses_body(&c, &[Message::user("go"), turn, Message::tool("call_7".into(), "ok".into())], None);
        let input = b["input"].as_array().unwrap();
        // Replayed in order; reasoning keeps its id + encrypted content, the
        // rest drop their ids (nothing is stored server-side).
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!((input[1]["id"].as_str(), input[1]["encrypted_content"].as_str()), (Some("rs_1"), Some("ENC")));
        assert!(input[2].get("id").is_none() && input[2].get("status").is_none());
        assert_eq!(input[3]["call_id"], "call_7");
        assert!(input[3].get("id").is_none());
        assert_eq!(input[4]["type"], "function_call_output");
    }
}

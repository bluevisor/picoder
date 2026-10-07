//! MCP (Model Context Protocol) client, over two transports:
//!
//! - **stdio** — the server is spawned as a child process and spoken to in
//!   newline-delimited JSON-RPC 2.0; a reader thread per server funnels stdout
//!   lines into a channel so request/response is synchronous with a timeout.
//! - **Streamable HTTP** — every message is POSTed to the server's URL and the
//!   reply is either JSON or an SSE stream carrying it (possibly after
//!   notifications). `Mcp-Session-Id` and `MCP-Protocol-Version` ride along;
//!   a 404 on a known session means it expired, so the client re-initializes.
//!   A 401 means sign-in: `/mcp login <server>` runs the MCP authorization flow
//!   (protected-resource metadata → authorization-server metadata → dynamic
//!   client registration → PKCE in the browser, see auth.rs) and keeps the
//!   token, refreshed on expiry, in picoder's config dir (0600).
//!
//! Servers come from config.json's `mcp_servers` plus a project `.mcp.json`
//! (Claude Code's `mcpServers` shape, in the working directory or at the repo
//! root). A project file can make picoder run any command, so each of its
//! servers waits for a one-time `/mcp approve <name>`, pinned to a hash of
//! that entry: a changed entry asks again.
//!
//! Tools are advertised as `mcp__<server>__<tool>`; resources through two
//! built-in tools (`list_mcp_resources`, `read_mcp_resource`); prompts as
//! slash commands (`/mcp__<server>__<prompt> args`). Lists are paginated.
//!
//! Dependency-free (ureq + serde_json, already here) so it still
//! cross-compiles to a static ARMv6 binary.

use crate::config::McpServerConfig;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// The protocol revision picoder offers; a server may answer with any of
/// `SUPPORTED` (everything here speaks the same tools/resources/prompts).
const PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const INIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound on list pages, against a server whose cursor never runs out.
const MAX_PAGES: usize = 50;
/// Files in picoder's config dir: project-server decisions and OAuth tokens.
const APPROVALS_FILE: &str = "mcp-approvals.json";
const TOKENS_FILE: &str = "mcp-oauth.json";

/// Built-in tools for MCP resources, advertised while any server has some.
pub const LIST_RESOURCES: &str = "list_mcp_resources";
pub const READ_RESOURCE: &str = "read_mcp_resource";

/// A tool exposed by an MCP server, in the shape the model schema needs.
#[derive(Clone)]
pub struct McpTool {
    /// `mcp__<server>__<tool>` — the name advertised to the model.
    pub full_name: String,
    pub server: String,
    pub tool: String,
    pub description: String,
    pub schema: Value,
}

/// A prompt a server offers, run as the slash command `/<command>`.
#[derive(Clone)]
pub struct McpPrompt {
    /// `mcp__<server>__<prompt>`, sanitized like tool names.
    pub command: String,
    pub server: String,
    pub name: String,
    pub description: String,
    /// (argument name, required) in the server's order.
    pub arguments: Vec<(String, bool)>,
}

/// Status of one server, for startup notices and `/mcp`.
pub struct ServerStatus {
    pub name: String,
    pub ok: bool,
    /// Not running because it waits on the user (approval or sign-in), not
    /// because it broke.
    pub waiting: bool,
    pub detail: String,
}

/// Why a request failed, kept typed so each transport can recover the right way.
#[derive(Debug, PartialEq)]
enum Fail {
    /// The connection itself broke: process gone, network down, bad HTTP status.
    Transport(String),
    /// The server answered with a JSON-RPC error.
    Rpc(String),
    /// HTTP 401: sign-in needed (with the `WWW-Authenticate` challenge).
    Auth(Option<String>),
    /// HTTP 404 on a request that carried a session id: the session is gone.
    Expired,
    Interrupted,
    Timeout(String),
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Transport(m) | Fail::Rpc(m) | Fail::Timeout(m) => f.write_str(m),
            Fail::Auth(_) => f.write_str("the server needs you to sign in"),
            Fail::Expired => f.write_str("the MCP session expired"),
            Fail::Interrupted => f.write_str("interrupted"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Transport {
    Stdio,
    Http,
}

/// Which transport a config asks for: `type` when set, else `url` → HTTP and
/// `command` → stdio.
fn transport_of(cfg: &McpServerConfig) -> Result<Transport, String> {
    match cfg.kind.to_ascii_lowercase().as_str() {
        "" if !cfg.url.is_empty() => Ok(Transport::Http),
        "" if !cfg.command.is_empty() => Ok(Transport::Stdio),
        "" => Err("needs a \"command\" (stdio) or a \"url\" (HTTP)".into()),
        "stdio" => Ok(Transport::Stdio),
        "http" | "streamable-http" | "streamable_http" | "streamablehttp" => Ok(Transport::Http),
        "sse" => Err("the legacy HTTP+SSE transport isn't supported; use the server's Streamable HTTP URL".into()),
        other => Err(format!("unknown transport type {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// stdio transport
// ---------------------------------------------------------------------------

struct StdioConn {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
}

impl StdioConn {
    fn spawn(cfg: &McpServerConfig) -> Result<StdioConn, Fail> {
        let mut child = Command::new(&cfg.command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Fail::Transport(format!("spawn failed: {e}")))?;
        let stdin = child.stdin.take().ok_or(Fail::Transport("no stdin".into()))?;
        let stdout = child.stdout.take().ok_or(Fail::Transport("no stdout".into()))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                if tx.send(line.trim_end().to_string()).is_err() {
                    break;
                }
                line.clear();
            }
        });
        Ok(StdioConn { child, stdin, rx })
    }

    fn send(&mut self, msg: &Value) -> Result<(), Fail> {
        let line = msg.to_string();
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| Fail::Transport(format!("write failed: {e}")))
    }

    /// Wait for the response to `id`, skipping notifications and anything
    /// else. Polls `cancel` so Esc can abort a slow call.
    fn wait(&mut self, id: i64, method: &str, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, Fail> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(Fail::Interrupted);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Fail::Timeout(format!("timed out waiting for {method}")));
            }
            let line = match self.rx.recv_timeout(left.min(Duration::from_millis(250))) {
                Ok(l) => l,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return Err(Fail::Transport("server closed the connection".into())),
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(r) = pick(&msg, id) {
                return r;
            }
        }
    }
}

/// The outcome carried by `msg` if it is the response to our request `id`.
/// A server→client request can reuse the same number (separate id spaces),
/// so anything with a `method` is never a response.
fn pick(msg: &Value, id: i64) -> Option<Result<Value, Fail>> {
    if msg.get("id").and_then(Value::as_i64) != Some(id) || msg.get("method").is_some() {
        return None;
    }
    if let Some(err) = msg.get("error") {
        let m = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
        return Some(Err(Fail::Rpc(m.to_string())));
    }
    Some(Ok(msg.get("result").cloned().unwrap_or(Value::Null)))
}

// ---------------------------------------------------------------------------
// Streamable HTTP transport
// ---------------------------------------------------------------------------

/// The `data` payload of each event in a `text/event-stream`.
struct SseEvents<R> {
    lines: std::io::Lines<R>,
}

impl<R: BufRead> Iterator for SseEvents<R> {
    type Item = String;
    fn next(&mut self) -> Option<String> {
        let mut data: Option<String> = None;
        loop {
            let Some(Ok(raw)) = self.lines.next() else {
                return data; // end of stream: flush what's pending
            };
            let line = raw.strip_suffix('\r').unwrap_or(&raw);
            if line.is_empty() {
                if data.is_some() {
                    return data;
                }
                continue;
            }
            // `event:`, `id:`, `retry:` and `:` comments carry nothing we need.
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                match &mut data {
                    Some(d) => {
                        d.push('\n');
                        d.push_str(rest);
                    }
                    None => data = Some(rest.to_string()),
                }
            }
        }
    }
}

/// The messages in one JSON-RPC payload (a single message or a batch).
fn batch(msg: &Value) -> Vec<&Value> {
    match msg.as_array() {
        Some(a) => a.iter().collect(),
        None => vec![msg],
    }
}

/// The response to request `id` from an HTTP reply body: JSON (one message or
/// a batch), or an SSE stream that may carry notifications and server
/// requests before it.
fn read_response(content_type: &str, mut body: impl BufRead, id: i64) -> Result<Value, Fail> {
    if content_type.starts_with("text/event-stream") {
        for data in (SseEvents { lines: body.lines() }) {
            let Ok(msg) = serde_json::from_str::<Value>(&data) else { continue };
            if let Some(r) = batch(&msg).into_iter().find_map(|m| pick(m, id)) {
                return r;
            }
        }
        return Err(Fail::Transport("the event stream ended before the response".into()));
    }
    let mut text = String::new();
    body.read_to_string(&mut text).map_err(|e| Fail::Transport(format!("read failed: {e}")))?;
    let msg: Value = serde_json::from_str(&text)
        .map_err(|_| Fail::Transport(format!("the reply isn't JSON: {}", crate::api::truncate(text.trim(), 200))))?;
    batch(&msg)
        .into_iter()
        .find_map(|m| pick(m, id))
        .unwrap_or_else(|| Err(Fail::Transport("the reply held no response to the request".into())))
}

/// One POST of a JSON-RPC message. Returns the session id the server set (if
/// any) and, for a request (`id`), its response.
fn http_post(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &str,
    id: Option<i64>,
) -> Result<(Option<String>, Value), Fail> {
    let mut req = agent
        .post(url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(body) {
        Ok(resp) => {
            let sid = resp.header("mcp-session-id").map(str::to_string);
            let Some(id) = id else { return Ok((sid, Value::Null)) };
            if resp.status() == 202 {
                return Err(Fail::Transport("the server accepted the request but sent no response".into()));
            }
            let ct = resp.content_type().to_ascii_lowercase();
            let v = read_response(&ct, BufReader::new(resp.into_reader()), id)?;
            Ok((sid, v))
        }
        Err(ureq::Error::Status(401, r)) => Err(Fail::Auth(r.header("www-authenticate").map(str::to_string))),
        Err(ureq::Error::Status(404, _)) => Err(Fail::Expired),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            Err(Fail::Transport(format!("HTTP {code}: {}", crate::api::truncate(detail.trim(), 300))))
        }
        Err(e) => Err(Fail::Transport(e.to_string())),
    }
}

/// Wait for a worker thread's result, giving up on timeout or cancel.
fn wait_on<T>(rx: &Receiver<T>, timeout: Duration, cancel: Option<&AtomicBool>, what: &str) -> Result<T, Fail> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(Fail::Interrupted);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Fail::Timeout(format!("timed out waiting for {what}")));
        }
        match rx.recv_timeout(left.min(Duration::from_millis(250))) {
            Ok(v) => return Ok(v),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return Err(Fail::Transport("the connection closed".into())),
        }
    }
}

struct HttpConn {
    agent: ureq::Agent,
    url: String,
    /// Static headers from the config (already env-expanded).
    headers: Vec<(String, String)>,
    /// OAuth access token, sent as a bearer unless a static header sets
    /// `Authorization` itself.
    token: Option<String>,
    session: Option<String>,
    /// The negotiated protocol version, sent on every request after initialize.
    version: Option<String>,
}

impl HttpConn {
    fn new(cfg: &McpServerConfig, token: Option<String>) -> HttpConn {
        HttpConn {
            agent: crate::api::agent_http(),
            url: cfg.url.clone(),
            headers: cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            token,
            session: None,
            version: None,
        }
    }

    fn request_headers(&self) -> Vec<(String, String)> {
        let mut h = self.headers.clone();
        if let Some(t) = &self.token {
            if !h.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")) {
                h.push(("Authorization".into(), format!("Bearer {t}")));
            }
        }
        if let Some(s) = &self.session {
            h.push(("Mcp-Session-Id".into(), s.clone()));
        }
        if let Some(v) = &self.version {
            h.push(("MCP-Protocol-Version".into(), v.clone()));
        }
        h
    }

    /// POST `msg` on a worker thread (so Esc and the timeout work) and return
    /// the response to `id`. Keeps the session id the server hands out.
    fn post(&mut self, msg: &Value, id: Option<i64>, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, Fail> {
        let (agent, url, headers, body) = (self.agent.clone(), self.url.clone(), self.request_headers(), msg.to_string());
        let had_session = self.session.is_some();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(http_post(&agent, &url, &headers, &body, id));
        });
        match wait_on(&rx, timeout, cancel, "the MCP server")? {
            Ok((sid, v)) => {
                if sid.is_some() {
                    self.session = sid;
                }
                Ok(v)
            }
            Err(Fail::Expired) if !had_session => Err(Fail::Transport("HTTP 404: no MCP endpoint at this URL".into())),
            Err(Fail::Expired) => {
                self.session = None;
                Err(Fail::Expired)
            }
            Err(e) => Err(e),
        }
    }

    /// End the session (best-effort, briefly: this runs at exit).
    fn close(&mut self) {
        if let Some(sid) = self.session.take() {
            let mut req = self.agent.delete(&self.url).timeout(Duration::from_secs(2)).set("Mcp-Session-Id", &sid);
            for (k, v) in self.request_headers() {
                req = req.set(&k, &v);
            }
            let _ = req.call();
        }
    }
}

// ---------------------------------------------------------------------------
// One connected server
// ---------------------------------------------------------------------------

enum Conn {
    Stdio(StdioConn),
    Http(HttpConn),
}

struct Server {
    conn: Conn,
    next_id: i64,
    has_tools: bool,
    has_resources: bool,
    has_prompts: bool,
}

impl Server {
    fn connect(cfg: &McpServerConfig, token: Option<String>) -> Result<Server, Fail> {
        let conn = match transport_of(cfg).map_err(Fail::Transport)? {
            Transport::Stdio => Conn::Stdio(StdioConn::spawn(cfg)?),
            Transport::Http => Conn::Http(HttpConn::new(cfg, token)),
        };
        let mut s = Server { conn, next_id: 1, has_tools: false, has_resources: false, has_prompts: false };
        if let Err(e) = s.initialize() {
            s.close();
            return Err(e);
        }
        Ok(s)
    }

    fn is_http(&self) -> bool {
        matches!(self.conn, Conn::Http(_))
    }

    fn send(&mut self, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, Fail> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match &mut self.conn {
            Conn::Stdio(c) => {
                c.send(&msg)?;
                c.wait(id, method, timeout, cancel)
            }
            Conn::Http(c) => c.post(&msg, Some(id), timeout, cancel),
        }
    }

    /// A request, re-initializing once if the HTTP session expired under it.
    fn request(&mut self, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, Fail> {
        match self.send(method, params.clone(), timeout, cancel) {
            Err(Fail::Expired) => {
                self.initialize()?;
                self.send(method, params, timeout, cancel)
            }
            r => r,
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match &mut self.conn {
            Conn::Stdio(c) => {
                let _ = c.send(&msg);
            }
            Conn::Http(c) => {
                let _ = c.post(&msg, None, INIT_TIMEOUT, None);
            }
        }
    }

    /// The handshake: offer our protocol version, accept any we support, note
    /// what the server can do.
    fn initialize(&mut self) -> Result<(), Fail> {
        if let Conn::Http(c) = &mut self.conn {
            c.session = None;
            c.version = None;
        }
        let r = self.send(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "picoder", "version": env!("CARGO_PKG_VERSION")}
            }),
            INIT_TIMEOUT,
            None,
        )?;
        let version = r.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        if !SUPPORTED.contains(&version) {
            return Err(Fail::Transport(format!("the server speaks MCP {version:?}, which picoder doesn't support")));
        }
        if let Conn::Http(c) = &mut self.conn {
            c.version = Some(version.to_string());
        }
        let caps = r.get("capabilities");
        let has = |k: &str| caps.and_then(|c| c.get(k)).is_some();
        (self.has_tools, self.has_resources, self.has_prompts) = (has("tools"), has("resources"), has("prompts"));
        self.notify("notifications/initialized", json!({}));
        Ok(())
    }

    /// Every item of a paginated list (`tools/list` → `tools`, …).
    fn list_all(&mut self, method: &str, key: &str) -> Result<Vec<Value>, Fail> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = self.request(method, params, INIT_TIMEOUT, None)?;
            if let Some(items) = page.get(key).and_then(Value::as_array) {
                out.extend(items.iter().cloned());
            }
            match page.get("nextCursor").and_then(Value::as_str) {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    fn set_token(&mut self, token: Option<String>) {
        if let Conn::Http(c) = &mut self.conn {
            c.token = token;
        }
    }

    fn close(&mut self) {
        match &mut self.conn {
            Conn::Stdio(c) => {
                let _ = c.child.kill();
                let _ = c.child.wait();
            }
            Conn::Http(c) => c.close(),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration: global + project servers, env expansion, approvals
// ---------------------------------------------------------------------------

/// Where a server's entry came from.
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    Global,
    /// A project `.mcp.json` in `dir`; `hash` pins the approved entry.
    Project { dir: PathBuf, hash: String },
}

/// Every configured server: config.json's plus the project's `.mcp.json`
/// (which wins on a shared name, as in Claude Code).
pub fn entries(global: &BTreeMap<String, McpServerConfig>, cwd: &Path) -> Vec<(String, McpServerConfig, Origin)> {
    let mut all: BTreeMap<String, (McpServerConfig, Origin)> =
        global.iter().map(|(k, v)| (k.clone(), (v.clone(), Origin::Global))).collect();
    if let Some((dir, servers)) = project_servers(cwd) {
        for (name, cfg, hash) in servers {
            all.insert(name, (cfg, Origin::Project { dir: dir.clone(), hash }));
        }
    }
    all.into_iter().map(|(name, (cfg, origin))| (name, cfg, origin)).collect()
}

/// (server name, entry, hash of the raw entry) from a `.mcp.json`.
type ProjectServer = (String, McpServerConfig, String);

/// The project's `.mcp.json` servers — from the working directory, else the
/// repo root — and the directory it was found in.
fn project_servers(cwd: &Path) -> Option<(PathBuf, Vec<ProjectServer>)> {
    let root = cwd.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf);
    let dir = std::iter::once(cwd.to_path_buf()).chain(root).find(|d| d.join(".mcp.json").is_file())?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(".mcp.json")).ok()?).ok()?;
    let servers = v.get("mcpServers").or_else(|| v.get("mcp_servers"))?.as_object()?;
    let list = servers
        .iter()
        .filter_map(|(name, raw)| {
            let cfg = McpServerConfig::parse(raw)?;
            Some((name.clone(), cfg, hex(&crate::auth::sha256(raw.to_string().as_bytes()))))
        })
        .collect();
    Some((dir, list))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Expand `${VAR}` and `${VAR:-default}` from the environment. A variable
/// that is unset with no default is an error naming it.
fn expand_env(s: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else {
            out.push_str(&rest[i..]); // unterminated: keep it literally
            return Ok(out);
        };
        let inner = &after[..j];
        let (var, default) = match inner.split_once(":-") {
            Some((v, d)) => (v, Some(d)),
            None => (inner, None),
        };
        // As in a shell: `:-` also covers a variable that is set but empty.
        let value = std::env::var(var).ok().filter(|v| !v.is_empty() || default.is_none());
        match (value, default) {
            (Some(v), _) => out.push_str(&v),
            (None, Some(d)) => out.push_str(d),
            (None, None) => return Err(format!("environment variable {var} is not set")),
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `cfg` with every string field env-expanded.
fn expanded(cfg: &McpServerConfig) -> Result<McpServerConfig, String> {
    let map = |m: &BTreeMap<String, String>| -> Result<BTreeMap<String, String>, String> {
        m.iter().map(|(k, v)| Ok((k.clone(), expand_env(v)?))).collect()
    };
    Ok(McpServerConfig {
        command: expand_env(&cfg.command)?,
        args: cfg.args.iter().map(|a| expand_env(a)).collect::<Result<_, _>>()?,
        env: map(&cfg.env)?,
        kind: cfg.kind.clone(),
        url: expand_env(&cfg.url)?,
        headers: map(&cfg.headers)?,
        oauth: cfg.oauth.clone(),
    })
}

fn read_store<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Write a store file atomically, readable only by the user (it can hold
/// OAuth tokens).
fn write_store<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    crate::config::atomic_write(path, text.as_bytes()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
struct Decision {
    allow: bool,
    #[serde(default)]
    hash: String,
}

/// project dir → server name → decision
type Approvals = BTreeMap<String, BTreeMap<String, Decision>>;

/// Some(true): approved for exactly this entry; Some(false): denied (a denial
/// sticks even if the entry changes); None: never decided, or the approved
/// entry has changed since.
fn approval(store: &Path, dir: &Path, name: &str, hash: &str) -> Option<bool> {
    let all: Approvals = read_store(&store.join(APPROVALS_FILE));
    let d = all.get(&dir.display().to_string())?.get(name)?;
    if !d.allow {
        return Some(false);
    }
    (d.hash == hash).then_some(true)
}

fn record_approval(store: &Path, dir: &Path, name: &str, hash: &str, allow: bool) -> Result<(), String> {
    let path = store.join(APPROVALS_FILE);
    let mut all: Approvals = read_store(&path);
    all.entry(dir.display().to_string())
        .or_default()
        .insert(name.to_string(), Decision { allow, hash: hash.to_string() });
    write_store(&path, &all)
}

// ---------------------------------------------------------------------------
// OAuth (MCP authorization spec)
// ---------------------------------------------------------------------------

/// A stored OAuth grant for one server URL.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Token {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_at: u64,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    token_endpoint: String,
    #[serde(default)]
    resource: String,
}

/// A parameter of a `WWW-Authenticate: Bearer k="v", k2=v2` challenge.
fn challenge_param(header: &str, key: &str) -> Option<String> {
    let mut rest = header;
    while let Some(i) = rest.find(key) {
        let before_ok = i == 0 || matches!(rest.as_bytes()[i - 1], b' ' | b',');
        let after = rest[i + key.len()..].trim_start();
        if before_ok {
            if let Some(v) = after.strip_prefix('=') {
                let v = v.trim_start();
                return Some(match v.strip_prefix('"') {
                    Some(q) => q.split('"').next().unwrap_or("").to_string(),
                    None => v.split([',', ' ']).next().unwrap_or("").to_string(),
                });
            }
        }
        rest = &rest[i + key.len()..];
    }
    None
}

/// (origin, path) of an http(s) URL: origin lowercased, path without query,
/// fragment or trailing slash ("" for the root).
fn split_url(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].to_ascii_lowercase();
    let path = rest[end..].split(['?', '#']).next().unwrap_or("").trim_end_matches('/').to_string();
    Some((format!("{scheme}://{host}"), path))
}

/// Where to look for the protected-resource metadata (RFC 9728) of a server
/// when its 401 didn't say: path-specific first, then the root.
fn resource_metadata_urls(server_url: &str) -> Vec<String> {
    let Some((origin, path)) = split_url(server_url) else { return Vec::new() };
    let base = format!("{origin}/.well-known/oauth-protected-resource");
    if path.is_empty() {
        vec![base]
    } else {
        vec![format!("{base}{path}"), base]
    }
}

/// Where to look for an issuer's metadata: RFC 8414 then OpenID Connect
/// discovery, with path insertion for issuers that have a path.
fn auth_metadata_urls(issuer: &str) -> Vec<String> {
    let Some((origin, path)) = split_url(issuer) else { return Vec::new() };
    if path.is_empty() {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ]
    } else {
        vec![
            format!("{origin}/.well-known/oauth-authorization-server{path}"),
            format!("{origin}/.well-known/openid-configuration{path}"),
            format!("{origin}{path}/.well-known/openid-configuration"),
        ]
    }
}

/// What an authorization server needs from us, discovered for one MCP server.
#[derive(Debug, PartialEq)]
struct AuthPlan {
    authorize: String,
    token: String,
    register: Option<String>,
    /// The canonical URI of the MCP server, sent as `resource` (RFC 8707).
    resource: String,
    scope: String,
}

/// Build the plan from the discovered documents: `prm` is the protected-
/// resource metadata (None for servers predating it, whose own origin is then
/// the authorization server), `asm` the authorization-server metadata (None:
/// the spec's default endpoints on the issuer).
fn auth_plan(server_url: &str, challenge: Option<&str>, prm: Option<&Value>, asm: Option<&Value>) -> Result<AuthPlan, String> {
    let (origin, path) = split_url(server_url).ok_or_else(|| format!("{server_url} isn't an http(s) URL"))?;
    let resource = prm
        .and_then(|p| p.get("resource").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{origin}{path}"));
    let issuer = prm
        .and_then(|p| p.get("authorization_servers").and_then(Value::as_array))
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or(origin);
    let endpoint = |k: &str, default: &str| {
        asm.and_then(|m| m.get(k).and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| format!("{issuer}{default}"))
    };
    if let Some(methods) = asm.and_then(|m| m.get("code_challenge_methods_supported")).and_then(Value::as_array) {
        if !methods.iter().any(|m| m.as_str() == Some("S256")) {
            return Err("the authorization server doesn't support PKCE (S256), which MCP requires".into());
        }
    }
    let scope = challenge
        .and_then(|c| challenge_param(c, "scope"))
        .or_else(|| {
            prm.and_then(|p| p.get("scopes_supported").and_then(Value::as_array)).map(|a| {
                a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")
            })
        })
        .unwrap_or_default();
    Ok(AuthPlan {
        authorize: endpoint("authorization_endpoint", "/authorize"),
        token: endpoint("token_endpoint", "/token"),
        register: match asm {
            Some(m) => m.get("registration_endpoint").and_then(Value::as_str).map(str::to_string),
            None => Some(format!("{issuer}/register")),
        },
        resource,
        scope,
    })
}

/// GET a JSON document; None on any failure (absence is a normal answer here).
fn fetch_json(agent: &ureq::Agent, url: &str) -> Option<Value> {
    let resp = agent.get(url).timeout(Duration::from_secs(10)).set("Accept", "application/json").call().ok()?;
    serde_json::from_str(&resp.into_string().ok()?).ok()
}

/// Discover how to sign in to the MCP server at `server_url`.
fn discover(agent: &ureq::Agent, server_url: &str, challenge: Option<&str>) -> Result<AuthPlan, String> {
    let prm_urls = challenge
        .and_then(|c| challenge_param(c, "resource_metadata"))
        .into_iter()
        .chain(resource_metadata_urls(server_url));
    let prm = prm_urls.filter_map(|u| fetch_json(agent, &u)).find(|v| v.get("authorization_servers").is_some());
    let issuer = prm
        .as_ref()
        .and_then(|p| p.get("authorization_servers")?.as_array()?.first()?.as_str().map(str::to_string))
        .or_else(|| split_url(server_url).map(|(o, _)| o))
        .unwrap_or_default();
    let asm = auth_metadata_urls(&issuer)
        .into_iter()
        .filter_map(|u| fetch_json(agent, &u))
        .find(|v| v.get("authorization_endpoint").is_some());
    auth_plan(server_url, challenge, prm.as_ref(), asm.as_ref())
}

/// Dynamic client registration (RFC 7591) as a public client.
fn register_client(agent: &ureq::Agent, endpoint: &str, redirect_uri: &str) -> Result<(String, String), String> {
    let body = json!({
        "client_name": "picoder",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let text = match agent.post(endpoint).set("Content-Type", "application/json").send_string(&body.to_string()) {
        Ok(r) => r.into_string().unwrap_or_default(),
        Err(ureq::Error::Status(code, r)) => {
            return Err(format!("client registration failed: HTTP {code}: {}", crate::api::truncate(&r.into_string().unwrap_or_default(), 300)))
        }
        Err(e) => return Err(format!("client registration failed: {e}")),
    };
    let v: Value = serde_json::from_str(&text).map_err(|_| "client registration returned non-JSON".to_string())?;
    let id = v.get("client_id").and_then(Value::as_str).filter(|s| !s.is_empty());
    let id = id.ok_or("client registration returned no client_id")?;
    let secret = v.get("client_secret").and_then(Value::as_str).unwrap_or_default();
    Ok((id.to_string(), secret.to_string()))
}

// ---------------------------------------------------------------------------
// The client: every configured server, running or not
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum State {
    Running,
    Failed(String),
    NeedsApproval,
    Denied,
    NeedsLogin,
}

struct Slot {
    name: String,
    /// As configured (unexpanded: `${VAR}`s are resolved at each connect).
    cfg: McpServerConfig,
    origin: Origin,
    state: State,
    server: Option<Server>,
    tools: Vec<McpTool>,
    prompts: Vec<McpPrompt>,
    resources: bool,
}

/// Owns the servers and the flattened tool list the model sees.
pub struct Mcp {
    slots: Vec<Slot>,
    tools: Vec<McpTool>,
    status: Vec<ServerStatus>,
    /// Where approvals and OAuth tokens are kept (picoder's config dir).
    store: PathBuf,
}

impl Mcp {
    pub fn disabled() -> Mcp {
        Mcp { slots: Vec::new(), tools: Vec::new(), status: Vec::new(), store: crate::config::config_dir() }
    }

    /// Start `entries` (see `entries`), keeping approvals and tokens under
    /// `store`. Project servers the user hasn't approved (for this exact
    /// entry) stay stopped. Failures are recorded in `status` (and reported via
    /// `/mcp`), never fatal.
    pub fn start(entries: Vec<(String, McpServerConfig, Origin)>, store: PathBuf) -> Mcp {
        let mut mcp = Mcp { slots: Vec::new(), tools: Vec::new(), status: Vec::new(), store };
        for (name, cfg, origin) in entries {
            let state = match &origin {
                Origin::Project { dir, hash } => match approval(&mcp.store, dir, &name, hash) {
                    Some(true) => State::Running,
                    Some(false) => State::Denied,
                    None => State::NeedsApproval,
                },
                Origin::Global => State::Running,
            };
            mcp.slots.push(Slot { name, cfg, origin, state, server: None, tools: Vec::new(), prompts: Vec::new(), resources: false });
        }
        for i in 0..mcp.slots.len() {
            if mcp.slots[i].state == State::Running {
                mcp.open(i);
            }
        }
        mcp.rebuild();
        mcp
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// (Re)connect slot `i` and load its tools and prompts.
    fn open(&mut self, i: usize) {
        if let Some(mut old) = self.slots[i].server.take() {
            old.close();
        }
        let cfg = match expanded(&self.slots[i].cfg) {
            Ok(c) => c,
            Err(e) => {
                self.slots[i].state = State::Failed(e);
                return;
            }
        };
        let token = if transport_of(&cfg) == Ok(Transport::Http) { self.bearer(&cfg.url, false) } else { None };
        let slot = &mut self.slots[i];
        (slot.tools, slot.prompts, slot.resources) = (Vec::new(), Vec::new(), false);
        let mut server = match Server::connect(&cfg, token) {
            Ok(s) => s,
            Err(Fail::Auth(_)) => {
                slot.state = State::NeedsLogin;
                return;
            }
            Err(e) => {
                slot.state = State::Failed(e.to_string());
                return;
            }
        };
        // Older servers list tools without declaring the capability: always
        // ask, but only a server that declared tools fails over the answer.
        match server.list_all("tools/list", "tools") {
            Ok(t) => slot.tools = parse_tools(&slot.name, &t),
            Err(e) if server.has_tools => {
                server.close();
                slot.state = State::Failed(format!("tools/list failed: {e}"));
                return;
            }
            Err(_) => {}
        }
        if server.has_prompts {
            if let Ok(p) = server.list_all("prompts/list", "prompts") {
                slot.prompts = parse_prompts(&slot.name, &p);
            }
        }
        slot.resources = server.has_resources;
        slot.server = Some(server);
        slot.state = State::Running;
    }

    /// Refresh the flattened tool list and statuses after any change.
    fn rebuild(&mut self) {
        let running = || self.slots.iter().filter(|s| s.state == State::Running);
        let mut tools: Vec<McpTool> = running().flat_map(|s| s.tools.iter().cloned()).collect();
        if running().any(|s| s.resources) {
            tools.extend(resource_tools());
        }
        self.tools = tools;
        self.status = self.slots.iter().map(Slot::status).collect();
    }

    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    pub fn status(&self) -> &[ServerStatus] {
        &self.status
    }

    /// True if `name` is one of our advertised tool names.
    pub fn handles(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.full_name == name)
    }

    /// Tools that only read (the resource tools): no approval, fine in plan mode.
    pub fn is_read_only(&self, name: &str) -> bool {
        name == LIST_RESOURCES || name == READ_RESOURCE
    }

    /// The prompts of the running servers as slash-palette entries
    /// (`/mcp__server__prompt`, description).
    pub fn prompt_palette(&self) -> Vec<(String, String)> {
        self.slots
            .iter()
            .filter(|s| s.state == State::Running)
            .flat_map(|s| s.prompts.iter())
            .map(|p| {
                let usage: Vec<String> =
                    p.arguments.iter().map(|(n, req)| if *req { format!("<{n}>") } else { format!("[{n}]") }).collect();
                let desc = if p.description.is_empty() { "MCP prompt".to_string() } else { p.description.clone() };
                let desc = if usage.is_empty() { desc } else { format!("{} — {desc}", usage.join(" ")) };
                (format!("/{}", p.command), desc)
            })
            .collect()
    }

    /// A request to server `name`, with the recovery each failure allows: an
    /// expired OAuth token is refreshed, a crashed stdio server is restarted
    /// once (only when the transport itself failed — a tool's own error must
    /// not re-run a side-effecting call), and a rejected token marks the
    /// server as needing sign-in.
    fn request_on(&mut self, name: &str, method: &str, params: Value, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<Value, Fail> {
        let i = self
            .slots
            .iter()
            .position(|s| s.name == name && s.server.is_some())
            .ok_or_else(|| Fail::Transport(format!("MCP server {name} is not running")))?;
        let http = self.slots[i].server.as_ref().is_some_and(Server::is_http);
        if http {
            // Swap in a fresh token before it lapses mid-call.
            let url = expanded(&self.slots[i].cfg).map(|c| c.url).unwrap_or_default();
            let token = self.bearer(&url, false);
            if let Some(s) = self.slots[i].server.as_mut() {
                s.set_token(token);
            }
        }
        let r = self.slots[i].server.as_mut().map(|s| s.request(method, params.clone(), timeout, cancel));
        match r.unwrap_or_else(|| Err(Fail::Transport("not running".into()))) {
            Err(Fail::Transport(e)) if !http => {
                self.open(i);
                self.rebuild();
                if self.slots[i].state != State::Running {
                    return Err(Fail::Transport(format!("server crashed ({e}) and couldn't be restarted")));
                }
                let s = self.slots[i].server.as_mut().ok_or(Fail::Transport("not running".into()))?;
                s.request(method, params, timeout, cancel)
            }
            Err(Fail::Auth(_)) if http => {
                let url = expanded(&self.slots[i].cfg).map(|c| c.url).unwrap_or_default();
                if let Some(token) = self.bearer(&url, true) {
                    if let Some(s) = self.slots[i].server.as_mut() {
                        s.set_token(Some(token));
                        if let Ok(v) = s.request(method, params, timeout, cancel) {
                            return Ok(v);
                        }
                    }
                }
                if let Some(mut s) = self.slots[i].server.take() {
                    s.close();
                }
                self.slots[i].state = State::NeedsLogin;
                self.rebuild();
                Err(Fail::Transport(format!("{name} needs you to sign in again: /mcp login {name}")))
            }
            r => r,
        }
    }

    /// Call an advertised tool with the given arguments; returns text.
    /// `cancel` aborts the wait (the server may still finish the work).
    pub fn call(&mut self, full_name: &str, args: &Value, cancel: &AtomicBool) -> String {
        match full_name {
            LIST_RESOURCES => return self.list_resources(args, cancel),
            READ_RESOURCE => return self.read_resource(args, cancel),
            _ => {}
        }
        let Some(tool) = self.tools.iter().find(|t| t.full_name == full_name).cloned() else {
            return format!("ERROR: unknown MCP tool {full_name}");
        };
        let params = json!({"name": tool.tool, "arguments": args});
        match self.request_on(&tool.server, "tools/call", params, CALL_TIMEOUT, Some(cancel)) {
            Ok(r) => render_tool_result(&r),
            Err(e) => format!("ERROR: MCP call failed: {e}"),
        }
    }

    fn resource_servers(&self, only: Option<&str>) -> Vec<String> {
        self.slots
            .iter()
            .filter(|s| s.state == State::Running && s.resources && only.is_none_or(|o| o == s.name))
            .map(|s| s.name.clone())
            .collect()
    }

    fn list_resources(&mut self, args: &Value, cancel: &AtomicBool) -> String {
        let only = args.get("server").and_then(Value::as_str).filter(|s| !s.is_empty());
        let names = self.resource_servers(only);
        if names.is_empty() {
            return match only {
                Some(o) => format!("ERROR: no running MCP server named {o} offers resources."),
                None => "No running MCP server offers resources.".into(),
            };
        }
        let mut out = String::new();
        for name in names {
            let listed = self.list_on(&name, "resources/list", "resources", cancel);
            match listed {
                Ok(items) if items.is_empty() => out.push_str(&format!("{name}: (no resources)\n")),
                Ok(items) => {
                    for r in items {
                        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("");
                        out.push_str(&format!("{name}: {}", s("uri")));
                        if !s("name").is_empty() {
                            out.push_str(&format!(" — {}", s("name")));
                        }
                        if !s("mimeType").is_empty() {
                            out.push_str(&format!(" ({})", s("mimeType")));
                        }
                        if !s("description").is_empty() {
                            out.push_str(&format!(": {}", s("description")));
                        }
                        out.push('\n');
                    }
                }
                Err(e) => out.push_str(&format!("{name}: ERROR {e}\n")),
            }
            if let Ok(templates) = self.list_on(&name, "resources/templates/list", "resourceTemplates", cancel) {
                for t in templates {
                    let s = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("");
                    out.push_str(&format!("{name}: template {} — {}\n", s("uriTemplate"), s("name")));
                }
            }
        }
        crate::api::truncate(out.trim_end(), crate::api::MAX_TOOL_OUTPUT)
    }

    /// Every page of a list on server `name`.
    fn list_on(&mut self, name: &str, method: &str, key: &str, cancel: &AtomicBool) -> Result<Vec<Value>, Fail> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = self.request_on(name, method, params, CALL_TIMEOUT, Some(cancel))?;
            if let Some(items) = page.get(key).and_then(Value::as_array) {
                out.extend(items.iter().cloned());
            }
            match page.get("nextCursor").and_then(Value::as_str) {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    fn read_resource(&mut self, args: &Value, cancel: &AtomicBool) -> String {
        let Some(uri) = args.get("uri").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            return "ERROR: read_mcp_resource needs a 'uri' (see list_mcp_resources).".into();
        };
        let named = args.get("server").and_then(Value::as_str).filter(|s| !s.is_empty());
        let candidates = self.resource_servers(named);
        let server = match (named, candidates.as_slice()) {
            (_, [one]) => one.clone(),
            (Some(n), []) => return format!("ERROR: no running MCP server named {n} offers resources."),
            (None, []) => return "ERROR: no running MCP server offers resources.".into(),
            (None, _) => return format!("ERROR: several servers offer resources ({}); pass 'server'.", candidates.join(", ")),
            (Some(n), _) => n.to_string(),
        };
        match self.request_on(&server, "resources/read", json!({"uri": uri}), CALL_TIMEOUT, Some(cancel)) {
            Ok(r) => render_resource(&r),
            Err(e) => format!("ERROR: reading {uri} failed: {e}"),
        }
    }

    /// Fetch prompt `command` (`mcp__server__prompt`, no slash) with `args`
    /// (positional, or `name=value`) and return its text to send as a turn.
    pub fn get_prompt(&mut self, command: &str, args: &str, cancel: &AtomicBool) -> Result<String, String> {
        let p = self
            .slots
            .iter()
            .filter(|s| s.state == State::Running)
            .flat_map(|s| s.prompts.iter())
            .find(|p| p.command == command)
            .cloned()
            .ok_or_else(|| format!("no MCP prompt /{command} (see /mcp)"))?;
        let values = bind_prompt_args(&p.arguments, args).map_err(|missing| {
            let usage: Vec<String> = p.arguments.iter().map(|(n, _)| format!("{n}=…")).collect();
            format!("/{command} needs {missing}. Usage: /{command} {}", usage.join(" "))
        })?;
        let r = self
            .request_on(&p.server, "prompts/get", json!({"name": p.name, "arguments": values}), CALL_TIMEOUT, Some(cancel))
            .map_err(|e| format!("prompt /{command} failed: {e}"))?;
        render_prompt(&r).ok_or_else(|| format!("prompt /{command} came back empty"))
    }

    /// `/mcp approve|deny <name>` for a project server: record the decision
    /// (pinned to the entry as it is now) and start or stop it.
    pub fn decide(&mut self, name: &str, allow: bool) -> Result<String, String> {
        let i = self.slot(name)?;
        let Origin::Project { dir, hash } = self.slots[i].origin.clone() else {
            return Err(format!("{name} comes from your own config.json; it needs no approval."));
        };
        record_approval(&self.store, &dir, name, &hash, allow)?;
        if allow {
            self.open(i);
        } else {
            if let Some(mut s) = self.slots[i].server.take() {
                s.close();
            }
            self.slots[i].state = State::Denied;
        }
        self.rebuild();
        Ok(match (allow, &self.slots[i].state) {
            (false, _) => format!("{name} denied; it won't start. /mcp approve {name} to allow it."),
            (true, State::Running) => format!("{name} approved and started: {}", self.status[i].detail),
            (true, _) => format!("{name} approved, but: {}", self.status[i].detail),
        })
    }

    /// `/mcp login <name>`: run the MCP authorization flow in the browser and
    /// reconnect with the token. `notify` shows progress (the URL, in case the
    /// browser can't open). Blocks until the redirect arrives (5-minute cap).
    pub fn login(&mut self, name: &str, notify: &dyn Fn(String)) -> Result<String, String> {
        let i = self.slot(name)?;
        if self.slots[i].state == State::NeedsApproval || self.slots[i].state == State::Denied {
            return Err(format!("{name} isn't approved yet: /mcp approve {name}"));
        }
        let cfg = expanded(&self.slots[i].cfg)?;
        if transport_of(&cfg)? != Transport::Http {
            return Err(format!("{name} is a stdio server; only HTTP servers sign in."));
        }
        let agent = crate::api::agent_http();
        // The server's 401 names its resource metadata (and maybe the scope).
        let probe = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": PROTOCOL_VERSION, "capabilities": {},
            "clientInfo": {"name": "picoder", "version": env!("CARGO_PKG_VERSION")}}});
        let headers: Vec<(String, String)> = cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let challenge = match http_post(&agent, &cfg.url, &headers, &probe.to_string(), Some(0)) {
            Err(Fail::Auth(h)) => h,
            _ => None,
        };
        let plan = discover(&agent, &cfg.url, challenge.as_deref())?;
        let oc = cfg.oauth.clone().unwrap_or_default();
        let (listener, redirect) = crate::auth::bind_redirect(oc.callback_port, "/callback").map_err(|e| e.to_string())?;
        let (client_id, client_secret) = if !oc.client_id.is_empty() {
            (oc.client_id.clone(), oc.client_secret.clone())
        } else if let Some(reg) = &plan.register {
            register_client(&agent, reg, &redirect)?
        } else {
            return Err(format!(
                "{name}'s authorization server has no dynamic client registration; register picoder with it \
                 (redirect {redirect}, or set oauth.callback_port) and put the client id in the server's \
                 \"oauth\": {{\"client_id\": …}}"
            ));
        };
        let resource = vec![("resource".to_string(), plan.resource.clone())];
        let provider = crate::auth::OAuthProvider {
            key: name.to_string(),
            label: format!("MCP server {name}"),
            auth_url: plan.authorize.clone(),
            token_url: plan.token.clone(),
            client_id: client_id.clone(),
            client_secret: client_secret.clone(),
            scope: if oc.scope.is_empty() { plan.scope.clone() } else { oc.scope.clone() },
            redirect_port: 0,
            redirect_path: "/callback".into(),
            extra: resource.clone(),
            token_extra: resource,
        };
        let login = crate::auth::start_on(provider, listener, redirect).map_err(|e| e.to_string())?;
        notify(format!("sign in to {name} in your browser (opening it now): {}", login.url));
        let t = login.finish().map_err(|e| e.to_string())?;
        let token = Token {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            expires_at: t.expires_at,
            client_id,
            client_secret,
            token_endpoint: plan.token,
            resource: plan.resource,
        };
        let path = self.store.join(TOKENS_FILE);
        let mut all: BTreeMap<String, Token> = read_store(&path);
        all.insert(cfg.url.clone(), token);
        write_store(&path, &all)?;
        self.open(i);
        self.rebuild();
        match &self.slots[i].state {
            State::Running => Ok(format!("signed in to {name}: {}", self.status[i].detail)),
            _ => Err(format!("signed in, but {name} still won't connect: {}", self.status[i].detail)),
        }
    }

    /// `/mcp logout <name>`: forget the token and disconnect.
    pub fn logout(&mut self, name: &str) -> Result<String, String> {
        let i = self.slot(name)?;
        let url = expanded(&self.slots[i].cfg)?.url;
        let path = self.store.join(TOKENS_FILE);
        let mut all: BTreeMap<String, Token> = read_store(&path);
        if all.remove(&url).is_none() {
            return Err(format!("not signed in to {name}."));
        }
        write_store(&path, &all)?;
        if let Some(mut s) = self.slots[i].server.take() {
            s.close();
        }
        self.slots[i].state = State::NeedsLogin;
        self.rebuild();
        Ok(format!("signed out of {name}."))
    }

    /// The bearer token for `url`: refreshed first when it's about to lapse
    /// (or `force`d, after a 401). None without a stored grant.
    fn bearer(&self, url: &str, force: bool) -> Option<String> {
        let path = self.store.join(TOKENS_FILE);
        let mut all: BTreeMap<String, Token> = read_store(&path);
        let t = all.get(url)?.clone();
        let stale = t.expires_at != 0 && crate::config::now_unix() + 60 >= t.expires_at;
        if !(force || stale) || t.refresh_token.is_empty() {
            return (!force).then_some(t.access_token);
        }
        let p = crate::auth::OAuthProvider {
            key: url.to_string(),
            label: url.to_string(),
            auth_url: String::new(),
            token_url: t.token_endpoint.clone(),
            client_id: t.client_id.clone(),
            client_secret: t.client_secret.clone(),
            scope: String::new(),
            redirect_port: 0,
            redirect_path: String::new(),
            extra: Vec::new(),
            token_extra: if t.resource.is_empty() { Vec::new() } else { vec![("resource".into(), t.resource.clone())] },
        };
        let old = crate::config::OAuthToken {
            access_token: t.access_token.clone(),
            refresh_token: t.refresh_token.clone(),
            expires_at: t.expires_at,
            account_id: String::new(),
        };
        let fresh = crate::auth::refresh(&p, &old).ok()?;
        let updated = Token {
            access_token: fresh.access_token.clone(),
            refresh_token: fresh.refresh_token,
            expires_at: fresh.expires_at,
            ..t
        };
        all.insert(url.to_string(), updated);
        let _ = write_store(&path, &all);
        Some(fresh.access_token)
    }

    fn slot(&self, name: &str) -> Result<usize, String> {
        self.slots
            .iter()
            .position(|s| s.name == name)
            .ok_or_else(|| format!("no MCP server named {name:?} (see /mcp)"))
    }

    /// Lines for `/mcp`: per server its state, transport, origin, auth and
    /// what it offers.
    pub fn describe(&self) -> Vec<String> {
        let tokens: BTreeMap<String, Token> = read_store(&self.store.join(TOKENS_FILE));
        let mut out = Vec::new();
        for (slot, st) in self.slots.iter().zip(&self.status) {
            let tag = match &slot.state {
                State::Running => "ok",
                State::Failed(_) => "FAILED",
                State::NeedsApproval => "needs approval",
                State::Denied => "denied",
                State::NeedsLogin => "needs sign-in",
            };
            let transport = match transport_of(&slot.cfg) {
                Ok(Transport::Http) => "http",
                Ok(Transport::Stdio) => "stdio",
                Err(_) => "?",
            };
            let origin = match &slot.origin {
                Origin::Global => "config.json".to_string(),
                Origin::Project { dir, .. } => format!("{}/.mcp.json", dir.display()),
            };
            let mut auth = String::new();
            if transport == "http" {
                let url = expanded(&slot.cfg).map(|c| c.url).unwrap_or_default();
                auth = match tokens.get(&url) {
                    Some(t) if t.expires_at == 0 => " · signed in".into(),
                    Some(t) => {
                        let left = t.expires_at.saturating_sub(crate::config::now_unix());
                        if left == 0 { " · token expired".into() } else { format!(" · signed in ({}m left)", left / 60) }
                    }
                    None if slot.cfg.headers.keys().any(|k| k.eq_ignore_ascii_case("authorization")) => " · auth header".into(),
                    None => " · no auth".into(),
                };
            }
            out.push(format!("mcp {} [{tag}] {transport} from {origin}{auth} — {}", slot.name, st.detail));
            if slot.state == State::Running {
                if !slot.tools.is_empty() {
                    let names: Vec<&str> = slot.tools.iter().map(|t| t.full_name.as_str()).collect();
                    out.push(format!("  tools: {}", names.join(", ")));
                }
                if !slot.prompts.is_empty() {
                    let names: Vec<String> = slot.prompts.iter().map(|p| format!("/{}", p.command)).collect();
                    out.push(format!("  prompts: {}", names.join(", ")));
                }
            }
        }
        out
    }

    /// Stop every server (best-effort) on shutdown.
    pub fn shutdown(&mut self) {
        for s in &mut self.slots {
            if let Some(mut server) = s.server.take() {
                server.close();
            }
        }
    }
}

impl Slot {
    fn status(&self) -> ServerStatus {
        let name = &self.name;
        let (ok, waiting, detail) = match &self.state {
            State::Running => {
                let n = self.tools.len();
                let mut d = format!("{n} tool{}", if n == 1 { "" } else { "s" });
                if !self.prompts.is_empty() {
                    let p = self.prompts.len();
                    d.push_str(&format!(", {p} prompt{}", if p == 1 { "" } else { "s" }));
                }
                if self.resources {
                    d.push_str(", resources");
                }
                (true, false, d)
            }
            State::NeedsApproval => {
                let from = match &self.origin {
                    Origin::Project { dir, .. } => format!("{}/.mcp.json", dir.display()),
                    Origin::Global => "config".into(),
                };
                let what = if self.cfg.url.is_empty() {
                    let cmdline: Vec<&str> = std::iter::once(self.cfg.command.as_str()).chain(self.cfg.args.iter().map(String::as_str)).collect();
                    format!("runs `{}`", cmdline.join(" "))
                } else {
                    format!("connects to {}", self.cfg.url)
                };
                (false, true, format!("project server from {from} {what} — not started until you approve it: /mcp approve {name} (or /mcp deny {name})"))
            }
            State::Denied => (false, true, format!("denied — /mcp approve {name} to allow it")),
            State::NeedsLogin => (false, true, format!("needs sign-in: /mcp login {name}")),
            State::Failed(e) => (false, false, e.clone()),
        };
        ServerStatus { name: name.clone(), ok, waiting, detail }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The built-in resource tools, advertised while any server has resources.
fn resource_tools() -> Vec<McpTool> {
    let tool = |name: &str, description: &str, schema: Value| McpTool {
        full_name: name.to_string(),
        server: String::new(),
        tool: String::new(),
        description: description.to_string(),
        schema,
    };
    vec![
        tool(
            LIST_RESOURCES,
            "List the resources (files, records, docs) your MCP servers offer, with their URIs. Optionally only one server's.",
            json!({"type": "object", "properties": {"server": {"type": "string", "description": "Only this server's resources."}}}),
        ),
        tool(
            READ_RESOURCE,
            "Read one MCP resource by URI (from list_mcp_resources).",
            json!({"type": "object", "properties": {
                "server": {"type": "string", "description": "The server offering it (optional with only one)."},
                "uri": {"type": "string"}
            }, "required": ["uri"]}),
        ),
    ]
}

/// OpenAI-compatible APIs require function names matching
/// `^[a-zA-Z0-9_-]{1,64}$`; one bad MCP name would 400 every request. Map
/// anything else to `_` and clamp the length. Dispatch looks tools up by this
/// sanitized name, while the call still uses the server's real tool name.
fn sanitize_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    out.truncate(64);
    out
}

fn parse_tools(server: &str, items: &[Value]) -> Vec<McpTool> {
    items
        .iter()
        .filter_map(|t| {
            let tool = t.get("name").and_then(Value::as_str)?;
            Some(McpTool {
                full_name: sanitize_name(&format!("mcp__{server}__{tool}")),
                server: server.to_string(),
                tool: tool.to_string(),
                description: t.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                schema: t.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            })
        })
        .collect()
}

fn parse_prompts(server: &str, items: &[Value]) -> Vec<McpPrompt> {
    items
        .iter()
        .filter_map(|p| {
            let name = p.get("name").and_then(Value::as_str)?;
            let arguments = p
                .get("arguments")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            let n = x.get("name").and_then(Value::as_str)?;
                            Some((n.to_string(), x.get("required").and_then(Value::as_bool).unwrap_or(false)))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(McpPrompt {
                command: sanitize_name(&format!("mcp__{server}__{name}")),
                server: server.to_string(),
                name: name.to_string(),
                description: p.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                arguments,
            })
        })
        .collect()
}

/// Bind a slash command's argument text to a prompt's arguments: `name=value`
/// pairs (a value runs until the next pair), else positional words with the
/// last argument taking the rest of the line. Err names the missing required
/// arguments.
fn bind_prompt_args(params: &[(String, bool)], raw: &str) -> Result<serde_json::Map<String, Value>, String> {
    let mut values = serde_json::Map::new();
    let words: Vec<&str> = raw.split_whitespace().collect();
    fn keyed<'a>(w: &'a str, params: &[(String, bool)]) -> Option<(&'a str, &'a str)> {
        w.split_once('=').filter(|(k, _)| params.iter().any(|(n, _)| n == k))
    }
    if words.first().is_some_and(|w| keyed(w, params).is_some()) {
        let mut current: Option<String> = None;
        for w in words {
            if let Some((k, v)) = keyed(w, params) {
                values.insert(k.to_string(), Value::String(v.to_string()));
                current = Some(k.to_string());
            } else if let Some(k) = &current {
                if let Some(Value::String(s)) = values.get_mut(k) {
                    s.push(' ');
                    s.push_str(w);
                }
            }
        }
    } else {
        for (i, (name, _)) in params.iter().enumerate() {
            let value = if i + 1 == params.len() { words.get(i..).map(|w| w.join(" ")) } else { words.get(i).map(|w| w.to_string()) };
            if let Some(v) = value.filter(|v| !v.is_empty()) {
                values.insert(name.clone(), Value::String(v));
            }
        }
    }
    let missing: Vec<&str> =
        params.iter().filter(|(n, req)| *req && !values.contains_key(n)).map(|(n, _)| n.as_str()).collect();
    if missing.is_empty() {
        Ok(values)
    } else {
        Err(missing.join(", "))
    }
}

/// A `prompts/get` result as one block of text to send as the user's turn;
/// None if it holds nothing.
fn render_prompt(result: &Value) -> Option<String> {
    let parts: Vec<String> = result
        .get("messages")?
        .as_array()?
        .iter()
        .filter_map(|m| {
            let text = content_text(m.get("content")?);
            if text.is_empty() {
                return None;
            }
            Some(match m.get("role").and_then(Value::as_str) {
                Some("assistant") => format!("(assistant) {text}"),
                _ => text,
            })
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The text of one content block (or a list of them).
fn content_text(c: &Value) -> String {
    if let Some(list) = c.as_array() {
        return list.iter().map(content_text).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
    }
    match c.get("type").and_then(Value::as_str) {
        Some("text") => c.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
        Some("resource") => {
            let r = c.get("resource");
            match r.and_then(|r| r.get("text")).and_then(Value::as_str) {
                Some(t) => t.to_string(),
                None => format!("[resource {}]", r.and_then(|r| r.get("uri")).and_then(Value::as_str).unwrap_or("")),
            }
        }
        Some("resource_link") => format!("[resource {}]", c.get("uri").and_then(Value::as_str).unwrap_or("")),
        Some(other) => format!("[{other} content]"),
        None => String::new(),
    }
}

/// A `resources/read` result: text contents as-is, binary as a note.
fn render_resource(result: &Value) -> String {
    let Some(contents) = result.get("contents").and_then(Value::as_array) else {
        return crate::api::truncate(&result.to_string(), crate::api::MAX_TOOL_OUTPUT);
    };
    let parts: Vec<String> = contents
        .iter()
        .map(|c| match (c.get("text").and_then(Value::as_str), c.get("blob").and_then(Value::as_str)) {
            (Some(t), _) => t.to_string(),
            (None, Some(b)) => format!(
                "[binary {}, {} bytes]",
                c.get("mimeType").and_then(Value::as_str).unwrap_or("data"),
                b.len() * 3 / 4
            ),
            _ => String::new(),
        })
        .collect();
    crate::api::truncate(&parts.join("\n"), crate::api::MAX_TOOL_OUTPUT)
}

/// MCP tool results are a list of content blocks; flatten them to text.
fn render_tool_result(result: &Value) -> String {
    let text = result.get("content").map(content_text).unwrap_or_default();
    if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
        return format!("ERROR: {}", if text.is_empty() { "tool reported an error".into() } else { text });
    }
    if text.is_empty() {
        // No content blocks (e.g. structured content only) — return the raw JSON.
        crate::api::truncate(&result.to_string(), crate::api::MAX_TOOL_OUTPUT)
    } else {
        crate::api::truncate(&text, crate::api::MAX_TOOL_OUTPUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("picoder-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const MINI_SERVER: &str = r#"
import sys, json
def send(o):
    sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    m=json.loads(line); i=m.get("id"); meth=m.get("method")
    if meth=="initialize":
        send({"jsonrpc":"2.0","id":i,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mini","version":"0.1"}}})
    elif meth=="tools/list":
        send({"jsonrpc":"2.0","id":i,"result":{"tools":[{"name":"add","description":"Add a and b.","inputSchema":{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}}}}]}})
    elif meth=="tools/call":
        a=m["params"]["arguments"]; send({"jsonrpc":"2.0","id":i,"result":{"content":[{"type":"text","text":"sum=%d"%(a.get("a",0)+a.get("b",0))}]}})
    elif i is not None:
        send({"jsonrpc":"2.0","id":i,"error":{"code":-32601,"message":"nope"}})
"#;

    fn stdio(command: &str, args: Vec<String>) -> McpServerConfig {
        McpServerConfig { command: command.into(), args, ..Default::default() }
    }

    #[test]
    #[ignore] // needs python3; run with `cargo test -- --ignored`
    fn mcp_handshake_list_and_call() {
        let path = std::env::temp_dir().join("picoder_mini_mcp.py");
        std::fs::write(&path, MINI_SERVER).unwrap();
        let cancel = AtomicBool::new(false);
        let cfg = stdio("python3", vec![path.to_string_lossy().into_owned()]);
        let mut mcp = Mcp::start(vec![("mini".into(), cfg, Origin::Global)], scratch("mini"));
        assert_eq!(mcp.tools().len(), 1, "status: {:?}", mcp.status().iter().map(|s| &s.detail).collect::<Vec<_>>());
        assert_eq!(mcp.tools()[0].full_name, "mcp__mini__add");
        assert!(mcp.handles("mcp__mini__add"));
        assert_eq!(mcp.call("mcp__mini__add", &json!({"a": 2, "b": 40}), &cancel), "sum=42");
        assert!(mcp.call("mcp__mini__missing", &json!({}), &cancel).starts_with("ERROR"));
    }

    #[test]
    fn sanitize_tool_names() {
        assert_eq!(sanitize_name("mcp__mini__add"), "mcp__mini__add");
        assert_eq!(sanitize_name("mcp__my.server__do/it"), "mcp__my_server__do_it");
        let long = sanitize_name(&format!("mcp__s__{}", "x".repeat(100)));
        assert_eq!(long.len(), 64);
    }

    #[test]
    fn bad_command_is_recorded_not_fatal() {
        let cfg = stdio("definitely-not-a-real-binary-xyz", vec![]);
        let mcp = Mcp::start(vec![("broken".into(), cfg, Origin::Global)], scratch("broken"));
        assert!(mcp.tools().is_empty());
        assert_eq!(mcp.status().len(), 1);
        assert!(!mcp.status()[0].ok && !mcp.status()[0].waiting);
    }

    #[test]
    fn transport_follows_type_then_url_or_command() {
        let mut c = McpServerConfig { url: "https://x/mcp".into(), ..Default::default() };
        assert_eq!(transport_of(&c), Ok(Transport::Http));
        c.kind = "sse".into();
        assert!(transport_of(&c).unwrap_err().contains("Streamable HTTP"));
        assert_eq!(transport_of(&stdio("npx", vec![])), Ok(Transport::Stdio));
        assert!(transport_of(&McpServerConfig::default()).is_err());
    }

    #[test]
    fn sse_events_skip_comments_and_join_multiline_data() {
        let stream = ": keepalive\nevent: message\ndata: {\"a\":\ndata: 1}\n\nid: 7\ndata: second\r\n\r\ndata: tail";
        let events: Vec<String> = SseEvents { lines: std::io::Cursor::new(stream).lines() }.collect();
        assert_eq!(events, ["{\"a\":\n1}", "second", "tail"]);
    }

    #[test]
    fn responses_come_from_json_batches_and_event_streams() {
        let body = |s: &str| std::io::Cursor::new(s.to_string());
        let ok = r#"{"jsonrpc":"2.0","id":3,"result":{"x":1}}"#;
        assert_eq!(read_response("application/json", body(ok), 3), Ok(json!({"x": 1})));
        let batch = r#"[{"jsonrpc":"2.0","method":"notifications/message"},{"jsonrpc":"2.0","id":3,"result":7}]"#;
        assert_eq!(read_response("application/json", body(batch), 3), Ok(json!(7)));
        let err = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-1,"message":"boom"}}"#;
        assert_eq!(read_response("application/json", body(err), 3), Err(Fail::Rpc("boom".into())));
        // A progress notification and a server request reusing our id come first.
        let sse = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":\"done\"}\n\n";
        assert_eq!(read_response("text/event-stream", body(sse), 3), Ok(json!("done")));
        let cut = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        assert!(matches!(read_response("text/event-stream", body(cut), 3), Err(Fail::Transport(_))));
    }

    #[test]
    fn oauth_discovery_urls_and_challenge() {
        let h = r#"Bearer error="invalid_token", resource_metadata="https://api.example.com/.well-known/oauth-protected-resource/mcp", scope="files:read files:write""#;
        assert_eq!(
            challenge_param(h, "resource_metadata").as_deref(),
            Some("https://api.example.com/.well-known/oauth-protected-resource/mcp")
        );
        assert_eq!(challenge_param(h, "scope").as_deref(), Some("files:read files:write"));
        assert_eq!(challenge_param("Bearer realm=x", "scope"), None);
        assert_eq!(
            resource_metadata_urls("https://API.example.com/v1/mcp/?q=1"),
            [
                "https://api.example.com/.well-known/oauth-protected-resource/v1/mcp",
                "https://api.example.com/.well-known/oauth-protected-resource"
            ]
        );
        assert_eq!(
            auth_metadata_urls("https://auth.example.com/tenant1"),
            [
                "https://auth.example.com/.well-known/oauth-authorization-server/tenant1",
                "https://auth.example.com/.well-known/openid-configuration/tenant1",
                "https://auth.example.com/tenant1/.well-known/openid-configuration"
            ]
        );
        assert_eq!(auth_metadata_urls("https://auth.example.com").len(), 2);
    }

    #[test]
    fn oauth_plan_from_metadata_documents() {
        let prm = json!({"resource": "https://api.example.com/mcp", "authorization_servers": ["https://auth.example.com/"], "scopes_supported": ["a", "b"]});
        let asm = json!({
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/oauth/authorize",
            "token_endpoint": "https://auth.example.com/oauth/token",
            "registration_endpoint": "https://auth.example.com/oauth/register",
            "code_challenge_methods_supported": ["S256"]
        });
        let plan = auth_plan("https://api.example.com/mcp", None, Some(&prm), Some(&asm)).unwrap();
        assert_eq!(plan.authorize, "https://auth.example.com/oauth/authorize");
        assert_eq!(plan.register.as_deref(), Some("https://auth.example.com/oauth/register"));
        assert_eq!((plan.resource.as_str(), plan.scope.as_str()), ("https://api.example.com/mcp", "a b"));
        // The 401's scope beats the advertised list.
        let ch = r#"Bearer scope="only""#;
        assert_eq!(auth_plan("https://api.example.com/mcp", Some(ch), Some(&prm), Some(&asm)).unwrap().scope, "only");
        // No metadata at all (a 2025-03-26 server): default endpoints on its origin.
        let bare = auth_plan("https://srv.example.com/mcp", None, None, None).unwrap();
        assert_eq!(bare.token, "https://srv.example.com/token");
        assert_eq!(bare.resource, "https://srv.example.com/mcp");
        // No PKCE, no deal.
        let plain = json!({"authorization_endpoint": "x", "token_endpoint": "y", "code_challenge_methods_supported": ["plain"]});
        assert!(auth_plan("https://srv.example.com/mcp", None, None, Some(&plain)).is_err());
    }

    #[test]
    fn env_expansion_with_defaults_and_missing_vars() {
        std::env::set_var("PICODER_TEST_MCP_TOKEN", "s3cret");
        assert_eq!(expand_env("Bearer ${PICODER_TEST_MCP_TOKEN}").unwrap(), "Bearer s3cret");
        assert_eq!(expand_env("${PICODER_TEST_MCP_NOPE:-http://localhost}/mcp").unwrap(), "http://localhost/mcp");
        assert!(expand_env("${PICODER_TEST_MCP_NOPE}").unwrap_err().contains("PICODER_TEST_MCP_NOPE"));
        assert_eq!(expand_env("no vars, ${unterminated").unwrap(), "no vars, ${unterminated");
    }

    #[test]
    fn project_servers_need_a_pinned_approval() {
        let store = scratch("approvals");
        let root = scratch("project");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let sub = root.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        let write = |cmd: &str| {
            std::fs::write(
                root.join(".mcp.json"),
                format!(r#"{{"mcpServers": {{"tool": {{"command": "{cmd}"}}, "remote": {{"type": "http", "url": "http://127.0.0.1:9/mcp"}}}}}}"#),
            )
            .unwrap()
        };
        write("definitely-not-a-real-binary-xyz");
        // Found from a subdirectory via the repo root; overrides a global entry.
        let mut global = BTreeMap::new();
        global.insert("tool".to_string(), stdio("echo", vec![]));
        let found = entries(&global, &sub);
        let tool = found.iter().find(|(n, ..)| n == "tool").unwrap();
        assert!(matches!(&tool.2, Origin::Project { dir, .. } if *dir == root));
        let mut mcp = Mcp::start(found, store.clone());
        assert!(mcp.status().iter().all(|s| !s.ok && s.waiting), "nothing runs before approval");
        assert!(mcp.status()[1].detail.contains("/mcp approve tool"));
        // Approve one: recorded and pinned (it then fails to spawn, which is fine here).
        assert!(mcp.decide("tool", true).is_ok());
        let again = Mcp::start(entries(&global, &sub), store.clone());
        assert!(!again.status()[1].waiting, "approval remembered");
        // Changing the entry asks again; a denial sticks.
        write("something-else");
        let changed = Mcp::start(entries(&global, &sub), store.clone());
        assert!(changed.status()[1].waiting);
        let mut m = Mcp::start(entries(&global, &sub), store.clone());
        m.decide("remote", false).unwrap();
        let denied = Mcp::start(entries(&global, &sub), store);
        assert!(denied.status()[0].detail.starts_with("denied"));
        assert!(m.decide("nope", true).is_err());
    }

    #[test]
    fn prompt_arguments_bind_by_name_or_position() {
        let params = vec![("lang".to_string(), true), ("topic".to_string(), false)];
        let got = bind_prompt_args(&params, "rust error handling in depth").unwrap();
        assert_eq!(got["lang"], "rust");
        assert_eq!(got["topic"], "error handling in depth");
        let got = bind_prompt_args(&params, "topic=async io lang=go").unwrap();
        assert_eq!((got["lang"].as_str(), got["topic"].as_str()), (Some("go"), Some("async io")));
        assert_eq!(bind_prompt_args(&params, "").unwrap_err(), "lang");
        assert!(bind_prompt_args(&[], "").unwrap().is_empty());
    }

    #[test]
    fn prompts_and_results_render_as_text() {
        let r = json!({"messages": [
            {"role": "user", "content": {"type": "text", "text": "Review this:"}},
            {"role": "user", "content": {"type": "resource", "resource": {"uri": "file:///a", "text": "fn main() {}"}}},
            {"role": "assistant", "content": {"type": "text", "text": "Sure."}}
        ]});
        assert_eq!(render_prompt(&r).unwrap(), "Review this:\n\nfn main() {}\n\n(assistant) Sure.");
        assert!(render_prompt(&json!({"messages": []})).is_none());
        let res = json!({"contents": [{"uri": "a", "text": "hello"}, {"uri": "b", "mimeType": "image/png", "blob": "AAAA"}]});
        assert_eq!(render_resource(&res), "hello\n[binary image/png, 3 bytes]");
        let err = json!({"isError": true, "content": [{"type": "text", "text": "bad input"}]});
        assert_eq!(render_tool_result(&err), "ERROR: bad input");
    }

    /// A Streamable HTTP server in-process: session ids, a paginated tool list
    /// with one page as SSE, a call answered over SSE after a notification,
    /// and a session that expires once (404) so the client must re-initialize.
    #[test]
    fn streamable_http_session_pagination_sse_and_expiry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        type Seen = Arc<Mutex<Vec<(String, BTreeMap<String, String>)>>>;
        let seen: Seen = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            let mut inits = 0;
            let mut expired_once = false;
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let mut headers = BTreeMap::new();
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                loop {
                    let mut l = String::new();
                    reader.read_line(&mut l).unwrap();
                    let l = l.trim_end();
                    if l.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = l.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                    }
                }
                let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                if first.starts_with("DELETE") {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let msg: Value = serde_json::from_slice(&body).unwrap();
                let method = msg["method"].as_str().unwrap_or("").to_string();
                log.lock().unwrap().push((method.clone(), headers.clone()));
                let id = msg["id"].clone();
                let session = headers.get("mcp-session-id").cloned().unwrap_or_default();
                let reply = |status: &str, extra: &str, ct: &str, body: String| {
                    format!("HTTP/1.1 {status}\r\n{extra}Content-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                };
                let json_ok = |result: Value| reply("200 OK", "", "application/json", json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
                let out = match method.as_str() {
                    "initialize" => {
                        inits += 1;
                        let result = json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}, "prompts": {}, "resources": {}}, "serverInfo": {"name": "fake", "version": "1"}}});
                        reply("200 OK", &format!("Mcp-Session-Id: s{inits}\r\n"), "application/json", result.to_string())
                    }
                    "notifications/initialized" => reply("202 Accepted", "", "text/plain", String::new()),
                    _ if session.is_empty() => reply("400 Bad Request", "", "text/plain", "no session".into()),
                    "tools/list" if msg["params"]["cursor"].is_null() => {
                        json_ok(json!({"tools": [{"name": "a", "inputSchema": {"type": "object"}}], "nextCursor": "p2"}))
                    }
                    "tools/list" => {
                        let r = json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "b", "description": "the b tool"}]}});
                        reply("200 OK", "", "text/event-stream", format!("event: message\ndata: {r}\n\n"))
                    }
                    "prompts/list" => json_ok(json!({"prompts": [{"name": "greet", "arguments": [{"name": "who", "required": true}]}]})),
                    "prompts/get" => json_ok(json!({"messages": [{"role": "user", "content": {"type": "text",
                        "text": format!("Say hi to {}", msg["params"]["arguments"]["who"].as_str().unwrap_or("?"))}}]})),
                    "tools/call" if session == "s1" && !expired_once => {
                        expired_once = true;
                        reply("404 Not Found", "", "text/plain", "session expired".into())
                    }
                    "tools/call" => {
                        let note = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progress": 1}});
                        let r = json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": "hi from b"}]}});
                        reply("200 OK", "", "text/event-stream", format!("data: {note}\n\ndata: {r}\n\n"))
                    }
                    _ => json_ok(json!({})),
                };
                let _ = s.write_all(out.as_bytes());
            }
        });

        let cfg = McpServerConfig { url: url.clone(), headers: [("X-Team".to_string(), "picoder".to_string())].into(), ..Default::default() };
        let mut mcp = Mcp::start(vec![("srv".into(), cfg, Origin::Global)], scratch("http"));
        assert!(mcp.status()[0].ok, "{}", mcp.status()[0].detail);
        let names: Vec<&str> = mcp.tools().iter().map(|t| t.full_name.as_str()).collect();
        assert_eq!(names, ["mcp__srv__a", "mcp__srv__b", LIST_RESOURCES, READ_RESOURCE], "both pages + resource tools");
        assert_eq!(mcp.prompt_palette()[0].0, "/mcp__srv__greet");

        let cancel = AtomicBool::new(false);
        assert_eq!(mcp.call("mcp__srv__b", &json!({}), &cancel), "hi from b", "re-initialized after the 404");
        assert_eq!(mcp.get_prompt("mcp__srv__greet", "Ada", &cancel).unwrap(), "Say hi to Ada");
        assert!(mcp.get_prompt("mcp__srv__greet", "", &cancel).unwrap_err().contains("needs who"));

        let log = seen.lock().unwrap();
        assert_eq!(log.iter().filter(|(m, _)| m == "initialize").count(), 2);
        for (method, h) in log.iter().filter(|(m, _)| m != "initialize") {
            assert_eq!(h.get("mcp-protocol-version").map(String::as_str), Some("2025-06-18"), "{method}");
            assert!(h.contains_key("mcp-session-id"), "{method}");
            assert_eq!(h.get("x-team").map(String::as_str), Some("picoder"));
        }
        let accept = &log[0].1["accept"];
        assert!(accept.contains("application/json") && accept.contains("text/event-stream"));
    }
}

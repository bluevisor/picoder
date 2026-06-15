//! Minimal MCP (Model Context Protocol) client over stdio. Spawns each
//! configured server as a child process and speaks newline-delimited JSON-RPC
//! 2.0 to it: `initialize`, `tools/list`, and `tools/call`. Discovered tools
//! are advertised to the model as `mcp__<server>__<tool>`.
//!
//! Dependency-free (serde_json only) so it still cross-compiles to a static
//! ARMv6 binary. A reader thread per server funnels stdout lines into a channel
//! so request/response is synchronous with a timeout and server-initiated
//! notifications are simply skipped.

use crate::config::McpServerConfig;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

const PROTOCOL_VERSION: &str = "2024-11-05";
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const INIT_TIMEOUT: Duration = Duration::from_secs(20);

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

struct Server {
    name: String,
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    next_id: i64,
}

impl Server {
    /// Send a request and wait for the response with the matching id, skipping
    /// notifications and unrelated messages. Returns the `result` value.
    /// Polls `cancel` (when given) so Esc can abort a slow tool call.
    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("write failed: {e}"))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
                return Err("interrupted".into());
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!("timed out waiting for {method}"));
            }
            // Short recv slices keep the cancel check responsive.
            let slice = remaining.min(Duration::from_millis(250));
            let line = match self.rx.recv_timeout(slice) {
                Ok(l) => l,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return Err("server closed the connection".into()),
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg.get("id").and_then(|v| v.as_i64()) != Some(id) {
                continue; // notification or a different response
            }
            if let Some(err) = msg.get("error") {
                let m = err.get("message").and_then(|v| v.as_str()).unwrap_or("unknown error");
                return Err(m.to_string());
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let line = json!({"jsonrpc":"2.0","method":method,"params":params}).to_string();
        let _ = self.stdin.write_all(line.as_bytes());
        let _ = self.stdin.write_all(b"\n");
        let _ = self.stdin.flush();
    }
}

/// Status of one server, for the `/mcp` command.
pub struct ServerStatus {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Owns the spawned servers and the flattened tool list.
pub struct Mcp {
    servers: Vec<Server>,
    tools: Vec<McpTool>,
    status: Vec<ServerStatus>,
    /// Stored configs so we can restart a crashed server on the next call.
    configs: BTreeMap<String, McpServerConfig>,
}

impl Mcp {
    pub fn disabled() -> Mcp {
        Mcp { servers: Vec::new(), tools: Vec::new(), status: Vec::new(), configs: BTreeMap::new() }
    }

    /// Spawn and initialize every configured server. Failures are recorded in
    /// `status` (and reported via `/mcp`) but never abort startup.
    pub fn launch(configs: &BTreeMap<String, McpServerConfig>) -> Mcp {
        let mut mcp = Mcp::disabled();
        mcp.configs = configs.clone();
        for (name, cfg) in configs {
            match Self::start_one(name, cfg) {
                Ok((server, mut tools)) => {
                    let n = tools.len();
                    mcp.tools.append(&mut tools);
                    mcp.servers.push(server);
                    mcp.status.push(ServerStatus {
                        name: name.clone(),
                        ok: true,
                        detail: format!("{n} tool{}", if n == 1 { "" } else { "s" }),
                    });
                }
                Err(e) => {
                    mcp.status.push(ServerStatus { name: name.clone(), ok: false, detail: e });
                }
            }
        }
        mcp
    }

    fn start_one(name: &str, cfg: &McpServerConfig) -> Result<(Server, Vec<McpTool>), String> {
        if cfg.command.is_empty() {
            return Err("no command configured".into());
        }
        let mut child = Command::new(&cfg.command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn failed: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
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
        let mut server = Server { name: name.to_string(), child, stdin, rx, next_id: 1 };

        server.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name":"picode","version":env!("CARGO_PKG_VERSION")}
            }),
            INIT_TIMEOUT,
            None,
        )?;
        server.notify("notifications/initialized", json!({}));

        let listed = server.request("tools/list", json!({}), INIT_TIMEOUT, None)?;
        let tools = parse_tools(name, &listed);
        Ok((server, tools))
    }

    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    pub fn status(&self) -> &[ServerStatus] {
        &self.status
    }

    /// True if `name` is one of our advertised `mcp__server__tool` names.
    pub fn handles(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.full_name == name)
    }

    /// Call an `mcp__server__tool` with the given arguments; returns text.
    /// `cancel` aborts the wait (the server may still finish the work).
    pub fn call(&mut self, full_name: &str, args: &Value, cancel: &AtomicBool) -> String {
        let Some(tool) = self.tools.iter().find(|t| t.full_name == full_name).cloned() else {
            return format!("ERROR: unknown MCP tool {full_name}");
        };
        let Some(server) = self.servers.iter_mut().find(|s| s.name == tool.server) else {
            return format!("ERROR: MCP server {} is not running", tool.server);
        };
        let params = json!({"name": tool.tool, "arguments": args});
        match server.request("tools/call", params, CALL_TIMEOUT, Some(cancel)) {
            Ok(result) => render_tool_result(&result),
            Err(e) => format!("ERROR: MCP call failed: {e}"),
        }
    }

    /// Terminate every server (best-effort) on shutdown.
    pub fn shutdown(&mut self) {
        for s in &mut self.servers {
            let _ = s.child.kill();
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.shutdown();
    }
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

fn parse_tools(server: &str, listed: &Value) -> Vec<McpTool> {
    let Some(arr) = listed.get("tools").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            let tool = t.get("name").and_then(|v| v.as_str())?;
            let description = t
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}}));
            Some(McpTool {
                full_name: sanitize_name(&format!("mcp__{server}__{tool}")),
                server: server.to_string(),
                tool: tool.to_string(),
                description,
                schema,
            })
        })
        .collect()
}

/// MCP tool results are a list of content blocks; flatten the text ones.
fn render_tool_result(result: &Value) -> String {
    if result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false) {
        let text = collect_text(result);
        return format!("ERROR: {}", if text.is_empty() { "tool reported an error".into() } else { text });
    }
    let text = collect_text(result);
    if text.is_empty() {
        // No text blocks (e.g. an image-only result) — return the raw JSON.
        crate::api::truncate(&result.to_string(), crate::api::MAX_TOOL_OUTPUT)
    } else {
        crate::api::truncate(&text, crate::api::MAX_TOOL_OUTPUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn mini_config() -> BTreeMap<String, McpServerConfig> {
        let path = std::env::temp_dir().join("picode_mini_mcp.py");
        std::fs::write(&path, MINI_SERVER).unwrap();
        let mut m = BTreeMap::new();
        m.insert(
            "mini".to_string(),
            McpServerConfig {
                command: "python3".into(),
                args: vec![path.to_string_lossy().into_owned()],
                env: Default::default(),
            },
        );
        m
    }

    #[test]
    #[ignore] // needs python3; run with `cargo test -- --ignored`
    fn mcp_handshake_list_and_call() {
        let cancel = AtomicBool::new(false);
        let mut mcp = Mcp::launch(&mini_config());
        assert_eq!(mcp.tools().len(), 1, "status: {:?}", mcp.status().iter().map(|s| &s.detail).collect::<Vec<_>>());
        assert_eq!(mcp.tools()[0].full_name, "mcp__mini__add");
        assert!(mcp.handles("mcp__mini__add"));
        let out = mcp.call("mcp__mini__add", &json!({"a": 2, "b": 40}), &cancel);
        assert_eq!(out, "sum=42");
        let bad = mcp.call("mcp__mini__missing", &json!({}), &cancel);
        assert!(bad.starts_with("ERROR"));
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
        let mut m = BTreeMap::new();
        m.insert(
            "broken".to_string(),
            McpServerConfig { command: "definitely-not-a-real-binary-xyz".into(), args: vec![], env: Default::default() },
        );
        let mcp = Mcp::launch(&m);
        assert!(mcp.tools().is_empty());
        assert_eq!(mcp.status().len(), 1);
        assert!(!mcp.status()[0].ok);
    }
}

fn collect_text(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            Some(other) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[{other} content]"));
            }
            None => {}
        }
    }
    out
}

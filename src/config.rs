//! Configuration: provider/base_url/model/api_key, loaded from
//! ~/.config/picode/config.json and overridable by env. Compatible with the
//! previous Python picode config so existing keys/memory carry over.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

/// One MCP server entry from config.json (`mcp_servers`). Spawned over stdio.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Model context window, for the ctx usage bar.
    #[serde(default = "default_ctx")]
    pub context_window: u32,
    /// USD per 1M tokens, for the session cost readout (DeepSeek defaults).
    #[serde(default = "default_price_in")]
    pub price_in: f64,
    #[serde(default = "default_price_out")]
    pub price_out: f64,
    /// MCP servers to launch, keyed by name (advertised as mcp__<name>__<tool>).
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    /// Auto-commit each successful edit to the working-directory git repo so
    /// every change is a restorable checkpoint. On by default; no-op outside a repo.
    #[serde(default = "default_true")]
    pub auto_commit: bool,
    /// True when the key came from the environment; we never persist it then.
    #[serde(skip)]
    pub key_from_env: bool,
}

fn default_true() -> bool {
    true
}

fn default_theme() -> String {
    "default".to_string()
}
fn default_ctx() -> u32 {
    128_000
}
fn default_price_in() -> f64 {
    0.27
}
fn default_price_out() -> f64 {
    1.10
}

impl Default for Config {
    fn default() -> Self {
        Config {
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-v4-pro".into(),
            api_key: String::new(),
            theme: default_theme(),
            context_window: default_ctx(),
            price_in: default_price_in(),
            price_out: default_price_out(),
            mcp_servers: BTreeMap::new(),
            auto_commit: true,
            key_from_env: false,
        }
    }
}

pub fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("picode")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn memory_path() -> PathBuf {
    config_dir().join("memory.md")
}

pub fn history_path() -> PathBuf {
    config_dir().join("history")
}

pub fn sessions_dir() -> PathBuf {
    config_dir().join("sessions")
}

/// Per-working-directory session file (so each project resumes its own chat).
pub fn session_path() -> PathBuf {
    use std::hash::{Hash, Hasher};
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cwd.hash(&mut h);
    sessions_dir().join(format!("{:016x}.json", h.finish()))
}

impl Config {
    pub fn load() -> Config {
        let mut cfg = Config::default();
        if let Ok(text) = std::fs::read_to_string(config_path()) {
            // Merge known fields; tolerate a partial/legacy file.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(s) = v.get("provider").and_then(|x| x.as_str()) {
                    cfg.provider = s.to_string();
                }
                if let Some(s) = v.get("base_url").and_then(|x| x.as_str()) {
                    cfg.base_url = s.to_string();
                }
                if let Some(s) = v.get("model").and_then(|x| x.as_str()) {
                    cfg.model = s.to_string();
                }
                if let Some(s) = v.get("api_key").and_then(|x| x.as_str()) {
                    cfg.api_key = s.to_string();
                }
                if let Some(s) = v.get("theme").and_then(|x| x.as_str()) {
                    cfg.theme = s.to_string();
                }
                if let Some(m) = v.get("mcp_servers") {
                    if let Ok(servers) = serde_json::from_value(m.clone()) {
                        cfg.mcp_servers = servers;
                    }
                }
                if let Some(b) = v.get("auto_commit").and_then(|x| x.as_bool()) {
                    cfg.auto_commit = b;
                }
            }
        }
        for var in ["DEEPSEEK_API_KEY", "PICODE_API_KEY"] {
            if let Ok(k) = std::env::var(var) {
                if !k.is_empty() {
                    cfg.api_key = k;
                    cfg.key_from_env = true;
                }
            }
        }
        cfg
    }

    /// Persist to disk with 0600 perms. An env-injected key is never written.
    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir).context("create config dir")?;
        let mut on_disk = self.clone();
        if self.key_from_env {
            on_disk.api_key = String::new();
        }
        let mut json = serde_json::json!({
            "provider": on_disk.provider,
            "base_url": on_disk.base_url,
            "model": on_disk.model,
            "api_key": on_disk.api_key,
            "theme": on_disk.theme,
        });
        // Preserve user-configured MCP servers across model/theme rewrites.
        if !on_disk.mcp_servers.is_empty() {
            json["mcp_servers"] = serde_json::to_value(&on_disk.mcp_servers).unwrap_or_default();
        }
        // Only written when turned off (default is on).
        if !on_disk.auto_commit {
            json["auto_commit"] = serde_json::json!(false);
        }
        let path = config_path();
        std::fs::write(&path, serde_json::to_string_pretty(&json)?)
            .context("write config")?;
        set_private(&path);
        Ok(())
    }

    /// Update only the model field on disk, preserving the rest of the file.
    pub fn persist_model(&self) {
        let mut disk = Config::default();
        if let Ok(text) = std::fs::read_to_string(config_path()) {
            if let Ok(c) = serde_json::from_str::<Config>(&text) {
                disk = c;
            }
        }
        disk.model = self.model.clone();
        disk.key_from_env = false;
        let _ = disk.save();
    }

    /// Persist only the theme name, preserving the rest of the file.
    pub fn persist_theme(theme: &str) {
        let mut disk = Config::default();
        if let Ok(text) = std::fs::read_to_string(config_path()) {
            if let Ok(c) = serde_json::from_str::<Config>(&text) {
                disk = c;
            }
        }
        disk.theme = theme.to_string();
        disk.key_from_env = false;
        let _ = disk.save();
    }
}

#[cfg(unix)]
fn set_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn set_private(_path: &std::path::Path) {}

/// Interactive first-run setup, executed before the TUI starts (cooked stdin).
pub fn run_setup() -> Result<Config> {
    let mut cfg = Config::load();
    println!("\x1b[1mpicode setup\x1b[0m");
    println!("Provider:  1) DeepSeek (default)   2) OpenAI   3) Groq   4) Custom");
    let choice = prompt("> [1] ")?;
    let choice = if choice.is_empty() { "1".into() } else { choice };
    let (prov, base, model): (String, String, String) = match choice.as_str() {
        "2" => ("openai".into(), "https://api.openai.com/v1".into(), "gpt-4o-mini".into()),
        "3" => (
            "groq".into(),
            "https://api.groq.com/openai/v1".into(),
            "llama-3.3-70b-versatile".into(),
        ),
        "4" => {
            let p = nonempty(prompt("provider name: ")?, "custom");
            let b = prompt("base_url (OpenAI-compatible): ")?;
            let m = prompt("model: ")?;
            (p, b, m)
        }
        _ => ("deepseek".into(), "https://api.deepseek.com".into(), "deepseek-v4-pro".into()),
    };
    let model = if choice == "4" {
        model
    } else {
        let m = prompt(&format!("model [{model}]: "))?;
        nonempty(m, &model)
    };
    let key = prompt("API key: ")?;
    cfg.provider = prov;
    cfg.base_url = base;
    cfg.model = model;
    if !key.is_empty() {
        cfg.api_key = key;
        cfg.key_from_env = false;
    }
    cfg.save()?;
    println!("\x1b[32msaved {}\x1b[0m", config_path().display());
    Ok(cfg)
}

fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn nonempty(s: String, fallback: &str) -> String {
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

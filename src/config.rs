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
    /// Effective API key for the current provider (resolved from api_keys or env;
    /// NOT persisted directly — api_keys is the persistent store).
    #[serde(default)]
    pub api_key: String,
    /// Per-provider API keys persisted to disk. Keyed by provider name.
    #[serde(default)]
    pub api_keys: BTreeMap<String, String>,
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
    /// Ask the model to think before answering, via the DeepSeek-style
    /// `"thinking": {"type": "enabled"}` request field. Off by default;
    /// providers that don't know the field may reject requests — turn it off.
    #[serde(default)]
    pub thinking: bool,
    /// Default permission mode for new sessions: "ask", "bypass", or "plan".
    #[serde(default = "default_permission")]
    pub permission: String,
    /// Max tool-call rounds per turn. 0 means "auto" (an internal safe limit).
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u32,
    /// True when the current provider's key came from the environment; we never
    /// persist it then.
    #[serde(skip)]
    pub key_from_env: bool,
}

fn default_true() -> bool {
    true
}

fn default_permission() -> String {
    "ask".to_string()
}
fn default_max_tool_calls() -> u32 {
    100
}

/// Provider presets: (name, base_url, default model) — the single source for
/// the first-run wizard and the `/config` panel's provider row.
pub const PROVIDERS: &[(&str, &str, &str)] = &[
    ("deepseek", "https://api.deepseek.com", "deepseek-v4-pro"),
    ("openai", "https://api.openai.com/v1", "gpt-4o-mini"),
    ("anthropic", "https://api.anthropic.com/v1", "claude-sonnet-4-20250514"),
    ("groq", "https://api.groq.com/openai/v1", "llama-3.3-70b-versatile"),
    ("openrouter", "https://openrouter.ai/api/v1", "openai/gpt-4o-mini"),
    ("google", "https://generativelanguage.googleapis.com/v1beta/openai", "gemini-2.5-flash"),
];

/// Env vars that may supply the API key for `provider`, highest priority
/// first — only the configured provider's key is ever picked up, so a machine
/// with several providers' keys exported can't hand the wrong one to this
/// one. PICODE_API_KEY is the explicit universal override and always wins.
fn key_env_vars(provider: &str) -> &'static [&'static str] {
    match provider {
        "deepseek" => &["PICODE_API_KEY", "DEEPSEEK_API_KEY"],
        "openai" => &["PICODE_API_KEY", "OPENAI_API_KEY"],
        "anthropic" => &["PICODE_API_KEY", "ANTHROPIC_API_KEY"],
        "groq" => &["PICODE_API_KEY", "GROQ_API_KEY"],
        "openrouter" => &["PICODE_API_KEY", "OPENROUTER_API_KEY"],
        "google" => &["PICODE_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY"],
        _ => &["PICODE_API_KEY"],
    }
}

/// One settings change from the `/config` panel. The worker applies it to its
/// live config and mirrors it to disk (each variant maps onto one field).
pub enum ConfigPatch {
    Provider { provider: String, base_url: String, model: String },
    BaseUrl(String),
    ApiKey(String),
    Thinking(bool),
    Permission(String),
    AutoCommit(bool),
    ContextWindow(u32),
    MaxToolCalls(u32),
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
            api_keys: BTreeMap::new(),
            theme: default_theme(),
            context_window: default_ctx(),
            price_in: default_price_in(),
            price_out: default_price_out(),
            mcp_servers: BTreeMap::new(),
            auto_commit: true,
            thinking: false,
            permission: default_permission(),
            max_tool_calls: default_max_tool_calls(),
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
/// Uses a deterministic FNV-1a hash so the same directory always gets the
/// same session file across invocations (DefaultHasher is randomly seeded).
pub fn session_path() -> PathBuf {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // FNV-1a 64-bit — deterministic, no dependency.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in cwd.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    sessions_dir().join(format!("{:016x}.json", h))
}

impl Config {
    /// Read config.json leniently: each known field is merged independently,
    /// so one malformed field (a typo in mcp_servers, say) can't reset the
    /// others. No env override — this is the on-disk truth, safe to rewrite.
    fn load_disk() -> Config {
        let mut cfg = Config::default();
        if let Ok(text) = std::fs::read_to_string(config_path()) {
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
                // New-style per-provider keys (api_keys object).
                if let Some(map) = v.get("api_keys").and_then(|x| x.as_object()) {
                    for (k, val) in map {
                        if let Some(s) = val.as_str() {
                            cfg.api_keys.insert(k.clone(), s.to_string());
                        }
                    }
                }
                // Legacy single api_key field — use as fallback for the current
                // provider if no per-provider entry exists.
                if let Some(s) = v.get("api_key").and_then(|x| x.as_str()) {
                    if !cfg.api_keys.contains_key(&cfg.provider) {
                        cfg.api_keys.insert(cfg.provider.clone(), s.to_string());
                    }
                }
                if let Some(s) = v.get("theme").and_then(|x| x.as_str()) {
                    let s = s.to_string();
                    // Validate; unknown theme names silently fall back to default
                    // so a hand-edited config can't cause a broken UI.
                    cfg.theme = if crate::ui::is_theme_name(&s) { s } else { default_theme() };
                }
                if let Some(n) = v.get("context_window").and_then(|x| x.as_u64()) {
                    cfg.context_window = n as u32;
                }
                if let Some(n) = v.get("price_in").and_then(|x| x.as_f64()) {
                    cfg.price_in = n;
                }
                if let Some(n) = v.get("price_out").and_then(|x| x.as_f64()) {
                    cfg.price_out = n;
                }
                if let Some(m) = v.get("mcp_servers") {
                    if let Ok(servers) = serde_json::from_value(m.clone()) {
                        cfg.mcp_servers = servers;
                    }
                }
                if let Some(b) = v.get("auto_commit").and_then(|x| x.as_bool()) {
                    cfg.auto_commit = b;
                }
                if let Some(b) = v.get("thinking").and_then(|x| x.as_bool()) {
                    cfg.thinking = b;
                }
                if let Some(s) = v.get("permission").and_then(|x| x.as_str()) {
                    cfg.permission = s.to_string();
                }
                if let Some(n) = v.get("max_tool_calls").and_then(|x| x.as_u64()) {
                    cfg.max_tool_calls = n as u32;
                }
            }
        }
        // Resolve the effective key from the per-provider map.
        cfg.resolve_key();
        cfg
    }

    pub fn load() -> Config {
        let mut cfg = Self::load_disk();
        // First match wins, so PICODE_API_KEY (listed first) overrides the
        // provider-specific var.
        for var in key_env_vars(&cfg.provider) {
            if let Ok(k) = std::env::var(var) {
                if !k.is_empty() {
                    cfg.api_key = k;
                    cfg.key_from_env = true;
                    break;
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
        // Update the per-provider map with the current effective key (unless env-sourced).
        if !self.key_from_env && !self.api_key.is_empty() {
            on_disk.api_keys.insert(self.provider.clone(), self.api_key.clone());
        }
        let mut json = serde_json::json!({
            "provider": on_disk.provider,
            "base_url": on_disk.base_url,
            "model": on_disk.model,
            "theme": on_disk.theme,
        });
        // Write per-provider keys (the canonical store). Also write the legacy
        // api_key field with the current provider's key for older picode readers.
        if !on_disk.api_keys.is_empty() {
            json["api_keys"] = serde_json::to_value(&on_disk.api_keys).unwrap_or_default();
            if let Some(k) = on_disk.api_keys.get(&on_disk.provider) {
                if !self.key_from_env {
                    json["api_key"] = serde_json::json!(k);
                }
            }
        }
        // Preserve user customizations across model/theme rewrites; only
        // non-default values are written, keeping the common file minimal.
        if !on_disk.mcp_servers.is_empty() {
            json["mcp_servers"] = serde_json::to_value(&on_disk.mcp_servers).unwrap_or_default();
        }
        if !on_disk.auto_commit {
            json["auto_commit"] = serde_json::json!(false);
        }
        if on_disk.context_window != default_ctx() {
            json["context_window"] = serde_json::json!(on_disk.context_window);
        }
        if on_disk.price_in != default_price_in() {
            json["price_in"] = serde_json::json!(on_disk.price_in);
        }
        if on_disk.price_out != default_price_out() {
            json["price_out"] = serde_json::json!(on_disk.price_out);
        }
        if on_disk.thinking {
            json["thinking"] = serde_json::json!(true);
        }
        if on_disk.permission != default_permission() {
            json["permission"] = serde_json::json!(on_disk.permission);
        }
        if on_disk.max_tool_calls != default_max_tool_calls() {
            json["max_tool_calls"] = serde_json::json!(on_disk.max_tool_calls);
        }
        let path = config_path();
        std::fs::write(&path, serde_json::to_string_pretty(&json)?)
            .context("write config")?;
        set_private(&path);
        Ok(())
    }

    /// Update only the model field on disk, preserving the rest of the file.
    /// Goes through the lenient loader — a strict parse here could fall back
    /// to defaults on any malformed field and wipe the saved key.
    pub fn persist_model(&self) {
        let mut disk = Config::load_disk();
        disk.model = self.model.clone();
        let _ = disk.save();
    }

    /// Persist only the theme name, preserving the rest of the file.
    pub fn persist_theme(theme: &str) {
        let mut disk = Config::load_disk();
        disk.theme = theme.to_string();
        let _ = disk.save();
    }

    /// Resolve the effective API key for the current provider from the per-provider
    /// map. Does NOT check env vars.
    pub fn resolve_key(&mut self) {
        self.api_key = self.api_keys.get(&self.provider).cloned().unwrap_or_default();
        self.key_from_env = false;
    }

    pub fn apply_patch(&mut self, p: &ConfigPatch) {
        match p {
            ConfigPatch::Provider { provider, base_url, model } => {
                self.provider = provider.clone();
                self.base_url = base_url.clone();
                self.model = model.clone();
                // Swap to the new provider's saved key (or empty).
                self.resolve_key();
            }
            ConfigPatch::BaseUrl(u) => self.base_url = u.clone(),
            ConfigPatch::ApiKey(k) => {
                self.api_key = k.clone();
                self.api_keys.insert(self.provider.clone(), k.clone());
                self.key_from_env = false;
            }
            ConfigPatch::Thinking(b) => self.thinking = *b,
            ConfigPatch::Permission(m) => self.permission = m.clone(),
            ConfigPatch::AutoCommit(b) => self.auto_commit = *b,
            ConfigPatch::ContextWindow(n) => self.context_window = (*n).max(1),
            ConfigPatch::MaxToolCalls(n) => self.max_tool_calls = *n,
        }
    }

    /// Mirror one patch to disk, preserving every other field.
    pub fn persist_patch(p: &ConfigPatch) {
        let mut disk = Config::load_disk();
        disk.apply_patch(p);
        let _ = disk.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_map_to_fields() {
        let mut c = Config::default();
        c.apply_patch(&ConfigPatch::Provider {
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
        });
        assert_eq!((c.provider.as_str(), c.model.as_str()), ("openai", "gpt-4o-mini"));
        c.apply_patch(&ConfigPatch::Thinking(true));
        c.apply_patch(&ConfigPatch::Permission("plan".into()));
        c.apply_patch(&ConfigPatch::AutoCommit(false));
        c.apply_patch(&ConfigPatch::ContextWindow(64000));
        c.apply_patch(&ConfigPatch::MaxToolCalls(42));
        assert!(c.thinking);
        assert_eq!(c.permission, "plan");
        assert!(!c.auto_commit);
        assert_eq!(c.context_window, 64000);
        assert_eq!(c.max_tool_calls, 42);
        // A key set through the panel always counts as a disk key.
        c.key_from_env = true;
        c.apply_patch(&ConfigPatch::ApiKey("sk-x".into()));
        assert!(!c.key_from_env);
        // Zero context window is clamped, not propagated.
        c.apply_patch(&ConfigPatch::ContextWindow(0));
        assert_eq!(c.context_window, 1);
    }

    #[test]
    fn env_keys_are_provider_scoped() {
        // Only the configured provider's var (plus the universal override)
        // can supply the key — never another provider's.
        for (name, _, _) in PROVIDERS {
            let vars = key_env_vars(name);
            assert_eq!(vars[0], "PICODE_API_KEY", "{name}: override must win");
            for v in &vars[1..] {
                let prov_upper = name.to_uppercase();
                assert!(
                    v.starts_with(&prov_upper) || (*name == "google" && v.starts_with("GEMINI")),
                    "{name} must not read {v}"
                );
            }
        }
        // Unknown/custom providers get only the explicit override.
        assert_eq!(key_env_vars("my-llm-box"), &["PICODE_API_KEY"]);
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
    println!("Provider:");
    for (i, (name, _, _)) in PROVIDERS.iter().enumerate() {
        let default = if i == 0 { " (default)" } else { "" };
        println!("  {}) {name}{default}", i + 1);
    }
    let custom_n = PROVIDERS.len() + 1;
    println!("  {custom_n}) custom");
    let choice = prompt("> [1] ")?;
    let n = choice.trim().parse::<usize>().unwrap_or(1);
    let custom = n == custom_n;
    let (prov, base, model): (String, String, String) = if custom {
        let p = prompt("provider name: ")?;
        let p = if p.trim().is_empty() { "custom".to_string() } else { p.trim().to_string() };
        let b = loop {
            let b = prompt("base_url (OpenAI-compatible): ")?;
            let b = b.trim().to_string();
            if b.starts_with("http") {
                break b;
            }
            println!("\x1b[31m  URL must start with http\x1b[0m");
        };
        let m = loop {
            let m = prompt("model: ")?;
            let m = m.trim().to_string();
            if !m.is_empty() {
                break m;
            }
            println!("\x1b[31m  model is required\x1b[0m");
        };
        (p, b, m)
    } else {
        let (p, b, m) = PROVIDERS[n.saturating_sub(1).min(PROVIDERS.len() - 1)];
        (p.into(), b.into(), m.into())
    };
    let model = if custom {
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
        cfg.api_keys.insert(cfg.provider.clone(), key);
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

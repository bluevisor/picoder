//! Permission rules — Claude Code `permissions.allow` / `permissions.deny`
//! style, plus Codex-like "don't ask again for this prefix" persistence.
//!
//! A rule is `tool` or `tool(pattern)`:
//!
//! ```text
//! bash(git *)          any git command
//! bash(cargo test:*)   `cargo test` and `cargo test …` (Claude Code prefix form)
//! edit_file(src/**)    edits under src/ (glob on the path)
//! write_file           any write
//! mcp__fs__*           every tool of the `fs` MCP server
//! web_fetch(*.github.com)
//! ```
//!
//! Claude Code's capitalised tool names (`Bash`, `Edit`, `Write`, `Read`,
//! `WebFetch(domain:…)`) are accepted as aliases so a `.claude/settings.json`
//! rule can be pasted verbatim.
//!
//! Rules are merged from, lowest to highest priority: `config.json`
//! (`permissions`), `.picoder/settings.json`, `.picoder/settings.local.json`.
//! `deny` always wins over `allow` and is enforced even in bypass mode; a
//! matching `allow` skips the approval prompt in ask mode. Compound bash
//! commands (`a && b | c`) are allowed only when every segment is allowed and
//! denied when any segment is denied.

use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub tool: String,
    pub pattern: Option<String>,
}

impl Rule {
    /// Parse `tool` / `tool(pattern)`; returns None for an empty string.
    pub fn parse(s: &str) -> Option<Rule> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let (tool, pattern) = match s.find('(') {
            Some(i) if s.ends_with(')') => (&s[..i], Some(s[i + 1..s.len() - 1].to_string())),
            _ => (s, None),
        };
        let tool = canonical_tool(tool.trim());
        // Claude Code: `WebFetch(domain:example.com)`.
        let pattern = pattern.map(|p| {
            let p = p.trim();
            p.strip_prefix("domain:")
                .map(|d| format!("*{d}*"))
                .unwrap_or_else(|| p.to_string())
        });
        Some(Rule {
            tool,
            pattern: pattern.filter(|p| !p.is_empty()),
        })
    }

    pub fn render(&self) -> String {
        match &self.pattern {
            Some(p) => format!("{}({p})", self.tool),
            None => self.tool.clone(),
        }
    }

    fn matches(&self, tool: &str, target: &str) -> bool {
        if !wild_match(&self.tool, tool) {
            return false;
        }
        let Some(p) = &self.pattern else { return true };
        if tool == "bash" {
            bash_pattern_matches(p, target)
        } else if is_path_tool(tool) {
            path_matches(p, target)
        } else {
            wild_match(p, target)
        }
    }
}

/// Map Claude Code / friendly names onto picoder's tool names.
pub fn canonical_tool(t: &str) -> String {
    match t.to_ascii_lowercase().as_str() {
        "bash" | "shell" | "sh" => "bash".into(),
        "edit" | "edit_file" => "edit_file".into(),
        "multiedit" | "multi_edit" => "multi_edit".into(),
        "write" | "write_file" => "write_file".into(),
        "read" | "read_file" => "read_file".into(),
        "list" | "ls" | "list_files" => "list_files".into(),
        "webfetch" | "web_fetch" => "web_fetch".into(),
        "websearch" | "web_search" => "web_search".into(),
        "grep" => "grep".into(),
        "glob" => "glob".into(),
        "task" | "agent" => "task".into(),
        _ => t.to_string(),
    }
}

fn is_path_tool(tool: &str) -> bool {
    matches!(
        tool,
        "edit_file" | "multi_edit" | "write_file" | "read_file" | "list_files" | "grep" | "glob"
    )
}

/// Shell-style wildcard match: `*` any run, `?` one char. Case-sensitive.
pub fn wild_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// `cargo test:*` → `cargo test` alone or followed by whitespace; otherwise a
/// plain wildcard match on the whole command. A pattern with no wildcard is
/// treated as a prefix so `bash(git status)` also covers `git status -s`.
fn bash_pattern_matches(pattern: &str, cmd: &str) -> bool {
    let cmd = cmd.trim();
    if let Some(prefix) = pattern.strip_suffix(":*") {
        let prefix = prefix.trim_end();
        return cmd == prefix
            || cmd
                .strip_prefix(prefix)
                .map(|rest| rest.starts_with(char::is_whitespace))
                .unwrap_or(false);
    }
    if !pattern.contains(['*', '?']) {
        return cmd == pattern
            || cmd
                .strip_prefix(pattern)
                .map(|rest| rest.starts_with(char::is_whitespace))
                .unwrap_or(false);
    }
    wild_match(pattern, cmd)
}

/// Glob a path pattern against the raw path, its `./`-stripped form, and its
/// cwd-relative form, so `src/**` matches whichever spelling the model used.
fn path_matches(pattern: &str, path: &str) -> bool {
    let pat = match glob::Pattern::new(pattern) {
        Ok(p) => p,
        Err(_) => return wild_match(pattern, path),
    };
    let opts = glob::MatchOptions {
        require_literal_separator: false,
        ..Default::default()
    };
    // Match the path the tool will actually touch, not just the spelling the
    // model used: `src/../src/secrets/k`, `./src/./secrets/k` and `~/…` all
    // collapse to the same file, and a deny rule must catch every spelling.
    let normalized = crate::tools::expand(path);
    let mut candidates = vec![path.to_string(), normalized.to_string_lossy().into_owned()];
    if let Some(s) = path.strip_prefix("./") {
        candidates.push(s.to_string());
    }
    if let Ok(cwd) = std::env::current_dir() {
        for p in [Path::new(path), normalized.as_path()] {
            if let Ok(rel) = p.strip_prefix(&cwd) {
                candidates.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    candidates.iter().any(|c| pat.matches_with(c, opts))
}

/// True when the command contains a construct that runs *another* command
/// hidden inside an argument — `$(…)`, backticks, `<(…)`/`>(…)`, or an
/// `eval`/`sh -c`-style word — outside quotes. Such commands can never be
/// auto-allowed by a prefix rule: `ls $(rm -rf ~)` is not an `ls`.
pub fn has_subshell(cmd: &str) -> bool {
    let chars: Vec<char> = cmd.chars().collect();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            // Inside double quotes $(…) and backticks still expand.
            Some('"') if c == '"' => quote = None,
            Some('"') if c == '$' && chars.get(i + 1) == Some(&'(') => return true,
            Some('"') if c == '`' => return true,
            Some('\'') if c == '\'' => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => i += 1,
                '`' => return true,
                '$' | '<' | '>' if chars.get(i + 1) == Some(&'(') => return true,
                _ => {}
            },
        }
        i += 1;
    }
    bash_segments(cmd).iter().any(|seg| {
        let first = seg.split_whitespace().next().unwrap_or("");
        let first = first.rsplit('/').next().unwrap_or(first);
        matches!(first, "eval" | "exec" | "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh" | "xargs" | "env" | "nohup" | "time" | "command" | "sudo" | "doas")
    })
}

/// Split a shell command on `&&`, `||`, `;`, `|`, `&` and newlines so every
/// segment can be checked. Quotes are respected so `echo "a && b"` stays one
/// segment. Segments are trimmed; empty ones are dropped.
pub fn bash_segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                } else if c == '\\' && i + 1 < chars.len() {
                    i += 1;
                    cur.push(chars[i]);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '\\' if i + 1 < chars.len() => {
                    cur.push(c);
                    i += 1;
                    cur.push(chars[i]);
                }
                '&' | '|' if i + 1 < chars.len() && chars[i + 1] == c => {
                    out.push(std::mem::take(&mut cur));
                    i += 1;
                }
                // `2>&1` / `>&2` are redirections, not a background `&`.
                '&' if i > 0 && chars[i - 1] == '>' => cur.push(c),
                ';' | '|' | '&' | '\n' => out.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            },
        }
        i += 1;
    }
    out.push(cur);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// A deny rule matched (the rule is named so the model/user can see why).
    Deny(String),
    /// An allow rule matched — skip the prompt.
    Allow(String),
    /// No rule: fall through to the permission mode.
    Ask,
}

#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub allow: Vec<Rule>,
    pub deny: Vec<Rule>,
}

impl Policy {
    /// Merge rules from every settings layer (see module docs).
    pub fn load() -> Policy {
        let mut p = Policy::default();
        for (_, v) in crate::config::settings_layers() {
            p.absorb(&v);
        }
        p
    }

    /// Read `permissions.allow` / `permissions.deny` (Claude Code shape) or
    /// top-level `allow` / `deny` arrays out of one settings document.
    pub fn absorb(&mut self, v: &Value) {
        let perms = v.get("permissions").unwrap_or(v);
        for (key, dst) in [("allow", &mut self.allow), ("deny", &mut self.deny)] {
            if let Some(arr) = perms.get(key).and_then(|a| a.as_array()) {
                for r in arr
                    .iter()
                    .filter_map(|x| x.as_str())
                    .filter_map(Rule::parse)
                {
                    if !dst.contains(&r) {
                        dst.push(r);
                    }
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }

    /// Decide for one tool call. `targets` are the things the rule pattern is
    /// matched against — one command / path / URL per entry (multi_edit passes
    /// every path). With no target only tool-level rules apply.
    pub fn check(&self, tool: &str, targets: &[&str]) -> Decision {
        let targets: Vec<String> = if targets.is_empty() {
            vec![String::new()]
        } else if tool == "bash" {
            targets.iter().flat_map(|t| bash_segments(t)).collect()
        } else {
            targets.iter().map(|s| s.to_string()).collect()
        };
        let targets: Vec<String> = if targets.is_empty() {
            vec![String::new()]
        } else {
            targets
        };
        for t in &targets {
            if let Some(r) = self.deny.iter().find(|r| r.matches(tool, t)) {
                return Decision::Deny(r.render());
            }
        }
        // A hidden inner command defeats prefix matching — never auto-allow.
        if tool == "bash" && targets.iter().any(|t| has_subshell(t)) {
            return Decision::Ask;
        }
        let mut hit: Option<&Rule> = None;
        for t in &targets {
            match self.allow.iter().find(|r| r.matches(tool, t)) {
                Some(r) => hit = Some(r),
                None => return Decision::Ask,
            }
        }
        match hit {
            Some(r) => Decision::Allow(r.render()),
            None => Decision::Ask,
        }
    }

    /// Human-readable listing for `/permissions`.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.is_empty() {
            out.push(
                "no permission rules (add with /permissions allow <rule> or deny <rule>).".into(),
            );
            return out;
        }
        if !self.allow.is_empty() {
            out.push("allow:".into());
            out.extend(self.allow.iter().map(|r| format!("  {}", r.render())));
        }
        if !self.deny.is_empty() {
            out.push("deny:".into());
            out.extend(self.deny.iter().map(|r| format!("  {}", r.render())));
        }
        out
    }
}

/// Suggest the rule to offer as "don't ask again" for a call about to be
/// approved. Bash gets a sub-command prefix (`cargo test:*`, `git status:*`,
/// or a bare `ls:*`); file tools get their top-level directory (`src/**`) or
/// the file itself; anything else gets the bare tool name.
pub fn suggest_rule(tool: &str, target: &str) -> String {
    match tool {
        "bash" => {
            let segs = bash_segments(target);
            let first = segs.first().map(String::as_str).unwrap_or(target);
            let words: Vec<&str> = first.split_whitespace().collect();
            let prefix = match words.as_slice() {
                [] => return "bash".into(),
                [a, b, ..] if has_subcommands(a) && !b.starts_with('-') => format!("{a} {b}"),
                [a, ..] => a.to_string(),
            };
            format!("bash({prefix}:*)")
        }
        "edit_file" | "write_file" | "multi_edit" | "read_file" => {
            let p = target.trim_start_matches("./");
            match p.split_once('/') {
                Some((top, _)) if !top.is_empty() && !p.starts_with('/') => {
                    format!("{tool}({top}/**)")
                }
                _ => format!("{tool}({p})"),
            }
        }
        _ => tool.to_string(),
    }
}

fn has_subcommands(cmd: &str) -> bool {
    matches!(
        cmd,
        "git"
            | "cargo"
            | "npm"
            | "pnpm"
            | "yarn"
            | "bun"
            | "make"
            | "docker"
            | "kubectl"
            | "gh"
            | "go"
            | "pip"
            | "pip3"
            | "python"
            | "python3"
            | "uv"
            | "poetry"
            | "systemctl"
            | "apt"
            | "apt-get"
            | "brew"
            | "rustup"
            | "flutter"
            | "dart"
            | "mix"
            | "gradle"
    )
}

/// Where a "don't ask again" rule is persisted: the project's
/// `.picoder/settings.local.json` when the working directory is a git repo or
/// already has a `.picoder/` folder, else the user's config.json.
pub fn persist_target() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if cwd.join(".picoder").is_dir() || crate::tools::in_git_repo(&cwd) {
        cwd.join(".picoder").join("settings.local.json")
    } else {
        crate::config::config_path()
    }
}

/// Append `rule` to `permissions.<kind>` in the settings file at `path`,
/// creating the file if needed and leaving every other key untouched.
pub fn persist_rule(path: &Path, kind: &str, rule: &str) -> std::io::Result<()> {
    // A file that exists but doesn't parse is the user's problem to fix — not
    // ours to overwrite with a one-rule document (their hooks would be gone).
    let mut doc: Value = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{} is not valid JSON: {e}", path.display()))
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(e) => return Err(e),
    };
    if !doc.is_object() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a JSON object", path.display()),
        ));
    }
    let perms = doc
        .as_object_mut()
        .unwrap()
        .entry("permissions")
        .or_insert_with(|| Value::Object(Default::default()));
    if !perms.is_object() {
        *perms = Value::Object(Default::default());
    }
    let arr = perms
        .as_object_mut()
        .unwrap()
        .entry(kind)
        .or_insert_with(|| Value::Array(Vec::new()));
    if !arr.is_array() {
        *arr = Value::Array(Vec::new());
    }
    let list = arr.as_array_mut().unwrap();
    if !list.iter().any(|v| v.as_str() == Some(rule)) {
        list.push(Value::String(rule.to_string()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::config::atomic_write(path, serde_json::to_string_pretty(&doc)?.as_bytes())
}

/// Remove `rule` from every layer that has it. Returns how many files changed.
pub fn remove_rule(rule: &str) -> usize {
    let mut n = 0;
    for (path, mut doc) in crate::config::settings_layers() {
        let mut changed = false;
        if let Some(perms) = doc.get_mut("permissions").and_then(|p| p.as_object_mut()) {
            for kind in ["allow", "deny"] {
                if let Some(arr) = perms.get_mut(kind).and_then(|a| a.as_array_mut()) {
                    let before = arr.len();
                    arr.retain(|v| v.as_str() != Some(rule));
                    changed |= arr.len() != before;
                }
            }
        }
        if changed {
            if let Ok(s) = serde_json::to_string_pretty(&doc) {
                if crate::config::atomic_write(&path, s.as_bytes()).is_ok() {
                    n += 1;
                }
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allow: &[&str], deny: &[&str]) -> Policy {
        Policy {
            allow: allow.iter().filter_map(|s| Rule::parse(s)).collect(),
            deny: deny.iter().filter_map(|s| Rule::parse(s)).collect(),
        }
    }

    #[test]
    fn parses_tool_and_pattern_forms() {
        assert_eq!(
            Rule::parse("bash"),
            Some(Rule {
                tool: "bash".into(),
                pattern: None
            })
        );
        assert_eq!(
            Rule::parse("Bash(git *)"),
            Some(Rule {
                tool: "bash".into(),
                pattern: Some("git *".into())
            })
        );
        assert_eq!(Rule::parse("Edit(src/**)").unwrap().tool, "edit_file");
        assert_eq!(
            Rule::parse("WebFetch(domain:github.com)")
                .unwrap()
                .pattern
                .as_deref(),
            Some("*github.com*")
        );
        assert_eq!(Rule::parse("   "), None);
    }

    #[test]
    fn bash_prefix_rules_cover_subcommands_but_not_lookalikes() {
        let p = policy(&["bash(cargo test:*)", "bash(git status)"], &[]);
        assert!(matches!(
            p.check("bash", &["cargo test"]),
            Decision::Allow(_)
        ));
        assert!(matches!(
            p.check("bash", &["cargo test --release"]),
            Decision::Allow(_)
        ));
        assert_eq!(p.check("bash", &["cargo testx"]), Decision::Ask);
        assert!(matches!(
            p.check("bash", &["git status -s"]),
            Decision::Allow(_)
        ));
        assert_eq!(p.check("bash", &["git stash"]), Decision::Ask);
    }

    #[test]
    fn compound_commands_need_every_segment_allowed_and_any_denied_segment_denies() {
        let p = policy(&["bash(git *)", "bash(ls:*)"], &["bash(rm *)"]);
        assert!(matches!(
            p.check("bash", &["git status && ls -la"]),
            Decision::Allow(_)
        ));
        assert_eq!(
            p.check("bash", &["git status && cargo build"]),
            Decision::Ask
        );
        assert!(matches!(
            p.check("bash", &["ls; rm -rf /"]),
            Decision::Deny(_)
        ));
        // Quoted operators don't split.
        assert!(matches!(
            p.check("bash", &["git commit -m \"a && b\""]),
            Decision::Allow(_)
        ));
    }

    #[test]
    fn background_ampersand_and_subshells_cannot_ride_an_allow_rule() {
        let p = policy(&["bash(ls:*)", "bash(echo:*)"], &["bash(rm *)"]);
        // `&` is a separator like `&&`: the second segment is checked on its own.
        assert!(matches!(p.check("bash", &["ls & rm -rf ~"]), Decision::Deny(_)));
        assert_eq!(p.check("bash", &["ls & cargo build"]), Decision::Ask);
        // Redirections are not separators.
        assert!(matches!(p.check("bash", &["ls 2>&1"]), Decision::Allow(_)));
        // Hidden inner commands never auto-allow, even when the prefix matches.
        for cmd in ["ls $(rm -rf ~)", "echo `whoami`", "echo \"$(cat /etc/passwd)\"", "ls <(id)", "sh -c 'ls'", "sudo ls", "xargs ls"] {
            assert_eq!(p.check("bash", &[cmd]), Decision::Ask, "{cmd}");
        }
        // ...but a literal in single quotes is just text.
        assert!(matches!(p.check("bash", &["echo '$(not run)'"]), Decision::Allow(_)));
        // Deny still wins inside a subshell command.
        assert!(matches!(p.check("bash", &["rm -rf $(pwd)"]), Decision::Deny(_)));
    }

    #[test]
    fn persist_rule_refuses_to_clobber_a_broken_settings_file() {
        let dir = std::env::temp_dir().join(format!("picoder-policy-broken-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.local.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(persist_rule(&path, "allow", "bash(ls:*)").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deny_beats_allow_and_path_globs_match_relative_spellings() {
        let p = policy(&["edit_file(src/**)"], &["edit_file(src/secrets/**)"]);
        assert!(matches!(
            p.check("edit_file", &["src/main.rs"]),
            Decision::Allow(_)
        ));
        assert!(matches!(
            p.check("edit_file", &["./src/ui/types.rs"]),
            Decision::Allow(_)
        ));
        assert!(matches!(
            p.check("edit_file", &["src/secrets/key.pem"]),
            Decision::Deny(_)
        ));
        // Every spelling of the same file is caught.
        for spelling in ["src/../src/secrets/key.pem", "./src/./secrets/key.pem", "src//secrets/key.pem"] {
            assert!(matches!(p.check("edit_file", &[spelling]), Decision::Deny(_)), "{spelling}");
        }
        let abs = std::env::current_dir().unwrap().join("src/secrets/key.pem");
        assert!(matches!(p.check("edit_file", &[abs.to_str().unwrap()]), Decision::Deny(_)));
        assert_eq!(p.check("edit_file", &["README.md"]), Decision::Ask);
        // multi_edit: every path must be allowed.
        let p = policy(&["multi_edit(src/**)"], &[]);
        assert!(matches!(
            p.check("multi_edit", &["src/a.rs", "src/b.rs"]),
            Decision::Allow(_)
        ));
        assert_eq!(
            p.check("multi_edit", &["src/a.rs", "Cargo.toml"]),
            Decision::Ask
        );
    }

    #[test]
    fn tool_level_and_mcp_wildcard_rules() {
        let p = policy(&["read_file", "mcp__fs__*"], &["mcp__fs__delete"]);
        assert!(matches!(
            p.check("read_file", &["anything"]),
            Decision::Allow(_)
        ));
        assert!(matches!(p.check("mcp__fs__read", &[]), Decision::Allow(_)));
        assert!(matches!(p.check("mcp__fs__delete", &[]), Decision::Deny(_)));
        assert_eq!(p.check("mcp__other__x", &[]), Decision::Ask);
    }

    #[test]
    fn suggested_rules_are_sensible() {
        assert_eq!(
            suggest_rule("bash", "cargo test --all"),
            "bash(cargo test:*)"
        );
        assert_eq!(suggest_rule("bash", "git -C . status"), "bash(git:*)");
        assert_eq!(suggest_rule("bash", "ls -la && pwd"), "bash(ls:*)");
        assert_eq!(
            suggest_rule("edit_file", "src/ui/types.rs"),
            "edit_file(src/**)"
        );
        assert_eq!(
            suggest_rule("write_file", "./README.md"),
            "write_file(README.md)"
        );
        assert_eq!(suggest_rule("mcp__fs__write", ""), "mcp__fs__write");
    }

    #[test]
    fn persist_rule_appends_without_clobbering_other_keys() {
        let dir = std::env::temp_dir().join(format!("picoder-policy-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("settings.local.json");
        std::fs::write(
            &path,
            r#"{"hooks":{"Stop":[]},"permissions":{"deny":["bash(rm *)"]}}"#,
        )
        .unwrap();
        persist_rule(&path, "allow", "bash(git:*)").unwrap();
        persist_rule(&path, "allow", "bash(git:*)").unwrap(); // idempotent
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            v["permissions"]["allow"],
            serde_json::json!(["bash(git:*)"])
        );
        assert_eq!(v["permissions"]["deny"], serde_json::json!(["bash(rm *)"]));
        assert!(v["hooks"].is_object());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absorb_reads_both_claude_and_flat_shapes() {
        let mut p = Policy::default();
        p.absorb(
            &serde_json::json!({"permissions": {"allow": ["Bash(ls:*)"], "deny": ["Bash(rm:*)"]}}),
        );
        p.absorb(&serde_json::json!({"allow": ["read_file"]}));
        assert_eq!(p.allow.len(), 2);
        assert_eq!(p.deny.len(), 1);
    }
}

//! Tool implementations. Pure-ish: each returns a String result for the model.
//! Approval and event emission live in the agent; preview/diff helpers here let
//! the agent show a change before it is applied.

use crate::api::{truncate, MAX_TOOL_OUTPUT};
use crate::config::{config_dir, memory_path};
use anyhow::Result;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn expand(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if path == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

// ----------------------------------------------------------------- bash -----

pub fn bash(command: &str, timeout: u64, cwd: &Path) -> String {
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => return format!("ERROR: failed to spawn shell: {e}"),
    };
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(Duration::from_secs(timeout.max(1))) {
        Ok(Ok(output)) => {
            let mut out = String::new();
            out.push_str(&String::from_utf8_lossy(&output.stdout));
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("[stderr]\n");
                out.push_str(&stderr);
            }
            let code = output.status.code().unwrap_or(-1);
            out = out.trim_end().to_string();
            out.push_str(&format!("\n[exit {code}]"));
            truncate(out.trim(), MAX_TOOL_OUTPUT)
        }
        Ok(Err(e)) => format!("ERROR: {e}"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = Command::new("kill").arg("-KILL").arg(pid.to_string()).status();
            format!("ERROR: command timed out after {timeout}s")
        }
        Err(e) => format!("ERROR: {e}"),
    }
}

// ------------------------------------------------------------ read/list -----

pub fn read_file(path: &str, start: Option<u64>, end: Option<u64>) -> String {
    let p = expand(path);
    let content = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => return format!("ERROR: {e}"),
    };
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let (slice, off) = match (start, end) {
        (None, None) => (&lines[..], 1usize),
        (s, e) => {
            let s = s.unwrap_or(1).max(1) as usize;
            let e = e.map(|x| x as usize).unwrap_or(lines.len());
            let lo = (s - 1).min(lines.len());
            let hi = e.min(lines.len()).max(lo);
            (&lines[lo..hi], s)
        }
    };
    if slice.is_empty() {
        return "(empty file)".into();
    }
    let mut out = String::new();
    for (i, ln) in slice.iter().enumerate() {
        out.push_str(&format!("{:>5}  {}", off + i, ln));
        if !ln.ends_with('\n') {
            out.push('\n');
        }
    }
    truncate(&out, MAX_TOOL_OUTPUT)
}

pub fn list_files(path: &str) -> String {
    let p = expand(if path.is_empty() { "." } else { path });
    let entries = match std::fs::read_dir(&p) {
        Ok(e) => e,
        Err(e) => return format!("ERROR: {e}"),
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        names.push(if is_dir { format!("{name}/") } else { name });
    }
    names.sort();
    if names.is_empty() {
        "(empty)".into()
    } else {
        truncate(&names.join("\n"), MAX_TOOL_OUTPUT)
    }
}

// --------------------------------------------------------- write / edit -----

/// Compute a preview diff for an upcoming write (existing content vs new).
pub fn write_preview(path: &str, content: &str) -> (String, bool) {
    let p = expand(path);
    let existed = p.exists();
    let old = std::fs::read_to_string(&p).unwrap_or_default();
    (crate::diff::unified(&old, content, 300), existed)
}

pub fn write_file(path: &str, content: &str) -> String {
    let p = expand(path);
    if let Some(dir) = p.parent() {
        if !dir.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                return format!("ERROR: {e}");
            }
        }
    }
    match std::fs::write(&p, content) {
        Ok(()) => format!("OK wrote {} ({} bytes)", p.display(), content.len()),
        Err(e) => format!("ERROR: {e}"),
    }
}

pub enum EditPreview {
    Ok { diff: String, new_content: String },
    Err(String),
}

pub fn edit_preview(path: &str, old_text: &str, new_text: &str) -> EditPreview {
    let p = expand(path);
    let data = match std::fs::read_to_string(&p) {
        Ok(d) => d,
        Err(e) => return EditPreview::Err(format!("ERROR: {e}")),
    };
    let n = data.matches(old_text).count();
    if n == 0 {
        return EditPreview::Err("ERROR: old_text not found.".into());
    }
    if n > 1 {
        return EditPreview::Err(format!("ERROR: old_text matches {n} times; make it unique."));
    }
    let new_content = data.replacen(old_text, new_text, 1);
    let diff = crate::diff::unified(&data, &new_content, 300);
    EditPreview::Ok { diff, new_content }
}

pub fn apply_write(path: &str, content: &str) -> String {
    let p = expand(path);
    match std::fs::write(&p, content) {
        Ok(()) => format!("OK edited {}", p.display()),
        Err(e) => format!("ERROR: {e}"),
    }
}

// ----------------------------------------------------------- grep/glob ------

const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__", "dist", "build"];

pub fn grep(pattern: &str, path: &str, ignore_case: bool) -> String {
    let re = match regex::RegexBuilder::new(pattern).case_insensitive(ignore_case).build() {
        Ok(r) => r,
        Err(e) => return format!("ERROR: bad regex: {e}"),
    };
    let root = expand(if path.is_empty() { "." } else { path });
    let mut files: Vec<PathBuf> = Vec::new();
    if root.is_file() {
        files.push(root);
    } else {
        collect_files(&root, &mut files, 0);
    }
    let mut out = String::new();
    let mut hits = 0usize;
    'outer: for f in files {
        let Ok(fh) = std::fs::File::open(&f) else { continue };
        let mut buf = Vec::new();
        if fh.take(1_000_000).read_to_end(&mut buf).is_err() {
            continue;
        }
        if buf.iter().take(1024).any(|&b| b == 0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&buf);
        let rel = f.display().to_string();
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                out.push_str(&format!("{}:{}: {}\n", rel, i + 1, line.trim_end()));
                hits += 1;
                if hits >= 200 {
                    out.push_str("... (200+ matches, stopping)\n");
                    break 'outer;
                }
            }
        }
    }
    if out.is_empty() {
        "(no matches)".into()
    } else {
        truncate(&out, MAX_TOOL_OUTPUT)
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 12 || out.len() > 5000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') && name.len() > 1 {
                continue;
            }
            collect_files(&path, out, depth + 1);
        } else {
            out.push(path);
        }
    }
}

pub fn glob_search(pattern: &str) -> String {
    let mut matches: Vec<String> = Vec::new();
    match glob::glob(pattern) {
        Ok(paths) => {
            for entry in paths.flatten() {
                matches.push(entry.display().to_string());
                if matches.len() >= 500 {
                    break;
                }
            }
        }
        Err(e) => return format!("ERROR: bad glob: {e}"),
    }
    matches.sort();
    if matches.is_empty() {
        "(no files match)".into()
    } else {
        truncate(&matches.join("\n"), MAX_TOOL_OUTPUT)
    }
}

// ----------------------------------------------------------- web_fetch ------

/// Fetch a URL and return readable text. HTML is stripped to text; anything
/// else (JSON, plain text, …) comes back as-is. Body capped at 2MB.
pub fn web_fetch(http: &ureq::Agent, url: &str) -> String {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return "ERROR: url must start with http:// or https://".into();
    }
    let resp = http
        .get(url)
        .timeout(Duration::from_secs(30))
        .set("Accept", "text/html, application/json, text/plain, */*")
        .call();
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            return format!("ERROR: HTTP {code}: {}", truncate(&body, 500));
        }
        Err(e) => return format!("ERROR: {e}"),
    };
    let html = resp
        .content_type()
        .to_ascii_lowercase()
        .contains("text/html");
    let mut body = String::new();
    if let Err(e) = resp.into_reader().take(2_000_000).read_to_string(&mut body) {
        return format!("ERROR: read failed (binary content?): {e}");
    }
    let text = if html { html_to_text(&body) } else { body };
    let text = text.trim();
    if text.is_empty() {
        "(empty response)".into()
    } else {
        truncate(text, MAX_TOOL_OUTPUT)
    }
}

/// Best-effort HTML → text: drop script/style/head, turn block-level closes
/// into newlines, strip the remaining tags, decode common entities.
fn html_to_text(html: &str) -> String {
    let strip_block = |s: &str, tag: &str| -> String {
        let re = regex::RegexBuilder::new(&format!(r"<{tag}\b.*?</{tag}>"))
            .case_insensitive(true)
            .dot_matches_new_line(true)
            .build()
            .unwrap();
        re.replace_all(s, " ").into_owned()
    };
    let mut s = html.to_string();
    for tag in ["script", "style", "head", "noscript", "svg"] {
        s = strip_block(&s, tag);
    }
    let breaks = regex::RegexBuilder::new(r"</(p|div|li|tr|h[1-6]|blockquote|pre)>|<br\s*/?>")
        .case_insensitive(true)
        .build()
        .unwrap();
    let s = breaks.replace_all(&s, "\n");
    let tags = regex::Regex::new(r"<[^>]*>").unwrap();
    let s = tags.replace_all(&s, " ");
    let s = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    // Collapse runs of spaces and blank lines left behind by stripped markup.
    let mut out = String::new();
    let mut blank_run = 0;
    for line in s.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

// ------------------------------------------------------------- memory -------

pub fn remember(note: &str) -> String {
    if let Err(e) = std::fs::create_dir_all(config_dir()) {
        return format!("ERROR: {e}");
    }
    use std::io::Write;
    let res = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(memory_path())
        .and_then(|mut f| writeln!(f, "- {note}"));
    match res {
        Ok(()) => format!("OK remembered: {note}"),
        Err(e) => format!("ERROR: {e}"),
    }
}

pub fn recall(query: Option<&str>) -> String {
    let content = match std::fs::read_to_string(memory_path()) {
        Ok(c) => c,
        Err(_) => return "(no memories yet)".into(),
    };
    let content = content.trim();
    if content.is_empty() {
        return "(no memories yet)".into();
    }
    match query {
        Some(q) if !q.is_empty() => {
            let ql = q.to_lowercase();
            let filtered: Vec<&str> =
                content.lines().filter(|l| l.to_lowercase().contains(&ql)).collect();
            if filtered.is_empty() {
                format!("(no memories matching '{q}')")
            } else {
                filtered.join("\n")
            }
        }
        _ => content.to_string(),
    }
}

pub fn load_memory() -> Result<Option<String>> {
    match std::fs::read_to_string(memory_path()) {
        Ok(c) if !c.trim().is_empty() => Ok(Some(c.trim().to_string())),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_markup() {
        let html = "<html><head><title>T</title><style>x{color:red}</style></head>\
                    <body><script>var a=1;</script><h1>Hello</h1>\
                    <p>World &amp; friends</p><ul><li>one</li><li>two</li></ul></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World & friends"));
        assert!(text.contains("one"));
        assert!(!text.contains("var a"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn web_fetch_rejects_non_http() {
        let http = crate::api::agent_http();
        assert!(web_fetch(&http, "file:///etc/passwd").starts_with("ERROR"));
        assert!(web_fetch(&http, "ftp://x").starts_with("ERROR"));
    }

    #[test]
    #[ignore] // hits the network; run with `cargo test -- --ignored`
    fn web_fetch_real_page() {
        let http = crate::api::agent_http();
        let text = web_fetch(&http, "https://example.com");
        assert!(text.contains("Example Domain"), "got: {text}");
        assert!(!text.contains('<'));
    }
}

//! Tool implementations. Pure-ish: each returns a String result for the model.
//! Approval and event emission live in the agent; preview/diff helpers here let
//! the agent show a change before it is applied.

use crate::api::{truncate, MAX_TOOL_OUTPUT};
use crate::config::{config_dir, memory_path};
use anyhow::Result;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
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
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group so we can kill the whole tree on timeout.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = match cmd.spawn() {
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
            // Negative pid = kill whole process group (sh + grandchildren).
            let _ = Command::new("kill").arg("-KILL").arg(format!("-{pid}")).status();
            // Give the waiter thread a moment to reap the killed child.
            let _ = rx.recv_timeout(Duration::from_millis(500));
            format!("ERROR: command timed out after {timeout}s")
        }
        Err(e) => format!("ERROR: {e}"),
    }
}

// -------------------------------------------------------- background jobs ---

/// A shell command running detached from the agent loop. Output accumulates
/// in `buf`; `read_to` tracks how much bash_output has already returned.
struct Job {
    pid: u32,
    command: String,
    buf: Arc<Mutex<String>>,
    exit: Arc<Mutex<Option<i32>>>,
    read_to: usize,
}

const JOB_BUF_MAX: usize = 1_000_000;

fn jobs() -> &'static Mutex<HashMap<u32, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<u32, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

const MAX_BG_JOBS: usize = 64;

pub fn bash_background(command: &str, cwd: &Path) -> String {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group, so bash_kill can take out the whole pipeline.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("ERROR: failed to spawn shell: {e}"),
    };
    let pid = child.id();
    let buf = Arc::new(Mutex::new(String::new()));
    let exit = Arc::new(Mutex::new(None));
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as _),
        child.stderr.take().map(|p| Box::new(p) as _),
    ];
    for pipe in pipes.into_iter().flatten() {
        let buf = buf.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(pipe);
            let mut line = String::new();
            while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                let mut b = buf.lock().unwrap();
                if b.len() < JOB_BUF_MAX {
                    b.push_str(&line);
                }
                line.clear();
            }
        });
    }
    {
        let exit = exit.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            *exit.lock().unwrap() = Some(code);
        });
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut map = jobs().lock().unwrap();
    // Evict finished jobs when the table gets large, to avoid unbounded memory.
    if map.len() >= MAX_BG_JOBS {
        map.retain(|_, j| j.exit.lock().unwrap().is_none());
    }
    map.insert(
        id,
        Job { pid, command: command.to_string(), buf, exit, read_to: 0 },
    );
    drop(map);
    format!("Started background job {id} (pid {pid}). Poll with bash_output, stop with bash_kill.")
}

/// Output produced since the last bash_output call, plus run status.
pub fn bash_output(id: u64) -> String {
    let mut map = jobs().lock().unwrap();
    let Some(job) = map.get_mut(&(id as u32)) else {
        return format!("ERROR: no background job {id}");
    };
    let buf = job.buf.lock().unwrap();
    let new = buf[job.read_to..].to_string();
    job.read_to = buf.len();
    drop(buf);
    let status = match *job.exit.lock().unwrap() {
        Some(code) => format!("[exited {code}]"),
        None => "[running]".to_string(),
    };
    let header = format!("job {id}: {}", truncate(&job.command, 100));
    let body = if new.is_empty() { "(no new output)".to_string() } else { truncate(&new, MAX_TOOL_OUTPUT) };
    format!("{header}\n{body}\n{status}")
}

pub fn bash_kill(id: u64) -> String {
    let map = jobs().lock().unwrap();
    let Some(job) = map.get(&(id as u32)) else {
        return format!("ERROR: no background job {id}");
    };
    if job.exit.lock().unwrap().is_some() {
        return format!("job {id} has already exited");
    }
    let pid = job.pid;
    drop(map);
    // Negative pid = the whole process group (sh + its children).
    let _ = Command::new("sh").arg("-c").arg(format!("kill -KILL -{pid}")).status();
    format!("killed job {id} (process group {pid})")
}

// ------------------------------------------------------------ read/list -----

pub fn read_file(path: &str, start: Option<u64>, end: Option<u64>) -> String {
    let p = expand(path);
    // Open and read only up to READ_FILE_LIMIT bytes to avoid OOM on huge files.
    const READ_FILE_LIMIT: usize = 1_000_000; // 1 MB
    let content = match std::fs::File::open(&p) {
        Ok(f) => {
            let mut buf = String::new();
            match std::io::BufReader::new(f)
                .take(READ_FILE_LIMIT as u64)
                .read_to_string(&mut buf)
            {
                Ok(_) => buf,
                Err(e) => return format!("ERROR: read failed: {e}"),
            }
        }
        Err(e) => return format!("ERROR: {e}"),
    };
    let truncated_read = content.len() >= READ_FILE_LIMIT;
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
    if truncated_read {
        out.push_str("... (file truncated at 1 MB)\n");
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

/// One requested edit (a unique-substring replacement in a file).
pub struct EditReq {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
}

/// A validated multi-file edit, ready to apply.
pub struct MultiEditPlan {
    /// Combined diff across all touched files, for the approval preview.
    pub diff: String,
    /// Final content per file, in first-touched order.
    pub files: Vec<(String, String)>,
}

/// Validate a batch of edits without writing. Edits are applied to in-memory
/// copies in order, so several edits to the same file compose correctly. Any
/// failure (file missing, old_text absent or ambiguous) aborts the whole
/// batch — nothing is half-applied.
pub fn multi_edit_plan(edits: &[EditReq]) -> std::result::Result<MultiEditPlan, String> {
    if edits.is_empty() {
        return Err("ERROR: no edits provided".into());
    }
    let mut originals: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut current: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (i, e) in edits.iter().enumerate() {
        if !current.contains_key(&e.path) {
            let disk = std::fs::read_to_string(expand(&e.path))
                .map_err(|err| format!("ERROR: edit {}: {} ({err})", i + 1, e.path))?;
            originals.insert(e.path.clone(), disk.clone());
            current.insert(e.path.clone(), disk);
            order.push(e.path.clone());
        }
        let cur = current.get(&e.path).unwrap();
        let n = cur.matches(&e.old_text).count();
        if n == 0 {
            return Err(format!("ERROR: edit {}: old_text not found in {}", i + 1, e.path));
        }
        if n > 1 {
            return Err(format!(
                "ERROR: edit {}: old_text matches {n} times in {}; make it unique",
                i + 1,
                e.path
            ));
        }
        let updated = cur.replacen(&e.old_text, &e.new_text, 1);
        current.insert(e.path.clone(), updated);
    }
    let mut diff = String::new();
    let mut files = Vec::new();
    for path in &order {
        let old = &originals[path];
        let new = &current[path];
        diff.push_str(&format!("--- {path}\n"));
        diff.push_str(&crate::diff::unified(old, new, 300));
        diff.push('\n');
        files.push((path.clone(), new.clone()));
    }
    Ok(MultiEditPlan { diff, files })
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

// --------------------------------------------------------------- git --------

/// True if `dir` is inside a git work tree.
pub fn in_git_repo(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Bump the patch version in `dir/Cargo.toml`. Returns the new version string,
/// or None if there's no Cargo.toml or the version can't be bumped.
pub fn bump_cargo_version(dir: &Path) -> Option<String> {
    let path = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).ok()?;
    // Match `version = "x.y.z"` and bump z.
    let re = regex::Regex::new(r#"^(\s*version\s*=\s*")(\d+)\.(\d+)\.(\d+)(")"#).ok()?;
    let mut bumped = String::new();
    let mut found = false;
    for line in text.lines() {
        if !found {
            if let Some(caps) = re.captures(line) {
                let z: u64 = caps[4].parse().ok()?;
                let new_version = format!("{}.{}.{}", &caps[2], &caps[3], z + 1);
                bumped.push_str(&format!("{}{}\"{}\n", &caps[1], new_version, &caps[5]));
                found = true;
                continue;
            }
        }
        bumped.push_str(line);
        bumped.push('\n');
    }
    if !found {
        return None;
    }
    std::fs::write(&path, &bumped).ok()?;
    // Return the new version as e.g. "0.2.3".
    let caps = re.captures(&bumped)?;
    Some(format!("{}.{}.{}", &caps[2], &caps[3], &caps[4]))
}

/// Commit the given paths to the repo at `dir` as an edit checkpoint. Also bumps
/// the patch version in Cargo.toml (if present) and includes it in the commit.
/// Returns a short note like ` [committed a1b2c3d]` for the tool result, or ""
/// when there was nothing to commit / not a repo. Best-effort: never surfaces an
/// error.
pub fn git_autocommit(dir: &Path, paths: &[String], message: &str) -> String {
    if paths.is_empty() || !in_git_repo(dir) {
        return String::new();
    }
    // Tools expand `~` before touching the filesystem; git does not, so the
    // model's raw path strings must be expanded the same way here.
    let mut paths: Vec<PathBuf> = paths.iter().map(|p| expand(p)).collect();

    // Bump Cargo.toml version if present.
    let cargo_toml = dir.join("Cargo.toml");
    let version_bumped = if cargo_toml.exists() {
        bump_cargo_version(dir)
    } else {
        None
    };
    if version_bumped.is_some() {
        paths.push(cargo_toml.clone());
    }

    let mut add = Command::new("git");
    add.arg("-C").arg(dir).arg("add").arg("--");
    for p in &paths {
        add.arg(p);
    }
    if add.stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| !s.success()).unwrap_or(true) {
        return String::new();
    }
    // A fresh Pi may have no user.name/email; retry with a fallback identity
    // only if the first attempt fails (so configured identity is preserved).
    let run_commit = |ident: bool| {
        let mut c = Command::new("git");
        c.arg("-C").arg(dir);
        if ident {
            c.args(["-c", "user.name=picode", "-c", "user.email=picode@localhost"]);
        }
        c.args(["commit", "--no-verify", "-m", message, "--"]);
        for p in &paths {
            c.arg(p);
        }
        c.stdout(Stdio::null()).stderr(Stdio::null()).status()
    };
    let committed = run_commit(false).map(|s| s.success()).unwrap_or(false)
        || run_commit(true).map(|s| s.success()).unwrap_or(false);
    if !committed {
        return String::new(); // nothing changed, or commit refused
    }
    let hash = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let mut note = if hash.is_empty() {
        " [committed]".to_string()
    } else {
        format!(" [committed {hash}]")
    };
    if let Some(v) = version_bumped {
        note.push_str(&format!(" version → {v}"));
    }
    note
}

/// Recent git history + working-tree status for `dir`, as a startup context
/// clue. None outside a repo.
pub fn git_context(dir: &Path) -> Option<String> {
    if !in_git_repo(dir) {
        return None;
    }
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let log = run(&["log", "--oneline", "-15"]);
    let status = run(&["status", "--short"]);
    let mut out = String::new();
    if !branch.is_empty() {
        out.push_str(&format!("branch: {branch}\n"));
    }
    if !log.is_empty() {
        out.push_str(&format!("recent commits:\n{log}\n"));
    }
    if !status.is_empty() {
        out.push_str(&format!("uncommitted changes:\n{}\n", truncate(&status, 2000)));
    }
    (!out.trim().is_empty()).then(|| out.trim().to_string())
}

/// Branch + clean/dirty state for the banner, e.g. `main · 3 changed` or
/// `main · clean`. `sep` joins them (mode-aware). None outside a repo.
pub fn git_status_line(dir: &Path, sep: &str) -> Option<String> {
    if !in_git_repo(dir) {
        return None;
    }
    let branch = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())?;
    let changed = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0);
    let state = if changed == 0 {
        "clean".to_string()
    } else {
        format!("{changed} changed")
    };
    Some(format!("{branch}{sep}{state}"))
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

// ------------------------------------------------------------- images -------

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
const IMAGE_MAX_BYTES: usize = 5_000_000;

pub fn is_image_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Read an image file and return a `data:image/...;base64,...` URI.
pub fn image_data_uri(path: &str) -> std::result::Result<String, String> {
    let p = expand(path);
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !IMAGE_EXTS.contains(&ext.as_str()) {
        return Err(format!("ERROR: {path} is not a supported image (png/jpg/gif/webp)"));
    }
    let bytes = std::fs::read(&p).map_err(|e| format!("ERROR: {e}"))?;
    if bytes.len() > IMAGE_MAX_BYTES {
        return Err(format!("ERROR: {path} is too large ({} bytes; max 5MB)", bytes.len()));
    }
    let mime = if ext == "jpg" { "jpeg".to_string() } else { ext };
    Ok(format!("data:image/{mime};base64,{}", base64_encode(&bytes)))
}

/// Standard base64 (no line wrapping). Kept tiny so we add no dependency.
pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
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
    let s = decode_entities(&tags.replace_all(&s, " "));
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

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
}

// ----------------------------------------------------------- web_search -----

/// Search DuckDuckGo's HTML endpoint and return "title / url / snippet" rows.
pub fn web_search(http: &ureq::Agent, query: &str) -> String {
    if query.trim().is_empty() {
        return "ERROR: empty query".into();
    }
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
    let resp = http
        .get(&url)
        .timeout(Duration::from_secs(30))
        .set("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) picode/0.1")
        .call();
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return format!("ERROR: search returned HTTP {code}"),
        Err(e) => return format!("ERROR: {e}"),
    };
    let mut body = String::new();
    if let Err(e) = resp.into_reader().take(2_000_000).read_to_string(&mut body) {
        return format!("ERROR: read failed: {e}");
    }
    parse_ddg(&body)
}

fn parse_ddg(html: &str) -> String {
    let re = |p: &str| {
        regex::RegexBuilder::new(p)
            .case_insensitive(true)
            .dot_matches_new_line(true)
            .build()
            .unwrap()
    };
    // Match DDG result links — look for uddg= redirect URLs (the stable
    // structural element) and extract the anchor text as title.  This is
    // more robust than depending on specific class names.
    let link_re = re(r#"<a[^>]*href="[^"]*uddg=[^"]*"[^>]*>(.*?)</a>"#);
    // For snippets, try any element whose class contains "snippet".
    let snip_re = re(r#"class="[^"]*snippet[^"]*"[^>]*>(.*?)</(?:a|td|div|span)>"#);
    let snippets: Vec<String> = snip_re
        .captures_iter(html)
        .map(|c| clean_inline(&c[1]))
        .collect();
    let mut out = String::new();
    for (i, c) in link_re.captures_iter(html).take(8).enumerate() {
        let title = clean_inline(&c[1]);
        // Extract uddg= value from the full match.
        let full = &c[0];
        let href_start = full.find("uddg=").map(|p| p + 5).unwrap_or(0);
        let href_rest = &full[href_start..];
        let href_end = href_rest.find('"').unwrap_or(href_rest.len());
        let encoded = &href_rest[..href_end];
        out.push_str(&format!("{}. {title}\n   {}\n", i + 1, urldecode(encoded)));
        if let Some(s) = snippets.get(i) {
            if !s.is_empty() {
                out.push_str(&format!("   {s}\n"));
            }
        }
    }
    if out.is_empty() {
        "(no results — DuckDuckGo may have changed its output format)".into()
    } else {
        truncate(&out, MAX_TOOL_OUTPUT)
    }
}

/// DDG result hrefs are redirects like `//duckduckgo.com/l/?uddg=<encoded>&…`;
/// pull out and decode the real destination.
fn resolve_ddg_url(href: &str) -> String {
    if let Some(pos) = href.find("uddg=") {
        let rest = &href[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        return urldecode(&rest[..end]);
    }
    if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.to_string()
    }
}

/// Strip tags and entities from an inline HTML fragment, collapsing whitespace.
fn clean_inline(s: &str) -> String {
    let tags = regex::Regex::new(r"<[^>]*>").unwrap();
    decode_entities(&tags.replace_all(s, ""))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// --------------------------------------------------------------- todo -------

/// Format the model's plan as a checklist. The returned string is both the
/// tool result (so the model sees the current plan) and the transcript view.
pub fn todo(items: &serde_json::Value) -> String {
    let Some(arr) = items.as_array() else {
        return "ERROR: items must be an array of {text, status}".into();
    };
    if arr.is_empty() {
        return "(plan cleared)".into();
    }
    let mut out = String::new();
    for it in arr {
        let text = it.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let mark = match it.get("status").and_then(|v| v.as_str()).unwrap_or("pending") {
            "completed" => "[x]",
            "in_progress" => "[>]",
            _ => "[ ]",
        };
        out.push_str(&format!("{mark} {text}\n"));
    }
    out.trim_end().to_string()
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
    fn url_encode_decode_roundtrip() {
        let s = "rust ureq & \"tools\" ~/.config";
        assert_eq!(urldecode(&urlencode(s)), s);
        assert_eq!(urlencode("a b"), "a+b");
        assert_eq!(urldecode("https%3A%2F%2Fexample.com%2Fx"), "https://example.com/x");
    }

    #[test]
    fn ddg_redirect_resolution() {
        assert_eq!(
            resolve_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&rut=abc"),
            "https://example.com/docs"
        );
        assert_eq!(resolve_ddg_url("https://direct.example.com"), "https://direct.example.com");
    }

    #[test]
    fn ddg_parse_fixture() {
        let html = r#"<div class="result"><h2 class="result__title">
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F&rut=x">Example <b>Domain</b></a></h2>
            <a class="result__snippet" href="x">This domain is for use in &amp;c.</a></div>"#;
        let out = parse_ddg(html);
        assert!(out.contains("1. Example Domain"), "got: {out}");
        assert!(out.contains("https://example.com/"));
        assert!(out.contains("This domain is for use in &c."));
    }

    #[test]
    fn todo_formats_checklist() {
        let items = serde_json::json!([
            {"text": "explore", "status": "completed"},
            {"text": "edit", "status": "in_progress"},
            {"text": "test"},
        ]);
        assert_eq!(todo(&items), "[x] explore\n[>] edit\n[ ] test");
        assert!(todo(&serde_json::json!("nope")).starts_with("ERROR"));
    }

    #[test]
    fn multi_edit_composes_same_file() {
        let dir = std::env::temp_dir().join("picode_multi_edit_test");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, "one two three\n").unwrap();
        std::fs::write(&b, "hello world\n").unwrap();
        let edits = vec![
            EditReq { path: a.to_string_lossy().into(), old_text: "one".into(), new_text: "1".into() },
            // Second edit to the same file sees the result of the first.
            EditReq { path: a.to_string_lossy().into(), old_text: "1 two".into(), new_text: "1 2".into() },
            EditReq { path: b.to_string_lossy().into(), old_text: "world".into(), new_text: "there".into() },
        ];
        let plan = multi_edit_plan(&edits).expect("plan ok");
        assert_eq!(plan.files.len(), 2);
        let a_final = &plan.files.iter().find(|(p, _)| p.contains("a.txt")).unwrap().1;
        assert_eq!(a_final, "1 2 three\n");
        // Apply and confirm disk matches.
        for (p, c) in &plan.files {
            apply_write(p, c);
        }
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1 2 three\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "hello there\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn multi_edit_aborts_on_missing_match() {
        let dir = std::env::temp_dir().join("picode_multi_edit_abort");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, "keep me\n").unwrap();
        let edits = vec![
            EditReq { path: a.to_string_lossy().into(), old_text: "keep".into(), new_text: "KEEP".into() },
            EditReq { path: a.to_string_lossy().into(), old_text: "absent".into(), new_text: "x".into() },
        ];
        assert!(multi_edit_plan(&edits).is_err());
        // Nothing applied — file unchanged.
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "keep me\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_autocommit_checkpoints_edit() {
        let dir = std::env::temp_dir().join("picode_git_test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            return; // no git available; skip
        }
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        assert!(in_git_repo(&dir));
        let f = dir.join("x.txt");
        std::fs::write(&f, "hi\n").unwrap();
        let note = git_autocommit(&dir, &[f.to_string_lossy().into()], "picode: write x.txt");
        assert!(note.contains("committed"), "got: {note:?}");
        // A second call with no change commits nothing.
        let note2 = git_autocommit(&dir, &[f.to_string_lossy().into()], "picode: noop");
        assert!(note2.is_empty(), "got: {note2:?}");
        let log = git(&["log", "--oneline"]);
        assert!(String::from_utf8_lossy(&log.stdout).contains("write x.txt"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // A non-UTF8 byte sequence (real image data isn't text).
        assert_eq!(base64_encode(&[0xff, 0xd8, 0xff]), "/9j/");
    }

    #[test]
    fn image_path_detection_and_uri() {
        assert!(is_image_path("shot.PNG"));
        assert!(is_image_path("a/b/diagram.jpeg"));
        assert!(!is_image_path("notes.txt"));
        assert!(!is_image_path("Makefile"));
        // jpg maps to the image/jpeg mime.
        let dir = std::env::temp_dir().join("picode_img_test.jpg");
        std::fs::write(&dir, [0xff, 0xd8, 0xff, 0xe0]).unwrap();
        let uri = image_data_uri(dir.to_str().unwrap()).unwrap();
        assert!(uri.starts_with("data:image/jpeg;base64,/9j/"), "got: {uri}");
        std::fs::remove_file(&dir).ok();
        assert!(image_data_uri("nope.png").unwrap_err().starts_with("ERROR"));
    }

    #[test]
    fn background_job_lifecycle() {
        let cwd = std::env::temp_dir();
        let start = bash_background("echo hello; sleep 30", &cwd);
        assert!(start.contains("Started background job"), "got: {start}");
        let id: u64 = start
            .split_whitespace()
            .nth(3)
            .and_then(|s| s.parse().ok())
            .expect("job id in start message");
        std::thread::sleep(Duration::from_millis(300));
        let out = bash_output(id);
        assert!(out.contains("hello"), "got: {out}");
        assert!(out.contains("[running]"), "got: {out}");
        // Second poll returns only new output.
        let out2 = bash_output(id);
        assert!(out2.contains("(no new output)"), "got: {out2}");
        let killed = bash_kill(id);
        assert!(killed.contains("killed job"), "got: {killed}");
        std::thread::sleep(Duration::from_millis(300));
        assert!(bash_output(id).contains("[exited"), "job should be dead");
        assert!(bash_output(999).starts_with("ERROR"));
    }

    #[test]
    #[ignore] // hits the network; run with `cargo test -- --ignored`
    fn web_search_real() {
        let http = crate::api::agent_http();
        let out = web_search(&http, "rust programming language");
        assert!(out.contains("rust-lang.org"), "got: {out}");
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

//! Bash tool: synchronous and background shell commands.

use crate::api::{truncate, MAX_TOOL_OUTPUT};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

fn expand(path: &str) -> PathBuf {
    let raw = if let Some(rest) = path.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => PathBuf::from(home).join(rest),
            Err(_) => PathBuf::from(path),
        }
    } else if path == "~" {
        match std::env::var("HOME") {
            Ok(home) => PathBuf::from(home),
            Err(_) => PathBuf::from(path),
        }
    } else {
        PathBuf::from(path)
    };
    normalize(&raw)
}

/// Collapse `.` and `..` lexically (without touching the filesystem, so it works
/// for paths that don't exist yet). This doesn't sandbox — a coding agent
/// legitimately reads files all over the box — but it turns a sneaky
/// `~/../../etc/x` into the plain `/etc/x` it really means, so the path the user
/// approves is the path that's used.
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => match out.components().next_back() {
                // Pop a real directory name…
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // …but never climb above the filesystem root.
                Some(Component::RootDir) => {}
                // At a relative start, keep the `..` (can't resolve it lexically).
                _ => out.push(".."),
            },
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Refuse to operate through a symlink: a symlinked path could point outside the
/// intended target. Reads and writes both go through this so the guard is
/// symmetric. Returns `Some(error)` to short-circuit, `None` to proceed.
fn deny_symlink(p: &Path, path: &str, verb: &str) -> Option<String> {
    match std::fs::symlink_metadata(p) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Some(format!("DENIED: {path} is a symlink; {verb} the real path instead."))
        }
        _ => None,
    }
}

// ----------------------------------------------------------------- bash -----

pub fn bash(command: &str, timeout: u64, cwd: &Path, cancel: &AtomicBool) -> String {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group so we can kill the whole tree on timeout or interrupt.
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
    // Poll on a short interval so an Esc (cancel) interrupts a long-running
    // command instead of blocking the worker until the timeout elapses.
    let deadline = Instant::now() + Duration::from_secs(timeout.max(1));
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
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
                return truncate(out.trim(), MAX_TOOL_OUTPUT);
            }
            Ok(Err(e)) => return format!("ERROR: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Negative pid = kill whole process group (sh + grandchildren).
                // Routed through `sh -c` to match bash_kill (a bare
                // `Command::new("kill")` mis-targets the group in some sandboxes).
                let kill = |pid: u32| {
                    let _ = Command::new("sh").arg("-c").arg(format!("kill -KILL -{pid}")).status();
                };
                if cancel.load(Ordering::Relaxed) {
                    kill(pid);
                    let _ = rx.recv_timeout(Duration::from_millis(500));
                    return "ERROR: interrupted by user".to_string();
                }
                if Instant::now() >= deadline {
                    kill(pid);
                    // Give the waiter thread a moment to reap the killed child.
                    let _ = rx.recv_timeout(Duration::from_millis(500));
                    return format!("ERROR: command timed out after {timeout}s");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return "ERROR: shell waiter disconnected".to_string();
            }
        }
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
    if id > u32::MAX as u64 {
        return format!("ERROR: invalid job id {id} (max {})", u32::MAX);
    }
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
    if id > u32::MAX as u64 {
        return format!("ERROR: invalid job id {id} (max {})", u32::MAX);
    }
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

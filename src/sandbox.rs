//! OS sandbox for the shell commands picoder runs (`bash`, background jobs,
//! `!cmd`). Off by default. Settings, from config.json or the project's
//! `.picoder/settings(.local).json` (the most specific layer wins):
//!
//! - `"sandbox"`: `"off"` | `"workspace"` (writes only under the working
//!   directory, its repo root, the temp dirs, picoder's config dir and
//!   `sandbox_allow_write`) | `"readonly"` (writes only to temp dirs).
//! - `"sandbox_network"`: `false` blocks network access (default `true`).
//! - `"sandbox_allow_write"`: extra writable paths, e.g. `["~/.cargo"]`.
//!
//! macOS wraps the shell in `sandbox-exec` with a generated profile. Linux
//! applies Landlock in the child before exec; a kernel without it runs the
//! command unsandboxed and says so once. Reads are never restricted: an
//! agent legitimately reads all over the box.

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(not(target_os = "macos"))]
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Workspace,
    ReadOnly,
}

#[derive(Debug)]
pub struct Sandbox {
    pub mode: Mode,
    pub network: bool,
    /// Canonical directories (or files) writes are allowed under.
    pub writable: Vec<PathBuf>,
}

/// What a shell spawn needs to keep alive until the child has started.
pub struct Guard {
    #[cfg(target_os = "linux")]
    _ruleset: Option<std::os::fd::OwnedFd>,
}

impl Sandbox {
    /// The sandbox the settings ask for in `cwd`, or None when it's off. A
    /// repo's own `.picoder/` settings can tighten the sandbox but never loosen
    /// it: the strictest mode and network setting across layers win, and only
    /// the user's config.json may add `sandbox_allow_write` paths (a cloned
    /// repo could otherwise make `~` writable or switch the sandbox off).
    pub fn current(cwd: &Path) -> Option<Sandbox> {
        Sandbox::from_layers(&crate::config::settings_layers(), &crate::config::config_path(), cwd)
    }

    fn from_layers(layers: &[(PathBuf, serde_json::Value)], user_config: &Path, cwd: &Path) -> Option<Sandbox> {
        let strictness = |v: &serde_json::Value| match v.get("sandbox").and_then(|x| x.as_str()) {
            Some("readonly") | Some("read-only") => 2,
            Some("workspace") => 1,
            _ => 0,
        };
        let mode = match layers.iter().map(|(_, v)| strictness(v)).max().unwrap_or(0) {
            2 => Mode::ReadOnly,
            1 => Mode::Workspace,
            _ => return None,
        };
        let network = layers
            .iter()
            .all(|(_, v)| v.get("sandbox_network").and_then(|x| x.as_bool()).unwrap_or(true));
        let extra: Vec<String> = layers
            .iter()
            .filter(|(path, _)| path == user_config)
            .filter_map(|(_, v)| v.get("sandbox_allow_write").and_then(|x| x.as_array()).cloned())
            .flatten()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        Some(Sandbox::new(mode, network, cwd, &extra))
    }

    pub fn new(mode: Mode, network: bool, cwd: &Path, extra: &[String]) -> Sandbox {
        let mut dirs: Vec<PathBuf> = temp_dirs();
        if mode == Mode::Workspace {
            dirs.push(cwd.to_path_buf());
            if let Some(root) = repo_root(cwd) {
                dirs.push(root);
            }
            dirs.push(crate::config::config_dir());
            dirs.extend(extra.iter().map(|p| crate::tools::expand(p)));
        }
        let mut writable: Vec<PathBuf> = dirs.iter().filter_map(|d| std::fs::canonicalize(d).ok()).collect();
        writable.sort();
        writable.dedup();
        Sandbox { mode, network, writable }
    }

    /// `sh -c command`, confined. On Linux without Landlock the command is
    /// returned unconfined with a one-time warning in the second slot.
    pub fn shell(&self, command: &str) -> (Command, Guard, Option<String>) {
        #[cfg(target_os = "macos")]
        {
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(self.macos_profile()).arg("sh").arg("-c").arg(command);
            (c, Guard {}, None)
        }
        #[cfg(target_os = "linux")]
        {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            match landlock::ruleset(&self.writable, self.network) {
                Ok(fd) => {
                    landlock::restrict_child(&mut c, &fd);
                    (c, Guard { _ruleset: Some(fd) }, None)
                }
                Err(why) => (c, Guard { _ruleset: None }, warn_once(&why)),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            (c, Guard {}, warn_once("no sandbox on this OS"))
        }
    }

    /// The `sandbox-exec` (SBPL) profile: everything allowed but file writes
    /// outside `writable` (and the device files every shell needs), plus IP
    /// networking when `network` is off.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn macos_profile(&self) -> String {
        let q = |p: &Path| p.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
        let mut s = String::from(
            "(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\n  \
             (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\")\n  \
             (literal \"/dev/dtracehelper\") (regex #\"^/dev/ttys[0-9]+$\") (regex #\"^/dev/fd/\")",
        );
        for d in &self.writable {
            s.push_str(&format!("\n  (subpath \"{}\")", q(d)));
        }
        s.push_str(")\n");
        if !self.network {
            s.push_str("(deny network-outbound (remote ip))\n(deny network-bind (local ip))\n");
        }
        s
    }

    /// One line for the model on what the sandbox allows.
    pub fn describe(&self) -> String {
        let dirs: Vec<String> = self.writable.iter().map(|d| d.display().to_string()).collect();
        let mode = match self.mode {
            Mode::Workspace => "workspace",
            Mode::ReadOnly => "readonly",
        };
        let net = if self.network { "network allowed" } else { "network blocked" };
        format!("sandbox {mode}: writes only under {}; {net}", dirs.join(", "))
    }

    /// When a failed command's output looks like the sandbox stopped it, a note
    /// saying so — a bare "Operation not permitted" sends a model in circles.
    pub fn explain(&self, output: &str, exit: i32) -> Option<String> {
        if exit == 0 {
            return None;
        }
        let lower = output.to_ascii_lowercase();
        let write = ["operation not permitted", "permission denied", "read-only file system"]
            .iter()
            .any(|s| lower.contains(s));
        let net = !self.network
            && ["couldn't connect", "could not connect", "failed to connect", "network is unreachable", "connection refused"]
                .iter()
                .any(|s| lower.contains(s));
        if !write && !net {
            return None;
        }
        Some(format!(
            "[blocked by sandbox? {}. If the command needs more, ask the user to add the path to \
             \"sandbox_allow_write\", allow the network (\"sandbox_network\": true) or set \"sandbox\": \"off\" \
             — don't try to work around it.]",
            self.describe()
        ))
    }
}

/// The temp dirs every toolchain writes to.
fn temp_dirs() -> Vec<PathBuf> {
    let mut v = vec![std::env::temp_dir(), PathBuf::from("/tmp")];
    if cfg!(target_os = "linux") {
        v.push(PathBuf::from("/var/tmp"));
        v.push(PathBuf::from("/dev/shm"));
    }
    v
}

/// The nearest ancestor of `dir` (itself included) holding a `.git`.
fn repo_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// A warning the first time the sandbox can't be applied, then silence.
#[cfg(not(target_os = "macos"))]
fn warn_once(why: &str) -> Option<String> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    (!WARNED.swap(true, Ordering::Relaxed))
        .then(|| format!("[sandbox unavailable ({why}); running this and later commands unsandboxed]"))
}

#[cfg(target_os = "linux")]
mod landlock {
    //! Landlock through raw syscalls (no crate): the ruleset is built in the
    //! parent; the child only calls prctl + landlock_restrict_self, both
    //! async-signal-safe, between fork and exec.
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;

    // The same numbers on every architecture (added after the syscall table
    // was unified).
    const SYS_CREATE_RULESET: libc::c_long = 444;
    const SYS_ADD_RULE: libc::c_long = 445;
    const SYS_RESTRICT_SELF: libc::c_long = 446;
    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: u32 = 1;

    const WRITE_FILE: u64 = 1 << 1;
    const REMOVE_DIR: u64 = 1 << 4;
    const REMOVE_FILE: u64 = 1 << 5;
    const MAKE_ALL: u64 = 0b111_1111 << 6; // MAKE_CHAR..MAKE_SYM
    const REFER: u64 = 1 << 13; // ABI 2
    const TRUNCATE: u64 = 1 << 14; // ABI 3
    const NET_BIND_TCP: u64 = 1 << 0; // ABI 4
    const NET_CONNECT_TCP: u64 = 1 << 1;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// The kernel's Landlock ABI version, or an error saying why there's none.
    fn abi() -> Result<i64, String> {
        // syscall returns a c_long: 32 bits on the Pi Zero.
        let v = unsafe { libc::syscall(SYS_CREATE_RULESET, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) } as i64;
        if v < 1 {
            let e = std::io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::ENOSYS) => "kernel has no Landlock".into(),
                Some(libc::EOPNOTSUPP) => "Landlock is disabled at boot".into(),
                _ => format!("Landlock unavailable: {e}"),
            });
        }
        Ok(v)
    }

    /// A ruleset that handles every write right (reads and exec stay free)
    /// and grants them all under `writable`, plus TCP when `network` is off
    /// and the kernel can enforce it.
    pub fn ruleset(writable: &[PathBuf], network: bool) -> Result<OwnedFd, String> {
        let abi = abi()?;
        let mut fs = WRITE_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_ALL;
        if abi >= 2 {
            fs |= REFER;
        }
        if abi >= 3 {
            fs |= TRUNCATE;
        }
        let net = if !network && abi >= 4 { NET_BIND_TCP | NET_CONNECT_TCP } else { 0 };
        let attr = RulesetAttr { handled_access_fs: fs, handled_access_net: net };
        let size = if abi >= 4 { std::mem::size_of::<RulesetAttr>() } else { 8 };
        let fd = unsafe { libc::syscall(SYS_CREATE_RULESET, &attr as *const RulesetAttr, size, 0u32) };
        if fd < 0 {
            return Err(format!("landlock_create_ruleset: {}", std::io::Error::last_os_error()));
        }
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        // Device files a shell writes to, beside the writable directories.
        let devices = ["/dev/null", "/dev/zero", "/dev/tty", "/dev/pts", "/dev/full"].map(PathBuf::from);
        for p in writable.iter().chain(devices.iter()) {
            let Ok(target) = std::fs::File::options().read(true).custom_flags(libc::O_PATH | libc::O_CLOEXEC).open(p)
            else {
                continue;
            };
            // A rule on a file may only grant rights that apply to files.
            let file_rights = fs & (WRITE_FILE | TRUNCATE);
            let allowed = if p.is_dir() { fs } else { file_rights };
            let rule = PathBeneathAttr { allowed_access: allowed, parent_fd: target.as_raw_fd() };
            let r = unsafe {
                libc::syscall(SYS_ADD_RULE, ruleset.as_raw_fd(), RULE_PATH_BENEATH, &rule as *const PathBeneathAttr, 0u32)
            };
            if r != 0 {
                return Err(format!("landlock_add_rule({}): {}", p.display(), std::io::Error::last_os_error()));
            }
        }
        Ok(ruleset)
    }

    /// Confine the child to `ruleset` right before it execs.
    pub fn restrict_child(cmd: &mut Command, ruleset: &OwnedFd) {
        let fd = ruleset.as_raw_fd();
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(SYS_RESTRICT_SELF, fd, 0u32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_profile_allows_the_workspace_and_temp_only() {
        let dir = std::env::temp_dir().join(format!("picoder-sbx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sb = Sandbox::new(Mode::Workspace, false, &dir, &[]);
        let canon = std::fs::canonicalize(&dir).unwrap();
        assert!(sb.writable.contains(&canon));
        let p = sb.macos_profile();
        assert!(p.contains("(deny file-write*)") && p.contains("(literal \"/dev/null\")"));
        assert!(p.contains(&format!("(subpath \"{}\")", canon.display())));
        assert!(p.contains("(deny network-outbound (remote ip))"), "network off");
        let ro = Sandbox::new(Mode::ReadOnly, true, &dir, &["~/.cargo".into()]);
        assert!(!ro.writable.contains(&canon), "readonly: not even the workspace");
        assert!(!ro.macos_profile().contains("network-outbound"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_repo_can_tighten_the_sandbox_but_never_loosen_it() {
        use serde_json::json;
        let user = PathBuf::from("/home/u/.config/picoder/config.json");
        let repo = PathBuf::from("/repo/.picoder/settings.json");
        let cwd = std::env::temp_dir();
        let on = |l: &[(PathBuf, serde_json::Value)]| Sandbox::from_layers(l, &user, &cwd);
        // The user's workspace sandbox survives a repo asking for "off", for
        // the network, or for extra writable paths.
        let s = on(&[
            (user.clone(), json!({"sandbox": "workspace", "sandbox_network": false})),
            (repo.clone(), json!({"sandbox": "off", "sandbox_network": true, "sandbox_allow_write": ["/"]})),
        ])
        .unwrap();
        assert_eq!(s.mode, Mode::Workspace);
        assert!(!s.network);
        assert!(!s.writable.contains(&PathBuf::from("/")), "repo can't add writable paths");
        // A repo may turn it on, or make it stricter.
        assert_eq!(on(&[(repo.clone(), json!({"sandbox": "readonly"}))]).unwrap().mode, Mode::ReadOnly);
        assert!(on(&[(user.clone(), json!({}))]).is_none(), "off by default");
        // Only the user's own config adds writable paths.
        let tmp = std::env::temp_dir().join(format!("picoder-sbx-extra-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let s = on(&[(user.clone(), json!({"sandbox": "workspace", "sandbox_allow_write": [tmp.display().to_string()]}))]).unwrap();
        assert!(s.writable.contains(&std::fs::canonicalize(&tmp).unwrap()));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sandbox_failures_are_explained() {
        let sb = Sandbox { mode: Mode::Workspace, network: false, writable: vec![PathBuf::from("/w")] };
        let note = sb.explain("sh: /etc/x: Operation not permitted\n[exit 1]", 1).unwrap();
        assert!(note.contains("blocked by sandbox") && note.contains("/w"), "{note}");
        assert!(sb.explain("curl: (7) Failed to connect to example.com", 7).is_some());
        assert!(sb.explain("all good", 0).is_none());
        assert!(sb.explain("error: tests failed", 1).is_none());
    }

    /// On Linux: with Landlock, a write outside the workspace is refused; on a
    /// kernel without it (the Jetson's), the command still runs, with a warning.
    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_confines_writes_or_degrades_with_a_warning() {
        let base = std::env::temp_dir().join(format!("picoder-ll-{}", std::process::id()));
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let home = std::env::var("HOME").map(PathBuf::from).unwrap();
        let outside = home.join(format!(".picoder-ll-probe-{}", std::process::id()));
        let sb = Sandbox::new(Mode::Workspace, true, &work, &[]);
        let (mut cmd, _g, warn) =
            sb.shell(&format!("echo in > inside.txt && echo ok; echo out > '{}'", outside.display()));
        let out = cmd.current_dir(&work).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(text.contains("ok") && work.join("inside.txt").exists(), "{text}");
        match warn {
            Some(w) => {
                assert!(w.contains("unsandboxed"), "{w}");
                eprintln!("degraded as designed: {w}");
            }
            None => assert!(!outside.exists(), "Landlock should block the outside write: {text}"),
        }
        let _ = std::fs::remove_file(&outside);
        std::fs::remove_dir_all(&base).ok();
    }

    /// The real thing on macOS: a write inside the workspace works, one outside
    /// is refused, and the command still runs.
    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_exec_blocks_writes_outside_the_workspace() {
        let base = std::env::temp_dir().join(format!("picoder-sbx-live-{}", std::process::id()));
        let work = base.join("work");
        let outside = std::env::var("HOME").map(PathBuf::from).unwrap().join(format!(".picoder-sbx-probe-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let sb = Sandbox::new(Mode::Workspace, true, &work, &[]);
        let (mut cmd, _g, warn) =
            sb.shell(&format!("echo in > inside.txt && echo ok; echo out > '{}'", outside.display()));
        assert!(warn.is_none());
        let out = cmd.current_dir(&work).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(text.contains("ok"), "{text}");
        assert!(work.join("inside.txt").exists(), "write inside the workspace");
        assert!(!outside.exists(), "write outside was blocked: {text}");
        assert!(text.contains("Operation not permitted"), "{text}");
        let _ = std::fs::remove_file(&outside);
        std::fs::remove_dir_all(&base).ok();
    }
}

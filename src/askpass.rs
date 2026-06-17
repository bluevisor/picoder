//! sudo password support for the agent's bash tool.
//!
//! The agent runs bash commands with no usable terminal for input — the TUI owns
//! the tty in raw mode — so `sudo` can't prompt for a password the normal way
//! (on the framebuffer console it can't reach a tty at all). Instead we register
//! an askpass helper, which is picoder itself re-invoked as
//! `picoder --askpass <socket>`, plus a `sudo` shim on PATH that forces
//! `sudo -A`. When sudo needs a password it runs the helper; the helper connects
//! back to the running picoder over a private Unix socket; picoder pops a masked
//! in-TUI prompt and returns what the user types.
//!
//! The password is only ever held in memory and handed straight to the local
//! sudo process. It is never logged, echoed to the transcript, written to the
//! session/history file, or sent over the network.

use crate::agent::UiEvent;
use crate::config::config_dir;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};

/// `picoder --askpass <sock> [prompt...]`: the helper sudo executes. It connects
/// to the running picoder, hands over the prompt sudo gave us, reads the password
/// back, and prints it on stdout for sudo. Returns the process exit code —
/// non-zero on any failure or user cancel, so sudo aborts rather than trying a
/// blank password.
pub fn run_helper(sock: &str, prompt: &str) -> i32 {
    let stream = match UnixStream::connect(sock) {
        Ok(s) => s,
        Err(_) => return 1,
    };
    // Forward sudo's prompt as a single line (collapse any newlines defensively).
    let one_line = prompt.replace(['\n', '\r'], " ");
    if writeln!(&stream, "{one_line}").is_err() {
        return 1;
    }
    let mut reader = BufReader::new(&stream);
    let mut pw = String::new();
    if reader.read_line(&mut pw).is_err() {
        return 1;
    }
    let pw = pw.strip_suffix('\n').unwrap_or(&pw);
    if pw.is_empty() {
        return 1; // cancelled (or empty) — don't attempt a blank password
    }
    println!("{pw}");
    0
}

/// Install the askpass helper + sudo shim and start the listener. Sets
/// `SUDO_ASKPASS` and prepends the shim dir to `PATH` in this process's
/// environment so the agent's bash children inherit both. No-op if sudo isn't
/// installed or any setup step fails. Must be called on the only live thread
/// (before the worker is spawned), since it mutates the process environment.
pub fn setup(ui: Sender<UiEvent>) {
    let Some(sudo) = find_sudo() else { return }; // no sudo here — nothing to do
    let Ok(exe) = std::env::current_exe() else { return };

    let dir = config_dir().join("run");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));

    let sock = dir.join("askpass.sock");
    let _ = std::fs::remove_file(&sock); // clear a stale socket from a prior run
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(_) => return,
    };
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));

    // askpass helper: re-exec picoder in --askpass mode, forwarding sudo's prompt.
    let askpass = dir.join("askpass.sh");
    let script = format!(
        "#!/bin/sh\nexec '{}' --askpass '{}' \"$@\"\n",
        exe.display(),
        sock.display()
    );
    if !write_exec(&askpass, &script, 0o700) {
        return;
    }

    // sudo shim: force `sudo -A` so sudo always uses our askpass helper instead
    // of trying to read from the (raw-mode) terminal it can't actually use.
    let shim_dir = dir.join("bin");
    if std::fs::create_dir_all(&shim_dir).is_err() {
        return;
    }
    let shim = shim_dir.join("sudo");
    let shim_script = format!("#!/bin/sh\nexec '{}' -A \"$@\"\n", sudo.display());
    if !write_exec(&shim, &shim_script, 0o755) {
        return;
    }

    // Make the agent's bash children use both. Safe: no other thread is running.
    std::env::set_var("SUDO_ASKPASS", &askpass);
    match std::env::var_os("PATH") {
        Some(path) => {
            let mut paths = vec![shim_dir.clone()];
            paths.extend(std::env::split_paths(&path));
            if let Ok(joined) = std::env::join_paths(paths) {
                std::env::set_var("PATH", joined);
            }
        }
        None => std::env::set_var("PATH", &shim_dir),
    }

    // Listener: one masked prompt per connection (sudo retries open new ones).
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => handle_conn(stream, &ui),
                Err(_) => break,
            }
        }
    });
}

/// Serve one askpass request: read the prompt sudo gave us, ask the UI thread
/// for the password, and write the answer back (empty line = user cancelled).
fn handle_conn(stream: UnixStream, ui: &Sender<UiEvent>) {
    let mut prompt = String::new();
    if BufReader::new(&stream).read_line(&mut prompt).is_err() {
        return;
    }
    let (tx, rx) = mpsc::channel::<Option<String>>();
    if ui
        .send(UiEvent::PasswordRequest { prompt: prompt.trim().to_string(), reply: tx })
        .is_err()
    {
        return;
    }
    let pw = rx.recv().ok().flatten().unwrap_or_default();
    let _ = writeln!(&stream, "{pw}");
}

fn write_exec(path: &Path, contents: &str, mode: u32) -> bool {
    if std::fs::write(path, contents).is_err() {
        return false;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).is_ok()
}

fn find_sudo() -> Option<PathBuf> {
    ["/usr/bin/sudo", "/bin/sudo", "/usr/local/bin/sudo", "/sbin/sudo"]
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .map(Path::to_path_buf)
}

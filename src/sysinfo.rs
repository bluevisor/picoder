//! Tiny system snapshot for the launch banner. Reads Linux /proc and /sys;
//! degrades to "n/a" anywhere those aren't present (e.g. the dev Mac).

/// Network status line for the banner, e.g.
/// `● ONLINE · WiFi: HomeNet · IP: 10.0.0.216`.
pub fn network_line(ascii: bool) -> String {
    let dot = if ascii { "*" } else { "●" };
    let sep = if ascii { " - " } else { " · " };
    let mut parts = vec![format!("{dot} ONLINE")];
    parts.push(match ssid() {
        Some(s) => format!("WiFi: {s}"),
        None => format!("WiFi: {}", wifi_state()),
    });
    if let Some(ip) = local_ip() {
        parts.push(format!("IP: {ip}"));
    }
    parts.join(sep)
}

/// Source IP of the default route (no packets are sent — connect() on a UDP
/// socket just selects the local address the kernel would route from).
fn local_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

/// Current WiFi SSID. Tries NetworkManager (nmcli), then iwgetid, then iw.
fn ssid() -> Option<String> {
    if let Ok(out) =
        std::process::Command::new("nmcli").args(["-t", "-f", "active,ssid", "dev", "wifi"]).output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(rest) = line.strip_prefix("yes:") {
                let s = rest.trim();
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    if let Ok(out) = std::process::Command::new("iwgetid").arg("-r").output() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    for iface in wireless_ifaces() {
        if let Ok(out) = std::process::Command::new("iw").args(["dev", &iface, "link"]).output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(rest) = line.trim().strip_prefix("SSID:") {
                    let s = rest.trim();
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    None
}

fn wireless_ifaces() -> Vec<String> {
    let mut out = vec!["wlan0".to_string()];
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("wl") && n != "wlan0" {
                out.push(n);
            }
        }
    }
    out
}

/// Total RAM in MB, for the host descriptor. None when /proc isn't present.
pub fn mem_total_mb() -> Option<u64> {
    mem_used_total().map(|(_, t)| t)
}

/// CPU core count, best-effort.
pub fn cpu_cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// The board/machine name from the device tree (e.g. "Raspberry Pi 5 Model B
/// Rev 1.0"). None on hosts without /proc/device-tree (e.g. the dev Mac).
pub fn board_model() -> Option<String> {
    let raw = std::fs::read("/proc/device-tree/model").ok()?;
    // Device-tree strings are NUL-terminated.
    let s = String::from_utf8_lossy(&raw).trim_end_matches('\0').trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Hardware summary for the banner, e.g.
/// `Raspberry Pi 5 Model B · aarch64 · 4 cores · MEM 567/8059MB`. Degrades to
/// just arch/cores where the board name or /proc aren't available.
pub fn hardware_line(ascii: bool) -> String {
    let sep = if ascii { " - " } else { " · " };
    let mut parts = Vec::new();
    if let Some(b) = board_model() {
        parts.push(b);
    }
    parts.push(std::env::consts::ARCH.to_string());
    let cores = cpu_cores();
    parts.push(format!("{cores} {}", if cores == 1 { "core" } else { "cores" }));
    if let Some((used, total)) = mem_used_total() {
        parts.push(format!("MEM {used}/{total}MB"));
    }
    parts.join(sep)
}

/// One-line hardware description for the agent's system prompt, e.g.
/// `NVIDIA Jetson Nano 2GB Developer Kit, aarch64, 4 cores, ~1965MB RAM
/// (Ubuntu 18.04.6 LTS)`. Lets one binary describe itself honestly on anything
/// from a Pi Zero W to a Jetson Nano — board name and OS are read at runtime
/// rather than hardcoded, so the agent knows what hardware/distro it's on.
pub fn host_descriptor() -> String {
    let arch = std::env::consts::ARCH;
    let cores = cpu_cores();
    let core_word = if cores == 1 { "core" } else { "cores" };
    let mem = mem_total_mb()
        .map(|t| format!(", ~{t}MB RAM"))
        .unwrap_or_default();
    let board = board_model().map(|b| format!("{b}, ")).unwrap_or_default();
    format!("{board}{arch}, {cores} {core_word}{mem} ({})", os_name())
}

/// Distro/OS pretty name from /etc/os-release (e.g. "Ubuntu 18.04.6 LTS"),
/// falling back to "Linux" where it isn't present (e.g. the dev Mac).
pub fn os_name() -> String {
    if let Ok(s) = std::fs::read_to_string("/etc/os-release") {
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                let v = v.trim().trim_matches('"');
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    "Linux".to_string()
}

fn mem_used_total() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let (mut total, mut avail) = (0u64, 0u64);
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = parse_kb(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail = parse_kb(v);
        }
    }
    if total == 0 {
        return None;
    }
    Some((total.saturating_sub(avail) / 1024, total / 1024))
}

fn parse_kb(s: &str) -> u64 {
    s.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0)
}

fn wifi_state() -> String {
    let interpret = |s: &str| {
        let s = s.trim();
        if s == "up" {
            "OK".to_string()
        } else {
            s.to_uppercase()
        }
    };
    if let Ok(s) = std::fs::read_to_string("/sys/class/net/wlan0/operstate") {
        return interpret(&s);
    }
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("wl") {
                if let Ok(s) = std::fs::read_to_string(e.path().join("operstate")) {
                    return interpret(&s);
                }
            }
        }
    }
    "n/a".into()
}

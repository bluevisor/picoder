//! OAuth 2.0 (PKCE) "subscription login" for providers that authenticate with
//! an account rather than a pay-as-you-go API key: Anthropic (Claude Pro/Max),
//! OpenAI (ChatGPT) and Google (Gemini). The browser is opened to the
//! provider's consent page and the redirect is caught on a localhost listener.
//!
//! Zero external crates — SHA-256, base64url, a CSPRNG (`/dev/urandom`) and
//! percent-encoding are implemented here so the Pi-Zero build stays tiny, the
//! same way the rest of picode hand-rolls its primitives (FNV in config.rs).
//!
//! The client ids / endpoints below are the public values shipped by each
//! vendor's own first-party CLI. They are reverse-engineered and may change;
//! every field can be overridden at runtime via env vars (see `provider`) so a
//! breakage is fixable without a recompile.

use crate::config::OAuthToken;
use anyhow::{anyhow, bail, Result};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// One OAuth provider's endpoints and client registration.
#[derive(Clone)]
pub struct OAuthProvider {
    /// Matches the `provider` name in config (deepseek has no entry).
    pub key: String,
    pub label: String,
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    /// Empty for public clients (PKCE only); some vendors (Google) ship one.
    pub client_secret: String,
    pub scope: String,
    pub redirect_port: u16,
    pub redirect_path: String,
    /// Extra query params appended to the authorize URL.
    pub extra: Vec<(String, String)>,
}

/// Built-in defaults, overridable via `PICODE_OAUTH_<PROVIDER>_<FIELD>` env vars
/// (e.g. `PICODE_OAUTH_ANTHROPIC_CLIENT_ID`). Returns None for providers that
/// don't support subscription login (e.g. deepseek — API key only).
pub fn provider(name: &str) -> Option<OAuthProvider> {
    let mut p = match name {
        "anthropic" => OAuthProvider {
            key: "anthropic".into(),
            label: "Claude Pro/Max".into(),
            auth_url: "https://claude.ai/oauth/authorize".into(),
            token_url: "https://console.anthropic.com/v1/oauth/token".into(),
            // Claude Code's public client id.
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".into(),
            client_secret: String::new(),
            scope: "org:create_api_key user:profile user:inference".into(),
            redirect_port: 54545,
            redirect_path: "/callback".into(),
            extra: vec![],
        },
        "openai" => OAuthProvider {
            key: "openai".into(),
            label: "ChatGPT".into(),
            auth_url: "https://auth.openai.com/oauth/authorize".into(),
            token_url: "https://auth.openai.com/oauth/token".into(),
            // Codex CLI's public client id.
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann".into(),
            client_secret: String::new(),
            scope: "openid profile email offline_access".into(),
            redirect_port: 1455,
            redirect_path: "/auth/callback".into(),
            extra: vec![
                ("prompt".into(), "login".into()),
                ("id_token_add_organizations".into(), "true".into()),
            ],
        },
        "google" => OAuthProvider {
            key: "google".into(),
            label: "Gemini".into(),
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            // Google requires a registered client id + secret; supply your own
            // (or Gemini CLI's public desktop pair) via
            // PICODE_OAUTH_GOOGLE_CLIENT_ID / PICODE_OAUTH_GOOGLE_CLIENT_SECRET.
            client_id: String::new(),
            client_secret: String::new(),
            scope: "https://www.googleapis.com/auth/cloud-platform \
                    https://www.googleapis.com/auth/userinfo.email \
                    https://www.googleapis.com/auth/userinfo.profile"
                .into(),
            redirect_port: 8085,
            redirect_path: "/oauth2callback".into(),
            extra: vec![
                ("access_type".into(), "offline".into()),
                ("prompt".into(), "consent".into()),
            ],
        },
        _ => return None,
    };
    // Env overrides keep reverse-engineered constants fixable without a rebuild.
    let up = name.to_uppercase();
    if let Ok(v) = std::env::var(format!("PICODE_OAUTH_{up}_CLIENT_ID")) {
        p.client_id = v;
    }
    if let Ok(v) = std::env::var(format!("PICODE_OAUTH_{up}_CLIENT_SECRET")) {
        p.client_secret = v;
    }
    if let Ok(v) = std::env::var(format!("PICODE_OAUTH_{up}_AUTH_URL")) {
        p.auth_url = v;
    }
    if let Ok(v) = std::env::var(format!("PICODE_OAUTH_{up}_TOKEN_URL")) {
        p.token_url = v;
    }
    Some(p)
}

/// Provider keys that support subscription login, for help text / completion.
pub fn supported() -> &'static [&'static str] {
    &["anthropic", "openai", "google"]
}

/// A login in progress: the authorize URL to show the user and the bound
/// listener that will catch the redirect. Split from `finish` so the caller can
/// surface the URL (and the "opening browser" notice) before blocking.
pub struct Login {
    pub url: String,
    provider: OAuthProvider,
    listener: TcpListener,
    verifier: String,
    state: String,
    redirect_uri: String,
}

/// Bind the localhost listener and build the authorize URL. Fails early if the
/// redirect port is already in use.
pub fn start(p: OAuthProvider) -> Result<Login> {
    if p.client_id.is_empty() {
        let up = p.key.to_uppercase();
        bail!(
            "no OAuth client id for {}. Set PICODE_OAUTH_{up}_CLIENT_ID (and \
             PICODE_OAUTH_{up}_CLIENT_SECRET if the provider needs one) and retry.",
            p.label
        );
    }
    let redirect_uri = format!("http://localhost:{}{}", p.redirect_port, p.redirect_path);
    let listener = TcpListener::bind(("127.0.0.1", p.redirect_port)).map_err(|e| {
        anyhow!("cannot bind localhost:{} for the OAuth redirect ({e}). Close whatever is using it and retry.", p.redirect_port)
    })?;
    let verifier = rand_b64url(32);
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let state = rand_b64url(16);
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        p.auth_url,
        pct(&p.client_id),
        pct(&redirect_uri),
        pct(&p.scope),
        pct(&state),
        pct(&challenge),
    );
    for (k, v) in &p.extra {
        url.push('&');
        url.push_str(&pct(k));
        url.push('=');
        url.push_str(&pct(v));
    }
    Ok(Login { url, provider: p, listener, verifier, state, redirect_uri })
}

impl Login {
    /// Open the browser, wait (up to 5 min) for the redirect, then exchange the
    /// authorization code for tokens.
    pub fn finish(self) -> Result<OAuthToken> {
        open_browser(&self.url);
        let code = wait_for_code(&self.listener, &self.state)?;
        exchange_code(&self.provider, &code, &self.verifier, &self.redirect_uri)
    }
}

/// Exchange an authorization code for tokens (standard OAuth form POST).
fn exchange_code(p: &OAuthProvider, code: &str, verifier: &str, redirect_uri: &str) -> Result<OAuthToken> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", &p.client_id),
        ("code_verifier", verifier),
    ];
    if !p.client_secret.is_empty() {
        form.push(("client_secret", &p.client_secret));
    }
    post_token(&p.token_url, &form)
}

/// Trade a refresh token for a fresh access token. Preserves the existing
/// refresh token when the provider doesn't return a new one.
pub fn refresh(p: &OAuthProvider, token: &OAuthToken) -> Result<OAuthToken> {
    if token.refresh_token.is_empty() {
        bail!("no refresh token for {} — run /login {} again", p.key, p.key);
    }
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", token.refresh_token.as_str()),
        ("client_id", &p.client_id),
    ];
    if !p.client_secret.is_empty() {
        form.push(("client_secret", &p.client_secret));
    }
    let mut fresh = post_token(&p.token_url, &form)?;
    if fresh.refresh_token.is_empty() {
        fresh.refresh_token = token.refresh_token.clone();
    }
    if fresh.account_id.is_empty() {
        fresh.account_id = token.account_id.clone();
    }
    Ok(fresh)
}

/// POST a form-encoded token request and parse the JSON response into a token.
fn post_token(url: &str, form: &[(&str, &str)]) -> Result<OAuthToken> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", pct(k), pct(v)))
        .collect::<Vec<_>>()
        .join("&");
    let http = crate::api::agent_http();
    let resp = http
        .post(url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Accept", "application/json")
        .send_string(&body);
    let text = match resp {
        Ok(r) => r.into_string().unwrap_or_default(),
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            bail!("token endpoint HTTP {code}: {}", truncate(&detail, 400));
        }
        Err(e) => bail!("token request failed: {e}"),
    };
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| anyhow!("token endpoint returned non-JSON: {}", truncate(&text, 200)))?;
    let access = v.get("access_token").and_then(|x| x.as_str()).unwrap_or_default();
    if access.is_empty() {
        bail!("token endpoint returned no access_token: {}", truncate(&text, 200));
    }
    let refresh = v.get("refresh_token").and_then(|x| x.as_str()).unwrap_or_default();
    let expires_in = v.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(0);
    let expires_at = if expires_in > 0 { now_unix() + expires_in } else { 0 };
    // OpenAI returns the ChatGPT account id inside the id_token JWT claims; other
    // providers may expose it directly. Best-effort — empty when absent.
    let account_id = v
        .get("account_id")
        .and_then(|x| x.as_str())
        .map(String::from)
        .or_else(|| {
            v.get("id_token")
                .and_then(|x| x.as_str())
                .and_then(jwt_account_id)
        })
        .unwrap_or_default();
    Ok(OAuthToken {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        expires_at,
        account_id,
    })
}

/// Pull `chatgpt_account_id` (or `organization_id`) out of an unverified JWT
/// payload. We only read a routing hint, never trust the token's signature.
fn jwt_account_id(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = b64url_decode(payload)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    // Codex nests the account id under a namespaced auth claim.
    let auth = v.get("https://api.openai.com/auth");
    auth.and_then(|a| a.get("chatgpt_account_id"))
        .or_else(|| auth.and_then(|a| a.get("organization_id")))
        .or_else(|| v.get("chatgpt_account_id"))
        .and_then(|x| x.as_str())
        .map(String::from)
}

/// Block on the listener until the OAuth redirect arrives (5-minute deadline),
/// validate `state`, reply with a friendly page, and return the code.
fn wait_for_code(listener: &TcpListener, expect_state: &str) -> Result<String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| anyhow!("listener setup failed: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() >= deadline {
            bail!("login timed out after 5 minutes with no browser redirect");
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let target = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
                let params = parse_query(target);
                let (msg, result) = if let Some(err) = params.iter().find(|(k, _)| k == "error") {
                    ("Login failed. You can close this tab.".to_string(), Err(anyhow!("provider returned error: {}", err.1)))
                } else if let Some((_, code)) = params.iter().find(|(k, _)| k == "code") {
                    let state = params.iter().find(|(k, _)| k == "state").map(|(_, v)| v.as_str()).unwrap_or("");
                    if !expect_state.is_empty() && state != expect_state {
                        ("Login failed (state mismatch).".to_string(), Err(anyhow!("OAuth state mismatch — possible CSRF, aborted")))
                    } else {
                        ("Login complete — you can close this tab and return to picode.".to_string(), Ok(code.clone()))
                    }
                } else {
                    // Not the callback (e.g. favicon.ico); ignore and keep waiting.
                    let _ = write_http(&mut stream, "Waiting for the OAuth redirect…");
                    continue;
                };
                let _ = write_http(&mut stream, &msg);
                return result;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => bail!("accept failed: {e}"),
        }
    }
}

fn write_http(stream: &mut std::net::TcpStream, body: &str) -> std::io::Result<()> {
    let html = format!(
        "<!doctype html><html><head><meta charset=utf-8><title>picode</title></head>\
         <body style=\"font-family:system-ui;background:#0b0b0b;color:#eee;display:flex;\
         align-items:center;justify-content:center;height:100vh;margin:0\">\
         <p style=\"font-size:1.1rem\">{body}</p></body></html>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    stream.write_all(resp.as_bytes())
}

/// Open `url` in the system browser (best-effort; failure is non-fatal since the
/// URL is also printed to the transcript).
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(target_os = "windows")]
    let prog = "explorer";
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let prog = "xdg-open";
    let _ = std::process::Command::new(prog)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ---------------------------------------------------------------------------
// Self-contained primitives (no crates): time, RNG, base64url, SHA-256, pct.
// ---------------------------------------------------------------------------

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `len` random bytes, base64url-encoded (no padding). Uses the OS CSPRNG.
fn rand_b64url(len: usize) -> String {
    b64url(&rand_bytes(len))
}

fn rand_bytes(len: usize) -> Vec<u8> {
    // Unix targets (Pi, Jetson, macOS) all expose /dev/urandom.
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut buf = vec![0u8; len];
        if f.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }
    // Last-resort fallback: mix time + address entropy. Only hit if /dev/urandom
    // is missing; PKCE secrecy degrades but the flow still works.
    let mut seed = now_unix() ^ (&len as *const usize as u64);
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed & 0xff) as u8
        })
        .collect()
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding (RFC 7636 / PKCE).
fn b64url(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64URL[(n >> 18 & 63) as usize] as char);
        out.push(B64URL[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL[(n & 63) as usize] as char);
        }
    }
    out
}

/// Decode base64url (padding optional) — used to read JWT payloads.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut rev = [255u8; 256];
    for (i, &b) in B64URL.iter().enumerate() {
        rev[b as usize] = i as u8;
    }
    let mut bits = 0u32;
    let mut nbits = 0;
    let mut out = Vec::new();
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = rev[c as usize];
        if v == 255 {
            return None;
        }
        bits = (bits << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Some(out)
}

/// SHA-256 (FIPS 180-4). Small and dependency-free; used for the PKCE S256
/// challenge.
fn sha256(msg: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut data = msg.to_vec();
    let bitlen = (msg.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bitlen.to_be_bytes());
    for block in data.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[i * 4], block[i * 4 + 1], block[i * 4 + 2], block[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(t1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

/// Percent-encode for query strings / form bodies (RFC 3986 unreserved set).
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-decode a single query value.
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = hex_val(b[i + 1]).zip(hex_val(b[i + 2]));
                if let Some((hi, lo)) = h {
                    out.push(hi << 4 | lo);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
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

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parse `key=value` pairs out of a request target's query string.
fn parse_query(target: &str) -> Vec<(String, String)> {
    let q = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        // FIPS 180-4 examples.
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn base64url_roundtrip_and_no_padding() {
        let data = b"Hello, PKCE world!";
        let enc = b64url(data);
        assert!(!enc.contains('='));
        assert_eq!(b64url_decode(&enc).unwrap(), data);
        // Known vector: base64url("abc") == "YWJj".
        assert_eq!(b64url(b"abc"), "YWJj");
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        // RFC 7636 appendix B vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(b64url(&sha256(verifier.as_bytes())), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn query_parsing_decodes_percent_escapes() {
        let q = parse_query("/callback?code=ab%2Fcd&state=xy%20z");
        assert_eq!(q.iter().find(|(k, _)| k == "code").unwrap().1, "ab/cd");
        assert_eq!(q.iter().find(|(k, _)| k == "state").unwrap().1, "xy z");
    }

    #[test]
    fn authorize_url_has_pkce_and_redirect() {
        let p = provider("anthropic").unwrap();
        let lg = start(p).unwrap();
        assert!(lg.url.contains("code_challenge_method=S256"));
        assert!(lg.url.contains("code_challenge="));
        assert!(lg.url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A54545%2Fcallback"));
        assert!(lg.url.starts_with("https://claude.ai/oauth/authorize?"));
    }

    #[test]
    fn unknown_provider_has_no_oauth() {
        assert!(provider("deepseek").is_none());
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}

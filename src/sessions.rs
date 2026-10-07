//! Saved conversations. Each working directory gets a folder under
//! `~/.config/picoder/sessions/<FNV-1a of its path>/` holding any number of
//! sessions: `<id>.json` is the conversation (the same message array a
//! session file always held) and `<id>.meta.json` its id, name, title, times
//! and size, so listing sessions never parses a conversation — cheap on a Pi.
//!
//! Before multiple sessions there was one file per directory,
//! `sessions/<hash>.json`; it moves into the folder as an ordinary session the
//! first time that directory is used.

use crate::api::Message;
use crate::config::{atomic_write, now_unix};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Sessions kept per directory, and across all directories; past that the
/// least recently updated are deleted so an SD card can't fill with them.
pub const KEEP_PER_DIR: usize = 50;
pub const KEEP_TOTAL: usize = 400;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The first prompt, one line.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    /// Messages other than the system prefix.
    #[serde(default)]
    pub messages: usize,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

impl Meta {
    /// `name` if set, else the title, else a placeholder.
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) => n.clone(),
            None if !self.title.is_empty() => self.title.clone(),
            None => "(no prompt yet)".into(),
        }
    }
}

/// The sessions of one working directory, rooted at `root`
/// (`~/.config/picoder/sessions` outside tests).
pub struct Store {
    pub root: PathBuf,
    pub cwd: PathBuf,
}

impl Store {
    /// The store for the process's working directory.
    pub fn here() -> Store {
        Store {
            root: crate::config::sessions_dir(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    fn hash(&self) -> String {
        // FNV-1a 64-bit of the path: deterministic, no dependency.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in self.cwd.display().to_string().bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    }

    /// This directory's session folder, after moving a pre-multi-session
    /// file into it.
    pub fn dir(&self) -> PathBuf {
        let dir = self.root.join(self.hash());
        let legacy = self.root.join(format!("{}.json", self.hash()));
        if legacy.is_file() {
            self.migrate(&legacy, &dir);
        }
        dir
    }

    /// Turn the old one-file session into a session in `dir`. On any failure
    /// the old file stays where it is (it's retried next time).
    fn migrate(&self, legacy: &Path, dir: &Path) {
        let Some(msgs) = load(legacy) else {
            // Empty or unreadable: nothing worth keeping as a session.
            return;
        };
        let when = std::fs::metadata(legacy)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or_else(now_unix);
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let id = new_id(dir);
        let path = dir.join(format!("{id}.json"));
        if std::fs::rename(legacy, &path).is_err() {
            return;
        }
        let meta = Meta {
            id,
            title: title_of(&msgs),
            created: when,
            updated: when,
            messages: conversation_len(&msgs),
            cwd: self.cwd.display().to_string(),
            ..Meta::default()
        };
        let _ = write_meta(&path, &meta);
    }

    /// A path for a new session here (the file appears on its first save).
    pub fn new_path(&self) -> PathBuf {
        let dir = self.dir();
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{}.json", new_id(&dir)))
    }

    /// Every session here, most recently updated first. A conversation file
    /// whose meta is missing (a crash between the two writes) still lists.
    pub fn list(&self) -> Vec<(Meta, PathBuf)> {
        let dir = self.dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<(Meta, PathBuf)> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_session_file(p))
            .map(|p| (read_meta(&p).unwrap_or_else(|| synth_meta(&p)), p))
            .collect();
        out.sort_by(|a, b| b.0.updated.cmp(&a.0.updated).then_with(|| b.0.id.cmp(&a.0.id)));
        out
    }

    pub fn latest(&self) -> Option<(Meta, PathBuf)> {
        self.list().into_iter().next()
    }

    /// The session `key` names: an id, a unique id prefix (4+ characters), or
    /// a name (any case).
    pub fn find(&self, key: &str) -> Result<(Meta, PathBuf), String> {
        let key = key.trim();
        let all = self.list();
        if let Some(hit) = all.iter().find(|(m, _)| m.id == key) {
            return Ok(hit.clone());
        }
        let by_name: Vec<_> = all
            .iter()
            .filter(|(m, _)| m.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(key)))
            .collect();
        if let [one] = by_name.as_slice() {
            return Ok((*one).clone());
        }
        let by_prefix: Vec<_> = all.iter().filter(|(m, _)| key.len() >= 4 && m.id.starts_with(key)).collect();
        match (by_prefix.as_slice(), by_name.len()) {
            ([one], 0) => Ok((*one).clone()),
            ([], 0) => Err(format!("no session here matches '{key}' (`picoder sessions` lists them)")),
            _ => Err(format!("'{key}' matches more than one session; use more of the id")),
        }
    }

    /// Drop sessions past the retention caps, least recently updated first,
    /// never `keep` (the one in use).
    pub fn prune(&self, keep: &Path) {
        for (_, p) in self.list().into_iter().skip(KEEP_PER_DIR) {
            if p != keep {
                delete(&p);
            }
        }
        let mut all: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten())
            .map(|e| e.path())
            .filter(|p| is_session_file(p))
            .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
            .collect();
        if all.len() > KEEP_TOTAL {
            all.sort_by(|a, b| b.0.cmp(&a.0));
            for (_, p) in all.into_iter().skip(KEEP_TOTAL) {
                if p != keep {
                    delete(&p);
                }
            }
        }
    }
}

fn is_session_file(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with(".json") && !name.ends_with(".meta.json") && !name.contains(".tmp.")
}

fn meta_path(session: &Path) -> PathBuf {
    session.with_extension("meta.json")
}

fn id_of(session: &Path) -> String {
    session.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
}

/// A fresh 8-hex-digit id unused in `dir` (from the clock; bumped on a clash).
fn new_id(dir: &Path) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut n = (millis ^ ((std::process::id() as u64) << 20)) as u32;
    loop {
        let id = format!("{n:08x}");
        if !dir.join(format!("{id}.json")).exists() && !dir.join(format!("{id}.meta.json")).exists() {
            return id;
        }
        n = n.wrapping_add(1);
    }
}

pub fn read_meta(session: &Path) -> Option<Meta> {
    let text = std::fs::read_to_string(meta_path(session)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_meta(session: &Path, meta: &Meta) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(meta).map_err(std::io::Error::other)?;
    atomic_write(&meta_path(session), json.as_bytes())
}

/// Meta rebuilt from the conversation itself, for a file that has none.
fn synth_meta(session: &Path) -> Meta {
    let msgs = load(session).unwrap_or_default();
    let when = std::fs::metadata(session)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Meta {
        id: id_of(session),
        title: title_of(&msgs),
        created: when,
        updated: when,
        messages: conversation_len(&msgs),
        ..Meta::default()
    }
}

/// A session's messages; None when missing, unreadable or empty.
pub fn load(session: &Path) -> Option<Vec<Message>> {
    let text = std::fs::read_to_string(session).ok()?;
    let msgs: Vec<Message> = serde_json::from_str(&text).ok()?;
    (!msgs.is_empty()).then_some(msgs)
}

/// Write the conversation and refresh its meta (title, size, update time).
/// `messages` should already be stripped of image payloads. The first save of
/// a session also applies the retention caps.
pub fn save(store: &Store, session: &Path, messages: &[Message]) -> std::io::Result<()> {
    if let Some(dir) = session.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(messages).map_err(std::io::Error::other)?;
    atomic_write(session, json.as_bytes())?;
    let now = now_unix();
    let first_save = read_meta(session).is_none();
    let mut meta = read_meta(session).unwrap_or_else(|| Meta {
        id: id_of(session),
        created: now,
        cwd: store.cwd.display().to_string(),
        ..Meta::default()
    });
    meta.updated = now;
    meta.messages = conversation_len(messages);
    let title = title_of(messages);
    if !title.is_empty() {
        meta.title = title;
    }
    write_meta(session, &meta)?;
    if first_save {
        store.prune(session);
    }
    Ok(())
}

/// Copy a conversation into a new session beside `from`, recording where it
/// came from. Returns the new session's path.
pub fn fork(store: &Store, from: &Path, messages: &[Message]) -> std::io::Result<PathBuf> {
    let to = store.new_path();
    let parent = read_meta(from).map(|m| m.id).unwrap_or_else(|| id_of(from));
    let meta = Meta {
        id: id_of(&to),
        created: now_unix(),
        cwd: store.cwd.display().to_string(),
        forked_from: Some(parent),
        ..Meta::default()
    };
    write_meta(&to, &meta)?;
    save(store, &to, messages)?;
    Ok(to)
}

pub fn rename(session: &Path, name: &str) -> std::io::Result<Meta> {
    let mut meta = read_meta(session).unwrap_or_else(|| synth_meta(session));
    let name = name.trim();
    meta.name = (!name.is_empty()).then(|| name.to_string());
    write_meta(session, &meta)?;
    Ok(meta)
}

pub fn delete(session: &Path) {
    let _ = std::fs::remove_file(session);
    let _ = std::fs::remove_file(meta_path(session));
}

/// The text the user typed, when `m` is a prompt: picoder's own notes to the
/// model (`[...]`, `(...)`) aren't. Attachments and hook context follow a
/// blank line and are left out.
pub fn prompt_text(m: &Message) -> Option<&str> {
    if m.role != "user" || m.content.starts_with('[') || m.content.starts_with('(') {
        return None;
    }
    let typed = m.content.split("\n\n").next().unwrap_or_default().trim();
    (!typed.is_empty()).then_some(typed)
}

/// The first prompt on one line, at most 60 characters.
pub fn title_of(messages: &[Message]) -> String {
    let Some(p) = messages.iter().find_map(prompt_text) else {
        return String::new();
    };
    let line = p.lines().next().unwrap_or_default().trim();
    if line.chars().count() > 60 {
        format!("{}…", line.chars().take(59).collect::<String>())
    } else {
        line.to_string()
    }
}

/// The last few exchanges as (role, text), to show when a session is resumed:
/// prompts as typed, and replies that have text.
pub fn replay(messages: &[Message]) -> Vec<(String, String)> {
    let all: Vec<(String, String)> = messages
        .iter()
        .filter_map(|m| match m.role.as_str() {
            "user" => prompt_text(m).map(|t| ("user".to_string(), t.to_string())),
            "assistant" if !m.content.trim().is_empty() => Some(("assistant".to_string(), m.content.trim().to_string())),
            _ => None,
        })
        .collect();
    all[all.len().saturating_sub(6)..].to_vec()
}

fn conversation_len(messages: &[Message]) -> usize {
    messages.iter().filter(|m| m.role != "system").count()
}

/// "just now", "5m ago", "3h ago", "2d ago", or the date past a month.
pub fn ago(then: u64, now: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        86_400..=2_591_999 => format!("{}d ago", s / 86_400),
        _ => local_time(then).split(' ').next().unwrap_or_default().to_string(),
    }
}

/// `YYYY-MM-DD HH:MM` in local time.
pub fn local_time(secs: u64) -> String {
    let t = secs as libc::time_t;
    // SAFETY: localtime_r only writes the tm we hand it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return String::new();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// One picker / listing line: "id  label · when · n msgs".
pub fn summary_line(meta: &Meta, now: u64, current: bool) -> String {
    let fork = meta.forked_from.as_ref().map(|f| format!(" · fork of {f}")).unwrap_or_default();
    let here = if current { " · current" } else { "" };
    format!(
        "{}  {} · {} · {} msgs{fork}{here}",
        meta.id,
        meta.label(),
        ago(meta.updated, now),
        meta.messages
    )
}

/// A readable Markdown transcript: prompts, replies, and each tool call with
/// its arguments and (truncated) result. picoder's system/context messages
/// are left out unless `all`.
pub fn to_markdown(meta: &Meta, messages: &[Message], all: bool) -> String {
    let mut out = format!("# {}\n\n", meta.label());
    out.push_str(&format!(
        "_picoder session `{}` · {} · started {}_\n",
        meta.id,
        if meta.cwd.is_empty() { "?" } else { &meta.cwd },
        local_time(meta.created)
    ));
    let result_of = |id: &str| {
        messages
            .iter()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(id))
            .map(|m| m.content.as_str())
    };
    for m in messages {
        match m.role.as_str() {
            "system" if all => {
                out.push_str(&format!("\n## System\n\n{}\n", m.content.trim()));
            }
            "user" => match prompt_text(m) {
                Some(_) => out.push_str(&format!("\n## You\n\n{}\n", m.content.trim())),
                None => {
                    for line in m.content.trim().lines() {
                        out.push_str(&format!("\n> {line}"));
                    }
                    out.push('\n');
                }
            },
            "assistant" => {
                if !m.content.trim().is_empty() {
                    out.push_str(&format!("\n## picoder\n\n{}\n", m.content.trim()));
                }
                for c in m.tool_calls.iter().flatten() {
                    let args = crate::api::truncate(&c.function.arguments, 800);
                    out.push_str(&format!("\n**{}** `{}`\n", c.function.name, args.replace('`', "'")));
                    if let Some(r) = result_of(&c.id) {
                        out.push_str(&format!("\n```\n{}\n```\n", crate::api::truncate(r.trim(), 1500)));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A file name for an export: the title as a lowercase slug, `.md`.
pub fn export_name(meta: &Meta) -> String {
    let slug: String = meta
        .label()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug: Vec<&str> = slug.split('-').filter(|s| !s.is_empty()).collect();
    let slug = slug.join("-");
    let slug: String = slug.chars().take(40).collect();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        format!("picoder-{}.md", meta.id)
    } else {
        format!("{slug}.md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Store {
        let root = std::env::temp_dir().join(format!("picoder-sessions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Store { root, cwd: PathBuf::from(format!("/work/{name}")) }
    }

    fn convo(prompt: &str) -> Vec<Message> {
        vec![Message::system("sys"), Message::user(prompt.to_string()), Message::user("[note]".to_string())]
    }

    #[test]
    fn several_sessions_per_directory_newest_first() {
        let s = store("multi");
        let a = s.new_path();
        save(&s, &a, &convo("first task")).unwrap();
        let b = s.new_path();
        assert_ne!(a, b);
        save(&s, &b, &convo("second task\n\n--- attached ---")).unwrap();
        // Make `a` the most recent.
        let mut meta = read_meta(&a).unwrap();
        meta.updated += 100;
        write_meta(&a, &meta).unwrap();
        let list = s.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0.title, "first task");
        assert_eq!(list[1].0.title, "second task", "attachments aren't part of the title");
        assert_eq!(list[1].0.messages, 2, "system messages aren't counted");
        assert_eq!(s.latest().unwrap().1, a);
        std::fs::remove_dir_all(&s.root).ok();
    }

    #[test]
    fn the_old_single_file_becomes_a_session() {
        let s = store("legacy");
        let legacy = s.root.join(format!("{}.json", s.hash()));
        std::fs::write(&legacy, serde_json::to_string(&convo("old work")).unwrap()).unwrap();
        let list = s.list();
        assert_eq!(list.len(), 1, "migrated");
        assert!(!legacy.exists(), "moved, not copied");
        assert_eq!(list[0].0.title, "old work");
        assert_eq!(load(&list[0].1).unwrap().len(), 3, "no messages lost");
        std::fs::remove_dir_all(&s.root).ok();
    }

    #[test]
    fn find_by_id_prefix_or_name() {
        let s = store("find");
        let a = s.new_path();
        save(&s, &a, &convo("a")).unwrap();
        rename(&a, "Shader Work").unwrap();
        let id = id_of(&a);
        assert_eq!(s.find(&id).unwrap().1, a);
        assert_eq!(s.find(&id[..5]).unwrap().1, a);
        assert_eq!(s.find("shader work").unwrap().1, a, "names match in any case");
        assert!(s.find("nope").unwrap_err().contains("no session"));
        assert!(s.find(&id[..2]).is_err(), "too short a prefix");
        std::fs::remove_dir_all(&s.root).ok();
    }

    #[test]
    fn fork_copies_and_keeps_the_original() {
        let s = store("fork");
        let a = s.new_path();
        save(&s, &a, &convo("original")).unwrap();
        let mut more = convo("original");
        more.push(Message::user("only in the fork".to_string()));
        let b = fork(&s, &a, &more).unwrap();
        assert_ne!(a, b);
        assert_eq!(load(&a).unwrap().len(), 3, "original untouched");
        assert_eq!(load(&b).unwrap().len(), 4);
        assert_eq!(read_meta(&b).unwrap().forked_from.as_deref(), Some(id_of(&a).as_str()));
        std::fs::remove_dir_all(&s.root).ok();
    }

    #[test]
    fn retention_drops_the_least_recently_updated() {
        let s = store("prune");
        let mut paths = Vec::new();
        for i in 0..KEEP_PER_DIR + 3 {
            let p = s.new_path();
            // Distinct, increasing update times without sleeping.
            std::fs::write(&p, serde_json::to_string(&convo(&format!("t{i}"))).unwrap()).unwrap();
            write_meta(&p, &Meta { id: id_of(&p), updated: 1000 + i as u64, title: format!("t{i}"), ..Meta::default() })
                .unwrap();
            paths.push(p);
        }
        s.prune(&paths[0]);
        let left = s.list();
        assert_eq!(left.len(), KEEP_PER_DIR + 1, "the cap, plus the one in use");
        assert!(paths[0].exists(), "the session in use survives");
        assert!(!paths[1].exists() && !paths[2].exists(), "oldest go first");
        std::fs::remove_dir_all(&s.root).ok();
    }

    #[test]
    fn markdown_export_reads_like_a_transcript() {
        let mut msgs = convo("make a page");
        let mut asst = Message::user(String::new());
        asst.role = "assistant".into();
        asst.content = "Writing it.".into();
        asst.tool_calls = Some(vec![crate::api::ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: crate::api::FunctionCall { name: "write_file".into(), arguments: "{\"path\":\"a.html\"}".into() },
        }]);
        msgs.push(asst);
        msgs.push(Message::tool("c1".to_string(), "OK wrote a.html".to_string()));
        let meta = Meta { id: "abcd1234".into(), title: "make a page".into(), ..Meta::default() };
        let md = to_markdown(&meta, &msgs, false);
        assert!(md.starts_with("# make a page"));
        assert!(md.contains("## You\n\nmake a page"));
        assert!(md.contains("**write_file** `{\"path\":\"a.html\"}`"));
        assert!(md.contains("OK wrote a.html"));
        assert!(md.contains("> [note]"), "picoder's notes are quoted");
        assert!(!md.contains("sys"), "system prompt left out");
        assert!(to_markdown(&meta, &msgs, true).contains("## System"));
        assert_eq!(export_name(&meta), "make-a-page.md");
        assert_eq!(export_name(&Meta { id: "ff".into(), ..Meta::default() }), "no-prompt-yet.md");
    }

    #[test]
    fn relative_times() {
        assert_eq!(ago(100, 130), "just now");
        assert_eq!(ago(0, 300), "5m ago");
        assert_eq!(ago(0, 7200), "2h ago");
        assert_eq!(ago(0, 3 * 86_400), "3d ago");
    }
}

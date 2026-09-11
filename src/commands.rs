//! Custom slash commands — prompt templates stored as Markdown files, the way
//! Claude Code's `.claude/commands/*.md` and skills work:
//!
//! ```text
//! .picoder/commands/<name>.md            project command  → /name
//! .picoder/skills/<name>/SKILL.md        project skill    → /name
//! ~/.config/picoder/commands/<name>.md   user command     → /name
//! ~/.config/picoder/skills/<name>/SKILL.md
//! .claude/commands/<name>.md             read for compatibility (prompts only)
//! ```
//!
//! Optional YAML-ish frontmatter supplies `description:` (shown in the `/`
//! palette) and `name:`. In the body `$ARGUMENTS` is replaced with everything
//! typed after the command and `$1`…`$9` with individual words; `@file`
//! references are attached like in a normal prompt. A built-in command of the
//! same name always wins.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: PathBuf,
}

/// Discover every custom command. Earlier sources win on a name clash
/// (project before user, picoder before claude).
pub fn load() -> Vec<CustomCommand> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let user = crate::config::config_dir();
    let mut roots: Vec<(PathBuf, bool)> = vec![
        (cwd.join(".picoder").join("commands"), false),
        (cwd.join(".picoder").join("skills"), true),
        (user.join("commands"), false),
        (user.join("skills"), true),
        (cwd.join(".claude").join("commands"), false),
        (cwd.join(".claude").join("skills"), true),
    ];
    roots.dedup();
    let mut out: Vec<CustomCommand> = Vec::new();
    for (root, skills) in roots {
        for c in load_dir(&root, skills) {
            if !out.iter().any(|x| x.name == c.name) {
                out.push(c);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Read one directory: `*.md` files (command name = file stem) or, for a
/// skills root, `<name>/SKILL.md` folders.
pub fn load_dir(root: &Path, skills: bool) -> Vec<CustomCommand> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let (file, name) = if skills {
            let Some(n) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            (path.join("SKILL.md"), n.to_string())
        } else {
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let Some(n) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            (path.clone(), n.to_string())
        };
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if let Some(c) = parse(&name, &text, &file) {
            out.push(c);
        }
    }
    out
}

/// Split frontmatter from body; derive a description when none is given.
pub fn parse(default_name: &str, text: &str, source: &Path) -> Option<CustomCommand> {
    let (front, body) = split_frontmatter(text);
    let mut name = default_name.to_string();
    let mut description = String::new();
    for line in front.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
        match k.trim() {
            "name" if !v.is_empty() => name = v.to_string(),
            "description" => description = v.to_string(),
            _ => {}
        }
    }
    let name: String = name
        .trim()
        .trim_start_matches('/')
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() || body.trim().is_empty() {
        return None;
    }
    if description.is_empty() {
        let first = body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("");
        description = crate::api::truncate(first, 60).replace('\n', " ");
        if description.is_empty() {
            description = "custom command".into();
        }
    }
    Some(CustomCommand {
        name,
        description,
        body: body.trim().to_string(),
        source: source.to_path_buf(),
    })
}

fn split_frontmatter(text: &str) -> (&str, &str) {
    let t = text.trim_start_matches('\u{feff}');
    let Some(rest) = t.strip_prefix("---") else {
        return ("", t);
    };
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    let Some(rest) = rest.strip_prefix('\n') else {
        return ("", t);
    };
    // Closing fence on its own line.
    for (i, _) in rest.match_indices("\n---") {
        let after = &rest[i + 4..];
        if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
            return (&rest[..i], after.trim_start_matches(['\r', '\n']));
        }
    }
    if let Some(body) = rest.strip_prefix("---") {
        return ("", body);
    }
    ("", t)
}

/// Substitute `$ARGUMENTS` and `$1`…`$9`. When the template mentions neither
/// and arguments were given, they're appended so nothing the user typed is lost.
pub fn expand(body: &str, args: &str) -> String {
    let args = args.trim();
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut out = String::with_capacity(body.len() + args.len());
    let mut used = false;
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' {
            let rest: String = chars[i + 1..].iter().take(9).collect();
            if rest.starts_with("ARGUMENTS") {
                out.push_str(args);
                used = true;
                i += 10;
                continue;
            }
            if let Some(d) = chars.get(i + 1).and_then(|c| c.to_digit(10)) {
                if d >= 1 {
                    out.push_str(words.get(d as usize - 1).copied().unwrap_or(""));
                    used = true;
                    i += 2;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    if !used && !args.is_empty() {
        out.push_str("\n\n");
        out.push_str(args);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_supplies_name_and_description() {
        let c = parse(
            "fix-issue",
            "---\ndescription: Fix a GitHub issue\nname: fix\n---\nFix issue $ARGUMENTS in this repo.\n",
            Path::new("x.md"),
        )
        .unwrap();
        assert_eq!(c.name, "fix");
        assert_eq!(c.description, "Fix a GitHub issue");
        assert_eq!(c.body, "Fix issue $ARGUMENTS in this repo.");
    }

    #[test]
    fn description_falls_back_to_the_first_prose_line_and_names_are_slugged() {
        let c = parse(
            "My Cmd",
            "# Title\n\nReview the diff carefully.\nMore.",
            Path::new("x.md"),
        )
        .unwrap();
        assert_eq!(c.name, "my-cmd");
        assert_eq!(c.description, "Review the diff carefully.");
        assert!(parse("empty", "---\ndescription: x\n---\n\n", Path::new("x.md")).is_none());
    }

    #[test]
    fn expand_substitutes_arguments_and_positionals_or_appends() {
        assert_eq!(expand("Fix $ARGUMENTS now", "issue 42"), "Fix issue 42 now");
        assert_eq!(expand("From $1 to $2 ($3)", "a b"), "From a to b ()");
        assert_eq!(
            expand("Do the thing", "with care"),
            "Do the thing\n\nwith care"
        );
        // An unset positional expands to nothing, like the shell.
        assert_eq!(expand("Costs $5", ""), "Costs ");
        assert_eq!(expand("Costs $$", ""), "Costs $$");
    }

    #[test]
    fn load_dir_reads_commands_and_skills() {
        let dir = std::env::temp_dir().join(format!("picoder-cmds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("commands")).unwrap();
        std::fs::create_dir_all(dir.join("skills/deploy")).unwrap();
        std::fs::write(dir.join("commands/review.md"), "Review $ARGUMENTS").unwrap();
        std::fs::write(dir.join("commands/notes.txt"), "ignored").unwrap();
        std::fs::write(
            dir.join("skills/deploy/SKILL.md"),
            "---\ndescription: Ship it\n---\nDeploy.",
        )
        .unwrap();
        let cmds = load_dir(&dir.join("commands"), false);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "review");
        let skills = load_dir(&dir.join("skills"), true);
        assert_eq!(skills.len(), 1);
        assert_eq!(
            (skills[0].name.as_str(), skills[0].description.as_str()),
            ("deploy", "Ship it")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Custom slash commands and skills — Markdown files, the way Claude Code's
//! `.claude/commands/*.md` and Agent Skills work:
//!
//! ```text
//! .picoder/commands/<name>.md, .claude/commands/<name>.md   project command → /name
//! ~/.config/picoder/commands/<name>.md                      user command    → /name
//! <skills dir>/<name>/SKILL.md                              skill → /name, and the
//!                                                           model's `skill` tool
//! ```
//!
//! Skills dirs, project before user: `.picoder/skills`, `.claude/skills` and
//! `.agents/skills` from the working directory up to the repo root, then
//! `~/.config/picoder/skills`, `~/.agents/skills`, `~/.claude/skills`. Only a
//! skill's name and description go in the system prompt; the model loads the
//! rest with the `skill` tool when a task calls for it.
//!
//! Optional YAML-ish frontmatter supplies `description:` (shown in the `/`
//! palette) and `name:`. In the body `$ARGUMENTS` is replaced with everything
//! typed after the command and `$1`…`$9` with individual words; `@file`
//! references are attached like in a normal prompt. A built-in command of the
//! same name always wins; otherwise the first found wins (project before user,
//! commands before skills).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: PathBuf,
}

/// Discover every custom command, skills included. Earlier sources win on a
/// name clash: project before user, and within each, commands before skills.
pub fn load() -> Vec<CustomCommand> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let user = crate::config::config_dir();
    let (project_skills, user_skills) = skill_roots(&cwd, &user, home().as_deref());
    let mut roots: Vec<(PathBuf, bool)> = vec![
        (cwd.join(".picoder").join("commands"), false),
        (cwd.join(".claude").join("commands"), false),
    ];
    roots.extend(project_skills.into_iter().map(|r| (r, true)));
    roots.push((user.join("commands"), false));
    roots.extend(user_skills.into_iter().map(|r| (r, true)));
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

/// A skill the model can load itself: a folder holding `SKILL.md` and
/// whatever scripts or references it points to.
#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill's folder; relative paths in its SKILL.md resolve here.
    pub dir: PathBuf,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Skills folders, project ones then the user's, most specific first: the
/// working directory's `.picoder/skills` and `.claude/skills`, `.agents/skills`
/// in each directory from it up to the repo root (just the working directory
/// outside a repo), then picoder's, the cross-agent and Claude's user folders.
fn skill_roots(cwd: &Path, user_cfg: &Path, home: Option<&Path>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut project = vec![cwd.join(".picoder").join("skills"), cwd.join(".claude").join("skills")];
    let in_repo = cwd.ancestors().any(|d| d.join(".git").exists());
    for d in cwd.ancestors() {
        project.push(d.join(".agents").join("skills"));
        if !in_repo || d.join(".git").exists() {
            break;
        }
    }
    let mut user = vec![user_cfg.join("skills")];
    if let Some(h) = home {
        user.push(h.join(".agents").join("skills"));
        user.push(h.join(".claude").join("skills"));
    }
    // The working directory may be $HOME: don't list a folder twice.
    user.retain(|r| !project.contains(r));
    (project, user)
}

/// Every skill in `roots`, first one winning a name clash.
fn skills_in(roots: &[PathBuf]) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for root in roots {
        for c in load_dir(root, true) {
            if out.iter().any(|s| s.name == c.name) {
                continue;
            }
            let dir = c.source.parent().map(Path::to_path_buf).unwrap_or_default();
            out.push(Skill { name: c.name, description: c.description, dir });
        }
    }
    out
}

/// Whether the model may see skills from other agents' user-wide folders
/// (`~/.agents/skills`, `~/.claude/skills`). Off unless `"shared_skills": true`:
/// those folders often hold work skills meant for one agent and one provider,
/// and every listed name and description goes out with each request.
fn shared_skills() -> bool {
    crate::config::settings_layers()
        .iter()
        .rev()
        .find_map(|(_, v)| v.get("shared_skills").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// The skills the model can see in the current working directory.
pub fn discover_skills() -> Vec<Skill> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let home = home().filter(|_| shared_skills());
    let (mut roots, user) = skill_roots(&cwd, &crate::config::config_dir(), home.as_deref());
    roots.extend(user);
    skills_in(&roots)
}

/// Caps on the skill list in the system prompt: it rides along with every
/// request, so on a Pi-sized model it has to stay small.
const SKILL_LIST_MAX: usize = 40;
const SKILL_DESC_MAX: usize = 160;
const SKILL_LIST_CHARS: usize = 4_000;

/// The system-prompt section listing `skills` by name and description only —
/// the bodies load on demand through the `skill` tool. None with no skills.
pub fn skills_prompt(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from(
        "Skills — task-specific instructions installed here. When a task matches a skill's \
         description, call the `skill` tool with its name before starting, then follow what it \
         says:\n",
    );
    let mut shown = 0;
    for s in skills.iter().take(SKILL_LIST_MAX) {
        let desc: String = s.description.chars().take(SKILL_DESC_MAX).collect();
        let line = format!("- {}: {}\n", s.name, desc.trim());
        if out.len() + line.len() > SKILL_LIST_CHARS {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < skills.len() {
        out.push_str(&format!(
            "- … and {} more (an unknown name makes the `skill` tool list them all)\n",
            skills.len() - shown
        ));
    }
    Some(out.trim_end().to_string())
}

/// What the `skill` tool returns: the skill's full SKILL.md instructions and
/// the files beside it, with absolute paths for read_file. An unknown name is
/// an error listing what exists.
pub fn skill_text(skills: &[Skill], name: &str) -> String {
    let key = slug(name);
    let Some(s) = skills.iter().find(|s| s.name == key) else {
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        return if names.is_empty() {
            format!("ERROR: no skill named '{name}' — no skills are installed here.")
        } else {
            format!("ERROR: no skill named '{name}'. Available skills: {}.", names.join(", "))
        };
    };
    let text = match std::fs::read_to_string(s.dir.join("SKILL.md")) {
        Ok(t) => t,
        Err(e) => return format!("ERROR: could not read {}: {e}", s.dir.join("SKILL.md").display()),
    };
    let (_, body) = split_frontmatter(&text);
    let body = crate::api::truncate(body.trim(), 24_000);
    let mut files = Vec::new();
    list_files(&s.dir, &s.dir, 0, &mut files);
    files.sort();
    let listing = if files.is_empty() {
        "(none besides SKILL.md)".to_string()
    } else {
        let more = files.len().saturating_sub(200);
        let mut l: Vec<String> = files.iter().take(200).map(|f| format!("- {}", s.dir.join(f).display())).collect();
        if more > 0 {
            l.push(format!("- … and {more} more"));
        }
        l.join("\n")
    };
    format!(
        "Skill `{}`: {}\nSkill directory: {}\n\n{body}\n\nFiles in this skill (relative paths in the \
         instructions above are relative to the skill directory; open them with read_file):\n{listing}",
        s.name,
        s.description,
        s.dir.display()
    )
}

/// The `skill` tool, against the skills installed for the working directory.
pub fn skill_tool(name: &str) -> String {
    if name.trim().is_empty() {
        return "ERROR: skill needs a 'name'.".into();
    }
    skill_text(&discover_skills(), name)
}

/// A skill folder's files besides SKILL.md, relative to `base`, skipping
/// hidden entries and stopping a few levels down.
fn list_files(base: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || out.len() >= 1_000 {
            continue;
        }
        let path = e.path();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if depth < 4 {
                list_files(base, &path, depth + 1, out);
            }
        } else if !(depth == 0 && name == "SKILL.md") {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.display().to_string());
            }
        }
    }
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
    for (k, v) in frontmatter_fields(front) {
        match k.as_str() {
            "name" if !v.is_empty() => name = v,
            "description" => description = v,
            _ => {}
        }
    }
    let name = slug(&name);
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

/// A command or skill name as typed after `/`: lowercase, with anything but
/// letters, digits, `-` and `_` turned into `-`.
fn slug(name: &str) -> String {
    name.trim()
        .trim_start_matches('/')
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect()
}

/// Top-level `key: value` pairs of YAML-ish frontmatter. A value may continue
/// on indented lines, including YAML's folded/literal block forms
/// (`description: >`), which other agents' SKILL.md files use; those are
/// joined into one line. Quotes around a value are dropped.
fn frontmatter_fields(front: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in front.lines() {
        let indented = line.starts_with([' ', '\t']);
        if indented {
            if let Some((_, v)) = out.last_mut() {
                let more = line.trim();
                if !more.is_empty() {
                    if !v.is_empty() {
                        v.push(' ');
                    }
                    v.push_str(more);
                }
            }
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let v = if matches!(v, ">" | "|" | ">-" | "|-" | ">+" | "|+") { "" } else { v };
        out.push((k.trim().to_string(), v.to_string()));
    }
    for (_, v) in &mut out {
        *v = v.trim_matches(|c| c == '"' || c == '\'').to_string();
    }
    out
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

#[cfg(test)]
mod skill_tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("picoder-skills-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn skill(root: &Path, name: &str, desc: &str) {
        fs::create_dir_all(root.join(name)).unwrap();
        fs::write(root.join(name).join("SKILL.md"), format!("---\ndescription: {desc}\n---\nBody of {name}.\n")).unwrap();
    }

    #[test]
    fn skills_are_found_project_first_up_to_the_repo_root() {
        let t = scratch("roots");
        let repo = t.join("repo");
        let cwd = repo.join("sub");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let (user_cfg, home) = (t.join("home/.config/picoder"), t.join("home"));
        skill(&cwd.join(".picoder/skills"), "deploy", "project picoder");
        skill(&repo.join(".agents/skills"), "deploy", "repo agents (shadowed)");
        skill(&repo.join(".agents/skills"), "changelog", "repo agents");
        skill(&t.join(".agents/skills"), "outside", "above the repo: never read");
        skill(&user_cfg.join("skills"), "changelog", "user (shadowed)");
        skill(&home.join(".claude/skills"), "review", "user claude");

        let (project, user) = skill_roots(&cwd, &user_cfg, Some(&home));
        assert!(project.contains(&repo.join(".agents/skills")) && !project.contains(&t.join(".agents/skills")));
        let mut all = project;
        all.extend(user);
        let got: Vec<(String, String)> = skills_in(&all).into_iter().map(|s| (s.name, s.description)).collect();
        assert_eq!(
            got,
            [
                ("deploy".into(), "project picoder".into()),
                ("changelog".into(), "repo agents".into()),
                ("review".into(), "user claude".into()),
            ]
        );
        let _ = fs::remove_dir_all(&t);
    }

    #[test]
    fn folded_yaml_descriptions_and_quotes_are_read() {
        let c = parse(
            "x",
            "---\nname: \"Release Notes\"\ndescription: >\n  Write release notes\n  in our format.\nlicense: MIT\n---\nDo it.",
            Path::new("SKILL.md"),
        )
        .unwrap();
        assert_eq!(c.name, "release-notes");
        assert_eq!(c.description, "Write release notes in our format.");
        let plain = parse("y", "---\ndescription: one line\n  that wraps\n---\nBody", Path::new("SKILL.md")).unwrap();
        assert_eq!(plain.description, "one line that wraps");
    }

    #[test]
    fn the_prompt_lists_names_and_descriptions_within_a_budget() {
        assert!(skills_prompt(&[]).is_none());
        let many: Vec<Skill> = (0..100)
            .map(|i| Skill { name: format!("s{i}"), description: "d".repeat(300), dir: PathBuf::new() })
            .collect();
        let p = skills_prompt(&many).unwrap();
        assert!(p.len() <= SKILL_LIST_CHARS + 100, "{}", p.len());
        assert!(p.contains("- s0: ") && !p.contains(&"d".repeat(SKILL_DESC_MAX + 1)));
        assert!(p.contains("more (an unknown name"), "the rest are counted, not dropped silently");
    }

    #[test]
    fn the_skill_tool_returns_the_body_and_files_or_lists_the_options() {
        let t = scratch("tool");
        let root = t.join("skills");
        skill(&root, "changelog", "Format changelog entries");
        fs::create_dir_all(root.join("changelog/references")).unwrap();
        fs::write(root.join("changelog/references/format.md"), "x").unwrap();
        fs::write(root.join("changelog/.hidden"), "x").unwrap();
        let skills = skills_in(&[root.clone()]);
        let out = skill_text(&skills, "Changelog");
        assert!(out.contains("Body of changelog."), "{out}");
        assert!(!out.contains("description:"), "frontmatter is stripped: {out}");
        let file = root.join("changelog/references/format.md").display().to_string();
        assert!(out.contains(&file), "files are listed by absolute path: {out}");
        assert!(!out.contains(".hidden"), "hidden files aren't listed: {out}");
        let err = skill_text(&skills, "nope");
        assert!(err.starts_with("ERROR") && err.contains("Available skills: changelog"), "{err}");
        assert!(skill_text(&[], "x").contains("no skills are installed"));
        let _ = fs::remove_dir_all(&t);
    }
}

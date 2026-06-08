//! Minimal unified diff for edit/write previews. Output uses `+`/`-`/` `
//! line prefixes and `...` between hunks; the UI colorizes by prefix.

use similar::{ChangeTag, TextDiff};

pub fn unified(old: &str, new: &str, max_lines: usize) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    let mut emitted = 0usize;
    for (i, group) in diff.grouped_ops(3).iter().enumerate() {
        if i > 0 {
            out.push_str("...\n");
        }
        for op in group {
            for change in diff.iter_changes(op) {
                if emitted >= max_lines {
                    out.push_str("... (diff truncated)\n");
                    return out;
                }
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                let value = change.value();
                out.push_str(sign);
                out.push(' ');
                out.push_str(value.strip_suffix('\n').unwrap_or(value));
                out.push('\n');
                emitted += 1;
            }
        }
    }
    if out.is_empty() {
        out.push_str("(no changes)\n");
    }
    out
}

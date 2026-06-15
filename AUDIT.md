# picode Audit Report

A comprehensive audit comparing picode to Codex CLI, Claude Code, and Agy,
and identifying concrete bugs, rough edges, and missing features.

## Feature Comparison

picode's 20 built-in tools are competitive. Unique features no competitor has:
`multi_edit` (atomic batched edits), `todo` (visible plan), `remember`/`recall`,
auto-load of all 4 project file formats (PICODE.md/AGENTS.md/CLAUDE.md/GEMINI.md),
apple-rainbow launch banner with WiFi/IP, `/config` interactive settings panel,
ASCII fallback for Linux framebuffer console, account balance display, sudo
askpass bridge. Runs on a Pi Zero W (512MB RAM) with a 2.5MB static binary —
zero competitors do this.

## Critical Bugs

| # | Bug | Location |
|---|-----|----------|
| 1 | Session resume broken: `DefaultHasher` randomized per process | config.rs:160-168 |
| 2 | One-shot deadlocks on ask_user/sudo: events silently discarded | main.rs:314 |
| 3 | Sub-agent panic poisons parent worker state permanently | agent.rs:506-536 |

## High-Impact Issues

| # | Issue | Location |
|---|-------|----------|
| 4 | DuckDuckGo scraping regex fragile; silent "(no results)" on breakage | tools.rs:736-745 |
| 5 | bash() timeout only kills sh, not grandchildren (no process group) | tools.rs:67-68 |
| 6 | bash() timeout TOCTOU race: kill targets potentially recycled PID | tools.rs:67-68 |
| 7 | read_file loads entire file before truncating (OOM on large files) | tools.rs:193 |
| 8 | No read timeout on streaming SSE body (hang on stalled server) | api.rs:295-340 |
| 9 | Git subprocesses have no timeout (hang on NFS/credentials) | tools.rs:356-440 |
| 10 | web_fetch no per-byte progress timeout (slow-loris) | tools.rs:632-648 |

## Medium Issues

| # | Issue | Location |
|---|-------|----------|
| 11 | Background job table never evicts entries (memory leak) | tools.rs:89-92 |
| 12 | /help lists only 7 of 13+ slash commands | ui.rs:1746 |
| 13 | One-shot discards reasoning/diff tokens (blank terminal) | main.rs:314 |
| 14 | Paste O(n²) performance; multiline flattened to single line | ui.rs:772-781 |
| 15 | Merge conflicts cause silent commit failure | tools.rs:400-408 |
| 16 | Sub-agents can call ask_user despite prompt saying they can't | agent.rs:921 + tools.rs:666 |
| 17 | task tool advertised to sub-agents but always fails | agent.rs:503-536 |
| 18 | Esc 50ms delay in composer feels sluggish | ui.rs:1373 |
| 19 | html_to_text regex compiled from scratch every call | tools.rs:663-678 |
| 20 | Concurrent picode instances can corrupt memory.md | tools.rs:858-870 |
| 21 | edit_file Unicode normalization mismatch | tools.rs:261-273 |
| 22 | Setup wizard has no validation of inputs | config.rs:384-423 |
| 23 | Symlinks transparently followed on writes | tools.rs:246-259 |

## Polish / Papercuts

| # | Issue | Location |
|---|-------|----------|
| 24 | Transcript trimming (4000 lines) is silent | ui.rs:603-607 |
| 25 | No "↓ new messages" indicator when scrolled up | ui.rs:1591-1601 |
| 26 | last_ctrl_c timer never expires; 2nd press shows prompt again after 2s gap | ui.rs:1366-1376 |
| 27 | Ctrl+D on non-empty composer does nothing | ui.rs:1398 |
| 28 | Slash-suggestion ranking scans full history per keystroke | ui.rs:801-823 |
| 29 | final_text in one-shot can be stale (empty last reply) | main.rs:277-284 |
| 30 | --banner flag can swallow next positional as theme name | main.rs:108 |
| 31 | PICODE.md loading reads entire file before truncating | main.rs:222 |
| 32 | MCP server crashes have no recovery/restart logic | mcp.rs |
| 33 | Compaction summary loses image context | agent.rs:854-878 |
| 34 | / not showing suggestions while agent is processing | ui.rs |

## Missing Features (vs Competitors)

- Session forking / naming / parallel worktrees
- JSON output / schema for scripting
- stdin piping
- CI/CD integration (GitHub Actions)
- Custom skills / slash commands
- Hooks (pre/post tool)
- MCP server mode
- OS-native sandboxing (Seatbelt/Bubblewrap)
- Execution policy rules
- Compaction focus hint
- Thrashing protection
- Built-in code review command

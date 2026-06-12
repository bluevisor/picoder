# picode

A tiny full-screen **agentic coding CLI** written in Rust, small enough to run
on a **Raspberry Pi Zero W** (ARMv6, single core, ~512 MB RAM) — and equally at
home on a Pi 5 or your laptop. It talks to any OpenAI-compatible chat API
(default: DeepSeek) and drives a Codex / Claude-Code-style tool loop inside a
[ratatui](https://ratatui.rs) terminal UI.

The whole thing is one ~2.5 MB statically linked binary with no runtime
dependencies.

## Features

- **Agentic tool loop** — `bash` (with timeout, or detached `background` jobs +
  `bash_output` / `bash_kill`), `read` / `write` / `edit` / `list`, `multi_edit`
  (a batch of edits across files in one approval), `grep` (regex), `glob`,
  `web_fetch`, `web_search`, `todo` (a visible plan), `ask_user`, `view_image`,
  and `remember` / `recall` memory.
- **Git auto-checkpoint** — every successful edit is committed to the
  working-directory repo (`auto_commit`, on by default), so each change is
  restorable; recent git history is fed into context as a clue.
- **Sub-agents** — the `task` tool delegates a self-contained job to a fresh
  agent with its own context and the same tools; only its report comes back.
- **MCP** — stdio MCP servers from `mcp_servers` in `config.json` are launched
  at start; their tools show up as `mcp__<server>__<tool>` (`/mcp` lists them).
- **Images** — `@image.png` attaches as a base64 data URI and `view_image`
  loads one from disk, sent as OpenAI multimodal content parts.
- **Streaming TUI** — a Claude-style composer with a reverse-block cursor,
  live token streaming, and colored unified diffs previewed before every
  write/edit.
- **Context compaction** — `/compact` summarizes older turns to free the
  window; triggers automatically at 80% full.
- **Queued input** — keep typing while the agent works; Enter queues messages
  that send as turns finish.
- **One-shot `--output`** — `picode "task" -o out.md` writes the final reply to
  disk after the run.
- **Permission modes** (`Shift+Tab`) — *ask* / *bypass* / *plan* (read-only).
- **Context files** — auto-loads `PICODE.md` / `AGENTS.md` / `CLAUDE.md` /
  `GEMINI.md` from the working directory.
- **Sessions** — persisted per working directory; resume with `picode --continue`.
- **Composer niceties** — a `/` command palette (suggestions ranked by your
  usage; ↑/↓ select, Tab fills, Enter runs), `@file` attach, Tab autocomplete
  (commands + paths), history, word-skip and word-delete, code-block
  highlighting.
- **Status bar** — model · session tokens + $ cost · context-window bar ·
  account balance.
- **Settings panel** (`/config`) — provider preset, base URL, model, API key,
  thinking mode, default permission mode, auto-commit, theme, and context
  window, edited in-TUI; changes apply live and persist to `config.json`.
- **Themes** (`/theme`) — `default`, `apple2` (green phosphor), `msdos`.
- **Tuned for the Pi framebuffer console** — ASCII fallback and clear-on-exit
  under `TERM=linux`, plus a launch banner with live MEM / Wi-Fi / IP.

## Install / build

picode is cross-compiled from macOS to static musl binaries (the Pi can't
compile Rust itself). Two targets are produced: **ARMv6** for the Pi Zero W and
**aarch64** for the Pi 5 — `deploy` picks the matching one per host.

```sh
# one-time toolchain setup
brew install messense/macos-cross-toolchains/arm-unknown-linux-musleabihf
brew install messense/macos-cross-toolchains/aarch64-unknown-linux-musl
rustup target add arm-unknown-linux-musleabihf
rustup target add aarch64-unknown-linux-musl

./build.sh                        # build both targets
./build.sh deploy                 # build + install the right binary to every host in PICODE_HOSTS
PICODE_HOSTS="pi@host-a pi@host-b" ./build.sh deploy   # custom targets
PI=user@host ./build.sh deploy    # build + install to a single host
./build.sh pull                   # pull on-device self-edits back to the Mac
```

Deploy targets are configurable via the `PICODE_HOSTS` env var (space-separated
`user@host` list) or `PI=user@host` for a single host — set `PICODE_HOSTS` in
your shell profile to make it permanent. `deploy` queries each host's `uname -m`
and installs the matching ~2.5 MB static binary to `~/.local/bin/picode`.

## Configuration

On first run, picode walks you through provider, model, and API key. State lives
in `~/.config/picode/`:

```
config.json   provider / model / key (+ optional mcp_servers)
memory.md     remember/recall store
history       composer history
sessions/     per-directory session transcripts
```

To expose [MCP](https://modelcontextprotocol.io) tools, add an `mcp_servers`
block to `config.json`; each entry is launched over stdio at start and its
tools appear as `mcp__<server>__<tool>`:

```json
"mcp_servers": {
  "filesystem": {
    "command": "npx",
    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/pi"]
  }
}
```

## Keyboard

| Key | Action |
| --- | --- |
| `Enter` | send |
| `Shift+Tab` | cycle permission mode |
| `↑` / `↓` | history |
| `Alt`/`Ctrl`/`Cmd + ←/→` | word / line motion |
| `Option`/`Alt + Backspace` | delete word backward |
| `Option`/`Alt + Delete` | delete word forward |
| `Tab` | autocomplete commands / paths |
| `PgUp` / `PgDn` | scroll transcript |
| `Esc` | interrupt turn / clear line |
| `Ctrl+C` | quit |

picode enables the [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
when the terminal supports it, so modified keys are reported unambiguously.
Without it, some terminals (e.g. Warp in full-screen mode) flatten
`Option+Backspace` to a plain Backspace.

## Architecture

A long-lived **worker thread** owns the message history and runs the blocking
HTTP + tool loop. The **UI thread** renders with ratatui and exchanges user
input / approvals over mpsc channels; the worker streams back tokens, tool
events, diffs, and approval requests. This keeps the UI responsive and lets
`Esc` interrupt a turn mid-flight.

See [`PICODE.md`](PICODE.md) for the per-file source map and deeper notes.

## License

MIT

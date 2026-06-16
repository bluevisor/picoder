# picode

A tiny full-screen agentic coding CLI written in Rust, built to run **on a
Raspberry Pi Zero W** (ARMv6, single core, ~512MB RAM). It talks to any
OpenAI-compatible chat API (default: DeepSeek `deepseek-v4-pro`) and drives a
Codex/Claude-Code-style tool loop with a ratatui terminal UI.

This is picode's own source. picode can read and edit these files, but **the Pi
cannot compile Rust** — see "Building" below.

## Layout

```
Cargo.toml            deps + release + jetson profiles (static musl, size-optimized)
.cargo/config.toml    cross-linkers for ARMv6/ARMv7/aarch64 musl + aarch64 gnu (Jetson)
build.sh              cross-compile (and `build.sh deploy` to install on the Pis)
src/
  main.rs             CLI args, first-run setup, one-shot mode (+ --output), session/context wiring
  config.rs           ~/.config/picode/config.json (incl. mcp_servers), setup wizard, session paths
  api.rs              OpenAI-compatible streaming SSE, tool schema, multimodal parts, retry, list_models
  tools.rs            tool impls: bash (+ background), read/write/edit/list, grep, glob,
                      web_fetch, web_search, todo, view_image, remember, recall
  agent.rs            worker thread: owns the conversation, runs the model/tool loop, sub-agents
  mcp.rs              stdio MCP client: spawn servers, JSON-RPC handshake, tools/list + tools/call
  ui.rs               ratatui full-screen TUI: transcript, composer, status bar
  diff.rs             unified diff for edit/write previews
  askpass.rs          sudo password support: askpass helper + in-TUI masked prompt
  sysinfo.rs          /proc & /sys probes: board model, WiFi SSID, IP, RAM, cores
```

## Architecture

A long-lived **worker thread** (`agent.rs`) owns the message history and runs the
blocking HTTP + tool loop. The **UI thread** (`ui.rs`) renders with ratatui and
sends user input / approvals over mpsc channels; the worker streams back tokens,
tool events, diffs, and approval requests. This keeps the UI responsive and lets
`Esc` interrupt a turn.

## Features

- Tools: bash (timeout, or `background` jobs + bash_output/bash_kill),
  read/write/edit/list, multi_edit (batch edits across files), grep (regex),
  glob, web_fetch (URL → readable text), web_search (DuckDuckGo), todo (visible
  plan), ask_user, task (sub-agent), view_image, remember/recall — plus any MCP tools.
- Git auto-checkpoint: every successful edit is committed to the working-dir
  repo (`auto_commit`, on by default; no-op outside a repo). Recent git history
  is loaded into context at startup so the agent can use `git log`/`show`/`diff`
  as a clue and `git revert` to undo a bad edit.
- Sub-agents: the `task` tool delegates a self-contained job to a fresh agent
  with its own context and the same tools; only its final report returns.
- MCP: stdio servers from `mcp_servers` in config.json are launched at start
  and their tools advertised as `mcp__<server>__<tool>` (`/mcp` lists them).
- Images: `@image.png` attaches as a base64 data URI, and `view_image` loads
  one from disk — both sent as OpenAI multimodal content parts.
- Streaming with a Claude-style composer (`›`, reverse-block cursor, placeholder).
- Context compaction: `/compact` summarizes older turns (keeping the system
  prefix and latest exchange); auto-triggers at 80% of the context window.
- Queued input: the composer stays live while the agent works — Enter queues
  messages that send in order as turns finish (Esc interrupts and restores them).
- One-shot `--output FILE` writes the final reply to disk after the run.
- Status bar: model · session tokens + $ cost · context-window bar · account balance.
- Permission modes via Shift+Tab: ask / bypass / plan (read-only); colored diff before write/edit.
- Auto-loads `PICODE.md`/`AGENTS.md`/`CLAUDE.md`/`GEMINI.md` as context.
- Session persistence + resume (`picode --continue`, per working directory).
- Composer: typing `/` opens a command palette (suggestions ranked by your
  usage history; ↑/↓ select, Tab fills, Enter runs), `@file` attach, Tab
  autocomplete (commands + paths), history,
  word-skip (Alt/Ctrl/Cmd + arrows), word-delete (Option/Alt+Backspace and
  Option/Alt+Delete), code-block highlighting.
- Enables the Kitty keyboard protocol when the terminal supports it, so
  modified keys (e.g. Option+Backspace) are reported unambiguously instead of
  being flattened to a bare Backspace by terminals like Warp.
- ASCII fallback + clear-on-exit for the Pi's framebuffer console (`TERM=linux`).
- Launch banner: rainbow-color PICODE logo + live status (MEM, WiFi SSID + IP).
- Themes (`/theme`, numbered picker): `Default`, `Apple ][` (green phosphor, `] ▒`),
  `MSDOS` (gray, `C:\>`), `macOS` (dark mode system colors, `~ `),
  `SUN` (amber/gold, `sun% `), `NeXT` (platinum monochrome, `NeXT>`),
  `SGI` (indigo/teal IRIX, `irix# `); persisted in config. Theme sets colors, prompt, cursor.
- `/model` opens an interactive picker over the provider's model list —
  type to filter (handy for OpenRouter's hundreds), ↑/↓ + Enter to select;
  `/model <id|number>` still sets directly.
- Slash commands: `/model /auto /reset /compact /config /mcp /memory /theme /init /new /clear /help /exit`.
- `/config`: interactive settings panel — provider preset (deepseek/openai/anthropic/groq/openrouter/google),
  base URL, model, API key (masked), thinking mode (DeepSeek-style
  `"thinking":{"type":"enabled"}` request field; off by default), default
  permission mode (ask/bypass/plan — applies live and saves for new sessions),
  auto-commit, theme, context window. Changes apply immediately and persist.

## Building (must be done on the Mac — the Pi can't compile this)

Cross-compiled from macOS to **three** static musl targets: ARMv6 for the Pi
Zero W, ARMv7 for 32-bit Pi OS on the Pi 2/3/4 (`armv7l`), and aarch64 for the
Pi 5 (a 64-bit box — running the 32-bit ARMv6 build under compat is fragile and
can segfault, so it gets a native binary). The ARMv7 build reuses the ARMv6
musl gcc as linker, so no extra toolchain is needed beyond
`rustup target add armv7-unknown-linux-musleabihf`.

```
# one-time toolchain setup:
brew install messense/macos-cross-toolchains/arm-unknown-linux-musleabihf
brew install messense/macos-cross-toolchains/aarch64-unknown-linux-musl
rustup target add arm-unknown-linux-musleabihf
rustup target add armv7-unknown-linux-musleabihf
rustup target add aarch64-unknown-linux-musl

./build.sh           # build all three targets under target/<triple>/release/picode
./build.sh deploy    # build + install the right binary per host in PICODE_HOSTS
PICODE_HOSTS="pi@a pi@b" ./build.sh deploy   # custom deploy targets
PI=user@host ./build.sh deploy               # install to a single host instead
```

`deploy` queries each host's `uname -m` and installs the matching binary
(`armv6l` → ARMv6, `armv7l` → ARMv7, `aarch64` → aarch64) to
`~/.local/bin/picode`. Hosts come
from the `PICODE_HOSTS` env var (space-separated `user@host`; defaults to the
Pi Zero + Pi 5), or `PI=user@host` for a single host.

`build.sh` sets the `ring` cross-compile env vars (`CC_/AR_/TARGET_CC`) per
target and the linkers come from `.cargo/config.toml`. Each output is a ~2.5MB
fully static binary.

> If you edit the source on the Pi, copy it back to the Mac before rebuilding,
> or those edits won't be in the next build. The Mac copy is canonical.

## Config / state (on the Pi)

`~/.config/picode/` — `config.json` (provider/model/key), `memory.md`,
`history`, `sessions/`.

MCP servers are optional and configured by hand in `config.json`:

```json
{
  "provider": "deepseek",
  "model": "deepseek-v4-pro",
  "api_key": "...",
  "mcp_servers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/pi"],
      "env": {}
    }
  }
}
```

Each server is spawned over stdio at launch; its tools appear as
`mcp__filesystem__<tool>`. `/model` and `/theme` rewrites preserve the block.

## Roadmap / wishlist

- **ui.rs split** (~3100 lines → submodules). A clean split would extract:
  `ui/banner.rs` (banner rendering, ~300 lines), `ui/panels.rs` (config picker,
  model picker, theme picker, `/` palette — ~800 lines), `ui/transcript.rs`
  (~400 lines), and `ui/palette.rs` (Palette + THEMES, ~150 lines). Each
  submodule depends on ratatui/crossterm; they'd be `pub(crate)` re-exported by
  `ui.rs` so the rest of the crate sees no change.

- **Config/environment-driven integration tests**. MCP handshake and web_search
  tests currently `#[ignore]` because they need a live server; a `PICODE_TEST=1`
  gate would run them in CI with a containerized MCP stub.

## Performance / safety notes

- **Transcript render cache** (`ui.rs`). `ensure_display_cache` renders the
  static (non-`live`) transcript into `disp_cache` once and reuses it until the
  content (`tver`, bumped via `dirty()`/`set_palette`) or terminal width
  changes; each frame clones only the visible window. This avoids re-wrapping
  thousands of lines per streamed token on a single-core Pi.
- **Atomic state writes** (`config::atomic_write`). `config.json` and session
  files are written to a per-process temp file, fsync'd, then renamed over the
  target, so a power loss on the SD card never leaves a truncated file.
- **Symmetric symlink refusal** (`tools::deny_symlink`). `read`/`list` refuse
  symlinked paths just like `write`/`edit`; `expand` lexically collapses
  `.`/`..` so an approved path is the path actually used.

## CI

`.github/workflows/ci.yml` runs `cargo test` on the host (hard gate) plus a
`cross build --release` for all three deploy targets (ARMv6 / ARMv7 / aarch64),
so a broken cross-compile is caught in CI rather than at deploy time. Clippy and
`cargo fmt --check` run advisory-only until the tree is clean.

# picode

A tiny full-screen agentic coding CLI written in Rust, built to run **on a
Raspberry Pi Zero W** (ARMv6, single core, ~512MB RAM). It talks to any
OpenAI-compatible chat API (default: DeepSeek `deepseek-v4-pro`) and drives a
Codex/Claude-Code-style tool loop with a ratatui terminal UI.

This is picode's own source. picode can read and edit these files, but **the Pi
cannot compile Rust** — see "Building" below.

## Layout

```
Cargo.toml            deps + release profile (static musl, size-optimized)
.cargo/config.toml    cross-linker for arm-unknown-linux-musleabihf
build.sh              cross-compile (and `build.sh deploy` to install on the Pis)
src/
  main.rs             CLI args, first-run setup, one-shot mode, session/context wiring
  config.rs           ~/.config/picode/config.json, setup wizard, session paths
  api.rs              OpenAI-compatible streaming SSE, tool schema, retry, list_models
  tools.rs            tool impls: bash, read/write/edit/list, grep, glob, remember, recall
  agent.rs            worker thread: owns the conversation, runs the model/tool loop
  ui.rs               ratatui full-screen TUI: transcript, composer, status bar
  diff.rs             unified diff for edit/write previews
```

## Architecture

A long-lived **worker thread** (`agent.rs`) owns the message history and runs the
blocking HTTP + tool loop. The **UI thread** (`ui.rs`) renders with ratatui and
sends user input / approvals over mpsc channels; the worker streams back tokens,
tool events, diffs, and approval requests. This keeps the UI responsive and lets
`Esc` interrupt a turn.

## Features

- Tools: bash (timeout), read/write/edit/list, grep (regex), glob, remember/recall.
- Streaming with a Claude-style composer (`›`, reverse-block cursor, placeholder).
- Status bar: model · session tokens + $ cost · context-window bar · account balance.
- Permission modes via Shift+Tab: ask / bypass / plan (read-only); colored diff before write/edit.
- Auto-loads `PICODE.md`/`AGENTS.md`/`CLAUDE.md`/`GEMINI.md` as context.
- Session persistence + resume (`picode --continue`, per working directory).
- Composer: `@file` attach, Tab autocomplete (commands + paths), history,
  word-skip (Alt/Ctrl/Cmd + arrows), word-delete (Option/Alt+Backspace and
  Option/Alt+Delete), code-block highlighting.
- Enables the Kitty keyboard protocol when the terminal supports it, so
  modified keys (e.g. Option+Backspace) are reported unambiguously instead of
  being flattened to a bare Backspace by terminals like Warp.
- ASCII fallback + clear-on-exit for the Pi's framebuffer console (`TERM=linux`).
- Launch banner: Apple-rainbow PICODE block art + live status (MEM, WiFi SSID + IP).
- Themes (`/theme`, numbered picker): `default`, `apple2` (green phosphor, `] ▒`),
  `msdos` (gray, `C:\>`); persisted in config. Theme sets colors, prompt, cursor.
- Slash commands: `/model /auto /reset /memory /theme /init /clear /help /exit`.

## Building (must be done on the Mac — the Pi can't compile this)

Cross-compiled from macOS to a static musl ARMv6 binary:

```
# one-time toolchain setup:
brew install messense/macos-cross-toolchains/arm-unknown-linux-musleabihf
rustup target add arm-unknown-linux-musleabihf

./build.sh           # build target/arm-unknown-linux-musleabihf/release/picode
./build.sh deploy    # build + install to ALL Pis (Pi Zero + Pi 5) at ~/.local/bin/picode
PI=user@host ./build.sh deploy   # install to a single host instead
```

`deploy` installs to every host in the `PIS` array in `build.sh` (the same
static ARMv6 binary runs on both the Pi Zero W and the Pi 5).

`build.sh` sets the `ring` cross-compile env vars (`CC_/AR_/TARGET_CC`) and the
linker comes from `.cargo/config.toml`. Output is a ~2.5MB fully static binary.

> If you edit the source on the Pi, copy it back to the Mac before rebuilding,
> or those edits won't be in the next build. The Mac copy is canonical.

## Config / state (on the Pi)

`~/.config/picode/` — `config.json` (provider/model/key), `memory.md`,
`history`, `sessions/`.

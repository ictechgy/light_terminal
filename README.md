# Light Terminal (`lterm`)

[한국어](README.ko.md) | English

## TL;DR

- **What** — A Rust-based PTY session daemon with a tmux-compatible shim. Persistent sessions you can detach and reattach by name or pane id.
- **Who it's for** — AI-agent tooling that expects tmux (`oh-my-codex`, `oh-my-claude`) and users running it inside `cmux`.
- **How** — `lterm new` to start, `lterm attach` to (re)connect, `lterm omx` / `lterm omc` for shimmed runs. Inside a session, the `tmux` command resolves to `lterm tmux-compat`.
- **Status** — alpha MVP. A same-user convenience daemon — **not** a sandbox, an escape-sequence sanitizer, or a full tmux replacement.

---

A lightweight terminal session daemon with a tmux-compatible shim, built for AI-agent workflows.

`lterm` is intentionally smaller than tmux. It keeps long-running PTY sessions alive, lets clients detach and reattach at will, forwards terminal escape sequences unchanged, and translates the subset of tmux commands commonly used by oh-my-codex and oh-my-claude tooling.

> **Status:** alpha MVP. Usable for local detached sessions and compatibility testing — not yet a full tmux replacement.
>
> **Security model:** `lterm` is a same-user convenience daemon, not a sandbox. It rejects cross-user Unix-socket peers and uses owner-only runtime directories, but any process running as your OS user should be considered capable of controlling your sessions.

## Why this exists

The project addresses three constraints:

1. **tmux-like persistence and remote access** — sessions run inside a background daemon and can be attached or detached by name or pane id. Remote access is available through `lterm ssh`, provided `lterm` is installed on the remote host.
2. **cmux compatibility** — when running inside cmux, `lterm` preserves OSC notifications, exposes `lterm notify`, and the tmux shim opens worker panes as native cmux splits when possible.
3. **AI tooling support** — `lterm omx`, `lterm omc`, and `lterm install-shim` provide a fake `tmux` command and the `TMUX` / `TMUX_PANE` environment variables that AI tools expect.

cmux compatibility is grounded in cmux's documented behavior: notifications via `cmux notify` and OSC 777 / OSC 99, a Unix-socket/CLI API for workspaces and splits, and a tmux shim that maps tmux commands into native cmux panes.

## Install from this checkout

Requires Rust 1.85 or newer.

```bash
cargo build --release
./target/release/lterm --help
```

For local development:

```bash
cargo run -- --help
```

To expose the tmux shim:

```bash
lterm install-shim
# Add the printed directory to PATH ahead of the real tmux, or eval the helper:
eval "$(lterm env)"
```

## Quick start

**Create a persistent session and attach immediately:**

```bash
lterm new -n api -- npm run dev
```

**Create detached, attach later:**

```bash
lterm new -d -n api -- npm run dev
lterm attach api

# Short aliases — `-a` goes right after `lterm`, separated from the target by a space:
lterm a api
lterm -a api
```

Attached clients render a small blue status bar on the bottom row showing the current session and pane; the PTY is resized to the remaining rows. For the older raw full-terminal attach, use `lterm attach --no-status api`.

**Inspect or send input:**

```bash
lterm ls
lterm ps api
lterm capture api -S=-80
lterm send api 'echo hello' --enter
```

**Stop a session:**

```bash
lterm kill api
```

## AI workflows

**Run Oh My Codex inside a shimmed session:**

```bash
lterm omx team
# Extra omx flags are passed through, e.g.:
lterm omx --madmax --xhigh
```

**Run Oh My Claude similarly:**

```bash
lterm omc team
# The OMC builds tested here reject --xhigh — use --madmax alone unless your
# installed `omc --help` explicitly lists --xhigh.
lterm omc --madmax
```

**Run any command with tmux compatibility enabled:**

```bash
lterm run --tmux -- omx hud --tmux
```

Inside that session, `tmux` resolves to the `lterm tmux-compat` shim. The shim implements the command subset most AI orchestration scripts rely on:

- **Sessions** — `new-session`, `attach-session`, `has-session`, `list-sessions`, `kill-session`
- **Panes** — `split-window`, `list-panes`, `display-message`, `capture-pane`, `send-keys`, `kill-pane`, `resize-pane`
- **Buffers / popups** — `display-popup`, `wait-for`, `load-buffer`, `save-buffer`, `paste-buffer`
- **No-op compatibility** — `select-pane`, `select-layout`, `set-option`, `show-option`

## cmux behavior

When `lterm tmux-compat split-window` detects cmux (via `CMUX_WORKSPACE_ID`, `CMUX_SURFACE_ID`, or a cmux socket), it:

1. Starts a new `lterm` PTY session for the worker command.
2. Asks cmux to create a native split (`cmux new-split right/down`).
3. Sends `lterm attach <pane>` into that split.

This gives cmux a real pane to decorate while `lterm` retains scrollback capture and `send-keys` compatibility.

**Notifications:**

```bash
lterm notify --title 'Task complete' --body 'All checks passed'
```

`lterm notify` first tries `cmux notify`. If that's unavailable, it emits OSC 777 so cmux or another compatible terminal can still surface the notification. Notification fields are stripped of terminal control characters before falling back to OSC.

## Remote access

If `lterm` is installed on a remote machine:

```bash
lterm ssh user@host main
```

This expands to `ssh -t user@host 'lterm attach-or-new main'`. Pass SSH flags after `--`:

```bash
lterm ssh devbox main -- -p 2222 -i ~/.ssh/id_ed25519
```

## Architecture

- **Daemon** — one Unix socket per user under `$XDG_RUNTIME_DIR`, with an owner-only fallback under `/tmp`.
- **PTY sessions** — spawned via `portable-pty`, backed by ring-buffer scrollback.
- **Attach protocol** — the CLI sends JSON over the Unix socket, optionally reserves the bottom row for a local status bar, then streams PTY bytes.
- **tmux shim** — a small shell script named `tmux` forwards commands to `lterm tmux-compat`.
- **cmux bridge** — optional; uses the cmux CLI when detected.

## Security notes

**Terminal output is forwarded as-is.** `lterm attach` passes PTY bytes through so full-screen terminal programs and cmux/OSC notifications keep working. The local status bar is purely a client-side decoration; use `--no-status` for a fully raw terminal surface. Untrusted child programs can still emit terminal escape sequences to an attached terminal — exactly as under tmux/screen. **Do not use `lterm` as an escape-sequence sanitizer or sandbox.**

**Capture output is sanitized for human/AI consumption.** `lterm capture` and `tmux capture-pane` strip common terminal control sequences before printing scrollback.

**Process visibility.** `lterm ps [session]` shows the process tree rooted at each session child, so long-running Codex/OMX/MCP subprocess buildup stays visible before it becomes a memory-leak surprise. The system `ps` is invoked by absolute path, and malformed process rows are skipped rather than guessed at.

**Socket location.** Custom `LTERM_SOCKET` paths must live in an owner-only directory. Prefer `LTERM_RUNTIME_DIR` when you need an isolated socket location.

**Popup commands.** `tmux-compat display-popup` runs the requested command through the user's shell to preserve tmux-like behavior. **Do not pass untrusted popup commands.**

**Build reproducibility.** Use the committed lockfile for release builds: `cargo build --release --locked`. The current lockfile pins `serde_json 1.0.149`. Its transitive `zmij` dependency is part of the official serde_json package metadata on docs.rs/crates.io — not a local vendored crate.

## Current limitations

- Session persistence lasts only while the daemon and host are alive — reboot/process-state restore is not implemented.
- Outside cmux, `split-window` creates additional managed PTY sessions but does not draw a tiled in-terminal UI.
- This is a compatibility subset, not a full tmux server. Scripts using advanced tmux formats or options may need additional shim commands.
- cmux pane capture is handled through `lterm` sessions, not cmux scrollback APIs.
- The daemon authenticates local clients via OS peer credentials and owner-only socket paths — there are no per-session ACLs yet.
- Session shutdown uses verified process-group signaling, so child trees like `shell → OMX → Codex → MCP` are cleaned up together when possible. Processes that intentionally detach into a different session/process group can outlive `lterm kill`; inspect them with `lterm ps` or OS process tools.

## Development

```bash
cargo fmt
cargo test
cargo build --locked
```

Use isolated runtime directories for manual testing:

```bash
TMP=$(mktemp -d)
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- new --name test -- sh -lc 'echo hi; sleep 10'
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- capture test -S=-20
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- shutdown
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

# Light Terminal (`lterm`)

`lterm` is a lightweight terminal session daemon with a tmux-compatible shim for AI-agent workflows. It is intentionally smaller than tmux: it keeps long-running PTY sessions alive, lets clients detach/reattach, forwards terminal escape sequences unchanged, and translates the subset of tmux commands commonly used by oh-my-codex / oh-my-claude style tooling.

> Status: alpha MVP. It is usable for local detached sessions and compatibility testing, but it is not a full tmux replacement yet.

> Security model: `lterm` is a same-user convenience daemon, not a sandbox. It rejects cross-user Unix-socket peers and uses owner-only runtime directories, but any process that can run as your OS user should be treated as capable of controlling your sessions.

## Why this exists

The project targets three constraints:

1. **tmux-like persistence and remote access** — sessions run in a background daemon and can be attached/detached by name or pane id. Remote access is available through `lterm ssh`, assuming `lterm` is installed on the remote host.
2. **cmux compatibility** — when running inside cmux, `lterm` preserves OSC notifications, exposes `lterm notify`, and the tmux shim opens worker panes as native cmux splits when possible.
3. **AI tooling support** — `lterm omx`, `lterm omc`, and `lterm install-shim` provide a fake `tmux` command plus `TMUX` / `TMUX_PANE` environment variables for tools that expect tmux.

cmux compatibility is based on cmux's documented behavior: cmux supports notifications through `cmux notify` and OSC 777 / OSC 99, exposes a Unix-socket/CLI API for workspaces and splits, and its own oh-my-codex integration uses a tmux shim that maps tmux commands into cmux-native panes.

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
# Add the printed directory to PATH before the real tmux, or eval:
eval "$(lterm env)"
```

## Quick start

Create and detach a persistent session:

```bash
lterm new --name api -- npm run dev
```

Attach later:

```bash
lterm attach api
```

Inspect or send input:

```bash
lterm list
lterm ps api
lterm capture api -S=-80
lterm send api 'echo hello' --enter
```

Stop it:

```bash
lterm kill api
```

## AI workflows

Run Oh My Codex inside a shimmed session:

```bash
lterm omx team
```

Run Oh My Claude similarly:

```bash
lterm omc team
```

Or run any command with tmux compatibility enabled:

```bash
lterm run --tmux -- omx hud --tmux
```

Inside that session, `tmux` resolves to an `lterm tmux-compat` shim. The shim currently implements the common command subset used by AI orchestration scripts:

- `new-session`, `attach-session`, `has-session`, `list-sessions`, `kill-session`
- `split-window`, `list-panes`, `display-message`, `capture-pane`, `send-keys`, `kill-pane`, `resize-pane`
- no-op compatibility for `select-pane`, `select-layout`, `set-option`, `show-option`
- `display-popup`, `wait-for`, `load-buffer`, `save-buffer`, `paste-buffer`

## cmux behavior

When `lterm tmux-compat split-window` detects cmux (`CMUX_WORKSPACE_ID`, `CMUX_SURFACE_ID`, or a cmux socket), it:

1. starts a new `lterm` PTY session for the worker command,
2. asks cmux to create a native split (`cmux new-split right/down`), and
3. sends `lterm attach <pane>` into that split.

That gives cmux a real pane to decorate, while `lterm` still owns scrollback capture and `send-keys` for compatibility.

Notifications:

```bash
lterm notify --title 'Task complete' --body 'All checks passed'
```

`lterm notify` first tries `cmux notify`; if that is unavailable, it emits OSC 777 so cmux or another compatible terminal can still surface the notification. Notification fields strip terminal control characters before emitting fallback OSC.

## Remote access

If `lterm` is installed on a remote machine:

```bash
lterm ssh user@host main
```

This runs `ssh -t user@host 'lterm attach-or-new main'`. SSH flags can be passed after `--`:

```bash
lterm ssh devbox main -- -p 2222 -i ~/.ssh/id_ed25519
```

## Architecture

- **Daemon:** one Unix socket per user under `$XDG_RUNTIME_DIR` or an owner-only fallback under `/tmp`.
- **PTY sessions:** spawned via `portable-pty`, with ring-buffer scrollback.
- **Attach protocol:** the CLI sends JSON over the Unix socket, then streams raw PTY bytes.
- **tmux shim:** a small shell script named `tmux` forwards commands to `lterm tmux-compat`.
- **cmux bridge:** optional; uses cmux CLI when detected.

## Security notes

- `lterm attach` intentionally forwards raw PTY bytes so full-screen terminal programs and cmux/OSC notifications keep working. Untrusted child programs can still emit terminal escape sequences to an attached terminal, just like under tmux/screen. Do not use `lterm` as an escape-sequence sanitizer or sandbox.
- `lterm capture` and `tmux capture-pane` strip common terminal control sequences by default before printing captured scrollback for humans or AI tools.
- `lterm ps [session]` shows the process tree rooted at each session child so long-running Codex/OMX/MCP subprocess buildup is visible before it becomes a memory leak surprise.
- Custom `LTERM_SOCKET` paths must live in an owner-only directory. Prefer `LTERM_RUNTIME_DIR` when you need an isolated socket location.
- `tmux-compat display-popup` runs the requested command through the user's shell to preserve tmux-like behavior; do not pass untrusted popup commands.
- Release builds should use the committed lockfile: `cargo build --release --locked`. The current lockfile includes `serde_json 1.0.149`; its `zmij` dependency is listed by the official serde_json package metadata on docs.rs/crates.io.

## Current limitations

- Session persistence lasts while the daemon and host are alive. Reboot/process-state restore is not implemented yet.
- Outside cmux, `split-window` creates additional managed PTY sessions but does not draw a tiled in-terminal UI.
- This is a compatibility subset, not a full tmux server. Scripts using advanced tmux formats/options may need more shim commands.
- cmux pane capture is handled through `lterm` sessions, not cmux scrollback APIs.
- The daemon currently authenticates local clients by OS peer credentials and owner-only socket paths, not by per-session ACLs.
- Session shutdown uses process-group signaling so child trees such as shells -> OMX -> Codex -> MCP servers are cleaned up together when possible.

## Development

```bash
cargo fmt
cargo test
cargo build --locked
```

Use isolated runtime directories for manual tests:

```bash
TMP=$(mktemp -d)
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- new --name test -- 'echo hi; sleep 10'
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- capture test -S=-20
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- shutdown
```

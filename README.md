# Light Terminal (`lterm`)

[한국어](README.ko.md) | English

## TL;DR

- **What** — A persistent terminal session daemon (like tmux, but smaller) with a tmux-compatible command layer for AI agent tooling. Detach and reattach by name or pane id.
- **Who it's for** — Terminal-first coding agents such as Claude Code, Codex CLI, Gemini CLI, `oh-my-codex` / `oh-my-claude`, and users running them inside `cmux`.
- **How** — `lterm start` to create, `lterm resume` to (re)connect, `lterm agent <profile>` / `lterm claude` / `lterm codex` / `lterm gemini` for shimmed agent runs. Inside a tmux-enabled session, the `tmux` command resolves to `lterm tmux-compat`.
- **Status** — alpha MVP. A same-user convenience daemon — **not** a sandbox, an escape-sequence sanitizer, or a full tmux replacement.

---

`lterm` is intentionally smaller than tmux. It keeps long-running PTY sessions alive, lets clients detach and reattach at will, forwards terminal escape sequences unchanged, and translates the subset of tmux commands commonly used by terminal-first agent tooling.

> **Security model:** `lterm` is a same-user convenience daemon, not a sandbox. It rejects cross-user Unix-socket peers and uses owner-only runtime directories, but any process running as your OS user should be considered capable of controlling your sessions.

## Why this exists

The project addresses three constraints:

1. **tmux-like persistence and remote access** — sessions run inside a background daemon and can be attached or detached by name or pane id. Remote access is available through `lterm ssh`, provided `lterm` is installed on the remote host.
2. **cmux compatibility** — when running inside cmux, `lterm` preserves OSC notifications, exposes `lterm notify`, and the tmux shim opens worker panes as native cmux splits when possible.
3. **AI tooling support** — `lterm agent <profile>`, `lterm claude`, `lterm codex`, `lterm gemini`, `lterm omx`, `lterm omc`, and `lterm install-shim` provide a fake `tmux` command and the `TMUX` / `TMUX_PANE` environment variables that agent tools expect.

cmux compatibility is grounded in cmux's documented behavior: notifications via `cmux notify` and OSC 777 / OSC 99, a Unix-socket/CLI API for workspaces and splits, and a tmux shim that maps tmux commands into native cmux panes.

## Install

With Homebrew:

```bash
brew install ictechgy/tap/lterm
```

With npm on supported macOS/Linux platforms:

```bash
npm install -g @ictechgy/lterm
```

Homebrew and npm both install the `lterm` command on your `PATH`; verify with `lterm --version`.

With Cargo from GitHub (replace `v0.1.0` with the latest release tag when newer versions are available):

```bash
cargo install --git https://github.com/ictechgy/light_terminal --tag v0.1.0
```

From this checkout, use Rust 1.85 or newer:

```bash
cargo build --release --locked
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
lterm start -n api -- npm run dev
```

**Create detached, attach later:**

```bash
lterm start -d -n api -- npm run dev
lterm resume api

# Compatibility names remain available:
lterm attach api
lterm a api
# `-a` goes right after `lterm`, separated from the target by a space.
lterm -a api
```

**Agent-terminal command vocabulary:**

| Task | General command | Compatibility names |
| --- | --- | --- |
| Start a persistent process | `lterm start -n api -- npm run dev` | `new` |
| Run a command with tmux compatibility enabled | `lterm run -- codex exec "summarize"` | None (`--no-tmux` opts out) |
| Open or create a session | `lterm open main` | `attach-or-new` |
| Resume an existing session | `lterm resume api` | `attach`, `a`, `-a` |
| List sessions | `lterm sessions` | `list`, `ls` |
| Inspect process trees | `lterm processes api --json` | `ps` |
| Rename a session | `lterm rename api api-renamed` | None |
| Read sanitized scrollback | `lterm logs api --start=-80` | `capture` |
| Write input to a PTY | `lterm input api 'echo hello' --enter` | `send` |
| Stop a session | `lterm close api` | `kill` |
| Run the background daemon explicitly | `lterm daemon` | None |
| Stop the daemon and all sessions | `lterm shutdown` | None |

Agent and shim utilities are also product CLI commands, not tmux aliases:

| Task | Product command | Compatibility boundary |
| --- | --- | --- |
| Launch a profiled agent session | `lterm agent claude -- --help` | Sibling shortcuts: `lterm claude`, `lterm codex`, `lterm gemini`, `lterm omx`, `lterm omc` |
| Inspect available agent profiles | `lterm agents --json` | PATH availability probe at command runtime |
| Install the `tmux` compatibility shim | `lterm install-shim` | Creates a shim that forwards to `lterm tmux-compat` |
| Print shell exports for tmux compatibility | `eval "$(lterm env)"` | Emits trusted `export` lines that prepend the shim dir to `$PATH` |
| Send a cmux-friendly notification | `lterm notify --title 'Done' --body 'Tests passed'` | OSC 777 fallback strips terminal controls while preserving Unicode text |
| Attach to a remote host | `lterm ssh user@host main` | Use trusted hosts; SSH handles host-key checks, and remote PTY bytes pass through without sanitization |
| Call the tmux shim namespace directly | `lterm tmux-compat list-commands` | Compatibility namespace, not a product alias table |

Use `eval "$(lterm env)"` only when you trust the `lterm` binary on your `PATH`.
It emits fixed `export` lines that prepend the shim directory to `$PATH`.

`lterm ssh` forwards remote PTY bytes to the local terminal without sanitizing
terminal control sequences, so a compromised remote can drive terminal features
that your local emulator permits: OSC 52 clipboard writes, OSC 8 hyperlinks,
window/title changes, cursor or screen manipulation, bracketed paste toggles, and
any emulator-specific escape handling. Treat it like direct `ssh` to a trusted
host and configure terminal features accordingly. "cmux-friendly" notification
means the fallback path emits the OSC 777 notification format that cmux watches.
The OSC 777 fallback sanitizer protects protocol framing; it does not normalize
Unicode bidi, format, or zero-width characters inside trusted title/body text.

Compatibility names are subcommands unless shown as a leading flag: `-a` is the legacy shortcut form and must be used as `lterm -a <target>`.

This table is the product CLI surface for humans and agents. `lterm tmux-compat ...` is a separate shim namespace for scripts that already speak tmux; not every product command has a tmux-compatible spelling. Use `lterm tmux-compat list-commands` to inspect the supported shim subset at runtime.

`lterm sessions` hides child panes by default, preserves the original first five tab-separated columns (`name`, `pane`, `alive`, `cwd`, `command`), then appends attach state (`attached` / `detached`) and parent pane (`-` or a pane id). The compatibility names `lterm list` and `lterm ls` keep the same output shape. Attached clients render a small blue status bar on the bottom row showing the current session and pane; the PTY is resized to the remaining rows. For the older raw full-terminal resume, use `lterm resume --no-status api` (or compatibility name `lterm attach --no-status api`), or set `LTERM_NO_STATUS=1` / `LTERM_STATUS=0` for clients whose status-line handling conflicts with lterm.

`lterm rename <target> <new-name>` changes only lterm metadata and target lookup; it does not restart the PTY or mutate the child process environment. Renaming a session to its current name is a no-op success, while renaming over a different in-use name fails with a conflict error.

Set `LTERM_STATUS_STYLE=full` or `LTERM_STATUS_STYLE=minimal` to choose the visual style. `full` (default for local terminals) shows black text on a bright-blue background; `minimal` drops all SGR colors in favor of plain text. SSH sessions (detected via `SSH_CONNECTION`, `SSH_CLIENT`, or `SSH_TTY`) default to `minimal` to avoid color-mapping issues on mobile SSH clients like Termius.

When the attached PTY enters the alternate screen buffer (e.g. `vim`, `less`, `htop` via `\x1b[?1049h`), lterm suspends its status bar to avoid conflicting with the application's UI. The status bar is redrawn immediately when the application exits alt-screen.

If `lterm resume` / `lterm attach` panics or aborts mid-session, a process-wide hook emits a minimal recovery sequence (scroll region reset, cursor visible, alt-screen exit, SGR reset) so the user's terminal isn't left in raw mode or with hidden cursor.

Session names containing CJK characters or emoji (including ZWJ families, country flags, and combining marks) are aligned by display width using `unicode-width` and `unicode-segmentation`, so the status bar stays correctly padded across mixed-width content.

When a child application enables the Kitty keyboard protocol through `CSI u` enhancement sequences, lterm tracks that and best-effort restores the terminal keyboard mode when attach exits so a crashed child does not leave later shell input looking like `1;1:3u` escape fragments.

**Inspect or control a session:**

`--children` includes managed child panes; `--all` includes sessions that are normally hidden from the default list.

```bash
lterm sessions
lterm sessions --children
lterm sessions --all
lterm processes api
lterm logs api --start=-80
lterm input api 'echo hello' --enter
```

The generic aliases above are meant for day-to-day agent-terminal use: `sessions` lists persistent work, `processes` inspects child process trees, `logs` reads sanitized scrollback, and `input` writes text to the target PTY. The compatibility names are visible aliases that remain available for scripts and muscle memory: `list` / `ls`, `ps`, `capture`, and `send`.

**Stop a session:**

```bash
lterm close api
```

`kill` is a visible compatibility alias for `close`; both names use the same session/pane termination path.

**Run the daemon explicitly (advanced):**

```bash
# Client commands start this on demand; run it directly for supervisors/debugging.
lterm daemon
```

**Stop the daemon and every session it owns:**

```bash
# This is daemon-wide, not a single-session close.
lterm shutdown
```

## AI workflows

**Run common agent CLIs inside shimmed sessions:**

```bash
lterm claude
lterm codex
lterm gemini -- -p "summarize this repo"
lterm agents
```

These are thin profile aliases for:

```bash
lterm agent claude
lterm agent codex
lterm agent gemini -- -p "summarize this repo"
```

Agent launchers accept the same session controls across built-in profiles and custom `lterm agent <profile>` launches:

```bash
lterm claude --name repo-review --cwd /path/to/repo
lterm codex --detach --name repo-codex -- exec "summarize this repo"
lterm gemini --status -- -p "keep lterm status visible"
```

Known Claude/Codex/Gemini profiles default to a raw full-terminal attach without the lterm status bar, so their own TUI/status/alternate-screen rendering stays in control. Use `--status` to force the lterm status bar on, or `--no-status` to force it off for profiles that default on. Put `--` before agent arguments that could be parsed as lterm launch options. Use `lterm run -- <command>` when you want the generic tmux-compatible primitive directly or need to launch an unprofiled future agent; `run` enables the shim by default and `--no-tmux` opts out.

Launcher controls are long-only (`--name`, `--cwd`, `--detach`, `--status`, `--no-status`) so common agent short flags such as `-c` pass through naturally. They apply uniformly to `claude`, `codex`, `gemini`, `omx`, `omc`, and `agent <profile>`.
`--detach` prints `name<TAB>pane<TAB>command` with control characters and Unicode line/paragraph separators in each field replaced by spaces; resume later with `lterm resume <name>` or compatibility name `lterm attach <name>`. The detach record does not echo `--cwd`; query the session if you need to inspect it later.
Explicit `--name` values use lterm's normal session-name syntax and must be free; they do not auto-suffix on conflict, so an in-use name fails with a conflict error.
Names may contain ASCII letters, digits, `.`, `_`, and `-`, must not start with `-` or `%`, must not consist only of digits, must not look like a UUID, and are limited to 128 bytes.
Use `lterm agents` (or `lterm agents --json`) to inspect profile defaults and whether their binaries are currently available in `PATH`. JSON rows use `kind` values of `built-in`, `custom`, or `configured`. Pass profile names, such as `lterm agents codex my-agent --json`, to inspect a selected built-in/custom/configured set; availability is a point-in-time PATH probe.
For reusable custom aliases, pass an explicit JSON config file:

```bash
cat > agents.json <<'JSON'
{ "profiles": [{ "name": "repo-review", "binary": "codex", "session_base": "repo-review-session", "status_default": false }] }
JSON
lterm agents --agent-config agents.json --json
lterm agent repo-review --agent-config agents.json -- exec "review this repo"
```

Configured names and binaries use the same safe profile syntax as `lterm agent <profile>`; built-in names cannot be redefined.
`binary` must be a bare command name resolved from `PATH`, not a shell fragment or path. `binary` defaults to `name`, `session_base` defaults to `<name>-lterm`, `status_default` defaults to `true` and must be a boolean when present, duplicate names and unknown JSON fields are rejected, and when `--agent-config` is supplied non-built-in selected names must exist in that file.

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
lterm run -- omx hud --tmux
lterm run -- claude
lterm run -- codex exec "summarize the repository"
```

Inside that session, `tmux` resolves to the `lterm tmux-compat` shim. This is a compatibility layer, not a second spelling of every `lterm` product command. The shim implements the command subset most AI orchestration scripts rely on:

- **Sessions** — `new-session`, `attach-session`, `has-session`, `list-sessions`, `rename-session`, `kill-session`
- **Queries** — `list-windows`, `list-clients`, `list-commands`, `show-options`, `show-window-options`
- **Panes** — `split-window`, `list-panes`, `display-message`, `capture-pane`, `send-keys`, `kill-pane`, `resize-pane`
- **Buffers / popups** — `display-popup`, `wait-for`, `load-buffer`, `save-buffer`, `paste-buffer`
- **No-op compatibility** — `select-pane`, `select-layout`, `set-option`, `set-window-option`, `set-environment`, `show-environment`

Compatibility notes: lterm models each root session as one pseudo-window
(`window_index=0`, `window_panes=1`). `client_pid` and `client_tty` expand to
empty strings because lterm does not expose per-client process or TTY metadata.
tmux `-f` filters are intentionally rejected instead of being silently ignored.

## cmux behavior

When `lterm tmux-compat split-window` detects cmux (via `CMUX_WORKSPACE_ID`, `CMUX_SURFACE_ID`, or a cmux socket), it:

1. Starts a new `lterm` PTY session for the worker command.
2. Asks cmux to create a native split (`cmux new-split right/down`).
3. Sends the compatibility command `lterm attach <pane>` into that split, so cmux panes still work if `LTERM_BIN` points at an older `lterm` build that predates `resume`.

This gives cmux a real pane to decorate while `lterm` retains scrollback capture and `send-keys` compatibility.

**Notifications:**

```bash
lterm notify --title 'Task complete' --body 'All checks passed'
```

`lterm notify` first tries `cmux notify`. If that's unavailable, it emits OSC 777 so cmux or another compatible terminal can still surface the notification. Notification fields are stripped of terminal control characters before falling back to OSC; subtitle/body separators such as newlines are normalized to spaces rather than concatenated.

## Remote access

If `lterm` is installed on a remote machine:

```bash
lterm ssh user@host main
```

This uses the same attach-or-create behavior as `lterm open main` on the remote host; the wire command remains `lterm attach-or-new main` so newer local clients still work with older remote `lterm` installs that do not know `open`. Pass SSH flags after `--`:

```bash
lterm ssh devbox main -- -p 2222 -i ~/.ssh/id_ed25519
```

## Architecture

- **Daemon** — one Unix socket per user under `$XDG_RUNTIME_DIR`, with an owner-only fallback under `/tmp`.
- **PTY sessions** — spawned via `portable-pty`, backed by ring-buffer scrollback.
- **Attach protocol** — the CLI sends JSON over the Unix socket, optionally reserves the bottom row for a local status bar, then streams PTY bytes.
- **tmux shim** — a small shell script named `tmux` forwards commands to `lterm tmux-compat`.
- **cmux bridge** — optional; uses the cmux CLI when detected.

After upgrading the `lterm` binary, restart any already-running daemon before
relying on newly added wire-protocol behavior; existing daemon processes keep
the old code until they are stopped.

## Security notes

**Terminal output is forwarded as-is.** `lterm resume` (compatibility name: `lterm attach`) passes PTY bytes through so full-screen terminal programs and cmux/OSC notifications keep working. The local status bar is purely a client-side decoration; use `--no-status` for a fully raw terminal surface. Untrusted child programs can still emit terminal escape sequences to an attached terminal — exactly as under tmux/screen. **Do not use `lterm` as an escape-sequence sanitizer or sandbox.**

**Capture output is sanitized for human/AI consumption.** `lterm logs` (compatibility name: `lterm capture`) and `tmux capture-pane` strip common terminal control sequences before printing scrollback.

**Process visibility.** `lterm processes [session]` (or compatibility name `lterm ps [session]`) shows the process tree rooted at each session child, so long-running Codex/OMX/MCP subprocess buildup stays visible before it becomes a memory-leak surprise. The system `ps` is invoked by absolute path, and malformed process rows are skipped rather than guessed at.

**Socket location.** Custom `LTERM_SOCKET` paths must live in an owner-only directory. Prefer `LTERM_RUNTIME_DIR` when you need an isolated socket location.

**Popup commands.** `tmux-compat display-popup` runs the requested command through the user's shell to preserve tmux-like behavior. **Do not pass untrusted popup commands.**

**Build reproducibility.** Use the committed lockfile for release builds: `cargo build --release --locked`. The current lockfile pins `serde_json 1.0.149`. Its transitive `zmij` dependency is part of the official serde_json package metadata on docs.rs/crates.io — not a local vendored crate.

## Current limitations

- Session persistence lasts only while the daemon and host are alive — reboot/process-state restore is not implemented.
- Outside cmux, `split-window` creates additional managed PTY sessions but does not draw a tiled in-terminal UI.
- This is a compatibility subset, not a full tmux server. Scripts using advanced tmux formats or options may need additional shim commands.
- cmux pane capture is handled through `lterm` sessions, not cmux scrollback APIs.
- The daemon authenticates local clients via OS peer credentials and owner-only socket paths — there are no per-session ACLs yet.
- Session shutdown uses verified process-group signaling, so child trees like `shell → OMX → Codex → MCP` are cleaned up together when possible. Processes that intentionally detach into a different session/process group can outlive `lterm close` / `lterm kill`; inspect them with `lterm processes` / `lterm ps` or OS process tools.

## Development

```bash
cargo fmt
cargo test
cargo build --locked
```

Use isolated runtime directories for manual testing:

```bash
TMP=$(mktemp -d)
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- start --name test -- sh -lc 'echo hi; sleep 10'
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- logs test -S=-20
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- shutdown
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

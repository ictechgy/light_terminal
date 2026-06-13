# lterm 1.0 Public Contract

This document defines the public command and output contract intended for the
`lterm` 1.x release line. The machine-readable source of truth is
[`docs/contract-manifest.json`](contract-manifest.json); this page explains the
stability classes, raw-stream boundary, and command surface in human terms.

## Stability classes

The 1.0 contract separates command stability from tmux compatibility breadth.
A command can be stable as an `lterm` product command while still implementing
only a small tmux-compatible subset.

| Class | Meaning |
| --- | --- |
| `stable` | Product behavior promised across 1.x unless it is deprecated first. |
| `compatibility-stable` | Documented tmux-compatible subset or no-op compatibility behavior that is intentionally supported. |
| `best-effort` | Useful behavior that may change as integrations evolve. |
| `internal` | Implementation detail, not a promised user-facing API. |
| `explicit-raw-unsafe` | Attached PTY stream behavior that is transparent and intentionally unsanitized. |

Output stability is tracked separately for text and JSON. Stable JSON outputs
must have a schema path in the manifest. The manifest schema lives at
[`docs/schemas/contract-manifest.schema.json`](schemas/contract-manifest.schema.json),
and `scripts/validate_contract_manifest.py` validates manifest fields including
`surface_contracts`, `stability_scope`, and the `best-effort` stability value.
Text output stability applies to the shape and documented fields, not to
user-controlled PTY content.

For mixed commands, the manifest's `surface_contracts` field is the
authoritative stability boundary for each output surface. For example,
`lterm start` has a stable detached summary row, but its default attached PTY
stream is raw and has no sanitized text contract. `lterm env` stability covers
the shell-evaluable exports and variable names, not the exact visual quote style.

## Sanitization and raw PTY boundary

Attached PTY streams are raw by design. Raw `lterm resume`, compatibility aliases
such as `lterm attach` / `lterm a` / `lterm -a`, raw `lterm open`, default
attached `lterm start`, `lterm run`, raw profiled agent launchers, and
`lterm ssh` forward PTY bytes without escape-sequence sanitization. This
preserves full-screen programs, OSC notifications, cmux passthrough, and shell
behavior, but it also means untrusted programs can emit terminal escape
sequences to the attaching terminal. Do not use raw attached `lterm` streams as
a sanitizer or sandbox.

Report-style surfaces are different: `sessions`, `processes`, `doctor`, `logs`,
`wait`, `watch`, `agents`, `notify` fallback output, and tmux-compat listing
surfaces sanitize terminal controls before printing human-readable or
machine-readable reports.

`lterm compose` / `lterm mobile` is also non-attached, but it is an interactive
composer UI rather than a machine-readable report surface. Its displayed
scrollback is sanitized capture output, it has no JSON output contract, and
committed input goes through the existing `input` / `send` path. Default commits
append Enter (`\r`), `--no-enter` sends exact message bytes, and compose must not
attach, resize, or alter attached-client counts or PTY geometry. Sanitization
belongs only to these non-attached surfaces; adding sanitization to attach/resume
would violate the 1.0 contract.

Mobile transcript attach is a pre-attach policy shim, not raw-stream
sanitization. `--attach-mode=auto` / `LTERM_ATTACH_MODE=auto` is the default,
and accepted values are `auto`, `raw`, and `mobile` (`mobile` means the
normal-screen transcript view). Desktop clients keep the raw attach path. Auto
mobile detection is conservative best effort: `LTERM_MOBILE=1` or a Termius
terminal identity marks the client as mobile, and the target must look like an
agent session via persisted `LTERM_AGENT` metadata, a built-in `*-lterm` agent
session name, or a known agent command basename. Termius connections that expose
only generic SSH variables such as `SSH_TTY` and `TERM=xterm-256color` stay on
the raw path unless the user explicitly opts into mobile transcript mode.
Scripts that need deterministic behavior should use explicit flags/env. The
transcript surface is non-attached: it displays sanitized capture output, sends
input through the existing `input` / `send` path, does not enter alternate
screen, does not increment attached-client counts, and does not resize PTY
geometry. The transcript UI may emit its own local SGR reset (`ESC[0m`) before
lterm-owned text so stale host terminal colors do not leak into sanitized
scrollback; that reset is not captured PTY payload and does not relax capture
sanitization. `--raw` / `LTERM_ATTACH_MODE=raw`
forces the raw path; `--mobile` / `LTERM_ATTACH_MODE=mobile` forces the
transcript path. CLI flags (`--raw`, `--mobile`, `--attach-mode`) are explicit
user intent and take precedence over `LTERM_ATTACH_MODE`.

Raw attach row presence is separate from attach transport. Ordinary raw sessions
default to a client-side bottom status row; built-in agent launchers default to
row-off full-height raw attach and may emit a compact terminal-title cue
(`lt:<session>:<pane> · <agent>`) plus a one-shot `[lterm] <session> <pane> ·
<agent> (status row hidden for agent TUI; use --status to show it)` banner as the
non-row presence indicator before raw attach. While the row remains hidden, lterm
refreshes only the terminal-title cue after idle gaps so Codex-like TUIs can own
the screen without permanently erasing the lterm identity.
`LTERM_AGENT_CUE=0` disables both cue forms; `LTERM_AGENT_BANNER=0` disables only
the inline banner while keeping the terminal-title cue. User-controlled fields in
these host-side cue surfaces are sanitized before printing, but the subsequent
attached PTY stream remains raw-transparent. The row can be disabled globally
with `LTERM_NO_STATUS=1` or `LTERM_STATUS=0`; those env gates also beat explicit
agent `--status` requests for safety. `--status` is scoped to agent launchers in
the current CLI and requests a raw status row only when the final transport is
raw. Mobile transcript ignores row-presence policy because it is not a raw-row
renderer. For row-on shell sessions, lterm may best-effort suspend the row when a
stable known-agent descendant is detected through the local process tree, then
restore it when the agent exits. Ambiguous or unavailable process detection must
fail safe by keeping the row. This host-side row management does not sanitize,
filter, or rewrite attached PTY bytes.

Every spawned session child receives `LTERM_SESSION` and `LTERM_PANE` as stable
in-session identity variables for prompt badges and lightweight environment-aware
tooling.

Compose target resolution uses the same session-or-pane target model as
`lterm logs`. The display sub-surface captures the last `--tail` sanitized
scrollback lines (default: 80) from that target; it does not expose independent
`--start` / `--end` range flags. `--once` performs that capture exactly once
before committing input, while interactive compose refreshes on the configured
`--refresh` interval (default: 500ms) and after local input or terminal resize
events.

Interactive compose uses the same commit rule as `--once`: pressing Enter commits
the current input buffer plus Enter (`\r`), including an empty buffer for prompts
that ask the user to press Enter. Ctrl-C, Ctrl-D, and Esc are local composer exit
keys and are not forwarded to the target PTY. The `lterm input --enter` option
uses the same Enter byte (`\r`).

When the manifest uses `surface_contracts`, nested `raw_stream_policy` values use
the same policy vocabulary as top-level entries. `not-applicable` is valid for a
sub-surface such as `committed-input-send` because that sub-surface emits no text
or raw output stream.

## Product command surface

| Command | Aliases | Classification | Text output | JSON output | Raw stream policy |
| --- | --- | --- | --- | --- | --- |
| `lterm start` | `lterm new` | `stable` | `best-effort` at command level; `stable` for the `--detach` summary row; attached stream is raw | none | `raw-transparent` when attached |
| `lterm run` | none | `stable` | none; attached stream is raw | none | `raw-transparent` |
| `lterm resume` | `lterm attach`, `lterm a`, `lterm -a` | `explicit-raw-unsafe` | none at command level; raw stream is transparent; local status/presence decorations are best-effort sub-surfaces | none | `raw-transparent` for raw attach; `sanitized-output-only` for mobile transcript |
| `lterm open` | `lterm attach-or-new` | `explicit-raw-unsafe` | none at command level; raw stream is transparent; local status/presence decorations are best-effort sub-surfaces | none | `raw-transparent` for raw attach; `sanitized-output-only` for mobile transcript |
| `lterm sessions` | `lterm list`, `lterm ls` | `stable` | `stable` tab-separated rows | `stable` | `sanitized-output-only` |
| `lterm processes` | `lterm ps` | `stable` | `stable` tab-separated rows | `stable` | `sanitized-output-only` |
| `lterm rename` | none | `stable` | `stable` updated `name\tpane` row | none | `sanitized-output-only` |
| `lterm status-theme` | `lterm theme` | `stable` | `stable` updated `name\tpane\ttheme` row | none | `sanitized-output-only` |
| `lterm init` | none | `best-effort` | `best-effort` setup preview; does not modify shell files | none | `sanitized-output-only` |
| `lterm logs` | `lterm capture` | `stable` | `stable` sanitized scrollback bytes for documented range semantics | none | `sanitized-output-only` |
| `lterm urls` | none | `stable` | `stable` recent URL rows as `N<TAB>URL`; `--last` emits only the newest valid URL | `stable` string array; `--last --json` emits `[]` or a one-element array | `sanitized-output-only` |
| `lterm trace` | `lterm record` | `explicit-raw-unsafe` | none; writes a private local JSONL trace file capped by --max-bytes | `best-effort` JSONL events with hex-encoded raw PTY output chunks | `raw-transparent` |
| `lterm trace-replay` | `lterm replay-trace` | `explicit-raw-unsafe` | `best-effort` raw bytes decoded from an explicit local trace file; `--timing` can preserve inter-chunk delays | none | `raw-transparent` |
| `lterm trace-info` | none | `best-effort` | `best-effort` raw-free metadata summary for a local trace file | `best-effort` raw-free metadata summary | `sanitized-output-only` |
| `lterm wait` | none | `stable` | `stable` status row | `stable` | `sanitized-output-only` |
| `lterm watch` | none | `stable` | `stable` status row | `stable` | `sanitized-output-only` |
| `lterm compose` | `lterm mobile` | `stable` | `best-effort` UI with stable sanitized capture display; `--transcript` uses the normal-screen transcript UI | none | `sanitized-output-only` |
| `lterm input` | `lterm send` | `stable` | none | none | `not-applicable` |
| `lterm close` | `lterm kill` | `stable` | none | none | `not-applicable` |
| `lterm doctor` | `lterm status` | `stable` | `stable` key/value rows | `stable` | `sanitized-output-only` |
| `lterm diagnose --bundle` | none | `best-effort` | none | `best-effort` local diagnostic bundle; does not start the daemon and excludes raw PTY bytes/scrollback by default | `sanitized-output-only` |
| `lterm daemon` | none | `internal` | none | none | `not-applicable` |
| `lterm shutdown` | none | `stable` | none | none | `not-applicable` |

### `lterm urls` stable extraction semantics

`lterm urls` is a sanitized scrollback report. It scans only the last `--tail`
sanitized lines and never attaches to, rewrites, or forwards data into the raw
PTY stream. Extraction matches `http://` and `https://` schemes
ASCII-case-insensitively while preserving the original URL text in output. The
numbered text rows and `--json` array use the same extracted URL set: first-seen
exact-text deduplicated unique ASCII URL tokens capped at 256 rows, complete raw URL
candidates longer than 4096 bytes are skipped before trimming rather than
truncated, and whitespace/control-bearing or non-ASCII URL tokens are
excluded. `--last` reports the newest valid within-length URL
occurrence in the captured tail even when it is a duplicate or appears after the
256-row unique-list cap; `--last --json` reports `[]` or a one-element array.
Empty text modes produce no rows, and empty JSON mode produces `[]`. Extracted
URLs are untrusted terminal output: producers can print phishing links, and
authentication URLs may contain short-lived bearer material. Automation should
handle URL rows as sensitive data, avoid logging them verbatim by default, and
prefer `--last` only when the newest valid link is known to be the intended one.

## Utility and integration surface

| Command | Aliases | Classification | Text output | JSON output | Raw stream policy |
| --- | --- | --- | --- | --- | --- |
| `lterm install-shim` | none | `stable` | `stable` shim path text | none | `sanitized-output-only` |
| `lterm env` | none | `stable` | `stable` shell exports; `--shell fish` emits fish syntax for `source`; quote style is not a stable visual API | none | `sanitized-output-only` |
| `lterm install-completions` | none | `best-effort` | `best-effort` user-local completion file install summary and activation hint; does not start the daemon | none | `sanitized-output-only` |
| `lterm install-ai-statusline` | none | `best-effort` | `best-effort` supported AI CLI statusline install summary; backs up mutated settings and reports unsupported custom statusline surfaces with an explanatory note | none | `sanitized-output-only` |
| `lterm completions` | none | `best-effort` | `best-effort` shell completion scripts for `bash`, `zsh`, and `fish`; generated output follows clap-complete behavior and does not start the daemon | none | `sanitized-output-only` |
| `lterm notify` | none | `best-effort` | `best-effort` cmux notification attempt plus sanitized OSC fallback | none | `sanitized-output-only` |
| `lterm ssh` | none | `explicit-raw-unsafe` | none | none | `raw-transparent` |
| `lterm agents` | none | `stable` | `stable` agent profile availability report | `stable` | `sanitized-output-only` |
| `lterm agent` | `lterm claude`, `lterm codex`, `lterm opencode`, `lterm copilot`, `lterm cursor-agent`, `lterm agy`, `lterm jules`, `lterm kiro`, `lterm aider`, `lterm goose`, `lterm amp`, `lterm crush`, `lterm kimi`, `lterm qwen`, `lterm gemini`, `lterm omx`, `lterm omc` | `best-effort` | `best-effort` launcher controls and pre-attach presence cue; raw attached agent PTY stream and external agent CLI behavior are outside the lterm sanitized-output contract; mobile transcript is sanitized capture UI | none | `raw-transparent` for raw attach; `sanitized-output-only` for mobile transcript |
| `lterm tmux-compat list-commands` | none | `compatibility-stable` | `stable` tmux shim command list | `stable` | `sanitized-output-only` |

`lterm sessions --json` and the embedded `session` object in `lterm wait/watch
--json` may include `agent_name` for sessions launched through an agent profile.
The value is sanitized profile metadata persisted from `LTERM_AGENT`; non-agent
sessions omit the field rather than emitting `null`.

## tmux compatibility boundary

`lterm tmux-compat ...` is a compatibility shim namespace for scripts that already
speak tmux. It is not a second spelling for every product command. The stable
contract is the documented subset exposed by `lterm tmux-compat list-commands`
and the focused compatibility docs. Its support tiers (`full`, `partial`, and
`noop`) describe tmux-shim coverage only; they do not replace the 1.0 stability
classes above.

The manifest classifies `lterm tmux-compat list-commands` as
`compatibility-stable`. Individual shim commands keep their behavior within the
subset documented in the README and wiki, while unsupported tmux commands remain
outside the 1.0 contract unless later added to the manifest.
No-op compatibility commands, including `set-hook`, are stable only as accepted
shim calls; lterm does not execute tmux hook dispatchers.

## Manifest-listed examples

The following examples are intentionally listed in
`docs/contract-manifest.json` and executed by the contract example gate. They are
kept side-effect-light so CI can run them with isolated `LTERM_RUNTIME_DIR` and
`LTERM_DATA_DIR` values.

```bash
lterm install-shim
lterm sessions --json
lterm doctor --json
lterm shutdown
lterm notify --title 'Task complete' --body 'All checks passed'
lterm agents --json
lterm tmux-compat list-commands --json
```

## Doctor JSON example

`lterm doctor --json` consolidates client/daemon identity, the same-user trust
boundary (`daemon_uid`), uptime, and a single-line `reason` for abnormal states
into one object. Fields with `null` defaults from older daemons may be omitted
entirely. The schema lives at
[`docs/schemas/doctor.schema.json`](schemas/doctor.schema.json).

```json
{
  "client_version": "1.0.3",
  "client_protocol_version": 3,
  "daemon_reachable": true,
  "daemon_version": "1.0.3",
  "daemon_protocol_version": 3,
  "version_match": true,
  "daemon_session_count": 0,
  "daemon_active_connections": 1,
  "daemon_shutting_down": false,
  "daemon_uid": 501,
  "daemon_started_at_unix_secs": 1779036000,
  "daemon_uptime_secs": 745,
  "daemon_error": null,
  "runtime_dir": "/var/folders/.../light-terminal-501",
  "data_dir": "/Users/<you>/.local/share/light-terminal",
  "socket_path": "/var/folders/.../light-terminal-501/lterm.sock",
  "shim_dir": "/Users/<you>/.local/share/light-terminal/shims",
  "tmux_shim_path": "/Users/<you>/.local/share/light-terminal/shims/tmux",
  "tmux_shim_exists": true,
  "shim_dir_in_path": false
}
```

When something is wrong, `reason` is a single sanitized sentence (e.g. `Daemon
is not reachable. It usually auto-starts on the next \`lterm\` command; run
\`lterm doctor\` again or inspect daemon startup with \`lterm logs\`.`). A
healthy daemon omits `reason`. Older daemons that predate `daemon_uid` /
`started_at_unix_secs` simply omit those fields; their absence is not an
error.

## Non-blocking P1 surfaces

Agent workflow cookbook recipes are useful follow-up work, but they are not 1.0
release blockers unless a future manifest change explicitly adds a recipe
surface as stable. Cookbook examples should not be used as release-gate contract
examples unless they are manifest-promoted. `lterm completions` is now
manifest-promoted as a best-effort utility surface, not a stable visual contract
for exact shell-script bytes.

## Release evidence

Before tagging a `1.x` release, attach a verification transcript to the release notes or
release checklist. The transcript should include the standard Rust checks,
manifest/schema/example validation, the required upgrade matrix, the release-gate
soak profile, and dependency audit evidence. This docs lane defines the public
contract; the validation, upgrade, soak, and security lanes own their respective
implementation evidence.

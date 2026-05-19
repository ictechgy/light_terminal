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

Attached PTY streams are raw by design. `lterm resume`, compatibility aliases
such as `lterm attach` / `lterm a` / `lterm -a`, `lterm open`, default attached
`lterm start`, `lterm run`, profiled agent launchers, and `lterm ssh` forward PTY
bytes without escape-sequence sanitization. This preserves full-screen programs,
OSC notifications, cmux passthrough, and shell behavior, but it also means
untrusted programs can emit terminal escape sequences to the attaching terminal.
Do not use attached `lterm` streams as a sanitizer or sandbox.

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
| `lterm resume` | `lterm attach`, `lterm a`, `lterm -a` | `explicit-raw-unsafe` | none | none | `raw-transparent` |
| `lterm open` | `lterm attach-or-new` | `explicit-raw-unsafe` | none | none | `raw-transparent` |
| `lterm sessions` | `lterm list`, `lterm ls` | `stable` | `stable` tab-separated rows | `stable` | `sanitized-output-only` |
| `lterm processes` | `lterm ps` | `stable` | `stable` tab-separated rows | `stable` | `sanitized-output-only` |
| `lterm rename` | none | `stable` | `stable` updated `name\tpane` row | none | `sanitized-output-only` |
| `lterm status-theme` | `lterm theme` | `stable` | `stable` updated `name\tpane\ttheme` row | none | `sanitized-output-only` |
| `lterm init` | none | `best-effort` | `best-effort` setup preview; does not modify shell files | none | `sanitized-output-only` |
| `lterm logs` | `lterm capture` | `stable` | `stable` sanitized scrollback bytes for documented range semantics | none | `sanitized-output-only` |
| `lterm wait` | none | `stable` | `stable` status row | `stable` | `sanitized-output-only` |
| `lterm watch` | none | `stable` | `stable` status row | `stable` | `sanitized-output-only` |
| `lterm compose` | `lterm mobile` | `stable` | `best-effort` UI with stable sanitized capture display | none | `sanitized-output-only` |
| `lterm input` | `lterm send` | `stable` | none | none | `not-applicable` |
| `lterm close` | `lterm kill` | `stable` | none | none | `not-applicable` |
| `lterm doctor` | `lterm status` | `stable` | `stable` key/value rows | `stable` | `sanitized-output-only` |
| `lterm daemon` | none | `internal` | none | none | `not-applicable` |
| `lterm shutdown` | none | `stable` | none | none | `not-applicable` |

## Utility and integration surface

| Command | Aliases | Classification | Contract notes |
| --- | --- | --- | --- |
| `lterm install-shim` | none | `stable` | Installs/prints the tmux shim path; text is sanitized. |
| `lterm env` | none | `stable` | Emits POSIX shell exports that prepend the lterm shim directory to `PATH`; `--shell fish` emits fish syntax for `source`. Shell-eval semantics are stable, quote style is not a stable visual API. |
| `lterm notify` | none | `best-effort` | Tries `cmux notify`, then emits sanitized OSC 777 fallback output. |
| `lterm ssh` | none | `explicit-raw-unsafe` | Uses SSH to run a remote attach-or-new command; remote PTY bytes are unsanitized. |
| `lterm agents` | none | `stable` | Reports built-in/configured/custom agent launcher profile availability. |
| `lterm agent` | `lterm claude`, `lterm codex`, `lterm gemini`, `lterm omx`, `lterm omc` | `best-effort` | Launcher controls are public, but the attached agent PTY stream is raw and the external agent CLI behavior is outside the lterm contract. |

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
  "client_version": "1.0.1",
  "client_protocol_version": 3,
  "daemon_reachable": true,
  "daemon_version": "1.0.1",
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

Shell completions and agent workflow cookbook recipes are useful follow-up work,
but they are not 1.0 release blockers unless a future manifest change explicitly
adds a completion or recipe surface as stable. Cookbook examples should not be
used as release-gate contract examples unless they are manifest-promoted.

## Release evidence

Before tagging a `1.x` release, attach a verification transcript to the release notes or
release checklist. The transcript should include the standard Rust checks,
manifest/schema/example validation, the required upgrade matrix, the release-gate
soak profile, and dependency audit evidence. This docs lane defines the public
contract; the validation, upgrade, soak, and security lanes own their respective
implementation evidence.

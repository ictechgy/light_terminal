# Security Policy

`lterm` is a same-user terminal session daemon. It is designed for convenience
and agent workflow persistence, not for sandboxing untrusted programs or
sanitizing interactive terminal output.

## Supported versions

Security fixes are prepared against the current `main` branch and the latest
published release line. Before reporting behavior from an upgraded install,
restart any already-running daemon with `lterm shutdown`; old daemon processes
keep their previous binary and wire-protocol behavior until stopped.

## Reporting vulnerabilities

- Preferred channel: open a private GitHub Security Advisory for
  `ictechgy/light_terminal` when that option is available.
- If private advisories are unavailable, open a minimal GitHub issue asking for
  a maintainer security contact and omit exploit details, secrets, paths, logs,
  and terminal captures until a private channel is agreed.
- Include the `lterm --version`, operating system, install method, whether a
  daemon was already running before upgrade, and a minimal reproduction using
  isolated `LTERM_RUNTIME_DIR` / `LTERM_DATA_DIR` paths when possible.

Do not include sensitive terminal scrollback, credentials, hostnames, access
tokens, socket paths from shared systems, or raw PTY byte streams in public
reports.

## Trust boundaries

### Same-user daemon boundary

The daemon accepts local Unix-socket clients that authenticate as the same OS
user. Processes running as that user should be considered capable of listing,
attaching to, sending input to, closing, or otherwise controlling that user's
`lterm` sessions. `lterm` does not provide per-session ACLs, multi-user
isolation, command sandboxes, seccomp profiles, or privilege separation.

### Socket paths and private directories

By default, `lterm` creates its runtime socket under `$XDG_RUNTIME_DIR` or an
owner-specific fallback under the system temporary directory. `LTERM_RUNTIME_DIR`,
`LTERM_DATA_DIR`, and custom `LTERM_SOCKET` parents must be absolute,
owner-owned, private directories (`0700` or stricter) and must not be symlinks.
The daemon refuses unsafe custom socket parents and refuses to replace an
existing non-socket file at the socket path.

The daemon sets the socket file to owner-only permissions after binding, but
filesystem permissions are defense in depth: the primary authorization boundary
is same-user peer credential verification.

### Peer credentials

On supported Unix platforms, each daemon connection is checked with OS peer
credentials and must match the daemon's effective uid. Cross-user socket peers
are rejected even if they can reach the socket path. Platforms without peer
credential support are not a supported security boundary for the daemon.

### Raw PTY attach/resume streams

`lterm resume`, `lterm attach`, `lterm a`, `lterm -a`, `lterm open` when it
attaches, agent launcher attaches, and `lterm ssh` forward PTY bytes to the
client terminal without sanitizing the stream. This is intentional so shells,
full-screen TUIs, OSC notifications, bracketed paste, Kitty keyboard mode,
cmux passthrough, and other terminal features keep working.

Treat attached sessions exactly like direct `ssh`, `tmux`, or `screen` sessions:
untrusted child programs can emit terminal escape sequences that affect the
local terminal, including cursor/screen changes, alternate-screen state,
clipboard-capable OSC sequences if the terminal allows them, hyperlinks, title
changes, and emulator-specific controls. Use `--no-status` when you need the
least-decorated attach path, but do not treat it as an escape-sequence filter.

### Sanitized report surfaces

Sanitization is limited to non-attached text/report surfaces intended for humans
and agents, including `logs` / `capture`, session and process list text output,
`doctor` / `status` text output, tmux-compat capture output, and notification
fallback fields. These paths strip or frame-protect control sequences for safer
reporting, but they do not retroactively sanitize raw attached terminals and are
not a substitute for running trusted commands.

Machine-readable JSON report surfaces are intended to remain parseable and
schema-stable where documented. Raw PTY streams are not JSON/text contract
surfaces.

### Shell command construction

Some commands intentionally invoke a shell to preserve terminal and tmux-like
behavior. Examples include session commands assembled from CLI arguments,
agent launcher profiles, `lterm ssh` remote execution, and
`lterm tmux-compat display-popup`. Pass only trusted command strings and trusted
profile configuration. Do not construct these commands from untrusted input
without application-level quoting and validation.

### Remote SSH

`lterm ssh <host> [target]` relies on the local `ssh` client for host-key
verification, authentication, transport security, and SSH option handling. The
remote host must have `lterm` installed. Remote PTY bytes are forwarded to the
local terminal without sanitization, so use this only with hosts and remote
accounts you trust to drive your terminal.

### Process cleanup and orphan visibility

`lterm close` / `lterm kill` and daemon shutdown attempt to terminate the
recorded session process group and reap children. Processes that deliberately
detach into another session or process group can outlive `lterm`; inspect them
with `lterm processes --orphans` and normal OS process tools. Security reports
about leaked children should include whether the child changed process group or
session id and whether `processes --orphans` exposed it.

### Dependencies and release audit

Release builds use the committed `Cargo.lock` and should be built with
`cargo build --release --locked`. Before tagging a release, maintainers should
run the standard Rust checks and `cargo audit`; any unfixed advisory must be
recorded in release evidence with impact, affected paths, and mitigation or
upgrade plan. The working dependency update policy is in
[`docs/dependency-hygiene.md`](docs/dependency-hygiene.md). npm wrapper releases
should publish binaries built from the same reviewed source revision.

## Security non-goals

`lterm` does not attempt to:

- isolate mutually untrusted users or mutually untrusted same-user processes;
- sanitize interactive PTY attach/resume streams;
- prevent a trusted session command from reading or writing files available to
  the invoking OS user;
- validate the behavior of remote hosts reached through SSH;
- provide a complete tmux security model or a general terminal emulator sandbox.

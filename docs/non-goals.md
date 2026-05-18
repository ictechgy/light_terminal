# Non-goals

This document records features that `lterm` deliberately does **not** pursue.
It is a companion to [`SECURITY.md`](../SECURITY.md), which covers trust
boundaries, and to [`docs/public-contract.md`](public-contract.md), which
covers stability promises.

The goal is to keep `lterm`'s surface small, predictable, and aligned with its
positioning as a same-user convenience daemon for terminal-first AI agents.
Issues, pull requests, and design discussions that propose work in these areas
should expect to be closed with a pointer to the relevant entry below, unless
they fundamentally change the assumptions listed under each item.

## Full tmux replacement

- **Why rejected.** `tmux` already exists and covers rich pane/window/layout
  multiplexing. `lterm` intentionally implements only the tmux command subset
  that AI agent tooling (Claude Code, Codex CLI, Gemini CLI, OMX/OMC, cmux)
  exercises. Expanding to the full surface would balloon code, tests, and the
  1.0 public contract, and would compete with `tmux` on terrain `tmux` already
  owns.
- **Alternative.** Use `tmux` directly when you need its full multiplexer
  surface. Use `lterm tmux-compat` when you need the documented subset for
  agent workflows. New tmux commands are added only when AI agent or cmux use
  cases require them.

## Sandbox / privilege isolation

- **Why rejected.** `lterm` is a same-user daemon. Any process running as the
  invoking OS user already has the authority to control sessions. Introducing
  ACLs, seccomp profiles, or per-session privilege separation would suggest a
  security guarantee that the same-user model cannot honor.
- **Alternative.** Run untrusted code under OS-level isolation primitives
  (containers, VMs, dedicated user accounts) and treat `lterm` like `tmux` or
  `screen` for that bounded environment.

## Raw attach / resume stream sanitization

- **Why rejected.** Attached PTY streams (`lterm resume`, `attach`, `a`, `-a`,
  attached `start`, `run`, agent launchers, `lterm ssh`) are intentionally
  raw so shells, full-screen TUIs, OSC notifications, bracketed paste, Kitty
  keyboard mode, and cmux passthrough keep working. Inserting a sanitizer in
  the attach path would break interactive terminal behavior and would create
  a false sense of safety. The 1.0 public contract names this surface
  `explicit-raw-unsafe`.
- **Alternative.** Sanitization belongs to non-attached report surfaces only:
  `logs`, `capture`, `sessions`, `processes`, `doctor`, `status`, `wait`,
  `watch`, `agents`, tmux-compat listings, notification fallbacks, and
  `compose` / `mobile` display. New sanitization work goes there, never on
  the attach path.

## Broad telemetry / phone-home

- **Why rejected.** `lterm` runs inside terminals that frequently handle
  credentials, source code, and private project data. Embedding broad usage
  telemetry would shift the trust model from "same-user local daemon" to
  "same-user daemon plus opaque outbound channel" and would require consent,
  privacy review, and ongoing operational cost that the project does not
  currently take on.
- **Alternative.** Self-service diagnostics. `lterm doctor`, `lterm status`,
  `lterm logs --start/--end`, `lterm processes --orphans`, and the existing
  contract manifest are designed for both humans and agents to inspect state
  locally. A future `lterm diagnose --bundle` style command for voluntary,
  redacted, user-initiated submission may be considered, but it is opt-in and
  user-driven, not telemetry.

## Multi-user daemon

- **Why rejected.** The daemon binds an owner-only Unix socket, checks peer
  credentials, and refuses cross-user peers by design. Adding shared
  multi-user sessions would invalidate the same-user trust boundary, require
  privilege separation that the project does not pursue, and contradict
  `SECURITY.md`. Cross-user pair programming or sharing is out of scope.
- **Alternative.** Use a shared user account, a dedicated screen-sharing
  tool, or terminal-sharing services that are designed for multi-user trust.
  `lterm ssh` is supported for the case where each user has their own remote
  `lterm` install on a host they trust.

## Shell command magic expansion

- **Why rejected.** Some `lterm` paths invoke a shell intentionally (session
  commands assembled from CLI args, agent launcher profiles, `lterm ssh`
  remote execution, `lterm tmux-compat display-popup`). Adding implicit
  expansion, alias resolution, glob extension, or "smart" quoting on behalf
  of the user would obscure what command actually runs, complicate the shell
  command construction trust boundary in `SECURITY.md`, and make automation
  by AI agents harder to reason about.
- **Alternative.** Callers (humans, scripts, agent profiles) construct shell
  command strings themselves with explicit quoting. `shlex`-style splitting
  is used where documented. Profile and configuration formats remain literal
  rather than templated.

## How this list is maintained

- Each addition or removal here is treated as a contract-level change and
  must be referenced from the relevant pull request description.
- Items here are not silent. They are echoed in short form from `README.md`
  and `README.ko.md` so users find them before filing requests in these
  areas.
- The list is intentionally short. Adding new items is fine; converting
  existing items into goals later requires an explicit, recorded decision.

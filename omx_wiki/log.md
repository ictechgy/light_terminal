# Wiki Log

Category: session-log
Tags: wiki, log

- 2026-05-12: Seeded command-surface reference after product CLI vocabulary redesign PRs.
- 2026-05-12: Added utility-surface coverage for setup, profile, notification, remote, and shim commands.
- 2026-05-12: Clarified `run` as the generic tmux-compatible product command and documented `--no-tmux` as the visible opt-out.
- 2026-05-13: Added tmux-compatible capture reference covering `logs`, `capture-pane`, range semantics, and parser invariants.
- 2026-05-13: Clarified shared tmux target override behavior and OSC notification separator sanitization.
- 2026-05-13: Documented daemon restart expectation for wire-protocol behavior changes.
- 2026-05-13: Documented stricter tmux-compatible value parsing for `new-session -s/-c` and `resize-pane -x/-y`.
- 2026-05-15: Documented `doctor` / `status`, product `logs --end`, process-group/orphan visibility, and verbose/JSON `tmux-compat list-commands`.
- 2026-05-15: Added agent observability notes for product `wait` / `watch`, including sanitized scrollback matching, exit waits, and cmux-friendly notifications.

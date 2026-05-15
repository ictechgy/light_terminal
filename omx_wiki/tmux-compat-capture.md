# tmux-Compatible Capture

Category: reference
Tags: capture, tmux-compat, logs, scrollback, parser

## Purpose

This page records the stable boundary between the product capture command and
the tmux shim capture command. Keep it aligned with `src/main.rs`,
`src/tmux_compat.rs`, and the capture smoke tests when parser or range behavior
changes.

## Command surfaces

| Surface | Command | Output behavior | Range support |
| --- | --- | --- | --- |
| Product CLI | `lterm logs <target>` | Always prints sanitized scrollback to stdout. `capture` is the compatibility alias. | Optional `-S` / `--start` and inclusive `-E` / `--end`. |
| tmux shim | `lterm tmux-compat capture-pane` | `-p` prints sanitized scrollback; without `-p`, capture is silent and writes the compatibility buffer read by `save-buffer`. `capturep` is the tmux alias. | Optional `-S start-line` and `-E end-line`; end is inclusive. |

Attach/resume is intentionally different: raw attach forwards PTY bytes as-is so
full-screen apps, OSC notifications, and cmux passthrough keep working. Capture
surfaces are non-attached text surfaces and are sanitized for human/AI
consumption.

## Range semantics

Shared numeric rules for `lterm logs -S` / `--start` and
`lterm tmux-compat capture-pane -S` / `-E`:

- Non-negative `-S` and `-E` values are absolute scrollback line indexes.
- Negative values count back from the current scrollback line count.

Product CLI constraints:

- `lterm logs` accepts integer `-S` / `--start` values only.
- `lterm logs` accepts integer `-E` / `--end` values. The end boundary is
  inclusive, so `lterm logs target -S0 -E0` captures only the first line.
- If the inclusive `-E` boundary resolves before the start, `lterm logs`
  returns no lines.

tmux shim additions:

- `capture-pane -E` is inclusive, so `-S0 -E0` captures only the first line.
- If the inclusive `capture-pane -E` boundary resolves before the start, capture
  returns no lines.
- `capture-pane -S top` is accepted as the first line; compact `-Stop` is
  equivalent.
- `capture-pane -S -` or `-E -` leaves that boundary open.

## Parser invariants

`capture-pane` parsing is intentionally one pass and left-to-right:

- `--` terminates option parsing.
- Later `-t` values override earlier target values, matching tmux-style command
  behavior. This is also the shared target-parser policy for other supported
  tmux commands before `--`.
- `-t target`, `-ttarget`, and `-t=target` are accepted.
- `-S value`, `-Svalue`, `-S=value`, `-E value`, `-Evalue`, and `-E=value`
  are accepted.
- `S`, `E`, `b`, and `t` are value-taking short flags for this parser. This
  keeps attached forms such as `-tS1` as target text instead of reinterpreting
  the `S` as a start-line flag.
- `-b` accepts and skips a buffer name. `lterm` exposes one compatibility buffer
  today, so the name is parsed to preserve flag boundaries rather than to select
  among multiple buffers.
- `-b -p` treats `-p` as the buffer name, not as the print flag; that command
  remains silent and writes the compatibility buffer instead of printing.

## Validation anchors

Behavior is covered by:

- `src/main.rs`: `Commands::Logs` product command with `--start` / `--end`
  and `capture` visible alias.
- `src/tmux_compat.rs`: `parse_capture_pane_args`,
  `parse_capture_line_value`, and `capture_pane`.
- `src/server.rs`: `capture_bytes_from_ring` and inclusive end-line tests.
- `tests/cli_smoke.rs`: product `logs` / `capture` help coverage,
  `capture_alias_captures_output`, `logs_supports_inclusive_end_range`, and
  the shared `-S=-20` polling helper.
- `tests/cli_smoke.rs`: `tmux_capture_without_print_is_silent_and_saves_buffer`
  and `tmux_capture_pane_skips_value_options_before_target`.

# Command Surface

Category: reference
Tags: command-surface, cli, aliases, tmux-compat, agent-terminal

## Purpose

`lterm` is a general agent-terminal surface for persistent PTY sessions. Human and agent workflows should prefer product-facing commands such as `start`, `resume`, `open`, `sessions`, `processes`, `logs`, `input`, and `close`. Compatibility names remain available for existing scripts and muscle memory.

## Product CLI vocabulary

| Task | Preferred command | Compatibility names |
| --- | --- | --- |
| Start a persistent process | `lterm start -n api -- npm run dev` | `new` |
| Run a command with tmux compatibility enabled | `lterm run -- codex exec "summarize"` | none (`--no-tmux` opts out) |
| Open or create a session | `lterm open main` | `attach-or-new` |
| Resume an existing session | `lterm resume api` | `attach`, `a`, `-a` |
| List sessions | `lterm sessions` | `list`, `ls` |
| Inspect process trees | `lterm processes api --json` | `ps` |
| Read sanitized scrollback | `lterm logs api --start=-80` | `capture` |
| Write input to a PTY | `lterm input api 'echo hello' --enter` | `send` |
| Stop a session or pane | `lterm close api` | `kill` |
| Run the daemon explicitly | `lterm daemon` | none |
| Stop the daemon and all sessions it owns | `lterm shutdown` | none |

## Utility and agent command surface

These are product CLI commands for agent-terminal setup, remote use, and
integrations. They are not tmux aliases unless they explicitly enter the
`tmux-compat` namespace.

| Task | Product command | Compatibility boundary |
| --- | --- | --- |
| Launch a profiled agent session | `lterm agent claude -- --help` | Sibling shortcuts: `lterm claude`, `lterm codex`, `lterm gemini`, `lterm omx`, `lterm omc` |
| Inspect available agent profiles | `lterm agents --json` | PATH availability probe at command runtime |
| Install the `tmux` compatibility shim | `lterm install-shim` | Creates a shim that forwards to `lterm tmux-compat` |
| Print shell exports for tmux compatibility | `eval "$(lterm env)"` | Fixed `export` lines for trusted shell setup; generated paths are valid POSIX shell tokens from `shlex` quoting, and `PATH` prepends the shim dir to `$PATH` |
| Send a cmux-friendly notification | `lterm notify --title 'Done' --body 'Tests passed'` | OSC 777 fallback drops Unicode controls `U+0000..U+001F`, `U+007F`, and `U+0080..U+009F`, replaces semicolons with spaces, and preserves non-control Unicode text |
| Attach to a remote host | `lterm ssh user@host main` | Use trusted hosts; SSH handles host-key checks, and remote PTY bytes pass through without sanitization |
| Call the tmux shim namespace directly | `lterm tmux-compat list-commands` | Compatibility namespace, not a product alias table |

## Compatibility rules

- `-a` is a positional legacy shortcut parsed before normal subcommand handling and must appear directly after `lterm`: `lterm -a <target>`.
- `attach`, `a`, and `-a` are compatibility entry points for `resume`; they must preserve the same raw `client::attach` path.
- Here, sanitization means escape-sequence/control-byte filtering for non-attached text surfaces such as `logs`, `sessions`, and `processes`; `resume` / raw attach stays a transparent PTY byte stream.
- `notify`'s OSC 777 fallback sanitizes Unicode scalar values after argument parsing, not UTF-8 bytes; non-control text such as Korean remains intact.
- `notify`'s OSC 777 fallback protects protocol framing only; it does not normalize Unicode bidi, format, or zero-width characters inside trusted title/body text.
- `env` export values are POSIX shell tokens produced by `shlex` quoting when metacharacters require it; the visual quote style is not a stable API.
- `env` prepends the shim directory to the caller's existing `$PATH` rather than replacing it.
- `ssh` raw attach can expose local terminal features including OSC 52 clipboard, OSC 8 hyperlinks, title changes, cursor/screen manipulation, bracketed paste toggles, and emulator-specific escapes.
- `run` is the generic product command for an unprofiled tmux-compatible session; it enables the shim by default, hides the legacy no-op `--tmux` flag from help, and uses `--no-tmux` as the visible opt-out.
- Remote `lterm ssh` currently keeps its wire command on compatibility spelling where needed so newer local clients can talk to older remote installs.
- cmux split handoff intentionally sends compatibility `lterm attach <pane>` so stale `LTERM_BIN` builds that predate `resume` still work.

## tmux-compat boundary

`lterm tmux-compat ...` is a shim namespace for scripts that already speak tmux. It is not a second spelling for every product CLI command. Use:

```bash
lterm tmux-compat list-commands
```

to inspect the supported shim subset at runtime.

The shim covers the tmux subset used by common AI orchestration scripts, including session commands, query commands, pane operations, buffers/popups, and deliberate no-op compatibility commands such as `select-pane` and `set-option`. Product-only lifecycle commands such as `daemon` and `shutdown` do not imply tmux-compatible aliases.

## Update policy

When changing command names, aliases, help text, README command tables, or tmux shim command coverage:

1. Verify `README.md` and `README.ko.md` already describe the new product vocabulary; update both together when their user-facing text needs changes.
2. Add or adjust smoke tests in `tests/cli_smoke.rs` for help discoverability and compatibility aliases.
3. Preserve backwards-compatible aliases unless a deliberate deprecation plan exists.

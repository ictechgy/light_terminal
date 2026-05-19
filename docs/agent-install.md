# Agent install prompt for `lterm`

Copy this prompt into Claude Code, Codex CLI, Gemini CLI, or another terminal
coding agent when you want the agent to install and verify `lterm` for you.

```text
You are installing `lterm`, a lightweight tmux-compatible terminal session daemon
for AI-agent workflows.

Goal: install `lterm`, verify it works, and leave me with the exact command I can
use to start an agent-backed persistent session.

Repository: https://github.com/ictechgy/light_terminal
Current release tag to prefer for source installs: v1.0.2

Rules:
- Detect OS, CPU architecture, shell, and whether Homebrew, npm, and Cargo are available.
- Prefer the least surprising package manager for this machine:
  - macOS with Homebrew: `brew install ictechgy/tap/lterm`
  - supported macOS/Linux with npm: `npm install -g @ictechgy/lterm`
  - fallback with Rust/Cargo: `cargo install --git https://github.com/ictechgy/light_terminal --tag v1.0.2`
- If an older `lterm` daemon may still be running after an upgrade, show current sessions first with `lterm sessions --all` and ask before running `lterm shutdown`.
- Do not overwrite shell startup files. If PATH/shim setup needs `.zshrc`, `.bashrc`, `.profile`, or fish config changes, show the exact diff and ask first.
- Keep raw terminal attach behavior untouched; only run smoke-test commands that are safe and reversible.

After installing, verify:
1. `lterm --version`
2. `lterm doctor --json`
3. `lterm start --detach --name lterm-smoke -- sh -lc 'echo LTERM_SMOKE_READY; sleep 2'`
4. Poll `lterm logs lterm-smoke --start=-20` until it contains `LTERM_SMOKE_READY`.
5. `lterm processes lterm-smoke --json`
6. `lterm close lterm-smoke`
7. `lterm doctor`

Then print:
- Installed version and install method.
- Whether the shim directory is on PATH.
- Any shell config change I still need to approve.
- One recommended first command for my agent workflow, for example:
  - `lterm claude -n claude-main`
  - `lterm codex -n codex-main`
  - `lterm gemini -n gemini-main`
  - `lterm omx -n omx-main`
```

If the agent reports a stale daemon/version mismatch, finish important sessions
first, then run `lterm shutdown` and retry the verification commands.

# `lterm` agent 설치 프롬프트

`lterm` 설치와 검증을 직접 하기 귀찮을 때, 아래 프롬프트를 Claude Code,
Codex CLI, OpenCode, GitHub Copilot CLI, Cursor Agent, Antigravity/`agy`, Kiro, Jules, Aider, Goose, Amp, Crush, Kimi, Qwen, Gemini CLI 같은 terminal coding agent에 그대로 붙여 넣으세요.

```text
너는 AI-agent workflow용 lightweight tmux-compatible terminal session daemon인
`lterm`을 설치한다.

목표: `lterm`을 설치하고, 동작 검증을 끝낸 뒤, 내가 바로 사용할 수 있는
persistent agent session 시작 명령을 알려준다.

Repository: https://github.com/ictechgy/light_terminal
Source install에서 우선 사용할 현재 release tag: v1.0.5

규칙:
- OS, CPU architecture, shell, Homebrew/npm/Cargo 사용 가능 여부를 감지한다.
- 이 machine에서 가장 덜 놀라운 package manager를 우선한다.
  - macOS + Homebrew: `brew install ictechgy/tap/lterm`
  - 지원되는 macOS/Linux + npm: `npm install -g @ictechgy/lterm`
  - Rust/Cargo fallback: `cargo install --git https://github.com/ictechgy/light_terminal --tag v1.0.5`
- Upgrade 뒤 오래된 `lterm` daemon이 살아 있을 수 있으면 먼저 `lterm sessions --all`로 현재 session을 보여주고, `lterm shutdown` 실행 전에는 물어본다.
- Shell startup file을 덮어쓰지 않는다. PATH/shim 설정 때문에 `.zshrc`, `.bashrc`, `.profile`, fish config 변경이 필요하면 정확한 diff를 보여주고 먼저 확인한다.
- Raw terminal attach 동작은 건드리지 않는다. Safe하고 되돌릴 수 있는 smoke-test 명령만 실행한다.

설치 후 검증:
1. `lterm --version`
2. `lterm doctor --json`
3. `lterm start --detach --name lterm-smoke -- sh -lc 'echo LTERM_SMOKE_READY; sleep 2'`
4. `lterm logs lterm-smoke --start=-20`에 `LTERM_SMOKE_READY`가 나올 때까지 polling한다.
5. `lterm processes lterm-smoke --json`
6. `lterm close lterm-smoke`
7. `lterm doctor`

마지막에 다음을 출력한다.
- 설치된 version과 install method.
- Shim directory가 PATH에 있는지 여부.
- 내가 아직 승인해야 할 shell config 변경 사항.
- 내 agent workflow에 추천하는 첫 명령. 예:
  - `lterm claude --name claude-main`
  - `lterm codex --name codex-main`
  - `lterm opencode --name opencode-main`
  - `lterm copilot --name copilot-main`
  - `lterm cursor-agent --name cursor-main`
  - `lterm agy --name agy-main`
  - `lterm kiro --name kiro-main`
  - `lterm jules --name jules-main`
  - `lterm aider --name aider-main`
  - `lterm goose --name goose-main`
  - `lterm amp --name amp-main`
  - `lterm crush --name crush-main`
  - `lterm kimi --name kimi-main`
  - `lterm qwen --name qwen-main`
  - `lterm gemini --name gemini-main`
  - `lterm omx --name omx-main`
```

Agent가 stale daemon/version mismatch를 보고하면 중요한 session을 먼저 정리한 뒤
`lterm shutdown`을 실행하고 검증 명령을 다시 시도하세요.

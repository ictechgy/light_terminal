# Light Terminal (`lterm`)

한국어 | [English](README.md) | 🌐 [브라우저용 HTML 가이드](https://ictechgy.github.io/light_terminal/)

## TL;DR

- **무엇** — tmux 같은 영속 터미널 세션 데몬을 더 작게 만든 도구. AI 에이전트 도구를 위한 tmux 호환 명령 계층을 제공하며, 세션을 이름이나 pane id로 detach·reattach할 수 있습니다.
- **대상** — Claude Code, Codex CLI, OpenCode, GitHub Copilot CLI, Cursor Agent, Antigravity/`agy`, Kiro, Jules, Aider, Goose, Amp, Crush, Kimi, Qwen, Gemini CLI, `oh-my-codex` / `oh-my-claude` 같은 terminal-first coding agent를 쓰는 사용자와, 이를 `cmux` 안에서 실행하는 사용자.
- **사용법** — `lterm start`로 만들고 `lterm resume`으로 (재)접속합니다. shim이 적용된 agent 실행에는 `lterm agent <profile>` / `lterm claude` / `lterm codex` / `lterm opencode` / `lterm agy` / `lterm kiro` / `lterm gemini` 같은 내장 단축 명령을 사용할 수 있습니다. tmux가 켜진 세션 안에서는 `tmux` 명령이 `lterm tmux-compat`으로 해석됩니다.
- **상태** — 1.0 명령/출력 호환성 경계를 문서화한 alpha MVP입니다. 같은 OS 사용자 안에서 쓰는 편의용 데몬이며, **샌드박스, escape-sequence sanitizer, 완전한 tmux 대체품 모두 아닙니다.**

---

`lterm`은 tmux 전체를 대체하려는 도구가 아닙니다. 오래 실행되는 PTY 세션을 유지하고, 클라이언트가 자유롭게 detach/reattach할 수 있게 하며, 터미널 escape sequence는 그대로 통과시키고, terminal-first agent 도구가 자주 사용하는 tmux 명령 일부를 호환 shim으로 제공합니다.

> **보안 모델:** `lterm`은 같은 OS 사용자 안에서 쓰는 편의용 데몬이며 샌드박스가 아닙니다. 다른 사용자의 Unix socket 접근은 거부하고 런타임 디렉터리는 소유자 전용 권한으로 만들지만, 같은 OS 사용자 권한으로 실행되는 프로세스는 세션을 제어할 수 있다고 보아야 합니다.
> 전체 trust boundary와 audit policy는 [SECURITY.md](SECURITY.md)를 참고하세요.
> Non-goals(의도적으로 지원하지 않는 항목)는 [docs/non-goals.md](docs/non-goals.md)를 참고하세요.

## 왜 tmux 대신 lterm인가요?

풍부한 pane/window/layout 관리를 원하면 tmux가 맞습니다. `lterm`은 AI
agent가 보통 필요로 하는 더 작은 표면에 집중합니다.

- **Agent-first persistence** — named PTY session이 detached client와 무관하게
  계속 실행되므로, 모든 workflow가 full tmux server를 직접 관리할 필요가
  적습니다.
- **Agent가 기대하는 tmux 호환성** — `lterm tmux-compat`는 Claude Code, Codex
  CLI, OpenCode, GitHub Copilot CLI, Cursor Agent, Antigravity/`agy`, Kiro, Jules, Aider, Goose, Amp, Crush, Kimi, Qwen, Gemini CLI, OMX/OMC 같은 terminal-first tooling이 쓰는 tmux command
  subset을 구현합니다.
- **Raw attach, safe reports** — attach된 PTY stream은 TUI/interactive shell을
  위해 raw로 유지합니다. 대신 `logs`, `capture`, `compose`, `doctor`,
  `diagnose`, `notify` fallback 경로 같은 report surface는 terminal
  control을 제거하면서도 한국어, CJK, emoji 같은 UTF-8 text는 보존합니다.
- **cmux-friendly 설계** — notification과 tmux shim call이 generic desktop
  multiplexer보다 cmux/agent pane orchestration에 맞춰져 있습니다.
- **내장 관측성** — `doctor` / `status`, bounded `logs --start/--end`,
  `wait` / `watch`, `processes --orphans`로 daemon, scrollback, 완료 조건,
  subprocess 상태를 사람이나 agent가 쉽게 확인할 수 있습니다.

## 왜 만들었나

다음 세 가지 요구를 충족하는 것이 목표입니다.

1. **tmux와 비슷한 세션 지속성과 원격 접속** — 세션은 백그라운드 데몬에서 실행되며, 이름이나 pane id로 attach/detach할 수 있습니다. 원격 호스트에 `lterm`이 설치되어 있다면 `lterm ssh`로 접속할 수 있습니다.
2. **cmux 호환성** — cmux 안에서 실행할 때는 OSC 알림을 그대로 통과시키고 `lterm notify`를 제공하며, tmux shim은 가능한 경우 worker pane을 cmux native split으로 엽니다.
3. **AI 도구 지원** — `lterm agent <profile>`, `lterm claude`, `lterm codex`, `lterm opencode`, `lterm copilot`, `lterm cursor-agent`, `lterm agy`, `lterm jules`, `lterm kiro`, `lterm aider`, `lterm goose`, `lterm amp`, `lterm crush`, `lterm kimi`, `lterm qwen`, `lterm gemini`, `lterm omx`, `lterm omc`, `lterm install-shim`은 tmux를 전제하는 agent 도구를 위해 가짜 `tmux` 명령과 `TMUX` / `TMUX_PANE` 환경 변수를 제공합니다.

cmux 호환 동작은 cmux가 문서화한 기능을 따릅니다. cmux는 `cmux notify`와 OSC 777 / OSC 99 알림, workspace/split을 위한 Unix socket·CLI API, 그리고 tmux 명령을 cmux native pane으로 매핑하는 tmux shim 모델을 문서화하고 있습니다.

## 설치

Homebrew로 설치:

```bash
brew install ictechgy/tap/lterm
```

지원되는 macOS/Linux 플랫폼에서는 npm으로 설치할 수 있습니다.

```bash
npm install -g @ictechgy/lterm
```

Homebrew와 npm 모두 `PATH`에 `lterm` 명령을 설치합니다. `lterm --version`으로 확인하세요.

수동 설치가 번거롭다면 [`docs/agent-install.ko.md`](docs/agent-install.ko.md)의
프롬프트를 Claude Code, Codex CLI, OpenCode, GitHub Copilot CLI, Cursor Agent, Antigravity/`agy`, Kiro, Jules, Aider, Goose, Amp, Crush, Kimi, Qwen, Gemini CLI 같은 terminal coding agent에
붙여 넣으세요. Agent가 platform을 감지하고, `lterm`을 설치하고, smoke test로
검증하며, shell startup file을 바꿔야 할 때는 먼저 diff를 보여주도록 안내합니다.

1.0 명령/출력 안정성 경계는 [public contract](docs/public-contract.md)와
machine-readable [contract manifest](docs/contract-manifest.json)를 참고하세요.

GitHub에서 Cargo로 설치할 때는 release tag를 고정하세요. 아래 예시는 현재
README 릴리스 기준이며, 더 최신 tag가 있는지는 Releases 페이지에서 확인하세요:

```bash
cargo install --locked --git https://github.com/ictechgy/light_terminal --tag v1.0.14
```

저장소를 클론한 뒤 직접 빌드하려면 Rust 1.85 이상이 필요합니다.

```bash
cargo build --release --locked
./target/release/lterm --help
```

개발 중에는 다음처럼 실행할 수 있습니다.

```bash
cargo run -- --help
```

터미널에서 tmux shim을 사용하려면:

```bash
lterm install-shim
# 출력된 디렉터리를 실제 tmux보다 앞쪽 PATH에 추가하거나, helper 출력을 확인한 뒤 실행하세요.
lterm env
eval "$(lterm env)"
# fish:
lterm env --shell fish | source
```

## 빠른 시작

**세션을 만들고 바로 attach:**

```bash
lterm start -n api -- npm run dev
```

**먼저 detached로 만든 뒤 나중에 attach:**

```bash
lterm start -d -n api -- npm run dev
lterm resume api

# 호환 이름도 계속 사용할 수 있습니다.
lterm attach api
lterm a api
# `-a`는 `lterm` 바로 뒤에 쓰고, target과는 공백으로 구분하세요.
lterm -a api
```

**Agent terminal 명령어:**

| 작업 | 일반 명령 | 호환 이름 |
| --- | --- | --- |
| 영속 프로세스 시작 | `lterm start -n api -- npm run dev` | `new` |
| tmux 호환을 켠 상태로 명령 실행 | `lterm run -- codex exec "요약해줘"` | 없음 (`--no-tmux`로 opt out) |
| 세션 열기 또는 생성 | `lterm open main` | `attach-or-new` |
| 기존 세션 재개 | `lterm resume api` | `attach`, `a`, `-a` |
| 모바일에서 agent 출력 확인 | `LTERM_MOBILE=1 lterm resume codex-lterm` | transcript 강제는 `--mobile`, 기존 raw attach 강제는 `--raw` |
| 세션 목록 보기 | `lterm sessions` | `list`, `ls` |
| 프로세스 트리 확인 | `lterm processes api --json --orphans` | `ps` |
| 세션 이름 변경 | `lterm rename api api-renamed` | 없음 |
| 세션 status theme 설정 | `lterm status-theme api green` | `theme` |
| 정제된 scrollback 읽기 | `lterm logs api --start=-80 --end=-1` | `capture` |
| 디버깅용 raw PTY 출력 기록 | `lterm trace api --duration 5s --output trace.jsonl` | `record` |
| 신뢰하는 raw PTY trace 재생 | `lterm trace-replay trace.jsonl` | `replay-trace` |
| 정제된 scrollback 위에 입력 컴포저 열기 | `lterm compose api` | `mobile` |
| 세션 출력 또는 종료 대기 | `lterm wait api --contains READY --timeout 30s --json` | 없음 |
| 세션을 감시하고 완료 시 알림 | `lterm watch api --exit --notify` | 없음 |
| PTY에 입력 쓰기 | `lterm input api 'echo hello' --enter` | `send` |
| 세션 종료 | `lterm close api` | `kill` |
| 데몬과 shim 상태 진단 | `lterm doctor --json` | `status` |
| redacted 로컬 진단 bundle 수집 | `lterm diagnose --bundle` | 없음 |
| 로컬 설정 단계 미리 보기 | `lterm init --shell zsh` | 없음 |
| shell completion 생성 | `mkdir -p ~/.zfunc && lterm completions zsh > ~/.zfunc/_lterm` | 없음 |
| 백그라운드 데몬 명시 실행 | `lterm daemon` | 없음 |
| 데몬과 모든 세션 종료 | `lterm shutdown` | 없음 |

에이전트·shim 유틸리티도 제품 CLI 명령이며, tmux alias가 아닙니다:

| 작업 | 제품 명령 | 호환 경계 |
| --- | --- | --- |
| profile 기반 agent 세션 실행 | `lterm agent claude -- --help` | sibling shortcuts: `lterm claude`, `lterm codex`, `lterm opencode`, `lterm copilot`, `lterm cursor-agent`, `lterm agy`, `lterm jules`, `lterm kiro`, `lterm aider`, `lterm goose`, `lterm amp`, `lterm crush`, `lterm kimi`, `lterm qwen`, `lterm gemini`, `lterm omx`, `lterm omc` |
| 사용 가능한 agent profile 확인 | `lterm agents --json` | 실행 시점의 `PATH` 사용 가능 여부 확인 |
| `tmux` 호환 shim 설치 | `lterm install-shim` | `lterm tmux-compat`으로 전달하는 shim 생성 |
| tmux 호환 shell export 출력 | `eval "$(lterm env)"` (`lterm env --shell fish \| source` for fish) | shim dir을 `$PATH` 앞에 추가하는 신뢰된 `export` 행 출력 |
| shell completion 생성 | `lterm completions bash\|zsh\|fish` | completion script만 출력하며 session을 조회하거나 daemon을 시작하지 않음 |
| cmux-friendly 알림 보내기 | `lterm notify --title 'Done' --body 'Tests passed'` | OSC 777 fallback은 터미널 제어 문자를 제거하고 Unicode text는 보존 |
| 원격 호스트에 attach | `lterm ssh user@host main` | 신뢰할 수 있는 host에서만 사용; host-key 확인은 SSH가 처리하고 remote PTY bytes는 정제 없이 전달 |
| tmux shim namespace 직접 호출 | `lterm tmux-compat list-commands` | 제품 alias 표가 아니라 호환 namespace |

`eval "$(lterm env)"`는 `PATH`의 `lterm` binary를 신뢰할 수 있을 때만 사용하세요.
이 명령은 shim directory를 `$PATH` 앞에 추가하는 고정 `export` 행을 출력합니다.
fish에서는 같은 확인 뒤 `lterm env --shell fish | source`를 사용하세요.
eval/source를 쓰고 싶지 않다면 먼저 `lterm env` 출력을 직접 확인한 뒤 필요한
export 행만 shell startup file에 복사하세요.

`lterm ssh`는 remote PTY bytes를 local terminal로 정제 없이 전달하므로,
compromised remote는 local terminal emulator가 허용하는 control sequence를
구동할 수 있습니다. 예를 들어 OSC 52 clipboard 쓰기, OSC 8 hyperlink,
window/title 변경, cursor/screen 조작, bracketed paste 토글, emulator별
escape 처리가 여기에 포함됩니다. 직접 `ssh`하듯 신뢰할 수 있는 host에만
사용하고 terminal feature 설정도 그에 맞게 관리하세요. "cmux-friendly"
알림은 fallback 경로가 cmux가 감시하는 OSC 777 notification 형식을
출력한다는 뜻입니다. OSC 777 fallback sanitizer는 protocol framing을
보호하는 범위이며, 신뢰된 title/body 내부의 Unicode bidi/format/zero-width
문자를 normalize하지 않습니다.

호환 이름은 앞에 flag 형태로 표시된 경우를 제외하면 subcommand입니다. `-a`는 기존 shortcut 형태라 `lterm -a <target>`처럼 사용해야 합니다.

이 표는 사람과 agent가 직접 쓰는 제품 CLI 표면입니다. `lterm tmux-compat ...`는 이미 tmux 명령을 사용하는 스크립트를 위한 별도 shim namespace이며, 모든 제품 명령에 tmux 호환 이름이 있는 것은 아닙니다. 런타임에 지원되는 shim subset은 `lterm tmux-compat list-commands`로 확인하세요.

`lterm sessions`는 기본적으로 하위 pane을 숨기고, 기존 첫 5개 tab-separated 열(`name`, `pane`, `alive`, `cwd`, `command`)을 유지한 뒤 attach 상태(`attached` / `detached`)와 parent pane(`-` 또는 pane id)을 뒤에 붙입니다. JSON 출력에는 agent profile로 띄운 세션에 한해 `agent_name` metadata가 추가되고, 일반 세션에는 이 field가 생략됩니다. 호환 이름인 `lterm list`와 `lterm ls`도 같은 text 출력 형식을 유지합니다. attach된 클라이언트는 아래쪽 한 줄에 status bar를 표시하고, PTY는 그 줄을 제외한 영역으로 resize됩니다. 예전처럼 전체 터미널을 raw 모드로 강제하려면 `lterm resume --raw --no-status api`(호환 이름: `lterm attach --raw --no-status api`)를 쓰거나 `LTERM_ATTACH_MODE=raw`를 설정하세요. 단순히 status line만 충돌하는 클라이언트에서는 `LTERM_NO_STATUS=1` 또는 같은 의미의 `LTERM_STATUS=0`을 함께 사용하면 됩니다.

row status 존재 여부는 attach mode와 별도입니다. `--attach-mode=auto`는 계속 raw attach와 mobile transcript 중 어느 transport를 쓸지만 결정합니다. raw attach 경로에서 일반 세션은 기본적으로 row status를 유지하고, 내장 agent launcher는 full-height row-off surface를 기본값으로 사용하며 terminal이 지원하면 terminal title cue를 emit합니다. row-on shell 세션 안에서 나중에 알려진 agent command가 child process로 실행된 것으로 보이면 lterm은 best-effort로 row를 suspend하고 PTY를 전체 높이로 복원했다가 agent가 끝나면 row를 되돌릴 수 있습니다. process 감지가 애매하면 안전하게 row를 유지합니다. 전역 `LTERM_NO_STATUS=1` / `LTERM_STATUS=0` kill-switch는 CLI status 요청보다 우선합니다.

`lterm resume` / `lterm open`의 기본 attach 정책은 `--attach-mode=auto`(또는 `LTERM_ATTACH_MODE=auto`)입니다. attach mode 값은 `auto`, `raw`, `mobile` 세 가지이고, 여기서 `mobile`은 일반 화면(normal screen)의 transcript view를 뜻합니다. 데스크톱에서는 기존처럼 raw PTY attach를 사용합니다. 자동 모바일 감지는 보수적인 best-effort 동작입니다. `LTERM_MOBILE=1` 또는 Termius 터미널 식별값이 있으면 모바일 클라이언트로 보고, 대상 세션은 persisted `LTERM_AGENT` metadata, 내장 agent의 `*-lterm` 세션 이름, 또는 알려진 agent command basename 중 하나에 맞아야 agent 세션으로 봅니다. 스크립트처럼 결과가 반드시 예측 가능해야 하는 경우에는 명시적으로 지정하세요. 기존 raw 경로를 강제로 쓰려면 `--raw` 또는 `LTERM_ATTACH_MODE=raw`, transcript를 강제로 쓰려면 `--mobile` 또는 `LTERM_ATTACH_MODE=mobile`을 사용합니다. CLI flag가 환경 변수보다 우선합니다. transcript의 표시 범위와 갱신 주기는 `--tail`, `--refresh`, `--read-only`로 조정할 수 있습니다.

`lterm rename <target> <new-name>`은 실행 중인 세션의 프로세스를 재시작하지 않고 이름만 바꿉니다. 현재 이름과 동일한 이름으로 바꾸면 no-op success이고, 다른 세션이 이미 쓰는 이름으로 바꾸면 conflict error로 실패합니다. `<target>`은 세션 이름, session id, pane id(`%0`), 또는 bare pane 번호(`0`)를 받습니다. 숫자만 있는 target은 세션 이름이 아니라 bare pane 번호로 해석됩니다(세션 이름은 숫자만으로 만들 수 없습니다). `<new-name>`은 `--name`과 같은 이름 규칙을 따릅니다.

`lterm status-theme <target> <theme>`(alias: `lterm theme`)은 PTY를 재시작하지 않고 세션별 status bar theme을 저장합니다. pane id를 지정하면 해당 pane이 속한 세션에 적용됩니다. `default`, `clear`, `none`을 쓰면 세션 override를 지우고 attach하는 client의 기본값으로 돌아갑니다. 이미 attach된 client는 detach 후 다시 attach할 때 새 색을 반영합니다. 새 세션은 `lterm start --status-theme green -n api -- npm run dev`(또는 alias `--status-color`)처럼 생성 시점에 같은 metadata를 저장할 수 있습니다.

`lterm doctor`(호환 이름: `lterm status`)는 client/daemon version, protocol 호환성, runtime/data/socket/shim path, shim directory가 `PATH`에 있는지 등을 보고합니다. 이 명령은 daemon을 시작하지 않습니다. 현재 socket에서 호환 daemon이 응답하지 않으면 `daemon_reachable=no` / `false`로 표시됩니다. 일반 client 동작 중 접근 가능한 daemon이 다른 lterm 또는 protocol version을 보고하면 stderr에 경고를 출력하며, 보통 binary upgrade 뒤 예전 daemon이 살아 있는 상황을 뜻합니다.

`lterm logs <target>`은 `--start` / `-S`와 `--end` / `-E` line offset을 받습니다. 0 이상의 값은 absolute scrollback line index이고, 음수 값은 현재 scrollback line count에서 뒤로 셉니다. `--end`는 inclusive라 `lterm logs api -S0 -E0`은 첫 번째 줄만 capture합니다. Capture 출력은 계속 정제된 text입니다. 즉 terminal control은 제거하고 한국어, CJK, emoji 같은 UTF-8 text는 보존합니다. attach된 PTY stream은 raw 그대로 유지됩니다.

`lterm trace <target> --duration 5s --output trace.jsonl`은 timestamp와
hex-encoded bytes를 담은 private local JSONL 파일로 raw PTY 출력 chunk를
기록합니다. 간헐적인 render 문제를 opt-in으로 디버깅하기 위한 기능이며,
recorder는 JSONL artifact만 쓰고 raw capture는 `--max-bytes`(기본 16 MiB)
한도 안에서 멈춥니다. 기존 trace file은 `--force` 없이는 덮어쓰지 않습니다.
재생은 신뢰할 수 있는 trace에만 `lterm trace-replay <file>`로 수행하세요.
`trace-replay`는 raw terminal bytes를 출력하기 전에 JSONL 전체를 먼저
검증하고, 기본 trace capture 크기와 trace당 chunk 수 한도를 적용합니다.

`lterm wait <target> --exit / --contains <text>`는 세션이 종료되거나 정제된 scrollback에 marker가 나타날 때까지 block합니다. 자동화용 health check에는 `--timeout 250ms|2s|5m|1h`, `--tail N`, `--json`을 함께 쓰세요. Timeout 시 `wait` / `watch`는 exit code `124`를 반환하고 JSON에는 `timed_out: true`가 들어갑니다. `lterm watch`는 같은 조건을 쓰며, `--notify`를 더하면 attach된 PTY bytes는 건드리지 않고 cmux-friendly 완료 알림을 보냅니다. `--json`을 함께 쓸 때는 notification fallback이 필요해도 stdout을 machine-readable JSON으로 유지합니다.
지나치게 큰 `--contains` needle은 명시적 오류로 거부되며, daemon은 동시에
block 중인 `wait` / `watch` check 수를 제한해 자동화가 무제한 waiter를
만들지 못하게 합니다.

`LTERM_STATUS_STYLE=full` 또는 `LTERM_STATUS_STYLE=minimal` 로 시각 스타일을 선택할 수 있습니다. `full`(로컬 터미널 기본값)은 색이 있는 bar를 그리고, `minimal`은 SGR 색을 모두 생략한 plain text로 동작합니다. SSH 세션(`SSH_CONNECTION` / `SSH_CLIENT` / `SSH_TTY` 감지)과 Termius 계열 클라이언트(터미널 식별 환경 변수 감지)에서는 자동으로 `minimal`이 적용되어 모바일 색상 매핑 문제를 줄이지만, 세션 또는 환경 theme을 명시하면 색이 유지됩니다.

`LTERM_STATUS_THEME=blue|green|magenta|cyan|amber|red|gray|plain` 으로 attach client의 기본 status bar 색을 바꿀 수 있습니다. 세션별 override가 환경값보다 우선합니다: `lterm start --status-theme amber -n api -- npm run dev`, `lterm run --status-color cyan -- cargo test`, `lterm status-theme api plain`. 이 변수를 shell startup 파일에서 export하면 SSH attach도 colored status bar로 opt-in됩니다. 모바일 SSH client에서 plain text가 필요하면 unset하거나 `LTERM_STATUS_STYLE=minimal`을 설정하세요. Theme 이름은 고정 allowlist에서만 파싱되며, lterm은 사용자 입력 escape sequence를 status row에 임의 삽입하지 않습니다.

### Status bar 커스터마이징

Status bar theme은 v0.1.3에서 추가되었습니다. 이 기능은 metadata만 바꿉니다. Theme을 바꿔도 PTY가 재시작되지 않고, attach된 PTY byte stream도 바꾸거나 정제하지 않으며, 사용자 입력으로 임의 terminal escape sequence를 status row에 넣지 않습니다.

원하는 범위에 맞춰 가장 좁은 설정을 사용하세요:

| 범위 | 예시 | 언제 쓰나요 |
| --- | --- | --- |
| 새 세션 1개 | `lterm start --status-theme green -n api -- npm run dev` | service나 agent 세션을 이후 attach에서도 쉽게 구분하고 싶을 때. |
| 기존 세션 | `lterm status-theme api amber` | 실행 중인 process를 재시작하지 않고 색만 바꿀 때. |
| Agent launcher 세션 | `lterm codex --status --status-color cyan -- exec "summarize"` | long-only launcher 옵션을 유지하면서 agent 세션에 지속 색상을 줄 때. |
| Attach client 기본값 | `export LTERM_STATUS_THEME=magenta` | 세션 override가 없는 session의 기본 색을 바꾸고 싶을 때. |
| Plain/minimal client | `export LTERM_STATUS_STYLE=minimal` | 모바일 SSH client나 색상 매핑이 불안한 terminal에서 text-only status를 선호할 때. |

허용되는 theme은 고정 목록입니다:

| Theme | 추천 용도 |
| --- | --- |
| `blue` | 로컬 status bar 기본값. |
| `green` | 오래 실행되는 service 또는 정상/background 작업. |
| `magenta` | 빠르게 구분하고 싶은 agent 또는 review 세션. |
| `cyan` | build/test/dev-tool 세션. |
| `amber` | 주의가 필요한 watch/diagnostic 세션. |
| `red` | 위험하거나 destructive이거나 production에 가까운 세션. |
| `gray` | 낮은 우선순위의 background 세션. |
| `plain` | 색상 bar 없이 status row만 유지하고 싶을 때. |

세션 override는 `lterm status-theme api default`(또는 `clear` / `none`)로 지웁니다. 이미 attach된 client는 detach 후 reattach할 때 새 색이 반영되므로, 사람이 붙어 있는 동안 scripted 변경을 적용해도 안전합니다.

attach된 PTY가 alternate screen buffer로 진입하면(예: `vim`, `less`, `htop`이 `\x1b[?1049h` 사용) lterm은 status bar를 일시 중단해 alt-screen 앱의 UI와 충돌을 피합니다. 앱이 alt-screen을 종료하는 즉시 status bar가 다시 그려집니다.

`lterm resume` / `lterm attach` 도중 panic이나 예기치 않은 종료가 lterm의 recovery hook을 거치면 최소 복구 sequence(scroll region 리셋, 커서 보이기, alt-screen 종료, SGR 리셋)를 best-effort로 emit해 사용자 터미널이 raw mode나 hidden cursor 상태로 남을 가능성을 줄입니다.

CJK 문자나 이모지(ZWJ family, 국기, 결합 문자 포함)가 들어간 세션 이름은 `unicode-width` / `unicode-segmentation` 으로 디스플레이 폭을 계산해 정렬되므로 wide character가 섞여도 status bar 패딩이 어긋나지 않습니다.

child 애플리케이션이 `CSI u` enhancement sequence로 Kitty keyboard protocol을 켜면, lterm은 이를 추적했다가 attach 종료 시 terminal keyboard mode를 best-effort로 복원합니다. 그래서 child가 비정상 종료된 뒤 shell 입력이 `1;1:3u` 같은 escape 조각으로 보이는 상황을 줄입니다.

**세션 확인 및 제어:**

`--children`는 관리되는 자식 pane을 포함하고, `--all`은 기본 목록에서 숨겨지는 세션까지 포함합니다.

```bash
lterm sessions
lterm sessions --children
lterm sessions --all
lterm processes api --orphans
lterm logs api --start=-80 --end=-1
lterm compose api
LTERM_MOBILE=1 lterm resume codex-lterm
lterm resume --raw codex-lterm
lterm wait api --contains READY --timeout 30s --json
lterm watch api --exit --notify
lterm input api 'echo hello' --enter
```

위의 일반 alias는 tmux 용어를 몰라도 agent terminal을 일상적으로 다루기 쉽게 하기 위한 표면입니다. `sessions`는 영속 작업을 나열하고, `processes`는 child process tree를 확인하고, `logs`는 정제된 scrollback을 읽고, `compose`는 정제된 scrollback과 하단 고정 prompt로 텍스트를 commit할 수 있게 하며, 모바일 transcript attach는 긴 agent 출력을 휴대폰의 기본 scrollback으로 읽을 수 있게 해 줍니다. `wait` / `watch`는 marker 또는 종료 조건을 script와 agent가 관측할 수 있게 하고, `input`은 대상 PTY에 텍스트를 씁니다. `lterm mobile`은 `lterm compose`의 visible alias이고, 별개의 attach flag인 `--mobile`은 normal-screen transcript attach 경로를 선택합니다. 호환 이름 `list` / `ls`, `ps`, `capture`, `send`는 스크립트와 기존 사용 습관에서도 계속 사용할 수 있습니다.

자동화와 테스트에는 `lterm compose api --once --message 'hello'`를 사용하면 한 번의 정제된 capture/send 사이클을 실행합니다. `logs`와 같은 session-or-pane target 모델에서 마지막 `--tail` 정제 라인(기본값: 80)을 capture한 뒤, 기본으로 Enter(`\r`)를 붙여 `lterm input --enter`와 맞추며, `--no-enter`를 추가하면 message byte만 정확히 보냅니다. `compose` / `mobile`은 attach client가 아니며 attached-client 수나 PTY geometry를 바꾸지 않습니다.
Interactive compose 화면은 `--refresh`(기본값: 500ms), 로컬 입력, 터미널 resize 이벤트마다 갱신됩니다. Enter를 누르면 현재 입력 buffer를 commit하고(빈 buffer도 commit됨), 위 one-shot 규칙처럼 기본으로 `\r`을 덧붙입니다. Ctrl-C, Ctrl-D, Esc는 PTY로 전달하지 않고 로컬 composer를 종료합니다.
`lterm compose api --transcript`를 사용하면 모바일 auto attach가 쓰는 것과 같은 normal-screen transcript UI를 직접 열 수 있습니다. alternate-screen composer 없이 정제된 scrollback과 간단한 line input만 쓰고 싶을 때 적합합니다. 출력만 보고 싶으면 `--read-only`를 추가하세요.

**세션 종료:**

```bash
lterm close api
```

`kill`은 `close`의 visible compatibility alias입니다. 두 이름 모두 같은 session/pane 종료 경로를 사용합니다.

**daemon 명시 실행 (고급):**

```bash
# 일반 client 명령은 필요할 때 daemon을 시작합니다. supervisor/debugging 용도로 직접 실행하세요.
lterm daemon
```

**daemon과 그 daemon이 소유한 모든 세션 종료:**

```bash
# 단일 세션 close가 아니라 daemon-wide 종료입니다.
lterm shutdown
```

## AI 워크플로

**자주 쓰는 agent CLI를 shim이 적용된 세션에서 실행:**

```bash
lterm claude
lterm codex
lterm opencode
lterm copilot
lterm cursor-agent
lterm agy -- -p "이 저장소를 요약해줘"
lterm kiro
lterm jules
lterm aider
lterm goose
lterm amp
lterm crush
lterm kimi
lterm qwen
lterm gemini -- -p "이 저장소를 요약해줘"  # Gemini CLI도 -p를 받습니다
lterm agents
```

위 명령은 built-in profile로 연결되는 얇은 alias입니다. 예를 들면 다음과 같습니다.

```bash
lterm agent claude
lterm agent codex
lterm agent opencode
lterm agent cursor-agent
lterm agent agy -- -p "이 저장소를 요약해줘"
lterm agent qwen
lterm agent gemini -- -p "이 저장소를 요약해줘"
```

agent launcher는 built-in profile과 custom `lterm agent <profile>` 실행에서 같은 세션 제어 옵션을 받습니다.

```bash
lterm claude --name repo-review --cwd /path/to/repo
lterm codex --detach --name repo-codex -- exec "이 저장소를 요약해줘"
lterm codex --mobile --tail 200 --refresh 1s --read-only
lterm agy --status -- -p "lterm status를 유지해줘"
```

Claude/Codex/OpenCode/Copilot/Cursor Agent/Antigravity/Kiro/Jules/Aider/Goose/Amp/Crush/Kimi/Qwen/Gemini/OMX/OMC profile의 기본 attach 정책은 `auto`입니다. 데스크톱에서는 lterm status bar를 끈 raw full-terminal attach를 사용하므로 각 도구의 자체 TUI/status/alternate-screen 렌더링이 그대로 동작합니다. Termius 계열 모바일 클라이언트에서는 `auto`가 위에서 설명한 normal-screen transcript로 전환되어 긴 agent 출력을 모바일 기본 scrollback으로 읽을 수 있습니다. raw attach를 강제하려면 `--raw`, transcript를 강제하려면 `--mobile`을 사용하세요. `--status`는 raw 경로에서 lterm status bar를 요청하고, raw launch/profile에서 표시되는 status bar는 `--no-status`로 숨길 수 있습니다. `--status`는 agent 디버깅용 best-effort override라 agent TUI와 충돌할 수 있으며, `--mobile --status`는 mobile transcript가 자체 UI를 소유하므로 raw status row를 만들지 않습니다. agent에 넘길 인자가 lterm launch option처럼 보일 수 있으면 앞에 `--`를 두세요. `lterm agent <name>`은 `PATH`에서 찾을 수 있는 안전한 bare command name이면 바로 동작하므로, 예를 들어 `lterm agent qwen-code`처럼 미래/서드파티 agent도 쓸 수 있습니다. `lterm run -- <command>`는 더 낮은 수준의 tmux-compatible primitive를 직접 쓰고 싶을 때만 사용하세요.

launcher 제어 옵션은 agent의 흔한 short flag(`-c` 등)를 빼앗지 않도록 long-only(`--name`, `--cwd`, `--detach`, `--status`, `--no-status`, `--status-theme`, `--status-color`, `--attach-mode`, `--raw`, `--mobile`, `--tail`, `--refresh`, `--read-only`)입니다. 이 옵션들은 `claude`, `codex`, `opencode`, `copilot`, `cursor-agent`, `agy`, `kiro`, `jules`, `aider`, `goose`, `amp`, `crush`, `kimi`, `qwen`, `gemini`, `omx`, `omc`, `agent <profile>`에 동일하게 적용됩니다. 해당 agent 세션이 이후 attach에서도 특정 lterm status 색을 유지하게 하려면 `--status-theme` / `--status-color`를 사용하세요.
`--detach`는 각 field의 control character와 Unicode line/paragraph separator를 공백으로 바꾼 `name<TAB>pane<TAB>command`를 출력하며, 나중에 `lterm resume <name>` 또는 호환 이름 `lterm attach <name>`으로 다시 붙으면 됩니다. detach record에는 `--cwd`가 포함되지 않으므로 나중에 필요하면 session을 조회하세요.
명시한 `--name`은 lterm의 일반 session-name 문법을 따르고 사용 중이지 않아야 합니다. 충돌 시 자동 suffix를 붙이지 않고 conflict error로 실패합니다.
이름에는 ASCII 문자/숫자와 `.`, `_`, `-`만 사용할 수 있고, `-` 또는 `%`로 시작할 수 없으며, 숫자만으로 이뤄질 수 없고, UUID처럼 보이면 안 되고, 128바이트를 넘을 수 없습니다.
`lterm agents` 또는 `lterm agents --json`으로 profile 기본값과 현재 `PATH`에서 binary를 찾을 수 있는지 확인할 수 있습니다. JSON row의 `kind` 값은 다음 중 하나입니다.

- `built-in`: lterm이 기본 제공하는 `claude`, `codex`, `kiro` 같은 profile.
- `custom`: 사용자가 요청한 safe bare command name에서 만든 임시 profile.
- `configured`: 명시적인 `--agent-config` 파일에서 읽은 profile.

`lterm agents codex my-agent --json`처럼 profile 이름을 넘기면 선택한 built-in/custom/configured profile만 확인합니다. availability는 실행 시점의 PATH probe입니다. Built-in은 `lterm agents`에 표시되는 binary 이름을 그대로 `PATH`에서 찾습니다. 대부분 profile 이름과 같지만 `kiro`는 `kiro-cli`를 사용합니다. 따라서 `lterm kiro`에는 `PATH`에서 찾을 수 있는 `kiro-cli`가 필요합니다. provider가 다른 command 이름으로 설치된다면 추측 alias에 의존하지 말고 `lterm agent <command>`나 configured profile을 사용하세요.
반복해서 쓰는 custom alias는 명시적인 JSON config 파일로 넘길 수 있습니다.

```bash
cat > agents.json <<'JSON'
{ "profiles": [{ "name": "repo-review", "binary": "codex", "session_base": "repo-review-session", "status_default": false }] }
JSON
lterm agents --agent-config agents.json --json
lterm agent repo-review --agent-config agents.json -- exec "이 저장소를 리뷰해줘"
```

configured name과 binary는 `lterm agent <profile>`과 같은 안전한 profile 문법을 사용하며, built-in 이름은 재정의할 수 없습니다.
Configured profile 규칙:

- `binary`는 shell fragment나 path가 아니라 `PATH`에서 찾는 bare command name이어야 합니다.
- `binary`의 기본값은 `name`입니다.
- `session_base`의 기본값은 `<name>-lterm`입니다.
- `status_default`의 기본값은 `true`이며, field가 있을 때는 boolean이어야 합니다.
- 중복 이름과 알 수 없는 JSON field는 거부됩니다.
- `--agent-config`를 넘긴 경우 built-in이 아닌 선택 이름은 그 파일 안에 있어야 합니다.

**Oh My Codex를 shim이 적용된 세션에서 실행:**

```bash
lterm omx team
# omx에 넘길 추가 flag는 그대로 전달됩니다.
lterm omx --madmax --xhigh
```

**Oh My Claude도 같은 방식으로 실행:**

```bash
lterm omc team
# 설치된 `omc --help`에 --xhigh가 보이지 않는다면 --xhigh 없이 --madmax만 사용하세요.
lterm omc --madmax
```

**임의의 명령을 tmux 호환 모드로 실행:**

```bash
lterm run -- omx hud --tmux
lterm run -- claude
lterm run -- codex exec "저장소를 요약해줘"
```

이 세션 안에서는 `tmux`가 `lterm tmux-compat` shim으로 해석됩니다. 이 shim은 호환 계층이지 모든 `lterm` 제품 명령의 두 번째 철자가 아닙니다. 현재 shim은 AI orchestration 스크립트가 자주 사용하는 다음 명령 subset을 구현합니다.

- **세션** — `new-session`, `attach-session`, `has-session`, `list-sessions`, `rename-session`, `kill-session`
- **조회** — `list-windows`, `list-clients`, `list-commands`, `show-options`, `show-window-options`
- **Pane** — `split-window`, `list-panes`, `display-message`, `capture-pane`, `send-keys`, `kill-pane`, `resize-pane`
- **Buffer / popup** — `display-popup`, `wait-for`, `load-buffer`, `save-buffer`, `paste-buffer`
- **호환용 no-op** — `select-pane`, `select-layout`, `set-hook`, `set-option`, `set-window-option`, `set-environment`, `show-environment`

호환성 참고: lterm은 각 root session을 하나의 pseudo-window로 모델링합니다
(`window_index=0`, `window_panes=1`). lterm은 client별 process/TTY metadata를
노출하지 않기 때문에 `client_pid`와 `client_tty`는 빈 문자열로 확장됩니다.
`set-hook`은 OMX `client-resized[...]` handler처럼 agent runtime이 쓰는 hook
등록/해제 형태를 받아들이지만, lterm이 tmux hook dispatcher를 실행하지는 않습니다.
Detached `split-window -d -t <target>`은 같은 daemon socket 안에서 기존 live lterm
target이면 현재 pane이 아니어도 허용합니다. 이는 tmux의 cross-pane helper launch
동작을 맞추기 위한 것이며, daemon은 요청 처리 전에 같은 OS 사용자 peer credential을
검증합니다. detached helper는 target pane 안에 붙지 않고 별도 lterm session으로
생성됩니다.
`lterm tmux-compat list-commands --verbose`는 `command`, alias, support tier,
usage를 tab-separated로 출력하고, `--json`은 machine-readable row를 출력합니다.
Support tier는 lterm compatibility boundary 안에서 `full`, `partial`, `noop`
중 하나입니다. `LTERM_DEBUG_TMUX=1`을 설정하면 지원하지 않는 tmux command가
shim에 도달했을 때 opt-in stderr diagnostic row를 출력합니다.
tmux `-f` filter는 조용히 무시하지 않고 의도적으로 거부합니다.

Status bar redraw는 보수적으로 allowlist된 terminal client(예: xterm/iTerm2/WezTerm
identity)에서만 xterm SGR stack control을 사용해 실행 중인 TUI의
foreground/background color state를 빼앗지 않도록 합니다.
`LTERM_STATUS_SGR_STACK=0`으로 `CSI # {` / `CSI # }`를 끌 수 있고, 클라이언트가
해당 private CSI를 지원하거나 안전하게 무시함을 확인했다면 `1`로 강제할 수 있습니다.
Kitty, Alacritty, Ghostty, Termius, generic `TERM=xterm-*` 값은 SGR-stack 동작이
검증될 때까지 opt-in 경로로 둡니다.

## cmux 동작

`lterm tmux-compat split-window`가 cmux 환경(`CMUX_WORKSPACE_ID`, `CMUX_SURFACE_ID`, 또는 cmux socket)을 감지하면 다음 순서로 동작합니다.

1. worker 명령을 위한 새 `lterm` PTY 세션을 시작합니다.
2. cmux에 native split 생성을 요청합니다 (`cmux new-split right/down`).
3. 생성된 split에는 호환 명령인 `lterm attach <pane>`을 보냅니다. 안전한 absolute executable이 `LTERM_BIN`으로 지정되어 있으면 그 값을 사용하고, 아니면 현재 executable로 fallback합니다. 이 호환 명령 덕분에 `resume`을 모르는 구버전 build에서도 cmux pane이 계속 동작합니다.

이렇게 하면 실제 pane은 cmux가 그리고, scrollback capture와 `send-keys` 호환은 `lterm`이 유지합니다.

**알림:**

```bash
lterm notify --title 'Task complete' --body 'All checks passed'
```

`lterm notify`는 먼저 `cmux notify`를 시도합니다. 사용할 수 없으면 OSC 777을 출력해 cmux나 호환 터미널이 알림을 표시할 수 있도록 합니다. fallback OSC에 들어가는 알림 필드는 터미널 제어 문자를 제거한 뒤 출력하며, subtitle/body 구분용 newline 같은 문자는 그대로 내보내지 않고 공백으로 바꿔 OSC 777 framing을 유지합니다.

Agent workflow에서 특정 세션 조건에 묶인 알림이 필요하면 `lterm watch <target> --exit --notify` 또는 `lterm watch <target> --contains DONE --notify`를 우선 사용하세요.

## 원격 접속

원격 머신에 `lterm`이 설치되어 있다면:

```bash
lterm ssh user@host main
```

원격 호스트에서는 `lterm open main`과 같은 attach-or-create 동작을 사용합니다. 구버전 원격 `lterm`이 `open`을 모르더라도 새 로컬 클라이언트가 계속 접속할 수 있도록 실제 wire command는 `lterm attach-or-new main`으로 유지합니다. SSH 옵션은 `--` 뒤에 전달할 수 있습니다.

```bash
lterm ssh devbox main -- -p 2222 -i ~/.ssh/id_ed25519
```

## 구조

- **Daemon** — 사용자별 Unix socket 하나를 `$XDG_RUNTIME_DIR` 아래에 만들고, 없으면 `/tmp` 아래 소유자 전용 fallback 경로를 사용합니다.
- **PTY 세션** — `portable-pty`로 실행하며 ring-buffer scrollback을 유지합니다.
- **Attach protocol** — CLI가 Unix socket으로 JSON을 보낸 뒤, 선택적으로 로컬 상태 바를 위해 아래쪽 한 줄을 예약하고 PTY byte stream을 전달합니다.
- **tmux shim** — `tmux`라는 작은 shell script가 명령을 `lterm tmux-compat`으로 넘깁니다.
- **cmux bridge** — cmux가 감지되면 cmux CLI를 사용합니다 (선택).

`lterm` binary를 업그레이드한 뒤 새 wire-protocol 동작에 의존하려면 이미
떠 있는 daemon을 재시작하세요. 실행 중인 daemon process는 종료 전까지
기존 코드를 계속 사용합니다.

## 보안 메모

**터미널 출력은 그대로 전달됩니다.** `lterm resume`(호환 이름: `lterm attach`)은 full-screen 터미널 프로그램과 cmux/OSC 알림이 정상 동작하도록 PTY byte를 그대로 통과시킵니다. 로컬 상태 바는 클라이언트 쪽 표시 요소일 뿐이며, 완전한 raw 모드 터미널이 필요하면 `--no-status`를 사용하세요. nested agent row suspension은 host-side geometry/status 관리일 뿐 attach된 PTY byte를 정제하거나 다시 쓰지 않습니다. 신뢰할 수 없는 child 프로그램은 tmux/screen에서와 마찬가지로 attach된 터미널에 escape sequence를 출력할 수 있습니다. **`lterm`을 escape-sequence sanitizer나 sandbox로 사용하지 마세요.**

**Capture 출력은 표시/로깅 전에 terminal control sequence를 제거합니다.** `lterm logs`(호환 이름: `lterm capture`), `lterm compose`(alias: `lterm mobile`), `tmux capture-pane`은 captured scrollback을 출력할 때 raw 또는 UTF-8 encoded C1 control을 포함한 터미널 제어 시퀀스를 제거합니다. 정상 UTF-8 text인 한국어, CJK, emoji는 보존합니다. 그래도 scrollback text는 신뢰할 수 없는 프로그램 출력일 수 있으므로 사람이나 agent에게 넘기기 전에 확인하세요. `compose`는 attach가 아닌 view에서 기존 input/send 경로로 텍스트를 commit하며, raw attached PTY stream을 변환하지 않습니다.

**프로세스 가시성.** `lterm processes [session]`(호환 이름: `lterm ps [session]`)은 process-group id와 함께 각 세션 child 아래의 process tree를 보여 줍니다. `--orphans`를 추가하면 기록된 session root의 descendant가 아니지만 같은 process group에 남아 있는 row도 함께 보여 주므로, Codex/OMX/MCP subprocess가 누적되어 메모리 누수처럼 커지기 전에 확인할 수 있습니다. 시스템 `ps`는 절대 경로로 호출하며, 형식이 잘못된 process row는 추측하지 않고 건너뜁니다.

**Socket 위치.** 커스텀 `LTERM_SOCKET` 경로는 소유자 전용 디렉터리 안에 있어야 합니다. 격리된 socket 위치가 필요할 때는 `LTERM_RUNTIME_DIR`를 우선 사용하세요.

**Binary override.** `LTERM_BIN`은 cmux split attach command처럼 child
`lterm`을 다시 호출할 때 쓰는 신뢰된 개발자용 override입니다. Absolute
path이고, control character가 없고, 실행 가능한 regular file일 때만
override로 인정합니다. 유효하지 않은 값은 무시하고 현재 executable 또는
`PATH`의 `lterm`으로 fallback합니다. 신뢰할 수 없는 environment data에서
`LTERM_BIN`을 설정하지 마세요.

**Popup 명령.** `tmux-compat display-popup`은 tmux와 비슷한 동작을 위해 요청된 명령을 사용자 shell로 실행합니다. **신뢰할 수 없는 popup 명령을 전달하지 마세요.**

**빌드 재현성.** 릴리스 빌드는 커밋된 lockfile을 사용하세요: `cargo build --release --locked`. 현재 lockfile은 `serde_json 1.0.149`와 registry dependency인 `zmij 1.0.21`을 고정합니다. 둘 다 Cargo가 lockfile에 따라 resolve하는 crate이며, 로컬에 vendor된 crate가 아닙니다. `cargo tree --locked -p serde_json`으로 의존성 집합을 확인할 수 있습니다.

## 현재 제한 사항

- 세션 지속성은 데몬과 호스트가 동작 중일 때만 유지됩니다. 재부팅 후 프로세스 상태 복원은 아직 구현하지 않았습니다.
- cmux 밖에서 `split-window`는 추가 managed PTY 세션을 만들지만, 터미널 안에 tiled UI를 직접 그리지는 않습니다.
- 이 프로젝트는 완전한 tmux server가 아니라 호환 subset만 제공합니다. 고급 tmux format/option을 사용하는 스크립트는 shim 명령 추가가 필요할 수 있습니다.
- cmux pane capture는 cmux scrollback API가 아니라 `lterm` 세션을 통해 처리합니다.
- 데몬은 로컬 클라이언트를 OS peer credential과 소유자 전용 socket 경로로 인증합니다. 세션별 ACL은 아직 없습니다.
- 세션 종료는 verified process-group signaling을 사용하므로 `shell → OMX → Codex → MCP` 같은 child tree는 가능한 한 함께 정리됩니다. 의도적으로 다른 session/process group으로 detach한 프로세스는 `lterm close` / `lterm kill` 이후에도 살아 있을 수 있으니, `lterm processes` / `lterm ps`나 OS process 도구로 확인하세요.

## 개발

```bash
cargo fmt
cargo test
cargo build --locked
```

수동 테스트에는 격리된 runtime directory 사용을 권장합니다.

```bash
TMP=$(mktemp -d)
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- start --name test -- sh -lc 'echo hi; sleep 10'
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- logs test -S=-20
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- shutdown
```

릴리스/계약 preflight 헬퍼:

```bash
scripts/release-preflight.sh --contract-only
scripts/release-preflight.sh --allow-occupied-skip --skip-audit
scripts/dependency-minor-dry-run.sh
```

`scripts/release-preflight.sh`의 `--run-soak`은 manual release-gate soak
profile에서만 사용하세요. Tagging 또는 publishing 전에 release, audit,
contract, dependency, soak evidence를 남길 때는
[`docs/release-evidence-template.md`](docs/release-evidence-template.md)를
사용하세요.

## 라이선스

다음 두 라이선스 중 하나를 선택해 사용할 수 있습니다.

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

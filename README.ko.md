# Light Terminal (`lterm`)

한국어 | [English](README.md)

## TL;DR

- **무엇** — Rust로 만든 PTY 세션 데몬 + tmux 호환 shim. 이름이나 pane id로 detach·reattach할 수 있는 영속 세션을 제공합니다.
- **대상** — tmux를 전제하는 AI 에이전트 도구(`oh-my-codex`, `oh-my-claude`)와, 그것을 `cmux` 안에서 돌리는 사용자.
- **사용법** — `lterm new`로 만들고 `lterm attach`로 (재)접속, shim이 적용된 실행에는 `lterm omx` / `lterm omc`. 세션 안에서는 `tmux` 명령이 `lterm tmux-compat`으로 해석됩니다.
- **상태** — alpha MVP. 같은 OS 사용자 안에서 쓰는 편의용 데몬이며, **샌드박스도 escape-sequence sanitizer도 완전한 tmux 대체품도 아닙니다.**

---

AI 에이전트 워크플로를 위해 만든 가벼운 터미널 세션 데몬입니다. tmux 호환 shim을 함께 제공합니다.

`lterm`은 tmux 전체를 대체하려는 도구가 아닙니다. 오래 실행되는 PTY 세션을 유지하고, 클라이언트가 자유롭게 detach/reattach할 수 있게 하며, 터미널 escape sequence는 그대로 통과시키고, oh-my-codex / oh-my-claude 계열 도구가 자주 사용하는 tmux 명령 일부를 호환 shim으로 제공합니다.

> **상태:** alpha MVP. 로컬 detached 세션과 호환성 테스트에는 사용할 수 있지만, 아직 완전한 tmux 대체품은 아닙니다.
>
> **보안 모델:** `lterm`은 같은 OS 사용자 안에서 쓰는 편의용 데몬이며 샌드박스가 아닙니다. 다른 사용자의 Unix socket 접근은 거부하고 런타임 디렉터리는 소유자 전용 권한으로 만들지만, 같은 OS 사용자 권한으로 실행되는 프로세스는 세션을 제어할 수 있다고 보아야 합니다.

## 왜 만들었나

다음 세 가지 요구를 충족하는 것이 목표입니다.

1. **tmux와 비슷한 세션 지속성과 원격 접속** — 세션은 백그라운드 데몬에서 실행되며, 이름이나 pane id로 attach/detach할 수 있습니다. 원격 호스트에 `lterm`이 설치되어 있다면 `lterm ssh`로 접속할 수 있습니다.
2. **cmux 호환성** — cmux 안에서 실행할 때는 OSC 알림을 그대로 통과시키고 `lterm notify`를 제공하며, tmux shim은 가능한 경우 worker pane을 cmux native split으로 엽니다.
3. **AI 도구 지원** — `lterm omx`, `lterm omc`, `lterm install-shim`은 tmux를 전제하는 도구를 위해 가짜 `tmux` 명령과 `TMUX` / `TMUX_PANE` 환경 변수를 제공합니다.

cmux 호환 동작은 cmux가 문서화한 기능을 따릅니다. cmux는 `cmux notify`와 OSC 777 / OSC 99 알림, workspace/split을 위한 Unix socket·CLI API, 그리고 tmux 명령을 cmux pane으로 매핑하는 oh-my-codex 통합을 제공합니다.

## 설치

Rust 1.85 이상이 필요합니다.

```bash
cargo build --release
./target/release/lterm --help
```

개발 중에는 다음처럼 실행할 수 있습니다.

```bash
cargo run -- --help
```

터미널에서 tmux shim을 사용하려면:

```bash
lterm install-shim
# 출력된 디렉터리를 실제 tmux보다 앞쪽 PATH에 추가하거나 다음을 실행하세요.
eval "$(lterm env)"
```

## 빠른 시작

**세션을 만들고 바로 attach:**

```bash
lterm new -n api -- npm run dev
```

**먼저 detached로 만든 뒤 나중에 attach:**

```bash
lterm new -d -n api -- npm run dev
lterm attach api

# 짧은 alias도 지원합니다. `-a`는 `lterm` 바로 뒤에 쓰고, target과는 공백으로 구분하세요.
lterm a api
lterm -a api
```

`lterm ls`는 기본적으로 하위 pane을 숨기고, 기존 첫 5개 tab-separated 열(`name`, `pane`, `alive`, `cwd`, `command`)을 유지한 뒤 attach 상태(`attached` / `detached`)와 parent pane(`-` 또는 pane id)을 뒤에 붙입니다. attach된 클라이언트는 아래쪽 한 줄에 파란 상태 바를 표시하고, PTY는 그 줄을 제외한 영역으로 resize됩니다. 예전처럼 전체 터미널을 raw 모드로 사용하려면 `lterm attach --no-status api`를 쓰거나, 상태줄 처리가 충돌하는 클라이언트에서는 `LTERM_NO_STATUS=1` / `LTERM_STATUS=0`을 설정하세요.

`LTERM_STATUS_STYLE=full` 또는 `LTERM_STATUS_STYLE=minimal` 로 시각 스타일을 선택할 수 있습니다. `full`(로컬 터미널 기본값)은 검정 글자 + bright-blue 배경, `minimal`은 SGR 색을 모두 생략한 plain text로 동작합니다. SSH 세션(`SSH_CONNECTION` / `SSH_CLIENT` / `SSH_TTY` 감지)에서는 자동으로 `minimal`이 적용되어 Termius 같은 모바일 SSH 클라이언트의 색 충돌을 줄입니다.

attach된 PTY가 alternate screen buffer로 진입하면(예: `vim`, `less`, `htop`이 `\x1b[?1049h` 사용) lterm은 status bar를 일시 중단해 alt-screen 앱의 UI와 충돌을 피합니다. 앱이 alt-screen을 종료하는 즉시 status bar가 다시 그려집니다.

`lterm attach` 도중 panic 이나 abort 가 발생해도 process-wide hook 이 최소 복구 sequence (scroll region 리셋, 커서 보이기, alt-screen 종료, SGR 리셋) 를 emit 해 사용자 터미널이 raw mode 나 hidden cursor 상태로 남지 않습니다.

CJK 문자나 이모지(ZWJ family, 국기, 결합 문자 포함)가 들어간 세션 이름은 `unicode-width` / `unicode-segmentation` 으로 디스플레이 폭을 계산해 정렬되므로 wide character가 섞여도 status bar 패딩이 어긋나지 않습니다.

child 애플리케이션이 `CSI u` enhancement sequence로 Kitty keyboard protocol을 켜면, lterm은 이를 추적했다가 attach 종료 시 terminal keyboard mode를 best-effort로 복원합니다. 그래서 child가 비정상 종료된 뒤 shell 입력이 `1;1:3u` 같은 escape 조각으로 보이는 상황을 줄입니다.

**세션 확인 및 입력 전송:**

```bash
lterm ls
lterm ls --children
lterm ls --all
lterm ps api
lterm capture api -S=-80
lterm send api 'echo hello' --enter
```

**세션 종료:**

```bash
lterm kill api
```

## AI 워크플로

**Oh My Codex를 shim이 적용된 세션에서 실행:**

```bash
lterm omx team
# omx에 넘길 추가 flag는 그대로 전달됩니다.
lterm omx --madmax --xhigh
```

**Oh My Claude도 같은 방식으로 실행:**

```bash
lterm omc team
# 본 README 작성 시점에 테스트한 OMC 빌드는 --xhigh를 거부합니다.
# 설치된 `omc --help`에 해당 flag가 보이지 않는다면 --xhigh 없이 --madmax만 사용하세요.
lterm omc --madmax
```

**임의의 명령을 tmux 호환 모드로 실행:**

```bash
lterm run --tmux -- omx hud --tmux
```

이 세션 안에서는 `tmux`가 `lterm tmux-compat` shim으로 해석됩니다. 현재 shim은 AI orchestration 스크립트가 자주 사용하는 다음 명령 subset을 구현합니다.

- **세션** — `new-session`, `attach-session`, `has-session`, `list-sessions`, `kill-session`
- **Pane** — `split-window`, `list-panes`, `display-message`, `capture-pane`, `send-keys`, `kill-pane`, `resize-pane`
- **Buffer / popup** — `display-popup`, `wait-for`, `load-buffer`, `save-buffer`, `paste-buffer`
- **호환용 no-op** — `select-pane`, `select-layout`, `set-option`, `show-option`

## cmux 동작

`lterm tmux-compat split-window`가 cmux 환경(`CMUX_WORKSPACE_ID`, `CMUX_SURFACE_ID`, 또는 cmux socket)을 감지하면 다음 순서로 동작합니다.

1. worker 명령을 위한 새 `lterm` PTY 세션을 시작합니다.
2. cmux에 native split 생성을 요청합니다 (`cmux new-split right/down`).
3. 생성된 split에 `lterm attach <pane>`을 보냅니다.

이렇게 하면 실제 pane은 cmux가 그리고, scrollback capture와 `send-keys` 호환은 `lterm`이 유지합니다.

**알림:**

```bash
lterm notify --title 'Task complete' --body 'All checks passed'
```

`lterm notify`는 먼저 `cmux notify`를 시도합니다. 사용할 수 없으면 OSC 777을 출력해 cmux나 호환 터미널이 알림을 표시할 수 있도록 합니다. fallback OSC에 들어가는 알림 필드는 터미널 제어 문자를 제거한 뒤 출력합니다.

## 원격 접속

원격 머신에 `lterm`이 설치되어 있다면:

```bash
lterm ssh user@host main
```

내부적으로 `ssh -t user@host 'lterm attach-or-new main'`을 실행합니다. SSH 옵션은 `--` 뒤에 전달할 수 있습니다.

```bash
lterm ssh devbox main -- -p 2222 -i ~/.ssh/id_ed25519
```

## 구조

- **Daemon** — 사용자별 Unix socket 하나를 `$XDG_RUNTIME_DIR` 아래에 만들고, 없으면 `/tmp` 아래 소유자 전용 fallback 경로를 사용합니다.
- **PTY 세션** — `portable-pty`로 실행하며 ring-buffer scrollback을 유지합니다.
- **Attach protocol** — CLI가 Unix socket으로 JSON을 보낸 뒤, 선택적으로 로컬 상태 바를 위해 아래쪽 한 줄을 예약하고 PTY byte stream을 전달합니다.
- **tmux shim** — `tmux`라는 작은 shell script가 명령을 `lterm tmux-compat`으로 넘깁니다.
- **cmux bridge** — cmux가 감지되면 cmux CLI를 사용합니다 (선택).

## 보안 메모

**터미널 출력은 그대로 전달됩니다.** `lterm attach`는 full-screen 터미널 프로그램과 cmux/OSC 알림이 정상 동작하도록 PTY byte를 그대로 통과시킵니다. 로컬 상태 바는 클라이언트 쪽 표시 요소일 뿐이며, 완전한 raw 모드 터미널이 필요하면 `--no-status`를 사용하세요. 신뢰할 수 없는 child 프로그램은 tmux/screen에서와 마찬가지로 attach된 터미널에 escape sequence를 출력할 수 있습니다. **`lterm`을 escape-sequence sanitizer나 sandbox로 사용하지 마세요.**

**Capture 출력은 사람/AI가 읽기 쉽도록 정제됩니다.** `lterm capture`와 `tmux capture-pane`은 captured scrollback을 출력할 때 일반적인 터미널 제어 시퀀스를 제거합니다.

**프로세스 가시성.** `lterm ps [session]`은 각 세션 child 아래의 process tree를 보여 줍니다. Codex/OMX/MCP subprocess가 누적되어 메모리 누수처럼 커지기 전에 확인하는 용도입니다. 시스템 `ps`는 절대 경로로 호출하며, 형식이 잘못된 process row는 추측하지 않고 건너뜁니다.

**Socket 위치.** 커스텀 `LTERM_SOCKET` 경로는 소유자 전용 디렉터리 안에 있어야 합니다. 격리된 socket 위치가 필요할 때는 `LTERM_RUNTIME_DIR`를 우선 사용하세요.

**Popup 명령.** `tmux-compat display-popup`은 tmux와 비슷한 동작을 위해 요청된 명령을 사용자 shell로 실행합니다. **신뢰할 수 없는 popup 명령을 전달하지 마세요.**

**빌드 재현성.** 릴리스 빌드는 커밋된 lockfile을 사용하세요: `cargo build --release --locked`. 현재 lockfile은 `serde_json 1.0.149`를 고정합니다. 이 버전의 transitive dependency인 `zmij`는 docs.rs/crates.io의 공식 serde_json package metadata에 등록된 의존성이며, 로컬에 vendor된 crate가 아닙니다.

## 현재 제한 사항

- 세션 지속성은 데몬과 호스트가 동작 중일 때만 유지됩니다. 재부팅 후 프로세스 상태 복원은 아직 구현하지 않았습니다.
- cmux 밖에서 `split-window`는 추가 managed PTY 세션을 만들지만, 터미널 안에 tiled UI를 직접 그리지는 않습니다.
- 이 프로젝트는 완전한 tmux server가 아니라 호환 subset만 제공합니다. 고급 tmux format/option을 사용하는 스크립트는 shim 명령 추가가 필요할 수 있습니다.
- cmux pane capture는 cmux scrollback API가 아니라 `lterm` 세션을 통해 처리합니다.
- 데몬은 로컬 클라이언트를 OS peer credential과 소유자 전용 socket 경로로 인증합니다. 세션별 ACL은 아직 없습니다.
- 세션 종료는 verified process-group signaling을 사용하므로 `shell → OMX → Codex → MCP` 같은 child tree는 가능한 한 함께 정리됩니다. 의도적으로 다른 session/process group으로 detach한 프로세스는 `lterm kill` 이후에도 살아 있을 수 있으니, `lterm ps`나 OS process 도구로 확인하세요.

## 개발

```bash
cargo fmt
cargo test
cargo build --locked
```

수동 테스트에는 격리된 runtime directory 사용을 권장합니다.

```bash
TMP=$(mktemp -d)
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- new --name test -- sh -lc 'echo hi; sleep 10'
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- capture test -S=-20
LTERM_RUNTIME_DIR="$TMP/run" LTERM_DATA_DIR="$TMP/data" cargo run -- shutdown
```

## 라이선스

다음 두 라이선스 중 하나를 선택해 사용할 수 있습니다.

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

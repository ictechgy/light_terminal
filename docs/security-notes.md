# lterm Security Triage Notes

이 문서는 보안 관련 finding 들의 **분류 결과(triage log)** 를 누적한다. **정책/
threat model 문서가 아니다** — lterm 의 비-목표 (non-goals) 는 `docs/non-goals.md`,
공식 contract 는 `docs/public-contract.md` 가 단일 출처로 보유한다.

각 entry 는 다음을 포함한다.

- **Date**: triage 날짜
- **Tool / Source**: 발견 출처 (semgrep MCP, manual review, dependency advisory 등)
- **Finding**: 도구가 출력한 원문 요약
- **Location**: 코드 위치 (가능하면 함수/심볼 이름 우선, line 번호는 *as of <commit-sha>* 로 명시)
- **Assessment**: 실재 위험 여부 + 근거
- **Threat model assumptions**: 이 평가가 의존하는 trust 전제
- **Action**: 적용한 조치 또는 후속 작업

---

## 2026-05-18 — semgrep "Path Traversal with Actix" at `src/server.rs` Request::New handler

### Finding (semgrep raw)
> The application builds a file path from potentially untrusted data, which can
> lead to a path traversal vulnerability. An attacker can manipulate the path
> which the application uses to access files. … (CWE-22)
>
> rule display name: "Path Traversal with Actix", severity: ERROR

### Location

`src/server.rs` 의 `Request::New` 처리 경로 (as of commit `2ade5f7` / PR #77 머지 직후 main):

- `params.cwd` 를 받아 `let cwd = params.cwd.or_else(...).unwrap_or_else(|| ".".to_string())` 으로 정규화 (line 번호는 향후 변동 가능)
- `cmd.cwd(PathBuf::from(&cwd))` 로 spawn 되는 child 의 working directory 결정
- tmux 모드일 때 `paths::shim_dir()` 결과를 `shlex::try_quote` 로 인용해 셸의 `PATH` 에 prepend
- `LTERM_SOCKET`, `LTERM_BIN` env 가 `paths::socket_path()`, `std::env::current_exe()` 결과로 채워짐

### Assessment

**False positive** (lterm 의 명시된 trust model 하에서). 근거는 다음 순서로 정리한다.
순서가 강한 것에서 약한 것으로 내려간다.

#### 결정적 근거 — same-UID trust boundary

1. **모든 RPC 연결은 `verify_peer_owner` 통과 후에만 처리된다** (`src/server.rs`,
   함수 이름 기준; macOS 는 `getpeereid(3)`, Linux 는 `SO_PEERCRED` socket option,
   기타 unix 는 명시적 cfg fallback). 즉 RPC 의 모든 입력은 daemon 을 띄운 UID
   본인의 요청이라는 trust boundary 안에서 처리된다.

2. **`params.cwd` 는 동일 UID 사용자의 입력이며 `cmd.cwd(...)` 는 동일 UID 권한
   내에서의 `chdir` 이다.** path traversal 이 진짜 vulnerability 가 되려면 더
   강한 권한이 더 낮은 권한 리소스에 접근해야 하는데, 본 코드는 그 반대 — 같은
   UID 본인의 권한으로 자기 자신이 원하는 cwd 에 child 를 띄울 뿐이다. `..` 포함
   cwd 든 절대 경로 cwd 든 정상적인 의도된 동작이다 (`lterm new -d ../foo -- sh`).

3. **child 의 cwd 변경에는 부작용이 존재함을 인정한다 — 다만 의도된 동작.**
   spawn 된 셸이 cwd-relative 파일 (`./.envrc`, `./.bashrc` 류) 을 자동 source
   하거나 direnv / autoenv 류 도구가 자동 실행할 수 있다. 본 동작은 사용자 본인
   이 직접 셸에서 `cd <path> && bash` 한 것과 동등하다 (lterm 이 추가 권한 상승
   브로커가 아니므로). 따라서 path traversal *vulnerability* 가 아니라
   **intentional behavior under same-user trust** 이다. 같은 UID 로 도는 다른
   untrusted 프로세스가 lterm 소켓에 접근할 수 있는 상황을 위협 모델에 포함시키
   면 별도의 socket 접근 제어 / 토큰 방식이 필요하지만, 그것은 본 finding 의 책임
   영역이 아니라 별도 threat model 결정이다 (현재 lterm 은 same-user 단일 신뢰
   모델을 채택).

4. **special path (`/proc/self/*` 등) 도 same-UID 한계 안.** 같은 UID 가 이미
   direct 로 접근 가능한 리소스의 부분집합이며, kernel-enforced 한계를 넘지 않는다.

#### 부수적 근거 — 룰 매칭 부정합

5. **lterm 은 Actix 또는 어떤 HTTP 서버도 사용하지 않는다.** Rust CLI/daemon 으로,
   Unix domain socket 기반 RPC 만 사용한다. `Cargo.toml` 에 직접 `actix*` 의존성
   없음 (transitive 도 확인되지 않음). 다만 semgrep 룰은 패턴 매칭만 하므로
   actix 의존성 유무는 **부수적 증거**일 뿐이며, false-positive 판정의 결정적
   근거는 #1-#3 (same-UID trust boundary) 이다. 향후 lterm 에 실제 HTTP/web
   서버 컴포넌트가 추가되면 이 트리아지는 **즉시 재평가**되어야 한다.

6. **`paths::socket_path()`, `std::env::current_exe()` 는 RPC attacker-controlled
   입력이 아니다.** `LTERM_SOCKET`, `LTERM_RUNTIME_DIR`, `LTERM_DATA_DIR` 같은
   환경변수가 daemon 시작 시 path 결정에 영향을 주지만, 이들은 daemon 자신의
   환경에서 읽히는 값이며 RPC client 가 변경할 수 없다. 추가로 `paths.rs` 는
   absolute-path 강제 (`require_absolute_env_path`), private dir 보장
   (`ensure_user_private_dir`), 부모 디렉터리 검증 (`validate_socket_parent`) 으
   로 환경변수 경로조차 약한 안전망을 갖는다.

### Threat model assumptions (이 평가가 의존하는 전제)

본 false-positive 판정은 다음 전제 위에서만 유효하다. 전제가 깨지면 재평가 필요.

- **(a) lterm 은 setuid/setgid bit 없이 사용자 권한으로 실행된다.** 권장 배포
  경로 (cargo install, homebrew formula, npm wrapper) 는 모두 일반 사용자 권한
  으로 daemon 을 띄운다.
- **(b) daemon 을 띄운 UID 가 신뢰 경계의 원점이다.** **root 로 daemon 을 띄우는
  것은 권장되지 않는다** — `lterm doctor` 의 `daemon_uid` 필드가 root 를 보고
  하면 위 #2-#3 의 "동일 UID 권한 = 안전" 논리는 무너지며, root 의 chdir 은 모든
  디렉터리에 접근 가능해진다.
- **(c) 같은 UID 안의 모든 프로세스는 상호 신뢰한다 (lterm 의 same-user 단일
  신뢰 모델).** lterm 은 sandbox / privilege isolation / 세션 토큰 같은 추가
  방어선을 **명시적으로 비-목표** 로 선언한다 — `docs/non-goals.md` "Sandbox /
  privilege isolation" 섹션 참조. 동일 UID 로 도는 다른 untrusted 프로세스가
  lterm 소켓에 접근할 수 있는 위협 모델은 본 프로젝트의 책임 영역이 아니다.
- **(d) 컨테이너 / sandbox 안에서는 UID namespace 안에서의 동등성만 보장된다.**
  컨테이너 안에서 UID 1000 처럼 보이는 daemon 이 호스트에서는 다른 UID 일 수
  있다는 사실은 컨테이너 운영자의 책임이며 lterm 의 trust model 은 namespace
  안의 UID 만 본다.

위 (a)-(d) 중 하나라도 깨지면 본 트리아지는 무효이며, 해당 시점에 새 entry 로
재평가를 기록해야 한다.

### Action

- **코드 변경 없음** (false positive).
- 본 triage 노트로 향후 같은 finding 이 재발했을 때 빠르게 참조할 수 있도록 기록.
- **semgrep MCP 룰 자체를 ignore 하지는 않는다** — 룰이 web context 에서는 여전히
  의미가 있고, 본 코드와 무관하다는 결정은 이 문서가 단일 출처로 보유한다.
  같은 룰이 다른 위치에 떠도 본 문서를 갱신하면 된다.

#### Trade-off

룰을 suppress 하지 않으므로 동일 finding 이 반복 보고된다. 향후 lterm 에 실제
HTTP/Actix 서버 컴포넌트 또는 임의 third-party path 를 같은-UID 사용자 입력으로
다루는 더 위험한 경로가 추가되면 이 트리아지를 즉시 재평가해야 한다.

### Cross-references

- 보안 모델: `src/server.rs::verify_peer_owner` (플랫폼별 cfg-gated 변형)
- 비-목표 / threat model 한계: `docs/non-goals.md` "Sandbox / privilege isolation"
- 격리/private dir 보장: `src/paths.rs::ensure_private_dir`,
  `validate_socket_parent`, `require_absolute_env_path`
- 회귀 가드: `tests/lifecycle.rs::stale_non_socket_at_socket_path_is_refused`,
  `tests/lifecycle.rs::symlink_socket_path_is_refused`,
  `tests/lifecycle.rs::symlink_socket_path_pointing_to_live_unix_socket_is_refused`

본 cross-reference 는 코드 변경 시 함께 갱신되어야 한다. CI 에 자동 검증 장치는
없으므로 trust-model 관련 변경이 들어오는 PR 에서는 본 문서도 review 범위에
포함시키는 것을 권장.

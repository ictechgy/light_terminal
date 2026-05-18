# lterm Security Triage Notes

작성 시점부터 이 문서는 보안 관련 finding들의 분류 결과를 누적한다. 새 발견이
나올 때마다 한 entry씩 추가한다. 각 entry는 다음을 포함한다.

- **Date**: triage 날짜
- **Tool / Source**: 발견 출처 (semgrep MCP, manual review, dependency advisory 등)
- **Finding**: 도구가 출력한 원문 요약
- **Location**: 코드 위치
- **Assessment**: 실재 위험 여부 + 근거
- **Action**: 적용한 조치 또는 후속 작업

---

## 2026-05-18 — semgrep "Path Traversal with Actix" at `src/server.rs:1631` (and nearby)

### Finding (semgrep raw)
> The application builds a file path from potentially untrusted data, which can
> lead to a path traversal vulnerability. An attacker can manipulate the path
> which the application uses to access files. … (CWE-22)
>
> rule display name: "Path Traversal with Actix", severity: ERROR

### Location

`src/server.rs` 의 `Request::New` 처리 경로:

- Line ~1615-1622: `let cwd = params.cwd.or_else(|| std::env::current_dir().ok().map(|p| p.display().to_string())).unwrap_or_else(|| ".".to_string());`
- Line ~1638: `cmd.cwd(PathBuf::from(&cwd));`
- Line ~1626-1629: tmux shim path 가 `paths::shim_dir()` 결과를 shlex-quote 해서 셸로 export 됨
- Line ~1645-1646: `LTERM_SOCKET`, `LTERM_BIN` env 가 `paths::socket_path()`, `std::env::current_exe()` 결과로 채워짐

### Assessment

**False positive**. 본 finding은 web/HTTP 컨텍스트에서 외부 사용자 입력이 path 로
직접 흘러들어가는 패턴을 잡도록 만든 룰 (rule name 자체가 "with Actix") 이며,
lterm 의 실제 trust model 과 맞지 않다.

근거:

1. **lterm 은 Actix 또는 어떤 HTTP 서버도 사용하지 않는다.** Rust CLI/daemon 으로,
   Unix domain socket 기반 RPC 만 사용한다. `Cargo.toml` 에 `actix*` 의존성 없음.

2. **모든 연결은 `verify_peer_owner` 로 peer UID 가 daemon UID 와 같음을 확인한
   뒤에만 처리된다** (`src/server.rs:1190`). macOS 는 `getpeereid(3)`, Linux 는
   `SO_PEERCRED` socket option 으로 OS 가 보고하는 커널 검증된 자격 증명을 사용
   하므로, RPC 의 모든 입력 (params.cwd 포함) 은 daemon 을 띄운 사용자 본인의
   요청이라는 trust boundary 안에서 처리된다.

3. **`cmd.cwd(...)` 의 결과는 동일 UID 권한 안에서의 `chdir`이다.** path traversal
   이 권한 상승으로 이어지려면 더 강한 권한으로 더 낮은 권한 사용자의 자원에
   접근해야 하는데, 본 코드는 정확히 그 반대 — 동일 사용자 본인의 권한으로 자기
   자신이 원하는 cwd 에 child 를 띄울 뿐이다. 사용자가 `..` 을 포함한 cwd 를
   보내거나 절대 경로를 보내는 것도 정상적인 의도된 동작이다 (`lterm new -d
   ../foo -- sh` 류).

4. **`paths::socket_path()`, `std::env::current_exe()` 등의 출력은 사용자 입력이
   아니다.** 각각 OS / `paths.rs` 가 정책에 따라 결정한 경로를 String 화한 것
   이며, 외부 공격자가 좌우할 수 없다.

5. **socket parent dir 의 trust boundary 는 `paths.rs` 가 별도로 강제한다.** runtime
   dir 은 0700 + symlink 거부 + uid 일치 검사를 거치며 (`paths.rs`의
   `ensure_user_private_dir`, `validate_existing_private_dir`, `validate_socket_parent`),
   소켓 path 가 `LTERM_SOCKET` 으로 override 될 때도 부모 디렉터리가 검증된다.

### Action

- 코드 변경 없음 (false positive).
- 본 triage 노트로 향후 같은 finding 이 재발했을 때 빠르게 참조할 수 있도록 기록.
- semgrep MCP 룰 자체를 ignore 하지는 않는다 — 룰이 web context 에서는 여전히
  의미가 있고, 본 코드와 무관하다는 결정은 이 문서가 단일 출처로 보유한다.
  같은 룰이 다른 위치에 떠도 본 문서를 갱신하면 된다.

### Cross-references

- 보안 모델: `src/server.rs::verify_peer_owner` (macOS/Linux/other 3 변형, 각각
  SAFETY 주석 포함)
- 격리/private dir 보장: `src/paths.rs::ensure_private_dir`,
  `validate_socket_parent`, `require_absolute_env_path`
- 회귀 가드: `tests/lifecycle.rs::stale_non_socket_at_socket_path_is_refused`,
  `tests/lifecycle.rs::symlink_socket_path_is_refused`,
  `tests/lifecycle.rs::symlink_socket_path_pointing_to_live_unix_socket_is_refused`

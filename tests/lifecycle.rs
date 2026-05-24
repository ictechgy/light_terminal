// tests/lifecycle.rs — 7 결정적 daemon lifecycle regression 케이스.
//
// 목적: alpha-MVP → 안정 1.x 약속을 PR마다 빠르게 검증한다. release-gate soak
// (tests/soak.rs)와 upgrade matrix(tests/upgrade_matrix.rs)는 무겁고 별도 트리거를
// 갖는 반면, 본 suite는 기본 `cargo test`에 포함되며 결정적·짧다.
//
// 각 테스트는 LTERM_RUNTIME_DIR / LTERM_DATA_DIR을 tempdir로 격리하므로 사용자의
// 실제 데몬이나 다른 테스트와 충돌하지 않는다. tempdir Drop이 socket·data 파일을
// 모두 정리한다. LifecycleEnv::Drop도 명시적으로 `lterm shutdown`을 호출해 임시
// 데몬이 누수되지 않도록 보장한다.

use serde_json::Value;
use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const LTERM_BIN: &str = env!("CARGO_BIN_EXE_lterm");

fn command_output_with_timeout(
    command: &mut Command,
    timeout: Duration,
    label: &str,
) -> TestResult<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return child
                .wait_with_output()
                .map_err(|err| format!("failed to collect output for {label}: {err}").into());
        }
        thread::sleep(Duration::from_millis(25));
    }

    let _ = child.kill();
    let output = child.wait_with_output()?;
    Err(format!(
        "timed out running {label} after {timeout:?}; stdout={:?}; stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

struct LifecycleEnv {
    temp: tempfile::TempDir,
}

impl LifecycleEnv {
    fn new() -> TestResult<Self> {
        let temp = tempfile::tempdir()?;
        fs::create_dir_all(temp.path().join("tmp"))?;
        Ok(Self { temp })
    }

    fn runtime_dir(&self) -> PathBuf {
        self.temp.path().join("run")
    }

    fn data_dir(&self) -> PathBuf {
        self.temp.path().join("data")
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(LTERM_BIN);
        cmd.env_remove("LTERM_SOCKET")
            .env_remove("LTERM_PANE")
            .env_remove("LTERM_PARENT_TOKEN")
            .env("LTERM_RUNTIME_DIR", self.runtime_dir())
            .env("LTERM_DATA_DIR", self.data_dir())
            .env("TMPDIR", self.temp.path().join("tmp"));
        cmd
    }

    fn doctor_json(&self) -> TestResult<Value> {
        let out = self.cmd().args(["doctor", "--json"]).output()?;
        assert!(
            out.status.success(),
            "doctor exited {:?}, stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(serde_json::from_slice(&out.stdout)?)
    }

    fn socket_path(&self) -> TestResult<PathBuf> {
        let report = self.doctor_json()?;
        let path = report
            .get("socket_path")
            .and_then(|v| v.as_str())
            .ok_or("doctor --json missing socket_path field")?;
        Ok(PathBuf::from(path))
    }

    // `lterm new --detach`는 client::ensure_server() 경로를 거치므로 데몬을 강제
    // spawn한다. dummy session은 즉시 종료시켜 누수 없이 데몬만 살려둔다.
    // tmux shim도 미리 설치해 doctor의 reason이 shim-missing으로 오해되지 않게 한다
    // (tempdir 격리 환경에서는 기본적으로 shim이 없으므로).
    fn ensure_daemon(&self, name: &str) -> TestResult {
        let shim_out = self.cmd().args(["install-shim"]).output()?;
        assert!(
            shim_out.status.success(),
            "install-shim failed: stderr={}",
            String::from_utf8_lossy(&shim_out.stderr)
        );
        let out = self
            .cmd()
            .args([
                "new", "--detach", "--name", name, "--", "sh", "-lc", "exit 0",
            ])
            .output()?;
        assert!(
            out.status.success(),
            "ensure_daemon({name}) failed: stderr={}, stdout={}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        // dummy session이 종료되도록 짧게 대기. session_count가 0이 되어야 후속
        // 테스트가 영향받지 않는다. deadline 만료 시 leaked state로 후속 테스트를
        // 오염시키는 대신 명시적 에러로 실패한다 (quad-review 합의 fix).
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = self.doctor_json()?;
            if r.get("daemon_session_count").and_then(|v| v.as_u64()) == Some(0)
                && r.get("daemon_reachable").and_then(|v| v.as_bool()) == Some(true)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "ensure_daemon({name}): timed out waiting for clean state (daemon_session_count=0 & daemon_reachable=true); last doctor: {r}"
                )
                .into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    // `lterm shutdown`은 자동 spawn 후 Shutdown 요청을 보낸다. 데몬은 종료해도
    // socket 파일을 명시적으로 정리하지 않으므로(`prepare_socket_path`가 다음 spawn
    // 시 stale socket을 ping 후 제거) 본 helper는 호출 후 ping이 실패할 때까지
    // polling한 뒤에만 socket을 안전하게 제거한다.
    //
    // 살아있는 socket을 force-remove하면 unlinked listener를 남긴 orphan daemon이
    // 생기고 후속 테스트가 잘못된 이유로 통과할 수 있다 (quad-review HIGH consensus
    // fix). deadline 도달 시 force-remove 대신 명시적 에러로 실패한다.
    fn shutdown_and_cleanup_socket(&self) -> TestResult {
        let out = self.cmd().arg("shutdown").output()?;
        let socket = self.runtime_dir().join("lterm.sock");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if !socket.exists() {
                return Ok(());
            }
            match UnixStream::connect(&socket) {
                Ok(stream) => {
                    // 데몬이 여전히 응답 — Shutdown 처리 중이거나 거부됐을 수 있다.
                    drop(stream);
                }
                Err(_) => {
                    // ping 실패 = 데몬이 더 이상 listen 하지 않음 = stale socket.
                    // 이 시점에서만 socket 파일을 안전하게 제거한다.
                    let _ = fs::remove_file(&socket);
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "shutdown_and_cleanup_socket: daemon still answering at {} after 3s deadline; \
                     refusing to force-remove a live socket. shutdown stderr: {}",
                    socket.display(),
                    String::from_utf8_lossy(&out.stderr)
                )
                .into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for LifecycleEnv {
    fn drop(&mut self) {
        let _ = self.cmd().arg("shutdown").output();
        let socket = self.runtime_dir().join("lterm.sock");
        let _ = fs::remove_file(&socket);
    }
}

// US-004 case 1: socket 경로에 사전 존재하는 비-소켓 파일은 silently 덮어쓰지 않고
// 명시적 에러로 거부되어야 한다. prepare_socket_path의 "refusing to remove
// non-socket path" 방어선 회귀 가드.
#[test]
fn stale_non_socket_at_socket_path_is_refused() -> TestResult {
    let env = LifecycleEnv::new()?;
    // doctor 한 번 부르면 runtime_dir(0700)이 만들어지고 socket_path를 얻는다.
    let socket = env.socket_path()?;
    env.shutdown_and_cleanup_socket()?;
    assert!(
        !socket.exists(),
        "socket file should be cleaned up before placing the bait non-socket file: {}",
        socket.display()
    );

    // 비-소켓 일반 파일을 사전에 둔다.
    fs::write(&socket, b"not a socket")?;

    // 다음 명령은 명시적으로 실패해야 한다. 어느 트레이스 경로든 (a) 데몬이
    // prepare_socket_path에서 "refusing to remove non-socket path"로 거부하거나
    // (macOS: ENOTSOCK → "non-socket"/"Socket operation"), (b) 클라이언트가
    // UnixStream::connect에서 즉시 거부 (Linux: ECONNREFUSED → "Connection
    // refused"). 두 경우 모두 trust boundary가 지켜진다.
    let out = env.cmd().args(["list"]).output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "lterm list must fail when socket path holds a non-socket file. stdout={}, stderr={}",
        String::from_utf8_lossy(&out.stdout),
        stderr
    );
    assert!(
        stderr.contains("non-socket")
            || stderr.contains("Socket operation")
            || stderr.contains("Connection refused"),
        "expected non-socket/Connection-refused refusal in stderr, got: {}",
        stderr
    );

    // 사전에 둔 비-소켓 파일은 보호되어야 한다 — silently 삭제/덮어쓰기 금지.
    // 이것이 trust boundary의 진짜 safety invariant이며 OS-portable.
    assert_eq!(
        fs::read(&socket)?,
        b"not a socket",
        "non-socket file must not be overwritten or removed"
    );
    fs::remove_file(&socket)?;
    Ok(())
}

// US-004 case 2: 명시적 shutdown 후에도 다음 lterm 호출이 자동으로 데몬을 다시
// 띄우고, 정상 상태에서는 doctor --json이 (1) 새 필드(daemon_uid,
// daemon_uptime_secs)를 포함하고 (2) reason 필드는 omit한다.
#[test]
fn auto_restart_after_explicit_shutdown_keeps_doctor_clean() -> TestResult {
    let env = LifecycleEnv::new()?;
    env.ensure_daemon("lifecycle-restart-init")?;
    let r1 = env.doctor_json()?;
    assert_eq!(
        r1.get("daemon_reachable").and_then(|v| v.as_bool()),
        Some(true)
    );

    env.shutdown_and_cleanup_socket()?;

    env.ensure_daemon("lifecycle-restart-after")?;
    let r2 = env.doctor_json()?;
    assert_eq!(
        r2.get("daemon_reachable").and_then(|v| v.as_bool()),
        Some(true),
        "daemon should auto-restart on next command: {r2:?}"
    );
    assert_eq!(
        r2.get("version_match").and_then(|v| v.as_bool()),
        Some(true),
        "client/daemon must agree on version after restart: {r2:?}"
    );

    // US-003에서 추가한 필드들은 이 빌드의 데몬이라면 반드시 보고된다.
    assert!(
        r2.get("daemon_uid").is_some(),
        "doctor --json should include daemon_uid after US-003: {r2:?}"
    );
    assert!(
        r2.get("daemon_uptime_secs").is_some(),
        "doctor --json should include daemon_uptime_secs after US-003: {r2:?}"
    );

    // 정상 상태에서는 reason이 보이지 않아야 한다 (skip_serializing_if).
    assert!(
        r2.get("reason").is_none(),
        "healthy daemon must omit reason field: {r2:?}"
    );
    Ok(())
}

// US-004 case 3: 소켓에 raw 연결만 하고 아무 바이트도 보내지 않는 half-open client가
// 다른 정상 클라이언트의 요청을 차단해서는 안 된다. RPC 라운드트립이 single-flight로
// 직렬화되면 day-1 사용자가 "lterm 명령이 행 걸린다"고 느낄 수 있다.
#[test]
fn half_open_client_does_not_block_other_clients() -> TestResult {
    let env = LifecycleEnv::new()?;
    env.ensure_daemon("lifecycle-half-open")?;
    let socket = env.socket_path()?;

    let half_open = UnixStream::connect(&socket)?;
    half_open.set_read_timeout(Some(Duration::from_millis(50)))?;

    // 다른 client가 정상 동작해야 한다. 동시에 RPC 라운드트립이 half-open peer 때문에
    // single-flight로 직렬화되면 사용자가 행 걸린다고 느낀다 — 합리적 상한선 안에서
    // 끝남을 명시적으로 가드한다. RPC_TIMEOUT(5초)에서 충분히 거리를 두되 디버그
    // 빌드 process 시작 비용 + RPC 라운드트립까지 흡수하도록 2초로 둔다. CI에서
    // flake가 관측되면 별도 PR에서 증액하거나 RPC-only 측정으로 분리한다.
    const HALF_OPEN_SECOND_CLIENT_MAX: Duration = Duration::from_secs(2);
    let started = Instant::now();
    let doctor_report = env.doctor_json()?;
    let elapsed = started.elapsed();
    // 진단 흐름: 기능 정상성(daemon_reachable)을 먼저 확인하고, 그 다음에 성능
    // 특성(elapsed bound)을 검증한다. 그래야 RPC가 빠르게 실패한 케이스가 latency
    // bound assertion에 가려지지 않는다.
    assert_eq!(
        doctor_report
            .get("daemon_reachable")
            .and_then(|v| v.as_bool()),
        Some(true),
        "second client should reach daemon while first is half-open: {doctor_report:?}"
    );
    assert!(
        elapsed < HALF_OPEN_SECOND_CLIENT_MAX,
        "second client must not be blocked by half-open peer: elapsed={elapsed:?}, bound={HALF_OPEN_SECOND_CLIENT_MAX:?}, doctor={doctor_report:?}"
    );

    // half-open 끝에 도달하는 응답 유무는 contract surface가 아니므로 panic만 가드.
    let mut buf = [0u8; 16];
    let _ = (&half_open).read(&mut buf);
    drop(half_open);
    Ok(())
}

// US-004 case 4: orphan/cleanup observability surface(`lterm processes --orphans`)이
// 결정적 입력에 대해 실행 가능하고, 출력 surface가 살아 있는지 회귀 가드한다.
// orphan 검출 자체는 OS 스케줄링·시그널 타이밍에 따라 비결정적이므로, 본 테스트는
// 명령이 정상 종료하고 sanitized 텍스트를 반환한다는 contract surface만 확인한다.
#[test]
fn processes_orphans_command_is_available() -> TestResult {
    let env = LifecycleEnv::new()?;
    env.ensure_daemon("lifecycle-orphans-init")?;

    let out = env.cmd().args(["processes", "--orphans"]).output()?;
    assert!(
        out.status.success(),
        "lterm processes --orphans should exit success on healthy daemon; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked at"),
        "processes --orphans must not panic: {stderr}"
    );
    Ok(())
}

// US-004 case 5: socket 경로에 symlink가 들어 있으면 prepare_socket_path는 명시적으로
// 거부해야 한다. SECURITY.md "Socket paths and private directories" trust boundary
// 회귀 가드. 동일-사용자 외 peer credential(`getpeereid` ≠ `geteuid`) 직접
// 시뮬레이션은 동일-사용자 테스트 환경에서 권한 없이 재현 불가능하므로 동일
// boundary를 path-level에서 행사하는 본 케이스로 갈음한다 (SECURITY.md "Peer
// credentials" 참조).
#[test]
fn symlink_socket_path_is_refused() -> TestResult {
    let env = LifecycleEnv::new()?;
    let socket = env.socket_path()?;
    env.shutdown_and_cleanup_socket()?;
    assert!(
        !socket.exists(),
        "socket path must be empty before placing symlink bait: {}",
        socket.display()
    );

    // 동일 tempdir 내에 임의 타깃을 두고 symlink로 만든다.
    let bait = env.temp.path().join("bait");
    fs::write(&bait, b"bait")?;
    symlink(&bait, &socket)?;

    let out = env.cmd().args(["list"]).output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "lterm list must fail when socket path is a symlink. stdout={}, stderr={}",
        String::from_utf8_lossy(&out.stdout),
        stderr
    );
    // 어느 트레이스 경로든 (a) 데몬이 prepare_socket_path에서 "refusing symlink"로
    // 거부하거나 (b) 클라이언트가 UnixStream::connect에서 ENOTSOCK(macOS:
    // "non-socket"/"Socket operation")이나 ECONNREFUSED(Linux: "Connection
    // refused")로 거부한다. 세 경우 모두 보안 invariant는 동일하다 — silently
    // symlink를 따라가지 않음.
    assert!(
        stderr.contains("symlink")
            || stderr.contains("non-socket")
            || stderr.contains("Socket operation")
            || stderr.contains("Connection refused"),
        "expected refusal of symlink/non-socket socket path, got: {}",
        stderr
    );

    // symlink 타깃 자체는 보존되어야 한다 — 거부 경로가 target을 건드리면 안 됨.
    assert_eq!(
        fs::read(&bait)?,
        b"bait",
        "symlink target must not be touched"
    );
    // socket path가 여전히 symlink인 채로 남아 있어야 한다 (silently 교체 금지).
    let socket_meta = fs::symlink_metadata(&socket)?;
    assert!(
        socket_meta.file_type().is_symlink(),
        "socket path must remain a symlink (not silently replaced): {:?}",
        socket_meta.file_type()
    );

    let _ = fs::remove_file(&socket);
    Ok(())
}

// US-004 case 6: socket path가 symlink이고 그 target이 살아있는 다른 Unix socket을
// 가리킬 때도 거부되어야 한다. dangling/regular-file symlink는 case 5에서 검증되며,
// 본 케이스는 "target이 valid Unix socket이라 connect는 성공할 수 있다"는 회피
// 경로를 가드한다 — lterm이 silently symlink를 따라가 임의의 socket peer에 연결하면
// trust boundary가 무너진다.
#[test]
fn symlink_socket_path_pointing_to_live_unix_socket_is_refused() -> TestResult {
    let env = LifecycleEnv::new()?;
    let socket = env.socket_path()?;
    env.shutdown_and_cleanup_socket()?;
    assert!(
        !socket.exists(),
        "socket path must be empty before placing live-socket symlink bait: {}",
        socket.display()
    );

    let bait_dir = env.temp.path().join("bait_dir");
    fs::create_dir(&bait_dir)?;
    let bait_socket = bait_dir.join("alive.sock");
    let listener = UnixListener::bind(&bait_socket)?;
    listener.set_nonblocking(true)?;
    let accepted = Arc::new(AtomicBool::new(false));
    let stop_accept = Arc::new(AtomicBool::new(false));
    let accepted_for_thread = Arc::clone(&accepted);
    let stop_for_thread = Arc::clone(&stop_accept);
    let accept_thread = thread::spawn(move || -> std::io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !stop_for_thread.load(Ordering::Acquire) && Instant::now() < deadline {
            match listener.accept() {
                Ok((_stream, _addr)) => {
                    accepted_for_thread.store(true, Ordering::Release);
                    return Ok(());
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    });
    symlink(&bait_socket, &socket)?;

    let mut list = env.cmd();
    list.args(["list"]);
    let out_result = command_output_with_timeout(
        &mut list,
        Duration::from_secs(3),
        "list with symlink-to-live-socket bait",
    );
    // Give the nonblocking accept loop a brief post-command drain window so a
    // forbidden connect queued just before process exit cannot be missed.
    thread::sleep(Duration::from_millis(150));
    stop_accept.store(true, Ordering::Release);
    accept_thread
        .join()
        .map_err(|_| "bait listener accept thread panicked")??;
    assert!(
        !accepted.load(Ordering::Acquire),
        "lterm list followed the symlink and connected to bait socket {}",
        bait_socket.display()
    );
    let out = out_result?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "lterm list must fail when socket path is a symlink to a live unix socket. stdout={}, stderr={}",
        String::from_utf8_lossy(&out.stdout),
        stderr
    );
    // case 5와 동일 invariant: silently symlink를 따라가지 않음. target이 살아있는
    // Unix socket이어도 bait listener에는 connect하지 않아야 하므로 protocol-level
    // handshake 오류가 아니라 path-level 거부만 성공으로 인정한다.
    assert!(
        stderr.contains("symlink")
            || stderr.contains("non-socket")
            || stderr.contains("Socket operation"),
        "expected path-level refusal of symlink-to-live-socket, got: {stderr}"
    );

    // bait socket 자체는 보존되어야 한다 (거부 경로가 target을 unbind하면 안 됨).
    assert!(
        fs::metadata(&bait_socket).is_ok(),
        "bait live socket must not be touched"
    );
    // socket_path는 여전히 symlink여야 한다 — silently 교체 금지.
    let socket_meta = fs::symlink_metadata(&socket)?;
    assert!(
        socket_meta.file_type().is_symlink(),
        "socket path must remain a symlink (not silently replaced): {:?}",
        socket_meta.file_type()
    );

    let _ = fs::remove_file(&socket);
    Ok(())
}

// US-004 case 7: `lterm processes --orphans`은 관찰만 하는 observability 명령이므로
// session/connection 카운터에 사이드 이펙트를 주면 안 된다. 기존 case 4가 "명령이
// 정상 종료 + panic-free" 표면만 확인했다면 본 케이스는 호출 전후 doctor 카운터가
// 정확히 보존됨을 확인해 관찰자 효과 회귀를 가드한다.
#[test]
fn processes_orphans_does_not_mutate_observable_state() -> TestResult {
    let env = LifecycleEnv::new()?;
    env.ensure_daemon("lifecycle-orphans-idem")?;

    let before = env.doctor_json()?;
    let before_sessions = before.get("daemon_session_count").and_then(|v| v.as_u64());
    let before_conns = before
        .get("daemon_active_connections")
        .and_then(|v| v.as_u64());

    let out = env.cmd().args(["processes", "--orphans"]).output()?;
    assert!(
        out.status.success(),
        "processes --orphans must exit success on healthy daemon; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout.clone())
        .map_err(|e| format!("processes --orphans stdout must be valid UTF-8: {e}"))?;
    assert!(
        !stdout.contains("panicked at"),
        "processes --orphans stdout must not contain panic trace: {stdout}"
    );

    let after = env.doctor_json()?;
    assert_eq!(
        after.get("daemon_session_count").and_then(|v| v.as_u64()),
        before_sessions,
        "session count must not change across processes --orphans"
    );
    assert_eq!(
        after
            .get("daemon_active_connections")
            .and_then(|v| v.as_u64()),
        before_conns,
        "active_connections must not change across processes --orphans"
    );
    Ok(())
}

use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const LEGACY_TAG: &str = "v0.1.4";
const LEGACY_BIN_ENV: &str = "LTERM_UPGRADE_V0_1_4_BIN";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(5);
const LEGACY_BUILD_TIMEOUT: Duration = Duration::from_secs(300);

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct MatrixEnv {
    temp: tempfile::TempDir,
}

impl MatrixEnv {
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

    fn tmp_dir(&self) -> PathBuf {
        self.temp.path().join("tmp")
    }

    fn apply_to(&self, command: &mut Command) {
        // 개발자 호스트의 live daemon socket 을 상속하지 않도록 LTERM_SOCKET 을
        // 제거한다. TMPDIR 도 sandbox 안에 고정해, future fallback-path 변경이
        // LTERM_RUNTIME_DIR 직접 사용을 멈추더라도 MatrixEnv 밖으로 빠지지 않게 한다.
        command.env_remove("LTERM_SOCKET");
        command.env("LTERM_RUNTIME_DIR", self.runtime_dir());
        command.env("LTERM_DATA_DIR", self.data_dir());
        command.env("TMPDIR", self.tmp_dir());
    }
}

struct LegacyBinary {
    _temp: Option<tempfile::TempDir>,
    path: PathBuf,
}

struct DaemonGuard {
    child: Option<Child>,
}

impl DaemonGuard {
    fn spawn(binary: &Path, env: &MatrixEnv) -> TestResult<Self> {
        let mut command = Command::new(binary);
        env.apply_to(&mut command);
        command
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command
            .spawn()
            .map_err(|err| format!("failed to spawn daemon from {}: {err}", binary.display()))?;
        Ok(Self { child: Some(child) })
    }

    fn kill_and_wait(&mut self) -> TestResult {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        if child.try_wait()?.is_none() {
            match child.kill() {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => {}
                Err(err) => {
                    return Err(format!("failed to kill daemon {}: {err}", child.id()).into());
                }
            }
        }
        let mut child = self.child.take().ok_or("daemon child already reaped")?;
        wait_child_exit(&mut child, Duration::from_secs(3))
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.kill_and_wait();
    }
}

#[test]
fn old_daemon_v0_1_4_current_client() -> TestResult {
    let Some(legacy) = legacy_lterm_binary()? else {
        return Ok(());
    };
    let env = MatrixEnv::new()?;
    let mut daemon = DaemonGuard::spawn(&legacy.path, &env)?;
    let current = current_lterm_binary();

    let doctor = wait_for_daemon(&current, &env, LEGACY_TAG.trim_start_matches('v'))?;
    assert_eq!(doctor["daemon_version"], "0.1.4");
    assert!(
        doctor["daemon_protocol_version"].as_u64().is_some(),
        "current client should receive an explicit daemon protocol in doctor report: {doctor:#}"
    );

    assert_session_round_trip(
        &current,
        &env,
        "upgrade-old-daemon-current-client",
        "LTERM_UPGRADE_OLD_DAEMON_CURRENT_CLIENT",
    )?;

    let _ = run_lterm(&current, &env, ["shutdown"], COMMAND_TIMEOUT);
    wait_child_exit(
        daemon.child.as_mut().ok_or("daemon already reaped")?,
        Duration::from_secs(3),
    )?;
    daemon.child = None;
    Ok(())
}

#[test]
fn current_daemon_v0_1_4_client() -> TestResult {
    let Some(legacy) = legacy_lterm_binary()? else {
        return Ok(());
    };
    let env = MatrixEnv::new()?;
    let current = current_lterm_binary();
    let mut daemon = DaemonGuard::spawn(&current, &env)?;

    let doctor = wait_for_daemon(&legacy.path, &env, env!("CARGO_PKG_VERSION"))?;
    assert_eq!(doctor["client_version"], "0.1.4");
    assert_eq!(doctor["daemon_version"], env!("CARGO_PKG_VERSION"));
    assert!(
        doctor["daemon_protocol_version"].as_u64().is_some(),
        "legacy client should receive an explicit daemon protocol in doctor report: {doctor:#}"
    );

    assert_session_round_trip(
        &legacy.path,
        &env,
        "upgrade-current-daemon-v0-1-4-client",
        "LTERM_UPGRADE_CURRENT_DAEMON_V0_1_4_CLIENT",
    )?;

    let _ = run_lterm(&legacy.path, &env, ["shutdown"], COMMAND_TIMEOUT);
    wait_child_exit(
        daemon.child.as_mut().ok_or("daemon already reaped")?,
        Duration::from_secs(3),
    )?;
    daemon.child = None;
    Ok(())
}

fn current_lterm_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lterm"))
}

fn legacy_lterm_binary() -> TestResult<Option<LegacyBinary>> {
    if let Some(path) = std::env::var_os(LEGACY_BIN_ENV) {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(format!(
                "{LEGACY_BIN_ENV} points to a missing lterm binary: {}",
                path.display()
            )
            .into());
        }
        return Ok(Some(LegacyBinary { _temp: None, path }));
    }

    let temp = tempfile::tempdir()?;
    let source_dir = temp.path().join("source");
    let target_dir = temp.path().join("target");
    fs::create_dir_all(&source_dir)?;

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tag_check = run_with_timeout(
        Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["rev-parse", "--verify"])
            .arg(format!("refs/tags/{LEGACY_TAG}^{{commit}}")),
        Duration::from_secs(10),
    );
    match tag_check {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            eprintln!(
                "skipping {LEGACY_TAG} upgrade matrix because the tag is unavailable: {}",
                output_preview(&output)
            );
            return Ok(None);
        }
        Err(err) => {
            eprintln!(
                "skipping {LEGACY_TAG} upgrade matrix because git metadata is unavailable: {err}"
            );
            return Ok(None);
        }
    }

    let archive_path = temp.path().join(format!("{LEGACY_TAG}.tar"));
    run_success(
        Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["archive", "--format=tar", "--output"])
            .arg(&archive_path)
            .arg(LEGACY_TAG),
        Duration::from_secs(30),
    )?;
    run_success(
        Command::new("tar")
            .arg("-xf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&source_dir),
        Duration::from_secs(30),
    )?;

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut build = Command::new(cargo);
    build
        .current_dir(&source_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "--quiet", "--locked", "--bin", "lterm"]);
    run_success(&mut build, LEGACY_BUILD_TIMEOUT)?;

    let bin = target_dir
        .join("debug")
        .join(format!("lterm{}", std::env::consts::EXE_SUFFIX));
    if !bin.is_file() {
        return Err(format!("legacy build did not produce {}", bin.display()).into());
    }

    Ok(Some(LegacyBinary {
        _temp: Some(temp),
        path: bin,
    }))
}

fn wait_for_daemon(
    binary: &Path,
    env: &MatrixEnv,
    expected_daemon_version: &str,
) -> TestResult<Value> {
    let deadline = Instant::now() + DAEMON_READY_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        match run_lterm(binary, env, ["doctor", "--json"], COMMAND_TIMEOUT) {
            Ok(output) if output.status.success() => {
                let report: Value = serde_json::from_slice(&output.stdout)?;
                if report["daemon_reachable"].as_bool() == Some(true) {
                    let daemon_version = report["daemon_version"].as_str().unwrap_or_default();
                    if daemon_version == expected_daemon_version {
                        return Ok(report);
                    }
                    last = format!("daemon version {daemon_version:?}; report={report:#}");
                } else {
                    last = format!("daemon not reachable; report={report:#}");
                }
            }
            Ok(output) => {
                last = output_preview(&output);
            }
            Err(err) => {
                last = err.to_string();
            }
        }
        thread::sleep(Duration::from_millis(80));
    }
    Err(format!(
        "timed out waiting for daemon version {expected_daemon_version:?} via {}; last: {last}",
        binary.display()
    )
    .into())
}

fn assert_session_round_trip(
    binary: &Path,
    env: &MatrixEnv,
    session: &str,
    marker: &str,
) -> TestResult {
    let script = format!("printf '%s\\n' {marker}; sleep 2");
    let output = run_lterm(
        binary,
        env,
        [
            "start", "--detach", "--name", session, "--", "sh", "-lc", &script,
        ],
        COMMAND_TIMEOUT,
    )?;
    assert_success(&output, "start --detach")?;

    wait_for_logs(binary, env, session, marker)?;

    let sessions = run_lterm(binary, env, ["sessions", "--json"], COMMAND_TIMEOUT)?;
    assert_success(&sessions, "sessions --json")?;
    let rows: Value = serde_json::from_slice(&sessions.stdout)?;
    assert!(
        rows.as_array().is_some_and(|items| items
            .iter()
            .any(|item| item["name"].as_str() == Some(session))),
        "session {session:?} missing from sessions --json: {rows:#}"
    );

    let close = run_lterm(binary, env, ["close", session], COMMAND_TIMEOUT)?;
    assert_success(&close, "close")
}

fn wait_for_logs(binary: &Path, env: &MatrixEnv, session: &str, marker: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = String::new();
    while Instant::now() < deadline {
        match run_lterm(
            binary,
            env,
            ["logs", session, "--start", "-20"],
            COMMAND_TIMEOUT,
        ) {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains(marker) {
                    return Ok(());
                }
                last = stdout.into_owned();
            }
            Ok(output) => last = output_preview(&output),
            Err(err) => last = err.to_string(),
        }
        thread::sleep(Duration::from_millis(80));
    }
    Err(format!("timed out waiting for {marker:?} in {session:?} logs; last: {last}").into())
}

fn run_lterm<const N: usize>(
    binary: &Path,
    env: &MatrixEnv,
    args: [&str; N],
    timeout: Duration,
) -> TestResult<Output> {
    let mut command = Command::new(binary);
    env.apply_to(&mut command);
    command.args(args);
    run_with_timeout(&mut command, timeout)
}

fn run_success(command: &mut Command, timeout: Duration) -> TestResult<Output> {
    let output = run_with_timeout(command, timeout)?;
    assert_success(&output, &format!("{command:?}"))?;
    Ok(output)
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> TestResult<Output> {
    let command_desc = format!("{command:?}");
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map_err(|err| {
                format!("failed to collect output for {command_desc}: {err}").into()
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            return Err(format!(
                "command timed out after {timeout:?}: {command_desc}; {}",
                output_preview(&output)
            )
            .into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_child_exit(child: &mut Child, timeout: Duration) -> TestResult {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!("process {} did not exit within {timeout:?}", child.id()).into())
}

fn assert_success(output: &Output, context: &str) -> TestResult {
    if output.status.success() {
        return Ok(());
    }
    Err(format!("{context} failed: {}", output_preview(output)).into())
}

fn output_preview(output: &Output) -> String {
    format!(
        "status={:?}; stdout={:?}; stderr={:?}",
        output.status.code(),
        truncate(&String::from_utf8_lossy(&output.stdout)),
        truncate(&String::from_utf8_lossy(&output.stderr))
    )
}

fn truncate(value: &str) -> String {
    const LIMIT: usize = 2_000;
    let mut truncated: String = value.chars().take(LIMIT).collect();
    if value.chars().count() > LIMIT {
        truncated.push('…');
    }
    truncated
}

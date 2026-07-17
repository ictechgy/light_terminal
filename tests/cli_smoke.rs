use std::collections::BTreeSet;
use std::ffi::OsString;
#[cfg(unix)]
use std::fs::File;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const ERR_BARE_PANE_ID: &str = "bare pane id";
const ERR_EMPTY_SESSION_NAME: &str = "session name cannot be empty";
const ERR_INVALID_SESSION_CHARS: &str = "may only contain ASCII";
const ERR_LEADING_DASH_NAME: &str = "cannot start with '-'";
const ERR_SESSION_EXISTS: &str = "session name already exists";
const ERR_SESSION_NAME: &str = "session name";
const MAX_TRACE_REPLAY_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const CLIENT_ONLY_ENV_SHOULD_NOT_FORWARD: &str = "LTERM_SHOULD_NOT_FORWARD_CODEX_HOME_REGRESSION";

struct TestEnv {
    temp: tempfile::TempDir,
}

impl TestEnv {
    fn new() -> TestResult<Self> {
        let temp = tempfile::tempdir()?;
        std::fs::create_dir_all(temp.path().join("tmp"))?;
        Ok(Self { temp })
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lterm"));
        cmd.env_remove("LTERM_SOCKET")
            .env_remove("LTERM_PANE")
            .env_remove("LTERM_PARENT_TOKEN")
            .env("LTERM_RUNTIME_DIR", self.temp.path().join("run"))
            .env("LTERM_DATA_DIR", self.temp.path().join("data"))
            .env("TMPDIR", self.temp.path().join("tmp"));
        cmd
    }

    fn capture_until(&self, target: &str, needle: &str) -> TestResult<String> {
        self.capture_command_until("logs", target, needle)
    }

    fn capture_command_until(
        &self,
        command: &str,
        target: &str,
        needle: &str,
    ) -> TestResult<String> {
        poll_until(
            Duration::from_secs(5),
            Duration::from_millis(50),
            &format!("{command} output containing {needle:?}"),
            || {
                let output = self.cmd().args([command, target, "-S=-20"]).output()?;
                if output.status.success() {
                    let captured = String::from_utf8_lossy(&output.stdout).to_string();
                    if captured.contains(needle) {
                        return Ok(PollStatus::Ready(captured));
                    }
                    Ok(PollStatus::Pending(format!("last capture: {captured}")))
                } else {
                    Ok(PollStatus::Pending(format!(
                        "status={:?}; stdout={:?}; stderr={:?}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    )))
                }
            },
        )
    }

    fn start_daemon_without_codex_home(&self) -> TestResult<ChildCleanup> {
        self.start_daemon_with_codex_home(None, &[])
    }

    fn start_daemon_with_codex_home(
        &self,
        codex_home: Option<&Path>,
        extra_removed_env: &[&str],
    ) -> TestResult<ChildCleanup> {
        let mut daemon = self.cmd();
        daemon.arg("daemon");
        match codex_home {
            Some(value) => {
                daemon.env("CODEX_HOME", value);
            }
            None => {
                daemon.env_remove("CODEX_HOME");
            }
        }
        for key in extra_removed_env {
            daemon.env_remove(key);
        }
        daemon
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = ChildCleanup::new(daemon.spawn()?);
        self.wait_for_reachable_daemon()?;
        Ok(child)
    }

    fn wait_for_reachable_daemon(&self) -> TestResult {
        poll_until(
            Duration::from_secs(15),
            Duration::from_millis(50),
            "prestarted daemon to become reachable",
            || {
                let output = self.cmd().args(["doctor", "--json"]).output()?;
                if output.status.success() {
                    let report: serde_json::Value = match serde_json::from_slice(&output.stdout) {
                        Ok(report) => report,
                        Err(err) => {
                            return Ok(PollStatus::Pending(format!(
                                "doctor returned non-json while daemon started: {err}; stdout={:?}; stderr={:?}",
                                String::from_utf8_lossy(&output.stdout),
                                String::from_utf8_lossy(&output.stderr)
                            )));
                        }
                    };
                    if report
                        .get("daemon_reachable")
                        .and_then(|value| value.as_bool())
                        == Some(true)
                    {
                        return Ok(PollStatus::Ready(()));
                    }
                    Ok(PollStatus::Pending(format!(
                        "daemon not reachable yet: {}",
                        String::from_utf8_lossy(&output.stdout)
                    )))
                } else {
                    Ok(PollStatus::Pending(format!(
                        "doctor status={:?}; stdout={:?}; stderr={:?}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    )))
                }
            },
        )
    }
}

fn temp_tree_snapshot(root: &Path) -> TestResult<BTreeSet<String>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeSet<String>) -> TestResult {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            let metadata = std::fs::symlink_metadata(&path)?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                out.insert(format!("dir:{rel}"));
                visit(root, &path, out)?;
            } else if file_type.is_file() {
                let bytes = std::fs::read(&path)?;
                let hex = bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                out.insert(format!("file:{rel}:{hex}"));
            } else if file_type.is_symlink() {
                out.insert(format!(
                    "symlink:{rel}:{}",
                    std::fs::read_link(&path)?.display()
                ));
            } else {
                out.insert(format!("other:{rel}"));
            }
        }
        Ok(())
    }

    let mut snapshot = BTreeSet::new();
    visit(root, root, &mut snapshot)?;
    Ok(snapshot)
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = self.cmd().arg("shutdown").status();
    }
}

#[cfg(unix)]
fn spawn_fake_capability_server<F>(
    listener: UnixListener,
    before_operation_response: F,
    send_success: bool,
) -> thread::JoinHandle<Result<Vec<u8>, String>>
where
    F: FnOnce() -> TestResult + Send + 'static,
{
    thread::spawn(move || {
        (|| -> TestResult<Vec<u8>> {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept()?;
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes)?;
                let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                let response = if request["type"] == "ping" {
                    serde_json::json!({"ok":true,"result":{"pong":true}})
                } else {
                    serde_json::json!({"ok":true,"result":{
                        "version":"1.0.30",
                        "protocol_version":5,
                        "session_count":0,
                        "active_connections":1,
                        "shutting_down":false
                    }})
                };
                stream.write_all(serde_json::to_string(&response)?.as_bytes())?;
            }
            let (stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut reader = std::io::BufReader::new(stream);
            let mut hello = Vec::new();
            reader.read_until(b'\n', &mut hello)?;
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&hello)?["type"],
                "capability_channel"
            );
            reader
                .get_mut()
                .write_all(b"{\"ok\":true,\"result\":{\"ready\":true,\"protocol_version\":5}}\n")?;
            reader.get_mut().flush()?;
            let mut sensitive = Vec::new();
            reader.read_until(b'\n', &mut sensitive)?;
            before_operation_response()?;
            if send_success {
                reader.get_mut().write_all(b"{\"ok\":true}\n")?;
                reader.get_mut().flush()?;
            }
            Ok(sensitive)
        })()
        .map_err(|err| err.to_string())
    })
}

enum PollStatus<T> {
    Ready(T),
    Pending(String),
}

enum PollUntilError<E> {
    Timeout(String),
    Check(E),
}

fn poll_until_result<T, E, F>(
    timeout: Duration,
    interval: Duration,
    label: &str,
    mut check: F,
) -> Result<T, PollUntilError<E>>
where
    F: FnMut() -> Result<PollStatus<T>, E>,
{
    let deadline = Instant::now() + timeout;
    let last = loop {
        match check().map_err(PollUntilError::Check)? {
            PollStatus::Ready(value) => return Ok(value),
            PollStatus::Pending(detail) => {
                let now = Instant::now();
                if now >= deadline {
                    break detail;
                }
                thread::sleep(interval.min(deadline.saturating_duration_since(now)));
            }
        }
    };
    Err(PollUntilError::Timeout(format!(
        "timed out waiting for {label} after {timeout:?}; last={last}"
    )))
}

fn poll_until<T, F>(timeout: Duration, interval: Duration, label: &str, check: F) -> TestResult<T>
where
    F: FnMut() -> TestResult<PollStatus<T>>,
{
    match poll_until_result(timeout, interval, label, check) {
        Ok(value) => Ok(value),
        Err(PollUntilError::Check(err)) => Err(err),
        Err(PollUntilError::Timeout(message)) => Err(message.into()),
    }
}

struct ChildCleanup {
    child: Option<Child>,
}

impl ChildCleanup {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> TestResult<&mut Child> {
        self.child.as_mut().ok_or("child already reaped".into())
    }

    fn kill_and_wait(&mut self) -> TestResult {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let child_id = child.id();
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        let mut kill_error = None;
        match child.kill() {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
                ) => {}
            Err(err) => {
                kill_error = Some(format!("failed to kill child {child_id}: {err}"));
            }
        }
        let wait_result = wait_child_exit(&mut child, Duration::from_secs(3));
        match (kill_error, wait_result) {
            (Some(kill_error), Err(wait_error)) => Err(format!(
                "{kill_error}; additionally failed to reap child {child_id}: {wait_error}"
            )
            .into()),
            (Some(kill_error), Ok(())) => Err(kill_error.into()),
            (None, Err(wait_error)) => Err(wait_error),
            (None, Ok(())) => Ok(()),
        }
    }
}

fn wait_child_exit(child: &mut Child, timeout: Duration) -> TestResult {
    poll_until(
        timeout,
        Duration::from_millis(50),
        &format!("process {} exit", child.id()),
        || {
            if child.try_wait()?.is_some() {
                Ok(PollStatus::Ready(()))
            } else {
                Ok(PollStatus::Pending("still running".to_string()))
            }
        },
    )
}

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        let _ = self.kill_and_wait();
    }
}

fn wait_for_child_success(child: &mut ChildCleanup, label: &str) -> TestResult {
    poll_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        label,
        || {
            if let Some(status) = child.child_mut()?.try_wait()? {
                assert!(status.success(), "{label} failed: {status:?}");
                child.child = None;
                Ok(PollStatus::Ready(()))
            } else {
                Ok(PollStatus::Pending("still running".to_string()))
            }
        },
    )
}

fn wait_for_child_output(
    mut child: ChildCleanup,
    timeout: Duration,
    label: &str,
) -> TestResult<std::process::Output> {
    let wait_result = poll_until_result(timeout, Duration::from_millis(25), label, || {
        if child.child_mut()?.try_wait()?.is_some() {
            Ok(PollStatus::Ready(()))
        } else {
            Ok(PollStatus::Pending("still running".to_string()))
        }
    });
    match wait_result {
        Ok(()) => {}
        Err(PollUntilError::Check(err)) => return Err(err),
        Err(PollUntilError::Timeout(wait_error)) => {
            let Some(mut process) = child.child.take() else {
                return Err(format!("{label} already reaped before timeout collection").into());
            };
            let _ = process.kill();
            let output = process.wait_with_output()?;
            return Err(format!(
                "{wait_error}; stdout={:?}; stderr={:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
    }

    let process = child.child.take().ok_or("child already reaped")?;
    process
        .wait_with_output()
        .map_err(|err| format!("failed to collect output for {label}: {err}").into())
}

fn list_row<'a>(stdout: &'a str, name: &str) -> Option<Vec<&'a str>> {
    stdout
        .lines()
        .find(|line| line.starts_with(&format!("{name}\t")))
        .map(|line| line.split('\t').collect())
}

fn assert_exact_line_set(stdout: &str, expected: &[&str]) {
    let actual: Vec<&str> = stdout.lines().collect();
    let actual_set: BTreeSet<&str> = actual.iter().copied().collect();
    let expected_set: BTreeSet<&str> = expected.iter().copied().collect();
    assert_eq!(
        actual.len(),
        expected.len(),
        "expected exactly {expected_set:?}, got lines {actual:?}"
    );
    assert_eq!(
        actual_set, expected_set,
        "expected exactly {expected_set:?}, got lines {actual:?}"
    );
}

fn session_names_json(env: &TestEnv) -> TestResult<BTreeSet<String>> {
    let output = env.cmd().args(["sessions", "--json"]).output()?;
    assert!(output.status.success(), "{output:?}");
    let sessions: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    Ok(sessions
        .iter()
        .filter_map(|row| row.get("name").and_then(serde_json::Value::as_str))
        .map(ToOwned::to_owned)
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRowJson {
    name: String,
    pane_id: String,
    parent_pane_id: Option<String>,
}

fn session_rows_json(env: &TestEnv, all: bool) -> TestResult<Vec<SessionRowJson>> {
    let mut cmd = env.cmd();
    cmd.args(["sessions", "--json"]);
    if all {
        cmd.arg("--all");
    }
    let output = cmd.output()?;
    assert!(output.status.success(), "{output:?}");
    let sessions: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    let mut rows = Vec::new();
    for session in sessions {
        rows.push(SessionRowJson {
            name: session
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("session row missing name: {session:?}"))?
                .to_string(),
            pane_id: session
                .get("pane_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("session row missing pane_id: {session:?}"))?
                .to_string(),
            parent_pane_id: session
                .get("parent_pane_id")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned),
        });
    }
    Ok(rows)
}

fn session_row_names(rows: &[SessionRowJson]) -> BTreeSet<String> {
    rows.iter().map(|row| row.name.clone()).collect()
}

fn wait_for_session_names_eq(
    env: &TestEnv,
    expected: &BTreeSet<String>,
    timeout: Duration,
) -> TestResult {
    poll_until(
        timeout,
        Duration::from_millis(100),
        &format!("session set {expected:?}"),
        || {
            let names = session_names_json(env)?;
            if &names == expected {
                Ok(PollStatus::Ready(()))
            } else {
                Ok(PollStatus::Pending(format!("{names:?}")))
            }
        },
    )
}

fn data_store_path(env: &TestEnv) -> PathBuf {
    env.temp.path().join("data").join("tmux-compat-store.json")
}

#[cfg(unix)]
fn fake_live_session(index: usize) -> serde_json::Value {
    serde_json::json!({
        "id": format!("00000000-0000-4000-8000-{index:012x}"),
        "name": format!("fake-session-{index}"),
        "pane_id": format!("%{index}"),
        "command": "sleep 30",
        "cwd": "/tmp",
        "created_unix_ms": index,
        "alive": true,
        "exit_code": null,
        "rows": 24,
        "cols": 80,
        "parent_pane_id": null,
        "parent_session_id": null,
        "attached_clients": 0,
        "process_id": null,
        "process_group_id": null
    })
}

#[cfg(unix)]
fn run_tmux_with_fake_sessions(
    env: &TestEnv,
    args: &[&str],
    sessions: Vec<serde_json::Value>,
) -> TestResult<std::process::Output> {
    let run_dir = env.temp.path().join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))?;
    let socket = run_dir.join("lterm.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let child_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_child_done = std::sync::Arc::clone(&child_done);
    let server = thread::spawn(move || -> Result<(), String> {
        (|| -> TestResult {
            const EXPECTED_REQUESTS: [&str; 3] = ["ping", "status", "list"];
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut request_count = 0usize;
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false)?;
                        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                        let mut bytes = Vec::new();
                        stream.read_to_end(&mut bytes)?;
                        let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                        let request_type = request["type"]
                            .as_str()
                            .ok_or("fake daemon request missing type")?;
                        if EXPECTED_REQUESTS.get(request_count) != Some(&request_type) {
                            return Err(format!(
                                "unexpected fake daemon request #{request_count}: {request}"
                            )
                            .into());
                        }
                        request_count += 1;
                        let response = match request_type {
                            "ping" => serde_json::json!({"ok":true,"result":{"pong":true}}),
                            "status" => serde_json::json!({"ok":true,"result":{
                                "version":env!("CARGO_PKG_VERSION"),
                                "protocol_version":8,
                                "session_count":sessions.len(),
                                "active_connections":1,
                                "shutting_down":false
                            }}),
                            "list" => serde_json::json!({"ok":true,"result":sessions}),
                            _ => unreachable!("request type was checked above"),
                        };
                        stream.write_all(serde_json::to_string(&response)?.as_bytes())?;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        if server_child_done.load(std::sync::atomic::Ordering::Acquire) {
                            break;
                        }
                        if Instant::now() >= deadline {
                            return Err(format!(
                                "fake daemon timed out after {request_count} request(s); expected {EXPECTED_REQUESTS:?}"
                            )
                            .into());
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            if request_count != EXPECTED_REQUESTS.len() {
                return Err(format!(
                    "tmux user-option command issued {request_count} request(s); expected {EXPECTED_REQUESTS:?}"
                )
                .into());
            }
            Ok(())
        })()
        .map_err(|err| err.to_string())
    });
    let mut command = env.cmd();
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match command.spawn() {
        Ok(child) => wait_for_child_output(
            ChildCleanup::new(child),
            Duration::from_secs(8),
            "tmux user-option command with fake sessions",
        ),
        Err(err) => Err(err.into()),
    };
    child_done.store(true, std::sync::atomic::Ordering::Release);
    let server_result = server.join().map_err(|_| "fake daemon panicked")?;
    let _ = std::fs::remove_file(socket);
    server_result?;
    output
}

#[cfg(unix)]
fn run_tmux_with_fake_failed_kill(
    env: &TestEnv,
    args: &[&str],
    sessions: Vec<serde_json::Value>,
) -> TestResult<(std::process::Output, Vec<String>)> {
    let run_dir = env.temp.path().join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))?;
    let socket = run_dir.join("lterm.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let child_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_child_done = std::sync::Arc::clone(&child_done);
    let server = thread::spawn(move || -> Result<Vec<String>, String> {
        (|| -> TestResult<Vec<String>> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut requests = Vec::new();
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false)?;
                        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                        let mut bytes = Vec::new();
                        stream.read_to_end(&mut bytes)?;
                        let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                        let request_type = request["type"]
                            .as_str()
                            .ok_or("fake daemon request missing type")?;
                        requests.push(request_type.to_string());
                        let response = match request_type {
                            "ping" => serde_json::json!({"ok":true,"result":{"pong":true}}),
                            "status" => serde_json::json!({"ok":true,"result":{
                                "version":env!("CARGO_PKG_VERSION"),
                                "protocol_version":8,
                                "session_count":sessions.len(),
                                "active_connections":1,
                                "shutting_down":false
                            }}),
                            "info" => {
                                let target = request["target"].as_str().unwrap_or_default();
                                let info = sessions.iter().find(|session| {
                                    session["id"].as_str() == Some(target)
                                        || session["name"].as_str() == Some(target)
                                        || session["pane_id"].as_str() == Some(target)
                                });
                                match info {
                                    Some(info) => serde_json::json!({"ok":true,"result":info}),
                                    None => {
                                        serde_json::json!({"ok":false,"error":"target not found"})
                                    }
                                }
                            }
                            "list" => serde_json::json!({"ok":true,"result":sessions}),
                            "kill" => serde_json::json!({
                                "ok":false,
                                "error":"injected fake daemon kill failure"
                            }),
                            other => {
                                return Err(
                                    format!("unexpected fake daemon request: {other}").into()
                                );
                            }
                        };
                        stream.write_all(serde_json::to_string(&response)?.as_bytes())?;
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        if server_child_done.load(std::sync::atomic::Ordering::Acquire) {
                            return Ok(requests);
                        }
                        if Instant::now() >= deadline {
                            return Err(
                                format!("fake daemon timed out; requests={requests:?}").into()
                            );
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        })()
        .map_err(|err| err.to_string())
    });
    let mut command = env.cmd();
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match command.spawn() {
        Ok(child) => wait_for_child_output(
            ChildCleanup::new(child),
            Duration::from_secs(8),
            "tmux command with injected failed kill",
        ),
        Err(err) => Err(err.into()),
    };
    child_done.store(true, std::sync::atomic::Ordering::Release);
    let requests = server.join().map_err(|_| "fake daemon panicked")??;
    let _ = std::fs::remove_file(socket);
    Ok((output?, requests))
}

#[cfg(unix)]
fn write_user_option_store(
    env: &TestEnv,
    pane_user_options: serde_json::Map<String, serde_json::Value>,
    session_user_options: serde_json::Map<String, serde_json::Value>,
    wait_generations: serde_json::Map<String, serde_json::Value>,
) -> TestResult<Vec<u8>> {
    let path = data_store_path(env);
    let parent = path.parent().ok_or("store path has no parent")?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    let store = serde_json::json!({
        "panes": {},
        "pane_user_options": pane_user_options,
        "session_user_options": session_user_options,
        "wait_generations": wait_generations,
        "wait_generation_touched_secs": {},
        "managed_attaches": {}
    });
    let bytes = serde_json::to_vec(&store)?;
    std::fs::write(path, &bytes)?;
    Ok(bytes)
}

fn seed_managed_attach_store_with_token(
    env: &TestEnv,
    pane_id: &str,
    updated_secs: u64,
    surface_id: Option<&str>,
    token: &str,
) -> TestResult {
    seed_managed_attach_store_with_token_and_pid(
        env,
        pane_id,
        updated_secs,
        surface_id,
        token,
        std::process::id(),
    )
}

fn seed_managed_attach_store_with_token_and_pid(
    env: &TestEnv,
    pane_id: &str,
    updated_secs: u64,
    surface_id: Option<&str>,
    token: &str,
    pid: u32,
) -> TestResult {
    let path = data_store_path(env);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut store = if path.exists() {
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path)?)?
    } else {
        serde_json::json!({
            "panes": {},
            "wait_generations": {},
            "wait_generation_touched_secs": {},
            "managed_attaches": {}
        })
    };
    if !store.is_object() {
        store = serde_json::json!({});
    }
    let obj = store.as_object_mut().expect("store object");
    obj.entry("panes").or_insert_with(|| serde_json::json!({}));
    obj.entry("wait_generations")
        .or_insert_with(|| serde_json::json!({}));
    obj.entry("wait_generation_touched_secs")
        .or_insert_with(|| serde_json::json!({}));
    let leases = obj
        .entry("managed_attaches")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .expect("managed_attaches object");
    let mut lease = serde_json::json!({
        "pane_id": pane_id,
        "token": token,
        "pid": pid,
        "cmux_surface_id": surface_id,
        "cmux_workspace_id": "workspace:owner",
        "cmux_window_id": "window:owner",
        "updated_secs": updated_secs
    });
    if let Some(process_start_id) = process_start_identity_for_test(pid) {
        lease["process_start_id"] = serde_json::json!(process_start_id);
    }
    leases.insert(pane_id.to_string(), lease);
    std::fs::write(path, serde_json::to_vec_pretty(&store)?)?;
    Ok(())
}

fn seed_identityless_managed_attach_store_with_token_and_pid(
    env: &TestEnv,
    pane_id: &str,
    updated_secs: u64,
    surface_id: Option<&str>,
    token: &str,
    pid: u32,
) -> TestResult {
    seed_managed_attach_store_with_token_and_pid(
        env,
        pane_id,
        updated_secs,
        surface_id,
        token,
        pid,
    )?;
    override_managed_attach_process_start_id(env, pane_id, None)
}

fn seed_managed_attach_store(
    env: &TestEnv,
    pane_id: &str,
    updated_secs: u64,
    surface_id: Option<&str>,
) -> TestResult {
    seed_managed_attach_store_with_token(env, pane_id, updated_secs, surface_id, "seed-owner")
}

fn override_managed_attach_process_start_id(
    env: &TestEnv,
    pane_id: &str,
    process_start_id: Option<&str>,
) -> TestResult {
    let path = data_store_path(env);
    let mut store: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let lease = store
        .get_mut("managed_attaches")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|leases| leases.get_mut(pane_id))
        .and_then(serde_json::Value::as_object_mut)
        .expect("managed attach lease object");
    match process_start_id {
        Some(value) => {
            lease.insert("process_start_id".to_string(), serde_json::json!(value));
        }
        None => {
            lease.remove("process_start_id");
        }
    }
    std::fs::write(path, serde_json::to_vec_pretty(&store)?)?;
    Ok(())
}

fn managed_attach_count(env: &TestEnv) -> TestResult<usize> {
    let bytes = std::fs::read(data_store_path(env))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    Ok(value
        .get("managed_attaches")
        .and_then(serde_json::Value::as_object)
        .map_or(0, serde_json::Map::len))
}

fn managed_attach_entry(env: &TestEnv, pane_id: &str) -> TestResult<Option<serde_json::Value>> {
    let bytes = std::fs::read(data_store_path(env))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    Ok(value
        .get("managed_attaches")
        .and_then(serde_json::Value::as_object)
        .and_then(|leases| leases.get(pane_id))
        .cloned())
}

fn fresh_managed_attach_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn dead_test_pid() -> u32 {
    let mut child = Command::new(command_path("sh").expect("locate sh for dead pid helper"))
        .arg("-c")
        .arg("exit 0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn short-lived child for dead pid helper");
    let pid = child.id();
    let status = child.wait().expect("wait for dead pid helper");
    assert!(status.success(), "dead pid helper should exit successfully");
    assert_ne!(pid, 0, "dead pid helper must return a positive pid");
    pid
}

#[cfg(target_os = "macos")]
fn process_start_identity_for_test(pid: u32) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let size_i32 = libc::c_int::try_from(size).ok()?;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size_i32,
        )
    };
    if usize::try_from(read).ok()? != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(format!(
        "macos:{}:{}:{}",
        info.pbi_pid, info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

#[cfg(target_os = "linux")]
fn process_start_identity_for_test(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let mut fields = after_comm.split_whitespace();
    let start_ticks = fields.nth(19)?;
    Some(format!("linux:{pid}:{start_ticks}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_start_identity_for_test(_pid: u32) -> Option<String> {
    None
}

fn create_sleep_session(env: &TestEnv, name: &str) -> TestResult<String> {
    let shell = command_path("sh")?.display().to_string();
    let sleep = command_path("sleep")?.display().to_string();
    let script = format!(
        "printf 'SESSION_READY:%s\\n' {}; {} 30",
        shlex::try_quote(name)?,
        shlex::try_quote(&sleep)?
    );
    let output = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            name,
            "--",
            shell.as_str(),
            "-lc",
            script.as_str(),
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let mut fields = first_line.split('\t');
    let _name = fields.next();
    let Some(pane_id) = fields.next() else {
        panic!("unexpected detached output: {stdout:?}");
    };
    let pane_id = pane_id.to_string();
    // Managed attach fallback assertions expect an already-observable screen
    // snapshot. On faster CI hosts, attaching immediately after `new --detach`
    // can race the PTY reader and produce an otherwise-successful empty replay.
    env.capture_until(&pane_id, &format!("SESSION_READY:{name}"))?;
    Ok(pane_id)
}

fn assert_stderr_contains(output: &std::process::Output, expected: &str) {
    // These fragments are part of lterm's user-facing CLI error contract.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "{stderr:?}");
}

fn wait_for_pid_exit(pid: &str) -> TestResult {
    poll_until(
        Duration::from_secs(3),
        Duration::from_millis(50),
        &format!("pid {pid} exit"),
        || {
            if !pid_alive(pid)? {
                Ok(PollStatus::Ready(()))
            } else {
                Ok(PollStatus::Pending("still alive".to_string()))
            }
        },
    )
}

fn wait_for_file_contents(path: &Path) -> TestResult<String> {
    poll_until(
        Duration::from_secs(3),
        Duration::from_millis(50),
        &format!("non-empty file {}", path.display()),
        || match std::fs::read_to_string(path) {
            Ok(contents) if !contents.trim().is_empty() => Ok(PollStatus::Ready(contents)),
            Ok(_) => Ok(PollStatus::Pending("file exists but is empty".to_string())),
            Err(err) => Ok(PollStatus::Pending(format!("read error: {err}"))),
        },
    )
}

fn wait_for_trace_start_event(path: &Path) -> TestResult {
    poll_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        &format!("trace start event in {}", path.display()),
        || {
            let contents = match std::fs::read_to_string(path) {
                Ok(contents) => contents,
                Err(err) => return Ok(PollStatus::Pending(format!("read error: {err}"))),
            };
            if contents.lines().next().is_some_and(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|event| {
                        event
                            .get("type")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some("start")
            }) {
                Ok(PollStatus::Ready(()))
            } else {
                Ok(PollStatus::Pending(format!("last contents: {contents:?}")))
            }
        },
    )
}

fn wait_for_no_client_rows(env: &TestEnv, sessions: &[&str]) -> TestResult {
    poll_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        "client rows to detach",
        || {
            let output = env
                .cmd()
                .args(["tmux-compat", "list-clients", "-F", "#{client_session}"])
                .output()?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                if !stdout.lines().any(|line| sessions.contains(&line)) {
                    Ok(PollStatus::Ready(()))
                } else {
                    Ok(PollStatus::Pending(format!("rows: {stdout:?}")))
                }
            } else {
                Ok(PollStatus::Pending(format!("last output: {output:?}")))
            }
        },
    )
}

fn wait_for_session_absent(env: &TestEnv, session: &str) -> TestResult {
    wait_for_session_absent_for(env, session, Duration::from_secs(10))
}

fn wait_for_session_absent_for(env: &TestEnv, session: &str, timeout: Duration) -> TestResult {
    poll_until(
        timeout,
        Duration::from_millis(100),
        &format!("session {session:?} to exit"),
        || {
            let output = env.cmd().arg("ls").output()?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                if list_row(&stdout, session).is_none() {
                    Ok(PollStatus::Ready(()))
                } else {
                    Ok(PollStatus::Pending(format!("last ls: {stdout:?}")))
                }
            } else {
                Ok(PollStatus::Pending(format!("last output: {output:?}")))
            }
        },
    )
}

fn wait_for_session_present(env: &TestEnv, session: &str) -> TestResult {
    poll_until(
        Duration::from_secs(10),
        Duration::from_millis(100),
        &format!("session {session:?} to appear"),
        || {
            let output = env.cmd().arg("ls").output()?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                if list_row(&stdout, session).is_some() {
                    Ok(PollStatus::Ready(()))
                } else {
                    Ok(PollStatus::Pending(format!("last ls: {stdout:?}")))
                }
            } else {
                Ok(PollStatus::Pending(format!("last output: {output:?}")))
            }
        },
    )
}

struct SessionCleanup<'a> {
    env: &'a TestEnv,
    target: String,
    armed: bool,
}

impl<'a> SessionCleanup<'a> {
    fn new(env: &'a TestEnv, target: impl Into<String>) -> Self {
        Self {
            env,
            target: target.into(),
            armed: true,
        }
    }

    fn kill_now(&mut self) -> TestResult {
        let output = self.env.cmd().args(["kill", &self.target]).output()?;
        if !output.status.success() {
            if wait_for_session_absent(self.env, &self.target).is_ok() {
                self.armed = false;
                return Ok(());
            }
            return Err(format!("failed to kill session {:?}: {output:?}", self.target).into());
        }
        wait_for_session_absent(self.env, &self.target)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for SessionCleanup<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .env
                .cmd()
                .args(["kill", &self.target])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = wait_for_session_absent_for(self.env, &self.target, Duration::from_secs(1));
        }
    }
}

fn write_executable(path: &Path, contents: &str) -> TestResult {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn path_with_prepended(dir: &Path) -> TestResult<OsString> {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    Ok(std::env::join_paths(paths)?)
}

fn command_path(command: &str) -> TestResult<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(command);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!("{command:?} not found in PATH").into())
}

#[cfg(unix)]
fn pid_alive(pid: &str) -> TestResult<bool> {
    let output = match Command::new(ps_path()?)
        .args(["-o", "stat=", "-p", pid])
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return pid_exists_by_signal(pid);
        }
        Err(err) => return Err(err.into()),
    };
    if !output.status.success() {
        if output.stdout.is_empty() && output.stderr.is_empty() {
            return Ok(false);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Operation not permitted") || stderr.contains("Permission denied") {
            return pid_exists_by_signal(pid);
        }
        return Err(format!(
            "ps failed while checking pid {pid}: status={:?}, stderr={}",
            output.status.code(),
            stderr
        )
        .into());
    }
    let stat = String::from_utf8_lossy(&output.stdout);
    let stat = stat.trim();
    // For cleanup tests, a zombie has already stopped executing and only awaits
    // parent reap, so treat it as terminated rather than as a surviving child.
    Ok(!stat.is_empty() && !stat.starts_with('Z'))
}

#[cfg(unix)]
fn ps_path() -> TestResult<&'static str> {
    ["/bin/ps", "/usr/bin/ps"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .ok_or_else(|| "ps binary not found in /bin/ps or /usr/bin/ps".into())
}

#[cfg(unix)]
fn pid_exists_by_signal(pid: &str) -> TestResult<bool> {
    let pid: libc::pid_t = pid.parse()?;
    if pid <= 0 {
        return Ok(false);
    }
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(err.into()),
    }
}

#[test]
fn keeps_session_and_captures_output() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "smoke",
            "--",
            "sh",
            "-lc",
            "echo READY; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("smoke", "READY")?;
    assert!(captured.contains("READY"));
    Ok(())
}

#[test]
fn session_identity_env_is_exported_to_child_process() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "identity-env",
            "--",
            "sh",
            "-lc",
            "printf 'SESSION:%s\\nPANE:%s\\n' \"$LTERM_SESSION\" \"$LTERM_PANE\"; sleep 2",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pane = stdout
        .lines()
        .find_map(|line| line.split('\t').nth(1))
        .ok_or_else(|| format!("detached output missing pane field: {stdout:?}"))?;

    let captured = env.capture_until("identity-env", &format!("PANE:{pane}"))?;
    assert!(captured.contains("SESSION:identity-env"), "{captured:?}");
    assert!(captured.contains(&format!("PANE:{pane}")), "{captured:?}");
    Ok(())
}

#[test]
fn codex_home_reaches_child_through_prestarted_daemon_request_env() -> TestResult {
    let env = TestEnv::new()?;
    let _daemon = env.start_daemon_with_codex_home(None, &[CLIENT_ONLY_ENV_SHOULD_NOT_FORWARD])?;
    let sentinel = env.temp.path().join("mat-session").join("CODEX_HOME");
    let client_only_secret = "client-only-secret-should-not-cross-daemon-hop";
    let probe_command = format!(
        "printf 'CODEX_HOME:%s\\nCLIENT_ONLY:%s\\n' \"${{CODEX_HOME-}}\" \"${{{CLIENT_ONLY_ENV_SHOULD_NOT_FORWARD}-}}\"; sleep 1"
    );

    let output = env
        .cmd()
        .env("CODEX_HOME", &sentinel)
        .env(CLIENT_ONLY_ENV_SHOULD_NOT_FORWARD, client_only_secret)
        .args([
            "new",
            "--detach",
            "--name",
            "codex-home-env",
            "--",
            "sh",
            "-c",
            probe_command.as_str(),
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");

    let expected = format!("CODEX_HOME:{}", sentinel.display());
    let captured = env.capture_until("codex-home-env", &expected)?;
    assert!(captured.contains(&expected), "{captured:?}");
    assert!(
        captured.contains("CLIENT_ONLY:"),
        "child output should include the client-only env probe: {captured:?}"
    );
    assert!(
        !captured.contains(client_only_secret),
        "client-only env outside the narrow CODEX_HOME allowlist must not cross the daemon hop: {captured:?}"
    );
    Ok(())
}

#[test]
fn client_codex_home_overrides_stale_daemon_codex_home() -> TestResult {
    let env = TestEnv::new()?;
    let stale = env.temp.path().join("daemon-stale").join("CODEX_HOME");
    let _daemon = env.start_daemon_with_codex_home(Some(&stale), &[])?;
    let sentinel = env.temp.path().join("mat-session").join("CODEX_HOME");

    let output = env
        .cmd()
        .env("CODEX_HOME", &sentinel)
        .args([
            "new",
            "--detach",
            "--name",
            "codex-home-stale-daemon",
            "--",
            "sh",
            "-c",
            "printf 'CODEX_HOME:%s\\n' \"${CODEX_HOME-}\"; sleep 1",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");

    let expected = format!("CODEX_HOME:{}", sentinel.display());
    let captured = env.capture_until("codex-home-stale-daemon", &expected)?;
    assert!(captured.contains(&expected), "{captured:?}");
    assert!(
        !captured.contains(&stale.display().to_string()),
        "request client CODEX_HOME should override stale daemon ambient value: {captured:?}"
    );
    Ok(())
}

#[test]
fn attach_or_new_auto_create_inherits_client_codex_home() -> TestResult {
    let env = TestEnv::new()?;
    let stale = env.temp.path().join("daemon-stale-open").join("CODEX_HOME");
    let _daemon = env.start_daemon_with_codex_home(Some(&stale), &[])?;
    let sentinel = env.temp.path().join("mat-session-open").join("CODEX_HOME");
    let target = "codex-home-attach-or-new";

    let output = wait_for_child_output(
        ChildCleanup::new(
            env.cmd()
                .env("CODEX_HOME", &sentinel)
                .args(["attach-or-new", target, "--no-status"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        ),
        Duration::from_secs(2),
        "attach-or-new CODEX_HOME auto-create EOF detach",
    )?;
    assert!(output.status.success(), "{output:?}");

    let input = env
        .cmd()
        .args([
            "input",
            target,
            "printf 'CODEX_HOME:%s\\n' \"${CODEX_HOME-}\"",
            "--enter",
        ])
        .output()?;
    assert!(input.status.success(), "{input:?}");

    let expected = format!("CODEX_HOME:{}", sentinel.display());
    let captured = env.capture_until(target, &expected)?;
    assert!(captured.contains(&expected), "{captured:?}");
    assert!(
        !captured.contains(&stale.display().to_string()),
        "attach-or-new auto-create should use request client CODEX_HOME, not daemon ambient value: {captured:?}"
    );
    Ok(())
}

#[test]
fn codex_home_reaches_fake_omx_launcher_through_prestarted_daemon() -> TestResult {
    let env = TestEnv::new()?;
    let _daemon = env.start_daemon_without_codex_home()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(
        &fake_bin.join("omx"),
        "#!/bin/sh\nprintf 'CODEX_HOME:%s\\n' \"${CODEX_HOME-}\"\n",
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let sentinel = env.temp.path().join("mat-session").join("CODEX_HOME");

    let output = env
        .cmd()
        .env("PATH", path)
        .env("CODEX_HOME", &sentinel)
        .stdin(Stdio::null())
        .args(["omx", "--raw", "--no-status", "--", "--probe"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!("CODEX_HOME:{}", sentinel.display());
    assert!(stdout.contains(&expected), "{stdout:?}");
    Ok(())
}

#[test]
fn capture_alias_captures_output() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "capture-alias",
            "--",
            "sh",
            "-lc",
            "echo CAPTURE_ALIAS_READY; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    let captured = env.capture_command_until("capture", "capture-alias", "CAPTURE_ALIAS_READY")?;
    assert!(captured.contains("CAPTURE_ALIAS_READY"));
    Ok(())
}

#[test]
fn wait_exit_json_reports_session_exit() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "wait-exit",
            "--",
            "sh",
            "-lc",
            "sleep 0.2; exit 7",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "wait-exit");

    let output = env
        .cmd()
        .args(["wait", "wait-exit", "--exit", "--timeout", "5s", "--json"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["event"], "exit");
    assert_eq!(result["matched"], true);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["exit_code"], 7);
    assert_eq!(result["session"]["name"], "wait-exit");
    Ok(())
}

#[test]
fn wait_contains_succeeds_against_sanitized_capture_text() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "wait-contains",
            "--",
            "sh",
            "-lc",
            "printf '\\033]52;c;secret\\007'; echo AGENT_READY; sleep 30",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "wait-contains");

    let output = env
        .cmd()
        .args([
            "wait",
            "wait-contains",
            "--contains",
            "AGENT_READY",
            "--timeout",
            "5s",
            "--tail",
            "20",
            "--json",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("secret"), "{stdout}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["event"], "contains");
    assert_eq!(result["matched"], true);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["needle"], "AGENT_READY");

    let stripped_output = env
        .cmd()
        .args([
            "wait",
            "wait-contains",
            "--contains",
            "secret",
            "--timeout",
            "200ms",
            "--json",
        ])
        .output()?;
    assert_eq!(
        stripped_output.status.code(),
        Some(124),
        "{stripped_output:?}"
    );
    let stripped: serde_json::Value = serde_json::from_slice(&stripped_output.stdout)?;
    assert_eq!(stripped["event"], "contains");
    assert_eq!(stripped["matched"], false);
    assert_eq!(stripped["timed_out"], true);
    assert_eq!(stripped["needle"], "secret");
    Ok(())
}

#[test]
fn wait_contains_tail_limits_sanitized_scrollback_scan() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "wait-tail",
            "--",
            "sh",
            "-lc",
            "printf 'OLD_MARKER\\nNEW_MARKER'; sleep 30",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "wait-tail");
    env.capture_until("wait-tail", "NEW_MARKER")?;

    let old_output = env
        .cmd()
        .args([
            "wait",
            "wait-tail",
            "--contains",
            "OLD_MARKER",
            "--tail",
            "1",
            "--timeout",
            "200ms",
            "--json",
        ])
        .output()?;
    assert_eq!(old_output.status.code(), Some(124), "{old_output:?}");
    let old_result: serde_json::Value = serde_json::from_slice(&old_output.stdout)?;
    assert_eq!(old_result["event"], "contains");
    assert_eq!(old_result["matched"], false);
    assert_eq!(old_result["timed_out"], true);

    let new_output = env
        .cmd()
        .args([
            "wait",
            "wait-tail",
            "--contains",
            "NEW_MARKER",
            "--tail",
            "1",
            "--timeout",
            "5s",
            "--json",
        ])
        .output()?;
    assert!(new_output.status.success(), "{new_output:?}");
    let new_result: serde_json::Value = serde_json::from_slice(&new_output.stdout)?;
    assert_eq!(new_result["event"], "contains");
    assert_eq!(new_result["matched"], true);
    assert_eq!(new_result["timed_out"], false);
    Ok(())
}

#[test]
fn wait_contains_matches_fast_exit_session() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "wait-fast-exit",
            "--",
            "sh",
            "-lc",
            "sleep 0.2; echo FAST_READY",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "wait-fast-exit");

    let output = env
        .cmd()
        .args([
            "wait",
            "wait-fast-exit",
            "--contains",
            "FAST_READY",
            "--timeout",
            "5s",
            "--json",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["event"], "contains");
    assert_eq!(result["matched"], true);
    assert_eq!(result["timed_out"], false);
    assert!(result["exited"].is_boolean(), "{result:?}");
    assert_eq!(result["needle"], "FAST_READY");
    Ok(())
}

#[test]
fn wait_contains_timeout_returns_nonzero_and_json_timeout_status() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "wait-timeout",
            "--",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "wait-timeout");

    let output = env
        .cmd()
        .args([
            "wait",
            "wait-timeout",
            "--contains",
            "NEVER_READY",
            "--timeout",
            "200ms",
            "--json",
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(124), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["event"], "contains");
    assert_eq!(result["matched"], false);
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["needle"], "NEVER_READY");
    Ok(())
}

#[test]
fn wait_contains_rejects_oversized_needles() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "wait-needle-cap",
            "--",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "wait-needle-cap");

    let oversized = "x".repeat(4097);
    let output = env
        .cmd()
        .args([
            "wait",
            "wait-needle-cap",
            "--contains",
            &oversized,
            "--timeout",
            "5s",
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "oversized wait needle should fail before waiting: {output:?}"
    );
    assert_stderr_contains(&output, "wait contains text exceeds 4096 bytes");
    Ok(())
}

#[test]
fn watch_exit_notify_invokes_cmux_with_sanitized_message() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\necho SHOULD_NOT_POLLUTE_JSON_STDOUT\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let mut paths = vec![fake_bin];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    let path = std::env::join_paths(paths)?;

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "watch-notify",
            "--",
            "sh",
            "-lc",
            "sleep 0.2; exit 0",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "watch-notify");

    let mut watch = env.cmd();
    watch.env("PATH", path);
    let output = watch
        .args([
            "watch",
            "watch-notify",
            "--exit",
            "--timeout",
            "5s",
            "--notify",
            "--json",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("SHOULD_NOT_POLLUTE_JSON_STDOUT"),
        "{stdout}"
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["event"], "exit");
    assert_eq!(result["matched"], true);

    let cmux = wait_for_file_contents(&cmux_log)?;
    assert!(cmux.contains("notify"), "{cmux:?}");
    assert!(cmux.contains("--title"), "{cmux:?}");
    assert!(cmux.contains("lterm watch matched"), "{cmux:?}");
    assert!(cmux.contains("--body"), "{cmux:?}");
    assert!(
        cmux.contains("session watch-notify exited with status 0"),
        "{cmux:?}"
    );
    Ok(())
}

#[test]
fn watch_exit_notify_runs_after_observed_session_exit() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-exit-order.log");
    let exit_marker = env.temp.path().join("watch-exit-marker");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {}\n\
             if [ -f {} ]; then\n\
               printf '%s\\n' MARKER_PRESENT >> {}\n\
             else\n\
               printf '%s\\n' EARLY_NOTIFY >> {}\n\
               exit 70\n\
             fi\n",
            shlex::try_quote(&cmux_log.display().to_string())?,
            shlex::try_quote(&exit_marker.display().to_string())?,
            shlex::try_quote(&cmux_log.display().to_string())?,
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let marker_path = exit_marker.display().to_string();
    let marker = shlex::try_quote(&marker_path)?;
    let payload = format!("sleep 0.2; printf done > {marker}; exit 0");

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "watch-exit-order",
            "--",
            "sh",
            "-lc",
            payload.as_str(),
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "watch-exit-order");

    let output = env
        .cmd()
        .env("PATH", path)
        .args([
            "watch",
            "watch-exit-order",
            "--exit",
            "--timeout",
            "5s",
            "--notify",
            "--json",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["event"], "exit");
    assert_eq!(result["matched"], true);

    let cmux = wait_for_file_contents(&cmux_log)?;
    assert!(cmux.contains("MARKER_PRESENT"), "{cmux:?}");
    assert!(
        !cmux.contains("EARLY_NOTIFY"),
        "cmux notifications must not fire before the watched session exits: {cmux:?}"
    );
    Ok(())
}

#[test]
fn watch_contains_notify_invokes_cmux_with_sanitized_message() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-contains.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\necho SHOULD_NOT_POLLUTE_JSON_STDOUT\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let mut paths = vec![fake_bin];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    let path = std::env::join_paths(paths)?;

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "watch-contains-notify",
            "--",
            "sh",
            "-lc",
            "echo 'WATCH;MATCH'; sleep 30",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "watch-contains-notify");

    let mut watch = env.cmd();
    watch.env("PATH", path);
    let output = watch
        .args([
            "watch",
            "watch-contains-notify",
            "--contains",
            "WATCH;MATCH",
            "--timeout",
            "5s",
            "--notify",
            "--json",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("SHOULD_NOT_POLLUTE_JSON_STDOUT"),
        "{stdout}"
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["event"], "contains");
    assert_eq!(result["matched"], true);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["needle"], "WATCH;MATCH");

    let cmux = wait_for_file_contents(&cmux_log)?;
    assert!(cmux.contains("notify"), "{cmux:?}");
    assert!(cmux.contains("--title"), "{cmux:?}");
    assert!(cmux.contains("lterm watch matched"), "{cmux:?}");
    assert!(cmux.contains("--body"), "{cmux:?}");
    assert!(
        cmux.contains("session watch-contains-notify output matched WATCH MATCH"),
        "{cmux:?}"
    );
    assert!(!cmux.contains("WATCH;MATCH"), "{cmux:?}");
    Ok(())
}

#[test]
fn notify_targets_cmux_context_when_available() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-context.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let mut notify = env.cmd();
    notify
        .env("PATH", path)
        .env("CMUX_WORKSPACE_ID", "workspace:2")
        .env("CMUX_WINDOW_ID", "window:1")
        .env("CMUX_SURFACE_ID", "surface:7")
        .env("CMUX_PANE_ID", "pane:ignored");
    let output = notify
        .args([
            "notify",
            "--title",
            "context-test",
            "--body",
            "context-body",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");

    let cmux = wait_for_file_contents(&cmux_log)?;
    assert!(cmux.contains("notify"), "{cmux:?}");
    assert!(cmux.contains("--workspace"), "{cmux:?}");
    assert!(cmux.contains("workspace:2"), "{cmux:?}");
    assert!(cmux.contains("--window"), "{cmux:?}");
    assert!(cmux.contains("window:1"), "{cmux:?}");
    assert!(cmux.contains("--surface"), "{cmux:?}");
    assert!(cmux.contains("surface:7"), "{cmux:?}");
    assert!(!cmux.contains("pane:ignored"), "{cmux:?}");
    Ok(())
}

#[test]
fn notify_ignores_unsafe_cmux_context_refs() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-context-filter.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", path)
        .env("CMUX_WORKSPACE_ID", "-workspace:bad")
        .env("CMUX_WINDOW_ID", "window:1")
        .env("CMUX_SURFACE_ID", "surface:bad/value")
        .args([
            "notify",
            "--title",
            "context-filter-test",
            "--body",
            "context-filter-body",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");

    let cmux = wait_for_file_contents(&cmux_log)?;
    assert!(cmux.contains("notify"), "{cmux:?}");
    assert!(cmux.contains("--window"), "{cmux:?}");
    assert!(cmux.contains("window:1"), "{cmux:?}");
    assert!(!cmux.contains("--workspace"), "{cmux:?}");
    assert!(!cmux.contains("-workspace:bad"), "{cmux:?}");
    assert!(!cmux.contains("--surface"), "{cmux:?}");
    assert!(!cmux.contains("surface:bad/value"), "{cmux:?}");
    Ok(())
}

#[test]
fn watch_json_notify_without_cmux_keeps_stdout_machine_readable() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "watch-json-no-cmux",
            "--",
            "sh",
            "-lc",
            "sleep 0.2; exit 0",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "watch-json-no-cmux");

    let no_cmux_path = env.temp.path().join("no-cmux-bin");
    std::fs::create_dir(&no_cmux_path)?;
    let mut watch = env.cmd();
    watch.env("PATH", &no_cmux_path);
    let output = watch
        .args([
            "watch",
            "watch-json-no-cmux",
            "--exit",
            "--timeout",
            "5s",
            "--notify",
            "--json",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["event"], "exit");
    assert_eq!(result["matched"], true);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("\u{1b}]777;notify;"),
        "OSC fallback should move to stderr when stdout is JSON: {stderr:?}"
    );
    Ok(())
}

#[test]
fn notify_falls_back_when_cmux_notify_hangs() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(&fake_bin.join("cmux"), "#!/bin/sh\n/bin/sleep 10\nexit 0\n")?;

    let started = Instant::now();
    let mut notify = env.cmd();
    notify.env("PATH", &fake_bin);
    let output = notify
        .args([
            "notify",
            "--title",
            "Task complete",
            "--body",
            "All checks passed",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "notify should not wait for a hung cmux helper: elapsed={:?}; output={output:?}",
        started.elapsed()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\u{1b}]777;notify;Task complete;All checks passed\u{7}"),
        "hung cmux should fall back to OSC 777 on stdout: {stdout:?}"
    );
    Ok(())
}

#[test]
fn logs_supports_inclusive_end_range() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "logs-end",
            "--",
            "sh",
            "-lc",
            "printf 'FIRST_LINE\\nSECOND_LINE\\nTHIRD_LINE\\n'; sleep 2",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("logs-end", "THIRD_LINE")?;

    let output = env
        .cmd()
        .args(["logs", "logs-end", "-S0", "--end", "0"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FIRST_LINE"), "{stdout:?}");
    assert!(
        !stdout.contains("SECOND_LINE"),
        "logs --end should stop at the inclusive end line: {stdout:?}"
    );
    assert!(
        !stdout.contains("THIRD_LINE"),
        "logs --end should stop at the inclusive end line: {stdout:?}"
    );

    let tail_output = env
        .cmd()
        .args(["logs", "logs-end", "-S-2", "--end", "-1"])
        .output()?;
    assert!(tail_output.status.success(), "{tail_output:?}");
    let tail_stdout = String::from_utf8_lossy(&tail_output.stdout);
    assert!(
        !tail_stdout.contains("FIRST_LINE"),
        "negative logs --end should keep the bounded tail range: {tail_stdout:?}"
    );
    assert!(tail_stdout.contains("SECOND_LINE"), "{tail_stdout:?}");
    assert!(tail_stdout.contains("THIRD_LINE"), "{tail_stdout:?}");
    Ok(())
}

#[test]
fn doctor_reports_daemon_version_and_paths() -> TestResult {
    let env = TestEnv::new()?;
    let initial = env.cmd().args(["doctor", "--json"]).output()?;
    assert!(initial.status.success(), "{initial:?}");
    let initial_report: serde_json::Value = serde_json::from_slice(&initial.stdout)?;
    assert_eq!(
        initial_report
            .get("client_version")
            .and_then(|v| v.as_str()),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        initial_report
            .get("daemon_reachable")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert!(
        initial_report
            .get("socket_path")
            .and_then(|v| v.as_str())
            .is_some_and(|path| path.contains("lterm.sock")),
        "{initial_report:?}"
    );
    let initial_compat = initial_report
        .get("tmux_compat")
        .and_then(|v| v.as_object())
        .ok_or("doctor JSON must include tmux_compat object")?;
    let supported = initial_compat
        .get("supported_command_count")
        .and_then(|v| v.as_u64())
        .ok_or("tmux_compat.supported_command_count must be present")?;
    let full = initial_compat
        .get("full_command_count")
        .and_then(|v| v.as_u64())
        .ok_or("tmux_compat.full_command_count must be present")?;
    let partial = initial_compat
        .get("partial_command_count")
        .and_then(|v| v.as_u64())
        .ok_or("tmux_compat.partial_command_count must be present")?;
    let noop = initial_compat
        .get("noop_command_count")
        .and_then(|v| v.as_u64())
        .ok_or("tmux_compat.noop_command_count must be present")?;
    assert_eq!(supported, full + partial + noop, "{initial_report:?}");
    assert!(supported > 0, "{initial_report:?}");

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "doctor-session",
            "--",
            "sh",
            "-lc",
            "echo DOCTOR_READY; sleep 2",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("doctor-session", "DOCTOR_READY")?;

    let live = env.cmd().args(["status", "--json"]).output()?;
    assert!(live.status.success(), "{live:?}");
    let report: serde_json::Value = serde_json::from_slice(&live.stdout)?;
    assert_eq!(
        report.get("daemon_reachable").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        report.get("daemon_version").and_then(|v| v.as_str()),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        report.get("version_match").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        report
            .get("daemon_session_count")
            .and_then(|v| v.as_u64())
            .is_some_and(|count| count >= 1),
        "{report:?}"
    );
    let text = env.cmd().args(["doctor"]).output()?;
    assert!(text.status.success(), "{text:?}");
    let text_stdout = String::from_utf8_lossy(&text.stdout);
    assert!(
        text_stdout.contains("tmux_compat_supported_command_count\t"),
        "{text_stdout}"
    );
    assert!(
        text_stdout.contains("tmux_compat_lterm_shim_shadowed_by_real_tmux\t"),
        "{text_stdout}"
    );
    Ok(())
}

#[test]
fn new_attaches_by_default() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args([
            "new",
            "-n",
            "attached",
            "--",
            "sh",
            "-lc",
            "echo ATTACHED_BY_DEFAULT; sleep 0.2; echo STILL_ATTACHED",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ATTACHED_BY_DEFAULT"), "{output:?}");
    assert!(stdout.contains("STILL_ATTACHED"), "{output:?}");
    Ok(())
}

#[test]
fn explicit_attach_detaches_on_stdin_eof() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "eof-attach",
            "--",
            "sh",
            "-lc",
            "sleep 5",
        ])
        .status()?;
    assert!(status.success());

    let mut attach = env
        .cmd()
        .args(["attach", "eof-attach", "--no-status"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(status) = attach.try_wait()? {
            assert!(status.success(), "{status:?}");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = attach.kill();
    let _ = attach.wait();
    Err("explicit attach did not detach after stdin EOF".into())
}

#[test]
fn attach_short_aliases_detach_on_stdin_eof() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "alias-attach",
            "--",
            "sh",
            "-lc",
            "sleep 5",
        ])
        .status()?;
    assert!(status.success());

    for alias in ["a", "-a", "resume"] {
        let started = Instant::now();
        let output = wait_for_child_output(
            ChildCleanup::new(
                env.cmd()
                    .args([alias, "alias-attach", "--no-status"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?,
            ),
            Duration::from_secs(2),
            &format!("{alias} attach EOF detach"),
        )?;
        assert!(output.status.success(), "{alias}: {output:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{alias} did not detach promptly after stdin EOF: {output:?}"
        );
    }
    Ok(())
}

#[test]
fn help_shows_common_aliases() -> TestResult {
    let env = TestEnv::new()?;
    let output = env.cmd().arg("--help").output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[aliases: attach, a]"),
        "resume compatibility aliases were not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: attach-or-new]"),
        "open compatibility alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: new]"),
        "new compatibility alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: list, ls]"),
        "sessions compatibility aliases were not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: ps]"),
        "ps compatibility alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: kill]"),
        "kill compatibility alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: send]"),
        "send compatibility alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: capture]"),
        "capture compatibility alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: record]"),
        "record compatibility alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: replay-trace]"),
        "trace replay compatibility alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: mobile]"),
        "mobile compose alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("[aliases: theme]"),
        "theme compatibility alias was not visible in help:\n{stdout}"
    );
    assert!(
        stdout.contains("Compatibility: lterm -a <target> is equivalent to lterm resume <target>."),
        "legacy -a shortcut was not discoverable in top-level help:\n{stdout}"
    );
    Ok(())
}

#[test]
fn help_exposes_utility_command_surface() -> TestResult {
    let env = TestEnv::new()?;
    let output = env.cmd().arg("--help").output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let normalized = normalize_help(&stdout);
    let mut exposed = BTreeSet::new();
    let mut in_commands = false;
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed == "Commands:" {
            in_commands = true;
            continue;
        }
        if trimmed.starts_with("Options") {
            break;
        }
        if !in_commands {
            continue;
        }
        if let Some(command) = trimmed.split_whitespace().next() {
            if command.starts_with('-') {
                continue;
            }
            exposed.insert(command);
        }
    }
    for command in [
        "install-shim",
        "env",
        "completions",
        "install-completions",
        "install-ai-statusline",
        "diagnose",
        "trace",
        "trace-replay",
        "trace-info",
        "tmux-compat",
        "wait",
        "watch",
        "init",
        "notify",
        "agents",
        "agent",
        "agy",
        "aider",
        "amp",
        "omx",
        "omc",
        "claude",
        "codex",
        "copilot",
        "crush",
        "cursor-agent",
        "gemini",
        "goose",
        "jules",
        "kiro",
        "kimi",
        "opencode",
        "qwen",
        "ssh",
    ] {
        assert!(
            exposed.contains(command),
            "top-level help should expose utility command {command:?}:\n{stdout}"
        );
    }
    for expected in [
        "tmux compatibility",
        "setup preview",
        "statusline integrations",
        "sanitized output",
        "cmux-friendly notification",
        "remote host",
    ] {
        assert!(
            normalized.contains(expected),
            "top-level help should keep utility command context {expected:?}:\n{stdout}"
        );
    }
    Ok(())
}

#[test]
fn completions_generate_shell_scripts_without_starting_daemon() -> TestResult {
    let env = TestEnv::new()?;

    let bash = env.cmd().args(["completions", "bash"]).output()?;
    assert!(bash.status.success(), "{bash:?}");
    assert!(bash.stderr.is_empty(), "{bash:?}");
    let bash_stdout = String::from_utf8_lossy(&bash.stdout);
    assert!(
        bash_stdout.contains("_lterm") && bash_stdout.contains("completions"),
        "bash completion should describe lterm commands:\n{bash_stdout}"
    );

    let zsh = env.cmd().args(["completions", "zsh"]).output()?;
    assert!(zsh.status.success(), "{zsh:?}");
    assert!(zsh.stderr.is_empty(), "{zsh:?}");
    let zsh_stdout = String::from_utf8_lossy(&zsh.stdout);
    assert!(
        zsh_stdout.contains("#compdef lterm"),
        "zsh completion should include compdef header:\n{zsh_stdout}"
    );

    let fish = env.cmd().args(["completions", "fish"]).output()?;
    assert!(fish.status.success(), "{fish:?}");
    assert!(fish.stderr.is_empty(), "{fish:?}");
    let fish_stdout = String::from_utf8_lossy(&fish.stdout);
    assert!(
        fish_stdout.contains("complete -c lterm"),
        "fish completion should include fish complete directives:\n{fish_stdout}"
    );

    let doctor = env.cmd().args(["doctor", "--json"]).output()?;
    assert!(doctor.status.success(), "{doctor:?}");
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout)?;
    assert_eq!(
        report.get("daemon_reachable").and_then(|v| v.as_bool()),
        Some(false),
        "completion generation must not auto-start the daemon: {report:?}"
    );
    Ok(())
}

#[test]
fn install_completions_writes_user_files_without_starting_daemon() -> TestResult {
    let env = TestEnv::new()?;
    let home = env.temp.path().join("home");
    let xdg_config = env.temp.path().join("xdg-config");
    let xdg_data = env.temp.path().join("xdg-data");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&xdg_config)?;
    std::fs::create_dir_all(&xdg_data)?;

    let zsh = env
        .cmd()
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("SHELL", "/bin/zsh")
        .arg("install-completions")
        .output()?;
    assert!(zsh.status.success(), "{zsh:?}");
    assert!(zsh.stderr.is_empty(), "{zsh:?}");
    let zsh_stdout = String::from_utf8_lossy(&zsh.stdout);
    let zsh_file = home.join(".zfunc/_lterm");
    assert!(zsh_file.is_file(), "missing zsh completion file");
    let zsh_script = std::fs::read_to_string(&zsh_file)?;
    assert!(zsh_script.contains("#compdef lterm"), "{zsh_script}");
    assert!(zsh_stdout.contains("shell\tzsh"), "{zsh_stdout}");
    assert!(
        zsh_stdout.contains("fpath=(") && zsh_stdout.contains("compinit"),
        "zsh install output should include activation hint:\n{zsh_stdout}"
    );

    let bash = env
        .cmd()
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &xdg_data)
        .args(["install-completions", "--shell", "bash"])
        .output()?;
    assert!(bash.status.success(), "{bash:?}");
    let bash_file = xdg_data.join("bash-completion/completions/lterm");
    assert!(bash_file.is_file(), "missing bash completion file");
    let bash_script = std::fs::read_to_string(&bash_file)?;
    assert!(bash_script.contains("complete -F _lterm"), "{bash_script}");

    let fish = env
        .cmd()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .args(["install-completions", "--shell", "fish"])
        .output()?;
    assert!(fish.status.success(), "{fish:?}");
    let fish_file = xdg_config.join("fish/completions/lterm.fish");
    assert!(fish_file.is_file(), "missing fish completion file");
    let fish_script = std::fs::read_to_string(&fish_file)?;
    assert!(fish_script.contains("complete -c lterm"), "{fish_script}");

    let doctor = env.cmd().args(["doctor", "--json"]).output()?;
    assert!(doctor.status.success(), "{doctor:?}");
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout)?;
    assert_eq!(
        report.get("daemon_reachable").and_then(|v| v.as_bool()),
        Some(false),
        "completion installation must not auto-start the daemon: {report:?}"
    );
    Ok(())
}

#[test]
fn install_ai_statusline_writes_claude_wrapper_and_settings() -> TestResult {
    let env = TestEnv::new()?;
    let home = env.temp.path().join("home");
    let claude = home.join(".claude");
    let hud = claude.join("hud");
    let codex = home.join(".codex");
    std::fs::create_dir_all(&hud)?;
    std::fs::create_dir_all(&codex)?;
    std::fs::write(
        claude.join("settings.json"),
        r#"{
  "statusLine": {
    "type": "command",
    "command": "node $HOME/.claude/hud/omc-hud.mjs",
    "padding": "keep"
  },
  "theme": "dark"
}
"#,
    )?;
    let original_codex_config = r#"[tui]
status_line = ["model-with-reasoning", "git-branch"]

[notice]
hide_rate_limit_model_nudge = true
"#;
    std::fs::write(codex.join("config.toml"), original_codex_config)?;

    let output = env
        .cmd()
        .env("HOME", &home)
        .arg("install-ai-statusline")
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("provider\tclaude\tinstalled"),
        "Claude statusline should be installed:\n{stdout}"
    );
    assert!(
        stdout.contains("provider\tcodex\tskipped"),
        "Codex statusline should be reported as unsupported:\n{stdout}"
    );
    assert!(
        stdout.contains("Codex status_line cannot render custom LTERM_SESSION/LTERM_PANE"),
        "Codex custom LTERM_SESSION limitation should be explicit:\n{stdout}"
    );

    let wrapper = hud.join("lterm-omc-hud.mjs");
    assert!(wrapper.is_file(), "missing lterm Claude HUD wrapper");
    let wrapper_script = std::fs::read_to_string(&wrapper)?;
    assert!(
        wrapper_script.contains("LTERM_SESSION") && wrapper_script.contains("omc-hud.mjs"),
        "wrapper should prepend lterm env and delegate to OMC HUD:\n{wrapper_script}"
    );
    #[cfg(unix)]
    assert_ne!(
        std::fs::metadata(&wrapper)?.permissions().mode() & 0o111,
        0,
        "wrapper should be executable"
    );

    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(claude.join("settings.json"))?)?;
    assert_eq!(
        settings
            .get("statusLine")
            .and_then(|status_line| status_line.get("command"))
            .and_then(serde_json::Value::as_str),
        Some("node $HOME/.claude/hud/lterm-omc-hud.mjs"),
        "Claude settings should point to the lterm wrapper: {settings:?}"
    );
    assert_eq!(
        settings.get("theme").and_then(serde_json::Value::as_str),
        Some("dark"),
        "unrelated Claude settings should be preserved: {settings:?}"
    );
    assert_eq!(
        settings
            .get("statusLine")
            .and_then(|status_line| status_line.get("padding"))
            .and_then(serde_json::Value::as_str),
        Some("keep"),
        "unrelated Claude statusLine settings should be preserved: {settings:?}"
    );
    let backups: Vec<_> = std::fs::read_dir(&claude)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("settings.json.bak.lterm-statusline."))
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "expected one settings backup: {backups:?}"
    );
    let codex_config = std::fs::read_to_string(codex.join("config.toml"))?;
    assert_eq!(
        codex_config, original_codex_config,
        "Codex config should not be mutated for unsupported custom lterm statusline items"
    );
    let codex_backups: Vec<_> = std::fs::read_dir(&codex)?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("config.toml.bak.lterm-statusline."))
        .collect();
    assert_eq!(
        codex_backups.len(),
        0,
        "unsupported Codex integration should not create backups: {codex_backups:?}"
    );

    let again = env
        .cmd()
        .env("HOME", &home)
        .arg("install-ai-statusline")
        .output()?;
    assert!(again.status.success(), "{again:?}");
    let again_stdout = String::from_utf8_lossy(&again.stdout);
    assert!(
        again_stdout.contains("provider\tclaude\talready-installed"),
        "second install should be idempotent:\n{again_stdout}"
    );
    assert!(
        again_stdout.contains("provider\tcodex\tskipped"),
        "second install should keep reporting Codex as unsupported:\n{again_stdout}"
    );
    Ok(())
}

#[test]
fn install_ai_statusline_does_not_overwrite_custom_claude_statusline() -> TestResult {
    let env = TestEnv::new()?;
    let home = env.temp.path().join("home");
    let claude = home.join(".claude");
    std::fs::create_dir_all(&claude)?;
    let settings_path = claude.join("settings.json");
    std::fs::write(
        &settings_path,
        r#"{
  "statusLine": {
    "type": "command",
    "command": "node $HOME/bin/my-statusline.mjs"
  }
}
"#,
    )?;
    let before = std::fs::read_to_string(&settings_path)?;

    let output = env
        .cmd()
        .env("HOME", &home)
        .arg("install-ai-statusline")
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("provider\tclaude\tskipped"),
        "custom Claude statusline should be skipped:\n{stdout}"
    );
    assert_eq!(
        before,
        std::fs::read_to_string(&settings_path)?,
        "custom Claude settings must not be overwritten"
    );
    assert!(
        !claude.join("hud/lterm-omc-hud.mjs").exists(),
        "skipped custom statusline should not create unused wrapper files"
    );
    Ok(())
}

#[test]
fn install_ai_statusline_does_not_substring_match_omc_like_commands() -> TestResult {
    let env = TestEnv::new()?;
    let home = env.temp.path().join("home");
    let claude = home.join(".claude");
    std::fs::create_dir_all(&claude)?;
    let settings_path = claude.join("settings.json");
    std::fs::write(
        &settings_path,
        r#"{
  "statusLine": {
    "type": "command",
    "command": "node $HOME/bin/custom-omc-hud.mjs --theme compact"
  }
}
"#,
    )?;
    let before = std::fs::read_to_string(&settings_path)?;

    let output = env
        .cmd()
        .env("HOME", &home)
        .arg("install-ai-statusline")
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("provider\tclaude\tskipped"),
        "OMC-like custom Claude statusline should be skipped, not migrated by substring:\n{stdout}"
    );
    assert_eq!(
        before,
        std::fs::read_to_string(&settings_path)?,
        "OMC-like custom Claude settings must not be overwritten"
    );
    assert!(
        !claude.join("hud/lterm-omc-hud.mjs").exists(),
        "skipped OMC-like custom statusline should not create unused wrapper files"
    );
    Ok(())
}

#[test]
fn install_ai_statusline_does_not_treat_custom_lterm_like_command_as_installed() -> TestResult {
    let env = TestEnv::new()?;
    let home = env.temp.path().join("home");
    let claude = home.join(".claude");
    std::fs::create_dir_all(&claude)?;
    let settings_path = claude.join("settings.json");
    std::fs::write(
        &settings_path,
        r#"{
  "statusLine": {
    "type": "command",
    "command": "node $HOME/bin/custom-lterm-omc-hud.mjs"
  }
}
"#,
    )?;

    let output = env
        .cmd()
        .env("HOME", &home)
        .arg("install-ai-statusline")
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("provider\tclaude\tskipped"),
        "custom lterm-like Claude statusline should be skipped, not treated as installed:\n{stdout}"
    );
    assert!(
        !stdout.contains("provider\tclaude\talready-installed"),
        "substring lterm-like command must not be accepted as already installed:\n{stdout}"
    );
    assert!(
        !claude.join("hud/lterm-omc-hud.mjs").exists(),
        "skipped lterm-like custom statusline should not create unused wrapper files"
    );
    Ok(())
}

#[test]
fn install_completions_sanitizes_error_paths() -> TestResult {
    let env = TestEnv::new()?;
    let bad_dir = env.temp.path().join("bad\u{001b}]0;owned\u{0007}");
    std::fs::write(&bad_dir, "not a directory")?;

    let output = env
        .cmd()
        .args(["install-completions", "--shell", "zsh", "--dir"])
        .arg(&bad_dir)
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains('\u{001b}') && !stderr.contains('\u{0007}'),
        "install-completions errors must be terminal-safe: {stderr:?}"
    );
    assert!(
        stderr.contains("create completion directory"),
        "stderr should preserve useful error context: {stderr:?}"
    );
    assert!(
        stderr.contains("bad"),
        "stderr should retain readable sanitized path text: {stderr:?}"
    );
    assert!(
        !stderr.contains("owned"),
        "stderr should strip OSC payload text from sanitized paths: {stderr:?}"
    );
    Ok(())
}

#[test]
fn install_completions_requires_supported_shell_when_undetected() -> TestResult {
    let env = TestEnv::new()?;
    let home = env.temp.path().join("home");
    std::fs::create_dir_all(&home)?;

    let output = env
        .cmd()
        .env("HOME", &home)
        .env("SHELL", "/bin/sh")
        .arg("install-completions")
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not detect a supported completion shell"),
        "stderr should explain unsupported shell detection: {stderr:?}"
    );
    assert!(
        !stderr.contains('\u{001b}') && !stderr.contains('\u{0007}'),
        "unsupported shell detection errors must be terminal-safe: {stderr:?}"
    );
    Ok(())
}

#[test]
fn diagnose_bundle_collects_redacted_local_state_without_starting_daemon() -> TestResult {
    let env = TestEnv::new()?;

    let cold = env.cmd().args(["diagnose", "--bundle"]).output()?;
    assert!(cold.status.success(), "{cold:?}");
    assert!(cold.stderr.is_empty(), "{cold:?}");
    let cold_bundle: serde_json::Value = serde_json::from_slice(&cold.stdout)?;
    assert_eq!(
        cold_bundle.get("schema_version").and_then(|v| v.as_str()),
        Some("1.1"),
        "recent-exit diagnostics must use the explicit v1.1 bundle schema: {cold_bundle:?}"
    );
    assert_eq!(
        cold_bundle
            .pointer("/doctor/daemon_reachable")
            .and_then(|v| v.as_bool()),
        Some(false),
        "cold diagnose should not auto-start daemon: {cold_bundle:?}"
    );
    assert_eq!(
        cold_bundle
            .pointer("/privacy/raw_pty_streams_included")
            .and_then(|v| v.as_bool()),
        Some(false),
        "diagnose bundle must not include raw PTY bytes by default: {cold_bundle:?}"
    );
    assert!(
        cold_bundle.get("sessions").is_some_and(|v| v.is_null()),
        "cold bundle should skip sessions without daemon auto-start: {cold_bundle:?}"
    );
    assert!(
        cold_bundle.get("recent_exits").is_some_and(|v| v.is_null()),
        "cold bundle should skip recent exits without daemon auto-start: {cold_bundle:?}"
    );
    assert!(
        cold_bundle
            .pointer("/privacy/notes")
            .and_then(|v| v.as_array())
            .is_some_and(|notes| notes.iter().any(|note| note
                .as_str()
                .is_some_and(|note| note.contains("recent exit summaries are raw-free")))),
        "diagnose privacy notes must describe the recent-exit allowlist: {cold_bundle:?}"
    );
    let cold_compat = cold_bundle
        .get("tmux_compat")
        .and_then(|v| v.as_object())
        .ok_or("cold diagnose bundle must include tmux_compat object")?;
    let cold_supported = cold_compat
        .get("supported_command_count")
        .and_then(|v| v.as_u64())
        .ok_or("cold tmux_compat supported count missing")?;
    assert!(
        cold_compat
            .get("commands")
            .and_then(|v| v.as_array())
            .is_some_and(|commands| commands.len() as u64 == cold_supported),
        "cold tmux_compat commands should match count: {cold_bundle:?}"
    );
    assert!(
        cold_compat
            .get("known_unsupported_common_commands")
            .and_then(|v| v.as_array())
            .is_some_and(|commands| commands.iter().any(|command| command == "join-pane")),
        "cold tmux_compat should include advisory gap hints: {cold_bundle:?}"
    );

    let secret = "DIAGNOSE_SECRET_SHOULD_NOT_LEAK_7519";
    let diagnose_command = format!("echo DIAGNOSE_READY; echo {secret}; sleep 30");
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "diagnose-session",
            "--",
            "sh",
            "-lc",
            diagnose_command.as_str(),
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("diagnose-session", secret)?;

    let live = env.cmd().args(["diagnose", "--bundle"]).output()?;
    assert!(live.status.success(), "{live:?}");
    let live_stdout = String::from_utf8_lossy(&live.stdout);
    assert!(
        !live_stdout.contains(secret),
        "diagnose bundle leaked scrollback or command args: {live_stdout}"
    );
    let live_bundle: serde_json::Value = serde_json::from_slice(&live.stdout)?;
    assert_eq!(
        live_bundle.get("schema_version").and_then(|v| v.as_str()),
        Some("1.1"),
        "live recent-exit diagnostics must retain the v1.1 bundle schema: {live_bundle:?}"
    );
    assert_eq!(
        live_bundle
            .pointer("/doctor/daemon_reachable")
            .and_then(|v| v.as_bool()),
        Some(true),
        "{live_bundle:?}"
    );
    assert_eq!(
        live_bundle
            .pointer("/privacy/raw_pty_streams_included")
            .and_then(|v| v.as_bool()),
        Some(false),
        "live diagnose bundle must not include raw PTY bytes: {live_bundle:?}"
    );
    assert_eq!(
        live_bundle
            .pointer("/privacy/session_commands_redacted")
            .and_then(|v| v.as_bool()),
        Some(true),
        "live diagnose bundle should redact session commands: {live_bundle:?}"
    );
    assert_eq!(
        live_bundle
            .pointer("/privacy/process_commands_redacted")
            .and_then(|v| v.as_bool()),
        Some(true),
        "live diagnose bundle should redact process commands: {live_bundle:?}"
    );
    let sessions = live_bundle
        .get("sessions")
        .and_then(|v| v.as_array())
        .ok_or("live diagnose bundle must include sessions array")?;
    assert!(
        sessions.iter().any(|session| session
            .get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|name| name == "diagnose-session")),
        "live diagnose bundle should include the created session: {live_bundle:?}"
    );
    assert!(
        live_bundle.get("processes").is_some_and(|v| v.is_array()),
        "live diagnose bundle should include process rows: {live_bundle:?}"
    );
    assert!(
        live_bundle
            .get("recent_exits")
            .is_some_and(|v| v.is_array()),
        "protocol-v8 diagnose bundle should include a bounded recent-exit array: {live_bundle:?}"
    );
    assert!(
        live_bundle.get("recent_exits_error").is_none(),
        "protocol-v8 recent-exit diagnostics should not report a bridge error: {live_bundle:?}"
    );
    assert!(
        live_bundle
            .pointer("/tmux_compat/supported_command_count")
            .and_then(|v| v.as_u64())
            .is_some_and(|count| count == cold_supported),
        "live diagnose bundle should include same local tmux compat count: {live_bundle:?}"
    );
    Ok(())
}

#[test]
fn inspect_json_matches_redacted_diagnostic_bundle_shape() -> TestResult {
    let env = TestEnv::new()?;

    let missing_json = env.cmd().arg("inspect").output()?;
    assert!(!missing_json.status.success(), "{missing_json:?}");

    let output = env.cmd().args(["inspect", "--json"]).output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let bundle: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        bundle
            .pointer("/doctor/daemon_reachable")
            .and_then(|v| v.as_bool()),
        Some(false),
        "cold inspect should not auto-start daemon: {bundle:?}"
    );
    assert_eq!(
        bundle
            .pointer("/privacy/raw_pty_streams_included")
            .and_then(|v| v.as_bool()),
        Some(false),
        "inspect bundle must not include raw PTY bytes: {bundle:?}"
    );
    assert!(
        bundle
            .pointer("/privacy/notes")
            .and_then(|v| v.as_array())
            .is_some_and(|notes| notes.iter().any(|note| note
                .as_str()
                .is_some_and(|note| note.contains("no real tmux commands are executed")))),
        "inspect privacy notes should describe safe tmux compatibility measurement: {bundle:?}"
    );
    assert!(
        bundle
            .pointer("/tmux_compat/supported_command_count")
            .and_then(|v| v.as_u64())
            .is_some_and(|count| count > 0),
        "inspect bundle should include tmux compatibility summary: {bundle:?}"
    );
    assert!(
        bundle
            .pointer("/tmux_compat/commands")
            .and_then(|v| v.as_array())
            .is_some_and(|commands| commands.iter().any(|command| command
                .get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| name == "list-commands"))),
        "inspect bundle should include local tmux command coverage data: {bundle:?}"
    );
    Ok(())
}

#[test]
fn trace_records_jsonl_and_supports_info_and_replay_surfaces() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "trace-session",
            "--",
            "sh",
            "-lc",
            "echo TRACE_READY; while IFS= read -r line; do echo TRACE_LIVE:$line; done",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("trace-session", "TRACE_READY")?;

    let trace_path = env.temp.path().join("trace.jsonl");
    let trace_path_str = trace_path.to_str().ok_or("non-utf8 trace path")?;
    let trace_child = ChildCleanup::new(
        env.cmd()
            .args([
                "trace",
                "trace-session",
                "--duration",
                "3s",
                "--max-bytes",
                "4096",
                "--output",
                trace_path_str,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    wait_for_trace_start_event(&trace_path)?;
    let send = env
        .cmd()
        .args(["input", "trace-session", "TRACE_DURING", "--enter"])
        .output()?;
    assert!(send.status.success(), "{send:?}");
    let output = wait_for_child_output(trace_child, Duration::from_secs(5), "trace command")?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "trace command should write only the file, not replay raw PTY stdout: {output:?}"
    );

    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&trace_path)?.permissions().mode() & 0o777,
        0o600,
        "trace files contain raw PTY bytes and must be owner-only"
    );

    let trace = std::fs::read_to_string(&trace_path)?;
    let events: Vec<serde_json::Value> = trace
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        events
            .first()
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str()),
        Some("start"),
        "trace must start with metadata event: {trace}"
    );
    assert_eq!(
        events
            .last()
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str()),
        Some("end"),
        "trace must end with terminal event: {trace}"
    );
    assert_eq!(
        events
            .first()
            .and_then(|v| v.get("max_bytes"))
            .and_then(|v| v.as_u64()),
        Some(4096),
        "trace start event should record the active byte cap: {trace}"
    );
    assert_eq!(
        events
            .first()
            .and_then(|v| v.get("format"))
            .and_then(|v| v.as_str()),
        Some("lterm-trace-jsonl"),
        "trace start event should record the trace file format: {trace}"
    );
    assert_eq!(
        events
            .first()
            .and_then(|v| v.get("producer"))
            .and_then(|v| v.as_str()),
        Some("lterm"),
        "trace start event should record the producer: {trace}"
    );
    assert!(
        events
            .first()
            .and_then(|v| v.get("trace_id"))
            .and_then(|v| v.as_str())
            .is_some_and(|trace_id| !trace_id.is_empty()),
        "trace start event should record a trace id: {trace}"
    );
    let combined_hex = events
        .iter()
        .filter(|event| event.get("type").and_then(|v| v.as_str()) == Some("output"))
        .filter_map(|event| event.get("bytes_hex").and_then(|v| v.as_str()))
        .collect::<String>();
    let known_payload_hex = "54524143455f4c4956453a54524143455f445552494e47";
    assert!(
        combined_hex.contains(known_payload_hex),
        "trace output chunks should contain live TRACE_LIVE:TRACE_DURING bytes encoded as hex: {trace}"
    );
    let output_chunks = events
        .iter()
        .filter(|event| event.get("type").and_then(|v| v.as_str()) == Some("output"))
        .count();
    assert!(
        events
            .iter()
            .filter(|event| event.get("type").and_then(|v| v.as_str()) == Some("output"))
            .all(|event| event.get("chunk_index").and_then(|v| v.as_u64()).is_some()),
        "trace output chunks should carry chunk indexes: {trace}"
    );
    assert_eq!(
        events
            .last()
            .and_then(|v| v.get("chunks_recorded"))
            .and_then(|v| v.as_u64()),
        Some(output_chunks as u64),
        "trace end event should record chunk totals: {trace}"
    );

    let info = env
        .cmd()
        .args(["trace-info", trace_path_str, "--json"])
        .output()?;
    assert!(info.status.success(), "{info:?}");
    let info_stdout = String::from_utf8_lossy(&info.stdout);
    let summary: serde_json::Value = serde_json::from_slice(&info.stdout)?;
    let summary_object = summary
        .as_object()
        .ok_or("trace-info JSON summary must be an object")?;
    let allowed_summary_keys = [
        "path",
        "schema_version",
        "format",
        "trace_id",
        "producer",
        "client_version",
        "client_protocol_version",
        "target",
        "created_at_unix_ms",
        "duration_ms",
        "max_bytes",
        "rows",
        "cols",
        "raw_stream_policy",
        "event_count",
        "output_chunks",
        "output_bytes",
        "first_output_elapsed_ms",
        "last_output_elapsed_ms",
        "end_elapsed_ms",
        "end_reason",
        "end_bytes_recorded",
        "end_chunks_recorded",
        "unknown_events",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert!(
        summary_object
            .keys()
            .all(|key| allowed_summary_keys.contains(key.as_str())),
        "trace-info should only expose raw-free summary keys: {info_stdout}"
    );
    assert_eq!(
        summary.get("format").and_then(|v| v.as_str()),
        Some("lterm-trace-jsonl"),
        "trace-info should expose raw-free metadata: {info_stdout}"
    );
    assert_eq!(
        summary.get("target").and_then(|v| v.as_str()),
        Some("trace-session"),
        "trace-info should report the traced target: {info_stdout}"
    );
    assert_eq!(
        summary.get("output_chunks").and_then(|v| v.as_u64()),
        Some(output_chunks as u64),
        "trace-info should summarize output chunks: {info_stdout}"
    );
    assert!(
        !info_stdout.contains("TRACE_DURING")
            && !info_stdout.contains("bytes_hex")
            && !info_stdout.contains(known_payload_hex)
            && events
                .iter()
                .filter(|event| event.get("type").and_then(|v| v.as_str()) == Some("output"))
                .filter_map(|event| event.get("bytes_hex").and_then(|v| v.as_str()))
                .all(|bytes_hex| !info_stdout.contains(bytes_hex)),
        "trace-info must not print raw trace payloads or their hex encoding: {info_stdout}"
    );

    let replay = env.cmd().args(["trace-replay", trace_path_str]).output()?;
    assert!(replay.status.success(), "{replay:?}");
    let replay_stdout = String::from_utf8_lossy(&replay.stdout);
    assert!(
        replay_stdout.contains("TRACE_LIVE:TRACE_DURING"),
        "trace-replay should decode output chunks to stdout: {replay_stdout:?}"
    );
    let timed_replay = env
        .cmd()
        .args(["trace-replay", trace_path_str, "--timing"])
        .output()?;
    assert!(timed_replay.status.success(), "{timed_replay:?}");
    let timed_replay_stdout = String::from_utf8_lossy(&timed_replay.stdout);
    assert!(
        timed_replay_stdout.contains("TRACE_LIVE:TRACE_DURING"),
        "trace-replay --timing should decode output chunks to stdout: {timed_replay_stdout:?}"
    );

    let before_overwrite = std::fs::read_to_string(&trace_path)?;
    let overwrite = env
        .cmd()
        .args([
            "record",
            "trace-session",
            "--duration",
            "100ms",
            "--output",
            trace_path_str,
        ])
        .output()?;
    assert!(
        !overwrite.status.success(),
        "trace should refuse to overwrite without --force: {overwrite:?}"
    );
    assert_stderr_contains(&overwrite, "--force");
    assert_eq!(
        std::fs::read_to_string(&trace_path)?,
        before_overwrite,
        "failed overwrite attempt must leave existing trace file unchanged"
    );

    let forced = env
        .cmd()
        .args([
            "record",
            "trace-session",
            "--duration",
            "100ms",
            "--output",
            trace_path_str,
            "--force",
        ])
        .output()?;
    assert!(forced.status.success(), "{forced:?}");
    assert_ne!(
        std::fs::read_to_string(&trace_path)?,
        before_overwrite,
        "--force should replace the existing trace file"
    );
    Ok(())
}

#[test]
fn trace_replay_rejects_malformed_or_unsafe_jsonl() -> TestResult {
    let env = TestEnv::new()?;
    let start = r#"{"type":"start","schema_version":"1.0","format":"lterm-trace-jsonl","duration_ms":120000}"#;
    let end = r#"{"type":"end","elapsed_ms":0,"reason":"duration","bytes_recorded":0,"chunks_recorded":0}"#;

    let no_start_path = env.temp.path().join("no-start.jsonl");
    std::fs::write(
        &no_start_path,
        r#"{"type":"output","chunk_index":0,"elapsed_ms":0,"direction":"stdout","len":1,"bytes_hex":"41"}"#,
    )?;
    let no_start = env.cmd().arg("trace-replay").arg(&no_start_path).output()?;
    assert!(
        !no_start.status.success(),
        "trace-replay should reject output before start: {no_start:?}"
    );
    assert!(
        no_start.stdout.is_empty(),
        "trace-replay should validate malformed traces before writing raw bytes: {no_start:?}"
    );
    assert_stderr_contains(&no_start, "output before start");

    let missing_end_path = env.temp.path().join("missing-end.jsonl");
    std::fs::write(
        &missing_end_path,
        format!(
            "{start}\n{}\n",
            r#"{"type":"output","chunk_index":0,"elapsed_ms":0,"direction":"stdout","len":9,"bytes_hex":"1b5d35323b633b5807"}"#
        ),
    )?;
    let missing_end = env
        .cmd()
        .arg("trace-replay")
        .arg(&missing_end_path)
        .output()?;
    assert!(
        !missing_end.status.success(),
        "trace-replay should reject missing end events: {missing_end:?}"
    );
    assert!(
        missing_end.stdout.is_empty(),
        "trace-replay should not emit raw bytes before validating the full trace: {missing_end:?}"
    );
    assert_stderr_contains(&missing_end, "missing an end event");

    let wrong_direction_path = env.temp.path().join("wrong-direction.jsonl");
    std::fs::write(
        &wrong_direction_path,
        format!(
            "{start}\n{}\n{end}\n",
            r#"{"type":"output","chunk_index":0,"elapsed_ms":0,"direction":"stderr","len":1,"bytes_hex":"41"}"#
        ),
    )?;
    let wrong_direction = env
        .cmd()
        .arg("trace-replay")
        .arg(&wrong_direction_path)
        .output()?;
    assert!(
        !wrong_direction.status.success(),
        "trace-replay should reject unsupported directions: {wrong_direction:?}"
    );
    assert_stderr_contains(&wrong_direction, "unsupported output direction");

    let delay_cap_path = env.temp.path().join("delay-cap.jsonl");
    std::fs::write(
        &delay_cap_path,
        format!(
            "{start}\n{}\n{}\n",
            r#"{"type":"output","chunk_index":0,"elapsed_ms":61000,"direction":"stdout","len":1,"bytes_hex":"41"}"#,
            r#"{"type":"end","elapsed_ms":61000,"reason":"duration","bytes_recorded":1,"chunks_recorded":1}"#
        ),
    )?;
    let delay_cap = env
        .cmd()
        .args(["trace-replay", "--timing"])
        .arg(&delay_cap_path)
        .output()?;
    assert!(
        !delay_cap.status.success(),
        "trace-replay --timing should cap untrusted sleep delays: {delay_cap:?}"
    );
    assert!(
        delay_cap.stdout.is_empty(),
        "trace-replay should reject oversized delays before writing raw bytes: {delay_cap:?}"
    );
    assert_stderr_contains(&delay_cap, "exceeds safety cap");

    let aggregate_cap_path = env.temp.path().join("aggregate-cap.jsonl");
    let oversized_len = MAX_TRACE_REPLAY_TOTAL_BYTES + 1;
    std::fs::write(
        &aggregate_cap_path,
        format!(
            "{start}\n{}\n",
            format_args!(
                r#"{{"type":"end","elapsed_ms":0,"reason":"duration","bytes_recorded":{},"chunks_recorded":0}}"#,
                oversized_len
            )
        ),
    )?;
    let aggregate_cap = env
        .cmd()
        .arg("trace-replay")
        .arg(&aggregate_cap_path)
        .output()?;
    assert!(
        !aggregate_cap.status.success(),
        "trace-replay should reject aggregate replay byte counts above the safety cap: {aggregate_cap:?}"
    );
    assert!(
        aggregate_cap.stdout.is_empty(),
        "trace-replay should reject aggregate caps before writing raw bytes: {aggregate_cap:?}"
    );
    assert_stderr_contains(&aggregate_cap, "total bytes exceed safety cap");

    let legacy_path = env.temp.path().join("legacy-v1-trace.jsonl");
    std::fs::write(
        &legacy_path,
        concat!(
            r#"{"type":"start","schema_version":"1.0","target":"legacy","created_at_unix_ms":1,"duration_ms":1000,"max_bytes":1024,"rows":24,"cols":80,"raw_stream_policy":"raw-transparent"}"#,
            "\n",
            r#"{"type":"output","elapsed_ms":1,"direction":"stdout","len":6,"bytes_hex":"4c4547414359"}"#,
            "\n",
            r#"{"type":"end","elapsed_ms":2,"reason":"duration"}"#,
            "\n"
        ),
    )?;
    let legacy = env.cmd().arg("trace-replay").arg(&legacy_path).output()?;
    assert!(
        legacy.status.success(),
        "trace-replay should accept v1.0.6 trace files without newer metadata: {legacy:?}"
    );
    assert_eq!(legacy.stdout, b"LEGACY");

    let slightly_over_duration_path = env.temp.path().join("slightly-over-duration.jsonl");
    std::fs::write(
        &slightly_over_duration_path,
        concat!(
            r#"{"type":"start","schema_version":"1.0","format":"lterm-trace-jsonl","duration_ms":1000}"#,
            "\n",
            r#"{"type":"output","chunk_index":0,"elapsed_ms":1001,"direction":"stdout","len":1,"bytes_hex":"41"}"#,
            "\n",
            r#"{"type":"end","elapsed_ms":1001,"reason":"duration","bytes_recorded":1,"chunks_recorded":1}"#,
            "\n"
        ),
    )?;
    let slightly_over_duration = env
        .cmd()
        .arg("trace-replay")
        .arg(&slightly_over_duration_path)
        .output()?;
    assert!(
        slightly_over_duration.status.success(),
        "trace-replay should tolerate scheduler drift past duration_ms: {slightly_over_duration:?}"
    );
    assert_eq!(slightly_over_duration.stdout, b"A");

    let info_path = env.temp.path().join("info-unknown.jsonl");
    std::fs::write(
        &info_path,
        format!(
            "{start}\nnot-json\n{}\n{end}\n",
            r#"{"type":"output","chunk_index":0,"elapsed_ms":0,"direction":"stdout","len":2,"bytes_hex":"41"}"#
        ),
    )?;
    let info = env
        .cmd()
        .args(["trace-info", "--json"])
        .arg(&info_path)
        .output()?;
    assert!(
        info.status.success(),
        "trace-info should summarize around malformed non-raw lines: {info:?}"
    );
    let summary: serde_json::Value = serde_json::from_slice(&info.stdout)?;
    assert_eq!(
        summary.get("unknown_events").and_then(|v| v.as_u64()),
        Some(2),
        "trace-info should count malformed/invalid output rows as unknown: {summary:?}"
    );

    let bad_len_type_path = env.temp.path().join("info-bad-len-type.jsonl");
    std::fs::write(
        &bad_len_type_path,
        format!(
            "{start}\n{}\n{end}\n",
            r#"{"type":"output","chunk_index":0,"elapsed_ms":0,"direction":"stdout","len":"2","bytes_hex":"4142"}"#
        ),
    )?;
    let bad_len_type = env
        .cmd()
        .args(["trace-info", "--json"])
        .arg(&bad_len_type_path)
        .output()?;
    assert!(
        bad_len_type.status.success(),
        "trace-info should summarize around non-u64 len fields: {bad_len_type:?}"
    );
    let bad_len_type_summary: serde_json::Value = serde_json::from_slice(&bad_len_type.stdout)?;
    assert_eq!(
        bad_len_type_summary
            .get("unknown_events")
            .and_then(|v| v.as_u64()),
        Some(1),
        "trace-info should count non-u64 len output rows as unknown: {bad_len_type_summary:?}"
    );
    assert_eq!(
        bad_len_type_summary
            .get("output_chunks")
            .and_then(|v| v.as_u64()),
        Some(0),
        "trace-info must not count non-u64 len rows as valid output chunks: {bad_len_type_summary:?}"
    );

    let duplicate_legacy_start_path = env.temp.path().join("info-duplicate-start.jsonl");
    std::fs::write(
        &duplicate_legacy_start_path,
        concat!(
            r#"{"type":"start","schema_version":"1.0","target":"first","duration_ms":1}"#,
            "\n",
            r#"{"type":"start","schema_version":"1.0","target":"second","duration_ms":1}"#,
            "\n",
            r#"{"type":"end","elapsed_ms":0}"#,
            "\n"
        ),
    )?;
    let duplicate_legacy_start = env
        .cmd()
        .args(["trace-info", "--json"])
        .arg(&duplicate_legacy_start_path)
        .output()?;
    assert!(
        duplicate_legacy_start.status.success(),
        "trace-info should summarize duplicate legacy start events without overwrite: {duplicate_legacy_start:?}"
    );
    let duplicate_start_summary: serde_json::Value =
        serde_json::from_slice(&duplicate_legacy_start.stdout)?;
    assert_eq!(
        duplicate_start_summary
            .get("target")
            .and_then(|v| v.as_str()),
        Some("first"),
        "trace-info should keep the first start event metadata: {duplicate_start_summary:?}"
    );
    assert_eq!(
        duplicate_start_summary
            .get("unknown_events")
            .and_then(|v| v.as_u64()),
        Some(1),
        "trace-info should count duplicate start events even when legacy start has no format: {duplicate_start_summary:?}"
    );

    let duplicate_reasonless_end_path = env.temp.path().join("info-duplicate-end.jsonl");
    std::fs::write(
        &duplicate_reasonless_end_path,
        concat!(
            r#"{"type":"start","schema_version":"1.0","format":"lterm-trace-jsonl","duration_ms":1}"#,
            "\n",
            r#"{"type":"end","elapsed_ms":0}"#,
            "\n",
            r#"{"type":"end","elapsed_ms":1}"#,
            "\n"
        ),
    )?;
    let duplicate_reasonless_end = env
        .cmd()
        .args(["trace-info", "--json"])
        .arg(&duplicate_reasonless_end_path)
        .output()?;
    assert!(
        duplicate_reasonless_end.status.success(),
        "trace-info should summarize duplicate reasonless end events without overwrite: {duplicate_reasonless_end:?}"
    );
    let duplicate_end_summary: serde_json::Value =
        serde_json::from_slice(&duplicate_reasonless_end.stdout)?;
    assert_eq!(
        duplicate_end_summary
            .get("end_elapsed_ms")
            .and_then(|v| v.as_u64()),
        Some(0),
        "trace-info should keep the first end event metadata: {duplicate_end_summary:?}"
    );
    assert_eq!(
        duplicate_end_summary
            .get("unknown_events")
            .and_then(|v| v.as_u64()),
        Some(1),
        "trace-info should count duplicate end events even when end has no reason: {duplicate_end_summary:?}"
    );
    Ok(())
}

#[test]
fn init_prints_setup_preview_without_modifying_files() -> TestResult {
    let env = TestEnv::new()?;
    let home = env.temp.path().join("home");
    std::fs::create_dir_all(&home)?;
    std::fs::write(home.join(".zshrc"), "original startup file\n")?;
    let before = temp_tree_snapshot(env.temp.path())?;

    let output = env
        .cmd()
        .env("HOME", &home)
        .args(["init", "--shell", "zsh"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "lterm init preview",
        "shell\tzsh",
        "modifies_files\tno",
        "step\t1\tlterm doctor --json",
        "step\t2\tlterm install-shim",
        "step\t3\teval \"$(lterm env)\"",
        "step\t4\tlterm install-completions --shell zsh",
        "step\t5\tlterm install-ai-statusline",
        "indicator\tLTERM_SESSION/LTERM_PANE",
        "Copy the enable command",
    ] {
        assert!(
            stdout.contains(expected),
            "init preview missing {expected:?}:\n{stdout}"
        );
    }
    assert_eq!(
        before,
        temp_tree_snapshot(env.temp.path())?,
        "lterm init must not modify shell startup files or daemon state"
    );
    Ok(())
}

#[test]
fn init_prints_fish_source_preview() -> TestResult {
    let env = TestEnv::new()?;
    let output = env.cmd().args(["init", "--shell", "fish"]).output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "shell\tfish",
        "step\t2\tlterm install-shim",
        "step\t3\tlterm env --shell fish | source",
        "step\t4\tlterm install-completions --shell fish",
        "step\t5\tlterm install-ai-statusline",
    ] {
        assert!(
            stdout.contains(expected),
            "fish init preview missing {expected:?}:\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("set -gx PATH (lterm install-shim) $PATH"),
        "fish init preview must not recommend re-running install-shim from shell startup:\n{stdout}"
    );
    Ok(())
}

#[test]
fn init_detects_shell_from_environment_when_omitted() -> TestResult {
    let env = TestEnv::new()?;
    for (shell_env, expected_shell, expected_step) in [
        (
            Some("/usr/local/bin/fish"),
            "shell\tfish",
            "step\t3\tlterm env --shell fish | source",
        ),
        (
            Some("/bin/zsh"),
            "shell\tzsh",
            "step\t3\teval \"$(lterm env)\"",
        ),
        (
            Some("/opt/custom/unknown-shell"),
            "shell\tposix",
            "step\t3\teval \"$(lterm env)\"",
        ),
        (None, "shell\tposix", "step\t3\teval \"$(lterm env)\""),
    ] {
        let mut cmd = env.cmd();
        if let Some(shell_env) = shell_env {
            cmd.env("SHELL", shell_env);
        } else {
            cmd.env_remove("SHELL");
        }
        let output = cmd.arg("init").output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(expected_shell) && stdout.contains(expected_step),
            "init preview for SHELL={shell_env:?} missing {expected_shell:?}/{expected_step:?}:\n{stdout}"
        );
    }
    Ok(())
}

#[test]
fn init_mobile_reconnect_preview_is_no_touch_and_reversible_for_all_shells() -> TestResult {
    let env = TestEnv::new()?;

    for shell in ["zsh", "bash", "fish", "posix"] {
        let before = temp_tree_snapshot(env.temp.path())?;
        let output = env
            .cmd()
            .args(["init", "--shell", shell, "--mobile-reconnect"])
            .output()?;
        assert!(output.status.success(), "{shell}: {output:?}");
        assert!(output.stderr.is_empty(), "{shell}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let expected_fragments = [
            "lterm init preview",
            "modifies_files\tno",
            "mobile_reconnect\toptional",
            "mobile_reconnect_command\tlterm reconnect --mobile",
            "interactive SSH terminal only",
            "LTERM_RECONNECT_DISABLE=1",
            "mobile_reconnect_remove\tdelete the copied lterm mobile reconnect block",
            "exec lterm reconnect --mobile",
        ];
        assert!(
            stdout.contains(&format!("shell\t{shell}")),
            "{shell} mobile reconnect preview missing shell row:\n{stdout}"
        );
        for expected in expected_fragments {
            assert!(
                stdout.contains(expected),
                "{shell} mobile reconnect preview missing {expected:?}:\n{stdout}"
            );
        }
        if shell == "fish" {
            assert!(
                stdout.contains("set -gx LTERM_RECONNECT_DISABLE 1")
                    && stdout.contains("status is-interactive"),
                "fish preview should use fish syntax:\n{stdout}"
            );
        } else {
            assert!(
                stdout.contains("export LTERM_RECONNECT_DISABLE=1")
                    && stdout.contains("case $- in *i*)"),
                "{shell} preview should use POSIX-compatible syntax:\n{stdout}"
            );
        }
        assert_eq!(
            before,
            temp_tree_snapshot(env.temp.path())?,
            "init --mobile-reconnect must not modify shell startup files or daemon state for {shell}"
        );
    }

    Ok(())
}

#[test]
fn init_rejects_unsupported_shell_values() -> TestResult {
    let env = TestEnv::new()?;
    let output = env.cmd().args(["init", "--shell", "powershell"]).output()?;
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid value") && stderr.contains("powershell"),
        "unsupported shell should be rejected by clap value parser:\n{stderr}"
    );
    Ok(())
}

#[test]
fn help_describes_general_agent_profile_surface() -> TestResult {
    let env = TestEnv::new()?;

    let top_level = env.cmd().arg("--help").output()?;
    assert!(top_level.status.success(), "{top_level:?}");
    let stdout = String::from_utf8_lossy(&top_level.stdout);
    let normalized = normalize_help(&stdout);
    assert!(
        normalized
            .contains("List agent launcher profiles, default settings, and PATH availability"),
        "top-level help should not describe agents as built-in-only:\n{stdout}"
    );
    assert!(
        normalized.contains(
            "Run a built-in, configured, or PATH-resolved agent CLI profile inside a tmux-compatible lterm session"
        ),
        "top-level help should expose the general agent profile model:\n{stdout}"
    );
    assert!(
        !normalized.contains("List built-in agent launcher profiles"),
        "stale built-in-only help leaked into top-level help:\n{stdout}"
    );
    assert!(
        !normalized.contains("Run a known or PATH-resolved agent CLI"),
        "stale known-only agent help leaked into top-level help:\n{stdout}"
    );

    let agents = env.cmd().args(["agents", "--help"]).output()?;
    assert!(agents.status.success(), "{agents:?}");
    let stdout = String::from_utf8_lossy(&agents.stdout);
    let normalized = normalize_help(&stdout);
    for expected in [
        "List agent launcher profiles, default settings, and PATH availability",
        "Print profiles as a JSON array",
        "JSON file with additional configured custom agent profiles",
        "Optional built-in, configured, or PATH-resolved custom profile names to inspect",
    ] {
        assert!(
            normalized.contains(expected),
            "missing {expected:?}:\n{stdout}"
        );
    }
    assert!(
        !normalized.contains("List built-in agent launcher profiles"),
        "stale built-in-only help leaked into agents help:\n{stdout}"
    );

    let agent = env.cmd().args(["agent", "--help"]).output()?;
    assert!(agent.status.success(), "{agent:?}");
    let stdout = String::from_utf8_lossy(&agent.stdout);
    let normalized = normalize_help(&stdout);
    for expected in [
        "Run a built-in, configured, or PATH-resolved agent CLI profile inside a tmux-compatible lterm session",
        "Built-in, configured, or PATH-resolved custom profile name, e.g. claude, codex, opencode, agy",
        "JSON file with additional configured custom agent profiles",
    ] {
        assert!(
            normalized.contains(expected),
            "missing {expected:?}:\n{stdout}"
        );
    }
    assert!(
        !normalized.contains("Run a known or PATH-resolved agent CLI"),
        "stale known-only agent help leaked into agent help:\n{stdout}"
    );

    Ok(())
}

#[test]
fn help_describes_forwarded_agent_arguments() -> TestResult {
    let env = TestEnv::new()?;

    let agent = env.cmd().args(["agent", "--help"]).output()?;
    assert!(agent.status.success(), "{agent:?}");
    let stdout = String::from_utf8_lossy(&agent.stdout);
    let normalized = normalize_help(&stdout);
    assert!(
        normalized.contains(
            "Arguments forwarded to the agent CLI; use `--` before args that look like lterm options"
        ),
        "generic agent help should explain forwarded args:\n{stdout}"
    );

    for (command, expected) in [
        (
            "claude",
            "Arguments forwarded to claude; use `--` before args that look like lterm options",
        ),
        (
            "codex",
            "Arguments forwarded to codex; use `--` before args that look like lterm options",
        ),
        (
            "opencode",
            "Arguments forwarded to opencode; use `--` before args that look like lterm options",
        ),
        (
            "copilot",
            "Arguments forwarded to copilot; use `--` before args that look like lterm options",
        ),
        (
            "cursor-agent",
            "Arguments forwarded to cursor-agent; use `--` before args that look like lterm options",
        ),
        (
            "agy",
            "Arguments forwarded to agy; use `--` before args that look like lterm options",
        ),
        (
            "jules",
            "Arguments forwarded to jules; use `--` before args that look like lterm options",
        ),
        (
            "kiro",
            "Arguments forwarded to kiro-cli; use `--` before args that look like lterm options",
        ),
        (
            "aider",
            "Arguments forwarded to aider; use `--` before args that look like lterm options",
        ),
        (
            "goose",
            "Arguments forwarded to goose; use `--` before args that look like lterm options",
        ),
        (
            "amp",
            "Arguments forwarded to amp; use `--` before args that look like lterm options",
        ),
        (
            "crush",
            "Arguments forwarded to crush; use `--` before args that look like lterm options",
        ),
        (
            "gemini",
            "Arguments forwarded to gemini; use `--` before args that look like lterm options",
        ),
        (
            "kimi",
            "Arguments forwarded to kimi; use `--` before args that look like lterm options",
        ),
        (
            "qwen",
            "Arguments forwarded to qwen; use `--` before args that look like lterm options",
        ),
        (
            "omx",
            "Arguments forwarded to omx; use `--` before args that look like lterm options",
        ),
        (
            "omc",
            "Arguments forwarded to omc; use `--` before args that look like lterm options",
        ),
    ] {
        let output = env.cmd().args([command, "--help"]).output()?;
        assert!(output.status.success(), "{command} help failed: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let normalized = normalize_help(&stdout);
        assert!(
            normalized.contains(expected),
            "{command} help should explain forwarded args:\n{stdout}"
        );
    }

    Ok(())
}

#[test]
fn help_describes_machine_readable_session_surfaces() -> TestResult {
    let env = TestEnv::new()?;

    for command in ["sessions", "list"] {
        let output = env.cmd().args([command, "--help"]).output()?;
        assert!(
            output.status.success(),
            "{command} --help failed: {output:?}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let normalized = normalize_help(&stdout);
        assert!(
            normalized.contains("Print sessions as a JSON array for automation"),
            "{command} help should describe --json output:\n{stdout}"
        );
        assert!(
            normalized.contains("Include child panes created from inside another lterm session"),
            "{command} help should describe --all scope:\n{stdout}"
        );
        assert!(
            normalized.contains("Show only child panes created from inside another lterm session"),
            "{command} help should describe --children scope:\n{stdout}"
        );
    }

    for command in ["ps", "processes"] {
        let ps = env.cmd().args([command, "--help"]).output()?;
        assert!(ps.status.success(), "{command} help failed: {ps:?}");
        let stdout = String::from_utf8_lossy(&ps.stdout);
        let normalized = normalize_help(&stdout);
        assert!(
            normalized.contains("Optional session or pane target to inspect"),
            "{command} help should describe target argument:\n{stdout}"
        );
        assert!(
            normalized.contains("Print process rows as a JSON array for automation"),
            "{command} help should describe --json output:\n{stdout}"
        );
        assert!(
            normalized.contains("Include same-process-group rows that escaped the child tree"),
            "{command} help should describe --orphans output:\n{stdout}"
        );
    }

    Ok(())
}

#[test]
fn instrument_emits_one_raw_free_json_line_and_rejects_unknown_targets() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "instrument-json")?;

    let output = env.cmd().args(["instrument", &pane, "--json"]).output()?;
    assert!(output.status.success(), "instrument failed: {output:?}");
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "instrument must emit exactly one JSON line: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let keys = value
        .as_object()
        .ok_or("instrument output should be a JSON object")?
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        keys,
        [
            "alive",
            "attached_clients",
            "cols",
            "observed_unix_ms",
            "output_closed",
            "output_revision",
            "output_total_bytes",
            "pane_id",
            "rows",
            "schema_version",
            "session_id",
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(value["schema_version"], "1.0");
    assert_eq!(value["pane_id"], pane);
    let encoded = String::from_utf8(output.stdout)?;
    for forbidden in [
        "SESSION_READY",
        "sleep 30",
        "\"command\"",
        "\"cwd\"",
        "\"name\"",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "leaked {forbidden:?}: {encoded}"
        );
    }

    let missing = env
        .cmd()
        .args(["instrument", "missing-instrument", "--json"])
        .output()?;
    assert!(!missing.status.success(), "unknown target should fail");
    assert!(
        missing.stdout.is_empty(),
        "failure must not emit fallback JSON"
    );
    assert_stderr_contains(&missing, "no such lterm session or pane");

    Ok(())
}

#[test]
#[cfg(unix)]
fn instrument_rejects_protocol_three_before_sending_instrument_request() -> TestResult {
    let env = TestEnv::new()?;
    let run_dir = env.temp.path().join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))?;
    let socket = run_dir.join("lterm.sock");
    let listener = UnixListener::bind(&socket)?;
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let thread_requests = requests.clone();
    let server = thread::spawn(move || -> Result<(), String> {
        (|| -> TestResult {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept()?;
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes)?;
                let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                let request_type = request["type"]
                    .as_str()
                    .ok_or("fake daemon request missing type")?
                    .to_string();
                thread_requests
                    .lock()
                    .map_err(|_| "fake daemon requests lock poisoned")?
                    .push(request_type.clone());
                let response = if request_type == "ping" {
                    serde_json::json!({"ok": true, "result": {"pong": true}})
                } else {
                    assert_eq!(request_type, "status");
                    serde_json::json!({
                        "ok": true,
                        "result": {
                            "version": "1.0.29",
                            "protocol_version": 3,
                            "session_count": 0,
                            "active_connections": 1,
                            "shutting_down": false
                        }
                    })
                };
                stream.write_all(serde_json::to_string(&response)?.as_bytes())?;
            }
            listener.set_nonblocking(true)?;
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut bytes = Vec::new();
                        stream.read_to_end(&mut bytes)?;
                        let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                        return Err(format!("unexpected fourth request: {request}").into());
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            Ok(())
        })()
        .map_err(|err| err.to_string())
    });

    let output = env.cmd().args(["instrument", "stale", "--json"]).output()?;
    assert!(!output.status.success(), "protocol 3 must be rejected");
    assert_stderr_contains(&output, "does not support instrument snapshots");
    assert_stderr_contains(&output, "lterm shutdown");
    server
        .join()
        .map_err(|_| "fake protocol-three daemon panicked")??;
    assert_eq!(
        *requests
            .lock()
            .map_err(|_| "fake daemon requests lock poisoned")?,
        ["ping", "status", "status"]
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn exits_json_uses_protocol_v8_bounded_raw_free_request_and_response() -> TestResult {
    let env = TestEnv::new()?;
    let run_dir = env.temp.path().join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))?;
    let listener = UnixListener::bind(run_dir.join("lterm.sock"))?;
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let thread_requests = requests.clone();
    let server = thread::spawn(move || -> Result<(), String> {
        (|| -> TestResult {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept()?;
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes)?;
                let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                let request_type = request["type"]
                    .as_str()
                    .ok_or("fake daemon request missing type")?;
                let response = match request_type {
                    "ping" => serde_json::json!({"ok": true, "result": {"pong": true}}),
                    "status" => serde_json::json!({
                        "ok": true,
                        "result": {
                            "version": env!("CARGO_PKG_VERSION"),
                            "protocol_version": 8,
                            "session_count": 0,
                            "active_connections": 1,
                            "shutting_down": false
                        }
                    }),
                    "recent_exits" => serde_json::json!({
                        "ok": true,
                        "result": [{
                            "schema_version": "1.0",
                            "session_id": "opaque-id",
                            "name": "agent",
                            "pane_id": "%7",
                            "created_unix_ms": 10,
                            "trigger_claimed_unix_ms": 20,
                            "reaped_unix_ms": 30,
                            "trigger": {"type": "leader_exited"},
                            "outcome_state": "complete",
                            "exit_code": 37,
                            "evidence_state": "complete"
                        }]
                    }),
                    other => return Err(format!("unexpected fake daemon request: {other}").into()),
                };
                thread_requests
                    .lock()
                    .map_err(|_| "fake daemon requests lock poisoned")?
                    .push(request);
                stream.write_all(serde_json::to_string(&response)?.as_bytes())?;
            }
            Ok(())
        })()
        .map_err(|err| err.to_string())
    });

    let output = env
        .cmd()
        .args(["exits", "opaque-id", "--limit", "1", "--all", "--json"])
        .output()?;
    assert!(output.status.success(), "exits failed: {output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(rows[0]["session_id"], "opaque-id");
    assert_eq!(rows[0]["trigger"]["type"], "leader_exited");
    assert_eq!(rows[0]["exit_code"], 37);
    let rendered = String::from_utf8(output.stdout)?;
    for forbidden in ["command", "cwd", "environment", "scrollback", "process_id"] {
        assert!(
            !rendered.contains(forbidden),
            "leaked {forbidden}: {rendered}"
        );
    }

    server.join().map_err(|_| "fake v8 daemon panicked")??;
    let requests = requests
        .lock()
        .map_err(|_| "fake daemon requests lock poisoned")?;
    assert_eq!(
        requests
            .iter()
            .filter_map(|request| request["type"].as_str())
            .collect::<Vec<_>>(),
        ["ping", "status", "status", "recent_exits"]
    );
    assert_eq!(requests[3]["target"], "opaque-id");
    assert_eq!(requests[3]["limit"], 1);
    assert_eq!(requests[3]["scope"], "all");
    Ok(())
}

#[test]
#[cfg(unix)]
fn metadata_rejects_protocol_five_before_sending_metadata_request() -> TestResult {
    let env = TestEnv::new()?;
    let run_dir = env.temp.path().join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))?;
    let listener = UnixListener::bind(run_dir.join("lterm.sock"))?;
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let thread_requests = requests.clone();
    let server = thread::spawn(move || -> Result<(), String> {
        (|| -> TestResult {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept()?;
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes)?;
                let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                let request_type = request["type"]
                    .as_str()
                    .ok_or("fake daemon request missing type")?
                    .to_string();
                thread_requests
                    .lock()
                    .map_err(|_| "fake daemon requests lock poisoned")?
                    .push(request_type.clone());
                let response = if request_type == "ping" {
                    serde_json::json!({"ok": true, "result": {"pong": true}})
                } else {
                    assert_eq!(request_type, "status");
                    serde_json::json!({
                        "ok": true,
                        "result": {
                            "version": "1.0.30",
                            "protocol_version": 5,
                            "session_count": 1,
                            "active_connections": 1,
                            "shutting_down": false
                        }
                    })
                };
                stream.write_all(serde_json::to_string(&response)?.as_bytes())?;
            }
            listener.set_nonblocking(true)?;
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut bytes = Vec::new();
                        stream.read_to_end(&mut bytes)?;
                        let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                        return Err(format!("unexpected metadata request: {request}").into());
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            Ok(())
        })()
        .map_err(|err| err.to_string())
    });

    let output = env
        .cmd()
        .args(["metadata", "history", "stale", "--json"])
        .output()?;
    assert!(!output.status.success(), "protocol 5 must be rejected");
    assert_stderr_contains(&output, "does not support metadata history");
    assert_stderr_contains(&output, "lterm shutdown");
    server
        .join()
        .map_err(|_| "fake protocol-five daemon panicked")??;
    assert_eq!(
        *requests
            .lock()
            .map_err(|_| "fake daemon requests lock poisoned")?,
        ["ping", "status", "status"]
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn explicit_tmux_parent_rejects_protocol_six_before_sending_new() -> TestResult {
    let env = TestEnv::new()?;
    let run_dir = env.temp.path().join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))?;
    let listener = UnixListener::bind(run_dir.join("lterm.sock"))?;
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let thread_requests = requests.clone();
    let server = thread::spawn(move || -> Result<(), String> {
        (|| -> TestResult {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept()?;
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes)?;
                let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                let request_type = request["type"]
                    .as_str()
                    .ok_or("fake daemon request missing type")?
                    .to_string();
                thread_requests
                    .lock()
                    .map_err(|_| "fake daemon requests lock poisoned")?
                    .push(request_type.clone());
                let response = match request_type.as_str() {
                    "ping" => serde_json::json!({"ok": true, "result": {"pong": true}}),
                    "status" => serde_json::json!({
                        "ok": true,
                        "result": {
                            "version": "1.0.31",
                            "protocol_version": 6,
                            "session_count": 1,
                            "active_connections": 1,
                            "shutting_down": false
                        }
                    }),
                    "info" => serde_json::json!({
                        "ok": true,
                        "result": {
                            "id": "stale-parent-id",
                            "name": "stale-parent",
                            "pane_id": "%7",
                            "command": "sleep 30",
                            "cwd": "/tmp",
                            "created_unix_ms": 1,
                            "alive": true,
                            "exit_code": null,
                            "rows": 24,
                            "cols": 80
                        }
                    }),
                    other => {
                        return Err(format!("unexpected request before preflight: {other}").into());
                    }
                };
                stream.write_all(serde_json::to_string(&response)?.as_bytes())?;
            }
            listener.set_nonblocking(true)?;
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut bytes = Vec::new();
                        stream.read_to_end(&mut bytes)?;
                        let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                        return Err(format!(
                            "unexpected request after protocol rejection: {request}"
                        )
                        .into());
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            Ok(())
        })()
        .map_err(|err| err.to_string())
    });

    let output = env
        .cmd()
        .env("LTERM_PANE", "%99")
        .env("LTERM_PARENT_TOKEN", "ambient-parent-token")
        .args([
            "tmux-compat",
            "split-window",
            "-d",
            "-t",
            "stale-parent",
            "sh",
            "-lc",
            "printf helper-should-not-run",
        ])
        .output()?;
    assert!(!output.status.success(), "protocol 6 must be rejected");
    assert_stderr_contains(&output, "does not support explicit tmux parent panes");
    assert_stderr_contains(&output, "requires protocol 7");
    assert_stderr_contains(&output, "lterm shutdown");
    server
        .join()
        .map_err(|_| "fake protocol-six daemon panicked")??;
    assert_eq!(
        *requests
            .lock()
            .map_err(|_| "fake daemon requests lock poisoned")?,
        ["ping", "status", "info", "ping", "status"]
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn capability_daemon_swap_sends_only_nonsecret_hello_before_ready() -> TestResult {
    let env = TestEnv::new()?;
    let run_dir = env.temp.path().join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))?;
    let socket = run_dir.join("lterm.sock");
    let listener = UnixListener::bind(&socket)?;
    let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let thread_observed = observed.clone();
    let server = thread::spawn(move || -> Result<(), String> {
        (|| -> TestResult {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept()?;
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes)?;
                let request: serde_json::Value = serde_json::from_slice(&bytes)?;
                let response = if request["type"] == "ping" {
                    serde_json::json!({"ok":true,"result":{"pong":true}})
                } else {
                    serde_json::json!({"ok":true,"result":{
                        "version":"1.0.30",
                        "protocol_version":5,
                        "session_count":0,
                        "active_connections":1,
                        "shutting_down":false
                    }})
                };
                stream.write_all(serde_json::to_string(&response)?.as_bytes())?;
            }
            let (stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut reader = std::io::BufReader::new(stream);
            let mut hello = Vec::new();
            reader.read_until(b'\n', &mut hello)?;
            *thread_observed
                .lock()
                .map_err(|_| "observed hello lock poisoned")? = hello;
            reader
                .get_mut()
                .write_all(b"{\"ok\":false,\"error\":\"unknown request type\"}\n")?;
            reader.get_mut().flush()?;
            Ok(())
        })()
        .map_err(|err| err.to_string())
    });

    let sentinel = "123e4567-e89b-42d3-a456-426614174000";
    let capability = env.temp.path().join("swap.cap");
    std::fs::write(
        &capability,
        format!("lterm-input-capability-v1\n{sentinel}\n"),
    )?;
    std::fs::set_permissions(&capability, std::fs::Permissions::from_mode(0o600))?;
    let mut child = env
        .cmd()
        .args(["capability", "input", "--capability"])
        .arg(&capability)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("capability stdin unavailable")?
        .write_all(b"SECRET_PAYLOAD")?;
    let output = child.wait_with_output()?;
    assert!(!output.status.success(), "swap daemon must reject channel");
    server.join().map_err(|_| "fake swap daemon panicked")??;
    let hello = observed
        .lock()
        .map_err(|_| "observed hello lock poisoned")?
        .clone();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&hello)?["type"],
        "capability_channel"
    );
    assert!(
        !hello
            .windows(sentinel.len())
            .any(|part| part == sentinel.as_bytes())
    );
    assert!(!hello.windows(14).any(|part| part == b"SECRET_PAYLOAD"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(sentinel));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SECRET_PAYLOAD"));
    Ok(())
}

#[test]
#[cfg(unix)]
fn capability_revoke_refuses_replaced_path_after_daemon_success() -> TestResult {
    let env = TestEnv::new()?;
    let run_dir = env.temp.path().join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))?;
    let listener = UnixListener::bind(run_dir.join("lterm.sock"))?;
    let capability = env.temp.path().join("revoke-replace.cap");
    let original = env.temp.path().join("revoke-original.cap");
    let original_token = "123e4567-e89b-42d3-a456-426614174000";
    let replacement_token = "223e4567-e89b-42d3-a456-426614174000";
    std::fs::write(
        &capability,
        format!("lterm-input-capability-v1\n{original_token}\n"),
    )?;
    std::fs::set_permissions(&capability, std::fs::Permissions::from_mode(0o600))?;
    let replace_path = capability.clone();
    let replace_original = original.clone();
    let server = spawn_fake_capability_server(
        listener,
        move || {
            std::fs::rename(&replace_path, &replace_original)?;
            std::fs::write(
                &replace_path,
                format!("lterm-input-capability-v1\n{replacement_token}\n"),
            )?;
            std::fs::set_permissions(&replace_path, std::fs::Permissions::from_mode(0o600))?;
            Ok(())
        },
        true,
    );
    let output = env
        .cmd()
        .args(["capability", "revoke", "--capability"])
        .arg(&capability)
        .output()?;
    assert!(
        !output.status.success(),
        "replaced leaf must not be removed"
    );
    assert_stderr_contains(&output, "changed; refusing to unlink");
    let sensitive = server
        .join()
        .map_err(|_| "fake capability server panicked")??;
    assert!(
        String::from_utf8_lossy(&sensitive).contains(original_token),
        "server should receive only the originally validated token"
    );
    assert_eq!(
        std::fs::read_to_string(&capability)?,
        format!("lterm-input-capability-v1\n{replacement_token}\n")
    );
    assert!(original.exists(), "original inode remains at moved path");
    Ok(())
}

#[test]
#[cfg(unix)]
fn capability_revoke_transport_failure_retains_private_file() -> TestResult {
    let env = TestEnv::new()?;
    let run_dir = env.temp.path().join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))?;
    let listener = UnixListener::bind(run_dir.join("lterm.sock"))?;
    let capability = env.temp.path().join("revoke-transport.cap");
    let token = "123e4567-e89b-42d3-a456-426614174000";
    let contents = format!("lterm-input-capability-v1\n{token}\n");
    std::fs::write(&capability, &contents)?;
    std::fs::set_permissions(&capability, std::fs::Permissions::from_mode(0o600))?;
    let server = spawn_fake_capability_server(listener, || Ok(()), false);
    let output = env
        .cmd()
        .args(["capability", "revoke", "--capability"])
        .arg(&capability)
        .output()?;
    assert!(
        !output.status.success(),
        "missing operation response must fail"
    );
    let sensitive = server
        .join()
        .map_err(|_| "fake capability server panicked")??;
    assert!(String::from_utf8_lossy(&sensitive).contains(token));
    assert_eq!(std::fs::read_to_string(&capability)?, contents);
    Ok(())
}

#[test]
fn help_describes_session_creation_arguments() -> TestResult {
    let env = TestEnv::new()?;

    for (command, tmux_flag, tmux_description, command_description) in [
        (
            "start",
            "--tmux",
            "Expose the lterm tmux compatibility shim inside the session (off by default)",
            "Shell command to run in the session; defaults to the user's shell when omitted",
        ),
        (
            "new",
            "--tmux",
            "Expose the lterm tmux compatibility shim inside the session (off by default)",
            "Shell command to run in the session; defaults to the user's shell when omitted",
        ),
        (
            "run",
            "--no-tmux",
            "Disable the lterm tmux compatibility shim for this run session (enabled by default)",
            "Required shell command to run in the tmux-compatible session",
        ),
    ] {
        let output = env.cmd().args([command, "--help"]).output()?;
        assert!(output.status.success(), "{command} help failed: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let normalized = normalize_help(&stdout);
        assert!(
            normalized.contains("Session name to use instead of an auto-generated name"),
            "{command} help should describe --name:\n{stdout}"
        );
        assert!(
            normalized.contains("Working directory for the session command"),
            "{command} help should describe --cwd:\n{stdout}"
        );
        assert!(
            normalized.contains(tmux_flag),
            "{command} help should show the tmux compatibility flag:\n{stdout}"
        );
        assert!(
            normalized.contains(tmux_description),
            "{command} help should describe the tmux compatibility flag:\n{stdout}"
        );
        assert!(
            normalized.contains("--status-theme <THEME>"),
            "{command} help should show --status-theme:\n{stdout}"
        );
        assert!(
            normalized.contains("Status bar theme stored on this session"),
            "{command} help should describe --status-theme:\n{stdout}"
        );
        if command == "run" {
            assert!(
                !normalized.contains(" --tmux "),
                "run help should not expose the hidden legacy --tmux no-op:\n{stdout}"
            );
        }
        assert!(
            normalized.contains(command_description),
            "{command} help should describe trailing command args:\n{stdout}"
        );
    }

    Ok(())
}

#[test]
fn help_describes_target_io_and_remote_arguments() -> TestResult {
    let env = TestEnv::new()?;

    for (command, expected) in [
        (
            "resume",
            &[
                "Session or pane target to resume",
                "default: %0",
                "Disable the lterm status bar while attached",
            ][..],
        ),
        (
            "attach",
            &[
                "Session or pane target to resume",
                "default: %0",
                "Disable the lterm status bar while attached",
            ][..],
        ),
        (
            "open",
            &[
                "Session or pane target to attach or create",
                "default: main",
            ][..],
        ),
        (
            "attach-or-new",
            &[
                "Session or pane target to attach or create",
                "default: main",
            ][..],
        ),
        (
            "reconnect",
            &[
                "Fallback session target to open when no recent session is available",
                "default: main",
                "Attach policy to use: auto, raw, or mobile transcript",
                "Force the mobile transcript view instead of raw attach",
            ][..],
        ),
        ("close", &["Session or pane target to close"][..]),
        ("kill", &["Session or pane target to close"][..]),
        (
            "rename",
            &[
                "Session or pane target whose session metadata should be renamed",
                "New session name for future target lookup",
            ][..],
        ),
        (
            "status-theme",
            &[
                "Session or pane target to update",
                "Theme name, or `default` to use the attaching client's default",
            ][..],
        ),
        (
            "theme",
            &[
                "Session or pane target to update",
                "Theme name, or `default` to use the attaching client's default",
            ][..],
        ),
        (
            "input",
            &[
                "Session or pane target to receive input",
                "Text to send to the target PTY",
                "Append Enter after the text",
            ][..],
        ),
        (
            "send",
            &[
                "Session or pane target to receive input",
                "Text to send to the target PTY",
                "Append Enter after the text",
            ][..],
        ),
        (
            "logs",
            &[
                "Session or pane target to capture",
                "Starting scrollback line offset, matching tmux -S semantics",
                "Inclusive ending scrollback line offset, matching tmux -E semantics",
            ][..],
        ),
        (
            "capture",
            &[
                "Session or pane target to capture",
                "Starting scrollback line offset, matching tmux -S semantics",
                "Inclusive ending scrollback line offset, matching tmux -E semantics",
            ][..],
        ),
        (
            "compose",
            &[
                "Session or pane target to review and receive committed input",
                "Number of sanitized scrollback lines to show",
                "Run one capture/send cycle for automation and tests",
                "Text to commit in --once mode",
                "Do not append Enter",
            ][..],
        ),
        (
            "mobile",
            &[
                "Session or pane target to review and receive committed input",
                "Number of sanitized scrollback lines to show",
                "Run one capture/send cycle for automation and tests",
                "Text to commit in --once mode",
                "Do not append Enter",
            ][..],
        ),
        (
            "wait",
            &[
                "Session or pane target to observe",
                "Wait until the session leader exits",
                "Wait until sanitized scrollback contains this text",
                "Maximum wait time",
                "Limit --contains scans to the last N sanitized scrollback lines",
                "Print a machine-readable JSON result",
            ][..],
        ),
        (
            "watch",
            &[
                "Session or pane target to observe",
                "Watch until the session leader exits",
                "Watch until sanitized scrollback contains this text",
                "Maximum watch time",
                "Limit --contains scans to the last N sanitized scrollback lines",
                "Print a machine-readable JSON result",
                "Send a cmux-friendly notification when the condition is met",
            ][..],
        ),
        (
            "tmux-compat",
            &["Arguments forwarded to the tmux compatibility parser"][..],
        ),
        (
            "notify",
            &[
                "Notification title",
                "Optional notification subtitle",
                "Notification body text",
            ][..],
        ),
        (
            "ssh",
            &[
                "SSH host to connect to",
                "Remote session or pane target to attach",
                "default: main",
                "Additional ssh arguments after `--`",
            ][..],
        ),
    ] {
        let output = env.cmd().args([command, "--help"]).output()?;
        assert!(output.status.success(), "{command} help failed: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let normalized = normalize_help(&stdout);
        for phrase in expected {
            assert!(
                normalized.contains(phrase),
                "{command} help should include {phrase:?}:\n{stdout}"
            );
        }

        let unexpected_defaults = match command {
            "resume" | "attach" => &["defaults to %0"][..],
            "open" | "attach-or-new" | "reconnect" | "ssh" => &["defaults to main"][..],
            _ => &[][..],
        };
        for phrase in unexpected_defaults {
            assert!(
                !normalized.contains(phrase),
                "{command} help should not duplicate clap default text with {phrase:?}:\n{stdout}"
            );
        }
    }

    Ok(())
}

#[test]
fn help_describes_daemon_lifecycle_commands() -> TestResult {
    let env = TestEnv::new()?;

    for (command, expected) in [
        ("daemon", "Run the background PTY session daemon"),
        ("doctor", "Diagnose daemon, shim, and version state"),
        ("inspect", "Inspect redacted local diagnostics as JSON"),
        ("status", "Diagnose daemon, shim, and version state"),
        ("shutdown", "Stop the daemon and all sessions"),
    ] {
        let output = env.cmd().args([command, "--help"]).output()?;
        assert!(output.status.success(), "{command} help failed: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let normalized = normalize_help(&stdout);
        assert!(
            normalized.contains(expected),
            "{command} help should include {expected:?}:\n{stdout}"
        );
    }

    Ok(())
}

#[test]
fn open_and_attach_or_new_create_missing_session() -> TestResult {
    let env = TestEnv::new()?;

    for (command, target) in [
        ("open", "open-missing"),
        ("attach-or-new", "attach-or-new-missing"),
    ] {
        let started = Instant::now();
        let output = wait_for_child_output(
            ChildCleanup::new(
                env.cmd()
                    .args([command, target, "--no-status"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?,
            ),
            Duration::from_secs(2),
            &format!("{command} missing-session EOF detach"),
        )?;
        assert!(output.status.success(), "{command}: {output:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{command} did not detach promptly after stdin EOF: {output:?}"
        );

        let listed = env.cmd().args(["sessions", "--all"]).output()?;
        assert!(listed.status.success(), "{listed:?}");
        let stdout = String::from_utf8_lossy(&listed.stdout);
        assert!(
            stdout.contains(target),
            "{command} did not create missing session {target}: {stdout}"
        );
    }

    for (command, target) in [
        ("open", "open-existing"),
        ("attach-or-new", "attach-or-new-existing"),
    ] {
        let status = env
            .cmd()
            .args([
                "start",
                "--detach",
                "--name",
                target,
                "--",
                "sh",
                "-lc",
                "echo READY; sleep 30",
            ])
            .status()?;
        assert!(
            status.success(),
            "failed to create existing target {target}"
        );
        let captured = env.capture_until(target, "READY")?;
        assert!(captured.contains("READY"), "missing output: {captured}");

        let started = Instant::now();
        let output = wait_for_child_output(
            ChildCleanup::new(
                env.cmd()
                    .args([command, target, "--no-status"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?,
            ),
            Duration::from_secs(2),
            &format!("{command} existing-session EOF detach"),
        )?;
        assert!(output.status.success(), "{command}: {output:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{command} did not attach existing session promptly after stdin EOF: {output:?}"
        );

        let listed = env.cmd().args(["sessions", "--all"]).output()?;
        assert!(listed.status.success(), "{listed:?}");
        let stdout = String::from_utf8_lossy(&listed.stdout);
        let rows = stdout
            .lines()
            .filter(|line| line.starts_with(&format!("{target}\t")))
            .count();
        assert_eq!(
            rows, 1,
            "{command} should attach existing {target} without creating duplicates:\n{stdout}"
        );
    }

    Ok(())
}

#[test]
fn reconnect_resumes_last_selected_session_in_mobile_transcript() -> TestResult {
    let env = TestEnv::new()?;

    for (name, marker) in [
        ("reconnect-first", "FIRST_RECONNECT_READY"),
        ("reconnect-second", "SECOND_RECONNECT_READY"),
    ] {
        let status = env
            .cmd()
            .args([
                "start",
                "--detach",
                "--name",
                name,
                "--",
                "sh",
                "-lc",
                &format!("printf '{marker}\\n'; sleep 30"),
            ])
            .status()?;
        assert!(status.success(), "failed to create {name}");
        env.capture_until(name, marker)?;
    }
    let _first_cleanup = SessionCleanup::new(&env, "reconnect-first");
    let _second_cleanup = SessionCleanup::new(&env, "reconnect-second");

    for (name, marker) in [
        ("reconnect-first", "FIRST_RECONNECT_READY"),
        ("reconnect-second", "SECOND_RECONNECT_READY"),
    ] {
        let output = env
            .cmd()
            .stdin(Stdio::null())
            .args(["resume", name, "--mobile", "--tail", "20"])
            .output()?;
        assert!(output.status.success(), "{name}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(marker), "{name} resume output: {stdout:?}");
    }

    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args(["reconnect", "--mobile", "--tail", "20"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("target=reconnect-second") && stdout.contains("SECOND_RECONNECT_READY"),
        "reconnect should resume the most recently selected session:\n{stdout}"
    );
    assert!(
        !stdout.contains("FIRST_RECONNECT_READY"),
        "reconnect attached the older session instead of the latest selection:\n{stdout}"
    );

    Ok(())
}

#[test]
fn reconnect_falls_back_to_main_when_state_is_missing_or_stale() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "start",
            "--detach",
            "--name",
            "main",
            "--",
            "sh",
            "-lc",
            "printf 'MAIN_RECONNECT_READY\\n'; sleep 30",
        ])
        .status()?;
    assert!(status.success(), "failed to create main fallback");
    let _cleanup = SessionCleanup::new(&env, "main");
    env.capture_until("main", "MAIN_RECONNECT_READY")?;

    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args(["reconnect", "--mobile", "--tail", "20"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("target=main") && stdout.contains("MAIN_RECONNECT_READY"),
        "reconnect with no state should fall back to main:\n{stdout}"
    );

    let state_path = env.temp.path().join("data").join("reconnect-state.json");
    std::fs::create_dir_all(
        state_path
            .parent()
            .ok_or("missing reconnect state parent")?,
    )?;
    std::fs::write(
        &state_path,
        br#"{"schema_version":1,"session_id":"stale-session","pane_id":"%999","session_name":"stale-reconnect","recorded_at_unix_ms":1}"#,
    )?;
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&state_path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&state_path, perms)?;
    }

    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args(["reconnect", "--mobile", "--tail", "20"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("target=main") && stdout.contains("MAIN_RECONNECT_READY"),
        "reconnect with stale state should fall back to main:\n{stdout}"
    );

    Ok(())
}

#[test]
fn reconnect_read_only_never_creates_missing_fallback() -> TestResult {
    let env = TestEnv::new()?;

    for args in [
        vec!["reconnect", "readonly-missing", "--read-only"],
        vec![
            "reconnect",
            "readonly-mobile-missing",
            "--mobile",
            "--read-only",
        ],
    ] {
        let output = env.cmd().stdin(Stdio::null()).args(&args).output()?;
        assert!(
            !output.status.success(),
            "read-only reconnect should fail instead of creating a missing fallback for {args:?}: {output:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--read-only requires an existing reconnect or fallback target"),
            "read-only reconnect should explain that it needs an existing target for {args:?}: {stderr}"
        );
    }

    let names = session_names_json(&env)?;
    assert!(
        !names.contains("readonly-missing") && !names.contains("readonly-mobile-missing"),
        "read-only reconnect must not create missing fallback sessions: {names:?}"
    );

    Ok(())
}

#[test]
fn reconnect_ignores_live_pane_or_name_when_session_id_mismatches() -> TestResult {
    let env = TestEnv::new()?;

    for (name, marker) in [
        ("main", "MAIN_ID_MISMATCH_READY"),
        ("reconnect-reused", "REUSED_ID_MISMATCH_READY"),
    ] {
        let status = env
            .cmd()
            .args([
                "start",
                "--detach",
                "--name",
                name,
                "--",
                "sh",
                "-lc",
                &format!("printf '{marker}\\n'; sleep 30"),
            ])
            .status()?;
        assert!(status.success(), "failed to create {name}");
        env.capture_until(name, marker)?;
    }
    let _main_cleanup = SessionCleanup::new(&env, "main");
    let _reused_cleanup = SessionCleanup::new(&env, "reconnect-reused");

    let rows = session_rows_json(&env, true)?;
    let reused_pane = rows
        .iter()
        .find(|row| row.name == "reconnect-reused")
        .ok_or_else(|| format!("missing reconnect-reused row: {rows:?}"))?
        .pane_id
        .clone();
    let state_path = env.temp.path().join("data").join("reconnect-state.json");
    std::fs::create_dir_all(
        state_path
            .parent()
            .ok_or("missing reconnect state parent")?,
    )?;
    std::fs::write(
        &state_path,
        format!(
            "{{\"schema_version\":1,\"session_id\":\"stale-session-id\",\"pane_id\":\"{reused_pane}\",\"session_name\":\"reconnect-reused\",\"recorded_at_unix_ms\":1}}\n"
        ),
    )?;
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&state_path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&state_path, perms)?;
    }

    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args(["reconnect", "--mobile", "--tail", "20"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("target=main") && stdout.contains("MAIN_ID_MISMATCH_READY"),
        "id-mismatched reconnect state should fall back to main:\n{stdout}"
    );
    assert!(
        !stdout.contains("REUSED_ID_MISMATCH_READY"),
        "reconnect must not attach a live pane/name whose session_id differs from the stored pointer:\n{stdout}"
    );

    Ok(())
}

fn normalize_help(help: &str) -> String {
    // Clap wraps help at terminal-dependent widths; collapse whitespace so the
    // smoke contract checks wording rather than a particular render width.
    help.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn attached_client_exits_when_session_kills_itself() -> TestResult {
    let env = TestEnv::new()?;
    let status = env.cmd().arg("list").status()?;
    assert!(status.success());

    let started = Instant::now();
    let output = wait_for_child_output(
        ChildCleanup::new(
            env.cmd()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .args([
                    "new",
                    "--no-status",
                    "-n",
                    "self-kill",
                    "--",
                    "sh",
                    "-lc",
                    "trap '' HUP TERM; echo READY; \"$LTERM_BIN\" kill self-kill; echo AFTER; sleep 30",
                ])
                .spawn()?,
        ),
        Duration::from_secs(2),
        "self-kill attach",
    )?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "self-kill attach did not exit promptly: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("READY"), "{output:?}");
    assert!(
        !stdout.contains("AFTER"),
        "attach kept streaming after self-kill began: {stdout:?}"
    );
    Ok(())
}

#[test]
fn close_kills_session() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "close-session",
            "--",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .status()?;
    assert!(status.success());
    wait_for_session_present(&env, "close-session")?;

    let status = env.cmd().args(["close", "close-session"]).status()?;
    assert!(status.success());
    wait_for_session_absent(&env, "close-session")?;
    Ok(())
}

#[test]
fn kill_alias_closes_session() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "kill-alias",
            "--",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .status()?;
    assert!(status.success());
    wait_for_session_present(&env, "kill-alias")?;

    let status = env.cmd().args(["kill", "kill-alias"]).status()?;
    assert!(status.success());
    wait_for_session_absent(&env, "kill-alias")?;
    Ok(())
}

#[test]
fn concurrent_new_sessions_get_unique_default_panes() -> TestResult {
    let env = TestEnv::new()?;
    let mut children = Vec::new();
    for _ in 0..24 {
        children.push(ChildCleanup::new(
            env.cmd()
                .args(["new", "--detach", "--", "sh", "-lc", "sleep 2"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        ));
    }

    let mut panes = std::collections::HashSet::new();
    for (index, child) in children.into_iter().enumerate() {
        let output = wait_for_child_output(
            child,
            Duration::from_secs(5),
            &format!("concurrent new client {index}"),
        )?;
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let pane = stdout
            .split('\t')
            .nth(1)
            .ok_or_else(|| format!("missing pane id in output: {stdout:?}"))?
            .trim()
            .to_string();
        assert!(panes.insert(pane.clone()), "duplicate pane id: {pane}");
    }
    assert_eq!(panes.len(), 24);
    Ok(())
}

#[test]
fn start_and_new_short_name_list_aliases_work() -> TestResult {
    let env = TestEnv::new()?;

    for (command, name, marker) in [
        ("start", "shorty-start", "SHORTY_START"),
        ("new", "shorty-new", "SHORTY_NEW"),
    ] {
        let script = format!("echo {marker}; sleep 10");
        let status = env
            .cmd()
            .args([command, "--detach", "-n", name, "--", "sh", "-lc", &script])
            .status()?;
        assert!(status.success(), "{command} failed to create {name}");

        for alias in ["sessions", "list", "ls"] {
            let listed = env.cmd().arg(alias).output()?;
            assert!(listed.status.success(), "{alias} failed: {listed:?}");
            let stdout = String::from_utf8_lossy(&listed.stdout);
            assert!(
                stdout.contains(name),
                "{alias} output missing session {name}:\n{stdout}"
            );
        }

        let captured = env.capture_until(name, marker)?;
        assert!(captured.contains(marker), "missing output: {captured}");
    }

    Ok(())
}

#[test]
fn status_theme_is_stored_and_mutable_per_session() -> TestResult {
    let env = TestEnv::new()?;
    let create = env
        .cmd()
        .args([
            "start",
            "--detach",
            "--name",
            "themed",
            "--status-color",
            "yellow",
            "--",
            "sh",
            "-lc",
            "sleep 60",
        ])
        .output()?;
    assert!(create.status.success(), "{create:?}");

    let sessions_json = env.cmd().args(["sessions", "--json"]).output()?;
    assert!(sessions_json.status.success(), "{sessions_json:?}");
    let sessions: serde_json::Value = serde_json::from_slice(&sessions_json.stdout)?;
    let themed = sessions
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.get("name").and_then(serde_json::Value::as_str) == Some("themed"))
        })
        .ok_or_else(|| format!("missing themed session: {sessions:?}"))?;
    assert_eq!(
        themed
            .get("status_theme")
            .and_then(serde_json::Value::as_str),
        Some("amber"),
        "session JSON should expose canonical status theme tokens: {sessions:?}"
    );

    let update = env.cmd().args(["status-theme", "themed", "red"]).output()?;
    assert!(update.status.success(), "{update:?}");
    assert_eq!(
        String::from_utf8_lossy(&update.stdout).trim(),
        "themed	%0	red"
    );

    let invalid = env
        .cmd()
        .args(["status-theme", "themed", "orange"])
        .output()?;
    assert!(!invalid.status.success(), "{invalid:?}");
    assert_stderr_contains(&invalid, "invalid status theme");
    assert_stderr_contains(&invalid, "amber (yellow)");

    let missing = env.cmd().args(["status-theme", "ghost", "red"]).output()?;
    assert!(!missing.status.success(), "{missing:?}");
    assert_stderr_contains(&missing, "no such lterm session or pane");

    let clear = env.cmd().args(["theme", "themed", "default"]).output()?;
    assert!(clear.status.success(), "{clear:?}");
    assert_eq!(
        String::from_utf8_lossy(&clear.stdout).trim(),
        "themed	%0	default"
    );

    let sessions_json = env.cmd().args(["sessions", "--json"]).output()?;
    assert!(sessions_json.status.success(), "{sessions_json:?}");
    let sessions: serde_json::Value = serde_json::from_slice(&sessions_json.stdout)?;
    let themed = sessions
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.get("name").and_then(serde_json::Value::as_str) == Some("themed"))
        })
        .ok_or_else(|| format!("missing themed session after clear: {sessions:?}"))?;
    assert!(
        themed.get("status_theme").is_none(),
        "default theme should be omitted from JSON: {sessions:?}"
    );

    Ok(())
}

#[test]
fn rename_existing_session_updates_targets() -> TestResult {
    let env = TestEnv::new()?;
    let marker = "RENAME_READY";
    let create = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "rename-old",
            "--",
            "sh",
            "-lc",
            &format!("echo {marker}; sleep 60"),
        ])
        .output()?;
    assert!(create.status.success(), "{create:?}");
    let create_stdout = String::from_utf8_lossy(&create.stdout);
    let pane_id = create_stdout
        .split('\t')
        .nth(1)
        .ok_or_else(|| format!("missing pane id in create output: {create_stdout:?}"))?
        .trim()
        .to_string();
    env.capture_until("rename-old", marker)?;

    let sessions_json = env.cmd().args(["sessions", "--json"]).output()?;
    assert!(sessions_json.status.success(), "{sessions_json:?}");
    let sessions: serde_json::Value = serde_json::from_slice(&sessions_json.stdout)?;
    let session_id = sessions
        .as_array()
        .and_then(|rows| {
            rows.iter().find_map(|row| {
                (row.get("name").and_then(serde_json::Value::as_str) == Some("rename-old"))
                    .then(|| row.get("id").and_then(serde_json::Value::as_str))
                    .flatten()
            })
        })
        .ok_or_else(|| format!("missing rename-old session id: {sessions:?}"))?
        .to_string();

    let rename = env
        .cmd()
        .args(["rename", &pane_id, "rename-new"])
        .output()?;
    assert!(rename.status.success(), "{rename:?}");
    assert_eq!(
        String::from_utf8_lossy(&rename.stdout).trim(),
        format!("rename-new\t{pane_id}")
    );

    let idempotent = env
        .cmd()
        .args(["rename", "rename-new", "rename-new"])
        .output()?;
    assert!(idempotent.status.success(), "{idempotent:?}");
    assert_eq!(
        String::from_utf8_lossy(&idempotent.stdout).trim(),
        format!("rename-new\t{pane_id}")
    );

    let rename_by_id = env
        .cmd()
        .args(["rename", &session_id, "rename-final"])
        .output()?;
    assert!(rename_by_id.status.success(), "{rename_by_id:?}");
    assert_eq!(
        String::from_utf8_lossy(&rename_by_id.stdout).trim(),
        format!("rename-final\t{pane_id}")
    );

    let listed = env.cmd().arg("sessions").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(list_row(&stdout, "rename-final").is_some(), "{stdout}");
    assert!(list_row(&stdout, "rename-new").is_none(), "{stdout}");
    assert!(list_row(&stdout, "rename-old").is_none(), "{stdout}");

    let names = session_names_json(&env)?;
    assert!(names.contains("rename-final"), "{names:?}");
    assert!(!names.contains("rename-new"), "{names:?}");
    assert!(!names.contains("rename-old"), "{names:?}");

    let old_logs = env.cmd().args(["logs", "rename-old"]).output()?;
    assert!(!old_logs.status.success(), "{old_logs:?}");
    let previous_logs = env.cmd().args(["logs", "rename-new"]).output()?;
    assert!(!previous_logs.status.success(), "{previous_logs:?}");
    assert!(env.capture_until("rename-final", marker)?.contains(marker));
    assert!(env.capture_until(&pane_id, marker)?.contains(marker));

    let close = env.cmd().args(["close", "rename-final"]).output()?;
    assert!(close.status.success(), "{close:?}");
    wait_for_session_absent(&env, "rename-final")?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn attached_status_line_tracks_external_rename() -> TestResult {
    let env = TestEnv::new()?;
    let marker = "STATUS_RENAME_READY";
    let create = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "status-rename-old",
            "--",
            "sh",
            "-lc",
            &format!("echo {marker}; sleep 60"),
        ])
        .status()?;
    assert!(create.success());
    env.capture_until("status-rename-old", marker)?;

    let (mut master, slave) = open_pty_pair()?;
    set_pty_window_size(&slave, 24, 100)?;
    let stdin = Stdio::from(slave.try_clone()?);
    let stdout = Stdio::from(slave.try_clone()?);
    let stderr = Stdio::from(slave.try_clone()?);
    let mut attach = ChildCleanup::new(
        env.cmd()
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .args(["resume", "status-rename-old", "--raw"])
            .spawn()?,
    );
    drop(slave);

    read_until_marker_bytes(
        &mut master,
        b"lterm  status-rename-old",
        Duration::from_secs(5),
    )?;

    let rename = env
        .cmd()
        .args(["rename", "status-rename-old", "status-rename-new"])
        .output()?;
    assert!(rename.status.success(), "{rename:?}");

    read_until_marker_bytes(
        &mut master,
        b"lterm  status-rename-new",
        Duration::from_secs(6),
    )?;

    attach.kill_and_wait()?;
    let close = env.cmd().args(["close", "status-rename-new"]).status()?;
    assert!(close.success());
    wait_for_session_absent(&env, "status-rename-new")?;
    Ok(())
}

#[test]
fn rename_rejects_conflicts_and_invalid_names_without_mutation() -> TestResult {
    let env = TestEnv::new()?;
    for name in ["rename-keep", "rename-taken"] {
        let created = env
            .cmd()
            .args(["new", "--detach", "-n", name, "--", "sleep", "60"])
            .output()?;
        assert!(
            created.status.success(),
            "failed to create {name}: {created:?}"
        );
    }

    let conflict = env
        .cmd()
        .args(["rename", "rename-keep", "rename-taken"])
        .output()?;
    assert!(!conflict.status.success(), "{conflict:?}");
    assert_stderr_contains(&conflict, ERR_SESSION_EXISTS);

    let invalid = env
        .cmd()
        .args(["rename", "rename-keep", "bad/name"])
        .output()?;
    assert!(!invalid.status.success(), "{invalid:?}");
    assert_stderr_contains(&invalid, ERR_INVALID_SESSION_CHARS);

    let leading_dash = env
        .cmd()
        .args(["rename", "rename-keep", "--", "-bad"])
        .output()?;
    assert!(!leading_dash.status.success(), "{leading_dash:?}");
    assert_stderr_contains(&leading_dash, ERR_LEADING_DASH_NAME);

    for numeric_name in ["0", "123", "007"] {
        let numeric = env
            .cmd()
            .args(["rename", "rename-keep", numeric_name])
            .output()?;
        assert!(!numeric.status.success(), "{numeric:?}");
        assert_stderr_contains(&numeric, ERR_BARE_PANE_ID);
    }

    let listed = env.cmd().arg("sessions").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(list_row(&stdout, "rename-keep").is_some(), "{stdout}");
    assert!(list_row(&stdout, "rename-taken").is_some(), "{stdout}");

    env.cmd().args(["close", "rename-keep"]).status()?;
    env.cmd().args(["close", "rename-taken"]).status()?;
    wait_for_session_absent(&env, "rename-keep")?;
    wait_for_session_absent(&env, "rename-taken")?;
    Ok(())
}

#[test]
fn metadata_history_undo_redo_and_irreversible_purge_are_exact() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "metadata-cli")?;

    let initial = env
        .cmd()
        .args(["metadata", "history", &pane, "--json"])
        .output()?;
    assert!(initial.status.success(), "{initial:?}");
    let initial: serde_json::Value = serde_json::from_slice(&initial.stdout)?;
    let keys = initial
        .as_object()
        .ok_or("metadata history must be an object")?
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        [
            "capacity",
            "current",
            "cursor",
            "entries",
            "pane_id",
            "purge",
            "schema_version",
            "session_id",
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(initial["entries"].as_array().map(Vec::len), Some(0));
    let session_id = initial["session_id"]
        .as_str()
        .ok_or("metadata session_id missing")?
        .to_string();

    let renamed = env
        .cmd()
        .args(["rename", &pane, "metadata-cli-renamed"])
        .output()?;
    assert!(renamed.status.success(), "{renamed:?}");
    let themed = env.cmd().args(["status-theme", &pane, "green"]).output()?;
    assert!(themed.status.success(), "{themed:?}");

    let history = env
        .cmd()
        .args(["metadata", "history", &pane, "--json"])
        .output()?;
    assert!(history.status.success(), "{history:?}");
    let history: serde_json::Value = serde_json::from_slice(&history.stdout)?;
    assert_eq!(history["cursor"], 2);
    assert_eq!(history["entries"].as_array().map(Vec::len), Some(2));
    assert_eq!(history["entries"][0]["operation"], "rename");
    assert_eq!(history["entries"][1]["operation"], "status_theme");
    assert_eq!(history["current"]["name"], "metadata-cli-renamed");
    assert_eq!(history["current"]["status_theme"], "green");
    let encoded = history.to_string();
    for forbidden in [
        "command",
        "cwd",
        "environment",
        "output",
        "scrollback",
        "token",
    ] {
        assert!(!encoded.contains(forbidden), "metadata leaked {forbidden}");
    }

    for expected_cursor in [1_u64, 0] {
        let undo = env.cmd().args(["metadata", "undo", &pane]).output()?;
        assert!(undo.status.success(), "{undo:?}");
        let undo: serde_json::Value = serde_json::from_slice(&undo.stdout)?;
        assert_eq!(undo["direction"], "undo");
        assert_eq!(undo["cursor"], expected_cursor);
    }
    for expected_cursor in [1_u64, 2] {
        let redo = env.cmd().args(["metadata", "redo", &pane]).output()?;
        assert!(redo.status.success(), "{redo:?}");
        let redo: serde_json::Value = serde_json::from_slice(&redo.stdout)?;
        assert_eq!(redo["direction"], "redo");
        assert_eq!(redo["cursor"], expected_cursor);
    }

    let missing_gate = env
        .cmd()
        .args([
            "metadata",
            "purge-history",
            &pane,
            "--session-id",
            &session_id,
        ])
        .output()?;
    assert!(!missing_gate.status.success(), "{missing_gate:?}");
    let wrong_id = "123e4567-e89b-42d3-a456-426614174000".to_string();
    let wrong_uuid = env
        .cmd()
        .args([
            "metadata",
            "purge-history",
            &pane,
            "--irreversible",
            "--session-id",
            &wrong_id,
        ])
        .output()?;
    assert!(!wrong_uuid.status.success(), "{wrong_uuid:?}");

    let purge = env
        .cmd()
        .args([
            "metadata",
            "purge-history",
            &pane,
            "--irreversible",
            "--session-id",
            &session_id,
        ])
        .output()?;
    assert!(purge.status.success(), "{purge:?}");
    let purge: serde_json::Value = serde_json::from_slice(&purge.stdout)?;
    assert_eq!(purge["purged_entries"], 2);
    assert_eq!(purge["current"]["name"], "metadata-cli-renamed");
    assert_eq!(purge["current"]["status_theme"], "green");

    let after = env
        .cmd()
        .args(["metadata", "history", &pane, "--json"])
        .output()?;
    assert!(after.status.success(), "{after:?}");
    let after: serde_json::Value = serde_json::from_slice(&after.stdout)?;
    assert_eq!(after["cursor"], 0);
    assert_eq!(after["entries"].as_array().map(Vec::len), Some(0));
    assert_eq!(after["purge"]["generation"], 1);
    assert_eq!(after["purge"]["purged_entries_total"], 2);

    env.cmd().args(["close", &pane]).status()?;
    wait_for_session_absent(&env, &pane)?;
    Ok(())
}

#[test]
fn child_sessions_are_hidden_from_default_list() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "parent-pane",
            "--",
            "sh",
            "-lc",
            "\"$LTERM_BIN\" new --detach -n child-pane -- sh -lc 'sleep 10' && echo CHILD_READY; sleep 10",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("parent-pane", "CHILD_READY")?;

    let default_list = env.cmd().arg("ls").output()?;
    assert!(default_list.status.success(), "{default_list:?}");
    let stdout = String::from_utf8_lossy(&default_list.stdout);
    assert!(
        stdout.lines().any(|line| line.starts_with("parent-pane\t")),
        "{stdout:?}"
    );
    assert!(
        !stdout.lines().any(|line| line.starts_with("child-pane\t")),
        "child pane should be hidden from default list: {stdout:?}"
    );
    let parent = list_row(&stdout, "parent-pane")
        .ok_or_else(|| format!("parent row missing from default list: {stdout:?}"))?;
    let parent_pane_id = parent
        .get(1)
        .ok_or_else(|| format!("parent row missing pane id: {parent:?}"))?
        .to_string();

    let children = env.cmd().args(["ls", "--children"]).output()?;
    assert!(children.status.success(), "{children:?}");
    let stdout = String::from_utf8_lossy(&children.stdout);
    assert!(
        stdout.lines().any(|line| line.starts_with("child-pane\t")),
        "{stdout:?}"
    );
    assert!(
        !stdout.lines().any(|line| line.starts_with("parent-pane\t")),
        "children list should not include root sessions: {stdout:?}"
    );
    let child = list_row(&stdout, "child-pane")
        .ok_or_else(|| format!("child row missing from children list: {stdout:?}"))?;
    assert_eq!(child.len(), 7, "unexpected child list columns: {child:?}");
    assert_eq!(
        child[6], parent_pane_id,
        "child list should show parent pane id"
    );

    let all = env.cmd().args(["ls", "--all"]).output()?;
    assert!(all.status.success(), "{all:?}");
    let stdout = String::from_utf8_lossy(&all.stdout);
    assert!(
        stdout.lines().any(|line| line.starts_with("parent-pane\t")),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("child-pane\t")),
        "{stdout:?}"
    );

    let tmux_list = env.cmd().args(["tmux-compat", "list-sessions"]).output()?;
    assert!(tmux_list.status.success(), "{tmux_list:?}");
    let stdout = String::from_utf8_lossy(&tmux_list.stdout);
    assert!(
        stdout.lines().any(|line| line == "parent-pane"),
        "tmux compat should include root sessions: {stdout:?}"
    );
    assert!(
        !stdout.lines().any(|line| line == "child-pane"),
        "tmux compat should hide child sessions by default: {stdout:?}"
    );
    Ok(())
}

#[test]
fn terminating_parent_session_terminates_child_sessions() -> TestResult {
    let env = TestEnv::new()?;
    let child_pid_file = env.temp.path().join("child-kill.pid");
    let child_script = env.temp.path().join("child-kill.sh");
    std::fs::write(
        &child_script,
        format!(
            "echo $$ > {}\nsleep 30\n",
            shlex::try_quote(&child_pid_file.display().to_string())?
        ),
    )?;
    let parent_command = format!(
        "\"$LTERM_BIN\" new --detach -n child-kill -- sh {} && echo CHILD_READY; sleep 30",
        shlex::try_quote(&child_script.display().to_string())?
    );
    let status = env
        .cmd()
        .args(["new", "--detach", "-n", "parent-kill", "--", "sh", "-lc"])
        .arg(parent_command)
        .status()?;
    assert!(status.success());
    env.capture_until("parent-kill", "CHILD_READY")?;
    let child_pid = wait_for_file_contents(&child_pid_file)?.trim().to_string();

    let status = env.cmd().args(["kill", "parent-kill"]).status()?;
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last = String::new();
    while Instant::now() < deadline {
        let output = env.cmd().args(["ls", "--all"]).output()?;
        assert!(output.status.success(), "{output:?}");
        last = String::from_utf8_lossy(&output.stdout).to_string();
        if !last.lines().any(|line| line.starts_with("child-kill\t")) {
            wait_for_pid_exit(&child_pid)?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    Err(format!("child session survived parent termination: {last:?}").into())
}

#[test]
fn child_sessions_end_when_parent_exits_naturally() -> TestResult {
    let env = TestEnv::new()?;
    let child_pid_file = env.temp.path().join("child-exit.pid");
    let child_script = env.temp.path().join("child-exit.sh");
    std::fs::write(
        &child_script,
        format!(
            "echo $$ > {}\nsleep 30\n",
            shlex::try_quote(&child_pid_file.display().to_string())?
        ),
    )?;
    let parent_command = format!(
        "\"$LTERM_BIN\" new --detach -n child-exit -- sh {} && echo CHILD_READY; sleep 0.2",
        shlex::try_quote(&child_script.display().to_string())?
    );
    let status = env
        .cmd()
        .args(["new", "--detach", "-n", "parent-exit", "--", "sh", "-lc"])
        .arg(parent_command)
        .status()?;
    assert!(status.success());
    env.capture_until("parent-exit", "CHILD_READY")?;
    let child_pid = wait_for_file_contents(&child_pid_file)?.trim().to_string();

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last = String::new();
    while Instant::now() < deadline {
        let output = env.cmd().args(["ls", "--all"]).output()?;
        assert!(output.status.success(), "{output:?}");
        last = String::from_utf8_lossy(&output.stdout).to_string();
        if !last.lines().any(|line| line.starts_with("parent-exit\t"))
            && !last.lines().any(|line| line.starts_with("child-exit\t"))
        {
            wait_for_pid_exit(&child_pid)?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    Err(format!("child session survived natural parent exit: {last:?}").into())
}

#[test]
fn forged_lterm_pane_without_token_is_not_parented() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "victim-parent",
            "--",
            "sh",
            "-lc",
            "sleep 10",
        ])
        .status()?;
    assert!(status.success());
    let listed = env.cmd().arg("ls").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    let victim = list_row(&stdout, "victim-parent")
        .ok_or_else(|| format!("victim parent row missing: {stdout:?}"))?;
    let victim_pane = victim
        .get(1)
        .ok_or_else(|| format!("victim row missing pane id: {victim:?}"))?;

    let status = env
        .cmd()
        .env("LTERM_PANE", victim_pane)
        .args([
            "new",
            "--detach",
            "-n",
            "spoof-child",
            "--",
            "sh",
            "-lc",
            "sleep 10",
        ])
        .status()?;
    assert!(status.success());

    let default_list = env.cmd().arg("ls").output()?;
    assert!(default_list.status.success(), "{default_list:?}");
    let stdout = String::from_utf8_lossy(&default_list.stdout);
    assert!(
        stdout.lines().any(|line| line.starts_with("spoof-child\t")),
        "spoofed child without token should remain a root session: {stdout:?}"
    );

    let children = env.cmd().args(["ls", "--children"]).output()?;
    assert!(children.status.success(), "{children:?}");
    let stdout = String::from_utf8_lossy(&children.stdout);
    assert!(
        !stdout.lines().any(|line| line.starts_with("spoof-child\t")),
        "spoofed child without token should not be parented: {stdout:?}"
    );
    Ok(())
}

#[test]
fn forged_lterm_parent_token_is_rejected_before_hiding_session() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "token-victim",
            "--",
            "sh",
            "-lc",
            "sleep 10",
        ])
        .status()?;
    assert!(status.success());
    let listed = env.cmd().arg("ls").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    let victim = list_row(&stdout, "token-victim")
        .ok_or_else(|| format!("token victim row missing: {stdout:?}"))?;
    let victim_pane = victim
        .get(1)
        .ok_or_else(|| format!("victim row missing pane id: {victim:?}"))?;

    let output = env
        .cmd()
        .env("LTERM_PANE", victim_pane)
        .env("LTERM_PARENT_TOKEN", "not-the-parent-token")
        .args([
            "new",
            "--detach",
            "-n",
            "fake-token-child",
            "--",
            "sh",
            "-lc",
            "sleep 10",
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "fake parent token should be rejected: {output:?}"
    );

    let all = env.cmd().args(["ls", "--all"]).output()?;
    assert!(all.status.success(), "{all:?}");
    let stdout = String::from_utf8_lossy(&all.stdout);
    assert!(
        !stdout
            .lines()
            .any(|line| line.starts_with("fake-token-child\t")),
        "failed fake-token child should not be listed: {stdout:?}"
    );
    Ok(())
}

#[test]
fn list_shows_attached_state() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "attach-state",
            "--",
            "sh",
            "-lc",
            "sleep 10",
        ])
        .status()?;
    assert!(status.success());

    let detached = env.cmd().arg("ls").output()?;
    assert!(detached.status.success(), "{detached:?}");
    let stdout = String::from_utf8_lossy(&detached.stdout);
    let row = list_row(&stdout, "attach-state")
        .ok_or_else(|| format!("attach-state row missing: {stdout:?}"))?;
    assert_eq!(row.len(), 7, "unexpected list columns: {row:?}");
    assert_eq!(
        row[5], "detached",
        "new detached session should list as detached"
    );
    assert_eq!(row[6], "-", "root session should have no parent");

    let mut attach = env
        .cmd()
        .args(["attach", "attach-state", "--no-status"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let attach_stdin = attach.stdin.take().ok_or("missing attach stdin")?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last = String::new();
    while Instant::now() < deadline {
        let listed = env.cmd().arg("ls").output()?;
        assert!(listed.status.success(), "{listed:?}");
        last = String::from_utf8_lossy(&listed.stdout).to_string();
        if list_row(&last, "attach-state")
            .is_some_and(|row| row.len() == 7 && row[5] == "attached" && row[6] == "-")
        {
            drop(attach_stdin);
            let wait_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < wait_deadline {
                if let Some(status) = attach.try_wait()? {
                    assert!(status.success(), "{status:?}");
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(25));
            }
            let _ = attach.kill();
            let _ = attach.wait();
            return Err("attach did not detach after stdin was closed".into());
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = attach.kill();
    let _ = attach.wait();
    Err(format!("timed out waiting for attached state; last list: {last:?}").into())
}

#[test]
fn attached_new_forwards_utf8_input_bytes() -> TestResult {
    let env = TestEnv::new()?;
    let mut child = env
        .cmd()
        .args([
            "new",
            "--no-status",
            "-n",
            "utf8-input",
            "--",
            "sh",
            "-lc",
            "IFS= read -r line; printf 'GOT:%s\\n' \"$line\"",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().ok_or("missing child stdin")?;
        stdin.write_all("한글 입력\n".as_bytes())?;
    }
    let output = wait_for_child_output(
        ChildCleanup::new(child),
        Duration::from_secs(3),
        "utf8 input child",
    )?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("GOT:한글 입력"), "{stdout:?}");
    Ok(())
}

#[test]
fn pane_ids_reuse_lowest_available_after_kill() -> TestResult {
    let env = TestEnv::new()?;
    let first = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "first-pane",
            "--",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .output()?;
    assert!(first.status.success(), "{first:?}");
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("first-pane\t%0\t"),
        "{first:?}"
    );

    let status = env.cmd().args(["kill", "first-pane"]).status()?;
    assert!(status.success());

    let second = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "second-pane",
            "--",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .output()?;
    assert!(second.status.success(), "{second:?}");
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("second-pane\t%0\t"),
        "{second:?}"
    );
    Ok(())
}

#[test]
fn new_uses_callers_current_directory_by_default() -> TestResult {
    let env = TestEnv::new()?;
    let cwd = env.temp.path().join("caller-cwd");
    std::fs::create_dir(&cwd)?;
    let status = env
        .cmd()
        .current_dir(&cwd)
        .args([
            "new",
            "--detach",
            "-n",
            "cwdtest",
            "--",
            "sh",
            "-lc",
            "pwd; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("cwdtest", &cwd.display().to_string())?;
    assert!(captured.contains(&cwd.display().to_string()), "{captured}");

    let listed = env.cmd().arg("ls").output()?;
    assert!(listed.status.success(), "{listed:?}");
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(&cwd.display().to_string()),
        "{listed:?}"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn explicit_cwd_works_when_callers_current_directory_was_removed() -> TestResult {
    let env = TestEnv::new()?;
    let removed = env.temp.path().join("removed-cwd");
    let target = env.temp.path().join("explicit-cwd");
    std::fs::create_dir(&removed)?;
    std::fs::create_dir(&target)?;

    let output = Command::new("sh")
        .env("LTERM_RUNTIME_DIR", env.temp.path().join("run"))
        .env("LTERM_DATA_DIR", env.temp.path().join("data"))
        .env_remove("LTERM_SOCKET")
        .env_remove("LTERM_SESSION")
        .env_remove("LTERM_PANE")
        .env_remove("LTERM_PARENT_TOKEN")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("LTERM_BIN", env!("CARGO_BIN_EXE_lterm"))
        .env("REMOVED_CWD", &removed)
        .env("TARGET_CWD", &target)
        .arg("-c")
        .arg(
            "cd \"$REMOVED_CWD\" && rmdir \"$REMOVED_CWD\" && \
             exec \"$LTERM_BIN\" new --detach -n explicit-cwd --cwd \"$TARGET_CWD\" -- sh -lc 'pwd; sleep 2'",
        )
        .output()?;
    assert!(output.status.success(), "{output:?}");

    let target = target.display().to_string();
    let captured = env.capture_until("explicit-cwd", &target)?;
    assert!(captured.contains(&target), "{captured}");
    Ok(())
}

#[test]
fn tmux_compat_send_keys_reaches_pty() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "keys",
            "echo READY; read first; echo GOT:$first; read second; echo GOT_COMPACT:$second; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    env.capture_until("keys", "READY")?;
    let status = env
        .cmd()
        .args(["tmux-compat", "send-keys", "-t", "keys", "hello", "C-m"])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("keys", "GOT:hello")?;
    assert!(captured.contains("READY"), "{captured}");
    assert!(captured.contains("GOT:hello"), "{captured}");
    let status = env
        .cmd()
        .args(["tmux-compat", "send-keys", "-tkeys", "compact", "C-m"])
        .status()?;
    assert!(status.success());
    let captured = env.capture_until("keys", "GOT_COMPACT:compact")?;
    assert!(captured.contains("GOT_COMPACT:compact"), "{captured}");
    Ok(())
}

#[test]
fn tmux_compat_send_keys_skips_repeat_count_before_target() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "keys-repeat",
            "echo READY_REPEAT; read line; echo GOT_REPEAT:$line; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    env.capture_until("keys-repeat", "READY_REPEAT")?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "send-keys",
            "-N",
            "1",
            "-t",
            "keys-repeat",
            "repeat",
            "C-m",
        ])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("keys-repeat", "GOT_REPEAT:repeat")?;
    assert!(captured.contains("GOT_REPEAT:repeat"), "{captured}");
    Ok(())
}

#[test]
fn tmux_compat_send_keys_repeats_with_repeat_count() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "keys-repeat-count",
            "echo READY_REPEAT_COUNT; for idx in 1 2 3; do read line; echo GOT_REPEAT_${idx}:$line; done; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    env.capture_until("keys-repeat-count", "READY_REPEAT_COUNT")?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "send-keys",
            "-N",
            "3",
            "-t",
            "keys-repeat-count",
            "repeat",
            "C-m",
        ])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("keys-repeat-count", "GOT_REPEAT_3:repeat")?;
    assert!(captured.contains("GOT_REPEAT_1:repeat"), "{captured}");
    assert!(captured.contains("GOT_REPEAT_2:repeat"), "{captured}");
    assert!(captured.contains("GOT_REPEAT_3:repeat"), "{captured}");
    Ok(())
}

#[test]
fn tmux_compat_split_window_supports_clustered_print_format() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "split-window",
            "-dPF",
            "#{pane_id}",
            "echo SPLIT_CLUSTER_READY; sleep 2",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        pane.starts_with('%'),
        "split-window -dPF should print the pane id, got {pane:?}"
    );
    let captured = env.capture_until(&pane, "SPLIT_CLUSTER_READY")?;
    assert!(captured.contains("SPLIT_CLUSTER_READY"), "{captured}");
    Ok(())
}

#[test]
fn tmux_compat_split_window_print_format_suppresses_cmux_noise() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-noisy.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:source\"}}}}'; exit 0 ;;\n\
               new-split) printf '%s\\n' 'OK surface:42 workspace:1'; exit 0 ;;\n\
               send) printf '%s\\n' 'OK noisy send output'; exit 0 ;;\n\
               close-surface) printf '%s\\n' 'OK noisy close output'; exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let shell = command_path("sh")?.display().to_string();
    let sleep = shlex::try_quote(&command_path("sleep")?.display().to_string())?.into_owned();
    let payload = format!("echo SPLIT_NOISY_READY; {sleep} 2");

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace-for-noisy-cmux")
        .env("PATH", &path)
        .args([
            "tmux-compat",
            "split-window",
            "-hPF",
            "#{pane_id}",
            shell.as_str(),
            "-lc",
            payload.as_str(),
        ])
        .output()?;
    assert!(
        output.status.success(),
        "split-window with noisy cmux shim should succeed: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pane = stdout.trim();
    assert!(
        pane.starts_with('%') && !pane.contains("OK noisy"),
        "split-window -P stdout should only contain the requested format, got {stdout:?}"
    );
    let captured = env.capture_until(pane, "SPLIT_NOISY_READY")?;
    assert!(captured.contains("SPLIT_NOISY_READY"), "{captured}");
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| line.starts_with(
            "send --surface surface:42 --workspace workspace:1 exec env LTERM_CMUX_MANAGED_ATTACH=1 "
        )),
        "managed attach command should target the new-split surface from stdout: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_targets_live_focused_cmux_context() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-live-context.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"caller\":{{\"surface_ref\":\"surface:stale\",\"workspace_ref\":\"workspace:stale\",\"window_ref\":\"window:stale\"}},\"focused\":{{\"surface_ref\":\"surface:focused\",\"workspace_ref\":\"workspace:focused\",\"window_ref\":\"window:focused\"}}}}'; exit 0 ;;\n\
               new-split)\n\
                 if [ \"$*\" != 'new-split right --surface surface:focused --workspace workspace:focused --window window:focused --focus true' ]; then\n\
                   printf 'unexpected split args: %s\\n' \"$*\" >&2\n\
                   exit 64\n\
                 fi\n\
                 printf '%s\\n' 'OK surface:created workspace:focused'\n\
                 exit 0 ;;\n\
               send)\n\
                 case \"$*\" in\n\
                   'send --surface surface:created --workspace workspace:focused --window window:focused exec env LTERM_CMUX_MANAGED_ATTACH=1 '*) exit 0 ;;\n\
                   *) printf 'unexpected send args: %s\\n' \"$*\" >&2; exit 65 ;;\n\
                 esac ;;\n\
               close-surface) exit 0 ;;\n\
               *) printf 'unexpected command: %s\\n' \"$*\" >&2; exit 66 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let shell = command_path("sh")?.display().to_string();
    let sleep = shlex::try_quote(&command_path("sleep")?.display().to_string())?.into_owned();
    let env_dump = env.temp.path().join("split-live-context-env.txt");
    let env_dump_tmp = env.temp.path().join("split-live-context-env.tmp");
    let env_dump_arg = shlex::try_quote(&env_dump.display().to_string())?.into_owned();
    let env_dump_tmp_arg = shlex::try_quote(&env_dump_tmp.display().to_string())?.into_owned();
    let payload = format!(
        "env > {env_dump_tmp_arg}; mv {env_dump_tmp_arg} {env_dump_arg}; echo SPLIT_LIVE_CONTEXT_READY; {sleep} 2"
    );

    let output = env
        .cmd()
        .env("CMUX_SURFACE_ID", "surface:stale")
        .env("CMUX_WORKSPACE_ID", "workspace:stale")
        .env("CMUX_SOCKET_PATH", "/tmp/cmux-socket-current.sock")
        .env("PATH", &path)
        .args([
            "tmux-compat",
            "split-window",
            "-hPF",
            "#{pane_id}",
            shell.as_str(),
            "-lc",
            payload.as_str(),
        ])
        .output()?;
    assert!(
        output.status.success(),
        "split-window should ignore stale cmux env and use live focused context: {output:?}"
    );
    let pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(pane.starts_with('%'), "expected pane id, got {pane:?}");
    let captured = env.capture_until(&pane, "SPLIT_LIVE_CONTEXT_READY")?;
    assert!(captured.contains("SPLIT_LIVE_CONTEXT_READY"), "{captured}");
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| {
            line == "new-split right --surface surface:focused --workspace workspace:focused --window window:focused --focus true"
        }),
        "split should target live focused cmux context, not stale env: {cmux_calls:?}"
    );
    assert!(
        cmux_calls.lines().any(|line| {
            line.starts_with(
                "send --surface surface:created --workspace workspace:focused --window window:focused exec env LTERM_CMUX_MANAGED_ATTACH=1 ",
            )
        }),
        "attach send should preserve created surface plus inherited workspace/window context: {cmux_calls:?}"
    );
    assert!(
        !cmux_calls.contains("surface:stale"),
        "stale cmux env refs must not be used: {cmux_calls:?}"
    );
    let child_env = wait_for_file_contents(&env_dump)?;
    assert!(
        child_env
            .lines()
            .any(|line| line == "CMUX_SURFACE_ID=surface:created"),
        "split child should inherit created cmux surface, not stale caller env: {child_env}"
    );
    assert!(
        child_env
            .lines()
            .any(|line| line == "CMUX_WORKSPACE_ID=workspace:focused"),
        "split child should inherit created cmux workspace: {child_env}"
    );
    assert!(
        child_env
            .lines()
            .any(|line| line == "CMUX_WINDOW_ID=window:focused"),
        "split child should inherit created cmux window fallback from focused source: {child_env}"
    );
    assert!(
        child_env
            .lines()
            .any(|line| line == "CMUX_SOCKET_PATH=/tmp/cmux-socket-current.sock"),
        "split child should preserve the current cmux socket path: {child_env}"
    );
    assert!(
        !child_env.contains("surface:stale") && !child_env.contains("workspace:stale"),
        "split child must not inherit stale caller cmux context: {child_env}"
    );
    assert!(
        !child_env
            .lines()
            .any(|line| line.starts_with("LTERM_CMUX_MANAGED_ATTACH=")),
        "split child must not inherit private managed attach marker: {child_env}"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_backward_flag_maps_cmux_direction() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-backward-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-backward-direction.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:focused\",\"workspace_ref\":\"workspace:focused\",\"window_ref\":\"window:focused\"}}}}'; exit 0 ;;\n\
               new-split)\n\
                 case \"$2\" in\n\
                   left|up) printf 'OK surface:%s workspace:focused window:focused\\n' \"$2\"; exit 0 ;;\n\
                   *) printf 'unexpected split direction: %s\\n' \"$*\" >&2; exit 64 ;;\n\
                 esac ;;\n\
               send) exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) printf 'unexpected command: %s\\n' \"$*\" >&2; exit 66 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let shell = command_path("sh")?.display().to_string();

    for (flags, expected_direction, marker) in [
        ("-bhPF", "left", "SPLIT_BACKWARD_LEFT_READY"),
        ("-bvPF", "up", "SPLIT_BACKWARD_UP_READY"),
    ] {
        let output = env
            .cmd()
            .env("CMUX_WORKSPACE_ID", "workspace:focused")
            .env("PATH", &path)
            .args([
                "tmux-compat",
                "split-window",
                flags,
                "#{pane_id}",
                shell.as_str(),
                "-lc",
                &format!("echo {marker}; sleep 2"),
            ])
            .output()?;
        assert!(
            output.status.success(),
            "split-window {flags} should map -b to {expected_direction}: {output:?}"
        );
        let pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert!(pane.starts_with('%'), "expected pane id, got {pane:?}");
        let captured = env.capture_until(&pane, marker)?;
        assert!(captured.contains(marker), "{captured}");
    }

    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| {
            line == "new-split left --surface surface:focused --workspace workspace:focused --window window:focused --focus true"
        }),
        "-b -h should request a left cmux split: {cmux_calls:?}"
    );
    assert!(
        cmux_calls.lines().any(|line| {
            line == "new-split up --surface surface:focused --workspace workspace:focused --window window:focused --focus true"
        }),
        "-b -v should request an up cmux split: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_duplicate_closes_caller_not_focused_surface() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-caller-not-focused")?;
    seed_managed_attach_store(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-caller-not-focused.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {}
case "$1" in
  identify) printf '%s\n' '{{"caller":{{"surface_ref":"surface:duplicate","workspace_ref":"workspace:1","window_ref":"window:1"}},"focused":{{"surface_ref":"surface:owner","workspace_ref":"workspace:1","window_ref":"window:1"}}}}'; exit 0 ;;
  close-surface) exit 0 ;;
  *) exit 0 ;;
esac
"#,
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "duplicate managed attach should exit cleanly after closing caller surface: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    let close_surface_calls = cmux_calls
        .lines()
        .filter(|line| line.starts_with("close-surface"))
        .collect::<Vec<_>>();
    assert_eq!(
        close_surface_calls,
        vec!["close-surface --surface surface:duplicate --workspace workspace:1 --window window:1"],
        "managed duplicate cleanup must close the caller surface, not focused/owner: {cmux_calls:?}"
    );
    assert!(
        close_surface_calls
            .iter()
            .all(|line| !line.contains("surface:owner")),
        "focused owner surface must never be closed by duplicate cleanup: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_focused_without_caller_proceeds_without_close() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-focused-no-caller")?;
    seed_managed_attach_store(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-focused-no-caller.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {}
case "$1" in
  identify) printf '%s\n' '{{"focused":{{"surface_ref":"surface:owner","workspace_ref":"workspace:1","window_ref":"window:1"}}}}'; exit 0 ;;
  close-surface) printf 'focused-only identify must not close\n' >&2; exit 70 ;;
  *) exit 0 ;;
esac
"#,
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "focused-only identify should fall back to normal attach: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("SESSION_READY:managed-focused-no-caller"),
        "focused-only fallback should replay the session output: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .all(|line| !line.starts_with("close-surface")),
        "focused-only identify must not trigger cmux close-surface: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_duplicate_exits_and_closes_current_surface() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-duplicate")?;
    seed_managed_attach_store(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-duplicate.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"caller\":{{\"surface_ref\":\"surface:duplicate\",\"workspace_ref\":\"workspace:1\",\"window_ref\":\"window:1\"}}}}'; exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "duplicate managed attach should exit cleanly: {output:?}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("SESSION_READY:managed-duplicate"),
        "duplicate managed attach must not enter the PTY stream: {output:?}"
    );
    assert!(
        session_names_json(&env)?.contains("managed-duplicate"),
        "duplicate managed attach must not kill the underlying session"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    let close_surface_calls = cmux_calls
        .lines()
        .filter(|line| line.starts_with("close-surface"))
        .collect::<Vec<_>>();
    assert_eq!(
        close_surface_calls,
        vec!["close-surface --surface surface:duplicate --workspace workspace:1 --window window:1"],
        "duplicate should close exactly its current cmux surface and never the owner: {cmux_calls:?}"
    );
    assert!(
        close_surface_calls
            .iter()
            .all(|line| !line.contains("surface:owner")),
        "duplicate must not close the stored owner surface: {cmux_calls:?}"
    );
    let owner_lease = managed_attach_entry(&env, &pane)?.expect("owner lease should remain");
    assert_eq!(
        owner_lease.get("token").and_then(serde_json::Value::as_str),
        Some("seed-owner"),
        "duplicate attach must not overwrite or release the owner lease: {owner_lease}"
    );
    assert_eq!(
        owner_lease
            .get("cmux_surface_id")
            .and_then(serde_json::Value::as_str),
        Some("surface:owner"),
        "duplicate attach must preserve the owner surface lease: {owner_lease}"
    );
    assert_eq!(
        managed_attach_count(&env)?,
        1,
        "duplicate attach must not leave an extra transient lease"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_close_failure_returns_error() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-close-fail")?;
    seed_managed_attach_store(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-close-fail.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {}
case "$1" in
  identify) printf '%s\n' '{{"caller":{{"surface_ref":"surface:duplicate","workspace_ref":"workspace:1"}}}}'; exit 0 ;;
  close-surface) printf 'close failed\n' >&2; exit 72 ;;
  *) exit 0 ;;
esac
"#,
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        !output.status.success(),
        "close-surface failure should propagate instead of reporting a clean detach: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("cmux managed duplicate close-surface failed"),
        "stderr should explain duplicate close failure: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .any(|line| line.starts_with("close-surface --surface surface:duplicate")),
        "duplicate surface close should have been attempted: {cmux_calls:?}"
    );
    let owner_lease = managed_attach_entry(&env, &pane)?.expect("owner lease should remain");
    assert_eq!(
        owner_lease.get("token").and_then(serde_json::Value::as_str),
        Some("seed-owner"),
        "failed duplicate close must not overwrite or release the owner lease: {owner_lease}"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_top_level_identity_proceeds_without_close() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-top-level-identity")?;
    seed_managed_attach_store(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-top-level-identity.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {}
case "$1" in
  identify) printf '%s\n' '{{"surface_ref":"surface:ambiguous","workspace_ref":"workspace:1"}}'; exit 0 ;;
  close-surface) printf 'top-level identity must not close\n' >&2; exit 70 ;;
  *) exit 0 ;;
esac
"#,
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "top-level-only identify should fall back to normal attach: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("SESSION_READY:managed-top-level-identity"),
        "top-level-only fallback should replay session output: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .all(|line| !line.starts_with("close-surface")),
        "ambiguous top-level identity must not trigger cmux close-surface: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_identify_none_proceeds_without_managed_cleanup() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-identify-none")?;
    seed_managed_attach_store(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-identify-none.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{}}'; exit 0 ;;\n\
               close-surface) printf 'close should not run without current surface identity\\n' >&2; exit 70 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "managed attach without current cmux identity should fall back to normal attach: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("SESSION_READY:managed-identify-none"),
        "fallback attach should replay the session output: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .all(|line| !line.starts_with("close-surface")),
        "unknown current surface identity must not trigger cmux close-surface: {cmux_calls:?}"
    );
    let owner_lease = managed_attach_entry(&env, &pane)?.expect("seed owner lease should remain");
    assert_eq!(
        owner_lease.get("token").and_then(serde_json::Value::as_str),
        Some("seed-owner"),
        "fallback attach must not overwrite existing managed owner lease: {owner_lease}"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_malformed_current_surface_proceeds_without_close() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-malformed")?;
    seed_managed_attach_store(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-malformed.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"--not-safe\"}}}}'; exit 0 ;;\n\
               close-surface) printf 'unsafe close should not run\\n' >&2; exit 70 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "malformed current cmux ref should fall back to normal attach: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("SESSION_READY:managed-malformed"),
        "malformed-identity fallback should replay the session output: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .all(|line| !line.starts_with("close-surface")),
        "malformed current refs must not be passed to cmux close-surface: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_live_pid_identity_mismatch_replaces_without_close() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-live-pid-reused")?;
    seed_managed_attach_store_with_token_and_pid(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
        "seed-owner",
        std::process::id(),
    )?;
    override_managed_attach_process_start_id(
        &env,
        &pane,
        Some("definitely-not-this-process-start"),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-live-pid-reused.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {}
case "$1" in
  identify) printf '%s\n' '{{"caller":{{"surface_ref":"surface:replacement","workspace_ref":"workspace:1"}}}}'; exit 0 ;;
  close-surface) printf 'reused pid must not close duplicate\n' >&2; exit 70 ;;
  *) exit 0 ;;
esac
"#,
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "live PID with mismatched start identity should be treated as stale: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("SESSION_READY:managed-live-pid-reused"),
        "identity-mismatch replacement should replay the session output: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .all(|line| !line.starts_with("close-surface")),
        "PID reuse identity mismatch must not trigger duplicate close: {cmux_calls:?}"
    );
    assert_eq!(
        managed_attach_count(&env)?,
        0,
        "replacement attach should claim and release after normal detach"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_unknown_live_owner_surface_replaces_without_close() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-unknown-live-owner")?;
    seed_managed_attach_store_with_token_and_pid(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        None,
        "seed-owner",
        std::process::id(),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-unknown-live-owner.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {}
case "$1" in
  identify) printf '%s\n' '{{"caller":{{"surface_ref":"surface:replacement","workspace_ref":"workspace:1"}}}}'; exit 0 ;;
  close-surface) printf 'owner unknown; close should not run\n' >&2; exit 70 ;;
  *) exit 0 ;;
esac
"#,
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "live owner without a known surface should be replaced by a normally-detaching attach: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("SESSION_READY:managed-unknown-live-owner"),
        "unknown-live-owner replacement should replay the session output: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .all(|line| !line.starts_with("close-surface")),
        "unknown live owner surface must not permit closing current surface: {cmux_calls:?}"
    );
    assert_eq!(
        managed_attach_count(&env)?,
        0,
        "replacement attach should claim and release its own lease after normal detach"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_unknown_owner_surface_replaces_without_close() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-unknown-owner")?;
    seed_managed_attach_store_with_token_and_pid(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        None,
        "seed-owner",
        dead_test_pid(),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-unknown-owner.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"caller\":{{\"surface_ref\":\"surface:maybe-owner\",\"workspace_ref\":\"workspace:1\"}}}}'; exit 0 ;;\n\
               close-surface) printf 'owner unknown; close should not run\\n' >&2; exit 70 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "unknown owner surface should be replaced by a normally-detaching attach: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("SESSION_READY:managed-unknown-owner"),
        "unknown-owner replacement should replay the session output: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .all(|line| !line.starts_with("close-surface")),
        "unknown owner surface must not permit closing current surface: {cmux_calls:?}"
    );
    assert_eq!(
        managed_attach_count(&env)?,
        0,
        "replacement attach should claim and release its own lease after normal detach"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_stale_live_owner_still_suppresses_duplicate() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-stale-live-owner")?;
    seed_managed_attach_store_with_token_and_pid(
        &env,
        &pane,
        1,
        Some("surface:owner"),
        "seed-owner",
        std::process::id(),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-stale-live-owner.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {}
case "$1" in
               identify) printf '%s\n' '{{"caller":{{"surface_ref":"--not-safe"}},"current":{{"surface_ref":"surface:duplicate","workspace_ref":"workspace:1","window_ref":"window:1"}}}}'; exit 0 ;;
  close-surface) exit 0 ;;
  *) exit 0 ;;
esac
"#,
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "stale but live owner should suppress duplicate and close duplicate surface: {output:?}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("SESSION_READY:managed-stale-live-owner"),
        "duplicate attach must not enter PTY stream when stale owner process is still live: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| {
            line == "close-surface --surface surface:duplicate --workspace workspace:1 --window window:1"
        }),
        "stale but live owner should close only the duplicate caller surface: {cmux_calls:?}"
    );
    let owner_lease = managed_attach_entry(&env, &pane)?.expect("owner lease should remain");
    assert_eq!(
        owner_lease.get("token").and_then(serde_json::Value::as_str),
        Some("seed-owner"),
        "stale but live owner lease must remain active: {owner_lease}"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_fresh_identityless_live_owner_suppresses_duplicate() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-fresh-identityless-live-owner")?;
    seed_identityless_managed_attach_store_with_token_and_pid(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
        "seed-owner",
        std::process::id(),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env
        .temp
        .path()
        .join("cmux-managed-fresh-identityless-live-owner.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> {}
case "$1" in
  identify) printf '%s\n' '{{"caller":{{"surface_ref":"surface:duplicate","workspace_ref":"workspace:1","window_ref":"window:1"}}}}'; exit 0 ;;
  close-surface) exit 0 ;;
  *) exit 0 ;;
esac
"#,
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "fresh identityless live owner should suppress duplicate and close duplicate surface: {output:?}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout)
            .contains("SESSION_READY:managed-fresh-identityless-live-owner"),
        "duplicate attach must not enter PTY stream when fresh identityless owner process is still live: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| {
            line == "close-surface --surface surface:duplicate --workspace workspace:1 --window window:1"
        }),
        "fresh identityless live owner should close only the duplicate caller surface: {cmux_calls:?}"
    );
    let owner_lease = managed_attach_entry(&env, &pane)?.expect("owner lease should remain");
    assert_eq!(
        owner_lease.get("token").and_then(serde_json::Value::as_str),
        Some("seed-owner"),
        "fresh identityless live owner lease must remain active: {owner_lease}"
    );
    assert!(
        owner_lease
            .get("process_start_id")
            .is_none_or(serde_json::Value::is_null),
        "test fixture should exercise identityless legacy/unsupported leases: {owner_lease}"
    );
    Ok(())
}

#[test]
fn managed_cmux_attach_stale_lease_allows_attach_and_releases() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-stale")?;
    seed_managed_attach_store_with_token_and_pid(
        &env,
        &pane,
        1,
        Some("surface:stale-owner"),
        "seed-owner",
        dead_test_pid(),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-managed-stale.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"caller\":{{\"surface_ref\":\"surface:fresh\",\"workspace_ref\":\"workspace:1\"}}}}'; exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "stale managed attach lease should not block attach: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("SESSION_READY:managed-stale"),
        "stale lease replacement should replay the session output: {output:?}"
    );
    assert_eq!(
        managed_attach_count(&env)?,
        0,
        "accepted attach should release its matching lease on normal detach"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .all(|line| !line.starts_with("close-surface")),
        "stale lease replacement must not close a fresh owner: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn unmarked_attach_is_not_managed_even_with_active_lease() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "managed-unmarked")?;
    seed_managed_attach_store(
        &env,
        &pane,
        fresh_managed_attach_timestamp(),
        Some("surface:owner"),
    )?;

    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-unmarked.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:manual\"}}}}'; exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .args(["attach", pane.as_str(), "--no-status"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        output.status.success(),
        "manual unmarked attach should keep normal raw attach behavior: {output:?}"
    );
    assert!(
        !cmux_log.exists(),
        "unmarked attach must not consult cmux managed-attach guard"
    );
    let owner_lease = managed_attach_entry(&env, &pane)?.expect("owner lease should remain");
    assert_eq!(
        owner_lease.get("token").and_then(serde_json::Value::as_str),
        Some("seed-owner"),
        "unmarked attach must not alter the managed owner lease: {owner_lease}"
    );
    assert_eq!(
        owner_lease
            .get("cmux_surface_id")
            .and_then(serde_json::Value::as_str),
        Some("surface:owner"),
        "unmarked attach must preserve owner surface metadata: {owner_lease}"
    );
    Ok(())
}

#[test]
fn plain_new_scrubs_ambient_tmux_and_cmux_environment() -> TestResult {
    let env = TestEnv::new()?;
    let env_dump = env.temp.path().join("plain-env.txt");
    let env_dump_tmp = env.temp.path().join("plain-env.tmp");
    let env_dump_arg = shlex::try_quote(&env_dump.display().to_string())?.into_owned();
    let env_dump_tmp_arg = shlex::try_quote(&env_dump_tmp.display().to_string())?.into_owned();
    let script = format!("env > {env_dump_tmp_arg}; mv {env_dump_tmp_arg} {env_dump_arg}; sleep 1");

    let output = env
        .cmd()
        .env("TMUX", "/tmp/real-tmux,1,2")
        .env("TMUX_PANE", "%real")
        .env("tmux", "/tmp/lower-tmux,3,4")
        .env("TmUx_PaNe", "%mixed")
        .env("CMUX_WORKSPACE_ID", "workspace:ambient")
        .env("CMUX_SURFACE_ID", "surface:ambient")
        .env("CMUX_WINDOW_ID", "window:ambient")
        .env("CMUX_SOCKET_PATH", "/tmp/cmux-ambient.sock")
        .env("cmux_workspace_id", "workspace:lower")
        .env("Cmux_SURFACE_ID", "surface:mixed")
        .env("cmux_socket_path", "/tmp/cmux-lower.sock")
        .env("CMUX_EXTRA_CONTEXT", "extra:ambient")
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args([
            "new",
            "--detach",
            "-n",
            "plain-env",
            "--",
            "sh",
            "-lc",
            script.as_str(),
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");

    let contents = wait_for_file_contents(&env_dump)?;
    assert!(
        !contents.lines().any(|line| line.starts_with("TMUX=")),
        "plain lterm new must not leak ambient TMUX: {contents}"
    );
    assert!(
        !contents.lines().any(|line| line.starts_with("TMUX_PANE=")),
        "plain lterm new must not leak ambient TMUX_PANE: {contents}"
    );
    assert!(
        !contents.lines().any(|line| {
            let key = line.split_once('=').map_or(line, |(key, _)| key);
            key.eq_ignore_ascii_case("TMUX") || key.eq_ignore_ascii_case("TMUX_PANE")
        }),
        "plain lterm new must not leak ambient TMUX keys with non-canonical casing: {contents}"
    );
    assert!(
        !contents.lines().any(|line| line.starts_with("CMUX_")),
        "plain lterm new must not leak ambient CMUX context: {contents}"
    );
    assert!(
        !contents.lines().any(|line| {
            let bytes = line.as_bytes();
            bytes.len() >= 5 && bytes[..5].eq_ignore_ascii_case(b"CMUX_")
        }),
        "plain lterm new must not leak ambient CMUX context with non-canonical casing: {contents}"
    );
    assert!(
        !contents
            .lines()
            .any(|line| line.starts_with("LTERM_CMUX_MANAGED_ATTACH=")),
        "plain lterm new must not leak private managed attach marker: {contents}"
    );
    Ok(())
}

#[test]
fn tmux_enabled_new_gets_fake_tmux_and_current_cmux_context() -> TestResult {
    let env = TestEnv::new()?;
    let env_dump = env.temp.path().join("tmux-env.txt");
    let env_dump_tmp = env.temp.path().join("tmux-env.tmp");
    let env_dump_arg = shlex::try_quote(&env_dump.display().to_string())?.into_owned();
    let env_dump_tmp_arg = shlex::try_quote(&env_dump_tmp.display().to_string())?.into_owned();
    let script = format!("env > {env_dump_tmp_arg}; mv {env_dump_tmp_arg} {env_dump_arg}; sleep 1");

    let output = env
        .cmd()
        .env("TMUX", "/tmp/real-tmux,1,2")
        .env("TMUX_PANE", "%real")
        .env("tmux", "/tmp/lower-tmux,3,4")
        .env("TmUx_PaNe", "%mixed")
        .env("CMUX_WORKSPACE_ID", "workspace:current")
        .env("CMUX_SURFACE_ID", "surface:current")
        .env("CMUX_WINDOW_ID", "window:current")
        .env("CMUX_SOCKET_PATH", "/tmp/cmux-current.sock")
        .env("cmux_workspace_id", "workspace:lower")
        .env("Cmux_SURFACE_ID", "surface:mixed")
        .env("cmux_socket_path", "/tmp/cmux-lower.sock")
        .env("CMUX_EXTRA_CONTEXT", "extra:current")
        .env("LTERM_CMUX_MANAGED_ATTACH", "1")
        .args([
            "new",
            "--tmux",
            "--detach",
            "-n",
            "tmux-env",
            "--",
            "sh",
            "-lc",
            script.as_str(),
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");

    let contents = wait_for_file_contents(&env_dump)?;
    assert!(
        contents.lines().any(|line| {
            line.starts_with("TMUX=")
                && line.contains("lterm.sock")
                && !line.contains("/tmp/real-tmux")
                && !line.contains("/tmp/lower-tmux")
        }),
        "tmux-enabled session should get lterm fake TMUX, not the ambient real one: {contents}"
    );
    assert!(
        !contents
            .lines()
            .any(|line| { line.starts_with("tmux=") || line.starts_with("TmUx_PaNe=") }),
        "tmux-enabled session should not inherit non-canonical ambient TMUX keys: {contents}"
    );
    assert!(
        contents.lines().any(|line| line == "TMUX_PANE=%0"),
        "tmux-enabled session should get its lterm pane id: {contents}"
    );
    assert!(
        contents
            .lines()
            .any(|line| line == "CMUX_WORKSPACE_ID=workspace:current"),
        "tmux-enabled session should inherit the current client CMUX context: {contents}"
    );
    assert!(
        contents
            .lines()
            .any(|line| line == "CMUX_SURFACE_ID=surface:current"),
        "tmux-enabled session should inherit the current client CMUX surface: {contents}"
    );
    assert!(
        contents
            .lines()
            .any(|line| line == "CMUX_WINDOW_ID=window:current"),
        "tmux-enabled session should inherit the current client CMUX window: {contents}"
    );
    assert!(
        contents
            .lines()
            .any(|line| line == "CMUX_SOCKET_PATH=/tmp/cmux-current.sock"),
        "tmux-enabled session should inherit the current client CMUX socket: {contents}"
    );
    assert!(
        !contents
            .lines()
            .any(|line| line.starts_with("CMUX_EXTRA_CONTEXT=")),
        "tmux-enabled session should not inherit unallowlisted ambient CMUX variables: {contents}"
    );
    assert!(
        !contents
            .lines()
            .any(|line| line.starts_with("cmux_") || line.starts_with("Cmux_")),
        "tmux-enabled session must not inherit non-canonical lowercase/mixed-case cmux variables: {contents}"
    );
    assert!(
        !contents
            .lines()
            .any(|line| line.starts_with("LTERM_CMUX_MANAGED_ATTACH=")),
        "tmux-enabled session must not leak private managed attach marker: {contents}"
    );
    Ok(())
}

#[test]
fn prompt_time_visible_split_semantics_are_not_globally_hidden() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-prompt-time-visible.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:source\",\"workspace_ref\":\"workspace:1\"}}}}'; exit 0 ;;\n\
               new-split) printf '%s\\n' 'OK surface:prompt workspace:1'; exit 0 ;;\n\
               send) exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let shell = command_path("sh")?.display().to_string();

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace:1")
        .env("PATH", &path)
        .args([
            "tmux-compat",
            "split-window",
            "-hPF",
            "#{pane_id}",
            shell.as_str(),
            "-lc",
            "sleep 1",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "visible split-window should still succeed: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| {
            line == "new-split right --surface surface:source --workspace workspace:1 --focus true"
        }),
        "non-detached split-window should create the exact visible cmux split without hidden/no-focus flags: {cmux_calls:?}"
    );
    assert!(
        cmux_calls.lines().any(|line| {
            line.starts_with(
                "send --surface surface:prompt --workspace workspace:1 exec env LTERM_CMUX_MANAGED_ATTACH=1 ",
            )
        }),
        "visible split should send a marker-bearing managed attach: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_kill_pane_closes_visible_cmux_split_surface() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-visible-kill.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:source\",\"workspace_ref\":\"workspace:1\",\"window_ref\":\"window:main\"}}}}'; exit 0 ;;\n\
               new-split) printf '%s\\n' 'OK surface:visible workspace:1 window:main'; exit 0 ;;\n\
               send) exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let shell = command_path("sh")?.display().to_string();

    let split = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace:1")
        .env("PATH", &path)
        .args([
            "tmux-compat",
            "split-window",
            "-vPF",
            "#{pane_id}",
            shell.as_str(),
            "-lc",
            "sleep 30",
        ])
        .output()?;
    assert!(split.status.success(), "{split:?}");
    let pane = String::from_utf8_lossy(&split.stdout).trim().to_string();
    assert!(pane.starts_with('%'), "expected pane id, got {pane:?}");

    let kill = env
        .cmd()
        .env("PATH", &path)
        .args(["tmux-compat", "kill-pane", "-t", pane.as_str()])
        .output()?;
    assert!(kill.status.success(), "{kill:?}");

    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| {
            line == "close-surface --surface surface:visible --workspace workspace:1 --window window:main"
        }),
        "kill-pane should close the visible cmux split surface: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_omx_hud_watch_split_stays_detached_in_cmux() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-hud-watch.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               new-split|send) exit 70 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let parent = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "omx-hud-parent",
            "sh",
            "-lc",
            "sleep 60",
        ])
        .output()?;
    assert!(parent.status.success(), "{parent:?}");
    wait_for_session_present(&env, "omx-hud-parent")?;

    let hud_cmd = "exec sh -lc 'sleep 30' # hud --watch";
    let split = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace:1")
        .env("PATH", &path)
        .args([
            "tmux-compat",
            "split-window",
            "-v",
            "-l",
            "2",
            "-t",
            "omx-hud-parent:0",
            "-e",
            "OMX_TMUX_HUD_OWNER=1",
            "-e",
            "OMX_TMUX_HUD_LEADER_PANE=%0",
            "-P",
            "-F",
            "#{pane_id}",
            hud_cmd,
        ])
        .output()?;
    assert!(
        split.status.success(),
        "OMX HUD watch split should be accepted without opening a visible cmux split: {split:?}"
    );
    let pane = String::from_utf8_lossy(&split.stdout).trim().to_string();
    assert!(pane.starts_with('%'), "expected pane id, got {pane:?}");

    let sessions = env.cmd().args(["ls", "--all", "--json"]).output()?;
    assert!(sessions.status.success(), "{sessions:?}");
    let sessions: serde_json::Value = serde_json::from_slice(&sessions.stdout)?;
    let attached_clients = sessions
        .as_array()
        .and_then(|items| {
            items.iter().find_map(|item| {
                (item["pane_id"] == pane).then(|| item["attached_clients"].as_u64())
            })
        })
        .flatten();
    assert_eq!(
        attached_clients,
        Some(0),
        "HUD watch panes should be detached, not attached to a visible cmux surface: {sessions:?}"
    );

    let cmux_calls = std::fs::read_to_string(&cmux_log).unwrap_or_default();
    assert!(
        !cmux_calls.contains("new-split") && !cmux_calls.contains("send"),
        "HUD watch compatibility path must not create or attach a visible cmux split: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_kill_session_closes_child_visible_cmux_split_surface() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-visible-kill-session.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:source\",\"workspace_ref\":\"workspace:1\",\"window_ref\":\"window:main\"}}}}'; exit 0 ;;\n\
               new-split) printf '%s\\n' 'OK surface:visible-child workspace:1 window:main'; exit 0 ;;\n\
               send) exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let child_pane_file = env.temp.path().join("cmux-child-pane.txt");
    let split_status_file = env.temp.path().join("cmux-child-split-status.txt");
    let child_pane_file_arg =
        shlex::try_quote(&child_pane_file.display().to_string())?.into_owned();
    let split_status_file_arg =
        shlex::try_quote(&split_status_file.display().to_string())?.into_owned();
    let shell_arg = shlex::try_quote(&command_path("sh")?.display().to_string())?.into_owned();
    let lterm_arg = shlex::try_quote(env!("CARGO_BIN_EXE_lterm"))?.into_owned();
    let child_payload = shlex::try_quote("sleep 60")?.into_owned();
    let path_string = path.to_string_lossy().to_string();
    let path_arg = shlex::try_quote(&path_string)?.into_owned();
    let parent_script = format!(
        "PATH={path_arg}; export PATH; \
         {lterm_arg} tmux-compat split-window -vPF '#{{pane_id}}' \
         {shell_arg} -lc {child_payload} > {child_pane_file_arg}; \
         status=$?; printf %s \"$status\" > {split_status_file_arg}; sleep 60"
    );

    let parent = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace:1")
        .env("PATH", &path)
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "cmux-visible-parent",
            "sh",
            "-lc",
            parent_script.as_str(),
        ])
        .output()?;
    assert!(parent.status.success(), "{parent:?}");
    wait_for_session_present(&env, "cmux-visible-parent")?;
    assert_eq!(wait_for_file_contents(&split_status_file)?.trim(), "0");
    let child_pane = wait_for_file_contents(&child_pane_file)?.trim().to_string();
    assert!(
        child_pane.starts_with('%'),
        "expected child pane id, got {child_pane:?}"
    );

    let kill = env
        .cmd()
        .env("PATH", &path)
        .args(["tmux-compat", "kill-session", "-t", "cmux-visible-parent"])
        .output()?;
    assert!(kill.status.success(), "{kill:?}");
    wait_for_session_absent(&env, "cmux-visible-parent")?;
    let has_child = env
        .cmd()
        .args(["tmux-compat", "has-session", "-t", child_pane.as_str()])
        .status()?;
    assert!(
        !has_child.success(),
        "kill-session should recursively remove child pane {child_pane}"
    );

    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| {
            line == "close-surface --surface surface:visible-child --workspace workspace:1 --window window:main"
        }),
        "kill-session should close visible cmux split surfaces for child panes: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_stops_option_parsing_at_command() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "split-window",
            "-dP",
            "sh",
            "-lc",
            "echo SPLIT_PAYLOAD_ARG:$1; sleep 2",
            "sh",
            "-F",
            "not-a-format",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        pane.starts_with('%'),
        "split-window command payload -F should not override the default format: {pane:?}"
    );
    let captured = env.capture_until(&pane, "SPLIT_PAYLOAD_ARG:-F")?;
    assert!(captured.contains("SPLIT_PAYLOAD_ARG:-F"), "{captured}");
    Ok(())
}

#[test]
fn tmux_compat_split_window_treats_b_as_boolean_outside_buffer_commands() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "split-window",
            "-bdPF",
            "#{pane_id}",
            "echo SPLIT_B_READY; sleep 2",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        pane.starts_with('%'),
        "split-window -bdPF should print the pane id, got {pane:?}"
    );
    let captured = env.capture_until(&pane, "SPLIT_B_READY")?;
    assert!(captured.contains("SPLIT_B_READY"), "{captured}");
    Ok(())
}

#[test]
fn tmux_compat_split_window_accepts_empty_format_value() -> TestResult {
    let env = TestEnv::new()?;
    let marker = env.temp.path().join("split-empty-format-marker.txt");
    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "split-window",
            "-dP",
            "-F",
            "",
            "sh",
            "-lc",
            "printf SPLIT_EMPTY_FORMAT_READY > \"$1\"; sleep 2",
            "sh",
            marker.to_str().ok_or("marker path should be UTF-8")?,
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "\n",
        "empty split-window format should be accepted and print one newline"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(
            std::fs::read_to_string(&marker).as_deref(),
            Ok("SPLIT_EMPTY_FORMAT_READY")
        ) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("split-window command payload did not run after empty -F value".into())
}

#[test]
fn tmux_compat_split_window_rejects_non_detached_target() -> TestResult {
    let env = TestEnv::new()?;
    let before = session_names_json(&env)?;

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "split-window",
            "-t",
            "some-target",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "split-window -t should be explicit instead of creating a session: {output:?}"
    );
    assert_stderr_contains(&output, "tmux split-window -t some-target is not supported");
    let after = session_names_json(&env)?;
    assert_eq!(
        after, before,
        "split-window -t must not create a hidden session"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_detached_hud_options_do_not_open_cmux_split() -> TestResult {
    let env = TestEnv::new()?;
    let parent_status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "split-detached-parent",
            "sleep 30",
        ])
        .status()?;
    assert!(parent_status.success(), "{parent_status:?}");
    wait_for_session_present(&env, "split-detached-parent")?;
    let listed = env.cmd().arg("ls").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let listed_stdout = String::from_utf8_lossy(&listed.stdout);
    let parent_row = list_row(&listed_stdout, "split-detached-parent")
        .ok_or_else(|| format!("split-detached-parent row missing: {listed_stdout:?}"))?;
    let parent_pane = parent_row
        .get(1)
        .ok_or_else(|| format!("split-detached-parent row missing pane id: {parent_row:?}"))?;
    let fake_bin = env.temp.path().join("fake-cmux-detached-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-detached-hud-options.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             exit 97\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let marker = env.temp.path().join("split-detached-hud-marker.txt");
    let cwd = env.temp.path().display().to_string();
    let shell = command_path("sh")?.display().to_string();

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace:1")
        .env("PATH", &path)
        .env("TMUX_PANE", parent_pane)
        .args([
            "tmux-compat",
            "split-window",
            "-v",
            "-l",
            "3",
            "-d",
            "-t",
            parent_pane,
            "-c",
            cwd.as_str(),
            shell.as_str(),
            "-lc",
            "printf SPLIT_DETACHED_HUD_READY > \"$1\"",
            "sh",
            marker.to_str().ok_or("marker path should be UTF-8")?,
        ])
        .output()?;
    assert!(
        output.status.success(),
        "detached HUD-style split-window should not be treated as a visible split: {output:?}"
    );
    assert!(
        !cmux_log.exists(),
        "detached split-window must not call cmux new-split/send; calls were: {}",
        std::fs::read_to_string(&cmux_log).unwrap_or_default()
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(
            std::fs::read_to_string(&marker).as_deref(),
            Ok("SPLIT_DETACHED_HUD_READY")
        ) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("detached HUD-style split-window payload did not run".into())
}

#[test]
fn tmux_compat_split_window_detached_rejects_missing_target_without_side_effect() -> TestResult {
    let env = TestEnv::new()?;
    let before = session_names_json(&env)?;
    let marker = env
        .temp
        .path()
        .join("split-detached-missing-target-marker.txt");
    let shell = command_path("sh")?.display().to_string();

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "split-window",
            "-d",
            "-t",
            "%999",
            shell.as_str(),
            "-lc",
            "printf SHOULD_NOT_RUN > \"$1\"",
            "sh",
            marker.to_str().ok_or("marker path should be UTF-8")?,
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "detached split-window with missing -t must fail before creating a session: {output:?}"
    );
    assert_stderr_contains(&output, "tmux split-window -d target not found");
    assert!(
        !marker.exists(),
        "rejected split-window target must not execute payload"
    );
    let after = session_names_json(&env)?;
    assert_eq!(
        after, before,
        "rejected detached split-window target must not create a hidden session"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_detached_accepts_existing_non_current_target() -> TestResult {
    let env = TestEnv::new()?;
    let marker = env.temp.path().join("split-detached-other-marker.txt");
    let release = env.temp.path().join("split-detached-other-release.txt");
    for name in ["split-current", "split-other"] {
        let status = env
            .cmd()
            .args(["tmux-compat", "new-session", "-d", "-s", name, "sleep 30"])
            .status()?;
        assert!(status.success(), "{status:?}");
        wait_for_session_present(&env, name)?;
    }

    let before = session_names_json(&env)?;
    let before_all = session_rows_json(&env, true)?;
    let before_all_names = session_row_names(&before_all);
    let split_other_pane = before_all
        .iter()
        .find(|row| row.name == "split-other")
        .map(|row| row.pane_id.clone())
        .ok_or("split-other row should exist before detached helper")?;

    let shell = command_path("sh")?.display().to_string();
    let output = env
        .cmd()
        .env("TMUX_PANE", "%0")
        .args([
            "tmux-compat",
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "split-other",
            shell.as_str(),
            "-lc",
            "printf SPLIT_NON_CURRENT_TARGET_READY > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.05; done",
            "sh",
            marker.to_str().ok_or("marker path should be UTF-8")?,
            release.to_str().ok_or("release path should be UTF-8")?,
        ])
        .output()?;
    assert!(
        output.status.success(),
        "detached split-window should accept an existing live target even when it is not current: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let helper_pane = stdout.trim();
    assert!(
        helper_pane.starts_with('%'),
        "detached split-window -P should print the helper pane id: {stdout:?}"
    );
    let marker_contents = wait_for_file_contents(&marker)?;
    assert_eq!(
        marker_contents.trim(),
        "SPLIT_NON_CURRENT_TARGET_READY",
        "accepted non-current target should execute payload"
    );
    let running_all = session_rows_json(&env, true)?;
    let running_all_names = session_row_names(&running_all);
    let helper_names: Vec<_> = running_all_names.difference(&before_all_names).collect();
    assert_eq!(
        helper_names.len(),
        1,
        "accepted detached split-window should create exactly one helper while running; before={before_all_names:?} running={running_all_names:?}"
    );
    let helper_row = running_all
        .iter()
        .find(|row| row.name == *helper_names[0])
        .ok_or_else(|| format!("helper row missing from all sessions: {running_all:?}"))?;
    assert_eq!(
        helper_row.pane_id, helper_pane,
        "split-window -P should print the helper pane id, not the target pane"
    );
    assert_ne!(
        helper_row.pane_id, split_other_pane,
        "detached helper must be a separate lterm session, not the target pane"
    );
    assert_eq!(
        session_names_json(&env)?,
        before,
        "an explicit detached split target should make the helper a hidden child while standalone sessions remain visible"
    );
    assert_eq!(
        helper_row.parent_pane_id.as_deref(),
        Some(split_other_pane.as_str()),
        "the helper must be recorded as the explicit live target's child"
    );
    let children = env.cmd().args(["ls", "--children"]).output()?;
    assert!(children.status.success(), "{children:?}");
    let children_stdout = String::from_utf8_lossy(&children.stdout);
    let child_list_row = list_row(&children_stdout, &helper_row.name)
        .ok_or_else(|| format!("helper missing from child list: {children_stdout:?}"))?;
    assert_eq!(child_list_row[1], helper_pane, "{children_stdout:?}");
    assert_eq!(child_list_row[6], split_other_pane, "{children_stdout:?}");
    assert!(
        !children_stdout.lines().any(|line| {
            line.starts_with("split-current\t") || line.starts_with("split-other\t")
        }),
        "standalone roots must not leak into the child-only list: {children_stdout:?}"
    );
    let tmux_sessions = env
        .cmd()
        .args(["tmux-compat", "list-sessions", "-F", "#{session_name}"])
        .output()?;
    assert!(tmux_sessions.status.success(), "{tmux_sessions:?}");
    let tmux_sessions_stdout = String::from_utf8_lossy(&tmux_sessions.stdout);
    assert_exact_line_set(&tmux_sessions_stdout, &["split-current", "split-other"]);
    let target_panes = env
        .cmd()
        .args([
            "tmux-compat",
            "list-panes",
            "-t",
            "split-other",
            "-F",
            "#{pane_id}",
        ])
        .output()?;
    assert!(target_panes.status.success(), "{target_panes:?}");
    let target_panes_stdout = String::from_utf8_lossy(&target_panes.stdout);
    assert_exact_line_set(&target_panes_stdout, &[&split_other_pane, helper_pane]);
    std::fs::write(&release, "release")?;
    wait_for_session_names_eq(&env, &before, Duration::from_secs(10))?;
    poll_until(
        Duration::from_secs(10),
        Duration::from_millis(100),
        "detached helper cleanup",
        || {
            let names = session_row_names(&session_rows_json(&env, true)?);
            if names == before_all_names {
                Ok(PollStatus::Ready(()))
            } else {
                Ok(PollStatus::Pending(format!("{names:?}")))
            }
        },
    )?;
    let final_all = session_rows_json(&env, true)?;
    assert_eq!(
        session_row_names(&final_all),
        before_all_names,
        "detached helper should be cleaned up after release"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_accepts_omx_team_window_target_and_full_size_flag() -> TestResult {
    let env = TestEnv::new()?;
    let marker = env.temp.path().join("split-team-window-target-marker.txt");
    let pane_file = env.temp.path().join("split-team-window-target-pane.txt");
    let grandchild_marker = env
        .temp
        .path()
        .join("split-team-window-target-grandchild-marker.txt");
    let grandchild_pane_file = env
        .temp
        .path()
        .join("split-team-window-target-grandchild-pane.txt");
    let grandchild_status_file = env
        .temp
        .path()
        .join("split-team-window-target-grandchild-status.txt");
    let marker_arg = shlex::try_quote(&marker.display().to_string())?.into_owned();
    let pane_file_arg = shlex::try_quote(&pane_file.display().to_string())?.into_owned();
    let grandchild_marker_arg =
        shlex::try_quote(&grandchild_marker.display().to_string())?.into_owned();
    let grandchild_pane_file_arg =
        shlex::try_quote(&grandchild_pane_file.display().to_string())?.into_owned();
    let grandchild_status_file_arg =
        shlex::try_quote(&grandchild_status_file.display().to_string())?.into_owned();
    let cwd_arg = shlex::try_quote(&env.temp.path().display().to_string())?.into_owned();
    let grandchild_payload =
        format!("printf TEAM_WINDOW_TARGET_GRANDCHILD_READY > {grandchild_marker_arg}; sleep 60");
    let grandchild_payload_arg = shlex::try_quote(&grandchild_payload)?.into_owned();
    let child_payload = format!(
        "printf TEAM_WINDOW_TARGET_READY > {marker_arg}; \
         \"$LTERM_BIN\" tmux-compat split-window -v -d -P -F '#{{pane_id}}' \
         -t \"$TMUX_PANE\" sh -lc {grandchild_payload_arg} > {grandchild_pane_file_arg}; \
         status=$?; printf %s \"$status\" > {grandchild_status_file_arg}; sleep 60"
    );
    let child_payload_arg = shlex::try_quote(&child_payload)?.into_owned();
    let parent_script = format!(
        "\"$LTERM_BIN\" tmux-compat split-window -v -f -l 4 -t team-window-parent:0 \
         -d -P -F '#{{pane_id}}' -c {cwd_arg} sh -lc {child_payload_arg} > {pane_file_arg}; \
         status=$?; echo TEAM_WINDOW_SPLIT_STATUS:$status; sleep 60"
    );

    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "team-window-parent",
            "sh",
            "-lc",
            parent_script.as_str(),
        ])
        .status()?;
    assert!(status.success(), "{status:?}");
    env.capture_until("team-window-parent", "TEAM_WINDOW_SPLIT_STATUS:0")?;
    let child_pane = wait_for_file_contents(&pane_file)?.trim().to_string();
    assert!(
        child_pane.starts_with('%'),
        "split-window -P should print a child pane id: {child_pane:?}"
    );
    assert_eq!(
        wait_for_file_contents(&marker)?.trim(),
        "TEAM_WINDOW_TARGET_READY"
    );
    assert_eq!(wait_for_file_contents(&grandchild_status_file)?.trim(), "0");
    let grandchild_pane = wait_for_file_contents(&grandchild_pane_file)?
        .trim()
        .to_string();
    assert!(
        grandchild_pane.starts_with('%'),
        "nested split-window -P should print a grandchild pane id: {grandchild_pane:?}"
    );
    assert_eq!(
        wait_for_file_contents(&grandchild_marker)?.trim(),
        "TEAM_WINDOW_TARGET_GRANDCHILD_READY"
    );

    let listed = env.cmd().arg("ls").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    let parent_row = list_row(&stdout, "team-window-parent")
        .ok_or_else(|| format!("team-window-parent row missing: {stdout:?}"))?;
    let parent_pane = parent_row
        .get(1)
        .ok_or_else(|| format!("team-window-parent row missing pane id: {parent_row:?}"))?;
    let roots = session_rows_json(&env, false)?;
    assert_eq!(
        roots
            .iter()
            .map(|row| row.pane_id.as_str())
            .collect::<Vec<_>>(),
        vec![*parent_pane],
        "nested detached splits should remain hidden from the default list: {roots:?}"
    );
    let all_rows = session_rows_json(&env, true)?;
    let child_row = all_rows
        .iter()
        .find(|row| row.pane_id == child_pane)
        .ok_or_else(|| format!("child pane missing from all sessions: {all_rows:?}"))?;
    assert_eq!(
        child_row.parent_pane_id.as_deref(),
        Some(*parent_pane),
        "first split should be a child of the explicit root target"
    );
    let grandchild_row = all_rows
        .iter()
        .find(|row| row.pane_id == grandchild_pane)
        .ok_or_else(|| format!("grandchild pane missing from all sessions: {all_rows:?}"))?;
    assert_eq!(
        grandchild_row.parent_pane_id.as_deref(),
        Some(child_pane.as_str()),
        "nested split should be a child of its explicit child-pane target"
    );

    let display = env
        .cmd()
        .args([
            "tmux-compat",
            "display-message",
            "-p",
            "-t",
            "team-window-parent:0",
            "#{window_width}:#{pane_id}",
        ])
        .output()?;
    assert!(
        display.status.success(),
        "display-message should resolve session:window target: {display:?}"
    );
    let display_stdout = String::from_utf8_lossy(&display.stdout);
    let (width, displayed_pane) = display_stdout
        .trim()
        .split_once(':')
        .ok_or_else(|| format!("unexpected display-message output: {display_stdout:?}"))?;
    assert!(width.parse::<u16>()? > 0, "{display_stdout:?}");
    assert_eq!(displayed_pane, *parent_pane, "{display_stdout:?}");

    for invalid_target in ["team-window-parent:99", "team-window-parent:0.1"] {
        let invalid = env
            .cmd()
            .args([
                "tmux-compat",
                "display-message",
                "-p",
                "-t",
                invalid_target,
                "#{pane_id}",
            ])
            .output()?;
        assert!(
            !invalid.status.success(),
            "unsupported window/pane target must not fall back to the session root: {invalid_target}: {invalid:?}"
        );
    }

    let panes = env
        .cmd()
        .args([
            "tmux-compat",
            "list-panes",
            "-t",
            "team-window-parent:0",
            "-F",
            "#{pane_id}:#{pane_dead}:#{pane_pid}:#{history_size}",
        ])
        .output()?;
    assert!(
        panes.status.success(),
        "list-panes should resolve session:window target: {panes:?}"
    );
    let panes_stdout = String::from_utf8_lossy(&panes.stdout);
    let pane_rows: Vec<Vec<&str>> = panes_stdout
        .lines()
        .map(|line| line.split(':').collect())
        .collect();
    for expected_pane in [*parent_pane, child_pane.as_str(), grandchild_pane.as_str()] {
        let row = pane_rows
            .iter()
            .find(|row| row.first().copied() == Some(expected_pane))
            .ok_or_else(|| {
                format!("pane {expected_pane} missing from list-panes output: {panes_stdout:?}")
            })?;
        assert_eq!(
            row.len(),
            4,
            "pane liveness row should have pane_id:pane_dead:pane_pid:history_size: {row:?}"
        );
        assert_eq!(
            row[1], "0",
            "live panes should expand pane_dead to 0: {row:?}"
        );
        row[2]
            .parse::<u32>()
            .map(|_| ())
            .map_err(|err| format!("pane_pid should be numeric for live panes: {row:?}: {err}"))?;
        assert_eq!(
            row[3], "0",
            "history_size fallback should be numeric zero for synthetic panes: {row:?}"
        );
    }
    Ok(())
}

#[test]
fn tmux_compat_display_message_expands_omc_window_shorthand() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "omc-format",
            "sleep 60",
        ])
        .status()?;
    assert!(status.success(), "{status:?}");
    wait_for_session_present(&env, "omc-format")?;

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "display-message",
            "-p",
            "-t",
            "omc-format",
            "#S:#I #{pane_id} #{pane_dead} #{history_size}",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut fields = stdout.split_whitespace();
    let Some(session_window) = fields.next() else {
        return Err(format!("missing session/window field: {stdout:?}").into());
    };
    let Some(pane_id) = fields.next() else {
        return Err(format!("missing pane id field: {stdout:?}").into());
    };
    let Some(pane_dead) = fields.next() else {
        return Err(format!("missing pane_dead field: {stdout:?}").into());
    };
    let Some(history_size) = fields.next() else {
        return Err(format!("missing history_size field: {stdout:?}").into());
    };
    assert_eq!(session_window, "omc-format:0", "{stdout:?}");
    assert!(pane_id.starts_with('%'), "{stdout:?}");
    assert_eq!(pane_dead, "0", "{stdout:?}");
    assert_eq!(history_size, "0", "{stdout:?}");
    Ok(())
}

#[test]
fn tmux_compat_session_zero_alias_works_for_direct_targets() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "direct-zero-alias",
            "sh",
            "-lc",
            "printf DIRECT_ZERO_ALIAS_READY; sleep 60",
        ])
        .status()?;
    assert!(status.success(), "{status:?}");
    wait_for_session_present(&env, "direct-zero-alias")?;

    let has = env
        .cmd()
        .args(["tmux-compat", "has-session", "-t", "direct-zero-alias:0"])
        .output()?;
    assert!(has.status.success(), "{has:?}");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut capture_stdout = String::new();
    while Instant::now() < deadline {
        let capture = env
            .cmd()
            .args([
                "tmux-compat",
                "capture-pane",
                "-t",
                "direct-zero-alias:0",
                "-p",
                "-S",
                "-80",
            ])
            .output()?;
        assert!(capture.status.success(), "{capture:?}");
        capture_stdout = String::from_utf8_lossy(&capture.stdout).to_string();
        if capture_stdout.contains("DIRECT_ZERO_ALIAS_READY") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        capture_stdout.contains("DIRECT_ZERO_ALIAS_READY"),
        "direct session:0 capture did not observe output: {capture_stdout:?}"
    );

    let kill = env
        .cmd()
        .args(["tmux-compat", "kill-session", "-t", "direct-zero-alias:0"])
        .output()?;
    assert!(kill.status.success(), "{kill:?}");
    wait_for_session_absent(&env, "direct-zero-alias")?;
    Ok(())
}

#[test]
fn tmux_compat_supports_omc_default_split_flow() -> TestResult {
    let env = TestEnv::new()?;
    let marker = env.temp.path().join("omc-default-split-marker.txt");
    let target_file = env.temp.path().join("omc-default-split-target.txt");
    let pane_file = env.temp.path().join("omc-default-split-pane.txt");
    let status_file = env.temp.path().join("omc-default-split-status.txt");
    let marker_arg = shlex::try_quote(&marker.display().to_string())?.into_owned();
    let target_file_arg = shlex::try_quote(&target_file.display().to_string())?.into_owned();
    let pane_file_arg = shlex::try_quote(&pane_file.display().to_string())?.into_owned();
    let status_file_arg = shlex::try_quote(&status_file.display().to_string())?.into_owned();
    let cwd_arg = shlex::try_quote(&env.temp.path().display().to_string())?.into_owned();
    let lterm_arg = shlex::try_quote(env!("CARGO_BIN_EXE_lterm"))?.into_owned();
    let payload = format!("printf OMC_DEFAULT_SPLIT_READY; printf ready > {marker_arg}; sleep 60");
    let payload_arg = shlex::try_quote(&payload)?.into_owned();
    let parent_script = format!(
        "{lterm_arg} tmux-compat display-message -p '#S:#I #{{pane_id}}' > {target_file_arg}; \
         {lterm_arg} tmux-compat split-window -h -t omc-default-parent:0 -d -P \
         -F '#{{pane_id}}' -c {cwd_arg} sh -lc {payload_arg} > {pane_file_arg}; \
         status=$?; printf %s \"$status\" > {status_file_arg}; sleep 60"
    );

    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "omc-default-parent",
            "sh",
            "-lc",
            parent_script.as_str(),
        ])
        .status()?;
    assert!(status.success(), "{status:?}");
    wait_for_session_present(&env, "omc-default-parent")?;
    assert_eq!(wait_for_file_contents(&status_file)?.trim(), "0");

    let target_stdout = wait_for_file_contents(&target_file)?;
    let mut target_fields = target_stdout.split_whitespace();
    let session_window = target_fields
        .next()
        .ok_or_else(|| format!("missing #S:#I field: {target_stdout:?}"))?;
    let leader_pane = target_fields
        .next()
        .ok_or_else(|| format!("missing leader pane field: {target_stdout:?}"))?;
    assert_eq!(session_window, "omc-default-parent:0", "{target_stdout:?}");
    assert!(leader_pane.starts_with('%'), "{target_stdout:?}");

    let worker_pane = wait_for_file_contents(&pane_file)?.trim().to_string();
    assert!(
        worker_pane.starts_with('%'),
        "split-window -P should return a pane id: {worker_pane:?}"
    );
    assert_ne!(
        worker_pane, leader_pane,
        "worker must be a helper pane, not the leader pane"
    );
    assert_eq!(wait_for_file_contents(&marker)?.trim(), "ready");

    let capture = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-t",
            worker_pane.as_str(),
            "-p",
            "-S",
            "-80",
        ])
        .output()?;
    assert!(capture.status.success(), "{capture:?}");
    assert!(
        String::from_utf8_lossy(&capture.stdout).contains("OMC_DEFAULT_SPLIT_READY"),
        "{capture:?}"
    );

    let pane_dead = env
        .cmd()
        .args([
            "tmux-compat",
            "display-message",
            "-t",
            worker_pane.as_str(),
            "-p",
            "#{pane_dead}",
        ])
        .output()?;
    assert!(pane_dead.status.success(), "{pane_dead:?}");
    assert_eq!(String::from_utf8_lossy(&pane_dead.stdout).trim(), "0");

    let panes = env
        .cmd()
        .args([
            "tmux-compat",
            "list-panes",
            "-t",
            session_window,
            "-F",
            "#{pane_id}\t#{pane_current_command}\t#{pane_start_command}",
        ])
        .output()?;
    assert!(panes.status.success(), "{panes:?}");
    let panes_stdout = String::from_utf8_lossy(&panes.stdout);
    assert!(
        panes_stdout
            .lines()
            .any(|line| line.starts_with(leader_pane)),
        "leader pane missing from list-panes output: {panes_stdout:?}"
    );
    assert!(
        panes_stdout
            .lines()
            .any(|line| line.starts_with(worker_pane.as_str())),
        "worker pane missing from list-panes output: {panes_stdout:?}"
    );
    assert!(
        panes_stdout
            .lines()
            .all(|line| line.split('\t').count() == 3),
        "HUD helper output must stay tab-parseable: {panes_stdout:?}"
    );

    let panes_from_worker_target = env
        .cmd()
        .args([
            "tmux-compat",
            "list-panes",
            "-t",
            worker_pane.as_str(),
            "-F",
            "#{pane_id}",
        ])
        .output()?;
    assert!(
        panes_from_worker_target.status.success(),
        "{panes_from_worker_target:?}"
    );
    let worker_target_stdout = String::from_utf8_lossy(&panes_from_worker_target.stdout);
    for expected_pane in [leader_pane, worker_pane.as_str()] {
        assert!(
            worker_target_stdout
                .lines()
                .any(|line| line == expected_pane),
            "list-panes -t <pane> should return the whole synthetic window so HUD dedupe can see sibling panes; missing {expected_pane}: {worker_target_stdout:?}"
        );
    }

    let kill = env
        .cmd()
        .args(["tmux-compat", "kill-pane", "-t", worker_pane.as_str()])
        .output()?;
    assert!(kill.status.success(), "{kill:?}");
    Ok(())
}

#[test]
fn tmux_compat_new_session_prints_omc_detached_target_format() -> TestResult {
    let env = TestEnv::new()?;
    let cwd_marker = env.temp.path().join("omc-detached-cwd.txt");
    let cwd_marker_arg = shlex::try_quote(&cwd_marker.display().to_string())?.into_owned();
    let payload = format!("pwd > {cwd_marker_arg}; printf OMC_DETACHED_READY; sleep 60");
    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-P",
            "-F",
            "#S:0 #{pane_id}",
            "-s",
            "omc-detached",
            "-c",
            env.temp
                .path()
                .to_str()
                .ok_or("temp path should be UTF-8")?,
            "sh",
            "-lc",
            payload.as_str(),
        ])
        .output()?;
    assert!(
        output.status.success(),
        "OMC detached new-session path should succeed: {output:?}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut fields = stdout.split_whitespace();
    assert_eq!(
        fields.next(),
        Some("omc-detached:0"),
        "new-session -P -F should print #S:0: {stdout:?}"
    );
    let pane_id = fields
        .next()
        .ok_or_else(|| format!("new-session output missing pane id: {stdout:?}"))?;
    assert!(pane_id.starts_with('%'), "{stdout:?}");
    let observed_cwd = std::fs::canonicalize(wait_for_file_contents(&cwd_marker)?.trim())?;
    let expected_cwd = std::fs::canonicalize(env.temp.path())?;
    assert_eq!(observed_cwd, expected_cwd);
    let capture = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-t",
            pane_id,
            "-p",
            "-S",
            "-80",
        ])
        .output()?;
    assert!(capture.status.success(), "{capture:?}");
    assert!(
        String::from_utf8_lossy(&capture.stdout).contains("OMC_DETACHED_READY"),
        "{capture:?}"
    );
    let kill = env
        .cmd()
        .args(["tmux-compat", "kill-session", "-t", "omc-detached"])
        .output()?;
    assert!(kill.status.success(), "{kill:?}");
    Ok(())
}

#[test]
fn tmux_compat_reports_version_for_common_aliases() -> TestResult {
    let env = TestEnv::new()?;
    for alias in ["-V", "--version", "version"] {
        let output = env.cmd().args(["tmux-compat", alias]).output()?;
        assert!(
            output.status.success(),
            "tmux compatibility version alias {alias:?} should succeed: {output:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "tmux 3.5a (light-terminal compat)",
            "tmux compatibility version alias {alias:?} should match the canonical version"
        );
        assert!(
            output.stderr.is_empty(),
            "tmux compatibility version alias {alias:?} should not warn: {output:?}"
        );
    }
    Ok(())
}

#[test]
fn tmux_compat_rejects_omc_invalid_window_targets_without_fallback() -> TestResult {
    let env = TestEnv::new()?;
    let marker = env.temp.path().join("invalid-window-target-marker.txt");
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "invalid-window-parent",
            "sleep 60",
        ])
        .status()?;
    assert!(status.success(), "{status:?}");
    wait_for_session_present(&env, "invalid-window-parent")?;
    let before = session_names_json(&env)?;

    for target in [
        "invalid-window-parent:#I",
        "invalid-window-parent:1",
        "invalid-window-parent:0.1",
    ] {
        let display = env
            .cmd()
            .args([
                "tmux-compat",
                "display-message",
                "-p",
                "-t",
                target,
                "#{pane_id}",
            ])
            .output()?;
        assert!(!display.status.success(), "{target}: {display:?}");
        assert_stderr_contains(&display, "unsupported tmux window target in lterm compat:");
        assert_stderr_contains(
            &display,
            "lterm supports bare session targets and session:0 only",
        );
    }

    let split = env
        .cmd()
        .args([
            "tmux-compat",
            "split-window",
            "-d",
            "-P",
            "-t",
            "invalid-window-parent:#I",
            "sh",
            "-lc",
            format!("printf bad > {}", marker.display()).as_str(),
        ])
        .output()?;
    assert!(!split.status.success(), "{split:?}");
    assert_stderr_contains(&split, "unsupported tmux window target in lterm compat:");
    assert_stderr_contains(
        &split,
        "lterm supports bare session targets and session:0 only",
    );
    assert!(
        !marker.exists(),
        "invalid split target must not execute payload"
    );
    let new_window = env
        .cmd()
        .args([
            "tmux-compat",
            "new-window",
            "-d",
            "-P",
            "-t",
            "invalid-window-parent:#I",
            "-n",
            "invalid-window-child",
            "sh",
            "-lc",
            format!("printf bad > {}", marker.display()).as_str(),
        ])
        .output()?;
    assert!(!new_window.status.success(), "{new_window:?}");
    assert_stderr_contains(
        &new_window,
        "unsupported tmux window target in lterm compat:",
    );
    assert_stderr_contains(
        &new_window,
        "lterm supports bare session targets and session:0 only",
    );
    assert!(
        !marker.exists(),
        "invalid new-window target must not execute payload"
    );
    assert_eq!(
        session_names_json(&env)?,
        before,
        "invalid window target must not create fallback helper sessions"
    );
    Ok(())
}

#[test]
fn tmux_compat_new_window_is_detached_only_without_visible_side_effects() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("new-window-cmux.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             exit 70\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let marker = env.temp.path().join("new-window-visible-marker.txt");
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "unsupported-window-parent",
            "sleep 60",
        ])
        .status()?;
    assert!(status.success(), "{status:?}");
    wait_for_session_present(&env, "unsupported-window-parent")?;

    let list = env.cmd().args(["tmux-compat", "list-commands"]).output()?;
    assert!(list.status.success(), "{list:?}");
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.lines().any(|line| line == "new-window"),
        "new-window should be advertised as a partial compatibility command: {list_stdout:?}"
    );
    for unsupported in ["select-window", "kill-window"] {
        assert!(
            !list_stdout.lines().any(|line| line == unsupported),
            "{unsupported} must remain outside the baseline command list: {list_stdout:?}"
        );
    }

    let new_window = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace:1")
        .env("PATH", &path)
        .args([
            "tmux-compat",
            "new-window",
            "-d",
            "-P",
            "-F",
            "#S:#I #{pane_id}",
            "-t",
            "unsupported-window-parent",
            "-n",
            "unsupported-window-child",
            "-c",
            env.temp
                .path()
                .to_str()
                .ok_or("temp path should be UTF-8")?,
            "sh",
            "-lc",
            "sleep 60",
        ])
        .output()?;
    assert!(new_window.status.success(), "{new_window:?}");
    let line = String::from_utf8_lossy(&new_window.stdout)
        .trim()
        .to_string();
    assert!(
        line.starts_with("unsupported-window-child:0 %"),
        "new-window should print the requested format using the detached lterm session: {line:?}"
    );
    wait_for_session_present(&env, "unsupported-window-child")?;
    let after_detached = session_names_json(&env)?;
    assert!(
        after_detached.contains("unsupported-window-child"),
        "detached new-window should create the named lterm session: {after_detached:?}"
    );
    let default_format = env
        .cmd()
        .args([
            "tmux-compat",
            "new-window",
            "-dP",
            "-t",
            "unsupported-window-parent",
            "-n",
            "unsupported-window-default-format",
            "sleep 60",
        ])
        .output()?;
    assert!(default_format.status.success(), "{default_format:?}");
    assert_eq!(
        String::from_utf8_lossy(&default_format.stdout).trim(),
        "unsupported-window-default-format:0",
        "new-window -P without -F should print a tmux-style window target"
    );
    assert!(
        !cmux_log.exists(),
        "detached new-window must not open a visible cmux split"
    );

    let visible_marker = shlex::try_quote(&marker.display().to_string())?.into_owned();
    let non_detached = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace:1")
        .env("PATH", &path)
        .args([
            "tmux-compat",
            "new-window",
            "-P",
            "-F",
            "#S:#I #{pane_id}",
            "-t",
            "unsupported-window-parent",
            "-n",
            "unsupported-window-visible-child",
            "sh",
            "-lc",
            &format!("printf bad > {visible_marker}; sleep 60"),
        ])
        .output()?;
    assert!(!non_detached.status.success(), "{non_detached:?}");
    assert_stderr_contains(
        &non_detached,
        "tmux new-window without -d is not supported by lterm compat",
    );
    assert!(
        !marker.exists(),
        "non-detached new-window must fail before executing its payload"
    );
    assert!(
        !cmux_log.exists(),
        "non-detached new-window must fail before opening a visible cmux split"
    );
    assert_eq!(
        session_names_json(&env)?,
        {
            let mut expected = after_detached.clone();
            expected.insert("unsupported-window-default-format".to_string());
            expected
        },
        "non-detached new-window must not create fallback helper sessions"
    );

    let unsupported_option = env
        .cmd()
        .args([
            "tmux-compat",
            "new-window",
            "-d",
            "-e",
            "LTERM_BAD=1",
            "-t",
            "unsupported-window-parent",
            "-n",
            "unsupported-window-env-child",
            "sh",
            "-lc",
            &format!("printf bad > {visible_marker}; sleep 60"),
        ])
        .output()?;
    assert!(
        !unsupported_option.status.success(),
        "{unsupported_option:?}"
    );
    assert_stderr_contains(
        &unsupported_option,
        "unsupported tmux new-window option: -e",
    );
    assert!(
        !marker.exists(),
        "unsupported new-window options must fail before executing shifted payload tokens"
    );

    let kill_window = env
        .cmd()
        .args([
            "tmux-compat",
            "kill-window",
            "-t",
            "unsupported-window-parent",
        ])
        .output()?;
    assert!(!kill_window.status.success(), "{kill_window:?}");
    assert_stderr_contains(
        &kill_window,
        "unsupported tmux command in lterm compat: kill-window",
    );
    assert_stderr_contains(
        &kill_window,
        "Run `lterm tmux-compat list-commands` to inspect supported commands",
    );
    assert_eq!(
        session_names_json(&env)?,
        {
            let mut expected = after_detached.clone();
            expected.insert("unsupported-window-default-format".to_string());
            expected
        },
        "unsupported kill-window must not kill pane/session state"
    );
    let cleanup = env
        .cmd()
        .args([
            "tmux-compat",
            "kill-session",
            "-t",
            "unsupported-window-child",
        ])
        .output()?;
    assert!(cleanup.status.success(), "{cleanup:?}");
    assert!(
        session_names_json(&env)?.contains("unsupported-window-parent"),
        "parent session should have been preserved throughout the new-window test"
    );
    Ok(())
}

#[test]
fn tmux_compat_run_shell_executes_background_shell_command() -> TestResult {
    let env = TestEnv::new()?;
    let marker = env.temp.path().join("run-shell-marker.txt");
    let marker_arg = shlex::try_quote(&marker.display().to_string())?.into_owned();
    let shell_command = format!("printf RUNSHELL_READY > {marker_arg}");
    let output = env
        .cmd()
        .args(["tmux-compat", "run-shell", "-b", shell_command.as_str()])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(wait_for_file_contents(&marker)?.trim(), "RUNSHELL_READY");
    Ok(())
}

#[test]
fn tmux_compat_run_shell_background_delay_defers_execution() -> TestResult {
    let env = TestEnv::new()?;
    let marker = env.temp.path().join("run-shell-delayed-marker.txt");
    let marker_arg = shlex::try_quote(&marker.display().to_string())?.into_owned();
    let shell_command = format!("printf RUNSHELL_DELAYED_READY > {marker_arg}");
    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "run-shell",
            "-b",
            "-d",
            "1",
            shell_command.as_str(),
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        !marker.exists(),
        "run-shell -b -d should return before running the delayed command"
    );
    assert_eq!(
        wait_for_file_contents(&marker)?.trim(),
        "RUNSHELL_DELAYED_READY"
    );
    Ok(())
}

#[test]
fn tmux_compat_run_shell_rejects_overflowing_delay_without_panic() -> TestResult {
    let env = TestEnv::new()?;
    let marker = env.temp.path().join("run-shell-overflow-marker.txt");
    let marker_arg = shlex::try_quote(&marker.display().to_string())?.into_owned();
    let shell_command = format!("printf RUNSHELL_OVERFLOW_SHOULD_NOT_RUN > {marker_arg}");
    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "run-shell",
            "-b",
            "-d",
            "18446744073709551616",
            shell_command.as_str(),
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "overflowing run-shell delay should fail before spawning: {output:?}"
    );
    assert_stderr_contains(
        &output,
        "tmux run-shell -d delay must be a finite non-negative duration",
    );
    assert!(
        !marker.exists(),
        "overflowing run-shell delay must not spawn the command"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_detached_e_applies_environment_and_cluster_values() -> TestResult {
    let env = TestEnv::new()?;
    let parent_status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "split-env-parent",
            "sleep 30",
        ])
        .status()?;
    assert!(parent_status.success(), "{parent_status:?}");
    wait_for_session_present(&env, "split-env-parent")?;
    let listed = env.cmd().arg("ls").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let listed_stdout = String::from_utf8_lossy(&listed.stdout);
    let parent_row = list_row(&listed_stdout, "split-env-parent")
        .ok_or_else(|| format!("split-env-parent row missing: {listed_stdout:?}"))?;
    let parent_pane = parent_row
        .get(1)
        .ok_or_else(|| format!("split-env-parent row missing pane id: {parent_row:?}"))?;
    let marker = env.temp.path().join("split-detached-env-marker.txt");
    let marker_separate = env
        .temp
        .path()
        .join("split-detached-env-marker-separate.txt");
    let shell = command_path("sh")?.display().to_string();

    let output = env
        .cmd()
        .env("TMUX_PANE", parent_pane)
        .args([
            "tmux-compat",
            "split-window",
            "-vl3",
            "-d",
            "-t",
            "split-env-parent",
            "-eSPLIT_WINDOW_ENV=from-e",
            shell.as_str(),
            "-lc",
            "printf '%s' \"$SPLIT_WINDOW_ENV\" > \"$1\"",
            "sh",
            marker.to_str().ok_or("marker path should be UTF-8")?,
        ])
        .output()?;
    assert!(
        output.status.success(),
        "detached split-window should apply -e and consume -l inline value: {output:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_inline_env = false;
    while Instant::now() < deadline {
        if matches!(std::fs::read_to_string(&marker).as_deref(), Ok("from-e")) {
            saw_inline_env = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !saw_inline_env {
        return Err("detached split-window payload did not receive inline -e environment".into());
    }

    let output = env
        .cmd()
        .env("TMUX_PANE", parent_pane)
        .args([
            "tmux-compat",
            "split-window",
            "-v",
            "-l",
            "3",
            "-d",
            "-t",
            "split-env-parent",
            "-e",
            "SPLIT_WINDOW_ENV=from-separate-e",
            shell.as_str(),
            "-lc",
            "printf '%s' \"$SPLIT_WINDOW_ENV\" > \"$1\"",
            "sh",
            marker_separate
                .to_str()
                .ok_or("marker path should be UTF-8")?,
        ])
        .output()?;
    assert!(
        output.status.success(),
        "detached split-window should apply separate -e and consume -l value: {output:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(
            std::fs::read_to_string(&marker_separate).as_deref(),
            Ok("from-separate-e")
        ) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("detached split-window payload did not receive both -e forms".into())
}

#[test]
fn tmux_compat_split_window_without_detach_requires_cmux_before_session_creation() -> TestResult {
    let env = TestEnv::new()?;
    let no_cmux_bin = env.temp.path().join("no-cmux-bin");
    std::fs::create_dir(&no_cmux_bin)?;
    let before = session_names_json(&env)?;

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace-for-missing-cmux")
        .env("PATH", &no_cmux_bin)
        .args(["tmux-compat", "split-window", "sh", "-lc", "sleep 30"])
        .output()?;
    assert!(
        !output.status.success(),
        "non-detached split-window without cmux must fail: {output:?}"
    );
    assert_stderr_contains(&output, "requires the cmux CLI in PATH");
    let after = session_names_json(&env)?;
    assert_eq!(
        after, before,
        "failed non-detached split-window must not create an orphan session"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_cmux_new_split_failure_does_not_create_session() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:failing\"}}}}'; exit 0 ;;\n\
               *) printf '\\033]52;c;secret\\007CMUX_FAIL\\nNEXT\\n' >&2; exit 42 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let before = session_names_json(&env)?;

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace-for-failing-cmux")
        .env("PATH", &fake_bin)
        .args(["tmux-compat", "split-window", "-v", "sh", "-lc", "sleep 30"])
        .output()?;
    assert!(
        !output.status.success(),
        "cmux new-split failure must fail split-window: {output:?}"
    );
    assert_stderr_contains(&output, "cmux new-split down failed");
    assert_stderr_contains(&output, "CMUX_FAIL NEXT");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains('\x1b') && !stderr.contains("secret"),
        "cmux stderr should be sanitized before surfacing: {stderr:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .any(|line| line == "new-split down --surface surface:failing --focus true"),
        "fake cmux should record attempted down split: {cmux_calls:?}"
    );
    let after = session_names_json(&env)?;
    assert_eq!(
        after, before,
        "cmux split failure must happen before lterm session creation"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_requires_identified_cmux_surface() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-identify-missing.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               new-split) exit 0 ;;\n\
               identify) printf '%s\\n' '{{}}'; exit 0 ;;\n\
               send|send-surface|close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let before = session_names_json(&env)?;

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace-for-missing-surface-id")
        .env("PATH", &fake_bin)
        .args(["tmux-compat", "split-window", "-h", "sh", "-lc", "sleep 30"])
        .output()?;
    assert!(
        !output.status.success(),
        "missing cmux surface id must fail split-window before lterm creation: {output:?}"
    );
    assert_stderr_contains(
        &output,
        "cmux identify did not report a split source surface id",
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        !cmux_calls
            .lines()
            .any(|line| line.starts_with("new-split ")),
        "missing source surface id must fail before cmux new-split: {cmux_calls:?}"
    );
    assert!(
        !cmux_calls
            .lines()
            .any(|line| line.starts_with("close-surface")),
        "missing source surface id must not close an untargeted cmux surface: {cmux_calls:?}"
    );
    assert!(
        !cmux_calls
            .lines()
            .any(|line| line == "send" || line.starts_with("send ")),
        "missing surface id must not fall back to untargeted cmux send: {cmux_calls:?}"
    );
    assert_eq!(
        session_names_json(&env)?,
        before,
        "missing cmux surface id must fail before lterm session creation"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_rejects_truncated_cmux_new_split_output() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-truncated-new-split.log");
    let identify_count = env.temp.path().join("cmux-truncated-identify-count");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) if [ -f {} ]; then printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:created\"}}}}'; else : > {}; printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:source\"}}}}'; fi; exit 0 ;;\n\
               new-split) i=0; while [ \"$i\" -lt 17000 ]; do printf x; i=$((i + 1)); done; exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?,
            shlex::try_quote(&identify_count.display().to_string())?,
            shlex::try_quote(&identify_count.display().to_string())?
        ),
    )?;
    let before = session_names_json(&env)?;

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace-for-truncated-new-split")
        .env("PATH", &fake_bin)
        .args(["tmux-compat", "split-window", "-h", "sh", "-lc", "sleep 30"])
        .output()?;
    assert!(
        !output.status.success(),
        "truncated cmux new-split stdout must fail split-window: {output:?}"
    );
    assert_stderr_contains(&output, "cmux new-split right output exceeded");
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .any(|line| line == "close-surface --surface surface:created"),
        "truncated cmux new-split output should close the focused split surface: {cmux_calls:?}"
    );
    assert!(
        !cmux_calls
            .lines()
            .any(|line| line == "send" || line.starts_with("send ")),
        "truncated cmux new-split output must not send attach command: {cmux_calls:?}"
    );
    assert_eq!(
        session_names_json(&env)?,
        before,
        "truncated cmux new-split output must fail before lterm session creation"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_rolls_back_cmux_split_when_lterm_creation_fails() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-rollback.log");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               new-split) printf '%s\\n' 'OK surface:42 workspace:1'; exit 0 ;;\n\
               identify) printf '%s\\n' '{{\"caller\":{{\"surface_ref\":\"surface-original\"}},\"focused\":{{\"surface_ref\":\"surface-wrong\"}}}}'; exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let bad_socket = env.temp.path().join("not-a-socket");
    std::fs::write(&bad_socket, b"not a socket")?;

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace-for-rollback")
        .env("PATH", &fake_bin)
        .env("LTERM_SOCKET", &bad_socket)
        .args(["tmux-compat", "split-window", "-h", "sh", "-lc", "sleep 30"])
        .output()?;
    assert!(
        !output.status.success(),
        "lterm session creation failure should fail split-window: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .any(|line| line == "new-split right --surface surface-wrong --focus true"),
        "fake cmux should target the identified focused surface for the split attempt: {cmux_calls:?}"
    );
    assert!(
        cmux_calls
            .lines()
            .any(|line| line == "close-surface --surface surface:42 --workspace workspace:1"),
        "failed lterm creation should roll back the cmux surface reported by new-split stdout: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_rolls_back_identified_cmux_split_when_lterm_creation_fails()
-> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-identified-rollback.log");
    let split_state = env.temp.path().join("cmux-identified-rollback-created");
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               new-split) : > {}; printf '%s\\n' 'OK workspace:1'; exit 0 ;;\n\
               identify)\n\
                 if [ -e {} ]; then\n\
                   printf '%s\\n' '{{\"caller\":{{\"surface_ref\":\"surface:caller\"}},\"focused\":{{\"surface_ref\":\"surface:new\"}}}}'\n\
                 else\n\
                   printf '%s\\n' '{{\"caller\":{{\"surface_ref\":\"surface:caller\"}},\"focused\":{{\"surface_ref\":\"surface:source\"}}}}'\n\
                 fi\n\
                 exit 0 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?,
            shlex::try_quote(&split_state.display().to_string())?,
            shlex::try_quote(&split_state.display().to_string())?
        ),
    )?;
    let bad_socket = env.temp.path().join("not-a-socket");
    std::fs::write(&bad_socket, b"not a socket")?;

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace-for-identified-rollback")
        .env("PATH", &fake_bin)
        .env("LTERM_SOCKET", &bad_socket)
        .args(["tmux-compat", "split-window", "-h", "sh", "-lc", "sleep 30"])
        .output()?;
    assert!(
        !output.status.success(),
        "lterm session creation failure should fail split-window: {output:?}"
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| line == "identify --json"),
        "fallback path should identify the focused cmux surface: {cmux_calls:?}"
    );
    assert!(
        cmux_calls
            .lines()
            .any(|line| line == "new-split right --surface surface:source --focus true"),
        "split should target the original focused source before fallback identify: {cmux_calls:?}"
    );
    assert!(
        cmux_calls
            .lines()
            .any(|line| line == "close-surface --surface surface:new"),
        "failed lterm creation should roll back only the post-split identified cmux surface: {cmux_calls:?}"
    );
    assert!(
        !cmux_calls
            .lines()
            .any(|line| line == "close-surface --surface surface:source"),
        "rollback must not close the original source cmux surface: {cmux_calls:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_split_window_rolls_back_when_cmux_send_fails() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-cmux-bin");
    std::fs::create_dir(&fake_bin)?;
    let cmux_log = env.temp.path().join("cmux-send-failure.log");
    let fake_surface = "surface:42";
    write_executable(
        &fake_bin.join("cmux"),
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$*\" >> {}\n\
             case \"$1\" in\n\
               identify) printf '%s\\n' '{{\"focused\":{{\"surface_ref\":\"surface:source\"}}}}'; exit 0 ;;\n\
               new-split) printf '%s\\n' 'OK {fake_surface} workspace:1'; exit 0 ;;\n\
               send) printf '%s\\n' 'CMUX_SEND_FAIL' >&2; exit 43 ;;\n\
               close-surface) exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;
    let path = path_with_prepended(&fake_bin)?;
    let shell = command_path("sh")?.display().to_string();
    let sleep = shlex::try_quote(&command_path("sleep")?.display().to_string())?.into_owned();
    let payload = format!("{sleep} 2");
    let before = session_names_json(&env)?;

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace-for-send-failure")
        .env("PATH", &path)
        .args([
            "tmux-compat",
            "split-window",
            "-h",
            shell.as_str(),
            "-lc",
            payload.as_str(),
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "cmux send failure must fail split-window: {output:?}"
    );
    assert_stderr_contains(&output, "cmux send attach command failed");
    assert_stderr_contains(&output, "CMUX_SEND_FAIL");
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls.lines().any(|line| {
            let args = line.split_whitespace().collect::<Vec<_>>();
            args.first() == Some(&"close-surface")
                && args
                    .windows(2)
                    .any(|window| window == ["--surface", fake_surface])
        }),
        "cmux send failure should roll back the reported cmux surface: {cmux_calls:?}"
    );
    assert_eq!(
        session_names_json(&env)?,
        before,
        "cmux send failure must not leave an orphan lterm session"
    );
    Ok(())
}

#[test]
fn tmux_compat_wait_for_signal_wakes_multiple_waiters() -> TestResult {
    let env = TestEnv::new()?;
    let channel = "broadcast-channel";
    let mut first = ChildCleanup::new(
        env.cmd()
            .args(["tmux-compat", "wait-for", channel])
            .spawn()?,
    );
    let mut second = ChildCleanup::new(
        env.cmd()
            .args(["tmux-compat", "wait-for", channel])
            .spawn()?,
    );
    thread::sleep(Duration::from_secs(1));

    let status = env
        .cmd()
        .args(["tmux-compat", "wait-for", "-S", channel])
        .status()?;
    assert!(status.success(), "wait-for -S failed: {status:?}");
    wait_for_child_success(&mut first, "first wait-for waiter")?;
    wait_for_child_success(&mut second, "second wait-for waiter")?;

    let mut late = ChildCleanup::new(
        env.cmd()
            .args(["tmux-compat", "wait-for", channel])
            .spawn()?,
    );
    thread::sleep(Duration::from_millis(200));
    assert!(
        late.child_mut()?.try_wait()?.is_none(),
        "a waiter started after a signal must wait for the next generation"
    );
    let status = env
        .cmd()
        .args(["tmux-compat", "wait-for", "-S", channel])
        .status()?;
    assert!(status.success(), "second wait-for -S failed: {status:?}");
    wait_for_child_success(&mut late, "late wait-for waiter")?;
    Ok(())
}

#[test]
fn tmux_compat_display_message_stops_option_parsing_at_message() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "display-payload",
            "sleep 2",
        ])
        .status()?;
    assert!(status.success());

    // A live default pane is required because display-message expands formats
    // against the selected pane even when the message itself is literal text.
    let output = env
        .cmd()
        .args(["tmux-compat", "display-message", "-p", "hello", "-t"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hello -t\n");
    Ok(())
}

#[test]
fn tmux_compat_display_message_accepts_empty_format_value() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "display-empty-format",
            "sleep 2",
        ])
        .status()?;
    assert!(status.success());

    let output = env
        .cmd()
        .args(["tmux-compat", "display-message", "-p", "-F", ""])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "\n",
        "empty display-message format should print one newline"
    );
    Ok(())
}

#[test]
fn tmux_compat_reports_focus_events_enabled() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .args(["tmux-compat", "show-option", "-gqv", "focus-events"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"on\n", "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");

    let refresh = env
        .cmd()
        .args(["tmux-compat", "refresh-client", "-S"])
        .output()?;
    assert!(refresh.status.success(), "{refresh:?}");
    Ok(())
}

#[test]
fn tmux_compat_user_option_contract_preserves_legacy_no_name_and_builtin_behavior() -> TestResult {
    let env = TestEnv::new()?;

    for (args, expected) in [
        (vec!["tmux-compat", "show-option"], b"".as_slice()),
        (vec!["tmux-compat", "show-option", "-q"], b"".as_slice()),
        (
            vec!["tmux-compat", "show-option", "-v"],
            b"off\n".as_slice(),
        ),
        (
            vec!["tmux-compat", "show-option", "-g"],
            b"off\n".as_slice(),
        ),
    ] {
        let output = env.cmd().args(args).output()?;
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected, "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    }

    let mutation = env
        .cmd()
        .args(["tmux-compat", "set-option", "-g", "status", "on"])
        .output()?;
    assert!(mutation.status.success(), "{mutation:?}");
    assert!(mutation.stdout.is_empty(), "{mutation:?}");
    assert!(mutation.stderr.is_empty(), "{mutation:?}");

    let query = env
        .cmd()
        .args(["tmux-compat", "show-option", "-gqv", "status"])
        .output()?;
    assert!(query.status.success(), "{query:?}");
    assert_eq!(query.stdout, b"off\n", "{query:?}");
    assert!(query.stderr.is_empty(), "{query:?}");

    let at_prefixed_target = env
        .cmd()
        .args(["tmux-compat", "show-option", "-qv", "-t", "@42", "status"])
        .output()?;
    assert!(
        at_prefixed_target.status.success(),
        "{at_prefixed_target:?}"
    );
    assert_eq!(
        at_prefixed_target.stdout, b"off\n",
        "{at_prefixed_target:?}"
    );
    assert!(
        at_prefixed_target.stderr.is_empty(),
        "{at_prefixed_target:?}"
    );

    for (label, args) in [
        (
            "pane-scoped legacy option",
            vec!["tmux-compat", "set-option", "-p", "status", "on"],
        ),
        (
            "legacy unset",
            vec!["tmux-compat", "set-option", "-u", "status"],
        ),
        (
            "legacy pane flags without a name",
            vec!["tmux-compat", "set-option", "-p", "-t", "%999"],
        ),
        (
            "at-prefixed legacy value",
            vec![
                "tmux-compat",
                "set-option",
                "-p",
                "pane-border-status",
                "@legacy-value",
            ],
        ),
        (
            "at-prefixed target is not an option name",
            vec!["tmux-compat", "set-option", "-t", "@42", "status", "on"],
        ),
    ] {
        let output = env.cmd().args(args).output()?;
        assert!(output.status.success(), "{label}: {output:?}");
        assert!(output.stdout.is_empty(), "{label}: {output:?}");
        assert!(output.stderr.is_empty(), "{label}: {output:?}");
    }
    assert!(
        !data_store_path(&env).exists(),
        "legacy set-option compatibility no-ops must not persist user-option state"
    );
    Ok(())
}

#[test]
fn tmux_compat_user_option_contract_accepts_closed_grammar_and_separates_pane_and_root_session_scopes()
-> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "user-option-grammar")?;
    let attached_target = format!("-t{pane}");
    let mut failures = Vec::new();

    for (name, args) in [
        (
            "session set with separate target",
            vec![
                "tmux-compat",
                "set-option",
                "-t",
                pane.as_str(),
                "@owner",
                "session-value",
            ],
        ),
        (
            "pane set alias with attached target",
            vec![
                "tmux-compat",
                "set",
                "-qp",
                attached_target.as_str(),
                "@owner",
                "pane-value",
            ],
        ),
        (
            "separator set",
            vec![
                "tmux-compat",
                "set-option",
                "-t",
                pane.as_str(),
                "--",
                "@separator",
                "separator-value",
            ],
        ),
        (
            "full allowed name alphabet",
            vec![
                "tmux-compat",
                "set-option",
                "-t",
                pane.as_str(),
                "@A.z_9:-",
                "alphabet-value",
            ],
        ),
    ] {
        let output = env.cmd().args(args).output()?;
        if !output.status.success() || !output.stdout.is_empty() || !output.stderr.is_empty() {
            failures.push(format!("{name}: {output:?}"));
        }
    }

    for (name, args, expected) in [
        (
            "session show with common flag order",
            vec![
                "tmux-compat",
                "show-option",
                "-qv",
                "-t",
                pane.as_str(),
                "@owner",
            ],
            b"session-value\n".as_slice(),
        ),
        (
            "session show with option name and value",
            vec![
                "tmux-compat",
                "show-option",
                "-q",
                "-t",
                pane.as_str(),
                "@owner",
            ],
            b"@owner session-value\n".as_slice(),
        ),
        (
            "pane show alias with boolean cluster",
            vec![
                "tmux-compat",
                "show",
                "-pqv",
                attached_target.as_str(),
                "@owner",
            ],
            b"pane-value\n".as_slice(),
        ),
        (
            "separator show",
            vec![
                "tmux-compat",
                "show-option",
                "-qv",
                "-t",
                pane.as_str(),
                "--",
                "@separator",
            ],
            b"separator-value\n".as_slice(),
        ),
        (
            "flags reordered after target value",
            vec![
                "tmux-compat",
                "show-option",
                "-t",
                pane.as_str(),
                "-qv",
                "@A.z_9:-",
            ],
            b"alphabet-value\n".as_slice(),
        ),
    ] {
        let output = env.cmd().args(args).output()?;
        if !output.status.success() || output.stdout != expected || !output.stderr.is_empty() {
            failures.push(format!("{name}: {output:?}"));
        }
    }

    let unset = env
        .cmd()
        .args([
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            pane.as_str(),
            "-u",
            "@owner",
        ])
        .output()?;
    if !unset.status.success() || !unset.stdout.is_empty() || !unset.stderr.is_empty() {
        failures.push(format!("valid unset: {unset:?}"));
    }
    let absent_after_unset = env
        .cmd()
        .args([
            "tmux-compat",
            "show-option",
            "-qv",
            "-p",
            "-t",
            pane.as_str(),
            "@owner",
        ])
        .output()?;
    if !absent_after_unset.status.success()
        || !absent_after_unset.stdout.is_empty()
        || !absent_after_unset.stderr.is_empty()
    {
        failures.push(format!("quiet absence after unset: {absent_after_unset:?}"));
    }

    assert!(
        failures.is_empty(),
        "valid user-option grammar/scope failures:\n{}",
        failures.join("\n")
    );
    Ok(())
}

#[test]
fn tmux_compat_user_option_contract_rejects_closed_grammar_violations() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "user-option-invalid-grammar")?;
    let cases: &[(&str, &[&str])] = &[
        (
            "empty user-option name",
            &[
                "tmux-compat",
                "set-option",
                "-p",
                "-t",
                pane.as_str(),
                "@",
                "value",
            ],
        ),
        (
            "missing set value",
            &[
                "tmux-compat",
                "set-option",
                "-p",
                "-t",
                pane.as_str(),
                "@owner",
            ],
        ),
        (
            "unset with value",
            &[
                "tmux-compat",
                "set-option",
                "-p",
                "-t",
                pane.as_str(),
                "-u",
                "@owner",
                "extra",
            ],
        ),
        (
            "set positional over-arity",
            &[
                "tmux-compat",
                "set-option",
                "-p",
                "-t",
                pane.as_str(),
                "@owner",
                "value",
                "extra",
            ],
        ),
        (
            "show positional over-arity",
            &[
                "tmux-compat",
                "show-option",
                "-qv",
                "-p",
                "-t",
                pane.as_str(),
                "@owner",
                "extra",
            ],
        ),
        (
            "unsupported global scope",
            &[
                "tmux-compat",
                "set-option",
                "-g",
                "-p",
                "-t",
                pane.as_str(),
                "@owner",
                "value",
            ],
        ),
        (
            "unsupported server scope",
            &["tmux-compat", "set-option", "-s", "@owner", "value"],
        ),
        (
            "unsupported value-taking flag cannot hide user-option name",
            &["tmux-compat", "set-option", "-F", "@owner", "value"],
        ),
        (
            "unsupported value-taking flag with separate value cannot hide user-option name",
            &["tmux-compat", "set-option", "-F", "fmt", "@owner", "value"],
        ),
        (
            "unsupported window scope",
            &[
                "tmux-compat",
                "set-option",
                "-w",
                "-t",
                pane.as_str(),
                "@owner",
                "value",
            ],
        ),
        (
            "target flag in cluster",
            &["tmux-compat", "show-option", "-pt", pane.as_str(), "@owner"],
        ),
        (
            "attached target in cluster cannot bypass user-option parsing",
            &["tmux-compat", "show-option", "-qvt@42", "@owner"],
        ),
        (
            "target flag in set cluster cannot hide user-option name",
            &["tmux-compat", "set-option", "-pt", "%0", "@owner", "value"],
        ),
        (
            "unknown flag",
            &[
                "tmux-compat",
                "show-option",
                "-x",
                "-p",
                "-t",
                pane.as_str(),
                "@owner",
            ],
        ),
        (
            "flag after first positional",
            &[
                "tmux-compat",
                "show-option",
                "@owner",
                "-qv",
                "-p",
                "-t",
                pane.as_str(),
            ],
        ),
    ];

    let mut failures = Vec::new();
    for (name, args) in cases {
        let output = env.cmd().args(*args).output()?;
        if output.status.success() || !output.stdout.is_empty() || output.stderr.is_empty() {
            failures.push(format!("{name}: {output:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "invalid user-option grammar was accepted or misreported:\n{}",
        failures.join("\n")
    );
    assert!(
        !data_store_path(&env).exists(),
        "rejected user-option grammar must not create or mutate the option store"
    );
    Ok(())
}

#[test]
fn tmux_compat_user_option_contract_distinguishes_absence_and_present_empty() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "user-option-empty")?;
    let mut failures = Vec::new();

    let quiet_absent = env
        .cmd()
        .args([
            "tmux-compat",
            "show-option",
            "-qv",
            "-p",
            "-t",
            pane.as_str(),
            "@missing",
        ])
        .output()?;
    if !quiet_absent.status.success()
        || !quiet_absent.stdout.is_empty()
        || !quiet_absent.stderr.is_empty()
    {
        failures.push(format!("quiet absence: {quiet_absent:?}"));
    }

    let loud_absent = env
        .cmd()
        .args([
            "tmux-compat",
            "show-option",
            "-v",
            "-p",
            "-t",
            pane.as_str(),
            "@missing",
        ])
        .output()?;
    let loud_absent_reordered = env
        .cmd()
        .args([
            "tmux-compat",
            "show-option",
            "-p",
            "-t",
            pane.as_str(),
            "-v",
            "@missing",
        ])
        .output()?;
    if loud_absent.status.success()
        || !loud_absent.stdout.is_empty()
        || loud_absent.stderr.is_empty()
        || loud_absent_reordered.status.success()
        || !loud_absent_reordered.stdout.is_empty()
        || loud_absent_reordered.stderr.is_empty()
        || loud_absent.stderr != loud_absent_reordered.stderr
    {
        failures.push(format!(
            "loud absence was not a stable diagnostic: first={loud_absent:?}, reordered={loud_absent_reordered:?}"
        ));
    }

    let set_empty = env
        .cmd()
        .args([
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            pane.as_str(),
            "@omx_instance_id",
            "",
        ])
        .output()?;
    assert!(set_empty.status.success(), "{set_empty:?}");

    let present_empty = env
        .cmd()
        .args([
            "tmux-compat",
            "show-option",
            "-qv",
            "-p",
            "-t",
            pane.as_str(),
            "@omx_instance_id",
        ])
        .output()?;
    if !present_empty.status.success()
        || present_empty.stdout != b"\n"
        || !present_empty.stderr.is_empty()
    {
        failures.push(format!("present empty: {present_empty:?}"));
    }

    let present_empty_with_name = env
        .cmd()
        .args([
            "tmux-compat",
            "show-option",
            "-q",
            "-p",
            "-t",
            pane.as_str(),
            "@omx_instance_id",
        ])
        .output()?;
    if !present_empty_with_name.status.success()
        || present_empty_with_name.stdout != b"@omx_instance_id \n"
        || !present_empty_with_name.stderr.is_empty()
    {
        failures.push(format!(
            "present empty with name: {present_empty_with_name:?}"
        ));
    }
    assert!(
        failures.is_empty(),
        "absence/present-empty contract failures:\n{}",
        failures.join("\n")
    );
    Ok(())
}

#[test]
fn tmux_compat_user_option_contract_enforces_exact_name_value_and_output_bounds() -> TestResult {
    let env = TestEnv::new()?;
    let pane = create_sleep_session(&env, "user-option-bounds")?;
    let valid_name = format!("@{}", "n".repeat(127));
    let oversized_name = format!("@{}", "n".repeat(128));
    let valid_value = "v".repeat(4096);
    let oversized_value = "v".repeat(4097);
    let mut failures = Vec::new();

    let valid = env
        .cmd()
        .args([
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            pane.as_str(),
            valid_name.as_str(),
            valid_value.as_str(),
        ])
        .output()?;
    if !valid.status.success() || !valid.stdout.is_empty() || !valid.stderr.is_empty() {
        failures.push(format!("exact name/value maxima rejected: {valid:?}"));
    }
    let shown = env
        .cmd()
        .args([
            "tmux-compat",
            "show-option",
            "-qv",
            "-p",
            "-t",
            pane.as_str(),
            valid_name.as_str(),
        ])
        .output()?;
    let mut expected = valid_value.as_bytes().to_vec();
    expected.push(b'\n');
    if !shown.status.success() || shown.stdout != expected || !shown.stderr.is_empty() {
        failures.push(format!("exact maxima did not round-trip: {shown:?}"));
    }

    for (name, value) in [
        ("@ordinary_space", "ordinary printable space"),
        ("@ordinary_combining", "e\u{0301}"),
    ] {
        let output = env
            .cmd()
            .args([
                "tmux-compat",
                "set-option",
                "-p",
                "-t",
                pane.as_str(),
                name,
                value,
            ])
            .output()?;
        if !output.status.success() || !output.stdout.is_empty() || !output.stderr.is_empty() {
            failures.push(format!(
                "safe printable value rejected for {name}: {output:?}"
            ));
        }
    }

    for (label, name, value) in [
        ("oversized name", oversized_name.as_str(), "value"),
        ("invalid name alphabet", "@bad/name", "value"),
        ("oversized value", "@owner", oversized_value.as_str()),
        ("C0 control", "@owner", "line\nbreak"),
        ("DEL control", "@owner", "delete\u{7f}control"),
        ("format control", "@owner", "word\u{2060}joiner"),
        ("bidi control", "@owner", "bidi\u{202e}override"),
        ("zero-width", "@owner", "zero\u{200b}width"),
        ("Arabic number sign Cf", "@owner", "value\u{0600}"),
        ("Arabic end of ayah Cf", "@owner", "value\u{06dd}"),
        ("Syriac abbreviation mark Cf", "@owner", "value\u{070f}"),
        ("Arabic pound mark above Cf", "@owner", "value\u{0890}"),
        ("Arabic disputed end of ayah Cf", "@owner", "value\u{08e2}"),
        ("interlinear annotation Cf", "@owner", "value\u{fff9}"),
        ("Kaithi number sign Cf", "@owner", "value\u{110bd}"),
        ("Kaithi number sign above Cf", "@owner", "value\u{110cd}"),
        (
            "Egyptian hieroglyph vertical joiner Cf",
            "@owner",
            "value\u{13430}",
        ),
    ] {
        let output = env
            .cmd()
            .args([
                "tmux-compat",
                "set-option",
                "-p",
                "-t",
                pane.as_str(),
                name,
                value,
            ])
            .output()?;
        if output.status.success() || !output.stdout.is_empty() || output.stderr.is_empty() {
            failures.push(format!(
                "{label} accepted for name_bytes={} value_bytes={}: {output:?}",
                name.len(),
                value.len()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "user-option bounds/control failures:\n{}",
        failures.join("\n")
    );
    Ok(())
}

#[test]
fn tmux_compat_user_option_contract_migrates_old_store_and_keeps_window_aliases_separate()
-> TestResult {
    let env = TestEnv::new()?;
    let data_dir = env.temp.path().join("data");
    std::fs::create_dir_all(&data_dir)?;
    #[cfg(unix)]
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        data_dir.join("tmux-compat-store.json"),
        br#"{"panes":{},"wait_generations":{},"wait_generation_touched_secs":{}}"#,
    )?;
    let pane = create_sleep_session(&env, "user-option-old-store")?;
    let mut failures = Vec::new();

    for alias in ["setw", "set-window-option"] {
        let window_alias = env
            .cmd()
            .args([
                "tmux-compat",
                alias,
                "-p",
                "-t",
                pane.as_str(),
                "@owner",
                "window-value",
            ])
            .output()?;
        if !window_alias.status.success()
            || !window_alias.stdout.is_empty()
            || !window_alias.stderr.is_empty()
        {
            failures.push(format!("{alias} legacy no-op: {window_alias:?}"));
        }
    }

    let absent = env
        .cmd()
        .args([
            "tmux-compat",
            "show-option",
            "-qv",
            "-p",
            "-t",
            pane.as_str(),
            "@owner",
        ])
        .output()?;
    if !absent.status.success() || !absent.stdout.is_empty() || !absent.stderr.is_empty() {
        failures.push(format!("window aliases mutated pane storage: {absent:?}"));
    }

    for alias in ["showw", "show-window-option", "show-window-options"] {
        let window_show = env
            .cmd()
            .args([
                "tmux-compat",
                alias,
                "-v",
                "-p",
                "-t",
                pane.as_str(),
                "@owner",
            ])
            .output()?;
        if !window_show.status.success()
            || window_show.stdout != b"off\n"
            || !window_show.stderr.is_empty()
        {
            failures.push(format!("{alias} reached pane storage: {window_show:?}"));
        }
    }

    let set = env
        .cmd()
        .args([
            "tmux-compat",
            "set",
            "-p",
            "-t",
            pane.as_str(),
            "@owner",
            "pane-value",
        ])
        .output()?;
    if !set.status.success() || !set.stdout.is_empty() || !set.stderr.is_empty() {
        failures.push(format!("set alias: {set:?}"));
    }
    let shown = env
        .cmd()
        .args([
            "tmux-compat",
            "show",
            "-qv",
            "-p",
            "-t",
            pane.as_str(),
            "@owner",
        ])
        .output()?;
    if !shown.status.success() || shown.stdout != b"pane-value\n" || !shown.stderr.is_empty() {
        failures.push(format!("show alias: {shown:?}"));
    }

    let store: serde_json::Value = serde_json::from_slice(&std::fs::read(data_store_path(&env))?)?;
    if store.get("pane_user_options").is_none() {
        failures.push(format!(
            "old store lacks pane_user_options after mutation: {store:?}"
        ));
    }
    if store.get("session_user_options").is_none() {
        failures.push(format!(
            "old store lacks session_user_options after mutation: {store:?}"
        ));
    }
    assert!(
        failures.is_empty(),
        "migration/window-alias failures:\n{}",
        failures.join("\n")
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn tmux_compat_user_option_contract_combines_scope_cap_and_persists_immutable_root_ids()
-> TestResult {
    let env = TestEnv::new()?;
    let root = fake_live_session(0);
    let root_id = root["id"].as_str().ok_or("root id missing")?.to_string();
    let mut pane_values = serde_json::Map::new();
    let mut session_values = serde_json::Map::new();
    for index in 0..32 {
        pane_values.insert(
            format!("@p{index:02}"),
            serde_json::json!(format!("p{index}")),
        );
        session_values.insert(
            format!("@s{index:02}"),
            serde_json::json!(format!("s{index}")),
        );
    }
    let mut pane_options = serde_json::Map::new();
    pane_options.insert(root_id.clone(), serde_json::Value::Object(pane_values));
    let mut session_options = serde_json::Map::new();
    session_options.insert(root_id.clone(), serde_json::Value::Object(session_values));
    write_user_option_store(&env, pane_options, session_options, serde_json::Map::new())?;

    let overwrite = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            "%0",
            "@p00",
            "overwritten",
        ],
        vec![root.clone()],
    )?;
    assert!(overwrite.status.success(), "{overwrite:?}");
    let at_cap = std::fs::read(data_store_path(&env))?;
    let at_cap_store: serde_json::Value = serde_json::from_slice(&at_cap)?;
    assert_eq!(
        at_cap_store["pane_user_options"][&root_id]["@p00"],
        "overwritten"
    );

    let rejected = run_tmux_with_fake_sessions(
        &env,
        &["tmux-compat", "set-option", "-t", "%0", "@new", "rejected"],
        vec![root.clone()],
    )?;
    assert!(!rejected.status.success(), "{rejected:?}");
    assert_stderr_contains(&rejected, "per-identity limit reached");
    assert_eq!(std::fs::read(data_store_path(&env))?, at_cap);

    let unset = run_tmux_with_fake_sessions(
        &env,
        &["tmux-compat", "set-option", "-u", "-p", "-t", "%0", "@p01"],
        vec![root.clone()],
    )?;
    assert!(unset.status.success(), "{unset:?}");
    let admitted = run_tmux_with_fake_sessions(
        &env,
        &["tmux-compat", "set-option", "-t", "%0", "@new", "admitted"],
        vec![root.clone()],
    )?;
    assert!(admitted.status.success(), "{admitted:?}");

    let mut renamed_root = root.clone();
    renamed_root["name"] = serde_json::json!("renamed-root");
    let shown = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "show-option",
            "-qv",
            "-t",
            "renamed-root",
            "@new",
        ],
        vec![renamed_root.clone()],
    )?;
    assert!(shown.status.success(), "{shown:?}");
    assert_eq!(shown.stdout, b"admitted\n");

    let mut child = fake_live_session(1);
    child["parent_pane_id"] = serde_json::json!("%0");
    child["parent_session_id"] = serde_json::json!(root_id.clone());
    let child_set = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "set-option",
            "-t",
            "%1",
            "@new",
            "from-child",
        ],
        vec![renamed_root, child.clone()],
    )?;
    assert!(child_set.status.success(), "{child_set:?}");
    let final_store: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data_store_path(&env))?)?;
    assert_eq!(
        final_store["session_user_options"][&root_id]["@new"],
        "from-child"
    );
    assert!(
        final_store["session_user_options"]
            .get(child["id"].as_str().unwrap())
            .is_none()
    );
    let encoded = final_store.to_string();
    for mutable_identity in [
        "%0",
        "%1",
        "fake-session-0",
        "fake-session-1",
        "renamed-root",
    ] {
        assert!(
            !encoded.contains(mutable_identity),
            "store used mutable identity {mutable_identity}"
        );
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn tmux_compat_user_option_contract_enforces_exact_512_combined_identity_cap_atomically()
-> TestResult {
    let env = TestEnv::new()?;
    let sessions: Vec<_> = (0..513).map(fake_live_session).collect();
    let mut pane_options = serde_json::Map::new();
    let mut session_options = serde_json::Map::new();
    for (index, session) in sessions.iter().take(512).enumerate() {
        let mut values = serde_json::Map::new();
        values.insert("@owner".to_string(), serde_json::json!("original"));
        let options = if index % 2 == 0 {
            &mut pane_options
        } else {
            &mut session_options
        };
        options.insert(
            session["id"].as_str().ok_or("fake id missing")?.to_string(),
            serde_json::Value::Object(values),
        );
    }
    write_user_option_store(&env, pane_options, session_options, serde_json::Map::new())?;
    let overwrite = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            "%0",
            "@owner",
            "updated",
        ],
        sessions.clone(),
    )?;
    assert!(overwrite.status.success(), "{overwrite:?}");
    let at_cap = std::fs::read(data_store_path(&env))?;
    let rejected = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            "%512",
            "@owner",
            "new",
        ],
        sessions,
    )?;
    assert!(!rejected.status.success(), "{rejected:?}");
    assert_stderr_contains(&rejected, "identity limit reached");
    assert_eq!(std::fs::read(data_store_path(&env))?, at_cap);
    Ok(())
}

#[test]
#[cfg(unix)]
fn tmux_compat_user_option_contract_enforces_4096_entries_and_16mib_store_atomically() -> TestResult
{
    let env = TestEnv::new()?;
    let sessions: Vec<_> = (0..65).map(fake_live_session).collect();
    let mut pane_options = serde_json::Map::new();
    let mut session_options = serde_json::Map::new();
    for session in sessions.iter().take(64) {
        let mut pane_values = serde_json::Map::new();
        let mut session_values = serde_json::Map::new();
        for option in 0..32 {
            pane_values.insert(format!("@p{option:02}"), serde_json::json!("value"));
            session_values.insert(format!("@s{option:02}"), serde_json::json!("value"));
        }
        let identity = session["id"].as_str().ok_or("fake id missing")?;
        pane_options.insert(identity.to_string(), serde_json::Value::Object(pane_values));
        session_options.insert(
            identity.to_string(),
            serde_json::Value::Object(session_values),
        );
    }
    write_user_option_store(&env, pane_options, session_options, serde_json::Map::new())?;
    let overwrite = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            "%0",
            "@p00",
            "updated",
        ],
        sessions.clone(),
    )?;
    assert!(overwrite.status.success(), "{overwrite:?}");
    let at_entry_cap = std::fs::read(data_store_path(&env))?;
    let rejected = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            "%64",
            "@new",
            "value",
        ],
        sessions,
    )?;
    assert!(!rejected.status.success(), "{rejected:?}");
    assert_stderr_contains(&rejected, "entry limit reached");
    assert_eq!(std::fs::read(data_store_path(&env))?, at_entry_cap);

    const STORE_LIMIT: usize = 16 * 1024 * 1024;
    let large_env = TestEnv::new()?;
    let mut wait_generations = serde_json::Map::new();
    wait_generations.insert("x".repeat(STORE_LIMIT - 2_048), serde_json::json!(1));
    let before = write_user_option_store(
        &large_env,
        serde_json::Map::new(),
        serde_json::Map::new(),
        wait_generations,
    )?;
    assert!(
        before.len() < STORE_LIMIT,
        "seed store unexpectedly oversized"
    );
    let value = "v".repeat(4_096);
    let oversized = run_tmux_with_fake_sessions(
        &large_env,
        &[
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            "%0",
            "@large",
            &value,
        ],
        vec![fake_live_session(0)],
    )?;
    assert!(!oversized.status.success(), "{oversized:?}");
    assert_stderr_contains(&oversized, "store exceeds");
    assert_eq!(std::fs::read(data_store_path(&large_env))?, before);
    Ok(())
}

#[test]
#[cfg(unix)]
fn tmux_compat_user_option_contract_preserves_store_when_atomic_temp_write_fails() -> TestResult {
    let env = TestEnv::new()?;
    let root = fake_live_session(0);
    let root_id = root["id"].as_str().ok_or("root id missing")?.to_string();
    let mut values = serde_json::Map::new();
    values.insert("@owner".to_string(), serde_json::json!("original"));
    let mut session_options = serde_json::Map::new();
    session_options.insert(root_id.clone(), serde_json::Value::Object(values));
    let before = write_user_option_store(
        &env,
        serde_json::Map::new(),
        session_options,
        serde_json::Map::new(),
    )?;
    let store_path = data_store_path(&env);
    let tmp_path = store_path.with_extension("json.tmp");
    std::fs::create_dir(&tmp_path)?;

    let rejected = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "set-option",
            "-t",
            "%0",
            "@owner",
            "rejected",
        ],
        vec![root.clone()],
    )?;
    assert!(!rejected.status.success(), "{rejected:?}");
    assert_stderr_contains(&rejected, "tmux-compat-store.json.tmp");
    let after_failure = std::fs::read(&store_path)?;
    assert_eq!(after_failure, before, "failed save changed store bytes");
    let failed_store: serde_json::Value = serde_json::from_slice(&after_failure)?;
    assert_eq!(
        failed_store["session_user_options"][&root_id]["@owner"], "original",
        "failed save partially mutated logical state"
    );

    std::fs::remove_dir(&tmp_path)?;
    let updated = run_tmux_with_fake_sessions(
        &env,
        &["tmux-compat", "set-option", "-t", "%0", "@owner", "updated"],
        vec![root],
    )?;
    assert!(updated.status.success(), "{updated:?}");
    let final_store: serde_json::Value = serde_json::from_slice(&std::fs::read(&store_path)?)?;
    assert_eq!(
        final_store["session_user_options"][&root_id]["@owner"],
        "updated"
    );
    assert!(!tmp_path.exists(), "successful save left tmp artifact");
    Ok(())
}

#[test]
#[cfg(unix)]
fn tmux_compat_g003_successful_pane_kill_removes_only_captured_immutable_id() -> TestResult {
    let env = TestEnv::new()?;
    let killed_pane = create_sleep_session(&env, "g003-pane-killed")?;
    let survivor_pane = create_sleep_session(&env, "g003-pane-survivor")?;
    let killed = read_session_json(&env, "g003-pane-killed")?;
    let survivor = read_session_json(&env, "g003-pane-survivor")?;
    let killed_id = killed["id"].as_str().ok_or("killed id missing")?;
    let survivor_id = survivor["id"].as_str().ok_or("survivor id missing")?;

    for (pane, value) in [
        (killed_pane.as_str(), "killed-value"),
        (survivor_pane.as_str(), "survivor-value"),
    ] {
        for pane_scope in [false, true] {
            let mut args = vec!["tmux-compat", "set-option"];
            if pane_scope {
                args.push("-p");
            }
            args.extend(["-t", pane, "@owner", value]);
            let output = env.cmd().args(args).output()?;
            assert!(output.status.success(), "{output:?}");
        }
    }

    let killed_output = env
        .cmd()
        .args(["tmux-compat", "kill-pane", "-t", killed_pane.as_str()])
        .output()?;
    assert!(killed_output.status.success(), "{killed_output:?}");
    let store: serde_json::Value = serde_json::from_slice(&std::fs::read(data_store_path(&env))?)?;
    for scope in ["pane_user_options", "session_user_options"] {
        assert!(
            store[scope].get(killed_id).is_none(),
            "successful pane kill retained captured immutable id {killed_id} in {scope}: {store}"
        );
        assert_eq!(
            store[scope][survivor_id]["@owner"], "survivor-value",
            "pane kill removed or changed unrelated immutable id {survivor_id} in {scope}: {store}"
        );
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn tmux_compat_g003_successful_session_kill_removes_root_and_descendant_immutable_ids() -> TestResult
{
    let env = TestEnv::new()?;
    let root_pane = create_sleep_session(&env, "g003-session-root")?;
    let survivor_pane = create_sleep_session(&env, "g003-session-survivor")?;
    let sleep = command_path("sleep")?.display().to_string();
    let child_output = env
        .cmd()
        .args([
            "tmux-compat",
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            root_pane.as_str(),
            sleep.as_str(),
            "30",
        ])
        .output()?;
    assert!(child_output.status.success(), "{child_output:?}");
    let child_pane = String::from_utf8(child_output.stdout)?.trim().to_string();
    let all_output = env.cmd().args(["sessions", "--json", "--all"]).output()?;
    assert!(all_output.status.success(), "{all_output:?}");
    let all: Vec<serde_json::Value> = serde_json::from_slice(&all_output.stdout)?;
    let root_id = all
        .iter()
        .find(|row| row["pane_id"].as_str() == Some(root_pane.as_str()))
        .and_then(|row| row["id"].as_str())
        .ok_or("root immutable id missing")?;
    let child_id = all
        .iter()
        .find(|row| row["pane_id"].as_str() == Some(child_pane.as_str()))
        .and_then(|row| row["id"].as_str())
        .ok_or("child immutable id missing")?;
    let survivor_id = all
        .iter()
        .find(|row| row["pane_id"].as_str() == Some(survivor_pane.as_str()))
        .and_then(|row| row["id"].as_str())
        .ok_or("survivor immutable id missing")?;

    for pane in [&root_pane, &child_pane, &survivor_pane] {
        let output = env
            .cmd()
            .args([
                "tmux-compat",
                "set-option",
                "-p",
                "-t",
                pane.as_str(),
                "@owner",
                "pane-value",
            ])
            .output()?;
        assert!(output.status.success(), "{output:?}");
    }
    for (target, value) in [
        (child_pane.as_str(), "root-session-value"),
        (survivor_pane.as_str(), "survivor-session-value"),
    ] {
        let output = env
            .cmd()
            .args(["tmux-compat", "set-option", "-t", target, "@owner", value])
            .output()?;
        assert!(output.status.success(), "{output:?}");
    }

    let killed = env
        .cmd()
        .args(["tmux-compat", "kill-session", "-t", "g003-session-root"])
        .output()?;
    assert!(killed.status.success(), "{killed:?}");
    let store: serde_json::Value = serde_json::from_slice(&std::fs::read(data_store_path(&env))?)?;
    for removed_id in [root_id, child_id] {
        assert!(
            store["pane_user_options"].get(removed_id).is_none(),
            "successful session kill retained pane identity {removed_id}: {store}"
        );
        assert!(
            store["session_user_options"].get(removed_id).is_none(),
            "successful session kill retained session identity {removed_id}: {store}"
        );
    }
    assert_eq!(
        store["pane_user_options"][survivor_id]["@owner"], "pane-value",
        "session cleanup removed unrelated pane identity: {store}"
    );
    assert_eq!(
        store["session_user_options"][survivor_id]["@owner"], "survivor-session-value",
        "session cleanup removed unrelated session identity: {store}"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn tmux_compat_g003_failed_kill_retains_live_user_option_values() -> TestResult {
    let env = TestEnv::new()?;
    let live = fake_live_session(0);
    let live_id = live["id"].as_str().ok_or("live id missing")?;
    let mut values = serde_json::Map::new();
    values.insert("@owner".to_string(), serde_json::json!("retained"));
    let mut pane_options = serde_json::Map::new();
    pane_options.insert(
        live_id.to_string(),
        serde_json::Value::Object(values.clone()),
    );
    let mut session_options = serde_json::Map::new();
    session_options.insert(live_id.to_string(), serde_json::Value::Object(values));
    let before =
        write_user_option_store(&env, pane_options, session_options, serde_json::Map::new())?;
    let (output, requests) = run_tmux_with_fake_failed_kill(
        &env,
        &["tmux-compat", "kill-pane", "-t", "%0"],
        vec![live],
    )?;
    assert!(!output.status.success(), "{output:?}");
    assert_stderr_contains(&output, "injected fake daemon kill failure");
    assert!(
        requests.iter().any(|request| request == "info"),
        "{requests:?}"
    );
    assert!(
        requests.iter().any(|request| request == "kill"),
        "{requests:?}"
    );
    assert_eq!(
        std::fs::read(data_store_path(&env))?,
        before,
        "failed kill changed persisted user-option values"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn tmux_compat_g003_remember_pane_reconciles_natural_exit_identities() -> TestResult {
    let env = TestEnv::new()?;
    let stale_id = "00000000-0000-4000-8000-ffffffffffff";
    let mut values = serde_json::Map::new();
    values.insert("@owner".to_string(), serde_json::json!("stale"));
    let mut pane_options = serde_json::Map::new();
    pane_options.insert(
        stale_id.to_string(),
        serde_json::Value::Object(values.clone()),
    );
    let mut session_options = serde_json::Map::new();
    session_options.insert(stale_id.to_string(), serde_json::Value::Object(values));
    write_user_option_store(&env, pane_options, session_options, serde_json::Map::new())?;

    create_sleep_session(&env, "g003-remember-reconcile")?;
    let store: serde_json::Value = serde_json::from_slice(&std::fs::read(data_store_path(&env))?)?;
    assert!(
        store["pane_user_options"].get(stale_id).is_none()
            && store["session_user_options"].get(stale_id).is_none(),
        "remember_pane did not reconcile naturally exited immutable id {stale_id}: {store}"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn tmux_compat_g003_mutations_prune_reused_pane_ids_and_empty_maps_idempotently() -> TestResult {
    let env = TestEnv::new()?;
    let mut old = fake_live_session(0);
    old["id"] = serde_json::json!("00000000-0000-4000-8000-00000000aaaa");
    let old_id = old["id"].as_str().ok_or("old id missing")?;
    let mut replacement = fake_live_session(0);
    replacement["id"] = serde_json::json!("00000000-0000-4000-8000-00000000bbbb");
    replacement["name"] = serde_json::json!("replacement-session");
    let replacement_id = replacement["id"].as_str().ok_or("replacement id missing")?;
    let mut values = serde_json::Map::new();
    values.insert("@owner".to_string(), serde_json::json!("old-owner"));
    let mut pane_options = serde_json::Map::new();
    pane_options.insert(
        old_id.to_string(),
        serde_json::Value::Object(values.clone()),
    );
    pane_options.insert(
        "00000000-0000-4000-8000-00000000cccc".to_string(),
        serde_json::json!({}),
    );
    let mut session_options = serde_json::Map::new();
    session_options.insert(old_id.to_string(), serde_json::Value::Object(values));
    write_user_option_store(&env, pane_options, session_options, serde_json::Map::new())?;

    let absent = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "show-option",
            "-qv",
            "-p",
            "-t",
            "%0",
            "@owner",
        ],
        vec![replacement.clone()],
    )?;
    assert!(absent.status.success(), "{absent:?}");
    assert!(
        absent.stdout.is_empty() && absent.stderr.is_empty(),
        "{absent:?}"
    );

    let set = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            "%0",
            "@owner",
            "new-owner",
        ],
        vec![replacement.clone()],
    )?;
    assert!(set.status.success(), "{set:?}");
    let after_set: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data_store_path(&env))?)?;
    assert!(
        after_set["pane_user_options"].get(old_id).is_none(),
        "{after_set}"
    );
    assert!(
        after_set["session_user_options"].get(old_id).is_none(),
        "{after_set}"
    );
    assert_eq!(
        after_set["pane_user_options"][replacement_id]["@owner"],
        "new-owner"
    );

    for _ in 0..2 {
        let unset = run_tmux_with_fake_sessions(
            &env,
            &[
                "tmux-compat",
                "set-option",
                "-u",
                "-p",
                "-t",
                "%0",
                "@owner",
            ],
            vec![replacement.clone()],
        )?;
        assert!(unset.status.success(), "{unset:?}");
    }
    let final_store: serde_json::Value =
        serde_json::from_slice(&std::fs::read(data_store_path(&env))?)?;
    assert!(
        final_store["pane_user_options"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty),
        "idempotent cleanup retained empty inner maps: {final_store}"
    );
    assert!(
        final_store["session_user_options"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty),
        "mutation reconciliation retained stale session maps: {final_store}"
    );
    Ok(())
}

// Pinned contract mirror, not authentication proof. Provenance:
// oh-my-codex 0.20.2 release artifact,
// src/scripts/notify-hook/managed-tmux.ts pane-first ownership evaluator.
fn omx_pane_first_binds(
    candidate: &str,
    pane_read: Result<&str, ()>,
    session_read: Result<Option<&str>, ()>,
) -> bool {
    match pane_read {
        Ok("") => matches!(session_read, Ok(Some(value)) if value == candidate),
        Ok(value) => value == candidate,
        Err(()) => false,
    }
}

#[test]
fn tmux_compat_user_option_contract_pins_omx_pane_first_classification_matrix() {
    let candidate = "omx-instance-a";
    let cases = [
        ("exact pane", Ok(candidate), Ok(None), true),
        (
            "pane mismatch rejects exact session fallback",
            Ok("omx-instance-b"),
            Ok(Some(candidate)),
            false,
        ),
        (
            "pane read error rejects exact session fallback",
            Err(()),
            Ok(Some(candidate)),
            false,
        ),
        (
            "empty pane permits exact session fallback",
            Ok(""),
            Ok(Some(candidate)),
            true,
        ),
        ("total absence rejects", Ok(""), Ok(None), false),
    ];

    for (name, pane, session, expected) in cases {
        assert_eq!(
            omx_pane_first_binds(candidate, pane, session),
            expected,
            "{name}"
        );
    }
}

#[test]
#[cfg(unix)]
fn tmux_compat_g003_executes_exact_raw_omx_vectors_and_list_format() -> TestResult {
    let env = TestEnv::new()?;
    let first = fake_live_session(0);
    let second = fake_live_session(1);
    let sessions = vec![first.clone(), second.clone()];
    let candidate = "omx-instance-a";

    for args in [
        vec![
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            "%0",
            "@omx_pane_instance_id",
            candidate,
        ],
        vec![
            "tmux-compat",
            "set-option",
            "-t",
            "fake-session-0",
            "@omx_instance_id",
            candidate,
        ],
        vec![
            "tmux-compat",
            "set-option",
            "-p",
            "-t",
            "%1",
            "@omx_pane_instance_id",
            "must-not-leak",
        ],
    ] {
        let output = run_tmux_with_fake_sessions(&env, &args, sessions.clone())?;
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{output:?}"
        );
    }

    let pane = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "show-option",
            "-qv",
            "-p",
            "-t",
            "%0",
            "@omx_pane_instance_id",
        ],
        sessions.clone(),
    )?;
    let session = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "show-option",
            "-qv",
            "-t",
            "fake-session-0",
            "@omx_instance_id",
        ],
        sessions.clone(),
    )?;
    assert_eq!(pane.stdout, b"omx-instance-a\n", "{pane:?}");
    assert_eq!(session.stdout, b"omx-instance-a\n", "{session:?}");
    assert!(pane.stderr.is_empty() && session.stderr.is_empty());
    let pane_value = String::from_utf8(pane.stdout)?.trim().to_string();
    let session_value = String::from_utf8(session.stdout)?.trim().to_string();
    assert!(omx_pane_first_binds(
        candidate,
        Ok(&pane_value),
        Ok(Some(&session_value))
    ));

    let list = run_tmux_with_fake_sessions(
        &env,
        &[
            "tmux-compat",
            "list-sessions",
            "-F",
            "#{session_name}\t#{@omx_instance_id}",
        ],
        sessions,
    )?;
    assert!(list.status.success(), "{list:?}");
    assert!(list.stderr.is_empty(), "{list:?}");
    assert_eq!(
        list.stdout, b"fake-session-0\tomx-instance-a\nfake-session-1\t\n",
        "exact OMX list vector must expand the root option and an empty absent field without literal/pane leakage: {list:?}"
    );
    let encoded = String::from_utf8(list.stdout)?;
    assert!(!encoded.contains("#{@omx_instance_id}"), "{encoded:?}");
    assert!(!encoded.contains("must-not-leak"), "{encoded:?}");
    assert!(
        !encoded.contains("%0") && !encoded.contains("%1"),
        "{encoded:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_missing_target_value_does_not_fall_back_to_default() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "target-required",
            "sleep 30",
        ])
        .status()?;
    assert!(status.success());
    wait_for_session_present(&env, "target-required")?;

    let output = env
        .cmd()
        .args(["tmux-compat", "kill-pane", "-t"])
        .output()?;
    assert!(
        !output.status.success(),
        "missing -t value must be rejected instead of killing the default pane: {output:?}"
    );
    let listed = env.cmd().args(["sessions", "--all"]).output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("target-required\t")),
        "session should survive rejected kill-pane -t: {stdout:?}"
    );
    Ok(())
}

#[test]
fn input_sends_text_to_pty() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "input-alias",
            "--",
            "sh",
            "-lc",
            "echo READY; read line; echo INPUT:$line; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    env.capture_until("input-alias", "READY")?;
    let status = env
        .cmd()
        .args(["input", "input-alias", "hello", "--enter"])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("input-alias", "INPUT:hello")?;
    assert!(captured.contains("READY"), "{captured}");
    assert!(captured.contains("INPUT:hello"), "{captured}");
    Ok(())
}

#[test]
#[cfg(unix)]
fn parser_degradation_keeps_wait_input_and_attach_live() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);
    let name = "parser-degrade-live";

    let status = env
        .cmd()
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_INTERNAL_TEST_DEGRADE_TERMINAL_PARSER", "1")
        .args([
            "new",
            "--detach",
            "--name",
            name,
            "--",
            "sh",
            "-lc",
            "echo READY; read first; echo GOT:$first; read second; echo GOT2:$second; sleep 2",
        ])
        .status()?;
    assert!(status.success(), "lterm new should succeed");
    wait_for_socket(&socket)?;

    let ready = env.capture_until(name, "READY")?;
    assert!(ready.contains("READY"), "{ready}");

    let wait_output = env
        .cmd()
        .args([
            "wait",
            name,
            "--contains",
            "__LTERM_TEST_NEVER_MATCH_PARSER_DEGRADE__",
            "--timeout",
            "200ms",
            "--json",
        ])
        .output()?;
    assert_eq!(wait_output.status.code(), Some(124), "{wait_output:?}");
    let wait_json: serde_json::Value = serde_json::from_slice(&wait_output.stdout)?;
    assert_eq!(
        wait_json["timed_out"], true,
        "wait should time out rather than report output-closed after parser degradation: {wait_json}"
    );
    assert_eq!(wait_json["matched"], false);

    let status = env
        .cmd()
        .args(["input", name, "AFTER_DEGRADE", "--enter"])
        .status()?;
    assert!(status.success(), "input command should keep writing to PTY");
    let captured = env.capture_until(name, "GOT:AFTER_DEGRADE")?;
    assert!(
        captured.contains("GOT:AFTER_DEGRADE"),
        "capture/log path must keep receiving PTY output after parser degradation: {captured}"
    );

    let (mut stream, _subscriber_id) = attach_with_geometry(&socket, name, 24, 80)?;
    wait_for_size(&env, name, (24, 80))?;
    let session = read_session_json(&env, name)?;
    assert_eq!(
        session
            .get("attached_clients")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "degraded parser must not prevent a live attach subscription: {session}"
    );

    let status = env.cmd().args(["send", name, "SECOND\n"]).status()?;
    assert!(status.success(), "send command should keep writing to PTY");
    let live = read_until_marker_bytes(&mut stream, b"GOT2:SECOND", Duration::from_secs(5))?;
    assert!(
        live.windows(b"GOT2:SECOND".len())
            .any(|window| window == b"GOT2:SECOND"),
        "attached stream should receive live output after parser degradation: {:?}",
        String::from_utf8_lossy(&live)
    );
    Ok(())
}

#[test]
fn send_alias_sends_text_to_pty() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "send-alias",
            "--",
            "sh",
            "-lc",
            "echo READY; read line; echo SEND:$line; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    env.capture_until("send-alias", "READY")?;
    let status = env
        .cmd()
        .args(["send", "send-alias", "hello", "--enter"])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("send-alias", "SEND:hello")?;
    assert!(captured.contains("READY"), "{captured}");
    assert!(captured.contains("SEND:hello"), "{captured}");
    Ok(())
}

#[test]
fn compose_once_message_appends_enter_by_default() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "compose-enter",
            "--",
            "sh",
            "-lc",
            "echo READY; read line; echo COMPOSE:$line; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    env.capture_until("compose-enter", "READY")?;
    let output = env
        .cmd()
        .args(["compose", "compose-enter", "--once", "--message", "hello"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("READY"),
        "one-shot compose should print sanitized capture output: {stdout:?}"
    );

    let captured = env.capture_until("compose-enter", "COMPOSE:hello")?;
    assert!(captured.contains("COMPOSE:hello"), "{captured}");
    Ok(())
}

#[test]
fn mobile_once_message_appends_enter_by_default() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "mobile-enter",
            "--",
            "sh",
            "-lc",
            "echo READY; read line; echo MOBILE:$line; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    env.capture_until("mobile-enter", "READY")?;
    let status = env
        .cmd()
        .args(["mobile", "mobile-enter", "--once", "--message", "hello"])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("mobile-enter", "MOBILE:hello")?;
    assert!(captured.contains("MOBILE:hello"), "{captured}");
    Ok(())
}

#[test]
fn compose_once_no_enter_sends_exact_message_bytes() -> TestResult {
    once_no_enter_sends_exact_message_bytes("compose", "compose-no-enter")
}

#[test]
fn mobile_once_no_enter_sends_exact_message_bytes() -> TestResult {
    once_no_enter_sends_exact_message_bytes("mobile", "mobile-no-enter")
}

fn once_no_enter_sends_exact_message_bytes(command: &str, name: &str) -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            name,
            "--",
            "sh",
            "-lc",
            "echo READY; stty raw -echo min 0 time 5; printf 'RAW_READY\\r\\n'; bytes=$(dd bs=1 count=4 2>/dev/null | od -An -tx1 | tr -d ' \\n'); stty sane 2>/dev/null || true; printf 'HEX:%s\\n' \"$bytes\"; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    env.capture_until(name, "RAW_READY")?;
    let status = env
        .cmd()
        .args([command, name, "--once", "--message", "hey", "--no-enter"])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until(name, "HEX:686579")?;
    assert!(captured.contains("HEX:686579"), "{captured}");
    assert!(
        !captured.contains("HEX:6865790d"),
        "--no-enter must not append carriage return: {captured}"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn compose_once_does_not_change_attached_clients_or_geometry() -> TestResult {
    once_does_not_change_attached_clients_or_geometry("compose", "compose-geometry")
}

#[test]
#[cfg(unix)]
fn mobile_once_does_not_change_attached_clients_or_geometry() -> TestResult {
    once_does_not_change_attached_clients_or_geometry("mobile", "mobile-geometry")
}

#[test]
#[cfg(unix)]
fn compose_interactive_does_not_change_attached_clients_or_geometry() -> TestResult {
    interactive_does_not_change_attached_clients_or_geometry(
        "compose",
        "compose-interactive-geometry",
    )
}

#[test]
#[cfg(unix)]
fn mobile_interactive_does_not_change_attached_clients_or_geometry() -> TestResult {
    interactive_does_not_change_attached_clients_or_geometry(
        "mobile",
        "mobile-interactive-geometry",
    )
}

#[cfg(unix)]
fn once_does_not_change_attached_clients_or_geometry(command: &str, name: &str) -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            name,
            "--",
            "sh",
            "-lc",
            "echo READY; read line; echo GOT:$line; sleep 30",
        ])
        .status()?;
    assert!(status.success());

    wait_for_socket(&socket)?;
    env.capture_until(name, "READY")?;
    let (_attached_stream, _subscriber_id) = attach_with_geometry(&socket, name, 40, 152)?;
    wait_for_size(&env, name, (40, 152))?;

    let before = read_session_json(&env, name)?;
    assert_eq!(
        before
            .get("attached_clients")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "pre-compose attached client count should reflect exactly one live attach: {before}"
    );

    let output = env
        .cmd()
        .args([command, name, "--once", "--message", "hello"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    env.capture_until(name, "GOT:hello")?;

    let after = read_session_json(&env, name)?;
    assert_eq!(
        after
            .get("attached_clients")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "compose must not create or drop attach subscribers: {after}"
    );
    assert_eq!(
        (
            after.get("rows").and_then(serde_json::Value::as_u64),
            after.get("cols").and_then(serde_json::Value::as_u64)
        ),
        (Some(40), Some(152)),
        "compose must not resize the raw attach PTY geometry: {after}"
    );
    Ok(())
}

#[cfg(unix)]
fn interactive_does_not_change_attached_clients_or_geometry(
    command: &str,
    name: &str,
) -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            name,
            "--",
            "sh",
            "-lc",
            "echo READY; sleep 30",
        ])
        .status()?;
    assert!(status.success());

    wait_for_socket(&socket)?;
    env.capture_until(name, "READY")?;
    let (_attached_stream, _subscriber_id) = attach_with_geometry(&socket, name, 40, 152)?;
    wait_for_size(&env, name, (40, 152))?;

    let before = read_session_json(&env, name)?;
    assert_eq!(
        before
            .get("attached_clients")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "pre-compose attached client count should reflect exactly one live attach: {before}"
    );

    let (mut master, slave) = open_pty_pair()?;
    set_pty_window_size(&slave, 24, 80)?;
    let stdin = Stdio::from(slave.try_clone()?);
    let stdout = Stdio::from(slave.try_clone()?);
    let mut compose = ChildCleanup::new(
        env.cmd()
            .args([command, name, "--refresh", "50ms"])
            .stdin(stdin)
            .stdout(stdout)
            .stderr(Stdio::null())
            .spawn()?,
    );
    drop(slave);
    read_until_marker_bytes(&mut master, b"> ", Duration::from_secs(5))?;
    master.write_all(b"\x1b")?;
    master.flush()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while Instant::now() < deadline {
        if let Some(status) = compose.child_mut()?.try_wait()? {
            assert!(
                status.success(),
                "interactive compose exited unsuccessfully after local Esc: {status}"
            );
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(exited, "interactive compose did not exit after local Esc");

    let after = read_session_json(&env, name)?;
    assert_eq!(
        after
            .get("attached_clients")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "interactive compose must not create or drop attach subscribers: {after}"
    );
    assert_eq!(
        (
            after.get("rows").and_then(serde_json::Value::as_u64),
            after.get("cols").and_then(serde_json::Value::as_u64)
        ),
        (Some(40), Some(152)),
        "interactive compose must not resize the raw attach PTY geometry: {after}"
    );
    Ok(())
}

#[test]
fn tmux_compat_display_message_honors_format_flag() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "display-format",
            "sleep 2",
        ])
        .status()?;
    assert!(status.success());

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "display-message",
            "-p",
            "-t",
            "display-format",
            "-F",
            "#{pane_id}",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with('%'),
        "{output:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_list_windows_reports_pseudo_window_metadata() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "window-query",
            "sleep 60",
        ])
        .status()?;
    assert!(status.success());

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "list-windows",
            "-t",
            "window-query",
            "-F",
            "#{session_name}:#{window_index}:#{window_name}:#{window_id}:#{window_panes}:#{window_active}:#{pane_width}:#{window_width}:#{pane_height}:#{window_height}:#{history_size}",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let row = stdout
        .lines()
        .find(|line| line.starts_with("window-query:0:window-query:@"))
        .ok_or_else(|| format!("window-query row missing: {stdout:?}"))?;
    let fields: Vec<_> = row.split(':').collect();
    assert_eq!(fields.len(), 11, "{row:?}");
    let window_id = fields[3]
        .strip_prefix('@')
        .ok_or_else(|| format!("window_id missing @ prefix: {row:?}"))?;
    assert!(!window_id.is_empty(), "{row:?}");
    assert!(window_id.chars().all(|ch| ch.is_ascii_digit()), "{row:?}");
    assert_eq!(fields[4], "1", "{row:?}");
    assert_eq!(fields[5], "1", "{row:?}");
    assert_eq!(fields[6], fields[7], "{row:?}");
    assert_eq!(fields[8], fields[9], "{row:?}");
    assert!(fields[6].parse::<u16>()? > 0, "{row:?}");
    assert!(fields[8].parse::<u16>()? > 0, "{row:?}");
    assert_eq!(fields[10], "0", "{row:?}");
    Ok(())
}

#[test]
fn tmux_compat_list_clients_reports_attached_lterm_client() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args([
            "run",
            "--tmux",
            "--no-status",
            "--",
            "sh",
            "-lc",
            "tmux list-clients -F '#{client_name}:#{client_session}:#{client_pane}:pid=#{client_pid}:end'",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line.starts_with("lterm:")
            && line
                .split(':')
                .nth(2)
                .is_some_and(|pane| pane.starts_with('%'))),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().all(|line| line.ends_with(":pid=:end")),
        "client_pid is intentionally unsupported and must expand empty, not as hazardous fake pid 0: {stdout:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_list_clients_honors_target_and_attached_row_count() -> TestResult {
    let env = TestEnv::new()?;
    for name in ["client-one", "client-two"] {
        let status = env
            .cmd()
            .args(["new", "--detach", "-n", name, "--", "sh", "-lc", "sleep 60"])
            .status()?;
        assert!(status.success());
    }

    let mut attach_one_a = ChildCleanup::new(
        env.cmd()
            .args(["attach", "client-one", "--no-status"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let attach_one_a_stdin = attach_one_a
        .child_mut()?
        .stdin
        .take()
        .ok_or("missing attach stdin")?;
    let mut attach_one_b = ChildCleanup::new(
        env.cmd()
            .args(["attach", "client-one", "--no-status"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let attach_one_b_stdin = attach_one_b
        .child_mut()?
        .stdin
        .take()
        .ok_or("missing attach stdin")?;
    let mut attach_two = ChildCleanup::new(
        env.cmd()
            .args(["attach", "client-two", "--no-status"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let attach_two_stdin = attach_two
        .child_mut()?
        .stdin
        .take()
        .ok_or("missing attach stdin")?;

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = String::new();
    let mut all_clients_seen = false;
    while Instant::now() < deadline {
        let output = env
            .cmd()
            .args([
                "tmux-compat",
                "list-clients",
                "-F",
                "#{client_session}:#{client_pane}",
            ])
            .output()?;
        if !output.status.success() {
            last = format!("{output:?}");
            break;
        }
        last = String::from_utf8_lossy(&output.stdout).to_string();
        let lines: Vec<_> = last.lines().collect();
        let client_one_rows = lines
            .iter()
            .filter(|line| line.starts_with("client-one:%"))
            .count();
        let client_two_rows = lines
            .iter()
            .filter(|line| line.starts_with("client-two:%"))
            .count();
        let unexpected_rows = lines
            .iter()
            .filter(|line| !line.starts_with("client-one:%") && !line.starts_with("client-two:%"))
            .count();
        if lines.len() == 3 && client_one_rows == 2 && client_two_rows == 1 && unexpected_rows == 0
        {
            all_clients_seen = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !all_clients_seen {
        drop(attach_one_a_stdin);
        drop(attach_one_b_stdin);
        drop(attach_two_stdin);
        let _ = attach_one_a.kill_and_wait();
        let _ = attach_one_b.kill_and_wait();
        let _ = attach_two.kill_and_wait();
        return Err(format!("timed out waiting for all client rows: {last:?}").into());
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let output = env
            .cmd()
            .args([
                "tmux-compat",
                "list-clients",
                "-t",
                "client-one",
                "-F",
                "#{client_session}:#{client_pane}:pid=#{client_pid}:end",
            ])
            .output()?;
        if !output.status.success() {
            last = format!("{output:?}");
            break;
        }
        last = String::from_utf8_lossy(&output.stdout).to_string();
        let lines: Vec<_> = last.lines().collect();
        if lines.len() == 2 && lines.iter().all(|line| line.starts_with("client-one:%")) {
            // lterm exposes the attached pane, not a per-client id, so two
            // clients attached to the same pane intentionally render as two
            // identical client rows.
            if !lines.iter().all(|line| line.ends_with(":pid=:end")) {
                drop(attach_one_a_stdin);
                drop(attach_one_b_stdin);
                drop(attach_two_stdin);
                let _ = attach_one_a.kill_and_wait();
                let _ = attach_one_b.kill_and_wait();
                let _ = attach_two.kill_and_wait();
                return Err(format!(
                    "client_pid must expand empty, not as hazardous fake pid 0: {last:?}"
                )
                .into());
            }
            drop(attach_one_a_stdin);
            drop(attach_one_b_stdin);
            drop(attach_two_stdin);
            attach_one_a.kill_and_wait()?;
            attach_one_b.kill_and_wait()?;
            attach_two.kill_and_wait()?;
            wait_for_no_client_rows(&env, &["client-one", "client-two"])?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    drop(attach_one_a_stdin);
    drop(attach_one_b_stdin);
    drop(attach_two_stdin);
    let _ = attach_one_a.kill_and_wait();
    let _ = attach_one_b.kill_and_wait();
    let _ = attach_two.kill_and_wait();
    Err(format!("timed out waiting for targeted client rows: {last:?}").into())
}

#[test]
fn tmux_compat_list_windows_defaults_to_current_target_unless_all() -> TestResult {
    let env = TestEnv::new()?;
    for name in ["window-one", "window-two", "foo-target"] {
        let status = env
            .cmd()
            .args(["tmux-compat", "new-session", "-d", "-s", name, "sleep 60"])
            .status()?;
        assert!(status.success());
    }
    let listed = env.cmd().arg("ls").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    let window_one = list_row(&stdout, "window-one")
        .ok_or_else(|| format!("window-one row missing: {stdout:?}"))?;
    let window_one_pane = window_one
        .get(1)
        .ok_or_else(|| format!("window-one row missing pane id: {window_one:?}"))?;

    let current = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-F", "#{session_name}"])
        .output()?;
    assert!(current.status.success(), "{current:?}");
    assert_eq!(
        String::from_utf8_lossy(&current.stdout).trim(),
        "window-one"
    );

    let attached_target = env
        .cmd()
        .args([
            "tmux-compat",
            "list-windows",
            "-tfoo-target",
            "-F",
            "#{session_name}",
        ])
        .output()?;
    assert!(attached_target.status.success(), "{attached_target:?}");
    assert_eq!(
        String::from_utf8_lossy(&attached_target.stdout).trim(),
        "foo-target"
    );

    let all = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-a", "-F", "#{session_name}"])
        .output()?;
    assert!(all.status.success(), "{all:?}");
    let stdout = String::from_utf8_lossy(&all.stdout);
    assert_exact_line_set(&stdout, &["window-one", "window-two", "foo-target"]);

    let clustered = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-aF", "#{session_name}"])
        .output()?;
    assert!(clustered.status.success(), "{clustered:?}");
    let stdout = String::from_utf8_lossy(&clustered.stdout);
    assert_exact_line_set(&stdout, &["window-one", "window-two", "foo-target"]);

    let clustered_inline = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-aF#{session_name}"])
        .output()?;
    assert!(clustered_inline.status.success(), "{clustered_inline:?}");
    let stdout = String::from_utf8_lossy(&clustered_inline.stdout);
    assert_exact_line_set(&stdout, &["window-one", "window-two", "foo-target"]);

    let clustered_equals_inline = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-aF=#{session_name}"])
        .output()?;
    assert!(
        clustered_equals_inline.status.success(),
        "{clustered_equals_inline:?}"
    );
    let stdout = String::from_utf8_lossy(&clustered_equals_inline.stdout);
    assert_exact_line_set(&stdout, &["window-one", "window-two", "foo-target"]);

    let literal_format = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-F", "-a"])
        .output()?;
    assert!(literal_format.status.success(), "{literal_format:?}");
    assert_eq!(String::from_utf8_lossy(&literal_format.stdout).trim(), "-a");

    let inline_format = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-F#{session_name}"])
        .output()?;
    assert!(inline_format.status.success(), "{inline_format:?}");
    assert_eq!(
        String::from_utf8_lossy(&inline_format.stdout).trim(),
        "window-one"
    );

    let equals_format = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-F=#{session_name}"])
        .output()?;
    assert!(equals_format.status.success(), "{equals_format:?}");
    assert_eq!(
        String::from_utf8_lossy(&equals_format.stdout).trim(),
        "window-one"
    );
    let target_like_format = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-F", "-t#{session_name}"])
        .output()?;
    assert!(
        target_like_format.status.success(),
        "{target_like_format:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&target_like_format.stdout).trim(),
        "-twindow-one"
    );

    let unsupported_filter = env
        .cmd()
        .args(["tmux-compat", "list-windows", "-f", "#{session_name}"])
        .output()?;
    assert!(
        !unsupported_filter.status.success(),
        "{unsupported_filter:?}"
    );
    let unsupported_clustered_filter = env
        .cmd()
        .args(["tmux-compat", "list-windows", "-af", "#{session_name}"])
        .output()?;
    assert!(
        !unsupported_clustered_filter.status.success(),
        "{unsupported_clustered_filter:?}"
    );
    let unsupported_pane_filter = env
        .cmd()
        .args(["tmux-compat", "list-panes", "-f", "#{pane_id}"])
        .output()?;
    assert!(
        !unsupported_pane_filter.status.success(),
        "{unsupported_pane_filter:?}"
    );
    let scoped_pane_format = env
        .cmd()
        .args(["tmux-compat", "list-panes", "-s", "-F", "literal"])
        .output()?;
    assert!(
        scoped_pane_format.status.success(),
        "{scoped_pane_format:?}"
    );
    let stdout = String::from_utf8_lossy(&scoped_pane_format.stdout);
    assert!(
        !stdout.trim().is_empty() && stdout.lines().all(|line| line == "literal"),
        "{stdout:?}"
    );
    let unsupported_scoped_pane_filter = env
        .cmd()
        .args(["tmux-compat", "list-panes", "-s", "-f", "#{pane_id}"])
        .output()?;
    assert!(
        !unsupported_scoped_pane_filter.status.success(),
        "{unsupported_scoped_pane_filter:?}"
    );

    let dash_dash_literal = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args([
            "tmux-compat",
            "list-windows",
            "-F",
            "#{session_name}",
            "--",
            "-f",
            "#{session_name}",
        ])
        .output()?;
    assert!(dash_dash_literal.status.success(), "{dash_dash_literal:?}");
    assert_eq!(
        String::from_utf8_lossy(&dash_dash_literal.stdout).trim(),
        "window-one"
    );
    Ok(())
}

#[test]
fn tmux_compat_list_windows_resolves_child_target_to_root_window() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "window-parent",
            "--",
            "sh",
            "-lc",
            "\"$LTERM_BIN\" new --detach -n window-child -- sh -lc 'sleep 60' && echo CHILD_READY; sleep 60",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("window-parent", "CHILD_READY")?;

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "list-windows",
            "-t",
            "window-child",
            "-F",
            "#{session_name}:#{window_name}:#{window_id}",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "{stdout:?}");
    assert!(
        lines[0].starts_with("window-parent:window-parent:@"),
        "{stdout:?}"
    );

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "window-decoy",
            "--",
            "sh",
            "-lc",
            "sleep 60",
        ])
        .status()?;
    assert!(status.success());

    let mut attach_parent = ChildCleanup::new(
        env.cmd()
            .args(["attach", "window-parent", "--no-status"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let attach_parent_stdin = attach_parent
        .child_mut()?
        .stdin
        .take()
        .ok_or("missing attach stdin")?;
    let mut attach_decoy = ChildCleanup::new(
        env.cmd()
            .args(["attach", "window-decoy", "--no-status"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let attach_decoy_stdin = attach_decoy
        .child_mut()?
        .stdin
        .take()
        .ok_or("missing attach stdin")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = String::new();
    let mut all_clients_seen = false;
    while Instant::now() < deadline {
        let output = env
            .cmd()
            .args([
                "tmux-compat",
                "list-clients",
                "-F",
                "#{client_session}:#{client_pane}",
            ])
            .output()?;
        if !output.status.success() {
            last = format!("{output:?}");
            break;
        }
        last = String::from_utf8_lossy(&output.stdout).to_string();
        let lines: Vec<_> = last.lines().collect();
        let parent_rows = lines
            .iter()
            .filter(|line| line.starts_with("window-parent:%"))
            .count();
        let decoy_rows = lines
            .iter()
            .filter(|line| line.starts_with("window-decoy:%"))
            .count();
        if lines.len() == 2 && parent_rows == 1 && decoy_rows == 1 {
            all_clients_seen = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !all_clients_seen {
        drop(attach_parent_stdin);
        drop(attach_decoy_stdin);
        let _ = attach_parent.kill_and_wait();
        let _ = attach_decoy.kill_and_wait();
        return Err(format!("timed out waiting for parent and decoy clients: {last:?}").into());
    }

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "list-clients",
            "-t",
            "window-child",
            "-F",
            "#{client_session}:#{client_pane}",
        ])
        .output()?;
    if !output.status.success() {
        drop(attach_parent_stdin);
        drop(attach_decoy_stdin);
        let _ = attach_parent.kill_and_wait();
        let _ = attach_decoy.kill_and_wait();
        return Err(format!("child-target list-clients failed: {output:?}").into());
    }
    last = String::from_utf8_lossy(&output.stdout).to_string();
    let lines: Vec<_> = last.lines().collect();
    if lines.len() != 1 || !lines[0].starts_with("window-parent:%") {
        drop(attach_parent_stdin);
        drop(attach_decoy_stdin);
        let _ = attach_parent.kill_and_wait();
        let _ = attach_decoy.kill_and_wait();
        return Err(format!(
            "child-target list-clients did not filter to exactly the root client session: {last:?}"
        )
        .into());
    }

    drop(attach_parent_stdin);
    drop(attach_decoy_stdin);
    attach_parent.kill_and_wait()?;
    attach_decoy.kill_and_wait()?;
    wait_for_no_client_rows(&env, &["window-parent", "window-decoy"])?;
    Ok(())
}

#[test]
fn tmux_compat_rename_session_updates_session_name() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "tmux-rename-old",
            "sleep 60",
        ])
        .status()?;
    assert!(status.success());

    let renamed = env
        .cmd()
        .args([
            "tmux-compat",
            "rename-session",
            "-t",
            "tmux-rename-old",
            "tmux-rename-new",
        ])
        .output()?;
    assert!(renamed.status.success(), "{renamed:?}");

    let renamed_again = env
        .cmd()
        .args([
            "tmux-compat",
            "rename-session",
            "tmux-rename-final",
            "-t",
            "tmux-rename-new",
        ])
        .output()?;
    assert!(renamed_again.status.success(), "{renamed_again:?}");

    let idempotent = env
        .cmd()
        .args([
            "tmux-compat",
            "rename-session",
            "-t",
            "tmux-rename-final",
            "tmux-rename-final",
        ])
        .output()?;
    assert!(idempotent.status.success(), "{idempotent:?}");

    let listed = env
        .cmd()
        .args(["tmux-compat", "list-sessions", "-F", "#{session_name}"])
        .output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.lines().any(|line| line == "tmux-rename-final"),
        "{stdout}"
    );
    assert!(
        !stdout.lines().any(|line| line == "tmux-rename-old"),
        "{stdout}"
    );
    assert!(
        !stdout.lines().any(|line| line == "tmux-rename-new"),
        "{stdout}"
    );

    let history = env
        .cmd()
        .args(["metadata", "history", "tmux-rename-final", "--json"])
        .output()?;
    assert!(history.status.success(), "{history:?}");
    let history: serde_json::Value = serde_json::from_slice(&history.stdout)?;
    assert_eq!(history["cursor"], 2);
    assert_eq!(history["entries"].as_array().map(Vec::len), Some(2));
    assert_eq!(history["entries"][0]["operation"], "rename");
    assert_eq!(history["entries"][1]["operation"], "rename");
    assert_eq!(history["current"]["name"], "tmux-rename-final");

    let old = env
        .cmd()
        .args(["tmux-compat", "has-session", "-t", "tmux-rename-old"])
        .status()?;
    assert!(!old.success());
    let previous = env
        .cmd()
        .args(["tmux-compat", "has-session", "-t", "tmux-rename-new"])
        .status()?;
    assert!(!previous.success());
    let new = env
        .cmd()
        .args(["tmux-compat", "has-session", "-t", "tmux-rename-final"])
        .status()?;
    assert!(new.success());

    env.cmd()
        .args(["tmux-compat", "kill-session", "-t", "tmux-rename-final"])
        .status()?;
    wait_for_session_absent(&env, "tmux-rename-final")?;
    Ok(())
}

#[test]
fn tmux_compat_list_commands_includes_agent_query_surface() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "has-alias",
            "sleep 60",
        ])
        .status()?;
    assert!(status.success());
    let status = env
        .cmd()
        .args(["tmux-compat", "has", "-t", "has-alias"])
        .status()?;
    assert!(status.success());
    for args in [
        vec!["tmux-compat", "set-environment", "LTERM_TEST_VAR", "1"],
        vec!["tmux-compat", "setenv", "LTERM_TEST_VAR", "1"],
        vec![
            "tmux-compat",
            "set-hook",
            "-t",
            "#{session_id}",
            "client-resized[867272301]",
            "run-shell",
            "-b",
            "tmux resize-pane -t %1 -y 2",
        ],
        vec![
            "tmux-compat",
            "set-hook",
            "-u",
            "-t",
            "#{session_id}",
            "client-resized[867272301]",
        ],
    ] {
        let status = env.cmd().args(args).status()?;
        assert!(status.success());
    }
    for args in [
        vec!["tmux-compat", "show-environment", "LTERM_TEST_VAR"],
        vec!["tmux-compat", "showenv", "LTERM_TEST_VAR"],
    ] {
        let output = env.cmd().args(args).output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(
            output.stdout.is_empty(),
            "show-environment is a compatibility no-op and should not synthesize values: {output:?}"
        );
    }

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "list-commands",
            "-F",
            "cmd=#{command_list_name}:#{command_list_alias}:#{command_list_usage}",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for (command, alias) in [
        ("list-windows", "lsw"),
        ("list-clients", "lsc"),
        ("list-commands", "lscm"),
        ("attach-session", "attach"),
        ("has-session", "has"),
        ("rename-session", "rename"),
        ("set-environment", "setenv"),
        ("set-hook", "seth"),
        ("show-environment", "showenv"),
    ] {
        let expected = format!("cmd={command}:{alias}:");
        assert!(
            stdout.lines().any(|line| line.starts_with(&expected)),
            "{command} missing from list-commands output: {stdout:?}"
        );
    }
    let list_commands_row = stdout
        .lines()
        .find(|line| line.starts_with("cmd=list-commands:lscm:"))
        .ok_or_else(|| format!("list-commands row missing: {stdout:?}"))?;
    assert!(
        list_commands_row.contains("[-F format]"),
        "list-commands usage field missing: {list_commands_row:?}"
    );
    let verbose = env
        .cmd()
        .args(["tmux-compat", "list-commands", "--verbose", "has"])
        .output()?;
    assert!(verbose.status.success(), "{verbose:?}");
    assert_eq!(
        String::from_utf8_lossy(&verbose.stdout).trim(),
        "has-session\thas\tfull\t[-t target-session]"
    );
    let json_output = env
        .cmd()
        .args(["tmux-compat", "list-commands", "--json", "show-option"])
        .output()?;
    assert!(json_output.status.success(), "{json_output:?}");
    let commands: serde_json::Value = serde_json::from_slice(&json_output.stdout)?;
    let row = commands
        .as_array()
        .and_then(|rows| rows.first())
        .ok_or("missing list-commands --json row")?;
    assert_eq!(
        row.get("name").and_then(|value| value.as_str()),
        Some("show-options")
    );
    assert_eq!(
        row.get("alias").and_then(|value| value.as_str()),
        Some("show")
    );
    assert_eq!(
        row.get("support").and_then(|value| value.as_str()),
        Some("partial")
    );
    let unsupported_filter = env
        .cmd()
        .args(["tmux-compat", "list-commands", "-f", "#{command_name}"])
        .output()?;
    assert!(
        !unsupported_filter.status.success(),
        "{unsupported_filter:?}"
    );

    for (alias, expected) in [
        ("has", "has-session:has"),
        ("a", "attach-session:attach"),
        ("rename", "rename-session:rename"),
        ("show-option", "show-options:show"),
        ("show-window-option", "show-window-options:showw"),
    ] {
        let filtered = env
            .cmd()
            .args([
                "tmux-compat",
                "list-commands",
                "-F",
                "#{command_list_name}:#{command_list_alias}",
                alias,
            ])
            .output()?;
        assert!(filtered.status.success(), "{filtered:?}");
        assert_eq!(String::from_utf8_lossy(&filtered.stdout).trim(), expected);
    }
    let unsupported_command = env
        .cmd()
        .env("LTERM_DEBUG_TMUX", "1")
        .args(["tmux-compat", "definitely-not-supported", "-x"])
        .output()?;
    assert!(
        !unsupported_command.status.success(),
        "{unsupported_command:?}"
    );
    let stderr = String::from_utf8_lossy(&unsupported_command.stderr);
    assert!(
        stderr.contains("lterm_tmux_compat\tunsupported_command\tdefinitely-not-supported\t-x"),
        "{stderr:?}"
    );
    assert!(
        stderr.contains("tmux-compat list-commands"),
        "unsupported command error should include discovery hint: {stderr:?}"
    );
    Ok(())
}

#[test]
fn tmux_compat_quotes_multi_arg_commands() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "quoted",
            "sh",
            "-c",
            "printf '%s\\n' \"$1\"; sleep 2",
            "sh",
            "a;b",
        ])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("quoted", "a;b")?;
    assert!(captured.contains("a;b"), "{captured}");
    Ok(())
}

#[test]
fn tmux_mode_keeps_lterm_shim_ahead_of_existing_tmux() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    let fake_tmux = fake_bin.join("tmux");
    std::fs::write(
        &fake_tmux,
        "#!/bin/sh\necho FAKE_TMUX_SHOULD_NOT_RUN\nexit 99\n",
    )?;
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_tmux)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_tmux, perms)?;
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    let output = env
        .cmd()
        .env("PATH", format!("{}:{old_path}", fake_bin.display()))
        .stdin(Stdio::null())
        .args([
            "run",
            "--tmux",
            "--no-status",
            "--",
            "sh",
            "-lc",
            "printf 'TMUX_BIN:%s\\n' \"$(command -v tmux)\"; tmux list-panes -t \"$TMUX_PANE\" -F '#{pane_id}'",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("FAKE_TMUX_SHOULD_NOT_RUN"),
        "fake tmux won PATH precedence: {stdout:?}"
    );
    assert!(
        !stdout.contains(&fake_tmux.display().to_string()),
        "command -v tmux resolved fake tmux: {stdout:?}"
    );
    assert!(stdout.contains("%0"), "{stdout:?}");
    Ok(())
}

#[test]
fn tmux_mode_lterm_shim_precedence_probe_fast_fails_without_live_socket() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(
        &fake_bin.join("tmux"),
        r#"#!/bin/sh
tmux_socket=${TMUX%%,*}
printf 'FAKE_TMUX_SOCKET:%s\n' "$tmux_socket"
printf 'FAKE_LTERM_SOCKET:%s\n' "$LTERM_SOCKET"
if [ "$tmux_socket" = "$LTERM_SOCKET" ]; then
  echo BAD_LIVE_SOCKET
  exit 42
fi
exit 1
"#,
    )?;
    let fake_bin = shlex::try_quote(&fake_bin.display().to_string())?.into_owned();
    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args([
            "run",
            "--tmux",
            "--no-status",
            "--",
            "sh",
            "-lc",
            &format!(
                "export PATH={fake_bin}:$PATH; \
                 tmux display-message -p '#{{extended-keys-format}}' || true; \
                 echo SHADOW_AFTER"
            ),
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FAKE_TMUX_SOCKET:"), "{stdout:?}");
    assert!(stdout.contains("FAKE_LTERM_SOCKET:"), "{stdout:?}");
    assert!(
        !stdout.contains("BAD_LIVE_SOCKET"),
        "shadowed real tmux must not see the live lterm daemon socket in TMUX: {stdout:?}"
    );
    assert!(
        stdout.contains("SHADOW_AFTER"),
        "shadowed real tmux probe should fail fast and let the child continue: {stdout:?}"
    );
    Ok(())
}

#[test]
fn run_no_tmux_does_not_inject_lterm_tmux_shim() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args([
            "run",
            "--no-tmux",
            "--no-status",
            "--",
            "sh",
            "-c",
            "printf 'PATH:%s\\n' \"$PATH\"",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let shim_dir = env.temp.path().join("data").join("shims");
    let shim_dir = shim_dir.display().to_string();
    assert!(
        !stdout.contains(&shim_dir),
        "lterm shim directory should not be injected when --no-tmux is set: {stdout:?}"
    );
    Ok(())
}

#[test]
fn run_defaults_to_lterm_tmux_shim() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args([
            "run",
            "--no-status",
            "--",
            "sh",
            "-c",
            "printf 'TMUX_BIN:%s\\n' \"$(command -v tmux)\"",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let shim_tmux = env.temp.path().join("data").join("shims").join("tmux");
    let shim_tmux = shim_tmux.display().to_string();
    assert!(
        stdout.contains(&shim_tmux),
        "run should inject the lterm tmux shim by default: {stdout:?}"
    );
    Ok(())
}

#[test]
fn run_exports_session_identity_env_to_child_process() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args([
            "run",
            "--no-tmux",
            "--no-status",
            "--",
            "sh",
            "-c",
            "printf 'SESSION:%s\\nPANE:%s\\n' \"$LTERM_SESSION\" \"$LTERM_PANE\"",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let session = stdout
        .lines()
        .find_map(|line| line.strip_prefix("SESSION:"))
        .ok_or_else(|| format!("run output missing session identity: {stdout:?}"))?;
    let pane = stdout
        .lines()
        .find_map(|line| line.strip_prefix("PANE:"))
        .ok_or_else(|| format!("run output missing pane identity: {stdout:?}"))?;
    assert!(!session.trim().is_empty(), "{stdout:?}");
    assert!(pane.starts_with('%'), "{stdout:?}");
    Ok(())
}

#[test]
fn run_hidden_tmux_flag_keeps_default_shim() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args([
            "run",
            "--tmux",
            "--no-status",
            "--",
            "sh",
            "-c",
            "printf 'TMUX_BIN:%s\\n' \"$(command -v tmux)\"",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let shim_tmux = env.temp.path().join("data").join("shims").join("tmux");
    let shim_tmux = shim_tmux.display().to_string();
    assert!(
        stdout.contains(&shim_tmux),
        "hidden run --tmux compatibility flag should keep the default shim: {stdout:?}"
    );
    Ok(())
}

#[test]
fn tmux_mode_list_shows_user_command_not_internal_path_prefix() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--tmux",
            "-n",
            "clean-command",
            "--",
            "sh",
            "-lc",
            "sleep 2",
        ])
        .status()?;
    assert!(status.success());

    let output = env.cmd().arg("list").output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("clean-command"), "{stdout:?}");
    assert!(stdout.contains("sh -lc"), "{stdout:?}");
    assert!(
        !stdout.contains("PATH="),
        "internal PATH prefix leaked into list output: {stdout:?}"
    );
    Ok(())
}

#[test]
fn agent_command_uses_unique_name_when_base_is_occupied() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    let fake_omx = fake_bin.join("omx");
    std::fs::write(&fake_omx, "#!/bin/sh\necho FAKE_OMX_NEW\nsleep 0.1\n")?;
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&fake_omx)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_omx, perms)?;
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{old_path}", fake_bin.display());
    let status = env
        .cmd()
        .env("PATH", &path)
        .args([
            "new",
            "--detach",
            "-n",
            "omx-lterm",
            "--",
            "sh",
            "-lc",
            "echo EXISTING_AGENT; sleep 5",
        ])
        .status()?;
    assert!(status.success());

    let output = env
        .cmd()
        .env("PATH", &path)
        .stdin(Stdio::null())
        .arg("omx")
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FAKE_OMX_NEW"), "{stdout:?}");
    assert!(
        !stdout.contains("EXISTING_AGENT"),
        "should start a new uniquely named agent session instead of attaching the occupied base name: {stdout:?}"
    );
    Ok(())
}

#[test]
fn agents_lists_builtin_profiles_and_path_availability() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    let config_path = env.temp.path().join("agents.json");
    std::fs::create_dir(&fake_bin)?;
    write_executable(&fake_bin.join("codex"), "#!/bin/sh\nexit 0\n")?;
    write_executable(&fake_bin.join("opencode"), "#!/bin/sh\nexit 0\n")?;
    write_executable(&fake_bin.join("kiro-cli"), "#!/bin/sh\nexit 0\n")?;
    write_executable(&fake_bin.join("agy"), "#!/bin/sh\nexit 0\n")?;
    write_executable(&fake_bin.join("my-agent"), "#!/bin/sh\nexit 0\n")?;
    write_executable(&fake_bin.join("helper"), "#!/bin/sh\nexit 0\n")?;
    std::fs::write(
        &config_path,
        r#"{
  "profiles": [
    {
      "name": "repo-review",
      "binary": "codex",
      "session_base": "repo-review-session",
      "status_default": false
    },
    { "name": "helper" }
  ]
}"#,
    )?;
    let path = std::env::join_paths([fake_bin.as_path()])?;

    let output = env.cmd().env("PATH", &path).arg("agents").output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    assert_eq!(
        lines.next(),
        Some("PROFILE\tBINARY\tSESSION_BASE\tSTATUS\tAVAILABLE\tPATH\tKIND"),
        "{stdout:?}"
    );
    let profile_names: Vec<_> = lines
        .clone()
        .filter_map(|line| line.split('\t').next())
        .collect();
    assert_eq!(
        profile_names,
        [
            "claude",
            "codex",
            "opencode",
            "copilot",
            "cursor-agent",
            "agy",
            "jules",
            "kiro",
            "aider",
            "goose",
            "amp",
            "crush",
            "gemini",
            "kimi",
            "qwen",
            "omx",
            "omc",
        ],
        "{stdout:?}"
    );

    let codex = stdout
        .lines()
        .find(|line| line.starts_with("codex\t"))
        .ok_or("missing codex row")?;
    let fields: Vec<_> = codex.split('\t').collect();
    assert_eq!(fields.len(), 7, "{codex:?}");
    assert_eq!(fields[2], "codex-lterm", "{codex:?}");
    assert_eq!(fields[3], "off", "{codex:?}");
    assert_eq!(fields[4], "available", "{codex:?}");
    assert!(fields[5].ends_with("/codex"), "{codex:?}");
    assert_eq!(fields[6], "built-in", "{codex:?}");

    let opencode = stdout
        .lines()
        .find(|line| line.starts_with("opencode\t"))
        .ok_or("missing opencode row")?;
    let fields: Vec<_> = opencode.split('\t').collect();
    assert_eq!(fields.len(), 7, "{opencode:?}");
    assert_eq!(fields[1], "opencode", "{opencode:?}");
    assert_eq!(fields[2], "opencode-lterm", "{opencode:?}");
    assert_eq!(fields[3], "off", "{opencode:?}");
    assert_eq!(fields[4], "available", "{opencode:?}");
    assert!(fields[5].ends_with("/opencode"), "{opencode:?}");
    assert_eq!(fields[6], "built-in", "{opencode:?}");

    let agy = stdout
        .lines()
        .find(|line| line.starts_with("agy\t"))
        .ok_or("missing agy row")?;
    let fields: Vec<_> = agy.split('\t').collect();
    assert_eq!(fields.len(), 7, "{agy:?}");
    assert_eq!(fields[2], "agy-lterm", "{agy:?}");
    assert_eq!(fields[3], "off", "{agy:?}");
    assert_eq!(fields[4], "available", "{agy:?}");
    assert!(fields[5].ends_with("/agy"), "{agy:?}");
    assert_eq!(fields[6], "built-in", "{agy:?}");

    let kiro = stdout
        .lines()
        .find(|line| line.starts_with("kiro\t"))
        .ok_or("missing kiro row")?;
    let fields: Vec<_> = kiro.split('\t').collect();
    assert_eq!(fields.len(), 7, "{kiro:?}");
    assert_eq!(fields[1], "kiro-cli", "{kiro:?}");
    assert_eq!(fields[2], "kiro-lterm", "{kiro:?}");
    assert_eq!(fields[3], "off", "{kiro:?}");
    assert_eq!(fields[4], "available", "{kiro:?}");
    assert!(fields[5].ends_with("/kiro-cli"), "{kiro:?}");
    assert_eq!(fields[6], "built-in", "{kiro:?}");

    let omx = stdout
        .lines()
        .find(|line| line.starts_with("omx\t"))
        .ok_or("missing omx row")?;
    let fields: Vec<_> = omx.split('\t').collect();
    assert_eq!(fields[3], "off", "{omx:?}");
    assert_eq!(fields[4], "missing", "{omx:?}");
    assert_eq!(fields[5], "-", "{omx:?}");
    assert_eq!(fields[6], "built-in", "{omx:?}");

    let json = env
        .cmd()
        .env("PATH", &path)
        .args(["agents", "--json"])
        .output()?;
    assert!(json.status.success(), "{json:?}");
    let profiles: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    let profiles = profiles
        .as_array()
        .ok_or("agent profiles JSON should be an array")?;
    assert_eq!(profiles.len(), 17, "{profiles:?}");
    let codex = profiles
        .iter()
        .find(|row| row["profile"] == "codex")
        .ok_or("missing codex JSON row")?;
    assert_eq!(codex["available"], true);
    assert!(
        codex["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/codex")
    );
    let opencode = profiles
        .iter()
        .find(|row| row["profile"] == "opencode")
        .ok_or("missing opencode JSON row")?;
    assert_eq!(opencode["status_default"], false);
    assert_eq!(opencode["available"], true);
    assert!(
        opencode["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/opencode")
    );
    let agy = profiles
        .iter()
        .find(|row| row["profile"] == "agy")
        .ok_or("missing agy JSON row")?;
    assert_eq!(agy["status_default"], false);
    assert_eq!(agy["available"], true);
    assert!(agy["path"].as_str().unwrap_or_default().ends_with("/agy"));
    let kiro = profiles
        .iter()
        .find(|row| row["profile"] == "kiro")
        .ok_or("missing kiro JSON row")?;
    assert_eq!(kiro["binary"], "kiro-cli");
    assert_eq!(kiro["session_base"], "kiro-lterm");
    assert_eq!(kiro["status_default"], false);
    assert_eq!(kiro["available"], true);
    assert!(
        kiro["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/kiro-cli")
    );
    let omx = profiles
        .iter()
        .find(|row| row["profile"] == "omx")
        .ok_or("missing omx JSON row")?;
    assert_eq!(omx["status_default"], false);
    assert_eq!(omx["available"], false);
    assert!(omx["path"].is_null());

    let custom = env
        .cmd()
        .env("PATH", &path)
        .args(["agents", "--json", "codex", "my-agent"])
        .output()?;
    assert!(custom.status.success(), "{custom:?}");
    let profiles: serde_json::Value = serde_json::from_slice(&custom.stdout)?;
    let profiles = profiles
        .as_array()
        .ok_or("selected agent profiles JSON should be an array")?;
    assert_eq!(profiles.len(), 2, "{profiles:?}");
    assert_eq!(profiles[0]["profile"], "codex");
    assert_eq!(profiles[0]["kind"], "built-in");
    assert_eq!(profiles[0]["available"], true);
    assert_eq!(profiles[1]["profile"], "my-agent");
    assert_eq!(profiles[1]["kind"], "custom");
    assert_eq!(profiles[1]["session_base"], "my-agent-lterm");
    assert_eq!(profiles[1]["status_default"], true);
    assert_eq!(profiles[1]["available"], true);
    assert!(
        profiles[1]["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/my-agent")
    );

    let configured = env
        .cmd()
        .env("PATH", &path)
        .args([
            "agents",
            "--agent-config",
            config_path
                .to_str()
                .ok_or("agent config path should be UTF-8")?,
            "--json",
            "repo-review",
            "helper",
        ])
        .output()?;
    assert!(configured.status.success(), "{configured:?}");
    let profiles: serde_json::Value = serde_json::from_slice(&configured.stdout)?;
    let profiles = profiles
        .as_array()
        .ok_or("configured agent profiles JSON should be an array")?;
    assert_eq!(profiles.len(), 2, "{profiles:?}");
    assert_eq!(profiles[0]["profile"], "repo-review");
    assert_eq!(profiles[0]["kind"], "configured");
    assert_eq!(profiles[0]["binary"], "codex");
    assert_eq!(profiles[0]["session_base"], "repo-review-session");
    assert_eq!(profiles[0]["status_default"], false);
    assert_eq!(profiles[0]["available"], true);
    assert_eq!(profiles[1]["profile"], "helper");
    assert_eq!(profiles[1]["kind"], "configured");
    assert_eq!(profiles[1]["binary"], "helper");
    assert_eq!(profiles[1]["session_base"], "helper-lterm");
    assert_eq!(profiles[1]["status_default"], true);
    assert_eq!(profiles[1]["available"], true);

    let unknown = env
        .cmd()
        .env("PATH", &path)
        .args([
            "agents",
            "--agent-config",
            config_path
                .to_str()
                .ok_or("agent config path should be UTF-8")?,
            "--json",
            "typo-agent",
        ])
        .output()?;
    assert!(!unknown.status.success(), "{unknown:?}");
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        stderr.contains("was not found in --agent-config"),
        "{stderr:?}"
    );
    Ok(())
}

#[test]
fn generic_agent_profile_forwards_args_and_tmux_environment() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(
        &fake_bin.join("codex"),
        r#"#!/bin/sh
printf 'LTERM_AGENT:%s\n' "$LTERM_AGENT"
printf 'LTERM_SESSION:%s\n' "$LTERM_SESSION"
printf 'LTERM_PANE:%s\n' "$LTERM_PANE"
printf 'TMUX_PANE:%s\n' "$TMUX_PANE"
printf 'TMUX_BIN:%s\n' "$(command -v tmux)"
i=1
for arg in "$@"; do
  printf 'ARG%d:%s\n' "$i" "$arg"
  i=$((i + 1))
done
printf 'PANE_LIST:'
tmux list-panes -t "$TMUX_PANE" -F '#{pane_id}'
"#,
    )?;
    write_executable(
        &fake_bin.join("tmux"),
        "#!/bin/sh\necho FAKE_TMUX_SHOULD_NOT_RUN\nexit 99\n",
    )?;
    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{old_path}", fake_bin.display());

    let output = env
        .cmd()
        .env("PATH", &path)
        .stdin(Stdio::null())
        .args([
            "agent",
            "codex",
            "--",
            "--model",
            "gpt 5",
            "semi;colon",
            "--flag",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("LTERM_AGENT:codex"), "{stdout:?}");
    assert!(stdout.contains("LTERM_SESSION:codex-lterm"), "{stdout:?}");
    assert!(stdout.contains("LTERM_PANE:%0"), "{stdout:?}");
    assert!(stdout.contains("TMUX_PANE:%0"), "{stdout:?}");
    assert!(stdout.contains("ARG1:--model"), "{stdout:?}");
    assert!(stdout.contains("ARG2:gpt 5"), "{stdout:?}");
    assert!(stdout.contains("ARG3:semi;colon"), "{stdout:?}");
    assert!(stdout.contains("ARG4:--flag"), "{stdout:?}");
    assert!(stdout.contains("PANE_LIST:%0"), "{stdout:?}");
    assert!(
        !stdout.contains("FAKE_TMUX_SHOULD_NOT_RUN"),
        "fake tmux won PATH precedence: {stdout:?}"
    );
    Ok(())
}

#[test]
fn agent_launcher_persists_agent_name_metadata() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    let config_path = env.temp.path().join("agents.json");
    std::fs::create_dir(&fake_bin)?;
    write_executable(
        &fake_bin.join("codex"),
        "#!/bin/sh\nprintf 'READY\\n'\nsleep 5\n",
    )?;
    std::fs::write(
        &config_path,
        r#"{
  "profiles": [
    {
      "name": "repo-review",
      "binary": "codex",
      "session_base": "repo-review-session",
      "status_default": false
    }
  ]
}"#,
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .args([
            "agent",
            "repo-review",
            "--agent-config",
            config_path
                .to_str()
                .ok_or("agent config path should be UTF-8")?,
            "--detach",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let _cleanup = SessionCleanup::new(&env, "repo-review-session");

    let sessions = env.cmd().args(["sessions", "--json"]).output()?;
    assert!(sessions.status.success(), "{sessions:?}");
    let sessions: serde_json::Value = serde_json::from_slice(&sessions.stdout)?;
    let session = sessions
        .as_array()
        .ok_or("sessions should be an array")?
        .iter()
        .find(|session| session["name"] == "repo-review-session")
        .ok_or("missing repo-review-session")?;
    assert_eq!(session["agent_name"], "repo-review");
    Ok(())
}

#[test]
fn mobile_auto_attach_uses_normal_screen_transcript_for_agent_sessions() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "codex-lterm",
            "--",
            "sh",
            "-lc",
            "printf 'READY\\n'; sleep 5",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "codex-lterm");

    env.capture_until("codex-lterm", "READY")?;
    let output = env
        .cmd()
        .env("LTERM_MOBILE", "1")
        .stdin(Stdio::null())
        .args(["attach", "codex-lterm"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("lterm mobile transcript"),
        "auto mobile attach should route to transcript mode: {stdout:?}"
    );
    assert!(stdout.contains("READY"), "{stdout:?}");
    assert!(
        !stdout.contains("\x1b[?1049h"),
        "mobile transcript must stay on the normal screen: {stdout:?}"
    );

    let sessions = env.cmd().args(["sessions", "--json"]).output()?;
    assert!(sessions.status.success(), "{sessions:?}");
    let sessions: serde_json::Value = serde_json::from_slice(&sessions.stdout)?;
    let session = sessions
        .as_array()
        .ok_or("sessions should be an array")?
        .iter()
        .find(|session| session["name"] == "codex-lterm")
        .ok_or("missing codex-lterm")?;
    assert_eq!(
        session["attached_clients"], 0,
        "transcript mode must not register as a raw attach client"
    );
    Ok(())
}

#[test]
fn agent_mobile_status_request_stays_on_transcript_surface() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(
        &fake_bin.join("codex"),
        "#!/bin/sh\nprintf 'READY\\n'\nsleep 5\n",
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", path)
        .stdin(Stdio::null())
        .args(["codex", "--mobile", "--status"])
        .output()?;
    let _cleanup = SessionCleanup::new(&env, "codex-lterm");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("lterm mobile transcript"), "{stdout:?}");
    assert!(stdout.contains("READY"), "{stdout:?}");
    assert!(
        !stdout.contains("lterm  codex-lterm"),
        "--mobile --status must not create a raw status row: {stdout:?}"
    );
    Ok(())
}

#[test]
fn mobile_auto_transcript_prints_sanitized_capture() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "codex-lterm",
            "--",
            "sh",
            "-lc",
            "printf 'VISIBLE \\033[31mRED\\033[0m \\033]52;c;secret\\007DONE\\n'; sleep 5",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "codex-lterm");

    env.capture_until("codex-lterm", "VISIBLE RED DONE")?;
    let output = env
        .cmd()
        .env("LTERM_MOBILE", "1")
        .stdin(Stdio::null())
        .args(["attach", "codex-lterm"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("VISIBLE RED DONE"), "{stdout:?}");
    assert_eq!(
        stdout.matches("\x1b[0m").count(),
        2,
        "mobile transcript should emit exactly one local reset for the banner and one for the transcript update; reset-only payload escapes must stay sanitized: {stdout:?}"
    );
    let stdout_without_local_resets = stdout.replacen("\x1b[0m", "", 2);
    assert!(
        !stdout_without_local_resets.contains('\x1b') && !stdout.contains("secret"),
        "mobile transcript must not print active escape/control payloads: {stdout:?}"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn mobile_auto_transcript_does_not_perturb_raw_attach_geometry() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "codex-lterm",
            "--",
            "sh",
            "-lc",
            "printf 'READY\\n'; sleep 30",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "codex-lterm");

    wait_for_socket(&socket)?;
    env.capture_until("codex-lterm", "READY")?;
    let (_desktop_stream, _desktop_id) = attach_with_geometry(&socket, "codex-lterm", 40, 152)?;
    wait_for_size(&env, "codex-lterm", (40, 152))?;

    let before = read_session_json(&env, "codex-lterm")?;
    assert_eq!(
        before
            .get("attached_clients")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "pre-mobile raw desktop attach should be the only attached client: {before}"
    );

    let output = env
        .cmd()
        .env("LTERM_MOBILE", "1")
        .stdin(Stdio::null())
        .args(["attach", "codex-lterm"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("lterm mobile transcript"), "{stdout:?}");
    assert!(stdout.contains("READY"), "{stdout:?}");

    let after = read_session_json(&env, "codex-lterm")?;
    assert_eq!(
        after
            .get("attached_clients")
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "mobile transcript must not become a second attach client: {after}"
    );
    assert_eq!(
        (
            after.get("rows").and_then(serde_json::Value::as_u64),
            after.get("cols").and_then(serde_json::Value::as_u64)
        ),
        (Some(40), Some(152)),
        "mobile transcript must not shrink or otherwise resize the raw desktop PTY geometry: {after}"
    );
    Ok(())
}

#[test]
fn invalid_attach_mode_env_does_not_create_open_session() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .env("LTERM_ATTACH_MODE", "bogus")
        .stdin(Stdio::null())
        .args(["open", "bad-env-open"])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    assert_stderr_contains(&output, "invalid LTERM_ATTACH_MODE");
    assert!(
        !session_names_json(&env)?.contains("bad-env-open"),
        "open must validate attach policy before creating a session"
    );
    Ok(())
}

#[test]
fn env_raw_read_only_does_not_create_open_session() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .env("LTERM_ATTACH_MODE", "raw")
        .stdin(Stdio::null())
        .args(["open", "raw-read-only-open", "--read-only"])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    assert_stderr_contains(&output, "--read-only requires mobile transcript mode");
    assert!(
        !session_names_json(&env)?.contains("raw-read-only-open"),
        "open must reject env-selected raw read-only before creating a session"
    );
    Ok(())
}

#[test]
fn auto_read_only_does_not_create_open_session() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .stdin(Stdio::null())
        .args(["open", "auto-read-only-open", "--read-only"])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    assert_stderr_contains(
        &output,
        "--read-only in auto attach mode requires an existing target",
    );
    assert!(
        !session_names_json(&env)?.contains("auto-read-only-open"),
        "open must not create a session when auto read-only has no target to classify"
    );
    Ok(())
}

#[test]
fn auto_read_only_rejects_plain_raw_attach_without_sending_input() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "plain-read-only",
            "--",
            "sh",
            "-lc",
            "echo READY; read line; echo GOT:$line; sleep 5",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "plain-read-only");
    env.capture_until("plain-read-only", "READY")?;

    let mut attach = env
        .cmd()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["attach", "plain-read-only", "--read-only"])
        .spawn()?;
    let write_result = attach
        .stdin
        .as_mut()
        .ok_or("missing attach stdin")?
        .write_all(b"SHOULD_NOT_SEND\n");
    if let Err(err) = write_result {
        if err.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(err.into());
        }
    }
    let output = attach.wait_with_output()?;
    assert!(!output.status.success(), "{output:?}");
    assert_stderr_contains(&output, "--read-only requires mobile transcript mode");

    let capture = env
        .cmd()
        .args(["logs", "plain-read-only", "-S=-20"])
        .output()?;
    assert!(capture.status.success(), "{capture:?}");
    let captured = String::from_utf8_lossy(&capture.stdout);
    assert!(
        !captured.contains("GOT:SHOULD_NOT_SEND"),
        "read-only auto fallback must not open a writable raw attach: {captured:?}"
    );
    Ok(())
}

#[test]
fn invalid_attach_mode_env_does_not_create_agent_session() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(&fake_bin.join("codex"), "#!/bin/sh\nsleep 5\n")?;
    let path = path_with_prepended(&fake_bin)?;
    let output = env
        .cmd()
        .env("PATH", path)
        .env("LTERM_ATTACH_MODE", "bogus")
        .stdin(Stdio::null())
        .args(["codex"])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    assert_stderr_contains(&output, "invalid LTERM_ATTACH_MODE");
    assert!(
        !session_names_json(&env)?.contains("codex-lterm"),
        "agent launcher must validate attach policy before creating a session"
    );
    Ok(())
}

#[test]
fn agent_auto_read_only_without_mobile_does_not_create_session() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(&fake_bin.join("codex"), "#!/bin/sh\nsleep 5\n")?;
    let path = path_with_prepended(&fake_bin)?;
    let output = env
        .cmd()
        .env("PATH", path)
        .stdin(Stdio::null())
        .args(["codex", "--read-only"])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    assert_stderr_contains(&output, "--read-only requires mobile transcript mode");
    assert!(
        !session_names_json(&env)?.contains("codex-lterm"),
        "agent launcher must not create a desktop raw session when --read-only is auto-only"
    );
    Ok(())
}

#[test]
fn env_raw_read_only_does_not_create_agent_session() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(&fake_bin.join("codex"), "#!/bin/sh\nsleep 5\n")?;
    let path = path_with_prepended(&fake_bin)?;
    let output = env
        .cmd()
        .env("PATH", path)
        .env("LTERM_ATTACH_MODE", "raw")
        .stdin(Stdio::null())
        .args(["codex", "--read-only"])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    assert_stderr_contains(&output, "--read-only requires mobile transcript mode");
    assert!(
        !session_names_json(&env)?.contains("codex-lterm"),
        "agent launcher must reject env-selected raw read-only before creating a session"
    );
    Ok(())
}

#[test]
fn tmux_sessions_preserve_current_terminal_identity_for_color_detection() -> TestResult {
    let env = TestEnv::new()?;
    let anchor = env
        .cmd()
        .env_remove("TERM_PROGRAM")
        .env_remove("LC_TERMINAL")
        .env_remove("COLORTERM")
        .args([
            "new",
            "--detach",
            "--name",
            "term-env-anchor",
            "--",
            "sh",
            "-lc",
            "sleep 5",
        ])
        .status()?;
    assert!(anchor.success());
    let _anchor_cleanup = SessionCleanup::new(&env, "term-env-anchor");

    let status = env
        .cmd()
        .env("TERM_PROGRAM", "Termius")
        .env("LC_TERMINAL", "Termius")
        .env("COLORTERM", "truecolor")
        .args([
            "new",
            "--tmux",
            "--detach",
            "--name",
            "term-env-child",
            "--",
            "sh",
            "-lc",
            "printf 'TERM_PROGRAM:%s\\n' \"${TERM_PROGRAM-}\"; printf 'LC_TERMINAL:%s\\n' \"${LC_TERMINAL-}\"; printf 'COLORTERM:%s\\n' \"${COLORTERM-}\"; printf 'TMUX_PANE:%s\\n' \"${TMUX_PANE-}\"; sleep 1",
        ])
        .status()?;
    assert!(status.success());
    let _child_cleanup = SessionCleanup::new(&env, "term-env-child");

    let captured = env.capture_until("term-env-child", "TMUX_PANE:")?;
    assert!(captured.contains("TERM_PROGRAM:Termius"), "{captured:?}");
    assert!(captured.contains("LC_TERMINAL:Termius"), "{captured:?}");
    assert!(captured.contains("COLORTERM:truecolor"), "{captured:?}");
    assert!(captured.contains("TMUX_PANE:%1"), "{captured:?}");
    Ok(())
}

#[test]
fn agent_alias_preserves_terminal_identity_for_color_detection() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(
        &fake_bin.join("omx"),
        r#"#!/bin/sh
printf 'LTERM_AGENT:%s\n' "$LTERM_AGENT"
printf 'TERM_PROGRAM:%s\n' "${TERM_PROGRAM-}"
printf 'LC_TERMINAL:%s\n' "${LC_TERMINAL-}"
printf 'COLORTERM:%s\n' "${COLORTERM-}"
sleep 1
"#,
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", path)
        .env("TERM_PROGRAM", "Termius")
        .env("LC_TERMINAL", "Termius")
        .env("COLORTERM", "truecolor")
        .stdin(Stdio::null())
        .args(["omx", "--raw", "--no-status", "--", "probe"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("LTERM_AGENT:omx"), "{stdout:?}");
    assert!(stdout.contains("TERM_PROGRAM:Termius"), "{stdout:?}");
    assert!(stdout.contains("LC_TERMINAL:Termius"), "{stdout:?}");
    assert!(stdout.contains("COLORTERM:truecolor"), "{stdout:?}");
    Ok(())
}

#[test]
fn agent_alias_scrubs_ambient_color_policy_env_for_child_tui() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(
        &fake_bin.join("omx"),
        r#"#!/bin/sh
printf 'NO_COLOR:%s\n' "${NO_COLOR-unset}"
printf 'FORCE_COLOR:%s\n' "${FORCE_COLOR-unset}"
printf 'CLICOLOR:%s\n' "${CLICOLOR-unset}"
printf 'CLICOLOR_FORCE:%s\n' "${CLICOLOR_FORCE-unset}"
if [ -n "${NO_COLOR-}" ]; then
  printf 'PLAIN_ONLY\n'
else
  printf '\033[31mCOLOR_OK\033[0m\n'
fi
sleep 1
"#,
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = env
        .cmd()
        .env("PATH", path)
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "3")
        .env("CLICOLOR", "0")
        .env("CLICOLOR_FORCE", "1")
        .stdin(Stdio::null())
        .args(["omx", "--raw", "--no-status", "--", "probe"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("NO_COLOR:unset"), "{stdout:?}");
    assert!(stdout.contains("FORCE_COLOR:unset"), "{stdout:?}");
    assert!(stdout.contains("CLICOLOR:unset"), "{stdout:?}");
    assert!(stdout.contains("CLICOLOR_FORCE:unset"), "{stdout:?}");
    assert!(
        output
            .stdout
            .windows(b"\x1b[31mCOLOR_OK\x1b[0m".len())
            .any(|window| window == b"\x1b[31mCOLOR_OK\x1b[0m"),
        "agent TUI should still be able to emit color SGR when parent lterm has NO_COLOR: {:?}",
        stdout
    );
    assert!(!stdout.contains("PLAIN_ONLY"), "{stdout:?}");
    Ok(())
}

#[test]
fn plain_sessions_preserve_ambient_color_policy_env() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "3")
        .env("CLICOLOR", "0")
        .env("CLICOLOR_FORCE", "1")
        .args([
            "new",
            "--detach",
            "--name",
            "plain-color-policy",
            "--",
            "sh",
            "-lc",
            "printf 'NO_COLOR:%s\\n' \"${NO_COLOR-unset}\"; \
             printf 'FORCE_COLOR:%s\\n' \"${FORCE_COLOR-unset}\"; \
             printf 'CLICOLOR:%s\\n' \"${CLICOLOR-unset}\"; \
             printf 'CLICOLOR_FORCE:%s\\n' \"${CLICOLOR_FORCE-unset}\"; \
             sleep 30",
        ])
        .status()?;
    assert!(status.success());
    let _cleanup = SessionCleanup::new(&env, "plain-color-policy");

    let captured = env.capture_until("plain-color-policy", "CLICOLOR_FORCE:1")?;
    assert!(captured.contains("NO_COLOR:1"), "{captured:?}");
    assert!(captured.contains("FORCE_COLOR:3"), "{captured:?}");
    assert!(captured.contains("CLICOLOR:0"), "{captured:?}");
    assert!(captured.contains("CLICOLOR_FORCE:1"), "{captured:?}");
    Ok(())
}

#[test]
fn configured_agent_profile_launches_configured_binary() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    let config_path = env.temp.path().join("agents.json");
    std::fs::create_dir(&fake_bin)?;
    write_executable(
        &fake_bin.join("codex"),
        r#"#!/bin/sh
printf 'LTERM_AGENT:%s\n' "$LTERM_AGENT"
printf 'LTERM_SESSION:%s\n' "$LTERM_SESSION"
printf 'ARG1:%s\n' "$1"
sleep 1
"#,
    )?;
    std::fs::write(
        &config_path,
        r#"{
  "profiles": [
    {
      "name": "repo-review",
      "binary": "codex",
      "session_base": "repo-review-session",
      "status_default": false
    }
  ]
}"#,
    )?;
    let path = std::env::join_paths([fake_bin.as_path()])?;

    let output = env
        .cmd()
        .env("PATH", &path)
        .stdin(Stdio::null())
        .args([
            "agent",
            "repo-review",
            "--agent-config",
            config_path
                .to_str()
                .ok_or("agent config path should be UTF-8")?,
            "--",
            "inspect",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("LTERM_AGENT:repo-review"), "{stdout:?}");
    assert!(
        stdout.contains("LTERM_SESSION:repo-review-session"),
        "{stdout:?}"
    );
    assert!(stdout.contains("ARG1:inspect"), "{stdout:?}");

    let typo = env
        .cmd()
        .env("PATH", &path)
        .stdin(Stdio::null())
        .args([
            "agent",
            "repo-revue",
            "--agent-config",
            config_path
                .to_str()
                .ok_or("agent config path should be UTF-8")?,
        ])
        .output()?;
    assert!(!typo.status.success(), "{typo:?}");
    let stderr = String::from_utf8_lossy(&typo.stderr);
    assert!(
        stderr.contains("was not found in --agent-config"),
        "{stderr:?}"
    );

    let bad_config = env.temp.path().join("bad-agents.json");
    std::fs::write(&bad_config, "{ not json")?;
    let built_in_with_bad_config = env
        .cmd()
        .env("PATH", &path)
        .stdin(Stdio::null())
        .args([
            "agent",
            "codex",
            "--agent-config",
            bad_config
                .to_str()
                .ok_or("bad agent config path should be UTF-8")?,
        ])
        .output()?;
    assert!(
        !built_in_with_bad_config.status.success(),
        "{built_in_with_bad_config:?}"
    );
    let stderr = String::from_utf8_lossy(&built_in_with_bad_config.stderr);
    assert!(stderr.contains("parse agent config"), "{stderr:?}");

    let escaped_config_path = env.temp.path().join("bad-\u{1b}[31m-agents.json");
    std::fs::write(&escaped_config_path, "{ not json")?;
    let escaped_error = env
        .cmd()
        .env("PATH", &path)
        .stdin(Stdio::null())
        .args([
            "agents",
            "--agent-config",
            escaped_config_path
                .to_str()
                .ok_or("escaped agent config path should be UTF-8")?,
            "--json",
        ])
        .output()?;
    assert!(!escaped_error.status.success(), "{escaped_error:?}");
    let stderr = String::from_utf8_lossy(&escaped_error.stderr);
    assert!(stderr.contains("parse agent config"), "{stderr:?}");
    assert!(stderr.contains("\\u{1b}"), "{stderr:?}");
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");

    let rejected_config = |file_name: &str, contents: &str, expected: &str| -> TestResult {
        let rejected_config_path = env.temp.path().join(file_name);
        std::fs::write(&rejected_config_path, contents)?;
        let output = env
            .cmd()
            .env("PATH", &path)
            .stdin(Stdio::null())
            .args([
                "agents",
                "--agent-config",
                rejected_config_path
                    .to_str()
                    .ok_or("rejected agent config path should be UTF-8")?,
                "--json",
            ])
            .output()?;
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "expected {expected:?} in {stderr:?}"
        );
        Ok(())
    };
    rejected_config(
        "unknown-field-agents.json",
        r#"{ "profiles": [{ "name": "helper", "unknown": true }] }"#,
        "unknown field",
    )?;
    rejected_config(
        "built-in-agents.json",
        r#"{ "profiles": [{ "name": "codex" }] }"#,
        "cannot redefine built-in",
    )?;
    rejected_config(
        "bad-binary-agents.json",
        r#"{ "profiles": [{ "name": "helper", "binary": "../codex" }] }"#,
        "invalid binary",
    )?;
    rejected_config(
        "bad-session-agents.json",
        r#"{ "profiles": [{ "name": "helper", "session_base": "-bad" }] }"#,
        "invalid session_base",
    )?;
    rejected_config(
        "null-status-agents.json",
        r#"{ "profiles": [{ "name": "helper", "status_default": null }] }"#,
        "invalid type",
    )?;
    Ok(())
}

#[test]
fn named_agent_alias_uses_profile_environment() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    let fake_agent = r#"#!/bin/sh
printf 'LTERM_AGENT:%s\n' "$LTERM_AGENT"
printf 'LTERM_SESSION:%s\n' "$LTERM_SESSION"
printf 'ARG1:%s\n' "$1"
"#;
    for binary in [
        "claude",
        "codex",
        "opencode",
        "copilot",
        "cursor-agent",
        "agy",
        "jules",
        "kiro-cli",
        "aider",
        "goose",
        "amp",
        "crush",
        "gemini",
        "kimi",
        "qwen",
        "omx",
        "omc",
    ] {
        write_executable(&fake_bin.join(binary), fake_agent)?;
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{old_path}", fake_bin.display());

    for (command, expected_agent, expected_session) in [
        ("claude", "claude", "claude-lterm"),
        ("codex", "codex", "codex-lterm"),
        ("opencode", "opencode", "opencode-lterm"),
        ("copilot", "copilot", "copilot-lterm"),
        ("cursor-agent", "cursor-agent", "cursor-agent-lterm"),
        ("agy", "agy", "agy-lterm"),
        ("jules", "jules", "jules-lterm"),
        ("kiro", "kiro", "kiro-lterm"),
        ("aider", "aider", "aider-lterm"),
        ("goose", "goose", "goose-lterm"),
        ("amp", "amp", "amp-lterm"),
        ("crush", "crush", "crush-lterm"),
        ("gemini", "gemini", "gemini-lterm"),
        ("kimi", "kimi", "kimi-lterm"),
        ("qwen", "qwen", "qwen-lterm"),
        ("omx", "omx", "omx-lterm"),
        ("omc", "omc", "omc-lterm"),
    ] {
        let output = env
            .cmd()
            .env("PATH", &path)
            .stdin(Stdio::null())
            .args([command, "--", "-p"])
            .output()?;
        assert!(output.status.success(), "{command} failed: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!("LTERM_AGENT:{expected_agent}")),
            "{stdout:?}"
        );
        assert!(
            stdout.contains(&format!("LTERM_SESSION:{expected_session}")),
            "{stdout:?}"
        );
        assert!(stdout.contains("ARG1:-p"), "{stdout:?}");
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn agent_alias_status_default_controls_attached_tty_rendering() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    for binary in ["omx", "omc"] {
        write_executable(
            &fake_bin.join(binary),
            "#!/bin/sh\nprintf 'AGENT_READY\\n'\nsleep 1\n",
        )?;
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(fake_bin.as_path().to_path_buf()).chain(std::env::split_paths(&old_path)),
    )?;

    for alias in ["omx", "omc"] {
        let status_indicator = format!("lterm  {alias}-lterm").into_bytes();
        let default_output = run_agent_alias_on_pty_until_exit(
            &env,
            &path,
            &[alias],
            &format!("{alias} default status"),
        )?;
        assert!(
            contains_subsequence(&default_output, b"AGENT_READY"),
            "{alias} fake agent marker should be forwarded: {:?}",
            String::from_utf8_lossy(&default_output)
        );
        assert!(
            !contains_subsequence(&default_output, &status_indicator),
            "{alias} should default to a raw full-terminal attach so the agent TUI owns color/input rendering: {:?}",
            String::from_utf8_lossy(&default_output)
        );

        let status_output = run_agent_alias_on_pty_until_exit(
            &env,
            &path,
            &[alias, "--status"],
            &format!("{alias} explicit status"),
        )?;
        assert!(
            contains_subsequence(&status_output, b"AGENT_READY"),
            "{alias} fake agent marker should be forwarded when --status is enabled: {:?}",
            String::from_utf8_lossy(&status_output)
        );
        assert!(
            contains_subsequence(&status_output, &status_indicator),
            "--status should still opt {alias} into the lterm status bar: {:?}",
            String::from_utf8_lossy(&status_output)
        );
    }

    Ok(())
}

#[test]
#[cfg(unix)]
fn agent_alias_force_status_repaints_after_alt_screen_startup_clear() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(
        &fake_bin.join("omc"),
        "#!/bin/sh\n\
         printf 'ARGS:%s\\n' \"$*\"\n\
         printf '\\033[?1049h\\033[2J\\033[HAGENT_READY\\n'\n\
         sleep 1\n",
    )?;
    let path = path_with_prepended(&fake_bin)?;

    let output = run_agent_alias_on_pty_until_exit(
        &env,
        &path,
        &["omc", "--status", "--madmax"],
        "omc force status after alt-screen startup",
    )?;
    assert!(
        contains_subsequence(&output, b"ARGS:--madmax"),
        "--madmax should be forwarded to omc: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        contains_subsequence(&output, b"AGENT_READY"),
        "fake omc marker should be forwarded: {:?}",
        String::from_utf8_lossy(&output)
    );
    let alt_enter = find_subsequence(&output, b"\x1b[?1049h").ok_or_else(|| {
        format!(
            "fake omc did not emit alt-screen enter: {:?}",
            String::from_utf8_lossy(&output)
        )
    })?;
    let after_alt_enter = &output[alt_enter..];
    let status_indicator = b"lterm  omc-lterm";
    assert!(
        contains_subsequence(after_alt_enter, status_indicator),
        "--status/ForceRow should repaint the lterm status row after an agent alt-screen startup clear: {:?}",
        String::from_utf8_lossy(after_alt_enter)
    );

    Ok(())
}

#[test]
#[cfg(unix)]
fn agent_launch_controls_set_name_cwd_and_detach() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    let workdir = env.temp.path().join("agent-workdir");
    let suffix = env
        .temp
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("agent")
        .trim_start_matches('.');
    let session_name = format!("repo-agent-{}-{suffix}", std::process::id());
    std::fs::create_dir(&fake_bin)?;
    std::fs::create_dir(&workdir)?;
    write_executable(
        &fake_bin.join("codex"),
        r#"#!/bin/sh
printf 'LTERM_AGENT:%s\n' "$LTERM_AGENT"
printf 'LTERM_SESSION:%s\n' "$LTERM_SESSION"
printf 'PWD:%s\n' "$(pwd -P)"
printf 'ARG1:%s\n' "$1"
sleep 300
"#,
    )?;
    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(fake_bin.as_path().to_path_buf()).chain(std::env::split_paths(&old_path)),
    )?;
    let expected_pwd = std::fs::canonicalize(&workdir)?;
    let mut cleanup = SessionCleanup::new(&env, session_name.clone());

    let started = Instant::now();
    let output = env
        .cmd()
        .env("PATH", &path)
        .current_dir(env.temp.path())
        .args([
            "agent",
            "codex",
            "--name",
            &session_name,
            "--cwd",
            workdir
                .to_str()
                .ok_or("temporary workdir path should be UTF-8")?,
            "--detach",
            "--",
            "exec",
        ])
        .output()?;
    let elapsed = started.elapsed();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        elapsed < Duration::from_secs(20),
        "--detach should return before the agent exits; elapsed={elapsed:?}, output={output:?}"
    );
    let fields: Vec<_> = stdout.trim_end().split('\t').collect();
    assert_eq!(fields.len(), 3, "{stdout:?}");
    assert_eq!(fields[0], session_name, "{stdout:?}");
    assert!(
        fields[1].len() > 1
            && fields[1].starts_with('%')
            && fields[1][1..].chars().all(|ch| ch.is_ascii_digit()),
        "{stdout:?}"
    );
    assert!(fields[2].contains("codex"), "{stdout:?}");
    wait_for_session_present(&env, &session_name)?;

    let captured = env.capture_until(&session_name, "ARG1:exec")?;
    assert!(captured.contains("LTERM_AGENT:codex"), "{captured:?}");
    assert!(
        captured.contains(&format!("LTERM_SESSION:{session_name}")),
        "{captured:?}"
    );
    assert!(
        captured.contains(&format!("PWD:{}", expected_pwd.display())),
        "{captured:?}"
    );
    assert!(captured.contains("ARG1:exec"), "{captured:?}");
    cleanup.kill_now()?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn agent_launch_explicit_name_conflict_does_not_autosuffix() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    write_executable(&fake_bin.join("codex"), "#!/bin/sh\nsleep 60\n")?;
    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(fake_bin.as_path().to_path_buf()).chain(std::env::split_paths(&old_path)),
    )?;
    let suffix = env
        .temp
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("agent")
        .trim_start_matches('.');
    let session_name = format!("repo-agent-conflict-{}-{suffix}", std::process::id());
    let mut cleanup = SessionCleanup::new(&env, session_name.clone());

    let first = env
        .cmd()
        .env("PATH", &path)
        .args(["agent", "codex", "--name", &session_name, "--detach"])
        .output()?;
    assert!(first.status.success(), "{first:?}");
    wait_for_session_present(&env, &session_name)?;

    let second = env
        .cmd()
        .env("PATH", &path)
        .args(["agent", "codex", "--name", &session_name, "--detach"])
        .output()?;
    let stderr = String::from_utf8_lossy(&second.stderr);

    let listed = env.cmd().arg("ls").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(list_row(&stdout, &session_name).is_some(), "{stdout:?}");
    let unexpected_prefixed: Vec<String> = stdout
        .lines()
        .filter_map(|line| line.split('\t').next())
        .filter(|name| *name != session_name.as_str() && name.starts_with(&session_name))
        .map(ToOwned::to_owned)
        .collect();
    for leaked in &unexpected_prefixed {
        let mut leaked_cleanup = SessionCleanup::new(&env, leaked.clone());
        let _ = leaked_cleanup.kill_now();
    }
    assert!(!second.status.success(), "{second:?}");
    assert!(
        stderr.contains(&format!(
            "failed to create agent session named {session_name}"
        )) && stderr.contains("session name already exists"),
        "{stderr:?}"
    );
    assert!(unexpected_prefixed.is_empty(), "{stdout:?}");
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.starts_with(&format!("{session_name}\t")))
            .count(),
        1,
        "{stdout:?}"
    );
    cleanup.kill_now()?;
    Ok(())
}

#[test]
fn missing_agent_profile_reports_binary_lookup_error() -> TestResult {
    let env = TestEnv::new()?;
    let fake_bin = env.temp.path().join("fake-bin");
    std::fs::create_dir(&fake_bin)?;
    let output = env
        .cmd()
        .env("PATH", &fake_bin)
        .args(["agent", "definitely-missing-agent"])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("definitely-missing-agent not found in PATH"),
        "{stderr:?}"
    );
    Ok(())
}

#[test]
fn env_outputs_only_shell_exports() -> TestResult {
    let env = TestEnv::new()?;
    let output = env.cmd().arg("env").output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().all(|line| line.starts_with("export ")),
        "env output should be eval-safe exports only: {stdout:?}"
    );
    assert!(stdout.contains("export PATH="), "{stdout:?}");
    assert!(!stdout.contains("\n/"), "bare shim path leaked: {stdout:?}");
    Ok(())
}

fn fish_quote_for_test(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn tmux_socket_field(tmux: &str) -> &str {
    tmux.split(',').next().unwrap_or("")
}

fn fish_sourceability_command() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("fish");
        if !candidate.is_file() {
            continue;
        }
        let candidate = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        let supports_no_config = Command::new(&candidate)
            .arg("--no-config")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if supports_no_config {
            return Some(candidate);
        }
    }
    None
}

#[test]
fn env_outputs_fish_exports_when_requested() -> TestResult {
    let env = TestEnv::new()?;
    let runtime = env
        .temp
        .path()
        .join("fish runtime with ' quote and \\ slash");
    let data = env.temp.path().join("fish data with ' quote and \\ slash");
    std::fs::create_dir_all(&runtime)?;
    std::fs::create_dir_all(&data)?;
    #[cfg(unix)]
    {
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))?;
    }
    let output = env
        .cmd()
        .env("LTERM_RUNTIME_DIR", &runtime)
        .env("LTERM_DATA_DIR", &data)
        .args(["env", "--shell", "fish"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_socket = runtime.join("lterm.sock").display().to_string();
    let expected_tmux_socket = runtime
        .join(".lterm.sock.tmux-compat")
        .display()
        .to_string();
    let expected_shim = data.join("shims").display().to_string();
    let expected_lines = vec![
        format!(
            "set -gx LTERM_SOCKET {}",
            fish_quote_for_test(&expected_socket)
        ),
        "string length -q -- \"$TMUX_PANE\"; or set -gx TMUX_PANE '%0'".to_string(),
        format!(
            "contains -- {} $PATH; or set -gx PATH {} $PATH",
            fish_quote_for_test(&expected_shim),
            fish_quote_for_test(&expected_shim)
        ),
    ];
    let lines: Vec<_> = stdout.lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 4, "fish env output should stay four lines");
    let quoted_tmux_socket = fish_quote_for_test(&expected_tmux_socket);
    let tmux_prefix = format!(
        "set -gx TMUX {},",
        quoted_tmux_socket
            .strip_suffix('\'')
            .expect("test quote should end with a single quote")
    );
    assert!(
        lines[1].starts_with(&tmux_prefix) && lines[1].ends_with(",0'"),
        "fish TMUX line should be quoted compat-socket,pid,0: {:?}",
        lines[1]
    );
    assert_eq!(
        vec![lines[0].clone(), lines[2].clone(), lines[3].clone()],
        expected_lines,
        "fish env output should keep exact sourceable line shape"
    );
    if let Some(fish) = fish_sourceability_command() {
        let script = format!(
            "{stdout}\nprintf '%s\\n' \"$LTERM_SOCKET\"\nprintf '%s\\n' \"$TMUX\"\nprintf '%s\\n' \"$TMUX_PANE\"\nstring join : $PATH\n"
        );
        let fish_output = Command::new(fish)
            .arg("--no-config")
            .arg("-c")
            .arg(script)
            .env("PATH", "BASE_PATH")
            .env_remove("TMUX_PANE")
            .output()?;
        assert!(fish_output.status.success(), "{fish_output:?}");
        let fish_stdout = String::from_utf8(fish_output.stdout)?;
        let sourced_lines: Vec<_> = fish_stdout.lines().collect();
        assert_eq!(sourced_lines.first(), Some(&expected_socket.as_str()));
        assert!(
            sourced_lines
                .get(1)
                .is_some_and(|tmux| tmux.starts_with(&format!("{expected_tmux_socket},"))
                    && tmux.ends_with(",0")),
            "{fish_stdout:?}"
        );
        assert_ne!(
            tmux_socket_field(sourced_lines[1]),
            expected_socket,
            "TMUX socket field must not be the live daemon socket"
        );
        assert_eq!(sourced_lines.get(2), Some(&"%0"));
        let expected_path = format!("{expected_shim}:BASE_PATH");
        assert_eq!(sourced_lines.get(3), Some(&expected_path.as_str()));
    }
    assert!(
        !stdout.contains("export "),
        "fish env output should not emit POSIX exports:\n{stdout}"
    );
    Ok(())
}

#[test]
fn env_rejects_unsupported_shell_values() -> TestResult {
    let env = TestEnv::new()?;
    let output = env.cmd().args(["env", "--shell", "powershell"]).output()?;
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid value") && stderr.contains("powershell"),
        "unsupported env shell should be rejected by clap value parser:\n{stderr}"
    );
    Ok(())
}

#[test]
fn env_posix_family_shells_emit_posix_exports() -> TestResult {
    let env = TestEnv::new()?;
    for shell in ["bash", "zsh", "posix"] {
        let output = env.cmd().args(["env", "--shell", shell]).output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        for expected in [
            "export LTERM_SOCKET=",
            "export TMUX=",
            "export TMUX_PANE=",
            "export PATH=",
        ] {
            assert!(
                stdout.contains(expected),
                "env --shell {shell} missing POSIX export {expected:?}:\n{stdout}"
            );
        }
        assert!(
            !stdout.contains("set -gx") && !stdout.contains("contains --"),
            "env --shell {shell} should not emit fish syntax:\n{stdout}"
        );
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn env_quotes_generated_paths_for_posix_shell_eval() -> TestResult {
    let env = TestEnv::new()?;
    let runtime = env.temp.path().join("runtime dir with ' quote and $dollar");
    let data = env.temp.path().join("data dir with ' quote and $dollar");
    std::fs::create_dir_all(&runtime)?;
    std::fs::create_dir_all(&data)?;
    #[cfg(unix)]
    {
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))?;
    }

    let output = env
        .cmd()
        .env("LTERM_RUNTIME_DIR", &runtime)
        .env("LTERM_DATA_DIR", &data)
        .arg("env")
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let exports = String::from_utf8(output.stdout)?;

    let script = format!(
        "{exports}\nprintf '%s\\n' \"$LTERM_SOCKET\"\nprintf '%s\\n' \"$TMUX\"\nprintf '%s\\n' \"$PATH\"\n"
    );
    let eval_output = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("PATH", "BASE_PATH")
        .output()?;
    assert!(eval_output.status.success(), "{eval_output:?}");
    let eval_stdout = String::from_utf8(eval_output.stdout)?;
    let lines: Vec<_> = eval_stdout.lines().collect();
    let expected_socket = runtime.join("lterm.sock").display().to_string();
    let expected_tmux_socket = runtime
        .join(".lterm.sock.tmux-compat")
        .display()
        .to_string();
    assert_eq!(
        lines.first(),
        Some(&expected_socket.as_str()),
        "{eval_stdout:?}"
    );
    assert!(
        lines
            .get(1)
            .is_some_and(|tmux| tmux.starts_with(&format!("{expected_tmux_socket},"))
                && tmux.ends_with(",0")),
        "{eval_stdout:?}"
    );
    assert_ne!(
        tmux_socket_field(lines[1]),
        expected_socket,
        "TMUX socket field must not be the live daemon socket"
    );
    let expected_path = format!("{}:BASE_PATH", data.join("shims").display());
    assert_eq!(
        lines.get(2),
        Some(&expected_path.as_str()),
        "{eval_stdout:?}"
    );
    Ok(())
}

#[test]
fn urls_extracts_recent_sanitized_scrollback_links() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "urls",
            "--",
            "sh",
            "-lc",
            "printf 'A https://a.example/path?x=1#frag.\\nB http://b.example/a(b)!\\nANSI \\033[31mhttps://red.example/ok\\033[0m\\nOSC \\033]52;c;secret\\007https://after-osc.example/done,\\nA2 https://a.example/path?x=1#frag\\nREADY_URLS\\n'; while :; do sleep 60; done",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("urls", "READY_URLS")?;

    let output = env.cmd().args(["urls", "urls"]).output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout,
        concat!(
            "1\thttps://a.example/path?x=1#frag\n",
            "2\thttp://b.example/a(b)\n",
            "3\thttps://red.example/ok\n",
            "4\thttps://after-osc.example/done\n",
        )
    );
    assert!(!stdout.contains("secret"), "{stdout:?}");

    let last = env.cmd().args(["urls", "urls", "--last"]).output()?;
    assert!(last.status.success(), "{last:?}");
    assert_eq!(
        String::from_utf8_lossy(&last.stdout),
        "https://a.example/path?x=1#frag\n"
    );

    let json = env.cmd().args(["urls", "urls", "--json"]).output()?;
    assert!(json.status.success(), "{json:?}");
    let urls: Vec<String> = serde_json::from_slice(&json.stdout)?;
    assert_eq!(
        urls,
        vec![
            "https://a.example/path?x=1#frag",
            "http://b.example/a(b)",
            "https://red.example/ok",
            "https://after-osc.example/done",
        ]
    );
    Ok(())
}

#[test]
fn urls_empty_results_are_machine_friendly() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "urls-empty",
            "--",
            "sh",
            "-lc",
            "printf 'NO_URLS_READY\\n'; while :; do sleep 60; done",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("urls-empty", "NO_URLS_READY")?;

    let text = env.cmd().args(["urls", "urls-empty"]).output()?;
    assert!(text.status.success(), "{text:?}");
    assert!(text.stdout.is_empty(), "{text:?}");

    let last = env.cmd().args(["urls", "urls-empty", "--last"]).output()?;
    assert!(last.status.success(), "{last:?}");
    assert!(last.stdout.is_empty(), "{last:?}");

    let json = env.cmd().args(["urls", "urls-empty", "--json"]).output()?;
    assert!(json.status.success(), "{json:?}");
    assert_eq!(String::from_utf8_lossy(&json.stdout), "[]\n");
    Ok(())
}

#[test]
fn search_extracts_recent_sanitized_scrollback_matches() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "search",
            "--",
            "sh",
            "-lc",
            "printf 'alpha needle\\nbeta\\nOSC \\033]52;c;secret\\007gamma needle\\nNEEDLE uppercase\\nREADY_SEARCH\\n'; while :; do sleep 60; done",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("search", "READY_SEARCH")?;

    let output = env.cmd().args(["search", "search", "needle"]).output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, "1\talpha needle\n2\tOSC gamma needle\n");
    assert!(!stdout.contains('\x1b'), "{stdout:?}");
    assert!(!stdout.contains("secret"), "{stdout:?}");

    let json = env
        .cmd()
        .args(["search", "search", "needle", "--json"])
        .output()?;
    assert!(json.status.success(), "{json:?}");
    let json_stdout = String::from_utf8_lossy(&json.stdout);
    assert!(!json_stdout.contains('\x1b'), "{json_stdout:?}");
    assert!(!json_stdout.contains("secret"), "{json_stdout:?}");
    let matches: Vec<String> = serde_json::from_slice(&json.stdout)?;
    assert_eq!(matches, vec!["alpha needle", "OSC gamma needle"]);

    let tail = env
        .cmd()
        .args(["search", "search", "needle", "--tail", "1"])
        .output()?;
    assert!(tail.status.success(), "{tail:?}");
    assert!(tail.stdout.is_empty(), "{tail:?}");
    Ok(())
}

#[test]
fn search_empty_results_are_machine_friendly() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "search-empty",
            "--",
            "sh",
            "-lc",
            "printf 'NO_SEARCH_READY\\n'; while :; do sleep 60; done",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("search-empty", "NO_SEARCH_READY")?;

    let text = env
        .cmd()
        .args(["search", "search-empty", "needle"])
        .output()?;
    assert!(text.status.success(), "{text:?}");
    assert!(text.stdout.is_empty(), "{text:?}");

    let json = env
        .cmd()
        .args(["search", "search-empty", "needle", "--json"])
        .output()?;
    assert!(json.status.success(), "{json:?}");
    let matches: Vec<String> = serde_json::from_slice(&json.stdout)?;
    assert_eq!(matches, Vec::<String>::new());
    Ok(())
}

#[test]
fn search_empty_query_is_rejected_before_reporting() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "search-empty-query",
            "--",
            "sh",
            "-lc",
            "printf 'READY_EMPTY_QUERY\\n'; while :; do sleep 60; done",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("search-empty-query", "READY_EMPTY_QUERY")?;

    let output = env
        .cmd()
        .args(["search", "search-empty-query", ""])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("search query cannot be empty"), "{stderr}");
    assert!(output.stdout.is_empty(), "{output:?}");
    Ok(())
}

#[test]
fn tmux_capture_without_print_is_silent_and_saves_buffer() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "cap",
            "echo CAPTURE_ME; sleep 2",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("cap", "CAPTURE_ME")?;

    let output = env
        .cmd()
        .args(["tmux-compat", "capture-pane", "-t", "cap"])
        .output()?;
    assert!(output.status.success());
    assert!(output.stdout.is_empty());

    let output = env
        .cmd()
        .args(["tmux-compat", "save-buffer", "-"])
        .output()?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("CAPTURE_ME"));
    Ok(())
}

#[test]
fn tmux_capture_pane_skips_value_options_before_target() -> TestResult {
    let env = TestEnv::new()?;
    for (name, marker) in [
        ("capture-first", "FIRST_MARK"),
        ("capture-second", "SECOND_MARK"),
    ] {
        let status = env
            .cmd()
            .args([
                "tmux-compat",
                "new-session",
                "-d",
                "-s",
                name,
                &format!("echo {marker}; sleep 2"),
            ])
            .status()?;
        assert!(status.success());
        env.capture_until(name, marker)?;
    }
    let status = env
        .cmd()
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "capture-start",
            "printf 'FIRST_LINE\\nSECOND_LINE\\nTHIRD_LINE\\n'; sleep 2",
        ])
        .status()?;
    assert!(status.success());
    env.capture_until("capture-start", "THIRD_LINE")?;

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-p",
            "-Stop",
            "-E",
            "10",
            "-t",
            "capture-second",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("SECOND_MARK"), "{stdout:?}");
    assert!(
        !stdout.contains("FIRST_MARK"),
        "capture-pane -S value should not hide the later -t target: {stdout:?}"
    );

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-p",
            "-S1",
            "-t",
            "capture-start",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("SECOND_LINE"), "{stdout:?}");
    assert!(
        !stdout.contains("FIRST_LINE"),
        "compact capture-pane -S value should set the start line: {stdout:?}"
    );

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-p",
            "-S0",
            "-E0",
            "-t",
            "capture-start",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FIRST_LINE"), "{stdout:?}");
    assert!(
        !stdout.contains("SECOND_LINE"),
        "compact capture-pane -E value should set the inclusive end line: {stdout:?}"
    );
    assert!(
        !stdout.contains("THIRD_LINE"),
        "compact capture-pane -E value should stop at the inclusive end line: {stdout:?}"
    );

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-p",
            "-S0",
            "-E-1",
            "-t",
            "capture-start",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FIRST_LINE"), "{stdout:?}");
    assert!(stdout.contains("SECOND_LINE"), "{stdout:?}");
    assert!(stdout.contains("THIRD_LINE"), "{stdout:?}");

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-p",
            "-E",
            "-t",
            "capture-start",
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "missing -E value must fail instead of consuming -t as a line value: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid capture-pane -E line value"),
        "{stderr:?}"
    );

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-p",
            "-Ewat",
            "-t",
            "capture-start",
        ])
        .output()?;
    assert!(
        !output.status.success(),
        "invalid compact -E value must fail: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid capture-pane -E line value"),
        "{stderr:?}"
    );

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-b",
            "named-buffer",
            "-S",
            "0",
            "-E",
            "10",
            "-t",
            "capture-second",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let buffer = env
        .cmd()
        .args(["tmux-compat", "save-buffer", "-"])
        .output()?;
    assert!(buffer.status.success(), "{buffer:?}");
    let stdout = String::from_utf8_lossy(&buffer.stdout);
    assert!(stdout.contains("SECOND_MARK"), "{stdout:?}");
    assert!(!stdout.contains("FIRST_MARK"), "{stdout:?}");

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "capture-pane",
            "-b",
            "-p",
            "-t",
            "capture-second",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "capture-pane -b -p must treat -p as the buffer name, not print flag: {output:?}"
    );
    let buffer = env
        .cmd()
        .args(["tmux-compat", "save-buffer", "-"])
        .output()?;
    assert!(buffer.status.success(), "{buffer:?}");
    let stdout = String::from_utf8_lossy(&buffer.stdout);
    assert!(stdout.contains("SECOND_MARK"), "{stdout:?}");
    assert!(!stdout.contains("FIRST_MARK"), "{stdout:?}");
    Ok(())
}

#[test]
fn tmux_buffer_commands_skip_buffer_name_options_before_path() -> TestResult {
    let env = TestEnv::new()?;
    let input = env.temp.path().join("buffer-input.txt");
    let output_path = env.temp.path().join("buffer-output.txt");
    std::fs::write(&input, b"BUFFER_OPTION_PAYLOAD")?;

    let load = env
        .cmd()
        .args([
            "tmux-compat",
            "load-buffer",
            "-b",
            "named-buffer",
            input.to_str().ok_or("input path should be UTF-8")?,
        ])
        .output()?;
    assert!(load.status.success(), "{load:?}");

    let save = env
        .cmd()
        .args([
            "tmux-compat",
            "save-buffer",
            "-b",
            "named-buffer",
            output_path.to_str().ok_or("output path should be UTF-8")?,
        ])
        .output()?;
    assert!(save.status.success(), "{save:?}");
    assert_eq!(std::fs::read(&output_path)?, b"BUFFER_OPTION_PAYLOAD");

    let stdout = env
        .cmd()
        .args(["tmux-compat", "save-buffer", "--", "-"])
        .output()?;
    assert!(stdout.status.success(), "{stdout:?}");
    assert_eq!(stdout.stdout, b"BUFFER_OPTION_PAYLOAD");
    Ok(())
}

#[test]
fn tmux_paste_buffer_skips_buffer_name_option_before_target() -> TestResult {
    let env = TestEnv::new()?;
    for (name, label) in [("paste-first", "FIRST"), ("paste-second", "SECOND")] {
        let status = env
            .cmd()
            .args([
                "tmux-compat",
                "new-session",
                "-d",
                "-s",
                name,
                &format!("echo READY_{label}; read line; echo {label}:$line; sleep 2"),
            ])
            .status()?;
        assert!(status.success());
        env.capture_until(name, &format!("READY_{label}"))?;
    }

    let input = env.temp.path().join("paste-buffer-input.txt");
    std::fs::write(&input, b"PASTE_PAYLOAD\n")?;
    // lterm exposes one compatibility buffer today. The -b value is parsed
    // only so it cannot hide the later target flag.
    let load = env
        .cmd()
        .args([
            "tmux-compat",
            "load-buffer",
            input.to_str().ok_or("input path should be UTF-8")?,
        ])
        .output()?;
    assert!(load.status.success(), "{load:?}");

    let paste = env
        .cmd()
        .args([
            "tmux-compat",
            "paste-buffer",
            "-btfoo",
            "-s",
            "",
            "-t",
            "paste-second",
        ])
        .output()?;
    assert!(paste.status.success(), "{paste:?}");
    let second = env.capture_until("paste-second", "SECOND:PASTE_PAYLOAD")?;
    assert!(second.contains("SECOND:PASTE_PAYLOAD"), "{second}");
    let first = env.capture_until("paste-first", "READY_FIRST")?;
    assert!(
        !first.contains("FIRST:PASTE_PAYLOAD"),
        "paste-buffer -b value should not hide the later -t target: {first:?}"
    );
    Ok(())
}

#[test]
fn attach_stdout_broken_pipe_detaches_cleanly() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "pipe-detach",
            "--",
            "sh",
            "-lc",
            "yes PIPE | head -c 200000; sleep 1",
        ])
        .status()?;
    assert!(status.success());

    let mut attach = env
        .cmd()
        .args(["attach", "pipe-detach", "--no-status"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let _stdin = attach.stdin.take().ok_or("missing attach stdin")?;
    let mut stdout = attach.stdout.take().ok_or("missing attach stdout")?;
    let mut byte = [0_u8; 1];
    stdout.read_exact(&mut byte)?;
    drop(stdout);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(status) = attach.try_wait()? {
            assert!(status.success(), "{status:?}");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = attach.kill();
    let _ = attach.wait();
    Err("attach did not exit cleanly after stdout pipe closed".into())
}

#[test]
#[cfg(unix)]
fn runtime_and_data_dirs_are_private() -> TestResult {
    let env = TestEnv::new()?;
    let status = env.cmd().arg("list").status()?;
    assert!(status.success());

    let runtime_mode = std::fs::metadata(env.temp.path().join("run"))?
        .permissions()
        .mode()
        & 0o777;
    let data_mode = std::fs::metadata(env.temp.path().join("data"))?
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(runtime_mode, 0o700);
    assert_eq!(data_mode, 0o700);
    Ok(())
}

#[test]
fn rejects_control_characters_in_session_names() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .args(["new", "--name", "bad\u{1b}name", "--", "true"])
        .output()?;
    assert!(!output.status.success());
    assert_stderr_contains(&output, ERR_SESSION_NAME);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    Ok(())
}

#[test]
fn rejects_bare_numeric_session_names() -> TestResult {
    let env = TestEnv::new()?;
    for numeric_name in ["0", "123", "007"] {
        let numeric = env
            .cmd()
            .args(["new", "--name", numeric_name, "--", "true"])
            .output()?;
        assert!(!numeric.status.success());
        assert_stderr_contains(&numeric, ERR_BARE_PANE_ID);
    }
    Ok(())
}

#[test]
fn rejects_empty_session_names() -> TestResult {
    let env = TestEnv::new()?;
    let empty_new = env
        .cmd()
        .args(["new", "--name", "", "--", "true"])
        .output()?;
    assert!(!empty_new.status.success(), "{empty_new:?}");
    assert_stderr_contains(&empty_new, ERR_EMPTY_SESSION_NAME);

    let created = env
        .cmd()
        .args(["new", "--detach", "-n", "empty-rename", "--", "sleep", "60"])
        .output()?;
    assert!(created.status.success(), "{created:?}");

    let empty_rename = env.cmd().args(["rename", "empty-rename", ""]).output()?;
    assert!(!empty_rename.status.success(), "{empty_rename:?}");
    assert_stderr_contains(&empty_rename, ERR_EMPTY_SESSION_NAME);

    env.cmd().args(["close", "empty-rename"]).status()?;
    wait_for_session_absent(&env, "empty-rename")?;
    Ok(())
}

#[test]
fn rejects_flag_like_session_names() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .args(["new", "--name=-bad", "--", "true"])
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    assert_stderr_contains(&output, ERR_LEADING_DASH_NAME);
    Ok(())
}

#[test]
fn capture_strips_terminal_escape_sequences() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "escaped",
            "--",
            "sh",
            "-lc",
            "printf '\\033]52;c;secret\\aSAFE\\n'; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("escaped", "SAFE")?;
    assert!(captured.contains("SAFE"), "{captured}");
    assert!(!captured.contains("secret"), "{captured}");
    assert!(!captured.contains('\u{1b}'), "{captured:?}");
    Ok(())
}

// 주어진 socket path에 lterm 데몬이 protocol 수준에서 살아있는지 확인한다.
// lterm CLI의 `doctor --json`을 LTERM_SOCKET override 하에 spawn해서 그 결과의
// `daemon_reachable` 필드를 검사한다. doctor는 (HANDOFF: "auto-spawn next lterm
// command other than doctor/shutdown") auto-spawn하지 않으므로 false positive를
// 만들지 않는다.
//
// helper는 path 인자를 받는 분리 형태이므로 임시 bait UnixListener에 대해서도
// 검증 테스트가 가능하다.
#[cfg(unix)]
fn runtime_daemon_reports_reachable_at(socket: &Path) -> bool {
    // cheap pre-check: connect 실패면 즉시 false. 실제 daemon이면 connect는
    // 거의 항상 즉시 성공하므로 빠른 부정 경로 제공.
    if UnixStream::connect(socket).is_err() {
        return false;
    }
    let out = match Command::new(env!("CARGO_BIN_EXE_lterm"))
        .env("LTERM_SOCKET", socket)
        .args(["doctor", "--json"])
        .output()
    {
        Ok(out) => out,
        Err(_) => return false,
    };
    if !out.status.success() {
        return false;
    }
    let report: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return false,
    };
    report
        .get("daemon_reachable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// LTERM_RUNTIME_DIR / LTERM_SOCKET / XDG_RUNTIME_DIR 를 모두 제거한 채 lterm
// CLI 의 fallback runtime path 동작을 검증하는 테스트 전용 Command builder.
//
// sandbox 인자로 `&tempfile::TempDir` 만 받는다. 이는 PR #83 quad-review 합의
// (Claude HIGH + Codex LOW): 인자 타입을 RAII tempdir로 좁혀 호출자가 임의
// `/tmp` 같은 user-default 경로를 실수로 넘기지 못하도록 sandbox 격리를 시그니처
// 차원에서 강제한다.
//
// 반환: `(Command, PathBuf, PathBuf)` = (LTERM_RUNTIME_DIR/LTERM_SOCKET/
// XDG_RUNTIME_DIR 제거 + TMPDIR/LTERM_DATA_DIR 주입된 Command, TMPDIR 로 쓰인
// 경로, LTERM_DATA_DIR 로 쓰인 경로). 호출자는 권한/심볼릭 검증을 위해 두
// PathBuf 를 그대로 사용할 수 있다. LTERM_SOCKET 도 별도 unset 하는 이유는
// LTERM_SOCKET 이 LTERM_RUNTIME_DIR 과 독립적으로 소켓 경로를 override 하기
// 때문이다 — RUNTIME_DIR 만 unset 하면 LTERM_SOCKET 이 그대로 살아 fallback
// 검증이 무효화될 수 있다.
//
// 본 helper 를 거치지 않고 fallback runtime selector(LTERM_RUNTIME_DIR,
// LTERM_SOCKET, XDG_RUNTIME_DIR)를 직접 unset 하는 테스트는 호스트에 떠 있는
// 사용자 데몬을 침범할 위험이 있다. 1차 방어선은 sandbox TMPDIR 환경 격리이다
// (PR #76 quad-review 합의 — TMPDIR isolation is the real protection). 같은 fallback 검증 패턴이 필요한 새 테스트는
// 반드시 본 helper 를 사용한다. fallback 검증이 아닌 테스트가 LTERM_SOCKET 만
// 제거해야 하는 경우에도 test-private LTERM_RUNTIME_DIR 과 TMPDIR 를 함께 주입해
// 부모 호스트의 default runtime path 로 빠지지 않게 한다.
#[cfg(unix)]
fn cmd_for_default_fallback_test(
    sandbox: &tempfile::TempDir,
) -> std::io::Result<(Command, std::path::PathBuf, std::path::PathBuf)> {
    let tmp = sandbox.path().join("tmp");
    let data = sandbox.path().join("data");
    // 멱등 — 두 번 호출되어도 OK (이미 존재하면 create_dir_all는 Ok).
    std::fs::create_dir_all(&tmp)?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lterm"));
    cmd.env_remove("LTERM_RUNTIME_DIR")
        .env_remove("LTERM_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", &tmp)
        .env("LTERM_DATA_DIR", &data);
    Ok((cmd, tmp, data))
}

#[cfg(unix)]
fn cmd_for_homeless_default_fallback_test(
    sandbox: &tempfile::TempDir,
) -> std::io::Result<(Command, std::path::PathBuf)> {
    let tmp = sandbox.path().join("tmp");
    std::fs::create_dir_all(&tmp)?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lterm"));
    cmd.env_remove("HOME")
        .env_remove("LTERM_DATA_DIR")
        .env_remove("LTERM_PANE")
        .env_remove("LTERM_PARENT_TOKEN")
        .env_remove("LTERM_RUNTIME_DIR")
        .env_remove("LTERM_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", &tmp);
    Ok((cmd, tmp))
}

#[test]
#[cfg(unix)]
fn default_tmp_runtime_dir_is_private_and_not_a_symlink() -> TestResult {
    let temp = tempfile::tempdir()?;
    let (mut list, tmp, _data) = cmd_for_default_fallback_test(&temp)?;
    list.arg("list");
    let output = list.output()?;
    assert!(output.status.success(), "{output:?}");

    let uid = std::fs::metadata(&tmp)?.uid();
    let runtime = tmp.join(format!("light-terminal-{uid}"));
    let meta = std::fs::symlink_metadata(&runtime)?;
    assert!(!meta.file_type().is_symlink());
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);

    let (mut shutdown, _, _) = cmd_for_default_fallback_test(&temp)?;
    let _ = shutdown.arg("shutdown").status();
    Ok(())
}

#[test]
#[cfg(unix)]
fn homeless_default_runtime_autostarts_and_tmux_store_uses_private_tmp_data() -> TestResult {
    let temp = tempfile::tempdir()?;

    let (mut list, tmp) = cmd_for_homeless_default_fallback_test(&temp)?;
    let output = list.arg("list").output()?;
    assert!(
        output.status.success(),
        "HOME-less list should auto-start via TMPDIR fallback: {output:?}"
    );

    let uid = std::fs::metadata(&tmp)?.uid();
    let runtime = tmp.join(format!("light-terminal-{uid}"));
    let data = runtime.join("data");
    let meta = std::fs::symlink_metadata(&data)?;
    assert!(!meta.file_type().is_symlink());
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);

    let (mut tmux, _) = cmd_for_homeless_default_fallback_test(&temp)?;
    let output = tmux
        .args([
            "tmux-compat",
            "new-session",
            "-d",
            "-s",
            "homeless-tmux-store",
            "-P",
            "-F",
            "#S:#I",
            "sh -lc 'echo HOMELESS_TMUX_READY; sleep 1'",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "HOME-less tmux compat should persist store under runtime data: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "homeless-tmux-store:0"
    );
    assert!(
        data.join("tmux-compat-store.json").exists(),
        "tmux compat store should be created under HOME-less runtime data dir"
    );

    let (mut shutdown, _) = cmd_for_homeless_default_fallback_test(&temp)?;
    let _ = shutdown.arg("shutdown").status();
    Ok(())
}

// runtime_daemon_reports_reachable_at은 단순 UnixStream::connect 가능성을 넘어
// "lterm 데몬이 protocol 수준에서 살아있는가" 까지 검증한다. 본 회귀 가드는
// 임의 UnixListener를 bait socket으로 두고 helper를 직접 호출해 false를 반환함을
// 확인한다.
//
// listener를 단순 bind만 한 채 두면 helper가 spawn하는 doctor가 RPC read-timeout
// (5초)까지 대기해 테스트가 느려진다 (quad-review 합의 — Codex Issue 1/3,
// Claude Issue 3). 짧은 accept thread를 두어 stream을 즉시 drop하면 doctor RPC
// 가 fast-fail로 끝나 daemon_reachable=false를 빠르게 반환한다.
#[test]
#[cfg(unix)]
fn runtime_daemon_reports_reachable_at_rejects_non_lterm_listener() -> TestResult {
    let temp = tempfile::tempdir()?;
    let bait = temp.path().join("alive.sock");
    let listener = UnixListener::bind(&bait)?;
    listener.set_nonblocking(true)?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_thread = std::sync::Arc::clone(&stop);
    let bait_clone = bait.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let accept_thread = std::thread::spawn(move || {
        // doctor가 시도하는 connect 를 accept 후 즉시 drop — protocol 응답을
        // 보내지 않으므로 doctor 의 RPC 가 fast-fail (EOF / unexpected close) 한다.
        // 백그라운드 thread 는 helper 호출 종료 후 stop flag 로 정리한다. accept를
        // nonblocking으로 두어 stop 후 join 이 wake-up connect 에 의존하지 않는다.
        while !stop_thread.load(std::sync::atomic::Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => drop(stream),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        // 시그널을 받았을 때 cleanup helper — listener 와 path 가 tempdir Drop
        // 으로 정리되지만 명시적 cleanup 도 안전망으로 둔다.
        let _ = std::fs::remove_file(&bait_clone);
        let _ = done_tx.send(());
    });
    let alive = runtime_daemon_reports_reachable_at(&bait);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .map_err(|err| {
            format!("bait listener accept thread did not stop within 1s after stop signal: {err}")
        })?;
    accept_thread
        .join()
        .map_err(|_| "bait listener accept thread panicked")?;
    assert!(
        !alive,
        "runtime_daemon_reports_reachable_at must return false for a plain UnixListener at {}; \
         got true, meaning the strict guard fell back to the cheap reachability check",
        bait.display()
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn custom_socket_requires_private_parent() -> TestResult {
    let temp = tempfile::tempdir()?;
    let parent = temp.path().join("shared");
    std::fs::create_dir(&parent)?;
    let mut perms = std::fs::metadata(&parent)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&parent, perms)?;
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lterm"));
    let output = daemon
        .env("LTERM_SOCKET", parent.join("lterm.sock"))
        .env("LTERM_DATA_DIR", temp.path().join("data"))
        .arg("daemon")
        .output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("owned by uid")
            || stderr.contains("must not be a symlink")
            || stderr.contains("not a directory")
            || stderr.contains("must be private"),
        "{stderr}"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn custom_socket_parent_permissions_are_not_silently_changed() -> TestResult {
    let temp = tempfile::tempdir()?;
    let parent = temp.path().join("shared");
    let data = temp.path().join("data");
    std::fs::create_dir(&parent)?;
    let mut perms = std::fs::metadata(&parent)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&parent, perms)?;

    let output = Command::new(env!("CARGO_BIN_EXE_lterm"))
        .env("LTERM_SOCKET", parent.join("lterm.sock"))
        .env("LTERM_DATA_DIR", &data)
        .arg("list")
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    let mode = std::fs::metadata(&parent)?.permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o755,
        "lterm should reject, not chmod, socket parents"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn custom_socket_refuses_existing_regular_file() -> TestResult {
    let temp = tempfile::tempdir()?;
    let parent = temp.path().join("run");
    let data = temp.path().join("data");
    std::fs::create_dir(&parent)?;
    let mut perms = std::fs::metadata(&parent)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(&parent, perms)?;
    let socket = parent.join("lterm.sock");
    std::fs::write(&socket, b"do not delete")?;

    let output = Command::new(env!("CARGO_BIN_EXE_lterm"))
        .env("LTERM_SOCKET", &socket)
        .env("LTERM_DATA_DIR", &data)
        .arg("daemon")
        .output()?;
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(std::fs::read(&socket)?, b"do not delete");
    Ok(())
}

#[test]
fn session_reaps_when_leader_exits_even_if_background_keeps_pty_open() -> TestResult {
    let env = TestEnv::new()?;
    let release = env.temp.path().join("leader-reap-release");
    let release_arg = release.to_string_lossy().to_string();
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "leader-reap",
            "--",
            "sh",
            "-lc",
            "trap '' HUP TERM; release=$1; sleep 30 & printf 'CHILD:%s\\nCHILD_READY\\n' \"$!\"; while [ ! -f \"$release\" ]; do sleep 0.05; done; echo LEADER_DONE",
            "sh",
            &release_arg,
        ])
        .status()?;
    assert!(status.success());
    let captured = env.capture_until("leader-reap", "CHILD_READY")?;
    let child_pid = captured
        .split("CHILD:")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .ok_or("missing background child pid")?;
    assert!(
        pid_alive(child_pid)?,
        "background child should be alive before leader exits"
    );

    std::fs::write(&release, b"go")?;
    wait_for_session_absent_for(&env, "leader-reap", Duration::from_secs(3))?;
    wait_for_pid_exit(child_pid)
}

#[test]
#[cfg(unix)]
fn kill_reaps_session_process_group_children() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "pgrp",
            "--",
            "sh",
            "-lc",
            "sleep 30 & echo CHILD:$!; wait",
        ])
        .status()?;
    assert!(status.success());

    let captured = env.capture_until("pgrp", "CHILD:")?;
    let child_pid = captured
        .split("CHILD:")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .ok_or("missing child pid")?;
    assert!(
        pid_alive(child_pid)?,
        "child process should be alive before lterm kill"
    );

    for command in ["ps", "processes"] {
        let ps_output = env
            .cmd()
            .args([command, "pgrp", "--orphans", "--json"])
            .output()?;
        assert!(
            ps_output.status.success(),
            "{command} failed: {ps_output:?}"
        );
        assert!(
            String::from_utf8_lossy(&ps_output.stdout).contains(child_pid),
            "lterm {command} should include child process tree"
        );
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&ps_output.stdout)?;
        let child = rows
            .iter()
            .find(|row| {
                row.get("pid")
                    .and_then(serde_json::Value::as_u64)
                    .map(|pid| pid.to_string() == child_pid)
                    .unwrap_or(false)
            })
            .ok_or("missing child process row")?;
        assert!(
            child.get("process_group_id").is_some(),
            "process rows should expose process group id: {child:?}"
        );
        assert_eq!(
            child.get("orphan").and_then(serde_json::Value::as_bool),
            Some(false),
            "normal child tree rows should not be marked orphan: {child:?}"
        );
    }

    let status = env.cmd().args(["kill", "pgrp"]).status()?;
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !pid_alive(child_pid)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!("child process {child_pid} survived lterm kill").into())
}

// ──────────────────────────────────────────────────────────────────────────
// PR #15: server-side per-client geometry + clamp-to-smallest 통합 테스트.
//
// `lterm attach` 는 raw TTY 가 필요해 일반 subprocess 로는 띄울 수 없으므로,
// 라이브러리 의존 없이 daemon 의 Unix socket 에 직접 JSON 프로토콜로 attach 한다.
// 두 attach 를 다른 geometry 로 등록한 뒤 `lterm sessions --json` 으로 PTY rows/cols
// 를 읽어 clamp 가 정확히 적용되는지, 좁은 쪽이 detach 하면 PTY 가 다시 자라는지를
// 검증한다. 이 테스트는 client-side 가드를 우회해 server 정책 만 직접 보는 것이
// 목적이라 내부 protocol 모듈을 import 하지 않는다 — wire-level JSON 으로 충분하다.
// ──────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn socket_path_for(env: &TestEnv) -> std::path::PathBuf {
    env.temp.path().join("run").join("lterm.sock")
}

/// daemon 이 socket 을 listen 시작할 때까지 기다린다. `lterm new` 가 daemon 을
/// fork 하므로 호출 직후 곧바로 connect 하면 ECONNREFUSED 가 날 수 있다.
#[cfg(unix)]
fn wait_for_socket(path: &Path) -> TestResult {
    poll_until(
        Duration::from_secs(3),
        Duration::from_millis(25),
        &format!("daemon socket {}", path.display()),
        || match UnixStream::connect(path) {
            Ok(_) => Ok(PollStatus::Ready(())),
            Err(err) => Ok(PollStatus::Pending(format!("connect error: {err}"))),
        },
    )
}

/// 한 줄짜리 JSON 응답을 읽어 `serde_json::Value` 로 반환한다. attach 응답은
/// `{"ok":true,"result":{"subscriber_id":N}}\n` 모양.
#[cfg(unix)]
fn read_response_line(stream: &mut UnixStream) -> TestResult<serde_json::Value> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    let deadline = Instant::now() + Duration::from_secs(3);
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    while Instant::now() < deadline {
        match stream.read(&mut byte) {
            Ok(0) => return Err("daemon closed before sending response line".into()),
            Ok(_) => {
                if byte[0] == b'\n' {
                    let value: serde_json::Value = serde_json::from_slice(&line)?;
                    return Ok(value);
                }
                line.push(byte[0]);
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Err("timed out waiting for response line".into())
}

/// 주어진 geometry 로 attach 한 뒤 응답에서 subscriber_id 를 꺼낸다. 호출자는
/// 반환된 stream 을 살려두어야 attach 가 유지되며, drop 하면 server 가 EOF 를 보고
/// unsubscribe 해 자연스럽게 detach 된다.
#[cfg(unix)]
fn attach_with_geometry(
    socket: &Path,
    target: &str,
    rows: u16,
    cols: u16,
) -> TestResult<(UnixStream, u64)> {
    let mut stream = UnixStream::connect(socket)?;
    let request = serde_json::json!({
        "type": "attach",
        "target": target,
        "rows": rows,
        "cols": cols,
    });
    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    let response = read_response_line(&mut stream)?;
    if response.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(format!("attach failed: {response}").into());
    }
    let subscriber_id = response
        .get("result")
        .and_then(|v| v.get("subscriber_id"))
        .and_then(|v| v.as_u64())
        .ok_or("attach response missing subscriber_id")?;
    Ok((stream, subscriber_id))
}

#[cfg(unix)]
fn run_agent_alias_on_pty_until_exit(
    env: &TestEnv,
    path: &OsString,
    args: &[&str],
    label: &str,
) -> TestResult<Vec<u8>> {
    let (mut master, slave) = open_pty_pair()?;
    set_pty_window_size(&slave, 24, 80)?;
    let stdin = Stdio::from(slave.try_clone()?);
    let stdout = Stdio::from(slave.try_clone()?);
    let stderr = Stdio::from(slave.try_clone()?);
    let mut child = ChildCleanup::new(
        env.cmd()
            .env("PATH", path)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .args(args)
            .spawn()?,
    );
    drop(slave);

    read_pty_until_child_exit(&mut master, &mut child, label, Duration::from_secs(5))
}

#[cfg(unix)]
fn set_pty_window_size(file: &File, rows: u16, cols: u16) -> TestResult {
    let winsize = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &winsize) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn read_pty_until_child_exit<R: Read + AsRawFd>(
    stdout: &mut R,
    child: &mut ChildCleanup,
    label: &str,
    timeout: Duration,
) -> TestResult<Vec<u8>> {
    let fd = stdout.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(format!(
                "failed to read PTY stdout flags for {label}: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        if (flags & libc::O_NONBLOCK) == 0 {
            let set = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            if set < 0 {
                return Err(format!(
                    "failed to set PTY stdout nonblocking for {label}: {}",
                    std::io::Error::last_os_error()
                )
                .into());
            }
        }
    }

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut child_exited = false;
    let mut drain_until: Option<Instant> = None;

    while Instant::now() < deadline {
        match stdout.read(&mut chunk) {
            Ok(0) => {
                if child_exited {
                    return Ok(buf);
                }
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if child_exited {
                    drain_until = Some(Instant::now() + Duration::from_millis(50));
                }
                continue;
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                if child_exited {
                    let drain_deadline = *drain_until
                        .get_or_insert_with(|| Instant::now() + Duration::from_millis(50));
                    if Instant::now() >= drain_deadline {
                        return Ok(buf);
                    }
                }
            }
            Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                // Linux PTY masters report EIO once the slave side is closed.
                // Treat it like EOF after the child is reaped; otherwise give
                // the child-status poll below a chance to observe the exit.
                if child_exited {
                    return Ok(buf);
                }
            }
            Err(err) => {
                return Err(format!("{label} pty read error: {err}").into());
            }
        }

        if !child_exited {
            if let Some(status) = child.child_mut()?.try_wait()? {
                assert!(status.success(), "{label} failed: {status:?}");
                child.child = None;
                child_exited = true;
                drain_until = Some(Instant::now() + Duration::from_millis(50));
            }
        }
        thread::sleep(Duration::from_millis(10));
    }

    Err(format!(
        "timed out waiting for {label}; buffer head: {:?}",
        String::from_utf8_lossy(&buf[..buf.len().min(256)])
    )
    .into())
}

#[cfg(unix)]
fn open_pty_pair() -> TestResult<(File, File)> {
    let mut master = -1;
    let mut slave = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    Ok((master, slave))
}

/// `lterm sessions --json` 으로 단일 세션 row 를 조회한다.
#[cfg(unix)]
fn read_session_json(env: &TestEnv, name: &str) -> TestResult<serde_json::Value> {
    let output = env.cmd().args(["sessions", "--json"]).output()?;
    if !output.status.success() {
        return Err(format!("lterm sessions --json failed: {output:?}").into());
    }
    let sessions: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    sessions
        .into_iter()
        .find(|s| s.get("name").and_then(|v| v.as_str()) == Some(name))
        .ok_or_else(|| format!("session {name} not in list").into())
}

/// `lterm sessions --json` 으로 단일 세션의 (rows, cols) 를 조회한다.
#[cfg(unix)]
fn read_session_size(env: &TestEnv, name: &str) -> TestResult<(u16, u16)> {
    let session = read_session_json(env, name)?;
    let rows = session
        .get("rows")
        .and_then(|v| v.as_u64())
        .ok_or("missing rows")
        .and_then(|value| u16::try_from(value).map_err(|_| "rows out of u16 range"))?;
    let cols = session
        .get("cols")
        .and_then(|v| v.as_u64())
        .ok_or("missing cols")
        .and_then(|value| u16::try_from(value).map_err(|_| "cols out of u16 range"))?;
    Ok((rows, cols))
}

/// 조건이 충족될 때까지 짧은 간격으로 폴링. apply_clamped_pty_size 와 list 사이에
/// 약간의 시차가 있을 수 있어 spin 보다 polling 이 안전하다.
#[cfg(unix)]
fn wait_for_size(env: &TestEnv, name: &str, want: (u16, u16)) -> TestResult<(u16, u16)> {
    poll_until(
        Duration::from_secs(3),
        Duration::from_millis(40),
        &format!("session {name} size {want:?}"),
        || {
            let size = read_session_size(env, name)?;
            if size == want {
                Ok(PollStatus::Ready(size))
            } else {
                Ok(PollStatus::Pending(format!("last size: {size:?}")))
            }
        },
    )
}

/// PR #15 핵심 시나리오: wide desktop attach + narrow mobile attach 가 공존하는
/// 동안 PTY 는 둘의 컴포넌트별 min 으로 clamp 되어야 한다. mobile 이 detach 하면
/// PTY 는 desktop 의 사이즈로 다시 자라야 한다 — PR #14 의 polling-only 가드는
/// 잡지 못했던 자동 회복 시나리오이다.
#[test]
#[cfg(unix)]
fn pty_size_clamps_to_smallest_attached_client_and_recovers_on_detach() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "clamp-test",
            "--",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .status()?;
    assert!(status.success(), "lterm new should succeed");
    wait_for_socket(&socket)?;

    // wide desktop 먼저 attach. 이 시점엔 attach 한 명뿐이라 PTY 는 desktop 사이즈.
    let (desktop_stream, _desktop_id) = attach_with_geometry(&socket, "clamp-test", 40, 152)?;
    wait_for_size(&env, "clamp-test", (40, 152))?;

    // narrow mobile 이 추가로 attach 하면 컴포넌트별 min 으로 clamp 되어야 한다.
    let (mobile_stream, _mobile_id) = attach_with_geometry(&socket, "clamp-test", 24, 80)?;
    wait_for_size(&env, "clamp-test", (24, 80))?;

    // mobile detach (socket close → server 측 EOF → unsubscribe → apply clamp).
    // unsubscribe 가 끝나면 PTY 는 살아있는 desktop 만의 사이즈로 자라야 한다.
    drop(mobile_stream);
    wait_for_size(&env, "clamp-test", (40, 152))?;

    drop(desktop_stream);
    Ok(())
}

#[test]
#[cfg(unix)]
fn attach_preserves_input_buffered_with_request_header() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "attach-tail",
            "--",
            "sh",
            "-lc",
            "echo READY_TAIL; read line; echo GOT_TAIL:$line; sleep 2",
        ])
        .status()?;
    assert!(status.success(), "lterm new should succeed");
    wait_for_socket(&socket)?;
    env.capture_until("attach-tail", "READY_TAIL")?;

    let mut stream = UnixStream::connect(&socket)?;
    let request = serde_json::json!({
        "type": "attach",
        "target": "attach-tail",
        "rows": 24,
        "cols": 80,
    });
    let mut frame = serde_json::to_vec(&request)?;
    frame.push(b'\n');
    frame.extend_from_slice(b"BUFFERED_INPUT\n");
    // This is an end-to-end smoke for clients that pipeline attach input behind
    // the request header. Unix streams have no message boundaries, so the
    // deterministic same-read-buffer invariant is locked by the
    // `request_chunk_parser_preserves_tail_from_same_read_buffer` unit test.
    stream.write_all(&frame)?;

    let response = read_response_line(&mut stream)?;
    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    let captured = env.capture_until("attach-tail", "GOT_TAIL:BUFFERED_INPUT")?;
    assert!(captured.contains("GOT_TAIL:BUFFERED_INPUT"), "{captured}");
    drop(stream);
    Ok(())
}

#[test]
#[cfg(unix)]
fn raw_attach_live_stream_preserves_escape_and_control_bytes() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);
    let payload_path = env.temp.path().join("raw-attach-payload.bin");
    let expected_payload =
        b"\x1b[31mRED\x1b[0m\x1b]52;c;RAW_SECRET\x07\x1bPqDCS_RAW\x1b\\\0\xff\x80BIN";
    std::fs::write(&payload_path, expected_payload)?;

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "raw-bytes",
            "--",
            "sh",
            "-c",
            concat!(
                "payload=$1; ",
                "printf 'READY_RAW\\n'; ",
                "while IFS= read -r line; do ",
                "if [ \"$line\" = GO_RAW ]; then ",
                "printf 'RAW_START'; ",
                "cat \"$payload\"; ",
                "printf 'RAW_END\\n'; ",
                "while :; do sleep 60; done; ",
                "fi; ",
                "done"
            ),
        ])
        .arg("raw-byte-script")
        .arg(&payload_path)
        .status()?;
    assert!(status.success(), "lterm new should succeed");
    wait_for_socket(&socket)?;
    env.capture_until("raw-bytes", "READY_RAW")?;

    let (mut stream, _subscriber_id) = attach_with_geometry(&socket, "raw-bytes", 24, 80)?;

    let send_status = env.cmd().args(["send", "raw-bytes", "GO_RAW\n"]).status()?;
    assert!(send_status.success(), "lterm send must succeed");

    let raw = read_until_marker_bytes(&mut stream, b"RAW_END", Duration::from_secs(5))?;
    assert!(
        contains_subsequence(&raw, b"\x1b[31mRED\x1b[0m"),
        "raw attach must preserve CSI color bytes: {:?}",
        String::from_utf8_lossy(&raw)
    );
    assert!(
        contains_subsequence(&raw, b"\x1b]52;c;RAW_SECRET\x07"),
        "raw attach must preserve OSC/BEL bytes: {:?}",
        String::from_utf8_lossy(&raw)
    );
    assert!(
        contains_subsequence(&raw, b"\x1bPqDCS_RAW\x1b\\"),
        "raw attach must preserve DCS/ST bytes: {:?}",
        String::from_utf8_lossy(&raw)
    );
    let raw_start =
        find_subsequence(&raw, b"RAW_START").expect("raw output should include RAW_START");
    let csi = find_subsequence(&raw, b"\x1b[31mRED\x1b[0m").expect("raw output should include CSI");
    let osc =
        find_subsequence(&raw, b"\x1b]52;c;RAW_SECRET\x07").expect("raw output should include OSC");
    let dcs =
        find_subsequence(&raw, b"\x1bPqDCS_RAW\x1b\\").expect("raw output should include DCS");
    let raw_end = find_subsequence(&raw, b"RAW_END").expect("raw output should include RAW_END");
    assert!(
        raw_start < csi && csi < osc && osc < dcs && dcs < raw_end,
        "raw attach must preserve payload order: {:?}",
        String::from_utf8_lossy(&raw)
    );
    let payload_start = raw_start + b"RAW_START".len();
    assert_eq!(
        &raw[payload_start..raw_end],
        expected_payload,
        "raw attach must preserve the exact marker-delimited payload bytes: {:?}",
        String::from_utf8_lossy(&raw)
    );

    let captured = env.capture_until("raw-bytes", "RAW_END")?;
    assert!(captured.contains("RED"), "{captured:?}");
    // Sanitized capture surfaces must drop active control-sequence payloads,
    // not merely erase the ESC byte and leave sensitive contents.
    assert!(
        !captured.contains('\x1b'),
        "sanitized capture should still strip escapes: {captured:?}"
    );
    assert!(
        !captured.contains("RAW_SECRET"),
        "sanitized capture should still strip OSC payloads: {captured:?}"
    );
    assert!(
        !captured.contains("DCS_RAW"),
        "sanitized capture should still strip DCS payloads: {captured:?}"
    );
    assert!(
        !captured.contains('\x07'),
        "sanitized capture should still strip BEL controls: {captured:?}"
    );

    let kill = env.cmd().args(["kill", "raw-bytes"]).status()?;
    assert!(kill.success(), "raw-bytes session should be killable");

    drop(stream);
    Ok(())
}

#[test]
#[cfg(unix)]
fn instrument_polling_does_not_perturb_direct_raw_attach() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);
    let payload_path = env.temp.path().join("instrument-raw-payload.bin");
    let expected_payload = b"\0\xff\x80\x1b[35mTWIN\x1b[0m\x1b]52;c;SIDE_SECRET\x07";
    std::fs::write(&payload_path, expected_payload)?;

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "instrument-raw",
            "--",
            "sh",
            "-c",
            concat!(
                "payload=$1; ",
                "printf 'INSTRUMENT_READY\\n'; ",
                "while IFS= read -r line; do ",
                "if [ \"$line\" = GO_INSTRUMENT ]; then ",
                "printf 'INSTRUMENT_START'; ",
                "cat \"$payload\"; ",
                "printf 'INSTRUMENT_END\\n'; ",
                "fi; ",
                "done"
            ),
        ])
        .arg("instrument-raw-script")
        .arg(&payload_path)
        .status()?;
    assert!(status.success(), "lterm new should succeed");
    wait_for_socket(&socket)?;
    env.capture_until("instrument-raw", "INSTRUMENT_READY")?;

    let (mut stream, _subscriber_id) = attach_with_geometry(&socket, "instrument-raw", 37, 119)?;
    let baseline_output = env
        .cmd()
        .args(["instrument", "instrument-raw", "--json"])
        .output()?;
    assert!(baseline_output.status.success(), "{baseline_output:?}");
    let baseline: serde_json::Value = serde_json::from_slice(&baseline_output.stdout)?;
    assert_eq!(baseline["attached_clients"], 1);
    assert_eq!(
        (baseline["rows"].as_u64(), baseline["cols"].as_u64()),
        (Some(37), Some(119))
    );

    let bin = env!("CARGO_BIN_EXE_lterm").to_string();
    let runtime_dir = env.temp.path().join("run");
    let data_dir = env.temp.path().join("data");
    let tmp_dir = env.temp.path().join("tmp");
    let (poll_started_tx, poll_started_rx) = std::sync::mpsc::sync_channel(0);
    let poller = thread::spawn(move || -> Result<Vec<serde_json::Value>, String> {
        let mut snapshots = Vec::new();
        for _ in 0..40 {
            let output = Command::new(&bin)
                .args(["instrument", "instrument-raw", "--json"])
                .env_remove("LTERM_SOCKET")
                .env_remove("LTERM_PANE")
                .env_remove("LTERM_PARENT_TOKEN")
                .env("LTERM_RUNTIME_DIR", &runtime_dir)
                .env("LTERM_DATA_DIR", &data_dir)
                .env("TMPDIR", &tmp_dir)
                .output()
                .map_err(|err| format!("spawn instrument poll: {err}"))?;
            if !output.status.success() {
                return Err(format!("instrument poll failed: {output:?}"));
            }
            snapshots.push(
                serde_json::from_slice(&output.stdout)
                    .map_err(|err| format!("decode instrument poll: {err}"))?,
            );
            if snapshots.len() == 1 {
                poll_started_tx
                    .send(())
                    .map_err(|err| format!("signal first successful instrument poll: {err}"))?;
            }
        }
        Ok(snapshots)
    });

    poll_started_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|err| format!("first successful instrument poll was not observed: {err}"))?;
    let send = env
        .cmd()
        .args(["send", "instrument-raw", "GO_INSTRUMENT\n"])
        .status()?;
    assert!(send.success(), "send should succeed");
    let raw = read_until_marker_bytes(&mut stream, b"INSTRUMENT_END", Duration::from_secs(5))?;
    let start = find_subsequence(&raw, b"INSTRUMENT_START")
        .ok_or("raw output missing INSTRUMENT_START")?
        + b"INSTRUMENT_START".len();
    let end =
        find_subsequence(&raw, b"INSTRUMENT_END").ok_or("raw output missing INSTRUMENT_END")?;
    assert_eq!(
        &raw[start..end],
        expected_payload,
        "instrument polling changed marker-delimited raw bytes"
    );

    let snapshots = poller.join().map_err(|_| "instrument poller panicked")?;
    let snapshots = snapshots.map_err(|err| -> Box<dyn std::error::Error> { err.into() })?;
    let after_output = env
        .cmd()
        .args(["instrument", "instrument-raw", "--json"])
        .output()?;
    assert!(after_output.status.success(), "{after_output:?}");
    let after: serde_json::Value = serde_json::from_slice(&after_output.stdout)?;

    let mut previous_revision = baseline["output_revision"].as_u64().unwrap_or(0);
    let mut previous_bytes = baseline["output_total_bytes"].as_u64().unwrap_or(0);
    for snapshot in snapshots.iter().chain(std::iter::once(&after)) {
        assert_eq!(snapshot["attached_clients"], 1);
        assert_eq!(snapshot["rows"], 37);
        assert_eq!(snapshot["cols"], 119);
        let revision = snapshot["output_revision"]
            .as_u64()
            .ok_or("instrument revision missing")?;
        let total_bytes = snapshot["output_total_bytes"]
            .as_u64()
            .ok_or("instrument byte count missing")?;
        assert!(revision >= previous_revision, "revision regressed");
        assert!(total_bytes >= previous_bytes, "byte count regressed");
        previous_revision = revision;
        previous_bytes = total_bytes;
    }
    assert!(
        after["output_total_bytes"].as_u64().unwrap_or(0)
            > baseline["output_total_bytes"].as_u64().unwrap_or(0),
        "output counter did not advance"
    );
    assert_eq!(after["attached_clients"], baseline["attached_clients"]);
    assert_eq!(after["rows"], baseline["rows"]);
    assert_eq!(after["cols"], baseline["cols"]);

    Ok(())
}

#[test]
#[cfg(unix)]
fn cooperative_input_capability_delivers_exact_binary_and_enforces_budget() -> TestResult {
    let env = TestEnv::new()?;
    let cap = env.temp.path().join("exact-input.cap");
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "cap-binary",
            "--",
            "sh",
            "-lc",
            "stty raw -echo; printf CAP_READY\\n; dd bs=1 count=7 2>/dev/null | od -An -tx1; printf '\\nCAP_DONE\\n'; sleep 2",
        ])
        .status()?;
    assert!(status.success(), "create capability target");
    env.capture_until("cap-binary", "CAP_READY")?;

    let issue = env
        .cmd()
        .args([
            "capability",
            "issue-input",
            "cap-binary",
            "--bytes",
            "7",
            "--output",
        ])
        .arg(&cap)
        .output()?;
    assert!(issue.status.success(), "issue failed: {issue:?}");
    assert!(issue.stdout.is_empty(), "token must not reach stdout");
    assert_eq!(std::fs::metadata(&cap)?.permissions().mode() & 0o777, 0o600);
    let private = std::fs::read_to_string(&cap)?;
    assert!(private.starts_with("lterm-input-capability-v1\n"));

    let exact = [0x00, 0xff, 0x80, 0x0d, 0x0a, 0x1b, 0x41];
    let mut child = env
        .cmd()
        .args(["capability", "input", "--capability"])
        .arg(&cap)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("capability stdin unavailable")?
        .write_all(&exact)?;
    let input = child.wait_with_output()?;
    assert!(input.status.success(), "capability input failed: {input:?}");
    assert!(input.stdout.is_empty(), "capability input must be silent");
    let captured = env.capture_until("cap-binary", "CAP_DONE")?;
    let words = captured.split_whitespace().collect::<Vec<_>>();
    assert!(
        words
            .windows(7)
            .any(|window| window == ["00", "ff", "80", "0d", "0a", "1b", "41"]),
        "exact binary payload was not preserved: {captured:?}"
    );

    let over = env
        .cmd()
        .args(["capability", "input", "--capability"])
        .arg(&cap)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut over = over;
    over.stdin
        .take()
        .ok_or("over-budget stdin unavailable")?
        .write_all(b"x")?;
    let rejected = over.wait_with_output()?;
    assert!(!rejected.status.success(), "exhausted token must fail");
    assert!(
        !String::from_utf8_lossy(&rejected.stderr).contains(private.trim()),
        "stderr must not reveal the token"
    );

    let revoke = env
        .cmd()
        .args(["capability", "revoke", "--capability"])
        .arg(&cap)
        .output()?;
    assert!(
        revoke.status.success(),
        "idempotent revoke failed: {revoke:?}"
    );
    assert!(!cap.exists(), "successful revoke must unlink the file");
    Ok(())
}

#[test]
#[cfg(unix)]
fn sessions_list_sanitizes_control_sequence_metadata() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);
    let metadata_arg = "LIST_VISIBLE_\x1b]52;c;LIST_SECRET\x07\x1bPqLIST_DCS\x1b\\_AFTER";

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "list-meta",
            "--",
            "sh",
            "-c",
            "printf 'READY_LIST_META\\n'; while :; do sleep 60; done",
            "list-meta-script",
        ])
        .arg(metadata_arg)
        .status()?;
    assert!(status.success(), "lterm new should succeed");
    wait_for_socket(&socket)?;
    env.capture_until("list-meta", "READY_LIST_META")?;

    let list = env.cmd().arg("sessions").output()?;
    assert!(list.status.success(), "{list:?}");
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    let row = list_row(&list_stdout, "list-meta")
        .ok_or_else(|| {
            format!("list-meta should remain visible in sessions list: {list_stdout:?}")
        })?
        .join("\t");
    assert!(
        row.contains("LIST_VISIBLE_") && row.contains("_AFTER"),
        "sessions list test must be non-vacuous and include sanitized metadata arg: {row:?}"
    );
    assert!(
        !row.contains('\x1b'),
        "sessions list should sanitize escapes in metadata: {row:?}"
    );
    assert!(
        !row.contains('\x07'),
        "sessions list should sanitize BEL controls in metadata: {row:?}"
    );
    assert!(
        !row.contains("LIST_SECRET"),
        "sessions list should strip OSC payload contents from metadata: {row:?}"
    );
    assert!(
        !row.contains("LIST_DCS"),
        "sessions list should strip DCS payload contents from metadata: {row:?}"
    );

    let kill = env.cmd().args(["kill", "list-meta"]).status()?;
    assert!(kill.success(), "list-meta session should be killable");
    Ok(())
}

/// stale subscriber id 를 실어 보낸 Resize 는 silent no-op 이 아니라 명시적
/// 에러로 surface 되어야 한다. 그렇지 않으면 client-side race 가 보이지 않는
/// 채로 PTY 사이즈가 영원히 어긋난 상태로 남을 수 있다.
#[test]
#[cfg(unix)]
fn resize_with_stale_subscriber_id_returns_error() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);

    let status = env
        .cmd()
        .args([
            "new", "--detach", "--name", "stale-id", "--", "sh", "-lc", "sleep 30",
        ])
        .status()?;
    assert!(status.success());
    wait_for_socket(&socket)?;

    let mut stream = UnixStream::connect(&socket)?;
    let request = serde_json::json!({
        "type": "resize",
        "target": "stale-id",
        "rows": 24_u16,
        "cols": 80_u16,
        "subscriber_id": 9_999_999_u64,
    });
    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    let response = read_response_line(&mut stream)?;
    assert_eq!(
        response.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "stale subscriber id must yield ok=false; response={response}"
    );
    let error = response.get("error").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        error.contains("subscriber id"),
        "error should mention subscriber id; got {error:?}"
    );
    Ok(())
}

/// rows == 0 또는 cols == 0 의 degenerate Resize 는 server 가 거부해야 한다 —
/// 기존에도 동일하게 동작했으나 PR #15 는 subscriber_id 분기와 함께 같은 가드를
/// 그대로 유지하므로 회귀 방지용 회로로 한 번 더 박는다.
#[test]
#[cfg(unix)]
fn resize_with_zero_dimensions_returns_error() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "zero-resize",
            "--",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .status()?;
    assert!(status.success());
    wait_for_socket(&socket)?;

    let mut stream = UnixStream::connect(&socket)?;
    let request = serde_json::json!({
        "type": "resize",
        "target": "zero-resize",
        "rows": 0_u16,
        "cols": 80_u16,
    });
    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    let response = read_response_line(&mut stream)?;
    assert_eq!(
        response.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "zero rows must be rejected; response={response}"
    );

    let mut stream = UnixStream::connect(&socket)?;
    let request = serde_json::json!({
        "type": "resize",
        "target": "zero-resize",
        "rows": 24_u16,
        "cols": 0_u16,
    });
    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    let response = read_response_line(&mut stream)?;
    assert_eq!(
        response.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "zero cols must be rejected; response={response}"
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn resize_with_oversized_dimensions_returns_error() -> TestResult {
    let env = TestEnv::new()?;
    let socket = socket_path_for(&env);

    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "oversized-resize",
            "--",
            "sh",
            "-lc",
            "sleep 30",
        ])
        .status()?;
    assert!(status.success());
    wait_for_socket(&socket)?;

    let mut stream = UnixStream::connect(&socket)?;
    let request = serde_json::json!({
        "type": "resize",
        "target": "oversized-resize",
        "rows": 1001_u16,
        "cols": 80_u16,
    });
    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    let response = read_response_line(&mut stream)?;
    assert_eq!(
        response.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "oversized rows must be rejected; response={response}"
    );
    assert!(
        response
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|msg| msg.contains("exceed maximum")),
        "oversized resize should explain the maximum; response={response}"
    );

    let mut stream = UnixStream::connect(&socket)?;
    let request = serde_json::json!({
        "type": "resize",
        "target": "oversized-resize",
        "rows": 24_u16,
        "cols": 1001_u16,
    });
    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    let response = read_response_line(&mut stream)?;
    assert_eq!(
        response.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "oversized cols must be rejected; response={response}"
    );
    assert!(
        response
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|msg| msg.contains("exceed maximum")),
        "oversized cols should explain the maximum; response={response}"
    );

    let mut stream = UnixStream::connect(&socket)?;
    let request = serde_json::json!({
        "type": "resize",
        "target": "oversized-resize",
        "rows": 1000_u16,
        "cols": 1000_u16,
    });
    stream.write_all(serde_json::to_string(&request)?.as_bytes())?;
    stream.write_all(b"\n")?;
    let response = read_response_line(&mut stream)?;
    assert_eq!(
        response.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "oversized area must be rejected; response={response}"
    );
    assert!(
        response
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|msg| msg.contains("terminal area")),
        "oversized resize should explain the area limit; response={response}"
    );
    Ok(())
}

/// PR #17: attach 직후 screen-state snapshot 이 broadcast 채널의 첫 chunk 로 흘러들어
/// attach stdout 의 head 에 박혀야 하고, 그 뒤에 라이브 chunk 들이 순서대로 따라와야
/// 한다. stale text 를 출력한 뒤 화면을 지우고 fresh frame 을 만든 다음 attach 해,
/// raw ring replay 였다면 보였을 stale history 가 새 attach 에 나오지 않음을 확인한다.
#[test]
#[cfg(unix)]
fn attach_replays_screen_state_before_live_output() -> TestResult {
    let env = TestEnv::new()?;
    // stale history 를 만든 뒤 화면을 지우고 fresh frame 만 현재 terminal state 에 남긴다.
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "--name",
            "snap-order",
            "--",
            "sh",
            "-lc",
            // PS1 가 들어오지 않도록 -i 없이 단순 sleep. 충분히 길게 sleep 해 attach
            // 시점에도 세션이 살아있게 한다.
            "printf 'STALE_PRELUDE\\n'; printf '\\033[2J\\033[HFRESH_PRELUDE'; sleep 30",
        ])
        .status()?;
    assert!(status.success());

    // FRESH_PRELUDE 가 PTY output 으로 들어갔는지 capture 로 폴링 — 들어가기 전에 attach
    // 하면 snapshot 이 비어 있어 본 테스트가 검증하려는 순서가 무의미해진다.
    env.capture_until("snap-order", "FRESH_PRELUDE")?;

    // attach 를 stdin/stdout pipe 로 띄운다 — `--no-status` 는 status bar 의
    // alt-screen / cursor 제어 이스케이프를 stdout 에 끼지 않게 한다.
    let mut attach = env
        .cmd()
        .args(["attach", "snap-order", "--no-status"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let _stdin = attach.stdin.take().ok_or("missing attach stdin")?;
    let mut stdout = attach.stdout.take().ok_or("missing attach stdout")?;

    // attach stdout 에서 FRESH_PRELUDE 를 먼저 본다 — broadcast 채널 첫 chunk 가
    // screen-state snapshot 이라는 PR #17 invariant. 이 시점까지 STALE_PRELUDE 가
    // 같이 보이면 raw ring replay 로 회귀한 것이다.
    let snapshot_head =
        read_until_marker_bytes(&mut stdout, b"FRESH_PRELUDE", Duration::from_secs(5))?;
    assert!(
        !snapshot_head
            .windows(b"STALE_PRELUDE".len())
            .any(|window| window == b"STALE_PRELUDE"),
        "screen-state snapshot must not replay cleared raw history: {:?}",
        String::from_utf8_lossy(&snapshot_head)
    );

    // 그 다음 라이브 stream 을 트리거하기 위해 호환 명령인 send 로 새 데이터를
    // 쏘아 넣는다 — `lterm send` 는 PTY writer 로 직접 쓰므로 PTY echo + reader
    // 경로를 거쳐 attach stdout 으로 흘러나온다.
    let send_status = env
        .cmd()
        .args(["send", "snap-order", "AFTER_PRELUDE\n"])
        .status()?;
    assert!(send_status.success(), "lterm send must succeed");

    // attach stdout 에서 LIVE marker 를 마저 읽는다. 두 번의 `read_until_marker` 가
    // 같은 stdout 위에서 순차로 검색하므로 (PR #16 quad-review LOW Forge 후속):
    // AFTER_PRELUDE 가 발견된 시점에 PRELUDE 는 이미 그 앞 byte 위치에서 발견되었음이
    // stream 순서로 강제된다. 별도 buffer 의 offset 비교 (`live_pos > prelude_pos`)
    // 는 두 buffer 가 분리된 시점부터 invalid 했으므로 제거.
    let _live_end = read_until_marker(&mut stdout, b"AFTER_PRELUDE", Duration::from_secs(5))?;

    drop(stdout);
    let _ = attach.kill();
    let _ = attach.wait();
    Ok(())
}

/// `stdout` 에서 `marker` 의 마지막 바이트가 등장한 byte offset 을 반환한다. timeout
/// 안에 못 찾으면 에러. 본 helper 는 attach 출력 head 부분의 순서 검증에만 쓰이며,
/// PTY 가 보내는 LF/CR 변환은 무시하고 marker 의 raw 바이트만 검사한다.
///
/// PR #16 quad-review MEDIUM 후속 (Codex/Forge 합의): 이전 구현은 `Instant::now <
/// deadline` 루프가 blocking `stdout.read()` 를 감싸는 구조였다. marker 가 절대
/// 도착하지 않더라도 child 가 살아있으면 `read()` 가 무한정 블록되어 `timeout`
/// 인자가 사실상 무력화됐다. fd 를 `O_NONBLOCK` 으로 두고 `WouldBlock` 시 짧게
/// sleep 하며 deadline 을 다시 본다 — child 가 살아있어도 진짜로 timeout 이 강제된다.
/// (reader 스레드 + `mpsc::recv_timeout` 패턴은 marker 발견 후 `reader.join()` 이
/// 매달린 read() 를 기다리며 hang 하는 부수효과가 있어 NONBLOCK 방식을 채택.)
///
/// fd 의 NONBLOCK 플래그는 본 호출 이후에도 유지된다 — caller 가 같은 reader 로
/// 후속 read 를 부르면 모두 NONBLOCK 으로 동작한다 (본 helper 의 일관된 사용 패턴).
/// 본 fd 는 child stdout 만 다루는 테스트 컨텍스트라 외부 영향을 걱정하지 않아도 된다.
#[cfg(unix)]
fn read_until_marker<R: Read + AsRawFd>(
    stdout: &mut R,
    marker: &[u8],
    timeout: Duration,
) -> TestResult<usize> {
    let buf = read_until_marker_bytes(stdout, marker, timeout)?;
    Ok(find_subsequence(&buf, marker).expect("marker already found") + marker.len())
}

#[cfg(unix)]
fn read_until_marker_bytes<R: Read + AsRawFd>(
    stdout: &mut R,
    marker: &[u8],
    timeout: Duration,
) -> TestResult<Vec<u8>> {
    // fd 를 non-blocking 으로 전환. 이미 non-blocking 이어도 멱등.
    let fd = stdout.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(format!(
                "failed to read marker stream flags: {}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        if (flags & libc::O_NONBLOCK) == 0 {
            let set = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            if set < 0 {
                return Err(format!(
                    "failed to set marker stream nonblocking: {}",
                    std::io::Error::last_os_error()
                )
                .into());
            }
        }
    }

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    while Instant::now() < deadline {
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subsequence(&buf, marker) {
                    buf.truncate(pos + marker.len());
                    return Ok(buf);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                return Err(format!("stdout read error: {err}").into());
            }
        }
    }
    Err(format!(
        "marker {:?} not found within {:?}; buffer head: {:?}",
        std::str::from_utf8(marker).unwrap_or("<binary>"),
        timeout,
        String::from_utf8_lossy(&buf[..buf.len().min(256)])
    )
    .into())
}

#[cfg(unix)]
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(unix)]
fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    find_subsequence(haystack, needle).is_some()
}

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const DEFAULT_SOAK_DURATION_SECS: u64 = 120;
const DEFAULT_SOAK_SESSIONS: usize = 8;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const SHORT_TIMEOUT: Duration = Duration::from_secs(3);
const SOAK_SESSION_SCRIPT: &str = r#"
name=$1
printf 'READY:%s\n' "$name"
while IFS= read -r line; do
  printf 'ECHO:%s:%s\n' "$name" "$line"
  if [ "$line" = STOP ]; then
    exit 0
  fi
done
"#;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct TestEnv {
    temp: tempfile::TempDir,
}

impl TestEnv {
    fn new() -> TestResult<Self> {
        Ok(Self {
            temp: tempfile::tempdir()?,
        })
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lterm"));
        cmd.env("LTERM_RUNTIME_DIR", self.temp.path().join("run"));
        cmd.env("LTERM_DATA_DIR", self.temp.path().join("data"));
        cmd
    }

    fn run(&self, args: &[&str]) -> TestResult<Output> {
        let mut cmd = self.cmd();
        cmd.args(args);
        run_command(cmd, COMMAND_TIMEOUT, &args.join(" "))
    }

    fn start_soak_session(&self, name: &str) -> TestResult {
        let mut cmd = self.cmd();
        cmd.args([
            "start",
            "--detach",
            "--name",
            name,
            "--",
            "sh",
            "-lc",
            SOAK_SESSION_SCRIPT,
            "sh",
            name,
        ]);
        let output = run_command(cmd, COMMAND_TIMEOUT, &format!("start {name}"))?;
        assert_success(&output, &format!("start {name}"))?;
        self.capture_until(name, &format!("READY:{name}"))?;
        Ok(())
    }

    fn capture_until(&self, target: &str, needle: &str) -> TestResult<String> {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut last = String::new();
        while Instant::now() < deadline {
            let output = self.run(&["logs", target, "-S=-80"])?;
            if output.status.success() {
                last = String::from_utf8_lossy(&output.stdout).to_string();
                if last.contains(needle) {
                    return Ok(last);
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(format!("timed out waiting for {needle:?}; last capture: {last:?}").into())
    }

    fn sessions_json(&self) -> TestResult<Vec<Value>> {
        let output = self.run(&["sessions", "--json"])?;
        assert_success(&output, "sessions --json")?;
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    fn wait_for_session(&self, name: &str) -> TestResult<Value> {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut last = Vec::new();
        while Instant::now() < deadline {
            last = self.sessions_json()?;
            if let Some(session) = find_session(&last, name) {
                return Ok(session.clone());
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(format!("timed out waiting for session {name:?}; last sessions: {last:?}").into())
    }

    fn wait_for_session_absent(&self, name: &str) -> TestResult {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut last = Vec::new();
        while Instant::now() < deadline {
            last = self.sessions_json()?;
            if find_session(&last, name).is_none() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(format!("timed out waiting for session {name:?} to disappear: {last:?}").into())
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let mut cmd = self.cmd();
        cmd.arg("shutdown");
        let _ = run_command(cmd, COMMAND_TIMEOUT, "shutdown");
    }
}

#[derive(Clone, Debug)]
struct SoakProfile {
    duration: Duration,
    sessions: usize,
}

impl SoakProfile {
    fn from_env() -> TestResult<Self> {
        let duration_secs = env_u64("LTERM_SOAK_DURATION", DEFAULT_SOAK_DURATION_SECS)?;
        let sessions = env_usize("LTERM_SOAK_SESSIONS", DEFAULT_SOAK_SESSIONS)?.max(1);
        Ok(Self {
            duration: Duration::from_secs(duration_secs.max(1)),
            sessions,
        })
    }
}

#[derive(Debug)]
struct SessionHandle {
    name: String,
    process_id: Option<String>,
}

#[test]
#[ignore = "manual/release-gate soak; set LTERM_SOAK_DURATION and LTERM_SOAK_SESSIONS"]
fn release_gate_soak_profile() -> TestResult {
    let profile = SoakProfile::from_env()?;
    let env = TestEnv::new()?;
    let mut sessions = Vec::with_capacity(profile.sessions);

    for index in 0..profile.sessions {
        let name = format!("soak-{index}");
        env.start_soak_session(&name)?;
        let session = env.wait_for_session(&name)?;
        sessions.push(SessionHandle {
            name,
            process_id: session_pid(&session),
        });
    }

    attach_and_detach(&env, &sessions[0].name)?;
    exercise_active_sessions(&env, &profile, &sessions)?;
    exercise_watch_exit(&env)?;
    cleanup_sessions(&env, &sessions)?;
    exercise_daemon_restart(&env)?;
    Ok(())
}

fn exercise_active_sessions(
    env: &TestEnv,
    profile: &SoakProfile,
    sessions: &[SessionHandle],
) -> TestResult {
    let deadline = Instant::now() + profile.duration;
    let mut tick = 0usize;
    while Instant::now() < deadline {
        let session = &sessions[tick % sessions.len()];
        let marker = format!("tick-{tick}");
        let needle = format!("ECHO:{}:{marker}", session.name);

        let input = env.run(&["input", &session.name, &marker, "--enter"])?;
        assert_success(&input, &format!("input {}", session.name))?;

        let wait = env.run(&[
            "wait",
            &session.name,
            "--contains",
            &needle,
            "--tail",
            "40",
            "--timeout",
            "5s",
            "--json",
        ])?;
        assert_success(&wait, &format!("wait --contains {needle}"))?;
        let wait_json: Value = serde_json::from_slice(&wait.stdout)?;
        assert_eq!(wait_json["matched"], Value::Bool(true), "{wait_json:?}");
        assert_eq!(wait_json["timed_out"], Value::Bool(false), "{wait_json:?}");

        let logs = env.capture_until(&session.name, &needle)?;
        assert!(logs.contains(&needle), "missing {needle:?} in {logs:?}");

        assert_sessions_present(env, sessions)?;
        assert_processes_json(env)?;

        tick += 1;
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

fn attach_and_detach(env: &TestEnv, name: &str) -> TestResult {
    let mut cmd = env.cmd();
    cmd.args(["resume", name, "--no-status"])
        .stdin(Stdio::null());
    let output = run_command(cmd, SHORT_TIMEOUT, &format!("resume {name} --no-status"))?;
    assert_success(&output, &format!("resume {name} --no-status"))
}

fn exercise_watch_exit(env: &TestEnv) -> TestResult {
    let name = "soak-watch-exit";
    env.start_soak_session(name)?;

    let mut watch_cmd = env.cmd();
    watch_cmd
        .args(["watch", name, "--exit", "--timeout", "10s", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let watch_child = watch_cmd.spawn()?;

    // Give the watcher a brief chance to subscribe before triggering exit so
    // the soak harness verifies watch lifecycle behavior instead of racing a
    // session that has already disappeared.
    thread::sleep(Duration::from_millis(500));

    let input = env.run(&["input", name, "STOP", "--enter"])?;
    assert_success(&input, "input soak-watch-exit STOP")?;

    let watch = wait_child_output(watch_child, COMMAND_TIMEOUT, "watch --exit")?;
    assert_success(&watch, "watch --exit")?;
    let watch_json: Value = serde_json::from_slice(&watch.stdout)?;
    assert_eq!(
        watch_json["event"],
        Value::String("exit".to_string()),
        "{watch_json:?}"
    );
    assert_eq!(watch_json["matched"], Value::Bool(true), "{watch_json:?}");
    assert_eq!(
        watch_json["timed_out"],
        Value::Bool(false),
        "{watch_json:?}"
    );
    env.wait_for_session_absent(name)
}

fn cleanup_sessions(env: &TestEnv, sessions: &[SessionHandle]) -> TestResult {
    for session in sessions {
        let output = env.run(&["close", &session.name])?;
        assert_success(&output, &format!("close {}", session.name))?;
        env.wait_for_session_absent(&session.name)?;
        if let Some(pid) = session.process_id.as_deref() {
            wait_for_pid_exit(pid)?;
        }
    }
    Ok(())
}

fn exercise_daemon_restart(env: &TestEnv) -> TestResult {
    let shutdown = env.run(&["shutdown"])?;
    assert_success(&shutdown, "shutdown before restart")?;

    let empty = env.sessions_json()?;
    assert!(
        empty.is_empty(),
        "new daemon after shutdown should start empty: {empty:?}"
    );

    env.start_soak_session("soak-restart")?;
    let sessions = env.sessions_json()?;
    assert!(
        find_session(&sessions, "soak-restart").is_some(),
        "restart session missing: {sessions:?}"
    );
    let close = env.run(&["close", "soak-restart"])?;
    assert_success(&close, "close soak-restart")?;
    env.wait_for_session_absent("soak-restart")?;
    let final_shutdown = env.run(&["shutdown"])?;
    assert_success(&final_shutdown, "final shutdown")
}

fn assert_sessions_present(env: &TestEnv, expected: &[SessionHandle]) -> TestResult {
    let sessions = env.sessions_json()?;
    for expected_session in expected {
        assert!(
            find_session(&sessions, &expected_session.name).is_some(),
            "missing session {}; sessions={sessions:?}",
            expected_session.name
        );
    }
    Ok(())
}

fn assert_processes_json(env: &TestEnv) -> TestResult {
    let output = env.run(&["processes", "--json", "--orphans"])?;
    assert_success(&output, "processes --json --orphans")?;
    let rows: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        rows.is_array(),
        "processes JSON should be an array: {rows:?}"
    );
    Ok(())
}

fn find_session<'a>(sessions: &'a [Value], name: &str) -> Option<&'a Value> {
    sessions
        .iter()
        .find(|row| row.get("name").and_then(Value::as_str) == Some(name))
}

fn session_pid(session: &Value) -> Option<String> {
    session
        .get("process_id")
        .and_then(Value::as_i64)
        .map(|pid| pid.to_string())
}

fn env_u64(name: &str, default: u64) -> TestResult<u64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|err| format!("{name} must be an integer number of seconds: {err}").into()),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(format!("failed to read {name}: {err}").into()),
    }
}

fn env_usize(name: &str, default: usize) -> TestResult<usize> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|err| format!("{name} must be an integer: {err}").into()),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(format!("failed to read {name}: {err}").into()),
    }
}

fn run_command(mut command: Command, timeout: Duration, label: &str) -> TestResult<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn()?;
    wait_child_output(child, timeout, label)
}

fn wait_child_output(mut child: Child, timeout: Duration, label: &str) -> TestResult<Output> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        thread::sleep(Duration::from_millis(25));
    }

    let _ = child.kill();
    let output = child.wait_with_output()?;
    Err(format!(
        "timed out running {label:?} after {timeout:?}; stdout={:?}; stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

fn assert_success(output: &Output, label: &str) -> TestResult {
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{label} failed: status={:?}; stdout={:?}; stderr={:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

#[cfg(unix)]
fn wait_for_pid_exit(pid: &str) -> TestResult {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    while Instant::now() < deadline {
        if !pid_alive(pid)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!("pid {pid} still alive after timeout").into())
}

#[cfg(not(unix))]
fn wait_for_pid_exit(_pid: &str) -> TestResult {
    Ok(())
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
            "ps failed while checking pid {pid}: status={:?}, stderr={stderr}",
            output.status.code()
        )
        .into());
    }
    let stat = String::from_utf8_lossy(&output.stdout);
    let stat = stat.trim();
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

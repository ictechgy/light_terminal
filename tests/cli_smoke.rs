use std::collections::BTreeSet;
#[cfg(unix)]
use std::fs::File;
use std::io::{Read, Write};
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

    fn capture_until(&self, target: &str, needle: &str) -> TestResult<String> {
        self.capture_command_until("logs", target, needle)
    }

    fn capture_command_until(
        &self,
        command: &str,
        target: &str,
        needle: &str,
    ) -> TestResult<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = String::new();
        while Instant::now() < deadline {
            let output = self.cmd().args([command, target, "-S=-20"]).output()?;
            if output.status.success() {
                last = String::from_utf8_lossy(&output.stdout).to_string();
                if last.contains(needle) {
                    return Ok(last);
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(format!("timed out waiting for {needle:?}; last capture: {last}").into())
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
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        if child.try_wait()?.is_none() {
            match child.kill() {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => {}
                Err(err) => {
                    return Err(format!("failed to kill child {}: {err}", child.id()).into());
                }
            }
        }
        let mut child = self.child.take().ok_or("child already reaped")?;
        child.wait()?;
        Ok(())
    }
}

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        let _ = self.kill_and_wait();
    }
}

fn wait_for_child_success(child: &mut ChildCleanup, label: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(status) = child.child_mut()?.try_wait()? {
            assert!(status.success(), "{label} failed: {status:?}");
            child.child = None;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(format!("timed out waiting for {label}").into())
}

fn list_row<'a>(stdout: &'a str, name: &str) -> Option<Vec<&'a str>> {
    stdout
        .lines()
        .find(|line| line.starts_with(&format!("{name}\t")))
        .map(|line| line.split('\t').collect())
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

fn assert_stderr_contains(output: &std::process::Output, expected: &str) {
    // These fragments are part of lterm's user-facing CLI error contract.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "{stderr:?}");
}

fn wait_for_pid_exit(pid: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !pid_alive(pid)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!("pid {pid} still alive after timeout").into())
}

fn wait_for_file_contents(path: &Path) -> TestResult<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last_err = None;
    while Instant::now() < deadline {
        match std::fs::read_to_string(path) {
            Ok(contents) if !contents.trim().is_empty() => return Ok(contents),
            Ok(_) => {}
            Err(err) => last_err = Some(err),
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "timed out waiting for file {}; last error: {:?}",
        path.display(),
        last_err
    )
    .into())
}

fn wait_for_no_client_rows(env: &TestEnv, sessions: &[&str]) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = String::new();
    while Instant::now() < deadline {
        let output = env
            .cmd()
            .args(["tmux-compat", "list-clients", "-F", "#{client_session}"])
            .output()?;
        if output.status.success() {
            last = String::from_utf8_lossy(&output.stdout).to_string();
            if !last.lines().any(|line| sessions.contains(&line)) {
                return Ok(());
            }
        } else {
            last = format!("{output:?}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!("timed out waiting for client rows to detach: {last:?}").into())
}

fn wait_for_session_absent(env: &TestEnv, session: &str) -> TestResult {
    wait_for_session_absent_for(env, session, Duration::from_secs(10))
}

fn wait_for_session_absent_for(env: &TestEnv, session: &str, timeout: Duration) -> TestResult {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        let output = env.cmd().arg("ls").output()?;
        if output.status.success() {
            last = String::from_utf8_lossy(&output.stdout).to_string();
            if list_row(&last, session).is_none() {
                return Ok(());
            }
        } else {
            last = format!("{output:?}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("timed out waiting for session {session:?} to exit: {last:?}").into())
}

fn wait_for_session_present(env: &TestEnv, session: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = String::new();
    while Instant::now() < deadline {
        let output = env.cmd().arg("ls").output()?;
        if output.status.success() {
            last = String::from_utf8_lossy(&output.stdout).to_string();
            if list_row(&last, session).is_some() {
                return Ok(());
            }
        } else {
            last = format!("{output:?}");
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("timed out waiting for session {session:?} to appear: {last:?}").into())
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
        let output = env
            .cmd()
            .args([alias, "alias-attach", "--no-status"])
            .stdin(Stdio::null())
            .output()?;
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
        "tmux-compat",
        "wait",
        "watch",
        "init",
        "notify",
        "agents",
        "agent",
        "omx",
        "omc",
        "claude",
        "codex",
        "gemini",
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
        "Built-in, configured, or PATH-resolved custom profile name, e.g. claude, codex, gemini",
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
            "gemini",
            "Arguments forwarded to gemini; use `--` before args that look like lterm options",
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
            "open" | "attach-or-new" | "ssh" => &["defaults to main"][..],
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
        let output = env
            .cmd()
            .args([command, target, "--no-status"])
            .stdin(Stdio::null())
            .output()?;
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
                "echo READY; sleep 5",
            ])
            .status()?;
        assert!(
            status.success(),
            "failed to create existing target {target}"
        );
        let captured = env.capture_until(target, "READY")?;
        assert!(captured.contains("READY"), "missing output: {captured}");

        let started = Instant::now();
        let output = env
            .cmd()
            .args([command, target, "--no-status"])
            .stdin(Stdio::null())
            .output()?;
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
    let output = env
        .cmd()
        .stdin(Stdio::null())
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
        .output()?;
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
        children.push(
            env.cmd()
                .args(["new", "--detach", "--", "sh", "-lc", "sleep 2"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }

    let mut panes = std::collections::HashSet::new();
    for child in children {
        let output = child.wait_with_output()?;
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
        let script = format!("echo {marker}; sleep 2");
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
    let output = child.wait_with_output()?;
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
               new-split) printf '%s\\n' 'OK surface:42 workspace:1'; exit 0 ;;\n\
               send) printf '%s\\n' 'OK noisy send output'; exit 0 ;;\n\
               close-surface) printf '%s\\n' 'OK noisy close output'; exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            shlex::try_quote(&cmux_log.display().to_string())?
        ),
    )?;

    let output = env
        .cmd()
        .env("CMUX_WORKSPACE_ID", "workspace-for-noisy-cmux")
        .env("PATH", &fake_bin)
        .args([
            "tmux-compat",
            "split-window",
            "-hPF",
            "#{pane_id}",
            "echo SPLIT_NOISY_READY; sleep 2",
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
        cmux_calls
            .lines()
            .any(|line| line == "send --surface surface:42 exec "
                || line.starts_with("send --surface surface:42 exec ")),
        "attach command should target the new-split surface from stdout: {cmux_calls:?}"
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
fn tmux_compat_split_window_target_is_rejected_before_session_creation() -> TestResult {
    let env = TestEnv::new()?;
    let before = session_names_json(&env)?;

    let output = env
        .cmd()
        .args([
            "tmux-compat",
            "split-window",
            "-d",
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
            "#!/bin/sh\nprintf '%s\n' \"$*\" >> {}\nexit 42\n",
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
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .any(|line| line == "new-split down --focus true"),
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
        "cmux identify did not report a new split surface id",
    );
    let cmux_calls = wait_for_file_contents(&cmux_log)?;
    assert!(
        cmux_calls
            .lines()
            .any(|line| line == "new-split right --focus true"),
        "fake cmux should record the split attempt: {cmux_calls:?}"
    );
    assert!(
        cmux_calls.lines().any(|line| line == "close-surface"),
        "missing surface id should close the focused split surface: {cmux_calls:?}"
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
            .any(|line| line == "new-split right --focus true"),
        "fake cmux should record the split attempt: {cmux_calls:?}"
    );
    assert!(
        cmux_calls
            .lines()
            .any(|line| line == "close-surface --surface surface:42"),
        "failed lterm creation should roll back the cmux surface reported by new-split stdout: {cmux_calls:?}"
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
    thread::sleep(Duration::from_millis(200));
    master.write_all(b"\x1b")?;
    master.flush()?;

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while Instant::now() < deadline {
        if let Some(status) = compose.child_mut()?.try_wait()? {
            assert!(status.success(), "interactive compose failed: {status:?}");
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(exited, "interactive compose did not exit after local Esc");
    compose.kill_and_wait()?;

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
            "#{session_name}:#{window_index}:#{window_name}:#{window_id}:#{window_panes}:#{window_active}:#{pane_width}:#{window_width}:#{pane_height}:#{window_height}",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let row = stdout
        .lines()
        .find(|line| line.starts_with("window-query:0:window-query:@"))
        .ok_or_else(|| format!("window-query row missing: {stdout:?}"))?;
    let fields: Vec<_> = row.split(':').collect();
    assert_eq!(fields.len(), 10, "{row:?}");
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
    assert!(
        stdout.lines().any(|line| line == "window-one"),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().any(|line| line == "window-two"),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().any(|line| line == "foo-target"),
        "{stdout:?}"
    );

    let clustered = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-aF", "#{session_name}"])
        .output()?;
    assert!(clustered.status.success(), "{clustered:?}");
    let stdout = String::from_utf8_lossy(&clustered.stdout);
    assert!(
        stdout.lines().any(|line| line == "window-one"),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().any(|line| line == "window-two"),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().any(|line| line == "foo-target"),
        "{stdout:?}"
    );

    let clustered_inline = env
        .cmd()
        .env("TMUX_PANE", window_one_pane)
        .args(["tmux-compat", "list-windows", "-aF#{session_name}"])
        .output()?;
    assert!(clustered_inline.status.success(), "{clustered_inline:?}");
    let stdout = String::from_utf8_lossy(&clustered_inline.stdout);
    assert!(
        stdout.lines().any(|line| line == "window-one"),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().any(|line| line == "window-two"),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().any(|line| line == "foo-target"),
        "{stdout:?}"
    );

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
    assert!(
        stdout.lines().any(|line| line == "window-one"),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().any(|line| line == "window-two"),
        "{stdout:?}"
    );
    assert!(
        stdout.lines().any(|line| line == "foo-target"),
        "{stdout:?}"
    );

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
        ["claude", "codex", "gemini", "omx", "omc"],
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

    let omx = stdout
        .lines()
        .find(|line| line.starts_with("omx\t"))
        .ok_or("missing omx row")?;
    let fields: Vec<_> = omx.split('\t').collect();
    assert_eq!(fields[3], "on", "{omx:?}");
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
    assert_eq!(profiles.len(), 5, "{profiles:?}");
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
    let omx = profiles
        .iter()
        .find(|row| row["profile"] == "omx")
        .ok_or("missing omx JSON row")?;
    assert_eq!(omx["status_default"], true);
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
    write_executable(
        &fake_bin.join("gemini"),
        r#"#!/bin/sh
printf 'LTERM_AGENT:%s\n' "$LTERM_AGENT"
printf 'LTERM_SESSION:%s\n' "$LTERM_SESSION"
printf 'ARG1:%s\n' "$1"
"#,
    )?;
    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{old_path}", fake_bin.display());

    let output = env
        .cmd()
        .env("PATH", &path)
        .stdin(Stdio::null())
        .args(["gemini", "--", "-p"])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("LTERM_AGENT:gemini"), "{stdout:?}");
    assert!(stdout.contains("LTERM_SESSION:gemini-lterm"), "{stdout:?}");
    assert!(stdout.contains("ARG1:-p"), "{stdout:?}");
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
    let quoted_socket = fish_quote_for_test(&expected_socket);
    let tmux_prefix = format!(
        "set -gx TMUX {},",
        quoted_socket
            .strip_suffix('\'')
            .expect("test quote should end with a single quote")
    );
    assert!(
        lines[1].starts_with(&tmux_prefix) && lines[1].ends_with(",0'"),
        "fish TMUX line should be quoted socket,pid,0: {:?}",
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
            sourced_lines.get(1).is_some_and(|tmux| tmux
                .starts_with(&format!("{expected_socket},"))
                && tmux.ends_with(",0")),
            "{fish_stdout:?}"
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
    assert_eq!(
        lines.first(),
        Some(&expected_socket.as_str()),
        "{eval_stdout:?}"
    );
    assert!(
        lines.get(1).is_some_and(
            |tmux| tmux.starts_with(&format!("{expected_socket},")) && tmux.ends_with(",0")
        ),
        "{eval_stdout:?}"
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

// 부모 프로세스 관점의 default fallback runtime socket path
// (env::temp_dir()/light-terminal-{euid}/lterm.sock). 가드 검사와 진단 메시지가
// 동일한 경로를 참조하도록 단일 출처로 분리한다.
#[cfg(unix)]
fn default_runtime_socket_path() -> std::path::PathBuf {
    // SAFETY: geteuid(2) is POSIX-required thread-safe and infallible.
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir()
        .join(format!("light-terminal-{uid}"))
        .join("lterm.sock")
}

// 주어진 socket path에 lterm 데몬이 protocol 수준에서 살아있는지 확인한다.
// lterm CLI의 `doctor --json`을 LTERM_SOCKET override 하에 spawn해서 그 결과의
// `daemon_reachable` 필드를 검사한다. doctor는 (HANDOFF: "auto-spawn next lterm
// command other than doctor/shutdown") auto-spawn하지 않으므로 false positive를
// 만들지 않는다.
//
// helper는 path 인자를 받는 분리 형태이므로 임시 bait UnixListener에 대해서도
// 검증 테스트가 가능하다. 가드 본문은 default_runtime_daemon_reports_reachable
// wrapper를 사용한다.
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

// 부모 호스트의 default fallback runtime path에 lterm 데몬이 실제로 살아있는지
// (protocol 수준) 확인한다. stale 소켓이나 임의 Unix listener는 false 반환.
// `default_runtime_socket_accepts_connections`보다 false-positive가 낮다.
#[cfg(unix)]
fn default_runtime_daemon_reports_reachable() -> bool {
    runtime_daemon_reports_reachable_at(&default_runtime_socket_path())
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
// 사용자 데몬을 침범할 위험이 있다. PR #75 의
// default_runtime_daemon_reports_reachable 가드가 보수적 안전망이지만, 1차
// 방어선은 sandbox TMPDIR 환경 격리이다 (PR #76 quad-review 합의 — TMPDIR
// isolation is the real protection). 같은 fallback 검증 패턴이 필요한 새 테스트는
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

// 위 헬퍼가 true일 때 사용하는 opt-in skip env. 이 env가 설정되어 있어야만
// 테스트가 silent skip(`Ok(())`)으로 빠진다. 미설정 시에는 panic하여 cargo test의
// FAIL 출력으로 "이 호스트는 안전하지 않다"는 신호가 가시화된다. CI는 default
// 경로의 socket이 비어있으므로 가드 자체가 발동하지 않아 본문이 그대로 실행된다.
#[cfg(unix)]
const LTERM_TEST_ALLOW_OCCUPIED_SKIP_ENV: &str = "LTERM_TEST_ALLOW_OCCUPIED_SKIP";

#[test]
#[cfg(unix)]
fn default_tmp_runtime_dir_is_private_and_not_a_symlink() -> TestResult {
    // 부모 호스트의 default fallback runtime path에 어떤 Unix listener가 떠 있으면
    // 이 테스트가 (LTERM_RUNTIME_DIR을 제거한 상태로) 그 path 인근의 사용자 데몬과
    // 상호작용해 attached 세션을 끊을 risk가 있다. 가드 발동 시 기본은 panic하여
    // cargo test FAIL 출력으로 회귀 신호가 가시화되도록 한다. 호스트에 의도적으로
    // 데몬을 띄운 개발자는 LTERM_TEST_ALLOW_OCCUPIED_SKIP env로 silent skip을
    // opt-in 할 수 있다 (이 경우 cargo test는 PASS로 카운트하므로 신호는 CI에서만
    // 보장된다). CI 환경은 default 경로 socket이 비어있어 가드가 발동하지 않으므로
    // 본문이 항상 실행된다.
    if default_runtime_daemon_reports_reachable() {
        let socket = default_runtime_socket_path();
        if std::env::var_os(LTERM_TEST_ALLOW_OCCUPIED_SKIP_ENV).is_some() {
            eprintln!(
                "skip default_tmp_runtime_dir_is_private_and_not_a_symlink: \
                 default runtime socket {} hosts a live lterm daemon \
                 ({LTERM_TEST_ALLOW_OCCUPIED_SKIP_ENV} set; any non-empty value)",
                socket.display()
            );
            return Ok(());
        }
        panic!(
            "default_tmp_runtime_dir_is_private_and_not_a_symlink would race with a live lterm \
             daemon currently reachable at {} (doctor --json daemon_reachable=true). \
             Either stop that daemon before running this test, or set \
             {LTERM_TEST_ALLOW_OCCUPIED_SKIP_ENV}=1 (any non-empty value) to opt-in to skipping \
             this test on hosts with an intentionally running lterm daemon.",
            socket.display()
        );
    }

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
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lterm"));
    let output = daemon
        .env("LTERM_SOCKET", "/tmp/lterm-insecure-test.sock")
        .env("LTERM_DATA_DIR", temp.path().join("data"))
        .arg("daemon")
        .output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("owned by uid")
            || stderr.contains("must not be a symlink")
            || stderr.contains("not a directory"),
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
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if UnixStream::connect(path).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(format!("daemon socket {} did not appear in time", path.display()).into())
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
        .ok_or("missing rows")? as u16;
    let cols = session
        .get("cols")
        .and_then(|v| v.as_u64())
        .ok_or("missing cols")? as u16;
    Ok((rows, cols))
}

/// 조건이 충족될 때까지 짧은 간격으로 폴링. apply_clamped_pty_size 와 list 사이에
/// 약간의 시차가 있을 수 있어 spin 보다 polling 이 안전하다.
#[cfg(unix)]
fn wait_for_size(env: &TestEnv, name: &str, want: (u16, u16)) -> TestResult<(u16, u16)> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last = (0_u16, 0_u16);
    while Instant::now() < deadline {
        last = read_session_size(env, name)?;
        if last == want {
            return Ok(last);
        }
        thread::sleep(Duration::from_millis(40));
    }
    Err(format!("session {name} size = {last:?}, expected {want:?}").into())
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
        if flags >= 0 && (flags & libc::O_NONBLOCK) == 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
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

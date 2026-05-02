use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

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

    fn capture_until(&self, target: &str, needle: &str) -> TestResult<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = String::new();
        while Instant::now() < deadline {
            let output = self.cmd().args(["capture", target, "-S=-20"]).output()?;
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

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = self.cmd().arg("shutdown").status();
    }
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
fn new_attaches_by_default() -> TestResult {
    let env = TestEnv::new()?;
    let output = env
        .cmd()
        .args([
            "new",
            "-n",
            "attached",
            "--",
            "sh",
            "-lc",
            "echo ATTACHED_BY_DEFAULT; sleep 1",
        ])
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ATTACHED_BY_DEFAULT"),
        "{output:?}"
    );
    Ok(())
}

#[test]
fn new_short_name_and_ls_alias_work() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
            "--detach",
            "-n",
            "shorty",
            "--",
            "sh",
            "-lc",
            "echo SHORTY; sleep 2",
        ])
        .status()?;
    assert!(status.success());

    let listed = env.cmd().arg("ls").output()?;
    assert!(listed.status.success(), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("shorty"), "{stdout}");

    let captured = env.capture_until("shorty", "SHORTY")?;
    assert!(captured.contains("SHORTY"));
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
            "echo READY; read line; echo GOT:$line; sleep 2",
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("session name"), "{stderr}");
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

#[test]
#[cfg(unix)]
fn default_tmp_runtime_dir_is_private_and_not_a_symlink() -> TestResult {
    let temp = tempfile::tempdir()?;
    let tmp = temp.path().join("tmp");
    let data = temp.path().join("data");
    std::fs::create_dir(&tmp)?;

    let mut list = Command::new(env!("CARGO_BIN_EXE_lterm"));
    list.env_remove("LTERM_RUNTIME_DIR")
        .env_remove("LTERM_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", &tmp)
        .env("LTERM_DATA_DIR", &data)
        .arg("list");
    let output = list.output()?;
    assert!(output.status.success(), "{output:?}");

    let uid = std::fs::metadata(&tmp)?.uid();
    let runtime = tmp.join(format!("light-terminal-{uid}"));
    let meta = std::fs::symlink_metadata(&runtime)?;
    assert!(!meta.file_type().is_symlink());
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);

    let mut shutdown = Command::new(env!("CARGO_BIN_EXE_lterm"));
    let _ = shutdown
        .env_remove("LTERM_RUNTIME_DIR")
        .env_remove("LTERM_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .env("TMPDIR", &tmp)
        .env("LTERM_DATA_DIR", &data)
        .arg("shutdown")
        .status();
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

    let ps_output = env.cmd().args(["ps", "pgrp", "--json"]).output()?;
    assert!(ps_output.status.success());
    assert!(
        String::from_utf8_lossy(&ps_output.stdout).contains(child_pid),
        "lterm ps should include child process tree"
    );

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

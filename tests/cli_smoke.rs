use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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

#[test]
fn keeps_session_and_captures_output() -> TestResult {
    let env = TestEnv::new()?;
    let status = env
        .cmd()
        .args([
            "new",
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

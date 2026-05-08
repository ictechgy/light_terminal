use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

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

fn list_row<'a>(stdout: &'a str, name: &str) -> Option<Vec<&'a str>> {
    stdout
        .lines()
        .find(|line| line.starts_with(&format!("{name}\t")))
        .map(|line| line.split('\t').collect())
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

    for alias in ["a", "-a"] {
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
fn help_shows_short_attach_alias() -> TestResult {
    let env = TestEnv::new()?;
    let output = env.cmd().arg("--help").output()?;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[aliases: a]"),
        "attach alias was not visible in help:\n{stdout}"
    );
    Ok(())
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
    assert!(stdout.contains("data/shims/tmux"), "{stdout:?}");
    assert!(stdout.contains("%0"), "{stdout:?}");
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
            "trap '' HUP; sleep 3 & echo LEADER_DONE",
        ])
        .status()?;
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        let output = env.cmd().arg("list").output()?;
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.lines().any(|line| line.starts_with("leader-reap	")) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("session leader exited but lterm kept the pane until background PTY holder exited".into())
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

// ──────────────────────────────────────────────────────────────────────────
// PR #15: server-side per-client geometry + clamp-to-smallest 통합 테스트.
//
// `lterm attach` 는 raw TTY 가 필요해 일반 subprocess 로는 띄울 수 없으므로,
// 라이브러리 의존 없이 daemon 의 Unix socket 에 직접 JSON 프로토콜로 attach 한다.
// 두 attach 를 다른 geometry 로 등록한 뒤 `lterm list --json` 으로 PTY rows/cols
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

/// `lterm list --json` 으로 단일 세션의 (rows, cols) 를 조회한다.
#[cfg(unix)]
fn read_session_size(env: &TestEnv, name: &str) -> TestResult<(u16, u16)> {
    let output = env.cmd().args(["list", "--json"]).output()?;
    if !output.status.success() {
        return Err(format!("lterm list --json failed: {output:?}").into());
    }
    let sessions: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    let session = sessions
        .iter()
        .find(|s| s.get("name").and_then(|v| v.as_str()) == Some(name))
        .ok_or_else(|| format!("session {name} not in list"))?;
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

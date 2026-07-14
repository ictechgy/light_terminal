#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const INTERNAL_TEST_LAUNCH_ARG: &str = "__lterm-internal-managed-launch-test-v1";
const INTERNAL_TEST_RECONCILE_ARG: &str = "__lterm-internal-managed-reconcile-test-v1";

fn private_temp() -> TempDir {
    let temp = TempDir::new().expect("temporary data directory");
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
        .expect("private temporary data directory");
    temp
}

fn shell_executable() -> std::path::PathBuf {
    fs::canonicalize("/bin/sh").expect("canonical shell executable")
}

fn slot_path(temp: &TempDir) -> std::path::PathBuf {
    temp.path()
        .join("speculation/process-registry-v1/slots/slot-0000.json")
}

fn slot_state(path: &std::path::Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("state")?
        .as_str()
        .map(str::to_owned)
}

fn wait_for_slot_state(path: &std::path::Path, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while slot_state(path).as_deref() != Some(expected) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(slot_state(path).as_deref(), Some(expected));
}

fn reconcile_until_tombstone(temp: &TempDir) {
    let slot = slot_path(temp);
    let deadline = Instant::now() + Duration::from_secs(10);
    while slot_state(&slot).as_deref() != Some("resolved_tombstone") && Instant::now() < deadline {
        let output = Command::new(env!("CARGO_BIN_EXE_lterm"))
            .arg(INTERNAL_TEST_RECONCILE_ARG)
            .env("LTERM_INTERNAL_TEST_MODE", "1")
            .env("LTERM_DATA_DIR", temp.path())
            .output()
            .expect("run restart reconciliation");
        assert!(
            output.status.success(),
            "reconcile driver failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(slot_state(&slot).as_deref(), Some("resolved_tombstone"));
}

fn run_managed_launch_with_layout(close_stdin: bool) {
    let temp = private_temp();
    let marker = temp.path().join("target-ran");
    let slot = slot_path(&temp);
    let script = format!(
        "grep -q '\"state\":\"identity_durable\"' '{}' && printf executed > '{}'",
        slot.display(),
        marker.display()
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_lterm"));
    command
        .arg(INTERNAL_TEST_LAUNCH_ARG)
        .arg(shell_executable())
        .arg("-c")
        .arg(script)
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_DATA_DIR", temp.path());
    if close_stdin {
        unsafe {
            command.pre_exec(|| {
                libc::close(libc::STDIN_FILENO);
                Ok(())
            });
        }
    }
    let output = command.output().expect("run managed-launch test driver");
    assert!(
        output.status.success(),
        "managed launch failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(fs::read_to_string(marker).unwrap(), "executed");
    assert_eq!(slot_state(&slot).as_deref(), Some("resolved_tombstone"));
}

#[test]
fn managed_launch_executes_with_ordinary_descriptor_layout() {
    run_managed_launch_with_layout(false);
}

#[test]
fn managed_launch_executes_when_sources_collide_with_reserved_descriptors() {
    run_managed_launch_with_layout(true);
}

#[test]
fn failed_exec_is_observed_and_durably_tombstoned() {
    let temp = private_temp();
    let invalid = temp.path().join("invalid-executable");
    fs::write(&invalid, b"not an executable format").unwrap();
    fs::set_permissions(&invalid, fs::Permissions::from_mode(0o700)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lterm"))
        .arg(INTERNAL_TEST_LAUNCH_ARG)
        .arg(&invalid)
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_DATA_DIR", temp.path())
        .output()
        .expect("run failed-exec launch driver");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("managed target exec failed"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        slot_state(&slot_path(&temp)).as_deref(),
        Some("resolved_tombstone")
    );
}

#[test]
fn missing_executable_tombstones_pre_spawn_intent() {
    let temp = private_temp();
    let output = Command::new(env!("CARGO_BIN_EXE_lterm"))
        .arg(INTERNAL_TEST_LAUNCH_ARG)
        .arg(temp.path().join("missing-executable"))
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_DATA_DIR", temp.path())
        .output()
        .expect("run missing-executable launch driver");
    assert!(!output.status.success());
    assert_eq!(
        slot_state(&slot_path(&temp)).as_deref(),
        Some("resolved_tombstone")
    );
}

#[test]
fn terminate_and_wait_reaps_root_and_tombstones() {
    let temp = private_temp();
    let output = Command::new(env!("CARGO_BIN_EXE_lterm"))
        .arg(INTERNAL_TEST_LAUNCH_ARG)
        .arg(shell_executable())
        .arg("-c")
        .arg("sleep 30")
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_INTERNAL_MANAGED_LAUNCH_TERMINATE", "1")
        .env("LTERM_DATA_DIR", temp.path())
        .output()
        .expect("run terminating launch driver");
    assert!(
        output.status.success(),
        "terminate driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        slot_state(&slot_path(&temp)).as_deref(),
        Some("resolved_tombstone")
    );
}

#[test]
fn restart_reconciliation_cleans_detached_root() {
    let temp = private_temp();
    let status = Command::new(env!("CARGO_BIN_EXE_lterm"))
        .arg(INTERNAL_TEST_LAUNCH_ARG)
        .arg(shell_executable())
        .arg("-c")
        .arg("sleep 30")
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_INTERNAL_MANAGED_LAUNCH_NO_WAIT", "1")
        .env("LTERM_DATA_DIR", temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("launch detached managed root");
    assert!(status.success());
    let slot = slot_path(&temp);
    wait_for_slot_state(&slot, "identity_durable");

    reconcile_until_tombstone(&temp);
}

#[test]
fn parent_and_gate_crash_boundaries_reconcile_without_early_target_execution() {
    let cases = [
        ("parent_before_intent", true),
        ("parent_after_intent", true),
        ("parent_after_spawn", true),
        ("parent_after_hello", true),
        ("parent_after_identity", true),
        ("parent_before_commit", true),
        ("parent_after_commit", false),
        ("gate_before_registration", true),
        ("gate_after_registration", true),
        ("gate_after_hello", true),
        ("gate_after_commit", true),
        ("gate_before_exec", true),
    ];

    for (failpoint, marker_must_be_absent) in cases {
        let temp = private_temp();
        let marker = temp.path().join("target-ran");
        let slot = slot_path(&temp);
        let script = format!(
            "grep -q '\"state\":\"identity_durable\"' '{}' && printf executed > '{}'",
            slot.display(),
            marker.display()
        );
        let status = Command::new(env!("CARGO_BIN_EXE_lterm"))
            .arg(INTERNAL_TEST_LAUNCH_ARG)
            .arg(shell_executable())
            .arg("-c")
            .arg(script)
            .env("LTERM_INTERNAL_TEST_MODE", "1")
            .env("LTERM_INTERNAL_MANAGED_FAILPOINT", failpoint)
            .env("LTERM_DATA_DIR", temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|err| panic!("run failpoint {failpoint}: {err}"));
        assert!(!status.success(), "failpoint {failpoint} did not terminate");

        if failpoint == "parent_before_intent" {
            assert_eq!(slot_state(&slot).as_deref(), Some("vacant"));
        } else {
            reconcile_until_tombstone(&temp);
        }
        if marker_must_be_absent {
            assert!(!marker.exists(), "target ran at failpoint {failpoint}");
        }
    }
}

#[test]
fn cleanup_crash_boundaries_resume_to_tombstone() {
    for failpoint in [
        "after_cleanup_pending",
        "after_cleanup_signal",
        "before_tombstone",
        "after_tombstone",
    ] {
        let temp = private_temp();
        let status = Command::new(env!("CARGO_BIN_EXE_lterm"))
            .arg(INTERNAL_TEST_LAUNCH_ARG)
            .arg(shell_executable())
            .arg("-c")
            .arg("sleep 30")
            .env("LTERM_INTERNAL_TEST_MODE", "1")
            .env("LTERM_INTERNAL_MANAGED_LAUNCH_NO_WAIT", "1")
            .env("LTERM_DATA_DIR", temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        wait_for_slot_state(&slot_path(&temp), "identity_durable");

        let mut injected = false;
        for _ in 0..5 {
            let status = Command::new(env!("CARGO_BIN_EXE_lterm"))
                .arg(INTERNAL_TEST_RECONCILE_ARG)
                .env("LTERM_INTERNAL_TEST_MODE", "1")
                .env("LTERM_INTERNAL_MANAGED_FAILPOINT", failpoint)
                .env("LTERM_DATA_DIR", temp.path())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            if !status.success() {
                injected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(injected, "cleanup failpoint {failpoint} was not reached");
        reconcile_until_tombstone(&temp);
    }
}
<<<<<<< HEAD
=======

#[test]
fn concurrent_reconcilers_fail_closed_and_converge() {
    let temp = private_temp();
    let status = Command::new(env!("CARGO_BIN_EXE_lterm"))
        .arg(INTERNAL_TEST_LAUNCH_ARG)
        .arg(shell_executable())
        .arg("-c")
        .arg("sleep 30")
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_INTERNAL_MANAGED_LAUNCH_NO_WAIT", "1")
        .env("LTERM_DATA_DIR", temp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    wait_for_slot_state(&slot_path(&temp), "identity_durable");

    let spawn_reconciler = || {
        Command::new(env!("CARGO_BIN_EXE_lterm"))
            .arg(INTERNAL_TEST_RECONCILE_ARG)
            .env("LTERM_INTERNAL_TEST_MODE", "1")
            .env("LTERM_DATA_DIR", temp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn concurrent reconciler")
    };
    let mut first = spawn_reconciler();
    let mut second = spawn_reconciler();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    reconcile_until_tombstone(&temp);
}
>>>>>>> 6b51e05

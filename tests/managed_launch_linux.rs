#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const INTERNAL_TEST_LAUNCH_ARG: &str = "__lterm-internal-managed-launch-test-v1";

fn run_managed_launch_with_layout(close_stdin: bool) {
    let temp = TempDir::new().expect("private temporary data directory");
    let marker = temp.path().join("target-ran");
    let slot = temp
        .path()
        .join("speculation/process-registry-v1/slots/slot-0000.json");
    let script = format!(
        "grep -q '\"state\":\"identity_durable\"' '{}' && printf executed > '{}'",
        slot.display(),
        marker.display()
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_lterm"));
    command
        .arg(INTERNAL_TEST_LAUNCH_ARG)
        .arg("/bin/sh")
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
}

#[test]
fn managed_launch_executes_with_ordinary_descriptor_layout() {
    run_managed_launch_with_layout(false);
}

#[test]
fn managed_launch_executes_when_sources_collide_with_reserved_descriptors() {
    run_managed_launch_with_layout(true);
}

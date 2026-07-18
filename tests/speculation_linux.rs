#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

fn run_required_component(failpoint: Option<&str>) -> Option<(tempfile::TempDir, Output)> {
    if std::env::var_os("LTERM_REQUIRE_REAL_BWRAP").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return None;
    }
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .expect("required real run needs LTERM_SPECULATION_CGROUP_ROOT");
    assert_eq!(
        fs::canonicalize("/usr/bin/bwrap").expect("exact /usr/bin/bwrap"),
        std::path::Path::new("/usr/bin/bwrap")
    );
    let fixture = tempfile::TempDir::new().expect("private speculation fixture");
    fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o700))
        .expect("private fixture mode");
    let data = fixture.path().join("data");
    fs::create_dir(&data).expect("private managed-launch data");
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).expect("private data mode");
    let component = fixture.path();
    let self_exe = fixture.path().join("lterm-test-bin");
    fs::copy(env!("CARGO_BIN_EXE_lterm"), &self_exe).expect("copy retained test executable");
    fs::set_permissions(&self_exe, fs::Permissions::from_mode(0o500))
        .expect("retained test executable mode");
    let mut command = Command::new(env!("CARGO_BIN_EXE_lterm"));
    command
        .arg("--internal-speculation-containment-test-v1")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_INTERNAL_SPECULATION_SELF_EXE", &self_exe)
        .env("LTERM_DATA_DIR", &data)
        .env("LTERM_REQUIRE_REAL_BWRAP", "1")
        .env("LTERM_SPECULATION_CGROUP_ROOT", cgroup_root)
        .env("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT", &component);
    if let Some(failpoint) = failpoint {
        command.env("LTERM_INTERNAL_SPECULATION_FAILPOINT", failpoint);
    }
    let output = command
        .output()
        .expect("run real speculation component driver");
    Some((fixture, output))
}

fn assert_no_tournament_domains() {
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .expect("required real run needs LTERM_SPECULATION_CGROUP_ROOT");
    let leaked = fs::read_dir(cgroup_root)
        .expect("enumerate delegated root")
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("lterm-g003-"))
                .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok())
        });
    assert!(!leaked, "speculation component leaked a tournament domain");
}

#[test]
fn required_real_bwrap_cgroup_component_path_executes_positive_case() {
    let Some((_fixture, output)) = run_required_component(None) else {
        return;
    };
    assert!(
        output.status.success(),
        "component driver failed with bounded stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"speculation-real-cases=4\n");
    assert!(output.stderr.is_empty());
    assert_no_tournament_domains();
}

#[test]
fn required_real_failpoint_is_bounded_and_leaves_no_tournament_domain() {
    let Some((_fixture, output)) = run_required_component(Some("before_tournament_create")) else {
        return;
    };
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: speculation_containment_evidence_unavailable\n"
    );
    assert_no_tournament_domains();
}

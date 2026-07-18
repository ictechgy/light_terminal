#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn required_real_bwrap_cgroup_component_path_executes_positive_case() {
    if std::env::var_os("LTERM_REQUIRE_REAL_BWRAP").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
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
    let component = fixture.path().join("component");
    fs::create_dir(&component).expect("component root");
    fs::set_permissions(&component, fs::Permissions::from_mode(0o700)).expect("component mode");
    let output = Command::new(env!("CARGO_BIN_EXE_lterm"))
        .arg("--internal-speculation-containment-test-v1")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_DATA_DIR", &data)
        .env("LTERM_REQUIRE_REAL_BWRAP", "1")
        .env("LTERM_SPECULATION_CGROUP_ROOT", cgroup_root)
        .env("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT", &component)
        .output()
        .expect("run real speculation component driver");
    assert!(
        output.status.success(),
        "component driver failed with bounded stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"speculation-real-cases=1\n");
}

#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};
use uuid::Uuid;

const INTERNAL_MANAGED_TEST_ARG: &str = "__lterm-internal-managed-launch-test-v1";

fn bind_seqpacket_test_listener(path: &std::path::Path) -> std::fs::File {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    assert!(fd >= 0, "create hostile seqpacket listener");
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    assert!(bytes.len() < address.sun_path.len());
    for (target, source) in address.sun_path.iter_mut().zip(bytes) {
        *target = *source as libc::c_char;
    }
    let length = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    assert_eq!(
        unsafe {
            libc::bind(
                file.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                length as libc::socklen_t,
            )
        },
        0,
        "bind hostile seqpacket listener"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    file
}

fn run_required_component_case(
    failpoint: Option<&str>,
    endpoint_only: bool,
) -> Option<(tempfile::TempDir, Output, bool)> {
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
        .env("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT", component);
    if let Some(failpoint) = failpoint {
        command.env("LTERM_INTERNAL_SPECULATION_FAILPOINT", failpoint);
    }
    if endpoint_only {
        command.env("LTERM_INTERNAL_SPECULATION_ENDPOINT_ONLY", "1");
    }
    let output = command
        .output()
        .expect("run real speculation component driver");
    let leaked_before_cleanup = cleanup_tournament_domains_before_assertions();
    Some((fixture, output, leaked_before_cleanup))
}

fn run_required_component(failpoint: Option<&str>) -> Option<(tempfile::TempDir, Output, bool)> {
    run_required_component_case(failpoint, false)
}

fn run_required_actor_service(terminal: &str) -> Option<(tempfile::TempDir, Output, bool)> {
    run_required_actor_service_case_with_lease(terminal, None, None)
}

fn run_required_actor_service_case(
    terminal: &str,
    prepare_failpoint: Option<&str>,
) -> Option<(tempfile::TempDir, Output, bool)> {
    run_required_actor_service_case_with_lease(terminal, prepare_failpoint, None)
}

fn run_required_actor_service_case_with_lease(
    terminal: &str,
    prepare_failpoint: Option<&str>,
    observed_run_timeout_ms: Option<u64>,
) -> Option<(tempfile::TempDir, Output, bool)> {
    if std::env::var_os("LTERM_REQUIRE_REAL_BWRAP").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return None;
    }
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .expect("required actor run needs LTERM_SPECULATION_CGROUP_ROOT");
    let fixture = tempfile::TempDir::new().expect("private actor fixture");
    fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o700))
        .expect("private actor fixture mode");
    let data = fixture.path().join("data");
    fs::create_dir(&data).expect("private actor managed-launch data");
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).expect("private actor data mode");
    let self_exe = fixture.path().join("lterm-test-bin");
    fs::copy(env!("CARGO_BIN_EXE_lterm"), &self_exe).expect("copy retained actor executable");
    fs::set_permissions(&self_exe, fs::Permissions::from_mode(0o500))
        .expect("retained actor executable mode");
    let mut command = Command::new(env!("CARGO_BIN_EXE_lterm"));
    command
        .arg("--internal-speculation-containment-test-v1")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_INTERNAL_SPECULATION_ACTOR_SERVICE", "1")
        .env("LTERM_INTERNAL_SPECULATION_ACTOR_TERMINAL", terminal)
        .env("LTERM_INTERNAL_SPECULATION_SELF_EXE", &self_exe)
        .env("LTERM_DATA_DIR", &data)
        .env("LTERM_REQUIRE_REAL_BWRAP", "1")
        .env("LTERM_SPECULATION_CGROUP_ROOT", cgroup_root)
        .env("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT", fixture.path());
    if let Some(failpoint) = prepare_failpoint {
        command.env("LTERM_INTERNAL_SPECULATION_PREPARE_FAILPOINT", failpoint);
    }
    if let Some(timeout_ms) = observed_run_timeout_ms {
        command.env(
            "LTERM_INTERNAL_SPECULATION_OBSERVE_LEASE_MS",
            timeout_ms.to_string(),
        );
    }
    let output = command.output().expect("run real actor service driver");
    let leaked_before_cleanup = cleanup_tournament_domains_before_assertions();
    Some((fixture, output, leaked_before_cleanup))
}

#[test]
fn production_startup_config_is_captured_before_parallel_recovery_threads() {
    let fixtures = (0..4)
        .map(|_| tempfile::TempDir::new().expect("startup config fixture"))
        .collect::<Vec<_>>();
    let children = fixtures
        .iter()
        .map(|fixture| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_lterm"));
            command
                .arg("--internal-speculation-startup-config-test-v1")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("LTERM_INTERNAL_TEST_MODE", "1")
                .env("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT", fixture.path())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            command
                .spawn()
                .expect("spawn startup config initialization probe")
        })
        .collect::<Vec<_>>();
    for child in children {
        let output = child
            .wait_with_output()
            .expect("wait for startup config initialization probe");
        assert!(
            output.status.success(),
            "startup config initialization probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"speculation-startup-config-captured=1\n");
        assert!(output.stderr.is_empty());
    }
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

fn cleanup_tournament_domains_before_assertions() -> bool {
    let Some(cgroup_root) = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT") else {
        return false;
    };
    let root = std::path::PathBuf::from(cgroup_root);
    let Ok(entries) = fs::read_dir(&root) else {
        return false;
    };
    let domains = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let suffix = name.to_str()?.strip_prefix("lterm-g003-")?;
            Uuid::parse_str(suffix).ok()?;
            Some(entry.path())
        })
        .collect::<Vec<_>>();
    for domain in &domains {
        let _ = fs::write(domain.join("cgroup.kill"), b"1\n");
        for _ in 0..500 {
            let populated = fs::read_to_string(domain.join("cgroup.events"))
                .ok()
                .and_then(|events| {
                    events.lines().find_map(|line| {
                        line.strip_prefix("populated ")
                            .and_then(|value| value.parse::<u8>().ok())
                    })
                })
                .unwrap_or(0);
            if populated == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for relative in [
            "candidate-0/control",
            "candidate-0/payload",
            "candidate-1/control",
            "candidate-1/payload",
            "candidate-0",
            "candidate-1",
        ] {
            let _ = fs::remove_dir(domain.join(relative));
        }
        let _ = fs::remove_dir(domain);
    }
    !domains.is_empty()
}

fn assert_bounded_raw_free_failure(output: &Output, fixture: &tempfile::TempDir, seam: &str) {
    assert!(!output.status.success(), "{seam} unexpectedly succeeded");
    assert!(output.stdout.is_empty(), "{seam} emitted stdout");
    assert!(output.stderr.len() <= 512, "{seam} stderr exceeded bound");
    let stderr = String::from_utf8(output.stderr.clone()).expect("bounded UTF-8 stderr");
    assert!(
        stderr
            .lines()
            .all(|line| line.starts_with("error: speculation_")),
        "{seam} emitted an unbounded error category: {stderr:?}"
    );
    assert!(
        !stderr.contains(&fixture.path().to_string_lossy().into_owned()),
        "{seam} exposed its fixture path"
    );
}

fn assert_candidate_workspaces_empty(fixture: &tempfile::TempDir, seam: &str) {
    for candidate in ["candidate-0", "candidate-1"] {
        let path = fixture.path().join(candidate);
        if path.exists() {
            assert!(
                fs::read_dir(path)
                    .expect("inspect restart candidate workspace")
                    .next()
                    .is_none(),
                "{seam} mutated {candidate}"
            );
        }
    }
}

fn assert_scripted_execution_workspaces_unchanged(fixture: &tempfile::TempDir, seam: &str) {
    for candidate in ["candidate-0", "candidate-1"] {
        let path = fixture.path().join("e").join(candidate);
        if path.exists() {
            let names = fs::read_dir(path)
                .expect("inspect candidate after failpoint")
                .map(|entry| entry.expect("candidate entry").file_name())
                .collect::<Vec<_>>();
            assert_eq!(
                names,
                [std::ffi::OsString::from("run.sh")],
                "{seam} unexpectedly mutated the candidate workspace"
            );
        }
    }
}

fn tournament_domain(tournament_uuid: Uuid) -> std::path::PathBuf {
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .expect("required restart run needs LTERM_SPECULATION_CGROUP_ROOT");
    std::path::PathBuf::from(cgroup_root).join(format!("lterm-g003-{tournament_uuid}"))
}

fn move_process_to_cgroup(child: &Child, cgroup: &std::path::Path) {
    fs::OpenOptions::new()
        .write(true)
        .open(cgroup.join("cgroup.procs"))
        .expect("open attack cgroup.procs")
        .write_all(format!("{}\n", child.id()).as_bytes())
        .expect("move attack process into descendant cgroup");
}

fn write_cgroup_leaf(cgroup: &std::path::Path, leaf: &str, bytes: &[u8]) {
    fs::OpenOptions::new()
        .write(true)
        .open(cgroup.join(leaf))
        .expect("open attack cgroup leaf")
        .write_all(bytes)
        .expect("write attack cgroup leaf");
}

fn reap_attack_process(child: &mut Child) {
    if child.try_wait().expect("inspect attack process").is_none() {
        child.kill().expect("kill surviving attack process");
    }
    child.wait().expect("reap attack process");
}

fn required_restart_fixture() -> Option<tempfile::TempDir> {
    if std::env::var_os("LTERM_REQUIRE_REAL_BWRAP").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return None;
    }
    let fixture = tempfile::TempDir::new().expect("private restart fixture");
    fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o700))
        .expect("private restart fixture mode");
    let data = fixture.path().join("data");
    fs::create_dir(&data).expect("private restart data");
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700))
        .expect("private restart data mode");
    let self_exe = fixture.path().join("lterm-test-bin");
    fs::copy(env!("CARGO_BIN_EXE_lterm"), &self_exe).expect("copy restart test executable");
    fs::set_permissions(&self_exe, fs::Permissions::from_mode(0o500))
        .expect("restart test executable mode");
    Some(fixture)
}

fn run_restart_component(
    fixture: &tempfile::TempDir,
    mode: &str,
    tournament_uuid: Uuid,
    failpoint: Option<&str>,
    candidate: u8,
) -> Output {
    restart_component_command(fixture, mode, tournament_uuid, failpoint, candidate)
        .output()
        .expect("run restart component driver")
}

fn restart_component_command(
    fixture: &tempfile::TempDir,
    mode: &str,
    tournament_uuid: Uuid,
    failpoint: Option<&str>,
    candidate: u8,
) -> Command {
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .expect("required restart run needs LTERM_SPECULATION_CGROUP_ROOT");
    let mut command = Command::new(env!("CARGO_BIN_EXE_lterm"));
    command
        .arg("--internal-speculation-containment-test-v1")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env(
            "LTERM_INTERNAL_SPECULATION_SELF_EXE",
            fixture.path().join("lterm-test-bin"),
        )
        .env("LTERM_DATA_DIR", fixture.path().join("data"))
        .env("LTERM_REQUIRE_REAL_BWRAP", "1")
        .env("LTERM_SPECULATION_CGROUP_ROOT", cgroup_root)
        .env("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT", fixture.path())
        .env("LTERM_INTERNAL_SPECULATION_RESTART_MODE", mode)
        .env(
            "LTERM_INTERNAL_SPECULATION_TOURNAMENT_UUID",
            tournament_uuid.to_string(),
        )
        .env(
            "LTERM_INTERNAL_SPECULATION_CREATE_CANDIDATE",
            candidate.to_string(),
        );
    if let Some(failpoint) = failpoint {
        command
            .env("LTERM_INTERNAL_SPECULATION_FAILPOINT", failpoint)
            .env("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION", "exit");
    }
    command
}

fn run_delayed_runner_deadline_case(
    fixture: &tempfile::TempDir,
    tournament_uuid: Uuid,
    force_pending_reap: bool,
    force_reaper_spawn_failure: bool,
) -> Output {
    let mut command = restart_component_command(
        fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_control_unlink"),
        0,
    );
    command
        .env_remove("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION")
        .env("LTERM_INTERNAL_SPECULATION_DELAY_RUNNER_EXEC_SECONDS", "6")
        .env(
            "LTERM_INTERNAL_SPECULATION_OBSERVE_FAILED_RUNNER_LIFETIME",
            "1",
        )
        .env("LTERM_INTERNAL_MANAGED_FIRST_CLEANUP_UNKNOWN", "1");
    if force_pending_reap {
        command.env("LTERM_INTERNAL_MANAGED_FORCE_PENDING_REAP", "1");
    }
    if force_reaper_spawn_failure {
        assert!(
            force_pending_reap,
            "reaper spawn-failure seam requires forced Pending"
        );
        command.env("LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE", "1");
    }
    command
        .output()
        .expect("run delayed runner deadline component driver")
}

fn run_post_ready_failure_case(fixture: &tempfile::TempDir, tournament_uuid: Uuid) -> Output {
    let mut command = restart_component_command(
        fixture,
        "crash-runtime",
        tournament_uuid,
        Some("after_payload_fd_send"),
        0,
    );
    command
        .env_remove("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION")
        .env(
            "LTERM_INTERNAL_SPECULATION_OBSERVE_FAILED_RUNNER_LIFETIME",
            "1",
        )
        .env("LTERM_INTERNAL_MANAGED_FORCE_PENDING_REAP", "1")
        .env("LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE", "1");
    command.output().expect("run post-READY failure driver")
}

fn run_failed_launch_settlement_case(fixture: &tempfile::TempDir, tournament_uuid: Uuid) -> Output {
    let mut command = restart_component_command(
        fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_control_unlink"),
        0,
    );
    command
        .env_remove("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION")
        .env(
            "LTERM_INTERNAL_SPECULATION_OBSERVE_FAILED_RUNNER_LIFETIME",
            "1",
        )
        .env("LTERM_INTERNAL_MANAGED_RETURN_ERROR", "parent_after_spawn")
        .env("LTERM_INTERNAL_MANAGED_FORCE_PENDING_REAP", "1")
        .env("LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE", "1");
    command
        .output()
        .expect("run unresolved failed-launch settlement driver")
}

fn run_resolved_launch_error_case(fixture: &tempfile::TempDir, tournament_uuid: Uuid) -> Output {
    let mut command = restart_component_command(
        fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_control_unlink"),
        0,
    );
    command
        .env_remove("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION")
        .env("LTERM_INTERNAL_MANAGED_RETURN_ERROR", "parent_after_spawn");
    command
        .output()
        .expect("run resolved failed-launch settlement driver")
}

fn run_prelaunch_returned_error_case(fixture: &tempfile::TempDir, tournament_uuid: Uuid) -> Output {
    let mut command = restart_component_command(
        fixture,
        "crash-runtime",
        tournament_uuid,
        Some("after_private_control_binding"),
        0,
    );
    command.env_remove("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION");
    command
        .output()
        .expect("run returned pre-managed-launch error driver")
}

fn retained_private_control(
    fixture: &tempfile::TempDir,
    tournament_uuid: Uuid,
) -> std::path::PathBuf {
    fixture
        .path()
        .join("control")
        .join(format!("lterm-g003-{tournament_uuid}-candidate-0"))
}

fn managed_artifact_bindings(fixture: &tempfile::TempDir) -> Vec<serde_json::Value> {
    let slots = fixture
        .path()
        .join("data/speculation/process-registry-v1/slots");
    let entries = match fs::read_dir(slots) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("enumerate managed registry slots: {error}"),
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            serde_json::from_slice::<serde_json::Value>(
                &fs::read(entry.path()).expect("read managed slot"),
            )
            .expect("parse managed slot")
        })
        .filter_map(|record| record.get("artifact_binding").cloned())
        .collect()
}

fn bound_private_control(
    fixture: &tempfile::TempDir,
    binding: &serde_json::Value,
) -> std::path::PathBuf {
    let leaf = binding
        .get("cleanup_quarantine")
        .or_else(|| binding.get("private_leaf"))
        .and_then(|value| value.as_str())
        .expect("managed private-control leaf");
    fixture.path().join("control").join(leaf)
}

fn rewrite_managed_binding_boot_uuid(fixture: &tempfile::TempDir, boot_uuid: Uuid) {
    let slots = fixture
        .path()
        .join("data")
        .join("speculation")
        .join("process-registry-v1")
        .join("slots");
    for entry in fs::read_dir(slots).unwrap() {
        let path = entry.unwrap().path();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let Some(binding) = value.get_mut("artifact_binding") else {
            continue;
        };
        if binding.is_null() {
            continue;
        }
        binding["control_root"]["boot_uuid"] = serde_json::Value::String(boot_uuid.to_string());
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
}

fn assert_delayed_runner_output_is_closed(failed: &Output, failed_elapsed: Duration) {
    assert_eq!(failed.status.code(), Some(1));
    assert!(
        failed_elapsed < Duration::from_secs(15),
        "failed runner cleanup exceeded its bound: {failed_elapsed:?}"
    );
    assert!(failed.stdout.is_empty());
    assert_eq!(failed.stderr, b"error: speculation_containment_timeout\n");
    assert_no_managed_output_markers(failed);
}

fn assert_no_managed_output_markers(output: &Output) {
    assert!(
        !output
            .stdout
            .windows(b"lterm-managed-stdout-marker".len())
            .any(|window| window == b"lterm-managed-stdout-marker")
    );
    assert!(
        !output
            .stderr
            .windows(b"lterm-managed-stderr-marker".len())
            .any(|window| window == b"lterm-managed-stderr-marker")
    );
}

fn required_component_stage(fixture: &tempfile::TempDir) -> &'static str {
    for (directory, stage) in [
        ("q", "pids-exhaustion"),
        ("m", "migration"),
        ("d", "detached-descendants"),
        ("f", "fork-storm"),
        ("i", "stream-overflow"),
        ("o", "bounded-overflow"),
        ("x", "output-boundary"),
        ("e", "timing-score"),
        ("p", "peer-attacks"),
    ] {
        if fixture.path().join(directory).is_dir() {
            return stage;
        }
    }
    "topology-attacks"
}

#[test]
fn required_real_bwrap_cgroup_component_path_executes_positive_case() {
    let Some((fixture, output, leaked_before_cleanup)) = run_required_component(None) else {
        return;
    };
    assert!(
        output.status.success(),
        "component driver failed at {} with bounded stderr: {}",
        required_component_stage(&fixture),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"speculation-real-cases=14\n");
    assert!(
        output.stderr.is_empty(),
        "positive component emitted stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !leaked_before_cleanup,
        "positive component leaked a tournament domain before test cleanup"
    );
    assert_no_tournament_domains();
}

#[test]
fn required_real_exact_control_endpoint_retires_inert_after_handshake() {
    let Some((fixture, output, leaked_before_cleanup)) = run_required_component_case(None, true)
    else {
        return;
    };
    assert!(
        output.status.success(),
        "endpoint-only fixed probe failed at {} with bounded stderr: {}",
        required_component_stage(&fixture),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"speculation-endpoint-probes=2\n");
    assert!(
        output.stderr.is_empty(),
        "endpoint-only fixed probe emitted stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !leaked_before_cleanup,
        "endpoint-only fixed probe leaked a tournament domain before test cleanup"
    );
    assert_no_tournament_domains();
}

#[test]
fn required_real_delayed_runner_deadline_reaps_synchronously_before_private_control_unlink() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let started = Instant::now();
    let failed = run_delayed_runner_deadline_case(&fixture, tournament_uuid, false, false);
    let failed_elapsed = started.elapsed();
    let lifetime_marker = fs::read(
        fixture
            .path()
            .join("control/failed-runner-reaped-before-private-control-drop"),
    );
    let first_cleanup_unknown = fs::read(
        fixture
            .path()
            .join("control/managed-first-cleanup-unknown-orphan-risk"),
    );
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);

    assert_delayed_runner_output_is_closed(&failed, failed_elapsed);
    assert_eq!(
        lifetime_marker.expect("synchronous failed-runner lifetime evidence"),
        b"1\n"
    );
    assert_eq!(
        first_cleanup_unknown.expect("first cleanup UnknownOrphanRisk evidence"),
        b"1\n"
    );
    assert!(
        recovered.status.success(),
        "delayed runner recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert_candidate_workspaces_empty(&fixture, "delayed-runner-deadline");
    assert_no_tournament_domains();
}

#[test]
fn required_real_forced_pending_runner_handoff_reaps_before_private_control_unlink() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let started = Instant::now();
    let failed = run_delayed_runner_deadline_case(&fixture, tournament_uuid, true, false);
    let failed_elapsed = started.elapsed();
    let lifetime_marker = fs::read(
        fixture
            .path()
            .join("control/failed-runner-reaped-before-private-control-drop"),
    );
    let first_cleanup_unknown = fs::read(
        fixture
            .path()
            .join("control/managed-first-cleanup-unknown-orphan-risk"),
    );
    let pending_handoff = fs::read(
        fixture
            .path()
            .join("control/failed-runner-pending-reaper-retained-private-control"),
    );
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);

    assert_delayed_runner_output_is_closed(&failed, failed_elapsed);
    assert_eq!(
        lifetime_marker.expect("synchronous failed-runner lifetime evidence"),
        b"1\n"
    );
    assert_eq!(
        first_cleanup_unknown.expect("first cleanup UnknownOrphanRisk evidence"),
        b"1\n"
    );
    assert_eq!(
        pending_handoff.expect("forced-Pending reaper handoff evidence"),
        b"1\n"
    );
    assert!(
        recovered.status.success(),
        "delayed runner recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert_candidate_workspaces_empty(&fixture, "forced-pending-runner-deadline");
    assert_no_tournament_domains();
}

#[test]
fn required_real_forced_pending_spawn_failure_retains_control_until_recovery() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let started = Instant::now();
    let failed = run_delayed_runner_deadline_case(&fixture, tournament_uuid, true, true);
    let failed_elapsed = started.elapsed();
    let pending_handoff = fs::read(
        fixture
            .path()
            .join("control/failed-runner-pending-reaper-retained-private-control"),
    );
    let retained_control = retained_private_control(&fixture, tournament_uuid);
    let retained_executable = retained_control.join("lterm").is_file();
    let retained_socket = retained_control.join("control.sock").exists();
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);

    assert_delayed_runner_output_is_closed(&failed, failed_elapsed);
    assert_eq!(
        pending_handoff.expect("spawn-failure handoff retained-control evidence"),
        b"1\n"
    );
    assert!(
        retained_executable,
        "spawn failure unlinked the runner early"
    );
    assert!(retained_socket, "spawn failure unlinked the socket early");
    assert!(
        recovered.status.success(),
        "spawn-failure recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert!(
        !retained_control.exists(),
        "recovery left the resolved private control behind"
    );
    assert_candidate_workspaces_empty(&fixture, "forced-reaper-spawn-failure");
    assert_no_tournament_domains();
}

#[test]
fn required_real_post_ready_failure_retains_waiter_and_control_until_recovery() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let failed = run_post_ready_failure_case(&fixture, tournament_uuid);
    let retained_control = retained_private_control(&fixture, tournament_uuid);
    let retained_executable = retained_control.join("lterm").is_file();
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);

    assert_bounded_raw_free_failure(&failed, &fixture, "post-READY payload FD failure");
    assert_no_managed_output_markers(&failed);
    assert!(
        retained_executable,
        "post-READY failure unlinked the runner before proven reap"
    );
    assert!(
        recovered.status.success(),
        "post-READY failure recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert!(
        !retained_control.exists(),
        "post-READY recovery left the resolved private control behind"
    );
    assert_candidate_workspaces_empty(&fixture, "post-READY failure");
    assert_no_tournament_domains();
}

#[test]
fn required_real_unresolved_failed_launch_retains_exact_child_and_control_until_recovery() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let failed = run_failed_launch_settlement_case(&fixture, tournament_uuid);
    let retained_control = retained_private_control(&fixture, tournament_uuid);
    let retained_executable = retained_control.join("lterm").is_file();
    let retained_socket = retained_control.join("control.sock").exists();
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);

    assert_bounded_raw_free_failure(&failed, &fixture, "unresolved failed-launch settlement");
    assert_no_managed_output_markers(&failed);
    assert!(
        retained_executable,
        "failed-launch settlement unlinked the runner without proven reap"
    );
    assert!(
        retained_socket,
        "failed-launch settlement unlinked the socket without proven reap"
    );
    assert!(
        recovered.status.success(),
        "failed-launch settlement recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert!(
        !retained_control.exists(),
        "failed-launch recovery left the resolved private control behind"
    );
    assert_candidate_workspaces_empty(&fixture, "failed-launch settlement");
    assert_no_tournament_domains();
}

#[test]
fn required_real_returned_launch_error_immediately_tombstones_and_acks_private_binding() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let failed = run_resolved_launch_error_case(&fixture, tournament_uuid);
    let retained_control = retained_private_control(&fixture, tournament_uuid);
    let control_absent_before_recovery = !retained_control.exists();
    let bindings_absent_before_recovery = managed_artifact_bindings(&fixture).is_empty();

    assert_bounded_raw_free_failure(&failed, &fixture, "resolved failed-launch settlement");
    assert_no_managed_output_markers(&failed);
    assert!(
        control_absent_before_recovery,
        "returned launch error released without immediate private-control cleanup"
    );
    assert!(
        bindings_absent_before_recovery,
        "returned launch error left its cleanup-pending artifact binding"
    );

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "resolved failed-launch tournament cleanup failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_candidate_workspaces_empty(&fixture, "resolved failed-launch settlement");
    assert_no_tournament_domains();
}

#[test]
fn required_real_returned_prelaunch_error_cleans_bound_directory_without_restart() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let failed = run_prelaunch_returned_error_case(&fixture, tournament_uuid);
    let control_absent = !retained_private_control(&fixture, tournament_uuid).exists();
    let bindings_absent = managed_artifact_bindings(&fixture).is_empty();

    assert_bounded_raw_free_failure(&failed, &fixture, "returned prelaunch error");
    assert_no_managed_output_markers(&failed);
    assert!(control_absent, "prelaunch error left its bound directory");
    assert!(bindings_absent, "prelaunch error left its artifact binding");

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "prelaunch tournament cleanup failed: {}; record={}",
        String::from_utf8_lossy(&recovered.stderr),
        fs::read_to_string(fixture.path().join("restart-record.json"))
            .unwrap_or_else(|error| format!("<unavailable: {error}>"))
    );
    assert_candidate_workspaces_empty(&fixture, "returned prelaunch error");
    assert_no_tournament_domains();
}

#[test]
fn required_real_prelaunch_crash_has_durable_owner_and_recovers_private_control() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    );
    let retained_control = retained_private_control(&fixture, tournament_uuid);
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "prelaunch crash stopped early: {}; retained={:?}",
        String::from_utf8_lossy(&crashed.stderr),
        fs::read_dir(&retained_control)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>(),
    );
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());
    assert!(retained_control.join("lterm").is_file());
    assert!(retained_control.join("control.sock").exists());
    assert!(retained_control.join("owner.json").is_file());
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(bindings.len(), 1, "created private binding was not durable");
    assert!(
        bindings[0].get("private_directory").is_some(),
        "created private binding omitted its directory identity"
    );

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "prelaunch crash recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert!(!retained_control.exists());
    assert!(
        managed_artifact_bindings(&fixture).is_empty(),
        "created private cleanup completed without binding acknowledgement"
    );
    assert_candidate_workspaces_empty(&fixture, "prelaunch crash recovery");
    assert_no_tournament_domains();
}

#[test]
fn required_real_unbound_private_artifact_publication_crashes_fail_closed() {
    let seams = [
        "after_private_runner_creation_intent",
        "after_private_socket_creation_intent",
        "before_private_socket_mode",
        "after_private_socket_publish",
        "after_private_owner_creation_intent",
        "before_private_owner_publish",
        "after_private_owner_publish",
    ];
    for seam in seams {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let tournament_uuid = Uuid::new_v4();
        let crashed =
            run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
        let control = retained_private_control(&fixture, tournament_uuid);
        assert_eq!(crashed.status.code(), Some(86), "{seam}");
        assert!(crashed.stdout.is_empty(), "{seam}");
        assert!(crashed.stderr.is_empty(), "{seam}");
        let names = fs::read_dir(&control)
            .expect("inspect private publication prefix")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if seam == "after_private_runner_creation_intent" {
            assert!(
                names.iter().all(|name| name != "lterm"),
                "runner intent crash unexpectedly created its executable"
            );
        } else {
            assert!(names.iter().any(|name| name == "lterm"), "{seam}");
        }
        let owner_path = control.join("owner.json");
        if owner_path.exists() {
            let owner: serde_json::Value = serde_json::from_slice(&fs::read(&owner_path).unwrap())
                .expect("published owner marker must always be complete JSON");
            assert_eq!(
                owner.get("schema_version").and_then(|v| v.as_u64()),
                Some(1)
            );
            assert_eq!(seam, "after_private_owner_publish");
        } else {
            assert_ne!(seam, "after_private_owner_publish");
        }
        if seam == "before_private_owner_publish" {
            assert!(
                names
                    .iter()
                    .any(|name| name.starts_with(".owner.json.create-")),
                "owner publish crash omitted its exact recoverable temp"
            );
        }
        if seam == "after_private_owner_creation_intent" {
            assert!(
                names
                    .iter()
                    .all(|name| !name.starts_with(".owner.json.create-")),
                "owner intent crash unexpectedly opened its temp"
            );
            assert!(
                !owner_path.exists(),
                "owner intent crash unexpectedly published its final owner"
            );
        }
        if seam == "before_private_socket_mode" {
            assert!(!control.join("control.sock").exists());
            assert!(names.iter().any(|name| name.starts_with(".sock-")));
        }
        if seam == "after_private_socket_creation_intent" {
            assert!(!control.join("control.sock").exists());
            assert!(
                names.iter().all(|name| !name.starts_with(".sock-")),
                "socket intent crash unexpectedly opened its temporary socket"
            );
        }
        if matches!(
            seam,
            "after_private_socket_publish"
                | "after_private_owner_creation_intent"
                | "before_private_owner_publish"
                | "after_private_owner_publish"
        ) {
            let metadata = fs::symlink_metadata(control.join("control.sock")).unwrap();
            assert!(std::os::unix::fs::FileTypeExt::is_socket(
                &metadata.file_type()
            ));
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        }

        let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
        assert_bounded_raw_free_failure(&rejected, &fixture, seam);
        let bindings = managed_artifact_bindings(&fixture);
        assert_eq!(bindings.len(), 1, "{seam} lost durable ownership");
        if seam == "after_private_runner_creation_intent" {
            assert_eq!(
                bindings[0]
                    .get("runner_create_pending")
                    .and_then(serde_json::Value::as_bool),
                Some(true),
                "{seam} omitted the durable runner creation intent"
            );
        }
        if matches!(
            seam,
            "after_private_socket_creation_intent"
                | "before_private_socket_mode"
                | "after_private_socket_publish"
        ) {
            assert_eq!(
                bindings[0]
                    .get("socket_create_pending")
                    .and_then(serde_json::Value::as_bool),
                Some(true),
                "{seam} omitted the durable socket creation intent"
            );
        }
        if matches!(
            seam,
            "after_private_owner_creation_intent"
                | "before_private_owner_publish"
                | "after_private_owner_publish"
        ) {
            assert_eq!(
                bindings[0]
                    .get("owner_create_pending")
                    .and_then(serde_json::Value::as_bool),
                Some(true),
                "{seam} omitted the durable owner creation intent"
            );
        }
        assert!(
            bound_private_control(&fixture, &bindings[0]).is_dir(),
            "{seam} deleted ambiguous private artifacts"
        );
        assert_candidate_workspaces_empty(&fixture, seam);
        cleanup_tournament_domains_before_assertions();
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_bound_socket_owner_creation_phase_is_restart_recoverable() {
    for seam in [
        "after_private_socket_binding",
        "after_private_artifact_binding",
    ] {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let tournament_uuid = Uuid::new_v4();
        let crashed =
            run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
        assert_eq!(crashed.status.code(), Some(86), "{seam}");
        assert!(crashed.stdout.is_empty(), "{seam}");
        assert!(crashed.stderr.is_empty(), "{seam}");
        let control = retained_private_control(&fixture, tournament_uuid);
        assert!(control.join("lterm").is_file(), "{seam}");
        assert!(control.join("control.sock").exists(), "{seam}");
        assert!(!control.join("owner.json").exists(), "{seam}");
        assert!(
            fs::read_dir(&control)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".owner.json.create-")),
            "{seam} unexpectedly entered owner publication"
        );
        let binding = managed_artifact_bindings(&fixture);
        assert_eq!(binding.len(), 1, "{seam}");
        assert!(binding[0].get("socket").is_some(), "{seam}");
        assert!(binding[0].get("owner").is_none(), "{seam}");
        assert!(
            !binding[0]
                .get("owner_create_pending")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            "{seam} entered owner creation before its explicit durable begin"
        );

        let recovered =
            run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
        assert!(
            recovered.status.success(),
            "{seam} recovery failed: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
        assert!(recovered.stderr.is_empty(), "{seam}");
        assert!(!control.exists(), "{seam}");
        assert!(managed_artifact_bindings(&fixture).is_empty(), "{seam}");
        assert_candidate_workspaces_empty(&fixture, seam);
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_pending_runner_and_socket_relocation_to_absence_preserves_without_ack() {
    for (seam, exact_leaf, prefix, pending_field) in [
        (
            "before_private_runner_binding",
            Some("lterm"),
            None,
            "runner_create_pending",
        ),
        (
            "before_private_socket_mode",
            None,
            Some(".sock-"),
            "socket_create_pending",
        ),
        (
            "after_private_socket_publish",
            Some("control.sock"),
            None,
            "socket_create_pending",
        ),
    ] {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let tournament_uuid = Uuid::new_v4();
        let crashed =
            run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
        assert_eq!(crashed.status.code(), Some(86), "{seam}");
        assert!(crashed.stdout.is_empty(), "{seam}");
        assert!(crashed.stderr.is_empty(), "{seam}");
        let control = retained_private_control(&fixture, tournament_uuid);
        let artifact = fs::read_dir(&control)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                let name = entry.file_name();
                exact_leaf.is_some_and(|leaf| name == std::ffi::OsStr::new(leaf))
                    || prefix.is_some_and(|prefix| name.to_string_lossy().starts_with(prefix))
            })
            .unwrap_or_else(|| panic!("{seam} omitted its unbound artifact"))
            .path();
        let retained = fixture.path().join(format!("relocated-{seam}"));
        fs::rename(&artifact, &retained).unwrap();
        assert!(!artifact.exists(), "{seam} original path still exists");
        assert!(
            retained.exists(),
            "{seam} relocation lost the genuine inode"
        );

        let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
        assert_bounded_raw_free_failure(&rejected, &fixture, seam);
        let bindings = managed_artifact_bindings(&fixture);
        assert_eq!(bindings.len(), 1, "{seam} acknowledged a pending binding");
        assert_eq!(
            bindings[0]
                .get(pending_field)
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "{seam} lost its durable creation intent"
        );
        assert!(
            bound_private_control(&fixture, &bindings[0]).is_dir(),
            "{seam} deleted the pending private control"
        );
        assert!(retained.exists(), "{seam} deleted the relocated inode");
        assert!(
            !artifact.exists(),
            "{seam} recreated or adopted the missing path"
        );
        assert_candidate_workspaces_empty(&fixture, seam);
        cleanup_tournament_domains_before_assertions();
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_unbound_partial_replacements_are_preserved_for_operator_resolution() {
    for (seam, prefix, socket) in [
        ("before_private_socket_mode", ".sock-", true),
        ("before_private_owner_publish", ".owner.json.create-", false),
        ("after_private_owner_publish", "owner.json", false),
    ] {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let tournament_uuid = Uuid::new_v4();
        let crashed =
            run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
        assert_eq!(crashed.status.code(), Some(86), "{seam}");
        assert!(crashed.stdout.is_empty(), "{seam}");
        assert!(crashed.stderr.is_empty(), "{seam}");
        let control = retained_private_control(&fixture, tournament_uuid);
        let partial = fs::read_dir(&control)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
            .expect("crash left its exact partial artifact")
            .path();
        let retained = fixture.path().join(format!("retained-{seam}"));
        fs::rename(&partial, &retained).unwrap();
        let replacement_listener = if socket {
            Some(bind_seqpacket_test_listener(&partial))
        } else {
            let bytes = fs::read(&retained).unwrap();
            fs::write(&partial, bytes).unwrap();
            fs::set_permissions(&partial, fs::Permissions::from_mode(0o600)).unwrap();
            None
        };

        let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
        assert_bounded_raw_free_failure(&rejected, &fixture, seam);
        let bindings = managed_artifact_bindings(&fixture);
        assert_eq!(bindings.len(), 1, "{seam}");
        if seam == "before_private_socket_mode" {
            assert_eq!(
                bindings[0]
                    .get("socket_create_pending")
                    .and_then(serde_json::Value::as_bool),
                Some(true),
                "{seam} lost its durable socket creation intent"
            );
        }
        if seam != "before_private_socket_mode" {
            assert_eq!(
                bindings[0]
                    .get("owner_create_pending")
                    .and_then(serde_json::Value::as_bool),
                Some(true),
                "{seam} lost its durable owner creation intent"
            );
        }
        let quarantined = bound_private_control(&fixture, &bindings[0]);
        let replacement = quarantined.join(partial.file_name().unwrap());
        assert!(retained.exists(), "{seam} deleted the relocated original");
        assert!(replacement.exists(), "{seam} deleted the replacement");
        drop(replacement_listener);
        assert_candidate_workspaces_empty(&fixture, seam);
        cleanup_tournament_domains_before_assertions();
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_bound_owner_inode_rejects_an_exact_content_replacement() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let seam = "after_private_owner_binding";
    let crashed = run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
    assert_eq!(crashed.status.code(), Some(86));
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());
    let control = retained_private_control(&fixture, tournament_uuid);
    let owner = control.join("owner.json");
    let exact_bytes = fs::read(&owner).unwrap();
    let retained = fixture.path().join("retained-owner.json");
    fs::rename(&owner, &retained).unwrap();
    fs::write(&owner, exact_bytes).unwrap();
    fs::set_permissions(&owner, fs::Permissions::from_mode(0o600)).unwrap();

    let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert_bounded_raw_free_failure(&rejected, &fixture, seam);
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(bindings.len(), 1);
    let quarantined = bound_private_control(&fixture, &bindings[0]);
    assert!(retained.is_file(), "bound original owner was deleted");
    assert!(
        quarantined.join("owner.json").is_file(),
        "exact-content owner replacement was deleted"
    );
    assert_candidate_workspaces_empty(&fixture, seam);
    cleanup_tournament_domains_before_assertions();
    assert_no_tournament_domains();
}

#[test]
fn required_real_private_cleanup_unlink_prefixes_are_restart_idempotent() {
    for seam in [
        "after_private_owner_unlink",
        "after_private_socket_unlink",
        "after_private_runner_unlink",
        "after_private_cleanup_completion_receipt",
    ] {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let tournament_uuid = Uuid::new_v4();
        let crashed = run_restart_component(
            &fixture,
            "crash-runtime",
            tournament_uuid,
            Some("before_managed_launch"),
            0,
        );
        assert_eq!(crashed.status.code(), Some(86));
        let interrupted =
            run_restart_component(&fixture, "recover-runtime", tournament_uuid, Some(seam), 0);
        assert_eq!(interrupted.status.code(), Some(86), "{seam}");
        assert!(interrupted.stdout.is_empty(), "{seam}");
        assert!(interrupted.stderr.is_empty(), "{seam}");
        let bindings = managed_artifact_bindings(&fixture);
        assert_eq!(bindings.len(), 1, "{seam}");
        assert!(bindings[0].get("cleanup_quarantine").is_some(), "{seam}");

        let recovered =
            run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
        assert!(
            recovered.status.success(),
            "{seam} retry failed: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
        assert!(recovered.stderr.is_empty());
        assert!(!retained_private_control(&fixture, tournament_uuid).exists());
        assert!(managed_artifact_bindings(&fixture).is_empty(), "{seam}");
        assert_candidate_workspaces_empty(&fixture, seam);
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_private_file_unlink_before_receipt_remains_operator_safe() {
    for (seam, step) in [
        ("after_private_ownership_physical_unlink", "ownership"),
        ("after_private_socket_physical_unlink", "socket"),
        ("after_private_runner_physical_unlink", "runner"),
    ] {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let tournament_uuid = Uuid::new_v4();
        let crashed = run_restart_component(
            &fixture,
            "crash-runtime",
            tournament_uuid,
            Some("before_managed_launch"),
            0,
        );
        assert_eq!(crashed.status.code(), Some(86), "{seam}");
        let interrupted =
            run_restart_component(&fixture, "recover-runtime", tournament_uuid, Some(seam), 0);
        assert_eq!(interrupted.status.code(), Some(86), "{seam}");
        assert!(interrupted.stdout.is_empty(), "{seam}");
        assert!(interrupted.stderr.is_empty(), "{seam}");
        let bindings = managed_artifact_bindings(&fixture);
        assert_eq!(bindings.len(), 1, "{seam}");
        assert_eq!(
            bindings[0]
                .get("cleanup_unlink_pending")
                .and_then(|value| value.as_str()),
            Some(step),
            "{seam} lost its durable unlink intent"
        );

        let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
        assert_bounded_raw_free_failure(&rejected, &fixture, seam);
        assert_eq!(
            managed_artifact_bindings(&fixture),
            bindings,
            "{seam} ambiguous physical absence was acknowledged"
        );
        assert_candidate_workspaces_empty(&fixture, seam);
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_private_directory_unlink_before_receipt_remains_operator_safe() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    );
    assert_eq!(crashed.status.code(), Some(86));

    let seam = "after_private_directory_unlink";
    let interrupted =
        run_restart_component(&fixture, "recover-runtime", tournament_uuid, Some(seam), 0);
    assert_eq!(interrupted.status.code(), Some(86));
    assert!(interrupted.stdout.is_empty());
    assert!(interrupted.stderr.is_empty());
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0]
            .get("cleanup_unlink_pending")
            .and_then(|value| value.as_str()),
        Some("directory")
    );
    assert_ne!(
        bindings[0]
            .get("cleanup_completed")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    let quarantine = bindings[0]
        .get("cleanup_quarantine")
        .and_then(|value| value.as_str())
        .expect("durable cleanup quarantine");
    assert!(!fixture.path().join("control").join(quarantine).exists());

    let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert_bounded_raw_free_failure(&rejected, &fixture, seam);
    assert_eq!(
        managed_artifact_bindings(&fixture),
        bindings,
        "ambiguous post-unlink absence was acknowledged or advanced"
    );
    assert_candidate_workspaces_empty(&fixture, seam);
    assert_no_tournament_domains();
}

#[test]
fn required_real_socket_retirement_intent_resumes_before_cleanup() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let seam = "after_private_socket_retirement_intent";
    let crashed = run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "{seam} did not crash at durable intent: {}",
        String::from_utf8_lossy(&crashed.stderr)
    );
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());
    let control = retained_private_control(&fixture, tournament_uuid);
    assert!(control.join("control.sock").exists());
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0]
            .get("socket_retire_pending")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "socket retirement intent did not resume: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert!(!control.exists());
    assert!(managed_artifact_bindings(&fixture).is_empty());
    assert_candidate_workspaces_empty(&fixture, seam);
    assert_no_tournament_domains();
}

#[test]
fn required_real_socket_retirement_intent_rechecks_identity_before_unlink() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let seam = "after_private_socket_retirement_intent";
    let control = retained_private_control(&fixture, tournament_uuid);
    let socket = control.join("control.sock");
    let retained = fixture.path().join("retained-retirement-socket");
    let output =
        restart_component_command(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0)
            .env(
                "LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION",
                "hostile-swap",
            )
            .env("LTERM_INTERNAL_SPECULATION_SWAP_PATH", &socket)
            .env("LTERM_INTERNAL_SPECULATION_SWAP_BACKUP", &retained)
            .env("LTERM_INTERNAL_SPECULATION_SWAP_KIND", "socket")
            .env("LTERM_INTERNAL_SPECULATION_OBSERVE_SOCKET_RETIREMENT", "1")
            .output()
            .expect("run socket-retirement intent swap");
    let replacement_is_socket =
        fs::symlink_metadata(&socket).map(|metadata| metadata.file_type().is_socket());
    let retained_is_socket =
        fs::symlink_metadata(&retained).map(|metadata| metadata.file_type().is_socket());
    let receipt_exists = fixture
        .path()
        .join("control/socket-retirement-receipt")
        .exists();
    let bindings = managed_artifact_bindings(&fixture);
    fs::remove_file(&socket).unwrap();
    fs::rename(&retained, &socket).unwrap();
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    let bindings_after = managed_artifact_bindings(&fixture);
    let leaked_before_cleanup = cleanup_tournament_domains_before_assertions();

    assert_bounded_raw_free_failure(&output, &fixture, seam);
    assert!(
        replacement_is_socket.unwrap(),
        "replacement socket was unlinked"
    );
    assert!(retained_is_socket.unwrap(), "retained socket was lost");
    assert!(
        !receipt_exists,
        "identity mismatch received a retirement receipt"
    );
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0]
            .get("socket_retire_pending")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_ne!(
        bindings[0]
            .get("socket_retired")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(
        recovered.status.success(),
        "restored socket intent did not recover: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(bindings_after.is_empty());
    assert!(
        !leaked_before_cleanup,
        "socket intent swap leaked a tournament"
    );
    assert_candidate_workspaces_empty(&fixture, seam);
    assert_no_tournament_domains();
}

#[test]
fn required_real_socket_retirement_unlink_before_receipt_remains_operator_safe() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let seam = "after_private_socket_retirement_physical_unlink";
    let crashed = run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
    assert_eq!(crashed.status.code(), Some(86));
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());
    let control = retained_private_control(&fixture, tournament_uuid);
    assert!(control.join("lterm").is_file());
    assert!(!control.join("control.sock").exists());
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0]
            .get("socket_retire_pending")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert_bounded_raw_free_failure(&rejected, &fixture, seam);
    assert_eq!(
        managed_artifact_bindings(&fixture),
        bindings,
        "ambiguous retired-socket absence was acknowledged"
    );
    assert!(control.join("lterm").is_file());
    assert_candidate_workspaces_empty(&fixture, seam);
    assert_no_tournament_domains();
}

#[test]
fn required_real_socket_retirement_receipt_proves_exact_unlink_and_runner_continuity() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let seam = "after_private_socket_retirement_receipt";
    let crashed =
        restart_component_command(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0)
            .env("LTERM_INTERNAL_SPECULATION_OBSERVE_SOCKET_RETIREMENT", "1")
            .output()
            .expect("run socket retirement receipt observer");
    assert_eq!(crashed.status.code(), Some(86));
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());

    let control = retained_private_control(&fixture, tournament_uuid);
    assert!(!control.join("control.sock").exists());
    let runner = fs::metadata(control.join("lterm")).expect("retained runner path");
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        bindings[0]
            .get("socket_retired")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_ne!(
        bindings[0]
            .get("socket_retire_pending")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(bindings[0]["runner"]["dev"].as_u64(), Some(runner.dev()));
    assert_eq!(bindings[0]["runner"]["ino"].as_u64(), Some(runner.ino()));

    let marker_path = fixture.path().join("control/socket-retirement-receipt");
    let marker = fs::read_to_string(&marker_path).expect("socket retirement observation");
    let observed = marker
        .lines()
        .map(|line| line.split_once('=').expect("bounded observation field"))
        .map(|(name, value)| (name, value.parse::<u64>().expect("numeric observation")))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(observed.get("socket_nlink"), Some(&0));
    assert_eq!(observed.get("runner_dev"), Some(&runner.dev()));
    assert_eq!(observed.get("runner_ino"), Some(&runner.ino()));
    fs::remove_file(marker_path).expect("remove debug receipt before recovery");

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "socket retirement receipt recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert!(!control.exists());
    assert!(managed_artifact_bindings(&fixture).is_empty());
    assert_candidate_workspaces_empty(&fixture, seam);
    assert_no_tournament_domains();
}

#[test]
fn required_real_quarantine_recovery_preserves_a_new_live_leaf_replacement() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    );
    assert_eq!(crashed.status.code(), Some(86));
    let interrupted = run_restart_component(
        &fixture,
        "recover-runtime",
        tournament_uuid,
        Some("after_private_quarantine_publish"),
        0,
    );
    assert_eq!(interrupted.status.code(), Some(86));
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(bindings.len(), 1);
    let quarantine_leaf = bindings[0]
        .get("cleanup_quarantine")
        .and_then(|value| value.as_str())
        .expect("durable cleanup quarantine");
    let control_root = fixture.path().join("control");
    let quarantine = control_root.join(quarantine_leaf);
    let live = retained_private_control(&fixture, tournament_uuid);
    assert!(quarantine.is_dir());
    assert!(!live.exists());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&live)
        .expect("install hostile live-leaf replacement");
    fs::write(live.join("hostile"), b"retain").unwrap();

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "quarantine retry failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(!quarantine.exists());
    assert_eq!(fs::read(live.join("hostile")).unwrap(), b"retain");
    assert!(managed_artifact_bindings(&fixture).is_empty());
    fs::remove_file(live.join("hostile")).unwrap();
    fs::remove_dir(&live).unwrap();
    assert_candidate_workspaces_empty(&fixture, "quarantine replacement");
    assert_no_tournament_domains();
}

#[test]
fn required_real_quarantine_relocation_never_clears_cleanup_binding() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    );
    assert_eq!(crashed.status.code(), Some(86));
    let interrupted = run_restart_component(
        &fixture,
        "recover-runtime",
        tournament_uuid,
        Some("after_private_quarantine_publish"),
        0,
    );
    assert_eq!(interrupted.status.code(), Some(86));
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(bindings.len(), 1);
    let quarantine_leaf = bindings[0]
        .get("cleanup_quarantine")
        .and_then(|value| value.as_str())
        .unwrap();
    let quarantine = fixture.path().join("control").join(quarantine_leaf);
    let relocated = fixture.path().join("relocated-private-control");
    fs::rename(&quarantine, &relocated).unwrap();

    let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    let binding_retained = managed_artifact_bindings(&fixture).len() == 1;
    let owned_retained = relocated.join("lterm").is_file();
    assert!(!rejected.status.success());
    assert!(
        binding_retained,
        "ambiguous absence cleared cleanup binding"
    );
    assert!(owned_retained, "relocated owned runner was destroyed");

    fs::rename(&relocated, &quarantine).unwrap();
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "restored quarantine did not converge: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(managed_artifact_bindings(&fixture).is_empty());
    assert_candidate_workspaces_empty(&fixture, "quarantine relocation");
    assert_no_tournament_domains();
}

#[test]
fn required_real_old_boot_binding_acks_without_touching_current_replacement() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    );
    assert_eq!(crashed.status.code(), Some(86));
    let live = retained_private_control(&fixture, tournament_uuid);
    assert!(live.is_dir());
    rewrite_managed_binding_boot_uuid(&fixture, Uuid::new_v4());
    fs::remove_dir_all(&live).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&live).unwrap();
    fs::write(live.join("hostile"), b"current boot replacement").unwrap();

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    let replacement = fs::read(live.join("hostile"));
    let binding_absent = managed_artifact_bindings(&fixture).is_empty();
    assert!(
        recovered.status.success(),
        "old-boot cleanup failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(replacement.unwrap(), b"current boot replacement");
    assert!(
        binding_absent,
        "old-boot logical absence was not acknowledged"
    );
    fs::remove_file(live.join("hostile")).unwrap();
    fs::remove_dir(&live).unwrap();
    assert_candidate_workspaces_empty(&fixture, "old boot replacement");
    assert_no_tournament_domains();
}

#[test]
fn required_real_resolved_ownerless_tombstone_does_not_block_private_alias_cleanup() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    );
    assert_eq!(crashed.status.code(), Some(86));
    let generic = Command::new(env!("CARGO_BIN_EXE_lterm"))
        .arg(INTERNAL_MANAGED_TEST_ARG)
        .arg("/usr/bin/true")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LTERM_INTERNAL_TEST_MODE", "1")
        .env("LTERM_DATA_DIR", fixture.path().join("data"))
        .output()
        .expect("create resolved ownerless managed tombstone");
    assert!(
        generic.status.success(),
        "generic managed launch failed: {}",
        String::from_utf8_lossy(&generic.stderr)
    );

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "resolved ownerless tombstone blocked private cleanup: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(!retained_private_control(&fixture, tournament_uuid).exists());
    assert!(managed_artifact_bindings(&fixture).is_empty());
    assert_candidate_workspaces_empty(&fixture, "resolved ownerless tombstone");
    assert_no_tournament_domains();
}

#[test]
fn required_real_created_binding_recovers_markerless_private_control() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("after_private_control_binding"),
        0,
    );
    let retained_control = retained_private_control(&fixture, tournament_uuid);
    assert_eq!(crashed.status.code(), Some(86));
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());
    assert!(retained_control.is_dir());
    assert!(
        fs::read_dir(&retained_control)
            .expect("inspect markerless creation-pending directory")
            .next()
            .is_none(),
        "creation-pending directory gained artifacts before the created binding"
    );
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(
        bindings.len(),
        1,
        "creation-pending binding was not durable"
    );
    let expected_leaf = format!("lterm-g003-{tournament_uuid}-candidate-0");
    assert_eq!(
        bindings[0]
            .get("private_leaf")
            .and_then(|value| value.as_str()),
        Some(expected_leaf.as_str())
    );
    assert!(
        bindings[0].get("private_directory").is_some(),
        "created binding omitted its fsynced directory identity"
    );
    assert!(
        !bindings[0]
            .get("runner_create_pending")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        "clean pre-runner phase entered physical creation"
    );

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "creation-pending recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert!(!retained_control.exists());
    assert!(
        managed_artifact_bindings(&fixture).is_empty(),
        "private cleanup completed without binding acknowledgement"
    );
    assert_candidate_workspaces_empty(&fixture, "creation-pending recovery");
    assert_no_tournament_domains();
}

#[test]
fn required_real_bound_runner_pre_socket_phase_is_restart_recoverable() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let seam = "after_private_runner_binding";
    let crashed = run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
    assert_eq!(crashed.status.code(), Some(86));
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());
    let control = retained_private_control(&fixture, tournament_uuid);
    assert!(control.join("lterm").is_file());
    assert!(!control.join("control.sock").exists());
    let bindings = managed_artifact_bindings(&fixture);
    assert_eq!(bindings.len(), 1);
    assert!(bindings[0].get("runner").is_some());
    assert!(bindings[0].get("socket").is_none());
    assert!(
        !bindings[0]
            .get("socket_create_pending")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        "clean pre-socket phase entered physical creation"
    );

    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "{seam} recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert!(!control.exists());
    assert!(managed_artifact_bindings(&fixture).is_empty());
    assert_candidate_workspaces_empty(&fixture, seam);
    assert_no_tournament_domains();
}

#[test]
fn required_real_create_pending_never_deletes_or_acks_an_empty_replacement() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_private_control_binding"),
        0,
    );
    let retained_control = retained_private_control(&fixture, tournament_uuid);
    assert_eq!(crashed.status.code(), Some(86));
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());
    assert!(retained_control.is_dir());
    assert!(
        fs::read_dir(&retained_control).unwrap().next().is_none(),
        "create-pending crash left an unexpected artifact"
    );
    let before = managed_artifact_bindings(&fixture);
    assert_eq!(before.len(), 1);
    assert!(before[0].get("private_directory").is_none());

    let blocked = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(!blocked.status.success());
    assert!(retained_control.is_dir(), "empty replacement was deleted");
    let blocked_bindings = managed_artifact_bindings(&fixture);
    assert_eq!(blocked_bindings.len(), 1, "empty replacement was ACKed");
    assert!(
        blocked_bindings[0].get("cleanup_quarantine").is_some(),
        "cleanup intent was not durable before inspecting the live leaf"
    );

    fs::remove_dir(&retained_control).expect("remove test-owned empty replacement");
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert!(
        recovered.status.success(),
        "absent fast-path recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
    assert!(recovered.stderr.is_empty());
    assert!(managed_artifact_bindings(&fixture).is_empty());
    assert_candidate_workspaces_empty(&fixture, "create-pending fail-closed recovery");
    assert_no_tournament_domains();
}

#[test]
fn required_real_unbound_runner_relocation_and_replacement_fail_closed() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let seam = "before_private_runner_binding";
    let crashed = run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
    assert_eq!(crashed.status.code(), Some(86));
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());
    let live = retained_private_control(&fixture, tournament_uuid);
    let runner = live.join("lterm");
    let relocated = fixture.path().join("relocated-unbound-runner");
    fs::rename(&runner, &relocated).unwrap();
    fs::write(&runner, b"hostile replacement").unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o500)).unwrap();
    let before = managed_artifact_bindings(&fixture);
    assert_eq!(before.len(), 1);
    assert!(before[0].get("runner").is_none());
    assert_eq!(
        before[0]
            .get("runner_create_pending")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );

    let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert_bounded_raw_free_failure(&rejected, &fixture, seam);
    let after = managed_artifact_bindings(&fixture);
    assert_eq!(after.len(), 1, "unbound runner binding was acknowledged");
    assert!(after[0].get("runner").is_none());
    assert_eq!(
        after[0]
            .get("runner_create_pending")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "unbound runner lost its durable creation intent"
    );
    assert!(
        after[0].get("cleanup_quarantine").is_none(),
        "creation-pending runner incorrectly entered cleanup"
    );
    assert!(live.is_dir(), "pending private control was renamed");
    assert_eq!(fs::read(&runner).unwrap(), b"hostile replacement");
    assert!(
        relocated.is_file(),
        "relocated unbound runner was destroyed"
    );
    assert_candidate_workspaces_empty(&fixture, seam);
    assert_no_tournament_domains();
}

#[test]
fn required_real_unbound_socket_relocation_and_replacement_fail_closed() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let seam = "before_private_socket_binding";
    let crashed = run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
    assert_eq!(crashed.status.code(), Some(86));
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());
    let live = retained_private_control(&fixture, tournament_uuid);
    let socket = live.join("control.sock");
    let relocated = fixture.path().join("relocated-unbound-socket");
    fs::rename(&socket, &relocated).unwrap();
    let _replacement = bind_seqpacket_test_listener(&socket);
    let before = managed_artifact_bindings(&fixture);
    assert_eq!(before.len(), 1);
    assert!(before[0].get("socket").is_none());
    assert_eq!(
        before[0]
            .get("socket_create_pending")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );

    let rejected = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    assert_bounded_raw_free_failure(&rejected, &fixture, seam);
    let after = managed_artifact_bindings(&fixture);
    assert_eq!(after.len(), 1, "unbound socket binding was acknowledged");
    assert!(after[0].get("socket").is_none());
    assert_eq!(
        after[0]
            .get("socket_create_pending")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "unbound socket lost its durable creation intent"
    );
    assert!(
        after[0].get("cleanup_quarantine").is_none(),
        "creation-pending socket incorrectly entered cleanup"
    );
    assert!(live.is_dir(), "pending private control was renamed");
    assert!(
        fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_socket(),
        "hostile replacement socket was destroyed"
    );
    assert!(
        fs::symlink_metadata(&relocated)
            .unwrap()
            .file_type()
            .is_socket(),
        "relocated unbound socket was destroyed"
    );
    assert_candidate_workspaces_empty(&fixture, seam);
    assert_no_tournament_domains();
}

#[test]
fn required_real_final_socket_binding_rejects_the_last_hostile_path_swap() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let seam = "before_private_socket_binding";
    let live = retained_private_control(&fixture, tournament_uuid);
    let socket = live.join("control.sock");
    let retained = fixture.path().join("retained-prebinding-socket");
    let output =
        restart_component_command(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0)
            .env(
                "LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION",
                "hostile-swap",
            )
            .env("LTERM_INTERNAL_SPECULATION_SWAP_PATH", &socket)
            .env("LTERM_INTERNAL_SPECULATION_SWAP_BACKUP", &retained)
            .env("LTERM_INTERNAL_SPECULATION_SWAP_KIND", "socket")
            .output()
            .expect("run final socket-binding hostile swap");
    let bindings = managed_artifact_bindings(&fixture);
    let replacement_root = bindings
        .first()
        .and_then(|binding| binding.get("cleanup_quarantine"))
        .and_then(|value| value.as_str())
        .map(|leaf| fixture.path().join("control").join(leaf))
        .unwrap_or(live);
    let replacement_is_socket = fs::symlink_metadata(replacement_root.join("control.sock"))
        .map(|metadata| metadata.file_type().is_socket());
    let retained_is_socket =
        fs::symlink_metadata(&retained).map(|metadata| metadata.file_type().is_socket());
    let recovery = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    let bindings_after = managed_artifact_bindings(&fixture);
    let leaked_before_cleanup = cleanup_tournament_domains_before_assertions();

    assert_bounded_raw_free_failure(&output, &fixture, seam);
    assert_bounded_raw_free_failure(&recovery, &fixture, seam);
    assert_eq!(bindings.len(), 1);
    assert!(bindings[0].get("socket").is_none());
    assert_eq!(bindings_after.len(), 1);
    assert!(bindings_after[0].get("socket").is_none());
    assert!(
        replacement_is_socket.unwrap(),
        "replacement socket was deleted"
    );
    assert!(
        retained_is_socket.unwrap(),
        "relocated original socket was deleted"
    );
    assert!(
        !leaked_before_cleanup,
        "final socket swap leaked a tournament"
    );
    assert_candidate_workspaces_empty(&fixture, seam);
    assert_no_tournament_domains();
}

#[test]
fn required_real_final_runner_name_swap_executes_only_the_retained_runner_fd() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let control = retained_private_control(&fixture, tournament_uuid);
    let runner = control.join("lterm");
    let retained = fixture.path().join("retained-original-runner");
    let output = restart_component_command(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    )
    .env(
        "LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION",
        "hostile-swap",
    )
    .env("LTERM_INTERNAL_SPECULATION_SWAP_PATH", &runner)
    .env("LTERM_INTERNAL_SPECULATION_SWAP_BACKUP", &retained)
    .env("LTERM_INTERNAL_SPECULATION_SWAP_KIND", "runner")
    .output()
    .expect("run retained-runner hostile swap case");
    let bindings = managed_artifact_bindings(&fixture);
    let replacement_root = bindings
        .first()
        .and_then(|binding| binding.get("cleanup_quarantine"))
        .and_then(|value| value.as_str())
        .map(|leaf| fixture.path().join("control").join(leaf))
        .unwrap_or(control);
    let replacement = fs::read(replacement_root.join("lterm"));
    let retained_exists = retained.is_file();
    let socket_retired = bindings
        .first()
        .and_then(|binding| binding.get("socket_retired"))
        .and_then(|value| value.as_bool());
    fs::remove_file(replacement_root.join("lterm")).unwrap();
    fs::rename(&retained, replacement_root.join("lterm")).unwrap();
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    let bindings_after = managed_artifact_bindings(&fixture);
    let leaked_before_cleanup = cleanup_tournament_domains_before_assertions();

    assert_bounded_raw_free_failure(&output, &fixture, "retained final runner swap");
    assert_eq!(replacement.unwrap(), b"hostile replacement");
    assert!(
        retained_exists,
        "original retained runner was not preserved"
    );
    assert_eq!(bindings.len(), 1);
    assert_eq!(
        socket_retired,
        Some(true),
        "runner never reached the authenticated control socket through the retained FD"
    );
    assert!(
        recovered.status.success(),
        "restored retained runner did not recover: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(bindings_after.is_empty());
    assert!(
        !leaked_before_cleanup,
        "retained runner swap leaked a tournament"
    );
    assert_candidate_workspaces_empty(&fixture, "retained final runner swap");
    assert_no_tournament_domains();
}

#[test]
fn required_real_control_socket_swap_authenticates_only_retained_endpoint_fd() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let control = retained_private_control(&fixture, tournament_uuid);
    let socket = control.join("control.sock");
    let retained = fixture.path().join("retained-original-control-socket");
    let output = restart_component_command(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    )
    .env(
        "LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION",
        "hostile-swap",
    )
    .env("LTERM_INTERNAL_SPECULATION_SWAP_PATH", &socket)
    .env("LTERM_INTERNAL_SPECULATION_SWAP_BACKUP", &retained)
    .env("LTERM_INTERNAL_SPECULATION_SWAP_KIND", "socket")
    .env("LTERM_INTERNAL_SPECULATION_OBSERVE_PINNED_CONTROL", "1")
    .output()
    .expect("run retained control-socket hostile swap case");
    let bindings = managed_artifact_bindings(&fixture);
    let replacement_root = bindings
        .first()
        .and_then(|binding| binding.get("cleanup_quarantine"))
        .and_then(|value| value.as_str())
        .map(|leaf| fixture.path().join("control").join(leaf))
        .unwrap_or(control);
    let rogue_uncontacted = fs::read(replacement_root.join(".lterm-rogue-socket-uncontacted"));
    let authenticated = fs::read(replacement_root.join(".lterm-retained-control-authenticated"));
    let replacement_is_socket = fs::symlink_metadata(replacement_root.join("control.sock"))
        .map(|metadata| metadata.file_type().is_socket());
    let retained_is_socket =
        fs::symlink_metadata(&retained).map(|metadata| metadata.file_type().is_socket());
    fs::remove_file(replacement_root.join("control.sock")).unwrap();
    let _ = fs::remove_file(replacement_root.join(".lterm-rogue-socket-uncontacted"));
    let _ = fs::remove_file(replacement_root.join(".lterm-retained-control-authenticated"));
    fs::rename(&retained, replacement_root.join("control.sock")).unwrap();
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    let bindings_after = managed_artifact_bindings(&fixture);
    let leaked_before_cleanup = cleanup_tournament_domains_before_assertions();

    assert_bounded_raw_free_failure(&output, &fixture, "retained control socket swap");
    assert_eq!(
        authenticated.unwrap_or_else(|error| panic!(
            "retained endpoint was not authenticated: {error}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )),
        b"retained-control-authenticated\n"
    );
    assert_eq!(
        rogue_uncontacted.unwrap_or_else(|error| panic!(
            "rogue contact proof was not written: {error}; bindings={bindings:?}; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )),
        b"rogue-control-uncontacted\n"
    );
    assert!(
        replacement_is_socket.unwrap(),
        "replacement is not a socket"
    );
    assert!(
        retained_is_socket.unwrap(),
        "retained endpoint was destroyed"
    );
    assert!(
        recovered.status.success(),
        "restored retained endpoint did not recover: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(bindings_after.is_empty());
    assert!(
        !leaked_before_cleanup,
        "control socket swap leaked a tournament"
    );
    assert_candidate_workspaces_empty(&fixture, "retained control socket swap");
    assert_no_tournament_domains();
}

#[test]
fn required_real_candidate_directory_swap_mounts_only_the_retained_directory_fd() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let candidate = fixture.path().join("candidate-0");
    let retained = fixture.path().join("retained-original-candidate");
    let output = restart_component_command(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    )
    .env(
        "LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION",
        "hostile-swap",
    )
    .env("LTERM_INTERNAL_SPECULATION_SWAP_PATH", &candidate)
    .env("LTERM_INTERNAL_SPECULATION_SWAP_BACKUP", &retained)
    .env("LTERM_INTERNAL_SPECULATION_SWAP_KIND", "directory")
    .env("LTERM_INTERNAL_SPECULATION_OBSERVE_PINNED_WORKSPACE", "1")
    .output()
    .expect("run retained-candidate hostile swap case");
    let retained_marker = fs::read(retained.join(".lterm-retained-workspace-mounted"));
    let replacement_marker = candidate.join(".lterm-retained-workspace-mounted").exists();
    let hostile = fs::read(candidate.join("hostile"));
    fs::remove_file(candidate.join("hostile")).unwrap();
    fs::remove_dir(&candidate).unwrap();
    fs::remove_file(retained.join(".lterm-retained-workspace-mounted")).unwrap();
    fs::rename(&retained, &candidate).unwrap();
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    let bindings = managed_artifact_bindings(&fixture);
    let leaked_before_cleanup = cleanup_tournament_domains_before_assertions();

    assert_bounded_raw_free_failure(&output, &fixture, "retained candidate directory swap");
    assert_eq!(retained_marker.unwrap(), b"retained-workspace-mounted\n");
    assert!(!replacement_marker, "replacement candidate was mounted");
    assert_eq!(hostile.unwrap(), b"retain");
    assert!(
        recovered.status.success(),
        "retained candidate recovery failed: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(
        bindings.is_empty(),
        "candidate recovery retained bindings: {bindings:?}; stderr={}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(!leaked_before_cleanup, "candidate swap leaked a tournament");
    assert_candidate_workspaces_empty(&fixture, "retained candidate directory swap");
    assert_no_tournament_domains();
}

#[test]
fn required_real_control_directory_swap_uses_retained_fd_and_never_contacts_rogue_socket() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let control = retained_private_control(&fixture, tournament_uuid);
    let retained = fixture.path().join("retained-original-control");
    let output = restart_component_command(
        &fixture,
        "crash-runtime",
        tournament_uuid,
        Some("before_managed_launch"),
        0,
    )
    .env(
        "LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION",
        "hostile-swap",
    )
    .env("LTERM_INTERNAL_SPECULATION_SWAP_PATH", &control)
    .env("LTERM_INTERNAL_SPECULATION_SWAP_BACKUP", &retained)
    .env("LTERM_INTERNAL_SPECULATION_SWAP_KIND", "control-directory")
    .env("LTERM_INTERNAL_SPECULATION_OBSERVE_PINNED_CONTROL", "1")
    .output()
    .expect("run retained-control hostile swap case");
    let authenticated = fs::read(retained.join(".lterm-retained-control-authenticated"));
    let rogue_uncontacted = fs::read(control.join(".lterm-rogue-control-uncontacted"));
    let hostile = fs::read(control.join("hostile"));
    fs::remove_file(control.join("control.sock")).unwrap();
    fs::remove_file(control.join(".lterm-rogue-control-uncontacted")).unwrap();
    fs::remove_file(control.join("hostile")).unwrap();
    fs::remove_dir(&control).unwrap();
    fs::remove_file(retained.join(".lterm-retained-control-authenticated")).unwrap();
    fs::rename(&retained, &control).unwrap();
    let recovered = run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
    let bindings = managed_artifact_bindings(&fixture);
    let leaked_before_cleanup = cleanup_tournament_domains_before_assertions();

    assert_bounded_raw_free_failure(&output, &fixture, "retained control directory swap");
    assert_eq!(authenticated.unwrap(), b"retained-control-authenticated\n");
    assert_eq!(rogue_uncontacted.unwrap(), b"rogue-control-uncontacted\n");
    assert_eq!(hostile.unwrap(), b"retain");
    assert!(
        recovered.status.success(),
        "restored retained control did not recover: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(bindings.is_empty());
    assert!(!leaked_before_cleanup, "control swap leaked a tournament");
    assert_candidate_workspaces_empty(&fixture, "retained control directory swap");
    assert_no_tournament_domains();
}

#[test]
fn required_real_actor_service_progresses_and_finalizes_loser_first() {
    let Some((_fixture, output, leaked_before_cleanup)) = run_required_actor_service("finalize")
    else {
        return;
    };
    assert!(
        output.status.success(),
        "actor service driver failed with bounded stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"speculation-actor-service=1\n");
    assert!(
        output.stderr.is_empty(),
        "actor service emitted stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !leaked_before_cleanup,
        "actor service leaked a tournament domain before test cleanup"
    );
    assert_no_tournament_domains();
}

#[test]
fn required_real_actor_service_observes_requested_running_and_result_pending_leases() {
    for timeout_ms in [45_000, 75_000] {
        let Some((_fixture, output, leaked_before_cleanup)) =
            run_required_actor_service_case_with_lease("finalize", None, Some(timeout_ms))
        else {
            return;
        };
        assert!(
            output.status.success(),
            "actor service timeout {timeout_ms} failed with bounded stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"speculation-actor-service=1\n");
        assert!(
            output.stderr.is_empty(),
            "actor service timeout {timeout_ms} emitted stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !leaked_before_cleanup,
            "actor service timeout {timeout_ms} leaked a tournament domain before test cleanup"
        );
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_actor_service_rollback_expiry_and_shutdown_converge() {
    for terminal in ["rollback", "expiry", "shutdown"] {
        let Some((_fixture, output, leaked_before_cleanup)) = run_required_actor_service(terminal)
        else {
            return;
        };
        assert!(
            output.status.success(),
            "actor service {terminal} failed with bounded stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"speculation-actor-service=1\n");
        assert!(
            output.stderr.is_empty(),
            "actor service {terminal} emitted stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !leaked_before_cleanup,
            "actor service {terminal} leaked a tournament domain before test cleanup"
        );
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_prepare_post_allocation_failpoints_close_positive_terminal() {
    for failpoint in [
        "after_prepared_allocation",
        "after_prepared_readback",
        "after_prepared_index_insert",
    ] {
        let Some((_fixture, output, leaked_before_cleanup)) =
            run_required_actor_service_case("finalize", Some(failpoint))
        else {
            return;
        };
        assert!(
            output.status.success(),
            "prepare failpoint {failpoint} failed with bounded stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"speculation-actor-service=1\n");
        assert!(output.stderr.is_empty());
        assert!(
            !leaked_before_cleanup,
            "prepare failpoint {failpoint} leaked a tournament domain before test cleanup"
        );
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_failpoint_is_bounded_and_leaves_no_tournament_domain() {
    let Some((_fixture, output, leaked_before_cleanup)) =
        run_required_component(Some("before_tournament_create"))
    else {
        return;
    };
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: speculation_containment_evidence_unavailable\n"
    );
    assert!(
        !leaked_before_cleanup,
        "pre-create failpoint leaked a tournament domain before test cleanup"
    );
    assert_no_tournament_domains();
}

#[test]
fn required_real_probe_canary_abrupt_exit_leaves_no_workspace_mutation() {
    let seam = "probe_after_workspace_canary_write";
    let Some((fixture, output, leaked_before_cleanup)) = run_required_component(Some(seam)) else {
        return;
    };
    assert_bounded_raw_free_failure(&output, &fixture, seam);
    assert_scripted_execution_workspaces_unchanged(&fixture, seam);
    assert!(
        !leaked_before_cleanup,
        "{seam} leaked a tournament domain before test cleanup"
    );
    assert_no_tournament_domains();
}

#[test]
fn required_real_runner_ancillary_failpoint_is_bounded_and_cleanup_converges() {
    let Some((_fixture, output, leaked_before_cleanup)) =
        run_required_component(Some("runner_before_payload_fd_validation"))
    else {
        return;
    };
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: speculation_containment_peer_rejected\n"
    );
    assert!(
        !leaked_before_cleanup,
        "ancillary runner failure leaked a tournament domain before test cleanup"
    );
    assert_no_tournament_domains();
}

#[test]
fn required_real_create_edges_recover_before_and_after_abrupt_restart() {
    let cases = [
        ("before_tournament_create", 0_u8, false),
        ("after_tournament_create", 0_u8, true),
        ("before_candidate_parent_create", 0_u8, false),
        ("after_candidate_parent_create", 0_u8, true),
        ("before_candidate_parent_create", 1_u8, false),
        ("after_candidate_parent_create", 1_u8, true),
        ("before_control_create", 0_u8, false),
        ("after_control_create", 0_u8, true),
        ("before_control_create", 1_u8, false),
        ("after_control_create", 1_u8, true),
        ("before_payload_create", 0_u8, false),
        ("after_payload_create", 0_u8, true),
        ("before_payload_create", 1_u8, false),
        ("after_payload_create", 1_u8, true),
    ];
    for (failpoint, candidate, expected_adopted) in cases {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let tournament_uuid = Uuid::new_v4();
        let crashed = run_restart_component(
            &fixture,
            "crash-create",
            tournament_uuid,
            Some(failpoint),
            candidate,
        );
        let recovered =
            run_restart_component(&fixture, "recover-create", tournament_uuid, None, candidate);
        assert_eq!(
            crashed.status.code(),
            Some(86),
            "{failpoint} candidate {candidate} did not stop abruptly"
        );
        assert!(crashed.stdout.is_empty(), "{failpoint} emitted stdout");
        assert!(crashed.stderr.is_empty(), "{failpoint} emitted stderr");
        assert!(
            recovered.status.success(),
            "{failpoint} candidate {candidate} recovery failed with bounded stderr: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(
            recovered.stdout,
            format!(
                "speculation-restart-recovered=1 adopted={}\n",
                expected_adopted
            )
            .as_bytes(),
            "{failpoint} candidate {candidate} reconciliation mode"
        );
        assert!(recovered.stderr.is_empty());
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_create_pending_rejects_unexpected_empty_child_without_adoption() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-create",
        tournament_uuid,
        Some("after_tournament_create"),
        0,
    );
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "crash setup failed: stdout={} stderr={}",
        String::from_utf8_lossy(&crashed.stdout),
        String::from_utf8_lossy(&crashed.stderr)
    );
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());

    let record_path = fixture.path().join("restart-record.json");
    let record_before = fs::read(&record_path).expect("read create-pending record");
    let domain = tournament_domain(tournament_uuid);
    let unexpected = domain.join("unexpected-empty");
    fs::create_dir(&unexpected).expect("create unexpected empty descendant");

    let recovered = run_restart_component(&fixture, "recover-create", tournament_uuid, None, 0);
    let foreign_topology_survived = unexpected.is_dir();
    let record_after = fs::read(&record_path).expect("reread rejected create record");

    fs::remove_dir(&unexpected).expect("remove owned empty attack child");
    fs::remove_dir(&domain).expect("remove rejected tournament domain");

    assert_bounded_raw_free_failure(&recovered, &fixture, "unexpected-empty-child");
    assert!(
        foreign_topology_survived,
        "recovery deleted the foreign empty child"
    );
    assert!(
        record_after == record_before,
        "recovery adopted or mutated a record with unexpected topology"
    );
    assert_no_tournament_domains();
}

#[test]
fn required_real_create_pending_rejects_populated_descendant_without_signaling_it() {
    let Some(fixture) = required_restart_fixture() else {
        return;
    };
    let tournament_uuid = Uuid::new_v4();
    let crashed = run_restart_component(
        &fixture,
        "crash-create",
        tournament_uuid,
        Some("after_tournament_create"),
        0,
    );
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "crash setup failed: stdout={} stderr={}",
        String::from_utf8_lossy(&crashed.stdout),
        String::from_utf8_lossy(&crashed.stderr)
    );
    assert!(crashed.stdout.is_empty());
    assert!(crashed.stderr.is_empty());

    let record_path = fixture.path().join("restart-record.json");
    let record_before = fs::read(&record_path).expect("read create-pending record");
    let domain = tournament_domain(tournament_uuid);
    let unexpected = domain.join("unexpected");
    let nested = unexpected.join("nested");
    fs::create_dir(&unexpected).expect("create unexpected descendant");
    fs::create_dir(&nested).expect("create nested descendant");
    let mut foreign = Command::new("/usr/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn foreign descendant");
    move_process_to_cgroup(&foreign, &nested);

    let recovered = run_restart_component(&fixture, "recover-create", tournament_uuid, None, 0);
    let foreign_survived = foreign
        .try_wait()
        .expect("inspect foreign descendant after recovery")
        .is_none();
    let foreign_topology_survived = nested.is_dir();
    let record_after = fs::read(&record_path).expect("reread rejected create record");

    reap_attack_process(&mut foreign);
    fs::remove_dir(&nested).expect("remove owned populated attack leaf");
    fs::remove_dir(&unexpected).expect("remove owned attack parent");
    fs::remove_dir(&domain).expect("remove rejected tournament domain");

    assert_bounded_raw_free_failure(&recovered, &fixture, "populated-descendant");
    assert!(foreign_survived, "recovery signaled a foreign descendant");
    assert!(
        foreign_topology_survived,
        "recovery deleted the foreign descendant topology"
    );
    assert!(
        record_after == record_before,
        "recovery adopted or mutated a record with a populated descendant"
    );
    assert_no_tournament_domains();
}

#[derive(Clone, Copy, Debug)]
enum CreatePendingTopologyAttack {
    DirectPopulation,
    WrongMode,
    WrongType,
    WrongNameSibling,
    PrematureNestedLeaf,
    StaleParentIdentity,
}

#[test]
fn required_real_create_pending_rejects_inexact_topology_matrix() {
    let attacks = [
        CreatePendingTopologyAttack::DirectPopulation,
        CreatePendingTopologyAttack::WrongMode,
        CreatePendingTopologyAttack::WrongType,
        CreatePendingTopologyAttack::WrongNameSibling,
        CreatePendingTopologyAttack::PrematureNestedLeaf,
        CreatePendingTopologyAttack::StaleParentIdentity,
    ];
    for attack in attacks {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let tournament_uuid = Uuid::new_v4();
        let (failpoint, candidate) = match attack {
            CreatePendingTopologyAttack::DirectPopulation
            | CreatePendingTopologyAttack::WrongMode
            | CreatePendingTopologyAttack::WrongType => ("after_tournament_create", 0),
            CreatePendingTopologyAttack::WrongNameSibling
            | CreatePendingTopologyAttack::PrematureNestedLeaf => {
                ("after_candidate_parent_create", 0)
            }
            CreatePendingTopologyAttack::StaleParentIdentity => ("after_control_create", 0),
        };
        let crashed = run_restart_component(
            &fixture,
            "crash-create",
            tournament_uuid,
            Some(failpoint),
            candidate,
        );
        assert_eq!(
            crashed.status.code(),
            Some(86),
            "{attack:?} crash setup failed: stdout={} stderr={}",
            String::from_utf8_lossy(&crashed.stdout),
            String::from_utf8_lossy(&crashed.stderr)
        );
        assert!(crashed.stdout.is_empty());
        assert!(crashed.stderr.is_empty());

        let record_path = fixture.path().join("restart-record.json");
        let record_before = fs::read(&record_path).expect("read attack create-pending record");
        let domain = tournament_domain(tournament_uuid);
        let parent = domain.join("candidate-0");
        let control = parent.join("control");
        let wrong_name = domain.join("candidate-wrong");
        let mut foreign = None;
        let cleanup = match attack {
            CreatePendingTopologyAttack::DirectPopulation => {
                let child = Command::new("/usr/bin/sleep")
                    .arg("30")
                    .spawn()
                    .expect("spawn directly populated attack process");
                move_process_to_cgroup(&child, &domain);
                foreign = Some(child);
                vec![domain.clone()]
            }
            CreatePendingTopologyAttack::WrongMode => {
                fs::set_permissions(&domain, fs::Permissions::from_mode(0o700))
                    .expect("change deterministic domain mode");
                vec![domain.clone()]
            }
            CreatePendingTopologyAttack::WrongType => {
                let threaded = domain.join("threaded-child");
                fs::create_dir(&threaded).expect("create threaded attack child");
                write_cgroup_leaf(&threaded, "cgroup.type", b"threaded\n");
                vec![threaded, domain.clone()]
            }
            CreatePendingTopologyAttack::WrongNameSibling => {
                fs::create_dir(&wrong_name).expect("create wrong-name candidate sibling");
                vec![wrong_name, parent, domain.clone()]
            }
            CreatePendingTopologyAttack::PrematureNestedLeaf => {
                fs::create_dir(&control).expect("create premature nested control leaf");
                vec![control, parent, domain.clone()]
            }
            CreatePendingTopologyAttack::StaleParentIdentity => {
                fs::remove_dir(&control).expect("remove original control leaf");
                fs::remove_dir(&parent).expect("remove original candidate parent");
                fs::create_dir(&parent).expect("recreate stale candidate parent");
                fs::create_dir(&control).expect("recreate control under stale parent");
                vec![control, parent, domain.clone()]
            }
        };

        let recovered =
            run_restart_component(&fixture, "recover-create", tournament_uuid, None, candidate);
        let record_after = fs::read(&record_path).expect("reread rejected topology record");
        let foreign_survived = foreign
            .as_mut()
            .map(|child| {
                child
                    .try_wait()
                    .expect("inspect directly populated attack process")
                    .is_none()
            })
            .unwrap_or(true);
        if let Some(child) = foreign.as_mut() {
            reap_attack_process(child);
        }
        for path in cleanup {
            fs::remove_dir(&path).expect("remove owned create-pending attack cgroup");
        }

        assert_bounded_raw_free_failure(&recovered, &fixture, &format!("{attack:?}"));
        assert!(
            foreign_survived,
            "{attack:?} recovery signaled foreign work"
        );
        assert!(
            record_after == record_before,
            "{attack:?} recovery adopted or mutated inexact topology"
        );
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_cleanup_edges_recover_before_and_after_abrupt_restart() {
    let candidate_edges = [
        "recovery_parent_kill",
        "recovery_parent_empty_proof",
        "payload_remove",
        "control_remove",
        "parent_remove",
    ];
    for candidate in 0_u8..2 {
        for edge in candidate_edges {
            for side in ["before", "after"] {
                let Some(fixture) = required_restart_fixture() else {
                    return;
                };
                let failpoint = format!("{side}_{edge}");
                let tournament_uuid = Uuid::new_v4();
                let crashed = run_restart_component(
                    &fixture,
                    "crash-cleanup",
                    tournament_uuid,
                    Some(&failpoint),
                    candidate,
                );
                let recovered = run_restart_component(
                    &fixture,
                    "recover-cleanup",
                    tournament_uuid,
                    None,
                    candidate,
                );
                assert_eq!(
                    crashed.status.code(),
                    Some(86),
                    "{failpoint} candidate {candidate} did not stop abruptly"
                );
                assert!(crashed.stdout.is_empty());
                assert!(crashed.stderr.is_empty());
                assert!(
                    recovered.status.success(),
                    "{failpoint} candidate {candidate} recovery failed with bounded stderr: {}",
                    String::from_utf8_lossy(&recovered.stderr)
                );
                assert_eq!(recovered.stdout, b"speculation-cleanup-recovered=1\n");
                assert!(recovered.stderr.is_empty());
                assert_no_tournament_domains();
            }
        }
    }

    for edge in ["recovery_tournament_empty_proof", "tournament_remove"] {
        for side in ["before", "after"] {
            let Some(fixture) = required_restart_fixture() else {
                return;
            };
            let failpoint = format!("{side}_{edge}");
            let tournament_uuid = Uuid::new_v4();
            let crashed = run_restart_component(
                &fixture,
                "crash-cleanup",
                tournament_uuid,
                Some(&failpoint),
                0,
            );
            let recovered =
                run_restart_component(&fixture, "recover-cleanup", tournament_uuid, None, 0);
            assert_eq!(crashed.status.code(), Some(86), "{failpoint}");
            assert!(crashed.stdout.is_empty());
            assert!(crashed.stderr.is_empty());
            assert!(
                recovered.status.success(),
                "{failpoint} recovery failed with bounded stderr: {}",
                String::from_utf8_lossy(&recovered.stderr)
            );
            assert_eq!(recovered.stdout, b"speculation-cleanup-recovered=1\n");
            assert!(recovered.stderr.is_empty());
            assert_no_tournament_domains();
        }
    }
}

#[test]
fn required_real_live_tournament_empty_proof_edges_recover_without_foreign_deletion() {
    for side in ["before", "after"] {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let failpoint = format!("{side}_tournament_empty_proof");
        let tournament_uuid = Uuid::new_v4();
        let crashed = run_restart_component(
            &fixture,
            "crash-cleanup",
            tournament_uuid,
            Some(&failpoint),
            0,
        );
        let live_pid = fs::read_to_string(fixture.path().join("live-topology-populated"))
            .expect("live cleanup observed populated topology")
            .trim()
            .parse::<u32>()
            .expect("bounded live topology pid evidence");
        assert_eq!(
            fs::read(fixture.path().join("live-topology-quiescent"))
                .expect("live cleanup observed quiescent topology"),
            b"1\n"
        );

        let cgroup_root = std::path::PathBuf::from(
            std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
                .expect("required restart run needs LTERM_SPECULATION_CGROUP_ROOT"),
        );
        let foreign_cgroup = cgroup_root.join(format!("lterm-g003-foreign-{tournament_uuid}"));
        fs::create_dir(&foreign_cgroup).expect("create foreign sibling cgroup");
        let mut foreign = Command::new("/usr/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn foreign sibling process");
        move_process_to_cgroup(&foreign, &foreign_cgroup);

        let recovered =
            run_restart_component(&fixture, "recover-cleanup", tournament_uuid, None, 0);
        let foreign_survived = foreign
            .try_wait()
            .expect("inspect foreign sibling process")
            .is_none();
        let foreign_topology_survived = foreign_cgroup.is_dir();
        reap_attack_process(&mut foreign);
        fs::remove_dir(&foreign_cgroup).expect("remove surviving foreign sibling cgroup");

        assert_eq!(
            crashed.status.code(),
            Some(86),
            "{failpoint} did not abruptly stop the live cleanup driver"
        );
        assert_bounded_raw_free_failure(&crashed, &fixture, &failpoint);

        assert!(
            recovered.status.success(),
            "{failpoint} recovery failed: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(recovered.stdout, b"speculation-cleanup-recovered=1\n");
        assert!(recovered.stderr.is_empty());
        assert_candidate_workspaces_empty(&fixture, &failpoint);
        assert!(foreign_survived, "{failpoint} signaled foreign work");
        assert!(
            foreign_topology_survived,
            "{failpoint} deleted foreign topology"
        );
        assert!(
            !std::path::Path::new("/proc")
                .join(live_pid.to_string())
                .exists(),
            "{failpoint} leaked the live topology process"
        );
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_missing_daemon_edges_die_abruptly_and_recover_separately() {
    let pids_edges = ["pids_enable_write", "pids_enable_readback"];
    for edge in pids_edges {
        for side in ["before", "after"] {
            let Some(fixture) = required_restart_fixture() else {
                return;
            };
            let failpoint = format!("{side}_{edge}");
            let tournament_uuid = Uuid::new_v4();
            let crashed = run_restart_component(
                &fixture,
                "crash-create",
                tournament_uuid,
                Some(&failpoint),
                0,
            );
            let recovered =
                run_restart_component(&fixture, "recover-create", tournament_uuid, None, 0);
            assert_eq!(crashed.status.code(), Some(86), "{failpoint}");
            assert!(crashed.stdout.is_empty(), "{failpoint} emitted stdout");
            assert!(crashed.stderr.is_empty(), "{failpoint} emitted stderr");
            assert!(
                recovered.status.success(),
                "{failpoint} recovery failed: {}",
                String::from_utf8_lossy(&recovered.stderr)
            );
            assert_eq!(
                recovered.stdout,
                b"speculation-restart-recovered=1 adopted=false\n"
            );
            assert!(recovered.stderr.is_empty());
            assert_candidate_workspaces_empty(&fixture, &failpoint);
            assert_no_tournament_domains();
        }
    }

    let runtime_edges = [
        "payload_limit_write",
        "payload_limit_readback",
        "managed_launch",
        "control_accept",
        "control_unlink",
        "argv_frame_send",
        "payload_fd_evidence",
        "payload_fd_send",
        "payload_membership_proof",
        "payload_release",
        "payload_kill",
        "payload_empty_proof",
        "parent_kill",
        "parent_empty_proof",
    ];
    for edge in runtime_edges {
        for side in ["before", "after"] {
            let Some(fixture) = required_restart_fixture() else {
                return;
            };
            let failpoint = format!("{side}_{edge}");
            let tournament_uuid = Uuid::new_v4();
            let crashed = run_restart_component(
                &fixture,
                "crash-runtime",
                tournament_uuid,
                Some(&failpoint),
                0,
            );
            let recovered =
                run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
            assert_eq!(
                crashed.status.code(),
                Some(86),
                "{failpoint} did not kill the daemon: stdout={} stderr={}",
                String::from_utf8_lossy(&crashed.stdout),
                String::from_utf8_lossy(&crashed.stderr)
            );
            assert_bounded_raw_free_failure(&crashed, &fixture, &failpoint);
            assert!(
                recovered.status.success(),
                "{failpoint} recovery failed: {}",
                String::from_utf8_lossy(&recovered.stderr)
            );
            assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
            assert!(recovered.stderr.is_empty());
            assert_candidate_workspaces_empty(&fixture, &failpoint);
            assert_no_tournament_domains();
        }
    }
}

#[test]
fn required_real_runner_abrupt_matrix_recovers_without_candidate_execution() {
    let seams = [
        "runner_before_payload_fd_receive",
        "runner_after_payload_fd_receive",
        "runner_before_payload_fd_validation",
        "runner_after_payload_fd_validation",
        "runner_before_payload_fd_ack",
        "runner_after_payload_fd_ack",
        "runner_duplicate_payload_fd_ack",
        "runner_before_candidate_fork",
        "runner_after_candidate_fork",
        "runner_before_child_placement",
        "runner_after_child_placement",
        "runner_before_payload_placed_send",
        "runner_after_payload_placed_send",
        "runner_before_release_receive",
        "runner_after_release_receive",
        "runner_before_child_exec",
        "runner_duplicate_payload_release",
        "runner_payload_release_after_rollback",
    ];
    for seam in seams {
        let Some(fixture) = required_restart_fixture() else {
            return;
        };
        let tournament_uuid = Uuid::new_v4();
        let crashed =
            run_restart_component(&fixture, "crash-runtime", tournament_uuid, Some(seam), 0);
        let recovered =
            run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
        assert_bounded_raw_free_failure(&crashed, &fixture, seam);
        assert!(
            recovered.status.success(),
            "{seam} recovery failed: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_eq!(recovered.stdout, b"speculation-runtime-recovered=1\n");
        assert!(recovered.stderr.is_empty());
        assert_candidate_workspaces_empty(&fixture, seam);
        assert_no_tournament_domains();
    }
}

#[test]
fn required_real_placement_ack_release_and_pre_exec_failpoints_cleanup() {
    let seams = [
        "runner_before_payload_fd_receive",
        "runner_after_payload_fd_receive",
        "runner_before_payload_fd_validation",
        "runner_after_payload_fd_validation",
        "runner_before_payload_fd_ack",
        "runner_after_payload_fd_ack",
        "runner_duplicate_payload_fd_ack",
        "runner_before_candidate_fork",
        "runner_after_candidate_fork",
        "runner_before_child_placement",
        "runner_after_child_placement",
        "runner_before_payload_placed_send",
        "runner_after_payload_placed_send",
        "runner_before_release_receive",
        "runner_after_release_receive",
        "runner_before_child_exec",
        "before_payload_fd_send",
        "after_payload_fd_send",
        "before_payload_membership_proof",
        "after_payload_membership_proof",
        "before_payload_release",
        "after_payload_release",
    ];
    for seam in seams {
        let Some((fixture, output, leaked_before_cleanup)) = run_required_component(Some(seam))
        else {
            return;
        };
        assert_bounded_raw_free_failure(&output, &fixture, seam);
        assert_scripted_execution_workspaces_unchanged(&fixture, seam);
        assert!(
            !leaked_before_cleanup,
            "{seam} leaked a tournament domain before test cleanup"
        );
        assert_no_tournament_domains();
    }
}

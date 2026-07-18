#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use uuid::Uuid;

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
        .env("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT", component);
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

fn assert_bounded_raw_free_failure(output: &Output, fixture: &tempfile::TempDir, seam: &str) {
    assert!(!output.status.success(), "{seam} unexpectedly succeeded");
    assert!(output.stdout.is_empty(), "{seam} emitted stdout");
    assert!(output.stderr.len() <= 512, "{seam} stderr exceeded bound");
    let stderr = String::from_utf8(output.stderr.clone()).expect("bounded UTF-8 stderr");
    assert!(
        stderr
            .lines()
            .all(|line| line.starts_with("error: speculation_")),
        "{seam} emitted an unbounded error category"
    );
    assert!(
        !stderr.contains(&fixture.path().to_string_lossy().into_owned()),
        "{seam} exposed its fixture path"
    );
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
    command.output().expect("run restart component driver")
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
    assert_eq!(output.stdout, b"speculation-real-cases=11\n");
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

#[test]
fn required_real_runner_ancillary_failpoint_is_bounded_and_cleanup_converges() {
    let Some((_fixture, output)) =
        run_required_component(Some("runner_before_payload_fd_validation"))
    else {
        return;
    };
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"error: speculation_control_io\nerror: speculation_containment_peer_rejected\n"
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
        assert_eq!(
            crashed.status.code(),
            Some(86),
            "{failpoint} candidate {candidate} did not stop abruptly"
        );
        assert!(crashed.stdout.is_empty(), "{failpoint} emitted stdout");
        assert!(crashed.stderr.is_empty(), "{failpoint} emitted stderr");

        let recovered =
            run_restart_component(&fixture, "recover-create", tournament_uuid, None, candidate);
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
                assert_eq!(
                    crashed.status.code(),
                    Some(86),
                    "{failpoint} candidate {candidate} did not stop abruptly"
                );
                assert!(crashed.stdout.is_empty());
                assert!(crashed.stderr.is_empty());
                let recovered = run_restart_component(
                    &fixture,
                    "recover-cleanup",
                    tournament_uuid,
                    None,
                    candidate,
                );
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
            assert_eq!(crashed.status.code(), Some(86), "{failpoint}");
            assert!(crashed.stdout.is_empty());
            assert!(crashed.stderr.is_empty());
            let recovered =
                run_restart_component(&fixture, "recover-cleanup", tournament_uuid, None, 0);
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
fn required_real_placement_ack_release_and_pre_exec_failpoints_cleanup() {
    let seams = [
        "runner_before_payload_fd_receive",
        "runner_after_payload_fd_receive",
        "runner_before_payload_fd_validation",
        "runner_after_payload_fd_validation",
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
        let Some((fixture, output)) = run_required_component(Some(seam)) else {
            return;
        };
        assert_bounded_raw_free_failure(&output, &fixture, seam);
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
        assert_no_tournament_domains();
    }
}

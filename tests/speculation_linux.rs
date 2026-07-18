#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Output};
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
    let Some((fixture, output)) = run_required_component(None) else {
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
fn required_real_probe_canary_abrupt_exit_leaves_no_workspace_mutation() {
    let seam = "probe_after_workspace_canary_write";
    let Some((fixture, output)) = run_required_component(Some(seam)) else {
        return;
    };
    assert_bounded_raw_free_failure(&output, &fixture, seam);
    assert_scripted_execution_workspaces_unchanged(&fixture, seam);
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
        assert_eq!(
            crashed.status.code(),
            Some(86),
            "{failpoint} did not abruptly stop the live cleanup driver"
        );
        assert_bounded_raw_free_failure(&crashed, &fixture, &failpoint);
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
            assert_eq!(crashed.status.code(), Some(86), "{failpoint}");
            assert!(crashed.stdout.is_empty(), "{failpoint} emitted stdout");
            assert!(crashed.stderr.is_empty(), "{failpoint} emitted stderr");
            let recovered =
                run_restart_component(&fixture, "recover-create", tournament_uuid, None, 0);
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
            assert_eq!(
                crashed.status.code(),
                Some(86),
                "{failpoint} did not kill the daemon: stdout={} stderr={}",
                String::from_utf8_lossy(&crashed.stdout),
                String::from_utf8_lossy(&crashed.stderr)
            );
            assert_bounded_raw_free_failure(&crashed, &fixture, &failpoint);
            let recovered =
                run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
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
        assert_bounded_raw_free_failure(&crashed, &fixture, seam);
        let recovered =
            run_restart_component(&fixture, "recover-runtime", tournament_uuid, None, 0);
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
        let Some((fixture, output)) = run_required_component(Some(seam)) else {
            return;
        };
        assert_bounded_raw_free_failure(&output, &fixture, seam);
        assert_scripted_execution_workspaces_unchanged(&fixture, seam);
        assert_no_tournament_domains();
    }
}

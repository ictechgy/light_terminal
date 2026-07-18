//! Linux-only speculation containment adapter.

use crate::speculation_fs::ValidatedDirectory;
use crate::speculation_runner::{DecisionKind, RunnerExitCategory, RunnerIdentity};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainmentErrorCode {
    Unsupported,
    InvalidIdentity,
    TopologyFailure,
    PinnedBwrapFailure,
    PeerRejected,
    DescriptorViolation,
    PlacementUnproven,
    TerminalBoundaryFailure,
    OutputLimit,
    Timeout,
    EvidenceUnavailable,
}

impl fmt::Display for ContainmentErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "speculation_containment_unsupported",
            Self::InvalidIdentity => "speculation_containment_invalid_identity",
            Self::TopologyFailure => "speculation_containment_topology_failure",
            Self::PinnedBwrapFailure => "speculation_containment_pinned_bwrap_failure",
            Self::PeerRejected => "speculation_containment_peer_rejected",
            Self::DescriptorViolation => "speculation_containment_descriptor_violation",
            Self::PlacementUnproven => "speculation_containment_placement_unproven",
            Self::TerminalBoundaryFailure => "speculation_containment_terminal_boundary_failure",
            Self::OutputLimit => "speculation_containment_output_limit",
            Self::Timeout => "speculation_containment_timeout",
            Self::EvidenceUnavailable => "speculation_containment_evidence_unavailable",
        })
    }
}

impl std::error::Error for ContainmentErrorCode {}

pub(crate) type ContainmentResult<T> = Result<T, ContainmentErrorCode>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContainmentEvent {
    Ready {
        candidate: u8,
    },
    PayloadFdAck {
        candidate: u8,
    },
    GoReceived {
        candidate: u8,
        elapsed_ns: u64,
    },
    PayloadPlaced {
        candidate: u8,
    },
    LeaderExited {
        candidate: u8,
        category: RunnerExitCategory,
    },
    OutputDrained {
        candidate: u8,
        bytes: u64,
    },
    DecisionAck {
        candidate: u8,
        decision: DecisionKind,
    },
    SyncEof {
        candidate: u8,
    },
    ManagedReaped {
        candidate: u8,
    },
}

#[derive(Debug)]
pub(crate) struct PrepareInputs {
    pub tournament_uuid: Uuid,
    pub generation: u64,
    pub source: PathBuf,
    pub candidates: [PathBuf; 2],
    pub ledger_root: PathBuf,
    pub cgroup_root: PathBuf,
    pub control_root: PathBuf,
    pub argv: Vec<OsString>,
}

pub(crate) struct LiveTournamentContext {
    identity: RunnerIdentity,
    source: ValidatedDirectory,
    candidates: [ValidatedDirectory; 2],
    ledger_root: ValidatedDirectory,
    cgroup_root: ValidatedDirectory,
    control_root: ValidatedDirectory,
    argv: Vec<Vec<u8>>,
    current_executable: PathBuf,
}

impl fmt::Debug for LiveTournamentContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveTournamentContext")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl LiveTournamentContext {
    pub(crate) fn identity(&self) -> RunnerIdentity {
        self.identity
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FixedBwrapInvocation {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
}

pub(crate) fn build_fixed_bwrap_invocation(
    candidate_path: &Path,
    control_path: &Path,
    identity: RunnerIdentity,
) -> ContainmentResult<FixedBwrapInvocation> {
    identity
        .validate()
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    validate_fixed_host_path(candidate_path)?;
    validate_fixed_host_path(control_path)?;
    let mut arguments = [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-net",
        "--unshare-ipc",
        "--unshare-uts",
        "--disable-userns",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--cap-drop",
        "ALL",
        "--sync-fd",
        "10",
        "--tmpfs",
        "/",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/sbin",
        "/sbin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--dir",
        "/home",
        "--dir",
        "/home/lterm",
        "--dir",
        "/run",
        "--dir",
        "/run/lterm-control",
        "--setenv",
        "PATH",
        "/usr/bin:/bin",
        "--setenv",
        "HOME",
        "/home/lterm",
        "--setenv",
        "TMPDIR",
        "/tmp",
        "--setenv",
        "LANG",
        "C.UTF-8",
        "--setenv",
        "LC_ALL",
        "C.UTF-8",
        "--setenv",
        "TERM",
        "xterm-256color",
        "--bind",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    arguments.push(candidate_path.as_os_str().to_owned());
    arguments.push(OsString::from("/workspace"));
    arguments.push(OsString::from("--ro-bind"));
    arguments.push(control_path.as_os_str().to_owned());
    arguments.push(OsString::from("/run/lterm-control"));
    arguments.extend(
        [
            "--chdir",
            "/workspace",
            "/run/lterm-control/lterm",
            "--internal-speculation-runner-v1",
            "--tournament",
        ]
        .into_iter()
        .map(OsString::from),
    );
    arguments.push(OsString::from(identity.tournament_uuid.to_string()));
    arguments.push(OsString::from("--candidate-index"));
    arguments.push(OsString::from(identity.candidate_index.to_string()));
    arguments.push(OsString::from("--generation"));
    arguments.push(OsString::from(identity.generation.to_string()));
    arguments.push(OsString::from("--control"));
    arguments.push(OsString::from("/run/lterm-control/control.sock"));
    Ok(FixedBwrapInvocation {
        executable: PathBuf::from("/usr/bin/bwrap"),
        arguments,
    })
}

fn validate_fixed_host_path(path: &Path) -> ContainmentResult<()> {
    use std::os::unix::ffi::OsStrExt;
    if !path.is_absolute()
        || path.as_os_str().as_bytes().is_empty()
        || path.as_os_str().as_bytes().len() > 4096
        || path.as_os_str().as_bytes().contains(&0)
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn validate_prepare(_inputs: PrepareInputs) -> ContainmentResult<LiveTournamentContext> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containment_errors_are_closed_and_raw_free() {
        assert_eq!(
            ContainmentErrorCode::PeerRejected.to_string(),
            "speculation_containment_peer_rejected"
        );
    }

    #[test]
    fn fixed_bwrap_argv_is_golden_and_forbids_host_or_caller_surface() {
        let identity = RunnerIdentity {
            tournament_uuid: Uuid::from_u128(7),
            candidate_index: 1,
            generation: 9,
        };
        let built = build_fixed_bwrap_invocation(
            Path::new("/isolated/candidate"),
            Path::new("/isolated/control"),
            identity,
        )
        .unwrap();
        assert_eq!(built.executable, Path::new("/usr/bin/bwrap"));
        let argv = built
            .arguments
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for required in [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-net",
            "--unshare-ipc",
            "--unshare-uts",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--sync-fd",
            "/workspace",
            "/run/lterm-control/lterm",
            "--internal-speculation-runner-v1",
        ] {
            assert!(
                argv.iter().any(|argument| argument == required),
                "{required}"
            );
        }
        for forbidden in [
            "--as-pid-1",
            "--unshare-all",
            "--unshare-cgroup-try",
            "--not-a-security-boundary",
            "/sys",
            "/var/run",
        ] {
            assert!(
                !argv.iter().any(|argument| argument == forbidden),
                "{forbidden}"
            );
        }
        assert_eq!(
            argv.iter()
                .filter(|argument| *argument == "/isolated/candidate")
                .count(),
            1
        );
        assert_eq!(
            argv.iter()
                .filter(|argument| *argument == "/isolated/control")
                .count(),
            1
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_prepare_fails_before_any_filesystem_mutation() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("must-not-exist");
        let result = validate_prepare(PrepareInputs {
            tournament_uuid: Uuid::from_u128(1),
            generation: 1,
            source: marker.clone(),
            candidates: [marker.clone(), marker.clone()],
            ledger_root: marker.clone(),
            cgroup_root: marker.clone(),
            control_root: marker.clone(),
            argv: vec![OsString::from("true")],
        });
        assert!(matches!(result, Err(ContainmentErrorCode::Unsupported)));
        assert!(!marker.exists());
    }
}

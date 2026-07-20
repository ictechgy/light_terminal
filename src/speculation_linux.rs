//! Linux-only speculation containment adapter.

#[cfg(all(target_os = "linux", debug_assertions))]
use crate::launch_registry::reconcile_managed_processes;
#[cfg(target_os = "linux")]
use crate::launch_registry::{
    ControlCgroupPlacement, MANAGED_SYNC_PIPE_TARGET_FD, ManagedArtifactBinding,
    ManagedArtifactCleanupStep, ManagedArtifactIdentity, ManagedAuxiliary, ManagedBoundedReap,
    ManagedCgroupDirectoryIdentity, ManagedCgroupMembership, ManagedController,
    ManagedDescendantProof, ManagedDirectoryIdentity, ManagedExecutablePolicy, ManagedKey,
    ManagedLaunchRequest, ManagedLifetimeGuard, ManagedOwnerKind, ManagedOwnerOutcome,
    ManagedOwnerRole, ManagedOwnerTag, ManagedPinnedCandidateDirectory,
    ManagedPinnedControlDirectory, ManagedPinnedControlSocket, ManagedPinnedRunner,
    ManagedPlacement, ManagedReconcileReport, ManagedStdioPolicy, ManagedWaiter, ReconcileOutcome,
    SyncPipeWrite, abort_managed_launch_reservation, acknowledge_managed_artifact_cleanup,
    begin_managed_artifact_cleanup, begin_managed_artifact_socket_retirement,
    begin_managed_artifact_unlink, drain_managed_reaper_queue_bounded,
    finish_managed_artifact_cleanup, finish_managed_artifact_create_pending_absence,
    finish_managed_artifact_logical_absence, finish_managed_artifact_socket_retirement,
    finish_managed_artifact_unlink, launch_managed_process, read_managed_artifact_binding,
    reconcile_managed_owner, reserve_managed_launch,
};
use crate::launch_registry::{
    MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD, MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD,
    MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD, MANAGED_PINNED_RUNNER_TARGET_FD,
};
use crate::speculation_fs::{DurableDirectoryIdentity, EvidenceError, ValidatedDirectory};
#[cfg(target_os = "linux")]
use crate::speculation_fs::{
    durable_identity_from_fd, open_existing_delegated_cgroup_root, open_existing_private_dir,
    open_existing_workspace_dir, validate_no_overlap,
};
use crate::speculation_ledger::ClientLedger;
#[cfg(target_os = "linux")]
use crate::speculation_registry::{
    AbsenceDisposition, CgroupForwardState, CgroupLifecycleState, ManagedOwnerEvidence,
    TournamentCgroupLifecycleState, TournamentRecoveryRecord,
};
use crate::speculation_registry::{
    CgroupComponent, ManagedOwnerRoleEvidence, PrivateCgroupRootLocator, PrivateRootIdentities,
    TournamentRecord,
};
#[cfg(target_os = "linux")]
use crate::speculation_runner::{
    ControlFrame, ControlMessage, PlacementDescriptorEvidence, PlacementDescriptorKind,
    SequenceValidator, argv_frames, receive_frame_packet, send_frame_packet,
    send_frame_with_one_fd,
};
use crate::speculation_runner::{DecisionKind, RunnerExitCategory, RunnerIdentity};
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::ffi::{CStr, CString};
use std::fmt;
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
#[cfg(all(target_os = "linux", debug_assertions))]
use std::os::unix::fs::DirBuilderExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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

pub(crate) const MAX_CONTAINMENT_ACTION_TIME: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub(crate) struct ContainmentDeadline(Instant);

impl ContainmentDeadline {
    pub(crate) fn from_now(timeout: Duration) -> ContainmentResult<Self> {
        if timeout.is_zero() || timeout > MAX_CONTAINMENT_ACTION_TIME {
            return Err(ContainmentErrorCode::Timeout);
        }
        Instant::now()
            .checked_add(timeout)
            .map(Self)
            .ok_or(ContainmentErrorCode::Timeout)
    }

    pub(crate) fn control_action() -> Self {
        Self(Instant::now() + MAX_CONTAINMENT_ACTION_TIME)
    }

    fn remaining(self) -> ContainmentResult<Duration> {
        self.0
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ContainmentErrorCode::Timeout)
    }

    fn expired(self) -> bool {
        Instant::now() >= self.0
    }

    fn instant(self) -> Instant {
        self.0
    }
}

pub(crate) const MAX_GO_RECEIPT_SKEW_NS: u64 = 50_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GoSendEvidence {
    pub candidate: u8,
    pub sent_monotonic_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GoReceiptEvidence {
    pub identity: RunnerIdentity,
    pub received_monotonic_ns: u64,
}

pub(crate) fn go_receipt_skew_ns(receipts: [GoReceiptEvidence; 2]) -> ContainmentResult<u64> {
    let [left, right] = receipts;
    if left.identity.candidate_index != 0
        || right.identity.candidate_index != 1
        || left.identity.tournament_uuid != right.identity.tournament_uuid
        || left.identity.generation != right.identity.generation
        || left.received_monotonic_ns == 0
        || right.received_monotonic_ns == 0
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let skew = left
        .received_monotonic_ns
        .abs_diff(right.received_monotonic_ns);
    if skew > MAX_GO_RECEIPT_SKEW_NS {
        return Err(ContainmentErrorCode::Timeout);
    }
    Ok(skew)
}

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
        monotonic_ns: u64,
    },
    PayloadPlaced {
        candidate: u8,
    },
    LeaderExited {
        candidate: u8,
        category: RunnerExitCategory,
        elapsed_ns: u64,
    },
    OutputLimitExceeded {
        candidate: u8,
        bytes: u64,
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
    #[cfg(target_os = "linux")]
    current_executable: RetainedExecutable,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutableIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    mode: u32,
    uid: u32,
    nlink: u64,
}

#[cfg(target_os = "linux")]
struct RetainedExecutable {
    file: File,
    identity: ExecutableIdentity,
}

#[cfg(target_os = "linux")]
impl fmt::Debug for RetainedExecutable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedExecutable")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
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

    pub(crate) fn candidate_identity(
        &self,
        candidate_index: u8,
    ) -> ContainmentResult<RunnerIdentity> {
        if candidate_index >= 2 {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        Ok(RunnerIdentity {
            candidate_index,
            ..self.identity
        })
    }

    pub(crate) fn authorize_control_generation(
        &mut self,
        generation: u64,
    ) -> ContainmentResult<RunnerIdentity> {
        if generation == 0 || generation <= self.identity.generation {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        self.identity.generation = generation;
        Ok(self.identity)
    }

    pub(crate) fn durable_record_evidence(
        &self,
    ) -> ContainmentResult<(PrivateRootIdentities, PrivateCgroupRootLocator)> {
        for directory in [
            &self.source,
            &self.candidates[0],
            &self.candidates[1],
            &self.ledger_root,
            &self.cgroup_root,
        ] {
            directory.revalidate().map_err(map_evidence)?;
        }
        Ok((
            PrivateRootIdentities {
                source: self.source.identity(),
                candidates: [self.candidates[0].identity(), self.candidates[1].identity()],
                ledger_root: self.ledger_root.identity(),
                cgroup_root: self.cgroup_root.identity(),
            },
            PrivateCgroupRootLocator::from_directory(&self.cgroup_root).map_err(map_evidence)?,
        ))
    }

    pub(crate) fn verified_ledger(&self) -> ContainmentResult<ClientLedger> {
        self.ledger_root.revalidate().map_err(map_evidence)?;
        Ok(ClientLedger::new(
            self.ledger_root
                .try_clone_validated()
                .map_err(map_evidence)?,
        ))
    }

    fn control_root_path(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(
            self.control_root.canonical_locator_bytes().to_vec(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FixedBwrapInvocation {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
}

pub(crate) fn build_fixed_bwrap_invocation(
    identity: RunnerIdentity,
) -> ContainmentResult<FixedBwrapInvocation> {
    identity
        .validate()
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
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
        "--bind-fd",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    arguments.push(OsString::from(
        MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD.to_string(),
    ));
    arguments.push(OsString::from("/workspace"));
    arguments.push(OsString::from("--ro-bind-fd"));
    arguments.push(OsString::from(
        MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD.to_string(),
    ));
    arguments.push(OsString::from("/run/lterm-control"));
    arguments.extend([
        OsString::from("--ro-bind-fd"),
        OsString::from(MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD.to_string()),
        OsString::from("/run/lterm-control/control.sock"),
        OsString::from("--ro-bind-fd"),
        OsString::from(MANAGED_PINNED_RUNNER_TARGET_FD.to_string()),
        OsString::from("/run/lterm-control/lterm"),
        OsString::from("--proc"),
        OsString::from("/proc"),
    ]);
    #[cfg(all(debug_assertions, target_os = "linux"))]
    let test_config = active_speculation_test_config();
    #[cfg(all(debug_assertions, target_os = "linux"))]
    if let Some(config) = test_config.as_ref() {
        if let Some(value) = config.failpoint.as_deref()
            && (value.as_bytes().starts_with(b"runner_")
                || value.as_bytes() == b"probe_after_workspace_canary_write")
            && value.as_bytes().len() <= 96
        {
            arguments.extend([
                OsString::from("--setenv"),
                OsString::from("LTERM_INTERNAL_SPECULATION_RUNNER_FAILPOINT"),
                value.to_os_string(),
            ]);
        }
        if config.action.as_deref() == Some(std::ffi::OsStr::new("exit")) {
            arguments.extend([
                OsString::from("--setenv"),
                OsString::from("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION"),
                OsString::from("exit"),
            ]);
        }
        if config.observe_pinned_workspace {
            arguments.extend([
                OsString::from("--setenv"),
                OsString::from("LTERM_INTERNAL_SPECULATION_OBSERVE_PINNED_WORKSPACE"),
                OsString::from("1"),
            ]);
        }
    }
    let mut runner_arguments = vec![
        OsString::from("--internal-speculation-runner-v1"),
        OsString::from("--tournament"),
        OsString::from(identity.tournament_uuid.to_string()),
        OsString::from("--candidate-index"),
        OsString::from(identity.candidate_index.to_string()),
        OsString::from("--generation"),
        OsString::from(identity.generation.to_string()),
        OsString::from("--control"),
        OsString::from("/run/lterm-control/control.sock"),
    ];
    arguments.extend([OsString::from("--chdir"), OsString::from("/workspace")]);
    #[cfg(all(debug_assertions, target_os = "linux"))]
    let delayed_runner_exec_seconds = match test_config.as_ref() {
        Some(config) if config.delayed_runner_exec_invalid => {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        Some(config) => config.delayed_runner_exec_seconds,
        None => None,
    };
    #[cfg(any(not(debug_assertions), not(target_os = "linux")))]
    let delayed_runner_exec_seconds: Option<u64> = None;
    if let Some(seconds) = delayed_runner_exec_seconds {
        arguments.extend([
            OsString::from("/usr/bin/sh"),
            OsString::from("-c"),
            OsString::from(
                "printf 'lterm-managed-stdout-marker\\n'; printf 'lterm-managed-stderr-marker\\n' >&2; exec /usr/bin/sleep \"$1\"",
            ),
            OsString::from("lterm-delayed-runner"),
            OsString::from(seconds.to_string()),
        ]);
    } else {
        arguments.push(OsString::from("/run/lterm-control/lterm"));
        arguments.append(&mut runner_arguments);
    }
    Ok(FixedBwrapInvocation {
        executable: PathBuf::from("/usr/bin/bwrap"),
        arguments,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn validate_prepare(
    _inputs: PrepareInputs,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<LiveTournamentContext> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn validate_prepare(
    inputs: PrepareInputs,
    deadline: ContainmentDeadline,
) -> ContainmentResult<LiveTournamentContext> {
    initialize_speculation_process_config();
    deadline.remaining()?;
    let identity = RunnerIdentity {
        tournament_uuid: inputs.tournament_uuid,
        candidate_index: 0,
        generation: inputs.generation,
    };
    identity
        .validate()
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    let argv = validate_exact_argv(&inputs.argv)?;
    let source = open_existing_workspace_dir(&inputs.source).map_err(map_evidence)?;
    let candidate_zero =
        open_existing_workspace_dir(&inputs.candidates[0]).map_err(map_evidence)?;
    let candidate_one = open_existing_workspace_dir(&inputs.candidates[1]).map_err(map_evidence)?;
    let ledger_root = open_existing_private_dir(&inputs.ledger_root).map_err(map_evidence)?;
    let cgroup_root =
        open_existing_delegated_cgroup_root(&inputs.cgroup_root).map_err(map_evidence)?;
    let control_root = open_existing_private_dir(&inputs.control_root).map_err(map_evidence)?;
    validate_no_overlap(&[
        &source,
        &candidate_zero,
        &candidate_one,
        &ledger_root,
        &cgroup_root,
        &control_root,
    ])
    .map_err(map_evidence)?;
    scan_workspace(&source, deadline)?;
    scan_workspace(&candidate_zero, deadline)?;
    scan_workspace(&candidate_one, deadline)?;
    let current_executable_path = configured_current_executable_path()
        .ok_or(ContainmentErrorCode::EvidenceUnavailable)?
        .canonicalize()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let current_executable = retain_current_executable(&current_executable_path)?;
    for candidate in [&candidate_zero, &candidate_one] {
        let path = Path::new(std::ffi::OsStr::from_bytes(
            candidate.canonical_locator_bytes(),
        ));
        if current_executable_path.starts_with(path) {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
    }
    validate_bwrap_path_object()?;
    for directory in [
        &source,
        &candidate_zero,
        &candidate_one,
        &ledger_root,
        &cgroup_root,
        &control_root,
    ] {
        directory.revalidate().map_err(map_evidence)?;
    }
    Ok(LiveTournamentContext {
        identity,
        source,
        candidates: [candidate_zero, candidate_one],
        ledger_root,
        cgroup_root,
        control_root,
        argv,
        current_executable,
    })
}

#[cfg(target_os = "linux")]
fn retain_current_executable(path: &Path) -> ContainmentResult<RetainedExecutable> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let identity = executable_identity(&file)?;
    validate_executable_identity(identity)?;
    let path_metadata =
        std::fs::symlink_metadata(path).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if path_metadata.dev() != identity.dev || path_metadata.ino() != identity.ino {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(RetainedExecutable { file, identity })
}

#[cfg(target_os = "linux")]
fn executable_identity(file: &File) -> ContainmentResult<ExecutableIdentity> {
    let metadata = file
        .metadata()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    Ok(ExecutableIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        len: metadata.len(),
        mode: metadata.mode(),
        uid: metadata.uid(),
        nlink: metadata.nlink(),
    })
}

#[cfg(target_os = "linux")]
fn validate_executable_identity(identity: ExecutableIdentity) -> ContainmentResult<()> {
    if identity.mode & libc::S_IFMT != libc::S_IFREG
        || identity.mode & 0o111 == 0
        || identity.mode & 0o022 != 0
        || identity.len == 0
        || identity.nlink == 0
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_retained_executable(
    source: &RetainedExecutable,
    destination: &mut File,
) -> ContainmentResult<()> {
    let before = executable_identity(&source.file)?;
    if before != source.identity {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let mut offset = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    while offset < before.len {
        let remaining = (before.len - offset).min(buffer.len() as u64) as usize;
        let read = source
            .file
            .read_at(&mut buffer[..remaining], offset)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        if read == 0 {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        destination
            .write_all(&buffer[..read])
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        offset = offset
            .checked_add(read as u64)
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    }
    if source
        .file
        .read_at(&mut buffer[..1], before.len)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        != 0
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    destination
        .sync_all()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if executable_identity(&source.file)? != before || before != source.identity {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_runner_copy(source: &RetainedExecutable, runner: &File) -> ContainmentResult<()> {
    let runner_identity = executable_identity(runner)?;
    if runner_identity.len != source.identity.len
        || runner_identity.mode & 0o7777 != 0o500
        || runner_identity.nlink != 1
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let mut source_bytes = [0_u8; 64 * 1024];
    let mut runner_bytes = [0_u8; 64 * 1024];
    let mut offset = 0_u64;
    while offset < source.identity.len {
        let remaining = (source.identity.len - offset).min(source_bytes.len() as u64) as usize;
        let source_read = source
            .file
            .read_at(&mut source_bytes[..remaining], offset)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        let runner_read = runner
            .read_at(&mut runner_bytes[..remaining], offset)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        if source_read == 0
            || source_read != runner_read
            || source_bytes[..source_read] != runner_bytes[..runner_read]
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        offset = offset
            .checked_add(source_read as u64)
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    }
    if executable_identity(&source.file)? != source.identity {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(())
}

fn map_evidence(error: EvidenceError) -> ContainmentErrorCode {
    match error {
        EvidenceError::Unsupported => ContainmentErrorCode::Unsupported,
        EvidenceError::InvalidDirectory
        | EvidenceError::InvalidIdentity
        | EvidenceError::Overlap
        | EvidenceError::Stale => ContainmentErrorCode::InvalidIdentity,
        _ => ContainmentErrorCode::EvidenceUnavailable,
    }
}

#[cfg(target_os = "linux")]
fn validate_exact_argv(arguments: &[OsString]) -> ContainmentResult<Vec<Vec<u8>>> {
    if arguments.is_empty() || arguments.len() > crate::speculation_runner::MAX_ARGV_ENTRIES {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let mut total = 0_usize;
    arguments
        .iter()
        .map(|argument| {
            let bytes = argument.as_os_str().as_bytes();
            if bytes.is_empty()
                || bytes.len() > crate::speculation_runner::MAX_ARG_BYTES
                || bytes.contains(&0)
            {
                return Err(ContainmentErrorCode::InvalidIdentity);
            }
            total = total
                .checked_add(bytes.len())
                .filter(|value| *value <= crate::speculation_runner::MAX_ARGV_BYTES)
                .ok_or(ContainmentErrorCode::InvalidIdentity)?;
            Ok(bytes.to_vec())
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn validate_bwrap_path_object() -> ContainmentResult<()> {
    let first = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/usr/bin/bwrap")
        .map_err(|_| ContainmentErrorCode::PinnedBwrapFailure)?;
    let second = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/usr/bin/bwrap")
        .map_err(|_| ContainmentErrorCode::PinnedBwrapFailure)?;
    let first_metadata = first
        .metadata()
        .map_err(|_| ContainmentErrorCode::PinnedBwrapFailure)?;
    let second_metadata = second
        .metadata()
        .map_err(|_| ContainmentErrorCode::PinnedBwrapFailure)?;
    if !first_metadata.is_file()
        || first_metadata.uid() != 0
        || first_metadata.mode() & 0o111 == 0
        || first_metadata.mode() & 0o022 != 0
        || first_metadata.nlink() != 1
        || first_metadata.dev() != second_metadata.dev()
        || first_metadata.ino() != second_metadata.ino()
    {
        return Err(ContainmentErrorCode::PinnedBwrapFailure);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn scan_workspace(
    root: &ValidatedDirectory,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    const MAX_ENTRIES: usize = 100_000;
    const MAX_DEPTH: usize = 128;
    const MAX_PATH_BYTES: usize = 4096;
    let retained = root.try_clone_retained_fd().map_err(map_evidence)?;
    let root_identity = root.identity();
    let mut entries = 0_usize;
    fn walk(
        directory: &File,
        root_identity: DurableDirectoryIdentity,
        depth: usize,
        path_bytes: usize,
        entries: &mut usize,
        deadline: ContainmentDeadline,
    ) -> ContainmentResult<()> {
        deadline.remaining()?;
        if depth > MAX_DEPTH {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            unsafe { libc::close(duplicate) };
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        let mut names = Vec::new();
        loop {
            if deadline.expired() {
                unsafe { libc::closedir(stream) };
                return Err(ContainmentErrorCode::Timeout);
            }
            unsafe { *libc::__errno_location() = 0 };
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                let errno = unsafe { *libc::__errno_location() };
                unsafe { libc::closedir(stream) };
                if errno != 0 {
                    return Err(ContainmentErrorCode::EvidenceUnavailable);
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                names.push(
                    CString::new(name.to_bytes())
                        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?,
                );
            }
        }
        for name in names {
            deadline.remaining()?;
            *entries = entries
                .checked_add(1)
                .filter(|count| *count <= MAX_ENTRIES)
                .ok_or(ContainmentErrorCode::InvalidIdentity)?;
            let next_path_bytes = path_bytes
                .checked_add(1)
                .and_then(|value| value.checked_add(name.to_bytes().len()))
                .filter(|value| *value <= MAX_PATH_BYTES)
                .ok_or(ContainmentErrorCode::InvalidIdentity)?;
            let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(ContainmentErrorCode::EvidenceUnavailable);
            }
            let stat = unsafe { stat.assume_init() };
            let kind = stat.st_mode & libc::S_IFMT;
            if stat.st_dev != root_identity.dev || stat.st_mode & 0o6000 != 0 {
                return Err(ContainmentErrorCode::InvalidIdentity);
            }
            match kind {
                libc::S_IFREG => {
                    if stat.st_nlink != 1 {
                        return Err(ContainmentErrorCode::InvalidIdentity);
                    }
                }
                libc::S_IFDIR => {
                    let child_fd = unsafe {
                        libc::openat(
                            directory.as_raw_fd(),
                            name.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    if child_fd < 0 {
                        return Err(ContainmentErrorCode::InvalidIdentity);
                    }
                    let child = unsafe { File::from_raw_fd(child_fd) };
                    let identity = durable_identity_from_fd(&child).map_err(map_evidence)?;
                    if identity.dev != root_identity.dev
                        || identity.statx_mnt_id_unique != root_identity.statx_mnt_id_unique
                    {
                        return Err(ContainmentErrorCode::InvalidIdentity);
                    }
                    walk(
                        &child,
                        root_identity,
                        depth + 1,
                        next_path_bytes,
                        entries,
                        deadline,
                    )?;
                }
                _ => return Err(ContainmentErrorCode::InvalidIdentity),
            }
        }
        Ok(())
    }
    walk(&retained, root_identity, 0, 0, &mut entries, deadline)?;
    root.revalidate().map_err(map_evidence)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TopologyAction {
    CreateTournamentDomain,
    CreateCandidateParent { candidate: u8 },
    CreateControlLeaf { candidate: u8 },
    CreatePayloadLeaf { candidate: u8 },
    ConfigurePayloadLimit { candidate: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TopologyEvidence {
    TournamentDomain(DurableDirectoryIdentity),
    CandidateParent {
        candidate: u8,
        identity: DurableDirectoryIdentity,
    },
    ControlLeaf {
        candidate: u8,
        identity: DurableDirectoryIdentity,
    },
    PayloadLeaf {
        candidate: u8,
        identity: DurableDirectoryIdentity,
    },
    PayloadLimit {
        candidate: u8,
        pids_max: u16,
    },
}

#[cfg(target_os = "linux")]
struct RetainedCgroupNode {
    file: File,
    identity: DurableDirectoryIdentity,
    membership: String,
}

#[cfg(target_os = "linux")]
impl fmt::Debug for RetainedCgroupNode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedCgroupNode")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct CandidateTopology {
    candidate_index: u8,
    parent: Option<RetainedCgroupNode>,
    control: Option<RetainedCgroupNode>,
    payload: Option<RetainedCgroupNode>,
    payload_limit_configured: bool,
}

#[cfg(target_os = "linux")]
impl CandidateTopology {
    fn empty(candidate_index: u8) -> Self {
        Self {
            candidate_index,
            parent: None,
            control: None,
            payload: None,
            payload_limit_configured: false,
        }
    }

    fn control(&self) -> ContainmentResult<&RetainedCgroupNode> {
        self.control
            .as_ref()
            .ok_or(ContainmentErrorCode::TopologyFailure)
    }

    fn payload(&self) -> ContainmentResult<&RetainedCgroupNode> {
        self.payload
            .as_ref()
            .filter(|_| self.payload_limit_configured)
            .ok_or(ContainmentErrorCode::TopologyFailure)
    }

    pub(crate) fn candidate_index(&self) -> u8 {
        self.candidate_index
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct TournamentTopology {
    tournament_uuid: Uuid,
    generation: u64,
    root: RetainedCgroupNode,
    domain: Option<RetainedCgroupNode>,
    candidates: [CandidateTopology; 2],
}

#[cfg(target_os = "linux")]
impl TournamentTopology {
    pub(crate) fn candidate(&self, candidate_index: u8) -> ContainmentResult<&CandidateTopology> {
        self.candidates
            .get(usize::from(candidate_index))
            .ok_or(ContainmentErrorCode::InvalidIdentity)
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn begin_topology(
    context: &LiveTournamentContext,
) -> ContainmentResult<TournamentTopology> {
    context.cgroup_root.revalidate().map_err(map_evidence)?;
    let file = context
        .cgroup_root
        .try_clone_retained_fd()
        .map_err(map_evidence)?;
    let membership = membership_for_cgroup_path(Path::new(std::ffi::OsStr::from_bytes(
        context.cgroup_root.canonical_locator_bytes(),
    )))?;
    let root = RetainedCgroupNode {
        file,
        identity: context.cgroup_root.identity(),
        membership,
    };
    prove_domain_task_free(&root)?;
    Ok(TournamentTopology {
        tournament_uuid: context.identity.tournament_uuid,
        generation: context.identity.generation,
        root,
        domain: None,
        candidates: [CandidateTopology::empty(0), CandidateTopology::empty(1)],
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct TournamentTopology;

#[cfg(not(target_os = "linux"))]
pub(crate) struct CandidateTopology;

#[cfg(not(target_os = "linux"))]
impl TournamentTopology {
    pub(crate) fn candidate(&self, _candidate_index: u8) -> ContainmentResult<&CandidateTopology> {
        Err(ContainmentErrorCode::Unsupported)
    }
}

#[cfg(not(target_os = "linux"))]
impl CandidateTopology {
    pub(crate) fn candidate_index(&self) -> u8 {
        0
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn begin_topology(
    _context: &LiveTournamentContext,
) -> ContainmentResult<TournamentTopology> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn create_topology(
    topology: &mut TournamentTopology,
    action: TopologyAction,
) -> ContainmentResult<TopologyEvidence> {
    match action {
        TopologyAction::CreateTournamentDomain => {
            if let Some(domain) = topology.domain.as_ref() {
                revalidate_cgroup_node(domain)?;
                enable_pids(domain)?;
                return Ok(TopologyEvidence::TournamentDomain(domain.identity));
            }
            failpoint("before_tournament_create")?;
            prove_domain_task_free(&topology.root)?;
            enable_pids(&topology.root)?;
            let name = cgroup_name(&format!("lterm-g003-{}", topology.tournament_uuid))?;
            let membership = join_membership(&topology.root.membership, name.to_bytes())?;
            let domain = create_cgroup_child(&topology.root, &name, membership)?;
            let identity = domain.identity;
            topology.domain = Some(domain);
            failpoint("after_tournament_create")?;
            enable_pids(
                topology
                    .domain
                    .as_ref()
                    .ok_or(ContainmentErrorCode::TopologyFailure)?,
            )?;
            Ok(TopologyEvidence::TournamentDomain(identity))
        }
        TopologyAction::CreateCandidateParent { candidate } => {
            let index = usize::from(candidate);
            if index >= topology.candidates.len() {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            if let Some(parent) = topology.candidates[index].parent.as_ref() {
                revalidate_cgroup_node(parent)?;
                enable_pids(parent)?;
                return Ok(TopologyEvidence::CandidateParent {
                    candidate,
                    identity: parent.identity,
                });
            }
            let domain = topology
                .domain
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            let name = cgroup_name(&format!("candidate-{candidate}"))?;
            let membership = join_membership(&domain.membership, name.to_bytes())?;
            failpoint("before_candidate_parent_create")?;
            let parent = create_cgroup_child(domain, &name, membership)?;
            let identity = parent.identity;
            topology.candidates[index].parent = Some(parent);
            failpoint("after_candidate_parent_create")?;
            enable_pids(
                topology.candidates[index]
                    .parent
                    .as_ref()
                    .ok_or(ContainmentErrorCode::TopologyFailure)?,
            )?;
            Ok(TopologyEvidence::CandidateParent {
                candidate,
                identity,
            })
        }
        TopologyAction::CreateControlLeaf { candidate } => {
            let candidate_topology = candidate_mut(topology, candidate)?;
            let parent = candidate_topology
                .parent
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            if let Some(control) = candidate_topology.control.as_ref() {
                revalidate_cgroup_node(control)?;
                return Ok(TopologyEvidence::ControlLeaf {
                    candidate,
                    identity: control.identity,
                });
            }
            let membership = join_membership(&parent.membership, b"control")?;
            failpoint("before_control_create")?;
            let control = create_cgroup_child(parent, c"control", membership)?;
            let identity = control.identity;
            candidate_topology.control = Some(control);
            failpoint("after_control_create")?;
            Ok(TopologyEvidence::ControlLeaf {
                candidate,
                identity,
            })
        }
        TopologyAction::CreatePayloadLeaf { candidate } => {
            let candidate_topology = candidate_mut(topology, candidate)?;
            let parent = candidate_topology
                .parent
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            if candidate_topology.control.is_none() {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            if let Some(payload) = candidate_topology.payload.as_ref() {
                revalidate_cgroup_node(payload)?;
                return Ok(TopologyEvidence::PayloadLeaf {
                    candidate,
                    identity: payload.identity,
                });
            }
            let membership = join_membership(&parent.membership, b"payload")?;
            failpoint("before_payload_create")?;
            let payload = create_cgroup_child(parent, c"payload", membership)?;
            let identity = payload.identity;
            candidate_topology.payload = Some(payload);
            failpoint("after_payload_create")?;
            Ok(TopologyEvidence::PayloadLeaf {
                candidate,
                identity,
            })
        }
        TopologyAction::ConfigurePayloadLimit { candidate } => {
            let candidate_topology = candidate_mut(topology, candidate)?;
            let payload = candidate_topology
                .payload
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            failpoint("before_payload_limit_write")?;
            write_leaf(payload, c"pids.max", b"256\n")?;
            failpoint("after_payload_limit_write")?;
            failpoint("before_payload_limit_readback")?;
            if read_leaf(payload, c"pids.max", 64)? != b"256\n" {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            failpoint("after_payload_limit_readback")?;
            candidate_topology.payload_limit_configured = true;
            Ok(TopologyEvidence::PayloadLimit {
                candidate,
                pids_max: 256,
            })
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn create_topology(
    _topology: &mut TournamentTopology,
    _action: TopologyAction,
) -> ContainmentResult<TopologyEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
fn candidate_mut(
    topology: &mut TournamentTopology,
    candidate: u8,
) -> ContainmentResult<&mut CandidateTopology> {
    topology
        .candidates
        .get_mut(usize::from(candidate))
        .ok_or(ContainmentErrorCode::InvalidIdentity)
}

#[cfg(target_os = "linux")]
fn membership_for_cgroup_path(path: &Path) -> ContainmentResult<String> {
    let relative = path
        .strip_prefix("/sys/fs/cgroup")
        .map_err(|_| ContainmentErrorCode::Unsupported)?;
    let bytes = relative.as_os_str().as_bytes();
    if bytes.contains(&0) || bytes.len() > 1024 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    if bytes.is_empty() {
        Ok("/".into())
    } else {
        let mut membership = Vec::with_capacity(bytes.len() + 1);
        membership.push(b'/');
        membership.extend_from_slice(bytes.strip_prefix(b"/").unwrap_or(bytes));
        String::from_utf8(membership).map_err(|_| ContainmentErrorCode::InvalidIdentity)
    }
}

#[cfg(target_os = "linux")]
fn join_membership(parent: &str, leaf: &[u8]) -> ContainmentResult<String> {
    if leaf.is_empty() || leaf.contains(&b'/') || !leaf.is_ascii() {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let joined = if parent == "/" {
        format!(
            "/{}",
            std::str::from_utf8(leaf).map_err(|_| ContainmentErrorCode::InvalidIdentity)?
        )
    } else {
        format!(
            "{parent}/{}",
            std::str::from_utf8(leaf).map_err(|_| ContainmentErrorCode::InvalidIdentity)?
        )
    };
    if joined.len() > 1024 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(joined)
}

#[cfg(target_os = "linux")]
fn cgroup_name(value: &str) -> ContainmentResult<CString> {
    if value.is_empty() || value.len() > 255 || value.contains('/') {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    CString::new(value).map_err(|_| ContainmentErrorCode::InvalidIdentity)
}

#[cfg(target_os = "linux")]
fn create_cgroup_child(
    parent: &RetainedCgroupNode,
    name: &CStr,
    membership: String,
) -> ContainmentResult<RetainedCgroupNode> {
    revalidate_cgroup_node(parent)?;
    if unsafe { libc::mkdirat(parent.file.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    let fd = unsafe {
        libc::openat(
            parent.file.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    let node = RetainedCgroupNode {
        file: unsafe { File::from_raw_fd(fd) },
        identity: DurableDirectoryIdentity {
            boot_uuid: Uuid::nil(),
            dev: 0,
            ino: 0,
            statx_mnt_id_unique: 0,
        },
        membership,
    };
    let identity = durable_identity_from_fd(&node.file).map_err(map_evidence)?;
    let node = RetainedCgroupNode { identity, ..node };
    prove_domain_task_free(&node)?;
    revalidate_cgroup_node(parent)?;
    Ok(node)
}

#[cfg(target_os = "linux")]
fn prove_domain_task_free(node: &RetainedCgroupNode) -> ContainmentResult<()> {
    revalidate_cgroup_node(node)?;
    let mut statfs = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(node.file.as_raw_fd(), statfs.as_mut_ptr()) } != 0
        || unsafe { statfs.assume_init() }.f_type as u64 != libc::CGROUP2_SUPER_MAGIC as u64
        || read_leaf(node, c"cgroup.type", 64)? != b"domain\n"
        || !read_leaf(node, c"cgroup.procs", 4096)?.is_empty()
    {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn enable_pids(node: &RetainedCgroupNode) -> ContainmentResult<()> {
    prove_domain_task_free(node)?;
    let controllers = read_leaf(node, c"cgroup.controllers", 4096)?;
    if !controllers
        .split(|byte| byte.is_ascii_whitespace())
        .any(|controller| controller == b"pids")
    {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    failpoint("before_pids_enable_write")?;
    write_leaf(node, c"cgroup.subtree_control", b"+pids\n")?;
    failpoint("after_pids_enable_write")?;
    failpoint("before_pids_enable_readback")?;
    let enabled = read_leaf(node, c"cgroup.subtree_control", 4096)?;
    if !enabled
        .split(|byte| byte.is_ascii_whitespace())
        .any(|controller| controller == b"pids")
    {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    failpoint("after_pids_enable_readback")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn revalidate_cgroup_node(node: &RetainedCgroupNode) -> ContainmentResult<()> {
    if durable_identity_from_fd(&node.file).map_err(map_evidence)? != node.identity {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_leaf(node: &RetainedCgroupNode, leaf: &CStr, cap: usize) -> ContainmentResult<Vec<u8>> {
    revalidate_cgroup_node(node)?;
    let fd = unsafe {
        libc::openat(
            node.file.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let mut bytes = Vec::with_capacity(cap.min(4096));
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ContainmentErrorCode::TopologyFailure)?;
    if bytes.len() > cap {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    revalidate_cgroup_node(node)?;
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn write_leaf(node: &RetainedCgroupNode, leaf: &CStr, bytes: &[u8]) -> ContainmentResult<()> {
    revalidate_cgroup_node(node)?;
    let fd = unsafe {
        libc::openat(
            node.file.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)
        .map_err(|_| ContainmentErrorCode::TopologyFailure)?;
    revalidate_cgroup_node(node)
}

#[cfg(target_os = "linux")]
fn failpoint(_name: &str) -> ContainmentResult<()> {
    #[cfg(debug_assertions)]
    if let Some(config) = active_speculation_test_config()
        && config.failpoint.as_deref() == Some(std::ffi::OsStr::new(_name))
    {
        if config.action.as_deref() == Some(std::ffi::OsStr::new("exit")) {
            unsafe { libc::_exit(86) };
        }
        if config.action.as_deref() == Some(std::ffi::OsStr::new("hostile-swap")) {
            return perform_internal_hostile_swap(&config);
        }
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn crash_failpoint(_name: &str) -> ContainmentResult<()> {
    #[cfg(debug_assertions)]
    if let Some(config) = active_speculation_test_config()
        && config.failpoint.as_deref() == Some(std::ffi::OsStr::new(_name))
        && config.action.as_deref() == Some(std::ffi::OsStr::new("exit"))
    {
        unsafe { libc::_exit(86) };
    }
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
#[derive(Clone, Default)]
pub(crate) struct SpeculationTestConfig {
    failpoint: Option<OsString>,
    action: Option<OsString>,
    swap_path: Option<PathBuf>,
    swap_backup: Option<PathBuf>,
    swap_kind: Option<String>,
    observe_failed_runner_lifetime: bool,
    force_reaper_spawn_failure: bool,
    observe_pinned_workspace: bool,
    delayed_runner_exec_seconds: Option<u64>,
    delayed_runner_exec_invalid: bool,
    observe_pinned_control: bool,
    observe_socket_retirement: bool,
    pub(crate) prepare_failpoint: Option<String>,
    pub(crate) fixture_root: Option<PathBuf>,
    pub(crate) cgroup_root: Option<PathBuf>,
    pub(crate) observed_run_timeout_ms: Option<u64>,
    pub(crate) observed_run_timeout_invalid: bool,
    pub(crate) client_dies_before_ledger: bool,
    pub(crate) arm_root_mismatch: bool,
    pub(crate) actor_terminal: Option<String>,
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct SpeculationProcessConfig {
    #[cfg(debug_assertions)]
    initialized_on: std::thread::ThreadId,
    current_executable_path: Option<PathBuf>,
    #[cfg(debug_assertions)]
    test: Option<SpeculationTestConfig>,
}

#[cfg(target_os = "linux")]
static SPECULATION_PROCESS_CONFIG: OnceLock<SpeculationProcessConfig> = OnceLock::new();

#[cfg(all(debug_assertions, target_os = "linux", test))]
thread_local! {
    static SPECULATION_LOCAL_TEST_CONFIG: std::cell::RefCell<Option<SpeculationTestConfig>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(debug_assertions, target_os = "linux", test))]
pub(crate) fn with_speculation_test_config<T>(
    config: SpeculationTestConfig,
    action: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<SpeculationTestConfig>);

    impl Drop for Restore {
        fn drop(&mut self) {
            SPECULATION_LOCAL_TEST_CONFIG.with(|state| {
                state.replace(self.0.take());
            });
        }
    }

    let previous = SPECULATION_LOCAL_TEST_CONFIG.with(|state| state.replace(Some(config)));
    let _restore = Restore(previous);
    action()
}

#[cfg(all(debug_assertions, target_os = "linux"))]
pub(crate) fn active_speculation_test_config() -> Option<SpeculationTestConfig> {
    #[cfg(test)]
    if let Some(config) = SPECULATION_LOCAL_TEST_CONFIG.with(|state| state.borrow().clone()) {
        return Some(config);
    }

    SPECULATION_PROCESS_CONFIG
        .get()
        .and_then(|config| config.test.clone())
}

#[cfg(target_os = "linux")]
pub(crate) fn initialize_speculation_process_config() {
    let _ = SPECULATION_PROCESS_CONFIG.get_or_init(SpeculationProcessConfig::capture);
}

#[cfg(all(debug_assertions, target_os = "linux"))]
pub(crate) fn speculation_process_config_initialized_on(expected: std::thread::ThreadId) -> bool {
    SPECULATION_PROCESS_CONFIG
        .get()
        .is_some_and(|config| config.initialized_on == expected)
}

#[cfg(target_os = "linux")]
impl SpeculationProcessConfig {
    fn capture() -> Self {
        // This is the only product-path environment snapshot. Callers invoke
        // initialize_speculation_process_config before creating service,
        // actor, recovery, or managed-reaper threads; background consumers
        // only read the immutable OnceLock value (or cfg(test) TLS override).
        #[cfg(debug_assertions)]
        let initialized_on = std::thread::current().id();
        let default_executable = std::env::current_exe().ok();
        #[cfg(all(debug_assertions, not(test)))]
        {
            let enabled = std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref()
                == Some(std::ffi::OsStr::new("1"));
            let enabled_env =
                |name: &str| std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new("1"));
            let delayed_runner_exec = enabled
                .then(|| std::env::var_os("LTERM_INTERNAL_SPECULATION_DELAY_RUNNER_EXEC_SECONDS"))
                .flatten();
            let delayed_runner_exec_seconds = delayed_runner_exec
                .as_deref()
                .and_then(std::ffi::OsStr::to_str)
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|seconds| (6..=30).contains(seconds));
            let observed_run_timeout = enabled
                .then(|| std::env::var_os("LTERM_INTERNAL_SPECULATION_OBSERVE_LEASE_MS"))
                .flatten();
            let observed_run_timeout_ms = observed_run_timeout
                .as_deref()
                .and_then(std::ffi::OsStr::to_str)
                .and_then(|value| value.parse::<u64>().ok());
            let test = enabled.then(|| SpeculationTestConfig {
                failpoint: std::env::var_os("LTERM_INTERNAL_SPECULATION_FAILPOINT"),
                action: std::env::var_os("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION"),
                swap_path: std::env::var_os("LTERM_INTERNAL_SPECULATION_SWAP_PATH")
                    .map(PathBuf::from),
                swap_backup: std::env::var_os("LTERM_INTERNAL_SPECULATION_SWAP_BACKUP")
                    .map(PathBuf::from),
                swap_kind: std::env::var("LTERM_INTERNAL_SPECULATION_SWAP_KIND").ok(),
                observe_failed_runner_lifetime: enabled_env(
                    "LTERM_INTERNAL_SPECULATION_OBSERVE_FAILED_RUNNER_LIFETIME",
                ),
                force_reaper_spawn_failure: enabled_env(
                    "LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE",
                ),
                observe_pinned_workspace: enabled_env(
                    "LTERM_INTERNAL_SPECULATION_OBSERVE_PINNED_WORKSPACE",
                ),
                delayed_runner_exec_seconds,
                delayed_runner_exec_invalid: delayed_runner_exec.is_some()
                    && delayed_runner_exec_seconds.is_none(),
                observe_pinned_control: enabled_env(
                    "LTERM_INTERNAL_SPECULATION_OBSERVE_PINNED_CONTROL",
                ),
                observe_socket_retirement: enabled_env(
                    "LTERM_INTERNAL_SPECULATION_OBSERVE_SOCKET_RETIREMENT",
                ),
                prepare_failpoint: std::env::var("LTERM_INTERNAL_SPECULATION_PREPARE_FAILPOINT")
                    .ok(),
                fixture_root: std::env::var_os("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT")
                    .map(PathBuf::from),
                cgroup_root: std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT").map(PathBuf::from),
                observed_run_timeout_ms,
                observed_run_timeout_invalid: observed_run_timeout.is_some()
                    && observed_run_timeout_ms.is_none(),
                client_dies_before_ledger: enabled_env(
                    "LTERM_INTERNAL_SPECULATION_CLIENT_DIES_BEFORE_LEDGER",
                ),
                arm_root_mismatch: enabled_env("LTERM_INTERNAL_SPECULATION_ARM_ROOT_MISMATCH"),
                actor_terminal: std::env::var("LTERM_INTERNAL_SPECULATION_ACTOR_TERMINAL").ok(),
            });
            let current_executable_path = std::env::var_os("LTERM_INTERNAL_SPECULATION_SELF_EXE")
                .filter(|_| enabled)
                .map(PathBuf::from)
                .or(default_executable);
            Self {
                #[cfg(debug_assertions)]
                initialized_on,
                current_executable_path,
                test,
            }
        }
        #[cfg(any(not(debug_assertions), test))]
        {
            Self {
                #[cfg(debug_assertions)]
                initialized_on,
                current_executable_path: default_executable,
                #[cfg(debug_assertions)]
                test: None,
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn configured_current_executable_path() -> Option<PathBuf> {
    SPECULATION_PROCESS_CONFIG
        .get()
        .and_then(|config| config.current_executable_path.clone())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn perform_internal_hostile_swap(config: &SpeculationTestConfig) -> ContainmentResult<()> {
    let path = config
        .swap_path
        .clone()
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let backup = config
        .swap_backup
        .clone()
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let kind = config
        .swap_kind
        .as_deref()
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    std::fs::rename(&path, &backup).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    match kind {
        "runner" | "owner" => {
            std::fs::write(&path, b"hostile replacement")
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            let mode = if kind == "runner" { 0o500 } else { 0o600 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        }
        "socket" => {
            let replacement = bind_seqpacket_listener(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            let holder = HOSTILE_CONTROL_LISTENER.get_or_init(|| Mutex::new(None));
            let mut holder = holder
                .lock()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            if holder.replace(replacement).is_some() {
                return Err(ContainmentErrorCode::InvalidIdentity);
            }
        }
        "directory" => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            std::fs::write(path.join("hostile"), b"retain")
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        }
        "control-directory" => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            std::fs::write(path.join("hostile"), b"retain")
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            let listener = bind_seqpacket_listener(&path.join("control.sock"))?;
            std::fs::set_permissions(
                path.join("control.sock"),
                std::fs::Permissions::from_mode(0o600),
            )
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            let holder = HOSTILE_CONTROL_LISTENER.get_or_init(|| Mutex::new(None));
            let mut holder = holder
                .lock()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            if holder.replace(listener).is_some() {
                return Err(ContainmentErrorCode::InvalidIdentity);
            }
        }
        _ => return Err(ContainmentErrorCode::InvalidIdentity),
    }
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
static HOSTILE_CONTROL_LISTENER: OnceLock<Mutex<Option<File>>> = OnceLock::new();

#[cfg(all(debug_assertions, target_os = "linux"))]
fn prove_hostile_control_listener_uncontacted() -> ContainmentResult<()> {
    let Some(holder) = HOSTILE_CONTROL_LISTENER.get() else {
        return Ok(());
    };
    let mut holder = holder
        .lock()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let Some(listener) = holder.take() else {
        return Ok(());
    };
    let mut readiness = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut readiness, 1, 0) };
    if ready < 0 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    if ready == 0 {
        let replacement = active_speculation_test_config()
            .and_then(|config| config.swap_path)
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        let marker_path = if replacement.is_dir() {
            replacement.join(".lterm-rogue-control-uncontacted")
        } else {
            replacement.with_file_name(".lterm-rogue-socket-uncontacted")
        };
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(marker_path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        marker
            .write_all(b"rogue-control-uncontacted\n")
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        marker
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        return Ok(());
    }
    if readiness.revents & libc::POLLIN == 0 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let accepted = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        )
    };
    if accepted >= 0 {
        unsafe { libc::close(accepted) };
    } else {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    Err(ContainmentErrorCode::InvalidIdentity)
}

#[cfg(all(target_os = "linux", test))]
const DURABLE_EDGE_FAILPOINTS: &[(&str, &str)] = &[
    ("before_tournament_create", "after_tournament_create"),
    ("before_pids_enable_write", "after_pids_enable_write"),
    ("before_pids_enable_readback", "after_pids_enable_readback"),
    (
        "before_candidate_parent_create",
        "after_candidate_parent_create",
    ),
    ("before_control_create", "after_control_create"),
    ("before_payload_create", "after_payload_create"),
    ("before_payload_limit_write", "after_payload_limit_write"),
    (
        "before_payload_limit_readback",
        "after_payload_limit_readback",
    ),
    (
        "before_private_control_reservation",
        "after_private_control_reservation",
    ),
    (
        "before_private_control_binding",
        "after_private_control_binding",
    ),
    (
        "before_private_runner_binding",
        "after_private_runner_binding",
    ),
    ("before_private_socket_mode", "after_private_socket_mode"),
    (
        "before_private_socket_publish",
        "after_private_socket_publish",
    ),
    (
        "before_private_socket_binding",
        "after_private_socket_binding",
    ),
    (
        "before_private_owner_publish",
        "after_private_owner_publish",
    ),
    (
        "before_private_quarantine_publish",
        "after_private_quarantine_publish",
    ),
    ("before_private_owner_unlink", "after_private_owner_unlink"),
    (
        "before_private_partial_owner_unlink",
        "after_private_partial_owner_unlink",
    ),
    (
        "before_private_socket_unlink",
        "after_private_socket_unlink",
    ),
    (
        "before_private_partial_socket_unlink",
        "after_private_partial_socket_unlink",
    ),
    (
        "before_private_runner_unlink",
        "after_private_runner_unlink",
    ),
    (
        "before_private_directory_unlink",
        "after_private_directory_unlink",
    ),
    ("before_managed_launch", "after_managed_launch"),
    ("before_control_accept", "after_control_accept"),
    ("before_control_unlink", "after_control_unlink"),
    ("before_argv_frame_send", "after_argv_frame_send"),
    ("before_payload_fd_evidence", "after_payload_fd_evidence"),
    ("before_payload_fd_send", "after_payload_fd_send"),
    (
        "before_payload_membership_proof",
        "after_payload_membership_proof",
    ),
    ("before_payload_release", "after_payload_release"),
    ("before_payload_kill", "after_payload_kill"),
    ("before_payload_empty_proof", "after_payload_empty_proof"),
    ("before_parent_kill", "after_parent_kill"),
    ("before_parent_empty_proof", "after_parent_empty_proof"),
    ("before_payload_remove", "after_payload_remove"),
    ("before_control_remove", "after_control_remove"),
    ("before_parent_remove", "after_parent_remove"),
    (
        "before_tournament_empty_proof",
        "after_tournament_empty_proof",
    ),
    ("before_tournament_remove", "after_tournament_remove"),
    ("before_recovery_parent_kill", "after_recovery_parent_kill"),
    (
        "before_recovery_parent_empty_proof",
        "after_recovery_parent_empty_proof",
    ),
    (
        "before_recovery_tournament_empty_proof",
        "after_recovery_tournament_empty_proof",
    ),
];

#[cfg(target_os = "linux")]
fn monotonic_now_ns() -> ContainmentResult<u64> {
    let mut now = std::mem::MaybeUninit::<libc::timespec>::zeroed();
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, now.as_mut_ptr()) } != 0 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let now = unsafe { now.assume_init() };
    if now.tv_sec < 0 || now.tv_nsec < 0 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    u64::try_from(now.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|seconds| seconds.checked_add(now.tv_nsec as u64))
        .ok_or(ContainmentErrorCode::EvidenceUnavailable)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeEvidence {
    pub candidate: u8,
    pub exited_zero: bool,
    pub output_bytes: u64,
    pub parent_populated_zero: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateCleanupAction {
    KillPayload,
    ProvePayloadEmpty,
    KillParent,
    ProveParentEmpty,
    RemovePayload,
    RemoveControl,
    RemoveParent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateCleanupEvidence {
    PayloadKillIssued { candidate: u8 },
    PayloadEmpty { candidate: u8 },
    ParentKillIssued { candidate: u8 },
    ParentEmpty { candidate: u8 },
    PayloadRemoved { candidate: u8 },
    ControlRemoved { candidate: u8 },
    ParentRemoved { candidate: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TournamentCleanupAction {
    ProveEmpty,
    RemoveDomain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TournamentCleanupEvidence {
    Empty,
    Removed,
}

#[cfg(target_os = "linux")]
pub(crate) struct CandidateEmptyProof {
    candidate: u8,
    payload: bool,
    node: RetainedCgroupNode,
}

#[cfg(target_os = "linux")]
pub(crate) struct TournamentEmptyProof {
    node: RetainedCgroupNode,
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct CandidateEmptyProof;

#[cfg(not(target_os = "linux"))]
pub(crate) struct TournamentEmptyProof;

#[cfg(target_os = "linux")]
pub(crate) fn prepare_candidate_empty_proof(
    tournament: &TournamentTopology,
    candidate: u8,
    payload: bool,
) -> ContainmentResult<CandidateEmptyProof> {
    let topology = tournament
        .candidates
        .get(usize::from(candidate))
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let node = if payload {
        topology.payload()?
    } else {
        topology
            .parent
            .as_ref()
            .ok_or(ContainmentErrorCode::TopologyFailure)?
    };
    Ok(CandidateEmptyProof {
        candidate,
        payload,
        node: clone_cgroup_node(node)?,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn prepare_candidate_empty_proof(
    _tournament: &TournamentTopology,
    _candidate: u8,
    _payload: bool,
) -> ContainmentResult<CandidateEmptyProof> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn prove_candidate_empty(
    proof: CandidateEmptyProof,
    deadline: ContainmentDeadline,
) -> ContainmentResult<CandidateCleanupEvidence> {
    revalidate_cgroup_node(&proof.node)?;
    failpoint(if proof.payload {
        "before_payload_empty_proof"
    } else {
        "before_parent_empty_proof"
    })?;
    wait_populated_zero(&proof.node, deadline)?;
    failpoint(if proof.payload {
        "after_payload_empty_proof"
    } else {
        "after_parent_empty_proof"
    })?;
    Ok(if proof.payload {
        CandidateCleanupEvidence::PayloadEmpty {
            candidate: proof.candidate,
        }
    } else {
        CandidateCleanupEvidence::ParentEmpty {
            candidate: proof.candidate,
        }
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn prove_candidate_empty(
    _proof: CandidateEmptyProof,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<CandidateCleanupEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn prepare_tournament_empty_proof(
    tournament: &TournamentTopology,
) -> ContainmentResult<TournamentEmptyProof> {
    let domain = tournament
        .domain
        .as_ref()
        .ok_or(ContainmentErrorCode::TopologyFailure)?;
    Ok(TournamentEmptyProof {
        node: clone_cgroup_node(domain)?,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn prepare_tournament_empty_proof(
    _tournament: &TournamentTopology,
) -> ContainmentResult<TournamentEmptyProof> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn prove_tournament_empty(
    proof: TournamentEmptyProof,
    deadline: ContainmentDeadline,
) -> ContainmentResult<TournamentCleanupEvidence> {
    revalidate_cgroup_node(&proof.node)?;
    failpoint("before_tournament_empty_proof")?;
    wait_populated_zero(&proof.node, deadline)?;
    failpoint("after_tournament_empty_proof")?;
    Ok(TournamentCleanupEvidence::Empty)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn prove_tournament_empty(
    _proof: TournamentEmptyProof,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<TournamentCleanupEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn perform_candidate_cleanup_action(
    tournament: &mut TournamentTopology,
    candidate_index: u8,
    action: CandidateCleanupAction,
    deadline: ContainmentDeadline,
) -> ContainmentResult<CandidateCleanupEvidence> {
    let index = usize::from(candidate_index);
    if index >= tournament.candidates.len() {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    match action {
        CandidateCleanupAction::KillPayload => {
            let payload = tournament.candidates[index].payload()?;
            failpoint("before_payload_kill")?;
            write_leaf(payload, c"cgroup.kill", b"1\n")?;
            failpoint("after_payload_kill")?;
            Ok(CandidateCleanupEvidence::PayloadKillIssued {
                candidate: candidate_index,
            })
        }
        CandidateCleanupAction::ProvePayloadEmpty => {
            let payload = tournament.candidates[index].payload()?;
            failpoint("before_payload_empty_proof")?;
            wait_populated_zero(payload, deadline)?;
            failpoint("after_payload_empty_proof")?;
            Ok(CandidateCleanupEvidence::PayloadEmpty {
                candidate: candidate_index,
            })
        }
        CandidateCleanupAction::KillParent => {
            let parent = tournament.candidates[index]
                .parent
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            failpoint("before_parent_kill")?;
            write_leaf(parent, c"cgroup.kill", b"1\n")?;
            failpoint("after_parent_kill")?;
            Ok(CandidateCleanupEvidence::ParentKillIssued {
                candidate: candidate_index,
            })
        }
        CandidateCleanupAction::ProveParentEmpty => {
            let parent = tournament.candidates[index]
                .parent
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            failpoint("before_parent_empty_proof")?;
            wait_populated_zero(parent, deadline)?;
            failpoint("after_parent_empty_proof")?;
            Ok(CandidateCleanupEvidence::ParentEmpty {
                candidate: candidate_index,
            })
        }
        CandidateCleanupAction::RemovePayload => {
            let candidate = &mut tournament.candidates[index];
            let parent = candidate
                .parent
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            let identity = candidate
                .payload
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?
                .identity;
            remove_cgroup_child(parent, c"payload", identity, "payload")?;
            candidate.payload = None;
            candidate.payload_limit_configured = false;
            Ok(CandidateCleanupEvidence::PayloadRemoved {
                candidate: candidate_index,
            })
        }
        CandidateCleanupAction::RemoveControl => {
            let candidate = &mut tournament.candidates[index];
            let parent = candidate
                .parent
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            let identity = candidate
                .control
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?
                .identity;
            remove_cgroup_child(parent, c"control", identity, "control")?;
            candidate.control = None;
            Ok(CandidateCleanupEvidence::ControlRemoved {
                candidate: candidate_index,
            })
        }
        CandidateCleanupAction::RemoveParent => {
            let domain = tournament
                .domain
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            let candidate = &mut tournament.candidates[index];
            if candidate.payload.is_some() || candidate.control.is_some() {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            let parent = candidate
                .parent
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            let parent_name = cgroup_name(&format!("candidate-{candidate_index}"))?;
            remove_cgroup_child(domain, &parent_name, parent.identity, "parent")?;
            candidate.parent = None;
            Ok(CandidateCleanupEvidence::ParentRemoved {
                candidate: candidate_index,
            })
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn perform_candidate_cleanup_action(
    _tournament: &mut TournamentTopology,
    _candidate_index: u8,
    _action: CandidateCleanupAction,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<CandidateCleanupEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn perform_tournament_cleanup_action(
    tournament: &mut TournamentTopology,
    action: TournamentCleanupAction,
    deadline: ContainmentDeadline,
) -> ContainmentResult<TournamentCleanupEvidence> {
    match action {
        TournamentCleanupAction::ProveEmpty => {
            let domain = tournament
                .domain
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            failpoint("before_tournament_empty_proof")?;
            wait_populated_zero(domain, deadline)?;
            failpoint("after_tournament_empty_proof")?;
            Ok(TournamentCleanupEvidence::Empty)
        }
        TournamentCleanupAction::RemoveDomain => {
            if tournament
                .candidates
                .iter()
                .any(|candidate| candidate.parent.is_some())
            {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            let domain = tournament
                .domain
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            let name = cgroup_name(&format!("lterm-g003-{}", tournament.tournament_uuid))?;
            remove_cgroup_child(&tournament.root, &name, domain.identity, "tournament")?;
            tournament.domain = None;
            Ok(TournamentCleanupEvidence::Removed)
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn perform_tournament_cleanup_action(
    _tournament: &mut TournamentTopology,
    _action: TournamentCleanupAction,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<TournamentCleanupEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryAction {
    ReconcileTournamentCreate,
    ReconcileCandidateCreate {
        candidate: u8,
        component: CgroupComponent,
    },
    ReconcileManagedOwner {
        candidate: u8,
        role: ManagedOwnerRoleEvidence,
    },
    KillParent {
        candidate: u8,
    },
    ProveParentEmpty {
        candidate: u8,
    },
    RemovePayload {
        candidate: u8,
    },
    RemoveControl {
        candidate: u8,
    },
    RemoveParent {
        candidate: u8,
    },
    ProveTournamentEmpty,
    RemoveTournamentDomain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryEvidence {
    TournamentCreateReconciled {
        identity: DurableDirectoryIdentity,
        adopted: bool,
    },
    CandidateCreateReconciled {
        candidate: u8,
        component: CgroupComponent,
        identity: DurableDirectoryIdentity,
        adopted: bool,
    },
    ManagedOwnerReconciled {
        candidate: u8,
        role: ManagedOwnerRoleEvidence,
    },
    CandidateActionComplete {
        candidate: u8,
        action: RecoveryAction,
        already_absent: bool,
    },
    TournamentActionComplete {
        action: RecoveryAction,
        already_absent: bool,
    },
    RollbackRequired,
}

/// Closed recovery surface for records from an earlier boot. No path, PID, or
/// cgroup authority is consumed by this API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OldBootRecoveryAction {
    ManagedOwner { candidate: u8 },
    CandidateComponents { candidate: u8 },
    TournamentDomain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OldBootRecoveryEvidence {
    ManagedOwnerAbsent { candidate: u8 },
    CandidateComponentsAbsent { candidate: u8 },
    TournamentDomainAbsent,
    RollbackRequired,
}

pub(crate) fn reconcile_different_boot(
    record: &TournamentRecord,
    current_private_root: DurableDirectoryIdentity,
    action: OldBootRecoveryAction,
) -> ContainmentResult<OldBootRecoveryEvidence> {
    if record.validate().is_err()
        || current_private_root.boot_uuid.is_nil()
        || current_private_root.boot_uuid == record.boot_uuid
    {
        return Ok(OldBootRecoveryEvidence::RollbackRequired);
    }
    match action {
        OldBootRecoveryAction::ManagedOwner { candidate } if candidate < 2 => {
            Ok(OldBootRecoveryEvidence::ManagedOwnerAbsent { candidate })
        }
        OldBootRecoveryAction::CandidateComponents { candidate } if candidate < 2 => {
            Ok(OldBootRecoveryEvidence::CandidateComponentsAbsent { candidate })
        }
        OldBootRecoveryAction::TournamentDomain => {
            Ok(OldBootRecoveryEvidence::TournamentDomainAbsent)
        }
        _ => Ok(OldBootRecoveryEvidence::RollbackRequired),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn reconcile_from_record(
    recovery: &TournamentRecoveryRecord,
    action: RecoveryAction,
    deadline: ContainmentDeadline,
) -> ContainmentResult<RecoveryEvidence> {
    let TournamentRecoveryRecord::Valid { record, .. } = recovery else {
        return Ok(RecoveryEvidence::RollbackRequired);
    };
    reconcile_valid_record(record, action, deadline)
}

#[cfg(target_os = "linux")]
fn reconcile_valid_record(
    record: &TournamentRecord,
    action: RecoveryAction,
    deadline: ContainmentDeadline,
) -> ContainmentResult<RecoveryEvidence> {
    if record.validate().is_err() {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    match action {
        RecoveryAction::ReconcileTournamentCreate => recover_pending_tournament_create(record),
        RecoveryAction::ReconcileCandidateCreate {
            candidate,
            component,
        } => recover_pending_candidate_create(record, candidate, component),
        RecoveryAction::ReconcileManagedOwner { candidate, role } => {
            reconcile_record_owner(record, candidate, role)
        }
        RecoveryAction::KillParent { candidate }
        | RecoveryAction::ProveParentEmpty { candidate }
        | RecoveryAction::RemovePayload { candidate }
        | RecoveryAction::RemoveControl { candidate }
        | RecoveryAction::RemoveParent { candidate } => {
            recover_candidate_action(record, action, candidate, deadline)
        }
        RecoveryAction::ProveTournamentEmpty | RecoveryAction::RemoveTournamentDomain => {
            recover_tournament_action(record, action, deadline)
        }
    }
}

#[cfg(target_os = "linux")]
fn recover_pending_tournament_create(
    record: &TournamentRecord,
) -> ContainmentResult<RecoveryEvidence> {
    if record.tournament_cgroup.lifecycle != TournamentCgroupLifecycleState::CreatePending
        || record.tournament_cgroup.domain.is_some()
        || record.cgroups.iter().any(|candidate| {
            candidate.lifecycle != CgroupLifecycleState::Forward(CgroupForwardState::Planned)
        })
    {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    let root = match reopen_recovery_root(record) {
        Ok(root) => root,
        Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
    };
    if enable_pids(&root).is_err() {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    let name = cgroup_name(&format!("lterm-g003-{}", record.status.tournament_uuid))?;
    let membership = join_membership(&root.membership, name.to_bytes())?;
    let (domain, adopted) = match create_or_adopt_cgroup_child(&root, &name, membership) {
        Ok(result) => result,
        Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
    };
    if prove_exact_recovery_subtree(&domain, &[]).is_err() || enable_pids(&domain).is_err() {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    Ok(RecoveryEvidence::TournamentCreateReconciled {
        identity: domain.identity,
        adopted,
    })
}

#[cfg(target_os = "linux")]
fn recover_pending_candidate_create(
    record: &TournamentRecord,
    candidate: u8,
    component: CgroupComponent,
) -> ContainmentResult<RecoveryEvidence> {
    let evidence = record
        .cgroups
        .get(usize::from(candidate))
        .filter(|evidence| evidence.candidate_index == candidate)
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let expected_state = match component {
        CgroupComponent::Parent => CgroupForwardState::ParentCreatePending,
        CgroupComponent::Control => CgroupForwardState::ControlCreatePending,
        CgroupComponent::Payload => CgroupForwardState::PayloadCreatePending,
    };
    if evidence.lifecycle != CgroupLifecycleState::Forward(expected_state)
        || evidence.lifecycle.same_boot_absence(component) != AbsenceDisposition::RetryCreate
    {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    let domain = match reopen_recovery_domain(record) {
        Ok(Some((_, _, domain))) => domain,
        Ok(None) | Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
    };
    let parent_name = cgroup_name(&format!("candidate-{candidate}"))?;
    let parent_membership = join_membership(&domain.membership, parent_name.to_bytes())?;

    let (node, adopted) = match component {
        CgroupComponent::Parent => {
            let pending_exists = match open_observed_cgroup_child(
                &domain,
                &parent_name,
                parent_membership.clone(),
            ) {
                Ok(observed) => observed.is_some(),
                Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
            };
            if prove_create_recovery_preflight(
                &domain,
                record,
                candidate,
                component,
                pending_exists,
            )
            .is_err()
            {
                return Ok(RecoveryEvidence::RollbackRequired);
            }
            match create_or_adopt_cgroup_child(&domain, &parent_name, parent_membership) {
                Ok(result) => result,
                Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
            }
        }
        CgroupComponent::Control => {
            let parent = match reopen_cgroup_child(
                &domain,
                &parent_name,
                parent_membership,
                evidence.parent,
            ) {
                Ok(Some(parent)) => parent,
                Ok(None) | Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
            };
            let pending_exists = match open_observed_cgroup_child(
                &parent,
                c"control",
                join_membership(&parent.membership, b"control")?,
            ) {
                Ok(observed) => observed.is_some(),
                Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
            };
            if prove_create_recovery_preflight(
                &domain,
                record,
                candidate,
                component,
                pending_exists,
            )
            .is_err()
            {
                return Ok(RecoveryEvidence::RollbackRequired);
            }
            match create_or_adopt_cgroup_child(
                &parent,
                c"control",
                join_membership(&parent.membership, b"control")?,
            ) {
                Ok(result) => result,
                Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
            }
        }
        CgroupComponent::Payload => {
            let parent = match reopen_cgroup_child(
                &domain,
                &parent_name,
                parent_membership,
                evidence.parent,
            ) {
                Ok(Some(parent)) => parent,
                Ok(None) | Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
            };
            match reopen_cgroup_child(
                &parent,
                c"control",
                join_membership(&parent.membership, b"control")?,
                evidence.control,
            ) {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
            }
            let pending_exists = match open_observed_cgroup_child(
                &parent,
                c"payload",
                join_membership(&parent.membership, b"payload")?,
            ) {
                Ok(observed) => observed.is_some(),
                Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
            };
            if prove_create_recovery_preflight(
                &domain,
                record,
                candidate,
                component,
                pending_exists,
            )
            .is_err()
            {
                return Ok(RecoveryEvidence::RollbackRequired);
            }
            match create_or_adopt_cgroup_child(
                &parent,
                c"payload",
                join_membership(&parent.membership, b"payload")?,
            ) {
                Ok(result) => result,
                Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
            }
        }
    };
    let expected = match expected_recovery_subtree(record, candidate, component, true) {
        Ok(expected) => expected,
        Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
    };
    if prove_exact_recovery_subtree(&domain, &expected).is_err() {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    if component == CgroupComponent::Parent && enable_pids(&node).is_err() {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    Ok(RecoveryEvidence::CandidateCreateReconciled {
        candidate,
        component,
        identity: node.identity,
        adopted,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn reconcile_from_record(
    _recovery: &crate::speculation_registry::TournamentRecoveryRecord,
    _action: RecoveryAction,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<RecoveryEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
fn reconcile_record_owner(
    record: &TournamentRecord,
    candidate: u8,
    role: ManagedOwnerRoleEvidence,
) -> ContainmentResult<RecoveryEvidence> {
    let Some(evidence) = record
        .managed_owners
        .iter()
        .flatten()
        .find(|evidence| evidence.candidate_index == candidate && evidence.role == role)
    else {
        return Ok(RecoveryEvidence::RollbackRequired);
    };
    let owner = ManagedOwnerTag {
        kind: ManagedOwnerKind::Speculation,
        tournament_uuid: record.status.tournament_uuid,
        candidate_index: evidence.candidate_index,
        role: match evidence.role {
            ManagedOwnerRoleEvidence::Probe => ManagedOwnerRole::Probe,
            ManagedOwnerRoleEvidence::Runner => ManagedOwnerRole::Runner,
        },
    };
    let outcome =
        reconcile_managed_owner(&owner).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    match outcome {
        ManagedOwnerOutcome::Absent => {
            Ok(RecoveryEvidence::ManagedOwnerReconciled { candidate, role })
        }
        ManagedOwnerOutcome::ResolvedTombstone(key)
            if key.slot() == evidence.slot && key.generation() == evidence.generation =>
        {
            Ok(RecoveryEvidence::ManagedOwnerReconciled { candidate, role })
        }
        ManagedOwnerOutcome::ResolvedTombstone(_) | ManagedOwnerOutcome::UnknownOrphanRisk(_) => {
            Ok(RecoveryEvidence::RollbackRequired)
        }
    }
}

#[cfg(target_os = "linux")]
fn recover_candidate_action(
    record: &TournamentRecord,
    action: RecoveryAction,
    candidate: u8,
    deadline: ContainmentDeadline,
) -> ContainmentResult<RecoveryEvidence> {
    let evidence = record
        .cgroups
        .get(usize::from(candidate))
        .filter(|evidence| evidence.candidate_index == candidate)
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    if evidence.lifecycle == CgroupLifecycleState::RollbackRequired {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    if !recovery_candidate_action_allowed(evidence.lifecycle, action) {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    let Some((_, _, domain)) = reopen_recovery_domain(record)? else {
        return Ok(RecoveryEvidence::RollbackRequired);
    };
    let parent_name = cgroup_name(&format!("candidate-{}", evidence.candidate_index))?;
    let parent_membership = join_membership(&domain.membership, parent_name.to_bytes())?;
    let parent = reopen_cgroup_child(&domain, &parent_name, parent_membership, evidence.parent)?;
    let Some(parent) = parent else {
        return Ok(
            match evidence
                .lifecycle
                .same_boot_absence(CgroupComponent::Parent)
            {
                AbsenceDisposition::RequiredNeverCreated | AbsenceDisposition::AcceptRemoval => {
                    RecoveryEvidence::CandidateActionComplete {
                        candidate,
                        action,
                        already_absent: true,
                    }
                }
                AbsenceDisposition::RetryCreate | AbsenceDisposition::Forbidden => {
                    RecoveryEvidence::RollbackRequired
                }
            },
        );
    };
    let already_absent = match action {
        RecoveryAction::KillParent { .. } => {
            failpoint("before_recovery_parent_kill")?;
            write_leaf(&parent, c"cgroup.kill", b"1\n")?;
            failpoint("after_recovery_parent_kill")?;
            false
        }
        RecoveryAction::ProveParentEmpty { .. } => {
            failpoint("before_recovery_parent_empty_proof")?;
            wait_populated_zero(&parent, deadline)?;
            failpoint("after_recovery_parent_empty_proof")?;
            false
        }
        RecoveryAction::RemovePayload { .. } => recover_remove_component(
            &parent,
            c"payload",
            evidence.payload,
            evidence
                .lifecycle
                .same_boot_absence(CgroupComponent::Payload),
            "payload",
        )?,
        RecoveryAction::RemoveControl { .. } => recover_remove_component(
            &parent,
            c"control",
            evidence.control,
            evidence
                .lifecycle
                .same_boot_absence(CgroupComponent::Control),
            "control",
        )?,
        RecoveryAction::RemoveParent { .. } => {
            remove_cgroup_child(&domain, &parent_name, parent.identity, "parent")?;
            false
        }
        _ => return Ok(RecoveryEvidence::RollbackRequired),
    };
    Ok(RecoveryEvidence::CandidateActionComplete {
        candidate,
        action,
        already_absent,
    })
}

#[cfg(target_os = "linux")]
fn recovery_candidate_action_allowed(
    lifecycle: CgroupLifecycleState,
    action: RecoveryAction,
) -> bool {
    matches!(
        (lifecycle, action),
        (
            CgroupLifecycleState::ParentKillPending { .. },
            RecoveryAction::KillParent { .. }
        ) | (
            CgroupLifecycleState::ParentKillPending { .. },
            RecoveryAction::ProveParentEmpty { .. }
        ) | (
            CgroupLifecycleState::PayloadRemovePending { .. },
            RecoveryAction::RemovePayload { .. }
        ) | (
            CgroupLifecycleState::ControlRemovePending { .. },
            RecoveryAction::RemoveControl { .. }
        ) | (
            CgroupLifecycleState::ParentRemovePending { .. },
            RecoveryAction::RemoveParent { .. }
        )
    )
}

#[cfg(target_os = "linux")]
fn recover_remove_component(
    parent: &RetainedCgroupNode,
    name: &CStr,
    expected: Option<DurableDirectoryIdentity>,
    absence: AbsenceDisposition,
    component: &'static str,
) -> ContainmentResult<bool> {
    let membership = join_membership(&parent.membership, name.to_bytes())?;
    let child = reopen_cgroup_child(parent, name, membership, expected)?;
    match child {
        Some(child) => remove_cgroup_child(parent, name, child.identity, component).map(|()| false),
        None if matches!(
            absence,
            AbsenceDisposition::RequiredNeverCreated | AbsenceDisposition::AcceptRemoval
        ) =>
        {
            Ok(true)
        }
        None => Err(ContainmentErrorCode::EvidenceUnavailable),
    }
}

#[cfg(target_os = "linux")]
fn recover_tournament_action(
    record: &TournamentRecord,
    action: RecoveryAction,
    deadline: ContainmentDeadline,
) -> ContainmentResult<RecoveryEvidence> {
    if !matches!(
        (record.tournament_cgroup.lifecycle, action),
        (
            TournamentCgroupLifecycleState::RemovePending,
            RecoveryAction::ProveTournamentEmpty | RecoveryAction::RemoveTournamentDomain
        ) | (
            TournamentCgroupLifecycleState::Removed,
            RecoveryAction::RemoveTournamentDomain
        )
    ) {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    let Some((root, name, domain)) = reopen_recovery_domain(record)? else {
        let allowed = matches!(
            record.tournament_cgroup.lifecycle,
            TournamentCgroupLifecycleState::Planned
                | TournamentCgroupLifecycleState::CreatePending
                | TournamentCgroupLifecycleState::Removed
                | TournamentCgroupLifecycleState::RemovePending
        ) && record
            .cgroups
            .iter()
            .all(|candidate| candidate.lifecycle == CgroupLifecycleState::Removed);
        return Ok(
            if allowed && action == RecoveryAction::RemoveTournamentDomain {
                RecoveryEvidence::TournamentActionComplete {
                    action,
                    already_absent: true,
                }
            } else {
                RecoveryEvidence::RollbackRequired
            },
        );
    };
    match action {
        RecoveryAction::ProveTournamentEmpty => {
            failpoint("before_recovery_tournament_empty_proof")?;
            wait_populated_zero(&domain, deadline)?;
            failpoint("after_recovery_tournament_empty_proof")?;
        }
        RecoveryAction::RemoveTournamentDomain => {
            if !record
                .cgroups
                .iter()
                .all(|candidate| candidate.lifecycle == CgroupLifecycleState::Removed)
            {
                return Ok(RecoveryEvidence::RollbackRequired);
            }
            remove_cgroup_child(&root, &name, domain.identity, "tournament")?;
        }
        _ => return Ok(RecoveryEvidence::RollbackRequired),
    }
    Ok(RecoveryEvidence::TournamentActionComplete {
        action,
        already_absent: false,
    })
}

#[cfg(target_os = "linux")]
fn reopen_recovery_domain(
    record: &TournamentRecord,
) -> ContainmentResult<Option<(RetainedCgroupNode, CString, RetainedCgroupNode)>> {
    let root_node = reopen_recovery_root(record)?;
    let tournament_name = cgroup_name(&format!("lterm-g003-{}", record.status.tournament_uuid))?;
    let membership = join_membership(&root_node.membership, tournament_name.to_bytes())?;
    let domain = reopen_cgroup_child(
        &root_node,
        &tournament_name,
        membership,
        record.tournament_cgroup.domain,
    )?;
    Ok(domain.map(|domain| (root_node, tournament_name, domain)))
}

#[cfg(target_os = "linux")]
fn reopen_recovery_root(record: &TournamentRecord) -> ContainmentResult<RetainedCgroupNode> {
    let root = record
        .cgroup_root_locator
        .reopen_and_verify()
        .map_err(map_evidence)?;
    let node = RetainedCgroupNode {
        file: root.try_clone_retained_fd().map_err(map_evidence)?,
        identity: root.identity(),
        membership: membership_for_cgroup_path(Path::new(std::ffi::OsStr::from_bytes(
            root.canonical_locator_bytes(),
        )))?,
    };
    prove_domain_task_free(&node)?;
    Ok(node)
}

#[cfg(target_os = "linux")]
fn reopen_cgroup_child(
    parent: &RetainedCgroupNode,
    name: &CStr,
    membership: String,
    expected: Option<DurableDirectoryIdentity>,
) -> ContainmentResult<Option<RetainedCgroupNode>> {
    let observed = open_observed_cgroup_child(parent, name, membership)?;
    let Some(observed) = observed else {
        return Ok(None);
    };
    if expected != Some(observed.identity) {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(Some(observed))
}

#[cfg(target_os = "linux")]
fn create_or_adopt_cgroup_child(
    parent: &RetainedCgroupNode,
    name: &CStr,
    membership: String,
) -> ContainmentResult<(RetainedCgroupNode, bool)> {
    if let Some(observed) = open_observed_cgroup_child(parent, name, membership.clone())? {
        prove_exact_recovery_subtree(&observed, &[])?;
        return Ok((observed, true));
    }
    let created = create_cgroup_child(parent, name, membership)?;
    prove_exact_recovery_subtree(&created, &[])?;
    Ok((created, false))
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct ExpectedRecoveryCgroup {
    name: CString,
    identity: Option<DurableDirectoryIdentity>,
    children: Vec<ExpectedRecoveryCgroup>,
}

#[cfg(target_os = "linux")]
fn expected_recovery_subtree(
    record: &TournamentRecord,
    pending_candidate: u8,
    pending_component: CgroupComponent,
    include_pending: bool,
) -> ContainmentResult<Vec<ExpectedRecoveryCgroup>> {
    let mut expected = Vec::new();
    for candidate in &record.cgroups {
        let is_pending = candidate.candidate_index == pending_candidate;
        let parent_pending =
            include_pending && is_pending && pending_component == CgroupComponent::Parent;
        let control_pending =
            include_pending && is_pending && pending_component == CgroupComponent::Control;
        let payload_pending =
            include_pending && is_pending && pending_component == CgroupComponent::Payload;
        if candidate.parent.is_none() && !parent_pending {
            if candidate.control.is_some()
                || candidate.payload.is_some()
                || control_pending
                || payload_pending
            {
                return Err(ContainmentErrorCode::InvalidIdentity);
            }
            continue;
        }

        let mut children = Vec::new();
        if candidate.control.is_some() || control_pending {
            children.push(ExpectedRecoveryCgroup {
                name: cgroup_name("control")?,
                identity: candidate.control,
                children: Vec::new(),
            });
        }
        if candidate.payload.is_some() || payload_pending {
            if candidate.control.is_none() {
                return Err(ContainmentErrorCode::InvalidIdentity);
            }
            children.push(ExpectedRecoveryCgroup {
                name: cgroup_name("payload")?,
                identity: candidate.payload,
                children: Vec::new(),
            });
        }
        expected.push(ExpectedRecoveryCgroup {
            name: cgroup_name(&format!("candidate-{}", candidate.candidate_index))?,
            identity: candidate.parent,
            children,
        });
    }
    Ok(expected)
}

#[cfg(target_os = "linux")]
fn prove_create_recovery_preflight(
    domain: &RetainedCgroupNode,
    record: &TournamentRecord,
    candidate: u8,
    component: CgroupComponent,
    pending_exists: bool,
) -> ContainmentResult<()> {
    let expected = expected_recovery_subtree(record, candidate, component, pending_exists)?;
    prove_exact_recovery_subtree(domain, &expected)
}

#[cfg(target_os = "linux")]
fn prove_exact_recovery_subtree(
    node: &RetainedCgroupNode,
    expected: &[ExpectedRecoveryCgroup],
) -> ContainmentResult<()> {
    prove_exact_owned_empty_domain(node)?;
    prove_exact_child_names(node, expected)?;
    for child in expected {
        let membership = join_membership(&node.membership, child.name.to_bytes())?;
        let observed = open_observed_cgroup_child(node, &child.name, membership)?
            .ok_or(ContainmentErrorCode::TopologyFailure)?;
        if child
            .identity
            .is_some_and(|identity| identity != observed.identity)
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        prove_exact_recovery_subtree(&observed, &child.children)?;
    }
    prove_exact_child_names(node, expected)?;
    prove_exact_owned_empty_domain(node)
}

#[cfg(target_os = "linux")]
fn prove_exact_owned_empty_domain(node: &RetainedCgroupNode) -> ContainmentResult<()> {
    revalidate_cgroup_node(node)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(node.file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    let stat = unsafe { stat.assume_init() };
    let mut statfs = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    if unsafe { libc::fstatfs(node.file.as_raw_fd(), statfs.as_mut_ptr()) } != 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
        || stat.st_mode & 0o7777 != 0o755
        || stat.st_uid != unsafe { libc::geteuid() }
        || stat.st_gid != unsafe { libc::getegid() }
        || unsafe { statfs.assume_init() }.f_type as u64 != libc::CGROUP2_SUPER_MAGIC as u64
        || read_leaf(node, c"cgroup.type", 64)? != b"domain\n"
        || !read_leaf(node, c"cgroup.procs", 4096)?.is_empty()
        || parse_populated(&read_leaf(node, c"cgroup.events", 4096)?)? != 0
    {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    revalidate_cgroup_node(node)
}

#[cfg(target_os = "linux")]
fn prove_exact_child_names(
    node: &RetainedCgroupNode,
    expected: &[ExpectedRecoveryCgroup],
) -> ContainmentResult<()> {
    let mut observed = enumerate_cgroup_child_names(node)?;
    let mut expected = expected
        .iter()
        .map(|child| child.name.to_bytes().to_vec())
        .collect::<Vec<_>>();
    observed.sort();
    expected.sort();
    if observed != expected {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn enumerate_cgroup_child_names(node: &RetainedCgroupNode) -> ContainmentResult<Vec<Vec<u8>>> {
    revalidate_cgroup_node(node)?;
    let independent = unsafe {
        libc::openat(
            node.file.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if independent < 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    let stream = unsafe { libc::fdopendir(independent) };
    if stream.is_null() {
        unsafe {
            libc::close(independent);
        }
        return Err(ContainmentErrorCode::TopologyFailure);
    }

    let mut children = Vec::new();
    loop {
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let errno = unsafe { *libc::__errno_location() };
            unsafe {
                libc::closedir(stream);
            }
            if errno != 0 {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        if unsafe {
            libc::fstatat(
                node.file.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            unsafe {
                libc::closedir(stream);
            }
            return Err(ContainmentErrorCode::TopologyFailure);
        }
        match unsafe { stat.assume_init() }.st_mode & libc::S_IFMT {
            libc::S_IFDIR => {
                if children.len() >= 16 {
                    unsafe {
                        libc::closedir(stream);
                    }
                    return Err(ContainmentErrorCode::TopologyFailure);
                }
                children.push(name.to_bytes().to_vec());
            }
            libc::S_IFREG => {}
            _ => {
                unsafe {
                    libc::closedir(stream);
                }
                return Err(ContainmentErrorCode::TopologyFailure);
            }
        }
    }
    revalidate_cgroup_node(node)?;
    Ok(children)
}

#[cfg(target_os = "linux")]
fn open_observed_cgroup_child(
    parent: &RetainedCgroupNode,
    name: &CStr,
    membership: String,
) -> ContainmentResult<Option<RetainedCgroupNode>> {
    revalidate_cgroup_node(parent)?;
    let fd = unsafe {
        libc::openat(
            parent.file.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(ContainmentErrorCode::TopologyFailure)
        };
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let observed = durable_identity_from_fd(&file).map_err(map_evidence)?;
    let node = RetainedCgroupNode {
        file,
        identity: observed,
        membership,
    };
    revalidate_cgroup_node(parent)?;
    Ok(Some(node))
}

#[cfg(target_os = "linux")]
fn wait_populated_zero(
    node: &RetainedCgroupNode,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    loop {
        let events = read_leaf(node, c"cgroup.events", 4096)?;
        let populated = parse_populated(&events)?;
        if populated == 0 {
            return Ok(());
        }
        deadline.remaining()?;
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn parse_populated(events: &[u8]) -> ContainmentResult<u8> {
    let text = std::str::from_utf8(events).map_err(|_| ContainmentErrorCode::TopologyFailure)?;
    text.lines()
        .find_map(|line| line.strip_prefix("populated "))
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| *value <= 1)
        .ok_or(ContainmentErrorCode::TopologyFailure)
}

#[cfg(target_os = "linux")]
fn remove_cgroup_child(
    parent: &RetainedCgroupNode,
    name: &CStr,
    expected: DurableDirectoryIdentity,
    component: &'static str,
) -> ContainmentResult<()> {
    revalidate_cgroup_node(parent)?;
    let fd = unsafe {
        libc::openat(
            parent.file.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let child = unsafe { File::from_raw_fd(fd) };
    if durable_identity_from_fd(&child).map_err(map_evidence)? != expected {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    drop(child);
    failpoint(match component {
        "payload" => "before_payload_remove",
        "control" => "before_control_remove",
        "parent" => "before_parent_remove",
        "tournament" => "before_tournament_remove",
        _ => return Err(ContainmentErrorCode::InvalidIdentity),
    })?;
    if unsafe { libc::unlinkat(parent.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    failpoint(match component {
        "payload" => "after_payload_remove",
        "control" => "after_control_remove",
        "parent" => "after_parent_remove",
        "tournament" => "after_tournament_remove",
        _ => return Err(ContainmentErrorCode::InvalidIdentity),
    })?;
    revalidate_cgroup_node(parent)
}

#[cfg(target_os = "linux")]
const PRIVATE_RUNNER_OWNER_LEAF: &CStr = c"owner.json";

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivateArtifactIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivateRunnerOwnership {
    schema_version: u32,
    owner: ManagedOwnerTag,
    slot: u16,
    generation: u64,
    binding: ManagedArtifactBinding,
    directory: DurableDirectoryIdentity,
    runner: PrivateArtifactIdentity,
    socket: PrivateArtifactIdentity,
}

#[cfg(target_os = "linux")]
struct PrivateRunnerControl {
    parent: File,
    parent_identity: DurableDirectoryIdentity,
    leaf: CString,
    directory: ValidatedDirectory,
    path: PathBuf,
    socket_path: PathBuf,
    runner_file: File,
    socket_file: File,
    listener: Option<File>,
    ownership: Option<PrivateRunnerOwnership>,
    ownership_file: Option<PrivateArtifactIdentity>,
    managed_key: Option<ManagedKey>,
    managed_binding: Option<ManagedArtifactBinding>,
    cleanup_complete: bool,
}

#[cfg(target_os = "linux")]
impl fmt::Debug for PrivateRunnerControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateRunnerControl")
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl PrivateRunnerControl {
    #[cfg(debug_assertions)]
    fn observe_authenticated_retained_control(&self) -> ContainmentResult<()> {
        if !active_speculation_test_config().is_some_and(|config| config.observe_pinned_control) {
            return Ok(());
        }
        let directory = self
            .directory
            .try_clone_retained_fd()
            .map_err(map_evidence)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c".lterm-retained-control-authenticated".as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        let mut marker = unsafe { File::from_raw_fd(fd) };
        marker
            .write_all(b"retained-control-authenticated\n")
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        marker
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        directory
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)
    }

    fn retire_socket_listener(&mut self) -> ContainmentResult<()> {
        self.directory.revalidate().map_err(map_evidence)?;
        let directory_fd = self
            .directory
            .try_clone_retained_fd()
            .map_err(map_evidence)?;
        let socket = open_validated_private_artifact_at(
            directory_fd.as_raw_fd(),
            c"control.sock",
            PrivateArtifactKind::Socket,
            self.ownership.as_ref().map(|record| record.socket),
        )?
        .ok_or(ContainmentErrorCode::PeerRejected)?;
        if !validate_private_artifact_at(
            directory_fd.as_raw_fd(),
            c"control.sock",
            PrivateArtifactKind::Socket,
            Some(artifact_identity(
                &socket
                    .metadata()
                    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
            )),
        )? {
            return Err(ContainmentErrorCode::PeerRejected);
        }
        let pending = match (self.managed_key, self.managed_binding.as_ref()) {
            (Some(key), Some(binding)) => Some(
                begin_managed_artifact_socket_retirement(key, binding)
                    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
            ),
            (None, None) => None,
            _ => return Err(ContainmentErrorCode::InvalidIdentity),
        };
        if pending
            .as_ref()
            .is_some_and(|binding| binding.socket_retired())
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        if pending.is_some() {
            failpoint("after_private_socket_retirement_intent")?;
        }
        let expected_socket = artifact_identity(
            &socket
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        );
        let final_socket = open_validated_private_artifact_at(
            directory_fd.as_raw_fd(),
            c"control.sock",
            PrivateArtifactKind::Socket,
            Some(expected_socket),
        )?
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        if artifact_identity(
            &final_socket
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        ) != expected_socket
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        self.listener.take();
        if unsafe { libc::unlinkat(directory_fd.as_raw_fd(), c"control.sock".as_ptr(), 0) } != 0 {
            return Err(ContainmentErrorCode::PeerRejected);
        }
        directory_fd
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        if socket
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            .nlink()
            != 0
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        if pending.is_some() {
            crash_failpoint("after_private_socket_retirement_physical_unlink")?;
        }
        if let (Some(key), Some(pending)) = (self.managed_key, pending) {
            self.managed_binding = Some(
                finish_managed_artifact_socket_retirement(key, &pending)
                    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
            );
            #[cfg(debug_assertions)]
            if active_speculation_test_config()
                .is_some_and(|config| config.observe_socket_retirement)
            {
                let runner = self
                    .runner_file
                    .metadata()
                    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
                let socket = socket
                    .metadata()
                    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
                let marker = self
                    .path
                    .parent()
                    .ok_or(ContainmentErrorCode::InvalidIdentity)?
                    .join("socket-retirement-receipt");
                let mut marker = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(marker)
                    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
                write!(
                    marker,
                    "socket_nlink={}\nrunner_dev={}\nrunner_ino={}\n",
                    socket.nlink(),
                    runner.dev(),
                    runner.ino()
                )
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
                marker
                    .sync_all()
                    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
                self.parent
                    .sync_all()
                    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            }
            crash_failpoint("after_private_socket_retirement_receipt")?;
        }
        self.directory.revalidate().map_err(map_evidence)
    }
}

#[cfg(target_os = "linux")]
impl Drop for PrivateRunnerControl {
    fn drop(&mut self) {
        self.listener.take();
        if self.cleanup_complete || self.managed_key.is_some() {
            return;
        }
        let _ = remove_private_runner_control_fd_relative(
            &self.parent,
            self.parent_identity,
            &self.leaf,
            &self.directory,
            PrivateControlCleanupExpectations {
                ownership: self.ownership.as_ref(),
                ownership_file: self.ownership_file,
                runner: self.ownership.as_ref().map(|record| record.runner),
                socket: self.ownership.as_ref().map(|record| record.socket),
                partial_socket_leaf: None,
                partial_owner_leaf: None,
                allow_unbound_files: true,
            },
            None,
        );
    }
}

#[cfg(target_os = "linux")]
fn managed_directory_identity(identity: DurableDirectoryIdentity) -> ManagedDirectoryIdentity {
    ManagedDirectoryIdentity {
        boot_uuid: identity.boot_uuid,
        dev: identity.dev,
        ino: identity.ino,
        statx_mnt_id_unique: identity.statx_mnt_id_unique,
    }
}

#[cfg(target_os = "linux")]
fn durable_directory_identity(identity: ManagedDirectoryIdentity) -> DurableDirectoryIdentity {
    DurableDirectoryIdentity {
        boot_uuid: identity.boot_uuid,
        dev: identity.dev,
        ino: identity.ino,
        statx_mnt_id_unique: identity.statx_mnt_id_unique,
    }
}

#[cfg(target_os = "linux")]
fn artifact_identity(metadata: &std::fs::Metadata) -> PrivateArtifactIdentity {
    PrivateArtifactIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

#[cfg(target_os = "linux")]
fn managed_artifact_identity(identity: PrivateArtifactIdentity) -> ManagedArtifactIdentity {
    ManagedArtifactIdentity {
        dev: identity.dev,
        ino: identity.ino,
    }
}

#[cfg(target_os = "linux")]
fn private_artifact_identity(identity: ManagedArtifactIdentity) -> PrivateArtifactIdentity {
    PrivateArtifactIdentity {
        dev: identity.dev,
        ino: identity.ino,
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum PrivateArtifactKind {
    Runner,
    Socket,
    PartialSocket,
    Ownership,
    PartialOwnership,
}

#[cfg(target_os = "linux")]
fn validate_private_artifact_at(
    directory_fd: RawFd,
    leaf: &CStr,
    kind: PrivateArtifactKind,
    expected: Option<PrivateArtifactIdentity>,
) -> ContainmentResult<bool> {
    open_validated_private_artifact_at(directory_fd, leaf, kind, expected)
        .map(|artifact| artifact.is_some())
}

#[cfg(target_os = "linux")]
fn open_validated_private_artifact_at(
    directory_fd: RawFd,
    leaf: &CStr,
    kind: PrivateArtifactKind,
    expected: Option<PrivateArtifactIdentity>,
) -> ContainmentResult<Option<File>> {
    let flags = match kind {
        PrivateArtifactKind::Runner
        | PrivateArtifactKind::Ownership
        | PrivateArtifactKind::PartialOwnership => {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        }
        PrivateArtifactKind::Socket | PrivateArtifactKind::PartialSocket => {
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC
        }
    };
    let fd = unsafe { libc::openat(directory_fd, leaf.as_ptr(), flags) };
    if fd < 0 {
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            if expected.is_some() {
                Err(ContainmentErrorCode::InvalidIdentity)
            } else {
                Ok(None)
            }
        } else {
            Err(ContainmentErrorCode::EvidenceUnavailable)
        };
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let valid_kind = match kind {
        PrivateArtifactKind::Runner
        | PrivateArtifactKind::Ownership
        | PrivateArtifactKind::PartialOwnership => metadata.is_file(),
        PrivateArtifactKind::Socket | PrivateArtifactKind::PartialSocket => {
            metadata.file_type().is_socket()
        }
    };
    let expected_mode = match kind {
        PrivateArtifactKind::Runner => 0o500,
        PrivateArtifactKind::Socket
        | PrivateArtifactKind::Ownership
        | PrivateArtifactKind::PartialOwnership => 0o600,
        PrivateArtifactKind::PartialSocket => metadata.mode() & 0o7777,
    };
    if !valid_kind
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != expected_mode
        || matches!(kind, PrivateArtifactKind::PartialSocket) && metadata.mode() & 0o7000 != 0
        || metadata.nlink() != 1
        || expected.is_some_and(|identity| artifact_identity(&metadata) != identity)
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(Some(file))
}

#[cfg(target_os = "linux")]
fn reopen_private_child_from_parent(
    parent: &File,
    leaf: &CStr,
    expected: DurableDirectoryIdentity,
) -> ContainmentResult<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let child = unsafe { File::from_raw_fd(fd) };
    if durable_identity_from_fd(&child).map_err(map_evidence)? != expected {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(child)
}

#[cfg(target_os = "linux")]
struct PrivateControlCleanupExpectations<'a> {
    ownership: Option<&'a PrivateRunnerOwnership>,
    ownership_file: Option<PrivateArtifactIdentity>,
    runner: Option<PrivateArtifactIdentity>,
    socket: Option<PrivateArtifactIdentity>,
    partial_socket_leaf: Option<&'a CStr>,
    partial_owner_leaf: Option<&'a CStr>,
    allow_unbound_files: bool,
}

#[cfg(target_os = "linux")]
struct ManagedPrivateCleanupProgress {
    key: ManagedKey,
    current: ManagedArtifactBinding,
}

#[cfg(target_os = "linux")]
impl ManagedPrivateCleanupProgress {
    fn new(
        key: ManagedKey,
        seed: ManagedArtifactBinding,
        current: ManagedArtifactBinding,
    ) -> ContainmentResult<Self> {
        if !same_private_artifact_binding(&seed, &current) {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        Ok(Self { key, current })
    }

    fn step_completed(&self, step: ManagedArtifactCleanupStep) -> bool {
        self.current.cleanup_step_completed(step)
    }

    fn begin(&mut self, step: ManagedArtifactCleanupStep) -> ContainmentResult<()> {
        self.current = begin_managed_artifact_unlink(self.key, &self.current, step)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        Ok(())
    }

    fn finish(&mut self, step: ManagedArtifactCleanupStep) -> ContainmentResult<()> {
        self.current = finish_managed_artifact_unlink(self.key, &self.current, step)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn remove_private_runner_control_fd_relative(
    parent: &File,
    parent_identity: DurableDirectoryIdentity,
    leaf: &CStr,
    directory: &ValidatedDirectory,
    expected: PrivateControlCleanupExpectations<'_>,
    mut managed_progress: Option<&mut ManagedPrivateCleanupProgress>,
) -> ContainmentResult<()> {
    if durable_identity_from_fd(parent).map_err(map_evidence)? != parent_identity {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    directory.revalidate().map_err(map_evidence)?;
    let directory_identity = directory.identity();
    let reopened = reopen_private_child_from_parent(parent, leaf, directory_identity)?;
    let names = directory.list_leaf_names().map_err(map_evidence)?;
    if names.iter().any(|name| {
        !matches!(name.to_bytes(), b"lterm" | b"control.sock" | b"owner.json")
            && expected
                .partial_socket_leaf
                .is_none_or(|partial| name.as_c_str() != partial)
            && expected
                .partial_owner_leaf
                .is_none_or(|partial| name.as_c_str() != partial)
    }) {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    if !expected.allow_unbound_files
        && expected.runner.is_none()
        && names.iter().any(|name| name.to_bytes() == b"control.sock")
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    if expected.ownership.is_some_and(|record| {
        expected
            .runner
            .is_some_and(|identity| identity != record.runner)
            || expected
                .socket
                .is_some_and(|identity| identity != record.socket)
    }) {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let expected_runner = expected
        .runner
        .or_else(|| expected.ownership.map(|record| record.runner));
    let expected_socket = expected
        .socket
        .or_else(|| expected.ownership.map(|record| record.socket));
    if expected
        .ownership
        .is_some_and(|record| record.directory != directory_identity)
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let ownership_completed = managed_progress
        .as_ref()
        .is_some_and(|progress| progress.step_completed(ManagedArtifactCleanupStep::Ownership));
    let socket_completed = managed_progress
        .as_ref()
        .is_some_and(|progress| progress.step_completed(ManagedArtifactCleanupStep::Socket));
    let runner_completed = managed_progress
        .as_ref()
        .is_some_and(|progress| progress.step_completed(ManagedArtifactCleanupStep::Runner));
    let socket_retired = managed_progress
        .as_ref()
        .is_some_and(|progress| progress.current.socket_retired());
    let managed_cleanup = managed_progress.is_some();
    let open_step_artifact = |name: &CStr,
                              kind: PrivateArtifactKind,
                              expected_identity: Option<PrivateArtifactIdentity>,
                              completed: bool|
     -> ContainmentResult<Option<File>> {
        if completed {
            if open_validated_private_artifact_at(reopened.as_raw_fd(), name, kind, None)?.is_some()
            {
                return Err(ContainmentErrorCode::InvalidIdentity);
            }
            return Ok(None);
        }
        let artifact = open_validated_private_artifact_at(
            reopened.as_raw_fd(),
            name,
            kind,
            expected_identity,
        )?;
        if managed_cleanup && expected_identity.is_none() && artifact.is_some() {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        Ok(artifact)
    };
    let runner = open_step_artifact(
        c"lterm",
        PrivateArtifactKind::Runner,
        expected_runner,
        runner_completed,
    )?;
    let socket = open_step_artifact(
        c"control.sock",
        PrivateArtifactKind::Socket,
        expected_socket,
        socket_completed || socket_retired,
    )?;
    let ownership_file = open_step_artifact(
        PRIVATE_RUNNER_OWNER_LEAF,
        PrivateArtifactKind::Ownership,
        expected.ownership_file,
        ownership_completed,
    )?;
    let partial_socket = expected
        .partial_socket_leaf
        .map(|leaf| {
            open_validated_private_artifact_at(
                reopened.as_raw_fd(),
                leaf,
                PrivateArtifactKind::PartialSocket,
                None,
            )
        })
        .transpose()?
        .flatten();
    let partial_owner = expected
        .partial_owner_leaf
        .map(|leaf| {
            open_validated_private_artifact_at(
                reopened.as_raw_fd(),
                leaf,
                PrivateArtifactKind::PartialOwnership,
                None,
            )
        })
        .transpose()?
        .flatten();
    if ownership_file.is_some() && partial_owner.is_some() {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    if managed_cleanup
        && (partial_socket.is_some()
            || partial_owner.is_some()
            || expected.ownership_file.is_none() && ownership_file.is_some())
    {
        // Partial socket/owner names and a final owner without a durable inode
        // binding are only pathname-shaped evidence. A same-UID actor can
        // relocate the genuine object and install an indistinguishable
        // replacement, so managed recovery must preserve them for operator
        // resolution rather than unlink or acknowledge them.
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    if managed_cleanup
        && !ownership_completed
        && expected.ownership_file.is_some()
        && ownership_file.is_none()
    {
        // Once the final owner inode is bound, absence without the durable
        // Ownership receipt is ambiguous between our unlink and relocation.
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    reopen_private_child_from_parent(parent, leaf, directory_identity)?;
    let unlink = |artifact: Option<&File>,
                  name: &CStr,
                  kind: PrivateArtifactKind,
                  before: &'static str|
     -> ContainmentResult<()> {
        let Some(artifact) = artifact else {
            return Ok(());
        };
        let identity = artifact_identity(
            &artifact
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        );
        failpoint(before)?;
        // Catch deterministic hostile swaps before mutation. A racing swap
        // after this check still cannot produce a cleanup ACK because the
        // retained owned inode must reach nlink=0 below.
        if open_validated_private_artifact_at(reopened.as_raw_fd(), name, kind, Some(identity))?
            .is_none()
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        if unsafe { libc::unlinkat(reopened.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        reopened
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        if artifact
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            .nlink()
            != 0
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        Ok(())
    };
    if !ownership_completed {
        if let Some(progress) = managed_progress.as_deref_mut() {
            progress.begin(ManagedArtifactCleanupStep::Ownership)?;
        }
        unlink(
            ownership_file.as_ref(),
            PRIVATE_RUNNER_OWNER_LEAF,
            PrivateArtifactKind::Ownership,
            "before_private_owner_unlink",
        )?;
        if let Some(partial_leaf) = expected.partial_owner_leaf {
            unlink(
                partial_owner.as_ref(),
                partial_leaf,
                PrivateArtifactKind::PartialOwnership,
                "before_private_partial_owner_unlink",
            )?;
            failpoint("after_private_partial_owner_unlink")?;
        }
        crash_failpoint("after_private_ownership_physical_unlink")?;
        if let Some(progress) = managed_progress.as_deref_mut() {
            progress.finish(ManagedArtifactCleanupStep::Ownership)?;
        }
        failpoint("after_private_owner_unlink")?;
    }
    if !socket_completed {
        if let Some(progress) = managed_progress.as_deref_mut() {
            progress.begin(ManagedArtifactCleanupStep::Socket)?;
        }
        unlink(
            socket.as_ref(),
            c"control.sock",
            PrivateArtifactKind::Socket,
            "before_private_socket_unlink",
        )?;
        if let Some(partial_leaf) = expected.partial_socket_leaf {
            unlink(
                partial_socket.as_ref(),
                partial_leaf,
                PrivateArtifactKind::PartialSocket,
                "before_private_partial_socket_unlink",
            )?;
            failpoint("after_private_partial_socket_unlink")?;
        }
        crash_failpoint("after_private_socket_physical_unlink")?;
        if let Some(progress) = managed_progress.as_deref_mut() {
            progress.finish(ManagedArtifactCleanupStep::Socket)?;
        }
        failpoint("after_private_socket_unlink")?;
    }
    if !runner_completed {
        if let Some(progress) = managed_progress.as_deref_mut() {
            progress.begin(ManagedArtifactCleanupStep::Runner)?;
        }
        unlink(
            runner.as_ref(),
            c"lterm",
            PrivateArtifactKind::Runner,
            "before_private_runner_unlink",
        )?;
        crash_failpoint("after_private_runner_physical_unlink")?;
        if let Some(progress) = managed_progress.as_deref_mut() {
            progress.finish(ManagedArtifactCleanupStep::Runner)?;
        }
        failpoint("after_private_runner_unlink")?;
    }
    drop(reopened);
    let final_reopened = reopen_private_child_from_parent(parent, leaf, directory_identity)?;
    if let Some(progress) = managed_progress.as_deref_mut() {
        progress.begin(ManagedArtifactCleanupStep::Directory)?;
    }
    failpoint("before_private_directory_unlink")?;
    reopen_private_child_from_parent(parent, leaf, directory_identity)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    parent
        .sync_all()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if durable_identity_from_fd(parent).map_err(map_evidence)? != parent_identity
        || durable_identity_from_fd(&final_reopened).map_err(map_evidence)? != directory_identity
        || final_reopened
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            .nlink()
            != 0
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    crash_failpoint("after_private_directory_unlink")?;
    if let Some(progress) = managed_progress {
        progress.current = finish_managed_artifact_cleanup(progress.key, &progress.current)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    failpoint("after_private_cleanup_completion_receipt")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn private_leaf_exists_at(parent: &File, leaf: &CStr) -> ContainmentResult<bool> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(ContainmentErrorCode::EvidenceUnavailable)
        };
    }
    drop(unsafe { File::from_raw_fd(fd) });
    Ok(true)
}

#[cfg(target_os = "linux")]
fn same_private_artifact_binding(
    ownership: &ManagedArtifactBinding,
    cleanup: &ManagedArtifactBinding,
) -> bool {
    ownership.nonce() == cleanup.nonce()
        && ownership.control_root() == cleanup.control_root()
        && ownership.private_leaf() == cleanup.private_leaf()
        && ownership.private_directory() == cleanup.private_directory()
        && ownership.runner() == cleanup.runner()
        && ownership.socket() == cleanup.socket()
}

#[cfg(target_os = "linux")]
fn validate_private_runner_ownership_against_binding(
    record: &PrivateRunnerOwnership,
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
    expected_owner: Option<&ManagedOwnerTag>,
    resolved_aliases: Option<&[&crate::launch_registry::ManagedReconcileEntry]>,
) -> ContainmentResult<()> {
    let expected_directory = binding
        .private_directory()
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let expected_runner = binding
        .runner()
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let expected_socket = binding
        .socket()
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    if record.slot != key.slot()
        || record.generation != key.generation()
        || record.directory != durable_directory_identity(expected_directory)
        || record.runner != private_artifact_identity(expected_runner)
        || record.socket != private_artifact_identity(expected_socket)
        || record.binding.cleanup_quarantine().is_some()
        || !record.binding.owner_create_pending()
        || binding.owner_create_pending()
        || !same_private_artifact_binding(&record.binding, binding)
        || expected_owner.is_some_and(|owner| owner != &record.owner)
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    if let Some(aliases) = resolved_aliases
        && !aliases.iter().any(|entry| {
            entry.owner.as_ref() == Some(&record.owner)
                && entry.key == Some(key)
                && entry.outcome == ReconcileOutcome::ResolvedTombstone
        })
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn cleanup_bound_private_runner_control(
    parent: &File,
    parent_identity: DurableDirectoryIdentity,
    parent_path: &Path,
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
    expected_owner: Option<&ManagedOwnerTag>,
    resolved_aliases: Option<&[&crate::launch_registry::ManagedReconcileEntry]>,
) -> ContainmentResult<()> {
    let owner = expected_owner.ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let Some(mut authoritative) = read_managed_artifact_binding(key, owner)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
    else {
        return Ok(());
    };
    if !same_private_artifact_binding(binding, &authoritative) {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    if authoritative.creation_pending() {
        // Before an exact runner/socket/owner inode is durably bound,
        // disappearance of its physical name is indistinguishable from
        // hostile relocation.
        // Preserve the directory and binding for operator resolution.
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    if authoritative.control_root().boot_uuid != parent_identity.boot_uuid {
        let completed = finish_managed_artifact_logical_absence(key, &authoritative)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        return acknowledge_managed_artifact_cleanup(key, &completed)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable);
    }
    if durable_identity_from_fd(parent).map_err(map_evidence)? != parent_identity
        || binding.control_root() != managed_directory_identity(parent_identity)
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let reopened_parent = open_existing_private_dir(parent_path).map_err(map_evidence)?;
    if reopened_parent.identity() != parent_identity {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }

    if authoritative.socket_retire_pending() {
        let source_leaf = CString::new(authoritative.private_leaf())
            .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
        let expected_directory = authoritative
            .private_directory()
            .map(durable_directory_identity)
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        let directory = reopen_private_child_from_parent(parent, &source_leaf, expected_directory)?;
        let directory_fd = directory
            .try_clone()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        let expected_socket = authoritative
            .socket()
            .map(private_artifact_identity)
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        let socket = open_validated_private_artifact_at(
            directory_fd.as_raw_fd(),
            c"control.sock",
            PrivateArtifactKind::Socket,
            Some(expected_socket),
        )?
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        if unsafe { libc::unlinkat(directory_fd.as_raw_fd(), c"control.sock".as_ptr(), 0) } != 0 {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        directory_fd
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        if socket
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            .nlink()
            != 0
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        crash_failpoint("after_private_socket_retirement_physical_unlink")?;
        authoritative = finish_managed_artifact_socket_retirement(key, &authoritative)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }

    // The quarantine name must be durable before any mutation.  It is derived
    // from the externally bound nonce, so a restart can distinguish the owned
    // directory from a replacement installed at the live leaf.
    let cleanup_binding = begin_managed_artifact_cleanup(key, &authoritative)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if cleanup_binding.cleanup_completed() {
        return acknowledge_managed_artifact_cleanup(key, &cleanup_binding)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable);
    }
    let quarantine_text = cleanup_binding
        .cleanup_quarantine()
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let source_leaf = CString::new(cleanup_binding.private_leaf())
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    let quarantine_leaf =
        CString::new(quarantine_text).map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    let expected_directory = cleanup_binding.private_directory();
    let expected_durable = expected_directory.map(durable_directory_identity);
    let quarantine_present = private_leaf_exists_at(parent, &quarantine_leaf)?;

    if quarantine_present {
        let expected = expected_durable.ok_or(ContainmentErrorCode::InvalidIdentity)?;
        reopen_private_child_from_parent(parent, &quarantine_leaf, expected)?;
    } else if private_leaf_exists_at(parent, &source_leaf)? {
        let expected = expected_durable.ok_or(ContainmentErrorCode::InvalidIdentity)?;
        let source = reopen_private_child_from_parent(parent, &source_leaf, expected)?;
        failpoint("before_private_quarantine_publish")?;
        if unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                parent.as_raw_fd(),
                source_leaf.as_ptr(),
                parent.as_raw_fd(),
                quarantine_leaf.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        parent
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        failpoint("after_private_quarantine_publish")?;
        if durable_identity_from_fd(parent).map_err(map_evidence)? != parent_identity
            || durable_identity_from_fd(&source).map_err(map_evidence)? != expected
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        reopen_private_child_from_parent(parent, &quarantine_leaf, expected)?;
    } else {
        if cleanup_binding.private_directory().is_none()
            && cleanup_binding.runner().is_none()
            && cleanup_binding.socket().is_none()
        {
            let completed = finish_managed_artifact_create_pending_absence(key, &cleanup_binding)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            return acknowledge_managed_artifact_cleanup(key, &completed)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable);
        }
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }

    let expected = expected_durable.ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let quarantine_path = parent_path.join(std::ffi::OsStr::from_bytes(quarantine_leaf.to_bytes()));
    let directory = open_existing_private_dir(&quarantine_path).map_err(map_evidence)?;
    if directory.identity() != expected {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let ownership = read_private_runner_ownership(&directory)?;
    if let Some((record, _)) = &ownership {
        validate_private_runner_ownership_against_binding(
            record,
            key,
            &cleanup_binding,
            expected_owner,
            resolved_aliases,
        )?;
    }
    let expected_runner = cleanup_binding.runner().map(private_artifact_identity);
    let expected_socket = cleanup_binding.socket().map(private_artifact_identity);
    let partial_socket_leaf = cleanup_binding
        .socket_create_pending()
        .then(|| private_socket_temp_leaf(cleanup_binding.nonce()));
    let partial_owner_leaf = cleanup_binding.owner_create_pending().then(|| {
        CString::new(format!(
            ".owner.json.create-{}",
            cleanup_binding.nonce().simple()
        ))
        .expect("UUID-derived private owner leaf")
    });
    let expected_ownership_file = cleanup_binding.owner_file().map(private_artifact_identity);
    let mut cleanup_progress =
        ManagedPrivateCleanupProgress::new(key, binding.clone(), cleanup_binding)?;
    remove_private_runner_control_fd_relative(
        parent,
        parent_identity,
        &quarantine_leaf,
        &directory,
        PrivateControlCleanupExpectations {
            ownership: ownership.as_ref().map(|(record, _)| record),
            ownership_file: expected_ownership_file,
            runner: expected_runner,
            socket: expected_socket,
            partial_socket_leaf: partial_socket_leaf.as_deref(),
            partial_owner_leaf: partial_owner_leaf.as_deref(),
            allow_unbound_files: false,
        },
        Some(&mut cleanup_progress),
    )?;
    if durable_identity_from_fd(parent).map_err(map_evidence)? != parent_identity {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    acknowledge_managed_artifact_cleanup(key, &cleanup_progress.current)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    failpoint("after_private_cleanup_ack")
}

#[cfg(target_os = "linux")]
struct RunnerLifetime {
    private_control: Mutex<PrivateRunnerControl>,
}

#[cfg(target_os = "linux")]
struct BoundPrivateArtifactCleanup {
    parent_identity: DurableDirectoryIdentity,
    parent_path: PathBuf,
    private_leaf: String,
    key: ManagedKey,
    owner: ManagedOwnerTag,
}

#[cfg(target_os = "linux")]
impl BoundPrivateArtifactCleanup {
    fn cleanup(&self) -> anyhow::Result<bool> {
        let Some(binding) = read_managed_artifact_binding(self.key, &self.owner)? else {
            return Ok(true);
        };
        if binding.control_root() != managed_directory_identity(self.parent_identity)
            || binding.private_leaf() != self.private_leaf
        {
            anyhow::bail!("prepared private artifact authority mismatch");
        }
        let parent_directory = open_existing_private_dir(&self.parent_path)
            .map_err(|code| anyhow::anyhow!("prepared private parent reopen failed: {code}"))?;
        if parent_directory.identity() != self.parent_identity {
            anyhow::bail!("prepared private parent identity changed");
        }
        let parent = parent_directory
            .try_clone_retained_fd()
            .map_err(|error| anyhow::anyhow!("prepared private parent clone failed: {error}"))?;
        cleanup_bound_private_runner_control(
            &parent,
            self.parent_identity,
            &self.parent_path,
            self.key,
            &binding,
            Some(&self.owner),
            None,
        )
        .map_err(|code| anyhow::anyhow!("prepared private artifact cleanup failed: {code}"))?;
        Ok(true)
    }
}

#[cfg(target_os = "linux")]
fn abort_prepared_runner_reservation(
    context: &LiveTournamentContext,
    owner: ManagedOwnerTag,
    reservation: crate::launch_registry::ManagedLaunchReservation,
) {
    let private_leaf = format!(
        "lterm-g003-{}-candidate-{}",
        owner.tournament_uuid, owner.candidate_index
    );
    let cleanup = Arc::new(BoundPrivateArtifactCleanup {
        parent_identity: context.control_root.identity(),
        parent_path: context.control_root_path(),
        private_leaf,
        key: reservation.key(),
        owner,
    });
    let callback = Arc::clone(&cleanup);
    abort_managed_launch_reservation(
        reservation,
        ManagedLifetimeGuard::with_cleanup(cleanup, move || callback.cleanup()),
    );
    let _ = drain_managed_reaper_queue_bounded(Duration::from_secs(2));
}

#[cfg(target_os = "linux")]
impl RunnerLifetime {
    fn new(private_control: PrivateRunnerControl) -> Self {
        Self {
            private_control: Mutex::new(private_control),
        }
    }

    fn private_control(&self) -> std::sync::MutexGuard<'_, PrivateRunnerControl> {
        self.private_control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cleanup_managed_artifacts(&self) -> anyhow::Result<bool> {
        let mut private_control = self.private_control();
        if private_control.cleanup_complete {
            return Ok(true);
        }
        let (Some(key), Some(binding)) = (
            private_control.managed_key,
            private_control.managed_binding.clone(),
        ) else {
            return Ok(true);
        };
        let owner = private_control
            .ownership
            .as_ref()
            .map(|record| record.owner.clone())
            .ok_or_else(|| anyhow::anyhow!("managed private artifact owner is absent"))?;
        let Some(authoritative) = read_managed_artifact_binding(key, &owner)? else {
            private_control.cleanup_complete = true;
            private_control.managed_key = None;
            private_control.managed_binding = None;
            return Ok(true);
        };
        if !same_private_artifact_binding(&binding, &authoritative) {
            anyhow::bail!("managed private artifact authority changed");
        }
        // The supervisor invokes physical artifact cleanup only after it has
        // positively reaped the exact managed child and durably resolved the
        // process slot. Record the debug lifetime proof at that boundary,
        // while the retained runner inode still exists.
        observe_failed_runner_lifetime(&private_control);
        cleanup_bound_private_runner_control(
            &private_control.parent,
            private_control.parent_identity,
            private_control
                .path
                .parent()
                .unwrap_or(&private_control.path),
            key,
            &authoritative,
            Some(&owner),
            None,
        )
        .map_err(|code| anyhow::anyhow!("managed private artifact cleanup failed: {code}"))?;
        private_control.cleanup_complete = true;
        private_control.managed_key = None;
        private_control.managed_binding = None;
        Ok(true)
    }
}

#[cfg(target_os = "linux")]
fn managed_runner_lifetime_guard(lifetime: &Arc<RunnerLifetime>) -> ManagedLifetimeGuard {
    let cleanup = Arc::clone(lifetime);
    ManagedLifetimeGuard::with_cleanup(Arc::clone(lifetime), move || {
        cleanup.cleanup_managed_artifacts()
    })
}

#[cfg(target_os = "linux")]
impl Drop for RunnerLifetime {
    fn drop(&mut self) {
        let private_control = self
            .private_control
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        observe_failed_runner_lifetime(private_control);
    }
}

#[cfg(target_os = "linux")]
struct RunnerLaunchFailureObserver {
    lifetime: Arc<RunnerLifetime>,
    armed: bool,
}

#[cfg(target_os = "linux")]
struct PendingManagedRunner {
    waiter: Option<ManagedWaiter>,
    lifetime: Arc<RunnerLifetime>,
}

#[cfg(target_os = "linux")]
struct PendingManagedReservation {
    reservation: Option<crate::launch_registry::ManagedLaunchReservation>,
    lifetime_guard: Option<ManagedLifetimeGuard>,
}

#[cfg(target_os = "linux")]
impl PendingManagedReservation {
    fn new(
        reservation: crate::launch_registry::ManagedLaunchReservation,
        lifetime_guard: ManagedLifetimeGuard,
    ) -> Self {
        Self {
            reservation: Some(reservation),
            lifetime_guard: Some(lifetime_guard),
        }
    }

    fn into_parts(
        mut self,
    ) -> (
        crate::launch_registry::ManagedLaunchReservation,
        ManagedLifetimeGuard,
    ) {
        (
            self.reservation
                .take()
                .expect("pending managed reservation invariant"),
            self.lifetime_guard
                .take()
                .expect("pending managed lifetime invariant"),
        )
    }
}

#[cfg(target_os = "linux")]
impl Drop for PendingManagedReservation {
    fn drop(&mut self) {
        if let (Some(reservation), Some(lifetime_guard)) =
            (self.reservation.take(), self.lifetime_guard.take())
        {
            abort_managed_launch_reservation(reservation, lifetime_guard);
            let _ = drain_managed_reaper_queue_bounded(Duration::from_secs(2));
        }
    }
}

#[cfg(target_os = "linux")]
impl PendingManagedRunner {
    fn new(waiter: ManagedWaiter, lifetime: Arc<RunnerLifetime>) -> Self {
        Self {
            waiter: Some(waiter),
            lifetime,
        }
    }

    fn into_waiter(mut self) -> ManagedWaiter {
        self.waiter
            .take()
            .expect("pending managed runner invariant")
    }
}

#[cfg(target_os = "linux")]
impl Drop for PendingManagedRunner {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            cleanup_managed_runner_waiter(waiter, Some(&self.lifetime));
        }
    }
}

#[cfg(target_os = "linux")]
fn cleanup_managed_runner_waiter(waiter: ManagedWaiter, lifetime: Option<&RunnerLifetime>) {
    if let ManagedBoundedReap::Pending(waiter) =
        waiter.terminate_and_reap_bounded(Duration::from_secs(2))
    {
        if let Some(lifetime) = lifetime {
            observe_pending_runner_handoff(&lifetime.private_control());
        }
        drop(waiter);
    }
    #[cfg(debug_assertions)]
    if active_speculation_test_config().is_some_and(|config| {
        config.observe_failed_runner_lifetime && !config.force_reaper_spawn_failure
    }) {
        // A positive reap can still enqueue cleanup-only work when the first
        // durable cleanup attempt is inconclusive. The real daemon keeps the
        // supervisor alive; the short-lived test driver waits here so it can
        // observe the same automatic retry before process exit.
        let _ = drain_managed_reaper_queue_bounded(Duration::from_secs(2));
    }
}

#[cfg(target_os = "linux")]
impl RunnerLaunchFailureObserver {
    fn new(lifetime: Arc<RunnerLifetime>) -> Self {
        Self {
            lifetime,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(target_os = "linux")]
impl Drop for RunnerLaunchFailureObserver {
    fn drop(&mut self) {
        if self.armed {
            observe_pending_runner_handoff(&self.lifetime.private_control());
        }
    }
}

#[cfg(target_os = "linux")]
fn observe_pending_runner_handoff(_private_control: &PrivateRunnerControl) {
    #[cfg(debug_assertions)]
    if active_speculation_test_config().is_some_and(|config| config.observe_failed_runner_lifetime)
        && _private_control.path.join("lterm").is_file()
    {
        let _ = std::fs::write(
            _private_control
                .path
                .parent()
                .unwrap_or(&_private_control.path)
                .join("failed-runner-pending-reaper-retained-private-control"),
            b"1\n",
        );
    }
}

#[cfg(target_os = "linux")]
fn observe_failed_runner_lifetime(_private_control: &PrivateRunnerControl) {
    #[cfg(debug_assertions)]
    if active_speculation_test_config().is_some_and(|config| config.observe_failed_runner_lifetime)
        && _private_control.path.join("lterm").is_file()
    {
        let _ = std::fs::write(
            _private_control
                .path
                .parent()
                .unwrap_or(&_private_control.path)
                .join("failed-runner-reaped-before-private-control-drop"),
            b"1\n",
        );
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn reconcile_private_runner_controls(
    control_root: &ValidatedDirectory,
    managed: &ManagedReconcileReport,
) -> ContainmentResult<()> {
    let Some(aliases) = private_runner_alias_groups(managed)? else {
        return Ok(());
    };
    for entries in aliases.values() {
        if !entries.iter().all(|entry| {
            entry.key.is_some() && entry.outcome == ReconcileOutcome::ResolvedTombstone
        }) {
            continue;
        }
        let owner = entries[0]
            .owner
            .as_ref()
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        remove_resolved_private_runner_control(control_root, owner, entries)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
type PrivateRunnerAliasGroups<'a> =
    std::collections::BTreeMap<(Uuid, u8), Vec<&'a crate::launch_registry::ManagedReconcileEntry>>;

#[cfg(target_os = "linux")]
fn private_runner_alias_groups(
    managed: &ManagedReconcileReport,
) -> ContainmentResult<Option<PrivateRunnerAliasGroups<'_>>> {
    // An unreadable slot or unresolved ownerless record makes global alias
    // correlation uncertain.  A resolved generic (ownerless) tombstone has no
    // physical speculation alias and therefore must not leak unrelated private
    // controls forever.
    if managed.entries.iter().any(|entry| {
        entry.key.is_none()
            || entry.owner.is_none() && entry.outcome != ReconcileOutcome::ResolvedTombstone
    }) {
        return Ok(None);
    }
    let mut aliases = std::collections::BTreeMap::<
        (Uuid, u8),
        Vec<&crate::launch_registry::ManagedReconcileEntry>,
    >::new();
    for entry in &managed.entries {
        let Some(owner) = entry.owner.as_ref() else {
            continue;
        };
        owner
            .validate()
            .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
        aliases
            .entry((owner.tournament_uuid, owner.candidate_index))
            .or_default()
            .push(entry);
    }
    Ok(Some(aliases))
}

#[cfg(target_os = "linux")]
fn read_private_runner_ownership(
    directory: &ValidatedDirectory,
) -> ContainmentResult<Option<(PrivateRunnerOwnership, PrivateArtifactIdentity)>> {
    let directory_fd = directory.try_clone_retained_fd().map_err(map_evidence)?;
    let fd = unsafe {
        libc::openat(
            directory_fd.as_raw_fd(),
            PRIVATE_RUNNER_OWNER_LEAF.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(ContainmentErrorCode::EvidenceUnavailable)
        };
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let mut bytes = Vec::new();
    file.take(4 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if bytes.len() > 4 * 1024 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let record: PrivateRunnerOwnership =
        serde_json::from_slice(&bytes).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if record.schema_version != 1 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(Some((record, artifact_identity(&metadata))))
}

#[cfg(target_os = "linux")]
fn remove_resolved_private_runner_control(
    control_root: &ValidatedDirectory,
    owner: &ManagedOwnerTag,
    resolved_aliases: &[&crate::launch_registry::ManagedReconcileEntry],
) -> ContainmentResult<()> {
    owner
        .validate()
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    control_root.revalidate().map_err(map_evidence)?;
    let mut bound = resolved_aliases
        .iter()
        .filter_map(|entry| {
            Some((
                entry.key?,
                entry.artifact_binding.as_ref()?,
                entry.owner.as_ref()?,
            ))
        })
        .collect::<Vec<_>>();
    if bound.len() > 1 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let bound = bound.pop();
    if bound.is_some_and(|(_, binding, _)| {
        let expected = binding.control_root();
        let current = managed_directory_identity(control_root.identity());
        expected.boot_uuid == current.boot_uuid && expected != current
    }) {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let leaf = cgroup_name(&format!(
        "lterm-g003-{}-candidate-{}",
        owner.tournament_uuid, owner.candidate_index
    ))?;
    let leaf_text = leaf
        .to_str()
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    if bound.is_some_and(|(_, binding, _)| binding.private_leaf() != leaf_text) {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let control_root_path = Path::new(std::ffi::OsStr::from_bytes(
        control_root.canonical_locator_bytes(),
    ));
    let path = control_root_path.join(std::ffi::OsStr::from_bytes(leaf.to_bytes()));
    let Some((managed_key, managed_binding, binding_owner)) = bound else {
        return match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(ContainmentErrorCode::InvalidIdentity),
        };
    };
    if binding_owner.tournament_uuid != owner.tournament_uuid
        || binding_owner.candidate_index != owner.candidate_index
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let parent = control_root.try_clone_retained_fd().map_err(map_evidence)?;
    cleanup_bound_private_runner_control(
        &parent,
        control_root.identity(),
        control_root_path,
        managed_key,
        managed_binding,
        Some(binding_owner),
        Some(resolved_aliases),
    )
}

#[cfg(target_os = "linux")]
pub(crate) struct CandidateContainment {
    control: CandidateControl,
    observer: CandidateObserver,
    observation: CandidateObservation,
}

#[cfg(target_os = "linux")]
struct CandidateProtocol {
    validator: SequenceValidator,
    output_limit_observed: bool,
}

#[cfg(target_os = "linux")]
pub(crate) struct CandidateControl {
    identity: RunnerIdentity,
    peer: File,
    protocol: Arc<Mutex<CandidateProtocol>>,
    payload_node: RetainedCgroupNode,
    payload_proof: Option<ManagedDescendantProof>,
    _lifetime: Arc<RunnerLifetime>,
}

#[cfg(target_os = "linux")]
pub(crate) struct CandidateObserver {
    identity: RunnerIdentity,
    peer: File,
    protocol: Arc<Mutex<CandidateProtocol>>,
    controller: ManagedController,
    sync_read: File,
    payload_node: RetainedCgroupNode,
    payload_membership: ManagedCgroupMembership,
    sync_eof_observed: bool,
    managed_reaped_observed: bool,
    waiter: Option<ManagedWaiter>,
    lifetime: Arc<RunnerLifetime>,
}

#[cfg(target_os = "linux")]
impl Drop for CandidateObserver {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            cleanup_managed_runner_waiter(waiter, Some(&self.lifetime));
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) struct PayloadPlacementEvidence {
    identity: RunnerIdentity,
    proof: ManagedDescendantProof,
}

#[cfg(target_os = "linux")]
pub(crate) struct CandidateContainmentParts {
    pub control: CandidateControl,
    pub observer: CandidateObserver,
    pub observation: CandidateObservation,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateObservation {
    pub identity: RunnerIdentity,
    pub managed_owner: ManagedOwnerEvidence,
}

#[cfg(target_os = "linux")]
impl CandidateContainment {
    pub(crate) fn observation(&self) -> CandidateObservation {
        self.observation.clone()
    }

    pub(crate) fn split(self) -> CandidateContainmentParts {
        CandidateContainmentParts {
            control: self.control,
            observer: self.observer,
            observation: self.observation,
        }
    }
}

#[cfg(target_os = "linux")]
impl fmt::Debug for CandidateContainment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CandidateContainment")
            .field("identity", &self.observation.identity)
            .finish_non_exhaustive()
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct CandidateContainment;

#[cfg(not(target_os = "linux"))]
pub(crate) struct CandidateControl;

#[cfg(not(target_os = "linux"))]
pub(crate) struct CandidateObserver;

#[cfg(not(target_os = "linux"))]
pub(crate) struct PayloadPlacementEvidence;

#[cfg(target_os = "linux")]
pub(crate) fn launch_runner(
    context: &LiveTournamentContext,
    topology: &TournamentTopology,
    candidate_index: u8,
    deadline: ContainmentDeadline,
) -> ContainmentResult<CandidateContainment> {
    let candidate = topology.candidate(candidate_index)?;
    launch_runner_with_argv(
        context,
        candidate,
        candidate_index,
        &context.argv,
        ManagedOwnerRole::Runner,
        deadline,
    )
}

#[cfg(target_os = "linux")]
fn receive_observer_frame(
    observer: &CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ControlFrame> {
    receive_protocol_frame(&observer.peer, &observer.protocol, deadline)
}

#[cfg(target_os = "linux")]
fn receive_protocol_frame(
    peer: &File,
    protocol: &Arc<Mutex<CandidateProtocol>>,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ControlFrame> {
    // The observer owns the only receive handle. Crucially, no sequence lock is
    // held while poll/recvmsg blocks, so an actor can always send the frame that
    // makes this wait complete.
    wait_fd_until(peer, libc::POLLIN, deadline)?;
    let frame = receive_frame_packet(peer).map_err(|_| ContainmentErrorCode::PeerRejected)?;
    protocol
        .lock()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        .validator
        .accept(&frame)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    Ok(frame)
}

#[cfg(target_os = "linux")]
fn send_control_frame(
    control: &CandidateControl,
    message: ControlMessage,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    let mut protocol = control
        .protocol
        .lock()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    send_validated_frame(
        &control.peer,
        &mut protocol.validator,
        message,
        control.identity,
        deadline,
    )
}

#[cfg(target_os = "linux")]
fn launch_runner_with_argv(
    context: &LiveTournamentContext,
    candidate: &CandidateTopology,
    candidate_index: u8,
    candidate_argv: &[Vec<u8>],
    owner_role: ManagedOwnerRole,
    deadline: ContainmentDeadline,
) -> ContainmentResult<CandidateContainment> {
    deadline.remaining()?;
    if candidate.candidate_index != candidate_index {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let control_node = candidate.control()?;
    let payload_node = candidate.payload()?;
    for directory in [
        &context.source,
        &context.candidates[0],
        &context.candidates[1],
        &context.ledger_root,
        &context.cgroup_root,
        &context.control_root,
    ] {
        directory.revalidate().map_err(map_evidence)?;
    }
    revalidate_cgroup_node(control_node)?;
    revalidate_cgroup_node(payload_node)?;
    let identity = context.candidate_identity(candidate_index)?;
    let owner = ManagedOwnerTag {
        kind: ManagedOwnerKind::Speculation,
        tournament_uuid: identity.tournament_uuid,
        candidate_index,
        role: owner_role,
    };
    let mut reservation = reserve_managed_launch(Some(owner.clone()))
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let private_control =
        match prepare_managed_runner_control(context, candidate_index, &owner, &mut reservation) {
            Ok(private_control) => private_control,
            Err(error) => {
                abort_prepared_runner_reservation(context, owner, reservation);
                return Err(error);
            }
        };
    let lifetime = Arc::new(RunnerLifetime::new(private_control));
    let mut failure_observer = RunnerLaunchFailureObserver::new(Arc::clone(&lifetime));
    let pending_reservation =
        PendingManagedReservation::new(reservation, managed_runner_lifetime_guard(&lifetime));
    let invocation = build_fixed_bwrap_invocation(identity)?;
    let control_membership = managed_membership(control_node, identity)?;
    let payload_membership = managed_membership(payload_node, identity)?;
    let placement = ControlCgroupPlacement::new(
        open_cgroup_procs(control_node)?,
        clone_file(&control_node.file)?,
        control_membership.clone(),
    )
    .map_err(|_| ContainmentErrorCode::PlacementUnproven)?;
    let mut sync_fds = [-1; 2];
    if unsafe { libc::pipe2(sync_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let sync_read = unsafe { File::from_raw_fd(sync_fds[0]) };
    let sync_write = unsafe { File::from_raw_fd(sync_fds[1]) };
    let sync_auxiliary = SyncPipeWrite::new(sync_write, MANAGED_SYNC_PIPE_TARGET_FD)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let pinned_runner = {
        let private_control = lifetime.private_control();
        private_control
            .directory
            .revalidate()
            .map_err(map_evidence)?;
        let directory_fd = private_control
            .directory
            .try_clone_retained_fd()
            .map_err(map_evidence)?;
        let expected = artifact_identity(
            &private_control
                .runner_file
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        );
        let reopened = open_validated_private_artifact_at(
            directory_fd.as_raw_fd(),
            c"lterm",
            PrivateArtifactKind::Runner,
            Some(expected),
        )?
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        if artifact_identity(
            &reopened
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        ) != expected
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        ManagedPinnedRunner::new(
            private_control
                .runner_file
                .try_clone()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        )
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?
    };
    let candidate_directory = {
        let candidate = context
            .candidates
            .get(usize::from(candidate_index))
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        candidate.revalidate().map_err(map_evidence)?;
        ManagedPinnedCandidateDirectory::new(
            candidate.try_clone_retained_fd().map_err(map_evidence)?,
            managed_directory_identity(candidate.identity()),
        )
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?
    };
    let control_directory = {
        let private_control = lifetime.private_control();
        private_control
            .directory
            .revalidate()
            .map_err(map_evidence)?;
        let expected = private_control
            .managed_binding
            .as_ref()
            .and_then(ManagedArtifactBinding::private_directory)
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        if expected != managed_directory_identity(private_control.directory.identity()) {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        ManagedPinnedControlDirectory::new(
            private_control
                .directory
                .try_clone_retained_fd()
                .map_err(map_evidence)?,
            expected,
        )
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?
    };
    let control_socket = {
        let private_control = lifetime.private_control();
        let expected = private_control
            .managed_binding
            .as_ref()
            .and_then(ManagedArtifactBinding::socket)
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        ManagedPinnedControlSocket::new(
            private_control
                .socket_file
                .try_clone()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
            expected,
        )
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?
    };
    failpoint("before_managed_launch")?;
    let (reservation, lifetime_guard) = pending_reservation.into_parts();
    let managed = match launch_managed_process(ManagedLaunchRequest {
        owner: Some(owner),
        reservation: Some(reservation),
        lifetime_guard: Some(lifetime_guard),
        executable_policy: ManagedExecutablePolicy::PinnedSystemBwrap,
        placement: ManagedPlacement::CgroupV2(placement),
        auxiliary: ManagedAuxiliary::Speculation {
            sync_pipe: sync_auxiliary,
            pinned_runner,
            candidate_directory,
            control_directory,
            control_socket,
        },
        executable: invocation.executable,
        arguments: invocation.arguments,
        current_dir: None,
        environment: Vec::new(),
        stdio: ManagedStdioPolicy::Null,
    }) {
        Ok(managed) => managed,
        Err(failure) => {
            drop(failure);
            return Err(ContainmentErrorCode::PinnedBwrapFailure);
        }
    };
    let managed_controller = managed.controller;
    let owner_receipt = managed.owner_receipt;
    let pending_waiter = PendingManagedRunner::new(managed.waiter, Arc::clone(&lifetime));
    let owner_receipt = owner_receipt.ok_or(ContainmentErrorCode::EvidenceUnavailable)?;
    failpoint("after_managed_launch")?;
    let listener = lifetime
        .private_control()
        .listener
        .as_ref()
        .ok_or(ContainmentErrorCode::PeerRejected)?
        .try_clone()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let peer = accept_authenticated_peer(
        &listener,
        &managed_controller,
        &control_membership,
        identity,
        deadline,
    )?;
    #[cfg(debug_assertions)]
    {
        lifetime
            .private_control()
            .observe_authenticated_retained_control()?;
        prove_hostile_control_listener_uncontacted()?;
    }
    failpoint("after_control_accept")?;
    failpoint("before_control_unlink")?;
    drop(listener);
    lifetime.private_control().retire_socket_listener()?;
    failpoint("after_control_unlink")?;
    let mut validator =
        SequenceValidator::new(identity).map_err(|_| ContainmentErrorCode::PeerRejected)?;
    wait_fd_until(&peer, libc::POLLIN, deadline)?;
    let hello = receive_frame_packet(&peer).map_err(|_| ContainmentErrorCode::PeerRejected)?;
    validator
        .accept(&hello)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    if !matches!(hello.message, ControlMessage::Hello) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    send_validated_frame(
        &peer,
        &mut validator,
        ControlMessage::HelloAck,
        identity,
        deadline,
    )?;
    for frame in argv_frames(identity, validator.next_sequence(), candidate_argv)
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?
    {
        failpoint("before_argv_frame_send")?;
        validator
            .accept(&frame)
            .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
        wait_fd_until(&peer, libc::POLLOUT, deadline)?;
        send_frame_packet(&peer, &frame).map_err(|_| ContainmentErrorCode::PeerRejected)?;
        failpoint("after_argv_frame_send")?;
    }
    let ready = receive_validated_frame(&peer, &mut validator, deadline)?;
    if !matches!(ready.message, ControlMessage::Ready) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    context.candidates[usize::from(candidate_index)]
        .revalidate()
        .map_err(map_evidence)?;
    lifetime
        .private_control()
        .directory
        .revalidate()
        .map_err(map_evidence)?;
    let owner = owner_receipt.owner();
    let observation = CandidateObservation {
        identity,
        managed_owner: ManagedOwnerEvidence {
            candidate_index: owner.candidate_index,
            role: match owner.role {
                ManagedOwnerRole::Probe => ManagedOwnerRoleEvidence::Probe,
                ManagedOwnerRole::Runner => ManagedOwnerRoleEvidence::Runner,
            },
            slot: owner_receipt.slot(),
            generation: owner_receipt.generation(),
        },
    };
    let observer_peer = peer
        .try_clone()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let control_payload_node = clone_cgroup_node(payload_node)?;
    let observer_payload_node = clone_cgroup_node(payload_node)?;
    let protocol = Arc::new(Mutex::new(CandidateProtocol {
        validator,
        output_limit_observed: false,
    }));
    failure_observer.disarm();
    let managed_waiter = pending_waiter.into_waiter();
    Ok(CandidateContainment {
        control: CandidateControl {
            identity,
            peer,
            protocol: Arc::clone(&protocol),
            payload_node: control_payload_node,
            payload_proof: None,
            _lifetime: Arc::clone(&lifetime),
        },
        observer: CandidateObserver {
            identity,
            peer: observer_peer,
            protocol,
            controller: managed_controller,
            sync_read,
            payload_node: observer_payload_node,
            payload_membership,
            sync_eof_observed: false,
            managed_reaped_observed: false,
            waiter: Some(managed_waiter),
            lifetime,
        },
        observation,
    })
}

#[cfg(target_os = "linux")]
fn clone_cgroup_node(node: &RetainedCgroupNode) -> ContainmentResult<RetainedCgroupNode> {
    Ok(RetainedCgroupNode {
        file: node
            .file
            .try_clone()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        identity: node.identity,
        membership: node.membership.clone(),
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn run_fixed_probe(
    context: &LiveTournamentContext,
    candidate_index: u8,
    topology: &CandidateTopology,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ProbeEvidence> {
    if topology.candidate_index() != candidate_index {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let containment = launch_fixed_probe(context, topology, candidate_index, deadline)?;
    let CandidateContainmentParts {
        mut control,
        mut observer,
        ..
    } = containment.split();
    transfer_payload_fd(&mut control, topology, deadline)?;
    receive_payload_fd_ack(&mut observer, deadline)?;
    send_go(&mut control, deadline)?;
    receive_go_receipt(&mut observer, deadline)?;
    let placement = receive_payload_placed(&mut observer, topology, deadline)?;
    send_payload_release(&mut control, placement, deadline)?;
    let leader = receive_leader_exited(&mut observer, deadline)?;
    let payload = topology.payload()?;
    write_leaf(payload, c"cgroup.kill", b"1\n")?;
    wait_populated_zero(payload, deadline)?;
    let drained = receive_output_drained(&mut observer, deadline)?;
    let exited_zero = matches!(
        leader,
        ContainmentEvent::LeaderExited {
            category: RunnerExitCategory::ExitedZero,
            ..
        }
    );
    let output_bytes = match drained {
        ContainmentEvent::OutputDrained { bytes, .. } => bytes,
        _ => return Err(ContainmentErrorCode::TerminalBoundaryFailure),
    };
    send_select_or_abort(&mut control, DecisionKind::Abort, deadline)?;
    receive_decision_ack(&mut observer, DecisionKind::Abort, deadline)?;
    observe_sync_eof(&mut observer, deadline)?;
    observe_managed_reaped(&mut observer, deadline)?;
    finish_containment(control, observer)?;
    let parent = topology
        .parent
        .as_ref()
        .ok_or(ContainmentErrorCode::TopologyFailure)?;
    wait_populated_zero(parent, deadline)?;
    if !exited_zero || output_bytes != 0 {
        return Err(ContainmentErrorCode::TerminalBoundaryFailure);
    }
    Ok(ProbeEvidence {
        candidate: candidate_index,
        exited_zero,
        output_bytes,
        parent_populated_zero: true,
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn launch_fixed_probe(
    context: &LiveTournamentContext,
    topology: &CandidateTopology,
    candidate_index: u8,
    deadline: ContainmentDeadline,
) -> ContainmentResult<CandidateContainment> {
    if topology.candidate_index() != candidate_index {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let probe_argv = vec![
        b"/run/lterm-control/lterm".to_vec(),
        b"--internal-speculation-probe-v1".to_vec(),
    ];
    launch_runner_with_argv(
        context,
        topology,
        candidate_index,
        &probe_argv,
        ManagedOwnerRole::Probe,
        deadline,
    )
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn run_fixed_probe(
    _context: &LiveTournamentContext,
    _candidate_index: u8,
    _topology: &CandidateTopology,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ProbeEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_fixed_probe(
    _context: &LiveTournamentContext,
    _topology: &CandidateTopology,
    _candidate_index: u8,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<CandidateContainment> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_runner(
    _context: &LiveTournamentContext,
    _topology: &TournamentTopology,
    _candidate_index: u8,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<CandidateContainment> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn transfer_payload_fd(
    control: &mut CandidateControl,
    topology: &CandidateTopology,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    if topology.candidate_index != control.identity.candidate_index {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let payload = topology.payload()?;
    if payload.identity != control.payload_node.identity {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    transfer_payload_fd_from_node(control, payload, deadline)
}

#[cfg(target_os = "linux")]
pub(crate) fn transfer_payload_fd_owned(
    control: &mut CandidateControl,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    let payload = clone_cgroup_node(&control.payload_node)?;
    transfer_payload_fd_from_node(control, &payload, deadline)
}

#[cfg(target_os = "linux")]
fn transfer_payload_fd_from_node(
    control: &mut CandidateControl,
    payload: &RetainedCgroupNode,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    let placement_fd = open_cgroup_procs(payload)?;
    failpoint("before_payload_fd_evidence")?;
    let placement = placement_descriptor_evidence(&placement_fd, payload, control.identity)?;
    failpoint("after_payload_fd_evidence")?;
    let mut protocol = control
        .protocol
        .lock()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let frame = ControlFrame::new(
        control.identity,
        protocol.validator.next_sequence(),
        ControlMessage::ReadyAck { placement },
    );
    protocol
        .validator
        .accept(&frame)
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
    failpoint("before_payload_fd_send")?;
    wait_fd_until(&control.peer, libc::POLLOUT, deadline)?;
    send_frame_with_one_fd(&control.peer, &frame, &placement_fd)
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
    failpoint("after_payload_fd_send")?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_payload_fd_ack(
    observer: &mut CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    let ack = receive_observer_frame(observer, deadline)?;
    if !matches!(ack.message, ControlMessage::PayloadFdAck) {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    Ok(ContainmentEvent::PayloadFdAck {
        candidate: observer.identity.candidate_index,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn transfer_payload_fd(
    _control: &mut CandidateControl,
    _topology: &CandidateTopology,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_payload_fd_ack(
    _observer: &mut CandidateObserver,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_go(
    control: &mut CandidateControl,
    deadline: ContainmentDeadline,
) -> ContainmentResult<GoSendEvidence> {
    let sent_monotonic_ns = monotonic_now_ns()?;
    send_control_frame(control, ControlMessage::Go, deadline)?;
    Ok(GoSendEvidence {
        candidate: control.identity.candidate_index,
        sent_monotonic_ns,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn send_go(
    _control: &mut CandidateControl,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<GoSendEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_go_receipt(
    observer: &mut CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<GoReceiptEvidence> {
    let received = receive_observer_frame(observer, deadline)?;
    let ControlMessage::GoReceived { monotonic_ns } = received.message else {
        return Err(ContainmentErrorCode::PeerRejected);
    };
    if monotonic_ns == 0 {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    Ok(GoReceiptEvidence {
        identity: observer.identity,
        received_monotonic_ns: monotonic_ns,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_go_receipt(
    _observer: &mut CandidateObserver,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<GoReceiptEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_payload_placed(
    observer: &mut CandidateObserver,
    topology: &CandidateTopology,
    deadline: ContainmentDeadline,
) -> ContainmentResult<PayloadPlacementEvidence> {
    let payload = topology.payload()?;
    if payload.identity != observer.payload_node.identity {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let payload = clone_cgroup_node(payload)?;
    receive_payload_placed_from_node(observer, &payload, deadline)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_payload_placed_owned(
    observer: &mut CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<PayloadPlacementEvidence> {
    let payload = clone_cgroup_node(&observer.payload_node)?;
    receive_payload_placed_from_node(observer, &payload, deadline)
}

#[cfg(target_os = "linux")]
fn receive_payload_placed_from_node(
    observer: &mut CandidateObserver,
    payload: &RetainedCgroupNode,
    deadline: ContainmentDeadline,
) -> ContainmentResult<PayloadPlacementEvidence> {
    let placed = receive_observer_frame(observer, deadline)?;
    if !matches!(placed.message, ControlMessage::PayloadPlaced) {
        return Err(ContainmentErrorCode::PlacementUnproven);
    }
    let observed = read_single_cgroup_pid(payload)?;
    failpoint("before_payload_membership_proof")?;
    let proof = observer
        .controller
        .prove_descendant_in_cgroup(observed, &observer.payload_membership)
        .map_err(|_| ContainmentErrorCode::PlacementUnproven)?;
    failpoint("after_payload_membership_proof")?;
    if proof.membership() != &observer.payload_membership {
        return Err(ContainmentErrorCode::PlacementUnproven);
    }
    prove_namespace_isolation(observed)?;
    Ok(PayloadPlacementEvidence {
        identity: observer.identity,
        proof,
    })
}

#[cfg(target_os = "linux")]
impl PayloadPlacementEvidence {
    pub(crate) fn event(&self) -> ContainmentEvent {
        ContainmentEvent::PayloadPlaced {
            candidate: self.identity.candidate_index,
        }
    }
}

#[cfg(target_os = "linux")]
fn prove_namespace_isolation(observed_host_pid: u32) -> ContainmentResult<()> {
    for namespace in ["user", "pid", "net", "ipc", "uts"] {
        let host = std::fs::metadata(format!("/proc/self/ns/{namespace}"))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        let child = std::fs::metadata(format!("/proc/{observed_host_pid}/ns/{namespace}"))
            .map_err(|_| ContainmentErrorCode::PlacementUnproven)?;
        if host.dev() == child.dev() && host.ino() == child.ino() {
            return Err(ContainmentErrorCode::PlacementUnproven);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_payload_placed(
    _observer: &mut CandidateObserver,
    _topology: &CandidateTopology,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<PayloadPlacementEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_payload_release(
    control: &mut CandidateControl,
    placement: PayloadPlacementEvidence,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    if placement.identity != control.identity
        || placement.proof.membership().candidate_index() != control.identity.candidate_index
        || placement.proof.membership().generation() != control.identity.generation
    {
        return Err(ContainmentErrorCode::PlacementUnproven);
    }
    control.payload_proof = Some(placement.proof);
    failpoint("before_payload_release")?;
    send_control_frame(control, ControlMessage::PayloadRelease, deadline)?;
    failpoint("after_payload_release")
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn send_payload_release(
    _control: &mut CandidateControl,
    _placement: PayloadPlacementEvidence,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_candidate_completion(
    observer: &mut CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<[ContainmentEvent; 2]> {
    let leader = receive_leader_exited(observer, deadline)?;
    let drained = receive_output_drained(observer, deadline)?;
    Ok([leader, drained])
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_candidate_completion(
    _observer: &mut CandidateObserver,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<[ContainmentEvent; 2]> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_leader_exited(
    observer: &mut CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    let event = receive_execution_event(observer, deadline)?;
    if !matches!(event, ContainmentEvent::LeaderExited { .. }) {
        return Err(ContainmentErrorCode::OutputLimit);
    }
    Ok(event)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_execution_event(
    observer: &mut CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    let leader = receive_observer_frame(observer, deadline)?;
    match leader.message {
        ControlMessage::LeaderExited {
            category,
            elapsed_ns,
        } if elapsed_ns != 0 => Ok(ContainmentEvent::LeaderExited {
            candidate: observer.identity.candidate_index,
            category,
            elapsed_ns,
        }),
        ControlMessage::OutputLimitExceeded { bytes }
            if bytes == crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES + 1 =>
        {
            let mut protocol = observer
                .protocol
                .lock()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            if protocol.output_limit_observed {
                return Err(ContainmentErrorCode::PeerRejected);
            }
            protocol.output_limit_observed = true;
            Ok(ContainmentEvent::OutputLimitExceeded {
                candidate: observer.identity.candidate_index,
                bytes,
            })
        }
        _ => Err(ContainmentErrorCode::PeerRejected),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_leader_exited(
    _observer: &mut CandidateObserver,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_execution_event(
    _observer: &mut CandidateObserver,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn acknowledge_output_cleanup_claimed(
    control: &mut CandidateControl,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    let protocol = control
        .protocol
        .lock()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if !protocol.output_limit_observed {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    drop(protocol);
    send_control_frame(control, ControlMessage::OutputCleanupClaimed, deadline)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn acknowledge_output_cleanup_claimed(
    _control: &mut CandidateControl,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_output_drained(
    observer: &mut CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    let drained = receive_observer_frame(observer, deadline)?;
    let ControlMessage::OutputDrained { bytes } = drained.message else {
        return Err(ContainmentErrorCode::PeerRejected);
    };
    let output_limit_observed = observer
        .protocol
        .lock()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        .output_limit_observed;
    if bytes > crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES && !output_limit_observed {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    Ok(ContainmentEvent::OutputDrained {
        candidate: observer.identity.candidate_index,
        bytes,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_output_drained(
    _observer: &mut CandidateObserver,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_select_or_abort(
    control: &mut CandidateControl,
    decision: DecisionKind,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    send_control_frame(control, ControlMessage::ResultAccepted, deadline)?;
    send_control_frame(control, ControlMessage::Decision { decision }, deadline)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_decision_ack(
    observer: &mut CandidateObserver,
    decision: DecisionKind,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    let ack = receive_observer_frame(observer, deadline)?;
    if !matches!(ack.message, ControlMessage::Ack { decision: observed } if observed == decision) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    Ok(ContainmentEvent::DecisionAck {
        candidate: observer.identity.candidate_index,
        decision,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn send_select_or_abort(
    _control: &mut CandidateControl,
    _decision: DecisionKind,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_decision_ack(
    _observer: &mut CandidateObserver,
    _decision: DecisionKind,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn observe_sync_eof(
    observer: &mut CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    if observer.sync_eof_observed {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    wait_fd_until(&observer.sync_read, libc::POLLIN, deadline)?;
    let mut byte = [0_u8; 1];
    if observer
        .sync_read
        .read(&mut byte)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        != 0
    {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    observer.sync_eof_observed = true;
    Ok(ContainmentEvent::SyncEof {
        candidate: observer.identity.candidate_index,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn observe_sync_eof(
    _observer: &mut CandidateObserver,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn observe_managed_reaped(
    observer: &mut CandidateObserver,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    if observer.managed_reaped_observed {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    observer
        .waiter
        .as_mut()
        .ok_or(ContainmentErrorCode::EvidenceUnavailable)?
        .wait_until(deadline.instant())
        .map_err(|_| {
            if deadline.expired() {
                ContainmentErrorCode::Timeout
            } else {
                ContainmentErrorCode::EvidenceUnavailable
            }
        })?;
    observer.waiter.take();
    observer.managed_reaped_observed = true;
    Ok(ContainmentEvent::ManagedReaped {
        candidate: observer.identity.candidate_index,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn observe_managed_reaped(
    _observer: &mut CandidateObserver,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn finish_containment(
    control: CandidateControl,
    observer: CandidateObserver,
) -> ContainmentResult<()> {
    if control.identity != observer.identity
        || !observer.sync_eof_observed
        || !observer.managed_reaped_observed
        || !control
            .protocol
            .lock()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            .validator
            .is_complete()
    {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn finish_containment(
    _control: CandidateControl,
    _observer: CandidateObserver,
) -> ContainmentResult<()> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
fn prepare_runner_control(
    context: &LiveTournamentContext,
    candidate_index: u8,
) -> ContainmentResult<PrivateRunnerControl> {
    prepare_runner_control_inner(context, candidate_index, None)
}

#[cfg(target_os = "linux")]
fn prepare_managed_runner_control(
    context: &LiveTournamentContext,
    candidate_index: u8,
    owner: &ManagedOwnerTag,
    reservation: &mut crate::launch_registry::ManagedLaunchReservation,
) -> ContainmentResult<PrivateRunnerControl> {
    prepare_runner_control_inner(context, candidate_index, Some((owner, reservation)))
}

#[cfg(target_os = "linux")]
fn prepare_runner_control_inner(
    context: &LiveTournamentContext,
    candidate_index: u8,
    mut managed: Option<(
        &ManagedOwnerTag,
        &mut crate::launch_registry::ManagedLaunchReservation,
    )>,
) -> ContainmentResult<PrivateRunnerControl> {
    context.control_root.revalidate().map_err(map_evidence)?;
    let leaf = cgroup_name(&format!(
        "lterm-g003-{}-candidate-{candidate_index}",
        context.identity.tournament_uuid
    ))?;
    let root = context
        .control_root
        .try_clone_retained_fd()
        .map_err(map_evidence)?;
    let path = context
        .control_root_path()
        .join(std::ffi::OsStr::from_bytes(leaf.to_bytes()));
    failpoint("before_private_control_reservation")?;
    if let Some((_, reservation)) = managed.as_mut() {
        reservation
            .begin_artifact_creation(
                managed_directory_identity(context.control_root.identity()),
                leaf.to_str()
                    .map_err(|_| ContainmentErrorCode::InvalidIdentity)?,
            )
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    failpoint("after_private_control_reservation")?;
    if unsafe { libc::mkdirat(root.as_raw_fd(), leaf.as_ptr(), 0o700) } != 0 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let directory = open_existing_private_dir(&path).map_err(map_evidence)?;
    directory
        .try_clone_retained_fd()
        .map_err(map_evidence)?
        .sync_all()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    root.sync_all()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    failpoint("before_private_control_binding")?;
    let mut managed_binding = managed
        .as_mut()
        .map(|(_, reservation)| {
            reservation.finish_artifact_creation(managed_directory_identity(directory.identity()))
        })
        .transpose()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    failpoint("after_private_control_binding")?;
    let managed_key = managed.as_ref().map(|(_, reservation)| reservation.key());
    let runner_path = path.join("lterm");
    if let Some((_, reservation)) = managed.as_mut() {
        managed_binding = Some(
            reservation
                .begin_artifact_runner_creation()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        );
    }
    failpoint("after_private_runner_creation_intent")?;
    let mut runner = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o500)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&runner_path)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    copy_retained_executable(&context.current_executable, &mut runner)?;
    drop(runner);
    let runner = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&runner_path)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    verify_runner_copy(&context.current_executable, &runner)?;
    let runner_identity = artifact_identity(
        &runner
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
    );
    directory
        .try_clone_retained_fd()
        .map_err(map_evidence)?
        .sync_all()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    revalidate_retained_private_runner(&directory, &runner, runner_identity)?;
    failpoint("before_private_runner_binding")?;
    // Re-run after the failpoint so a test or concurrent actor cannot swap the
    // published name between the initial retained proof and durable binding.
    revalidate_retained_private_runner(&directory, &runner, runner_identity)?;
    if let Some((_, reservation)) = managed.as_mut() {
        managed_binding = Some(
            reservation
                .finish_artifact_runner(managed_artifact_identity(runner_identity))
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        );
    }
    failpoint("after_private_runner_binding")?;
    if let Some((_, reservation)) = managed.as_mut() {
        managed_binding = Some(
            reservation
                .begin_artifact_socket_creation()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        );
    }
    failpoint("after_private_socket_creation_intent")?;
    let socket_path = path.join("control.sock");
    let (listener, socket_identity) = create_published_seqpacket_listener(
        &directory,
        &socket_path,
        managed_binding
            .as_ref()
            .map(ManagedArtifactBinding::nonce)
            .unwrap_or_else(Uuid::new_v4),
    )?;
    directory.revalidate().map_err(map_evidence)?;
    failpoint("before_private_socket_binding")?;
    let final_socket = open_validated_private_artifact_at(
        directory
            .try_clone_retained_fd()
            .map_err(map_evidence)?
            .as_raw_fd(),
        c"control.sock",
        PrivateArtifactKind::Socket,
        Some(socket_identity),
    )?
    .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    if artifact_identity(
        &final_socket
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
    ) != socket_identity
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    prove_seqpacket_listener_path_continuity(&listener, &socket_path)?;
    directory.revalidate().map_err(map_evidence)?;
    if let Some((_, reservation)) = managed.as_mut() {
        managed_binding = Some(
            reservation
                .finish_artifact_socket(managed_artifact_identity(socket_identity))
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        );
    }
    failpoint("after_private_socket_binding")?;
    failpoint("after_private_artifact_binding")?;
    if let Some((_, reservation)) = managed.as_mut() {
        managed_binding = Some(
            reservation
                .begin_artifact_owner_creation()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        );
    }
    failpoint("after_private_owner_creation_intent")?;
    let ownership = managed
        .as_ref()
        .map(|(owner, reservation)| PrivateRunnerOwnership {
            schema_version: 1,
            owner: (*owner).clone(),
            slot: reservation.key().slot(),
            generation: reservation.key().generation(),
            binding: managed_binding
                .clone()
                .expect("managed private runner binding invariant"),
            directory: directory.identity(),
            runner: runner_identity,
            socket: socket_identity,
        });
    let ownership_file = ownership
        .as_ref()
        .map(|record| write_private_runner_ownership(&directory, record))
        .transpose()?;
    if let (Some(identity), Some((_, reservation))) = (ownership_file, managed.as_mut()) {
        managed_binding = Some(
            reservation
                .finish_artifact_owner(managed_artifact_identity(identity))
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        );
    }
    failpoint("after_private_owner_binding")?;
    directory.revalidate().map_err(map_evidence)?;
    Ok(PrivateRunnerControl {
        parent: root,
        parent_identity: context.control_root.identity(),
        leaf,
        directory,
        path,
        socket_path,
        runner_file: runner,
        socket_file: final_socket,
        listener: Some(listener),
        ownership,
        ownership_file,
        managed_key,
        managed_binding,
        cleanup_complete: false,
    })
}

#[cfg(target_os = "linux")]
fn revalidate_retained_private_runner(
    directory: &ValidatedDirectory,
    runner: &File,
    runner_identity: PrivateArtifactIdentity,
) -> ContainmentResult<()> {
    let reopened_runner = open_validated_private_artifact_at(
        directory
            .try_clone_retained_fd()
            .map_err(map_evidence)?
            .as_raw_fd(),
        c"lterm",
        PrivateArtifactKind::Runner,
        Some(runner_identity),
    )?
    .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    if artifact_identity(
        &reopened_runner
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
    ) != artifact_identity(
        &runner
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
    ) {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn write_private_runner_ownership(
    directory: &ValidatedDirectory,
    ownership: &PrivateRunnerOwnership,
) -> ContainmentResult<PrivateArtifactIdentity> {
    let bytes =
        serde_json::to_vec(ownership).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if bytes.len() > 4 * 1024 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let directory_fd = directory.try_clone_retained_fd().map_err(map_evidence)?;
    let temp_leaf = CString::new(format!(
        ".owner.json.create-{}",
        ownership.binding.nonce().simple()
    ))
    .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    let mut temp_owned = false;
    let mut temp_identity = None;
    let result = (|| {
        let fd = unsafe {
            libc::openat(
                directory_fd.as_raw_fd(),
                temp_leaf.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        temp_owned = true;
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        let unpublished = file
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        if !unpublished.is_file()
            || unpublished.uid() != unsafe { libc::geteuid() }
            || unpublished.mode() & 0o7777 != 0o600
            || unpublished.nlink() != 1
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        let unpublished_identity = artifact_identity(&unpublished);
        temp_identity = Some(unpublished_identity);
        if !validate_private_artifact_at(
            directory_fd.as_raw_fd(),
            &temp_leaf,
            PrivateArtifactKind::PartialOwnership,
            Some(unpublished_identity),
        )? {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        directory_fd
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        failpoint("before_private_owner_publish")?;
        if unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                directory_fd.as_raw_fd(),
                temp_leaf.as_ptr(),
                directory_fd.as_raw_fd(),
                PRIVATE_RUNNER_OWNER_LEAF.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        failpoint("after_private_owner_rename_before_identity")?;
        let final_file = open_validated_private_artifact_at(
            directory_fd.as_raw_fd(),
            PRIVATE_RUNNER_OWNER_LEAF,
            PrivateArtifactKind::Ownership,
            Some(unpublished_identity),
        )?
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        if artifact_identity(
            &final_file
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        ) != artifact_identity(
            &file
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        ) {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        directory_fd
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        failpoint("after_private_owner_publish")?;
        let published = file
            .metadata()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        if published.nlink() != 1 {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        directory.revalidate().map_err(map_evidence)?;
        Ok(artifact_identity(&published))
    })();
    if result.is_err()
        && temp_owned
        && temp_identity.is_some_and(|identity| {
            validate_private_artifact_at(
                directory_fd.as_raw_fd(),
                &temp_leaf,
                PrivateArtifactKind::PartialOwnership,
                Some(identity),
            ) == Ok(true)
        })
        && unsafe { libc::unlinkat(directory_fd.as_raw_fd(), temp_leaf.as_ptr(), 0) } == 0
    {
        let _ = directory_fd.sync_all();
    }
    result
}

#[cfg(target_os = "linux")]
fn create_seqpacket_listener(path: &Path) -> ContainmentResult<File> {
    let listener = bind_seqpacket_listener(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    Ok(listener)
}

#[cfg(target_os = "linux")]
fn private_socket_temp_leaf(nonce: Uuid) -> CString {
    // sockaddr_un paths are short.  The directory itself is already uniquely
    // bound to the full nonce, so an eight-hex suffix is sufficient to make
    // the single in-directory creation leaf deterministic without making an
    // otherwise valid final socket path exceed sun_path.
    CString::new(format!(".sock-{:08x}", nonce.as_u128() as u32))
        .expect("UUID-derived private socket leaf")
}

#[cfg(target_os = "linux")]
fn create_published_seqpacket_listener(
    directory: &ValidatedDirectory,
    path: &Path,
    nonce: Uuid,
) -> ContainmentResult<(File, PrivateArtifactIdentity)> {
    let temp_leaf = private_socket_temp_leaf(nonce);
    let final_leaf = c"control.sock";
    let temp_path = path.with_file_name(std::ffi::OsStr::from_bytes(temp_leaf.as_bytes()));
    let directory_fd = directory.try_clone_retained_fd().map_err(map_evidence)?;
    let mut temp_owned = false;
    let mut temp_identity = None;
    let result = (|| {
        let listener = bind_seqpacket_listener(&temp_path)?;
        temp_owned = true;
        failpoint("after_private_socket_bind_before_identity")?;
        let metadata = std::fs::symlink_metadata(&temp_path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        let identity = artifact_identity(&metadata);
        let unpublished = open_validated_private_artifact_at(
            directory_fd.as_raw_fd(),
            &temp_leaf,
            PrivateArtifactKind::PartialSocket,
            Some(identity),
        )?
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        temp_identity = Some(identity);
        failpoint("before_private_socket_mode")?;
        std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ContainmentErrorCode::PeerRejected)?;
        failpoint("after_private_socket_mode")?;
        if !validate_private_artifact_at(
            directory_fd.as_raw_fd(),
            &temp_leaf,
            PrivateArtifactKind::PartialSocket,
            Some(identity),
        )? {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        directory_fd
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        failpoint("before_private_socket_publish")?;
        if unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                directory_fd.as_raw_fd(),
                temp_leaf.as_ptr(),
                directory_fd.as_raw_fd(),
                final_leaf.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(ContainmentErrorCode::PeerRejected);
        }
        failpoint("after_private_socket_rename_before_identity")?;
        let published = open_validated_private_artifact_at(
            directory_fd.as_raw_fd(),
            final_leaf,
            PrivateArtifactKind::Socket,
            Some(identity),
        )?
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        if artifact_identity(
            &published
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        ) != artifact_identity(
            &unpublished
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        ) {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        prove_seqpacket_listener_path_continuity(&listener, path)?;
        directory_fd
            .sync_all()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        failpoint("after_private_socket_publish")?;
        let final_readback = open_validated_private_artifact_at(
            directory_fd.as_raw_fd(),
            final_leaf,
            PrivateArtifactKind::Socket,
            Some(identity),
        )?
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        if artifact_identity(
            &final_readback
                .metadata()
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        ) != identity
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
        directory.revalidate().map_err(map_evidence)?;
        Ok((listener, identity))
    })();
    if result.is_err()
        && temp_owned
        && temp_identity.is_some_and(|identity| {
            validate_private_artifact_at(
                directory_fd.as_raw_fd(),
                &temp_leaf,
                PrivateArtifactKind::PartialSocket,
                Some(identity),
            ) == Ok(true)
        })
        && unsafe { libc::unlinkat(directory_fd.as_raw_fd(), temp_leaf.as_ptr(), 0) } == 0
    {
        let _ = directory_fd.sync_all();
    }
    result
}

#[cfg(target_os = "linux")]
fn prove_seqpacket_listener_path_continuity(listener: &File, path: &Path) -> ContainmentResult<()> {
    let client = connect_seqpacket_nonblocking(path)?;
    let token = Uuid::new_v4().into_bytes();
    if unsafe {
        libc::send(
            client.as_raw_fd(),
            token.as_ptr().cast(),
            token.len(),
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    } != token.len() as isize
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let mut pollfd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pollfd, 1, 250) } != 1 || pollfd.revents & libc::POLLIN == 0 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let accepted_fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        )
    };
    if accepted_fd < 0 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let accepted = unsafe { File::from_raw_fd(accepted_fd) };
    let mut accepted_pollfd = libc::pollfd {
        fd: accepted.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut accepted_pollfd, 1, 250) } != 1
        || accepted_pollfd.revents & libc::POLLIN == 0
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let mut observed = [0_u8; 16];
    if unsafe {
        libc::recv(
            accepted.as_raw_fd(),
            observed.as_mut_ptr().cast(),
            observed.len(),
            libc::MSG_DONTWAIT,
        )
    } != observed.len() as isize
        || observed != token
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::zeroed();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            accepted.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    } != 0
        || length as usize != std::mem::size_of::<libc::ucred>()
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let credentials = unsafe { credentials.assume_init() };
    if credentials.pid != std::process::id() as libc::pid_t
        || credentials.uid != unsafe { libc::geteuid() }
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn connect_seqpacket_nonblocking(path: &Path) -> ContainmentResult<File> {
    let bytes = path.as_os_str().as_bytes();
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_un>::zeroed();
    let address_mut = unsafe { address.assume_init_mut() };
    if bytes.is_empty() || bytes.len() >= address_mut.sun_path.len() {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    address_mut.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address_mut.sun_path.iter_mut().zip(bytes) {
        *target = *source as libc::c_char;
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(ContainmentErrorCode::Unsupported);
    }
    let socket = unsafe { File::from_raw_fd(fd) };
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
        .try_into()
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            address_mut as *const libc::sockaddr_un as *const libc::sockaddr,
            length,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(ContainmentErrorCode::PeerRejected);
        }
        let mut pollfd = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pollfd, 1, 250) } != 1 || pollfd.revents & libc::POLLOUT == 0 {
            return Err(ContainmentErrorCode::PeerRejected);
        }
        let mut socket_error = 0;
        let mut socket_error_len = std::mem::size_of::<i32>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut socket_error as *mut i32).cast(),
                &mut socket_error_len,
            )
        } != 0
            || socket_error != 0
        {
            return Err(ContainmentErrorCode::PeerRejected);
        }
    }
    Ok(socket)
}

#[cfg(target_os = "linux")]
fn bind_seqpacket_listener(path: &Path) -> ContainmentResult<File> {
    let bytes = path.as_os_str().as_bytes();
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_un>::zeroed();
    let address_mut = unsafe { address.assume_init_mut() };
    if bytes.is_empty() || bytes.len() >= address_mut.sun_path.len() {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    address_mut.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address_mut.sun_path.iter_mut().zip(bytes) {
        *target = *source as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(ContainmentErrorCode::Unsupported);
    }
    let listener = unsafe { File::from_raw_fd(fd) };
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
        .try_into()
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    if unsafe {
        libc::bind(
            listener.as_raw_fd(),
            address_mut as *const libc::sockaddr_un as *const libc::sockaddr,
            length,
        )
    } != 0
        || unsafe { libc::listen(listener.as_raw_fd(), 1) } != 0
    {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    Ok(listener)
}

#[cfg(target_os = "linux")]
fn connect_seqpacket_test(path: &Path) -> ContainmentResult<File> {
    let bytes = path.as_os_str().as_bytes();
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_un>::zeroed();
    let address_mut = unsafe { address.assume_init_mut() };
    if bytes.is_empty() || bytes.len() >= address_mut.sun_path.len() {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    address_mut.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address_mut.sun_path.iter_mut().zip(bytes) {
        *target = *source as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(ContainmentErrorCode::Unsupported);
    }
    let socket = unsafe { File::from_raw_fd(fd) };
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
        .try_into()
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    if unsafe {
        libc::connect(
            socket.as_raw_fd(),
            address_mut as *const libc::sockaddr_un as *const libc::sockaddr,
            length,
        )
    } != 0
    {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    Ok(socket)
}

#[cfg(target_os = "linux")]
fn accept_authenticated_peer(
    listener: &File,
    controller: &ManagedController,
    membership: &ManagedCgroupMembership,
    identity: RunnerIdentity,
    deadline: ContainmentDeadline,
) -> ContainmentResult<File> {
    validate_peer_binding(membership, identity)?;
    failpoint("before_control_accept")?;
    wait_fd_until(listener, libc::POLLIN, deadline)?;
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    let peer = unsafe { File::from_raw_fd(fd) };
    set_socket_timeout(&peer, deadline.remaining()?)?;
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::zeroed();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            peer.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    } != 0
        || length as usize != std::mem::size_of::<libc::ucred>()
    {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    let credentials = unsafe { credentials.assume_init() };
    if credentials.uid != unsafe { libc::geteuid() } || credentials.pid <= 0 {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    controller
        .prove_descendant_in_cgroup(credentials.pid as u32, membership)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    prove_namespace_isolation(credentials.pid as u32)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    Ok(peer)
}

#[cfg(target_os = "linux")]
fn validate_peer_binding(
    membership: &ManagedCgroupMembership,
    identity: RunnerIdentity,
) -> ContainmentResult<()> {
    if membership.candidate_index() != identity.candidate_index
        || membership.generation() != identity.generation
    {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_socket_timeout(socket: &File, timeout: Duration) -> ContainmentResult<()> {
    let value = libc::timeval {
        tv_sec: timeout.as_secs().try_into().unwrap_or(libc::time_t::MAX),
        tv_usec: timeout.subsec_micros().into(),
    };
    for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&value as *const libc::timeval).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        } != 0
        {
            return Err(ContainmentErrorCode::PeerRejected);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn wait_fd_until(
    file: &File,
    events: libc::c_short,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    loop {
        let remaining = deadline.remaining()?;
        let millis = remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: file.as_raw_fd(),
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, millis.max(1)) };
        if result > 0 {
            if descriptor.revents & libc::POLLNVAL != 0 || descriptor.revents & libc::POLLERR != 0 {
                return Err(ContainmentErrorCode::EvidenceUnavailable);
            }
            if descriptor.revents & (events | libc::POLLHUP) != 0 {
                return Ok(());
            }
        } else if result == 0 {
            return Err(ContainmentErrorCode::Timeout);
        } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
    }
}

#[cfg(target_os = "linux")]
fn managed_membership(
    node: &RetainedCgroupNode,
    identity: RunnerIdentity,
) -> ContainmentResult<ManagedCgroupMembership> {
    ManagedCgroupMembership::new(
        node.membership.clone(),
        ManagedCgroupDirectoryIdentity {
            boot_uuid: node.identity.boot_uuid,
            dev: node.identity.dev,
            ino: node.identity.ino,
            statx_mnt_id_unique: node.identity.statx_mnt_id_unique,
        },
        identity.candidate_index,
        identity.generation,
    )
    .map_err(|_| ContainmentErrorCode::InvalidIdentity)
}

#[cfg(target_os = "linux")]
fn open_cgroup_procs(node: &RetainedCgroupNode) -> ContainmentResult<File> {
    revalidate_cgroup_node(node)?;
    let fd = unsafe {
        libc::openat(
            node.file.as_raw_fd(),
            c"cgroup.procs".as_ptr(),
            libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn clone_file(file: &File) -> ContainmentResult<File> {
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn placement_descriptor_evidence(
    file: &File,
    payload: &RetainedCgroupNode,
    identity: RunnerIdentity,
) -> ContainmentResult<PlacementDescriptorEvidence> {
    const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
    revalidate_cgroup_node(payload)?;
    let metadata = file
        .metadata()
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
    let independently_opened = open_cgroup_procs(payload)?;
    let independently_opened_metadata = independently_opened
        .metadata()
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
    if metadata.dev() != independently_opened_metadata.dev()
        || metadata.ino() != independently_opened_metadata.ino()
    {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BASIC_STATS | STATX_MNT_ID_UNIQUE,
            statx.as_mut_ptr(),
        )
    } != 0
    {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & STATX_MNT_ID_UNIQUE == 0
        || statx.stx_mnt_id != payload.identity.statx_mnt_id_unique
    {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    let evidence = PlacementDescriptorEvidence {
        kind: PlacementDescriptorKind::PayloadCgroupProcs,
        file_dev: metadata.dev(),
        file_ino: metadata.ino(),
        file_statx_mnt_id_unique: statx.stx_mnt_id,
        payload_dev: payload.identity.dev,
        payload_ino: payload.identity.ino,
        payload_statx_mnt_id_unique: payload.identity.statx_mnt_id_unique,
        candidate_index: identity.candidate_index,
        generation: identity.generation,
    };
    evidence
        .validate(identity)
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
    revalidate_cgroup_node(payload)?;
    Ok(evidence)
}

#[cfg(target_os = "linux")]
fn send_validated_frame(
    peer: &File,
    validator: &mut SequenceValidator,
    message: ControlMessage,
    identity: RunnerIdentity,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    let frame = ControlFrame::new(identity, validator.next_sequence(), message);
    validator
        .accept(&frame)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    wait_fd_until(peer, libc::POLLOUT, deadline)?;
    send_frame_packet(peer, &frame).map_err(|_| ContainmentErrorCode::PeerRejected)
}

#[cfg(target_os = "linux")]
fn receive_validated_frame(
    peer: &File,
    validator: &mut SequenceValidator,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ControlFrame> {
    wait_fd_until(peer, libc::POLLIN, deadline)?;
    let frame = receive_frame_packet(peer).map_err(|_| ContainmentErrorCode::PeerRejected)?;
    validator
        .accept(&frame)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    Ok(frame)
}

#[cfg(target_os = "linux")]
fn read_single_cgroup_pid(node: &RetainedCgroupNode) -> ContainmentResult<u32> {
    let bytes = read_leaf(node, c"cgroup.procs", 4096)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| ContainmentErrorCode::PlacementUnproven)?;
    let mut lines = text.lines();
    let pid = lines
        .next()
        .and_then(|line| line.parse::<u32>().ok())
        .filter(|pid| *pid != 0)
        .ok_or(ContainmentErrorCode::PlacementUnproven)?;
    if lines.next().is_some() {
        return Err(ContainmentErrorCode::PlacementUnproven);
    }
    Ok(pid)
}

#[cfg(debug_assertions)]
pub(crate) fn dispatch_internal_containment_test_driver(
    arguments: &[OsString],
) -> ContainmentResult<bool> {
    let mode = arguments.get(1).and_then(|value| value.to_str());
    if mode == Some("--internal-speculation-peer-connect-test-v1") {
        if std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return Err(ContainmentErrorCode::Unsupported);
        }
        #[cfg(not(target_os = "linux"))]
        return Err(ContainmentErrorCode::Unsupported);
        #[cfg(target_os = "linux")]
        {
            let path = std::env::var_os("LTERM_INTERNAL_SPECULATION_PEER_SOCKET")
                .map(PathBuf::from)
                .ok_or(ContainmentErrorCode::InvalidIdentity)?;
            let mut peer = connect_seqpacket_test(&path)?;
            let mut byte = [0_u8; 1];
            let _ = peer.read(&mut byte);
            return Ok(true);
        }
    }
    if mode == Some("--internal-speculation-startup-config-test-v1") {
        if std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return Err(ContainmentErrorCode::Unsupported);
        }
        #[cfg(not(target_os = "linux"))]
        return Err(ContainmentErrorCode::Unsupported);
        #[cfg(target_os = "linux")]
        {
            crate::speculation_service::SpeculationService::run_startup_config_capture_test_driver(
            )
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            println!("speculation-startup-config-captured=1");
            return Ok(true);
        }
    }
    if mode != Some("--internal-speculation-containment-test-v1") {
        return Ok(false);
    }
    if std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Err(ContainmentErrorCode::Unsupported);
    }
    #[cfg(not(target_os = "linux"))]
    return Err(ContainmentErrorCode::Unsupported);
    #[cfg(target_os = "linux")]
    {
        initialize_speculation_process_config();
        if std::env::var_os("LTERM_INTERNAL_SPECULATION_ACTOR_SERVICE").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            crate::speculation_service::run_real_actor_service_driver()?;
            println!("speculation-actor-service=1");
            return Ok(true);
        }
        if let Some(mode) = std::env::var_os("LTERM_INTERNAL_SPECULATION_RESTART_MODE") {
            if matches!(mode.as_bytes(), b"crash-cleanup" | b"recover-cleanup") {
                run_cleanup_restart_driver(&mode)?;
                println!("speculation-cleanup-recovered=1");
                return Ok(true);
            }
            if matches!(mode.as_bytes(), b"crash-runtime" | b"recover-runtime") {
                run_runtime_restart_driver(&mode)?;
                println!("speculation-runtime-recovered=1");
                return Ok(true);
            }
            let adopted = run_create_restart_driver(&mode)?;
            println!("speculation-restart-recovered=1 adopted={adopted}");
            return Ok(true);
        }
        let endpoint_only = std::env::var_os("LTERM_INTERNAL_SPECULATION_ENDPOINT_ONLY").as_deref()
            == Some(std::ffi::OsStr::new("1"));
        run_real_component_driver(endpoint_only)?;
        if endpoint_only {
            println!("speculation-endpoint-probes=2");
        } else {
            println!("speculation-real-cases=14");
        }
        Ok(true)
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_runtime_restart_driver(mode: &std::ffi::OsStr) -> ContainmentResult<()> {
    let fixture = std::env::var_os("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let record_path = fixture.join("restart-record.json");
    match mode.as_bytes() {
        b"crash-runtime" => run_runtime_crash_driver(&fixture, &record_path),
        b"recover-runtime" => {
            let mut record = read_restart_record(&record_path)?;
            if fixture.join("expect-no-candidate-exec").exists() {
                prove_no_candidate_exec(&record, &fixture)?;
            }
            let owner = ManagedOwnerTag {
                kind: ManagedOwnerKind::Speculation,
                tournament_uuid: record.status.tournament_uuid,
                candidate_index: 0,
                role: ManagedOwnerRole::Runner,
            };
            let private_cleanup = (|| match reconcile_managed_owner(&owner)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            {
                ManagedOwnerOutcome::Absent => Ok(()),
                ManagedOwnerOutcome::ResolvedTombstone(_) => {
                    let control_root = open_existing_private_dir(&fixture.join("control"))
                        .map_err(map_evidence)?;
                    let managed = reconcile_managed_processes()
                        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
                    reconcile_private_runner_controls(&control_root, &managed)
                }
                ManagedOwnerOutcome::UnknownOrphanRisk(_) => {
                    Err(ContainmentErrorCode::EvidenceUnavailable)
                }
            })();
            cleanup_restart_record(&record_path, &mut record)?;
            private_cleanup
        }
        _ => Err(ContainmentErrorCode::InvalidIdentity),
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_runtime_crash_driver(fixture: &Path, record_path: &Path) -> ContainmentResult<()> {
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::Unsupported)?;
    let tournament_uuid = std::env::var("LTERM_INTERNAL_SPECULATION_TOURNAMENT_UUID")
        .ok()
        .and_then(|value| Uuid::parse_str(&value).ok())
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let failpoint = std::env::var("LTERM_INTERNAL_SPECULATION_FAILPOINT")
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    if failpoint.starts_with("runner_") {
        std::fs::write(fixture.join("expect-no-candidate-exec"), b"1\n")
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    let source = fixture.join("source");
    let candidates = [fixture.join("candidate-0"), fixture.join("candidate-1")];
    let ledger = fixture.join("ledger");
    let control = fixture.join("control");
    for path in [&source, &candidates[0], &candidates[1], &ledger, &control] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    let argv = if failpoint.starts_with("runner_") {
        vec![
            OsString::from("/usr/bin/touch"),
            OsString::from("/workspace/candidate-executed"),
        ]
    } else {
        vec![OsString::from("/usr/bin/sleep"), OsString::from("30")]
    };
    let context = validate_prepare(
        PrepareInputs {
            tournament_uuid,
            generation: 1,
            source,
            candidates,
            ledger_root: ledger,
            cgroup_root,
            control_root: control,
            argv,
        },
        ContainmentDeadline::control_action(),
    )?;
    let mut record = restart_record_from_context(&context)?;
    let mut topology = begin_topology(&context)?;
    for action in [
        TopologyAction::CreateTournamentDomain,
        TopologyAction::CreateCandidateParent { candidate: 0 },
        TopologyAction::CreateControlLeaf { candidate: 0 },
        TopologyAction::CreatePayloadLeaf { candidate: 0 },
    ] {
        let evidence = create_topology(&mut topology, action)?;
        apply_topology_evidence(&mut record, evidence)?;
        persist_restart_record(record_path, &record)?;
    }
    create_topology(
        &mut topology,
        TopologyAction::ConfigurePayloadLimit { candidate: 0 },
    )?;
    persist_restart_record(record_path, &record)?;

    let containment = launch_runner(
        &context,
        &topology,
        0,
        ContainmentDeadline::control_action(),
    )?;
    let observation = containment.observation();
    record.managed_owners[0] = Some(observation.managed_owner);
    persist_restart_record(record_path, &record)?;
    let CandidateContainmentParts {
        mut control,
        mut observer,
        ..
    } = containment.split();
    transfer_payload_fd(
        &mut control,
        topology.candidate(0)?,
        ContainmentDeadline::control_action(),
    )?;
    receive_payload_fd_ack(&mut observer, ContainmentDeadline::control_action())?;
    send_go(&mut control, ContainmentDeadline::control_action())?;
    receive_go_receipt(&mut observer, ContainmentDeadline::control_action())?;
    let placement = receive_payload_placed(
        &mut observer,
        topology.candidate(0)?,
        ContainmentDeadline::control_action(),
    )?;
    if failpoint == "runner_duplicate_payload_release" {
        send_duplicate_stale_payload_release(&mut control)?;
        unsafe { libc::_exit(86) };
    }
    if failpoint == "runner_payload_release_after_rollback" {
        perform_candidate_cleanup_action(
            &mut topology,
            0,
            CandidateCleanupAction::KillPayload,
            ContainmentDeadline::control_action(),
        )?;
        perform_candidate_cleanup_action(
            &mut topology,
            0,
            CandidateCleanupAction::ProvePayloadEmpty,
            ContainmentDeadline::control_action(),
        )?;
        let _ = send_payload_release(
            &mut control,
            placement,
            ContainmentDeadline::control_action(),
        );
        unsafe { libc::_exit(86) };
    }
    send_payload_release(
        &mut control,
        placement,
        ContainmentDeadline::control_action(),
    )?;

    if failpoint == "runner_before_child_exec" {
        receive_execution_event(&mut observer, ContainmentDeadline::control_action())?;
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let edge = failpoint
        .strip_prefix("before_")
        .or_else(|| failpoint.strip_prefix("after_"))
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    match edge {
        "payload_kill" => {
            perform_candidate_cleanup_action(
                &mut topology,
                0,
                CandidateCleanupAction::KillPayload,
                ContainmentDeadline::control_action(),
            )?;
        }
        "payload_empty_proof" => {
            perform_candidate_cleanup_action(
                &mut topology,
                0,
                CandidateCleanupAction::KillPayload,
                ContainmentDeadline::control_action(),
            )?;
            perform_candidate_cleanup_action(
                &mut topology,
                0,
                CandidateCleanupAction::ProvePayloadEmpty,
                ContainmentDeadline::control_action(),
            )?;
        }
        "parent_kill" => {
            perform_candidate_cleanup_action(
                &mut topology,
                0,
                CandidateCleanupAction::KillParent,
                ContainmentDeadline::control_action(),
            )?;
        }
        "parent_empty_proof" => {
            perform_candidate_cleanup_action(
                &mut topology,
                0,
                CandidateCleanupAction::KillParent,
                ContainmentDeadline::control_action(),
            )?;
            perform_candidate_cleanup_action(
                &mut topology,
                0,
                CandidateCleanupAction::ProveParentEmpty,
                ContainmentDeadline::control_action(),
            )?;
        }
        _ => return Err(ContainmentErrorCode::EvidenceUnavailable),
    }
    Err(ContainmentErrorCode::EvidenceUnavailable)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn prove_no_candidate_exec(record: &TournamentRecord, fixture: &Path) -> ContainmentResult<()> {
    for candidate in ["candidate-0", "candidate-1"] {
        let path = fixture.join(candidate);
        if path.is_dir()
            && std::fs::read_dir(path)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
                .next()
                .is_some()
        {
            return Err(ContainmentErrorCode::TerminalBoundaryFailure);
        }
    }
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::Unsupported)?;
    let payload = cgroup_root
        .join(format!("lterm-g003-{}", record.status.tournament_uuid))
        .join("candidate-0")
        .join("payload")
        .join("cgroup.procs");
    let pids =
        std::fs::read_to_string(payload).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    for pid in pids.lines() {
        let executable = match std::fs::read_link(format!("/proc/{pid}/exe")) {
            Ok(executable) => executable,
            Err(_) => continue,
        };
        if matches!(
            executable.as_path(),
            path if path == Path::new("/usr/bin/sleep") || path == Path::new("/usr/bin/touch")
        ) {
            return Err(ContainmentErrorCode::TerminalBoundaryFailure);
        }
    }
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn send_duplicate_stale_payload_release(control: &mut CandidateControl) -> ContainmentResult<()> {
    let protocol = control
        .protocol
        .lock()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let sequence = protocol
        .validator
        .next_sequence()
        .checked_sub(1)
        .ok_or(ContainmentErrorCode::PeerRejected)?;
    let duplicate = ControlFrame::new(control.identity, sequence, ControlMessage::PayloadRelease);
    send_frame_packet(&control.peer, &duplicate).map_err(|_| ContainmentErrorCode::PeerRejected)?;
    let _ = send_frame_packet(&control.peer, &duplicate);
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_cleanup_restart_driver(mode: &std::ffi::OsStr) -> ContainmentResult<()> {
    let fixture = std::env::var_os("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let record_path = fixture.join("restart-record.json");
    match mode.as_bytes() {
        b"crash-cleanup" => run_cleanup_crash_driver(&fixture, &record_path),
        b"recover-cleanup" => {
            let mut record = read_restart_record(&record_path)?;
            cleanup_restart_record(&record_path, &mut record)
        }
        _ => Err(ContainmentErrorCode::InvalidIdentity),
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_cleanup_crash_driver(fixture: &Path, record_path: &Path) -> ContainmentResult<()> {
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::Unsupported)?;
    let tournament_uuid = std::env::var("LTERM_INTERNAL_SPECULATION_TOURNAMENT_UUID")
        .ok()
        .and_then(|value| Uuid::parse_str(&value).ok())
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let candidate = std::env::var("LTERM_INTERNAL_SPECULATION_CREATE_CANDIDATE")
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(0);
    if candidate > 1 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let source = fixture.join("source");
    let candidates = [fixture.join("candidate-0"), fixture.join("candidate-1")];
    let ledger = fixture.join("ledger");
    let control = fixture.join("control");
    for path in [&source, &candidates[0], &candidates[1], &ledger, &control] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    let context = validate_prepare(
        PrepareInputs {
            tournament_uuid,
            generation: 1,
            source,
            candidates,
            ledger_root: ledger,
            cgroup_root,
            control_root: control,
            argv: vec![OsString::from("/usr/bin/true")],
        },
        ContainmentDeadline::control_action(),
    )?;
    let mut record = restart_record_from_context(&context)?;
    let mut topology = begin_topology(&context)?;
    for action in [
        TopologyAction::CreateTournamentDomain,
        TopologyAction::CreateCandidateParent { candidate },
        TopologyAction::CreateControlLeaf { candidate },
        TopologyAction::CreatePayloadLeaf { candidate },
    ] {
        let evidence = create_topology(&mut topology, action)?;
        apply_topology_evidence(&mut record, evidence)?;
    }
    create_topology(
        &mut topology,
        TopologyAction::ConfigurePayloadLimit { candidate },
    )?;
    persist_restart_record(record_path, &record)?;

    let failpoint = std::env::var("LTERM_INTERNAL_SPECULATION_FAILPOINT")
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    let target = cleanup_restart_target(&failpoint, candidate)?;
    match target {
        CleanupRestartTarget::Candidate(action) => {
            prepare_candidate_cleanup_action(record_path, &mut record, candidate, action)?;
            reconcile_valid_record(&record, action, ContainmentDeadline::control_action())?;
        }
        CleanupRestartTarget::Tournament(action) => {
            complete_candidate_to_removed(record_path, &mut record, candidate)?;
            let other = 1_u8 - candidate;
            record.cgroups[usize::from(other)].lifecycle = CgroupLifecycleState::Removed;
            persist_restart_record(record_path, &record)?;
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::RemovePending;
            persist_restart_record(record_path, &record)?;
            if action == RecoveryAction::RemoveTournamentDomain {
                require_recovery_action_complete(reconcile_valid_record(
                    &record,
                    RecoveryAction::ProveTournamentEmpty,
                    ContainmentDeadline::control_action(),
                )?)?;
            }
            reconcile_valid_record(&record, action, ContainmentDeadline::control_action())?;
        }
        CleanupRestartTarget::LiveTournament => {
            run_live_tournament_empty_crash(
                fixture,
                record_path,
                &mut record,
                &mut topology,
                candidate,
            )?;
        }
    }
    Err(ContainmentErrorCode::EvidenceUnavailable)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
#[derive(Clone, Copy)]
enum CleanupRestartTarget {
    Candidate(RecoveryAction),
    Tournament(RecoveryAction),
    LiveTournament,
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn cleanup_restart_target(name: &str, candidate: u8) -> ContainmentResult<CleanupRestartTarget> {
    let target = match name
        .strip_prefix("before_")
        .or_else(|| name.strip_prefix("after_"))
    {
        Some("recovery_parent_kill") => {
            CleanupRestartTarget::Candidate(RecoveryAction::KillParent { candidate })
        }
        Some("recovery_parent_empty_proof") => {
            CleanupRestartTarget::Candidate(RecoveryAction::ProveParentEmpty { candidate })
        }
        Some("payload_remove") => {
            CleanupRestartTarget::Candidate(RecoveryAction::RemovePayload { candidate })
        }
        Some("control_remove") => {
            CleanupRestartTarget::Candidate(RecoveryAction::RemoveControl { candidate })
        }
        Some("parent_remove") => {
            CleanupRestartTarget::Candidate(RecoveryAction::RemoveParent { candidate })
        }
        Some("recovery_tournament_empty_proof") => {
            CleanupRestartTarget::Tournament(RecoveryAction::ProveTournamentEmpty)
        }
        Some("tournament_empty_proof") => CleanupRestartTarget::LiveTournament,
        Some("tournament_remove") => {
            CleanupRestartTarget::Tournament(RecoveryAction::RemoveTournamentDomain)
        }
        _ => return Err(ContainmentErrorCode::InvalidIdentity),
    };
    Ok(target)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_live_tournament_empty_crash(
    fixture: &Path,
    record_path: &Path,
    record: &mut TournamentRecord,
    topology: &mut TournamentTopology,
    candidate: u8,
) -> ContainmentResult<()> {
    let index = usize::from(candidate);
    let from = match record.cgroups[index].lifecycle {
        CgroupLifecycleState::Forward(from) if from != CgroupForwardState::Planned => from,
        _ => return Err(ContainmentErrorCode::EvidenceUnavailable),
    };
    let mut live = std::process::Command::new("/usr/bin/sleep")
        .arg("30")
        .spawn()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let live_pid = live.id();
    let cleanup_result = (|| {
        let candidate_topology = topology.candidate(candidate)?;
        write_leaf(
            candidate_topology.payload()?,
            c"cgroup.procs",
            format!("{live_pid}\n").as_bytes(),
        )?;
        wait_populated_one(
            candidate_topology.payload()?,
            ContainmentDeadline::control_action(),
        )?;
        wait_populated_one(
            candidate_topology
                .parent
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?,
            ContainmentDeadline::control_action(),
        )?;
        let domain = topology
            .domain
            .as_ref()
            .ok_or(ContainmentErrorCode::TopologyFailure)?;
        wait_populated_one(domain, ContainmentDeadline::control_action())?;
        persist_live_cleanup_evidence(
            fixture,
            "live-topology-populated",
            format!("{live_pid}\n").as_bytes(),
        )?;

        record.cgroups[index].lifecycle = CgroupLifecycleState::ParentKillPending { from };
        persist_restart_record(record_path, record)?;
        perform_candidate_cleanup_action(
            topology,
            candidate,
            CandidateCleanupAction::KillParent,
            ContainmentDeadline::control_action(),
        )?;
        perform_candidate_cleanup_action(
            topology,
            candidate,
            CandidateCleanupAction::ProveParentEmpty,
            ContainmentDeadline::control_action(),
        )?;
        Ok(())
    })();
    if cleanup_result.is_err() {
        let _ = live.kill();
    }
    live.wait()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    cleanup_result?;
    persist_live_cleanup_evidence(fixture, "live-topology-quiescent", b"1\n")?;

    record.cgroups[index].lifecycle = CgroupLifecycleState::ParentEmpty { from };
    persist_restart_record(record_path, record)?;
    record.cgroups[index].lifecycle = CgroupLifecycleState::PayloadRemovePending { from };
    persist_restart_record(record_path, record)?;
    perform_candidate_cleanup_action(
        topology,
        candidate,
        CandidateCleanupAction::RemovePayload,
        ContainmentDeadline::control_action(),
    )?;
    record.cgroups[index].payload = None;
    record.cgroups[index].lifecycle = CgroupLifecycleState::PayloadRemoved { from };
    persist_restart_record(record_path, record)?;

    record.cgroups[index].lifecycle = CgroupLifecycleState::ControlRemovePending { from };
    persist_restart_record(record_path, record)?;
    perform_candidate_cleanup_action(
        topology,
        candidate,
        CandidateCleanupAction::RemoveControl,
        ContainmentDeadline::control_action(),
    )?;
    record.cgroups[index].control = None;
    record.cgroups[index].lifecycle = CgroupLifecycleState::ControlRemoved { from };
    persist_restart_record(record_path, record)?;

    record.cgroups[index].lifecycle = CgroupLifecycleState::ParentRemovePending { from };
    persist_restart_record(record_path, record)?;
    perform_candidate_cleanup_action(
        topology,
        candidate,
        CandidateCleanupAction::RemoveParent,
        ContainmentDeadline::control_action(),
    )?;
    record.cgroups[index].parent = None;
    record.cgroups[index].lifecycle = CgroupLifecycleState::Removed;
    let other = usize::from(1_u8 - candidate);
    record.cgroups[other].lifecycle = CgroupLifecycleState::Removed;
    record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::RemovePending;
    persist_restart_record(record_path, record)?;

    perform_tournament_cleanup_action(
        topology,
        TournamentCleanupAction::ProveEmpty,
        ContainmentDeadline::control_action(),
    )?;
    Err(ContainmentErrorCode::EvidenceUnavailable)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn wait_populated_one(
    node: &RetainedCgroupNode,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    loop {
        if parse_populated(&read_leaf(node, c"cgroup.events", 4096)?)? == 1 {
            return Ok(());
        }
        deadline.remaining()?;
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn persist_live_cleanup_evidence(
    fixture: &Path,
    name: &str,
    bytes: &[u8],
) -> ContainmentResult<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options
        .open(fixture.join(name))
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    file.write_all(bytes)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    file.sync_all()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    File::open(fixture)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn prepare_candidate_cleanup_action(
    record_path: &Path,
    record: &mut TournamentRecord,
    candidate: u8,
    target: RecoveryAction,
) -> ContainmentResult<()> {
    let index = usize::from(candidate);
    let from = match record.cgroups[index].lifecycle {
        CgroupLifecycleState::Forward(from) if from != CgroupForwardState::Planned => from,
        _ => return Err(ContainmentErrorCode::EvidenceUnavailable),
    };
    record.cgroups[index].lifecycle = CgroupLifecycleState::ParentKillPending { from };
    persist_restart_record(record_path, record)?;
    if target == (RecoveryAction::KillParent { candidate }) {
        return Ok(());
    }
    require_recovery_action_complete(reconcile_valid_record(
        record,
        RecoveryAction::KillParent { candidate },
        ContainmentDeadline::control_action(),
    )?)?;
    if target == (RecoveryAction::ProveParentEmpty { candidate }) {
        return Ok(());
    }
    require_recovery_action_complete(reconcile_valid_record(
        record,
        RecoveryAction::ProveParentEmpty { candidate },
        ContainmentDeadline::control_action(),
    )?)?;
    record.cgroups[index].lifecycle = CgroupLifecycleState::ParentEmpty { from };
    persist_restart_record(record_path, record)?;

    record.cgroups[index].lifecycle = CgroupLifecycleState::PayloadRemovePending { from };
    persist_restart_record(record_path, record)?;
    if target == (RecoveryAction::RemovePayload { candidate }) {
        return Ok(());
    }
    require_recovery_action_complete(reconcile_valid_record(
        record,
        RecoveryAction::RemovePayload { candidate },
        ContainmentDeadline::control_action(),
    )?)?;
    record.cgroups[index].payload = None;
    record.cgroups[index].lifecycle = CgroupLifecycleState::PayloadRemoved { from };
    persist_restart_record(record_path, record)?;

    record.cgroups[index].lifecycle = CgroupLifecycleState::ControlRemovePending { from };
    persist_restart_record(record_path, record)?;
    if target == (RecoveryAction::RemoveControl { candidate }) {
        return Ok(());
    }
    require_recovery_action_complete(reconcile_valid_record(
        record,
        RecoveryAction::RemoveControl { candidate },
        ContainmentDeadline::control_action(),
    )?)?;
    record.cgroups[index].control = None;
    record.cgroups[index].lifecycle = CgroupLifecycleState::ControlRemoved { from };
    persist_restart_record(record_path, record)?;

    record.cgroups[index].lifecycle = CgroupLifecycleState::ParentRemovePending { from };
    persist_restart_record(record_path, record)?;
    if target == (RecoveryAction::RemoveParent { candidate }) {
        return Ok(());
    }
    Err(ContainmentErrorCode::InvalidIdentity)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn complete_candidate_to_removed(
    record_path: &Path,
    record: &mut TournamentRecord,
    candidate: u8,
) -> ContainmentResult<()> {
    prepare_candidate_cleanup_action(
        record_path,
        record,
        candidate,
        RecoveryAction::RemoveParent { candidate },
    )?;
    require_recovery_action_complete(reconcile_valid_record(
        record,
        RecoveryAction::RemoveParent { candidate },
        ContainmentDeadline::control_action(),
    )?)?;
    let candidate = &mut record.cgroups[usize::from(candidate)];
    candidate.parent = None;
    candidate.lifecycle = CgroupLifecycleState::Removed;
    persist_restart_record(record_path, record)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_create_restart_driver(mode: &std::ffi::OsStr) -> ContainmentResult<bool> {
    let fixture = std::env::var_os("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let record_path = fixture.join("restart-record.json");
    match mode.as_bytes() {
        b"crash-create" => run_create_crash_driver(&fixture, &record_path).map(|()| false),
        b"recover-create" => {
            let mut record = read_restart_record(&record_path)?;
            let action = pending_create_recovery_action(&record)?;
            let evidence =
                reconcile_valid_record(&record, action, ContainmentDeadline::control_action())?;
            let adopted = match evidence {
                RecoveryEvidence::TournamentCreateReconciled { adopted, .. }
                | RecoveryEvidence::CandidateCreateReconciled { adopted, .. } => adopted,
                _ => return Err(ContainmentErrorCode::EvidenceUnavailable),
            };
            apply_recovered_create(&mut record, evidence)?;
            persist_restart_record(&record_path, &record)?;
            cleanup_restart_record(&record_path, &mut record)?;
            Ok(adopted)
        }
        _ => Err(ContainmentErrorCode::InvalidIdentity),
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_create_crash_driver(fixture: &Path, record_path: &Path) -> ContainmentResult<()> {
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::Unsupported)?;
    let tournament_uuid = std::env::var("LTERM_INTERNAL_SPECULATION_TOURNAMENT_UUID")
        .ok()
        .and_then(|value| Uuid::parse_str(&value).ok())
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let candidate = std::env::var("LTERM_INTERNAL_SPECULATION_CREATE_CANDIDATE")
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(0);
    if candidate > 1 {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let source = fixture.join("source");
    let candidates = [fixture.join("candidate-0"), fixture.join("candidate-1")];
    let ledger = fixture.join("ledger");
    let control = fixture.join("control");
    for path in [&source, &candidates[0], &candidates[1], &ledger, &control] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    let context = validate_prepare(
        PrepareInputs {
            tournament_uuid,
            generation: 1,
            source,
            candidates,
            ledger_root: ledger,
            cgroup_root,
            control_root: control,
            argv: vec![OsString::from("/usr/bin/true")],
        },
        ContainmentDeadline::control_action(),
    )?;
    let mut record = restart_record_from_context(&context)?;
    let mut topology = begin_topology(&context)?;
    let failpoint = std::env::var("LTERM_INTERNAL_SPECULATION_FAILPOINT")
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;

    if matches!(
        failpoint.as_str(),
        "before_tournament_create"
            | "after_tournament_create"
            | "before_pids_enable_write"
            | "after_pids_enable_write"
            | "before_pids_enable_readback"
            | "after_pids_enable_readback"
    ) {
        record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::CreatePending;
        persist_restart_record(record_path, &record)?;
        create_topology(&mut topology, TopologyAction::CreateTournamentDomain)?;
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    apply_topology_evidence(
        &mut record,
        create_topology(&mut topology, TopologyAction::CreateTournamentDomain)?,
    )?;

    if matches!(
        failpoint.as_str(),
        "before_candidate_parent_create" | "after_candidate_parent_create"
    ) {
        record.cgroups[usize::from(candidate)].lifecycle =
            CgroupLifecycleState::Forward(CgroupForwardState::ParentCreatePending);
        persist_restart_record(record_path, &record)?;
        create_topology(
            &mut topology,
            TopologyAction::CreateCandidateParent { candidate },
        )?;
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    apply_topology_evidence(
        &mut record,
        create_topology(
            &mut topology,
            TopologyAction::CreateCandidateParent { candidate },
        )?,
    )?;

    if matches!(
        failpoint.as_str(),
        "before_control_create" | "after_control_create"
    ) {
        record.cgroups[usize::from(candidate)].lifecycle =
            CgroupLifecycleState::Forward(CgroupForwardState::ControlCreatePending);
        persist_restart_record(record_path, &record)?;
        create_topology(
            &mut topology,
            TopologyAction::CreateControlLeaf { candidate },
        )?;
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    apply_topology_evidence(
        &mut record,
        create_topology(
            &mut topology,
            TopologyAction::CreateControlLeaf { candidate },
        )?,
    )?;

    if matches!(
        failpoint.as_str(),
        "before_payload_create" | "after_payload_create"
    ) {
        record.cgroups[usize::from(candidate)].lifecycle =
            CgroupLifecycleState::Forward(CgroupForwardState::PayloadCreatePending);
        persist_restart_record(record_path, &record)?;
        create_topology(
            &mut topology,
            TopologyAction::CreatePayloadLeaf { candidate },
        )?;
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    Err(ContainmentErrorCode::InvalidIdentity)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn restart_record_from_context(
    context: &LiveTournamentContext,
) -> ContainmentResult<TournamentRecord> {
    let candidate_uuids = [Uuid::new_v4(), Uuid::new_v4()];
    let candidate_status = |index: u8| crate::protocol::SpeculationCandidateStatus {
        candidate_uuid: candidate_uuids[usize::from(index)],
        index,
        ready: false,
        ready_elapsed_ns: None,
        go_received: false,
        go_received_elapsed_ns: None,
        result_accepted: false,
        exit_success: None,
        exit_category: None,
        elapsed_ns: None,
        output_bytes: None,
        eligible: false,
        cleanup: Default::default(),
    };
    let record = TournamentRecord {
        schema_version: crate::speculation_registry::TournamentRecordSchema::V1,
        boot_uuid: context.source.identity().boot_uuid,
        roots: crate::speculation_registry::PrivateRootIdentities {
            source: context.source.identity(),
            candidates: [
                context.candidates[0].identity(),
                context.candidates[1].identity(),
            ],
            ledger_root: context.ledger_root.identity(),
            cgroup_root: context.cgroup_root.identity(),
        },
        cgroup_root_locator: crate::speculation_registry::PrivateCgroupRootLocator::from_directory(
            &context.cgroup_root,
        )
        .map_err(map_evidence)?,
        tournament_cgroup: crate::speculation_registry::TournamentCgroupEvidence {
            deterministic_name_uuid: context.identity.tournament_uuid,
            lifecycle: TournamentCgroupLifecycleState::Planned,
            domain: None,
        },
        cgroups: std::array::from_fn(|index| {
            crate::speculation_registry::CandidateCgroupEvidence {
                candidate_index: index as u8,
                deterministic_name_uuid: candidate_uuids[index],
                lifecycle: CgroupLifecycleState::Forward(CgroupForwardState::Planned),
                parent: None,
                control: None,
                payload: None,
            }
        }),
        managed_owners: [None, None],
        restart_prior_phase: None,
        terminal_completed_unix_ms: None,
        status: crate::protocol::SpeculationStatus {
            schema_version: crate::protocol::SpeculationSchemaVersion::V1,
            tournament_uuid: context.identity.tournament_uuid,
            daemon_instance_uuid: Uuid::new_v4(),
            phase: crate::protocol::SpeculationPhase::Prepared,
            generation: context.identity.generation,
            lease_deadline_unix_ms: 1,
            reason_code: Some(crate::protocol::SpeculationReasonCode::PreparedLease),
            candidates: [candidate_status(0), candidate_status(1)],
            fixed_score_order: crate::protocol::SPECULATION_SCORE_ORDER,
            selected_index: None,
            rollback_required: false,
            error_codes: Default::default(),
        },
    };
    record
        .validate()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    Ok(record)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn apply_topology_evidence(
    record: &mut TournamentRecord,
    evidence: TopologyEvidence,
) -> ContainmentResult<()> {
    match evidence {
        TopologyEvidence::TournamentDomain(identity) => {
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Created;
            record.tournament_cgroup.domain = Some(identity);
        }
        TopologyEvidence::CandidateParent {
            candidate,
            identity,
        } => {
            let candidate = &mut record.cgroups[usize::from(candidate)];
            candidate.lifecycle = CgroupLifecycleState::Forward(CgroupForwardState::ParentCreated);
            candidate.parent = Some(identity);
        }
        TopologyEvidence::ControlLeaf {
            candidate,
            identity,
        } => {
            let candidate = &mut record.cgroups[usize::from(candidate)];
            candidate.lifecycle = CgroupLifecycleState::Forward(CgroupForwardState::ControlCreated);
            candidate.control = Some(identity);
        }
        TopologyEvidence::PayloadLeaf {
            candidate,
            identity,
        } => {
            let candidate = &mut record.cgroups[usize::from(candidate)];
            candidate.lifecycle = CgroupLifecycleState::Forward(CgroupForwardState::PayloadCreated);
            candidate.payload = Some(identity);
        }
        TopologyEvidence::PayloadLimit { .. } => {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
    }
    record
        .validate()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn pending_create_recovery_action(record: &TournamentRecord) -> ContainmentResult<RecoveryAction> {
    if record.tournament_cgroup.lifecycle == TournamentCgroupLifecycleState::CreatePending {
        return Ok(RecoveryAction::ReconcileTournamentCreate);
    }
    record
        .cgroups
        .iter()
        .find_map(|candidate| {
            let component = match candidate.lifecycle {
                CgroupLifecycleState::Forward(CgroupForwardState::ParentCreatePending) => {
                    CgroupComponent::Parent
                }
                CgroupLifecycleState::Forward(CgroupForwardState::ControlCreatePending) => {
                    CgroupComponent::Control
                }
                CgroupLifecycleState::Forward(CgroupForwardState::PayloadCreatePending) => {
                    CgroupComponent::Payload
                }
                _ => return None,
            };
            Some(RecoveryAction::ReconcileCandidateCreate {
                candidate: candidate.candidate_index,
                component,
            })
        })
        .ok_or(ContainmentErrorCode::InvalidIdentity)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn apply_recovered_create(
    record: &mut TournamentRecord,
    evidence: RecoveryEvidence,
) -> ContainmentResult<()> {
    match evidence {
        RecoveryEvidence::TournamentCreateReconciled { identity, .. } => {
            apply_topology_evidence(record, TopologyEvidence::TournamentDomain(identity))
        }
        RecoveryEvidence::CandidateCreateReconciled {
            candidate,
            component: CgroupComponent::Parent,
            identity,
            ..
        } => apply_topology_evidence(
            record,
            TopologyEvidence::CandidateParent {
                candidate,
                identity,
            },
        ),
        RecoveryEvidence::CandidateCreateReconciled {
            candidate,
            component: CgroupComponent::Control,
            identity,
            ..
        } => apply_topology_evidence(
            record,
            TopologyEvidence::ControlLeaf {
                candidate,
                identity,
            },
        ),
        RecoveryEvidence::CandidateCreateReconciled {
            candidate,
            component: CgroupComponent::Payload,
            identity,
            ..
        } => apply_topology_evidence(
            record,
            TopologyEvidence::PayloadLeaf {
                candidate,
                identity,
            },
        ),
        _ => Err(ContainmentErrorCode::EvidenceUnavailable),
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn persist_restart_record(path: &Path, record: &TournamentRecord) -> ContainmentResult<()> {
    record
        .validate()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let bytes =
        serde_json::to_vec(record).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if bytes.is_empty() || bytes.len() > 64 * 1024 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    file.write_all(&bytes)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    file.sync_all()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    File::open(
        path.parent()
            .ok_or(ContainmentErrorCode::EvidenceUnavailable)?,
    )
    .and_then(|directory| directory.sync_all())
    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn read_restart_record(path: &Path) -> ContainmentResult<TournamentRecord> {
    let bytes = std::fs::read(path).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if bytes.is_empty() || bytes.len() > 64 * 1024 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let record = serde_json::from_slice::<TournamentRecord>(&bytes)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    record
        .validate()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    Ok(record)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn cleanup_restart_record(
    record_path: &Path,
    record: &mut TournamentRecord,
) -> ContainmentResult<()> {
    for candidate_index in 0_u8..2 {
        let index = usize::from(candidate_index);
        loop {
            match record.cgroups[index].lifecycle {
                CgroupLifecycleState::Forward(CgroupForwardState::Planned) => {
                    record.cgroups[index].lifecycle = CgroupLifecycleState::Removed;
                    persist_restart_record(record_path, record)?;
                }
                CgroupLifecycleState::Forward(from)
                | CgroupLifecycleState::CleanupPending { from } => {
                    record.cgroups[index].lifecycle =
                        CgroupLifecycleState::ParentKillPending { from };
                    persist_restart_record(record_path, record)?;
                }
                CgroupLifecycleState::ParentKillPending { from } => {
                    require_recovery_action_complete(reconcile_valid_record(
                        record,
                        RecoveryAction::KillParent {
                            candidate: candidate_index,
                        },
                        ContainmentDeadline::control_action(),
                    )?)?;
                    require_recovery_action_complete(reconcile_valid_record(
                        record,
                        RecoveryAction::ProveParentEmpty {
                            candidate: candidate_index,
                        },
                        ContainmentDeadline::control_action(),
                    )?)?;
                    record.cgroups[index].lifecycle = CgroupLifecycleState::ParentEmpty { from };
                    persist_restart_record(record_path, record)?;
                }
                CgroupLifecycleState::ParentEmpty { from } => {
                    record.cgroups[index].lifecycle =
                        CgroupLifecycleState::PayloadRemovePending { from };
                    persist_restart_record(record_path, record)?;
                }
                CgroupLifecycleState::PayloadRemovePending { from } => {
                    require_recovery_action_complete(reconcile_valid_record(
                        record,
                        RecoveryAction::RemovePayload {
                            candidate: candidate_index,
                        },
                        ContainmentDeadline::control_action(),
                    )?)?;
                    record.cgroups[index].payload = None;
                    record.cgroups[index].lifecycle = CgroupLifecycleState::PayloadRemoved { from };
                    persist_restart_record(record_path, record)?;
                }
                CgroupLifecycleState::PayloadRemoved { from } => {
                    record.cgroups[index].lifecycle =
                        CgroupLifecycleState::ControlRemovePending { from };
                    persist_restart_record(record_path, record)?;
                }
                CgroupLifecycleState::ControlRemovePending { from } => {
                    require_recovery_action_complete(reconcile_valid_record(
                        record,
                        RecoveryAction::RemoveControl {
                            candidate: candidate_index,
                        },
                        ContainmentDeadline::control_action(),
                    )?)?;
                    record.cgroups[index].control = None;
                    record.cgroups[index].lifecycle = CgroupLifecycleState::ControlRemoved { from };
                    persist_restart_record(record_path, record)?;
                }
                CgroupLifecycleState::ControlRemoved { from } => {
                    record.cgroups[index].lifecycle =
                        CgroupLifecycleState::ParentRemovePending { from };
                    persist_restart_record(record_path, record)?;
                }
                CgroupLifecycleState::ParentRemovePending { .. } => {
                    require_recovery_action_complete(reconcile_valid_record(
                        record,
                        RecoveryAction::RemoveParent {
                            candidate: candidate_index,
                        },
                        ContainmentDeadline::control_action(),
                    )?)?;
                    record.cgroups[index].parent = None;
                    record.cgroups[index].lifecycle = CgroupLifecycleState::Removed;
                    persist_restart_record(record_path, record)?;
                }
                CgroupLifecycleState::Removed => break,
                CgroupLifecycleState::RollbackRequired => {
                    return Err(ContainmentErrorCode::EvidenceUnavailable);
                }
            }
        }
    }

    match record.tournament_cgroup.lifecycle {
        TournamentCgroupLifecycleState::Created => {
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::RemovePending;
            persist_restart_record(record_path, record)?;
        }
        TournamentCgroupLifecycleState::RemovePending => {}
        TournamentCgroupLifecycleState::Removed => return Ok(()),
        _ => return Err(ContainmentErrorCode::EvidenceUnavailable),
    }
    if reopen_recovery_domain(record)?.is_some() {
        require_recovery_action_complete(reconcile_valid_record(
            record,
            RecoveryAction::ProveTournamentEmpty,
            ContainmentDeadline::control_action(),
        )?)?;
    }
    require_recovery_action_complete(reconcile_valid_record(
        record,
        RecoveryAction::RemoveTournamentDomain,
        ContainmentDeadline::control_action(),
    )?)?;
    record.tournament_cgroup.domain = None;
    record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Removed;
    persist_restart_record(record_path, record)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn require_recovery_action_complete(evidence: RecoveryEvidence) -> ContainmentResult<()> {
    match evidence {
        RecoveryEvidence::CandidateActionComplete { .. }
        | RecoveryEvidence::TournamentActionComplete { .. } => Ok(()),
        _ => Err(ContainmentErrorCode::EvidenceUnavailable),
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_real_component_driver(endpoint_only: bool) -> ContainmentResult<()> {
    let fixture = std::env::var_os("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::Unsupported)?;
    if endpoint_only {
        run_real_execution_case(
            &fixture.join("endpoint"),
            &cgroup_root,
            vec![OsString::from("/usr/bin/true")],
            None,
            RealExecutionExpectation::ProbeOnly,
        )?;
        return Ok(());
    }
    run_real_topology_attack_matrix(&cgroup_root)?;
    run_real_peer_attack_matrix(&fixture.join("p"), &cgroup_root)?;
    let elapsed = run_real_execution_case(
        &fixture.join("e"),
        &cgroup_root,
        vec![OsString::from("/workspace/run.sh")],
        Some([
            b"#!/bin/sh\n/usr/bin/sleep 0.02\n",
            b"#!/bin/sh\n/usr/bin/sleep 0.18\n",
        ]),
        RealExecutionExpectation::Complete { output_bytes: 0 },
    )?;
    if elapsed[0].elapsed_ns >= elapsed[1].elapsed_ns
        || crate::speculation::score_candidates([
            elapsed[0].candidate_result(0),
            elapsed[1].candidate_result(1),
        ]) != crate::speculation::ScoreDecision::Selected(0)
    {
        return Err(ContainmentErrorCode::TerminalBoundaryFailure);
    }
    run_real_execution_case(
        &fixture.join("x"),
        &cgroup_root,
        vec![
            OsString::from("/usr/bin/head"),
            OsString::from("-c"),
            OsString::from(crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES.to_string()),
            OsString::from("/dev/zero"),
        ],
        None,
        RealExecutionExpectation::Complete {
            output_bytes: crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES,
        },
    )?;
    run_real_execution_case(
        &fixture.join("o"),
        &cgroup_root,
        vec![
            OsString::from("/usr/bin/head"),
            OsString::from("-c"),
            OsString::from((crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES + 1).to_string()),
            OsString::from("/dev/zero"),
        ],
        None,
        RealExecutionExpectation::Overflow,
    )?;
    run_real_execution_case(
        &fixture.join("i"),
        &cgroup_root,
        vec![OsString::from("/usr/bin/cat"), OsString::from("/dev/zero")],
        None,
        RealExecutionExpectation::Overflow,
    )?;
    let fork_storm = b"#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 64 ]; do /usr/bin/sleep 30 & i=$((i + 1)); done\nexit 0\n";
    run_real_execution_case(
        &fixture.join("f"),
        &cgroup_root,
        vec![OsString::from("/workspace/run.sh")],
        Some([fork_storm, fork_storm]),
        RealExecutionExpectation::Complete { output_bytes: 0 },
    )?;
    let orphan_and_setsid = b"#!/bin/sh\n( ( /usr/bin/sleep 30 & exit 0 ) & exit 0 ) &\n/usr/bin/setsid /usr/bin/sleep 30 >/dev/null 2>&1 &\nexit 0\n";
    run_real_execution_case(
        &fixture.join("d"),
        &cgroup_root,
        vec![OsString::from("/workspace/run.sh")],
        Some([orphan_and_setsid, orphan_and_setsid]),
        RealExecutionExpectation::DescendantsRemain,
    )?;
    let migration = b"#!/bin/sh\nif /usr/bin/grep -q ' - cgroup2 ' /proc/self/mountinfo; then exit 91; fi\n{ printf '0\\n' > /proc/1/root/sys/fs/cgroup/cgroup.procs; } 2>/dev/null && exit 92\nexit 0\n";
    run_real_execution_case(
        &fixture.join("m"),
        &cgroup_root,
        vec![OsString::from("/workspace/run.sh")],
        Some([migration, migration]),
        RealExecutionExpectation::Complete { output_bytes: 0 },
    )?;
    // The topology action above proves the fixed production pids.max=256.
    // This debug-only execution case lowers its already-verified leaf to 32
    // so hosted runners can prove kernel rejection inside one action bound.
    let pids_exhaustion = b"#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 36 ]; do\n    /usr/bin/sleep 30 2>/dev/null &\n    i=$((i + 1))\ndone 2>/dev/null\nexit 0\n";
    run_real_execution_case(
        &fixture.join("q"),
        &cgroup_root,
        vec![OsString::from("/workspace/run.sh")],
        Some([pids_exhaustion, pids_exhaustion]),
        RealExecutionExpectation::PidsExhausted,
    )?;
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_real_topology_attack_matrix(cgroup_root: &Path) -> ContainmentResult<()> {
    let root = open_existing_delegated_cgroup_root(cgroup_root).map_err(map_evidence)?;
    let root = RetainedCgroupNode {
        file: root.try_clone_retained_fd().map_err(map_evidence)?,
        identity: root.identity(),
        membership: membership_for_cgroup_path(cgroup_root)?,
    };
    prove_domain_task_free(&root)?;
    enable_pids(&root)?;

    let prefix = format!("lterm-g003-attack-{}", Uuid::new_v4());
    let missing_parent_name = cgroup_name(&format!("{prefix}-missing-parent"))?;
    let missing_parent = create_cgroup_child(
        &root,
        &missing_parent_name,
        join_membership(&root.membership, missing_parent_name.to_bytes())?,
    )?;
    let missing_leaf_name = cgroup_name("missing-controller")?;
    let missing_leaf = create_cgroup_child(
        &missing_parent,
        &missing_leaf_name,
        join_membership(&missing_parent.membership, missing_leaf_name.to_bytes())?,
    )?;
    if enable_pids(&missing_leaf).is_ok() {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    remove_cgroup_child(
        &missing_parent,
        &missing_leaf_name,
        missing_leaf.identity,
        "payload",
    )?;
    remove_cgroup_child(
        &root,
        &missing_parent_name,
        missing_parent.identity,
        "parent",
    )?;

    let flat_name = cgroup_name(&format!("{prefix}-flat"))?;
    let flat = create_cgroup_child(
        &root,
        &flat_name,
        join_membership(&root.membership, flat_name.to_bytes())?,
    )?;
    let mut flat_task = std::process::Command::new("/usr/bin/sleep")
        .arg("30")
        .spawn()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    write_leaf(
        &flat,
        c"cgroup.procs",
        format!("{}\n", flat_task.id()).as_bytes(),
    )?;
    if prove_domain_task_free(&flat).is_ok() {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    write_leaf(&flat, c"cgroup.kill", b"1\n")?;
    wait_populated_zero(&flat, ContainmentDeadline::control_action())?;
    flat_task
        .wait()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    remove_cgroup_child(&root, &flat_name, flat.identity, "parent")?;

    let threaded_name = cgroup_name(&format!("{prefix}-threaded"))?;
    let threaded = create_cgroup_child(
        &root,
        &threaded_name,
        join_membership(&root.membership, threaded_name.to_bytes())?,
    )?;
    match write_leaf(&threaded, c"cgroup.type", b"threaded\n") {
        Ok(()) if prove_domain_task_free(&threaded).is_ok() => {
            return Err(ContainmentErrorCode::TopologyFailure);
        }
        Ok(()) | Err(ContainmentErrorCode::TopologyFailure) => {}
        Err(error) => return Err(error),
    }
    remove_cgroup_child(&root, &threaded_name, threaded.identity, "parent")?;

    let replaced_name = cgroup_name(&format!("{prefix}-replaced"))?;
    let replaced_membership = join_membership(&root.membership, replaced_name.to_bytes())?;
    let replaced = create_cgroup_child(&root, &replaced_name, replaced_membership.clone())?;
    let stale_identity = replaced.identity;
    remove_cgroup_child(&root, &replaced_name, stale_identity, "parent")?;
    let replacement = create_cgroup_child(&root, &replaced_name, replaced_membership.clone())?;
    if !matches!(
        reopen_cgroup_child(
            &root,
            &replaced_name,
            replaced_membership,
            Some(stale_identity),
        ),
        Err(ContainmentErrorCode::InvalidIdentity)
    ) {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    remove_cgroup_child(&root, &replaced_name, replacement.identity, "parent")
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_real_peer_attack_matrix(fixture: &Path, cgroup_root: &Path) -> ContainmentResult<()> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(fixture)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let source = fixture.join("source");
    let candidates = [fixture.join("candidate-0"), fixture.join("candidate-1")];
    let ledger = fixture.join("ledger");
    let control = fixture.join("control");
    for path in [&source, &candidates[0], &candidates[1], &ledger, &control] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    let context = validate_prepare(
        PrepareInputs {
            tournament_uuid: Uuid::new_v4(),
            generation: 1,
            source,
            candidates,
            ledger_root: ledger,
            cgroup_root: cgroup_root.to_path_buf(),
            control_root: control,
            argv: vec![OsString::from("/usr/bin/true")],
        },
        ContainmentDeadline::control_action(),
    )?;
    let mut topology = begin_topology(&context)?;
    create_topology(&mut topology, TopologyAction::CreateTournamentDomain)?;
    for candidate in 0_u8..2 {
        create_topology(
            &mut topology,
            TopologyAction::CreateCandidateParent { candidate },
        )?;
        create_topology(
            &mut topology,
            TopologyAction::CreateControlLeaf { candidate },
        )?;
    }
    let identity = context.candidate_identity(0)?;
    let expected_membership = managed_membership(topology.candidate(0)?.control()?, identity)?;
    let self_exe =
        configured_current_executable_path().ok_or(ContainmentErrorCode::EvidenceUnavailable)?;

    let same_uid_control = prepare_runner_control(&context, 0)?;
    let mut sleep = launch_peer_test_process(
        topology.candidate(1)?.control()?,
        context.candidate_identity(1)?,
        PathBuf::from("/usr/bin/sleep"),
        vec![OsString::from("0.2")],
        Vec::new(),
    )?;
    let same_uid_connector = connect_seqpacket_test(&same_uid_control.socket_path)?;
    let before = test_open_fd_count()?;
    if !matches!(
        accept_authenticated_peer(
            same_uid_control
                .listener
                .as_ref()
                .ok_or(ContainmentErrorCode::PeerRejected)?,
            &sleep.controller,
            &expected_membership,
            identity,
            ContainmentDeadline::control_action(),
        ),
        Err(ContainmentErrorCode::PeerRejected)
    ) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    if test_open_fd_count()? != before {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    drop(same_uid_connector);
    sleep
        .waiter
        .wait_until(ContainmentDeadline::control_action().instant())
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    drop(same_uid_control);

    let sibling_control = prepare_runner_control(&context, 0)?;
    let mut sibling = launch_peer_test_process(
        topology.candidate(1)?.control()?,
        context.candidate_identity(1)?,
        self_exe.clone(),
        vec![OsString::from(
            "--internal-speculation-peer-connect-test-v1",
        )],
        vec![
            (
                OsString::from("LTERM_INTERNAL_TEST_MODE"),
                OsString::from("1"),
            ),
            (
                OsString::from("LTERM_INTERNAL_SPECULATION_PEER_SOCKET"),
                sibling_control.socket_path.as_os_str().to_owned(),
            ),
        ],
    )?;
    let before = test_open_fd_count()?;
    if !matches!(
        accept_authenticated_peer(
            sibling_control
                .listener
                .as_ref()
                .ok_or(ContainmentErrorCode::PeerRejected)?,
            &sibling.controller,
            &expected_membership,
            identity,
            ContainmentDeadline::control_action(),
        ),
        Err(ContainmentErrorCode::PeerRejected)
    ) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    if test_open_fd_count()? != before {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    drop(sibling_control);
    sibling
        .waiter
        .wait_until(ContainmentDeadline::control_action().instant())
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;

    let wrong_namespace_control = prepare_runner_control(&context, 0)?;
    let mut wrong_namespace = launch_peer_test_process(
        topology.candidate(0)?.control()?,
        identity,
        PathBuf::from("/usr/bin/dash"),
        vec![
            OsString::from("-c"),
            OsString::from("\"$0\" --internal-speculation-peer-connect-test-v1 & wait"),
            self_exe.clone().into_os_string(),
        ],
        vec![
            (
                OsString::from("LTERM_INTERNAL_TEST_MODE"),
                OsString::from("1"),
            ),
            (
                OsString::from("LTERM_INTERNAL_SPECULATION_PEER_SOCKET"),
                wrong_namespace_control.socket_path.as_os_str().to_owned(),
            ),
        ],
    )?;
    let before = test_open_fd_count()?;
    if !matches!(
        accept_authenticated_peer(
            wrong_namespace_control
                .listener
                .as_ref()
                .ok_or(ContainmentErrorCode::PeerRejected)?,
            &wrong_namespace.controller,
            &expected_membership,
            identity,
            ContainmentDeadline::control_action(),
        ),
        Err(ContainmentErrorCode::PeerRejected)
    ) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    if test_open_fd_count()? != before {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    drop(wrong_namespace_control);
    wrong_namespace
        .waiter
        .wait_until(ContainmentDeadline::control_action().instant())
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;

    let mut stale = launch_peer_test_process(
        topology.candidate(0)?.control()?,
        identity,
        PathBuf::from("/usr/bin/true"),
        Vec::new(),
        Vec::new(),
    )?;
    let stale_controller = stale.controller.clone();
    stale
        .waiter
        .wait_until(ContainmentDeadline::control_action().instant())
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let stale_control = prepare_runner_control(&context, 0)?;
    let mut replacement = launch_peer_test_process(
        topology.candidate(0)?.control()?,
        identity,
        self_exe.clone(),
        vec![OsString::from(
            "--internal-speculation-peer-connect-test-v1",
        )],
        vec![
            (
                OsString::from("LTERM_INTERNAL_TEST_MODE"),
                OsString::from("1"),
            ),
            (
                OsString::from("LTERM_INTERNAL_SPECULATION_PEER_SOCKET"),
                stale_control.socket_path.as_os_str().to_owned(),
            ),
        ],
    )?;
    let before = test_open_fd_count()?;
    if !matches!(
        accept_authenticated_peer(
            stale_control
                .listener
                .as_ref()
                .ok_or(ContainmentErrorCode::PeerRejected)?,
            &stale_controller,
            &expected_membership,
            identity,
            ContainmentDeadline::control_action(),
        ),
        Err(ContainmentErrorCode::PeerRejected)
    ) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    if test_open_fd_count()? != before {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    drop(stale_control);
    replacement
        .waiter
        .wait_until(ContainmentDeadline::control_action().instant())
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;

    let wrong_generation_control = prepare_runner_control(&context, 0)?;
    let mut wrong_generation = launch_peer_test_process(
        topology.candidate(0)?.control()?,
        identity,
        self_exe,
        vec![OsString::from(
            "--internal-speculation-peer-connect-test-v1",
        )],
        vec![
            (
                OsString::from("LTERM_INTERNAL_TEST_MODE"),
                OsString::from("1"),
            ),
            (
                OsString::from("LTERM_INTERNAL_SPECULATION_PEER_SOCKET"),
                wrong_generation_control.socket_path.as_os_str().to_owned(),
            ),
        ],
    )?;
    let wrong_identity = RunnerIdentity {
        generation: identity.generation + 1,
        ..identity
    };
    let before = test_open_fd_count()?;
    if !matches!(
        accept_authenticated_peer(
            wrong_generation_control
                .listener
                .as_ref()
                .ok_or(ContainmentErrorCode::PeerRejected)?,
            &wrong_generation.controller,
            &expected_membership,
            wrong_identity,
            ContainmentDeadline::control_action(),
        ),
        Err(ContainmentErrorCode::PeerRejected)
    ) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    if test_open_fd_count()? != before {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    wrong_generation
        .controller
        .terminate()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    wrong_generation
        .waiter
        .wait_until(ContainmentDeadline::control_action().instant())
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    drop(wrong_generation_control);

    for candidate in 0_u8..2 {
        let candidate_topology = topology.candidate(candidate)?;
        let control_node = candidate_topology.control()?;
        wait_populated_zero(control_node, ContainmentDeadline::control_action())?;
        let parent = candidate_topology
            .parent
            .as_ref()
            .ok_or(ContainmentErrorCode::TopologyFailure)?;
        remove_cgroup_child(parent, c"control", control_node.identity, "control")?;
    }
    for candidate in 0_u8..2 {
        let candidate_topology = topology.candidate(candidate)?;
        let parent = candidate_topology
            .parent
            .as_ref()
            .ok_or(ContainmentErrorCode::TopologyFailure)?;
        let domain = topology
            .domain
            .as_ref()
            .ok_or(ContainmentErrorCode::TopologyFailure)?;
        let name = cgroup_name(&format!("candidate-{candidate}"))?;
        remove_cgroup_child(domain, &name, parent.identity, "parent")?;
    }
    let domain = topology
        .domain
        .as_ref()
        .ok_or(ContainmentErrorCode::TopologyFailure)?;
    wait_populated_zero(domain, ContainmentDeadline::control_action())?;
    let name = cgroup_name(&format!("lterm-g003-{}", context.identity.tournament_uuid))?;
    remove_cgroup_child(&topology.root, &name, domain.identity, "tournament")
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn test_open_fd_count() -> ContainmentResult<usize> {
    std::fs::read_dir("/proc/self/fd")
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)
        .map(|entries| entries.count())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn launch_peer_test_process(
    control: &RetainedCgroupNode,
    identity: RunnerIdentity,
    executable: PathBuf,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
) -> ContainmentResult<crate::launch_registry::ManagedLaunch> {
    let membership = managed_membership(control, identity)?;
    let placement = ControlCgroupPlacement::new(
        open_cgroup_procs(control)?,
        clone_file(&control.file)?,
        membership,
    )
    .map_err(|_| ContainmentErrorCode::PlacementUnproven)?;
    launch_managed_process(ManagedLaunchRequest {
        owner: None,
        reservation: None,
        lifetime_guard: None,
        executable_policy: ManagedExecutablePolicy::Legacy,
        placement: ManagedPlacement::CgroupV2(placement),
        auxiliary: ManagedAuxiliary::None,
        executable,
        arguments,
        current_dir: None,
        environment,
        stdio: ManagedStdioPolicy::Inherit,
    })
    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
#[derive(Clone, Copy)]
enum RealExecutionExpectation {
    ProbeOnly,
    Complete { output_bytes: u64 },
    DescendantsRemain,
    PidsExhausted,
    Overflow,
}

#[cfg(all(debug_assertions, target_os = "linux"))]
#[derive(Clone, Copy)]
struct RealExecutionEvidence {
    elapsed_ns: u64,
    output_bytes: u64,
    exited_zero: bool,
    overflowed: bool,
}

#[cfg(all(debug_assertions, target_os = "linux"))]
struct RealTopologyCleanup(Option<TournamentTopology>);

#[cfg(all(debug_assertions, target_os = "linux"))]
impl RealTopologyCleanup {
    fn new(topology: TournamentTopology) -> Self {
        Self(Some(topology))
    }

    fn disarm(&mut self) {
        self.0.take();
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
impl std::ops::Deref for RealTopologyCleanup {
    type Target = TournamentTopology;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("real topology guard is armed")
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
impl std::ops::DerefMut for RealTopologyCleanup {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("real topology guard is armed")
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
impl Drop for RealTopologyCleanup {
    fn drop(&mut self) {
        if let Some(mut topology) = self.0.take() {
            let _ = cleanup_real_topology(&mut topology);
        }
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
impl RealExecutionEvidence {
    fn candidate_result(self, input_index: u8) -> crate::speculation::CandidateResult {
        crate::speculation::CandidateResult {
            input_index,
            exit_success: Some(self.exited_zero),
            elapsed_ns: Some(self.elapsed_ns),
            output_bytes: Some(self.output_bytes),
            quiescent: true,
            output_overflowed: self.overflowed,
        }
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_real_execution_case(
    fixture: &Path,
    cgroup_root: &Path,
    argv: Vec<OsString>,
    scripts: Option<[&[u8]; 2]>,
    expectation: RealExecutionExpectation,
) -> ContainmentResult<[RealExecutionEvidence; 2]> {
    let has_scripts = scripts.is_some();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(fixture)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let source = fixture.join("source");
    let candidates = [fixture.join("candidate-0"), fixture.join("candidate-1")];
    let ledger = fixture.join("ledger");
    let control = fixture.join("control");
    for path in [&source, &candidates[0], &candidates[1]] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    if let Some(scripts) = scripts {
        for (candidate, script) in candidates.iter().zip(scripts) {
            let path = candidate.join("run.sh");
            std::fs::write(&path, script).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o500))
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        }
    }
    for path in [&ledger, &control] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    let context = validate_prepare(
        PrepareInputs {
            tournament_uuid: Uuid::new_v4(),
            generation: 1,
            source: source.clone(),
            candidates: candidates.clone(),
            ledger_root: ledger,
            cgroup_root: cgroup_root.to_path_buf(),
            control_root: control,
            argv,
        },
        ContainmentDeadline::control_action(),
    )?;
    let mut tournament = RealTopologyCleanup::new(begin_topology(&context)?);
    create_topology(&mut tournament, TopologyAction::CreateTournamentDomain)?;
    for candidate in 0_u8..2 {
        create_topology(
            &mut tournament,
            TopologyAction::CreateCandidateParent { candidate },
        )?;
        create_topology(
            &mut tournament,
            TopologyAction::CreateControlLeaf { candidate },
        )?;
        create_topology(
            &mut tournament,
            TopologyAction::CreatePayloadLeaf { candidate },
        )?;
        create_topology(
            &mut tournament,
            TopologyAction::ConfigurePayloadLimit { candidate },
        )?;
    }
    if matches!(expectation, RealExecutionExpectation::PidsExhausted) {
        for candidate in 0_u8..2 {
            let payload = tournament.candidate(candidate)?.payload()?;
            if read_leaf(payload, c"pids.max", 64)? != b"256\n" {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            write_leaf(payload, c"pids.max", b"32\n")?;
            if read_leaf(payload, c"pids.max", 64)? != b"32\n" {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
        }
    }
    let first = tournament.candidate(0)?;
    if first.control()?.identity == first.payload()?.identity {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    let wrong_placement = open_cgroup_procs(first.control()?)?;
    if placement_descriptor_evidence(
        &wrong_placement,
        first.payload()?,
        context.candidate_identity(0)?,
    )
    .is_ok()
    {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    for candidate in 0_u8..2 {
        let candidate_topology = tournament.candidate(candidate)?;
        let probe = match run_fixed_probe(
            &context,
            candidate,
            candidate_topology,
            ContainmentDeadline::control_action(),
        ) {
            Ok(probe) => probe,
            Err(error) => {
                cleanup_real_topology(&mut tournament)?;
                return Err(error);
            }
        };
        if !probe.exited_zero || !probe.parent_populated_zero || probe.output_bytes != 0 {
            return Err(ContainmentErrorCode::TerminalBoundaryFailure);
        }
    }
    if matches!(expectation, RealExecutionExpectation::ProbeOnly) {
        cleanup_real_topology(&mut tournament)?;
        tournament.disarm();
        return Ok([RealExecutionEvidence {
            elapsed_ns: 0,
            output_bytes: 0,
            exited_zero: true,
            overflowed: false,
        }; 2]);
    }
    let containments = [
        launch_runner(
            &context,
            &tournament,
            0,
            ContainmentDeadline::control_action(),
        )?,
        launch_runner(
            &context,
            &tournament,
            1,
            ContainmentDeadline::control_action(),
        )?,
    ];
    let observations = containments
        .each_ref()
        .map(CandidateContainment::observation);
    for (candidate, containment) in containments.iter().enumerate() {
        let observation = containment.observation();
        let before_second_connect = test_open_fd_count()?;
        let second_connect =
            connect_seqpacket_test(&containment.control._lifetime.private_control().socket_path);
        let second_connect_accepted = second_connect.is_ok();
        drop(second_connect);
        let after_second_connect = test_open_fd_count()?;
        if usize::from(observation.identity.candidate_index) != candidate
            || usize::from(observation.managed_owner.candidate_index) != candidate
            || observation.managed_owner.role != ManagedOwnerRoleEvidence::Runner
            || containment
                .control
                ._lifetime
                .private_control()
                .socket_path
                .exists()
            || second_connect_accepted
            || after_second_connect != before_second_connect
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
    }
    if observations[0].managed_owner.slot == observations[1].managed_owner.slot {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let [left, right] = containments;
    let [left, right] = [left.split(), right.split()];
    let mut controls = [left.control, right.control];
    let mut observers = [left.observer, right.observer];
    for candidate in 0_u8..2 {
        let index = usize::from(candidate);
        transfer_payload_fd(
            &mut controls[index],
            tournament.candidate(candidate)?,
            ContainmentDeadline::control_action(),
        )?;
        receive_payload_fd_ack(&mut observers[index], ContainmentDeadline::control_action())?;
    }
    let sends = [
        send_go(&mut controls[0], ContainmentDeadline::control_action())?,
        send_go(&mut controls[1], ContainmentDeadline::control_action())?,
    ];
    if sends[0]
        .sent_monotonic_ns
        .abs_diff(sends[1].sent_monotonic_ns)
        > MAX_GO_RECEIPT_SKEW_NS
    {
        return Err(ContainmentErrorCode::Timeout);
    }
    let receipts = [
        receive_go_receipt(&mut observers[0], ContainmentDeadline::control_action())?,
        receive_go_receipt(&mut observers[1], ContainmentDeadline::control_action())?,
    ];
    go_receipt_skew_ns(receipts)?;
    for candidate in 0_u8..2 {
        let index = usize::from(candidate);
        let placement = receive_payload_placed(
            &mut observers[index],
            tournament.candidate(candidate)?,
            ContainmentDeadline::control_action(),
        )?;
        send_payload_release(
            &mut controls[index],
            placement,
            ContainmentDeadline::control_action(),
        )?;
    }
    let mut evidence = [RealExecutionEvidence {
        elapsed_ns: 0,
        output_bytes: 0,
        exited_zero: false,
        overflowed: false,
    }; 2];
    match expectation {
        RealExecutionExpectation::ProbeOnly => unreachable!("probe-only returned before launch"),
        RealExecutionExpectation::Complete { .. }
        | RealExecutionExpectation::DescendantsRemain
        | RealExecutionExpectation::PidsExhausted => {
            for candidate in 0_u8..2 {
                let index = usize::from(candidate);
                let event = receive_execution_event(
                    &mut observers[index],
                    ContainmentDeadline::control_action(),
                )?;
                let ContainmentEvent::LeaderExited {
                    category,
                    elapsed_ns,
                    ..
                } = event
                else {
                    return Err(ContainmentErrorCode::OutputLimit);
                };
                evidence[index].elapsed_ns = elapsed_ns;
                evidence[index].exited_zero = category == RunnerExitCategory::ExitedZero;
                match expectation {
                    RealExecutionExpectation::ProbeOnly => {
                        unreachable!("probe-only returned before execution")
                    }
                    RealExecutionExpectation::DescendantsRemain => {
                        prove_descendants_remain(tournament.candidate(candidate)?.payload()?)?
                    }
                    RealExecutionExpectation::PidsExhausted => {
                        prove_pids_limit_hit(tournament.candidate(candidate)?.payload()?)?
                    }
                    RealExecutionExpectation::Complete { .. }
                    | RealExecutionExpectation::Overflow => {}
                }
                perform_candidate_cleanup_action(
                    &mut tournament,
                    candidate,
                    CandidateCleanupAction::KillPayload,
                    ContainmentDeadline::control_action(),
                )?;
            }
        }
        RealExecutionExpectation::Overflow => {
            for candidate in 0_u8..2 {
                let index = usize::from(candidate);
                let event = receive_execution_event(
                    &mut observers[index],
                    ContainmentDeadline::control_action(),
                )?;
                if event
                    != (ContainmentEvent::OutputLimitExceeded {
                        candidate,
                        bytes: crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES + 1,
                    })
                {
                    return Err(ContainmentErrorCode::OutputLimit);
                }
                acknowledge_output_cleanup_claimed(
                    &mut controls[index],
                    ContainmentDeadline::control_action(),
                )?;
                perform_candidate_cleanup_action(
                    &mut tournament,
                    candidate,
                    CandidateCleanupAction::KillPayload,
                    ContainmentDeadline::control_action(),
                )?;
                evidence[index].overflowed = true;
            }
        }
    }
    for candidate in 0_u8..2 {
        perform_candidate_cleanup_action(
            &mut tournament,
            candidate,
            CandidateCleanupAction::ProvePayloadEmpty,
            ContainmentDeadline::control_action(),
        )?;
    }
    if matches!(expectation, RealExecutionExpectation::Overflow) {
        for candidate in 0_u8..2 {
            let index = usize::from(candidate);
            let event = receive_execution_event(
                &mut observers[index],
                ContainmentDeadline::control_action(),
            )?;
            let ContainmentEvent::LeaderExited {
                category,
                elapsed_ns,
                ..
            } = event
            else {
                return Err(ContainmentErrorCode::OutputLimit);
            };
            if category != RunnerExitCategory::OutputLimitExceeded {
                return Err(ContainmentErrorCode::OutputLimit);
            }
            evidence[index].elapsed_ns = elapsed_ns;
        }
    }
    for candidate in 0_u8..2 {
        let index = usize::from(candidate);
        let drained =
            receive_output_drained(&mut observers[index], ContainmentDeadline::control_action())?;
        let ContainmentEvent::OutputDrained { bytes, .. } = drained else {
            return Err(ContainmentErrorCode::TerminalBoundaryFailure);
        };
        match expectation {
            RealExecutionExpectation::ProbeOnly => unreachable!("probe-only returned before drain"),
            RealExecutionExpectation::Complete { output_bytes } if bytes == output_bytes => {}
            RealExecutionExpectation::DescendantsRemain
            | RealExecutionExpectation::PidsExhausted
                if bytes == 0 => {}
            RealExecutionExpectation::Overflow
                if bytes > crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES => {}
            _ => return Err(ContainmentErrorCode::OutputLimit),
        }
        evidence[index].output_bytes = bytes;
        send_select_or_abort(
            &mut controls[index],
            DecisionKind::Abort,
            ContainmentDeadline::control_action(),
        )?;
        receive_decision_ack(
            &mut observers[index],
            DecisionKind::Abort,
            ContainmentDeadline::control_action(),
        )?;
    }
    for (control, mut observer) in controls.into_iter().zip(observers) {
        observe_sync_eof(&mut observer, ContainmentDeadline::control_action())?;
        observe_managed_reaped(&mut observer, ContainmentDeadline::control_action())?;
        finish_containment(control, observer)?;
    }
    cleanup_real_topology(&mut tournament)?;
    tournament.disarm();
    if std::fs::read_dir(&source)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        .next()
        .is_some()
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    for candidate in candidates {
        let names = std::fs::read_dir(candidate)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            .map(|entry| {
                entry
                    .map(|entry| entry.file_name())
                    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)
            })
            .collect::<ContainmentResult<Vec<_>>>()?;
        if (!has_scripts && !names.is_empty())
            || (has_scripts && (names.len() != 1 || names[0] != std::ffi::OsStr::new("run.sh")))
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
    }
    Ok(evidence)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn prove_descendants_remain(payload: &RetainedCgroupNode) -> ContainmentResult<()> {
    let procs = read_leaf(payload, c"cgroup.procs", 4096)?;
    if procs
        .split(|byte| *byte == b'\n')
        .any(|pid| !pid.is_empty())
    {
        Ok(())
    } else {
        Err(ContainmentErrorCode::TerminalBoundaryFailure)
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn prove_pids_limit_hit(payload: &RetainedCgroupNode) -> ContainmentResult<()> {
    let events = read_leaf(payload, c"pids.events", 4096)?;
    let events =
        std::str::from_utf8(&events).map_err(|_| ContainmentErrorCode::TerminalBoundaryFailure)?;
    if events.lines().any(|line| {
        line.strip_prefix("max ")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|value| value > 0)
    }) {
        Ok(())
    } else {
        Err(ContainmentErrorCode::TerminalBoundaryFailure)
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn cleanup_real_topology(tournament: &mut TournamentTopology) -> ContainmentResult<()> {
    let mut first_error = None;
    for candidate in 0_u8..2 {
        for action in [
            CandidateCleanupAction::KillParent,
            CandidateCleanupAction::ProveParentEmpty,
            CandidateCleanupAction::RemovePayload,
            CandidateCleanupAction::RemoveControl,
            CandidateCleanupAction::RemoveParent,
        ] {
            if let Err(error) = perform_candidate_cleanup_action(
                tournament,
                candidate,
                action,
                ContainmentDeadline::control_action(),
            ) {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Err(error) = perform_tournament_cleanup_action(
        tournament,
        TournamentCleanupAction::ProveEmpty,
        ContainmentDeadline::control_action(),
    ) {
        first_error.get_or_insert(error);
    }
    if let Err(error) = perform_tournament_cleanup_action(
        tournament,
        TournamentCleanupAction::RemoveDomain,
        ContainmentDeadline::control_action(),
    ) {
        first_error.get_or_insert(error);
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_os = "linux", debug_assertions))]
    struct ScopedSpeculationTestConfig(Option<SpeculationTestConfig>);

    #[cfg(all(target_os = "linux", debug_assertions))]
    impl ScopedSpeculationTestConfig {
        fn new() -> Self {
            Self(SPECULATION_LOCAL_TEST_CONFIG.with(|state| state.replace(None)))
        }

        fn hostile_swap(
            &self,
            failpoint: &str,
            path: &std::path::Path,
            backup: &std::path::Path,
            kind: &str,
        ) {
            SPECULATION_LOCAL_TEST_CONFIG.with(|state| {
                state.replace(Some(SpeculationTestConfig {
                    failpoint: Some(OsString::from(failpoint)),
                    action: Some(OsString::from("hostile-swap")),
                    swap_path: Some(path.to_owned()),
                    swap_backup: Some(backup.to_owned()),
                    swap_kind: Some(kind.to_owned()),
                    ..SpeculationTestConfig::default()
                }));
            });
        }
    }

    #[cfg(all(target_os = "linux", debug_assertions))]
    impl Drop for ScopedSpeculationTestConfig {
        fn drop(&mut self) {
            SPECULATION_LOCAL_TEST_CONFIG.with(|state| {
                state.replace(self.0.take());
            });
        }
    }

    #[cfg(target_os = "linux")]
    fn private_recovery_fixture(
        owner: &ManagedOwnerTag,
        key: ManagedKey,
    ) -> (
        tempfile::TempDir,
        ValidatedDirectory,
        PathBuf,
        PrivateRunnerOwnership,
        PrivateArtifactIdentity,
        File,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let control_path = temp.path().join("control");
        let control_root =
            crate::speculation_fs::open_or_create_private_dir(&control_path).unwrap();
        let leaf = format!(
            "lterm-g003-{}-candidate-{}",
            owner.tournament_uuid, owner.candidate_index
        );
        let path = control_path.join(&leaf);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = open_existing_private_dir(&path).unwrap();
        let runner_path = path.join("lterm");
        std::fs::write(&runner_path, b"runner").unwrap();
        std::fs::set_permissions(&runner_path, std::fs::Permissions::from_mode(0o500)).unwrap();
        let runner = std::fs::File::open(&runner_path).unwrap();
        let listener = create_seqpacket_listener(&path.join("control.sock")).unwrap();
        let socket_metadata = std::fs::symlink_metadata(path.join("control.sock")).unwrap();
        let runner_identity = artifact_identity(&runner.metadata().unwrap());
        let socket_identity = artifact_identity(&socket_metadata);
        let binding = ManagedArtifactBinding::test_value(
            Uuid::new_v4(),
            managed_directory_identity(control_root.identity()),
            &leaf,
            Some(managed_directory_identity(directory.identity())),
        )
        .test_with_files(
            managed_artifact_identity(runner_identity),
            managed_artifact_identity(socket_identity),
        );
        let mut record = PrivateRunnerOwnership {
            schema_version: 1,
            owner: owner.clone(),
            slot: key.slot(),
            generation: key.generation(),
            binding: binding.clone(),
            directory: directory.identity(),
            runner: runner_identity,
            socket: socket_identity,
        };
        let ownership_identity = write_private_runner_ownership(&directory, &record).unwrap();
        // owner.json deliberately records the pre-owner binding so its own
        // inode is not self-referential. Recovery receives the authoritative
        // post-publication owner identity from the external registry binding.
        record.binding = record
            .binding
            .clone()
            .test_with_owner(managed_artifact_identity(ownership_identity));
        (
            temp,
            control_root,
            path,
            record,
            ownership_identity,
            listener,
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_recovery_requires_every_probe_runner_alias_to_be_resolved() {
        let tournament_uuid = Uuid::new_v4();
        let runner_owner = ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid,
            candidate_index: 0,
            role: ManagedOwnerRole::Runner,
        };
        let probe_owner = ManagedOwnerTag {
            role: ManagedOwnerRole::Probe,
            ..runner_owner.clone()
        };
        let runner_key = ManagedKey::test_value(2, 8);
        let (_temp, control_root, path, record, _ownership, _listener) =
            private_recovery_fixture(&runner_owner, runner_key);
        let report = ManagedReconcileReport {
            entries: vec![
                crate::launch_registry::ManagedReconcileEntry {
                    key: Some(ManagedKey::test_value(1, 7)),
                    owner: Some(probe_owner.clone()),
                    artifact_binding: None,
                    outcome: ReconcileOutcome::ResolvedTombstone,
                },
                crate::launch_registry::ManagedReconcileEntry {
                    key: Some(runner_key),
                    owner: Some(runner_owner.clone()),
                    artifact_binding: Some(record.binding.clone()),
                    outcome: ReconcileOutcome::UnknownOrphanRisk(
                        crate::launch_registry::ManagedReconcileCode::ProcessEvidenceUnavailable,
                    ),
                },
            ],
        };
        reconcile_private_runner_controls(&control_root, &report).unwrap();
        assert!(path.join("lterm").is_file());

        let mut ownerless_blocked = report;
        ownerless_blocked.entries[1].outcome = ReconcileOutcome::ResolvedTombstone;
        ownerless_blocked
            .entries
            .push(crate::launch_registry::ManagedReconcileEntry {
                key: Some(ManagedKey::test_value(9, 11)),
                owner: None,
                artifact_binding: None,
                outcome: ReconcileOutcome::ResolvedTombstone,
            });
        let groups = private_runner_alias_groups(&ownerless_blocked)
            .unwrap()
            .expect("resolved generic ownerless tombstone blocked alias grouping");
        let candidate = groups.get(&(tournament_uuid, 0)).unwrap();
        assert_eq!(candidate.len(), 2);
        assert!(
            candidate
                .iter()
                .all(|entry| entry.outcome == ReconcileOutcome::ResolvedTombstone)
        );

        ownerless_blocked.entries.last_mut().unwrap().outcome = ReconcileOutcome::UnknownOrphanRisk(
            crate::launch_registry::ManagedReconcileCode::ProcessEvidenceUnavailable,
        );
        assert!(
            private_runner_alias_groups(&ownerless_blocked)
                .unwrap()
                .is_none()
        );
        assert!(path.join("lterm").is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_recovery_path_swap_preserves_original_and_hostile_replacement() {
        let owner = ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: Uuid::new_v4(),
            candidate_index: 0,
            role: ManagedOwnerRole::Runner,
        };
        let key = ManagedKey::test_value(3, 9);
        let (_temp, control_root, path, record, ownership_identity, _listener) =
            private_recovery_fixture(&owner, key);
        let directory = open_existing_private_dir(&path).unwrap();
        let original = path.with_extension("original");
        std::fs::rename(&path, &original).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::write(path.join("hostile"), b"retain").unwrap();
        let parent = control_root.try_clone_retained_fd().unwrap();
        let leaf = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
        assert!(
            remove_private_runner_control_fd_relative(
                &parent,
                control_root.identity(),
                &leaf,
                &directory,
                PrivateControlCleanupExpectations {
                    ownership: Some(&record),
                    ownership_file: Some(ownership_identity),
                    runner: Some(record.runner),
                    socket: Some(record.socket),
                    partial_socket_leaf: None,
                    partial_owner_leaf: None,
                    allow_unbound_files: false,
                },
                None,
            )
            .is_err()
        );
        assert_eq!(std::fs::read(path.join("hostile")).unwrap(), b"retain");
        assert!(original.join("lterm").is_file());
        assert!(original.join("owner.json").is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn expected_runner_and_socket_relocation_fail_closed_until_restored() {
        for artifact_leaf in ["lterm", "control.sock"] {
            let owner = ManagedOwnerTag {
                kind: ManagedOwnerKind::Speculation,
                tournament_uuid: Uuid::new_v4(),
                candidate_index: 0,
                role: ManagedOwnerRole::Runner,
            };
            let key = ManagedKey::test_value(32, 92);
            let (temp, control_root, path, record, ownership_identity, _listener) =
                private_recovery_fixture(&owner, key);
            let directory = open_existing_private_dir(&path).unwrap();
            let parent = control_root.try_clone_retained_fd().unwrap();
            let leaf = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
            let artifact = path.join(artifact_leaf);
            let relocated = temp.path().join(format!("relocated-{artifact_leaf}"));
            std::fs::rename(&artifact, &relocated).unwrap();

            let cleanup = || {
                remove_private_runner_control_fd_relative(
                    &parent,
                    control_root.identity(),
                    &leaf,
                    &directory,
                    PrivateControlCleanupExpectations {
                        ownership: Some(&record),
                        ownership_file: Some(ownership_identity),
                        runner: Some(record.runner),
                        socket: Some(record.socket),
                        partial_socket_leaf: None,
                        partial_owner_leaf: None,
                        allow_unbound_files: false,
                    },
                    None,
                )
            };
            assert_eq!(cleanup(), Err(ContainmentErrorCode::InvalidIdentity));
            assert!(relocated.exists(), "relocated {artifact_leaf} was deleted");
            assert!(path.is_dir(), "private directory was acknowledged early");

            std::fs::rename(&relocated, &artifact).unwrap();
            cleanup().unwrap();
            assert!(!path.exists(), "restored {artifact_leaf} did not converge");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_cleanup_pre_unlink_swaps_fail_closed_without_ack_proof() {
        let test_config = ScopedSpeculationTestConfig::new();
        let owner = ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: Uuid::new_v4(),
            candidate_index: 0,
            role: ManagedOwnerRole::Runner,
        };
        let key = ManagedKey::test_value(30, 90);
        let (_temp, control_root, path, record, ownership_identity, _listener) =
            private_recovery_fixture(&owner, key);
        let directory = open_existing_private_dir(&path).unwrap();
        let parent = control_root.try_clone_retained_fd().unwrap();
        let leaf = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
        let runner = path.join("lterm");
        let retained = path.join("owned-runner");
        test_config.hostile_swap("before_private_runner_unlink", &runner, &retained, "runner");
        let result = remove_private_runner_control_fd_relative(
            &parent,
            control_root.identity(),
            &leaf,
            &directory,
            PrivateControlCleanupExpectations {
                ownership: Some(&record),
                ownership_file: Some(ownership_identity),
                runner: Some(record.runner),
                socket: Some(record.socket),
                partial_socket_leaf: None,
                partial_owner_leaf: None,
                allow_unbound_files: false,
            },
            None,
        );
        assert_eq!(result, Err(ContainmentErrorCode::InvalidIdentity));
        assert_eq!(std::fs::read(&runner).unwrap(), b"hostile replacement");
        assert_eq!(std::fs::read(&retained).unwrap(), b"runner");
        assert!(path.is_dir());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_cleanup_final_rmdir_swap_preserves_hostile_replacement() {
        let test_config = ScopedSpeculationTestConfig::new();
        let owner = ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: Uuid::new_v4(),
            candidate_index: 0,
            role: ManagedOwnerRole::Runner,
        };
        let key = ManagedKey::test_value(31, 91);
        let (_temp, control_root, path, record, ownership_identity, _listener) =
            private_recovery_fixture(&owner, key);
        let directory = open_existing_private_dir(&path).unwrap();
        let parent = control_root.try_clone_retained_fd().unwrap();
        let leaf = CString::new(path.file_name().unwrap().as_bytes()).unwrap();
        let retained = path.with_extension("owned-directory");
        test_config.hostile_swap(
            "before_private_directory_unlink",
            &path,
            &retained,
            "directory",
        );
        let result = remove_private_runner_control_fd_relative(
            &parent,
            control_root.identity(),
            &leaf,
            &directory,
            PrivateControlCleanupExpectations {
                ownership: Some(&record),
                ownership_file: Some(ownership_identity),
                runner: Some(record.runner),
                socket: Some(record.socket),
                partial_socket_leaf: None,
                partial_owner_leaf: None,
                allow_unbound_files: false,
            },
            None,
        );
        assert_eq!(result, Err(ContainmentErrorCode::InvalidIdentity));
        assert_eq!(std::fs::read(path.join("hostile")).unwrap(), b"retain");
        assert!(retained.is_dir(), "owned empty directory was lost");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_runner_binding_rejects_hostile_final_path_swap() {
        let test_config = ScopedSpeculationTestConfig::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = open_existing_private_dir(temp.path()).unwrap();
        let runner_path = temp.path().join("lterm");
        std::fs::write(&runner_path, b"owned runner").unwrap();
        std::fs::set_permissions(&runner_path, std::fs::Permissions::from_mode(0o500)).unwrap();
        let runner = File::open(&runner_path).unwrap();
        let identity = artifact_identity(&runner.metadata().unwrap());
        revalidate_retained_private_runner(&directory, &runner, identity).unwrap();
        let retained = temp.path().join("owned-runner");
        test_config.hostile_swap(
            "before_private_runner_binding",
            &runner_path,
            &retained,
            "runner",
        );
        failpoint("before_private_runner_binding").unwrap();
        assert_eq!(
            revalidate_retained_private_runner(&directory, &runner, identity),
            Err(ContainmentErrorCode::InvalidIdentity)
        );
        assert_eq!(std::fs::read(&runner_path).unwrap(), b"hostile replacement");
        assert_eq!(std::fs::read(&retained).unwrap(), b"owned runner");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_final_publication_rejects_hostile_owner_and_socket_swaps() {
        let test_config = ScopedSpeculationTestConfig::new();

        let pre_metadata_temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            pre_metadata_temp.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let pre_metadata_directory = open_existing_private_dir(pre_metadata_temp.path()).unwrap();
        let pre_metadata_nonce = Uuid::new_v4();
        let pre_metadata_path = pre_metadata_temp.path().join("control.sock");
        let pre_metadata_leaf = private_socket_temp_leaf(pre_metadata_nonce);
        let pre_metadata_socket = pre_metadata_temp
            .path()
            .join(std::ffi::OsStr::from_bytes(pre_metadata_leaf.as_bytes()));
        let pre_metadata_owned = pre_metadata_temp.path().join("owned-pre-metadata.sock");
        test_config.hostile_swap(
            "after_private_socket_bind_before_identity",
            &pre_metadata_socket,
            &pre_metadata_owned,
            "socket",
        );
        let pre_metadata_result = create_published_seqpacket_listener(
            &pre_metadata_directory,
            &pre_metadata_path,
            pre_metadata_nonce,
        );
        let pre_metadata_replacement = std::fs::symlink_metadata(&pre_metadata_path)
            .map(|metadata| metadata.file_type().is_socket());
        let pre_metadata_original = std::fs::symlink_metadata(&pre_metadata_owned)
            .map(|metadata| metadata.file_type().is_socket());

        let socket_temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(socket_temp.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let socket_directory = open_existing_private_dir(socket_temp.path()).unwrap();
        let socket = socket_temp.path().join("control.sock");
        let socket_owned = socket_temp.path().join("owned.sock");
        test_config.hostile_swap(
            "after_private_socket_rename_before_identity",
            &socket,
            &socket_owned,
            "socket",
        );
        let socket_result =
            create_published_seqpacket_listener(&socket_directory, &socket, Uuid::new_v4());
        let replacement_socket =
            std::fs::symlink_metadata(&socket).map(|metadata| metadata.file_type().is_socket());
        let owned_socket = std::fs::symlink_metadata(&socket_owned)
            .map(|metadata| metadata.file_type().is_socket());

        let owner_temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(owner_temp.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let owner_directory = open_existing_private_dir(owner_temp.path()).unwrap();
        let owner = ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: Uuid::new_v4(),
            candidate_index: 0,
            role: ManagedOwnerRole::Runner,
        };
        let binding = ManagedArtifactBinding::test_value(
            Uuid::new_v4(),
            managed_directory_identity(owner_directory.identity()),
            "private-runner",
            Some(managed_directory_identity(owner_directory.identity())),
        )
        .test_with_files(
            ManagedArtifactIdentity { dev: 1, ino: 2 },
            ManagedArtifactIdentity { dev: 3, ino: 4 },
        );
        let record = PrivateRunnerOwnership {
            schema_version: 1,
            owner,
            slot: 1,
            generation: 2,
            binding,
            directory: owner_directory.identity(),
            runner: PrivateArtifactIdentity { dev: 1, ino: 2 },
            socket: PrivateArtifactIdentity { dev: 3, ino: 4 },
        };
        let owner_path = owner_temp.path().join("owner.json");
        let owner_owned = owner_temp.path().join("owned-owner.json");
        test_config.hostile_swap(
            "after_private_owner_rename_before_identity",
            &owner_path,
            &owner_owned,
            "owner",
        );
        let owner_result = write_private_runner_ownership(&owner_directory, &record);
        let replacement_owner = std::fs::read(&owner_path);
        let owned_owner = std::fs::read(owner_owned)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());

        assert!(pre_metadata_result.is_err());
        assert!(pre_metadata_replacement.unwrap());
        assert!(pre_metadata_original.unwrap());
        assert!(socket_result.is_err());
        assert!(replacement_socket.unwrap());
        assert!(owned_socket.unwrap());
        assert!(owner_result.is_err());
        assert_eq!(replacement_owner.unwrap(), b"hostile replacement");
        assert!(owned_owner.is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_recovery_rejects_external_binding_identity_mismatch() {
        let owner = ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: Uuid::new_v4(),
            candidate_index: 0,
            role: ManagedOwnerRole::Runner,
        };
        let key = ManagedKey::test_value(4, 10);
        let (_temp, control_root, path, record, _ownership, _listener) =
            private_recovery_fixture(&owner, key);
        let mismatched_binding = ManagedArtifactBinding::test_value(
            record.binding.nonce(),
            record.binding.control_root(),
            record.binding.private_leaf(),
            Some(ManagedDirectoryIdentity {
                ino: record.binding.private_directory().unwrap().ino + 1,
                ..record.binding.private_directory().unwrap()
            }),
        );
        let report = ManagedReconcileReport {
            entries: vec![crate::launch_registry::ManagedReconcileEntry {
                key: Some(key),
                owner: Some(owner),
                artifact_binding: Some(mismatched_binding),
                outcome: ReconcileOutcome::ResolvedTombstone,
            }],
        };

        assert!(reconcile_private_runner_controls(&control_root, &report).is_err());
        assert!(path.join("lterm").is_file());
        assert!(path.join("control.sock").exists());
        assert!(path.join("owner.json").is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_recovery_refuses_markerless_directory_without_external_binding() {
        let temp = tempfile::tempdir().unwrap();
        let control_root =
            crate::speculation_fs::open_or_create_private_dir(&temp.path().join("control"))
                .unwrap();
        let owner = ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: Uuid::new_v4(),
            candidate_index: 0,
            role: ManagedOwnerRole::Runner,
        };
        let path = temp
            .path()
            .join("control")
            .join(format!("lterm-g003-{}-candidate-0", owner.tournament_uuid));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let report = ManagedReconcileReport {
            entries: vec![crate::launch_registry::ManagedReconcileEntry {
                key: Some(ManagedKey::test_value(5, 11)),
                owner: Some(owner),
                artifact_binding: None,
                outcome: ReconcileOutcome::ResolvedTombstone,
            }],
        };

        assert!(reconcile_private_runner_controls(&control_root, &report).is_err());
        assert!(path.is_dir());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn creation_pending_recovery_refuses_unbound_contents() {
        let temp = tempfile::tempdir().unwrap();
        let control_root =
            crate::speculation_fs::open_or_create_private_dir(&temp.path().join("control"))
                .unwrap();
        let owner = ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: Uuid::new_v4(),
            candidate_index: 0,
            role: ManagedOwnerRole::Runner,
        };
        let leaf = format!("lterm-g003-{}-candidate-0", owner.tournament_uuid);
        let path = temp.path().join("control").join(&leaf);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::write(path.join("hostile"), b"retain").unwrap();
        let binding = ManagedArtifactBinding::test_value(
            Uuid::new_v4(),
            managed_directory_identity(control_root.identity()),
            &leaf,
            None,
        );
        let report = ManagedReconcileReport {
            entries: vec![crate::launch_registry::ManagedReconcileEntry {
                key: Some(ManagedKey::test_value(6, 12)),
                owner: Some(owner),
                artifact_binding: Some(binding),
                outcome: ReconcileOutcome::ResolvedTombstone,
            }],
        };

        assert!(reconcile_private_runner_controls(&control_root, &report).is_err());
        assert_eq!(std::fs::read(path.join("hostile")).unwrap(), b"retain");
    }

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
        let built = build_fixed_bwrap_invocation(identity).unwrap();
        assert_eq!(built.executable, std::path::Path::new("/usr/bin/bwrap"));
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
        let capability_mounts = argv
            .windows(3)
            .filter(|triple| matches!(triple[0].as_str(), "--bind-fd" | "--ro-bind-fd"))
            .map(|triple| [triple[0].as_str(), triple[1].as_str(), triple[2].as_str()])
            .collect::<Vec<_>>();
        assert_eq!(
            capability_mounts,
            vec![
                ["--bind-fd", "12", "/workspace"],
                ["--ro-bind-fd", "13", "/run/lterm-control"],
                ["--ro-bind-fd", "14", "/run/lterm-control/control.sock"],
                ["--ro-bind-fd", "11", "/run/lterm-control/lterm"],
            ]
        );
        assert!(
            argv.iter()
                .all(|argument| !argument.starts_with("/proc/self/fd/")),
            "bwrap argv retained a mutable descriptor pathname mount"
        );
        for forbidden in [
            "--as-pid-1",
            "--unshare-all",
            "--unshare-cgroup-try",
            "--not-a-security-boundary",
            "/sys",
            "/var/run",
            "/isolated/candidate",
            "/isolated/control",
        ] {
            assert!(
                !argv.iter().any(|argument| argument == forbidden),
                "{forbidden}"
            );
        }
    }

    #[cfg(all(target_os = "linux", debug_assertions))]
    #[test]
    fn immutable_speculation_controls_are_thread_local_and_parallel_safe_in_process() {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let configured = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                with_speculation_test_config(
                    SpeculationTestConfig {
                        failpoint: Some(OsString::from("runner_after_ready")),
                        action: Some(OsString::from("exit")),
                        observe_pinned_workspace: true,
                        delayed_runner_exec_seconds: Some(6),
                        prepare_failpoint: Some("after_prepared_allocation".to_owned()),
                        ..SpeculationTestConfig::default()
                    },
                    || {
                        barrier.wait();
                        let built = build_fixed_bwrap_invocation(RunnerIdentity {
                            tournament_uuid: Uuid::from_u128(101),
                            candidate_index: 0,
                            generation: 1,
                        })
                        .unwrap();
                        let active = active_speculation_test_config().unwrap();
                        (built.arguments, active.prepare_failpoint)
                    },
                )
            })
        };
        let ordinary = {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                with_speculation_test_config(SpeculationTestConfig::default(), || {
                    barrier.wait();
                    let built = build_fixed_bwrap_invocation(RunnerIdentity {
                        tournament_uuid: Uuid::from_u128(102),
                        candidate_index: 1,
                        generation: 2,
                    })
                    .unwrap();
                    let active = active_speculation_test_config().unwrap();
                    (built.arguments, active.prepare_failpoint)
                })
            })
        };
        let (configured, configured_prepare) = configured.join().unwrap();
        let (ordinary, ordinary_prepare) = ordinary.join().unwrap();
        let configured = configured
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let ordinary = ordinary
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(configured.windows(3).any(|triple| {
            triple
                == [
                    "--setenv",
                    "LTERM_INTERNAL_SPECULATION_RUNNER_FAILPOINT",
                    "runner_after_ready",
                ]
        }));
        assert!(configured.windows(3).any(|triple| {
            triple
                == [
                    "--setenv",
                    "LTERM_INTERNAL_SPECULATION_OBSERVE_PINNED_WORKSPACE",
                    "1",
                ]
        }));
        assert!(configured.iter().any(|argument| argument == "/usr/bin/sh"));
        assert!(configured.iter().any(|argument| argument == "6"));
        assert_eq!(
            configured_prepare.as_deref(),
            Some("after_prepared_allocation")
        );
        assert!(
            ordinary
                .iter()
                .any(|argument| argument == "/run/lterm-control/lterm")
        );
        assert!(!ordinary.iter().any(|argument| {
            argument == "LTERM_INTERNAL_SPECULATION_RUNNER_FAILPOINT"
                || argument == "LTERM_INTERNAL_SPECULATION_OBSERVE_PINNED_WORKSPACE"
                || argument == "/usr/bin/sh"
        }));
        assert!(ordinary_prepare.is_none());
    }

    #[test]
    fn go_receipts_use_one_monotonic_epoch_and_enforce_the_fifty_ms_bound() {
        let tournament_uuid = Uuid::from_u128(7);
        let receipt = |candidate_index, received_monotonic_ns| GoReceiptEvidence {
            identity: RunnerIdentity {
                tournament_uuid,
                candidate_index,
                generation: 9,
            },
            received_monotonic_ns,
        };
        assert_eq!(
            go_receipt_skew_ns([receipt(0, 1_000), receipt(1, 50_001_000)]),
            Ok(MAX_GO_RECEIPT_SKEW_NS)
        );
        assert_eq!(
            go_receipt_skew_ns([receipt(0, 1_000), receipt(1, 50_001_001)]),
            Err(ContainmentErrorCode::Timeout)
        );
        assert_eq!(
            go_receipt_skew_ns([receipt(1, 1_000), receipt(0, 2_000)]),
            Err(ContainmentErrorCode::InvalidIdentity)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deliberate_monotonic_receipt_skew_over_fifty_ms_is_rejected() {
        let tournament_uuid = Uuid::from_u128(7);
        let first = monotonic_now_ns().unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let second = monotonic_now_ns().unwrap();
        assert!(second.abs_diff(first) > MAX_GO_RECEIPT_SKEW_NS);
        assert_eq!(
            go_receipt_skew_ns([
                GoReceiptEvidence {
                    identity: RunnerIdentity {
                        tournament_uuid,
                        candidate_index: 0,
                        generation: 9,
                    },
                    received_monotonic_ns: first,
                },
                GoReceiptEvidence {
                    identity: RunnerIdentity {
                        tournament_uuid,
                        candidate_index: 1,
                        generation: 9,
                    },
                    received_monotonic_ns: second,
                },
            ]),
            Err(ContainmentErrorCode::Timeout)
        );
    }

    #[test]
    fn action_deadlines_are_explicit_and_never_exceed_the_control_bound() {
        assert!(ContainmentDeadline::from_now(Duration::from_millis(1)).is_ok());
        assert!(ContainmentDeadline::from_now(MAX_CONTAINMENT_ACTION_TIME).is_ok());
        assert!(matches!(
            ContainmentDeadline::from_now(Duration::ZERO),
            Err(ContainmentErrorCode::Timeout)
        ));
        assert!(matches!(
            ContainmentDeadline::from_now(MAX_CONTAINMENT_ACTION_TIME + Duration::from_nanos(1)),
            Err(ContainmentErrorCode::Timeout)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_runner_copy_survives_path_replacement_and_reads_back_exact_content() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("source-lterm");
        std::fs::write(&source_path, b"retained-object").unwrap();
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o500)).unwrap();
        let retained = retain_current_executable(&source_path).unwrap();
        std::fs::rename(&source_path, root.path().join("original-moved")).unwrap();
        std::fs::write(&source_path, b"replacement-object").unwrap();
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o500)).unwrap();

        let copy_path = root.path().join("runner-copy");
        let mut copy = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o500)
            .open(&copy_path)
            .unwrap();
        copy_retained_executable(&retained, &mut copy).unwrap();
        verify_runner_copy(&retained, &copy).unwrap();
        assert_eq!(std::fs::read(copy_path).unwrap(), b"retained-object");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_connect_and_missing_sync_eof_expire_without_blocking_the_actor() {
        let root = tempfile::tempdir().unwrap();
        let listener = create_seqpacket_listener(&root.path().join("control.sock")).unwrap();
        assert!(matches!(
            wait_fd_until(
                &listener,
                libc::POLLIN,
                ContainmentDeadline::from_now(Duration::from_millis(10)).unwrap(),
            ),
            Err(ContainmentErrorCode::Timeout)
        ));

        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let read = unsafe { File::from_raw_fd(descriptors[0]) };
        let _write = unsafe { File::from_raw_fd(descriptors[1]) };
        assert!(matches!(
            wait_fd_until(
                &read,
                libc::POLLIN,
                ContainmentDeadline::from_now(Duration::from_millis(10)).unwrap(),
            ),
            Err(ContainmentErrorCode::Timeout)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn step5_actor_can_own_control_while_observer_moves_to_a_waiter() {
        fn assert_send<T: Send>() {}
        assert_send::<CandidateControl>();
        assert_send::<CandidateObserver>();

        let _actor_go: fn(
            &mut CandidateControl,
            ContainmentDeadline,
        ) -> ContainmentResult<GoSendEvidence> = send_go;
        let _actor_release: fn(
            &mut CandidateControl,
            PayloadPlacementEvidence,
            ContainmentDeadline,
        ) -> ContainmentResult<()> = send_payload_release;
        let _actor_decision: fn(
            &mut CandidateControl,
            DecisionKind,
            ContainmentDeadline,
        ) -> ContainmentResult<()> = send_select_or_abort;
        let _waiter_event: fn(
            &mut CandidateObserver,
            ContainmentDeadline,
        ) -> ContainmentResult<ContainmentEvent> = receive_execution_event;
        let _waiter_sync: fn(
            &mut CandidateObserver,
            ContainmentDeadline,
        ) -> ContainmentResult<ContainmentEvent> = observe_sync_eof;
        let _waiter_reap: fn(
            &mut CandidateObserver,
            ContainmentDeadline,
        ) -> ContainmentResult<ContainmentEvent> = observe_managed_reaped;
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observer_first_wait_never_holds_the_sequence_lock() {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    descriptors.as_mut_ptr(),
                )
            },
            0
        );
        let observer_peer = unsafe { File::from_raw_fd(descriptors[0]) };
        let actor_peer = unsafe { File::from_raw_fd(descriptors[1]) };
        let identity = RunnerIdentity {
            tournament_uuid: Uuid::from_u128(7),
            candidate_index: 0,
            generation: 9,
        };
        let protocol = Arc::new(Mutex::new(CandidateProtocol {
            validator: SequenceValidator::new(identity).unwrap(),
            output_limit_observed: false,
        }));
        let observer_protocol = Arc::clone(&protocol);
        let waiter = std::thread::spawn(move || {
            receive_protocol_frame(
                &observer_peer,
                &observer_protocol,
                ContainmentDeadline::from_now(Duration::from_secs(1)).unwrap(),
            )
        });
        std::thread::sleep(Duration::from_millis(25));
        {
            assert!(protocol.try_lock().is_ok());
        }
        send_frame_packet(
            &actor_peer,
            &ControlFrame::new(identity, 0, ControlMessage::Hello),
        )
        .unwrap();
        assert!(matches!(
            waiter.join().unwrap().unwrap().message,
            ControlMessage::Hello
        ));
        assert_eq!(protocol.lock().unwrap().validator.next_sequence(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_binding_rejects_stale_generation_and_sibling_candidate() {
        let identity = RunnerIdentity {
            tournament_uuid: Uuid::from_u128(7),
            candidate_index: 0,
            generation: 9,
        };
        let membership = |candidate_index, generation| {
            ManagedCgroupMembership::new(
                "/delegated/candidate/control".into(),
                ManagedCgroupDirectoryIdentity {
                    boot_uuid: Uuid::from_u128(1),
                    dev: 2,
                    ino: 3,
                    statx_mnt_id_unique: 4,
                },
                candidate_index,
                generation,
            )
            .unwrap()
        };
        assert_eq!(validate_peer_binding(&membership(0, 9), identity), Ok(()));
        assert_eq!(
            validate_peer_binding(&membership(0, 10), identity),
            Err(ContainmentErrorCode::PeerRejected)
        );
        assert_eq!(
            validate_peer_binding(&membership(1, 9), identity),
            Err(ContainmentErrorCode::PeerRejected)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn workspace_scan_rejects_symlink_hardlink_fifo_socket_and_setid_residue() {
        fn workspace() -> (tempfile::TempDir, ValidatedDirectory) {
            let root = tempfile::tempdir().unwrap();
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let directory = open_existing_workspace_dir(root.path()).unwrap();
            (root, directory)
        }
        let assert_rejected = |directory: &ValidatedDirectory| {
            assert_eq!(
                scan_workspace(directory, ContainmentDeadline::control_action()),
                Err(ContainmentErrorCode::InvalidIdentity)
            );
        };

        let (root, directory) = workspace();
        std::os::unix::fs::symlink("missing", root.path().join("symlink")).unwrap();
        assert_rejected(&directory);

        let (root, directory) = workspace();
        std::fs::write(root.path().join("first"), b"fixed").unwrap();
        std::fs::hard_link(root.path().join("first"), root.path().join("second")).unwrap();
        assert_rejected(&directory);

        let (root, directory) = workspace();
        let fifo = CString::new(root.path().join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert_rejected(&directory);

        let (root, directory) = workspace();
        let _listener = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
        assert_rejected(&directory);

        let (root, directory) = workspace();
        let setid = root.path().join("setid");
        std::fs::write(&setid, b"fixed").unwrap();
        std::fs::set_permissions(&setid, std::fs::Permissions::from_mode(0o4500)).unwrap();
        assert_rejected(&directory);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_actions_match_only_their_exact_durable_pending_edge() {
        use crate::speculation_registry::CgroupForwardState as F;
        use CgroupLifecycleState as L;
        let from = F::PayloadAttached;
        assert!(recovery_candidate_action_allowed(
            L::ParentKillPending { from },
            RecoveryAction::KillParent { candidate: 0 },
        ));
        assert!(recovery_candidate_action_allowed(
            L::ParentKillPending { from },
            RecoveryAction::ProveParentEmpty { candidate: 0 },
        ));
        assert!(recovery_candidate_action_allowed(
            L::PayloadRemovePending { from },
            RecoveryAction::RemovePayload { candidate: 0 },
        ));
        assert!(recovery_candidate_action_allowed(
            L::ControlRemovePending { from },
            RecoveryAction::RemoveControl { candidate: 0 },
        ));
        assert!(recovery_candidate_action_allowed(
            L::ParentRemovePending { from },
            RecoveryAction::RemoveParent { candidate: 0 },
        ));
        assert!(!recovery_candidate_action_allowed(
            L::ParentEmpty { from },
            RecoveryAction::RemovePayload { candidate: 0 },
        ));
        assert!(!recovery_candidate_action_allowed(
            L::PayloadRemoved { from },
            RecoveryAction::RemovePayload { candidate: 0 },
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn every_step4_durable_edge_has_distinct_before_and_after_failpoints() {
        let mut names = std::collections::BTreeSet::new();
        assert!(DURABLE_EDGE_FAILPOINTS.len() >= 27);
        for (before, after) in DURABLE_EDGE_FAILPOINTS {
            assert!(before.starts_with("before_"), "{before}");
            assert!(after.starts_with("after_"), "{after}");
            assert_eq!(before.strip_prefix("before_"), after.strip_prefix("after_"));
            assert!(names.insert(*before), "duplicate {before}");
            assert!(names.insert(*after), "duplicate {after}");
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_prepare_fails_before_any_filesystem_mutation() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("must-not-exist");
        let result = validate_prepare(
            PrepareInputs {
                tournament_uuid: Uuid::from_u128(1),
                generation: 1,
                source: marker.clone(),
                candidates: [marker.clone(), marker.clone()],
                ledger_root: marker.clone(),
                cgroup_root: marker.clone(),
                control_root: marker.clone(),
                argv: vec![OsString::from("true")],
            },
            ContainmentDeadline::control_action(),
        );
        assert!(matches!(result, Err(ContainmentErrorCode::Unsupported)));
        assert!(!marker.exists());
    }
}

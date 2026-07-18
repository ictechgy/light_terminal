//! Linux-only speculation containment adapter.

#[cfg(target_os = "linux")]
use crate::launch_registry::{
    ControlCgroupPlacement, MANAGED_SYNC_PIPE_TARGET_FD, ManagedAuxiliary,
    ManagedCgroupDirectoryIdentity, ManagedCgroupMembership, ManagedController,
    ManagedDescendantProof, ManagedExecutablePolicy, ManagedLaunchRequest, ManagedOwnerKind,
    ManagedOwnerOutcome, ManagedOwnerRole, ManagedOwnerTag, ManagedPlacement, ManagedWaiter,
    SyncPipeWrite, launch_managed_process, reconcile_managed_owner,
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
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
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

    fn candidate_path(&self, candidate_index: u8) -> ContainmentResult<PathBuf> {
        let candidate = self
            .candidates
            .get(usize::from(candidate_index))
            .ok_or(ContainmentErrorCode::InvalidIdentity)?;
        Ok(PathBuf::from(OsString::from_vec(
            candidate.canonical_locator_bytes().to_vec(),
        )))
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
    #[cfg(all(debug_assertions, target_os = "linux"))]
    if std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref() == Some(std::ffi::OsStr::new("1")) {
        if let Some(value) = std::env::var_os("LTERM_INTERNAL_SPECULATION_FAILPOINT")
            && (value.as_bytes().starts_with(b"runner_")
                || value.as_bytes() == b"probe_after_workspace_canary_write")
            && value.as_bytes().len() <= 96
        {
            arguments.extend([
                OsString::from("--setenv"),
                OsString::from("LTERM_INTERNAL_SPECULATION_RUNNER_FAILPOINT"),
                value,
            ]);
        }
        if std::env::var_os("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION").as_deref()
            == Some(std::ffi::OsStr::new("exit"))
        {
            arguments.extend([
                OsString::from("--setenv"),
                OsString::from("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION"),
                OsString::from("exit"),
            ]);
        }
    }
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
    #[cfg(debug_assertions)]
    let test_executable = (std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref()
        == Some(std::ffi::OsStr::new("1")))
    .then(|| std::env::var_os("LTERM_INTERNAL_SPECULATION_SELF_EXE"))
    .flatten();
    #[cfg(not(debug_assertions))]
    let test_executable: Option<OsString> = None;
    let current_executable_path = test_executable
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
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
    if std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref() == Some(std::ffi::OsStr::new("1"))
        && std::env::var("LTERM_INTERNAL_SPECULATION_FAILPOINT").as_deref() == Ok(_name)
    {
        if std::env::var_os("LTERM_INTERNAL_SPECULATION_FAILPOINT_ACTION").as_deref()
            == Some(std::ffi::OsStr::new("exit"))
        {
            unsafe { libc::_exit(86) };
        }
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    Ok(())
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
struct PrivateRunnerControl {
    directory: ValidatedDirectory,
    path: PathBuf,
    socket_path: PathBuf,
    listener: Option<File>,
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
impl Drop for PrivateRunnerControl {
    fn drop(&mut self) {
        self.listener.take();
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(self.path.join("lterm"));
        let _ = std::fs::remove_dir(&self.path);
    }
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
    _lifetime: Arc<PrivateRunnerControl>,
}

#[cfg(target_os = "linux")]
pub(crate) struct CandidateObserver {
    identity: RunnerIdentity,
    peer: File,
    protocol: Arc<Mutex<CandidateProtocol>>,
    controller: ManagedController,
    waiter: Option<ManagedWaiter>,
    sync_read: File,
    payload_node: RetainedCgroupNode,
    payload_membership: ManagedCgroupMembership,
    sync_eof_observed: bool,
    managed_reaped_observed: bool,
    _lifetime: Arc<PrivateRunnerControl>,
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
    let mut private_control = prepare_runner_control(context, candidate_index)?;
    let invocation = build_fixed_bwrap_invocation(
        &context.candidate_path(candidate_index)?,
        &private_control.path,
        identity,
    )?;
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
    failpoint("before_managed_launch")?;
    let managed = launch_managed_process(ManagedLaunchRequest {
        owner: Some(ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: identity.tournament_uuid,
            candidate_index,
            role: owner_role,
        }),
        executable_policy: ManagedExecutablePolicy::PinnedSystemBwrap,
        placement: ManagedPlacement::CgroupV2(placement),
        auxiliary: ManagedAuxiliary::SyncPipeWrite(
            SyncPipeWrite::new(sync_write, MANAGED_SYNC_PIPE_TARGET_FD)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
        ),
        executable: invocation.executable,
        arguments: invocation.arguments,
        current_dir: None,
        environment: Vec::new(),
    })
    .map_err(|_| ContainmentErrorCode::PinnedBwrapFailure)?;
    let owner_receipt = managed
        .owner_receipt
        .clone()
        .ok_or(ContainmentErrorCode::EvidenceUnavailable)?;
    failpoint("after_managed_launch")?;
    let listener = private_control
        .listener
        .as_ref()
        .ok_or(ContainmentErrorCode::PeerRejected)?;
    let peer = accept_authenticated_peer(
        listener,
        &managed.controller,
        &control_membership,
        identity,
        deadline,
    )?;
    failpoint("after_control_accept")?;
    failpoint("before_control_unlink")?;
    std::fs::remove_file(&private_control.socket_path)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    failpoint("after_control_unlink")?;
    private_control.listener.take();
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
    for directory in [
        &context.candidates[usize::from(candidate_index)],
        &private_control.directory,
    ] {
        directory.revalidate().map_err(map_evidence)?;
    }
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
    let lifetime = Arc::new(private_control);
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
            controller: managed.controller,
            waiter: Some(managed.waiter),
            sync_read,
            payload_node: observer_payload_node,
            payload_membership,
            sync_eof_observed: false,
            managed_reaped_observed: false,
            _lifetime: lifetime,
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
    let waiter = observer
        .waiter
        .as_mut()
        .ok_or(ContainmentErrorCode::EvidenceUnavailable)?;
    waiter.wait_until(deadline.instant()).map_err(|_| {
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
    context.control_root.revalidate().map_err(map_evidence)?;
    let leaf = cgroup_name(&format!(
        "lterm-g003-{}-candidate-{candidate_index}",
        context.identity.tournament_uuid
    ))?;
    let root = context
        .control_root
        .try_clone_retained_fd()
        .map_err(map_evidence)?;
    if unsafe { libc::mkdirat(root.as_raw_fd(), leaf.as_ptr(), 0o700) } != 0 {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let path = context
        .control_root_path()
        .join(std::ffi::OsStr::from_bytes(leaf.to_bytes()));
    let directory = open_existing_private_dir(&path).map_err(map_evidence)?;
    let runner_path = path.join("lterm");
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
    directory
        .try_clone_retained_fd()
        .map_err(map_evidence)?
        .sync_all()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let socket_path = path.join("control.sock");
    let listener = create_seqpacket_listener(&socket_path)?;
    directory.revalidate().map_err(map_evidence)?;
    Ok(PrivateRunnerControl {
        directory,
        path,
        socket_path,
        listener: Some(listener),
    })
}

#[cfg(target_os = "linux")]
fn create_seqpacket_listener(path: &Path) -> ContainmentResult<File> {
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
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
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
        run_real_component_driver()?;
        println!("speculation-real-cases=14");
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
            match reconcile_managed_owner(&owner)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            {
                ManagedOwnerOutcome::Absent | ManagedOwnerOutcome::ResolvedTombstone(_) => {}
                ManagedOwnerOutcome::UnknownOrphanRisk(_) => {
                    return Err(ContainmentErrorCode::EvidenceUnavailable);
                }
            }
            cleanup_restart_record(&record_path, &mut record)
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
fn run_real_component_driver() -> ContainmentResult<()> {
    let fixture = std::env::var_os("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::Unsupported)?;
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
    let self_exe = std::env::var_os("LTERM_INTERNAL_SPECULATION_SELF_EXE")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::EvidenceUnavailable)?;

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
        executable_policy: ManagedExecutablePolicy::Legacy,
        placement: ManagedPlacement::CgroupV2(placement),
        auxiliary: ManagedAuxiliary::None,
        executable,
        arguments,
        current_dir: None,
        environment,
    })
    .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
#[derive(Clone, Copy)]
enum RealExecutionExpectation {
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
        let second_connect = connect_seqpacket_test(&containment.control._lifetime.socket_path);
        let second_connect_accepted = second_connect.is_ok();
        drop(second_connect);
        let after_second_connect = test_open_fd_count()?;
        if usize::from(observation.identity.candidate_index) != candidate
            || usize::from(observation.managed_owner.candidate_index) != candidate
            || observation.managed_owner.role != ManagedOwnerRoleEvidence::Runner
            || containment.control._lifetime.socket_path.exists()
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

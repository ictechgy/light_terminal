//! Linux-only speculation containment adapter.

#[cfg(target_os = "linux")]
use crate::launch_registry::{
    ControlCgroupPlacement, MANAGED_SYNC_PIPE_TARGET_FD, ManagedAuxiliary,
    ManagedCgroupDirectoryIdentity, ManagedCgroupMembership, ManagedController,
    ManagedDescendantProof, ManagedExecutablePolicy, ManagedLaunchRequest, ManagedOwnerKind,
    ManagedOwnerOutcome, ManagedOwnerReceipt, ManagedOwnerRole, ManagedOwnerTag, ManagedPlacement,
    ManagedWaiter, SyncPipeWrite, launch_managed_process, reconcile_managed_owner,
};
use crate::speculation_fs::{DurableDirectoryIdentity, EvidenceError, ValidatedDirectory};
#[cfg(target_os = "linux")]
use crate::speculation_fs::{
    durable_identity_from_fd, open_existing_delegated_cgroup_root, open_existing_private_dir,
    open_existing_workspace_dir, validate_no_overlap,
};
#[cfg(target_os = "linux")]
use crate::speculation_registry::{
    AbsenceDisposition, CgroupForwardState, CgroupLifecycleState, ManagedOwnerEvidence,
    TournamentCgroupLifecycleState, TournamentRecord, TournamentRecoveryRecord,
};
use crate::speculation_registry::{CgroupComponent, ManagedOwnerRoleEvidence};
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
    if std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref() == Some(std::ffi::OsStr::new("1"))
        && let Some(value) = std::env::var_os("LTERM_INTERNAL_SPECULATION_FAILPOINT")
        && value.as_bytes().starts_with(b"runner_")
        && value.as_bytes().len() <= 96
    {
        arguments.extend([
            OsString::from("--setenv"),
            OsString::from("LTERM_INTERNAL_SPECULATION_RUNNER_FAILPOINT"),
            value,
        ]);
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
    let root = reopen_recovery_root(record)?;
    enable_pids(&root)?;
    let name = cgroup_name(&format!("lterm-g003-{}", record.status.tournament_uuid))?;
    let membership = join_membership(&root.membership, name.to_bytes())?;
    let (domain, adopted) = create_or_adopt_cgroup_child(&root, &name, membership)?;
    for candidate in 0_u8..2 {
        let candidate_name = cgroup_name(&format!("candidate-{candidate}"))?;
        if open_observed_cgroup_child(
            &domain,
            &candidate_name,
            join_membership(&domain.membership, candidate_name.to_bytes())?,
        )?
        .is_some()
        {
            return Ok(RecoveryEvidence::RollbackRequired);
        }
    }
    enable_pids(&domain)?;
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
    let Some((_, _, domain)) = reopen_recovery_domain(record)? else {
        return Ok(RecoveryEvidence::RollbackRequired);
    };
    let parent_name = cgroup_name(&format!("candidate-{candidate}"))?;
    let parent_membership = join_membership(&domain.membership, parent_name.to_bytes())?;

    let (node, adopted) = match component {
        CgroupComponent::Parent => {
            let result = create_or_adopt_cgroup_child(&domain, &parent_name, parent_membership)?;
            for leaf in [c"control", c"payload"] {
                if open_observed_cgroup_child(
                    &result.0,
                    leaf,
                    join_membership(&result.0.membership, leaf.to_bytes())?,
                )?
                .is_some()
                {
                    return Ok(RecoveryEvidence::RollbackRequired);
                }
            }
            enable_pids(&result.0)?;
            result
        }
        CgroupComponent::Control => {
            let Some(parent) =
                reopen_cgroup_child(&domain, &parent_name, parent_membership, evidence.parent)?
            else {
                return Ok(RecoveryEvidence::RollbackRequired);
            };
            if open_observed_cgroup_child(
                &parent,
                c"payload",
                join_membership(&parent.membership, b"payload")?,
            )?
            .is_some()
            {
                return Ok(RecoveryEvidence::RollbackRequired);
            }
            create_or_adopt_cgroup_child(
                &parent,
                c"control",
                join_membership(&parent.membership, b"control")?,
            )?
        }
        CgroupComponent::Payload => {
            let Some(parent) =
                reopen_cgroup_child(&domain, &parent_name, parent_membership, evidence.parent)?
            else {
                return Ok(RecoveryEvidence::RollbackRequired);
            };
            if reopen_cgroup_child(
                &parent,
                c"control",
                join_membership(&parent.membership, b"control")?,
                evidence.control,
            )?
            .is_none()
            {
                return Ok(RecoveryEvidence::RollbackRequired);
            }
            create_or_adopt_cgroup_child(
                &parent,
                c"payload",
                join_membership(&parent.membership, b"payload")?,
            )?
        }
    };
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
        prove_domain_task_free(&observed)?;
        return Ok((observed, true));
    }
    create_cgroup_child(parent, name, membership).map(|created| (created, false))
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
    identity: RunnerIdentity,
    peer: File,
    validator: SequenceValidator,
    controller: ManagedController,
    owner_receipt: ManagedOwnerReceipt,
    waiter: Option<ManagedWaiter>,
    sync_read: File,
    control: PrivateRunnerControl,
    payload_membership: ManagedCgroupMembership,
    payload_proof: Option<ManagedDescendantProof>,
    output_limit_observed: bool,
    sync_eof_observed: bool,
    managed_reaped_observed: bool,
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
        let owner = self.owner_receipt.owner();
        CandidateObservation {
            identity: self.identity,
            managed_owner: ManagedOwnerEvidence {
                candidate_index: owner.candidate_index,
                role: match owner.role {
                    ManagedOwnerRole::Probe => ManagedOwnerRoleEvidence::Probe,
                    ManagedOwnerRole::Runner => ManagedOwnerRoleEvidence::Runner,
                },
                slot: self.owner_receipt.slot(),
                generation: self.owner_receipt.generation(),
            },
        }
    }
}

#[cfg(target_os = "linux")]
impl fmt::Debug for CandidateContainment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CandidateContainment")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct CandidateContainment;

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
    Ok(CandidateContainment {
        identity,
        peer,
        validator,
        controller: managed.controller,
        owner_receipt,
        waiter: Some(managed.waiter),
        sync_read,
        control: private_control,
        payload_membership,
        payload_proof: None,
        output_limit_observed: false,
        sync_eof_observed: false,
        managed_reaped_observed: false,
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
    let probe_argv = vec![
        b"/run/lterm-control/lterm".to_vec(),
        b"--internal-speculation-probe-v1".to_vec(),
    ];
    let mut containment = launch_runner_with_argv(
        context,
        topology,
        candidate_index,
        &probe_argv,
        ManagedOwnerRole::Probe,
        deadline,
    )?;
    transfer_payload_fd(&mut containment, topology, deadline)?;
    send_go(&mut containment, deadline)?;
    receive_go_receipt(&mut containment, deadline)?;
    receive_payload_placed(&mut containment, topology, deadline)?;
    send_payload_release(&mut containment, deadline)?;
    let leader = receive_leader_exited(&mut containment, deadline)?;
    let payload = topology.payload()?;
    write_leaf(payload, c"cgroup.kill", b"1\n")?;
    wait_populated_zero(payload, deadline)?;
    let drained = receive_output_drained(&mut containment, deadline)?;
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
    send_select_or_abort(&mut containment, DecisionKind::Abort, deadline)?;
    observe_sync_eof(&mut containment, deadline)?;
    observe_managed_reaped(&mut containment, deadline)?;
    finish_containment(containment)?;
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
    containment: &mut CandidateContainment,
    topology: &CandidateTopology,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    if topology.candidate_index != containment.identity.candidate_index {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let payload = topology.payload()?;
    let placement_fd = open_cgroup_procs(payload)?;
    failpoint("before_payload_fd_evidence")?;
    let placement = placement_descriptor_evidence(&placement_fd, payload, containment.identity)?;
    failpoint("after_payload_fd_evidence")?;
    let frame = ControlFrame::new(
        containment.identity,
        containment.validator.next_sequence(),
        ControlMessage::ReadyAck { placement },
    );
    containment
        .validator
        .accept(&frame)
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
    failpoint("before_payload_fd_send")?;
    wait_fd_until(&containment.peer, libc::POLLOUT, deadline)?;
    send_frame_with_one_fd(&containment.peer, &frame, &placement_fd)
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
    failpoint("after_payload_fd_send")?;
    let ack = receive_validated_frame(&containment.peer, &mut containment.validator, deadline)?;
    if !matches!(ack.message, ControlMessage::PayloadFdAck) {
        return Err(ContainmentErrorCode::DescriptorViolation);
    }
    Ok(ContainmentEvent::PayloadFdAck {
        candidate: containment.identity.candidate_index,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn transfer_payload_fd(
    _containment: &mut CandidateContainment,
    _topology: &CandidateTopology,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_go(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<GoSendEvidence> {
    let sent_monotonic_ns = monotonic_now_ns()?;
    send_validated_frame(
        &containment.peer,
        &mut containment.validator,
        ControlMessage::Go,
        containment.identity,
        deadline,
    )?;
    Ok(GoSendEvidence {
        candidate: containment.identity.candidate_index,
        sent_monotonic_ns,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn send_go(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<GoSendEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_go_receipt(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<GoReceiptEvidence> {
    let received =
        receive_validated_frame(&containment.peer, &mut containment.validator, deadline)?;
    let ControlMessage::GoReceived { monotonic_ns } = received.message else {
        return Err(ContainmentErrorCode::PeerRejected);
    };
    if monotonic_ns == 0 {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    Ok(GoReceiptEvidence {
        identity: containment.identity,
        received_monotonic_ns: monotonic_ns,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_go_receipt(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<GoReceiptEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_payload_placed(
    containment: &mut CandidateContainment,
    topology: &CandidateTopology,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    let placed = receive_validated_frame(&containment.peer, &mut containment.validator, deadline)?;
    if !matches!(placed.message, ControlMessage::PayloadPlaced) {
        return Err(ContainmentErrorCode::PlacementUnproven);
    }
    let payload = topology.payload()?;
    let observed = read_single_cgroup_pid(payload)?;
    failpoint("before_payload_membership_proof")?;
    let proof = containment
        .controller
        .prove_descendant_in_cgroup(observed, &containment.payload_membership)
        .map_err(|_| ContainmentErrorCode::PlacementUnproven)?;
    failpoint("after_payload_membership_proof")?;
    if proof.membership() != &containment.payload_membership {
        return Err(ContainmentErrorCode::PlacementUnproven);
    }
    prove_namespace_isolation(observed)?;
    containment.payload_proof = Some(proof);
    Ok(ContainmentEvent::PayloadPlaced {
        candidate: containment.identity.candidate_index,
    })
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
    _containment: &mut CandidateContainment,
    _topology: &CandidateTopology,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_payload_release(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    if containment.payload_proof.is_none() {
        return Err(ContainmentErrorCode::PlacementUnproven);
    }
    failpoint("before_payload_release")?;
    send_validated_frame(
        &containment.peer,
        &mut containment.validator,
        ControlMessage::PayloadRelease,
        containment.identity,
        deadline,
    )?;
    failpoint("after_payload_release")
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn send_payload_release(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_candidate_completion(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<[ContainmentEvent; 2]> {
    let leader = receive_leader_exited(containment, deadline)?;
    let drained = receive_output_drained(containment, deadline)?;
    Ok([leader, drained])
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_candidate_completion(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<[ContainmentEvent; 2]> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_leader_exited(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    let event = receive_execution_event(containment, deadline)?;
    if !matches!(event, ContainmentEvent::LeaderExited { .. }) {
        return Err(ContainmentErrorCode::OutputLimit);
    }
    Ok(event)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_execution_event(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    let leader = receive_validated_frame(&containment.peer, &mut containment.validator, deadline)?;
    match leader.message {
        ControlMessage::LeaderExited {
            category,
            elapsed_ns,
        } if elapsed_ns != 0 => Ok(ContainmentEvent::LeaderExited {
            candidate: containment.identity.candidate_index,
            category,
            elapsed_ns,
        }),
        ControlMessage::OutputLimitExceeded { bytes }
            if bytes == crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES + 1
                && !containment.output_limit_observed =>
        {
            containment.output_limit_observed = true;
            Ok(ContainmentEvent::OutputLimitExceeded {
                candidate: containment.identity.candidate_index,
                bytes,
            })
        }
        _ => Err(ContainmentErrorCode::PeerRejected),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_leader_exited(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_execution_event(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn acknowledge_output_cleanup_claimed(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    if !containment.output_limit_observed {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    send_validated_frame(
        &containment.peer,
        &mut containment.validator,
        ControlMessage::OutputCleanupClaimed,
        containment.identity,
        deadline,
    )
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn acknowledge_output_cleanup_claimed(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<()> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_output_drained(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    let drained = receive_validated_frame(&containment.peer, &mut containment.validator, deadline)?;
    let ControlMessage::OutputDrained { bytes } = drained.message else {
        return Err(ContainmentErrorCode::PeerRejected);
    };
    if bytes > crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES && !containment.output_limit_observed
    {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    Ok(ContainmentEvent::OutputDrained {
        candidate: containment.identity.candidate_index,
        bytes,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_output_drained(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_select_or_abort(
    containment: &mut CandidateContainment,
    decision: DecisionKind,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    send_validated_frame(
        &containment.peer,
        &mut containment.validator,
        ControlMessage::ResultAccepted,
        containment.identity,
        deadline,
    )?;
    send_validated_frame(
        &containment.peer,
        &mut containment.validator,
        ControlMessage::Decision { decision },
        containment.identity,
        deadline,
    )?;
    let ack = receive_validated_frame(&containment.peer, &mut containment.validator, deadline)?;
    if !matches!(ack.message, ControlMessage::Ack { decision: observed } if observed == decision) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    Ok(ContainmentEvent::DecisionAck {
        candidate: containment.identity.candidate_index,
        decision,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn send_select_or_abort(
    _containment: &mut CandidateContainment,
    _decision: DecisionKind,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn observe_sync_eof(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    if containment.sync_eof_observed {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    wait_fd_until(&containment.sync_read, libc::POLLIN, deadline)?;
    let mut byte = [0_u8; 1];
    if containment
        .sync_read
        .read(&mut byte)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        != 0
    {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    containment.sync_eof_observed = true;
    Ok(ContainmentEvent::SyncEof {
        candidate: containment.identity.candidate_index,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn observe_sync_eof(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn observe_managed_reaped(
    containment: &mut CandidateContainment,
    deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    if containment.managed_reaped_observed {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let waiter = containment
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
    containment.waiter.take();
    containment.managed_reaped_observed = true;
    Ok(ContainmentEvent::ManagedReaped {
        candidate: containment.identity.candidate_index,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn observe_managed_reaped(
    _containment: &mut CandidateContainment,
    _deadline: ContainmentDeadline,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn finish_containment(containment: CandidateContainment) -> ContainmentResult<()> {
    if !containment.sync_eof_observed || !containment.managed_reaped_observed {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn finish_containment(_containment: CandidateContainment) -> ContainmentResult<()> {
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
fn accept_authenticated_peer(
    listener: &File,
    controller: &ManagedController,
    membership: &ManagedCgroupMembership,
    _identity: RunnerIdentity,
    deadline: ContainmentDeadline,
) -> ContainmentResult<File> {
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
    Ok(peer)
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
    if arguments.get(1).and_then(|value| value.to_str())
        != Some("--internal-speculation-containment-test-v1")
    {
        return Ok(false);
    }
    if std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Err(ContainmentErrorCode::Unsupported);
    }
    #[cfg(not(target_os = "linux"))]
    return Err(ContainmentErrorCode::Unsupported);
    #[cfg(target_os = "linux")]
    {
        run_real_component_driver()?;
        println!("speculation-real-cases=4");
        Ok(true)
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
        vec![OsString::from("/usr/bin/yes")],
        None,
        RealExecutionExpectation::Overflow,
    )?;
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
#[derive(Clone, Copy)]
enum RealExecutionExpectation {
    Complete { output_bytes: u64 },
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
    let mut tournament = begin_topology(&context)?;
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
    let mut containments = [
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
    for (candidate, containment) in containments.iter().enumerate() {
        let observation = containment.observation();
        if usize::from(observation.identity.candidate_index) != candidate
            || usize::from(observation.managed_owner.candidate_index) != candidate
            || observation.managed_owner.role != ManagedOwnerRoleEvidence::Runner
            || containment.control.socket_path.exists()
            || std::os::unix::net::UnixStream::connect(&containment.control.socket_path).is_ok()
        {
            return Err(ContainmentErrorCode::InvalidIdentity);
        }
    }
    if containments[0].owner_receipt.slot() == containments[1].owner_receipt.slot() {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    for candidate in 0_u8..2 {
        transfer_payload_fd(
            &mut containments[usize::from(candidate)],
            tournament.candidate(candidate)?,
            ContainmentDeadline::control_action(),
        )?;
    }
    let sends = [
        send_go(&mut containments[0], ContainmentDeadline::control_action())?,
        send_go(&mut containments[1], ContainmentDeadline::control_action())?,
    ];
    if sends[0]
        .sent_monotonic_ns
        .abs_diff(sends[1].sent_monotonic_ns)
        > MAX_GO_RECEIPT_SKEW_NS
    {
        return Err(ContainmentErrorCode::Timeout);
    }
    let receipts = [
        receive_go_receipt(&mut containments[0], ContainmentDeadline::control_action())?,
        receive_go_receipt(&mut containments[1], ContainmentDeadline::control_action())?,
    ];
    go_receipt_skew_ns(receipts)?;
    for candidate in 0_u8..2 {
        let index = usize::from(candidate);
        receive_payload_placed(
            &mut containments[index],
            tournament.candidate(candidate)?,
            ContainmentDeadline::control_action(),
        )?;
        send_payload_release(
            &mut containments[index],
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
        RealExecutionExpectation::Complete { .. } => {
            for candidate in 0_u8..2 {
                let index = usize::from(candidate);
                let event = receive_execution_event(
                    &mut containments[index],
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
                    &mut containments[index],
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
                    &mut containments[index],
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
                &mut containments[index],
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
        let drained = receive_output_drained(
            &mut containments[index],
            ContainmentDeadline::control_action(),
        )?;
        let ContainmentEvent::OutputDrained { bytes, .. } = drained else {
            return Err(ContainmentErrorCode::TerminalBoundaryFailure);
        };
        match expectation {
            RealExecutionExpectation::Complete { output_bytes } if bytes == output_bytes => {}
            RealExecutionExpectation::Overflow
                if bytes > crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES => {}
            _ => return Err(ContainmentErrorCode::OutputLimit),
        }
        evidence[index].output_bytes = bytes;
        send_select_or_abort(
            &mut containments[index],
            DecisionKind::Abort,
            ContainmentDeadline::control_action(),
        )?;
    }
    for mut containment in containments {
        observe_sync_eof(&mut containment, ContainmentDeadline::control_action())?;
        observe_managed_reaped(&mut containment, ContainmentDeadline::control_action())?;
        finish_containment(containment)?;
    }
    cleanup_real_topology(&mut tournament)?;
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
fn cleanup_real_topology(tournament: &mut TournamentTopology) -> ContainmentResult<()> {
    for candidate in 0_u8..2 {
        for action in [
            CandidateCleanupAction::KillParent,
            CandidateCleanupAction::ProveParentEmpty,
            CandidateCleanupAction::RemovePayload,
            CandidateCleanupAction::RemoveControl,
            CandidateCleanupAction::RemoveParent,
        ] {
            perform_candidate_cleanup_action(
                tournament,
                candidate,
                action,
                ContainmentDeadline::control_action(),
            )?;
        }
    }
    perform_tournament_cleanup_action(
        tournament,
        TournamentCleanupAction::ProveEmpty,
        ContainmentDeadline::control_action(),
    )?;
    perform_tournament_cleanup_action(
        tournament,
        TournamentCleanupAction::RemoveDomain,
        ContainmentDeadline::control_action(),
    )?;
    Ok(())
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

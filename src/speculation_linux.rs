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
#[cfg(target_os = "linux")]
use crate::speculation_registry::{
    AbsenceDisposition, CgroupComponent, CgroupLifecycleState, ManagedOwnerRoleEvidence,
    TournamentCgroupLifecycleState, TournamentRecord, TournamentRecoveryRecord,
};
#[cfg(target_os = "linux")]
use crate::speculation_runner::{
    ControlFrame, ControlMessage, PlacementDescriptorEvidence, SequenceValidator, argv_frames,
    receive_frame_packet, send_frame_packet, send_frame_with_one_fd,
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
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
pub(crate) fn validate_prepare(inputs: PrepareInputs) -> ContainmentResult<LiveTournamentContext> {
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
    scan_workspace(&source)?;
    scan_workspace(&candidate_zero)?;
    scan_workspace(&candidate_one)?;
    #[cfg(debug_assertions)]
    let test_executable = (std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref()
        == Some(std::ffi::OsStr::new("1")))
    .then(|| std::env::var_os("LTERM_INTERNAL_SPECULATION_SELF_EXE"))
    .flatten();
    #[cfg(not(debug_assertions))]
    let test_executable: Option<OsString> = None;
    let current_executable = test_executable
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        .canonicalize()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    for candidate in [&candidate_zero, &candidate_one] {
        let path = Path::new(std::ffi::OsStr::from_bytes(
            candidate.canonical_locator_bytes(),
        ));
        if current_executable.starts_with(path) {
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
fn scan_workspace(root: &ValidatedDirectory) -> ContainmentResult<()> {
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
    ) -> ContainmentResult<()> {
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
                    walk(&child, root_identity, depth + 1, next_path_bytes, entries)?;
                }
                _ => return Err(ContainmentErrorCode::InvalidIdentity),
            }
        }
        Ok(())
    }
    walk(&retained, root_identity, 0, 0, &mut entries)?;
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
            if topology.domain.is_some() {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            failpoint("before_tournament_create")?;
            prove_domain_task_free(&topology.root)?;
            enable_pids(&topology.root)?;
            let name = cgroup_name(&format!("lterm-g003-{}", topology.tournament_uuid))?;
            let membership = join_membership(&topology.root.membership, name.to_bytes())?;
            let domain = create_cgroup_child(&topology.root, &name, membership)?;
            enable_pids(&domain)?;
            failpoint("after_tournament_create")?;
            let identity = domain.identity;
            topology.domain = Some(domain);
            Ok(TopologyEvidence::TournamentDomain(identity))
        }
        TopologyAction::CreateCandidateParent { candidate } => {
            let index = usize::from(candidate);
            if index >= topology.candidates.len() || topology.candidates[index].parent.is_some() {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            let domain = topology
                .domain
                .as_ref()
                .ok_or(ContainmentErrorCode::TopologyFailure)?;
            let name = cgroup_name(&format!("candidate-{candidate}"))?;
            let membership = join_membership(&domain.membership, name.to_bytes())?;
            let parent = create_cgroup_child(domain, &name, membership)?;
            enable_pids(&parent)?;
            let identity = parent.identity;
            topology.candidates[index].parent = Some(parent);
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
            if candidate_topology.control.is_some() {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            let membership = join_membership(&parent.membership, b"control")?;
            let control = create_cgroup_child(parent, c"control", membership)?;
            let identity = control.identity;
            candidate_topology.control = Some(control);
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
            if candidate_topology.control.is_none() || candidate_topology.payload.is_some() {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
            let membership = join_membership(&parent.membership, b"payload")?;
            let payload = create_cgroup_child(parent, c"payload", membership)?;
            let identity = payload.identity;
            candidate_topology.payload = Some(payload);
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
            write_leaf(payload, c"pids.max", b"256\n")?;
            if read_leaf(payload, c"pids.max", 64)? != b"256\n" {
                return Err(ContainmentErrorCode::TopologyFailure);
            }
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
    write_leaf(node, c"cgroup.subtree_control", b"+pids\n")?;
    let enabled = read_leaf(node, c"cgroup.subtree_control", 4096)?;
    if !enabled
        .split(|byte| byte.is_ascii_whitespace())
        .any(|controller| controller == b"pids")
    {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
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
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EmptyEvidence {
    pub candidate: u8,
    pub populated_zero: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeEvidence {
    pub candidate: u8,
    pub exited_zero: bool,
    pub output_bytes: u64,
    pub parent_populated_zero: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemovalEvidence {
    pub candidate: u8,
    pub payload_removed: bool,
    pub control_removed: bool,
    pub parent_removed: bool,
}

#[cfg(target_os = "linux")]
pub(crate) fn kill_payload_and_prove_empty(
    topology: &CandidateTopology,
) -> ContainmentResult<EmptyEvidence> {
    let payload = topology.payload()?;
    failpoint("before_payload_kill")?;
    write_leaf(payload, c"cgroup.kill", b"1\n")?;
    wait_populated_zero(payload, Duration::from_secs(5))?;
    failpoint("after_payload_empty")?;
    Ok(EmptyEvidence {
        candidate: topology.candidate_index,
        populated_zero: true,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn kill_payload_and_prove_empty(
    _topology: &CandidateTopology,
) -> ContainmentResult<EmptyEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn cleanup_parent_and_remove(
    tournament: &TournamentTopology,
    topology: &CandidateTopology,
) -> ContainmentResult<RemovalEvidence> {
    let parent = topology
        .parent
        .as_ref()
        .ok_or(ContainmentErrorCode::TopologyFailure)?;
    let domain = tournament
        .domain
        .as_ref()
        .ok_or(ContainmentErrorCode::TopologyFailure)?;
    write_leaf(parent, c"cgroup.kill", b"1\n")?;
    wait_populated_zero(parent, Duration::from_secs(5))?;
    if let Some(payload) = &topology.payload {
        wait_populated_zero(payload, Duration::from_secs(5))?;
        remove_cgroup_child(parent, c"payload", payload.identity)?;
    }
    if let Some(control) = &topology.control {
        wait_populated_zero(control, Duration::from_secs(5))?;
        remove_cgroup_child(parent, c"control", control.identity)?;
    }
    let parent_name = cgroup_name(&format!("candidate-{}", topology.candidate_index))?;
    remove_cgroup_child(domain, &parent_name, parent.identity)?;
    Ok(RemovalEvidence {
        candidate: topology.candidate_index,
        payload_removed: true,
        control_removed: true,
        parent_removed: true,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn cleanup_parent_and_remove(
    _tournament: &TournamentTopology,
    _topology: &CandidateTopology,
) -> ContainmentResult<RemovalEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn remove_tournament_domain(tournament: &TournamentTopology) -> ContainmentResult<()> {
    let domain = tournament
        .domain
        .as_ref()
        .ok_or(ContainmentErrorCode::TopologyFailure)?;
    wait_populated_zero(domain, Duration::from_secs(5))?;
    let name = cgroup_name(&format!("lterm-g003-{}", tournament.tournament_uuid))?;
    remove_cgroup_child(&tournament.root, &name, domain.identity)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn remove_tournament_domain(_tournament: &TournamentTopology) -> ContainmentResult<()> {
    Err(ContainmentErrorCode::Unsupported)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryEvidence {
    CleanupComplete {
        tournament_uuid: Uuid,
        candidates_removed: u8,
        domain_removed: bool,
    },
    RollbackRequired,
}

#[cfg(target_os = "linux")]
pub(crate) fn reconcile_from_record(
    recovery: &TournamentRecoveryRecord,
) -> ContainmentResult<RecoveryEvidence> {
    let TournamentRecoveryRecord::Valid { record, .. } = recovery else {
        return Ok(RecoveryEvidence::RollbackRequired);
    };
    if record.validate().is_err() {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    if reconcile_record_owners(record).is_err() {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    let root = match record.cgroup_root_locator.reopen_and_verify() {
        Ok(root) => root,
        Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
    };
    let root_node = RetainedCgroupNode {
        file: root.try_clone_retained_fd().map_err(map_evidence)?,
        identity: root.identity(),
        membership: membership_for_cgroup_path(Path::new(std::ffi::OsStr::from_bytes(
            root.canonical_locator_bytes(),
        )))?,
    };
    let tournament_name = cgroup_name(&format!("lterm-g003-{}", record.status.tournament_uuid))?;
    let tournament_membership = join_membership(&root_node.membership, tournament_name.to_bytes())?;
    let domain = match reopen_cgroup_child(
        &root_node,
        &tournament_name,
        tournament_membership,
        record.tournament_cgroup.domain,
    ) {
        Ok(domain) => domain,
        Err(_) => return Ok(RecoveryEvidence::RollbackRequired),
    };
    let Some(domain) = domain else {
        let domain_absence_allowed = matches!(
            record.tournament_cgroup.lifecycle,
            TournamentCgroupLifecycleState::Planned
                | TournamentCgroupLifecycleState::CreatePending
                | TournamentCgroupLifecycleState::Removed
        ) || (record.tournament_cgroup.lifecycle
            == TournamentCgroupLifecycleState::RemovePending
            && record
                .cgroups
                .iter()
                .all(|candidate| candidate.lifecycle == CgroupLifecycleState::Removed));
        return Ok(if domain_absence_allowed {
            RecoveryEvidence::CleanupComplete {
                tournament_uuid: record.status.tournament_uuid,
                candidates_removed: 2,
                domain_removed: true,
            }
        } else {
            RecoveryEvidence::RollbackRequired
        });
    };
    if !matches!(
        record.tournament_cgroup.lifecycle,
        TournamentCgroupLifecycleState::Created
            | TournamentCgroupLifecycleState::RemovePending
            | TournamentCgroupLifecycleState::RollbackRequired
    ) {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    for candidate in &record.cgroups {
        if recover_candidate_cgroup(&domain, candidate).is_err() {
            return Ok(RecoveryEvidence::RollbackRequired);
        }
    }
    wait_populated_zero(&domain, Duration::from_secs(5))?;
    if remove_cgroup_child(&root_node, &tournament_name, domain.identity).is_err() {
        return Ok(RecoveryEvidence::RollbackRequired);
    }
    Ok(RecoveryEvidence::CleanupComplete {
        tournament_uuid: record.status.tournament_uuid,
        candidates_removed: 2,
        domain_removed: true,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn reconcile_from_record(
    _recovery: &crate::speculation_registry::TournamentRecoveryRecord,
) -> ContainmentResult<RecoveryEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
fn reconcile_record_owners(record: &TournamentRecord) -> ContainmentResult<()> {
    for evidence in record.managed_owners.iter().flatten() {
        let role = match evidence.role {
            ManagedOwnerRoleEvidence::Probe => ManagedOwnerRole::Probe,
            ManagedOwnerRoleEvidence::Runner => ManagedOwnerRole::Runner,
        };
        let owner = ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: record.status.tournament_uuid,
            candidate_index: evidence.candidate_index,
            role,
        };
        match reconcile_managed_owner(&owner)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        {
            ManagedOwnerOutcome::Absent => {}
            ManagedOwnerOutcome::ResolvedTombstone(key)
                if key.slot() == evidence.slot && key.generation() == evidence.generation => {}
            ManagedOwnerOutcome::ResolvedTombstone(_)
            | ManagedOwnerOutcome::UnknownOrphanRisk(_) => {
                return Err(ContainmentErrorCode::EvidenceUnavailable);
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn recover_candidate_cgroup(
    domain: &RetainedCgroupNode,
    evidence: &crate::speculation_registry::CandidateCgroupEvidence,
) -> ContainmentResult<()> {
    if evidence.lifecycle == CgroupLifecycleState::RollbackRequired {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let parent_name = cgroup_name(&format!("candidate-{}", evidence.candidate_index))?;
    let parent_membership = join_membership(&domain.membership, parent_name.to_bytes())?;
    let parent = reopen_cgroup_child(domain, &parent_name, parent_membership, evidence.parent)?;
    let Some(parent) = parent else {
        return match evidence
            .lifecycle
            .same_boot_absence(CgroupComponent::Parent)
        {
            AbsenceDisposition::RequiredNeverCreated | AbsenceDisposition::AcceptRemoval => Ok(()),
            AbsenceDisposition::RetryCreate | AbsenceDisposition::Forbidden => {
                Err(ContainmentErrorCode::EvidenceUnavailable)
            }
        };
    };
    write_leaf(&parent, c"cgroup.kill", b"1\n")?;
    wait_populated_zero(&parent, Duration::from_secs(5))?;
    recover_and_remove_leaf(
        &parent,
        c"payload",
        evidence.payload,
        evidence
            .lifecycle
            .same_boot_absence(CgroupComponent::Payload),
    )?;
    recover_and_remove_leaf(
        &parent,
        c"control",
        evidence.control,
        evidence
            .lifecycle
            .same_boot_absence(CgroupComponent::Control),
    )?;
    remove_cgroup_child(domain, &parent_name, parent.identity)
}

#[cfg(target_os = "linux")]
fn recover_and_remove_leaf(
    parent: &RetainedCgroupNode,
    name: &CStr,
    expected: Option<DurableDirectoryIdentity>,
    absence: AbsenceDisposition,
) -> ContainmentResult<()> {
    let membership = join_membership(&parent.membership, name.to_bytes())?;
    let child = reopen_cgroup_child(parent, name, membership, expected)?;
    match child {
        Some(child) => {
            wait_populated_zero(&child, Duration::from_secs(5))?;
            remove_cgroup_child(parent, name, child.identity)
        }
        None if matches!(
            absence,
            AbsenceDisposition::RequiredNeverCreated | AbsenceDisposition::AcceptRemoval
        ) =>
        {
            Ok(())
        }
        None => Err(ContainmentErrorCode::EvidenceUnavailable),
    }
}

#[cfg(target_os = "linux")]
fn reopen_cgroup_child(
    parent: &RetainedCgroupNode,
    name: &CStr,
    membership: String,
    expected: Option<DurableDirectoryIdentity>,
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
    if expected != Some(observed) {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    revalidate_cgroup_node(parent)?;
    Ok(Some(RetainedCgroupNode {
        file,
        identity: observed,
        membership,
    }))
}

#[cfg(target_os = "linux")]
fn wait_populated_zero(node: &RetainedCgroupNode, timeout: Duration) -> ContainmentResult<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = read_leaf(node, c"cgroup.events", 4096)?;
        let populated = parse_populated(&events)?;
        if populated == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ContainmentErrorCode::Timeout);
        }
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
    let child_node = RetainedCgroupNode {
        file: child,
        identity: expected,
        membership: String::new(),
    };
    wait_populated_zero(&child_node, Duration::from_secs(5))?;
    failpoint("before_cgroup_remove")?;
    if unsafe { libc::unlinkat(parent.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(ContainmentErrorCode::TopologyFailure);
    }
    failpoint("after_cgroup_remove")?;
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
    waiter: Option<ManagedWaiter>,
    sync_read: File,
    control: PrivateRunnerControl,
    payload_membership: ManagedCgroupMembership,
    payload_proof: Option<ManagedDescendantProof>,
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
) -> ContainmentResult<CandidateContainment> {
    let candidate = topology
        .candidates
        .get(usize::from(candidate_index))
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    launch_runner_with_argv(
        context,
        candidate,
        candidate_index,
        &context.argv,
        ManagedOwnerRole::Runner,
    )
}

#[cfg(target_os = "linux")]
fn launch_runner_with_argv(
    context: &LiveTournamentContext,
    candidate: &CandidateTopology,
    candidate_index: u8,
    candidate_argv: &[Vec<u8>],
    owner_role: ManagedOwnerRole,
) -> ContainmentResult<CandidateContainment> {
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
    failpoint("after_managed_launch")?;
    let listener = private_control
        .listener
        .as_ref()
        .ok_or(ContainmentErrorCode::PeerRejected)?;
    let peer =
        accept_authenticated_peer(listener, &managed.controller, &control_membership, identity)?;
    std::fs::remove_file(&private_control.socket_path)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    private_control.listener.take();
    let mut validator =
        SequenceValidator::new(identity).map_err(|_| ContainmentErrorCode::PeerRejected)?;
    let hello = receive_frame_packet(&peer).map_err(|_| ContainmentErrorCode::PeerRejected)?;
    validator
        .accept(&hello)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    if !matches!(hello.message, ControlMessage::Hello) {
        return Err(ContainmentErrorCode::PeerRejected);
    }
    send_validated_frame(&peer, &mut validator, ControlMessage::HelloAck, identity)?;
    for frame in argv_frames(identity, validator.next_sequence(), candidate_argv)
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?
    {
        validator
            .accept(&frame)
            .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
        send_frame_packet(&peer, &frame).map_err(|_| ContainmentErrorCode::PeerRejected)?;
    }
    let ready = receive_validated_frame(&peer, &mut validator)?;
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
        waiter: Some(managed.waiter),
        sync_read,
        control: private_control,
        payload_membership,
        payload_proof: None,
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn run_fixed_probe(
    context: &LiveTournamentContext,
    candidate_index: u8,
    topology: &CandidateTopology,
) -> ContainmentResult<ProbeEvidence> {
    if topology.candidate_index != candidate_index {
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
    )?;
    transfer_payload_fd(&mut containment, topology)?;
    send_go(&mut containment, topology)?;
    send_payload_release(&mut containment)?;
    let leader = receive_leader_exited(&mut containment)?;
    kill_payload_and_prove_empty(topology)?;
    let drained = receive_output_drained(&mut containment)?;
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
    send_select_or_abort(&mut containment, DecisionKind::Abort)?;
    wait_runner_evidence(containment)?;
    let parent = topology
        .parent
        .as_ref()
        .ok_or(ContainmentErrorCode::TopologyFailure)?;
    wait_populated_zero(parent, Duration::from_secs(5))?;
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
) -> ContainmentResult<ProbeEvidence> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_runner(
    _context: &LiveTournamentContext,
    _topology: &TournamentTopology,
    _candidate_index: u8,
) -> ContainmentResult<CandidateContainment> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn transfer_payload_fd(
    containment: &mut CandidateContainment,
    topology: &CandidateTopology,
) -> ContainmentResult<ContainmentEvent> {
    if topology.candidate_index != containment.identity.candidate_index {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let payload = topology.payload()?;
    let placement_fd = open_cgroup_procs(payload)?;
    let placement = placement_descriptor_evidence(&placement_fd, containment.identity)?;
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
    send_frame_with_one_fd(&containment.peer, &frame, &placement_fd)
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
    failpoint("after_payload_fd_send")?;
    let ack = receive_validated_frame(&containment.peer, &mut containment.validator)?;
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
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_go(
    containment: &mut CandidateContainment,
    topology: &CandidateTopology,
) -> ContainmentResult<Vec<ContainmentEvent>> {
    send_validated_frame(
        &containment.peer,
        &mut containment.validator,
        ControlMessage::Go,
        containment.identity,
    )?;
    let received = receive_validated_frame(&containment.peer, &mut containment.validator)?;
    let ControlMessage::GoReceived { elapsed_ns } = received.message else {
        return Err(ContainmentErrorCode::PeerRejected);
    };
    let placed = receive_validated_frame(&containment.peer, &mut containment.validator)?;
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
    if proof.membership() != &containment.payload_membership {
        return Err(ContainmentErrorCode::PlacementUnproven);
    }
    containment.payload_proof = Some(proof);
    Ok(vec![
        ContainmentEvent::GoReceived {
            candidate: containment.identity.candidate_index,
            elapsed_ns,
        },
        ContainmentEvent::PayloadPlaced {
            candidate: containment.identity.candidate_index,
        },
    ])
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn send_go(
    _containment: &mut CandidateContainment,
    _topology: &CandidateTopology,
) -> ContainmentResult<Vec<ContainmentEvent>> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_payload_release(
    containment: &mut CandidateContainment,
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
    )
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn send_payload_release(
    _containment: &mut CandidateContainment,
) -> ContainmentResult<()> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_candidate_completion(
    containment: &mut CandidateContainment,
) -> ContainmentResult<[ContainmentEvent; 2]> {
    let leader = receive_leader_exited(containment)?;
    let drained = receive_output_drained(containment)?;
    Ok([leader, drained])
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_candidate_completion(
    _containment: &mut CandidateContainment,
) -> ContainmentResult<[ContainmentEvent; 2]> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_leader_exited(
    containment: &mut CandidateContainment,
) -> ContainmentResult<ContainmentEvent> {
    let leader = receive_validated_frame(&containment.peer, &mut containment.validator)?;
    let ControlMessage::LeaderExited { category } = leader.message else {
        return Err(ContainmentErrorCode::PeerRejected);
    };
    Ok(ContainmentEvent::LeaderExited {
        candidate: containment.identity.candidate_index,
        category,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_leader_exited(
    _containment: &mut CandidateContainment,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn receive_output_drained(
    containment: &mut CandidateContainment,
) -> ContainmentResult<ContainmentEvent> {
    let drained = receive_validated_frame(&containment.peer, &mut containment.validator)?;
    let ControlMessage::OutputDrained { bytes } = drained.message else {
        return Err(ContainmentErrorCode::PeerRejected);
    };
    if bytes > crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES {
        return Err(ContainmentErrorCode::OutputLimit);
    }
    Ok(ContainmentEvent::OutputDrained {
        candidate: containment.identity.candidate_index,
        bytes,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn receive_output_drained(
    _containment: &mut CandidateContainment,
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn send_select_or_abort(
    containment: &mut CandidateContainment,
    decision: DecisionKind,
) -> ContainmentResult<ContainmentEvent> {
    send_validated_frame(
        &containment.peer,
        &mut containment.validator,
        ControlMessage::ResultAccepted,
        containment.identity,
    )?;
    send_validated_frame(
        &containment.peer,
        &mut containment.validator,
        ControlMessage::Decision { decision },
        containment.identity,
    )?;
    let ack = receive_validated_frame(&containment.peer, &mut containment.validator)?;
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
) -> ContainmentResult<ContainmentEvent> {
    Err(ContainmentErrorCode::Unsupported)
}

#[cfg(target_os = "linux")]
pub(crate) fn wait_runner_evidence(
    mut containment: CandidateContainment,
) -> ContainmentResult<[ContainmentEvent; 2]> {
    let mut byte = [0_u8; 1];
    if containment
        .sync_read
        .read(&mut byte)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        != 0
    {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let waiter = containment
        .waiter
        .take()
        .ok_or(ContainmentErrorCode::EvidenceUnavailable)?;
    waiter
        .wait()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    Ok([
        ContainmentEvent::SyncEof {
            candidate: containment.identity.candidate_index,
        },
        ContainmentEvent::ManagedReaped {
            candidate: containment.identity.candidate_index,
        },
    ])
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn wait_runner_evidence(
    _containment: CandidateContainment,
) -> ContainmentResult<[ContainmentEvent; 2]> {
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
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&context.current_executable)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let source_metadata = source
        .metadata()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if !source_metadata.is_file()
        || source_metadata.mode() & 0o111 == 0
        || source_metadata.mode() & 0o022 != 0
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let runner_path = path.join("lterm");
    let mut runner = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o500)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&runner_path)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    std::io::copy(&mut source, &mut runner)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    runner
        .sync_all()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let runner_metadata = runner
        .metadata()
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if !runner_metadata.is_file()
        || runner_metadata.mode() & 0o7777 != 0o500
        || runner_metadata.nlink() != 1
    {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
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
) -> ContainmentResult<File> {
    failpoint("before_control_accept")?;
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
    set_socket_timeout(&peer, Duration::from_secs(5))?;
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
    identity: RunnerIdentity,
) -> ContainmentResult<PlacementDescriptorEvidence> {
    const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
    let metadata = file
        .metadata()
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
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
    let evidence = PlacementDescriptorEvidence {
        dev: metadata.dev(),
        ino: metadata.ino(),
        statx_mnt_id_unique: statx.stx_mnt_id,
        candidate_index: identity.candidate_index,
        generation: identity.generation,
    };
    evidence
        .validate(identity)
        .map_err(|_| ContainmentErrorCode::DescriptorViolation)?;
    Ok(evidence)
}

#[cfg(target_os = "linux")]
fn send_validated_frame(
    peer: &File,
    validator: &mut SequenceValidator,
    message: ControlMessage,
    identity: RunnerIdentity,
) -> ContainmentResult<()> {
    let frame = ControlFrame::new(identity, validator.next_sequence(), message);
    validator
        .accept(&frame)
        .map_err(|_| ContainmentErrorCode::PeerRejected)?;
    send_frame_packet(peer, &frame).map_err(|_| ContainmentErrorCode::PeerRejected)
}

#[cfg(target_os = "linux")]
fn receive_validated_frame(
    peer: &File,
    validator: &mut SequenceValidator,
) -> ContainmentResult<ControlFrame> {
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
        println!("speculation-real-cases=1");
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
    let source = fixture.join("source");
    let candidates = [fixture.join("candidate-0"), fixture.join("candidate-1")];
    let ledger = fixture.join("ledger");
    let control = fixture.join("control");
    for path in [&source, &candidates[0], &candidates[1]] {
        std::fs::create_dir(path).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    for path in [&ledger, &control] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    let context = validate_prepare(PrepareInputs {
        tournament_uuid: Uuid::new_v4(),
        generation: 1,
        source,
        candidates,
        ledger_root: ledger,
        cgroup_root,
        control_root: control,
        argv: vec![OsString::from("/usr/bin/true")],
    })?;
    let mut tournament = begin_topology(&context)?;
    create_topology(&mut tournament, TopologyAction::CreateTournamentDomain)?;
    for candidate in 0..2 {
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
    for candidate in 0..2 {
        let candidate_topology = &tournament.candidates[usize::from(candidate)];
        let probe = run_fixed_probe(&context, candidate, candidate_topology)?;
        if !probe.exited_zero || !probe.parent_populated_zero || probe.output_bytes != 0 {
            return Err(ContainmentErrorCode::TerminalBoundaryFailure);
        }
        let mut containment = launch_runner(&context, &tournament, candidate)?;
        transfer_payload_fd(&mut containment, candidate_topology)?;
        send_go(&mut containment, candidate_topology)?;
        send_payload_release(&mut containment)?;
        receive_leader_exited(&mut containment)?;
        kill_payload_and_prove_empty(candidate_topology)?;
        receive_output_drained(&mut containment)?;
        send_select_or_abort(&mut containment, DecisionKind::Abort)?;
        wait_runner_evidence(containment)?;
        cleanup_parent_and_remove(&tournament, candidate_topology)?;
    }
    remove_tournament_domain(&tournament)
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

//! Durable, Linux-only managed root-process launch substrate.
//!
//! This module is intentionally disconnected from ordinary sessions and the
//! public protocol.  A future feature may call `launch_managed_process`; until
//! then only the hidden gate dispatcher is wired into the binary.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::RawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
#[cfg(target_os = "linux")]
use std::os::unix::fs::FileExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SLOT_SCHEMA_VERSION: u32 = 2;
const GATE_SCHEMA_VERSION: u32 = 1;
const SLOT_SCHEMA_MARKER: &str = "slot-schema-v2";
const SLOT_MIGRATION_STAGING_PREFIX: &str = ".slots-v2-create-";
const SLOT_MIGRATION_BUILD_PREFIX: &str = ".slots-v2-build-";
const SLOT_MIGRATION_GC_PREFIX: &str = ".slots-v2-gc-";
const SLOT_COUNT: usize = 1_024;
const MAX_RECORD_BYTES: usize = 8 * 1_024;
const TOMBSTONE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const INTERNAL_GATE_ARG: &str = "__lterm-internal-managed-launch-gate-v1";
#[cfg(all(target_os = "linux", not(test)))]
static MANAGED_PROCESS_REGISTRY_PATH: OnceLock<PathBuf> = OnceLock::new();
#[cfg(debug_assertions)]
const INTERNAL_TEST_LAUNCH_ARG: &str = "__lterm-internal-managed-launch-test-v1";
#[cfg(debug_assertions)]
const INTERNAL_TEST_RECONCILE_ARG: &str = "__lterm-internal-managed-reconcile-test-v1";
#[cfg(target_os = "linux")]
const GATE_CONTROL_FD: RawFd = 3;
#[cfg(target_os = "linux")]
const GATE_GUARD_FD: RawFd = 4;
#[cfg(target_os = "linux")]
const GATE_PLACEMENT_FD: RawFd = 5;
pub(crate) const MANAGED_SYNC_PIPE_TARGET_FD: RawFd = 10;
pub(crate) const MANAGED_PINNED_RUNNER_TARGET_FD: RawFd = 11;
pub(crate) const MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD: RawFd = 12;
pub(crate) const MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD: RawFd = 13;
pub(crate) const MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD: RawFd = 14;
#[cfg(target_os = "linux")]
const MAX_COMMIT_FDS: usize = 8;
#[cfg(all(debug_assertions, target_os = "linux"))]
static FIRST_SPECULATION_CLEANUP_UNKNOWN_INJECTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(debug_assertions, target_os = "linux"))]
static MANAGED_RETURN_ERROR_ONCE_INJECTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(debug_assertions, target_os = "linux"))]
static FORCE_REAPER_SPAWN_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(debug_assertions, target_os = "linux"))]
static FORCE_REAPER_WAIT_ERROR: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(debug_assertions, target_os = "linux"))]
static FORCE_PENDING_REAP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(debug_assertions, target_os = "linux"))]
static FORCE_CLEANUP_ERROR: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(all(debug_assertions, target_os = "linux"))]
static FORCE_JOB_STUCK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(all(debug_assertions, target_os = "linux"))]
struct ManagedProcessEnvSeams {
    initialized_on: std::thread::ThreadId,
    failpoint: Option<std::ffi::OsString>,
    return_error: Option<std::ffi::OsString>,
    return_error_once: bool,
}

#[cfg(all(debug_assertions, target_os = "linux"))]
impl Default for ManagedProcessEnvSeams {
    fn default() -> Self {
        Self {
            initialized_on: std::thread::current().id(),
            failpoint: None,
            return_error: None,
            return_error_once: false,
        }
    }
}
#[cfg(all(debug_assertions, target_os = "linux"))]
static MANAGED_PROCESS_ENV_SEAMS: OnceLock<ManagedProcessEnvSeams> = OnceLock::new();
#[cfg(all(debug_assertions, target_os = "linux"))]
std::thread_local! {
    static MANAGED_LOCAL_RETURN_ERROR: std::cell::RefCell<Option<(&'static str, bool, bool)>> = const { std::cell::RefCell::new(None) };
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "evidence", content = "value")]
pub(crate) enum Evidence<T> {
    Present(T),
    Absent,
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessIdentity {
    pub boot_uuid: Uuid,
    pub pid_namespace_inode: u64,
    pub pid: u32,
    pub start_ticks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagedOwnerKind {
    Speculation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagedOwnerRole {
    Probe,
    Runner,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedOwnerTag {
    pub kind: ManagedOwnerKind,
    pub tournament_uuid: Uuid,
    pub candidate_index: u8,
    pub role: ManagedOwnerRole,
}

impl ManagedOwnerTag {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.kind == ManagedOwnerKind::Speculation,
            "unsupported managed owner kind"
        );
        ensure!(
            !self.tournament_uuid.is_nil(),
            "managed owner tournament UUID must not be nil"
        );
        ensure!(
            self.candidate_index < 2,
            "managed owner candidate index must be 0 or 1"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
enum SlotState {
    Vacant,
    IntentDurable {
        nonce: Uuid,
        created_unix_secs: u64,
    },
    IdentityDurable {
        nonce: Uuid,
        identity: ProcessIdentity,
        release_may_have_occurred: bool,
    },
    CleanupPending {
        nonce: Uuid,
        identity: ProcessIdentity,
        release_may_have_occurred: bool,
    },
    ResolvedTombstone {
        nonce: Uuid,
        identity: Option<ProcessIdentity>,
        resolved_unix_secs: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SlotRecord {
    schema_version: u32,
    slot: u16,
    generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<ManagedOwnerTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact_binding: Option<Box<ManagedArtifactBinding>>,
    #[serde(flatten)]
    state: SlotState,
}

/// Exact on-disk shape used before authoritative private-artifact bindings
/// were introduced.  Keeping the legacy decoder explicit prevents a new
/// binary from silently interpreting unknown v1 fields, while the schema bump
/// makes an old binary reject every v2 slot before it can allocate or clean.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SlotRecordV1 {
    schema_version: u32,
    slot: u16,
    generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<ManagedOwnerTag>,
    #[serde(flatten)]
    state: SlotState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GateRegistration {
    schema_version: u32,
    slot: u16,
    generation: u64,
    nonce: Uuid,
    identity: ProcessIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RegistrationRecord {
    schema_version: u32,
    slot: u16,
    generation: u64,
    registration: Option<GateRegistration>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GateHello {
    protocol: String,
    registration: GateRegistration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateCommit {
    protocol: String,
    slot: u16,
    generation: u64,
    nonce: Uuid,
    identity: ProcessIdentity,
    descriptors: Vec<CommitDescriptor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CommitDescriptorRole {
    TargetExecutable,
    SyncPipeWrite,
    PinnedRunner,
    PinnedCandidateDirectory,
    PinnedControlDirectory,
    PinnedControlSocket,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitDescriptor {
    role: CommitDescriptorRole,
    target_fd: Option<RawFd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory_identity: Option<ManagedDirectoryIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GateExecFailure {
    protocol: String,
    errno: Option<i32>,
    message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManagedReconcileCode {
    InvalidIntentState,
    BusyGuardIdentityAbsent,
    RegistrationUnavailable,
    ProcessStillLive,
    ProcessEvidenceUnavailable,
    ReconciliationFailed,
    UnknownSlot,
    DuplicateOwner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReconcileOutcome {
    Absent,
    Live,
    UnknownOrphanRisk(ManagedReconcileCode),
    ResolvedTombstone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ManagedKey {
    slot: u16,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedDirectoryIdentity {
    pub boot_uuid: Uuid,
    pub dev: u64,
    pub ino: u64,
    pub statx_mnt_id_unique: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedArtifactIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl ManagedArtifactIdentity {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.dev != 0 && self.ino != 0,
            "managed artifact identity is zero"
        );
        Ok(())
    }
}

impl ManagedDirectoryIdentity {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.boot_uuid.is_nil(),
            "managed directory boot UUID is nil"
        );
        ensure!(
            self.dev != 0 && self.ino != 0,
            "managed directory identity is zero"
        );
        ensure!(
            self.statx_mnt_id_unique != 0,
            "managed directory mount identity is zero"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedArtifactBinding {
    nonce: Uuid,
    control_root: ManagedDirectoryIdentity,
    private_leaf: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    private_directory: Option<ManagedDirectoryIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runner: Option<ManagedArtifactIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    socket: Option<ManagedArtifactIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<ManagedArtifactIdentity>,
    #[serde(default, skip_serializing_if = "is_false")]
    runner_create_pending: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    socket_create_pending: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    owner_create_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup_quarantine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup_unlink_pending: Option<ManagedArtifactCleanupStep>,
    #[serde(default, skip_serializing_if = "is_false")]
    socket_retire_pending: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    socket_retired: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    cleanup_ownership_removed: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    cleanup_socket_removed: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    cleanup_runner_removed: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    cleanup_completed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagedArtifactCleanupStep {
    Ownership,
    Socket,
    Runner,
    Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedArtifactCreationPhase {
    Reserved,
    DirectoryCreated,
    RunnerCreatePending,
    RunnerCreated,
    SocketCreatePending,
    SocketCreated,
    OwnerCreatePending,
    OwnerCreated,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ManagedArtifactBinding {
    pub(crate) fn nonce(&self) -> Uuid {
        self.nonce
    }

    pub(crate) fn control_root(&self) -> ManagedDirectoryIdentity {
        self.control_root
    }

    pub(crate) fn private_leaf(&self) -> &str {
        &self.private_leaf
    }

    pub(crate) fn private_directory(&self) -> Option<ManagedDirectoryIdentity> {
        self.private_directory
    }

    pub(crate) fn runner(&self) -> Option<ManagedArtifactIdentity> {
        self.runner
    }

    pub(crate) fn socket(&self) -> Option<ManagedArtifactIdentity> {
        self.socket
    }

    pub(crate) fn owner_file(&self) -> Option<ManagedArtifactIdentity> {
        self.owner
    }

    pub(crate) fn runner_create_pending(&self) -> bool {
        self.runner_create_pending
    }

    pub(crate) fn socket_create_pending(&self) -> bool {
        self.socket_create_pending
    }

    pub(crate) fn owner_create_pending(&self) -> bool {
        self.owner_create_pending
    }

    pub(crate) fn creation_pending(&self) -> bool {
        self.runner_create_pending || self.socket_create_pending || self.owner_create_pending
    }

    pub(crate) fn cleanup_quarantine(&self) -> Option<&str> {
        self.cleanup_quarantine.as_deref()
    }

    pub(crate) fn cleanup_completed(&self) -> bool {
        self.cleanup_completed
    }

    pub(crate) fn cleanup_step_completed(&self, step: ManagedArtifactCleanupStep) -> bool {
        match step {
            ManagedArtifactCleanupStep::Ownership => self.cleanup_ownership_removed,
            ManagedArtifactCleanupStep::Socket => self.cleanup_socket_removed,
            ManagedArtifactCleanupStep::Runner => self.cleanup_runner_removed,
            ManagedArtifactCleanupStep::Directory => self.cleanup_completed,
        }
    }

    pub(crate) fn cleanup_unlink_pending(&self) -> Option<ManagedArtifactCleanupStep> {
        self.cleanup_unlink_pending
    }

    pub(crate) fn socket_retire_pending(&self) -> bool {
        self.socket_retire_pending
    }

    pub(crate) fn socket_retired(&self) -> bool {
        self.socket_retired
    }

    fn same_artifact(&self, other: &Self) -> bool {
        self.nonce == other.nonce
            && self.control_root == other.control_root
            && self.private_leaf == other.private_leaf
            && self.private_directory == other.private_directory
            && self.runner == other.runner
            && self.socket == other.socket
            && self.owner == other.owner
            && self.runner_create_pending == other.runner_create_pending
            && self.socket_create_pending == other.socket_create_pending
            && self.owner_create_pending == other.owner_create_pending
    }

    fn creation_phase(&self) -> Option<ManagedArtifactCreationPhase> {
        match (
            self.private_directory.is_some(),
            self.runner.is_some(),
            self.socket.is_some(),
            self.owner.is_some(),
            self.runner_create_pending,
            self.socket_create_pending,
            self.owner_create_pending,
        ) {
            (false, false, false, false, false, false, false) => {
                Some(ManagedArtifactCreationPhase::Reserved)
            }
            (true, false, false, false, false, false, false) => {
                Some(ManagedArtifactCreationPhase::DirectoryCreated)
            }
            (true, false, false, false, true, false, false) => {
                Some(ManagedArtifactCreationPhase::RunnerCreatePending)
            }
            (true, true, false, false, false, false, false) => {
                Some(ManagedArtifactCreationPhase::RunnerCreated)
            }
            (true, true, false, false, false, true, false) => {
                Some(ManagedArtifactCreationPhase::SocketCreatePending)
            }
            (true, true, true, false, false, false, false) => {
                Some(ManagedArtifactCreationPhase::SocketCreated)
            }
            (true, true, true, false, false, false, true) => {
                Some(ManagedArtifactCreationPhase::OwnerCreatePending)
            }
            (true, true, true, true, false, false, false) => {
                Some(ManagedArtifactCreationPhase::OwnerCreated)
            }
            _ => None,
        }
    }

    fn creation_progresses_to(&self, next: &Self) -> bool {
        use ManagedArtifactCreationPhase as Phase;
        match (self.creation_phase(), next.creation_phase()) {
            (Some(Phase::Reserved), Some(Phase::DirectoryCreated)) => true,
            (Some(Phase::DirectoryCreated), Some(Phase::RunnerCreatePending)) => {
                self.private_directory == next.private_directory
            }
            (Some(Phase::RunnerCreatePending), Some(Phase::RunnerCreated)) => {
                self.private_directory == next.private_directory
            }
            (Some(Phase::RunnerCreated), Some(Phase::SocketCreatePending)) => {
                self.private_directory == next.private_directory && self.runner == next.runner
            }
            (Some(Phase::SocketCreatePending), Some(Phase::SocketCreated)) => {
                self.private_directory == next.private_directory && self.runner == next.runner
            }
            (Some(Phase::SocketCreated), Some(Phase::OwnerCreatePending)) => {
                self.private_directory == next.private_directory
                    && self.runner == next.runner
                    && self.socket == next.socket
            }
            (Some(Phase::OwnerCreatePending), Some(Phase::OwnerCreated)) => {
                self.private_directory == next.private_directory
                    && self.runner == next.runner
                    && self.socket == next.socket
            }
            _ => false,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.nonce.is_nil(), "managed artifact nonce is nil");
        self.control_root.validate()?;
        validate_managed_artifact_leaf(&self.private_leaf)?;
        if let Some(identity) = self.private_directory {
            identity.validate()?;
        }
        ensure!(
            self.socket.is_none() || self.runner.is_some(),
            "managed socket identity exists before its runner"
        );
        ensure!(
            self.runner.is_none() || self.private_directory.is_some(),
            "managed artifacts exist before their directory"
        );
        if let Some(identity) = self.runner {
            identity.validate()?;
        }
        if let Some(identity) = self.socket {
            identity.validate()?;
        }
        if let Some(identity) = self.owner {
            identity.validate()?;
            ensure!(
                self.socket.is_some(),
                "managed owner identity exists before its socket"
            );
        }
        ensure!(
            self.creation_phase().is_some(),
            "managed artifact creation phase is invalid"
        );
        if let Some(quarantine) = &self.cleanup_quarantine {
            validate_managed_artifact_leaf(quarantine)?;
            ensure!(
                quarantine != &self.private_leaf,
                "managed artifact quarantine aliases its live leaf"
            );
        }
        ensure!(
            !self.cleanup_completed || self.cleanup_quarantine.is_some(),
            "managed artifact cleanup completion lacks quarantine phase"
        );
        ensure!(
            !self.socket_retire_pending || (self.socket.is_some() && !self.socket_retired),
            "managed socket retirement intent is invalid"
        );
        ensure!(
            !self.socket_retired || (self.socket.is_some() && !self.socket_retire_pending),
            "managed socket retirement receipt is invalid"
        );
        ensure!(
            !self.cleanup_socket_removed || self.cleanup_ownership_removed,
            "managed socket cleanup precedes ownership cleanup"
        );
        ensure!(
            !self.cleanup_runner_removed || self.cleanup_socket_removed,
            "managed runner cleanup precedes socket cleanup"
        );
        ensure!(
            !self.cleanup_completed || self.cleanup_runner_removed,
            "managed directory cleanup precedes runner cleanup"
        );
        if let Some(step) = self.cleanup_unlink_pending {
            ensure!(
                self.cleanup_quarantine.is_some() && !self.cleanup_step_completed(step),
                "managed artifact unlink intent is outside its cleanup step"
            );
            ensure!(
                match step {
                    ManagedArtifactCleanupStep::Ownership => {
                        !self.cleanup_ownership_removed
                            && !self.cleanup_socket_removed
                            && !self.cleanup_runner_removed
                    }
                    ManagedArtifactCleanupStep::Socket => {
                        self.cleanup_ownership_removed
                            && !self.cleanup_socket_removed
                            && !self.cleanup_runner_removed
                    }
                    ManagedArtifactCleanupStep::Runner => {
                        self.cleanup_ownership_removed
                            && self.cleanup_socket_removed
                            && !self.cleanup_runner_removed
                    }
                    ManagedArtifactCleanupStep::Directory => {
                        self.cleanup_ownership_removed
                            && self.cleanup_socket_removed
                            && self.cleanup_runner_removed
                            && !self.cleanup_completed
                    }
                },
                "managed artifact unlink intent is out of order"
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn test_value(
        nonce: Uuid,
        control_root: ManagedDirectoryIdentity,
        private_leaf: &str,
        private_directory: Option<ManagedDirectoryIdentity>,
    ) -> Self {
        Self {
            nonce,
            control_root,
            private_leaf: private_leaf.to_owned(),
            private_directory,
            runner: None,
            socket: None,
            owner: None,
            runner_create_pending: false,
            socket_create_pending: false,
            owner_create_pending: false,
            cleanup_quarantine: None,
            cleanup_unlink_pending: None,
            socket_retire_pending: false,
            socket_retired: false,
            cleanup_ownership_removed: false,
            cleanup_socket_removed: false,
            cleanup_runner_removed: false,
            cleanup_completed: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_with_files(
        mut self,
        runner: ManagedArtifactIdentity,
        socket: ManagedArtifactIdentity,
    ) -> Self {
        self.runner = Some(runner);
        self.socket = Some(socket);
        self
    }

    #[cfg(test)]
    pub(crate) fn test_with_owner(mut self, owner: ManagedArtifactIdentity) -> Self {
        self.owner = Some(owner);
        self.owner_create_pending = false;
        self
    }

    #[cfg(test)]
    pub(crate) fn test_with_owner_create_pending(mut self) -> Self {
        self.owner_create_pending = true;
        self
    }
}

fn validate_managed_artifact_leaf(leaf: &str) -> Result<()> {
    ensure!(
        !leaf.is_empty()
            && leaf.len() <= 255
            && leaf != "."
            && leaf != ".."
            && !leaf.as_bytes().contains(&b'/')
            && !leaf.as_bytes().contains(&0),
        "managed artifact leaf is invalid"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedReconcileEntry {
    pub key: Option<ManagedKey>,
    pub owner: Option<ManagedOwnerTag>,
    pub artifact_binding: Option<ManagedArtifactBinding>,
    pub outcome: ReconcileOutcome,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ManagedReconcileReport {
    pub entries: Vec<ManagedReconcileEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManagedOwnerCorrelation {
    Absent,
    Matched {
        key: ManagedKey,
        outcome: ReconcileOutcome,
    },
    Unresolved,
}

impl ManagedReconcileReport {
    pub(crate) fn correlate_owner(&self, owner: &ManagedOwnerTag) -> ManagedOwnerCorrelation {
        if owner.validate().is_err() {
            return ManagedOwnerCorrelation::Unresolved;
        }
        let mut matched = None;
        for entry in &self.entries {
            let Some(entry_owner) = &entry.owner else {
                return ManagedOwnerCorrelation::Unresolved;
            };
            if entry_owner != owner {
                continue;
            }
            let Some(key) = entry.key else {
                return ManagedOwnerCorrelation::Unresolved;
            };
            if matched.is_some() {
                return ManagedOwnerCorrelation::Unresolved;
            }
            matched = Some(ManagedOwnerCorrelation::Matched {
                key,
                outcome: entry.outcome.clone(),
            });
        }
        matched.unwrap_or(ManagedOwnerCorrelation::Absent)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManagedOwnerOutcome {
    Absent,
    ResolvedTombstone(ManagedKey),
    UnknownOrphanRisk(ManagedReconcileCode),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedOwnerReceipt {
    owner: ManagedOwnerTag,
    key: ManagedKey,
}

impl ManagedOwnerReceipt {
    pub(crate) fn new(owner: ManagedOwnerTag, key: ManagedKey) -> Result<Self> {
        owner.validate()?;
        ensure!(
            key.generation != 0,
            "managed owner receipt generation is zero"
        );
        Ok(Self { owner, key })
    }

    pub(crate) fn owner(&self) -> &ManagedOwnerTag {
        &self.owner
    }

    pub(crate) fn slot(&self) -> u16 {
        self.key.slot
    }

    pub(crate) fn generation(&self) -> u64 {
        self.key.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedExecutablePolicy {
    /// ADR-0004 compatibility for ordinary owner-less managed launches.
    Legacy,
    /// Exact retained `/usr/bin/bwrap` object required by speculation.
    PinnedSystemBwrap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ManagedCgroupDirectoryIdentity {
    pub boot_uuid: Uuid,
    pub dev: u64,
    pub ino: u64,
    pub statx_mnt_id_unique: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedCgroupMembership {
    normalized_path: String,
    leaf_identity: ManagedCgroupDirectoryIdentity,
    candidate_index: u8,
    generation: u64,
}

impl ManagedCgroupMembership {
    pub(crate) fn new(
        normalized_path: String,
        leaf_identity: ManagedCgroupDirectoryIdentity,
        candidate_index: u8,
        generation: u64,
    ) -> Result<Self> {
        validate_cgroup_membership(&normalized_path)?;
        ensure!(candidate_index < 2, "candidate index is out of range");
        ensure!(generation != 0, "cgroup membership generation is zero");
        ensure!(
            !leaf_identity.boot_uuid.is_nil()
                && leaf_identity.dev != 0
                && leaf_identity.ino != 0
                && leaf_identity.statx_mnt_id_unique != 0,
            "cgroup leaf identity is incomplete"
        );
        Ok(Self {
            normalized_path,
            leaf_identity,
            candidate_index,
            generation,
        })
    }

    pub(crate) fn normalized_path(&self) -> &str {
        &self.normalized_path
    }

    pub(crate) fn candidate_index(&self) -> u8 {
        self.candidate_index
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedEvidenceCode {
    ProcessAbsent,
    IdentityUnavailable,
    NotDescendant,
    MembershipMismatch,
    InvalidEvidence,
}

impl std::fmt::Display for ManagedEvidenceCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ProcessAbsent => "managed_process_absent",
            Self::IdentityUnavailable => "managed_identity_unavailable",
            Self::NotDescendant => "managed_not_descendant",
            Self::MembershipMismatch => "managed_membership_mismatch",
            Self::InvalidEvidence => "managed_invalid_evidence",
        })
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct ManagedDescendantProof {
    _pidfd: PidFd,
    _identity: ProcessIdentity,
    membership: ManagedCgroupMembership,
}

#[cfg(target_os = "linux")]
impl ManagedDescendantProof {
    pub(crate) fn membership(&self) -> &ManagedCgroupMembership {
        &self.membership
    }
}

#[derive(Debug)]
pub(crate) struct ControlCgroupPlacement {
    cgroup_procs: File,
    _control_leaf: File,
    expected_membership: ManagedCgroupMembership,
}

impl ControlCgroupPlacement {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(
        cgroup_procs: File,
        control_leaf: File,
        expected_membership: ManagedCgroupMembership,
    ) -> Result<Self> {
        validate_cgroup_procs_fd(&cgroup_procs)?;
        validate_cgroup_leaf_fd(&control_leaf, expected_membership.leaf_identity)?;
        set_cloexec(cgroup_procs.as_raw_fd())?;
        set_cloexec(control_leaf.as_raw_fd())?;
        Ok(Self {
            cgroup_procs,
            _control_leaf: control_leaf,
            expected_membership,
        })
    }

    #[cfg(all(target_os = "linux", debug_assertions))]
    fn new_internal_test(cgroup_procs: File, expected_membership: String) -> Result<Self> {
        let control_leaf = open_control_leaf_for_internal_test(&cgroup_procs)?;
        let identity = observe_cgroup_directory_identity(&control_leaf)?;
        Self::new(
            cgroup_procs,
            control_leaf,
            ManagedCgroupMembership::new(expected_membership, identity, 0, 1)?,
        )
    }
}

#[derive(Debug, Default)]
pub(crate) enum ManagedPlacement {
    #[default]
    None,
    CgroupV2(ControlCgroupPlacement),
}

#[derive(Debug)]
pub(crate) struct SyncPipeWrite {
    file: File,
    target_fd: RawFd,
}

#[derive(Debug)]
pub(crate) struct ManagedPinnedRunner {
    file: File,
    target_fd: RawFd,
}

#[derive(Debug)]
pub(crate) struct ManagedPinnedCandidateDirectory {
    file: File,
    target_fd: RawFd,
    identity: ManagedDirectoryIdentity,
}

#[derive(Debug)]
pub(crate) struct ManagedPinnedControlDirectory {
    file: File,
    target_fd: RawFd,
    identity: ManagedDirectoryIdentity,
}

#[derive(Debug)]
pub(crate) struct ManagedPinnedControlSocket {
    file: File,
    target_fd: RawFd,
    identity: ManagedArtifactIdentity,
}

impl ManagedPinnedRunner {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(file: File) -> Result<Self> {
        validate_managed_pinned_runner_fd(&file)?;
        set_cloexec(file.as_raw_fd())?;
        Ok(Self {
            file,
            target_fd: MANAGED_PINNED_RUNNER_TARGET_FD,
        })
    }
}

impl ManagedPinnedCandidateDirectory {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(file: File, identity: ManagedDirectoryIdentity) -> Result<Self> {
        validate_managed_pinned_directory_fd(
            &file,
            identity,
            ManagedPinnedDirectoryKind::Candidate,
        )?;
        set_cloexec(file.as_raw_fd())?;
        Ok(Self {
            file,
            target_fd: MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD,
            identity,
        })
    }
}

impl ManagedPinnedControlDirectory {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(file: File, identity: ManagedDirectoryIdentity) -> Result<Self> {
        validate_managed_pinned_directory_fd(&file, identity, ManagedPinnedDirectoryKind::Control)?;
        set_cloexec(file.as_raw_fd())?;
        Ok(Self {
            file,
            target_fd: MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD,
            identity,
        })
    }
}

impl ManagedPinnedControlSocket {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(file: File, identity: ManagedArtifactIdentity) -> Result<Self> {
        validate_managed_pinned_control_socket_fd(&file, identity)?;
        set_cloexec(file.as_raw_fd())?;
        Ok(Self {
            file,
            target_fd: MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD,
            identity,
        })
    }
}

impl SyncPipeWrite {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(file: File, target_fd: RawFd) -> Result<Self> {
        ensure!(
            target_fd == MANAGED_SYNC_PIPE_TARGET_FD,
            "sync pipe target FD must be the fixed managed sync FD"
        );
        validate_sync_pipe_write_fd(&file)?;
        set_cloexec(file.as_raw_fd())?;
        Ok(Self { file, target_fd })
    }
}

#[derive(Debug, Default)]
pub(crate) enum ManagedAuxiliary {
    #[default]
    None,
    SyncPipeWrite(SyncPipeWrite),
    Speculation {
        sync_pipe: SyncPipeWrite,
        pinned_runner: ManagedPinnedRunner,
        candidate_directory: ManagedPinnedCandidateDirectory,
        control_directory: ManagedPinnedControlDirectory,
        control_socket: ManagedPinnedControlSocket,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedStdioPolicy {
    Inherit,
    Null,
}

#[derive(Debug)]
pub(crate) struct ManagedLaunchRequest {
    pub owner: Option<ManagedOwnerTag>,
    pub reservation: Option<ManagedLaunchReservation>,
    pub lifetime_guard: Option<ManagedLifetimeGuard>,
    pub executable_policy: ManagedExecutablePolicy,
    pub placement: ManagedPlacement,
    pub auxiliary: ManagedAuxiliary,
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub environment: Vec<(OsString, OsString)>,
    pub stdio: ManagedStdioPolicy,
}

#[derive(Clone)]
pub(crate) struct ManagedLifetimeGuard {
    inner: Arc<dyn Send + Sync>,
    cleanup: Option<Arc<dyn Fn() -> Result<bool> + Send + Sync>>,
}

impl ManagedLifetimeGuard {
    pub(crate) fn new<T>(value: Arc<T>) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self {
            inner: value,
            cleanup: None,
        }
    }

    pub(crate) fn with_cleanup<T, F>(value: Arc<T>, cleanup: F) -> Self
    where
        T: Send + Sync + 'static,
        F: Fn() -> Result<bool> + Send + Sync + 'static,
    {
        Self {
            inner: value,
            cleanup: Some(Arc::new(cleanup)),
        }
    }

    fn cleanup_artifacts(&self) -> Result<bool> {
        self.cleanup.as_ref().map_or(Ok(true), |cleanup| cleanup())
    }
}

impl std::fmt::Debug for ManagedLifetimeGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedLifetimeGuard")
            .field("strong_count", &Arc::strong_count(&self.inner))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ManagedController {
    inner: Arc<ManagedControllerInner>,
}

#[derive(Debug)]
struct ManagedControllerInner {
    key: ManagedKey,
    identity: ProcessIdentity,
    owner: Option<ManagedOwnerTag>,
    registry: Registry,
}

#[derive(Debug)]
pub(crate) struct ManagedWaiter {
    child: Option<std::process::Child>,
    cleanup: ManagedCleanupOwner,
    lifetime_guard: Option<ManagedLifetimeGuard>,
}

#[derive(Clone, Debug)]
enum ManagedCleanupOwner {
    Controller(ManagedController),
    FailedLaunch {
        registry: Registry,
        slot: u16,
        generation: u64,
    },
}

#[derive(Debug)]
pub(crate) struct ManagedLaunch {
    pub controller: ManagedController,
    pub waiter: ManagedWaiter,
    pub owner_receipt: Option<ManagedOwnerReceipt>,
}

pub(crate) struct ManagedLaunchFailure {
    error: anyhow::Error,
    pending: Option<Box<ManagedWaiter>>,
}

impl ManagedLaunchFailure {
    fn resolved(error: anyhow::Error) -> Self {
        Self {
            error,
            pending: None,
        }
    }

    fn pending(error: anyhow::Error, waiter: ManagedWaiter) -> Self {
        Self {
            error,
            pending: Some(Box::new(waiter)),
        }
    }
}

impl std::fmt::Debug for ManagedLaunchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedLaunchFailure")
            .field("error", &self.error)
            .field("pending", &self.pending.is_some())
            .finish()
    }
}

impl std::fmt::Display for ManagedLaunchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for ManagedLaunchFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.root_cause())
    }
}

impl From<anyhow::Error> for ManagedLaunchFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::resolved(error)
    }
}

#[cfg(target_os = "linux")]
pub(crate) enum ManagedBoundedReap {
    Reaped,
    Pending(ManagedWaiter),
}

impl ManagedController {
    pub(crate) fn key(&self) -> ManagedKey {
        self.inner.key
    }

    pub(crate) fn owner(&self) -> Option<&ManagedOwnerTag> {
        self.inner.owner.as_ref()
    }

    pub(crate) fn identity(&self) -> &ProcessIdentity {
        &self.inner.identity
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn prove_descendant_in_cgroup(
        &self,
        observed_host_pid: u32,
        expected: &ManagedCgroupMembership,
    ) -> std::result::Result<ManagedDescendantProof, ManagedEvidenceCode> {
        let identity = match observe_identity(observed_host_pid) {
            Evidence::Present(identity) => identity,
            Evidence::Absent => return Err(ManagedEvidenceCode::ProcessAbsent),
            Evidence::Unavailable(_) => return Err(ManagedEvidenceCode::IdentityUnavailable),
        };
        let pidfd = match open_verified_pidfd(&identity) {
            Evidence::Present(pidfd) => pidfd,
            Evidence::Absent => return Err(ManagedEvidenceCode::ProcessAbsent),
            Evidence::Unavailable(_) => return Err(ManagedEvidenceCode::IdentityUnavailable),
        };
        if !prove_process_ancestry(&identity, &self.inner.identity)? {
            return Err(ManagedEvidenceCode::NotDescendant);
        }
        verify_process_cgroup_membership(observed_host_pid, expected.normalized_path())
            .map_err(|_| ManagedEvidenceCode::MembershipMismatch)?;
        match verify_exact_process(&self.inner.identity) {
            Evidence::Present(_) => {}
            Evidence::Absent => return Err(ManagedEvidenceCode::ProcessAbsent),
            Evidence::Unavailable(_) => return Err(ManagedEvidenceCode::IdentityUnavailable),
        }
        Ok(ManagedDescendantProof {
            _pidfd: pidfd,
            _identity: identity,
            membership: expected.clone(),
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn status(&self) -> ReconcileOutcome {
        match verify_exact_process(&self.inner.identity) {
            Evidence::Present(_) => ReconcileOutcome::Live,
            Evidence::Absent => ReconcileOutcome::Absent,
            Evidence::Unavailable(_) => ReconcileOutcome::UnknownOrphanRisk(
                ManagedReconcileCode::ProcessEvidenceUnavailable,
            ),
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn terminate(&self) -> Result<ReconcileOutcome> {
        #[cfg(debug_assertions)]
        if self
            .inner
            .owner
            .as_ref()
            .is_some_and(|owner| owner.kind == ManagedOwnerKind::Speculation)
            && managed_reaper_env_seams().first_cleanup_unknown
            && !FIRST_SPECULATION_CLEANUP_UNKNOWN_INJECTED
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            if let Some(fixture) = &managed_reaper_env_seams().fixture_root {
                let _ = std::fs::write(
                    fixture.join("control/managed-first-cleanup-unknown-orphan-risk"),
                    b"1\n",
                );
            }
            return Ok(ReconcileOutcome::UnknownOrphanRisk(
                ManagedReconcileCode::ProcessEvidenceUnavailable,
            ));
        }
        self.inner
            .registry
            .cleanup(self.inner.key.slot, self.inner.key.generation)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn reconcile_owner(&self) -> Result<ManagedOwnerOutcome> {
        let owner = self
            .inner
            .owner
            .as_ref()
            .context("managed launch has no typed owner")?;
        self.inner.registry.reconcile_owner(owner)
    }
}

#[cfg(target_os = "linux")]
impl ManagedCleanupOwner {
    fn terminate(&self) -> Result<ReconcileOutcome> {
        if managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_CLEANUP_ERROR") {
            bail!("injected managed cleanup error");
        }
        match self {
            Self::Controller(controller) => controller.terminate(),
            Self::FailedLaunch {
                registry,
                slot,
                generation,
            } => registry.cleanup(*slot, *generation),
        }
    }
}

impl ManagedKey {
    pub(crate) fn slot(self) -> u16 {
        self.slot
    }

    pub(crate) fn generation(self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) fn test_value(slot: u16, generation: u64) -> Self {
        Self { slot, generation }
    }
}

#[cfg(target_os = "linux")]
struct ManagedReapJob {
    child: Option<std::process::Child>,
    cleanup: ManagedCleanupOwner,
    lifetime_guard: Option<ManagedLifetimeGuard>,
    not_before: Option<std::time::Instant>,
}

#[cfg(target_os = "linux")]
impl ManagedReapJob {
    fn new(
        child: Option<std::process::Child>,
        cleanup: ManagedCleanupOwner,
        lifetime_guard: Option<ManagedLifetimeGuard>,
    ) -> Self {
        let not_before = managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_JOB_STUCK")
            .then(|| std::time::Instant::now() + Duration::from_millis(500));
        Self {
            child,
            cleanup,
            lifetime_guard,
            not_before,
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct ManagedReaperState {
    jobs: VecDeque<ManagedReapJob>,
    supervisor_running: bool,
    active_jobs: usize,
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct ManagedReaperQueue {
    state: Mutex<ManagedReaperState>,
    changed: Condvar,
}

#[cfg(target_os = "linux")]
static MANAGED_REAPER_QUEUE: OnceLock<Arc<ManagedReaperQueue>> = OnceLock::new();

#[cfg(all(debug_assertions, target_os = "linux"))]
struct ManagedReaperEnvSeams {
    initialized_on: std::thread::ThreadId,
    spawn_failure: bool,
    wait_error: bool,
    pending_reap: bool,
    cleanup_error: bool,
    job_stuck: bool,
    first_cleanup_unknown: bool,
    fixture_root: Option<PathBuf>,
}

#[cfg(all(debug_assertions, target_os = "linux"))]
static MANAGED_REAPER_ENV_SEAMS: OnceLock<ManagedReaperEnvSeams> = OnceLock::new();

#[cfg(all(debug_assertions, target_os = "linux"))]
fn managed_reaper_env_seams() -> &'static ManagedReaperEnvSeams {
    MANAGED_REAPER_ENV_SEAMS.get_or_init(|| {
        let enabled = std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref()
            == Some(std::ffi::OsStr::new("1"));
        let enabled_seam = |name: &str| {
            enabled && std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new("1"))
        };
        ManagedReaperEnvSeams {
            initialized_on: std::thread::current().id(),
            spawn_failure: enabled_seam("LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE"),
            wait_error: enabled_seam("LTERM_INTERNAL_MANAGED_FORCE_REAPER_WAIT_ERROR"),
            pending_reap: enabled_seam("LTERM_INTERNAL_MANAGED_FORCE_PENDING_REAP"),
            cleanup_error: enabled_seam("LTERM_INTERNAL_MANAGED_FORCE_CLEANUP_ERROR"),
            job_stuck: enabled_seam("LTERM_INTERNAL_MANAGED_FORCE_JOB_STUCK"),
            first_cleanup_unknown: enabled_seam("LTERM_INTERNAL_MANAGED_FIRST_CLEANUP_UNKNOWN"),
            fixture_root: enabled
                .then(|| std::env::var_os("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT"))
                .flatten()
                .map(PathBuf::from),
        }
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn initialize_managed_reaper_config() {
    #[cfg(debug_assertions)]
    {
        let _ = managed_process_env_seams();
        let _ = managed_reaper_env_seams();
    }
}

#[cfg(target_os = "linux")]
fn managed_reaper_queue() -> Arc<ManagedReaperQueue> {
    Arc::clone(MANAGED_REAPER_QUEUE.get_or_init(|| Arc::new(ManagedReaperQueue::default())))
}

#[cfg(target_os = "linux")]
fn managed_reaper_seam(name: &str) -> bool {
    #[cfg(debug_assertions)]
    {
        use std::sync::atomic::Ordering;
        let env = managed_reaper_env_seams();
        match name {
            "LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE" => {
                env.spawn_failure || FORCE_REAPER_SPAWN_FAILURE.load(Ordering::SeqCst)
            }
            "LTERM_INTERNAL_MANAGED_FORCE_REAPER_WAIT_ERROR" => {
                env.wait_error || FORCE_REAPER_WAIT_ERROR.load(Ordering::SeqCst)
            }
            "LTERM_INTERNAL_MANAGED_FORCE_PENDING_REAP" => {
                env.pending_reap || FORCE_PENDING_REAP.load(Ordering::SeqCst)
            }
            "LTERM_INTERNAL_MANAGED_FORCE_CLEANUP_ERROR" => {
                env.cleanup_error || FORCE_CLEANUP_ERROR.load(Ordering::SeqCst)
            }
            "LTERM_INTERNAL_MANAGED_FORCE_JOB_STUCK" => {
                env.job_stuck || FORCE_JOB_STUCK.load(Ordering::SeqCst)
            }
            _ => false,
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = name;
        false
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn set_managed_reaper_seam(name: &str, enabled: bool) -> Result<()> {
    use std::sync::atomic::Ordering;
    let seam = match name {
        "LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE" => &FORCE_REAPER_SPAWN_FAILURE,
        "LTERM_INTERNAL_MANAGED_FORCE_REAPER_WAIT_ERROR" => &FORCE_REAPER_WAIT_ERROR,
        "LTERM_INTERNAL_MANAGED_FORCE_PENDING_REAP" => &FORCE_PENDING_REAP,
        "LTERM_INTERNAL_MANAGED_FORCE_CLEANUP_ERROR" => &FORCE_CLEANUP_ERROR,
        "LTERM_INTERNAL_MANAGED_FORCE_JOB_STUCK" => &FORCE_JOB_STUCK,
        _ => bail!("unknown managed reaper seam"),
    };
    seam.store(enabled, Ordering::SeqCst);
    managed_reaper_queue().changed.notify_all();
    Ok(())
}

#[cfg(target_os = "linux")]
fn enqueue_managed_reap_job(job: ManagedReapJob) {
    let queue = managed_reaper_queue();
    {
        let mut state = queue
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.jobs.push_back(job);
        queue.changed.notify_all();
    }
}

#[cfg(target_os = "linux")]
fn ensure_managed_reaper_supervisor(queue: &Arc<ManagedReaperQueue>) -> Result<()> {
    initialize_managed_reaper_config();
    let mut state = queue
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.supervisor_running {
        return Ok(());
    }
    let worker_queue = Arc::clone(queue);
    std::thread::Builder::new()
        .name("lterm-managed-root-reaper".into())
        .spawn(move || run_managed_reaper_supervisor(worker_queue))
        .context("start managed root-reaper supervisor")?;
    state.supervisor_running = true;
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_managed_reaper_supervisor(queue: Arc<ManagedReaperQueue>) {
    loop {
        let mut job = {
            let mut state = queue
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while state.jobs.is_empty()
                || managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE")
            {
                let (next, _) = queue
                    .changed
                    .wait_timeout(state, Duration::from_millis(25))
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state = next;
            }
            let job = state
                .jobs
                .pop_front()
                .expect("managed reaper queue became empty while locked");
            state.active_jobs += 1;
            job
        };
        let reap_resolved = match job.child.as_mut() {
            Some(_)
                if job
                    .not_before
                    .is_some_and(|deadline| std::time::Instant::now() < deadline) =>
            {
                false
            }
            Some(_) if managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_REAPER_WAIT_ERROR") => {
                false
            }
            Some(child) => {
                // The queue owns the exact handle and never blocks on one job.
                // Repeated termination plus try_wait lets later jobs converge
                // even when an earlier child is temporarily non-reapable.
                let _ = child.kill();
                matches!(child.try_wait(), Ok(Some(_)))
            }
            None => true,
        };
        if !reap_resolved {
            let mut state = queue
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.jobs.push_back(job);
            state.active_jobs -= 1;
            queue.changed.notify_all();
            drop(state);
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }
        job.child.take();
        let cleanup_result = job.cleanup.terminate();
        let process_cleanup_resolved = matches!(
            cleanup_result,
            Ok(ReconcileOutcome::ResolvedTombstone | ReconcileOutcome::Absent)
        );
        let artifact_cleanup_resolved = process_cleanup_resolved
            && job
                .lifetime_guard
                .as_ref()
                .map_or(Ok(true), ManagedLifetimeGuard::cleanup_artifacts)
                .unwrap_or(false);
        let mut state = queue
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active_jobs -= 1;
        if artifact_cleanup_resolved {
            drop(job.lifetime_guard.take());
        } else {
            state.jobs.push_back(job);
        }
        queue.changed.notify_all();
        drop(state);
        if !artifact_cleanup_resolved {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn drain_managed_reaper_queue_bounded(timeout: Duration) -> bool {
    let queue = managed_reaper_queue();
    if ensure_managed_reaper_supervisor(&queue).is_err() {
        return false;
    }
    queue.changed.notify_all();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let state = queue
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.jobs.is_empty() && state.active_jobs == 0 {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(now);
        let _ = queue
            .changed
            .wait_timeout(state, remaining.min(Duration::from_millis(25)));
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
pub(crate) fn managed_reaper_pending_jobs() -> usize {
    let queue = managed_reaper_queue();
    let state = queue
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.jobs.len() + state.active_jobs
}

impl ManagedWaiter {
    #[cfg(target_os = "linux")]
    fn handoff_cleanup_only(&mut self) {
        enqueue_managed_reap_job(ManagedReapJob::new(
            None,
            self.cleanup.clone(),
            self.lifetime_guard.take(),
        ));
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn terminate_and_reap_bounded(
        mut self,
        reap_timeout: Duration,
    ) -> ManagedBoundedReap {
        let Some(child) = self.child.as_mut() else {
            if !matches!(
                self.cleanup.terminate(),
                Ok(ReconcileOutcome::ResolvedTombstone | ReconcileOutcome::Absent)
            ) || !self
                .lifetime_guard
                .as_ref()
                .map_or(Ok(true), ManagedLifetimeGuard::cleanup_artifacts)
                .unwrap_or(false)
            {
                self.handoff_cleanup_only();
            }
            return ManagedBoundedReap::Reaped;
        };
        let _ = child.kill();
        if managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_PENDING_REAP") {
            return ManagedBoundedReap::Pending(self);
        }
        match reap_child_until(child, reap_timeout) {
            Ok(true) => {
                self.child.take();
                let process_cleanup_resolved = matches!(
                    self.cleanup.terminate(),
                    Ok(ReconcileOutcome::ResolvedTombstone | ReconcileOutcome::Absent)
                );
                let artifact_cleanup_resolved = process_cleanup_resolved
                    && self
                        .lifetime_guard
                        .as_ref()
                        .map_or(Ok(true), ManagedLifetimeGuard::cleanup_artifacts)
                        .unwrap_or(false);
                if !artifact_cleanup_resolved {
                    self.handoff_cleanup_only();
                }
                ManagedBoundedReap::Reaped
            }
            Ok(false) | Err(_) => ManagedBoundedReap::Pending(self),
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn wait_until(
        &mut self,
        deadline: std::time::Instant,
    ) -> Result<std::process::ExitStatus> {
        let status = loop {
            let child = self
                .child
                .as_mut()
                .context("managed root-process handle was already consumed")?;
            if let Some(status) = child.try_wait().context("poll managed root process")? {
                break status;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "managed root-process wait deadline expired"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        self.child.take();
        let cleanup = self.cleanup.terminate();
        let process_cleanup_resolved = matches!(
            cleanup,
            Ok(ReconcileOutcome::ResolvedTombstone | ReconcileOutcome::Absent)
        );
        let artifact_cleanup_resolved = process_cleanup_resolved
            && self
                .lifetime_guard
                .as_ref()
                .map_or(Ok(true), ManagedLifetimeGuard::cleanup_artifacts)
                .unwrap_or(false);
        if !artifact_cleanup_resolved {
            self.handoff_cleanup_only();
            bail!("managed root process reaped with cleanup pending");
        }
        Ok(status)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn wait(mut self) -> Result<std::process::ExitStatus> {
        let status = self
            .child
            .as_mut()
            .context("managed root-process handle was already consumed")?
            .wait()
            .context("wait for managed root process")?;
        self.child.take();
        let cleanup = self.cleanup.terminate();
        let process_cleanup_resolved = matches!(
            cleanup,
            Ok(ReconcileOutcome::ResolvedTombstone | ReconcileOutcome::Absent)
        );
        let artifact_cleanup_resolved = process_cleanup_resolved
            && self
                .lifetime_guard
                .as_ref()
                .map_or(Ok(true), ManagedLifetimeGuard::cleanup_artifacts)
                .unwrap_or(false);
        if !artifact_cleanup_resolved {
            self.handoff_cleanup_only();
            bail!("managed root process reaped with cleanup pending");
        }
        Ok(status)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn terminate_and_wait(mut self) -> Result<std::process::ExitStatus> {
        let child = self
            .child
            .as_mut()
            .context("managed root-process handle was already consumed")?;
        let _ = child.kill();
        let status = child
            .wait()
            .context("reap managed root process after termination")?;
        self.child.take();
        let final_cleanup = self.cleanup.terminate();
        let process_cleanup_resolved = matches!(
            final_cleanup,
            Ok(ReconcileOutcome::ResolvedTombstone | ReconcileOutcome::Absent)
        );
        let artifact_cleanup_resolved = process_cleanup_resolved
            && self
                .lifetime_guard
                .as_ref()
                .map_or(Ok(true), ManagedLifetimeGuard::cleanup_artifacts)
                .unwrap_or(false);
        if !artifact_cleanup_resolved {
            self.handoff_cleanup_only();
            bail!("managed root process reaped with cleanup pending");
        }
        Ok(status)
    }
}

impl Drop for ManagedWaiter {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = self.cleanup.terminate();
            enqueue_managed_reap_job(ManagedReapJob::new(
                Some(child),
                self.cleanup.clone(),
                self.lifetime_guard.take(),
            ));
        }
    }
}

#[derive(Clone, Debug)]
struct Registry {
    root: PathBuf,
    slots: PathBuf,
    guards: PathBuf,
    registrations: PathBuf,
    slot_count: usize,
}

#[derive(Debug)]
struct OfdLock {
    file: File,
}

#[derive(Debug)]
struct LaunchIntent {
    record: SlotRecord,
    guard: OfdLock,
}

#[derive(Debug)]
pub(crate) struct ManagedLaunchReservation {
    registry: Registry,
    intent: Option<LaunchIntent>,
}

impl ManagedLaunchReservation {
    pub(crate) fn key(&self) -> ManagedKey {
        let record = &self
            .intent
            .as_ref()
            .expect("managed launch reservation invariant")
            .record;
        ManagedKey {
            slot: record.slot,
            generation: record.generation,
        }
    }

    pub(crate) fn owner(&self) -> Option<&ManagedOwnerTag> {
        self.intent
            .as_ref()
            .expect("managed launch reservation invariant")
            .record
            .owner
            .as_ref()
    }

    pub(crate) fn artifact_binding(&self) -> Option<&ManagedArtifactBinding> {
        self.intent
            .as_ref()
            .expect("managed launch reservation invariant")
            .record
            .artifact_binding
            .as_deref()
    }

    pub(crate) fn begin_artifact_creation(
        &mut self,
        control_root: ManagedDirectoryIdentity,
        private_leaf: &str,
    ) -> Result<ManagedArtifactBinding> {
        control_root.validate()?;
        let intent = self
            .intent
            .as_mut()
            .context("managed launch reservation was already consumed")?;
        ensure!(
            intent.record.artifact_binding.is_none(),
            "managed launch reservation already has an artifact binding"
        );
        let SlotState::IntentDurable { nonce, .. } = intent.record.state else {
            bail!("managed artifact binding requires an intent reservation")
        };
        let binding = ManagedArtifactBinding {
            nonce,
            control_root,
            private_leaf: private_leaf.to_owned(),
            private_directory: None,
            runner: None,
            socket: None,
            owner: None,
            runner_create_pending: false,
            socket_create_pending: false,
            owner_create_pending: false,
            cleanup_quarantine: None,
            cleanup_unlink_pending: None,
            socket_retire_pending: false,
            socket_retired: false,
            cleanup_ownership_removed: false,
            cleanup_socket_removed: false,
            cleanup_runner_removed: false,
            cleanup_completed: false,
        };
        binding.validate()?;
        let next = SlotRecord {
            artifact_binding: Some(Box::new(binding.clone())),
            ..intent.record.clone()
        };
        self.registry.replace_slot(&intent.record, &next)?;
        intent.record = next;
        Ok(binding)
    }

    pub(crate) fn begin_artifact_runner_creation(&mut self) -> Result<ManagedArtifactBinding> {
        let intent = self
            .intent
            .as_mut()
            .context("managed launch reservation was already consumed")?;
        let created = intent
            .record
            .artifact_binding
            .as_deref()
            .context("managed artifact creation was not begun")?;
        ensure!(
            created.creation_phase() == Some(ManagedArtifactCreationPhase::DirectoryCreated),
            "managed runner creation is out of order"
        );
        let binding = ManagedArtifactBinding {
            runner_create_pending: true,
            ..created.clone()
        };
        binding.validate()?;
        let next = SlotRecord {
            artifact_binding: Some(Box::new(binding.clone())),
            ..intent.record.clone()
        };
        self.registry.replace_slot(&intent.record, &next)?;
        intent.record = next;
        Ok(binding)
    }

    pub(crate) fn finish_artifact_runner(
        &mut self,
        runner: ManagedArtifactIdentity,
    ) -> Result<ManagedArtifactBinding> {
        runner.validate()?;
        let intent = self
            .intent
            .as_mut()
            .context("managed launch reservation was already consumed")?;
        let created = intent
            .record
            .artifact_binding
            .as_deref()
            .context("managed artifact creation was not begun")?;
        ensure!(
            created.creation_phase() == Some(ManagedArtifactCreationPhase::RunnerCreatePending),
            "managed runner creation was not durably begun"
        );
        let binding = ManagedArtifactBinding {
            runner: Some(runner),
            runner_create_pending: false,
            ..created.clone()
        };
        let next = SlotRecord {
            artifact_binding: Some(Box::new(binding.clone())),
            ..intent.record.clone()
        };
        self.registry.replace_slot(&intent.record, &next)?;
        intent.record = next;
        Ok(binding)
    }

    pub(crate) fn begin_artifact_socket_creation(&mut self) -> Result<ManagedArtifactBinding> {
        let intent = self
            .intent
            .as_mut()
            .context("managed launch reservation was already consumed")?;
        let created = intent
            .record
            .artifact_binding
            .as_deref()
            .context("managed artifact creation was not begun")?;
        ensure!(
            created.creation_phase() == Some(ManagedArtifactCreationPhase::RunnerCreated),
            "managed socket creation is out of order"
        );
        let binding = ManagedArtifactBinding {
            socket_create_pending: true,
            ..created.clone()
        };
        binding.validate()?;
        let next = SlotRecord {
            artifact_binding: Some(Box::new(binding.clone())),
            ..intent.record.clone()
        };
        self.registry.replace_slot(&intent.record, &next)?;
        intent.record = next;
        Ok(binding)
    }

    pub(crate) fn finish_artifact_socket(
        &mut self,
        socket: ManagedArtifactIdentity,
    ) -> Result<ManagedArtifactBinding> {
        socket.validate()?;
        let intent = self
            .intent
            .as_mut()
            .context("managed launch reservation was already consumed")?;
        let created = intent
            .record
            .artifact_binding
            .as_deref()
            .context("managed artifact creation was not begun")?;
        ensure!(
            created.creation_phase() == Some(ManagedArtifactCreationPhase::SocketCreatePending),
            "managed socket creation was not durably begun"
        );
        let binding = ManagedArtifactBinding {
            socket: Some(socket),
            socket_create_pending: false,
            ..created.clone()
        };
        let next = SlotRecord {
            artifact_binding: Some(Box::new(binding.clone())),
            ..intent.record.clone()
        };
        self.registry.replace_slot(&intent.record, &next)?;
        intent.record = next;
        Ok(binding)
    }

    pub(crate) fn finish_artifact_owner(
        &mut self,
        owner: ManagedArtifactIdentity,
    ) -> Result<ManagedArtifactBinding> {
        owner.validate()?;
        let intent = self
            .intent
            .as_mut()
            .context("managed launch reservation was already consumed")?;
        let created = intent
            .record
            .artifact_binding
            .as_deref()
            .context("managed artifact creation was not begun")?;
        ensure!(
            created.creation_phase() == Some(ManagedArtifactCreationPhase::OwnerCreatePending),
            "managed owner creation was not durably begun"
        );
        let binding = ManagedArtifactBinding {
            owner: Some(owner),
            owner_create_pending: false,
            ..created.clone()
        };
        binding.validate()?;
        let next = SlotRecord {
            artifact_binding: Some(Box::new(binding.clone())),
            ..intent.record.clone()
        };
        self.registry.replace_slot(&intent.record, &next)?;
        intent.record = next;
        Ok(binding)
    }

    pub(crate) fn begin_artifact_owner_creation(&mut self) -> Result<ManagedArtifactBinding> {
        let intent = self
            .intent
            .as_mut()
            .context("managed launch reservation was already consumed")?;
        let created = intent
            .record
            .artifact_binding
            .as_deref()
            .context("managed artifact creation was not begun")?;
        ensure!(
            created.creation_phase() == Some(ManagedArtifactCreationPhase::SocketCreated),
            "managed owner creation is out of order"
        );
        let binding = ManagedArtifactBinding {
            owner_create_pending: true,
            ..created.clone()
        };
        binding.validate()?;
        let next = SlotRecord {
            artifact_binding: Some(Box::new(binding.clone())),
            ..intent.record.clone()
        };
        self.registry.replace_slot(&intent.record, &next)?;
        intent.record = next;
        Ok(binding)
    }

    pub(crate) fn finish_artifact_creation(
        &mut self,
        private_directory: ManagedDirectoryIdentity,
    ) -> Result<ManagedArtifactBinding> {
        private_directory.validate()?;
        let intent = self
            .intent
            .as_mut()
            .context("managed launch reservation was already consumed")?;
        let pending = intent
            .record
            .artifact_binding
            .as_deref()
            .context("managed artifact creation was not begun")?;
        ensure!(
            pending.creation_phase() == Some(ManagedArtifactCreationPhase::Reserved),
            "managed artifact directory creation is out of order"
        );
        let binding = ManagedArtifactBinding {
            private_directory: Some(private_directory),
            ..pending.clone()
        };
        let next = SlotRecord {
            artifact_binding: Some(Box::new(binding.clone())),
            ..intent.record.clone()
        };
        self.registry.replace_slot(&intent.record, &next)?;
        intent.record = next;
        Ok(binding)
    }

    fn into_parts(mut self) -> (Registry, LaunchIntent) {
        let intent = self
            .intent
            .take()
            .expect("managed launch reservation invariant");
        (self.registry.clone(), intent)
    }
}

impl Drop for ManagedLaunchReservation {
    fn drop(&mut self) {
        let Some(intent) = self.intent.take() else {
            return;
        };
        #[cfg(not(target_os = "linux"))]
        {
            drop(intent);
        }
        #[cfg(target_os = "linux")]
        {
            let slot = intent.record.slot;
            let generation = intent.record.generation;
            drop(intent);
            let _ = self.registry.cleanup(slot, generation);
        }
    }
}

#[derive(Debug)]
enum SlotRead {
    Valid(SlotRecord),
    Unknown(String),
}

enum StagedSlotGeneration {
    Legacy {
        path: PathBuf,
        records: Vec<SlotRecordV1>,
    },
    Current {
        path: PathBuf,
        records: Vec<SlotRecord>,
    },
}

impl StagedSlotGeneration {
    fn path(&self) -> &Path {
        match self {
            Self::Legacy { path, .. } | Self::Current { path, .. } => path,
        }
    }
}

impl Registry {
    #[cfg(target_os = "linux")]
    fn open_default() -> Result<Self> {
        Self::open_at(managed_process_registry_path()?, SLOT_COUNT)
    }

    fn open_at(root: PathBuf, slot_count: usize) -> Result<Self> {
        ensure!(slot_count > 0 && slot_count <= u16::MAX as usize);
        let registry = Self {
            slots: root.join("slots"),
            guards: root.join("guards"),
            registrations: root.join("registrations"),
            root,
            slot_count,
        };
        registry.ensure_genesis()?;
        registry.validate_layout()?;
        if !registry.current_schema_ready_without_migration()? {
            registry.migrate_v1_slots()?;
        }
        Ok(registry)
    }

    fn ensure_genesis(&self) -> Result<()> {
        match fs::symlink_metadata(&self.root) {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("lstat {}", self.root.display())),
        }

        let parent = self.root.parent().context("registry root has no parent")?;
        ensure_exact_private_dir(parent)?;
        let leaf = self
            .root
            .file_name()
            .context("registry root has no file name")?
            .to_string_lossy();
        let temp = parent.join(format!(".{leaf}.genesis-{}", Uuid::new_v4()));
        let cleanup = TempTree::new(temp.clone());

        create_exact_dir(&temp)?;
        let slots = temp.join("slots");
        let guards = temp.join("guards");
        let registrations = temp.join("registrations");
        create_exact_dir(&slots)?;
        create_exact_dir(&guards)?;
        create_exact_dir(&registrations)?;
        create_exact_file(&temp.join("registry.lock"), b"")?;
        create_exact_file(&temp.join(SLOT_SCHEMA_MARKER), b"2\n")?;

        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            let record = SlotRecord {
                schema_version: SLOT_SCHEMA_VERSION,
                slot,
                generation: 0,
                owner: None,
                artifact_binding: None,
                state: SlotState::Vacant,
            };
            create_exact_json_file(&slots.join(slot_name(slot)), &record)?;
            create_exact_file(&guards.join(guard_name(slot)), b"")?;
            let registration = RegistrationRecord {
                schema_version: GATE_SCHEMA_VERSION,
                slot,
                generation: 0,
                registration: None,
            };
            create_exact_json_file(&registrations.join(registration_name(slot)), &registration)?;
        }

        sync_dir(&slots)?;
        sync_dir(&guards)?;
        sync_dir(&registrations)?;
        sync_dir(&temp)?;
        match rename_noreplace(&temp, &self.root) {
            Ok(()) => cleanup.disarm(),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                drop(cleanup);
            }
            Err(err) => return Err(err).context("install registry genesis with no-replace rename"),
        }
        sync_dir(parent)?;
        Ok(())
    }

    fn validate_layout(&self) -> Result<()> {
        validate_exact_dir(&self.root)?;
        validate_exact_dir(&self.slots)?;
        validate_exact_dir(&self.guards)?;
        validate_exact_dir(&self.registrations)?;
        validate_exact_file(&self.root.join("registry.lock"))?;
        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            validate_exact_file(&self.slots.join(slot_name(slot)))?;
            validate_exact_file(&self.guards.join(guard_name(slot)))?;
            validate_exact_file(&self.registrations.join(registration_name(slot)))?;
        }
        Ok(())
    }

    fn current_schema_ready_without_migration(&self) -> Result<bool> {
        if !self.slot_schema_marker_ready()? {
            return Ok(false);
        }
        for entry in fs::read_dir(&self.root)? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with(SLOT_MIGRATION_STAGING_PREFIX)
                || name.starts_with(SLOT_MIGRATION_BUILD_PREFIX)
                || name.starts_with(SLOT_MIGRATION_GC_PREFIX)
            {
                return Ok(false);
            }
        }
        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            let value: serde_json::Value = read_bounded_json(&self.slots.join(slot_name(slot)))?;
            ensure!(
                value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_u64)
                    == Some(u64::from(SLOT_SCHEMA_VERSION)),
                "managed schema marker published over a non-v2 slot"
            );
            let record: SlotRecord =
                serde_json::from_value(value).context("parse current managed slot record")?;
            validate_slot_record(&record, slot)?;
        }
        Ok(true)
    }

    fn reclaim_migration_work_directories(&self) -> Result<()> {
        let mut work = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let kind = if name.starts_with(SLOT_MIGRATION_BUILD_PREFIX) {
                Some(true)
            } else if name.starts_with(SLOT_MIGRATION_GC_PREFIX) {
                Some(false)
            } else {
                None
            };
            let Some(is_build) = kind else {
                continue;
            };
            let prefix = if is_build {
                SLOT_MIGRATION_BUILD_PREFIX
            } else {
                SLOT_MIGRATION_GC_PREFIX
            };
            let suffix = name
                .strip_prefix(prefix)
                .context("managed migration work prefix vanished")?;
            ensure!(
                suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "managed migration work name is invalid"
            );
            ensure!(work.len() < 16, "too many managed migration work trees");
            work.push((entry.path(), is_build));
        }
        for (path, is_build) in work {
            let gc = if is_build {
                validate_exact_dir(&path)?;
                let gc = self.root.join(format!(
                    "{SLOT_MIGRATION_GC_PREFIX}{}",
                    Uuid::new_v4().simple()
                ));
                rename_noreplace(&path, &gc)
                    .context("quarantine incomplete managed migration generation")?;
                sync_dir(&self.root)?;
                gc
            } else {
                path
            };
            #[cfg(target_os = "linux")]
            managed_test_failpoint("migration_after_gc_publish");
            self.remove_migration_gc_tree(&gc)?;
        }
        Ok(())
    }

    fn remove_migration_gc_tree(&self, gc: &Path) -> Result<()> {
        validate_exact_dir(gc)?;
        let entries = fs::read_dir(gc)?.collect::<std::io::Result<Vec<_>>>()?;
        ensure!(
            entries.len() <= self.slot_count,
            "managed migration GC tree has too many entries"
        );
        for entry in entries {
            let name = entry.file_name();
            let name = name
                .to_str()
                .context("managed migration GC entry is not UTF-8")?;
            let slot = name
                .strip_prefix("slot-")
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|slot| slot.parse::<u16>().ok())
                .filter(|slot| usize::from(*slot) < self.slot_count)
                .context("managed migration GC entry name is invalid")?;
            ensure!(
                name == slot_name(slot),
                "managed migration GC slot name changed"
            );
            let path = entry.path();
            validate_exact_file(&path)?;
            fs::remove_file(path)?;
            #[cfg(target_os = "linux")]
            managed_test_failpoint("migration_mid_gc_reclaim");
        }
        sync_dir(gc)?;
        fs::remove_dir(gc)?;
        sync_dir(&self.root)
    }

    fn scan_staged_slot_generations(&self) -> Result<Vec<StagedSlotGeneration>> {
        let mut staged = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(suffix) = name.strip_prefix(SLOT_MIGRATION_STAGING_PREFIX) else {
                continue;
            };
            ensure!(
                suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "managed slot migration staging name is invalid"
            );
            ensure!(
                staged.len() < 8,
                "too many managed slot migration generations"
            );
            let path = entry.path();
            validate_exact_dir(&path)?;
            let entries = fs::read_dir(&path)?.collect::<std::io::Result<Vec<_>>>()?;
            ensure!(
                entries.len() == self.slot_count,
                "managed slot migration generation has unexpected entries"
            );
            let mut legacy = Vec::with_capacity(self.slot_count);
            let mut current = Vec::with_capacity(self.slot_count);
            for slot in 0..self.slot_count {
                let slot = u16::try_from(slot).context("slot index overflow")?;
                let value: serde_json::Value = read_bounded_json(&path.join(slot_name(slot)))?;
                match value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_u64)
                {
                    Some(1) => {
                        validate_v1_slot_json(&value)?;
                        let record: SlotRecordV1 = serde_json::from_value(value)?;
                        validate_v1_slot_record(&record, slot)?;
                        legacy.push(record);
                    }
                    Some(version) if version == u64::from(SLOT_SCHEMA_VERSION) => {
                        let record: SlotRecord = serde_json::from_value(value)?;
                        validate_slot_record(&record, slot)?;
                        current.push(record);
                    }
                    _ => bail!("managed slot migration generation schema is invalid"),
                }
            }
            ensure!(
                legacy.is_empty() || current.is_empty(),
                "managed slot migration generation is mixed"
            );
            staged.push(if legacy.is_empty() {
                StagedSlotGeneration::Current {
                    path,
                    records: current,
                }
            } else {
                StagedSlotGeneration::Legacy {
                    path,
                    records: legacy,
                }
            });
        }
        Ok(staged)
    }

    fn remove_staged_slot_generation(&self, staged: &StagedSlotGeneration) -> Result<()> {
        let path = staged.path();
        validate_exact_dir(path)?;
        let gc = self.root.join(format!(
            "{SLOT_MIGRATION_GC_PREFIX}{}",
            Uuid::new_v4().simple()
        ));
        rename_noreplace(path, &gc).context("quarantine retired managed slot generation")?;
        sync_dir(&self.root)?;
        #[cfg(target_os = "linux")]
        managed_test_failpoint("migration_after_gc_publish");
        self.remove_migration_gc_tree(&gc)
    }

    fn legacy_generation_matches_current(
        &self,
        legacy: &[SlotRecordV1],
        current: &[SlotRecord],
    ) -> Result<()> {
        ensure!(
            legacy.len() == self.slot_count && current.len() == self.slot_count,
            "managed migration generation is incomplete"
        );
        for (legacy, current) in legacy.iter().zip(current) {
            ensure!(
                current.schema_version == SLOT_SCHEMA_VERSION
                    && current.slot == legacy.slot
                    && current.generation == legacy.generation
                    && current.owner == legacy.owner
                    && current.artifact_binding.is_none()
                    && current.state == legacy.state,
                "managed migration rollback generation does not match active v2"
            );
        }
        Ok(())
    }

    fn migrate_v1_slots(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed registry schema migration is busy")?;
        #[cfg(not(target_os = "linux"))]
        let _registry_lock = open_exact_file(&self.root.join("registry.lock"), true)?;
        self.reclaim_migration_work_directories()?;
        let marker_ready = self.slot_schema_marker_ready()?;
        let mut legacy_records = Vec::with_capacity(self.slot_count);
        let mut current_records = Vec::with_capacity(self.slot_count);
        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            let path = self.slots.join(slot_name(slot));
            let value: serde_json::Value = read_bounded_json(&path)?;
            let schema_version = value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                .context("managed slot schema version is missing")?;
            if schema_version == u64::from(SLOT_SCHEMA_VERSION) {
                let record: SlotRecord =
                    serde_json::from_value(value).context("parse current managed slot record")?;
                validate_slot_record(&record, slot)?;
                current_records.push(record);
            } else if schema_version == 1 {
                ensure!(
                    !marker_ready,
                    "managed schema marker published over a legacy slot"
                );
                validate_v1_slot_json(&value)?;
                let legacy: SlotRecordV1 =
                    serde_json::from_value(value).context("parse legacy managed slot record")?;
                validate_v1_slot_record(&legacy, slot)?;
                legacy_records.push(legacy);
            } else {
                bail!("unsupported managed slot schema version {schema_version}");
            }
        }

        ensure!(
            legacy_records.is_empty() || current_records.is_empty(),
            "mixed managed slot schema generation is invalid"
        );
        let staged = self.scan_staged_slot_generations()?;
        if legacy_records.is_empty() {
            ensure!(
                current_records.len() == self.slot_count,
                "managed v2 slot generation is incomplete"
            );
            ensure!(
                staged.len() <= 1,
                "multiple managed migration generations are ambiguous"
            );
            if !marker_ready {
                let Some(StagedSlotGeneration::Legacy { records, .. }) = staged.first() else {
                    bail!("unmarked managed v2 generation lacks rollback evidence")
                };
                self.legacy_generation_matches_current(records, &current_records)?;
                sync_dir(&self.root)?;
                #[cfg(target_os = "linux")]
                managed_test_errorpoint("migration_after_exchange_before_marker")?;
            } else if let Some(staged) = staged.first() {
                match staged {
                    StagedSlotGeneration::Legacy { records, .. } => {
                        self.legacy_generation_matches_current(records, &current_records)?;
                    }
                    StagedSlotGeneration::Current { records, .. } => ensure!(
                        records == &current_records,
                        "stale managed v2 staging generation is not active v2"
                    ),
                }
            }
            if !marker_ready {
                self.publish_slot_schema_marker()?;
            }
            for staged in &staged {
                self.remove_staged_slot_generation(staged)?;
            }
            return Ok(());
        }
        ensure!(
            legacy_records.len() == self.slot_count,
            "managed v1 slot generation is incomplete"
        );
        ensure!(
            staged.len() <= 1,
            "multiple managed migration generations are ambiguous"
        );
        for staged in &staged {
            let StagedSlotGeneration::Current { .. } = staged else {
                bail!("legacy active generation has ambiguous legacy staging evidence")
            };
            self.remove_staged_slot_generation(staged)?;
        }

        // Only a legacy generation requires slot-guard quiescence. Gate
        // children routinely open the already-current registry while their
        // parent retains a v2 slot guard, so scanning guards unconditionally
        // would reject valid launches. Legacy record_identity writes do not
        // take registry.lock, but they retain their per-slot guard for the
        // entire launch lifetime. registry.lock prevents any new legacy
        // allocator while this scan runs; a guard that is observed free cannot
        // be reacquired until migration releases the registry lock.
        #[cfg(target_os = "linux")]
        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            drop(
                OfdLock::try_acquire(&self.guards.join(guard_name(slot)))
                    .with_context(|| format!("managed slot {slot} is busy during migration"))?,
            );
        }

        // Re-read the complete generation after quiescence so a legacy writer
        // that finished between the first read and its guard probe cannot
        // escape into the prepared generation.
        legacy_records.clear();
        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            let value: serde_json::Value = read_bounded_json(&self.slots.join(slot_name(slot)))?;
            ensure!(
                value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_u64)
                    == Some(1),
                "managed v1 slot generation changed during migration"
            );
            validate_v1_slot_json(&value)?;
            let legacy: SlotRecordV1 =
                serde_json::from_value(value).context("parse quiesced legacy slot record")?;
            validate_v1_slot_record(&legacy, slot)?;
            legacy_records.push(legacy);
        }

        #[cfg(not(target_os = "linux"))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic managed slot generation exchange is unavailable on this platform",
        )
        .into());

        #[cfg(target_os = "linux")]
        {
            let generation = Uuid::new_v4();
            let building = self.root.join(format!(
                "{SLOT_MIGRATION_BUILD_PREFIX}{}",
                generation.simple()
            ));
            let prepared = self.root.join(format!(
                "{SLOT_MIGRATION_STAGING_PREFIX}{}",
                generation.simple()
            ));
            let cleanup = TempTree::new(building.clone());
            create_exact_dir(&building)?;
            let _legacy_count = legacy_records.len();
            for (_index, record) in legacy_records.into_iter().enumerate() {
                let migrated = SlotRecord {
                    schema_version: SLOT_SCHEMA_VERSION,
                    slot: record.slot,
                    generation: record.generation,
                    owner: record.owner,
                    artifact_binding: None,
                    state: record.state,
                };
                validate_slot_record(&migrated, migrated.slot)?;
                create_exact_json_file(&building.join(slot_name(migrated.slot)), &migrated)?;
                #[cfg(target_os = "linux")]
                if _index + 1 == _legacy_count.div_ceil(2) {
                    managed_test_failpoint("migration_mid_build");
                }
            }
            sync_dir(&building)?;
            rename_noreplace(&building, &prepared)
                .context("publish complete managed migration staging generation")?;
            cleanup.disarm();
            sync_dir(&self.root)?;
            // From this point the complete, atomically published staging
            // generation is durable recovery evidence.
            #[cfg(target_os = "linux")]
            managed_test_errorpoint("migration_after_staging_sync_before_exchange")?;
            rename_exchange(&prepared, &self.slots)
                .context("publish managed v2 slot directory generation")?;
            #[cfg(target_os = "linux")]
            managed_test_errorpoint("migration_after_exchange_before_root_sync")?;
            sync_dir(&self.root)?;
            #[cfg(target_os = "linux")]
            managed_test_errorpoint("migration_after_exchange_root_sync")?;
            let mut readback_records = Vec::with_capacity(self.slot_count);
            for slot in 0..self.slot_count {
                let slot = u16::try_from(slot).context("slot index overflow")?;
                let readback: SlotRecord = read_bounded_json(&self.slots.join(slot_name(slot)))?;
                validate_slot_record(&readback, slot)?;
                readback_records.push(readback);
            }
            let retired = self.scan_staged_slot_generations()?;
            ensure!(retired.len() == 1, "managed rollback generation is missing");
            let StagedSlotGeneration::Legacy { records, .. } = &retired[0] else {
                bail!("managed rollback generation has the wrong schema")
            };
            self.legacy_generation_matches_current(records, &readback_records)?;
            self.publish_slot_schema_marker()?;
            #[cfg(target_os = "linux")]
            managed_test_errorpoint("migration_after_marker_before_reclaim")?;
            self.remove_staged_slot_generation(&retired[0])?;
            Ok(())
        }
    }

    fn publish_slot_schema_marker(&self) -> Result<()> {
        let temp_name = format!(".{SLOT_SCHEMA_MARKER}.create");
        let temp = self.root.join(&temp_name);
        match fs::symlink_metadata(&temp) {
            Ok(_) => {
                validate_exact_file(&temp)?;
                fs::remove_file(&temp)?;
                sync_dir(&self.root)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        create_exact_file(&temp, b"2\n")?;
        rename_noreplace(&temp, &self.root.join(SLOT_SCHEMA_MARKER))?;
        sync_dir(&self.root)
    }

    fn slot_schema_marker_ready(&self) -> Result<bool> {
        let path = self.root.join(SLOT_SCHEMA_MARKER);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        let file = open_exact_file(&path, false)?;
        let mut bytes = Vec::new();
        file.take(16).read_to_end(&mut bytes)?;
        ensure!(bytes == b"2\n", "managed slot schema marker is invalid");
        Ok(true)
    }

    #[cfg(target_os = "linux")]
    fn allocate_intent(
        &self,
        now_unix_secs: u64,
        owner: Option<ManagedOwnerTag>,
    ) -> Result<LaunchIntent> {
        if let Some(owner) = &owner {
            owner.validate()?;
        }
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let mut selected = None;
        let mut unknown = 0usize;
        let mut unreadable = 0usize;

        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            match self.read_slot(slot) {
                SlotRead::Valid(record) => {
                    ensure!(
                        owner.is_none() || record.owner.as_ref() != owner.as_ref(),
                        "managed owner already has a durable registry record"
                    );
                    match &record.state {
                        SlotState::Vacant => {
                            selected.get_or_insert(record);
                        }
                        SlotState::ResolvedTombstone {
                            resolved_unix_secs, ..
                        } => {
                            if now_unix_secs
                                .checked_sub(*resolved_unix_secs)
                                .is_some_and(|age| age >= TOMBSTONE_RETENTION.as_secs())
                                && record.artifact_binding.is_none()
                            {
                                let vacant = SlotRecord {
                                    owner: None,
                                    state: SlotState::Vacant,
                                    ..record.clone()
                                };
                                self.replace_slot(&record, &vacant)?;
                                selected.get_or_insert(vacant);
                            } else {
                                unknown += 1;
                            }
                        }
                        _ => unknown += 1,
                    }
                }
                SlotRead::Unknown(_) => {
                    unknown += 1;
                    unreadable += 1;
                }
            }
        }

        ensure!(
            owner.is_none() || unreadable == 0,
            "unknown_orphan_risk: managed owner uniqueness is uncertain ({unreadable}/{} unreadable)",
            self.slot_count
        );

        let vacant = selected.with_context(|| {
            format!(
                "unknown_orphan_risk: managed registry capacity exhausted ({unknown}/{} unresolved)",
                self.slot_count
            )
        })?;
        let generation = vacant
            .generation
            .checked_add(1)
            .context("slot generation exhausted; generation must never wrap")?;
        let nonce = Uuid::new_v4();

        self.replace_registration(
            vacant.slot,
            &RegistrationRecord {
                schema_version: GATE_SCHEMA_VERSION,
                slot: vacant.slot,
                generation,
                registration: None,
            },
        )?;
        let intent = SlotRecord {
            schema_version: SLOT_SCHEMA_VERSION,
            slot: vacant.slot,
            generation,
            owner,
            artifact_binding: None,
            state: SlotState::IntentDurable {
                nonce,
                created_unix_secs: now_unix_secs,
            },
        };
        self.replace_slot(&vacant, &intent)?;

        // This ordering is intentional: the intent is durable before a child
        // can inherit the fixed guard's open file description.
        let guard = OfdLock::try_acquire(&self.guards.join(guard_name(vacant.slot)))
            .context("unknown_orphan_risk: intent durable but slot guard is busy")?;
        Ok(LaunchIntent {
            record: intent,
            guard,
        })
    }

    fn read_slot(&self, slot: u16) -> SlotRead {
        let path = self.slots.join(slot_name(slot));
        match read_bounded_json::<SlotRecord>(&path).and_then(|record| {
            validate_slot_record(&record, slot)?;
            Ok(record)
        }) {
            Ok(record) => SlotRead::Valid(record),
            Err(err) => SlotRead::Unknown(format!("{}: {err:#}", path.display())),
        }
    }

    fn read_valid_slot(&self, slot: u16) -> Result<SlotRecord> {
        match self.read_slot(slot) {
            SlotRead::Valid(record) => Ok(record),
            SlotRead::Unknown(reason) => bail!("unknown_orphan_risk: {reason}"),
        }
    }

    fn replace_slot(&self, expected: &SlotRecord, next: &SlotRecord) -> Result<()> {
        validate_transition(expected, next)?;
        let current = self.read_valid_slot(expected.slot)?;
        ensure!(
            current == *expected,
            "generation/state conflict replacing slot {}",
            expected.slot
        );
        atomic_replace_json(&self.slots, &slot_name(expected.slot), next)?;
        #[cfg(target_os = "linux")]
        managed_test_errorpoint("after_slot_replace_before_readback")?;
        let readback = self.read_valid_slot(expected.slot)?;
        ensure!(readback == *next, "slot durable readback mismatch");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn acknowledge_artifact_cleanup(
        &self,
        key: ManagedKey,
        binding: &ManagedArtifactBinding,
    ) -> Result<()> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        if current.artifact_binding.is_none()
            && matches!(current.state, SlotState::ResolvedTombstone { .. })
        {
            return Ok(());
        }
        let authoritative = current
            .artifact_binding
            .as_deref()
            .context("managed artifact cleanup was already acknowledged")?;
        ensure!(
            authoritative.same_artifact(binding),
            "managed artifact cleanup acknowledgement mismatch"
        );
        ensure!(
            !authoritative.creation_pending(),
            "managed artifact creation is unresolved"
        );
        ensure!(
            matches!(current.state, SlotState::ResolvedTombstone { .. })
                && authoritative.cleanup_quarantine.is_some()
                && authoritative.cleanup_completed,
            "managed artifact cleanup acknowledgement lacks durable completion proof"
        );
        let next = SlotRecord {
            artifact_binding: None,
            ..current.clone()
        };
        self.replace_slot(&current, &next)
    }

    #[cfg(target_os = "linux")]
    fn begin_artifact_cleanup(
        &self,
        key: ManagedKey,
        binding: &ManagedArtifactBinding,
    ) -> Result<ManagedArtifactBinding> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        ensure!(
            matches!(current.state, SlotState::ResolvedTombstone { .. }),
            "managed artifact cleanup requires a resolved process tombstone"
        );
        let authoritative = current
            .artifact_binding
            .as_deref()
            .context("managed artifact cleanup binding is absent")?;
        ensure!(
            !authoritative.socket_retire_pending,
            "managed socket retirement is unresolved"
        );
        ensure!(
            !authoritative.creation_pending(),
            "managed artifact creation is unresolved"
        );
        ensure!(
            authoritative.same_artifact(binding),
            "managed artifact cleanup binding mismatch"
        );
        if authoritative.cleanup_quarantine.is_some() {
            return Ok(authoritative.clone());
        }
        let quarantine = format!(
            ".{}.cleanup-{}",
            authoritative.private_leaf,
            authoritative.nonce.simple()
        );
        validate_managed_artifact_leaf(&quarantine)?;
        let next_binding = ManagedArtifactBinding {
            cleanup_quarantine: Some(quarantine),
            cleanup_completed: false,
            ..authoritative.clone()
        };
        let next = SlotRecord {
            artifact_binding: Some(Box::new(next_binding.clone())),
            ..current.clone()
        };
        self.replace_slot(&current, &next)?;
        Ok(next_binding)
    }

    #[cfg(target_os = "linux")]
    fn begin_artifact_socket_retirement(
        &self,
        key: ManagedKey,
        binding: &ManagedArtifactBinding,
    ) -> Result<ManagedArtifactBinding> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        ensure!(
            matches!(current.state, SlotState::IdentityDurable { .. }),
            "managed socket retirement requires a live identity"
        );
        let authoritative = current
            .artifact_binding
            .as_deref()
            .context("managed socket binding is absent")?;
        ensure!(
            !authoritative.creation_pending() && authoritative.owner.is_some(),
            "managed artifact creation is unresolved"
        );
        ensure!(
            authoritative.same_artifact(binding)
                && authoritative.cleanup_quarantine.is_none()
                && authoritative.socket.is_some(),
            "managed socket retirement binding mismatch"
        );
        if authoritative.socket_retired || authoritative.socket_retire_pending {
            return Ok(authoritative.clone());
        }
        let pending = ManagedArtifactBinding {
            socket_retire_pending: true,
            ..authoritative.clone()
        };
        let next = SlotRecord {
            artifact_binding: Some(Box::new(pending.clone())),
            ..current.clone()
        };
        self.replace_slot(&current, &next)?;
        Ok(pending)
    }

    #[cfg(target_os = "linux")]
    fn finish_artifact_socket_retirement(
        &self,
        key: ManagedKey,
        binding: &ManagedArtifactBinding,
    ) -> Result<ManagedArtifactBinding> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        let authoritative = current
            .artifact_binding
            .as_deref()
            .context("managed socket binding is absent")?;
        ensure!(
            authoritative.same_artifact(binding) && authoritative.cleanup_quarantine.is_none(),
            "managed socket retirement receipt mismatch"
        );
        if authoritative.socket_retired {
            return Ok(authoritative.clone());
        }
        ensure!(
            authoritative.socket_retire_pending,
            "managed socket retirement receipt lacks durable intent"
        );
        let retired = ManagedArtifactBinding {
            socket_retire_pending: false,
            socket_retired: true,
            ..authoritative.clone()
        };
        let next = SlotRecord {
            artifact_binding: Some(Box::new(retired.clone())),
            ..current.clone()
        };
        self.replace_slot(&current, &next)?;
        Ok(retired)
    }

    #[cfg(target_os = "linux")]
    fn finish_artifact_cleanup(
        &self,
        key: ManagedKey,
        binding: &ManagedArtifactBinding,
    ) -> Result<ManagedArtifactBinding> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        ensure!(
            matches!(current.state, SlotState::ResolvedTombstone { .. }),
            "managed artifact completion requires a resolved process tombstone"
        );
        let authoritative = current
            .artifact_binding
            .as_deref()
            .context("managed artifact completion binding is absent")?;
        ensure!(
            authoritative.same_artifact(binding),
            "managed artifact completion binding mismatch"
        );
        if authoritative.cleanup_completed {
            return Ok(authoritative.clone());
        }
        ensure!(
            authoritative.cleanup_quarantine.is_some()
                && authoritative.cleanup_ownership_removed
                && authoritative.cleanup_socket_removed
                && authoritative.cleanup_runner_removed
                && authoritative.cleanup_unlink_pending
                    == Some(ManagedArtifactCleanupStep::Directory),
            "managed artifact completion lacks quarantine phase"
        );
        let completed = ManagedArtifactBinding {
            cleanup_unlink_pending: None,
            cleanup_completed: true,
            ..authoritative.clone()
        };
        let next = SlotRecord {
            artifact_binding: Some(Box::new(completed.clone())),
            ..current.clone()
        };
        self.replace_slot(&current, &next)?;
        Ok(completed)
    }

    #[cfg(target_os = "linux")]
    fn begin_artifact_unlink(
        &self,
        key: ManagedKey,
        binding: &ManagedArtifactBinding,
        step: ManagedArtifactCleanupStep,
    ) -> Result<ManagedArtifactBinding> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        ensure!(
            matches!(current.state, SlotState::ResolvedTombstone { .. }),
            "managed artifact unlink intent requires a resolved process tombstone"
        );
        let authoritative = current
            .artifact_binding
            .as_deref()
            .context("managed artifact cleanup binding is absent")?;
        ensure!(
            authoritative.same_artifact(binding),
            "managed artifact unlink intent binding mismatch"
        );
        ensure!(
            authoritative.cleanup_quarantine.is_some() && !authoritative.cleanup_completed,
            "managed artifact unlink intent is outside cleanup phase"
        );
        if authoritative.cleanup_step_completed(step) {
            return Ok(authoritative.clone());
        }
        if let Some(pending) = authoritative.cleanup_unlink_pending {
            ensure!(
                pending == step,
                "another managed artifact unlink is uncertain"
            );
            return Ok(authoritative.clone());
        }
        let pending = ManagedArtifactBinding {
            cleanup_unlink_pending: Some(step),
            ..authoritative.clone()
        };
        pending.validate()?;
        let next = SlotRecord {
            artifact_binding: Some(Box::new(pending.clone())),
            ..current.clone()
        };
        self.replace_slot(&current, &next)?;
        Ok(pending)
    }

    #[cfg(target_os = "linux")]
    fn finish_artifact_unlink(
        &self,
        key: ManagedKey,
        binding: &ManagedArtifactBinding,
        step: ManagedArtifactCleanupStep,
    ) -> Result<ManagedArtifactBinding> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        ensure!(
            matches!(current.state, SlotState::ResolvedTombstone { .. }),
            "managed artifact cleanup step requires a resolved process tombstone"
        );
        let authoritative = current
            .artifact_binding
            .as_deref()
            .context("managed artifact cleanup binding is absent")?;
        ensure!(
            authoritative.same_artifact(binding),
            "managed artifact cleanup step binding mismatch"
        );
        ensure!(
            authoritative.cleanup_quarantine.is_some() && !authoritative.cleanup_completed,
            "managed artifact cleanup step is outside cleanup phase"
        );
        if authoritative.cleanup_step_completed(step) {
            return Ok(authoritative.clone());
        }
        ensure!(
            authoritative.cleanup_unlink_pending == Some(step),
            "managed artifact unlink receipt lacks durable intent"
        );
        let mut advanced = authoritative.clone();
        advanced.cleanup_unlink_pending = None;
        match step {
            ManagedArtifactCleanupStep::Ownership => {
                ensure!(
                    !advanced.cleanup_socket_removed && !advanced.cleanup_runner_removed,
                    "managed ownership cleanup step is out of order"
                );
                advanced.cleanup_ownership_removed = true;
            }
            ManagedArtifactCleanupStep::Socket => {
                ensure!(
                    advanced.cleanup_ownership_removed && !advanced.cleanup_runner_removed,
                    "managed socket cleanup step is out of order"
                );
                advanced.cleanup_socket_removed = true;
            }
            ManagedArtifactCleanupStep::Runner => {
                ensure!(
                    advanced.cleanup_ownership_removed && advanced.cleanup_socket_removed,
                    "managed runner cleanup step is out of order"
                );
                advanced.cleanup_runner_removed = true;
            }
            ManagedArtifactCleanupStep::Directory => {
                bail!("managed directory unlink uses the completion receipt")
            }
        }
        let next = SlotRecord {
            artifact_binding: Some(Box::new(advanced.clone())),
            ..current.clone()
        };
        self.replace_slot(&current, &next)?;
        Ok(advanced)
    }

    #[cfg(target_os = "linux")]
    fn finish_artifact_logical_absence(
        &self,
        key: ManagedKey,
        binding: &ManagedArtifactBinding,
    ) -> Result<ManagedArtifactBinding> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        ensure!(
            matches!(current.state, SlotState::ResolvedTombstone { .. }),
            "managed logical artifact absence requires a resolved process tombstone"
        );
        let authoritative = current
            .artifact_binding
            .as_deref()
            .context("managed artifact cleanup binding is absent")?;
        ensure!(
            authoritative.same_artifact(binding),
            "managed logical artifact absence binding mismatch"
        );
        ensure!(
            !authoritative.creation_pending(),
            "managed artifact creation is unresolved"
        );
        let current_boot = match read_boot_uuid() {
            Evidence::Present(boot_uuid) => boot_uuid,
            Evidence::Absent | Evidence::Unavailable(_) => {
                bail!("current boot identity is unavailable")
            }
        };
        ensure!(
            authoritative.control_root.boot_uuid != current_boot,
            "managed logical artifact absence requires an old-boot binding"
        );
        if authoritative.cleanup_completed {
            return Ok(authoritative.clone());
        }
        let quarantine = authoritative.cleanup_quarantine.clone().unwrap_or_else(|| {
            format!(
                ".{}.cleanup-{}",
                authoritative.private_leaf,
                authoritative.nonce.simple()
            )
        });
        validate_managed_artifact_leaf(&quarantine)?;
        let completed = ManagedArtifactBinding {
            cleanup_quarantine: Some(quarantine),
            cleanup_unlink_pending: None,
            socket_retire_pending: false,
            socket_retired: authoritative.socket_retired || authoritative.socket.is_some(),
            cleanup_ownership_removed: true,
            cleanup_socket_removed: true,
            cleanup_runner_removed: true,
            cleanup_completed: true,
            ..authoritative.clone()
        };
        let next = SlotRecord {
            artifact_binding: Some(Box::new(completed.clone())),
            ..current.clone()
        };
        self.replace_slot(&current, &next)?;
        Ok(completed)
    }

    #[cfg(target_os = "linux")]
    fn finish_artifact_create_pending_absence(
        &self,
        key: ManagedKey,
        binding: &ManagedArtifactBinding,
    ) -> Result<ManagedArtifactBinding> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        ensure!(
            matches!(current.state, SlotState::ResolvedTombstone { .. }),
            "managed create-pending absence requires a resolved process tombstone"
        );
        let authoritative = current
            .artifact_binding
            .as_deref()
            .context("managed artifact cleanup binding is absent")?;
        ensure!(
            authoritative.same_artifact(binding)
                && authoritative.private_directory.is_none()
                && authoritative.runner.is_none()
                && authoritative.socket.is_none()
                && authoritative.cleanup_quarantine.is_some()
                && authoritative.cleanup_unlink_pending.is_none()
                && !authoritative.socket_retire_pending
                && !authoritative.socket_retired,
            "managed create-pending absence binding mismatch"
        );
        let current_boot = match read_boot_uuid() {
            Evidence::Present(boot_uuid) => boot_uuid,
            Evidence::Absent | Evidence::Unavailable(_) => {
                bail!("current boot identity is unavailable")
            }
        };
        ensure!(
            authoritative.control_root.boot_uuid == current_boot,
            "managed create-pending absence requires the current boot"
        );
        if authoritative.cleanup_completed {
            return Ok(authoritative.clone());
        }
        let completed = ManagedArtifactBinding {
            cleanup_ownership_removed: true,
            cleanup_socket_removed: true,
            cleanup_runner_removed: true,
            cleanup_completed: true,
            ..authoritative.clone()
        };
        let next = SlotRecord {
            artifact_binding: Some(Box::new(completed.clone())),
            ..current.clone()
        };
        self.replace_slot(&current, &next)?;
        Ok(completed)
    }

    #[cfg(target_os = "linux")]
    fn artifact_binding(
        &self,
        key: ManagedKey,
        expected_owner: &ManagedOwnerTag,
    ) -> Result<Option<ManagedArtifactBinding>> {
        expected_owner.validate()?;
        let current = self.read_valid_slot(key.slot)?;
        ensure!(current.generation == key.generation, "generation conflict");
        ensure!(
            matches!(current.state, SlotState::ResolvedTombstone { .. }),
            "managed artifact cleanup requires a resolved process tombstone"
        );
        ensure!(
            current.owner.as_ref() == Some(expected_owner),
            "managed artifact cleanup owner mismatch"
        );
        Ok(current.artifact_binding.as_deref().cloned())
    }

    fn read_registration(&self, slot: u16) -> Result<RegistrationRecord> {
        let path = self.registrations.join(registration_name(slot));
        let record = read_bounded_json::<RegistrationRecord>(&path)?;
        ensure!(record.schema_version == GATE_SCHEMA_VERSION);
        ensure!(record.slot == slot);
        Ok(record)
    }

    fn replace_registration(&self, slot: u16, next: &RegistrationRecord) -> Result<()> {
        ensure!(next.schema_version == GATE_SCHEMA_VERSION && next.slot == slot);
        if let Some(registration) = &next.registration {
            ensure!(registration.schema_version == GATE_SCHEMA_VERSION);
            ensure!(registration.slot == slot);
            ensure!(registration.generation == next.generation);
        }
        atomic_replace_json(&self.registrations, &registration_name(slot), next)?;
        ensure!(self.read_registration(slot)? == *next);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn record_identity(
        &self,
        intent: &SlotRecord,
        registration: &GateRegistration,
    ) -> Result<SlotRecord> {
        let SlotState::IntentDurable { nonce, .. } = intent.state else {
            bail!("slot is not IntentDurable");
        };
        ensure!(registration.slot == intent.slot);
        ensure!(registration.generation == intent.generation);
        ensure!(registration.nonce == nonce);
        let durable_registration = self.read_registration(intent.slot)?;
        ensure!(
            durable_registration.registration.as_ref() == Some(registration),
            "gate registration durable readback mismatch"
        );
        let identity = SlotRecord {
            schema_version: SLOT_SCHEMA_VERSION,
            slot: intent.slot,
            generation: intent.generation,
            owner: intent.owner.clone(),
            artifact_binding: intent.artifact_binding.clone(),
            state: SlotState::IdentityDurable {
                nonce,
                identity: registration.identity.clone(),
                // Conservative before COMMIT: a crash after the durable write
                // cannot prove whether the packet was delivered.
                release_may_have_occurred: true,
            },
        };
        self.replace_slot(intent, &identity)?;
        Ok(identity)
    }

    #[cfg(target_os = "linux")]
    fn reconcile_intent(&self, record: &SlotRecord) -> ReconcileOutcome {
        let SlotState::IntentDurable { nonce, .. } = record.state else {
            return ReconcileOutcome::UnknownOrphanRisk(ManagedReconcileCode::InvalidIntentState);
        };
        match OfdLock::try_acquire(&self.guards.join(guard_name(record.slot))) {
            Ok(_guard) => ReconcileOutcome::Absent,
            Err(_) => match self.read_registration(record.slot) {
                Ok(sidecar)
                    if sidecar.generation == record.generation
                        && sidecar.registration.as_ref().is_some_and(|registration| {
                            registration.nonce == nonce
                                && registration.slot == record.slot
                                && registration.generation == record.generation
                        }) =>
                {
                    let registration = sidecar.registration.expect("checked some");
                    match verify_exact_process(&registration.identity) {
                        Evidence::Present(_) => ReconcileOutcome::Live,
                        Evidence::Absent => ReconcileOutcome::UnknownOrphanRisk(
                            ManagedReconcileCode::BusyGuardIdentityAbsent,
                        ),
                        Evidence::Unavailable(_) => ReconcileOutcome::UnknownOrphanRisk(
                            ManagedReconcileCode::ProcessEvidenceUnavailable,
                        ),
                    }
                }
                Ok(_) | Err(_) => ReconcileOutcome::UnknownOrphanRisk(
                    ManagedReconcileCode::RegistrationUnavailable,
                ),
            },
        }
    }

    #[cfg(target_os = "linux")]
    fn cleanup(&self, slot: u16, generation: u64) -> Result<ReconcileOutcome> {
        let _registry_lock = OfdLock::try_acquire(&self.root.join("registry.lock"))
            .context("managed launch registry is busy")?;
        let current = self.read_valid_slot(slot)?;
        ensure!(current.generation == generation, "generation conflict");
        let (nonce, identity, release_may_have_occurred) = match &current.state {
            SlotState::IdentityDurable {
                nonce,
                identity,
                release_may_have_occurred,
            }
            | SlotState::CleanupPending {
                nonce,
                identity,
                release_may_have_occurred,
            } => (*nonce, identity.clone(), *release_may_have_occurred),
            SlotState::ResolvedTombstone { .. } => {
                return Ok(ReconcileOutcome::ResolvedTombstone);
            }
            SlotState::IntentDurable { nonce, .. } => {
                return match self.reconcile_intent(&current) {
                    ReconcileOutcome::Absent => self.persist_tombstone(&current, *nonce, None),
                    outcome => Ok(outcome),
                };
            }
            SlotState::Vacant => return Ok(ReconcileOutcome::Absent),
        };

        let pending = SlotRecord {
            state: SlotState::CleanupPending {
                nonce,
                identity: identity.clone(),
                release_may_have_occurred,
            },
            ..current.clone()
        };
        if current != pending {
            self.replace_slot(&current, &pending)?;
        }
        managed_test_failpoint("after_cleanup_pending");

        match open_verified_pidfd(&identity) {
            Evidence::Present(pidfd) => {
                pidfd.send_signal(libc::SIGKILL)?;
                managed_test_failpoint("after_cleanup_signal");
                pidfd.wait(Duration::from_secs(5))?;
                match verify_exact_process(&identity) {
                    Evidence::Absent => self.persist_tombstone(&pending, nonce, Some(identity)),
                    Evidence::Present(_) => Ok(ReconcileOutcome::UnknownOrphanRisk(
                        ManagedReconcileCode::ProcessStillLive,
                    )),
                    Evidence::Unavailable(_) => Ok(ReconcileOutcome::UnknownOrphanRisk(
                        ManagedReconcileCode::ProcessEvidenceUnavailable,
                    )),
                }
            }
            Evidence::Absent => self.persist_tombstone(&pending, nonce, Some(identity)),
            Evidence::Unavailable(_) => Ok(ReconcileOutcome::UnknownOrphanRisk(
                ManagedReconcileCode::ProcessEvidenceUnavailable,
            )),
        }
    }

    fn persist_tombstone(
        &self,
        current: &SlotRecord,
        nonce: Uuid,
        identity: Option<ProcessIdentity>,
    ) -> Result<ReconcileOutcome> {
        let tombstone = SlotRecord {
            state: SlotState::ResolvedTombstone {
                nonce,
                identity,
                resolved_unix_secs: now_unix_secs()?,
            },
            ..current.clone()
        };
        #[cfg(target_os = "linux")]
        managed_test_failpoint("before_tombstone");
        self.replace_slot(current, &tombstone)?;
        #[cfg(target_os = "linux")]
        managed_test_failpoint("after_tombstone");
        Ok(ReconcileOutcome::ResolvedTombstone)
    }

    #[cfg(target_os = "linux")]
    fn reconcile_all(&self) -> ManagedReconcileReport {
        let entries = (0..self.slot_count)
            .map(|slot| u16::try_from(slot).expect("validated registry slot count"))
            .filter_map(|slot| match self.read_slot(slot) {
                SlotRead::Valid(SlotRecord {
                    state: SlotState::Vacant,
                    ..
                }) => None,
                SlotRead::Valid(
                    record @ SlotRecord {
                        state: SlotState::ResolvedTombstone { .. },
                        ..
                    },
                ) => Some(ManagedReconcileEntry {
                    key: Some(ManagedKey {
                        slot,
                        generation: record.generation,
                    }),
                    owner: record.owner.clone(),
                    artifact_binding: record.artifact_binding.as_deref().cloned(),
                    outcome: ReconcileOutcome::ResolvedTombstone,
                }),
                SlotRead::Valid(record) => Some(ManagedReconcileEntry {
                    key: Some(ManagedKey {
                        slot,
                        generation: record.generation,
                    }),
                    owner: record.owner.clone(),
                    artifact_binding: record.artifact_binding.as_deref().cloned(),
                    outcome: self.cleanup(slot, record.generation).unwrap_or(
                        ReconcileOutcome::UnknownOrphanRisk(
                            ManagedReconcileCode::ReconciliationFailed,
                        ),
                    ),
                }),
                SlotRead::Unknown(_) => Some(ManagedReconcileEntry {
                    key: None,
                    owner: None,
                    artifact_binding: None,
                    outcome: ReconcileOutcome::UnknownOrphanRisk(ManagedReconcileCode::UnknownSlot),
                }),
            })
            .collect();
        ManagedReconcileReport { entries }
    }

    #[cfg(target_os = "linux")]
    fn reconcile_owner(&self, owner: &ManagedOwnerTag) -> Result<ManagedOwnerOutcome> {
        owner.validate()?;
        let mut matched = None;
        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).expect("validated registry slot count");
            let record = match self.read_slot(slot) {
                SlotRead::Valid(record) => record,
                SlotRead::Unknown(_) => {
                    return Ok(ManagedOwnerOutcome::UnknownOrphanRisk(
                        ManagedReconcileCode::UnknownSlot,
                    ));
                }
            };
            if record.owner.as_ref() == Some(owner) {
                if matched.is_some() {
                    return Ok(ManagedOwnerOutcome::UnknownOrphanRisk(
                        ManagedReconcileCode::DuplicateOwner,
                    ));
                }
                matched = Some(record);
            }
        }
        let Some(record) = matched else {
            return Ok(ManagedOwnerOutcome::Absent);
        };
        let key = ManagedKey {
            slot: record.slot,
            generation: record.generation,
        };
        match record.state {
            SlotState::ResolvedTombstone { .. } => Ok(ManagedOwnerOutcome::ResolvedTombstone(key)),
            SlotState::Vacant => Ok(ManagedOwnerOutcome::Absent),
            _ => match self.cleanup(record.slot, record.generation) {
                Err(_) => Ok(ManagedOwnerOutcome::UnknownOrphanRisk(
                    ManagedReconcileCode::ReconciliationFailed,
                )),
                Ok(outcome) => match outcome {
                    ReconcileOutcome::ResolvedTombstone => {
                        Ok(ManagedOwnerOutcome::ResolvedTombstone(key))
                    }
                    ReconcileOutcome::Absent => Ok(ManagedOwnerOutcome::Absent),
                    ReconcileOutcome::Live => Ok(ManagedOwnerOutcome::UnknownOrphanRisk(
                        ManagedReconcileCode::ProcessStillLive,
                    )),
                    ReconcileOutcome::UnknownOrphanRisk(reason) => {
                        Ok(ManagedOwnerOutcome::UnknownOrphanRisk(reason))
                    }
                },
            },
        }
    }
}

#[cfg(all(target_os = "linux", not(test)))]
fn managed_process_registry_path() -> Result<PathBuf> {
    if let Some(path) = MANAGED_PROCESS_REGISTRY_PATH.get() {
        return Ok(path.clone());
    }
    let path = crate::paths::process_registry_dir()?;
    let _ = MANAGED_PROCESS_REGISTRY_PATH.set(path.clone());
    ensure!(
        MANAGED_PROCESS_REGISTRY_PATH.get() == Some(&path),
        "managed process registry path changed after initialization"
    );
    Ok(path)
}

#[cfg(all(target_os = "linux", test))]
fn managed_process_registry_path() -> Result<PathBuf> {
    crate::paths::process_registry_dir()
}

#[cfg(target_os = "linux")]
pub(crate) fn initialize_managed_process_registry_path(path: &Path) -> Result<()> {
    #[cfg(not(test))]
    {
        if let Some(current) = MANAGED_PROCESS_REGISTRY_PATH.get() {
            ensure!(current == path, "managed process registry path changed");
        } else {
            let _ = MANAGED_PROCESS_REGISTRY_PATH.set(path.to_path_buf());
            ensure!(
                MANAGED_PROCESS_REGISTRY_PATH.get().map(PathBuf::as_path) == Some(path),
                "managed process registry path initialization raced"
            );
        }
    }
    #[cfg(test)]
    let _ = path;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn reconcile_managed_processes() -> Result<ManagedReconcileReport> {
    let queue = managed_reaper_queue();
    ensure_managed_reaper_supervisor(&queue)?;
    queue.changed.notify_all();
    Ok(Registry::open_default()?.reconcile_all())
}

#[cfg(target_os = "linux")]
pub(crate) fn reconcile_managed_processes_at(path: &Path) -> Result<ManagedReconcileReport> {
    let queue = managed_reaper_queue();
    ensure_managed_reaper_supervisor(&queue)?;
    queue.changed.notify_all();
    Ok(Registry::open_at(path.to_path_buf(), SLOT_COUNT)?.reconcile_all())
}

#[cfg(target_os = "linux")]
pub(crate) fn reconcile_managed_owner(owner: &ManagedOwnerTag) -> Result<ManagedOwnerOutcome> {
    let queue = managed_reaper_queue();
    ensure_managed_reaper_supervisor(&queue)?;
    queue.changed.notify_all();
    Registry::open_default()?.reconcile_owner(owner)
}

#[cfg(target_os = "linux")]
pub(crate) fn acknowledge_managed_artifact_cleanup(
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
) -> Result<()> {
    binding.validate()?;
    Registry::open_default()?.acknowledge_artifact_cleanup(key, binding)
}

#[cfg(target_os = "linux")]
pub(crate) fn begin_managed_artifact_cleanup(
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
) -> Result<ManagedArtifactBinding> {
    binding.validate()?;
    Registry::open_default()?.begin_artifact_cleanup(key, binding)
}

#[cfg(target_os = "linux")]
pub(crate) fn begin_managed_artifact_socket_retirement(
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
) -> Result<ManagedArtifactBinding> {
    binding.validate()?;
    Registry::open_default()?.begin_artifact_socket_retirement(key, binding)
}

#[cfg(target_os = "linux")]
pub(crate) fn finish_managed_artifact_socket_retirement(
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
) -> Result<ManagedArtifactBinding> {
    binding.validate()?;
    Registry::open_default()?.finish_artifact_socket_retirement(key, binding)
}

#[cfg(target_os = "linux")]
pub(crate) fn finish_managed_artifact_cleanup(
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
) -> Result<ManagedArtifactBinding> {
    binding.validate()?;
    Registry::open_default()?.finish_artifact_cleanup(key, binding)
}

#[cfg(target_os = "linux")]
pub(crate) fn begin_managed_artifact_unlink(
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
    step: ManagedArtifactCleanupStep,
) -> Result<ManagedArtifactBinding> {
    binding.validate()?;
    Registry::open_default()?.begin_artifact_unlink(key, binding, step)
}

#[cfg(target_os = "linux")]
pub(crate) fn finish_managed_artifact_unlink(
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
    step: ManagedArtifactCleanupStep,
) -> Result<ManagedArtifactBinding> {
    binding.validate()?;
    Registry::open_default()?.finish_artifact_unlink(key, binding, step)
}

#[cfg(target_os = "linux")]
pub(crate) fn finish_managed_artifact_logical_absence(
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
) -> Result<ManagedArtifactBinding> {
    binding.validate()?;
    Registry::open_default()?.finish_artifact_logical_absence(key, binding)
}

#[cfg(target_os = "linux")]
pub(crate) fn finish_managed_artifact_create_pending_absence(
    key: ManagedKey,
    binding: &ManagedArtifactBinding,
) -> Result<ManagedArtifactBinding> {
    binding.validate()?;
    Registry::open_default()?.finish_artifact_create_pending_absence(key, binding)
}

#[cfg(target_os = "linux")]
pub(crate) fn read_managed_artifact_binding(
    key: ManagedKey,
    expected_owner: &ManagedOwnerTag,
) -> Result<Option<ManagedArtifactBinding>> {
    Registry::open_default()?.artifact_binding(key, expected_owner)
}

#[cfg(target_os = "linux")]
pub(crate) fn reserve_managed_launch(
    owner: Option<ManagedOwnerTag>,
) -> Result<ManagedLaunchReservation> {
    if let Some(owner) = &owner {
        owner.validate()?;
    }
    let queue = managed_reaper_queue();
    ensure_managed_reaper_supervisor(&queue)?;
    let registry = Registry::open_default()?;
    managed_test_failpoint("parent_before_intent");
    let intent = registry.allocate_intent(now_unix_secs()?, owner)?;
    managed_test_failpoint("parent_after_intent");
    Ok(ManagedLaunchReservation {
        registry,
        intent: Some(intent),
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn abort_managed_launch_reservation(
    reservation: ManagedLaunchReservation,
    lifetime_guard: ManagedLifetimeGuard,
) {
    let (registry, intent) = reservation.into_parts();
    let slot = intent.record.slot;
    let generation = intent.record.generation;
    drop(intent);
    let queue = managed_reaper_queue();
    enqueue_managed_reap_job(ManagedReapJob::new(
        None,
        ManagedCleanupOwner::FailedLaunch {
            registry,
            slot,
            generation,
        },
        Some(lifetime_guard),
    ));
    let _ = ensure_managed_reaper_supervisor(&queue);
    queue.changed.notify_all();
}

fn validate_v1_slot_json(value: &serde_json::Value) -> Result<()> {
    let object = value
        .as_object()
        .context("legacy managed slot is not a JSON object")?;
    let state = object
        .get("state")
        .and_then(serde_json::Value::as_str)
        .context("legacy managed slot state is missing")?;
    let state_field = |key: &str| match state {
        "vacant" => false,
        "intent_durable" => matches!(key, "nonce" | "created_unix_secs"),
        "identity_durable" | "cleanup_pending" => {
            matches!(key, "nonce" | "identity" | "release_may_have_occurred")
        }
        "resolved_tombstone" => {
            matches!(key, "nonce" | "identity" | "resolved_unix_secs")
        }
        _ => false,
    };
    ensure!(
        matches!(
            state,
            "vacant"
                | "intent_durable"
                | "identity_durable"
                | "cleanup_pending"
                | "resolved_tombstone"
        ),
        "legacy managed slot state is unsupported"
    );
    ensure!(
        object.keys().all(|key| {
            matches!(
                key.as_str(),
                "schema_version" | "slot" | "generation" | "owner" | "state"
            ) || state_field(key)
        }),
        "legacy managed slot contains unknown fields"
    );
    Ok(())
}

fn validate_v1_slot_record(record: &SlotRecordV1, expected_slot: u16) -> Result<()> {
    ensure!(
        record.schema_version == 1,
        "legacy managed slot schema mismatch"
    );
    ensure!(record.slot == expected_slot);
    if let Some(owner) = &record.owner {
        owner.validate()?;
    }
    match &record.state {
        SlotState::Vacant => ensure!(record.owner.is_none(), "vacant v1 slot retained an owner"),
        SlotState::IntentDurable { nonce, .. }
        | SlotState::IdentityDurable { nonce, .. }
        | SlotState::CleanupPending { nonce, .. }
        | SlotState::ResolvedTombstone { nonce, .. } => ensure!(!nonce.is_nil()),
    }
    Ok(())
}

fn validate_slot_record(record: &SlotRecord, expected_slot: u16) -> Result<()> {
    ensure!(record.schema_version == SLOT_SCHEMA_VERSION);
    ensure!(record.slot == expected_slot);
    if let Some(owner) = &record.owner {
        owner.validate()?;
    }
    if let Some(binding) = &record.artifact_binding {
        binding.validate()?;
        ensure!(
            record.owner.is_some(),
            "managed artifact binding has no owner"
        );
        let state_nonce = match &record.state {
            SlotState::Vacant => bail!("vacant slot retained an artifact binding"),
            SlotState::IntentDurable { nonce, .. }
            | SlotState::IdentityDurable { nonce, .. }
            | SlotState::CleanupPending { nonce, .. }
            | SlotState::ResolvedTombstone { nonce, .. } => *nonce,
        };
        ensure!(
            binding.nonce == state_nonce,
            "managed artifact nonce mismatch"
        );
        ensure!(
            binding.cleanup_quarantine.is_none()
                || matches!(record.state, SlotState::ResolvedTombstone { .. }),
            "managed artifact cleanup phase exists before process tombstone"
        );
    }
    match &record.state {
        SlotState::Vacant => ensure!(
            record.owner.is_none() && record.artifact_binding.is_none(),
            "vacant slot retained an owner or artifact binding"
        ),
        SlotState::IntentDurable { nonce, .. }
        | SlotState::IdentityDurable { nonce, .. }
        | SlotState::CleanupPending { nonce, .. }
        | SlotState::ResolvedTombstone { nonce, .. } => ensure!(!nonce.is_nil()),
    }
    Ok(())
}

fn validate_transition(current: &SlotRecord, next: &SlotRecord) -> Result<()> {
    ensure!(current.schema_version == SLOT_SCHEMA_VERSION);
    ensure!(next.schema_version == SLOT_SCHEMA_VERSION);
    ensure!(current.slot == next.slot);
    let mut binding_only = current.clone();
    binding_only.artifact_binding = next.artifact_binding.clone();
    let binding_update = binding_only == *next
        && match (&current.artifact_binding, &next.artifact_binding) {
            (None, Some(_)) => matches!(current.state, SlotState::IntentDurable { .. }),
            (Some(before), Some(after)) => {
                let same_artifact_identity = before.nonce == after.nonce
                    && before.control_root == after.control_root
                    && before.private_leaf == after.private_leaf;
                let creation_progress = matches!(current.state, SlotState::IntentDurable { .. })
                    && before.cleanup_quarantine.is_none()
                    && after.cleanup_quarantine.is_none()
                    && before.cleanup_unlink_pending.is_none()
                    && after.cleanup_unlink_pending.is_none()
                    && !before.cleanup_ownership_removed
                    && !after.cleanup_ownership_removed
                    && !before.cleanup_socket_removed
                    && !after.cleanup_socket_removed
                    && !before.cleanup_runner_removed
                    && !after.cleanup_runner_removed
                    && !before.cleanup_completed
                    && !after.cleanup_completed
                    && !before.socket_retire_pending
                    && !after.socket_retire_pending
                    && !before.socket_retired
                    && !after.socket_retired
                    && before.creation_progresses_to(after);
                let socket_retire_intent = before.same_artifact(after)
                    && before.cleanup_quarantine.is_none()
                    && after.cleanup_quarantine.is_none()
                    && !before.socket_retire_pending
                    && after.socket_retire_pending
                    && !before.socket_retired
                    && !after.socket_retired
                    && before.cleanup_unlink_pending == after.cleanup_unlink_pending
                    && before.cleanup_ownership_removed == after.cleanup_ownership_removed
                    && before.cleanup_socket_removed == after.cleanup_socket_removed
                    && before.cleanup_runner_removed == after.cleanup_runner_removed
                    && before.cleanup_completed == after.cleanup_completed;
                let socket_retire_receipt = before.same_artifact(after)
                    && before.cleanup_quarantine.is_none()
                    && after.cleanup_quarantine.is_none()
                    && before.socket_retire_pending
                    && !after.socket_retire_pending
                    && !before.socket_retired
                    && after.socket_retired
                    && before.cleanup_unlink_pending == after.cleanup_unlink_pending
                    && before.cleanup_ownership_removed == after.cleanup_ownership_removed
                    && before.cleanup_socket_removed == after.cleanup_socket_removed
                    && before.cleanup_runner_removed == after.cleanup_runner_removed
                    && before.cleanup_completed == after.cleanup_completed;
                let socket_retirement_unchanged = before.socket_retire_pending
                    == after.socket_retire_pending
                    && before.socket_retired == after.socket_retired;
                let cleanup_progress = matches!(current.state, SlotState::ResolvedTombstone { .. })
                    && before.private_directory == after.private_directory
                    && before.runner == after.runner
                    && before.socket == after.socket
                    && before.owner == after.owner
                    && before.runner_create_pending == after.runner_create_pending
                    && before.socket_create_pending == after.socket_create_pending
                    && before.owner_create_pending == after.owner_create_pending
                    && before.cleanup_quarantine.is_none()
                    && after.cleanup_quarantine.is_some()
                    && before.cleanup_unlink_pending.is_none()
                    && after.cleanup_unlink_pending.is_none()
                    && !before.cleanup_ownership_removed
                    && !after.cleanup_ownership_removed
                    && !before.cleanup_socket_removed
                    && !after.cleanup_socket_removed
                    && !before.cleanup_runner_removed
                    && !after.cleanup_runner_removed
                    && !before.cleanup_completed
                    && !after.cleanup_completed;
                let cleanup_step_intent =
                    matches!(current.state, SlotState::ResolvedTombstone { .. })
                        && before.same_artifact(after)
                        && before.cleanup_quarantine == after.cleanup_quarantine
                        && before.cleanup_quarantine.is_some()
                        && !before.cleanup_completed
                        && !after.cleanup_completed
                        && before.cleanup_unlink_pending.is_none()
                        && after.cleanup_unlink_pending.is_some()
                        && before.cleanup_ownership_removed == after.cleanup_ownership_removed
                        && before.cleanup_socket_removed == after.cleanup_socket_removed
                        && before.cleanup_runner_removed == after.cleanup_runner_removed;
                let cleanup_step_receipt =
                    matches!(current.state, SlotState::ResolvedTombstone { .. })
                        && before.same_artifact(after)
                        && before.cleanup_quarantine == after.cleanup_quarantine
                        && before.cleanup_quarantine.is_some()
                        && !before.cleanup_completed
                        && !after.cleanup_completed
                        && before.cleanup_unlink_pending.is_some()
                        && after.cleanup_unlink_pending.is_none()
                        && matches!(
                            (
                                before.cleanup_unlink_pending,
                                before.cleanup_ownership_removed,
                                before.cleanup_socket_removed,
                                before.cleanup_runner_removed,
                                after.cleanup_ownership_removed,
                                after.cleanup_socket_removed,
                                after.cleanup_runner_removed,
                            ),
                            (
                                Some(ManagedArtifactCleanupStep::Ownership),
                                false,
                                false,
                                false,
                                true,
                                false,
                                false
                            ) | (
                                Some(ManagedArtifactCleanupStep::Socket),
                                true,
                                false,
                                false,
                                true,
                                true,
                                false
                            ) | (
                                Some(ManagedArtifactCleanupStep::Runner),
                                true,
                                true,
                                false,
                                true,
                                true,
                                true
                            )
                        );
                let cleanup_completion =
                    matches!(current.state, SlotState::ResolvedTombstone { .. })
                        && before.same_artifact(after)
                        && before.cleanup_quarantine == after.cleanup_quarantine
                        && before.cleanup_quarantine.is_some()
                        && before.cleanup_ownership_removed
                        && before.cleanup_socket_removed
                        && before.cleanup_runner_removed
                        && after.cleanup_ownership_removed
                        && after.cleanup_socket_removed
                        && after.cleanup_runner_removed
                        && before.cleanup_unlink_pending
                            == Some(ManagedArtifactCleanupStep::Directory)
                        && after.cleanup_unlink_pending.is_none()
                        && !before.cleanup_completed
                        && after.cleanup_completed;
                let logical_absence_completion =
                    matches!(current.state, SlotState::ResolvedTombstone { .. })
                        && before.same_artifact(after)
                        && before.cleanup_quarantine == after.cleanup_quarantine
                        && before.cleanup_quarantine.is_some()
                        && before.cleanup_unlink_pending.is_none()
                        && !before.cleanup_ownership_removed
                        && !before.cleanup_socket_removed
                        && !before.cleanup_runner_removed
                        && !before.cleanup_completed
                        && after.cleanup_unlink_pending.is_none()
                        && after.cleanup_ownership_removed
                        && after.cleanup_socket_removed
                        && after.cleanup_runner_removed
                        && after.cleanup_completed;
                let old_boot_logical_absence_completion =
                    matches!(current.state, SlotState::ResolvedTombstone { .. })
                        && before.same_artifact(after)
                        && after.cleanup_quarantine.is_some()
                        && after.cleanup_unlink_pending.is_none()
                        && !after.socket_retire_pending
                        && after.socket_retired
                            == (before.socket_retired || before.socket.is_some())
                        && after.cleanup_ownership_removed
                        && after.cleanup_socket_removed
                        && after.cleanup_runner_removed
                        && !before.cleanup_completed
                        && after.cleanup_completed;
                same_artifact_identity
                    && (creation_progress
                        || socket_retire_intent
                        || socket_retire_receipt
                        || old_boot_logical_absence_completion
                        || (socket_retirement_unchanged
                            && (cleanup_progress
                                || cleanup_step_intent
                                || cleanup_step_receipt
                                || cleanup_completion
                                || logical_absence_completion)))
            }
            (Some(before), None) => {
                matches!(current.state, SlotState::ResolvedTombstone { .. })
                    && before.cleanup_quarantine.is_some()
                    && before.cleanup_completed
            }
            _ => false,
        };
    let legal = binding_update
        || match (&current.state, &next.state) {
            (SlotState::Vacant, SlotState::IntentDurable { .. }) => current
                .generation
                .checked_add(1)
                .is_some_and(|generation| generation == next.generation),
            (
                SlotState::IntentDurable { nonce: a, .. },
                SlotState::IdentityDurable { nonce: b, .. },
            )
            | (
                SlotState::IdentityDurable { nonce: a, .. },
                SlotState::CleanupPending { nonce: b, .. },
            )
            | (
                SlotState::CleanupPending { nonce: a, .. },
                SlotState::ResolvedTombstone { nonce: b, .. },
            ) => current.generation == next.generation && a == b,
            (
                SlotState::IntentDurable { nonce: a, .. },
                SlotState::ResolvedTombstone { nonce: b, .. },
            ) => current.generation == next.generation && a == b,
            (SlotState::ResolvedTombstone { .. }, SlotState::Vacant) => {
                current.generation == next.generation
                    && current.artifact_binding.is_none()
                    && next.owner.is_none()
                    && next.artifact_binding.is_none()
            }
            // Idempotent durable readback/rewrite is permitted only for the same
            // full record, never as a state shortcut.
            _ => current == next,
        };
    let owner_stable = match (&current.state, &next.state) {
        (SlotState::Vacant, SlotState::IntentDurable { .. }) => {
            if let Some(owner) = &next.owner {
                owner.validate()?;
            }
            true
        }
        (SlotState::ResolvedTombstone { .. }, SlotState::Vacant) => next.owner.is_none(),
        _ => current.owner == next.owner,
    };
    ensure!(owner_stable, "managed owner changed across slot transition");
    ensure!(
        binding_update || current.artifact_binding == next.artifact_binding,
        "managed artifact binding changed across slot transition"
    );
    ensure!(legal, "illegal managed registry state transition");
    Ok(())
}

fn slot_name(slot: u16) -> String {
    format!("slot-{slot:04}.json")
}

fn guard_name(slot: u16) -> String {
    format!("guard-{slot:04}")
}

fn registration_name(slot: u16) -> String {
    format!("registration-{slot:04}.json")
}

fn now_unix_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

fn create_exact_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create directory {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    validate_exact_dir(path)
}

fn ensure_exact_private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        let parent = path.parent().context("private directory has no parent")?;
        if !parent.exists() {
            ensure_exact_private_dir(parent)?;
        }
        create_exact_dir(path)?;
        sync_dir(parent)?;
    }
    validate_exact_dir(path)
}

fn create_exact_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    ensure!(bytes.len() <= MAX_RECORD_BYTES);
    create_exact_file(path, &bytes)
}

fn create_exact_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    validate_file_handle(&file, path)
}

fn atomic_replace_json<T: Serialize>(directory: &Path, name: &str, value: &T) -> Result<()> {
    validate_exact_dir(directory)?;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "record exceeds 8 KiB bound"
    );
    let temp_name = format!(".{name}.{}.tmp", Uuid::new_v4());
    let temp = directory.join(&temp_name);
    create_exact_file(&temp, &bytes)?;
    validate_exact_file(&temp)?;
    fs::rename(&temp, directory.join(name))?;
    sync_dir(directory)?;
    Ok(())
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = open_exact_file(path, false)?;
    let length = file.metadata()?.len();
    ensure!(
        length <= MAX_RECORD_BYTES as u64,
        "record exceeds 8 KiB bound"
    );
    let mut bytes = Vec::with_capacity(length as usize);
    file.take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "record exceeds 8 KiB bound"
    );
    serde_json::from_slice(&bytes).context("parse durable JSON record")
}

fn open_exact_file(path: &Path, write: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.write(write);
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    validate_file_handle(&file, path)?;
    Ok(file)
}

fn validate_exact_file(path: &Path) -> Result<()> {
    let _ = open_exact_file(path, false)?;
    Ok(())
}

fn validate_file_handle(file: &File, path: &Path) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    ensure!(metadata.uid() == crate::paths::current_euid());
    ensure!(
        metadata.nlink() == 1,
        "{} must have nlink=1",
        path.display()
    );
    ensure!(metadata.permissions().mode() & 0o777 == 0o600);
    Ok(())
}

fn validate_exact_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(!metadata.file_type().is_symlink());
    ensure!(metadata.is_dir());
    ensure!(metadata.uid() == crate::paths::current_euid());
    ensure!(metadata.permissions().mode() & 0o777 == 0o700);
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.sync_all()?;
    Ok(())
}

struct TempTree {
    path: PathBuf,
    armed: std::cell::Cell<bool>,
}

impl TempTree {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            armed: std::cell::Cell::new(true),
        }
    }

    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        if self.armed.get() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn rename_exchange(left: &Path, right: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let left = CString::new(left.as_os_str().as_bytes())?;
    let right = CString::new(right.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    // Non-Linux builds never launch managed processes. This fallback exists so
    // platform-neutral durability/state tests can exercise the file model.
    fs::rename(from, to)
}

#[cfg(not(target_os = "linux"))]
fn rename_exchange(_left: &Path, _right: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic managed slot generation exchange is unavailable on this platform",
    ))
}

#[cfg(target_os = "linux")]
impl OfdLock {
    fn try_acquire(path: &Path) -> Result<Self> {
        let file = open_exact_file(path, true)?;
        let mut lock = libc::flock {
            l_type: libc::F_WRLCK as i16,
            l_whence: libc::SEEK_SET as i16,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &mut lock) };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("acquire exclusive OFD lock");
        }
        Ok(Self { file })
    }
}

#[cfg(target_os = "linux")]
fn read_boot_uuid() -> Evidence<Uuid> {
    match fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        Ok(text) => match Uuid::parse_str(text.trim()) {
            Ok(uuid) => Evidence::Present(uuid),
            Err(err) => Evidence::Unavailable(format!("invalid boot UUID: {err}")),
        },
        Err(err) => Evidence::Unavailable(format!("boot UUID unavailable: {err}")),
    }
}

#[cfg(target_os = "linux")]
fn read_pid_namespace_inode(pid: u32) -> Evidence<u64> {
    match fs::metadata(format!("/proc/{pid}/ns/pid")) {
        Ok(metadata) => Evidence::Present(metadata.ino()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Evidence::Absent,
        Err(err) => Evidence::Unavailable(format!("PID namespace unavailable: {err}")),
    }
}

fn parse_proc_stat_start_ticks(stat: &str) -> Result<u64> {
    let close = stat
        .rfind(')')
        .context("proc stat has no closing command delimiter")?;
    let rest = stat
        .get(close + 1..)
        .context("proc stat delimiter is invalid")?;
    let value = rest
        .split_whitespace()
        .nth(19)
        .context("proc stat is missing field 22")?;
    value.parse().context("proc stat field 22 is invalid")
}

#[cfg(target_os = "linux")]
fn read_start_ticks(pid: u32) -> Evidence<u64> {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match parse_proc_stat_start_ticks(&stat) {
            Ok(ticks) => Evidence::Present(ticks),
            Err(err) => Evidence::Unavailable(format!("start ticks unavailable: {err:#}")),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Evidence::Absent,
        Err(err) => Evidence::Unavailable(format!("start ticks unavailable: {err}")),
    }
}

#[cfg(target_os = "linux")]
fn observe_identity(pid: u32) -> Evidence<ProcessIdentity> {
    let boot_uuid = match read_boot_uuid() {
        Evidence::Present(value) => value,
        Evidence::Absent => return Evidence::Unavailable("boot UUID unexpectedly absent".into()),
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    };
    let pid_namespace_inode = match read_pid_namespace_inode(pid) {
        Evidence::Present(value) => value,
        Evidence::Absent => return Evidence::Absent,
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    };
    let start_ticks = match read_start_ticks(pid) {
        Evidence::Present(value) => value,
        Evidence::Absent => return Evidence::Absent,
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    };
    Evidence::Present(ProcessIdentity {
        boot_uuid,
        pid_namespace_inode,
        pid,
        start_ticks,
    })
}

#[cfg(target_os = "linux")]
fn verify_exact_process(expected: &ProcessIdentity) -> Evidence<ProcessIdentity> {
    match read_boot_uuid() {
        Evidence::Present(boot) if boot != expected.boot_uuid => return Evidence::Absent,
        Evidence::Present(_) => {}
        Evidence::Absent => return Evidence::Unavailable("boot UUID unexpectedly absent".into()),
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    }
    match open_verified_pidfd(expected) {
        Evidence::Present(_pidfd) => Evidence::Present(expected.clone()),
        Evidence::Absent => Evidence::Absent,
        Evidence::Unavailable(reason) => Evidence::Unavailable(reason),
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct PidFd(File);

#[cfg(target_os = "linux")]
impl PidFd {
    fn open(pid: u32) -> Evidence<Self> {
        let result = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if result >= 0 {
            use std::os::fd::FromRawFd;
            return Evidence::Present(Self(unsafe { File::from_raw_fd(result as RawFd) }));
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Evidence::Absent
        } else {
            Evidence::Unavailable(format!("pidfd_open unavailable: {err}"))
        }
    }

    fn send_signal(&self, signal: i32) -> Result<()> {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(err).context("pidfd_send_signal")
            }
        }
    }

    fn wait(&self, timeout: Duration) -> Result<()> {
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut pollfd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if result < 0 {
            Err(std::io::Error::last_os_error()).context("poll pidfd")
        } else if result == 0 {
            bail!("timed out waiting for pidfd readiness")
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn open_verified_pidfd(expected: &ProcessIdentity) -> Evidence<PidFd> {
    let pidfd = match PidFd::open(expected.pid) {
        Evidence::Present(pidfd) => pidfd,
        Evidence::Absent => return Evidence::Absent,
        Evidence::Unavailable(reason) => return Evidence::Unavailable(reason),
    };
    // The reread happens after pidfd_open. A mismatch proves that the recorded
    // incarnation is absent, but the current PID must never be signalled.
    match observe_identity(expected.pid) {
        Evidence::Present(actual) if actual == *expected => Evidence::Present(pidfd),
        Evidence::Present(_) | Evidence::Absent => Evidence::Absent,
        Evidence::Unavailable(reason) => Evidence::Unavailable(reason),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn launch_managed_process(
    mut request: ManagedLaunchRequest,
) -> std::result::Result<ManagedLaunch, ManagedLaunchFailure> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let reservation = match request.reservation.take() {
        Some(reservation) => reservation,
        None => {
            if let Some(owner) = &request.owner {
                owner.validate()?;
            }
            reserve_managed_launch(request.owner.clone())?
        }
    };
    if let Some(owner) = &request.owner
        && let Err(error) = owner.validate()
    {
        let (registry, intent) = reservation.into_parts();
        return Err(ManagedLaunchFailure::resolved(resolve_unspawned_intent(
            registry,
            intent,
            request.lifetime_guard.take(),
            error,
        )));
    }
    if let Err(error) = ensure_managed_reaper_supervisor(&managed_reaper_queue()) {
        let (registry, intent) = reservation.into_parts();
        return Err(ManagedLaunchFailure::resolved(resolve_unspawned_intent(
            registry,
            intent,
            request.lifetime_guard.take(),
            error,
        )));
    }
    if reservation.owner() != request.owner.as_ref() {
        let (registry, intent) = reservation.into_parts();
        return Err(ManagedLaunchFailure::resolved(resolve_unspawned_intent(
            registry,
            intent,
            request.lifetime_guard.take(),
            anyhow::anyhow!("managed launch reservation owner mismatch"),
        )));
    }
    let (registry, intent) = reservation.into_parts();
    if request.executable_policy == ManagedExecutablePolicy::PinnedSystemBwrap
        && request.stdio != ManagedStdioPolicy::Null
    {
        return Err(ManagedLaunchFailure::resolved(resolve_unspawned_intent(
            registry,
            intent,
            request.lifetime_guard.take(),
            anyhow::anyhow!("pinned bwrap requires closed managed stdio"),
        )));
    }
    let SlotState::IntentDurable { nonce, .. } = &intent.record.state else {
        unreachable!("allocator returns intent")
    };
    let nonce = *nonce;
    let target = match open_pinned_executable(&request.executable, request.executable_policy) {
        Ok(target) => target,
        Err(err) => {
            return Err(resolve_unspawned_intent(
                registry,
                intent,
                request.lifetime_guard.take(),
                err,
            )
            .into());
        }
    };
    let (parent_control, child_control) = match seqpacket_pair() {
        Ok(pair) => pair,
        Err(err) => {
            return Err(resolve_unspawned_intent(
                registry,
                intent,
                request.lifetime_guard.take(),
                err,
            )
            .into());
        }
    };
    let guard_fd = intent.guard.file.as_raw_fd();
    let control_fd = child_control.as_raw_fd();
    let placement = match request.placement {
        ManagedPlacement::None => None,
        ManagedPlacement::CgroupV2(placement) => Some(placement),
    };
    let placement_fd = placement
        .as_ref()
        .map(|placement| placement.cgroup_procs.as_raw_fd());

    let mut command = Command::new("/proc/self/exe");
    command
        .arg(INTERNAL_GATE_ARG)
        .arg(&registry.root)
        .arg(intent.record.slot.to_string())
        .arg(intent.record.generation.to_string())
        .arg(nonce.to_string())
        .arg(if placement.is_some() {
            "cgroup_v2"
        } else {
            "none"
        })
        .arg(request.executable.as_os_str())
        .args(&request.arguments)
        .env_clear()
        .envs(request.environment);
    if request.stdio == ManagedStdioPolicy::Null {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    if let Some(current_dir) = request.current_dir {
        command.current_dir(current_dir);
    }
    unsafe {
        command.pre_exec(move || remap_gate_fds(control_fd, guard_fd, placement_fd));
    }
    let child = match command.spawn().context("spawn trusted managed launch gate") {
        Ok(child) => child,
        Err(err) => {
            return Err(resolve_unspawned_intent(
                registry,
                intent,
                request.lifetime_guard.take(),
                err,
            )
            .into());
        }
    };
    let mut waiter = ManagedWaiter {
        child: Some(child),
        cleanup: ManagedCleanupOwner::FailedLaunch {
            registry: registry.clone(),
            slot: intent.record.slot,
            generation: intent.record.generation,
        },
        lifetime_guard: request.lifetime_guard.take(),
    };
    managed_test_failpoint("parent_after_spawn");
    drop(child_control);
    let launch_result = (|| -> Result<ProcessIdentity> {
        set_socket_timeout(parent_control.as_raw_fd(), Duration::from_secs(10))?;
        managed_test_errorpoint("parent_after_spawn")?;
        let hello: GateHello = recv_packet(parent_control.as_raw_fd())?;
        ensure!(hello.protocol == "lterm-managed-hello-v1");
        ensure!(hello.registration.slot == intent.record.slot);
        ensure!(hello.registration.generation == intent.record.generation);
        ensure!(hello.registration.nonce == nonce);
        ensure!(
            hello.registration.identity.pid
                == waiter
                    .child
                    .as_ref()
                    .context("managed child vanished during launch")?
                    .id()
        );
        if let Some(placement) = &placement {
            verify_process_cgroup_membership(
                hello.registration.identity.pid,
                placement.expected_membership.normalized_path(),
            )?;
        }
        managed_test_failpoint("parent_after_hello");
        let verified = match open_verified_pidfd(&hello.registration.identity) {
            Evidence::Present(pidfd) => pidfd,
            Evidence::Absent => bail!("managed gate exited before identity promotion"),
            Evidence::Unavailable(reason) => bail!("managed gate identity unavailable: {reason}"),
        };
        drop(verified);
        let identity_record = registry.record_identity(&intent.record, &hello.registration)?;
        managed_test_failpoint("parent_after_identity");
        let SlotState::IdentityDurable { identity, .. } = &identity_record.state else {
            unreachable!()
        };
        let commit = GateCommit {
            protocol: "lterm-managed-commit-v1".into(),
            slot: intent.record.slot,
            generation: intent.record.generation,
            nonce,
            identity: identity.clone(),
            descriptors: match &request.auxiliary {
                ManagedAuxiliary::None => vec![CommitDescriptor {
                    role: CommitDescriptorRole::TargetExecutable,
                    target_fd: None,
                    directory_identity: None,
                }],
                ManagedAuxiliary::SyncPipeWrite(sync) => vec![
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                        directory_identity: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::SyncPipeWrite,
                        target_fd: Some(sync.target_fd),
                        directory_identity: None,
                    },
                ],
                ManagedAuxiliary::Speculation {
                    sync_pipe,
                    pinned_runner,
                    candidate_directory,
                    control_directory,
                    control_socket,
                } => vec![
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                        directory_identity: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::SyncPipeWrite,
                        target_fd: Some(sync_pipe.target_fd),
                        directory_identity: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::PinnedRunner,
                        target_fd: Some(pinned_runner.target_fd),
                        directory_identity: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::PinnedCandidateDirectory,
                        target_fd: Some(candidate_directory.target_fd),
                        directory_identity: Some(candidate_directory.identity),
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::PinnedControlDirectory,
                        target_fd: Some(control_directory.target_fd),
                        directory_identity: Some(control_directory.identity),
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::PinnedControlSocket,
                        target_fd: Some(control_socket.target_fd),
                        directory_identity: None,
                    },
                ],
            },
        };
        managed_test_failpoint("parent_before_commit");
        let mut commit_fds = vec![target.as_raw_fd()];
        match &request.auxiliary {
            ManagedAuxiliary::None => {}
            ManagedAuxiliary::SyncPipeWrite(sync) => commit_fds.push(sync.file.as_raw_fd()),
            ManagedAuxiliary::Speculation {
                sync_pipe,
                pinned_runner,
                candidate_directory,
                control_directory,
                control_socket,
            } => {
                commit_fds.push(sync_pipe.file.as_raw_fd());
                commit_fds.push(pinned_runner.file.as_raw_fd());
                commit_fds.push(candidate_directory.file.as_raw_fd());
                commit_fds.push(control_directory.file.as_raw_fd());
                commit_fds.push(control_socket.file.as_raw_fd());
            }
        }
        send_commit_with_fds(parent_control.as_raw_fd(), &commit, &commit_fds)?;
        managed_test_failpoint("parent_after_commit");
        wait_for_gate_exec(parent_control.as_raw_fd())?;
        if request.executable_policy == ManagedExecutablePolicy::PinnedSystemBwrap {
            prove_managed_executable_object(hello.registration.identity.pid, &target)?;
        }
        Ok(identity.clone())
    })();

    match launch_result {
        Ok(identity) => {
            let key = ManagedKey {
                slot: intent.record.slot,
                generation: intent.record.generation,
            };
            let owner_receipt = intent
                .record
                .owner
                .clone()
                .map(|owner| ManagedOwnerReceipt::new(owner, key))
                .transpose()
                .expect("validated managed owner receipt invariant");
            let controller = ManagedController {
                inner: Arc::new(ManagedControllerInner {
                    key,
                    identity,
                    owner: intent.record.owner.clone(),
                    registry,
                }),
            };
            waiter.cleanup = ManagedCleanupOwner::Controller(controller.clone());
            Ok(ManagedLaunch {
                waiter,
                controller,
                owner_receipt,
            })
        }
        Err(err) => {
            drop(parent_control);
            drop(intent);
            let pending = settle_failed_launch(waiter);
            Err(match pending {
                Some(waiter) => ManagedLaunchFailure::pending(err, waiter),
                None => ManagedLaunchFailure::resolved(err),
            })
        }
    }
}

#[cfg(target_os = "linux")]
fn resolve_unspawned_intent(
    registry: Registry,
    intent: LaunchIntent,
    lifetime_guard: Option<ManagedLifetimeGuard>,
    error: anyhow::Error,
) -> anyhow::Error {
    let slot = intent.record.slot;
    let generation = intent.record.generation;
    drop(intent);
    let mut job = ManagedReapJob::new(
        None,
        ManagedCleanupOwner::FailedLaunch {
            registry,
            slot,
            generation,
        },
        lifetime_guard,
    );
    let process_cleanup_resolved = matches!(
        job.cleanup.terminate(),
        Ok(ReconcileOutcome::ResolvedTombstone | ReconcileOutcome::Absent)
    );
    let artifact_cleanup_resolved = process_cleanup_resolved
        && job
            .lifetime_guard
            .as_ref()
            .map_or(Ok(true), ManagedLifetimeGuard::cleanup_artifacts)
            .unwrap_or(false);
    if artifact_cleanup_resolved {
        drop(job.lifetime_guard.take());
    } else {
        enqueue_managed_reap_job(job);
        managed_reaper_queue().changed.notify_all();
    }
    error
}

#[cfg(target_os = "linux")]
fn wait_for_gate_exec(control_fd: RawFd) -> Result<()> {
    let mut bytes = [0u8; MAX_RECORD_BYTES + 1];
    let received = unsafe {
        libc::recv(
            control_fd,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            libc::MSG_TRUNC,
        )
    };
    if received == 0 {
        return Ok(());
    }
    if received < 0 {
        return Err(std::io::Error::last_os_error()).context("wait for managed target exec");
    }
    ensure!(
        received as usize <= MAX_RECORD_BYTES,
        "oversized gate exec status"
    );
    let failure: GateExecFailure = serde_json::from_slice(&bytes[..received as usize])
        .context("malformed gate exec failure")?;
    ensure!(failure.protocol == "lterm-managed-exec-failure-v1");
    bail!(
        "managed target exec failed{}: {}",
        failure
            .errno
            .map(|errno| format!(" (errno {errno})"))
            .unwrap_or_default(),
        failure.message
    )
}

#[cfg(target_os = "linux")]
fn settle_failed_launch(waiter: ManagedWaiter) -> Option<ManagedWaiter> {
    match waiter.terminate_and_reap_bounded(Duration::from_secs(5)) {
        ManagedBoundedReap::Reaped => None,
        ManagedBoundedReap::Pending(waiter) => Some(waiter),
    }
}

#[cfg(target_os = "linux")]
fn reap_child_until(child: &mut std::process::Child, timeout: Duration) -> Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn managed_test_failpoint(_name: &str) {
    #[cfg(debug_assertions)]
    if managed_process_env_seams().failpoint.as_deref() == Some(std::ffi::OsStr::new(_name)) {
        unsafe { libc::_exit(86) };
    }
}

#[cfg(target_os = "linux")]
fn managed_test_errorpoint(_name: &str) -> Result<()> {
    #[cfg(debug_assertions)]
    if managed_local_return_error(_name) || managed_process_return_error(_name) {
        bail!("injected managed launch error")
    }
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn managed_process_env_seams() -> &'static ManagedProcessEnvSeams {
    MANAGED_PROCESS_ENV_SEAMS.get_or_init(|| {
        if std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        {
            return ManagedProcessEnvSeams::default();
        }
        ManagedProcessEnvSeams {
            initialized_on: std::thread::current().id(),
            failpoint: std::env::var_os("LTERM_INTERNAL_MANAGED_FAILPOINT"),
            return_error: std::env::var_os("LTERM_INTERNAL_MANAGED_RETURN_ERROR"),
            return_error_once: std::env::var_os("LTERM_INTERNAL_MANAGED_RETURN_ERROR_ONCE")
                .as_deref()
                == Some(std::ffi::OsStr::new("1")),
        }
    })
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn managed_process_return_error(name: &str) -> bool {
    let seams = managed_process_env_seams();
    seams.return_error.as_deref() == Some(std::ffi::OsStr::new(name))
        && (!seams.return_error_once
            || !MANAGED_RETURN_ERROR_ONCE_INJECTED.swap(true, std::sync::atomic::Ordering::SeqCst))
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn managed_local_return_error(name: &str) -> bool {
    MANAGED_LOCAL_RETURN_ERROR.with(|state| {
        let mut state = state.borrow_mut();
        let Some((expected, once, consumed)) = state.as_mut() else {
            return false;
        };
        if *expected != name || (*once && *consumed) {
            return false;
        }
        *consumed = true;
        true
    })
}

#[cfg(all(debug_assertions, target_os = "linux", test))]
struct ScopedManagedReturnError(Option<(&'static str, bool, bool)>);

#[cfg(all(debug_assertions, target_os = "linux", test))]
impl ScopedManagedReturnError {
    fn once(name: &'static str) -> Self {
        let previous =
            MANAGED_LOCAL_RETURN_ERROR.with(|state| state.replace(Some((name, true, false))));
        Self(previous)
    }

    fn reset(&self) {
        MANAGED_LOCAL_RETURN_ERROR.with(|state| {
            let mut state = state.borrow_mut();
            if let Some((_, _, consumed)) = state.as_mut() {
                *consumed = false;
            }
        });
    }
}

#[cfg(all(debug_assertions, target_os = "linux", test))]
impl Drop for ScopedManagedReturnError {
    fn drop(&mut self) {
        MANAGED_LOCAL_RETURN_ERROR.with(|state| {
            state.replace(self.0.take());
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_managed_process(
    _request: ManagedLaunchRequest,
) -> std::result::Result<ManagedLaunch, ManagedLaunchFailure> {
    Err(ManagedLaunchFailure::resolved(anyhow::anyhow!(
        "durable managed-process launch is supported only on Linux"
    )))
}

pub(crate) fn dispatch_internal_gate() -> Result<bool> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new(INTERNAL_GATE_ARG)) {
        return Ok(false);
    }
    #[cfg(target_os = "linux")]
    {
        run_gate(arguments.collect())?;
        Ok(true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        bail!("internal managed launch gate is supported only on Linux")
    }
}

#[cfg(debug_assertions)]
#[cfg(target_os = "linux")]
fn relocate_internal_test_fd_without_cloexec(file: File, minimum: RawFd) -> Result<File> {
    let relocated = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, minimum) };
    ensure!(
        relocated >= minimum,
        "relocate internal test descriptor without CLOEXEC"
    );
    drop(file);
    let relocated = unsafe { File::from_raw_fd(relocated) };
    let flags = unsafe { libc::fcntl(relocated.as_raw_fd(), libc::F_GETFD) };
    ensure!(flags >= 0 && flags & libc::FD_CLOEXEC == 0);
    Ok(relocated)
}

#[cfg(debug_assertions)]
pub(crate) fn dispatch_internal_test_driver() -> Result<bool> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let action = arguments.next();
    if action.as_deref() != Some(std::ffi::OsStr::new(INTERNAL_TEST_LAUNCH_ARG))
        && action.as_deref() != Some(std::ffi::OsStr::new(INTERNAL_TEST_RECONCILE_ARG))
    {
        return Ok(false);
    }
    ensure!(
        std::env::var_os("LTERM_INTERNAL_TEST_MODE").as_deref() == Some(std::ffi::OsStr::new("1")),
        "internal managed-launch test driver requires LTERM_INTERNAL_TEST_MODE=1"
    );
    #[cfg(not(target_os = "linux"))]
    bail!("internal managed-launch test driver is supported only on Linux");
    #[cfg(target_os = "linux")]
    if action.as_deref() == Some(std::ffi::OsStr::new(INTERNAL_TEST_RECONCILE_ARG)) {
        for entry in reconcile_managed_processes()?.entries {
            println!("managed-reconcile entry={entry:?}");
        }
        return Ok(true);
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(mode) = std::env::var_os("LTERM_INTERNAL_MANAGED_REAPER_SELF_TEST") {
            run_internal_reaper_self_test(&mode)?;
            println!("managed-reaper-self-test=1");
            return Ok(true);
        }
        let non_cloexec_inputs = std::env::var_os("LTERM_INTERNAL_MANAGED_NON_CLOEXEC_INPUTS")
            .as_deref()
            == Some(std::ffi::OsStr::new("1"));
        let mut launch_environment = std::env::vars_os().collect::<Vec<_>>();
        let executable = arguments
            .next()
            .map(PathBuf::from)
            .context("internal managed-launch test driver requires an executable")?;
        let owner = std::env::var("LTERM_INTERNAL_MANAGED_OWNER_UUID")
            .ok()
            .map(|tournament_uuid| -> Result<ManagedOwnerTag> {
                let role = match std::env::var("LTERM_INTERNAL_MANAGED_OWNER_ROLE")?.as_str() {
                    "probe" => ManagedOwnerRole::Probe,
                    "runner" => ManagedOwnerRole::Runner,
                    _ => bail!("invalid internal managed owner role"),
                };
                let owner = ManagedOwnerTag {
                    kind: ManagedOwnerKind::Speculation,
                    tournament_uuid: Uuid::parse_str(&tournament_uuid)?,
                    candidate_index: std::env::var("LTERM_INTERNAL_MANAGED_OWNER_CANDIDATE")?
                        .parse()?,
                    role,
                };
                owner.validate()?;
                Ok(owner)
            })
            .transpose()?;
        let placement = match (
            std::env::var_os("LTERM_INTERNAL_MANAGED_CONTROL_CGROUP_PROCS"),
            std::env::var("LTERM_INTERNAL_MANAGED_CONTROL_CGROUP_MEMBERSHIP").ok(),
        ) {
            (None, None) => ManagedPlacement::None,
            (Some(path), Some(membership)) => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                    .open(path)
                    .context("open internal test control cgroup.procs")?;
                if non_cloexec_inputs {
                    file = relocate_internal_test_fd_without_cloexec(file, 64)?;
                    launch_environment.push((
                        OsString::from("LTERM_INTERNAL_MANAGED_PLACEMENT_SOURCE_FD"),
                        OsString::from(file.as_raw_fd().to_string()),
                    ));
                }
                ManagedPlacement::CgroupV2(ControlCgroupPlacement::new_internal_test(
                    file, membership,
                )?)
            }
            _ => bail!("internal managed placement requires path and membership together"),
        };
        let (auxiliary, mut sync_read) = if std::env::var_os("LTERM_INTERNAL_MANAGED_SYNC_PIPE")
            .as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            let mut fds = [-1; 2];
            let flags = if non_cloexec_inputs {
                0
            } else {
                libc::O_CLOEXEC
            };
            ensure!(unsafe { libc::pipe2(fds.as_mut_ptr(), flags) } == 0);
            let read = unsafe { File::from_raw_fd(fds[0]) };
            let mut write = unsafe { File::from_raw_fd(fds[1]) };
            if non_cloexec_inputs {
                write = relocate_internal_test_fd_without_cloexec(write, 80)?;
                launch_environment.push((
                    OsString::from("LTERM_INTERNAL_MANAGED_SYNC_SOURCE_FD"),
                    OsString::from(write.as_raw_fd().to_string()),
                ));
            }
            (
                ManagedAuxiliary::SyncPipeWrite(SyncPipeWrite::new(
                    write,
                    MANAGED_SYNC_PIPE_TARGET_FD,
                )?),
                Some(read),
            )
        } else {
            (ManagedAuxiliary::None, None)
        };
        let process = launch_managed_process(ManagedLaunchRequest {
            owner,
            reservation: None,
            lifetime_guard: None,
            executable_policy: ManagedExecutablePolicy::Legacy,
            placement,
            auxiliary,
            executable,
            arguments: arguments.collect(),
            current_dir: None,
            environment: launch_environment,
            stdio: ManagedStdioPolicy::Inherit,
        })?;
        let key = process.controller.key();
        let pid = process.controller.identity().pid;
        println!(
            "managed-launch slot={} generation={} pid={}",
            key.slot(),
            key.generation(),
            pid
        );
        if std::env::var_os("LTERM_INTERNAL_MANAGED_LAUNCH_NO_WAIT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            // This debug-only path models abrupt daemon death. `_exit` skips
            // Rust destructors, leaving only durable restart ownership.
            std::io::stdout().flush()?;
            unsafe { libc::_exit(0) };
        }
        let terminate = std::env::var_os("LTERM_INTERNAL_MANAGED_LAUNCH_TERMINATE").as_deref()
            == Some(std::ffi::OsStr::new("1"));
        let status = if terminate {
            process.waiter.terminate_and_wait()?
        } else {
            process.waiter.wait()?
        };
        if !terminate {
            ensure!(
                status.success(),
                "managed root process exited with {status}"
            );
        }
        if let Some(sync_read) = &mut sync_read {
            let mut unexpected = Vec::new();
            sync_read
                .read_to_end(&mut unexpected)
                .context("wait for managed sync pipe EOF")?;
            ensure!(
                unexpected.is_empty(),
                "managed sync pipe carried unexpected bytes"
            );
        }
        Ok(true)
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
struct InternalReaperLifetime(PathBuf);

#[cfg(all(debug_assertions, target_os = "linux"))]
impl Drop for InternalReaperLifetime {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"released\n");
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn launch_internal_reaper_guarded(marker: PathBuf) -> Result<ManagedLaunch> {
    let lifetime = Arc::new(InternalReaperLifetime(marker));
    let process = launch_managed_process(ManagedLaunchRequest {
        owner: None,
        reservation: None,
        lifetime_guard: Some(ManagedLifetimeGuard::new(Arc::clone(&lifetime))),
        executable_policy: ManagedExecutablePolicy::Legacy,
        placement: ManagedPlacement::None,
        auxiliary: ManagedAuxiliary::None,
        executable: PathBuf::from("/usr/bin/sleep"),
        arguments: vec![OsString::from("30")],
        current_dir: None,
        environment: Vec::new(),
        stdio: ManagedStdioPolicy::Null,
    })?;
    drop(lifetime);
    Ok(process)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_internal_reaper_self_test(mode: &std::ffi::OsStr) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    if mode.as_bytes() == b"environment-initialization-order" {
        ensure!(
            MANAGED_REAPER_ENV_SEAMS.get().is_none(),
            "managed reaper environment was initialized before the test caller"
        );
        ensure!(
            MANAGED_PROCESS_ENV_SEAMS.get().is_none(),
            "managed process environment was initialized before the test caller"
        );
        let caller = std::thread::current().id();
        let queue = managed_reaper_queue();
        ensure_managed_reaper_supervisor(&queue)?;
        ensure!(
            managed_reaper_env_seams().initialized_on == caller,
            "managed reaper environment was initialized on a background thread"
        );
        ensure!(
            managed_process_env_seams().initialized_on == caller,
            "managed process environment was initialized on a background thread"
        );
        return Ok(());
    }
    let marker = std::env::var_os("LTERM_INTERNAL_MANAGED_REAPER_GUARD_MARKER")
        .map(PathBuf::from)
        .context("managed reaper self-test marker is missing")?;
    if mode.as_bytes() == b"cleanup-fairness" {
        return run_internal_reaper_cleanup_fairness_test(&marker);
    }
    if mode.as_bytes() == b"reap-fairness" {
        return run_internal_reaper_reap_fairness_test(&marker);
    }
    if mode.as_bytes() == b"sync-cleanup-error" {
        return run_internal_sync_cleanup_handoff_test(&marker);
    }
    if mode.as_bytes() == b"sync-terminate-cleanup-error" {
        return run_internal_sync_terminate_cleanup_handoff_test(&marker);
    }
    let seam = match mode.as_bytes() {
        b"spawn-failure" => "LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE",
        b"wait-error" => "LTERM_INTERNAL_MANAGED_FORCE_REAPER_WAIT_ERROR",
        b"cleanup-error" => "LTERM_INTERNAL_MANAGED_FORCE_CLEANUP_ERROR",
        _ => bail!("unknown managed reaper self-test mode"),
    };
    let process = launch_internal_reaper_guarded(marker.clone())?;
    set_managed_reaper_seam(seam, true)?;
    let ManagedLaunch {
        controller,
        waiter,
        owner_receipt: _,
    } = process;
    drop(controller);
    drop(waiter);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while managed_reaper_pending_jobs() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(managed_reaper_pending_jobs() == 1);
    ensure!(
        !marker.exists(),
        "lifetime guard released before exact reap"
    );
    set_managed_reaper_seam(seam, false)?;
    let convergence_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while (!marker.is_file() || managed_reaper_pending_jobs() != 0)
        && std::time::Instant::now() < convergence_deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(
        managed_reaper_pending_jobs() == 0 && marker.is_file(),
        "managed reaper supervisor did not converge automatically"
    );
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_internal_sync_cleanup_handoff_test(marker: &Path) -> Result<()> {
    let process = launch_internal_reaper_guarded(marker.to_path_buf())?;
    set_managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_CLEANUP_ERROR", true)?;
    let ManagedLaunch {
        controller,
        mut waiter,
        owner_receipt: _,
    } = process;
    drop(controller);
    waiter
        .child
        .as_mut()
        .context("sync cleanup test lost exact child")?
        .kill()?;
    ensure!(
        waiter.wait().is_err(),
        "forced cleanup error was not surfaced"
    );
    let queued_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while managed_reaper_pending_jobs() == 0 && std::time::Instant::now() < queued_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(managed_reaper_pending_jobs() == 1 && !marker.exists());
    set_managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_CLEANUP_ERROR", false)?;
    managed_reaper_queue().changed.notify_all();
    let convergence_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while (!marker.is_file() || managed_reaper_pending_jobs() != 0)
        && std::time::Instant::now() < convergence_deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(marker.is_file() && managed_reaper_pending_jobs() == 0);
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_internal_sync_terminate_cleanup_handoff_test(marker: &Path) -> Result<()> {
    let process = launch_internal_reaper_guarded(marker.to_path_buf())?;
    set_managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_CLEANUP_ERROR", true)?;
    let ManagedLaunch {
        controller,
        waiter,
        owner_receipt: _,
    } = process;
    drop(controller);
    let started = std::time::Instant::now();
    ensure!(
        waiter.terminate_and_wait().is_err(),
        "forced terminate cleanup error was not surfaced"
    );
    ensure!(
        started.elapsed() < Duration::from_secs(2),
        "terminate_and_wait blocked on a sleeping child before signalling"
    );
    set_managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_CLEANUP_ERROR", false)?;
    managed_reaper_queue().changed.notify_all();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while (!marker.is_file() || managed_reaper_pending_jobs() != 0)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(marker.is_file() && managed_reaper_pending_jobs() == 0);
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_internal_reaper_reap_fairness_test(marker: &Path) -> Result<()> {
    let first_marker = marker.with_extension("stuck-first-released");
    let second_marker = marker.with_extension("ready-second-released");
    let first = launch_internal_reaper_guarded(first_marker.clone())?;
    let second = launch_internal_reaper_guarded(second_marker.clone())?;
    set_managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_JOB_STUCK", true)?;
    drop(first.controller);
    drop(first.waiter);
    set_managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_JOB_STUCK", false)?;
    drop(second.controller);
    drop(second.waiter);

    let fairness_deadline = std::time::Instant::now() + Duration::from_millis(400);
    while !second_marker.is_file() && std::time::Instant::now() < fairness_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(
        second_marker.is_file() && !first_marker.exists(),
        "temporarily non-reapable first child starved a completed second job"
    );
    let convergence_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while (!first_marker.is_file() || managed_reaper_pending_jobs() != 0)
        && std::time::Instant::now() < convergence_deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(first_marker.is_file() && managed_reaper_pending_jobs() == 0);
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn run_internal_reaper_cleanup_fairness_test(marker: &Path) -> Result<()> {
    let first_marker = marker.with_extension("first-released");
    let second_marker = marker.with_extension("second-released");
    let first = launch_internal_reaper_guarded(first_marker.clone())?;
    let second = launch_internal_reaper_guarded(second_marker.clone())?;
    let registry = first.controller.inner.registry.clone();
    let first_key = first.controller.inner.key;
    let first_record = registry.read_valid_slot(first_key.slot)?;
    let first_slot = registry.slots.join(slot_name(first_key.slot));

    // Hold both exact child jobs in the prestarted supervisor, then make only
    // the first cleanup retryable.  The second guard must release while the
    // first remains reachable, proving a failed cleanup cannot starve the
    // queue or get mistaken for a completed reap.
    set_managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE", true)?;
    fs::write(&first_slot, b"{\"schema_version\":2")?;
    fs::set_permissions(&first_slot, fs::Permissions::from_mode(0o600))?;
    sync_dir(&registry.slots)?;
    let ManagedLaunch {
        controller: first_controller,
        waiter: first_waiter,
        owner_receipt: _,
    } = first;
    let ManagedLaunch {
        controller: second_controller,
        waiter: second_waiter,
        owner_receipt: _,
    } = second;
    drop(first_controller);
    drop(second_controller);
    drop(first_waiter);
    drop(second_waiter);
    let queued_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while managed_reaper_pending_jobs() < 2 && std::time::Instant::now() < queued_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(managed_reaper_pending_jobs() >= 2);
    set_managed_reaper_seam("LTERM_INTERNAL_MANAGED_FORCE_REAPER_SPAWN_FAILURE", false)?;
    managed_reaper_queue().changed.notify_all();

    let fairness_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !second_marker.is_file() && std::time::Instant::now() < fairness_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(
        second_marker.is_file() && !first_marker.exists(),
        "retrying cleanup starved a later exact-child cleanup job"
    );

    atomic_replace_json(&registry.slots, &slot_name(first_key.slot), &first_record)?;
    managed_reaper_queue().changed.notify_all();
    let convergence_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while (!first_marker.is_file() || managed_reaper_pending_jobs() != 0)
        && std::time::Instant::now() < convergence_deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    ensure!(
        first_marker.is_file() && second_marker.is_file() && managed_reaper_pending_jobs() == 0,
        "cleanup-only retry queue did not converge after durable evidence recovery"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn remap_gate_fds(
    control_fd: RawFd,
    guard_fd: RawFd,
    placement_fd: Option<RawFd>,
) -> std::io::Result<()> {
    let control_copy = duplicate_for_gate_remap(control_fd)?;
    let guard_copy = match duplicate_for_gate_remap(guard_fd) {
        Ok(fd) => fd,
        Err(err) => {
            unsafe { libc::close(control_copy) };
            return Err(err);
        }
    };
    let placement_copy = match placement_fd.map(duplicate_for_gate_remap).transpose() {
        Ok(fd) => fd,
        Err(err) => {
            unsafe {
                libc::close(control_copy);
                libc::close(guard_copy);
            }
            return Err(err);
        }
    };
    let result = if unsafe { libc::dup2(control_copy, GATE_CONTROL_FD) } < 0
        || unsafe { libc::dup2(guard_copy, GATE_GUARD_FD) } < 0
        || placement_copy.is_some_and(|fd| unsafe { libc::dup2(fd, GATE_PLACEMENT_FD) } < 0)
    {
        Err(std::io::Error::last_os_error())
    } else {
        if placement_copy.is_none() {
            unsafe { libc::close(GATE_PLACEMENT_FD) };
        }
        Ok(())
    };
    unsafe {
        libc::close(control_copy);
        libc::close(guard_copy);
        if let Some(fd) = placement_copy {
            libc::close(fd);
        }
    }
    result
}

#[cfg(target_os = "linux")]
fn duplicate_for_gate_remap(fd: RawFd) -> std::io::Result<RawFd> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, GATE_PLACEMENT_FD + 1) };
    if duplicate < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(duplicate)
    }
}

#[cfg(target_os = "linux")]
fn run_gate(arguments: Vec<OsString>) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    ensure!(arguments.len() >= 6, "malformed internal gate invocation");
    let registry_root = PathBuf::from(&arguments[0]);
    ensure!(registry_root.is_absolute());
    let slot: u16 = arguments[1].to_string_lossy().parse()?;
    let generation: u64 = arguments[2].to_string_lossy().parse()?;
    let nonce = Uuid::parse_str(&arguments[3].to_string_lossy())?;
    let placement_kind = arguments[4].to_string_lossy();
    ensure!(placement_kind == "none" || placement_kind == "cgroup_v2");
    let target_argv = &arguments[5..];
    ensure!(!target_argv.is_empty());

    let control = unsafe { File::from_raw_fd(GATE_CONTROL_FD) };
    let guard = unsafe { File::from_raw_fd(GATE_GUARD_FD) };
    unsafe {
        libc::close(MANAGED_SYNC_PIPE_TARGET_FD);
        libc::close(MANAGED_PINNED_RUNNER_TARGET_FD);
        libc::close(MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD);
        libc::close(MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD);
        libc::close(MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD);
    };
    validate_seqpacket(control.as_raw_fd())?;
    if placement_kind == "cgroup_v2" {
        let placement = unsafe { File::from_raw_fd(GATE_PLACEMENT_FD) };
        place_gate_in_control_cgroup(placement)?;
    }
    let registry = Registry::open_at(registry_root, SLOT_COUNT)?;
    validate_inherited_guard(&registry, slot, &guard)?;
    let intent = registry.read_valid_slot(slot)?;
    ensure!(intent.generation == generation);
    ensure!(matches!(
        intent.state,
        SlotState::IntentDurable { nonce: value, .. } if value == nonce
    ));
    managed_test_failpoint("gate_before_registration");

    let identity = match observe_identity(std::process::id()) {
        Evidence::Present(identity) => identity,
        Evidence::Absent => bail!("gate cannot observe its own process identity"),
        Evidence::Unavailable(reason) => bail!("gate identity unavailable: {reason}"),
    };
    let registration = GateRegistration {
        schema_version: GATE_SCHEMA_VERSION,
        slot,
        generation,
        nonce,
        identity: identity.clone(),
    };
    // Gate self-registration is durable before HELLO. Recovery may therefore
    // identify a busy inherited guard even if the daemon dies before HELLO.
    registry.replace_registration(
        slot,
        &RegistrationRecord {
            schema_version: GATE_SCHEMA_VERSION,
            slot,
            generation,
            registration: Some(registration.clone()),
        },
    )?;
    managed_test_failpoint("gate_after_registration");
    send_packet(
        control.as_raw_fd(),
        &GateHello {
            protocol: "lterm-managed-hello-v1".into(),
            registration: registration.clone(),
        },
    )?;
    managed_test_failpoint("gate_after_hello");

    set_socket_timeout(control.as_raw_fd(), Duration::from_secs(30))?;
    let (
        commit,
        target,
        sync_pipe,
        pinned_runner,
        candidate_directory,
        control_directory,
        control_socket,
    ) = recv_commit_with_fds(control.as_raw_fd())?;
    let (target, sync_pipe, pinned_runner, candidate_directory, control_directory, control_socket) =
        relocate_received_commit_authorities(
            target,
            sync_pipe,
            pinned_runner,
            candidate_directory,
            control_directory,
            control_socket,
        )?;
    managed_test_failpoint("gate_after_commit");
    ensure!(commit.protocol == "lterm-managed-commit-v1");
    ensure!(commit.slot == slot && commit.generation == generation && commit.nonce == nonce);
    ensure!(commit.identity == identity);
    let durable = registry.read_valid_slot(slot)?;
    ensure!(matches!(
        durable.state,
        SlotState::IdentityDurable {
            nonce: value,
            identity: ref durable_identity,
            release_may_have_occurred: true,
        } if value == nonce && durable_identity == &identity
    ));
    if let Some(control_descriptor) = commit.descriptors.get(4) {
        let expected_control = durable
            .artifact_binding
            .as_deref()
            .and_then(ManagedArtifactBinding::private_directory)
            .context("speculation COMMIT lacks a durable private directory binding")?;
        ensure!(
            control_descriptor.directory_identity == Some(expected_control),
            "pinned control directory does not match the durable private binding"
        );
    }
    let expected_control_socket = if commit.descriptors.len() == 6 {
        let descriptor = commit
            .descriptors
            .get(5)
            .context("speculation COMMIT lacks pinned socket metadata")?;
        ensure!(
            descriptor.role == CommitDescriptorRole::PinnedControlSocket
                && descriptor.target_fd == Some(MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD),
            "pinned control socket descriptor changed"
        );
        Some(
            durable
                .artifact_binding
                .as_deref()
                .and_then(ManagedArtifactBinding::socket)
                .context("speculation COMMIT lacks a durable socket binding")?,
        )
    } else {
        None
    };
    if let (Some(socket), Some(expected)) = (control_socket.as_ref(), expected_control_socket) {
        validate_managed_pinned_control_socket_fd(socket, expected)?;
    }
    ensure!(matches!(
        observe_identity(std::process::id()),
        Evidence::Present(ref actual) if actual == &identity
    ));
    validate_pinned_executable(&target, ManagedExecutablePolicy::Legacy)?;
    prepare_target_fd_for_exec(&target)?;
    if let Some(sync_pipe) = sync_pipe {
        install_sync_pipe(sync_pipe, MANAGED_SYNC_PIPE_TARGET_FD)?;
    }
    if let Some(pinned_runner) = pinned_runner {
        install_pinned_runner(pinned_runner, MANAGED_PINNED_RUNNER_TARGET_FD)?;
    }
    if let Some(candidate_directory) = candidate_directory {
        install_pinned_directory(
            candidate_directory,
            MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD,
        )?;
    }
    if let Some(control_directory) = control_directory {
        install_pinned_directory(
            control_directory,
            MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD,
        )?;
    }
    if let (Some(control_socket), Some(expected)) = (control_socket, expected_control_socket) {
        install_pinned_control_socket(
            control_socket,
            MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD,
            expected,
        )?;
    }
    set_cloexec(control.as_raw_fd())?;
    set_cloexec(guard.as_raw_fd())?;
    managed_test_failpoint("gate_before_exec");

    let argv = target_argv
        .iter()
        .map(|argument| std::ffi::CString::new(argument.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut argv_ptrs = argv
        .iter()
        .map(|value| value.as_ptr().cast_mut())
        .collect::<Vec<_>>();
    argv_ptrs.push(std::ptr::null_mut());
    let environment = std::env::vars_os()
        .map(|(key, value)| {
            let mut bytes = key.as_bytes().to_vec();
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_bytes());
            std::ffi::CString::new(bytes)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut env_ptrs = environment
        .iter()
        .map(|value| value.as_ptr().cast_mut())
        .collect::<Vec<_>>();
    env_ptrs.push(std::ptr::null_mut());
    let empty = c"";
    let result = unsafe {
        libc::execveat(
            target.as_raw_fd(),
            empty.as_ptr(),
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        let _ = send_packet(
            control.as_raw_fd(),
            &GateExecFailure {
                protocol: "lterm-managed-exec-failure-v1".into(),
                errno: error.raw_os_error(),
                message: error.to_string(),
            },
        );
        return Err(error).context("execveat failed");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_inherited_guard(registry: &Registry, slot: u16, inherited: &File) -> Result<()> {
    validate_file_handle(inherited, Path::new("inherited slot guard"))?;
    let expected = open_exact_file(&registry.guards.join(guard_name(slot)), false)?;
    let actual_meta = inherited.metadata()?;
    let expected_meta = expected.metadata()?;
    ensure!(actual_meta.dev() == expected_meta.dev());
    ensure!(actual_meta.ino() == expected_meta.ino());
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_pinned_executable(path: &Path, policy: ManagedExecutablePolicy) -> Result<File> {
    if policy == ManagedExecutablePolicy::PinnedSystemBwrap {
        ensure!(
            path.as_os_str() == std::ffi::OsStr::new("/usr/bin/bwrap"),
            "pinned system executable path is not exact"
        );
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("open pinned target executable {}", path.display()))?;
    validate_pinned_executable(&file, policy)?;
    if policy == ManagedExecutablePolicy::PinnedSystemBwrap {
        let readback = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/usr/bin/bwrap")
            .context("reopen pinned system executable")?;
        validate_pinned_executable(&readback, policy)?;
        ensure!(
            same_file_object(&file, &readback)?,
            "pinned system executable changed during validation"
        );
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn validate_pinned_executable(file: &File, policy: ManagedExecutablePolicy) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "target executable is not a regular file"
    );
    ensure!(
        metadata.permissions().mode() & 0o111 != 0,
        "target is not executable"
    );
    if policy == ManagedExecutablePolicy::PinnedSystemBwrap {
        ensure!(
            metadata.uid() == 0,
            "pinned system executable is not root-owned"
        );
        ensure!(
            metadata.permissions().mode() & 0o022 == 0,
            "pinned system executable is group/other writable"
        );
        ensure!(
            metadata.nlink() == 1,
            "pinned system executable has extra links"
        );
        ensure!(
            !is_shebang_script(file)?,
            "pinned system executable is not a native object"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn same_file_object(left: &File, right: &File) -> Result<bool> {
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(target_os = "linux")]
fn prove_managed_executable_object(pid: u32, expected: &File) -> Result<()> {
    let actual = fs::metadata(format!("/proc/{pid}/exe"))
        .context("read back managed executable identity")?;
    let expected = expected.metadata()?;
    ensure!(
        actual.dev() == expected.dev() && actual.ino() == expected.ino(),
        "managed executable object does not match the pinned object"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn is_shebang_script(file: &File) -> Result<bool> {
    let mut magic = [0u8; 2];
    let read = file
        .read_at(&mut magic, 0)
        .context("read pinned target executable header")?;
    Ok(read == magic.len() && magic == *b"#!")
}

#[cfg(target_os = "linux")]
fn prepare_target_fd_for_exec(file: &File) -> Result<()> {
    if is_shebang_script(file)? {
        // Linux resolves an AT_EMPTY_PATH shebang through the target FD when
        // starting the interpreter. A CLOEXEC target therefore makes execveat
        // fail with ENOENT. Keep native binaries CLOEXEC, but let this one FD
        // survive into the interpreter for scripts; its lifetime there is an
        // unavoidable consequence of descriptor-backed shebang execution.
        clear_cloexec(file.as_raw_fd())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn seqpacket_pair() -> Result<(File, File)> {
    use std::os::fd::FromRawFd;
    let mut fds = [-1; 2];
    let result = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("create SOCK_SEQPACKET pair");
    }
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

#[cfg(target_os = "linux")]
fn validate_seqpacket(fd: RawFd) -> Result<()> {
    let mut kind = 0i32;
    let mut length = std::mem::size_of::<i32>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut i32).cast(),
            &mut length,
        )
    };
    ensure!(result == 0, "SO_TYPE unavailable");
    ensure!(
        kind == libc::SOCK_SEQPACKET,
        "control FD is not SOCK_SEQPACKET"
    );
    Ok(())
}

fn validate_cgroup_membership(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 1_024,
        "control cgroup membership is empty or oversized"
    );
    ensure!(
        value.starts_with('/'),
        "control cgroup membership is not absolute"
    );
    ensure!(
        !value.as_bytes().contains(&0) && !value.contains('\n'),
        "control cgroup membership contains a forbidden byte"
    );
    for component in Path::new(value).components() {
        ensure!(
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            ),
            "control cgroup membership is not normalized"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_cgroup_procs_fd(file: &File) -> Result<()> {
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    ensure!(flags >= 0, "cgroup.procs FD flags unavailable");
    ensure!(
        flags & libc::O_ACCMODE == libc::O_WRONLY || flags & libc::O_ACCMODE == libc::O_RDWR,
        "cgroup.procs FD is not writable"
    );
    let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::fstatfs(file.as_raw_fd(), &mut statfs) } == 0,
        "cgroup.procs filesystem identity unavailable"
    );
    ensure!(
        statfs.f_type as u64 == libc::CGROUP2_SUPER_MAGIC as u64,
        "placement FD is not on cgroup v2"
    );
    let link = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .context("resolve placement FD")?;
    ensure!(
        link.file_name().is_some_and(|name| name == "cgroup.procs"),
        "placement FD is not cgroup.procs"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn observe_cgroup_directory_identity(file: &File) -> Result<ManagedCgroupDirectoryIdentity> {
    const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
    let metadata = file.metadata()?;
    ensure!(metadata.is_dir(), "cgroup leaf is not a directory");
    let mut statfs: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(unsafe { libc::fstatfs(file.as_raw_fd(), &mut statfs) } == 0);
    ensure!(statfs.f_type as u64 == libc::CGROUP2_SUPER_MAGIC as u64);
    let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    ensure!(
        unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_BASIC_STATS | STATX_MNT_ID_UNIQUE,
                statx.as_mut_ptr(),
            )
        } == 0,
        "cgroup unique mount identity is unavailable"
    );
    let statx = unsafe { statx.assume_init() };
    ensure!(
        statx.stx_mask & STATX_MNT_ID_UNIQUE != 0 && statx.stx_mnt_id != 0,
        "cgroup unique mount identity is unavailable"
    );
    let boot_uuid = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .and_then(|value| Uuid::parse_str(value.trim()).ok())
        .context("boot identity is unavailable")?;
    Ok(ManagedCgroupDirectoryIdentity {
        boot_uuid,
        dev: metadata.dev(),
        ino: metadata.ino(),
        statx_mnt_id_unique: statx.stx_mnt_id,
    })
}

#[cfg(target_os = "linux")]
fn validate_cgroup_leaf_fd(file: &File, expected: ManagedCgroupDirectoryIdentity) -> Result<()> {
    ensure!(
        observe_cgroup_directory_identity(file)? == expected,
        "cgroup leaf identity does not match durable evidence"
    );
    let cgroup_type = fs::read(format!("/proc/self/fd/{}/cgroup.type", file.as_raw_fd()))
        .context("read cgroup leaf type")?;
    ensure!(cgroup_type == b"domain\n", "cgroup leaf is not a domain");
    Ok(())
}

#[cfg(all(target_os = "linux", debug_assertions))]
fn open_control_leaf_for_internal_test(cgroup_procs: &File) -> Result<File> {
    let target = fs::read_link(format!("/proc/self/fd/{}", cgroup_procs.as_raw_fd()))
        .context("resolve internal test cgroup.procs")?;
    let parent = target
        .parent()
        .context("internal test cgroup leaf is absent")?;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)
        .context("open internal test cgroup leaf")
}

#[cfg(target_os = "linux")]
fn place_gate_in_control_cgroup(file: File) -> Result<()> {
    validate_cgroup_procs_fd(&file)?;
    let bytes = b"0\n";
    let written = unsafe { libc::write(file.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
    ensure!(
        written == bytes.len() as isize,
        "control cgroup placement write failed"
    );
    drop(file);
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_process_cgroup_membership(pid: u32, expected: &str) -> Result<()> {
    validate_cgroup_membership(expected)?;
    let bytes = fs::read(format!("/proc/{pid}/cgroup")).context("read managed gate cgroup")?;
    ensure!(
        bytes.len() <= 4 * 1_024,
        "managed gate cgroup report is oversized"
    );
    let actual = std::str::from_utf8(&bytes).context("managed gate cgroup report is not UTF-8")?;
    ensure!(
        actual == format!("0::{expected}\n"),
        "managed gate is not in the exact control cgroup"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn prove_process_ancestry(
    descendant: &ProcessIdentity,
    managed_root: &ProcessIdentity,
) -> std::result::Result<bool, ManagedEvidenceCode> {
    if descendant.pid == managed_root.pid {
        return Ok(false);
    }
    let mut current = descendant.pid;
    for _ in 0..256 {
        let parent = read_proc_parent_pid(current)?;
        if parent == managed_root.pid {
            return Ok(
                matches!(verify_exact_process(managed_root), Evidence::Present(_))
                    && matches!(verify_exact_process(descendant), Evidence::Present(_)),
            );
        }
        if parent <= 1 || parent == current {
            return Ok(false);
        }
        current = parent;
    }
    Err(ManagedEvidenceCode::InvalidEvidence)
}

#[cfg(target_os = "linux")]
fn read_proc_parent_pid(pid: u32) -> std::result::Result<u32, ManagedEvidenceCode> {
    let bytes = fs::read(format!("/proc/{pid}/stat")).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ManagedEvidenceCode::ProcessAbsent
        } else {
            ManagedEvidenceCode::IdentityUnavailable
        }
    })?;
    if bytes.len() > 4 * 1024 {
        return Err(ManagedEvidenceCode::InvalidEvidence);
    }
    let close = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or(ManagedEvidenceCode::InvalidEvidence)?;
    let suffix = std::str::from_utf8(bytes.get(close + 1..).unwrap_or_default())
        .map_err(|_| ManagedEvidenceCode::InvalidEvidence)?;
    suffix
        .split_whitespace()
        .nth(1)
        .ok_or(ManagedEvidenceCode::InvalidEvidence)?
        .parse()
        .map_err(|_| ManagedEvidenceCode::InvalidEvidence)
}

#[cfg(target_os = "linux")]
fn validate_sync_pipe_write_fd(file: &File) -> Result<()> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::fstat(file.as_raw_fd(), &mut stat) } == 0,
        "sync pipe FD metadata unavailable"
    );
    ensure!(
        stat.st_mode & libc::S_IFMT == libc::S_IFIFO,
        "sync auxiliary FD is not a pipe"
    );
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    ensure!(flags >= 0, "sync pipe FD flags unavailable");
    ensure!(
        flags & libc::O_ACCMODE == libc::O_WRONLY || flags & libc::O_ACCMODE == libc::O_RDWR,
        "sync auxiliary FD is not writable"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_managed_pinned_runner_fd(file: &File) -> Result<()> {
    let metadata = file
        .metadata()
        .context("pinned runner metadata unavailable")?;
    ensure!(metadata.is_file(), "pinned runner FD is not a regular file");
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == 0o500
            && metadata.nlink() == 1,
        "pinned runner FD identity is not private executable data"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum ManagedPinnedDirectoryKind {
    Candidate,
    Control,
}

#[cfg(target_os = "linux")]
fn observe_managed_directory_identity(file: &File) -> Result<ManagedDirectoryIdentity> {
    const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
    let metadata = file
        .metadata()
        .context("pinned directory metadata unavailable")?;
    ensure!(metadata.is_dir(), "pinned directory FD is not a directory");
    let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    ensure!(
        unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_BASIC_STATS | STATX_MNT_ID_UNIQUE,
                statx.as_mut_ptr(),
            )
        } == 0,
        "pinned directory unique mount identity is unavailable"
    );
    let statx = unsafe { statx.assume_init() };
    ensure!(
        statx.stx_mask & STATX_MNT_ID_UNIQUE != 0 && statx.stx_mnt_id != 0,
        "pinned directory unique mount identity is unavailable"
    );
    let boot_uuid = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .and_then(|value| Uuid::parse_str(value.trim()).ok())
        .context("boot identity is unavailable")?;
    Ok(ManagedDirectoryIdentity {
        boot_uuid,
        dev: metadata.dev(),
        ino: metadata.ino(),
        statx_mnt_id_unique: statx.stx_mnt_id,
    })
}

#[cfg(target_os = "linux")]
fn validate_managed_pinned_directory_fd(
    file: &File,
    expected: ManagedDirectoryIdentity,
    kind: ManagedPinnedDirectoryKind,
) -> Result<()> {
    expected.validate()?;
    ensure!(
        observe_managed_directory_identity(file)? == expected,
        "pinned directory FD identity does not match durable evidence"
    );
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "pinned directory FD is not owned by the current user"
    );
    match kind {
        ManagedPinnedDirectoryKind::Candidate => ensure!(
            metadata.mode() & 0o6000 == 0 && metadata.mode() & 0o022 == 0,
            "pinned candidate directory permissions are unsafe"
        ),
        ManagedPinnedDirectoryKind::Control => ensure!(
            metadata.mode() & 0o7777 == 0o700,
            "pinned control directory permissions are unsafe"
        ),
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_managed_pinned_control_socket_fd(
    file: &File,
    expected: ManagedArtifactIdentity,
) -> Result<()> {
    expected.validate()?;
    let metadata = file
        .metadata()
        .context("pinned control socket metadata unavailable")?;
    ensure!(
        metadata.mode() & libc::S_IFMT == libc::S_IFSOCK
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o7777 == 0o600
            && metadata.nlink() == 1
            && metadata.dev() == expected.dev
            && metadata.ino() == expected.ino,
        "pinned control socket FD does not match durable private endpoint evidence"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_socket_timeout(fd: RawFd, timeout: Duration) -> Result<()> {
    let value = libc::timeval {
        tv_sec: timeout.as_secs().try_into().unwrap_or(libc::time_t::MAX),
        tv_usec: timeout.subsec_micros().into(),
    };
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&value as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("set gate receive timeout")
    }
}

#[cfg(target_os = "linux")]
fn send_packet<T: Serialize>(fd: RawFd, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_RECORD_BYTES);
    let sent = unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL) };
    ensure!(
        sent == bytes.len() as isize,
        "short or failed seqpacket send"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn recv_packet<T: for<'de> Deserialize<'de>>(fd: RawFd) -> Result<T> {
    let mut bytes = [0u8; MAX_RECORD_BYTES + 1];
    let received =
        unsafe { libc::recv(fd, bytes.as_mut_ptr().cast(), bytes.len(), libc::MSG_TRUNC) };
    ensure!(received > 0, "gate control EOF or receive failure");
    ensure!(
        received as usize <= MAX_RECORD_BYTES,
        "oversized gate packet"
    );
    serde_json::from_slice(&bytes[..received as usize]).context("malformed gate packet")
}

#[cfg(target_os = "linux")]
fn send_commit_with_fds(fd: RawFd, commit: &GateCommit, rights: &[RawFd]) -> Result<()> {
    let bytes = serde_json::to_vec(commit)?;
    ensure!(bytes.len() <= MAX_RECORD_BYTES);
    ensure!(!rights.is_empty() && rights.len() <= MAX_COMMIT_FDS);
    ensure!(commit.descriptors.len() == rights.len());
    let mut iovec = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let rights_bytes = rights
        .len()
        .checked_mul(std::mem::size_of::<RawFd>())
        .context("COMMIT descriptor byte count overflow")?;
    let control_len = unsafe { libc::CMSG_SPACE(rights_bytes as u32) } as usize;
    let mut control = vec![0u8; control_len];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        ensure!(!header.is_null());
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(rights_bytes as u32) as usize;
        std::ptr::copy_nonoverlapping(
            rights.as_ptr(),
            libc::CMSG_DATA(header).cast::<RawFd>(),
            rights.len(),
        );
        message.msg_controllen = (*header).cmsg_len;
    }
    let sent = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
    ensure!(sent == bytes.len() as isize, "atomic COMMIT send failed");
    Ok(())
}

#[cfg(target_os = "linux")]
type ReceivedCommitAuthorities = (
    GateCommit,
    File,
    Option<File>,
    Option<File>,
    Option<File>,
    Option<File>,
    Option<File>,
);

#[cfg(target_os = "linux")]
fn recv_commit_with_fds(fd: RawFd) -> Result<ReceivedCommitAuthorities> {
    use std::os::fd::FromRawFd;
    let mut bytes = [0u8; MAX_RECORD_BYTES + 1];
    let mut iovec = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let rights_bytes = MAX_COMMIT_FDS * std::mem::size_of::<RawFd>();
    let control_len = unsafe { libc::CMSG_SPACE(rights_bytes as u32) } as usize;
    let mut control = vec![0u8; control_len];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    let received =
        unsafe { libc::recvmsg(fd, &mut message, libc::MSG_CMSG_CLOEXEC | libc::MSG_TRUNC) };
    ensure!(received > 0, "COMMIT EOF or receive failure");
    let mut received_files = Vec::new();
    let mut ancillary_shape_valid = true;
    let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !header.is_null() {
        let minimum = unsafe { libc::CMSG_LEN(0) } as usize;
        let header_len = unsafe { (*header).cmsg_len };
        if header_len < minimum {
            ancillary_shape_valid = false;
            break;
        }
        let is_rights = unsafe {
            (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS
        };
        if !is_rights {
            ancillary_shape_valid = false;
        } else {
            let payload_len = header_len - minimum;
            if payload_len % std::mem::size_of::<RawFd>() != 0 {
                ancillary_shape_valid = false;
            } else {
                let count = payload_len / std::mem::size_of::<RawFd>();
                for index in 0..count {
                    let received_fd = unsafe {
                        std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>().add(index))
                    };
                    if received_fd < 0 {
                        ancillary_shape_valid = false;
                    } else {
                        // Own every delivered descriptor immediately. All later
                        // validation and JSON failures therefore close it.
                        received_files.push(unsafe { File::from_raw_fd(received_fd) });
                    }
                }
            }
        }
        header = unsafe { libc::CMSG_NXTHDR(&message, header) };
    }
    ensure!(received as usize <= MAX_RECORD_BYTES, "oversized COMMIT");
    ensure!(
        message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0,
        "truncated COMMIT packet or ancillary data"
    );
    ensure!(ancillary_shape_valid, "malformed COMMIT ancillary data");
    let commit: GateCommit =
        serde_json::from_slice(&bytes[..received as usize]).context("malformed COMMIT")?;
    ensure!(
        commit.descriptors.len() == received_files.len(),
        "COMMIT descriptor metadata/rights count mismatch"
    );
    ensure!(
        matches!(
            commit.descriptors.as_slice(),
            [CommitDescriptor {
                role: CommitDescriptorRole::TargetExecutable,
                target_fd: None,
                directory_identity: None,
            }] | [
                CommitDescriptor {
                    role: CommitDescriptorRole::TargetExecutable,
                    target_fd: None,
                    directory_identity: None,
                },
                CommitDescriptor {
                    role: CommitDescriptorRole::SyncPipeWrite,
                    target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
                    directory_identity: None,
                }
            ] | [
                CommitDescriptor {
                    role: CommitDescriptorRole::TargetExecutable,
                    target_fd: None,
                    directory_identity: None,
                },
                CommitDescriptor {
                    role: CommitDescriptorRole::SyncPipeWrite,
                    target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
                    directory_identity: None,
                },
                CommitDescriptor {
                    role: CommitDescriptorRole::PinnedRunner,
                    target_fd: Some(MANAGED_PINNED_RUNNER_TARGET_FD),
                    directory_identity: None,
                },
                CommitDescriptor {
                    role: CommitDescriptorRole::PinnedCandidateDirectory,
                    target_fd: Some(MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD),
                    directory_identity: Some(_),
                },
                CommitDescriptor {
                    role: CommitDescriptorRole::PinnedControlDirectory,
                    target_fd: Some(MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD),
                    directory_identity: Some(_),
                },
                CommitDescriptor {
                    role: CommitDescriptorRole::PinnedControlSocket,
                    target_fd: Some(MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD),
                    directory_identity: None,
                }
            ]
        ),
        "COMMIT descriptor roles or target mapping are invalid"
    );
    let mut received_files = received_files.into_iter();
    let target = received_files
        .next()
        .context("COMMIT has no target executable FD")?;
    let sync_pipe = received_files.next();
    let pinned_runner = received_files.next();
    let candidate_directory = received_files.next();
    let control_directory = received_files.next();
    let control_socket = received_files.next();
    ensure!(received_files.next().is_none());
    if let (Some(candidate), Some(descriptor)) =
        (candidate_directory.as_ref(), commit.descriptors.get(3))
    {
        validate_managed_pinned_directory_fd(
            candidate,
            descriptor
                .directory_identity
                .context("candidate directory identity is absent")?,
            ManagedPinnedDirectoryKind::Candidate,
        )?;
    }
    if let (Some(control), Some(descriptor)) =
        (control_directory.as_ref(), commit.descriptors.get(4))
    {
        validate_managed_pinned_directory_fd(
            control,
            descriptor
                .directory_identity
                .context("control directory identity is absent")?,
            ManagedPinnedDirectoryKind::Control,
        )?;
    }
    if let Some(socket) = control_socket.as_ref() {
        let metadata = socket.metadata()?;
        validate_managed_pinned_control_socket_fd(
            socket,
            ManagedArtifactIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
            },
        )?;
    }
    Ok((
        commit,
        target,
        sync_pipe,
        pinned_runner,
        candidate_directory,
        control_directory,
        control_socket,
    ))
}

#[cfg(target_os = "linux")]
fn move_file_away_from_fixed_auxiliary_fds(file: File) -> Result<File> {
    if !(MANAGED_SYNC_PIPE_TARGET_FD..=MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD)
        .contains(&file.as_raw_fd())
    {
        return Ok(file);
    }
    let duplicate = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD + 1,
        )
    };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error())
            .context("move COMMIT descriptor away from fixed auxiliary FDs");
    }
    Ok(unsafe { File::from_raw_fd(duplicate) })
}

#[cfg(target_os = "linux")]
type RelocatedCommitAuthorities = (
    File,
    Option<File>,
    Option<File>,
    Option<File>,
    Option<File>,
    Option<File>,
);

#[cfg(target_os = "linux")]
fn relocate_received_commit_authorities(
    target: File,
    sync_pipe: Option<File>,
    pinned_runner: Option<File>,
    candidate_directory: Option<File>,
    control_directory: Option<File>,
    control_socket: Option<File>,
) -> Result<RelocatedCommitAuthorities> {
    // SCM_RIGHTS allocates the lowest free descriptors.  Because the gate
    // deliberately closes 10..=14 before receiving COMMIT, any received
    // authority can initially own a reserved target number.  Move every
    // authority out of that range before installing even the first target;
    // otherwise dup3(sync, 10) can overwrite an as-yet-unmoved control FD.
    let target = move_file_away_from_fixed_auxiliary_fds(target)?;
    let sync_pipe = sync_pipe
        .map(move_file_away_from_fixed_auxiliary_fds)
        .transpose()?;
    let pinned_runner = pinned_runner
        .map(move_file_away_from_fixed_auxiliary_fds)
        .transpose()?;
    let candidate_directory = candidate_directory
        .map(move_file_away_from_fixed_auxiliary_fds)
        .transpose()?;
    let control_directory = control_directory
        .map(move_file_away_from_fixed_auxiliary_fds)
        .transpose()?;
    let control_socket = control_socket
        .map(move_file_away_from_fixed_auxiliary_fds)
        .transpose()?;
    Ok((
        target,
        sync_pipe,
        pinned_runner,
        candidate_directory,
        control_directory,
        control_socket,
    ))
}

#[cfg(target_os = "linux")]
fn install_sync_pipe(file: File, target_fd: RawFd) -> Result<()> {
    validate_sync_pipe_write_fd(&file)?;
    if file.as_raw_fd() != target_fd {
        let result = unsafe { libc::dup3(file.as_raw_fd(), target_fd, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("install fixed sync pipe FD");
        }
    } else {
        clear_cloexec(target_fd)?;
        let _ = file.into_raw_fd();
        return Ok(());
    }
    drop(file);
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_pinned_runner(file: File, target_fd: RawFd) -> Result<()> {
    ensure!(
        target_fd == MANAGED_PINNED_RUNNER_TARGET_FD,
        "pinned runner target FD changed"
    );
    validate_managed_pinned_runner_fd(&file)?;
    if file.as_raw_fd() != target_fd {
        let result = unsafe { libc::dup3(file.as_raw_fd(), target_fd, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("install pinned runner FD");
        }
    } else {
        clear_cloexec(target_fd)?;
        let _ = file.into_raw_fd();
        return Ok(());
    }
    drop(file);
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_pinned_directory(file: File, target_fd: RawFd) -> Result<()> {
    ensure!(
        matches!(
            target_fd,
            MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD
                | MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD
        ),
        "pinned directory target FD changed"
    );
    ensure!(
        file.metadata()?.is_dir(),
        "pinned mount authority is not a directory"
    );
    if file.as_raw_fd() != target_fd {
        let result = unsafe { libc::dup3(file.as_raw_fd(), target_fd, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("install pinned directory FD");
        }
    } else {
        clear_cloexec(target_fd)?;
        let _ = file.into_raw_fd();
        return Ok(());
    }
    drop(file);
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_pinned_control_socket(
    file: File,
    target_fd: RawFd,
    expected: ManagedArtifactIdentity,
) -> Result<()> {
    ensure!(
        target_fd == MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD,
        "pinned control socket target FD changed"
    );
    validate_managed_pinned_control_socket_fd(&file, expected)?;
    if file.as_raw_fd() != target_fd {
        let result = unsafe { libc::dup3(file.as_raw_fd(), target_fd, 0) };
        if result < 0 {
            return Err(std::io::Error::last_os_error())
                .context("install pinned control socket FD");
        }
    } else {
        clear_cloexec(target_fd)?;
        let _ = file.into_raw_fd();
        return Ok(());
    }
    drop(file);
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_cloexec(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    ensure!(flags >= 0, "descriptor flags unavailable before CLOEXEC");
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    ensure!(result == 0, "failed to set descriptor CLOEXEC");
    let verified = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    ensure!(
        verified >= 0 && verified & libc::FD_CLOEXEC != 0,
        "descriptor CLOEXEC verification failed"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn clear_cloexec(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    ensure!(flags >= 0);
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    ensure!(result == 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn registry(slot_count: usize) -> (TempDir, Registry) {
        let temp = TempDir::new().expect("tempdir");
        let parent = temp.path().join("speculation");
        create_exact_dir(&parent).expect("private parent");
        let registry = Registry::open_at(parent.join("process-registry-v1"), slot_count)
            .expect("registry genesis");
        (temp, registry)
    }

    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            boot_uuid: Uuid::from_u128(1),
            pid_namespace_inode: 2,
            pid,
            start_ticks: 3,
        }
    }

    fn owner(candidate_index: u8, role: ManagedOwnerRole) -> ManagedOwnerTag {
        ManagedOwnerTag {
            kind: ManagedOwnerKind::Speculation,
            tournament_uuid: Uuid::from_u128(0x1234),
            candidate_index,
            role,
        }
    }

    fn directory_identity(seed: u64) -> ManagedDirectoryIdentity {
        ManagedDirectoryIdentity {
            boot_uuid: Uuid::from_u128(1),
            dev: seed,
            ino: seed + 1,
            statx_mnt_id_unique: seed + 2,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn v1_slots_upgrade_explicitly_and_v1_reader_fails_closed_on_v2_binding() {
        let (_temp, registry) = registry(2);
        let root = registry.root.clone();
        let legacy_nonce = Uuid::from_u128(0x51);
        let legacy = SlotRecordV1 {
            schema_version: 1,
            slot: 0,
            generation: 7,
            owner: Some(owner(0, ManagedOwnerRole::Runner)),
            state: SlotState::IntentDurable {
                nonce: legacy_nonce,
                created_unix_secs: 11,
            },
        };
        atomic_replace_json(&registry.slots, &slot_name(0), &legacy).unwrap();
        let legacy_vacant = SlotRecordV1 {
            schema_version: 1,
            slot: 1,
            generation: 0,
            owner: None,
            state: SlotState::Vacant,
        };
        atomic_replace_json(&registry.slots, &slot_name(1), &legacy_vacant).unwrap();
        fs::remove_file(registry.root.join(SLOT_SCHEMA_MARKER)).unwrap();
        sync_dir(&registry.root).unwrap();
        drop(registry);

        let migrated = Registry::open_at(root, 2).unwrap();
        let readback = migrated.read_valid_slot(0).unwrap();
        assert_eq!(readback.schema_version, SLOT_SCHEMA_VERSION);
        assert_eq!(readback.owner, legacy.owner);
        assert_eq!(readback.generation, legacy.generation);
        assert_eq!(readback.artifact_binding, None);
        assert!(matches!(
            readback.state,
            SlotState::IntentDurable { nonce, .. } if nonce == legacy_nonce
        ));
        let old_view: SlotRecordV1 =
            serde_json::from_slice(&serde_json::to_vec(&readback).unwrap()).unwrap();
        assert!(validate_v1_slot_record(&old_view, 0).is_err());

        let intent = migrated
            .allocate_intent(12, Some(owner(1, ManagedOwnerRole::Runner)))
            .unwrap();
        let mut reservation = ManagedLaunchReservation {
            registry: migrated.clone(),
            intent: Some(intent),
        };
        reservation
            .begin_artifact_creation(directory_identity(70), "private-runner-v2")
            .unwrap();
        let v2_with_binding = migrated.read_valid_slot(reservation.key().slot()).unwrap();
        let old_view: SlotRecordV1 =
            serde_json::from_slice(&serde_json::to_vec(&v2_with_binding).unwrap()).unwrap();
        assert!(
            validate_v1_slot_record(&old_view, reservation.key().slot()).is_err(),
            "a v1 binary accepted an authoritative v2 artifact binding"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn migration_quiesces_legacy_slot_writer_and_verifies_marker_generation() {
        let (_temp, registry) = registry(1);
        let root = registry.root.clone();
        let legacy = SlotRecordV1 {
            schema_version: 1,
            slot: 0,
            generation: 4,
            owner: None,
            state: SlotState::Vacant,
        };
        atomic_replace_json(&registry.slots, &slot_name(0), &legacy).unwrap();
        fs::remove_file(registry.root.join(SLOT_SCHEMA_MARKER)).unwrap();
        sync_dir(&registry.root).unwrap();

        let paused_legacy_writer =
            OfdLock::try_acquire(&registry.guards.join(guard_name(0))).unwrap();
        let error = Registry::open_at(root.clone(), 1).unwrap_err().to_string();
        assert!(error.contains("slot 0 is busy"), "{error}");
        let still_legacy: serde_json::Value =
            read_bounded_json(&registry.slots.join(slot_name(0))).unwrap();
        assert_eq!(
            still_legacy
                .get("schema_version")
                .and_then(|value| value.as_u64()),
            Some(1)
        );
        let completed_legacy_write = SlotRecordV1 {
            generation: 5,
            ..legacy.clone()
        };
        atomic_replace_json(&registry.slots, &slot_name(0), &completed_legacy_write).unwrap();
        drop(paused_legacy_writer);

        // A concurrently spawned test child can retain an O_CLOEXEC guard FD
        // during its fork-to-exec window. Migration must stay nonblocking and
        // fail closed, so retry only that transient lock result after the
        // authoritative legacy writer has released its guard.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let migrated = loop {
            match Registry::open_at(root.clone(), 1) {
                Ok(registry) => break registry,
                Err(error)
                    if error.chain().any(|cause| {
                        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                            matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EACCES))
                        })
                    }) && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("reopen released legacy migration: {error:#}"),
            }
        };
        let migrated_record = migrated.read_valid_slot(0).unwrap();
        assert_eq!(migrated_record.schema_version, 2);
        assert_eq!(
            migrated_record.generation, completed_legacy_write.generation,
            "migration lost the final write from the quiesced v1 owner"
        );
        assert!(migrated.slot_schema_marker_ready().unwrap());

        // Marker presence is not trusted without a complete slot readback.
        atomic_replace_json(&migrated.slots, &slot_name(0), &legacy).unwrap();
        assert!(Registry::open_at(root, 1).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_v2_open_does_not_contend_on_the_migration_lock() {
        let (_temp, registry) = registry(2);
        let root = registry.root.clone();
        let migration_lock = OfdLock::try_acquire(&root.join("registry.lock")).unwrap();

        let reopened = Registry::open_at(root, 2).expect("open clean current v2 registry");

        drop(migration_lock);
        assert_eq!(reopened.read_valid_slot(0).unwrap().schema_version, 2);
        assert_eq!(reopened.read_valid_slot(1).unwrap().schema_version, 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn migration_recovers_durable_pre_and_post_exchange_generations() {
        let (_temp, registry) = registry(2);
        let root = registry.root.clone();
        let legacy = (0..2_u16)
            .map(|slot| SlotRecordV1 {
                schema_version: 1,
                slot,
                generation: u64::from(slot) + 7,
                owner: None,
                state: SlotState::Vacant,
            })
            .collect::<Vec<_>>();
        for record in &legacy {
            atomic_replace_json(&registry.slots, &slot_name(record.slot), record).unwrap();
        }
        fs::remove_file(registry.root.join(SLOT_SCHEMA_MARKER)).unwrap();
        let prepared = registry.root.join(format!(
            "{SLOT_MIGRATION_STAGING_PREFIX}{}",
            Uuid::new_v4().simple()
        ));
        create_exact_dir(&prepared).unwrap();
        for record in &legacy {
            let current = SlotRecord {
                schema_version: SLOT_SCHEMA_VERSION,
                slot: record.slot,
                generation: record.generation,
                owner: record.owner.clone(),
                artifact_binding: None,
                state: record.state.clone(),
            };
            create_exact_json_file(&prepared.join(slot_name(record.slot)), &current).unwrap();
        }
        sync_dir(&prepared).unwrap();
        sync_dir(&registry.root).unwrap();
        drop(registry);

        let migrated = Registry::open_at(root.clone(), 2).unwrap();
        assert!(migrated.slot_schema_marker_ready().unwrap());
        assert!(migrated.scan_staged_slot_generations().unwrap().is_empty());
        for record in &legacy {
            assert_eq!(
                migrated.read_valid_slot(record.slot).unwrap().generation,
                record.generation
            );
        }

        let rollback = migrated.root.join(format!(
            "{SLOT_MIGRATION_STAGING_PREFIX}{}",
            Uuid::new_v4().simple()
        ));
        create_exact_dir(&rollback).unwrap();
        for record in &legacy {
            create_exact_json_file(&rollback.join(slot_name(record.slot)), record).unwrap();
        }
        sync_dir(&rollback).unwrap();
        fs::remove_file(migrated.root.join(SLOT_SCHEMA_MARKER)).unwrap();
        sync_dir(&migrated.root).unwrap();
        drop(migrated);

        let finalized = Registry::open_at(root, 2).unwrap();
        assert!(finalized.slot_schema_marker_ready().unwrap());
        assert!(finalized.scan_staged_slot_generations().unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn migration_post_exchange_error_preserves_rollback_until_retry() {
        let _return_error =
            ScopedManagedReturnError::once("migration_after_exchange_before_root_sync");

        let (_temp, registry) = registry(1);
        let root = registry.root.clone();
        let legacy = SlotRecordV1 {
            schema_version: 1,
            slot: 0,
            generation: 19,
            owner: None,
            state: SlotState::Vacant,
        };
        atomic_replace_json(&registry.slots, &slot_name(0), &legacy).unwrap();
        fs::remove_file(registry.root.join(SLOT_SCHEMA_MARKER)).unwrap();
        sync_dir(&registry.root).unwrap();
        drop(registry);

        assert!(Registry::open_at(root.clone(), 1).is_err());
        let uncertain = Registry {
            root: root.clone(),
            slots: root.join("slots"),
            guards: root.join("guards"),
            registrations: root.join("registrations"),
            slot_count: 1,
        };
        let staged = uncertain.scan_staged_slot_generations().unwrap();
        assert!(matches!(
            staged.as_slice(),
            [StagedSlotGeneration::Legacy { .. }]
        ));

        let recovered = Registry::open_at(root, 1).unwrap();
        assert_eq!(recovered.read_valid_slot(0).unwrap().generation, 19);
        assert!(recovered.slot_schema_marker_ready().unwrap());
        assert!(recovered.scan_staged_slot_generations().unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn migration_mid_build_and_mid_reclaim_work_trees_are_restart_idempotent() {
        let (_temp, registry) = registry(2);
        let root = registry.root.clone();
        let build = root.join(format!(
            "{SLOT_MIGRATION_BUILD_PREFIX}{}",
            Uuid::new_v4().simple()
        ));
        create_exact_dir(&build).unwrap();
        create_exact_json_file(
            &build.join(slot_name(0)),
            &registry.read_valid_slot(0).unwrap(),
        )
        .unwrap();
        sync_dir(&build).unwrap();

        let gc = root.join(format!(
            "{SLOT_MIGRATION_GC_PREFIX}{}",
            Uuid::new_v4().simple()
        ));
        create_exact_dir(&gc).unwrap();
        create_exact_json_file(
            &gc.join(slot_name(1)),
            &registry.read_valid_slot(1).unwrap(),
        )
        .unwrap();
        sync_dir(&gc).unwrap();
        sync_dir(&root).unwrap();
        drop(registry);

        let reopened = Registry::open_at(root, 2).unwrap();
        assert!(!build.exists());
        assert!(!gc.exists());
        assert!(reopened.scan_staged_slot_generations().unwrap().is_empty());
        assert!(reopened.slot_schema_marker_ready().unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn migration_process_death_mid_build_and_mid_reclaim_converges_after_reopen() {
        const CHILD_MODE: &str = "LTERM_INTERNAL_MANAGED_MIGRATION_CRASH_CHILD";
        const ROOT: &str = "LTERM_INTERNAL_MANAGED_MIGRATION_CRASH_ROOT";
        const TEST_NAME: &str = "launch_registry::tests::migration_process_death_mid_build_and_mid_reclaim_converges_after_reopen";

        if std::env::var_os(CHILD_MODE).is_some() {
            let root = PathBuf::from(std::env::var_os(ROOT).expect("migration crash root"));
            let result = Registry::open_at(root, 2);
            panic!("migration crash seam did not terminate the child: {result:?}");
        }

        let crash_open = |root: &Path, failpoint: &str| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(TEST_NAME)
                .arg("--nocapture")
                .env("LTERM_INTERNAL_TEST_MODE", "1")
                .env("LTERM_INTERNAL_MANAGED_FAILPOINT", failpoint)
                .env(CHILD_MODE, "1")
                .env(ROOT, root)
                .output()
                .unwrap()
        };
        let migration_work = |root: &Path, prefix: &str| {
            fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| name.to_string_lossy().starts_with(prefix))
                .collect::<Vec<_>>()
        };

        let (_temp, registry) = registry(2);
        let root = registry.root.clone();
        for slot in 0..2_u16 {
            let legacy = SlotRecordV1 {
                schema_version: 1,
                slot,
                generation: u64::from(slot) + 41,
                owner: None,
                state: SlotState::Vacant,
            };
            atomic_replace_json(&registry.slots, &slot_name(slot), &legacy).unwrap();
        }
        fs::remove_file(registry.root.join(SLOT_SCHEMA_MARKER)).unwrap();
        sync_dir(&registry.root).unwrap();
        drop(registry);

        let mid_build = crash_open(&root, "migration_mid_build");
        assert_eq!(
            mid_build.status.code(),
            Some(86),
            "mid-build child did not die at the failpoint: stdout={} stderr={}",
            String::from_utf8_lossy(&mid_build.stdout),
            String::from_utf8_lossy(&mid_build.stderr)
        );
        assert_eq!(migration_work(&root, SLOT_MIGRATION_BUILD_PREFIX).len(), 1);
        assert!(migration_work(&root, SLOT_MIGRATION_STAGING_PREFIX).is_empty());

        let reopened = Registry::open_at(root.clone(), 2).unwrap();
        assert!(reopened.slot_schema_marker_ready().unwrap());
        assert!(reopened.scan_staged_slot_generations().unwrap().is_empty());
        assert!(migration_work(&root, SLOT_MIGRATION_BUILD_PREFIX).is_empty());
        assert!(migration_work(&root, SLOT_MIGRATION_GC_PREFIX).is_empty());
        for slot in 0..2_u16 {
            assert_eq!(
                reopened.read_valid_slot(slot).unwrap().generation,
                u64::from(slot) + 41
            );
        }

        let gc = root.join(format!(
            "{SLOT_MIGRATION_GC_PREFIX}{}",
            Uuid::new_v4().simple()
        ));
        create_exact_dir(&gc).unwrap();
        for slot in 0..2_u16 {
            create_exact_json_file(
                &gc.join(slot_name(slot)),
                &reopened.read_valid_slot(slot).unwrap(),
            )
            .unwrap();
        }
        sync_dir(&gc).unwrap();
        sync_dir(&root).unwrap();
        drop(reopened);

        let mid_reclaim = crash_open(&root, "migration_mid_gc_reclaim");
        assert_eq!(
            mid_reclaim.status.code(),
            Some(86),
            "mid-reclaim child did not die at the failpoint: stdout={} stderr={}",
            String::from_utf8_lossy(&mid_reclaim.stdout),
            String::from_utf8_lossy(&mid_reclaim.stderr)
        );
        assert!(gc.is_dir(), "mid-reclaim crash removed the GC root early");
        assert_eq!(fs::read_dir(&gc).unwrap().count(), 1);

        let recovered = Registry::open_at(root.clone(), 2).unwrap();
        assert!(recovered.slot_schema_marker_ready().unwrap());
        assert!(recovered.scan_staged_slot_generations().unwrap().is_empty());
        assert!(migration_work(&root, SLOT_MIGRATION_BUILD_PREFIX).is_empty());
        assert!(migration_work(&root, SLOT_MIGRATION_GC_PREFIX).is_empty());
        for slot in 0..2_u16 {
            assert_eq!(
                recovered.read_valid_slot(slot).unwrap().generation,
                u64::from(slot) + 41
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn legacy_slot_migration_fails_closed_without_atomic_generation_exchange() {
        let (_temp, registry) = registry(1);
        let root = registry.root.clone();
        let legacy = SlotRecordV1 {
            schema_version: 1,
            slot: 0,
            generation: 4,
            owner: None,
            state: SlotState::Vacant,
        };
        atomic_replace_json(&registry.slots, &slot_name(0), &legacy).unwrap();
        fs::remove_file(registry.root.join(SLOT_SCHEMA_MARKER)).unwrap();
        sync_dir(&registry.root).unwrap();
        drop(registry);

        let error = Registry::open_at(root.clone(), 1).unwrap_err();
        assert!(
            error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::Unsupported)
            }),
            "{error:#}"
        );
        let still_legacy: serde_json::Value =
            read_bounded_json(&root.join("slots").join(slot_name(0))).unwrap();
        assert_eq!(
            still_legacy
                .get("schema_version")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(
            fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".slots-v2-create-"))
                .count(),
            0
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn artifact_binding_is_durable_before_launch_and_blocks_recycling_until_ack() {
        let (_temp, registry) = registry(1);
        let intent = registry
            .allocate_intent(1, Some(owner(0, ManagedOwnerRole::Runner)))
            .unwrap();
        let mut reservation = ManagedLaunchReservation {
            registry: registry.clone(),
            intent: Some(intent),
        };
        let key = reservation.key();
        let pending = reservation
            .begin_artifact_creation(directory_identity(10), "private-runner")
            .unwrap();
        assert_eq!(pending.private_directory(), None);
        let durable_pending = registry.read_valid_slot(key.slot()).unwrap();
        assert_eq!(durable_pending.artifact_binding.as_deref(), Some(&pending));
        assert!(matches!(
            durable_pending.state,
            SlotState::IntentDurable { .. }
        ));

        let directory_binding = reservation
            .finish_artifact_creation(directory_identity(20))
            .unwrap();
        let directory_record = registry.read_valid_slot(key.slot()).unwrap();
        let stale_runner = ManagedArtifactBinding {
            runner: Some(ManagedArtifactIdentity { dev: 30, ino: 31 }),
            ..directory_binding.clone()
        };
        stale_runner.validate().unwrap();
        assert!(
            validate_transition(
                &directory_record,
                &SlotRecord {
                    artifact_binding: Some(Box::new(stale_runner)),
                    ..directory_record.clone()
                },
            )
            .is_err(),
            "runner identity bypassed its durable creation intent"
        );
        assert!(
            reservation
                .finish_artifact_runner(ManagedArtifactIdentity { dev: 30, ino: 31 })
                .is_err(),
            "runner completion bypassed its durable creation intent"
        );
        let runner_pending = reservation.begin_artifact_runner_creation().unwrap();
        assert!(runner_pending.runner_create_pending());
        let runner_binding = reservation
            .finish_artifact_runner(ManagedArtifactIdentity { dev: 30, ino: 31 })
            .unwrap();
        assert!(!runner_binding.runner_create_pending());
        let runner_record = registry.read_valid_slot(key.slot()).unwrap();
        let stale_socket = ManagedArtifactBinding {
            socket: Some(ManagedArtifactIdentity { dev: 40, ino: 41 }),
            ..runner_binding.clone()
        };
        stale_socket.validate().unwrap();
        assert!(
            validate_transition(
                &runner_record,
                &SlotRecord {
                    artifact_binding: Some(Box::new(stale_socket)),
                    ..runner_record.clone()
                },
            )
            .is_err(),
            "socket identity bypassed its durable creation intent"
        );
        assert!(
            reservation
                .finish_artifact_socket(ManagedArtifactIdentity { dev: 40, ino: 41 })
                .is_err(),
            "socket completion bypassed its durable creation intent"
        );
        let socket_pending = reservation.begin_artifact_socket_creation().unwrap();
        assert!(socket_pending.socket_create_pending());
        let socket_binding = reservation
            .finish_artifact_socket(ManagedArtifactIdentity { dev: 40, ino: 41 })
            .unwrap();
        assert!(!socket_binding.socket_create_pending());
        assert!(!socket_binding.owner_create_pending());
        let socket_record = registry.read_valid_slot(key.slot()).unwrap();
        let stale_owner = ManagedArtifactBinding {
            owner: Some(ManagedArtifactIdentity { dev: 50, ino: 51 }),
            ..socket_binding.clone()
        };
        stale_owner.validate().unwrap();
        let stale_owner_record = SlotRecord {
            artifact_binding: Some(Box::new(stale_owner)),
            ..socket_record.clone()
        };
        assert!(
            validate_transition(&socket_record, &stale_owner_record).is_err(),
            "owner identity bypassed its durable creation intent"
        );
        assert!(
            reservation
                .finish_artifact_owner(ManagedArtifactIdentity { dev: 50, ino: 51 })
                .is_err(),
            "owner completion bypassed its durable creation intent"
        );
        let owner_pending = reservation.begin_artifact_owner_creation().unwrap();
        assert!(owner_pending.owner_create_pending());
        assert_eq!(owner_pending.owner_file(), None);
        let binding = reservation
            .finish_artifact_owner(ManagedArtifactIdentity { dev: 50, ino: 51 })
            .unwrap();
        assert!(!binding.owner_create_pending());
        let durable = registry.read_valid_slot(key.slot()).unwrap();
        assert_eq!(durable.artifact_binding.as_deref(), Some(&binding));
        assert_eq!(
            directory_binding.private_directory(),
            Some(directory_identity(20))
        );
        assert_eq!(
            runner_binding.runner(),
            Some(ManagedArtifactIdentity { dev: 30, ino: 31 })
        );
        assert_eq!(
            socket_binding.socket(),
            Some(ManagedArtifactIdentity { dev: 40, ino: 41 })
        );
        assert_eq!(
            binding.owner_file(),
            Some(ManagedArtifactIdentity { dev: 50, ino: 51 })
        );
        assert!(matches!(durable.state, SlotState::IntentDurable { .. }));

        let (_, intent) = reservation.into_parts();
        let SlotState::IntentDurable { nonce, .. } = intent.record.state else {
            panic!("test reservation lost its intent state")
        };
        let identity_record = SlotRecord {
            state: SlotState::IdentityDurable {
                nonce,
                identity: identity(u32::MAX - 1),
                release_may_have_occurred: true,
            },
            ..intent.record.clone()
        };
        registry
            .replace_slot(&intent.record, &identity_record)
            .unwrap();
        drop(intent);
        let pending = registry
            .begin_artifact_socket_retirement(key, &binding)
            .unwrap();
        assert!(pending.socket_retire_pending());
        let binding = registry
            .finish_artifact_socket_retirement(key, &pending)
            .unwrap();
        assert!(binding.socket_retired());
        let current = registry.read_valid_slot(key.slot()).unwrap();
        let SlotState::IdentityDurable {
            nonce,
            identity,
            release_may_have_occurred,
        } = current.state.clone()
        else {
            panic!("test socket receipt lost its identity state")
        };
        let cleanup = SlotRecord {
            state: SlotState::CleanupPending {
                nonce,
                identity: identity.clone(),
                release_may_have_occurred,
            },
            ..current.clone()
        };
        registry.replace_slot(&current, &cleanup).unwrap();
        let tombstone = SlotRecord {
            state: SlotState::ResolvedTombstone {
                nonce,
                identity: Some(identity),
                resolved_unix_secs: 11,
            },
            ..cleanup.clone()
        };
        registry.replace_slot(&cleanup, &tombstone).unwrap();
        assert!(
            registry
                .allocate_intent(
                    now_unix_secs().unwrap() + TOMBSTONE_RETENTION.as_secs() + 1,
                    None,
                )
                .is_err(),
            "unacknowledged artifact tombstone was recycled"
        );
        assert!(
            registry
                .acknowledge_artifact_cleanup(key, &binding)
                .is_err()
        );
        let cleanup_binding = registry.begin_artifact_cleanup(key, &binding).unwrap();
        assert!(
            registry
                .acknowledge_artifact_cleanup(key, &cleanup_binding)
                .is_err(),
            "cleanup-pending binding was acknowledged without deletion receipt"
        );
        let mut completed_binding = cleanup_binding;
        for step in [
            ManagedArtifactCleanupStep::Ownership,
            ManagedArtifactCleanupStep::Socket,
            ManagedArtifactCleanupStep::Runner,
        ] {
            completed_binding = registry
                .begin_artifact_unlink(key, &completed_binding, step)
                .unwrap();
            completed_binding = registry
                .finish_artifact_unlink(key, &completed_binding, step)
                .unwrap();
        }
        completed_binding = registry
            .begin_artifact_unlink(
                key,
                &completed_binding,
                ManagedArtifactCleanupStep::Directory,
            )
            .unwrap();
        completed_binding = registry
            .finish_artifact_cleanup(key, &completed_binding)
            .unwrap();
        registry
            .acknowledge_artifact_cleanup(key, &completed_binding)
            .unwrap();
        let recycled = registry
            .allocate_intent(
                now_unix_secs().unwrap() + TOMBSTONE_RETENTION.as_secs() + 1,
                None,
            )
            .unwrap();
        assert_eq!(recycled.record.generation, key.generation() + 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn artifact_cleanup_retries_refresh_authoritative_progress_after_uncertain_readback() {
        let (_temp, registry) = registry(1);
        let intent = registry
            .allocate_intent(10, Some(owner(0, ManagedOwnerRole::Runner)))
            .unwrap();
        let mut reservation = ManagedLaunchReservation {
            registry: registry.clone(),
            intent: Some(intent),
        };
        let key = reservation.key();
        let _initial = reservation
            .begin_artifact_creation(directory_identity(100), "private-retry")
            .unwrap();
        let _directory = reservation
            .finish_artifact_creation(directory_identity(110))
            .unwrap();
        let _runner_pending = reservation.begin_artifact_runner_creation().unwrap();
        let _runner = reservation
            .finish_artifact_runner(ManagedArtifactIdentity { dev: 120, ino: 121 })
            .unwrap();
        let _socket_pending = reservation.begin_artifact_socket_creation().unwrap();
        let _socket = reservation
            .finish_artifact_socket(ManagedArtifactIdentity { dev: 130, ino: 131 })
            .unwrap();
        let _owner_pending = reservation.begin_artifact_owner_creation().unwrap();
        let binding = reservation
            .finish_artifact_owner(ManagedArtifactIdentity { dev: 140, ino: 141 })
            .unwrap();
        let (_, intent) = reservation.into_parts();
        let SlotState::IntentDurable { nonce, .. } = intent.record.state else {
            panic!("test reservation lost its intent state")
        };
        let identity_record = SlotRecord {
            state: SlotState::IdentityDurable {
                nonce,
                identity: identity(u32::MAX - 2),
                release_may_have_occurred: true,
            },
            ..intent.record.clone()
        };
        registry
            .replace_slot(&intent.record, &identity_record)
            .unwrap();
        drop(intent);
        let return_error = ScopedManagedReturnError::once("after_slot_replace_before_readback");
        let inject_once = || return_error.reset();
        inject_once();
        assert!(
            registry
                .begin_artifact_socket_retirement(key, &binding)
                .is_err()
        );
        let pending = registry
            .begin_artifact_socket_retirement(key, &binding)
            .unwrap();
        assert!(pending.socket_retire_pending());
        let stale_pending = pending.clone();
        inject_once();
        assert!(
            registry
                .finish_artifact_socket_retirement(key, &pending)
                .is_err()
        );
        let binding = registry
            .finish_artifact_socket_retirement(key, &stale_pending)
            .unwrap();
        assert!(binding.socket_retired());
        let current = registry.read_valid_slot(key.slot()).unwrap();
        let SlotState::IdentityDurable {
            nonce,
            identity,
            release_may_have_occurred,
        } = current.state.clone()
        else {
            panic!("test socket receipt lost its identity state")
        };
        let cleanup = SlotRecord {
            state: SlotState::CleanupPending {
                nonce,
                identity: identity.clone(),
                release_may_have_occurred,
            },
            ..current.clone()
        };
        registry.replace_slot(&current, &cleanup).unwrap();
        let tombstone = SlotRecord {
            state: SlotState::ResolvedTombstone {
                nonce,
                identity: Some(identity),
                resolved_unix_secs: 11,
            },
            ..cleanup.clone()
        };
        registry.replace_slot(&cleanup, &tombstone).unwrap();
        inject_once();
        assert!(registry.begin_artifact_cleanup(key, &binding).is_err());
        let mut current = registry.begin_artifact_cleanup(key, &binding).unwrap();
        assert!(current.cleanup_quarantine().is_some());
        for step in [
            ManagedArtifactCleanupStep::Ownership,
            ManagedArtifactCleanupStep::Socket,
            ManagedArtifactCleanupStep::Runner,
        ] {
            inject_once();
            assert!(registry.begin_artifact_unlink(key, &current, step).is_err());
            current = registry.begin_artifact_unlink(key, &current, step).unwrap();
            assert_eq!(current.cleanup_unlink_pending(), Some(step));
            let stale = current.clone();
            inject_once();
            assert!(
                registry
                    .finish_artifact_unlink(key, &current, step)
                    .is_err()
            );
            current = registry.finish_artifact_unlink(key, &stale, step).unwrap();
            assert!(current.cleanup_step_completed(step));
        }
        current = registry
            .begin_artifact_unlink(key, &current, ManagedArtifactCleanupStep::Directory)
            .unwrap();
        let stale = current.clone();
        inject_once();
        assert!(registry.finish_artifact_cleanup(key, &current).is_err());
        current = registry.finish_artifact_cleanup(key, &stale).unwrap();
        assert!(current.cleanup_completed());
        inject_once();
        assert!(
            registry
                .acknowledge_artifact_cleanup(key, &current)
                .is_err()
        );
        registry
            .acknowledge_artifact_cleanup(key, &current)
            .unwrap();
        assert!(
            registry
                .artifact_binding(key, &owner(0, ManagedOwnerRole::Runner))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn managed_owner_receipt_and_full_report_correlation_are_identity_bound() {
        let tag = owner(1, ManagedOwnerRole::Runner);
        let key = ManagedKey {
            slot: 7,
            generation: 11,
        };
        let receipt = ManagedOwnerReceipt::new(tag.clone(), key).unwrap();
        assert_eq!(receipt.owner(), &tag);
        assert_eq!(receipt.slot(), 7);
        assert_eq!(receipt.generation(), 11);

        let report = ManagedReconcileReport {
            entries: vec![ManagedReconcileEntry {
                key: Some(key),
                owner: Some(tag.clone()),
                artifact_binding: None,
                outcome: ReconcileOutcome::ResolvedTombstone,
            }],
        };
        assert_eq!(
            report.correlate_owner(&tag),
            ManagedOwnerCorrelation::Matched {
                key,
                outcome: ReconcileOutcome::ResolvedTombstone,
            }
        );

        let duplicate = ManagedReconcileReport {
            entries: vec![report.entries[0].clone(), report.entries[0].clone()],
        };
        assert_eq!(
            duplicate.correlate_owner(&tag),
            ManagedOwnerCorrelation::Unresolved
        );
    }

    #[test]
    fn genesis_creates_fixed_exact_layout_and_reopens() {
        let (_temp, registry) = registry(4);
        registry.validate_layout().expect("valid layout");
        assert_eq!(fs::read_dir(&registry.slots).unwrap().count(), 4);
        assert_eq!(fs::read_dir(&registry.guards).unwrap().count(), 4);
        assert_eq!(fs::read_dir(&registry.registrations).unwrap().count(), 4);
        let reopened = Registry::open_at(registry.root.clone(), 4).expect("reopen");
        assert_eq!(
            reopened.read_valid_slot(0).unwrap().state,
            SlotState::Vacant
        );
    }

    #[test]
    fn symlink_or_wrong_mode_fixed_file_fails_closed() {
        use std::os::unix::fs::symlink;
        let (_temp, registry) = registry(2);
        let slot = registry.slots.join(slot_name(0));
        fs::remove_file(&slot).unwrap();
        symlink(registry.slots.join(slot_name(1)), &slot).unwrap();
        assert!(registry.validate_layout().is_err());

        fs::remove_file(&slot).unwrap();
        create_exact_json_file(
            &slot,
            &SlotRecord {
                schema_version: SLOT_SCHEMA_VERSION,
                slot: 0,
                generation: 0,
                owner: None,
                artifact_binding: None,
                state: SlotState::Vacant,
            },
        )
        .unwrap();
        fs::set_permissions(&slot, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(registry.validate_layout().is_err());
    }

    #[test]
    fn corrupt_and_oversized_records_are_unknown_not_absent() {
        let (_temp, registry) = registry(2);
        let slot = registry.slots.join(slot_name(0));
        fs::write(&slot, b"not json").unwrap();
        fs::set_permissions(&slot, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(registry.read_slot(0), SlotRead::Unknown(_)));

        let slot = registry.slots.join(slot_name(1));
        fs::write(&slot, vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        fs::set_permissions(&slot, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(registry.read_slot(1), SlotRead::Unknown(_)));
    }

    #[test]
    fn legal_state_machine_rejects_shortcuts_and_generation_wrap() {
        let nonce = Uuid::from_u128(9);
        let vacant = SlotRecord {
            schema_version: SLOT_SCHEMA_VERSION,
            slot: 0,
            generation: 4,
            owner: None,
            artifact_binding: None,
            state: SlotState::Vacant,
        };
        let intent = SlotRecord {
            generation: 5,
            state: SlotState::IntentDurable {
                nonce,
                created_unix_secs: 1,
            },
            ..vacant.clone()
        };
        assert!(validate_transition(&vacant, &intent).is_ok());
        let cleanup = SlotRecord {
            state: SlotState::CleanupPending {
                nonce,
                identity: identity(10),
                release_may_have_occurred: true,
            },
            ..intent.clone()
        };
        assert!(validate_transition(&intent, &cleanup).is_err());
        let exhausted = SlotRecord {
            generation: u64::MAX,
            ..vacant.clone()
        };
        let wrapped = SlotRecord {
            generation: 0,
            state: intent.state,
            ..exhausted.clone()
        };
        assert!(validate_transition(&exhausted, &wrapped).is_err());
    }

    #[test]
    fn managed_socket_retirement_requires_durable_intent_before_receipt() {
        let nonce = Uuid::from_u128(9);
        let binding = ManagedArtifactBinding::test_value(
            nonce,
            directory_identity(10),
            "private-runner",
            Some(directory_identity(20)),
        )
        .test_with_files(
            ManagedArtifactIdentity { dev: 30, ino: 31 },
            ManagedArtifactIdentity { dev: 40, ino: 41 },
        );
        let current = SlotRecord {
            schema_version: SLOT_SCHEMA_VERSION,
            slot: 0,
            generation: 1,
            owner: Some(owner(0, ManagedOwnerRole::Runner)),
            artifact_binding: Some(Box::new(binding.clone())),
            state: SlotState::IdentityDurable {
                nonce,
                identity: identity(10),
                release_may_have_occurred: true,
            },
        };
        let pending_binding = ManagedArtifactBinding {
            socket_retire_pending: true,
            ..binding.clone()
        };
        pending_binding.validate().unwrap();
        let pending = SlotRecord {
            artifact_binding: Some(Box::new(pending_binding.clone())),
            ..current.clone()
        };
        assert!(validate_transition(&current, &pending).is_ok());

        let retired_binding = ManagedArtifactBinding {
            socket_retire_pending: false,
            socket_retired: true,
            ..pending_binding
        };
        retired_binding.validate().unwrap();
        let retired = SlotRecord {
            artifact_binding: Some(Box::new(retired_binding.clone())),
            ..pending.clone()
        };
        assert!(validate_transition(&pending, &retired).is_ok());
        let shortcut = SlotRecord {
            artifact_binding: Some(Box::new(retired_binding)),
            ..current.clone()
        };
        assert!(validate_transition(&current, &shortcut).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn old_boot_logical_absence_atomically_subsumes_every_pending_artifact_intent() {
        let cases = [
            (None, true, false, false, false),
            (
                Some(ManagedArtifactCleanupStep::Ownership),
                false,
                false,
                false,
                false,
            ),
            (
                Some(ManagedArtifactCleanupStep::Socket),
                false,
                true,
                false,
                false,
            ),
            (
                Some(ManagedArtifactCleanupStep::Runner),
                false,
                true,
                true,
                false,
            ),
            (
                Some(ManagedArtifactCleanupStep::Directory),
                false,
                true,
                true,
                true,
            ),
        ];
        for (pending, socket_retire_pending, ownership_removed, socket_removed, runner_removed) in
            cases
        {
            let (_temp, registry) = registry(1);
            let nonce = Uuid::new_v4();
            let quarantine = pending.map(|_| format!(".private.cleanup-{}", nonce.simple()));
            let binding = ManagedArtifactBinding {
                nonce,
                control_root: directory_identity(10),
                private_leaf: "private".into(),
                private_directory: Some(directory_identity(20)),
                runner: Some(ManagedArtifactIdentity { dev: 30, ino: 31 }),
                socket: Some(ManagedArtifactIdentity { dev: 40, ino: 41 }),
                owner: Some(ManagedArtifactIdentity { dev: 50, ino: 51 }),
                runner_create_pending: false,
                socket_create_pending: false,
                owner_create_pending: false,
                cleanup_quarantine: quarantine,
                cleanup_unlink_pending: pending,
                socket_retire_pending,
                socket_retired: false,
                cleanup_ownership_removed: ownership_removed,
                cleanup_socket_removed: socket_removed,
                cleanup_runner_removed: runner_removed,
                cleanup_completed: false,
            };
            binding.validate().unwrap();
            let record = SlotRecord {
                schema_version: SLOT_SCHEMA_VERSION,
                slot: 0,
                generation: 1,
                owner: Some(owner(0, ManagedOwnerRole::Runner)),
                artifact_binding: Some(Box::new(binding.clone())),
                state: SlotState::ResolvedTombstone {
                    nonce,
                    identity: None,
                    resolved_unix_secs: 1,
                },
            };
            atomic_replace_json(&registry.slots, &slot_name(0), &record).unwrap();
            let key = ManagedKey {
                slot: 0,
                generation: 1,
            };
            let completed = registry
                .finish_artifact_logical_absence(key, &binding)
                .unwrap();
            assert!(completed.cleanup_completed());
            assert!(completed.cleanup_unlink_pending().is_none());
            assert!(!completed.socket_retire_pending());
            assert!(completed.socket_retired());
            registry
                .acknowledge_artifact_cleanup(key, &completed)
                .unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_boot_never_created_binding_has_a_narrow_completion_transition() {
        let (_temp, registry) = registry(1);
        let nonce = Uuid::new_v4();
        let mut current_root = directory_identity(10);
        current_root.boot_uuid = match read_boot_uuid() {
            Evidence::Present(boot) => boot,
            evidence => panic!("boot identity unavailable: {evidence:?}"),
        };
        let binding = ManagedArtifactBinding {
            nonce,
            control_root: current_root,
            private_leaf: "private".into(),
            private_directory: None,
            runner: None,
            socket: None,
            owner: None,
            runner_create_pending: false,
            socket_create_pending: false,
            owner_create_pending: false,
            cleanup_quarantine: Some(format!(".private.cleanup-{}", nonce.simple())),
            cleanup_unlink_pending: None,
            socket_retire_pending: false,
            socket_retired: false,
            cleanup_ownership_removed: false,
            cleanup_socket_removed: false,
            cleanup_runner_removed: false,
            cleanup_completed: false,
        };
        binding.validate().unwrap();
        let record = SlotRecord {
            schema_version: SLOT_SCHEMA_VERSION,
            slot: 0,
            generation: 1,
            owner: Some(owner(0, ManagedOwnerRole::Runner)),
            artifact_binding: Some(Box::new(binding.clone())),
            state: SlotState::ResolvedTombstone {
                nonce,
                identity: None,
                resolved_unix_secs: 1,
            },
        };
        atomic_replace_json(&registry.slots, &slot_name(0), &record).unwrap();
        let completed = registry
            .finish_artifact_create_pending_absence(
                ManagedKey {
                    slot: 0,
                    generation: 1,
                },
                &binding,
            )
            .unwrap();
        assert!(completed.cleanup_completed());
    }

    #[test]
    fn owner_tag_is_bounded_stable_and_old_records_remain_compatible() {
        let old = br#"{"schema_version":1,"slot":0,"generation":0,"state":"vacant"}"#;
        let old_record: SlotRecord = serde_json::from_slice(old).unwrap();
        assert_eq!(old_record.owner, None);
        assert!(
            !String::from_utf8(serde_json::to_vec(&old_record).unwrap())
                .unwrap()
                .contains("owner")
        );

        let nil = ManagedOwnerTag {
            tournament_uuid: Uuid::nil(),
            ..owner(0, ManagedOwnerRole::Probe)
        };
        assert!(nil.validate().is_err());
        assert!(owner(2, ManagedOwnerRole::Runner).validate().is_err());

        let tagged = owner(1, ManagedOwnerRole::Runner);
        let vacant = SlotRecord {
            schema_version: SLOT_SCHEMA_VERSION,
            slot: 0,
            generation: 0,
            owner: None,
            artifact_binding: None,
            state: SlotState::Vacant,
        };
        let intent = SlotRecord {
            generation: 1,
            owner: Some(tagged.clone()),
            state: SlotState::IntentDurable {
                nonce: Uuid::from_u128(1),
                created_unix_secs: 1,
            },
            ..vacant.clone()
        };
        assert!(validate_transition(&vacant, &intent).is_ok());
        let changed = SlotRecord {
            owner: Some(owner(0, ManagedOwnerRole::Probe)),
            state: SlotState::IdentityDurable {
                nonce: Uuid::from_u128(1),
                identity: identity(42),
                release_may_have_occurred: true,
            },
            ..intent.clone()
        };
        assert!(validate_transition(&intent, &changed).is_err());
        let stable = SlotRecord {
            owner: Some(tagged),
            ..changed
        };
        assert!(validate_transition(&intent, &stable).is_ok());
    }

    #[test]
    fn typed_cgroup_membership_binds_identity_candidate_and_generation() {
        let identity = ManagedCgroupDirectoryIdentity {
            boot_uuid: Uuid::from_u128(1),
            dev: 2,
            ino: 3,
            statx_mnt_id_unique: 4,
        };
        let membership = ManagedCgroupMembership::new(
            "/delegated/tournament/candidate-1/control".into(),
            identity,
            1,
            9,
        )
        .unwrap();
        assert_eq!(membership.candidate_index(), 1);
        assert_eq!(membership.generation(), 9);
        assert_eq!(
            membership.normalized_path(),
            "/delegated/tournament/candidate-1/control"
        );
        assert!(ManagedCgroupMembership::new("relative".into(), identity, 1, 9).is_err());
        assert!(ManagedCgroupMembership::new("/valid".into(), identity, 2, 9).is_err());
        assert!(ManagedCgroupMembership::new("/valid".into(), identity, 1, 0).is_err());
    }

    #[test]
    fn registration_is_bound_to_slot_generation_and_nonce() {
        let (_temp, registry) = registry(1);
        let registration = GateRegistration {
            schema_version: GATE_SCHEMA_VERSION,
            slot: 0,
            generation: 7,
            nonce: Uuid::from_u128(7),
            identity: identity(42),
        };
        registry
            .replace_registration(
                0,
                &RegistrationRecord {
                    schema_version: GATE_SCHEMA_VERSION,
                    slot: 0,
                    generation: 7,
                    registration: Some(registration.clone()),
                },
            )
            .unwrap();
        let readback = registry.read_registration(0).unwrap();
        assert_eq!(readback.registration, Some(registration));
    }

    #[test]
    fn proc_stat_parser_handles_closing_parentheses_inside_command() {
        let mut fields = vec!["S".to_string()];
        fields.extend((4..=21).map(|value| value.to_string()));
        fields.push("987654".into());
        fields.push("23".into());
        let stat = format!("123 (worker ) name) {}", fields.join(" "));
        assert_eq!(parse_proc_stat_start_ticks(&stat).unwrap(), 987654);
        assert!(parse_proc_stat_start_ticks("123 broken").is_err());
    }

    #[test]
    fn non_linux_launch_fails_closed() {
        #[cfg(not(target_os = "linux"))]
        {
            let result = launch_managed_process(ManagedLaunchRequest {
                owner: None,
                reservation: None,
                lifetime_guard: None,
                executable_policy: ManagedExecutablePolicy::Legacy,
                placement: ManagedPlacement::None,
                auxiliary: ManagedAuxiliary::None,
                executable: PathBuf::from("/bin/echo"),
                arguments: Vec::new(),
                current_dir: None,
                environment: Vec::new(),
                stdio: ManagedStdioPolicy::Inherit,
            });
            assert!(result.unwrap_err().to_string().contains("only on Linux"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn separate_ofd_descriptions_contend() {
        let (_temp, registry) = registry(1);
        let first = OfdLock::try_acquire(&registry.guards.join(guard_name(0))).unwrap();
        assert!(OfdLock::try_acquire(&registry.guards.join(guard_name(0))).is_err());
        drop(first);
        OfdLock::try_acquire(&registry.guards.join(guard_name(0))).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exec_target_fd_survives_only_for_shebang_scripts() {
        for (contents, should_survive) in [
            (b"\x7fELF".as_slice(), false),
            (b"#!/bin/sh\n".as_slice(), true),
        ] {
            let temp = tempfile::NamedTempFile::new().unwrap();
            fs::write(temp.path(), contents).unwrap();
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let target =
                open_pinned_executable(temp.path(), ManagedExecutablePolicy::Legacy).unwrap();
            assert_ne!(
                unsafe { libc::fcntl(target.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );

            prepare_target_fd_for_exec(&target).unwrap();

            assert_eq!(
                unsafe { libc::fcntl(target.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC == 0,
                should_survive
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_commit_closes_received_target_fd() {
        let (sender, receiver) = seqpacket_pair().unwrap();
        let target = tempfile::NamedTempFile::new().unwrap();
        let target_metadata = target.as_file().metadata().unwrap();
        let count_target_fds = || {
            fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| fs::metadata(entry.path()).ok())
                .filter(|metadata| {
                    metadata.dev() == target_metadata.dev()
                        && metadata.ino() == target_metadata.ino()
                })
                .count()
        };
        let before = count_target_fds();
        assert_eq!(before, 1, "test must observe the sender-owned target FD");

        let malformed = b"{";
        let mut iovec = libc::iovec {
            iov_base: malformed.as_ptr().cast_mut().cast(),
            iov_len: malformed.len(),
        };
        let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut control = vec![0u8; control_len];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iovec;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            assert!(!header.is_null());
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize;
            std::ptr::write(
                libc::CMSG_DATA(header).cast::<RawFd>(),
                target.as_file().as_raw_fd(),
            );
            message.msg_controllen = (*header).cmsg_len;
        }
        assert_eq!(
            unsafe { libc::sendmsg(sender.as_raw_fd(), &message, libc::MSG_NOSIGNAL) },
            malformed.len() as isize
        );

        let error = recv_commit_with_fds(receiver.as_raw_fd()).unwrap_err();
        assert!(error.to_string().contains("malformed COMMIT"), "{error:#}");
        assert_eq!(
            count_target_fds(),
            before,
            "the received SCM_RIGHTS descriptor must close on JSON parse failure"
        );
    }

    #[cfg(target_os = "linux")]
    fn test_commit(descriptors: Vec<CommitDescriptor>) -> GateCommit {
        GateCommit {
            protocol: "lterm-managed-commit-v1".into(),
            slot: 0,
            generation: 1,
            nonce: Uuid::from_u128(1),
            identity: identity(42),
            descriptors,
        }
    }

    #[cfg(target_os = "linux")]
    fn send_raw_commit(fd: RawFd, bytes: &[u8], rights: &[RawFd]) {
        if rights.is_empty() {
            assert_eq!(
                unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL) },
                bytes.len() as isize
            );
            return;
        }
        let mut iovec = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let rights_bytes = std::mem::size_of_val(rights);
        let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(rights_bytes as u32) } as usize];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iovec;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&message);
            assert!(!header.is_null());
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(rights_bytes as u32) as usize;
            std::ptr::copy_nonoverlapping(
                rights.as_ptr(),
                libc::CMSG_DATA(header).cast::<RawFd>(),
                rights.len(),
            );
            message.msg_controllen = (*header).cmsg_len;
        }
        assert_eq!(
            unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) },
            bytes.len() as isize
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn commit_descriptor_shapes_fail_closed_and_close_every_received_fd() {
        let target = tempfile::NamedTempFile::new().unwrap();
        let target_metadata = target.as_file().metadata().unwrap();
        let count_target_fds = || {
            fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| fs::metadata(entry.path()).ok())
                .filter(|metadata| {
                    metadata.dev() == target_metadata.dev()
                        && metadata.ino() == target_metadata.ino()
                })
                .count()
        };
        let source = target.as_file().as_raw_fd();
        let cases = [
            (
                test_commit(vec![CommitDescriptor {
                    role: CommitDescriptorRole::TargetExecutable,
                    target_fd: None,
                    directory_identity: None,
                }]),
                vec![],
            ),
            (
                test_commit(vec![CommitDescriptor {
                    role: CommitDescriptorRole::TargetExecutable,
                    target_fd: None,
                    directory_identity: None,
                }]),
                vec![source, source],
            ),
            (
                test_commit(vec![
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                        directory_identity: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                        directory_identity: None,
                    },
                ]),
                vec![source, source],
            ),
            (
                test_commit(vec![CommitDescriptor {
                    role: CommitDescriptorRole::SyncPipeWrite,
                    target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
                    directory_identity: None,
                }]),
                vec![source],
            ),
            (
                test_commit(vec![
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                        directory_identity: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::SyncPipeWrite,
                        target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD + 1),
                        directory_identity: None,
                    },
                ]),
                vec![source, source],
            ),
            (
                test_commit(vec![
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                        directory_identity: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::PinnedRunner,
                        target_fd: Some(MANAGED_PINNED_RUNNER_TARGET_FD),
                        directory_identity: None,
                    },
                ]),
                vec![source, source],
            ),
            (
                test_commit(vec![
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                        directory_identity: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::SyncPipeWrite,
                        target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
                        directory_identity: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::PinnedRunner,
                        target_fd: Some(MANAGED_PINNED_RUNNER_TARGET_FD + 1),
                        directory_identity: None,
                    },
                ]),
                vec![source, source, source],
            ),
        ];

        for (commit, rights) in cases {
            let (sender, receiver) = seqpacket_pair().unwrap();
            let before = count_target_fds();
            send_raw_commit(
                sender.as_raw_fd(),
                &serde_json::to_vec(&commit).unwrap(),
                &rights,
            );
            assert!(recv_commit_with_fds(receiver.as_raw_fd()).is_err());
            assert_eq!(
                count_target_fds(),
                before,
                "malformed COMMIT leaked a received descriptor"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn truncated_ancillary_closes_all_kernel_delivered_fds() {
        let (sender, receiver) = seqpacket_pair().unwrap();
        let target = tempfile::NamedTempFile::new().unwrap();
        let target_metadata = target.as_file().metadata().unwrap();
        let count_target_fds = || {
            fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| fs::metadata(entry.path()).ok())
                .filter(|metadata| {
                    metadata.dev() == target_metadata.dev()
                        && metadata.ino() == target_metadata.ino()
                })
                .count()
        };
        let before = count_target_fds();
        let rights = vec![target.as_file().as_raw_fd(); MAX_COMMIT_FDS + 1];
        let descriptors = vec![
            CommitDescriptor {
                role: CommitDescriptorRole::TargetExecutable,
                target_fd: None,
                directory_identity: None,
            };
            rights.len()
        ];
        send_raw_commit(
            sender.as_raw_fd(),
            &serde_json::to_vec(&test_commit(descriptors)).unwrap(),
            &rights,
        );
        assert!(recv_commit_with_fds(receiver.as_raw_fd()).is_err());
        assert_eq!(count_target_fds(), before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sync_pipe_mapping_accepts_only_writable_pipe_and_fixed_target() {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let read = unsafe { File::from_raw_fd(fds[0]) };
        let write = unsafe { File::from_raw_fd(fds[1]) };
        assert!(SyncPipeWrite::new(write, MANAGED_SYNC_PIPE_TARGET_FD).is_ok());
        assert!(SyncPipeWrite::new(read, MANAGED_SYNC_PIPE_TARGET_FD).is_err());

        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let _read = unsafe { File::from_raw_fd(fds[0]) };
        let write = unsafe { File::from_raw_fd(fds[1]) };
        assert!(SyncPipeWrite::new(write, MANAGED_SYNC_PIPE_TARGET_FD + 1).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sync_pipe_constructor_establishes_cloexec_on_non_cloexec_input() {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), 0) }, 0);
        let _read = unsafe { File::from_raw_fd(fds[0]) };
        let write = unsafe { File::from_raw_fd(fds[1]) };
        assert_eq!(
            unsafe { libc::fcntl(write.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );

        let sync = SyncPipeWrite::new(write, MANAGED_SYNC_PIPE_TARGET_FD).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(sync.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exact_target_and_sync_commit_returns_two_typed_owned_descriptors() {
        let (sender, receiver) = seqpacket_pair().unwrap();
        let target = tempfile::NamedTempFile::new().unwrap();
        let mut pipe_fds = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let _sync_read = unsafe { File::from_raw_fd(pipe_fds[0]) };
        let sync_write = unsafe { File::from_raw_fd(pipe_fds[1]) };
        let commit = test_commit(vec![
            CommitDescriptor {
                role: CommitDescriptorRole::TargetExecutable,
                target_fd: None,
                directory_identity: None,
            },
            CommitDescriptor {
                role: CommitDescriptorRole::SyncPipeWrite,
                target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
                directory_identity: None,
            },
        ]);
        send_commit_with_fds(
            sender.as_raw_fd(),
            &commit,
            &[target.as_file().as_raw_fd(), sync_write.as_raw_fd()],
        )
        .unwrap();
        let (
            received,
            received_target,
            received_sync,
            received_runner,
            received_candidate,
            received_control,
            received_control_socket,
        ) = recv_commit_with_fds(receiver.as_raw_fd()).unwrap();
        assert_eq!(received, commit);
        let target_metadata = target.as_file().metadata().unwrap();
        let received_metadata = received_target.metadata().unwrap();
        assert_eq!(
            (received_metadata.dev(), received_metadata.ino()),
            (target_metadata.dev(), target_metadata.ino())
        );
        validate_sync_pipe_write_fd(&received_sync.unwrap()).unwrap();
        assert!(received_runner.is_none());
        assert!(received_candidate.is_none());
        assert!(received_control.is_none());
        assert!(received_control_socket.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn speculation_commit_returns_exact_sync_and_pinned_runner_authorities() {
        let (sender, receiver) = seqpacket_pair().unwrap();
        let target = tempfile::NamedTempFile::new().unwrap();
        let runner = tempfile::NamedTempFile::new().unwrap();
        let candidate = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        fs::set_permissions(candidate.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(control.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let candidate_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(candidate.path())
            .unwrap();
        let control_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(control.path())
            .unwrap();
        let control_socket_path = control.path().join("control.sock");
        let _control_listener =
            std::os::unix::net::UnixListener::bind(&control_socket_path).unwrap();
        fs::set_permissions(&control_socket_path, fs::Permissions::from_mode(0o600)).unwrap();
        let control_socket_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&control_socket_path)
            .unwrap();
        let candidate_identity = observe_managed_directory_identity(&candidate_file).unwrap();
        let control_identity = observe_managed_directory_identity(&control_file).unwrap();
        runner
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o500))
            .unwrap();
        let mut pipe_fds = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let _sync_read = unsafe { File::from_raw_fd(pipe_fds[0]) };
        let sync_write = unsafe { File::from_raw_fd(pipe_fds[1]) };
        let commit = test_commit(vec![
            CommitDescriptor {
                role: CommitDescriptorRole::TargetExecutable,
                target_fd: None,
                directory_identity: None,
            },
            CommitDescriptor {
                role: CommitDescriptorRole::SyncPipeWrite,
                target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
                directory_identity: None,
            },
            CommitDescriptor {
                role: CommitDescriptorRole::PinnedRunner,
                target_fd: Some(MANAGED_PINNED_RUNNER_TARGET_FD),
                directory_identity: None,
            },
            CommitDescriptor {
                role: CommitDescriptorRole::PinnedCandidateDirectory,
                target_fd: Some(MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD),
                directory_identity: Some(candidate_identity),
            },
            CommitDescriptor {
                role: CommitDescriptorRole::PinnedControlDirectory,
                target_fd: Some(MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD),
                directory_identity: Some(control_identity),
            },
            CommitDescriptor {
                role: CommitDescriptorRole::PinnedControlSocket,
                target_fd: Some(MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD),
                directory_identity: None,
            },
        ]);
        send_commit_with_fds(
            sender.as_raw_fd(),
            &commit,
            &[
                target.as_file().as_raw_fd(),
                sync_write.as_raw_fd(),
                runner.as_file().as_raw_fd(),
                candidate_file.as_raw_fd(),
                control_file.as_raw_fd(),
                control_socket_file.as_raw_fd(),
            ],
        )
        .unwrap();
        let (
            received,
            received_target,
            received_sync,
            received_runner,
            received_candidate,
            received_control,
            received_control_socket,
        ) = recv_commit_with_fds(receiver.as_raw_fd()).unwrap();
        assert_eq!(received, commit);
        let received_target_metadata = received_target.metadata().unwrap();
        let target_metadata = target.as_file().metadata().unwrap();
        assert_eq!(
            (
                received_target_metadata.dev(),
                received_target_metadata.ino()
            ),
            (target_metadata.dev(), target_metadata.ino())
        );
        validate_sync_pipe_write_fd(&received_sync.unwrap()).unwrap();
        let received_runner = received_runner.unwrap();
        validate_managed_pinned_runner_fd(&received_runner).unwrap();
        let received_runner_metadata = received_runner.metadata().unwrap();
        let runner_metadata = runner.as_file().metadata().unwrap();
        assert_eq!(
            (
                received_runner_metadata.dev(),
                received_runner_metadata.ino()
            ),
            (runner_metadata.dev(), runner_metadata.ino())
        );
        assert_eq!(
            observe_managed_directory_identity(&received_candidate.unwrap()).unwrap(),
            candidate_identity
        );
        assert_eq!(
            observe_managed_directory_identity(&received_control.unwrap()).unwrap(),
            control_identity
        );
        let received_control_socket = received_control_socket.unwrap();
        let socket_metadata = control_socket_file.metadata().unwrap();
        validate_managed_pinned_control_socket_fd(
            &received_control_socket,
            ManagedArtifactIdentity {
                dev: socket_metadata.dev(),
                ino: socket_metadata.ino(),
            },
        )
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn commit_authorities_bulk_relocate_before_fd10_installation() {
        const CHILD_MODE: &str = "LTERM_INTERNAL_MANAGED_COMMIT_FD10_CHILD";
        if std::env::var_os(CHILD_MODE).as_deref() == Some(std::ffi::OsStr::new("receiver")) {
            let reserve_three = File::open("/dev/null").unwrap();
            if reserve_three.as_raw_fd() != 3 {
                assert_eq!(unsafe { libc::dup3(reserve_three.as_raw_fd(), 3, 0) }, 3);
            }
            let reserve_four = File::open("/dev/null").unwrap();
            if reserve_four.as_raw_fd() != 4 {
                assert_eq!(unsafe { libc::dup3(reserve_four.as_raw_fd(), 4, 0) }, 4);
            }
            for fd in 6..=64 {
                unsafe { libc::close(fd) };
            }
            let receiver = unsafe { File::from_raw_fd(5) };
            validate_seqpacket(receiver.as_raw_fd()).unwrap();
            let (_, target, sync, runner, candidate, control, control_socket) =
                recv_commit_with_fds(receiver.as_raw_fd()).unwrap();
            assert_eq!(
                control.as_ref().unwrap().as_raw_fd(),
                MANAGED_SYNC_PIPE_TARGET_FD,
                "the fifth received authority must reproduce the fd10 collision"
            );
            let identities = [
                target
                    .metadata()
                    .map(|value| (value.dev(), value.ino()))
                    .unwrap(),
                sync.as_ref()
                    .unwrap()
                    .metadata()
                    .map(|value| (value.dev(), value.ino()))
                    .unwrap(),
                runner
                    .as_ref()
                    .unwrap()
                    .metadata()
                    .map(|value| (value.dev(), value.ino()))
                    .unwrap(),
                candidate
                    .as_ref()
                    .unwrap()
                    .metadata()
                    .map(|value| (value.dev(), value.ino()))
                    .unwrap(),
                control
                    .as_ref()
                    .unwrap()
                    .metadata()
                    .map(|value| (value.dev(), value.ino()))
                    .unwrap(),
                control_socket
                    .as_ref()
                    .unwrap()
                    .metadata()
                    .map(|value| (value.dev(), value.ino()))
                    .unwrap(),
            ];
            let (target, sync, runner, candidate, control, control_socket) =
                relocate_received_commit_authorities(
                    target,
                    sync,
                    runner,
                    candidate,
                    control,
                    control_socket,
                )
                .unwrap();
            let authorities = [
                Some(&target),
                sync.as_ref(),
                runner.as_ref(),
                candidate.as_ref(),
                control.as_ref(),
                control_socket.as_ref(),
            ];
            for (index, authority) in authorities.into_iter().enumerate() {
                let authority = authority.unwrap();
                assert!(
                    !(MANAGED_SYNC_PIPE_TARGET_FD..=MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD)
                        .contains(&authority.as_raw_fd()),
                    "authority {index} remained in the reserved target range"
                );
                assert_eq!(
                    authority
                        .metadata()
                        .map(|value| (value.dev(), value.ino()))
                        .unwrap(),
                    identities[index],
                    "authority {index} changed identity during bulk relocation"
                );
            }
            validate_sync_pipe_write_fd(sync.as_ref().unwrap()).unwrap();
            validate_managed_pinned_runner_fd(runner.as_ref().unwrap()).unwrap();
            assert!(candidate.as_ref().unwrap().metadata().unwrap().is_dir());
            assert!(control.as_ref().unwrap().metadata().unwrap().is_dir());
            let socket = control_socket.as_ref().unwrap();
            let metadata = socket.metadata().unwrap();
            validate_managed_pinned_control_socket_fd(
                socket,
                ManagedArtifactIdentity {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                },
            )
            .unwrap();

            install_sync_pipe(sync.unwrap(), MANAGED_SYNC_PIPE_TARGET_FD).unwrap();
            install_pinned_runner(runner.unwrap(), MANAGED_PINNED_RUNNER_TARGET_FD).unwrap();
            install_pinned_directory(
                candidate.unwrap(),
                MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD,
            )
            .unwrap();
            install_pinned_directory(control.unwrap(), MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD)
                .unwrap();
            install_pinned_control_socket(
                control_socket.unwrap(),
                MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD,
                ManagedArtifactIdentity {
                    dev: identities[5].0,
                    ino: identities[5].1,
                },
            )
            .unwrap();
            for (offset, fd) in
                (MANAGED_SYNC_PIPE_TARGET_FD..=MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD).enumerate()
            {
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                assert!(flags >= 0 && flags & libc::FD_CLOEXEC == 0, "fd {fd}");
                let duplicate = unsafe {
                    libc::fcntl(
                        fd,
                        libc::F_DUPFD_CLOEXEC,
                        MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD + 1,
                    )
                };
                assert!(duplicate > MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD);
                let installed = unsafe { File::from_raw_fd(duplicate) };
                let metadata = installed.metadata().unwrap();
                let identity_index = match offset {
                    0 => 1,
                    1 => 2,
                    2 => 3,
                    3 => 4,
                    4 => 5,
                    _ => unreachable!(),
                };
                assert_eq!(
                    (metadata.dev(), metadata.ino()),
                    identities[identity_index],
                    "fd {fd} changed identity during fixed-target installation"
                );
            }
            return;
        }
        if std::env::var_os(CHILD_MODE).as_deref() != Some(std::ffi::OsStr::new("sender")) {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(
                    "launch_registry::tests::commit_authorities_bulk_relocate_before_fd10_installation",
                )
                .arg("--nocapture")
                .env(CHILD_MODE, "sender")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated fd10 sender failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let (sender, receiver) = seqpacket_pair().unwrap();
        let receiver_fd = receiver.as_raw_fd();
        let duplicated_receiver = receiver_fd != 5;
        if duplicated_receiver {
            assert_eq!(unsafe { libc::dup3(receiver_fd, 5, 0) }, 5);
        } else {
            clear_cloexec(5).unwrap();
        }
        let target = tempfile::NamedTempFile::new().unwrap();
        let runner = tempfile::NamedTempFile::new().unwrap();
        runner
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o500))
            .unwrap();
        let candidate = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        fs::set_permissions(candidate.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(control.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let candidate_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(candidate.path())
            .unwrap();
        let control_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(control.path())
            .unwrap();
        let control_socket_path = control.path().join("control.sock");
        let _control_listener =
            std::os::unix::net::UnixListener::bind(&control_socket_path).unwrap();
        fs::set_permissions(&control_socket_path, fs::Permissions::from_mode(0o600)).unwrap();
        let control_socket_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&control_socket_path)
            .unwrap();
        let mut pipe_fds = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let _sync_read = unsafe { File::from_raw_fd(pipe_fds[0]) };
        let sync_write = unsafe { File::from_raw_fd(pipe_fds[1]) };
        let commit = test_commit(vec![
            CommitDescriptor {
                role: CommitDescriptorRole::TargetExecutable,
                target_fd: None,
                directory_identity: None,
            },
            CommitDescriptor {
                role: CommitDescriptorRole::SyncPipeWrite,
                target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
                directory_identity: None,
            },
            CommitDescriptor {
                role: CommitDescriptorRole::PinnedRunner,
                target_fd: Some(MANAGED_PINNED_RUNNER_TARGET_FD),
                directory_identity: None,
            },
            CommitDescriptor {
                role: CommitDescriptorRole::PinnedCandidateDirectory,
                target_fd: Some(MANAGED_PINNED_CANDIDATE_DIRECTORY_TARGET_FD),
                directory_identity: Some(
                    observe_managed_directory_identity(&candidate_file).unwrap(),
                ),
            },
            CommitDescriptor {
                role: CommitDescriptorRole::PinnedControlDirectory,
                target_fd: Some(MANAGED_PINNED_CONTROL_DIRECTORY_TARGET_FD),
                directory_identity: Some(
                    observe_managed_directory_identity(&control_file).unwrap(),
                ),
            },
            CommitDescriptor {
                role: CommitDescriptorRole::PinnedControlSocket,
                target_fd: Some(MANAGED_PINNED_CONTROL_SOCKET_TARGET_FD),
                directory_identity: None,
            },
        ]);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(
                "launch_registry::tests::commit_authorities_bulk_relocate_before_fd10_installation",
            )
            .arg("--nocapture")
            .env(CHILD_MODE, "receiver")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(receiver);
        if duplicated_receiver {
            unsafe { libc::close(5) };
        }
        send_commit_with_fds(
            sender.as_raw_fd(),
            &commit,
            &[
                target.as_file().as_raw_fd(),
                sync_write.as_raw_fd(),
                runner.as_file().as_raw_fd(),
                candidate_file.as_raw_fd(),
                control_file.as_raw_fd(),
                control_socket_file.as_raw_fd(),
            ],
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "fd10 relocation child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn placement_rejects_non_cgroup_fd_and_unbounded_or_non_normal_membership() {
        let ordinary = tempfile::NamedTempFile::new().unwrap();
        assert!(
            ControlCgroupPlacement::new_internal_test(
                ordinary.reopen().unwrap(),
                "/speculation/control".into(),
            )
            .is_err()
        );
        assert!(validate_cgroup_membership("relative/control").is_err());
        assert!(validate_cgroup_membership("/speculation/../control").is_err());
        assert!(validate_cgroup_membership(&format!("/{}", "x".repeat(1_025))).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fixed_capacity_fails_before_reusing_unresolved_records() {
        let (_temp, registry) = registry(2);
        let first = registry.allocate_intent(1, None).unwrap();
        let second = registry.allocate_intent(1, None).unwrap();
        assert!(registry.allocate_intent(1, None).is_err());
        drop((first, second));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn full_fixed_capacity_refuses_before_creating_another_intent() {
        let (_temp, registry) = registry(SLOT_COUNT);
        for slot in 0..SLOT_COUNT {
            let slot = u16::try_from(slot).unwrap();
            let vacant = registry.read_valid_slot(slot).unwrap();
            registry
                .replace_slot(
                    &vacant,
                    &SlotRecord {
                        generation: 1,
                        state: SlotState::IntentDurable {
                            nonce: Uuid::new_v4(),
                            created_unix_secs: 1,
                        },
                        ..vacant.clone()
                    },
                )
                .unwrap();
        }
        let error = registry.allocate_intent(2, None).unwrap_err().to_string();
        assert!(error.contains("1024/1024 unresolved"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unlocked_intent_is_durably_tombstoned() {
        let (_temp, registry) = registry(1);
        let intent = registry.allocate_intent(1, None).unwrap();
        let slot = intent.record.slot;
        let generation = intent.record.generation;
        let SlotState::IntentDurable { nonce, .. } = intent.record.state else {
            unreachable!()
        };
        drop(intent);

        assert_eq!(
            registry.cleanup(slot, generation).unwrap(),
            ReconcileOutcome::ResolvedTombstone
        );
        assert!(matches!(
            registry.read_valid_slot(slot).unwrap().state,
            SlotState::ResolvedTombstone {
                nonce: value,
                identity: None,
                ..
            } if value == nonce
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn busy_intent_with_absent_registered_identity_stays_unknown() {
        let (_temp, registry) = registry(1);
        let intent = registry.allocate_intent(1, None).unwrap();
        let SlotState::IntentDurable { nonce, .. } = intent.record.state else {
            unreachable!()
        };
        registry
            .replace_registration(
                intent.record.slot,
                &RegistrationRecord {
                    schema_version: GATE_SCHEMA_VERSION,
                    slot: intent.record.slot,
                    generation: intent.record.generation,
                    registration: Some(GateRegistration {
                        schema_version: GATE_SCHEMA_VERSION,
                        slot: intent.record.slot,
                        generation: intent.record.generation,
                        nonce,
                        identity: identity(u32::MAX),
                    }),
                },
            )
            .unwrap();

        assert!(matches!(
            registry
                .cleanup(intent.record.slot, intent.record.generation)
                .unwrap(),
            ReconcileOutcome::UnknownOrphanRisk(ManagedReconcileCode::BusyGuardIdentityAbsent)
        ));
        assert!(matches!(
            registry.read_valid_slot(intent.record.slot).unwrap().state,
            SlotState::IntentDurable { .. }
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owner_is_durable_before_spawn_retained_through_tombstone_and_reconciled_typed() {
        let (_temp, registry) = registry(2);
        let owner = owner(0, ManagedOwnerRole::Runner);
        let intent = registry.allocate_intent(1, Some(owner.clone())).unwrap();
        assert_eq!(
            registry.read_valid_slot(intent.record.slot).unwrap().owner,
            Some(owner.clone())
        );
        assert!(
            registry
                .allocate_intent(2, Some(owner.clone()))
                .unwrap_err()
                .to_string()
                .contains("already has a durable registry record")
        );
        let key = ManagedKey {
            slot: intent.record.slot,
            generation: intent.record.generation,
        };
        drop(intent);

        assert_eq!(
            registry.reconcile_owner(&owner).unwrap(),
            ManagedOwnerOutcome::ResolvedTombstone(key)
        );
        let tombstone = registry.read_valid_slot(key.slot).unwrap();
        assert_eq!(tombstone.owner, Some(owner.clone()));
        assert!(matches!(
            tombstone.state,
            SlotState::ResolvedTombstone { .. }
        ));
        assert_eq!(
            registry.reconcile_owner(&owner).unwrap(),
            ManagedOwnerOutcome::ResolvedTombstone(key)
        );
        let report = registry.reconcile_all();
        assert!(report.entries.iter().any(|entry| {
            entry.key == Some(key)
                && entry.owner.as_ref() == Some(&owner)
                && entry.outcome == ReconcileOutcome::ResolvedTombstone
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unknown_slot_blocks_typed_owner_allocation_before_another_intent() {
        for replacement in [b"{".to_vec(), vec![b'x'; MAX_RECORD_BYTES + 1]] {
            let (_temp, registry) = registry(2);
            let owner = owner(0, ManagedOwnerRole::Runner);
            let prior = registry.allocate_intent(1, Some(owner.clone())).unwrap();
            let prior_slot = prior.record.slot;
            drop(prior);
            fs::write(registry.slots.join(slot_name(prior_slot)), replacement).unwrap();

            let error = registry
                .allocate_intent(2, Some(owner.clone()))
                .unwrap_err()
                .to_string();
            assert!(error.contains("owner uniqueness is uncertain"), "{error}");
            assert!(matches!(
                registry.read_valid_slot(1).unwrap().state,
                SlotState::Vacant
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn controller_is_cloneable_without_cloning_waiter_ownership() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<ManagedController>();

        let (_temp, registry) = registry(1);
        let controller = ManagedController {
            inner: Arc::new(ManagedControllerInner {
                key: ManagedKey {
                    slot: 0,
                    generation: 0,
                },
                identity: identity(u32::MAX),
                owner: None,
                registry,
            }),
        };
        let clone = controller.clone();
        assert_eq!(clone.key(), controller.key());
        assert_eq!(clone.owner(), None);
        assert_eq!(clone.status(), ReconcileOutcome::Absent);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_process_identity_is_typed_and_pidfd_verified() {
        let observed = observe_identity(std::process::id());
        let Evidence::Present(identity) = observed else {
            panic!("current identity unavailable: {observed:?}");
        };
        assert!(matches!(
            open_verified_pidfd(&identity),
            Evidence::Present(_)
        ));
        let reused = ProcessIdentity {
            start_ticks: identity.start_ticks.saturating_add(1),
            ..identity
        };
        assert!(matches!(open_verified_pidfd(&reused), Evidence::Absent));
    }
}

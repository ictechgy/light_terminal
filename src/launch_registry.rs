//! Durable, Linux-only managed root-process launch substrate.
//!
//! This module is intentionally disconnected from ordinary sessions and the
//! public protocol.  A future feature may call `launch_managed_process`; until
//! then only the hidden gate dispatcher is wired into the binary.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 1;
const SLOT_COUNT: usize = 1_024;
const MAX_RECORD_BYTES: usize = 8 * 1_024;
const TOMBSTONE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const INTERNAL_GATE_ARG: &str = "__lterm-internal-managed-launch-gate-v1";
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
#[cfg(target_os = "linux")]
pub(crate) const MANAGED_SYNC_PIPE_TARGET_FD: RawFd = 10;
#[cfg(target_os = "linux")]
const MAX_COMMIT_FDS: usize = 8;

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitDescriptor {
    role: CommitDescriptorRole,
    target_fd: Option<RawFd>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GateExecFailure {
    protocol: String,
    errno: Option<i32>,
    message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReconcileOutcome {
    Absent,
    Live,
    UnknownOrphanRisk(String),
    ResolvedTombstone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ManagedKey {
    slot: u16,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedReconcileEntry {
    pub key: Option<ManagedKey>,
    pub owner: Option<ManagedOwnerTag>,
    pub outcome: ReconcileOutcome,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ManagedReconcileReport {
    pub entries: Vec<ManagedReconcileEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManagedOwnerOutcome {
    Absent,
    ResolvedTombstone(ManagedKey),
    UnknownOrphanRisk(String),
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
}

#[derive(Debug)]
pub(crate) struct ManagedLaunchRequest {
    pub owner: Option<ManagedOwnerTag>,
    pub executable_policy: ManagedExecutablePolicy,
    pub placement: ManagedPlacement,
    pub auxiliary: ManagedAuxiliary,
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub environment: Vec<(OsString, OsString)>,
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
    controller: ManagedController,
}

#[derive(Debug)]
pub(crate) struct ManagedLaunch {
    pub controller: ManagedController,
    pub waiter: ManagedWaiter,
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
            Evidence::Unavailable(reason) => ReconcileOutcome::UnknownOrphanRisk(reason),
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn terminate(&self) -> Result<ReconcileOutcome> {
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

impl ManagedKey {
    pub(crate) fn slot(self) -> u16 {
        self.slot
    }

    pub(crate) fn generation(self) -> u64 {
        self.generation
    }
}

impl ManagedWaiter {
    #[cfg(target_os = "linux")]
    pub(crate) fn wait(mut self) -> Result<std::process::ExitStatus> {
        let status = self
            .child
            .take()
            .context("managed root-process handle was already consumed")?
            .wait()
            .context("wait for managed root process")?;
        ensure!(
            self.controller.terminate()? == ReconcileOutcome::ResolvedTombstone,
            "managed root process exited without a durable tombstone"
        );
        Ok(status)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn terminate_and_wait(mut self) -> Result<std::process::ExitStatus> {
        let first = self.controller.terminate()?;
        ensure!(
            matches!(
                first,
                ReconcileOutcome::ResolvedTombstone | ReconcileOutcome::UnknownOrphanRisk(_)
            ),
            "managed cleanup did not start conservatively: {first:?}"
        );
        let status = self
            .child
            .take()
            .context("managed root-process handle was already consumed")?
            .wait()
            .context("reap managed root process after cleanup")?;
        ensure!(
            self.controller.terminate()? == ReconcileOutcome::ResolvedTombstone,
            "managed cleanup did not reach a durable tombstone after reap"
        );
        Ok(status)
    }
}

impl Drop for ManagedWaiter {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(mut child) = self.child.take() {
            let controller = self.controller.clone();
            let _ = std::thread::Builder::new()
                .name("lterm-managed-root-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                    let _ = controller.terminate();
                });
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
enum SlotRead {
    Valid(SlotRecord),
    Unknown(String),
}

impl Registry {
    #[cfg(target_os = "linux")]
    fn open_default() -> Result<Self> {
        Self::open_at(crate::paths::process_registry_dir()?, SLOT_COUNT)
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

        for slot in 0..self.slot_count {
            let slot = u16::try_from(slot).context("slot index overflow")?;
            let record = SlotRecord {
                schema_version: SCHEMA_VERSION,
                slot,
                generation: 0,
                owner: None,
                state: SlotState::Vacant,
            };
            create_exact_json_file(&slots.join(slot_name(slot)), &record)?;
            create_exact_file(&guards.join(guard_name(slot)), b"")?;
            let registration = RegistrationRecord {
                schema_version: SCHEMA_VERSION,
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
                schema_version: SCHEMA_VERSION,
                slot: vacant.slot,
                generation,
                registration: None,
            },
        )?;
        let intent = SlotRecord {
            schema_version: SCHEMA_VERSION,
            slot: vacant.slot,
            generation,
            owner,
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
        let readback = self.read_valid_slot(expected.slot)?;
        ensure!(readback == *next, "slot durable readback mismatch");
        Ok(())
    }

    fn read_registration(&self, slot: u16) -> Result<RegistrationRecord> {
        let path = self.registrations.join(registration_name(slot));
        let record = read_bounded_json::<RegistrationRecord>(&path)?;
        ensure!(record.schema_version == SCHEMA_VERSION);
        ensure!(record.slot == slot);
        Ok(record)
    }

    fn replace_registration(&self, slot: u16, next: &RegistrationRecord) -> Result<()> {
        ensure!(next.schema_version == SCHEMA_VERSION && next.slot == slot);
        if let Some(registration) = &next.registration {
            ensure!(registration.schema_version == SCHEMA_VERSION);
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
            schema_version: SCHEMA_VERSION,
            slot: intent.slot,
            generation: intent.generation,
            owner: intent.owner.clone(),
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
            return ReconcileOutcome::UnknownOrphanRisk("not an intent record".into());
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
                            "busy intent guard has an absent registered identity".into(),
                        ),
                        Evidence::Unavailable(reason) => {
                            ReconcileOutcome::UnknownOrphanRisk(reason)
                        }
                    }
                }
                Ok(_) => ReconcileOutcome::UnknownOrphanRisk(
                    "busy intent guard has missing or stale registration sidecar".into(),
                ),
                Err(err) => ReconcileOutcome::UnknownOrphanRisk(format!(
                    "busy intent guard registration unavailable: {err:#}"
                )),
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
                        "matching process remained live after pidfd signal".into(),
                    )),
                    Evidence::Unavailable(reason) => {
                        Ok(ReconcileOutcome::UnknownOrphanRisk(reason))
                    }
                }
            }
            Evidence::Absent => self.persist_tombstone(&pending, nonce, Some(identity)),
            Evidence::Unavailable(reason) => Ok(ReconcileOutcome::UnknownOrphanRisk(reason)),
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
                ) => record.owner.map(|owner| ManagedReconcileEntry {
                    key: Some(ManagedKey {
                        slot,
                        generation: record.generation,
                    }),
                    owner: Some(owner),
                    outcome: ReconcileOutcome::ResolvedTombstone,
                }),
                SlotRead::Valid(record) => Some(ManagedReconcileEntry {
                    key: Some(ManagedKey {
                        slot,
                        generation: record.generation,
                    }),
                    owner: record.owner.clone(),
                    outcome: self.cleanup(slot, record.generation).unwrap_or_else(|err| {
                        ReconcileOutcome::UnknownOrphanRisk(format!(
                            "slot {slot} reconciliation failed: {err:#}"
                        ))
                    }),
                }),
                SlotRead::Unknown(reason) => Some(ManagedReconcileEntry {
                    key: None,
                    owner: None,
                    outcome: ReconcileOutcome::UnknownOrphanRisk(reason),
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
                SlotRead::Unknown(reason) => {
                    return Ok(ManagedOwnerOutcome::UnknownOrphanRisk(format!(
                        "managed owner lookup encountered unknown slot {slot}: {reason}"
                    )));
                }
            };
            if record.owner.as_ref() == Some(owner) {
                if matched.is_some() {
                    return Ok(ManagedOwnerOutcome::UnknownOrphanRisk(
                        "duplicate managed owner records".into(),
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
                Err(err) => Ok(ManagedOwnerOutcome::UnknownOrphanRisk(format!(
                    "managed owner reconciliation failed: {err:#}"
                ))),
                Ok(outcome) => match outcome {
                    ReconcileOutcome::ResolvedTombstone => {
                        Ok(ManagedOwnerOutcome::ResolvedTombstone(key))
                    }
                    ReconcileOutcome::Absent => Ok(ManagedOwnerOutcome::Absent),
                    ReconcileOutcome::Live => Ok(ManagedOwnerOutcome::UnknownOrphanRisk(
                        "managed owner remained live after reconciliation".into(),
                    )),
                    ReconcileOutcome::UnknownOrphanRisk(reason) => {
                        Ok(ManagedOwnerOutcome::UnknownOrphanRisk(reason))
                    }
                },
            },
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn reconcile_managed_processes() -> Result<ManagedReconcileReport> {
    Ok(Registry::open_default()?.reconcile_all())
}

#[cfg(target_os = "linux")]
pub(crate) fn reconcile_managed_owner(owner: &ManagedOwnerTag) -> Result<ManagedOwnerOutcome> {
    Registry::open_default()?.reconcile_owner(owner)
}

fn validate_slot_record(record: &SlotRecord, expected_slot: u16) -> Result<()> {
    ensure!(record.schema_version == SCHEMA_VERSION);
    ensure!(record.slot == expected_slot);
    if let Some(owner) = &record.owner {
        owner.validate()?;
    }
    match &record.state {
        SlotState::Vacant => ensure!(record.owner.is_none(), "vacant slot retained an owner"),
        SlotState::IntentDurable { nonce, .. }
        | SlotState::IdentityDurable { nonce, .. }
        | SlotState::CleanupPending { nonce, .. }
        | SlotState::ResolvedTombstone { nonce, .. } => ensure!(!nonce.is_nil()),
    }
    Ok(())
}

fn validate_transition(current: &SlotRecord, next: &SlotRecord) -> Result<()> {
    ensure!(current.schema_version == SCHEMA_VERSION);
    ensure!(next.schema_version == SCHEMA_VERSION);
    ensure!(current.slot == next.slot);
    let legal = match (&current.state, &next.state) {
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
            current.generation == next.generation && next.owner.is_none()
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

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    // Non-Linux builds never launch managed processes. This fallback exists so
    // platform-neutral durability/state tests can exercise the file model.
    fs::rename(from, to)
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
pub(crate) fn launch_managed_process(request: ManagedLaunchRequest) -> Result<ManagedLaunch> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    if let Some(owner) = &request.owner {
        owner.validate()?;
    }
    let registry = Registry::open_default()?;
    managed_test_failpoint("parent_before_intent");
    let intent = registry.allocate_intent(now_unix_secs()?, request.owner.clone())?;
    managed_test_failpoint("parent_after_intent");
    let SlotState::IntentDurable { nonce, .. } = &intent.record.state else {
        unreachable!("allocator returns intent")
    };
    let nonce = *nonce;
    let target = match open_pinned_executable(&request.executable, request.executable_policy) {
        Ok(target) => target,
        Err(err) => return Err(resolve_unspawned_intent(&registry, intent, err)),
    };
    let (parent_control, child_control) = match seqpacket_pair() {
        Ok(pair) => pair,
        Err(err) => return Err(resolve_unspawned_intent(&registry, intent, err)),
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
    if let Some(current_dir) = request.current_dir {
        command.current_dir(current_dir);
    }
    unsafe {
        command.pre_exec(move || remap_gate_fds(control_fd, guard_fd, placement_fd));
    }
    let mut child = match command.spawn().context("spawn trusted managed launch gate") {
        Ok(child) => child,
        Err(err) => return Err(resolve_unspawned_intent(&registry, intent, err)),
    };
    managed_test_failpoint("parent_after_spawn");
    drop(child_control);
    let launch_result = (|| -> Result<ProcessIdentity> {
        set_socket_timeout(parent_control.as_raw_fd(), Duration::from_secs(10))?;
        let hello: GateHello = recv_packet(parent_control.as_raw_fd())?;
        ensure!(hello.protocol == "lterm-managed-hello-v1");
        ensure!(hello.registration.slot == intent.record.slot);
        ensure!(hello.registration.generation == intent.record.generation);
        ensure!(hello.registration.nonce == nonce);
        ensure!(hello.registration.identity.pid == child.id());
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
                }],
                ManagedAuxiliary::SyncPipeWrite(sync) => vec![
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::SyncPipeWrite,
                        target_fd: Some(sync.target_fd),
                    },
                ],
            },
        };
        managed_test_failpoint("parent_before_commit");
        let mut commit_fds = vec![target.as_raw_fd()];
        if let ManagedAuxiliary::SyncPipeWrite(sync) = &request.auxiliary {
            commit_fds.push(sync.file.as_raw_fd());
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
            let controller = ManagedController {
                inner: Arc::new(ManagedControllerInner {
                    key: ManagedKey {
                        slot: intent.record.slot,
                        generation: intent.record.generation,
                    },
                    identity,
                    owner: intent.record.owner.clone(),
                    registry,
                }),
            };
            Ok(ManagedLaunch {
                waiter: ManagedWaiter {
                    child: Some(child),
                    controller: controller.clone(),
                },
                controller,
            })
        }
        Err(err) => {
            drop(parent_control);
            settle_failed_launch(
                &registry,
                intent.record.slot,
                intent.record.generation,
                &mut child,
            );
            Err(err)
        }
    }
}

#[cfg(target_os = "linux")]
fn resolve_unspawned_intent(
    registry: &Registry,
    intent: LaunchIntent,
    error: anyhow::Error,
) -> anyhow::Error {
    let slot = intent.record.slot;
    let generation = intent.record.generation;
    drop(intent);
    match registry.cleanup(slot, generation) {
        Ok(ReconcileOutcome::ResolvedTombstone) => error,
        Ok(outcome) => error.context(format!(
            "pre-spawn intent did not reach a tombstone: {outcome:?}"
        )),
        Err(cleanup_error) => error.context(format!(
            "pre-spawn intent cleanup failed: {cleanup_error:#}"
        )),
    }
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
fn settle_failed_launch(
    registry: &Registry,
    slot: u16,
    generation: u64,
    child: &mut std::process::Child,
) {
    if !reap_child_until(child, Duration::from_secs(2)).unwrap_or(false) {
        let _ = registry.cleanup(slot, generation);
        let _ = reap_child_until(child, Duration::from_secs(5));
    }
    let _ = registry.cleanup(slot, generation);
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
    if std::env::var("LTERM_INTERNAL_MANAGED_FAILPOINT").as_deref() == Ok(_name) {
        unsafe { libc::_exit(86) };
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn launch_managed_process(_request: ManagedLaunchRequest) -> Result<ManagedLaunch> {
    bail!("durable managed-process launch is supported only on Linux")
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
            executable_policy: ManagedExecutablePolicy::Legacy,
            placement,
            auxiliary,
            executable,
            arguments: arguments.collect(),
            current_dir: None,
            environment: launch_environment,
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
            drop(process);
            return Ok(true);
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
    unsafe { libc::close(MANAGED_SYNC_PIPE_TARGET_FD) };
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
        schema_version: SCHEMA_VERSION,
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
            schema_version: SCHEMA_VERSION,
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
    let (commit, target, sync_pipe) = recv_commit_with_fds(control.as_raw_fd())?;
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
    ensure!(matches!(
        observe_identity(std::process::id()),
        Evidence::Present(ref actual) if actual == &identity
    ));
    let target = move_file_away_from_fd(target, MANAGED_SYNC_PIPE_TARGET_FD)?;
    validate_pinned_executable(&target, ManagedExecutablePolicy::Legacy)?;
    prepare_target_fd_for_exec(&target)?;
    if let Some(sync_pipe) = sync_pipe {
        install_sync_pipe(sync_pipe, MANAGED_SYNC_PIPE_TARGET_FD)?;
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
fn recv_commit_with_fds(fd: RawFd) -> Result<(GateCommit, File, Option<File>)> {
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
            }] | [
                CommitDescriptor {
                    role: CommitDescriptorRole::TargetExecutable,
                    target_fd: None,
                },
                CommitDescriptor {
                    role: CommitDescriptorRole::SyncPipeWrite,
                    target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
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
    ensure!(received_files.next().is_none());
    Ok((commit, target, sync_pipe))
}

#[cfg(target_os = "linux")]
fn move_file_away_from_fd(file: File, prohibited: RawFd) -> Result<File> {
    if file.as_raw_fd() != prohibited {
        return Ok(file);
    }
    let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, prohibited + 1) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error())
            .context("move pinned executable away from fixed auxiliary FD");
    }
    Ok(unsafe { File::from_raw_fd(duplicate) })
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
                schema_version: SCHEMA_VERSION,
                slot: 0,
                generation: 0,
                owner: None,
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
            schema_version: SCHEMA_VERSION,
            slot: 0,
            generation: 4,
            owner: None,
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
            schema_version: SCHEMA_VERSION,
            slot: 0,
            generation: 0,
            owner: None,
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
            schema_version: SCHEMA_VERSION,
            slot: 0,
            generation: 7,
            nonce: Uuid::from_u128(7),
            identity: identity(42),
        };
        registry
            .replace_registration(
                0,
                &RegistrationRecord {
                    schema_version: SCHEMA_VERSION,
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
                executable_policy: ManagedExecutablePolicy::Legacy,
                placement: ManagedPlacement::None,
                auxiliary: ManagedAuxiliary::None,
                executable: PathBuf::from("/bin/echo"),
                arguments: Vec::new(),
                current_dir: None,
                environment: Vec::new(),
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
                }]),
                vec![],
            ),
            (
                test_commit(vec![CommitDescriptor {
                    role: CommitDescriptorRole::TargetExecutable,
                    target_fd: None,
                }]),
                vec![source, source],
            ),
            (
                test_commit(vec![
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                    },
                ]),
                vec![source, source],
            ),
            (
                test_commit(vec![CommitDescriptor {
                    role: CommitDescriptorRole::SyncPipeWrite,
                    target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
                }]),
                vec![source],
            ),
            (
                test_commit(vec![
                    CommitDescriptor {
                        role: CommitDescriptorRole::TargetExecutable,
                        target_fd: None,
                    },
                    CommitDescriptor {
                        role: CommitDescriptorRole::SyncPipeWrite,
                        target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD + 1),
                    },
                ]),
                vec![source, source],
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
            },
            CommitDescriptor {
                role: CommitDescriptorRole::SyncPipeWrite,
                target_fd: Some(MANAGED_SYNC_PIPE_TARGET_FD),
            },
        ]);
        send_commit_with_fds(
            sender.as_raw_fd(),
            &commit,
            &[target.as_file().as_raw_fd(), sync_write.as_raw_fd()],
        )
        .unwrap();
        let (received, received_target, received_sync) =
            recv_commit_with_fds(receiver.as_raw_fd()).unwrap();
        assert_eq!(received, commit);
        let target_metadata = target.as_file().metadata().unwrap();
        let received_metadata = received_target.metadata().unwrap();
        assert_eq!(
            (received_metadata.dev(), received_metadata.ino()),
            (target_metadata.dev(), target_metadata.ino())
        );
        validate_sync_pipe_write_fd(&received_sync.unwrap()).unwrap();
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
                    schema_version: SCHEMA_VERSION,
                    slot: intent.record.slot,
                    generation: intent.record.generation,
                    registration: Some(GateRegistration {
                        schema_version: SCHEMA_VERSION,
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
            ReconcileOutcome::UnknownOrphanRisk(reason)
                if reason.contains("busy intent guard")
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

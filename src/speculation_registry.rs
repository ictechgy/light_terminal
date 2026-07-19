use crate::protocol::{
    MAX_SPECULATION_ERROR_CODES, SPECULATION_SCORE_ORDER, SpeculationCandidateStatus,
    SpeculationExitCategory, SpeculationPhase, SpeculationReasonCode, SpeculationSchemaVersion,
    SpeculationStatus, SpeculationUnixPath,
};
use crate::speculation_fs::{
    DurableDirectoryIdentity, EvidenceError, EvidenceResult, FileIdentity,
    MAX_SPECULATION_JSON_BYTES, ValidatedDirectory, atomic_create_json, atomic_replace_json,
    open_or_create_private_dir, read_json,
};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;
use uuid::Uuid;

pub const MAX_TOURNAMENT_RECORDS: usize = 1024;
pub const MAX_LIVE_TOURNAMENTS: usize = 8;
pub const TERMINAL_RETENTION_MILLIS: u64 = 7 * 24 * 60 * 60 * 1000;
const STORE_MANIFEST_LEAF: &CStr = c"store-manifest.json";
const STORE_MATERIALIZATION_WORDS: usize = MAX_TOURNAMENT_RECORDS / u64::BITS as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum TournamentStoreManifestSchema {
    #[serde(rename = "lterm.speculation.tournament-store-manifest.v1")]
    V1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TournamentStoreManifest {
    schema_version: TournamentStoreManifestSchema,
    slot_count: u16,
    materialized_slots: [u64; STORE_MATERIALIZATION_WORDS],
}

impl TournamentStoreManifest {
    fn fixed() -> Self {
        Self {
            schema_version: TournamentStoreManifestSchema::V1,
            slot_count: MAX_TOURNAMENT_RECORDS as u16,
            materialized_slots: [0; STORE_MATERIALIZATION_WORDS],
        }
    }

    fn validate(&self) -> EvidenceResult<()> {
        if self.schema_version != TournamentStoreManifestSchema::V1
            || self.slot_count as usize != MAX_TOURNAMENT_RECORDS
        {
            return Err(EvidenceError::Corrupt);
        }
        Ok(())
    }

    fn is_materialized(&self, slot: u16) -> bool {
        let slot = slot as usize;
        let word = slot / u64::BITS as usize;
        let bit = slot % u64::BITS as usize;
        self.materialized_slots[word] & (1_u64 << bit) != 0
    }

    fn merge_materialized(&mut self, present: &[u64; STORE_MATERIALIZATION_WORDS]) -> bool {
        let mut changed = false;
        for (current, present) in self.materialized_slots.iter_mut().zip(present) {
            let merged = *current | *present;
            changed |= merged != *current;
            *current = merged;
        }
        changed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TournamentRecordSchema {
    #[serde(rename = "lterm.speculation.tournament-record.v1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum TournamentSlotSchema {
    #[serde(rename = "lterm.speculation.tournament-slot.v1")]
    V1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TournamentSlotRecord {
    schema_version: TournamentSlotSchema,
    slot: u16,
    record: Option<Box<TournamentRecord>>,
}

impl TournamentSlotRecord {
    fn vacant(slot: u16) -> Self {
        Self {
            schema_version: TournamentSlotSchema::V1,
            slot,
            record: None,
        }
    }

    fn occupied(slot: u16, record: TournamentRecord) -> Self {
        Self {
            schema_version: TournamentSlotSchema::V1,
            slot,
            record: Some(Box::new(record)),
        }
    }

    fn validate(&self, expected_slot: u16) -> EvidenceResult<()> {
        if self.schema_version != TournamentSlotSchema::V1 || self.slot != expected_slot {
            return Err(EvidenceError::Corrupt);
        }
        if let Some(record) = &self.record {
            record.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateRootIdentities {
    pub source: DurableDirectoryIdentity,
    pub candidates: [DurableDirectoryIdentity; 2],
    pub ledger_root: DurableDirectoryIdentity,
    pub cgroup_root: DurableDirectoryIdentity,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCgroupRootLocator {
    canonical_path: SpeculationUnixPath,
    pub identity: DurableDirectoryIdentity,
}

impl std::fmt::Debug for PrivateCgroupRootLocator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PrivateCgroupRootLocator")
            .field("canonical_path", &self.canonical_path)
            .field("identity", &self.identity)
            .finish()
    }
}

impl PrivateCgroupRootLocator {
    pub fn from_directory(directory: &ValidatedDirectory) -> EvidenceResult<Self> {
        let path = Path::new(OsStr::from_bytes(directory.canonical_locator_bytes()));
        if !path.is_absolute() {
            return Err(EvidenceError::InvalidDirectory);
        }
        let canonical_path =
            SpeculationUnixPath::from_path(path).map_err(|_| EvidenceError::InvalidDirectory)?;
        Ok(Self {
            canonical_path,
            identity: directory.identity(),
        })
    }

    pub fn reopen_and_verify(&self) -> EvidenceResult<ValidatedDirectory> {
        let directory = crate::speculation_fs::open_existing_delegated_cgroup_root(
            &self.canonical_path.to_path_buf(),
        )?;
        if directory.identity() != self.identity {
            return Err(EvidenceError::Stale);
        }
        Ok(directory)
    }

    #[cfg(test)]
    pub(crate) fn test_value(identity: DurableDirectoryIdentity) -> Self {
        Self {
            canonical_path: SpeculationUnixPath::from_path(Path::new("/cgroup"))
                .expect("fixed test cgroup path"),
            identity,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TournamentCgroupLifecycleState {
    Planned,
    CreatePending,
    Created,
    RemovePending,
    Removed,
    RollbackRequired,
}

impl TournamentCgroupLifecycleState {
    pub fn is_legal_transition(self, next: Self) -> bool {
        use TournamentCgroupLifecycleState as S;
        matches!(
            (self, next),
            (
                S::Planned,
                S::Planned | S::CreatePending | S::Removed | S::RollbackRequired
            ) | (
                S::CreatePending,
                S::CreatePending | S::Created | S::RollbackRequired
            ) | (
                S::Created,
                S::Created | S::RemovePending | S::RollbackRequired
            ) | (
                S::RemovePending,
                S::RemovePending | S::Removed | S::RollbackRequired
            ) | (S::Removed, S::Removed | S::RollbackRequired)
                | (S::RollbackRequired, S::RollbackRequired)
        )
    }
}

/// Durable evidence for the shared task-free tournament domain above the two
/// candidate domains.  Its deterministic name is derived from the tournament
/// UUID and its identity is independently persisted before child creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TournamentCgroupEvidence {
    pub deterministic_name_uuid: Uuid,
    pub lifecycle: TournamentCgroupLifecycleState,
    pub domain: Option<DurableDirectoryIdentity>,
}

impl TournamentCgroupEvidence {
    fn validate(&self, tournament_uuid: Uuid, boot_uuid: Uuid) -> EvidenceResult<()> {
        if self.deterministic_name_uuid != tournament_uuid
            || self.domain.is_some_and(|identity| {
                identity.boot_uuid != boot_uuid || !valid_directory_identity(&identity)
            })
        {
            return Err(EvidenceError::Corrupt);
        }
        let should_have_identity = matches!(
            self.lifecycle,
            TournamentCgroupLifecycleState::Created | TournamentCgroupLifecycleState::RemovePending
        );
        if self.domain.is_some() != should_have_identity {
            return Err(EvidenceError::Corrupt);
        }
        Ok(())
    }

    fn immutable_identity_progresses_to(&self, next: &Self) -> bool {
        self.deterministic_name_uuid == next.deterministic_name_uuid
            && self.domain.is_none_or(|identity| {
                next.domain == Some(identity)
                    || (next.lifecycle == TournamentCgroupLifecycleState::Removed
                        && next.domain.is_none())
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedOwnerRoleEvidence {
    Probe,
    Runner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedOwnerEvidence {
    pub candidate_index: u8,
    pub role: ManagedOwnerRoleEvidence,
    pub slot: u16,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CgroupForwardState {
    Planned,
    ParentCreatePending,
    ParentCreated,
    ControlCreatePending,
    ControlCreated,
    PayloadCreatePending,
    PayloadCreated,
    ProbePending,
    ProbeEmpty,
    ControlAttachPending,
    ControlAttached,
    PayloadFdTransferPending,
    PayloadArmed,
    PayloadExecPending,
    PayloadAttached,
    PayloadKillPending,
    PayloadEmpty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    content = "detail",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CgroupLifecycleState {
    Forward(CgroupForwardState),
    CleanupPending { from: CgroupForwardState },
    ParentKillPending { from: CgroupForwardState },
    ParentEmpty { from: CgroupForwardState },
    PayloadRemovePending { from: CgroupForwardState },
    PayloadRemoved { from: CgroupForwardState },
    ControlRemovePending { from: CgroupForwardState },
    ControlRemoved { from: CgroupForwardState },
    ParentRemovePending { from: CgroupForwardState },
    Removed,
    RollbackRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupComponent {
    Parent,
    Control,
    Payload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsenceDisposition {
    RequiredNeverCreated,
    RetryCreate,
    Forbidden,
    AcceptRemoval,
}

impl CgroupLifecycleState {
    pub fn is_legal_transition(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        use CgroupForwardState as F;
        use CgroupLifecycleState as L;
        match (self, next) {
            (L::Forward(from), L::CleanupPending { from: retained }) => from == retained,
            (L::Forward(F::Planned), L::Forward(F::ParentCreatePending))
            | (L::Forward(F::Planned), L::Removed)
            | (L::Forward(F::ParentCreatePending), L::Forward(F::ParentCreated))
            | (L::Forward(F::ParentCreated), L::Forward(F::ControlCreatePending))
            | (L::Forward(F::ControlCreatePending), L::Forward(F::ControlCreated))
            | (L::Forward(F::ControlCreated), L::Forward(F::PayloadCreatePending))
            | (L::Forward(F::PayloadCreatePending), L::Forward(F::PayloadCreated))
            | (L::Forward(F::PayloadCreated), L::Forward(F::ProbePending))
            | (L::Forward(F::ProbePending), L::Forward(F::ProbeEmpty))
            | (L::Forward(F::ProbeEmpty), L::Forward(F::ControlAttachPending))
            | (L::Forward(F::ControlAttachPending), L::Forward(F::ControlAttached))
            | (L::Forward(F::ControlAttached), L::Forward(F::PayloadFdTransferPending))
            | (L::Forward(F::PayloadFdTransferPending), L::Forward(F::PayloadArmed))
            | (L::Forward(F::PayloadArmed), L::Forward(F::PayloadExecPending))
            | (L::Forward(F::PayloadExecPending), L::Forward(F::PayloadAttached))
            | (L::Forward(F::PayloadAttached), L::Forward(F::PayloadKillPending))
            | (L::Forward(F::PayloadKillPending), L::Forward(F::PayloadEmpty))
            | (L::Removed, L::Removed)
            | (L::RollbackRequired, L::RollbackRequired) => true,
            (L::CleanupPending { from }, L::ParentKillPending { from: next })
            | (L::CleanupPending { from }, L::ParentEmpty { from: next })
            | (L::ParentKillPending { from }, L::ParentEmpty { from: next })
            | (L::ParentEmpty { from }, L::PayloadRemovePending { from: next })
            | (L::PayloadRemovePending { from }, L::PayloadRemoved { from: next })
            | (L::PayloadRemoved { from }, L::ControlRemovePending { from: next })
            | (L::ControlRemovePending { from }, L::ControlRemoved { from: next })
            | (L::ControlRemoved { from }, L::ParentRemovePending { from: next }) => from == next,
            (L::ParentRemovePending { .. }, L::Removed) => true,
            (_, L::RollbackRequired) => true,
            _ => false,
        }
    }

    pub fn same_boot_absence(self, component: CgroupComponent) -> AbsenceDisposition {
        use AbsenceDisposition as A;
        use CgroupComponent as C;
        use CgroupForwardState as F;
        use CgroupLifecycleState as L;
        match (self, component) {
            (L::Forward(F::Planned), _) => A::RequiredNeverCreated,
            (L::Forward(F::ParentCreatePending), C::Parent) => A::RetryCreate,
            (L::Forward(F::ParentCreatePending), _) => A::RequiredNeverCreated,
            (L::Forward(F::ParentCreated | F::ControlCreatePending), C::Parent) => A::Forbidden,
            (L::Forward(F::ControlCreatePending), C::Control) => A::RetryCreate,
            (L::Forward(F::ParentCreated | F::ControlCreatePending), _) => A::RequiredNeverCreated,
            (L::Forward(F::ControlCreated | F::PayloadCreatePending), C::Payload) => {
                if matches!(self, L::Forward(F::PayloadCreatePending)) {
                    A::RetryCreate
                } else {
                    A::RequiredNeverCreated
                }
            }
            (L::Forward(F::ControlCreated | F::PayloadCreatePending), _) => A::Forbidden,
            (L::Forward(_), _) => A::Forbidden,
            (L::CleanupPending { from }, component)
            | (L::ParentKillPending { from }, component)
            | (L::ParentEmpty { from }, component) => {
                if component_was_created(from, component) {
                    A::Forbidden
                } else {
                    A::RequiredNeverCreated
                }
            }
            (L::PayloadRemovePending { .. }, C::Payload)
            | (L::PayloadRemoved { .. }, C::Payload)
            | (L::ControlRemovePending { .. }, C::Payload | C::Control)
            | (L::ControlRemoved { .. }, C::Payload | C::Control)
            | (L::ParentRemovePending { .. }, _)
            | (L::Removed, _) => A::AcceptRemoval,
            (
                L::PayloadRemovePending { from }
                | L::PayloadRemoved { from }
                | L::ControlRemovePending { from }
                | L::ControlRemoved { from },
                component,
            ) => {
                if component_was_created(from, component) {
                    A::Forbidden
                } else {
                    A::RequiredNeverCreated
                }
            }
            (L::RollbackRequired, _) => A::Forbidden,
        }
    }
}

fn component_was_created(from: CgroupForwardState, component: CgroupComponent) -> bool {
    use CgroupComponent as C;
    use CgroupForwardState as F;
    match component {
        C::Parent => !matches!(from, F::Planned | F::ParentCreatePending),
        C::Control => !matches!(
            from,
            F::Planned | F::ParentCreatePending | F::ParentCreated | F::ControlCreatePending
        ),
        C::Payload => matches!(
            from,
            F::PayloadCreated
                | F::ProbePending
                | F::ProbeEmpty
                | F::ControlAttachPending
                | F::ControlAttached
                | F::PayloadFdTransferPending
                | F::PayloadArmed
                | F::PayloadExecPending
                | F::PayloadAttached
                | F::PayloadKillPending
                | F::PayloadEmpty
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateCgroupEvidence {
    pub candidate_index: u8,
    pub deterministic_name_uuid: Uuid,
    pub lifecycle: CgroupLifecycleState,
    pub parent: Option<DurableDirectoryIdentity>,
    pub control: Option<DurableDirectoryIdentity>,
    pub payload: Option<DurableDirectoryIdentity>,
}

impl CandidateCgroupEvidence {
    fn validate(&self, expected_index: u8, boot_uuid: Uuid) -> EvidenceResult<()> {
        if self.candidate_index != expected_index
            || self.deterministic_name_uuid.is_nil()
            || self
                .parent
                .iter()
                .chain(self.control.iter())
                .chain(self.payload.iter())
                .any(|identity| identity.boot_uuid != boot_uuid)
            || self
                .parent
                .iter()
                .chain(self.control.iter())
                .chain(self.payload.iter())
                .any(|identity| !valid_directory_identity(identity))
        {
            return Err(EvidenceError::Corrupt);
        }
        use CgroupForwardState as F;
        let creation_floor = match self.lifecycle {
            CgroupLifecycleState::Forward(F::Planned | F::ParentCreatePending) => 0,
            CgroupLifecycleState::Forward(F::ParentCreated | F::ControlCreatePending) => 1,
            CgroupLifecycleState::Forward(F::ControlCreated | F::PayloadCreatePending) => 2,
            CgroupLifecycleState::Forward(_) => 3,
            CgroupLifecycleState::CleanupPending { from }
            | CgroupLifecycleState::ParentKillPending { from }
            | CgroupLifecycleState::ParentEmpty { from }
            | CgroupLifecycleState::PayloadRemovePending { from }
            | CgroupLifecycleState::PayloadRemoved { from }
            | CgroupLifecycleState::ControlRemovePending { from }
            | CgroupLifecycleState::ControlRemoved { from }
            | CgroupLifecycleState::ParentRemovePending { from } => match from {
                F::Planned | F::ParentCreatePending => 0,
                F::ParentCreated | F::ControlCreatePending => 1,
                F::ControlCreated | F::PayloadCreatePending => 2,
                _ => 3,
            },
            CgroupLifecycleState::Removed | CgroupLifecycleState::RollbackRequired => 0,
        };
        let present_floor = match (
            self.parent.is_some(),
            self.control.is_some(),
            self.payload.is_some(),
        ) {
            (false, false, false) => 0,
            (true, false, false) => 1,
            (true, true, false) => 2,
            (true, true, true) => 3,
            _ => return Err(EvidenceError::Corrupt),
        };
        let expected_present_floor = match self.lifecycle {
            CgroupLifecycleState::PayloadRemoved { .. }
            | CgroupLifecycleState::ControlRemovePending { .. } => creation_floor.min(2),
            CgroupLifecycleState::ControlRemoved { .. }
            | CgroupLifecycleState::ParentRemovePending { .. } => creation_floor.min(1),
            CgroupLifecycleState::Removed => 0,
            CgroupLifecycleState::RollbackRequired => present_floor,
            _ => creation_floor,
        };
        if present_floor != expected_present_floor {
            return Err(EvidenceError::Corrupt);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TournamentRecord {
    pub schema_version: TournamentRecordSchema,
    pub boot_uuid: Uuid,
    pub roots: PrivateRootIdentities,
    pub cgroup_root_locator: PrivateCgroupRootLocator,
    pub tournament_cgroup: TournamentCgroupEvidence,
    pub cgroups: [CandidateCgroupEvidence; 2],
    pub managed_owners: [Option<ManagedOwnerEvidence>; 2],
    /// Private recovery-only evidence. This is never projected into the public
    /// status or client ledger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_prior_phase: Option<SpeculationPhase>,
    pub terminal_completed_unix_ms: Option<u64>,
    pub status: SpeculationStatus,
}

impl TournamentRecord {
    pub fn validate(&self) -> EvidenceResult<()> {
        self.validate_core()?;
        if self.terminal_completed_unix_ms.is_some() != self.is_positive_terminal() {
            return Err(EvidenceError::Corrupt);
        }
        Ok(())
    }

    fn validate_core(&self) -> EvidenceResult<()> {
        if self.schema_version != TournamentRecordSchema::V1
            || self.boot_uuid.is_nil()
            || !valid_directory_identity(&self.roots.source)
            || !valid_directory_identity(&self.roots.ledger_root)
            || !valid_directory_identity(&self.roots.cgroup_root)
            || self
                .roots
                .candidates
                .iter()
                .any(|identity| !valid_directory_identity(identity))
            || self.boot_uuid != self.roots.source.boot_uuid
            || self.boot_uuid != self.roots.ledger_root.boot_uuid
            || self.boot_uuid != self.roots.cgroup_root.boot_uuid
            || self
                .roots
                .candidates
                .iter()
                .any(|identity| identity.boot_uuid != self.boot_uuid)
            || self.cgroup_root_locator.identity != self.roots.cgroup_root
            || self
                .tournament_cgroup
                .validate(self.status.tournament_uuid, self.boot_uuid)
                .is_err()
            || self.cgroups[0].validate(0, self.boot_uuid).is_err()
            || self.cgroups[1].validate(1, self.boot_uuid).is_err()
            || self.cgroups[0].deterministic_name_uuid != self.status.candidates[0].candidate_uuid
            || self.cgroups[1].deterministic_name_uuid != self.status.candidates[1].candidate_uuid
            || validate_status_semantics(&self.status).is_err()
            || self
                .managed_owners
                .iter()
                .enumerate()
                .any(|(index, owner)| {
                    owner.as_ref().is_some_and(|owner| {
                        owner.candidate_index != index as u8 || owner.generation == 0
                    })
                })
            || self.restart_prior_phase.is_some_and(|prior| {
                prior.is_terminal()
                    || prior == SpeculationPhase::RollbackRequired
                    || !matches!(
                        self.status.phase,
                        SpeculationPhase::RollbackRequired
                            | SpeculationPhase::RollbackPending
                            | SpeculationPhase::RolledBack
                    )
            })
        {
            return Err(EvidenceError::Corrupt);
        }
        Ok(())
    }

    pub fn is_positive_terminal(&self) -> bool {
        self.validate_core().is_ok()
            && self.status.is_terminal()
            && !self.status.rollback_required
            && self
                .cgroups
                .iter()
                .all(|candidate| candidate.lifecycle == CgroupLifecycleState::Removed)
            && self.tournament_cgroup.lifecycle == TournamentCgroupLifecycleState::Removed
            && self.status.candidates.iter().all(|candidate| {
                let cleanup = &candidate.cleanup;
                cleanup.runner_ack
                    && cleanup.bwrap_reaped
                    && cleanup.sync_eof
                    && cleanup.cgroup_empty
                    && cleanup.managed_tombstone
            })
    }

    fn immutable_identity_matches(&self, next: &Self) -> bool {
        self.boot_uuid == next.boot_uuid
            && self.roots == next.roots
            && self.cgroup_root_locator == next.cgroup_root_locator
            && self
                .tournament_cgroup
                .immutable_identity_progresses_to(&next.tournament_cgroup)
            && self.status.tournament_uuid == next.status.tournament_uuid
            && self.status.daemon_instance_uuid == next.status.daemon_instance_uuid
            && self.status.candidates[0].candidate_uuid == next.status.candidates[0].candidate_uuid
            && self.status.candidates[1].candidate_uuid == next.status.candidates[1].candidate_uuid
    }

    fn public_evidence_progresses_to(
        &self,
        next: &Self,
        retain_reason: bool,
        allow_rollback_selection_clear: bool,
    ) -> bool {
        self.status.lease_deadline_unix_ms <= next.status.lease_deadline_unix_ms
            && (!retain_reason
                || option_is_retained(self.status.reason_code, next.status.reason_code))
            && (allow_rollback_selection_clear
                || option_is_retained(self.status.selected_index, next.status.selected_index))
            && slice_is_prefix(
                self.status.error_codes.as_slice(),
                next.status.error_codes.as_slice(),
            )
            && self
                .status
                .candidates
                .iter()
                .zip(next.status.candidates.iter())
                .all(|(current, next)| candidate_progresses_to(current, next))
            && self
                .managed_owners
                .iter()
                .zip(next.managed_owners.iter())
                .all(|(current, next)| managed_owner_progresses_to(current.as_ref(), next.as_ref()))
    }
}

fn option_is_retained<T: Copy + Eq>(current: Option<T>, next: Option<T>) -> bool {
    current.is_none_or(|value| next == Some(value))
}

fn slice_is_prefix<T: Eq>(current: &[T], next: &[T]) -> bool {
    next.starts_with(current)
}

fn candidate_progresses_to(
    current: &SpeculationCandidateStatus,
    next: &SpeculationCandidateStatus,
) -> bool {
    current.candidate_uuid == next.candidate_uuid
        && current.index == next.index
        && (!current.ready || next.ready)
        && option_is_retained(current.ready_elapsed_ns, next.ready_elapsed_ns)
        && (!current.go_received || next.go_received)
        && option_is_retained(current.go_received_elapsed_ns, next.go_received_elapsed_ns)
        && (!current.result_accepted || next.result_accepted)
        && option_is_retained(current.exit_success, next.exit_success)
        && option_is_retained(current.exit_category, next.exit_category)
        && option_is_retained(current.elapsed_ns, next.elapsed_ns)
        && option_is_retained(current.output_bytes, next.output_bytes)
        && (!current.eligible || next.eligible)
        && cleanup_progresses_to(current.cleanup, next.cleanup)
}

fn cleanup_progresses_to(
    current: crate::protocol::SpeculationCleanupStatus,
    next: crate::protocol::SpeculationCleanupStatus,
) -> bool {
    (!current.runner_ack || next.runner_ack)
        && (!current.bwrap_reaped || next.bwrap_reaped)
        && (!current.sync_eof || next.sync_eof)
        && (!current.cgroup_empty || next.cgroup_empty)
        && (!current.managed_tombstone || next.managed_tombstone)
}

fn managed_owner_progresses_to(
    current: Option<&ManagedOwnerEvidence>,
    next: Option<&ManagedOwnerEvidence>,
) -> bool {
    match (current, next) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(current), Some(next)) if current == next => true,
        (Some(current), Some(next)) => {
            current.candidate_index == next.candidate_index
                && current.role == ManagedOwnerRoleEvidence::Probe
                && next.role == ManagedOwnerRoleEvidence::Runner
        }
    }
}

fn valid_directory_identity(identity: &DurableDirectoryIdentity) -> bool {
    !identity.boot_uuid.is_nil()
        && identity.dev != 0
        && identity.ino != 0
        && identity.statx_mnt_id_unique != 0
}

fn validate_status_semantics(status: &SpeculationStatus) -> EvidenceResult<()> {
    if status.schema_version != SpeculationSchemaVersion::V1
        || status.tournament_uuid.is_nil()
        || status.daemon_instance_uuid.is_nil()
        || status.generation == 0
        || status.fixed_score_order != SPECULATION_SCORE_ORDER
        || status.candidates[0].index != 0
        || status.candidates[1].index != 1
        || status.candidates[0].candidate_uuid.is_nil()
        || status.candidates[1].candidate_uuid.is_nil()
        || status.candidates[0].candidate_uuid == status.candidates[1].candidate_uuid
        || status.error_codes.as_slice().len() > MAX_SPECULATION_ERROR_CODES
        || status
            .candidates
            .iter()
            .any(|candidate| validate_candidate_status(candidate).is_err())
    {
        return Err(EvidenceError::Corrupt);
    }

    let rollback_phase = matches!(
        status.phase,
        SpeculationPhase::RollbackRequired
            | SpeculationPhase::DecisionUncertain
            | SpeculationPhase::RollbackPending
    );
    if status.rollback_required != rollback_phase {
        return Err(EvidenceError::Corrupt);
    }

    match status.phase {
        SpeculationPhase::Selected => {
            let selected = status.selected_index.ok_or(EvidenceError::Corrupt)?;
            if selected > 1
                || status.candidates.iter().any(|candidate| {
                    !candidate.ready || !candidate.go_received || !candidate.result_accepted
                })
                || !status.candidates[selected as usize].eligible
                || score_selected_index(&status.candidates) != Some(selected)
            {
                return Err(EvidenceError::Corrupt);
            }
        }
        SpeculationPhase::RolledBack
        | SpeculationPhase::RollbackRequired
        | SpeculationPhase::DecisionUncertain
        | SpeculationPhase::RollbackPending => {
            if status.selected_index.is_some() {
                return Err(EvidenceError::Corrupt);
            }
        }
        _ => {
            if status.selected_index.is_some_and(|selected| {
                selected > 1 || score_selected_index(&status.candidates) != Some(selected)
            }) {
                return Err(EvidenceError::Corrupt);
            }
        }
    }
    Ok(())
}

fn validate_candidate_status(candidate: &SpeculationCandidateStatus) -> EvidenceResult<()> {
    if candidate.ready != candidate.ready_elapsed_ns.is_some()
        || candidate.go_received != candidate.go_received_elapsed_ns.is_some()
        || (candidate.go_received && !candidate.ready)
        || (candidate.result_accepted && !candidate.go_received)
        || (candidate.eligible && (!candidate.result_accepted || !candidate.go_received))
    {
        return Err(EvidenceError::Corrupt);
    }
    let complete_result = candidate.exit_success.is_some()
        && candidate.exit_category.is_some()
        && candidate.elapsed_ns.is_some()
        && candidate.output_bytes.is_some();
    let empty_result = candidate.exit_success.is_none()
        && candidate.exit_category.is_none()
        && candidate.elapsed_ns.is_none()
        && candidate.output_bytes.is_none();
    if if candidate.result_accepted {
        !complete_result
    } else {
        !empty_result || candidate.eligible
    } {
        return Err(EvidenceError::Corrupt);
    }
    if let (Some(success), Some(category)) = (candidate.exit_success, candidate.exit_category) {
        if success != matches!(category, SpeculationExitCategory::ExitedZero) {
            return Err(EvidenceError::Corrupt);
        }
        if candidate.eligible
            && (matches!(
                category,
                SpeculationExitCategory::SpawnFailed
                    | SpeculationExitCategory::OutputLimitExceeded
                    | SpeculationExitCategory::EvidenceIncomplete
            ) || candidate
                .output_bytes
                .is_none_or(|bytes| bytes > crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES))
        {
            return Err(EvidenceError::Corrupt);
        }
    }
    Ok(())
}

fn score_selected_index(candidates: &[SpeculationCandidateStatus; 2]) -> Option<u8> {
    if !candidates.iter().any(|candidate| candidate.eligible) {
        return None;
    }
    let score = |candidate: &SpeculationCandidateStatus| {
        (
            candidate.eligible,
            candidate.exit_success,
            std::cmp::Reverse(candidate.elapsed_ns),
            std::cmp::Reverse(candidate.output_bytes),
            std::cmp::Reverse(candidate.index),
        )
    };
    if score(&candidates[0]) >= score(&candidates[1]) {
        Some(candidates[0].index)
    } else {
        Some(candidates[1].index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TournamentKey {
    slot: u16,
    generation: u64,
    tournament_uuid: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TournamentWriteKind {
    LivePhaseTransition,
    SamePhaseEvidence,
    OldBootAbsence { current_boot_uuid: Uuid },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTournamentUpdate {
    pub key: TournamentKey,
    pub record: TournamentRecord,
}

impl TournamentKey {
    pub fn slot(&self) -> u16 {
        self.slot
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn tournament_uuid(&self) -> Uuid {
        self.tournament_uuid
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TournamentRecoveryRecord {
    Valid {
        key: TournamentKey,
        record: Box<TournamentRecord>,
    },
    Corrupt {
        slot: Option<u16>,
    },
}

enum SlotState {
    Vacant {
        identity: Option<FileIdentity>,
    },
    Valid {
        record: Box<TournamentRecord>,
        identity: FileIdentity,
    },
    Corrupt,
}

struct StoreState {
    slots: Vec<SlotState>,
    unknown_corrupt_records: usize,
}

pub struct TournamentStore {
    directory: ValidatedDirectory,
    state: Mutex<StoreState>,
}

impl std::fmt::Debug for TournamentStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TournamentStore")
            .field("directory_identity", &self.directory.identity())
            .finish_non_exhaustive()
    }
}

impl TournamentStore {
    pub fn open_or_create(path: &Path) -> EvidenceResult<Self> {
        Self::open(open_or_create_private_dir(path)?)
    }

    pub fn open(directory: ValidatedDirectory) -> EvidenceResult<Self> {
        let mut leaves = directory.list_leaf_names()?;
        if !leaves
            .iter()
            .any(|leaf| leaf.as_c_str() == STORE_MANIFEST_LEAF)
        {
            if !leaves.is_empty() {
                return Err(EvidenceError::Corrupt);
            }
            match atomic_create_json(
                &directory,
                STORE_MANIFEST_LEAF,
                &TournamentStoreManifest::fixed(),
                MAX_SPECULATION_JSON_BYTES,
            ) {
                Ok(_) | Err(EvidenceError::AlreadyExists) => {}
                Err(error) => return Err(error),
            }
            leaves = directory.list_leaf_names()?;
        }
        let mut present_slots = [0_u64; STORE_MATERIALIZATION_WORDS];
        for leaf in &leaves {
            if let Some(slot) = parse_slot_leaf(leaf) {
                mark_materialized_word(&mut present_slots, slot);
            }
        }
        let manifest = persist_materialized_slots(&directory, &present_slots)?;
        let mut slots = (0..MAX_TOURNAMENT_RECORDS)
            .map(|slot| {
                if manifest.is_materialized(slot as u16) {
                    SlotState::Corrupt
                } else {
                    SlotState::Vacant { identity: None }
                }
            })
            .collect::<Vec<_>>();
        let mut seen = vec![false; MAX_TOURNAMENT_RECORDS];
        let mut unknown_corrupt_records = 0_usize;
        for leaf in leaves {
            if leaf.as_c_str() == STORE_MANIFEST_LEAF {
                continue;
            }
            let Some(slot) = parse_slot_leaf(&leaf) else {
                unknown_corrupt_records = unknown_corrupt_records.saturating_add(1);
                continue;
            };
            if seen[slot as usize] {
                slots[slot as usize] = SlotState::Corrupt;
                continue;
            }
            seen[slot as usize] = true;
            slots[slot as usize] = match read_json::<TournamentSlotRecord>(
                &directory,
                &leaf,
                MAX_SPECULATION_JSON_BYTES,
            ) {
                Ok(entry) if entry.value.validate(slot).is_ok() => match entry.value.record {
                    Some(record) => SlotState::Valid {
                        record,
                        identity: entry.identity,
                    },
                    None => SlotState::Vacant {
                        identity: Some(entry.identity),
                    },
                },
                _ => SlotState::Corrupt,
            };
        }
        Ok(Self {
            directory,
            state: Mutex::new(StoreState {
                slots,
                unknown_corrupt_records,
            }),
        })
    }

    pub fn allocate_prepared(
        &self,
        record: TournamentRecord,
        now_unix_ms: u64,
    ) -> EvidenceResult<TournamentKey> {
        record.validate()?;
        if record.status.phase != SpeculationPhase::Prepared
            || record.terminal_completed_unix_ms.is_some()
        {
            return Err(EvidenceError::Corrupt);
        }
        let mut state = self.state.lock().map_err(|_| EvidenceError::Poisoned)?;
        if state.slots.iter().any(|slot| {
            matches!(slot, SlotState::Valid { record: current, .. } if current.status.tournament_uuid == record.status.tournament_uuid)
        }) {
            return Err(EvidenceError::AlreadyExists);
        }
        if occupied_live_count(&state) >= MAX_LIVE_TOURNAMENTS {
            return Err(EvidenceError::Capacity);
        }
        let slot = select_allocation_slot(&state, now_unix_ms).ok_or(EvidenceError::Capacity)?;
        let leaf = slot_leaf(slot)?;
        let envelope = TournamentSlotRecord::occupied(slot, record.clone());
        let entry = match &state.slots[slot as usize] {
            SlotState::Vacant {
                identity: Some(identity),
            } => atomic_replace_json(
                &self.directory,
                &leaf,
                *identity,
                &envelope,
                MAX_SPECULATION_JSON_BYTES,
            ),
            SlotState::Vacant { identity: None } => atomic_create_json(
                &self.directory,
                &leaf,
                &envelope,
                MAX_SPECULATION_JSON_BYTES,
            ),
            SlotState::Valid { identity, .. } => atomic_replace_json(
                &self.directory,
                &leaf,
                *identity,
                &envelope,
                MAX_SPECULATION_JSON_BYTES,
            ),
            SlotState::Corrupt => return Err(EvidenceError::Capacity),
        };
        let newly_materialized = matches!(
            &state.slots[slot as usize],
            SlotState::Vacant { identity: None }
        );
        let entry = match entry {
            Ok(entry) if entry.value.validate(slot).is_ok() => entry,
            Ok(_) => {
                state.slots[slot as usize] = SlotState::Corrupt;
                return Err(EvidenceError::Corrupt);
            }
            Err(error) => {
                state.slots[slot as usize] = SlotState::Corrupt;
                return Err(error);
            }
        };
        let Some(stored_record) = entry.value.record else {
            state.slots[slot as usize] = SlotState::Corrupt;
            return Err(EvidenceError::Corrupt);
        };
        if newly_materialized {
            let mut present = [0_u64; STORE_MATERIALIZATION_WORDS];
            mark_materialized_word(&mut present, slot);
            if let Err(error) = persist_materialized_slots(&self.directory, &present) {
                state.slots[slot as usize] = SlotState::Corrupt;
                return Err(error);
            }
        }
        let key = key_for(slot, &record);
        state.slots[slot as usize] = SlotState::Valid {
            record: stored_record,
            identity: entry.identity,
        };
        Ok(key)
    }

    pub fn compare_and_swap(
        &self,
        key: &TournamentKey,
        expected_generation: u64,
        next: TournamentRecord,
    ) -> EvidenceResult<TournamentKey> {
        self.write(
            key,
            expected_generation,
            next,
            TournamentWriteKind::LivePhaseTransition,
        )
        .map(|stored| stored.key)
    }

    pub fn write(
        &self,
        key: &TournamentKey,
        expected_generation: u64,
        next: TournamentRecord,
        kind: TournamentWriteKind,
    ) -> EvidenceResult<StoredTournamentUpdate> {
        next.validate()?;
        let mut state = self.state.lock().map_err(|_| EvidenceError::Poisoned)?;
        let slot_state = state
            .slots
            .get_mut(key.slot as usize)
            .ok_or(EvidenceError::GenerationMismatch)?;
        let SlotState::Valid {
            record: current,
            identity,
        } = slot_state
        else {
            return Err(EvidenceError::GenerationMismatch);
        };
        if key.tournament_uuid != current.status.tournament_uuid
            || key.generation != expected_generation
            || current.status.generation != expected_generation
            || next.status.generation
                != expected_generation
                    .checked_add(1)
                    .ok_or(EvidenceError::GenerationMismatch)?
            || !current.immutable_identity_matches(&next)
            || !match kind {
                TournamentWriteKind::LivePhaseTransition => {
                    current.restart_prior_phase == next.restart_prior_phase
                        && crate::speculation::is_legal_transition(
                            current.status.phase,
                            next.status.phase,
                        )
                }
                TournamentWriteKind::SamePhaseEvidence => {
                    current.status.phase == next.status.phase
                        && current.restart_prior_phase == next.restart_prior_phase
                }
                TournamentWriteKind::OldBootAbsence { current_boot_uuid } => {
                    !current_boot_uuid.is_nil()
                        && current.boot_uuid != current_boot_uuid
                        && current.status.phase == SpeculationPhase::RollbackPending
                        && next.status.phase == SpeculationPhase::RollbackPending
                        && current.restart_prior_phase == next.restart_prior_phase
                        && current.managed_owners == next.managed_owners
                        && next
                            .cgroups
                            .iter()
                            .all(|candidate| candidate.lifecycle == CgroupLifecycleState::Removed)
                        && next.tournament_cgroup.lifecycle
                            == TournamentCgroupLifecycleState::Removed
                        && next.status.candidates.iter().all(|candidate| {
                            candidate.cleanup.runner_ack
                                && candidate.cleanup.bwrap_reaped
                                && candidate.cleanup.sync_eof
                                && candidate.cleanup.cgroup_empty
                                && candidate.cleanup.managed_tombstone
                        })
                }
            }
            || !current.public_evidence_progresses_to(
                &next,
                matches!(kind, TournamentWriteKind::SamePhaseEvidence),
                matches!(kind, TournamentWriteKind::LivePhaseTransition)
                    && next.status.phase.is_rollback_only(),
            )
            || !(matches!(kind, TournamentWriteKind::OldBootAbsence { .. })
                || current
                    .tournament_cgroup
                    .lifecycle
                    .is_legal_transition(next.tournament_cgroup.lifecycle))
            || current
                .cgroups
                .iter()
                .zip(next.cgroups.iter())
                .any(|(current, next)| {
                    !matches!(kind, TournamentWriteKind::OldBootAbsence { .. })
                        && !current.lifecycle.is_legal_transition(next.lifecycle)
                })
        {
            return Err(EvidenceError::GenerationMismatch);
        }
        let envelope = TournamentSlotRecord::occupied(key.slot, next);
        let entry = atomic_replace_json(
            &self.directory,
            &slot_leaf(key.slot)?,
            *identity,
            &envelope,
            MAX_SPECULATION_JSON_BYTES,
        );
        let entry = match entry {
            Ok(entry) if entry.value.validate(key.slot).is_ok() => entry,
            Ok(_) => {
                *slot_state = SlotState::Corrupt;
                return Err(EvidenceError::Corrupt);
            }
            Err(error) => {
                *slot_state = SlotState::Corrupt;
                return Err(error);
            }
        };
        let Some(stored_record) = entry.value.record else {
            *slot_state = SlotState::Corrupt;
            return Err(EvidenceError::Corrupt);
        };
        let next_key = key_for(key.slot, &stored_record);
        let stored_update = StoredTournamentUpdate {
            key: next_key,
            record: (*stored_record).clone(),
        };
        *slot_state = SlotState::Valid {
            record: stored_record,
            identity: entry.identity,
        };
        Ok(stored_update)
    }

    pub fn normalize_after_daemon_restart(
        &self,
        key: &TournamentKey,
        expected_generation: u64,
        current_daemon_instance_uuid: Uuid,
    ) -> EvidenceResult<StoredTournamentUpdate> {
        if current_daemon_instance_uuid.is_nil() {
            return Err(EvidenceError::Corrupt);
        }
        let current = self
            .load_by_uuid(key.tournament_uuid)?
            .ok_or(EvidenceError::GenerationMismatch)?;
        if key.generation != expected_generation
            || current.status.generation != expected_generation
            || current.status.daemon_instance_uuid == current_daemon_instance_uuid
        {
            return Err(EvidenceError::GenerationMismatch);
        }
        if current.status.is_terminal()
            || current.status.phase == SpeculationPhase::RollbackRequired
        {
            return Ok(StoredTournamentUpdate {
                key: key_for(key.slot, &current),
                record: current,
            });
        }
        if current.restart_prior_phase.is_some() {
            return Err(EvidenceError::Corrupt);
        }
        let mut next = current.clone();
        next.status.generation = expected_generation
            .checked_add(1)
            .ok_or(EvidenceError::GenerationMismatch)?;
        next.restart_prior_phase = Some(current.status.phase);
        next.status.phase = SpeculationPhase::RollbackRequired;
        next.status.rollback_required = true;
        next.status.selected_index = None;
        next.status.reason_code = Some(
            if current.status.phase == SpeculationPhase::DecisionUncertain {
                SpeculationReasonCode::DecisionUncertainAfterRestart
            } else {
                SpeculationReasonCode::ContainmentEvidenceUnavailable
            },
        );
        self.write_restart_normalization(key, expected_generation, current, next)
    }

    fn write_restart_normalization(
        &self,
        key: &TournamentKey,
        expected_generation: u64,
        current_snapshot: TournamentRecord,
        next: TournamentRecord,
    ) -> EvidenceResult<StoredTournamentUpdate> {
        next.validate()?;
        let mut state = self.state.lock().map_err(|_| EvidenceError::Poisoned)?;
        let slot_state = state
            .slots
            .get_mut(key.slot as usize)
            .ok_or(EvidenceError::GenerationMismatch)?;
        let SlotState::Valid {
            record: current,
            identity,
        } = slot_state
        else {
            return Err(EvidenceError::GenerationMismatch);
        };
        if current.as_ref() != &current_snapshot
            || key.tournament_uuid != current.status.tournament_uuid
            || key.generation != expected_generation
            || current.status.generation != expected_generation
            || next.status.generation
                != expected_generation
                    .checked_add(1)
                    .ok_or(EvidenceError::GenerationMismatch)?
            || !current.immutable_identity_matches(&next)
            || next.restart_prior_phase != Some(current.status.phase)
            || next.status.phase != SpeculationPhase::RollbackRequired
            || !next.status.rollback_required
            || next.status.selected_index.is_some()
            || current.terminal_completed_unix_ms.is_some()
        {
            return Err(EvidenceError::GenerationMismatch);
        }
        let entry = atomic_replace_json(
            &self.directory,
            &slot_leaf(key.slot)?,
            *identity,
            &TournamentSlotRecord::occupied(key.slot, next),
            MAX_SPECULATION_JSON_BYTES,
        );
        let entry = match entry {
            Ok(entry) if entry.value.validate(key.slot).is_ok() => entry,
            Ok(_) => {
                *slot_state = SlotState::Corrupt;
                return Err(EvidenceError::Corrupt);
            }
            Err(error) => {
                *slot_state = SlotState::Corrupt;
                return Err(error);
            }
        };
        let Some(stored_record) = entry.value.record else {
            *slot_state = SlotState::Corrupt;
            return Err(EvidenceError::Corrupt);
        };
        let stored = StoredTournamentUpdate {
            key: key_for(key.slot, &stored_record),
            record: (*stored_record).clone(),
        };
        *slot_state = SlotState::Valid {
            record: stored_record,
            identity: entry.identity,
        };
        Ok(stored)
    }

    pub fn load_by_uuid(&self, id: Uuid) -> EvidenceResult<Option<TournamentRecord>> {
        let mut state = self.state.lock().map_err(|_| EvidenceError::Poisoned)?;
        for (slot, slot_state) in state.slots.iter_mut().enumerate() {
            let SlotState::Valid { record, identity } = slot_state else {
                continue;
            };
            if record.status.tournament_uuid != id {
                continue;
            }
            let entry = read_json::<TournamentSlotRecord>(
                &self.directory,
                &slot_leaf(slot as u16)?,
                MAX_SPECULATION_JSON_BYTES,
            );
            return match entry {
                Ok(entry)
                    if entry.identity == *identity
                        && entry.value.validate(slot as u16).is_ok()
                        && entry.value.record.as_deref() == Some(record.as_ref()) =>
                {
                    Ok(entry.value.record.map(|record| *record))
                }
                _ => {
                    *slot_state = SlotState::Corrupt;
                    Err(EvidenceError::Corrupt)
                }
            };
        }
        Ok(None)
    }

    pub fn scan_recovery(&self) -> EvidenceResult<Vec<TournamentRecoveryRecord>> {
        let state = self.state.lock().map_err(|_| EvidenceError::Poisoned)?;
        let mut recovery = Vec::new();
        for (slot, state) in state.slots.iter().enumerate() {
            match state {
                SlotState::Vacant { .. } => {}
                SlotState::Valid { record, .. } => recovery.push(TournamentRecoveryRecord::Valid {
                    key: key_for(slot as u16, record),
                    record: record.clone(),
                }),
                SlotState::Corrupt => recovery.push(TournamentRecoveryRecord::Corrupt {
                    slot: Some(slot as u16),
                }),
            }
        }
        recovery.extend(
            (0..state.unknown_corrupt_records)
                .map(|_| TournamentRecoveryRecord::Corrupt { slot: None }),
        );
        Ok(recovery)
    }

    #[cfg(test)]
    pub(crate) fn poison_for_test(&self) {
        let _ = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _guard = self.state.lock().expect("unpoisoned test store");
                    panic!("poison tournament store for fail-closed coverage");
                })
                .join()
        });
    }
}

fn mark_materialized_word(words: &mut [u64; STORE_MATERIALIZATION_WORDS], slot: u16) {
    let slot = slot as usize;
    words[slot / u64::BITS as usize] |= 1_u64 << (slot % u64::BITS as usize);
}

fn persist_materialized_slots(
    directory: &ValidatedDirectory,
    present: &[u64; STORE_MATERIALIZATION_WORDS],
) -> EvidenceResult<TournamentStoreManifest> {
    for _ in 0..64 {
        let current = read_json::<TournamentStoreManifest>(
            directory,
            STORE_MANIFEST_LEAF,
            MAX_SPECULATION_JSON_BYTES,
        )?;
        current.value.validate()?;
        let mut next = current.value;
        if !next.merge_materialized(present) {
            return Ok(next);
        }
        match atomic_replace_json(
            directory,
            STORE_MANIFEST_LEAF,
            current.identity,
            &next,
            MAX_SPECULATION_JSON_BYTES,
        ) {
            Ok(stored) => return Ok(stored.value),
            Err(EvidenceError::Stale) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(EvidenceError::Stale)
}

fn key_for(slot: u16, record: &TournamentRecord) -> TournamentKey {
    TournamentKey {
        slot,
        generation: record.status.generation,
        tournament_uuid: record.status.tournament_uuid,
    }
}

fn occupied_live_count(state: &StoreState) -> usize {
    state.unknown_corrupt_records.saturating_add(
        state
            .slots
            .iter()
            .filter(|slot| match slot {
                SlotState::Vacant { .. } => false,
                SlotState::Corrupt => true,
                SlotState::Valid { record, .. } => !record.is_positive_terminal(),
            })
            .count(),
    )
}

fn select_allocation_slot(state: &StoreState, now_unix_ms: u64) -> Option<u16> {
    state
        .slots
        .iter()
        .position(|slot| matches!(slot, SlotState::Vacant { .. }))
        .or_else(|| {
            state.slots.iter().position(|slot| {
                let SlotState::Valid { record, .. } = slot else {
                    return false;
                };
                record.is_positive_terminal()
                    && record.terminal_completed_unix_ms.is_some_and(|completed| {
                        completed.saturating_add(TERMINAL_RETENTION_MILLIS) <= now_unix_ms
                    })
            })
        })
        .and_then(|slot| u16::try_from(slot).ok())
}

fn slot_leaf(slot: u16) -> EvidenceResult<CString> {
    if slot as usize >= MAX_TOURNAMENT_RECORDS {
        return Err(EvidenceError::InvalidLeaf);
    }
    CString::new(format!("slot-{slot:04}.json")).map_err(|_| EvidenceError::InvalidLeaf)
}

fn parse_slot_leaf(leaf: &CStr) -> Option<u16> {
    let value = std::str::from_utf8(leaf.to_bytes()).ok()?;
    let digits = value.strip_prefix("slot-")?.strip_suffix(".json")?;
    if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let slot = digits.parse::<u16>().ok()?;
    ((slot as usize) < MAX_TOURNAMENT_RECORDS).then_some(slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal_record() -> TournamentRecord {
        let identity = DurableDirectoryIdentity::test_value();
        let candidate_uuids = [Uuid::from_u128(2), Uuid::from_u128(3)];
        let cleanup = crate::protocol::SpeculationCleanupStatus {
            runner_ack: true,
            bwrap_reaped: true,
            sync_eof: true,
            cgroup_empty: true,
            managed_tombstone: true,
        };
        let candidate = |index: u8, success: bool, elapsed_ns: u64| SpeculationCandidateStatus {
            candidate_uuid: candidate_uuids[index as usize],
            index,
            ready: true,
            ready_elapsed_ns: Some(1),
            go_received: true,
            go_received_elapsed_ns: Some(2),
            result_accepted: true,
            exit_success: Some(success),
            exit_category: Some(if success {
                SpeculationExitCategory::ExitedZero
            } else {
                SpeculationExitCategory::ExitedNonzero
            }),
            elapsed_ns: Some(elapsed_ns),
            output_bytes: Some(8),
            eligible: true,
            cleanup,
        };
        TournamentRecord {
            schema_version: TournamentRecordSchema::V1,
            boot_uuid: identity.boot_uuid,
            roots: PrivateRootIdentities {
                source: identity,
                candidates: [identity, identity],
                ledger_root: identity,
                cgroup_root: identity,
            },
            cgroup_root_locator: PrivateCgroupRootLocator {
                canonical_path: SpeculationUnixPath::from_path(Path::new("/cgroup")).unwrap(),
                identity,
            },
            tournament_cgroup: TournamentCgroupEvidence {
                deterministic_name_uuid: Uuid::from_u128(4),
                lifecycle: TournamentCgroupLifecycleState::Removed,
                domain: None,
            },
            cgroups: std::array::from_fn(|index| CandidateCgroupEvidence {
                candidate_index: index as u8,
                deterministic_name_uuid: candidate_uuids[index],
                lifecycle: CgroupLifecycleState::Removed,
                parent: None,
                control: None,
                payload: None,
            }),
            managed_owners: [None, None],
            restart_prior_phase: None,
            terminal_completed_unix_ms: Some(100),
            status: SpeculationStatus {
                schema_version: SpeculationSchemaVersion::V1,
                tournament_uuid: Uuid::from_u128(4),
                daemon_instance_uuid: Uuid::from_u128(5),
                phase: SpeculationPhase::Selected,
                generation: 9,
                lease_deadline_unix_ms: 42,
                reason_code: None,
                candidates: [candidate(0, true, 10), candidate(1, false, 9)],
                fixed_score_order: SPECULATION_SCORE_ORDER,
                selected_index: Some(0),
                rollback_required: false,
                error_codes: Default::default(),
            },
        }
    }

    fn prepared_record() -> TournamentRecord {
        let mut record = terminal_record();
        record.terminal_completed_unix_ms = None;
        record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Planned;
        for (index, cgroup) in record.cgroups.iter_mut().enumerate() {
            cgroup.lifecycle = CgroupLifecycleState::Forward(CgroupForwardState::Planned);
            cgroup.parent = None;
            cgroup.control = None;
            cgroup.payload = None;
            record.status.candidates[index] = SpeculationCandidateStatus {
                candidate_uuid: cgroup.deterministic_name_uuid,
                index: index as u8,
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
        }
        record.status.phase = SpeculationPhase::Prepared;
        record.status.selected_index = None;
        record.status.rollback_required = false;
        record
    }

    #[test]
    fn registry_caps_retention_and_slot_names_are_fixed() {
        assert_eq!(MAX_TOURNAMENT_RECORDS, 1024);
        assert_eq!(MAX_LIVE_TOURNAMENTS, 8);
        assert_eq!(TERMINAL_RETENTION_MILLIS, 7 * 24 * 60 * 60 * 1000);
        assert_eq!(parse_slot_leaf(&slot_leaf(1023).unwrap()), Some(1023));
        assert!(slot_leaf(1024).is_err());
    }

    fn armed_record_with_reused_managed_slot() -> TournamentRecord {
        let mut armed = prepared_record();
        armed.status.phase = SpeculationPhase::Armed;
        armed.status.generation += 1;
        armed.managed_owners[0] = Some(ManagedOwnerEvidence {
            candidate_index: 0,
            role: ManagedOwnerRoleEvidence::Runner,
            slot: 7,
            generation: armed.status.generation + 1,
        });
        armed
    }

    #[test]
    fn managed_slot_generation_is_independent_from_tournament_generation() {
        let armed = armed_record_with_reused_managed_slot();
        assert!(
            armed.managed_owners[0]
                .as_ref()
                .is_some_and(|owner| owner.generation > armed.status.generation)
        );
        armed.validate().unwrap();
    }

    #[test]
    fn same_phase_progress_rejects_public_and_owner_regression() {
        let current = armed_record_with_reused_managed_slot();
        let mut next = current.clone();
        next.status.generation += 1;
        assert!(current.public_evidence_progresses_to(&next, true, false));

        next.managed_owners[0] = None;
        assert!(!current.public_evidence_progresses_to(&next, true, false));

        let mut probe = current.clone();
        probe.managed_owners[0].as_mut().unwrap().role = ManagedOwnerRoleEvidence::Probe;
        let mut runner = probe.clone();
        runner.managed_owners[0] = current.managed_owners[0].clone();
        assert!(probe.public_evidence_progresses_to(&runner, true, false));
        assert!(!runner.public_evidence_progresses_to(&probe, true, false));

        let mut ready = current.clone();
        ready.status.candidates[0].ready = true;
        ready.status.candidates[0].ready_elapsed_ns = Some(1);
        assert!(current.public_evidence_progresses_to(&ready, true, false));
        assert!(!ready.public_evidence_progresses_to(&current, true, false));
    }

    #[test]
    fn rollback_transition_may_clear_a_scored_selection_but_same_phase_may_not() {
        let mut current = terminal_record();
        current.terminal_completed_unix_ms = None;
        current.status.phase = SpeculationPhase::PendingFinalize;
        let mut rollback = current.clone();
        rollback.status.generation += 1;
        rollback.status.phase = SpeculationPhase::RollbackPending;
        rollback.status.selected_index = None;
        rollback.status.rollback_required = true;
        assert!(!current.public_evidence_progresses_to(&rollback, false, false));
        assert!(current.public_evidence_progresses_to(&rollback, false, true));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn store_write_modes_and_restart_normalization_are_closed() {
        let root = tempfile::tempdir().unwrap();
        let store = TournamentStore::open_or_create(&root.path().join("store")).unwrap();
        let prepared = prepared_record();
        let key = store.allocate_prepared(prepared.clone(), 1).unwrap();

        let mut same_phase = prepared.clone();
        same_phase.status.generation += 1;
        let stored = store
            .write(
                &key,
                key.generation,
                same_phase.clone(),
                TournamentWriteKind::SamePhaseEvidence,
            )
            .unwrap();
        assert_eq!(stored.record, same_phase);

        let mut illegal_live = stored.record.clone();
        illegal_live.status.generation += 1;
        assert_eq!(
            store.write(
                &stored.key,
                stored.key.generation,
                illegal_live,
                TournamentWriteKind::LivePhaseTransition,
            ),
            Err(EvidenceError::GenerationMismatch)
        );

        let current_daemon = Uuid::from_u128(99);
        let normalized = store
            .normalize_after_daemon_restart(&stored.key, stored.key.generation, current_daemon)
            .unwrap();
        assert_eq!(
            normalized.record.status.phase,
            SpeculationPhase::RollbackRequired
        );
        assert_eq!(
            normalized.record.restart_prior_phase,
            Some(SpeculationPhase::Prepared)
        );
        assert_eq!(
            normalized.record.status.reason_code,
            Some(SpeculationReasonCode::ContainmentEvidenceUnavailable)
        );
        assert_eq!(
            store.load_by_uuid(key.tournament_uuid).unwrap(),
            Some(normalized.record)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reused_managed_slot_receipt_survives_cas_reload_and_recovery_scan() {
        let root = tempfile::tempdir().unwrap();
        let store = TournamentStore::open_or_create(&root.path().join("store")).unwrap();
        let record = prepared_record();
        let key = store.allocate_prepared(record, 1).unwrap();
        let armed = armed_record_with_reused_managed_slot();

        let key = store
            .compare_and_swap(&key, key.generation, armed.clone())
            .unwrap();
        assert_eq!(
            store.load_by_uuid(key.tournament_uuid).unwrap(),
            Some(armed.clone())
        );
        assert!(store.scan_recovery().unwrap().into_iter().any(|entry| {
            matches!(
                entry,
                TournamentRecoveryRecord::Valid { record, .. }
                    if *record == armed
            )
        }));
    }

    #[test]
    fn corrupt_and_unresolved_slots_consume_live_capacity_and_never_reclaim() {
        let mut slots = (0..MAX_TOURNAMENT_RECORDS)
            .map(|_| SlotState::Vacant {
                identity: Some(FileIdentity { dev: 1, ino: 1 }),
            })
            .collect::<Vec<_>>();
        for slot in slots.iter_mut().take(MAX_LIVE_TOURNAMENTS - 1) {
            *slot = SlotState::Corrupt;
        }
        let state = StoreState {
            slots,
            unknown_corrupt_records: 1,
        };
        assert_eq!(occupied_live_count(&state), MAX_LIVE_TOURNAMENTS);
        assert!(select_allocation_slot(&state, u64::MAX).is_some());
    }

    #[test]
    fn malformed_terminal_evidence_is_never_a_reclaimable_tombstone() {
        let valid = terminal_record();
        valid.validate().unwrap();
        assert!(valid.is_positive_terminal());

        let mut malformed = Vec::new();
        let mut missing_selection = valid.clone();
        missing_selection.status.selected_index = None;
        malformed.push(missing_selection);
        let mut wrong_selection = valid.clone();
        wrong_selection.status.selected_index = Some(1);
        malformed.push(wrong_selection);
        let mut wrong_score_order = valid.clone();
        wrong_score_order.status.fixed_score_order.swap(0, 1);
        malformed.push(wrong_score_order);
        let mut duplicate_score_order = valid.clone();
        duplicate_score_order.status.fixed_score_order[0] =
            duplicate_score_order.status.fixed_score_order[1];
        malformed.push(duplicate_score_order);
        let mut partial_result = valid.clone();
        partial_result.status.candidates[0].result_accepted = false;
        malformed.push(partial_result);
        let mut accepted_without_go = valid.clone();
        let selected = &mut accepted_without_go.status.candidates[0];
        selected.go_received = false;
        selected.go_received_elapsed_ns = None;
        malformed.push(accepted_without_go);
        let mut missing_loser_result = valid.clone();
        let loser = &mut missing_loser_result.status.candidates[1];
        loser.result_accepted = false;
        loser.exit_success = None;
        loser.exit_category = None;
        loser.elapsed_ns = None;
        loser.output_bytes = None;
        loser.eligible = false;
        malformed.push(missing_loser_result);
        let mut wrong_candidate_uuid = valid.clone();
        wrong_candidate_uuid.cgroups[0].deterministic_name_uuid = Uuid::from_u128(99);
        malformed.push(wrong_candidate_uuid);
        let mut zero_root = valid;
        zero_root.roots.source.dev = 0;
        malformed.push(zero_root);

        for record in malformed {
            assert_eq!(record.validate(), Err(EvidenceError::Corrupt));
            assert!(!record.is_positive_terminal());
            let state = StoreState {
                slots: std::iter::once(SlotState::Valid {
                    record: Box::new(record),
                    identity: FileIdentity { dev: 1, ino: 1 },
                })
                .chain((1..MAX_TOURNAMENT_RECORDS).map(|_| SlotState::Corrupt))
                .collect(),
                unknown_corrupt_records: 0,
            };
            assert_eq!(occupied_live_count(&state), MAX_TOURNAMENT_RECORDS);
            assert_eq!(select_allocation_slot(&state, u64::MAX), None);
        }
    }

    #[test]
    fn fixed_store_manifest_commits_exactly_1024_virtual_slots() {
        let manifest = TournamentStoreManifest::fixed();
        manifest.validate().unwrap();
        assert_eq!(manifest.slot_count as usize, MAX_TOURNAMENT_RECORDS);
        assert!(manifest.materialized_slots.iter().all(|word| *word == 0));
        let slots = (0..manifest.slot_count)
            .map(|_| SlotState::Vacant { identity: None })
            .collect::<Vec<_>>();
        assert_eq!(slots.len(), 1024);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn manifest_genesis_is_restart_atomic_and_partial_pre_genesis_fails_closed() {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("store");
        let store = TournamentStore::open_or_create(&path).unwrap();
        {
            let state = store.state.lock().unwrap();
            assert_eq!(state.slots.len(), MAX_TOURNAMENT_RECORDS);
            assert!(
                state
                    .slots
                    .iter()
                    .all(|slot| matches!(slot, SlotState::Vacant { identity: None }))
            );
        }
        drop(store);
        let reopened = TournamentStore::open_or_create(&path).unwrap();
        assert_eq!(reopened.state.lock().unwrap().slots.len(), 1024);
        drop(reopened);

        let directory = crate::speculation_fs::open_existing_private_dir(&path).unwrap();
        atomic_create_json(
            &directory,
            &slot_leaf(7).unwrap(),
            &TournamentSlotRecord::vacant(7),
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        drop(directory);
        let repaired = TournamentStore::open_or_create(&path).unwrap();
        assert!(matches!(
            repaired.state.lock().unwrap().slots[7],
            SlotState::Vacant { identity: Some(_) }
        ));
        drop(repaired);
        let directory = crate::speculation_fs::open_existing_private_dir(&path).unwrap();
        let manifest = read_json::<TournamentStoreManifest>(
            &directory,
            STORE_MANIFEST_LEAF,
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        assert!(manifest.value.is_materialized(7));
        std::fs::remove_file(path.join("slot-0007.json")).unwrap();
        drop(directory);
        let missing = TournamentStore::open_or_create(&path).unwrap();
        assert!(matches!(
            missing.state.lock().unwrap().slots[7],
            SlotState::Corrupt
        ));

        let partial = root.path().join("partial");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&partial)
            .unwrap();
        let bytes = serde_json::to_vec(&TournamentSlotRecord::vacant(0)).unwrap();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        std::io::Write::write_all(
            &mut options.open(partial.join("slot-0000.json")).unwrap(),
            &bytes,
        )
        .unwrap();
        assert!(matches!(
            TournamentStore::open_or_create(&partial),
            Err(EvidenceError::Corrupt)
        ));
    }

    #[test]
    fn cgroup_lifecycle_preserves_cleanup_origin_and_absence_rules() {
        use AbsenceDisposition as A;
        use CgroupComponent as C;
        use CgroupForwardState as F;
        use CgroupLifecycleState as L;

        assert!(
            L::Forward(F::PayloadCreated).is_legal_transition(L::CleanupPending {
                from: F::PayloadCreated,
            })
        );
        assert!(
            !L::Forward(F::PayloadCreated).is_legal_transition(L::CleanupPending {
                from: F::ControlCreated,
            })
        );
        assert_eq!(
            L::Forward(F::ParentCreatePending).same_boot_absence(C::Parent),
            A::RetryCreate
        );
        assert_eq!(
            L::CleanupPending { from: F::Planned }.same_boot_absence(C::Parent),
            A::RequiredNeverCreated
        );
        assert_eq!(
            L::PayloadRemovePending {
                from: F::PayloadAttached,
            }
            .same_boot_absence(C::Payload),
            A::AcceptRemoval
        );
        assert_eq!(
            L::Forward(F::PayloadAttached).same_boot_absence(C::Payload),
            A::Forbidden
        );
    }

    #[test]
    fn shared_tournament_domain_has_independent_durable_lifecycle() {
        use TournamentCgroupLifecycleState as S;
        assert!(S::Planned.is_legal_transition(S::CreatePending));
        assert!(S::CreatePending.is_legal_transition(S::Created));
        assert!(S::Created.is_legal_transition(S::RemovePending));
        assert!(S::RemovePending.is_legal_transition(S::Removed));
        assert!(!S::Planned.is_legal_transition(S::Created));

        let identity = DurableDirectoryIdentity::test_value();
        let mut evidence = TournamentCgroupEvidence {
            deterministic_name_uuid: Uuid::from_u128(4),
            lifecycle: S::Created,
            domain: Some(identity),
        };
        assert!(
            evidence
                .validate(Uuid::from_u128(4), identity.boot_uuid)
                .is_ok()
        );

        evidence.deterministic_name_uuid = Uuid::from_u128(99);
        assert_eq!(
            evidence.validate(Uuid::from_u128(4), identity.boot_uuid),
            Err(EvidenceError::Corrupt)
        );
    }
}

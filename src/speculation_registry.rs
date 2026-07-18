use crate::protocol::{SpeculationPhase, SpeculationStatus, SpeculationUnixPath};
use crate::speculation_fs::{
    DirectoryIdentity, EvidenceError, EvidenceResult, FileIdentity, MAX_SPECULATION_JSON_BYTES,
    ValidatedDirectory, atomic_create_json, atomic_replace_json, open_or_create_private_dir,
    read_json,
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
    pub source: DirectoryIdentity,
    pub candidates: [DirectoryIdentity; 2],
    pub ledger_root: DirectoryIdentity,
    pub cgroup_root: DirectoryIdentity,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateCgroupRootLocator {
    canonical_path: SpeculationUnixPath,
    pub identity: DirectoryIdentity,
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
        let directory =
            crate::speculation_fs::open_existing_private_dir(&self.canonical_path.to_path_buf())?;
        if directory.identity() != self.identity {
            return Err(EvidenceError::Stale);
        }
        Ok(directory)
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
    pub parent: Option<DirectoryIdentity>,
    pub control: Option<DirectoryIdentity>,
    pub payload: Option<DirectoryIdentity>,
}

impl CandidateCgroupEvidence {
    fn validate(&self, expected_index: u8, boot_uuid: Uuid) -> EvidenceResult<()> {
        if self.candidate_index != expected_index
            || self
                .parent
                .iter()
                .chain(self.control.iter())
                .chain(self.payload.iter())
                .any(|identity| identity.boot_uuid != boot_uuid)
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
        if !matches!(
            self.lifecycle,
            CgroupLifecycleState::Removed | CgroupLifecycleState::RollbackRequired
        ) && present_floor != creation_floor
        {
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
    pub cgroups: [CandidateCgroupEvidence; 2],
    pub managed_owners: [Option<ManagedOwnerEvidence>; 2],
    pub terminal_completed_unix_ms: Option<u64>,
    pub status: SpeculationStatus,
}

impl TournamentRecord {
    pub fn validate(&self) -> EvidenceResult<()> {
        if self.schema_version != TournamentRecordSchema::V1
            || self.boot_uuid != self.roots.source.boot_uuid
            || self.boot_uuid != self.roots.ledger_root.boot_uuid
            || self.boot_uuid != self.roots.cgroup_root.boot_uuid
            || self
                .roots
                .candidates
                .iter()
                .any(|identity| identity.boot_uuid != self.boot_uuid)
            || self.cgroup_root_locator.identity != self.roots.cgroup_root
            || self.cgroups[0].validate(0, self.boot_uuid).is_err()
            || self.cgroups[1].validate(1, self.boot_uuid).is_err()
            || self.status.candidates[0].index != 0
            || self.status.candidates[1].index != 1
            || self
                .managed_owners
                .iter()
                .enumerate()
                .any(|(index, owner)| {
                    owner
                        .as_ref()
                        .is_some_and(|owner| owner.candidate_index != index as u8)
                })
        {
            return Err(EvidenceError::Corrupt);
        }
        if self.terminal_completed_unix_ms.is_some() != self.is_positive_terminal() {
            return Err(EvidenceError::Corrupt);
        }
        Ok(())
    }

    pub fn is_positive_terminal(&self) -> bool {
        self.status.is_terminal()
            && !self.status.rollback_required
            && self
                .cgroups
                .iter()
                .all(|candidate| candidate.lifecycle == CgroupLifecycleState::Removed)
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
            && self.status.tournament_uuid == next.status.tournament_uuid
            && self.status.daemon_instance_uuid == next.status.daemon_instance_uuid
            && self.status.candidates[0].candidate_uuid == next.status.candidates[0].candidate_uuid
            && self.status.candidates[1].candidate_uuid == next.status.candidates[1].candidate_uuid
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TournamentKey {
    slot: u16,
    generation: u64,
    tournament_uuid: Uuid,
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
        identity: FileIdentity,
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
        if leaves.is_empty() {
            initialize_vacant_slots(&directory)?;
            leaves = directory.list_leaf_names()?;
        }
        let mut slots = (0..MAX_TOURNAMENT_RECORDS)
            .map(|_| SlotState::Corrupt)
            .collect::<Vec<_>>();
        let mut seen = vec![false; MAX_TOURNAMENT_RECORDS];
        let mut unknown_corrupt_records = 0_usize;
        for leaf in leaves {
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
                        identity: entry.identity,
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
            SlotState::Vacant { identity } => atomic_replace_json(
                &self.directory,
                &leaf,
                *identity,
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
            || !crate::speculation::is_legal_transition(current.status.phase, next.status.phase)
            || current
                .cgroups
                .iter()
                .zip(next.cgroups.iter())
                .any(|(current, next)| !current.lifecycle.is_legal_transition(next.lifecycle))
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
        *slot_state = SlotState::Valid {
            record: stored_record,
            identity: entry.identity,
        };
        Ok(next_key)
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

fn initialize_vacant_slots(directory: &ValidatedDirectory) -> EvidenceResult<()> {
    for slot in 0..MAX_TOURNAMENT_RECORDS {
        let slot = u16::try_from(slot).map_err(|_| EvidenceError::Capacity)?;
        atomic_create_json(
            directory,
            &slot_leaf(slot)?,
            &TournamentSlotRecord::vacant(slot),
            MAX_SPECULATION_JSON_BYTES,
        )?;
    }
    Ok(())
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

    #[test]
    fn registry_caps_retention_and_slot_names_are_fixed() {
        assert_eq!(MAX_TOURNAMENT_RECORDS, 1024);
        assert_eq!(MAX_LIVE_TOURNAMENTS, 8);
        assert_eq!(TERMINAL_RETENTION_MILLIS, 7 * 24 * 60 * 60 * 1000);
        assert_eq!(parse_slot_leaf(&slot_leaf(1023).unwrap()), Some(1023));
        assert!(slot_leaf(1024).is_err());
    }

    #[test]
    fn corrupt_and_unresolved_slots_consume_live_capacity_and_never_reclaim() {
        let mut slots = (0..MAX_TOURNAMENT_RECORDS)
            .map(|_| SlotState::Vacant {
                identity: FileIdentity { dev: 1, ino: 1 },
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
}

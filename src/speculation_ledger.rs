use crate::protocol::{SpeculationPhase, SpeculationStatus};
use crate::speculation_fs::{
    DirectoryIdentity, EvidenceError, EvidenceResult, MAX_SPECULATION_JSON_BYTES, StoredJson,
    ValidatedDirectory, atomic_create_json, atomic_replace_json, read_json,
};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientLedgerSchema {
    #[serde(rename = "lterm.speculation.client-ledger.v1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerAction {
    Prepared,
    ArmRequested,
    StatusMirrored,
    FinalizeRequested,
    RollbackRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRootIdentities {
    pub source: DirectoryIdentity,
    pub candidates: [DirectoryIdentity; 2],
    pub ledger_root: DirectoryIdentity,
    pub cgroup_root: DirectoryIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientLedgerRecord {
    pub schema_version: ClientLedgerSchema,
    pub tournament_uuid: Uuid,
    pub daemon_instance_uuid: Uuid,
    pub generation: u64,
    pub action: LedgerAction,
    pub roots: ClientRootIdentities,
    pub status: SpeculationStatus,
}

impl ClientLedgerRecord {
    pub fn validate(&self) -> EvidenceResult<()> {
        if self.schema_version != ClientLedgerSchema::V1
            || self.tournament_uuid != self.status.tournament_uuid
            || self.daemon_instance_uuid != self.status.daemon_instance_uuid
            || self.generation != self.status.generation
            || self.status.candidates[0].index != 0
            || self.status.candidates[1].index != 1
            || !all_roots_share_boot(&self.roots)
        {
            return Err(EvidenceError::Corrupt);
        }
        if self.action == LedgerAction::Prepared && self.status.phase != SpeculationPhase::Prepared
        {
            return Err(EvidenceError::Corrupt);
        }
        Ok(())
    }

    fn same_identity_as(&self, other: &Self) -> bool {
        self.tournament_uuid == other.tournament_uuid
            && self.daemon_instance_uuid == other.daemon_instance_uuid
            && self.roots == other.roots
    }
}

pub type ClientLedgerEntry = StoredJson<ClientLedgerRecord>;

pub struct ClientLedger {
    root: ValidatedDirectory,
}

impl std::fmt::Debug for ClientLedger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientLedger")
            .field("root_identity", &self.root.identity())
            .finish()
    }
}

impl ClientLedger {
    pub fn new(root: ValidatedDirectory) -> Self {
        Self { root }
    }

    pub fn create_prepared(
        &self,
        record: &ClientLedgerRecord,
    ) -> EvidenceResult<ClientLedgerEntry> {
        record.validate()?;
        if record.action != LedgerAction::Prepared
            || record.roots.ledger_root != self.root.identity()
        {
            return Err(EvidenceError::InvalidIdentity);
        }
        atomic_create_json(
            &self.root,
            &record_leaf(record.tournament_uuid)?,
            record,
            MAX_SPECULATION_JSON_BYTES,
        )
    }

    pub fn write_before_action(
        &self,
        current: &ClientLedgerEntry,
        next: &ClientLedgerRecord,
        action: LedgerAction,
        generation: u64,
    ) -> EvidenceResult<ClientLedgerEntry> {
        current.value.validate()?;
        next.validate()?;
        if !current.value.same_identity_as(next)
            || next.action != action
            || next.generation != generation
            || current.value.generation != generation
            || action == LedgerAction::Prepared
            || action == LedgerAction::StatusMirrored
        {
            return Err(EvidenceError::GenerationMismatch);
        }
        atomic_replace_json(
            &self.root,
            &record_leaf(next.tournament_uuid)?,
            current.identity,
            next,
            MAX_SPECULATION_JSON_BYTES,
        )
    }

    pub fn mirror_status(
        &self,
        current: &ClientLedgerEntry,
        status: &SpeculationStatus,
    ) -> EvidenceResult<ClientLedgerEntry> {
        current.value.validate()?;
        if status.tournament_uuid != current.value.tournament_uuid
            || status.daemon_instance_uuid != current.value.daemon_instance_uuid
            || status.generation < current.value.generation
        {
            return Err(EvidenceError::GenerationMismatch);
        }
        let next = ClientLedgerRecord {
            schema_version: ClientLedgerSchema::V1,
            tournament_uuid: status.tournament_uuid,
            daemon_instance_uuid: status.daemon_instance_uuid,
            generation: status.generation,
            action: LedgerAction::StatusMirrored,
            roots: current.value.roots.clone(),
            status: status.clone(),
        };
        next.validate()?;
        atomic_replace_json(
            &self.root,
            &record_leaf(next.tournament_uuid)?,
            current.identity,
            &next,
            MAX_SPECULATION_JSON_BYTES,
        )
    }

    pub fn read_verified(
        &self,
        tournament_uuid: Uuid,
        daemon_instance_uuid: Uuid,
    ) -> EvidenceResult<ClientLedgerEntry> {
        let entry: ClientLedgerEntry = read_json(
            &self.root,
            &record_leaf(tournament_uuid)?,
            MAX_SPECULATION_JSON_BYTES,
        )?;
        entry.value.validate()?;
        if entry.value.tournament_uuid != tournament_uuid
            || entry.value.daemon_instance_uuid != daemon_instance_uuid
            || entry.value.roots.ledger_root != self.root.identity()
            || entry.value.roots.ledger_root.boot_uuid != self.root.identity().boot_uuid
        {
            return Err(EvidenceError::Stale);
        }
        Ok(entry)
    }
}

fn all_roots_share_boot(roots: &ClientRootIdentities) -> bool {
    let expected = roots.ledger_root.boot_uuid;
    roots.source.boot_uuid == expected
        && roots.cgroup_root.boot_uuid == expected
        && roots
            .candidates
            .iter()
            .all(|identity| identity.boot_uuid == expected)
}

fn record_leaf(tournament_uuid: Uuid) -> EvidenceResult<CString> {
    CString::new(format!("{tournament_uuid}.json")).map_err(|_| EvidenceError::InvalidLeaf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> SpeculationStatus {
        serde_json::from_value(serde_json::json!({
            "schema_version": "lterm.speculation.status.v1",
            "tournament_uuid": Uuid::nil(),
            "daemon_instance_uuid": Uuid::from_u128(1),
            "phase": "prepared",
            "generation": 1,
            "lease_deadline_unix_ms": 42,
            "reason_code": "prepared_lease",
            "candidates": [
                {"candidate_uuid": Uuid::from_u128(2), "index": 0, "ready": false, "ready_elapsed_ns": null, "go_received": false, "go_received_elapsed_ns": null, "result_accepted": false, "exit_success": null, "exit_category": null, "elapsed_ns": null, "output_bytes": null, "eligible": false, "cleanup": {"runner_ack": false, "bwrap_reaped": false, "sync_eof": false, "cgroup_empty": false, "managed_tombstone": false}},
                {"candidate_uuid": Uuid::from_u128(3), "index": 1, "ready": false, "ready_elapsed_ns": null, "go_received": false, "go_received_elapsed_ns": null, "result_accepted": false, "exit_success": null, "exit_category": null, "elapsed_ns": null, "output_bytes": null, "eligible": false, "cleanup": {"runner_ack": false, "bwrap_reaped": false, "sync_eof": false, "cgroup_empty": false, "managed_tombstone": false}}
            ],
            "fixed_score_order": ["eligibility_descending", "exit_success_descending", "elapsed_ns_ascending", "output_bytes_ascending", "input_index_ascending"],
            "selected_index": null,
            "rollback_required": false,
            "error_codes": []
        })).unwrap()
    }

    fn roots() -> ClientRootIdentities {
        let identity = DirectoryIdentity::test_value();
        ClientRootIdentities {
            source: identity,
            candidates: [identity, identity],
            ledger_root: identity,
            cgroup_root: identity,
        }
    }

    #[test]
    fn ledger_contract_is_versioned_strict_and_raw_free() {
        let record = ClientLedgerRecord {
            schema_version: ClientLedgerSchema::V1,
            tournament_uuid: Uuid::nil(),
            daemon_instance_uuid: Uuid::from_u128(1),
            generation: 1,
            action: LedgerAction::Prepared,
            roots: roots(),
            status: status(),
        };
        record.validate().unwrap();
        let encoded = serde_json::to_string(&record).unwrap();
        for prohibited in [
            "path",
            "argv",
            "environment",
            "pid",
            "socket",
            "control",
            "locator",
        ] {
            assert!(!encoded.contains(prohibited), "leaked {prohibited}");
        }
        let mut unknown = serde_json::to_value(record).unwrap();
        unknown["cgroup_root_locator"] = "secret".into();
        assert!(serde_json::from_value::<ClientLedgerRecord>(unknown).is_err());
    }

    #[test]
    fn ledger_identity_or_generation_mismatch_is_fail_closed() {
        let mut record = ClientLedgerRecord {
            schema_version: ClientLedgerSchema::V1,
            tournament_uuid: Uuid::nil(),
            daemon_instance_uuid: Uuid::from_u128(1),
            generation: 1,
            action: LedgerAction::Prepared,
            roots: roots(),
            status: status(),
        };
        record.generation = 2;
        assert_eq!(record.validate(), Err(EvidenceError::Corrupt));
        record.generation = 1;
        record.daemon_instance_uuid = Uuid::from_u128(9);
        assert_eq!(record.validate(), Err(EvidenceError::Corrupt));
    }
}

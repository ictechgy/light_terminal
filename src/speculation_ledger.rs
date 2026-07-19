use crate::protocol::{SpeculationPhase, SpeculationStatus};
use crate::speculation_fs::{
    DurableDirectoryIdentity, EvidenceError, EvidenceResult, MAX_SPECULATION_JSON_BYTES,
    StoredJson, ValidatedDirectory, atomic_create_json, atomic_replace_json, read_json,
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

impl LedgerAction {
    pub(crate) const fn is_requested(self) -> bool {
        matches!(
            self,
            Self::ArmRequested | Self::FinalizeRequested | Self::RollbackRequested
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientRootIdentities {
    pub source: DurableDirectoryIdentity,
    pub candidates: [DurableDirectoryIdentity; 2],
    pub ledger_root: DurableDirectoryIdentity,
    pub cgroup_root: DurableDirectoryIdentity,
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
            || self.tournament_uuid.is_nil()
            || self.daemon_instance_uuid.is_nil()
            || self.generation == 0
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientLedgerAuthority {
    CurrentBoot,
    CleanupOnlyAfterBoot,
}

#[derive(Debug)]
pub struct TournamentLedgerEntry {
    pub entry: ClientLedgerEntry,
    pub authority: ClientLedgerAuthority,
}

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
        self.require_current_root(&current.value)?;
        if !current.value.same_identity_as(next)
            || next.action != action
            || next.generation != generation
            || current.value.generation != generation
            || current.value.action.is_requested()
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
        self.require_current_root(&current.value)?;
        if status.tournament_uuid != current.value.tournament_uuid
            || status.daemon_instance_uuid != current.value.daemon_instance_uuid
            || status.generation < current.value.generation
            || (current.value.action.is_requested()
                && status.generation == current.value.generation)
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
        {
            return Err(EvidenceError::Stale);
        }
        self.require_current_root(&entry.value)?;
        Ok(entry)
    }

    pub fn read_tournament(&self, tournament_uuid: Uuid) -> EvidenceResult<TournamentLedgerEntry> {
        if tournament_uuid.is_nil() {
            return Err(EvidenceError::InvalidIdentity);
        }
        let entry: ClientLedgerEntry = read_json(
            &self.root,
            &record_leaf(tournament_uuid)?,
            MAX_SPECULATION_JSON_BYTES,
        )?;
        entry.value.validate()?;
        if entry.value.tournament_uuid != tournament_uuid {
            return Err(EvidenceError::Stale);
        }
        let authority =
            classify_ledger_authority(entry.value.roots.ledger_root, self.root.identity())?;
        Ok(TournamentLedgerEntry { entry, authority })
    }

    fn require_current_root(&self, record: &ClientLedgerRecord) -> EvidenceResult<()> {
        if record.roots.ledger_root != self.root.identity() {
            return Err(EvidenceError::Stale);
        }
        Ok(())
    }
}

fn classify_ledger_authority(
    stored_root: DurableDirectoryIdentity,
    current_root: DurableDirectoryIdentity,
) -> EvidenceResult<ClientLedgerAuthority> {
    if stored_root == current_root {
        Ok(ClientLedgerAuthority::CurrentBoot)
    } else if stored_root.boot_uuid != current_root.boot_uuid {
        Ok(ClientLedgerAuthority::CleanupOnlyAfterBoot)
    } else {
        Err(EvidenceError::Stale)
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
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::PermissionsExt as _;

    fn status(tournament_uuid: Uuid) -> SpeculationStatus {
        serde_json::from_value(serde_json::json!({
            "schema_version": "lterm.speculation.status.v1",
            "tournament_uuid": tournament_uuid,
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
        let identity = DurableDirectoryIdentity::test_value();
        ClientRootIdentities {
            source: identity,
            candidates: [identity, identity],
            ledger_root: identity,
            cgroup_root: identity,
        }
    }

    fn test_record(tournament_uuid: Uuid, roots: ClientRootIdentities) -> ClientLedgerRecord {
        ClientLedgerRecord {
            schema_version: ClientLedgerSchema::V1,
            tournament_uuid,
            daemon_instance_uuid: Uuid::from_u128(1),
            generation: 1,
            action: LedgerAction::Prepared,
            roots,
            status: status(tournament_uuid),
        }
    }

    #[cfg(target_os = "linux")]
    fn private_ledger() -> (tempfile::TempDir, ClientLedger) {
        let directory = tempfile::tempdir().unwrap();
        let ledger_path = directory.path().join("ledger");
        std::fs::create_dir(&ledger_path).unwrap();
        std::fs::set_permissions(&ledger_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = crate::speculation_fs::open_existing_private_dir(&ledger_path).unwrap();
        (directory, ClientLedger::new(root))
    }

    #[test]
    fn authority_classification_is_exact_and_boot_scoped() {
        let current = DurableDirectoryIdentity::test_value();
        assert_eq!(
            classify_ledger_authority(current, current),
            Ok(ClientLedgerAuthority::CurrentBoot)
        );
        let mut replaced = current;
        replaced.ino = replaced.ino.saturating_add(1);
        assert_eq!(
            classify_ledger_authority(replaced, current),
            Err(EvidenceError::Stale)
        );
        let mut old_boot = replaced;
        old_boot.boot_uuid = Uuid::new_v4();
        assert_ne!(old_boot.boot_uuid, current.boot_uuid);
        assert_eq!(
            classify_ledger_authority(old_boot, current),
            Ok(ClientLedgerAuthority::CleanupOnlyAfterBoot)
        );
    }

    #[test]
    fn ledger_contract_is_versioned_strict_and_raw_free() {
        let record = test_record(Uuid::from_u128(7), roots());
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
        let mut record = test_record(Uuid::from_u128(7), roots());
        record.generation = 2;
        assert_eq!(record.validate(), Err(EvidenceError::Corrupt));
        record.generation = 1;
        record.daemon_instance_uuid = Uuid::from_u128(9);
        assert_eq!(record.validate(), Err(EvidenceError::Corrupt));
    }

    #[test]
    fn ledger_rejects_nil_authority_and_zero_generation() {
        let mut record = test_record(Uuid::from_u128(7), roots());
        record.tournament_uuid = Uuid::nil();
        record.status.tournament_uuid = Uuid::nil();
        assert_eq!(record.validate(), Err(EvidenceError::Corrupt));

        let mut record = test_record(Uuid::from_u128(7), roots());
        record.daemon_instance_uuid = Uuid::nil();
        record.status.daemon_instance_uuid = Uuid::nil();
        assert_eq!(record.validate(), Err(EvidenceError::Corrupt));

        let mut record = test_record(Uuid::from_u128(7), roots());
        record.generation = 0;
        record.status.generation = 0;
        assert_eq!(record.validate(), Err(EvidenceError::Corrupt));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn standalone_lookup_distinguishes_current_and_cleanup_only_authority() {
        let (_directory, ledger) = private_ledger();
        let tournament = Uuid::from_u128(7);
        let current_roots = ClientRootIdentities {
            source: ledger.root.identity(),
            candidates: [ledger.root.identity(), ledger.root.identity()],
            ledger_root: ledger.root.identity(),
            cgroup_root: ledger.root.identity(),
        };
        ledger
            .create_prepared(&test_record(tournament, current_roots.clone()))
            .unwrap();
        let current = ledger.read_tournament(tournament).unwrap();
        assert_eq!(current.authority, ClientLedgerAuthority::CurrentBoot);
        assert_eq!(current.entry.value.roots, current_roots);

        let (_old_directory, old_ledger) = private_ledger();
        let old_tournament = Uuid::from_u128(8);
        let mut old_identity = old_ledger.root.identity();
        old_identity.boot_uuid = Uuid::new_v4();
        assert_ne!(old_identity.boot_uuid, old_ledger.root.identity().boot_uuid);
        let old_roots = ClientRootIdentities {
            source: old_identity,
            candidates: [old_identity, old_identity],
            ledger_root: old_identity,
            cgroup_root: old_identity,
        };
        let old_record = test_record(old_tournament, old_roots);
        atomic_create_json(
            &old_ledger.root,
            &record_leaf(old_tournament).unwrap(),
            &old_record,
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        let cleanup_only = old_ledger.read_tournament(old_tournament).unwrap();
        assert_eq!(
            cleanup_only.authority,
            ClientLedgerAuthority::CleanupOnlyAfterBoot
        );
        assert!(matches!(
            old_ledger.read_verified(old_tournament, Uuid::from_u128(1)),
            Err(EvidenceError::Stale)
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn same_boot_root_replacement_and_wrong_tournament_fail_closed() {
        let (_directory, ledger) = private_ledger();
        let tournament = Uuid::from_u128(9);
        let mut replaced = ledger.root.identity();
        replaced.ino = replaced.ino.saturating_add(1);
        let replaced_roots = ClientRootIdentities {
            source: replaced,
            candidates: [replaced, replaced],
            ledger_root: replaced,
            cgroup_root: replaced,
        };
        atomic_create_json(
            &ledger.root,
            &record_leaf(tournament).unwrap(),
            &test_record(tournament, replaced_roots),
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        assert!(matches!(
            ledger.read_tournament(tournament),
            Err(EvidenceError::Stale)
        ));

        let wrong_leaf = Uuid::from_u128(10);
        atomic_create_json(
            &ledger.root,
            &record_leaf(wrong_leaf).unwrap(),
            &test_record(Uuid::from_u128(11), {
                let identity = ledger.root.identity();
                ClientRootIdentities {
                    source: identity,
                    candidates: [identity, identity],
                    ledger_root: identity,
                    cgroup_root: identity,
                }
            }),
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        assert!(matches!(
            ledger.read_tournament(wrong_leaf),
            Err(EvidenceError::Stale)
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn requested_action_is_immutable_until_new_generation_is_mirrored() {
        let (_directory, ledger) = private_ledger();
        let tournament = Uuid::from_u128(15);
        let identity = ledger.root.identity();
        let current_roots = ClientRootIdentities {
            source: identity,
            candidates: [identity, identity],
            ledger_root: identity,
            cgroup_root: identity,
        };
        let prepared = ledger
            .create_prepared(&test_record(tournament, current_roots))
            .unwrap();
        let mut arm_record = prepared.value.clone();
        arm_record.action = LedgerAction::ArmRequested;
        let arm = ledger
            .write_before_action(
                &prepared,
                &arm_record,
                LedgerAction::ArmRequested,
                prepared.value.generation,
            )
            .unwrap();

        for next_action in [
            LedgerAction::ArmRequested,
            LedgerAction::FinalizeRequested,
            LedgerAction::RollbackRequested,
        ] {
            let mut next = arm.value.clone();
            next.action = next_action;
            assert!(matches!(
                ledger.write_before_action(&arm, &next, next_action, arm.value.generation),
                Err(EvidenceError::GenerationMismatch)
            ));
        }
        assert!(matches!(
            ledger.mirror_status(&arm, &arm.value.status),
            Err(EvidenceError::GenerationMismatch)
        ));

        let mut newer_status = arm.value.status.clone();
        newer_status.generation += 1;
        newer_status.phase = SpeculationPhase::Armed;
        let mirrored = ledger.mirror_status(&arm, &newer_status).unwrap();
        assert_eq!(mirrored.value.action, LedgerAction::StatusMirrored);
        assert_eq!(mirrored.value.generation, 2);

        let mut rollback_record = mirrored.value.clone();
        rollback_record.action = LedgerAction::RollbackRequested;
        let rollback = ledger
            .write_before_action(
                &mirrored,
                &rollback_record,
                LedgerAction::RollbackRequested,
                mirrored.value.generation,
            )
            .unwrap();
        assert_eq!(rollback.value.action, LedgerAction::RollbackRequested);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn cleanup_only_entry_cannot_rewrite_or_mirror() {
        let (_directory, ledger) = private_ledger();
        let tournament = Uuid::from_u128(12);
        let mut old_identity = ledger.root.identity();
        old_identity.boot_uuid = Uuid::new_v4();
        let old_roots = ClientRootIdentities {
            source: old_identity,
            candidates: [old_identity, old_identity],
            ledger_root: old_identity,
            cgroup_root: old_identity,
        };
        let old_record = test_record(tournament, old_roots);
        atomic_create_json(
            &ledger.root,
            &record_leaf(tournament).unwrap(),
            &old_record,
            MAX_SPECULATION_JSON_BYTES,
        )
        .unwrap();
        let cleanup_only = ledger.read_tournament(tournament).unwrap();
        let mut next = cleanup_only.entry.value.clone();
        next.action = LedgerAction::RollbackRequested;
        assert!(matches!(
            ledger.write_before_action(
                &cleanup_only.entry,
                &next,
                LedgerAction::RollbackRequested,
                next.generation,
            ),
            Err(EvidenceError::Stale)
        ));
        assert!(matches!(
            ledger.mirror_status(&cleanup_only.entry, &cleanup_only.entry.value.status),
            Err(EvidenceError::Stale)
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn symlink_corruption_and_live_root_replacement_are_rejected() {
        use std::os::unix::fs::symlink;

        let (directory, ledger) = private_ledger();
        let ledger_path = directory.path().join("ledger");
        let corrupt = Uuid::from_u128(13);
        let corrupt_path = ledger_path.join(format!("{corrupt}.json"));
        std::fs::write(&corrupt_path, b"{}\n").unwrap();
        std::fs::set_permissions(&corrupt_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            ledger.read_tournament(corrupt),
            Err(EvidenceError::Corrupt)
        ));

        let linked = Uuid::from_u128(14);
        let target = directory.path().join("attacker-record");
        std::fs::write(&target, b"{}\n").unwrap();
        symlink(&target, ledger_path.join(format!("{linked}.json"))).unwrap();
        assert!(ledger.read_tournament(linked).is_err());

        let moved = directory.path().join("ledger-old");
        std::fs::rename(&ledger_path, &moved).unwrap();
        std::fs::create_dir(&ledger_path).unwrap();
        std::fs::set_permissions(&ledger_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            ledger.read_tournament(Uuid::from_u128(15)),
            Err(EvidenceError::Stale)
        ));
    }
}

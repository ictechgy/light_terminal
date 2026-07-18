use crate::protocol::{
    SPECULATION_SCORE_ORDER, SpeculationArmRequest, SpeculationArmResponse,
    SpeculationCandidateStatus, SpeculationErrorCode, SpeculationFinalizeRequest,
    SpeculationFinalizeResponse, SpeculationPhase, SpeculationPrepareRequest,
    SpeculationPrepareResponse, SpeculationReasonCode, SpeculationRollbackRequest,
    SpeculationRollbackResponse, SpeculationSchemaVersion, SpeculationStatus,
    SpeculationStatusRequest, SpeculationStatusResponse,
};
use crate::speculation::PREPARED_LEASE;
use crate::speculation_ledger::{
    ClientLedgerEntry, ClientLedgerRecord, ClientRootIdentities, LedgerAction,
};
use crate::speculation_linux::{
    ContainmentDeadline, ContainmentErrorCode, LiveTournamentContext, PrepareInputs,
    validate_prepare,
};
#[cfg(target_os = "linux")]
use crate::speculation_linux::{
    OldBootRecoveryAction, OldBootRecoveryEvidence, RecoveryAction, RecoveryEvidence,
    reconcile_different_boot, reconcile_from_record,
};
use crate::speculation_registry::{
    CandidateCgroupEvidence, CgroupForwardState, CgroupLifecycleState, TournamentCgroupEvidence,
    TournamentCgroupLifecycleState, TournamentKey, TournamentRecord, TournamentRecordSchema,
    TournamentStore, TournamentWriteKind,
};
#[cfg(target_os = "linux")]
use crate::speculation_registry::{StoredTournamentUpdate, TournamentRecoveryRecord};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const ACTOR_MAILBOX_CAPACITY: usize = 64;
const PREPARE_SERVICE_BUDGET: Duration = Duration::from_secs(3);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Availability {
    Disabled = 0,
    Reconciling = 1,
    Ready = 2,
    Unresolved = 3,
    ShuttingDown = 4,
}

impl Availability {
    fn from_raw(value: u8) -> Self {
        match value {
            1 => Self::Reconciling,
            2 => Self::Ready,
            3 => Self::Unresolved,
            4 => Self::ShuttingDown,
            _ => Self::Disabled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceError {
    Unsupported,
    Unavailable,
    Capacity,
    InvalidRequest,
    InvalidTransition,
    StaleGeneration,
    GenerationExhausted,
    ContainmentUnavailable,
    RollbackRequired,
    DecisionUncertain,
    EvidenceUnavailable,
}

impl ServiceError {
    pub(crate) const fn public_code(self) -> &'static str {
        match self {
            Self::Unsupported => "speculation_unsupported",
            Self::Unavailable => "speculation_unavailable",
            Self::Capacity => "speculation_capacity_exhausted",
            Self::InvalidRequest => "speculation_invalid_request",
            Self::InvalidTransition => "speculation_invalid_transition",
            Self::StaleGeneration => "speculation_stale_generation",
            Self::GenerationExhausted => "speculation_generation_exhausted",
            Self::ContainmentUnavailable => "speculation_containment_unavailable",
            Self::RollbackRequired => "speculation_rollback_required",
            Self::DecisionUncertain => "speculation_decision_uncertain",
            Self::EvidenceUnavailable => "speculation_evidence_unavailable",
        }
    }
}

impl From<ContainmentErrorCode> for ServiceError {
    fn from(value: ContainmentErrorCode) -> Self {
        match value {
            ContainmentErrorCode::Unsupported => Self::Unsupported,
            ContainmentErrorCode::InvalidIdentity => Self::InvalidRequest,
            ContainmentErrorCode::Timeout => Self::ContainmentUnavailable,
            _ => Self::ContainmentUnavailable,
        }
    }
}

type RpcReply = SyncSender<Result<SpeculationStatus, ServiceError>>;

enum ActorEvent {
    Arm {
        request: SpeculationArmRequest,
        reply: RpcReply,
    },
    Finalize {
        request: SpeculationFinalizeRequest,
        reply: RpcReply,
    },
    Rollback {
        request: SpeculationRollbackRequest,
        reason: SpeculationReasonCode,
        reply: RpcReply,
    },
    Shutdown {
        reply: SyncSender<Result<(), ServiceError>>,
    },
    WatchdogTick {
        now_unix_ms: u64,
    },
}

#[derive(Clone)]
struct ActorHandle {
    sender: SyncSender<ActorEvent>,
    snapshot: Arc<Mutex<SpeculationStatus>>,
}

#[derive(Default)]
struct ServiceIndex {
    live: HashMap<Uuid, ActorHandle>,
    terminal: HashMap<Uuid, Arc<SpeculationStatus>>,
}

struct ServiceCore {
    availability: AtomicU8,
    index: Mutex<ServiceIndex>,
    store: OnceLock<Arc<TournamentStore>>,
    control_root: OnceLock<PathBuf>,
    daemon_instance_uuid: Uuid,
    watchdog_stop: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct SpeculationService {
    core: Arc<ServiceCore>,
}

impl Default for SpeculationService {
    fn default() -> Self {
        Self {
            core: Arc::new(ServiceCore {
                availability: AtomicU8::new(Availability::Disabled as u8),
                index: Mutex::new(ServiceIndex::default()),
                store: OnceLock::new(),
                control_root: OnceLock::new(),
                daemon_instance_uuid: Uuid::nil(),
                watchdog_stop: AtomicBool::new(true),
            }),
        }
    }
}

impl SpeculationService {
    #[cfg(target_os = "linux")]
    pub(crate) fn production() -> Result<Self, ServiceError> {
        let service = Self::new_reconciling(Uuid::new_v4())?;
        let worker = service.clone();
        thread::Builder::new()
            .name("lterm-speculation-recovery".into())
            .spawn(move || worker.reconcile_startup())
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        Ok(service)
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn production() -> Result<Self, ServiceError> {
        Ok(Self::default())
    }

    pub(crate) fn new_reconciling(daemon_instance_uuid: Uuid) -> Result<Self, ServiceError> {
        if daemon_instance_uuid.is_nil() {
            return Err(ServiceError::InvalidRequest);
        }
        Ok(Self {
            core: Arc::new(ServiceCore {
                availability: AtomicU8::new(Availability::Reconciling as u8),
                index: Mutex::new(ServiceIndex::default()),
                store: OnceLock::new(),
                control_root: OnceLock::new(),
                daemon_instance_uuid,
                watchdog_stop: AtomicBool::new(false),
            }),
        })
    }

    pub(crate) fn availability(&self) -> Availability {
        Availability::from_raw(self.core.availability.load(Ordering::Acquire))
    }

    pub(crate) fn install_ready(
        &self,
        store: Arc<TournamentStore>,
        control_root: PathBuf,
    ) -> Result<(), ServiceError> {
        if self.availability() != Availability::Reconciling
            || self.core.store.set(store).is_err()
            || self.core.control_root.set(control_root).is_err()
        {
            self.mark_unresolved();
            return Err(ServiceError::EvidenceUnavailable);
        }
        self.core
            .availability
            .store(Availability::Ready as u8, Ordering::Release);
        Ok(())
    }

    pub(crate) fn mark_unresolved(&self) {
        self.core
            .availability
            .store(Availability::Unresolved as u8, Ordering::Release);
    }

    #[cfg(target_os = "linux")]
    fn reconcile_startup(&self) {
        if self.reconcile_startup_inner().is_err() {
            self.mark_unresolved();
        }
    }

    #[cfg(target_os = "linux")]
    fn reconcile_startup_inner(&self) -> Result<(), ServiceError> {
        let control_path = crate::paths::speculation_control_dir()
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let control_root = crate::speculation_fs::open_or_create_private_dir(&control_path)
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let current_private_identity = control_root.identity();
        control_root
            .revalidate()
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let store_path = crate::paths::tournament_registry_dir()
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let store = Arc::new(
            TournamentStore::open_or_create(&store_path)
                .map_err(|_| ServiceError::EvidenceUnavailable)?,
        );
        let managed = crate::launch_registry::reconcile_managed_processes()
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        if managed.entries.iter().any(|entry| {
            entry.owner.is_none()
                || matches!(
                    entry.outcome,
                    crate::launch_registry::ReconcileOutcome::Live
                        | crate::launch_registry::ReconcileOutcome::UnknownOrphanRisk(_)
                )
        }) {
            return Err(ServiceError::EvidenceUnavailable);
        }
        let recovery = store
            .scan_recovery()
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let mut terminal = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for entry in recovery {
            let TournamentRecoveryRecord::Valid { key, record } = entry else {
                return Err(ServiceError::EvidenceUnavailable);
            };
            if !seen.insert(record.status.tournament_uuid) {
                return Err(ServiceError::EvidenceUnavailable);
            }
            if record.is_positive_terminal() {
                terminal.insert(
                    record.status.tournament_uuid,
                    Arc::new(record.status.clone()),
                );
                continue;
            }
            let mut normalized = store
                .normalize_after_daemon_restart(
                    &key,
                    key.generation(),
                    self.core.daemon_instance_uuid,
                )
                .map_err(map_store_error)?;
            if normalized.record.status.phase != SpeculationPhase::RollbackRequired {
                return Err(ServiceError::EvidenceUnavailable);
            }
            if normalized.record.boot_uuid != current_private_identity.boot_uuid {
                normalized = close_different_boot(&store, normalized, current_private_identity)?;
            } else {
                normalized = close_same_boot(&store, normalized)?;
            }
            if !normalized.record.is_positive_terminal() {
                return Err(ServiceError::EvidenceUnavailable);
            }
            terminal.insert(
                normalized.record.status.tournament_uuid,
                Arc::new(normalized.record.status),
            );
        }
        {
            let mut index = self.index_lock()?;
            index.terminal = terminal;
        }
        self.install_ready(store, control_path)?;
        self.start_watchdog()?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn start_watchdog(&self) -> Result<(), ServiceError> {
        let service = self.clone();
        thread::Builder::new()
            .name("lterm-speculation-watchdog".into())
            .spawn(move || {
                while !service.core.watchdog_stop.load(Ordering::Acquire) {
                    if service.availability() == Availability::Ready {
                        if let Ok(now) = unix_time_ms() {
                            let _ = service.enqueue_watchdog_tick(now);
                        }
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            })
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        Ok(())
    }

    pub(crate) fn prepare(
        &self,
        request: SpeculationPrepareRequest,
    ) -> Result<SpeculationPrepareResponse, ServiceError> {
        self.require_ready()?;
        let store = Arc::clone(self.core.store.get().ok_or(ServiceError::Unavailable)?);
        let control_root = self
            .core
            .control_root
            .get()
            .cloned()
            .ok_or(ServiceError::Unavailable)?;
        let tournament_uuid = Uuid::new_v4();
        let generation = 1;
        let context = validate_prepare(
            PrepareInputs {
                tournament_uuid,
                generation,
                source: request.source().to_path_buf(),
                candidates: [
                    request.candidates()[0].to_path_buf(),
                    request.candidates()[1].to_path_buf(),
                ],
                ledger_root: request.ledger_root().to_path_buf(),
                cgroup_root: request.cgroup_root().to_path_buf(),
                control_root,
                argv: request.argv().to_os_strings(),
            },
            ContainmentDeadline::from_now(PREPARE_SERVICE_BUDGET)?,
        )?;
        let (roots, cgroup_root_locator) = context.durable_record_evidence()?;
        let now = unix_time_ms()?;
        let lease_deadline_unix_ms = now
            .checked_add(PREPARED_LEASE.as_millis() as u64)
            .ok_or(ServiceError::GenerationExhausted)?;
        let candidate_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        let status = prepared_status(
            tournament_uuid,
            self.core.daemon_instance_uuid,
            candidate_uuids,
            generation,
            lease_deadline_unix_ms,
        );
        let ledger = context.verified_ledger()?;
        let ledger_entry = ledger
            .create_prepared(&ClientLedgerRecord {
                schema_version: crate::speculation_ledger::ClientLedgerSchema::V1,
                tournament_uuid,
                daemon_instance_uuid: self.core.daemon_instance_uuid,
                generation,
                action: LedgerAction::Prepared,
                roots: ClientRootIdentities {
                    source: roots.source,
                    candidates: roots.candidates,
                    ledger_root: roots.ledger_root,
                    cgroup_root: roots.cgroup_root,
                },
                status: status.clone(),
            })
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let record = TournamentRecord {
            schema_version: TournamentRecordSchema::V1,
            boot_uuid: roots.source.boot_uuid,
            roots,
            cgroup_root_locator,
            tournament_cgroup: TournamentCgroupEvidence {
                deterministic_name_uuid: tournament_uuid,
                lifecycle: TournamentCgroupLifecycleState::Planned,
                domain: None,
            },
            cgroups: std::array::from_fn(|candidate| CandidateCgroupEvidence {
                candidate_index: candidate as u8,
                deterministic_name_uuid: candidate_uuids[candidate],
                lifecycle: CgroupLifecycleState::Forward(CgroupForwardState::Planned),
                parent: None,
                control: None,
                payload: None,
            }),
            managed_owners: [None, None],
            restart_prior_phase: None,
            terminal_completed_unix_ms: None,
            status: status.clone(),
        };
        let key = store
            .allocate_prepared(record, now)
            .map_err(map_store_error)?;
        let exact = store
            .load_by_uuid(tournament_uuid)
            .map_err(|_| ServiceError::EvidenceUnavailable)?
            .ok_or(ServiceError::EvidenceUnavailable)?;
        if exact.status != status || exact.status.generation != key.generation() {
            self.mark_unresolved();
            return Err(ServiceError::EvidenceUnavailable);
        }
        let snapshot = Arc::new(Mutex::new(exact.status.clone()));
        let (sender, receiver) = sync_channel(ACTOR_MAILBOX_CAPACITY);
        {
            let mut index = self.index_lock()?;
            if index.live.len() >= crate::speculation::MAX_LIVE_TOURNAMENTS
                || index.live.contains_key(&tournament_uuid)
            {
                self.mark_unresolved();
                return Err(ServiceError::Capacity);
            }
            index.live.insert(
                tournament_uuid,
                ActorHandle {
                    sender,
                    snapshot: Arc::clone(&snapshot),
                },
            );
        }
        let core = Arc::clone(&self.core);
        thread::Builder::new()
            .name("lterm-speculation-actor".into())
            .spawn(move || {
                actor_loop(ActorState {
                    core,
                    store,
                    key,
                    record: exact,
                    context,
                    ledger_entry,
                    snapshot,
                    receiver,
                });
            })
            .map_err(|_| {
                self.mark_unresolved();
                ServiceError::EvidenceUnavailable
            })?;
        Ok(SpeculationPrepareResponse { status })
    }

    pub(crate) fn arm(
        &self,
        request: SpeculationArmRequest,
    ) -> Result<SpeculationArmResponse, ServiceError> {
        let status = self.request_actor(request.tournament_uuid, |reply| ActorEvent::Arm {
            request,
            reply,
        })?;
        Ok(SpeculationArmResponse { status })
    }

    pub(crate) fn status(
        &self,
        request: SpeculationStatusRequest,
    ) -> Result<SpeculationStatusResponse, ServiceError> {
        self.require_ready()?;
        let status = self.cached_status(request.tournament_uuid)?;
        validate_status_identity(
            &status,
            request.tournament_uuid,
            request.daemon_instance_uuid,
            request.generation,
            false,
        )?;
        Ok(SpeculationStatusResponse { status })
    }

    pub(crate) fn finalize(
        &self,
        request: SpeculationFinalizeRequest,
    ) -> Result<SpeculationFinalizeResponse, ServiceError> {
        self.require_ready()?;
        if let Some(terminal) = self.cached_terminal(request.tournament_uuid)? {
            validate_status_identity(
                &terminal,
                request.tournament_uuid,
                request.daemon_instance_uuid,
                request.generation,
                false,
            )?;
            return Ok(SpeculationFinalizeResponse { status: terminal });
        }
        let status = self.request_actor(request.tournament_uuid, |reply| ActorEvent::Finalize {
            request,
            reply,
        })?;
        Ok(SpeculationFinalizeResponse { status })
    }

    pub(crate) fn rollback(
        &self,
        request: SpeculationRollbackRequest,
    ) -> Result<SpeculationRollbackResponse, ServiceError> {
        self.require_ready()?;
        if let Some(terminal) = self.cached_terminal(request.tournament_uuid)? {
            validate_status_identity(
                &terminal,
                request.tournament_uuid,
                request.daemon_instance_uuid,
                request.generation,
                false,
            )?;
            return Ok(SpeculationRollbackResponse { status: terminal });
        }
        let status = self.request_actor(request.tournament_uuid, |reply| ActorEvent::Rollback {
            request,
            reason: SpeculationReasonCode::ExplicitRollback,
            reply,
        })?;
        Ok(SpeculationRollbackResponse { status })
    }

    pub(crate) fn claim_shutdown(&self, deadline: Duration) -> Result<(), ServiceError> {
        self.core
            .availability
            .store(Availability::ShuttingDown as u8, Ordering::Release);
        let senders = {
            let index = self.index_lock()?;
            index
                .live
                .values()
                .map(|handle| handle.sender.clone())
                .collect::<Vec<_>>()
        };
        let end = std::time::Instant::now()
            .checked_add(deadline)
            .ok_or(ServiceError::EvidenceUnavailable)?;
        let mut replies = Vec::with_capacity(senders.len());
        for sender in senders {
            let (reply, receive) = sync_channel(1);
            sender
                .send(ActorEvent::Shutdown { reply })
                .map_err(|_| ServiceError::EvidenceUnavailable)?;
            replies.push(receive);
        }
        for reply in replies {
            let remaining = end.saturating_duration_since(std::time::Instant::now());
            reply
                .recv_timeout(remaining)
                .map_err(|_| ServiceError::EvidenceUnavailable)??;
        }
        Ok(())
    }

    pub(crate) fn enqueue_watchdog_tick(&self, now_unix_ms: u64) -> Result<(), ServiceError> {
        let senders = {
            let index = self.index_lock()?;
            index
                .live
                .values()
                .map(|handle| handle.sender.clone())
                .collect::<Vec<_>>()
        };
        for sender in senders {
            let _ = sender.try_send(ActorEvent::WatchdogTick { now_unix_ms });
        }
        Ok(())
    }

    fn request_actor(
        &self,
        tournament_uuid: Uuid,
        event: impl FnOnce(RpcReply) -> ActorEvent,
    ) -> Result<SpeculationStatus, ServiceError> {
        self.require_ready()?;
        let sender = {
            let index = self.index_lock()?;
            index
                .live
                .get(&tournament_uuid)
                .map(|handle| handle.sender.clone())
                .ok_or(ServiceError::InvalidRequest)?
        };
        let (reply, receive) = sync_channel(1);
        sender
            .send(event(reply))
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        receive
            .recv_timeout(crate::speculation::CONTROL_ACK_TIMEOUT)
            .map_err(|_| ServiceError::EvidenceUnavailable)?
    }

    fn cached_status(&self, tournament_uuid: Uuid) -> Result<SpeculationStatus, ServiceError> {
        let source = {
            let index = self.index_lock()?;
            if let Some(status) = index.terminal.get(&tournament_uuid) {
                return Ok((**status).clone());
            }
            index
                .live
                .get(&tournament_uuid)
                .map(|handle| Arc::clone(&handle.snapshot))
                .ok_or(ServiceError::InvalidRequest)?
        };
        source.lock().map(|status| status.clone()).map_err(|_| {
            self.mark_unresolved();
            ServiceError::EvidenceUnavailable
        })
    }

    fn cached_terminal(
        &self,
        tournament_uuid: Uuid,
    ) -> Result<Option<SpeculationStatus>, ServiceError> {
        let index = self.index_lock()?;
        Ok(index
            .terminal
            .get(&tournament_uuid)
            .map(|status| (**status).clone()))
    }

    fn require_ready(&self) -> Result<(), ServiceError> {
        match self.availability() {
            Availability::Ready => Ok(()),
            Availability::Disabled => Err(ServiceError::Unsupported),
            Availability::Reconciling | Availability::Unresolved | Availability::ShuttingDown => {
                Err(ServiceError::Unavailable)
            }
        }
    }

    fn index_lock(&self) -> Result<std::sync::MutexGuard<'_, ServiceIndex>, ServiceError> {
        self.core.index.lock().map_err(|_| {
            self.mark_unresolved();
            ServiceError::EvidenceUnavailable
        })
    }
}

#[cfg(target_os = "linux")]
fn close_different_boot(
    store: &Arc<TournamentStore>,
    mut update: StoredTournamentUpdate,
    current_private_identity: crate::speculation_fs::DurableDirectoryIdentity,
) -> Result<StoredTournamentUpdate, ServiceError> {
    update = transition_recovery_phase(store, update, SpeculationPhase::RollbackPending)?;
    for candidate in 0..2 {
        if !matches!(
            reconcile_different_boot(
                &update.record,
                current_private_identity,
                OldBootRecoveryAction::ManagedOwner { candidate },
            )?,
            OldBootRecoveryEvidence::ManagedOwnerAbsent { candidate: observed }
                if observed == candidate
        ) || !matches!(
            reconcile_different_boot(
                &update.record,
                current_private_identity,
                OldBootRecoveryAction::CandidateComponents { candidate },
            )?,
            OldBootRecoveryEvidence::CandidateComponentsAbsent { candidate: observed }
                if observed == candidate
        ) {
            return Err(ServiceError::EvidenceUnavailable);
        }
    }
    if reconcile_different_boot(
        &update.record,
        current_private_identity,
        OldBootRecoveryAction::TournamentDomain,
    )? != OldBootRecoveryEvidence::TournamentDomainAbsent
    {
        return Err(ServiceError::EvidenceUnavailable);
    }
    let mut next = update.record.clone();
    next.status.generation = increment(update.key.generation())?;
    for (index, candidate) in next.cgroups.iter_mut().enumerate() {
        candidate.lifecycle = CgroupLifecycleState::Removed;
        candidate.parent = None;
        candidate.control = None;
        candidate.payload = None;
        next.status.candidates[index].cleanup = fully_closed_cleanup();
    }
    next.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Removed;
    next.tournament_cgroup.domain = None;
    update = store
        .write(
            &update.key,
            update.key.generation(),
            next,
            TournamentWriteKind::OldBootAbsence {
                current_boot_uuid: current_private_identity.boot_uuid,
            },
        )
        .map_err(map_store_error)?;
    finish_recovery(store, update)
}

#[cfg(target_os = "linux")]
fn close_same_boot(
    store: &Arc<TournamentStore>,
    mut update: StoredTournamentUpdate,
) -> Result<StoredTournamentUpdate, ServiceError> {
    update = transition_recovery_phase(store, update, SpeculationPhase::RollbackPending)?;
    for candidate in 0_u8..2 {
        let index = usize::from(candidate);
        if let Some(owner) = update.record.managed_owners[index].as_ref() {
            require_recovery_complete(reconcile_from_record(
                &recovery_entry(&update),
                RecoveryAction::ReconcileManagedOwner {
                    candidate,
                    role: owner.role,
                },
                ContainmentDeadline::control_action(),
            )?)?;
        }
        loop {
            use CgroupForwardState as F;
            use CgroupLifecycleState as L;
            let lifecycle = update.record.cgroups[index].lifecycle;
            match lifecycle {
                L::Forward(F::Planned) => {
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].lifecycle = L::Removed;
                    })?;
                }
                L::Forward(F::ParentCreatePending) => {
                    let evidence = reconcile_from_record(
                        &recovery_entry(&update),
                        RecoveryAction::ReconcileCandidateCreate {
                            candidate,
                            component: crate::speculation_registry::CgroupComponent::Parent,
                        },
                        ContainmentDeadline::control_action(),
                    )?;
                    let RecoveryEvidence::CandidateCreateReconciled { identity, .. } = evidence
                    else {
                        return Err(ServiceError::EvidenceUnavailable);
                    };
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].parent = Some(identity);
                        record.cgroups[index].lifecycle = L::Forward(F::ParentCreated);
                    })?;
                }
                L::Forward(F::ControlCreatePending) => {
                    let evidence = reconcile_from_record(
                        &recovery_entry(&update),
                        RecoveryAction::ReconcileCandidateCreate {
                            candidate,
                            component: crate::speculation_registry::CgroupComponent::Control,
                        },
                        ContainmentDeadline::control_action(),
                    )?;
                    let RecoveryEvidence::CandidateCreateReconciled { identity, .. } = evidence
                    else {
                        return Err(ServiceError::EvidenceUnavailable);
                    };
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].control = Some(identity);
                        record.cgroups[index].lifecycle = L::Forward(F::ControlCreated);
                    })?;
                }
                L::Forward(F::PayloadCreatePending) => {
                    let evidence = reconcile_from_record(
                        &recovery_entry(&update),
                        RecoveryAction::ReconcileCandidateCreate {
                            candidate,
                            component: crate::speculation_registry::CgroupComponent::Payload,
                        },
                        ContainmentDeadline::control_action(),
                    )?;
                    let RecoveryEvidence::CandidateCreateReconciled { identity, .. } = evidence
                    else {
                        return Err(ServiceError::EvidenceUnavailable);
                    };
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].payload = Some(identity);
                        record.cgroups[index].lifecycle = L::Forward(F::PayloadCreated);
                    })?;
                }
                L::Forward(from) => {
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].lifecycle = L::CleanupPending { from };
                    })?;
                }
                L::CleanupPending { from } => {
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].lifecycle = L::ParentKillPending { from };
                    })?;
                }
                L::ParentKillPending { from } => {
                    for action in [
                        RecoveryAction::KillParent { candidate },
                        RecoveryAction::ProveParentEmpty { candidate },
                    ] {
                        require_recovery_complete(reconcile_from_record(
                            &recovery_entry(&update),
                            action,
                            ContainmentDeadline::control_action(),
                        )?)?;
                    }
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].lifecycle = L::ParentEmpty { from };
                    })?;
                }
                L::ParentEmpty { from } => {
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].lifecycle = L::PayloadRemovePending { from };
                    })?;
                }
                L::PayloadRemovePending { from } => {
                    require_recovery_complete(reconcile_from_record(
                        &recovery_entry(&update),
                        RecoveryAction::RemovePayload { candidate },
                        ContainmentDeadline::control_action(),
                    )?)?;
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].payload = None;
                        record.cgroups[index].lifecycle = L::PayloadRemoved { from };
                    })?;
                }
                L::PayloadRemoved { from } => {
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].lifecycle = L::ControlRemovePending { from };
                    })?;
                }
                L::ControlRemovePending { from } => {
                    require_recovery_complete(reconcile_from_record(
                        &recovery_entry(&update),
                        RecoveryAction::RemoveControl { candidate },
                        ContainmentDeadline::control_action(),
                    )?)?;
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].control = None;
                        record.cgroups[index].lifecycle = L::ControlRemoved { from };
                    })?;
                }
                L::ControlRemoved { from } => {
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].lifecycle = L::ParentRemovePending { from };
                    })?;
                }
                L::ParentRemovePending { .. } => {
                    require_recovery_complete(reconcile_from_record(
                        &recovery_entry(&update),
                        RecoveryAction::RemoveParent { candidate },
                        ContainmentDeadline::control_action(),
                    )?)?;
                    update = mutate_same(store, update, |record| {
                        record.cgroups[index].parent = None;
                        record.cgroups[index].lifecycle = L::Removed;
                    })?;
                }
                L::Removed => break,
                L::RollbackRequired => return Err(ServiceError::EvidenceUnavailable),
            }
        }
        update = mutate_same(store, update, |record| {
            record.status.candidates[index].cleanup = fully_closed_cleanup();
        })?;
    }
    update = match update.record.tournament_cgroup.lifecycle {
        TournamentCgroupLifecycleState::Planned => mutate_same(store, update, |record| {
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Removed;
        })?,
        TournamentCgroupLifecycleState::CreatePending => {
            let RecoveryEvidence::TournamentCreateReconciled { identity, .. } =
                reconcile_from_record(
                    &recovery_entry(&update),
                    RecoveryAction::ReconcileTournamentCreate,
                    ContainmentDeadline::control_action(),
                )?
            else {
                return Err(ServiceError::EvidenceUnavailable);
            };
            mutate_same(store, update, |record| {
                record.tournament_cgroup.domain = Some(identity);
                record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Created;
            })?
        }
        _ => update,
    };
    if update.record.tournament_cgroup.lifecycle == TournamentCgroupLifecycleState::Created {
        update = mutate_same(store, update, |record| {
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::RemovePending;
        })?;
    }
    if update.record.tournament_cgroup.lifecycle == TournamentCgroupLifecycleState::RemovePending {
        for action in [
            RecoveryAction::ProveTournamentEmpty,
            RecoveryAction::RemoveTournamentDomain,
        ] {
            require_recovery_complete(reconcile_from_record(
                &recovery_entry(&update),
                action,
                ContainmentDeadline::control_action(),
            )?)?;
        }
        update = mutate_same(store, update, |record| {
            record.tournament_cgroup.domain = None;
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Removed;
        })?;
    }
    if update.record.tournament_cgroup.lifecycle != TournamentCgroupLifecycleState::Removed {
        return Err(ServiceError::EvidenceUnavailable);
    }
    finish_recovery(store, update)
}

#[cfg(target_os = "linux")]
fn recovery_entry(update: &StoredTournamentUpdate) -> TournamentRecoveryRecord {
    TournamentRecoveryRecord::Valid {
        key: update.key,
        record: Box::new(update.record.clone()),
    }
}

#[cfg(target_os = "linux")]
fn require_recovery_complete(evidence: RecoveryEvidence) -> Result<(), ServiceError> {
    match evidence {
        RecoveryEvidence::CandidateActionComplete { .. }
        | RecoveryEvidence::TournamentActionComplete { .. }
        | RecoveryEvidence::ManagedOwnerReconciled { .. } => Ok(()),
        _ => Err(ServiceError::EvidenceUnavailable),
    }
}

#[cfg(target_os = "linux")]
fn transition_recovery_phase(
    store: &Arc<TournamentStore>,
    update: StoredTournamentUpdate,
    phase: SpeculationPhase,
) -> Result<StoredTournamentUpdate, ServiceError> {
    if update.record.status.phase == phase {
        return Ok(update);
    }
    let mut next = update.record.clone();
    next.status.generation = increment(update.key.generation())?;
    next.status.phase = phase;
    next.status.rollback_required = phase.is_rollback_only();
    next.status.selected_index = None;
    store
        .write(
            &update.key,
            update.key.generation(),
            next,
            TournamentWriteKind::LivePhaseTransition,
        )
        .map_err(map_store_error)
}

#[cfg(target_os = "linux")]
fn mutate_same(
    store: &Arc<TournamentStore>,
    update: StoredTournamentUpdate,
    mutate: impl FnOnce(&mut TournamentRecord),
) -> Result<StoredTournamentUpdate, ServiceError> {
    let mut next = update.record.clone();
    next.status.generation = increment(update.key.generation())?;
    mutate(&mut next);
    store
        .write(
            &update.key,
            update.key.generation(),
            next,
            TournamentWriteKind::SamePhaseEvidence,
        )
        .map_err(map_store_error)
}

#[cfg(target_os = "linux")]
fn finish_recovery(
    store: &Arc<TournamentStore>,
    update: StoredTournamentUpdate,
) -> Result<StoredTournamentUpdate, ServiceError> {
    let mut next = update.record.clone();
    next.status.generation = increment(update.key.generation())?;
    next.status.phase = SpeculationPhase::RolledBack;
    next.status.rollback_required = false;
    next.status.selected_index = None;
    next.terminal_completed_unix_ms = Some(unix_time_ms()?);
    store
        .write(
            &update.key,
            update.key.generation(),
            next,
            TournamentWriteKind::LivePhaseTransition,
        )
        .map_err(map_store_error)
}

#[cfg(target_os = "linux")]
fn fully_closed_cleanup() -> crate::protocol::SpeculationCleanupStatus {
    crate::protocol::SpeculationCleanupStatus {
        runner_ack: true,
        bwrap_reaped: true,
        sync_eof: true,
        cgroup_empty: true,
        managed_tombstone: true,
    }
}

fn increment(generation: u64) -> Result<u64, ServiceError> {
    generation
        .checked_add(1)
        .ok_or(ServiceError::GenerationExhausted)
}

struct ActorState {
    core: Arc<ServiceCore>,
    store: Arc<TournamentStore>,
    key: TournamentKey,
    record: TournamentRecord,
    context: LiveTournamentContext,
    ledger_entry: ClientLedgerEntry,
    snapshot: Arc<Mutex<SpeculationStatus>>,
    receiver: Receiver<ActorEvent>,
}

fn actor_loop(mut actor: ActorState) {
    while let Ok(event) = actor.receiver.recv() {
        match event {
            ActorEvent::Arm { request, reply } => {
                let result = actor.arm(request);
                let _ = reply.send(result);
            }
            ActorEvent::Finalize { request, reply } => {
                let result = actor.claim(
                    request.tournament_uuid,
                    request.daemon_instance_uuid,
                    request.generation,
                    SpeculationPhase::FinalizingLoser,
                    None,
                );
                let _ = reply.send(result);
            }
            ActorEvent::Rollback {
                request,
                reason,
                reply,
            } => {
                let result = actor.claim(
                    request.tournament_uuid,
                    request.daemon_instance_uuid,
                    request.generation,
                    SpeculationPhase::RollbackPending,
                    Some(reason),
                );
                let _ = reply.send(result);
            }
            ActorEvent::Shutdown { reply } => {
                let result = actor
                    .claim(
                        actor.record.status.tournament_uuid,
                        actor.record.status.daemon_instance_uuid,
                        actor.record.status.generation,
                        shutdown_claim_phase(actor.record.status.phase),
                        Some(SpeculationReasonCode::DaemonShutdown),
                    )
                    .map(|_| ());
                let _ = reply.send(result);
            }
            ActorEvent::WatchdogTick { now_unix_ms }
                if now_unix_ms >= actor.record.status.lease_deadline_unix_ms =>
            {
                let _ = actor.claim(
                    actor.record.status.tournament_uuid,
                    actor.record.status.daemon_instance_uuid,
                    actor.record.status.generation,
                    shutdown_claim_phase(actor.record.status.phase),
                    actor
                        .record
                        .status
                        .reason_code
                        .or(Some(SpeculationReasonCode::ContainmentEvidenceUnavailable)),
                );
            }
            ActorEvent::WatchdogTick { .. } => {}
        }
    }
}

impl ActorState {
    fn arm(&mut self, request: SpeculationArmRequest) -> Result<SpeculationStatus, ServiceError> {
        validate_status_identity(
            &self.record.status,
            request.tournament_uuid,
            request.daemon_instance_uuid,
            request.generation,
            true,
        )?;
        let ledger = self.context.verified_ledger()?;
        let exact = ledger
            .read_verified(request.tournament_uuid, request.daemon_instance_uuid)
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        if exact.value.action != LedgerAction::ArmRequested
            || exact.value.generation != request.generation
        {
            return Err(ServiceError::EvidenceUnavailable);
        }
        self.ledger_entry = exact;
        let status = self.claim(
            request.tournament_uuid,
            request.daemon_instance_uuid,
            request.generation,
            SpeculationPhase::Armed,
            None,
        )?;
        self.context
            .authorize_control_generation(status.generation)
            .map_err(|_| {
                self.core
                    .availability
                    .store(Availability::Unresolved as u8, Ordering::Release);
                ServiceError::EvidenceUnavailable
            })?;
        Ok(status)
    }

    fn claim(
        &mut self,
        tournament_uuid: Uuid,
        daemon_instance_uuid: Uuid,
        generation: u64,
        phase: SpeculationPhase,
        reason: Option<SpeculationReasonCode>,
    ) -> Result<SpeculationStatus, ServiceError> {
        validate_status_identity(
            &self.record.status,
            tournament_uuid,
            daemon_instance_uuid,
            generation,
            true,
        )?;
        if self.record.status.phase == phase && phase.is_terminal() {
            return Ok(self.record.status.clone());
        }
        if !crate::speculation::is_legal_transition(self.record.status.phase, phase) {
            return Err(ServiceError::InvalidTransition);
        }
        let mut next = self.record.clone();
        next.status.generation = generation
            .checked_add(1)
            .ok_or(ServiceError::GenerationExhausted)?;
        next.status.phase = phase;
        if let Some(reason) = reason {
            next.status.reason_code = Some(reason);
        }
        next.status.rollback_required = phase.is_rollback_only();
        if phase.is_rollback_only() {
            next.status.selected_index = None;
            let _ = next.status.error_codes.push(match phase {
                SpeculationPhase::DecisionUncertain => SpeculationErrorCode::DecisionUncertain,
                _ => SpeculationErrorCode::RollbackRequired,
            });
        }
        let stored = self
            .store
            .write(
                &self.key,
                generation,
                next,
                TournamentWriteKind::LivePhaseTransition,
            )
            .map_err(map_store_error)?;
        self.key = stored.key;
        self.record = stored.record;
        let status = self.record.status.clone();
        *self.snapshot.lock().map_err(|_| {
            self.core
                .availability
                .store(Availability::Unresolved as u8, Ordering::Release);
            ServiceError::EvidenceUnavailable
        })? = status.clone();
        Ok(status)
    }
}

fn shutdown_claim_phase(phase: SpeculationPhase) -> SpeculationPhase {
    if crate::speculation::is_legal_transition(phase, SpeculationPhase::RollbackPending) {
        SpeculationPhase::RollbackPending
    } else {
        SpeculationPhase::RollbackRequired
    }
}

fn prepared_status(
    tournament_uuid: Uuid,
    daemon_instance_uuid: Uuid,
    candidate_uuids: [Uuid; 2],
    generation: u64,
    lease_deadline_unix_ms: u64,
) -> SpeculationStatus {
    SpeculationStatus {
        schema_version: SpeculationSchemaVersion::V1,
        tournament_uuid,
        daemon_instance_uuid,
        phase: SpeculationPhase::Prepared,
        generation,
        lease_deadline_unix_ms,
        reason_code: Some(SpeculationReasonCode::PreparedLease),
        candidates: std::array::from_fn(|index| SpeculationCandidateStatus {
            candidate_uuid: candidate_uuids[index],
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
        }),
        fixed_score_order: SPECULATION_SCORE_ORDER,
        selected_index: None,
        rollback_required: false,
        error_codes: Default::default(),
    }
}

fn validate_status_identity(
    status: &SpeculationStatus,
    tournament_uuid: Uuid,
    daemon_instance_uuid: Uuid,
    generation: u64,
    exact_generation: bool,
) -> Result<(), ServiceError> {
    if tournament_uuid.is_nil()
        || daemon_instance_uuid.is_nil()
        || generation == 0
        || status.tournament_uuid != tournament_uuid
        || status.daemon_instance_uuid != daemon_instance_uuid
    {
        return Err(ServiceError::InvalidRequest);
    }
    if (exact_generation && status.generation != generation)
        || (!exact_generation && generation > status.generation)
    {
        return Err(ServiceError::StaleGeneration);
    }
    Ok(())
}

fn unix_time_ms() -> Result<u64, ServiceError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ServiceError::EvidenceUnavailable)?
        .as_millis();
    u64::try_from(millis).map_err(|_| ServiceError::EvidenceUnavailable)
}

fn map_store_error(error: crate::speculation_fs::EvidenceError) -> ServiceError {
    use crate::speculation_fs::EvidenceError;
    match error {
        EvidenceError::Capacity => ServiceError::Capacity,
        EvidenceError::GenerationMismatch => ServiceError::StaleGeneration,
        EvidenceError::Poisoned | EvidenceError::Corrupt | EvidenceError::Stale => {
            ServiceError::EvidenceUnavailable
        }
        _ => ServiceError::InvalidRequest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_default_has_no_store_threads_or_directories() {
        let service = SpeculationService::default();
        assert_eq!(service.availability(), Availability::Disabled);
        assert!(service.core.store.get().is_none());
        assert!(service.core.control_root.get().is_none());
        assert!(service.core.watchdog_stop.load(Ordering::Relaxed));
        assert_eq!(service.require_ready(), Err(ServiceError::Unsupported));
    }

    #[test]
    fn availability_and_errors_are_closed_raw_free_values() {
        assert_eq!(Availability::from_raw(255), Availability::Disabled);
        let values = [
            ServiceError::Unsupported,
            ServiceError::Unavailable,
            ServiceError::Capacity,
            ServiceError::InvalidRequest,
            ServiceError::InvalidTransition,
            ServiceError::StaleGeneration,
            ServiceError::GenerationExhausted,
            ServiceError::ContainmentUnavailable,
            ServiceError::RollbackRequired,
            ServiceError::DecisionUncertain,
            ServiceError::EvidenceUnavailable,
        ];
        assert!(
            values
                .iter()
                .all(|value| value.public_code().starts_with("speculation_"))
        );
    }

    #[test]
    fn status_identity_accepts_stale_poll_but_not_stale_mutation() {
        let status = prepared_status(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            [Uuid::from_u128(3), Uuid::from_u128(4)],
            9,
            10,
        );
        assert!(
            validate_status_identity(
                &status,
                status.tournament_uuid,
                status.daemon_instance_uuid,
                8,
                false,
            )
            .is_ok()
        );
        assert_eq!(
            validate_status_identity(
                &status,
                status.tournament_uuid,
                status.daemon_instance_uuid,
                8,
                true,
            ),
            Err(ServiceError::StaleGeneration)
        );
    }

    #[test]
    fn shutdown_claim_table_never_selects_or_resumes() {
        for phase in SpeculationPhase::ALL {
            let claim = shutdown_claim_phase(phase);
            assert!(matches!(
                claim,
                SpeculationPhase::RollbackPending | SpeculationPhase::RollbackRequired
            ));
        }
    }

    #[test]
    fn poisoned_index_fails_closed_and_marks_service_unresolved() {
        let service = SpeculationService::new_reconciling(Uuid::new_v4()).unwrap();
        let core = Arc::clone(&service.core);
        let _ = thread::spawn(move || {
            let _guard = core.index.lock().unwrap();
            panic!("poison speculation-only index");
        })
        .join();
        assert!(service.index_lock().is_err());
        assert_eq!(service.availability(), Availability::Unresolved);
    }

    #[test]
    fn disconnected_actor_prevents_successful_shutdown_claim() {
        let service = SpeculationService::new_reconciling(Uuid::new_v4()).unwrap();
        service
            .core
            .availability
            .store(Availability::Ready as u8, Ordering::Release);
        let (sender, receiver) = sync_channel(ACTOR_MAILBOX_CAPACITY);
        drop(receiver);
        let status = prepared_status(
            Uuid::from_u128(1),
            service.core.daemon_instance_uuid,
            [Uuid::from_u128(2), Uuid::from_u128(3)],
            1,
            2,
        );
        service.core.index.lock().unwrap().live.insert(
            status.tournament_uuid,
            ActorHandle {
                sender,
                snapshot: Arc::new(Mutex::new(status)),
            },
        );
        assert_eq!(
            service.claim_shutdown(Duration::from_millis(25)),
            Err(ServiceError::EvidenceUnavailable)
        );
        assert_eq!(service.availability(), Availability::ShuttingDown);
    }

    #[test]
    fn actor_mailbox_is_exactly_bounded_and_ticks_coalesce() {
        let (sender, _receiver) = sync_channel(ACTOR_MAILBOX_CAPACITY);
        for tick in 0..ACTOR_MAILBOX_CAPACITY {
            assert!(
                sender
                    .try_send(ActorEvent::WatchdogTick {
                        now_unix_ms: tick as u64,
                    })
                    .is_ok()
            );
        }
        assert!(matches!(
            sender.try_send(ActorEvent::WatchdogTick { now_unix_ms: 99 }),
            Err(std::sync::mpsc::TrySendError::Full(_))
        ));
    }

    #[test]
    fn live_index_capacity_is_eight_without_touching_terminal_cache() {
        let service = SpeculationService::new_reconciling(Uuid::new_v4()).unwrap();
        let mut index = service.core.index.lock().unwrap();
        for value in 1_u128..=8 {
            let (sender, _receiver) = sync_channel(1);
            let status = prepared_status(
                Uuid::from_u128(value),
                service.core.daemon_instance_uuid,
                [Uuid::from_u128(value + 20), Uuid::from_u128(value + 40)],
                1,
                2,
            );
            index.live.insert(
                status.tournament_uuid,
                ActorHandle {
                    sender,
                    snapshot: Arc::new(Mutex::new(status)),
                },
            );
        }
        assert_eq!(index.live.len(), crate::speculation::MAX_LIVE_TOURNAMENTS);
        assert!(index.terminal.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fresh_startup_reconciles_before_ready_and_starts_one_watchdog() {
        let _lock = crate::TEST_ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let data = root.path().join("data");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::create_dir(&data).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        let previous_runtime = std::env::var_os("LTERM_RUNTIME_DIR");
        let previous_data = std::env::var_os("LTERM_DATA_DIR");
        // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
        unsafe {
            std::env::set_var("LTERM_RUNTIME_DIR", &runtime);
            std::env::set_var("LTERM_DATA_DIR", &data);
        }
        let service = SpeculationService::production().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while service.availability() == Availability::Reconciling
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(service.availability(), Availability::Ready);
        assert!(service.core.store.get().is_some());
        assert!(service.core.control_root.get().is_some());
        service.core.watchdog_stop.store(true, Ordering::Release);
        // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation.
        unsafe {
            match previous_runtime {
                Some(value) => std::env::set_var("LTERM_RUNTIME_DIR", value),
                None => std::env::remove_var("LTERM_RUNTIME_DIR"),
            }
            match previous_data {
                Some(value) => std::env::set_var("LTERM_DATA_DIR", value),
                None => std::env::remove_var("LTERM_DATA_DIR"),
            }
        }
    }
}

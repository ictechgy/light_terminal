#[cfg(target_os = "linux")]
use crate::protocol::SpeculationExitCategory;
use crate::protocol::{
    SPECULATION_SCORE_ORDER, SpeculationArmRequest, SpeculationArmResponse,
    SpeculationCandidateStatus, SpeculationErrorCode, SpeculationFinalizeRequest,
    SpeculationFinalizeResponse, SpeculationPhase, SpeculationPrepareRequest,
    SpeculationPrepareResponse, SpeculationReasonCode, SpeculationRollbackRequest,
    SpeculationRollbackResponse, SpeculationSchemaVersion, SpeculationStatus,
    SpeculationStatusRequest, SpeculationStatusResponse,
};
#[cfg(target_os = "linux")]
use crate::speculation::MAX_CANDIDATE_OUTPUT_BYTES;
use crate::speculation::{
    DEFAULT_RUN_TIMEOUT, PENDING_FINALIZE_LEASE, PREPARED_LEASE, READY_LEASE,
};
use crate::speculation_ledger::{
    ClientLedgerEntry, ClientLedgerRecord, ClientRootIdentities, LedgerAction,
};
#[cfg(target_os = "linux")]
use crate::speculation_linux::{
    CandidateCleanupAction, CandidateControl, CandidateObserver, ContainmentEvent,
    GoReceiptEvidence, OldBootRecoveryAction, OldBootRecoveryEvidence, PayloadPlacementEvidence,
    RecoveryAction, RecoveryEvidence, TopologyAction, TopologyEvidence, TournamentCleanupAction,
    TournamentTopology, acknowledge_output_cleanup_claimed, begin_topology, create_topology,
    finish_containment, go_receipt_skew_ns, launch_fixed_probe, launch_runner,
    observe_managed_reaped, observe_sync_eof, perform_candidate_cleanup_action,
    perform_tournament_cleanup_action, receive_decision_ack, receive_execution_event,
    receive_go_receipt, receive_output_drained, receive_payload_fd_ack,
    receive_payload_placed_owned, reconcile_different_boot, reconcile_from_record, send_go,
    send_payload_release, send_select_or_abort, transfer_payload_fd_owned,
};
use crate::speculation_linux::{
    ContainmentDeadline, ContainmentErrorCode, LiveTournamentContext, PrepareInputs,
    validate_prepare,
};
use crate::speculation_registry::{
    CandidateCgroupEvidence, CgroupForwardState, CgroupLifecycleState, TournamentCgroupEvidence,
    TournamentCgroupLifecycleState, TournamentKey, TournamentRecord, TournamentRecordSchema,
    TournamentStore, TournamentWriteKind,
};
#[cfg(target_os = "linux")]
use crate::speculation_registry::{StoredTournamentUpdate, TournamentRecoveryRecord};
#[cfg(target_os = "linux")]
use crate::speculation_runner::{DecisionKind, RunnerExitCategory};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
#[cfg(target_os = "linux")]
use std::sync::mpsc::TryRecvError;
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
    #[cfg(target_os = "linux")]
    Observed(Box<ObserverCompletion>),
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum ObserverOperation {
    PayloadFdAck,
    GoReceipt,
    PayloadPlaced,
    Execution,
    OutputDrained,
    DecisionAck(DecisionKind),
    SyncEof,
    ManagedReaped,
}

#[cfg(target_os = "linux")]
enum ObserverEvidence {
    Event(ContainmentEvent),
    GoReceipt(GoReceiptEvidence),
    PayloadPlaced(PayloadPlacementEvidence),
}

#[cfg(target_os = "linux")]
struct ObserverCompletion {
    authorization_generation: u64,
    candidate: u8,
    observer: CandidateObserver,
    result: Result<ObserverEvidence, ContainmentErrorCode>,
}

#[cfg(target_os = "linux")]
struct LiveCandidate {
    control: CandidateControl,
    observer: Option<CandidateObserver>,
}

#[cfg(target_os = "linux")]
struct LivePipeline {
    topology: TournamentTopology,
    candidates: [Option<LiveCandidate>; 2],
    authorization_generation: u64,
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
                    sender: sender.clone(),
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
                    sender: sender.clone(),
                    receiver,
                    #[cfg(target_os = "linux")]
                    pipeline: None,
                    interrupt_requested: false,
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
    sender: SyncSender<ActorEvent>,
    receiver: Receiver<ActorEvent>,
    #[cfg(target_os = "linux")]
    pipeline: Option<LivePipeline>,
    interrupt_requested: bool,
}

fn actor_loop(mut actor: ActorState) {
    while let Ok(event) = actor.receiver.recv() {
        match event {
            ActorEvent::Arm { request, reply } => {
                let result = actor.arm(request);
                let _claimed = result.is_ok();
                let _ = reply.send(result);
                #[cfg(target_os = "linux")]
                if _claimed && actor.run_to_pending_finalize().is_err() {
                    actor.fail_closed_pipeline();
                }
            }
            ActorEvent::Finalize { request, reply } => {
                let result = actor.claim(
                    request.tournament_uuid,
                    request.daemon_instance_uuid,
                    request.generation,
                    SpeculationPhase::FinalizingLoser,
                    None,
                );
                let _claimed = result.is_ok();
                let _ = reply.send(result);
                #[cfg(target_os = "linux")]
                if _claimed && actor.finalize_pipeline().is_err() {
                    actor.fail_closed_pipeline();
                }
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
                let _claimed = result.is_ok();
                let _ = reply.send(result);
                #[cfg(target_os = "linux")]
                if _claimed {
                    actor.rollback_pipeline();
                }
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
                #[cfg(target_os = "linux")]
                actor.rollback_pipeline();
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
                #[cfg(target_os = "linux")]
                actor.rollback_pipeline();
            }
            ActorEvent::WatchdogTick { .. } => {}
            #[cfg(target_os = "linux")]
            ActorEvent::Observed(_) => {
                actor.fail_closed_pipeline();
            }
        }
        if actor.record.status.is_terminal() {
            #[cfg(target_os = "linux")]
            actor.publish_terminal();
            break;
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
        let lease = match phase {
            SpeculationPhase::Ready | SpeculationPhase::GoPending => Some(READY_LEASE),
            SpeculationPhase::Running | SpeculationPhase::ResultPending => {
                Some(DEFAULT_RUN_TIMEOUT)
            }
            SpeculationPhase::PendingFinalize
            | SpeculationPhase::FinalizingLoser
            | SpeculationPhase::WinnerSelectionPending
            | SpeculationPhase::FinalizingWinner => Some(PENDING_FINALIZE_LEASE),
            _ => None,
        };
        if let Some(lease) = lease {
            next.status.lease_deadline_unix_ms = unix_time_ms()?
                .checked_add(lease.as_millis() as u64)
                .ok_or(ServiceError::GenerationExhausted)?;
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

#[cfg(target_os = "linux")]
impl ActorState {
    fn persist_same(
        &mut self,
        mutate: impl FnOnce(&mut TournamentRecord),
    ) -> Result<SpeculationStatus, ServiceError> {
        let mut next = self.record.clone();
        next.status.generation = increment(self.key.generation())?;
        mutate(&mut next);
        let stored = self
            .store
            .write(
                &self.key,
                self.key.generation(),
                next,
                TournamentWriteKind::SamePhaseEvidence,
            )
            .map_err(map_store_error)?;
        self.install_stored(stored)
    }

    fn install_stored(
        &mut self,
        stored: StoredTournamentUpdate,
    ) -> Result<SpeculationStatus, ServiceError> {
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

    fn transition(
        &mut self,
        phase: SpeculationPhase,
        reason: Option<SpeculationReasonCode>,
    ) -> Result<SpeculationStatus, ServiceError> {
        self.claim(
            self.record.status.tournament_uuid,
            self.record.status.daemon_instance_uuid,
            self.record.status.generation,
            phase,
            reason,
        )
    }

    fn spawn_observer(
        &self,
        candidate: u8,
        observer: CandidateObserver,
        operation: ObserverOperation,
    ) -> Result<(), ServiceError> {
        let sender = self.sender.clone();
        let authorization_generation = self.pipeline.as_ref().map_or_else(
            || self.context.identity().generation,
            |pipeline| pipeline.authorization_generation,
        );
        thread::Builder::new()
            .name("lterm-speculation-observer".into())
            .spawn(move || {
                let mut observer = observer;
                let deadline = ContainmentDeadline::control_action();
                let result = match operation {
                    ObserverOperation::PayloadFdAck => {
                        receive_payload_fd_ack(&mut observer, deadline).map(ObserverEvidence::Event)
                    }
                    ObserverOperation::GoReceipt => {
                        receive_go_receipt(&mut observer, deadline).map(ObserverEvidence::GoReceipt)
                    }
                    ObserverOperation::PayloadPlaced => {
                        receive_payload_placed_owned(&mut observer, deadline)
                            .map(ObserverEvidence::PayloadPlaced)
                    }
                    ObserverOperation::Execution => {
                        receive_execution_event(&mut observer, deadline)
                            .map(ObserverEvidence::Event)
                    }
                    ObserverOperation::OutputDrained => {
                        receive_output_drained(&mut observer, deadline).map(ObserverEvidence::Event)
                    }
                    ObserverOperation::DecisionAck(decision) => {
                        receive_decision_ack(&mut observer, decision, deadline)
                            .map(ObserverEvidence::Event)
                    }
                    ObserverOperation::SyncEof => {
                        observe_sync_eof(&mut observer, deadline).map(ObserverEvidence::Event)
                    }
                    ObserverOperation::ManagedReaped => {
                        observe_managed_reaped(&mut observer, deadline).map(ObserverEvidence::Event)
                    }
                };
                let _ = sender.send(ActorEvent::Observed(Box::new(ObserverCompletion {
                    authorization_generation,
                    candidate,
                    observer,
                    result,
                })));
            })
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        Ok(())
    }

    fn wait_observer(
        &mut self,
        candidate: u8,
        observer: CandidateObserver,
        operation: ObserverOperation,
    ) -> Result<(CandidateObserver, ObserverEvidence), ServiceError> {
        let expected_generation = self.pipeline.as_ref().map_or_else(
            || self.context.identity().generation,
            |pipeline| pipeline.authorization_generation,
        );
        self.spawn_observer(candidate, observer, operation)?;
        loop {
            match self
                .receiver
                .recv()
                .map_err(|_| ServiceError::EvidenceUnavailable)?
            {
                ActorEvent::Observed(completion)
                    if completion.authorization_generation == expected_generation
                        && completion.candidate == candidate =>
                {
                    let ObserverCompletion {
                        observer, result, ..
                    } = *completion;
                    return result
                        .map(|evidence| (observer, evidence))
                        .map_err(ServiceError::from);
                }
                ActorEvent::Observed(_) => {}
                event => self.handle_pipeline_event(event)?,
            }
        }
    }

    fn handle_pipeline_event(&mut self, event: ActorEvent) -> Result<(), ServiceError> {
        match event {
            ActorEvent::Rollback {
                request,
                reason,
                reply,
            } => {
                let result = self.claim(
                    request.tournament_uuid,
                    request.daemon_instance_uuid,
                    request.generation,
                    SpeculationPhase::RollbackPending,
                    Some(reason),
                );
                if result.is_ok() {
                    self.interrupt_requested = true;
                }
                let _ = reply.send(result);
            }
            ActorEvent::Shutdown { reply } => {
                let result = self
                    .transition(
                        shutdown_claim_phase(self.record.status.phase),
                        Some(SpeculationReasonCode::DaemonShutdown),
                    )
                    .map(|_| ());
                if result.is_ok() {
                    self.interrupt_requested = true;
                }
                let _ = reply.send(result);
            }
            ActorEvent::WatchdogTick { now_unix_ms }
                if now_unix_ms >= self.record.status.lease_deadline_unix_ms =>
            {
                self.transition(
                    shutdown_claim_phase(self.record.status.phase),
                    self.record
                        .status
                        .reason_code
                        .or(Some(SpeculationReasonCode::ContainmentEvidenceUnavailable)),
                )?;
                self.interrupt_requested = true;
            }
            ActorEvent::WatchdogTick { .. } => {}
            ActorEvent::Arm { reply, .. } => {
                let _ = reply.send(Err(ServiceError::InvalidTransition));
            }
            ActorEvent::Finalize { reply, .. } => {
                let _ = reply.send(Err(ServiceError::InvalidTransition));
            }
            ActorEvent::Observed(_) => {}
        }
        Ok(())
    }

    fn drain_pipeline_events(&mut self) -> Result<(), ServiceError> {
        loop {
            match self.receiver.try_recv() {
                Ok(ActorEvent::Observed(_)) => return Err(ServiceError::EvidenceUnavailable),
                Ok(event) => self.handle_pipeline_event(event)?,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => return Err(ServiceError::EvidenceUnavailable),
            }
        }
    }

    fn ensure_not_interrupted(&mut self) -> Result<(), ServiceError> {
        self.drain_pipeline_events()?;
        if self.interrupt_requested || self.record.status.phase.is_rollback_only() {
            Err(ServiceError::RollbackRequired)
        } else {
            Ok(())
        }
    }

    fn run_to_pending_finalize(&mut self) -> Result<(), ServiceError> {
        self.transition(SpeculationPhase::Starting, None)?;
        self.pipeline = Some(LivePipeline {
            topology: begin_topology(&self.context)?,
            candidates: [None, None],
            authorization_generation: self.context.identity().generation,
        });
        self.create_durable_topology()?;
        for candidate in 0_u8..2 {
            self.run_probe(candidate)?;
            self.ensure_not_interrupted()?;
        }

        let authorization_generation = increment(self.record.status.generation)?;
        self.context
            .authorize_control_generation(authorization_generation)?;
        self.pipeline
            .as_mut()
            .ok_or(ServiceError::EvidenceUnavailable)?
            .authorization_generation = authorization_generation;
        for candidate in 0_u8..2 {
            self.persist_same(|record| {
                record.cgroups[usize::from(candidate)].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::ControlAttachPending);
            })?;
            let containment = {
                let pipeline = self
                    .pipeline
                    .as_ref()
                    .ok_or(ServiceError::EvidenceUnavailable)?;
                launch_runner(
                    &self.context,
                    &pipeline.topology,
                    candidate,
                    ContainmentDeadline::control_action(),
                )?
            };
            let observation = containment.observation();
            let parts = containment.split();
            self.persist_same(|record| {
                let index = usize::from(candidate);
                record.managed_owners[index] = Some(observation.managed_owner);
                record.cgroups[index].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::ControlAttached);
                record.status.candidates[index].ready = true;
                record.status.candidates[index].ready_elapsed_ns = Some(elapsed_stamp());
            })?;
            self.pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?
                .candidates[usize::from(candidate)] = Some(LiveCandidate {
                control: parts.control,
                observer: Some(parts.observer),
            });
            self.ensure_not_interrupted()?;
        }
        self.transition(
            SpeculationPhase::Ready,
            Some(SpeculationReasonCode::ReadyLease),
        )?;
        self.arm_payload_descriptors()?;
        self.transition(SpeculationPhase::GoPending, None)?;
        for candidate in 0_u8..2 {
            self.persist_same(|record| {
                record.cgroups[usize::from(candidate)].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::PayloadExecPending);
            })?;
        }
        let sends = [self.send_go_for(0)?, self.send_go_for(1)?];
        if sends[0]
            .sent_monotonic_ns
            .abs_diff(sends[1].sent_monotonic_ns)
            > crate::speculation_linux::MAX_GO_RECEIPT_SKEW_NS
        {
            return Err(ServiceError::ContainmentUnavailable);
        }
        let receipts = [self.observe_go_receipt(0)?, self.observe_go_receipt(1)?];
        go_receipt_skew_ns(receipts)?;
        for (candidate, receipt) in receipts.into_iter().enumerate() {
            self.persist_same(|record| {
                let status = &mut record.status.candidates[candidate];
                status.go_received = true;
                status.go_received_elapsed_ns = Some(receipt.received_monotonic_ns);
            })?;
        }
        for candidate in 0_u8..2 {
            let placement = self.observe_payload_placement(candidate)?;
            self.persist_same(|record| {
                record.cgroups[usize::from(candidate)].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::PayloadAttached);
            })?;
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            let live = pipeline.candidates[usize::from(candidate)]
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            send_payload_release(
                &mut live.control,
                placement,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.transition(
            SpeculationPhase::Running,
            Some(SpeculationReasonCode::RunningLease),
        )?;
        for candidate in 0_u8..2 {
            self.collect_candidate_result(candidate)?;
            self.ensure_not_interrupted()?;
        }
        self.transition(SpeculationPhase::ResultPending, None)?;
        let decision = crate::speculation::score_candidates(std::array::from_fn(|index| {
            let candidate = &self.record.status.candidates[index];
            crate::speculation::CandidateResult {
                input_index: index as u8,
                exit_success: candidate.exit_success,
                elapsed_ns: candidate.elapsed_ns,
                output_bytes: candidate.output_bytes,
                quiescent: self.record.cgroups[index].lifecycle
                    == CgroupLifecycleState::Forward(CgroupForwardState::PayloadEmpty),
                output_overflowed: candidate
                    .output_bytes
                    .is_some_and(|bytes| bytes > MAX_CANDIDATE_OUTPUT_BYTES),
            }
        }));
        let crate::speculation::ScoreDecision::Selected(selected) = decision else {
            self.transition(
                SpeculationPhase::RollbackPending,
                Some(SpeculationReasonCode::BothCandidatesIneligible),
            )?;
            return Err(ServiceError::RollbackRequired);
        };
        self.persist_same(|record| record.status.selected_index = Some(selected))?;
        self.transition(
            SpeculationPhase::PendingFinalize,
            Some(SpeculationReasonCode::PendingFinalizeLease),
        )?;
        Ok(())
    }

    fn create_durable_topology(&mut self) -> Result<(), ServiceError> {
        self.persist_same(|record| {
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::CreatePending;
        })?;
        let TopologyEvidence::TournamentDomain(identity) =
            self.topology_action(TopologyAction::CreateTournamentDomain)?
        else {
            return Err(ServiceError::EvidenceUnavailable);
        };
        self.persist_same(|record| {
            record.tournament_cgroup.domain = Some(identity);
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Created;
        })?;
        for candidate in 0_u8..2 {
            let index = usize::from(candidate);
            self.persist_same(|record| {
                record.cgroups[index].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::ParentCreatePending);
            })?;
            let TopologyEvidence::CandidateParent {
                identity: parent, ..
            } = self.topology_action(TopologyAction::CreateCandidateParent { candidate })?
            else {
                return Err(ServiceError::EvidenceUnavailable);
            };
            self.persist_same(|record| {
                record.cgroups[index].parent = Some(parent);
                record.cgroups[index].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::ParentCreated);
            })?;
            self.persist_same(|record| {
                record.cgroups[index].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::ControlCreatePending);
            })?;
            let TopologyEvidence::ControlLeaf {
                identity: control, ..
            } = self.topology_action(TopologyAction::CreateControlLeaf { candidate })?
            else {
                return Err(ServiceError::EvidenceUnavailable);
            };
            self.persist_same(|record| {
                record.cgroups[index].control = Some(control);
                record.cgroups[index].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::ControlCreated);
            })?;
            self.persist_same(|record| {
                record.cgroups[index].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::PayloadCreatePending);
            })?;
            let TopologyEvidence::PayloadLeaf {
                identity: payload, ..
            } = self.topology_action(TopologyAction::CreatePayloadLeaf { candidate })?
            else {
                return Err(ServiceError::EvidenceUnavailable);
            };
            self.persist_same(|record| {
                record.cgroups[index].payload = Some(payload);
                record.cgroups[index].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::PayloadCreated);
            })?;
            self.persist_same(|_| {})?;
            if !matches!(
                self.topology_action(TopologyAction::ConfigurePayloadLimit { candidate })?,
                TopologyEvidence::PayloadLimit {
                    candidate: observed,
                    pids_max: 256,
                } if observed == candidate
            ) {
                return Err(ServiceError::EvidenceUnavailable);
            }
        }
        Ok(())
    }

    fn topology_action(
        &mut self,
        action: TopologyAction,
    ) -> Result<TopologyEvidence, ServiceError> {
        let pipeline = self
            .pipeline
            .as_mut()
            .ok_or(ServiceError::EvidenceUnavailable)?;
        create_topology(&mut pipeline.topology, action).map_err(ServiceError::from)
    }

    fn candidate_action(
        &mut self,
        candidate: u8,
        action: CandidateCleanupAction,
        deadline: ContainmentDeadline,
    ) -> Result<crate::speculation_linux::CandidateCleanupEvidence, ServiceError> {
        let pipeline = self
            .pipeline
            .as_mut()
            .ok_or(ServiceError::EvidenceUnavailable)?;
        perform_candidate_cleanup_action(&mut pipeline.topology, candidate, action, deadline)
            .map_err(ServiceError::from)
    }

    fn run_probe(&mut self, candidate: u8) -> Result<(), ServiceError> {
        let index = usize::from(candidate);
        self.persist_same(|record| {
            record.cgroups[index].lifecycle =
                CgroupLifecycleState::Forward(CgroupForwardState::ProbePending);
        })?;
        let containment = {
            let pipeline = self
                .pipeline
                .as_ref()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            launch_fixed_probe(
                &self.context,
                pipeline.topology.candidate(candidate)?,
                candidate,
                ContainmentDeadline::control_action(),
            )?
        };
        let observation = containment.observation();
        let parts = containment.split();
        self.persist_same(|record| {
            record.managed_owners[index] = Some(observation.managed_owner);
        })?;
        self.pipeline
            .as_mut()
            .ok_or(ServiceError::EvidenceUnavailable)?
            .candidates[index] = Some(LiveCandidate {
            control: parts.control,
            observer: Some(parts.observer),
        });
        self.persist_same(|_| {})?;
        {
            let live = self
                .pipeline
                .as_mut()
                .and_then(|pipeline| pipeline.candidates[index].as_mut())
                .ok_or(ServiceError::EvidenceUnavailable)?;
            transfer_payload_fd_owned(&mut live.control, ContainmentDeadline::control_action())?;
        }
        let observer = self.take_observer(candidate)?;
        let (observer, _) =
            self.wait_observer(candidate, observer, ObserverOperation::PayloadFdAck)?;
        self.restore_observer(candidate, observer)?;
        self.ensure_not_interrupted()?;
        self.persist_same(|_| {})?;
        self.send_go_for(candidate)?;
        let observer = self.take_observer(candidate)?;
        let (observer, evidence) =
            self.wait_observer(candidate, observer, ObserverOperation::GoReceipt)?;
        self.restore_observer(candidate, observer)?;
        if !matches!(evidence, ObserverEvidence::GoReceipt(_)) {
            return Err(ServiceError::EvidenceUnavailable);
        }
        self.ensure_not_interrupted()?;
        let observer = self.take_observer(candidate)?;
        let (observer, evidence) =
            self.wait_observer(candidate, observer, ObserverOperation::PayloadPlaced)?;
        self.restore_observer(candidate, observer)?;
        let ObserverEvidence::PayloadPlaced(observed) = evidence else {
            return Err(ServiceError::EvidenceUnavailable);
        };
        self.ensure_not_interrupted()?;
        self.persist_same(|_| {})?;
        {
            let live = self
                .pipeline
                .as_mut()
                .and_then(|pipeline| pipeline.candidates[index].as_mut())
                .ok_or(ServiceError::EvidenceUnavailable)?;
            send_payload_release(
                &mut live.control,
                observed,
                ContainmentDeadline::control_action(),
            )?;
        }
        let event = self.observe_event(candidate, ObserverOperation::Execution)?;
        if !matches!(
            event,
            ContainmentEvent::LeaderExited {
                category: RunnerExitCategory::ExitedZero,
                ..
            }
        ) {
            return Err(ServiceError::ContainmentUnavailable);
        }
        self.ensure_not_interrupted()?;
        self.persist_same(|_| {})?;
        self.candidate_action(
            candidate,
            CandidateCleanupAction::KillPayload,
            ContainmentDeadline::control_action(),
        )?;
        self.persist_same(|_| {})?;
        self.candidate_action(
            candidate,
            CandidateCleanupAction::ProvePayloadEmpty,
            ContainmentDeadline::control_action(),
        )?;
        let evidence = self.observe_event(candidate, ObserverOperation::OutputDrained)?;
        if !matches!(evidence, ContainmentEvent::OutputDrained { bytes: 0, .. }) {
            return Err(ServiceError::ContainmentUnavailable);
        }
        self.ensure_not_interrupted()?;
        self.persist_same(|_| {})?;
        let mut live = self
            .pipeline
            .as_mut()
            .and_then(|pipeline| pipeline.candidates[index].take())
            .ok_or(ServiceError::EvidenceUnavailable)?;
        send_select_or_abort(
            &mut live.control,
            DecisionKind::Abort,
            ContainmentDeadline::control_action(),
        )?;
        let observer = live
            .observer
            .take()
            .ok_or(ServiceError::EvidenceUnavailable)?;
        let (observer, _) = self.wait_observer(
            candidate,
            observer,
            ObserverOperation::DecisionAck(DecisionKind::Abort),
        )?;
        let (observer, _) = self.wait_observer(candidate, observer, ObserverOperation::SyncEof)?;
        let (observer, _) =
            self.wait_observer(candidate, observer, ObserverOperation::ManagedReaped)?;
        finish_containment(live.control, observer)?;
        self.persist_same(|_| {})?;
        self.candidate_action(
            candidate,
            CandidateCleanupAction::ProveParentEmpty,
            ContainmentDeadline::control_action(),
        )?;
        self.persist_same(|record| {
            record.cgroups[index].lifecycle =
                CgroupLifecycleState::Forward(CgroupForwardState::ProbeEmpty);
        })?;
        Ok(())
    }

    fn arm_payload_descriptors(&mut self) -> Result<(), ServiceError> {
        for candidate in 0_u8..2 {
            let index = usize::from(candidate);
            self.persist_same(|record| {
                record.cgroups[index].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::PayloadFdTransferPending);
            })?;
            {
                let live = self
                    .pipeline
                    .as_mut()
                    .and_then(|pipeline| pipeline.candidates[index].as_mut())
                    .ok_or(ServiceError::EvidenceUnavailable)?;
                transfer_payload_fd_owned(
                    &mut live.control,
                    ContainmentDeadline::control_action(),
                )?;
            }
        }
        for candidate in 0_u8..2 {
            let observer = self.take_observer(candidate)?;
            let (observer, evidence) =
                self.wait_observer(candidate, observer, ObserverOperation::PayloadFdAck)?;
            self.restore_observer(candidate, observer)?;
            if !matches!(
                evidence,
                ObserverEvidence::Event(ContainmentEvent::PayloadFdAck {
                    candidate: observed,
                }) if observed == candidate
            ) {
                return Err(ServiceError::EvidenceUnavailable);
            }
            self.persist_same(|record| {
                record.cgroups[usize::from(candidate)].lifecycle =
                    CgroupLifecycleState::Forward(CgroupForwardState::PayloadArmed);
            })?;
            self.ensure_not_interrupted()?;
        }
        Ok(())
    }

    fn take_observer(&mut self, candidate: u8) -> Result<CandidateObserver, ServiceError> {
        self.pipeline
            .as_mut()
            .and_then(|pipeline| pipeline.candidates[usize::from(candidate)].as_mut())
            .and_then(|candidate| candidate.observer.take())
            .ok_or(ServiceError::EvidenceUnavailable)
    }

    fn restore_observer(
        &mut self,
        candidate: u8,
        observer: CandidateObserver,
    ) -> Result<(), ServiceError> {
        let slot = self
            .pipeline
            .as_mut()
            .and_then(|pipeline| pipeline.candidates[usize::from(candidate)].as_mut())
            .ok_or(ServiceError::EvidenceUnavailable)?;
        if slot.observer.replace(observer).is_some() {
            return Err(ServiceError::EvidenceUnavailable);
        }
        Ok(())
    }

    fn send_go_for(
        &mut self,
        candidate: u8,
    ) -> Result<crate::speculation_linux::GoSendEvidence, ServiceError> {
        let live = self
            .pipeline
            .as_mut()
            .and_then(|pipeline| pipeline.candidates[usize::from(candidate)].as_mut())
            .ok_or(ServiceError::EvidenceUnavailable)?;
        send_go(&mut live.control, ContainmentDeadline::control_action())
            .map_err(ServiceError::from)
    }

    fn observe_go_receipt(&mut self, candidate: u8) -> Result<GoReceiptEvidence, ServiceError> {
        let observer = self.take_observer(candidate)?;
        let (observer, evidence) =
            self.wait_observer(candidate, observer, ObserverOperation::GoReceipt)?;
        self.restore_observer(candidate, observer)?;
        let ObserverEvidence::GoReceipt(receipt) = evidence else {
            return Err(ServiceError::EvidenceUnavailable);
        };
        if receipt.identity.generation
            != self
                .pipeline
                .as_ref()
                .ok_or(ServiceError::EvidenceUnavailable)?
                .authorization_generation
        {
            return Err(ServiceError::StaleGeneration);
        }
        Ok(receipt)
    }

    fn observe_payload_placement(
        &mut self,
        candidate: u8,
    ) -> Result<PayloadPlacementEvidence, ServiceError> {
        let observer = self.take_observer(candidate)?;
        let (observer, evidence) =
            self.wait_observer(candidate, observer, ObserverOperation::PayloadPlaced)?;
        self.restore_observer(candidate, observer)?;
        let ObserverEvidence::PayloadPlaced(placement) = evidence else {
            return Err(ServiceError::EvidenceUnavailable);
        };
        Ok(placement)
    }

    fn observe_event(
        &mut self,
        candidate: u8,
        operation: ObserverOperation,
    ) -> Result<ContainmentEvent, ServiceError> {
        let observer = self.take_observer(candidate)?;
        let (observer, evidence) = self.wait_observer(candidate, observer, operation)?;
        self.restore_observer(candidate, observer)?;
        let ObserverEvidence::Event(event) = evidence else {
            return Err(ServiceError::EvidenceUnavailable);
        };
        Ok(event)
    }

    fn collect_candidate_result(&mut self, candidate: u8) -> Result<(), ServiceError> {
        let index = usize::from(candidate);
        let first = self.observe_event(candidate, ObserverOperation::Execution)?;
        let (mut category, mut elapsed_ns, output_overflowed) = match first {
            ContainmentEvent::LeaderExited {
                candidate: observed,
                category,
                elapsed_ns,
            } if observed == candidate => (category, elapsed_ns, false),
            ContainmentEvent::OutputLimitExceeded {
                candidate: observed,
                bytes,
            } if observed == candidate && bytes == MAX_CANDIDATE_OUTPUT_BYTES + 1 => {
                let live = self
                    .pipeline
                    .as_mut()
                    .and_then(|pipeline| pipeline.candidates[index].as_mut())
                    .ok_or(ServiceError::EvidenceUnavailable)?;
                acknowledge_output_cleanup_claimed(
                    &mut live.control,
                    ContainmentDeadline::control_action(),
                )?;
                (RunnerExitCategory::OutputLimitExceeded, 1, true)
            }
            _ => return Err(ServiceError::EvidenceUnavailable),
        };
        self.persist_same(|record| {
            record.cgroups[index].lifecycle =
                CgroupLifecycleState::Forward(CgroupForwardState::PayloadKillPending);
        })?;
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            perform_candidate_cleanup_action(
                &mut pipeline.topology,
                candidate,
                CandidateCleanupAction::KillPayload,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.persist_same(|_| {})?;
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            perform_candidate_cleanup_action(
                &mut pipeline.topology,
                candidate,
                CandidateCleanupAction::ProvePayloadEmpty,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.persist_same(|record| {
            record.cgroups[index].lifecycle =
                CgroupLifecycleState::Forward(CgroupForwardState::PayloadEmpty);
        })?;
        if output_overflowed {
            let event = self.observe_event(candidate, ObserverOperation::Execution)?;
            let ContainmentEvent::LeaderExited {
                candidate: observed,
                category: observed_category,
                elapsed_ns: observed_elapsed,
            } = event
            else {
                return Err(ServiceError::EvidenceUnavailable);
            };
            if observed != candidate || observed_category != RunnerExitCategory::OutputLimitExceeded
            {
                return Err(ServiceError::EvidenceUnavailable);
            }
            category = observed_category;
            elapsed_ns = observed_elapsed;
        }
        let drained = self.observe_event(candidate, ObserverOperation::OutputDrained)?;
        let ContainmentEvent::OutputDrained {
            candidate: observed,
            bytes: output_bytes,
        } = drained
        else {
            return Err(ServiceError::EvidenceUnavailable);
        };
        if observed != candidate {
            return Err(ServiceError::EvidenceUnavailable);
        }
        let exit_category = public_exit_category(category);
        let exit_success = category == RunnerExitCategory::ExitedZero;
        let eligible = !output_overflowed
            && output_bytes <= MAX_CANDIDATE_OUTPUT_BYTES
            && !matches!(
                category,
                RunnerExitCategory::SpawnFailed
                    | RunnerExitCategory::OutputLimitExceeded
                    | RunnerExitCategory::EvidenceIncomplete
            );
        self.persist_same(|record| {
            let status = &mut record.status.candidates[index];
            status.result_accepted = true;
            status.exit_success = Some(exit_success);
            status.exit_category = Some(exit_category);
            status.elapsed_ns = Some(elapsed_ns);
            status.output_bytes = Some(output_bytes);
            status.eligible = eligible;
        })?;
        Ok(())
    }

    fn finalize_pipeline(&mut self) -> Result<(), ServiceError> {
        let selected = self
            .record
            .status
            .selected_index
            .ok_or(ServiceError::EvidenceUnavailable)?;
        let loser = 1_u8
            .checked_sub(selected)
            .ok_or(ServiceError::EvidenceUnavailable)?;
        self.finish_live_candidate(loser, DecisionKind::Abort)?;
        self.cleanup_candidate(loser)?;
        self.transition(SpeculationPhase::WinnerSelectionPending, None)?;
        self.transition(SpeculationPhase::FinalizingWinner, None)?;
        self.finish_live_candidate(selected, DecisionKind::Select)?;
        self.cleanup_candidate(selected)?;
        self.cleanup_tournament()?;
        self.transition_terminal(SpeculationPhase::Selected)?;
        Ok(())
    }

    fn finish_live_candidate(
        &mut self,
        candidate: u8,
        decision: DecisionKind,
    ) -> Result<(), ServiceError> {
        let index = usize::from(candidate);
        let mut live = self
            .pipeline
            .as_mut()
            .and_then(|pipeline| pipeline.candidates[index].take())
            .ok_or(ServiceError::EvidenceUnavailable)?;
        self.persist_same(|_| {})?;
        send_select_or_abort(
            &mut live.control,
            decision,
            ContainmentDeadline::control_action(),
        )?;
        let observer = live
            .observer
            .take()
            .ok_or(ServiceError::EvidenceUnavailable)?;
        let (observer, evidence) = self.wait_observer(
            candidate,
            observer,
            ObserverOperation::DecisionAck(decision),
        )?;
        if !matches!(
            evidence,
            ObserverEvidence::Event(ContainmentEvent::DecisionAck {
                candidate: observed,
                decision: observed_decision,
            }) if observed == candidate && observed_decision == decision
        ) {
            return Err(ServiceError::EvidenceUnavailable);
        }
        self.persist_same(|record| {
            record.status.candidates[index].cleanup.runner_ack = true;
        })?;
        let (observer, evidence) =
            self.wait_observer(candidate, observer, ObserverOperation::SyncEof)?;
        if !matches!(
            evidence,
            ObserverEvidence::Event(ContainmentEvent::SyncEof {
                candidate: observed,
            }) if observed == candidate
        ) {
            return Err(ServiceError::EvidenceUnavailable);
        }
        self.persist_same(|record| {
            record.status.candidates[index].cleanup.sync_eof = true;
        })?;
        let (observer, evidence) =
            self.wait_observer(candidate, observer, ObserverOperation::ManagedReaped)?;
        if !matches!(
            evidence,
            ObserverEvidence::Event(ContainmentEvent::ManagedReaped {
                candidate: observed,
            }) if observed == candidate
        ) {
            return Err(ServiceError::EvidenceUnavailable);
        }
        self.persist_same(|record| {
            let cleanup = &mut record.status.candidates[index].cleanup;
            cleanup.bwrap_reaped = true;
            cleanup.managed_tombstone = true;
        })?;
        finish_containment(live.control, observer)?;
        Ok(())
    }

    fn cleanup_candidate(&mut self, candidate: u8) -> Result<(), ServiceError> {
        let index = usize::from(candidate);
        let from = match self.record.cgroups[index].lifecycle {
            CgroupLifecycleState::Forward(from) => from,
            CgroupLifecycleState::Removed => return Ok(()),
            _ => return Err(ServiceError::EvidenceUnavailable),
        };
        self.persist_same(|record| {
            record.cgroups[index].lifecycle = CgroupLifecycleState::CleanupPending { from };
        })?;
        self.persist_same(|record| {
            record.cgroups[index].lifecycle = CgroupLifecycleState::ParentKillPending { from };
        })?;
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            perform_candidate_cleanup_action(
                &mut pipeline.topology,
                candidate,
                CandidateCleanupAction::KillParent,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.persist_same(|_| {})?;
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            perform_candidate_cleanup_action(
                &mut pipeline.topology,
                candidate,
                CandidateCleanupAction::ProveParentEmpty,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.persist_same(|record| {
            record.cgroups[index].lifecycle = CgroupLifecycleState::ParentEmpty { from };
            record.status.candidates[index].cleanup.cgroup_empty = true;
        })?;
        self.persist_same(|record| {
            record.cgroups[index].lifecycle = CgroupLifecycleState::PayloadRemovePending { from };
        })?;
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            perform_candidate_cleanup_action(
                &mut pipeline.topology,
                candidate,
                CandidateCleanupAction::RemovePayload,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.persist_same(|record| {
            record.cgroups[index].payload = None;
            record.cgroups[index].lifecycle = CgroupLifecycleState::PayloadRemoved { from };
        })?;
        self.persist_same(|record| {
            record.cgroups[index].lifecycle = CgroupLifecycleState::ControlRemovePending { from };
        })?;
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            perform_candidate_cleanup_action(
                &mut pipeline.topology,
                candidate,
                CandidateCleanupAction::RemoveControl,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.persist_same(|record| {
            record.cgroups[index].control = None;
            record.cgroups[index].lifecycle = CgroupLifecycleState::ControlRemoved { from };
        })?;
        self.persist_same(|record| {
            record.cgroups[index].lifecycle = CgroupLifecycleState::ParentRemovePending { from };
        })?;
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            perform_candidate_cleanup_action(
                &mut pipeline.topology,
                candidate,
                CandidateCleanupAction::RemoveParent,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.persist_same(|record| {
            record.cgroups[index].parent = None;
            record.cgroups[index].lifecycle = CgroupLifecycleState::Removed;
        })?;
        Ok(())
    }

    fn cleanup_tournament(&mut self) -> Result<(), ServiceError> {
        if self.record.tournament_cgroup.lifecycle == TournamentCgroupLifecycleState::Removed {
            return Ok(());
        }
        self.persist_same(|record| {
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::RemovePending;
        })?;
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            perform_tournament_cleanup_action(
                &mut pipeline.topology,
                TournamentCleanupAction::ProveEmpty,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.persist_same(|_| {})?;
        {
            let pipeline = self
                .pipeline
                .as_mut()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            perform_tournament_cleanup_action(
                &mut pipeline.topology,
                TournamentCleanupAction::RemoveDomain,
                ContainmentDeadline::control_action(),
            )?;
        }
        self.persist_same(|record| {
            record.tournament_cgroup.domain = None;
            record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Removed;
        })?;
        Ok(())
    }

    fn transition_terminal(
        &mut self,
        phase: SpeculationPhase,
    ) -> Result<SpeculationStatus, ServiceError> {
        if !matches!(
            phase,
            SpeculationPhase::Selected | SpeculationPhase::RolledBack
        ) || !crate::speculation::is_legal_transition(self.record.status.phase, phase)
        {
            return Err(ServiceError::InvalidTransition);
        }
        let mut next = self.record.clone();
        next.status.generation = increment(self.key.generation())?;
        next.status.phase = phase;
        next.status.rollback_required = false;
        if phase == SpeculationPhase::RolledBack {
            next.status.selected_index = None;
        }
        next.terminal_completed_unix_ms = Some(unix_time_ms()?);
        let stored = self
            .store
            .write(
                &self.key,
                self.key.generation(),
                next,
                TournamentWriteKind::LivePhaseTransition,
            )
            .map_err(map_store_error)?;
        self.install_stored(stored)
    }

    fn fail_closed_pipeline(&mut self) {
        if !self.record.status.phase.is_terminal() && !self.record.status.phase.is_rollback_only() {
            let target = shutdown_claim_phase(self.record.status.phase);
            let _ = self.transition(
                target,
                Some(SpeculationReasonCode::ContainmentEvidenceUnavailable),
            );
        }
        self.rollback_pipeline();
    }

    fn rollback_pipeline(&mut self) {
        if self.record.status.is_terminal() {
            return;
        }
        if self.record.status.phase != SpeculationPhase::RollbackPending
            && self
                .transition(
                    SpeculationPhase::RollbackPending,
                    self.record
                        .status
                        .reason_code
                        .or(Some(SpeculationReasonCode::ContainmentEvidenceUnavailable)),
                )
                .is_err()
        {
            self.core
                .availability
                .store(Availability::Unresolved as u8, Ordering::Release);
            return;
        }
        if self.pipeline.is_none() {
            if self.record.cgroups.iter().any(|candidate| {
                candidate.lifecycle != CgroupLifecycleState::Forward(CgroupForwardState::Planned)
            }) || self.record.tournament_cgroup.lifecycle
                != TournamentCgroupLifecycleState::Planned
            {
                self.core
                    .availability
                    .store(Availability::Unresolved as u8, Ordering::Release);
                return;
            }
            for candidate in 0..2 {
                let _ = self.persist_same(|record| {
                    record.cgroups[candidate].lifecycle = CgroupLifecycleState::Removed;
                    record.status.candidates[candidate].cleanup = fully_closed_cleanup();
                });
            }
            let _ = self.persist_same(|record| {
                record.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Removed;
            });
        } else {
            for candidate in 0_u8..2 {
                if self
                    .pipeline
                    .as_ref()
                    .and_then(|pipeline| pipeline.candidates[usize::from(candidate)].as_ref())
                    .is_some()
                {
                    let _ = self.finish_live_candidate(candidate, DecisionKind::Abort);
                } else {
                    let _ = self.persist_same(|record| {
                        record.status.candidates[usize::from(candidate)]
                            .cleanup
                            .runner_ack = true;
                        record.status.candidates[usize::from(candidate)]
                            .cleanup
                            .sync_eof = true;
                        record.status.candidates[usize::from(candidate)]
                            .cleanup
                            .bwrap_reaped = true;
                        record.status.candidates[usize::from(candidate)]
                            .cleanup
                            .managed_tombstone = true;
                    });
                }
                let _ = self.cleanup_candidate(candidate);
            }
            let _ = self.cleanup_tournament();
        }
        if self.record.is_positive_terminal() {
            return;
        }
        if self
            .record
            .cgroups
            .iter()
            .all(|candidate| candidate.lifecycle == CgroupLifecycleState::Removed)
            && self.record.tournament_cgroup.lifecycle == TournamentCgroupLifecycleState::Removed
            && self.record.status.candidates.iter().all(|candidate| {
                let cleanup = candidate.cleanup;
                cleanup.runner_ack
                    && cleanup.bwrap_reaped
                    && cleanup.sync_eof
                    && cleanup.cgroup_empty
                    && cleanup.managed_tombstone
            })
        {
            let _ = self.transition_terminal(SpeculationPhase::RolledBack);
        } else {
            self.core
                .availability
                .store(Availability::Unresolved as u8, Ordering::Release);
        }
    }

    fn publish_terminal(&mut self) {
        if !self.record.is_positive_terminal() {
            return;
        }
        let terminal = Arc::new(self.record.status.clone());
        let tournament_uuid = self.record.status.tournament_uuid;
        match self.core.index.lock() {
            Ok(mut index) => {
                index.live.remove(&tournament_uuid);
                index.terminal.insert(tournament_uuid, terminal);
            }
            Err(_) => self
                .core
                .availability
                .store(Availability::Unresolved as u8, Ordering::Release),
        }
    }
}

#[cfg(target_os = "linux")]
fn public_exit_category(category: RunnerExitCategory) -> SpeculationExitCategory {
    match category {
        RunnerExitCategory::ExitedZero => SpeculationExitCategory::ExitedZero,
        RunnerExitCategory::ExitedNonzero => SpeculationExitCategory::ExitedNonzero,
        RunnerExitCategory::Signaled => SpeculationExitCategory::Signaled,
        RunnerExitCategory::SpawnFailed => SpeculationExitCategory::SpawnFailed,
        RunnerExitCategory::OutputLimitExceeded => SpeculationExitCategory::OutputLimitExceeded,
        RunnerExitCategory::EvidenceIncomplete => SpeculationExitCategory::EvidenceIncomplete,
    }
}

fn elapsed_stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
        .filter(|value| *value != 0)
        .unwrap_or(1)
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

#[cfg(all(debug_assertions, target_os = "linux"))]
pub(crate) fn run_real_actor_service_driver() -> Result<(), ContainmentErrorCode> {
    use crate::protocol::{SpeculationArgv, SpeculationUnixPath};
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = std::env::var_os("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let source = fixture.join("source");
    let candidates = [fixture.join("candidate-0"), fixture.join("candidate-1")];
    let ledger_path = fixture.join("ledger");
    let control = fixture.join("control");
    for path in [
        &source,
        &candidates[0],
        &candidates[1],
        &ledger_path,
        &control,
    ] {
        std::fs::create_dir(path).map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    for (path, script) in candidates.iter().zip([
        b"#!/bin/sh\nprintf a\n".as_slice(),
        b"#!/bin/sh\nsleep 0.05\nprintf bb\n".as_slice(),
    ]) {
        let script_path = path.join("run.sh");
        std::fs::write(&script_path, script)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o500))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    }
    let cgroup_root = std::env::var_os("LTERM_SPECULATION_CGROUP_ROOT")
        .map(PathBuf::from)
        .ok_or(ContainmentErrorCode::Unsupported)?;
    let store = Arc::new(
        TournamentStore::open_or_create(&fixture.join("store"))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
    );
    let service = SpeculationService::new_reconciling(Uuid::new_v4())
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    service
        .install_ready(store, control)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let request = SpeculationPrepareRequest::new(
        SpeculationUnixPath::from_path(&source)
            .map_err(|_| ContainmentErrorCode::InvalidIdentity)?,
        [
            SpeculationUnixPath::from_path(&candidates[0])
                .map_err(|_| ContainmentErrorCode::InvalidIdentity)?,
            SpeculationUnixPath::from_path(&candidates[1])
                .map_err(|_| ContainmentErrorCode::InvalidIdentity)?,
        ],
        SpeculationUnixPath::from_path(&ledger_path)
            .map_err(|_| ContainmentErrorCode::InvalidIdentity)?,
        SpeculationUnixPath::from_path(&cgroup_root)
            .map_err(|_| ContainmentErrorCode::InvalidIdentity)?,
        60_000,
        SpeculationArgv::from_os_strings(vec![
            OsString::from("/bin/sh"),
            OsString::from("/workspace/run.sh"),
        ])
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?,
    )
    .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    let prepared = service
        .prepare(request)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let ledger = crate::speculation_ledger::ClientLedger::new(
        crate::speculation_fs::open_existing_private_dir(&ledger_path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
    );
    let current = ledger
        .read_verified(
            prepared.status.tournament_uuid,
            prepared.status.daemon_instance_uuid,
        )
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let mut arm_record = current.value.clone();
    arm_record.action = LedgerAction::ArmRequested;
    ledger
        .write_before_action(
            &current,
            &arm_record,
            LedgerAction::ArmRequested,
            prepared.status.generation,
        )
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let armed = service
        .arm(SpeculationArmRequest {
            tournament_uuid: prepared.status.tournament_uuid,
            daemon_instance_uuid: prepared.status.daemon_instance_uuid,
            generation: prepared.status.generation,
        })
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if armed.status.phase != SpeculationPhase::Armed {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let pending = loop {
        if std::time::Instant::now() >= deadline {
            return Err(ContainmentErrorCode::Timeout);
        }
        let status = service
            .status(SpeculationStatusRequest {
                tournament_uuid: armed.status.tournament_uuid,
                daemon_instance_uuid: armed.status.daemon_instance_uuid,
                generation: armed.status.generation,
            })
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            .status;
        if status.phase == SpeculationPhase::PendingFinalize {
            break status;
        }
        if status.phase.is_rollback_only() || status.is_terminal() {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        thread::sleep(Duration::from_millis(10));
    };
    if pending.selected_index != Some(0)
        || pending.candidates.iter().any(|candidate| {
            !candidate.ready || !candidate.go_received || !candidate.result_accepted
        })
    {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    let terminal_phase = match std::env::var_os("LTERM_INTERNAL_SPECULATION_ACTOR_TERMINAL")
        .as_deref()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("finalize")
    {
        "finalize" => {
            let finalizing = service
                .finalize(SpeculationFinalizeRequest {
                    tournament_uuid: pending.tournament_uuid,
                    daemon_instance_uuid: pending.daemon_instance_uuid,
                    generation: pending.generation,
                })
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            if finalizing.status.phase != SpeculationPhase::FinalizingLoser {
                return Err(ContainmentErrorCode::EvidenceUnavailable);
            }
            SpeculationPhase::Selected
        }
        "rollback" => {
            let rolling_back = service
                .rollback(SpeculationRollbackRequest {
                    tournament_uuid: pending.tournament_uuid,
                    daemon_instance_uuid: pending.daemon_instance_uuid,
                    generation: pending.generation,
                })
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            if rolling_back.status.phase != SpeculationPhase::RollbackPending {
                return Err(ContainmentErrorCode::EvidenceUnavailable);
            }
            SpeculationPhase::RolledBack
        }
        "expiry" => {
            service
                .enqueue_watchdog_tick(pending.lease_deadline_unix_ms.saturating_add(1))
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            SpeculationPhase::RolledBack
        }
        "shutdown" => {
            service
                .claim_shutdown(Duration::from_secs(5))
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            SpeculationPhase::RolledBack
        }
        _ => return Err(ContainmentErrorCode::InvalidIdentity),
    };
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(ContainmentErrorCode::Timeout);
        }
        let status = if service.availability() == Availability::ShuttingDown {
            service
                .cached_status(pending.tournament_uuid)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        } else {
            service
                .status(SpeculationStatusRequest {
                    tournament_uuid: pending.tournament_uuid,
                    daemon_instance_uuid: pending.daemon_instance_uuid,
                    generation: pending.generation,
                })
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
                .status
        };
        if status.phase == terminal_phase {
            if status.selected_index
                == if terminal_phase == SpeculationPhase::Selected {
                    Some(0)
                } else {
                    None
                }
                && status.candidates.iter().all(|candidate| {
                    let cleanup = candidate.cleanup;
                    cleanup.runner_ack
                        && cleanup.bwrap_reaped
                        && cleanup.sync_eof
                        && cleanup.cgroup_empty
                        && cleanup.managed_tombstone
                })
            {
                return Ok(());
            }
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        if status.is_terminal()
            || (terminal_phase == SpeculationPhase::Selected && status.phase.is_rollback_only())
        {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FakeEffectStep {
        Durable(&'static str, u64),
        Action(&'static str, u64),
        Observed(&'static str, u64),
    }

    #[cfg(target_os = "linux")]
    struct FakeEffectMachine {
        generation: u64,
        authorized: Option<(&'static str, u64)>,
        trace: Vec<FakeEffectStep>,
    }

    #[cfg(target_os = "linux")]
    impl FakeEffectMachine {
        fn new() -> Self {
            Self {
                generation: 1,
                authorized: None,
                trace: Vec::new(),
            }
        }

        fn persist(&mut self, action: &'static str) -> u64 {
            self.generation += 1;
            self.authorized = Some((action, self.generation));
            self.trace
                .push(FakeEffectStep::Durable(action, self.generation));
            self.generation
        }

        fn act(&mut self, action: &'static str, generation: u64) {
            assert_eq!(self.authorized.take(), Some((action, generation)));
            self.trace.push(FakeEffectStep::Action(action, generation));
        }

        fn observe(&mut self, event: &'static str, generation: u64) {
            assert!(generation <= self.generation, "future observer evidence");
            self.trace.push(FakeEffectStep::Observed(event, generation));
        }
    }

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
    fn deterministic_fake_effect_machine_proves_actor_ordering_contract() {
        let mut machine = FakeEffectMachine::new();
        for action in ["topology", "probe-0", "probe-1", "runner-0", "runner-1"] {
            let generation = machine.persist(action);
            machine.act(action, generation);
            machine.observe(action, generation);
        }
        let go_generation = machine.persist("go-both");
        machine.act("go-both", go_generation);
        machine.observe("go-0", go_generation);
        machine.observe("go-1", go_generation);
        for candidate in ["0", "1"] {
            let placed = if candidate == "0" {
                "placed-0"
            } else {
                "placed-1"
            };
            let release = if candidate == "0" {
                "release-0"
            } else {
                "release-1"
            };
            let result = if candidate == "0" {
                "result-0"
            } else {
                "result-1"
            };
            let generation = machine.persist(placed);
            machine.observe(placed, generation);
            let generation = machine.persist(release);
            machine.act(release, generation);
            let generation = machine.persist(result);
            machine.observe(result, generation);
        }
        for action in [
            "abort-loser-1",
            "cleanup-loser-1",
            "select-winner-0",
            "cleanup-winner-0",
            "cleanup-tournament",
        ] {
            let generation = machine.persist(action);
            machine.act(action, generation);
        }
        let select = machine
            .trace
            .iter()
            .position(|step| matches!(step, FakeEffectStep::Action("select-winner-0", _)))
            .unwrap();
        let loser_cleanup = machine
            .trace
            .iter()
            .position(|step| matches!(step, FakeEffectStep::Action("cleanup-loser-1", _)))
            .unwrap();
        assert!(loser_cleanup < select);
        for (index, step) in machine.trace.iter().enumerate() {
            if let FakeEffectStep::Action(action, generation) = step {
                assert!(machine.trace[..index].iter().any(|prior| {
                    matches!(prior, FakeEffectStep::Durable(observed, observed_generation)
                        if observed == action && observed_generation == generation)
                }));
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[should_panic(expected = "assertion `left == right` failed")]
    fn deterministic_fake_effect_machine_rejects_action_without_exact_authorization() {
        let mut machine = FakeEffectMachine::new();
        let generation = machine.persist("go-both");
        machine.act("release-0", generation);
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

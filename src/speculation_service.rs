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
use crate::speculation::{PENDING_FINALIZE_LEASE, PREPARED_LEASE, READY_LEASE};
#[cfg(all(debug_assertions, target_os = "linux"))]
use crate::speculation_ledger::{ClientLedgerRecord, ClientLedgerSchema};
use crate::speculation_ledger::{ClientRootIdentities, LedgerAction};
#[cfg(target_os = "linux")]
use crate::speculation_linux::{
    CandidateCleanupAction, CandidateControl, CandidateEmptyProof, CandidateObserver,
    ContainmentEvent, GoReceiptEvidence, OldBootRecoveryAction, OldBootRecoveryEvidence,
    PayloadPlacementEvidence, RecoveryAction, RecoveryEvidence, TopologyAction, TopologyEvidence,
    TournamentCleanupAction, TournamentEmptyProof, TournamentTopology,
    acknowledge_output_cleanup_claimed, begin_topology, create_topology, finish_containment,
    go_receipt_skew_ns, launch_fixed_probe, launch_runner, observe_managed_reaped,
    observe_sync_eof, perform_candidate_cleanup_action, perform_tournament_cleanup_action,
    prepare_candidate_empty_proof, prepare_tournament_empty_proof, prove_candidate_empty,
    prove_tournament_empty, receive_decision_ack, receive_execution_event, receive_go_receipt,
    receive_output_drained, receive_payload_fd_ack, receive_payload_placed_owned,
    reconcile_different_boot, reconcile_from_record, reconcile_private_runner_controls, send_go,
    send_payload_release, send_select_or_abort, transfer_payload_fd_owned,
};
use crate::speculation_linux::{
    ContainmentDeadline, ContainmentErrorCode, LiveTournamentContext, PrepareInputs,
    validate_prepare,
};
use crate::speculation_registry::StoredTournamentUpdate;
#[cfg(target_os = "linux")]
use crate::speculation_registry::TournamentRecoveryRecord;
use crate::speculation_registry::{
    CandidateCgroupEvidence, CgroupForwardState, CgroupLifecycleState, TournamentCgroupEvidence,
    TournamentCgroupLifecycleState, TournamentKey, TournamentRecord, TournamentRecordSchema,
    TournamentStore, TournamentWriteKind,
};
#[cfg(target_os = "linux")]
use crate::speculation_registry::{ManagedOwnerEvidence, ManagedOwnerRoleEvidence};
#[cfg(target_os = "linux")]
use crate::speculation_runner::{DecisionKind, RunnerExitCategory};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
#[cfg(target_os = "linux")]
use std::sync::mpsc::TryRecvError;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
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
    #[cfg(target_os = "linux")]
    Proof(Box<ProofCompletion>),
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
enum ProofEvidence {
    Candidate(crate::speculation_linux::CandidateCleanupEvidence),
    Tournament(crate::speculation_linux::TournamentCleanupEvidence),
}

#[cfg(target_os = "linux")]
enum ProofOperation {
    Candidate(CandidateEmptyProof),
    Tournament(TournamentEmptyProof),
}

#[cfg(target_os = "linux")]
struct ProofCompletion {
    authorization_generation: u64,
    result: Result<ProofEvidence, ContainmentErrorCode>,
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
    slot: u16,
    sender: SyncSender<ActorEvent>,
    snapshot: Arc<Mutex<SpeculationStatus>>,
}

#[derive(Default)]
struct ServiceIndex {
    live: HashMap<Uuid, ActorHandle>,
    live_slots: HashMap<u16, Uuid>,
    terminal: HashMap<Uuid, TerminalCacheEntry>,
    terminal_slots: HashMap<u16, Uuid>,
}

struct TerminalCacheEntry {
    slot: u16,
    status: Arc<SpeculationStatus>,
}

impl ServiceIndex {
    fn insert_live(
        &mut self,
        slot: u16,
        tournament_uuid: Uuid,
        handle: ActorHandle,
    ) -> Result<(), ServiceError> {
        if usize::from(slot) >= crate::speculation_registry::MAX_TOURNAMENT_RECORDS {
            return Err(ServiceError::EvidenceUnavailable);
        }
        let replacing_slot = self.live_slots.get(&slot).copied();
        if self.live.len() >= crate::speculation::MAX_LIVE_TOURNAMENTS
            && replacing_slot.is_none()
            && !self.live.contains_key(&tournament_uuid)
        {
            return Err(ServiceError::Capacity);
        }
        self.evict_slot(slot);
        if let Some(previous) = self.live.insert(tournament_uuid, handle) {
            self.live_slots.remove(&previous.slot);
        }
        self.live_slots.insert(slot, tournament_uuid);
        Ok(())
    }

    fn cache_terminal(
        &mut self,
        slot: u16,
        status: Arc<SpeculationStatus>,
    ) -> Result<(), ServiceError> {
        if usize::from(slot) >= crate::speculation_registry::MAX_TOURNAMENT_RECORDS {
            return Err(ServiceError::EvidenceUnavailable);
        }
        let tournament_uuid = status.tournament_uuid;
        self.evict_slot(slot);
        if let Some(previous) = self
            .terminal
            .insert(tournament_uuid, TerminalCacheEntry { slot, status })
        {
            self.terminal_slots.remove(&previous.slot);
        }
        self.terminal_slots.insert(slot, tournament_uuid);
        if self.terminal.len() > crate::speculation_registry::MAX_TOURNAMENT_RECORDS {
            return Err(ServiceError::EvidenceUnavailable);
        }
        Ok(())
    }

    fn publish_actor_terminal(
        &mut self,
        slot: u16,
        tournament_uuid: Uuid,
        status: Arc<SpeculationStatus>,
    ) -> Result<(), ServiceError> {
        if self.live_slots.get(&slot) != Some(&tournament_uuid)
            || !self.live.contains_key(&tournament_uuid)
        {
            return Ok(());
        }
        self.cache_terminal(slot, status)
    }

    fn evict_slot(&mut self, slot: u16) {
        if let Some(previous) = self.live_slots.remove(&slot) {
            self.live.remove(&previous);
        }
        if let Some(previous) = self.terminal_slots.remove(&slot) {
            self.terminal.remove(&previous);
        }
    }
}

#[cfg(all(debug_assertions, target_os = "linux"))]
#[derive(Clone, Copy)]
struct ActorLeaseObservation {
    phase: SpeculationPhase,
    now_unix_ms: u64,
    lease_deadline_unix_ms: u64,
}

struct ServiceCore {
    availability: AtomicU8,
    admission: Mutex<()>,
    index: Mutex<ServiceIndex>,
    store: OnceLock<Arc<TournamentStore>>,
    control_root: OnceLock<PathBuf>,
    daemon_instance_uuid: Uuid,
    watchdog_stop: AtomicBool,
    #[cfg(all(debug_assertions, target_os = "linux"))]
    lease_observer: Mutex<Option<SyncSender<ActorLeaseObservation>>>,
}

#[derive(Clone)]
pub(crate) struct SpeculationService {
    core: Arc<ServiceCore>,
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct ServiceStartupConfig {
    control_path: PathBuf,
    store_path: PathBuf,
    managed_registry_path: PathBuf,
}

#[cfg(target_os = "linux")]
impl ServiceStartupConfig {
    fn capture() -> Result<Self, ServiceError> {
        Ok(Self {
            control_path: crate::paths::speculation_control_dir()
                .map_err(|_| ServiceError::EvidenceUnavailable)?,
            store_path: crate::paths::tournament_registry_dir()
                .map_err(|_| ServiceError::EvidenceUnavailable)?,
            managed_registry_path: crate::paths::process_registry_dir()
                .map_err(|_| ServiceError::EvidenceUnavailable)?,
        })
    }
}

struct PreparedAllocationGuard<'a> {
    service: &'a SpeculationService,
    store: Arc<TournamentStore>,
    key: TournamentKey,
    record: TournamentRecord,
    armed: bool,
}

impl PreparedAllocationGuard<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PreparedAllocationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match close_unowned_prepared(&self.store, self.key, self.record.clone()) {
            Ok(stored) => {
                if self
                    .service
                    .cache_terminal_update(&stored.key, &stored.record.status)
                    .is_err()
                {
                    self.service.mark_unresolved();
                }
            }
            Err(_) => self.service.mark_unresolved(),
        }
    }
}

impl Default for SpeculationService {
    fn default() -> Self {
        Self {
            core: Arc::new(ServiceCore {
                availability: AtomicU8::new(Availability::Disabled as u8),
                admission: Mutex::new(()),
                index: Mutex::new(ServiceIndex::default()),
                store: OnceLock::new(),
                control_root: OnceLock::new(),
                daemon_instance_uuid: Uuid::nil(),
                watchdog_stop: AtomicBool::new(true),
                #[cfg(all(debug_assertions, target_os = "linux"))]
                lease_observer: Mutex::new(None),
            }),
        }
    }
}

impl SpeculationService {
    #[cfg(target_os = "linux")]
    pub(crate) fn production() -> Result<Self, ServiceError> {
        Self::production_with_recovery_gate(None)
    }

    #[cfg(target_os = "linux")]
    fn production_with_recovery_gate(
        recovery_gate: Option<Arc<std::sync::Barrier>>,
    ) -> Result<Self, ServiceError> {
        let startup = ServiceStartupConfig::capture()?;
        Self::production_with_startup_config(startup, recovery_gate)
    }

    #[cfg(target_os = "linux")]
    fn production_with_startup_config(
        startup: ServiceStartupConfig,
        recovery_gate: Option<Arc<std::sync::Barrier>>,
    ) -> Result<Self, ServiceError> {
        crate::launch_registry::initialize_managed_process_registry_path(
            &startup.managed_registry_path,
        )
        .map_err(|_| ServiceError::EvidenceUnavailable)?;
        crate::launch_registry::initialize_managed_reaper_config();
        crate::speculation_linux::initialize_speculation_process_config();
        let service = Self::new_reconciling(Uuid::new_v4())?;
        let worker = service.clone();
        thread::Builder::new()
            .name("lterm-speculation-recovery".into())
            .spawn(move || {
                if let Some(gate) = recovery_gate {
                    gate.wait();
                }
                worker.reconcile_startup(startup);
            })
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        Ok(service)
    }

    #[cfg(all(debug_assertions, target_os = "linux"))]
    pub(crate) fn run_startup_config_capture_test_driver() -> Result<(), ServiceError> {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = std::env::var_os("LTERM_INTERNAL_SPECULATION_FIXTURE_ROOT")
            .map(PathBuf::from)
            .ok_or(ServiceError::InvalidRequest)?;
        let primary_runtime = fixture.join("primary-runtime");
        let primary_data = fixture.join("primary-data");
        let alternate_runtime = fixture.join("alternate-runtime");
        let alternate_data = fixture.join("alternate-data");
        for path in [
            &primary_runtime,
            &primary_data,
            &alternate_runtime,
            &alternate_data,
        ] {
            std::fs::create_dir(path).map_err(|_| ServiceError::EvidenceUnavailable)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| ServiceError::EvidenceUnavailable)?;
        }
        // This debug-only driver is a fresh subprocess with no threads yet.
        unsafe {
            std::env::set_var("LTERM_RUNTIME_DIR", &primary_runtime);
            std::env::set_var("LTERM_DATA_DIR", &primary_data);
        }
        let caller = std::thread::current().id();
        let gate = Arc::new(std::sync::Barrier::new(2));
        let service = Self::production_with_recovery_gate(Some(Arc::clone(&gate)))?;
        if !crate::speculation_linux::speculation_process_config_initialized_on(caller) {
            return Err(ServiceError::EvidenceUnavailable);
        }
        // production_with_recovery_gate synchronously captured every relevant
        // path and managed-reaper seam before spawning the gated worker. No
        // background thread can read the process environment after this point.
        unsafe {
            std::env::set_var("LTERM_RUNTIME_DIR", &alternate_runtime);
            std::env::set_var("LTERM_DATA_DIR", &alternate_data);
        }
        gate.wait();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while service.availability() == Availability::Reconciling
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        if service.availability() != Availability::Ready
            || service.core.control_root.get()
                != Some(&primary_runtime.join("speculation/control-v1"))
            || !primary_data
                .join("speculation/tournament-registry-v1")
                .is_dir()
            || !primary_data
                .join("speculation/process-registry-v1")
                .is_dir()
            || alternate_runtime.join("speculation").exists()
            || alternate_data.join("speculation").exists()
        {
            return Err(ServiceError::EvidenceUnavailable);
        }
        crate::launch_registry::reconcile_managed_processes()
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        if alternate_data.join("speculation").exists() {
            return Err(ServiceError::EvidenceUnavailable);
        }
        service.core.watchdog_stop.store(true, Ordering::Release);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn production() -> Result<Self, ServiceError> {
        Ok(Self::default())
    }

    pub(crate) fn new_reconciling(daemon_instance_uuid: Uuid) -> Result<Self, ServiceError> {
        #[cfg(target_os = "linux")]
        crate::speculation_linux::initialize_speculation_process_config();
        if daemon_instance_uuid.is_nil() {
            return Err(ServiceError::InvalidRequest);
        }
        Ok(Self {
            core: Arc::new(ServiceCore {
                availability: AtomicU8::new(Availability::Reconciling as u8),
                admission: Mutex::new(()),
                index: Mutex::new(ServiceIndex::default()),
                store: OnceLock::new(),
                control_root: OnceLock::new(),
                daemon_instance_uuid,
                watchdog_stop: AtomicBool::new(false),
                #[cfg(all(debug_assertions, target_os = "linux"))]
                lease_observer: Mutex::new(None),
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
    fn reconcile_startup(&self, startup: ServiceStartupConfig) {
        if self.reconcile_startup_inner(&startup).is_err() {
            self.mark_unresolved();
        }
    }

    #[cfg(target_os = "linux")]
    fn reconcile_startup_inner(&self, startup: &ServiceStartupConfig) -> Result<(), ServiceError> {
        let control_root = crate::speculation_fs::open_or_create_private_dir(&startup.control_path)
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let current_private_identity = control_root.identity();
        control_root
            .revalidate()
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let store = Arc::new(
            TournamentStore::open_or_create(&startup.store_path)
                .map_err(|_| ServiceError::EvidenceUnavailable)?,
        );
        let managed =
            crate::launch_registry::reconcile_managed_processes_at(&startup.managed_registry_path)
                .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let mut recovery = store
            .scan_recovery()
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        validate_recovery_records(&recovery)?;
        correlate_managed_startup(&store, &managed, &mut recovery)?;
        let mut terminal = ServiceIndex::default();
        let mut seen = std::collections::HashSet::new();
        for entry in recovery {
            let TournamentRecoveryRecord::Valid { key, record } = entry else {
                return Err(ServiceError::EvidenceUnavailable);
            };
            if !seen.insert(record.status.tournament_uuid) {
                return Err(ServiceError::EvidenceUnavailable);
            }
            if record.is_positive_terminal() {
                terminal.cache_terminal(key.slot(), Arc::new(record.status.clone()))?;
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
            terminal.cache_terminal(normalized.key.slot(), Arc::new(normalized.record.status))?;
        }
        reconcile_private_runner_controls(&control_root, &managed)
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        {
            let mut index = self.index_lock()?;
            index.terminal = terminal.terminal;
            index.terminal_slots = terminal.terminal_slots;
        }
        self.install_ready(store, startup.control_path.clone())?;
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
        let admission_end = std::time::Instant::now()
            .checked_add(crate::speculation::CONTROL_ACK_TIMEOUT)
            .ok_or(ServiceError::EvidenceUnavailable)?;
        let _admission = self.admission_lock_until(admission_end)?;
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
        let unowned_record = record.clone();
        let key = store
            .allocate_prepared(record, now)
            .map_err(|error| self.map_store_error(error))?;
        let mut allocation_guard = PreparedAllocationGuard {
            service: self,
            store: Arc::clone(&store),
            key,
            record: unowned_record,
            armed: true,
        };
        prepare_failpoint("after_prepared_allocation")?;
        let exact = self
            .load_store_record(&store, tournament_uuid)?
            .ok_or(ServiceError::EvidenceUnavailable)?;
        if exact.status != status || exact.status.generation != key.generation() {
            self.mark_unresolved();
            return Err(ServiceError::EvidenceUnavailable);
        }
        prepare_failpoint("after_prepared_readback")?;
        let snapshot = Arc::new(Mutex::new(exact.status.clone()));
        let (sender, receiver) = sync_channel(ACTOR_MAILBOX_CAPACITY);
        {
            let mut index = self.index_lock()?;
            if index.live.contains_key(&tournament_uuid) {
                self.mark_unresolved();
                return Err(ServiceError::Capacity);
            }
            index.insert_live(
                key.slot(),
                tournament_uuid,
                ActorHandle {
                    slot: key.slot(),
                    sender: sender.clone(),
                    snapshot: Arc::clone(&snapshot),
                },
            )?;
        }
        prepare_failpoint("after_prepared_index_insert")?;
        let core = Arc::clone(&self.core);
        let actor_store = Arc::clone(&store);
        let transition = ActorTransitionState::from_prepare(
            Arc::clone(&core),
            actor_store,
            key,
            exact,
            Arc::clone(&snapshot),
            &request,
        );
        thread::Builder::new()
            .name("lterm-speculation-actor".into())
            .spawn(move || {
                actor_loop(ActorState {
                    transition,
                    context,
                    sender: sender.clone(),
                    receiver,
                    #[cfg(target_os = "linux")]
                    pipeline: None,
                    interrupt_requested: false,
                });
            })
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        allocation_guard.disarm();
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
        let end = std::time::Instant::now()
            .checked_add(deadline)
            .ok_or(ServiceError::EvidenceUnavailable)?;
        let admission = self.admission_lock_until(end)?;
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
        drop(admission);
        let mut replies = Vec::with_capacity(senders.len());
        for sender in senders {
            let (reply, receive) = sync_channel(1);
            send_actor_event_until(&sender, ActorEvent::Shutdown { reply }, end)?;
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
        self.request_actor_until(
            tournament_uuid,
            event,
            crate::speculation::CONTROL_ACK_TIMEOUT,
        )
    }

    fn request_actor_until(
        &self,
        tournament_uuid: Uuid,
        event: impl FnOnce(RpcReply) -> ActorEvent,
        timeout: Duration,
    ) -> Result<SpeculationStatus, ServiceError> {
        self.require_ready()?;
        let end = std::time::Instant::now()
            .checked_add(timeout)
            .ok_or(ServiceError::EvidenceUnavailable)?;
        let admission = self.admission_lock_until(end)?;
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
        send_actor_event_until(&sender, event(reply), end)?;
        drop(admission);
        let remaining = end.saturating_duration_since(std::time::Instant::now());
        receive
            .recv_timeout(remaining)
            .map_err(|_| ServiceError::EvidenceUnavailable)?
    }

    fn cached_status(&self, tournament_uuid: Uuid) -> Result<SpeculationStatus, ServiceError> {
        let source = {
            let index = self.index_lock()?;
            if let Some(entry) = index.terminal.get(&tournament_uuid) {
                return Ok((*entry.status).clone());
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
            .map(|entry| (*entry.status).clone()))
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

    fn admission_lock_until(
        &self,
        end: std::time::Instant,
    ) -> Result<MutexGuard<'_, ()>, ServiceError> {
        loop {
            match self.core.admission.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(_)) => {
                    self.mark_unresolved();
                    return Err(ServiceError::EvidenceUnavailable);
                }
                Err(TryLockError::WouldBlock) if std::time::Instant::now() < end => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(ServiceError::EvidenceUnavailable);
                }
            }
        }
    }

    fn map_store_error(&self, error: crate::speculation_fs::EvidenceError) -> ServiceError {
        map_store_error_for_core(&self.core, error)
    }

    fn load_store_record(
        &self,
        store: &TournamentStore,
        tournament_uuid: Uuid,
    ) -> Result<Option<TournamentRecord>, ServiceError> {
        store
            .load_by_uuid(tournament_uuid)
            .map_err(|error| self.map_store_error(error))
    }

    fn cache_terminal_update(
        &self,
        key: &TournamentKey,
        status: &SpeculationStatus,
    ) -> Result<(), ServiceError> {
        let mut index = self.index_lock()?;
        index.cache_terminal(key.slot(), Arc::new(status.clone()))
    }
}

fn send_actor_event_until(
    sender: &SyncSender<ActorEvent>,
    mut event: ActorEvent,
    end: std::time::Instant,
) -> Result<(), ServiceError> {
    loop {
        match sender.try_send(event) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                return Err(ServiceError::EvidenceUnavailable);
            }
            Err(TrySendError::Full(returned)) if std::time::Instant::now() < end => {
                event = returned;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Full(_)) => return Err(ServiceError::EvidenceUnavailable),
        }
    }
}

fn close_unowned_prepared(
    store: &Arc<TournamentStore>,
    key: TournamentKey,
    record: TournamentRecord,
) -> Result<StoredTournamentUpdate, ServiceError> {
    if record.status.phase != SpeculationPhase::Prepared
        || record.status.generation != key.generation()
        || record.status.tournament_uuid != key.tournament_uuid()
    {
        return Err(ServiceError::EvidenceUnavailable);
    }
    let mut next = record;
    next.status.generation = increment(key.generation())?;
    next.status.phase = SpeculationPhase::RollbackPending;
    next.status.rollback_required = true;
    next.status.reason_code = Some(SpeculationReasonCode::ContainmentEvidenceUnavailable);
    next.status.selected_index = None;
    let mut stored = store
        .write(
            &key,
            key.generation(),
            next,
            TournamentWriteKind::LivePhaseTransition,
        )
        .map_err(map_store_error)?;
    for candidate in 0..2 {
        let mut next = stored.record.clone();
        next.status.generation = increment(stored.key.generation())?;
        next.cgroups[candidate].lifecycle = CgroupLifecycleState::Removed;
        next.status.candidates[candidate].cleanup = fully_closed_cleanup();
        stored = store
            .write(
                &stored.key,
                stored.key.generation(),
                next,
                TournamentWriteKind::SamePhaseEvidence,
            )
            .map_err(map_store_error)?;
    }
    let mut next = stored.record.clone();
    next.status.generation = increment(stored.key.generation())?;
    next.tournament_cgroup.lifecycle = TournamentCgroupLifecycleState::Removed;
    stored = store
        .write(
            &stored.key,
            stored.key.generation(),
            next,
            TournamentWriteKind::SamePhaseEvidence,
        )
        .map_err(map_store_error)?;
    let mut next = stored.record.clone();
    next.status.generation = increment(stored.key.generation())?;
    next.status.phase = SpeculationPhase::RolledBack;
    next.status.rollback_required = false;
    next.terminal_completed_unix_ms = Some(unix_time_ms()?);
    store
        .write(
            &stored.key,
            stored.key.generation(),
            next,
            TournamentWriteKind::LivePhaseTransition,
        )
        .map_err(map_store_error)
}

fn prepare_failpoint(_name: &str) -> Result<(), ServiceError> {
    #[cfg(all(debug_assertions, target_os = "linux"))]
    if crate::speculation_linux::active_speculation_test_config()
        .is_some_and(|config| config.prepare_failpoint.as_deref() == Some(_name))
    {
        return Err(ServiceError::EvidenceUnavailable);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_recovery_records(recovery: &[TournamentRecoveryRecord]) -> Result<(), ServiceError> {
    let mut seen = std::collections::HashSet::new();
    for entry in recovery {
        let TournamentRecoveryRecord::Valid { record, .. } = entry else {
            return Err(ServiceError::EvidenceUnavailable);
        };
        if !seen.insert(record.status.tournament_uuid) {
            return Err(ServiceError::EvidenceUnavailable);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn correlate_managed_startup(
    store: &Arc<TournamentStore>,
    managed: &crate::launch_registry::ManagedReconcileReport,
    recovery: &mut [TournamentRecoveryRecord],
) -> Result<(), ServiceError> {
    use crate::launch_registry::{ManagedOwnerRole, ReconcileOutcome};

    let records = recovery
        .iter()
        .enumerate()
        .map(|(index, entry)| match entry {
            TournamentRecoveryRecord::Valid { record, .. } => {
                Ok((record.status.tournament_uuid, index))
            }
            TournamentRecoveryRecord::Corrupt { .. } => Err(ServiceError::EvidenceUnavailable),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    if records.len() != recovery.len() {
        return Err(ServiceError::EvidenceUnavailable);
    }

    let mut seen_keys = std::collections::HashSet::new();
    let mut seen_owners = std::collections::HashSet::new();
    for entry in &managed.entries {
        if !matches!(
            entry.outcome,
            ReconcileOutcome::Absent | ReconcileOutcome::ResolvedTombstone
        ) {
            return Err(ServiceError::EvidenceUnavailable);
        }
        if let Some(key) = entry.key {
            if !seen_keys.insert((key.slot(), key.generation())) {
                return Err(ServiceError::EvidenceUnavailable);
            }
        }
        let Some(owner) = entry.owner.as_ref() else {
            continue;
        };
        owner
            .validate()
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        let key = entry.key.ok_or(ServiceError::EvidenceUnavailable)?;
        let role_key = matches!(owner.role, ManagedOwnerRole::Runner);
        if !seen_owners.insert((owner.tournament_uuid, owner.candidate_index, role_key)) {
            return Err(ServiceError::EvidenceUnavailable);
        }
        let position = *records
            .get(&owner.tournament_uuid)
            .ok_or(ServiceError::EvidenceUnavailable)?;
        let candidate = usize::from(owner.candidate_index);
        let evidence = ManagedOwnerEvidence {
            candidate_index: owner.candidate_index,
            role: match owner.role {
                ManagedOwnerRole::Probe => ManagedOwnerRoleEvidence::Probe,
                ManagedOwnerRole::Runner => ManagedOwnerRoleEvidence::Runner,
            },
            slot: key.slot(),
            generation: key.generation(),
        };
        let TournamentRecoveryRecord::Valid {
            key: tournament_key,
            record,
        } = &mut recovery[position]
        else {
            return Err(ServiceError::EvidenceUnavailable);
        };
        match record.managed_owners[candidate].as_ref() {
            Some(existing) if existing == &evidence => continue,
            Some(existing)
                if existing.candidate_index == evidence.candidate_index
                    && existing.role == ManagedOwnerRoleEvidence::Probe
                    && evidence.role == ManagedOwnerRoleEvidence::Runner => {}
            Some(_) => return Err(ServiceError::EvidenceUnavailable),
            None if record.is_positive_terminal() => {
                return Err(ServiceError::EvidenceUnavailable);
            }
            None => {}
        }
        let mut next = (**record).clone();
        next.status.generation = increment(tournament_key.generation())?;
        next.managed_owners[candidate] = Some(evidence);
        let stored = store
            .write(
                tournament_key,
                tournament_key.generation(),
                next,
                TournamentWriteKind::SamePhaseEvidence,
            )
            .map_err(map_store_error)?;
        *tournament_key = stored.key;
        **record = stored.record;
    }
    Ok(())
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

fn checked_lease_deadline_unix_ms(now_unix_ms: u64, lease: Duration) -> Result<u64, ServiceError> {
    let lease_ms =
        u64::try_from(lease.as_millis()).map_err(|_| ServiceError::GenerationExhausted)?;
    now_unix_ms
        .checked_add(lease_ms)
        .ok_or(ServiceError::GenerationExhausted)
}

fn lease_duration_for_phase(phase: SpeculationPhase, run_timeout: Duration) -> Option<Duration> {
    match phase {
        SpeculationPhase::Ready | SpeculationPhase::GoPending => Some(READY_LEASE),
        SpeculationPhase::Running | SpeculationPhase::ResultPending => Some(run_timeout),
        SpeculationPhase::PendingFinalize
        | SpeculationPhase::FinalizingLoser
        | SpeculationPhase::WinnerSelectionPending
        | SpeculationPhase::FinalizingWinner => Some(PENDING_FINALIZE_LEASE),
        _ => None,
    }
}

struct ActorTransitionState {
    core: Arc<ServiceCore>,
    store: Arc<TournamentStore>,
    key: TournamentKey,
    record: TournamentRecord,
    snapshot: Arc<Mutex<SpeculationStatus>>,
    run_timeout: Duration,
    #[cfg(all(debug_assertions, target_os = "linux"))]
    lease_observer: Option<SyncSender<ActorLeaseObservation>>,
}

impl ActorTransitionState {
    fn from_prepare(
        core: Arc<ServiceCore>,
        store: Arc<TournamentStore>,
        key: TournamentKey,
        record: TournamentRecord,
        snapshot: Arc<Mutex<SpeculationStatus>>,
        request: &SpeculationPrepareRequest,
    ) -> Self {
        #[cfg(all(debug_assertions, target_os = "linux"))]
        let lease_observer = core
            .lease_observer
            .lock()
            .ok()
            .and_then(|observer| observer.clone());
        Self {
            core,
            store,
            key,
            record,
            snapshot,
            run_timeout: Duration::from_millis(request.timeout_ms()),
            #[cfg(all(debug_assertions, target_os = "linux"))]
            lease_observer,
        }
    }

    fn map_store_error(&self, error: crate::speculation_fs::EvidenceError) -> ServiceError {
        map_store_error_for_core(&self.core, error)
    }

    fn claim(
        &mut self,
        tournament_uuid: Uuid,
        daemon_instance_uuid: Uuid,
        generation: u64,
        phase: SpeculationPhase,
        reason: Option<SpeculationReasonCode>,
    ) -> Result<SpeculationStatus, ServiceError> {
        self.claim_with_clock(
            tournament_uuid,
            daemon_instance_uuid,
            generation,
            phase,
            reason,
            unix_time_ms,
        )
    }

    fn claim_at(
        &mut self,
        tournament_uuid: Uuid,
        daemon_instance_uuid: Uuid,
        generation: u64,
        phase: SpeculationPhase,
        reason: Option<SpeculationReasonCode>,
        now_unix_ms: u64,
    ) -> Result<SpeculationStatus, ServiceError> {
        self.claim_with_clock(
            tournament_uuid,
            daemon_instance_uuid,
            generation,
            phase,
            reason,
            || Ok(now_unix_ms),
        )
    }

    fn claim_with_clock(
        &mut self,
        tournament_uuid: Uuid,
        daemon_instance_uuid: Uuid,
        generation: u64,
        phase: SpeculationPhase,
        reason: Option<SpeculationReasonCode>,
        now: impl FnOnce() -> Result<u64, ServiceError>,
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
        #[cfg(all(debug_assertions, target_os = "linux"))]
        let mut lease_observation = None;
        let lease = lease_duration_for_phase(phase, self.run_timeout);
        if let Some(lease) = lease {
            let now_unix_ms = now()?;
            let lease_deadline_unix_ms = checked_lease_deadline_unix_ms(now_unix_ms, lease)?;
            next.status.lease_deadline_unix_ms = lease_deadline_unix_ms;
            #[cfg(all(debug_assertions, target_os = "linux"))]
            if matches!(
                phase,
                SpeculationPhase::Running | SpeculationPhase::ResultPending
            ) {
                lease_observation = Some(ActorLeaseObservation {
                    phase,
                    now_unix_ms,
                    lease_deadline_unix_ms,
                });
            }
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
            .map_err(|error| self.map_store_error(error))?;
        self.key = stored.key;
        self.record = stored.record;
        let status = self.record.status.clone();
        *self.snapshot.lock().map_err(|_| {
            self.core
                .availability
                .store(Availability::Unresolved as u8, Ordering::Release);
            ServiceError::EvidenceUnavailable
        })? = status.clone();
        #[cfg(all(debug_assertions, target_os = "linux"))]
        if let (Some(observer), Some(observation)) = (&self.lease_observer, lease_observation) {
            let _ = observer.try_send(observation);
        }
        Ok(status)
    }
}

struct ActorState {
    transition: ActorTransitionState,
    context: LiveTournamentContext,
    sender: SyncSender<ActorEvent>,
    receiver: Receiver<ActorEvent>,
    #[cfg(target_os = "linux")]
    pipeline: Option<LivePipeline>,
    interrupt_requested: bool,
}

impl std::ops::Deref for ActorState {
    type Target = ActorTransitionState;

    fn deref(&self) -> &Self::Target {
        &self.transition
    }
}

impl std::ops::DerefMut for ActorState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.transition
    }
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
            #[cfg(target_os = "linux")]
            ActorEvent::Proof(_) => {
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
    fn claim(
        &mut self,
        tournament_uuid: Uuid,
        daemon_instance_uuid: Uuid,
        generation: u64,
        phase: SpeculationPhase,
        reason: Option<SpeculationReasonCode>,
    ) -> Result<SpeculationStatus, ServiceError> {
        self.transition.claim(
            tournament_uuid,
            daemon_instance_uuid,
            generation,
            phase,
            reason,
        )
    }

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
        let (roots, _) = self.context.durable_record_evidence()?;
        let expected_roots = ClientRootIdentities {
            source: roots.source,
            candidates: roots.candidates,
            ledger_root: roots.ledger_root,
            cgroup_root: roots.cgroup_root,
        };
        if exact.value.action != LedgerAction::ArmRequested
            || exact.value.generation != request.generation
            || exact.value.roots != expected_roots
        {
            return Err(ServiceError::EvidenceUnavailable);
        }
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
            .map_err(|error| self.map_store_error(error))?;
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

    fn wait_proof(&mut self, operation: ProofOperation) -> Result<ProofEvidence, ServiceError> {
        let authorization_generation = self.pipeline.as_ref().map_or_else(
            || self.context.identity().generation,
            |pipeline| pipeline.authorization_generation,
        );
        let sender = self.sender.clone();
        thread::Builder::new()
            .name("lterm-speculation-proof".into())
            .spawn(move || {
                let deadline = ContainmentDeadline::control_action();
                let result = match operation {
                    ProofOperation::Candidate(proof) => {
                        prove_candidate_empty(proof, deadline).map(ProofEvidence::Candidate)
                    }
                    ProofOperation::Tournament(proof) => {
                        prove_tournament_empty(proof, deadline).map(ProofEvidence::Tournament)
                    }
                };
                let _ = sender.send(ActorEvent::Proof(Box::new(ProofCompletion {
                    authorization_generation,
                    result,
                })));
            })
            .map_err(|_| ServiceError::EvidenceUnavailable)?;
        loop {
            match self
                .receiver
                .recv()
                .map_err(|_| ServiceError::EvidenceUnavailable)?
            {
                ActorEvent::Proof(completion)
                    if completion.authorization_generation == authorization_generation =>
                {
                    return completion.result.map_err(ServiceError::from);
                }
                ActorEvent::Proof(_) | ActorEvent::Observed(_) => {}
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
            ActorEvent::Proof(_) => {}
        }
        Ok(())
    }

    fn drain_pipeline_events(&mut self) -> Result<(), ServiceError> {
        loop {
            match self.receiver.try_recv() {
                Ok(ActorEvent::Observed(_) | ActorEvent::Proof(_)) => {
                    return Err(ServiceError::EvidenceUnavailable);
                }
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

    fn prove_candidate_empty_async(
        &mut self,
        candidate: u8,
        payload: bool,
    ) -> Result<(), ServiceError> {
        let proof = {
            let pipeline = self
                .pipeline
                .as_ref()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            prepare_candidate_empty_proof(&pipeline.topology, candidate, payload)?
        };
        match self.wait_proof(ProofOperation::Candidate(proof))? {
            ProofEvidence::Candidate(
                crate::speculation_linux::CandidateCleanupEvidence::PayloadEmpty {
                    candidate: observed,
                },
            ) if payload && observed == candidate => Ok(()),
            ProofEvidence::Candidate(
                crate::speculation_linux::CandidateCleanupEvidence::ParentEmpty {
                    candidate: observed,
                },
            ) if !payload && observed == candidate => Ok(()),
            _ => Err(ServiceError::EvidenceUnavailable),
        }
    }

    fn prove_tournament_empty_async(&mut self) -> Result<(), ServiceError> {
        let proof = {
            let pipeline = self
                .pipeline
                .as_ref()
                .ok_or(ServiceError::EvidenceUnavailable)?;
            prepare_tournament_empty_proof(&pipeline.topology)?
        };
        match self.wait_proof(ProofOperation::Tournament(proof))? {
            ProofEvidence::Tournament(
                crate::speculation_linux::TournamentCleanupEvidence::Empty,
            ) => Ok(()),
            _ => Err(ServiceError::EvidenceUnavailable),
        }
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
        self.prove_candidate_empty_async(candidate, true)?;
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
        self.prove_candidate_empty_async(candidate, false)?;
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
        self.prove_candidate_empty_async(candidate, true)?;
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
        self.prove_candidate_empty_async(candidate, false)?;
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
        self.prove_tournament_empty_async()?;
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
            .map_err(|error| self.map_store_error(error))?;
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
                if index
                    .publish_actor_terminal(self.key.slot(), tournament_uuid, terminal)
                    .is_err()
                {
                    self.core
                        .availability
                        .store(Availability::Unresolved as u8, Ordering::Release);
                }
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

fn store_error_invalidates_authority(error: &crate::speculation_fs::EvidenceError) -> bool {
    matches!(
        error,
        crate::speculation_fs::EvidenceError::Poisoned
            | crate::speculation_fs::EvidenceError::Corrupt
            | crate::speculation_fs::EvidenceError::Stale
    )
}

fn map_store_error_for_core(
    core: &ServiceCore,
    error: crate::speculation_fs::EvidenceError,
) -> ServiceError {
    if store_error_invalidates_authority(&error) {
        core.availability
            .store(Availability::Unresolved as u8, Ordering::Release);
    }
    map_store_error(error)
}

#[cfg(all(debug_assertions, target_os = "linux"))]
pub(crate) fn run_real_actor_service_driver() -> Result<(), ContainmentErrorCode> {
    use crate::protocol::{SpeculationArgv, SpeculationUnixPath};
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt as _;

    let test_config = crate::speculation_linux::active_speculation_test_config()
        .ok_or(ContainmentErrorCode::InvalidIdentity)?;
    let fixture = test_config
        .fixture_root
        .clone()
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
    let cgroup_root = test_config
        .cgroup_root
        .clone()
        .ok_or(ContainmentErrorCode::Unsupported)?;
    let store = Arc::new(
        TournamentStore::open_or_create(&fixture.join("store"))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
    );
    let service = SpeculationService::new_reconciling(Uuid::new_v4())
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    service
        .install_ready(Arc::clone(&store), control)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if test_config.observed_run_timeout_invalid {
        return Err(ContainmentErrorCode::InvalidIdentity);
    }
    let observed_run_timeout_ms = test_config.observed_run_timeout_ms;
    let lease_receiver = if observed_run_timeout_ms.is_some() {
        let (observer, receiver) = sync_channel(2);
        *service
            .core
            .lease_observer
            .lock()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)? = Some(observer);
        Some(receiver)
    } else {
        None
    };
    let run_timeout_ms = observed_run_timeout_ms.unwrap_or(60_000);
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
        run_timeout_ms,
        SpeculationArgv::from_os_strings(vec![
            OsString::from("/bin/sh"),
            OsString::from("/workspace/run.sh"),
        ])
        .map_err(|_| ContainmentErrorCode::InvalidIdentity)?,
    )
    .map_err(|_| ContainmentErrorCode::InvalidIdentity)?;
    if test_config.prepare_failpoint.is_some() {
        if service.prepare(request).is_ok() {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        let recovery = store
            .scan_recovery()
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        if recovery.len() != 1
            || !matches!(
                &recovery[0],
                TournamentRecoveryRecord::Valid { record, .. }
                    if record.is_positive_terminal()
                        && record.status.phase == SpeculationPhase::RolledBack
            )
            || service.availability() != Availability::Ready
        {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        service
            .claim_shutdown(Duration::from_millis(250))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        return Ok(());
    }
    let prepared = service
        .prepare(request)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if std::fs::read_dir(&ledger_path)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        .next()
        .is_some()
    {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
    if test_config.client_dies_before_ledger {
        let prepared_record = service
            .load_store_record(&store, prepared.status.tournament_uuid)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            .ok_or(ContainmentErrorCode::EvidenceUnavailable)?;
        if prepared_record.status.phase != SpeculationPhase::Prepared
            || prepared_record.tournament_cgroup.lifecycle
                != TournamentCgroupLifecycleState::Planned
            || prepared_record.managed_owners.iter().any(Option::is_some)
            || prepared_record.cgroups.iter().any(|candidate| {
                candidate.lifecycle != CgroupLifecycleState::Forward(CgroupForwardState::Planned)
                    || candidate.parent.is_some()
                    || candidate.control.is_some()
                    || candidate.payload.is_some()
            })
        {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        service
            .enqueue_watchdog_tick(prepared.status.lease_deadline_unix_ms.saturating_add(1))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let closed = service
                .cached_status(prepared.status.tournament_uuid)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            if closed.phase == SpeculationPhase::RolledBack {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(ContainmentErrorCode::Timeout);
            }
            thread::sleep(Duration::from_millis(10));
        }
        if std::fs::read_dir(&ledger_path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
            .next()
            .is_some()
        {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        service
            .claim_shutdown(Duration::from_millis(250))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        return Ok(());
    }
    let ledger = crate::speculation_ledger::ClientLedger::new(
        crate::speculation_fs::open_existing_private_dir(&ledger_path)
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?,
    );
    let prepared_record = service
        .load_store_record(&store, prepared.status.tournament_uuid)
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?
        .ok_or(ContainmentErrorCode::EvidenceUnavailable)?;
    let roots = ClientRootIdentities {
        source: prepared_record.roots.source,
        candidates: prepared_record.roots.candidates,
        ledger_root: prepared_record.roots.ledger_root,
        cgroup_root: prepared_record.roots.cgroup_root,
    };
    let mut roots = roots;
    if test_config.arm_root_mismatch {
        roots.source.dev = roots.source.dev.saturating_add(1);
    }
    let created = ledger
        .create_prepared(&ClientLedgerRecord {
            schema_version: ClientLedgerSchema::V1,
            tournament_uuid: prepared.status.tournament_uuid,
            daemon_instance_uuid: prepared.status.daemon_instance_uuid,
            generation: prepared.status.generation,
            action: LedgerAction::Prepared,
            roots,
            status: prepared.status.clone(),
        })
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    let current = ledger
        .read_verified(
            prepared.status.tournament_uuid,
            prepared.status.daemon_instance_uuid,
        )
        .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
    if current.identity != created.identity
        || current.value != created.value
        || current.value.action != LedgerAction::Prepared
    {
        return Err(ContainmentErrorCode::EvidenceUnavailable);
    }
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
    if test_config.arm_root_mismatch {
        if service
            .arm(SpeculationArmRequest {
                tournament_uuid: prepared.status.tournament_uuid,
                daemon_instance_uuid: prepared.status.daemon_instance_uuid,
                generation: prepared.status.generation,
            })
            .is_ok()
        {
            return Err(ContainmentErrorCode::EvidenceUnavailable);
        }
        service
            .enqueue_watchdog_tick(prepared.status.lease_deadline_unix_ms.saturating_add(1))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let closed = service
                .cached_status(prepared.status.tournament_uuid)
                .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
            if closed.phase == SpeculationPhase::RolledBack {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(ContainmentErrorCode::Timeout);
            }
            thread::sleep(Duration::from_millis(10));
        }
        service
            .claim_shutdown(Duration::from_millis(250))
            .map_err(|_| ContainmentErrorCode::EvidenceUnavailable)?;
        return Ok(());
    }
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
    if let Some(receiver) = lease_receiver {
        for expected_phase in [SpeculationPhase::Running, SpeculationPhase::ResultPending] {
            let observation = receiver
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| ContainmentErrorCode::Timeout)?;
            if observation.phase != expected_phase
                || observation
                    .lease_deadline_unix_ms
                    .checked_sub(observation.now_unix_ms)
                    != Some(run_timeout_ms)
            {
                return Err(ContainmentErrorCode::EvidenceUnavailable);
            }
        }
    }
    let terminal_phase = match test_config.actor_terminal.as_deref().unwrap_or("finalize") {
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

    fn test_prepared_record(tournament_uuid: Uuid, daemon_instance_uuid: Uuid) -> TournamentRecord {
        let identity = crate::speculation_fs::DurableDirectoryIdentity::test_value();
        let candidate_uuids = [Uuid::new_v4(), Uuid::new_v4()];
        TournamentRecord {
            schema_version: TournamentRecordSchema::V1,
            boot_uuid: identity.boot_uuid,
            roots: crate::speculation_registry::PrivateRootIdentities {
                source: identity,
                candidates: [identity, identity],
                ledger_root: identity,
                cgroup_root: identity,
            },
            cgroup_root_locator: crate::speculation_registry::PrivateCgroupRootLocator::test_value(
                identity,
            ),
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
            status: prepared_status(tournament_uuid, daemon_instance_uuid, candidate_uuids, 1, 2),
        }
    }

    fn ready_service() -> SpeculationService {
        let service = SpeculationService::new_reconciling(Uuid::new_v4()).unwrap();
        service
            .core
            .availability
            .store(Availability::Ready as u8, Ordering::Release);
        service
    }

    #[cfg(target_os = "linux")]
    fn prepare_request_with_timeout(timeout_ms: u64) -> SpeculationPrepareRequest {
        use crate::protocol::{SpeculationArgv, SpeculationUnixPath};

        SpeculationPrepareRequest::new(
            SpeculationUnixPath::from_path(std::path::Path::new("/source")).unwrap(),
            [
                SpeculationUnixPath::from_path(std::path::Path::new("/candidate-0")).unwrap(),
                SpeculationUnixPath::from_path(std::path::Path::new("/candidate-1")).unwrap(),
            ],
            SpeculationUnixPath::from_path(std::path::Path::new("/ledger")).unwrap(),
            SpeculationUnixPath::from_path(std::path::Path::new("/cgroup")).unwrap(),
            timeout_ms,
            SpeculationArgv::from_os_strings(vec![std::ffi::OsString::from("/bin/true")]).unwrap(),
        )
        .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn transition_state_for_prepare(
        request: &SpeculationPrepareRequest,
    ) -> (
        tempfile::TempDir,
        Arc<TournamentStore>,
        ActorTransitionState,
    ) {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(TournamentStore::open_or_create(&root.path().join("store")).unwrap());
        let service = SpeculationService::new_reconciling(Uuid::new_v4()).unwrap();
        service
            .install_ready(Arc::clone(&store), root.path().join("control"))
            .unwrap();
        let tournament_uuid = Uuid::new_v4();
        let record = test_prepared_record(tournament_uuid, service.core.daemon_instance_uuid);
        let key = store.allocate_prepared(record, 1).unwrap();
        let record = store.load_by_uuid(tournament_uuid).unwrap().unwrap();
        let snapshot = Arc::new(Mutex::new(record.status.clone()));
        let state = ActorTransitionState::from_prepare(
            Arc::clone(&service.core),
            Arc::clone(&store),
            key,
            record,
            snapshot,
            request,
        );
        (root, store, state)
    }

    #[cfg(target_os = "linux")]
    fn claim_test_phase(
        state: &mut ActorTransitionState,
        phase: SpeculationPhase,
        reason: Option<SpeculationReasonCode>,
        now_unix_ms: u64,
    ) -> SpeculationStatus {
        let current = state.record.status.clone();
        state
            .claim_at(
                current.tournament_uuid,
                current.daemon_instance_uuid,
                current.generation,
                phase,
                reason,
                now_unix_ms,
            )
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn public_prepare_timeouts_flow_through_actor_claims_and_persisted_leases() {
        for timeout_ms in [45_000, 75_000] {
            let request = prepare_request_with_timeout(timeout_ms);
            assert_eq!(request.timeout_ms(), timeout_ms);
            let (_root, store, mut state) = transition_state_for_prepare(&request);

            claim_test_phase(&mut state, SpeculationPhase::Armed, None, 1_000);
            claim_test_phase(&mut state, SpeculationPhase::Starting, None, 2_000);
            claim_test_phase(
                &mut state,
                SpeculationPhase::Ready,
                Some(SpeculationReasonCode::ReadyLease),
                3_000,
            );
            claim_test_phase(&mut state, SpeculationPhase::GoPending, None, 4_000);

            let running = claim_test_phase(
                &mut state,
                SpeculationPhase::Running,
                Some(SpeculationReasonCode::RunningLease),
                5_000,
            );
            assert_eq!(running.lease_deadline_unix_ms, 5_000 + timeout_ms);
            assert_eq!(
                store
                    .load_by_uuid(running.tournament_uuid)
                    .unwrap()
                    .unwrap()
                    .status,
                running
            );
            assert_eq!(*state.snapshot.lock().unwrap(), running);

            let result_pending =
                claim_test_phase(&mut state, SpeculationPhase::ResultPending, None, 6_000);
            assert_eq!(result_pending.lease_deadline_unix_ms, 6_000 + timeout_ms);
            assert_eq!(
                store
                    .load_by_uuid(result_pending.tournament_uuid)
                    .unwrap()
                    .unwrap()
                    .status,
                result_pending
            );
            assert_eq!(*state.snapshot.lock().unwrap(), result_pending);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn actor_claim_reads_clock_only_after_validation_and_for_lease_phases() {
        let request = prepare_request_with_timeout(45_000);
        let (_root, _store, mut state) = transition_state_for_prepare(&request);
        let current = state.record.status.clone();

        assert_eq!(
            state.claim_with_clock(
                Uuid::nil(),
                current.daemon_instance_uuid,
                current.generation,
                SpeculationPhase::Armed,
                None,
                || panic!("invalid identity read the clock"),
            ),
            Err(ServiceError::InvalidRequest)
        );

        let armed = state
            .claim_with_clock(
                current.tournament_uuid,
                current.daemon_instance_uuid,
                current.generation,
                SpeculationPhase::Armed,
                None,
                || panic!("no-lease transition read the clock"),
            )
            .unwrap();
        assert_eq!(armed.phase, SpeculationPhase::Armed);

        state.record.status.phase = SpeculationPhase::RolledBack;
        let terminal = state.record.status.clone();
        assert_eq!(
            state
                .claim_with_clock(
                    terminal.tournament_uuid,
                    terminal.daemon_instance_uuid,
                    terminal.generation,
                    SpeculationPhase::RolledBack,
                    None,
                    || panic!("idempotent terminal claim read the clock"),
                )
                .unwrap(),
            terminal
        );
    }

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
        let tournament_uuid = status.tournament_uuid;
        service
            .core
            .index
            .lock()
            .unwrap()
            .insert_live(
                0,
                tournament_uuid,
                ActorHandle {
                    slot: 0,
                    sender,
                    snapshot: Arc::new(Mutex::new(status)),
                },
            )
            .unwrap();
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
    fn admitted_prepare_is_inserted_before_shutdown_authoritative_snapshot() {
        let service = ready_service();
        let tournament_uuid = Uuid::new_v4();
        let status = prepared_status(
            tournament_uuid,
            service.core.daemon_instance_uuid,
            [Uuid::new_v4(), Uuid::new_v4()],
            1,
            2,
        );
        let (actor_sender, actor_receiver) = sync_channel(ACTOR_MAILBOX_CAPACITY);
        let actor = thread::spawn(move || match actor_receiver.recv().unwrap() {
            ActorEvent::Shutdown { reply } => reply.send(Ok(())).unwrap(),
            _ => panic!("shutdown snapshot sent the wrong actor event"),
        });
        let (held_sender, held_receiver) = sync_channel(1);
        let (release_sender, release_receiver) = sync_channel(1);
        let preparing = {
            let service = service.clone();
            thread::spawn(move || {
                let end = std::time::Instant::now() + Duration::from_secs(2);
                let admission = service.admission_lock_until(end).unwrap();
                held_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                service
                    .core
                    .index
                    .lock()
                    .unwrap()
                    .insert_live(
                        7,
                        tournament_uuid,
                        ActorHandle {
                            slot: 7,
                            sender: actor_sender,
                            snapshot: Arc::new(Mutex::new(status)),
                        },
                    )
                    .unwrap();
                drop(admission);
            })
        };
        held_receiver.recv().unwrap();
        let (done_sender, done_receiver) = sync_channel(1);
        let shutdown = {
            let service = service.clone();
            thread::spawn(move || {
                done_sender
                    .send(service.claim_shutdown(Duration::from_secs(1)))
                    .unwrap();
            })
        };
        assert!(
            done_receiver
                .recv_timeout(Duration::from_millis(25))
                .is_err(),
            "shutdown bypassed an admitted prepare"
        );
        release_sender.send(()).unwrap();
        assert_eq!(
            done_receiver.recv_timeout(Duration::from_secs(1)),
            Ok(Ok(()))
        );
        preparing.join().unwrap();
        shutdown.join().unwrap();
        actor.join().unwrap();
    }

    #[test]
    fn shutdown_admission_timeout_leaves_admitted_prepare_ready_and_owned() {
        let service = ready_service();
        let tournament_uuid = Uuid::new_v4();
        let status = prepared_status(
            tournament_uuid,
            service.core.daemon_instance_uuid,
            [Uuid::new_v4(), Uuid::new_v4()],
            1,
            2,
        );
        let (actor_sender, _actor_receiver) = sync_channel(ACTOR_MAILBOX_CAPACITY);
        let (held_sender, held_receiver) = sync_channel(1);
        let (release_sender, release_receiver) = sync_channel(1);
        let preparing = {
            let service = service.clone();
            thread::spawn(move || {
                let end = std::time::Instant::now() + Duration::from_secs(2);
                let admission = service.admission_lock_until(end).unwrap();
                held_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                service
                    .core
                    .index
                    .lock()
                    .unwrap()
                    .insert_live(
                        7,
                        tournament_uuid,
                        ActorHandle {
                            slot: 7,
                            sender: actor_sender,
                            snapshot: Arc::new(Mutex::new(status)),
                        },
                    )
                    .unwrap();
                drop(admission);
            })
        };
        held_receiver.recv().unwrap();

        let started = std::time::Instant::now();
        assert_eq!(
            service.claim_shutdown(Duration::from_millis(25)),
            Err(ServiceError::EvidenceUnavailable)
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(service.availability(), Availability::Ready);

        release_sender.send(()).unwrap();
        preparing.join().unwrap();
        assert_eq!(service.availability(), Availability::Ready);
        assert!(
            service
                .core
                .index
                .lock()
                .unwrap()
                .live
                .contains_key(&tournament_uuid)
        );
    }

    #[test]
    fn full_actor_mailbox_bounds_mutation_and_shutdown_admission() {
        let service = ready_service();
        let tournament_uuid = Uuid::new_v4();
        let status = prepared_status(
            tournament_uuid,
            service.core.daemon_instance_uuid,
            [Uuid::new_v4(), Uuid::new_v4()],
            1,
            2,
        );
        let (sender, _receiver) = sync_channel(1);
        sender
            .try_send(ActorEvent::WatchdogTick { now_unix_ms: 1 })
            .unwrap();
        service
            .core
            .index
            .lock()
            .unwrap()
            .insert_live(
                0,
                tournament_uuid,
                ActorHandle {
                    slot: 0,
                    sender,
                    snapshot: Arc::new(Mutex::new(status.clone())),
                },
            )
            .unwrap();

        let started = std::time::Instant::now();
        assert_eq!(
            service.request_actor_until(
                tournament_uuid,
                |reply| ActorEvent::Arm {
                    request: SpeculationArmRequest {
                        tournament_uuid,
                        daemon_instance_uuid: status.daemon_instance_uuid,
                        generation: status.generation,
                    },
                    reply,
                },
                Duration::from_millis(25),
            ),
            Err(ServiceError::EvidenceUnavailable)
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(service.availability(), Availability::Ready);

        let started = std::time::Instant::now();
        assert_eq!(
            service.claim_shutdown(Duration::from_millis(25)),
            Err(ServiceError::EvidenceUnavailable)
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(service.availability(), Availability::ShuttingDown);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn post_allocation_owner_closes_every_pre_actor_failure_boundary() {
        for boundary in ["allocation", "readback", "index-insert", "spawn"] {
            let root = tempfile::tempdir().unwrap();
            let store =
                Arc::new(TournamentStore::open_or_create(&root.path().join("store")).unwrap());
            let service = SpeculationService::new_reconciling(Uuid::new_v4()).unwrap();
            service
                .install_ready(Arc::clone(&store), root.path().join("control"))
                .unwrap();
            let tournament_uuid = Uuid::new_v4();
            let record = test_prepared_record(tournament_uuid, service.core.daemon_instance_uuid);
            let key = store.allocate_prepared(record.clone(), 1).unwrap();
            let guard = PreparedAllocationGuard {
                service: &service,
                store: Arc::clone(&store),
                key,
                record,
                armed: true,
            };
            if matches!(boundary, "readback" | "index-insert" | "spawn") {
                assert!(store.load_by_uuid(tournament_uuid).unwrap().is_some());
            }
            if matches!(boundary, "index-insert" | "spawn") {
                let (sender, _receiver) = sync_channel(1);
                let status = store.load_by_uuid(tournament_uuid).unwrap().unwrap().status;
                service
                    .core
                    .index
                    .lock()
                    .unwrap()
                    .insert_live(
                        key.slot(),
                        tournament_uuid,
                        ActorHandle {
                            slot: key.slot(),
                            sender,
                            snapshot: Arc::new(Mutex::new(status)),
                        },
                    )
                    .unwrap();
            }
            drop(guard);
            let closed = store.load_by_uuid(tournament_uuid).unwrap().unwrap();
            assert!(closed.is_positive_terminal(), "unclosed {boundary}");
            assert_eq!(closed.status.phase, SpeculationPhase::RolledBack);
            let index = service.core.index.lock().unwrap();
            assert!(!index.live.contains_key(&tournament_uuid));
            assert!(index.terminal.contains_key(&tournament_uuid));
        }
    }

    #[test]
    fn terminal_cache_tracks_store_slots_and_evicts_recycled_uuid() {
        let mut index = ServiceIndex::default();
        let daemon = Uuid::new_v4();
        let oldest = Uuid::from_u128(1);
        for value in 1_u128..=1025 {
            let status = prepared_status(
                Uuid::from_u128(value),
                daemon,
                [Uuid::new_v4(), Uuid::new_v4()],
                1,
                2,
            );
            index
                .cache_terminal(((value - 1) % 1024) as u16, Arc::new(status))
                .unwrap();
        }
        assert_eq!(index.terminal.len(), 1024);
        assert_eq!(index.terminal_slots.len(), 1024);
        assert!(!index.terminal.contains_key(&oldest));
        assert!(index.terminal.contains_key(&Uuid::from_u128(1025)));
        assert_eq!(index.terminal_slots.get(&0), Some(&Uuid::from_u128(1025)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn poisoned_or_invalid_store_authority_transitions_out_of_ready() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(TournamentStore::open_or_create(&root.path().join("store")).unwrap());
        let service = SpeculationService::new_reconciling(Uuid::new_v4()).unwrap();
        service
            .install_ready(Arc::clone(&store), root.path().join("control"))
            .unwrap();
        store.poison_for_test();
        assert_eq!(
            service.load_store_record(&store, Uuid::new_v4()),
            Err(ServiceError::EvidenceUnavailable)
        );
        assert_eq!(service.availability(), Availability::Unresolved);

        for error in [
            crate::speculation_fs::EvidenceError::Corrupt,
            crate::speculation_fs::EvidenceError::Stale,
        ] {
            let service = ready_service();
            assert_eq!(
                service.map_store_error(error),
                ServiceError::EvidenceUnavailable
            );
            assert_eq!(service.availability(), Availability::Unresolved);
        }
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
            let tournament_uuid = status.tournament_uuid;
            index
                .insert_live(
                    (value - 1) as u16,
                    tournament_uuid,
                    ActorHandle {
                        slot: (value - 1) as u16,
                        sender,
                        snapshot: Arc::new(Mutex::new(status)),
                    },
                )
                .unwrap();
        }
        assert_eq!(index.live.len(), crate::speculation::MAX_LIVE_TOURNAMENTS);
        assert!(index.terminal.is_empty());
    }

    #[cfg(target_os = "linux")]
    fn managed_owner_tag(
        tournament_uuid: Uuid,
        candidate_index: u8,
        role: crate::launch_registry::ManagedOwnerRole,
    ) -> crate::launch_registry::ManagedOwnerTag {
        crate::launch_registry::ManagedOwnerTag {
            kind: crate::launch_registry::ManagedOwnerKind::Speculation,
            tournament_uuid,
            candidate_index,
            role,
        }
    }

    #[cfg(target_os = "linux")]
    fn managed_entry(
        owner: Option<crate::launch_registry::ManagedOwnerTag>,
        slot: u16,
        generation: u64,
        outcome: crate::launch_registry::ReconcileOutcome,
    ) -> crate::launch_registry::ManagedReconcileEntry {
        crate::launch_registry::ManagedReconcileEntry {
            key: Some(crate::launch_registry::ManagedKey::test_value(
                slot, generation,
            )),
            owner,
            artifact_binding: None,
            outcome,
        }
    }

    #[cfg(target_os = "linux")]
    fn correlation_store(
        record: TournamentRecord,
    ) -> (
        tempfile::TempDir,
        Arc<TournamentStore>,
        Vec<TournamentRecoveryRecord>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(TournamentStore::open_or_create(&root.path().join("store")).unwrap());
        store.allocate_prepared(record, 1).unwrap();
        let recovery = store.scan_recovery().unwrap();
        (root, store, recovery)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_owner_correlation_persists_crash_gap_and_rejects_invalid_matrix() {
        use crate::launch_registry::{
            ManagedOwnerRole, ManagedReconcileCode, ManagedReconcileReport, ReconcileOutcome,
        };

        let daemon = Uuid::new_v4();
        let tournament_uuid = Uuid::new_v4();
        let record = test_prepared_record(tournament_uuid, daemon);
        let (_root, store, mut recovery) = correlation_store(record);
        let report = ManagedReconcileReport {
            entries: vec![managed_entry(
                Some(managed_owner_tag(
                    tournament_uuid,
                    0,
                    ManagedOwnerRole::Runner,
                )),
                7,
                9,
                ReconcileOutcome::ResolvedTombstone,
            )],
        };
        correlate_managed_startup(&store, &report, &mut recovery).unwrap();
        assert_eq!(
            store
                .load_by_uuid(tournament_uuid)
                .unwrap()
                .unwrap()
                .managed_owners[0],
            Some(ManagedOwnerEvidence {
                candidate_index: 0,
                role: ManagedOwnerRoleEvidence::Runner,
                slot: 7,
                generation: 9,
            })
        );

        let mut record = test_prepared_record(Uuid::new_v4(), daemon);
        record.managed_owners[0] = Some(ManagedOwnerEvidence {
            candidate_index: 0,
            role: ManagedOwnerRoleEvidence::Probe,
            slot: 1,
            generation: 2,
        });
        let tournament_uuid = record.status.tournament_uuid;
        let (_root, store, mut recovery) = correlation_store(record);
        let runner_crash_gap = ManagedReconcileReport {
            entries: vec![managed_entry(
                Some(managed_owner_tag(
                    tournament_uuid,
                    0,
                    ManagedOwnerRole::Runner,
                )),
                3,
                4,
                ReconcileOutcome::ResolvedTombstone,
            )],
        };
        correlate_managed_startup(&store, &runner_crash_gap, &mut recovery).unwrap();
        assert_eq!(
            store
                .load_by_uuid(tournament_uuid)
                .unwrap()
                .unwrap()
                .managed_owners[0]
                .as_ref()
                .unwrap()
                .role,
            ManagedOwnerRoleEvidence::Runner
        );

        let record = test_prepared_record(Uuid::new_v4(), daemon);
        let (_root, store, mut recovery) = correlation_store(record.clone());
        let owner = managed_owner_tag(record.status.tournament_uuid, 0, ManagedOwnerRole::Probe);
        let duplicate = ManagedReconcileReport {
            entries: vec![
                managed_entry(
                    Some(owner.clone()),
                    1,
                    2,
                    ReconcileOutcome::ResolvedTombstone,
                ),
                managed_entry(Some(owner), 2, 3, ReconcileOutcome::ResolvedTombstone),
            ],
        };
        assert_eq!(
            correlate_managed_startup(&store, &duplicate, &mut recovery),
            Err(ServiceError::EvidenceUnavailable)
        );

        let record = test_prepared_record(Uuid::new_v4(), daemon);
        let (_root, store, mut recovery) = correlation_store(record);
        let orphan = ManagedReconcileReport {
            entries: vec![managed_entry(
                Some(managed_owner_tag(
                    Uuid::new_v4(),
                    0,
                    ManagedOwnerRole::Runner,
                )),
                1,
                2,
                ReconcileOutcome::ResolvedTombstone,
            )],
        };
        assert_eq!(
            correlate_managed_startup(&store, &orphan, &mut recovery),
            Err(ServiceError::EvidenceUnavailable)
        );

        let record = test_prepared_record(Uuid::new_v4(), daemon);
        let tournament_uuid = record.status.tournament_uuid;
        let (_root, store, mut recovery) = correlation_store(record);
        let wrong_candidate = ManagedReconcileReport {
            entries: vec![managed_entry(
                Some(managed_owner_tag(
                    tournament_uuid,
                    2,
                    ManagedOwnerRole::Runner,
                )),
                1,
                2,
                ReconcileOutcome::ResolvedTombstone,
            )],
        };
        assert_eq!(
            correlate_managed_startup(&store, &wrong_candidate, &mut recovery),
            Err(ServiceError::EvidenceUnavailable)
        );

        let mut record = test_prepared_record(Uuid::new_v4(), daemon);
        record.managed_owners[0] = Some(ManagedOwnerEvidence {
            candidate_index: 0,
            role: ManagedOwnerRoleEvidence::Runner,
            slot: 1,
            generation: 2,
        });
        let tournament_uuid = record.status.tournament_uuid;
        let (_root, store, mut recovery) = correlation_store(record);
        let wrong_role = ManagedReconcileReport {
            entries: vec![managed_entry(
                Some(managed_owner_tag(
                    tournament_uuid,
                    0,
                    ManagedOwnerRole::Probe,
                )),
                1,
                2,
                ReconcileOutcome::ResolvedTombstone,
            )],
        };
        assert_eq!(
            correlate_managed_startup(&store, &wrong_role, &mut recovery),
            Err(ServiceError::EvidenceUnavailable)
        );

        let record = test_prepared_record(Uuid::new_v4(), daemon);
        let (_root, store, mut recovery) = correlation_store(record);
        let resolved_ownerless = ManagedReconcileReport {
            entries: vec![managed_entry(
                None,
                1,
                2,
                ReconcileOutcome::ResolvedTombstone,
            )],
        };
        correlate_managed_startup(&store, &resolved_ownerless, &mut recovery).unwrap();
        let unresolved_ownerless = ManagedReconcileReport {
            entries: vec![managed_entry(
                None,
                2,
                3,
                ReconcileOutcome::UnknownOrphanRisk(ManagedReconcileCode::UnknownSlot),
            )],
        };
        assert_eq!(
            correlate_managed_startup(&store, &unresolved_ownerless, &mut recovery),
            Err(ServiceError::EvidenceUnavailable)
        );
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
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let data = root.path().join("data");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::create_dir(&data).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
        let startup = ServiceStartupConfig {
            control_path: runtime.join("speculation/control-v1"),
            store_path: data.join("speculation/tournament-registry-v1"),
            managed_registry_path: data.join("speculation/process-registry-v1"),
        };
        let service = SpeculationService::production_with_startup_config(startup, None).unwrap();
        // Fresh registry genesis durably creates and syncs its bounded slot set.
        // Slow hosted filesystems can legitimately take longer than the service's
        // per-request budget, so this asynchronous-startup test needs its own
        // generous scheduling/I/O bound rather than borrowing that budget.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while service.availability() == Availability::Reconciling
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(service.availability(), Availability::Ready);
        assert!(service.core.store.get().is_some());
        assert!(service.core.control_root.get().is_some());
        service.core.watchdog_stop.store(true, Ordering::Release);
    }
}

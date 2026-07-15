//! Session-finalization authority and bounded publication barrier.
//!
//! This module deliberately contains no PTY byte handling. It owns only the
//! in-memory lifecycle truth published before destructive cleanup and later
//! acts as the acknowledgement boundary for the private exit journal.

use crate::protocol::{
    ExitEvidenceState, ExitListScope, ExitOutcomeState, RecentSessionExit, SessionExitTrigger,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(crate) const PUBLICATION_TIMEOUT: Duration = Duration::from_millis(100);
pub(crate) const JOURNAL_SCHEMA_VERSION: &str = "1.0";
pub(crate) const MAX_EVENT_LINE_BYTES: usize = 16 * 1024;
const MAX_SEGMENT_BYTES: u64 = 1024 * 1024;
const WRITER_QUEUE_CAPACITY: usize = 64;
const CURRENT_SEGMENT: &[u8] = b"current.jsonl\0";
const PREVIOUS_SEGMENT: &[u8] = b"previous.jsonl\0";
const LOCK_FILE: &[u8] = b"journal.lock\0";
const MAX_NAME_BYTES: usize = 128;
const MAX_PANE_BYTES: usize = 64;
const MAX_AGENT_BYTES: usize = 64;
const MAX_SIGNAL_BYTES: usize = 64;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FinalizeTrigger {
    LeaderExited,
    CloseRequested,
    DaemonShutdown,
    ParentCascade { parent_session_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionPresentation {
    Healthy,
    MonitorFailed,
    Ending { trigger: FinalizeTrigger },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WriteOutcome {
    Persisted,
    QueueFull,
    Timeout,
    IoFailed,
    WriterUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClaimAttempt {
    pub event_id: Uuid,
    pub authoritative_trigger: FinalizeTrigger,
    pub claimant: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PublicationResolution {
    pub event_id: Uuid,
    pub trigger: FinalizeTrigger,
    pub outcome: WriteOutcome,
}

#[derive(Debug)]
enum PublicationState {
    Unclaimed(SessionPresentation),
    Claimed {
        event_id: Uuid,
        trigger: FinalizeTrigger,
        deadline: Instant,
    },
    Resolved(PublicationResolution),
}

#[derive(Debug)]
pub(crate) struct LifecyclePublication {
    state: Mutex<PublicationState>,
    changed: Condvar,
    conflicts: AtomicU64,
}

impl Default for LifecyclePublication {
    fn default() -> Self {
        Self {
            state: Mutex::new(PublicationState::Unclaimed(SessionPresentation::Healthy)),
            changed: Condvar::new(),
            conflicts: AtomicU64::new(0),
        }
    }
}

impl LifecyclePublication {
    /// Claims immutable lifecycle authority and clears compatibility alive
    /// while holding the same state lock. The claimant exclusively owns one
    /// event id and one enqueue attempt.
    pub(crate) fn claim(&self, requested: FinalizeTrigger, alive: &AtomicBool) -> ClaimAttempt {
        let mut state = lock(&self.state);
        match &*state {
            PublicationState::Unclaimed(_) => {
                let event_id = Uuid::new_v4();
                let deadline = Instant::now()
                    .checked_add(PUBLICATION_TIMEOUT)
                    .unwrap_or_else(Instant::now);
                *state = PublicationState::Claimed {
                    event_id,
                    trigger: requested.clone(),
                    deadline,
                };
                alive.store(false, Ordering::SeqCst);
                self.changed.notify_all();
                ClaimAttempt {
                    event_id,
                    authoritative_trigger: requested,
                    claimant: true,
                }
            }
            PublicationState::Claimed {
                event_id, trigger, ..
            } => {
                if trigger != &requested {
                    self.conflicts.fetch_add(1, Ordering::Relaxed);
                }
                ClaimAttempt {
                    event_id: *event_id,
                    authoritative_trigger: trigger.clone(),
                    claimant: false,
                }
            }
            PublicationState::Resolved(resolution) => {
                if resolution.trigger != requested {
                    self.conflicts.fetch_add(1, Ordering::Relaxed);
                }
                ClaimAttempt {
                    event_id: resolution.event_id,
                    authoritative_trigger: resolution.trigger.clone(),
                    claimant: false,
                }
            }
        }
    }

    pub(crate) fn mark_monitor_failed(&self) -> bool {
        let mut state = lock(&self.state);
        match &*state {
            PublicationState::Unclaimed(SessionPresentation::Healthy) => {
                *state = PublicationState::Unclaimed(SessionPresentation::MonitorFailed);
                self.changed.notify_all();
                true
            }
            PublicationState::Unclaimed(SessionPresentation::MonitorFailed)
            | PublicationState::Claimed { .. }
            | PublicationState::Resolved(_) => false,
            PublicationState::Unclaimed(SessionPresentation::Ending { .. }) => {
                unreachable!("ending is represented by claimed publication")
            }
        }
    }

    pub(crate) fn presentation(&self) -> SessionPresentation {
        match &*lock(&self.state) {
            PublicationState::Unclaimed(presentation) => presentation.clone(),
            PublicationState::Claimed { trigger, .. } => SessionPresentation::Ending {
                trigger: trigger.clone(),
            },
            PublicationState::Resolved(resolution) => SessionPresentation::Ending {
                trigger: resolution.trigger.clone(),
            },
        }
    }

    pub(crate) fn authoritative_trigger(&self) -> Option<FinalizeTrigger> {
        match &*lock(&self.state) {
            PublicationState::Unclaimed(_) => None,
            PublicationState::Claimed { trigger, .. } => Some(trigger.clone()),
            PublicationState::Resolved(resolution) => Some(resolution.trigger.clone()),
        }
    }

    /// Resolve only the matching in-flight event. Late ACKs cannot overwrite a
    /// timeout, and stale event ids cannot resolve another publication.
    pub(crate) fn resolve(&self, event_id: Uuid, outcome: WriteOutcome) -> bool {
        let mut state = lock(&self.state);
        let PublicationState::Claimed {
            event_id: current_id,
            trigger,
            ..
        } = &*state
        else {
            return false;
        };
        if *current_id != event_id {
            return false;
        }
        let resolution = PublicationResolution {
            event_id,
            trigger: trigger.clone(),
            outcome,
        };
        *state = PublicationState::Resolved(resolution);
        self.changed.notify_all();
        true
    }

    /// Wait only until ACK/failure or the stored absolute 100 ms deadline.
    /// Any waiter may atomically publish timeout and open the cleanup barrier.
    pub(crate) fn wait_for_resolution(&self) -> PublicationResolution {
        let mut state = lock(&self.state);
        loop {
            match &*state {
                PublicationState::Unclaimed(_) => {
                    panic!("publication wait requires a claimed trigger")
                }
                PublicationState::Resolved(resolution) => return resolution.clone(),
                PublicationState::Claimed {
                    event_id,
                    trigger,
                    deadline,
                } => {
                    let now = Instant::now();
                    if now >= *deadline {
                        let resolution = PublicationResolution {
                            event_id: *event_id,
                            trigger: trigger.clone(),
                            outcome: WriteOutcome::Timeout,
                        };
                        *state = PublicationState::Resolved(resolution.clone());
                        self.changed.notify_all();
                        return resolution;
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    let wait_result = self.changed.wait_timeout(state, remaining);
                    let (next, _) = match wait_result {
                        Ok(result) => result,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    state = next;
                }
            }
        }
    }

    pub(crate) fn conflict_count(&self) -> u64 {
        self.conflicts.load(Ordering::Relaxed)
    }
}

/// Converts claimant unwind or early return into a sanitized failure and wakes
/// cleanup contenders. Disarm once enqueue transfers ownership to the writer
/// or synchronously reports an enqueue failure.
pub(crate) struct ClaimResolutionGuard<'a> {
    publication: &'a LifecyclePublication,
    event_id: Uuid,
    armed: bool,
}

impl<'a> ClaimResolutionGuard<'a> {
    pub(crate) fn new(publication: &'a LifecyclePublication, event_id: Uuid) -> Self {
        Self {
            publication,
            event_id,
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ClaimResolutionGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.publication
                .resolve(self.event_id, WriteOutcome::IoFailed);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LifecycleIdentity {
    pub session_id: String,
    pub name: String,
    pub pane_id: String,
    pub parent_session_id: Option<String>,
    pub parent_pane_id: Option<String>,
    pub agent_name: Option<String>,
    pub created_unix_ms: u128,
    pub process_id: Option<u32>,
    pub process_group_id: Option<i32>,
    pub attached_clients: usize,
}

impl LifecycleIdentity {
    fn sanitized(mut self) -> Self {
        self.session_id = sanitized_field(&self.session_id, MAX_PANE_BYTES);
        self.name = sanitized_field(&self.name, MAX_NAME_BYTES);
        self.pane_id = sanitized_field(&self.pane_id, MAX_PANE_BYTES);
        self.parent_session_id = self
            .parent_session_id
            .map(|value| sanitized_field(&value, MAX_PANE_BYTES));
        self.parent_pane_id = self
            .parent_pane_id
            .map(|value| sanitized_field(&value, MAX_PANE_BYTES));
        self.agent_name = self
            .agent_name
            .map(|value| sanitized_field(&value, MAX_AGENT_BYTES));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredIdentity {
    session_id: String,
    name: String,
    pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_name: Option<String>,
    created_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_group_id: Option<i32>,
    attached_clients: usize,
}

impl From<LifecycleIdentity> for StoredIdentity {
    fn from(identity: LifecycleIdentity) -> Self {
        let identity = identity.sanitized();
        Self {
            session_id: identity.session_id,
            name: identity.name,
            pane_id: identity.pane_id,
            parent_session_id: identity.parent_session_id,
            parent_pane_id: identity.parent_pane_id,
            agent_name: identity.agent_name,
            created_unix_ms: identity.created_unix_ms,
            process_id: identity.process_id,
            process_group_id: identity.process_group_id,
            attached_clients: identity.attached_clients,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LifecycleEvent {
    TriggerClaimed {
        schema_version: String,
        event_id: Uuid,
        event_seq: u8,
        #[serde(flatten)]
        identity: StoredIdentity,
        trigger_claimed_unix_ms: u128,
        trigger: SessionExitTrigger,
    },
    LeaderReaped {
        schema_version: String,
        event_id: Uuid,
        event_seq: u8,
        #[serde(flatten)]
        identity: StoredIdentity,
        trigger_claimed_unix_ms: u128,
        reaped_unix_ms: u128,
        trigger: SessionExitTrigger,
        exit_code: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
}

impl LifecycleEvent {
    pub(crate) fn trigger_claimed(
        event_id: Uuid,
        identity: LifecycleIdentity,
        trigger_claimed_unix_ms: u128,
        trigger: SessionExitTrigger,
    ) -> Self {
        Self::TriggerClaimed {
            schema_version: JOURNAL_SCHEMA_VERSION.to_string(),
            event_id,
            event_seq: 1,
            identity: identity.into(),
            trigger_claimed_unix_ms,
            trigger,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn leader_reaped(
        event_id: Uuid,
        identity: LifecycleIdentity,
        trigger_claimed_unix_ms: u128,
        reaped_unix_ms: u128,
        trigger: SessionExitTrigger,
        exit_code: i32,
        signal: Option<String>,
    ) -> Self {
        Self::LeaderReaped {
            schema_version: JOURNAL_SCHEMA_VERSION.to_string(),
            event_id,
            event_seq: 2,
            identity: identity.into(),
            trigger_claimed_unix_ms,
            reaped_unix_ms,
            trigger,
            exit_code,
            signal: signal.map(|value| sanitized_field(&value, MAX_SIGNAL_BYTES)),
        }
    }

    fn session_id(&self) -> &str {
        &self.identity().session_id
    }

    fn event_id(&self) -> Uuid {
        match self {
            Self::TriggerClaimed { event_id, .. } | Self::LeaderReaped { event_id, .. } => {
                *event_id
            }
        }
    }

    fn identity(&self) -> &StoredIdentity {
        match self {
            Self::TriggerClaimed { identity, .. } | Self::LeaderReaped { identity, .. } => identity,
        }
    }

    fn trigger_claimed_at(&self) -> u128 {
        match self {
            Self::TriggerClaimed {
                trigger_claimed_unix_ms,
                ..
            }
            | Self::LeaderReaped {
                trigger_claimed_unix_ms,
                ..
            } => *trigger_claimed_unix_ms,
        }
    }

    fn trigger(&self) -> &SessionExitTrigger {
        match self {
            Self::TriggerClaimed { trigger, .. } | Self::LeaderReaped { trigger, .. } => trigger,
        }
    }

    fn to_json_line(&self) -> Result<Vec<u8>> {
        let mut line = serde_json::to_vec(self).context("serialize lifecycle event")?;
        if line.len().saturating_add(1) > MAX_EVENT_LINE_BYTES {
            bail!("lifecycle event exceeds size cap");
        }
        line.push(b'\n');
        Ok(line)
    }
}

#[derive(Default)]
struct FoldBucket {
    triggers: Vec<LifecycleEvent>,
    reaps: Vec<LifecycleEvent>,
}

pub(crate) fn fold_events(
    events: &[LifecycleEvent],
    storage_degraded: bool,
    target: Option<&str>,
    limit: u16,
    scope: ExitListScope,
) -> Vec<RecentSessionExit> {
    let mut buckets: BTreeMap<String, FoldBucket> = BTreeMap::new();
    let mut seen = HashSet::new();
    for event in events {
        let fingerprint = serde_json::to_string(event).unwrap_or_default();
        if !seen.insert(fingerprint) {
            continue;
        }
        let bucket = buckets.entry(event.session_id().to_string()).or_default();
        match event {
            LifecycleEvent::TriggerClaimed { .. } => bucket.triggers.push(event.clone()),
            LifecycleEvent::LeaderReaped { .. } => bucket.reaps.push(event.clone()),
        }
    }

    let mut exits: Vec<_> = buckets
        .into_values()
        .filter_map(|bucket| fold_bucket(bucket, storage_degraded))
        .filter(|exit| match scope {
            ExitListScope::TopLevel => exit.parent_session_id.is_none(),
            ExitListScope::Children => exit.parent_session_id.is_some(),
            ExitListScope::All => true,
        })
        .filter(|exit| {
            target.is_none_or(|target| {
                exit.session_id == target || exit.name == target || exit.pane_id == target
            })
        })
        .collect();
    exits.sort_by(|left, right| {
        right
            .trigger_claimed_unix_ms
            .cmp(&left.trigger_claimed_unix_ms)
            .then_with(|| right.session_id.cmp(&left.session_id))
    });
    exits.truncate(usize::from(limit));
    exits
}

fn fold_bucket(bucket: FoldBucket, storage_degraded: bool) -> Option<RecentSessionExit> {
    let trigger = bucket.triggers.first();
    let reap = bucket.reaps.first();
    let trigger_conflict = bucket.triggers.len() > 1;
    let reap_conflict = bucket.reaps.len() > 1;
    let cross_conflict = match (trigger, reap) {
        (Some(trigger), Some(reap)) => {
            trigger.event_id() != reap.event_id()
                || trigger.identity() != reap.identity()
                || trigger.trigger_claimed_at() != reap.trigger_claimed_at()
                || trigger.trigger() != reap.trigger()
        }
        _ => false,
    };
    let conflicted = trigger_conflict || reap_conflict || cross_conflict;
    let source = trigger.or(reap)?;
    let identity = source.identity();

    let (public_trigger, evidence_state) = if conflicted {
        (SessionExitTrigger::Unknown, ExitEvidenceState::Conflicted)
    } else if let Some(trigger) = trigger {
        (
            trigger.trigger().clone(),
            if storage_degraded {
                ExitEvidenceState::StorageDegraded
            } else {
                ExitEvidenceState::Complete
            },
        )
    } else {
        (
            reap?.trigger().clone(),
            if storage_degraded {
                ExitEvidenceState::StorageDegraded
            } else {
                ExitEvidenceState::DegradedMissingTriggerEvent
            },
        )
    };

    let (outcome_state, reaped_unix_ms, exit_code, signal) = if reap_conflict || cross_conflict {
        (ExitOutcomeState::Unknown, None, None, None)
    } else if let Some(LifecycleEvent::LeaderReaped {
        reaped_unix_ms,
        exit_code,
        signal,
        ..
    }) = reap
    {
        (
            ExitOutcomeState::Complete,
            Some(*reaped_unix_ms),
            Some(*exit_code),
            signal.clone(),
        )
    } else {
        (ExitOutcomeState::Pending, None, None, None)
    };

    Some(RecentSessionExit {
        schema_version: JOURNAL_SCHEMA_VERSION.to_string(),
        session_id: identity.session_id.clone(),
        name: identity.name.clone(),
        pane_id: identity.pane_id.clone(),
        parent_session_id: identity.parent_session_id.clone(),
        parent_pane_id: identity.parent_pane_id.clone(),
        agent_name: identity.agent_name.clone(),
        created_unix_ms: identity.created_unix_ms,
        trigger_claimed_unix_ms: source.trigger_claimed_at(),
        reaped_unix_ms,
        trigger: public_trigger,
        outcome_state,
        exit_code,
        signal,
        evidence_state,
    })
}

#[derive(Default)]
struct JournalSnapshot {
    events: Vec<LifecycleEvent>,
    degraded: bool,
}

struct WriterRequest {
    event: LifecycleEvent,
    acknowledgement: Option<(Arc<LifecyclePublication>, Uuid)>,
}

/// Handle to the single bounded journal writer. Dropping the handle only
/// disconnects the channel; cleanup never joins the writer thread.
pub(crate) struct LifecycleJournal {
    sender: SyncSender<WriterRequest>,
    snapshot: Arc<RwLock<JournalSnapshot>>,
}

impl LifecycleJournal {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let storage = SecureStorage::open(path)?;
        let initial = storage.load_all()?;
        let snapshot = Arc::new(RwLock::new(JournalSnapshot {
            events: initial.events,
            degraded: initial.degraded,
        }));
        let (sender, receiver) = mpsc::sync_channel::<WriterRequest>(WRITER_QUEUE_CAPACITY);
        let writer_snapshot = Arc::clone(&snapshot);
        std::thread::Builder::new()
            .name("lterm-exit-journal".to_string())
            .spawn(move || writer_loop(storage, receiver, writer_snapshot))
            .context("spawn lifecycle journal writer")?;
        Ok(Self { sender, snapshot })
    }

    /// Exactly one nonblocking enqueue attempt. On failure this method resolves
    /// the publication itself, so the caller may safely disarm its guard.
    pub(crate) fn enqueue_claimed(
        &self,
        event: LifecycleEvent,
        publication: Arc<LifecyclePublication>,
        event_id: Uuid,
    ) {
        let request = WriterRequest {
            event,
            acknowledgement: Some((Arc::clone(&publication), event_id)),
        };
        match self.sender.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                mark_snapshot_degraded(&self.snapshot);
                publication.resolve(event_id, WriteOutcome::QueueFull);
            }
            Err(TrySendError::Disconnected(_)) => {
                mark_snapshot_degraded(&self.snapshot);
                publication.resolve(event_id, WriteOutcome::WriterUnavailable);
            }
        }
    }

    pub(crate) fn enqueue_event(&self, event: LifecycleEvent) -> WriteOutcome {
        let request = WriterRequest {
            event,
            acknowledgement: None,
        };
        match self.sender.try_send(request) {
            Ok(()) => WriteOutcome::Persisted,
            Err(TrySendError::Full(_)) => {
                mark_snapshot_degraded(&self.snapshot);
                WriteOutcome::QueueFull
            }
            Err(TrySendError::Disconnected(_)) => {
                mark_snapshot_degraded(&self.snapshot);
                WriteOutcome::WriterUnavailable
            }
        }
    }

    pub(crate) fn recent_exits(
        &self,
        target: Option<&str>,
        limit: u16,
        scope: ExitListScope,
    ) -> Vec<RecentSessionExit> {
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        fold_events(&snapshot.events, snapshot.degraded, target, limit, scope)
    }
}

fn writer_loop(
    mut storage: SecureStorage,
    receiver: mpsc::Receiver<WriterRequest>,
    snapshot: Arc<RwLock<JournalSnapshot>>,
) {
    while let Ok(request) = receiver.recv() {
        let result = storage.append(&request.event);
        match result {
            Ok(rotated) => {
                let mut current = snapshot
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if rotated {
                    match storage.load_all() {
                        Ok(loaded) => {
                            current.events = loaded.events;
                            current.degraded |= loaded.degraded;
                        }
                        Err(_) => current.degraded = true,
                    }
                } else {
                    current.events.push(request.event);
                }
                drop(current);
                if let Some((publication, event_id)) = request.acknowledgement {
                    publication.resolve(event_id, WriteOutcome::Persisted);
                }
            }
            Err(_) => {
                mark_snapshot_degraded(&snapshot);
                if let Some((publication, event_id)) = request.acknowledgement {
                    publication.resolve(event_id, WriteOutcome::IoFailed);
                }
            }
        }
    }
}

fn mark_snapshot_degraded(snapshot: &RwLock<JournalSnapshot>) {
    snapshot
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .degraded = true;
}

struct LoadedEvents {
    events: Vec<LifecycleEvent>,
    degraded: bool,
}

struct SecureStorage {
    dir: File,
    _lock: File,
    current: File,
    current_bytes: u64,
}

impl SecureStorage {
    fn open(path: &Path) -> Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes()).context("journal path contains NUL")?;
        let dir_fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if dir_fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open lifecycle journal directory {}", path.to_string_lossy()));
        }
        let dir = unsafe { File::from_raw_fd(dir_fd) };
        validate_dir(&dir).context("validate lifecycle journal directory")?;

        let lock_file = open_regular_at(dir.as_raw_fd(), leaf(LOCK_FILE), true, true)?
            .context("open lifecycle journal lock")?;
        let lock_result = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if lock_result != 0 {
            return Err(std::io::Error::last_os_error()).context("lock lifecycle journal");
        }

        let current = open_regular_at(dir.as_raw_fd(), leaf(CURRENT_SEGMENT), true, true)?
            .context("open current lifecycle journal segment")?;
        let current_bytes = current
            .metadata()
            .context("stat current lifecycle journal segment")?
            .len();
        Ok(Self {
            dir,
            _lock: lock_file,
            current,
            current_bytes,
        })
    }

    fn append(&mut self, event: &LifecycleEvent) -> Result<bool> {
        let line = event.to_json_line()?;
        let line_len = u64::try_from(line.len()).context("lifecycle line length overflow")?;
        let rotated = self.current_bytes > 0
            && self.current_bytes.saturating_add(line_len) > MAX_SEGMENT_BYTES;
        if rotated {
            self.rotate()?;
        }
        self.current
            .write_all(&line)
            .context("append lifecycle journal event")?;
        self.current_bytes = self.current_bytes.saturating_add(line_len);
        Ok(rotated)
    }

    fn rotate(&mut self) -> Result<()> {
        if let Some(previous) = open_regular_at(
            self.dir.as_raw_fd(),
            leaf(PREVIOUS_SEGMENT),
            false,
            false,
        )? {
            drop(previous);
            let result = unsafe {
                libc::unlinkat(self.dir.as_raw_fd(), leaf(PREVIOUS_SEGMENT).as_ptr(), 0)
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("remove previous lifecycle journal segment");
            }
        }
        let result = unsafe {
            libc::renameat(
                self.dir.as_raw_fd(),
                leaf(CURRENT_SEGMENT).as_ptr(),
                self.dir.as_raw_fd(),
                leaf(PREVIOUS_SEGMENT).as_ptr(),
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("rotate lifecycle journal segment");
        }
        self.current = open_regular_at(
            self.dir.as_raw_fd(),
            leaf(CURRENT_SEGMENT),
            true,
            true,
        )?
        .context("create current lifecycle journal segment")?;
        self.current_bytes = 0;
        Ok(())
    }

    fn load_all(&self) -> Result<LoadedEvents> {
        let mut loaded = LoadedEvents {
            events: Vec::new(),
            degraded: false,
        };
        for name in [leaf(PREVIOUS_SEGMENT), leaf(CURRENT_SEGMENT)] {
            let Some(file) = open_regular_at(self.dir.as_raw_fd(), name, false, false)? else {
                continue;
            };
            load_segment(file, &mut loaded)?;
        }
        Ok(loaded)
    }
}

fn load_segment(file: File, loaded: &mut LoadedEvents) -> Result<()> {
    let mut reader = BufReader::new(file);
    loop {
        let mut line = Vec::new();
        let bytes = reader
            .read_until(b'\n', &mut line)
            .context("read lifecycle journal segment")?;
        if bytes == 0 {
            break;
        }
        if line.last() != Some(&b'\n') || line.len() > MAX_EVENT_LINE_BYTES {
            loaded.degraded = true;
            continue;
        }
        line.pop();
        match serde_json::from_slice::<LifecycleEvent>(&line) {
            Ok(event) => loaded.events.push(event),
            Err(_) => loaded.degraded = true,
        }
    }
    Ok(())
}

fn leaf(bytes: &'static [u8]) -> &'static CStr {
    CStr::from_bytes_with_nul(bytes).expect("static journal leaf")
}

fn open_regular_at(
    dir_fd: RawFd,
    name: &CStr,
    create: bool,
    writable: bool,
) -> Result<Option<File>> {
    let mut flags = libc::O_NOFOLLOW | libc::O_CLOEXEC;
    flags |= if writable { libc::O_RDWR } else { libc::O_RDONLY };
    if create {
        flags |= libc::O_CREAT | libc::O_APPEND;
    }
    let fd = unsafe { libc::openat(dir_fd, name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if !create && error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("open journal leaf {}", name.to_string_lossy()));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    validate_regular(&file, name)?;
    Ok(Some(file))
}

fn validate_dir(dir: &File) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(dir.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("fstat journal directory");
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        bail!("lifecycle journal root is not a directory");
    }
    validate_owner_mode(stat.st_uid, stat.st_mode, "lifecycle journal root")
}

fn validate_regular(file: &File, name: &CStr) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("fstat journal leaf");
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        bail!("journal leaf {} is not a regular file", name.to_string_lossy());
    }
    if stat.st_nlink != 1 {
        bail!(
            "journal leaf {} has unexpected link count {}",
            name.to_string_lossy(),
            stat.st_nlink
        );
    }
    validate_owner_mode(stat.st_uid, stat.st_mode, &name.to_string_lossy())
}

fn validate_owner_mode(uid: libc::uid_t, mode: libc::mode_t, label: &str) -> Result<()> {
    let expected = unsafe { libc::geteuid() };
    if uid != expected {
        bail!("{label} is owned by uid {uid}, expected uid {expected}");
    }
    if mode & 0o077 != 0 {
        bail!("{label} is not owner-private (mode {:03o})", mode & 0o777);
    }
    Ok(())
}

fn sanitized_field(value: &str, max_bytes: usize) -> String {
    let sanitized = crate::sanitize::terminal_text(value);
    if sanitized.len() <= max_bytes {
        return sanitized;
    }
    let mut end = max_bytes;
    while end > 0 && !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn first_trigger_is_authoritative_and_immediately_marks_ending() {
        let publication = LifecyclePublication::default();
        let alive = AtomicBool::new(true);

        let first = publication.claim(FinalizeTrigger::CloseRequested, &alive);
        let second = publication.claim(FinalizeTrigger::DaemonShutdown, &alive);

        assert!(first.claimant);
        assert!(!second.claimant);
        assert_eq!(first.event_id, second.event_id);
        assert_eq!(
            second.authoritative_trigger,
            FinalizeTrigger::CloseRequested
        );
        assert!(!alive.load(Ordering::SeqCst));
        assert_eq!(
            publication.presentation(),
            SessionPresentation::Ending {
                trigger: FinalizeTrigger::CloseRequested
            }
        );
        assert_eq!(publication.conflict_count(), 1);
    }

    #[test]
    fn any_waiter_opens_barrier_at_fixed_deadline_and_late_ack_is_ignored() {
        let publication = Arc::new(LifecyclePublication::default());
        let alive = AtomicBool::new(true);
        let claim = publication.claim(FinalizeTrigger::LeaderExited, &alive);
        let publication_for_waiter = Arc::clone(&publication);

        let started = Instant::now();
        let waiter = thread::spawn(move || publication_for_waiter.wait_for_resolution());
        let resolution = waiter.join().expect("publication waiter");
        let elapsed = started.elapsed();

        assert_eq!(resolution.outcome, WriteOutcome::Timeout);
        assert!(elapsed >= PUBLICATION_TIMEOUT);
        assert!(elapsed < Duration::from_millis(500));
        assert!(!publication.resolve(claim.event_id, WriteOutcome::Persisted));
        assert_eq!(
            publication.wait_for_resolution().outcome,
            WriteOutcome::Timeout
        );
    }

    #[test]
    fn guard_resolves_early_return_and_monitor_failure_is_non_destructive() {
        let publication = LifecyclePublication::default();
        let alive = AtomicBool::new(true);
        assert!(publication.mark_monitor_failed());
        assert_eq!(
            publication.presentation(),
            SessionPresentation::MonitorFailed
        );
        assert!(alive.load(Ordering::SeqCst));

        let claim = publication.claim(FinalizeTrigger::CloseRequested, &alive);
        {
            let _guard = ClaimResolutionGuard::new(&publication, claim.event_id);
        }
        assert_eq!(
            publication.wait_for_resolution().outcome,
            WriteOutcome::IoFailed
        );
        assert!(!alive.load(Ordering::SeqCst));
    }
}

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
use std::io::{BufRead, BufReader, Read, Write};
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
const OVERSIZED_SEGMENT_SENTINEL: &[u8] = b"oversized segment discarded\n";
const MAX_NAME_BYTES: usize = 128;
const MAX_PANE_BYTES: usize = 64;
const MAX_AGENT_BYTES: usize = 64;
const MAX_SIGNAL_BYTES: usize = 64;
#[cfg(debug_assertions)]
const INTERNAL_TEST_MODE_ENV: &str = "LTERM_INTERNAL_TEST_MODE";
#[cfg(debug_assertions)]
const INTERNAL_TEST_WRITER_ENTERED_ENV: &str = "LTERM_INTERNAL_TEST_LIFECYCLE_WRITER_ENTERED";
#[cfg(debug_assertions)]
const INTERNAL_TEST_WRITER_RELEASE_ENV: &str = "LTERM_INTERNAL_TEST_LIFECYCLE_WRITER_RELEASE";

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

/// Holds lifecycle state stable while an admitted identity mutation commits.
///
/// Identity mutations acquire this only after taking the session-map and
/// metadata locks. Trigger claimants acquire only lifecycle state, release it,
/// and then snapshot metadata, so this guard preserves the established lock
/// order while closing the admissibility-to-commit race.
pub(crate) struct IdentityCommitGuard<'a> {
    _state: MutexGuard<'a, PublicationState>,
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
    pub(crate) fn identity_commit_guard(&self) -> Option<IdentityCommitGuard<'_>> {
        let state = lock(&self.state);
        matches!(&*state, PublicationState::Unclaimed(_))
            .then_some(IdentityCommitGuard { _state: state })
    }

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

    #[cfg(test)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct StoredIdentity {
    session_id: String,
    name: String,
    pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_name: Option<String>,
    created_unix_ms: u64,
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
            created_unix_ms: u64::try_from(identity.created_unix_ms).unwrap_or(u64::MAX),
            process_id: identity.process_id,
            process_group_id: identity.process_group_id,
            attached_clients: identity.attached_clients,
        }
    }
}

impl StoredIdentity {
    fn sanitize_loaded(&mut self) -> bool {
        let before = self.clone();
        self.session_id = sanitized_field(&self.session_id, MAX_PANE_BYTES);
        self.name = sanitized_field(&self.name, MAX_NAME_BYTES);
        self.pane_id = sanitized_field(&self.pane_id, MAX_PANE_BYTES);
        self.parent_session_id = self
            .parent_session_id
            .take()
            .map(|value| sanitized_field(&value, MAX_PANE_BYTES));
        self.parent_pane_id = self
            .parent_pane_id
            .take()
            .map(|value| sanitized_field(&value, MAX_PANE_BYTES));
        self.agent_name = self
            .agent_name
            .take()
            .map(|value| sanitized_field(&value, MAX_AGENT_BYTES));
        *self != before
    }

    fn has_valid_shape(&self) -> bool {
        Uuid::parse_str(&self.session_id).is_ok()
            && !self.name.is_empty()
            && self.pane_id.starts_with('%')
            && self
                .parent_session_id
                .as_deref()
                .is_none_or(|id| Uuid::parse_str(id).is_ok())
            && self
                .parent_pane_id
                .as_deref()
                .is_none_or(|pane| pane.starts_with('%'))
            && self
                .agent_name
                .as_deref()
                .is_none_or(|name| !name.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LifecycleEvent {
    TriggerClaimed {
        schema_version: String,
        event_id: Uuid,
        event_seq: u8,
        #[serde(flatten)]
        identity: StoredIdentity,
        trigger_claimed_unix_ms: u64,
        trigger: SessionExitTrigger,
    },
    LeaderReaped {
        schema_version: String,
        event_id: Uuid,
        event_seq: u8,
        #[serde(flatten)]
        identity: StoredIdentity,
        trigger_claimed_unix_ms: u64,
        reaped_unix_ms: u64,
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
            trigger_claimed_unix_ms: u64::try_from(trigger_claimed_unix_ms).unwrap_or(u64::MAX),
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
            trigger_claimed_unix_ms: u64::try_from(trigger_claimed_unix_ms).unwrap_or(u64::MAX),
            reaped_unix_ms: u64::try_from(reaped_unix_ms).unwrap_or(u64::MAX),
            trigger,
            exit_code,
            signal: signal.map(|value| sanitize_exit_signal(&value)),
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
            } => u128::from(*trigger_claimed_unix_ms),
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

    fn validate_and_sanitize_loaded(mut self) -> Option<(Self, bool)> {
        let changed = match &mut self {
            Self::TriggerClaimed {
                schema_version,
                event_seq,
                identity,
                trigger,
                ..
            } => {
                if schema_version != JOURNAL_SCHEMA_VERSION || *event_seq != 1 {
                    return None;
                }
                identity.sanitize_loaded() | sanitize_loaded_trigger(trigger)?
            }
            Self::LeaderReaped {
                schema_version,
                event_seq,
                identity,
                trigger,
                signal,
                ..
            } => {
                if schema_version != JOURNAL_SCHEMA_VERSION || *event_seq != 2 {
                    return None;
                }
                let mut changed = identity.sanitize_loaded() | sanitize_loaded_trigger(trigger)?;
                if let Some(value) = signal {
                    let sanitized = sanitized_field(value, MAX_SIGNAL_BYTES);
                    changed |= *value != sanitized;
                    *value = sanitized;
                }
                changed
            }
        };
        if !self.identity().has_valid_shape() {
            return None;
        }
        Some((self, changed))
    }
}

fn sanitize_loaded_trigger(trigger: &mut SessionExitTrigger) -> Option<bool> {
    match trigger {
        SessionExitTrigger::LeaderExited
        | SessionExitTrigger::CloseRequested
        | SessionExitTrigger::DaemonShutdown => Some(false),
        SessionExitTrigger::ParentCascade { parent_session_id } => {
            let sanitized = sanitized_field(parent_session_id, MAX_PANE_BYTES);
            let changed = *parent_session_id != sanitized;
            *parent_session_id = sanitized;
            Uuid::parse_str(parent_session_id).ok().map(|_| changed)
        }
        SessionExitTrigger::Unknown => None,
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
        if !seen.insert(event) {
            continue;
        }
        let bucket = buckets.entry(event.session_id().to_string()).or_default();
        match event {
            LifecycleEvent::TriggerClaimed { .. } => bucket.triggers.push(event.clone()),
            LifecycleEvent::LeaderReaped { .. } => bucket.reaps.push(event.clone()),
        }
    }
    let mut exits: Vec<_> = buckets
        .into_iter()
        .filter_map(|(session_id, bucket)| fold_bucket(&session_id, bucket, storage_degraded))
        .collect();
    exits.sort_by(|left, right| {
        right
            .trigger_claimed_unix_ms
            .cmp(&left.trigger_claimed_unix_ms)
            .then_with(|| right.session_id.cmp(&left.session_id))
    });

    if let Some(target) = target
        && let Some(exact) = exits.iter().find(|exit| exit.session_id == target).cloned()
    {
        exits = if exit_matches_scope(&exact, scope) {
            vec![exact]
        } else {
            Vec::new()
        };
        exits.truncate(usize::from(limit));
        return exits;
    }

    exits.retain(|exit| exit_matches_scope(exit, scope));
    if let Some(target) = target {
        exits.retain(|exit| exit.name == target || exit.pane_id == target);
        exits.truncate(1);
    }
    exits.truncate(usize::from(limit));
    exits
}

fn exit_matches_scope(exit: &RecentSessionExit, scope: ExitListScope) -> bool {
    match scope {
        // A conflicted DTO intentionally contains no candidate identity. Do
        // not reinterpret its fail-closed `None` parent as top-level truth.
        ExitListScope::TopLevel => {
            exit.evidence_state != ExitEvidenceState::Conflicted && exit.parent_session_id.is_none()
        }
        ExitListScope::Children => {
            exit.evidence_state != ExitEvidenceState::Conflicted && exit.parent_session_id.is_some()
        }
        ExitListScope::All => true,
    }
}

fn fold_bucket(
    session_id: &str,
    bucket: FoldBucket,
    storage_degraded: bool,
) -> Option<RecentSessionExit> {
    let trigger = bucket.triggers.first();
    let reap = bucket.reaps.first();
    let trigger_conflict = bucket.triggers.len() > 1;
    let reap_conflict = bucket.reaps.len() > 1;
    let cross_conflict = match (trigger, reap) {
        (Some(trigger), Some(reap)) => {
            trigger.event_id() != reap.event_id()
                || !stable_identity_matches(trigger.identity(), reap.identity())
                || trigger.trigger_claimed_at() != reap.trigger_claimed_at()
                || trigger.trigger() != reap.trigger()
        }
        _ => false,
    };
    let conflicted = trigger_conflict || reap_conflict || cross_conflict;
    let source = trigger.or(reap)?;
    if conflicted {
        return Some(RecentSessionExit {
            schema_version: JOURNAL_SCHEMA_VERSION.to_string(),
            session_id: session_id.to_string(),
            name: String::new(),
            pane_id: String::new(),
            parent_session_id: None,
            parent_pane_id: None,
            agent_name: None,
            created_unix_ms: 0,
            trigger_claimed_unix_ms: 0,
            reaped_unix_ms: None,
            trigger: SessionExitTrigger::Unknown,
            outcome_state: ExitOutcomeState::Unknown,
            exit_code: None,
            signal: None,
            evidence_state: ExitEvidenceState::Conflicted,
        });
    }
    let identity = source.identity();

    let (public_trigger, evidence_state) = if let Some(trigger) = trigger {
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

    let (outcome_state, reaped_unix_ms, exit_code, signal) =
        if let Some(LifecycleEvent::LeaderReaped {
            reaped_unix_ms,
            exit_code,
            signal,
            ..
        }) = reap
        {
            (
                ExitOutcomeState::Complete,
                Some(u128::from(*reaped_unix_ms)),
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
        created_unix_ms: u128::from(identity.created_unix_ms),
        trigger_claimed_unix_ms: source.trigger_claimed_at(),
        reaped_unix_ms,
        trigger: public_trigger,
        outcome_state,
        exit_code,
        signal,
        evidence_state,
    })
}

fn stable_identity_matches(left: &StoredIdentity, right: &StoredIdentity) -> bool {
    left.session_id == right.session_id
        && left.name == right.name
        && left.pane_id == right.pane_id
        && left.parent_session_id == right.parent_session_id
        && left.parent_pane_id == right.parent_pane_id
        && left.agent_name == right.agent_name
        && left.created_unix_ms == right.created_unix_ms
        && left.process_id == right.process_id
        && left.process_group_id == right.process_group_id
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
        internal_test_block_lifecycle_writer();
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

#[cfg(debug_assertions)]
fn internal_test_block_lifecycle_writer() {
    if !super::env_bool(INTERNAL_TEST_MODE_ENV) {
        return;
    }
    let Some(entered) = std::env::var_os(INTERNAL_TEST_WRITER_ENTERED_ENV) else {
        return;
    };
    let Some(release) = std::env::var_os(INTERNAL_TEST_WRITER_RELEASE_ENV) else {
        return;
    };
    if std::fs::write(&entered, b"entered\n").is_err() {
        return;
    }
    while !Path::new(&release).exists() {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(not(debug_assertions))]
fn internal_test_block_lifecycle_writer() {}

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
        let path =
            CString::new(path.as_os_str().as_bytes()).context("journal path contains NUL")?;
        let dir_fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if dir_fd < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "open lifecycle journal directory {}",
                    path.to_string_lossy()
                )
            });
        }
        let dir = unsafe { File::from_raw_fd(dir_fd) };
        validate_dir(&dir).context("validate lifecycle journal directory")?;

        let lock_file = open_regular_at(dir.as_raw_fd(), leaf(LOCK_FILE), true, true)?
            .context("open lifecycle journal lock")?;
        let lock_result =
            unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if lock_result != 0 {
            return Err(std::io::Error::last_os_error()).context("lock lifecycle journal");
        }

        let mut current = open_regular_at(dir.as_raw_fd(), leaf(CURRENT_SEGMENT), true, true)?
            .context("open current lifecycle journal segment")?;
        let mut previous = open_regular_at(dir.as_raw_fd(), leaf(PREVIOUS_SEGMENT), false, true)?;
        repair_oversized_segment(&mut current)?;
        if let Some(previous) = previous.as_mut() {
            repair_oversized_segment(previous)?;
        }
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
        let repaired = self.repair_oversized_segments()?;
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
        Ok(repaired || rotated)
    }

    fn rotate(&mut self) -> Result<()> {
        if let Some(mut previous) =
            open_regular_at(self.dir.as_raw_fd(), leaf(PREVIOUS_SEGMENT), false, true)?
        {
            repair_oversized_segment(&mut previous)?;
            drop(previous);
        }
        repair_oversized_segment(&mut self.current)?;
        self.current_bytes = self
            .current
            .metadata()
            .context("stat current lifecycle journal segment before rotation")?
            .len();
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
        self.current = open_regular_at(self.dir.as_raw_fd(), leaf(CURRENT_SEGMENT), true, true)?
            .context("create current lifecycle journal segment")?;
        self.current_bytes = 0;
        Ok(())
    }

    fn repair_oversized_segments(&mut self) -> Result<bool> {
        let mut repaired = repair_oversized_segment(&mut self.current)?;
        self.current_bytes = self
            .current
            .metadata()
            .context("stat current lifecycle journal segment")?
            .len();
        if let Some(mut previous) =
            open_regular_at(self.dir.as_raw_fd(), leaf(PREVIOUS_SEGMENT), false, true)?
        {
            repaired |= repair_oversized_segment(&mut previous)?;
        }
        Ok(repaired)
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

fn repair_oversized_segment(file: &mut File) -> Result<bool> {
    if file
        .metadata()
        .context("stat lifecycle journal segment for bound repair")?
        .len()
        <= MAX_SEGMENT_BYTES
    {
        return Ok(false);
    }
    file.set_len(0)
        .context("truncate oversized lifecycle journal segment")?;
    file.write_all(OVERSIZED_SEGMENT_SENTINEL)
        .context("mark oversized lifecycle journal segment degraded")?;
    Ok(true)
}

fn load_segment(file: File, loaded: &mut LoadedEvents) -> Result<()> {
    if file
        .metadata()
        .context("stat lifecycle journal segment")?
        .len()
        > MAX_SEGMENT_BYTES
    {
        loaded.degraded = true;
    }
    let mut reader = BufReader::new(file.take(MAX_SEGMENT_BYTES));
    loop {
        match read_bounded_line(&mut reader)? {
            BoundedLine::Eof => break,
            BoundedLine::Invalid => loaded.degraded = true,
            BoundedLine::Complete(line) => match serde_json::from_slice::<LifecycleEvent>(&line) {
                Ok(event) => match event.validate_and_sanitize_loaded() {
                    Some((event, changed)) => {
                        loaded.degraded |= changed;
                        loaded.events.push(event);
                    }
                    None => loaded.degraded = true,
                },
                Err(_) => loaded.degraded = true,
            },
        }
    }
    Ok(())
}

enum BoundedLine {
    Eof,
    Complete(Vec<u8>),
    Invalid,
}

fn read_bounded_line(reader: &mut impl BufRead) -> Result<BoundedLine> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader
            .fill_buf()
            .context("read lifecycle journal segment")?;
        if available.is_empty() {
            return if line.is_empty() && !oversized {
                Ok(BoundedLine::Eof)
            } else {
                Ok(BoundedLine::Invalid)
            };
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if !oversized
                && line.len().saturating_add(newline).saturating_add(1) <= MAX_EVENT_LINE_BYTES
            {
                line.extend_from_slice(&available[..newline]);
            } else {
                oversized = true;
            }
            reader.consume(newline + 1);
            return if oversized {
                Ok(BoundedLine::Invalid)
            } else {
                Ok(BoundedLine::Complete(line))
            };
        }

        if !oversized
            && line.len().saturating_add(available.len()).saturating_add(1) <= MAX_EVENT_LINE_BYTES
        {
            line.extend_from_slice(available);
        } else {
            oversized = true;
        }
        let consumed = available.len();
        reader.consume(consumed);
    }
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
    flags |= if writable {
        libc::O_RDWR
    } else {
        libc::O_RDONLY
    };
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
        bail!(
            "journal leaf {} is not a regular file",
            name.to_string_lossy()
        );
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

pub(crate) fn sanitize_exit_signal(value: &str) -> String {
    sanitized_field(value, MAX_SIGNAL_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
    use std::sync::Arc;
    use std::thread;

    fn identity(id: &str) -> LifecycleIdentity {
        LifecycleIdentity {
            session_id: id.to_string(),
            name: "agent\u{1b}]52;c;secret\u{7}".to_string(),
            pane_id: "%1".to_string(),
            parent_session_id: None,
            parent_pane_id: None,
            agent_name: Some("codex".to_string()),
            created_unix_ms: 10,
            process_id: Some(123),
            process_group_id: Some(123),
            attached_clients: 1,
        }
    }

    fn load_bytes(bytes: &[u8]) -> LoadedEvents {
        let mut file = tempfile::tempfile().expect("journal tempfile");
        file.write_all(bytes).expect("write journal bytes");
        file.seek(SeekFrom::Start(0)).expect("rewind journal");
        let mut loaded = LoadedEvents {
            events: Vec::new(),
            degraded: false,
        };
        load_segment(file, &mut loaded).expect("load journal segment");
        loaded
    }

    fn write_private_sized(path: &Path, size: u64) {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .expect("create private journal segment");
        file.set_len(size).expect("size private journal segment");
    }

    fn assert_retained_segments_bounded(path: &Path) {
        for leaf in ["current.jsonl", "previous.jsonl"] {
            let segment = path.join(leaf);
            if let Ok(metadata) = fs::metadata(&segment) {
                assert!(
                    metadata.len() <= MAX_SEGMENT_BYTES,
                    "{leaf} retained {} bytes above {MAX_SEGMENT_BYTES}",
                    metadata.len()
                );
            }
        }
    }

    fn assert_boundary_repair_survives_restart(leaf: &str, size: u64) {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
            .expect("private tempdir");
        write_private_sized(&temp.path().join(leaf), size);

        let session_id = Uuid::new_v4().to_string();
        let event = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            identity(&session_id),
            20,
            SessionExitTrigger::LeaderExited,
        );
        {
            let mut storage = SecureStorage::open(temp.path()).expect("open bounded storage");
            storage
                .append(&event)
                .expect("append after boundary repair");
            assert_retained_segments_bounded(temp.path());
            let loaded = storage.load_all().expect("load repaired storage");
            let exits = fold_events(
                &loaded.events,
                loaded.degraded,
                Some(&session_id),
                10,
                ExitListScope::All,
            );
            assert_eq!(exits.len(), 1, "{leaf} size {size}");
            assert_eq!(
                exits[0].evidence_state,
                ExitEvidenceState::StorageDegraded,
                "{leaf} size {size} must fail closed"
            );
        }

        let storage = SecureStorage::open(temp.path()).expect("restart bounded storage");
        assert_retained_segments_bounded(temp.path());
        let loaded = storage.load_all().expect("reload repaired storage");
        let exits = fold_events(
            &loaded.events,
            loaded.degraded,
            Some(&session_id),
            10,
            ExitListScope::All,
        );
        assert_eq!(exits.len(), 1, "restart {leaf} size {size}");
        assert_eq!(
            exits[0].evidence_state,
            ExitEvidenceState::StorageDegraded,
            "restart {leaf} size {size} must retain degraded evidence"
        );
    }

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

    #[test]
    fn journal_serialization_is_bounded_raw_free_and_sanitized() {
        let event = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            identity("session-1"),
            20,
            SessionExitTrigger::CloseRequested,
        );
        let line = event.to_json_line().expect("serialize lifecycle event");
        let text = String::from_utf8(line).expect("json utf8");
        assert!(text.len() <= MAX_EVENT_LINE_BYTES);
        for forbidden in [
            "command",
            "cwd",
            "environment",
            "scrollback",
            "token",
            "secret",
        ] {
            assert!(!text.contains(forbidden), "leaked forbidden field: {text}");
        }
    }

    #[test]
    fn folding_is_order_independent_and_reap_recovers_missing_trigger() {
        let event_id = Uuid::new_v4();
        let trigger = LifecycleEvent::trigger_claimed(
            event_id,
            identity("session-1"),
            20,
            SessionExitTrigger::LeaderExited,
        );
        let reap = LifecycleEvent::leader_reaped(
            event_id,
            identity("session-1"),
            20,
            30,
            SessionExitTrigger::LeaderExited,
            37,
            Some("TERM".to_string()),
        );
        let forward = fold_events(
            &[trigger.clone(), reap.clone()],
            false,
            None,
            10,
            ExitListScope::All,
        );
        let reverse = fold_events(
            &[reap.clone(), trigger.clone()],
            false,
            None,
            10,
            ExitListScope::All,
        );
        assert_eq!(forward, reverse);
        let duplicated = fold_events(
            &[trigger.clone(), reap.clone(), trigger, reap.clone()],
            false,
            None,
            10,
            ExitListScope::All,
        );
        assert_eq!(forward, duplicated);
        assert_eq!(forward[0].exit_code, Some(37));
        assert_eq!(forward[0].evidence_state, ExitEvidenceState::Complete);

        let recovered = fold_events(&[reap], false, None, 10, ExitListScope::All);
        assert_eq!(
            recovered[0].evidence_state,
            ExitEvidenceState::DegradedMissingTriggerEvent
        );
        assert_eq!(recovered[0].trigger, SessionExitTrigger::LeaderExited);
    }

    #[test]
    fn conflicting_same_sequence_rows_fail_closed() {
        let session_id = Uuid::new_v4().to_string();
        let parent_session_id = Uuid::new_v4().to_string();
        let mut first_identity = identity(&session_id);
        first_identity.name = "candidate-a".to_string();
        first_identity.pane_id = "%1".to_string();
        first_identity.parent_session_id = Some(parent_session_id.clone());
        first_identity.parent_pane_id = Some("%parent".to_string());
        first_identity.created_unix_ms = 10;
        let mut second_identity = identity(&session_id);
        second_identity.name = "candidate-b".to_string();
        second_identity.pane_id = "%9".to_string();
        second_identity.parent_session_id = Some(parent_session_id);
        second_identity.parent_pane_id = Some("%parent".to_string());
        second_identity.created_unix_ms = 99;
        let first = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            first_identity,
            20,
            SessionExitTrigger::LeaderExited,
        );
        let second = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            second_identity,
            200,
            SessionExitTrigger::CloseRequested,
        );
        let folded = fold_events(
            &[first.clone(), second.clone()],
            false,
            None,
            10,
            ExitListScope::All,
        );
        let reversed = fold_events(
            &[second.clone(), first.clone()],
            false,
            None,
            10,
            ExitListScope::All,
        );
        assert_eq!(folded, reversed);
        assert_eq!(folded[0].evidence_state, ExitEvidenceState::Conflicted);
        assert_eq!(folded[0].trigger, SessionExitTrigger::Unknown);
        assert_eq!(folded[0].session_id, session_id);
        assert_eq!(folded[0].name, "");
        assert_eq!(folded[0].pane_id, "");
        assert_eq!(folded[0].created_unix_ms, 0);
        assert_eq!(folded[0].trigger_claimed_unix_ms, 0);
        assert_eq!(folded[0].outcome_state, ExitOutcomeState::Unknown);

        let default = fold_events(
            &[first.clone(), second.clone()],
            false,
            None,
            10,
            ExitListScope::TopLevel,
        );
        assert!(
            default.is_empty(),
            "candidate-free conflict DTO must not become top-level merely because parent identity was removed"
        );
        let children = fold_events(
            &[first.clone(), second.clone()],
            false,
            None,
            10,
            ExitListScope::Children,
        );
        assert!(
            children.is_empty(),
            "conflicted identity is not authoritative enough for a scoped child view"
        );
        let all = fold_events(&[first, second], false, None, 10, ExitListScope::All);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].evidence_state, ExitEvidenceState::Conflicted);
        assert_eq!(all[0].parent_session_id, None);
    }

    #[test]
    fn target_resolution_prefers_exact_uuid_and_one_newest_eligible_tombstone() {
        let old_id = "00000000-0000-4000-8000-000000000001";
        let top_new_id = "00000000-0000-4000-8000-000000000002";
        let child_new_id = "00000000-0000-4000-8000-000000000003";
        let uuid_shadow_id = "00000000-0000-4000-8000-000000000004";
        let scoped_shadow_id = "00000000-0000-4000-8000-000000000005";
        let parent_id = "00000000-0000-4000-8000-000000000099";

        let mut old = identity(old_id);
        old.name = "reused".to_string();
        old.pane_id = "%7".to_string();
        let old = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            old,
            100,
            SessionExitTrigger::LeaderExited,
        );

        let mut top_new = identity(top_new_id);
        top_new.name = "reused".to_string();
        top_new.pane_id = "%8".to_string();
        let top_new = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            top_new,
            200,
            SessionExitTrigger::LeaderExited,
        );

        let mut child_new = identity(child_new_id);
        child_new.name = "reused".to_string();
        child_new.pane_id = "%7".to_string();
        child_new.parent_session_id = Some(parent_id.to_string());
        child_new.parent_pane_id = Some("%parent".to_string());
        let child_new = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            child_new,
            300,
            SessionExitTrigger::ParentCascade {
                parent_session_id: parent_id.to_string(),
            },
        );

        let mut uuid_shadow = identity(uuid_shadow_id);
        uuid_shadow.name = old_id.to_string();
        uuid_shadow.pane_id = "%9".to_string();
        let uuid_shadow = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            uuid_shadow,
            400,
            SessionExitTrigger::LeaderExited,
        );

        let mut scoped_shadow = identity(scoped_shadow_id);
        scoped_shadow.name = child_new_id.to_string();
        scoped_shadow.pane_id = "%10".to_string();
        let scoped_shadow = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            scoped_shadow,
            500,
            SessionExitTrigger::LeaderExited,
        );

        let events = [top_new, uuid_shadow, old, scoped_shadow, child_new];
        let by_name = fold_events(&events, false, Some("reused"), 10, ExitListScope::TopLevel);
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].session_id, top_new_id);

        let by_name_all = fold_events(&events, false, Some("reused"), 10, ExitListScope::All);
        assert_eq!(by_name_all.len(), 1);
        assert_eq!(by_name_all[0].session_id, child_new_id);

        let by_pane = fold_events(&events, false, Some("%7"), 10, ExitListScope::All);
        assert_eq!(by_pane.len(), 1);
        assert_eq!(by_pane[0].session_id, child_new_id);

        let exact = fold_events(&events, false, Some(old_id), 10, ExitListScope::All);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].session_id, old_id);

        let scoped_exact = fold_events(
            &events,
            false,
            Some(child_new_id),
            10,
            ExitListScope::TopLevel,
        );
        assert!(
            scoped_exact.is_empty(),
            "an out-of-scope exact UUID must not fall back to a mutable name collision"
        );
    }

    #[test]
    fn newest_first_order_uses_descending_uuid_as_stable_tie_break() {
        let lower_id = "00000000-0000-4000-8000-000000000010";
        let higher_id = "00000000-0000-4000-8000-000000000011";
        let lower = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            identity(lower_id),
            500,
            SessionExitTrigger::LeaderExited,
        );
        let higher = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            identity(higher_id),
            500,
            SessionExitTrigger::LeaderExited,
        );

        let folded = fold_events(&[lower, higher], false, None, 10, ExitListScope::TopLevel);
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].session_id, higher_id);
        assert_eq!(folded[1].session_id, lower_id);
    }

    #[test]
    fn existing_current_segment_respects_cap_boundaries_and_restart() {
        for size in [
            MAX_SEGMENT_BYTES - 1,
            MAX_SEGMENT_BYTES,
            MAX_SEGMENT_BYTES + 1,
        ] {
            assert_boundary_repair_survives_restart("current.jsonl", size);
        }
    }

    #[test]
    fn existing_previous_segment_respects_cap_boundaries_and_restart() {
        for size in [
            MAX_SEGMENT_BYTES - 1,
            MAX_SEGMENT_BYTES,
            MAX_SEGMENT_BYTES + 1,
        ] {
            assert_boundary_repair_survives_restart("previous.jsonl", size);
        }
    }

    #[test]
    fn corrupt_reads_are_line_and_segment_bounded_and_ignore_truncated_tail() {
        let id = Uuid::new_v4();
        let valid = LifecycleEvent::trigger_claimed(
            id,
            identity(&id.to_string()),
            20,
            SessionExitTrigger::LeaderExited,
        )
        .to_json_line()
        .expect("valid line");
        let parsed: LifecycleEvent =
            serde_json::from_slice(&valid[..valid.len() - 1]).expect("parse valid line");
        assert!(parsed.validate_and_sanitize_loaded().is_some());
        assert_eq!(load_bytes(&valid).events.len(), 1);

        let mut oversized_line = vec![b'x'; MAX_EVENT_LINE_BYTES + 128];
        oversized_line.push(b'\n');
        oversized_line.extend_from_slice(&valid);
        oversized_line.extend_from_slice(br#"{"kind":"trigger_claimed"}"#);
        let loaded = load_bytes(&oversized_line);
        assert!(loaded.degraded);
        assert_eq!(
            loaded.events.len(),
            1,
            "valid row after oversized row survives"
        );

        let oversized_segment = vec![b'x'; usize::try_from(MAX_SEGMENT_BYTES).unwrap() + 128];
        let loaded = load_bytes(&oversized_segment);
        assert!(loaded.degraded);
        assert!(loaded.events.is_empty());
    }

    #[test]
    fn load_rejects_invalid_schema_sequence_and_identity_and_resanitizes_fields() {
        let id = Uuid::new_v4();
        let event = LifecycleEvent::trigger_claimed(
            id,
            identity(&id.to_string()),
            20,
            SessionExitTrigger::LeaderExited,
        );
        let base = serde_json::to_value(&event).expect("event value");
        let mut rows = Vec::new();
        for (field, value) in [
            ("schema_version", serde_json::json!("999")),
            ("event_seq", serde_json::json!(9)),
            ("session_id", serde_json::json!("not-a-uuid")),
        ] {
            let mut invalid = base.clone();
            invalid[field] = value;
            serde_json::to_writer(&mut rows, &invalid).expect("invalid row");
            rows.push(b'\n');
        }
        let mut unsafe_fields = base;
        unsafe_fields["name"] = serde_json::json!(format!(
            "safe\u{1b}]52;c;secret\u{7}{}",
            "x".repeat(MAX_NAME_BYTES + 32)
        ));
        unsafe_fields["agent_name"] = serde_json::json!(format!(
            "agent\u{1b}[31m{}",
            "y".repeat(MAX_AGENT_BYTES + 32)
        ));
        serde_json::to_writer(&mut rows, &unsafe_fields).expect("unsafe row");
        rows.push(b'\n');

        let loaded = load_bytes(&rows);
        assert!(loaded.degraded);
        assert_eq!(loaded.events.len(), 1);
        let folded = fold_events(
            &loaded.events,
            loaded.degraded,
            None,
            10,
            ExitListScope::All,
        );
        assert_eq!(folded.len(), 1);
        assert!(folded[0].name.len() <= MAX_NAME_BYTES);
        assert!(folded[0].agent_name.as_ref().unwrap().len() <= MAX_AGENT_BYTES);
        assert!(!folded[0].name.contains("secret"));
        assert_eq!(folded[0].evidence_state, ExitEvidenceState::StorageDegraded);
    }

    #[test]
    fn secure_storage_rejects_symlink_and_hardlink_leaves() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
            .expect("private tempdir");
        let target = temp.path().join("target");
        fs::write(&target, b"target").expect("target");
        symlink(&target, temp.path().join("current.jsonl")).expect("journal symlink");
        assert!(LifecycleJournal::open(temp.path()).is_err());

        fs::remove_file(temp.path().join("current.jsonl")).expect("remove symlink");
        fs::hard_link(&target, temp.path().join("current.jsonl")).expect("journal hardlink");
        assert!(LifecycleJournal::open(temp.path()).is_err());

        fs::remove_file(temp.path().join("current.jsonl")).expect("remove hardlink");
        fs::write(temp.path().join("current.jsonl"), b"").expect("journal leaf");
        fs::set_permissions(
            temp.path().join("current.jsonl"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("broaden journal leaf");
        assert!(LifecycleJournal::open(temp.path()).is_err());
    }

    #[test]
    fn previous_and_lock_leaves_enforce_private_regular_single_link_safety() {
        for leaf in ["previous.jsonl", "journal.lock"] {
            let temp = tempfile::tempdir().expect("tempdir");
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
                .expect("private tempdir");
            let target = temp.path().join("target");
            write_private_sized(&target, 0);
            symlink(&target, temp.path().join(leaf)).expect("unsafe journal symlink");
            assert!(
                LifecycleJournal::open(temp.path()).is_err(),
                "{leaf} symlink"
            );

            fs::remove_file(temp.path().join(leaf)).expect("remove symlink");
            fs::hard_link(&target, temp.path().join(leaf)).expect("unsafe journal hardlink");
            assert!(
                LifecycleJournal::open(temp.path()).is_err(),
                "{leaf} hardlink"
            );

            fs::remove_file(temp.path().join(leaf)).expect("remove hardlink");
            write_private_sized(&temp.path().join(leaf), 0);
            fs::set_permissions(temp.path().join(leaf), fs::Permissions::from_mode(0o644))
                .expect("broaden unsafe journal leaf");
            assert!(LifecycleJournal::open(temp.path()).is_err(), "{leaf} mode");

            fs::remove_file(temp.path().join(leaf)).expect("remove broad leaf");
            fs::create_dir(temp.path().join(leaf)).expect("unsafe journal directory leaf");
            assert!(
                LifecycleJournal::open(temp.path()).is_err(),
                "{leaf} non-regular"
            );
        }
    }

    #[test]
    fn rotation_refuses_unsafe_previous_target_without_losing_current() {
        for hardlink in [false, true] {
            let temp = tempfile::tempdir().expect("tempdir");
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
                .expect("private tempdir");
            let mut storage = SecureStorage::open(temp.path()).expect("open storage");
            let current = temp.path().join("current.jsonl");
            let before = LifecycleEvent::trigger_claimed(
                Uuid::new_v4(),
                identity(&Uuid::new_v4().to_string()),
                20,
                SessionExitTrigger::LeaderExited,
            );
            storage.append(&before).expect("seed current segment");
            let current_len = fs::metadata(&current).expect("stat current").len();

            let target = temp.path().join("rotation-target");
            write_private_sized(&target, 0);
            if hardlink {
                fs::hard_link(&target, temp.path().join("previous.jsonl"))
                    .expect("unsafe hardlink rotation target");
            } else {
                symlink(&target, temp.path().join("previous.jsonl"))
                    .expect("unsafe symlink rotation target");
            }
            assert!(storage.rotate().is_err());
            assert_eq!(
                fs::metadata(&current).expect("current remains").len(),
                current_len
            );
        }
    }

    #[test]
    fn failed_rotation_preserves_previous_segment() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
            .expect("private tempdir");
        let mut storage = SecureStorage::open(temp.path()).expect("open storage");
        let current = LifecycleEvent::trigger_claimed(
            Uuid::new_v4(),
            identity(&Uuid::new_v4().to_string()),
            20,
            SessionExitTrigger::LeaderExited,
        );
        storage.append(&current).expect("seed current segment");

        let previous_path = temp.path().join("previous.jsonl");
        let previous_bytes = b"retained previous segment\n";
        fs::write(&previous_path, previous_bytes).expect("seed previous segment");
        fs::set_permissions(&previous_path, fs::Permissions::from_mode(0o600))
            .expect("private previous segment");

        fs::remove_file(temp.path().join("current.jsonl"))
            .expect("remove current path while storage keeps its open descriptor");
        assert!(
            storage.rotate().is_err(),
            "missing source path must fail rotation"
        );
        assert_eq!(
            fs::read(&previous_path).expect("previous segment must remain readable"),
            previous_bytes,
            "failed replacement must preserve the retained previous segment"
        );
    }

    #[test]
    fn advisory_lock_serializes_storage_instances() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
            .expect("private tempdir");
        let first = SecureStorage::open(temp.path()).expect("first storage lock");
        assert!(
            SecureStorage::open(temp.path()).is_err(),
            "second storage must not bypass the advisory lock"
        );
        drop(first);
        SecureStorage::open(temp.path()).expect("lock released after first storage drops");
    }
}

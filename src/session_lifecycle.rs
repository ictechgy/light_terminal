//! Session-finalization authority and bounded publication barrier.
//!
//! This module deliberately contains no PTY byte handling. It owns only the
//! in-memory lifecycle truth published before destructive cleanup and later
//! acts as the acknowledgement boundary for the private exit journal.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(crate) const PUBLICATION_TIMEOUT: Duration = Duration::from_millis(100);

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

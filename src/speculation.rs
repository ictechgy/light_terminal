#![allow(dead_code)]

use crate::protocol::SpeculationPhase;
use std::cmp::Ordering;
use std::fmt;
use std::time::Duration;

pub const CANDIDATE_COUNT: usize = 2;
pub const MAX_LIVE_TOURNAMENTS: usize = 8;
pub const MAX_DURABLE_RECORDS: usize = 1_024;
pub const MAX_DURABLE_RECORD_BYTES: usize = 32 * 1_024;
pub const MAX_CANDIDATE_OUTPUT_BYTES: u64 = 64 * 1_024 * 1_024;
pub const PREPARED_LEASE: Duration = Duration::from_secs(30);
pub const READY_LEASE: Duration = Duration::from_secs(30);
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const MIN_RUN_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_RUN_TIMEOUT: Duration = Duration::from_secs(60 * 60);
pub const PENDING_FINALIZE_LEASE: Duration = Duration::from_secs(10 * 60);
pub const CONTROL_ACK_TIMEOUT: Duration = Duration::from_secs(5);
pub const TERMINAL_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateResult {
    pub input_index: u8,
    pub exit_success: Option<bool>,
    pub elapsed_ns: Option<u64>,
    pub output_bytes: Option<u64>,
    pub quiescent: bool,
    pub output_overflowed: bool,
}

impl CandidateResult {
    pub fn is_eligible(self) -> bool {
        self.input_index < CANDIDATE_COUNT as u8
            && self.exit_success.is_some()
            && self.elapsed_ns.is_some()
            && self
                .output_bytes
                .is_some_and(|bytes| bytes <= MAX_CANDIDATE_OUTPUT_BYTES)
            && self.quiescent
            && !self.output_overflowed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreDecision {
    Selected(u8),
    Rollback,
}

pub fn score_candidates(candidates: [CandidateResult; CANDIDATE_COUNT]) -> ScoreDecision {
    if candidates[0].input_index == candidates[1].input_index
        || !candidates
            .iter()
            .all(|candidate| candidate.input_index < CANDIDATE_COUNT as u8)
    {
        return ScoreDecision::Rollback;
    }
    match compare_candidates(candidates[0], candidates[1]) {
        None => ScoreDecision::Rollback,
        Some(Ordering::Greater | Ordering::Equal) => {
            ScoreDecision::Selected(candidates[0].input_index)
        }
        Some(Ordering::Less) => ScoreDecision::Selected(candidates[1].input_index),
    }
}

fn compare_candidates(left: CandidateResult, right: CandidateResult) -> Option<Ordering> {
    let left_eligible = left.is_eligible();
    let right_eligible = right.is_eligible();
    match (left_eligible, right_eligible) {
        (false, false) => return None,
        (true, false) => return Some(Ordering::Greater),
        (false, true) => return Some(Ordering::Less),
        (true, true) => {}
    }

    Some(
        left.exit_success
            .cmp(&right.exit_success)
            .then_with(|| right.elapsed_ns.cmp(&left.elapsed_ns))
            .then_with(|| right.output_bytes.cmp(&left.output_bytes))
            .then_with(|| right.input_index.cmp(&left.input_index)),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TournamentState {
    pub phase: SpeculationPhase,
    pub generation: u64,
}

impl TournamentState {
    pub fn transition(
        &mut self,
        expected_generation: u64,
        next: SpeculationPhase,
    ) -> Result<(), TransitionError> {
        if expected_generation != self.generation {
            return Err(TransitionError::StaleGeneration);
        }
        if self.phase == next && self.phase.is_terminal() {
            return Ok(());
        }
        if !is_legal_transition(self.phase, next) {
            return Err(TransitionError::IllegalTransition);
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(TransitionError::GenerationExhausted)?;
        self.phase = next;
        self.generation = generation;
        Ok(())
    }

    pub fn normalize_after_daemon_restart(
        &mut self,
        expected_generation: u64,
    ) -> Result<(), TransitionError> {
        if self.phase.is_terminal() || self.phase == SpeculationPhase::RollbackRequired {
            return Ok(());
        }
        if expected_generation != self.generation {
            return Err(TransitionError::StaleGeneration);
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(TransitionError::GenerationExhausted)?;
        self.phase = SpeculationPhase::RollbackRequired;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionError {
    StaleGeneration,
    IllegalTransition,
    GenerationExhausted,
}

impl TransitionError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::StaleGeneration => "stale_generation",
            Self::IllegalTransition => "illegal_transition",
            Self::GenerationExhausted => "generation_exhausted",
        }
    }
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for TransitionError {}

pub const fn is_legal_transition(from: SpeculationPhase, to: SpeculationPhase) -> bool {
    use SpeculationPhase::*;
    match from {
        Prepared => matches!(to, Armed | RollbackPending),
        Armed => matches!(to, Starting | RollbackPending | RollbackRequired),
        Starting => matches!(to, Ready | RollbackPending | RollbackRequired),
        Ready => matches!(to, GoPending | RollbackPending),
        GoPending => matches!(to, Running | RollbackPending | DecisionUncertain),
        Running => matches!(to, ResultPending | RollbackPending | RollbackRequired),
        ResultPending => matches!(to, PendingFinalize | RollbackPending | RollbackRequired),
        PendingFinalize => {
            matches!(to, FinalizingLoser | RollbackPending | RollbackRequired)
        }
        FinalizingLoser => matches!(
            to,
            WinnerSelectionPending | RollbackRequired | DecisionUncertain
        ),
        WinnerSelectionPending => {
            matches!(to, FinalizingWinner | RollbackRequired | DecisionUncertain)
        }
        FinalizingWinner => matches!(to, Selected | RollbackRequired | DecisionUncertain),
        RollbackRequired => matches!(to, RollbackPending | RollbackRequired),
        DecisionUncertain => matches!(to, RollbackPending | DecisionUncertain),
        RollbackPending => matches!(to, RolledBack | RollbackRequired),
        Selected => matches!(to, Selected),
        RolledBack => matches!(to, RolledBack),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        input_index: u8,
        exit_success: bool,
        elapsed_ns: u64,
        output_bytes: u64,
    ) -> CandidateResult {
        CandidateResult {
            input_index,
            exit_success: Some(exit_success),
            elapsed_ns: Some(elapsed_ns),
            output_bytes: Some(output_bytes),
            quiescent: true,
            output_overflowed: false,
        }
    }

    #[test]
    fn speculation_fixed_score_is_deterministic_at_every_tier() {
        assert_eq!(
            score_candidates([candidate(0, true, 100, 100), candidate(1, false, 1, 1)]),
            ScoreDecision::Selected(0)
        );
        assert_eq!(
            score_candidates([candidate(0, true, 2, 1), candidate(1, true, 1, 99)]),
            ScoreDecision::Selected(1)
        );
        assert_eq!(
            score_candidates([candidate(0, true, 1, 2), candidate(1, true, 1, 1)]),
            ScoreDecision::Selected(1)
        );
        assert_eq!(
            score_candidates([candidate(1, true, 1, 1), candidate(0, true, 1, 1)]),
            ScoreDecision::Selected(0)
        );
    }

    #[test]
    fn speculation_ineligible_results_fail_closed() {
        let eligible = candidate(0, false, 10, 10);
        let mut invalid_right = candidate(1, true, 1, 1);
        invalid_right.quiescent = false;
        assert_eq!(
            score_candidates([eligible, invalid_right]),
            ScoreDecision::Selected(0)
        );

        let mut invalid_left = candidate(0, true, 1, MAX_CANDIDATE_OUTPUT_BYTES + 1);
        invalid_left.output_overflowed = true;
        assert_eq!(
            score_candidates([invalid_left, invalid_right]),
            ScoreDecision::Rollback
        );

        let mut missing = candidate(0, true, 1, 1);
        missing.elapsed_ns = None;
        assert_eq!(
            score_candidates([
                missing,
                candidate(1, true, 1, MAX_CANDIDATE_OUTPUT_BYTES + 1)
            ]),
            ScoreDecision::Rollback
        );
    }

    #[test]
    fn speculation_phase_graph_is_exhaustive_and_fail_closed() {
        use SpeculationPhase::*;
        let legal: &[(SpeculationPhase, &[SpeculationPhase])] = &[
            (Prepared, &[Armed, RollbackPending]),
            (Armed, &[Starting, RollbackPending, RollbackRequired]),
            (Starting, &[Ready, RollbackPending, RollbackRequired]),
            (Ready, &[GoPending, RollbackPending]),
            (GoPending, &[Running, RollbackPending, DecisionUncertain]),
            (Running, &[ResultPending, RollbackPending, RollbackRequired]),
            (
                ResultPending,
                &[PendingFinalize, RollbackPending, RollbackRequired],
            ),
            (
                PendingFinalize,
                &[FinalizingLoser, RollbackPending, RollbackRequired],
            ),
            (
                FinalizingLoser,
                &[WinnerSelectionPending, RollbackRequired, DecisionUncertain],
            ),
            (
                WinnerSelectionPending,
                &[FinalizingWinner, RollbackRequired, DecisionUncertain],
            ),
            (
                FinalizingWinner,
                &[Selected, RollbackRequired, DecisionUncertain],
            ),
            (RollbackRequired, &[RollbackPending, RollbackRequired]),
            (DecisionUncertain, &[RollbackPending, DecisionUncertain]),
            (RollbackPending, &[RolledBack, RollbackRequired]),
            (Selected, &[Selected]),
            (RolledBack, &[RolledBack]),
        ];
        for from in SpeculationPhase::ALL {
            let expected = legal
                .iter()
                .find_map(|(phase, next)| (*phase == from).then_some(*next))
                .expect("every phase is enumerated");
            for to in SpeculationPhase::ALL {
                assert_eq!(
                    is_legal_transition(from, to),
                    expected.contains(&to),
                    "{from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn speculation_generation_cas_allows_only_one_race_winner() {
        let mut state = TournamentState {
            phase: SpeculationPhase::PendingFinalize,
            generation: 4,
        };
        state
            .transition(4, SpeculationPhase::FinalizingLoser)
            .expect("finalize wins CAS");
        assert_eq!(
            state.transition(4, SpeculationPhase::RollbackPending),
            Err(TransitionError::StaleGeneration)
        );
        assert_eq!(state.phase, SpeculationPhase::FinalizingLoser);
        assert_eq!(state.generation, 5);
    }

    #[test]
    fn speculation_terminal_transitions_are_generation_stable_and_idempotent() {
        for phase in [SpeculationPhase::Selected, SpeculationPhase::RolledBack] {
            let mut state = TournamentState {
                phase,
                generation: 9,
            };
            state
                .transition(9, phase)
                .expect("terminal request is idempotent");
            assert_eq!(state.phase, phase);
            assert_eq!(state.generation, 9);
        }
    }

    #[test]
    fn speculation_restart_is_cleanup_only_and_selection_order_is_enforced() {
        let mut state = TournamentState {
            phase: SpeculationPhase::DecisionUncertain,
            generation: 11,
        };
        state
            .normalize_after_daemon_restart(11)
            .expect("restart normalizes to rollback required");
        assert_eq!(state.phase, SpeculationPhase::RollbackRequired);
        assert_eq!(state.generation, 12);

        let mut pending = TournamentState {
            phase: SpeculationPhase::PendingFinalize,
            generation: 2,
        };
        assert_eq!(
            pending.transition(2, SpeculationPhase::WinnerSelectionPending),
            Err(TransitionError::IllegalTransition)
        );
    }

    #[test]
    fn speculation_contract_constants_match_fixed_bounds() {
        assert_eq!(CANDIDATE_COUNT, 2);
        assert_eq!(MAX_LIVE_TOURNAMENTS, 8);
        assert_eq!(MAX_DURABLE_RECORDS, 1_024);
        assert_eq!(MAX_DURABLE_RECORD_BYTES, 32 * 1_024);
        assert_eq!(MAX_CANDIDATE_OUTPUT_BYTES, 64 * 1_024 * 1_024);
        assert_eq!(PREPARED_LEASE, Duration::from_secs(30));
        assert_eq!(READY_LEASE, Duration::from_secs(30));
        assert_eq!(DEFAULT_RUN_TIMEOUT, Duration::from_secs(600));
        assert_eq!(MIN_RUN_TIMEOUT, Duration::from_secs(1));
        assert_eq!(MAX_RUN_TIMEOUT, Duration::from_secs(3_600));
        assert_eq!(PENDING_FINALIZE_LEASE, Duration::from_secs(600));
        assert_eq!(CONTROL_ACK_TIMEOUT, Duration::from_secs(5));
        assert_eq!(TERMINAL_RETENTION, Duration::from_secs(604_800));
    }
}

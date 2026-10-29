//! Shared operational lifecycle for worker submission and correction.
//!
//! These records neither accept mathematical work nor widen a request's scope.
//! The ordinary checker and source-backed publication remain authoritative.
//! Hosts persist events and immutable evidence before requesting transitions.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    pub max_submissions: u32,
    pub max_infrastructure_retries: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self { version: 1, max_submissions: 3, max_infrastructure_retries: 2 }
    }
}

impl Policy {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.max_submissions == 0 {
            return Err("unsupported worker submission policy or zero submission limit".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: String,
    pub thread_id: String,
    /// Stable, private provider home, retained across this attempt's turns.
    pub home: String,
}

impl Session {
    fn validate(&self) -> Result<(), String> {
        if [&self.provider, &self.model, &self.reasoning_effort, &self.thread_id, &self.home]
            .iter().any(|s| s.trim().is_empty()) {
            return Err("worker continuation requires an exact provider session binding".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub process: crate::dispatch::ProcessIdentity,
    pub turn: u32,
    /// Measured by the owned launcher, not a worker-supplied timestamp.
    pub elapsed_ms: u64,
    /// The owned watchdog stopped/reaped execution at allowance exhaustion.
    /// The captured source may still be checked, but the conversation cannot
    /// receive a correction turn. This is not a provider turn.completed claim.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exhausted: bool,
    pub response_sha256: String,
    pub session: Session,
}

/// Authenticated stopped transport, not a provider submission or proof verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interruption {
    pub process: crate::dispatch::ProcessIdentity,
    pub turn: u32,
    pub elapsed_ms: u64,
    pub evidence_sha256: String,
    pub session: Session,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    Ready { feedback_sha256: Option<String> },
    TransportReady,
    Running { process: crate::dispatch::ProcessIdentity, started_at_ms: u64 },
    Submitted { completion: Completion },
    Checking { candidate_sha256: String },
    Finished,
}

/// One explicit host-fault recovery, never a policy refill or proof decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfrastructureRecoveryGrant {
    pub candidate_sha256: String,
    pub retry: u32,
    pub acceptance_round: u32,
    pub finalization: u32,
    pub completion_sha256: String,
}

impl InfrastructureRecoveryGrant {
    fn validate(&self, policy: &Policy) -> Result<(), String> {
        digest(&self.candidate_sha256)?;
        digest(&self.completion_sha256)?;
        if self.acceptance_round == 0 || self.finalization == 0
            || Some(self.retry) != policy.max_infrastructure_retries.checked_add(1) {
            return Err("host recovery requires one extra finalization retry and exact positive coordinates".into());
        }
        Ok(())
    }
}

/// Compact current state only. Full submissions, diagnostics and receipts live
/// in verified attempt history; do not append them to the protocol checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub policy: Policy,
    pub contract_sha256: String,
    pub allowance_ms: u64,
    pub consumed_ms: u64,
    pub turn: u32,
    pub submissions: u32,
    pub infrastructure_retries: u32,
    #[serde(default, skip_serializing_if = "crate::worker_attempt::is_zero")]
    pub transport_retries: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_interruption: Option<Interruption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub infrastructure_recovery_grant: Option<InfrastructureRecoveryGrant>,
    pub session: Option<Session>,
    pub phase: Phase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialAttempt {
    pub request_id: u32,
    pub attempt: Attempt,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Launched { process: crate::dispatch::ProcessIdentity, started_at_ms: u64 },
    TurnCompleted { completion: Completion },
    Interrupted { interruption: Interruption },
    Sealed { candidate_sha256: String },
    /// Issued by host validation after checking the original frozen contract.
    /// It authorizes correction, never a mathematical transition/publication.
    Correction { candidate_sha256: String, feedback_sha256: String },
    InfrastructureRetry { candidate_sha256: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery_completion_sha256: Option<String> },
    /// Owned expiry without a delivered candidate or established conversation.
    Expired { process: crate::dispatch::ProcessIdentity },
    Finished,
}

fn is_zero(value: &u32) -> bool { *value == 0 }

fn digest(value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return Err("worker lifecycle requires a canonical SHA-256 identity".into());
    }
    Ok(())
}

impl Attempt {
    pub fn new(policy: Policy, contract_sha256: String, allowance_ms: u64) -> Result<Self, String> {
        policy.validate()?;
        digest(&contract_sha256)?;
        if allowance_ms == 0 { return Err("worker allowance must be positive".into()); }
        Ok(Self { policy, contract_sha256, allowance_ms, consumed_ms: 0, turn: 1,
            submissions: 0, infrastructure_retries: 0, transport_retries: 0, last_interruption: None,
            infrastructure_recovery_grant: None, session: None,
            phase: Phase::Ready { feedback_sha256: None } })
    }

    pub fn remaining_ms(&self) -> u64 { self.allowance_ms.saturating_sub(self.consumed_ms) }

    pub fn validate(&self) -> Result<(), String> {
        self.policy.validate()?;
        digest(&self.contract_sha256)?;
        let limit = if let Some(grant) = &self.infrastructure_recovery_grant {
            grant.validate(&self.policy)?;
            if self.infrastructure_retries < self.policy.max_infrastructure_retries {
                return Err("host recovery grant precedes exhausted infrastructure allowance".into());
            }
            grant.retry
        } else { self.policy.max_infrastructure_retries };
        if self.allowance_ms == 0 || self.consumed_ms > self.allowance_ms || self.turn == 0
            || self.submissions > self.policy.max_submissions || self.infrastructure_retries > limit {
            return Err("invalid worker attempt budget or submission counters".into());
        }
        if let Some(session) = &self.session { session.validate()?; }
        if self.transport_retries > self.infrastructure_retries {
            return Err("transport retries exceed the infrastructure allowance".into());
        }
        if let Some(interruption) = &self.last_interruption {
            digest(&interruption.evidence_sha256)?;
            interruption.session.validate()?;
            if self.session.as_ref() != Some(&interruption.session) || interruption.turn > self.turn {
                return Err("transport history differs from the retained conversation".into());
            }
        }
        if !matches!(self.phase, Phase::Finished) {
            let submitted = matches!(self.phase, Phase::Checking { .. });
            if self.turn != self.submissions.checked_add(u32::from(!submitted)).ok_or("submission count overflow")? {
                return Err("worker attempt turn does not match its submission phase".into());
            }
        }
        match &self.phase {
            Phase::TransportReady => {
                if self.transport_retries == 0 || self.last_interruption.as_ref().is_none_or(|i| i.turn != self.turn)
                    || self.remaining_ms() == 0 {
                    return Err("transport continuation has no stopped execution or allowance".into());
                }
            }
            Phase::Ready { feedback_sha256 } => {
                if feedback_sha256.is_some() != (self.turn > 1) {
                    return Err("worker correction requires retained checker feedback".into());
                }
                if let Some(feedback) = feedback_sha256 { digest(feedback)?; }
            }
            Phase::Running { process, .. } => {
                if process.pid == 0 || process.start_ticks == 0 || process.boot_id.is_empty() {
                    return Err("worker turn requires an owned process identity".into());
                }
            }
            Phase::Submitted { completion } => {
                digest(&completion.response_sha256)?;
                if completion.turn != self.turn || completion.elapsed_ms > self.consumed_ms
                    || self.session.as_ref() != Some(&completion.session) {
                    return Err("worker submission does not match its completion receipt".into());
                }
            }
            Phase::Checking { candidate_sha256 } => {
                digest(candidate_sha256)?;
                if self.session.is_none() || self.submissions == 0 {
                    return Err("worker checking requires a completed provider turn".into());
                }
            }
            Phase::Finished => {}
        }
        if self.turn > 1 && self.session.is_none() {
            return Err("worker correction requires the original provider conversation".into());
        }
        Ok(())
    }

    pub fn deadline_ms(&self) -> Option<u64> {
        match self.phase {
            Phase::Running { started_at_ms, .. } => Some(started_at_ms.saturating_add(self.remaining_ms())),
            _ => None,
        }
    }

    pub fn can_correct(&self) -> bool {
        matches!(self.phase, Phase::Checking { .. }) && self.remaining_ms() > 0
            && self.submissions < self.policy.max_submissions && self.session.is_some()
    }

    pub fn digest(&self) -> Result<String, String> {
        Ok(crate::trusted_artifact_rebind::digest(&serde_json::to_value(self).map_err(|e| e.to_string())?))
    }

    pub fn grant_infrastructure_recovery(&mut self, grant: InfrastructureRecoveryGrant) -> Result<bool, String> {
        self.validate()?;
        self.checking(&grant.candidate_sha256)?;
        grant.validate(&self.policy)?;
        if let Some(previous) = &self.infrastructure_recovery_grant {
            return if previous == &grant { Ok(false) }
                else { Err("worker attempt already has a different host recovery grant".into()) };
        }
        if self.infrastructure_retries != self.policy.max_infrastructure_retries {
            return Err("host recovery requires exhausted infrastructure allowance".into());
        }
        self.infrastructure_recovery_grant = Some(grant);
        Ok(true)
    }

    pub(crate) fn check_retry_binding(&self, candidate: &str, retry: u32, completion: Option<&str>) -> Result<(), String> {
        self.checking(candidate)?;
        if retry > self.policy.max_infrastructure_retries {
            if !self.infrastructure_recovery_grant.as_ref().is_some_and(|grant|
                grant.retry == retry && grant.candidate_sha256 == candidate
                    && completion == Some(grant.completion_sha256.as_str())) {
                return Err("worker host-validation retry limit is exhausted; retain the submission".into());
            }
        } else if completion.is_some() {
            return Err("ordinary host retry cannot consume a recovery completion".into());
        }
        Ok(())
    }

    /// Rejected transitions are atomic. Duplicate completion/launch deliveries
    /// are harmless only when they match the exact retained active turn.
    pub fn apply(&mut self, event: Event) -> Result<(), String> {
        let mut next = self.clone();
        next.validate()?;
        next.transition(event)?;
        next.validate()?;
        *self = next;
        Ok(())
    }

    fn checking(&self, candidate: &str) -> Result<(), String> {
        digest(candidate)?;
        if self.phase != (Phase::Checking { candidate_sha256: candidate.into() }) {
            return Err("worker check belongs to a different submission or phase".into());
        }
        Ok(())
    }

    fn transition(&mut self, event: Event) -> Result<(), String> {
        self.policy.validate()?;
        match event {
            Event::Launched { process, started_at_ms } => {
                if self.phase == (Phase::Running { process: process.clone(), started_at_ms }) { return Ok(()); }
                if !matches!(self.phase, Phase::Ready { .. } | Phase::TransportReady) || self.remaining_ms() == 0
                    || self.submissions >= self.policy.max_submissions {
                    return Err("worker attempt has no authorized execution turn remaining".into());
                }
                if process.pid == 0 || process.boot_id.is_empty() || process.start_ticks == 0 {
                    return Err("worker turn requires an owned process identity".into());
                }
                self.phase = Phase::Running { process, started_at_ms };
            }
            Event::TurnCompleted { completion } => {
                if self.phase == (Phase::Submitted { completion: completion.clone() }) { return Ok(()); }
                let Phase::Running { process, .. } = &self.phase else {
                    return Err("worker completion requires a running turn".into());
                };
                if process != &completion.process || completion.turn != self.turn {
                    return Err("worker completion belongs to a different execution turn".into());
                }
                if completion.elapsed_ms > self.remaining_ms() {
                    return Err("worker turn completed after its remaining allowance".into());
                }
                if completion.exhausted && completion.elapsed_ms != self.remaining_ms() {
                    return Err("exhausted worker capture must spend the entire remaining allowance".into());
                }
                completion.session.validate()?;
                digest(&completion.response_sha256)?;
                if self.session.as_ref().is_some_and(|session| session != &completion.session) {
                    return Err("worker correction did not resume the same provider conversation".into());
                }
                self.consumed_ms += completion.elapsed_ms;
                self.session = Some(completion.session.clone());
                self.phase = Phase::Submitted { completion };
            }
            Event::Interrupted { interruption } => {
                if self.last_interruption.as_ref() == Some(&interruption) { return Ok(()); }
                if !matches!(&self.phase, Phase::Running { process, .. } if process == &interruption.process)
                    || interruption.turn != self.turn || interruption.elapsed_ms > self.remaining_ms() {
                    return Err("transport interruption differs from the owned running turn".into());
                }
                interruption.session.validate()?;
                digest(&interruption.evidence_sha256)?;
                if self.session.as_ref().is_some_and(|s| s != &interruption.session) {
                    return Err("transport interruption changed the provider conversation".into());
                }
                self.consumed_ms += interruption.elapsed_ms;
                self.session = Some(interruption.session.clone());
                self.last_interruption = Some(interruption);
                if self.remaining_ms() == 0 || self.infrastructure_retries >= self.policy.max_infrastructure_retries {
                    self.phase = Phase::Finished;
                } else {
                    self.infrastructure_retries += 1;
                    self.transport_retries = self.transport_retries.checked_add(1).ok_or("transport retry overflow")?;
                    self.phase = Phase::TransportReady;
                }
            }
            Event::Sealed { candidate_sha256 } => {
                digest(&candidate_sha256)?;
                if self.phase == (Phase::Checking { candidate_sha256: candidate_sha256.clone() }) { return Ok(()); }
                if !matches!(self.phase, Phase::Submitted { .. }) {
                    return Err("worker source sealing requires completed execution and cleanup".into());
                }
                self.submissions = self.submissions.checked_add(1).ok_or("submission count overflow")?;
                self.phase = Phase::Checking { candidate_sha256 };
            }
            Event::Correction { candidate_sha256, feedback_sha256 } => {
                self.checking(&candidate_sha256)?;
                digest(&feedback_sha256)?;
                if !self.can_correct() {
                    return Err("worker correction allowance or submission limit is exhausted".into());
                }
                self.turn = self.turn.checked_add(1).ok_or("worker turn overflow")?;
                self.phase = Phase::Ready { feedback_sha256: Some(feedback_sha256) };
            }
            Event::InfrastructureRetry { candidate_sha256, recovery_completion_sha256 } => {
                let retry = self.infrastructure_retries.checked_add(1).ok_or("host retry counter overflow")?;
                self.check_retry_binding(&candidate_sha256, retry, recovery_completion_sha256.as_deref())?;
                self.infrastructure_retries = retry;
            }
            Event::Expired { process } => {
                if !matches!(&self.phase, Phase::Running { process: owned, .. } if owned == &process) {
                    return Err("worker expiry requires its running owned process".into());
                }
                self.consumed_ms = self.allowance_ms;
                self.phase = Phase::Finished;
            }
            Event::Finished => self.phase = Phase::Finished,
        }
        Ok(())
    }
}

/// The same checker feedback bytes authorize the native correction and the
/// next provider turn. Provider-declared Invalid/Stuck remain terminal.
pub fn feedback(response: &crate::model::WorkerResponse) -> serde_json::Value {
    serde_json::json!({"response": response})
}

pub fn serial_correction_allowed(
    state: &crate::model::ProtocolState,
    candidate: &str,
    submitted: Option<crate::model::WorkerOutcome>,
    response: &crate::model::WorkerResponse,
) -> Result<bool, String> {
    use crate::model::{WorkerOutcome, ResponseStatus};
    let Some(record) = &state.worker_attempt else { return Ok(false) };
    let request = state.in_flight_request.as_ref().ok_or("worker check has no current request")?;
    if state.dispatch.is_some() || record.request_id != request.id
        || response.request_id != request.id || response.cycle != request.cycle {
        return Err("worker check belongs to another serial request".into());
    }
    record.attempt.checking(candidate)?;
    if !record.attempt.can_correct() || response.transport_failure || response.preflight_error.is_some()
        || !(response.outcome == WorkerOutcome::Invalid || response.status == ResponseStatus::Malformed)
        || !(submitted == Some(WorkerOutcome::Valid) || submitted.is_none() && response.status == ResponseStatus::Malformed) {
        return Ok(false);
    }
    let preview = crate::engine::apply_event(state.clone(), crate::engine::ProtocolEvent::WrapperResponse {
        response: crate::model::WrapperResponse::Worker(response.clone()),
    }).map_err(|e| format!("worker validation preview failed: {e:?}"))?;
    Ok(!preview.commands.iter().any(|command| matches!(command,
        crate::engine::ProtocolCommand::WriteHaltSentinel { .. })))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hash(c: char) -> String { c.to_string().repeat(64) }
    fn process(turn: u32) -> crate::dispatch::ProcessIdentity {
        crate::dispatch::ProcessIdentity { pid: turn, start_ticks: turn.into(), boot_id: "boot".into() }
    }
    fn completion(turn: u32, elapsed_ms: u64) -> Completion {
        Completion { process: process(turn), turn, elapsed_ms, exhausted: false, response_sha256: hash('a'),
            session: Session { provider: "codex".into(), model: "sol".into(), reasoning_effort: "high".into(),
                thread_id: "exact-thread".into(), home: "/private/attempt/home".into() } }
    }
    fn start(a: &mut Attempt, now: u64) {
        a.apply(Event::Launched { process: process(a.turn), started_at_ms: now }).unwrap();
    }
    fn submit(a: &mut Attempt, elapsed: u64) {
        a.apply(Event::TurnCompleted { completion: completion(a.turn, elapsed) }).unwrap();
        a.apply(Event::Sealed { candidate_sha256: hash('b') }).unwrap();
    }
    fn attempt() -> Attempt { Attempt::new(Policy::default(), hash('c'), 1000).unwrap() }

    #[test]
    fn transport_interruption_resumes_exact_turn_with_bounded_total_clock() {
        let mut a = attempt();
        start(&mut a, 100);
        let original = a.clone();
        let stopped = Interruption { process: process(1), turn: 1, elapsed_ms: 100,
            evidence_sha256: hash('d'), session: completion(1, 100).session };
        let mut wrong = stopped.clone(); wrong.process = process(2);
        assert!(a.apply(Event::Interrupted { interruption: wrong }).is_err());
        assert_eq!(a, original);
        a.apply(Event::Interrupted { interruption: stopped.clone() }).unwrap();
        assert_eq!((a.turn, a.submissions, a.consumed_ms, a.transport_retries, a.infrastructure_retries), (1, 0, 100, 1, 1));
        assert_eq!(a.phase, Phase::TransportReady);
        let once = a.clone();
        a.apply(Event::Interrupted { interruption: stopped }).unwrap(); assert_eq!(a, once);
        a.apply(Event::Launched { process: process(2), started_at_ms: 2000 }).unwrap();
        let mut done = completion(1, 300); done.process = process(2);
        a.apply(Event::TurnCompleted { completion: done }).unwrap();
        a.apply(Event::Sealed { candidate_sha256: hash('b') }).unwrap();
        assert_eq!((a.turn, a.submissions, a.consumed_ms), (1, 1, 400));
        a.apply(Event::Correction { candidate_sha256: hash('b'), feedback_sha256: hash('e') }).unwrap();
        start(&mut a, 3000);
        a.apply(Event::Interrupted { interruption: Interruption { process: process(2), turn: 2,
            elapsed_ms: 100, evidence_sha256: hash('f'), session: completion(1, 0).session } }).unwrap();
        assert_eq!((a.turn, a.submissions, a.consumed_ms, a.infrastructure_retries), (2, 1, 500, 2));
        a.apply(Event::Launched { process: process(3), started_at_ms: 4000 }).unwrap();
        a.apply(Event::Interrupted { interruption: Interruption { process: process(3), turn: 2,
            elapsed_ms: 50, evidence_sha256: hash('a'), session: completion(1, 0).session } }).unwrap();
        assert_eq!(a.phase, Phase::Finished);
        assert_eq!((a.consumed_ms, a.submissions, a.infrastructure_retries, a.transport_retries), (550, 1, 2, 2));
    }

    #[test]
    fn transport_interruption_rejects_changed_session_and_overspend_atomically() {
        let mut a = attempt(); start(&mut a, 0);
        a.session = Some(completion(1, 0).session);
        let original = a.clone();
        let mut stopped = Interruption { process: process(1), turn: 1, elapsed_ms: 1001,
            evidence_sha256: hash('d'), session: completion(1, 0).session };
        assert!(a.apply(Event::Interrupted { interruption: stopped.clone() }).is_err());
        stopped.elapsed_ms = 10; stopped.session.thread_id = "other".into();
        assert!(a.apply(Event::Interrupted { interruption: stopped.clone() }).is_err());
        assert_eq!(a, original);
        stopped.session = completion(1, 0).session; stopped.elapsed_ms = 1000;
        a.apply(Event::Interrupted { interruption: stopped }).unwrap();
        assert_eq!(a.phase, Phase::Finished); assert_eq!(a.consumed_ms, 1000);
        assert_eq!(a.transport_retries, 0);
    }

    #[test]
    fn serial_worker_threadless_expiry_consumes_original_allowance() {
        let mut a = attempt();
        assert!(a.apply(Event::Expired { process: process(1) }).is_err());
        start(&mut a, 100);
        let before = a.clone();
        assert!(a.apply(Event::Expired { process: process(2) }).is_err());
        assert_eq!(a, before);
        a.apply(Event::Expired { process: process(1) }).unwrap();
        assert_eq!(a.remaining_ms(), 0);
        assert_eq!(a.consumed_ms, a.allowance_ms);
        assert_eq!(a.phase, Phase::Finished);
        assert!(a.session.is_none());
        assert!(a.apply(Event::Launched { process: process(1), started_at_ms: 200 }).is_err());
    }

    #[test]
    fn correction_charges_original_allowance_without_host_wait_or_reset() {
        let mut a = attempt(); start(&mut a, 100); submit(&mut a, 600);
        assert_eq!(a.deadline_ms(), None);
        a.apply(Event::Correction { candidate_sha256: hash('b'), feedback_sha256: hash('d') }).unwrap();
        assert_eq!(a.remaining_ms(), 400);
        // A day of host checking does not spend the remaining worker allowance.
        start(&mut a, 86_400_000);
        assert_eq!(a.deadline_ms(), Some(86_400_400));
        submit(&mut a, 400);
        assert_eq!(a.remaining_ms(), 0);
        assert!(!a.can_correct());
        a.apply(Event::Finished).unwrap();
    }

    #[test]
    fn exact_exhaustion_still_allows_host_check_but_late_completion_does_not() {
        for elapsed in [1000, 1001] {
            let mut a = attempt(); start(&mut a, 10); let before = a.clone();
            let result = a.apply(Event::TurnCompleted { completion: completion(1, elapsed) });
            if elapsed == 1000 {
                result.unwrap(); assert_eq!(a.deadline_ms(), None);
                a.apply(Event::Sealed { candidate_sha256: hash('b') }).unwrap();
            } else { assert!(result.is_err()); assert_eq!(a, before); }
        }
    }

    #[test]
    fn bounded_submissions_and_infrastructure_retries_are_independent() {
        let mut a = attempt();
        for i in 1..=3 {
            start(&mut a, i * 100); submit(&mut a, 1);
            assert_eq!(a.submissions, i as u32);
            if i < 3 { a.apply(Event::Correction { candidate_sha256: hash('b'), feedback_sha256: hash('d') }).unwrap(); }
        }
        assert!(!a.can_correct());
        for _ in 0..2 { a.apply(Event::InfrastructureRetry { candidate_sha256: hash('b'), recovery_completion_sha256: None }).unwrap(); }
        assert!(a.apply(Event::InfrastructureRetry { candidate_sha256: hash('b'), recovery_completion_sha256: None }).is_err());
        assert_eq!(a.remaining_ms(), 997);
        assert_eq!(a.submissions, 3);
    }

    #[test]
    fn exact_session_and_process_survive_restart_and_stale_events_are_rejected() {
        let mut a = attempt(); start(&mut a, 1); submit(&mut a, 10);
        a.apply(Event::Correction { candidate_sha256: hash('b'), feedback_sha256: hash('d') }).unwrap();
        a = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        start(&mut a, 10000);
        for fault in ["thread", "model", "home", "turn", "process"] {
            let mut c = completion(2, 10);
            match fault {
                "thread" => c.session.thread_id = "fresh-context".into(),
                "model" => c.session.model = "different-model".into(),
                "home" => c.session.home = "/another-home".into(),
                "turn" => c.turn = 1,
                _ => c.process = process(1),
            }
            let before = a.clone();
            assert!(a.apply(Event::TurnCompleted { completion: c }).is_err(), "{fault}");
            assert_eq!(a, before);
        }
        submit(&mut a, 10);
        a.apply(Event::Finished).unwrap();
        assert!(a.apply(Event::Launched { process: process(3), started_at_ms: 20000 }).is_err());
    }

    #[test]
    fn exhausted_capture_spends_exact_remainder_and_cannot_authorize_correction() {
        let mut a = attempt();
        start(&mut a, 1); submit(&mut a, 300);
        a.apply(Event::Correction { candidate_sha256: hash('b'), feedback_sha256: hash('d') }).unwrap();
        start(&mut a, 1_000_000);
        let before = a.clone();
        for elapsed in [0, 699, 701] {
            let mut capture = completion(2, elapsed); capture.exhausted = true;
            assert!(a.apply(Event::TurnCompleted { completion: capture }).is_err());
            assert_eq!(a, before, "an invalid exhaustion receipt must be atomic");
        }
        let mut capture = completion(2, 700); capture.exhausted = true;
        a.apply(Event::TurnCompleted { completion: capture.clone() }).unwrap();
        let submitted = a.clone();
        a.apply(Event::TurnCompleted { completion: capture }).unwrap();
        assert_eq!(a, submitted, "replayed exhaustion must not debit twice");
        a.apply(Event::Sealed { candidate_sha256: hash('b') }).unwrap();
        assert_eq!(a.consumed_ms, 1000);
        assert_eq!(a.deadline_ms(), None);
        assert!(!a.can_correct());
        let checked = a.clone();
        assert!(a.apply(Event::Correction { candidate_sha256: hash('b'), feedback_sha256: hash('d') }).is_err());
        assert_eq!(a, checked);
        a.apply(Event::InfrastructureRetry { candidate_sha256: hash('b'), recovery_completion_sha256: None }).unwrap();
        assert_eq!(a.remaining_ms(), 0, "host retry must not replenish exhausted execution");
    }
    fn exhausted_checking() -> Attempt {
        let mut a = attempt(); start(&mut a, 100); submit(&mut a, 10);
        for _ in 0..a.policy.max_infrastructure_retries {
            a.apply(Event::InfrastructureRetry { candidate_sha256: hash('b'), recovery_completion_sha256: None }).unwrap();
        }
        a
    }

    fn recovery_grant() -> InfrastructureRecoveryGrant {
        InfrastructureRecoveryGrant { candidate_sha256: hash('b'), retry: 3,
            acceptance_round: 4, finalization: 3, completion_sha256: hash('e') }
    }

    #[test]
    fn recovery_grant_preserves_frozen_attempt_and_allows_exactly_one_retry() {
        let mut a = exhausted_checking();
        let original = a.clone(); let grant = recovery_grant();
        assert!(a.grant_infrastructure_recovery(grant.clone()).unwrap());
        let mut expected = original; expected.infrastructure_recovery_grant = Some(grant.clone());
        assert_eq!(a, expected);
        assert!(!a.grant_infrastructure_recovery(grant.clone()).unwrap());
        for (candidate, completion) in [(hash('b'), None), (hash('b'), Some(hash('f'))), (hash('c'), Some(hash('e')))] {
            assert!(a.apply(Event::InfrastructureRetry { candidate_sha256: candidate, recovery_completion_sha256: completion }).is_err());
            assert_eq!(a, expected);
        }
        a.apply(Event::InfrastructureRetry { candidate_sha256: hash('b'), recovery_completion_sha256: Some(hash('e')) }).unwrap();
        expected.infrastructure_retries = 3;
        assert_eq!(a, expected);
        assert!(!a.grant_infrastructure_recovery(grant).unwrap());
        assert!(a.apply(Event::InfrastructureRetry { candidate_sha256: hash('b'), recovery_completion_sha256: Some(hash('e')) }).is_err());
        assert_eq!(a, expected);
        a.apply(Event::Finished).unwrap(); a.validate().unwrap();
        assert!(a.infrastructure_recovery_grant.is_some(), "spent provenance survives completion");
    }

    #[test]
    fn recovery_grant_rejects_wrong_bindings_and_unexhausted_or_overflow_atomically() {
        let original = exhausted_checking();
        for fault in ["candidate", "hash", "round", "finalization", "retry", "phase", "remaining", "overflow", "existing"] {
            let mut a = original.clone(); let mut grant = recovery_grant();
            match fault {
                "candidate" => grant.candidate_sha256 = hash('c'),
                "hash" => grant.completion_sha256 = "not a digest".into(),
                "round" => grant.acceptance_round = 0,
                "finalization" => grant.finalization = 0,
                "retry" => grant.retry = 4,
                "phase" => a.phase = Phase::Finished,
                "remaining" => a.infrastructure_retries = 1,
                "overflow" => { a.policy.max_infrastructure_retries = u32::MAX; a.infrastructure_retries = u32::MAX; grant.retry = 0; },
                _ => { a.grant_infrastructure_recovery(grant.clone()).unwrap(); grant.completion_sha256 = hash('f'); },
            }
            let before = a.clone();
            assert!(a.grant_infrastructure_recovery(grant).is_err(), "{fault}");
            assert_eq!(a, before, "{fault}");
        }
        let mut zero = original; zero.policy.max_infrastructure_retries = 0; zero.infrastructure_retries = 0;
        let mut grant = recovery_grant(); grant.retry = 1;
        zero.grant_infrastructure_recovery(grant).unwrap();
        zero.apply(Event::InfrastructureRetry { candidate_sha256: hash('b'), recovery_completion_sha256: Some(hash('e')) }).unwrap();
        assert_eq!(zero.infrastructure_retries, 1);
    }

    #[test]
    fn recovery_grant_cannot_follow_a_correction_to_another_candidate() {
        for spent in [false, true] {
            let mut a = exhausted_checking();
            a.grant_infrastructure_recovery(recovery_grant()).unwrap();
            if spent { a.apply(Event::InfrastructureRetry { candidate_sha256: hash('b'), recovery_completion_sha256: Some(hash('e')) }).unwrap(); }
            a.apply(Event::Correction { candidate_sha256: hash('b'), feedback_sha256: hash('d') }).unwrap();
            start(&mut a, 200);
            a.apply(Event::TurnCompleted { completion: completion(2, 10) }).unwrap();
            a.apply(Event::Sealed { candidate_sha256: hash('c') }).unwrap();
            let before = a.clone();
            assert!(a.apply(Event::InfrastructureRetry { candidate_sha256: hash('c'), recovery_completion_sha256: Some(hash('e')) }).is_err());
            assert_eq!(a, before); a.validate().unwrap();
        }
    }

    #[test]
    fn recovery_grant_old_wire_and_attempt_digest_remain_exact() {
        let a = exhausted_checking();
        let old = serde_json::to_value(&a).unwrap();
        assert!(old.get("infrastructure_recovery_grant").is_none());
        let restored: Attempt = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), old);
        assert_eq!(a.digest().unwrap(), crate::trusted_artifact_rebind::digest(&old));
        let event = serde_json::json!({"kind":"infrastructure_retry","candidate_sha256":hash('b')});
        let decoded: Event = serde_json::from_value(event.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), event);
        let mut value = serde_json::to_value(recovery_grant()).unwrap();
        value["reset_budget"] = true.into();
        assert!(serde_json::from_value::<InfrastructureRecoveryGrant>(value).is_err());
    }

}

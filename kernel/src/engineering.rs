//! Pre-extraction engineering decisions. Filesystem operations remain host-owned.
use crate::engine::{ProtocolCommand, ProtocolEvent, TransitionError, TransitionOutcome};
use crate::model::{Phase, ProtocolState, RequestKind, Stage, WrapperRequest};
use crate::worker_attempt::{self, Completion, SerialAttempt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose { SourceReading, Plan, PlanReview, Implementation, CodeReview, Performance, RetryReview }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: String,
    pub allowance_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subject {
    pub route: crate::paper_implementation::PaperImplementationRoute,
    pub paper_sha256: String,
    pub targets_sha256: String,
    pub contract_sha256: String,
    pub notes_sha256: Option<String>,
    pub options_sha256: String,
    pub tools_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanEntry { pub source: String, pub description: String, pub consequences: String }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub requirements: Vec<PlanEntry>,
    pub interpretations: Vec<PlanEntry>,
    pub unspecified: Vec<PlanEntry>,
    pub departures: Vec<PlanEntry>,
}
impl Plan {
    pub fn validate(&self) -> Result<(), String> {
        if self.requirements.is_empty() { return Err("requirements must be nonempty".into()); }
        for entry in self.requirements.iter().chain(&self.interpretations).chain(&self.unspecified).chain(&self.departures) {
            for text in [&entry.source, &entry.description, &entry.consequences] {
                if text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)).is_empty() {
                    return Err("record text must be nonempty".into());
                }
            }
        }
        Ok(())
    }
}

/// Exact current source/build identities are checked again by the host before publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub source_sha256: String,
    pub build_sha256: String,
    pub lock_sha256: String,
    pub tools_sha256: String,
    pub plan_sha256: String,
    pub checks: Vec<Check>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub argv: Vec<String>,
    pub exit_code: i32,
    pub basis_sha256: String,
    pub receipt_sha256: String,
}
impl Candidate {
    pub fn basis(&self) -> String {
        hash(&(&self.source_sha256, &self.build_sha256, &self.lock_sha256, &self.tools_sha256, &self.plan_sha256))
    }
    fn validate(&self, subject: &Subject, plan: &str) -> Result<(), String> {
        for id in [&self.source_sha256, &self.build_sha256, &self.lock_sha256, &self.tools_sha256, &self.plan_sha256] { digest(id)?; }
        if self.tools_sha256 != subject.tools_sha256 || self.plan_sha256 != plan || self.checks.len() != 2 {
            return Err("candidate requires current tools, plan and both Cargo checks".into());
        }
        for (check, command) in self.checks.iter().zip(["check", "test"]) {
            if check.argv != ["cargo", command, "--locked", "--offline", "--all-targets"]
                || check.exit_code != 0 || check.basis_sha256 != self.basis() {
                return Err("candidate requires successful current Cargo check/test --locked".into());
            }
            digest(&check.receipt_sha256)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    pub candidate_sha256: String,
    pub reviewed_candidate_sha256: String,
    pub harness_sha256: String,
    pub record_sha256: String,
    pub exit_codes: Vec<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewedCandidate {
    pub candidate: Candidate,
    pub review_sha256: String,
    pub subject_sha256: String,
    pub generation: u32,
    pub reading_sha256: String,
    pub plan_approval_sha256: String,
    pub plan_approval_basis_sha256: String,
}
impl ReviewedCandidate {
    fn validate(&self, subject: &Subject) -> Result<(), String> {
        if self.subject_sha256 != hash(subject) || self.generation == 0 { return Err("retained candidate inputs changed".into()); }
        for id in [&self.review_sha256, &self.reading_sha256, &self.plan_approval_sha256] { digest(id)?; }
        if self.plan_approval_basis_sha256 != hash(&(subject, self.generation, Some(&self.reading_sha256), Some(&self.candidate.plan_sha256))) {
            return Err("retained candidate approval basis differs".into());
        }
        self.candidate.validate(subject, &self.candidate.plan_sha256)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssignmentFeedback {
    pub checked_sha256: String,
    pub outcome: Outcome,
    pub prior_failure: Option<RetryReview>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub reviewed_candidate_sha256: String,
    pub harness_sha256: String,
    pub assessment_sha256: String,
    pub measurement_sha256: Vec<String>,
    pub selection_receipt_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consumption {
    pub request_id: u32,
    pub turn: u32,
    pub request_sha256: String,
    pub candidate_sha256: String,
    pub checked_sha256: String,
    pub predecessor_sha256: Option<String>,
}

/// One explicit operator transfer to the existing preparation checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    pub predecessor_sha256: String,
    pub baseline_sha256: String,
    pub record_sha256: String,
    pub downstream_commit: String,
    pub downstream_state_sha256: String,
    pub downstream_source_sha256: String,
}
impl Handoff {
    pub fn validate(&self) -> Result<(), String> {
        for id in [&self.predecessor_sha256, &self.baseline_sha256, &self.record_sha256,
            &self.downstream_state_sha256, &self.downstream_source_sha256] { digest(id)?; }
        if ![40, 64].contains(&self.downstream_commit.len()) ||
            !self.downstream_commit.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
            return Err("engineering handoff requires an exact downstream checkpoint".into());
        }
        Ok(())
    }
}

/// Live parent-owned directories, inherited only by the terminal checkpoint hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryPin {
    pub path: std::path::PathBuf,
    pub fd: i32,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointGuard {
    pub request_id: u32,
    pub turn: u32,
    pub request_sha256: String,
    pub candidate_sha256: String,
    pub checked_sha256: String,
    pub roots: [DirectoryPin; 2],
}

impl CheckpointGuard {
    pub fn validate(&self, checked: &Checked) -> Result<(), String> {
        if self.request_id != checked.request_id || self.turn != checked.turn
            || self.request_sha256 != checked.request_sha256
            || self.candidate_sha256 != checked.candidate_sha256
            || self.checked_sha256 != hash(checked)
            || self.roots[0].fd < 3 || self.roots[1].fd < 3
            || self.roots[0].fd == self.roots[1].fd
            || !self.roots[0].path.is_absolute()
            || self.roots[1].path != self.roots[0].path.join(".trellis") {
            return Err("engineering checkpoint requires original request/directory ownership".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub subject: Subject,
    pub profiles: BTreeMap<Purpose, Profile>,
    pub purpose: Purpose,
    pub generation: u32,
    pub reading_sha256: Option<String>,
    pub plan_sha256: Option<String>,
    pub plan_approval_sha256: Option<String>,
    pub plan_approval_basis_sha256: Option<String>,
    pub candidate: Option<Candidate>,
    pub reviewed: BTreeMap<String, ReviewedCandidate>,
    pub current_reviewed_candidate_sha256: Option<String>,
    pub assignment_feedback: Option<AssignmentFeedback>,
    pub measurements: BTreeMap<String, Measurement>,
    pub baseline: Option<Baseline>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<Handoff>,
    pub completed: Option<Completion>,
    pub last_consumption: Option<Consumption>,
    pub halt: Option<Halt>,
    pub failed: bool,
    pub feedback: Option<serde_json::Value>,
    pub retry_review: Option<RetryReview>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryReview {
    pub failed_purpose: Purpose,
    pub basis_sha256: String,
    pub request_sha256: String,
    pub candidate_sha256: String,
    pub checked_sha256: String,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Halt { Interrupted, Expired }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub purpose: Purpose,
    pub profile: Profile,
    pub subject: Subject,
    pub generation: u32,
    pub basis_sha256: String,
    pub reading_sha256: Option<String>,
    pub plan_sha256: Option<String>,
    pub plan_approval_sha256: Option<String>,
    pub candidate_sha256: Option<String>,
    pub output_paths: Vec<String>,
    pub reviewed_candidate_sha256: Option<String>,
    pub assignment_feedback: Option<AssignmentFeedback>,
    pub retry_review: Option<RetryReview>,
    pub artifact_contract: serde_json::Value,
    pub artifact_contract_sha256: String,
    pub checker_sha256: String,
}

/// Finite engineering artifact shapes; this data never authorizes a transition.
pub fn artifact_contract(purpose: Purpose) -> serde_json::Value {
    use crate::artifact_validation::{PREPARATION_PERFORMANCE_MAX_ARGUMENTS,
        PREPARATION_PERFORMANCE_MAX_COMMANDS, PREPARATION_PERFORMANCE_MAX_TIMEOUT_SECONDS};
    let mut contract = serde_json::json!({
        "version": 1, "purpose": purpose,
        "record_path": match purpose {
            Purpose::SourceReading => Some("reading.json"),
            Purpose::Plan | Purpose::Implementation => Some("plan.json"), _ => None,
        },
        "record_fields": ["requirements", "interpretations", "unspecified", "departures"],
        "entry_fields": ["source", "description", "consequences"], "nonempty_list": "requirements",
        "summary_fields": ["summary"], "review_fields": ["decision", "reason"],
        "decisions": if purpose == Purpose::RetryReview { vec!["revise", "fail"] }
            else { vec!["accept", "revise", "reject"] },
        "performance_fields": {
            "measure": ["action", "reason", "commands"], "improve": ["action", "reason", "instruction"],
            "accept": ["action", "reason", "evidence", "report"], "fail": ["action", "reason"]
        },
        "command_fields": ["name", "argv", "cwd", "kind"], "command_kinds": ["build", "run"],
        "command_roots": ["implementation", "harness", "build", "output"],
        "max_commands": PREPARATION_PERFORMANCE_MAX_COMMANDS,
        "max_arguments": PREPARATION_PERFORMANCE_MAX_ARGUMENTS,
        "max_timeout_seconds": PREPARATION_PERFORMANCE_MAX_TIMEOUT_SECONDS,
        "identifier_bytes": 128, "report_prefix": "benchmarks/"
    });
    if purpose == Purpose::Performance {
        contract["performance_fields"]["reset_budget"] = serde_json::json!([
            "action", "reason", "baseline_evidence", "current_evidence", "metric", "improvement_bps"]);
    }
    contract
}

pub fn checker_sha256() -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(include_bytes!("../../trellis/engineering_check.py")))
}

pub fn ready_turn(state: &ProtocolState) -> Result<serde_json::Value, String> {
    state.validate()?;
    if state.phase != Phase::Engineering || state.engineering.as_ref().is_none_or(|e| e.halt.is_some() || e.failed || e.baseline.is_some()) {
        return Err("engineering turn requires an active native assignment".into());
    }
    let request = state.in_flight_request.as_ref().ok_or("engineering request is absent")?;
    let attempt = &state.worker_attempt.as_ref().ok_or("engineering attempt is absent")?.attempt;
    let feedback_sha256 = match &attempt.phase {
        worker_attempt::Phase::Ready { feedback_sha256 } => feedback_sha256.clone(),
        worker_attempt::Phase::TransportReady if attempt.turn == 1 => None,
        worker_attempt::Phase::TransportReady => {
            let feedback = state.engineering.as_ref().unwrap().feedback.as_ref()
                .ok_or("engineering transport lost its checker feedback")?;
            Some(hash(feedback))
        }
        _ => return Err("engineering turn is not ready".into()),
    };
    let mut turn = serde_json::json!({"engineering_request": request, "engineering_attempt": attempt,
        "turn": attempt.turn, "remaining_ms": attempt.allowance_ms - attempt.consumed_ms,
        "session": attempt.session, "feedback_sha256": feedback_sha256});
    if matches!(attempt.phase, worker_attempt::Phase::TransportReady) {
        turn["transport_ordinal"] = serde_json::json!(attempt.transport_retries);
        turn["transport_continuation"] = serde_json::json!(true);
    }
    Ok(turn)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCheck {
    pub ok: bool,
    pub data: Option<serde_json::Value>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn check_artifact_text(purpose: Purpose, raw: &str, record: Option<&str>) -> ArtifactCheck {
    let fail = |path: &str| ArtifactCheck { ok: false, data: None,
        diagnostics: vec![Diagnostic { path: path.into(), rule: DiagnosticRule::Type }] };
    let Ok(raw) = serde_json::from_str(raw) else { return fail("result"); };
    let record = match record.map(serde_json::from_str) {
        Some(Ok(record)) => Some(record),
        Some(Err(_)) => return fail(if purpose == Purpose::SourceReading { "reading.json" } else { "plan.json" }),
        None => None,
    };
    check_artifacts(purpose, &raw, record.as_ref())
}

pub fn checked_event(state: &ProtocolState, checked: Checked) -> Result<ProtocolEvent, String> {
    validate_state(state)?;
    checked_binding(state, &checked)?;
    if matches!(checked.outcome, Outcome::Malformed { .. })
        && state.worker_attempt.as_ref().ok_or("engineering attempt missing")?.attempt.can_correct() {
        Ok(ProtocolEvent::EngineeringChecked { checked: Box::new(checked) })
    } else {
        Ok(ProtocolEvent::EngineeringOutcome { checked: Box::new(checked) })
    }
}

pub fn check_artifacts(purpose: Purpose, raw: &serde_json::Value, record: Option<&serde_json::Value>) -> ArtifactCheck {
    use serde_json::json;
    let fail = |path: &str, rule| ArtifactCheck { ok: false, data: None,
        diagnostics: vec![Diagnostic { path: path.into(), rule }] };
    let Some(obj) = raw.as_object() else { return fail("result", DiagnosticRule::Type); };
    let data = if purpose == Purpose::Performance {
        if obj.get("action").and_then(|v| v.as_str()).is_some_and(|v| v.trim().eq_ignore_ascii_case("reset_budget")) {
            let Some(reset) = reset_artifact(raw) else { return fail("result", DiagnosticRule::Type); };
            reset
        } else {
            let checked = crate::artifact_validation::validate_preparation_performance_result_data(raw);
            if !checked.ok { return fail("result", DiagnosticRule::Type); }
            checked.data.expect("validated performance artifact")
        }
    } else if matches!(purpose, Purpose::PlanReview | Purpose::CodeReview | Purpose::RetryReview) {
        if obj.len() != 2 || !obj.contains_key("decision") || !obj.contains_key("reason") {
            return fail("result", DiagnosticRule::ClosedFields);
        }
        let (Some(decision), Some(reason)) = (obj["decision"].as_str(), obj["reason"].as_str()) else { return fail("result", DiagnosticRule::Type); };
        let decision = decision.trim().to_ascii_lowercase();
        let allowed = if purpose == Purpose::RetryReview { &["revise", "fail"][..] } else { &["accept", "revise", "reject"][..] };
        if !allowed.contains(&decision.as_str()) { return fail("result.decision", DiagnosticRule::Enum); }
        if reason.trim().is_empty() { return fail("result.reason", DiagnosticRule::Nonempty); }
        json!({"decision": decision, "reason": reason.trim()})
    } else {
        if obj.len() != 1 || !obj.contains_key("summary") { return fail("result", DiagnosticRule::ClosedFields); }
        let Some(summary) = obj["summary"].as_str() else { return fail("result.summary", DiagnosticRule::Type); };
        if summary.trim().is_empty() { return fail("result.summary", DiagnosticRule::Nonempty); }
        json!({"summary": summary.trim()})
    };
    let contract = artifact_contract(purpose);
    if let Some(path) = contract["record_path"].as_str() {
        let Some(record) = record else { return fail(path, DiagnosticRule::Required); };
        let Ok(plan) = serde_json::from_value::<Plan>(record.clone()) else { return fail(path, DiagnosticRule::Type); };
        if plan.validate().is_err() { return fail(path, DiagnosticRule::Nonempty); }
    }
    ArtifactCheck { ok: true, data: Some(data), diagnostics: vec![] }
}

fn reset_artifact(raw: &serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::json;
    let obj = raw.as_object()?;
    let fields = ["action", "reason", "baseline_evidence", "current_evidence", "metric", "improvement_bps"];
    if fields.iter().any(|key| !obj.contains_key(*key))
        || obj.keys().any(|key| !fields.contains(&key.as_str()) && key != "system_feedback") { return None; }
    if let Some(feedback) = obj.get("system_feedback").filter(|v| !v.is_null()) { feedback.as_str()?; }
    let reason = obj["reason"].as_str()?.trim(); let metric = obj["metric"].as_str()?.trim();
    if reason.is_empty() || metric.is_empty() { return None; }
    let gain = obj["improvement_bps"].as_u64()?;
    if gain < 1000 || gain > u32::MAX as u64 { return None; }
    let ids = |key: &str| -> Option<Vec<String>> {
        let mut result = Vec::new();
        for value in obj[key].as_array()? {
            let id = value.as_str()?.trim();
            if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || result.iter().any(|prior| prior == id) { return None; }
            result.push(id.to_string());
        }
        if result.is_empty() { None } else { Some(result) }
    };
    Some(json!({"action":"reset_budget", "reason":reason, "metric":metric, "improvement_bps":gain,
        "baseline_evidence":ids("baseline_evidence")?, "current_evidence":ids("current_evidence")?}))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision { Accept, Revise, Reject, Fail }

/// Produced by the trusted checker after validating the sealed candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Malformed { diagnostics: Vec<Diagnostic> },
    Record { summary: String, record: Plan },
    Review { decision: Decision, reason: String },
    Implementation { summary: String, candidate: Candidate },
    Measure { reason: String, measurement: Measurement },
    Improve { reason: String, instruction: String },
    /// The host derives the round ceiling; the kernel validates this typed declaration and its measured candidates.
    ResetBudget { reason: String, baseline_measurement_sha256: Vec<String>, current_measurement_sha256: Vec<String>,
        metric: String, improvement_bps: u32 },
    Accept { reason: String, baseline: Baseline },
    Fail { reason: String },
    /// Proposed departures return to planning before any new source is adopted.
    Propose { reason: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic { pub path: String, pub rule: DiagnosticRule }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticRule { Required, ClosedFields, Type, Nonempty, Enum, Stale, FailedCheck }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checked {
    pub request_id: u32,
    pub cycle: u32,
    pub request_sha256: String,
    pub turn: u32,
    pub candidate_sha256: String,
    pub basis_sha256: String,
    pub host_receipt_sha256: String,
    pub outcome: Outcome,
}

pub fn hash(value: &impl Serialize) -> String {
    crate::trusted_artifact_rebind::digest(&serde_json::to_value(value).expect("typed engineering value"))
}
fn digest(id: &str) -> Result<(), String> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err("engineering evidence requires a canonical SHA-256 identity".into());
    }
    Ok(())
}
fn nonempty(text: &str) -> Result<(), String> {
    if text.trim().is_empty() { return Err("engineering text must be nonempty".into()); }
    Ok(())
}

impl Subject {
    fn validate(&self) -> Result<(), String> {
        if self.route != crate::paper_implementation::PaperImplementationRoute::RustAeneas { return Err("original-paper engineering requires the Rust route".into()); }
        for id in [&self.paper_sha256, &self.targets_sha256, &self.contract_sha256, &self.options_sha256, &self.tools_sha256] { digest(id)?; }
        if let Some(notes) = &self.notes_sha256 { digest(notes)?; }
        Ok(())
    }
}
impl State {
    pub fn basis(&self) -> String {
        hash(&(&self.subject, self.generation, &self.reading_sha256, &self.plan_sha256,
            &self.plan_approval_sha256, &self.candidate, &self.current_reviewed_candidate_sha256, &self.reviewed, &self.measurements))
    }
    fn contract(&self) -> Result<Contract, String> {
        let profile = self.profiles.get(&if self.purpose == Purpose::RetryReview { Purpose::PlanReview } else { self.purpose }).ok_or("engineering role profile missing")?.clone();
        let output_paths = match self.purpose {
            Purpose::SourceReading => vec!["reading.json"],
            Purpose::Plan => vec!["plan.json"],
            Purpose::Implementation => vec!["source", "outputs"],
            Purpose::Performance => vec!["benchmarks/harness", "benchmarks/report.md"],
            Purpose::PlanReview | Purpose::CodeReview | Purpose::RetryReview => vec![],
        }.into_iter().map(String::from).collect();
        let artifact_contract = artifact_contract(self.purpose);
        Ok(Contract { purpose: self.purpose, profile, subject: self.subject.clone(), generation: self.generation,
            basis_sha256: self.basis(), reading_sha256: self.reading_sha256.clone(), plan_sha256: self.plan_sha256.clone(),
            plan_approval_sha256: self.plan_approval_sha256.clone(), candidate_sha256: self.candidate.as_ref().map(hash), output_paths, retry_review: self.retry_review.clone(), reviewed_candidate_sha256: self.current_reviewed_candidate_sha256.clone(), assignment_feedback: self.assignment_feedback.clone(),
            artifact_contract_sha256: hash(&artifact_contract), artifact_contract, checker_sha256: checker_sha256() })
    }
    fn plan_basis(&self) -> String { hash(&(&self.subject, self.generation, &self.reading_sha256, &self.plan_sha256)) }
    fn require_plan(&self) -> Result<&str, String> {
        if self.plan_approval_sha256.is_none() || self.plan_approval_basis_sha256.as_deref() != Some(&self.plan_basis()) { return Err("engineering requires an approved current plan".into()); }
        self.plan_sha256.as_deref().ok_or("engineering plan is absent".into())
    }
    fn reopen_plan(&mut self) -> Result<(), String> {
        self.generation = self.generation.checked_add(1).ok_or("engineering generation overflow")?;
        self.plan_approval_sha256 = None;
        self.plan_approval_basis_sha256 = None;
        self.current_reviewed_candidate_sha256 = None;
        self.purpose = Purpose::Plan;
        Ok(())
    }
}

pub fn initialize(subject: Subject, profiles: BTreeMap<Purpose, Profile>, policy: worker_attempt::Policy) -> Result<ProtocolState, String> {
    subject.validate()?;
    policy.validate()?;
    let purposes = [Purpose::SourceReading, Purpose::Plan, Purpose::PlanReview, Purpose::Implementation, Purpose::CodeReview, Purpose::Performance];
    if profiles.len() != purposes.len() || purposes.iter().any(|p| !profiles.contains_key(p)) {
        return Err("engineering requires every purpose's configured role profile".into());
    }
    for profile in profiles.values() {
        for value in [&profile.provider, &profile.model, &profile.reasoning_effort] { nonempty(value)?; }
        if profile.allowance_ms == 0 { return Err("engineering allowance must be positive".into()); }
    }
    let state = ProtocolState { phase: Phase::Engineering, stage: Stage::Engineering, worker_submission_policy: Some(policy),
        engineering: Some(State { subject, profiles, purpose: Purpose::SourceReading, generation: 1,
            reading_sha256: None, plan_sha256: None, plan_approval_sha256: None, plan_approval_basis_sha256: None, candidate: None,
            reviewed: BTreeMap::new(), current_reviewed_candidate_sha256: None, assignment_feedback: None, measurements: BTreeMap::new(), baseline: None, completed: None,
            last_consumption: None, handoff: None, halt: None, failed: false, feedback: None, retry_review: None }), ..ProtocolState::default() };
    validate_state(&state)?;
    Ok(state)
}

pub fn validate_state(state: &ProtocolState) -> Result<(), String> {
    if state.phase != Phase::Engineering {
        if state.engineering.is_some() { return Err("engineering state belongs to its pre-extraction phase".into()); }
        return Ok(());
    }
    let eng = state.engineering.as_ref().ok_or("engineering phase lacks its subject")?;
    eng.subject.validate()?;
    if eng.generation == 0 { return Err("engineering generation must be positive".into()); }
    for purpose in [Purpose::SourceReading, Purpose::Plan, Purpose::PlanReview, Purpose::Implementation, Purpose::CodeReview, Purpose::Performance] {
        let profile = eng.profiles.get(&purpose).ok_or("engineering profile missing")?;
        for text in [&profile.provider, &profile.model, &profile.reasoning_effort] { nonempty(text)?; }
        if profile.allowance_ms == 0 { return Err("engineering allowance must be positive".into()); }
    }
    if eng.profiles.len() != 6 { return Err("unexpected engineering role profile".into()); }
    for value in [&eng.reading_sha256, &eng.plan_sha256, &eng.plan_approval_sha256, &eng.plan_approval_basis_sha256] {
        if let Some(id) = value { digest(id)?; }
    }
    if eng.plan_approval_sha256.is_some() { eng.require_plan()?; }
    else if eng.plan_approval_basis_sha256.is_some() { return Err("plan approval basis has no approval".into()); }
    for (id, retained) in &eng.reviewed {
        retained.validate(&eng.subject)?;
        if id != &hash(retained) { return Err("retained reviewed-context identity differs".into()); }
    }
    if let Some(id) = &eng.current_reviewed_candidate_sha256 {
        let retained = eng.reviewed.get(id).ok_or("current reviewed candidate is missing")?;
        if Some(&retained.candidate) != eng.candidate.as_ref()
            || Some(&retained.plan_approval_sha256) != eng.plan_approval_sha256.as_ref()
            || retained.generation != eng.generation || Some(&retained.reading_sha256) != eng.reading_sha256.as_ref() {
            return Err("current reviewed pointer differs from the active candidate and plan approval".into());
        }
    }
    if let Some(feedback) = &eng.assignment_feedback {
        if eng.last_consumption.as_ref().is_none_or(|c| c.checked_sha256 != feedback.checked_sha256)
            || !matches!(feedback.outcome, Outcome::Malformed { .. } | Outcome::Review { .. } | Outcome::Improve { .. } | Outcome::Propose { .. }) {
            return Err("successor instruction has no consumed checked origin".into());
        }
    }
    if !matches!(state.stage, Stage::Engineering | Stage::EngineeringReady) || state.dispatch.is_some()
        || state.worker_execution.is_some() || state.modeling_human_approved || state.paper_implementation.is_some()
        || state.paper_implementation_approved_sha256.is_some() || !state.live.present_nodes.is_empty() {
        return Err("engineering cannot carry formal, parallel or human-approval state".into());
    }
    if (eng.failed || eng.baseline.is_some()) && (state.in_flight_request.is_some() || state.worker_attempt.is_some()) {
        return Err("terminal engineering state retains an assignment".into());
    }
    if (state.stage == Stage::EngineeringReady) != eng.baseline.is_some() {
        return Err("engineering readiness requires the selected baseline".into());
    }
    if let Some(handoff) = &eng.handoff {
        handoff.validate()?;
        if eng.baseline.as_ref().map(hash).as_ref() != Some(&handoff.baseline_sha256) {
            return Err("engineering transfer baseline changed".into());
        }
    }
    // The pre-extraction state has no mathematical evidence or gate authority.
    let mut unrelated = state.clone();
    unrelated.engineering = None;
    unrelated.phase = Phase::TheoremStating; unrelated.stage = Stage::Start;
    unrelated.cycle = 0; unrelated.request_seq = 0; unrelated.attempt = 0;
    unrelated.worker_attempt = None; unrelated.in_flight_request = None;
    unrelated.worker_submission_policy = None;
    unrelated.retry_outcome_kind = crate::model::RetryOutcomeKind::None; unrelated.invalid_attempt = false;
    if unrelated != ProtocolState::default() { return Err("engineering contains unrelated formal state".into()); }
    if let Some(review) = &eng.retry_review {
        if review.failed_purpose == Purpose::RetryReview || review.basis_sha256 != eng.basis()
            || review.diagnostics.is_empty() || eng.purpose != Purpose::RetryReview {
            return Err("retry review differs from the exhausted assignment".into());
        }
        for id in [&review.request_sha256, &review.candidate_sha256, &review.checked_sha256] { digest(id)?; }
    } else if eng.purpose == Purpose::RetryReview { return Err("retry review requires an exhausted assignment".into()); }
    state.worker_submission_policy.as_ref().ok_or("engineering submission policy missing")?.validate()?;
    if let Some(request) = &state.in_flight_request {
        if **request != expected_request(state, state.request_seq)? || state.stage != Stage::Engineering {
            return Err("engineering request differs from the current native contract".into());
        }
    }
    if let Some(record) = &state.worker_attempt {
        record.attempt.validate()?;
        let request = state.in_flight_request.as_ref().ok_or("engineering attempt has no retained request")?;
        if record.request_id != request.id || record.attempt.contract_sha256 != hash(&**request) {
            return Err("engineering attempt differs from its request".into());
        }
        match &record.attempt.phase {
            worker_attempt::Phase::Finished => {
                if eng.halt != Some(Halt::Interrupted) || eng.completed.is_some() || record.attempt.last_interruption.is_none()
                    || (record.attempt.remaining_ms() > 0
                        && record.attempt.infrastructure_retries < record.attempt.policy.max_infrastructure_retries) {
                    return Err("engineering finished transport requires its exhausted halt".into());
                }
            }
            worker_attempt::Phase::TransportReady if eng.halt.is_some() => {
                return Err("halted engineering cannot authorize transport replay".into());
            }
            worker_attempt::Phase::Checking { .. } | worker_attempt::Phase::Submitted { .. } => {
                let completion = eng.completed.as_ref().ok_or("engineering completion evidence missing")?;
                if completion.turn != record.attempt.turn || completion.exhausted || Some(&completion.session) != record.attempt.session.as_ref() {
                    return Err("engineering completion differs from its attempt".into());
                }
                if let worker_attempt::Phase::Submitted { completion: retained } = &record.attempt.phase {
                    if retained != completion { return Err("engineering completion evidence differs".into()); }
                }
            }
            _ if eng.completed.is_some() => return Err("engineering completion has no submitted turn".into()),
            _ => {},
        }
        if let Some(session) = &record.attempt.session {
            let profile = &request.engineering.as_ref().unwrap().profile;
            if session.provider != profile.provider || session.model != profile.model || session.reasoning_effort != profile.reasoning_effort {
                return Err("engineering session differs from its role profile".into());
            }
        }
    } else if eng.completed.is_some() {
        return Err("engineering completion has no attempt".into());
    }
    Ok(())
}

/// Slice 1 has no provider adapter. Both production adapter entry points call this guard.
pub fn require_dispatch_supported(request: &WrapperRequest) -> Result<(), String> {
    if request.kind == RequestKind::Engineering || request.phase == Phase::Engineering || request.engineering.is_some() {
        return Err("native engineering provider transport is not installed".into());
    }
    Ok(())
}

pub fn expected_request(state: &ProtocolState, id: u32) -> Result<WrapperRequest, String> {
    let eng = state.engineering.as_ref().ok_or("engineering subject missing")?;
    Ok(WrapperRequest { id, cycle: state.cycle, kind: RequestKind::Engineering,
        phase: Phase::Engineering, engineering: Some(eng.contract()?), fresh_context: true,
        acceptance_logic_identity: crate::model::acceptance_logic_identity(),
        worker_submission_policy: state.worker_submission_policy.clone(), ..WrapperRequest::default() })
}

fn issue(state: &mut ProtocolState) -> Result<Vec<ProtocolCommand>, String> {
    let eng = state.engineering.as_ref().ok_or("engineering subject missing")?;
    if state.stage != Stage::Engineering || eng.halt.is_some() || eng.failed || eng.baseline.is_some()
        || state.in_flight_request.is_some() || state.worker_attempt.is_some() {
        return Err("engineering issuance requires an idle active assignment".into());
    }
    match eng.purpose {
        Purpose::Plan if eng.reading_sha256.is_none() => return Err("planning requires independent source reading".into()),
        Purpose::PlanReview if eng.plan_sha256.is_none() => return Err("plan review requires a candidate plan".into()),
        Purpose::Implementation | Purpose::CodeReview | Purpose::Performance => {
            eng.require_plan()?;
            if eng.purpose != Purpose::Implementation {
                let candidate = eng.candidate.as_ref().ok_or("engineering review requires a candidate")?;
                candidate.validate(&eng.subject, eng.require_plan()?)?;
                if eng.purpose == Purpose::Performance && eng.current_reviewed_candidate_sha256.is_none() {
                    return Err("performance requires current independent code review".into());
                }
            }
        }
        _ => {}
    }
    state.cycle = state.cycle.checked_add(1).ok_or("engineering cycle overflow")?;
    state.request_seq = state.request_seq.checked_add(1).ok_or("engineering request overflow")?;
    let request = expected_request(state, state.request_seq)?;
    state.in_flight_request = Some(Box::new(request.clone()));
    Ok(vec![ProtocolCommand::IssueRequest { request: Box::new(request) }])
}

fn checked_binding(state: &ProtocolState, checked: &Checked) -> Result<(), String> {
    let eng = state.engineering.as_ref().ok_or("engineering subject missing")?;
    let request = state.in_flight_request.as_ref().ok_or("engineering result has no request")?;
    let attempt = &state.worker_attempt.as_ref().ok_or("engineering result has no attempt")?.attempt;
    if eng.halt.is_some() || eng.completed.as_ref().is_none_or(|c| c.turn != attempt.turn || c.exhausted)
        || checked.cycle != request.cycle || checked.request_id != request.id || checked.request_sha256 != hash(&**request)
        || checked.turn != attempt.turn || checked.basis_sha256 != eng.basis()
        || !matches!(&attempt.phase, worker_attempt::Phase::Checking { candidate_sha256 } if candidate_sha256 == &checked.candidate_sha256) {
        return Err("engineering result requires its completed sealed current attempt".into());
    }
    digest(&checked.host_receipt_sha256)?;
    Ok(())
}

fn observe(state: &mut ProtocolState, request_id: u32, observation: worker_attempt::Event) -> Result<(), String> {
    let request = state.in_flight_request.as_ref().filter(|r| r.id == request_id).ok_or("engineering observation has no request")?;
    let contract = request.engineering.as_ref().ok_or("engineering contract missing")?;
    let eng = state.engineering.as_mut().ok_or("engineering state missing")?;
    if eng.halt.is_some() { return Err("engineering execution is halted".into()); }
    let attempt = &mut state.worker_attempt.as_mut().filter(|a| a.request_id == request_id).ok_or("engineering attempt missing")?.attempt;
    match &observation {
        worker_attempt::Event::Interrupted { interruption } => {
            if interruption.turn != attempt.turn { return Err("interruption differs from the active turn".into()); }
            attempt.apply(observation.clone())?;
            if matches!(attempt.phase, worker_attempt::Phase::Finished) { eng.halt = Some(Halt::Interrupted); }
            return Ok(());
        }
        worker_attempt::Event::Expired { .. } => { let mut checked = attempt.clone(); checked.apply(observation.clone())?; eng.halt = Some(Halt::Expired); return Ok(()); }
        worker_attempt::Event::Finished | worker_attempt::Event::Correction { .. } => return Err("engineering completion and correction require checked outcomes".into()),
        worker_attempt::Event::TurnCompleted { completion } => {
            if completion.exhausted { let mut checked = attempt.clone(); checked.apply(observation.clone())?; eng.halt = Some(Halt::Expired); return Ok(()); }
            let profile = &contract.profile;
            if completion.session.provider != profile.provider || completion.session.model != profile.model
                || completion.session.reasoning_effort != profile.reasoning_effort {
                return Err("engineering completion changed its configured role".into());
            }
            attempt.apply(observation.clone())?;
            eng.completed = Some(completion.clone());
            return Ok(());
        }
        _ => {}
    }
    attempt.apply(observation)
}

fn apply_outcome(eng: &mut State, checked: &Checked) -> Result<(), String> {
    match (&eng.purpose, &checked.outcome) {
        (Purpose::RetryReview, Outcome::Review { decision, reason }) => {
            nonempty(reason)?;
            let review = eng.retry_review.as_ref().ok_or("retry review context missing")?;
            if review.basis_sha256 != eng.basis() { return Err("retry review basis changed".into()); }
            match decision {
                Decision::Revise => { eng.purpose = review.failed_purpose; eng.retry_review = None; }
                Decision::Fail => { eng.failed = true; }
                _ => return Err("retry review permits revise or fail".into()),
            }
        }

        (Purpose::SourceReading | Purpose::Plan, Outcome::Record { summary, record }) => {
            nonempty(summary)?; record.validate()?;
            if eng.purpose == Purpose::SourceReading { eng.reading_sha256 = Some(hash(record)); eng.purpose = Purpose::Plan; }
            else { eng.plan_sha256 = Some(hash(record)); eng.plan_approval_sha256 = None; eng.purpose = Purpose::PlanReview; }
        }
        (Purpose::PlanReview | Purpose::CodeReview, Outcome::Review { decision, reason }) => {
            nonempty(reason)?;
            match decision {
                Decision::Accept if eng.purpose == Purpose::PlanReview => {
                    if eng.plan_sha256.is_none() || eng.reading_sha256.is_none() { return Err("plan acceptance requires its reading and plan".into()); }
                    eng.plan_approval_sha256 = Some(hash(checked)); eng.plan_approval_basis_sha256 = Some(eng.plan_basis()); eng.purpose = Purpose::Implementation;
                }
                Decision::Accept => {
                    let candidate = eng.candidate.as_ref().ok_or("code review requires a candidate")?;
                    candidate.validate(&eng.subject, eng.require_plan()?)?;
                    let retained = ReviewedCandidate { candidate: candidate.clone(), review_sha256: hash(checked),
                        subject_sha256: hash(&eng.subject), generation: eng.generation, reading_sha256: eng.reading_sha256.clone().unwrap(),
                        plan_approval_sha256: eng.plan_approval_sha256.clone().unwrap(), plan_approval_basis_sha256: eng.plan_approval_basis_sha256.clone().unwrap() };
                    let identity = hash(&retained);
                    eng.reviewed.insert(identity.clone(), retained);
                    eng.current_reviewed_candidate_sha256 = Some(identity);
                    eng.purpose = Purpose::Performance;
                }
                Decision::Revise if eng.purpose == Purpose::CodeReview => { eng.current_reviewed_candidate_sha256 = None; eng.purpose = Purpose::Implementation; }
                Decision::Fail => return Err("plan/code review permits accept, revise or reject".into()),
                _ => { eng.reopen_plan()?; }
            }
        }
        (Purpose::Implementation, Outcome::Implementation { summary, candidate }) => {
            nonempty(summary)?;
            candidate.validate(&eng.subject, eng.require_plan()?)?;
            eng.candidate = Some(candidate.clone()); eng.current_reviewed_candidate_sha256 = None; eng.purpose = Purpose::CodeReview;
        }
        (Purpose::Implementation | Purpose::CodeReview | Purpose::Performance, Outcome::Propose { reason }) => {
            nonempty(reason)?; eng.reopen_plan()?;
        }
        (Purpose::Performance, Outcome::Measure { reason, measurement }) => {
            nonempty(reason)?;
            let candidate = eng.candidate.as_ref().ok_or("measurement requires current source")?;
            if measurement.candidate_sha256 != hash(candidate)
                || Some(&measurement.reviewed_candidate_sha256) != eng.current_reviewed_candidate_sha256.as_ref() {
                return Err("measurement requires the current reviewed candidate".into());
            }
            digest(&measurement.record_sha256)?; digest(&measurement.harness_sha256)?;
            if eng.measurements.contains_key(&measurement.record_sha256) { return Err("measurement already recorded".into()); }
            eng.measurements.insert(measurement.record_sha256.clone(), measurement.clone());
        }
        (Purpose::Performance, Outcome::Improve { reason, instruction }) => {
            nonempty(reason)?; nonempty(instruction)?;
            eng.current_reviewed_candidate_sha256 = None; eng.purpose = Purpose::Implementation;
        }
        (Purpose::Performance, Outcome::ResetBudget { reason, baseline_measurement_sha256, current_measurement_sha256, metric, improvement_bps }) => {
            nonempty(reason)?; nonempty(metric)?;
            if *improvement_bps < 1000 { return Err("budget reset requires an assessed ten percent gain".into()); }
            let group = |ids: &Vec<String>| -> Result<Measurement, String> {
                let first = eng.measurements.get(ids.first().ok_or("budget reset requires measurements")?)
                    .ok_or("budget reset measurement is not recorded")?;
                let mut seen = BTreeSet::new();
                for id in ids {
                    digest(id)?;
                    let row = eng.measurements.get(id).ok_or("budget reset measurement is not recorded")?;
                    let retained = eng.reviewed.get(&row.reviewed_candidate_sha256).ok_or("budget reset measurement lacks review")?;
                    if row.record_sha256 != *id || row.candidate_sha256 != hash(&retained.candidate)
                        || !seen.insert(id) || row.exit_codes.is_empty() || row.exit_codes.iter().any(|code| *code != 0)
                        || row.candidate_sha256 != first.candidate_sha256 || row.reviewed_candidate_sha256 != first.reviewed_candidate_sha256
                        || row.harness_sha256 != first.harness_sha256 { return Err("budget reset requires successful measurement groups".into()); }
                }
                Ok(first.clone())
            };
            let baseline = group(baseline_measurement_sha256)?; let current = group(current_measurement_sha256)?;
            let candidate = eng.candidate.as_ref().ok_or("budget reset requires a current candidate")?;
            if current.candidate_sha256 != hash(candidate)
                || Some(&current.reviewed_candidate_sha256) != eng.current_reviewed_candidate_sha256.as_ref() {
                return Err("budget reset requires the current reviewed candidate".into());
            }
            let prior = &eng.reviewed.get(&baseline.reviewed_candidate_sha256).ok_or("budget reset baseline lacks review")?.candidate;
            if (prior.source_sha256.as_str(), prior.build_sha256.as_str(), prior.lock_sha256.as_str())
                == (candidate.source_sha256.as_str(), candidate.build_sha256.as_str(), candidate.lock_sha256.as_str()) {
                return Err("budget reset requires a changed implementation".into());
            }
        }
        (Purpose::Performance, Outcome::Accept { reason, baseline }) => {
            nonempty(reason)?; digest(&baseline.harness_sha256)?; digest(&baseline.assessment_sha256)?; digest(&baseline.selection_receipt_sha256)?;
            let selected = eng.reviewed.get(&baseline.reviewed_candidate_sha256).ok_or("baseline requires an independently reviewed candidate")?.clone();
            selected.validate(&eng.subject)?;
            if hash(&selected) != baseline.reviewed_candidate_sha256 { return Err("selected reviewed context changed".into()); }
            if baseline.measurement_sha256.is_empty() { return Err("baseline requires actual measurements".into()); }
            for id in &baseline.measurement_sha256 {
                let measure = eng.measurements.get(id).ok_or("baseline measurement is not recorded")?;
                if (measure.exit_codes.is_empty() || measure.exit_codes.iter().any(|code| *code != 0)) || measure.candidate_sha256 != hash(&selected.candidate) || measure.reviewed_candidate_sha256 != baseline.reviewed_candidate_sha256 || measure.harness_sha256 != baseline.harness_sha256 {
                    return Err("baseline requires successful measurements of the selected candidate".into());
                }
            }
            eng.generation = selected.generation;
            eng.reading_sha256 = Some(selected.reading_sha256);
            eng.plan_sha256 = Some(selected.candidate.plan_sha256.clone());
            eng.plan_approval_sha256 = Some(selected.plan_approval_sha256);
            eng.plan_approval_basis_sha256 = Some(selected.plan_approval_basis_sha256);
            eng.candidate = Some(selected.candidate);
            eng.current_reviewed_candidate_sha256 = Some(baseline.reviewed_candidate_sha256.clone());
            eng.baseline = Some(baseline.clone());
        }
        (Purpose::Performance, Outcome::Fail { reason }) => { nonempty(reason)?; eng.failed = true; }
        _ => return Err("engineering outcome is outside this purpose".into()),
    }
    Ok(())
}

pub fn apply(mut state: ProtocolState, event: ProtocolEvent) -> Result<TransitionOutcome, TransitionError> {
    let event = match event {
        ProtocolEvent::WrapperResponse { response: crate::model::WrapperResponse::Engineering(checked) } => ProtocolEvent::EngineeringOutcome { checked: Box::new(checked) },
        other => other,
    };
    let result = (|| -> Result<Vec<ProtocolCommand>, String> {
        validate_state(&state)?;
        match event {
            ProtocolEvent::EngineeringHandoff { handoff } => {
                handoff.validate()?;
                let eng = state.engineering.as_ref().unwrap();
                if let Some(previous) = &eng.handoff {
                    if previous != handoff.as_ref() { return Err("engineering handoff is already recorded differently".into()); }
                    return Ok(vec![]);
                }
                if state.stage != Stage::EngineeringReady || state.in_flight_request.is_some()
                    || state.worker_attempt.is_some() || hash(&state) != handoff.predecessor_sha256
                    || eng.baseline.as_ref().map(hash).as_ref() != Some(&handoff.baseline_sha256) {
                    return Err("engineering handoff requires its exact idle Ready predecessor and baseline".into());
                }
                state.engineering.as_mut().unwrap().handoff = Some(*handoff);
                Ok(vec![ProtocolCommand::CommitCheckpoint])
            }
            ProtocolEvent::StartCycle => issue(&mut state),
            ProtocolEvent::WorkerAttemptStarted { request_id, allowance_ms } => {
                let request = state.in_flight_request.as_ref().filter(|r| r.id == request_id).ok_or("engineering attempt has no request")?;
                let eng = state.engineering.as_ref().unwrap();
                if eng.halt.is_some() || allowance_ms != request.engineering.as_ref().unwrap().profile.allowance_ms {
                    return Err("engineering attempt changed its admitted allowance or is halted".into());
                }
                let attempt = SerialAttempt { request_id, attempt: worker_attempt::Attempt::new(
                    request.worker_submission_policy.clone().unwrap(), hash(&**request), allowance_ms)? };
                if let Some(previous) = &state.worker_attempt {
                    if previous != &attempt { return Err("engineering attempt is already active".into()); }
                } else { state.worker_attempt = Some(attempt); }
                Ok(vec![])
            }
            ProtocolEvent::WorkerAttemptObserved { request_id, observation } => { observe(&mut state, request_id, observation)?; Ok(vec![]) }
            ProtocolEvent::EngineeringChecked { checked } => {
                checked_binding(&state, &checked)?;
                let Outcome::Malformed { diagnostics } = &checked.outcome else { return Err("engineering correction requires checker rejection".into()); };
                if diagnostics.is_empty() { return Err("checker rejection requires exact diagnostics".into()); }
                let feedback = serde_json::to_value(&checked.outcome).map_err(|e| e.to_string())?;
                state.worker_attempt.as_mut().unwrap().attempt.apply(worker_attempt::Event::Correction {
                    candidate_sha256: checked.candidate_sha256, feedback_sha256: hash(&feedback) })?;
                let eng = state.engineering.as_mut().unwrap(); eng.feedback = Some(feedback); eng.completed = None;
                Ok(vec![])
            }
            ProtocolEvent::EngineeringOutcome { checked } => {
                checked_binding(&state, &checked)?;
                let malformed = if let Outcome::Malformed { diagnostics } = &checked.outcome {
                    if diagnostics.is_empty() || state.worker_attempt.as_ref().unwrap().attempt.can_correct() {
                        return Err("malformed engineering outcome requires available correction".into());
                    }
                    Some(diagnostics.clone())
                } else { None };
                let retry = malformed.is_some() && crate::engine::continue_worker_retry(&mut state, crate::model::RetryOutcomeKind::Invalid);
                if malformed.is_none() { crate::engine::clear_retry_context(&mut state); }
                let eng = state.engineering.as_mut().unwrap();
                let prior_failure = eng.retry_review.clone();
                if let Some(diagnostics) = malformed {
                    eng.feedback = Some(serde_json::to_value(&checked.outcome).map_err(|e| e.to_string())?);
                    if !retry {
                        if eng.purpose == Purpose::RetryReview { eng.failed = true; }
                        else {
                            eng.retry_review = Some(RetryReview { failed_purpose: eng.purpose, basis_sha256: eng.basis(),
                                request_sha256: checked.request_sha256.clone(), candidate_sha256: checked.candidate_sha256.clone(),
                                checked_sha256: hash(&checked), diagnostics });
                            eng.purpose = Purpose::RetryReview;
                        }
                    }
                } else { apply_outcome(eng, &checked)?; }
                eng.assignment_feedback = match &checked.outcome {
                    Outcome::Malformed { .. } | Outcome::Review { decision: Decision::Revise | Decision::Reject, .. }
                        | Outcome::Improve { .. } | Outcome::Propose { .. } => Some(AssignmentFeedback {
                            checked_sha256: hash(&checked), outcome: checked.outcome.clone(), prior_failure }),
                    _ => None,
                };
                eng.last_consumption = Some(Consumption { request_id: checked.request_id, turn: checked.turn,
                    request_sha256: checked.request_sha256.clone(), candidate_sha256: checked.candidate_sha256.clone(),
                    checked_sha256: hash(&checked), predecessor_sha256: eng.last_consumption.as_ref().map(hash) });
                eng.completed = None;
                state.worker_attempt = None; state.in_flight_request = None;
                if eng.baseline.is_some() { state.stage = Stage::EngineeringReady; }
                Ok(vec![ProtocolCommand::CommitCheckpoint])
            }
            _ => Err("event is unavailable during pre-extraction engineering".into()),
        }
    })().map_err(TransitionError::IllegalResponse)?;
    validate_state(&state).map_err(TransitionError::InvariantViolation)?;
    Ok(TransitionOutcome { state, commands: result })
}

//! Durable scheduling facts. This module cannot accept mathematical work.
//!
//! The host supplies checked requests and complete dependency observations;
//! ordinary kernel acceptance remains responsible for their results. A slot
//! and its scope stay owned until checking and process cleanup both finish.
//! Opt-in execution pipelining releases provider capacity during checking;
//! semantic ownership and the separate total in-flight bound remain held.
use crate::model::{NodeId, Phase, ReviewResponse, TaskMode, WrapperRequest};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(test)]
use std::sync::Arc;

pub type TaskId = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTaskKind {
    #[default]
    Ordinary,
    LeanClosure,
}

impl WorkerTaskKind {
    pub fn is_ordinary(&self) -> bool { *self == Self::Ordinary }
}

fn closure_tasks() -> BTreeSet<WorkerTaskKind> {
    BTreeSet::from([WorkerTaskKind::LeanClosure])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompilerCapability {
    None,
    Managed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: String,
    pub timeout_seconds: u64,
    pub max_timeout_seconds: u64,
    /// A model-strength policy, distinct from compiler capability. Unknown
    /// custom profiles default conservatively to closure work until configured.
    #[serde(default = "closure_tasks")]
    pub allowed_tasks: BTreeSet<WorkerTaskKind>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cost_guidance: String,
    /// Explicit launch policy; absence preserves historical frozen bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lightweight: Option<bool>,
}

impl ModelProfile {
    pub fn is_sol(&self) -> bool {
        self.model.split('-').any(|part| part.eq_ignore_ascii_case("sol"))
    }
}

/// Host execution binding on an isolated worker request. Task IDs name
/// attempts, so a retry cannot reuse another attempt's live conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskExecutionContext {
    pub task_id: TaskId,
    pub profile: String,
    pub binding: ModelProfile,
    pub compiler: CompilerCapability,
    pub task_kind: WorkerTaskKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DispatchConfig {
    pub worker_slots: usize,
    /// Opt-in total unresolved-task bound. When set, Checking no longer uses
    /// provider capacity, but retains its semantic scope and backlog position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight_tasks: Option<usize>,
    /// Absent in historical configurations; new launches default to six.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sol_worker_slots: Option<usize>,
    /// Explicit model classes replace the historical model-name cap. Once
    /// enabled, these are live admission limits, not bounds on retained work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heavy_worker_slots: Option<usize>,
    /// Missing on historical runs; enabling it is a recorded migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coarse_goal_slots: Option<usize>,
    /// Per-node launch budget across lightweight profiles in a mixed menu.
    /// Historical absence keeps the existing policy until explicitly migrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lightweight_attempts_per_node: Option<usize>,
    /// Opt-in until submit/check/repair has passed staged rollout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_submission_policy: Option<crate::worker_attempt::Policy>,
    pub compiler_slots: usize,
    pub verifier_slots: usize,
    pub profiles: BTreeMap<String, ModelProfile>,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        let profile = |model: &str, effort: &str, seconds, allowed_tasks, lightweight| ModelProfile {
            provider: "codex".into(), model: model.into(), reasoning_effort: effort.into(),
            timeout_seconds: seconds, max_timeout_seconds: seconds, allowed_tasks, cost_guidance: String::new(), lightweight: Some(lightweight),
        };
        Self {
            worker_slots: 8, max_in_flight_tasks: None, sol_worker_slots: Some(6), heavy_worker_slots: None, coarse_goal_slots: Some(2), lightweight_attempts_per_node: Some(2), worker_submission_policy: None, compiler_slots: 6, verifier_slots: 8,
            profiles: BTreeMap::from([
                ("sol-xhigh".into(), profile("gpt-6.1-sol", "xhigh", 4 * 3600,
                    BTreeSet::from([WorkerTaskKind::Ordinary, WorkerTaskKind::LeanClosure]), false)),
                ("luna-high".into(), profile("gpt-6-luna", "high", 3600, closure_tasks(), true)),
            ]),
        }
    }
}

pub(crate) fn valid_key(s: &str) -> bool {
    !s.is_empty() && s.len() <= 96 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

impl DispatchConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.worker_slots == 0 || self.compiler_slots == 0 || self.verifier_slots == 0 {
            return Err("worker, compiler and verifier capacities must be positive".into());
        }
        if self.max_in_flight_tasks.is_some_and(|limit| limit < self.worker_slots) {
            return Err("max_in_flight_tasks must be at least worker_slots or unset".into());
        }
        if self.sol_worker_slots == Some(0) {
            return Err("Sol worker capacity must be positive or unset".into());
        }
        if self.heavy_worker_slots == Some(0) {
            return Err("heavy worker capacity must be positive or unset".into());
        }
        if self.heavy_worker_slots.is_some() && self.profiles.values().any(|p| p.lightweight.is_none()) {
            return Err("live agent limits require explicit lightweight true or false on every worker profile".into());
        }
        if self.coarse_goal_slots == Some(0) { return Err("coarse-goal capacity must be positive or unset".into()); }
        if self.lightweight_attempts_per_node == Some(0) {
            return Err("lightweight_attempts_per_node must be positive or unset".into());
        }
        if let Some(policy) = &self.worker_submission_policy {
            policy.validate()?;
            if self.profiles.values().any(|profile| !matches!(profile.provider.as_str(), "codex" | "deepseek" | "kimi")) {
                return Err("worker submissions currently require Codex, DeepSeek, or Kimi worker profiles".into());
            }
        }
        if self.profiles.is_empty() { return Err("at least one worker model profile is required".into()); }
        for (id, p) in &self.profiles {
            if !valid_key(id) || p.provider.trim().is_empty() || p.model.trim().is_empty()
                || p.reasoning_effort.trim().is_empty() || p.timeout_seconds == 0
                || p.timeout_seconds > p.max_timeout_seconds || p.allowed_tasks.is_empty()
            {
                return Err(format!("invalid worker profile {id:?}: require a safe ID, provider, model, effort and positive timeout within its launch limit"));
            }
        }
        Ok(())
    }

    pub fn bind(&self, id: &str, timeout: Option<u64>) -> Result<ModelProfile, String> {
        let mut profile = self.profiles.get(id).cloned()
            .ok_or_else(|| format!("worker profile {id:?} is unavailable"))?;
        let seconds = timeout.unwrap_or(profile.timeout_seconds);
        if seconds == 0 || seconds > profile.max_timeout_seconds {
            return Err(format!("worker profile {id:?} permits a timeout of 1–{} seconds", profile.max_timeout_seconds));
        }
        profile.timeout_seconds = seconds;
        Ok(profile)
    }
}

/// Trusted union of semantic, Lean-import and definition dependencies. Missing
/// entries are unknown, not evidence that a node is dependency-free.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyGraph {
    pub edges: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

impl DependencyGraph {
    pub fn closure(&self, roots: &BTreeSet<NodeId>) -> Result<BTreeSet<NodeId>, String> {
        let mut seen = BTreeSet::new();
        let mut pending: Vec<_> = roots.iter().cloned().collect();
        while let Some(node) = pending.pop() {
            if !seen.insert(node.clone()) { continue; }
            let deps = self.edges.get(&node)
                .ok_or_else(|| format!("dependency observation is incomplete for {node}"))?;
            pending.extend(deps.iter().cloned());
        }
        Ok(seen)
    }

    // Diagnostic only, after the existing conflict predicate has refused.
    // Sorted roots/edges make the shortest dependency path deterministic.
    fn dependency_path(&self, readers: &BTreeSet<NodeId>, writers: &BTreeSet<NodeId>) -> Option<Vec<NodeId>> {
        let mut parent: BTreeMap<NodeId, Option<NodeId>> = readers.iter().cloned().map(|n| (n, None)).collect();
        let mut pending: VecDeque<_> = readers.iter().cloned().collect();
        while let Some(node) = pending.pop_front() {
            if writers.contains(&node) {
                let mut path = vec![node.clone()];
                let mut cursor = node;
                while let Some(Some(previous)) = parent.get(&cursor) {
                    path.push(previous.clone());
                    cursor = previous.clone();
                }
                path.reverse();
                return Some(path);
            }
            for dependency in self.edges.get(&node).into_iter().flatten() {
                if !parent.contains_key(dependency) {
                    parent.insert(dependency.clone(), Some(node.clone()));
                    pending.push_back(dependency.clone());
                }
            }
        }
        None
    }

    fn dependency_conflict_detail(&self, a: &EditScope, a_kind: WorkerTaskKind,
        b: &EditScope, b_kind: WorkerTaskKind, label: &str) -> Option<String> {
        // Preserve the existing generic diagnostic for non-dependency conflicts.
        if a.exclusive || b.exclusive || !a.nodes.is_disjoint(&b.nodes)
            || !a.shared_writes.is_disjoint(&b.shared_writes)
            || a.helper_prefixes.iter().any(|x| b.helper_prefixes.iter().any(|y| x.starts_with(y) || y.starts_with(x)))
        { return None; }
        let directed = if b_kind == WorkerTaskKind::Ordinary {
            self.dependency_path(&a.nodes, &b.nodes).map(|path|
                ("proposed task".to_owned(), label.to_owned(), path))
        } else { None };
        let (reader, writer, path) = directed.or_else(|| {
            if a_kind == WorkerTaskKind::Ordinary {
                self.dependency_path(&b.nodes, &a.nodes).map(|path|
                    (label.to_owned(), "proposed task".to_owned(), path))
            } else { None }
        })?;
        let names: Vec<_> = path.iter().map(ToString::to_string).collect();
        let displayed = if names.len() <= 12 { names.join(" -> ") } else {
            format!("{} -> ... ({} nodes omitted) ... -> {}",
                names[..6].join(" -> "), names.len() - 12, names[names.len()-6..].join(" -> "))
        };
        Some(format!("{reader} reads a dependency written by {writer} (ordinary); dependency path (reader -> dependency): {displayed}. Only a lean_closure dependency writer permits this overlap"))
    }

    pub fn conflicts(&self, a: &EditScope, b: &EditScope) -> Result<bool, String> {
        self.conflicts_for_tasks(a, WorkerTaskKind::Ordinary, b, WorkerTaskKind::Ordinary)
    }

    pub fn conflicts_for_tasks(&self, a: &EditScope, a_kind: WorkerTaskKind,
        b: &EditScope, b_kind: WorkerTaskKind) -> Result<bool, String> {
        if a.exclusive || b.exclusive || !a.nodes.is_disjoint(&b.nodes) { return Ok(true); }
        if !a.shared_writes.is_disjoint(&b.shared_writes)
            || a.helper_prefixes.iter().any(|x| b.helper_prefixes.iter().any(|y| x.starts_with(y) || y.starts_with(x)))
        { return Ok(true); }
        let a_reads = self.closure(&a.nodes)?;
        let b_reads = self.closure(&b.nodes)?;
        // Only the dependency writer's closure restriction permits overlap.
        // Acceptance still checks each candidate against current source.
        Ok((b_kind != WorkerTaskKind::LeanClosure && !a_reads.is_disjoint(&b.nodes))
            || (a_kind != WorkerTaskKind::LeanClosure && !b_reads.is_disjoint(&a.nodes)))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditScope {
    pub nodes: BTreeSet<NodeId>,
    /// Prefixes are allocated by the host; arbitrary helpers still undergo
    /// ordinary task legality and graph validation at integration.
    pub helper_prefixes: BTreeSet<String>,
    pub shared_writes: BTreeSet<String>,
    pub exclusive: bool,
}

impl EditScope {
    pub fn validate(&self, graph: &DependencyGraph) -> Result<(), String> {
        if !self.exclusive && self.nodes.is_empty() {
            return Err("a concurrent worker requires a nonempty authorized edit scope".into());
        }
        if self.helper_prefixes.iter().any(|p| !valid_key(p)) {
            return Err("helper ownership prefixes must contain only letters, digits, underscores or hyphens".into());
        }
        if !self.exclusive && !self.shared_writes.is_empty() {
            return Err("shared source, configuration and preamble changes require an exclusive task".into());
        }
        graph.closure(&self.nodes)?;
        Ok(())
    }
}

/// Shared reservation conflict check. Callers supply only occupied scopes.
pub fn validate_scope_conflicts<'a>(
    scope: &EditScope, kind: WorkerTaskKind, graph: &DependencyGraph,
    occupied: impl IntoIterator<Item = (String, &'a EditScope, WorkerTaskKind)>,
) -> Result<(), String> {
    for (label, other, other_kind) in occupied {
        if graph.conflicts_for_tasks(scope, kind, other, other_kind)? {
            let mut message = format!("authorized edit scope conflicts with {label}");
            if let Some(detail) = graph.dependency_conflict_detail(scope, kind, other, other_kind, &label) {
                message.push_str(": ");
                message.push_str(&detail);
            }
            return Err(message);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize)]
pub struct FrozenTaskScope {
    #[serde(default)]
    pub task_kind: WorkerTaskKind,
    pub id: TaskId,
    pub status: TaskStatus,
    pub scope: EditScope,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FrozenAssignmentScope {
    #[serde(default)]
    pub task_kind: WorkerTaskKind,
    pub id: String,
    pub review: ReviewResponse,
    #[serde(default)]
    pub evidence_paths: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FrozenScopeCheck {
    pub phase: Phase,
    pub active_node: Option<NodeId>,
    pub current_present_nodes: BTreeSet<NodeId>,
    pub current_deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    pub tasks: Vec<FrozenTaskScope>,
    pub assignments: Vec<FrozenAssignmentScope>,
}

/// Advisory preflight over known semantic edges, not a reservation. Admission
/// observes additional Lean/definition edges and rechecks current state. These
/// explicit authorizations are guaranteed subsets of the eventual edit scope;
/// unknown/legacy routing envelopes are left to ordinary worker preparation.
pub fn check_frozen_scopes(input: &FrozenScopeCheck) -> Result<BTreeMap<String, String>, String> {
    let graph = DependencyGraph { edges: input.current_present_nodes.iter().map(|node|
        (node.clone(), input.current_deps.get(node).cloned().unwrap_or_default())).collect() };
    let mut occupied: Vec<_> = input.tasks.iter().filter(|task| task.status.occupies_slot())
        .map(|task| (format!("task {}", task.id), task.scope.clone(), task.task_kind)).collect();
    let mut errors = BTreeMap::new();
    for assignment in &input.assignments {
        let review = &assignment.review;
        let nodes = match (input.phase, review.next_mode) {
            (Phase::ProofFormalization, TaskMode::Restructure | TaskMode::CoarseRestructure)
            | (Phase::TheoremStating | Phase::RevisionStating, TaskMode::Restructure) =>
                review.authorized_nodes.clone(),
            (Phase::ProofFormalization | Phase::TheoremStating | Phase::RevisionStating,
             TaskMode::Local | TaskMode::Targeted) =>
                review.next_active.as_ref().or(input.active_node.as_ref()).cloned().into_iter().collect(),
            _ => BTreeSet::new(),
        };
        let evidence = &assignment.evidence_paths;
        if !evidence.is_empty() && (input.phase != Phase::Modeling
            || assignment.task_kind != WorkerTaskKind::Ordinary || evidence.len() > 64
            || evidence.iter().any(|path| crate::model_preservation::validate_evidence_path(path).is_err()))
        {
            errors.insert(assignment.id.clone(), "evidence_paths requires exact model-evidence files in ordinary Modeling".into());
            continue;
        }
        let exclusive = input.phase == Phase::Modeling || !evidence.is_empty();
        if nodes.is_empty() && !exclusive { continue; }
        let scope = EditScope { nodes, shared_writes: evidence.clone(), exclusive, ..Default::default() };
        let result = scope.validate(&graph).and_then(|()| validate_scope_conflicts(&scope, assignment.task_kind, &graph,
            occupied.iter().map(|(label, scope, kind)| (label.clone(), scope, *kind))));
        if let Err(error) = result {
            errors.insert(assignment.id.clone(), error);
        } else {
            occupied.push((format!("assignment {}", assignment.id), scope, assignment.task_kind));
        }
    }
    Ok(errors)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedAssignment {
    /// Unique within a reviewer response; the receipt uses this exact ID.
    pub assignment_id: String,
    pub profile: String,
    pub timeout_seconds: Option<u64>,
    pub compiler: CompilerCapability,
    pub task_kind: WorkerTaskKind,
    pub scope: EditScope,
    pub base_snapshot: String,
    /// Exact relevant input hashes, not a global event counter.
    pub input_hashes: BTreeMap<String, String>,
    /// Readonly proof dependencies eligible for fresh acceptance after body edits.
    /// Exact hashes remain available for canonical change detection.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub proof_body_rebase_inputs: BTreeMap<String, String>,
    /// Frozen request bytes are shared across transactional ledger snapshots.
    /// Binding uses copy-on-write; resolved contracts may become verified history references.
    pub request: crate::dispatch_history::FrozenRequest,
    /// Frozen ordinary assignment, replayed against current state at acceptance.
    #[serde(default)]
    pub review: Option<Box<crate::model::ReviewResponse>>,
    #[serde(default)]
    pub authorized_worker: Option<crate::dispatch_contract::AuthorizedWorker>,
}

impl PreparedAssignment {
    pub fn input_matches(&self, path: &str, expected: &str,
        inputs: &BTreeMap<String, String>, prefixes: &BTreeMap<String, String>) -> bool {
        inputs.get(path).is_some_and(|actual| actual == expected)
            || (expected == "absent" && !inputs.contains_key(path) && self.scope.exclusive
                && self.scope.shared_writes.contains(path)
                && crate::model_preservation::validate_evidence_path(path).is_ok())
            || self.proof_body_rebase_inputs.get(path)
                .is_some_and(|prefix| prefixes.get(path) == Some(prefix))
    }

    fn validate_permissions(&self, binding: &ModelProfile) -> Result<(), String> {
        if !binding.allowed_tasks.contains(&self.task_kind) {
            return Err(format!("profile {:?} is not permitted for {:?} tasks; select a profile authorized for this work",
                self.profile, self.task_kind));
        }
        if self.task_kind == WorkerTaskKind::LeanClosure
            && (self.compiler != CompilerCapability::Managed || self.scope.nodes.len() != 1
                || !self.scope.helper_prefixes.is_empty() || !self.scope.shared_writes.is_empty()
                || self.scope.exclusive)
        {
            return Err("Lean-closure work requires one existing node and managed Lean; only its proof body and import additions are permitted, without helpers or shared edits".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
    pub boot_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TaskStatus {
    Reserved,
    Running,
    /// Provider execution ended on time; cleanup and immutable sealing remain.
    Submitted,
    Checking { candidate_sha256: String },
    /// Includes cancellation of checker jobs. Slot release requires a second
    /// event confirming the whole attempt's processes have been reaped.
    Stopping { outcome: AttemptOutcome },
    Resolved { outcome: AttemptOutcome, resolved_at_ms: u64 },
}

impl TaskStatus {
    pub fn occupies_slot(&self) -> bool { !matches!(self, Self::Resolved { .. }) }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttemptOutcome {
    Accepted { event_id: u64 },
    Rejected { reason: String },
    Stuck { reason: String },
    NeedsRestructure { reason: String },
    TargetFalseUnderModel { reason: String },
    TimedOut,
    Cancelled { reason: String },
    LaunchFailed { reason: String },
    Stale { reason: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryContext {
    pub outcome_kind: crate::model::RetryOutcomeKind,
    pub attempt: u32,
    pub transport_attempt: u32,
    pub consecutive_transport_failure_node: Option<NodeId>,
    pub consecutive_transport_failure_count: u32,
}

impl RetryContext {
    /// Retain the ordinary engine's consecutive-outcome semantics and its
    /// separate transport circuit breaker. No scheduler-specific retry policy.
    pub fn capture(state: &crate::model::ProtocolState) -> Self {
        Self {
            outcome_kind: state.retry_outcome_kind,
            attempt: state.attempt,
            transport_attempt: state.transport_attempt,
            consecutive_transport_failure_node: state.consecutive_transport_failure_node.clone(),
            consecutive_transport_failure_count: state.consecutive_transport_failure_count,
        }
    }

    pub fn install(&self, state: &mut crate::model::ProtocolState) {
        state.retry_outcome_kind = self.outcome_kind;
        state.attempt = self.attempt;
        state.transport_attempt = self.transport_attempt;
        state.consecutive_transport_failure_node = self.consecutive_transport_failure_node.clone();
        state.consecutive_transport_failure_count = self.consecutive_transport_failure_count;
        state.invalid_attempt = self.outcome_kind == crate::model::RetryOutcomeKind::Invalid;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTask {
    pub id: TaskId,
    pub review_id: u32,
    pub assignment: PreparedAssignment,
    pub binding: ModelProfile,
    pub status: TaskStatus,
    pub retry_of: Option<TaskId>,
    pub retry: RetryContext,
    pub reserved_at_ms: u64,
    pub launched_at_ms: Option<u64>,
    pub process: Option<ProcessIdentity>,
    pub checker_wait_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<crate::dispatch_integration::AttemptReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_retry: Option<RetryContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_refusal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_attempt: Option<crate::worker_attempt::Attempt>,
    /// Released only after an owned submission is sealed. Retained through
    /// host checking/cleanup and queued correction; reacquired at launch.
    /// Omission preserves every historical task's execution reservation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub execution_capacity_released: bool,
}

fn is_false(value: &bool) -> bool { !value }

impl WorkerTask {
    pub fn deadline_ms(&self) -> Option<u64> {
        if let Some(attempt) = &self.worker_attempt { return attempt.deadline_ms(); }
        self.launched_at_ms.map(|t| t.saturating_add(self.binding.timeout_seconds.saturating_mul(1000)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchDecision {
    pub assignment_id: String,
    pub task_id: Option<TaskId>,
    pub rejection: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchReceipt {
    pub review_id: u32,
    pub response_sha256: String,
    pub decisions: Vec<DispatchDecision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "preparation", rename_all = "snake_case")]
pub enum PreparedItem {
    Ready { assignment: PreparedAssignment },
    Refused { assignment_id: String, reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCursor {
    pub request_id: u32,
    pub observed_event: u64,
}

/// These are host lifecycle observations, never mathematical approvals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DispatchEvent {
    VerifierRequestsRetired {
        #[serde(deserialize_with = "deserialize_id_map")]
        requests: BTreeMap<u64, crate::verifier_history::Reference>,
        #[serde(skip)]
        verified: BTreeMap<u64, crate::verifier_history::Verified>,
    },
    RequestsArchived {
        #[serde(deserialize_with = "deserialize_id_map")]
        requests: BTreeMap<TaskId, crate::dispatch_history::RequestReference>,
        #[serde(skip)]
        verified: BTreeMap<TaskId, crate::dispatch_history::VerifiedRequest>,
    },
    WorkerCapacityChanged { expected_slots: usize, worker_slots: usize },
    ExecutionPipelineChanged { expected_max_in_flight_tasks: Option<usize>, max_in_flight_tasks: Option<usize> },
    AgentCapacityChanged { expected_worker_slots: usize, expected_heavy_worker_slots: Option<usize>,
        worker_slots: usize, heavy_worker_slots: usize },
    CompilerCapacityChanged { expected_slots: usize, compiler_slots: usize },
    SolWorkerCapacityChanged { expected_slots: Option<usize>, sol_worker_slots: Option<usize> },
    LightweightBudgetChanged { expected_limit: Option<usize>, lightweight_attempts_per_node: Option<usize> },
    ModelProfilesChanged {
        expected_profiles: BTreeMap<String, ModelProfile>,
        profiles: BTreeMap<String, ModelProfile>,
    },
    ReviewStarted { request_id: u32 },
    AssignmentsDispatched {
        review_id: u32, response_sha256: String, assignments: Vec<PreparedAssignment>,
        graph: DependencyGraph, now_ms: u64,
    },
    BatchPrepared {
        review_id: u32, response_sha256: String, items: Vec<PreparedItem>,
        graph: DependencyGraph, now_ms: u64,
    },
    Launched { task_id: TaskId, process: ProcessIdentity, now_ms: u64 },
    ProviderStarted { task_id: TaskId, process: ProcessIdentity, started_at_ms: u64 },
    TurnCompleted { task_id: TaskId, completion: crate::worker_attempt::Completion },
    WorkerInterrupted { task_id: TaskId, interruption: crate::worker_attempt::Interruption },
    WorkerExpired { task_id: TaskId, process: ProcessIdentity, turn: u32, elapsed_ms: u64 },
    CheckRetry { task_id: TaskId, candidate_sha256: String, retry: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery_completion_sha256: Option<String> },
    CandidateReady { task_id: TaskId, candidate_sha256: String, now_ms: u64 },
    AttemptStopping { task_id: TaskId, outcome: AttemptOutcome },
    ProcessesReaped { task_id: TaskId, now_ms: u64 },
    RetryPrepared {
        predecessor: TaskId, assignment: Option<PreparedAssignment>, refusal: Option<String>,
        graph: DependencyGraph, now_ms: u64,
    },
    Drain { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerHandoff {
    pub review_id: u32,
    pub comments_sha256: String,
}

impl ReviewerHandoff {
    pub fn new(review_id: u32, comments: &str) -> Self {
        Self { review_id, comments_sha256: crate::trusted_artifact_rebind::digest(&serde_json::json!(comments)) }
    }

    pub fn matches(&self, comments: &str) -> bool {
        self.comments_sha256 == crate::trusted_artifact_rebind::digest(&serde_json::json!(comments))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchLedger {
    pub config: DispatchConfig,
    pub phase: Phase,
    #[serde(deserialize_with = "deserialize_id_map")]
    pub tasks: BTreeMap<TaskId, WorkerTask>,
    #[serde(deserialize_with = "deserialize_id_map")]
    pub receipts: BTreeMap<u32, DispatchReceipt>,
    pub next_task_id: TaskId,
    pub event_seq: u64,
    pub reviewed_event: u64,
    pub reviewer: Option<ReviewCursor>,
    /// Origin of advisory notes forwarded through an ordinary reviewer action.
    /// This does not authorize edits or contribute proof evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_handoff: Option<ReviewerHandoff>,
    pub draining: Option<String>,
    #[serde(default)]
    pub verifiers: crate::dispatch_verifiers::VerifierQueue,
    /// Source acceptance and routing are distinct boundaries. Required lanes
    /// and exclusive operations are derived after source-backed issuance.
    #[serde(default)]
    pub routing_pending: bool,
    #[serde(default)]
    pub continuations: std::collections::VecDeque<Box<crate::dispatch_routing::Continuation>>,
    #[serde(default, deserialize_with = "deserialize_id_map")]
    pub exclusive_workers: BTreeMap<TaskId, crate::dispatch_routing::ExclusiveWorker>,
    #[serde(default, deserialize_with = "deserialize_id_map")]
    pub publications: BTreeMap<TaskId, crate::dispatch_integration::PublicationReceipt>,
}

/// JSON object keys stay strings inside serde's internally tagged enum buffer.
/// Decode them explicitly so complete ledgers round-trip through CLI requests
/// as well as directly from checkpoint files.
pub(crate) fn deserialize_id_map<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where D: serde::Deserializer<'de>, K: std::str::FromStr + Ord + ToString,
      V: serde::Deserialize<'de>,
{
    let values = BTreeMap::<String, V>::deserialize(deserializer)?;
    values.into_iter().map(|(key, value)| {
        let id = key.parse::<K>().map_err(|_| serde::de::Error::custom("invalid numeric dispatch ID"))?;
        if id.to_string() != key { return Err(serde::de::Error::custom("noncanonical dispatch ID")); }
        Ok((id, value))
    }).collect()
}

impl DispatchLedger {
    pub fn apply(&mut self, event: DispatchEvent) -> Result<(), String> {
        // A rejected event leaves even the operational ledger unchanged.
        let mut next = self.clone();
        match event {
            DispatchEvent::VerifierRequestsRetired { requests, verified } => {
                if requests.is_empty() { return Err("empty resolved verifier retirement".into()); }
                for (id, reference) in requests {
                    reference.validate()?;
                    let job = next.verifiers.jobs.get_mut(&id).ok_or("unknown retired verifier")?;
                    if !matches!(job.status, crate::dispatch_verifiers::VerifierStatus::Resolved { .. }) {
                        return Err("only resolved verifier requests may be retired".into());
                    }
                    if job.request.is_none() && job.retired_request.as_ref() == Some(&reference) { continue; }
                    if !verified.get(&id).is_some_and(|proof| proof.matches(&reference, job)) {
                        return Err("resolved verifier retirement lacks its exact checkpoint proof".into());
                    }
                    job.request = None;
                    job.retired_request = Some(reference);
                }
            }
            DispatchEvent::RequestsArchived { requests, verified } => {
                if requests.is_empty() { return Err("empty dispatch request archival event".into()); }
                for (id, reference) in requests {
                    reference.validate()?;
                    let task = next.tasks.get_mut(&id).ok_or("unknown archived task")?;
                    if !matches!(task.status, TaskStatus::Resolved { .. }) || task.pending_retry.is_some() {
                        return Err("only resolved tasks without pending retries can be archived".into());
                    }
                    match &task.assignment.request {
                        crate::dispatch_history::FrozenRequest::Inline(request) => {
                            if !verified.get(&id).is_some_and(|proof| proof.matches(&reference, request))
                                && crate::dispatch_history::RequestReference::from_request(request)?.0 != reference {
                                return Err("archived request does not match the exact frozen task contract".into());
                            }
                        },
                        crate::dispatch_history::FrozenRequest::Archived(previous) if previous == &reference => {},
                        _ => return Err("conflicting dispatch request archival replay".into()),
                    }
                    task.assignment.request = crate::dispatch_history::FrozenRequest::Archived(reference);
                }
            }

            DispatchEvent::AgentCapacityChanged { expected_worker_slots, expected_heavy_worker_slots,
                worker_slots, heavy_worker_slots } => {
                if next.config.worker_slots != expected_worker_slots
                    || next.config.heavy_worker_slots != expected_heavy_worker_slots {
                    return Err("agent limits changed since they were inspected; refresh and try again".into());
                }
                if next.config.max_in_flight_tasks.is_some() && (worker_slots < next.tasks.values()
                    .filter(|t| next.occupies_execution_capacity(t)).count()
                    || heavy_worker_slots < next.occupied_heavy_slots()) {
                    return Err("execution pipeline limits cannot be lowered below current provider reservations; wait for checking or drain first".into());
                }
                if next.config.worker_slots != worker_slots || next.config.heavy_worker_slots != Some(heavy_worker_slots) {
                    next.config.worker_slots = worker_slots;
                    next.config.heavy_worker_slots = Some(heavy_worker_slots);
                    next.signal()?;
                }
                // Existing reservations, provider turns and host checks keep
                // ownership. Legacy mode grandfathers lower limits; opt-in
                // pipelining retains strict execution bounds and refuses a
                // reduction below current leases until they have drained.
            }
            DispatchEvent::ExecutionPipelineChanged { expected_max_in_flight_tasks, max_in_flight_tasks } => {
                if next.config.max_in_flight_tasks != expected_max_in_flight_tasks {
                    return Err("execution pipeline changed since it was inspected".into());
                }
                if next.reviewer.is_some() || next.tasks.values().any(|t| t.status.occupies_slot())
                    || next.verifiers.occupied_slots() > 0 {
                    return Err("execution pipeline changes require drained workers, reviewer and verifiers".into());
                }
                if max_in_flight_tasks != expected_max_in_flight_tasks {
                    next.config.max_in_flight_tasks = max_in_flight_tasks;
                    next.config.validate()?;
                    next.signal()?;
                }
            }
            DispatchEvent::WorkerCapacityChanged { expected_slots, worker_slots } => {
                if next.config.worker_slots != expected_slots {
                    return Err("worker capacity changed since it was inspected".into());
                }
                if worker_slots == 0 {
                    return Err("worker capacity must be positive".into());
                }
                if next.reviewer.is_some() || next.tasks.values().any(|t| t.status.occupies_slot())
                    || next.verifiers.occupied_slots() > 0
                {
                    return Err("worker capacity changes require drained workers, reviewer and verifiers".into());
                }
                if worker_slots != expected_slots {
                    next.config.worker_slots = worker_slots;
                    next.signal()?;
                }
            }
            DispatchEvent::CompilerCapacityChanged { expected_slots, compiler_slots } => {
                if next.config.compiler_slots != expected_slots {
                    return Err("compiler capacity changed since it was inspected".into());
                }
                if compiler_slots == 0 {
                    return Err("compiler capacity must be positive".into());
                }
                if next.reviewer.is_some() || next.tasks.values().any(|t| t.status.occupies_slot())
                    || next.verifiers.occupied_slots() > 0
                {
                    return Err("compiler capacity changes require drained workers, reviewer and verifiers".into());
                }
                if compiler_slots != expected_slots {
                    next.config.compiler_slots = compiler_slots;
                    next.signal()?;
                }
            }
            DispatchEvent::SolWorkerCapacityChanged { expected_slots, sol_worker_slots } => {
                if next.config.sol_worker_slots != expected_slots {
                    return Err("Sol worker capacity changed since it was inspected".into());
                }
                if sol_worker_slots == Some(0) { return Err("Sol worker capacity must be positive or unset".into()); }
                if next.reviewer.is_some() || next.tasks.values().any(|t| t.status.occupies_slot())
                    || next.verifiers.occupied_slots() > 0
                {
                    return Err("Sol worker capacity changes require drained workers, reviewer and verifiers".into());
                }
                if sol_worker_slots != expected_slots {
                    next.config.sol_worker_slots = sol_worker_slots;
                    next.signal()?;
                }
            }
            DispatchEvent::LightweightBudgetChanged { expected_limit, lightweight_attempts_per_node } => {
                if next.config.lightweight_attempts_per_node != expected_limit {
                    return Err("lightweight attempt budget changed since it was inspected".into());
                }
                if lightweight_attempts_per_node == Some(0) {
                    return Err("lightweight_attempts_per_node must be positive or unset".into());
                }
                if next.reviewer.is_some() || next.tasks.values().any(|t| t.status.occupies_slot())
                    || next.verifiers.occupied_slots() > 0
                {
                    return Err("lightweight attempt budget changes require drained workers, reviewer and verifiers".into());
                }
                if lightweight_attempts_per_node != expected_limit {
                    next.config.lightweight_attempts_per_node = lightweight_attempts_per_node;
                    next.signal()?;
                }
            }
            DispatchEvent::ModelProfilesChanged { expected_profiles, profiles } => {
                if next.config.profiles != expected_profiles {
                    return Err("worker model profiles changed since they were inspected".into());
                }
                if next.reviewer.is_some() || next.tasks.values().any(|t| t.status.occupies_slot())
                    || next.verifiers.occupied_slots() > 0
                {
                    return Err("worker model profile changes require drained workers, reviewer and verifiers".into());
                }
                // Legacy requests have no designation. Their stored profile ID
                // is the explicit migration key; keep it so a renamed alias
                // cannot erase launches without rewriting frozen evidence.
                for task in next.tasks.values().filter(|t| t.launched_at_ms.is_some() && t.binding.lightweight.is_none()) {
                    let id = &task.assignment.profile;
                    if next.config.profiles.get(id).is_some_and(|p| p.lightweight == Some(true))
                        && !profiles.get(id).is_some_and(|p| p.lightweight == Some(true))
                    {
                        return Err(format!("retain profile {id:?} with lightweight=true: historical launches still use that explicit designation"));
                    }
                }
                if profiles != expected_profiles {
                    next.config.profiles = profiles;
                    next.config.validate()?;
                    next.signal()?;
                }
            }
            DispatchEvent::ReviewStarted { request_id } => next.start_review(request_id)?,
            DispatchEvent::AssignmentsDispatched { review_id, response_sha256, assignments, graph, now_ms } => {
                next.dispatch(review_id, response_sha256, assignments, &graph, now_ms)?;
            }
            DispatchEvent::BatchPrepared { review_id, response_sha256, items, graph, now_ms } => {
                next.dispatch_prepared(review_id, response_sha256, items, &graph, now_ms)?;
            }
            DispatchEvent::Launched { task_id, process, now_ms } => next.launched(task_id, process, now_ms)?,
            DispatchEvent::ProviderStarted { task_id, process, started_at_ms } => next.provider_started(task_id, process, started_at_ms)?,
            DispatchEvent::TurnCompleted { task_id, completion } => next.turn_completed(task_id, completion)?,
            DispatchEvent::WorkerInterrupted { task_id, interruption } => next.worker_interrupted(task_id, interruption)?,
            DispatchEvent::WorkerExpired { task_id, process, turn, elapsed_ms } => {
                let task = next.tasks.get(&task_id).ok_or("unknown worker task")?;
                let attempt = task.worker_attempt.as_ref().ok_or("legacy task has no provider clock")?;
                if task.status != TaskStatus::Running || task.process.as_ref() != Some(&process)
                    || attempt.turn != turn || elapsed_ms < attempt.remaining_ms()
                    || !matches!(attempt.phase, crate::worker_attempt::Phase::Running { .. }) {
                    return Err("worker expiry does not match its owned execution clock".into());
                }
                next.stop(task_id, AttemptOutcome::TimedOut)?;
            }
            DispatchEvent::CheckRetry { task_id, candidate_sha256, retry, recovery_completion_sha256 } => {
                let task = next.task_mut(task_id)?;
                if task.status != (TaskStatus::Checking { candidate_sha256: candidate_sha256.clone() }) {
                    return Err("host retry belongs to another candidate or phase".into());
                }
                let attempt = task.worker_attempt.as_mut().ok_or("legacy task has no host retry allowance")?;
                let next_retry = attempt.infrastructure_retries.checked_add(1);
                if retry == 0 || (Some(retry) != next_retry && retry != attempt.infrastructure_retries) {
                    return Err("host retry does not follow the recorded infrastructure allowance".into());
                }
                attempt.check_retry_binding(&candidate_sha256, retry, recovery_completion_sha256.as_deref())?;
                if Some(retry) == next_retry {
                    attempt.apply(crate::worker_attempt::Event::InfrastructureRetry { candidate_sha256, recovery_completion_sha256 })?;
                }
            }
            DispatchEvent::CandidateReady { task_id, candidate_sha256, now_ms } => next.candidate(task_id, candidate_sha256, now_ms)?,
            DispatchEvent::AttemptStopping { task_id, outcome } => {
                if matches!(outcome, AttemptOutcome::TimedOut)
                    && next.tasks.get(&task_id).is_some_and(|t| t.worker_attempt.is_some()) {
                    return Err("worker timeout requires an owned provider expiry observation".into());
                }
                if matches!(outcome, AttemptOutcome::Accepted { .. }) {
                    return Err("scheduler observations cannot approve a worker candidate; use authoritative worker acceptance".into());
                }
                next.stop(task_id, outcome)?;
            }
            DispatchEvent::ProcessesReaped { task_id, now_ms } => next.reaped(task_id, now_ms)?,
            DispatchEvent::RetryPrepared { predecessor, assignment, refusal, graph, now_ms } => {
                let prior = next.tasks.get(&predecessor).ok_or("unknown retry predecessor")?.clone();
                if next.tasks.values().any(|t| t.retry_of == Some(predecessor)) { return Ok(()); }
                let retry = prior.pending_retry.ok_or("predecessor has no automatic retry")?;
                if !matches!(prior.status, TaskStatus::Resolved { .. }) {
                    return Err("automatic retry must wait for predecessor cleanup".into());
                }
                match (assignment, refusal) {
                    (Some(assignment), None) => {
                        if assignment.scope != prior.assignment.scope
                            || assignment.profile != prior.assignment.profile
                            || assignment.timeout_seconds != prior.assignment.timeout_seconds
                            || assignment.compiler != prior.assignment.compiler
                            || assignment.task_kind != prior.assignment.task_kind
                            || assignment.input_hashes != prior.assignment.input_hashes
                            || assignment.review != prior.assignment.review
                        { return Err("automatic retry changed the predecessor's assignment contract".into()); }
                        let id = next.reserve(prior.review_id, assignment, &graph, now_ms)?;
                        let task = next.tasks.get_mut(&id).unwrap();
                        task.retry_of = Some(predecessor);
                        task.retry = retry;
                        task.report = prior.report;
                        if let Some(mut seed) = next.exclusive_workers.get(&predecessor).cloned() {
                            let task = &next.tasks[&id];
                            task.retry.install(&mut seed.state);
                            if let Some(report) = &task.report { report.install(&mut seed.state); }
                            seed.state.worker_execution = task.assignment.request.inline()?.task_execution.clone();
                            seed.state.in_flight_request = Some(Box::new(task.assignment.request.inline()?.clone()));
                            next.exclusive_workers.insert(id, seed);
                        }
                    }
                    (None, Some(reason)) if !reason.trim().is_empty() => {
                        next.tasks.get_mut(&predecessor).unwrap().retry_refusal = Some(reason);
                    }
                    _ => return Err("retry preparation must contain either an assignment or a refusal".into()),
                }
                next.tasks.get_mut(&predecessor).unwrap().pending_retry = None;
            }
            DispatchEvent::Drain { reason } => next.drain(reason)?,
        }
        next.validate()?;
        // Private seeds are acceptance/retry inputs, not terminal receipts.
        // Keep them through process cleanup and automatic retry admission;
        // RetryPrepared above transfers the seed before clearing pending_retry.
        // Reclaim legacy terminal seeds only as part of a successful event,
        // never on load: publication recovery binds the exact saved state.
        // Continuations carry their own response and never read these seeds.
        next.exclusive_workers.retain(|id, _| next.tasks.get(id).is_none_or(|task|
            !matches!(task.status, TaskStatus::Resolved { .. }) || task.pending_retry.is_some()));
        *self = next;
        Ok(())
    }

    pub fn new(config: DispatchConfig, phase: Phase) -> Result<Self, String> {
        config.validate()?;
        Ok(Self { config, phase, tasks: BTreeMap::new(), receipts: BTreeMap::new(),
            next_task_id: 1, event_seq: 1, reviewed_event: 0, reviewer: None,
            worker_handoff: None, draining: None,
            verifiers: crate::dispatch_verifiers::VerifierQueue::default(),
            routing_pending: false, continuations: Default::default(), exclusive_workers: BTreeMap::new(), publications: BTreeMap::new() })
    }

    pub fn available_slots(&self) -> usize {
        if self.config.max_in_flight_tasks.is_some() && self.tasks.values().any(|t|
            t.status == TaskStatus::Reserved && t.execution_capacity_released && self.worker_execution_runnable(t.id)) {
            return 0; // Queued corrections take precedence over fresh reservations.
        }
        let execution = self.config.worker_slots.saturating_sub(self.tasks.values()
            .filter(|t| self.occupies_execution_capacity(t)).count());
        execution.min(self.config.max_in_flight_tasks.map(|limit| limit.saturating_sub(
            self.tasks.values().filter(|t| t.status.occupies_slot()).count())).unwrap_or(usize::MAX))
    }

    fn occupies_execution_capacity(&self, task: &WorkerTask) -> bool {
        task.status.occupies_slot() && (self.config.max_in_flight_tasks.is_none()
            || !task.execution_capacity_released)
    }

    /// Corrections can return from Checking to Reserved after another task
    /// took the released provider slot. They queue without losing scope or
    /// budget; this fresh native guard also applies immediately before launch.
    pub fn worker_execution_runnable(&self, id: TaskId) -> bool {
        let Some(task) = self.tasks.get(&id) else { return false; };
        if self.config.max_in_flight_tasks.is_none() || task.status != TaskStatus::Reserved
            || !task.execution_capacity_released {
            return true;
        }
        let active: Vec<_> = self.tasks.values().filter(|t| self.occupies_execution_capacity(t)).collect();
        if active.len() >= self.config.worker_slots { return false; }
        if let Some(limit) = self.config.heavy_worker_slots {
            self.task_is_lightweight(task) || active.iter().filter(|t| !self.task_is_lightweight(t)).count() < limit
        } else {
            !task.binding.is_sol() || self.config.sol_worker_slots.is_none_or(|limit|
                active.iter().filter(|t| t.binding.is_sol()).count() < limit)
        }
    }

    pub fn occupied_sol_slots(&self) -> usize {
        self.tasks.values().filter(|t| self.occupies_execution_capacity(t) && t.binding.is_sol()).count()
    }

    pub fn available_sol_slots(&self) -> usize {
        self.available_slots().min(self.config.sol_worker_slots
            .map(|limit| limit.saturating_sub(self.occupied_sol_slots())).unwrap_or(usize::MAX))
    }

    pub fn occupied_heavy_slots(&self) -> usize {
        self.tasks.values().filter(|t| self.occupies_execution_capacity(t) && !self.task_is_lightweight(t)).count()
    }

    pub fn available_heavy_slots(&self) -> usize {
        self.available_slots().min(self.config.heavy_worker_slots
            .map(|limit| limit.saturating_sub(self.occupied_heavy_slots())).unwrap_or(usize::MAX))
    }

    pub fn profile_capacity_available(&self, binding: &ModelProfile) -> bool {
        if self.config.heavy_worker_slots.is_some() {
            binding.lightweight == Some(true) || self.available_heavy_slots() > 0
        } else {
            !binding.is_sol() || self.available_sol_slots() > 0
        }
    }

    fn task_is_lightweight(&self, task: &WorkerTask) -> bool {
        task.binding.lightweight.unwrap_or_else(|| self.config.profiles.get(&task.assignment.profile)
            .is_some_and(|p| p.lightweight == Some(true)))
    }

    pub fn lightweight_limit(&self) -> Option<usize> {
        if self.config.profiles.values().any(|p| p.lightweight == Some(true))
            && self.config.profiles.values().any(|p| p.lightweight != Some(true))
        { self.config.lightweight_attempts_per_node } else { None }
    }

    /// Every launched attempt counts, irrespective of outcome or phase. Exact
    /// archived requests need not be hydrated: binding, scope and launch time
    /// remain in the ledger, which ordinary LastClean routing retains.
    pub fn lightweight_attempts_by_node(&self) -> BTreeMap<NodeId, usize> {
        let mut counts = BTreeMap::new();
        for task in self.tasks.values().filter(|t| t.launched_at_ms.is_some() && self.task_is_lightweight(t)) {
            for node in &task.assignment.scope.nodes { *counts.entry(node.clone()).or_default() += 1; }
        }
        counts
    }

    pub(crate) fn check_lightweight_budget(&self, assignment: &PreparedAssignment, binding: &ModelProfile,
        launching: Option<TaskId>) -> Result<(), String> {
        let Some(limit) = self.lightweight_limit() else { return Ok(()); };
        if binding.lightweight != Some(true) { return Ok(()); }
        let mut counts = self.lightweight_attempts_by_node();
        // Reservations hold budget but are not attempts. Cancelling before
        // launch frees that budget; replay/adoption never increments history.
        for task in self.tasks.values().filter(|t| Some(t.id) != launching && t.launched_at_ms.is_none()
            && t.status == TaskStatus::Reserved && self.task_is_lightweight(t)) {
            for node in &task.assignment.scope.nodes { *counts.entry(node.clone()).or_default() += 1; }
        }
        for node in &assignment.scope.nodes {
            if counts.get(node).copied().unwrap_or(0) >= limit {
                return Err(format!("lightweight attempt budget exhausted for {node} ({limit} per node, including reserved attempts); select an eligible non-lightweight profile or another node"));
            }
        }
        Ok(())
    }

    pub fn reviewer_runnable(&self) -> bool {
        self.reviewer.is_none() && self.event_seq > self.reviewed_event
            && !self.routing_pending
            && !self.tasks.values().any(|t| t.assignment.scope.exclusive && t.status.occupies_slot())
            && (self.available_slots() > 0 || self.draining.is_some())
            && !self.tasks.values().any(|t| t.pending_retry.is_some())
    }

    pub fn start_review(&mut self, request_id: u32) -> Result<(), String> {
        if !self.reviewer_runnable() { return Err("reviewer has no new actionable scheduler event".into()); }
        self.reviewer = Some(ReviewCursor { request_id, observed_event: self.event_seq });
        Ok(())
    }

    pub(crate) fn signal(&mut self) -> Result<(), String> {
        self.event_seq = self.event_seq.checked_add(1).ok_or("scheduler event counter exhausted")?;
        Ok(())
    }

    /// Process a batch in order. A refusal never consumes a slot. Response
    /// replay returns its durable receipt, including refusals, without launch.
    pub fn dispatch(
        &mut self, review_id: u32, response_sha256: String, assignments: Vec<PreparedAssignment>,
        graph: &DependencyGraph, now_ms: u64,
    ) -> Result<DispatchReceipt, String> {
        self.dispatch_prepared(review_id, response_sha256,
            assignments.into_iter().map(|assignment| PreparedItem::Ready { assignment }).collect(), graph, now_ms)
    }

    pub fn dispatch_prepared(
        &mut self, review_id: u32, response_sha256: String, items: Vec<PreparedItem>,
        graph: &DependencyGraph, now_ms: u64,
    ) -> Result<DispatchReceipt, String> {
        if let Some(receipt) = self.receipts.get(&review_id) {
            return if receipt.response_sha256 == response_sha256 { Ok(receipt.clone()) }
                else { Err("review ID already has a receipt for different response bytes".into()) };
        }
        let cursor = self.reviewer.as_ref().filter(|r| r.request_id == review_id)
            .ok_or("dispatch does not match the active reviewer")?.clone();
        let mut seen = BTreeSet::new();
        let mut decisions = Vec::new();
        for item in items {
            let id = match &item {
                PreparedItem::Ready { assignment } => assignment.assignment_id.clone(),
                PreparedItem::Refused { assignment_id, .. } => assignment_id.clone(),
            };
            let result = if !valid_key(&id) || !seen.insert(id.clone()) {
                Err("assignment ID must be safe and unique within this response".into())
            } else {
                match item {
                    PreparedItem::Ready { assignment } => self.reserve(review_id, assignment, graph, now_ms),
                    PreparedItem::Refused { reason, .. } => Err(reason),
                }
            };
            decisions.push(match result {
                Ok(task_id) => DispatchDecision { assignment_id: id, task_id: Some(task_id), rejection: None },
                Err(reason) => DispatchDecision { assignment_id: id, task_id: None, rejection: Some(reason) },
            });
        }
        let receipt = DispatchReceipt { review_id, response_sha256, decisions };
        self.receipts.insert(review_id, receipt.clone());
        self.reviewed_event = cursor.observed_event;
        self.reviewer = None;
        if receipt.decisions.iter().any(|d| d.rejection.is_some()) {
            self.signal()?; // Let the reviewer correct refused assignments.
        }
        Ok(receipt)
    }

    pub(crate) fn reserve(&mut self, review_id: u32, mut assignment: PreparedAssignment, graph: &DependencyGraph, now_ms: u64) -> Result<TaskId, String> {
        if let Some(reason) = &self.draining { return Err(format!("dispatch is draining: {reason}")); }
        if self.available_slots() == 0 { return Err("all worker slots are occupied, including attempts waiting for checking".into()); }
        if assignment.request.inline()?.phase != self.phase { return Err("assignment belongs to a different phase".into()); }
        if assignment.request.inline()?.kind != crate::model::RequestKind::Worker { return Err("a worker assignment must carry a Worker request".into()); }
        if assignment.base_snapshot.is_empty() || assignment.input_hashes.is_empty() {
            return Err("assignment has no captured base snapshot or input identities".into());
        }
        let binding = self.config.bind(&assignment.profile, assignment.timeout_seconds)?;
        if !self.profile_capacity_available(&binding) {
            return Err(if self.config.heavy_worker_slots.is_some() {
                "heavy worker limit reached, including checking and cleanup; choose an eligible lightweight profile or wait for a task to finish"
            } else {
                "all Sol worker slots are occupied, including checking and cleanup; choose an eligible non-Sol profile or wait for a Sol task to finish"
            }.into());
        }
        assignment.validate_permissions(&binding)?;
        self.check_lightweight_budget(&assignment, &binding, None)?;
        assignment.scope.validate(graph)?;
        validate_scope_conflicts(&assignment.scope, assignment.task_kind, graph, self.tasks.values()
            .filter(|task| task.status.occupies_slot())
            .map(|task| (format!("task {}", task.id), &task.assignment.scope, task.assignment.task_kind)))?;
        let id = self.next_task_id;
        self.next_task_id = id.checked_add(1).ok_or("task ID counter exhausted")?;
        let request = assignment.request.make_mut()?;
        request.task_execution = Some(TaskExecutionContext {
            task_id: id, profile: assignment.profile.clone(), binding: binding.clone(),
            compiler: assignment.compiler, task_kind: assignment.task_kind,
        });
        // Execution changes closure and NL-only permissions. Use the same
        // complete projection as expected_request, including feature overlays.
        crate::populate_request_prompt_contracts(request, None);
        let worker_attempt = request.worker_submission_policy.clone().map(|policy| {
            let contract = crate::trusted_artifact_rebind::digest(&serde_json::to_value(&*request).map_err(|e| e.to_string())?);
            crate::worker_attempt::Attempt::new(policy, contract, binding.timeout_seconds.saturating_mul(1000))
        }).transpose()?;
        self.tasks.insert(id, WorkerTask {
            id, review_id, assignment, binding, status: TaskStatus::Reserved, retry_of: None,
            retry: RetryContext::default(), reserved_at_ms: now_ms, launched_at_ms: None,
            process: None, checker_wait_ms: 0,
            report: None, pending_retry: None, retry_refusal: None,
            worker_attempt,
            execution_capacity_released: false,
        });
        Ok(id)
    }

    pub fn launched(&mut self, id: TaskId, process: ProcessIdentity, now_ms: u64) -> Result<(), String> {
        let task = self.tasks.get(&id).ok_or_else(|| format!("unknown worker task {id}"))?;
        if task.status == TaskStatus::Running && task.process.as_ref() == Some(&process) {
            return Ok(()); // adoption never restarts the deadline
        }
        if task.status != TaskStatus::Reserved { return Err("only a reserved task can launch".into()); }
        if !self.worker_execution_runnable(id) {
            return Err("worker execution capacity is occupied; retain the queued reservation".into());
        }
        if now_ms < task.reserved_at_ms || process.pid == 0 || process.boot_id.is_empty() {
            return Err("invalid launch time or process identity".into());
        }
        if task.launched_at_ms.is_none() {
            self.check_lightweight_budget(&task.assignment, &task.binding, Some(id))?;
        }
        let task = self.task_mut(id)?;
        task.launched_at_ms.get_or_insert(now_ms);
        task.process = Some(process);
        task.status = TaskStatus::Running;
        task.execution_capacity_released = false;
        Ok(())
    }

    /// Launcher ownership precedes permission, preparation and provider startup.
    /// Only the owned provider monitor starts the worker's execution allowance.
    pub fn provider_started(&mut self, id: TaskId, process: ProcessIdentity, started_at_ms: u64) -> Result<(), String> {
        let task = self.task_mut(id)?;
        if task.status != TaskStatus::Running || task.process.as_ref() != Some(&process) {
            return Err("provider start does not match the active task launcher".into());
        }
        if task.launched_at_ms.is_none_or(|t| started_at_ms < t) {
            return Err("provider execution cannot precede launcher authorization".into());
        }
        task.worker_attempt.as_mut().ok_or("legacy tasks do not accept provider-start observations")?
            .apply(crate::worker_attempt::Event::Launched { process, started_at_ms })
    }

    pub fn candidate(&mut self, id: TaskId, digest: String, now_ms: u64) -> Result<(), String> {
        let pipeline = self.config.max_in_flight_tasks.is_some();
        let task = self.task_mut(id)?;
        if task.status == (TaskStatus::Checking { candidate_sha256: digest.clone() }) { return Ok(()); }
        if digest.len() != 64 || !digest.bytes().all(|x| x.is_ascii_hexdigit()) {
            return Err("candidate requires an exact SHA-256 identity".into());
        }
        if let Some(attempt) = &mut task.worker_attempt {
            if task.status != TaskStatus::Submitted { return Err("candidate requires acknowledged worker completion".into()); }
            attempt.apply(crate::worker_attempt::Event::Sealed { candidate_sha256: digest.clone() })?;
        } else {
            if task.status != TaskStatus::Running { return Err("candidate received for a task that is not running".into()); }
            if task.deadline_ms().is_some_and(|d| now_ms >= d) { return Err("candidate arrived after the attempt deadline".into()); }
        }
        task.status = TaskStatus::Checking { candidate_sha256: digest };
        if pipeline && task.worker_attempt.is_some() {
            task.execution_capacity_released = true;
            self.signal()?;
        }
        Ok(())
    }

    pub fn turn_completed(&mut self, id: TaskId, completion: crate::worker_attempt::Completion) -> Result<(), String> {
        let task = self.task_mut(id)?;
        if !matches!(task.status, TaskStatus::Running | TaskStatus::Submitted) {
            return Err("worker completion cannot reopen a stopped or checked task".into());
        }
        if completion.session.provider != task.binding.provider || completion.session.model != task.binding.model
            || completion.session.reasoning_effort != task.binding.reasoning_effort {
            return Err("worker completion differs from its assigned model binding".into());
        }
        task.worker_attempt.as_mut().ok_or("legacy tasks do not accept submission lifecycle events")?
            .apply(crate::worker_attempt::Event::TurnCompleted { completion })?;
        task.status = TaskStatus::Submitted;
        Ok(())
    }

    pub fn worker_interrupted(&mut self, id: TaskId, interruption: crate::worker_attempt::Interruption) -> Result<(), String> {
        let task = self.task_mut(id)?;
        let attempt = task.worker_attempt.as_mut().ok_or("legacy worker has no transport clock")?;
        if attempt.last_interruption.as_ref() == Some(&interruption) { return Ok(()); }
        if task.status != TaskStatus::Running || task.process.as_ref() != Some(&interruption.process)
            || interruption.session.provider != task.binding.provider || interruption.session.model != task.binding.model
            || interruption.session.reasoning_effort != task.binding.reasoning_effort {
            return Err("transport interruption differs from the current worker binding".into());
        }
        attempt.apply(crate::worker_attempt::Event::Interrupted { interruption })?;
        if matches!(attempt.phase, crate::worker_attempt::Phase::Finished) {
            task.status = TaskStatus::Stopping { outcome: AttemptOutcome::Cancelled {
                reason: "Provider transport interrupted; bounded continuation allowance exhausted. Original source, conversation and receipts retained; no completed submission.".into() } };
        } else {
            task.status = TaskStatus::Reserved;
            task.process = None;
        }
        Ok(())
    }

    /// Called only after native host-result classification. The original task,
    /// scope, retry count and lightweight launch remain reserved.
    pub fn correct(&mut self, id: TaskId, candidate: String, feedback: String) -> Result<(), String> {
        let task = self.task_mut(id)?;
        if task.status != (TaskStatus::Checking { candidate_sha256: candidate.clone() }) {
            return Err("correction does not match the task's checked submission".into());
        }
        task.worker_attempt.as_mut().ok_or("legacy tasks do not permit within-attempt correction")?
            .apply(crate::worker_attempt::Event::Correction { candidate_sha256: candidate, feedback_sha256: feedback })?;
        task.status = TaskStatus::Reserved;
        task.process = None;
        Ok(())
    }

    pub fn stop(&mut self, id: TaskId, outcome: AttemptOutcome) -> Result<(), String> {
        let task = self.task_mut(id)?;
        if task.worker_attempt.is_some() && matches!(outcome, AttemptOutcome::TimedOut)
            && matches!(task.status, TaskStatus::Submitted | TaskStatus::Checking { .. }) {
            return Err("completed worker execution cannot time out during host checking".into());
        }
        match &task.status {
            TaskStatus::Stopping { outcome: prior } | TaskStatus::Resolved { outcome: prior, .. } => {
                return if prior == &outcome { Ok(()) } else { Err("attempt already has a different terminal outcome".into()) };
            }
            TaskStatus::Checking { .. } => {}
            _ if matches!(outcome, AttemptOutcome::Accepted { .. }) => {
                return Err("an unchecked candidate cannot be accepted".into());
            }
            _ => {}
        }
        task.status = TaskStatus::Stopping { outcome };
        if let Some(attempt) = &mut task.worker_attempt { attempt.apply(crate::worker_attempt::Event::Finished)?; }
        Ok(())
    }

    pub fn reaped(&mut self, id: TaskId, now_ms: u64) -> Result<(), String> {
        let task = self.task_mut(id)?;
        let outcome = match &task.status {
            TaskStatus::Stopping { outcome } => outcome.clone(),
            TaskStatus::Resolved { .. } => return Ok(()),
            _ => return Err("process cleanup cannot resolve an attempt without an outcome".into()),
        };
        task.status = TaskStatus::Resolved { outcome, resolved_at_ms: now_ms };
        self.signal()
    }

    pub fn expired(&self, now_ms: u64) -> Vec<TaskId> {
        self.tasks.values().filter(|t| matches!(t.status, TaskStatus::Running)
            && t.deadline_ms().is_some_and(|d| now_ms >= d)).map(|t| t.id).collect()
    }

    /// The authoritative publication path calls this immediately before its
    /// durable commit. The worker deadline was checked when the sealed candidate
    /// entered Checking; host acceptance and publication do not expire it.
    pub fn require_live_candidate(&self, id: TaskId, candidate: &str, _now_ms: u64) -> Result<(), String> {
        let task = self.tasks.get(&id).ok_or("unknown worker task")?;
        if task.status != (TaskStatus::Checking { candidate_sha256: candidate.into() }) {
            return Err("candidate no longer owns this attempt's acceptance slot".into());
        }
        Ok(())
    }

    pub fn drain(&mut self, reason: String) -> Result<(), String> {
        if reason.trim().is_empty() { return Err("drain requires a reason".into()); }
        if self.draining.as_ref() != Some(&reason) { self.draining = Some(reason); self.signal()?; }
        Ok(())
    }

    pub fn drained(&self) -> bool {
        self.draining.is_some() && !self.tasks.values().any(|t| t.status.occupies_slot())
            && self.reviewer.is_none()
            && self.verifiers.jobs.values().all(|j| matches!(j.status, crate::dispatch_verifiers::VerifierStatus::Resolved { .. }))
    }

    pub fn check_candidate_inputs(&self, id: TaskId, inputs: &BTreeMap<String, String>, proof_body_rebase_inputs: &BTreeMap<String, String>, actual_scope: &EditScope, graph: &DependencyGraph) -> Result<(), String> {
        self.check_candidate_inputs_with_fresh_readonly(id, inputs, proof_body_rebase_inputs, actual_scope, graph, false)
    }

    pub(crate) fn check_candidate_inputs_with_fresh_readonly(&self, id: TaskId,
        inputs: &BTreeMap<String, String>, proof_body_rebase_inputs: &BTreeMap<String, String>,
        actual_scope: &EditScope, graph: &DependencyGraph, fresh_readonly: bool) -> Result<(), String> {
        let task = self.tasks.get(&id).ok_or("unknown task")?;
        if !matches!(task.status, TaskStatus::Checking { .. }) { return Err("task is not awaiting acceptance".into()); }
        for (path, expected) in &task.assignment.input_hashes {
            // Only a fresh host check can replace launch-byte authority for a
            // readonly module. Imports and secondary definitions stay byte-bound.
            let readonly_node = path.strip_prefix("Tablet/").and_then(|file|
                file.strip_suffix(".lean").or_else(|| file.strip_suffix(".tex")))
                .filter(|name| !name.contains('/') && !name.contains('\\')
                    && !matches!(*name, "Preamble" | "Axioms" | "Assumptions" | "header"))
                .map(NodeId::from);
            let freshly_checked = fresh_readonly && inputs.contains_key(path)
                && readonly_node.is_some_and(|node| graph.edges.contains_key(&node)
                    && !actual_scope.nodes.contains(&node) && !task.assignment.scope.nodes.contains(&node));
            if !freshly_checked && !task.assignment.input_matches(path, expected, inputs, proof_body_rebase_inputs) {
                return Err(format!("stale task input: {path}"));
            }
        }
        if actual_scope.exclusive && !task.assignment.scope.exclusive
            || !actual_scope.shared_writes.is_subset(&task.assignment.scope.shared_writes)
        { return Err("candidate requires an unreserved global edit".into()); }
        for node in actual_scope.nodes.difference(&task.assignment.scope.nodes) {
            if !task.assignment.scope.helper_prefixes.iter().any(|p| node.as_str().starts_with(p)) {
                return Err(format!("candidate edits unreserved node {node}"));
            }
        }
        actual_scope.validate(graph)?;
        // Helpers can introduce a new path between two other active scopes,
        // so recheck every occupied pair, not just this candidate's roots.
        let scopes: Vec<_> = self.tasks.values().filter(|t| t.status.occupies_slot())
            .map(|t| (if t.id == id { actual_scope } else { &t.assignment.scope }, t.assignment.task_kind)).collect();
        for (i, a) in scopes.iter().enumerate() {
            for b in &scopes[i + 1..] {
                if graph.conflicts_for_tasks(a.0, a.1, b.0, b.1)? { return Err("candidate dependencies cross active task reservations".into()); }
            }
        }
        Ok(())
    }

    fn task_mut(&mut self, id: TaskId) -> Result<&mut WorkerTask, String> {
        self.tasks.get_mut(&id).ok_or_else(|| format!("unknown worker task {id}"))
    }

    pub fn validate(&self) -> Result<(), String> {
        self.config.validate()?;
        self.verifiers.validate(self.config.verifier_slots)?;
        if !self.continuations.is_empty() && !self.routing_pending {
            return Err("pending ordinary continuations require a routing wakeup".into());
        }
        if self.config.max_in_flight_tasks.is_some_and(|limit|
            self.tasks.values().filter(|t| t.status.occupies_slot()).count() > limit) {
            return Err("unresolved worker assignments exceed the in-flight task bound".into());
        }
        if self.config.max_in_flight_tasks.is_some() {
            if self.tasks.values().filter(|t| self.occupies_execution_capacity(t)).count() > self.config.worker_slots {
                return Err("worker execution reservations exceed worker capacity".into());
            }
            if self.config.heavy_worker_slots.is_some_and(|limit| self.occupied_heavy_slots() > limit)
                || self.config.heavy_worker_slots.is_none() && self.config.sol_worker_slots.is_some_and(|limit| self.occupied_sol_slots() > limit) {
                return Err("worker execution reservations exceed model capacity".into());
            }
        }
        if self.config.max_in_flight_tasks.is_none() && self.config.heavy_worker_slots.is_none()
            && self.tasks.values().filter(|t| t.status.occupies_slot()).count() > self.config.worker_slots {
            return Err("unresolved worker assignments exceed worker capacity".into());
        }
        if self.config.max_in_flight_tasks.is_none() && self.config.heavy_worker_slots.is_none()
            && self.config.sol_worker_slots.is_some_and(|limit| self.occupied_sol_slots() > limit) {
            return Err("unresolved Sol worker assignments exceed Sol worker capacity".into());
        }
        if self.reviewed_event > self.event_seq || self.reviewer.as_ref().is_some_and(|r| r.observed_event > self.event_seq) {
            return Err("review cursor is ahead of scheduler events".into());
        }
        for (id, receipt) in &self.publications {
            let valid_hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
            if receipt.event_id > self.event_seq || !valid_hash(&receipt.candidate_sha256)
                || !valid_hash(&receipt.payload_sha256)
                || self.tasks.get(id).is_none_or(|t| !matches!(t.status,
                    TaskStatus::Stopping { outcome: AttemptOutcome::Accepted { event_id } }
                    | TaskStatus::Resolved { outcome: AttemptOutcome::Accepted { event_id }, .. }
                    if event_id == receipt.event_id))
            { return Err("publication receipt disagrees with its accepted task".into()); }
        }
        for (id, t) in &self.tasks {
            if t.execution_capacity_released && (t.worker_attempt.as_ref().is_none_or(|a| a.submissions == 0)
                || matches!(t.status, TaskStatus::Running | TaskStatus::Submitted)
                || self.config.max_in_flight_tasks.is_none() && t.status.occupies_slot()) {
                return Err("released execution capacity lacks an owned completed submission".into());
            }
            if id != &t.id || *id >= self.next_task_id { return Err("invalid task ledger identity".into()); }
            if t.status.occupies_slot() && t.assignment.request.inline()?.phase != self.phase { return Err("active task crosses a phase boundary".into()); }
            if matches!(t.status, TaskStatus::Running | TaskStatus::Submitted | TaskStatus::Checking { .. })
                && (t.launched_at_ms.is_none() || t.process.is_none())
            { return Err("active task has no durable launch identity".into()); }
            if let Some(attempt) = &t.worker_attempt {
                use crate::worker_attempt::Phase as WorkerPhase;
                attempt.validate()?;
                let matches = match (&t.status, &attempt.phase) {
                    (TaskStatus::Reserved, WorkerPhase::Ready { .. })
                    | (TaskStatus::Running, WorkerPhase::Ready { .. })
                    | (TaskStatus::Reserved | TaskStatus::Running, WorkerPhase::TransportReady)
                    | (TaskStatus::Submitted, WorkerPhase::Submitted { .. })
                    | (TaskStatus::Stopping { .. } | TaskStatus::Resolved { .. }, WorkerPhase::Finished) => true,
                    (TaskStatus::Running, WorkerPhase::Running { process, .. }) => t.process.as_ref() == Some(process),
                    (TaskStatus::Checking { candidate_sha256: a }, WorkerPhase::Checking { candidate_sha256: b }) => a == b,
                    _ => false,
                };
                if !matches { return Err("worker lifecycle and dispatch reservation disagree".into()); }
            } else if t.status == TaskStatus::Submitted {
                return Err("legacy task has no submission lifecycle".into());
            }
            // Completed attempts retain the exact model they actually used.
            // Only active reservations must match the current launch menu.
            if (t.status.occupies_slot()
                && t.binding != self.config.bind(&t.assignment.profile, t.assignment.timeout_seconds)?)
                || !t.binding.allowed_tasks.contains(&t.assignment.task_kind)
            { return Err("task model binding or task permissions differ from launch configuration".into()); }
            t.assignment.validate_permissions(&t.binding)?;
            let context = TaskExecutionContext {
                task_id: *id, profile: t.assignment.profile.clone(), binding: t.binding.clone(),
                compiler: t.assignment.compiler, task_kind: t.assignment.task_kind,
            };
            if matches!(t.assignment.request, crate::dispatch_history::FrozenRequest::Stored(_)) {
                return Err("stored dispatch request must be verified and hydrated before validation".into());
            }
            if let crate::dispatch_history::FrozenRequest::Archived(reference) = &t.assignment.request {
                reference.validate()?;
                if !matches!(t.status, TaskStatus::Resolved { .. }) || t.pending_retry.is_some() {
                    return Err("unresolved or retrying task has an archived request".into());
                }
            }
            if t.assignment.request.execution() != Some(&context) {
                return Err("worker request does not carry its reserved task/model binding".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(names: &[&str]) -> BTreeSet<NodeId> { names.iter().map(|n| NodeId::from(*n)).collect() }
    fn graph() -> DependencyGraph {
        DependencyGraph { edges: BTreeMap::from([
            ("A".into(), nodes(&["C"])), ("B".into(), nodes(&["C"])),
            ("C".into(), nodes(&[])), ("D".into(), nodes(&["A"])),
        ]) }
    }
    fn assignment(id: &str, ns: &[&str]) -> PreparedAssignment {
        PreparedAssignment {
            assignment_id: id.into(), profile: "sol-xhigh".into(), timeout_seconds: None,
            compiler: CompilerCapability::Managed,
            task_kind: WorkerTaskKind::Ordinary,
            scope: EditScope { nodes: nodes(ns), ..EditScope::default() },
            proof_body_rebase_inputs: BTreeMap::new(),
            base_snapshot: "base".into(), input_hashes: BTreeMap::from([("C".into(), "content".into())]),
            request: Arc::new(WrapperRequest { phase: Phase::ProofFormalization, ..WrapperRequest::default() }).into(),
            review: None, authorized_worker: None,
        }
    }
    fn ledger(slots: usize) -> DispatchLedger {
        // Existing scheduling tests retain their explicit historical capacities.
        DispatchLedger::new(DispatchConfig { worker_slots: slots, sol_worker_slots: Some(2), compiler_slots: 2, ..DispatchConfig::default() }, Phase::ProofFormalization).unwrap()
    }
    fn launch(l: &mut DispatchLedger, id: TaskId, ms: u64) {
        l.launched(id, ProcessIdentity { pid: id as u32, start_ticks: 23, boot_id: "boot".into() }, ms).unwrap();
    }
    fn batch(l: &mut DispatchLedger, id: u32, assignments: Vec<PreparedAssignment>) -> DispatchReceipt {
        l.start_review(id).unwrap();
        l.dispatch(id, format!("response-{id}"), assignments, &graph(), 10).unwrap()
    }

    fn light_assignment(id: &str, node: &str) -> PreparedAssignment {
        let mut a = assignment(id, &[node]);
        a.profile = "luna-high".into(); a.task_kind = WorkerTaskKind::LeanClosure;
        a
    }

    fn finished_light(l: &mut DispatchLedger, node: &str, outcome: AttemptOutcome) -> TaskId {
        let id = l.reserve(1, light_assignment("light", node), &graph(), 1).unwrap();
        launch(l, id, 2); l.stop(id, outcome).unwrap(); l.reaped(id, 3).unwrap(); id
    }

    #[test]
    fn evidence_absence_is_exact_and_confined_to_explicit_exclusive_paths() {
        let path = "reference/model-evidence/new.rs";
        let mut a = assignment("evidence", &["A"]);
        a.scope.exclusive = true;
        a.scope.shared_writes.insert(path.into());
        let empty = BTreeMap::new();
        assert!(a.input_matches(path, "absent", &empty, &empty));
        let present = BTreeMap::from([(path.into(), "a".repeat(64))]);
        assert!(!a.input_matches(path, "absent", &present, &empty));
        assert!(!a.input_matches(path, &"a".repeat(64), &empty, &empty));
        assert!(!a.input_matches("reference/model-evidence/other.rs", "absent", &empty, &empty));
        a.scope.shared_writes.insert("reference/policy.json".into());
        assert!(!a.input_matches("reference/policy.json", "absent", &empty, &empty));
        a.scope.exclusive = false;
        assert!(!a.input_matches(path, "absent", &empty, &empty));
    }

    #[test]
    fn evidence_advisory_scope_is_exclusive_even_without_tablet_selection() {
        let mut input = frozen_scope_input();
        input.phase = Phase::Modeling;
        input.assignments.truncate(1);
        input.assignments[0].evidence_paths.insert("reference/model-evidence/new.rs".into());
        assert!(check_frozen_scopes(&input).unwrap()["repair"].contains("conflicts"));
        input.tasks.clear();
        assert!(check_frozen_scopes(&input).unwrap().is_empty());
        input.phase = Phase::ProofFormalization;
        assert!(check_frozen_scopes(&input).unwrap()["repair"].contains("ordinary Modeling"));
    }

    #[test]
    fn live_limits_preserve_inflight_work_and_use_explicit_strength() {
        let mut l = ledger(4);
        l.config.profiles.get_mut("sol-xhigh").unwrap().model = "custom-heavy".into();
        let g = DependencyGraph { edges: ["A", "B", "C", "D", "E"]
            .into_iter().map(|n| (n.into(), BTreeSet::new())).collect() };
        for node in ["A", "B", "C"] { l.reserve(1, assignment(node, &[node]), &g, 10).unwrap(); }
        launch(&mut l, 1, 20);
        l.candidate(1, "a".repeat(64), 30).unwrap();
        l.start_review(2).unwrap();
        let before = l.clone();
        let lower = DispatchEvent::AgentCapacityChanged { expected_worker_slots: 4,
            expected_heavy_worker_slots: None, worker_slots: 2, heavy_worker_slots: 1 };
        let event_bytes = serde_json::to_vec(&lower).unwrap();
        l.apply(lower.clone()).unwrap();
        assert_eq!(l.tasks, before.tasks);
        assert_eq!(l.reviewer, before.reviewer);
        assert_eq!(l.available_slots(), 0);
        assert_eq!(l.available_heavy_slots(), 0);
        assert_eq!(l.occupied_heavy_slots(), 3);
        assert!(l.reserve(3, assignment("four", &["D"]), &g, 40).is_err());
        l.validate().unwrap();
        let mut replay = before;
        replay.apply(serde_json::from_slice(&event_bytes).unwrap()).unwrap();
        assert_eq!(replay, l);
        assert!(l.apply(lower).unwrap_err().contains("changed since"));
        l.apply(DispatchEvent::AgentCapacityChanged { expected_worker_slots: 2,
            expected_heavy_worker_slots: Some(1), worker_slots: 4, heavy_worker_slots: 1 }).unwrap();
        assert!(l.reserve(3, assignment("four", &["D"]), &g, 40).unwrap_err().contains("heavy worker limit"));
        let mut light = assignment("light", &["D"]);
        light.profile = "luna-high".into(); light.task_kind = WorkerTaskKind::LeanClosure;
        l.config.profiles.get_mut("luna-high").unwrap().model = "custom-light-sol".into();
        l.reserve(3, light, &g, 40).unwrap();
        for id in [1, 2, 3] { l.stop(id, AttemptOutcome::TimedOut).unwrap(); l.reaped(id, 50).unwrap(); }
        assert_eq!(l.available_heavy_slots(), 1);
        l.reserve(3, assignment("next", &["E"]), &g, 60).unwrap();
        l.validate().unwrap();
    }

    #[test]
    fn shared_three_worker_one_heavy_capacity_survives_aliases_checking_and_restart() {
        let mut l = ledger(3);
        l.config.heavy_worker_slots = Some(1);
        l.config.profiles.insert("heavy-alias".into(), l.config.profiles["sol-xhigh"].clone());
        l.config.profiles.insert("light-alias".into(), l.config.profiles["luna-high"].clone());
        let g = DependencyGraph { edges: ["A", "B", "C", "D", "E"]
            .into_iter().map(|n| (n.into(), BTreeSet::new())).collect() };
        let heavy = l.reserve(1, assignment("heavy", &["A"]), &g, 10).unwrap();
        let mut second_heavy = assignment("second-heavy", &["E"]);
        second_heavy.profile = "heavy-alias".into();
        assert!(l.reserve(1, second_heavy.clone(), &g, 10).unwrap_err().contains("heavy worker limit"));
        launch(&mut l, heavy, 20);
        l.candidate(heavy, "a".repeat(64), 30).unwrap();
        let first_light = l.reserve(1, light_assignment("light", "B"), &g, 30).unwrap();
        let mut alias_light = light_assignment("alias-light", "C");
        alias_light.profile = "light-alias".into();
        l.reserve(1, alias_light, &g, 30).unwrap();
        assert_eq!((l.available_slots(), l.occupied_heavy_slots()), (0, 1));
        l.validate().unwrap();
        let mut resumed: DispatchLedger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
        assert_eq!(resumed, l);
        assert!(resumed.reserve(2, second_heavy.clone(), &g, 40).unwrap_err().contains("all worker slots"));
        resumed.stop(heavy, AttemptOutcome::Accepted { event_id: 7 }).unwrap();
        assert_eq!((resumed.available_slots(), resumed.available_heavy_slots()), (0, 0));
        resumed.reaped(heavy, 50).unwrap();
        assert_eq!((resumed.available_slots(), resumed.available_heavy_slots()), (1, 1));
        // There is no reserved heavy partition: a third light can use the slot.
        resumed.reserve(2, light_assignment("third-light", "D"), &g, 50).unwrap();
        assert_eq!((resumed.available_slots(), resumed.occupied_heavy_slots()), (0, 0));
        assert!(resumed.reserve(2, second_heavy.clone(), &g, 50).unwrap_err().contains("all worker slots"));
        resumed.stop(first_light, AttemptOutcome::Cancelled { reason: "before launch".into() }).unwrap();
        assert_eq!(resumed.available_slots(), 0, "stopping still retains the shared slot");
        resumed.reaped(first_light, 60).unwrap();
        resumed.reserve(2, second_heavy, &g, 60).unwrap();
        assert_eq!((resumed.available_slots(), resumed.occupied_heavy_slots()), (0, 1));
        resumed.validate().unwrap();
    }

    #[test]
    fn live_limits_require_explicit_classes_and_valid_capacities() {
        let mut l = ledger(4);
        let event = DispatchEvent::AgentCapacityChanged { expected_worker_slots: 4,
            expected_heavy_worker_slots: None, worker_slots: 3, heavy_worker_slots: 2 };
        l.config.profiles.get_mut("luna-high").unwrap().lightweight = None;
        let before = l.clone();
        assert!(l.apply(event).unwrap_err().contains("explicit lightweight"));
        assert_eq!(l, before);
        l.config.profiles.get_mut("luna-high").unwrap().lightweight = Some(true);
        for (workers, heavy) in [(0, 1), (1, 0)] {
            assert!(l.apply(DispatchEvent::AgentCapacityChanged { expected_worker_slots: 4,
                expected_heavy_worker_slots: None, worker_slots: workers, heavy_worker_slots: heavy }).is_err());
        }
    }

    #[test]
    fn configuration_changes_request_normal_checkpoint_only_when_changed() {
        std::thread::Builder::new().stack_size(16 * 1024 * 1024).spawn(|| {
        use crate::engine::{apply_event, ProtocolEvent, ProtocolCommand};
        let state = crate::ProtocolState { phase: Phase::ProofFormalization, dispatch: Some(ledger(4)), ..crate::ProtocolState::default() };
        let old_profiles = state.dispatch.as_ref().unwrap().config.profiles.clone();
        let mut profiles = old_profiles.clone();
        let light = profiles.get_mut("luna-high").unwrap();
        light.timeout_seconds = 7200;
        light.max_timeout_seconds = 7200;
        let events = [
            DispatchEvent::AgentCapacityChanged { expected_worker_slots: 4, expected_heavy_worker_slots: None,
                worker_slots: 8, heavy_worker_slots: 6 },
            DispatchEvent::WorkerCapacityChanged { expected_slots: 4, worker_slots: 8 },
            DispatchEvent::ExecutionPipelineChanged { expected_max_in_flight_tasks: None, max_in_flight_tasks: Some(8) },
            DispatchEvent::SolWorkerCapacityChanged { expected_slots: Some(2), sol_worker_slots: Some(6) },
            DispatchEvent::CompilerCapacityChanged { expected_slots: 2, compiler_slots: 6 },
            DispatchEvent::ModelProfilesChanged { expected_profiles: old_profiles.clone(), profiles },
        ];
        for event in events {
            let result = apply_event(state.clone(), ProtocolEvent::Dispatch { payload: event }).unwrap();
            assert_eq!(result.commands, vec![ProtocolCommand::CommitCheckpoint]);
        }
        let result = apply_event(state.clone(), ProtocolEvent::Dispatch { payload:
            DispatchEvent::WorkerCapacityChanged { expected_slots: 4, worker_slots: 4 } }).unwrap();
        assert!(result.commands.is_empty());
        let result = apply_event(state, ProtocolEvent::Dispatch { payload:
            DispatchEvent::ModelProfilesChanged { expected_profiles: old_profiles.clone(), profiles: old_profiles } }).unwrap();
        assert!(result.commands.is_empty());
        }).unwrap().join().unwrap();
    }

    #[test]
    fn worker_transport_continuation_preserves_reservation_and_has_no_submission() {
        let mut l = ledger(4);
        let mut a = light_assignment("repair", "A");
        a.request.make_mut().unwrap().worker_submission_policy = Some(Default::default());
        let id = l.reserve(1, a, &graph(), 1).unwrap();
        launch(&mut l, id, 2);
        let process = l.tasks[&id].process.clone().unwrap();
        l.provider_started(id, process.clone(), 10).unwrap();
        let original = l.tasks[&id].assignment.clone();
        let binding = l.tasks[&id].binding.clone();
        let stopped = crate::worker_attempt::Interruption { process, turn: 1, elapsed_ms: 500,
            evidence_sha256: "a".repeat(64), session: crate::worker_attempt::Session {
                provider: binding.provider, model: binding.model, reasoning_effort: binding.reasoning_effort,
                thread_id: "same-thread".into(), home: "/same/home".into() } };
        l.apply(DispatchEvent::WorkerInterrupted { task_id: id, interruption: stopped.clone() }).unwrap();
        assert_eq!(l.tasks[&id].status, TaskStatus::Reserved);
        assert!(l.tasks[&id].process.is_none());
        let attempt = l.tasks[&id].worker_attempt.as_ref().unwrap().clone();
        l.apply(DispatchEvent::WorkerInterrupted { task_id: id, interruption: stopped }).unwrap();
        assert_eq!(l.tasks[&id].worker_attempt.as_ref(), Some(&attempt));
        assert_eq!((attempt.turn, attempt.submissions, attempt.consumed_ms), (1, 0, 500));
        assert_eq!(l.tasks[&id].assignment, original);
        assert_eq!(l.lightweight_attempts_by_node()[&NodeId::from("A")], 1);
        assert!(l.tasks[&id].pending_retry.is_none());
        l = serde_json::from_value(serde_json::to_value(l).unwrap()).unwrap();
        launch(&mut l, id, 1000);
        l.provider_started(id, l.tasks[&id].process.clone().unwrap(), 1010).unwrap();
        l.validate().unwrap();
        assert_eq!(l.available_slots(), 3);
        assert_eq!(l.tasks[&id].assignment, original);
    }

    #[test]
    fn worker_submissions_retain_lightweight_attempt_scope_and_budget() {
        let mut l = ledger(4);
        let mut a = light_assignment("repair", "A");
        a.request.make_mut().unwrap().worker_submission_policy = Some(Default::default());
        let id = l.reserve(1, a, &graph(), 1).unwrap();
        launch(&mut l, id, 2);
        assert_eq!(l.tasks[&id].deadline_ms(), None);
        l.provider_started(id, l.tasks[&id].process.clone().unwrap(), 340_000).unwrap();
        assert_eq!(l.tasks[&id].deadline_ms(), Some(3_940_000));
        let original = l.tasks[&id].assignment.clone();
        let binding = l.tasks[&id].binding.clone();
        let c = crate::worker_attempt::Completion {
            process: l.tasks[&id].process.clone().unwrap(), turn: 1, elapsed_ms: 3_000_000, exhausted: false,
            response_sha256: "a".repeat(64), session: crate::worker_attempt::Session {
                provider: binding.provider, model: binding.model, reasoning_effort: binding.reasoning_effort,
                thread_id: "one-conversation".into(), home: "/private/task/home".into(),
            },
        };
        l.turn_completed(id, c).unwrap();
        assert!(l.expired(u64::MAX).is_empty());
        assert!(l.stop(id, AttemptOutcome::TimedOut).is_err());
        l.candidate(id, "b".repeat(64), 99_000_000).unwrap();
        assert!(l.stop(id, AttemptOutcome::TimedOut).is_err());
        l.correct(id, "b".repeat(64), "c".repeat(64)).unwrap();
        l = serde_json::from_value(serde_json::to_value(l).unwrap()).unwrap();
        launch(&mut l, id, 100_000_000);
        assert_eq!(l.tasks[&id].deadline_ms(), None);
        l.provider_started(id, l.tasks[&id].process.clone().unwrap(), 100_200_000).unwrap();
        assert_eq!(l.tasks[&id].deadline_ms(), Some(100_800_000));
        assert_eq!(l.lightweight_attempts_by_node()[&NodeId::from("A")], 1);
        assert_eq!(l.tasks[&id].assignment, original);
        assert_eq!(l.available_slots(), 3);
        l.validate().unwrap();
    }

    fn pipeline_graph() -> DependencyGraph {
        DependencyGraph { edges: (0..12).map(|n| (format!("N{n}").into(), BTreeSet::new())).collect() }
    }

    fn pipeline_assignment(n: usize, heavy: bool) -> PreparedAssignment {
        let node = format!("N{n}");
        let mut a = if heavy { assignment(&node, &[&node]) } else { light_assignment(&node, &node) };
        a.request.make_mut().unwrap().worker_submission_policy = Some(Default::default());
        a
    }

    fn pipeline_seal(l: &mut DispatchLedger, id: TaskId) {
        if l.tasks[&id].status == TaskStatus::Reserved { launch(l, id, 20); }
        let task = l.tasks[&id].clone();
        let process = task.process.unwrap();
        l.provider_started(id, process.clone(), 30).unwrap();
        l.turn_completed(id, crate::worker_attempt::Completion {
            process, turn: task.worker_attempt.as_ref().unwrap().turn, elapsed_ms: 1000, exhausted: false,
            response_sha256: "a".repeat(64), session: crate::worker_attempt::Session {
                provider: task.binding.provider, model: task.binding.model, reasoning_effort: task.binding.reasoning_effort,
                thread_id: format!("thread-{id}"), home: format!("/task/{id}"),
            },
        }).unwrap();
        l.candidate(id, "b".repeat(64), 2000).unwrap();
    }

    fn full_pipeline() -> DispatchLedger {
        let mut l = ledger(5);
        l.config.max_in_flight_tasks = Some(8);
        l.config.heavy_worker_slots = Some(2);
        let g = pipeline_graph();
        for n in 0..3 {
            let id = l.reserve(1, pipeline_assignment(n, false), &g, 10).unwrap();
            pipeline_seal(&mut l, id);
        }
        for n in 3..8 {
            let id = l.reserve(1, pipeline_assignment(n, n < 5), &g, 10).unwrap();
            launch(&mut l, id, 20);
        }
        l.validate().unwrap();
        l
    }

    #[test]
    fn pipeline_keeps_bounded_checking_scopes_and_acceptance_cleanup() {
        let mut l = full_pipeline();
        assert_eq!(l.available_slots(), 0);
        assert_eq!(l.occupied_heavy_slots(), 2);
        assert!(l.reserve(1, pipeline_assignment(8, false), &pipeline_graph(), 10).is_err());
        // Accepted checking work does not reacquire provider capacity while
        // five unrelated providers already own their execution reservations.
        l.stop(1, AttemptOutcome::Accepted { event_id: 3 }).unwrap();
        l.validate().unwrap();
        assert!(l.tasks[&1].execution_capacity_released);
        assert!(l.tasks[&1].status.occupies_slot());
        l.reaped(1, 2001).unwrap();
        assert_eq!(l.available_slots(), 0); // Still five provider reservations.
        l.validate().unwrap();
    }

    #[test]
    fn pipeline_repairs_queue_and_reacquire_exact_capacity_after_restart() {
        let mut l = full_pipeline();
        l.correct(1, "b".repeat(64), "c".repeat(64)).unwrap();
        l.correct(2, "b".repeat(64), "c".repeat(64)).unwrap();
        l.validate().unwrap();
        assert!(!l.worker_execution_runnable(1));
        let process = ProcessIdentity { pid: 90, start_ticks: 23, boot_id: "boot".into() };
        assert!(l.launched(1, process.clone(), 3000).unwrap_err().contains("capacity"));
        // A running cancellation holds its lease until descendant cleanup.
        l.stop(8, AttemptOutcome::Cancelled { reason: "stop".into() }).unwrap();
        assert!(!l.worker_execution_runnable(1));
        l.reaped(8, 3000).unwrap();
        l = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
        assert!(l.worker_execution_runnable(1));
        assert!(l.worker_execution_runnable(2));
        assert_eq!(l.available_slots(), 0); // Repairs precede fresh admission.
        l.launched(1, process, 3001).unwrap();
        assert!(!l.tasks[&1].execution_capacity_released);
        assert!(!l.worker_execution_runnable(2));
        l.validate().unwrap();
    }

    #[test]
    fn pipeline_repair_transport_retains_lease_budget_and_rejects_old_submission() {
        let mut l = full_pipeline();
        l.correct(1, "b".repeat(64), "c".repeat(64)).unwrap();
        l.stop(8, AttemptOutcome::Cancelled { reason: "stop".into() }).unwrap();
        l.reaped(8, 3000).unwrap();
        launch(&mut l, 1, 3010);
        let task = l.tasks[&1].clone();
        let process = task.process.unwrap();
        l.provider_started(1, process.clone(), 3020).unwrap();
        let session = crate::worker_attempt::Session { provider: task.binding.provider,
            model: task.binding.model, reasoning_effort: task.binding.reasoning_effort,
            thread_id: "thread-1".into(), home: "/task/1".into() };
        assert!(l.candidate(1, "b".repeat(64), 3030).is_err());
        assert!(l.turn_completed(1, crate::worker_attempt::Completion { process: process.clone(),
            turn: 1, elapsed_ms: 1000, exhausted: false, response_sha256: "a".repeat(64),
            session: session.clone() }).is_err());
        assert!(!l.tasks[&1].execution_capacity_released);
        l.worker_interrupted(1, crate::worker_attempt::Interruption { process, turn: 2,
            elapsed_ms: 500, evidence_sha256: "e".repeat(64), session }).unwrap();
        assert_eq!(l.tasks[&1].status, TaskStatus::Reserved);
        assert!(!l.tasks[&1].execution_capacity_released);
        let before = l.tasks[&1].worker_attempt.clone().unwrap();
        l = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
        launch(&mut l, 1, 4000);
        let process = l.tasks[&1].process.clone().unwrap();
        l.launched(1, process, 5000).unwrap(); // Exact launcher adoption.
        assert_eq!(l.tasks[&1].worker_attempt.as_ref().unwrap(), &before);
        assert_eq!((before.turn, before.submissions, before.consumed_ms), (2, 1, 1500));
        assert_eq!(l.available_slots(), 0);
        l.validate().unwrap();
    }

    #[test]
    fn pipeline_legacy_checking_and_exclusive_scopes_remain_reserved() {
        let mut l = ledger(1);
        l.config.max_in_flight_tasks = Some(2);
        let mut a = assignment("legacy", &["A"]);
        a.scope.exclusive = true;
        let id = l.reserve(1, a, &graph(), 1).unwrap();
        launch(&mut l, id, 2);
        l.candidate(id, "a".repeat(64), 3).unwrap();
        assert!(!l.tasks[&id].execution_capacity_released);
        assert_eq!(l.available_slots(), 0);
        assert!(!l.reviewer_runnable());
        l.drain("operator".into()).unwrap();
        assert!(!l.drained());
        l.validate().unwrap();
    }

    #[test]
    fn pipeline_free_provider_capacity_is_not_a_free_semantic_scope() {
        let mut l = ledger(2);
        l.config.max_in_flight_tasks = Some(4);
        let id = l.reserve(1, pipeline_assignment(0, false), &pipeline_graph(), 10).unwrap();
        pipeline_seal(&mut l, id);
        assert_eq!(l.available_slots(), 2);
        assert!(l.reserve(2, pipeline_assignment(0, false), &pipeline_graph(), 20).unwrap_err().contains("conflict"));
        let state = crate::ProtocolState { phase: Phase::ProofFormalization, stage: crate::Stage::HumanGate,
            dispatch: Some(l.clone()), ..Default::default() };
        assert!(crate::dispatch_routing::human_decision(&state, Default::default()).unwrap_err().contains("drained"));
        l.drain("operator".into()).unwrap();
        assert!(!l.drained());
    }

    #[test]
    fn pipeline_blocked_heavy_repair_leaves_light_capacity_available() {
        let mut l = ledger(5);
        l.config.max_in_flight_tasks = Some(8);
        l.config.heavy_worker_slots = Some(2);
        let g = pipeline_graph();
        let id = l.reserve(1, pipeline_assignment(0, true), &g, 10).unwrap();
        pipeline_seal(&mut l, id);
        for n in 1..3 { l.reserve(1, pipeline_assignment(n, true), &g, 10).unwrap(); }
        l.correct(id, "b".repeat(64), "c".repeat(64)).unwrap();
        assert!(!l.worker_execution_runnable(id));
        assert_eq!(l.available_slots(), 3);
        l.reserve(1, pipeline_assignment(3, false), &g, 10).unwrap();
        l.validate().unwrap();
    }

    #[test]
    fn pipeline_live_lowering_preserves_strict_leases_and_rejects_without_mutation() {
        let mut l = full_pipeline();
        let before = l.clone();
        assert!(l.apply(DispatchEvent::AgentCapacityChanged { expected_worker_slots: 5,
            expected_heavy_worker_slots: Some(2), worker_slots: 4, heavy_worker_slots: 2 })
            .unwrap_err().contains("cannot be lowered"));
        assert_eq!(l, before);
        assert!(l.apply(DispatchEvent::AgentCapacityChanged { expected_worker_slots: 5,
            expected_heavy_worker_slots: Some(2), worker_slots: 5, heavy_worker_slots: 1 }).is_err());
        assert_eq!(l, before);
        l.apply(DispatchEvent::AgentCapacityChanged { expected_worker_slots: 5,
            expected_heavy_worker_slots: Some(2), worker_slots: 6, heavy_worker_slots: 2 }).unwrap();
        assert_eq!(l.config.worker_slots, 6);
        assert_eq!(l.tasks, before.tasks);
    }

    #[test]
    fn pipeline_configuration_is_drained_compare_and_swap_and_legacy_omitted() {
        let mut l = ledger(2);
        assert!(serde_json::to_value(&l.config).unwrap().get("max_in_flight_tasks").is_none());
        let event = DispatchEvent::ExecutionPipelineChanged {
            expected_max_in_flight_tasks: None, max_in_flight_tasks: Some(4) };
        let bytes = serde_json::to_vec(&event).unwrap();
        let before = l.clone();
        l.apply(event.clone()).unwrap();
        let mut replay = before;
        replay.apply(serde_json::from_slice(&bytes).unwrap()).unwrap();
        assert_eq!(l, replay);
        assert!(l.apply(event).unwrap_err().contains("changed since"));
        assert!(l.apply(DispatchEvent::ExecutionPipelineChanged {
            expected_max_in_flight_tasks: Some(4), max_in_flight_tasks: Some(1) }).is_err());
        let id = l.reserve(1, pipeline_assignment(0, false), &pipeline_graph(), 10).unwrap();
        pipeline_seal(&mut l, id);
        assert!(l.apply(DispatchEvent::ExecutionPipelineChanged {
            expected_max_in_flight_tasks: Some(4), max_in_flight_tasks: None }).unwrap_err().contains("drained"));
    }

    #[test]
    fn historical_worker_tasks_keep_legacy_deadline_and_wire_shape() {
        let mut l = ledger(2);
        let id = l.reserve(1, assignment("old", &["A"]), &graph(), 1).unwrap();
        launch(&mut l, id, 2);
        let value = serde_json::to_value(&l.tasks[&id]).unwrap();
        assert!(value.get("worker_attempt").is_none());
        assert!(value["assignment"]["request"].get("worker_submission_policy").is_none());
        let deadline = l.tasks[&id].deadline_ms().unwrap();
        assert!(l.candidate(id, "a".repeat(64), deadline).is_err());
        assert_eq!(l.expired(deadline), vec![id]);
    }

    fn submission_clock_fixture() -> (DispatchLedger, TaskId, ProcessIdentity) {
        let mut l = ledger(4);
        let mut a = light_assignment("clock", "A");
        a.request.make_mut().unwrap().worker_submission_policy = Some(Default::default());
        let id = l.reserve(1, a, &graph(), 1).unwrap();
        launch(&mut l, id, 2);
        let process = l.tasks[&id].process.clone().unwrap();
        (l, id, process)
    }

    #[test]
    fn submission_config_is_opt_in_strict_and_requires_resumable_profiles() {
        let legacy = DispatchConfig::default();
        assert!(legacy.worker_submission_policy.is_none());
        let historical = serde_json::to_value(&legacy).unwrap();
        assert!(historical.get("worker_submission_policy").is_none());
        assert_eq!(serde_json::to_value(serde_json::from_value::<DispatchConfig>(historical.clone()).unwrap()).unwrap(), historical);
        let mut configured = legacy.clone();
        configured.worker_submission_policy = Some(Default::default());
        configured.validate().unwrap();
        assert_eq!(serde_json::from_value::<DispatchConfig>(serde_json::to_value(&configured).unwrap()).unwrap(), configured);
        for provider in ["deepseek", "kimi"] {
            let mut other = configured.clone();
            for profile in other.profiles.values_mut() { profile.provider = provider.into(); }
            assert!(other.validate().is_ok());
        }
        for provider in ["claude", "unknown", "Codex", "codex "] {
            let mut other = configured.clone();
            other.profiles.get_mut("luna-high").unwrap().provider = provider.into();
            assert!(other.validate().unwrap_err().contains("Codex"));
        }
        for policy in [crate::worker_attempt::Policy { version: 2, ..Default::default() },
                       crate::worker_attempt::Policy { max_submissions: 0, ..Default::default() }] {
            let mut invalid = configured.clone(); invalid.worker_submission_policy = Some(policy);
            assert!(invalid.validate().is_err());
        }
        let mut unknown = historical; unknown["worker_submissions"] = serde_json::json!(true);
        assert!(serde_json::from_value::<DispatchConfig>(unknown).is_err());
        let mut unknown = serde_json::to_value(&configured).unwrap();
        unknown["worker_submission_policy"]["reset_budget"] = serde_json::json!(true);
        assert!(serde_json::from_value::<DispatchConfig>(unknown).is_err());
        let mut zero_retries = configured;
        zero_retries.worker_submission_policy.as_mut().unwrap().max_infrastructure_retries = 0;
        zero_retries.validate().unwrap();
    }

    #[test]
    fn provider_clock_excludes_startup_and_rejects_timeout_bypasses_atomically() {
        let (mut l, id, process) = submission_clock_fixture();
        let remaining = l.tasks[&id].worker_attempt.as_ref().unwrap().remaining_ms();
        assert!(l.expired(u64::MAX).is_empty(), "launch permission and bootstrap have no execution deadline");
        let before = l.clone();
        for event in [
            DispatchEvent::AttemptStopping { task_id: id, outcome: AttemptOutcome::TimedOut },
            DispatchEvent::WorkerExpired { task_id: id, process: process.clone(), turn: 1, elapsed_ms: remaining },
            DispatchEvent::ProviderStarted { task_id: id, process: ProcessIdentity { pid: 999, ..process.clone() }, started_at_ms: 100 },
            DispatchEvent::ProviderStarted { task_id: id, process: process.clone(), started_at_ms: 1 },
        ] {
            assert!(l.apply(event).is_err()); assert_eq!(l, before);
        }
        let start = DispatchEvent::ProviderStarted { task_id: id, process: process.clone(), started_at_ms: 86_400_000 };
        l.apply(start.clone()).unwrap();
        let running = l.clone();
        l.apply(start).unwrap(); assert_eq!(l, running);
        assert_eq!(l.tasks[&id].deadline_ms(), Some(86_400_000 + remaining));
        for (owner, turn, elapsed_ms) in [(process.clone(), 1, remaining - 1),
            (process.clone(), 2, remaining), (ProcessIdentity { pid: 999, ..process.clone() }, 1, remaining)] {
            assert!(l.apply(DispatchEvent::WorkerExpired { task_id: id, process: owner, turn, elapsed_ms }).is_err());
            assert_eq!(l, running);
        }
        l.apply(DispatchEvent::WorkerExpired { task_id: id, process, turn: 1, elapsed_ms: remaining }).unwrap();
        assert_eq!(l.tasks[&id].status, TaskStatus::Stopping { outcome: AttemptOutcome::TimedOut });
    }

    #[test]
    fn check_retry_is_candidate_bound_idempotent_and_never_resets_worker_budget() {
        let (mut l, id, process) = submission_clock_fixture();
        l.provider_started(id, process.clone(), 100).unwrap();
        let binding = l.tasks[&id].binding.clone();
        l.turn_completed(id, crate::worker_attempt::Completion {
            process, turn: 1, elapsed_ms: 1234, exhausted: false, response_sha256: "a".repeat(64),
            session: crate::worker_attempt::Session { provider: binding.provider, model: binding.model,
                reasoning_effort: binding.reasoning_effort, thread_id: "same-thread".into(), home: "/same/home".into() },
        }).unwrap();
        l.candidate(id, "b".repeat(64), u64::MAX).unwrap();
        let original = l.tasks[&id].worker_attempt.clone().unwrap();
        for retry in 1..=2 {
            let before = l.clone();
            for (candidate, ordinal) in [("c".repeat(64), retry), ("b".repeat(64), 0), ("b".repeat(64), retry + 1)] {
                assert!(l.apply(DispatchEvent::CheckRetry { task_id: id, candidate_sha256: candidate, retry: ordinal, recovery_completion_sha256: None }).is_err());
                assert_eq!(l, before);
            }
            let event = DispatchEvent::CheckRetry { task_id: id, candidate_sha256: "b".repeat(64), retry, recovery_completion_sha256: None };
            l.apply(event.clone()).unwrap(); let once = l.clone();
            l.apply(event).unwrap(); assert_eq!(l, once);
            let mut expected = original.clone(); expected.infrastructure_retries = retry;
            assert_eq!(l.tasks[&id].worker_attempt.as_ref(), Some(&expected));
        }
        let capped = l.clone();
        for retry in [1, 3] {
            assert!(l.apply(DispatchEvent::CheckRetry { task_id: id, candidate_sha256: "b".repeat(64), retry, recovery_completion_sha256: None }).is_err());
            assert_eq!(l, capped);
        }
        assert_eq!(l.lightweight_attempts_by_node()[&NodeId::from("A")], 1);
        assert_eq!(l.available_slots(), 3);
    }

    #[test]
    fn check_retry_old_wire_omits_recovery_completion() {
        let old = serde_json::json!({"kind":"check_retry", "task_id":1,
            "candidate_sha256":"b".repeat(64), "retry":1});
        let event: DispatchEvent = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(event).unwrap(), old);
    }

    #[test]
    fn lightweight_budget_defaults_are_explicit_and_old_wire_stays_exact() {
        let cfg = DispatchConfig::default();
        assert_eq!(cfg.lightweight_attempts_per_node, Some(2));
        assert_eq!(cfg.profiles["luna-high"].lightweight, Some(true));
        assert_eq!(cfg.profiles["sol-xhigh"].lightweight, Some(false));
        let mut old = serde_json::to_value(cfg).unwrap();
        old.as_object_mut().unwrap().remove("lightweight_attempts_per_node");
        for p in old["profiles"].as_object_mut().unwrap().values_mut() { p.as_object_mut().unwrap().remove("lightweight"); }
        let restored: DispatchConfig = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(&restored).unwrap(), old);
        let mut l = DispatchLedger::new(restored, Phase::ProofFormalization).unwrap();
        assert_eq!(l.lightweight_limit(), None);
        for _ in 0..3 { finished_light(&mut l, "A", AttemptOutcome::TimedOut); }
        assert!(l.lightweight_attempts_by_node().is_empty(), "names never designate model strength");
    }

    #[test]
    fn lightweight_budget_shared_across_aliases_and_all_launched_outcomes() {
        for outcome in [AttemptOutcome::TimedOut, AttemptOutcome::Stuck { reason: "stuck".into() },
            AttemptOutcome::Rejected { reason: "invalid".into() }, AttemptOutcome::Cancelled { reason: "after launch".into() }] {
            let mut l = ledger(4);
            finished_light(&mut l, "A", outcome.clone());
            l.config.profiles.insert("alias".into(), l.config.profiles["luna-high"].clone());
            let mut a = light_assignment("alias", "A"); a.profile = "alias".into();
            let id = l.reserve(2, a.clone(), &graph(), 10).unwrap();
            launch(&mut l, id, 11);
            let process = l.tasks[&id].process.clone().unwrap();
            l.launched(id, process, 12).unwrap(); // adoption/replay does not spend twice
            l.stop(id, outcome).unwrap(); l.reaped(id, 13).unwrap();
            assert_eq!(l.lightweight_attempts_by_node()[&NodeId::from("A")], 2);
            let receipt = batch(&mut l, 3, vec![a, light_assignment("other", "B")]);
            assert!(receipt.decisions[0].rejection.as_ref().unwrap().contains("lightweight attempt budget exhausted for A"));
            assert!(receipt.decisions[1].task_id.is_some());
            assert_eq!(l.dispatch(3, "response-3".into(), vec![], &graph(), 30).unwrap(), receipt);
            assert!(l.reserve(4, assignment("strong", &["A"]), &graph(), 31).is_ok());
            assert!(!l.lightweight_attempts_by_node().contains_key(&NodeId::from("C")), "read-only dependency is not charged");
        }
    }

    #[test]
    fn lightweight_reservation_holds_budget_and_prelaunch_cancel_frees_it() {
        let mut l = ledger(4); l.config.lightweight_attempts_per_node = Some(1);
        let a = light_assignment("one", "A");
        let id = l.reserve(1, a.clone(), &graph(), 1).unwrap();
        assert!(l.lightweight_attempts_by_node().is_empty());
        assert!(l.check_lightweight_budget(&a, &l.config.profiles["luna-high"], None).is_err());
        assert!(l.reserve(2, a.clone(), &graph(), 2).unwrap_err().contains("budget exhausted"));
        l.stop(id, AttemptOutcome::Cancelled { reason: "before launch".into() }).unwrap(); l.reaped(id, 3).unwrap();
        let id = l.reserve(2, a.clone(), &graph(), 4).unwrap();
        launch(&mut l, id, 5); // own reservation is not double-counted
        assert_eq!(l.lightweight_attempts_by_node()[&NodeId::from("A")], 1);
        l.stop(id, AttemptOutcome::TimedOut).unwrap(); l.reaped(id, 6).unwrap();
        assert!(l.reserve(3, a, &graph(), 7).is_err());
    }

    #[test]
    fn lightweight_multinode_scope_counts_each_node_and_single_menu_is_uncapped() {
        let mut l = ledger(4);
        l.config.profiles.get_mut("luna-high").unwrap().allowed_tasks.insert(WorkerTaskKind::Ordinary);
        let mut a = assignment("multi", &["A", "B"]); a.profile = "luna-high".into();
        let id = l.reserve(1, a, &graph(), 1).unwrap();
        launch(&mut l, id, 2); l.stop(id, AttemptOutcome::TimedOut).unwrap(); l.reaped(id, 3).unwrap();
        assert_eq!(l.lightweight_attempts_by_node(), BTreeMap::from([("A".into(), 1), ("B".into(), 1)]));
        finished_light(&mut l, "A", AttemptOutcome::TimedOut);
        assert!(l.reserve(2, light_assignment("exhausted", "A"), &graph(), 4).is_err());
        l.config.profiles.remove("sol-xhigh");
        assert_eq!(l.lightweight_limit(), None);
        finished_light(&mut l, "A", AttemptOutcome::TimedOut);
        l.config.profiles.insert("heavy".into(), DispatchConfig::default().profiles["sol-xhigh"].clone());
        assert_eq!(l.lightweight_limit(), Some(2));
        assert!(l.reserve(2, light_assignment("again", "A"), &graph(), 5).is_err());
        l.config.lightweight_attempts_per_node = None;
        finished_light(&mut l, "A", AttemptOutcome::TimedOut);
    }

    #[test]
    fn lightweight_exhaustion_refuses_automatic_retry_and_unblocks_reviewer() {
        let mut l = ledger(4);
        finished_light(&mut l, "A", AttemptOutcome::Stuck { reason: "first".into() });
        let id = finished_light(&mut l, "A", AttemptOutcome::Rejected { reason: "invalid".into() });
        l.tasks.get_mut(&id).unwrap().pending_retry = Some(RetryContext::default());
        let event = DispatchEvent::RetryPrepared { predecessor: id,
            assignment: Some(l.tasks[&id].assignment.clone()), refusal: None, graph: graph(), now_ms: 10 };
        let before = l.clone(); assert!(l.apply(event).unwrap_err().contains("budget exhausted")); assert_eq!(l, before);
        let state = crate::ProtocolState { dispatch: Some(l.clone()), ..Default::default() };
        let prepared = crate::dispatch_coordinator::prepare_retry(&state, id, graph(), BTreeMap::new(), BTreeMap::new(), "base".into(), 11).unwrap().unwrap();
        assert!(prepared.worker.is_none() && prepared.task_id.is_none());
        l.apply(prepared.event).unwrap();
        assert!(l.tasks[&id].pending_retry.is_none());
        assert!(l.tasks[&id].retry_refusal.as_ref().unwrap().contains("non-lightweight"));
        assert!(l.reviewer_runnable());
    }

    #[test]
    fn lightweight_legacy_migration_archival_and_lastclean_retain_accounting() {
        use crate::dispatch_history::{FrozenRequest, RequestReference};
        let mut l = ledger(4); l.config.lightweight_attempts_per_node = None;
        for p in l.config.profiles.values_mut() { p.lightweight = None; }
        for _ in 0..3 { finished_light(&mut l, "A", AttemptOutcome::TimedOut); }
        let frozen = serde_json::to_value(&l.tasks).unwrap();
        let mut profiles = l.config.profiles.clone(); profiles.get_mut("luna-high").unwrap().lightweight = Some(true);
        l.apply(DispatchEvent::ModelProfilesChanged { expected_profiles: l.config.profiles.clone(), profiles }).unwrap();
        l.apply(DispatchEvent::LightweightBudgetChanged { expected_limit: None, lightweight_attempts_per_node: Some(2) }).unwrap();
        assert_eq!(serde_json::to_value(&l.tasks).unwrap(), frozen, "migration never rewrites old bindings or requests");
        assert_eq!(l.lightweight_attempts_by_node()[&NodeId::from("A")], 3);
        assert!(l.reserve(4, light_assignment("again", "A"), &graph(), 10).is_err());
        let mut aliases = l.config.profiles.clone();
        let renamed = aliases.remove("luna-high").unwrap(); aliases.insert("alias".into(), renamed);
        assert!(l.apply(DispatchEvent::ModelProfilesChanged { expected_profiles: l.config.profiles.clone(), profiles: aliases }).unwrap_err().contains("retain profile"));
        for task in l.tasks.values_mut() {
            let reference = RequestReference::from_request(task.assignment.request.inline().unwrap()).unwrap().0;
            task.assignment.request = FrozenRequest::Archived(reference);
        }
        l.validate().unwrap();
        let wire = serde_json::to_vec(&l).unwrap();
        let restored: DispatchLedger = serde_json::from_slice(&wire).unwrap();
        restored.validate().unwrap(); assert_eq!(serde_json::to_vec(&restored).unwrap(), wire);
        let mut state = crate::ProtocolState::default();
        state.last_clean_verifier_mirror_ready = true; state.last_clean_local_closure_mirror_ready = true;
        state.dispatch = Some(restored);
        assert_eq!(state.apply_last_clean_reset(), Ok(true));
        let mut restored = state.dispatch.unwrap();
        assert!(restored.reserve(5, light_assignment("after-rewind", "A"), &graph(), 11).is_err());
    }

    #[test]
    fn lightweight_policy_events_require_drained_compare_and_swap() {
        let mut l = ledger(4);
        let event = DispatchEvent::LightweightBudgetChanged { expected_limit: Some(2), lightweight_attempts_per_node: Some(3) };
        l.apply(serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap()).unwrap();
        assert_eq!(l.lightweight_limit(), Some(3));
        assert!(l.apply(event).is_err());
        assert!(l.apply(DispatchEvent::LightweightBudgetChanged { expected_limit: Some(3), lightweight_attempts_per_node: Some(0) }).is_err());
        l.reserve(1, light_assignment("reserved", "A"), &graph(), 1).unwrap();
        let before = l.clone();
        assert!(l.apply(DispatchEvent::LightweightBudgetChanged { expected_limit: Some(3), lightweight_attempts_per_node: None }).unwrap_err().contains("drained"));
        assert_eq!(l, before);
    }

    fn frozen_scope_input() -> FrozenScopeCheck {
        let review = |id: &str, node: &str| FrozenAssignmentScope { evidence_paths: BTreeSet::new(), id: id.into(), task_kind: WorkerTaskKind::Ordinary,
            review: ReviewResponse { next_active: Some("D".into()), next_mode: TaskMode::Restructure,
                authorized_nodes: nodes(&[node]), ..Default::default() } };
        FrozenScopeCheck { phase: Phase::ProofFormalization, active_node: Some("A".into()),
            current_present_nodes: nodes(&["A", "B", "C", "D"]), current_deps: graph().edges,
            tasks: vec![FrozenTaskScope { id: 17, task_kind: WorkerTaskKind::Ordinary, status: TaskStatus::Running,
                scope: EditScope { nodes: nodes(&["D"]), ..Default::default() } }],
            assignments: vec![review("repair", "C"), review("independent", "B")] }
    }

    #[test]
    fn frozen_modeling_writer_scopes_are_serial_but_later_phases_keep_independent_work() {
        let mut input = frozen_scope_input();
        input.tasks.clear();
        for (assignment, node) in input.assignments.iter_mut().zip(["A", "B"]) {
            assignment.review.authorized_nodes = nodes(&[node]);
        }
        input.phase = Phase::Modeling;
        let errors = check_frozen_scopes(&input).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors["independent"], "authorized edit scope conflicts with assignment repair");
        for phase in [Phase::TheoremStating, Phase::RevisionStating, Phase::ProofFormalization] {
            input.phase = phase;
            assert!(check_frozen_scopes(&input).unwrap().is_empty(), "{phase:?}");
        }
    }

    #[test]
    fn frozen_scope_checks_transitive_conflict_without_treating_anchor_as_editable() {
        let mut input = frozen_scope_input();
        let errors = check_frozen_scopes(&input).unwrap();
        // Reviewer841: occupied ancestor D depends on authorized helper C.
        assert_eq!(errors.len(), 1);
        assert!(errors["repair"].contains("task 17 reads a dependency written by proposed task (ordinary)"));
        assert!(errors["repair"].contains("D -> A -> C"));
        // Both assignments name occupied D as anchor; B is still independent.
        input.tasks[0].status = TaskStatus::Stopping { outcome: AttemptOutcome::TimedOut };
        assert_eq!(check_frozen_scopes(&input).unwrap(), errors);
        input.tasks[0].status = TaskStatus::Resolved {
            outcome: AttemptOutcome::TimedOut, resolved_at_ms: 1 };
        input.assignments.truncate(1);
        assert!(check_frozen_scopes(&input).unwrap().is_empty());
        input.current_deps.get_mut(&NodeId::from("D")).unwrap().insert("Unknown".into());
        input.tasks[0].status = TaskStatus::Running;
        assert!(check_frozen_scopes(&input).unwrap()["repair"].contains("incomplete"));
    }

    #[test]
    fn frozen_scope_checks_local_active_and_rejects_conflicting_peer_only_in_feedback() {
        let mut input = frozen_scope_input();
        input.tasks.clear();
        for assignment in &mut input.assignments {
            assignment.review.next_mode = TaskMode::Local;
            assignment.review.next_active = Some("B".into());
            assignment.review.authorized_nodes.clear();
        }
        let errors = check_frozen_scopes(&input).unwrap();
        assert_eq!(errors["independent"], "authorized edit scope conflicts with assignment repair");
        // An earlier peer can fail admission for a fact absent from preflight.
        // This feedback must not poison the later native partial admission.
        let mut bad = assignment("repair", &["B"]);
        bad.input_hashes.clear();
        let mut l = ledger(4);
        let receipt = batch(&mut l, 841, vec![bad, assignment("independent", &["B"])]);
        assert!(receipt.decisions[0].rejection.is_some());
        assert_eq!(receipt.decisions[1].task_id, Some(1));
    }

    #[test]
    fn frozen_scope_feedback_is_not_current_admission_or_a_complete_observation_graph() {
        let mut l = ledger(4);
        l.reserve(1, assignment("running", &["D"]), &graph(), 1).unwrap();
        let mut input = frozen_scope_input();
        input.tasks[0].id = 1;
        assert!(check_frozen_scopes(&input).unwrap().contains_key("repair"));
        l.stop(1, AttemptOutcome::TimedOut).unwrap();
        l.reaped(1, 2).unwrap();
        assert!(l.reserve(2, assignment("repair", &["C"]), &graph(), 3).is_ok());

        let mut l = ledger(4);
        let mut observed = graph();
        observed.edges.get_mut(&NodeId::from("D")).unwrap().insert("B".into());
        l.reserve(1, assignment("running", &["D"]), &observed, 1).unwrap();
        input.assignments.remove(0); // semantic-only graph permits B
        assert!(check_frozen_scopes(&input).unwrap().is_empty());
        let error = l.reserve(2, assignment("independent", &["B"]), &observed, 2).unwrap_err();
        assert!(error.starts_with("authorized edit scope conflicts with task 1:"));
        assert!(error.contains("task 1 reads a dependency written by proposed task (ordinary)"));
        assert!(error.contains("D -> B"));
    }

    #[test]
    fn sol_capacity_is_shared_across_profiles_and_held_until_reaping() {
        let mut l = ledger(4);
        let mut alias = l.config.profiles["sol-xhigh"].clone();
        alias.model = "gpt-6.1-sol".into();
        l.config.profiles.insert("other-name".into(), alias);
        let g = DependencyGraph { edges: ["A", "B", "C", "D"]
            .into_iter().map(|n| (n.into(), BTreeSet::new())).collect() };
        let a = assignment("one", &["A"]);
        let mut b = assignment("two", &["B"]); b.profile = "other-name".into();
        let c = assignment("three", &["C"]);
        let mut luna = assignment("cheap", &["D"]);
        luna.profile = "luna-high".into(); luna.task_kind = WorkerTaskKind::LeanClosure;
        l.start_review(1).unwrap();
        let receipt = l.dispatch(1, "response".into(), vec![a, b, c.clone(), luna], &g, 10).unwrap();
        assert_eq!(receipt.decisions.iter().map(|d| d.task_id).collect::<Vec<_>>(),
            vec![Some(1), Some(2), None, Some(3)]);
        assert!(receipt.decisions[2].rejection.as_ref().unwrap().contains("Sol worker slots"));
        assert_eq!(l.available_slots(), 1);
        assert_eq!(l.available_sol_slots(), 0);
        let mut corrupted = l.clone(); corrupted.config.sol_worker_slots = Some(1);
        assert!(corrupted.validate().unwrap_err().contains("Sol worker capacity"));
        launch(&mut l, 1, 20);
        l.candidate(1, "a".repeat(64), 30).unwrap();
        assert!(l.reserve(2, c.clone(), &g, 35).unwrap_err().contains("Sol worker slots"));
        l.stop(1, AttemptOutcome::TimedOut).unwrap();
        assert!(l.reserve(2, c.clone(), &g, 36).unwrap_err().contains("Sol worker slots"));
        l.reaped(1, 40).unwrap();
        assert_eq!(l.available_sol_slots(), 1);
        assert_eq!(l.reserve(2, c, &g, 41).unwrap(), 4);
        l.validate().unwrap();
    }

    #[test]
    fn sol_capacity_applies_to_automatic_retries_without_losing_pending_retry() {
        let mut l = ledger(4); l.config.sol_worker_slots = Some(1);
        l.reserve(1, assignment("one", &["A"]), &graph(), 10).unwrap();
        l.tasks.get_mut(&1).unwrap().pending_retry = Some(RetryContext::default());
        l.stop(1, AttemptOutcome::TimedOut).unwrap(); l.reaped(1, 20).unwrap();
        l.reserve(2, assignment("other", &["B"]), &graph(), 21).unwrap();
        let event = DispatchEvent::RetryPrepared { predecessor: 1,
            assignment: Some(l.tasks[&1].assignment.clone()), refusal: None, graph: graph(), now_ms: 40 };
        let before = l.clone();
        let state = crate::ProtocolState { dispatch: Some(l.clone()), ..crate::ProtocolState::default() };
        assert!(crate::dispatch_coordinator::prepare_retry(&state, 1, graph(),
            BTreeMap::new(), BTreeMap::new(), "base".into(), 40).unwrap().is_none());
        assert_eq!(state.dispatch.as_ref().unwrap(), &before);
        assert!(l.apply(event.clone()).unwrap_err().contains("Sol worker slots"));
        assert_eq!(l, before);
        l.stop(2, AttemptOutcome::TimedOut).unwrap(); l.reaped(2, 30).unwrap();
        l.apply(event).unwrap();
        assert_eq!(l.tasks[&3].retry_of, Some(1));
        assert_eq!(l.available_sol_slots(), 0);
    }

    #[test]
    fn sol_capacity_legacy_wire_and_explicit_override_round_trip() {
        assert_eq!(DispatchConfig::default().sol_worker_slots, Some(6));
        let mut value = serde_json::to_value(DispatchConfig::default()).unwrap();
        value.as_object_mut().unwrap().remove("sol_worker_slots");
        let legacy: DispatchConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(legacy.sol_worker_slots, None);
        assert_eq!(serde_json::to_value(&legacy).unwrap(), value);
        let mut l = DispatchLedger::new(legacy, Phase::ProofFormalization).unwrap();
        let g = DependencyGraph { edges: ["A", "B", "C", "D"]
            .into_iter().map(|n| (n.into(), BTreeSet::new())).collect() };
        for node in ["A", "B", "C", "D"] { l.reserve(1, assignment(node, &[node]), &g, 10).unwrap(); }
        let wire = serde_json::to_vec(&l).unwrap();
        let restored: DispatchLedger = serde_json::from_slice(&wire).unwrap();
        restored.validate().unwrap();
        assert_eq!(serde_json::to_vec(&restored).unwrap(), wire);
        l.config.sol_worker_slots = Some(4); l.validate().unwrap();
        assert_eq!(l.occupied_sol_slots(), 4);
        let value = serde_json::to_value(&l).unwrap();
        let restored: DispatchLedger = serde_json::from_value(value).unwrap(); restored.validate().unwrap();
    }

    #[test]
    fn sol_capacity_change_is_drained_compare_and_swap() {
        let mut l = ledger(4); l.config.sol_worker_slots = None;
        let event = DispatchEvent::SolWorkerCapacityChanged { expected_slots: None, sol_worker_slots: Some(2) };
        let mut expected = l.clone(); expected.config.sol_worker_slots = Some(2); expected.event_seq += 1;
        l.apply(serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap()).unwrap();
        assert_eq!(l, expected);
        assert!(l.apply(event).is_err()); assert_eq!(l, expected);
        l.apply(DispatchEvent::SolWorkerCapacityChanged { expected_slots: Some(2), sol_worker_slots: None }).unwrap();
        assert_eq!(l.config.sol_worker_slots, None);
        l.apply(DispatchEvent::SolWorkerCapacityChanged { expected_slots: None, sol_worker_slots: Some(2) }).unwrap();
        assert!(l.apply(DispatchEvent::SolWorkerCapacityChanged { expected_slots: Some(2), sol_worker_slots: Some(0) }).is_err());
        l.start_review(1).unwrap();
        let event = DispatchEvent::SolWorkerCapacityChanged { expected_slots: Some(2), sol_worker_slots: Some(3) };
        let before = l.clone(); assert!(l.apply(event.clone()).is_err()); assert_eq!(l, before);
        l.dispatch(1, "response".into(), vec![assignment("one", &["A"])], &graph(), 10).unwrap();
        let before = l.clone(); assert!(l.apply(event).is_err()); assert_eq!(l, before);
    }

    #[test]
    fn completed_request_archival_is_exact_atomic_and_retry_safe() {
        use crate::dispatch_history::{FrozenRequest, RequestReference};
        let mut l = ledger(2);
        let first = l.reserve(1, assignment("one", &["A"]), &graph(), 10).unwrap();
        let second = l.reserve(1, assignment("two", &["B"]), &graph(), 10).unwrap();
        let reference = RequestReference::from_request(l.tasks[&first].assignment.request.inline().unwrap()).unwrap().0;
        let event = DispatchEvent::RequestsArchived { requests: BTreeMap::from([(first, reference.clone())]), verified: BTreeMap::new() };
        for status in [TaskStatus::Reserved, TaskStatus::Running,
            TaskStatus::Checking { candidate_sha256: "a".repeat(64) },
            TaskStatus::Stopping { outcome: AttemptOutcome::TimedOut }] {
            l.tasks.get_mut(&first).unwrap().status = status;
            let before = l.clone();
            assert!(l.apply(event.clone()).is_err());
            assert_eq!(l, before);
        }
        l.tasks.get_mut(&first).unwrap().status = TaskStatus::Resolved { outcome: AttemptOutcome::TimedOut, resolved_at_ms: 20 };
        l.tasks.get_mut(&first).unwrap().pending_retry = Some(RetryContext::default());
        let before = l.clone();
        assert!(l.apply(event.clone()).is_err());
        assert_eq!(l, before);
        l.tasks.get_mut(&first).unwrap().pending_retry = None;
        // A later bad item cannot leave the earlier request compacted.
        let wrong = DispatchEvent::RequestsArchived { requests: BTreeMap::from([(first, reference.clone()), (99, reference.clone())]), verified: BTreeMap::new() };
        let before = l.clone();
        assert!(l.apply(wrong).is_err());
        assert_eq!(l, before);
        let sibling = l.tasks[&second].clone();
        l.apply(event.clone()).unwrap();
        assert!(matches!(l.tasks[&first].assignment.request, FrozenRequest::Archived(_)));
        assert_eq!(l.tasks[&second], sibling);
        let archived = l.clone();
        l.apply(event).unwrap();
        assert_eq!(l, archived, "exact archive replay is idempotent");
        let mut wrong = reference;
        wrong.task_execution.as_mut().unwrap().task_id = second;
        assert!(l.apply(DispatchEvent::RequestsArchived { requests: BTreeMap::from([(first, wrong)]), verified: BTreeMap::new() }).is_err());
        assert_eq!(l, archived);
        l.tasks.get_mut(&first).unwrap().pending_retry = Some(RetryContext::default());
        assert!(l.validate().is_err(), "archived evidence cannot become a retry authority");
    }

    #[test]
    fn completed_request_history_replays_and_exports_exact_objects() {
        use crate::dispatch_history;
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut l = ledger(1);
        l.start_review(1).unwrap();
        let mut event = crate::ProtocolEvent::Dispatch { payload: DispatchEvent::AssignmentsDispatched {
            review_id: 1, response_sha256: "a".repeat(64), assignments: vec![assignment("one", &["A"])], graph: graph(), now_ms: 10,
        } };
        let original = event.clone();
        dispatch_history::externalize_event(source.path(), &mut event).unwrap();
        let stored_bytes = serde_json::to_vec(&event).unwrap();
        assert!(stored_bytes.len() < serde_json::to_vec(&original).unwrap().len());
        dispatch_history::hydrate_event(source.path(), &mut event).unwrap();
        assert_eq!(event, original);
        if let crate::ProtocolEvent::Dispatch { payload } = event { l.apply(payload).unwrap(); }
        let request = l.tasks[&1].assignment.request.inline().unwrap().clone();
        l.tasks.get_mut(&1).unwrap().status = TaskStatus::Resolved { outcome: AttemptOutcome::TimedOut, resolved_at_ms: 20 };
        let mut state = crate::ProtocolState { dispatch: Some(l), ..Default::default() };
        let archive = dispatch_history::archive_event(source.path(), &state).unwrap().unwrap();
        let legacy = state.clone();
        state.dispatch.as_mut().unwrap().apply(archive.clone()).unwrap();
        let mut replayed = legacy;
        replayed.dispatch.as_mut().unwrap().apply(archive).unwrap();
        assert_eq!(replayed, state);
        dispatch_history::export_state_objects(source.path(), destination.path(), &state).unwrap();
        let restored: crate::ProtocolState = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        dispatch_history::verify_state(destination.path(), &restored).unwrap();
        let mut frozen = restored.dispatch.unwrap().tasks[&1].assignment.request.clone();
        dispatch_history::hydrate(destination.path(), &mut frozen).unwrap();
        assert_eq!(frozen.inline().unwrap(), &request);
        std::fs::remove_dir_all(destination.path().join(dispatch_history::DIRECTORY)).unwrap();
        assert!(dispatch_history::verify_state(destination.path(), &state).is_err());
    }

    #[test]
    fn capacity_defaults_and_historical_wire_remain_explicit() {
        let current = DispatchConfig::default();
        assert_eq!((current.worker_slots, current.sol_worker_slots, current.compiler_slots), (8, Some(6), 6));
        let historical = ledger(4).config;
        let wire = serde_json::to_vec(&historical).unwrap();
        let restored: DispatchConfig = serde_json::from_slice(&wire).unwrap();
        assert_eq!(restored, historical);
        assert_eq!((restored.worker_slots, restored.sol_worker_slots, restored.compiler_slots), (4, Some(2), 2));
    }

    #[test]
    fn worker_capacity_change_is_drained_compare_and_swap_and_replayable() {
        let mut original = ledger(4);
        let id = original.reserve(1, assignment("history", &["A"]), &graph(), 10).unwrap();
        let event = DispatchEvent::WorkerCapacityChanged { expected_slots: 4, worker_slots: 8 };
        for status in [TaskStatus::Reserved, TaskStatus::Running, TaskStatus::Submitted,
            TaskStatus::Checking { candidate_sha256: "a".repeat(64) },
            TaskStatus::Stopping { outcome: AttemptOutcome::TimedOut }] {
            let mut busy = original.clone();
            busy.tasks.get_mut(&id).unwrap().status = status;
            let before = busy.clone();
            assert!(busy.apply(event.clone()).is_err());
            assert_eq!(busy, before);
        }
        original.stop(id, AttemptOutcome::TimedOut).unwrap();
        original.reaped(id, 20).unwrap();
        let mut reviewing = original.clone();
        reviewing.start_review(2).unwrap();
        let before = reviewing.clone();
        assert!(reviewing.apply(event.clone()).is_err());
        assert_eq!(reviewing, before);
        let protocol_event = crate::ProtocolEvent::Dispatch { payload: event.clone() };
        let wire = serde_json::to_vec(&protocol_event).unwrap();
        let mut replayed = original.clone();
        if let crate::ProtocolEvent::Dispatch { payload } = serde_json::from_slice(&wire).unwrap() {
            replayed.apply(payload).unwrap();
        } else { panic!("expected dispatch event"); }
        let mut expected = original.clone();
        expected.config.worker_slots = 8;
        expected.event_seq += 1;
        assert_eq!(replayed, expected);
        assert!(replayed.apply(event).is_err());
        assert_eq!(replayed, expected);
        assert!(replayed.apply(DispatchEvent::WorkerCapacityChanged { expected_slots: 8, worker_slots: 0 }).is_err());
        assert_eq!(replayed, expected);
        replayed.apply(DispatchEvent::WorkerCapacityChanged { expected_slots: 8, worker_slots: 8 }).unwrap();
        assert_eq!(replayed, expected);
        replayed.apply(DispatchEvent::WorkerCapacityChanged { expected_slots: 8, worker_slots: 4 }).unwrap();
        assert_eq!(replayed.config.worker_slots, 4);
        assert_eq!(replayed.tasks, original.tasks);
    }

    #[test]
    fn worker_capacity_change_waits_for_running_and_checking_verifiers() {
        use crate::dispatch_verifiers::{VerifierIdentity, VerifierLane};
        let mut queued = ledger(4);
        let id = queued.verifiers.enqueue(VerifierIdentity {
            lane: VerifierLane::Correspondence, panel_sha256: "panel".into(),
            input_hashes: BTreeMap::from([("A.lean".into(), "source".into())]),
            compiler: CompilerCapability::Managed,
        }, true, false).unwrap();
        let event = DispatchEvent::WorkerCapacityChanged { expected_slots: 4, worker_slots: 8 };
        let mut running = queued.clone();
        running.verifiers.launch(id, ProcessIdentity {
            pid: 123, start_ticks: 456, boot_id: "boot".into(),
        }, 10, running.config.verifier_slots).unwrap();
        let before = running.clone();
        assert!(running.apply(event.clone()).is_err());
        assert_eq!(running, before);
        running.verifiers.candidate(id, "a".repeat(64)).unwrap();
        let before = running.clone();
        assert!(running.apply(event.clone()).is_err());
        assert_eq!(running, before);
        let jobs = queued.verifiers.clone();
        queued.apply(event).unwrap();
        assert_eq!(queued.config.worker_slots, 8);
        assert_eq!(queued.verifiers, jobs);
    }

    #[test]
    fn compiler_capacity_change_preserves_contracts_and_rejects_busy_or_stale_updates() {
        let mut l = ledger(4);
        let mut expected = l.clone();
        expected.config.compiler_slots = 4;
        expected.event_seq += 1;
        let event = DispatchEvent::CompilerCapacityChanged { expected_slots: 2, compiler_slots: 4 };
        let wire = serde_json::to_vec(&event).unwrap();
        l.apply(serde_json::from_slice(&wire).unwrap()).unwrap();
        assert_eq!(l, expected);
        assert!(l.apply(event).is_err());
        assert_eq!(l, expected);
        assert!(l.apply(DispatchEvent::CompilerCapacityChanged { expected_slots: 4, compiler_slots: 0 }).is_err());
        assert_eq!(l, expected);
        l.start_review(100).unwrap();
        let reviewing = l.clone();
        assert!(l.apply(DispatchEvent::CompilerCapacityChanged { expected_slots: 4, compiler_slots: 2 }).is_err());
        assert_eq!(l, reviewing);
        l.dispatch(100, "response".into(), vec![assignment("a", &["A"])], &graph(), 10).unwrap();
        let busy = l.clone();
        assert!(l.apply(DispatchEvent::CompilerCapacityChanged { expected_slots: 4, compiler_slots: 2 }).is_err());
        assert_eq!(l, busy);
        assert_eq!(l.tasks[&1].assignment, busy.tasks[&1].assignment);
    }

    #[test]
    fn model_profile_change_preserves_history_and_rejects_busy_stale_or_invalid_updates() {
        let mut l = ledger(4);
        l.config.profiles.get_mut("sol-xhigh").unwrap().model = "gpt-5.6-sol".into();
        let previous = l.config.profiles.clone();
        let mut profiles = previous.clone();
        profiles.get_mut("sol-xhigh").unwrap().model = "gpt-6.1-sol".into();
        let event = DispatchEvent::ModelProfilesChanged { expected_profiles: previous.clone(), profiles: profiles.clone() };
        l.start_review(100).unwrap();
        let reviewing = l.clone();
        assert!(l.apply(event.clone()).is_err());
        assert_eq!(l, reviewing);
        l.dispatch(100, "response".into(), vec![assignment("a", &["A"])], &graph(), 10).unwrap();
        let busy = l.clone();
        assert!(l.apply(event.clone()).is_err());
        assert_eq!(l, busy);
        l.stop(1, AttemptOutcome::Cancelled { reason: "operator".into() }).unwrap();
        l.reaped(1, 20).unwrap();
        let mut expected = l.clone();
        expected.config.profiles = profiles.clone();
        expected.event_seq += 1;
        let wire = serde_json::to_vec(&event).unwrap();
        l.apply(serde_json::from_slice(&wire).unwrap()).unwrap();
        assert_eq!(l, expected);
        assert_eq!(l.tasks[&1].binding.model, "gpt-5.6-sol");
        l.validate().unwrap();
        assert_eq!(l.config.bind("sol-xhigh", None).unwrap().model, "gpt-6.1-sol");
        assert!(l.apply(event).is_err());
        assert_eq!(l, expected);
        assert!(l.apply(DispatchEvent::ModelProfilesChanged {
            expected_profiles: profiles.clone(), profiles: BTreeMap::new(),
        }).is_err());
        assert_eq!(l, expected);
        l.apply(DispatchEvent::ModelProfilesChanged {
            expected_profiles: profiles.clone(), profiles,
        }).unwrap();
        assert_eq!(l, expected);
    }

    #[test]
    fn compiler_capacity_change_waits_for_running_and_checking_verifiers() {
        use crate::dispatch_verifiers::{VerifierIdentity, VerifierLane};
        let mut queued = ledger(4);
        let id = queued.verifiers.enqueue(VerifierIdentity {
            lane: VerifierLane::Correspondence, panel_sha256: "panel".into(),
            input_hashes: BTreeMap::from([("A.lean".into(), "source".into())]),
            compiler: CompilerCapability::Managed,
        }, true, false).unwrap();
        let event = DispatchEvent::CompilerCapacityChanged { expected_slots: 2, compiler_slots: 4 };
        let mut running = queued.clone();
        running.verifiers.launch(id, ProcessIdentity {
            pid: 123, start_ticks: 456, boot_id: "boot".into(),
        }, 10, running.config.verifier_slots).unwrap();
        let before = running.clone();
        assert!(running.apply(event.clone()).is_err());
        assert_eq!(running, before);
        let profiles = running.config.profiles.clone();
        assert!(running.apply(DispatchEvent::ModelProfilesChanged {
            expected_profiles: profiles.clone(), profiles,
        }).is_err());
        assert_eq!(running, before);
        running.verifiers.candidate(id, "a".repeat(64)).unwrap();
        let before = running.clone();
        assert!(running.apply(event.clone()).is_err());
        assert_eq!(running, before);
        let profiles = running.config.profiles.clone();
        assert!(running.apply(DispatchEvent::ModelProfilesChanged {
            expected_profiles: profiles.clone(), profiles,
        }).is_err());
        assert_eq!(running, before);
        // Queued jobs hold no compiler or process reservation and keep their
        // exact verifier identity when the drained compiler limit changes.
        let jobs = queued.verifiers.clone();
        queued.apply(event).unwrap();
        assert_eq!(queued.config.compiler_slots, 4);
        assert_eq!(queued.verifiers, jobs);
    }

    #[test]
    fn frozen_request_sharing_preserves_legacy_wire_and_roundtrip() {
        let mut prepared = assignment("shared", &["A"]);
        prepared.request.make_mut().unwrap().worker_contract =
            serde_json::json!({"frozen": ["source", "closure", "policy"]});
        let shared = prepared.clone();
        assert!(Arc::ptr_eq(prepared.request.inline_arc().unwrap(), shared.request.inline_arc().unwrap()));
        let legacy = Box::new(prepared.request.inline().unwrap().clone());
        assert_eq!(serde_json::to_vec(&prepared.request).unwrap(), serde_json::to_vec(&legacy).unwrap());
        assert_eq!(serde_json::to_vec_pretty(&prepared.request).unwrap(), serde_json::to_vec_pretty(&legacy).unwrap());
        let wire = serde_json::to_vec(&prepared).unwrap();
        let restored: PreparedAssignment = serde_json::from_slice(&wire).unwrap();
        assert_eq!(restored, prepared);
        assert_eq!(serde_json::to_vec(&restored).unwrap(), wire);
        // Persistence retains values, not process-local pointer identity.
        assert!(!Arc::ptr_eq(restored.request.inline_arc().unwrap(), prepared.request.inline_arc().unwrap()));
    }

    #[test]
    fn reservation_binds_shared_request_without_mutating_prepared_contract() {
        let prepared = assignment("shared", &["A"]);
        let frozen = prepared.request.clone();
        let original = serde_json::to_vec(&frozen).unwrap();
        let mut l = ledger(2);
        let id = l.reserve(1, prepared, &graph(), 10).unwrap();
        let bound = &l.tasks[&id].assignment.request;
        assert!(!Arc::ptr_eq(frozen.inline_arc().unwrap(), bound.inline_arc().unwrap()));
        assert!(frozen.inline().unwrap().task_execution.is_none());
        assert_eq!(serde_json::to_vec(&frozen).unwrap(), original);
        let mut expected = frozen.inline().unwrap().clone();
        expected.task_execution = bound.inline().unwrap().task_execution.clone();
        crate::populate_request_prompt_contracts(&mut expected, None);
        assert_eq!(bound.inline().unwrap(), &expected);

        let before_launch = l.clone();
        assert!(Arc::ptr_eq(before_launch.tasks[&id].assignment.request.inline_arc().unwrap(), bound.inline_arc().unwrap()));
        l.apply(DispatchEvent::Launched {
            task_id: id,
            process: ProcessIdentity { pid: 123, start_ticks: 456, boot_id: "boot".into() },
            now_ms: 20,
        }).unwrap();
        assert_eq!(before_launch.tasks[&id].status, TaskStatus::Reserved);
        assert_eq!(l.tasks[&id].status, TaskStatus::Running);
        assert!(Arc::ptr_eq(before_launch.tasks[&id].assignment.request.inline_arc().unwrap(), l.tasks[&id].assignment.request.inline_arc().unwrap()));
        let before_failure = l.clone();
        assert!(l.apply(DispatchEvent::CandidateReady {
            task_id: 99, candidate_sha256: "candidate".into(), now_ms: 30,
        }).is_err());
        assert_eq!(l, before_failure);
        assert!(Arc::ptr_eq(l.tasks[&id].assignment.request.inline_arc().unwrap(), before_failure.tasks[&id].assignment.request.inline_arc().unwrap()));
        assert_eq!(serde_json::to_vec(&frozen).unwrap(), original);
    }

    #[test]
    fn shared_unchanged_prerequisites_are_allowed_but_full_scopes_conflict() {
        let mut l = ledger(4);
        l.config.sol_worker_slots = Some(4);
        let r = batch(&mut l, 100, vec![assignment("a", &["A"]), assignment("b", &["B"]),
            assignment("ancestor", &["C"]), assignment("scope", &["B", "D"])]);
        assert_eq!(r.decisions.iter().map(|d| d.task_id).collect::<Vec<_>>(), vec![Some(1), Some(2), None, None]);
        assert!(r.decisions[2].rejection.as_ref().unwrap().contains("conflicts"));
        l.validate().unwrap();
    }

    #[test]
    fn checker_wait_and_process_cleanup_keep_slot_and_reservation() {
        let mut l = ledger(1);
        batch(&mut l, 100, vec![assignment("a", &["A"])]);
        launch(&mut l, 1, 20);
        l.candidate(1, "a".repeat(64), 30).unwrap();
        assert!(l.require_live_candidate(1, &"a".repeat(64), 31).is_ok());
        assert!(l.require_live_candidate(1, &"b".repeat(64), 31).is_err());
        let deadline = l.tasks[&1].deadline_ms().unwrap();
        assert!(l.require_live_candidate(1, &"a".repeat(64), deadline + 1).is_ok());
        assert!(l.expired(deadline + 1).is_empty());
        let state = crate::model::ProtocolState { dispatch: Some(l.clone()), ..Default::default() };
        assert!(crate::dispatch_coordinator::status(&state).unwrap()["tasks"]["1"]["deadline_ms"].is_null());
        assert_eq!(l.available_slots(), 0);
        assert!(!l.reviewer_runnable());
        l.stop(1, AttemptOutcome::Accepted { event_id: 55 }).unwrap();
        assert_eq!(l.available_slots(), 0);
        l.reaped(1, 40).unwrap();
        assert_eq!(l.available_slots(), 1);
        assert!(l.reviewer_runnable());
    }

    #[test]
    fn status_omits_private_payloads_without_changing_scheduling_or_authority() {
        let mut l = ledger(2);
        let mut work = assignment("a", &["A"]);
        work.request.make_mut().unwrap().reachable_opaque_inventory = serde_json::json!("private payload".repeat(100_000));
        let mut request = Box::new(work.request.inline().unwrap().clone());
        request.verify_nodes = ["A".into()].into();
        request.substantiveness_verify_nodes = ["A".into(), "B".into()].into();
        request.verify_targets = ["target".into()].into();
        batch(&mut l, 100, vec![work]);
        launch(&mut l, 1, 20);
        let mut seed = crate::model::ProtocolState::default();
        seed.in_flight_request = Some(request.clone());
        l.exclusive_workers.insert(1, crate::dispatch_routing::ExclusiveWorker {
            state: Box::new(seed), canonical_sha256: "0".repeat(64),
        });
        let job = l.verifiers.enqueue(crate::dispatch_verifiers::VerifierIdentity {
            lane: crate::dispatch_verifiers::VerifierLane::Sound,
            panel_sha256: "panel".into(), input_hashes: BTreeMap::from([("A".into(), "source".into())]),
            compiler: CompilerCapability::None,
        }, true, false).unwrap();
        l.verifiers.jobs.get_mut(&job).unwrap().request = Some(request);
        let state = crate::model::ProtocolState { dispatch: Some(l), stage: crate::model::Stage::Reviewer,
            ..crate::model::ProtocolState::default() };
        let original = serde_json::to_vec(&state).unwrap();
        let view = crate::dispatch_coordinator::status(&state).unwrap();
        let ledger = state.dispatch.as_ref().unwrap();
        assert_eq!(view["available_slots"], ledger.available_slots());
        assert_eq!(view["tasks"]["1"]["process"], serde_json::to_value(&ledger.tasks[&1].process).unwrap());
        assert_eq!(view["tasks"]["1"]["deadline_ms"], ledger.tasks[&1].deadline_ms().unwrap());
        assert_eq!(view["tasks"]["1"]["assignment"]["input_hashes"]["C"], "content");
        assert_eq!(view["verifiers"]["jobs"][job.to_string()]["identity"],
            serde_json::to_value(&ledger.verifiers.jobs[&job].identity).unwrap());
        assert_eq!(view["verifiers"]["jobs"][job.to_string()]["nodes"], serde_json::json!(["A", "B"]));
        assert_eq!(view["verifiers"]["jobs"][job.to_string()]["targets"], serde_json::json!(["target"]));
        assert!(view.get("exclusive_workers").is_none());
        assert!(view["tasks"]["1"]["assignment"].get("request").is_none());
        assert!(view["verifiers"]["jobs"][job.to_string()].get("request").is_none());
        assert!(serde_json::to_vec(&view).unwrap().len() < 20_000);
        assert_eq!(serde_json::to_vec(&state).unwrap(), original);
    }

    fn add_exclusive_seed(l: &mut DispatchLedger, id: TaskId) {
        let state = crate::model::ProtocolState {
            in_flight_request: Some(Box::new(l.tasks[&id].assignment.request.inline().unwrap().clone())),
            ..crate::model::ProtocolState::default()
        };
        l.exclusive_workers.insert(id, crate::dispatch_routing::ExclusiveWorker {
            state: Box::new(state), canonical_sha256: "0".repeat(64),
        });
    }

    #[test]
    fn terminal_exclusive_seeds_wait_for_reaping_and_preserve_receipts_and_continuations() {
        let mut l = ledger(2);
        batch(&mut l, 100, vec![assignment("a", &["A"]), assignment("b", &["B"])]);
        add_exclusive_seed(&mut l, 1);
        add_exclusive_seed(&mut l, 2);
        launch(&mut l, 1, 20);
        l.candidate(1, "a".repeat(64), 30).unwrap();
        l.routing_pending = true;
        l.continuations.push_back(Box::new(crate::dispatch_routing::Continuation::CleanupWorker));
        let continuations = l.continuations.clone();
        let receipts = l.receipts.clone();
        l.apply(DispatchEvent::AttemptStopping { task_id: 1, outcome: AttemptOutcome::TimedOut }).unwrap();
        assert_eq!(l.exclusive_workers.len(), 2, "cleanup still owns the seed");
        let reap = DispatchEvent::ProcessesReaped { task_id: 1, now_ms: 40 };
        l.apply(reap.clone()).unwrap();
        assert!(!l.exclusive_workers.contains_key(&1));
        assert!(l.exclusive_workers.contains_key(&2), "reserved work retains its seed");
        assert_eq!(l.receipts, receipts);
        assert_eq!(l.continuations, continuations);
        let resolved = l.clone();
        l.apply(reap).unwrap();
        assert_eq!(l, resolved, "cleanup replay remains idempotent");
    }

    #[test]
    fn legacy_terminal_exclusive_seeds_reclaimed_only_after_successful_event() {
        let mut l = ledger(1);
        batch(&mut l, 100, vec![assignment("a", &["A"])]);
        add_exclusive_seed(&mut l, 1);
        l.stop(1, AttemptOutcome::TimedOut).unwrap();
        l.reaped(1, 40).unwrap(); // Simulate a checkpoint from before reclamation.
        let before = l.clone();
        assert!(l.apply(DispatchEvent::Drain { reason: String::new() }).is_err());
        assert_eq!(l, before, "rejected events cannot alter retained history");
        l.apply(DispatchEvent::Drain { reason: "test drain".into() }).unwrap();
        assert!(l.exclusive_workers.is_empty());
        assert_eq!(l.tasks, before.tasks, "complete historical contracts remain available");
        assert_eq!(l.receipts, before.receipts);
    }

    #[test]
    fn exclusive_seed_survives_pending_retry_then_is_released_on_refusal() {
        let mut l = ledger(1);
        batch(&mut l, 100, vec![assignment("a", &["A"])]);
        add_exclusive_seed(&mut l, 1);
        l.tasks.get_mut(&1).unwrap().pending_retry = Some(RetryContext::default());
        l.stop(1, AttemptOutcome::Rejected { reason: "retryable".into() }).unwrap();
        l.apply(DispatchEvent::ProcessesReaped { task_id: 1, now_ms: 40 }).unwrap();
        assert!(l.exclusive_workers.contains_key(&1));
        let before = l.clone();
        assert!(l.apply(DispatchEvent::RetryPrepared { predecessor: 1, assignment: None,
            refusal: Some(String::new()), graph: graph(), now_ms: 50 }).is_err());
        assert_eq!(l, before);
        l.apply(DispatchEvent::RetryPrepared { predecessor: 1, assignment: None,
            refusal: Some("inputs changed".into()), graph: graph(), now_ms: 50 }).unwrap();
        assert!(l.exclusive_workers.is_empty());
        assert_eq!(l.tasks[&1].retry_refusal.as_deref(), Some("inputs changed"));
    }

    #[test]
    fn replay_of_mixed_batch_preserves_receipt_and_never_launches_again() {
        let mut l = ledger(4);
        let a = vec![assignment("a", &["A"]), assignment("bad", &["C"])];
        let first = batch(&mut l, 100, a.clone());
        let wire = serde_json::to_string(&l).unwrap();
        let mut recovered: DispatchLedger = serde_json::from_str(&wire).unwrap();
        assert_eq!(recovered.dispatch(100, "response-100".into(), a.clone(), &graph(), 999).unwrap(), first);
        assert_eq!(recovered.tasks.len(), 1);
        assert!(recovered.dispatch(100, "different".into(), a, &graph(), 999).is_err());
        recovered.validate().unwrap();
    }

    #[test]
    fn completion_during_review_is_not_lost_and_empty_review_does_not_spin() {
        let mut l = ledger(2);
        batch(&mut l, 100, vec![assignment("a", &["A"]), assignment("b", &["B"])]);
        launch(&mut l, 1, 20); launch(&mut l, 2, 20);
        l.stop(1, AttemptOutcome::Stuck { reason: "needs a lemma".into() }).unwrap();
        l.reaped(1, 30).unwrap();
        l.start_review(101).unwrap();
        l.stop(2, AttemptOutcome::TimedOut).unwrap();
        l.reaped(2, 40).unwrap();
        l.dispatch(101, "second".into(), vec![], &graph(), 50).unwrap();
        assert!(l.reviewer_runnable());
        batch(&mut l, 102, vec![]);
        assert!(!l.reviewer_runnable());
    }

    #[test]
    fn deadline_starts_at_launch_survives_adoption_and_ends_at_checking() {
        let mut l = ledger(1);
        let mut a = assignment("a", &["A"]); a.timeout_seconds = Some(60);
        batch(&mut l, 100, vec![a]);
        assert!(l.expired(90_000).is_empty());
        launch(&mut l, 1, 100_000); launch(&mut l, 1, 101_000);
        assert_eq!(l.tasks[&1].deadline_ms(), Some(160_000));
        assert_eq!(l.expired(160_000), vec![1]);
        assert!(l.candidate(1, "a".repeat(64), 160_000).is_err());
        l.candidate(1, "a".repeat(64), 159_999).unwrap();
        assert!(l.expired(160_000).is_empty());
        assert!(l.require_live_candidate(1, &"a".repeat(64), 260_000).is_ok());
        // Explicit historical outcomes remain replayable; only automatic
        // worker expiry is disabled once the candidate has been sealed.
        l.stop(1, AttemptOutcome::TimedOut).unwrap();
        assert!(l.candidate(1, "a".repeat(64), 160_001).is_err());
        assert!(l.stop(1, AttemptOutcome::Accepted { event_id: 60 }).is_err());
        assert_eq!(l.available_slots(), 0);
    }

    #[test]
    fn profile_selection_is_explicit_and_luna_can_be_shortened() {
        let cfg = DispatchConfig::default();
        assert_eq!(cfg.bind("sol-xhigh", None).unwrap().timeout_seconds, 14_400);
        assert_eq!(cfg.bind("luna-high", Some(600)).unwrap().model, "gpt-6-luna");
        assert!(cfg.bind("luna-high", Some(3601)).is_err());
        assert!(cfg.bind("luna-high", Some(0)).is_err());
        assert!(cfg.bind("missing", None).is_err());
        let mut cfg = cfg;
        cfg.profiles.get_mut("sol-xhigh").unwrap().max_timeout_seconds = 28_800;
        assert!(cfg.bind("sol-xhigh", Some(21_600)).is_ok());
    }

    #[test]
    fn weak_profile_is_closure_only_and_cannot_acquire_restructure_scope() {
        let mut l = ledger(4);
        let mut ordinary = assignment("ordinary", &["A"]);
        ordinary.profile = "luna-high".into();
        let mut closure = assignment("closure", &["A"]);
        closure.profile = "luna-high".into();
        closure.task_kind = WorkerTaskKind::LeanClosure;
        let mut wide = assignment("wide", &["B", "C"]);
        wide.profile = "luna-high".into();
        wide.task_kind = WorkerTaskKind::LeanClosure;
        let receipt = batch(&mut l, 100, vec![ordinary, closure, wide]);
        assert!(receipt.decisions[0].rejection.as_ref().unwrap().contains("not permitted"));
        assert_eq!(receipt.decisions[1].task_id, Some(1));
        assert!(receipt.decisions[2].rejection.as_ref().unwrap().contains("one existing node"));
        assert_eq!(l.available_slots(), 3);
        l.validate().unwrap();
        let mut restored: DispatchLedger = serde_json::from_slice(&serde_json::to_vec(&l).unwrap()).unwrap();
        restored.validate().unwrap();
        restored.tasks.get_mut(&1).unwrap().assignment.scope.helper_prefixes.insert("new_".into());
        assert!(restored.validate().unwrap_err().contains("without helpers"));
    }

    #[test]
    fn dependencies_missing_from_capture_fail_closed() {
        let mut l = ledger(2);
        l.start_review(100).unwrap();
        let mut g = graph(); g.edges.remove(&NodeId::from("C"));
        let r = l.dispatch(100, "response".into(), vec![assignment("a", &["A"])], &g, 0).unwrap();
        assert!(r.decisions[0].rejection.as_ref().unwrap().contains("incomplete for C"));
        assert_eq!(l.available_slots(), 2);
    }

    #[test]
    fn closure_dependency_overlap_agrees_at_preflight_admission_and_candidate() {
        use WorkerTaskKind::{LeanClosure, Ordinary};
        // D transitively reads C through A. Permission belongs to writer C.
        for (dependent_kind, dependency_kind, allowed) in [
            (Ordinary, LeanClosure, true), (LeanClosure, LeanClosure, true),
            (LeanClosure, Ordinary, false), (Ordinary, Ordinary, false),
        ] {
            for reverse in [false, true] {
                let mut dependent = assignment("dependent", &["D"]);
                dependent.task_kind = dependent_kind;
                let mut dependency = assignment("dependency", &["C"]);
                dependency.task_kind = dependency_kind;
                let mut assignments = vec![dependent, dependency];
                if reverse { assignments.reverse(); }
                let frozen = FrozenScopeCheck {
                    phase: Phase::ProofFormalization, active_node: None,
                    current_present_nodes: nodes(&["A", "B", "C", "D"]), current_deps: graph().edges,
                    tasks: vec![FrozenTaskScope { id: 17, status: TaskStatus::Running,
                        scope: assignments[0].scope.clone(), task_kind: assignments[0].task_kind }],
                    assignments: vec![FrozenAssignmentScope { evidence_paths: BTreeSet::new(), id: "second".into(), task_kind: assignments[1].task_kind,
                        review: ReviewResponse { next_active: assignments[1].scope.nodes.first().cloned(),
                            next_mode: TaskMode::Local, ..Default::default() } }],
                };
                assert_eq!(check_frozen_scopes(&frozen).unwrap().is_empty(), allowed);
                let mut l = ledger(2);
                let receipt = batch(&mut l, 100, assignments.clone());
                assert_eq!(receipt.decisions[1].task_id.is_some(), allowed);
                // An occupied pair must undergo the identical rule at acceptance,
                // including graph changes after their original reservations.
                if !allowed {
                    let independent = DependencyGraph { edges: graph().edges.into_keys()
                        .map(|node| (node, BTreeSet::new())).collect() };
                    l.reserve(100, assignments[1].clone(), &independent, 10).unwrap();
                }
                launch(&mut l, 1, 20); l.candidate(1, "a".repeat(64), 30).unwrap();
                assert_eq!(l.check_candidate_inputs(1, &assignments[0].input_hashes,
                    &BTreeMap::new(), &assignments[0].scope, &graph()).is_ok(), allowed);
            }
        }
    }

    #[test]
    fn scope_diagnostic_explains_consumer_of_ordinary_writer_in_both_orders() {
        let graph = DependencyGraph { edges: BTreeMap::from([
            ("Consumer".into(), nodes(&["Middle"])),
            ("Middle".into(), nodes(&["Writer"])),
            ("Writer".into(), BTreeSet::new()),
        ]) };
        let consumer = EditScope { nodes: nodes(&["Consumer"]), ..Default::default() };
        let writer = EditScope { nodes: nodes(&["Writer"]), ..Default::default() };
        let error = validate_scope_conflicts(&consumer, WorkerTaskKind::LeanClosure, &graph,
            [("task 144".into(), &writer, WorkerTaskKind::Ordinary)]).unwrap_err();
        assert!(error.contains("proposed task reads a dependency written by task 144 (ordinary)"));
        assert!(error.contains("Consumer -> Middle -> Writer"));
        assert!(error.contains("Only a lean_closure dependency writer permits this overlap"));
        let reverse = validate_scope_conflicts(&writer, WorkerTaskKind::Ordinary, &graph,
            [("assignment consumer".into(), &consumer, WorkerTaskKind::LeanClosure)]).unwrap_err();
        assert!(reverse.contains("assignment consumer reads a dependency written by proposed task (ordinary)"));
        assert!(reverse.contains("Consumer -> Middle -> Writer"));
        assert!(validate_scope_conflicts(&consumer, WorkerTaskKind::Ordinary, &graph,
            [("task 144".into(), &writer, WorkerTaskKind::LeanClosure)]).is_ok());
        let mut exclusive = writer.clone(); exclusive.exclusive = true;
        assert_eq!(validate_scope_conflicts(&consumer, WorkerTaskKind::LeanClosure, &graph,
            [("task 144".into(), &exclusive, WorkerTaskKind::Ordinary)]).unwrap_err(),
            "authorized edit scope conflicts with task 144");
    }

    #[test]
    fn scope_diagnostic_path_is_shortest_deterministic_and_cycle_safe() {
        let graph = DependencyGraph { edges: BTreeMap::from([
            ("Reader".into(), nodes(&["Long", "Short"])),
            ("Long".into(), nodes(&["More"])),
            ("More".into(), nodes(&["Reader", "Writer"])),
            ("Short".into(), nodes(&["Reader", "Writer"])),
            ("Writer".into(), BTreeSet::new()),
        ]) };
        assert_eq!(graph.dependency_path(&nodes(&["Reader"]), &nodes(&["Writer"])),
            Some(vec!["Reader".into(), "Short".into(), "Writer".into()]));
        assert_eq!(graph.dependency_path(&nodes(&["Long", "Short"]), &nodes(&["Writer"])),
            Some(vec!["Short".into(), "Writer".into()]));
        assert!(graph.dependency_path(&nodes(&["Reader"]), &nodes(&["Absent"])).is_none());
    }

    #[test]
    fn scope_diagnostic_bounds_displayed_path_without_changing_conflict() {
        let mut edges = BTreeMap::new();
        for i in 0..20 {
            edges.insert(NodeId::from(format!("N{i}")), if i == 19 { BTreeSet::new() }
                else { BTreeSet::from([NodeId::from(format!("N{}", i + 1))]) });
        }
        let graph = DependencyGraph { edges };
        let reader = EditScope { nodes: nodes(&["N0"]), ..Default::default() };
        let writer = EditScope { nodes: nodes(&["N19"]), ..Default::default() };
        let error = validate_scope_conflicts(&reader, WorkerTaskKind::LeanClosure, &graph,
            [("task 144".into(), &writer, WorkerTaskKind::Ordinary)]).unwrap_err();
        assert!(error.contains("N0 -> N1") && error.contains("N18 -> N19"));
        assert!(error.contains("8 nodes omitted") && !error.contains("N10"));
    }

    #[test]
    fn closure_overlap_retains_same_node_exclusive_and_shared_conflicts() {
        let a = assignment("a", &["A"]).scope;
        let mut b = assignment("b", &["C"]).scope;
        let conflicts = |a: &EditScope, b: &EditScope| graph().conflicts_for_tasks(
            a, WorkerTaskKind::LeanClosure, b, WorkerTaskKind::LeanClosure).unwrap();
        assert!(conflicts(&a, &a));
        assert!(!conflicts(&a, &b));
        b.exclusive = true; assert!(conflicts(&a, &b)); b.exclusive = false;
        let mut a = a;
        a.shared_writes.insert("shared".into()); b.shared_writes.insert("shared".into());
        assert!(conflicts(&a, &b));
        a.shared_writes.clear(); b.shared_writes.clear();
        a.helper_prefixes.insert("helper_".into()); b.helper_prefixes.insert("helper_sub_".into());
        assert!(conflicts(&a, &b));
    }

    #[test]
    fn readonly_proof_prefix_rebase_keeps_other_inputs_and_legacy_exact() {
        let mut a = assignment("a", &["A"]);
        a.input_hashes = BTreeMap::from([
            ("Tablet/A.lean".into(), "own".into()), ("Tablet/C.lean".into(), "old-proof".into()),
            ("Tablet/C.tex".into(), "statement".into()), ("Tablet/Definition.lean".into(), "definition".into()),
            ("lakefile.toml".into(), "configuration".into()),
        ]);
        a.proof_body_rebase_inputs.insert("Tablet/C.lean".into(), "prefix".into());
        let prefixes = a.proof_body_rebase_inputs.clone();
        let mut l = ledger(1); batch(&mut l, 100, vec![a.clone()]);
        launch(&mut l, 1, 20); l.candidate(1, "a".repeat(64), 30).unwrap();
        let mut inputs = a.input_hashes.clone(); inputs.insert("Tablet/C.lean".into(), "new-proof".into());
        l.check_candidate_inputs(1, &inputs, &prefixes, &a.scope, &graph()).unwrap();
        for path in ["Tablet/A.lean", "Tablet/C.tex", "Tablet/Definition.lean", "lakefile.toml"] {
            let mut changed = inputs.clone(); changed.insert(path.into(), "changed".into());
            assert_eq!(l.check_candidate_inputs(1, &changed, &prefixes, &a.scope, &graph()).unwrap_err(),
                format!("stale task input: {path}"));
        }
        assert!(l.check_candidate_inputs(1, &inputs, &BTreeMap::new(), &a.scope, &graph()).is_err());
        let changed_prefix = BTreeMap::from([("Tablet/C.lean".into(), "changed-prefix".into())]);
        assert!(l.check_candidate_inputs(1, &inputs, &changed_prefix, &a.scope, &graph()).is_err());
        l.tasks.get_mut(&1).unwrap().assignment.proof_body_rebase_inputs.clear();
        assert!(l.check_candidate_inputs(1, &inputs, &prefixes, &a.scope, &graph()).is_err());
        l.check_candidate_inputs(1, &a.input_hashes, &BTreeMap::new(), &a.scope, &graph()).unwrap();
    }

    #[test]
    fn stale_relevant_input_is_rejected_but_unrelated_completion_is_not() {
        let mut l = ledger(2);
        batch(&mut l, 100, vec![assignment("a", &["A"]), assignment("b", &["B"])]);
        launch(&mut l, 1, 20); launch(&mut l, 2, 20);
        l.candidate(1, "a".repeat(64), 30).unwrap();
        l.stop(2, AttemptOutcome::Rejected { reason: "invalid proof".into() }).unwrap();
        l.reaped(2, 40).unwrap();
        let a = l.tasks[&1].assignment.clone();
        l.check_candidate_inputs(1, &a.input_hashes, &BTreeMap::new(), &a.scope, &graph()).unwrap();
        assert!(l.check_candidate_inputs(1, &BTreeMap::from([("C".into(), "new".into())]), &BTreeMap::new(), &a.scope, &graph()).unwrap_err().contains("stale task input: C"));
    }

    #[test]
    fn fresh_staged_inputs_only_replace_readonly_launch_sources() {
        let mut l = ledger(2);
        let mut a = assignment("a", &["A"]);
        a.input_hashes = BTreeMap::from([
            ("Tablet/A.lean".into(), "own".into()), ("Tablet/C.lean".into(), "old-imports".into()),
            ("trellis.config.json".into(), "config".into()), ("Tablet/Preamble.lean".into(), "preamble".into())]);
        batch(&mut l, 100, vec![a]); launch(&mut l, 1, 20); l.candidate(1, "a".repeat(64), 30).unwrap();
        let a = l.tasks[&1].assignment.clone();
        let mut inputs = a.input_hashes.clone();
        inputs.insert("Tablet/C.lean".into(), "three new imports and changed secondary definitions".into());
        assert!(l.check_candidate_inputs(1, &inputs, &BTreeMap::new(), &a.scope, &graph()).is_err());
        l.check_candidate_inputs_with_fresh_readonly(1, &inputs, &BTreeMap::new(), &a.scope, &graph(), true).unwrap();
        for path in ["Tablet/A.lean", "trellis.config.json", "Tablet/Preamble.lean"] {
            let mut changed = inputs.clone(); changed.insert(path.into(), "different".into());
            assert!(l.check_candidate_inputs_with_fresh_readonly(1, &changed, &BTreeMap::new(), &a.scope, &graph(), true).is_err(), "{path}");
        }
        inputs.remove("Tablet/C.lean");
        assert!(l.check_candidate_inputs_with_fresh_readonly(1, &inputs, &BTreeMap::new(), &a.scope, &graph(), true).is_err());
        let mut unowned = a.scope.clone(); unowned.nodes.insert("Unknown".into());
        assert!(l.check_candidate_inputs_with_fresh_readonly(1, &a.input_hashes, &BTreeMap::new(), &unowned, &graph(), true).is_err());
    }

    #[test]
    fn helper_dependency_cannot_join_active_scopes_at_integration() {
        let mut l = ledger(2);
        let mut a = assignment("a", &["A"]); a.scope.helper_prefixes.insert("Task1_".into());
        batch(&mut l, 100, vec![a, assignment("b", &["B"])]);
        launch(&mut l, 1, 20); l.candidate(1, "a".repeat(64), 30).unwrap();
        let a = l.tasks[&1].assignment.clone();
        let mut scope = a.scope.clone(); scope.nodes.insert("Task1_Helper".into());
        let mut g = graph(); g.edges.insert("Task1_Helper".into(), nodes(&["B"]));
        assert!(l.check_candidate_inputs(1, &a.input_hashes, &BTreeMap::new(), &scope, &g).unwrap_err().contains("cross active"));
        g.edges.insert("Task1_Helper".into(), nodes(&["C"]));
        l.check_candidate_inputs(1, &a.input_hashes, &BTreeMap::new(), &scope, &g).unwrap();
        scope.nodes.insert("Unowned".into());
        assert!(l.check_candidate_inputs(1, &a.input_hashes, &BTreeMap::new(), &scope, &g).unwrap_err().contains("unreserved node"));
    }

    #[test]
    fn closure_imports_recheck_new_dependencies_against_active_writers() {
        let mut l = ledger(2);
        let mut a = assignment("a", &["A"]);
        a.task_kind = WorkerTaskKind::LeanClosure;
        batch(&mut l, 100, vec![a, assignment("b", &["B"])]);
        launch(&mut l, 1, 20); l.candidate(1, "a".repeat(64), 30).unwrap();
        let a = l.tasks[&1].assignment.clone();
        // Importing stable support is accepted, without claiming write access.
        let mut g = graph(); g.edges.get_mut("A").unwrap().insert("C".into());
        l.check_candidate_inputs(1, &a.input_hashes, &BTreeMap::new(), &a.scope, &g).unwrap();
        // A new import of a concurrently mutable statement is a real conflict.
        g.edges.get_mut("A").unwrap().insert("B".into());
        assert!(l.check_candidate_inputs(1, &a.input_hashes, &BTreeMap::new(), &a.scope, &g)
            .unwrap_err().contains("cross active"));
        // An upstream closure task remains compatible with downstream work.
        l.tasks.get_mut(&2).unwrap().assignment.task_kind = WorkerTaskKind::LeanClosure;
        l.check_candidate_inputs(1, &a.input_hashes, &BTreeMap::new(), &a.scope, &g).unwrap();
    }

    #[test]
    fn exclusive_operations_wait_for_resolution_and_reaping() {
        let mut l = ledger(2);
        batch(&mut l, 100, vec![assignment("a", &["A"])]);
        l.drain("preamble repair".into()).unwrap();
        let r = batch(&mut l, 101, vec![assignment("b", &["B"])]);
        assert!(r.decisions[0].rejection.as_ref().unwrap().contains("draining"));
        assert!(!l.drained());
        l.stop(1, AttemptOutcome::Cancelled { reason: "never launched".into() }).unwrap();
        assert!(!l.drained());
        l.reaped(1, 30).unwrap();
        assert!(l.drained());
    }

    #[test]
    fn ledger_round_trips_inside_tagged_runtime_requests() {
        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(tag = "action", rename_all = "snake_case")]
        enum Request { Observe { ledger: DispatchLedger } }
        let mut l = ledger(4);
        batch(&mut l, 1, vec![assignment("a", &["A"]), assignment("b", &["B"])]);
        let request = Request::Observe { ledger: l };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        assert_eq!(serde_json::from_value::<Request>(serde_json::to_value(&request).unwrap()).unwrap(), request);
    }

    #[test]
    fn current_protocol_hints_precede_dispatch_history() {
        let mut state = crate::model::ProtocolState::default();
        let mut history = ledger(2);
        let mut old = assignment("old", &["A"]);
        old.request = Arc::new(crate::model::WrapperRequest::default()).into();
        batch(&mut history, 100, vec![old]);
        state.dispatch = Some(history);
        let wire = serde_json::to_string(&state).unwrap();
        let dispatch_offset = wire.find("\"dispatch\":").unwrap();
        for field in ["phase", "stage", "cycle", "gate_kind"] {
            assert!(wire.find(&format!("\"{field}\":")).unwrap() < dispatch_offset);
        }
        assert_eq!(serde_json::from_str::<crate::model::ProtocolState>(&wire).unwrap(), state);
    }

    #[test]
    fn absent_ledger_preserves_historical_state_wire_shape() {
        let state = crate::model::ProtocolState::default();
        let value = serde_json::to_value(&state).unwrap();
        assert!(value.get("dispatch").is_none());
        let restored: crate::model::ProtocolState = serde_json::from_value(value).unwrap();
        assert!(restored.dispatch.is_none());
    }
}

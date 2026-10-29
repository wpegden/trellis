//! Cone-scoped advisory audits use the ordinary verifier lifetime and immutable
//! evidence. Global decisions retain the ordinary StuckMathAudit transition.
use crate::coarse_goals::{GoalStatus, relevant_blockers};
use crate::dispatch::{CompilerCapability, EditScope};
use crate::dispatch_verifiers::{VerifierIdentity, VerifierLane, VerifierStatus};
use crate::model::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditIntent {
    #[serde(default)] pub retry: u32,
    #[serde(default)] pub feedback: String,
    pub goals: BTreeSet<NodeId>,
    pub reason: String,
    pub reviewer_requested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryReference { pub sha256: String, pub bytes: u64 }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditContext {
    pub retry: u32,
    pub feedback: String,
    pub memory_entry_ids: BTreeSet<String>,
    pub goals: BTreeSet<NodeId>,
    pub nodes: BTreeSet<NodeId>,
    pub reason: String,
    pub previous: BTreeMap<NodeId, HistoryReference>,
    pub completed_attempts: BTreeMap<NodeId, u64>,
    #[serde(default)] pub recent_attempts: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReport {
    pub version: u32,
    pub context: AuditContext,
    pub evidence: Option<VerifierIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_from: Option<String>,
    pub plan: AuditPlan,
}

/// Full transitive semantic AND Lean support. Shared premises remain shared:
/// the lock covers writes to this evidence, not another cone's distinct root.
pub fn evidence_nodes(state: &ProtocolState, goals: &BTreeSet<NodeId>) -> Result<BTreeSet<NodeId>, String> {
    let mut nodes = BTreeSet::new();
    let mut pending: Vec<_> = goals.iter().cloned().collect();
    while let Some(node) = pending.pop() {
        if !state.live.present_nodes.contains(&node) { return Err(format!("audit input {node} is no longer present")); }
        if !nodes.insert(node.clone()) { continue; }
        pending.extend(state.deps.get(&node).into_iter().flatten().cloned());
        // When the compiler has not attested an import surface, retain the
        // complete source view. Never infer a small read set from NL deps alone.
        let Some(record) = state.local_closure_records.get(&node) else {
            return Ok(state.live.present_nodes.clone());
        };
        let Some(imports) = record.node_certificate_evidence.as_ref()
            .filter(|e| !record.active_decl_hash.is_empty() && e.source_sha256 == record.active_decl_hash)
            .and_then(|e| e.direct_imports.as_ref()) else {
                return Ok(state.live.present_nodes.clone());
            };
        pending.extend(record.boundary_theorems.keys().cloned());
        pending.extend(record.strict_theorem_deps.keys().cloned());
        pending.extend(record.strict_definition_deps.keys().cloned());
        for name in imports.iter().filter_map(|i| i.strip_prefix("Tablet.")) {
            if matches!(name, "Preamble" | "Axioms") && !state.live.present_nodes.contains(&NodeId::from(name)) { continue; }
            pending.push(NodeId::from(name));
        }
    }
    Ok(nodes)
}

pub fn input_in_scope(path: &str, context: &AuditContext) -> bool {
    if matches!(path, "Tablet/INDEX.md" | "Tablet/README.md") { return false; }
    if let Some(file) = path.strip_prefix("Tablet/") {
        // Node source is the only per-node surface accepted here. Shared Tablet
        // files stay bound. Missing .tex files are represented explicitly below.
        if let Some(node) = file.strip_suffix(".lean").or_else(|| file.strip_suffix(".tex")) {
            return context.nodes.contains(&NodeId::from(node)) || matches!(node, "Preamble" | "Axioms");
        }
    }
    if let Some(path) = path.strip_prefix("process-memory/") {
        return path.split('/').next().is_some_and(|scope| scope == "global" || context.goals.contains(&NodeId::from(scope)));
    }
    // Operational artifacts and mutable global renderings are not audit
    // evidence. Reports/history are supplied through verified, scoped links.
    !path.starts_with(".trellis/") && !path.starts_with(".trellis-history/")
}

pub fn inputs(all: &BTreeMap<String, String>, context: &AuditContext) -> BTreeMap<String, String> {
    let mut result: BTreeMap<_, _> = all.iter().filter(|(p, _)| input_in_scope(p, context))
        .map(|(p, h)| (p.clone(), h.clone())).collect();
    for node in &context.nodes {
        for ext in ["lean", "tex"] {
            result.entry(format!("Tablet/{node}.{ext}")).or_insert_with(|| "absent".into());
        }
    }
    result
}

/// Only declared local evidence enters the prompt. The private host state is
/// still complete, so no canonical verification ledger is erased or forged.
pub fn request(state: &ProtocolState, id: u32, context: &AuditContext) -> WrapperRequest {
    let mut r = WrapperRequest { id, cycle: state.cycle, kind: RequestKind::StuckMathAudit,
        runtime_support_required: RequestKind::StuckMathAudit.requires_runtime_support(),
        phase: state.phase, scoped_audit: Some(context.clone()),
        active_coarse_node: context.goals.iter().next().cloned(),
        current_present_nodes: context.nodes.clone(), coarse_dag_nodes: context.goals.clone(),
        current_proof_nodes: state.proof_nodes.intersection(&context.nodes).cloned().collect(),
        blockers: relevant_blockers(state, &context.nodes),
        stuck_math_audit: StuckMathAuditState { active: true, trigger: context.reason.clone(), ..Default::default() },
        stuck_math_audit_binding: Default::default(),
        ..Default::default() };
    r.current_deps = context.nodes.iter().map(|n| (n.clone(), state.deps.get(n).cloned().unwrap_or_default())).collect();
    r.current_node_kinds = state.node_kinds.iter().filter(|(n, _)| context.nodes.contains(*n)).map(|(n,k)| (n.clone(), *k)).collect();
    r.current_target_claims = state.target_claims.iter().filter(|(n,_)| context.nodes.contains(*n)).map(|(n,v)| (n.clone(),v.clone())).collect();
    r.current_challenge_claims = state.challenge_claims.iter().filter(|(n,_)| context.nodes.contains(*n)).map(|(n,v)| (n.clone(),v.clone())).collect();
    r.node_role = state.node_role.iter().filter(|(n,_)| context.nodes.contains(*n)).map(|(n,v)| (n.clone(),*v)).collect();
    r.configured_targets = state.configured_targets.clone();
    r.configured_challenge_targets = state.configured_challenge_targets.clone();
    r.configured_reference_papers = state.configured_reference_papers.clone();
    r.is_pv = state.is_pv();
    r.node_reference_grounds = state.node_reference_grounds.iter().filter(|(n,_)| context.nodes.contains(*n)).map(|(n,v)| (n.clone(),v.clone())).collect();
    r.authored_goals_decidable = state.authored_goals_decidable;
    r.model_preservation_enabled = state.model_preservation_enabled;
    r.pv_live_polarity = state.pv_live_polarity.clone();
    r.pv_goal_mode = state.pv_goal_mode;
    r.trust_base_required_v1 = state.trust_base.required();
    // Paper authority and the reviewed source boundary remain global evidence;
    // this read-only report still receives only its scoped node inventory.
    if state.paper_implementation.is_some() {
        r.paper_implementation = state.paper_implementation.clone();
        r.paper_implementation_review = crate::paper_implementation::review_evidence(state);
        r.modeling_route_decisions = state.modeling_route_decisions.clone();
    }
    r.pending_memory_challenges = state.pending_memory_challenges.iter().filter(|c|
        context.memory_entry_ids.contains(&c.entry_id)).cloned().collect();
    r.latest_stuck_math_audit_rejection_reason = context.feedback.clone();
    // This report author has no statement, reset, assumption, or polarity
    // authority. Those require the existing run-wide audit and human gates.
    crate::request_contracts::populate_request_prompt_contracts(&mut r, None);
    r
}

pub fn identity(request: &WrapperRequest, all: &BTreeMap<String, String>) -> Result<VerifierIdentity, String> {
    let scope = request.scoped_audit.as_ref().ok_or("missing scoped audit contract")?;
    let mut value = serde_json::to_value(request).map_err(|e| e.to_string())?;
    value.as_object_mut().unwrap().remove("id");
    value.as_object_mut().unwrap().remove("cycle");
    Ok(VerifierIdentity { lane: VerifierLane::Audit,
        panel_sha256: crate::trusted_artifact_rebind::digest(&value),
        input_hashes: inputs(all, scope), compiler: CompilerCapability::Managed })
}

pub fn conflicts(state: &ProtocolState, scope: &EditScope) -> Result<bool, String> {
    let Some(goals) = &state.coarse_goals else { return Ok(false) };
    if state.phase != Phase::ProofFormalization { return Ok(false); }
    let mut scopes: Vec<_> = goals.pending_audits.iter().map(|a| a.goals.clone()).collect();
    if let Some(ledger) = &state.dispatch {
        scopes.extend(ledger.verifiers.jobs.values().filter(|j| !matches!(j.status, VerifierStatus::Resolved { .. }))
            .filter_map(|j| j.request.as_ref()?.scoped_audit.as_ref().map(|a| a.goals.clone())));
    }
    for goals in scopes {
        if scope.exclusive || !scope.shared_writes.is_empty() || !scope.nodes.is_disjoint(&evidence_nodes(state, &goals)?) { return Ok(true); }
    }
    Ok(false)
}

pub fn submit(state: &mut ProtocolState, intent: AuditIntent) -> Result<(), String> {
    if let Some(existing) = state.dispatch.as_ref().into_iter().flat_map(|d| d.verifiers.jobs.values())
        .filter(|j| !matches!(j.status, VerifierStatus::Resolved { .. }))
        .filter_map(|j| j.request.as_ref()?.scoped_audit.as_ref())
        .find(|a| !a.goals.is_disjoint(&intent.goals)) {
        return if existing.goals == intent.goals && existing.reason == intent.reason { Ok(()) }
            else { Err("these goals already have a queued or running audit; let it finish before requesting another question".into()) };
    }
    let goals = state.coarse_goals.as_mut().ok_or("scoped audits require multiple coarse goals")?;
    if state.phase != Phase::ProofFormalization || intent.goals.is_empty()
        || intent.goals.iter().any(|n| goals.goals.get(n).is_none_or(|g| g.status != GoalStatus::Active)) {
        return Err("audit coarse_nodes must name active proof-formalization goals".into());
    }
    if intent.reason.trim().is_empty() { return Err("audit reason must be nonempty".into()); }
    if let Some(existing) = goals.pending_audits.iter_mut().find(|a| a.goals == intent.goals && a.reason == intent.reason) {
        existing.reviewer_requested |= intent.reviewer_requested;
    } else {
        // At most one pending question per scope. Never silently replace a
        // different question or create an unbounded audit-request backlog.
        if goals.pending_audits.iter().any(|a| !a.goals.is_disjoint(&intent.goals)) {
            return Err("these goals already have a pending audit; let it finish before requesting another question".into());
        }
        goals.pending_audits.push(intent);
    }
    Ok(())
}

pub fn enqueue_ready(state: &mut ProtocolState, all: &BTreeMap<String, String>) -> Result<(), String> {
    if state.coarse_goals.is_none() || state.phase != Phase::ProofFormalization || state.audit_lane_in_flight()
        || state.dispatch.as_ref().is_none_or(|d| d.draining.is_some()) { return Ok(()) }
    let automatic: Vec<_> = state.coarse_goals.as_ref().unwrap().goals.iter()
        .filter(|(_,g)| g.status == GoalStatus::Active
            && g.attempts_since_progress >= u64::from(stuck_math_audit_shallow_coarse_no_progress_threshold())
            && g.completed_attempts.saturating_sub(g.last_audit_attempt) >= u64::from(stuck_math_audit_reaudit_interval_cycles()))
        .map(|(n,_)| n.clone()).collect();
    for node in automatic {
        let pending = state.coarse_goals.as_ref().unwrap().pending_audits.iter().any(|a| a.goals.contains(&node));
        let running = state.dispatch.as_ref().unwrap().verifiers.jobs.values().any(|j|
            !matches!(j.status, VerifierStatus::Resolved { .. }) && j.request.as_ref()
                .and_then(|r| r.scoped_audit.as_ref()).is_some_and(|s| s.goals.contains(&node)));
        if !pending && !running {
            submit(state, AuditIntent { goals: [node].into(), reason: "No progress in the recent completed attempts; reassess this goal and its prior remedies".into(), reviewer_requested: false, retry: 0, feedback: String::new() })?;
        }
    }
    let goals = state.coarse_goals.as_ref().unwrap();
    let intents = goals.pending_audits.clone();
    for intent in intents {
        let mut nodes = evidence_nodes(state, &intent.goals)?;
        if nodes.iter().any(|n| state.local_closure_records.get(n).is_none_or(|r|
            all.get(&format!("Tablet/{n}.lean")) != Some(&r.active_decl_hash))) {
            nodes = state.live.present_nodes.clone();
        }
        let ledger = state.dispatch.as_ref().ok_or("scoped audits require parallel dispatch")?;
        if ledger.tasks.values().any(|t| (t.status.occupies_slot() || t.pending_retry.is_some())
            && (t.assignment.scope.exclusive || !t.assignment.scope.shared_writes.is_empty()
                || !t.assignment.scope.nodes.is_disjoint(&nodes))) { continue; }
        if ledger.verifiers.jobs.values().any(|j| !matches!(j.status, VerifierStatus::Resolved { .. })
            && j.request.as_ref().and_then(|r| r.scoped_audit.as_ref()).is_some_and(|a| !a.goals.is_disjoint(&intent.goals))) { continue; }
        let registry = state.coarse_goals.as_ref().unwrap();
        let context = AuditContext { retry: intent.retry, feedback: intent.feedback.clone(),
            recent_attempts: ledger.tasks.values().rev()
                .filter(|t| !t.status.occupies_slot() && !t.assignment.scope.nodes.is_disjoint(&nodes))
                .take(12).map(|t| serde_json::json!({"task_id": t.id, "status": t.status,
                    "nodes": t.assignment.scope.nodes,
                    "summary": t.report.as_ref().map(|r| r.summary.chars().take(2000).collect::<String>()),
                    "comments": t.report.as_ref().map(|r| r.comments.chars().take(4000).collect::<String>()),
                    "rejection_reasons": t.report.as_ref().map(|r| r.rejection_reasons.iter().take(8)
                        .map(|s| s.chars().take(1000).collect::<String>()).collect::<Vec<_>>())})).collect(),
            memory_entry_ids: all.keys().filter_map(|path| {
                let parts: Vec<_> = path.split('/').collect();
                (parts.len() == 3 && parts[0] == "process-memory" && intent.goals.contains(&NodeId::from(parts[1])))
                    .then(|| parts[2].strip_suffix(".md").map(str::to_string)).flatten()
            }).collect(), nodes, goals: intent.goals.clone(), reason: intent.reason.clone(),
            previous: intent.goals.iter().filter_map(|n| registry.goals[n].history_head.clone().map(|h| (n.clone(),h))).collect(),
            completed_attempts: intent.goals.iter().map(|n| (n.clone(),registry.goals[n].completed_attempts)).collect() };
        state.request_seq = state.request_seq.checked_add(1).ok_or("request counter exhausted")?;
        let request = request(state, state.request_seq, &context);
        let identity = identity(&request, all)?;
        let ledger = state.dispatch.as_mut().unwrap();
        let id = ledger.verifiers.enqueue(identity, !intent.reviewer_requested, intent.reviewer_requested)?;
        ledger.verifiers.jobs.get_mut(&id).unwrap().request = Some(Box::new(request));
        ledger.verifiers.jobs.get_mut(&id).unwrap().retired_request = None;
        ledger.signal()?;
        for n in &intent.goals { let goal = state.coarse_goals.as_mut().unwrap().goals.get_mut(n).unwrap(); goal.last_audit_attempt = goal.completed_attempts; }
        state.coarse_goals.as_mut().unwrap().pending_audits.retain(|a| a != &intent);
    }
    Ok(())
}

fn history_path(repo: &Path, reference: &HistoryReference) -> Result<PathBuf, String> {
    if reference.bytes == 0 || reference.bytes > 8 * 1024 * 1024 || reference.sha256.len() != 64
        || !reference.sha256.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return Err("invalid coarse audit history reference".into());
    }
    Ok(repo.join(".trellis-history/coarse-audits").join(format!("{}.json", reference.sha256)))
}

pub fn encode(report: &AuditReport) -> Result<(HistoryReference, Vec<u8>), String> {
    let bytes = serde_json::to_vec(&serde_json::to_value(report).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
    Ok((HistoryReference { sha256: format!("{:x}", Sha256::digest(&bytes)), bytes: bytes.len() as u64 }, bytes))
}
fn decode(reference: &HistoryReference, bytes: &[u8]) -> Result<AuditReport, String> {
    if bytes.len() as u64 != reference.bytes || format!("{:x}", Sha256::digest(bytes)) != reference.sha256 {
        return Err("coarse audit history has wrong size or digest".into());
    }
    let report: AuditReport = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if report.version != 1 { return Err("unsupported coarse audit history version".into()); }
    if report.evidence.is_some() == report.migrated_from.is_some() {
        return Err("audit history must have a panel identity or a migration predecessor".into());
    }
    Ok(report)
}
fn read_bytes(repo: &Path, reference: &HistoryReference) -> Result<Vec<u8>, String> {
    let path = history_path(repo, reference)?;
    for path in [repo.join(".trellis-history"), repo.join(".trellis-history/coarse-audits"), path.clone()] {
        if path.symlink_metadata().map_err(|e|e.to_string())?.file_type().is_symlink() { return Err("audit history cannot follow symlinks".into()); }
    }
    let metadata = path.metadata().map_err(|e|e.to_string())?;
    if !metadata.is_file() || metadata.len() != reference.bytes { return Err("coarse audit history has wrong size or type".into()); }
    // Bound the read even if a corrupt file changes after stat.
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path).map_err(|e|e.to_string())?.take(reference.bytes + 1)
        .read_to_end(&mut bytes).map_err(|e|e.to_string())?;
    decode(reference, &bytes)?;
    Ok(bytes)
}
pub fn read(repo: &Path, reference: &HistoryReference) -> Result<AuditReport, String> {
    decode(reference, &read_bytes(repo, reference)?)
}
pub fn publish(repo: &Path, report: &AuditReport) -> Result<HistoryReference, String> {
    let (reference, bytes) = encode(report)?;
    publish_bytes(repo, &reference, &bytes)?;
    Ok(reference)
}
fn publish_bytes(repo: &Path, reference: &HistoryReference, bytes: &[u8]) -> Result<(), String> {
    let path = history_path(repo, reference)?;
    decode(reference, bytes)?;
    for part in [repo.join(".trellis-history"), repo.join(".trellis-history/coarse-audits")] {
        std::fs::create_dir_all(&part).map_err(|e|e.to_string())?;
        if part.symlink_metadata().map_err(|e|e.to_string())?.file_type().is_symlink() { return Err("audit history cannot follow symlinks".into()); }
    }
    if path.exists() { read(repo, reference)?; return Ok(()); }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e|e.to_string())?;
    let temp = path.with_extension(format!("{:032x}.tmp", u128::from_be_bytes(nonce)));
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp).map_err(|e|e.to_string())?;
    let result = (|| {
        file.write_all(bytes).and_then(|_|file.sync_all()).map_err(|e|e.to_string())?;
        match std::fs::hard_link(&temp, &path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read(repo, reference).map(|_| ()),
            Err(e) => Err(e.to_string()),
        }
    })();
    let _ = std::fs::remove_file(temp);
    result?;
    std::fs::File::open(path.parent().unwrap()).and_then(|d|d.sync_all()).map_err(|e|e.to_string())?;
    Ok(())
}

pub fn finish(mut state: ProtocolState, id: u64, response: WrapperResponse, evidence: VerifierIdentity)
    -> Result<crate::engine::TransitionOutcome, String> {
    use crate::engine::{ProtocolCommand, TransitionOutcome};
    let job = state.dispatch.as_ref().unwrap().verifiers.jobs[&id].clone();
    let frozen = job.request.as_ref().ok_or("scoped audit lost its request")?;
    let context = frozen.scoped_audit.as_ref().ok_or("missing audit scope")?.clone();
    let WrapperResponse::StuckMathAudit(mut response) = response else { return Err("scoped audit requires a StuckMathAudit result".into()) };
    response.request_id = frozen.id;
    response.cycle = state.cycle;
    let mut private = crate::dispatch_routing::verifier_state(&state, id)?;
    let refusal = if response.status != ResponseStatus::Ok {
        Some("malformed cone audit result".into())
    } else { crate::engine::stuck_math_audit_validation_failure_for_contract(&private, &response, None) };
    state.dispatch.as_mut().unwrap().verifiers.finish(id, &evidence)?;
    if let Some(reason) = refusal {
        // A malformed report is ordinary agent feedback, never a supervisor
        // failure or permission to advance a gate. Preserve the sealed result.
        if let VerifierStatus::Resolved { stale_reason, .. } = &mut state.dispatch.as_mut().unwrap().verifiers.jobs.get_mut(&id).unwrap().status {
            *stale_reason = Some(reason.clone());
        }
        if context.retry < STUCK_MATH_AUDIT_BURST_RETRY_LIMIT
            && context.goals.iter().all(|n| state.coarse_goals.as_ref()
                .and_then(|g| g.goals.get(n)).is_some_and(|g| g.status == GoalStatus::Active)) {
            submit(&mut state, AuditIntent { goals: context.goals, reason: context.reason,
                reviewer_requested: job.reviewer_requested, retry: context.retry + 1, feedback: reason.clone() })?;
        }
        state.dispatch.as_mut().unwrap().signal()?;
        return Ok(TransitionOutcome { state, commands: vec![ProtocolCommand::CommitCheckpoint] });
    }
    crate::engine::accept_ordinary_audit_plan(&mut private, &response);
    let plan = private.audit_plan.take().ok_or("accepted audit has no plan")?;
    let report = AuditReport { version: 1, context: context.clone(), evidence: Some(evidence), migrated_from: None, plan: plan.clone() };
    let (reference, _) = encode(&report)?;
    for node in &context.goals {
        let goal = state.coarse_goals.as_mut().unwrap().goals.get_mut(node).ok_or("audited goal disappeared")?;
        goal.plan = Some(plan.clone());
        goal.history_head = Some(reference.clone());
        goal.last_audit_attempt = goal.completed_attempts;
    }
    let mut commands = crate::engine::consume_process_memory_operations(&mut state, &response);
    commands.push(ProtocolCommand::PublishCoarseAudit { report: Box::new(report) });
    commands.push(ProtocolCommand::CommitCheckpoint);
    // Terminal panel inputs live in the verified report, not another expanding
    // full request in the hot scheduler ledger. Keep result/identity digests.
    let ledger = state.dispatch.as_mut().unwrap();
    let job = ledger.verifiers.jobs.get_mut(&id).unwrap();
    job.request = None;
    job.identity.input_hashes.clear();
    ledger.signal()?;
    state.in_flight_request = state.in_flight_request.as_ref().map(|r| Box::new(state.expected_request(r.id,r.kind)));
    state.validate()?;
    Ok(TransitionOutcome { state, commands })
}

pub fn needs_routing(state: &ProtocolState) -> bool {
    let Some(goals) = &state.coarse_goals else { return false };
    let Some(ledger) = &state.dispatch else { return false };
    if state.phase != Phase::ProofFormalization || state.audit_lane_in_flight() || ledger.draining.is_some() { return false; }
    for intent in &goals.pending_audits {
        if ledger.verifiers.jobs.values().any(|j| !matches!(j.status, VerifierStatus::Resolved { .. })
            && j.request.as_ref().and_then(|r| r.scoped_audit.as_ref()).is_some_and(|a| !a.goals.is_disjoint(&intent.goals))) { continue; }
        let Ok(nodes) = evidence_nodes(state, &intent.goals) else { return true };
        if !ledger.tasks.values().any(|t| (t.status.occupies_slot() || t.pending_retry.is_some())
            && (t.assignment.scope.exclusive || !t.assignment.scope.shared_writes.is_empty() || !t.assignment.scope.nodes.is_disjoint(&nodes))) {
            return true;
        }
    }
    goals.goals.iter().any(|(n,g)| g.status == GoalStatus::Active
        && g.attempts_since_progress >= u64::from(stuck_math_audit_shallow_coarse_no_progress_threshold())
        && g.completed_attempts.saturating_sub(g.last_audit_attempt) >= u64::from(stuck_math_audit_reaudit_interval_cycles())
        && !goals.pending_audits.iter().any(|a| a.goals.contains(n))
        && !ledger.verifiers.jobs.values().any(|j| !matches!(j.status, VerifierStatus::Resolved { .. })
            && j.request.as_ref().and_then(|r| r.scoped_audit.as_ref()).is_some_and(|a| a.goals.contains(n))))
}

pub fn verify_history_with(state: &ProtocolState, mut resolve: impl FnMut(&HistoryReference) -> Result<AuditReport, String>) -> Result<(), String> {
    let mut pending: Vec<_> = state.coarse_goals.iter().flat_map(|g| &g.goals)
        .filter_map(|(n,g)| g.history_head.clone().map(|r| (n.clone(),r))).collect();
    let mut seen = BTreeSet::new();
    while let Some((node, reference)) = pending.pop() {
        if !seen.insert((node.clone(), reference.sha256.clone())) { continue; }
        let report = resolve(&reference)?;
        if !report.context.goals.contains(&node) { return Err("coarse audit history belongs to a different goal".into()); }
        pending.extend(report.context.previous.into_iter());
    }
    Ok(())
}
pub fn export_history(source: &Path, destination: &Path, state: &ProtocolState) -> Result<(), String> {
    verify_history_with(state, |reference| {
        let bytes = read_bytes(source, reference)?;
        publish_bytes(destination, reference, &bytes)?;
        decode(reference, &bytes)
    })
}
pub fn read_git(repo: &Path, revision: &str, reference: &HistoryReference) -> Result<AuditReport, String> {
    history_path(repo, reference)?;
    let object = format!("{revision}:.trellis-history/coarse-audits/{}.json", reference.sha256);
    let size = std::process::Command::new("git").arg("-C").arg(repo).args(["cat-file", "-s", &object])
        .output().map_err(|e|e.to_string())?;
    if !size.status.success() || String::from_utf8_lossy(&size.stdout).trim().parse::<u64>().ok() != Some(reference.bytes) {
        return Err(format!("invalid committed audit history size {}", reference.sha256));
    }
    let out = std::process::Command::new("git").arg("-C").arg(repo).args(["cat-file", "blob", &object])
        .output().map_err(|e|e.to_string())?;
    if !out.status.success() || out.stdout.len() as u64 != reference.bytes
        || format!("{:x}",Sha256::digest(&out.stdout)) != reference.sha256 { return Err(format!("invalid committed audit history object {}", reference.sha256)); }
    decode(reference, &out.stdout)
}

pub fn response_restriction(context: &AuditContext, response: &StuckMathAuditResponse) -> Option<String> {
    let allowed = ["request_id", "cycle", "status", "report", "tasks", "probe_paths", "memory_operations"];
    let actual = serde_json::to_value(response).expect("audit response is JSON");
    let defaults = serde_json::to_value(StuckMathAuditResponse::default()).unwrap();
    if let Some((key,_)) = actual.as_object().unwrap().iter().find(|(key,value)| !allowed.contains(&key.as_str()) && defaults.get(*key) != Some(*value)) {
        return Some(format!("cone audits cannot set {key}; request a run-wide audit for changes to process authority"));
    }
    for op in &response.memory_operations {
        let permitted = if op.op == "add" { context.goals.contains(&NodeId::from(op.coarse_node.trim())) }
            else { context.memory_entry_ids.contains(op.entry_id.trim()) };
        if !permitted { return Some("a cone audit may update process-memory only for its named goals; request a run-wide audit for shared entries".into()); }
    }
    None
}

/// Freshness retries retain the requested question, without spending the
/// malformed-response retry allowance. Resolved panels no longer reserve it.
pub fn requeue_stale(state: &mut ProtocolState, id: u64) -> Result<(), String> {
    let job = state.dispatch.as_ref().unwrap().verifiers.jobs[&id].clone();
    let Some(scope) = job.request.as_ref().and_then(|r| r.scoped_audit.as_ref()) else { return Ok(()) };
    if state.phase == Phase::ProofFormalization && scope.goals.iter().all(|n|
        state.coarse_goals.as_ref().and_then(|g| g.goals.get(n)).is_some_and(|g| g.status == GoalStatus::Active)) {
        submit(state, AuditIntent { goals: scope.goals.clone(), reason: scope.reason.clone(),
            reviewer_requested: job.reviewer_requested, retry: scope.retry, feedback: scope.feedback.clone() })?;
    }
    Ok(())
}

/// Carry the previous accepted serial plan across migration as historical
/// advice. Its predecessor digest is provenance, never fresh audit evidence.
pub fn archive_migrated_plans(state: &mut ProtocolState, predecessor: &str) -> Result<Vec<crate::engine::ProtocolCommand>, String> {
    let mut commands = Vec::new();
    let Some(registry) = &mut state.coarse_goals else { return Ok(commands) };
    for (node, goal) in &mut registry.goals {
        let Some(plan) = goal.plan.as_ref().filter(|_| goal.history_head.is_none()) else { continue };
        let report = AuditReport { version: 1, evidence: None, migrated_from: Some(predecessor.into()), plan: plan.clone(),
            context: AuditContext { retry: 0, feedback: String::new(), memory_entry_ids: BTreeSet::new(),
                goals: [node.clone()].into(), nodes: BTreeSet::new(), reason: "Previously accepted plan, retained at coarse-goal migration; recheck against current sources".into(),
                previous: BTreeMap::new(), completed_attempts: BTreeMap::new(), recent_attempts: Vec::new() } };
        goal.history_head = Some(encode(&report)?.0);
        commands.push(crate::engine::ProtocolCommand::PublishCoarseAudit { report: Box::new(report) });
    }
    Ok(commands)
}

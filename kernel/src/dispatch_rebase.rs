//! Reconstruct a fixed-structure checked worker delta on current mathematics.
//!
//! The host must authenticate the original completion, bind exact source and
//! tool/policy inputs, and keep conflicting host checks reserved through
//! publication. This module does not turn a source prefix or a diagnostic read
//! trace into proof evidence. Artifact/certificate reconciliation still runs at
//! the ordinary native publication boundary.
use crate::dispatch::DependencyGraph;
use crate::engine::{apply_event, ProtocolCommand, ProtocolEvent};
use crate::model::*;
use std::collections::{BTreeMap, BTreeSet};

/// Bounded diagnostic only; never used to decide whether reuse is legal.
pub fn refusal_diagnostic(lane: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({"lane": lane, "reason": reason.chars().take(1024).collect::<String>(),
                      "truncated": reason.chars().nth(1024).is_some()})
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RebaseFootprint {
    pub write_nodes: BTreeSet<NodeId>,
    pub effect_nodes: BTreeSet<NodeId>,
    pub read_nodes: BTreeSet<NodeId>,
    pub targets: BTreeSet<TargetId>,
}

/// Imports, semantic dependencies and recursive certificate consumers all
/// participate. Target aggregates are shared writes even when source writes
/// are disjoint. The caller treats unsupported source/structure edits as
/// exclusive; sharing an immutable dependency is only a read/read overlap.
pub fn footprint(
    state: &ProtocolState, graph: &DependencyGraph, writes: &BTreeSet<NodeId>,
) -> Result<RebaseFootprint, String> {
    if !writes.is_subset(&state.live.present_nodes) {
        return Err("checked response rebase requires existing source owners".into());
    }
    let mut edges = graph.edges.clone();
    for (node, record) in &state.local_closure_records {
        let deps = edges.entry(node.clone()).or_default();
        deps.extend(record.authoritative_dependency_owners(state));
        if let Some(cert) = &record.node_certificate {
            deps.extend(cert.dependency_certificate_roots.keys().cloned());
        }
    }
    let mut effects = writes.clone();
    loop {
        let consumers: BTreeSet<_> = edges.iter()
            .filter(|(_, deps)| !deps.is_disjoint(&effects)).map(|(node, _)| node.clone()).collect();
        let old_len = effects.len();
        effects.extend(consumers);
        if effects.len() == old_len { break; }
    }
    let targets: BTreeSet<_> = state.live.coverage.iter().chain(state.live.protected_closure_nodes_per_target.iter())
        .filter(|(_, nodes)| !nodes.is_disjoint(&effects)).map(|(target, _)| target.clone()).collect();
    // Snapshot deltas may contain changed consumer and target observations,
    // not just owned proof probes. Their complete compiled inputs must remain
    // exact too; certificate reconciliation does not recompute these facts.
    let mut compiled_roots = effects.clone();
    for target in &targets {
        compiled_roots.extend(state.live.coverage.get(target).into_iter().flatten().cloned());
        compiled_roots.extend(state.live.protected_closure_nodes_per_target.get(target).into_iter().flatten().cloned());
    }
    if let Some(request) = state.in_flight_request.as_deref() {
        compiled_roots.extend(crate::runtime_cli_observations::paper_target_corr_reopen_guard_nodes(
            &request.approved_target_nodes, &request.approved_corr_fingerprints));
    }
    let read_nodes = DependencyGraph { edges }.closure(&compiled_roots)?;
    Ok(RebaseFootprint { write_nodes: writes.clone(), effect_nodes: effects, read_nodes, targets })
}

/// Host-authenticated complete source inventory. Extra source-view members
/// (publication manifest and dispatch prefix hints) are not reuse authority.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct SourceInputs {
    pub graph: DependencyGraph,
    pub input_hashes: BTreeMap<String, String>,
    /// Host-only checker and artifact carriers are never worker candidate paths.
    #[serde(default)]
    pub authority_inputs: BTreeMap<String, String>,
}

fn generated_index(path: &str) -> bool {
    matches!(path, "Tablet/INDEX.md" | "Tablet/README.md")
}

/// Exact source freshness for a typed read set. Only registered sibling node
/// source pairs may differ. All other paths, including header.tex, Preamble,
/// reference inputs and checker/configuration carriers remain exact. Missing
/// TeX is a recorded absence, not permission to ignore a newly created file.
pub fn check_source_inputs(
    before: &ProtocolState, current: &ProtocolState, old: &SourceInputs, new: &SourceInputs,
    reads: &BTreeSet<NodeId>,
) -> Result<(), String> {
    if before.live.present_nodes != current.live.present_nodes || old.graph != new.graph {
        return Err("checked response source membership or dependency graph changed".into());
    }
    if old.input_hashes.is_empty() || new.input_hashes.is_empty() {
        return Err("checked response requires complete captured source inputs".into());
    }
    if old.authority_inputs != new.authority_inputs {
        return Err("checked response host authority inputs changed".into());
    }
    let node_file = |path: &str| path.strip_prefix("Tablet/")
        .and_then(|file| file.strip_suffix(".lean").or_else(|| file.strip_suffix(".tex")))
        .map(NodeId::from).filter(|node| before.live.present_nodes.contains(node));
    for path in old.input_hashes.keys().chain(new.input_hashes.keys()).collect::<BTreeSet<_>>() {
        if generated_index(path) { continue; }
        if node_file(path).is_some_and(|node| !reads.contains(&node)) { continue; }
        if old.input_hashes.get(path) != new.input_hashes.get(path) {
            return Err(format!("checked response semantic source input changed: {path}"));
        }
    }
    for node in reads {
        for extension in ["lean", "tex"] {
            let path = format!("Tablet/{node}.{extension}");
            let valid = |value: Option<&String>| value.is_some_and(|hash|
                extension == "tex" && hash == "absent" || hash.len() == 64
                    && hash.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
            if !valid(old.input_hashes.get(&path)) || !valid(new.input_hashes.get(&path)) {
                return Err(format!("checked response lacks exact source read identity: {path}"));
            }
        }
    }
    Ok(())
}

pub fn supported_request(state: &ProtocolState) -> bool {
    let Some(request) = state.in_flight_request.as_deref() else { return false; };
    let closure = request.task_execution.as_ref().is_some_and(|execution|
        execution.task_kind == crate::dispatch::WorkerTaskKind::LeanClosure);
    request.kind == RequestKind::Worker && state.stage == Stage::Worker
        && state.worker_execution.is_some() && !state.live.modeling_draft
        && request.assumption_authoring.is_none()
        && (state.phase == Phase::ProofFormalization && matches!(request.worker_acceptance.validation_kind,
            WorkerValidationKind::ProofEasy | WorkerValidationKind::ProofLocal | WorkerValidationKind::ProofRestructure)
            || closure && matches!(state.phase, Phase::TheoremStating | Phase::RevisionStating))
}

fn merge_map<K: Ord + Clone + std::fmt::Debug, V: Clone + PartialEq>(
    label: &str, before: &BTreeMap<K, V>, checked: &BTreeMap<K, V>, current: &BTreeMap<K, V>,
    allowed: &BTreeSet<K>,
) -> Result<BTreeMap<K, V>, String> {
    let mut merged = current.clone();
    for key in before.keys().chain(checked.keys()).collect::<BTreeSet<_>>() {
        if before.get(key) == checked.get(key) { continue; }
        if !allowed.contains(key) { return Err(format!("checked response changes {label} outside its effect footprint: {key:?}")); }
        if current.get(key) != before.get(key) { return Err(format!("checked response conflicts with current {label}: {key:?}")); }
        match checked.get(key) {
            Some(value) => { merged.insert(key.clone(), value.clone()); }
            None => { merged.remove(key); }
        }
    }
    Ok(merged)
}

fn merge_set<K: Ord + Clone + std::fmt::Debug>(
    label: &str, before: &BTreeSet<K>, checked: &BTreeSet<K>, current: &BTreeSet<K>, allowed: &BTreeSet<K>,
) -> Result<BTreeSet<K>, String> {
    let mut merged = current.clone();
    for key in before.symmetric_difference(checked) {
        if !allowed.contains(key) { return Err(format!("checked response changes {label} outside its effect footprint: {key:?}")); }
        if current.contains(key) != before.contains(key) { return Err(format!("checked response conflicts with current {label}: {key:?}")); }
        if checked.contains(key) { merged.insert(key.clone()); } else { merged.remove(key); }
    }
    Ok(merged)
}

/// Exhaustive construction deliberately makes a new snapshot field a compile
/// error here until its read/write semantics have been considered.
fn rebase_snapshot(
    before: &WorkingSnapshot, checked: &WorkingSnapshot, current: &WorkingSnapshot, scope: &RebaseFootprint,
) -> Result<WorkingSnapshot, String> {
    if before.modeling_draft || checked.modeling_draft || current.modeling_draft
        || before.present_nodes != checked.present_nodes || before.present_nodes != current.present_nodes {
        return Err("checked response rebase requires fixed certified membership".into());
    }
    macro_rules! nodes { ($field:ident) => { merge_map(stringify!($field), &before.$field, &checked.$field, &current.$field, &scope.effect_nodes)? }; }
    macro_rules! targets { ($field:ident) => { merge_map(stringify!($field), &before.$field, &checked.$field, &current.$field, &scope.targets)? }; }
    macro_rules! set { ($field:ident) => { merge_set(stringify!($field), &before.$field, &checked.$field, &current.$field, &scope.effect_nodes)? }; }
    if before.deviation_current_fingerprints != checked.deviation_current_fingerprints
        || before.deviation_current_fingerprints != current.deviation_current_fingerprints
        || before.challenge_coverage != checked.challenge_coverage || before.challenge_coverage != current.challenge_coverage {
        return Err("checked response rebase cannot change deviation or challenge coverage".into());
    }
    Ok(WorkingSnapshot {
        modeling_draft: false, present_nodes: current.present_nodes.clone(),
        open_nodes: set!(open_nodes), coverage: targets!(coverage),
        target_fingerprints: nodes!(target_fingerprints), corr_current_fingerprints: nodes!(corr_current_fingerprints),
        paper_current_fingerprints: targets!(paper_current_fingerprints),
        sound_current_fingerprints: nodes!(sound_current_fingerprints),
        deviation_current_fingerprints: current.deviation_current_fingerprints.clone(),
        sound_current_fingerprint_parts: nodes!(sound_current_fingerprint_parts),
        sketch_proof_nodes: set!(sketch_proof_nodes),
        placeholder_definition_nodes: set!(placeholder_definition_nodes),
        substantiveness_current_fingerprints: nodes!(substantiveness_current_fingerprints),
        protected_closure_nodes_per_target: targets!(protected_closure_nodes_per_target),
        challenge_coverage: current.challenge_coverage.clone(),
    })
}

/// A successful checker may emit no-op structural rows. They are harmless;
/// actual graph, membership, kind or claim updates require exclusive checking.
fn fixed_response(before: &ProtocolState, response: &WorkerResponse) -> Result<(), String> {
    if response.status != ResponseStatus::Ok || response.outcome != WorkerOutcome::Valid
        || !response.acceptance_check_passed || response.transport_failure || response.preflight_error.is_some()
        || !response.deterministic_rejection_reasons.is_empty() {
        return Err("checked response rebase requires an authoritative Valid result".into());
    }
    let same_proofs = response.proof_node_updates.iter().all(|(n, u)| match u {
        Update::Same => true, Update::Set(v) => before.proof_nodes.contains(n) == *v,
    });
    let same_kinds = response.node_kind_updates.iter().all(|(n, u)| match u {
        Update::Same => true, Update::Set(v) => before.node_kinds.get(n) == Some(v),
    });
    let same_deps = response.dep_updates.iter().all(|(n, u)| match u {
        Update::Same => true, Update::Set(v) => before.deps.get(n) == Some(v),
    });
    let same_targets = response.target_claim_updates.iter().all(|(n, u)| match u {
        Update::Same => true, Update::Set(v) => before.target_claims.get(n) == Some(v),
    });
    if !same_proofs || !same_kinds || !same_deps || !same_targets
        || !response.challenge_claim_updates.is_empty() || !response.difficulty_updates.is_empty()
        || !response.deviation_requests.is_empty() || !response.node_deviation_claims.is_empty()
        || !response.node_reference_grounds.is_empty() || !response.deviation_deletions.is_empty()
        || !response.deleted_nodes.is_empty() || !response.protected_semantic_change_nodes.is_empty()
        || response.rust_witness_artifact.is_some() || response.rust_witness_artifact_record.is_some()
        || response.rust_witness_artifact_payload.is_some() || response.conditional_theorem_proposal.is_some()
        || response.conditional_statement_derivation.is_some() || !response.conditional_theorem_withdrawals.is_empty()
        || !response.conditional_assumption_snapshots.is_empty() || !response.authored_assumption_id.is_empty()
        || !response.authored_axiom_name.is_empty() || !response.staged_assumption_lean_statement.is_empty()
        || !response.staged_assumption_nl_statement.is_empty() || !response.authored_citation_locator.is_empty()
        || !response.authored_rust_justification.is_empty() || !response.authored_claim_class.is_empty()
        || response.cleanup_transition_preflight.is_some() {
        return Err("checked response requires exclusive structural or trust acceptance".into());
    }
    Ok(())
}

fn same_projection<K: Ord, V: PartialEq>(a: &BTreeMap<K, V>, b: &BTreeMap<K, V>, keys: &BTreeSet<K>) -> bool {
    keys.iter().all(|key| a.get(key) == b.get(key))
}

/// Reviewed old producers may donate authenticated fixed-structure checks to
/// this implementation. This does not waive any source, policy or transition
/// guard, and must never rewrite the original request or completion receipt.
fn compatible_producer(old: &str, current: &str) -> bool {
    compatible_producer_for_lane(old, current, "proof")
}

fn compatible_producer_for_lane(old: &str, current: &str, lane: &str) -> bool {
    if old == current { return true; }
    if current != crate::model::acceptance_logic_identity() { return false; }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Policy { version: u32, identities: BTreeSet<String>,
        #[serde(default)] modeling_identities: BTreeSet<String>,
        #[serde(default)] theorem_stating_identities: BTreeSet<String> }
    static POLICY: std::sync::OnceLock<Policy> = std::sync::OnceLock::new();
    let policy = POLICY.get_or_init(|| serde_json::from_str(include_str!(
        "../../trellis/dispatch/compatible_acceptance_producers.json"
    )).expect("embedded acceptance producer compatibility policy"));
    policy.version == 1 && match lane {
        "modeling" => &policy.modeling_identities,
        "theorem_stating" => &policy.theorem_stating_identities,
        _ => &policy.identities,
    }.contains(old)
}

/// Compare acceptance operands, not administrative state identity. Reports,
/// scheduling counters, audit advice and process memory are not proof facts;
/// the current engine consumes their current values. The authorized envelope,
/// mathematical registries and protections remain exact. Approval changes
/// relevant to the checked read/effect closure also remain exact.
fn same_authority(before: &ProtocolState, current: &ProtocolState, scope: &RebaseFootprint) -> Result<(), String> {
    let old = before.in_flight_request.as_deref().ok_or("original worker request is missing")?;
    let new = current.in_flight_request.as_deref().ok_or("current worker request is missing")?;
    if !supported_request(before) || !supported_request(current) || current.phase != before.phase {
        return Err("checked response rebase requires an isolated fixed-structure proof worker".into());
    }
    macro_rules! same { ($($field:ident),+ $(,)?) => { $(if old.$field != new.$field {
        return Err(format!("checked response acceptance operand changed: {}", stringify!($field)));
    })+ }; }
    if !compatible_producer(&old.acceptance_logic_identity, &new.acceptance_logic_identity) {
        return Err("checked response acceptance implementation is not compatible".into());
    }
    same!(work_kind, active_node, held_target, mode, task_execution,
        worker_submission_policy, worker_context, worker_acceptance, configured_targets,
        configured_challenge_targets, current_challenge_claims, node_role, is_pv, trust_base_required_v1,
        pv_goal_mode, lean_statement_transforms_enabled, conditional_coexistence_enabled,
        model_preservation_enabled, modeling_draft, pv_live_polarity, trust_seam_repair_context,
        authorized_deviations, rejected_deviations, current_deviation_files, node_deviation_claims,
        configured_reference_papers, node_reference_grounds, approved_target_nodes, coarse_dag_nodes,
        protected_semantic_change_confirmation, protected_reapproval_nodes,
        protected_reapproval_corr_fingerprint_pairs, current_present_nodes, current_proof_nodes,
        current_node_kinds, current_deps, current_target_claims, assumption_authoring,
        pending_assumption_draft, staged_assumptions_awaiting_admission, revision_context,
        human_input_outstanding, gate_kind, global_repair_mode_enabled, consumed_global_repair_grant);
    if before.trust_base != current.trust_base || before.approved_targets != current.approved_targets
        || before.tablet_target != current.tablet_target || before.node_target != current.node_target
        || before.proof_nodes != current.proof_nodes || before.node_kinds != current.node_kinds
        || before.deps != current.deps || before.target_claims != current.target_claims {
        return Err("checked response mathematical authority or structure changed".into());
    }
    if !same_projection(&old.approved_corr_fingerprints, &new.approved_corr_fingerprints, &scope.read_nodes)
        || !same_projection(&old.current_paper_approved_fingerprints, &new.current_paper_approved_fingerprints, &scope.targets) {
        return Err("checked response relevant approval changed".into());
    }
    // Presence is fixed. Every mathematical observation in the read closure
    // must still have its original value; source hashes are checked by the
    // host in addition, including definitions and imported theorem bodies.
    macro_rules! node_map { ($($field:ident),+ $(,)?) => { $(if !same_projection(&before.live.$field, &current.live.$field, &scope.read_nodes) {
        return Err(format!("checked response semantic read changed: {}", stringify!($field)));
    })+ }; }
    node_map!(target_fingerprints, corr_current_fingerprints, sound_current_fingerprints,
        sound_current_fingerprint_parts, substantiveness_current_fingerprints);
    if !same_projection(&before.live.paper_current_fingerprints, &current.live.paper_current_fingerprints, &scope.targets)
        || !same_projection(&before.live.coverage, &current.live.coverage, &scope.targets)
        || !same_projection(&before.live.protected_closure_nodes_per_target, &current.live.protected_closure_nodes_per_target, &scope.targets) {
        return Err("checked response relevant target aggregate changed".into());
    }
    for (old, new) in [(&before.live.open_nodes, &current.live.open_nodes),
        (&before.live.sketch_proof_nodes, &current.live.sketch_proof_nodes),
        (&before.live.placeholder_definition_nodes, &current.live.placeholder_definition_nodes)] {
        if old.symmetric_difference(new).any(|node| scope.read_nodes.contains(node)) {
            return Err("checked response source classification changed in its read closure".into());
        }
    }
    Ok(())
}

/// The ordinary host normalizer reports explicit-claim coverage. Native state
/// also credits the present live side of authored Decide goals and refutations.
/// Reconstruct only that derived difference, never a new claim or registry.
/// The caller has authenticated a fixed-structure Valid completion; all source,
/// footprint, snapshot and current-transition checks remain unchanged.
fn reconcile_derived_challenge_coverage(
    before: &ProtocolState, current: &ProtocolState, response: &mut WorkerResponse,
) -> Result<(), String> {
    if response.snapshot.challenge_coverage == before.live.challenge_coverage {
        return Ok(());
    }
    if !response.challenge_claim_updates.is_empty()
        || before.live.present_nodes != response.snapshot.present_nodes
        || before.live.present_nodes != current.live.present_nodes
        || before.challenge_claims != current.challenge_claims
        || before.configured_challenge_targets != current.configured_challenge_targets
        || before.authored_decide_goals != current.authored_decide_goals
        || before.pv_live_polarity != current.pv_live_polarity
        || before.live.challenge_coverage != current.live.challenge_coverage {
        return Err("checked response derived challenge coverage authority changed".into());
    }
    for state in [before, current] {
        let request = state.in_flight_request.as_deref().ok_or("challenge coverage request missing")?;
        if request.current_challenge_claims != state.challenge_claims
            || request.configured_challenge_targets != state.configured_challenge_targets
            || state.live.challenge_coverage != state.challenge_coverage_from_claims_with_present(
                &state.challenge_claims, &state.live.present_nodes) {
            return Err("challenge coverage is not its native authority projection".into());
        }
    }
    let explicit = crate::worker_normalization::challenge_coverage_from_claims(
        &before.configured_challenge_targets.keys().cloned().collect(),
        &before.challenge_claims, &before.live.present_nodes);
    if response.snapshot.challenge_coverage != explicit {
        return Err("checked response challenge coverage is not the exact host claim projection".into());
    }
    response.snapshot.challenge_coverage = current.live.challenge_coverage.clone();
    Ok(())
}

/// This is a pure delta reconstruction, not a replacement ProtocolState.
/// `writes` and the graph must be derived from authenticated host source views.
/// Source/read-set and artifact checks remain mandatory at the caller.
pub fn rebase_checked_response(
    before: &ProtocolState, current: &ProtocolState, checked: &WorkerResponse,
    before_graph: &DependencyGraph, current_graph: &DependencyGraph, writes: &BTreeSet<NodeId>,
) -> Result<WorkerResponse, String> {
    fixed_response(before, checked)?;
    if before.in_flight_request.as_ref().map(|request| request.id) != Some(checked.request_id)
        || checked.cycle != before.cycle {
        return Err("checked response does not match its original worker request".into());
    }
    let original_scope = footprint(before, before_graph, writes)?;
    let scope = footprint(current, current_graph, writes)?;
    if scope != original_scope { return Err("checked response dependency/effect footprint changed".into()); }
    same_authority(before, current, &scope)?;
    let request = current.in_flight_request.as_ref().unwrap();
    let mut authorized = request.worker_context.authorized_nodes.clone();
    authorized.extend(request.worker_acceptance.authorized_nodes.iter().cloned());
    if matches!(request.worker_acceptance.validation_kind, WorkerValidationKind::ProofEasy | WorkerValidationKind::ProofLocal) {
        authorized.extend(request.active_node.iter().cloned());
    }
    if !writes.is_subset(&authorized) {
        return Err("checked response source writes exceed its current authorized scope".into());
    }
    let mut response = checked.clone();
    reconcile_derived_challenge_coverage(before, current, &mut response)?;
    response.snapshot = rebase_snapshot(&before.live, &response.snapshot, &current.live, &scope)?;
    // No-op structural rows must not spuriously invalidate sibling records.
    response.proof_node_updates.clear(); response.node_kind_updates.clear();
    response.dep_updates.clear(); response.target_claim_updates.clear();
    // Shared-consumer probes also depend on their imported artifact
    // environment. Source/record equality alone does not authenticate it.
    // Retain only owned probes with the endpoint's exact compiled read binding;
    // the normal publication issuer reconciles every affected consumer fresh.
    let reusable_owner = |node: &NodeId| scope.write_nodes.contains(node);
    response.local_closure_results.retain(|node, _| reusable_owner(node));
    response.elaboration_costs.retain(|node, _| scope.effect_nodes.contains(node));
    response.late_heartbeats.retain(|node, _| scope.effect_nodes.contains(node));
    if let Some(batch) = &mut response.local_closure_revalidation {
        batch.refreshed.retain(|(node, _)| reusable_owner(node));
        batch.still_unverified.retain(|(node, _)| reusable_owner(node));
    }
    let request = current.in_flight_request.as_ref().unwrap();
    response.request_id = request.id; response.cycle = current.cycle;
    let outcome = apply_event(current.clone_without_dispatch(), ProtocolEvent::WrapperResponse {
        response: WrapperResponse::Worker(response.clone()),
    }).map_err(|error| format!("current worker transition rejected checked response rebase: {error:?}"))?;
    if outcome.state.phase != current.phase || outcome.state.stage != Stage::Reviewer
        || outcome.commands.iter().any(|command| !matches!(command, ProtocolCommand::DeleteLocalClosureRecord { .. })) {
        return Err("checked response rebase requires current acceptance or global routing work".into());
    }
    Ok(response)
}

/// Reuse a full ordinary theorem-stating check across exact delivery drift.
/// The original host completion remains authoritative for its checked sources;
/// the current native transition still decides structural scope and routing.
/// This lane carries no artifact witness and performs no scoped proof merge.
pub fn rebase_stating_context(
    before: &ProtocolState, current: &ProtocolState, checked: &WorkerResponse,
    old_source: &SourceInputs, new_source: &SourceInputs,
) -> Result<WorkerResponse, String> {
    let old = before.in_flight_request.as_deref().ok_or("original worker request is missing")?;
    let new = current.in_flight_request.as_deref().ok_or("current worker request is missing")?;
    if before.phase != Phase::TheoremStating || current.phase != Phase::TheoremStating
        || old.worker_acceptance.validation_kind != WorkerValidationKind::TheoremTargeted
        || before.stage != Stage::Worker || current.stage != Stage::Worker
        || old.kind != RequestKind::Worker || new.kind != RequestKind::Worker
        || old.id != checked.request_id || checked.cycle != before.cycle
        || before.worker_execution.as_ref().is_none_or(|execution|
            execution.task_kind != crate::dispatch::WorkerTaskKind::Ordinary
                || execution.compiler != crate::dispatch::CompilerCapability::Managed)
        || checked.status != ResponseStatus::Ok || checked.outcome != WorkerOutcome::Valid
        || !checked.acceptance_check_passed || checked.transport_failure
        || checked.preflight_error.is_some() || !checked.deterministic_rejection_reasons.is_empty()
    { return Err("theorem-stating context reuse requires an authenticated ordinary managed Valid check".into()); }
    // Cycle one has distinct initial SKETCH rules. Counters cannot rewind.
    if before.cycle <= 1 || current.cycle < before.cycle
        || current.cycles_since_clean < before.cycles_since_clean {
        return Err("theorem-stating context crossed an acceptance cycle boundary".into());
    }
    if old_source.input_hashes.is_empty() || old_source.authority_inputs.is_empty()
        || old_source.input_hashes != new_source.input_hashes
        || old_source.authority_inputs != new_source.authority_inputs || old_source.graph != new_source.graph {
        return Err("theorem-stating context sources or authority changed".into());
    }
    if !compatible_producer_for_lane(&old.acceptance_logic_identity, &new.acceptance_logic_identity, "theorem_stating") {
        return Err("theorem-stating checked producer is not compatible".into());
    }
    for (state, request) in [(before, old), (current, new)] {
        if request.id != state.request_seq || request.cycle != state.cycle
            || request.cycles_since_clean != state.cycles_since_clean
            || request.worker_closure_baseline.as_ref().is_some_and(|baseline| baseline.cycle != state.cycle) {
            return Err("theorem-stating context coordinates are not their native projection".into());
        }
        if request.review_verifier_evidence.sound != state.latest_sound_reviewer_evidence {
            return Err("theorem-stating Sound presentation is not its native projection".into());
        }
        let mut rendered = request.clone();
        crate::request_contracts::populate_request_prompt_contracts(&mut rendered, None);
        if state.tablet_target != crate::backend::BackendId::Lean
            || state.node_target.values().any(|target| *target != crate::backend::BackendId::Lean)
            || request.worker_contract.get("prompt_fragments")
                != rendered.worker_contract.get("prompt_fragments") {
            return Err("theorem-stating prompt fragments are not their native Lean projection".into());
        }
        if let Some(identity) = request.worker_contract.pointer("/request_summary/acceptance_logic_identity") {
            if identity.as_str() != Some(request.acceptance_logic_identity.as_str()) {
                return Err("theorem-stating contract producer is not its native projection".into());
            }
        }
    }
    // Native checkpoint history appends and evicts its oldest entry at the
    // fixed cap. Surviving observations and audit debounce must stay exact.
    // Actual approval authority is still compared exhaustively below.
    if !stating_history_extends(&before.progress_history, &current.progress_history) {
        return Err("theorem-stating progress history is not retained append-only bookkeeping".into());
    }
    let mut request = old.clone();
    request.id = new.id; request.cycle = new.cycle;
    request.cycles_since_clean = new.cycles_since_clean;
    request.acceptance_logic_identity = new.acceptance_logic_identity.clone();
    if let Some(baseline) = &mut request.worker_closure_baseline { baseline.cycle = current.cycle; }
    if let Some(identity) = request.worker_contract.pointer_mut("/request_summary/acceptance_logic_identity") {
        *identity = serde_json::json!(new.acceptance_logic_identity);
    }
    // Only removal of old advisory presentation is allowed. Nonempty new
    // evidence stays cold, and every approval/fingerprint remains exact.
    if current.latest_sound_reviewer_evidence.is_empty() && new.review_verifier_evidence.sound.is_empty() {
        request.review_verifier_evidence.sound.clear();
    }
    request.worker_contract["prompt_fragments"] = new.worker_contract["prompt_fragments"].clone();
    if request != *new { return Err("theorem-stating acceptance request changed".into()); }
    let mut projected = before.clone_without_dispatch();
    projected.cycle = current.cycle;
    projected.cycles_since_clean = current.cycles_since_clean;
    projected.request_seq = current.request_seq;
    projected.in_flight_request = current.in_flight_request.clone();
    projected.progress_history = current.progress_history.clone();
    if current.latest_review_rejection_reasons.is_empty() { projected.latest_review_rejection_reasons.clear(); }
    if current.latest_sound_review_nodes.is_empty() { projected.latest_sound_review_nodes.clear(); }
    if current.latest_sound_reviewer_evidence.is_empty() { projected.latest_sound_reviewer_evidence.clear(); }
    if projected != current.clone_without_dispatch() {
        return Err("theorem-stating mathematical state or authority changed".into());
    }
    let mut response = checked.clone();
    response.request_id = new.id; response.cycle = current.cycle;
    let outcome = apply_event(current.clone_without_dispatch(), ProtocolEvent::WrapperResponse {
        response: WrapperResponse::Worker(response.clone()),
    }).map_err(|error| format!("current theorem-stating transition rejected context reuse: {error:?}"))?;
    if outcome.state.phase != current.phase || outcome.state.stage != Stage::Reviewer
        || outcome.commands.iter().any(|command| !matches!(command, ProtocolCommand::DeleteLocalClosureRecord { .. })) {
        return Err("theorem-stating context reuse requires current rejection or global routing".into());
    }
    Ok(response)
}

fn stating_history_extends(
    before: &crate::progress_history::ProgressHistory,
    current: &crate::progress_history::ProgressHistory,
) -> bool {
    use crate::progress_history::PROGRESS_HISTORY_MAX_SNAPSHOTS as CAP;
    let well_formed = |history: &crate::progress_history::ProgressHistory| {
        let length = history.next_index.min(CAP as u64) as usize;
        history.next_index != u64::MAX
            && history.snapshots.len() == length
            && history.last_dispatched_index.is_none_or(|index| index < history.next_index)
            && history.snapshots.iter().enumerate().all(|(offset, snapshot)|
                snapshot.snapshot_index == history.next_index - length as u64 + offset as u64)
    };
    if !well_formed(before) || !well_formed(current)
        || before.last_dispatched_index != current.last_dispatched_index
        || current.next_index < before.next_index {
        return false;
    }
    let first = current.next_index - current.snapshots.len() as u64;
    // With no surviving old observation the endpoints cannot authenticate
    // continuity. Leave that case cold rather than infer an unseen chain.
    if !before.snapshots.is_empty() && first >= before.next_index {
        return false;
    }
    before.snapshots.iter().filter(|snapshot| snapshot.snapshot_index >= first)
        .all(|snapshot| current.snapshots.get((snapshot.snapshot_index - first) as usize) == Some(snapshot))
}

/// Reuse a completed ordinary Modeling check across delivery bookkeeping only.
/// Its entire source inventory, mathematical state, approvals and acceptance
/// contract remain exact. Scope/structure edits in the checked result need no
/// new observation when only the current cycle/progress history has advanced.
/// The host authenticates the completion; current native transition checks
/// consume the current counters/history rather than restoring the old state.
pub fn rebase_modeling_context(
    before: &ProtocolState, current: &ProtocolState, checked: &WorkerResponse,
    old_source: &SourceInputs, new_source: &SourceInputs,
) -> Result<WorkerResponse, String> {
    let old = before.in_flight_request.as_deref().ok_or("original worker request is missing")?;
    let new = current.in_flight_request.as_deref().ok_or("current worker request is missing")?;
    if before.phase != Phase::Modeling || current.phase != Phase::Modeling
        || before.stage != Stage::Worker || current.stage != Stage::Worker
        || old.kind != RequestKind::Worker || new.kind != RequestKind::Worker
        || old.id != checked.request_id || checked.cycle != before.cycle
        || before.worker_execution.as_ref().is_none_or(|execution|
            execution.task_kind != crate::dispatch::WorkerTaskKind::Ordinary)
        || checked.status != ResponseStatus::Ok || checked.outcome != WorkerOutcome::Valid
        || !checked.acceptance_check_passed || checked.transport_failure
        || checked.preflight_error.is_some() || !checked.deterministic_rejection_reasons.is_empty()
    { return Err("Modeling context reuse requires an authenticated ordinary Valid check".into()); }
    if old_source.input_hashes.is_empty() || old_source.input_hashes != new_source.input_hashes
        || old_source.authority_inputs != new_source.authority_inputs || old_source.graph != new_source.graph {
        return Err("Modeling context reuse sources or authority changed".into());
    }
    if !compatible_producer_for_lane(&old.acceptance_logic_identity, &new.acceptance_logic_identity, "modeling") {
        return Err("Modeling checked producer is not compatible".into());
    }
    let mut request = old.clone();
    request.id = new.id;
    request.cycle = new.cycle;
    request.cycles_since_clean = new.cycles_since_clean;
    request.acceptance_logic_identity = new.acceptance_logic_identity.clone();
    // Only a native projection of the delivery cycle may be rebound. Every
    // closure baseline input and every other rendered contract byte stays exact.
    for (state, original) in [(before, old), (current, new)] {
        if original.cycle != state.cycle || original.cycles_since_clean != state.cycles_since_clean
            || original.worker_closure_baseline.as_ref().is_some_and(|baseline| baseline.cycle != state.cycle) {
            return Err("Modeling context coordinates are not their native projection".into());
        }
        if let Some(identity) = original.worker_contract.pointer("/request_summary/acceptance_logic_identity") {
            if identity.as_str() != Some(original.acceptance_logic_identity.as_str()) {
                return Err("Modeling contract producer identity is not its native projection".into());
            }
        }
    }
    if let Some(baseline) = &mut request.worker_closure_baseline { baseline.cycle = current.cycle; }
    if let Some(identity) = request.worker_contract.pointer_mut("/request_summary/acceptance_logic_identity") {
        *identity = serde_json::json!(new.acceptance_logic_identity);
    }
    if request != *new { return Err("Modeling context reuse acceptance request changed".into()); }
    let mut projected = before.clone_without_dispatch();
    projected.cycle = current.cycle;
    projected.cycles_since_clean = current.cycles_since_clean;
    projected.progress_history = current.progress_history.clone();
    projected.request_seq = current.request_seq;
    projected.in_flight_request = current.in_flight_request.clone();
    if projected != current.clone_without_dispatch() {
        return Err("Modeling context reuse mathematical state or authority changed".into());
    }
    let mut response = checked.clone();
    response.request_id = new.id;
    response.cycle = current.cycle;
    let outcome = apply_event(current.clone_without_dispatch(), ProtocolEvent::WrapperResponse {
        response: WrapperResponse::Worker(response.clone()),
    }).map_err(|error| format!("current Modeling transition rejected context reuse: {error:?}"))?;
    if outcome.state.phase != current.phase || outcome.state.stage != Stage::Reviewer
        || outcome.commands.iter().any(|command| !matches!(command, ProtocolCommand::DeleteLocalClosureRecord { .. })) {
        return Err("Modeling context reuse requires current rejection or global routing".into());
    }
    Ok(response)
}

/// The exclusive Modeling full check may outlive verifier delivery. Reuse its
/// exact checked sources/facts only when new approvals protect the SAME checked
/// meaning and every other actual acceptance operand is unchanged. This is not
/// fixed-structure rebase, nor retrospective artifact/environment attestation.
pub fn rebase_modeling_verifier_delivery(
    before: &ProtocolState, current: &ProtocolState, checked: &WorkerResponse,
    old_source: &SourceInputs, new_source: &SourceInputs,
) -> Result<WorkerResponse, String> {
    let old = before.in_flight_request.as_deref().ok_or("original worker request is missing")?;
    let new = current.in_flight_request.as_deref().ok_or("current worker request is missing")?;
    if before.phase != Phase::Modeling || current.phase != Phase::Modeling
        || before.cycle != current.cycle || old.id != checked.request_id || checked.cycle != before.cycle
        || old.kind != RequestKind::Worker || new.kind != RequestKind::Worker
        || old.worker_context.validation_kind != WorkerValidationKind::TheoremGlobal
        || before.worker_execution.as_ref().is_none_or(|execution|
            execution.task_kind != crate::dispatch::WorkerTaskKind::Ordinary
                || execution.compiler != crate::dispatch::CompilerCapability::Managed)
        || !matches!(old.worker_acceptance.validation_execution_plan.as_slice(),
            [WorkerValidationExecutionPlanStep::ScopedTablet {
                allowed_nodes_mode: ScopedTabletAllowedNodesMode::AllPresent, explicit_nodes
            }] if explicit_nodes.is_empty())
        || !old.approved_target_nodes.is_empty()
    {
        return Err("Modeling reuse requires the exclusive all-present ordinary check".into());
    }
    if old_source.input_hashes.is_empty() || old_source.input_hashes != new_source.input_hashes
        || old_source.authority_inputs != new_source.authority_inputs || old_source.graph != new_source.graph {
        return Err("Modeling checked canonical sources or authority changed".into());
    }
    // Keep the original trust/conditional/deviation carrier exclusions. Only
    // ordinary checked structural metadata is allowed by this separate lane.
    if checked.challenge_claim_updates.iter().any(|(node, update)| match update {
        Update::Same => false,
        Update::Set(claims) => if before.live.present_nodes.contains(node) {
            before.challenge_claims.get(node).cloned().unwrap_or_default() != *claims
        } else { !claims.is_empty() },
    }) {
        return Err("Modeling verifier reuse cannot change challenge claims".into());
    }
    let mut carriers = checked.clone();
    carriers.proof_node_updates.clear(); carriers.node_kind_updates.clear();
    carriers.dep_updates.clear(); carriers.target_claim_updates.clear();
    carriers.challenge_claim_updates.clear(); carriers.difficulty_updates.clear();
    fixed_response(before, &carriers)?;
    if !compatible_producer_for_lane(&old.acceptance_logic_identity, &new.acceptance_logic_identity, "modeling") {
        return Err("Modeling checked producer is not compatible".into());
    }
    fn approval_additions<K: Ord>(old: &BTreeMap<K, String>, new: &BTreeMap<K, String>,
        prior: &BTreeMap<K, String>, checked: &BTreeMap<K, String>,
        old_status: &BTreeMap<K, CorrStatus>, new_status: &BTreeMap<K, CorrStatus>) -> bool {
        old.iter().all(|(key, value)| new.get(key) == Some(value))
            && new.iter().all(|(key, value)| old.contains_key(key)
                || prior.get(key) == Some(value) && checked.get(key) == Some(value)
                    && new_status.get(key) == Some(&CorrStatus::Pass))
            && old_status.keys().chain(new_status.keys()).all(|key|
                old_status.get(key) == new_status.get(key)
                    || !old.contains_key(key) && new.contains_key(key)
                        && new_status.get(key) == Some(&CorrStatus::Pass))
    }
    if !approval_additions(&before.corr_approved_fingerprints, &current.corr_approved_fingerprints,
        &before.live.corr_current_fingerprints, &checked.snapshot.corr_current_fingerprints,
        &before.corr_status, &current.corr_status)
        || !approval_additions(&before.paper_approved_fingerprints, &current.paper_approved_fingerprints,
            &before.live.paper_current_fingerprints, &checked.snapshot.paper_current_fingerprints,
            &before.paper_status, &current.paper_status) {
        return Err("Modeling verifier approval changes the checked meaning or revokes authority".into());
    }
    // This projection is shared with the actual paper fingerprint consumer.
    // New approvals are not generally irrelevant: nonempty preamble subsets
    // can change the observation even with identical source files.
    for target in old.current_paper_approved_fingerprints.keys().chain(new.current_paper_approved_fingerprints.keys()) {
        if crate::paper_fingerprints::approved_preamble_hashes(old.current_paper_approved_fingerprints.get(target))
            != crate::paper_fingerprints::approved_preamble_hashes(new.current_paper_approved_fingerprints.get(target)) {
            return Err("Modeling paper approval changes its observation operand".into());
        }
    }
    let mut request = old.clone();
    request.id = new.id; request.acceptance_logic_identity = new.acceptance_logic_identity.clone();
    request.current_paper_approved_fingerprints = new.current_paper_approved_fingerprints.clone();
    request.blocked_targets = new.blocked_targets.clone();
    request.previous_corr_lane_findings = new.previous_corr_lane_findings.clone();
    request.previous_paper_lane_findings = new.previous_paper_lane_findings.clone();
    request.review_verifier_evidence = new.review_verifier_evidence.clone();
    // Producer compatibility also covers this rendered copy of the exact
    // identity. Never normalize an arbitrary nested contract value.
    for original in [old, new] {
        if let Some(identity) = original.worker_contract.pointer("/request_summary/acceptance_logic_identity") {
            if identity.as_str() != Some(original.acceptance_logic_identity.as_str()) {
                return Err("Modeling contract producer identity is not its native projection".into());
            }
        }
    }
    if let Some(identity) = request.worker_contract.pointer_mut("/request_summary/acceptance_logic_identity") {
        *identity = serde_json::json!(new.acceptance_logic_identity);
    }
    // This contract field is a rendered copy of blocked_targets, not an
    // independent worker scope. Every other contract byte remains compared.
    for original in [old, new] {
        if let Some(pending) = original.worker_contract.pointer("/scope_contract/pending_targets") {
            if pending != &serde_json::json!(original.blocked_targets) {
                return Err("Modeling pending-target contract is not its native projection".into());
            }
        }
    }
    if let Some(pending) = request.worker_contract.pointer_mut("/scope_contract/pending_targets") {
        *pending = serde_json::json!(new.blocked_targets);
    }
    if request != *new { return Err("Modeling actual acceptance request changed".into()); }
    let mut projected = before.clone_without_dispatch();
    projected.in_flight_request = current.in_flight_request.clone();
    projected.request_seq = current.request_seq;
    macro_rules! verifier { ($($field:ident),+ $(,)?) => { $(projected.$field = current.$field.clone();)+ }; }
    verifier!(corr_approved_fingerprints, corr_status, paper_approved_fingerprints, paper_status,
        latest_corr_review_nodes, latest_corr_reviewer_evidence, latest_paper_review_targets,
        latest_paper_reviewer_evidence, previous_corr_lane_findings, previous_paper_lane_findings);
    if projected != current.clone_without_dispatch() {
        return Err("Modeling mathematical state, protection or authority changed".into());
    }
    let mut response = checked.clone();
    response.request_id = new.id;
    let outcome = apply_event(current.clone_without_dispatch(), ProtocolEvent::WrapperResponse {
        response: WrapperResponse::Worker(response.clone()),
    }).map_err(|error| format!("current Modeling transition rejected checked result: {error:?}"))?;
    if outcome.state.phase != current.phase || outcome.state.stage != Stage::Reviewer
        || outcome.commands.iter().any(|command| !matches!(command, ProtocolCommand::DeleteLocalClosureRecord { .. })) {
        return Err("Modeling checked result requires current rejection or global routing".into());
    }
    Ok(response)
}

fn compatible_positive_finalizer(old: &str, current: &str) -> bool {
    if old == current { return true; }
    if current != crate::model::acceptance_logic_identity() { return false; }
    let policy: serde_json::Value = serde_json::from_str(include_str!(
        "../../trellis/dispatch/compatible_staged_producers.json")).expect("checked staged policy");
    policy["version"] == 1 && policy["positive_math_finalizers"].as_array()
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(old)))
}

/// Failure reports add attempts, not mathematical progress. Permit only the
/// paired counter increments produced by record_coarse_attempt; retain every
/// current value when applying the checked response. Audit queue/launch
/// counters are scheduling, not completed advice. Selected/top-level plans,
/// all completion history, membership and status remain exact.
fn reconcile_report_counters(projected: &mut ProtocolState, current: &ProtocolState,
    writes: &BTreeSet<NodeId>) -> Result<(), String> {
    let selected = projected.coarse_task_goal.clone();
    if let (Some(old), Some(new)) = (&mut projected.coarse_goals, &current.coarse_goals) {
        for (node, goal) in &mut old.goals {
            let Some(now) = new.goals.get(node) else { continue };
            let delta = now.completed_attempts.checked_sub(goal.completed_attempts)
                .ok_or("coarse completed-attempt counter decreased")?;
            if now.attempts_since_progress != goal.attempts_since_progress.saturating_add(delta) {
                return Err("coarse progress changed beyond report bookkeeping".into());
            }
            goal.completed_attempts = now.completed_attempts;
            goal.attempts_since_progress = now.attempts_since_progress;
            goal.last_audit_attempt = now.last_audit_attempt;
            // A reviewer may dismiss an idle sibling's advice. It cannot
            // alter this worker's selected plan or a source-write goal.
            if selected.as_ref() != Some(node)
                && crate::coarse_goals::shallow_region(current, node).is_disjoint(writes) {
                goal.plan = now.plan.clone();
            }
        }
        old.pending_audits = new.pending_audits.clone();
    }
    Ok(())
}

/// The full ordinary gate may have checked helpers/import/deletion metadata.
/// Its authenticated result can use the same exact-source current transition
/// as staged finalization; this is never a scoped merge or producer upgrade.
pub fn rebase_ordinary_context(
    before: &ProtocolState, current: &ProtocolState, checked: &WorkerResponse,
    old_source: &SourceInputs, new_source: &SourceInputs, writes: &BTreeSet<NodeId>,
) -> Result<WorkerResponse, String> {
    let old = before.in_flight_request.as_deref().ok_or("original worker request is missing")?;
    let new = current.in_flight_request.as_deref().ok_or("current worker request is missing")?;
    if before.phase != Phase::ProofFormalization
        || old.acceptance_logic_identity != new.acceptance_logic_identity {
        return Err("ordinary context reuse requires the same PF checker implementation".into());
    }
    rebase_staged_finalization(before, current, checked, old_source, new_source, writes)
}

/// Rebind an authenticated completed staged finalization after report-only
/// drift. Unlike a semantic delta merge, all mathematical state and every
/// canonical source read remain exact; the checked structural delta survives
/// intact. The caller must authenticate the original staged completion.
pub fn rebase_staged_finalization(
    before: &ProtocolState, current: &ProtocolState, checked: &WorkerResponse,
    old_source: &SourceInputs, new_source: &SourceInputs, writes: &BTreeSet<NodeId>,
) -> Result<WorkerResponse, String> {
    let valid = checked.outcome == WorkerOutcome::Valid && checked.acceptance_check_passed
        && checked.deterministic_rejection_reasons.is_empty();
    let invalid = checked.outcome == WorkerOutcome::Invalid && !checked.acceptance_check_passed
        && !checked.deterministic_rejection_reasons.is_empty();
    if checked.status != ResponseStatus::Ok || checked.transport_failure || checked.preflight_error.is_some()
        || !(valid || invalid) {
        return Err("staged finalization rebase requires an authenticated Valid or Invalid result".into());
    }
    let old = before.in_flight_request.as_deref().ok_or("original worker request is missing")?;
    let new = current.in_flight_request.as_deref().ok_or("current worker request is missing")?;
    if old.id != checked.request_id || checked.cycle != before.cycle {
        return Err("checked response does not match its original worker request".into());
    }
    if before.phase == Phase::Modeling && valid
        && old.acceptance_logic_identity == new.acceptance_logic_identity {
        // Same complete current-source/authority/request comparison as the
        // ordinary Modeling gate. No old full verdict is used across changes
        // to approvals, structure, roles or other mathematical operands.
        return rebase_modeling_context(before, current, checked, old_source, new_source);
    }
    if before.phase != Phase::ProofFormalization ||
        (old.acceptance_logic_identity != new.acceptance_logic_identity
            && (!valid || old.is_pv || new.is_pv || !old.node_role.is_empty() || !new.node_role.is_empty()
                || !old.configured_challenge_targets.is_empty() || !new.configured_challenge_targets.is_empty()
                || !compatible_positive_finalizer(&old.acceptance_logic_identity, &new.acceptance_logic_identity))) {
        return Err("staged finalization phase or implementation changed".into());
    }
    let scope = RebaseFootprint { write_nodes: writes.clone(), effect_nodes: current.live.present_nodes.clone(),
        read_nodes: current.live.present_nodes.clone(), targets: current.configured_targets.clone() };
    // Only this positive-finalizer lane may substitute a reviewed producer.
    // Every actual acceptance operand is still compared by the shared gate.
    let mut authority_before = before.clone_without_dispatch();
    authority_before.in_flight_request.as_mut().unwrap().acceptance_logic_identity = new.acceptance_logic_identity.clone();
    same_authority(&authority_before, current, &scope)?;
    check_source_inputs(before, current, old_source, new_source, &scope.read_nodes)?;
    // Intentionally exhaustive: an unrecognized state change stays cold.
    // Normalize the listed delivery/report drift and one-way panel clear.
    // Stored mathematical authority stays exact; ordinary transition validation
    // consumes the current reviewer eligibility and other current values.
    let mut projected = before.clone_without_dispatch();
    projected.cycle = current.cycle;
    projected.request_seq = current.request_seq;
    projected.in_flight_request = current.in_flight_request.clone();
    projected.reviewer_comments = current.reviewer_comments.clone();
    projected.latest_worker_summary = current.latest_worker_summary.clone();
    projected.latest_worker_comments = current.latest_worker_comments.clone();
    // Later native routing may clear the previous substantiveness
    // panel without changing a verdict. Preserve current reviewer eligibility;
    // approvals, fingerprints, certificates and the fixed Worker envelope
    // remain exact. Nonempty panel changes are still cold.
    if current.latest_substantiveness_review_nodes.is_empty() {
        projected.latest_substantiveness_review_nodes.clear();
    }
    // A completed sibling can remain marked Active until the next ordinary
    // reviewer boundary. Recompute that derived bookkeeping on comparison
    // copies, never discard the goal registry or change actual authority.
    // Request archival advances this checkpoint locator without changing any
    // clean mathematical mirror. Keep those mirrors exact below. The current
    // transition whitelist excludes LastClean/HEAD restore commands; ordinary
    // Invalid restores its active-worker base, not this commit pointer.
    projected.last_clean_commit = current.last_clean_commit.clone();
    reconcile_report_counters(&mut projected, current, writes)?;
    projected.reconcile_coarse_goals();
    let mut comparable = current.clone_without_dispatch();
    comparable.reconcile_coarse_goals();
    if projected != comparable {
        return Err("staged finalization mathematical state or authority changed".into());
    }
    let mut response = checked.clone();
    response.request_id = new.id;
    response.cycle = current.cycle;
    let outcome = apply_event(current.clone_without_dispatch(), ProtocolEvent::WrapperResponse {
        response: WrapperResponse::Worker(response.clone()),
    }).map_err(|error| format!("current worker transition rejected staged finalization rebase: {error:?}"))?;
    if invalid {
        // Retain the checked failure verbatim, including partial candidate
        // diagnostics. Only the ordinary current failure/retry policy may run;
        // no candidate source, structural delta or certificate may be adopted.
        if outcome.state.live != current.live || outcome.state.phase != current.phase
            || outcome.state.proof_nodes != current.proof_nodes || outcome.state.node_kinds != current.node_kinds
            || outcome.state.deps != current.deps || outcome.state.target_claims != current.target_claims
            || outcome.state.local_closure_records != current.local_closure_records
            || outcome.state.local_closure_unverified_nodes != current.local_closure_unverified_nodes
            || outcome.state.local_closure_failures != current.local_closure_failures
            || outcome.commands.iter().any(|command| match command {
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase | ProtocolCommand::WriteHaltSentinel { .. } => false,
                ProtocolCommand::IssueRequest { request } => request.kind != RequestKind::Worker,
                _ => true,
            }) {
            return Err("staged failure rebase attempted mathematical progress or global routing".into());
        }
    } else if outcome.state.phase != current.phase || outcome.state.stage != Stage::Reviewer
        || outcome.commands.iter().any(|command| !matches!(command, ProtocolCommand::DeleteLocalClosureRecord { .. })) {
        return Err("staged finalization rebase requires current acceptance or global routing work".into());
    }
    Ok(response)
}

/// Honest Stuck reports carry no source/certificate delta. Reapply the current
/// failure policy while retaining the authenticated report and retry class.
pub fn rebase_checked_stuck(
    before: &ProtocolState, current: &ProtocolState, checked: &WorkerResponse,
    before_graph: &DependencyGraph, current_graph: &DependencyGraph, writes: &BTreeSet<NodeId>,
) -> Result<WorkerResponse, String> {
    if checked.status != ResponseStatus::Ok || checked.outcome != WorkerOutcome::Stuck
        || checked.acceptance_check_passed || checked.transport_failure || checked.preflight_error.is_some() {
        return Err("checked failure rebase requires an honest Stuck response".into());
    }
    if before.in_flight_request.as_ref().map(|request| request.id) != Some(checked.request_id)
        || checked.cycle != before.cycle {
        return Err("checked failure does not match its original worker request".into());
    }
    let scope = footprint(current, current_graph, writes)?;
    if footprint(before, before_graph, writes)? != scope { return Err("checked failure footprint changed".into()); }
    same_authority(before, current, &scope)?;
    let mut response = checked.clone();
    response.request_id = current.in_flight_request.as_ref().unwrap().id;
    response.cycle = current.cycle;
    response.snapshot = current.live.clone();
    response.proof_node_updates.clear(); response.node_kind_updates.clear();
    response.dep_updates.clear(); response.target_claim_updates.clear(); response.challenge_claim_updates.clear();
    response.local_closure_results.clear(); response.local_closure_revalidation = None;
    response.elaboration_costs.clear(); response.late_heartbeats.clear();
    let outcome = apply_event(current.clone_without_dispatch(), ProtocolEvent::WrapperResponse {
        response: WrapperResponse::Worker(response.clone()),
    }).map_err(|error| format!("current worker failure transition rejected checked response rebase: {error:?}"))?;
    if outcome.state.live != current.live || outcome.state.phase != current.phase
        || outcome.state.local_closure_records != current.local_closure_records
        || outcome.commands.iter().any(|command| match command {
            ProtocolCommand::RestoreWorktreeToActiveWorkerBase | ProtocolCommand::WriteHaltSentinel { .. } => false,
            ProtocolCommand::IssueRequest { request } => request.kind != RequestKind::Worker,
            _ => true,
        }) {
        return Err("checked failure rebase attempted source progress or global routing".into());
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    #[test]
    fn refusal_diagnostic_is_bounded_without_splitting_unicode() {
        let reason = "λ".repeat(1025);
        let row = super::refusal_diagnostic("ordinary_context", &reason);
        assert_eq!(row["reason"].as_str().unwrap(), "λ".repeat(1024));
        assert_eq!(row["truncated"], true);
        let short = super::refusal_diagnostic("scoped_response_checks", "source changed");
        assert_eq!(short["reason"], "source changed");
        assert_eq!(short["truncated"], false);
    }
    use super::*;

    fn graph() -> DependencyGraph {
        DependencyGraph { edges: BTreeMap::from([
            ("A".into(), BTreeSet::new()), ("B".into(), BTreeSet::new()),
            ("Join".into(), BTreeSet::from(["A".into(), "B".into()])),
        ]) }
    }

    fn fixture() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 3;
        state.request_seq = 7;
        state.live.present_nodes = BTreeSet::from(["A".into(), "B".into(), "Join".into()]);
        state.live.open_nodes = state.live.present_nodes.clone();
        state.proof_nodes = state.live.present_nodes.clone();
        state.deps = graph().edges;
        for (node, target) in [("A", "a"), ("B", "b"), ("Join", "join")] {
            state.configured_targets.insert(target.into());
            state.target_claims.insert(node.into(), BTreeSet::from([target.into()]));
            state.live.corr_current_fingerprints.insert(node.into(), format!("{node}-meaning"));
            state.live.target_fingerprints.insert(node.into(), format!("{node}-target"));
        }
        state.active_node = Some("A".into());
        state.proof_edit_mode = ProofEditMode::Local;
        state.pending_task = Some(PendingTask { node: Some("A".into()), mode: TaskMode::Local,
            authorized_nodes: BTreeSet::from(["A".into()]), ..Default::default() });
        let binding = crate::dispatch::ModelProfile {
            provider: "test".into(), model: "test".into(), reasoning_effort: "high".into(),
            timeout_seconds: 60, max_timeout_seconds: 60,
            allowed_tasks: BTreeSet::from([crate::dispatch::WorkerTaskKind::Ordinary]),
            cost_guidance: String::new(), lightweight: None,
        };
        state.worker_execution = Some(crate::dispatch::TaskExecutionContext {
            task_id: 1, profile: "test".into(), binding,
            compiler: crate::dispatch::CompilerCapability::Managed,
            task_kind: crate::dispatch::WorkerTaskKind::Ordinary,
        });
        state.normalize_all_structural_state();
        state.ensure_node_metadata();
        state.commit_live();
        state.in_flight_request = Some(Box::new(state.expected_request(7, RequestKind::Worker)));
        state
    }

    fn stating_fixture() -> (ProtocolState, ProtocolState, WorkerResponse, SourceInputs) {
        let mut before = fixture();
        before.phase = Phase::TheoremStating;
        before.target_edit_mode = TargetEditMode::Targeted;
        before.pending_task.as_mut().unwrap().mode = TaskMode::Targeted;
        before.latest_review_rejection_reasons = vec!["old reviewer presentation".into()];
        before.latest_sound_review_nodes.insert("B".into());
        before.latest_sound_reviewer_evidence.insert("B".into(), BTreeMap::from([
            ("lane".into(), Default::default())]));
        before.in_flight_request = Some(Box::new(before.expected_request(7, RequestKind::Worker)));
        let checked = WorkerResponse { request_id: 7, cycle: before.cycle,
            status: ResponseStatus::Ok, outcome: WorkerOutcome::Valid, acceptance_check_passed: true,
            snapshot: before.live.clone(),
            dep_updates: BTreeMap::from([("A".into(), Update::Set(BTreeSet::from(["B".into()])))]),
            ..Default::default() };
        let mut current = before.clone();
        current.cycle += 2; current.cycles_since_clean += 2; current.request_seq = 9;
        current.latest_review_rejection_reasons.clear();
        current.latest_sound_review_nodes.clear(); current.latest_sound_reviewer_evidence.clear();
        current.progress_history.push_snapshot(current.live.present_nodes.clone(), BTreeSet::new());
        current.in_flight_request = Some(Box::new(current.expected_request(9, RequestKind::Worker)));
        let source = SourceInputs { graph: graph(), input_hashes: BTreeMap::from([
            ("Tablet/A.lean".into(), "1".repeat(64)), ("GOAL.md".into(), "2".repeat(64))]),
            authority_inputs: BTreeMap::from([(".trellis/scripts/check.py".into(), "3".repeat(64))]) };
        (before, current, checked, source)
    }

    #[test]
    fn stating_history_accepts_only_exact_bounded_append_and_eviction() {
        use crate::progress_history::ProgressHistory;
        let history = |count| {
            let mut history = ProgressHistory::default();
            for index in 0..count {
                history.push_snapshot(BTreeSet::from([format!("node-{index}").into()]), BTreeSet::new());
            }
            history
        };
        for (count, appended) in [(0, 1), (5, 2), (63, 1), (64, 1), (64, 7), (80, 63)] {
            let before = history(count);
            let current = history(count + appended);
            assert!(stating_history_extends(&before, &current), "{count}+{appended}");
            assert!(stating_history_extends(&before, &before));
        }
        let before = history(80);
        for fault in ["overlap", "gap", "debounce", "rewind", "reset", "oversize",
            "divergent", "saturated", "no_overlap", "short"] {
            let mut current = history(81);
            match fault {
                "overlap" => { current.snapshots.front_mut().unwrap().present.insert("changed".into()); }
                "gap" => current.snapshots.front_mut().unwrap().snapshot_index += 1,
                "debounce" => current.last_dispatched_index = Some(79),
                "rewind" => current = history(79),
                "reset" => current.clear(),
                "oversize" => current.snapshots.push_front(before.snapshots.front().unwrap().clone()),
                "divergent" => { current = before.clone(); current.snapshots.back_mut().unwrap().progressed.insert("changed".into()); }
                "saturated" => current.next_index = u64::MAX,
                "no_overlap" => current = history(144),
                "short" => { current.snapshots.pop_front(); }
                _ => unreachable!(),
            }
            assert!(!stating_history_extends(&before, &current), "{fault}");
        }
        let mut malformed = before.clone();
        malformed.snapshots.back_mut().unwrap().snapshot_index -= 1;
        assert!(!stating_history_extends(&malformed, &history(81)));
    }

    #[test]
    fn stating_context_reuses_full_history_after_native_fifo_eviction() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (mut before, mut current, checked, source) = stating_fixture();
            before.progress_history.clear();
            for _ in 0..64 {
                before.progress_history.push_snapshot(before.live.present_nodes.clone(), BTreeSet::new());
            }
            current.progress_history = before.progress_history.clone();
            current.progress_history.push_snapshot(current.live.present_nodes.clone(), BTreeSet::new());
            let _no_compiler = crate::receipt_only::Guard::enter(("lean".into(), "lake".into(), "collector".into()));
            let response = rebase_stating_context(&before, &current, &checked, &source, &source).unwrap();
            assert_eq!(response.request_id, current.request_seq);
            assert_eq!(response.dep_updates, checked.dep_updates);
            current.progress_history.snapshots.front_mut().unwrap().progressed.insert("A".into());
            assert!(rebase_stating_context(&before, &current, &checked, &source, &source).is_err());
        }).unwrap().join().unwrap();
    }

    #[test]
    fn stating_context_reuses_structural_check_only_across_exact_delivery_drift() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (before, current, checked, source) = stating_fixture();
            let _no_compiler = crate::receipt_only::Guard::enter(("lean".into(), "lake".into(), "collector".into()));
            let response = rebase_stating_context(&before, &current, &checked, &source, &source).unwrap();
            let mut expected = checked.clone(); expected.request_id = 9; expected.cycle = current.cycle;
            assert_eq!(response, expected);
            for change in ["source", "authority", "graph", "approval", "scope", "contract", "fragments",
                "evidence", "rejection", "cycle_one", "rewind", "coordinates", "baseline", "history",
                "producer", "compiler", "phase", "failed", "response", "role", "challenge", "gate",
                "current_transition"] {
                let mut old = before.clone(); let mut now = current.clone();
                let mut inputs = source.clone(); let mut result = checked.clone();
                match change {
                    "source" => { inputs.input_hashes.insert("Tablet/A.lean".into(), "4".repeat(64)); }
                    "authority" => inputs.authority_inputs.clear(),
                    "graph" => inputs.graph.edges.clear(),
                    "approval" => { now.corr_status.insert("A".into(), CorrStatus::Pass); }
                    "scope" => { now.in_flight_request.as_mut().unwrap().worker_acceptance.authorized_nodes.insert("B".into()); }
                    "contract" => { now.in_flight_request.as_mut().unwrap().worker_contract["future_acceptance"] = serde_json::json!(true); }
                    "fragments" => { now.in_flight_request.as_mut().unwrap().worker_contract["prompt_fragments"] = serde_json::json!([]); }
                    "evidence" => { now.latest_sound_reviewer_evidence.insert("A".into(), BTreeMap::new()); }
                    "rejection" => now.latest_review_rejection_reasons.push("new refusal".into()),
                    "cycle_one" => old.cycle = 1,
                    "rewind" => now.cycles_since_clean = before.cycles_since_clean.saturating_sub(1),
                    "coordinates" => now.request_seq += 1,
                    "baseline" => now.in_flight_request.as_mut().unwrap().worker_closure_baseline.as_mut().unwrap().cycle += 1,
                    "history" => now.progress_history.last_dispatched_index = Some(99),
                    "producer" => now.in_flight_request.as_mut().unwrap().acceptance_logic_identity = "other".into(),
                    "compiler" => now.worker_execution.as_mut().unwrap().compiler = crate::dispatch::CompilerCapability::None,
                    "phase" => now.phase = Phase::ProofFormalization,
                    "failed" => result.acceptance_check_passed = false,
                    "response" => result.request_id += 1,
                    "role" => { now.in_flight_request.as_mut().unwrap().is_pv = true; }
                    "challenge" => { now.in_flight_request.as_mut().unwrap().configured_challenge_targets.insert("new".into(), Default::default()); }
                    "gate" => now.human_input_outstanding = true,
                    "current_transition" => { result.snapshot.present_nodes.clear(); }
                    _ => unreachable!(),
                }
                assert!(rebase_stating_context(&old, &now, &result, &source, &inputs).is_err(), "{change}");
            }
        }).unwrap().join().unwrap();
    }

    #[test]
    fn stating_context_uses_complete_native_pv_fragment_projection() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (mut before, mut current, checked, source) = stating_fixture();
            for state in [&mut before, &mut current] {
                state.conditional_coexistence_enabled = true;
                state.model_preservation_enabled = true;
                state.authored_goals_decidable = true;
                state.in_flight_request = Some(Box::new(state.expected_request(state.request_seq, RequestKind::Worker)));
            }
            let request = before.in_flight_request.as_ref().unwrap();
            assert_ne!(request.worker_contract["prompt_fragments"],
                crate::request_contracts::worker_contract_payload(request, None)["prompt_fragments"]);
            let _no_compiler = crate::receipt_only::Guard::enter(("lean".into(), "lake".into(), "collector".into()));
            let response = rebase_stating_context(&before, &current, &checked, &source, &source).unwrap();
            assert_eq!(response.dep_updates, checked.dep_updates);
        }).unwrap().join().unwrap();
    }

    #[test]
    fn stating_producer_category_is_separate_from_proof_and_modeling() {
        let producer = "trellis-worker-acceptance-v2:sha256:3c7491d4d80477ea42008e3574f4ff1b2b91bf480e08e536ec1c0c3fd47591da";
        let current = crate::model::acceptance_logic_identity();
        assert!(compatible_producer_for_lane(producer, &current, "theorem_stating"));
        assert!(!compatible_producer_for_lane(producer, &current, "proof"));
        assert!(!compatible_producer_for_lane(producer, &current, "modeling"));
        assert!(!compatible_producer_for_lane(producer, "unreviewed consumer", "theorem_stating"));
    }

    fn modeling_fixture() -> (ProtocolState, ProtocolState, WorkerResponse, SourceInputs) {
        let mut before = fixture();
        before.phase = Phase::Modeling;
        before.target_edit_mode = TargetEditMode::Global;
        before.pending_task = None;
        before.active_node = None;
        let paper = r#"{"target":"a","covering_nodes":{},"preamble_definition_hashes":[]}"#.to_string();
        before.live.paper_current_fingerprints.insert("a".into(), paper.clone());
        before.commit_live();
        before.in_flight_request = Some(Box::new(before.expected_request(7, RequestKind::Worker)));
        let checked = WorkerResponse { request_id: 7, cycle: before.cycle,
            status: ResponseStatus::Ok, outcome: WorkerOutcome::Valid, acceptance_check_passed: true,
            snapshot: before.live.clone(),
            // Normalized structural carriers are retained, not erased to pass
            // the old fixed-response gate (real task2 carries seventeen rows).
            challenge_claim_updates: BTreeMap::from([("A".into(), Update::Set(BTreeSet::new()))]),
            ..Default::default() };
        let mut current = before.clone();
        current.corr_approved_fingerprints.insert("A".into(), "A-meaning".into());
        current.corr_status.insert("A".into(), CorrStatus::Pass);
        current.paper_approved_fingerprints.insert("a".into(), paper);
        current.paper_status.insert("a".into(), CorrStatus::Pass);
        current.request_seq = 8;
        current.in_flight_request = Some(Box::new(current.expected_request(8, RequestKind::Worker)));
        let source = SourceInputs { graph: graph(), input_hashes: BTreeMap::from([
            ("Tablet/A.lean".into(), "1".repeat(64)), ("GOAL.md".into(), "2".repeat(64))]),
            authority_inputs: BTreeMap::from([(".trellis/scripts/check.py".into(), "3".repeat(64))]) };
        (before, current, checked, source)
    }

    #[test]
    fn modeling_context_reuses_scoped_check_with_current_bookkeeping() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (mut before, _, mut checked, source) = modeling_fixture();
            before.target_edit_mode = TargetEditMode::Restructure;
            before.active_node = Some("A".into());
            before.pending_task = Some(PendingTask { node: Some("A".into()), mode: TaskMode::Restructure,
                authorized_nodes: BTreeSet::from(["A".into()]), ..Default::default() });
            before.worker_execution.as_mut().unwrap().compiler = crate::dispatch::CompilerCapability::None;
            before.in_flight_request = Some(Box::new(before.expected_request(7, RequestKind::Worker)));
            checked.challenge_claim_updates.clear();
            let mut current = before.clone();
            current.cycle += 1;
            current.cycles_since_clean += 1;
            current.progress_history.push_snapshot(current.live.present_nodes.clone(), BTreeSet::new());
            current.in_flight_request = Some(Box::new(current.expected_request(8, RequestKind::Worker)));
            let _no_compiler = crate::receipt_only::Guard::enter(("lean".into(), "lake".into(), "collector".into()));
            let response = rebase_modeling_context(&before, &current, &checked, &source, &source).unwrap();
            assert_eq!(response.cycle, current.cycle);
            assert_eq!(response.request_id, 8);
            assert_eq!(response.snapshot, checked.snapshot);
            let staged = rebase_staged_finalization(&before, &current, &checked, &source, &source,
                &BTreeSet::from(["A".into()])).unwrap();
            assert_eq!(staged, response);
            for change in ["approval", "source", "authority", "scope", "baseline", "failed", "phase"] {
                let mut now = current.clone(); let mut inputs = source.clone(); let mut result = checked.clone();
                match change {
                    "approval" => { now.corr_status.insert("A".into(), CorrStatus::Pass); }
                    "source" => { inputs.input_hashes.insert("Tablet/A.lean".into(), "4".repeat(64)); }
                    "authority" => inputs.authority_inputs.clear(),
                    "scope" => { now.in_flight_request.as_mut().unwrap().worker_acceptance.authorized_nodes.insert("B".into()); }
                    "baseline" => { now.in_flight_request.as_mut().unwrap().worker_closure_baseline.as_mut().unwrap().cycle += 1; }
                    "failed" => result.acceptance_check_passed = false,
                    "phase" => now.phase = Phase::ProofFormalization,
                    _ => unreachable!(),
                }
                assert!(rebase_modeling_context(&before, &now, &result, &source, &inputs).is_err(), "{change}");
                assert!(rebase_staged_finalization(&before, &now, &result, &source, &inputs,
                    &BTreeSet::from(["A".into()])).is_err(), "{change}");
            }
        }).unwrap().join().unwrap();
    }

    #[test]
    fn modeling_verifier_delivery_reuses_exact_checked_structural_facts() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (before, current, checked, source) = modeling_fixture();
            let _no_compiler = crate::receipt_only::Guard::enter(("lean".into(), "lake".into(), "collector".into()));
            let rebased = rebase_modeling_verifier_delivery(&before, &current, &checked, &source, &source).unwrap();
            assert_eq!(rebased.request_id, 8);
            assert_eq!(rebased.snapshot, checked.snapshot);
            assert_eq!(rebased.challenge_claim_updates, checked.challenge_claim_updates);
            let outcome = apply_event(current.clone_without_dispatch(), ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(rebased), }).unwrap();
            assert_eq!(outcome.state.paper_approved_fingerprints, current.paper_approved_fingerprints);
            assert_eq!(outcome.state.corr_approved_fingerprints, current.corr_approved_fingerprints);
            assert_eq!(checked.request_id, 7, "donor evidence is immutable");
        }).unwrap().join().unwrap();
    }

    #[test]
    fn modeling_verifier_delivery_rejects_source_authority_or_changed_new_protection() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (before, current, checked, source) = modeling_fixture();
            let mut changed = source.clone(); changed.input_hashes.insert("GOAL.md".into(), "4".repeat(64));
            assert!(rebase_modeling_verifier_delivery(&before, &current, &checked, &source, &changed).unwrap_err().contains("sources"));
            let mut changed = current.clone(); changed.modeling_human_approved = !changed.modeling_human_approved;
            assert!(rebase_modeling_verifier_delivery(&before, &changed, &checked, &source, &source).is_err());
            let mut changed = checked.clone(); changed.snapshot.corr_current_fingerprints.insert("A".into(), "changed definition".into());
            assert!(rebase_modeling_verifier_delivery(&before, &current, &changed, &source, &source).unwrap_err().contains("meaning"));
            let mut changed = current.clone(); changed.in_flight_request.as_mut().unwrap().worker_acceptance.validation_execution_plan.push(WorkerValidationExecutionPlanStep::ApprovedMeaningPreserving {});
            assert!(rebase_modeling_verifier_delivery(&before, &changed, &checked, &source, &source).unwrap_err().contains("request"));
            let mut changed = checked.clone(); changed.authored_assumption_id = "new axiom".into();
            assert!(rebase_modeling_verifier_delivery(&before, &current, &changed, &source, &source).is_err());
        }).unwrap().join().unwrap();
    }

    #[test]
    fn modeling_verifier_delivery_preserves_helpers_and_rejects_current_orphans() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (before, current, mut checked, source) = modeling_fixture();
            checked.snapshot.present_nodes.insert("NewHelper".into());
            checked.snapshot.open_nodes.insert("NewHelper".into());
            checked.proof_node_updates.insert("NewHelper".into(), Update::Set(true));
            checked.node_kind_updates.insert("NewHelper".into(), Update::Set(NodeKind::Proof));
            checked.dep_updates.insert("NewHelper".into(), Update::Set(BTreeSet::new()));
            checked.target_claim_updates.insert("NewHelper".into(), Update::Set(BTreeSet::new()));
            checked.challenge_claim_updates.insert("NewHelper".into(), Update::Set(BTreeSet::new()));
            let mut consumers = before.deps[&NodeId::from("Join")].clone();
            consumers.insert("NewHelper".into());
            checked.dep_updates.insert("Join".into(), Update::Set(consumers));
            let response = rebase_modeling_verifier_delivery(&before, &current, &checked, &source, &source).unwrap();
            assert_eq!(response.dep_updates, checked.dep_updates);
            assert!(response.snapshot.present_nodes.contains(&NodeId::from("NewHelper")));
            // A saved Valid label never bypasses the current native structural
            // transition: this detached helper is a newly introduced orphan.
            checked.dep_updates.remove(&NodeId::from("Join"));
            assert!(rebase_modeling_verifier_delivery(&before, &current, &checked, &source, &source).is_err());
        }).unwrap().join().unwrap();
    }

    #[test]
    fn modeling_verifier_delivery_rejects_challenge_reassignment_and_targeted_scope() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (before, current, mut checked, source) = modeling_fixture();
            checked.challenge_claim_updates.insert("A".into(), Update::Set(BTreeSet::from(["challenge".into()])));
            assert!(rebase_modeling_verifier_delivery(&before, &current, &checked, &source, &source).unwrap_err().contains("challenge"));
            checked.challenge_claim_updates.clear();
            let mut targeted = before.clone();
            targeted.in_flight_request.as_mut().unwrap().worker_context.validation_kind = WorkerValidationKind::TheoremTargeted;
            assert!(rebase_modeling_verifier_delivery(&targeted, &current, &checked, &source, &source).unwrap_err().contains("exclusive"));
            let mut changed = source.clone(); changed.authority_inputs.clear();
            assert!(rebase_modeling_verifier_delivery(&before, &current, &checked, &source, &changed).unwrap_err().contains("authority"));
        }).unwrap().join().unwrap();
    }

    #[test]
    fn modeling_verifier_delivery_binds_rendered_reviewed_producer_identity() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (mut before, mut current, checked, source) = modeling_fixture();
            let producer = "trellis-worker-acceptance-v2:sha256:c1f83ea36e761ae6ad4c9139bced3bd75f63a63011b55b9010fc2a7e3be897a7";
            let consumer = crate::model::acceptance_logic_identity();
            for (state, identity) in [(&mut before, producer), (&mut current, consumer.as_str())] {
                let request = state.in_flight_request.as_mut().unwrap();
                request.acceptance_logic_identity = identity.into();
                request.worker_contract = serde_json::json!({"request_summary": {"acceptance_logic_identity": identity}});
            }
            rebase_modeling_verifier_delivery(&before, &current, &checked, &source, &source).unwrap();
            before.in_flight_request.as_mut().unwrap().worker_contract["request_summary"]["acceptance_logic_identity"] = serde_json::json!("unreviewed");
            assert!(rebase_modeling_verifier_delivery(&before, &current, &checked, &source, &source).unwrap_err().contains("native projection"));
        }).unwrap().join().unwrap();
    }

    #[test]
    fn modeling_verifier_delivery_binds_actual_paper_preamble_operand() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let (mut before, mut current, mut checked, source) = modeling_fixture();
            let paper = r#"{"target":"a","covering_nodes":{},"preamble_definition_hashes":["definition"]}"#.to_string();
            before.live.paper_current_fingerprints.insert("a".into(), paper.clone());
            before.commit_live(); current.live = before.live.clone(); current.committed = before.committed.clone();
            checked.snapshot = before.live.clone();
            current.paper_approved_fingerprints.insert("a".into(), paper.clone());
            current.in_flight_request.as_mut().unwrap().current_paper_approved_fingerprints.insert("a".into(), paper);
            assert!(rebase_modeling_verifier_delivery(&before, &current, &checked, &source, &source).unwrap_err().contains("observation operand"));
            let producer = "trellis-worker-acceptance-v2:sha256:c1f83ea36e761ae6ad4c9139bced3bd75f63a63011b55b9010fc2a7e3be897a7";
            assert!(!compatible_producer(producer, &crate::model::acceptance_logic_identity()));
            assert!(compatible_producer_for_lane(producer, &crate::model::acceptance_logic_identity(), "modeling"));
        }).unwrap().join().unwrap();
    }

    #[test]
    fn dispatch_rebase_shared_consumer_binds_all_inputs_of_derived_facts() {
        let state = fixture();
        let scope = footprint(&state, &graph(), &BTreeSet::from(["A".into()])).unwrap();
        assert_eq!(scope.read_nodes, BTreeSet::from(["A".into(), "B".into(), "Join".into()]));
        assert!(scope.effect_nodes.contains(&NodeId::from("Join")));
        assert!(scope.targets.contains(&TargetId::from("join")));
        let mut inputs = BTreeMap::new();
        for node in &state.live.present_nodes {
            inputs.insert(format!("Tablet/{node}.lean"), "1".repeat(64));
            inputs.insert(format!("Tablet/{node}.tex"), "absent".into());
        }
        let old = SourceInputs { graph: graph(), input_hashes: inputs, authority_inputs: BTreeMap::new() };
        let mut new = old.clone();
        // Join is unapproved, but a changed Join observation can depend on B.
        new.input_hashes.insert("Tablet/B.lean".into(), "2".repeat(64));
        assert!(check_source_inputs(&state, &state, &old, &new, &scope.read_nodes).is_err());
    }

    #[test]
    fn dispatch_rebase_independent_cones_can_share_immutable_inputs() {
        let mut state = fixture();
        state.live.present_nodes.insert("H".into());
        let graph = DependencyGraph { edges: BTreeMap::from([
            ("A".into(), BTreeSet::from(["H".into()])),
            ("B".into(), BTreeSet::from(["H".into()])),
            ("H".into(), BTreeSet::new()), ("Join".into(), BTreeSet::new()),
        ]) };
        state.deps = graph.edges.clone();
        let a = footprint(&state, &graph, &BTreeSet::from(["A".into()])).unwrap();
        let b = footprint(&state, &graph, &BTreeSet::from(["B".into()])).unwrap();
        assert_eq!(a.read_nodes, BTreeSet::from(["A".into(), "H".into()]));
        assert_eq!(b.read_nodes, BTreeSet::from(["B".into(), "H".into()]));
        assert!(a.read_nodes.is_disjoint(&b.write_nodes));
        assert!(b.read_nodes.is_disjoint(&a.write_nodes));
    }

    #[test]
    fn dispatch_rebase_independent_publication_preserves_both_completed_branches() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let mut before = fixture();
            // Separate target cones, with a common unchanged prerequisite.
            before.live.present_nodes.insert("H".into());
            before.proof_nodes.insert("H".into());
            let graph = DependencyGraph { edges: BTreeMap::from([
                ("A".into(), BTreeSet::from(["H".into()])),
                ("B".into(), BTreeSet::from(["H".into()])),
                ("H".into(), BTreeSet::new()), ("Join".into(), BTreeSet::new()),
            ]) };
            before.deps = graph.edges.clone();
            before.normalize_all_structural_state(); before.ensure_node_metadata(); before.commit_live();
            before.in_flight_request = Some(Box::new(before.expected_request(7, RequestKind::Worker)));
            let mut current = before.clone();
            current.live.open_nodes.remove(&NodeId::from("B"));
            current.live.sound_current_fingerprints.insert("B".into(), "accepted B evidence".into());
            current.commit_live();
            current.cycle += 1; current.request_seq += 1;
            current.in_flight_request = Some(Box::new(current.expected_request(current.request_seq, RequestKind::Worker)));
            let mut snapshot = before.live.clone();
            snapshot.open_nodes.remove(&NodeId::from("A"));
            let checked = WorkerResponse { request_id: 7, cycle: 3,
                status: ResponseStatus::Ok, outcome: WorkerOutcome::Valid,
                acceptance_check_passed: true, snapshot, ..Default::default() };
            let response = rebase_checked_response(&before, &current, &checked, &graph, &graph,
                &BTreeSet::from(["A".into()])).unwrap();
            assert!(!response.snapshot.open_nodes.contains(&NodeId::from("A")));
            assert!(!response.snapshot.open_nodes.contains(&NodeId::from("B")));
            assert_eq!(response.snapshot.sound_current_fingerprints[&NodeId::from("B")], "accepted B evidence");
        }).unwrap().join().unwrap();
    }

    #[test]
    fn dispatch_rebase_restores_only_native_derived_decide_coverage() {
        for polarity in [ChallengePolarity::Prove, ChallengePolarity::Disprove] {
            let (before, checked) = derived_coverage_fixture(polarity);
            let original = checked.clone();
            let mut response = checked;
            let mut current = before.clone();
            current.reviewer_comments = "later advisory presentation".into();
            reconcile_derived_challenge_coverage(&before, &current, &mut response).unwrap();
            assert_eq!(response.snapshot.challenge_coverage, before.live.challenge_coverage);
            response.snapshot.challenge_coverage = original.snapshot.challenge_coverage.clone();
            assert_eq!(response, original, "no checked field other than derived coverage changes");
            let mut exact = WorkerResponse { snapshot: before.live.clone(), ..original };
            let unchanged = exact.clone();
            reconcile_derived_challenge_coverage(&before, &current, &mut exact).unwrap();
            assert_eq!(exact, unchanged, "already complete native coverage is unchanged");
        }
    }

    fn derived_coverage_fixture(polarity: ChallengePolarity) -> (ProtocolState, WorkerResponse) {
        let mut before = fixture();
        let primary = ChallengeTargetId::from("authored:A");
        before.live.present_nodes.insert("A__Refutation".into());
        before.configured_challenge_targets.insert(primary.clone(), ChallengeTargetSpec {
            name: "A".into(), resolution: ChallengeResolution::Decide, ..Default::default()
        });
        before.configured_challenge_targets.insert("authored:A__refutation".into(), ChallengeTargetSpec {
            name: "A__Refutation".into(), ..Default::default()
        });
        before.authored_decide_goals.insert(primary.clone(), "tA".into());
        before.pv_live_polarity.insert(primary, polarity);
        before.live.challenge_coverage = before.challenge_coverage_from_claims_with_present(
            &before.challenge_claims, &before.live.present_nodes);
        before.in_flight_request = Some(Box::new(before.expected_request(7, RequestKind::Worker)));
        let mut checked = WorkerResponse { status: ResponseStatus::Ok, outcome: WorkerOutcome::Valid,
            acceptance_check_passed: true, snapshot: before.live.clone(), ..Default::default() };
        checked.snapshot.challenge_coverage = crate::worker_normalization::challenge_coverage_from_claims(
            &before.configured_challenge_targets.keys().cloned().collect(),
            &before.challenge_claims, &before.live.present_nodes);
        assert_ne!(checked.snapshot.challenge_coverage, before.live.challenge_coverage);
        (before, checked)
    }

    #[test]
    fn dispatch_rebase_derived_coverage_refuses_changed_authority_or_checked_projection() {
        let (before, checked) = derived_coverage_fixture(ChallengePolarity::Prove);
        for fault in ["claims", "registry", "authored", "polarity", "present", "current_coverage",
                      "request_claims", "request_registry", "checked_claim", "missing_row", "forged_row"] {
            let mut current = before.clone();
            let mut response = checked.clone();
            match fault {
                "claims" => { current.challenge_claims.insert("B".into(), BTreeSet::from(["authored:A".into()])); }
                "registry" => { current.configured_challenge_targets.get_mut(&ChallengeTargetId::from("authored:A")).unwrap().name = "B".into(); }
                "authored" => { current.authored_decide_goals.clear(); }
                "polarity" => { current.pv_live_polarity.insert("authored:A".into(), ChallengePolarity::Disprove); }
                "present" => { response.snapshot.present_nodes.remove(&NodeId::from("B")); }
                "current_coverage" => { current.live.challenge_coverage.clear(); }
                "request_claims" => { current.in_flight_request.as_mut().unwrap().current_challenge_claims.insert("B".into(), BTreeSet::from(["authored:A".into()])); }
                "request_registry" => { current.in_flight_request.as_mut().unwrap().configured_challenge_targets.clear(); }
                "checked_claim" => { response.challenge_claim_updates.insert("A".into(), Update::Set(BTreeSet::new())); }
                "missing_row" => { response.snapshot.challenge_coverage.remove(&ChallengeTargetId::from("authored:A")); }
                "forged_row" => { response.snapshot.challenge_coverage.insert("authored:A".into(), BTreeSet::from(["B".into()])); }
                _ => unreachable!(),
            }
            let unchanged = response.clone();
            assert!(reconcile_derived_challenge_coverage(&before, &current, &mut response).is_err(), "{fault}");
            assert_eq!(response, unchanged, "refusal retains the original checked response: {fault}");
        }
    }

    #[test]
    fn dispatch_rebase_derived_coverage_keeps_deviation_and_snapshot_guards() {
        let (before, mut checked) = derived_coverage_fixture(ChallengePolarity::Prove);
        reconcile_derived_challenge_coverage(&before, &before, &mut checked).unwrap();
        let scope = footprint(&before, &graph(), &BTreeSet::from(["A".into()])).unwrap();
        assert_eq!(rebase_snapshot(&before.live, &checked.snapshot, &before.live, &scope).unwrap(), before.live);
        checked.snapshot.deviation_current_fingerprints.insert("changed".into(), "not authenticated".into());
        assert!(rebase_snapshot(&before.live, &checked.snapshot, &before.live, &scope).is_err());
    }

    #[test]
    fn dispatch_rebase_typed_snapshot_preserves_siblings_and_rejects_conflicts() {
        let state = fixture();
        let scope = footprint(&state, &graph(), &BTreeSet::from(["A".into()])).unwrap();
        let mut checked = state.live.clone();
        checked.open_nodes.remove(&NodeId::from("A"));
        let mut current = state.live.clone();
        current.open_nodes.remove(&NodeId::from("B"));
        current.corr_current_fingerprints.insert("B".into(), "new sibling meaning".into());
        let rebased = rebase_snapshot(&state.live, &checked, &current, &scope).unwrap();
        assert!(!rebased.open_nodes.contains(&NodeId::from("A")));
        assert!(!rebased.open_nodes.contains(&NodeId::from("B")));
        assert_eq!(rebased.corr_current_fingerprints[&NodeId::from("B")], "new sibling meaning");
        checked.corr_current_fingerprints.insert("B".into(), "stale overwrite".into());
        assert!(rebase_snapshot(&state.live, &checked, &current, &scope).unwrap_err().contains("outside its effect"));
        checked = state.live.clone();
        checked.corr_current_fingerprints.insert("A".into(), "checked meaning".into());
        current.corr_current_fingerprints.insert("A".into(), "competing meaning".into());
        assert!(rebase_snapshot(&state.live, &checked, &current, &scope).unwrap_err().contains("conflicts"));
        checked = state.live.clone();
        checked.present_nodes.remove(&NodeId::from("B"));
        assert!(rebase_snapshot(&state.live, &checked, &current, &scope).unwrap_err().contains("membership"));
    }

    #[test]
    fn dispatch_rebase_shared_target_changes_compare_before() {
        let state = fixture();
        let scope = footprint(&state, &graph(), &BTreeSet::from(["A".into()])).unwrap();
        let mut checked = state.live.clone();
        checked.paper_current_fingerprints.insert("join".into(), "checked".into());
        let mut current = state.live.clone();
        current.paper_current_fingerprints.insert("join".into(), "other publication".into());
        assert!(rebase_snapshot(&state.live, &checked, &current, &scope).unwrap_err().contains("conflicts"));
        checked.paper_current_fingerprints = state.live.paper_current_fingerprints.clone();
        assert_eq!(rebase_snapshot(&state.live, &checked, &current, &scope).unwrap().paper_current_fingerprints,
            current.paper_current_fingerprints, "unchanged aggregate observations cannot overwrite current ones");
    }

    #[test]
    fn dispatch_rebase_admin_changes_do_not_erase_current_authority() {
        let before = fixture();
        let scope = footprint(&before, &graph(), &BTreeSet::from(["A".into()])).unwrap();
        let mut current = before.clone();
        current.cycle += 1;
        current.request_seq += 1;
        current.reviewer_comments = "another task's report".into();
        current.latest_worker_summary = "another task finished".into();
        current.latest_worker_comments = "retained current context".into();
        current.cycles_since_clean += 1;
        current.in_flight_request = Some(Box::new(current.expected_request(current.request_seq, RequestKind::Worker)));
        same_authority(&before, &current, &scope).unwrap();
        current.in_flight_request.as_mut().unwrap().worker_acceptance.authorized_nodes.insert("B".into());
        assert!(same_authority(&before, &current, &scope).unwrap_err().contains("worker_acceptance"));
        current = before.clone();
        current.in_flight_request.as_mut().unwrap().approved_corr_fingerprints.insert("A".into(), "different approval".into());
        assert!(same_authority(&before, &current, &scope).unwrap_err().contains("approval"));
        current = before.clone();
        current.live.corr_current_fingerprints.insert("A".into(), "different read".into());
        assert!(same_authority(&before, &current, &scope).unwrap_err().contains("semantic read"));
    }

    #[test]
    fn dispatch_rebase_reviewed_producers_keep_current_semantic_guards() {
        let current = fixture();
        let scope = footprint(&current, &graph(), &BTreeSet::from(["A".into()])).unwrap();
        for producer in [
            "trellis-worker-acceptance-v2:sha256:604f9c1ade349a11d5cf9b37d98192d5cf50dd0b00ebe1579808fbf5ee5af121",
            "trellis-worker-acceptance-v2:sha256:018ff429de9b6e8a2ef4f73a855dbe175bacc2e80335b0dae4ca43f252058611",
        ] {
            let mut before = current.clone();
            before.in_flight_request.as_mut().unwrap().acceptance_logic_identity = producer.into();
            same_authority(&before, &current, &scope).unwrap();
            let mut changed = current.clone();
            changed.in_flight_request.as_mut().unwrap().worker_acceptance.authorized_nodes.insert("B".into());
            assert!(same_authority(&before, &changed, &scope).unwrap_err().contains("worker_acceptance"));
            changed = current.clone();
            changed.live.corr_current_fingerprints.insert("A".into(), "changed".into());
            assert!(same_authority(&before, &changed, &scope).unwrap_err().contains("semantic read"));
            assert!(!compatible_producer(producer, "unreviewed consumer"));
        }
        assert!(!compatible_producer("unreviewed producer", &crate::model::acceptance_logic_identity()));
    }

    #[test]
    fn dispatch_rebase_rejects_unchecked_and_structural_responses() {
        let before = fixture();
        let mut response = WorkerResponse { status: ResponseStatus::Ok, outcome: WorkerOutcome::Valid,
            snapshot: before.live.clone(), ..Default::default() };
        assert!(fixed_response(&before, &response).is_err());
        response.acceptance_check_passed = true;
        response.dep_updates.insert("A".into(), Update::Set(BTreeSet::new()));
        fixed_response(&before, &response).unwrap();
        response.dep_updates.insert("A".into(), Update::Set(BTreeSet::from(["B".into()])));
        assert!(fixed_response(&before, &response).is_err());
        response.dep_updates.clear();
        response.authored_assumption_id = "new premise".into();
        assert!(fixed_response(&before, &response).is_err());
    }

    #[test]
    fn dispatch_rebase_runs_current_transition_and_drops_stale_shared_records() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let before = fixture();
            before.validate().unwrap();
            let mut current = before.clone();
            current.cycle += 1;
            current.request_seq += 1;
            current.latest_worker_summary = "a sibling's accepted work".into();
            current.local_closure_records.insert("Join".into(), LocalClosureRecord {
                node: "Join".into(), active_decl_hash: "new-current-record".into(), ..Default::default()
            });
            current.in_flight_request = Some(Box::new(current.expected_request(current.request_seq, RequestKind::Worker)));
            let checked = WorkerResponse { request_id: 7, cycle: 3,
                status: ResponseStatus::Ok, outcome: WorkerOutcome::Valid, acceptance_check_passed: true,
                snapshot: before.live.clone(), local_closure_revalidation: Some(RevalidationBatch {
                    refreshed: vec![("Join".into(), LocalClosureRecord {
                        node: "Join".into(), active_decl_hash: "old-check-record".into(), ..Default::default()
                    })], still_unverified: Vec::new(),
                }), ..Default::default() };
            let unchanged = current.clone();
            let response = rebase_checked_response(&before, &current, &checked, &graph(), &graph(),
                &BTreeSet::from(["A".into()])).unwrap();
            assert_eq!(response.request_id, current.request_seq);
            assert_eq!(response.cycle, current.cycle);
            let mut wrong_request = checked.clone();
            wrong_request.request_id += 1;
            assert!(rebase_checked_response(&before, &current, &wrong_request, &graph(), &graph(),
                &BTreeSet::from(["A".into()])).unwrap_err().contains("original worker request"));
            assert!(response.local_closure_revalidation.as_ref().unwrap().refreshed.is_empty());
            assert_eq!(current, unchanged, "pure preview never installs its speculative state");
            let mut revoked = current.clone();
            // A changed task contract is rejected by the current authority guard.
            revoked.in_flight_request.as_mut().unwrap().worker_context.must_close_active = true;
            assert!(rebase_checked_response(&before, &revoked, &checked, &graph(), &graph(),
                &BTreeSet::from(["A".into()])).is_err());
        }).unwrap().join().unwrap();
    }
    #[test]
    fn dispatch_rebase_stating_requires_private_closure_execution() {
        for phase in [Phase::TheoremStating, Phase::RevisionStating] {
            let mut state = fixture();
            state.phase = phase;
            state.in_flight_request = Some(Box::new(state.expected_request(7, RequestKind::Worker)));
            assert!(!supported_request(&state));
            state.worker_execution.as_mut().unwrap().task_kind = crate::dispatch::WorkerTaskKind::LeanClosure;
            state.in_flight_request = Some(Box::new(state.expected_request(7, RequestKind::Worker)));
            assert!(supported_request(&state));
            state.live.modeling_draft = true;
            assert!(!supported_request(&state));
        }
    }

    #[test]
    fn dispatch_rebase_actual_approved_guard_roots_are_reads() {
        let mut before = fixture();
        let request = before.in_flight_request.as_mut().unwrap();
        request.approved_target_nodes.insert("Join".into());
        request.approved_corr_fingerprints.insert("Join".into(), "approved".into());
        let scope = footprint(&before, &graph(), &BTreeSet::from(["A".into()])).unwrap();
        assert_eq!(scope.read_nodes, before.live.present_nodes);
        let mut current = before.clone();
        current.in_flight_request.as_mut().unwrap().approved_corr_fingerprints.insert("Join".into(), "new approval".into());
        assert!(same_authority(&before, &current, &scope).unwrap_err().contains("approval"));
    }

    #[test]
    fn dispatch_rebase_exact_source_binding_excludes_only_indexes_and_unread_siblings() {
        let state = fixture();
        let mut inputs = BTreeMap::from([("trellis.config.json".into(), "9".repeat(64)),
            ("Tablet/header.tex".into(), "8".repeat(64)),
            ("Tablet/INDEX.md".into(), "7".repeat(64)),
            ("Tablet/README.md".into(), "6".repeat(64)),
            (".trellis/scripts/check.py".into(), "5".repeat(64)),
            ("reference/paper.tex".into(), "4".repeat(64))]);
        for node in &state.live.present_nodes {
            inputs.insert(format!("Tablet/{node}.lean"), "1".repeat(64));
            inputs.insert(format!("Tablet/{node}.tex"), "absent".into());
        }
        let old = SourceInputs { graph: graph(), input_hashes: inputs, authority_inputs: BTreeMap::new() };
        let reads = BTreeSet::from(["A".into()]);
        for path in ["Tablet/INDEX.md", "Tablet/README.md", "Tablet/B.lean"] {
            let mut new = old.clone();
            new.input_hashes.insert(path.into(), "2".repeat(64));
            check_source_inputs(&state, &state, &old, &new, &reads).unwrap();
        }
        for path in ["Tablet/A.lean", "Tablet/A.tex", "Tablet/header.tex", "trellis.config.json",
            ".trellis/scripts/check.py", "reference/paper.tex", "Tablet/Unknown.lean"] {
            let mut new = old.clone();
            new.input_hashes.insert(path.into(), "2".repeat(64));
            assert!(check_source_inputs(&state, &state, &old, &new, &reads).is_err(), "{path}");
        }
        let mut missing = old.clone();
        missing.input_hashes.remove("Tablet/A.tex");
        assert!(check_source_inputs(&state, &state, &missing, &missing, &reads).is_err());
        let mut changed_graph = old.clone();
        changed_graph.graph.edges.get_mut(&NodeId::from("A")).unwrap().insert("B".into());
        assert!(check_source_inputs(&state, &state, &old, &changed_graph, &reads).is_err());
    }

    #[test]
    fn dispatch_rebase_host_authority_inputs_are_exact_and_separate() {
        let state = fixture();
        let reads = BTreeSet::from([NodeId::from("A")]);
        let legacy: SourceInputs = serde_json::from_value(serde_json::json!({
            "graph": graph(),
            "input_hashes": {"Tablet/A.lean": "1".repeat(64), "Tablet/A.tex": "absent"},
        })).unwrap();
        assert!(legacy.authority_inputs.is_empty());
        check_source_inputs(&state, &state, &legacy, &legacy, &reads).unwrap();
        let mut old = legacy.clone();
        old.authority_inputs = BTreeMap::from([
            (".trellis/scripts/check.py".into(), "2".repeat(64)),
            (".trellis/artifacts/policy.json".into(), "3".repeat(64)),
        ]);
        assert!(!old.input_hashes.contains_key(".trellis/scripts/check.py"));
        check_source_inputs(&state, &state, &old, &old, &reads).unwrap();
        for (path, replacement) in [
            (".trellis/scripts/check.py", Some("4".repeat(64))),
            (".trellis/artifacts/policy.json", None),
            (".trellis/artifacts/new-policy.json", Some("5".repeat(64))),
        ] {
            let mut new = old.clone();
            match replacement {
                Some(hash) => { new.authority_inputs.insert(path.into(), hash); }
                None => { new.authority_inputs.remove(path); }
            }
            assert!(check_source_inputs(&state, &state, &old, &new, &reads)
                .unwrap_err().contains("host authority inputs"));
        }
        assert!(check_source_inputs(&state, &state, &legacy, &old, &reads).is_err());
        assert!(check_source_inputs(&state, &state, &old, &legacy, &reads).is_err());
    }

    #[test]
    fn reviewed_positive_finalizer_is_not_generic_or_negative_compatibility() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let mut before = fixture();
            let mut current = before.clone();
            let producer = "trellis-worker-acceptance-v2:sha256:8871145c937e3236d72ba2d7f584f94b1ee24de2de3e15262a528a76d03f3cea";
            let consumer = crate::model::acceptance_logic_identity();
            before.in_flight_request.as_mut().unwrap().acceptance_logic_identity = producer.into();
            current.in_flight_request.as_mut().unwrap().acceptance_logic_identity = consumer.clone();
            let sources = staged_admin_sources(&before);
            let checked = staged_admin_checked(&before);
            let writes = BTreeSet::from([NodeId::from("A")]);
            assert!(compatible_positive_finalizer(producer, &consumer));
            assert!(!compatible_producer(producer, &consumer));
            rebase_staged_finalization(&before, &current, &checked, &sources, &sources, &writes).unwrap();
            let mut negative = checked.clone(); negative.outcome = WorkerOutcome::Invalid;
            negative.acceptance_check_passed = false; negative.deterministic_rejection_reasons = vec!["bad proof".into()];
            assert!(rebase_staged_finalization(&before, &current, &negative, &sources, &sources, &writes).is_err());
            let mut changed = sources.clone(); changed.input_hashes.insert("Tablet/B.lean".into(), "different imports".into());
            assert!(rebase_staged_finalization(&before, &current, &checked, &sources, &changed, &writes).is_err());
            before.in_flight_request.as_mut().unwrap().acceptance_logic_identity = "unreviewed".into();
            assert!(rebase_staged_finalization(&before, &current, &checked, &sources, &sources, &writes).is_err());
        }).unwrap().join().unwrap();
    }

    #[test]
    fn dispatch_rebase_stuck_keeps_report_without_source_or_proof_adoption() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let before = fixture();
            let mut current = before.clone();
            current.cycle += 1;
            current.request_seq += 1;
            current.latest_worker_comments = "current sibling report".into();
            current.in_flight_request = Some(Box::new(current.expected_request(current.request_seq, RequestKind::Worker)));
            let checked = WorkerResponse { request_id: 7, cycle: 3,
                status: ResponseStatus::Ok, outcome: WorkerOutcome::Stuck,
                summary: "The attempt could not finish".into(),
                snapshot: WorkingSnapshot::default(),
                local_closure_results: BTreeMap::from([("A".into(), LocalClosureProbeOutput::default())]),
                ..Default::default() };
            let response = rebase_checked_stuck(&before, &current, &checked, &graph(), &graph(),
                &BTreeSet::from(["A".into()])).unwrap();
            assert_eq!(response.snapshot, current.live);
            assert_eq!(response.summary, checked.summary);
            assert_eq!(response.outcome, WorkerOutcome::Stuck);
            assert!(!response.acceptance_check_passed);
            assert!(response.local_closure_results.is_empty());
        }).unwrap().join().unwrap();
    }

    fn staged_admin_sources(state: &ProtocolState) -> SourceInputs {
        let mut inputs = BTreeMap::from([("trellis.config.json".into(), "9".repeat(64))]);
        for node in &state.live.present_nodes {
            inputs.insert(format!("Tablet/{node}.lean"), "1".repeat(64));
            inputs.insert(format!("Tablet/{node}.tex"), "absent".into());
        }
        SourceInputs { graph: graph(), input_hashes: inputs,
            authority_inputs: BTreeMap::from([(".trellis/scripts/check.py".into(), "2".repeat(64))]) }
    }

    fn staged_admin_checked(before: &ProtocolState) -> WorkerResponse {
        WorkerResponse { request_id: before.in_flight_request.as_ref().unwrap().id, cycle: before.cycle,
            status: ResponseStatus::Ok, outcome: WorkerOutcome::Valid, acceptance_check_passed: true,
            snapshot: before.live.clone(), ..Default::default() }
    }

    #[test]
    fn staged_finalization_admin_rebase_preserves_helper_and_import_delta() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let mut before = fixture();
            // cf08: a previously published sibling is already mathematically
            // complete; only its derived coarse status catches up later.
            before.coarse_dag_nodes = BTreeSet::from(["A".into(), "B".into()]);
            before.active_coarse_node = Some("A".into());
            before.enable_coarse_goals(2).unwrap();
            before.select_coarse_goal(&NodeId::from("B")).unwrap();
            before.live.open_nodes.remove(&NodeId::from("B"));
            before.corr_status.insert("B".into(), CorrStatus::Pass);
            before.corr_approved_fingerprints.insert("B".into(), before.live.corr_current_fingerprints[&NodeId::from("B")].clone());
            before.live.substantiveness_current_fingerprints.insert("B".into(), "sub-B".into());
            before.substantiveness_status.insert("B".into(), SubstantivenessStatus::Pass);
            before.substantiveness_approved_fingerprints.insert("B".into(), "sub-B".into());
            before.live.sound_current_fingerprints.insert("B".into(), "sound-B".into());
            before.sound_status.insert("B".into(), SoundStatus::Pass);
            before.sound_approved_fingerprints.insert("B".into(), "sound-B".into());
            for target in &before.configured_targets {
                before.live.paper_current_fingerprints.insert(target.clone(), "paper".into());
                before.paper_approved_fingerprints.insert(target.clone(), "paper".into());
                before.paper_status.insert(target.clone(), CorrStatus::Pass);
            }
            before.commit_live();
            before.reconcile_coarse_goals();
            assert_eq!(before.coarse_goals.as_ref().unwrap().goals[&NodeId::from("B")].status,
                crate::coarse_goals::GoalStatus::Complete);
            before.coarse_goals.as_mut().unwrap().goals.get_mut(&NodeId::from("B")).unwrap().status = crate::coarse_goals::GoalStatus::Active;
            before.in_flight_request = Some(Box::new(before.expected_request(before.request_seq, RequestKind::Worker)));
            let sources = staged_admin_sources(&before);
            let mut current = before.clone();
            current.cycle += 1; current.request_seq += 1;
            current.reviewer_comments = "current review prose".into();
            current.latest_worker_summary = "an unrelated task stopped".into();
            current.latest_worker_comments = "current report".into();
            current.reconcile_coarse_goals();
            assert_eq!(current.coarse_goals.as_ref().unwrap().goals[&NodeId::from("B")].status,
                crate::coarse_goals::GoalStatus::Complete);
            current.in_flight_request = Some(Box::new(current.expected_request(current.request_seq, RequestKind::Worker)));
            for (helper, rejected) in [(false, false), (true, false), (true, true)] {
                let mut checked = staged_admin_checked(&before);
                if rejected {
                    checked.outcome = WorkerOutcome::Invalid;
                    checked.acceptance_check_passed = false;
                    checked.deterministic_rejection_reasons = vec!["Root: rfl failed after D changed".into()];
                }
                let dependency = if helper { NodeId::from("helperH") } else { NodeId::from("B") };
                checked.dep_updates.insert("A".into(), Update::Set(BTreeSet::from([dependency.clone()])));
                if helper {
                    checked.snapshot.present_nodes.insert(dependency.clone());
                    checked.node_kind_updates.insert(dependency.clone(), Update::Set(NodeKind::Definition));
                    checked.proof_node_updates.insert(dependency.clone(), Update::Set(false));
                    checked.dep_updates.insert(dependency.clone(), Update::Set(BTreeSet::new()));
                    checked.target_claim_updates.insert(dependency, Update::Set(BTreeSet::new()));
                }
                let saved = current.clone();
                let response = rebase_staged_finalization(&before, &current, &checked, &sources, &sources,
                    &BTreeSet::from(["A".into()])).unwrap();
                let mut expected = checked;
                expected.request_id = current.request_seq; expected.cycle = current.cycle;
                assert_eq!(response, expected, "only current delivery coordinates may change");
                assert_eq!(current, saved, "transition preview must not mutate canonical state");
            }
        }).unwrap().join().unwrap();
    }

    #[test]
    fn staged_finalization_rebase_allows_only_cleared_substantiveness_panel() {
        let mut before = fixture();
        before.latest_substantiveness_review_nodes = BTreeSet::from(["A".into(), "B".into()]);
        let sources = staged_admin_sources(&before);
        let writes = BTreeSet::from(["A".into()]);
        let mut current = before.clone();
        current.latest_substantiveness_review_nodes.clear();
        current.cycle += 2;
        current.request_seq += 4;
        current.latest_worker_summary = "Unchanged sibling Stuck".into();
        current.latest_worker_comments = "No source files changed".into();
        current.in_flight_request = Some(Box::new(current.expected_request(current.request_seq, RequestKind::Worker)));
        for valid in [true, false] {
            let mut checked = staged_admin_checked(&before);
            if !valid {
                checked.outcome = WorkerOutcome::Invalid;
                checked.acceptance_check_passed = false;
                checked.deterministic_rejection_reasons = vec!["original checked failure".into()];
            }
            let saved = current.clone();
            let rebased = rebase_staged_finalization(&before, &current, &checked, &sources, &sources, &writes).unwrap();
            let mut expected = checked.clone();
            expected.request_id = current.request_seq; expected.cycle = current.cycle;
            assert_eq!(rebased, expected);
            assert_eq!(current, saved, "the current panel must not be restored from the old state");
            for change in ["nonempty_panel", "verdict", "approval", "fingerprint", "certificate"] {
                let mut changed = current.clone();
                match change {
                    "nonempty_panel" => { changed.latest_substantiveness_review_nodes.insert("B".into()); }
                    "verdict" => { changed.substantiveness_status.insert("B".into(), SubstantivenessStatus::Fail); }
                    "approval" => { changed.substantiveness_approved_fingerprints.insert("B".into(), "changed".into()); }
                    "fingerprint" => { changed.live.substantiveness_current_fingerprints.insert("B".into(), "changed".into()); }
                    "certificate" => { changed.local_closure_records.insert("B".into(), LocalClosureRecord::default()); }
                    _ => unreachable!(),
                }
                assert!(rebase_staged_finalization(&before, &changed, &checked, &sources, &sources, &writes).is_err(), "{change}");
            }
        }
    }

    #[test]
    fn staged_finalization_admin_rebase_keeps_all_semantic_operands_exact() {
        let before = fixture();
        let source = staged_admin_sources(&before);
        let checked = staged_admin_checked(&before);
        let writes = BTreeSet::from(["A".into()]);
        for change in ["live", "status", "approval", "certificate", "difficulty", "scope", "counter"] {
            let mut current = before.clone();
            match change {
                "live" => { current.live.open_nodes.remove(&NodeId::from("B")); }
                "status" => { current.corr_status.insert("B".into(), CorrStatus::Fail); }
                "approval" => { current.corr_approved_fingerprints.insert("B".into(), "changed".into()); }
                "certificate" => { current.local_closure_records.insert("B".into(), LocalClosureRecord::default()); }
                "difficulty" => { current.easy_max_retries += 1; }
                "scope" => { current.in_flight_request.as_mut().unwrap().worker_context.authorized_nodes.insert("B".into()); }
                "counter" => { current.consecutive_transport_failure_count += 1; }
                _ => unreachable!(),
            }
            assert!(rebase_staged_finalization(&before, &current, &checked, &source, &source, &writes).is_err(), "{change}");
        }
        for path in ["Tablet/B.lean", "Tablet/Join.tex", "trellis.config.json", "reference/new.tex"] {
            let mut changed = source.clone(); changed.input_hashes.insert(path.into(), "3".repeat(64));
            assert!(rebase_staged_finalization(&before, &before, &checked, &source, &changed, &writes).is_err(), "{path}");
        }
        let mut authority = source.clone(); authority.authority_inputs.clear();
        assert!(rebase_staged_finalization(&before, &before, &checked, &source, &authority, &writes).is_err());
        let mut unchecked = checked.clone(); unchecked.acceptance_check_passed = false;
        assert!(rebase_staged_finalization(&before, &before, &unchecked, &source, &source, &writes).is_err());
        let mut stale = checked; stale.request_id += 1;
        assert!(rebase_staged_finalization(&before, &before, &stale, &source, &source, &writes).is_err());
    }

    #[test]
    fn staged_finalization_negative_rebase_keeps_failure_and_exact_freshness() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let before = fixture();
            let source = staged_admin_sources(&before);
            let writes = BTreeSet::from([NodeId::from("A")]);
            let mut current = before.clone();
            current.request_seq += 1;
            current.latest_worker_summary = "unrelated report".into();
            current.in_flight_request = Some(Box::new(current.expected_request(current.request_seq, RequestKind::Worker)));
            let mut checked = staged_admin_checked(&before);
            checked.outcome = WorkerOutcome::Invalid;
            checked.acceptance_check_passed = false;
            checked.deterministic_rejection_reasons = vec!["Root: tactic rfl failed".into()];
            checked.snapshot.open_nodes.remove(&NodeId::from("A"));
            checked.local_closure_results.insert("A".into(), LocalClosureProbeOutput::default());
            let response = rebase_staged_finalization(&before, &current, &checked, &source, &source, &writes).unwrap();
            let mut expected = checked.clone(); expected.request_id = current.request_seq;
            assert_eq!(response, expected, "do not erase diagnostics or manufacture a successful result");
            let outcome = apply_event(current.clone_without_dispatch(), ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(response),
            }).unwrap();
            assert_eq!(outcome.state.live, current.live);
            assert_eq!(outcome.state.local_closure_records, current.local_closure_records);
            let mut changed = source.clone(); changed.input_hashes.insert("Tablet/B.lean".into(), "4".repeat(64));
            assert!(rebase_staged_finalization(&before, &current, &checked, &source, &changed, &writes).is_err());
            let mut unapproved = checked.clone(); unapproved.acceptance_check_passed = true;
            assert!(rebase_staged_finalization(&before, &current, &unapproved, &source, &source, &writes).is_err());
            unapproved = checked.clone(); unapproved.deterministic_rejection_reasons.clear();
            assert!(rebase_staged_finalization(&before, &current, &unapproved, &source, &source, &writes).is_err());
            let mut policy = current.clone(); policy.easy_max_retries += 1;
            assert!(rebase_staged_finalization(&before, &policy, &checked, &source, &source, &writes).is_err());
        }).unwrap().join().unwrap();
    }

    #[test]
    fn staged_finalization_admin_rebase_runs_current_transition() {
        std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
            let before = fixture();
            let source = staged_admin_sources(&before);
            let mut checked = staged_admin_checked(&before);
            // A newly introduced orphan is rejected by the ordinary engine,
            // even with the host completion bit and exact administrative base.
            checked.snapshot.present_nodes.insert("orphan".into());
            checked.node_kind_updates.insert("orphan".into(), Update::Set(NodeKind::Definition));
            checked.proof_node_updates.insert("orphan".into(), Update::Set(false));
            checked.target_claim_updates.insert("orphan".into(), Update::Set(BTreeSet::new()));
            let result = rebase_staged_finalization(&before, &before, &checked, &source, &source,
                &BTreeSet::from(["A".into()]));
            assert!(result.is_err(), "an authoritative flag cannot replace current native legality");
        }).unwrap().join().unwrap();
    }

}

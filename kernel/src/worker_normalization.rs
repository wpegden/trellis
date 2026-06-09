use crate::model::{
    DeviationId, DeviationRequest, Fingerprint, NodeBoolUpdates, NodeDifficulty, NodeId, NodeKind,
    NodeKindUpdates, NodeSetUpdates, TargetClaimUpdates, TargetId, Update, WorkerOutcome,
    WorkerProofDeltaMode, WorkerResponse, WorkerValidationExecutionPlanStep, WorkingSnapshot,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const AXIOMS_NAME: &str = "Axioms";
const HEADER_NAME: &str = "header";
const PREAMBLE_NAME: &str = "Preamble";
const PROOF_BEARING_ENVS: &[&str] = &["theorem", "lemma", "corollary", "helper"];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerNormalizationInput {
    pub repo_path: PathBuf,
    pub configured_targets: BTreeSet<TargetId>,
    pub current_present_nodes: BTreeSet<NodeId>,
    pub current_proof_nodes: BTreeSet<NodeId>,
    pub current_node_kinds: BTreeMap<NodeId, NodeKind>,
    pub current_deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    pub current_target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    pub approved_paper_fingerprints: BTreeMap<TargetId, Fingerprint>,
    pub target_claim_updates: BTreeMap<NodeId, BTreeSet<TargetId>>,
    pub target_fingerprints: BTreeMap<NodeId, Fingerprint>,
    pub sound_current_fingerprints: BTreeMap<NodeId, Fingerprint>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerNormalizationOutput {
    pub snapshot: WorkingSnapshot,
    pub proof_node_updates: NodeBoolUpdates,
    pub node_kind_updates: NodeKindUpdates,
    pub dep_updates: NodeSetUpdates,
    pub target_claim_updates: TargetClaimUpdates,
    pub contract_errors: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerAcceptanceInput {
    pub request_id: u32,
    pub cycle: u32,
    pub payload_outcome: WorkerOutcome,
    pub difficulty_updates: BTreeMap<NodeId, Update<NodeDifficulty>>,
    #[serde(default)]
    pub deviation_requests: BTreeMap<DeviationId, DeviationRequest>,
    #[serde(default)]
    pub node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>>,
    #[serde(default)]
    pub deviation_deletions: BTreeSet<DeviationId>,
    /// Current kernel `node_deviation_claims` at the moment the worker
    /// burst was issued. Used by the deletion contract check to verify
    /// that, after the response's claim updates are notionally applied,
    /// no node still claims a to-delete id.
    #[serde(default)]
    pub current_node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>>,
    /// Current kernel `deviation_files` (id -> path) at the moment the
    /// worker burst was issued. Used by the unknown-claim-id contract
    /// check and the deletion-file-hygiene contract check.
    #[serde(default)]
    pub current_deviation_files: BTreeMap<DeviationId, String>,
    pub before_snapshot: BTreeMap<String, String>,
    pub forbid_tablet_changes_when_stuck: bool,
    pub normalization: WorkerNormalizationInput,
    pub validation_execution_plan: Vec<WorkerValidationExecutionPlanStep>,
    pub validation_step_results: Vec<WorkerValidationStepResult>,
    pub protected_semantic_change_nodes: BTreeSet<NodeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HydrateWorkerResponseInput {
    pub repo_path: PathBuf,
    pub configured_targets: BTreeSet<TargetId>,
    pub current_target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    #[serde(default)]
    pub current_deviation_files: BTreeMap<DeviationId, String>,
    #[serde(default)]
    pub current_node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>>,
    pub approved_paper_fingerprints: BTreeMap<TargetId, Fingerprint>,
    /// Path to the configured paper file (relative to `repo_path` or
    /// absolute), used to compute the substantiveness
    /// fingerprint's `paper_source_sha` field. Optional: empty value
    /// produces empty `paper_source_sha`, leaving the per-node lane
    /// effectively dormant for that delta — appropriate for legacy
    /// configs with no paper file.
    #[serde(default)]
    pub paper_source_path: Option<PathBuf>,
    /// Node kinds at the time of the response (typically the kernel-known
    /// kinds, since worker structural updates land after this hydrator
    /// runs). Used to populate the `node_kind` field in the per-node
    /// paper fingerprint.
    #[serde(default)]
    pub current_node_kinds: BTreeMap<NodeId, crate::model::NodeKind>,
    pub response: WorkerResponse,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerAcceptanceOutput {
    pub response: WorkerResponse,
    pub contract_errors: Vec<String>,
    pub validation_errors: Vec<String>,
    pub final_outcome: WorkerOutcome,
    pub ok: bool,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerValidationStepResult {
    pub kind: String,
    pub ok: bool,
    pub detail: String,
    pub errors: Vec<String>,
    pub build_output: String,
    pub allowed_nodes: BTreeSet<NodeId>,
    /// Patch B+: per-node local-closure probe outputs produced by the
    /// `must_close_active` gate (Patch B) or by other accepts that
    /// transition a node sorryd→sorry-free (Patch C). Carries through
    /// to `WorkerResponse.local_closure_results` so downstream patches
    /// can persist records or surface diagnostics.
    ///
    /// Patch C-Q Q8 (doc refresh): post-C-O the map can carry MORE
    /// than one entry. The MCA gate populates the active node; the
    /// cleanup-burst pipeline can additionally include probe results
    /// for any sorryd→sorry-free transitions that fell out of the
    /// cleanup edit (handled by the engine's `apply_local_closure_acceptance_bookkeeping`
    /// step (e) loop). The cleanup-burst pipeline also attaches a
    /// pre-built `RevalidationBatch` via
    /// `WorkerResponse.local_closure_revalidation` (separate channel,
    /// handled by step (f) via `apply_revalidation_batch`).
    #[serde(default)]
    pub local_closure_results: BTreeMap<NodeId, crate::model::LocalClosureProbeOutput>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerGateObservationInput {
    pub repo_path: PathBuf,
    pub current_present_nodes: BTreeSet<NodeId>,
    pub active_node: Option<NodeId>,
    pub observation_plan: crate::model::WorkerAcceptanceObservationPlan,
    pub collect_observations: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerGateObservationOutput {
    pub before_snapshot: BTreeMap<String, String>,
    pub before_tablet_contents: BTreeMap<String, String>,
    pub imports_before: Vec<String>,
    pub expected_active_hash: String,
    pub baseline_declaration_hashes: BTreeMap<NodeId, String>,
}

pub fn normalize_worker_response(
    input: &WorkerNormalizationInput,
) -> Result<WorkerNormalizationOutput, String> {
    let present_nodes = present_nodes_from_repo(&input.repo_path)?;
    let open_nodes = open_nodes_from_repo(&input.repo_path, &present_nodes);
    let node_kinds = node_kinds_from_repo(&input.repo_path, &present_nodes);
    let proof_nodes = proof_nodes_from_kinds(&node_kinds, &present_nodes);
    let deps = direct_deps_from_repo(&input.repo_path, &present_nodes);
    let dep_updates = diff_node_sets(&input.current_deps, &deps);
    let new_nodes: BTreeSet<_> = present_nodes
        .difference(&input.current_present_nodes)
        .cloned()
        .collect();
    let changed_dep_nodes: BTreeSet<_> = dep_updates.keys().cloned().collect();
    let target_claim_updates = normalize_target_claim_updates(
        &input.target_claim_updates,
        &input.current_target_claims,
        &present_nodes,
        &input.configured_targets,
        &new_nodes,
    );
    let target_claims = apply_target_claim_updates(
        &input.current_target_claims,
        &target_claim_updates,
        &present_nodes,
        &input.configured_targets,
    );
    let coverage = coverage_from_claims(&input.configured_targets, &target_claims, &present_nodes);
    let snapshot = WorkingSnapshot {
        present_nodes: present_nodes.clone(),
        open_nodes,
        coverage,
        corr_current_fingerprints: complete_fingerprint_map(
            &input.target_fingerprints,
            &present_nodes,
        ),
        target_fingerprints: complete_fingerprint_map(&input.target_fingerprints, &present_nodes),
        paper_current_fingerprints: BTreeMap::new(),
        sound_current_fingerprints: complete_fingerprint_map(
            &input.sound_current_fingerprints,
            &present_nodes,
        ),
        sound_current_fingerprint_parts: BTreeMap::new(),
        deviation_current_fingerprints: BTreeMap::new(),
        sketch_proof_nodes: BTreeSet::new(),
        // Worker normalization doesn't synthesise substantiveness
        // faithfulness fingerprints (the runtime CLI hydrates them after
        // the worker delta lands; see
        // `populate_response_fingerprints`). Empty here keeps the
        // snapshot shape valid; the runtime fills it in before the
        // kernel applies the response.
        substantiveness_current_fingerprints: BTreeMap::new(),
        // Same hydration pattern as substantiveness: the runtime CLI
        // (`populate_response_fingerprints` →
        // `observe_protected_closure_nodes`) populates this from the
        // cached `lean_semantic_payload` sidecars after worker
        // normalisation runs, so it can be left empty here.
        protected_closure_nodes_per_target: BTreeMap::new(),
    };
    let mut contract_errors = worker_contract_errors(
        &new_nodes,
        &changed_dep_nodes,
        &input.target_claim_updates,
        &target_claims,
    );
    contract_errors.extend(tablet_lean_layout_errors(&input.repo_path, &present_nodes));
    Ok(WorkerNormalizationOutput {
        snapshot,
        proof_node_updates: diff_proof_nodes(&input.current_proof_nodes, &proof_nodes),
        node_kind_updates: diff_node_kinds(&input.current_node_kinds, &node_kinds, &present_nodes),
        dep_updates,
        target_claim_updates,
        contract_errors,
    })
}

fn validation_step_kind_name(step: &WorkerValidationExecutionPlanStep) -> &'static str {
    match step {
        WorkerValidationExecutionPlanStep::TheoremTargetEditScope { .. } => {
            "theorem_target_edit_scope"
        }
        WorkerValidationExecutionPlanStep::ScopedTablet { .. } => "scoped_tablet",
        WorkerValidationExecutionPlanStep::ProofEasyScope { .. } => "proof_easy_scope",
        WorkerValidationExecutionPlanStep::ProofWorkerDelta { .. } => "proof_worker_delta",
        WorkerValidationExecutionPlanStep::CleanupPreserving {} => "cleanup_preserving",
        WorkerValidationExecutionPlanStep::FinalCleanupPreserving { .. } => {
            "final_cleanup_preserving"
        }
    }
}

fn validation_errors_from_step_results(
    expected_steps: &[WorkerValidationExecutionPlanStep],
    observed_steps: &[WorkerValidationStepResult],
) -> Vec<String> {
    let mut errors = Vec::new();
    if expected_steps.len() != observed_steps.len() {
        errors.push(format!(
            "worker validation returned {} step results for {} expected execution steps",
            observed_steps.len(),
            expected_steps.len()
        ));
    }
    for (idx, expected) in expected_steps.iter().enumerate() {
        let expected_kind = validation_step_kind_name(expected);
        let Some(observed) = observed_steps.get(idx) else {
            errors.push(format!(
                "worker validation is missing execution result for step {} ({expected_kind})",
                idx
            ));
            continue;
        };
        let observed_kind = observed.kind.trim().to_ascii_lowercase();
        if observed_kind != expected_kind {
            errors.push(format!(
                "worker validation step {} kind mismatch: expected {}, observed {}",
                idx,
                expected_kind,
                if observed_kind.is_empty() {
                    "<empty>"
                } else {
                    observed_kind.as_str()
                }
            ));
        }
        if !observed.ok || !observed.errors.is_empty() {
            if observed.errors.is_empty() {
                if observed.detail.trim().is_empty() {
                    errors.push(format!(
                        "worker validation step {} ({expected_kind}) failed without an error message",
                        idx
                    ));
                } else {
                    errors.push(observed.detail.trim().to_string());
                }
            } else {
                errors.extend(
                    observed
                        .errors
                        .iter()
                        .filter(|err| !err.trim().is_empty())
                        .cloned(),
                );
            }
        }
    }
    errors
}

fn has_cleanup_validation_step(steps: &[WorkerValidationExecutionPlanStep]) -> bool {
    steps.iter().any(|step| {
        matches!(
            step,
            WorkerValidationExecutionPlanStep::CleanupPreserving {}
        )
    })
}

fn has_final_cleanup_validation_step(steps: &[WorkerValidationExecutionPlanStep]) -> bool {
    steps.iter().any(|step| {
        matches!(
            step,
            WorkerValidationExecutionPlanStep::FinalCleanupPreserving { .. }
        )
    })
}

fn cleanup_dep_closure(
    seed: &BTreeSet<NodeId>,
    live_present: &BTreeSet<NodeId>,
    deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> BTreeSet<NodeId> {
    let mut closure: BTreeSet<NodeId> = seed
        .iter()
        .filter(|node| live_present.contains(*node))
        .cloned()
        .collect();
    let mut frontier: Vec<NodeId> = closure.iter().cloned().collect();
    while let Some(node) = frontier.pop() {
        for dep in deps.get(&node).into_iter().flatten() {
            if !live_present.contains(dep) {
                continue;
            }
            if closure.insert(dep.clone()) {
                frontier.push(dep.clone());
            }
        }
    }
    closure
}

fn cleanup_orphan_nodes(
    configured_targets: &BTreeSet<TargetId>,
    current_target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    current_present_nodes: &BTreeSet<NodeId>,
    current_deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> BTreeSet<NodeId> {
    let roots: BTreeSet<NodeId> = current_target_claims
        .iter()
        .filter(|(node, targets)| {
            current_present_nodes.contains(*node)
                && targets
                    .iter()
                    .any(|target| configured_targets.contains(target))
        })
        .map(|(node, _)| node.clone())
        .collect();
    let supported = cleanup_dep_closure(&roots, current_present_nodes, current_deps);
    current_present_nodes
        .iter()
        .filter(|node| node.as_str() != PREAMBLE_NAME && !supported.contains(*node))
        .cloned()
        .collect()
}

fn cleanup_set_delta_nodes(
    current_set: &BTreeSet<NodeId>,
    next_set: &BTreeSet<NodeId>,
) -> BTreeSet<NodeId> {
    current_set
        .difference(next_set)
        .chain(next_set.difference(current_set))
        .cloned()
        .collect()
}

fn cleanup_node_set_update_legal(
    node: &NodeId,
    update: &Update<BTreeSet<NodeId>>,
    current: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    removed_nodes: &BTreeSet<NodeId>,
    orphan_nodes: &BTreeSet<NodeId>,
) -> bool {
    match update {
        Update::Same => true,
        Update::Set(next) if removed_nodes.contains(node) => next.is_empty(),
        Update::Set(next) => {
            let current_set = current.get(node).cloned().unwrap_or_default();
            cleanup_set_delta_nodes(&current_set, next)
                .iter()
                .all(|dep| orphan_nodes.contains(dep))
        }
    }
}

fn cleanup_contract_errors(
    input: &WorkerAcceptanceInput,
    normalized: &WorkerNormalizationOutput,
) -> Vec<String> {
    if !has_cleanup_validation_step(&input.validation_execution_plan) {
        return Vec::new();
    }

    let orphan_nodes = cleanup_orphan_nodes(
        &input.normalization.configured_targets,
        &input.normalization.current_target_claims,
        &input.normalization.current_present_nodes,
        &input.normalization.current_deps,
    );
    let removed_nodes: BTreeSet<NodeId> = input
        .normalization
        .current_present_nodes
        .difference(&normalized.snapshot.present_nodes)
        .cloned()
        .collect();
    let added_nodes: BTreeSet<NodeId> = normalized
        .snapshot
        .present_nodes
        .difference(&input.normalization.current_present_nodes)
        .cloned()
        .collect();
    let mut errors = Vec::new();

    if matches!(
        input.payload_outcome,
        WorkerOutcome::Stuck | WorkerOutcome::NeedsRestructure
    ) {
        errors.push("cleanup worker outcome must be one of ['valid', 'invalid']".to_string());
    }
    if !added_nodes.is_empty() {
        errors.push(format!(
            "cleanup may not add nodes: {:?}",
            added_nodes.into_iter().collect::<Vec<_>>()
        ));
    }
    let illegal_removed: Vec<_> = removed_nodes.difference(&orphan_nodes).cloned().collect();
    if !illegal_removed.is_empty() {
        errors.push(format!(
            "cleanup may only delete current orphan nodes: {:?}",
            illegal_removed
        ));
    }
    if !input.difficulty_updates.is_empty() {
        errors.push("cleanup may not report difficulty_updates".to_string());
    }
    if !normalized.node_kind_updates.is_empty() {
        errors.push(format!(
            "cleanup may not change node kinds: {:?}",
            normalized
                .node_kind_updates
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        ));
    }
    let illegal_proof_updates: Vec<_> = normalized
        .proof_node_updates
        .iter()
        .filter_map(|(node, update)| {
            if removed_nodes.contains(node) && matches!(update, Update::Set(false)) {
                None
            } else {
                Some(node.clone())
            }
        })
        .collect();
    if !illegal_proof_updates.is_empty() {
        errors.push(format!(
            "cleanup may not change proof-node classification except when deleting orphan nodes: {:?}",
            illegal_proof_updates
        ));
    }
    let illegal_target_claim_updates: Vec<_> = normalized
        .target_claim_updates
        .iter()
        .filter_map(|(node, update)| match update {
            Update::Same => None,
            Update::Set(targets) if removed_nodes.contains(node) && targets.is_empty() => None,
            _ => Some(node.clone()),
        })
        .collect();
    if !illegal_target_claim_updates.is_empty() {
        errors.push(format!(
            "cleanup may not change paper target claims except to clear claims on deleted orphan nodes: {:?}",
            illegal_target_claim_updates
        ));
    }
    let current_coverage = coverage_from_claims(
        &input.normalization.configured_targets,
        &input.normalization.current_target_claims,
        &input.normalization.current_present_nodes,
    );
    if normalized.snapshot.coverage != current_coverage {
        errors.push("cleanup may not change paper-target coverage".to_string());
    }
    let illegal_dep_updates: Vec<_> = normalized
        .dep_updates
        .iter()
        .filter(|(node, update)| {
            !cleanup_node_set_update_legal(
                node,
                update,
                &input.normalization.current_deps,
                &removed_nodes,
                &orphan_nodes,
            )
        })
        .map(|(node, _)| node.clone())
        .collect();
    if !illegal_dep_updates.is_empty() {
        errors.push(format!(
            "cleanup may only change direct imports by deleting orphan nodes or adding/removing orphan-node imports: {:?}",
            illegal_dep_updates
        ));
    }
    // cleanup-mode semantic_dep_updates rule removed with the
    // protected_correspondence refactor: semantic_deps is no longer a
    // tracked protocol concept.
    let attached_orphan = normalized.dep_updates.iter().any(|(node, update)| {
        if removed_nodes.contains(node) {
            return false;
        }
        match update {
            Update::Same => false,
            Update::Set(next) => {
                let current_set = input
                    .normalization
                    .current_deps
                    .get(node)
                    .cloned()
                    .unwrap_or_default();
                cleanup_set_delta_nodes(&current_set, next)
                    .iter()
                    .any(|dep| orphan_nodes.contains(dep))
            }
        }
    });
    if removed_nodes.is_empty() && !attached_orphan {
        errors.push(
            "cleanup must remove at least one current orphan node or change imports to attach one"
                .to_string(),
        );
    }

    errors
}

fn final_cleanup_contract_errors(
    input: &WorkerAcceptanceInput,
    normalized: &WorkerNormalizationOutput,
) -> Vec<String> {
    if !has_final_cleanup_validation_step(&input.validation_execution_plan) {
        return Vec::new();
    }

    let mut errors = Vec::new();
    if matches!(
        input.payload_outcome,
        WorkerOutcome::Stuck | WorkerOutcome::NeedsRestructure
    ) {
        errors.push("final cleanup worker outcome must be one of ['valid', 'invalid']".to_string());
    }
    if !input.difficulty_updates.is_empty() {
        errors.push("final cleanup may not report difficulty_updates".to_string());
    }
    let illegal_target_claim_updates: Vec<_> = normalized
        .target_claim_updates
        .iter()
        .filter_map(|(node, update)| match update {
            Update::Same => None,
            Update::Set(_) => Some(node.clone()),
        })
        .collect();
    if !illegal_target_claim_updates.is_empty() {
        errors.push(format!(
            "final cleanup may not change paper target claims: {:?}",
            illegal_target_claim_updates
        ));
    }
    let illegal_node_kind_updates: Vec<_> = normalized
        .node_kind_updates
        .iter()
        .filter_map(|(node, update)| match update {
            Update::Same => None,
            Update::Set(_) => Some(node.clone()),
        })
        .collect();
    if !illegal_node_kind_updates.is_empty() {
        errors.push(format!(
            "final cleanup may not change node kinds: {:?}",
            illegal_node_kind_updates
        ));
    }
    let illegal_proof_updates: Vec<_> = normalized
        .proof_node_updates
        .iter()
        .filter_map(|(node, update)| match update {
            Update::Same => None,
            Update::Set(_) => Some(node.clone()),
        })
        .collect();
    if !illegal_proof_updates.is_empty() {
        errors.push(format!(
            "final cleanup may not change proof-node classification: {:?}",
            illegal_proof_updates
        ));
    }
    let current_coverage = coverage_from_claims(
        &input.normalization.configured_targets,
        &input.normalization.current_target_claims,
        &input.normalization.current_present_nodes,
    );
    if normalized.snapshot.coverage != current_coverage {
        errors.push("final cleanup may not change paper-target coverage".to_string());
    }

    errors
}

fn proof_local_like_contract_errors(
    input: &WorkerAcceptanceInput,
    normalized: &WorkerNormalizationOutput,
) -> Vec<String> {
    let local_like_proof = input.validation_execution_plan.iter().any(|step| {
        matches!(
            step,
            WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                mode: WorkerProofDeltaMode::Easy | WorkerProofDeltaMode::Local,
                ..
            }
        )
    });
    if !local_like_proof {
        return Vec::new();
    }

    let new_nodes: BTreeSet<_> = normalized
        .snapshot
        .present_nodes
        .difference(&input.normalization.current_present_nodes)
        .cloned()
        .collect();
    let illegal_target_claim_nodes: Vec<_> = new_nodes
        .iter()
        .filter(|node| {
            normalized
                .snapshot
                .coverage
                .values()
                .any(|nodes| nodes.contains(*node))
        })
        .cloned()
        .collect();
    if illegal_target_claim_nodes.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "proof-local/easy helper nodes may not claim paper targets: {:?}",
            illegal_target_claim_nodes
        )]
    }
}

fn tex_proof_starts_with_sketch_marker(tex_content: &str) -> bool {
    let Some((_, after_begin)) = tex_content.split_once("\\begin{proof}") else {
        return false;
    };
    let proof = after_begin
        .split_once("\\end{proof}")
        .map(|(proof, _)| proof)
        .unwrap_or(after_begin);
    for line in proof.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return trimmed == "SKETCH:";
    }
    false
}

/// Contract check for `WorkerResponse.deviation_deletions`: every
/// to-delete id must be unclaimed by every node after the response's
/// own `node_deviation_claims` and `deviation_requests` updates are
/// notionally applied to the current `node_deviation_claims` view.
/// Workers must explicitly clear claims for the id (per node) in the
/// same response, or rely on a prior burst having already done so.
fn deviation_deletion_contract_errors(input: &WorkerAcceptanceInput) -> Vec<String> {
    if input.deviation_deletions.is_empty() {
        return Vec::new();
    }
    let mut post_claims = input.current_node_deviation_claims.clone();
    for (id, request) in &input.deviation_requests {
        if request.path.trim().is_empty() {
            continue;
        }
        for node in &request.affected_nodes {
            if input.normalization.current_present_nodes.contains(node) {
                post_claims
                    .entry(node.clone())
                    .or_default()
                    .insert(id.clone());
            }
        }
    }
    for (node, claims) in &input.node_deviation_claims {
        if claims.is_empty() {
            post_claims.remove(node);
        } else {
            post_claims.insert(node.clone(), claims.clone());
        }
    }
    let mut errors = Vec::new();
    for id in &input.deviation_deletions {
        let mut stale_nodes: Vec<&NodeId> = post_claims
            .iter()
            .filter(|(_, claims)| claims.contains(id))
            .map(|(node, _)| node)
            .collect();
        stale_nodes.sort();
        if !stale_nodes.is_empty() {
            errors.push(format!(
                "deviation_deletions contains `{id}` but node_deviation_claims (after applying this response's updates) still claims it from: {}. Clear the claim from each node in the same response before deleting, or run a prior burst that clears the claims first.",
                stale_nodes
                    .iter()
                    .map(|n| n.as_str().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    errors
}

/// Contract check for `WorkerResponse.deviation_requests`: every
/// requested deviation with a non-empty path must point at a readable
/// file on disk relative to `repo_path`. Without this, a worker can
/// register a deviation whose TeX file does not exist; the deviation
/// verifier then runs against an empty fingerprint and the kernel
/// cannot pin a stable Pass/Fail (see `apply_deviation_updates` and
/// `current_deviation_state`), so the deviation lane loops forever.
fn deviation_request_file_existence_errors(input: &WorkerAcceptanceInput) -> Vec<String> {
    let mut errors = Vec::new();
    for (id, request) in &input.deviation_requests {
        let path = request.path.trim();
        if path.is_empty() {
            continue;
        }
        let file_path = input.normalization.repo_path.join(&request.path);
        if !file_path.is_file() {
            errors.push(format!(
                "deviation_requests.{id}.path = `{path}` but no readable file exists at that path. Create the TeX file before emitting the deviation request."
            ));
        }
    }
    errors
}

/// Contract check for `WorkerResponse.node_deviation_claims`: every
/// claimed deviation id must either already be tracked in the kernel
/// (`current_deviation_files`) or be created/updated by this same
/// response's `deviation_requests` (with a non-empty path). Otherwise
/// `normalize_live_structural_state` silently drops the claim, hiding
/// typos behind a later substantiveness failure instead of giving
/// deterministic worker-contract feedback.
fn node_deviation_claim_unknown_id_errors(input: &WorkerAcceptanceInput) -> Vec<String> {
    let mut errors = Vec::new();
    let requested_with_path: BTreeSet<&DeviationId> = input
        .deviation_requests
        .iter()
        .filter(|(_, request)| !request.path.trim().is_empty())
        .map(|(id, _)| id)
        .collect();
    for (node, claims) in &input.node_deviation_claims {
        for id in claims {
            if input.current_deviation_files.contains_key(id) {
                continue;
            }
            if requested_with_path.contains(id) {
                continue;
            }
            errors.push(format!(
                "node_deviation_claims.{node} claims `{id}` but no such deviation is tracked or being requested in this response. Remove the claim, fix the id (typo?), or add a `deviation_requests` entry that creates it."
            ));
        }
    }
    errors
}

/// Contract check for `WorkerResponse.deviation_deletions`: for every
/// to-delete id whose path is tracked by the kernel, the underlying
/// `reference/<path>.tex` file must already be removed from disk.
/// Without this, deleting a deviation leaves a stale reference file
/// that future workers may mistake for an active deviation.
/// Silent no-op when the id is not in `current_deviation_files` —
/// mirrors `apply_worker_structure_updates`'s no-op semantics for
/// unknown-id deletion.
fn deviation_deletion_file_hygiene_errors(input: &WorkerAcceptanceInput) -> Vec<String> {
    let mut errors = Vec::new();
    for id in &input.deviation_deletions {
        let Some(path) = input.current_deviation_files.get(id) else {
            continue;
        };
        if path.trim().is_empty() {
            continue;
        }
        let file_path = input.normalization.repo_path.join(path);
        if file_path.exists() {
            errors.push(format!(
                "deviation_deletions contains `{id}` but `{path}` still exists on disk. Remove the file before listing the id in deviation_deletions."
            ));
        }
    }
    errors
}

/// Contract check closing the symmetric gap to the existing P1 and P3
/// checks (`deviation_request_file_existence_errors` and
/// `deviation_deletion_file_hygiene_errors`). Without this rule a
/// worker can silently `rm` a tracked deviation file — one already
/// recorded in `current_deviation_files` — without listing the id in
/// `deviation_deletions` and without re-emitting a `deviation_requests`
/// entry. The kernel would then keep the deviation in its tracking map
/// while the underlying reference TeX is gone, leaving the deviation
/// verifier to fingerprint an empty file and the deviation lane to
/// loop on a Pass/Fail mismatch.
///
/// P1 (`deviation_request_file_existence_errors`) rejects "registered
/// without file present"; P3 (`deviation_deletion_file_hygiene_errors`)
/// rejects "deleted with file still present". This check rejects the
/// remaining quadrant: "still tracked, but file silently removed and
/// the response neither deletes the id nor re-emits a request for it".
/// Together the three checks form a closed invariant — every tracked
/// deviation either has its file on disk or is explicitly being
/// retired or refreshed in the same burst.
fn deviation_tracked_file_still_present_errors(input: &WorkerAcceptanceInput) -> Vec<String> {
    let mut errors = Vec::new();
    let requested_with_path: BTreeSet<&DeviationId> = input
        .deviation_requests
        .iter()
        .filter(|(_, request)| !request.path.trim().is_empty())
        .map(|(id, _)| id)
        .collect();
    for (id, path) in &input.current_deviation_files {
        if input.deviation_deletions.contains(id) {
            // covered by `deviation_deletion_file_hygiene_errors`
            continue;
        }
        if requested_with_path.contains(id) {
            // worker is updating it; `deviation_request_file_existence_errors`
            // checks the new path's file
            continue;
        }
        if path.trim().is_empty() {
            continue;
        }
        let file_path = input.normalization.repo_path.join(path);
        if !file_path.is_file() {
            errors.push(format!(
                "deviation `{id}` was registered at `{path}` but the file is no longer on disk and the response neither lists `{id}` in deviation_deletions nor re-emits a deviation_requests entry for it. Restore the file, list `{id}` in deviation_deletions, or re-emit a deviation_requests entry."
            ));
        }
    }
    errors
}

fn post_initial_new_sketch_node_contract_errors(
    input: &WorkerAcceptanceInput,
    normalized: &WorkerNormalizationOutput,
) -> Vec<String> {
    if input.cycle <= 1 {
        return Vec::new();
    }
    let offending_nodes: Vec<_> = normalized
        .proof_node_updates
        .iter()
        .filter_map(|(node, update)| match update {
            Update::Set(true) => Some(node),
            _ => None,
        })
        .filter(|node| !input.normalization.current_present_nodes.contains(*node))
        .filter(|node| {
            tex_proof_starts_with_sketch_marker(&read_text(&node_tex_path(
                &input.normalization.repo_path,
                node,
            )))
        })
        .cloned()
        .collect();
    if offending_nodes.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "FILESPEC post-initial SKETCH rule failed in cycle {}: new proof-bearing nodes created after cycle 1 may not use a SKETCH marker; write a complete NL proof expected to pass strict soundness verification or do not create the node. Offending nodes: {:?}",
            input.cycle, offending_nodes
        )]
    }
}

pub fn accept_worker_response(
    input: &WorkerAcceptanceInput,
) -> Result<WorkerAcceptanceOutput, String> {
    let normalized = normalize_worker_response(&input.normalization)?;
    let cleanup_worker = has_cleanup_validation_step(&input.validation_execution_plan)
        || has_final_cleanup_validation_step(&input.validation_execution_plan);
    let mut contract_errors = normalized.contract_errors.clone();
    contract_errors.extend(cleanup_contract_errors(input, &normalized));
    contract_errors.extend(final_cleanup_contract_errors(input, &normalized));
    contract_errors.extend(proof_local_like_contract_errors(input, &normalized));
    contract_errors.extend(post_initial_new_sketch_node_contract_errors(
        input,
        &normalized,
    ));
    contract_errors.extend(deviation_deletion_contract_errors(input));
    contract_errors.extend(deviation_request_file_existence_errors(input));
    contract_errors.extend(node_deviation_claim_unknown_id_errors(input));
    contract_errors.extend(deviation_deletion_file_hygiene_errors(input));
    contract_errors.extend(deviation_tracked_file_still_present_errors(input));
    let validation_errors = validation_errors_from_step_results(
        &input.validation_execution_plan,
        &input.validation_step_results,
    );

    let (final_outcome, ok, errors) = match input.payload_outcome {
        WorkerOutcome::Valid => {
            let errors: Vec<String> = validation_errors
                .iter()
                .chain(contract_errors.iter())
                .cloned()
                .collect();
            if errors.is_empty() {
                (WorkerOutcome::Valid, true, Vec::new())
            } else {
                (WorkerOutcome::Invalid, false, errors)
            }
        }
        WorkerOutcome::Invalid => (WorkerOutcome::Invalid, true, Vec::new()),
        // Stuck and NeedsRestructure are NOT reclassified to Invalid when the
        // worker left a tablet delta — that rule was load-bearing before the
        // automatic worktree rollback widening (commit daf5ecf), but is now
        // redundant with `worker_response_should_preserve_attempt` +
        // `RestoreWorktreeToActiveWorkerBase` + engine-level
        // `state.restore_committed()`. Honouring the worker's actual outcome
        // preserves verdict signal for the reviewer (decomposition broken vs
        // fumble). The `forbid_tablet_changes_when_stuck` flag is now inert.
        // See CLAUDES_NOTES_remove_stuck_nr_no_delta_rule.md.
        WorkerOutcome::Stuck => {
            if cleanup_worker && !contract_errors.is_empty() {
                (WorkerOutcome::Invalid, false, contract_errors.clone())
            } else {
                (WorkerOutcome::Stuck, true, Vec::new())
            }
        }
        WorkerOutcome::NeedsRestructure => {
            if cleanup_worker && !contract_errors.is_empty() {
                (WorkerOutcome::Invalid, false, contract_errors.clone())
            } else {
                (WorkerOutcome::NeedsRestructure, true, Vec::new())
            }
        }
    };

    // Patch B: merge per-node local-closure probe payloads from every
    // step result into the final WorkerResponse. Multiple step results
    // (e.g., proof_worker_delta + cleanup_preserving in a multi-step
    // plan) may each populate distinct entries; later entries win on
    // node-id collisions, but in practice each node appears in at most
    // one step's payload because only the `must_close_active` gate
    // emits results in Patch B. Empty on rejected accepts.
    let local_closure_results: BTreeMap<NodeId, crate::model::LocalClosureProbeOutput> =
        if final_outcome == WorkerOutcome::Valid {
            let mut merged: BTreeMap<NodeId, crate::model::LocalClosureProbeOutput> =
                BTreeMap::new();
            for step in &input.validation_step_results {
                for (node, probe) in &step.local_closure_results {
                    merged.insert(node.clone(), probe.clone());
                }
            }
            merged
        } else {
            BTreeMap::new()
        };

    Ok(WorkerAcceptanceOutput {
        response: WorkerResponse {
            request_id: input.request_id,
            cycle: input.cycle,
            outcome: final_outcome,
            snapshot: normalized.snapshot,
            proof_node_updates: normalized.proof_node_updates,
            node_kind_updates: normalized.node_kind_updates,
            dep_updates: normalized.dep_updates,
            target_claim_updates: normalized.target_claim_updates,
            difficulty_updates: input.difficulty_updates.clone(),
            deviation_requests: if final_outcome == WorkerOutcome::Valid {
                input.deviation_requests.clone()
            } else {
                BTreeMap::new()
            },
            node_deviation_claims: if final_outcome == WorkerOutcome::Valid {
                input.node_deviation_claims.clone()
            } else {
                BTreeMap::new()
            },
            deviation_deletions: if final_outcome == WorkerOutcome::Valid {
                input.deviation_deletions.clone()
            } else {
                BTreeSet::new()
            },
            protected_semantic_change_nodes: if final_outcome == WorkerOutcome::Valid {
                input.protected_semantic_change_nodes.clone()
            } else {
                BTreeSet::new()
            },
            local_closure_results,
            ..WorkerResponse::default()
        },
        contract_errors,
        validation_errors,
        final_outcome,
        ok,
        errors,
    })
}

pub fn prepare_worker_gate_observations(
    input: &WorkerGateObservationInput,
) -> Result<WorkerGateObservationOutput, String> {
    if !input.collect_observations {
        return Ok(WorkerGateObservationOutput::default());
    }

    let mut output = WorkerGateObservationOutput::default();
    if input.observation_plan.capture_before_snapshot {
        output.before_snapshot = snapshot_tablet_dir(&input.repo_path);
    }
    if input.observation_plan.capture_before_tablet_contents {
        output.before_tablet_contents = snapshot_tablet_file_contents(&input.repo_path);
    }

    if let Some(active_node) = input.active_node.as_ref() {
        let lean_path = node_lean_path(&input.repo_path, active_node);
        if lean_path.exists() {
            let lean_content = read_text(&lean_path);
            if input.observation_plan.capture_imports_before {
                output.imports_before = extract_imports(&lean_content);
            }
            if input.observation_plan.capture_expected_active_hash {
                output.expected_active_hash =
                    declaration_hash_for_gate(&input.repo_path, &lean_content, active_node)?;
            }
        }
    }

    if input.observation_plan.capture_baseline_declaration_hashes {
        for node in &input.current_present_nodes {
            let lean_path = node_lean_path(&input.repo_path, node);
            if lean_path.exists() {
                let content = read_text(&lean_path);
                let hash = declaration_hash_for_gate(&input.repo_path, &content, node)?;
                output
                    .baseline_declaration_hashes
                    .insert(node.clone(), hash);
            }
        }
    }

    Ok(output)
}

fn tablet_dir(repo_path: &Path) -> PathBuf {
    repo_path.join("Tablet")
}

fn hash_bytes(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

pub fn snapshot_tablet_dir(repo_path: &Path) -> BTreeMap<String, String> {
    let tablet_dir = tablet_dir(repo_path);
    if !tablet_dir.exists() {
        return BTreeMap::new();
    }
    let Ok(entries) = fs::read_dir(&tablet_dir) else {
        return BTreeMap::new();
    };
    let mut snapshot = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(content) = fs::read(&path) else {
            continue;
        };
        snapshot.insert(name.to_string(), hash_bytes(&content));
    }
    snapshot
}

pub fn snapshot_tablet_file_contents(repo_path: &Path) -> BTreeMap<String, String> {
    let tablet_dir = tablet_dir(repo_path);
    if !tablet_dir.exists() {
        return BTreeMap::new();
    }
    let Ok(entries) = fs::read_dir(&tablet_dir) else {
        return BTreeMap::new();
    };
    let mut snapshot = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        snapshot.insert(name.to_string(), content);
    }
    snapshot
}

fn node_lean_path(repo_path: &Path, node: &str) -> PathBuf {
    tablet_dir(repo_path).join(format!("{node}.lean"))
}

fn node_tex_path(repo_path: &Path, node: &str) -> PathBuf {
    tablet_dir(repo_path).join(format!("{node}.tex"))
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

pub(crate) fn present_nodes_from_repo(repo_path: &Path) -> Result<BTreeSet<NodeId>, String> {
    let tablet_dir = tablet_dir(repo_path);
    if !tablet_dir.exists() {
        return Ok(BTreeSet::new());
    }
    let mut names = BTreeSet::new();
    let entries = fs::read_dir(&tablet_dir)
        .map_err(|err| format!("failed to read {}: {err}", tablet_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to read tablet entry: {err}"))?;
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        match path.extension().and_then(|value| value.to_str()) {
            Some("lean") if stem != AXIOMS_NAME => {
                names.insert(NodeId::from(stem));
            }
            Some("tex") if stem != HEADER_NAME => {
                names.insert(NodeId::from(stem));
            }
            _ => {}
        }
    }
    Ok(names)
}

/// Walk `<repo>/Tablet/` recursively for `.lean` files and reject any that
/// are neither `Tablet/Preamble.lean`, the kernel-managed `Tablet/Axioms.lean`,
/// nor `Tablet/<X>.lean` for some `X` in `present_nodes`. Subdirectory-nested
/// `.lean` files are always rejected: they fall outside the tablet protocol's
/// compilation surface and would otherwise fail later at lake-build time with
/// opaque cascading "object file ... does not exist" errors across every
/// importing node.
///
/// Each offending file produces one rejection line carrying its repo-relative
/// path and the suggested phrasing from FILESPEC.md.
fn tablet_lean_layout_errors(repo_path: &Path, present_nodes: &BTreeSet<NodeId>) -> Vec<String> {
    let tablet_dir = tablet_dir(repo_path);
    if !tablet_dir.exists() {
        return Vec::new();
    }
    let mut offending: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<PathBuf> = vec![tablet_dir.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|v| v.to_str()) != Some("lean") {
                continue;
            }
            let rel_display = path
                .strip_prefix(repo_path)
                .unwrap_or(&path)
                .display()
                .to_string();
            // Determine if this is an allowed top-level file.
            let parent_is_tablet_root = path.parent().is_some_and(|p| p == tablet_dir);
            if parent_is_tablet_root {
                let Some(stem) = path.file_stem().and_then(|v| v.to_str()) else {
                    continue;
                };
                if stem == PREAMBLE_NAME || stem == AXIOMS_NAME || present_nodes.contains(stem) {
                    continue;
                }
            }
            offending.insert(rel_display);
        }
    }
    offending
        .into_iter()
        .map(|path| {
            format!(
                "{path}: not a registered tablet node. All Lean source under Tablet/ must live in a registered tablet node (Tablet/<NodeName>.lean) or in Tablet/Preamble.lean. Move shared declarations into Preamble.lean or factor them into a tablet node."
            )
        })
        .collect()
}

pub(crate) fn node_kinds_from_repo(
    repo_path: &Path,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<NodeId, NodeKind> {
    present_nodes
        .iter()
        .map(|node| {
            let kind = if node == PREAMBLE_NAME {
                NodeKind::Preamble
            } else {
                let env = tex_statement_environment(&read_text(&node_tex_path(repo_path, node)));
                if PROOF_BEARING_ENVS.contains(&env.as_str()) {
                    NodeKind::Proof
                } else {
                    NodeKind::Definition
                }
            };
            (node.clone(), kind)
        })
        .collect()
}

pub(crate) fn proof_nodes_from_kinds(
    node_kinds: &BTreeMap<NodeId, NodeKind>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeSet<NodeId> {
    present_nodes
        .iter()
        .filter(|node| node_kinds.get(*node) == Some(&NodeKind::Proof))
        .cloned()
        .collect()
}

pub(crate) fn proof_nodes_from_repo(
    repo_path: &Path,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeSet<NodeId> {
    let node_kinds = node_kinds_from_repo(repo_path, present_nodes);
    proof_nodes_from_kinds(&node_kinds, present_nodes)
}

pub fn open_nodes_from_repo(
    repo_path: &Path,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeSet<NodeId> {
    present_nodes
        .iter()
        .filter(|node| {
            let lean_path = node_lean_path(repo_path, node);
            if !lean_path.exists() {
                return true;
            }
            has_sorry(&read_text(&lean_path))
        })
        .cloned()
        .collect()
}

pub fn direct_deps_from_repo(
    repo_path: &Path,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<NodeId, BTreeSet<NodeId>> {
    present_nodes
        .iter()
        .map(|node| {
            let deps = extract_tablet_imports(&read_text(&node_lean_path(repo_path, node)))
                .into_iter()
                .filter(|dep| dep != node)
                .collect();
            (node.clone(), deps)
        })
        .collect()
}

fn extract_tablet_imports(lean_content: &str) -> Vec<NodeId> {
    lean_content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let suffix = trimmed.strip_prefix("import Tablet.")?;
            if suffix.is_empty() {
                None
            } else {
                Some(NodeId::from(suffix.trim()))
            }
        })
        .collect()
}

fn extract_imports(lean_content: &str) -> Vec<String> {
    lean_content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let suffix = trimmed.strip_prefix("import ")?;
            if suffix.is_empty() {
                None
            } else {
                Some(suffix.trim().to_string())
            }
        })
        .collect()
}

fn tex_statement_environment(tex_content: &str) -> String {
    let mut start = 0usize;
    while let Some(idx) = tex_content[start..].find("\\begin{") {
        let env_start = start + idx + "\\begin{".len();
        let Some(end_idx) = tex_content[env_start..].find('}') else {
            break;
        };
        let env = tex_content[env_start..env_start + end_idx]
            .trim()
            .to_ascii_lowercase();
        if matches!(
            env.as_str(),
            "theorem" | "lemma" | "definition" | "corollary" | "proposition" | "helper"
        ) {
            return env;
        }
        start = env_start + end_idx + 1;
    }
    String::new()
}

fn has_sorry(lean_content: &str) -> bool {
    let masked = mask_comments_and_strings(lean_content);
    if has_macro_rules_sorry_rewrite(&masked) {
        // `local macro_rules | `(tactic| sorry) => ...` (or the `term`
        // variant) rewrites the literal `sorry` token into a real proof
        // body at parse time. The compiled term contains no `sorryAx`,
        // so the file is functionally sorry-free even though the source
        // text still contains the token. Treat it as such here so
        // `open_nodes_from_repo` doesn't keep marking the node as open.
        return false;
    }
    let mut token = String::new();
    for ch in masked.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            if token == "sorry" {
                return true;
            }
            token.clear();
        }
    }
    token == "sorry"
}

/// Detect the `macro_rules | `(tactic| sorry) => ...` (or `term`)
/// pattern that masks a literal `sorry` token by rewriting it to a real
/// proof body. Whitespace-tolerant, no regex dep needed.
fn has_macro_rules_sorry_rewrite(masked: &str) -> bool {
    let normalized: String = masked.chars().filter(|c| !c.is_whitespace()).collect();
    normalized.contains("macro_rules|`(tactic|sorry)=>")
        || normalized.contains("macro_rules|`(term|sorry)=>")
}

// The legacy text-based declaration hash helpers below are now used
// exclusively by `declaration_hash_for_gate` under `cfg(test)`. Production
// builds route through `crate::filespec_split::declaration_hash_strict`,
// which finds the body delimiter via the FILESPEC `-- BODY` marker line
// and the outer `:=` token preceding it. Marking these `cfg(test)`
// keeps the production binary free of the buggy let-truncation path
// (the original motivation for this split was that the `rfind(":=")`
// approach got confused by `let X := Y` in multi-line signatures).
#[cfg(test)]
const DECL_PREFIXES: &[&str] = &[
    "noncomputable theorem ",
    "noncomputable def ",
    "theorem ",
    "lemma ",
    "def ",
    "abbrev ",
];

#[cfg(test)]
const NAMESPACE_PREFIXES: &[&str] = &[
    "Filter.",
    "Real.",
    "Nat.",
    "Int.",
    "Set.",
    "Finset.",
    "MeasureTheory.",
    "Topology.",
    "ENNReal.",
    "NNReal.",
];

#[cfg(test)]
fn declaration_name_matches(line: &str, node_name: &str) -> bool {
    let trimmed = line.trim();
    DECL_PREFIXES.iter().any(|prefix| {
        trimmed
            .strip_prefix(prefix)
            .and_then(|rest| rest.split_whitespace().next())
            == Some(node_name)
    })
}

#[cfg(test)]
fn find_declaration(content: &str, node_name: &str) -> Option<String> {
    let mut decl_lines: Vec<String> = Vec::new();
    let mut found = false;
    for line in content.lines() {
        if declaration_name_matches(line, node_name) {
            found = true;
            decl_lines = vec![line.trim().to_string()];
            if line.contains(":=") {
                return Some(decl_lines.join(" "));
            }
            continue;
        }
        if found {
            decl_lines.push(line.trim().to_string());
            if line.contains(":=") {
                return Some(decl_lines.join(" "));
            }
        }
    }
    if decl_lines.is_empty() {
        None
    } else {
        Some(decl_lines.join(" "))
    }
}

#[cfg(test)]
fn normalize_declaration(decl: &str) -> String {
    // Strip the body binding (`:=` and everything after) via rfind, so a
    // default-argument `:=` inside the signature stays intact. Matches the
    // sibling implementation in `runtime_cli_observations.rs::normalize_declaration`
    // — they must produce the same normalized form or capture-time and
    // check-time hashes will disagree and the checker will report a
    // "Declaration signature changed" false positive on proof-body-only
    // edits (e.g. replacing `:= by sorry` with `:= by <proof>`).
    let mut normalized = decl.trim().to_string();
    if let Some(pos) = normalized.rfind(":=") {
        normalized.truncate(pos);
        normalized = normalized.trim().to_string();
    }
    for prefix in NAMESPACE_PREFIXES {
        normalized = normalized.replace(prefix, "");
    }
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
fn declaration_hash(content: &str, node_name: &str) -> String {
    find_declaration(content, node_name)
        .map(|decl| hash_bytes(normalize_declaration(&decl).as_bytes()))
        .unwrap_or_default()
}

/// Producer-side hash entry point for `prepare_worker_gate_observations`.
///
/// Production builds: routes through `filespec_split::declaration_hash_strict`,
/// which finds the FILESPEC `-- BODY` marker line and hashes the
/// file-prefix slice `content[..body_marker_start_byte]` after
/// namespace-prefix stripping and whitespace collapse. Pure-text: no
/// Lean dependency, no checker-socket round-trip. Errors on FILESPEC
/// violations (missing / multiple markers); semantic-gate callers
/// fail closed rather than silently fall back to the legacy text
/// splitter (which had the `let X := …` truncation bug on 79 / 377
/// live tablet nodes; see `project_declaration_hash_bug.md`).
///
/// Test builds (`cfg(test)`): uses the legacy `declaration_hash` text path
/// so unit tests that construct synthetic repos without the FILESPEC
/// marker continue to exercise the function. The fallback is
/// compile-time excluded from release builds.
#[cfg(test)]
fn declaration_hash_for_gate(
    _repo_path: &Path,
    content: &str,
    node_name: &str,
) -> Result<String, String> {
    Ok(declaration_hash(content, node_name))
}

#[cfg(not(test))]
fn declaration_hash_for_gate(
    repo_path: &Path,
    content: &str,
    node_name: &str,
) -> Result<String, String> {
    crate::filespec_split::declaration_hash_strict(repo_path, content, node_name)
}

fn mask_comments_and_strings(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut block_depth = 0usize;
    while i < chars.len() {
        if block_depth > 0 {
            if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '-' {
                block_depth += 1;
                out.push(' ');
                out.push(' ');
                i += 2;
            } else if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1] == '/' {
                block_depth -= 1;
                out.push(' ');
                out.push(' ');
                i += 2;
            } else if chars[i] == '\n' {
                out.push('\n');
                i += 1;
            } else {
                out.push(' ');
                i += 1;
            }
            continue;
        }

        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '-' {
            block_depth = 1;
            out.push(' ');
            out.push(' ');
            i += 2;
            continue;
        }
        if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1] == '-' {
            out.push(' ');
            out.push(' ');
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if chars[i] == '"' {
            out.push(' ');
            i += 1;
            while i < chars.len() {
                let ch = chars[i];
                if ch == '\n' {
                    out.push('\n');
                    i += 1;
                    break;
                }
                out.push(' ');
                if ch == '"' && (i == 0 || chars[i - 1] != '\\') {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn normalize_target_claim_updates(
    raw_updates: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    current_target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    present_nodes: &BTreeSet<NodeId>,
    configured_targets: &BTreeSet<TargetId>,
    force_nodes: &BTreeSet<NodeId>,
) -> TargetClaimUpdates {
    raw_updates
        .iter()
        .filter_map(|(node, targets)| {
            if !present_nodes.contains(node) {
                return None;
            }
            let normalized: BTreeSet<_> = targets
                .iter()
                .filter(|target| configured_targets.contains(*target))
                .cloned()
                .collect();
            let current = current_target_claims.get(node).cloned().unwrap_or_default();
            if !force_nodes.contains(node) && normalized == current {
                None
            } else {
                Some((node.clone(), Update::Set(normalized)))
            }
        })
        .collect()
}

pub(crate) fn apply_target_claim_updates(
    base: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    updates: &TargetClaimUpdates,
    present_nodes: &BTreeSet<NodeId>,
    configured_targets: &BTreeSet<TargetId>,
) -> BTreeMap<NodeId, BTreeSet<TargetId>> {
    present_nodes
        .iter()
        .map(|node| {
            let next = match updates.get(node) {
                Some(Update::Set(targets)) => targets.clone(),
                Some(Update::Same) | None => base
                    .get(node)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|target| configured_targets.contains(target))
                    .collect(),
            };
            (node.clone(), next)
        })
        .collect()
}

pub(crate) fn coverage_from_claims(
    configured_targets: &BTreeSet<TargetId>,
    target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<TargetId, BTreeSet<NodeId>> {
    configured_targets
        .iter()
        .map(|target| {
            let covered = present_nodes
                .iter()
                .filter(|node| {
                    target_claims
                        .get(*node)
                        .is_some_and(|targets| targets.contains(target))
                })
                .cloned()
                .collect();
            (target.clone(), covered)
        })
        .collect()
}

fn complete_fingerprint_map(
    source: &BTreeMap<NodeId, Fingerprint>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<NodeId, Fingerprint> {
    present_nodes
        .iter()
        .map(|node| (node.clone(), source.get(node).cloned().unwrap_or_default()))
        .collect()
}

fn diff_proof_nodes(
    current_proof_nodes: &BTreeSet<NodeId>,
    next_proof_nodes: &BTreeSet<NodeId>,
) -> NodeBoolUpdates {
    current_proof_nodes
        .union(next_proof_nodes)
        .filter_map(|node| {
            let current = current_proof_nodes.contains(node);
            let next = next_proof_nodes.contains(node);
            if current == next {
                None
            } else {
                Some((node.clone(), Update::Set(next)))
            }
        })
        .collect()
}

fn diff_node_kinds(
    current_node_kinds: &BTreeMap<NodeId, NodeKind>,
    next_node_kinds: &BTreeMap<NodeId, NodeKind>,
    present_nodes: &BTreeSet<NodeId>,
) -> NodeKindUpdates {
    present_nodes
        .iter()
        .filter_map(|node| {
            let current = current_node_kinds.get(node).copied().unwrap_or_default();
            let next = next_node_kinds.get(node).copied().unwrap_or_default();
            if current == next {
                None
            } else {
                Some((node.clone(), Update::Set(next)))
            }
        })
        .collect()
}

pub fn diff_node_sets(
    current: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    next: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> NodeSetUpdates {
    current
        .keys()
        .chain(next.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|node| {
            let current_set = current.get(&node).cloned().unwrap_or_default();
            let next_set = next.get(&node).cloned().unwrap_or_default();
            if current_set == next_set {
                None
            } else {
                Some((node, Update::Set(next_set)))
            }
        })
        .collect()
}

fn worker_contract_errors(
    new_nodes: &BTreeSet<NodeId>,
    _changed_dep_nodes: &BTreeSet<NodeId>,
    raw_target_claim_updates: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    next_target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
) -> Vec<String> {
    // `missing_new_semantic` / `missing_changed_dep_semantic` rules
    // removed: semantic_deps is no longer a tracked protocol concept
    // (retired with the protected_correspondence refactor). Workers do
    // not need to declare `semantic_dep_updates` explicitly for new or
    // changed-imports nodes anymore — the fingerprint-based protection
    // uses the real Lean-import closure directly, not a worker-declared
    // parallel graph.
    let missing_new_claims: Vec<_> = new_nodes
        .iter()
        .filter(|node| !raw_target_claim_updates.contains_key(*node))
        .cloned()
        .collect();

    let mut errors = Vec::new();
    if !missing_new_claims.is_empty() {
        errors.push(format!(
            "worker must explicitly report target_claim_updates for every new node (use [] when empty): {:?}",
            missing_new_claims
        ));
    }
    let multi_target_nodes: Vec<_> = next_target_claims
        .iter()
        .filter(|(_, targets)| targets.len() > 1)
        .map(|(node, _)| node.clone())
        .collect();
    if !multi_target_nodes.is_empty() {
        errors.push(format!(
            "a single node may not directly claim multiple paper targets: {:?}",
            multi_target_nodes
        ));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn normalize_worker_response_derives_snapshot_from_repo() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        );

        let input = WorkerNormalizationInput {
            repo_path: repo.clone(),
            configured_targets: BTreeSet::from([TargetId::from("t.a")]),
            current_present_nodes: BTreeSet::from([NodeId::from("A"), NodeId::from("Preamble")]),
            current_target_claims: BTreeMap::from([(
                NodeId::from("A"),
                BTreeSet::from([TargetId::from("t.a")]),
            )]),
            target_claim_updates: BTreeMap::from([(
                NodeId::from("A"),
                BTreeSet::from([TargetId::from("t.a")]),
            )]),
            target_fingerprints: BTreeMap::from([
                (NodeId::from("Preamble"), "".to_string()),
                (NodeId::from("A"), "corr-A".to_string()),
            ]),
            sound_current_fingerprints: BTreeMap::from([
                (NodeId::from("Preamble"), "".to_string()),
                (NodeId::from("A"), "sound-A".to_string()),
            ]),
            ..WorkerNormalizationInput::default()
        };

        let normalized = normalize_worker_response(&input).unwrap();
        assert_eq!(
            normalized.snapshot.present_nodes,
            BTreeSet::from([NodeId::from("A"), NodeId::from("Preamble")])
        );
        assert_eq!(
            normalized.snapshot.open_nodes,
            BTreeSet::from([NodeId::from("A")])
        );
        assert_eq!(
            normalized.snapshot.coverage.get("t.a"),
            Some(&BTreeSet::from([NodeId::from("A")]))
        );
        assert!(normalized.snapshot.paper_current_fingerprints.is_empty());
        assert_eq!(
            normalized.dep_updates.get("A"),
            Some(&Update::Set(BTreeSet::from([NodeId::from("Preamble")])))
        );
        assert!(normalized.contract_errors.is_empty());
    }

    #[test]
    fn normalize_worker_response_marks_new_helper_as_proof_kind() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/Helper.lean"),
            "import Tablet.Preamble\n\ntheorem Helper : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/Helper.tex"),
            "\\begin{helper}Helper claim.\\end{helper}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let input = WorkerNormalizationInput {
            repo_path: repo,
            current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
            current_node_kinds: BTreeMap::from([(NodeId::from("Preamble"), NodeKind::Preamble)]),
            current_target_claims: BTreeMap::from([(NodeId::from("Preamble"), BTreeSet::new())]),
            target_claim_updates: BTreeMap::from([(NodeId::from("Helper"), BTreeSet::new())]),
            ..WorkerNormalizationInput::default()
        };

        let normalized = normalize_worker_response(&input).unwrap();

        assert_eq!(
            normalized.node_kind_updates.get("Helper"),
            Some(&Update::Set(NodeKind::Proof))
        );
        assert_eq!(
            normalized.proof_node_updates.get("Helper"),
            Some(&Update::Set(true))
        );
        assert!(normalized.contract_errors.is_empty());
    }

    #[test]
    fn normalize_worker_response_preserves_explicit_empty_updates_for_new_nodes() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ndef A : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{definition}A\\end{definition}\n",
        );

        let input = WorkerNormalizationInput {
            repo_path: repo,
            current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
            target_claim_updates: BTreeMap::from([(NodeId::from("A"), BTreeSet::new())]),
            ..WorkerNormalizationInput::default()
        };

        let normalized = normalize_worker_response(&input).unwrap();
        assert_eq!(
            normalized.target_claim_updates.get("A"),
            Some(&Update::Set(BTreeSet::new()))
        );
        assert!(normalized.contract_errors.is_empty());
    }

    #[test]
    fn normalize_worker_response_reports_missing_explicit_new_node_updates() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ndef A : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{definition}A\\end{definition}\n",
        );

        let input = WorkerNormalizationInput {
            repo_path: repo,
            current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
            ..WorkerNormalizationInput::default()
        };

        let normalized = normalize_worker_response(&input).unwrap();
        assert_eq!(normalized.contract_errors.len(), 1);
        assert!(normalized
            .contract_errors
            .iter()
            .any(|err| err.contains("target_claim_updates")));
    }

    #[test]
    fn normalize_worker_response_rejects_multi_target_claim_nodes() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ndef A : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{definition}A\\end{definition}\n",
        );

        let normalized = normalize_worker_response(&WorkerNormalizationInput {
            repo_path: repo,
            configured_targets: BTreeSet::from([TargetId::from("t.a"), TargetId::from("t.b")]),
            current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
            target_claim_updates: BTreeMap::from([(
                NodeId::from("A"),
                BTreeSet::from([TargetId::from("t.a"), TargetId::from("t.b")]),
            )]),
            ..WorkerNormalizationInput::default()
        })
        .unwrap();

        assert!(normalized
            .contract_errors
            .iter()
            .any(|err| err.contains("multiple paper targets")));
    }

    #[test]
    fn normalize_worker_response_rejects_non_node_lean_files_under_tablet() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet/Support")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ndef A : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{definition}A\\end{definition}\n",
        );
        // Subdirectory `.lean` file (the original <your-host>.example.com failure mode).
        write(
            &repo.join("Tablet/Support/TwoBitesSupport.lean"),
            "def shared : Nat := 0\n",
        );
        // Top-level `.lean` whose stem is not a registered node.
        write(&repo.join("Tablet/Stray.lean"), "def stray : Nat := 0\n");

        let normalized = normalize_worker_response(&WorkerNormalizationInput {
            repo_path: repo,
            current_present_nodes: BTreeSet::from([NodeId::from("A"), NodeId::from("Preamble")]),
            target_claim_updates: BTreeMap::from([(NodeId::from("A"), BTreeSet::new())]),
            ..WorkerNormalizationInput::default()
        })
        .unwrap();

        // `Stray.lean` is now picked up by `present_nodes_from_repo` (which
        // walks the top level), so it appears in `present_nodes`. The layout
        // check still rejects subdirectory files. The narrower "stem isn't a
        // tablet node" failure mode is exercised in the layout-only test below.
        let support_errors: Vec<&String> = normalized
            .contract_errors
            .iter()
            .filter(|e| e.contains("Tablet/Support/TwoBitesSupport.lean"))
            .collect();
        assert_eq!(
            support_errors.len(),
            1,
            "expected exactly one rejection for the subdirectory file, got {:?}",
            normalized.contract_errors
        );
        assert!(
            support_errors[0].contains(
                "not a registered tablet node. All Lean source under Tablet/ must live in a registered tablet node"
            ),
            "rejection message did not include suggested phrasing: {:?}",
            support_errors[0]
        );
    }

    #[test]
    fn tablet_lean_layout_errors_rejects_unregistered_top_level_lean() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("Tablet/Preamble.lean"), "");
        write(&repo.join("Tablet/A.lean"), "");
        // `Stray` is on disk but not in the registered node set passed in.
        write(&repo.join("Tablet/Stray.lean"), "");
        // Allowed kernel-managed file.
        write(&repo.join("Tablet/Axioms.lean"), "");

        let errors = tablet_lean_layout_errors(
            &repo,
            &BTreeSet::from([NodeId::from("A"), NodeId::from("Preamble")]),
        );
        assert_eq!(errors.len(), 1, "got {:?}", errors);
        assert!(errors[0].starts_with("Tablet/Stray.lean: not a registered tablet node."));
    }

    #[test]
    fn accept_worker_response_maps_contract_failures_to_invalid_for_valid_payloads() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ndef A : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{definition}A\\end{definition}\n",
        );

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 7,
            cycle: 3,
            payload_outcome: WorkerOutcome::Valid,
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(!output.ok);
        assert_eq!(output.response.outcome, WorkerOutcome::Invalid);
        assert!(output
            .errors
            .iter()
            .any(|err| err.contains("target_claim_updates")));
    }

    #[test]
    fn accept_worker_response_allows_stuck_with_tablet_changes() {
        // Post-rule-removal: Stuck-with-delta is honoured (no longer
        // reclassified to Invalid). Engine-level restore_committed +
        // RestoreWorktreeToActiveWorkerBase + last_invalid capture
        // preserve safety. Setting forbid_tablet_changes_when_stuck=true
        // explicitly is now inert.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let before_snapshot = snapshot_tablet_dir(&repo);
        write(
            &repo.join("Tablet/Preamble.tex"),
            "\\begin{definition}extra\\end{definition}\n",
        );

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 8,
            cycle: 4,
            payload_outcome: WorkerOutcome::Stuck,
            before_snapshot,
            // Explicitly true to prove the field is inert post-removal.
            forbid_tablet_changes_when_stuck: true,
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Stuck);
        assert!(output.ok);
        assert_eq!(output.response.outcome, WorkerOutcome::Stuck);
        assert!(output.errors.is_empty());
    }

    #[test]
    fn accept_worker_response_allows_needs_restructure_with_tablet_changes() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let before_snapshot = snapshot_tablet_dir(&repo);
        write(
            &repo.join("Tablet/Preamble.tex"),
            "\\begin{definition}explored\\end{definition}\n",
        );

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 11,
            cycle: 6,
            payload_outcome: WorkerOutcome::NeedsRestructure,
            before_snapshot,
            forbid_tablet_changes_when_stuck: true,
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::NeedsRestructure);
        assert!(output.ok);
        assert_eq!(output.response.outcome, WorkerOutcome::NeedsRestructure);
        assert!(output.errors.is_empty());
    }

    #[test]
    fn accept_worker_response_allows_stuck_without_tablet_changes() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let before_snapshot = snapshot_tablet_dir(&repo);

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 9,
            cycle: 5,
            payload_outcome: WorkerOutcome::Stuck,
            before_snapshot,
            forbid_tablet_changes_when_stuck: true,
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Stuck);
        assert!(output.ok);
        assert_eq!(output.response.outcome, WorkerOutcome::Stuck);
        assert!(output.errors.is_empty());
    }

    #[test]
    fn accept_worker_response_rejects_proof_local_helper_nodes_claiming_targets() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/Helper.lean"),
            "import Tablet.Preamble\n\ndef Helper : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/Helper.tex"),
            "\\begin{definition}Helper\\end{definition}\n",
        );

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 11,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    NodeId::from("A"),
                ]),
                current_proof_nodes: BTreeSet::from([NodeId::from("A")]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (NodeId::from("A"), NodeKind::Proof),
                ]),
                target_claim_updates: BTreeMap::from([(
                    NodeId::from("Helper"),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
                ..WorkerNormalizationInput::default()
            },
            validation_execution_plan: vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                active_node: Some(NodeId::from("A")),
                mode: WorkerProofDeltaMode::Local,
                authorized_nodes: BTreeSet::new(),
                protected_semantic_change_nodes: BTreeSet::new(),
                allow_new_obligations: true,
                must_close_active: false,
            }],
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(output
            .errors
            .iter()
            .any(|err| err.contains("proof-local/easy helper nodes may not claim paper targets")));
    }

    #[test]
    fn accept_worker_response_rejects_deviation_deletion_with_stale_claim() {
        // Worker tries to retire `dev:a` but node `N` still claims it
        // in the current state and the response's `node_deviation_claims`
        // doesn't clear it. The contract check rejects the response and
        // flips the outcome to Invalid.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );

        let dev_id = DeviationId::from("dev:a");
        let claimed_node = NodeId::from("A");
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            deviation_deletions: BTreeSet::from([dev_id.clone()]),
            current_node_deviation_claims: BTreeMap::from([(
                claimed_node.clone(),
                BTreeSet::from([dev_id.clone()]),
            )]),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_proof_nodes: BTreeSet::from([claimed_node.clone()]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (claimed_node.clone(), NodeKind::Proof),
                ]),
                ..WorkerNormalizationInput::default()
            },
            validation_execution_plan: vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                active_node: Some(claimed_node.clone()),
                mode: WorkerProofDeltaMode::Local,
                authorized_nodes: BTreeSet::new(),
                protected_semantic_change_nodes: BTreeSet::new(),
                allow_new_obligations: true,
                must_close_active: false,
            }],
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(
            output
                .errors
                .iter()
                .any(|err| err.contains("deviation_deletions contains `dev:a`")
                    && err.contains(claimed_node.as_str())),
            "expected stale-claim error mentioning the node; got {:?}",
            output.errors
        );
        // The Invalid outcome zeroes the response's deviation_deletions
        // so the apply path can't act on the rejected request.
        assert!(output.response.deviation_deletions.is_empty());
    }

    #[test]
    fn accept_worker_response_allows_deviation_deletion_when_claim_cleared_in_same_burst() {
        // Same as above but the worker also empties `N`'s
        // node_deviation_claims in the response. The contract check
        // accepts; outcome stays Valid and the deletion flows through.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );

        let dev_id = DeviationId::from("dev:a");
        let claimed_node = NodeId::from("A");
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            node_deviation_claims: BTreeMap::from([(claimed_node.clone(), BTreeSet::new())]),
            deviation_deletions: BTreeSet::from([dev_id.clone()]),
            current_node_deviation_claims: BTreeMap::from([(
                claimed_node.clone(),
                BTreeSet::from([dev_id.clone()]),
            )]),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_proof_nodes: BTreeSet::from([claimed_node.clone()]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (claimed_node.clone(), NodeKind::Proof),
                ]),
                ..WorkerNormalizationInput::default()
            },
            // Empty validation_execution_plan keeps the test focused on
            // the deviation-deletion contract: the framework otherwise
            // surfaces "missing step result" errors that drown out the
            // signal we care about.
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert!(
            output.final_outcome == WorkerOutcome::Valid && output.errors.is_empty(),
            "expected Valid outcome with no errors; got {:?} with errors {:?}",
            output.final_outcome,
            output.errors
        );
        assert_eq!(
            output.response.deviation_deletions,
            BTreeSet::from([dev_id])
        );
    }

    #[test]
    fn accept_worker_response_rejects_deviation_request_with_missing_file() {
        // Worker emits a `deviation_requests` entry whose `path` does
        // not exist on disk. The acceptance contract must reject so the
        // deviation lane never enters the "empty fingerprint" loop.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );
        // Note: reference/dev_a.tex is INTENTIONALLY NOT created.

        let dev_id = DeviationId::from("dev:a");
        let claimed_node = NodeId::from("A");
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            deviation_requests: BTreeMap::from([(
                dev_id.clone(),
                DeviationRequest {
                    path: "reference/dev_a.tex".to_string(),
                    summary: "A deviation about constants".to_string(),
                    affected_nodes: BTreeSet::from([claimed_node.clone()]),
                },
            )]),
            node_deviation_claims: BTreeMap::from([(
                claimed_node.clone(),
                BTreeSet::from([dev_id.clone()]),
            )]),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_proof_nodes: BTreeSet::from([claimed_node.clone()]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (claimed_node.clone(), NodeKind::Proof),
                ]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(
            output
                .errors
                .iter()
                .any(|err| err.contains("reference/dev_a.tex")
                    && err.contains("no readable file exists")),
            "expected missing-file error mentioning the path; got {:?}",
            output.errors
        );
        // Rejected outcome zeroes the response's deviation_requests so
        // the apply path can't act on the rejected request.
        assert!(output.response.deviation_requests.is_empty());
    }

    #[test]
    fn accept_worker_response_rejects_node_deviation_claim_with_unknown_id() {
        // Worker emits `node_deviation_claims[N] = {typo_id}` where
        // `typo_id` is neither tracked in `current_deviation_files`
        // nor being requested in this response. The acceptance contract
        // must reject — otherwise the typo is silently pruned in
        // `normalize_live_structural_state` and the worker never learns.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );

        let typo_id = DeviationId::from("dev:typo");
        let claimed_node = NodeId::from("A");
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            node_deviation_claims: BTreeMap::from([(
                claimed_node.clone(),
                BTreeSet::from([typo_id.clone()]),
            )]),
            // Empty current_deviation_files: no tracked deviations.
            // Empty deviation_requests: not being created either.
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_proof_nodes: BTreeSet::from([claimed_node.clone()]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (claimed_node.clone(), NodeKind::Proof),
                ]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(
            output
                .errors
                .iter()
                .any(|err| err.contains("dev:typo")
                    && err.contains("no such deviation is tracked")),
            "expected unknown-claim-id error mentioning the typo'd id; got {:?}",
            output.errors
        );
    }

    #[test]
    fn accept_worker_response_rejects_deviation_deletion_when_file_still_on_disk() {
        // Worker emits `deviation_deletions = {dev:a}` but the
        // `reference/dev_a.tex` file is still present on disk. The
        // contract must reject so retired deviations don't leave stale
        // process evidence under `reference/`.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("reference")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );
        // File still on disk:
        write(
            &repo.join("reference/dev_a.tex"),
            "\\section*{dev_a}\nA difference\n",
        );

        let dev_id = DeviationId::from("dev:a");
        let claimed_node = NodeId::from("A");
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            deviation_deletions: BTreeSet::from([dev_id.clone()]),
            current_deviation_files: BTreeMap::from([(
                dev_id.clone(),
                "reference/dev_a.tex".to_string(),
            )]),
            // No node still claims it (so the stale-claim contract is happy):
            current_node_deviation_claims: BTreeMap::new(),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_proof_nodes: BTreeSet::from([claimed_node.clone()]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (claimed_node.clone(), NodeKind::Proof),
                ]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(
            output
                .errors
                .iter()
                .any(|err| err.contains("reference/dev_a.tex")
                    && err.contains("still exists on disk")),
            "expected file-hygiene error mentioning the path; got {:?}",
            output.errors
        );
        // Rejected outcome zeroes the response's deviation_deletions.
        assert!(output.response.deviation_deletions.is_empty());
    }

    #[test]
    fn accept_worker_response_allows_deviation_deletion_when_file_removed() {
        // Happy-path companion: same shape as the rejection test but
        // the file has been removed from disk. Outcome must be Valid.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        // Note: reference/dev_a.tex is INTENTIONALLY NOT created.
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );

        let dev_id = DeviationId::from("dev:a");
        let claimed_node = NodeId::from("A");
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            deviation_deletions: BTreeSet::from([dev_id.clone()]),
            current_deviation_files: BTreeMap::from([(
                dev_id.clone(),
                "reference/dev_a.tex".to_string(),
            )]),
            current_node_deviation_claims: BTreeMap::new(),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_proof_nodes: BTreeSet::from([claimed_node.clone()]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (claimed_node.clone(), NodeKind::Proof),
                ]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert!(
            output.final_outcome == WorkerOutcome::Valid && output.errors.is_empty(),
            "expected Valid outcome with no errors; got {:?} with errors {:?}",
            output.final_outcome,
            output.errors
        );
        assert_eq!(
            output.response.deviation_deletions,
            BTreeSet::from([dev_id])
        );
    }

    #[test]
    fn accept_worker_response_rejects_silent_removal_of_tracked_deviation_file() {
        // A deviation `dev:a` is already tracked by the kernel at
        // `reference/dev_a.tex` but the file is missing from disk.
        // The worker response neither lists the id in
        // `deviation_deletions` nor re-emits a `deviation_requests`
        // entry for it. The new contract check must reject so the
        // worker can't silently `rm` a tracked deviation file.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("reference")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );
        // Note: reference/dev_a.tex is INTENTIONALLY NOT created — this
        // is the silent-`rm` bug we're guarding against.

        let dev_id = DeviationId::from("dev:a");
        let claimed_node = NodeId::from("A");
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            // No deviation_requests, no deviation_deletions, no
            // node_deviation_claims for `dev:a` — the worker simply
            // doesn't mention the id.
            current_deviation_files: BTreeMap::from([(
                dev_id.clone(),
                "reference/dev_a.tex".to_string(),
            )]),
            current_node_deviation_claims: BTreeMap::new(),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_proof_nodes: BTreeSet::from([claimed_node.clone()]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (claimed_node.clone(), NodeKind::Proof),
                ]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(
            output.errors.iter().any(|err| {
                err.contains("dev:a")
                    && err.contains("reference/dev_a.tex")
                    && err.contains("file is no longer on disk")
            }),
            "expected tracked-file-missing error mentioning the id and path; got {:?}",
            output.errors
        );
    }

    #[test]
    fn accept_worker_response_accepts_when_tracked_deviation_file_still_on_disk() {
        // Happy-path companion: the deviation is tracked at
        // `reference/dev_a.tex` AND the file is present on disk. The
        // worker doesn't mention the id at all. The new contract check
        // must NOT fire — outcome stays Valid.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("reference")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );
        // File present on disk:
        write(
            &repo.join("reference/dev_a.tex"),
            "\\section*{dev_a}\nA difference\n",
        );

        let dev_id = DeviationId::from("dev:a");
        let claimed_node = NodeId::from("A");
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            current_deviation_files: BTreeMap::from([(
                dev_id.clone(),
                "reference/dev_a.tex".to_string(),
            )]),
            current_node_deviation_claims: BTreeMap::new(),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_proof_nodes: BTreeSet::from([claimed_node.clone()]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (claimed_node.clone(), NodeKind::Proof),
                ]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert!(
            output.final_outcome == WorkerOutcome::Valid && output.errors.is_empty(),
            "expected Valid outcome with no errors; got {:?} with errors {:?}",
            output.final_outcome,
            output.errors
        );
        // Defensive: confirm no error from the new check leaked through.
        assert!(
            !output
                .errors
                .iter()
                .any(|err| err.contains("file is no longer on disk")),
            "tracked-file check fired on the happy path; got {:?}",
            output.errors
        );
    }

    #[test]
    fn accept_worker_response_skips_tracked_file_check_when_request_reemitted() {
        // Re-emission exemption: when the worker re-emits a
        // `deviation_requests` entry for an already-tracked id with a
        // non-empty path, the existing P1 check
        // (`deviation_request_file_existence_errors`) covers the new
        // path. The new check must skip the id so we don't double-fire
        // on the old path being absent — and so the re-emission path
        // remains the supported way to refresh a deviation file.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("reference")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial\\end{proof}\n",
        );
        // Old path tracked but file absent — would normally trip the
        // new check. The re-emission below should exempt it.
        // Note: reference/dev_a.tex is INTENTIONALLY NOT created.
        // New path's file IS present so P1 stays happy:
        write(
            &repo.join("reference/dev_a_v2.tex"),
            "\\section*{dev_a}\nRefreshed difference\n",
        );

        let dev_id = DeviationId::from("dev:a");
        let claimed_node = NodeId::from("A");
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            deviation_requests: BTreeMap::from([(
                dev_id.clone(),
                DeviationRequest {
                    path: "reference/dev_a_v2.tex".to_string(),
                    summary: "Refreshed deviation".to_string(),
                    affected_nodes: BTreeSet::from([claimed_node.clone()]),
                },
            )]),
            node_deviation_claims: BTreeMap::from([(
                claimed_node.clone(),
                BTreeSet::from([dev_id.clone()]),
            )]),
            current_deviation_files: BTreeMap::from([(
                dev_id.clone(),
                "reference/dev_a.tex".to_string(),
            )]),
            current_node_deviation_claims: BTreeMap::from([(
                claimed_node.clone(),
                BTreeSet::from([dev_id.clone()]),
            )]),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_proof_nodes: BTreeSet::from([claimed_node.clone()]),
                current_node_kinds: BTreeMap::from([
                    (NodeId::from("Preamble"), NodeKind::Preamble),
                    (claimed_node.clone(), NodeKind::Proof),
                ]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert!(
            output.final_outcome == WorkerOutcome::Valid && output.errors.is_empty(),
            "expected Valid outcome with no errors; got {:?} with errors {:?}",
            output.final_outcome,
            output.errors
        );
        // Defensive: confirm the new check didn't fire on the
        // already-absent old path.
        assert!(
            !output
                .errors
                .iter()
                .any(|err| err.contains("file is no longer on disk")),
            "tracked-file check fired despite re-emission exemption; got {:?}",
            output.errors
        );
    }

    #[test]
    fn accept_worker_response_rejects_new_sketch_proof_node_after_cycle_one() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}\nSKETCH:\nTrivial.\n\\end{proof}\n",
        );

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 12,
            cycle: 2,
            payload_outcome: WorkerOutcome::Valid,
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                current_proof_nodes: BTreeSet::new(),
                current_node_kinds: BTreeMap::from([(
                    NodeId::from("Preamble"),
                    NodeKind::Preamble,
                )]),
                target_claim_updates: BTreeMap::from([(NodeId::from("A"), BTreeSet::new())]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(output.errors.iter().any(|err| {
            err.contains(
                "new proof-bearing nodes created after cycle 1 may not use a SKETCH marker",
            ) && err.contains("A")
        }));
    }

    #[test]
    fn accept_worker_response_allows_new_sketch_proof_node_on_cycle_one() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}\nSKETCH:\nTrivial.\n\\end{proof}\n",
        );

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 1,
            cycle: 1,
            payload_outcome: WorkerOutcome::Valid,
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                current_proof_nodes: BTreeSet::new(),
                current_node_kinds: BTreeMap::from([(
                    NodeId::from("Preamble"),
                    NodeKind::Preamble,
                )]),
                target_claim_updates: BTreeMap::from([(NodeId::from("A"), BTreeSet::new())]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Valid);
    }

    #[test]
    fn accept_worker_response_rejects_cleanup_stuck_outcome() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let before_snapshot = snapshot_tablet_dir(&repo);

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 10,
            cycle: 6,
            payload_outcome: WorkerOutcome::Stuck,
            before_snapshot,
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            validation_execution_plan: vec![
                WorkerValidationExecutionPlanStep::CleanupPreserving {},
            ],
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(!output.ok);
        assert_eq!(output.response.outcome, WorkerOutcome::Invalid);
        assert!(output
            .errors
            .iter()
            .any(|err| err.contains("cleanup worker outcome must be one of ['valid', 'invalid']")));
    }

    #[test]
    fn prepare_worker_gate_observations_collects_requested_baselines() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "import Mathlib.Data.Set.Basic\nimport Tablet.Preamble\n\n\
             theorem A : Nat := by\n  exact 0\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        );

        let output = prepare_worker_gate_observations(&WorkerGateObservationInput {
            repo_path: repo,
            current_present_nodes: BTreeSet::from([NodeId::from("A"), NodeId::from("Preamble")]),
            active_node: Some(NodeId::from("A")),
            observation_plan: crate::model::WorkerAcceptanceObservationPlan {
                capture_before_snapshot: true,
                capture_imports_before: true,
                capture_expected_active_hash: true,
                capture_baseline_declaration_hashes: true,
                ..crate::model::WorkerAcceptanceObservationPlan::default()
            },
            collect_observations: true,
        })
        .unwrap();

        assert!(output.before_snapshot.contains_key("A.lean"));
        assert_eq!(
            output.imports_before,
            vec![
                "Mathlib.Data.Set.Basic".to_string(),
                "Tablet.Preamble".to_string()
            ]
        );
        assert!(!output.expected_active_hash.is_empty());
        assert!(output.baseline_declaration_hashes.contains_key("A"));
        assert!(output.baseline_declaration_hashes.contains_key("Preamble"));
    }
}

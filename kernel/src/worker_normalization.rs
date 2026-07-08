use crate::model::{
    AuditRequest, AuditRequestReasonKind, ChallengeClaimUpdates, ChallengeTargetId,
    ChallengeTargetKind, ChallengeTargetSpec, CleanupTaskKind, DeviationId, DeviationRequest,
    Fingerprint, NodeBoolUpdates, NodeDifficulty, NodeId, NodeKind, NodeKindUpdates,
    NodeSetUpdates, PvRole, TargetClaimUpdates, TargetId, Update, WorkerOutcome,
    WorkerProofDeltaMode, WorkerResponse, WorkerValidationExecutionPlanStep, WorkingSnapshot,
    AUDIT_TASK_REASON_MAX_CHARS,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const AXIOMS_NAME: &str = "Axioms";
const HEADER_NAME: &str = "header";
const PREAMBLE_NAME: &str = "Preamble";
const ASSUMPTIONS_NAME: &str = crate::assumptions_registry::ASSUMPTIONS_NODE;
const PROOF_BEARING_ENVS: &[&str] = &["theorem", "lemma", "corollary", "helper"];
const WHOLE_FILE_DECLARATION_HASH_PREFIX: &str = "whole-file-v1:";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerNormalizationInput {
    pub repo_path: PathBuf,
    pub configured_targets: BTreeSet<TargetId>,
    /// Full challenge registry (not just ids): acceptance needs the
    /// prescribed text for the byte-conformance check.
    #[serde(default)]
    pub configured_challenge_targets: BTreeMap<ChallengeTargetId, ChallengeTargetSpec>,
    pub current_present_nodes: BTreeSet<NodeId>,
    pub current_proof_nodes: BTreeSet<NodeId>,
    pub current_node_kinds: BTreeMap<NodeId, NodeKind>,
    pub current_deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    pub current_target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    #[serde(default)]
    pub current_challenge_claims: BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    /// PV Phase 2: present nodes with `PvRole::ExtractionModel`. The
    /// `extraction_chain_errors` gate (sibling to `challenge_conformance_errors`)
    /// fails CLOSED when such a node claims a challenge target whose provenance
    /// has an empty `source_sha256` or `extractor_toolchain_sha256`. Empty for
    /// all-math / non-PV runs ⇒ the gate is a no-op (byte-identical acceptance).
    #[serde(default)]
    pub extraction_model_nodes: BTreeSet<NodeId>,
    pub approved_paper_fingerprints: BTreeMap<TargetId, Fingerprint>,
    pub target_claim_updates: BTreeMap<NodeId, BTreeSet<TargetId>>,
    #[serde(default)]
    pub challenge_claim_updates: BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
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
    #[serde(default)]
    pub challenge_claim_updates: ChallengeClaimUpdates,
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
    #[serde(default)]
    pub deleted_nodes: BTreeSet<NodeId>,
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
    /// On-demand "call for an audit" (advisory) raw shape, mirroring the
    /// reviewer side's `RawReviewPayload::audit_request`. `accept_worker_response`
    /// parses/normalizes it (reason_kind enum, non-empty trimmed reason, char
    /// cap) onto `WorkerResponse::audit_request`. `None` => the worker is not
    /// requesting an audit this burst.
    #[serde(default)]
    pub audit_request: Option<crate::review_normalization::RawAuditRequest>,
    /// Process memory (spec §5): raw worker challenges against active
    /// entries. `accept_worker_response` parses/normalizes them onto
    /// `WorkerResponse::memory_challenges` (trimmed non-empty fields,
    /// reason char cap). Legal on any outcome.
    #[serde(default)]
    pub memory_challenges: Vec<crate::process_memory::MemoryChallenge>,
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
    /// PV role map. Empty for all-math / non-PV hydration.
    #[serde(default)]
    pub node_role: BTreeMap<NodeId, PvRole>,
    /// PV under-model assumption staging nodes. Empty for all-math / non-PV
    /// hydration, so ordinary nodes named `Assumptions` keep ordinary
    /// correspondence semantics.
    #[serde(default)]
    pub under_model_assumption_nodes: BTreeSet<NodeId>,
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
    /// PV under-model assumption staging nodes. Empty for all-math / non-PV
    /// runs, so an ordinary node named `Assumptions` still captures the normal
    /// active declaration hash.
    pub under_model_assumption_nodes: BTreeSet<NodeId>,
    /// The under-model assumptions node for an actual assumption-authoring
    /// burst. This is stricter than `under_model_assumption_nodes`: it is set
    /// only when the request carrier authorizes staging a new assumption.
    #[serde(default)]
    pub assumption_authoring_node: Option<NodeId>,
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
    let configured_challenge_ids: BTreeSet<ChallengeTargetId> =
        input.configured_challenge_targets.keys().cloned().collect();
    let challenge_claim_updates = normalize_challenge_claim_updates(
        &input.challenge_claim_updates,
        &input.current_challenge_claims,
        &present_nodes,
        &configured_challenge_ids,
        &new_nodes,
    );
    let challenge_claims = apply_challenge_claim_updates(
        &input.current_challenge_claims,
        &challenge_claim_updates,
        &present_nodes,
        &configured_challenge_ids,
    );
    let challenge_coverage = challenge_coverage_from_claims(
        &configured_challenge_ids,
        &challenge_claims,
        &present_nodes,
    );
    let snapshot = WorkingSnapshot {
        present_nodes: present_nodes.clone(),
        open_nodes,
        coverage,
        challenge_coverage,
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
        placeholder_definition_nodes: BTreeSet::new(),
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
    contract_errors.extend(challenge_claim_contract_errors(
        input,
        &new_nodes,
        &challenge_claims,
    ));
    contract_errors.extend(challenge_conformance_errors(
        &input.repo_path,
        &input.configured_challenge_targets,
        &challenge_claims,
        &present_nodes,
    ));
    contract_errors.extend(extraction_chain_errors(
        &input.configured_challenge_targets,
        &challenge_claims,
        &present_nodes,
        &input.extraction_model_nodes,
    ));
    contract_errors.extend(challenge_deletion_guard_errors(
        &input.configured_challenge_targets,
        &input.current_challenge_claims,
        &input.current_present_nodes,
        &snapshot.challenge_coverage,
    ));
    contract_errors.extend(tablet_lean_layout_errors(&input.repo_path, &present_nodes));
    Ok(WorkerNormalizationOutput {
        snapshot,
        proof_node_updates: diff_proof_nodes(&input.current_proof_nodes, &proof_nodes),
        node_kind_updates: diff_node_kinds(&input.current_node_kinds, &node_kinds, &present_nodes),
        dep_updates,
        target_claim_updates,
        challenge_claim_updates,
        contract_errors,
    })
}

fn validation_step_kind_name(step: &WorkerValidationExecutionPlanStep) -> &'static str {
    match step {
        WorkerValidationExecutionPlanStep::TheoremTargetEditScope { .. } => {
            "theorem_target_edit_scope"
        }
        WorkerValidationExecutionPlanStep::RevisionStatementEditScope { .. } => {
            "revision_statement_edit_scope"
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
    configured_challenge_targets: &BTreeMap<ChallengeTargetId, ChallengeTargetSpec>,
    current_challenge_claims: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    current_present_nodes: &BTreeSet<NodeId>,
    current_deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> BTreeSet<NodeId> {
    // Challenge-covering nodes root support exactly like paper-covering
    // nodes (mirrors `ProtocolState::orphan_nodes`).
    let mut roots: BTreeSet<NodeId> = current_target_claims
        .iter()
        .filter(|(node, targets)| {
            current_present_nodes.contains(*node)
                && targets
                    .iter()
                    .any(|target| configured_targets.contains(target))
        })
        .map(|(node, _)| node.clone())
        .collect();
    roots.extend(
        current_challenge_claims
            .iter()
            .filter(|(node, targets)| {
                current_present_nodes.contains(*node)
                    && targets
                        .iter()
                        .any(|target| configured_challenge_targets.contains_key(target))
            })
            .map(|(node, _)| node.clone()),
    );
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
        &input.normalization.configured_challenge_targets,
        &input.normalization.current_challenge_claims,
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
        WorkerOutcome::Stuck
            | WorkerOutcome::NeedsRestructure
            | WorkerOutcome::TargetFalseUnderModel
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
    let illegal_challenge_claim_updates: Vec<_> = normalized
        .challenge_claim_updates
        .iter()
        .filter_map(|(node, update)| match update {
            Update::Same => None,
            Update::Set(targets) if removed_nodes.contains(node) && targets.is_empty() => None,
            _ => Some(node.clone()),
        })
        .collect();
    if !illegal_challenge_claim_updates.is_empty() {
        errors.push(format!(
            "cleanup may not change challenge-target claims except to clear claims on deleted orphan nodes: {:?}",
            illegal_challenge_claim_updates
        ));
    }
    let current_challenge_coverage = challenge_coverage_from_claims(
        &input
            .normalization
            .configured_challenge_targets
            .keys()
            .cloned()
            .collect(),
        &input.normalization.current_challenge_claims,
        &input.normalization.current_present_nodes,
    );
    if normalized.snapshot.challenge_coverage != current_challenge_coverage {
        errors.push("cleanup may not change challenge-target coverage".to_string());
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

fn final_cleanup_deleted_target_proof_clear_allowed(
    input: &WorkerAcceptanceInput,
    removed_nodes: &BTreeSet<NodeId>,
    node: &NodeId,
    update: &Update<bool>,
) -> bool {
    matches!(update, Update::Set(false))
        && removed_nodes.contains(node)
        && input.deleted_nodes.contains(node)
        && input.validation_execution_plan.iter().any(|step| {
            matches!(
                step,
                WorkerValidationExecutionPlanStep::FinalCleanupPreserving {
                    task_kind: Some(CleanupTaskKind::Substitution { .. }),
                    target_node: Some(target_node),
                    ..
                } if target_node == node
            )
        })
}

fn final_cleanup_contract_errors(
    input: &WorkerAcceptanceInput,
    normalized: &WorkerNormalizationOutput,
) -> Vec<String> {
    if !has_final_cleanup_validation_step(&input.validation_execution_plan) {
        return Vec::new();
    }

    let removed_nodes: BTreeSet<NodeId> = input
        .normalization
        .current_present_nodes
        .difference(&normalized.snapshot.present_nodes)
        .cloned()
        .collect();
    let mut errors = Vec::new();
    if matches!(
        input.payload_outcome,
        WorkerOutcome::Stuck
            | WorkerOutcome::NeedsRestructure
            | WorkerOutcome::TargetFalseUnderModel
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
            Update::Set(_)
                if final_cleanup_deleted_target_proof_clear_allowed(
                    input,
                    &removed_nodes,
                    node,
                    update,
                ) =>
            {
                None
            }
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
    let illegal_challenge_claim_updates: Vec<_> = normalized
        .challenge_claim_updates
        .iter()
        .filter_map(|(node, update)| match update {
            Update::Same => None,
            Update::Set(_) => Some(node.clone()),
        })
        .collect();
    if !illegal_challenge_claim_updates.is_empty() {
        errors.push(format!(
            "final cleanup may not change challenge-target claims: {:?}",
            illegal_challenge_claim_updates
        ));
    }

    errors
}

fn apply_raw_target_claim_updates_for_present(
    base: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    raw_updates: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    present_nodes: &BTreeSet<NodeId>,
    configured_targets: &BTreeSet<TargetId>,
) -> BTreeMap<NodeId, BTreeSet<TargetId>> {
    present_nodes
        .iter()
        .map(|node| {
            let next = raw_updates
                .get(node)
                .cloned()
                .unwrap_or_else(|| base.get(node).cloned().unwrap_or_default())
                .into_iter()
                .filter(|target| configured_targets.contains(target))
                .collect();
            (node.clone(), next)
        })
        .collect()
}

fn apply_raw_challenge_claim_updates_for_present(
    base: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    raw_updates: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    present_nodes: &BTreeSet<NodeId>,
    configured_challenge_targets: &BTreeSet<ChallengeTargetId>,
) -> BTreeMap<NodeId, BTreeSet<ChallengeTargetId>> {
    present_nodes
        .iter()
        .map(|node| {
            let next = raw_updates
                .get(node)
                .cloned()
                .unwrap_or_else(|| base.get(node).cloned().unwrap_or_default())
                .into_iter()
                .filter(|target| configured_challenge_targets.contains(target))
                .collect();
            (node.clone(), next)
        })
        .collect()
}

fn deps_after_normalized_updates_for_present(
    input: &WorkerAcceptanceInput,
    normalized: &WorkerNormalizationOutput,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<NodeId, BTreeSet<NodeId>> {
    present_nodes
        .iter()
        .map(|node| {
            let deps = match normalized.dep_updates.get(node) {
                Some(Update::Set(deps)) => deps.clone(),
                Some(Update::Same) | None => input
                    .normalization
                    .current_deps
                    .get(node)
                    .cloned()
                    .unwrap_or_default(),
            };
            (node.clone(), deps)
        })
        .collect()
}

fn roots_from_claim_maps(
    configured_targets: &BTreeSet<TargetId>,
    target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    configured_challenge_targets: &BTreeSet<ChallengeTargetId>,
    challenge_claims: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeSet<NodeId> {
    let mut roots: BTreeSet<NodeId> = target_claims
        .iter()
        .filter(|(node, targets)| {
            present_nodes.contains(*node)
                && targets
                    .iter()
                    .any(|target| configured_targets.contains(target))
        })
        .map(|(node, _)| node.clone())
        .collect();
    roots.extend(
        challenge_claims
            .iter()
            .filter(|(node, targets)| {
                present_nodes.contains(*node)
                    && targets
                        .iter()
                        .any(|target| configured_challenge_targets.contains(target))
            })
            .map(|(node, _)| node.clone()),
    );
    roots
}

fn orphan_nodes_for_parts(
    present_nodes: &BTreeSet<NodeId>,
    roots: &BTreeSet<NodeId>,
    deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> BTreeSet<NodeId> {
    let supported = cleanup_dep_closure(roots, present_nodes, deps);
    present_nodes
        .iter()
        .filter(|node| node.as_str() != PREAMBLE_NAME && !supported.contains(*node))
        .cloned()
        .collect()
}

fn post_response_orphan_nodes(
    input: &WorkerAcceptanceInput,
    normalized: &WorkerNormalizationOutput,
) -> BTreeSet<NodeId> {
    let present_nodes = &normalized.snapshot.present_nodes;
    let deps = deps_after_normalized_updates_for_present(input, normalized, present_nodes);
    let roots: BTreeSet<NodeId> = normalized
        .snapshot
        .coverage
        .values()
        .chain(normalized.snapshot.challenge_coverage.values())
        .flat_map(|nodes| nodes.iter().cloned())
        .collect();
    orphan_nodes_for_parts(present_nodes, &roots, &deps)
}

fn same_burst_orphan_nodes_before_deletion(
    input: &WorkerAcceptanceInput,
    normalized: &WorkerNormalizationOutput,
    removed_nodes: &BTreeSet<NodeId>,
) -> BTreeSet<NodeId> {
    let mut synthetic_present = normalized.snapshot.present_nodes.clone();
    synthetic_present.extend(removed_nodes.iter().cloned());
    let deps = deps_after_normalized_updates_for_present(input, normalized, &synthetic_present);
    let target_claims = apply_raw_target_claim_updates_for_present(
        &input.normalization.current_target_claims,
        &input.normalization.target_claim_updates,
        &synthetic_present,
        &input.normalization.configured_targets,
    );
    let configured_challenge_targets: BTreeSet<ChallengeTargetId> = input
        .normalization
        .configured_challenge_targets
        .keys()
        .cloned()
        .collect();
    let challenge_claims = apply_raw_challenge_claim_updates_for_present(
        &input.normalization.current_challenge_claims,
        &input.normalization.challenge_claim_updates,
        &synthetic_present,
        &configured_challenge_targets,
    );
    let roots = roots_from_claim_maps(
        &input.normalization.configured_targets,
        &target_claims,
        &configured_challenge_targets,
        &challenge_claims,
        &synthetic_present,
    );
    orphan_nodes_for_parts(&synthetic_present, &roots, &deps)
}

fn tablet_deletion_contract_errors(
    input: &WorkerAcceptanceInput,
    normalized: &WorkerNormalizationOutput,
) -> Vec<String> {
    if input.payload_outcome != WorkerOutcome::Valid {
        return Vec::new();
    }

    let removed_nodes: BTreeSet<NodeId> = input
        .normalization
        .current_present_nodes
        .difference(&normalized.snapshot.present_nodes)
        .cloned()
        .collect();
    let cleanup_worker = has_cleanup_validation_step(&input.validation_execution_plan)
        || has_final_cleanup_validation_step(&input.validation_execution_plan);
    let mut errors = Vec::new();

    if input.deleted_nodes != removed_nodes {
        errors.push(format!(
            "deleted_nodes must exactly match Tablet nodes removed on disk; declared {:?}, actual {:?}",
            input.deleted_nodes.iter().cloned().collect::<Vec<_>>(),
            removed_nodes.iter().cloned().collect::<Vec<_>>()
        ));
    }

    // Orphans that already existed in the PRE-burst baseline are not this
    // burst's to answer for. Historically every accepted state was
    // orphan-free, so "post-response orphans" and "orphans this burst
    // created" coincided; a Decide polarity flip breaks that (demoting the
    // pair's live side orphans its exclusive support cone with no worker
    // involved). dec2flt cycle 280: the flip left seven pre-existing
    // orphans and the Assumptions-only authoring burst — which may not
    // touch them — was unacceptable under the exact-equality rule. The
    // reviewer routes pre-existing orphans as ordinary work; a burst must
    // delete the orphans its own edits create, may also delete
    // pre-existing ones, and tolerates the rest.
    let pre_existing_orphans = {
        let configured_challenge: BTreeSet<ChallengeTargetId> = input
            .normalization
            .configured_challenge_targets
            .keys()
            .cloned()
            .collect();
        let roots = roots_from_claim_maps(
            &input.normalization.configured_targets,
            &input.normalization.current_target_claims,
            &configured_challenge,
            &input.normalization.current_challenge_claims,
            &input.normalization.current_present_nodes,
        );
        orphan_nodes_for_parts(
            &input.normalization.current_present_nodes,
            &roots,
            &input.normalization.current_deps,
        )
    };

    if !cleanup_worker {
        let same_burst_orphans =
            same_burst_orphan_nodes_before_deletion(input, normalized, &removed_nodes);
        let required: BTreeSet<NodeId> = same_burst_orphans
            .difference(&pre_existing_orphans)
            .cloned()
            .collect();
        let illegal: BTreeSet<NodeId> = removed_nodes
            .difference(&same_burst_orphans)
            .cloned()
            .collect();
        if !required.is_subset(&removed_nodes) || !illegal.is_empty() {
            errors.push(format!(
                "non-cleanup deleted_nodes must cover the orphans this burst created and contain only orphans; deleted {:?}, required (same-burst minus pre-existing) {:?}, pre-existing (tolerated) {:?}",
                removed_nodes.iter().cloned().collect::<Vec<_>>(),
                required.iter().cloned().collect::<Vec<_>>(),
                pre_existing_orphans.iter().cloned().collect::<Vec<_>>()
            ));
        }
    }

    let post_orphans = post_response_orphan_nodes(input, normalized);
    let new_post_orphans: BTreeSet<NodeId> = post_orphans
        .difference(&pre_existing_orphans)
        .cloned()
        .collect();
    if !new_post_orphans.is_empty() {
        errors.push(format!(
            "valid worker response leaves NEW live orphan nodes: {:?}",
            new_post_orphans.into_iter().collect::<Vec<_>>()
        ));
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
    let mut errors = Vec::new();
    if !illegal_target_claim_nodes.is_empty() {
        errors.push(format!(
            "proof-local/easy helper nodes may not claim paper targets: {:?}",
            illegal_target_claim_nodes
        ));
    }
    let illegal_challenge_claim_nodes: Vec<_> = new_nodes
        .iter()
        .filter(|node| {
            normalized
                .snapshot
                .challenge_coverage
                .values()
                .any(|nodes| nodes.contains(*node))
        })
        .cloned()
        .collect();
    if !illegal_challenge_claim_nodes.is_empty() {
        errors.push(format!(
            "proof-local/easy helper nodes may not claim challenge targets: {:?}",
            illegal_challenge_claim_nodes
        ));
    }
    errors
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

/// Parse + validate the worker's optional on-demand `audit_request`,
/// mirroring the reviewer normalizer (`normalize_review_response`):
/// `reason_kind` must be a known enum value, `reason` must be a non-empty
/// trimmed problem statement, and it is capped at
/// `AUDIT_TASK_REASON_MAX_CHARS`. The worker carrier is advisory and
/// carries no other action fields, so there is no worker-side mutual-
/// exclusion rule to enforce (unlike the reviewer's
/// global_repair_request mutex). Returns `Ok(None)` when the worker did
/// not request an audit.
fn parse_worker_audit_request(
    raw: Option<&crate::review_normalization::RawAuditRequest>,
) -> Result<Option<AuditRequest>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let reason_kind = AuditRequestReasonKind::parse(&raw.reason_kind).ok_or_else(|| {
        "audit_request.reason_kind must be one of ['approach', 'suspect_report']".to_string()
    })?;
    let reason = raw.reason.trim().to_string();
    if reason.is_empty() {
        return Err("audit_request.reason must be a non-empty problem statement".into());
    }
    if reason.chars().count() > AUDIT_TASK_REASON_MAX_CHARS {
        return Err(format!(
            "audit_request.reason must be at most {AUDIT_TASK_REASON_MAX_CHARS} characters"
        ));
    }
    Ok(Some(AuditRequest {
        reason_kind,
        reason,
    }))
}

pub fn accept_worker_response(
    input: &WorkerAcceptanceInput,
) -> Result<WorkerAcceptanceOutput, String> {
    let normalized = normalize_worker_response(&input.normalization)?;
    let audit_request = parse_worker_audit_request(input.audit_request.as_ref())?;
    let memory_challenges =
        crate::process_memory::parse_memory_challenges(&input.memory_challenges)?;
    let cleanup_worker = has_cleanup_validation_step(&input.validation_execution_plan)
        || has_final_cleanup_validation_step(&input.validation_execution_plan);
    let mut contract_errors = normalized.contract_errors.clone();
    contract_errors.extend(cleanup_contract_errors(input, &normalized));
    contract_errors.extend(final_cleanup_contract_errors(input, &normalized));
    contract_errors.extend(tablet_deletion_contract_errors(input, &normalized));
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
        // PV under-model (Slice 1): like Stuck/NR, the worker's verdict signal
        // reaches the reviewer untranslated (the worker authored no tablet
        // edit). Reclassified to Invalid only if a cleanup worker emitted it
        // with contract errors (illegal in cleanup, same as Stuck/NR).
        WorkerOutcome::TargetFalseUnderModel => {
            if cleanup_worker && !contract_errors.is_empty() {
                (WorkerOutcome::Invalid, false, contract_errors.clone())
            } else {
                (WorkerOutcome::TargetFalseUnderModel, true, Vec::new())
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
            challenge_claim_updates: normalized.challenge_claim_updates,
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
            deleted_nodes: if final_outcome == WorkerOutcome::Valid {
                input.deleted_nodes.clone()
            } else {
                BTreeSet::new()
            },
            protected_semantic_change_nodes: if final_outcome == WorkerOutcome::Valid {
                input.protected_semantic_change_nodes.clone()
            } else {
                BTreeSet::new()
            },
            local_closure_results,
            // On-demand "call for an audit" (advisory). Explicitly set
            // rather than left to `..WorkerResponse::default()` (= None):
            // the validator extracts the field and `CheckedWorkerPayload`
            // forwards it here, so it must survive onto the response and
            // thence to `record_latest_worker_rationale`. Legal on any
            // outcome (non-blocking on Valid).
            audit_request,
            // Process memory challenges: same allowlist-strip discipline
            // as `audit_request` above — explicitly set so the validated
            // list survives onto the response and thence to
            // `record_latest_worker_rationale`. Legal on any outcome.
            memory_challenges,
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
            let is_assumption_authoring_node = active_node.as_str() == ASSUMPTIONS_NAME
                && input
                    .assumption_authoring_node
                    .as_ref()
                    .is_some_and(|node| node == active_node);
            if input.observation_plan.capture_expected_active_hash && !is_assumption_authoring_node
            {
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

fn is_structural_declaration_hash_node(node_name: &str) -> bool {
    node_name == PREAMBLE_NAME || node_name == AXIOMS_NAME
}

fn whole_file_declaration_hash(content: &str) -> String {
    format!(
        "{WHOLE_FILE_DECLARATION_HASH_PREFIX}{}",
        hash_bytes(content.as_bytes())
    )
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

/// The node-source file path for `node` under `target`'s backend: `.lean` for
/// Lean (identical to [`node_lean_path`]), `.thy` for Isabelle/HOL. Reads the
/// extension from the backend descriptor (`node_source_ext`) so node discovery
/// finds an Isabelle tablet's `Tablet/<Node>.thy` files. For `target == Lean`
/// the extension is `"lean"`, making this byte-identical to `node_lean_path`.
///
/// `pub` so the node-eval routing in `runtime_cli_observations`
/// (`observe_node`, the per-node probe re-read, the owner-scan exists-check,
/// the noderef-closure leaf checks) can select the source path per target —
/// the R4 `.thy` node-evaluation routing. That module is `#[path]`-included
/// into the `runtime_cli` bin (a separate crate), which reaches it as
/// `trellis_kernel::node_source_path`, so full `pub` (not `pub(crate)`) is
/// required — matching the `tablet_target_for_repo` visibility it sits beside.
/// On the all-Lean live run `target` resolves to `Lean`, so the returned path
/// is byte-identical to the historical hardcoded `Tablet/<node>.lean`.
pub fn node_source_path(
    repo_path: &Path,
    node: &str,
    target: crate::backend::BackendId,
) -> PathBuf {
    let ext = crate::backend::descriptor_for(target).node_source_ext;
    tablet_dir(repo_path).join(format!("{node}.{ext}"))
}

/// Resolve the tablet-wide backend target for `repo_path` from the repo's
/// `trellis.config.json` (`workflow.default_target`), so the repo-keyed call
/// sites — node discovery here, and the deep checker-op transports in
/// `tablet_support`/`runtime_cli_observations`, all of which carry `repo_path`
/// but not `ProtocolState` — can route per-target.
///
/// This is the repo-keyed analogue of `ProtocolState::effective_node_target`.
/// `node_target` per-node overrides are never populated in this phase (the map
/// is always empty), so `effective_node_target(node) ≡ tablet_target` for every
/// node, and the tablet-wide target read here is exactly that resolution.
///
/// **Byte-identical for Lean.** Any error — file absent (the common case for
/// fixtures / smoke repos), malformed JSON, missing/empty `workflow` or
/// `default_target`, or an unrecognized value — resolves to `Lean`. Only an
/// explicit `"isabelle_hol"` resolves to `IsabelleHol`. So on the all-Lean live
/// run every routed site selects the Lean driver / `.lean` source path,
/// preserving the historical hardcoded behavior exactly. Mirrors the
/// `local_closure_axcheck_enabled_for_repo` config-read convention (legacy
/// `lagent.config.json` fallback) in `runtime_cli_observations.rs` and the
/// `workflow.default_target` parse in `bin/runtime_cli.rs::target_from_config`.
pub fn tablet_target_for_repo(repo_path: &Path) -> crate::backend::BackendId {
    let trellis_path = repo_path.join("trellis.config.json");
    let candidate = if trellis_path.is_file() {
        trellis_path
    } else {
        let legacy = repo_path.join("lagent.config.json");
        if legacy.is_file() {
            legacy
        } else {
            return crate::backend::BackendId::Lean;
        }
    };
    let Ok(text) = fs::read_to_string(&candidate) else {
        return crate::backend::BackendId::Lean;
    };
    let Ok(raw) = serde_json::from_str::<serde_json::Value>(&text) else {
        return crate::backend::BackendId::Lean;
    };
    let target_str = raw
        .as_object()
        .and_then(|obj| obj.get("workflow"))
        .and_then(serde_json::Value::as_object)
        .and_then(|workflow| workflow.get("default_target"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match target_str {
        Some(value) => serde_json::from_value::<crate::backend::BackendId>(
            serde_json::Value::String(value.to_string()),
        )
        .unwrap_or(crate::backend::BackendId::Lean),
        None => crate::backend::BackendId::Lean,
    }
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
    // The node-source extension is the target's (`lean` for the Lean live run,
    // `thy` for Isabelle/HOL), so an Isabelle tablet's `Tablet/<Node>.thy`
    // files are discovered. For Lean this is `"lean"` ⇒ byte-identical to the
    // historical scan. The `.tex` statement file is backend-agnostic (both
    // backends carry the LaTeX statement in `Tablet/<Node>.tex`), so it is
    // matched for both targets.
    let source_ext =
        crate::backend::descriptor_for(tablet_target_for_repo(repo_path)).node_source_ext;
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
            Some(ext) if ext == source_ext && stem != AXIOMS_NAME => {
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

/// Classify a node's kind from its name + `.tex` content. The exact logic
/// formerly inline in `node_kinds_from_repo`'s closure, extracted verbatim
/// so the trait (`backend::SourceModel::classify_declaration`) and
/// `node_kinds_from_repo` share one body.
///
/// The `Preamble` name is special-cased; otherwise the classification keys
/// off the **B** `tex_statement_environment` (the naive, non-nesting-aware
/// `find("\\begin{")` scan in THIS module), NOT the nesting-aware
/// `filespec::tex_statement_environment` (A). Preserving B here is
/// load-bearing: A and B diverge on nested/malformed `\begin{...}` (pinned
/// by `tests::classify_node_kind_uses_naive_b_not_top_level_a`).
pub(crate) fn classify_node_kind_from_tex(node: &str, tex_content: &str) -> NodeKind {
    if node == PREAMBLE_NAME {
        NodeKind::Preamble
    } else {
        let env = tex_statement_environment(tex_content);
        if PROOF_BEARING_ENVS.contains(&env.as_str()) {
            NodeKind::Proof
        } else {
            NodeKind::Definition
        }
    }
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
                classify_node_kind_from_tex(node, &read_text(&node_tex_path(repo_path, node)))
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
    // Route the node-source path + open-marker scan per target. For the Lean
    // live run `target == Lean` ⇒ `.lean` path + `has_sorry` scan, byte-
    // identical to the historical hardcoded path; for Isabelle it reads
    // `Tablet/<Node>.thy` and the Isabelle `is_node_open` (live `sorry`).
    let target = tablet_target_for_repo(repo_path);
    let model = crate::backend::source_model_for(target);
    present_nodes
        .iter()
        .filter(|node| {
            let source_path = node_source_path(repo_path, node, target);
            if !source_path.exists() {
                return true;
            }
            model.is_node_open(&read_text(&source_path))
        })
        .cloned()
        .collect()
}

pub fn direct_deps_from_repo(
    repo_path: &Path,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<NodeId, BTreeSet<NodeId>> {
    // Route the node-source path + intra-tablet import scan per target. For the
    // Lean live run `target == Lean` ⇒ `.lean` path + `import Tablet.X` scan,
    // byte-identical; for Isabelle it reads `Tablet/<Node>.thy` and the
    // Isabelle `tablet_imports` (the `\noderef`/session-import shape).
    let target = tablet_target_for_repo(repo_path);
    let model = crate::backend::source_model_for(target);
    present_nodes
        .iter()
        .map(|node| {
            let deps = model
                .tablet_imports(&read_text(&node_source_path(repo_path, node, target)))
                .into_iter()
                .filter(|dep| dep != node)
                .collect();
            (node.clone(), deps)
        })
        .collect()
}

pub(crate) fn extract_tablet_imports(lean_content: &str) -> Vec<NodeId> {
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

/// Whole-file "does this node still contain a live `sorry`?" scan. Masks
/// comments and strings, then unmasks the `macro_rules | `(tactic| sorry)
/// => …` rewrite (which turns the literal token into a real proof body, so
/// the file is functionally sorry-free). Pure text; no prover. Feeds
/// `open_nodes_from_repo` → the contract's `openNodes` → phase advance.
pub(crate) fn has_sorry(lean_content: &str) -> bool {
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
    "structure ",
    "inductive ",
    "class ",
    "instance ",
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
    if is_structural_declaration_hash_node(node_name) {
        return Ok(whole_file_declaration_hash(content));
    }
    Ok(declaration_hash(content, node_name))
}

#[cfg(not(test))]
fn declaration_hash_for_gate(
    repo_path: &Path,
    content: &str,
    node_name: &str,
) -> Result<String, String> {
    if is_structural_declaration_hash_node(node_name) {
        return Ok(whole_file_declaration_hash(content));
    }
    crate::backend::source_model_for(crate::backend::BackendId::Lean)
        .signature_hash(repo_path, content, node_name)
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

fn normalize_challenge_claim_updates(
    raw_updates: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    current_challenge_claims: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    present_nodes: &BTreeSet<NodeId>,
    configured_challenge_targets: &BTreeSet<ChallengeTargetId>,
    force_nodes: &BTreeSet<NodeId>,
) -> ChallengeClaimUpdates {
    raw_updates
        .iter()
        .filter_map(|(node, targets)| {
            if !present_nodes.contains(node) {
                return None;
            }
            let normalized: BTreeSet<_> = targets
                .iter()
                .filter(|target| configured_challenge_targets.contains(*target))
                .cloned()
                .collect();
            let current = current_challenge_claims
                .get(node)
                .cloned()
                .unwrap_or_default();
            if !force_nodes.contains(node) && normalized == current {
                None
            } else {
                Some((node.clone(), Update::Set(normalized)))
            }
        })
        .collect()
}

pub(crate) fn apply_challenge_claim_updates(
    base: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    updates: &ChallengeClaimUpdates,
    present_nodes: &BTreeSet<NodeId>,
    configured_challenge_targets: &BTreeSet<ChallengeTargetId>,
) -> BTreeMap<NodeId, BTreeSet<ChallengeTargetId>> {
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
                    .filter(|target| configured_challenge_targets.contains(target))
                    .collect(),
            };
            (node.clone(), next)
        })
        .collect()
}

pub(crate) fn challenge_coverage_from_claims(
    configured_challenge_targets: &BTreeSet<ChallengeTargetId>,
    challenge_claims: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<ChallengeTargetId, BTreeSet<NodeId>> {
    configured_challenge_targets
        .iter()
        .map(|target| {
            let covered = present_nodes
                .iter()
                .filter(|node| {
                    challenge_claims
                        .get(*node)
                        .is_some_and(|targets| targets.contains(target))
                })
                .cloned()
                .collect();
            (target.clone(), covered)
        })
        .collect()
}

/// Contract checks on the post-update challenge claim map: explicit
/// updates for new nodes (only while challenge targets are configured,
/// so paper-only runs see no new requirement) and claim exclusivity
/// (at most one challenge target per node).
fn challenge_claim_contract_errors(
    input: &WorkerNormalizationInput,
    new_nodes: &BTreeSet<NodeId>,
    next_challenge_claims: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
) -> Vec<String> {
    let mut errors = Vec::new();
    if !input.configured_challenge_targets.is_empty() {
        let missing_new_claims: Vec<_> = new_nodes
            .iter()
            .filter(|node| !input.challenge_claim_updates.contains_key(*node))
            .cloned()
            .collect();
        if !missing_new_claims.is_empty() {
            errors.push(format!(
                "worker must explicitly report challenge_claim_updates for every new node (use [] when empty): {:?}",
                missing_new_claims
            ));
        }
    }
    let multi_target_nodes: Vec<_> = next_challenge_claims
        .iter()
        .filter(|(_, targets)| targets.len() > 1)
        .map(|(node, _)| node.clone())
        .collect();
    if !multi_target_nodes.is_empty() {
        errors.push(format!(
            "a challenge claim is exclusive: a node may not claim multiple challenge targets: {:?}",
            multi_target_nodes
        ));
    }
    errors
}

/// Quote a line for a rejection reason, making absence explicit.
fn quoted_or_absent(line: Option<&str>) -> String {
    match line {
        Some(line) => format!("`{line}`"),
        None => "(no line)".to_string(),
    }
}

/// Kernel-owned byte-for-byte challenge conformance check, run at
/// every worker acceptance in every mode (`proof_coarse_restructure`
/// included — challenge conformance has no ProtectedReapproval
/// escape). For each present node claiming a challenge target:
///   - name parity: the node (and therefore, by FILESPEC's
///     one-principal-declaration rule, its declaration) is named
///     exactly the target's prescribed `name`;
///   - theorem: the FILESPEC slice between the `-- [TABLET NODE: ...]`
///     marker line and the `-- BODY` line, surrounding blank lines
///     trimmed, byte-equals the prescribed text (imports above the
///     marker stay free);
///   - def: the same slice extended through end of file (the `-- BODY`
///     marker line itself excluded — it is kernel-owned scaffolding,
///     not prescription), byte-equals the prescribed text.
/// The prescribed text is canonical at import time, so no
/// normalization happens here.
fn challenge_conformance_errors(
    repo_path: &Path,
    configured_challenge_targets: &BTreeMap<ChallengeTargetId, ChallengeTargetSpec>,
    next_challenge_claims: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    present_nodes: &BTreeSet<NodeId>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (node, claims) in next_challenge_claims {
        if !present_nodes.contains(node) {
            continue;
        }
        for target in claims {
            let Some(spec) = configured_challenge_targets.get(target) else {
                continue;
            };
            // The node stem is the FILESPEC-legal form of the prescribed decl
            // name: namespace dots are sanitized to underscores
            // (`FiniteDecimal.value` -> node `FiniteDecimal_value`), because a
            // dot-bearing stem is illegal under FILESPEC. The byte-conformance
            // and namespace-context checks below pin the ACTUAL declaration and
            // its namespace scoping, so here we only verify the stem corresponds
            // to the prescribed name.
            let expected_stem = spec.name.replace('.', "_");
            if node.as_str() != expected_stem {
                errors.push(format!(
                    "challenge name-parity rule failed: node `{node}` claims challenge target `{target}`, whose prescribed declaration name is `{name}` (FILESPEC node stem `{expected_stem}`); only a node named `{expected_stem}` may claim it",
                    name = spec.name,
                ));
                continue;
            }
            let content = match crate::filespec_split::read_node_file(repo_path, node.as_str()) {
                Ok((content, _hash)) => content,
                Err(err) => {
                    errors.push(format!(
                        "challenge byte-conformance rule failed: node `{node}` claims challenge target `{target}` but its tablet file could not be read: {err}"
                    ));
                    continue;
                }
            };
            let include_body = spec.kind == ChallengeTargetKind::Def;
            let actual = match crate::filespec_split::prescribed_region(&content, include_body) {
                Ok(region) => region,
                Err(err) => {
                    errors.push(format!(
                        "challenge byte-conformance rule failed: node `{node}` claims challenge target `{target}` but its FILESPEC slice could not be computed: {err}"
                    ));
                    continue;
                }
            };
            if actual != spec.lean {
                let (line_no, expected_line, actual_line) =
                    first_differing_line(&spec.lean, &actual);
                let scope = match spec.kind {
                    ChallengeTargetKind::Theorem => {
                        "the slice between the tablet-node marker and `-- BODY`"
                    }
                    ChallengeTargetKind::Def => {
                        "the slice from the tablet-node marker through end of file"
                    }
                };
                errors.push(format!(
                    "challenge byte-conformance rule failed for node `{node}` claiming challenge target `{target}` ({kind}): {scope} must byte-equal the prescribed text in every mode, proof_coarse_restructure included. First difference at line {line_no}: expected {expected}, actual {actual}",
                    kind = match spec.kind {
                        ChallengeTargetKind::Theorem => "theorem",
                        ChallengeTargetKind::Def => "def",
                    },
                    expected = quoted_or_absent(expected_line.as_deref()),
                    actual = quoted_or_absent(actual_line.as_deref()),
                ));
            }
            // Namespace-context byte-pin (PV ExtractionModel identity). For a
            // target whose extractor pinned a `namespace_context` (Aeneas model
            // nodes; empty for plain math targets ⇒ this block is inert and the
            // all-math path stays byte-identical), the node file's preamble
            // namespace context (the `namespace …`/`end …` lines that scope the
            // principal declaration) must byte-equal the pinned context. This
            // closes the structural hole where the byte-pinned slice covers the
            // decl's VALUE but not its fully-qualified NAME: stripping
            // `namespace ntt_montgomery` silently retargets trusted
            // `ntt_montgomery.FIELD_MODULUS` to top-level `FIELD_MODULUS`.
            // `import`/`open`/`set_option` preamble lines stay free (see
            // `filespec_split::namespace_context`), so adding an `open` or import
            // is NOT rejected; altering/stripping the `namespace` IS. A mismatch
            // is a flat `contract_error` (deterministic worker reject, every
            // mode, no HumanGate).
            // Normalise BOTH sides with the same `namespace`/`end`-only
            // extraction before the byte-compare. The node side comes through
            // `namespace_context` (marker-aware preamble scan); the pinned spec
            // value, which a configurer may write as a full preamble (e.g.
            // `"open Aeneas …\nnamespace ntt_montgomery"`), is run through the
            // marker-free `namespace_context_from_preamble`. Comparing
            // namespace-only-vs-namespace-only is exactly the "opens free,
            // namespace pinned" policy; without it a full-preamble pin can never
            // byte-equal the extractor's namespace-only value and the target is
            // permanently unsatisfiable. An empty pin still short-circuits above
            // (block inert ⇒ all-math path byte-identical).
            let pinned_ctx =
                crate::filespec_split::namespace_context_from_preamble(&spec.namespace_context);
            if !pinned_ctx.trim().is_empty() {
                match crate::filespec_split::namespace_context(&content) {
                    Ok(actual_ctx) => {
                        if actual_ctx != pinned_ctx {
                            errors.push(format!(
                                "challenge namespace-context rule failed for node `{node}` claiming challenge target `{target}`: the node's preamble namespace context (the `namespace …`/`end …` lines scoping the declaration) must byte-equal the extractor-pinned namespace context in every mode; `import`/`open`/`set_option` lines stay free. Expected {expected:?}, actual {actual:?}",
                                expected = pinned_ctx,
                                actual = actual_ctx,
                            ));
                        }
                    }
                    Err(err) => {
                        errors.push(format!(
                            "challenge namespace-context rule failed: node `{node}` claims challenge target `{target}` but its preamble namespace context could not be computed: {err}"
                        ));
                    }
                }
            }
        }
    }
    errors
}

/// PV Phase 2 (Slice 1+2): the extraction-chain acceptance gate, a sibling to
/// `challenge_conformance_errors` (the byte-pin) that runs at every worker
/// acceptance. The byte-pin proves the model TEXT matches the prescription; this
/// gate proves the model carries a complete EXTRACTION CHAIN provenance — the
/// `source_sha256` of the Rust it was extracted from AND the
/// `extractor_toolchain_sha256` of the toolchain that produced it.
///
/// FAIL-CLOSED discipline (the revision-mode lesson): for each PRESENT node that
/// is an ExtractionModel node (`extraction_model_nodes`) AND claims a challenge
/// target, the claimed target's provenance MUST carry a non-empty
/// `source_sha256` and a non-empty `extractor_toolchain_sha256`; an empty value
/// is REJECTED (never silently accepted). A node that is not an ExtractionModel
/// node, or a claim on a target with no provenance, is untouched — so a plain
/// math challenge target (no toolchain) is never gated, and the all-math path
/// is byte-identical (`extraction_model_nodes` empty ⇒ no iterations).
fn extraction_chain_errors(
    configured_challenge_targets: &BTreeMap<ChallengeTargetId, ChallengeTargetSpec>,
    next_challenge_claims: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    present_nodes: &BTreeSet<NodeId>,
    extraction_model_nodes: &BTreeSet<NodeId>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (node, claims) in next_challenge_claims {
        if !present_nodes.contains(node) || !extraction_model_nodes.contains(node) {
            continue;
        }
        for target in claims {
            let Some(spec) = configured_challenge_targets.get(target) else {
                continue;
            };
            if spec.provenance.source_sha256.trim().is_empty() {
                errors.push(format!(
                    "extraction-chain rule failed: ExtractionModel node `{node}` claims challenge target `{target}` whose provenance carries an empty `source_sha256`; an extraction model must record the source it was extracted from (the source fingerprint that drives source→model reopen)"
                ));
            }
            if spec.provenance.extractor_toolchain_sha256.trim().is_empty() {
                errors.push(format!(
                    "extraction-chain rule failed: ExtractionModel node `{node}` claims challenge target `{target}` whose provenance carries an empty `extractor_toolchain_sha256`; an extraction model must record the toolchain that produced it (pin `pv_tablet.extractor_stack` + `extraction_toolchain`)"
                ));
            }
        }
    }
    errors
}

/// Challenge-target DELETION GUARD: a per-worker-acceptance invariant
/// (sibling to `challenge_conformance_errors` / `extraction_chain_errors`)
/// that protects the EXISTENCE of every configured challenge target — the
/// byte-pin protects a covering node's TEXT and NAMESPACE; this protects
/// the fact that SOME node still covers the target.
///
/// The byte-pin alone cannot catch a deletion: when a worker removes the
/// covering node (or orphan-cleans it), the node leaves `present_nodes`,
/// its challenge claim is dropped, and `challenge_conformance_errors`
/// (which only iterates over PRESENT nodes that still CLAIM a target)
/// never fires for it — the contract is simply gone. `ChallengeCoverage`
/// blockers detect this, but only at AdvancePhase/Done (`global_blockers`),
/// long after the deleting worker result was already accepted `valid`.
///
/// This gate closes that timing hole: for each configured challenge
/// target that was COVERED before the burst (some present node claimed
/// it in the committed state) but is UNCOVERED after the response's
/// claim/present updates are applied (empty `challenge_coverage` set), the
/// result is a flat `contract_error` REJECT in every mode — the same
/// deterministic worker-reject path as `challenge_conformance_errors`,
/// with no HumanGate and no ProtectedReapproval escape.
///
/// A legitimate rename/replacement (delete node `A`, add node `B` named
/// after the same target and claiming it) keeps the target's coverage set
/// non-empty, so it is ALLOWED — the guard protects coverage, not any
/// particular node id. A target that was already uncovered before the
/// burst (e.g. theorem-stating before the covering node is first placed)
/// is not regressed by this result, so it does not fire here — that
/// not-yet-covered state stays the province of the AdvancePhase
/// `ChallengeCoverage` blocker.
///
/// Gated on prior coverage being non-empty AND
/// `configured_challenge_targets` being non-empty, so an all-math run
/// (empty registry ⇒ no iterations) accepts byte-identically.
fn challenge_deletion_guard_errors(
    configured_challenge_targets: &BTreeMap<ChallengeTargetId, ChallengeTargetSpec>,
    current_challenge_claims: &BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    current_present_nodes: &BTreeSet<NodeId>,
    next_challenge_coverage: &BTreeMap<ChallengeTargetId, BTreeSet<NodeId>>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for target in configured_challenge_targets.keys() {
        let covered_before = current_challenge_claims
            .iter()
            .any(|(node, claims)| current_present_nodes.contains(node) && claims.contains(target));
        if !covered_before {
            continue;
        }
        let covered_after = next_challenge_coverage
            .get(target)
            .map(|nodes| !nodes.is_empty())
            .unwrap_or(false);
        if !covered_after {
            errors.push(format!(
                "challenge deletion-guard rule failed: this result leaves configured challenge target `{target}` UNCOVERED — its covering node was removed/orphan-deleted (or its challenge claim dropped) with no other node covering it. A pinned challenge target may never be deleted: keep its covering node present and claiming the target, or replace it with another node that reproduces the prescribed declaration and claims the target. Being un-imported is the normal shape of a goal root; do not orphan-clean it"
            ));
        }
    }
    errors
}

/// Locate the first line where `expected` and `actual` diverge.
/// Returns the 1-based line number plus both lines (None for a side
/// that has no line there).
fn first_differing_line(expected: &str, actual: &str) -> (usize, Option<String>, Option<String>) {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let max = expected_lines.len().max(actual_lines.len());
    for idx in 0..max {
        let e = expected_lines.get(idx);
        let a = actual_lines.get(idx);
        if e != a {
            return (idx + 1, e.map(|s| s.to_string()), a.map(|s| s.to_string()));
        }
    }
    (max.max(1), None, None)
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
    use crate::model::CleanupReplacement;
    use std::io::Write;
    use tempfile::tempdir;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    /// Step-4 open-node masking pin. The textual open-node decision feeds
    /// the contract's `openNodes` (phase advance), so the four masking
    /// behaviors of the wrapped `has_sorry` must be preserved exactly:
    ///   * a live `:= by sorry` => OPEN
    ///   * the `macro_rules | `(tactic| sorry) => …` rewrite masks it => CLOSED
    ///   * a `sorry` inside a `/- … -/` block comment is masked => CLOSED
    ///   * a `sorry` inside a `-- …` line comment is masked => CLOSED
    /// Each is asserted via the trait seam (`is_node_open`) AND via
    /// `open_nodes_from_repo` over a temp repo (which routes through the
    /// seam), plus the file-missing => OPEN rule. `direct_deps_from_repo`'s
    /// intra-tablet import edges + `dep != node` self-filter are pinned too.
    #[test]
    fn open_node_masking_and_import_filtering_pin() {
        let model = crate::backend::lean_source_model();

        // 1. Live sorry => open.
        let live = "import Tablet.Preamble\ntheorem Foo : True := by\n  sorry\n";
        assert!(model.is_node_open(live));

        // 2. macro_rules sorry rewrite masks the token => closed.
        let masked_macro = "\
import Tablet.Preamble
local macro_rules | `(tactic| sorry) => `(tactic| trivial)
theorem Foo : True := by
  sorry
";
        assert!(!model.is_node_open(masked_macro));

        // 3. Block-comment sorry => closed.
        let block_comment = "\
import Tablet.Preamble
/- this proof avoids sorry entirely -/
theorem Foo : True := by
  trivial
";
        assert!(!model.is_node_open(block_comment));

        // 4. Line-comment sorry => closed.
        let line_comment = "\
import Tablet.Preamble
-- TODO replace the sorry placeholder later
theorem Foo : True := by
  trivial
";
        assert!(!model.is_node_open(line_comment));

        // End-to-end over a temp repo: open_nodes_from_repo routes through
        // the seam, and a node with no .lean file is open.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("Tablet/Open.lean"), live);
        write(&repo.join("Tablet/Closed.lean"), block_comment);
        // `Missing` is present in the set but has no file on disk.
        let present: BTreeSet<NodeId> = ["Open", "Closed", "Missing"]
            .into_iter()
            .map(NodeId::from)
            .collect();
        let open = open_nodes_from_repo(&repo, &present);
        assert!(open.contains(&NodeId::from("Open")));
        assert!(open.contains(&NodeId::from("Missing")));
        assert!(!open.contains(&NodeId::from("Closed")));

        // tablet_imports + the dep != node self-filter in direct_deps_from_repo.
        let self_importing = "\
import Tablet.Preamble
import Tablet.Dep1
import Tablet.SelfNode
import Mathlib.Data.Nat.Basic
theorem SelfNode : True := by
-- BODY
  trivial
";
        // Raw seam extraction keeps the self-import and the Tablet deps,
        // drops the Mathlib import (only `import Tablet.X` counts).
        let raw = model.tablet_imports(self_importing);
        assert_eq!(
            raw,
            vec![
                NodeId::from("Preamble"),
                NodeId::from("Dep1"),
                NodeId::from("SelfNode"),
            ]
        );
        write(&repo.join("Tablet/SelfNode.lean"), self_importing);
        write(
            &repo.join("Tablet/Dep1.lean"),
            "import Tablet.Preamble\ntheorem Dep1 : True := by\n-- BODY\n  trivial\n",
        );
        let deps = direct_deps_from_repo(
            &repo,
            &["SelfNode", "Dep1"].into_iter().map(NodeId::from).collect(),
        );
        // SelfNode's own deps exclude itself (dep != node filter) but keep
        // Preamble + Dep1.
        let self_deps = &deps[&NodeId::from("SelfNode")];
        assert!(self_deps.contains(&NodeId::from("Preamble")));
        assert!(self_deps.contains(&NodeId::from("Dep1")));
        assert!(!self_deps.contains(&NodeId::from("SelfNode")));
    }

    /// Step-3 divergence pin. `classify_node_kind_from_tex` keys off the
    /// **naive (B)** `tex_statement_environment` in THIS module (first
    /// `\begin{...}` in the match set at ANY nesting depth), NOT the
    /// nesting-aware filespec (A) `tex_statement_environment` (top-level
    /// envs only). On a `\begin{theorem}` nested inside a non-matching
    /// top-level `\begin{remark}`, the two diverge: B sees the nested
    /// theorem and returns "theorem"; A sees only the top-level remark
    /// (not in the set) and returns "". The kernel's node-kind
    /// classification must follow B, so this node classifies as `Proof`.
    /// If `classify_declaration` ever (wrongly) switched to A it would
    /// classify as `Definition` and this test would fail.
    #[test]
    fn classify_node_kind_uses_naive_b_not_top_level_a() {
        let tex = "\
\\begin{remark}
\\begin{theorem}
The nested theorem.
\\end{theorem}
\\end{remark}
";
        // B (this module's private fn): finds the nested theorem.
        assert_eq!(super::tex_statement_environment(tex), "theorem");
        // A (filespec, nesting-aware): sees only the top-level remark,
        // which is not in the match set, so returns empty.
        assert_eq!(crate::filespec::tex_statement_environment(tex), "");
        // The classifier must follow B -> Proof.
        assert_eq!(
            classify_node_kind_from_tex("SomeNode", tex),
            NodeKind::Proof
        );
        // And the trait seam must agree (it delegates to the same helper).
        assert_eq!(
            crate::backend::lean_source_model().classify_declaration("SomeNode", tex),
            NodeKind::Proof
        );
        // Sanity: Preamble name short-circuits regardless of tex content.
        assert_eq!(
            classify_node_kind_from_tex("Preamble", tex),
            NodeKind::Preamble
        );
    }

    fn write_supported_orphan_fixture(repo: &Path, a_imports_b: bool, include_b_on_disk: bool) {
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let a_imports = if a_imports_b {
            "import Tablet.Preamble\nimport Tablet.B\n"
        } else {
            "import Tablet.Preamble\n"
        };
        write(
            &repo.join("Tablet/A.lean"),
            &format!("{a_imports}\ntheorem A : True := by\n  trivial\n"),
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        if include_b_on_disk {
            write(
                &repo.join("Tablet/B.lean"),
                "import Tablet.Preamble\n\ndef B : Nat := 0\n",
            );
            write(
                &repo.join("Tablet/B.tex"),
                "\\begin{definition}B\\end{definition}\n",
            );
        }
    }

    fn rooted_ab_normalization(repo: PathBuf) -> WorkerNormalizationInput {
        WorkerNormalizationInput {
            repo_path: repo,
            configured_targets: BTreeSet::from([TargetId::from("t.a")]),
            current_present_nodes: BTreeSet::from([
                NodeId::from("Preamble"),
                NodeId::from("A"),
                NodeId::from("B"),
            ]),
            current_proof_nodes: BTreeSet::from([NodeId::from("A")]),
            current_node_kinds: BTreeMap::from([
                (NodeId::from("Preamble"), NodeKind::Preamble),
                (NodeId::from("A"), NodeKind::Proof),
                (NodeId::from("B"), NodeKind::Definition),
            ]),
            current_deps: BTreeMap::from([
                (
                    NodeId::from("A"),
                    BTreeSet::from([NodeId::from("Preamble"), NodeId::from("B")]),
                ),
                (
                    NodeId::from("B"),
                    BTreeSet::from([NodeId::from("Preamble")]),
                ),
            ]),
            current_target_claims: BTreeMap::from([(
                NodeId::from("A"),
                BTreeSet::from([TargetId::from("t.a")]),
            )]),
            ..WorkerNormalizationInput::default()
        }
    }

    fn node_set(nodes: &[&str]) -> BTreeSet<NodeId> {
        nodes.iter().map(|node| NodeId::from(*node)).collect()
    }

    fn final_cleanup_substitution_step(target: &str) -> WorkerValidationExecutionPlanStep {
        WorkerValidationExecutionPlanStep::FinalCleanupPreserving {
            task_kind: Some(CleanupTaskKind::Substitution {
                replacement: CleanupReplacement::Mathlib {
                    citation: "Nat.add_comm".to_string(),
                },
            }),
            target_node: Some(NodeId::from(target)),
            authorized_nodes: BTreeSet::new(),
            protected_statement_node_set: BTreeSet::new(),
        }
    }

    fn final_cleanup_contract_errors_for_proof_update(
        current_present_nodes: &[&str],
        next_present_nodes: &[&str],
        deleted_nodes: &[&str],
        target_node: &str,
        update_node: &str,
        update: Update<bool>,
    ) -> Vec<String> {
        let input = WorkerAcceptanceInput {
            payload_outcome: WorkerOutcome::Valid,
            deleted_nodes: node_set(deleted_nodes),
            normalization: WorkerNormalizationInput {
                current_present_nodes: node_set(current_present_nodes),
                ..WorkerNormalizationInput::default()
            },
            validation_execution_plan: vec![final_cleanup_substitution_step(target_node)],
            ..WorkerAcceptanceInput::default()
        };
        let normalized = WorkerNormalizationOutput {
            snapshot: WorkingSnapshot {
                present_nodes: node_set(next_present_nodes),
                ..WorkingSnapshot::default()
            },
            proof_node_updates: BTreeMap::from([(NodeId::from(update_node), update)]),
            ..WorkerNormalizationOutput::default()
        };
        final_cleanup_contract_errors(&input, &normalized)
    }

    #[test]
    fn final_cleanup_substitution_accepts_deleted_target_proof_node_clear() {
        let errors = final_cleanup_contract_errors_for_proof_update(
            &["Preamble", "Target"],
            &["Preamble"],
            &["Target"],
            "Target",
            "Target",
            Update::Set(false),
        );

        assert!(
            errors.is_empty(),
            "expected deleted target proof clear to pass, got {:?}",
            errors
        );
    }

    #[test]
    fn final_cleanup_rejects_proof_node_clear_for_non_deleted_node() {
        let errors = final_cleanup_contract_errors_for_proof_update(
            &["Preamble", "Target"],
            &["Preamble", "Target"],
            &["Target"],
            "Target",
            "Target",
            Update::Set(false),
        );

        assert!(
            errors
                .iter()
                .any(|err| err.contains("final cleanup may not change proof-node classification")),
            "expected proof-node classification rejection, got {:?}",
            errors
        );
    }

    #[test]
    fn final_cleanup_rejects_deleted_proof_node_clear_when_not_substitution_target() {
        let errors = final_cleanup_contract_errors_for_proof_update(
            &["Preamble", "Target", "Other"],
            &["Preamble", "Target"],
            &["Other"],
            "Target",
            "Other",
            Update::Set(false),
        );

        assert!(
            errors
                .iter()
                .any(|err| err.contains("final cleanup may not change proof-node classification")),
            "expected proof-node classification rejection, got {:?}",
            errors
        );
    }

    #[test]
    fn final_cleanup_rejects_deleted_proof_node_clear_when_not_declared() {
        let errors = final_cleanup_contract_errors_for_proof_update(
            &["Preamble", "Target"],
            &["Preamble"],
            &[],
            "Target",
            "Target",
            Update::Set(false),
        );

        assert!(
            errors
                .iter()
                .any(|err| err.contains("final cleanup may not change proof-node classification")),
            "expected proof-node classification rejection, got {:?}",
            errors
        );
    }

    #[test]
    fn accept_worker_response_allows_final_cleanup_substitution_target_deletion() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 42,
            cycle: 9,
            payload_outcome: WorkerOutcome::Valid,
            deleted_nodes: BTreeSet::from([NodeId::from("Target")]),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    NodeId::from("Target"),
                ]),
                current_proof_nodes: BTreeSet::from([NodeId::from("Target")]),
                current_node_kinds: BTreeMap::from([(
                    NodeId::from("Preamble"),
                    NodeKind::Preamble,
                )]),
                ..WorkerNormalizationInput::default()
            },
            validation_execution_plan: vec![final_cleanup_substitution_step("Target")],
            validation_step_results: vec![WorkerValidationStepResult {
                kind: "final_cleanup_preserving".to_string(),
                ok: true,
                ..WorkerValidationStepResult::default()
            }],
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(
            output.final_outcome,
            WorkerOutcome::Valid,
            "{:?}",
            output.errors
        );
        assert!(output.errors.is_empty(), "{:?}", output.errors);
        assert_eq!(
            output.response.deleted_nodes,
            BTreeSet::from([NodeId::from("Target")])
        );
        assert_eq!(
            output.response.proof_node_updates.get("Target"),
            Some(&Update::Set(false))
        );
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
    fn accept_worker_response_allows_same_burst_orphan_deletion() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_supported_orphan_fixture(&repo, false, false);

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 12,
            cycle: 4,
            payload_outcome: WorkerOutcome::Valid,
            deleted_nodes: BTreeSet::from([NodeId::from("B")]),
            normalization: rooted_ab_normalization(repo),
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert!(
            output.final_outcome == WorkerOutcome::Valid && output.errors.is_empty(),
            "expected valid same-burst deletion, got {:?} with errors {:?}",
            output.final_outcome,
            output.errors
        );
        assert_eq!(
            output.response.deleted_nodes,
            BTreeSet::from([NodeId::from("B")])
        );
    }

    #[test]
    fn accept_worker_response_rejects_missing_deleted_nodes_field_entry() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_supported_orphan_fixture(&repo, false, false);

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 13,
            cycle: 4,
            payload_outcome: WorkerOutcome::Valid,
            normalization: rooted_ab_normalization(repo),
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(output.response.deleted_nodes.is_empty());
        assert!(
            output
                .errors
                .iter()
                .any(|err| err.contains("deleted_nodes must exactly match")),
            "expected exact deleted_nodes mismatch, got {:?}",
            output.errors
        );
    }

    #[test]
    fn accept_worker_response_rejects_supported_node_deletion() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_supported_orphan_fixture(&repo, true, false);

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 14,
            cycle: 4,
            payload_outcome: WorkerOutcome::Valid,
            deleted_nodes: BTreeSet::from([NodeId::from("B")]),
            normalization: rooted_ab_normalization(repo),
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(
            output
                .errors
                .iter()
                .any(|err| err.contains("contain only orphans")),
            "expected same-burst orphan legality error, got {:?}",
            output.errors
        );
    }

    #[test]
    fn pre_existing_orphan_is_tolerated_by_a_scoped_burst() {
        // dec2flt cycle-280 regression: a Decide flip orphaned nodes no
        // worker created; a scoped burst (deleting nothing, touching only
        // its authorized node) must not be rejected for them. Baseline:
        // A is rooted, B is ALREADY an orphan (nothing imports it, it
        // claims no target); disk matches.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_supported_orphan_fixture(&repo, false, true);
        let mut normalization = rooted_ab_normalization(repo);
        normalization.current_deps.insert(
            NodeId::from("A"),
            BTreeSet::from([NodeId::from("Preamble")]),
        );
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 16,
            cycle: 4,
            payload_outcome: WorkerOutcome::Valid,
            normalization,
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();
        assert!(
            !output
                .errors
                .iter()
                .any(|err| err.contains("orphan")),
            "a pre-existing orphan must not reject the burst; got {:?}",
            output.errors
        );
    }

    #[test]
    fn accept_worker_response_rejects_live_orphan_after_valid_delta() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_supported_orphan_fixture(&repo, false, true);

        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 15,
            cycle: 4,
            payload_outcome: WorkerOutcome::Valid,
            normalization: rooted_ab_normalization(repo),
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(
            output
                .errors
                .iter()
                .any(|err| err.contains("leaves NEW live orphan nodes")),
            "expected live-orphan rejection, got {:?}",
            output.errors
        );
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
    fn accept_worker_response_carries_worker_audit_request_onto_response() {
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
            request_id: 21,
            cycle: 7,
            payload_outcome: WorkerOutcome::Stuck,
            before_snapshot,
            audit_request: Some(crate::review_normalization::RawAuditRequest {
                reason_kind: "approach".to_string(),
                reason: "  the decomposition cannot close  ".to_string(),
            }),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        let ar = output
            .response
            .audit_request
            .expect("worker audit_request must survive onto the response");
        assert_eq!(ar.reason_kind, AuditRequestReasonKind::Approach);
        // Reason is trimmed by the normalizer (mirrors the reviewer path).
        assert_eq!(ar.reason, "the decomposition cannot close");
    }

    #[test]
    fn accept_worker_response_carries_memory_challenges_onto_response() {
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
            request_id: 23,
            cycle: 9,
            payload_outcome: WorkerOutcome::Stuck,
            before_snapshot,
            memory_challenges: vec![crate::process_memory::MemoryChallenge {
                entry_id: "  pm-0004-z  ".to_string(),
                reason: "  the probe compiles the forbidden route  ".to_string(),
            }],
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();

        assert_eq!(
            output.response.memory_challenges,
            vec![crate::process_memory::MemoryChallenge {
                entry_id: "pm-0004-z".to_string(),
                reason: "the probe compiles the forbidden route".to_string(),
            }],
            "memory_challenges must survive (trimmed) onto the response"
        );

        // Invalid shape is rejected, mirroring the audit_request path.
        let tmp2 = tempdir().unwrap();
        let repo2 = tmp2.path().join("repo");
        fs::create_dir_all(repo2.join("Tablet")).unwrap();
        write(
            &repo2.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo2.join("Tablet/Preamble.tex"), "");
        let before_snapshot = snapshot_tablet_dir(&repo2);
        let err = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 24,
            cycle: 9,
            payload_outcome: WorkerOutcome::Stuck,
            before_snapshot,
            memory_challenges: vec![crate::process_memory::MemoryChallenge {
                entry_id: "pm-0004-z".to_string(),
                reason: "  ".to_string(),
            }],
            normalization: WorkerNormalizationInput {
                repo_path: repo2,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap_err();
        assert!(err.contains("memory_challenges[0].reason"));
    }

    #[test]
    fn accept_worker_response_rejects_worker_audit_request_with_bad_reason_kind() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let before_snapshot = snapshot_tablet_dir(&repo);

        let err = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 22,
            cycle: 8,
            payload_outcome: WorkerOutcome::Stuck,
            before_snapshot,
            audit_request: Some(crate::review_normalization::RawAuditRequest {
                reason_kind: "not_a_kind".to_string(),
                reason: "broken".to_string(),
            }),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap_err();
        assert!(
            err.contains("reason_kind"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn accept_worker_response_rejects_worker_audit_request_with_empty_reason() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let before_snapshot = snapshot_tablet_dir(&repo);

        let err = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 23,
            cycle: 9,
            payload_outcome: WorkerOutcome::Stuck,
            before_snapshot,
            audit_request: Some(crate::review_normalization::RawAuditRequest {
                reason_kind: "approach".to_string(),
                reason: "   ".to_string(),
            }),
            normalization: WorkerNormalizationInput {
                repo_path: repo,
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                ..WorkerNormalizationInput::default()
            },
            ..WorkerAcceptanceInput::default()
        })
        .unwrap_err();
        assert!(err.contains("non-empty"), "unexpected error message: {err}");
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_target_claims: BTreeMap::from([(
                    claimed_node.clone(),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_target_claims: BTreeMap::from([(
                    claimed_node.clone(),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_target_claims: BTreeMap::from([(
                    claimed_node.clone(),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_target_claims: BTreeMap::from([(
                    claimed_node.clone(),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_target_claims: BTreeMap::from([(
                    claimed_node.clone(),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_target_claims: BTreeMap::from([(
                    claimed_node.clone(),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_target_claims: BTreeMap::from([(
                    claimed_node.clone(),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_target_claims: BTreeMap::from([(
                    claimed_node.clone(),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    claimed_node.clone(),
                ]),
                current_target_claims: BTreeMap::from([(
                    claimed_node.clone(),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                current_proof_nodes: BTreeSet::new(),
                current_node_kinds: BTreeMap::from([(
                    NodeId::from("Preamble"),
                    NodeKind::Preamble,
                )]),
                target_claim_updates: BTreeMap::from([(
                    NodeId::from("A"),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
                configured_targets: BTreeSet::from([TargetId::from("t.a")]),
                current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
                current_proof_nodes: BTreeSet::new(),
                current_node_kinds: BTreeMap::from([(
                    NodeId::from("Preamble"),
                    NodeKind::Preamble,
                )]),
                target_claim_updates: BTreeMap::from([(
                    NodeId::from("A"),
                    BTreeSet::from([TargetId::from("t.a")]),
                )]),
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
             -- [TABLET NODE: A]\n\
             theorem A : True := by\n-- BODY\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        );

        let output = prepare_worker_gate_observations(&WorkerGateObservationInput {
            repo_path: repo,
            current_present_nodes: BTreeSet::from([NodeId::from("A"), NodeId::from("Preamble")]),
            active_node: Some(NodeId::from("A")),
            under_model_assumption_nodes: BTreeSet::new(),
            assumption_authoring_node: None,
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
        let ordinary_hash = output
            .baseline_declaration_hashes
            .get("A")
            .expect("ordinary node hash");
        let preamble_hash = output
            .baseline_declaration_hashes
            .get("Preamble")
            .expect("Preamble hash");
        assert!(!ordinary_hash.is_empty());
        assert!(!ordinary_hash.starts_with(WHOLE_FILE_DECLARATION_HASH_PREFIX));
        assert_eq!(
            preamble_hash,
            &whole_file_declaration_hash("import Mathlib.Data.Nat.Basic\n")
        );
        assert!(preamble_hash.starts_with(WHOLE_FILE_DECLARATION_HASH_PREFIX));
    }

    const CHALLENGE_THEOREM_FILE: &str = "\
import Tablet.Preamble

-- [TABLET NODE: UnitBound]
theorem UnitBound : 1 \u{2264} 2 := by
-- BODY
  sorry
";

    fn challenge_theorem_spec() -> ChallengeTargetSpec {
        ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: "UnitBound".to_string(),
            lean: "theorem UnitBound : 1 \u{2264} 2 := by".to_string(),
            ..ChallengeTargetSpec::default()
        }
    }

    fn challenge_def_spec() -> ChallengeTargetSpec {
        ChallengeTargetSpec {
            kind: ChallengeTargetKind::Def,
            name: "planeDim".to_string(),
            lean: "def planeDim : Nat :=\n  2".to_string(),
            ..ChallengeTargetSpec::default()
        }
    }

    fn challenge_repo(tmp: &tempfile::TempDir, node: &str, lean: &str) -> PathBuf {
        let repo = tmp.path().join("repo");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(&repo.join(format!("Tablet/{node}.lean")), lean);
        write(
            &repo.join(format!("Tablet/{node}.tex")),
            "\\begin{theorem}stmt\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        );
        repo
    }

    fn challenge_input(
        repo: PathBuf,
        node: &str,
        spec: ChallengeTargetSpec,
        target_id: &str,
    ) -> WorkerNormalizationInput {
        WorkerNormalizationInput {
            repo_path: repo,
            configured_challenge_targets: BTreeMap::from([(
                ChallengeTargetId::from(target_id),
                spec,
            )]),
            current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
            challenge_claim_updates: BTreeMap::from([(
                NodeId::from(node),
                BTreeSet::from([ChallengeTargetId::from(target_id)]),
            )]),
            target_claim_updates: BTreeMap::from([(NodeId::from(node), BTreeSet::new())]),
            ..WorkerNormalizationInput::default()
        }
    }

    #[test]
    fn challenge_claim_normalization_mirrors_into_coverage() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "UnitBound", CHALLENGE_THEOREM_FILE);
        let input = challenge_input(repo, "UnitBound", challenge_theorem_spec(), "challenge:ub");

        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            normalized.contract_errors.is_empty(),
            "expected clean accept, got {:?}",
            normalized.contract_errors
        );
        assert_eq!(
            normalized.challenge_claim_updates.get("UnitBound"),
            Some(&Update::Set(BTreeSet::from([ChallengeTargetId::from(
                "challenge:ub"
            )])))
        );
        assert_eq!(
            normalized.snapshot.challenge_coverage.get("challenge:ub"),
            Some(&BTreeSet::from([NodeId::from("UnitBound")]))
        );
    }

    #[test]
    fn challenge_claim_normalization_drops_unknown_target_ids() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "UnitBound", CHALLENGE_THEOREM_FILE);
        let mut input =
            challenge_input(repo, "UnitBound", challenge_theorem_spec(), "challenge:ub");
        input
            .challenge_claim_updates
            .get_mut(&NodeId::from("UnitBound"))
            .unwrap()
            .insert(ChallengeTargetId::from("challenge:unknown"));

        let normalized = normalize_worker_response(&input).unwrap();
        assert_eq!(
            normalized.snapshot.challenge_coverage.get("challenge:ub"),
            Some(&BTreeSet::from([NodeId::from("UnitBound")]))
        );
        assert!(normalized
            .snapshot
            .challenge_coverage
            .get("challenge:unknown")
            .is_none());
    }

    #[test]
    fn challenge_theorem_byte_check_accepts_exact_slice_and_free_proof_body() {
        let tmp = tempdir().unwrap();
        let with_other_proof = CHALLENGE_THEOREM_FILE.replace("  sorry", "  omega");
        let repo = challenge_repo(&tmp, "UnitBound", &with_other_proof);
        let input = challenge_input(repo, "UnitBound", challenge_theorem_spec(), "challenge:ub");
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            normalized.contract_errors.is_empty(),
            "theorem proof body is worker-authored; got {:?}",
            normalized.contract_errors
        );
    }

    #[test]
    fn challenge_theorem_byte_check_rejects_statement_drift_quoting_first_diff() {
        let tmp = tempdir().unwrap();
        let drifted = CHALLENGE_THEOREM_FILE.replace("1 \u{2264} 2", "1 \u{2264} 3");
        let repo = challenge_repo(&tmp, "UnitBound", &drifted);
        let input = challenge_input(repo, "UnitBound", challenge_theorem_spec(), "challenge:ub");
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge byte-conformance rule failed"),
            "missing byte-conformance reason: {joined}"
        );
        assert!(
            joined.contains("challenge:ub"),
            "names the target: {joined}"
        );
        assert!(
            joined.contains("expected `theorem UnitBound : 1 \u{2264} 2 := by`"),
            "quotes the expected line: {joined}"
        );
        assert!(
            joined.contains("actual `theorem UnitBound : 1 \u{2264} 3 := by`"),
            "quotes the actual line: {joined}"
        );
        assert!(
            joined.contains("proof_coarse_restructure"),
            "names the no-escape rule: {joined}"
        );
    }

    const CHALLENGE_DEF_FILE: &str = "\
import Tablet.Preamble

-- [TABLET NODE: planeDim]
def planeDim : Nat :=
-- BODY
  2
";

    #[test]
    fn challenge_def_byte_check_covers_full_text_through_eof() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "planeDim", CHALLENGE_DEF_FILE);
        let input = challenge_input(repo, "planeDim", challenge_def_spec(), "challenge:dim");
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            normalized.contract_errors.is_empty(),
            "expected conformant def, got {:?}",
            normalized.contract_errors
        );
    }

    // ── Challenge deletion guard: a pinned target may never be deleted ────
    // The byte-pin protects a covering node's TEXT/NAMESPACE; the deletion
    // guard protects its EXISTENCE. These exercise the covered→uncovered
    // transition at worker acceptance (the timing hole the AdvancePhase-only
    // `ChallengeCoverage` blocker left open).

    /// Build a normalization input where the target is COVERED in the
    /// committed prior state (`current_present_nodes` + `current_challenge_claims`
    /// both name the covering node), then apply the supplied response claim
    /// updates. With `challenge_claim_updates` clearing the node's claim and
    /// the node absent on disk, this models a deletion.
    fn deletion_guard_input(
        repo: PathBuf,
        node: &str,
        spec: ChallengeTargetSpec,
        target_id: &str,
        challenge_claim_updates: BTreeMap<NodeId, BTreeSet<ChallengeTargetId>>,
    ) -> WorkerNormalizationInput {
        WorkerNormalizationInput {
            repo_path: repo,
            configured_challenge_targets: BTreeMap::from([(
                ChallengeTargetId::from(target_id),
                spec,
            )]),
            // Committed prior state: Preamble + the covering node present,
            // and the covering node claiming the target.
            current_present_nodes: BTreeSet::from([NodeId::from("Preamble"), NodeId::from(node)]),
            current_challenge_claims: BTreeMap::from([(
                NodeId::from(node),
                BTreeSet::from([ChallengeTargetId::from(target_id)]),
            )]),
            challenge_claim_updates,
            ..WorkerNormalizationInput::default()
        }
    }

    /// A challenge target covered before the burst, whose covering node the
    /// response removes (gone from disk + claim cleared), is REJECTED with a
    /// deletion-guard contract error naming the target. (Gate test (a).)
    #[test]
    fn challenge_deletion_guard_rejects_removed_covering_node() {
        let tmp = tempdir().unwrap();
        // Only Preamble on disk: the UnitBound covering node has been deleted.
        let repo = tmp.path().join("repo");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let input = deletion_guard_input(
            repo,
            "UnitBound",
            challenge_theorem_spec(),
            "challenge:ub",
            // Worker clears the claim on the deleted node ("Removed orphaned
            // goal nodes").
            BTreeMap::from([(NodeId::from("UnitBound"), BTreeSet::new())]),
        );
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge deletion-guard rule failed"),
            "missing deletion-guard reason: {joined}"
        );
        assert!(
            joined.contains("challenge:ub"),
            "names the uncovered target: {joined}"
        );
        assert!(
            normalized
                .snapshot
                .challenge_coverage
                .get("challenge:ub")
                .map(|nodes| nodes.is_empty())
                .unwrap_or(true),
            "coverage is empty after the deletion"
        );
    }

    /// The incident reproduction: a worker that drops a
    /// `montgomery_reduce_Correctness`-style pinned goal node (leaving it
    /// uncovered) is rejected. (Gate test (d).)
    #[test]
    fn challenge_deletion_guard_rejects_dropping_montgomery_reduce_correctness() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let spec = ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: "montgomery_reduce_Correctness".to_string(),
            lean: "theorem montgomery_reduce_Correctness : True := by".to_string(),
            ..ChallengeTargetSpec::default()
        };
        let input = deletion_guard_input(
            repo,
            "montgomery_reduce_Correctness",
            spec,
            "challenge:montgomery_reduce_Correctness",
            BTreeMap::from([(
                NodeId::from("montgomery_reduce_Correctness"),
                BTreeSet::new(),
            )]),
        );
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge deletion-guard rule failed"),
            "incident repro must be rejected: {joined}"
        );
        assert!(
            joined.contains("challenge:montgomery_reduce_Correctness"),
            "names the dropped target: {joined}"
        );
    }

    /// A result that keeps the covering node present and claiming the target
    /// (e.g. editing only the proof body) is ACCEPTED — the guard does not
    /// fire when coverage is preserved. (Gate test (b).)
    #[test]
    fn challenge_deletion_guard_accepts_preserved_coverage() {
        let tmp = tempdir().unwrap();
        let with_other_proof = CHALLENGE_THEOREM_FILE.replace("  sorry", "  omega");
        let repo = challenge_repo(&tmp, "UnitBound", &with_other_proof);
        // Covering node still present + still claims the target (no claim
        // update needed; the committed claim carries through).
        let input = deletion_guard_input(
            repo,
            "UnitBound",
            challenge_theorem_spec(),
            "challenge:ub",
            BTreeMap::new(),
        );
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            !normalized
                .contract_errors
                .iter()
                .any(|e| e.contains("challenge deletion-guard rule failed")),
            "preserved coverage must not trip the deletion guard: {:?}",
            normalized.contract_errors
        );
        assert_eq!(
            normalized.snapshot.challenge_coverage.get("challenge:ub"),
            Some(&BTreeSet::from([NodeId::from("UnitBound")])),
            "target stays covered"
        );
    }

    /// A legitimate rename/replacement — delete node `OldName`, add `UnitBound`
    /// reproducing the prescribed statement and claiming the same target —
    /// keeps the target covered, so it is ALLOWED.
    #[test]
    fn challenge_deletion_guard_allows_rename_that_keeps_target_covered() {
        let tmp = tempdir().unwrap();
        // On disk: the NEW node `UnitBound` (the old node is gone).
        let repo = challenge_repo(&tmp, "UnitBound", CHALLENGE_THEOREM_FILE);
        let input = WorkerNormalizationInput {
            repo_path: repo,
            configured_challenge_targets: BTreeMap::from([(
                ChallengeTargetId::from("challenge:ub"),
                challenge_theorem_spec(),
            )]),
            // Prior state: an OLD node covered the target.
            current_present_nodes: BTreeSet::from([
                NodeId::from("Preamble"),
                NodeId::from("OldName"),
            ]),
            current_challenge_claims: BTreeMap::from([(
                NodeId::from("OldName"),
                BTreeSet::from([ChallengeTargetId::from("challenge:ub")]),
            )]),
            // Response: clear the old node's claim, the new node claims it.
            challenge_claim_updates: BTreeMap::from([
                (NodeId::from("OldName"), BTreeSet::new()),
                (
                    NodeId::from("UnitBound"),
                    BTreeSet::from([ChallengeTargetId::from("challenge:ub")]),
                ),
            ]),
            ..WorkerNormalizationInput::default()
        };
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            !normalized
                .contract_errors
                .iter()
                .any(|e| e.contains("challenge deletion-guard rule failed")),
            "a rename that keeps the target covered must be allowed: {:?}",
            normalized.contract_errors
        );
        assert_eq!(
            normalized.snapshot.challenge_coverage.get("challenge:ub"),
            Some(&BTreeSet::from([NodeId::from("UnitBound")])),
            "the new node covers the target"
        );
    }

    /// All-math (empty challenge registry): the deletion guard is inert. A
    /// response that removes a paper-only node trips no deletion-guard error.
    /// (Gate test (c).)
    #[test]
    fn challenge_deletion_guard_inert_when_no_challenge_targets() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        // No configured_challenge_targets; a node was present before and is gone now.
        let input = WorkerNormalizationInput {
            repo_path: repo,
            current_present_nodes: BTreeSet::from([
                NodeId::from("Preamble"),
                NodeId::from("SomeNode"),
            ]),
            ..WorkerNormalizationInput::default()
        };
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            !normalized
                .contract_errors
                .iter()
                .any(|e| e.contains("challenge deletion-guard rule failed")),
            "all-math runs must never see the deletion guard: {:?}",
            normalized.contract_errors
        );
    }

    /// A target that was NOT yet covered before the burst (theorem-stating
    /// before the covering node is first placed) does not trip the guard —
    /// not-yet-covered is the AdvancePhase blocker's province, not a deletion.
    #[test]
    fn challenge_deletion_guard_silent_when_target_was_already_uncovered() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        // Prior state: target configured but no node covers it yet.
        let input = WorkerNormalizationInput {
            repo_path: repo,
            configured_challenge_targets: BTreeMap::from([(
                ChallengeTargetId::from("challenge:ub"),
                challenge_theorem_spec(),
            )]),
            current_present_nodes: BTreeSet::from([NodeId::from("Preamble")]),
            current_challenge_claims: BTreeMap::new(),
            ..WorkerNormalizationInput::default()
        };
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            !normalized
                .contract_errors
                .iter()
                .any(|e| e.contains("challenge deletion-guard rule failed")),
            "not-yet-covered must not trip the deletion guard: {:?}",
            normalized.contract_errors
        );
    }

    // ── PV ExtractionModel identity: namespace-context byte-pin ───────────
    // A model def file carries `namespace ntt_montgomery` in the FREE preamble
    // (above the marker). The byte-pinned slice covers the def VALUE but not its
    // fully-qualified NAME; the namespace-context pin closes that hole. The spec
    // pins `namespace_context = "namespace ntt_montgomery"`.

    const MODEL_DEF_FILE: &str = "\
import Tablet.Preamble
open Aeneas Aeneas.Std Result ControlFlow Error
namespace ntt_montgomery

-- [TABLET NODE: planeDim]
def planeDim : Nat :=
-- BODY
  2
";

    fn model_def_spec() -> ChallengeTargetSpec {
        let mut spec = challenge_def_spec();
        spec.namespace_context = "namespace ntt_montgomery".to_string();
        spec
    }

    /// A goal-node challenge target whose configured `namespace_context` is a
    /// FULL preamble (`open …\nnamespace ntt_montgomery`), as the montgomery
    /// goal targets carry. The pinned side must be normalised to the
    /// namespace-only scope before the byte-compare, else the node's extracted
    /// namespace-only context can never match and the target is permanently
    /// unsatisfiable.
    fn model_def_spec_full_preamble() -> ChallengeTargetSpec {
        let mut spec = challenge_def_spec();
        spec.namespace_context =
            "open Aeneas Aeneas.Std Result ControlFlow Error\nnamespace ntt_montgomery".to_string();
        spec
    }

    /// (b) An ExtractionModel def with only an ADDED `import` / `open` /
    /// `set_option` (namespace untouched) is ACCEPTED: those preamble lines are
    /// free, so the namespace context is byte-identical to the pin.
    #[test]
    fn model_namespace_context_accepts_added_import_open_set_option() {
        let tmp = tempdir().unwrap();
        let augmented = MODEL_DEF_FILE.replace(
            "import Tablet.Preamble\n",
            "import Tablet.Preamble\nimport Tablet.Extra\nopen Nat\nset_option maxHeartbeats 400000\n",
        );
        let repo = challenge_repo(&tmp, "planeDim", &augmented);
        let input = challenge_input(repo, "planeDim", model_def_spec(), "model:planeDim");
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            normalized.contract_errors.is_empty(),
            "added import/open/set_option are free preamble edits; got {:?}",
            normalized.contract_errors
        );
    }

    /// (a) Stripping `namespace ntt_montgomery` from an ExtractionModel def is
    /// REJECTED by the namespace-context byte-pin (this is the montgomery
    /// FIELD_MODULUS/MONTGOMERY_QINV incident: the strip silently retargets the
    /// trusted decl's fully-qualified name).
    #[test]
    fn model_namespace_context_rejects_namespace_strip() {
        let tmp = tempdir().unwrap();
        let stripped = MODEL_DEF_FILE.replace("namespace ntt_montgomery\n", "");
        let repo = challenge_repo(&tmp, "planeDim", &stripped);
        let input = challenge_input(repo, "planeDim", model_def_spec(), "model:planeDim");
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge namespace-context rule failed"),
            "stripping the namespace must be a contract error: {joined}"
        );
        assert!(
            joined.contains("model:planeDim"),
            "names the target: {joined}"
        );
    }

    /// (a') ALTERING the namespace (renaming the crate) is likewise REJECTED.
    #[test]
    fn model_namespace_context_rejects_namespace_rename() {
        let tmp = tempdir().unwrap();
        let renamed = MODEL_DEF_FILE.replace("namespace ntt_montgomery", "namespace other_crate");
        let repo = challenge_repo(&tmp, "planeDim", &renamed);
        let input = challenge_input(repo, "planeDim", model_def_spec(), "model:planeDim");
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge namespace-context rule failed"),
            "renaming the namespace must be a contract error: {joined}"
        );
    }

    /// (regression) A goal node whose preamble carries `open …\nnamespace
    /// ntt_montgomery`, against a target that pins the FULL preamble
    /// `"open …\nnamespace ntt_montgomery"`, is ACCEPTED. Both sides normalise to
    /// the namespace-only scope `"namespace ntt_montgomery"`, so the byte-compare
    /// matches. (Before the fix the pinned side was compared verbatim, so the
    /// node's namespace-only extract never matched — the montgomery halt.)
    #[test]
    fn model_namespace_context_full_preamble_pin_accepts_matching_node() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "planeDim", MODEL_DEF_FILE);
        let input = challenge_input(
            repo,
            "planeDim",
            model_def_spec_full_preamble(),
            "model:planeDim",
        );
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            normalized.contract_errors.is_empty(),
            "a full-preamble pin must normalise to namespace-only and accept a matching node; got {:?}",
            normalized.contract_errors
        );
    }

    /// (regression, protection still holds) Under the SAME full-preamble pin, a
    /// node that STRIPS `namespace ntt_montgomery` is still REJECTED — opens are
    /// free but the namespace scope remains pinned.
    #[test]
    fn model_namespace_context_full_preamble_pin_rejects_namespace_strip() {
        let tmp = tempdir().unwrap();
        let stripped = MODEL_DEF_FILE.replace("namespace ntt_montgomery\n", "");
        let repo = challenge_repo(&tmp, "planeDim", &stripped);
        let input = challenge_input(
            repo,
            "planeDim",
            model_def_spec_full_preamble(),
            "model:planeDim",
        );
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge namespace-context rule failed"),
            "stripping the namespace under a full-preamble pin must still be rejected: {joined}"
        );
    }

    /// A pin that is ONLY free preamble (an `open`, no `namespace`/`end`)
    /// normalises to empty ⇒ the block stays inert, matching the empty-pin
    /// all-math path.
    #[test]
    fn model_namespace_context_open_only_pin_is_inert() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "planeDim", CHALLENGE_DEF_FILE);
        let mut spec = challenge_def_spec();
        spec.namespace_context = "open Aeneas Aeneas.Std".to_string();
        let input = challenge_input(repo, "planeDim", spec, "challenge:dim");
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            normalized.contract_errors.is_empty(),
            "an open-only pin normalises to empty and must stay inert; got {:?}",
            normalized.contract_errors
        );
    }

    /// A target with an EMPTY `namespace_context` (every math challenge target)
    /// is never namespace-checked — the all-math path is byte-identical.
    #[test]
    fn empty_namespace_context_is_inert_for_math_targets() {
        let tmp = tempdir().unwrap();
        // A plain (no namespace line) math def, default spec (`namespace_context = ""`).
        let repo = challenge_repo(&tmp, "planeDim", CHALLENGE_DEF_FILE);
        let input = challenge_input(repo, "planeDim", challenge_def_spec(), "challenge:dim");
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            normalized.contract_errors.is_empty(),
            "empty namespace_context must not gate a math target; got {:?}",
            normalized.contract_errors
        );
    }

    /// `filespec_split::namespace_context` extracts only `namespace`/`end`
    /// lines and ignores `import`/`open`/`set_option`/comments.
    #[test]
    fn namespace_context_extracts_only_scope_lines() {
        let ctx = crate::filespec_split::namespace_context(MODEL_DEF_FILE).unwrap();
        assert_eq!(ctx, "namespace ntt_montgomery");
    }

    // ── PV Phase 2: the extraction-chain acceptance gate ──────────────────
    // A def-kind ExtractionModel spec with provenance, claimed by a node the
    // engine marks as `PvRole::ExtractionModel`. The byte-pin is satisfied (the
    // file matches the spec) so any contract error comes from the extraction
    // gate alone.

    fn extraction_def_spec(
        source_sha256: &str,
        extractor_toolchain_sha256: &str,
    ) -> ChallengeTargetSpec {
        let mut spec = challenge_def_spec();
        spec.provenance.source_sha256 = source_sha256.to_string();
        spec.provenance.extractor_toolchain_sha256 = extractor_toolchain_sha256.to_string();
        spec
    }

    fn extraction_input(
        repo: PathBuf,
        node: &str,
        spec: ChallengeTargetSpec,
        target_id: &str,
        mark_extraction_model: bool,
    ) -> WorkerNormalizationInput {
        let mut input = challenge_input(repo, node, spec, target_id);
        if mark_extraction_model {
            input.extraction_model_nodes = BTreeSet::from([NodeId::from(node)]);
        }
        input
    }

    /// FAIL-CLOSED: an ExtractionModel node whose claimed target has an empty
    /// `source_sha256` is REJECTED — an extraction model must record the source
    /// it was extracted from (the source fingerprint that drives reopen).
    #[test]
    fn extraction_chain_rejects_empty_source_sha256() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "planeDim", CHALLENGE_DEF_FILE);
        let input = extraction_input(
            repo,
            "planeDim",
            extraction_def_spec("", "tool-hash"),
            "model:dim",
            true,
        );
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("extraction-chain rule failed")
                && joined.contains("empty `source_sha256`"),
            "expected fail-closed on empty source_sha256, got {joined}"
        );
    }

    /// FAIL-CLOSED: an ExtractionModel node whose claimed target has an empty
    /// `extractor_toolchain_sha256` is REJECTED — it must pin the toolchain that
    /// produced it.
    #[test]
    fn extraction_chain_rejects_empty_toolchain() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "planeDim", CHALLENGE_DEF_FILE);
        let input = extraction_input(
            repo,
            "planeDim",
            extraction_def_spec("src-hash", ""),
            "model:dim",
            true,
        );
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("extraction-chain rule failed")
                && joined.contains("empty `extractor_toolchain_sha256`"),
            "expected fail-closed on empty toolchain, got {joined}"
        );
    }

    /// A complete extraction chain (non-empty source + toolchain) passes the
    /// gate (and the byte-pin), with no contract error.
    #[test]
    fn extraction_chain_accepts_complete_provenance() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "planeDim", CHALLENGE_DEF_FILE);
        let input = extraction_input(
            repo,
            "planeDim",
            extraction_def_spec("src-hash", "tool-hash"),
            "model:dim",
            true,
        );
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            normalized.contract_errors.is_empty(),
            "expected a complete extraction chain to pass, got {:?}",
            normalized.contract_errors
        );
    }

    /// Behavior-preservation: a node NOT marked ExtractionModel (the all-math /
    /// plain-challenge path, `extraction_model_nodes` empty) is NOT gated even
    /// with empty provenance — the extraction chain check is a no-op, only the
    /// byte-pin applies. This is the all-math byte-identity guarantee at the
    /// acceptance gate.
    #[test]
    fn extraction_chain_skips_non_extraction_model_nodes() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "planeDim", CHALLENGE_DEF_FILE);
        let input = extraction_input(
            repo,
            "planeDim",
            extraction_def_spec("", ""),
            "model:dim",
            false,
        );
        let normalized = normalize_worker_response(&input).unwrap();
        assert!(
            normalized.contract_errors.is_empty(),
            "a non-ExtractionModel node must not be gated by the extraction chain; got {:?}",
            normalized.contract_errors
        );
    }

    #[test]
    fn challenge_def_byte_check_rejects_value_drift_below_body_marker() {
        let tmp = tempdir().unwrap();
        let drifted = CHALLENGE_DEF_FILE.replace("  2", "  3");
        let repo = challenge_repo(&tmp, "planeDim", &drifted);
        let input = challenge_input(repo, "planeDim", challenge_def_spec(), "challenge:dim");
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge byte-conformance rule failed"),
            "def value below -- BODY is load-bearing: {joined}"
        );
        assert!(joined.contains("expected `  2`"), "{joined}");
        assert!(joined.contains("actual `  3`"), "{joined}");
    }

    /// PV Phase 1 Slice 2: an ExtractionModel pin is exactly a `Def`-kind
    /// `ChallengeTargetSpec` (what `pv_tablet_from_config` produces, with the
    /// Rust source's `source_sha256` provenance), so the SAME byte-pin that
    /// guards challenge defs rejects a hand-edited generated model def
    /// whole-decl-through-EOF. No new pin machinery — this is the mechanical
    /// half of Model-Correspondence riding `challenge_conformance_errors`
    /// VERBATIM. A re-extraction (which restores the prescribed bytes) is the
    /// only way back to acceptance.
    #[test]
    fn pv_extraction_model_byte_pin_rejects_hand_edited_generated_def() {
        let tmp = tempdir().unwrap();
        // The worker hand-edits the generated model's value below `-- BODY`.
        let hand_edited = CHALLENGE_DEF_FILE.replace("  2", "  41");
        let repo = challenge_repo(&tmp, "planeDim", &hand_edited);
        let extraction_model_spec = ChallengeTargetSpec {
            kind: ChallengeTargetKind::Def,
            name: "planeDim".to_string(),
            lean: "def planeDim : Nat :=\n  2".to_string(),
            provenance: crate::ChallengeTargetProvenance {
                source_sha256: "deadbeef".to_string(),
                ..crate::ChallengeTargetProvenance::default()
            },
            ..ChallengeTargetSpec::default()
        };
        let input = challenge_input(repo, "planeDim", extraction_model_spec, "model:planeDim");
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge byte-conformance rule failed"),
            "a hand-edited generated model def must be rejected by the byte-pin: {joined}"
        );
        assert!(joined.contains("model:planeDim"), "names the pin: {joined}");
        assert!(joined.contains("expected `  2`"), "{joined}");
        assert!(joined.contains("actual `  41`"), "{joined}");
        // No-escape: the rule fires in every mode, coarse-restructure included.
        assert!(
            joined.contains("proof_coarse_restructure"),
            "names the no-escape rule: {joined}"
        );
    }

    /// PV mode-B contract-predicate pin: a `verification_target_definitions`
    /// entry is exactly a `Def`-kind `ChallengeTargetSpec` (forced kind, no
    /// provenance), so the SAME whole-decl byte-pin rejects a worker that
    /// WEAKENS the predicate body — the soundness keystone. A `Spec` predicate
    /// pinned as `… := ret = value % 7` cannot be silently re-defined to
    /// `:= True`; the pin's `include_body=true` slice runs through EOF, so the
    /// predicate body is load-bearing. Without this pin a worker could discharge
    /// the goal theorem vacuously by collapsing its postcondition predicate.
    #[test]
    fn pv_contract_predicate_def_byte_pin_rejects_weakened_spec_body() {
        let tmp = tempdir().unwrap();
        // The pinned Spec predicate def (the shape a `verification_target_
        // definitions` entry seeds): a real postcondition over the model output.
        let spec_file = "import Tablet.Preamble

-- [TABLET NODE: montgomery_reduce_Spec]
def montgomery_reduce_Spec (value ret : Int) : Prop :=
-- BODY
  ret = value % 7
";
        // The worker WEAKENS the predicate body to `True` (a vacuous spec).
        let weakened = spec_file.replace("  ret = value % 7", "  True");
        let repo = challenge_repo(&tmp, "montgomery_reduce_Spec", &weakened);
        let pinned_spec = ChallengeTargetSpec {
            kind: ChallengeTargetKind::Def,
            name: "montgomery_reduce_Spec".to_string(),
            lean: "def montgomery_reduce_Spec (value ret : Int) : Prop :=\n  ret = value % 7"
                .to_string(),
            ..ChallengeTargetSpec::default()
        };
        let input = challenge_input(
            repo,
            "montgomery_reduce_Spec",
            pinned_spec,
            "spec:montgomery_reduce",
        );
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge byte-conformance rule failed"),
            "a weakened pinned Spec predicate body must be rejected by the byte-pin: {joined}"
        );
        assert!(
            joined.contains("spec:montgomery_reduce"),
            "names the pin: {joined}"
        );
        assert!(joined.contains("expected `  ret = value % 7`"), "{joined}");
        assert!(joined.contains("actual `  True`"), "{joined}");
        // The pin covers the whole declaration through EOF (def-kind scope).
        assert!(
            joined.contains("through end of file"),
            "names the whole-decl scope: {joined}"
        );
    }

    #[test]
    fn challenge_name_parity_rejects_misnamed_claiming_node() {
        let tmp = tempdir().unwrap();
        let renamed = CHALLENGE_THEOREM_FILE.replace("UnitBound", "WrongName");
        let repo = challenge_repo(&tmp, "WrongName", &renamed);
        let input = challenge_input(repo, "WrongName", challenge_theorem_spec(), "challenge:ub");
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("challenge name-parity rule failed"),
            "missing name-parity reason: {joined}"
        );
        assert!(joined.contains("`WrongName`"), "{joined}");
        assert!(joined.contains("`UnitBound`"), "{joined}");
    }

    #[test]
    fn challenge_name_parity_accepts_namespaced_sanitized_stem() {
        // A namespaced prescribed decl (`NS.UnitBound`) is claimable by a node
        // carrying the FILESPEC-sanitized stem `NS_UnitBound` (dots ->
        // underscores) — a dot-bearing node stem is illegal under FILESPEC, so
        // name-parity must compare against the sanitized stem, not the raw name.
        let tmp = tempdir().unwrap();
        let file = CHALLENGE_THEOREM_FILE
            .replace(
                "-- [TABLET NODE: UnitBound]",
                "-- [TABLET NODE: NS_UnitBound]",
            )
            .replace("theorem UnitBound", "theorem NS.UnitBound");
        let repo = challenge_repo(&tmp, "NS_UnitBound", &file);
        let spec = ChallengeTargetSpec {
            name: "NS.UnitBound".to_string(),
            lean: "theorem NS.UnitBound : 1 \u{2264} 2 := by".to_string(),
            ..challenge_theorem_spec()
        };
        let input = challenge_input(repo, "NS_UnitBound", spec, "challenge:ns");
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            !joined.contains("name-parity rule failed"),
            "namespaced node `NS_UnitBound` (FILESPEC stem of prescribed `NS.UnitBound`) must pass name-parity: {joined}"
        );
        assert!(
            joined.is_empty(),
            "namespaced challenge claim should fully conform, got: {joined}"
        );
    }

    #[test]
    fn challenge_claims_are_exclusive_per_node() {
        let tmp = tempdir().unwrap();
        let repo = challenge_repo(&tmp, "UnitBound", CHALLENGE_THEOREM_FILE);
        let mut input =
            challenge_input(repo, "UnitBound", challenge_theorem_spec(), "challenge:ub");
        input.configured_challenge_targets.insert(
            ChallengeTargetId::from("challenge:other"),
            ChallengeTargetSpec {
                name: "UnitBound".to_string(),
                lean: "theorem UnitBound : 1 \u{2264} 2 := by".to_string(),
                ..challenge_theorem_spec()
            },
        );
        input
            .challenge_claim_updates
            .get_mut(&NodeId::from("UnitBound"))
            .unwrap()
            .insert(ChallengeTargetId::from("challenge:other"));
        let normalized = normalize_worker_response(&input).unwrap();
        let joined = normalized.contract_errors.join("\n");
        assert!(
            joined.contains("a challenge claim is exclusive"),
            "missing exclusivity rejection: {joined}"
        );
    }

    #[test]
    fn challenge_byte_check_has_no_coarse_restructure_escape() {
        // Same drifted statement as the rejection test, but flowing
        // through `accept_worker_response` with an explicit
        // coarse-restructure execution plan whose validation step
        // passed. The byte check fires regardless of mode: the final
        // outcome is Invalid and the reason names the rule.
        let tmp = tempdir().unwrap();
        let drifted = CHALLENGE_THEOREM_FILE.replace("1 \u{2264} 2", "1 \u{2264} 3");
        let repo = challenge_repo(&tmp, "UnitBound", &drifted);
        let normalization =
            challenge_input(repo, "UnitBound", challenge_theorem_spec(), "challenge:ub");
        let plan = vec![WorkerValidationExecutionPlanStep::ProofWorkerDelta {
            active_node: Some(NodeId::from("UnitBound")),
            mode: WorkerProofDeltaMode::CoarseRestructure,
            authorized_nodes: BTreeSet::from([NodeId::from("UnitBound")]),
            protected_semantic_change_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
        }];
        let output = accept_worker_response(&WorkerAcceptanceInput {
            request_id: 7,
            cycle: 3,
            payload_outcome: WorkerOutcome::Valid,
            normalization,
            validation_execution_plan: plan,
            validation_step_results: vec![WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: true,
                ..WorkerValidationStepResult::default()
            }],
            ..WorkerAcceptanceInput::default()
        })
        .unwrap();
        assert_eq!(output.final_outcome, WorkerOutcome::Invalid);
        assert!(!output.ok);
        let joined = output.errors.join("\n");
        assert!(
            joined.contains("challenge byte-conformance rule failed"),
            "coarse_restructure must have no escape: {joined}"
        );
    }
}

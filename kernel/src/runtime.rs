use crate::engine::{apply_event, ProtocolCommand, ProtocolEvent, TransitionError};
use crate::model::{
    CleanupUnreachableDeletionRecord, GateKind, HumanChoice, NodeId, Phase, ProtocolState,
    ResponseStatus, SubstantivenessStatus, WorkerOutcome, WorkerResponse, WorkingSnapshot,
    WrapperRequest, WrapperResponse, SOUND_ASSESSMENT_SCHEMA_VERSION,
};
use crate::trust_base::{
    hydrate_seed_support_definition_files, seed_support_definition_projection,
    parse_json_strict, raw_sha256, tagged_hash,
    verify_evidence_tool_manifest, verify_seed_definition_bundle, AuthoritativeRecord, DomainTag,
    EventKind, SchemaRegistry, TrustRecord, TrustRecordSeedRoots,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct RuntimePaths {
    pub root: PathBuf,
    pub state_path: PathBuf,
    pub checkpoint_path: PathBuf,
    pub metadata_path: PathBuf,
    /// The ONE deterministic finalization-archive path (Q7, Codex R2-7).
    /// Derived from the root, never serialized; assembly writes a temp file
    /// and atomically renames it here BEFORE verification re-reads the
    /// renamed bytes.
    pub package_archive_path: PathBuf,
}

impl RuntimePaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            state_path: root.join("protocol_state.json"),
            checkpoint_path: root.join("checkpoint.json"),
            metadata_path: root.join("runtime_metadata.json"),
            package_archive_path: root.join("trust_package.archive"),
            root,
        }
    }
}

const TRUST_ADVANCE_GATE_PRESENTATION_FILE: &str = "ADVANCE_GATE_PRESENTATION.bin";

/// Repository/runtime-root writes whose meaning is a human assumption-batch
/// decision. They are executed only after the checkpoint, state, and event-log
/// durability barrier. `RenderAdvanceGate` can follow `ProjectAll` in the same
/// command list on the legacy path and therefore shares the ordered buffer.
enum DeferredAssumptionDecisionWrite {
    ProjectAll { repo_path: PathBuf },
    RejectAll { repo_path: PathBuf, reason: String },
    RenderAdvanceGate { repo_path: PathBuf },
}

fn trust_advance_gate_presentation_path(runtime_root: &Path) -> PathBuf {
    runtime_root.join(TRUST_ADVANCE_GATE_PRESENTATION_FILE)
}

/// Write a sibling temp file and atomically rename it over `destination`.
/// This is the finalization archive's established publication primitive,
/// shared with gate re-presentation so neither delivered artifact can be
/// observed half-written.
fn atomically_replace_file(destination: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = destination.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "atomic replacement destination has no file name",
        )
    })?;
    let temp_path = destination.with_file_name(format!(
        "{}.tmp-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    fs::write(&temp_path, bytes)?;
    fs::rename(temp_path, destination)
}

/// Render the complete Stage-10 required-v1 advance-gate instrument from
/// current protocol state.  The top-level document and every nested section
/// are canonical JSON. This is a pure projection of the retained evidence
/// and decision state.
pub fn trust_advance_gate_presentation_bytes(
    state: &ProtocolState,
) -> Result<Vec<u8>, RuntimeError> {
    trust_advance_gate_presentation_bytes_with_repo(state, None)
}

/// Render the existing `ADVANCE_GATE_PRESENTATION` from protocol state and,
/// when it has meaning-bearing PV files to disclose, the attached campaign
/// repository. The state-only entry point above remains useful only for non-PV
/// fixtures. A PV presentation is deliberately repository-backed: hashes are
/// not a substitute for the exact `GOAL.md` and Tablet source the human is
/// being asked to approve.
///
/// Every collection is sourced from a `BTree*` container or explicitly sorted
/// by canonical JSON before serialization.  Identical state and repository
/// bytes therefore produce byte-identical output.
pub fn trust_advance_gate_presentation_bytes_for_repo(
    state: &ProtocolState,
    repo_path: &Path,
) -> Result<Vec<u8>, RuntimeError> {
    // `ProtocolState` is very large and the canonical instrument contains a
    // number of nested serde projections. Runtime transactions already retain
    // rollback/checkpoint state on their calling stack, so give this pure
    // renderer a bounded dedicated stack rather than making gate success
    // depend on the caller's remaining stack headroom.
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("trellis-gate-render".into())
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, || {
                trust_advance_gate_presentation_bytes_with_repo(state, Some(repo_path))
            })
            .map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "failed to start advance-gate renderer: {error}"
                ))
            })?;
        handle.join().map_err(|_| {
            RuntimeError::InvalidRuntimeState("advance-gate renderer panicked".into())
        })?
    })
}

fn gate_read_utf8(path: &Path, what: &str, require_nonempty: bool) -> Result<String, RuntimeError> {
    let bytes = fs::read(path).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "cannot render {what}: failed to read {}: {error}",
            path.display()
        ))
    })?;
    let text = String::from_utf8(bytes).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "cannot render {what}: {} is not UTF-8: {error}",
            path.display()
        ))
    })?;
    if require_nonempty && text.trim().is_empty() {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "cannot render {what}: {} is empty",
            path.display()
        )));
    }
    Ok(text)
}

fn gate_read_json(path: &Path, what: &str) -> Result<serde_json::Value, RuntimeError> {
    let text = gate_read_utf8(path, what, true)?;
    serde_json::from_str(&text).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "cannot render {what}: failed to parse {}: {error}",
            path.display()
        ))
    })
}

fn gate_check_state_name(value: crate::model::CurrentCheckState) -> &'static str {
    match value {
        crate::model::CurrentCheckState::Pass => "pass",
        crate::model::CurrentCheckState::Fail => "fail",
        crate::model::CurrentCheckState::Unknown => "unknown",
    }
}

fn gate_node_is_definition(state: &ProtocolState, node: &NodeId) -> bool {
    state.node_kinds.get(node).copied().unwrap_or_else(|| {
        if node.as_str() == "Preamble" {
            crate::model::NodeKind::Preamble
        } else if state.proof_nodes.contains(node) {
            crate::model::NodeKind::Proof
        } else {
            crate::model::NodeKind::Definition
        }
    }) == crate::model::NodeKind::Definition
}

fn gate_node_sources(
    repo_path: Option<&Path>,
    node: &NodeId,
    what: &str,
) -> Result<(String, String), RuntimeError> {
    let repo_path = repo_path.ok_or_else(|| {
        RuntimeError::InvalidRuntimeState(format!(
            "cannot render {what} `{}` without an attached campaign repository",
            node.as_str()
        ))
    })?;
    let tablet = repo_path.join("Tablet");
    let lean = gate_read_utf8(
        &tablet.join(format!("{}.lean", node.as_str())),
        &format!("{what} `{}` Lean body", node.as_str()),
        true,
    )?;
    let tex = gate_read_utf8(
        &tablet.join(format!("{}.tex", node.as_str())),
        &format!("{what} `{}` TeX meaning", node.as_str()),
        true,
    )?;
    Ok((lean, tex))
}

fn gate_sort_json_rows(
    rows: &mut Vec<serde_json::Value>,
    what: &str,
) -> Result<(), RuntimeError> {
    let mut keyed = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        let key = crate::trust_base::canonical_json(&row).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "cannot deterministically order {what}: {error}"
            ))
        })?;
        keyed.push((key, row));
    }
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    rows.extend(keyed.into_iter().map(|(_, row)| row));
    Ok(())
}

fn trust_advance_gate_presentation_bytes_with_repo(
    state: &ProtocolState,
    repo_path: Option<&Path>,
) -> Result<Vec<u8>, RuntimeError> {
    if state.is_pv() && repo_path.is_none() {
        return Err(RuntimeError::InvalidRuntimeState(
            "cannot render a PV advance-gate presentation without an attached campaign repository"
                .into(),
        ));
    }
    let seed_roots = if state.trust_base.required() {
        serde_json::to_value(trust_record_seed_roots(&state.trust_base)?)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?
    } else {
        json!({"status": "not_applicable_legacy"})
    };
    let registered_statement_authority_sha256: BTreeMap<_, _> = state
        .configured_challenge_targets
        .keys()
        .filter_map(|target| {
            crate::model::registered_statement_sha256(state, target)
                .map(|digest| (target.clone(), digest))
        })
        .collect();
    let authored_seed_section = json!({
        "approved_evidence_tool_input_root": state.trust_base.approved_evidence_tool_input_root,
        "authored_semantic_root": state.trust_base.authored_semantic_root,
        "configured_challenge_targets": state.configured_challenge_targets,
        "configured_targets": state.configured_targets,
        "launch_acknowledgment_sha256": state.trust_base.launch_acknowledgment_sha256,
        "pv_authored_statements": state.pv_authored_statements,
        "registered_statement_authority_sha256": registered_statement_authority_sha256,
        "seed_roots": seed_roots,
        "seed_support_definitions": state.trust_base.seed_support_definitions,
    });
    let phase0_section = if state.is_pv() && state.trust_base.phase0.is_some() {
        let roots = state.trust_base.phase0.as_ref().expect("checked above");
        let repo = repo_path.ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "PV advance gate cannot verify Phase-0 without its repository".into(),
            )
        })?;
        crate::phase0::phase0_advance_gate_section(repo, roots)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?
    } else if !state.is_pv() {
        json!({"section": "phase0_frozen_source_adaptation", "status": "not_applicable_non_pv"})
    } else {
        // Legacy/synthetic states can reach the pure renderer without going
        // through runtime installation.  A real PV launch/resume cannot:
        // runtime_cli verifies the required seed projection and repository
        // before constructing the runtime.
        json!({"section": "phase0_frozen_source_adaptation", "status": "not_applicable_legacy_state"})
    };

    // Per-target polarity ledger and refutation dossiers: built by the
    // SHARED row builders in `trust_base::claim` — the same writer feeds the
    // finalization claim artifacts, so the pre-gate instrument and the
    // packaged claim can never drift (one writer, no drift).
    let polarity_ledger = crate::trust_base::claim::polarity_ledger_rows(state);
    let refutation_dossiers = crate::trust_base::claim::refutation_dossier_rows(state);

    // The pre-gate edition of the shared claim rows. Terminal outcomes are
    // provisional and closures are absent by construction.
    let ratification_candidates: Vec<_> = state
        .trust_base
        .conditional_candidates
        .values()
        .filter(|candidate| {
            candidate.stage == crate::trust_base::ConditionalStage::CorrespondencePass
                && candidate.disposition.is_none()
                && candidate.ratification_gate_episode_id.is_some()
        })
        .collect();
    if ratification_candidates.len() > 1 {
        return Err(RuntimeError::InvalidRuntimeState(
            "an advance gate cannot carry more than one conditional packet".into(),
        ));
    }
    let conditional_ratification_packet = ratification_candidates
        .first()
        .map(|candidate| crate::trust_base::conditional_ratification_packet(candidate))
        .transpose()
        .map_err(RuntimeError::InvalidRuntimeState)?
        .unwrap_or(serde_json::Value::Null);
    let claim_rows_section = if state.trust_base.required()
        && conditional_ratification_packet.is_null()
    {
        let claim_rows = crate::trust_base::claim_rows_from_state(
            state,
            crate::trust_base::ClaimContext::pre_gate(),
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
        let claim_rows_value = serde_json::to_value(&claim_rows)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        json!({
            "edition": "pre_gate",
            "claim_rows": claim_rows_value,
        })
    } else if !conditional_ratification_packet.is_null() {
        json!({"status": "deferred_to_conditional_ratification_packet"})
    } else {
        json!({"status": "not_applicable_legacy"})
    };

    // The old section stopped at two digests.  Keep the same ordered
    // presentation structure, but make each semantic-closure definition a
    // reviewable source row.  Include the covering root when it is itself a
    // definition and conservatively union the stored Lean-semantic closure
    // with the ordinary dependency closure.  Over-disclosure is harmless;
    // silently losing a meaning-bearing definition is not.
    let mut machine_definitions = Vec::new();
    for (target, roots) in &state.live.coverage {
        if !state.configured_targets.contains(target) {
            continue;
        }
        let mut closure = state
            .live
            .protected_closure_nodes_per_target
            .get(target)
            .cloned()
            .unwrap_or_default();
        closure.extend(roots.iter().cloned());
        closure.extend(state.dep_closure(roots, &state.live.present_nodes, &state.deps));
        for root in roots {
            closure.extend(state.lean_relevant_dependencies_of(root));
        }
        for node in &closure {
            if !gate_node_is_definition(state, node) {
                continue;
            }
            let (lean_source_utf8, tex_meaning_utf8) =
                gate_node_sources(repo_path, node, "semantic-closure definition")?;
            machine_definitions.push(json!({
                "pinned_statement": {"kind": "paper_target", "id": target},
                "node": node,
                "lean_source_utf8": lean_source_utf8,
                "tex_meaning_utf8": tex_meaning_utf8,
                "verifier_status": {
                    "correspondence": gate_check_state_name(state.current_corr_state(node)),
                    "substantiveness": gate_check_state_name(
                        state.current_substantiveness_state(node)
                    ),
                },
            }));
        }
    }
    for (target, roots) in &state.live.challenge_coverage {
        if !state.configured_challenge_targets.contains_key(target) {
            continue;
        }
        let mut closure = state.dep_closure(roots, &state.live.present_nodes, &state.deps);
        for root in roots {
            closure.extend(state.lean_relevant_dependencies_of(root));
        }
        for node in &closure {
            if !gate_node_is_definition(state, node) {
                continue;
            }
            let (lean_source_utf8, tex_meaning_utf8) =
                gate_node_sources(repo_path, node, "semantic-closure definition")?;
            machine_definitions.push(json!({
                "pinned_statement": {"kind": "challenge_target", "id": target},
                "node": node,
                "lean_source_utf8": lean_source_utf8,
                "tex_meaning_utf8": tex_meaning_utf8,
                "verifier_status": {
                    "correspondence": gate_check_state_name(state.current_corr_state(node)),
                    "substantiveness": gate_check_state_name(
                        state.current_substantiveness_state(node)
                    ),
                },
            }));
        }
    }
    gate_sort_json_rows(&mut machine_definitions, "semantic-closure definitions")?;
    machine_definitions.dedup();

    // The target statement is part of the decision, not merely an index into
    // a fingerprint map.  Rows are per target/root so shared support remains
    // visibly shared rather than being flattened into an unexplained set.
    let mut covering_statements = Vec::new();
    for (target, roots) in &state.live.coverage {
        if !state.configured_targets.contains(target) {
            continue;
        }
        for node in roots {
            let (lean_source_utf8, tex_meaning_utf8) =
                gate_node_sources(repo_path, node, "covering statement")?;
            covering_statements.push(json!({
                "pinned_statement": {"kind": "paper_target", "id": target},
                "node": node,
                "lean_source_utf8": lean_source_utf8,
                "tex_meaning_utf8": tex_meaning_utf8,
                "verifier_status": {
                    "correspondence": gate_check_state_name(state.current_corr_state(node)),
                    "substantiveness": gate_check_state_name(
                        state.current_substantiveness_state(node)
                    ),
                    "goal_faithfulness": gate_check_state_name(
                        state.current_paper_state(target)
                    ),
                },
            }));
        }
    }
    for (target, roots) in &state.live.challenge_coverage {
        let Some(spec) = state.configured_challenge_targets.get(target) else {
            continue;
        };
        let paper_target = crate::model::TargetId::from(
            target
                .as_str()
                .strip_prefix("goal:")
                .unwrap_or(target.as_str()),
        );
        let goal_faithfulness = if state.configured_targets.contains(&paper_target) {
            gate_check_state_name(state.current_paper_state(&paper_target))
        } else {
            "not_applicable_seed_pinned_statement"
        };
        for node in roots {
            let (lean_source_utf8, tex_meaning_utf8) =
                gate_node_sources(repo_path, node, "covering statement")?;
            covering_statements.push(json!({
                "pinned_statement": {"kind": "challenge_target", "id": target},
                "registered_lean_statement_utf8": spec.lean,
                "registered_tex_meaning_utf8": spec.informal,
                "node": node,
                "lean_source_utf8": lean_source_utf8,
                "tex_meaning_utf8": tex_meaning_utf8,
                "verifier_status": {
                    "correspondence": gate_check_state_name(state.current_corr_state(node)),
                    "substantiveness": gate_check_state_name(
                        state.current_substantiveness_state(node)
                    ),
                    "goal_faithfulness": goal_faithfulness,
                },
            }));
        }
    }
    gate_sort_json_rows(&mut covering_statements, "covering statements")?;
    covering_statements.dedup();

    let goal = if state.is_pv() {
        if let Some(repo_path) = repo_path {
            json!({
                "path": "GOAL.md",
                "utf8": gate_read_utf8(&repo_path.join("GOAL.md"), "GOAL.md", true)?,
            })
        } else {
            json!({"status": "unavailable_in_state_only_projection"})
        }
    } else {
        json!({"status": "not_applicable_non_pv"})
    };

    let pending_assumptions: Vec<serde_json::Value> = if let Some(repo_path) = repo_path {
        let mut pending: Vec<_> = crate::assumptions_registry::load_proposed(repo_path)
            .map_err(RuntimeError::InvalidRuntimeState)?
            .pending()
            .cloned()
            .collect();
        pending.sort_by(|left, right| {
            (left.id.as_str(), left.axiom_name.as_str())
                .cmp(&(right.id.as_str(), right.axiom_name.as_str()))
        });
        pending
            .into_iter()
            .map(|record| serde_json::to_value(record).expect("ProposedAssumption serializes"))
            .collect()
    } else {
        if state.pending_under_model_assumptions != 0 {
            return Err(RuntimeError::InvalidRuntimeState(
                "cannot render the pending ProposedAssumption batch without an attached campaign repository"
                    .into(),
            ));
        }
        Vec::new()
    };
    // Required-v1 seals the human decision before executing the projection
    // command that removes the on-disk pending records.  The pure transition
    // has already cleared its count at that point, while
    // `gate_commit_pending` proves that this is the decision-bearing
    // intermediate state rather than an unexplained registry mismatch.  The
    // renderer must still bind the on-disk batch the human actually saw.
    let decision_is_consuming_presented_batch = state.trust_base.required()
        && state.trust_base.gate_commit_pending
        && matches!(
            state.trust_base.routine_gate_state,
            crate::model::TrustRoutineGateState::ApprovalCommitPending
                | crate::model::TrustRoutineGateState::FeedbackCommitPending
        )
        && state.pending_under_model_assumptions == 0;
    if pending_assumptions.len() != state.pending_under_model_assumptions as usize
        && !decision_is_consuming_presented_batch
    {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "cannot render pending ProposedAssumption batch: protocol state records {} pending but PROPOSED_ASSUMPTIONS.json contains {}",
            state.pending_under_model_assumptions,
            pending_assumptions.len()
        )));
    }

    // Batch consistency is judged over one interpretation.  A validity hook
    // named by any pending axiom therefore belongs to the batch disclosure
    // beside the assumptions, not merely in one target-indexed row elsewhere
    // in the presentation.  A referenced hook absent from the rendered
    // semantic closure is a gate defect, never a reason to scan unrelated
    // definitions or silently print less than the batch conditions on.
    let conditioned_validity_names: BTreeSet<String> = pending_assumptions
        .iter()
        .filter_map(|assumption| assumption.get("lean_statement"))
        .filter_map(serde_json::Value::as_str)
        .flat_map(crate::assumptions_registry::rust_validity_hook_names)
        .collect();
    let mut found = BTreeSet::new();
    let mut pending_validity_definitions = Vec::new();
    for row in &machine_definitions {
        let Some(lean_source_utf8) = row.get("lean_source_utf8").and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let declared: BTreeSet<String> = conditioned_validity_names
            .iter()
            .filter(|name| {
                crate::trust_base::bootstrap::lean_declares_definition(lean_source_utf8, name)
            })
            .cloned()
            .collect();
        if declared.is_empty() {
            continue;
        }
        found.extend(declared.iter().cloned());
        pending_validity_definitions.push(json!({
            "conditioned_validity_definitions": declared,
            "node": row.get("node"),
            "lean_source_utf8": row.get("lean_source_utf8"),
            "tex_meaning_utf8": row.get("tex_meaning_utf8"),
            "verifier_status": row.get("verifier_status"),
        }));
    }
    let missing: BTreeSet<_> = conditioned_validity_names
        .difference(&found)
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "cannot render pending assumption batch: referenced validity definitions are absent from the reviewable semantic closure: {missing:?}"
        )));
    }
    gate_sort_json_rows(
        &mut pending_validity_definitions,
        "conditioned validity definitions",
    )?;
    pending_validity_definitions.dedup();

    let (tcb_manifest, extraction_provenance) = if state.is_pv() {
        if let Some(repo_path) = repo_path {
            let manifest = gate_read_json(&repo_path.join("tcb_manifest.json"), "TCB manifest")?;
            let provenance = manifest.get("extraction_provenance").cloned().ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(
                    "cannot render platform/build pins: tcb_manifest.json lacks extraction_provenance"
                        .into(),
                )
            })?;
            (manifest, provenance)
        } else {
            (
                json!({"status": "unavailable_in_state_only_projection"}),
                json!({"status": "unavailable_in_state_only_projection"}),
            )
        }
    } else {
        (
            json!({"status": "not_applicable_non_pv"}),
            json!({"status": "not_applicable_non_pv"}),
        )
    };

    let mut resource_effect_gaps = Vec::new();
    if let Some(axioms) = state
        .pv_reachable_opaque_inventory
        .get("unresolved_boundary_axioms")
        .and_then(serde_json::Value::as_array)
    {
        for axiom in axioms {
            resource_effect_gaps.push(json!({
                "kind": "reachable_opaque_boundary",
                "record": axiom,
            }));
        }
    }
    if let Some(not_recorded) = extraction_provenance
        .get("not_recorded")
        .and_then(serde_json::Value::as_array)
    {
        for gap in not_recorded {
            resource_effect_gaps.push(json!({
                "kind": "extraction_fact_not_recorded",
                "record": gap,
            }));
        }
    }
    for assumption in &pending_assumptions {
        resource_effect_gaps.push(json!({
            "kind": "pending_under_model_characterization",
            "assumption_id": assumption.get("id"),
            "status": "provisionally_admitted_not_human_ratified",
        }));
    }
    if resource_effect_gaps.is_empty() && state.is_pv() {
        resource_effect_gaps.push(json!({
            "kind": "resource_or_effect_modeling",
            "status": "no_separate_machine_readable_gap_record",
            "note": "absence of a recorded gap does not establish that resources and effects are fully modeled",
        }));
    }
    gate_sort_json_rows(&mut resource_effect_gaps, "resource/effect gaps")?;

    let mut kernel_axioms = BTreeSet::new();
    let axiom_rows: Vec<serde_json::Value> = state
        .local_closure_records
        .iter()
        .map(|(node, record)| {
            kernel_axioms.extend(record.kernel_axioms.iter().cloned());
            json!({"node": node, "kernel_axioms": record.kernel_axioms})
        })
        .collect();
    let approved_axioms: BTreeSet<String> = crate::model::CANONICAL_APPROVED_AXIOMS
        .iter()
        .map(|axiom| (*axiom).to_string())
        .collect();
    let unapproved_delta: BTreeSet<String> = kernel_axioms
        .difference(&approved_axioms)
        .cloned()
        .collect();
    let axiom_closure_delta = json!({
        "records": axiom_rows,
        "kernel_axioms": kernel_axioms,
        "approved_axioms": approved_axioms,
        "unapproved_delta": unapproved_delta,
    });

    let authored_statement_authority: Vec<serde_json::Value> = state
        .pv_authored_statements
        .iter()
        .map(|(target, binding)| {
            json!({
                "target_id": target,
                "statement_authority_sha256":
                    crate::model::registered_statement_sha256(state, target),
                "node": binding.node,
                "name": binding.name,
                "lean": binding.lean,
                "namespace_context": binding.namespace_context,
                "imports": binding.imports,
                "opens": binding.opens,
            })
        })
        .collect();
    let mut rust_witness_dossiers = crate::trust_base::artifact_gate_dossiers(
        &state.trust_base.rust_witness_artifact_records,
        &state.trust_base.rust_witness_artifact_payloads,
    )
    .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    for dossier in &mut rust_witness_dossiers {
        let target = dossier
            .pointer("/record/target_id")
            .and_then(serde_json::Value::as_str)
            .map(crate::model::ChallengeTargetId::from)
            .ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(
                    "Rust witness dossier lacks its primary target identity".into(),
                )
            })?;
        let primary = state.configured_challenge_targets.get(&target).ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "Rust witness dossier target is absent from the challenge registry".into(),
            )
        })?;
        let refutation_id = crate::model::refutation_target_id(&target);
        let refutation = state
            .configured_challenge_targets
            .get(&refutation_id)
            .ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(
                    "Rust witness dossier lacks its selected refutation statement".into(),
                )
            })?;
        let closure = state
            .local_closure_records
            .get(&NodeId::from(refutation.name.as_str()))
            .ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(
                    "Rust witness dossier lacks its checked Lean disproof closure".into(),
                )
            })?;
        let payload = state
            .trust_base
            .rust_witness_artifact_payloads
            .get(&target)
            .expect("artifact state coherence was checked");
        let cargo_observation = payload
            .execution_receipt
            .as_ref()
            .and_then(|receipt| receipt.get("parsed_stdout"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let object = dossier.as_object_mut().expect("dossier is an object");
        object.insert("goal".into(), goal.clone());
        object.insert("goal_sha256".into(), json!(match &goal {
            serde_json::Value::Object(map) => map
                .get("utf8")
                .and_then(serde_json::Value::as_str)
                .map(|text| raw_sha256(text.as_bytes())),
            _ => None,
        }));
        object.insert("goal_target_prose_utf8".into(), json!(primary.informal));
        object.insert("target_lean_utf8".into(), json!(primary.lean));
        object.insert("selected_result_statement_utf8".into(), json!(refutation.lean));
        object.insert("checked_lean_disproof_closure".into(), json!(closure));
        object.insert("cargo_observation".into(), cargo_observation);
    }
    let sections = vec![
        phase0_section,
        json!({"section": "polarity_ledger", "rows": polarity_ledger}),
        json!({"section": "refutation_dossiers", "rows": refutation_dossiers}),
        json!({
            "section": "authored_statement_authority",
            "rows": authored_statement_authority,
        }),
        json!({"section": "goal", "rows": goal}),
        json!({"section": "covering_statements", "rows": covering_statements}),
        json!({"section": "claim_rows", "rows": claim_rows_section}),
        json!({
            "section": "conditional_ratification_packet",
            "rows": conditional_ratification_packet,
        }),
        json!({
            "section": "rust_witness_artifact_dossiers",
            "label": "corroborating_evidence",
            "rows": rust_witness_dossiers,
        }),
        json!({"section": "adaptation_ledger", "rows": state.trust_base.adaptation_ledger}),
        json!({
            "section": "semantic_closure_definition_bodies",
            "rows": machine_definitions,
        }),
        json!({
            "section": "pending_proposed_assumption_batch",
            "status": "provisionally_admitted_not_human_ratified",
            "rows": pending_assumptions,
            "conditioned_validity_definition_bodies": pending_validity_definitions,
        }),
        json!({
            "section": "reachable_opaque_inventory",
            "rows": state.pv_reachable_opaque_inventory,
        }),
        json!({
            "section": "extraction_provenance_platform_and_build_pins",
            "rows": extraction_provenance,
        }),
        json!({"section": "tcb_manifest", "rows": tcb_manifest}),
        json!({
            "section": "unmodelled_resource_and_effect_gaps",
            "rows": resource_effect_gaps,
        }),
        json!({"section": "challenge_claims", "rows": state.challenge_claims}),
        json!({"section": "open_nodes", "rows": state.live.open_nodes}),
        json!({"section": "axiom_closure_delta", "rows": axiom_closure_delta}),
    ];
    let instrument = json!({
        "authored_seed_section": authored_seed_section,
        "live_disclosure": sections,
        "schema": "trellis-trust-advance-gate-presentation/v1",
    });
    crate::trust_base::canonical_json(&instrument)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))
}

/// Resolve the per-cycle event-log directory.
///
/// The log lives inside the TRACKED repo tree at
/// `<repo>/.trellis-history/event-log/` so checkpoint commits (`git add -A`)
/// version it alongside `supervisor_state.json`. When `repo_path` is unset
/// (headless tests, imports run before a repo is attached) fall back to a
/// runtime-local `<root>/.event-log-fallback/` so the runtime still has a
/// place to append.
pub fn event_log_dir_for(root: &Path, metadata: &RuntimeMetadata) -> PathBuf {
    match metadata.repo_path.as_deref() {
        Some(repo) => repo.join(".trellis-history").join("event-log"),
        None => root.join(".event-log-fallback"),
    }
}

/// The per-cycle event-log file for `cycle` inside `dir`. Lexical name order
/// (zero-padded width 6) equals cycle order equals global index order.
pub fn event_log_cycle_file(dir: &Path, cycle: u32) -> PathBuf {
    dir.join(format!("cycle-{cycle:06}.jsonl"))
}

/// Lexically-sorted absolute paths of the per-cycle event-log files
/// (`cycle-NNNNNN.jsonl`) present in `dir`. Returns an empty vec when the
/// directory does not yet exist.
pub fn event_log_cycle_files(dir: &Path) -> Result<Vec<PathBuf>, RuntimeError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_cycle_file = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|name| name.starts_with("cycle-") && name.ends_with(".jsonl"))
            .unwrap_or(false);
        if is_cycle_file {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeMetadata {
    pub repo_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub native_history_kinds: BTreeSet<String>,
    /// Fresh-runtime lifecycle marker. Repository setup and runtime genesis
    /// necessarily precede checker-server startup (the server itself binds to
    /// this runtime root), so initial closure issuance cannot run inside
    /// `Init`/`InitFromConfig`. A checker-backed `Step`/`Run` must mint every
    /// genesis owner, persist the complete state, and clear this marker before
    /// dispatching any work.
    ///
    /// The serde default is deliberately `false`: a runtime created before
    /// this field existed is legacy state, not an interrupted fresh genesis,
    /// and missing records in it must fail loudly or be handled by the
    /// explicit offline migration tool.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local_closure_initial_issuance_pending: bool,
    /// Replay determinism gate for the fresh-run initial planner. `true` on
    /// runs initialized after the feature landed (stamped at Init /
    /// InitFromConfig); `false` (serde default) on pre-feature metadata.
    /// `seed_state_from_config` seeds `initial_planning` only when this is
    /// set, so `replay_to_event_count` over a pre-feature event log rebuilds
    /// the seed state byte-identically to the original run.
    #[serde(default)]
    pub initial_planning_seeded: bool,
    /// Tolerated legacy inputs (Q1, plan doc 32 Stage 3): the retired
    /// journal/actor-key paths are accepted, ignored, and logged so old
    /// scratch directories load without a hard error.
    #[serde(default)]
    pub trust_journal_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_actor_key_manifest_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_gate_presentation_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_seed_manifest_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_seed_definition_bundle_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_evidence_tool_manifest_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_evidence_tool_root_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_seed_transaction_id: Option<String>,
    #[serde(default)]
    pub trust_advance_gate_episode_id: Option<String>,
    #[serde(default)]
    pub trust_manifest_authority_roots: BTreeMap<String, String>,
    /// Replay determinism gate for periodic coverage re-planning, the exact
    /// `initial_planning_seeded` pattern (a separate flag: initial-planner-era
    /// logs must NOT arm the coverage trigger at replay, or replay would
    /// dispatch a planner where the original run dispatched a worker). `true`
    /// on runs initialized after the feature landed (stamped at Init /
    /// InitFromConfig; imports never fresh-plan, so ImportLegacy /
    /// ImportRevisionProject stamp `false`); `seed_state_from_config` seeds
    /// `ProtocolState.coverage_replanning_source` only when this is set.
    #[serde(default)]
    pub coverage_replanning_seeded: bool,
    /// Replay determinism gate for the plan-review cadence, the same pattern
    /// as `coverage_replanning_seeded` and a SEPARATE flag from it: a
    /// coverage-era log replayed with this armed would dispatch a planner
    /// where the original run dispatched a worker, because the plan-review
    /// trigger fires in the covered regime where the coverage trigger is
    /// false. `true` on runs initialized after the feature landed (stamped at
    /// Init / InitFromConfig; imports never fresh-plan, so ImportLegacy /
    /// ImportRevisionProject stamp `false`); `seed_state_from_config` sets
    /// `ProtocolState.plan_review_cadence_enabled` only when this is set.
    // False must remain absent for byte-identical replay of metadata written
    // before this cadence gate existed, matching the later replay-gate fields.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan_review_cadence_seeded: bool,
    /// Replay determinism gate for the theorem-phase verifier-only
    /// continuation — the exact `initial_planning_seeded` pattern applied
    /// to a TRANSITION instead of a seed: `true` on runs initialized after
    /// the feature landed (stamped at Init / InitFromConfig; imports stamp
    /// `false`); `seed_state_from_config` enables
    /// `ProtocolState::theorem_verifier_only_continuation_enabled` only
    /// when this is set, so `replay_to_event_count` over a pre-feature
    /// event log applies every guard-shaped theorem Continue exactly as
    /// the original run did (worker task installed, Sound deferred one
    /// slot). False stays off the wire (`skip_serializing_if`) so
    /// pre-feature metadata files — and the `CheckpointHookPayload`
    /// bytes the A1 fixtures pin — round-trip byte-identically.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub theorem_verifier_only_continuation_seeded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCheckpoint {
    pub cycle: u32,
    pub phase: Phase,
    pub gate_kind: GateKind,
    pub active_node: Option<NodeId>,
    pub committed: WorkingSnapshot,
    /// Kernel-owned unreachable-node deletion performed at the start of this
    /// cleanup cycle. The event-log command carries the same record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_unreachable_deletion: Option<CleanupUnreachableDeletionRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointHookPayload {
    pub root: PathBuf,
    pub state_path: PathBuf,
    /// Per-cycle event-log directory (`<repo>/.trellis-history/event-log/`).
    /// Informational only — the checkpoint hook stages everything via
    /// `git add -A` and never reads this path.
    pub event_log_dir: PathBuf,
    pub checkpoint_path: PathBuf,
    pub metadata_path: PathBuf,
    pub metadata: RuntimeMetadata,
    pub state: ProtocolState,
    pub checkpoint: RuntimeCheckpoint,
    pub commands: Vec<ProtocolCommand>,
    pub event_count: u64,
    /// True iff `state.global_blockers().is_empty()` at emission time.
    /// Checkpoint hook uses this to write an additional
    /// `supervisor2/clean-NNNNNN` tag so reviewer-driven
    /// `ResetChoice::LastClean` has something to rewind to.
    #[serde(default)]
    pub is_clean: bool,
    /// Q1 gate-decision transaction (plan doc 32 Stage 3): the full
    /// prospective event-log line plus its full trust-record digest — the
    /// exact content of the tracked decision-record file
    /// `.trellis-history/trust-decisions/<digest>.json`.  Skip-serialized so
    /// math-mode hook-payload bytes never carry the key (Codex R2-1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_record: Option<TrustDecisionCarrier>,
}

/// The decision carrier handed to the checkpoint hook: `line_json` is the
/// exact single-line JSON the runtime will append to the event log (no
/// trailing newline), byte-equal to the committed record file's line;
/// `record_sha256` is the full `TrustRecord` self digest whose 12-hex prefix
/// names the `supervisor2/trust-decision-*` tag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustDecisionCarrier {
    pub line_json: String,
    pub record_sha256: crate::trust_base::Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventLogRecord {
    pub index: u64,
    pub event: ProtocolEvent,
    pub commands: Vec<ProtocolCommand>,
    pub phase: Phase,
    pub stage: crate::model::Stage,
    pub cycle: u32,
    /// Wall-clock timestamp when this record was appended to the log, in
    /// milliseconds since the Unix epoch. `#[serde(default)]` keeps older
    /// event logs (without the field) parseable — they'll read as 0.
    #[serde(default)]
    pub ts_ms: u64,
    /// Q1 (plan doc 32 Stage 3): the required-v1 trust record attached to
    /// the line of the step in which the decision/station outcome occurred.
    /// Math-mode lines omit the key entirely (byte-identical logs); older
    /// logs load via `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_record: Option<TrustRecord>,
    /// A decision-bearing transition can produce a second, non-terminal
    /// trust fact at the same atomic boundary.  Claim-B authorization is
    /// the first such transaction: `AuditAuthorization` is the tagged
    /// decision record and `ConditionalAuthorized` binds the ledger row.
    /// Both live in the same prospective line committed by the decision
    /// carrier.  Empty on every pre-Claim-B and math-mode line, preserving
    /// their bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_trust_records: Vec<TrustRecord>,
}

const CHECKPOINT_TRANSACTION_JOURNAL_FILENAME: &str = "checkpoint_transaction.pending.json";
const CHECKPOINT_TRANSACTION_JOURNAL_SCHEMA_VERSION: u32 = 1;

/// Runtime-local intent written before the external checkpoint hook runs.
///
/// The hook commits the post-step state into Git before `protocol_state.json`
/// and the event log advance.  If the process stops in that interval, this
/// compact journal supplies the otherwise-missing event while hashes bind it
/// to the exact post-step state committed at HEAD.  It is deliberately not a
/// protocol/event field and never enters worker or audit result allowlists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PendingCheckpointTransaction {
    schema_version: u32,
    repo_path: PathBuf,
    pre_commit_head: Option<String>,
    post_state_sha256: String,
    metadata_sha256: String,
    checkpoint_sha256: String,
    event_record: EventLogRecord,
    is_clean: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeStepStatus {
    Transitioned,
    /// Required-v1 has committed final cleanup and is parked at the Q7
    /// barrier until the finalization archive (with its embedded approval
    /// record) assembles and verifies. The supervisor must stop
    /// dispatching agents; no human response is requested.
    PackageReady,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStepOutcome {
    pub status: RuntimeStepStatus,
    pub event: Option<ProtocolEvent>,
    pub commands: Vec<ProtocolCommand>,
}

pub trait WrapperAdapter {
    fn dispatch(&mut self, request: &WrapperRequest) -> Result<WrapperResponse, String>;

    /// Whether the adapter returns a judgment produced before this dispatch.
    /// Wrappers must forward this capability so an outer referent guard cannot
    /// mistake an already-produced response for a live external judgment.
    fn judgment_is_precomputed(&self) -> bool {
        false
    }
}

pub trait CheckpointSink {
    fn commit(&mut self, payload: &CheckpointHookPayload) -> Result<(), String>;

    /// Durable-sink capability gate (Codex R2-4): a trust gate decision must
    /// never "commit" through a no-op sink into rewindable files alone.
    /// Only the process checkpoint hook — which writes the tracked decision
    /// record file and the never-deleted `supervisor2/trust-decision-*`
    /// tag — returns `true`.
    fn provides_durable_decision_tags(&self) -> bool {
        false
    }
}

/// Runtime I/O hook which closes the source/record transaction. It runs after
/// every kernel-authored source mutation (including command-side file moves
/// and generated obligation materialization) and before checkpoint/state
/// persistence. The production supervisor supplies the checker-backed
/// implementation; pure engine/runtime tests may use the no-op implementation.
pub trait LocalClosureIssuer {
    fn reconcile_authoritative_source_apply(
        &mut self,
        pre_state: &ProtocolState,
        post_state: &mut ProtocolState,
        repo_path: &Path,
        event: &ProtocolEvent,
        commands: &[ProtocolCommand],
    ) -> Result<(), RuntimeError>;
}

#[derive(Default)]
pub struct NoopLocalClosureIssuer;

impl LocalClosureIssuer for NoopLocalClosureIssuer {
    fn reconcile_authoritative_source_apply(
        &mut self,
        _pre_state: &ProtocolState,
        _post_state: &mut ProtocolState,
        _repo_path: &Path,
        _event: &ProtocolEvent,
        _commands: &[ProtocolCommand],
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[derive(Default)]
pub struct NoopCheckpointSink;

impl CheckpointSink for NoopCheckpointSink {
    fn commit(&mut self, _payload: &CheckpointHookPayload) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Io(std::io::Error),
    Serde(serde_json::Error),
    Kernel(TransitionError),
    Adapter(String),
    CheckpointSink(String),
    InvalidRuntimeState(String),
    MissingLocalClosureRecord {
        node: crate::model::NodeId,
        kind: crate::model::NodeKind,
        expected_apply_site: &'static str,
    },
    StaleLocalClosureRecord {
        node: crate::model::NodeId,
        kind: crate::model::NodeKind,
        expected_apply_site: &'static str,
    },
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Serde(err) => write!(f, "serde error: {err}"),
            Self::Kernel(err) => write!(f, "kernel error: {:?}", err),
            Self::Adapter(err) => write!(f, "adapter error: {err}"),
            Self::CheckpointSink(err) => write!(f, "checkpoint sink error: {err}"),
            Self::InvalidRuntimeState(err) => write!(f, "invalid runtime state: {err}"),
            Self::MissingLocalClosureRecord {
                node,
                kind,
                expected_apply_site,
            } => write!(
                f,
                "missing local-closure record: node={} kind={kind:?} expected_apply_site={expected_apply_site}",
                node.as_str()
            ),
            Self::StaleLocalClosureRecord {
                node,
                kind,
                expected_apply_site,
            } => write!(
                f,
                "stale local-closure record without matching invalidation: node={} kind={kind:?} expected_apply_site={expected_apply_site}",
                node.as_str()
            ),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<std::io::Error> for RuntimeError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value)
    }
}

impl From<TransitionError> for RuntimeError {
    fn from(value: TransitionError) -> Self {
        Self::Kernel(value)
    }
}

/// Keep the pure transition's large debug-build return slot out of the
/// runtime durability frame. `ProtocolState` contains several wide trust
/// carriers; allowing this call to inline (or returning its outcome directly
/// into `step_committed_event_with_checkpoint_sink`) can exhaust Rust's
/// default 2 MiB test-thread stack on distribution re-pin events.
#[inline(never)]
fn apply_event_boxed(
    state: ProtocolState,
    event: ProtocolEvent,
) -> Result<Box<crate::engine::TransitionOutcome>, TransitionError> {
    apply_event(state, event).map(Box::new)
}

/// Fail-loud producer invariant used at operational boundaries. Loading a
/// pre-certificate state remains legal; beginning normal work does not.
pub fn require_local_closure_records(
    state: &ProtocolState,
    expected_apply_site: &'static str,
) -> Result<(), RuntimeError> {
    for node in state
        .live
        .present_nodes
        .iter()
        .filter(|node| state.local_closure_owner_eligible(node))
    {
        if !state.local_closure_records.contains_key(node) {
            return Err(RuntimeError::MissingLocalClosureRecord {
                node: node.clone(),
                kind: state
                    .node_kinds
                    .get(node)
                    .copied()
                    .unwrap_or(crate::model::NodeKind::Proof),
                expected_apply_site,
            });
        }
    }
    Ok(())
}

pub struct SupervisorRuntime {
    paths: RuntimePaths,
    state: ProtocolState,
    metadata: RuntimeMetadata,
    event_count: u64,
}

const ACTIVE_WORKER_BASE_SCHEMA_VERSION: u32 = 2;
const CERTIFICATE_ARTIFACT_EPOCH_SCHEMA_VERSION: u32 = 1;

/// Presence is part of the snapshot: in particular, an absent `reference/`
/// at dispatch must be absent again after rollback even though entering the
/// worker sandbox creates that writable directory on demand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveWorkerBaseManifest {
    schema_version: u32,
    tablet_present: bool,
    reference_present: bool,
    artifact_epoch: crate::trust_base::Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CertificateArtifactEpochEntry {
    relative_path: String,
    sha256: crate::trust_base::Sha256Digest,
    size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CertificateArtifactEpochContent {
    artifact_root_present: bool,
    entries: Vec<CertificateArtifactEpochEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CertificateArtifactEpochManifest {
    schema_version: u32,
    epoch: crate::trust_base::Sha256Digest,
    artifact_root_present: bool,
    entries: Vec<CertificateArtifactEpochEntry>,
}

impl SupervisorRuntime {
    fn acceptance_disagreement_error(
        &self,
        pre_step_state: &ProtocolState,
        response: &WorkerResponse,
        engine_rejection: String,
    ) -> RuntimeError {
        let acceptance_logic_identity = pre_step_state
            .in_flight_request
            .as_deref()
            .map(|request| request.acceptance_logic_identity.as_str())
            .unwrap_or_default();
        let marker_result = crate::runtime_cli_observations::write_acceptance_transition_disagreement_halt_marker_at(
            &self.paths.root,
            response.cycle,
            response.request_id,
            acceptance_logic_identity,
            &engine_rejection,
        );
        let marker_diagnostic = marker_result
            .map(|path| format!("halt marker: {}", path.display()))
            .unwrap_or_else(|error| format!("halt marker write also failed: {error}"));
        RuntimeError::InvalidRuntimeState(format!(
            "worker-runnable acceptance checker succeeded but the engine rejected the same response: {engine_rejection}; {marker_diagnostic}"
        ))
    }

    /// A checker-accepted worker response the engine refused must leave no
    /// replayable residue once the kernel restores the worktree out from
    /// under it. The bridge replays `bridge/latest_worker.json` (and,
    /// failing that, re-normalizes the staging `.done` artifact) verbatim
    /// for a matching `(kind, request_id, cycle)`; neither layer consults
    /// the worktree, and the pre-accept delta guard
    /// (`refuse_response_whose_delta_is_not_on_disk`) is presence-only by
    /// design — a content-only delta, the modal cleanup shape, passes it.
    /// So every pre-halt restore manufactures a post-delta-response /
    /// pre-delta-worktree pair: on resume the replay is re-certified
    /// against pre-delta bytes and the post-accept reconcile "repairs"
    /// records to pre-edit sources — a silent discard of accepted work
    /// with a lying event log. Retiring the residue at the same sites that
    /// restore forces a clean re-dispatch of the burst on resume.
    ///
    /// Rename-aside with a timestamp, never delete: these files are the
    /// primary forensic artifacts for exactly this halt class. Missing
    /// files are a no-op; failures are returned as diagnostics for the
    /// halt reason rather than errors, so retirement can never mask the
    /// disagreement itself.
    fn retire_replayable_worker_response_residue(&self, request_id: u32) -> Vec<String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let mut diagnostics = Vec::new();
        let mut retire = |path: PathBuf| {
            if !path.exists() {
                return;
            }
            let mut aside = path.clone().into_os_string();
            aside.push(format!(".retired-{ts}"));
            let aside = PathBuf::from(aside);
            match fs::rename(&path, &aside) {
                Ok(()) => diagnostics.push(format!(
                    "retired replayable response residue {} -> {}",
                    path.display(),
                    aside.display()
                )),
                Err(error) => diagnostics.push(format!(
                    "failed to retire replayable response residue {}: {error}",
                    path.display()
                )),
            }
        };
        retire(self.paths.root.join("bridge").join("latest_worker.json"));
        if let (Some(repo_path), Some(runtime_name)) = (
            self.metadata.repo_path.as_deref(),
            self.paths.root.file_name(),
        ) {
            retire(
                repo_path
                    .join(".trellis")
                    .join("runtime")
                    .join(runtime_name)
                    .join("staging")
                    .join(format!("trellis_worker_{request_id}_result.done")),
            );
        }
        diagnostics
    }

    fn active_worker_base_dir(&self) -> PathBuf {
        self.paths.root.join("active_worker_base")
    }

    fn active_worker_base_manifest_path(&self) -> PathBuf {
        self.active_worker_base_dir().join("worker_surfaces.json")
    }

    fn active_worker_base_tablet_dir(&self) -> PathBuf {
        self.active_worker_base_dir().join("Tablet")
    }

    fn active_worker_base_reference_dir(&self) -> PathBuf {
        self.active_worker_base_dir().join("reference")
    }

    fn certificate_artifact_store_dir(&self) -> PathBuf {
        self.paths.root.join("certificate-artifact-store")
    }

    fn certificate_artifact_blob_dir(&self) -> PathBuf {
        self.certificate_artifact_store_dir().join("blobs")
    }

    fn certificate_artifact_epoch_dir(&self) -> PathBuf {
        self.certificate_artifact_store_dir().join("epochs")
    }

    fn certificate_artifact_blob_path(&self, digest: crate::trust_base::Sha256Digest) -> PathBuf {
        self.certificate_artifact_blob_dir()
            .join(digest.to_string())
    }

    fn certificate_artifact_epoch_path(&self, epoch: crate::trust_base::Sha256Digest) -> PathBuf {
        self.certificate_artifact_epoch_dir()
            .join(format!("{epoch}.json"))
    }

    fn capture_certificate_artifact_epoch(
        &self,
        state: &ProtocolState,
        repo_path: &Path,
    ) -> Result<crate::trust_base::Sha256Digest, RuntimeError> {
        let workspace = certificate_workspace_for_repo(repo_path);
        let artifact_root = certificate_artifact_root(&workspace);
        let content = capture_certificate_artifact_epoch_content(&artifact_root)?;
        validate_certificate_artifact_epoch_against_state(state, &content)?;
        fs::create_dir_all(self.certificate_artifact_blob_dir())?;
        fs::create_dir_all(self.certificate_artifact_epoch_dir())?;
        for entry in &content.entries {
            let source = artifact_root.join(&entry.relative_path);
            let bytes = fs::read(&source)?;
            publish_immutable_artifact_file(
                &self.certificate_artifact_blob_path(entry.sha256),
                &bytes,
                entry.sha256,
                entry.size_bytes,
            )?;
        }
        let canonical = serde_json::to_vec(&content)?;
        let epoch = raw_sha256(&canonical);
        let manifest = CertificateArtifactEpochManifest {
            schema_version: CERTIFICATE_ARTIFACT_EPOCH_SCHEMA_VERSION,
            epoch,
            artifact_root_present: content.artifact_root_present,
            entries: content.entries,
        };
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        publish_immutable_artifact_file(
            &self.certificate_artifact_epoch_path(epoch),
            &bytes,
            raw_sha256(&bytes),
            bytes.len() as u64,
        )?;
        Ok(epoch)
    }

    fn load_certificate_artifact_epoch(
        &self,
        state: &ProtocolState,
        epoch: crate::trust_base::Sha256Digest,
    ) -> Result<CertificateArtifactEpochContent, RuntimeError> {
        let path = self.certificate_artifact_epoch_path(epoch);
        let manifest: CertificateArtifactEpochManifest = serde_json::from_slice(
            &read_regular_file_without_symlinks(&path, "certificate artifact epoch")?,
        )
        .map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "certificate artifact epoch {} is malformed: {error}",
                path.display()
            ))
        })?;
        if manifest.schema_version != CERTIFICATE_ARTIFACT_EPOCH_SCHEMA_VERSION {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "certificate artifact epoch {} has unsupported schema_version {}; expected {}",
                path.display(),
                manifest.schema_version,
                CERTIFICATE_ARTIFACT_EPOCH_SCHEMA_VERSION,
            )));
        }
        if manifest.epoch != epoch {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "certificate artifact epoch {} names {}, expected {epoch}",
                path.display(),
                manifest.epoch,
            )));
        }
        let content = CertificateArtifactEpochContent {
            artifact_root_present: manifest.artifact_root_present,
            entries: manifest.entries,
        };
        let actual_epoch = raw_sha256(&serde_json::to_vec(&content)?);
        if actual_epoch != epoch {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "certificate artifact epoch {} content hashes to {actual_epoch}, expected {epoch}",
                path.display(),
            )));
        }
        validate_certificate_artifact_epoch_against_state(state, &content)?;
        for entry in &content.entries {
            validate_immutable_artifact_file(
                &self.certificate_artifact_blob_path(entry.sha256),
                entry.sha256,
                entry.size_bytes,
                "certificate artifact blob",
            )?;
        }
        Ok(content)
    }

    fn stage_certificate_artifact_epoch(
        &self,
        repo_path: &Path,
        content: &CertificateArtifactEpochContent,
    ) -> Result<PathBuf, RuntimeError> {
        let workspace = certificate_workspace_for_repo(repo_path);
        let artifact_root = certificate_artifact_root(&workspace);
        let parent = artifact_root.parent().ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(format!(
                "certificate artifact root {} has no parent",
                artifact_root.display()
            ))
        })?;
        fs::create_dir_all(parent)?;
        let stage = parent.join(format!(
            ".Tablet.artifact-epoch-stage-{}",
            std::process::id()
        ));
        remove_path_without_following_symlinks(&stage)?;
        if content.artifact_root_present {
            fs::create_dir_all(&stage)?;
            set_dir_mode_group_writable(&stage)?;
            for entry in &content.entries {
                let destination = safe_artifact_epoch_join(&stage, &entry.relative_path)?;
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                    set_dir_mode_group_writable(parent)?;
                }
                materialize_immutable_artifact(
                    &self.certificate_artifact_blob_path(entry.sha256),
                    &destination,
                )?;
                validate_immutable_artifact_file(
                    &destination,
                    entry.sha256,
                    entry.size_bytes,
                    "staged certificate artifact",
                )?;
            }
        }
        Ok(stage)
    }

    fn install_staged_certificate_artifact_epoch(
        &self,
        repo_path: &Path,
        content: &CertificateArtifactEpochContent,
        stage: &Path,
    ) -> Result<(), RuntimeError> {
        let workspace = certificate_workspace_for_repo(repo_path);
        let artifact_root = certificate_artifact_root(&workspace);
        let parent = artifact_root.parent().ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(format!(
                "certificate artifact root {} has no parent",
                artifact_root.display()
            ))
        })?;
        let backup = parent.join(format!(
            ".Tablet.artifact-epoch-backup-{}",
            std::process::id()
        ));
        remove_path_without_following_symlinks(&backup)?;
        let had_artifact_root = match fs::symlink_metadata(&artifact_root) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                fs::rename(&artifact_root, &backup)?;
                true
            }
            Ok(_) => {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "certificate artifact root {} is not a directory",
                    artifact_root.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(RuntimeError::Io(error)),
        };

        let install_result = if content.artifact_root_present {
            fs::rename(stage, &artifact_root).map_err(RuntimeError::Io)
        } else {
            Ok(())
        };
        if let Err(error) = install_result {
            if had_artifact_root {
                let _ = fs::rename(&backup, &artifact_root);
            }
            return Err(error);
        }
        if had_artifact_root {
            remove_path_without_following_symlinks(&backup)?;
        }
        Ok(())
    }

    fn capture_active_worker_base_for_request(
        &self,
        state: &ProtocolState,
        request: &crate::model::WrapperRequest,
    ) -> Result<(), RuntimeError> {
        if request.kind != crate::model::RequestKind::Worker {
            return Ok(());
        }
        state
            .validate_local_closure_root_consistency()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        let repo_path = match self.metadata.repo_path.as_deref() {
            Some(path) => path,
            None => return Ok(()),
        };
        let tablet_dir = repo_path.join("Tablet");
        let reference_dir = repo_path.join("reference");
        let tablet_present = worker_surface_directory_present(&tablet_dir)?;
        let reference_present = worker_surface_directory_present(&reference_dir)?;
        let artifact_epoch = self.capture_certificate_artifact_epoch(state, repo_path)?;
        let capture_root = self.active_worker_base_dir();
        if capture_root.exists() {
            fs::remove_dir_all(&capture_root)?;
        }
        fs::create_dir_all(&capture_root)?;
        if tablet_present {
            copy_dir_recursive(&tablet_dir, &self.active_worker_base_tablet_dir())?;
        }
        if reference_present {
            copy_dir_recursive(&reference_dir, &self.active_worker_base_reference_dir())?;
        }
        let manifest = ActiveWorkerBaseManifest {
            schema_version: ACTIVE_WORKER_BASE_SCHEMA_VERSION,
            tablet_present,
            reference_present,
            artifact_epoch,
        };
        fs::write(
            self.active_worker_base_manifest_path(),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        Ok(())
    }

    pub fn initialize(paths: RuntimePaths, state: ProtocolState) -> Result<Self, RuntimeError> {
        Self::initialize_with_metadata(paths, state, RuntimeMetadata::default())
    }

    pub fn initialize_with_metadata(
        paths: RuntimePaths,
        mut state: ProtocolState,
        metadata: RuntimeMetadata,
    ) -> Result<Self, RuntimeError> {
        fs::create_dir_all(&paths.root)?;
        state.normalize_all_structural_state();
        let mut runtime = Self {
            paths,
            state,
            metadata,
            event_count: 0,
        };
        runtime.reconcile_trust_gate_record()?;
        // Fresh CLI initialization seeds configured Decide definitions and
        // materializes their files as separate lifecycle operations. Do not
        // inspect the intermediate worktree here. The first load (before any
        // dispatch), every post-step state, and active-worker restoration all
        // validate the complete disk layout fail-closed.
        if runtime.state.trust_base.required() {
            runtime
                .state
                .validate()
                .map_err(RuntimeError::InvalidRuntimeState)?;
        }
        runtime.persist_state()?;
        runtime.persist_metadata()?;
        Ok(runtime)
    }

    pub fn load(paths: RuntimePaths) -> Result<Self, RuntimeError> {
        Self::load_internal(paths)
    }

    fn load_internal(paths: RuntimePaths) -> Result<Self, RuntimeError> {
        let mut state: ProtocolState =
            serde_json::from_str(&fs::read_to_string(&paths.state_path)?)?;
        let mut metadata = read_metadata(&paths.metadata_path)?;
        // Audit #10: the checkpoint hook commits the canonical post-step
        // state before the runtime persists its own state/event files.  A
        // pre-hook journal binds the missing event to that exact committed
        // state, allowing this load boundary to finish the transaction
        // idempotently before any state/disk fingerprint validation runs.
        recover_pending_checkpoint_transaction(&paths, &mut state, &mut metadata)?;
        let event_count = read_event_count(&event_log_dir_for(&paths.root, &metadata))?;
        // Misorder guard (segmentation migration): an ABSENT/empty
        // event-log dir is indistinguishable from a cold start by count
        // alone. If the loaded state is non-initial (cycle >= 1 implies
        // at least the start_cycle event was appended), an empty dir
        // means the operator launched a segmentation-aware binary
        // before running `segment_event_log` — appending would restart
        // the dense index at 0 and corrupt the log. Fail loud instead.
        if event_count == 0 && state.cycle >= 1 {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "event-log dir is absent/empty but the loaded state is non-initial \
                 (cycle {}); run `segment_event_log` (or restore the per-cycle \
                 files) before launching this binary",
                state.cycle
            )));
        }
        let mut runtime = Self {
            paths,
            state,
            metadata,
            event_count,
        };
        runtime.state.normalize_all_structural_state();
        // Decide-pair registration backfill (audit round 2 B2; ORDERING fix,
        // round-2 follow-up 1). This has to run HERE, not from the CLI's
        // post-load migration block, because on a `TrustBaseMode::RequiredV1`
        // run `reconcile_trust_gate_record` below ends in
        // `ProtocolState::validate()` — the first gate a trust-required load
        // hits. Run afterwards, the migration's `node_kinds` repair arm (and
        // the `open_nodes` re-derivation that guards it) was unreachable on
        // exactly the class of run it was written for: validate() rejects a
        // wrong/absent Decide-pair `node_kinds` value before the repair can
        // touch it. Running it first also puts it ahead of
        // `migrate_corr_fingerprint_schema` /
        // `migrate_soundness_fingerprint_schema_if_enabled`, both of which key
        // off `node_kinds` and would otherwise recompute against a kind this
        // migration is about to correct.
        //
        // `validate()` is NOT weakened: it still runs, unchanged, immediately
        // after — this only gives a REPAIRABLE state the chance to be repaired
        // before it is judged. Anything the migration cannot re-derive
        // (unconfigured target, second claimant, a shape outside its scope)
        // still fails loud, now with the migration's own diagnostic printed
        // first.
        //
        // MATH-MODE NEUTRALITY: the migration's body is a loop over
        // `configured_challenge_targets` filtered by `is_decide_primary`, so a
        // non-PV state returns `Ok(false)` before touching a field and nothing
        // is persisted. The `repo_path` + `Tablet/` guard is carried over
        // verbatim from the CLI call site, so the set of loads on which it does
        // any work is unchanged — only its position in the load order moved.
        //
        // Paired safety change: the migration now defers under ANY in-flight
        // request, not just a Worker burst. `validate()` pins the persisted
        // request against `expected_request` recomputed from the state, and
        // that projection reads the facts this migration writes — so repairing
        // ahead of validate() while a request is in flight would desync the
        // pair and hard-fail the load. Deferring costs nothing: the repair is
        // idempotent and lands at the next idle load.
        //
        // Known ordering nuance: this now precedes
        // `recover_interrupted_configured_decide_flips` below, so on a load
        // that catches a TORN Decide file move the `open_nodes` re-derivation
        // reads disk before the layout is repaired and may under-state
        // openness. That combination (torn move AND a stranded kind AND an
        // active closure tier) still fails loud in validate() — i.e. no worse
        // than today, where such a state is rejected outright.
        let mut persist_after_load = match runtime.metadata.repo_path.clone() {
            Some(repo_path) if repo_path.join("Tablet").is_dir() => {
                crate::runtime_cli_observations::migrate_stranded_decide_registration(
                    &mut runtime.state,
                    &repo_path,
                )
                .map_err(RuntimeError::InvalidRuntimeState)?
            }
            _ => false,
        };
        // Reverse indices are `#[serde(skip)]` — rebuild them from the
        // freshly-loaded `local_closure_records` before any code path can
        // run `validate()`, otherwise the new H-1 reverse-index assert
        // will fire on the first event after restart.
        crate::model::recompute_local_closure_reverse_indices(&mut runtime.state);
        // The persisted in-flight request includes dispatch-only execution
        // hints attached immediately before a burst: actor/verifier bindings,
        // prompt contracts, and `fresh_context`.  `ProtocolState::validate`,
        // however, compares the request with the semantic projection produced
        // by `expected_request` (whose execution hints are deliberately
        // unresolved).  Trust-v1 reconciliation validates the state below, so
        // normalize ONLY those execution-hint fields before entering the trust
        // boundary.  Replacing the whole request here would erase semantic or
        // seed-bound tampering before validation could reject it.
        //
        // After the recorded trust projection and seed closure are verified,
        // the ordinary full refresh below reflects the verified state;
        // execution hints are then reattached from the pinned runtime config.
        runtime.normalize_in_flight_request_execution_hints_for_validation();
        persist_after_load |= runtime.reconcile_trust_gate_record_inner()?;
        persist_after_load |= runtime.apply_sound_assessment_schema_cutover()?;
        let pre_heal_coarse_count = runtime.state.coarse_dag_nodes.len();
        runtime.heal_coarse_dag_from_git_if_needed();
        // Persist if the heal actually changed something. Without this the
        // heal lives only in memory until the next step writes — and step
        // can take many minutes (post-restart materialize-tablet-oleans is
        // typically 5-15 min before the first step write). Persisting here
        // makes the heal durable: a supervisor crash mid-materialize won't
        // require the next operator restart to re-discover the empty field.
        if runtime.state.coarse_dag_nodes.len() != pre_heal_coarse_count {
            persist_after_load = true;
        }
        if persist_after_load {
            runtime.persist_state()?;
        }
        runtime.refresh_in_flight_request_from_state();
        runtime.apply_request_dispatch_hints()?;
        // A crashed Worker may have left its writable Tablet/reference trees
        // in a partial state. Loading must remain possible so the bridge can
        // invoke `restore_active_worker_base_for_inflight`; that restore
        // validates the Decide layout before relaunch. Every non-Worker load
        // must already have an authoritative disk layout and validates here.
        let worker_restore_pending = runtime
            .state
            .in_flight_request
            .as_ref()
            .is_some_and(|request| request.kind == crate::model::RequestKind::Worker);
        if !worker_restore_pending {
            if let Some(repo_path) = runtime.metadata.repo_path.as_deref() {
                crate::dormant_store::recover_interrupted_configured_decide_flips(
                    repo_path,
                    &runtime.state,
                )
                .map_err(RuntimeError::InvalidRuntimeState)?;
                crate::dormant_store::validate_configured_decide_layout(repo_path, &runtime.state)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
            }
        }
        // Atomicity (audit, Option C): refuse to start if the loaded
        // state's last_clean readiness is internally inconsistent with
        // git. Fail loud here so a downstream reviewer-driven LastClean
        // doesn't either fail mid-step or silently rewind to a stale
        // tag describing a different state.
        runtime.validate_last_clean_tag_consistency()?;
        Ok(runtime)
    }

    /// Reconcile the durable trust projection for an already loaded, idle
    /// runtime: the persisted gate-decision records in the event log plus
    /// the git-tag decision history (Q1, plan doc 32 Stage 3).
    pub fn reconcile_external_trust_authority(&mut self) -> Result<(), RuntimeError> {
        if self.state.in_flight_request.is_some() {
            return Err(RuntimeError::InvalidRuntimeState(
                "external trust authority may be reconciled only at an idle request boundary"
                    .into(),
            ));
        }
        if self.reconcile_trust_gate_record()? {
            self.persist_state()?;
        }
        Ok(())
    }

    /// Rebuild the trust projection from the durable Q1 sources before any
    /// rewindable checkpoint is trusted (plan doc 32 Stage 3): (a) the
    /// persisted gate-decision records in the event log, (b) the
    /// `supervisor2/trust-decision-*` tag history, and (c) the persisted
    /// gate-presentation hash store.  A tag-ahead state is completed
    /// deterministically ONLY under the exact crash-adjacency guard (Codex
    /// R3-4a.2) — which binds REPOSITORY state (current `HEAD` must be the
    /// decision tag's own commit; Stage-4 audit fix, Codex 3), since every
    /// protocol-state binding rewinds together under the documented rewind
    /// procedure; everywhere else the tag history is historical lookup —
    /// nothing is appended at an old index and no decision from an
    /// abandoned timeline is auto-enacted.
    fn reconcile_trust_gate_record(&mut self) -> Result<bool, RuntimeError> {
        self.reconcile_trust_gate_record_inner()
    }

    fn reconcile_trust_gate_record_inner(&mut self) -> Result<bool, RuntimeError> {
        if !self.state.trust_base.required() {
            return Ok(false);
        }
        // Tolerated legacy inputs (Q1): journal/actor-key metadata paths are
        // accepted, ignored, and logged once.
        if self.metadata.trust_journal_path.is_some()
            || self.metadata.trust_actor_key_manifest_path.is_some()
        {
            static LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "note: runtime metadata carries retired trust journal/actor-key paths; \
                     they are ignored (Q1, plan doc 32 Stage 3)"
                );
            }
        }
        let mut log_records = self.scan_event_log_trust_records()?;
        let tag_records = self.load_trust_decision_tag_records()?;
        // Deterministic completion — crash adjacency ONLY (item 4, Codex
        // R3-4a.2; kind-generic per Stage-3 fix 4; repository-bound per the
        // Stage-4 fix, Codex 3): a decision tag whose record is absent from
        // the log, whose intended index equals the CURRENT event count, and
        // whose target commit is the repository's current HEAD completes
        // under EXACT per-kind bindings —
        // pre-state (state not yet advanced: re-apply the recorded
        // transition and append) or post-state (state persisted, log
        // missing: append only).  Every other configuration is lookup-only.
        // Runs BEFORE the old_* captures below so the projection arms see
        // the completed shape and the change flag forces a persist.
        let completed = self.maybe_complete_tag_ahead_decision(&mut log_records, &tag_records)?;
        let old_approval = self.state.trust_base.current_human_approval_event_hash;
        let old_gate_state = self.state.trust_base.routine_gate_state;
        let old_revision_lane = self.state.trust_base.active_revision_lane_id.clone();
        let old_conditional_candidates = self.state.trust_base.conditional_candidates.clone();

        // Routine advance-gate projection.
        let advance_records: Vec<&TrustRecord> = log_records
            .iter()
            .filter(|record| {
                matches!(
                    record.kind,
                    EventKind::AdvanceGateApproved | EventKind::AdvanceGateFeedback
                )
            })
            .collect();
        if advance_records.len() > 1 {
            return Err(RuntimeError::InvalidRuntimeState(
                "the event log records more than one routine advance-gate decision".into(),
            ));
        }
        match advance_records.first() {
            None => {
                // No routine decision in the log.  A state claiming a
                // terminal outcome must be corroborated by the surviving
                // tag history (lookup only) — e.g. a manual rewind
                // truncated the log line while the runtime root kept the
                // decided state.
                match self.state.trust_base.routine_gate_state {
                    crate::model::TrustRoutineGateState::NotPresented
                    | crate::model::TrustRoutineGateState::ApprovalCommitPending
                    | crate::model::TrustRoutineGateState::FeedbackCommitPending => {
                        if self
                            .state
                            .trust_base
                            .current_human_approval_event_hash
                            .is_some()
                        {
                            return Err(RuntimeError::InvalidRuntimeState(
                                "checkpoint claims a trust-gate result absent from the recorded trust decisions"
                                    .into(),
                            ));
                        }
                    }
                    crate::model::TrustRoutineGateState::Approved => {
                        let approval = self
                            .state
                            .trust_base
                            .current_human_approval_event_hash
                            .ok_or_else(|| {
                                RuntimeError::InvalidRuntimeState(
                                    "approved required-v1 state lost its approval digest".into(),
                                )
                            })?;
                        // Both approved terminal kinds corroborate (Stage-3
                        // fix 5): after an exceptional revision the current
                        // approval is the ProtectedReapprovalApproved digest.
                        let corroborated = tag_records.iter().any(|tag| {
                            matches!(
                                tag.record.kind,
                                EventKind::AdvanceGateApproved
                                    | EventKind::ProtectedReapprovalApproved
                            ) && tag.record.record_sha256 == approval
                        });
                        if !corroborated {
                            return Err(RuntimeError::InvalidRuntimeState(
                                "checkpoint claims a trust-gate result absent from the recorded trust decisions"
                                    .into(),
                            ));
                        }
                    }
                    crate::model::TrustRoutineGateState::FeedbackTerminated => {
                        let corroborated = tag_records
                            .iter()
                            .any(|tag| tag.record.kind == EventKind::AdvanceGateFeedback);
                        if !corroborated {
                            return Err(RuntimeError::InvalidRuntimeState(
                                "checkpoint claims a trust-gate result absent from the recorded trust decisions"
                                    .into(),
                            ));
                        }
                    }
                }
            }
            Some(record) if record.kind == EventKind::AdvanceGateApproved => {
                if self.state.trust_base.routine_gate_state
                    != crate::model::TrustRoutineGateState::Approved
                {
                    crate::engine::recover_trust_advance_approval(&mut self.state)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                self.state.trust_base.routine_gate_state =
                    crate::model::TrustRoutineGateState::Approved;
                // Stage-3 fix 5 (Claude 3): a later protected reapproval
                // SUPERSEDES the routine approval digest — reconcile must
                // not flip the state's approval back to the advance digest
                // on restart. The persisted approval survives iff it is
                // corroborated as a ProtectedReapprovalApproved record in
                // the log or the surviving tag history; anything else
                // re-installs the routine advance digest as before.
                let superseding_protected_approval = self
                    .state
                    .trust_base
                    .current_human_approval_event_hash
                    .filter(|current| {
                        log_records.iter().any(|logged| {
                            logged.kind == EventKind::ProtectedReapprovalApproved
                                && logged.record_sha256 == *current
                        }) || tag_records.iter().any(|tag| {
                            tag.record.kind == EventKind::ProtectedReapprovalApproved
                                && tag.record.record_sha256 == *current
                        })
                    });
                self.state.trust_base.current_human_approval_event_hash =
                    Some(superseding_protected_approval.unwrap_or(record.record_sha256));
                self.state.trust_base.gate_commit_pending = false;
            }
            Some(_feedback) => {
                if self.state.trust_base.routine_gate_state
                    != crate::model::TrustRoutineGateState::FeedbackTerminated
                {
                    crate::engine::recover_trust_advance_feedback(&mut self.state)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                self.state.trust_base.routine_gate_state =
                    crate::model::TrustRoutineGateState::FeedbackTerminated;
                self.state.trust_base.current_human_approval_event_hash = None;
                self.state.trust_base.gate_commit_pending = false;
            }
        }

        // Exceptional-lane projection (audit F9): an active lane is exactly
        // an `AuditAuthorization` record with no subsequent
        // `ProtectedReapproval{Approved,Feedback}` terminal for that lane.
        // A manual rewind may have truncated the terminal LINE while its
        // durable decision tag survives (fail-closed matrix rows 2-3), so
        // the tag history is consulted — lookup only — before concluding a
        // log-open lane is still open.
        let mut projected_lane = project_open_revision_lane(&log_records);
        let mut lane_open_in_log = false;
        if let Some(open_lane) = projected_lane.clone() {
            lane_open_in_log = true;
            let logged: std::collections::BTreeSet<crate::trust_base::Sha256Digest> = log_records
                .iter()
                .map(|record| record.record_sha256)
                .collect();
            let tag_terminal = tag_records.iter().any(|tag| {
                tag.record.lane.as_deref() == Some(open_lane.as_str())
                    && matches!(
                        tag.record.kind,
                        EventKind::ProtectedReapprovalApproved
                            | EventKind::ProtectedReapprovalFeedback
                    )
                    && !logged.contains(&tag.record.record_sha256)
            });
            if tag_terminal {
                projected_lane = None;
            }
        }
        match (old_revision_lane.as_deref(), projected_lane.as_deref()) {
            (None, Some(lane)) => {
                crate::engine::recover_trust_revision_open(&mut self.state)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                self.state.trust_base.active_revision_lane_id = Some(lane.to_owned());
            }
            (Some(checkpoint_lane), None) => {
                // The terminal lookup consults the log first, then the tag
                // history (lookup only — never re-appended).
                let log_terminal = revision_terminal_kind(&log_records, checkpoint_lane);
                let terminal = log_terminal
                    .or_else(|| {
                        tag_records
                            .iter()
                            .filter(|tag| {
                                tag.record.lane.as_deref() == Some(checkpoint_lane)
                                    && matches!(
                                        tag.record.kind,
                                        EventKind::ProtectedReapprovalApproved
                                            | EventKind::ProtectedReapprovalFeedback
                                    )
                            })
                            .map(|tag| (tag.record.kind, tag.record.record_sha256))
                            .next_back()
                    })
                    .ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "checkpoint revision lane disappeared without a protected terminal"
                                .into(),
                        )
                    })?;
                if terminal.0 == EventKind::ProtectedReapprovalFeedback
                    && self.metadata.repo_path.is_some()
                {
                    return Err(RuntimeError::InvalidRuntimeState(
                        "protected-reapproval feedback preserved the prior trust basis, but the runtime cannot prove that provisional revision worktree bytes were rolled back; restore the pre-revision checkpoint before resuming"
                            .into(),
                    ));
                }
                if terminal.0 == EventKind::ProtectedReapprovalApproved {
                    if log_terminal.is_none() && lane_open_in_log {
                        // The approved terminal survives ONLY in the tag
                        // history while state AND log coherently show the
                        // lane still open — an exact operator rewind to a
                        // pre-decision checkpoint, never a crash (adjacent
                        // crash shapes complete above under the repository
                        // guard; log-truncated-past-the-state shapes take
                        // the tag reconstruction below).  Historical lookup
                        // only (Codex R3-4a.2 / Stage-4 fix, Codex 3): no
                        // approval from a deliberately abandoned timeline
                        // is auto-enacted — the lane stays open and the
                        // protected gate re-presents; abandoning the
                        // recorded decision stays A-6's explicit manual
                        // ref-delete.
                    } else {
                        crate::engine::recover_trust_revision_terminal(&mut self.state)
                            .map_err(RuntimeError::InvalidRuntimeState)?;
                        self.state.trust_base.current_human_approval_event_hash = Some(terminal.1);
                        self.state.trust_base.active_revision_lane_id = None;
                    }
                } else {
                    self.state.trust_base.active_revision_lane_id = None;
                }
            }
            (Some(checkpoint_lane), Some(projected)) if checkpoint_lane != projected => {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "checkpoint revision lane {checkpoint_lane} differs from the recorded open lane {projected}"
                )));
            }
            _ => {}
        }
        for record in &log_records {
            if matches!(
                record.kind,
                EventKind::AdvanceGateApproved | EventKind::ProtectedReapprovalApproved
            ) {
                crate::engine::bind_conditional_ratification(
                    &mut self.state,
                    &record.gate_episode_id,
                    record.record_sha256,
                )
                .map_err(RuntimeError::InvalidRuntimeState)?;
            }
        }
        self.state.trust_base.last_fail_closed_reason = None;
        let seed_support_projection_migrated =
            verify_runtime_trust_seed_projection(&mut self.state, &self.metadata)?;
        self.state
            .validate()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        Ok(completed
            || old_approval != self.state.trust_base.current_human_approval_event_hash
            || old_gate_state != self.state.trust_base.routine_gate_state
            || old_revision_lane != self.state.trust_base.active_revision_lane_id
            || old_conditional_candidates != self.state.trust_base.conditional_candidates
            || seed_support_projection_migrated)
    }

    fn apply_sound_assessment_schema_cutover(&mut self) -> Result<bool, RuntimeError> {
        if self.state.sound_assessment_schema_version >= SOUND_ASSESSMENT_SCHEMA_VERSION {
            return Ok(false);
        }
        if self.state.sound_assessment_cutover_requires_rewind() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "soundness assessment schema cutover requires a rewind: this state predates \
                 sound_assessment_schema_version={} but already contains Sound verifier lane \
                 evidence. Rewind the run to just before any Soundness lanes were dispatched \
                 (no in-flight Sound request, no sound_status / sound_approved_fingerprints, \
                 and no latest/previous Sound lane evidence), then restart.",
                SOUND_ASSESSMENT_SCHEMA_VERSION
            )));
        }
        self.state.sound_assessment_schema_version = SOUND_ASSESSMENT_SCHEMA_VERSION;
        Ok(true)
    }

    /// Recover `coarse_dag_nodes` from the supervisor's git history when the
    /// loaded state is in (or past) ProofFormalization but the field is
    /// empty — typically because of a manual rewind across the
    /// TheoremStating → ProofFormalization phase boundary, or a state file
    /// imported from a system version that didn't track the field.
    ///
    /// `coarse_dag_nodes` is normally captured ONCE at the phase
    /// transition (engine.rs around the
    /// `state.coarse_dag_nodes = state.live.present_nodes.clone()` line)
    /// and never re-derived. If lost, signature-protection in Restructure
    /// mode silently degrades (the legacy fallback in
    /// `runtime_cli_observations.rs` treats every node as coarse — safe but
    /// over-restrictive: helpers added later under Restructure can never
    /// have their signatures revised), and the reviewer prompt + viewer
    /// can't surface which nodes are actually coarse-protected.
    ///
    /// We could heal by snapshotting current `live.present_nodes`, but that
    /// would over-include helpers added during proof-formalization (they
    /// would be incorrectly marked as coarse forever). Instead, recover the
    /// authentic value by walking git log of the configured repo:
    /// checkpoint commits write `.trellis-history/supervisor_state.json`
    /// containing the live state, including `coarse_dag_nodes`. The most
    /// recent commit with a populated value is the authoritative snapshot.
    ///
    /// Fails soft: if `repo_path` is unset, the repo isn't a git repo, no
    /// historical commit had a populated value, or any git invocation
    /// errors, this is a no-op (the field stays empty and the legacy
    /// fallback takes over).
    fn heal_coarse_dag_from_git_if_needed(&mut self) {
        if !self.state.coarse_dag_nodes.is_empty() {
            return;
        }
        if self.state.phase.is_theorem_stating_like() {
            return;
        }
        let Some(repo_path) = self.metadata.repo_path.as_deref() else {
            return;
        };
        if let Some(recovered) = recover_coarse_dag_from_git(repo_path) {
            if !recovered.is_empty() {
                self.state.coarse_dag_nodes = recovered;
            }
        }
    }

    pub fn load_or_initialize(
        paths: RuntimePaths,
        initial_state: ProtocolState,
    ) -> Result<Self, RuntimeError> {
        if paths.state_path.exists() {
            Self::load(paths)
        } else {
            Self::initialize(paths, initial_state)
        }
    }

    pub fn state(&self) -> &ProtocolState {
        &self.state
    }

    pub fn metadata(&self) -> &RuntimeMetadata {
        &self.metadata
    }

    /// Commit the one scheduled closure issuance that belongs to fresh
    /// runtime genesis. State is persisted before the metadata marker is
    /// cleared. If the process stops between those writes, restart observes
    /// `pending=true` plus already-current records and safely completes the
    /// marker clear without re-probing.
    pub fn complete_local_closure_initial_issuance(
        &mut self,
        state: ProtocolState,
    ) -> Result<(), RuntimeError> {
        if !self.metadata.local_closure_initial_issuance_pending {
            return Err(RuntimeError::InvalidRuntimeState(
                "local-closure initial issuance is not scheduled for this runtime".into(),
            ));
        }
        require_local_closure_records(&state, "fresh_runtime_initial_issuance")?;
        state
            .validate()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        state
            .validate_local_closure_root_consistency()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        self.state = state;
        self.persist_state()?;
        self.metadata.local_closure_initial_issuance_pending = false;
        self.persist_metadata()?;
        Ok(())
    }

    /// Run a one-shot post-load state migration. The closure receives a
    /// mutable reference to the loaded `ProtocolState`; it must be
    /// idempotent (running twice is a no-op). When the closure returns
    /// `Ok(true)`, this method persists the mutated state to disk so the
    /// migration is durable across restarts. Returns `Ok(false)` if the
    /// closure reports no mutation. Errors from the closure are surfaced
    /// as `RuntimeError::InvalidRuntimeState`.
    ///
    /// Used by `bin/runtime_cli.rs` to run schema migrations after `load`
    /// but before the kernel begins servicing requests. The closure runs
    /// before the first dispatch, so any in-memory mutations it makes are
    /// visible to all subsequent state queries.
    pub fn try_post_load_state_migration<F>(&mut self, migrate: F) -> Result<bool, RuntimeError>
    where
        F: FnOnce(&mut ProtocolState) -> Result<bool, String>,
    {
        let mutated = migrate(&mut self.state).map_err(RuntimeError::InvalidRuntimeState)?;
        if mutated {
            self.persist_state()?;
        }
        Ok(mutated)
    }

    /// Rebuild a request that the pure engine issued immediately before a
    /// post-step trust state migration.  Trust-v1 conditional candidate
    /// activation is the motivating case: the next request has not left the
    /// runtime yet, but its graph/blocker projection and Worker base
    /// snapshot were captured before the newly activated node existed.
    ///
    /// Recomputing the same request id/kind, reapplying execution hints, and
    /// recapturing `active_worker_base` keeps the persisted request, returned
    /// `IssueRequest`, and authoritative repository bytes atomic at the
    /// dispatch boundary.
    pub fn refresh_in_flight_after_external_state_change(
        &mut self,
    ) -> Result<Option<crate::model::WrapperRequest>, RuntimeError> {
        if self.state.in_flight_request.is_none() {
            self.state
                .validate()
                .map_err(RuntimeError::InvalidRuntimeState)?;
            self.state
                .validate_local_closure_root_consistency()
                .map_err(RuntimeError::InvalidRuntimeState)?;
            self.persist_state()?;
            return Ok(None);
        }
        self.refresh_in_flight_request_from_state();
        // Validate the SEMANTIC projection, BEFORE execution hints are
        // reattached. `expected_request` leaves the dispatch pass's hints
        // (`fresh_context`, lane bindings) deliberately unresolved, so
        // validating the hint-decorated form compares it against a projection
        // it can never equal — `validate()` would reject every refresh that
        // happens while a hint-bearing request is in flight. The reload path
        // avoids the same collision by stripping hints first
        // (`normalize_in_flight_request_execution_hints_for_validation`).
        // Verified live: the first statement binding to ever commit died here
        // with "in-flight request payload does not match derived state" AFTER
        // the binding had been durably applied.
        let mut next_state = self.state.clone();
        self.apply_request_execution_hints_to_state(&mut next_state, true)?;
        // Validate the SEMANTIC projection of the post-hint state. The hints
        // pass also materializes deferred certificate obligations, so the
        // check must run AFTER it — but `expected_request` leaves the dispatch
        // hints (`fresh_context`, lane bindings, contracts) deliberately
        // unresolved, so validating the decorated form compares it against a
        // projection it can never equal and rejects every refresh made while a
        // hint-bearing request is in flight. Strip exactly those fields first,
        // as the reload path does.
        //
        // Verified live: the first prose statement binding to ever commit died
        // here with "in-flight request payload does not match derived state"
        // AFTER the binding had been durably applied.
        let mut validation_view = next_state.clone();
        normalize_in_flight_request_execution_hints(&mut validation_view);
        validation_view
            .validate()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        next_state
            .validate_local_closure_root_consistency()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        if let Some(request) = next_state.in_flight_request.as_deref() {
            self.capture_active_worker_base_for_request(&next_state, request)?;
        }
        self.state = next_state;
        self.persist_state()?;
        Ok(self.state.in_flight_request.as_deref().cloned())
    }

    pub fn paths(&self) -> &RuntimePaths {
        &self.paths
    }

    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    /// Per-cycle event-log directory for this runtime, resolved lazily from
    /// `metadata.repo_path` (fallback `<root>/.event-log-fallback`).
    pub fn event_log_dir(&self) -> PathBuf {
        event_log_dir_for(&self.paths.root, &self.metadata)
    }

    pub fn step<A: WrapperAdapter>(
        &mut self,
        adapter: &mut A,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        let mut sink = NoopCheckpointSink;
        self.step_with_checkpoint_sink(adapter, &mut sink)
    }

    pub fn step_with_checkpoint_sink<A: WrapperAdapter, C: CheckpointSink>(
        &mut self,
        adapter: &mut A,
        checkpoint_sink: &mut C,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        let mut issuer = NoopLocalClosureIssuer;
        self.step_with_checkpoint_sink_and_local_closure_issuer(
            adapter,
            checkpoint_sink,
            &mut issuer,
        )
    }

    pub fn step_with_checkpoint_sink_and_local_closure_issuer<
        A: WrapperAdapter,
        C: CheckpointSink,
        I: LocalClosureIssuer,
    >(
        &mut self,
        adapter: &mut A,
        checkpoint_sink: &mut C,
        local_closure_issuer: &mut I,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        // Q7 package-ready barrier (plan doc 32 Stage 3, Codex 6/R2-7): at
        // the barrier the runtime mechanically assembles/reads the exact
        // archive at the ONE deterministic `package_archive_path`, verifies
        // the embedded approval record against state and the persisted gate
        // hash, and on success proceeds to finalization with the
        // `PackageFinalizationRecord` payload; on failure it stays parked
        // (still `PackageReady`, still loud).
        if self.state.trust_base.required()
            && self.state.trust_base.package_ready
            && self.state.trust_base.package_finalization.is_none()
        {
            match self.prepare_package_finalization() {
                Ok(record) => {
                    let pre_step_state = self.state.clone();
                    let pre_step_metadata = self.metadata.clone();
                    return self.step_committed_event_with_checkpoint_sink(
                        ProtocolEvent::FinalizeAuthorizedPackage { record },
                        checkpoint_sink,
                        pre_step_state,
                        pre_step_metadata,
                        None,
                        None,
                        Some(local_closure_issuer),
                    );
                }
                Err(reason) => {
                    eprintln!("trellis: required-v1 package-ready barrier is parked: {reason}");
                    return Ok(RuntimeStepOutcome {
                        status: RuntimeStepStatus::PackageReady,
                        event: None,
                        commands: vec![],
                    });
                }
            }
        }
        if self.state.phase == Phase::Complete || self.state.stage == crate::model::Stage::Complete
        {
            return Ok(RuntimeStepOutcome {
                status: RuntimeStepStatus::Complete,
                event: None,
                commands: vec![],
            });
        }

        // Snapshot pre-step in-memory state for atomicity rollback (used
        // only on checkpoint_sink failure below — see the comment block
        // at the bottom of this function). Captured before any step
        // mutations so a sink failure restores `self.state` and
        // `self.metadata` to the exact "before this step" snapshot,
        // leaving the persisted state file (which has not been
        // overwritten yet) consistent with the unchanged git repo.
        //
        // metadata is included because `record_native_history` and
        // `maybe_clear_worker_history_for_checker_mismatch` mutate
        // `metadata.native_history_kinds` between pre-step capture
        // and the sink call. Without snapshotting metadata, a rolled-
        // back step would leave history-key mutations in place and
        // a re-step's `request_requires_fresh_context` decision could
        // diverge from a fresh-process startup. event_count is NOT
        // snapshotted because `append_event_log` runs after the sink,
        // so a sink failure leaves event_count unchanged.
        let pre_step_state = self.state.clone();
        let pre_step_metadata = self.metadata.clone();

        // Re-run the coarse-DAG heal at every step boundary. It's a no-op
        // once the field is populated (the early-return on
        // `!coarse_dag_nodes.is_empty()` skips the git scan), but acts as
        // a continuous self-heal: if anything ever clears the field
        // mid-run (a future rewind path, a hand-edited state file,
        // whatever), the next step recovers it from git history without
        // needing a supervisor restart.
        self.heal_coarse_dag_from_git_if_needed();

        self.apply_request_dispatch_hints()?;
        let prior_request = self
            .state
            .in_flight_request
            .as_ref()
            .map(|req| (req.kind, req.phase));
        // burst-history ledger: snapshot the full dispatch-time
        // WrapperRequest so we can pair it with the upcoming response.
        // Cloning is cheap relative to the response wait that follows.
        let burst_history_request_snapshot = self.state.in_flight_request.as_deref().cloned();
        let event = self.next_event(adapter)?;
        self.step_committed_event_with_checkpoint_sink(
            event,
            checkpoint_sink,
            pre_step_state,
            pre_step_metadata,
            prior_request,
            burst_history_request_snapshot,
            Some(local_closure_issuer),
        )
    }

    /// Parallel-closure sidecar: drive an externally-constructed event
    /// (the boundary hook's `SidecarClosure`) through the FULL step
    /// machinery — sink-first durability ordering, rollback on sink
    /// failure, state/metadata persist, event-log append. A mechanical
    /// twin of `step_with_checkpoint_sink` with the adapter-derived
    /// `next_event` replaced by the injected event; both share
    /// `step_committed_event_with_checkpoint_sink`, so the existing
    /// path is byte-identical.
    pub fn step_injected_event_with_checkpoint_sink<C: CheckpointSink>(
        &mut self,
        event: ProtocolEvent,
        checkpoint_sink: &mut C,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        let mut issuer = NoopLocalClosureIssuer;
        self.step_injected_event_with_checkpoint_sink_and_local_closure_issuer(
            event,
            checkpoint_sink,
            &mut issuer,
        )
    }

    pub fn step_injected_event_with_checkpoint_sink_and_local_closure_issuer<
        C: CheckpointSink,
        I: LocalClosureIssuer,
    >(
        &mut self,
        event: ProtocolEvent,
        checkpoint_sink: &mut C,
        local_closure_issuer: &mut I,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        if self.state.phase == Phase::Complete || self.state.stage == crate::model::Stage::Complete
        {
            return Ok(RuntimeStepOutcome {
                status: RuntimeStepStatus::Complete,
                event: None,
                commands: vec![],
            });
        }
        let pre_step_state = self.state.clone();
        let pre_step_metadata = self.metadata.clone();
        self.heal_coarse_dag_from_git_if_needed();
        self.apply_request_dispatch_hints()?;
        self.step_committed_event_with_checkpoint_sink(
            event,
            checkpoint_sink,
            pre_step_state,
            pre_step_metadata,
            None,
            None,
            Some(local_closure_issuer),
        )
    }

    /// Shared tail of `step_with_checkpoint_sink` /
    /// `step_injected_event_with_checkpoint_sink`: applies `event`,
    /// honors commands, runs the sink-first durability barrier, and
    /// persists state + metadata + the event-log line. Extracted
    /// verbatim (mechanical refactor, no behavior change).
    fn step_committed_event_with_checkpoint_sink<C: CheckpointSink>(
        &mut self,
        event: ProtocolEvent,
        checkpoint_sink: &mut C,
        pre_step_state: ProtocolState,
        pre_step_metadata: RuntimeMetadata,
        prior_request: Option<(crate::model::RequestKind, Phase)>,
        burst_history_request_snapshot: Option<crate::model::WrapperRequest>,
        local_closure_issuer: Option<&mut dyn LocalClosureIssuer>,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        let captured_last_invalid = self.capture_last_invalid_snapshot_for_event(&event)?;
        // Fail-loud dual-check seam: a worker response the acceptance
        // checker accepted must never be silently rejected by the engine.
        // Any engine-side rejection of a checker-accepted response is a
        // checker/engine disagreement and halts with a marker.
        let checker_accepted = match &event {
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(response),
            } if response.acceptance_check_passed
                && response.status == ResponseStatus::Ok
                && response.outcome == WorkerOutcome::Valid =>
            {
                Some(response)
            }
            _ => None,
        };
        let outcome = match apply_event_boxed(self.state.clone(), event.clone()) {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(response) = checker_accepted {
                    // Fail-loud seam, engine-transition-ERROR arm. The engine
                    // errored instead of rejecting, so it emitted no commands
                    // and there is no engine-ordered restore to execute — but
                    // the stranded pair is the same as the rejection arm's:
                    // retained pre-step STATE paired with the unaccepted
                    // burst's raw edits on DISK. `active_worker_base` is the
                    // snapshot consistent with the retained state, and the
                    // restore touches only the worker-writable surfaces the
                    // burst wrote (which survive in the retired bridge
                    // artifacts and burst history), so restore to it before
                    // halting. Then retire the bridge's replay residue so an
                    // operator resume re-dispatches the burst instead of
                    // replaying a response that deterministically errors the
                    // engine — or, if the error is later fixed, silently
                    // re-certifying that response against restored pre-delta
                    // bytes. Failures append to the halt reason; they never
                    // mask the disagreement.
                    let mut engine_rejection = format!("engine transition error: {error:?}");
                    if let Some(repo_path) = self.metadata.repo_path.clone() {
                        if let Err(restore_error) =
                            self.restore_repo_worktree_to_active_worker_base(&repo_path)
                        {
                            engine_rejection = format!(
                                "{engine_rejection}; pre-halt worktree restore to active_worker_base also failed: {restore_error}"
                            );
                        }
                    }
                    for diagnostic in
                        self.retire_replayable_worker_response_residue(response.request_id)
                    {
                        engine_rejection = format!("{engine_rejection}; {diagnostic}");
                    }
                    return Err(self.acceptance_disagreement_error(
                        &pre_step_state,
                        response,
                        engine_rejection,
                    ));
                }
                return Err(RuntimeError::Kernel(error));
            }
        };
        if let Some(response) = checker_accepted {
            let restored_rejected_worker = outcome.commands.iter().any(|command| {
                matches!(command, ProtocolCommand::RestoreWorktreeToActiveWorkerBase)
            });
            if restored_rejected_worker {
                let mut reasons = outcome
                    .state
                    .deterministic_worker_rejection_reasons
                    .join("; ");
                if reasons.is_empty() {
                    reasons = "engine emitted RestoreWorktreeToActiveWorkerBase without a deterministic rejection reason".to_string();
                }
                // The halt below short-circuits before the command loop, so
                // the engine-ordered worktree restore would never execute —
                // leaving retained pre-step STATE paired with the rejected
                // burst's raw edits on DISK. That stranded pair is
                // deterministic fallout (the next authoritative apply's
                // verify-only coverage check hard-errors on the divergence),
                // so execute the restore the engine ordered before halting.
                // Restore failure must not mask the disagreement: append it
                // to the marker/diagnostic instead of returning it. The
                if restored_rejected_worker {
                    if let Some(repo_path) = self.metadata.repo_path.clone() {
                        if let Err(restore_error) =
                            self.restore_repo_worktree_to_active_worker_base(&repo_path)
                        {
                            reasons = format!(
                                "{reasons}; pre-halt worktree restore to active_worker_base also failed: {restore_error}"
                            );
                        }
                    }
                    // The restore above invalidated the on-disk premise of
                    // the bridge's replay layers: a content-only delta
                    // passes the presence-only delta guard, so on resume
                    // the cached response would be re-certified against the
                    // restored pre-delta bytes. Retire the residue
                    // (rename-aside, forensics preserved) so resume
                    // re-dispatches the burst instead.
                    for diagnostic in
                        self.retire_replayable_worker_response_residue(response.request_id)
                    {
                        reasons = format!("{reasons}; {diagnostic}");
                    }
                }
                return Err(self.acceptance_disagreement_error(
                    &pre_step_state,
                    response,
                    reasons,
                ));
            }
        }
        self.finish_committed_event_with_checkpoint_sink(
            event,
            checkpoint_sink,
            pre_step_state,
            pre_step_metadata,
            prior_request,
            burst_history_request_snapshot,
            local_closure_issuer,
            captured_last_invalid,
            outcome,
        )
    }

    /// Keep the command/durability transaction in a separate frame from the
    /// pure transition. Some trust transitions recursively project deeply
    /// nested JSON, while this tail necessarily retains rollback snapshots;
    /// the two phases must not consume the test thread's stack concurrently.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn finish_committed_event_with_checkpoint_sink<C: CheckpointSink>(
        &mut self,
        event: ProtocolEvent,
        checkpoint_sink: &mut C,
        pre_step_state: ProtocolState,
        pre_step_metadata: RuntimeMetadata,
        prior_request: Option<(crate::model::RequestKind, Phase)>,
        burst_history_request_snapshot: Option<crate::model::WrapperRequest>,
        mut local_closure_issuer: Option<&mut dyn LocalClosureIssuer>,
        captured_last_invalid: Option<PathBuf>,
        mut outcome: Box<crate::engine::TransitionOutcome>,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        let mut next_state = outcome.state;
        // Step 0 — durable-sink capability gate (Codex R2-4; Stage-3 fix 2):
        // enforced by PRE-SCANNING the outcome commands so the refusal
        // precedes EVERY command side effect. The engine can order
        // repo-mutating commands ahead of the trust command (the
        // formalization-complete protected-approve arm emits the
        // `SyncTabletRootForPaperTargets` umbrella rewrite first), and a
        // refusal that has already rewritten worktree bytes would leave
        // residue for a later checkpoint's `git add -A` to sweep into an
        // unrelated decision. Nothing has been executed or persisted yet,
        // so the gate simply re-presents.
        if next_state.trust_base.required()
            && !checkpoint_sink.provides_durable_decision_tags()
            && outcome
                .commands
                .iter()
                .any(|command| matches!(command, ProtocolCommand::CommitTrustGateDecision { .. }))
        {
            return Err(RuntimeError::CheckpointSink(
                "a required-v1 trust gate decision needs a checkpoint hook that \
                 writes durable supervisor2/trust-decision tags; set \
                 TRELLIS_RUNTIME_CHECKPOINT_HOOK and re-present the gate"
                    .into(),
            ));
        }
        if outcome.commands.iter().any(|command| {
            matches!(
                command,
                ProtocolCommand::ProjectAllPendingAssumptions
                    | ProtocolCommand::RejectAllPendingAssumptions { .. }
            )
        }) && !outcome
            .commands
            .iter()
            .any(|command| matches!(command, ProtocolCommand::CommitCheckpoint))
        {
            return Err(RuntimeError::InvalidRuntimeState(
                "a human assumption-batch decision must cross a checkpoint durability barrier \
                 before its repository projection is written"
                    .into(),
            ));
        }
        // Q1 gate-decision transaction (plan doc 32 Stage 3): the pending
        // decision constructed by the CommitTrustGateDecision arm below —
        // the exact prospective event-log line bytes and the record digest
        // the checkpoint hook commits as the tracked record file + tag.
        let mut pending_trust_decision: Option<TrustDecisionCarrier> = None;
        // Audit L-1 — pending side-effect deletes deferred past the
        // checkpoint durability barrier. Engine emits
        // `ProtocolCommand::DeleteLocalClosureRecord` to drop the
        // persisted JSON for an invalidated record; doing the disk
        // delete inline (before sink commit + persist_state) leaves a
        // window where a sink failure rolls back in-memory state to
        // pre_step_state (which holds the record) but the disk file is
        // already gone. Buffering the deletes and flushing only on
        // success closes that window — failed steps leave both memory
        // and disk consistent.
        let mut pending_local_closure_disk_deletes: Vec<NodeId> = Vec::new();
        // Audit F3: publishing approval/rejection in the registries before
        // the checkpoint and event-log sinks can make a failed/aborted step
        // look human-ratified on disk. Buffer those writes just like local-
        // closure deletion. The legacy approve path's next-gate render reads
        // the projected registries, so it joins the same ordered buffer.
        let mut deferred_assumption_decision_writes = Vec::new();
        // #54: kernel-emitted RestoreWorktree* commands replace the
        // event-shape-driven restore. Each variant maps to its own runtime
        // method; commands are processed in order so any restore happens
        // before subsequent commands (CommitCheckpoint, IssueRequest).
        for command in &outcome.commands {
            match command {
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase => {
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "repo worktree restore required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    // This rollback is intentionally limited to the exact
                    // semantic source surfaces writable by a Worker burst.
                    // Falling back to a repo-wide HEAD reset can erase
                    // accepted, uncheckpointed kernel writes (Dormant flips,
                    // assumption registries, manifests, and similar state).
                    // A missing/corrupt snapshot therefore fails closed.
                    self.restore_repo_worktree_to_active_worker_base(repo_path)?;
                }
                ProtocolCommand::RestoreWorktreeToHead => {
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "repo worktree restore required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    self.restore_repo_worktree_to_head(repo_path)?;
                }
                ProtocolCommand::RestoreWorktreeToLastClean => {
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "repo worktree restore required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    // Process memory (spec §7): the reviewer decision that
                    // triggered this LastClean carries the carry-forward
                    // flag. Non-review triggers keep the spec default
                    // (preserve).
                    let preserve_process_memory = match &event {
                        ProtocolEvent::WrapperResponse {
                            response: WrapperResponse::Review(review),
                        } => review.preserve_process_memory,
                        _ => true,
                    };
                    self.restore_repo_worktree_to_last_clean(repo_path, preserve_process_memory)?;
                }
                ProtocolCommand::RestoreTheoremStatingNodeAndPruneOrphans { node } => {
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "theorem-stating node reset required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    if let Err(err) = self.restore_theorem_stating_node_and_prune_orphans(
                        repo_path,
                        &mut next_state,
                        node,
                    ) {
                        if let Err(rollback_err) = self.restore_repo_worktree_to_head(repo_path) {
                            eprintln!(
                                "trellis: theorem-stating node reset failed ({err}); rollback to HEAD also failed: {rollback_err}"
                            );
                        }
                        return Err(err);
                    }
                }
                ProtocolCommand::DeleteCleanupUnreachableNodePairs { deletion } => {
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "cleanup unreachable-node deletion required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    delete_cleanup_unreachable_node_pairs(repo_path, deletion)?;
                }
                ProtocolCommand::DeleteLocalClosureRecord { node } => {
                    // Audit L-1 (disk durability ordering): defer the
                    // disk delete until AFTER the checkpoint sink
                    // commits + state.json persists. If the sink fails
                    // we restore in-memory state from pre_step_state,
                    // which still holds the record; deleting the disk
                    // file early would leave state.json (or its
                    // rollback) carrying a record whose persisted JSON
                    // is gone, forcing the next migration to re-probe.
                    // Buffering preserves the original semantic ("the
                    // engine wants this record's disk file gone") but
                    // gates it on the durability barrier so a failed
                    // step is fully rolled back.
                    pending_local_closure_disk_deletes.push(node.clone());
                }
                ProtocolCommand::WriteHaltSentinel { reason } => {
                    // Circuit-breaker: write `.trellis-stop-after-checkpoint`
                    // to the supervisor repo so the outer driver halts at
                    // the next checkpoint boundary. We surface the reason
                    // both inside the sentinel and to stderr so an operator
                    // diagnosing the halt doesn't have to scrape logs.
                    // Timestamp uses SystemTime so we don't pull in a new
                    // chrono dependency for a single timestamp.
                    if let Some(repo_path) = self.metadata.repo_path.as_deref() {
                        let stop_file = repo_path.join(".trellis-stop-after-checkpoint");
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs().to_string())
                            .unwrap_or_else(|_| "unknown".to_string());
                        let payload = format!(
                            "[kernel circuit-breaker] {reason}\n\
                             Written by trellis_runtime_cli at unix_ts={ts}.\n",
                        );
                        if let Err(err) = std::fs::write(&stop_file, &payload) {
                            eprintln!(
                                "trellis: failed to write halt sentinel at {}: {err}",
                                stop_file.display()
                            );
                        } else {
                            eprintln!(
                                "trellis: circuit-breaker halt sentinel written to {}; supervisor will exit at next checkpoint boundary.",
                                stop_file.display()
                            );
                        }
                    } else {
                        eprintln!(
                            "trellis: circuit-breaker tripped ({reason}) but runtime metadata is missing repo_path; cannot write halt sentinel."
                        );
                    }
                }
                ProtocolCommand::SyncTabletRootForPaperTargets { node_names } => {
                    // Paper-target umbrella sync at PF→Cleanup (2026-05-29):
                    // rewrite `<repo>/Tablet.lean` to import the resolved
                    // covering-node set ∪ {Preamble}. The legacy
                    // `sync_tablet_root_from_repo` API is retained for
                    // setup_repo.sh + TheoremStating-reset hot paths;
                    // this command honors the supervisor's PF→Cleanup
                    // boundary specifically.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "paper-target tablet root sync required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::tablet_root::sync_tablet_root(repo_path, node_names)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::FlipDormantDecideFiles {
                    newly_live,
                    newly_dormant,
                } => {
                    // PV dormant store: the engine flipped a `Decide` pair's
                    // polarity. Move the now-live side `Dormant/ → Tablet/` and
                    // the now-dormant side `Tablet/ → Dormant/` so the supervisor's
                    // next observation re-derives `present_nodes` from `Tablet/`
                    // with the new live node present and the old one absent
                    // (the `present_nodes = scan(Tablet/)` chokepoint propagates
                    // the change to every lane). Atomic + crash-safe: see
                    // `dormant_store::flip_decide_pair_on_disk`.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "dormant decide flip required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::dormant_store::flip_decide_pair_on_disk(
                        repo_path,
                        newly_live,
                        newly_dormant,
                    )
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::SeedDormantRefutationFile {
                    primary_node_stem,
                    refutation_node_name,
                    refutation_lean,
                    refutation_informal,
                } => {
                    // Prose audit repair (finding 1): `StatementBound` just
                    // patched the pair's names, so the layout gate below now
                    // demands the twin's `Dormant/` files. Seed them HERE —
                    // inside the same step, before that gate runs on
                    // `next_state` — so the binding event can commit. See the
                    // command's doc for ordering rationale; the seed is
                    // idempotent, so a crash between this write and state
                    // persistence re-runs harmlessly.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "dormant refutation seed required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::dormant_store::seed_dormant_refutation_file(
                        repo_path,
                        primary_node_stem,
                        refutation_node_name,
                        refutation_lean,
                        refutation_informal,
                    )
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::RefreshDormantRefutationFile {
                    primary_node_stem,
                    refutation_node_name,
                    refutation_lean,
                    refutation_informal,
                } => {
                    // The accepted primary kept its prescribed statement but
                    // moved to a new effective Lean preamble. Replace the
                    // dormant twin (including any old proof body) so a later
                    // promotion must be proved under exactly that context.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "dormant refutation refresh required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::dormant_store::refresh_dormant_refutation_file(
                        repo_path,
                        primary_node_stem,
                        refutation_node_name,
                        refutation_lean,
                        refutation_informal,
                    )
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::RecordProposedAssumption { record } => {
                    // PV under-model (Slice 2): the assumptions lane passed a
                    // worker-authored `C` — record it `status:"pending"` in
                    // `PROPOSED_ASSUMPTIONS.json`. The Lean/NL blocks already
                    // live in `Tablet/Assumptions.{lean,tex}` and have passed
                    // ordinary NodeCorr. While pending it is NOT in the
                    // permanent `APPROVED_AXIOMS.json` sink; the closure gate
                    // admits it only through the explicitly provisional
                    // pending allowlist, which rejection revokes.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "record proposed assumption required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::assumptions_registry::record_proposed(repo_path, record.clone())
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::RejectStagedAssumption {
                    assumption_id,
                    reason: _,
                } => {
                    // PV under-model (Slice 2): the assumptions lane rejected a
                    // staged candidate before it became a pending proposal.
                    // Remove its marked Lean/NL blocks from
                    // `Tablet/Assumptions.{lean,tex}`.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "reject staged assumption required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::assumptions_registry::remove_staged_assumption_blocks(
                        repo_path,
                        assumption_id,
                    )
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::ProjectAllPendingAssumptions => {
                    // PV under-model (Slice 2): after the operator ratifies the
                    // batch at legacy AssumptionReview or in required-v1's
                    // sole Advance decision, project every pending `C` into
                    // the permanent trust sinks (APPROVED_AXIOMS.json global
                    // and tcb_manifest.json disclosure), then clear it from
                    // the proposed file. The staged Lean/NL blocks remain in
                    // Tablet/Assumptions.{lean,tex}.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "project approved assumptions required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    deferred_assumption_decision_writes.push(
                        DeferredAssumptionDecisionWrite::ProjectAll {
                            repo_path: repo_path.to_path_buf(),
                        },
                    );
                }
                ProtocolCommand::RejectAllPendingAssumptions { reason } => {
                    // PV under-model (Slice 2): the operator declined the batch —
                    // mark every pending `C` rejected and remove its staged
                    // blocks (the parked targets route via ordinary proving on
                    // the reviewer's next turn).
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "reject pending assumptions required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    deferred_assumption_decision_writes.push(
                        DeferredAssumptionDecisionWrite::RejectAll {
                            repo_path: repo_path.to_path_buf(),
                            reason: reason.clone(),
                        },
                    );
                }
                ProtocolCommand::RenderAssumptionsReview => {
                    // PV under-model (Slice 2): (re)render `ASSUMPTIONS_REVIEW.md`
                    // from the pending entries so the operator sees the verbatim
                    // batch at the AssumptionReview gate.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "render assumptions review required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::assumptions_registry::render_review(repo_path)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::RenderTrustAdvanceGatePresentation => {
                    // Materialize the LIVE meaning instrument at the exact
                    // gate-arm transition. Required-v1 uses its sole Advance
                    // gate; legacy PV additionally uses the same presentation
                    // for Advance and the batch AssumptionReview gate.
                    if next_state.stage != crate::model::Stage::HumanGate
                        || !matches!(
                            next_state.gate_kind,
                            GateKind::Advance | GateKind::AssumptionReview
                        )
                        || !next_state.is_pv()
                    {
                        return Err(RuntimeError::InvalidRuntimeState(
                            "advance-gate presentation command emitted outside a PV \
                             HumanGate/Advance-or-AssumptionReview arm"
                                .into(),
                        ));
                    }
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "advance-gate presentation requires an attached campaign repository"
                                .into(),
                        )
                    })?;
                    if deferred_assumption_decision_writes.iter().any(|write| {
                        matches!(
                            write,
                            DeferredAssumptionDecisionWrite::ProjectAll { .. }
                                | DeferredAssumptionDecisionWrite::RejectAll { .. }
                        )
                    }) {
                        deferred_assumption_decision_writes.push(
                            DeferredAssumptionDecisionWrite::RenderAdvanceGate {
                                repo_path: repo_path.to_path_buf(),
                            },
                        );
                    } else {
                        let bytes =
                            trust_advance_gate_presentation_bytes_for_repo(&next_state, repo_path)?;
                        let path = trust_advance_gate_presentation_path(&self.paths.root);
                        fs::write(&path, bytes).map_err(|error| {
                            RuntimeError::InvalidRuntimeState(format!(
                                "failed to write live trust advance-gate presentation {}: {error}",
                                path.display()
                            ))
                        })?;
                    }
                }
                ProtocolCommand::ApplyProcessMemoryOperations { ops } => {
                    // Process memory (spec §6): materialize the accepted
                    // audit's entry files + regenerate INDEX.md. Runs
                    // BEFORE the durability barrier below, so the
                    // mutations land in the same checkpoint commit as the
                    // state they were derived from (the checkpoint hook's
                    // `git add -A` picks up the tracked directory).
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "process-memory apply required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::process_memory::apply_file_ops(repo_path, ops)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::CommitTrustGateDecision {
                    gate_episode_id,
                    choice,
                    kind,
                    lane,
                } => {
                    // The durable-sink capability gate already ran as the
                    // step-0 pre-scan above (Stage-3 fix 2) — before any
                    // command side effect, not merely before this arm.
                    pending_trust_decision = Some(self.prepare_trust_gate_decision(
                        &mut next_state,
                        gate_episode_id,
                        *choice,
                        *kind,
                        lane.as_deref(),
                        &event,
                        &outcome.commands,
                    )?);
                }
                ProtocolCommand::IssueRequest { .. }
                | ProtocolCommand::InstallScheduledLocalClosureRecords { .. }
                | ProtocolCommand::CommitCheckpoint => {}
            }
        }
        self.maybe_clear_worker_history_for_checker_mismatch(&event);
        self.apply_request_execution_hints_to_state(&mut next_state, true)?;
        let records_before_operational_issuance = next_state.local_closure_records.clone();
        if let (Some(issuer), Some(repo_path)) = (
            local_closure_issuer.as_deref_mut(),
            self.metadata.repo_path.as_deref(),
        ) {
            issuer.reconcile_authoritative_source_apply(
                &pre_step_state,
                &mut next_state,
                repo_path,
                &event,
                &outcome.commands,
            )?;
            // The pure engine may have issued the next request before this
            // operational boundary installed a newly minted record. Issuance
            // can remove an owner from the unverified frontier (or otherwise
            // change closure-derived request fields), so the not-yet-dispatched
            // request must be projected from the post-issuance state. Keep the
            // persisted request and the event-carried IssueRequest command
            // identical; neither is allowed to describe the pre-certificate
            // state.
            if let Some(previous) = next_state.in_flight_request.as_deref().cloned() {
                next_state.in_flight_request = Some(Box::new(
                    next_state.expected_request(previous.id, previous.kind),
                ));
                self.apply_request_execution_hints_to_state(&mut next_state, true)?;
                let refreshed = next_state
                    .in_flight_request
                    .as_deref()
                    .expect("request was rebuilt above")
                    .clone();
                for command in &mut outcome.commands {
                    if let ProtocolCommand::IssueRequest { request } = command {
                        *request = Box::new(refreshed.clone());
                    }
                }
            }
        }
        let replay_snapshot_pending =
            local_closure_replay_snapshot_pending_path(&self.paths.root).is_file();
        let replay_records: BTreeMap<crate::model::NodeId, crate::model::LocalClosureRecord> =
            if self.event_count == 0 || replay_snapshot_pending {
                // Genesis issuance happens before StartCycle and therefore has
                // no protocol event of its own. The first durable line carries
                // the complete set so config-seeded replay starts covered.
                next_state.local_closure_records.clone()
            } else {
                next_state
                    .local_closure_records
                    .iter()
                    .filter(|(node, record)| {
                        records_before_operational_issuance.get(*node) != Some(*record)
                    })
                    .map(|(node, record)| (node.clone(), record.clone()))
                    .collect()
            };
        if !replay_records.is_empty() {
            outcome
                .commands
                .push(ProtocolCommand::InstallScheduledLocalClosureRecords {
                    records: replay_records,
                });
            // Decision lines are constructed early because the checkpoint
            // sink must commit their exact bytes. Scheduled issuance runs
            // after command-side source I/O, so refresh only the command
            // carrier in that prospective line; the sealed TrustRecord and
            // its digest are unchanged.
            if let Some(pending) = pending_trust_decision.as_mut() {
                let mut line: EventLogRecord = serde_json::from_str(&pending.line_json)?;
                line.commands = outcome.commands.clone();
                pending.line_json = serde_json::to_string(&line)?;
            }
        }
        if let Some(repo_path) = self.metadata.repo_path.as_deref() {
            crate::dormant_store::validate_configured_decide_layout(repo_path, &next_state)
                .map_err(RuntimeError::InvalidRuntimeState)?;
        }
        // Checker-backed issuance above closes the source/record transaction.
        // The pure engine state is allowed to be transiently inconsistent
        // while a provider has its new root and its consumers still pin the
        // old one; no durable boundary or Worker base may observe that state.
        next_state
            .validate_local_closure_root_consistency()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        if let Some(request) = next_state.in_flight_request.as_deref() {
            self.capture_active_worker_base_for_request(&next_state, request)?;
        }
        self.state = next_state;
        self.update_last_invalid_for_event(&event, captured_last_invalid.as_deref())?;
        if matches!(event, ProtocolEvent::WrapperResponse { .. }) {
            if let Some((kind, phase)) = prior_request {
                if self.should_record_native_history_for_event(&event, kind) {
                    self.record_native_history(kind, phase);
                }
            }
            // Burst-history ledger append is deferred until after the
            // checkpoint sink and the durable persist_state /
            // append_event_log calls below — see the post-persistence
            // hook for the actual append. Rationale: if the checkpoint
            // sink fails (lines below), in-memory state rolls back to
            // pre_step_state, and we don't want burst-history.jsonl to
            // carry a row for a response the runtime didn't durably
            // commit. The persist_state / append_event_log calls below
            // are the durability barrier; the append happens after.
        }
        // Construct the exact ordinary event-log record before crossing a
        // checkpoint boundary.  If the hook commits Git and the process dies
        // before runtime state/event persistence, the recovery journal must
        // carry the same timestamped record that normal completion would
        // append.  Decision-bearing steps already have their stronger,
        // byte-exact TrustDecisionCarrier transaction.
        let prepared_event_record = if pending_trust_decision.is_none() {
            Some(self.prepare_event_log_record(&event, &outcome.commands, None)?)
        } else {
            None
        };
        // Atomicity (audit): for steps that emit CommitCheckpoint, the
        // engine has already called `commit_live()` which mutated
        // `state.committed_*`, `state.last_clean_*`, `has_ever_been_clean`,
        // and `last_clean_verifier_mirror_ready`. Persisting state BEFORE
        // running the checkpoint sink (which performs the git commit + tag
        // creation) leaves a hazard: if the sink fails, the on-disk state
        // file claims a checkpoint exists but git has no corresponding
        // commit/tag. On the next load:
        //   - LastCommit's `git reset --hard HEAD` lands on the OLD commit
        //     (the new one was supposed to be created by the failed sink).
        //   - LastClean's `git reset --hard supervisor2/clean-N` picks the
        //     PREVIOUS clean tag; the `last_clean_*` mirrors point at a
        //     state that doesn't match the tag.
        // Fix: run the sink FIRST. On success → persist state/event log
        // (everything consistent). On failure → restore in-memory state
        // from the pre-step clone and propagate the error; state file
        // remains at the prior generation, so next startup loads a state
        // consistent with the unchanged git.
        let has_checkpoint = outcome
            .commands
            .iter()
            .any(|command| matches!(command, ProtocolCommand::CommitCheckpoint));
        let mut checkpoint_journal_written = false;
        if has_checkpoint {
            // persist_checkpoint writes a derived/cache file
            // (paths.checkpoint_path) that the runtime never reads back on
            // load — no rollback needed. Sink failure is the failure to
            // worry about.
            let checkpoint = match self.persist_checkpoint() {
                Ok(c) => c,
                Err(e) => {
                    self.state = pre_step_state;
                    self.metadata = pre_step_metadata;
                    return Err(e);
                }
            };
            let payload = self.checkpoint_hook_payload(
                checkpoint,
                &outcome.commands,
                pending_trust_decision.clone(),
            );
            let is_clean_checkpoint = payload.is_clean;
            if pending_trust_decision.is_none() && payload.metadata.repo_path.is_some() {
                self.persist_checkpoint_transaction_journal(
                    &payload,
                    prepared_event_record.as_ref().expect(
                        "ordinary checkpoint has a preconstructed event-log record",
                    ),
                )?;
                checkpoint_journal_written = true;
            }
            if let Err(sink_err) = checkpoint_sink.commit(&payload) {
                // Sink failure on a decision-bearing step is INDETERMINATE
                // (item 4 step 3): the decision tag is the hook's LAST
                // fallible operation, so probe for it before deciding.
                if let Some(pending) = pending_trust_decision.as_ref() {
                    match self.probe_trust_decision_tag(pending) {
                        Ok(TrustDecisionTagProbe::PresentSameDigest) => {
                            // The decision is durably committed — proceed as
                            // committed (deterministic completion; the gate
                            // is never re-solicited).
                            eprintln!(
                                "trellis: checkpoint sink failed after the trust decision tag \
                                 was durably written ({sink_err}); completing the committed \
                                 decision without re-presenting the gate"
                            );
                        }
                        Ok(TrustDecisionTagProbe::Absent) => {
                            self.state = pre_step_state;
                            self.metadata = pre_step_metadata;
                            return Err(RuntimeError::CheckpointSink(sink_err));
                        }
                        Ok(TrustDecisionTagProbe::PresentDifferentContent) => {
                            return Err(RuntimeError::InvalidRuntimeState(format!(
                                "trust decision tag exists with different record content after a \
                                 sink failure ({sink_err}); refusing to proceed"
                            )));
                        }
                        Err(probe_err) => {
                            return Err(RuntimeError::InvalidRuntimeState(format!(
                                "cannot determine trust-decision durability after a sink failure \
                                 ({sink_err}): {probe_err}"
                            )));
                        }
                    }
                } else {
                    self.state = pre_step_state;
                    self.metadata = pre_step_metadata;
                    return Err(RuntimeError::CheckpointSink(sink_err));
                }
            }
            // Bug 2: record the durable commit pointer for the LastClean
            // rewind target. The checkpoint hook (sink) has now committed
            // the clean checkpoint and written its `supervisor2/clean-*`
            // tag, so HEAD is the exact commit the just-snapshotted
            // `last_clean_*` mirrors correspond to. Persisting the SHA in
            // state lets `restore_repo_worktree_to_last_clean` rewind by
            // commit pointer instead of the non-monotonic lexical-max tag.
            // Best-effort: a rev-parse failure leaves the pointer at its
            // prior value and the rewind falls back to ancestor-of-HEAD
            // tag selection. Set BEFORE persist_state so it lands durably.
            if is_clean_checkpoint {
                if let Some(repo_path) = self.metadata.repo_path.as_deref() {
                    if let Some(sha) = git_head_sha(repo_path) {
                        self.state.last_clean_commit = Some(sha);
                    }
                }
            }
        }
        self.persist_state()?;
        self.persist_metadata()?;
        if let Some(record) = prepared_event_record.as_ref() {
            self.append_prepared_event_log_record(record)?;
        } else {
            self.append_event_log(
                &event,
                &outcome.commands,
                pending_trust_decision.as_ref(),
                None,
            )?;
        }
        if checkpoint_journal_written {
            self.clear_checkpoint_transaction_journal()?;
        }
        // Audit F3 — only now can the repositories say "human-ratified" or
        // "human-rejected": the checkpoint sink has succeeded, the matching
        // protocol state is persisted, and the exact command-bearing event is
        // durable. Each operation is idempotent so a fail-loud partial sink
        // write can be completed without changing the recorded decision.
        for write in deferred_assumption_decision_writes {
            match write {
                DeferredAssumptionDecisionWrite::ProjectAll { repo_path } => {
                    crate::assumptions_registry::project_all_pending(&repo_path)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                DeferredAssumptionDecisionWrite::RejectAll { repo_path, reason } => {
                    crate::assumptions_registry::reject_all_pending(&repo_path, &reason)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                DeferredAssumptionDecisionWrite::RenderAdvanceGate { repo_path } => {
                    let bytes =
                        trust_advance_gate_presentation_bytes_for_repo(&self.state, &repo_path)?;
                    let path = trust_advance_gate_presentation_path(&self.paths.root);
                    fs::write(&path, bytes).map_err(|error| {
                        RuntimeError::InvalidRuntimeState(format!(
                            "failed to write live trust advance-gate presentation {}: {error}",
                            path.display()
                        ))
                    })?;
                }
            }
        }
        if replay_snapshot_pending {
            let marker = local_closure_replay_snapshot_pending_path(&self.paths.root);
            match fs::remove_file(&marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => eprintln!(
                    "trellis: event log now carries the offline local-closure migration snapshot, but failed to clear {}: {error}; the next event will safely carry it again",
                    marker.display()
                ),
            }
        }
        // Audit L-1 — flush deferred local-closure record disk deletes
        // now that the state file durably reflects the in-memory
        // tombstones. Earlier in the step the engine emitted
        // `ProtocolCommand::DeleteLocalClosureRecord` for each
        // invalidated record; the inline buffer ensures we never delete
        // a JSON file whose corresponding record is still referenced by
        // the previous-generation state.json. Idempotent: re-running a
        // delete on an absent file is a no-op (handled by
        // `delete_persisted_local_closure_record`).
        for node in &pending_local_closure_disk_deletes {
            delete_persisted_local_closure_record(&self.paths.root, node);
        }
        // Burst-history ledger append (deferred to here so the ledger
        // never gets a row for a response the runtime didn't durably
        // commit). At this point: checkpoint sink (if any) succeeded,
        // persist_state succeeded, append_event_log succeeded. Any
        // failure above this point either rolled back in-memory state
        // (checkpoint branch) or propagated an error before reaching
        // here. Best-effort: errors inside `append` are swallowed so
        // a telemetry I/O hiccup never masks a successful step.
        if matches!(event, ProtocolEvent::WrapperResponse { .. }) {
            if let (Some(repo_path), Some(request), ProtocolEvent::WrapperResponse { response }) = (
                self.metadata.repo_path.as_deref(),
                burst_history_request_snapshot.as_ref(),
                &event,
            ) {
                crate::burst_history::append(repo_path, request, response);
            }
        }
        // A reviewer response that consumed the outstanding human input
        // (`clear_human_input = true` on a response the engine ACCEPTED:
        // the flag held on the PRE-step state and is false on the
        // post-transition state) retires the operator prose in
        // `<repo>/HUMAN_INPUT.md`. Left in place, the retracted text
        // lingers in the repo root indefinitely and misleads later
        // readers. Archive it cycle-stamped under
        // `.trellis-history/human-input/` and truncate the repo-root file.
        //
        // Placement — AFTER the durability barrier (mirroring the
        // burst-history append above), NOT before the checkpoint sink: if
        // the sink fails, in-memory state rolls back to
        // `human_input_outstanding = true` and the re-issued reviewer
        // prompt names HUMAN_INPUT.md as the single source of truth — a
        // pre-barrier truncate would leave that prompt pointing at an
        // empty file. Mutating only after the sink + persists succeed
        // means the truncation can never outrun the state that consumed
        // the input; the repo mutation lands in the NEXT checkpoint
        // commit, exactly like the burst-history ledger row. (Chosen over
        // restore-on-rollback because a restore is a second best-effort
        // mutation that can itself fail, recreating the very divergence
        // it exists to repair.)
        //
        // The PRE-step flag gate matters: `clear_human_input` on a
        // response whose pre-step state had `human_input_outstanding =
        // false` — e.g. a legality-REJECTED review, which returns Ok with
        // a re-issued request — is not a clear transition and must leave
        // the file alone. Best-effort: I/O failures inside the helper are
        // logged, never propagated — this is hygiene, not state.
        if let ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Review(review),
        } = &event
        {
            if review.clear_human_input
                && pre_step_state.human_input_outstanding
                && !self.state.human_input_outstanding
            {
                if let Some(repo_path) = self.metadata.repo_path.as_deref() {
                    archive_and_truncate_human_input(repo_path, self.state.cycle);
                }
            }
        }
        Ok(RuntimeStepOutcome {
            status: RuntimeStepStatus::Transitioned,
            event: Some(event),
            commands: outcome.commands,
        })
    }

    fn next_event<A: WrapperAdapter>(
        &self,
        adapter: &mut A,
    ) -> Result<ProtocolEvent, RuntimeError> {
        if self.state.stage == crate::model::Stage::Start && self.state.in_flight_request.is_none()
        {
            return Ok(ProtocolEvent::StartCycle);
        }
        let Some(request) = self.state.in_flight_request.as_ref() else {
            return Err(RuntimeError::InvalidRuntimeState(
                "no in-flight request available for current stage".into(),
            ));
        };
        let response = adapter.dispatch(request).map_err(RuntimeError::Adapter)?;
        Ok(ProtocolEvent::WrapperResponse { response })
    }

    fn persist_state(&self) -> Result<(), RuntimeError> {
        fs::create_dir_all(&self.paths.root)?;
        let data = serde_json::to_vec_pretty(&self.state)?;
        atomically_replace_file(&self.paths.state_path, &data)?;
        Ok(())
    }

    fn persist_metadata(&self) -> Result<(), RuntimeError> {
        fs::create_dir_all(&self.paths.root)?;
        let data = serde_json::to_vec_pretty(&self.metadata)?;
        atomically_replace_file(&self.paths.metadata_path, &data)?;
        Ok(())
    }

    fn checkpoint_transaction_journal_path(&self) -> PathBuf {
        self.paths
            .root
            .join(CHECKPOINT_TRANSACTION_JOURNAL_FILENAME)
    }

    fn persist_checkpoint_transaction_journal(
        &self,
        payload: &CheckpointHookPayload,
        event_record: &EventLogRecord,
    ) -> Result<(), RuntimeError> {
        let repo_path = payload.metadata.repo_path.clone().ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "checkpoint recovery journal requires metadata.repo_path".into(),
            )
        })?;
        let transaction = PendingCheckpointTransaction {
            schema_version: CHECKPOINT_TRANSACTION_JOURNAL_SCHEMA_VERSION,
            pre_commit_head: git_head_sha(&repo_path),
            repo_path,
            post_state_sha256: serialized_sha256(&payload.state)?,
            metadata_sha256: serialized_sha256(&payload.metadata)?,
            checkpoint_sha256: serialized_sha256(&payload.checkpoint)?,
            event_record: event_record.clone(),
            is_clean: payload.is_clean,
        };
        fs::create_dir_all(&self.paths.root)?;
        let bytes = serde_json::to_vec_pretty(&transaction)?;
        atomically_replace_file(&self.checkpoint_transaction_journal_path(), &bytes)?;
        Ok(())
    }

    fn clear_checkpoint_transaction_journal(&self) -> Result<(), RuntimeError> {
        match fs::remove_file(self.checkpoint_transaction_journal_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RuntimeError::Io(error)),
        }
    }

    fn persist_checkpoint(&self) -> Result<RuntimeCheckpoint, RuntimeError> {
        self.state
            .validate_local_closure_root_consistency()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        let checkpoint = RuntimeCheckpoint {
            cycle: self.state.cycle,
            phase: self.state.phase,
            gate_kind: self.state.gate_kind,
            active_node: self.state.active_node.clone(),
            committed: self.state.committed.clone(),
            cleanup_unreachable_deletion: self
                .state
                .cleanup_unreachable_deletion_log
                .last()
                .filter(|record| record.cycle == self.state.cycle)
                .cloned(),
            // Q1 (Codex 7): always `null` — the binding died with the
            // journal; the key survives for byte-identical math checkpoints.
        };
        let data = serde_json::to_vec_pretty(&checkpoint)?;
        atomically_replace_file(&self.paths.checkpoint_path, &data)?;
        Ok(checkpoint)
    }

    /// The runtime half of the Q1 gate-decision transaction (item 4 step 2):
    /// read the presentation bytes, hash and persist them into the runtime
    /// directory, construct and seal the `TrustRecord` (intended index =
    /// current event count), install the approve-arm approval digest (A-4),
    /// finalize the routine gate state, and build the exact prospective
    /// event-log line the checkpoint hook commits as the tracked record
    /// file and `append_event_log` later appends byte-identically.
    #[allow(clippy::too_many_arguments)]
    fn prepare_trust_gate_decision(
        &self,
        next_state: &mut ProtocolState,
        gate_episode_id: &str,
        choice: HumanChoice,
        kind: EventKind,
        lane: Option<&str>,
        event: &ProtocolEvent,
        commands: &[ProtocolCommand],
    ) -> Result<TrustDecisionCarrier, RuntimeError> {
        if !next_state.trust_base.required() {
            return Err(RuntimeError::InvalidRuntimeState(
                "trust gate commit command emitted outside a required-v1 run".into(),
            ));
        }
        let advance_kind = match kind {
            EventKind::AdvanceGateApproved | EventKind::AdvanceGateFeedback => {
                if !next_state.trust_base.gate_commit_pending {
                    return Err(RuntimeError::InvalidRuntimeState(
                        "trust gate commit command emitted outside a pending required-v1 gate"
                            .into(),
                    ));
                }
                true
            }
            EventKind::AuditAuthorization
            | EventKind::ProtectedReapprovalApproved
            | EventKind::ProtectedReapprovalFeedback => {
                if lane.is_none_or(str::is_empty) {
                    return Err(RuntimeError::InvalidRuntimeState(
                        "an exceptional-lane trust record requires its lane id".into(),
                    ));
                }
                false
            }
            EventKind::SeedCommitted => {
                return Err(RuntimeError::InvalidRuntimeState(
                    "this trust record kind does not ride the gate-decision command".into(),
                ));
            }
        };
        // Stage 7 (A-2/Codex R2-5): the `AuditAuthorization` record is
        // engine-emitted at the flagged-repair authorization, not a human
        // gate submission — its presentation is the canonical bytes of the
        // AUTHORIZING adaptation-ledger row (the lane id is the digest of
        // exactly that row + cycle, so the binding is re-derivable).  Every
        // human-decision kind keeps the exact gate presentation bytes.
        let conditional_gate_candidate = next_state
            .trust_base
            .conditional_candidates
            .values()
            .find(|candidate| {
                candidate.stage == crate::trust_base::ConditionalStage::CorrespondencePass
                    && candidate.disposition.is_none()
                    && candidate.ratification_gate_episode_id.as_deref()
                        == lane.or(Some(gate_episode_id))
            });
        let presentation_bytes = if kind == EventKind::AuditAuthorization
            && conditional_gate_candidate.is_some()
        {
            let packet = crate::trust_base::conditional_ratification_packet(
                conditional_gate_candidate.expect("conditional gate candidate checked above"),
            )
            .map_err(RuntimeError::InvalidRuntimeState)?;
            crate::trust_base::canonical_json(&packet)
                .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?
        } else if kind == EventKind::AuditAuthorization {
            let lane_id = lane.unwrap_or_default();
            let row = next_state
                .trust_base
                .adaptation_ledger
                .iter()
                .find(|row| {
                    crate::model::derive_revision_lane_id(row, next_state.cycle)
                        .as_deref()
                        == Ok(lane_id)
                })
                .ok_or_else(|| {
                    RuntimeError::InvalidRuntimeState(
                        "audit-authorization lane id does not derive from any \
                         adaptation-ledger row at this cycle"
                            .into(),
                    )
                })?;
            crate::trust_base::canonical_json(row)
                .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?
        } else if matches!(
            kind,
            EventKind::ProtectedReapprovalApproved | EventKind::ProtectedReapprovalFeedback
        ) && conditional_gate_candidate.is_some()
        {
            let candidate = conditional_gate_candidate
                .expect("conditional ratification candidate checked above");
            let packet = crate::trust_base::conditional_ratification_packet(candidate)
                .map_err(RuntimeError::InvalidRuntimeState)?;
            crate::trust_base::canonical_json(&packet)
                .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?
        } else if matches!(
            kind,
            EventKind::AdvanceGateApproved | EventKind::AdvanceGateFeedback
        ) {
            // Stage 10 anti-staleness: the durable decision may bind only the
            // exact live projection rendered when this gate armed.  Recompute
            // from the current semantic state BEFORE hashing or installing
            // any approval digest, then byte-compare with what the human read.
            // `trust_advance_gate_presentation_bytes` deliberately excludes
            // terminal bookkeeping (phase/gate state/current approval), so
            // the post-engine/pre-record `next_state` projects the same
            // decision facts as the armed pre-response state.
            let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(
                    "advance-gate presentation recheck requires an attached campaign repository"
                        .into(),
                )
            })?;
            let expected =
                trust_advance_gate_presentation_bytes_for_repo(next_state, repo_path)?;
            let path = trust_advance_gate_presentation_path(&self.paths.root);
            let rendered = fs::read(&path).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "failed to read live trust advance-gate presentation {}: {error}",
                    path.display()
                ))
            })?;
            if rendered != expected {
                atomically_replace_file(&path, &expected).map_err(|error| {
                    RuntimeError::InvalidRuntimeState(format!(
                        "failed to atomically re-render live trust advance-gate presentation \
                         {}: {error}",
                        path.display()
                    ))
                })?;
                return Err(RuntimeError::InvalidRuntimeState(
                    "presentation differs from the current semantic decision state; the \
                     instrument has been re-rendered — re-read before responding"
                        .into(),
                ));
            }
            rendered
        } else {
            let presentation_path = self
                .metadata
                .trust_gate_presentation_path
                .as_deref()
                .ok_or_else(|| {
                    RuntimeError::InvalidRuntimeState(
                        "trust-base v1 requires exact gate presentation bytes".into(),
                    )
                })?;
            fs::read(presentation_path).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "failed to read gate presentation {}: {error}",
                    presentation_path.display()
                ))
            })?
        };
        let presentation_hash = tagged_hash(DomainTag::GatePresentation, &presentation_bytes);
        persist_trust_gate_presentation(&self.paths.root, presentation_hash, &presentation_bytes)?;
        let seed_roots = trust_record_seed_roots(&next_state.trust_base)?;
        let record = TrustRecord {
            kind,
            lane: lane.map(str::to_owned),
            gate_episode_id: gate_episode_id.to_owned(),
            presentation_sha256: presentation_hash,
            seed_roots,
            cycle: next_state.cycle,
            intended_event_log_index: self.event_count,
            record_sha256: crate::trust_base::Sha256Digest::ZERO,
        }
        .seal()
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        // Approve arm ONLY (audit A-4): the record digest becomes the
        // current human approval; feedback terminals carry the digest in
        // the record/tag alone.
        match kind {
            EventKind::AdvanceGateApproved | EventKind::ProtectedReapprovalApproved => {
                next_state.trust_base.current_human_approval_event_hash =
                    Some(record.record_sha256);
                crate::engine::bind_conditional_ratification(
                    next_state,
                    gate_episode_id,
                    record.record_sha256,
                )
                .map_err(RuntimeError::InvalidRuntimeState)?;
            }
            _ => {}
        }
        if advance_kind {
            next_state.trust_base.gate_commit_pending = false;
            next_state.trust_base.routine_gate_state = match choice {
                HumanChoice::Approve => crate::model::TrustRoutineGateState::Approved,
                HumanChoice::Feedback => crate::model::TrustRoutineGateState::FeedbackTerminated,
            };
        }
        next_state.trust_base.last_fail_closed_reason = None;
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut line_record = EventLogRecord {
            index: self.event_count,
            event: event.clone(),
            commands: commands.to_vec(),
            phase: next_state.phase,
            stage: next_state.stage,
            cycle: next_state.cycle,
            ts_ms,
            trust_record: Some(record.clone()),
            additional_trust_records: Vec::new(),
        };
        // Idempotent same-answer retry (Stage-3 fix 3, Codex 3): a prior
        // attempt may have written (and possibly committed) the digest-named
        // record file before failing pre-tag. The record digest covers every
        // decision-relevant field but NOT the envelope `ts_ms`, so a retry
        // that restamped `now` would byte-differ from the surviving file and
        // wedge the never-overwrite hook writer forever. Under exact
        // adjacency — the surviving line is field-identical apart from its
        // timestamp — REUSE the existing prospective line verbatim,
        // including its timestamp, so the retry converges on the same
        // digest/path/bytes. A genuinely different decision line keeps the
        // fresh stamp and still hard-errors downstream.
        if let Some(repo) = self.metadata.repo_path.as_deref() {
            let existing_path = repo
                .join(TRUST_DECISION_RECORD_DIR)
                .join(format!("{}.json", record.record_sha256.to_hex()));
            if let Ok(existing_text) = fs::read_to_string(&existing_path) {
                let existing_line = existing_text.trim_end_matches('\n');
                if let Ok(parsed) = serde_json::from_str::<EventLogRecord>(existing_line) {
                    let existing_ts = parsed.ts_ms;
                    if parsed
                        .trust_record
                        .as_ref()
                        .is_some_and(|prior| prior.record_sha256 == record.record_sha256)
                    {
                        line_record.ts_ms = existing_ts;
                        if serde_json::to_string(&line_record)? == existing_line {
                            return Ok(TrustDecisionCarrier {
                                line_json: existing_line.to_owned(),
                                record_sha256: record.record_sha256,
                            });
                        }
                        line_record.ts_ms = ts_ms;
                    }
                }
            }
        }
        let line_json = serde_json::to_string(&line_record)?;
        Ok(TrustDecisionCarrier {
            line_json,
            record_sha256: record.record_sha256,
        })
    }

    /// Scan the event log for lines carrying a `trust_record`, verifying
    /// each record's self digest before use.
    fn scan_event_log_trust_records(&self) -> Result<Vec<TrustRecord>, RuntimeError> {
        let dir = self.event_log_dir();
        let mut records = Vec::new();
        for file in event_log_cycle_files(&dir)? {
            for line in fs::read_to_string(&file)?.lines() {
                if line.trim().is_empty() || !line.contains("\"trust_record\"") {
                    continue;
                }
                let parsed: EventLogRecord = serde_json::from_str(line).map_err(|error| {
                    RuntimeError::InvalidRuntimeState(format!(
                        "unparseable event-log line in {}: {error}",
                        file.display()
                    ))
                })?;
                let mut line_records = parsed.additional_trust_records;
                if let Some(record) = parsed.trust_record {
                    line_records.insert(0, record);
                }
                for record in line_records {
                    record.verify().map_err(|error| {
                        RuntimeError::InvalidRuntimeState(format!(
                            "event-log trust record failed digest verification: {error}"
                        ))
                    })?;
                    records.push(record);
                }
            }
        }
        Ok(records)
    }

    /// Load every surviving `supervisor2/trust-decision-*` tag's committed
    /// record file via `git show <tag>:<path>` — blob bytes are exact.  Each
    /// file's record is re-hashed against the FULL digest stored in the file
    /// and that digest is prefix-checked against the tag name; a truncation
    /// collision is a loud same-name-different-content hard error, never a
    /// silently accepted record (Claude R3-3).
    fn load_trust_decision_tag_records(&self) -> Result<Vec<TrustDecisionTagRecord>, RuntimeError> {
        let Some(repo) = self.metadata.repo_path.as_deref() else {
            return Ok(Vec::new());
        };
        let Some(stdout) = git_stdout(
            repo,
            &["tag", "-l", &format!("{TRUST_DECISION_TAG_PREFIX}*")],
        )?
        else {
            return Ok(Vec::new());
        };
        let mut records = Vec::new();
        for tag in String::from_utf8_lossy(&stdout).lines() {
            let tag = tag.trim();
            if tag.is_empty() {
                continue;
            }
            let Some(prefix12) = tag.strip_prefix(TRUST_DECISION_TAG_PREFIX) else {
                continue;
            };
            let Some(listing) = git_stdout(
                repo,
                &[
                    "ls-tree",
                    "-r",
                    "--name-only",
                    tag,
                    TRUST_DECISION_RECORD_DIR,
                ],
            )?
            else {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "trust decision tag {tag} exists but its commit tree cannot be read"
                )));
            };
            let listing = String::from_utf8_lossy(&listing).to_string();
            let mut matched = None;
            for path in listing.lines() {
                let name = path.rsplit('/').next().unwrap_or(path);
                if name.starts_with(prefix12) && name.ends_with(".json") {
                    matched = Some(path.trim().to_owned());
                    break;
                }
            }
            let Some(record_path) = matched else {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "trust decision tag {tag} has no committed record file under {TRUST_DECISION_RECORD_DIR}"
                )));
            };
            let Some(file_bytes) = git_stdout(repo, &["show", &format!("{tag}:{record_path}")])?
            else {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "trust decision tag {tag} record file {record_path} cannot be read"
                )));
            };
            let text = String::from_utf8_lossy(&file_bytes).to_string();
            let line_json = text.trim_end_matches('\n');
            let line: EventLogRecord = serde_json::from_str(line_json).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "trust decision record file for {tag} is unparseable: {error}"
                ))
            })?;
            let record = line.trust_record.clone().ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(format!(
                    "trust decision record file for {tag} carries no trust record"
                ))
            })?;
            record.verify().map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "trust decision record for {tag} failed digest verification: {error}"
                ))
            })?;
            if !record.record_sha256.to_hex().starts_with(prefix12) {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "trust decision tag {tag} names a different record digest {} — \
                     same-name-different-content is a hard error",
                    record.record_sha256
                )));
            }
            records.push(TrustDecisionTagRecord {
                tag: tag.to_owned(),
                file_bytes,
                line,
                record,
            });
        }
        Ok(records)
    }

    /// Repository crash adjacency for tag-ahead completion (Stage-4 audit
    /// fix, Codex 3).  Protocol-state bindings cannot distinguish a crash
    /// from an exact operator rewind: the documented rewind procedure
    /// (`git reset --hard <checkpoint-tag>` + restored runtime root)
    /// rewinds state file, event log, episode, and seed roots TOGETHER, so
    /// every state-side binding is reproduced while the never-pruned
    /// decision tag survives.  Completion therefore additionally binds
    /// REPOSITORY state.  The required relationship is pinned from the
    /// hook (`trellis/runtime/git_checkpoint_hook.py`): the record file is
    /// committed BEFORE the checkpoint commit(s), and the lightweight
    /// decision tag is the hook's LAST fallible operation, written on the
    /// then-current `HEAD` — so the tag targets a commit whose tree
    /// CONTAINS the record file at its canonical placement, and after a
    /// genuine crash (no commit can intervene between the hook's return
    /// and the runtime's own persist/append in the same step, nor before
    /// load-time reconcile) the repository `HEAD` still IS that commit.
    /// An operator rewind moves `HEAD` off it by construction (refs
    /// survive `reset --hard`; the tag itself is never checked out as
    /// `HEAD` by the rewind procedure, which resets to checkpoint tags).
    /// Required, fail-closed: (a) current `HEAD` resolves to exactly the
    /// decision tag's target commit, and (b) that commit's tree carries
    /// the record file at `TRUST_DECISION_RECORD_DIR/<full-digest>.json`
    /// with the exact carrier bytes.  Anything else is lookup-only.
    fn trust_decision_tag_is_repo_adjacent(
        &self,
        tag: &TrustDecisionTagRecord,
    ) -> Result<bool, RuntimeError> {
        let Some(repo) = self.metadata.repo_path.as_deref() else {
            return Ok(false);
        };
        let Some(head) = git_stdout(repo, &["rev-parse", "HEAD^{commit}"])? else {
            return Ok(false);
        };
        let Some(target) = git_stdout(repo, &["rev-parse", &format!("{}^{{commit}}", tag.tag)])?
        else {
            return Ok(false);
        };
        if head != target {
            return Ok(false);
        }
        let placement = format!(
            "{}:{}/{}.json",
            tag.tag,
            TRUST_DECISION_RECORD_DIR,
            tag.record.record_sha256.to_hex()
        );
        let Some(bytes) = git_stdout(repo, &["show", &placement])? else {
            return Ok(false);
        };
        Ok(bytes == tag.file_bytes)
    }

    /// Deterministic tag-ahead completion (item 4, Codex R3-4a.2;
    /// kind-generic per Stage-3 fix 4).  A candidate is a decision tag whose
    /// record is absent from the log, whose intended index equals the
    /// CURRENT event count, and whose repository is CRASH-ADJACENT —
    /// current `HEAD` is the tag's target commit with the record file at
    /// its expected placement (`trust_decision_tag_is_repo_adjacent`;
    /// Stage-4 audit fix, Codex 3 — protocol-state bindings all rewind
    /// together, so only the repository distinguishes a crash from an
    /// exact operator rewind).  It completes through exactly one of two
    /// per-kind branches:
    ///   - PRE-STATE bindings (crash after the decision tag, before
    ///     `persist_state`): the loaded state is the record's exact
    ///     pre-step shape — re-apply the recorded transition, then append
    ///     the record file's exact line bytes;
    ///   - POST-STATE bindings (crash after `persist_state`, before the
    ///     event-log append): the record's digest/lane/terminal state is
    ///     already installed — append only, mutating nothing.
    /// All five decision kinds participate.  Any other configuration —
    /// notably an operator rewind reproducing the trigger shape with stale
    /// (or even byte-identical) state bindings — leaves the tag history
    /// lookup-only: nothing is appended or enacted (A-6: abandoning a
    /// recorded decision stays the explicit manual ref-delete).  Returns
    /// whether a completion ran.
    fn maybe_complete_tag_ahead_decision(
        &mut self,
        log_records: &mut Vec<TrustRecord>,
        tag_records: &[TrustDecisionTagRecord],
    ) -> Result<bool, RuntimeError> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum CompletionBranch {
            PreState,
            PostState,
        }
        let logged: std::collections::BTreeSet<crate::trust_base::Sha256Digest> = log_records
            .iter()
            .map(|record| record.record_sha256)
            .collect();
        let state_roots = trust_record_seed_roots(&self.state.trust_base).ok();
        let mut matches: Vec<(&TrustDecisionTagRecord, CompletionBranch)> = Vec::new();
        for tag in tag_records {
            let record = &tag.record;
            if logged.contains(&record.record_sha256)
                || record.intended_event_log_index != self.event_count
            {
                continue;
            }
            // REPOSITORY crash adjacency is required for ANY completion
            // branch (Stage-4 audit fix, Codex 3): an exact operator
            // rewind reproduces every protocol-state binding below, but
            // only a genuine crash leaves `HEAD` on the decision tag's
            // own commit.
            if !self.trust_decision_tag_is_repo_adjacent(tag)? {
                continue;
            }
            // The record's seed roots must bind the loaded state for ANY
            // completion branch.
            if state_roots.as_ref() != Some(&record.seed_roots) {
                continue;
            }
            let state_lane = self.state.trust_base.active_revision_lane_id.as_deref();
            let state_approval = self.state.trust_base.current_human_approval_event_hash;
            let at_advance_gate = self.state.stage == crate::model::Stage::HumanGate
                && self.state.gate_kind == GateKind::Advance
                && self.state.trust_base.advance_gate_episode_id.as_deref()
                    == Some(record.gate_episode_id.as_str());
            let at_protected_gate = self.state.stage == crate::model::Stage::HumanGate
                && self.state.gate_kind == GateKind::ProtectedReapproval
                && self.state.phase == Phase::ProofFormalization
                && record.lane.is_some()
                && state_lane == record.lane.as_deref();
            let branch = match record.kind {
                EventKind::AdvanceGateApproved => {
                    let post = self.state.trust_base.routine_gate_state
                        == crate::model::TrustRoutineGateState::Approved
                        && state_approval == Some(record.record_sha256)
                        && !self.state.trust_base.gate_commit_pending;
                    if at_advance_gate {
                        Some(CompletionBranch::PreState)
                    } else if post {
                        Some(CompletionBranch::PostState)
                    } else {
                        None
                    }
                }
                EventKind::AdvanceGateFeedback => {
                    let post = self.state.trust_base.routine_gate_state
                        == crate::model::TrustRoutineGateState::FeedbackTerminated
                        && state_approval.is_none()
                        && !self.state.trust_base.gate_commit_pending;
                    if at_advance_gate {
                        Some(CompletionBranch::PreState)
                    } else if post {
                        Some(CompletionBranch::PostState)
                    } else {
                        None
                    }
                }
                EventKind::AuditAuthorization => {
                    // Pre-step: the mid-PF authorizing transition has not
                    // opened the lane yet; post-step: the recorded lane is
                    // already the state's open lane.
                    let pre = self.state.phase == Phase::ProofFormalization
                        && state_lane.is_none()
                        && self.state.trust_base.routine_gate_state
                            == crate::model::TrustRoutineGateState::Approved;
                    let post = record.lane.is_some() && state_lane == record.lane.as_deref();
                    if post {
                        Some(CompletionBranch::PostState)
                    } else if pre {
                        Some(CompletionBranch::PreState)
                    } else {
                        None
                    }
                }
                EventKind::ProtectedReapprovalApproved => {
                    let post = state_lane.is_none() && state_approval == Some(record.record_sha256);
                    if at_protected_gate {
                        Some(CompletionBranch::PreState)
                    } else if post {
                        Some(CompletionBranch::PostState)
                    } else {
                        None
                    }
                }
                EventKind::ProtectedReapprovalFeedback => {
                    // Post-step: the feedback arm's exact persisted shape —
                    // lane cleared, back at the PF reviewer boundary with
                    // the routine approval retained.
                    let post = state_lane.is_none()
                        && self.state.phase == Phase::ProofFormalization
                        && self.state.stage == crate::model::Stage::Reviewer
                        && self.state.gate_kind == GateKind::None
                        && self.state.trust_base.routine_gate_state
                            == crate::model::TrustRoutineGateState::Approved;
                    if at_protected_gate {
                        Some(CompletionBranch::PreState)
                    } else if post {
                        Some(CompletionBranch::PostState)
                    } else {
                        None
                    }
                }
                EventKind::SeedCommitted => None,
            };
            if let Some(branch) = branch {
                matches.push((tag, branch));
            }
        }
        let Some((tag, branch)) = matches.first().copied() else {
            return Ok(false);
        };
        if matches.len() > 1 {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "{} unlogged trust-decision tags simultaneously match the crash-adjacency \
                 completion bindings at index {}; refusing to choose between them",
                matches.len(),
                self.event_count
            )));
        }
        if branch == CompletionBranch::PreState {
            // Re-apply the recorded transition onto the exact pre-step state.
            match tag.record.kind {
                EventKind::AdvanceGateApproved => {
                    crate::engine::recover_trust_advance_approval(&mut self.state)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                    self.state.trust_base.routine_gate_state =
                        crate::model::TrustRoutineGateState::Approved;
                    self.state.trust_base.current_human_approval_event_hash =
                        Some(tag.record.record_sha256);
                    self.state.trust_base.gate_commit_pending = false;
                }
                EventKind::AdvanceGateFeedback => {
                    crate::engine::recover_trust_advance_feedback(&mut self.state)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                    self.state.trust_base.routine_gate_state =
                        crate::model::TrustRoutineGateState::FeedbackTerminated;
                    self.state.trust_base.current_human_approval_event_hash = None;
                    self.state.trust_base.gate_commit_pending = false;
                }
                EventKind::AuditAuthorization => {
                    crate::engine::recover_trust_revision_open(&mut self.state)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                    self.state.trust_base.active_revision_lane_id = tag.record.lane.clone();
                }
                EventKind::ProtectedReapprovalApproved => {
                    crate::engine::recover_trust_revision_terminal(&mut self.state)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                    self.state.trust_base.active_revision_lane_id = None;
                    // The protected approval digest becomes the current
                    // human approval (Stage-3 fix 5 semantics).
                    self.state.trust_base.current_human_approval_event_hash =
                        Some(tag.record.record_sha256);
                }
                EventKind::ProtectedReapprovalFeedback => {
                    crate::engine::recover_trust_revision_feedback_terminal(&mut self.state)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                    self.state.trust_base.active_revision_lane_id = None;
                }
                EventKind::SeedCommitted => {
                    unreachable!("non-decision kinds never match a completion branch")
                }
            }
        }
        // Both branches: append the record file's EXACT line bytes at the
        // intended index.
        let dir = self.event_log_dir();
        fs::create_dir_all(&dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(event_log_cycle_file(&dir, tag.line.cycle))?;
        file.write_all(&tag.file_bytes)?;
        if !tag.file_bytes.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        self.event_count += 1;
        eprintln!(
            "trellis: completed the tag-ahead trust decision {} at index {} (crash-adjacency \
             deterministic completion, {} bindings; the gate is not re-solicited)",
            tag.tag,
            tag.record.intended_event_log_index,
            match branch {
                CompletionBranch::PreState => "pre-state",
                CompletionBranch::PostState => "post-state",
            }
        );
        log_records.push(tag.record.clone());
        Ok(true)
    }

    /// Probe git for the pending decision's tag after a sink failure (item
    /// 4 step 3): absent ⇒ the decision did not durably commit; present
    /// with the same committed record content ⇒ proceed as committed;
    /// present with different content ⇒ hard error.
    fn probe_trust_decision_tag(
        &self,
        pending: &TrustDecisionCarrier,
    ) -> Result<TrustDecisionTagProbe, String> {
        let repo = self
            .metadata
            .repo_path
            .as_deref()
            .ok_or_else(|| "no repo_path to probe for the decision tag".to_owned())?;
        let tag = trust_decision_tag_name(pending.record_sha256);
        let listed = git_stdout(repo, &["tag", "-l", &tag]).map_err(|error| error.to_string())?;
        let exists = listed
            .as_deref()
            .map(|stdout| !String::from_utf8_lossy(stdout).trim().is_empty())
            .unwrap_or(false);
        if !exists {
            return Ok(TrustDecisionTagProbe::Absent);
        }
        let path = format!(
            "{TRUST_DECISION_RECORD_DIR}/{}.json",
            pending.record_sha256.to_hex()
        );
        let Some(file_bytes) = git_stdout(repo, &["show", &format!("{tag}:{path}")])
            .map_err(|error| error.to_string())?
        else {
            return Ok(TrustDecisionTagProbe::PresentDifferentContent);
        };
        let committed = String::from_utf8_lossy(&file_bytes);
        if committed.trim_end_matches('\n') == pending.line_json {
            Ok(TrustDecisionTagProbe::PresentSameDigest)
        } else {
            Ok(TrustDecisionTagProbe::PresentDifferentContent)
        }
    }

    /// Locate the current approval's full `TrustRecord`: the event-log scan
    /// first, then the surviving tag history (lookup only — the Q7 archive
    /// embedding never depends on the log line surviving; Codex R2-3).
    fn lookup_approval_trust_record(
        &self,
        approval: crate::trust_base::Sha256Digest,
    ) -> Result<TrustRecord, String> {
        // Both approved terminal kinds qualify (Stage-3 fix 5): the current
        // approval is either the routine AdvanceGateApproved record or — after
        // an approved exceptional revision — the ProtectedReapprovalApproved
        // record that superseded it.
        let is_current_approval = |record: &TrustRecord| {
            matches!(
                record.kind,
                EventKind::AdvanceGateApproved | EventKind::ProtectedReapprovalApproved
            ) && record.record_sha256 == approval
        };
        if let Ok(records) = self.scan_event_log_trust_records() {
            if let Some(record) = records.into_iter().find(is_current_approval) {
                return Ok(record);
            }
        }
        let tags = self
            .load_trust_decision_tag_records()
            .map_err(|error| error.to_string())?;
        tags.into_iter()
            .map(|tag| tag.record)
            .find(is_current_approval)
            .ok_or_else(|| {
                "the current human approval's trust record is absent from both the event log \
                 and the trust-decision tag history"
                    .to_owned()
            })
    }

    /// Q7 barrier (runtime side): assemble the archive at the ONE
    /// deterministic path (temp + atomic rename BEFORE verification),
    /// re-read the renamed bytes, verify the embedded approval record
    /// against state and the persisted gate hash, and construct the
    /// `PackageFinalizationRecord` whose `archive_sha256` attests the
    /// verified re-read bytes.
    fn prepare_package_finalization(
        &self,
    ) -> Result<crate::model::PackageFinalizationRecord, String> {
        let approval = self
            .state
            .trust_base
            .current_human_approval_event_hash
            .ok_or_else(|| "package finalization requires a current human approval".to_owned())?;
        let approval_record = self.lookup_approval_trust_record(approval)?;
        let archive_path = &self.paths.package_archive_path;
        // Stage-3 fix 6 (Codex 6): ALWAYS assemble the current deterministic
        // bytes to a temp file and atomically rename over the barrier path.
        // The supported rewind procedure quarantines runtime subdirectories
        // but not this root-level file, so a stale runtime-generated
        // `trust_package.archive` surviving a rewind must never park a new
        // approval. Assembly is byte-deterministic, so overwriting an
        // already-correct archive is idempotent.
        // Derive the claim twins from LIVE state: the structured rows (the
        // canonical `claim_rows.json` bytes) feed the assembler, which
        // renders the ONE prose surface from them.
        let claim_rows = crate::trust_base::claim_rows_from_state(
            &self.state,
            crate::trust_base::ClaimContext::finalization(
                approval,
                approval_record.presentation_sha256,
            ),
        )
        .map_err(|reason| format!("claim rows derivation failed: {reason}"))?;
        let claim_rows_bytes = crate::trust_base::claim_rows_bytes(&claim_rows)
            .map_err(|reason| format!("claim rows canonicalization failed: {reason}"))?;
        let mut artifact_members = crate::trust_base::artifact_package_members(
            &self.state.trust_base.rust_witness_artifact_records,
            &self.state.trust_base.rust_witness_artifact_payloads,
        )
        .map_err(|error| format!("artifact archive projection failed: {error}"))?;
        if self.state.is_pv() && self.state.trust_base.phase0.is_some() {
            let roots = self.state.trust_base.phase0.as_ref().expect("checked above");
            let repo = self
                .metadata
                .repo_path
                .as_ref()
                .ok_or_else(|| "PV package finalization lacks repo_path".to_owned())?;
            artifact_members.extend(
                crate::phase0::phase0_package_artifacts(repo, roots)
                    .map_err(|error| format!("Phase-0 package projection failed: {error}"))?,
            );
        }
        let bytes = crate::trust_base::assemble_trust_finalization_archive(
            crate::trust_base::TrustFinalizationArchiveRequest {
                approval_record: &approval_record,
                claim_rows_bytes: claim_rows_bytes.clone(),
                artifacts: artifact_members.clone(),
            },
        )
        .map_err(|error| format!("archive assembly failed: {error}"))?;
        atomically_replace_file(archive_path, &bytes)
            .map_err(|error| format!("archive atomic replacement failed: {error}"))?;
        // Verification re-reads the RENAMED bytes — the verified bytes and
        // the delivered file cannot diverge (Codex R2-7).
        let renamed_bytes =
            fs::read(archive_path).map_err(|error| format!("archive re-read failed: {error}"))?;
        let embedded = crate::trust_base::verify_trust_finalization_archive(&renamed_bytes)
            .map_err(|error| format!("archive verification failed: {error}"))?;
        if embedded.approval_record_sha256 != approval {
            return Err(format!(
                "embedded approval record digest {} differs from the current human approval {approval}",
                embedded.approval_record_sha256
            ));
        }
        // Presentation hash == the persisted gate hash (the immutable
        // runtime presentation store written at decision time).
        let presentation_path = self
            .paths
            .root
            .join("presentations")
            .join(format!("{}.bin", embedded.presentation_sha256));
        let presentation_bytes = fs::read(&presentation_path).map_err(|_| {
            format!(
                "embedded presentation hash {} has no persisted gate presentation at {}",
                embedded.presentation_sha256,
                presentation_path.display()
            )
        })?;
        if tagged_hash(DomainTag::GatePresentation, &presentation_bytes)
            != embedded.presentation_sha256
        {
            return Err(
                "persisted gate presentation bytes do not hash to the embedded presentation digest"
                    .to_owned(),
            );
        }
        let state_roots =
            trust_record_seed_roots(&self.state.trust_base).map_err(|error| error.to_string())?;
        if embedded.seed_roots != state_roots {
            return Err(
                "embedded seed roots differ from the state's five seed-root fields".to_owned(),
            );
        }
        // Stage-10 anti-staleness pattern, finalization edition: recompute
        // the claim rows from LIVE state and byte-compare against the
        // verified archive member — the packaged claim can never drift
        // from the semantic decision state it claims to describe.
        let recomputed_rows = crate::trust_base::claim_rows_from_state(
            &self.state,
            crate::trust_base::ClaimContext::finalization(
                embedded.approval_record_sha256,
                embedded.presentation_sha256,
            ),
        )
        .map_err(|reason| format!("claim rows recomputation failed: {reason}"))?;
        let recomputed_bytes = crate::trust_base::claim_rows_bytes(&recomputed_rows)
            .map_err(|reason| format!("claim rows recanonicalization failed: {reason}"))?;
        if recomputed_bytes != embedded.claim_rows_bytes {
            return Err(
                "archived claim rows differ from the current semantic decision state".to_owned(),
            );
        }
        for artifact in artifact_members {
            let archived = crate::trust_base::archive::read_package_member(
                &renamed_bytes,
                &artifact.path,
                u64::try_from(artifact.bytes.len())
                    .map_err(|_| "archive artifact length does not fit u64".to_owned())?,
            )
            .map_err(|error| format!("archive artifact re-read failed: {error}"))?;
            if archived != artifact.bytes {
                return Err(format!(
                    "archived package artifact {} differs from live frozen state",
                    artifact.path
                ));
            }
        }
        Ok(crate::model::PackageFinalizationRecord {
            approval_record: embedded.approval_record,
            approval_record_sha256: embedded.approval_record_sha256,
            presentation_sha256: embedded.presentation_sha256,
            seed_roots: embedded.seed_roots,
            archive_sha256: raw_sha256(&renamed_bytes),
            cycle: self.state.cycle,
        })
    }


    fn append_event_log(
        &mut self,
        event: &ProtocolEvent,
        commands: &[ProtocolCommand],
        pending_trust_decision: Option<&TrustDecisionCarrier>,
        station_trust_record: Option<TrustRecord>,
    ) -> Result<(), RuntimeError> {
        let cycle = self.state.cycle;
        // Defensive: every appended record carries `state.cycle`, and no
        // cycle-0 / pre-cycle events exist (start_cycle increments the cycle
        // before the first append). A cycle==0 here would mean the writer
        // got ahead of the engine's cycle bump — fail loud rather than write
        // a `cycle-000000.jsonl` file that breaks the dense-index invariant.
        if cycle == 0 {
            return Err(RuntimeError::InvalidRuntimeState(
                "refusing to append event log record with cycle==0".into(),
            ));
        }
        // Decision-bearing steps append the EXACT pre-constructed line bytes
        // from the transaction's step 2 — byte-equal to the committed record
        // file (Codex R3-4a.1).
        if let Some(pending) = pending_trust_decision {
            let line: EventLogRecord = serde_json::from_str(&pending.line_json)?;
            if line.index != self.event_count {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "pending trust-decision line targets index {} but the event log is at {}",
                    line.index, self.event_count
                )));
            }
            let dir = self.event_log_dir();
            fs::create_dir_all(&dir)?;
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(event_log_cycle_file(&dir, line.cycle))?;
            file.write_all(pending.line_json.as_bytes())?;
            file.write_all(b"\n")?;
            self.event_count += 1;
            return Ok(());
        }
        let record = self.prepare_event_log_record(event, commands, station_trust_record)?;
        self.append_prepared_event_log_record(&record)
    }

    fn prepare_event_log_record(
        &self,
        event: &ProtocolEvent,
        commands: &[ProtocolCommand],
        trust_record: Option<TrustRecord>,
    ) -> Result<EventLogRecord, RuntimeError> {
        let cycle = self.state.cycle;
        if cycle == 0 {
            return Err(RuntimeError::InvalidRuntimeState(
                "refusing to construct event log record with cycle==0".into(),
            ));
        }
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ok(EventLogRecord {
            index: self.event_count,
            event: event.clone(),
            commands: commands.to_vec(),
            phase: self.state.phase,
            stage: self.state.stage,
            cycle,
            ts_ms,
            trust_record,
            additional_trust_records: Vec::new(),
        })
    }

    fn append_prepared_event_log_record(
        &mut self,
        record: &EventLogRecord,
    ) -> Result<(), RuntimeError> {
        if record.index != self.event_count {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "prepared event-log record targets index {} but the event log is at {}",
                record.index, self.event_count
            )));
        }
        let dir = self.event_log_dir();
        fs::create_dir_all(&dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(event_log_cycle_file(&dir, record.cycle))?;
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        self.event_count += 1;
        Ok(())
    }

    fn checkpoint_hook_payload(
        &self,
        checkpoint: RuntimeCheckpoint,
        commands: &[ProtocolCommand],
        trust_record: Option<TrustDecisionCarrier>,
    ) -> CheckpointHookPayload {
        let is_clean = self.state.clean_checkpoint_ready();
        CheckpointHookPayload {
            root: self.paths.root.clone(),
            state_path: self.paths.state_path.clone(),
            event_log_dir: self.event_log_dir(),
            checkpoint_path: self.paths.checkpoint_path.clone(),
            metadata_path: self.paths.metadata_path.clone(),
            metadata: self.metadata.clone(),
            state: self.state.clone(),
            checkpoint,
            commands: commands.to_vec(),
            event_count: self.event_count,
            is_clean,
            trust_record,
        }
    }

    fn apply_request_dispatch_hints(&mut self) -> Result<(), RuntimeError> {
        let mut next_state = self.state.clone();
        self.apply_request_execution_hints_to_state(&mut next_state, false)?;
        self.state = next_state;
        Ok(())
    }

    #[cfg(test)]
    fn apply_request_execution_hints(&mut self) -> Result<(), RuntimeError> {
        let mut next_state = self.state.clone();
        self.apply_request_execution_hints_to_state(&mut next_state, true)?;
        if let Some(request) = next_state.in_flight_request.as_deref() {
            self.capture_active_worker_base_for_request(&next_state, request)?;
        }
        self.state = next_state;
        Ok(())
    }

    fn apply_request_execution_hints_to_state(
        &self,
        state: &mut ProtocolState,
        prepare_support: bool,
    ) -> Result<(), RuntimeError> {

        let fresh = match state.in_flight_request.as_ref() {
            Some(request) => {
                self.request_requires_fresh_context(request.kind)
                    || matches!(
                        request.worker_context.next_context_mode,
                        crate::model::WorkerContextMode::Fresh
                    )
            }
            None => false,
        };
        if let Some(request) = state.in_flight_request.as_mut() {
            request.fresh_context = fresh;
            // Sidecar queue redesign (Q6/A4): resolve the prompt
            // ADVERTISEMENT flag from the config block, BEFORE the
            // prompt-contract population below renders the payload /
            // fragment list. The verifier-bindings precedent:
            // `metadata.config_path` is read fresh on every
            // issue/reissue, so enablement is a config edit — with the
            // same accepted caveat that a mid-flight config flip
            // changes advertisement between issue and reissue (the
            // VALIDATED queue fields stay state-derived and
            // byte-stable regardless).
            if request.kind == crate::model::RequestKind::Review {
                if let Some(config_path) = self.metadata.config_path.as_deref() {
                    let cfg = crate::sidecar::load_sidecar_runtime_config(config_path)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                    request.sidecar_advertise_queue_fields = cfg.is_some();
                } else {
                    request.sidecar_advertise_queue_fields = false;
                }
            } else {
                request.sidecar_advertise_queue_fields = false;
            }
            // Process memory (spec §5): same pass, same precedent —
            // resolve the `memory_challenges` ADVERTISEMENT flag from
            // `process-memory/` on disk, which only the runtime can
            // read. Worker and Review are the two contracts that carry
            // the channel; every other kind stays off the wire so
            // memory-less runs keep byte-identical requests.
            request.process_memory_active = matches!(
                request.kind,
                crate::model::RequestKind::Worker | crate::model::RequestKind::Review
            ) && self
                .metadata
                .repo_path
                .as_deref()
                .is_some_and(crate::process_memory::has_active_entries);
            crate::populate_request_prompt_contracts(request, self.metadata.repo_path.as_deref());
            if matches!(
                request.kind,
                crate::model::RequestKind::Paper
                    | crate::model::RequestKind::Corr
                    | crate::model::RequestKind::Sound
            ) {
                let config_path = self.metadata.config_path.as_deref().ok_or_else(|| {
                    RuntimeError::InvalidRuntimeState(
                        "runtime is missing config_path for verifier lane binding resolution"
                            .into(),
                    )
                })?;
                let bindings = crate::resolve_request_verifier_bindings(config_path, request)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                request.paper_verify_lane_bindings = bindings.paper_verify_lane_bindings;
                request.corr_verify_lane_bindings = bindings.corr_verify_lane_bindings;
                request.sound_verify_lane_bindings = bindings.sound_verify_lane_bindings;
            } else {
                request.paper_verify_lane_bindings.clear();
                request.corr_verify_lane_bindings.clear();
                request.sound_verify_lane_bindings.clear();
            }
            if matches!(
                request.kind,
                crate::model::RequestKind::Worker
                    | crate::model::RequestKind::Review
                    | crate::model::RequestKind::Audit
                    | crate::model::RequestKind::StuckMathAudit
            ) {
                let config_path = self.metadata.config_path.as_deref().ok_or_else(|| {
                    RuntimeError::InvalidRuntimeState(
                        "runtime is missing config_path for actor binding resolution".into(),
                    )
                })?;
                let bindings = crate::resolve_request_actor_bindings(config_path, request)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                request.worker_binding = bindings.worker_binding;
                request.reviewer_binding = bindings.reviewer_binding;
                request.stuck_math_audit_binding = bindings.stuck_math_audit_binding;
            } else {
                request.worker_binding = crate::BridgeActorBinding::default();
                request.reviewer_binding = crate::BridgeActorBinding::default();
                request.stuck_math_audit_binding = crate::BridgeActorBinding::default();
            }
            if prepare_support && request.runtime_support_required {
                let Some(repo_path) = self.metadata.repo_path.as_deref() else {
                    return Err(RuntimeError::InvalidRuntimeState(
                        "support-required request missing repo_path metadata".into(),
                    ));
                };
                crate::ensure_tablet_support_available(repo_path, &request.current_present_nodes)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
            }
        }
        Ok(())
    }

    fn restore_repo_worktree_to_head(&self, repo_path: &Path) -> Result<(), RuntimeError> {
        restore_worktree_to_head(repo_path)
    }

    /// List `supervisor2/clean-*` tags in the given repo, sorted
    /// newest-first. Shared between `validate_last_clean_tag_consistency`
    /// (load-time atomicity check, audit Option C) and
    /// `restore_repo_worktree_to_last_clean` (runtime LastClean apply).
    ///
    /// Returns:
    /// - `Ok(vec)` — git ran successfully (`status.success()`); `vec`
    ///   contains the trimmed non-empty tag names in newest-first order.
    ///   May be empty if the repo legitimately has no clean tags.
    /// - `Err(reason)` — git invocation failed entirely (binary missing,
    ///   spawn error) OR git exited non-zero (repo not a git repo,
    ///   permission error, etc.). `reason` captures stderr + exit code
    ///   (or the io error message) for operator triage. Callers must
    ///   distinguish "git unavailable" from "tags listed cleanly with
    ///   empty result" — the validator soft-no-ops on `Err` (defers to
    ///   downstream paths that need git for proper context), the
    ///   LastClean apply errs hard on `Err` and includes `reason` in
    ///   the surfaced message.
    fn list_supervisor_clean_tags(repo_path: &Path) -> Result<Vec<String>, String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["tag", "--list", "supervisor2/clean-*", "--sort=-refname"])
            .output()
            .map_err(|err| format!("git tag spawn failed: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "git tag exited with code {:?}; stderr={:?}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Resolve the commit-ish a LastClean reset should rewind to.
    ///
    /// Bug 2 (live-run incident 2026-06-26): selection MUST be
    /// commit-pinned / ancestor-of-HEAD — never the lexically-highest
    /// `supervisor2/clean-{event_count}` tag. `event_count` (the tag
    /// suffix) is the per-cycle event-log line count, which is
    /// NON-monotonic across event-log segmentation and prior rewinds,
    /// so the old `--sort=-refname` + `.first()` could pick an ancient
    /// checkpoint (the incident's 226-cycle catastrophic rollback).
    ///
    /// Selection (both candidates must be ANCESTORS of HEAD; pick the one
    /// NEAREST to HEAD = fewest commits behind = most recent):
    /// 1. `state.last_clean_commit` (the durable commit pointer recorded
    ///    when the clean checkpoint was written) — the exact commit the
    ///    `last_clean_*` logical mirrors correspond to.
    /// 2. The `supervisor2/clean-*` tag that is an ANCESTOR of HEAD and
    ///    NEAREST to HEAD. On the linear checkpoint history this is the
    ///    greatest-cycle clean checkpoint reachable from HEAD.
    ///
    /// We take the MORE-RECENT (nearer-HEAD) of the two rather than
    /// unconditionally preferring the commit pointer (round-2 audit
    /// fold-in): if a `rev-parse HEAD` failed during a later clean
    /// checkpoint, `last_clean_commit` can lag behind a newer clean tag,
    /// and the newer tag is the correct (less-lossy) target. Ties go to
    /// the commit pointer (it is the precise mirror match). If neither
    /// yields a HEAD-ancestor target → `Err`. We never fall back to a
    /// non-ancestor or a stale higher-event_count tag.
    fn resolve_last_clean_commitish(
        &self,
        repo_path: &Path,
        tags: &[String],
    ) -> Result<String, RuntimeError> {
        // Track the nearest-HEAD candidate (smallest commits-behind wins).
        let mut best: Option<(u64, String)> = None;
        let mut consider = |behind: u64, commitish: String| {
            let take = match &best {
                Some((best_behind, _)) => behind < *best_behind,
                None => true,
            };
            if take {
                best = Some((behind, commitish));
            }
        };
        // (1) Commit pointer recorded in state.
        if let Some(commit) = self.state.last_clean_commit.as_deref() {
            let commit = commit.trim();
            if !commit.is_empty() && git_is_ancestor_of_head(repo_path, commit) {
                // When the distance is computable we compare it against the
                // tags; when it is not (rev-list error) we fall back to 0 so a
                // resolvable HEAD-ancestor pointer still wins — the round-1
                // "prefer the pointer" behaviour, retained only for the
                // unknown-distance case.
                let behind = git_commits_behind_head(repo_path, commit).unwrap_or(0);
                consider(behind, commit.to_string());
            } else if !commit.is_empty() {
                eprintln!(
                    "trellis: recorded last_clean_commit={commit} is not an ancestor of HEAD \
                     in {} — falling back to ancestor-of-HEAD clean-tag selection.",
                    repo_path.display()
                );
            }
        }
        // (2) Nearest HEAD-ancestor clean tag.
        for tag in tags {
            let tag = tag.as_str();
            if !git_is_ancestor_of_head(repo_path, tag) {
                continue;
            }
            let Some(behind) = git_commits_behind_head(repo_path, tag) else {
                continue;
            };
            consider(behind, tag.to_string());
        }
        if let Some((_, commitish)) = best {
            return Ok(commitish);
        }
        // (3) No safe target.
        Err(RuntimeError::InvalidRuntimeState(format!(
            "LastClean reset requested but no safe rewind target found in {}: \
             state.last_clean_commit is {} and none of the {} supervisor2/clean-* \
             tag(s) is an ancestor of HEAD. Refusing to rewind to a non-ancestor / \
             stale tag (Bug 2 guard). Investigate the checkpoint history and resolve \
             manually.",
            repo_path.display(),
            self.state
                .last_clean_commit
                .as_deref()
                .map(|c| format!("set to `{c}` (not a HEAD ancestor)"))
                .unwrap_or_else(|| "absent".to_string()),
            tags.len(),
        )))
    }

    /// Atomicity validator (audit, Option C): on `load()`, verify that
    /// the loaded state's claim "last_clean mirrors are ready" is
    /// consistent with the git repo actually having at least one
    /// `supervisor2/clean-*` tag. The two can diverge if the
    /// checkpoint sink succeeded at producing the in-memory commit but
    /// failed before writing the clean tag (or if a process crash
    /// landed between the sink's commit and the kernel's
    /// `persist_state` write — the post-A reorder narrows that window
    /// from "any sink failure" to "process kill in microseconds").
    /// Without this check, the divergence would surface only on a
    /// reviewer-driven LastClean rewind — at which point
    /// `restore_repo_worktree_to_last_clean` would either fail loudly
    /// (no tag) or silently rewind to a STALE tag whose state doesn't
    /// match the loaded `last_clean_*` mirrors. Better to fail at
    /// load with an actionable error.
    ///
    /// Returns Err(InvalidRuntimeState) when the state is internally
    /// inconsistent — specifically when git ran cleanly AND the repo
    /// has zero `supervisor2/clean-*` tags despite state claiming
    /// readiness. Returns Ok(()) for any benign case (no repo_path,
    /// mirrors not ready, OR git invocation failed entirely so we
    /// can't tell — bridge's existing error paths surface real git
    /// corruption with proper context when they actually need git).
    fn validate_last_clean_tag_consistency(&self) -> Result<(), RuntimeError> {
        if !self.state.last_clean_verifier_mirror_ready {
            return Ok(());
        }
        let Some(repo_path) = self.metadata.repo_path.as_deref() else {
            return Ok(());
        };
        // Soft no-op when git is unavailable (helper returns Err for
        // binary-missing, repo-not-git, permission errors, etc.) —
        // bridge's existing runtime paths surface real corruption when
        // they actually need git, with proper context. The validator's
        // job is the narrower one: catch the specific divergence where
        // git ran cleanly AND the repo has zero clean tags.
        let tags = match Self::list_supervisor_clean_tags(repo_path) {
            Ok(t) => t,
            Err(_) => return Ok(()),
        };
        if !tags.is_empty() {
            return Ok(());
        }
        Err(RuntimeError::InvalidRuntimeState(format!(
            "loaded state at cycle={} has last_clean_verifier_mirror_ready=true \
             (mirror fields populated, has_ever_been_clean={}) but the git repo \
             at {} has zero `supervisor2/clean-*` tags. The state file is ahead \
             of git — most likely a checkpoint sink failure or process crash \
             between sink success and state persistence. LastClean reset cannot \
             land safely (no tag to rewind to). Investigate {}/.trellis-history \
             for the most recent successful checkpoint and either roll back the \
             state file or recreate the missing tag(s).",
            self.state.cycle,
            self.state.has_ever_been_clean,
            repo_path.display(),
            repo_path.display(),
        )))
    }

    /// Rewind the repo worktree to the most recent `supervisor2/clean-*`
    /// tag written by `git_checkpoint_hook.py`. These tags mark checkpoints
    /// where `state.global_blockers().is_empty()` at emission time. Returns
    /// an error if no such tag exists — the reviewer should only send
    /// `ResetChoice::LastClean` when `cycles_since_clean >= 1`, and the
    /// allowed-resets gate enforces that, so in practice at least one
    /// clean tag should always exist when this is called.
    ///
    /// Process memory (spec §7): when `preserve_process_memory` is true
    /// (the reviewer default), the pre-rewind HEAD's `process-memory/`
    /// directory is restored after the reset — a LastClean rewind usually
    /// means "this line failed", which is when its refuted-route entries
    /// are most valuable. Not-yet-checkpointed entries (untracked at
    /// rewind time) are additionally spared by a `git clean` exclusion and
    /// join the carry-forward, with `INDEX.md` regenerated over the union.
    /// The §4 monotonicity invariant (files only added or status-flipped
    /// forward) makes this file-level restore the correct union merge.
    /// The next checkpoint commits the carry-forward. When false (poisoned
    /// memory), tracked entries revert with every other tracked file and
    /// untracked ones are swept by the clean; the abandoned committed
    /// entries remain reachable on the `trellis-rewound/*` branch.
    fn restore_repo_worktree_to_last_clean(
        &self,
        repo_path: &Path,
        preserve_process_memory: bool,
    ) -> Result<(), RuntimeError> {
        let start = std::time::Instant::now();
        let tags_result = Self::list_supervisor_clean_tags(repo_path);
        let duration = start.elapsed().as_secs_f64();
        // Telemetry: `ok = git invocation succeeded` (matches pre-fix
        // semantics — Err means the subprocess didn't run cleanly).
        // `stdout_len` is the sum of returned tag bytes + 1 each;
        // off-by-N from raw git stdout bytes but the consumer at
        // trellis/usage_report.py:135-164 only aggregates counts +
        // `ok`/duration, not byte sums for control flow.
        let git_ran = tags_result.is_ok();
        let tags_vec: Vec<String> = match &tags_result {
            Ok(v) => v.clone(),
            Err(_) => Vec::new(),
        };
        crate::check_ledger::append_kind(
            repo_path,
            "git",
            "tag",
            duration,
            git_ran,
            tags_vec.iter().map(|t| t.len() + 1).sum(),
            0,
        );
        let tags_vec = tags_result.map_err(|reason| {
            // Propagate the helper's captured stderr/exit/io error so
            // operators triaging a failed LastClean apply have the
            // actual git failure context, not just a generic message.
            RuntimeError::InvalidRuntimeState(format!(
                "list supervisor2/clean-* tags failed: {reason}"
            ))
        })?;
        if tags_vec.is_empty() && self.state.last_clean_commit.is_none() {
            return Err(RuntimeError::InvalidRuntimeState(
                "LastClean reset requested but no supervisor2/clean-* tag found in repo".into(),
            ));
        }
        // Bug 2: select by commit pointer / nearest HEAD-ancestor tag,
        // never the lexically-highest (potentially stale) tag.
        let target = self.resolve_last_clean_commitish(repo_path, &tags_vec)?;
        let tag = target.as_str();
        // Process memory (spec §7): capture the pre-reset HEAD so the
        // carry-forward below can restore `process-memory/` from the
        // abandoned line. Best-effort capture: without a resolvable HEAD
        // there is nothing to carry forward.
        let pre_rewind_head = if preserve_process_memory {
            git_head_sha(repo_path)
        } else {
            None
        };
        // Preserve the abandoned lineage as a `trellis-rewound/...` branch
        // BEFORE the destructive reset. The branch ref keeps the commits
        // reachable so they can be inspected later (and pushed to the
        // archive remote, which then carries the full history of what was
        // attempted, not just the surviving line). Best-effort: branch
        // creation failure is logged but does not block the reset.
        self.preserve_abandoned_branch_for_rewind(repo_path, tag);
        // Process memory (spec §7): entries materialized since the last
        // checkpoint are still UNTRACKED, so it is the `git clean` — not
        // the reset — that would delete them. With preserve=true (the
        // reviewer default) the sweep spares `process-memory/` and the
        // survivors join the committed carry-forward below; with
        // preserve=false (poisoned memory) the exclusion is intentionally
        // omitted so memory reverts fully to the clean tag (the reset
        // reverts tracked entries, the unexcluded clean removes the
        // untracked ones).
        // Anchored (`/`-prefixed) so only the repo-root process-memory dir
        // is spared; a stray nested `Tablet/process-memory/` is still swept.
        let pm_exclude = format!("/{}", crate::process_memory::PROCESS_MEMORY_DIR);
        let mut clean_command = vec![
            "clean",
            "-fd",
            "-e",
            ".trellis-history/event-log.restore-shield",
        ];
        if preserve_process_memory {
            clean_command.push("-e");
            clean_command.push(pm_exclude.as_str());
        }
        with_event_log_shielded(repo_path, || {
            for command in [vec!["reset", "--hard", tag], clean_command] {
                let start = std::time::Instant::now();
                let output = Command::new("git")
                    .arg("-C")
                    .arg(repo_path)
                    .args(&command)
                    .output();
                let duration = start.elapsed().as_secs_f64();
                let output = match output {
                    Ok(o) => {
                        crate::check_ledger::append_kind(
                            repo_path,
                            "git",
                            command[0],
                            duration,
                            o.status.success(),
                            o.stdout.len(),
                            o.stderr.len(),
                        );
                        o
                    }
                    Err(err) => {
                        crate::check_ledger::append_kind(
                            repo_path, "git", command[0], duration, false, 0, 0,
                        );
                        return Err(err.into());
                    }
                };
                if !output.status.success() {
                    return Err(RuntimeError::InvalidRuntimeState(format!(
                    "restore last-clean worktree failed for `git {}` with exit code {:?}; stdout={:?}; stderr={:?}",
                    command.join(" "),
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                )));
                }
            }
            Ok(())
        })?;
        // Process memory (spec §7): restore `process-memory/` from the
        // pre-rewind HEAD. Tolerates the directory not existing in that
        // commit (pre-migration runs / no entries yet) by probing with
        // `git ls-tree` first; an actual checkout failure is a real
        // error and aborts the step.
        if let Some(sha) = pre_rewind_head.as_deref() {
            restore_process_memory_from_commit(repo_path, sha)?;
        }
        if preserve_process_memory {
            // The carry-forward checkout restores the pre-rewind COMMITTED
            // INDEX.md, which does not list the untracked survivors spared
            // by the clean exclusion above. INDEX.md is derived state;
            // regenerate it from the surviving union of entry files.
            crate::process_memory::regenerate_index(repo_path).map_err(|err| {
                RuntimeError::InvalidRuntimeState(format!(
                    "process-memory index regeneration after LastClean restore failed: {err}"
                ))
            })?;
        }
        // Purge stale .lake/build/lib/lean/Tablet/ artifacts for nodes whose
        // source `.lean` file no longer exists on disk after the rewind. Without
        // this, deleted node oleans persist and Lean resolves imports for nodes
        // that have no current source — i.e. "ghost" imports that pollute the
        // worker/audit semantic view of the tablet (probe.lean compiles against
        // dead code, reviewer/worker reason about deleted declarations). The
        // git clean above doesn't touch .lake/build because it's gitignored.
        purge_stale_tablet_build_artifacts(repo_path);
        Ok(())
    }

    fn restore_theorem_stating_node_and_prune_orphans(
        &self,
        repo_path: &Path,
        state: &mut ProtocolState,
        node: &NodeId,
    ) -> Result<(), RuntimeError> {
        if !state.resettable_theorem_stating_nodes().contains(node) {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "theorem-stating reset requested for non-resettable node `{}`",
                node.as_str()
            )));
        }
        let baseline = recover_theorem_stating_baseline_from_git(repo_path).ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "could not recover theorem-stating baseline checkpoint from git history".into(),
            )
        })?;
        if !baseline.state.live.present_nodes.contains(node) {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "theorem-stating baseline commit {} does not contain node `{}`",
                baseline.commit,
                node.as_str()
            )));
        }

        restore_repo_path_from_git(
            repo_path,
            &baseline.commit,
            &format!("Tablet/{}.lean", node.as_str()),
        )?;
        let restored_lean = fs::read_to_string(
            repo_path
                .join("Tablet")
                .join(format!("{}.lean", node.as_str())),
        )?;
        crate::filespec_split::validate_filespec(&restored_lean, node.as_str()).map_err(|err| {
            RuntimeError::InvalidRuntimeState(format!(
                "theorem-stating baseline commit {} restored Tablet/{}.lean, but it does not satisfy current FILESPEC: {}",
                baseline.commit,
                node.as_str(),
                err
            ))
        })?;
        restore_repo_path_from_git(
            repo_path,
            &baseline.commit,
            &format!("Tablet/{}.tex", node.as_str()),
        )?;

        let present_after_restore = crate::worker_normalization::present_nodes_from_repo(repo_path)
            .map_err(RuntimeError::InvalidRuntimeState)?;
        let deps_after_restore =
            crate::worker_normalization::direct_deps_from_repo(repo_path, &present_after_restore);
        let mut target_claims =
            target_claims_after_theorem_stating_node_restore(state, &baseline.state, node);
        retain_target_claims_for_present(
            &mut target_claims,
            &present_after_restore,
            &state.configured_targets,
        );
        let coverage_after_restore = crate::worker_normalization::coverage_from_claims(
            &state.configured_targets,
            &target_claims,
            &present_after_restore,
        );
        // Challenge-covering nodes root support exactly like
        // paper-covering nodes; without them a cone-clean on a
        // challenge run would sweep the covering declarations (and
        // their support cones) as orphans.
        let mut orphan_roots: std::collections::BTreeSet<NodeId> = coverage_after_restore
            .values()
            .flat_map(|nodes| nodes.iter().cloned())
            .collect();
        let configured_challenge_ids: std::collections::BTreeSet<_> =
            state.configured_challenge_targets.keys().cloned().collect();
        let challenge_coverage_after_restore =
            crate::worker_normalization::challenge_coverage_from_claims(
                &configured_challenge_ids,
                &state.challenge_claims,
                &present_after_restore,
            );
        orphan_roots.extend(
            challenge_coverage_after_restore
                .values()
                .flat_map(|nodes| nodes.iter().cloned()),
        );
        let orphans = ProtocolState::orphan_nodes_for_roots(
            &present_after_restore,
            &orphan_roots,
            &deps_after_restore,
        );
        if orphans.contains(node) {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "theorem-stating reset would make reset node `{}` orphaned; refusing to delete the selected node",
                node.as_str()
            )));
        }
        for orphan in &orphans {
            remove_tablet_node_files(repo_path, orphan)?;
        }

        crate::tablet_support::sync_tablet_support_from_repo(repo_path)
            .map_err(RuntimeError::InvalidRuntimeState)?;
        purge_stale_tablet_build_artifacts(repo_path);

        let paper_approved_for_observation =
            paper_approved_after_theorem_stating_node_restore(state, &baseline.state, node);
        let observed = observe_live_tablet_state_from_repo(
            repo_path,
            state,
            target_claims,
            &paper_approved_for_observation,
            paper_source_path_from_config(self.metadata.config_path.as_deref()).as_deref(),
            goal_prose_path_from_config(
                self.metadata.config_path.as_deref(),
                repo_path,
                state,
            )
            .as_deref(),
        )?;
        let mut changed_nodes = orphans.clone();
        changed_nodes.insert(node.clone());
        for old in state
            .live
            .present_nodes
            .difference(&observed.live.present_nodes)
        {
            changed_nodes.insert(old.clone());
        }
        for new in observed
            .live
            .present_nodes
            .difference(&state.live.present_nodes)
        {
            changed_nodes.insert(new.clone());
        }
        state.install_observed_live_tablet_state(
            observed.live,
            observed.node_kinds,
            observed.proof_nodes,
            observed.deps,
            observed.target_claims,
        );
        state.restore_theorem_stating_baseline_for_node(node, &baseline.state);
        let deleted_records = state.prune_local_closure_after_runtime_tablet_reset(&changed_nodes);
        for deleted in deleted_records {
            delete_persisted_local_closure_record(&self.paths.root, &deleted);
        }
        state.commit_live();
        // Sidecar queue redesign §1.2: the deterministic tail prune
        // normally runs inside `apply_event` immediately before
        // `validate()`, but this runtime sweep mutates the live tablet
        // (orphan deletion) OUTSIDE the transition function — a queued
        // node deleted by the orphan sweep would otherwise leave a stale
        // queue entry and fail the queue invariant below (reason
        // `deleted` in the prune log, same as the apply_event path).
        state.prune_sidecar_queue();
        // Same rationale for closure provenance: the orphan sweep
        // deletes live tablet nodes outside the transition function, so
        // a provenance entry for a swept node would outlive the node
        // and fail the provenance invariant below.
        state.prune_closure_provenance();
        state
            .validate()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        Ok(())
    }

    /// Create a `trellis-rewound/{YYYYMMDD-HHMMSS}-to-{tag-suffix}` branch
    /// pointing at the current HEAD, so the soon-to-be-abandoned line stays
    /// reachable after a `git reset --hard` rewind. Quiet best-effort: any
    /// failure is recorded in the check ledger and otherwise swallowed —
    /// preserving history is a nice-to-have, not a precondition for the
    /// rewind itself.
    fn preserve_abandoned_branch_for_rewind(&self, repo_path: &Path, target_tag: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let secs = now as i64;
        // Crude UTC formatter — avoids pulling in chrono. We only need a
        // monotonic-ish, human-readable suffix; precision is irrelevant.
        let day = secs / 86400;
        let day_secs = secs % 86400;
        let hh = day_secs / 3600;
        let mm = (day_secs % 3600) / 60;
        let ss = day_secs % 60;
        // Days since 1970-01-01 → naive Y/M/D split. Good enough for a label.
        let mut year = 1970i64;
        let mut days_left = day;
        loop {
            let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            let in_year = if leap { 366 } else { 365 };
            if days_left < in_year {
                break;
            }
            days_left -= in_year;
            year += 1;
        }
        let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let mdays = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        let mut month = 1i64;
        for &dm in mdays.iter() {
            if days_left < dm {
                break;
            }
            days_left -= dm;
            month += 1;
        }
        let day_of_month = days_left + 1;
        let ts_label = format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            year, month, day_of_month, hh, mm, ss,
        );
        let tag_suffix = target_tag
            .strip_prefix("supervisor2/clean-")
            .unwrap_or(target_tag)
            .replace('/', "-");
        // Disambiguate concurrent rewinds with the abandoned HEAD's short SHA.
        let mut suffix = String::new();
        let head_proc = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["rev-parse", "--short=8", "HEAD"])
            .output();
        if let Ok(o) = head_proc {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !s.is_empty() {
                    suffix = format!("-{}", s);
                }
            }
        }
        let branch_name = format!("trellis-rewound/{}-to-{}{}", ts_label, tag_suffix, suffix);

        // `git branch <name> HEAD` is non-destructive: fails harmlessly if a
        // branch with this exact name already exists. We don't `--force` it
        // because two rewinds at the same second from the same HEAD would
        // produce identical lineage anyway — first writer wins.
        let start = std::time::Instant::now();
        let res = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["branch", &branch_name, "HEAD"])
            .output();
        let duration = start.elapsed().as_secs_f64();
        match res {
            Ok(o) => {
                crate::check_ledger::append_kind(
                    repo_path,
                    "git",
                    "branch",
                    duration,
                    o.status.success(),
                    o.stdout.len(),
                    o.stderr.len(),
                );
            }
            Err(_) => {
                crate::check_ledger::append_kind(repo_path, "git", "branch", duration, false, 0, 0);
            }
        }
    }

    fn restore_repo_worktree_to_active_worker_base(
        &self,
        repo_path: &Path,
    ) -> Result<(), RuntimeError> {
        let manifest_path = self.active_worker_base_manifest_path();
        let manifest: ActiveWorkerBaseManifest =
            serde_json::from_slice(&fs::read(&manifest_path).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "active worker base manifest {} is unavailable: {error}",
                    manifest_path.display()
                ))
            })?)
            .map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "active worker base manifest {} is malformed: {error}",
                    manifest_path.display()
                ))
            })?;
        if manifest.schema_version != ACTIVE_WORKER_BASE_SCHEMA_VERSION {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "active worker base manifest {} has unsupported schema_version {}; expected {}",
                manifest_path.display(),
                manifest.schema_version,
                ACTIVE_WORKER_BASE_SCHEMA_VERSION,
            )));
        }
        let artifact_epoch =
            self.load_certificate_artifact_epoch(&self.state, manifest.artifact_epoch)?;

        // Validate the complete source/artifact snapshot before deleting any
        // live destination. A corrupt nested entry, blob, or manifest
        // contradiction must be a non-mutating failure, never a destructive
        // half-restore.
        validate_worker_surface_snapshot(
            &self.active_worker_base_tablet_dir(),
            manifest.tablet_present,
            "Tablet",
        )?;
        validate_worker_surface_snapshot(
            &self.active_worker_base_reference_dir(),
            manifest.reference_present,
            "reference",
        )?;
        let staged_artifacts = self.stage_certificate_artifact_epoch(repo_path, &artifact_epoch)?;
        restore_worker_surface(
            &self.active_worker_base_tablet_dir(),
            &repo_path.join("Tablet"),
            manifest.tablet_present,
        )?;
        restore_worker_surface(
            &self.active_worker_base_reference_dir(),
            &repo_path.join("reference"),
            manifest.reference_present,
        )?;
        self.install_staged_certificate_artifact_epoch(
            repo_path,
            &artifact_epoch,
            &staged_artifacts,
        )?;
        crate::dormant_store::validate_configured_decide_layout(repo_path, &self.state)
            .map_err(RuntimeError::InvalidRuntimeState)?;
        Ok(())
    }

    /// Audit followup #2 (Problem B): SIGHUP-style restart leaves the
    /// worker repo dirty if a partial worker burst mutated `Tablet/`
    /// before the supervisor was killed. The next bridge reissue must
    /// restore the worker repo to the captured `active_worker_base`
    /// snapshot BEFORE rebuilding the acceptance context — otherwise
    /// `before_snapshot` is captured against the post-mutation disk and
    /// the unauthorized edits become baseline rather than candidate
    /// changes. Exposed via the `RestoreActiveWorkerBase` CLI subcommand
    /// for the Python bridge to invoke at the top of `_handle_worker`
    /// when no `.done` artifact is present (i.e., we're about to
    /// relaunch the worker, possibly after a crash).
    ///
    /// Returns `Ok(false)` and is a no-op when:
    ///   - runtime metadata has no `repo_path` (legacy / dry-run state),
    ///   - no in-flight request exists (bridge dispatching a fresh request),
    ///   - the in-flight request is not a Worker request (no Tablet baseline
    ///     to restore for non-worker burst kinds).
    /// Returns `Ok(true)` after a successful restore.
    ///
    /// Returns `Err(InvalidRuntimeState(...))` when the in-flight request
    /// IS a Worker but the `active_worker_base/worker_surfaces.json` snapshot
    /// manifest is missing. This is the dirty-disk-relaunch hazard: the bridge calls
    /// this from `_handle_worker` precisely because a previous worker
    /// burst may have crashed mid-write; if the snapshot it would
    /// rewind to is also gone (interrupted earlier step, manual cleanup,
    /// migration), the bridge cannot establish a clean baseline before
    /// rebuilding `before_snapshot`. Failing loudly here lets the bridge
    /// route via its existing exception handler to a transport_failure
    /// classification, which the kernel then handles via its
    /// transport-attempt budget — rather than silently absorbing dirty
    /// Tablet/ writes into the new acceptance baseline.
    pub fn restore_active_worker_base_for_inflight(&self) -> Result<bool, RuntimeError> {
        let Some(repo_path) = self.metadata.repo_path.as_deref() else {
            return Ok(false);
        };
        let Some(request) = self.state.in_flight_request.as_ref() else {
            return Ok(false);
        };
        if request.kind != crate::model::RequestKind::Worker {
            return Ok(false);
        }
        if !self.active_worker_base_manifest_path().is_file() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "restore_active_worker_base_for_inflight: in-flight Worker \
                 request id={} cycle={} but active_worker_base/worker_surfaces.json \
                 snapshot manifest is missing — cannot establish clean baseline for \
                 worker relaunch",
                request.id, request.cycle,
            )));
        }
        self.restore_repo_worktree_to_active_worker_base(repo_path)?;
        Ok(true)
    }

    /// True for any non-Valid worker response. Three call sites use it:
    ///   1. `event_requires_repo_worktree_restore` /
    ///      `restore_repo_worktree_for_event` → triggers worktree
    ///      rollback to `active_worker_base` so out-of-scope and
    ///      contract-violating disk effects don't leak between attempts.
    ///   2. `capture_last_invalid_snapshot_for_event` → snapshots
    ///      `Tablet/` to a sidecar directory before rollback so the
    ///      worker's WIP is preserved.
    ///   3. `update_last_invalid_for_event` → persists the snapshot +
    ///      metadata to `.trellis-history/worker_state/last_invalid/`
    ///      for the next worker's prompt context.
    ///
    /// Stuck and NeedsRestructure used to be excluded from the rollback
    /// + snapshot paths under the assumption that the worker had
    /// reverted its tablet changes before returning, but that assumption
    /// was never enforced and let a corruption (a worker editing a
    /// sibling file outside its Easy-mode scope) survive across worker
    /// bursts and pollute the next baseline. Treat them the same as
    /// Invalid: capture the WIP, then restore disk to baseline.
    fn worker_response_should_preserve_attempt(response: &crate::model::WorkerResponse) -> bool {
        response.status == ResponseStatus::Malformed
            || matches!(
                response.outcome,
                WorkerOutcome::Invalid
                    | WorkerOutcome::Stuck
                    | WorkerOutcome::NeedsRestructure
                    // PV under-model (Slice 1): a non-progress verdict with no
                    // committed tablet edit — capture WIP, restore baseline,
                    // same as Stuck/NR.
                    | WorkerOutcome::TargetFalseUnderModel
            )
    }

    fn worker_response_has_checker_mismatch(response: &crate::model::WorkerResponse) -> bool {
        response
            .deterministic_rejection_reasons
            .iter()
            .any(|reason| reason.starts_with("authoritative checker mismatch:"))
    }

    fn maybe_clear_worker_history_for_checker_mismatch(&mut self, event: &ProtocolEvent) {
        let ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(response),
        } = event
        else {
            return;
        };
        if !Self::worker_response_has_checker_mismatch(response) {
            return;
        }
        self.metadata
            .native_history_kinds
            .remove(&request_history_key(
                crate::model::RequestKind::Worker,
                self.state.phase,
            ));
    }

    fn should_record_native_history_for_event(
        &self,
        event: &ProtocolEvent,
        kind: crate::model::RequestKind,
    ) -> bool {
        match event {
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(response),
            } if kind == crate::model::RequestKind::Worker => {
                !Self::worker_response_has_checker_mismatch(response)
            }
            _ => true,
        }
    }

    fn capture_last_invalid_snapshot_for_event(
        &self,
        event: &ProtocolEvent,
    ) -> Result<Option<PathBuf>, RuntimeError> {
        let ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(response),
        } = event
        else {
            return Ok(None);
        };
        // Capture the Tablet/ snapshot for any non-Valid outcome — the
        // worker's WIP (whether rejected edits, stuck mid-progress, or
        // needs-restructure abandonment) is on disk and the next worker
        // benefits from seeing it. The kernel will roll the worktree
        // back to active_worker_base after this capture so the WIP
        // doesn't pollute the next worker's baseline; preserving it as
        // a sidecar snapshot is what makes the rollback non-destructive.
        //
        // ALSO capture for a Valid outcome: a Valid response can still be
        // rejected deterministically at apply time (e.g. the live-orphan
        // rule), and that decision isn't known until after apply — by
        // which point the rollback has already destroyed the WIP unless
        // this capture exists (dec2flt example request 938, 2026-07-03: a
        // checker-passing restructure was rolled back with the retry
        // prompt pointing at a snapshot that was never written). An
        // ACCEPTED Valid response's capture is discarded in
        // `update_last_invalid_for_event`.
        if !Self::worker_response_should_preserve_attempt(response)
            && !(response.status == ResponseStatus::Ok && response.outcome == WorkerOutcome::Valid)
        {
            return Ok(None);
        }
        let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "invalid worker snapshot capture requires repo_path metadata".into(),
            )
        })?;
        let tablet_dir = repo_path.join("Tablet");
        if !tablet_dir.is_dir() {
            return Ok(None);
        }
        let capture_root = self.paths.root.join("last_invalid_capture");
        if capture_root.exists() {
            fs::remove_dir_all(&capture_root)?;
        }
        let capture_tablet = capture_root.join("Tablet");
        copy_dir_recursive(&tablet_dir, &capture_tablet)?;
        Ok(Some(capture_root))
    }

    fn update_last_invalid_for_event(
        &self,
        event: &ProtocolEvent,
        captured_snapshot_root: Option<&Path>,
    ) -> Result<(), RuntimeError> {
        let repo_path = match self.metadata.repo_path.as_deref() {
            Some(path) => path,
            None => return Ok(()),
        };
        let last_invalid_dir = repo_path
            .join(".trellis-history")
            .join("worker_state")
            .join("last_invalid");
        let last_invalid_tablet = last_invalid_dir.join("Tablet");
        let last_invalid_metadata = last_invalid_dir.join("metadata.json");
        match event {
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(response),
            } => {
                // Runs AFTER `self.state = next_state`, so the engine's
                // apply decision is visible here. A Valid response the
                // engine rejected deterministically (live-orphan rule,
                // etc.) leaves `deterministic_worker_rejection_reasons`
                // non-empty (an accept clears it via
                // `clear_retry_context`) — preserve its WIP exactly like
                // the non-Valid exits.
                let kernel_rejected_valid = response.outcome == WorkerOutcome::Valid
                    && !self.state.deterministic_worker_rejection_reasons.is_empty();
                if Self::worker_response_should_preserve_attempt(response) || kernel_rejected_valid
                {
                    if last_invalid_dir.exists() {
                        fs::remove_dir_all(&last_invalid_dir)?;
                    }
                    if let Some(snapshot_root) = captured_snapshot_root {
                        let captured_tablet = snapshot_root.join("Tablet");
                        if captured_tablet.is_dir() {
                            copy_dir_recursive(&captured_tablet, &last_invalid_tablet)?;
                        }
                    }
                    fs::create_dir_all(&last_invalid_dir)?;
                    let rejection_reasons = if response.deterministic_rejection_reasons.is_empty() {
                        &self.state.deterministic_worker_rejection_reasons
                    } else {
                        &response.deterministic_rejection_reasons
                    };
                    let metadata = json!({
                        "request_id": response.request_id,
                        "cycle": response.cycle,
                        "status": format!("{:?}", response.status),
                        "outcome": format!("{:?}", response.outcome),
                        "summary": response.summary,
                        "comments": response.comments,
                        "deterministic_rejection_reasons": crate::model::prompt_safe_deterministic_worker_rejection_reasons(
                            rejection_reasons,
                        ),
                        "present_nodes": response.snapshot.present_nodes,
                        "open_nodes": response.snapshot.open_nodes,
                        "coverage": response.snapshot.coverage,
                    });
                    fs::write(
                        last_invalid_metadata,
                        serde_json::to_string_pretty(&metadata)? + "\n",
                    )?;
                } else if last_invalid_dir.exists() {
                    fs::remove_dir_all(&last_invalid_dir)?;
                }
            }
            _ => {}
        }
        if let Some(snapshot_root) = captured_snapshot_root {
            if snapshot_root.exists() {
                fs::remove_dir_all(snapshot_root)?;
            }
        }
        Ok(())
    }

    fn refresh_in_flight_request_from_state(&mut self) {
        let Some(request) = self.state.in_flight_request.as_ref().cloned() else {
            return;
        };
        self.state.in_flight_request = Some(Box::new(
            self.state.expected_request(request.id, request.kind),
        ));
    }

    /// Normalize runtime hints and deploy-changed derived projections so the
    /// semantic in-flight-request invariant can be checked on reload. Every
    /// other request field remains byte-for-byte as persisted and is therefore
    /// still covered by `ProtocolState::validate`.
    fn normalize_in_flight_request_execution_hints_for_validation(&mut self) {
        normalize_in_flight_request_execution_hints(&mut self.state);
    }
}

/// Free-standing form of the reload path's hint strip, so any code that must
/// validate a state whose in-flight request carries dispatch-applied execution
/// hints can check the SEMANTIC projection instead. `expected_request` leaves
/// these fields deliberately unresolved, so `validate()` on the decorated form
/// compares against a projection it can never equal.
pub(crate) fn normalize_in_flight_request_execution_hints(state: &mut ProtocolState) {
    {
        let Some(persisted) = state.in_flight_request.as_ref() else {
            return;
        };
        let expected = state.expected_request(persisted.id, persisted.kind);
        let persisted = state.in_flight_request.as_mut().unwrap();
        persisted.fresh_context = expected.fresh_context;
        // The stating sidecar window became permissive by default. An old
        // checkpoint may therefore carry `false` here while current state
        // derives `true`; refresh this state-derived presentation bit before
        // strict request validation. Logged responses do not author it.
        persisted.sidecar_window_open = expected.sidecar_window_open;
        // The two runtime-resolved ADVERTISEMENT flags (sidecar queue
        // prompt surfaces; process-memory `memory_challenges`) are set by
        // the dispatch-hint pass from config / repo disk — never by the
        // engine, so `expected_request` always derives them false. They
        // were omitted here when the upstream dispatch pass grew them
        // (sidecar queue 5/7; memory_challenges advertisement), and the
        // first trust-v1 run that persisted a Worker request after an
        // audit's memory op activated `process-memory/` failed EVERY
        // reload with "in-flight request payload does not match derived
        // state" (c19, dec2flt example 20260825T205720Z: five consecutive
        // restore_active_worker_base transport failures → circuit
        // breaker). Strip them exactly like `fresh_context`; the
        // post-reconcile `apply_request_dispatch_hints` reattaches both
        // from the same sources the original dispatch read.
        persisted.sidecar_advertise_queue_fields = expected.sidecar_advertise_queue_fields;
        persisted.process_memory_active = expected.process_memory_active;
        // Source-derived acceptance identity and Cleanup transition baseline
        // projections are runtime facts, not agent-authored request content.
        // A deploy may add these fields while a Worker request is persisted;
        // normalize them before the semantic equality check, then the full
        // post-validation refresh below reissues the exact current projection.
        persisted.acceptance_logic_identity = expected.acceptance_logic_identity;
        persisted.current_orphan_nodes = expected.current_orphan_nodes;
        persisted.cleanup_baseline_formalization_valid =
            expected.cleanup_baseline_formalization_valid;
        // Derived from the hash-bound model-refinement projection in protocol
        // state. A pre-field in-flight request deserializes this as null while
        // the current binary derives the populated Worker/Corr view.
        persisted.reachable_opaque_inventory = expected.reachable_opaque_inventory.clone();
        // Dedicated artifact Corr bytes are a projection of the frozen trust
        // payload, not bridge-authored request state. Rebuild them from state
        // on reload exactly like the opaque-inventory projection.
        persisted.rust_witness_artifact_correspondence =
            expected.rust_witness_artifact_correspondence.clone();
        persisted.conditional_theorem_correspondence =
            expected.conditional_theorem_correspondence.clone();
        persisted.conditional_ratification_packet =
            expected.conditional_ratification_packet.clone();
        persisted.prompt_contract_version = expected.prompt_contract_version;
        persisted.project_invariants = expected.project_invariants;
        persisted.paper_contract = expected.paper_contract;
        persisted.corr_contract = expected.corr_contract;
        persisted.sound_contract = expected.sound_contract;
        persisted.worker_contract = expected.worker_contract;
        persisted.review_contract = expected.review_contract;
        persisted.audit_contract = expected.audit_contract;
        persisted.stuck_math_audit_contract = expected.stuck_math_audit_contract;
        persisted.paper_verify_lane_bindings = expected.paper_verify_lane_bindings;
        persisted.corr_verify_lane_bindings = expected.corr_verify_lane_bindings;
        persisted.sound_verify_lane_bindings = expected.sound_verify_lane_bindings;
        persisted.worker_binding = expected.worker_binding;
        persisted.reviewer_binding = expected.reviewer_binding;
        persisted.stuck_math_audit_binding = expected.stuck_math_audit_binding;
    }
}

impl SupervisorRuntime {
    fn request_requires_fresh_context(&self, kind: crate::model::RequestKind) -> bool {
        match kind {
            crate::model::RequestKind::Paper
            | crate::model::RequestKind::Corr
            | crate::model::RequestKind::Sound => true,
            crate::model::RequestKind::Worker | crate::model::RequestKind::Review => !self
                .metadata
                .native_history_kinds
                .contains(&request_history_key(kind, self.state.phase)),
            crate::model::RequestKind::HumanGate => false,
            // Cleanup-v2 audit is a single-burst structured-output role
            // with its own prompt-fragment family. Always treat it as
            // requiring a fresh context until/unless the bridge gains
            // audit-specific history tracking. Continuation bursts within
            // a single audit round carry their state via the scratchpad
            // surfaced in the prompt, not via bridge history.
            crate::model::RequestKind::Audit | crate::model::RequestKind::StuckMathAudit => true,
        }
    }

    fn record_native_history(&mut self, kind: crate::model::RequestKind, phase: Phase) {
        self.metadata
            .native_history_kinds
            .insert(request_history_key(kind, phase));
    }
}

fn persist_trust_gate_presentation(
    runtime_root: &Path,
    digest: crate::trust_base::Sha256Digest,
    bytes: &[u8],
) -> Result<(), RuntimeError> {
    let directory = runtime_root.join("presentations");
    fs::create_dir_all(&directory).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "failed to create immutable presentation store {}: {error}",
            directory.display()
        ))
    })?;
    let path = directory.join(format!("{digest}.bin"));
    match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(mut file) => {
            file.write_all(bytes).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "failed to write immutable gate presentation {}: {error}",
                    path.display()
                ))
            })?;
            file.sync_all().map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "failed to sync immutable gate presentation {}: {error}",
                    path.display()
                ))
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&path).map_err(|read_error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "failed to read immutable gate presentation {}: {read_error}",
                    path.display()
                ))
            })?;
            if existing != bytes || tagged_hash(DomainTag::GatePresentation, &existing) != digest {
                return Err(RuntimeError::InvalidRuntimeState(
                    "immutable gate-presentation digest collision or tampering detected".into(),
                ));
            }
        }
        Err(error) => {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "failed to create immutable gate presentation {}: {error}",
                path.display()
            )))
        }
    }
    Ok(())
}

/// Restore the worker repo's working tree to git HEAD via
/// `git reset --hard HEAD` then `git clean -fd`. Used by the
/// runtime's `restore_repo_worktree_for_event` so partial filesystem
/// mutations from a rejected event don't leak into the next attempt's
/// `before_snapshot`. Free function (not `&self`-bound) so it can be
/// reused without constructing a `SupervisorRuntime`. Bug X principled
/// fix (Phase 1-4) made the prior `RollbackWorkerAttempt` CLI variant
/// dead — the kernel-driven `RestoreWorktreeToActiveWorkerBase` is the
/// only restore path the bridge needs; transport failures are conveyed
/// via `transport_failure=true` Malformed responses and the kernel
/// handles the rest.
/// The per-cycle event log (`.trellis-history/event-log/`) is append-only
/// history: the in-memory `event_count` never rewinds, so a worktree restore
/// that reverted or deleted cycle files would tear a hole in the dense global
/// index (caught fail-loud at the next load, with the torn events
/// unrecoverable). Shield the directory across destructive git commands by
/// renaming it aside and moving it back afterwards, replacing whatever git
/// materialized at the path. The shield lives inside `.trellis-history/` so
/// `git clean -fd -e .trellis-history...` invocations skip it.
fn with_event_log_shielded<F>(repo_path: &Path, f: F) -> Result<(), RuntimeError>
where
    F: FnOnce() -> Result<(), RuntimeError>,
{
    let dir = repo_path.join(".trellis-history").join("event-log");
    if !dir.is_dir() {
        return f();
    }
    let shield = repo_path
        .join(".trellis-history")
        .join("event-log.restore-shield");
    if shield.exists() {
        fs::remove_dir_all(&shield)?;
    }
    fs::rename(&dir, &shield)?;
    let result = f();
    if dir.exists() {
        // Whatever the git restore materialized at the path (e.g. the
        // clean tag's older cycle files) is superseded by the shielded
        // live log.
        let _ = fs::remove_dir_all(&dir);
    }
    if let Err(err) = fs::rename(&shield, &dir) {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "event-log shield restore failed: {err}; the live event log is at \
             {} and MUST be moved back to {} before relaunch",
            shield.display(),
            dir.display()
        )));
    }
    result
}

pub fn restore_worktree_to_head(repo_path: &Path) -> Result<(), RuntimeError> {
    with_event_log_shielded(repo_path, || restore_worktree_to_head_inner(repo_path))
}

/// Full HEAD commit SHA, or `None` on any git error. Used to record the
/// durable LastClean commit pointer (Bug 2) after a clean checkpoint.
/// Process memory (spec §7): restore `process-memory/` (worktree + index)
/// from `commit` after a LastClean reset. No-op when the commit carries no
/// `process-memory/` tree (pre-migration runs); loud error when the
/// checkout itself fails.
fn restore_process_memory_from_commit(repo_path: &Path, commit: &str) -> Result<(), RuntimeError> {
    let probe = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "ls-tree",
            "-d",
            commit,
            "--",
            crate::process_memory::PROCESS_MEMORY_DIR,
        ])
        .output()
        .map_err(RuntimeError::from)?;
    if !probe.status.success() || String::from_utf8_lossy(&probe.stdout).trim().is_empty() {
        return Ok(());
    }
    let checkout = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "checkout",
            commit,
            "--",
            crate::process_memory::PROCESS_MEMORY_DIR,
        ])
        .output()
        .map_err(RuntimeError::from)?;
    if !checkout.status.success() {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "process-memory carry-forward failed for `git checkout {commit} -- {}`: exit code {:?}; stderr={:?}",
            crate::process_memory::PROCESS_MEMORY_DIR,
            checkout.status.code(),
            String::from_utf8_lossy(&checkout.stderr),
        )));
    }
    Ok(())
}

fn git_head_sha(repo_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// `true` iff `commit` resolves and is an ancestor of (or equal to) HEAD.
/// Uses `git merge-base --is-ancestor <commit> HEAD` (exit 0 = ancestor,
/// exit 1 = not, other = error). On any spawn/resolution error we return
/// `false` (treat as "not a safe target") so the Bug-2 selection never
/// rewinds to something it cannot confirm is on the current line.
fn git_is_ancestor_of_head(repo_path: &Path, commit: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Number of commits in `commit..HEAD` (how far `commit` is behind HEAD).
/// `Some(0)` means `commit == HEAD`. Returns `None` on any git error
/// (e.g. unrelated histories, unresolvable ref). On the linear checkpoint
/// history the ancestor with the SMALLEST value is the nearest / greatest-
/// cycle clean checkpoint.
fn git_commits_behind_head(repo_path: &Path, commit: &str) -> Option<u64> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-list", "--count", &format!("{commit}..HEAD")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn restore_worktree_to_head_inner(repo_path: &Path) -> Result<(), RuntimeError> {
    // Anchored so only the repo-root process-memory dir is spared; a stray
    // nested `Tablet/process-memory/` is still swept.
    let pm_exclude = format!("/{}", crate::process_memory::PROCESS_MEMORY_DIR);
    // Same anchoring, same reason, for the Isabelle session scaffold. The
    // checker owns `<repo>/isabelle/` (`isabelle_scaffold.sync_session` is its
    // only writer) and only `ROOT` + `Tablet_Preamble.thy` are tracked; the
    // `base/` session dir and the per-node `Tablet_<N>.thy` projections are
    // UNTRACKED, so an unexcluded repo-root `git clean -fd` deletes exactly
    // them and leaves a half-scaffold behind. Live loss (conn-isa, 2026-08-17
    // 10:21): the auto-rewind on a fingerprint-divergence at load swept
    // `isabelle/base/` and all 55 projections, after which `isa-query` could
    // not run at all ("isabelle/base is missing from this checkout" — reviewer
    // 197) and every payload cache key failed to construct, so each corr
    // fingerprint sweep re-probed all 54 nodes live instead of hitting cache.
    // Nothing regenerates the scaffold on its own; it needs an explicit
    // `isabelle-sync-session`.
    //
    // Lean-inert: a Lean repo has no top-level `isabelle/`, so the pattern
    // never matches and the sweep is byte-identical to before.
    let isabelle_exclude = format!("/{}", crate::backend::ISABELLE_SESSION_DIR);
    for command in [
        vec!["reset", "--hard", "HEAD"],
        vec![
            "clean",
            "-fd",
            "-e",
            ".trellis-history",
            "-e",
            ".trellis-stop-after-checkpoint",
            // Process memory (spec §7): entries materialized at audit
            // acceptance stay untracked until the next cycle-Start
            // checkpoint commits them; `process-memory/` is kernel-owned
            // durable state (like `.trellis-history/`), so the sweep must
            // spare it. Before this exclusion, the rejection-cycle
            // worker-retry restore silently deleted the entry files and
            // INDEX.md while `process_memory_seq` kept its bumped value.
            "-e",
            pm_exclude.as_str(),
            "-e",
            isabelle_exclude.as_str(),
        ],
    ] {
        let start = std::time::Instant::now();
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(&command)
            .output();
        let duration = start.elapsed().as_secs_f64();
        let output = match output {
            Ok(o) => {
                crate::check_ledger::append_kind(
                    repo_path,
                    "git",
                    command[0],
                    duration,
                    o.status.success(),
                    o.stdout.len(),
                    o.stderr.len(),
                );
                o
            }
            Err(err) => {
                crate::check_ledger::append_kind(
                    repo_path, "git", command[0], duration, false, 0, 0,
                );
                return Err(err.into());
            }
        };
        if !output.status.success() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "restore committed worktree failed for `git {}` with exit code {:?}; stdout={:?}; stderr={:?}",
                command.join(" "),
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )));
        }
    }
    // Purge stale Tablet/*.olean (and friends) whose source file was deleted
    // in this rewind. Same reasoning as `restore_repo_worktree_to_last_clean`:
    // .lake/build is gitignored, so git clean misses it; without this, Lean
    // resolves imports for nodes whose sources are gone, polluting downstream
    // probes and worker reasoning with ghost declarations.
    purge_stale_tablet_build_artifacts(repo_path);
    // INDEX.md is usually TRACKED (committed at prior checkpoints), so the
    // `reset --hard` above reverts an uncommitted audit-acceptance rewrite of
    // it even though the entry files themselves survive the clean exclusion.
    // INDEX.md is derived state; regenerate it from the surviving entry files
    // (no-op for repos without a process-memory dir).
    crate::process_memory::regenerate_index(repo_path).map_err(|err| {
        RuntimeError::InvalidRuntimeState(format!(
            "process-memory index regeneration after worktree restore failed: {err}"
        ))
    })?;
    Ok(())
}

/// Delete stale Lake build artifacts for Tablet nodes whose
/// `Tablet/<stem>.lean` source file is no longer present after a worktree
/// rewind or cone-clean prune. Two artifact classes are purged, in both
/// `.lake/build/lib/lean/Tablet/` and `.lake/build/ir/Tablet/`:
///
///   1. The deleted modules' OWN artifacts (`<stem>.{olean,ilean,olean.hash,
///      ilean.hash,c,c.hash,ll,trace,setup.json,...}`). The artifacts are
///      gitignored so neither `git reset --hard` nor `git clean -fd` touches
///      them; without this purge, Lean's import resolver happily finds the
///      orphaned olean and consumers (probes, workers, reviewers) end up
///      reasoning about declarations whose source no longer exists.
///
///   2. DEPENDENT modules' artifacts — any surviving module whose cached
///      Lake import graph (`.lake/build/ir/Tablet/<m>.setup.json`) still
///      references a deleted `Tablet.<stem>` module. Lake trusts the cached
///      setup rather than re-resolving imports from the (unchanged) source,
///      so `lake build` hard-fails with "object file '.../<stem>.olean' of
///      module Tablet.<stem> does not exist" even when no current source
///      imports the deleted node (observed: unitdistance cycle 694, cone
///      clean of `BigonCorridorSideCopy`). Dropping the dependents'
///      artifacts is safe — their source survives and rebuilds cleanly.
///
/// Best-effort: ignores I/O errors (any individual deletion failure is
/// surfaced via stderr but does not abort the rewind). The set of "live"
/// stems is derived from the current on-disk Tablet/*.lean listing, so
/// multiple deletions in one burst are handled uniformly. Only files
/// directly under the repo's own `.lake/build/{lib/lean,ir}/Tablet/` are
/// ever removed; source files are never touched.
fn purge_stale_tablet_build_artifacts(repo_path: &Path) {
    purge_invalidated_tablet_build_artifacts(repo_path, &std::collections::BTreeSet::new());
}

/// Materialize the engine's cleanup-cycle deletion command. All pairs are
/// validated before the first unlink so a missing, non-regular, or path-like
/// node fails loudly without partially applying an otherwise valid batch.
fn delete_cleanup_unreachable_node_pairs(
    repo_path: &Path,
    deletion: &CleanupUnreachableDeletionRecord,
) -> Result<(), RuntimeError> {
    if deletion.deleted_nodes.contains(&NodeId::from("Preamble")) {
        return Err(RuntimeError::InvalidRuntimeState(
            "cleanup unreachable-node deletion command includes Preamble".into(),
        ));
    }
    if deletion.source_targets.keys().cloned().collect::<BTreeSet<_>>()
        != deletion.deleted_nodes
    {
        return Err(RuntimeError::InvalidRuntimeState(
            "cleanup unreachable-node deletion source-target keys do not match deleted nodes"
                .into(),
        ));
    }

    let mut pairs = Vec::new();
    for node in &deletion.deleted_nodes {
        let raw = node.as_str();
        let mut components = Path::new(raw).components();
        let safe_single_component = matches!(
            (components.next(), components.next()),
            (Some(std::path::Component::Normal(name)), None) if name == raw
        );
        if raw.is_empty() || !safe_single_component {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "cleanup unreachable-node deletion has unsafe node id `{raw}`"
            )));
        }
        let target = deletion.source_targets.get(node).copied().ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(format!(
                "cleanup unreachable-node deletion lacks source target for `{node}`"
            ))
        })?;
        let source = crate::worker_normalization::node_source_path(repo_path, raw, target);
        let tex = repo_path.join("Tablet").join(format!("{raw}.tex"));
        for path in [&source, &tex] {
            let metadata = fs::symlink_metadata(path).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "cleanup unreachable-node deletion cannot inspect {}: {error}",
                    path.display()
                ))
            })?;
            if !metadata.file_type().is_file() {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "cleanup unreachable-node deletion refuses non-regular file {}",
                    path.display()
                )));
            }
        }
        pairs.push((source, tex));
    }

    for (source, tex) in pairs {
        fs::remove_file(&source).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "cleanup unreachable-node deletion failed to remove {}: {error}",
                source.display()
            ))
        })?;
        fs::remove_file(&tex).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "cleanup unreachable-node deletion failed to remove {}: {error}",
                tex.display()
            ))
        })?;
    }
    let invalidated: BTreeSet<String> = deletion
        .deleted_nodes
        .iter()
        .map(|node| node.as_str().to_string())
        .collect();
    purge_invalidated_tablet_build_artifacts(repo_path, &invalidated);
    Ok(())
}

/// Generalized entry point shared by the deletion trigger
/// (`purge_stale_tablet_build_artifacts`, invalidated set empty) and the
/// edit trigger (worker-burst acceptance in `bin/runtime_cli.rs`, which
/// passes the stems of every Tablet `.lean` file the burst modified or
/// added). A stem's artifacts are invalidated when its source is missing
/// OR it appears in `invalidated_stems`; dependents whose cached
/// `setup.json` import graph references any invalidated stem are swept in
/// the same pass.
///
/// The edit trigger exists because a bare probe (`lake env lean`, which
/// never builds) resolves imports through whatever olean is on disk: after
/// an accepted signature-changing edit, the pre-edit olean is a "phantom"
/// that shows the OLD signature (observed: unitdistance cycle 696,
/// reviewer scratch probe of `EndpointSidePrefixConstruction` displayed
/// the pre-edit signature an hour after the accepted edit). Deleting the
/// artifacts converts that silent phantom into either a fresh rebuild
/// (every support-required dispatch and the acceptance hydrate phase run
/// `materialize-tablet-oleans` = `lake build`, whose caches gate on olean
/// PRESENCE and therefore force a real dispatch once the files are gone)
/// or an unambiguous missing-olean error for a probe that races ahead of
/// the rebuild — strictly better than reasoning about dead declarations.
///
/// Idempotent: deleting already-missing files is a tolerated no-op, so a
/// delete+edit in one burst (acceptance purge, then a later rewind-path
/// purge over the same stems) never errors.
pub fn purge_invalidated_tablet_build_artifacts(
    repo_path: &Path,
    invalidated_stems: &std::collections::BTreeSet<String>,
) {
    use std::collections::BTreeSet;
    let tablet_dir = repo_path.join("Tablet");
    let lib_dir = repo_path.join(".lake/build/lib/lean/Tablet");
    let ir_dir = repo_path.join(".lake/build/ir/Tablet");
    if !lib_dir.is_dir() && !ir_dir.is_dir() {
        return;
    }
    let live_stems: BTreeSet<String> = match std::fs::read_dir(&tablet_dir) {
        Ok(iter) => iter
            .filter_map(|e| e.ok())
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("lean") {
                    path.file_stem().and_then(|s| s.to_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => return,
    };
    let directly_invalidated =
        |stem: &str| !live_stems.contains(stem) || invalidated_stems.contains(stem);
    // Identify surviving dependents whose cached import graph references an
    // invalidated module BEFORE purging, so the scan sees every setup.json.
    let stale_dependents = tablet_dependents_of_invalidated_modules(&ir_dir, &directly_invalidated);
    let mut purged = 0usize;
    for dir in [&lib_dir, &ir_dir] {
        purged += purge_tablet_artifact_dir_entries(dir, |stem| {
            directly_invalidated(stem) || stale_dependents.contains(stem)
        });
    }
    if purged > 0 {
        eprintln!(
            "trellis: purged {purged} stale .lake/build/{{lib/lean,ir}}/Tablet/ \
             entr{plural} for deleted/edited-source nodes and their cached-import \
             dependents (post-rewind/cone-clean/acceptance cleanup).",
            plural = if purged == 1 { "y" } else { "ies" }
        );
    }
}

/// Delete every regular file directly under `dir` whose leading stem (text
/// before the first '.' — handles .olean, .olean.hash, .ilean, .ilean.hash,
/// .c, .c.hash, .ll, .trace, .setup.json) satisfies `is_stale`. Returns the
/// number of files removed. Missing directory or unreadable entries are
/// skipped; individual deletion failures are surfaced via stderr but never
/// abort the purge (missing files are fine — another pass may already have
/// removed them).
fn purge_tablet_artifact_dir_entries(dir: &Path, is_stale: impl Fn(&str) -> bool) -> usize {
    let entries = match std::fs::read_dir(dir) {
        Ok(iter) => iter,
        Err(_) => return 0,
    };
    let mut purged = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let stem = match name.split('.').next() {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        if !is_stale(stem) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => purged += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!(
                    "trellis: failed to purge stale Tablet build artifact {}: {err}",
                    path.display()
                );
            }
        }
    }
    purged
}

/// Scan `<ir_dir>/*.setup.json` for surviving Tablet modules whose cached
/// Lake import graph still references an invalidated `Tablet.<stem>`
/// module (source deleted, or content edited this burst), returning their
/// stems. Matching is exact-module-name — a JSON object key or string
/// value equal to `Tablet.<stem>` with exactly one segment after
/// `Tablet.` — never substring, so an invalidated `Tablet.Foo` does not
/// flag a dependent that only imports `Tablet.FooBar`. Walking keys AND
/// string values keeps the check robust across Lake setup-file schema
/// variations (`importArts` keys today; plain import-name arrays in other
/// versions). An unparseable setup.json is conservatively treated as
/// stale — invalidating it only costs a rebuild from source, whereas
/// trusting it risks the hard `lake build` failure this purge exists to
/// prevent.
fn tablet_dependents_of_invalidated_modules(
    ir_dir: &Path,
    is_invalidated: &dyn Fn(&str) -> bool,
) -> std::collections::BTreeSet<String> {
    let mut stale = std::collections::BTreeSet::new();
    let entries = match std::fs::read_dir(ir_dir) {
        Ok(iter) => iter,
        Err(_) => return stale,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        let stem = match name.strip_suffix(".setup.json") {
            Some(s) if !s.is_empty() && !s.contains('.') => s,
            _ => continue,
        };
        if is_invalidated(stem) {
            // The invalidated module's own setup.json; the direct pass
            // already removes it, and its references are moot.
            continue;
        }
        let references_deleted = match std::fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|text| {
                serde_json::from_str::<serde_json::Value>(&text).map_err(|err| err.to_string())
            }) {
            Ok(value) => json_references_invalidated_tablet_module(&value, is_invalidated),
            Err(err) => {
                eprintln!(
                    "trellis: unreadable Lake setup file {} ({err}); \
                     conservatively invalidating module {stem}'s build artifacts.",
                    path.display()
                );
                true
            }
        };
        if references_deleted {
            stale.insert(stem.to_string());
        }
    }
    stale
}

/// True iff any JSON object key or string value anywhere in `value` is an
/// exact `Tablet.<stem>` module name whose `<stem>` satisfies
/// `is_invalidated` (deleted source or edited-this-burst).
fn json_references_invalidated_tablet_module(
    value: &serde_json::Value,
    is_invalidated: &dyn Fn(&str) -> bool,
) -> bool {
    match value {
        serde_json::Value::String(s) => is_invalidated_tablet_module_name(s, is_invalidated),
        serde_json::Value::Array(items) => items
            .iter()
            .any(|item| json_references_invalidated_tablet_module(item, is_invalidated)),
        serde_json::Value::Object(map) => map.iter().any(|(key, item)| {
            is_invalidated_tablet_module_name(key, is_invalidated)
                || json_references_invalidated_tablet_module(item, is_invalidated)
        }),
        _ => false,
    }
}

/// True iff `candidate` is exactly `Tablet.<stem>` (one segment, non-empty)
/// with `<stem>` satisfying `is_invalidated`. Tablet node modules are flat,
/// so multi-segment names (`Tablet.Foo.Bar`) are never node references.
fn is_invalidated_tablet_module_name(
    candidate: &str,
    is_invalidated: &dyn Fn(&str) -> bool,
) -> bool {
    match candidate.strip_prefix("Tablet.") {
        Some(stem) if !stem.is_empty() && !stem.contains('.') => is_invalidated(stem),
        _ => false,
    }
}

/// Patch C-Q Q5 — canonical filesystem path for a persisted local-closure
/// record under `<runtime_root>/checker-state/local-closure-records/`.
/// Escapes `/` in node IDs to `_` so deletion and persistence stay in
/// lockstep (the persistence path in `bin/runtime_cli.rs` does the same
/// substitution, and the audit flagged the mismatch as a future-proofing
/// risk even though current `NodeId`s don't contain `/`). Centralizing
/// the construction here means any future filename-mapping change has
/// exactly one site to update.
pub fn persisted_record_path(runtime_root: &Path, node: &NodeId) -> PathBuf {
    let safe_name = node.as_str().replace('/', "_");
    runtime_root
        .join("checker-state")
        .join("local-closure-records")
        .join(format!("{}.json", safe_name))
}

/// Patch C-Q Q5 — filename component (without parent directory) for a
/// persisted local-closure record. Used by `persist_record_to_disk` in
/// `bin/runtime_cli.rs`, which already owns the `records_dir`. Keeps
/// the same escape logic as `persisted_record_path`.
pub fn persisted_record_file_name(node: &NodeId) -> String {
    let safe_name = node.as_str().replace('/', "_");
    format!("{}.json", safe_name)
}

/// Durable handoff from the stopped-runtime migrator to the next event-log
/// writer. Presence means the next successfully appended event must carry a
/// complete local-closure record snapshot so replay has an exact boundary for
/// the offline migration result. It authorizes no probing or issuance.
pub fn local_closure_replay_snapshot_pending_path(runtime_root: &Path) -> PathBuf {
    runtime_root
        .join("checker-state")
        .join("local-closure-replay-snapshot.pending")
}

/// Patch C-O HIGH 1 (c) — remove the persisted local-closure record
/// file at `<runtime_root>/checker-state/local-closure-records/<node>.json`.
/// Called by the runtime when the engine emits
/// `ProtocolCommand::DeleteLocalClosureRecord`. Missing-file is not an
/// error (no probe has persisted a record yet for that node). Other
/// I/O failures are logged to stderr — the engine's in-memory tombstone
/// (Patch C-O HIGH 1 (a)) is the load-bearing guard; the disk delete is
/// hygiene to avoid stale files accumulating.
///
/// Patch C-Q Q5 — uses `persisted_record_path` so the filename escape
/// matches the persistence side.
///
/// Audit L-1 — surfaced as `pub` so integration tests
/// (`kernel/tests/local_closure_disk_durability.rs`) can pin the
/// per-file delete primitive that the L-1 flush loop in
/// `step_with_checkpoint_sink` iterates. The internal callers are still
/// the only paths that DRIVE the delete (engine emits a command, the
/// runtime processes it); test-side direct calls verify the primitive's
/// idempotency contract.
pub fn delete_persisted_local_closure_record(runtime_root: &Path, node: &NodeId) {
    let file = persisted_record_path(runtime_root, node);
    match fs::remove_file(&file) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            eprintln!(
                "[local-closure delete] failed to remove {}: {err}",
                file.display()
            );
        }
    }
}

/// Archive the repo-root `HUMAN_INPUT.md` under
/// `.trellis-history/human-input/cycle-NNNNNN.md` and truncate the root
/// file. Called from `step_with_checkpoint_sink` AFTER the checkpoint
/// durability barrier, when a reviewer response with `clear_human_input =
/// true` was accepted (pre-step `human_input_outstanding` held and the
/// post-transition state cleared it — the outstanding operator input was
/// consumed), so retracted operator prose does not linger in the repo
/// root. The repo mutation lands in the NEXT checkpoint commit, like the
/// burst-history ledger append. Missing/empty root file → nothing to
/// archive (truncation of an absent file is skipped). A second clear
/// stamped with the same cycle appends to the existing archive file.
/// Best-effort throughout: failures are logged, never propagated — this
/// is hygiene, not state.
pub fn archive_and_truncate_human_input(repo_path: &Path, cycle: u32) {
    let root_file = repo_path.join("HUMAN_INPUT.md");
    let content = match fs::read_to_string(&root_file) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            eprintln!(
                "[human-input archive] failed to read {}: {err}",
                root_file.display()
            );
            return;
        }
    };
    if !content.trim().is_empty() {
        let archive_dir = repo_path.join(".trellis-history").join("human-input");
        let archive_file = archive_dir.join(format!("cycle-{cycle:06}.md"));
        let archived = fs::create_dir_all(&archive_dir)
            .and_then(|()| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&archive_file)
            })
            .and_then(|mut file| file.write_all(content.as_bytes()));
        if let Err(err) = archived {
            eprintln!(
                "[human-input archive] failed to archive {} to {}: {err}",
                root_file.display(),
                archive_file.display()
            );
            // Leave the root file intact rather than destroy unarchived
            // operator prose.
            return;
        }
    }
    if let Err(err) = fs::write(&root_file, "") {
        eprintln!(
            "[human-input archive] failed to truncate {}: {err}",
            root_file.display()
        );
    }
}

/// Spool directory for the checker's asynchronous heartbeat measurements:
/// `<runtime_root>/checker-state/heartbeat-measurements/`. The checker
/// server's background measurer writes one `<node>.json` per completed
/// measurement (atomic rename, last-wins per node); the runtime drains the
/// directory onto `WorkerResponse.late_heartbeats` at the next worker step.
pub fn heartbeat_measurements_dir(runtime_root: &Path) -> PathBuf {
    runtime_root
        .join("checker-state")
        .join("heartbeat-measurements")
}

/// Upper bound on spool files consumed (and removed) per drain. Purely a
/// safety valve: the writer keys files by node so the population is bounded
/// by tablet size in ordinary operation.
const HEARTBEAT_SPOOL_DRAIN_MAX_FILES: usize = 1024;

/// Mirror of the checker-side absurdity ceiling
/// (`observations._HEARTBEATS_ABSURD_MAX`): counts above this are corrupt
/// output, not big builds.
const HEARTBEAT_ABSURD_MAX: u64 = 1_000_000_000_000;

/// Drain the checker's heartbeat-measurement spool, fail-open.
///
/// Every consumed file is REMOVED, parsed or not: a malformed file is
/// dropped silently (instrumentation must never wedge the step), and a
/// well-formed one must not be re-delivered on the next step. Any I/O
/// failure — missing directory (the ordinary pre-feature state), unreadable
/// file, garbage JSON, absurd count — degrades to fewer measurements and
/// nothing else. Called on the live step path only; replay never consults
/// the spool because the drained data rides the logged response.
///
/// File schema (extra keys ignored):
/// `{"node": "<NodeId>", "heartbeats": <u64>, "heartbeats_key": "<hash>"}`
pub fn drain_pending_heartbeat_measurements(
    runtime_root: &Path,
) -> BTreeMap<NodeId, crate::model::LateHeartbeatMeasurement> {
    let mut out = BTreeMap::new();
    let dir = heartbeat_measurements_dir(runtime_root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths.truncate(HEARTBEAT_SPOOL_DRAIN_MAX_FILES);
    for path in paths {
        let parsed = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        // Consume the file regardless of parse outcome so garbage cannot
        // accumulate or be re-scanned forever.
        let _ = fs::remove_file(&path);
        let Some(value) = parsed else { continue };
        let Some(node) = value.get("node").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(heartbeats) = value.get("heartbeats").and_then(|v| v.as_u64()) else {
            continue;
        };
        let Some(key) = value.get("heartbeats_key").and_then(|v| v.as_str()) else {
            continue;
        };
        if node.is_empty() || key.is_empty() || heartbeats > HEARTBEAT_ABSURD_MAX {
            continue;
        }
        out.insert(
            NodeId::from(node),
            crate::model::LateHeartbeatMeasurement {
                heartbeats,
                heartbeats_key: key.to_string(),
            },
        );
    }
    out
}

fn certificate_workspace_for_repo(repo_path: &Path) -> PathBuf {
    let supervisor = repo_path.join(".trellis/supervisor/repo");
    if supervisor.join("Tablet").is_dir() {
        supervisor
    } else {
        repo_path.to_path_buf()
    }
}

fn certificate_artifact_root(certificate_workspace: &Path) -> PathBuf {
    certificate_workspace.join(".lake/build/lib/lean/Tablet")
}

fn safe_artifact_epoch_join(root: &Path, relative: &str) -> Result<PathBuf, RuntimeError> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "certificate artifact epoch contains unsafe relative path {relative:?}"
        )));
    }
    Ok(root.join(path))
}

fn capture_certificate_artifact_epoch_content(
    artifact_root: &Path,
) -> Result<CertificateArtifactEpochContent, RuntimeError> {
    fn collect(
        root: &Path,
        current: &Path,
        entries: &mut Vec<CertificateArtifactEpochEntry>,
    ) -> Result<(), RuntimeError> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                collect(root, &path, entries)?;
            } else if file_type.is_file() {
                let relative = path.strip_prefix(root).map_err(|_| {
                    RuntimeError::InvalidRuntimeState(format!(
                        "artifact path {} escaped root {}",
                        path.display(),
                        root.display()
                    ))
                })?;
                let relative = relative.to_str().ok_or_else(|| {
                    RuntimeError::InvalidRuntimeState(format!(
                        "certificate artifact path {} is not UTF-8",
                        relative.display()
                    ))
                })?;
                safe_artifact_epoch_join(Path::new("."), relative)?;
                let bytes = fs::read(&path)?;
                entries.push(CertificateArtifactEpochEntry {
                    relative_path: relative.to_string(),
                    sha256: raw_sha256(&bytes),
                    size_bytes: bytes.len() as u64,
                });
            } else {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "certificate artifact tree contains non-regular entry {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    match fs::symlink_metadata(artifact_root) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            let mut entries = Vec::new();
            collect(artifact_root, artifact_root, &mut entries)?;
            entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
            Ok(CertificateArtifactEpochContent {
                artifact_root_present: true,
                entries,
            })
        }
        Ok(_) => Err(RuntimeError::InvalidRuntimeState(format!(
            "certificate artifact root {} must be a directory when present",
            artifact_root.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(CertificateArtifactEpochContent {
                artifact_root_present: false,
                entries: Vec::new(),
            })
        }
        Err(error) => Err(RuntimeError::Io(error)),
    }
}

fn certificate_artifact_relative_path(node: &NodeId, level: &str) -> Result<String, RuntimeError> {
    let node_path = Path::new(node.as_str());
    if node.as_str().is_empty()
        || node_path.is_absolute()
        || node_path.components().count() != 1
        || node_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "node {} cannot name a certificate artifact file",
            node.as_str()
        )));
    }
    let suffix = match level {
        "exported" => ".olean",
        "server" => ".olean.server",
        "private" => ".olean.private",
        other => {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "node {} certificate has unsupported artifact level {other:?}",
                node.as_str()
            )));
        }
    };
    Ok(format!("{}{suffix}", node.as_str()))
}

fn validate_certificate_artifact_epoch_against_state(
    state: &ProtocolState,
    content: &CertificateArtifactEpochContent,
) -> Result<(), RuntimeError> {
    if !content.artifact_root_present && !content.entries.is_empty() {
        return Err(RuntimeError::InvalidRuntimeState(
            "absent certificate artifact root has non-empty epoch entries".into(),
        ));
    }
    let mut entries = BTreeMap::new();
    let mut previous = None;
    for entry in &content.entries {
        safe_artifact_epoch_join(Path::new("."), &entry.relative_path)?;
        if previous
            .as_ref()
            .is_some_and(|path: &String| path >= &entry.relative_path)
        {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "certificate artifact epoch entries are not strictly path-sorted at {:?}",
                entry.relative_path
            )));
        }
        previous = Some(entry.relative_path.clone());
        if entries.insert(entry.relative_path.clone(), entry).is_some() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "certificate artifact epoch repeats path {:?}",
                entry.relative_path
            )));
        }
    }

    for (node, record) in &state.local_closure_records {
        let Some(certificate) = record.node_certificate.as_ref() else {
            continue;
        };
        if certificate.artifact_bundle.is_empty() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "node {node} certificate has an empty artifact bundle"
            )));
        }
        let mut expected_levels = BTreeSet::new();
        for part in &certificate.artifact_bundle {
            if !expected_levels.insert(part.level.as_str()) {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "node {node} certificate repeats artifact level {:?}",
                    part.level
                )));
            }
            let relative = certificate_artifact_relative_path(node, &part.level)?;
            let epoch_entry = entries.get(&relative).ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(format!(
                    "certificate artifact epoch is missing {relative} for node {node}"
                ))
            })?;
            if epoch_entry.sha256 != part.sha256 || epoch_entry.size_bytes != part.size_bytes {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "certificate artifact epoch entry {relative} disagrees with node {node} certificate: epoch=({}, {}), certificate=({}, {})",
                    epoch_entry.sha256,
                    epoch_entry.size_bytes,
                    part.sha256,
                    part.size_bytes,
                )));
            }
        }
        for level in ["exported", "server", "private"] {
            let relative = certificate_artifact_relative_path(node, level)?;
            if entries.contains_key(&relative) != expected_levels.contains(level) {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "certificate artifact epoch has a visibility-shape mismatch for node {node}: level={level}"
                )));
            }
        }
    }
    Ok(())
}

fn read_regular_file_without_symlinks(
    path: &Path,
    description: &str,
) -> Result<Vec<u8>, RuntimeError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "{description} {} is unavailable: {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "{description} {} is not a regular file",
            path.display()
        )));
    }
    fs::read(path).map_err(RuntimeError::Io)
}

fn validate_immutable_artifact_file(
    path: &Path,
    expected_sha256: crate::trust_base::Sha256Digest,
    expected_size: u64,
    description: &str,
) -> Result<(), RuntimeError> {
    let bytes = read_regular_file_without_symlinks(path, description)?;
    let actual_sha256 = raw_sha256(&bytes);
    if bytes.len() as u64 != expected_size || actual_sha256 != expected_sha256 {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "{description} {} failed content-address validation: actual=({}, {}), expected=({}, {})",
            path.display(),
            actual_sha256,
            bytes.len(),
            expected_sha256,
            expected_size,
        )));
    }
    Ok(())
}

fn publish_immutable_artifact_file(
    destination: &Path,
    bytes: &[u8],
    expected_sha256: crate::trust_base::Sha256Digest,
    expected_size: u64,
) -> Result<(), RuntimeError> {
    if bytes.len() as u64 != expected_size || raw_sha256(bytes) != expected_sha256 {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "refusing to publish artifact {} under an incorrect content address",
            destination.display()
        )));
    }
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return validate_immutable_artifact_file(
                destination,
                expected_sha256,
                expected_size,
                "existing immutable artifact",
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(RuntimeError::Io(error)),
    }
    let parent = destination.parent().ok_or_else(|| {
        RuntimeError::InvalidRuntimeState(format!(
            "immutable artifact destination {} has no parent",
            destination.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    let temp = destination.with_file_name(format!(
        ".{}.tmp-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact"),
        std::process::id()
    ));
    remove_path_without_following_symlinks(&temp)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    set_file_read_only(&temp)?;
    match fs::rename(&temp, destination) {
        Ok(()) => Ok(()),
        Err(_error)
            if fs::symlink_metadata(destination)
                .map(|_| true)
                .unwrap_or(false) =>
        {
            remove_path_without_following_symlinks(&temp)?;
            validate_immutable_artifact_file(
                destination,
                expected_sha256,
                expected_size,
                "concurrently published immutable artifact",
            )
        }
        Err(error) => Err(RuntimeError::Io(error)),
    }
}

fn set_file_read_only(path: &Path) -> Result<(), RuntimeError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o444);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn materialize_immutable_artifact(source: &Path, destination: &Path) -> Result<(), RuntimeError> {
    use std::os::fd::AsRawFd;

    let source_file = fs::File::open(source)?;
    let destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let reflink_result = unsafe {
        libc::ioctl(
            destination_file.as_raw_fd(),
            FICLONE,
            source_file.as_raw_fd(),
        )
    };
    if reflink_result == 0 {
        drop(destination_file);
        set_file_read_only(destination)?;
        return Ok(());
    }
    let reflink_error = std::io::Error::last_os_error();
    drop(destination_file);
    remove_path_without_following_symlinks(destination)?;
    fs::hard_link(source, destination).map_err(|hardlink_error| {
        RuntimeError::InvalidRuntimeState(format!(
            "cannot materialize immutable certificate artifact {} at {} by reflink ({reflink_error}) or hardlink ({hardlink_error}); runtime and supervisor workspace must share a reflink- or hardlink-capable filesystem",
            source.display(),
            destination.display(),
        ))
    })?;
    Ok(())
}

fn worker_surface_directory_present(path: &Path) -> Result<bool, RuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(RuntimeError::InvalidRuntimeState(format!(
            "worker-writable semantic surface {} must be a directory when present",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(RuntimeError::Io(error)),
    }
}

fn remove_path_without_following_symlinks(path: &Path) -> Result<(), RuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(RuntimeError::Io(error)),
    }
    Ok(())
}

/// Restore one worker-writable semantic source tree without touching any
/// sibling path. `expected_present` records absence as well as presence, so a
/// worker-created tree cannot survive a rollback merely because no baseline
/// directory exists to copy over it.
fn restore_worker_surface(
    snapshot: &Path,
    destination: &Path,
    expected_present: bool,
) -> Result<(), RuntimeError> {
    remove_path_without_following_symlinks(destination)?;
    if expected_present {
        copy_dir_recursive(snapshot, destination)?;
    }
    Ok(())
}

fn validate_worker_surface_snapshot(
    snapshot: &Path,
    expected_present: bool,
    surface_name: &str,
) -> Result<(), RuntimeError> {
    let snapshot_present = worker_surface_directory_present(snapshot)?;
    if snapshot_present != expected_present {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "active worker base surface `{surface_name}` disagrees with its manifest: \
             manifest present={expected_present}, snapshot directory present={snapshot_present}"
        )));
    }
    if expected_present {
        validate_worker_surface_tree(snapshot)?;
    }
    Ok(())
}

fn validate_worker_surface_tree(root: &Path) -> Result<(), RuntimeError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            validate_worker_surface_tree(&path)?;
        } else if !file_type.is_file() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "refusing to copy non-regular worker-surface entry {} \
                 (symlinks, FIFOs, sockets, and device nodes are forbidden)",
                path.display()
            )));
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), RuntimeError> {
    fs::create_dir_all(dst)?;
    // Normalize directory mode to group-writable (0o2775 keeps the setgid bit
    // so children inherit the parent group). The rollback path
    // (`restore_repo_worktree_to_active_worker_base`) writes into the worker
    // repo's `Tablet/` as the supervisor user; the next worker
    // burst runs inside a bwrap as the burst user, which is in the
    // supervisor's group. Without this, dirs inherit the supervisor's umask (0o775
    // typically), which is fine, but we re-assert it explicitly so the
    // invariant is local to this helper rather than scattered across
    // shell-level umask + bwrap config.
    set_dir_mode_group_writable(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src_path, &dst_path)?;
            // 2026-04-28 fix: `fs::copy` preserves the source's mode bits.
            // When an agent writes Tablet files via tools that default to
            // 0o600 (e.g. `tempfile.mkstemp` followed by atomic rename, or
            // codex's internal write path), those 0o600 modes get captured
            // into `active_worker_base/Tablet/` and then restored back to
            // the worker repo on the rollback path. The next worker burst
            // — running as the burst user, in the supervisor's group
            // — cannot read or modify a file owned by the supervisor user
            // with mode 0o600 (no group access). The dir-level lock is
            // group-writable so the worker can `rm` and re-create the file,
            // but that wastes a retry cycle on a self-inflicted permission
            // detour and surfaces as a transport_failure on the deterministic
            // checker (`sync_tablet_support` writing `Tablet/README.md`).
            // Normalize to 0o664 so any group member can read/write
            // restored content.
            set_file_mode_group_writable(&dst_path, &src_path)?;
        } else {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "refusing to copy non-regular worker-surface entry {} \
                 (symlinks, FIFOs, sockets, and device nodes are forbidden)",
                src_path.display()
            )));
        }
    }
    Ok(())
}

fn set_dir_mode_group_writable(path: &Path) -> Result<(), RuntimeError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    // 0o2775 = setgid + rwx for owner & group, rx for other. Setgid keeps
    // newly-created children in the parent's group (Tablet/ is owned by
    // the supervisor's group on the live runtime, dir mode 2775).
    perms.set_mode(0o2775);
    fs::set_permissions(path, perms)?;
    Ok(())
}

fn set_file_mode_group_writable(dst: &Path, src: &Path) -> Result<(), RuntimeError> {
    use std::os::unix::fs::PermissionsExt;
    // Preserve the executable bit if the source had it (Tablet/ files are
    // never executable, but this helper is shared by all `copy_dir_recursive`
    // callers, including ones that may handle scripts in the future). Apply
    // 0o664 base + 0o111 mask if any execute bit was set on the source.
    let src_mode = fs::metadata(src)?.permissions().mode() & 0o777;
    let any_exec = src_mode & 0o111 != 0;
    let target_mode = if any_exec { 0o775 } else { 0o664 };
    let mut perms = fs::metadata(dst)?.permissions();
    perms.set_mode(target_mode);
    fs::set_permissions(dst, perms)?;
    Ok(())
}

fn request_kind_key(kind: crate::model::RequestKind) -> &'static str {
    match kind {
        crate::model::RequestKind::Worker => "worker",
        crate::model::RequestKind::Paper => "paper",
        crate::model::RequestKind::Corr => "corr",
        crate::model::RequestKind::Sound => "sound",
        crate::model::RequestKind::Review => "review",
        crate::model::RequestKind::HumanGate => "human_gate",
        crate::model::RequestKind::Audit => "audit",
        crate::model::RequestKind::StuckMathAudit => "stuck_math_audit",
    }
}

fn phase_key(phase: Phase) -> &'static str {
    match phase {
        Phase::TheoremStating => "theorem_stating",
        Phase::RevisionStating => "revision_stating",
        Phase::ProofFormalization => "proof_formalization",
        Phase::Cleanup => "cleanup",
        Phase::Complete => "complete",
    }
}

fn request_history_key(kind: crate::model::RequestKind, phase: Phase) -> String {
    match kind {
        crate::model::RequestKind::Worker | crate::model::RequestKind::Review => {
            format!("{}:{}", request_kind_key(kind), phase_key(phase))
        }
        _ => request_kind_key(kind).to_string(),
    }
}

/// Derive `event_count` by summing non-blank lines across the per-cycle
/// event-log files, with a fail-loud reconciliation: the highest record's
/// `index` must equal `sum - 1` (dense 0..N-1 global index). A mismatch means
/// a gap, a duplicate, or a non-contiguous cycle file — refuse to start so a
/// downstream resume can't compute the wrong event_count.
fn read_event_count(dir: &Path) -> Result<u64, RuntimeError> {
    let files = event_log_cycle_files(dir)?;
    if files.is_empty() {
        return Ok(0);
    }
    let mut sum: u64 = 0;
    let mut max_index: Option<u64> = None;
    for path in &files {
        let text = fs::read_to_string(path)?;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            sum += 1;
            // The highest file (last in lexical order) carries the highest
            // index, but parse every record's index so a gap anywhere is
            // caught by the max-vs-sum reconciliation below.
            if let Ok(record) = serde_json::from_str::<EventLogRecord>(line) {
                max_index = Some(max_index.map_or(record.index, |m| m.max(record.index)));
            }
        }
    }
    if sum == 0 {
        return Ok(0);
    }
    match max_index {
        Some(max_index) if max_index + 1 == sum => Ok(sum),
        Some(max_index) => Err(RuntimeError::InvalidRuntimeState(format!(
            "event-log index density violated in {}: highest record index={max_index} but \
             {sum} non-blank records present (expected {} for a dense 0..N-1 index). A gap, \
             duplicate, or non-contiguous cycle file is present; refusing to start.",
            dir.display(),
            max_index + 1
        ))),
        None => Err(RuntimeError::InvalidRuntimeState(format!(
            "event-log directory {} has {sum} non-blank line(s) but no parseable record to \
             reconcile the global index against; refusing to start.",
            dir.display()
        ))),
    }
}

fn serialized_sha256<T: Serialize>(value: &T) -> Result<String, RuntimeError> {
    Ok(raw_sha256(&serde_json::to_vec(value)?).to_string())
}

fn git_ref_targets_head(repo: &Path, reference: &str) -> Result<bool, RuntimeError> {
    let Some(head) = git_stdout(repo, &["rev-parse", "HEAD^{commit}"])? else {
        return Ok(false);
    };
    let Some(target) = git_stdout(repo, &["rev-parse", &format!("{reference}^{{commit}}")])? else {
        return Ok(false);
    };
    Ok(String::from_utf8_lossy(&head).trim() == String::from_utf8_lossy(&target).trim())
}

fn git_update_tag_to_head(repo: &Path, tag: &str) -> Result<(), RuntimeError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["tag", "-f", tag, "HEAD"])
        .output()?;
    if !output.status.success() {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "failed to bind recovery tag {tag} to HEAD: exit={:?}; stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn event_log_record_at_index(
    dir: &Path,
    index: u64,
) -> Result<Option<EventLogRecord>, RuntimeError> {
    for path in event_log_cycle_files(dir)? {
        for line in fs::read_to_string(&path)?.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let record: EventLogRecord = serde_json::from_str(line).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "cannot verify checkpoint recovery against event log {}: {error}",
                    path.display()
                ))
            })?;
            if record.index == index {
                return Ok(Some(record));
            }
        }
    }
    Ok(None)
}

/// A process may stop while serde is appending the journaled event.  The
/// state files are atomically replaced, but JSONL necessarily appends to an
/// existing cycle file.  Accept only an authenticated byte prefix of the
/// exact journaled record at the dense tail, remove that prefix, and let the
/// recovery path append the complete record below.
fn repair_partial_checkpoint_event_tail(
    dir: &Path,
    expected_record: &EventLogRecord,
) -> Result<(), RuntimeError> {
    let target = event_log_cycle_file(dir, expected_record.cycle);
    if !target.is_file() {
        return Ok(());
    }
    let target_bytes = fs::read(&target)?;
    if target_bytes.is_empty() || target_bytes.ends_with(b"\n") {
        return Ok(());
    }
    let files = event_log_cycle_files(dir)?;
    if files.last() != Some(&target) {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "partial checkpoint event appears in {}, but it is not the final event-log cycle file",
            target.display()
        )));
    }
    let complete_end = target_bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let partial = &target_bytes[complete_end..];
    let expected_bytes = serde_json::to_vec(expected_record)?;
    let partial_is_exact_record =
        serde_json::from_slice::<EventLogRecord>(partial).ok().as_ref() == Some(expected_record);
    if !partial_is_exact_record && !expected_bytes.starts_with(partial) {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "event-log tail in {} is not a prefix of the journaled checkpoint event",
            target.display()
        )));
    }

    let mut next_index = 0_u64;
    for path in &files {
        let bytes = fs::read(path)?;
        let visible = if path == &target {
            &bytes[..complete_end]
        } else {
            bytes.as_slice()
        };
        for line in visible.split(|byte| *byte == b'\n') {
            if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue;
            }
            let record: EventLogRecord = serde_json::from_slice(line).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "cannot validate event-log prefix in {} during checkpoint recovery: {error}",
                    path.display()
                ))
            })?;
            if record.index != next_index {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "checkpoint recovery expected dense event index {next_index}, found {} in {}",
                    record.index,
                    path.display()
                )));
            }
            next_index += 1;
        }
    }
    if next_index != expected_record.index {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "partial checkpoint event targets index {}, but the complete event-log prefix ends at {next_index}",
            expected_record.index
        )));
    }
    atomically_replace_file(&target, &target_bytes[..complete_end])?;
    Ok(())
}

/// Complete a non-decision checkpoint whose Git commit won the crash race
/// against runtime-state persistence.  Decision-bearing checkpoints use the
/// stronger trust-decision tag/record transaction and never write this
/// journal.
fn recover_pending_checkpoint_transaction(
    paths: &RuntimePaths,
    state: &mut ProtocolState,
    metadata: &mut RuntimeMetadata,
) -> Result<(), RuntimeError> {
    let journal_path = paths.root.join(CHECKPOINT_TRANSACTION_JOURNAL_FILENAME);
    if !journal_path.is_file() {
        return Ok(());
    }
    let transaction: PendingCheckpointTransaction =
        serde_json::from_slice(&fs::read(&journal_path)?).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "checkpoint recovery journal {} is malformed: {error}",
                journal_path.display()
            ))
        })?;
    if transaction.schema_version != CHECKPOINT_TRANSACTION_JOURNAL_SCHEMA_VERSION {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "checkpoint recovery journal {} has unsupported schema version {}",
            journal_path.display(),
            transaction.schema_version
        )));
    }
    if transaction
        .event_record
        .trust_record
        .as_ref()
        .is_some_and(|record| record.kind.is_decision_kind())
    {
        return Err(RuntimeError::InvalidRuntimeState(
            "generic checkpoint recovery journal unexpectedly carries a decision trust record"
                .into(),
        ));
    }

    let checkpoint_tag = format!(
        "supervisor2/checkpoint-{:06}",
        transaction.event_record.index
    );
    let current_head = git_head_sha(&transaction.repo_path);
    let head_advanced = current_head != transaction.pre_commit_head;
    if head_advanced {
        let has_expected_parent = match transaction.pre_commit_head.as_deref() {
            Some(pre_head) => git_stdout(&transaction.repo_path, &["rev-parse", "HEAD^"])?
                .is_some_and(|parent| String::from_utf8_lossy(&parent).trim() == pre_head),
            None => git_stdout(
                &transaction.repo_path,
                &["rev-list", "--parents", "-n", "1", "HEAD"],
            )?
            .is_some_and(|line| String::from_utf8_lossy(&line).split_whitespace().count() == 1),
        };
        if !has_expected_parent {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "Git HEAD advanced by more than the single checkpoint commit recorded in {}; refusing to attach runtime recovery to a later commit",
                journal_path.display()
            )));
        }
    }
    let history_bytes = git_stdout(
        &transaction.repo_path,
        &["show", "HEAD:.trellis-history/supervisor_state.json"],
    )?;

    // A crash before the hook's commit leaves only an uncommitted intent.
    // With neither the checkpoint tag nor a matching committed history blob,
    // the pre-step runtime files remain authoritative and the intent can be
    // discarded.  A tag at HEAD, by contrast, is a durable commit witness and
    // every mismatch below is corruption, never a reason to guess.
    let Some(history_bytes) = history_bytes else {
        if head_advanced {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "Git HEAD advanced after {}, but the committed supervisor_state.json is missing",
                journal_path.display()
            )));
        }
        fs::remove_file(&journal_path)?;
        return Ok(());
    };
    let history_json: serde_json::Value =
        serde_json::from_slice(&history_bytes).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "cannot parse committed supervisor_state.json while recovering checkpoint: {error}"
            ))
        })?;
    let history_json = crate::shared_state_codec::decode_shared_state(history_json)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let history_event_count = history_json
        .get("event_count")
        .and_then(serde_json::Value::as_u64);
    let history_state: Option<ProtocolState> = history_json
        .get("state")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let history_metadata: Option<RuntimeMetadata> = history_json
        .get("metadata")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let history_checkpoint: Option<RuntimeCheckpoint> = history_json
        .get("checkpoint")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let history_commands: Option<Vec<ProtocolCommand>> = history_json
        .get("commands")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());

    let committed_matches = match (
        history_state.as_ref(),
        history_metadata.as_ref(),
        history_checkpoint.as_ref(),
        history_commands.as_ref(),
    ) {
        (Some(post_state), Some(post_metadata), Some(checkpoint), Some(commands)) => {
            history_event_count == Some(transaction.event_record.index)
                && commands == &transaction.event_record.commands
                && serialized_sha256(post_state)? == transaction.post_state_sha256
                && serialized_sha256(post_metadata)? == transaction.metadata_sha256
                && serialized_sha256(checkpoint)? == transaction.checkpoint_sha256
                && post_state.phase == transaction.event_record.phase
                && post_state.stage == transaction.event_record.stage
                && post_state.cycle == transaction.event_record.cycle
        }
        _ => false,
    };
    if !committed_matches && !head_advanced {
        fs::remove_file(&journal_path)?;
        return Ok(());
    }
    if !committed_matches {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "Git HEAD advanced, but the committed checkpoint payload does not match {}",
            journal_path.display()
        )));
    }
    if !git_ref_targets_head(&transaction.repo_path, &checkpoint_tag)? {
        git_update_tag_to_head(&transaction.repo_path, &checkpoint_tag)?;
    }
    if transaction.is_clean {
        let clean_tag = format!("supervisor2/clean-{:06}", transaction.event_record.index);
        if !git_ref_targets_head(&transaction.repo_path, &clean_tag)? {
            git_update_tag_to_head(&transaction.repo_path, &clean_tag)?;
        }
    }

    let post_state = history_state.expect("committed_matches requires state");
    let post_metadata = history_metadata.expect("committed_matches requires metadata");
    let checkpoint = history_checkpoint.expect("committed_matches requires checkpoint");
    let log_dir = event_log_dir_for(&paths.root, &post_metadata);
    repair_partial_checkpoint_event_tail(&log_dir, &transaction.event_record)?;
    let event_count = read_event_count(&log_dir)?;
    match event_count {
        count if count == transaction.event_record.index => {}
        count if count == transaction.event_record.index + 1 => {
            if event_log_record_at_index(&log_dir, transaction.event_record.index)?.as_ref()
                != Some(&transaction.event_record)
            {
                return Err(RuntimeError::InvalidRuntimeState(
                    "checkpoint recovery found a different event at the journaled index".into(),
                ));
            }
        }
        count => {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "checkpoint recovery journal targets event index {}, but the event log count is {count}",
                transaction.event_record.index
            )));
        }
    }

    atomically_replace_file(
        &paths.state_path,
        serde_json::to_string_pretty(&post_state)?.as_bytes(),
    )?;
    atomically_replace_file(
        &paths.metadata_path,
        serde_json::to_string_pretty(&post_metadata)?.as_bytes(),
    )?;
    atomically_replace_file(
        &paths.checkpoint_path,
        serde_json::to_string_pretty(&checkpoint)?.as_bytes(),
    )?;
    if event_count == transaction.event_record.index {
        fs::create_dir_all(&log_dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(event_log_cycle_file(
                &log_dir,
                transaction.event_record.cycle,
            ))?;
        serde_json::to_writer(&mut file, &transaction.event_record)?;
        file.write_all(b"\n")?;
    }
    *state = post_state;
    *metadata = post_metadata;
    fs::remove_file(&journal_path)?;
    Ok(())
}

fn read_metadata(path: &Path) -> Result<RuntimeMetadata, RuntimeError> {
    if !path.exists() {
        return Ok(RuntimeMetadata::default());
    }
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

/// Checkpoint copies of seed-derived worker guidance are never authoritative.
/// Re-read the externally configured seed closure on every trust-runtime load
/// (and after protected-revision recovery), then require exact projection
/// equality before any request can be refreshed or dispatched.
fn verify_runtime_trust_seed_projection(
    state: &mut ProtocolState,
    metadata: &RuntimeMetadata,
) -> Result<bool, RuntimeError> {
    let seed_path = metadata
        .trust_seed_manifest_path
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires the seed manifest path on runtime load".into(),
            )
        })?;
    let seed_value = parse_json_strict(&fs::read(seed_path).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "failed to read seed manifest {} on runtime load: {error}",
            seed_path.display()
        ))
    })?)
    .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let registry = SchemaRegistry::v1()
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let seed = AuthoritativeRecord::parse(&registry, seed_value)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let bundle_path = metadata
        .trust_seed_definition_bundle_path
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires the seed definition bundle path on runtime load".into(),
            )
        })?;
    let closure = verify_seed_definition_bundle(
        &seed,
        &fs::read(bundle_path).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "failed to read seed definition bundle {} on runtime load: {error}",
                bundle_path.display()
            ))
        })?,
    )
    .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let evidence_manifest_path = metadata
        .trust_evidence_tool_manifest_path
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires the evidence/tool manifest on runtime load".into(),
            )
        })?;
    let evidence_root_path = metadata
        .trust_evidence_tool_root_path
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires the evidence/tool root on runtime load".into(),
            )
        })?;
    let evidence = verify_evidence_tool_manifest(
        evidence_root_path,
        &fs::read(evidence_manifest_path).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "failed to read evidence/tool manifest {} on runtime load: {error}",
                evidence_manifest_path.display()
            ))
        })?,
    )
    .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let mut support_definitions = seed_support_definition_projection(&evidence)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    // Headless authority-recovery runtimes have no attached worktree.  The
    // worker path requires a repository and rechecks these bytes before use.
    if let Some(repo_path) = metadata.repo_path.as_deref() {
        support_definitions = hydrate_seed_support_definition_files(
            &repo_path.join("Tablet"),
            &support_definitions,
        )
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    } else {
        // A headless authority-recovery runtime cannot re-read the body. If
        // state already has the exact digest-bound body, retain it while the
        // metadata-only external projection proves its identity.
        for (node, support) in &mut support_definitions {
            if let Some(recorded) = state.trust_base.seed_support_definitions.get(node) {
                if recorded.logical_id == support.logical_id
                    && recorded.evidence_relative_path == support.evidence_relative_path
                    && recorded.raw_sha256 == support.raw_sha256
                {
                    support.definition_utf8 = recorded.definition_utf8.clone();
                }
            }
        }
    }
    let mut migrated = migrate_legacy_seed_support_projection(state, &support_definitions)?;
    let boundary_sha256 = if evidence
        .leaves_by_logical_id
        .contains_key("trusted-platform-boundary-v1")
    {
        Some(
            evidence
                .trusted_platform_boundary_projection_sha256(evidence_root_path)
                .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?,
        )
    } else {
        None
    };
    match (
        state.trust_base.trusted_platform_boundary_sha256,
        boundary_sha256,
    ) {
        (Some(expected), Some(observed)) if expected != observed => {
            return Err(RuntimeError::InvalidRuntimeState(
                "meaning-bearing trusted platform boundary changed".into(),
            ));
        }
        (Some(_), None) => {
            return Err(RuntimeError::InvalidRuntimeState(
                "trusted platform boundary unchanged proof: verified evidence closure lacks trusted-platform-boundary-v1".into(),
            ));
        }
        (None, Some(observed)) => {
            state.trust_base.trusted_platform_boundary_sha256 = Some(observed);
            migrated = true;
        }
        _ => {}
    }
    if state.trust_base.seed_support_definitions != support_definitions {
        return Err(RuntimeError::InvalidRuntimeState(
            "seed_support_definitions differ from the verified evidence closure"
                .into(),
        ));
    }
    // Seed adaptation-ledger rows are re-derived from the configured closure.
    // The ledger comparison is a
    // projection of the SEED rows only.  Stage 7's sanctioned seam-repair
    // authorization appends run rows (`Authorized`/`Ratified`) to the same
    // vector; comparing the whole vector to a seed-only projection made
    // the FIRST authorization of any run hard-error on the next load and
    // at the next in-process idle reconcile.  Seed-borne rows always carry
    // `Seed` status (`seed_adaptation_ledger_projection` refuses anything
    // else), so this filter is the exact left side of that projection —
    // the same pattern `TrustSeamRepairContext::from_state` already uses.
    //
    let ledger_rows = crate::trust_base::seed_adaptation_ledger_projection(&closure)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let state_seed_rows: Vec<_> = state
        .trust_base
        .adaptation_ledger
        .iter()
        .filter(|row| row.status == crate::trust_base::AdaptationLedgerStatus::Seed)
        .cloned()
        .collect();
    if state_seed_rows != ledger_rows {
        return Err(RuntimeError::InvalidRuntimeState(
            "seed-status adaptation-ledger rows differ from the verified seed closure"
                .into(),
        ));
    }
    // The external closure still verifies and exposes raw runner, manifest,
    // bundle, acknowledgment, and Trellis build hashes as provenance. Those
    // values are intentionally absent from the load decision: only the
    // platform and seed-semantic projections above can refuse a resume.
    Ok(migrated)
}

/// Older required-v1 checkpoints predate the explicit non-node support
/// projection.  It is a derived cache, not authority, so reconstruct an empty
/// legacy field only from the currently verified evidence closure. No
/// closure result is promoted by this migration; affected nodes still enter
/// deterministic revalidation and must earn a newly bound record.
fn migrate_legacy_seed_support_projection(
    state: &mut ProtocolState,
    projection: &BTreeMap<NodeId, crate::model::TrustSeedSupportDefinition>,
) -> Result<bool, RuntimeError> {
    if !state.trust_base.seed_support_definitions.is_empty() {
        if state.trust_base.seed_support_definitions == *projection {
            return Ok(false);
        }
        let mut recorded_metadata = state.trust_base.seed_support_definitions.clone();
        let mut projected_metadata = projection.clone();
        for support in recorded_metadata.values_mut() {
            support.definition_utf8 = None;
        }
        for support in projected_metadata.values_mut() {
            support.definition_utf8 = None;
        }
        if recorded_metadata == projected_metadata
            && state
                .trust_base
                .seed_support_definitions
                .values()
                .all(|support| support.definition_utf8.is_none())
            && projection
                .values()
                .all(|support| support.definition_utf8.is_some())
        {
            state.trust_base.seed_support_definitions = projection.clone();
            return Ok(true);
        }
        return Ok(false);
    }
    if !state.trust_base.required() {
        return Err(RuntimeError::InvalidRuntimeState(
            "seed support projection migration is valid only in required-v1 mode".into(),
        ));
    }
    if state.local_closure_records.values().any(|record| {
        !record.seed_support_definition_deps.is_empty()
            || record.seed_support_evidence_root.is_some()
            || !record.seed_support_file_hashes.is_empty()
    }) {
        return Err(RuntimeError::InvalidRuntimeState(
            "checkpoint has support-bound local-closure records but no seed support projection"
                .into(),
        ));
    }
    state.trust_base.seed_support_definitions = projection.clone();
    Ok(true)
}

/// The most recent protected terminal recorded for `revision_lane_id` in the
/// scanned event-log trust records, with its record digest.
fn revision_terminal_kind(
    records: &[TrustRecord],
    revision_lane_id: &str,
) -> Option<(EventKind, crate::trust_base::Sha256Digest)> {
    records
        .iter()
        .filter(|record| {
            record.lane.as_deref() == Some(revision_lane_id)
                && matches!(
                    record.kind,
                    EventKind::ProtectedReapprovalApproved | EventKind::ProtectedReapprovalFeedback
                )
        })
        .map(|record| (record.kind, record.record_sha256))
        .next_back()
}

/// Lane-open detection (audit F9): the active lane is exactly an
/// `AuditAuthorization` record with no subsequent
/// `ProtectedReapproval{Approved,Feedback}` terminal for that lane.
fn project_open_revision_lane(records: &[TrustRecord]) -> Option<String> {
    let mut open: Option<String> = None;
    for record in records {
        match record.kind {
            EventKind::AuditAuthorization => {
                open = record.lane.clone();
            }
            EventKind::ProtectedReapprovalApproved | EventKind::ProtectedReapprovalFeedback => {
                if open.is_some() && open.as_deref() == record.lane.as_deref() {
                    open = None;
                }
            }
            _ => {}
        }
    }
    open
}

/// The five named seed roots bound into every trust record, projected from
/// state (all five are required post-gate).
fn trust_record_seed_roots(
    trust: &crate::model::TrustBaseProtocolState,
) -> Result<TrustRecordSeedRoots, RuntimeError> {
    let field = |value: Option<crate::trust_base::Sha256Digest>, name: &str| {
        value.ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(format!(
                "trust record construction requires the seed root {name}"
            ))
        })
    };
    Ok(TrustRecordSeedRoots {
        seed_manifest_sha256: field(trust.seed_manifest_sha256, "seed_manifest_sha256")?,
        seed_definition_bundle_sha256: field(
            trust.seed_definition_bundle_sha256,
            "seed_definition_bundle_sha256",
        )?,
        evidence_tool_manifest_sha256: field(
            trust.evidence_tool_manifest_sha256,
            "evidence_tool_manifest_sha256",
        )?,
        authored_semantic_root: field(trust.authored_semantic_root, "authored_semantic_root")?,
        approved_evidence_tool_input_root: field(
            trust.approved_evidence_tool_input_root,
            "approved_evidence_tool_input_root",
        )?,
    })
}

pub(crate) const TRUST_DECISION_TAG_PREFIX: &str = "supervisor2/trust-decision-";
pub(crate) const TRUST_DECISION_RECORD_DIR: &str = ".trellis-history/trust-decisions";

/// Digest-keyed tag name (audit D2): the 12-hex prefix escapes the routine
/// rewind scripts' numeric-suffix tag prune by construction; an
/// `event_count` suffix is FORBIDDEN (non-monotonic across segmentation and
/// rewinds — the precedent's own doc records the collision).
pub(crate) fn trust_decision_tag_name(digest: crate::trust_base::Sha256Digest) -> String {
    format!("{TRUST_DECISION_TAG_PREFIX}{}", &digest.to_hex()[..12])
}

/// One decision recovered from the surviving tag history: the tag name, the
/// committed record file's exact bytes, and its parsed, digest-verified
/// line/record.
#[cfg_attr(test, derive(Debug))]
struct TrustDecisionTagRecord {
    tag: String,
    file_bytes: Vec<u8>,
    line: EventLogRecord,
    record: TrustRecord,
}

enum TrustDecisionTagProbe {
    Absent,
    PresentSameDigest,
    PresentDifferentContent,
}

fn git_stdout(repo: &Path, args: &[&str]) -> Result<Option<Vec<u8>>, RuntimeError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(output.stdout))
}

struct TheoremStatingBaseline {
    commit: String,
    state: ProtocolState,
}

struct ObservedLiveTabletState {
    live: WorkingSnapshot,
    node_kinds: BTreeMap<NodeId, crate::model::NodeKind>,
    proof_nodes: BTreeSet<NodeId>,
    deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    target_claims: BTreeMap<NodeId, BTreeSet<crate::model::TargetId>>,
}

fn restore_repo_path_from_git(
    repo_path: &Path,
    commit: &str,
    rel_path: &str,
) -> Result<(), RuntimeError> {
    let show_arg = format!("{commit}:{rel_path}");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["show", &show_arg])
        .output()?;
    if !output.status.success() {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "failed to restore {rel_path} from {commit}; exit={:?}; stderr={:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        )));
    }
    let dest = repo_path.join(rel_path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, output.stdout)?;
    Ok(())
}

fn remove_tablet_node_files(repo_path: &Path, node: &NodeId) -> Result<(), RuntimeError> {
    for ext in ["lean", "tex"] {
        let path = repo_path
            .join("Tablet")
            .join(format!("{}.{}", node.as_str(), ext));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(RuntimeError::Io(err)),
        }
    }
    Ok(())
}

fn target_claims_after_theorem_stating_node_restore(
    state: &ProtocolState,
    baseline: &ProtocolState,
    node: &NodeId,
) -> BTreeMap<NodeId, BTreeSet<crate::model::TargetId>> {
    // Cone clean restores one node. Other surviving nodes keep live claims;
    // orphan pruning and verifier fingerprints reconcile the mixed state.
    let mut target_claims = state.target_claims.clone();
    match baseline.target_claims.get(node) {
        Some(targets) => {
            target_claims.insert(node.clone(), targets.clone());
        }
        None => {
            target_claims.remove(node);
        }
    }
    target_claims
}

fn paper_approved_after_theorem_stating_node_restore(
    state: &ProtocolState,
    baseline: &ProtocolState,
    node: &NodeId,
) -> BTreeMap<crate::model::TargetId, crate::model::Fingerprint> {
    let mut approved = state.paper_approved_fingerprints.clone();
    for target in baseline.target_claims.get(node).into_iter().flatten() {
        if let Some(fp) = baseline.paper_approved_fingerprints.get(target) {
            approved.insert(target.clone(), fp.clone());
        }
    }
    approved
}

fn retain_target_claims_for_present(
    target_claims: &mut BTreeMap<NodeId, BTreeSet<crate::model::TargetId>>,
    present_nodes: &BTreeSet<NodeId>,
    configured_targets: &BTreeSet<crate::model::TargetId>,
) {
    target_claims.retain(|node, targets| {
        if !present_nodes.contains(node) {
            return false;
        }
        targets.retain(|target| configured_targets.contains(target));
        !targets.is_empty()
    });
}

fn paper_source_path_from_config(config_path: Option<&Path>) -> Option<PathBuf> {
    let config_path = config_path?;
    let text = fs::read_to_string(config_path).ok()?;
    let raw: serde_json::Value = serde_json::from_str(&text).ok()?;
    let paper = raw
        .as_object()
        .and_then(|obj| obj.get("workflow"))
        .and_then(|workflow| workflow.as_object())
        .and_then(|workflow| workflow.get("paper_tex_path"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(PathBuf::from(paper))
}

/// GAP B (W10): the prose goal file path for fingerprint observation —
/// `Some` only for PV runs. Mirrors `paper_source_path_from_config`
/// (config top-level `goal_file`, default `GOAL.md`), resolved against the
/// repo so observation sites can read it directly.
pub fn goal_prose_path_from_config(
    config_path: Option<&Path>,
    repo_path: &Path,
    state: &ProtocolState,
) -> Option<PathBuf> {
    if !state.is_pv() {
        return None;
    }
    let rel = (|| -> Option<String> {
        let text = fs::read_to_string(config_path?).ok()?;
        let raw: serde_json::Value = serde_json::from_str(&text).ok()?;
        raw.as_object()?
            .get("goal_file")?
            .as_str()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })()
    .unwrap_or_else(|| "GOAL.md".to_string());
    let candidate = PathBuf::from(&rel);
    Some(if candidate.is_absolute() {
        candidate
    } else {
        repo_path.join(candidate)
    })
}

fn observe_live_tablet_state_from_repo(
    repo_path: &Path,
    state: &ProtocolState,
    mut target_claims: BTreeMap<NodeId, BTreeSet<crate::model::TargetId>>,
    approved_paper_fingerprints: &BTreeMap<crate::model::TargetId, crate::model::Fingerprint>,
    paper_source_path: Option<&Path>,
    goal_prose_path: Option<&Path>,
) -> Result<ObservedLiveTabletState, RuntimeError> {
    let present_nodes = crate::worker_normalization::present_nodes_from_repo(repo_path)
        .map_err(RuntimeError::InvalidRuntimeState)?;
    retain_target_claims_for_present(
        &mut target_claims,
        &present_nodes,
        &state.configured_targets,
    );
    let open_nodes = crate::worker_normalization::open_nodes_from_repo(repo_path, &present_nodes);
    let node_kinds = crate::worker_normalization::node_kinds_from_repo(repo_path, &present_nodes);
    let proof_nodes =
        crate::worker_normalization::proof_nodes_from_kinds(&node_kinds, &present_nodes);
    let deps = crate::worker_normalization::direct_deps_from_repo(repo_path, &present_nodes);
    let coverage = crate::worker_normalization::coverage_from_claims(
        &state.configured_targets,
        &target_claims,
        &present_nodes,
    );
    let under_model_assumption_nodes =
        crate::runtime_cli_observations::under_model_assumption_nodes_from_state(state);
    let target_fingerprints =
        crate::runtime_cli_observations::observe_correspondence_fingerprints_with_under_model_assumptions(
            repo_path,
            &present_nodes,
            &under_model_assumption_nodes,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let sound_current_fingerprints =
        crate::runtime_cli_observations::observe_soundness_fingerprints(
            repo_path,
            &present_nodes,
            &node_kinds,
            &state.node_role,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let sound_current_fingerprint_parts =
        crate::runtime_cli_observations::observe_soundness_fingerprint_parts(
            repo_path,
            &present_nodes,
            &node_kinds,
            &state.node_role,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let sketch_proof_nodes =
        crate::runtime_cli_observations::observe_sketch_proof_nodes(repo_path, &present_nodes);
    let placeholder_definition_nodes =
        crate::runtime_cli_observations::observe_placeholder_definition_nodes(
            repo_path,
            &present_nodes,
            &node_kinds,
        );
    let covering_union: BTreeSet<NodeId> = coverage
        .values()
        .flatten()
        .filter(|node| !under_model_assumption_nodes.contains(*node))
        .cloned()
        .collect();
    let lean_relevant_per_covering =
        crate::runtime_cli_observations::observe_lean_relevant_definition_descendants_per_node(
            repo_path,
            &covering_union,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let paper_current_fingerprints = crate::observe_paper_faithfulness_fingerprints(
        repo_path,
        &state.configured_targets,
        &target_claims,
        &present_nodes,
        approved_paper_fingerprints,
        &lean_relevant_per_covering,
        goal_prose_path,
    );
    let deviation_current_fingerprints =
        crate::runtime_cli_observations::observe_deviation_fingerprints(
            repo_path,
            &state.deviation_files,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let substantiveness_current_fingerprints =
        crate::runtime_cli_observations::observe_substantiveness_fingerprints(
            repo_path,
            &present_nodes,
            paper_source_path,
            &node_kinds,
            &state.node_deviation_claims,
            &deviation_current_fingerprints,
            &state.configured_reference_papers,
            &state.node_reference_grounds,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let certificate_coverage: BTreeMap<crate::model::TargetId, BTreeSet<NodeId>> = coverage
        .iter()
        .map(|(target, nodes)| {
            (
                target.clone(),
                nodes
                    .iter()
                    .filter(|node| !under_model_assumption_nodes.contains(*node))
                    .cloned()
                    .collect(),
            )
        })
        .collect();
    let protected_closure_nodes_per_target =
        crate::runtime_cli_observations::observe_protected_closure_nodes(
            repo_path,
            &certificate_coverage,
            &present_nodes,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    Ok(ObservedLiveTabletState {
        live: WorkingSnapshot {
            present_nodes,
            open_nodes,
            coverage,
            target_fingerprints: target_fingerprints.clone(),
            corr_current_fingerprints: target_fingerprints,
            paper_current_fingerprints,
            sound_current_fingerprints,
            deviation_current_fingerprints,
            sound_current_fingerprint_parts,
            sketch_proof_nodes,
            placeholder_definition_nodes,
            substantiveness_current_fingerprints,
            protected_closure_nodes_per_target,
            // Challenge coverage is kernel state derived from
            // `challenge_claims`; the observation layer doesn't read it
            // from the repo. `normalize_live_structural_state` recomputes
            // it right after this snapshot is installed.
            challenge_coverage: BTreeMap::new(),
        },
        node_kinds,
        proof_nodes,
        deps,
        target_claims,
    })
}

/// Walk the configured repo's git history for the most recent commit whose
/// `.trellis-history/supervisor_state.json` carried a populated
/// `coarse_dag_nodes`. Used by
/// [`SupervisorRuntime::heal_coarse_dag_from_git_if_needed`] to recover
/// from a state that lost the field.
///
/// Bounded: scans at most [`COARSE_DAG_GIT_SCAN_LIMIT`] commits. Returns
/// `None` if the repo isn't a git repo, no historical commit had a
/// populated value, or any git invocation errors.
fn recover_coarse_dag_from_git(repo_path: &Path) -> Option<BTreeSet<NodeId>> {
    let log_output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "log",
            "--format=%H",
            &format!("--max-count={COARSE_DAG_GIT_SCAN_LIMIT}"),
            "--",
            COARSE_DAG_HISTORY_PATH,
        ])
        .output()
        .ok()?;
    if !log_output.status.success() {
        return None;
    }
    let log_text = String::from_utf8(log_output.stdout).ok()?;
    for sha in log_text.lines() {
        let sha = sha.trim();
        if sha.is_empty() {
            continue;
        }
        let show_arg = format!("{sha}:{COARSE_DAG_HISTORY_PATH}");
        let show_output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["show", &show_arg])
            .output()
            .ok()?;
        if !show_output.status.success() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_slice(&show_output.stdout) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Historical blobs are plain and pass through untouched; a
        // `trellis-shared-state/1` blob is expanded before any typed read.
        let parsed = match crate::shared_state_codec::decode_shared_state(parsed) {
            Ok(value) => value,
            Err(err) => {
                eprintln!(
                    "recover_coarse_dag_from_git: {sha}:{COARSE_DAG_HISTORY_PATH} is a corrupt \
                     shared-state document ({err}); skipping this revision"
                );
                continue;
            }
        };
        let Some(arr) = parsed
            .get("state")
            .and_then(|s| s.get("coarse_dag_nodes"))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        if arr.is_empty() {
            continue;
        }
        let nodes: BTreeSet<NodeId> = arr
            .iter()
            .filter_map(|v| v.as_str().map(NodeId::from))
            .collect();
        if !nodes.is_empty() {
            return Some(nodes);
        }
    }
    None
}

fn recover_theorem_stating_baseline_from_git(repo_path: &Path) -> Option<TheoremStatingBaseline> {
    let log_output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "log",
            "--format=%H",
            &format!("--max-count={THEOREM_STATING_BASELINE_GIT_SCAN_LIMIT}"),
            "--",
            COARSE_DAG_HISTORY_PATH,
        ])
        .output()
        .ok()?;
    if !log_output.status.success() {
        return None;
    }
    let log_text = String::from_utf8(log_output.stdout).ok()?;
    let mut candidate: Option<TheoremStatingBaseline> = None;
    for sha in log_text
        .lines()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
    {
        let show_arg = format!("{sha}:{COARSE_DAG_HISTORY_PATH}");
        let show_output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["show", &show_arg])
            .output()
            .ok()?;
        if !show_output.status.success() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_slice(&show_output.stdout) {
            Ok(value) => value,
            Err(_) => continue,
        };
        // Same as `recover_coarse_dag_from_git`: decode before the typed read.
        let parsed = match crate::shared_state_codec::decode_shared_state(parsed) {
            Ok(value) => value,
            Err(err) => {
                eprintln!(
                    "recover_theorem_stating_baseline_from_git: {sha}:{COARSE_DAG_HISTORY_PATH} \
                     is a corrupt shared-state document ({err}); skipping this revision"
                );
                continue;
            }
        };
        let Some(state_value) = parsed.get("state").cloned() else {
            continue;
        };
        let mut parsed_state: ProtocolState = match serde_json::from_value(state_value) {
            Ok(state) => state,
            Err(_) => continue,
        };
        parsed_state.normalize_all_structural_state();
        parsed_state.ensure_node_metadata();
        if parsed_state.phase == Phase::ProofFormalization
            && !parsed_state.coarse_dag_nodes.is_empty()
        {
            candidate = Some(TheoremStatingBaseline {
                commit: sha.to_string(),
                state: parsed_state,
            });
            continue;
        }
        if candidate.is_some() && parsed_state.phase.is_theorem_stating_like() {
            break;
        }
    }
    candidate
}

/// Path inside the repo where the supervisor's git checkpoint hook writes
/// a snapshot of the live `ProtocolState`. Each `supervisor2/checkpoint-*`
/// commit updates this file (see `trellis/runtime/git_checkpoint_hook.py`),
/// so historical revisions are the canonical source for recovering the
/// authentic `coarse_dag_nodes` value.
const COARSE_DAG_HISTORY_PATH: &str = ".trellis-history/supervisor_state.json";

/// Cap the number of historical commits scanned during the heal. Each
/// commit needs one `git show` subprocess. 500 is well past any realistic
/// rewind distance and bounds worst-case load latency.
const COARSE_DAG_GIT_SCAN_LIMIT: u32 = 500;

/// The theorem-stating baseline can be far behind a long proof run. This
/// scan only happens when the reviewer confirms the targeted reset, so a
/// higher bound is preferable to failing on mature runs.
const THEOREM_STATING_BASELINE_GIT_SCAN_LIMIT: u32 = 5000;

/// W1 Tier 1: the required-v1 gate/endgame test primitives, promoted out of
/// `#[cfg(test)]` into shared integration-test support.  NO behaviour
/// change: every item here moved verbatim from this file's `mod tests`.
///
/// A child module of `runtime` (not `kernel/tests/common/`) because these
/// primitives legitimately reach `SupervisorRuntime`'s private fields
/// (`state`, `event_count`, `paths`) to park fixtures mid-flow — access an
/// external test crate cannot have without widening the production API.
/// The `test-support` cargo feature (enabled for every test target by the
/// self dev-dependency) keeps all of it out of production builds.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    use crate::model::{
        CorrStatus, HumanChoice, HumanGateResponse, RequestKind, ResponseStatus,
        ReviewDecisionKind, ReviewResponse, SoundStatus, TrustBaseMode, TrustRoutineGateState,
    };
    use std::collections::{BTreeSet, VecDeque};
    use std::os::unix::fs::PermissionsExt;

    const FIXTURE_TRUSTED_PLATFORM_BOUNDARY: &[u8] =
        br#"{"schema":"trellis-trusted-platform-boundary/v1","toolchain":"fixture-lean-v1"}"#;

    pub fn set<T: From<String> + Ord>(items: &[&str]) -> BTreeSet<T> {
        items.iter().map(|s| T::from((*s).to_string())).collect()
    }

    /// Mutable access to a runtime's protocol state for fixture surgery.
    /// Test-support only: production code never mutates state out-of-band.
    pub fn state_mut(runtime: &mut SupervisorRuntime) -> &mut ProtocolState {
        &mut runtime.state
    }

    /// Set the in-memory event counter (fixtures that hand-seed event-log
    /// lines must keep the dense-index invariant).
    pub fn set_event_count(runtime: &mut SupervisorRuntime, count: u64) {
        runtime.event_count = count;
    }

    pub fn mark_substantiveness_pass(state: &mut ProtocolState, node: &str, fp: &str) {
        state
            .substantiveness_status
            .insert(node.into(), SubstantivenessStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert(node.into(), fp.into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert(node.into(), fp.into());
    }

    pub fn base_state() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.configured_targets = set(&["t"]);
        state.proof_nodes = set(&["a"]);
        state.target_claims.insert("a".into(), set(&["t"]));
        state.live.present_nodes = set(&["a", "b"]);
        state.live.open_nodes = set(&["a", "b"]);
        state.live.coverage.insert("t".into(), set(&["a"]));
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ta".into());
        state
            .live
            .target_fingerprints
            .insert("a".into(), "ta".into());
        state
            .live
            .corr_current_fingerprints
            .insert("a".into(), "ca".into());
        state
            .live
            .corr_current_fingerprints
            .insert("b".into(), "cb".into());
        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa".into());
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.committed = state.live.clone();
        state.corr_status.insert("a".into(), CorrStatus::Pass);
        state.corr_status.insert("b".into(), CorrStatus::Pass);
        state.paper_status.insert("t".into(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert("a".into(), "ca".into());
        state
            .corr_approved_fingerprints
            .insert("b".into(), "cb".into());
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta".into());
        state.sound_status.insert("a".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("a".into(), "sa".into());
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();
        state
    }

    pub fn seed_test_support_repo(repo: &Path) {
        fs::create_dir_all(repo.join(".trellis/scripts")).expect("script dir");
        fs::create_dir_all(repo.join("Tablet")).expect("tablet dir");
        fs::write(repo.join("GOAL.md"), "Verify the fixture target.\n")
            .expect("write fixture GOAL.md");
        fs::write(
            repo.join("tcb_manifest.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema": "pv-tcb-disclosure/v1",
                "global": [],
                "nodes": {},
                "tcb_disclosure": [],
                "extraction_provenance": {
                    "extraction_toolchain": {
                        "lean": "fixture",
                        "target_triple": "fixture-target",
                        "target_pointer_width": 64,
                        "extraction_profile": "dev",
                        "overflow_checks": true,
                        "panic_strategy": "unwind"
                    },
                    "extractor_stack": ["fixture-extractor"],
                    "extractor_toolchain_sha256": crate::trust_base::raw_sha256(
                        b"fixture-extractor"
                    ),
                    "source_digests": [],
                    "not_recorded": []
                }
            }))
            .expect("serialize fixture TCB manifest"),
        )
        .expect("write fixture TCB manifest");
        fs::write(
            repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        )
        .expect("write preamble lean");
        fs::write(
            repo.join("Tablet/Assumptions.lean"),
            "def RustValidSliceU8 : Prop := True\n",
        )
        .expect("write support lean");
        fs::write(repo.join("Tablet/Preamble.tex"), "").expect("write preamble tex");
        fs::write(
            repo.join("Tablet/a.lean"),
            "import Tablet.Preamble\n\ntheorem a : True := by\n  sorry\n",
        )
        .expect("write a lean");
        fs::write(
            repo.join("Tablet/a.tex"),
            "\\begin{theorem}a\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        )
        .expect("write a tex");
        fs::write(
            repo.join("Tablet/b.lean"),
            "import Tablet.Preamble\n\ndef b : Nat := by\n  sorry\n",
        )
        .expect("write b lean");
        fs::write(
            repo.join("Tablet/b.tex"),
            "\\begin{definition}b\\end{definition}\n",
        )
        .expect("write b tex");
        let check_path = repo.join(".trellis/scripts/check.py");
        fs::write(
            &check_path,
            "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("write check script");
        let mut permissions = fs::metadata(&check_path)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&check_path, permissions).expect("chmod script");
    }

    pub fn write_test_config(repo: &Path) -> PathBuf {
        let config_path = repo.join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "repo_path": repo,
                "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
                "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
                "workflow": {}
            })
            .to_string(),
        )
        .expect("write config");
        config_path
    }

    pub fn write_test_config_with_verifiers(repo: &Path) -> PathBuf {
        let config_path = repo.join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "repo_path": repo,
                "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
                "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
                "workflow": {},
                "verification": {
                    "correspondence_agents": [
                        {"provider": "claude", "model": "corr-a", "label": "corr-a"},
                        {"provider": "gemini", "model": "corr-b", "label": "corr-b"}
                    ],
                    "soundness_agents": [
                        {"provider": "claude", "model": "sound-a", "label": "sound-a"},
                        {"provider": "gemini", "model": "sound-b", "label": "sound-b"}
                    ]
                }
            })
            .to_string(),
        )
        .expect("write config");
        config_path
    }

    pub fn init_git_repo(repo: &Path) {
        for command in [
            vec!["init"],
            vec!["config", "user.name", "trellis-test"],
            vec!["config", "user.email", "trellis-test@example.com"],
            vec!["add", "-A"],
            vec!["commit", "-m", "Initial commit"],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(&command)
                .status()
                .expect("run git command");
            assert!(status.success(), "git command failed: {}", command.join(" "));
        }
    }

    pub fn commit_all(repo: &Path, message: &str) {
        for command in [vec!["add", "-A"], vec!["commit", "-m", message]] {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(&command)
                .status()
                .expect("run git command");
            assert!(status.success(), "git command failed: {}", command.join(" "));
        }
    }

    pub struct QueueAdapter {
        responses: VecDeque<WrapperResponse>,
    }

    impl QueueAdapter {
        pub fn new(responses: Vec<WrapperResponse>) -> Self {
            Self {
                responses: responses.into(),
            }
        }
    }

    impl WrapperAdapter for QueueAdapter {
        fn dispatch(&mut self, _request: &WrapperRequest) -> Result<WrapperResponse, String> {
            self.responses
                .pop_front()
                .ok_or_else(|| "no response queued".to_string())
        }
    }

    #[derive(Default)]
    pub struct RecordingCheckpointSink {
        pub payloads: Vec<CheckpointHookPayload>,
        pub fail_with: Option<String>,
    }

    impl CheckpointSink for RecordingCheckpointSink {
        fn commit(&mut self, payload: &CheckpointHookPayload) -> Result<(), String> {
            self.payloads.push(payload.clone());
            if let Some(message) = self.fail_with.clone() {
                return Err(message);
            }
            Ok(())
        }
    }

}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::model::{
        CorrResponse, CorrStatus, HumanChoice, HumanGateResponse, PaperResponse, RequestKind,
        ResponseStatus, ReviewDecisionKind, ReviewResponse, SoundResponse, SoundStatus, TargetId,
        TaskMode, TrustBaseMode, TrustRoutineGateState, WorkerOutcome, WorkerResponse,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir_in;

    fn on_production_sized_stack(body: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(body)
            .expect("spawn production-sized test stack")
            .join()
            .expect("production-sized test body panicked");
    }

    fn empty_corr_node_lanes(
        lanes: &BTreeSet<String>,
    ) -> BTreeMap<String, BTreeMap<NodeId, crate::model::Update<CorrStatus>>> {
        lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect()
    }

    fn empty_corr_target_lanes(
        lanes: &BTreeSet<String>,
    ) -> BTreeMap<String, BTreeMap<TargetId, crate::model::Update<CorrStatus>>> {
        lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect()
    }

    fn empty_sound_lanes(
        lanes: &BTreeSet<String>,
    ) -> BTreeMap<String, BTreeMap<NodeId, crate::model::Update<SoundStatus>>> {
        lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect()
    }


    /// A bridge-produced deviation authorization writes its semantic
    /// adaptation-ledger row without turning a repository build into a trust
    /// event; the runtime persists it and the NEXT process start accepts the
    /// checkpoint. Before the resume fix
    /// `verify_runtime_trust_seed_projection` compared the WHOLE ledger
    /// and the CURRENT pin against the seed-only projection, so the first
    /// authorization of a run hard-errored `InvalidRuntimeState` on the
    /// next load (and at the next in-process idle reconcile).
    #[test]
    fn initialize_normalizes_total_target_corr_fingerprints() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let mut state = ProtocolState::default();
        state.configured_targets = set(&["t"]);
        state.live.present_nodes = set(&["Preamble"]);
        state.committed.present_nodes = set(&["Preamble"]);
        state
            .live
            .corr_current_fingerprints
            .insert("Preamble".into(), "".into());
        state
            .live
            .target_fingerprints
            .insert("Preamble".into(), "".into());
        state
            .committed
            .corr_current_fingerprints
            .insert("Preamble".into(), "".into());
        state
            .committed
            .target_fingerprints
            .insert("Preamble".into(), "".into());

        let runtime =
            SupervisorRuntime::initialize_with_metadata(paths, state, RuntimeMetadata::default())
                .expect("initialize runtime");

        assert_eq!(
            runtime.state.live.paper_current_fingerprints.get("t"),
            Some(&"".to_string())
        );
        assert_eq!(
            runtime.state.committed.paper_current_fingerprints.get("t"),
            Some(&"".to_string())
        );
    }

    fn local_tempdir() -> tempfile::TempDir {
        let tmp_root = std::env::current_dir()
            .expect("current dir")
            .join(".tmp-tests");
        fs::create_dir_all(&tmp_root).expect("tmp root");
        tempdir_in(&tmp_root).expect("tempdir")
    }

    /// Cone-clean artifact invalidation (unitdistance cycle 694): pruning a
    /// node's source must also drop (a) the pruned module's own Lake build
    /// artifacts in BOTH `lib/lean/Tablet/` and `ir/Tablet/`, and (b) the
    /// artifacts of surviving dependents whose cached `ir/Tablet/<m>.setup.json`
    /// import graph still references a pruned module — otherwise `lake build`
    /// hard-fails on the missing olean even though no current source imports
    /// the pruned node. Unrelated modules' artifacts must survive, including
    /// a dependent that imports a LIVE module whose name has a pruned module's
    /// name as a strict prefix (substring matching would wrongly flag it).
    #[test]
    fn purge_stale_tablet_build_artifacts_invalidates_cached_import_dependents() {
        let dir = local_tempdir();
        let repo = dir.path();
        let tablet = repo.join("Tablet");
        let lib = repo.join(".lake/build/lib/lean/Tablet");
        let ir = repo.join(".lake/build/ir/Tablet");
        fs::create_dir_all(&tablet).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&ir).unwrap();

        // Live sources. `Pruned` and `AlsoPruned` were cone-cleaned (no
        // source), exercising multiple deletions in one burst. `PrunedExtra`
        // is live and has `Pruned` as a strict name prefix.
        for stem in [
            "Preamble",
            "Dep",
            "DepTwo",
            "Bystander",
            "PrunedExtra",
            "Broken",
        ] {
            fs::write(tablet.join(format!("{stem}.lean")), "-- source\n").unwrap();
        }

        let write_artifacts = |stem: &str| {
            fs::write(lib.join(format!("{stem}.olean")), b"olean").unwrap();
            fs::write(lib.join(format!("{stem}.ilean")), b"ilean").unwrap();
            fs::write(lib.join(format!("{stem}.olean.hash")), b"hash").unwrap();
            fs::write(ir.join(format!("{stem}.c")), b"c").unwrap();
        };
        for stem in [
            "Preamble",
            "Dep",
            "DepTwo",
            "Bystander",
            "PrunedExtra",
            "Broken",
            "Pruned",
            "AlsoPruned",
        ] {
            write_artifacts(stem);
        }
        // Cached import graphs. `Dep` references pruned `Tablet.Pruned` via
        // an `importArts`-style object key; `DepTwo` references pruned
        // `Tablet.AlsoPruned` via a plain string array (schema variation).
        // `Bystander` imports only live modules — including `Tablet.PrunedExtra`,
        // the substring trap. `Broken` has an unparseable setup.json and is
        // conservatively invalidated.
        fs::write(
            ir.join("Dep.setup.json"),
            r#"{"name":"Tablet.Dep","importArts":{"Tablet.Pruned":["x"],"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("DepTwo.setup.json"),
            r#"{"name":"Tablet.DepTwo","imports":["Tablet.AlsoPruned","Tablet.Preamble"]}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Bystander.setup.json"),
            r#"{"name":"Tablet.Bystander","importArts":{"Tablet.PrunedExtra":["x"],"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("PrunedExtra.setup.json"),
            r#"{"name":"Tablet.PrunedExtra","importArts":{"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Preamble.setup.json"),
            r#"{"name":"Tablet.Preamble","importArts":{}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Pruned.setup.json"),
            r#"{"name":"Tablet.Pruned","importArts":{"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(ir.join("Broken.setup.json"), "{not json").unwrap();

        purge_stale_tablet_build_artifacts(repo);

        // Pruned modules' own artifacts are gone from both build dirs.
        for stem in ["Pruned", "AlsoPruned"] {
            assert!(!lib.join(format!("{stem}.olean")).exists(), "{stem} olean");
            assert!(!lib.join(format!("{stem}.ilean")).exists(), "{stem} ilean");
            assert!(
                !lib.join(format!("{stem}.olean.hash")).exists(),
                "{stem} olean.hash"
            );
            assert!(!ir.join(format!("{stem}.c")).exists(), "{stem} ir .c");
        }
        assert!(!ir.join("Pruned.setup.json").exists(), "Pruned setup.json");
        // Dependents with a stale cached import graph are invalidated (they
        // rebuild from their surviving source), as is the unparseable one.
        for stem in ["Dep", "DepTwo", "Broken"] {
            assert!(
                !lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should be invalidated"
            );
            assert!(
                !ir.join(format!("{stem}.setup.json")).exists(),
                "{stem} setup.json should be invalidated"
            );
            assert!(
                !ir.join(format!("{stem}.c")).exists(),
                "{stem} ir .c should be invalidated"
            );
            assert!(
                tablet.join(format!("{stem}.lean")).exists(),
                "{stem} source must never be touched"
            );
        }
        // Unrelated modules survive untouched — including the substring trap.
        for stem in ["Preamble", "Bystander", "PrunedExtra"] {
            assert!(
                lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should survive"
            );
            assert!(
                ir.join(format!("{stem}.setup.json")).exists(),
                "{stem} setup.json should survive"
            );
            assert!(
                ir.join(format!("{stem}.c")).exists(),
                "{stem} ir .c should survive"
            );
        }
    }

    /// Edit-driven artifact invalidation (unitdistance cycle 696,
    /// reviewer-3116): an ACCEPTED worker edit to a Tablet source must drop
    /// (a) the edited module's own Lake build artifacts in BOTH
    /// `lib/lean/Tablet/` and `ir/Tablet/` (the pre-edit olean is a phantom
    /// that bare `lake env lean` probes would import, displaying the
    /// pre-edit signature), and (b) the artifacts of dependents whose cached
    /// `ir/Tablet/<m>.setup.json` import graph references an edited module.
    /// Unrelated modules' artifacts must survive, including a dependent that
    /// imports a live untouched module whose name has an edited module's
    /// name as a strict prefix (substring matching would wrongly flag it).
    /// A deleted-source stem in the same burst is swept by the same call
    /// (delete+edit in one burst), and re-running the purge is an idempotent
    /// no-op on the already-removed files.
    #[test]
    fn purge_invalidated_tablet_build_artifacts_invalidates_edited_stems_and_dependents() {
        let dir = local_tempdir();
        let repo = dir.path();
        let tablet = repo.join("Tablet");
        let lib = repo.join(".lake/build/lib/lean/Tablet");
        let ir = repo.join(".lake/build/ir/Tablet");
        fs::create_dir_all(&tablet).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&ir).unwrap();

        // Live sources. `Edited` was modified by the accepted burst (source
        // still on disk — unlike the deletion trigger). `EditedExtra` is
        // live, untouched, and has `Edited` as a strict name prefix.
        // `Gone` was deleted by the same burst (no source on disk).
        for stem in [
            "Preamble",
            "Edited",
            "Dep",
            "DepTwo",
            "Bystander",
            "EditedExtra",
        ] {
            fs::write(tablet.join(format!("{stem}.lean")), "-- source\n").unwrap();
        }

        let write_artifacts = |stem: &str| {
            fs::write(lib.join(format!("{stem}.olean")), b"olean").unwrap();
            fs::write(lib.join(format!("{stem}.ilean")), b"ilean").unwrap();
            fs::write(lib.join(format!("{stem}.olean.hash")), b"hash").unwrap();
            fs::write(ir.join(format!("{stem}.c")), b"c").unwrap();
        };
        for stem in [
            "Preamble",
            "Edited",
            "Dep",
            "DepTwo",
            "Bystander",
            "EditedExtra",
            "Gone",
        ] {
            write_artifacts(stem);
        }
        // Cached import graphs. `Dep` references edited `Tablet.Edited` via
        // an `importArts`-style object key; `DepTwo` references it via a
        // plain string array (schema variation). `Bystander` imports only
        // live untouched modules — including `Tablet.EditedExtra`, the
        // substring trap.
        fs::write(
            ir.join("Dep.setup.json"),
            r#"{"name":"Tablet.Dep","importArts":{"Tablet.Edited":["x"],"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("DepTwo.setup.json"),
            r#"{"name":"Tablet.DepTwo","imports":["Tablet.Edited","Tablet.Preamble"]}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Bystander.setup.json"),
            r#"{"name":"Tablet.Bystander","importArts":{"Tablet.EditedExtra":["x"],"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("EditedExtra.setup.json"),
            r#"{"name":"Tablet.EditedExtra","importArts":{"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Preamble.setup.json"),
            r#"{"name":"Tablet.Preamble","importArts":{}}"#,
        )
        .unwrap();
        // The edited module's own setup.json references only Preamble; it
        // is purged via the direct invalidated-stem clause, not the
        // dependent scan.
        fs::write(
            ir.join("Edited.setup.json"),
            r#"{"name":"Tablet.Edited","importArts":{"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();

        let edited: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::from(["Edited".to_string()]);
        purge_invalidated_tablet_build_artifacts(repo, &edited);

        // The edited module's own artifacts are gone from both build dirs,
        // even though its source is still live; the same-burst deleted
        // stem's artifacts are swept by the same call.
        for stem in ["Edited", "Gone"] {
            assert!(!lib.join(format!("{stem}.olean")).exists(), "{stem} olean");
            assert!(!lib.join(format!("{stem}.ilean")).exists(), "{stem} ilean");
            assert!(
                !lib.join(format!("{stem}.olean.hash")).exists(),
                "{stem} olean.hash"
            );
            assert!(!ir.join(format!("{stem}.c")).exists(), "{stem} ir .c");
        }
        assert!(!ir.join("Edited.setup.json").exists(), "Edited setup.json");
        // Dependents whose cached import graph references the edited module
        // are invalidated (they rebuild from their surviving source).
        for stem in ["Dep", "DepTwo"] {
            assert!(
                !lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should be invalidated"
            );
            assert!(
                !ir.join(format!("{stem}.setup.json")).exists(),
                "{stem} setup.json should be invalidated"
            );
            assert!(
                !ir.join(format!("{stem}.c")).exists(),
                "{stem} ir .c should be invalidated"
            );
            assert!(
                tablet.join(format!("{stem}.lean")).exists(),
                "{stem} source must never be touched"
            );
        }
        // The edited module's SOURCE is never touched.
        assert!(
            tablet.join("Edited.lean").exists(),
            "edited source must never be touched"
        );
        // Unrelated modules survive untouched — including the substring trap.
        for stem in ["Preamble", "Bystander", "EditedExtra"] {
            assert!(
                lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should survive"
            );
            assert!(
                ir.join(format!("{stem}.setup.json")).exists(),
                "{stem} setup.json should survive"
            );
            assert!(
                ir.join(format!("{stem}.c")).exists(),
                "{stem} ir .c should survive"
            );
        }

        // Idempotence (delete+edit interplay with the part-1 rewind-path
        // purge): a second pass over the same stems — or the deletion-keyed
        // wrapper that a later rewind would run — must be a quiet no-op and
        // must not disturb the survivors.
        purge_invalidated_tablet_build_artifacts(repo, &edited);
        purge_stale_tablet_build_artifacts(repo);
        for stem in ["Preamble", "Bystander", "EditedExtra"] {
            assert!(
                lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should survive repeated purges"
            );
        }
    }

    /// Q7 (Codex 6/R2-7): the package-ready barrier parks LOUDLY until the
    /// approval record is durably locatable, then assembles the archive at
    /// the one deterministic path, verifies the re-read renamed bytes, and
    /// finalizes mechanically with the `PackageFinalizationRecord` payload.
    /// The approval lookup reads the persisted event-log record, so a
    /// rewound log with a surviving tag behaves identically
    /// (`archive_embeds_approval_record_after_log_rewind` below).
    #[test]
    fn reload_refreshes_derived_in_flight_request_fields() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 3;
        initial.request_seq = 1;
        initial.target_edit_mode = crate::model::TargetEditMode::Targeted;
        initial.active_node = Some("a".into());
        initial.in_flight_request = Some(Box::new(WrapperRequest {
            id: 1,
            kind: RequestKind::Worker,
            cycle: 3,
            worker_context: crate::model::WorkerContext {
                enabled: true,
                validation_kind: crate::model::WorkerValidationKind::TheoremTargeted,
                authorized_nodes: set(&["a"]),
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract::default(),
            current_present_nodes: BTreeSet::new(),
            current_node_kinds: BTreeMap::new(),
            ..WrapperRequest::default()
        }));
        let mut seeded = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        // The misorder guard rejects non-initial states with an empty
        // event log; seed one record so reload sees a consistent log.
        seeded
            .append_event_log(&ProtocolEvent::StartCycle, &[], None, None)
            .unwrap();
        drop(seeded);

        let runtime = SupervisorRuntime::load(paths).unwrap();
        let request = runtime
            .state()
            .in_flight_request
            .as_ref()
            .expect("reloaded worker request");
        assert_eq!(
            request.worker_acceptance.validation_kind,
            crate::model::WorkerValidationKind::TheoremTargeted
        );
        assert_eq!(
            request.worker_acceptance.validation_execution_plan,
            vec![
                crate::model::WorkerValidationExecutionPlanStep::TheoremTargetEditScope {
                    target: Some("a".into()),
                    initial_scope: set(&["a"]),
                },
                crate::model::WorkerValidationExecutionPlanStep::ScopedTablet {
                    allowed_nodes_mode:
                        crate::model::ScopedTabletAllowedNodesMode::PreviousOrExplicit,
                    explicit_nodes: set(&["a"]),
                },
            ]
        );
        assert_eq!(request.current_present_nodes, set(&["a", "b"]));
        assert_eq!(
            request.current_node_kinds.get("a"),
            Some(&crate::model::NodeKind::Proof)
        );
        assert_eq!(
            request.current_node_kinds.get("b"),
            Some(&crate::model::NodeKind::Definition)
        );
    }

    #[test]
    fn reload_sanitizes_persisted_checker_mismatch_rejection_reasons() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let raw_reason = format!(
            "{} worker={{\"snapshot\":\"{}\"}} supervisor={{\"errors\":[\"{}\"]}}",
            crate::model::CHECKER_MISMATCH_REJECTION_PREFIX,
            "w".repeat(600_000),
            "s".repeat(600_000)
        );
        let mut initial = base_state();
        initial.phase = Phase::ProofFormalization;
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 215;
        initial.request_seq = 1;
        initial.active_node = Some("a".into());
        initial.deterministic_worker_rejection_reasons = vec![raw_reason.clone()];
        initial.in_flight_request = Some(Box::new(WrapperRequest {
            id: 1,
            kind: RequestKind::Review,
            deterministic_worker_rejection_reasons: vec![raw_reason.clone()],
            review_contract: serde_json::json!({
                "request_summary": {
                    "deterministic_worker_rejection_reasons": [raw_reason.clone()],
                },
            }),
            ..WrapperRequest::default()
        }));
        let mut seeded = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        // The misorder guard rejects non-initial states with an empty
        // event log; seed one record so reload sees a consistent log.
        seeded
            .append_event_log(&ProtocolEvent::StartCycle, &[], None, None)
            .unwrap();
        drop(seeded);

        let runtime = SupervisorRuntime::load(paths).unwrap();
        let request = runtime
            .state()
            .in_flight_request
            .as_ref()
            .expect("reloaded review request");
        let reason = request
            .deterministic_worker_rejection_reasons
            .first()
            .expect("sanitized reason");

        assert_eq!(request.kind, RequestKind::Review);
        assert_eq!(
            runtime.state().deterministic_worker_rejection_reasons[0],
            raw_reason
        );
        assert!(reason.starts_with(crate::model::CHECKER_MISMATCH_REJECTION_PREFIX));
        assert!(!reason.contains("worker={"));
        assert!(!reason.contains("supervisor={"));
        assert!(reason.len() < 600);
        assert_eq!(
            request.review_contract["request_summary"]["deterministic_worker_rejection_reasons"],
            serde_json::json!(request.deterministic_worker_rejection_reasons.clone())
        );
    }

    #[test]
    fn invalid_worker_retry_restores_repo_worktree_before_next_support_prep() {
        on_production_sized_stack(invalid_worker_retry_restores_repo_worktree_before_next_support_prep_on_production_stack);
    }

    fn invalid_worker_retry_restores_repo_worktree_before_next_support_prep_on_production_stack() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        let check_path = repo.join(".trellis/scripts/check.py");
        fs::write(
            &check_path,
            "#!/usr/bin/env python3\nimport json, pathlib, sys\nrepo = pathlib.Path(__file__).resolve().parents[2]\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    preamble = (repo / 'Tablet/Preamble.lean').read_text()\n    if 'BROKEN_IMPORT' in preamble:\n        json.dump({'returncode': 1, 'stdout': '', 'stderr': 'broken preamble', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n        sys.exit(0)\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("rewrite check script");
        let mut perms = fs::metadata(&check_path)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&check_path, perms).expect("chmod script");
        let original_preamble =
            fs::read_to_string(repo.join("Tablet/Preamble.lean")).expect("read original preamble");
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 1;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Worker)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        let active_request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime
            .capture_active_worker_base_for_request(&runtime.state, &active_request)
            .expect("capture pre-worker baseline");
        fs::write(repo.join("Tablet/Preamble.lean"), "import BROKEN_IMPORT\n")
            .expect("write broken preamble");
        fs::write(
            repo.join("Tablet/orphan.lean"),
            "def orphan : True := True.intro\n",
        )
        .expect("write untracked orphan");
        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Worker(WorkerResponse {
            request_id: 1,
            cycle: 1,
            status: ResponseStatus::Ok,
            outcome: WorkerOutcome::Invalid,
            snapshot: runtime.state().live.clone(),
            difficulty_updates: BTreeMap::new(),
            ..WorkerResponse::default()
        })]);

        let outcome = runtime.step(&mut adapter).expect("retry should not fail");
        assert!(matches!(
            outcome.commands.as_slice(),
            [
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase,
                ProtocolCommand::IssueRequest { request },
            ] if request.kind == RequestKind::Worker
        ));
        assert_eq!(
            fs::read_to_string(repo.join("Tablet/Preamble.lean")).expect("restored preamble"),
            original_preamble
        );
        assert!(!repo.join("Tablet/orphan.lean").exists());
    }

    #[test]
    fn invalid_cleanup_retry_restores_pre_request_worker_base_before_next_support_prep() {
        on_production_sized_stack(invalid_cleanup_retry_restores_pre_request_worker_base_before_next_support_prep_on_production_stack);
    }

    fn invalid_cleanup_retry_restores_pre_request_worker_base_before_next_support_prep_on_production_stack() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        fs::write(
            repo.join("Tablet/c.lean"),
            "-- [TABLET NODE: c]\nimport Tablet.Preamble\n\ntheorem c : True := by\n  trivial\n",
        )
        .expect("write accepted c lean");
        fs::write(
            repo.join("Tablet/c.tex"),
            "\\begin{theorem}Synthetic accepted node c.\\end{theorem}\n",
        )
        .expect("write accepted c tex");
        // Commit c.lean/c.tex so they survive the pre-snapshot HEAD reset
        // that capture_active_worker_base_for_request now performs. Without
        // this commit, the new HEAD reset would wipe them as untracked
        // files before the next snapshot captures them.
        commit_all(&repo, "add c node");
        let check_path = repo.join(".trellis/scripts/check.py");
        fs::write(
            &check_path,
            "#!/usr/bin/env python3\nimport json, pathlib, sys\nrepo = pathlib.Path(__file__).resolve().parents[2]\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    node = repo / 'Tablet/c.lean'\n    if not node.exists():\n        json.dump({'returncode': 1, 'stdout': '', 'stderr': '[c]\\nno such file or directory\\n  file: Tablet/c.lean', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n        sys.exit(0)\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("rewrite check script");
        let mut perms = fs::metadata(&check_path)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&check_path, perms).expect("chmod script");

        let mut initial = base_state();
        initial.phase = Phase::TheoremStating;
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 1;
        initial.request_seq = 1;
        initial.live.present_nodes = set(&["Preamble", "a", "b", "c"]);
        initial.live.open_nodes = set(&["a", "b", "c"]);
        initial
            .node_kinds
            .insert("c".into(), crate::model::NodeKind::Proof);
        initial.deps.insert("c".into(), set(&["Preamble"]));
        initial.target_claims.insert("c".into(), BTreeSet::new());
        initial.normalize_all_structural_state();
        // b67ccbf: `validation_kind == Cleanup` is derived from an active
        // orphan-cleanup task (`orphan_cleanup_nodes` non-empty), not from the
        // in-flight request label alone. Park a task over the live orphans
        // (`b`, `c` are unsupported here) so the cleanup-validation pass — and
        // thus the Cleanup-labelled retry this test asserts — is reachable and
        // survives the reject_cleanup Leave path.
        initial.pending_task = Some(crate::model::PendingTask {
            task_blockers: BTreeSet::new(),
            node: initial.active_node.clone(),
            mode: initial.current_mode(),
            orphan_cleanup_nodes: set(&["b", "c"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: crate::model::WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: crate::model::WorkerWorkStyleHint::Restructure,
            consumed_global_repair_grant: false,
            node_retirement: None,
        });
        let mut request = initial.expected_request(1, RequestKind::Worker);
        request.worker_context.validation_kind = crate::model::WorkerValidationKind::Cleanup;
        request.current_present_nodes = initial.live.present_nodes.clone();
        initial.in_flight_request = Some(Box::new(request));

        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial.clone(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        // Simulate the active_worker_base capture that would have happened at
        // the end of the prior step() when the in-flight worker request was
        // issued. Under #54, kernel emits RestoreWorktreeToActiveWorkerBase
        // unconditionally on cleanup-retry rejection (see implementation
        // note in reject_cleanup_worker_response).
        let active_request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime
            .capture_active_worker_base_for_request(&runtime.state, &active_request)
            .expect("seed active worker base");

        struct DirtyInvalidCleanupAdapter {
            repo: PathBuf,
            snapshot: WorkingSnapshot,
        }

        impl WrapperAdapter for DirtyInvalidCleanupAdapter {
            fn dispatch(&mut self, _request: &WrapperRequest) -> Result<WrapperResponse, String> {
                fs::remove_file(self.repo.join("Tablet/c.lean")).map_err(|err| err.to_string())?;
                fs::remove_file(self.repo.join("Tablet/c.tex")).map_err(|err| err.to_string())?;
                Ok(WrapperResponse::Worker(WorkerResponse {
                    request_id: 1,
                    cycle: 1,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: self.snapshot.clone(),
                    ..WorkerResponse::default()
                }))
            }
        }

        let mut adapter = DirtyInvalidCleanupAdapter {
            repo: repo.clone(),
            snapshot: initial.live.clone(),
        };

        let outcome = runtime
            .step(&mut adapter)
            .expect("cleanup retry should not fail");
        // #54: cleanup-retry rejection emits [RestoreWorktreeToActiveWorkerBase,
        // IssueRequest{Worker}]. Disk gets restored so worker's destructive
        // delete doesn't leave state.live (still has `c`) and disk (lacks `c`)
        // out of sync.
        assert!(matches!(
            outcome.commands.as_slice(),
            [
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase,
                ProtocolCommand::IssueRequest { request },
            ] if request.kind == RequestKind::Worker
                && request.worker_context.validation_kind == crate::model::WorkerValidationKind::Cleanup
                && request.current_present_nodes.contains("c")
        ));
        assert!(repo.join("Tablet/c.lean").exists());
        assert!(repo.join("Tablet/c.tex").exists());
    }

    #[test]
    fn stuck_worker_retry_restores_repo_worktree_and_captures_snapshot() {
        on_production_sized_stack(stuck_worker_retry_restores_repo_worktree_and_captures_snapshot_on_production_stack);
    }

    fn stuck_worker_retry_restores_repo_worktree_and_captures_snapshot_on_production_stack() {
        // A Stuck worker that left dirty state on disk (out-of-scope edits,
        // partial proofs, whatever) used to leak its modifications across
        // bursts because the kernel only rolled back on Invalid/Malformed.
        // After the predicate broadening, Stuck triggers the same rollback +
        // last_invalid snapshot capture as Invalid does.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        let check_path = repo.join(".trellis/scripts/check.py");
        fs::write(
            &check_path,
            "#!/usr/bin/env python3\nimport json, sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("rewrite check script");
        let mut perms = fs::metadata(&check_path)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&check_path, perms).expect("chmod script");
        // The original (HEAD-committed) preamble that the worker request
        // baseline will restore to.
        let original_preamble =
            fs::read_to_string(repo.join("Tablet/Preamble.lean")).expect("read original preamble");
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 1;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Worker)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        let active_request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime
            .capture_active_worker_base_for_request(&runtime.state, &active_request)
            .expect("capture pre-worker baseline");
        // Simulate the failure mode that motivated this fix: the worker burst
        // left a sibling file modified out-of-scope after its baseline was
        // captured. The retry must discard both writes.
        fs::write(
            repo.join("Tablet/Preamble.lean"),
            "import OUT_OF_SCOPE_MODIFICATION\n",
        )
        .expect("write contract-violating preamble edit");
        fs::write(
            repo.join("Tablet/leftover_orphan.lean"),
            "def leftover : True := True.intro\n",
        )
        .expect("write untracked leftover");
        // The worker reports Stuck. Under the OLD contract the kernel
        // assumed the worker had reverted its changes; under the new
        // contract the kernel snapshots and rolls back unconditionally.
        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Worker(WorkerResponse {
            request_id: 1,
            cycle: 1,
            status: ResponseStatus::Ok,
            outcome: WorkerOutcome::Stuck,
            snapshot: runtime.state().live.clone(),
            difficulty_updates: BTreeMap::new(),
            ..WorkerResponse::default()
        })]);

        let outcome = runtime
            .step(&mut adapter)
            .expect("stuck step should not fail");
        // Stuck routes through a Worker retry first (continue_worker_retry
        // returns true while stuck-retries remain); only when retries are
        // exhausted does it begin_retry_review and emit Reviewer. With a
        // fresh state it's the retry path. Under #54 the kernel emits
        // [RestoreWorktreeToActiveWorkerBase, IssueRequest{Worker}].
        assert!(matches!(
            outcome.commands.as_slice(),
            [
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase,
                ProtocolCommand::IssueRequest { request },
            ] if request.kind == RequestKind::Worker
        ));
        // Disk MUST be back to baseline — the out-of-scope modification
        // was discarded, the leftover untracked file was cleaned.
        assert_eq!(
            fs::read_to_string(repo.join("Tablet/Preamble.lean")).expect("restored preamble"),
            original_preamble,
            "Stuck worker's out-of-scope Preamble edit should be rolled back"
        );
        assert!(
            !repo.join("Tablet/leftover_orphan.lean").exists(),
            "Stuck worker's untracked leftover should be cleaned"
        );
        // The pre-rollback Tablet snapshot MUST be preserved at the
        // last_invalid sidecar so the next worker's prompt can show
        // the prior attempt's WIP.
        let last_invalid_preamble =
            repo.join(".trellis-history/worker_state/last_invalid/Tablet/Preamble.lean");
        assert!(
            last_invalid_preamble.exists(),
            "Stuck snapshot should be captured to last_invalid sidecar"
        );
        assert_eq!(
            fs::read_to_string(&last_invalid_preamble).expect("read sidecar preamble"),
            "import OUT_OF_SCOPE_MODIFICATION\n",
            "sidecar should contain the worker's WIP, not the rolled-back baseline"
        );
        let last_invalid_metadata =
            repo.join(".trellis-history/worker_state/last_invalid/metadata.json");
        let metadata_text =
            fs::read_to_string(&last_invalid_metadata).expect("read sidecar metadata");
        assert!(
            metadata_text.contains("\"outcome\": \"Stuck\""),
            "metadata.json should record outcome=Stuck; got {metadata_text}"
        );
    }

    #[test]
    fn valid_response_rejected_by_kernel_rule_preserves_wip_snapshot() {
        // dec2flt request 938 (2026-07-03): a Valid, checker-passing
        // response was rejected post-hoc by the engine's live-orphan rule.
        // The rollback ran but no WIP snapshot was written (the old capture
        // gate keyed on non-Valid outcomes only), so the retry prompt
        // pointed at a nonexistent `last_invalid` and the work was lost.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 1;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Worker)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        fs::write(
            repo.join("Tablet/Preamble.lean"),
            "import VALID_BUT_REJECTED_WIP\n",
        )
        .expect("write wip");
        let response = WorkerResponse {
            request_id: 1,
            cycle: 1,
            status: ResponseStatus::Ok,
            outcome: WorkerOutcome::Valid,
            snapshot: runtime.state().live.clone(),
            ..WorkerResponse::default()
        };
        let event = ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(response),
        };
        let captured = runtime
            .capture_last_invalid_snapshot_for_event(&event)
            .expect("capture ok");
        assert!(
            captured.is_some(),
            "a Valid response must be captured pre-apply — the engine may still reject it"
        );
        // Simulate the engine's apply outcome: deterministic rejection of
        // the Valid response leaves the reasons non-empty (an accept would
        // clear them via clear_retry_context).
        runtime.state.deterministic_worker_rejection_reasons = vec![
            "valid worker response leaves NEW live orphan nodes: [\"b\"]"
                .into(),
        ];
        runtime
            .update_last_invalid_for_event(&event, captured.as_deref())
            .expect("update ok");
        let sidecar = repo.join(".trellis-history/worker_state/last_invalid/Tablet/Preamble.lean");
        assert!(
            sidecar.exists(),
            "kernel-rejected Valid WIP must survive in the last_invalid sidecar"
        );
        assert_eq!(
            fs::read_to_string(&sidecar).expect("read sidecar"),
            "import VALID_BUT_REJECTED_WIP\n"
        );
        let metadata = fs::read_to_string(
            repo.join(".trellis-history/worker_state/last_invalid/metadata.json"),
        )
        .expect("read metadata");
        assert!(metadata.contains("live orphan nodes"), "{metadata}");
        assert!(metadata.contains("\"outcome\": \"Valid\""), "{metadata}");

        // An ACCEPTED Valid response (reasons cleared by the engine)
        // discards the capture and removes the stale sidecar.
        runtime.state.deterministic_worker_rejection_reasons.clear();
        let captured2 = runtime
            .capture_last_invalid_snapshot_for_event(&event)
            .expect("capture ok");
        runtime
            .update_last_invalid_for_event(&event, captured2.as_deref())
            .expect("update ok");
        assert!(
            !repo
                .join(".trellis-history/worker_state/last_invalid")
                .exists(),
            "an accepted Valid response must clear the sidecar"
        );
    }

    #[test]
    fn illegal_reset_review_response_does_not_modify_repo_disk() {
        // #54: under the new ProtocolCommand-driven restore, the runtime
        // only mutates disk when the kernel emits a RestoreWorktree*
        // command. A reviewer response with `reset: LastCommit` against
        // a request whose `allowed_resets` is `{None}` is rejected as
        // illegal by `review_response_legal`; the kernel reissues Review
        // and emits NO restore command. Disk MUST be left untouched.
        // (Pre-#54 the runtime restored disk anyway via the
        // event-shape-based `restore_repo_worktree_for_event`, leading
        // to silent state-vs-disk divergence — the bug #54 fixes.)
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        fs::write(
            repo.join("Tablet/a.tex"),
            "\\begin{theorem}changed\\end{theorem}\n",
        )
        .expect("dirty tracked tex");
        fs::write(repo.join("Tablet/temp.tex"), "temporary\n").expect("write untracked temp");
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::LastCommit,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);

        let outcome = runtime.step(&mut adapter).expect("review should succeed");
        // Kernel rejected the response as illegal → reissues Review.
        // No RestoreWorktree* command anywhere in the vec.
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
        // Disk MUST be untouched — the worker's WIP is preserved.
        assert_eq!(
            fs::read_to_string(repo.join("Tablet/a.tex")).expect("read tex"),
            "\\begin{theorem}changed\\end{theorem}\n",
        );
        assert!(repo.join("Tablet/temp.tex").exists());
    }

    #[test]
    fn malformed_review_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::Review(ReviewResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                ..ReviewResponse::default()
            }),
            WrapperResponse::Review(ReviewResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                decision: ReviewDecisionKind::Continue,
                comments: String::new(),
                task_blockers: BTreeSet::new(),
                override_blockers: BTreeSet::new(),
                reset_blockers: BTreeSet::new(),
                next_active: Some("a".into()),
                reset: crate::model::ResetChoice::None,
                next_mode: TaskMode::Global,
                difficulty_updates: BTreeMap::new(),
                clear_human_input: false,
                ..ReviewResponse::default()
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed review should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued review request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued review should succeed");
        assert!(second
            .commands
            .iter()
            .any(|command| matches!(command, ProtocolCommand::CommitCheckpoint)));
        assert_eq!(runtime.state().stage, crate::model::Stage::Start);
    }

    #[test]
    fn malformed_paper_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::VerifyPaper;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(Box::new(initial.expected_request(1, RequestKind::Paper)));
        let verifier_lanes = initial.verifier_lanes.clone();
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::Paper(PaperResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                ..PaperResponse::default()
            }),
            WrapperResponse::Paper(PaperResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                target_lane_updates: empty_corr_target_lanes(&verifier_lanes),
                node_lane_updates: BTreeMap::new(),
                reviewer_evidence: BTreeMap::new(),
                node_reviewer_evidence: BTreeMap::new(),
                ..PaperResponse::default()
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed paper should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Paper && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::VerifyPaper);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued paper request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued paper should succeed");
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert!(matches!(
            second.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn malformed_corr_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::VerifyCorr;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(Box::new(initial.expected_request(1, RequestKind::Corr)));
        let verifier_lanes = initial.verifier_lanes.clone();
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::Corr(CorrResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                ..CorrResponse::default()
            }),
            WrapperResponse::Corr(CorrResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                node_lane_updates: empty_corr_node_lanes(&verifier_lanes),
                target_lane_updates: empty_corr_target_lanes(&verifier_lanes),
                reviewer_evidence: BTreeMap::new(),
                rust_witness_artifact_correspondence: None,
                conditional_theorem_correspondence: None,
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed corr should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Corr && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::VerifyCorr);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued corr request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued corr should succeed");
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert!(matches!(
            second.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn malformed_sound_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::VerifySound;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.held_target = Some("a".into());
        initial.in_flight_request = Some(Box::new(initial.expected_request(1, RequestKind::Sound)));
        let verifier_lanes = initial.verifier_lanes.clone();
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::Sound(SoundResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                ..SoundResponse::default()
            }),
            WrapperResponse::Sound(SoundResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                lane_updates: empty_sound_lanes(&verifier_lanes),
                reviewer_evidence: BTreeMap::new(),
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed sound should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Sound && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::VerifySound);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued sound request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued sound should succeed");
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert!(matches!(
            second.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn malformed_human_gate_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::HumanGate;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.gate_kind = GateKind::NeedInput;
        initial.in_flight_request = Some(Box::new(
            initial.expected_request(1, RequestKind::HumanGate),
        ));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::HumanGate(HumanGateResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                choice: HumanChoice::Approve,
            }),
            WrapperResponse::HumanGate(HumanGateResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                choice: HumanChoice::Approve,
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed human gate should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::HumanGate && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::HumanGate);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued human gate request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued human gate should succeed");
        assert!(matches!(
            second.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("review request after human gate")
                .kind,
            RequestKind::Review
        );
    }

    #[test]
    fn checkpoint_written_on_commit_command() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        let outcome = runtime.step(&mut adapter).unwrap();
        assert!(outcome
            .commands
            .iter()
            .any(|command| matches!(command, ProtocolCommand::CommitCheckpoint)));
        assert!(paths.checkpoint_path.exists());
        let checkpoint: RuntimeCheckpoint =
            serde_json::from_str(&fs::read_to_string(paths.checkpoint_path).unwrap()).unwrap();
        assert_eq!(checkpoint.cycle, 4);
        assert_eq!(checkpoint.phase, Phase::TheoremStating);
    }

    /// A reviewer response with `clear_human_input = true` consumes the
    /// outstanding operator input; the runtime must archive the repo-root
    /// `HUMAN_INPUT.md` (cycle-stamped, under `.trellis-history/human-input/`)
    /// and truncate the root file so retracted operator prose does not
    /// linger indefinitely and mislead later readers.
    #[test]
    fn clear_human_input_archives_and_truncates_human_input_file() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let operator_prose = "# Operator input\n\nPlease hold node a until the lemma is split.\n";
        fs::write(repo.join("HUMAN_INPUT.md"), operator_prose).expect("write human input");
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.human_input_outstanding = true;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: true,
            ..ReviewResponse::default()
        })]);
        runtime.step(&mut adapter).expect("review should succeed");
        assert!(
            !runtime.state().human_input_outstanding,
            "the clear must have been applied"
        );
        // Root file truncated — no retracted prose left in the repo root.
        assert_eq!(
            fs::read_to_string(repo.join("HUMAN_INPUT.md")).expect("root file still exists"),
            "",
            "HUMAN_INPUT.md must be truncated once the input is consumed"
        );
        // Content archived cycle-stamped under the history area.
        let archive = repo
            .join(".trellis-history")
            .join("human-input")
            .join("cycle-000004.md");
        assert_eq!(
            fs::read_to_string(&archive).expect("archive file must exist"),
            operator_prose,
            "the consumed operator prose must be archived verbatim"
        );
    }

    /// Counterpart: a reviewer response that does NOT clear the outstanding
    /// input leaves `HUMAN_INPUT.md` untouched.
    #[test]
    fn review_without_clear_human_input_leaves_human_input_file_alone() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let operator_prose = "# Operator input\n\nStill relevant.\n";
        fs::write(repo.join("HUMAN_INPUT.md"), operator_prose).expect("write human input");
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.human_input_outstanding = true;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        runtime.step(&mut adapter).expect("review should succeed");
        assert_eq!(
            fs::read_to_string(repo.join("HUMAN_INPUT.md")).expect("root file"),
            operator_prose,
            "an unconsumed HUMAN_INPUT.md must be left untouched"
        );
        assert!(
            !repo.join(".trellis-history").join("human-input").exists(),
            "no archive may be written when the input was not consumed"
        );
    }

    /// Durability ordering: the HUMAN_INPUT.md archive/truncate must not
    /// outrun the checkpoint durability barrier. When the checkpoint sink
    /// fails, in-memory state rolls back to `human_input_outstanding =
    /// true` and the re-issued reviewer prompt names HUMAN_INPUT.md as the
    /// single source of truth — so the root file must still hold the
    /// operator prose verbatim, and no archive may exist.
    #[test]
    fn clear_human_input_leaves_file_intact_when_checkpoint_sink_fails() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let operator_prose = "# Operator input\n\nPlease hold node a until the lemma is split.\n";
        fs::write(repo.join("HUMAN_INPUT.md"), operator_prose).expect("write human input");
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.human_input_outstanding = true;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: true,
            ..ReviewResponse::default()
        })]);
        let mut sink = RecordingCheckpointSink {
            fail_with: Some("simulated sink failure".into()),
            ..RecordingCheckpointSink::default()
        };
        let error = runtime
            .step_with_checkpoint_sink(&mut adapter, &mut sink)
            .expect_err("a failing checkpoint sink must fail the step");
        assert!(
            matches!(error, RuntimeError::CheckpointSink(_)),
            "expected CheckpointSink error, got: {error:?}"
        );
        assert!(
            !sink.payloads.is_empty(),
            "test setup: the accepted review must have reached the sink"
        );
        assert!(
            runtime.state().human_input_outstanding,
            "in-memory state must roll back to the outstanding flag"
        );
        assert_eq!(
            fs::read_to_string(repo.join("HUMAN_INPUT.md")).expect("root file"),
            operator_prose,
            "HUMAN_INPUT.md must survive a failed step verbatim: the rolled-back \
             state re-prompts the reviewer to read it as the single source of truth"
        );
        assert!(
            !repo.join(".trellis-history").join("human-input").exists(),
            "no archive may be written for a step that did not durably commit"
        );
    }

    /// A `clear_human_input = true` response whose PRE-step state had no
    /// outstanding human input is not a clear transition and must not touch
    /// HUMAN_INPUT.md. The concrete trigger: a legality-REJECTED review
    /// (the runtime returns Ok with a re-issued request) — the flag is
    /// false both before and after the step, and any prose in the file
    /// (e.g. operator input written mid-cycle, not yet latched as
    /// outstanding) must survive.
    #[test]
    fn rejected_review_with_clear_human_input_leaves_human_input_file_alone() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let operator_prose = "# Operator input\n\nNot yet latched as outstanding.\n";
        fs::write(repo.join("HUMAN_INPUT.md"), operator_prose).expect("write human input");
        let mut initial = base_state();
        initial.phase = Phase::TheoremStating;
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.human_input_outstanding = false;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        // `Done` is not a legal TheoremStating review decision, so this
        // response is legality-rejected: the engine returns Ok with a
        // re-issued Review request and the step completes normally.
        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Done,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: None,
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: true,
            ..ReviewResponse::default()
        })]);
        runtime
            .step(&mut adapter)
            .expect("a legality-rejected review still steps Ok (re-issued request)");
        assert!(
            !runtime.state().latest_review_rejection_reasons.is_empty(),
            "test setup: the review must have been legality-rejected"
        );
        assert!(
            !runtime.state().human_input_outstanding,
            "the flag stays false across the rejected step"
        );
        assert_eq!(
            fs::read_to_string(repo.join("HUMAN_INPUT.md")).expect("root file"),
            operator_prose,
            "a response without a genuine outstanding->cleared transition must \
             not truncate HUMAN_INPUT.md"
        );
        assert!(
            !repo.join(".trellis-history").join("human-input").exists(),
            "no archive may be written without a genuine clear transition"
        );
    }

    #[test]
    fn checkpoint_sink_called_on_commit_command() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        let mut sink = RecordingCheckpointSink::default();

        runtime
            .step_with_checkpoint_sink(&mut adapter, &mut sink)
            .unwrap();
        assert_eq!(sink.payloads.len(), 1);
        assert_eq!(sink.payloads[0].checkpoint.cycle, 4);
        assert_eq!(
            sink.payloads[0].commands,
            vec![ProtocolCommand::CommitCheckpoint]
        );
    }

    fn git_in(repo: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Worktree restores must never rewind the append-only event log:
    /// `reset --hard HEAD` used to revert the dirty current-cycle file,
    /// tearing a hole in the dense index.
    #[test]
    fn restore_worktree_to_head_shields_event_log() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".trellis-history/event-log")).unwrap();
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "committed").unwrap();
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000001.jsonl"),
            "{\"index\":0}\n",
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c1"]);
        // Dirty both: Tablet edit must be reverted, event-log tail must survive.
        std::fs::write(repo.join("Tablet/A.lean"), "dirty").unwrap();
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000001.jsonl"),
            "{\"index\":0}\n{\"index\":1}\n",
        )
        .unwrap();
        restore_worktree_to_head(&repo).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("Tablet/A.lean")).unwrap(),
            "committed"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join(".trellis-history/event-log/cycle-000001.jsonl"))
                .unwrap(),
            "{\"index\":0}\n{\"index\":1}\n",
            "event-log appends must survive a HEAD restore"
        );
        assert!(
            !repo
                .join(".trellis-history/event-log.restore-shield")
                .exists(),
            "shield must be moved back, not left behind"
        );
    }

    /// Regression for the live loss (live run, 2026-07-04): entries
    /// materialized at audit acceptance are UNTRACKED until the next
    /// cycle-Start checkpoint commits them. In a rejection cycle the
    /// worker-retry restore (`RestoreWorktreeToActiveWorkerBase` /
    /// `RestoreWorktreeToHead` → `restore_worktree_to_head`) ran an
    /// unexcluded repo-root `git clean -fd`, deleting the entry files and
    /// INDEX.md while `process_memory_seq` kept its bumped value.
    /// Regression for the live loss (conn-isa run, 2026-08-17 10:21): the
    /// auto-rewind on a fingerprint-divergence at load ran an unexcluded
    /// repo-root `git clean -fd`, which deleted `isabelle/base/` and all 55
    /// untracked per-node `Tablet_<N>.thy` projections while leaving the two
    /// git-tracked files behind. The resulting half-scaffold made `isa-query`
    /// unrunnable for the agents and made every payload cache key fail to
    /// construct, so each corr fingerprint sweep re-probed all 54 nodes live.
    /// Nothing regenerates the scaffold implicitly.
    #[test]
    fn restore_worktree_to_head_spares_untracked_isabelle_session_scaffold() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        std::fs::create_dir_all(repo.join("isabelle")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.thy"), "committed").unwrap();
        // The two scaffold files that ARE tracked.
        std::fs::write(repo.join("isabelle/ROOT"), "session Tablet = HOL\n").unwrap();
        std::fs::write(
            repo.join("isabelle/Tablet_Preamble.thy"),
            "theory Tablet_Preamble imports Complex_Main begin end\n",
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c1"]);

        // The checker-owned UNTRACKED scaffold: the base session dir and the
        // per-node projections `sync_session` writes.
        std::fs::create_dir_all(repo.join("isabelle/base")).unwrap();
        std::fs::write(repo.join("isabelle/base/ROOT"), "session Tablet_Base\n").unwrap();
        std::fs::write(repo.join("isabelle/Tablet_A.thy"), "theory Tablet_A").unwrap();

        // A rejected burst's stray mutations must still be swept — including a
        // stray NESTED isabelle dir, since the exclusion is anchored to the
        // repo root exactly as the process-memory one is.
        std::fs::write(repo.join("Tablet/A.thy"), "dirty").unwrap();
        std::fs::write(repo.join("Tablet/Stray.thy"), "junk").unwrap();
        std::fs::create_dir_all(repo.join("Tablet/isabelle")).unwrap();
        std::fs::write(repo.join("Tablet/isabelle/forged.thy"), "x").unwrap();

        restore_worktree_to_head(&repo).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("Tablet/A.thy")).unwrap(),
            "committed",
            "tracked files must be restored to HEAD"
        );
        assert!(
            !repo.join("Tablet/Stray.thy").exists(),
            "untracked non-scaffold files must still be cleaned"
        );
        assert!(
            !repo.join("Tablet/isabelle/forged.thy").exists(),
            "the exclusion is repo-root anchored; a nested isabelle/ is still swept"
        );
        assert!(
            repo.join("isabelle/base/ROOT").is_file(),
            "the untracked base session dir must survive the restore"
        );
        assert!(
            repo.join("isabelle/Tablet_A.thy").is_file(),
            "untracked per-node projections must survive the restore"
        );
    }

    #[test]
    fn restore_worktree_to_head_spares_untracked_process_memory() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "committed").unwrap();
        // A pre-existing COMMITTED entry + INDEX: the audit-acceptance
        // rewrite of the tracked INDEX.md is an uncommitted modification
        // that `reset --hard` reverts, so the restore must regenerate it.
        crate::process_memory::apply_file_ops(
            &repo,
            &[crate::process_memory::ProcessMemoryFileOp::Add {
                entry_id: "pm-0001-old".into(),
                entry_type: "constraint".into(),
                coarse_node: "global".into(),
                title: "old".into(),
                body: "Committed-era constraint.".into(),
                cycle: 4,
                request_id: 17,
            }],
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c1"]);
        // Materialize an audit-authored entry via the same code path the
        // runtime's ApplyProcessMemoryOperations handler uses. Nothing
        // commits it yet — exactly the acceptance-to-checkpoint window.
        crate::process_memory::apply_file_ops(
            &repo,
            &[crate::process_memory::ProcessMemoryFileOp::Add {
                entry_id: "pm-0003-route-y".into(),
                entry_type: "refuted-route".into(),
                coarse_node: "ConeB".into(),
                title: "t".into(),
                body: "Route Y refuted; see cycle 5 audit.".into(),
                cycle: 5,
                request_id: 21,
            }],
        )
        .unwrap();
        // A rejected burst's stray mutations must still be swept — including
        // a stray NESTED process-memory dir (the exclusion is anchored to
        // the repo root).
        std::fs::write(repo.join("Tablet/A.lean"), "dirty").unwrap();
        std::fs::write(repo.join("Tablet/Stray.lean"), "junk").unwrap();
        std::fs::create_dir_all(repo.join("Tablet/process-memory")).unwrap();
        std::fs::write(repo.join("Tablet/process-memory/forged.md"), "x").unwrap();

        restore_worktree_to_head(&repo).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("Tablet/A.lean")).unwrap(),
            "committed",
            "tracked files must be restored to HEAD"
        );
        assert!(
            !repo.join("Tablet/Stray.lean").exists(),
            "untracked non-memory files must still be cleaned"
        );
        let entry = repo.join("process-memory/ConeB/pm-0003-route-y.md");
        assert!(
            entry.is_file(),
            "not-yet-checkpointed process-memory entries must survive the restore"
        );
        assert!(
            !repo.join("Tablet/process-memory").exists(),
            "nested stray process-memory dirs must still be swept (anchored exclusion)"
        );
        let index = std::fs::read_to_string(repo.join("process-memory/INDEX.md")).unwrap();
        assert!(
            index.contains("pm-0003-route-y") && index.contains("pm-0001-old"),
            "INDEX.md must be regenerated after the restore to list both the \
             committed entry and the untracked survivor (reset --hard reverts \
             the tracked INDEX to its committed content): {index}"
        );

        // A checkpoint-style commit (`git add -A`, see commit_checkpoint)
        // must pick the survivors up.
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "checkpoint"]);
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["ls-files", "process-memory"])
            .output()
            .unwrap();
        let tracked = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(tracked.contains("process-memory/ConeB/pm-0003-route-y.md"));
        assert!(tracked.contains("process-memory/INDEX.md"));
    }

    /// LastClean's `reset --hard <clean-tag>` + `clean -fd` must spare the
    /// event log: post-clean cycle files would otherwise be deleted outright.
    #[test]
    fn restore_last_clean_shields_event_log() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join(".trellis-history/event-log")).unwrap();
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "clean").unwrap();
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000001.jsonl"),
            "{\"index\":0}\n",
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "clean point"]);
        git_in(&repo, &["tag", "supervisor2/clean-000001"]);
        // Advance past the clean point: new committed cycle file + edits.
        std::fs::write(repo.join("Tablet/A.lean"), "post-clean").unwrap();
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000002.jsonl"),
            "{\"index\":1}\n",
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "post clean"]);
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000002.jsonl"),
            "{\"index\":1}\n{\"index\":2}\n",
        )
        .unwrap();

        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        runtime
            .restore_repo_worktree_to_last_clean(&repo, true)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("Tablet/A.lean")).unwrap(),
            "clean",
            "non-event-log files must be at the clean tag"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join(".trellis-history/event-log/cycle-000002.jsonl"))
                .unwrap(),
            "{\"index\":1}\n{\"index\":2}\n",
            "post-clean cycle files and dirty appends must survive LastClean"
        );
        assert!(!repo
            .join(".trellis-history/event-log.restore-shield")
            .exists());
    }

    /// Process memory (spec §7): shared harness — a clean-tagged commit
    /// WITHOUT `process-memory/`, then a later commit that adds an entry.
    fn build_process_memory_rewind_repo(dir: &tempfile::TempDir) -> (SupervisorRuntime, PathBuf) {
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "clean").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "clean point"]);
        git_in(&repo, &["tag", "supervisor2/clean-000001"]);
        // Advance: an audit materialized a process-memory entry.
        std::fs::write(repo.join("Tablet/A.lean"), "post-clean").unwrap();
        crate::process_memory::apply_file_ops(
            &repo,
            &[crate::process_memory::ProcessMemoryFileOp::Add {
                entry_id: "pm-0001-route-x".into(),
                entry_type: "refuted-route".into(),
                coarse_node: "ConeA".into(),
                title: "t".into(),
                body: "Route X refuted; counterexample inline.".into(),
                cycle: 3,
                request_id: 9,
            }],
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "post clean with memory"]);
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        (runtime, repo)
    }

    /// Process memory (spec §7): `preserve_process_memory=true` restores
    /// `process-memory/` from the pre-rewind HEAD after a LastClean reset.
    #[test]
    fn restore_last_clean_carries_process_memory_forward_when_preserving() {
        let dir = local_tempdir();
        let (runtime, repo) = build_process_memory_rewind_repo(&dir);
        runtime
            .restore_repo_worktree_to_last_clean(&repo, true)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("Tablet/A.lean")).unwrap(),
            "clean",
            "tracked files must be at the clean tag"
        );
        let entry = repo.join("process-memory/ConeA/pm-0001-route-x.md");
        assert!(
            entry.is_file(),
            "process-memory entry must be carried forward across LastClean"
        );
        assert!(repo.join("process-memory/INDEX.md").is_file());
    }

    /// Process memory (spec §7): `preserve_process_memory=false` (the
    /// poisoned-memory case) keeps existing behavior — the reset reverts
    /// memory with every other tracked file.
    #[test]
    fn restore_last_clean_drops_process_memory_when_not_preserving() {
        let dir = local_tempdir();
        let (runtime, repo) = build_process_memory_rewind_repo(&dir);
        runtime
            .restore_repo_worktree_to_last_clean(&repo, false)
            .unwrap();
        assert!(
            !repo.join("process-memory").exists(),
            "poisoned-memory rewind must revert process-memory/ to the clean tag"
        );
    }

    /// Extend the shared harness with a SECOND entry that is still
    /// untracked (materialized after the last checkpoint commit).
    fn add_untracked_entry(repo: &Path) {
        crate::process_memory::apply_file_ops(
            repo,
            &[crate::process_memory::ProcessMemoryFileOp::Add {
                entry_id: "pm-0002-route-y".into(),
                entry_type: "constraint".into(),
                coarse_node: "ConeA".into(),
                title: "t".into(),
                body: "Bound must stay below n/3.".into(),
                cycle: 4,
                request_id: 11,
            }],
        )
        .unwrap();
    }

    /// Process memory (spec §7): with `preserve_process_memory=true`, a
    /// not-yet-checkpointed (untracked) entry survives the LastClean
    /// rewind alongside the committed carry-forward, and INDEX.md is
    /// regenerated to list the union.
    #[test]
    fn restore_last_clean_keeps_untracked_process_memory_when_preserving() {
        let dir = local_tempdir();
        let (runtime, repo) = build_process_memory_rewind_repo(&dir);
        add_untracked_entry(&repo);
        runtime
            .restore_repo_worktree_to_last_clean(&repo, true)
            .unwrap();
        assert!(
            repo.join("process-memory/ConeA/pm-0001-route-x.md")
                .is_file(),
            "committed entry must be carried forward"
        );
        assert!(
            repo.join("process-memory/ConeA/pm-0002-route-y.md")
                .is_file(),
            "untracked entry must survive the LastClean clean sweep"
        );
        let index = std::fs::read_to_string(repo.join("process-memory/INDEX.md")).unwrap();
        assert!(index.contains("pm-0001-route-x]"));
        assert!(
            index.contains("pm-0002-route-y]"),
            "INDEX.md must be regenerated over the carried-forward + untracked union"
        );
    }

    /// Process memory (spec §7): with `preserve_process_memory=false`
    /// (poisoned memory) the rewind reverts memory to the clean tag —
    /// tracked entries via the reset, untracked ones removed by the sweep.
    #[test]
    fn restore_last_clean_removes_untracked_process_memory_when_not_preserving() {
        let dir = local_tempdir();
        let (runtime, repo) = build_process_memory_rewind_repo(&dir);
        add_untracked_entry(&repo);
        runtime
            .restore_repo_worktree_to_last_clean(&repo, false)
            .unwrap();
        assert!(
            !repo.join("process-memory").exists(),
            "poisoned-memory rewind must also remove untracked process-memory files"
        );
    }

    /// Bug 2 selection harness: build a LINEAR three-commit repo where the
    /// clean-tag suffixes are NON-MONOTONIC in real run time (a stale
    /// pre-segmentation `clean-003334` on an OLDER commit, a live
    /// post-segmentation `clean-001799` on a NEWER commit). Returns the
    /// runtime + repo path + the live (newer) commit SHA.
    fn build_nonmonotonic_clean_repo(
        dir: &tempfile::TempDir,
    ) -> (SupervisorRuntime, PathBuf, String) {
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        // c1: STALE clean checkpoint, high event_count suffix (lexical max).
        std::fs::write(repo.join("Tablet/A.lean"), "v1").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c1 stale clean"]);
        git_in(&repo, &["tag", "supervisor2/clean-003334"]);
        // c2: LIVE clean checkpoint, lower event_count suffix (post-segmentation).
        std::fs::write(repo.join("Tablet/A.lean"), "v2").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c2 live clean"]);
        git_in(&repo, &["tag", "supervisor2/clean-001799"]);
        let live_sha = {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        // c3: current HEAD (dirty work advanced past the live clean point).
        std::fs::write(repo.join("Tablet/A.lean"), "v3").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c3 head"]);
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        (runtime, repo, live_sha)
    }

    /// Bug 2 root-cause #1/#3: with no commit pointer in state, the
    /// rewind target must be the NEAREST HEAD-ancestor clean tag (the
    /// live `clean-001799`), NOT the lexically-highest stale
    /// `clean-003334`. The old `--sort=-refname` + `.first()` picked the
    /// stale tag — a 226-cycle catastrophic rollback.
    #[test]
    fn last_clean_target_picks_nearest_ancestor_not_lexical_max() {
        let dir = local_tempdir();
        let (runtime, repo, _live_sha) = build_nonmonotonic_clean_repo(&dir);
        assert!(runtime.state.last_clean_commit.is_none());
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        // git --sort=-refname yields the stale tag first (the old bug).
        assert_eq!(
            tags.first().map(String::as_str),
            Some("supervisor2/clean-003334")
        );
        let target = runtime.resolve_last_clean_commitish(&repo, &tags).unwrap();
        assert_eq!(
            target, "supervisor2/clean-001799",
            "must pick the nearest HEAD-ancestor clean tag, not the lexical-max stale tag"
        );
    }

    /// Bug 2 fix #1: a recorded `last_clean_commit` pointer that is an
    /// ancestor of HEAD is used verbatim, overriding tag selection.
    #[test]
    fn last_clean_target_prefers_commit_pointer() {
        let dir = local_tempdir();
        let (mut runtime, repo, live_sha) = build_nonmonotonic_clean_repo(&dir);
        runtime.state.last_clean_commit = Some(live_sha.clone());
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        let target = runtime.resolve_last_clean_commitish(&repo, &tags).unwrap();
        assert_eq!(target, live_sha, "commit pointer (HEAD ancestor) must win");
    }

    /// Round-2 fold-in: when the recorded `last_clean_commit` pointer is a
    /// HEAD ancestor but STALE relative to a newer clean tag (e.g. a later
    /// `rev-parse HEAD` failed so the pointer lagged), selection must take the
    /// MORE-RECENT (nearer-HEAD) target — the newer tag — not the stale
    /// pointer. This is the "prefer more-recent of {pointer, tag}" change over
    /// the round-1 "unconditionally prefer the pointer" behaviour.
    #[test]
    fn last_clean_target_prefers_more_recent_tag_over_stale_pointer() {
        let dir = local_tempdir();
        let (mut runtime, repo, _live_sha) = build_nonmonotonic_clean_repo(&dir);
        // Stale pointer = the OLDER clean checkpoint c1 (HEAD-ancestor, but two
        // commits behind); the newer clean tag c2 is one commit behind.
        let stale_sha = {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "supervisor2/clean-003334"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        runtime.state.last_clean_commit = Some(stale_sha);
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        let target = runtime.resolve_last_clean_commitish(&repo, &tags).unwrap();
        assert_eq!(
            target, "supervisor2/clean-001799",
            "must prefer the more-recent HEAD-ancestor target over the stale commit pointer"
        );
    }

    /// Bug 2 fix: a stale `last_clean_commit` that is NOT an ancestor of
    /// HEAD is ignored; selection falls back to the nearest ancestor tag.
    #[test]
    fn last_clean_target_ignores_non_ancestor_commit_pointer() {
        let dir = local_tempdir();
        let (mut runtime, repo, _live_sha) = build_nonmonotonic_clean_repo(&dir);
        runtime.state.last_clean_commit = Some("0000000000000000000000000000000000000000".into());
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        let target = runtime.resolve_last_clean_commitish(&repo, &tags).unwrap();
        assert_eq!(target, "supervisor2/clean-001799");
    }

    /// Bug 2 fix #2: when the only clean tag is on a SIBLING line (not an
    /// ancestor of HEAD) and there is no commit pointer, refuse to rewind
    /// (fail loud) rather than reset to a non-ancestor stale tag.
    #[test]
    fn last_clean_target_fails_loud_when_no_ancestor_tag() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "base").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "base"]);
        // Sibling branch carries the (stale) clean tag, NOT reachable from main.
        git_in(&repo, &["checkout", "-b", "sibling"]);
        std::fs::write(repo.join("Tablet/A.lean"), "sibling").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "sibling clean"]);
        git_in(&repo, &["tag", "supervisor2/clean-009999"]);
        git_in(&repo, &["checkout", "main"]);
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        assert_eq!(tags, vec!["supervisor2/clean-009999".to_string()]);
        let err = runtime
            .resolve_last_clean_commitish(&repo, &tags)
            .unwrap_err();
        match err {
            RuntimeError::InvalidRuntimeState(msg) => {
                assert!(msg.contains("no safe rewind target"), "got: {msg}");
            }
            other => panic!("expected InvalidRuntimeState, got {other:?}"),
        }
    }

    /// Operator directive (a): a STATE-INTEGRITY fault (the loaded
    /// `last_clean_*` mirrors claim readiness but git has zero clean tags)
    /// must FAIL LOUD at load — it must NOT silently rewind. This is the
    /// "state-fault path does NOT pick LastClean" guard.
    #[test]
    fn state_integrity_fault_fails_loud_does_not_rewind() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "x").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c1"]);
        // No supervisor2/clean-* tag exists, but state claims mirror readiness.
        let mut state = base_state();
        state.last_clean_verifier_mirror_ready = true;
        state.has_ever_been_clean = true;
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        let err = runtime.validate_last_clean_tag_consistency().unwrap_err();
        match err {
            RuntimeError::InvalidRuntimeState(msg) => {
                assert!(
                    msg.contains("zero `supervisor2/clean-*` tags"),
                    "got: {msg}"
                );
            }
            other => panic!("expected fail-loud InvalidRuntimeState, got {other:?}"),
        }
    }

    #[test]
    fn checkpoint_sink_failure_is_reported() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        let mut sink = RecordingCheckpointSink {
            payloads: Vec::new(),
            fail_with: Some("hook failed".into()),
        };

        let error = runtime
            .step_with_checkpoint_sink(&mut adapter, &mut sink)
            .expect_err("sink failure should bubble");
        assert!(matches!(error, RuntimeError::CheckpointSink(message) if message == "hook failed"));
    }

    #[test]
    fn load_rejects_state_claiming_clean_mirror_ready_when_git_has_no_clean_tag() {
        // Atomicity (audit, Option C): a state file with
        // last_clean_verifier_mirror_ready=true must be backed by at
        // least one supervisor2/clean-* tag in git. Otherwise a
        // future LastClean reset has nothing to rewind to. Fail at
        // load with an actionable error.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        init_git_repo(&repo);
        // No supervisor2/clean-* tag is created by init_git_repo —
        // the synthetic seed only produces a single root commit.
        let mut initial = base_state();
        // Simulate a state file that thinks a clean checkpoint exists.
        initial.last_clean_verifier_mirror_ready = true;
        initial.has_ever_been_clean = true;
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize_with_metadata should succeed (no validation there)");

        // load() runs the validator and should refuse.
        let result = SupervisorRuntime::load(paths);
        let err = match result {
            Ok(_) => panic!("load should refuse when state expects a clean tag git lacks"),
            Err(e) => e,
        };
        let RuntimeError::InvalidRuntimeState(msg) = err else {
            panic!("expected InvalidRuntimeState; got {err:?}");
        };
        assert!(msg.contains("supervisor2/clean-"), "msg={msg}");
        assert!(msg.contains("zero"), "msg={msg}");
    }

    #[test]
    fn load_accepts_state_with_clean_mirror_ready_when_git_has_clean_tag() {
        // Sanity counterpart: when git DOES have a clean tag, load
        // accepts the state. (Without this counterpart, the validator
        // could have a bug that always rejects.)
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        init_git_repo(&repo);
        // Create a fake clean tag.
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["tag", "supervisor2/clean-000001", "HEAD"])
            .output()
            .expect("git tag");
        let mut initial = base_state();
        initial.last_clean_verifier_mirror_ready = true;
        initial.has_ever_been_clean = true;
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        let _runtime = SupervisorRuntime::load(paths).expect("load should accept");
    }

    #[test]
    fn checkpoint_sink_failure_rolls_back_in_memory_state_and_state_file() {
        // Atomicity (audit): checkpoint sink failure must not advance
        // either in-memory state OR the persisted state file. Otherwise
        // a subsequent process start (with state file ahead of git)
        // would see LastCommit pointing at an OLD commit and LastClean
        // pointing at a clean tag that the sink never created, with
        // `last_clean_*` mirrors describing a state git doesn't hold.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        // Snapshot pre-step in-memory state and the on-disk state file.
        let pre_step_state = runtime.state().clone();
        let pre_step_state_file = fs::read_to_string(&paths.state_path)
            .expect("state file should exist after initialize");

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        let mut sink = RecordingCheckpointSink {
            payloads: Vec::new(),
            fail_with: Some("hook failed for atomicity test".into()),
        };

        let error = runtime
            .step_with_checkpoint_sink(&mut adapter, &mut sink)
            .expect_err("sink failure should bubble");
        assert!(matches!(error, RuntimeError::CheckpointSink(_)));

        // In-memory state restored to pre-step.
        assert_eq!(
            runtime.state(),
            &pre_step_state,
            "in-memory state must roll back to pre-step on sink failure",
        );
        // metadata.native_history_kinds also restored to pre-step.
        // record_native_history may have inserted (Review, phase) before
        // the sink ran; the rollback restores metadata to its pre-step
        // shape. Without this assertion, a regression that drops the
        // self.metadata = pre_step_metadata line wouldn't be caught.
        assert!(
            runtime.metadata.native_history_kinds.is_empty(),
            "metadata.native_history_kinds must roll back to pre-step \
             (was empty); got {:?}",
            runtime.metadata.native_history_kinds,
        );
        // State file untouched (the new persist_state runs AFTER sink success).
        let post_step_state_file =
            fs::read_to_string(&paths.state_path).expect("state file still readable");
        assert_eq!(
            post_step_state_file, pre_step_state_file,
            "state file must not be advanced when checkpoint sink fails",
        );
    }

    #[test]
    fn load_completes_checkpoint_committed_before_runtime_state_persist() {
        on_production_sized_stack(
            load_completes_checkpoint_committed_before_runtime_state_persist_on_production_stack,
        );
    }

    fn load_completes_checkpoint_committed_before_runtime_state_persist_on_production_stack() {
        struct CommitThenPanicSink {
            repo: PathBuf,
        }

        impl CheckpointSink for CommitThenPanicSink {
            fn commit(&mut self, payload: &CheckpointHookPayload) -> Result<(), String> {
                let history_dir = self.repo.join(".trellis-history");
                fs::create_dir_all(&history_dir).expect("history dir");
                fs::write(
                    history_dir.join("supervisor_state.json"),
                    serde_json::to_vec_pretty(&serde_json::json!({
                        "event_count": payload.event_count,
                        "metadata": payload.metadata,
                        "checkpoint": payload.checkpoint,
                        "state": payload.state,
                        "commands": payload.commands,
                    }))
                    .expect("serialize committed history"),
                )
                .expect("write committed history");
                git_in(&self.repo, &["add", "-A"]);
                git_in(
                    &self.repo,
                    &["commit", "-m", "checkpoint before injected crash"],
                );
                panic!("INJECTED_CRASH_AFTER_GIT_COMMIT_BEFORE_RUNTIME_PERSIST")
            }
        }

        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path().join("runtime"));
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        init_git_repo(&repo);

        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        // A mature runtime already has a dense prefix.  Seed one line so the
        // load-time segmentation guard is part of the exercised path.
        runtime
            .append_event_log(&ProtocolEvent::StartCycle, &[], None, None)
            .expect("seed pre-step event prefix");
        let pre_step = runtime.state().clone();

        let response = WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            next_active: Some("a".into()),
            next_mode: TaskMode::Global,
            ..ReviewResponse::default()
        });
        let mut adapter = QueueAdapter::new(vec![response]);
        let mut sink = CommitThenPanicSink { repo: repo.clone() };
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = runtime.step_with_checkpoint_sink(&mut adapter, &mut sink);
        }));
        assert!(
            interrupted.is_err(),
            "fixture must stop after the Git commit"
        );
        let committed_post_step = runtime.state().clone();
        assert_ne!(
            committed_post_step, pre_step,
            "fixture must advance in-memory state"
        );
        assert_eq!(
            serde_json::from_str::<ProtocolState>(
                &fs::read_to_string(&paths.state_path).expect("pre-step state remains")
            )
            .expect("parse pre-step state"),
            pre_step,
            "the injected interruption must precede runtime-state persistence",
        );
        // Also model a stop during the later JSONL append. Recovery may
        // discard only an authenticated prefix of this exact journaled line.
        let pending: PendingCheckpointTransaction = serde_json::from_slice(
            &fs::read(paths.root.join(CHECKPOINT_TRANSACTION_JOURNAL_FILENAME))
                .expect("read pending checkpoint journal"),
        )
        .expect("parse pending checkpoint journal");
        let event_bytes = serde_json::to_vec(&pending.event_record).unwrap();
        OpenOptions::new()
            .append(true)
            .open(event_log_cycle_file(&runtime.event_log_dir(), pending.event_record.cycle))
            .unwrap()
            .write_all(&event_bytes[..event_bytes.len() / 2])
            .unwrap();
        drop(runtime);

        let recovered = SupervisorRuntime::load(paths.clone())
            .expect("load must complete the Git-ahead checkpoint transaction");
        assert_eq!(recovered.state().stage, committed_post_step.stage);
        assert_eq!(recovered.state().phase, committed_post_step.phase);
        assert_eq!(
            recovered.state().request_seq,
            committed_post_step.request_seq
        );
        assert_eq!(
            recovered.event_count, 2,
            "the missing event is appended exactly once"
        );
        assert!(
            git_ref_targets_head(&repo, "supervisor2/checkpoint-000001").unwrap(),
            "recovery finishes a hook interrupted before checkpoint tagging"
        );
        assert!(
            git_ref_targets_head(&repo, "supervisor2/clean-000001").unwrap(),
            "recovery finishes the matching clean tag"
        );
        assert!(
            !paths
                .root
                .join("checkpoint_transaction.pending.json")
                .exists(),
            "recovery journal clears only after state and event persistence complete",
        );
    }

    #[test]
    fn load_validator_soft_no_ops_when_git_invocation_fails() {
        // Audit follow-up regression: the validator must NOT reject when
        // git is unavailable (binary missing, repo path can't be opened
        // by git, etc.). Prior to this fix, the helper collapsed
        // git-unavailable into "empty Vec" and the validator treated
        // that as "zero clean tags exist" → spurious rejection on
        // hosts/repos where git can't run.
        //
        // Use a path that's GUARANTEED not to exist as a directory.
        // git -C <nonexistent> exits with "fatal: cannot change to
        // '...': No such file or directory" (status 128) BEFORE any
        // ancestor .git discovery walks the filesystem. This avoids
        // the fragility of relying on /tmp being a separate filesystem
        // mount — on hosts where /tmp shares a filesystem with a
        // parent .git, git -C /tmp/<name> would succeed by walking up.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let nonexistent_repo = dir
            .path()
            .join("definitely-does-not-exist")
            .join("nor-does-this");
        assert!(
            !nonexistent_repo.exists(),
            "test precondition: repo path must not exist on disk so \
             git -C errors with 'cannot change to dir' before any \
             ancestor .git discovery",
        );
        let mut initial = base_state();
        initial.last_clean_verifier_mirror_ready = true;
        initial.has_ever_been_clean = true;
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(nonexistent_repo),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        SupervisorRuntime::load(paths).expect(
            "load must succeed when git is unavailable (helper returns Err) — \
             the validator soft-no-ops, not blame the state file",
        );
    }

    #[test]
    fn checkpoint_persist_failure_also_rolls_back_state_and_metadata() {
        // Audit follow-up: the prior rollback test only exercised the
        // sink.commit failure path. The persist_checkpoint failure
        // path's `self.state = pre_step_state; self.metadata =
        // pre_step_metadata;` lines were uncovered. Inject a failure
        // by pointing checkpoint_path at a path inside a nonexistent
        // directory — fs::write returns Err(NotFound) because the
        // parent doesn't exist.
        let dir = local_tempdir();
        let mut paths = RuntimePaths::new(dir.path());
        paths.checkpoint_path = dir
            .path()
            .join("nonexistent-parent-dir")
            .join("checkpoint.json");
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request =
            Some(Box::new(initial.expected_request(1, RequestKind::Review)));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let pre_step_state = runtime.state().clone();
        let pre_step_state_file = fs::read_to_string(&paths.state_path).unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        // NoopCheckpointSink — the failure must come from
        // persist_checkpoint, not from the sink.
        let mut sink = NoopCheckpointSink;

        let error = runtime
            .step_with_checkpoint_sink(&mut adapter, &mut sink)
            .expect_err("persist_checkpoint failure should bubble");
        assert!(
            matches!(error, RuntimeError::Io(_)),
            "expected Io error from fs::write to nonexistent parent; got {error:?}",
        );

        // Both state and metadata rolled back from this distinct
        // failure path (separate from the sink-commit failure path).
        assert_eq!(
            runtime.state(),
            &pre_step_state,
            "state must roll back on persist_checkpoint failure",
        );
        assert!(
            runtime.metadata.native_history_kinds.is_empty(),
            "metadata.native_history_kinds must roll back on persist_checkpoint \
             failure (was empty); got {:?}",
            runtime.metadata.native_history_kinds,
        );
        assert_eq!(
            fs::read_to_string(&paths.state_path).unwrap(),
            pre_step_state_file,
            "state file must not advance on persist_checkpoint failure",
        );
    }

    /// Build a `.trellis-history/supervisor_state.json` payload mirroring
    /// the supervisor's git checkpoint hook output. Only the
    /// `state.coarse_dag_nodes` field is consumed by the heal, but we mirror
    /// the surrounding shape so a future change to the recovery logic
    /// (e.g., reading metadata too) doesn't quietly break.
    fn write_history_state(repo: &Path, coarse_dag_nodes: &[&str]) {
        let history_dir = repo.join(".trellis-history");
        fs::create_dir_all(&history_dir).expect("create .trellis-history dir");
        let payload = serde_json::json!({
            "event_count": 0,
            "metadata": {},
            "checkpoint": {},
            "state": {
                "phase": "ProofFormalization",
                "coarse_dag_nodes": coarse_dag_nodes,
            },
            "commands": [],
        });
        fs::write(
            history_dir.join("supervisor_state.json"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .expect("write history supervisor_state.json");
    }

    /// Same artifact in the `trellis-shared-state/1` form the checkpoint hook
    /// writes once `history.shared_state` is on: `coarse_dag_nodes` reaches
    /// the reader only through a `$pool` reference whose members are interned
    /// strings. Hand-built, because `trellis/history_artifacts.py` is the only
    /// encoder; the obligation under test is that the git-history recovery
    /// paths decode before their typed read, and a reader that skipped the
    /// decode would deserialize `{"$r": ...}` into a `BTreeSet<NodeId>` and
    /// fail rather than silently pass.
    fn write_encoded_history_state(
        repo: &Path,
        coarse_dag_nodes: &[&str],
        extra_state: serde_json::Value,
    ) {
        let history_dir = repo.join(".trellis-history");
        fs::create_dir_all(&history_dir).expect("create .trellis-history dir");
        let mut strings = serde_json::Map::new();
        let mut members = Vec::new();
        for (index, node) in coarse_dag_nodes.iter().enumerate() {
            let key = format!("{:016x}", index + 1);
            strings.insert(key.clone(), serde_json::Value::String((*node).to_string()));
            members.push(serde_json::json!({ "$s": key }));
        }
        let nodes_key = "ffffffffffffffff";
        let mut state = match extra_state {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        state.insert(
            "phase".into(),
            serde_json::Value::String("ProofFormalization".into()),
        );
        state.insert(
            "coarse_dag_nodes".into(),
            serde_json::json!({ "$r": nodes_key }),
        );
        let payload = serde_json::json!({
            "$format": "trellis-shared-state/1",
            "event_count": 0,
            "metadata": {},
            "commands": [],
            "$strings": serde_json::Value::Object(strings),
            "$pool": { nodes_key: serde_json::Value::Array(members) },
            "checkpoint": {},
            "state": serde_json::Value::Object(state),
        });
        fs::write(
            history_dir.join("supervisor_state.json"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .expect("write encoded history supervisor_state.json");
    }

    fn git_commit_all(repo: &Path, message: &str) {
        let add = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", "-A"])
            .status()
            .expect("git add");
        assert!(add.success(), "git add failed");
        let commit = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["commit", "-m", message])
            .status()
            .expect("git commit");
        assert!(commit.success(), "git commit failed");
    }

    #[test]
    fn load_recovers_coarse_dag_from_git_history_when_state_field_is_empty() {
        // Mirrors the production failure mode: a manual rewind landed us in
        // ProofFormalization with empty coarse_dag_nodes, but a prior
        // checkpoint commit in git history still has the authentic value.
        // SupervisorRuntime::load must transparently recover.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        init_git_repo(&repo);

        // Commit 2: history captures coarse_dag_nodes populated.
        write_history_state(&repo, &["Preamble", "MainProof", "DepLemma"]);
        git_commit_all(&repo, "supervisor2 checkpoint with populated coarse_dag");

        // Commit 3: a later checkpoint that LOST the field (mirrors the
        // post-rewind state). The heal must still pick up the populated
        // value from commit 2.
        write_history_state(&repo, &[]);
        git_commit_all(&repo, "supervisor2 checkpoint after rewind (empty)");

        // Initialize runtime with empty coarse_dag_nodes in protocol_state
        // and phase=ProofFormalization (heal precondition).
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.coarse_dag_nodes.clear();
        let metadata = RuntimeMetadata {
            repo_path: Some(repo.clone()),
            config_path: Some(repo.join("trellis.config.json")),
            native_history_kinds: BTreeSet::new(),
            initial_planning_seeded: false,
            ..RuntimeMetadata::default()
        };
        let runtime = SupervisorRuntime::initialize_with_metadata(paths.clone(), state, metadata)
            .expect("initialize runtime");
        // initialize doesn't run the heal; load does.
        drop(runtime);

        let healed = SupervisorRuntime::load(paths).expect("load runtime");
        assert_eq!(
            healed.state.coarse_dag_nodes,
            BTreeSet::from([
                NodeId::from("Preamble"),
                NodeId::from("MainProof"),
                NodeId::from("DepLemma"),
            ]),
            "expected git heal to recover the populated coarse_dag_nodes from history",
        );
    }

    #[test]
    fn load_recovers_coarse_dag_from_a_shared_state_encoded_history_blob() {
        // The startup-side reader of the writer flip. `load` runs the heal on
        // every startup, and a format problem here would surface as an empty
        // coarse DAG -- i.e. as a spurious divergence, not as a parse error.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        init_git_repo(&repo);

        write_encoded_history_state(
            &repo,
            &["Preamble", "MainProof", "DepLemma"],
            serde_json::json!({}),
        );
        git_commit_all(&repo, "supervisor2 checkpoint (shared-state encoded)");

        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.coarse_dag_nodes.clear();
        let metadata = RuntimeMetadata {
            repo_path: Some(repo.clone()),
            config_path: Some(repo.join("trellis.config.json")),
            native_history_kinds: BTreeSet::new(),
            initial_planning_seeded: false,
            ..RuntimeMetadata::default()
        };
        let runtime = SupervisorRuntime::initialize_with_metadata(paths.clone(), state, metadata)
            .expect("initialize runtime");
        drop(runtime);

        let healed = SupervisorRuntime::load(paths).expect("load runtime");
        assert_eq!(
            healed.state.coarse_dag_nodes,
            BTreeSet::from([
                NodeId::from("Preamble"),
                NodeId::from("MainProof"),
                NodeId::from("DepLemma"),
            ]),
            "the git heal must decode a shared-state blob before its typed read",
        );
    }

    #[test]
    fn theorem_stating_baseline_recovers_from_a_shared_state_encoded_history_blob() {
        // The other git-history reader that deserializes a whole
        // `ProtocolState` out of this artifact; it gates the theorem-stating
        // reset, and an undecoded blob would look like "no baseline in
        // history" rather than like a corrupt file.
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        init_git_repo(&repo);

        let mut baseline = base_state();
        baseline.phase = Phase::ProofFormalization;
        baseline.live.present_nodes = set(&["Preamble", "MainProof"]);
        let baseline_json = serde_json::to_value(&baseline).expect("serialize baseline state");
        write_encoded_history_state(&repo, &["Preamble", "MainProof"], baseline_json);
        git_commit_all(
            &repo,
            "supervisor2 checkpoint (shared-state encoded baseline)",
        );

        let recovered =
            recover_theorem_stating_baseline_from_git(&repo).expect("recover baseline from git");
        assert_eq!(
            recovered.state.coarse_dag_nodes,
            BTreeSet::from([NodeId::from("Preamble"), NodeId::from("MainProof")]),
        );
        assert!(recovered
            .state
            .live
            .present_nodes
            .contains(&NodeId::from("MainProof")));
    }

    #[test]
    fn load_does_not_overwrite_already_populated_coarse_dag() {
        // Heal must be a no-op if the loaded state already has a value —
        // even if git history disagrees. The on-disk state is authoritative
        // when present.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        init_git_repo(&repo);
        write_history_state(&repo, &["DifferentNode"]);
        git_commit_all(&repo, "history with different coarse_dag");

        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.coarse_dag_nodes =
            BTreeSet::from([NodeId::from("OnDiskNode1"), NodeId::from("OnDiskNode2")]);
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(repo.join("trellis.config.json")),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        drop(runtime);

        let loaded = SupervisorRuntime::load(paths).expect("load runtime");
        assert_eq!(
            loaded.state.coarse_dag_nodes,
            BTreeSet::from([NodeId::from("OnDiskNode1"), NodeId::from("OnDiskNode2")]),
            "heal must not touch an already-populated coarse_dag_nodes",
        );
    }

    #[test]
    fn step_re_heals_coarse_dag_if_field_clears_mid_run() {
        // Defensive: if anything clears coarse_dag_nodes after load (a
        // future state-mutation path, a manual edit between steps), the
        // step boundary heal must recover it without needing a restart.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        init_git_repo(&repo);
        write_history_state(&repo, &["Preamble", "MainProof"]);
        git_commit_all(&repo, "supervisor2 checkpoint with populated coarse_dag");

        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        // Populate so load() leaves it alone.
        state.coarse_dag_nodes =
            BTreeSet::from([NodeId::from("Preamble"), NodeId::from("MainProof")]);
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(repo.join("trellis.config.json")),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        // Simulate the field being cleared mid-run (the failure mode this
        // hook exists to defend against).
        runtime.state.coarse_dag_nodes.clear();

        // step() must transparently re-heal before doing anything else.
        let mut adapter = QueueAdapter::new(vec![]);
        let _ = runtime.step(&mut adapter);
        assert_eq!(
            runtime.state.coarse_dag_nodes,
            BTreeSet::from([NodeId::from("Preamble"), NodeId::from("MainProof")]),
            "step boundary must re-heal coarse_dag_nodes if it gets cleared mid-run",
        );
    }

    #[test]
    fn load_heal_is_noop_when_no_git_history_available() {
        // Repo isn't a git repo (or has no checkpoint history). Heal must
        // fail soft — the field stays empty and the legacy
        // "treat all as coarse" fallback in runtime_cli_observations.rs
        // takes over.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        // NB: deliberately do NOT init_git_repo — git invocations will fail.

        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.coarse_dag_nodes.clear();
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(repo.join("trellis.config.json")),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        drop(runtime);

        let loaded = SupervisorRuntime::load(paths).expect("load runtime");
        assert!(
            loaded.state.coarse_dag_nodes.is_empty(),
            "no git history → heal must be a no-op, not crash and not populate from anywhere",
        );
    }

    #[test]
    fn event_log_appends_one_record_per_step() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![]);
        runtime.step(&mut adapter).unwrap();
        // The single step's start_cycle record lands in the NEW cycle's
        // per-cycle file (cycle 1), and the global event_count reconciles.
        let event_log_dir = runtime.event_log_dir();
        let cycle_file = event_log_cycle_file(&event_log_dir, runtime.state().cycle);
        let lines = fs::read_to_string(&cycle_file).unwrap();
        assert_eq!(lines.lines().count(), 1);
        assert_eq!(read_event_count(&event_log_dir).unwrap(), 1);
    }

    #[test]
    fn first_event_carries_complete_genesis_local_closure_coverage() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        init_git_repo(&repo);
        let owner = NodeId::from("a");
        let mut state = base_state();
        state
            .node_kinds
            .insert(owner.clone(), crate::model::NodeKind::Proof);
        let record = crate::model::LocalClosureRecord {
            node: owner.clone(),
            closure_version: "local-closure-v4".to_string(),
            toolchain_hash: "toolchain".to_string(),
            lean_executable_hash: "lean".to_string(),
            lake_executable_hash: "lake".to_string(),
            checker_script_hash: "checker".to_string(),
            lake_manifest_hash: "manifest".to_string(),
            preamble_hash: "preamble".to_string(),
            approved_axioms_hash: "axioms".to_string(),
            active_decl_hash: "decl".to_string(),
            active_statement_hash: "statement".to_string(),
            ..crate::model::LocalClosureRecord::default()
        };
        state
            .local_closure_records
            .insert(owner.clone(), record.clone());
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize covered genesis state");
        let mut adapter = QueueAdapter::new(Vec::new());
        let outcome = runtime.step(&mut adapter).expect("start first cycle");
        let carried = outcome.commands.iter().find_map(|command| match command {
            ProtocolCommand::InstallScheduledLocalClosureRecords { records } => Some(records),
            _ => None,
        });
        assert_eq!(
            carried.and_then(|records| records.get(&owner)),
            Some(&record),
            "the first event must make pre-dispatch genesis issuance replayable"
        );
        let lines = fs::read_to_string(event_log_cycle_file(&runtime.event_log_dir(), 1))
            .expect("read first event-log cycle");
        let logged: EventLogRecord =
            serde_json::from_str(lines.lines().next().expect("first log line"))
                .expect("parse first log line");
        assert!(logged.commands.iter().any(|command| matches!(
            command,
            ProtocolCommand::InstallScheduledLocalClosureRecords { records }
                if records.get(&owner) == Some(&record)
        )));
    }

    #[test]
    fn append_event_log_keys_files_on_cycle_and_reconciles_count() {
        // Direct writer/loader unit test (no engine drive): append records
        // spanning two cycles, assert per-cycle membership, dense in-order
        // concatenation, and that read_event_count sums across files.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        // Two records in cycle 1, one in cycle 2. `append_event_log` keys on
        // `state.cycle`, so set it before each append.
        runtime.state.cycle = 1;
        runtime
            .append_event_log(&ProtocolEvent::StartCycle, &[], None, None)
            .unwrap();
        runtime
            .append_event_log(&ProtocolEvent::StartCycle, &[], None, None)
            .unwrap();
        runtime.state.cycle = 2;
        runtime
            .append_event_log(&ProtocolEvent::StartCycle, &[], None, None)
            .unwrap();

        let event_log_dir = runtime.event_log_dir();
        let files = event_log_cycle_files(&event_log_dir).unwrap();
        assert_eq!(files.len(), 2, "one file per cycle");
        let c1 = fs::read_to_string(event_log_cycle_file(&event_log_dir, 1)).unwrap();
        let c2 = fs::read_to_string(event_log_cycle_file(&event_log_dir, 2)).unwrap();
        assert_eq!(c1.lines().count(), 2);
        assert_eq!(c2.lines().count(), 1);

        // Dense, in-order concatenation: indices 0,1,2 across the sorted files.
        let mut indices: Vec<u64> = Vec::new();
        for path in &files {
            for line in fs::read_to_string(path).unwrap().lines() {
                let record: EventLogRecord = serde_json::from_str(line).unwrap();
                indices.push(record.index);
            }
        }
        assert_eq!(indices, vec![0, 1, 2]);
        assert_eq!(read_event_count(&event_log_dir).unwrap(), 3);
    }

    #[test]
    fn read_event_count_fails_loud_on_index_gap() {
        // A gap in the global index (missing index 1) must fail loud rather
        // than silently returning a count that disagrees with max_index+1.
        let dir = local_tempdir();
        let event_log_dir = dir.path().join("event-log");
        fs::create_dir_all(&event_log_dir).unwrap();
        let mk = |index: u64, cycle: u32| {
            let record = EventLogRecord {
                index,
                event: ProtocolEvent::StartCycle,
                commands: vec![],
                phase: Phase::TheoremStating,
                stage: crate::model::Stage::Start,
                cycle,
                ts_ms: 0,
                trust_record: None,
                additional_trust_records: Vec::new(),
            };
            format!("{}\n", serde_json::to_string(&record).unwrap())
        };
        // indices 0 and 2 present, 1 missing → sum=2 but max_index=2.
        fs::write(
            event_log_cycle_file(&event_log_dir, 1),
            format!("{}{}", mk(0, 1), mk(2, 1)),
        )
        .unwrap();
        let err = read_event_count(&event_log_dir).unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidRuntimeState(_)),
            "index gap must surface as InvalidRuntimeState, got {err:?}"
        );
    }

    #[test]
    fn read_event_count_is_zero_for_absent_dir() {
        let dir = local_tempdir();
        let absent = dir.path().join("nope");
        assert_eq!(read_event_count(&absent).unwrap(), 0);
    }

    #[test]
    fn load_rejects_non_initial_state_with_absent_event_log() {
        // Segmentation misorder guard: a non-initial state (cycle >= 1)
        // with an absent/empty event-log dir means the binary was
        // launched before `segment_event_log` ran; appending would
        // restart the dense index at 0. Must fail loud, not cold-start.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let mut initial = base_state();
        initial.cycle = 490;
        SupervisorRuntime::initialize(paths.clone(), initial).unwrap();

        let err = match SupervisorRuntime::load(paths) {
            Ok(_) => panic!("absent event log with non-initial state must reject"),
            Err(err) => err,
        };
        let RuntimeError::InvalidRuntimeState(msg) = err else {
            panic!("expected InvalidRuntimeState; got {err:?}");
        };
        assert!(msg.contains("segment_event_log"), "msg={msg}");
    }

    fn seed_runtime_decide_pair(state: &mut ProtocolState, disprove_live: bool) {
        let primary = crate::model::ChallengeTargetId::from("correct");
        // Realistic spec shape: production seed-pinned specs ALWAYS carry
        // non-empty `lean` (config parse enforces it; twins get the
        // computed `¬T`), and `validate()` rejects a claimed seed-pinned
        // spec with empty `lean` (the D6/W2 invariant).
        state.configured_challenge_targets.insert(
            primary.clone(),
            crate::model::ChallengeTargetSpec {
                name: "Correct".into(),
                lean: "theorem Correct : True := by".into(),
                resolution: crate::model::ChallengeResolution::Decide,
                ..crate::model::ChallengeTargetSpec::default()
            },
        );
        state.configured_challenge_targets.insert(
            crate::model::refutation_target_id(&primary),
            crate::model::ChallengeTargetSpec {
                name: "Correct__Refutation".into(),
                lean: "theorem Correct__Refutation : ¬ (True) := by".into(),
                ..crate::model::ChallengeTargetSpec::default()
            },
        );
        if disprove_live {
            state
                .pv_live_polarity
                .insert(primary, crate::model::ChallengePolarity::Disprove);
        }
    }

    /// Stage 5 fixes (S5 audit finding 1): a Disprove-live REQUIRED-v1
    /// fixture must carry the flip-authorizing record class plus the
    /// enactment-latched applied context — `validate()` refuses a live
    /// Disprove without them.  Requires the state's seed-contract registry
    /// to already hold the target's entry (use
    /// `required_v1_runtime_fixture_with_contracts`); installs a
    /// kernel-stamped GiveUp record + latch, mirroring what a real
    /// enactment leaves behind.
    fn worker_inflight_runtime(
        runtime_root: &Path,
        repo: &Path,
        mut state: ProtocolState,
    ) -> SupervisorRuntime {
        state.stage = crate::model::Stage::Worker;
        // Cycle zero keeps these direct-runtime fixtures independent of the
        // event-log segmentation guard when a test exercises crash reload.
        state.cycle = 0;
        state.request_seq = 1;
        state.in_flight_request = Some(Box::new(state.expected_request(1, RequestKind::Worker)));
        let config_path = repo.join("trellis.config.json");
        SupervisorRuntime::initialize_with_metadata(
            RuntimePaths::new(runtime_root),
            state,
            RuntimeMetadata {
                repo_path: Some(repo.to_path_buf()),
                config_path: config_path.is_file().then_some(config_path),
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize worker runtime")
    }

    #[test]
    fn decide_layout_allows_config_only_load_but_rejects_partial_materialization() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        let mut state = base_state();
        seed_runtime_decide_pair(&mut state, false);
        let paths = RuntimePaths::new(dir.path().join("runtime"));

        // Definition seeding may precede source-file materialization during a
        // single fresh-init operation, so initialization accepts this
        // intermediate state.
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo),
                ..RuntimeMetadata::default()
            },
        )
        .expect("fresh init permits the pre-materialization intermediate");

        // Config-only state remains loadable so a worker can author the live
        // primary. No state snapshot claims either side and all eight pair
        // paths are absent.
        let runtime = SupervisorRuntime::load(paths.clone())
            .expect("wholly unmaterialized config-only Decide pair may load");
        drop(runtime);

        // The exemption ends as soon as any pair path appears: one lone file
        // is partial materialization and must fail before dispatch.
        fs::write(
            dir.path().join("repo/Tablet/Correct.lean"),
            "partial primary",
        )
        .unwrap();
        let error = match SupervisorRuntime::load(paths) {
            Ok(_) => panic!("post-init load must reject a partial Decide pair"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("invalid disk layout"),
            "unexpected load error: {error}"
        );
    }

    #[test]
    fn active_worker_restore_preserves_precheckpoint_decide_flip_on_first_relaunch() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("Dormant")).unwrap();
        for (dir_name, node, body) in [
            ("Tablet", "Correct", "POSITIVE"),
            ("Dormant", "Correct__Refutation", "REFUTATION"),
        ] {
            fs::write(repo.join(format!("{dir_name}/{node}.lean")), body).unwrap();
            fs::write(repo.join(format!("{dir_name}/{node}.tex")), body).unwrap();
        }
        // HEAD intentionally records the pre-flip layout. The kernel flip is
        // accepted before the next checkpoint, exactly as in the regression.
        init_git_repo(&repo);
        crate::dormant_store::flip_decide_pair_on_disk(
            &repo,
            &NodeId::from("Correct__Refutation"),
            &NodeId::from("Correct"),
        )
        .unwrap();
        write_test_config(&repo);

        let mut state = base_state();
        seed_runtime_decide_pair(&mut state, true);
        let runtime_root = dir.path().join("runtime");
        let runtime = worker_inflight_runtime(&runtime_root, &repo, state);
        let request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime
            .capture_active_worker_base_for_request(&runtime.state, &request)
            .unwrap();

        // Simulate a partial worker attempt plus sandbox-created reference/.
        fs::write(repo.join("Tablet/Correct__Refutation.lean"), "DIRTY").unwrap();
        fs::remove_file(repo.join("Tablet/Correct__Refutation.tex")).unwrap();
        fs::write(repo.join("Tablet/worker_orphan.lean"), "DIRTY").unwrap();
        fs::create_dir_all(repo.join("reference")).unwrap();
        fs::write(repo.join("reference/worker.tex"), "DIRTY").unwrap();

        // Loading must tolerate a partial in-flight Worker surface so the
        // pre-dispatch restore can repair it; non-Worker loads validate the
        // same malformed Decide layout immediately.
        drop(runtime);
        let runtime = SupervisorRuntime::load(RuntimePaths::new(&runtime_root))
            .expect("reload dirty in-flight Worker for fail-closed restore");
        assert!(runtime.restore_active_worker_base_for_inflight().unwrap());
        assert_eq!(
            fs::read_to_string(repo.join("Tablet/Correct__Refutation.lean")).unwrap(),
            "REFUTATION"
        );
        assert!(repo.join("Dormant/Correct.lean").is_file());
        assert!(!repo.join("Dormant/Correct__Refutation.lean").exists());
        assert!(!repo.join("Tablet/Correct.lean").exists());
        assert!(!repo.join("Tablet/worker_orphan.lean").exists());
        assert!(
            !repo.join("reference").exists(),
            "absence of a worker source surface is part of its baseline"
        );
    }

    #[test]
    fn load_recovers_interrupted_decide_flip_before_validation() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("Dormant")).unwrap();
        // Persisted polarity is Prove: primary live in Tablet/, refutation
        // dormant in Dormant/. Seed that valid layout, then tear the FIRST
        // forward rename toward Disprove — the new-live refutation `.lean` is
        // promoted to Tablet/ while its `.tex` is still dormant (an admitted
        // recovery prefix relative to the persisted polarity).
        for (dir_name, node, body) in [
            ("Tablet", "Correct", "PRIMARY"),
            ("Dormant", "Correct__Refutation", "REFUTATION"),
        ] {
            fs::write(repo.join(format!("{dir_name}/{node}.lean")), body).unwrap();
            fs::write(repo.join(format!("{dir_name}/{node}.tex")), body).unwrap();
        }
        fs::rename(
            repo.join("Dormant/Correct__Refutation.lean"),
            repo.join("Tablet/Correct__Refutation.lean"),
        )
        .unwrap();

        let mut state = base_state();
        seed_runtime_decide_pair(&mut state, false);
        let paths = RuntimePaths::new(dir.path().join("runtime"));
        // Init does not inspect the worktree; it just persists state + metadata.
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                ..RuntimeMetadata::default()
            },
        )
        .expect("init persists a Prove-live Decide pair");

        // NO in-flight Worker request, so load runs the recover→validate pair
        // at runtime.rs:449-455 end-to-end. Recovery repairs the torn forward
        // prefix back to the persisted polarity, then validation passes — so
        // load succeeds instead of failing on the transient layout.
        let runtime =
            SupervisorRuntime::load(paths).expect("torn forward prefix is recovered at load");

        let primary = NodeId::from("Correct");
        let refutation = NodeId::from("Correct__Refutation");
        // Disk matches the persisted Prove polarity, every file in exactly one
        // location.
        assert!(crate::dormant_store::node_in_tablet(&repo, &primary));
        assert!(!crate::dormant_store::node_in_dormant(&repo, &primary));
        assert!(crate::dormant_store::node_in_dormant(&repo, &refutation));
        assert!(!crate::dormant_store::node_in_tablet(&repo, &refutation));
        crate::dormant_store::validate_configured_decide_layout(&repo, &runtime.state)
            .expect("recovered layout validates against the persisted polarity");
    }

    /// Prose audit repair (finding 1): the prose lifecycle must survive the
    /// RUNTIME's configured-Decide layout gate, not only the pure
    /// transitions. Fail-before evidence (both halves):
    ///   * a worker CLAIM on the still-unbound authored pair made
    ///     `state_covered` true, so load/apply resolved the skeleton's empty
    ///     `name` to the degenerate `""`/`"__Refutation"` nodes and the gate
    ///     rejected every event — the lifecycle was dead in production;
    ///   * even given a loadable claim, the `StatementBound` event itself
    ///     could never commit: the post-event gate demanded the twin's
    ///     `Dormant/` files, which only a post-event CLI driver seeded.
    /// The bound pair (and every seed-pinned pair) stays validated exactly
    /// as before — the tail of this test proves the exemption did not widen.
    #[test]
    fn prose_claim_and_statement_binding_cross_the_runtime_layout_gate() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        let lean = "theorem GoalStmt (x : Nat) : x + 0 = x := by";
        fs::write(
            repo.join("Tablet/GoalStmt.lean"),
            format!("-- [TABLET NODE: GoalStmt]\n{lean}\n-- BODY\n  sorry\n"),
        )
        .unwrap();
        fs::write(
            repo.join("Tablet/GoalStmt.tex"),
            "\\begin{theorem}\nf returns its argument unchanged\n\\end{theorem}\n\\begin{proof}\nSKETCH:\n\\end{proof}\n",
        )
        .unwrap();

        // The engine-side acceptance-ready fixture (W4), on the runtime's
        // base state: unbound WorkerAuthored decide pair, exactly one
        // claimant with recorded corr/substantiveness/faithfulness passes.
        let mut state = base_state();
        state.cycle = 0;
        state.pv_tablet_configured = true;
        let primary = crate::model::ChallengeTargetId::from("goal:f");
        let node = NodeId::from("GoalStmt");
        state.configured_challenge_targets.insert(
            primary.clone(),
            crate::model::ChallengeTargetSpec {
                kind: crate::model::ChallengeTargetKind::Theorem,
                resolution: crate::model::ChallengeResolution::Decide,
                statement_provenance: crate::model::StatementProvenance::WorkerAuthored,
                ..crate::model::ChallengeTargetSpec::default()
            },
        );
        state.configured_challenge_targets.insert(
            crate::model::refutation_target_id(&primary),
            crate::model::ChallengeTargetSpec {
                kind: crate::model::ChallengeTargetKind::Theorem,
                statement_provenance: crate::model::StatementProvenance::KernelDerived,
                ..crate::model::ChallengeTargetSpec::default()
            },
        );
        state.live.present_nodes.insert(node.clone());
        state.proof_nodes.insert(node.clone());
        state.deps.insert(node.clone(), BTreeSet::new());
        state
            .challenge_claims
            .insert(node.clone(), BTreeSet::from([primary.clone()]));
        state.configured_targets.insert("f".into());
        state.target_claims.insert(node.clone(), set(&["f"]));
        state.paper_status.insert("f".into(), CorrStatus::Pass);
        state
            .live
            .paper_current_fingerprints
            .insert("f".into(), "goal-fp".into());
        state
            .paper_approved_fingerprints
            .insert("f".into(), "goal-fp".into());
        state.corr_status.insert(node.clone(), CorrStatus::Pass);
        state
            .live
            .corr_current_fingerprints
            .insert(node.clone(), "corr-goal".into());
        state
            .corr_approved_fingerprints
            .insert(node.clone(), "corr-goal".into());
        state
            .substantiveness_status
            .insert(node.clone(), SubstantivenessStatus::Pass);
        state
            .live
            .substantiveness_current_fingerprints
            .insert(node.clone(), "subst-goal".into());
        state
            .substantiveness_approved_fingerprints
            .insert(node.clone(), "subst-goal".into());
        state.normalize_all_structural_state();
        state.ensure_node_metadata();
        assert!(
            state.pv_statement_acceptance_ready(&primary).is_some(),
            "fixture must be acceptance-ready"
        );
        assert!(
            state
                .live
                .challenge_coverage
                .get(&primary)
                .is_some_and(|nodes| !nodes.is_empty()),
            "the claim must be live — exactly the coverage that used to close the exemption"
        );

        // (1) The CLAIMED, still-unbound pair crosses the load-time gate:
        // there is no pair layout to validate until the binding names one.
        // (Cycle 0 keeps this reload independent of the event-log
        // segmentation guard — the worker_inflight_runtime precedent.)
        let unbound_paths = RuntimePaths::new(dir.path().join("runtime-unbound"));
        SupervisorRuntime::initialize_with_metadata(
            unbound_paths.clone(),
            state.clone(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize claimed unbound prose runtime");
        drop(
            SupervisorRuntime::load(unbound_paths)
                .expect("a claimed, still-unbound authored pair must be loadable"),
        );

        // A second runtime at cycle 1 for the binding step (the event log
        // refuses cycle-0 appends; production bindings always run mid-cycle).
        state.cycle = 1;
        let paths = RuntimePaths::new(dir.path().join("runtime"));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize prose runtime for the binding step");

        // (2) `StatementBound` commits THROUGH the runtime's apply gate, and
        // the kernel twin's dormant files exist once it returns — seeded by
        // the transition's own command, before the gate saw the bound names.
        let binding = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(lean.as_bytes());
            crate::model::AuthoredStatementBinding {
                node: node.clone(),
                name: "GoalStmt".into(),
                lean: lean.into(),
                namespace_context: String::new(),
                informal: "f returns its argument unchanged".into(),
                statement_sha256: format!("{:x}", hasher.finalize()),
                imports: vec!["Tablet.Preamble".into()],
                opens: Vec::new(),
                bound_cycle: 0,
                generation: 1,
            }
        };
        let mut sink = NoopCheckpointSink;
        runtime
            .step_injected_event_with_checkpoint_sink(
                ProtocolEvent::StatementBound {
                    target: primary.clone(),
                    binding,
                },
                &mut sink,
            )
            .expect("the StatementBound event must commit through the runtime layout gate");
        assert!(
            runtime.state.pv_authored_statements.contains_key(&primary),
            "the binding latched"
        );
        assert!(repo.join("Dormant/GoalStmt__Refutation.lean").is_file());
        assert!(repo.join("Dormant/GoalStmt__Refutation.tex").is_file());

        // (3) The BOUND pair is validated exactly like a mode-B pair — the
        // complete two-location layout reloads...
        drop(runtime);
        SupervisorRuntime::load(paths.clone())
            .expect("bound pair with complete two-location layout loads");

        // (4) ...and the exemption did NOT widen to bound pairs: delete the
        // twin's dormant `.lean` and the load gate refuses.
        fs::remove_file(repo.join("Dormant/GoalStmt__Refutation.lean")).unwrap();
        let error = match SupervisorRuntime::load(paths) {
            Ok(_) => {
                panic!("a bound pair with a missing dormant twin must fail the layout gate")
            }
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("invalid disk layout"),
            "unexpected load error: {error}"
        );
    }

    /// Audit round 2, follow-up 1 — ORDERING. On a `TrustBaseMode::RequiredV1`
    /// run, `reconcile_trust_gate_record` ends in
    /// `ProtocolState::validate()`, and that is the FIRST gate a load reaches.
    /// While the stranded-Decide migration was invoked from the CLI *after*
    /// `SupervisorRuntime::load` returned, its `node_kinds` repair arm (and the
    /// `open_nodes` re-derivation guarding it) was unreachable on exactly the
    /// class of run it was written for: validate()'s Decide-pair value clause
    /// rejected the wrong kind before the repair could touch it. The migration
    /// now runs inside `load`, ahead of that validate(), so a REPAIRABLE
    /// checkpoint is repaired rather than refused.
    #[test]
    fn active_worker_restore_rolls_back_reference_tree_exactly() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("reference/nested")).unwrap();
        fs::write(repo.join("Tablet/a.lean"), "BASE").unwrap();
        fs::write(repo.join("reference/deviation.tex"), "BASE-REF").unwrap();
        fs::write(repo.join("reference/nested/note.tex"), "BASE-NESTED").unwrap();
        let runtime = worker_inflight_runtime(&dir.path().join("runtime"), &repo, base_state());
        let request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime
            .capture_active_worker_base_for_request(&runtime.state, &request)
            .unwrap();

        fs::write(repo.join("reference/deviation.tex"), "DIRTY").unwrap();
        fs::remove_file(repo.join("reference/nested/note.tex")).unwrap();
        fs::write(repo.join("reference/orphan.tex"), "DIRTY").unwrap();
        runtime.restore_active_worker_base_for_inflight().unwrap();

        assert_eq!(
            fs::read_to_string(repo.join("reference/deviation.tex")).unwrap(),
            "BASE-REF"
        );
        assert_eq!(
            fs::read_to_string(repo.join("reference/nested/note.tex")).unwrap(),
            "BASE-NESTED"
        );
        assert!(!repo.join("reference/orphan.tex").exists());
    }

    #[test]
    fn restore_active_worker_base_for_inflight_errs_when_snapshot_missing_for_worker() {
        // Audit followup: previously this returned Ok(false) silently when
        // the in-flight request was a Worker but no active-worker manifest
        // snapshot existed. The bridge discarded the boolean and proceeded
        // to rebuild `before_snapshot` against dirty disk — exactly the
        // baseline-poisoning hazard the restore call was supposed to
        // prevent. Now Errs so the bridge's KernelCliError handler routes
        // to a transport_failure classification.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("repo dir");
        let mut state = base_state();
        state.stage = crate::model::Stage::Worker;
        state.cycle = 1;
        state.request_seq = 1;
        state.in_flight_request = Some(Box::new(state.expected_request(1, RequestKind::Worker)));
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        // Deliberately do NOT seed active_worker_base/worker_surfaces.json.
        let result = runtime.restore_active_worker_base_for_inflight();
        let Err(err) = result else {
            panic!(
                "expected Err when in-flight Worker has no snapshot dir; \
                 got Ok({:?})",
                result.unwrap()
            );
        };
        let RuntimeError::InvalidRuntimeState(msg) = err else {
            panic!("expected InvalidRuntimeState; got {:?}", err);
        };
        assert!(
            msg.contains("active_worker_base/worker_surfaces.json"),
            "msg={msg}"
        );
        assert!(msg.contains("snapshot manifest is missing"), "msg={msg}");
    }

    /// `expected_request` leaves the dispatch pass's execution hints
    /// deliberately unresolved, so a state whose in-flight request carries
    /// them cannot pass `validate()` directly — the semantic projection must
    /// be checked instead. This is what lets a refresh run while a
    /// hint-bearing request is in flight.
    ///
    /// Verified live: the first prose statement binding to ever commit died in
    /// `refresh_in_flight_after_external_state_change` with "in-flight request
    /// payload does not match derived state" AFTER the binding had been
    /// durably applied.
    #[test]
    fn hint_decorated_request_needs_normalization_before_validate() {
        let mut state = base_state();
        state.stage = crate::model::Stage::VerifySound;
        state.cycle = 1;
        state.request_seq = 1;
        state.ensure_node_metadata();
        let mut request = state.expected_request(1, RequestKind::Sound);
        // Stand in for the dispatch pass, which decorates the persisted
        // request with fields the engine's derivation leaves unresolved.
        request.fresh_context = !request.fresh_context;
        state.in_flight_request = Some(Box::new(request));

        let decorated = state.validate();
        assert!(
            decorated.is_err(),
            "a hint-decorated request must not validate against the hint-free derivation"
        );
        assert!(
            decorated.unwrap_err().contains("in-flight request payload does not match derived state"),
            "the rejection must be the payload mismatch, not an unrelated invariant"
        );

        super::normalize_in_flight_request_execution_hints(&mut state);
        state
            .validate()
            .expect("the semantic projection validates once the hints are stripped");
        let refreshed = state.in_flight_request.as_ref().expect("still in flight");
        assert_eq!(refreshed.id, 1, "identity preserved");
        assert_eq!(refreshed.kind, RequestKind::Sound, "kind preserved");
    }

    #[test]
    fn legacy_stating_sidecar_window_is_normalized_before_validate() {
        let mut state = base_state();
        state.phase = Phase::TheoremStating;
        state.stage = crate::model::Stage::Reviewer;
        state.cycle = 1;
        state.request_seq = 1;
        let target = crate::model::TargetId::from("uncovered");
        state.configured_targets.insert(target.clone());
        state.live.coverage.insert(target.clone(), BTreeSet::new());
        state.committed.coverage.insert(target.clone(), BTreeSet::new());
        state
            .live
            .paper_current_fingerprints
            .insert(target.clone(), String::new());
        state
            .committed
            .paper_current_fingerprints
            .insert(target.clone(), String::new());
        state
            .paper_approved_fingerprints
            .insert(target, String::new());
        state.ensure_node_metadata();
        let mut request = state.expected_request(1, RequestKind::Review);
        assert!(request.sidecar_window_open);
        request.sidecar_window_open = false;
        state.in_flight_request = Some(Box::new(request));

        assert!(state.validate().is_err());
        super::normalize_in_flight_request_execution_hints(&mut state);
        state
            .validate()
            .expect("legacy derived window flag must refresh before validation");
        assert!(state
            .in_flight_request
            .as_deref()
            .expect("request remains in flight")
            .sidecar_window_open);
    }

    #[test]
    fn reachable_opaque_inventory_skew_is_normalized_before_validate() {
        let mut state = base_state();
        state.stage = crate::model::Stage::Worker;
        state.cycle = 1;
        state.request_seq = 1;
        state.ensure_node_metadata();
        state.pv_reachable_opaque_inventory = serde_json::json!({
            "schema": "trellis-reachable-opaque-inventory/v1",
            "unresolved_boundary_axioms": [{"name": "core.slice.first"}],
            "nodes": [{"node_id": "entry"}]
        });
        let mut request = state.expected_request(1, RequestKind::Worker);
        request.reachable_opaque_inventory = serde_json::Value::Null;
        state.in_flight_request = Some(Box::new(request));

        assert!(state
            .validate()
            .expect_err("skewed inventory must fail validation")
            .contains("in-flight request payload does not match derived state"));
        super::normalize_in_flight_request_execution_hints(&mut state);
        state
            .validate()
            .expect("normalization restores the state-derived inventory");
    }

    #[test]
    fn artifact_corr_projection_is_restored_during_in_flight_normalization() {
        let mut state = base_state();
        state.stage = crate::model::Stage::VerifyCorr;
        state.cycle = 1;
        state.request_seq = 1;
        let target = crate::model::ChallengeTargetId::from("goal:generic-artifact");
        let digest = crate::trust_base::raw_sha256(b"generic artifact binding");
        let reviewed = crate::trust_base::RustWitnessReviewedDigests {
            target_statement_sha256: digest,
            negated_statement_sha256: digest,
            negative_closure_sha256: digest,
            artifact_sha256: digest,
            receipt_sha256: digest,
        };
        let correspondence = crate::trust_base::RustWitnessCorrespondenceRequest {
            schema: crate::trust_base::RUST_WITNESS_CORRESPONDENCE_REQUEST_SCHEMA.into(),
            target_id: target.clone(),
            goal_target_prose_utf8: "generic target prose".into(),
            target_lean_utf8: "theorem Generic : True := by".into(),
            negated_target_lean_utf8: "theorem Generic__Refutation : ¬ True := by".into(),
            checked_negative_proof_closure: crate::model::LocalClosureRecord::default(),
            artifact_source_utf8: "#[test] fn witness() {}\n".into(),
            execution_receipt: serde_json::json!({"receipt_sha256": digest}),
            reviewed_digests: reviewed,
            request_sha256: digest,
        };
        state.trust_base.rust_witness_artifact_payloads.insert(
            target.clone(),
            crate::trust_base::RustWitnessArtifactPayload {
                source_utf8: correspondence.artifact_source_utf8.clone(),
                runner_request: serde_json::json!({}),
                execution_receipt: Some(correspondence.execution_receipt.clone()),
                correspondence_request: Some(correspondence.clone()),
                correspondence_verdict: None,
            },
        );
        state.trust_base.pending_rust_witness_correspondence_target = Some(target);
        let mut persisted = state.expected_request(1, RequestKind::Corr);
        assert_eq!(
            persisted.rust_witness_artifact_correspondence,
            Some(correspondence.clone())
        );
        persisted.rust_witness_artifact_correspondence = None;
        state.in_flight_request = Some(Box::new(persisted));

        super::normalize_in_flight_request_execution_hints(&mut state);
        assert_eq!(
            state
                .in_flight_request
                .as_ref()
                .unwrap()
                .rust_witness_artifact_correspondence,
            Some(correspondence)
        );
    }

    #[test]
    fn restore_active_worker_base_for_inflight_returns_false_for_benign_no_inflight() {
        // The Ok(false) path should still apply for the benign cases
        // (no in-flight request, non-Worker request, no metadata) — only
        // the in-flight-Worker + missing-snapshot case errs.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("repo dir");
        let state = base_state(); // in_flight_request = None
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        assert_eq!(
            runtime.restore_active_worker_base_for_inflight().unwrap(),
            false,
            "no in-flight request → Ok(false) (nothing to restore)",
        );
    }

    #[test]
    fn delete_persisted_local_closure_record_removes_existing_file() {
        // Patch C-O HIGH 1 (c) — the engine emits
        // `ProtocolCommand::DeleteLocalClosureRecord` after invalidating
        // a record. The runtime CLI handler removes the file under
        // `<runtime_root>/checker-state/local-closure-records/<node>.json`.
        // Verify the helper does that.
        let dir = local_tempdir();
        let runtime_root = dir.path();
        let records_dir = runtime_root
            .join("checker-state")
            .join("local-closure-records");
        fs::create_dir_all(&records_dir).expect("records dir");
        let file = records_dir.join("FooNode.json");
        fs::write(&file, r#"{"node":"FooNode"}"#).expect("write record");
        assert!(file.exists(), "precondition: record file must exist");

        delete_persisted_local_closure_record(runtime_root, &NodeId::from("FooNode"));

        assert!(
            !file.exists(),
            "DeleteLocalClosureRecord command must remove the persisted file"
        );
    }

    #[test]
    fn delete_persisted_local_closure_record_is_noop_when_file_missing() {
        // Patch C-O HIGH 1 (c) — missing file is not an error; the
        // engine emits the command at the moment of in-memory
        // invalidation, but no probe may have persisted a record yet.
        let dir = local_tempdir();
        let runtime_root = dir.path();
        // No records-dir created; the helper must NOT panic.
        delete_persisted_local_closure_record(runtime_root, &NodeId::from("Ghost"));
    }

    #[test]
    fn persisted_record_path_escapes_slash_consistently() {
        // Patch C-Q Q5 — both save (`bin/runtime_cli.rs:persist_record_to_disk`)
        // and delete (`delete_persisted_local_closure_record`) must use
        // the same on-disk filename mapping. The audit flagged a
        // pre-Q5 drift where save escaped `/` but delete did not — even
        // though current `NodeId`s don't contain `/`, the helper future-
        // proofs both sites. Verify the helper's escape behavior so a
        // future drift surfaces here.
        let dir = local_tempdir();
        let runtime_root = dir.path();
        let plain = NodeId::from("FooNode");
        let with_slash = NodeId::from("Group/Inner");
        let plain_path = persisted_record_path(runtime_root, &plain);
        let slash_path = persisted_record_path(runtime_root, &with_slash);
        assert_eq!(
            plain_path.file_name().and_then(|s| s.to_str()),
            Some("FooNode.json"),
            "plain node id keeps its name + .json suffix",
        );
        assert_eq!(
            slash_path.file_name().and_then(|s| s.to_str()),
            Some("Group_Inner.json"),
            "slash in node id is replaced with `_` for filesystem safety",
        );
        // File-name helper must match the path helper's last segment.
        assert_eq!(persisted_record_file_name(&plain), "FooNode.json",);
        assert_eq!(persisted_record_file_name(&with_slash), "Group_Inner.json",);
        // And the delete site must agree with the path: write a file
        // whose name matches `persisted_record_file_name`, ask the
        // delete helper to remove it, and confirm it actually went.
        let records_dir = runtime_root
            .join("checker-state")
            .join("local-closure-records");
        fs::create_dir_all(&records_dir).expect("records dir");
        let file = records_dir.join(persisted_record_file_name(&with_slash));
        fs::write(&file, r#"{"node":"Group/Inner"}"#).expect("write record");
        assert!(file.exists(), "precondition");
        delete_persisted_local_closure_record(runtime_root, &with_slash);
        assert!(
            !file.exists(),
            "delete helper must agree with persisted_record_file_name's escape",
        );
    }

    #[test]
    fn theorem_stating_node_reset_prunes_orphan_deleted_sidecar_queue_entry() {
        // Sidecar delta audit F1: this runtime sweep deletes orphan node
        // files and installs observed state OUTSIDE `apply_event`, then
        // calls `validate()` directly — so the deterministic queue prune
        // must run here too. Without it, a queued node orphan-deleted by
        // the sweep leaves a stale queue entry, `validate()` fails the
        // queue invariant, and a legitimate reviewer theorem_stating_node
        // reset downs the whole step.
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        for stale in ["a.lean", "a.tex", "b.lean", "b.tex"] {
            fs::remove_file(repo.join("Tablet").join(stale)).expect("drop seeded node file");
        }
        // The observation pass after the sweep needs `lean-semantic-payloads`
        // too; extend the seeded stub script with it.
        fs::write(
            repo.join(".trellis/scripts/check.py"),
            "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'lean-semantic-payloads':\n    json.dump({'A': {'ok': True, 'payload': 'root|A||const|Tablet.A|theorem|type=(const True)', 'error': ''}, 'X': {'ok': True, 'payload': 'root|X||const|Tablet.X|theorem|type=(const True)', 'error': ''}, 'Preamble': {'ok': False, 'payload': '', 'error': ''}}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("extend check script");
        let a_baseline = "import Tablet.Preamble\n-- [TABLET NODE: A]\ntheorem A : True := by\n-- BODY\n  sorry\n";
        fs::write(repo.join("Tablet/A.lean"), a_baseline).expect("write baseline A lean");
        fs::write(
            repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        )
        .expect("write A tex");
        // Theorem-stating baseline snapshot: ProofFormalization + a
        // non-empty coarse DAG containing the reset node.
        let mut baseline = ProtocolState::default();
        baseline.phase = crate::model::Phase::ProofFormalization;
        baseline.coarse_dag_nodes = set(&["A"]);
        baseline.configured_targets = set(&["t"]);
        baseline.proof_nodes = set(&["A"]);
        baseline.target_claims.insert("A".into(), set(&["t"]));
        baseline.live.present_nodes = set(&["Preamble", "A"]);
        baseline.live.open_nodes = set(&["A"]);
        baseline.live.coverage.insert("t".into(), set(&["A"]));
        baseline.committed = baseline.live.clone();
        fs::create_dir_all(repo.join(".trellis-history")).expect("history dir");
        fs::write(
            repo.join(".trellis-history/supervisor_state.json"),
            serde_json::json!({ "state": baseline }).to_string(),
        )
        .expect("write baseline snapshot");
        init_git_repo(&repo); // commit 1 = the theorem-stating baseline
                              // Live edit after the baseline: A leans on new helper X.
        let a_live = "import Tablet.Preamble\nimport Tablet.X\n-- [TABLET NODE: A]\ntheorem A : True := by\n-- BODY\n  exact X\n";
        fs::write(repo.join("Tablet/A.lean"), a_live).expect("write live A lean");
        fs::write(
            repo.join("Tablet/X.lean"),
            "import Tablet.Preamble\n-- [TABLET NODE: X]\ntheorem X : True := by\n-- BODY\n  sorry\n",
        )
        .expect("write X lean");
        fs::write(
            repo.join("Tablet/X.tex"),
            "\\begin{theorem}X\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        )
        .expect("write X tex");
        commit_all(&repo, "live: A leans on helper X");

        let config_path = write_test_config(&repo);
        let paths = RuntimePaths::new(dir.path().join("runtime"));
        let mut state = ProtocolState::default();
        state.phase = crate::model::Phase::ProofFormalization;
        state.coarse_dag_nodes = set(&["A"]);
        state.configured_targets = set(&["t"]);
        state.proof_nodes = set(&["A", "X"]);
        state.target_claims.insert("A".into(), set(&["t"]));
        state.deps.insert("A".into(), set(&["X"]));
        state.live.present_nodes = set(&["Preamble", "A", "X"]);
        state.live.open_nodes = set(&["X"]);
        state.live.coverage.insert("t".into(), set(&["A"]));
        state.committed = state.live.clone();
        // The reviewer queued X for a sidecar grunt.
        state.sidecar_queue.push(crate::model::SidecarQueueEntry {
            node: "X".into(),
            entry_seq: 7,
            queued_at_cycle: 3,
            origin: Default::default(),
        });
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                coverage_replanning_seeded: false,
                plan_review_cadence_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut next_state = runtime.state.clone();
        runtime
            .restore_theorem_stating_node_and_prune_orphans(&repo, &mut next_state, &"A".into())
            .expect("reset must succeed: the orphaned queue entry is pruned, not fatal");

        assert!(
            !next_state.live.present_nodes.contains(&NodeId::from("X")),
            "the orphan sweep must have deleted X"
        );
        assert!(
            next_state.sidecar_queue.is_empty(),
            "X's stale queue entry must be pruned before validate()"
        );
        let prune = next_state
            .sidecar_queue_prune_log
            .last()
            .expect("prune log must record the removal");
        assert_eq!(prune.node.as_str(), "X");
        assert_eq!(prune.entry_seq, 7);
        assert_eq!(prune.reason, "deleted");
    }
}

#[cfg(test)]
mod heartbeat_spool_tests {
    use super::*;

    fn local_tempdir() -> tempfile::TempDir {
        let tmp_root = std::env::current_dir()
            .expect("current dir")
            .join(".tmp-tests");
        fs::create_dir_all(&tmp_root).expect("tmp root");
        tempfile::tempdir_in(&tmp_root).expect("tempdir")
    }

    fn on_production_sized_stack(body: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .name("production-sized-stack-case".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(body)
            .expect("spawn production-sized-stack case")
            .join()
            .expect("production-sized-stack case");
    }

    #[test]
    fn drain_is_fail_open_consuming_and_idempotent() {
        // Missing directory — the pre-feature steady state — drains empty.
        let tmp = local_tempdir();
        let root = tmp.path();
        assert!(drain_pending_heartbeat_measurements(root).is_empty());

        // One good file, one garbage file, one non-json bystander. The good
        // entry is returned, BOTH .json files are consumed (garbage must not
        // be re-scanned forever), the bystander survives, and a second drain
        // is empty — so a measurement is delivered exactly once.
        let dir = heartbeat_measurements_dir(root);
        fs::create_dir_all(&dir).expect("spool dir");
        fs::write(
            dir.join("Good.json"),
            r#"{"node":"Good","heartbeats":286944,"heartbeats_key":"h1","extra":"ignored"}"#,
        )
        .expect("good file");
        fs::write(dir.join("Bad.json"), "not json at all").expect("bad file");
        fs::write(dir.join("notes.txt"), "bystander").expect("bystander");

        let drained = drain_pending_heartbeat_measurements(root);
        assert_eq!(drained.len(), 1);
        let entry = drained.get(&NodeId::from("Good")).expect("good entry");
        assert_eq!(entry.heartbeats, 286_944);
        assert_eq!(entry.heartbeats_key, "h1");
        assert!(!dir.join("Good.json").exists(), "delivered file consumed");
        assert!(!dir.join("Bad.json").exists(), "garbage consumed too");
        assert!(dir.join("notes.txt").exists(), "non-json left alone");
        assert!(drain_pending_heartbeat_measurements(root).is_empty());
    }
}

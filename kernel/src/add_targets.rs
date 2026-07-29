//! Add-targets mode: revive a COMPLETED run to state new paper targets.
//!
//! `add_paper_targets_to_state` is the pure state mutation behind the offline
//! `add_paper_targets` runtime-CLI action. It takes a run that finished the
//! full pipeline (`Phase::Complete` after Cleanup) and adds paper targets
//! (tex labels from the SAME paper) to it, reviving the run into the existing
//! `RevisionStating` phase so the new targets get stated through the normal
//! RevisionStating -> Advance HumanGate -> ProofFormalization -> Cleanup ->
//! Complete pipeline.
//!
//! Design invariants (audited scope):
//! - every existing approval is byte-untouched: corr / sound / paper /
//!   substantiveness approved fingerprints and statuses are not rewritten;
//!   the only state diffs are the whitelisted revival paths (configured
//!   targets + the derived per-target map entries, phase/stage, the fresh
//!   revision context + planning lane, progress history, the cleanup latch
//!   set, active-seat fields, edit modes, human_input_outstanding);
//! - the planner is ON: the revival seeds the revision-planning
//!   StuckMathAudit so the first post-revive cycle dispatches the planner;
//! - the cycle number and event log stay CONTINUOUS (unlike
//!   `import_revision_project`, which builds a FRESH run dir and therefore
//!   resets the cycle to 0);
//! - frozen nodes may gain `target_claim_updates` on ANY target
//!   (reuse-via-claim); duplication is forbidden by substantiveness, not by
//!   the freeze;
//! - existing targets do NOT re-verify when their nodes join new targets'
//!   closures (per-target fingerprints).

use crate::model::{
    CorrStatus, GateKind, NodeId, Phase, ProofEditMode, ProtocolState, RevisionCarriedApprovals,
    RevisionContext, RevisionKind, RevisionNodeDisposition, RevisionPlanningContext,
    RevisionTargetDelta,
    RevisionTargetDeltaKind, Stage, StuckMathAuditState, TargetEditMode, TargetId,
};
use crate::revision_import::compute_frozen_nodes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// One target the operator is adding, already resolved against the configured
/// paper (`resolve_main_result_targets` with `raw_targets = None` and the
/// union label list — labels-only re-resolution, amendment 1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddedTargetSpec {
    /// Configured `TargetId` for the new target — the bare tex label
    /// (mode-A configured ids ARE labels).
    pub target: TargetId,
    pub label: String,
    pub start_line: i64,
    pub end_line: i64,
}

/// Summary returned by `add_paper_targets_to_state`, surfaced in the CLI
/// response and useful for tests / operator inspection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddTargetsSummary {
    pub added_targets: Vec<TargetId>,
    pub total_targets: usize,
    pub frozen_nodes: usize,
    pub editable_nodes: usize,
    pub trigger: String,
    pub notes: Vec<String>,
}

pub fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Civil date (UTC) from a Unix timestamp in seconds, as `YYYY-MM-DD`.
/// Howard Hinnant's `civil_from_days`; no external time dependency.
pub fn iso_date_utc(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// True when the run is a program-verification run (`pv_tablet` configured).
/// Add-targets mode is a paper-target (mode-A math) feature; PV runs have no
/// paper-faithfulness bucket to grow.
fn is_pv_run(state: &ProtocolState) -> bool {
    state.pv_tablet_configured
        || !state.node_role.is_empty()
        || !state.pv_verification_target_prefixes.is_empty()
}

/// Assert every add-targets precondition. MUTATES NOTHING: called on the
/// still-pristine state before any field is written, and again (trivially)
/// by `add_paper_targets_to_state` itself. Error strings are stable —
/// the precondition-matrix tests match on their prefixes.
pub fn check_add_targets_preconditions(
    state: &ProtocolState,
    added: &[AddedTargetSpec],
) -> Result<(), String> {
    if state.phase != Phase::Complete {
        return Err(format!(
            "add_paper_targets requires a COMPLETED run (phase == Complete); found phase {:?}. \
             Finish (or rewind) the run first — reviving a mid-flight run is not supported.",
            state.phase
        ));
    }
    if state.in_flight_request.is_some() {
        return Err(
            "add_paper_targets requires no in-flight request; the loaded state carries one"
                .to_string(),
        );
    }
    if state.gate_kind != GateKind::None {
        return Err(format!(
            "add_paper_targets requires no active human gate; found gate_kind {:?}",
            state.gate_kind
        ));
    }
    if state.pending_task.is_some() {
        return Err("add_paper_targets requires no pending worker task".to_string());
    }
    if !state.pending_protected_reapproval_nodes.is_empty() {
        return Err(format!(
            "add_paper_targets requires no pending protected-reapproval nodes; found {:?}",
            state.pending_protected_reapproval_nodes
        ));
    }
    if is_pv_run(state) {
        return Err(
            "add_paper_targets is a paper-target (mode-A) feature and cannot run on a \
             program-verification run (pv_tablet configured)"
                .to_string(),
        );
    }
    if state.configured_targets.is_empty() {
        return Err(
            "add_paper_targets requires a mode-A run with a non-empty configured-target set; \
             this run has no configured paper targets"
                .to_string(),
        );
    }
    if added.is_empty() {
        return Err(
            "add_paper_targets: no new targets to add — every supplied label is already \
             configured (or the label list supplied none). Append the new tex labels to \
             workflow.main_result_labels and re-run."
                .to_string(),
        );
    }
    let mut seen: BTreeSet<&TargetId> = BTreeSet::new();
    for spec in added {
        if spec.label.trim().is_empty() {
            return Err(format!(
                "add_paper_targets: added target `{}` has an empty tex label",
                spec.target.as_str()
            ));
        }
        if spec.start_line <= 0 || spec.end_line <= 0 || spec.end_line < spec.start_line {
            return Err(format!(
                "add_paper_targets: added target `{}` resolved to non-positive/inverted paper \
                 block lines {}-{}; the label must resolve to a labeled statement block in the \
                 configured paper",
                spec.target.as_str(),
                spec.start_line,
                spec.end_line
            ));
        }
        if state.configured_targets.contains(&spec.target) {
            return Err(format!(
                "add_paper_targets: target `{}` is already configured; only NEW labels may be \
                 added",
                spec.target.as_str()
            ));
        }
        if !seen.insert(&spec.target) {
            return Err(format!(
                "add_paper_targets: target `{}` appears more than once in the added set",
                spec.target.as_str()
            ));
        }
    }
    Ok(())
}

/// Revive a `Phase::Complete` state into `RevisionStating` with `added` new
/// paper targets. Pure state mutation: no filesystem, no config IO. On `Err`
/// the state is untouched — the mutation runs on an internal clone and is
/// assigned back only after the final `validate()` passes, so BOTH the
/// precondition rejections and the (should-be-unreachable) validate failure
/// leave the caller's state byte-identical.
///
/// `paper_path` / `paper_sha` describe the configured paper (the SAME paper
/// the run was built against — old == new in the synthesized
/// `RevisionContext`); `source_id` is the provenance string
/// (`add-targets:<ISO date>`).
pub fn add_paper_targets_to_state(
    state: &mut ProtocolState,
    added: &[AddedTargetSpec],
    paper_path: &str,
    paper_sha: &str,
    source_id: &str,
) -> Result<AddTargetsSummary, String> {
    check_add_targets_preconditions(state, added)?;
    let mut candidate = state.clone();
    let summary =
        apply_add_paper_targets(&mut candidate, added, paper_path, paper_sha, source_id)?;
    *state = candidate;
    Ok(summary)
}

/// The mutation body. Only ever called on a clone of a precondition-checked
/// state (see `add_paper_targets_to_state`).
fn apply_add_paper_targets(
    state: &mut ProtocolState,
    added: &[AddedTargetSpec],
    paper_path: &str,
    paper_sha: &str,
    source_id: &str,
) -> Result<AddTargetsSummary, String> {
    // ---- Target deltas: every existing target Unchanged (same paper),
    // every added target Added with its resolved block lines. -------------
    let mut target_deltas: BTreeMap<TargetId, RevisionTargetDelta> = BTreeMap::new();
    for target in &state.configured_targets {
        target_deltas.insert(
            target.clone(),
            RevisionTargetDelta {
                target: target.clone(),
                label: Some(target.as_str().to_string()),
                kind: RevisionTargetDeltaKind::Unchanged,
                ..RevisionTargetDelta::default()
            },
        );
    }
    for spec in added {
        target_deltas.insert(
            spec.target.clone(),
            RevisionTargetDelta {
                target: spec.target.clone(),
                label: Some(spec.label.clone()),
                new_start_line: Some(spec.start_line),
                new_end_line: Some(spec.end_line),
                kind: RevisionTargetDeltaKind::Added,
                ..RevisionTargetDelta::default()
            },
        );
    }

    // ---- Frozen / editable split. Every existing target is Unchanged, so
    // `compute_frozen_nodes` freezes the union of existing coverage +
    // protected closures + Preamble/Axioms + challenge-pinned nodes; the
    // added targets have no coverage yet and release nothing. -------------
    let frozen_nodes = compute_frozen_nodes(state, &target_deltas);
    let editable_nodes: BTreeSet<NodeId> = state
        .live
        .present_nodes
        .iter()
        .filter(|node| !frozen_nodes.contains(*node))
        .cloned()
        .collect();
    let mut node_dispositions: BTreeMap<NodeId, RevisionNodeDisposition> = BTreeMap::new();
    for node in &state.live.present_nodes {
        node_dispositions.insert(
            node.clone(),
            if frozen_nodes.contains(node) {
                RevisionNodeDisposition::Freeze
            } else {
                RevisionNodeDisposition::Unclassified
            },
        );
    }

    // ---- Carried approvals (display/audit snapshot; the live maps are the
    // source of truth and are inherited by NOT touching them). ------------
    let carried_approvals = RevisionCarriedApprovals {
        corr_approved_fingerprints: state.corr_approved_fingerprints.clone(),
        sound_approved_fingerprints: state.sound_approved_fingerprints.clone(),
        paper_approved_fingerprints: state.paper_approved_fingerprints.clone(),
        substantiveness_rebaselined_nodes: BTreeSet::new(),
    };

    // ---- Grow the configured-target set. The added targets enter the
    // paper lane fresh: status Unknown, no approved fingerprint (mirroring
    // `import_revision_project`'s Added handling — invalidated_targets =
    // the added set, not the empty-coverage-implies-Fail coincidence). ----
    let added_ids: BTreeSet<TargetId> = added.iter().map(|s| s.target.clone()).collect();
    state
        .configured_targets
        .extend(added_ids.iter().cloned());
    for target in &added_ids {
        state.paper_approved_fingerprints.remove(target);
        state.paper_status.insert(target.clone(), CorrStatus::Unknown);
    }
    // Structural normalize derives the whitelisted per-target additions:
    // empty coverage entries + empty paper-fingerprint entries for the added
    // targets in BOTH the live and committed snapshots (`validate()`
    // requires configured-target coverage of those maps).
    state.normalize_all_structural_state();
    // LastClean mirrors were captured before the add. A post-add LastClean
    // rewind restores `live` from them wholesale; without the added targets'
    // (empty) coverage + paper-fingerprint entries the restored state would
    // fail `validate()`'s configured-target coverage checks. Patch ONLY the
    // added targets' entries in (never rewrite the mirrors otherwise), and
    // only when the mirrors are populated at all.
    if state.last_clean_mirrors_populated() {
        for target in &added_ids {
            state
                .last_clean_live
                .coverage
                .entry(target.clone())
                .or_default();
            state
                .last_clean_live
                .paper_current_fingerprints
                .entry(target.clone())
                .or_default();
        }
    }

    // ---- Synthesize the RevisionContext (old == new paper). -------------
    let revision_context = RevisionContext {
        revision_kind: RevisionKind::TargetAddition,
        old_paper_path: paper_path.to_string(),
        old_paper_sha: paper_sha.to_string(),
        old_source_id: source_id.to_string(),
        new_paper_path: paper_path.to_string(),
        new_paper_sha: paper_sha.to_string(),
        new_source_id: source_id.to_string(),
        target_deltas: target_deltas.clone(),
        node_dispositions,
        frozen_nodes: frozen_nodes.clone(),
        editable_nodes: editable_nodes.clone(),
        invalidated_nodes: BTreeSet::new(),
        invalidated_targets: added_ids.clone(),
        carried_approvals,
        revision_plan_written_by_request: None,
        planner_target_actions: BTreeMap::new(),
    };

    // ---- Revision-planning packet: coverage + protected closure scoped to
    // the delta targets, mirroring `import_revision_project`. -------------
    let mut planning_coverage: BTreeMap<TargetId, BTreeSet<NodeId>> = BTreeMap::new();
    let mut planning_closure: BTreeMap<TargetId, BTreeSet<NodeId>> = BTreeMap::new();
    for target in target_deltas.keys() {
        if let Some(nodes) = state.live.coverage.get(target) {
            planning_coverage.insert(target.clone(), nodes.clone());
        }
        if let Some(nodes) = state.live.protected_closure_nodes_per_target.get(target) {
            planning_closure.insert(target.clone(), nodes.clone());
        }
    }
    let revision_planning = RevisionPlanningContext {
        revision_kind: RevisionKind::TargetAddition,
        old_paper_path: paper_path.to_string(),
        new_paper_path: paper_path.to_string(),
        target_deltas,
        coverage: planning_coverage,
        protected_closure_nodes_per_target: planning_closure,
        frozen_nodes: frozen_nodes.clone(),
        editable_nodes: editable_nodes.clone(),
    };

    // ---- Complete-revival recipe. ----------------------------------------
    // Phase/stage flip. Stage::Start (NOT StuckMathAudit) so the run loop
    // emits StartCycle first; `start_cycle` then routes the RevisionStating
    // cycle to the planner while `revision_planning.is_some()`. The cycle
    // number and event log stay CONTINUOUS — the cycle-0 reset in
    // `import_revision_project` is for fresh run dirs only.
    state.phase = Phase::RevisionStating;
    state.stage = Stage::Start;
    state.gate_kind = GateKind::None;
    state.gate_from_invalid_attempt = false;
    state.human_input_outstanding = false;
    state.in_flight_request = None;
    state.pending_task = None;
    state.active_node = None;
    state.active_coarse_node = None;
    state.cycles_in_coarse_repair_mode = 0;
    state.held_target = None;
    state.target_edit_mode = TargetEditMode::Global;
    state.proof_edit_mode = ProofEditMode::Local;

    // Fresh StuckMathAuditState carrying ONLY the revision-planning lane
    // (the at-most-one-lane mutex in `validate()` must pass); every sibling
    // lane carrier is None via `..Default::default()`.
    let trigger = format!(
        "add-targets revival ({source_id}): {} new paper target(s) [{}] appended to a completed \
         run; the revision planner routes their statement work",
        added_ids.len(),
        added_ids
            .iter()
            .map(|t| t.as_str().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    state.stuck_math_audit = StuckMathAuditState {
        active: true,
        trigger: trigger.clone(),
        active_since_cycle: state.cycle,
        revision_planning: Some(revision_planning),
        ..StuckMathAuditState::default()
    };
    state.revision_context = Some(revision_context);

    // A stale streak in the progress buffer must not trip the
    // no-Sound-progress trigger on the first post-revive checkpoint.
    state.reset_progress_history();

    // Clear the `enter_cleanup_phase` latch set + the pending
    // audit/repair/retirement carriers (the revival leaves Cleanup/Complete;
    // these are the same fields `enter_cleanup_phase` resets on entry).
    state.cleanup_audit_tasks.clear();
    state.cleanup_audit_scratchpad.clear();
    state.cleanup_audit_burst_count = 0;
    state.cleanup_audit_round = 1;
    state.cleanup_consecutive_invalid_workers = 0;
    state.cleanup_active_task = None;
    state.cleanup_force_done = false;
    state.latest_audit_rejection_reason.clear();
    state.audit_burst_retry_count = 0;
    state.pending_global_repair_request = None;
    state.pending_global_repair_grant = None;
    state.latest_global_repair_audit_decline_reason.clear();
    state.latest_global_repair_audit_decline_cycle = None;
    state.pending_node_retirement = None;
    state.latest_node_retirement_decline = None;
    state.pending_audit_request = None;
    state.pending_worker_audit_request = None;

    // No `prune_sidecar_queue` needed before this direct validate: revival
    // starts from Complete, where the queue is provably empty (queue entries
    // must be open nodes and `formalization_complete` requires none).
    //
    // The closure-provenance prune IS called: revival adds paper targets
    // rather than reopening nodes, so it is a no-op today, but it is
    // cheap, idempotent, and keeps the rule "every direct `validate()`
    // caller prunes first" free of exceptions to re-prove.
    state.prune_closure_provenance();
    state
        .validate()
        .map_err(|err| format!("add_paper_targets produced an invalid state (bug): {err}"))?;

    Ok(AddTargetsSummary {
        added_targets: added_ids.iter().cloned().collect(),
        total_targets: state.configured_targets.len(),
        frozen_nodes: frozen_nodes.len(),
        editable_nodes: editable_nodes.len(),
        trigger,
        notes: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_date_utc_matches_known_dates() {
        assert_eq!(iso_date_utc(0), "1970-01-01");
        // 2026-07-14 00:00:00 UTC
        assert_eq!(iso_date_utc(1_783_987_200), "2026-07-14");
        // Leap-day check: 2024-02-29.
        assert_eq!(iso_date_utc(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn sha256_hex_is_stable() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}

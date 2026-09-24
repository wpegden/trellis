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
use crate::paper_diff::{hash_block, normalize_block_text};
use crate::paper_targets::{
    extract_paper_statement_blocks, MainResultPreviewEntry, MainResultTarget, NestedMainResultBlock,
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

/// Every recorded target that can be compared at all (label + resolved range),
/// mapped to its recorded window. Callers slice these windows out of the paper
/// as it was when the ranges were recorded. Line numbers are NOT a filter here:
/// a rebinding can preserve a range exactly (a one-line-style enclosing block),
/// so every comparable target needs its content evidence.
pub fn comparable_configured_targets(
    recorded: &[MainResultTarget],
) -> BTreeMap<String, (i64, i64)> {
    recorded
        .iter()
        .filter_map(comparable_recorded_target)
        .map(|(label, start_line, end_line)| (label.to_string(), (start_line, end_line)))
        .collect()
}

/// Drift tripwire for the add-targets config rewrite. `recorded` is
/// `workflow.main_result_targets` as configured; `resolved` is the fresh
/// labels-only re-resolution (with block text) that is about to REPLACE it
/// wholesale; `nested_blocks` is the resolver's cross-env nesting report for
/// the same resolution; `as_recorded_blocks` maps a label to the verbatim
/// paper text of its RECORDED line window as of when that window was recorded
/// — the evidence of what the target used to denote.
///
/// CONTENT decides, never line numbers. An identical range is not evidence of
/// an identical denotation: widening the env set can rebind a label to an
/// enclosing block that occupies exactly the same lines. An operator editing
/// the paper above a block, conversely, shifts every range below it without
/// touching any statement.
///
/// * content evidence available and equal ⇒ same statement: silent when the
///   range held, an operator-visible note when it shifted (the caller
///   re-records the new range by rewriting the config);
/// * content evidence available and different ⇒ hard error, showing what the
///   target used to denote and what it denotes now;
/// * no usable content evidence ⇒ the resolver's `rebinds_inner_label` signal
///   decides: a label rebound from a swallowed inner block hard-errors even
///   with no evidence, a moved range hard-errors as unclassifiable, and only a
///   held range with no rebinding signal is accepted;
/// * label no longer resolves ⇒ hard error. Fails closed throughout.
pub fn classify_configured_target_drift(
    recorded: &[MainResultTarget],
    resolved: &[MainResultPreviewEntry],
    nested_blocks: &[NestedMainResultBlock],
    as_recorded_blocks: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    for target in recorded {
        let Some((label, start_line, end_line)) = comparable_recorded_target(target) else {
            continue;
        };
        let Some(current) = find_resolved(resolved, label) else {
            return Err(format!(
                "add_paper_targets: configured target `{label}` (recorded at lines \
                 {start_line}-{end_line}) no longer resolves against the configured paper. The \
                 paper or workflow.main_result_envs changed since the run was configured; \
                 restore them, or migrate deliberately by re-recording \
                 workflow.main_result_targets."
            ));
        };
        let moved = current.start_line != start_line || current.end_line != end_line;
        let recorded_block = as_recorded_blocks
            .get(label)
            .and_then(|window| recorded_block_from_window(window, label));
        let Some(recorded_block) = recorded_block else {
            if label_is_rebound(nested_blocks, current, label) {
                return Err(format!(
                    "add_paper_targets: configured target `{label}` is now bound by an enclosing \
                     {} block at lines {}-{} that swallowed the statement carrying the label, and \
                     what it denoted when recorded could not be recovered. Restore \
                     workflow.main_result_envs, or migrate deliberately by re-recording \
                     workflow.main_result_targets.",
                    current.env, current.start_line, current.end_line
                ));
            }
            if moved {
                return Err(format!(
                    "add_paper_targets: configured target `{label}` moved from lines \
                     {start_line}-{end_line} to lines {}-{}, and what it denoted at the recorded \
                     range could not be recovered, so a benign line shift cannot be told apart \
                     from a rebinding. Commit the paper and config together in the run repo, or \
                     migrate deliberately by re-recording workflow.main_result_targets.",
                    current.start_line, current.end_line
                ));
            }
            // Range held and the resolver reports no rebinding: the label binds
            // the same block boundaries it was recorded with, so the config
            // rewrite is a no-op for this target. Nothing to classify.
            continue;
        };
        // Known limit: both sides are block text as the resolver sees it, i.e.
        // cut at the first unescaped `%` with no notion of \verb / lstlisting /
        // \url. Two statements differing only after such a literal `%` hash
        // equal here. That is the candidacy scan's own blindness — the kernel
        // never sees the text past it — and comparing unstripped text would be
        // comparing against something the resolver cannot produce.
        if hash_block(&recorded_block) == hash_block(&current.text) {
            if moved {
                notes.push(format!(
                    "add_paper_targets: target `{label}` shifted from lines \
                     {start_line}-{end_line} to lines {}-{} with identical statement content \
                     (benign paper edit above it); re-recorded in workflow.main_result_targets.",
                    current.start_line, current.end_line
                ));
            }
            continue;
        }
        return Err(format!(
            "add_paper_targets: configured target `{label}` now denotes DIFFERENT content — \
             recorded at lines {start_line}-{end_line} it was: {}; at lines {}-{} it is now: \
             {}. A widened workflow.main_result_envs can rebind a label to an enclosing block, \
             and a paper edit can restate it; restore them, or migrate deliberately by \
             re-recording workflow.main_result_targets.",
            excerpt(&recorded_block),
            current.start_line,
            current.end_line,
            excerpt(&current.text)
        ));
    }
    Ok(notes)
}

/// Recover the statement block bound by `label` from an as-recorded line
/// window, using the resolver's own extractor over the full canonical env set
/// (the env set in force when the window was recorded is unknown). Returns the
/// BLOCK text, not the window: a recorded window is whole lines, so a block
/// whose `\begin` shares a line with preceding prose carries that prose, which
/// is not part of what the target denotes. None ⇒ the window does not contain
/// a statement block that `label` binds, so it cannot testify.
fn recorded_block_from_window(window: &str, label: &str) -> Option<String> {
    extract_paper_statement_blocks(window, None)
        .into_iter()
        .find(|block| block.labels.first().map(String::as_str) == Some(label))
        .map(|block| block.text)
}

/// True when the block the label now binds is an enclosing block that
/// swallowed a nested statement and inherited its label — the resolver's own
/// `rebinds_inner_label` finding for exactly this target.
fn label_is_rebound(
    nested_blocks: &[NestedMainResultBlock],
    current: &MainResultPreviewEntry,
    label: &str,
) -> bool {
    nested_blocks.iter().any(|nested| {
        nested.rebinds_inner_label
            && nested.outer_start_line == current.start_line
            && nested.outer_end_line == current.end_line
            && nested.inner_labels.iter().any(|inner| inner == label)
    })
}

/// A recorded entry is comparable only when it carries a label AND a resolved
/// range; label-only entries (hand-written configs) have nothing to compare.
fn comparable_recorded_target(target: &MainResultTarget) -> Option<(&str, i64, i64)> {
    let label = target
        .tex_label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())?;
    if target.start_line <= 0 || target.end_line <= 0 {
        return None;
    }
    Some((label, target.start_line, target.end_line))
}

fn find_resolved<'a>(
    resolved: &'a [MainResultPreviewEntry],
    label: &str,
) -> Option<&'a MainResultPreviewEntry> {
    resolved.iter().find(|entry| {
        entry
            .target
            .tex_label
            .as_deref()
            .map(str::trim)
            .is_some_and(|existing| existing == label)
    })
}

/// Whitespace-normalized head of a block, for "used to denote / now denotes".
fn excerpt(text: &str) -> String {
    let normalized = normalize_block_text(text);
    let head: String = normalized.chars().take(160).collect();
    if head.chars().count() < normalized.chars().count() {
        format!("`{head}…`")
    } else {
        format!("`{head}`")
    }
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
    state.cleanup_repair_node = None;
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

    fn target(label: &str, start_line: i64, end_line: i64) -> MainResultTarget {
        MainResultTarget {
            start_line,
            end_line,
            tex_label: Some(label.to_string()),
        }
    }

    fn block(label: &str, start_line: i64, end_line: i64, text: &str) -> MainResultPreviewEntry {
        block_in("theorem", label, start_line, end_line, text)
    }

    fn block_in(
        env: &str,
        label: &str,
        start_line: i64,
        end_line: i64,
        text: &str,
    ) -> MainResultPreviewEntry {
        MainResultPreviewEntry {
            target: target(label, start_line, end_line),
            env: env.to_string(),
            text: text.to_string(),
            start_line,
            end_line,
        }
    }

    const MAIN_BLOCK: &str =
        "\\begin{theorem}\\label{thm:main}\nA graph is Berge iff it is perfect.\n\\end{theorem}";

    fn as_recorded(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(label, text)| ((*label).to_string(), (*text).to_string()))
            .collect()
    }

    fn classify(
        recorded: &[MainResultTarget],
        resolved: &[MainResultPreviewEntry],
        as_recorded_blocks: &BTreeMap<String, String>,
    ) -> Result<Vec<String>, String> {
        classify_configured_target_drift(recorded, resolved, &[], as_recorded_blocks)
    }

    #[test]
    fn target_drift_check_passes_when_content_still_matches() {
        let recorded = vec![target("thm:main", 2, 4), target("lem:aux", 5, 7)];
        let aux = "\\begin{corollary}\\label{lem:aux}\nAux.\n\\end{corollary}";
        let resolved = vec![
            block("thm:main", 2, 4, MAIN_BLOCK),
            block("lem:aux", 5, 7, aux),
            block("thm:new", 8, 10, "\\begin{theorem}\\end{theorem}"),
        ];
        assert_eq!(
            classify(
                &recorded,
                &resolved,
                &as_recorded(&[("thm:main", MAIN_BLOCK), ("lem:aux", aux)])
            ),
            Ok(Vec::new())
        );
    }

    /// Regression: an identical line range is NOT evidence of an identical
    /// denotation. A one-line-style enclosing block can swallow the statement
    /// and inherit its label while occupying exactly the recorded lines.
    #[test]
    fn rebinding_that_preserves_the_line_range_still_hard_errors() {
        let recorded = vec![target("thm:main", 2, 4)];
        let swallowed = "\\begin{proposition}Framing. \\begin{theorem}\\label{thm:main}\nA graph is Berge iff it is perfect.\n\\end{theorem} extra\\end{proposition}";
        let resolved = vec![block("thm:main", 2, 4, swallowed)];
        let err = classify(&recorded, &resolved, &as_recorded(&[("thm:main", MAIN_BLOCK)]))
            .expect_err("content must decide even when the range held");
        assert!(err.contains("`thm:main` now denotes DIFFERENT content"), "{err}");
        assert!(err.contains("Framing."), "{err}");
    }

    /// Same hazard with NO content evidence: the resolver's own
    /// `rebinds_inner_label` finding must still block it.
    #[test]
    fn rebinding_without_content_evidence_hard_errors_on_the_nesting_signal() {
        let recorded = vec![target("thm:main", 2, 4)];
        let resolved = vec![block_in(
            "proposition",
            "thm:main",
            2,
            4,
            "\\begin{proposition}…\\end{proposition}",
        )];
        let nested = vec![NestedMainResultBlock {
            outer_env: "proposition".to_string(),
            outer_start_line: 2,
            outer_end_line: 4,
            inner_env: "theorem".to_string(),
            inner_start_line: 2,
            inner_end_line: 4,
            inner_labels: vec!["thm:main".to_string()],
            inner_is_candidate: false,
            rebinds_inner_label: true,
        }];
        let err =
            classify_configured_target_drift(&recorded, &resolved, &nested, &BTreeMap::new())
                .expect_err("the nesting signal must fail closed without evidence");
        assert!(err.contains("`thm:main` is now bound by an enclosing"), "{err}");
        assert!(err.contains("proposition"), "{err}");
    }

    #[test]
    fn benign_line_shift_with_identical_content_is_re_recorded_with_a_note() {
        let recorded = vec![target("thm:main", 2, 4)];
        // Paper edited above the block: same statement, lower down, rewrapped.
        // The recorded window additionally carries a `%` comment; neither
        // wrapping nor comments are content.
        let commented_window = "\\begin{theorem}\\label{thm:main}\n% reviewer note\nA graph is Berge iff it is perfect.\n\\end{theorem}";
        let shifted =
            "\\begin{theorem}\\label{thm:main}\nA graph is Berge\niff it is perfect.\n\\end{theorem}";
        let resolved = vec![block("thm:main", 7, 9, shifted)];
        let notes = classify(
            &recorded,
            &resolved,
            &as_recorded(&[("thm:main", commented_window)]),
        )
            .expect("a pure line shift must pass");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("`thm:main`"), "{}", notes[0]);
        assert!(notes[0].contains("lines 2-4"), "{}", notes[0]);
        assert!(notes[0].contains("lines 7-9"), "{}", notes[0]);
        assert!(notes[0].contains("identical statement content"), "{}", notes[0]);
    }

    /// A recorded window is whole LINES, so the block's `\begin` can share a
    /// line with preceding prose and its `\end` can carry trailing text. The
    /// block is extracted out of the window, so the surrounding prose is not
    /// mistaken for a content change.
    #[test]
    fn line_sharing_recorded_window_still_corroborates() {
        let recorded = vec![target("thm:main", 2, 4)];
        let resolved = vec![block("thm:main", 9, 11, MAIN_BLOCK)];
        let window = "We now prove the main result. \\begin{theorem}\\label{thm:main}\nA graph is Berge iff it is perfect.\n\\end{theorem} This completes the section.";
        let notes = classify(&recorded, &resolved, &as_recorded(&[("thm:main", window)]))
            .expect("a line-sharing block must corroborate");
        assert_eq!(notes.len(), 1, "benign shift: {notes:?}");
    }

    #[test]
    fn changed_content_at_the_new_range_hard_errors() {
        let recorded = vec![target("thm:main", 5, 7)];
        let swallowed = "\\begin{proposition}\nOuter framing.\n\\begin{theorem}\\label{thm:main}\nA graph is Berge iff it is perfect.\n\\end{theorem}\n\\end{proposition}";
        let resolved = vec![block("thm:main", 3, 8, swallowed)];
        let err = classify(&recorded, &resolved, &as_recorded(&[("thm:main", MAIN_BLOCK)]))
            .expect_err("a content change must hard-error");
        assert!(err.contains("`thm:main` now denotes DIFFERENT content"), "{err}");
        assert!(err.contains("lines 5-7"), "{err}");
        assert!(err.contains("lines 3-8"), "{err}");
        assert!(err.contains("A graph is Berge iff it is perfect."), "{err}");
        assert!(err.contains("Outer framing."), "{err}");
    }

    /// A run that was ALWAYS configured with the enclosing env is not drift:
    /// the recorded window is that same enclosing block, so content matches
    /// even though the nesting signal fires.
    #[test]
    fn a_long_standing_nested_binding_is_not_drift() {
        let outer = "\\begin{proposition}\nFraming.\n\\begin{theorem}\\label{thm:main}\nStatement.\n\\end{theorem}\n\\end{proposition}";
        let recorded = vec![target("thm:main", 2, 7)];
        let resolved = vec![block("thm:main", 2, 7, outer)];
        let nested = vec![NestedMainResultBlock {
            outer_env: "proposition".to_string(),
            outer_start_line: 2,
            outer_end_line: 7,
            inner_env: "theorem".to_string(),
            inner_start_line: 4,
            inner_end_line: 6,
            inner_labels: vec!["thm:main".to_string()],
            inner_is_candidate: false,
            rebinds_inner_label: true,
        }];
        assert_eq!(
            classify_configured_target_drift(
                &recorded,
                &resolved,
                &nested,
                &as_recorded(&[("thm:main", outer)])
            ),
            Ok(Vec::new())
        );
    }

    #[test]
    fn target_drift_check_fires_when_a_recorded_target_vanishes() {
        let recorded = vec![target("thm:main", 2, 4), target("cor:aux", 5, 7)];
        let resolved = vec![block("thm:main", 2, 4, MAIN_BLOCK)];
        let err = classify(&recorded, &resolved, &BTreeMap::new())
            .expect_err("a vanished target must hard-error");
        assert!(err.contains("`cor:aux`"), "{err}");
        assert!(err.contains("no longer resolves"), "{err}");
    }

    #[test]
    fn unrecoverable_as_recorded_content_fails_closed_on_a_moved_range() {
        let recorded = vec![target("thm:main", 2, 4)];
        let resolved = vec![block("thm:main", 7, 9, MAIN_BLOCK)];
        let err = classify(&recorded, &resolved, &BTreeMap::new())
            .expect_err("an unclassifiable shift must hard-error");
        assert!(err.contains("could not be recovered"), "{err}");
    }

    /// The one line-based shortcut that survives: range held AND the resolver
    /// reports no rebinding. The config rewrite is a no-op for that target, so
    /// a missing content basis must not manufacture a failure.
    #[test]
    fn a_held_range_without_evidence_or_rebinding_is_accepted() {
        let recorded = vec![target("thm:main", 2, 4)];
        let resolved = vec![block("thm:main", 2, 4, MAIN_BLOCK)];
        assert_eq!(classify(&recorded, &resolved, &BTreeMap::new()), Ok(Vec::new()));
    }

    #[test]
    fn as_recorded_window_that_does_not_bind_the_label_fails_closed() {
        let recorded = vec![target("thm:main", 2, 4)];
        let resolved = vec![block("thm:main", 7, 9, MAIN_BLOCK)];
        // Some other block: it cannot testify to what `thm:main` denoted.
        let other = "\\begin{theorem}\\label{thm:other}\nSomething else.\n\\end{theorem}";
        let err = classify(&recorded, &resolved, &as_recorded(&[("thm:main", other)]))
            .expect_err("an uncorroborated window must hard-error");
        assert!(err.contains("could not be recovered"), "{err}");

        // Label present but not FIRST: it does not bind that block.
        let second_label =
            "\\begin{theorem}\\label{thm:other}\\label{thm:main}\nText.\n\\end{theorem}";
        let err = classify(&recorded, &resolved, &as_recorded(&[("thm:main", second_label)]))
            .expect_err("a non-binding label must hard-error");
        assert!(err.contains("could not be recovered"), "{err}");
    }

    #[test]
    fn target_drift_check_skips_entries_with_nothing_recorded_to_compare() {
        // Label-only entries (a hand-written config) and unlabeled line
        // windows carry no recorded range to drift against.
        let recorded = vec![
            target("thm:main", 0, 0),
            MainResultTarget {
                start_line: 5,
                end_line: 7,
                tex_label: None,
            },
        ];
        let resolved = vec![block("thm:main", 30, 40, MAIN_BLOCK)];
        assert!(comparable_configured_targets(&recorded).is_empty());
        assert_eq!(classify(&recorded, &resolved, &BTreeMap::new()), Ok(Vec::new()));
    }

    #[test]
    fn sha256_hex_is_stable() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}

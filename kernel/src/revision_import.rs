//! Revision-mode project import (`revision_plan.md` §5, §8, §16 steps 4-5).
//!
//! `import_revision_project` builds the initial `ProtocolState` of a revision
//! run from an existing, already-formalized tablet. It hydrates the prior
//! run's full `ProtocolState` (the `full_state` import path — the only path
//! supported in v1, decision 4), then transforms it into a `RevisionStating`
//! state:
//!
//! - phase/stage set to `RevisionStating` / `StuckMathAudit`, in-flight and
//!   gate state cleared, so the supervisor loop dispatches the first
//!   revision-planning audit;
//! - the prior corr / sound / paper-faithfulness approvals are *inherited
//!   verbatim* in the live `ProtocolState` maps (the `current == approved`
//!   gate is the source of truth — decision 2); they are also copied into
//!   `RevisionContext.carried_approvals` as a display/audit snapshot only;
//! - paper-faithfulness is force-invalidated for every Changed/Added target
//!   (`revision_plan.md` §7 / §8);
//! - substantiveness is re-baselined against the *new* paper for every present
//!   node (`revision_plan.md` §8, decision 3): both the approved and the live
//!   current fingerprint are set to the freshly observed value and the status
//!   to `Pass`, so `paper_source_sha` cancels and only node-local edits reopen;
//! - the frozen / editable node sets are computed from the live snapshot
//!   (decision 5).

use crate::model::{
    CorrStatus, GateKind, NodeId, Phase, ProtocolState, RevisionCarriedApprovals, RevisionContext,
    RevisionKind, RevisionNodeDisposition, RevisionPlanningContext, RevisionTargetDelta,
    RevisionTargetDeltaKind, Stage, TargetId,
};
use crate::paper_diff::{diff_labeled_targets, label_target_id, revision_diff_report};
use crate::runtime_cli_observations::observe_substantiveness_fingerprints;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const PREAMBLE_NAME: &str = "Preamble";
const AXIOMS_NAME: &str = "Axioms";

/// Summary returned by `import_revision_project`, surfaced in the CLI response
/// (`revision_plan.md` §13) and useful for tests / operator inspection.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RevisionImportSummary {
    pub unchanged_targets: usize,
    pub changed_targets: usize,
    pub added_targets: usize,
    pub removed_targets: usize,
    pub present_nodes: usize,
    pub frozen_nodes: usize,
    pub editable_nodes: usize,
    pub substantiveness_rebaselined_nodes: usize,
    pub diff_report: String,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionImportResult {
    pub repo_path: PathBuf,
    pub state: ProtocolState,
    pub summary: RevisionImportSummary,
}

fn read_text_file(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|err| format!("failed to read {}: {err}", path.display()))
}

fn hash_text(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Resolve `repo_path` and the configured `workflow.paper_tex_path` from the
/// run config. `paper_tex_path` is returned as a path resolved against the
/// repo when relative (matching the runtime's own resolution), so the §3
/// invariant `paper_tex_path == new.tex` can be asserted.
fn config_repo_and_paper(config_path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let text = read_text_file(config_path)?;
    let raw: Value = serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse config {}: {err}", config_path.display()))?;
    let obj = raw
        .as_object()
        .ok_or_else(|| format!("config {} must be a JSON object", config_path.display()))?;
    let repo_raw = obj
        .get("repo_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("config.repo_path must be a non-empty string in {}", config_path.display()))?;
    let repo_candidate = PathBuf::from(repo_raw);
    let repo_path = if repo_candidate.is_absolute() {
        repo_candidate
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(repo_candidate)
    };
    let repo_path = fs::canonicalize(&repo_path).unwrap_or(repo_path);
    if !repo_path.is_dir() {
        return Err(format!("repo_path is not a directory: {}", repo_path.display()));
    }

    let paper_raw = obj
        .get("workflow")
        .and_then(Value::as_object)
        .and_then(|workflow| workflow.get("paper_tex_path"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "config.workflow.paper_tex_path must be a non-empty string in {}",
                config_path.display()
            )
        })?;
    let paper_candidate = PathBuf::from(paper_raw);
    let configured_paper = if paper_candidate.is_absolute() {
        paper_candidate
    } else {
        repo_path.join(paper_candidate)
    };
    Ok((repo_path, configured_paper))
}

/// Same path resolution the runtime uses: absolute paths pass through, relative
/// paths resolve against the repo. Canonicalize when possible so two spellings
/// of the same file compare equal.
fn resolve_against_repo(repo_path: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_path.join(path)
    };
    fs::canonicalize(&joined).unwrap_or(joined)
}

/// Map of `--target-map` overrides: configured `TargetId` -> TeX label. Decision
/// 1: an OPTIONAL override, not mandatory; absent, each configured target is
/// matched to its delta by `label_target_id(configured_id)` directly (the
/// configured ids of the motivating tablet already ARE bare labels).
fn parse_target_map(value: &Value) -> Result<BTreeMap<TargetId, String>, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "target-map must be a JSON object of {target_id: label}".to_string())?;
    let mut out = BTreeMap::new();
    for (key, val) in obj {
        let label = val
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("target-map value for `{key}` must be a non-empty string label"))?;
        out.insert(TargetId::from(key.trim()), label.to_string());
    }
    Ok(out)
}

/// For each configured target, find its delta in the label-keyed diff. The
/// configured target id is mapped to a label by the `--target-map` override
/// when present, else by treating the configured id itself as the label.
fn target_delta_for(
    configured: &TargetId,
    target_map: &BTreeMap<TargetId, String>,
    deltas_by_label: &BTreeMap<TargetId, RevisionTargetDelta>,
) -> Option<RevisionTargetDelta> {
    let label = target_map
        .get(configured)
        .cloned()
        .unwrap_or_else(|| configured.as_str().to_string());
    deltas_by_label.get(&label_target_id(&label)).cloned()
}

/// Compute the deterministic initial frozen-node set (`revision_plan.md` §7,
/// decision 5): covering + protected-closure nodes of every Unchanged carried
/// target, plus Preamble / Axioms / challenge-pinned nodes, MINUS any node also
/// covering a Changed or Added target. Confined to present nodes.
pub(crate) fn compute_frozen_nodes(
    state: &ProtocolState,
    target_deltas: &BTreeMap<TargetId, RevisionTargetDelta>,
) -> BTreeSet<NodeId> {
    let present = &state.live.present_nodes;
    let mut unchanged_targets = BTreeSet::new();
    let mut changed_or_added_targets = BTreeSet::new();
    for (target, delta) in target_deltas {
        match delta.kind {
            RevisionTargetDeltaKind::Unchanged => {
                unchanged_targets.insert(target.clone());
            }
            RevisionTargetDeltaKind::Changed | RevisionTargetDeltaKind::Added => {
                changed_or_added_targets.insert(target.clone());
            }
            RevisionTargetDeltaKind::Removed => {}
        }
    }

    // Covering + protected-closure nodes of Unchanged targets.
    let mut frozen: BTreeSet<NodeId> = BTreeSet::new();
    for target in &unchanged_targets {
        if let Some(nodes) = state.live.coverage.get(target) {
            frozen.extend(nodes.iter().cloned());
        }
        if let Some(nodes) = state.live.protected_closure_nodes_per_target.get(target) {
            frozen.extend(nodes.iter().cloned());
        }
    }

    // Preamble / kernel-managed Axioms node.
    frozen.extend(
        present
            .iter()
            .filter(|node| node.as_str() == PREAMBLE_NAME || node.as_str() == AXIOMS_NAME)
            .cloned(),
    );
    // Challenge byte-pinned covering nodes (reuse the existing challenge
    // coverage set — same predicate the Restructure freeze uses).
    frozen.extend(
        state
            .live
            .challenge_coverage
            .values()
            .flat_map(|set| set.iter().cloned()),
    );

    // A node that also covers (or is in the closure of) a Changed/Added target
    // must stay editable.
    let mut editable_release: BTreeSet<NodeId> = BTreeSet::new();
    for target in &changed_or_added_targets {
        if let Some(nodes) = state.live.coverage.get(target) {
            editable_release.extend(nodes.iter().cloned());
        }
        if let Some(nodes) = state.live.protected_closure_nodes_per_target.get(target) {
            editable_release.extend(nodes.iter().cloned());
        }
    }
    // Preamble / Axioms / challenge-pinned stay frozen regardless: their
    // statements are kernel-/challenge-managed and never released by a paper
    // delta.
    let always_frozen: BTreeSet<NodeId> = present
        .iter()
        .filter(|node| node.as_str() == PREAMBLE_NAME || node.as_str() == AXIOMS_NAME)
        .cloned()
        .chain(
            state
                .live
                .challenge_coverage
                .values()
                .flat_map(|set| set.iter().cloned()),
        )
        .collect();
    frozen.retain(|node| always_frozen.contains(node) || !editable_release.contains(node));

    frozen.retain(|node| present.contains(node));
    frozen
}

/// Re-baseline substantiveness against the new paper for every present node
/// (`revision_plan.md` §8, decision 3). Observes the fingerprint against
/// `new.tex` and writes BOTH the approved and the live current fingerprint, and
/// sets the status to `Pass`. Preamble and tex-less nodes observe to an empty
/// fingerprint (the lane short-circuits Preamble to Pass anyway); they get the
/// same empty value in approved and current and so still inherit Pass.
fn rebaseline_substantiveness(
    state: &mut ProtocolState,
    repo_path: &Path,
    new_paper_path: &Path,
) -> Result<BTreeSet<NodeId>, String> {
    let present = state.live.present_nodes.clone();
    let observed = observe_substantiveness_fingerprints(
        repo_path,
        &present,
        Some(new_paper_path),
        &state.node_kinds,
        &state.node_deviation_claims,
        &state.live.deviation_current_fingerprints,
        &state.configured_reference_papers,
        &state.node_reference_grounds,
    )?;
    let mut rebaselined = BTreeSet::new();
    for node in &present {
        let fp = observed.get(node).cloned().unwrap_or_default();
        state
            .live
            .substantiveness_current_fingerprints
            .insert(node.clone(), fp.clone());
        state
            .substantiveness_approved_fingerprints
            .insert(node.clone(), fp);
        state
            .substantiveness_status
            .insert(node.clone(), CorrStatus::Pass);
        rebaselined.insert(node.clone());
    }
    Ok(rebaselined)
}

/// Build the initial revision `ProtocolState` from an existing repo.
///
/// `config_path` is the new run's config (its `repo_path` and
/// `workflow.paper_tex_path` are read; the latter MUST equal `new_paper_path`).
/// `full_state_path` is a JSON file holding the prior run's full
/// `ProtocolState` (the `state` value of `.trellis-history/supervisor_state.json`,
/// or a bare `protocol_state.json`). `old_paper_path` / `new_paper_path` are the
/// two paper sources; `*_source_id` are provenance strings; `target_map` is the
/// optional configured-target -> label override (decision 1).
#[allow(clippy::too_many_arguments)]
pub fn import_revision_project(
    config_path: &Path,
    full_state_path: &Path,
    old_paper_path: &Path,
    new_paper_path: &Path,
    old_source_id: &str,
    new_source_id: &str,
    target_map: Option<&Value>,
) -> Result<RevisionImportResult, String> {
    let config_path = config_path
        .canonicalize()
        .map_err(|err| format!("failed to resolve config path {}: {err}", config_path.display()))?;
    let (repo_path, configured_paper) = config_repo_and_paper(&config_path)?;

    // §3 invariant: the configured verifier paper MUST be the new paper, else
    // the substantiveness re-baseline would pin against a different file than
    // the lane reads at runtime and a spurious one-cycle reopen wave follows.
    let new_resolved = resolve_against_repo(&repo_path, new_paper_path);
    let configured_resolved = fs::canonicalize(&configured_paper).unwrap_or(configured_paper.clone());
    if new_resolved != configured_resolved {
        return Err(format!(
            "revision import invariant violated: config.workflow.paper_tex_path resolves to {} \
             but --new-paper resolves to {}; the configured verifier paper must be the new paper \
             (revision_plan.md §3)",
            configured_resolved.display(),
            new_resolved.display()
        ));
    }

    let old_resolved = resolve_against_repo(&repo_path, old_paper_path);
    let old_text = read_text_file(&old_resolved)?;
    let new_text = read_text_file(&new_resolved)?;
    if old_text.trim().is_empty() {
        return Err(format!("old paper {} is empty", old_resolved.display()));
    }
    if new_text.trim().is_empty() {
        return Err(format!("new paper {} is empty", new_resolved.display()));
    }

    // Hydrate the prior full ProtocolState. Accept either a bare ProtocolState
    // JSON or the supervisor_state.json envelope with a top-level `state` key.
    let raw_state = read_text_file(full_state_path)?;
    let parsed: Value = serde_json::from_str(&raw_state)
        .map_err(|err| format!("failed to parse full state {}: {err}", full_state_path.display()))?;
    let state_value = match parsed.get("state") {
        Some(inner) if inner.is_object() => inner.clone(),
        _ => parsed,
    };
    let mut state: ProtocolState = serde_json::from_value(state_value).map_err(|err| {
        format!(
            "failed to hydrate ProtocolState from {}: {err}",
            full_state_path.display()
        )
    })?;
    if state.live.present_nodes.is_empty() {
        return Err(format!(
            "hydrated state from {} has no present nodes; cannot import a revision from an empty tablet",
            full_state_path.display()
        ));
    }
    if state.configured_targets.is_empty() {
        return Err(format!(
            "hydrated state from {} has no configured targets; supply a --target-map and a paper with labeled statements",
            full_state_path.display()
        ));
    }

    let target_map = match target_map {
        Some(value) => parse_target_map(value)?,
        None => BTreeMap::new(),
    };

    // §6 paper diff: every label in either paper -> delta, keyed by label.
    let deltas_by_label = diff_labeled_targets(&old_text, &new_text)?;
    let diff_report = revision_diff_report(&deltas_by_label);

    // Project the label-keyed diff onto the configured targets of this run.
    let mut notes = Vec::new();
    let mut target_deltas: BTreeMap<TargetId, RevisionTargetDelta> = BTreeMap::new();
    for configured in &state.configured_targets {
        match target_delta_for(configured, &target_map, &deltas_by_label) {
            Some(mut delta) => {
                // Re-key the delta to the configured TargetId so downstream
                // state (coverage, paper_status) lines up.
                delta.target = configured.clone();
                target_deltas.insert(configured.clone(), delta);
            }
            None => {
                notes.push(format!(
                    "configured target `{configured}` has no matching labeled block in either paper; \
                     treated as Changed (force-invalidated) so it cannot inherit a stale paper pass",
                    configured = configured.as_str()
                ));
                target_deltas.insert(
                    configured.clone(),
                    RevisionTargetDelta {
                        target: configured.clone(),
                        label: None,
                        kind: RevisionTargetDeltaKind::Changed,
                        ..RevisionTargetDelta::default()
                    },
                );
            }
        }
    }
    // Also carry the additive (Added/Removed) deltas the diff found that are
    // not yet configured targets, so the planner sees them in the context.
    for (label_id, delta) in &deltas_by_label {
        if !target_deltas.contains_key(label_id)
            && matches!(
                delta.kind,
                RevisionTargetDeltaKind::Added | RevisionTargetDeltaKind::Removed
            )
        {
            target_deltas.insert(label_id.clone(), delta.clone());
        }
    }

    // §8: corr / sound / paper approvals are inherited verbatim — they already
    // sit in the hydrated live ProtocolState maps (the current == approved gate
    // reads those, decision 2). The carried-approvals record is a display-only
    // snapshot HumanGate surfaces.
    let carried_approvals = RevisionCarriedApprovals {
        corr_approved_fingerprints: state.corr_approved_fingerprints.clone(),
        sound_approved_fingerprints: state.sound_approved_fingerprints.clone(),
        paper_approved_fingerprints: state.paper_approved_fingerprints.clone(),
        substantiveness_rebaselined_nodes: BTreeSet::new(),
    };

    // §7 / §8: force-invalidate paper faithfulness for Changed / Added targets.
    let mut invalidated_targets: BTreeSet<TargetId> = BTreeSet::new();
    for (target, delta) in &target_deltas {
        if matches!(
            delta.kind,
            RevisionTargetDeltaKind::Changed | RevisionTargetDeltaKind::Added
        ) && state.configured_targets.contains(target)
        {
            invalidated_targets.insert(target.clone());
        }
    }
    for target in &invalidated_targets {
        // Drop the approved fingerprint and reset status to Unknown so the
        // paper lane re-runs against the new paper. The live current
        // fingerprint is left as observed by the prior run; the kernel reopen
        // rule (current != approved => Unknown) fires because approved is gone.
        state.paper_approved_fingerprints.remove(target);
        state.paper_status.insert(target.clone(), CorrStatus::Unknown);
    }

    // §8: re-baseline substantiveness against the new paper for every present
    // node (decision 3 — done HERE, not left to a post-import observation).
    let new_paper_for_subst = if new_resolved.is_absolute() {
        new_resolved.clone()
    } else {
        repo_path.join(&new_resolved)
    };
    let rebaselined = rebaseline_substantiveness(&mut state, &repo_path, &new_paper_for_subst)?;

    // Frozen / editable sets (decision 5).
    let frozen_nodes = compute_frozen_nodes(&state, &target_deltas);
    let editable_nodes: BTreeSet<NodeId> = state
        .live
        .present_nodes
        .iter()
        .filter(|node| !frozen_nodes.contains(*node))
        .cloned()
        .collect();

    // Initial node dispositions: Freeze for frozen nodes, Unclassified for the
    // rest (the accepted plan refines them later, §9).
    let mut node_dispositions: BTreeMap<NodeId, RevisionNodeDisposition> = BTreeMap::new();
    for node in &state.live.present_nodes {
        let disposition = if frozen_nodes.contains(node) {
            RevisionNodeDisposition::Freeze
        } else {
            RevisionNodeDisposition::Unclassified
        };
        node_dispositions.insert(node.clone(), disposition);
    }

    let unchanged_targets = target_deltas
        .values()
        .filter(|d| d.kind == RevisionTargetDeltaKind::Unchanged)
        .count();
    let changed_targets = target_deltas
        .values()
        .filter(|d| d.kind == RevisionTargetDeltaKind::Changed)
        .count();
    let added_targets = target_deltas
        .values()
        .filter(|d| d.kind == RevisionTargetDeltaKind::Added)
        .count();
    let removed_targets = target_deltas
        .values()
        .filter(|d| d.kind == RevisionTargetDeltaKind::Removed)
        .count();

    let mut carried_approvals = carried_approvals;
    carried_approvals.substantiveness_rebaselined_nodes = rebaselined.clone();

    // Build the revision-planning audit packet (`revision_plan.md` §9) BEFORE
    // moving `target_deltas` into the RevisionContext. The first StuckMathAudit
    // a revision run dispatches reads this and IS the revision planner. Coverage
    // and protected closure are scoped to the targets present in the deltas (the
    // configured + additive labels the planner needs to route).
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
        revision_kind: RevisionKind::PaperRevision,
        old_paper_path: old_paper_path.to_string_lossy().to_string(),
        new_paper_path: new_paper_path.to_string_lossy().to_string(),
        target_deltas: target_deltas.clone(),
        coverage: planning_coverage,
        protected_closure_nodes_per_target: planning_closure,
        frozen_nodes: frozen_nodes.clone(),
        editable_nodes: editable_nodes.clone(),
    };

    let revision_context = RevisionContext {
        revision_kind: RevisionKind::PaperRevision,
        old_paper_path: old_paper_path.to_string_lossy().to_string(),
        old_paper_sha: hash_text(&old_text),
        old_source_id: old_source_id.trim().to_string(),
        new_paper_path: new_paper_path.to_string_lossy().to_string(),
        new_paper_sha: hash_text(&new_text),
        new_source_id: new_source_id.trim().to_string(),
        target_deltas,
        node_dispositions,
        frozen_nodes: frozen_nodes.clone(),
        editable_nodes: editable_nodes.clone(),
        invalidated_nodes: BTreeSet::new(),
        invalidated_targets,
        carried_approvals,
        revision_plan_written_by_request: None,
        planner_target_actions: BTreeMap::new(),
    };

    // Transform into a RevisionStating run. Seed at Stage::Start (NOT
    // StuckMathAudit) so the run loop emits StartCycle first: StartCycle bumps
    // the run cursor 0 -> 1 and writes the cycle's first event-log record, then
    // start_cycle routes RevisionStating's first cycle to the revision planner
    // (see engine.rs). Seeding StuckMathAudit directly skips StartCycle, leaving
    // cycle pinned at 0 — and the kernel refuses every event-log append at
    // cycle==0 (runtime.rs append_event_log), which both empties the event log
    // (the viewer's chat dropdown enumerates it) and kills the run on the first
    // burst it tries to record. Clear any prior in-flight / gate / active-node.
    state.phase = Phase::RevisionStating;
    state.stage = Stage::Start;
    state.gate_kind = GateKind::None;
    state.human_input_outstanding = false;
    state.active_node = None;
    state.in_flight_request = None;
    // A revision run inherits the prior tablet's *verifier* state but starts a
    // fresh *run*: cycle 0, empty event log. The prior cycle (e.g. 140) would
    // make the run-loader's segmentation guard reject the seed
    // (`event_count == 0 && cycle >= 1`, runtime.rs). Reset to 0 so the first
    // StartCycle stamps cycle 1 and appends the run's first event-log record.
    state.cycle = 0;
    state.stuck_math_audit.active = true;
    // Seed the revision-planning lane so the first dispatched StuckMathAudit IS
    // the revision planner (closes the §9 "no startup path into
    // Stage::StuckMathAudit with a revision planner" gap). Mutually exclusive
    // with the other scenario fields — none of which a freshly imported state
    // carries — per the audit-role mutex.
    state.stuck_math_audit.revision_planning = Some(revision_planning);
    state.revision_context = Some(revision_context);

    let summary = RevisionImportSummary {
        unchanged_targets,
        changed_targets,
        added_targets,
        removed_targets,
        present_nodes: state.live.present_nodes.len(),
        frozen_nodes: frozen_nodes.len(),
        editable_nodes: editable_nodes.len(),
        substantiveness_rebaselined_nodes: rebaselined.len(),
        diff_report,
        notes,
    };

    Ok(RevisionImportResult {
        repo_path,
        state,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fingerprint, NodeKind, WorkingSnapshot};

    /// Stable lookup of a fingerprint map for test assertions.
    fn fp(map: &BTreeMap<NodeId, Fingerprint>, node: &str) -> Option<Fingerprint> {
        map.get(&NodeId::from(node)).cloned()
    }

    use std::io::Write;
    use tempfile::tempdir;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn doc(body: &str) -> String {
        format!("\\begin{{document}}\n{body}\n\\end{{document}}\n")
    }

    /// Build a minimal but internally consistent prior ProtocolState with a
    /// handful of nodes, written to disk, plus its repo + config + papers.
    /// Returns (config_path, full_state_path, old_paper_path, new_paper_path).
    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: PathBuf,
        config: PathBuf,
        full_state: PathBuf,
        old_paper: PathBuf,
        new_paper: PathBuf,
    }

    fn nid(s: &str) -> NodeId {
        NodeId::from(s)
    }

    fn build_fixture(old_body: &str, new_body: &str) -> Fixture {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("paper/revision")).unwrap();

        // Tablet nodes on disk (needed for the substantiveness observation).
        write(&repo.join("Tablet/Preamble.lean"), "import Mathlib\n");
        write(&repo.join("Tablet/Preamble.tex"), "");
        for node in ["MainTheorem", "Aux", "Helper"] {
            write(
                &repo.join(format!("Tablet/{node}.lean")),
                "import Tablet.Preamble\ntheorem t : True := by trivial\n",
            );
            write(
                &repo.join(format!("Tablet/{node}.tex")),
                &format!("\\begin{{theorem}}\\label{{lbl}}{node} statement.\\end{{theorem}}\n"),
            );
        }

        let old_paper = repo.join("paper/revision/old.tex");
        let new_paper = repo.join("paper/revision/new.tex");
        write(&old_paper, &doc(old_body));
        write(&new_paper, &doc(new_body));

        // Config: paper_tex_path MUST equal new.tex (the §3 invariant).
        let config = tmp.path().join("trellis.config.json");
        write(
            &config,
            &format!(
                "{{\"repo_path\":\"{}\",\"workflow\":{{\"paper_tex_path\":\"paper/revision/new.tex\",\"main_result_targets\":[]}}}}",
                repo.display()
            ),
        );

        // Prior ProtocolState: Complete run, three targets covered, all lanes
        // approved. Write it as a bare ProtocolState JSON.
        let mut state = ProtocolState::default();
        let present: BTreeSet<NodeId> =
            ["Preamble", "MainTheorem", "Aux", "Helper"].iter().map(|n| nid(n)).collect();
        let targets: BTreeSet<TargetId> = ["thm:main", "lem:aux"]
            .iter()
            .map(|t| TargetId::from(*t))
            .collect();
        let mut coverage: BTreeMap<TargetId, BTreeSet<NodeId>> = BTreeMap::new();
        coverage.insert(TargetId::from("thm:main"), BTreeSet::from([nid("MainTheorem")]));
        coverage.insert(TargetId::from("lem:aux"), BTreeSet::from([nid("Aux")]));
        let mut closure: BTreeMap<TargetId, BTreeSet<NodeId>> = BTreeMap::new();
        // Helper is in the protected closure of the unchanged lem:aux target.
        closure.insert(TargetId::from("lem:aux"), BTreeSet::from([nid("Helper")]));

        let mut node_kinds: BTreeMap<NodeId, NodeKind> = BTreeMap::new();
        for n in &present {
            node_kinds.insert(
                n.clone(),
                if n.as_str() == "Preamble" {
                    NodeKind::Preamble
                } else {
                    NodeKind::Proof
                },
            );
        }

        state.configured_targets = targets;
        state.node_kinds = node_kinds.clone();
        state.committed_node_kinds = node_kinds;
        state.live = WorkingSnapshot {
            present_nodes: present.clone(),
            open_nodes: BTreeSet::new(),
            coverage: coverage.clone(),
            protected_closure_nodes_per_target: closure,
            ..WorkingSnapshot::default()
        };
        state.committed = state.live.clone();
        // Inherited approvals (verbatim corr/sound/paper).
        for n in &present {
            state
                .corr_approved_fingerprints
                .insert(n.clone(), format!("corr-{}", n.as_str()));
            state.corr_status.insert(n.clone(), CorrStatus::Pass);
            state
                .sound_approved_fingerprints
                .insert(n.clone(), format!("sound-{}", n.as_str()));
        }
        state
            .paper_approved_fingerprints
            .insert(TargetId::from("thm:main"), "paper-main".into());
        state
            .paper_approved_fingerprints
            .insert(TargetId::from("lem:aux"), "paper-aux".into());
        state.paper_status.insert(TargetId::from("thm:main"), CorrStatus::Pass);
        state.paper_status.insert(TargetId::from("lem:aux"), CorrStatus::Pass);
        state.phase = Phase::Complete;
        state.stage = Stage::Complete;

        let full_state = tmp.path().join("supervisor_state.json");
        write(&full_state, &serde_json::to_string(&state).unwrap());

        Fixture {
            _tmp: tmp,
            repo,
            config,
            full_state,
            old_paper,
            new_paper,
        }
    }

    fn import(fx: &Fixture) -> RevisionImportResult {
        import_revision_project(
            &fx.config,
            &fx.full_state,
            &fx.old_paper,
            &fx.new_paper,
            "arXiv:v1",
            "arXiv:v3",
            None,
        )
        .expect("import")
    }

    // thm:main strengthened (Changed), lem:aux unchanged, thm:weak added.
    fn old_body() -> &'static str {
        "\\begin{theorem}\\label{thm:main}\nFor s>=4 the bound holds.\n\\end{theorem}\n\
         \\begin{lemma}\\label{lem:aux}\nAuxiliary fact.\n\\end{lemma}"
    }
    fn new_body() -> &'static str {
        "\\begin{theorem}\\label{thm:main}\nFor s>=3 the bound holds.\n\\end{theorem}\n\
         \\begin{lemma}\\label{lem:aux}\nAuxiliary fact.\n\\end{lemma}\n\
         \\begin{theorem}\\label{thm:weak}\nWeak result.\n\\end{theorem}"
    }

    #[test]
    fn import_sets_revision_phase_and_planning_stage() {
        let fx = build_fixture(old_body(), new_body());
        let result = import(&fx);
        assert_eq!(result.state.phase, Phase::RevisionStating);
        // Seeded at Stage::Start so the run loop emits StartCycle first (cycle
        // 0 -> 1 + first event-log record); start_cycle then routes the first
        // RevisionStating cycle to the StuckMathAudit revision planner.
        assert_eq!(result.state.stage, Stage::Start);
        assert_eq!(result.state.gate_kind, GateKind::None);
        assert!(result.state.in_flight_request.is_none());
        // A revision run starts a fresh run cursor (cycle 0) so the run-loader's
        // segmentation guard accepts the seed with an empty event log.
        assert_eq!(result.state.cycle, 0);
        assert!(result.state.stuck_math_audit.active);
        assert!(result.state.revision_context.is_some());
        // The planning lane is seeded so start_cycle dispatches the planner.
        assert!(result.state.stuck_math_audit.revision_planning.is_some());
    }

    #[test]
    fn import_seeds_revision_planning_lane() {
        let fx = build_fixture(old_body(), new_body());
        let result = import(&fx);
        // §9: the first dispatched audit IS the revision planner -> the
        // revision_planning scenario field is populated, and the other
        // scenario fields are not co-set (audit-role mutex).
        let planning = result
            .state
            .stuck_math_audit
            .revision_planning
            .as_ref()
            .expect("revision_planning seeded at import");
        assert!(result.state.stuck_math_audit.need_input_audit.is_none());
        assert!(result.state.stuck_math_audit.gap_research.is_none());
        assert!(result.state.stuck_math_audit.gap_plan_critique.is_none());
        assert!(result.state.pending_global_repair_request.is_none());
        // The planner packet mirrors the computed import context.
        let ctx = result.state.revision_context.as_ref().unwrap();
        assert_eq!(planning.target_deltas, ctx.target_deltas);
        assert_eq!(planning.frozen_nodes, ctx.frozen_nodes);
        assert_eq!(planning.editable_nodes, ctx.editable_nodes);
        // Coverage + protected closure are scoped to the targets in the deltas.
        assert_eq!(
            planning.coverage.get(&TargetId::from("lem:aux")),
            Some(&BTreeSet::from([nid("Aux")]))
        );
        assert_eq!(
            planning
                .protected_closure_nodes_per_target
                .get(&TargetId::from("lem:aux")),
            Some(&BTreeSet::from([nid("Helper")]))
        );
    }

    #[test]
    fn import_classifies_target_deltas() {
        let fx = build_fixture(old_body(), new_body());
        let result = import(&fx);
        let ctx = result.state.revision_context.as_ref().unwrap();
        assert_eq!(
            ctx.target_deltas[&TargetId::from("thm:main")].kind,
            RevisionTargetDeltaKind::Changed
        );
        assert_eq!(
            ctx.target_deltas[&TargetId::from("lem:aux")].kind,
            RevisionTargetDeltaKind::Unchanged
        );
        // thm:weak is added (not a configured target, but carried for planner).
        assert_eq!(
            ctx.target_deltas[&TargetId::from("thm:weak")].kind,
            RevisionTargetDeltaKind::Added
        );
        assert_eq!(result.summary.changed_targets, 1);
        assert_eq!(result.summary.unchanged_targets, 1);
        assert_eq!(result.summary.added_targets, 1);
    }

    #[test]
    fn import_inherits_corr_and_sound_verbatim() {
        let fx = build_fixture(old_body(), new_body());
        let result = import(&fx);
        assert_eq!(
            fp(&result.state.corr_approved_fingerprints, "MainTheorem"),
            Some("corr-MainTheorem".to_string())
        );
        assert_eq!(
            fp(&result.state.sound_approved_fingerprints, "Aux"),
            Some("sound-Aux".to_string())
        );
        // Carried-approvals snapshot mirrors them.
        let ctx = result.state.revision_context.as_ref().unwrap();
        assert_eq!(
            fp(&ctx.carried_approvals.corr_approved_fingerprints, "MainTheorem"),
            Some("corr-MainTheorem".to_string())
        );
    }

    #[test]
    fn import_force_invalidates_changed_and_added_targets_for_paper() {
        let fx = build_fixture(old_body(), new_body());
        let result = import(&fx);
        // thm:main is a configured Changed target -> approval dropped, Unknown.
        assert!(!result
            .state
            .paper_approved_fingerprints
            .contains_key(&TargetId::from("thm:main")));
        assert_eq!(
            result.state.paper_status.get(&TargetId::from("thm:main")),
            Some(&CorrStatus::Unknown)
        );
        // lem:aux unchanged -> approval inherited.
        assert_eq!(
            result.state.paper_approved_fingerprints.get(&TargetId::from("lem:aux")),
            Some(&"paper-aux".to_string())
        );
        let ctx = result.state.revision_context.as_ref().unwrap();
        assert!(ctx.invalidated_targets.contains(&TargetId::from("thm:main")));
        assert!(!ctx.invalidated_targets.contains(&TargetId::from("lem:aux")));
    }

    #[test]
    fn import_rebaselines_substantiveness_to_pass_for_every_present_node() {
        let fx = build_fixture(old_body(), new_body());
        let result = import(&fx);
        for node in &result.state.live.present_nodes {
            assert_eq!(
                result.state.substantiveness_status.get(node),
                Some(&CorrStatus::Pass),
                "node {node} must be substantiveness Pass after re-baseline"
            );
            // approved == current for every node, so the gate returns Pass and
            // a pure paper-version swap reopens nothing.
            assert_eq!(
                result.state.substantiveness_approved_fingerprints.get(node),
                result.state.live.substantiveness_current_fingerprints.get(node),
                "approved must equal current for node {node}"
            );
            assert!(result.state.current_substantiveness_pass(node));
        }
        let ctx = result.state.revision_context.as_ref().unwrap();
        assert_eq!(
            ctx.carried_approvals.substantiveness_rebaselined_nodes.len(),
            result.state.live.present_nodes.len()
        );
    }

    #[test]
    fn import_freezes_unchanged_target_closure_and_preamble() {
        let fx = build_fixture(old_body(), new_body());
        let result = import(&fx);
        let ctx = result.state.revision_context.as_ref().unwrap();
        // lem:aux unchanged: its covering node (Aux) and closure (Helper) freeze.
        assert!(ctx.frozen_nodes.contains(&nid("Aux")));
        assert!(ctx.frozen_nodes.contains(&nid("Helper")));
        assert!(ctx.frozen_nodes.contains(&nid("Preamble")));
        // thm:main changed: its covering node (MainTheorem) stays editable.
        assert!(!ctx.frozen_nodes.contains(&nid("MainTheorem")));
        assert!(ctx.editable_nodes.contains(&nid("MainTheorem")));
        // editable = present - frozen.
        for node in &result.state.live.present_nodes {
            assert_eq!(
                ctx.frozen_nodes.contains(node),
                !ctx.editable_nodes.contains(node),
                "node {node} must be exactly one of frozen/editable"
            );
        }
    }

    #[test]
    fn import_rejects_when_config_paper_is_not_new_paper() {
        let fx = build_fixture(old_body(), new_body());
        // Point the config paper at old.tex instead of new.tex.
        write(
            &fx.config,
            &format!(
                "{{\"repo_path\":\"{}\",\"workflow\":{{\"paper_tex_path\":\"paper/revision/old.tex\",\"main_result_targets\":[]}}}}",
                fx.repo.display()
            ),
        );
        let err = import_revision_project(
            &fx.config,
            &fx.full_state,
            &fx.old_paper,
            &fx.new_paper,
            "arXiv:v1",
            "arXiv:v3",
            None,
        )
        .unwrap_err();
        assert!(err.contains("invariant violated"), "got: {err}");
        assert!(err.contains("paper_tex_path"), "got: {err}");
    }

    #[test]
    fn import_accepts_supervisor_state_envelope() {
        let fx = build_fixture(old_body(), new_body());
        // Re-wrap the bare state in the {event_count, state, ...} envelope.
        let bare: Value = serde_json::from_str(&fs::read_to_string(&fx.full_state).unwrap()).unwrap();
        let envelope = serde_json::json!({ "event_count": 7, "state": bare });
        write(&fx.full_state, &serde_json::to_string(&envelope).unwrap());
        let result = import(&fx);
        assert_eq!(result.state.phase, Phase::RevisionStating);
        assert_eq!(result.summary.present_nodes, 4);
    }

    #[test]
    fn import_honors_target_map_override() {
        // Configured target id `T1` doesn't match a label; the override maps it
        // to `thm:main`.
        let fx = build_fixture(old_body(), new_body());
        let mut state: ProtocolState =
            serde_json::from_str(&fs::read_to_string(&fx.full_state).unwrap()).unwrap();
        state.configured_targets = BTreeSet::from([TargetId::from("T1")]);
        state
            .live
            .coverage
            .insert(TargetId::from("T1"), BTreeSet::from([nid("MainTheorem")]));
        write(&fx.full_state, &serde_json::to_string(&state).unwrap());

        let map = serde_json::json!({ "T1": "thm:main" });
        let result = import_revision_project(
            &fx.config,
            &fx.full_state,
            &fx.old_paper,
            &fx.new_paper,
            "arXiv:v1",
            "arXiv:v3",
            Some(&map),
        )
        .expect("import with target map");
        let ctx = result.state.revision_context.as_ref().unwrap();
        assert_eq!(
            ctx.target_deltas[&TargetId::from("T1")].kind,
            RevisionTargetDeltaKind::Changed
        );
        assert!(ctx.invalidated_targets.contains(&TargetId::from("T1")));
    }

    #[test]
    fn after_rebaseline_a_restated_node_reopens_and_a_new_node_starts_unknown() {
        // §8: the re-baseline pins approved=current against the new paper.
        // Editing a node's `.tex` (restate) diverges its observed current
        // fingerprint from the pinned approved -> the gate reopens it. A brand
        // new node has no approved entry -> Unknown. We simulate the post-import
        // worker delta by re-observing one edited node and adding one new node.
        let fx = build_fixture(old_body(), new_body());
        let mut result = import(&fx);

        // Sanity: every node is Pass at import.
        assert!(result.state.current_substantiveness_pass(&nid("MainTheorem")));

        // Restate MainTheorem: change its .tex on disk and re-observe just that
        // node's current fingerprint (what the runtime does after a worker
        // burst). Its approved entry is unchanged (pinned at import).
        write(
            &fx.repo.join("Tablet/MainTheorem.lean"),
            "import Tablet.Preamble\ntheorem t : True := by trivial\n",
        );
        write(
            &fx.repo.join("Tablet/MainTheorem.tex"),
            "\\begin{theorem}\\label{lbl}MainTheorem STRONGER statement.\\end{theorem}\n",
        );
        let reobserved = observe_substantiveness_fingerprints(
            &fx.repo,
            &BTreeSet::from([nid("MainTheorem")]),
            Some(&fx.new_paper),
            &result.state.node_kinds,
            &result.state.node_deviation_claims,
            &result.state.live.deviation_current_fingerprints,
            &result.state.configured_reference_papers,
            &result.state.node_reference_grounds,
        )
        .unwrap();
        result.state.live.substantiveness_current_fingerprints.insert(
            nid("MainTheorem"),
            reobserved.get(&nid("MainTheorem")).cloned().unwrap_or_default(),
        );
        // own_tex diverged -> current != approved -> reopens (Unknown).
        assert!(
            result.state.current_substantiveness_unknown(&nid("MainTheorem")),
            "restated node must reopen substantiveness against the new paper"
        );

        // A new node with no approved entry is Unknown.
        write(
            &fx.repo.join("Tablet/NewBranch.lean"),
            "import Tablet.Preamble\ntheorem t : True := by trivial\n",
        );
        write(
            &fx.repo.join("Tablet/NewBranch.tex"),
            "\\begin{theorem}\\label{lbl2}New branch.\\end{theorem}\n",
        );
        result.state.live.present_nodes.insert(nid("NewBranch"));
        result
            .state
            .node_kinds
            .insert(nid("NewBranch"), NodeKind::Proof);
        // No approved/current entry seeded -> the gate returns Unknown.
        assert!(
            result.state.current_substantiveness_unknown(&nid("NewBranch")),
            "a new node must start Unknown (no approved entry to inherit)"
        );

        // Unchanged Helper still inherits Pass (the pure paper swap is a no-op).
        assert!(result.state.current_substantiveness_pass(&nid("Helper")));
    }
}

use crate::model::{
    ApprovedTargetSnapshot, Blocker, CorrStatus, Fingerprint, GateKind, NodeDifficulty, NodeId,
    Phase, ProofEditMode, ProtocolState, SoundStatus, Stage, TargetEditMode, TargetId,
    WorkingSnapshot,
};
use crate::paper_fingerprints::observe_paper_faithfulness_fingerprints;
use crate::worker_normalization::{
    direct_deps_from_repo, node_kinds_from_repo, present_nodes_from_repo, proof_nodes_from_repo,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyImportSummary {
    pub legacy_cycle: u32,
    pub legacy_phase: String,
    pub legacy_resume_from: String,
    pub normalized_stage: Stage,
    pub legacy_active_node: Option<NodeId>,
    pub normalized_active_node: Option<NodeId>,
    pub normalized_mode: crate::model::TaskMode,
    pub held_target: Option<NodeId>,
    pub blocked_targets: BTreeSet<TargetId>,
    pub global_blockers: BTreeSet<Blocker>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyImportResult {
    pub repo_path: PathBuf,
    pub state: ProtocolState,
    pub summary: LegacyImportSummary,
}

#[derive(Clone, Debug)]
struct ConfiguredTarget {
    id: TargetId,
    label: Option<String>,
    start_line: i64,
    end_line: i64,
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("failed to parse {}: {err}", path.display()))
}

fn as_object<'a>(
    value: &'a Value,
    context: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{context} must be a JSON object"))
}

fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn i64_field(map: &serde_json::Map<String, Value>, key: &str) -> i64 {
    map.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn bool_field(map: &serde_json::Map<String, Value>, key: &str) -> bool {
    map.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn resolve_repo_path(config_path: &Path, config_raw: &Value) -> Result<PathBuf, String> {
    let config_obj = as_object(config_raw, "config")?;
    let repo_raw = string_field(config_obj, "repo_path");
    if repo_raw.is_empty() {
        return Err("config is missing repo_path".to_string());
    }
    let repo = {
        let path = PathBuf::from(&repo_raw);
        if path.is_absolute() {
            path
        } else {
            config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(path)
        }
    }
    .canonicalize()
    .map_err(|err| format!("failed to resolve repo_path {repo_raw}: {err}"))?;
    if !repo.is_dir() {
        return Err(format!("repo_path is not a directory: {}", repo.display()));
    }
    Ok(repo)
}

fn target_id(label: &Option<String>, start_line: i64, end_line: i64) -> TargetId {
    TargetId::from(
        label
            .clone()
            .unwrap_or_else(|| format!("lines:{start_line}-{end_line}")),
    )
}

fn configured_targets(
    config_path: &Path,
    config_raw: &Value,
) -> Result<(PathBuf, Vec<ConfiguredTarget>), String> {
    let repo_path = resolve_repo_path(config_path, config_raw)?;
    let config_obj = as_object(config_raw, "config")?;
    let workflow = config_obj
        .get("workflow")
        .and_then(Value::as_object)
        .ok_or_else(|| "config.workflow must be an object".to_string())?;
    let raw_targets = workflow
        .get("main_result_targets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for item in raw_targets {
        let obj = match item.as_object() {
            Some(obj) => obj,
            None => continue,
        };
        let start_line = i64_field(obj, "start_line");
        let end_line = i64_field(obj, "end_line");
        if start_line <= 0 || end_line <= 0 {
            continue;
        }
        let (start_line, end_line) = if start_line <= end_line {
            (start_line, end_line)
        } else {
            (end_line, start_line)
        };
        let label = {
            let raw = string_field(obj, "tex_label");
            if raw.is_empty() {
                None
            } else {
                Some(raw)
            }
        };
        out.push(ConfiguredTarget {
            id: target_id(&label, start_line, end_line),
            label,
            start_line,
            end_line,
        });
    }
    Ok((repo_path, out))
}

fn configured_state_dir(config_raw: &Value, repo_path: &Path) -> PathBuf {
    let config_obj = as_object(config_raw, "config").ok();
    let state_dir_raw = config_obj
        .map(|obj| string_field(obj, "state_dir"))
        .unwrap_or_default();
    if !state_dir_raw.is_empty() {
        let state_dir = PathBuf::from(&state_dir_raw);
        if state_dir.is_absolute() {
            return state_dir;
        }
        return repo_path.join(state_dir);
    }
    let trellis = repo_path.join(".trellis");
    if trellis.is_dir() {
        return trellis;
    }
    let lagent = repo_path.join(".agent-supervisor");
    if lagent.is_dir() {
        return lagent;
    }
    trellis
}

fn default_state_path(config_path: &Path, config_raw: &Value, repo_path: &Path) -> PathBuf {
    let _ = config_path;
    configured_state_dir(config_raw, repo_path).join("state.json")
}

fn default_tablet_path(config_path: &Path, config_raw: &Value, repo_path: &Path) -> PathBuf {
    let _ = config_path;
    configured_state_dir(config_raw, repo_path).join("tablet.json")
}

fn compute_ranks(deps: &BTreeMap<NodeId, BTreeSet<NodeId>>) -> BTreeMap<NodeId, u32> {
    fn rank_of(
        node: &str,
        deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
        memo: &mut BTreeMap<NodeId, u32>,
        visiting: &mut BTreeSet<NodeId>,
    ) -> u32 {
        if let Some(rank) = memo.get(node) {
            return *rank;
        }
        if !visiting.insert(NodeId::from(node)) {
            return 0;
        }
        let rank = deps
            .get(node)
            .into_iter()
            .flat_map(|items| items.iter())
            .map(|dep| rank_of(dep, deps, memo, visiting))
            .max()
            .unwrap_or(0)
            + if deps.get(node).is_some_and(|items| !items.is_empty()) {
                1
            } else {
                0
            };
        visiting.remove(node);
        memo.insert(NodeId::from(node), rank);
        rank
    }

    let mut memo = BTreeMap::new();
    for node in deps.keys() {
        let mut visiting = BTreeSet::new();
        let _ = rank_of(node, deps, &mut memo, &mut visiting);
    }
    memo
}

fn normalize_status(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pass" => "pass",
        "fail" => "fail",
        "structural" => "structural",
        _ => "unknown",
    }
}

fn current_corr_fingerprint(node_obj: &serde_json::Map<String, Value>) -> Fingerprint {
    let preferred = [
        "correspondence_content_hash",
        "verification_content_hash",
        "lean_statement_hash",
        "closed_content_hash",
    ];
    for key in preferred {
        let value = string_field(node_obj, key);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

fn hash_content(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(32)
        .collect()
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn extract_tex_statement(tex_content: &str) -> String {
    tex_content
        .split("\\begin{proof}")
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn current_repo_sound_fingerprint(
    repo_path: &Path,
    node: &str,
    deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
) -> Fingerprint {
    let tex_content = read_text(&repo_path.join("Tablet").join(format!("{node}.tex")));
    if tex_content.trim().is_empty() {
        return String::new();
    }

    let direct_children: Vec<_> = deps
        .get(node)
        .into_iter()
        .flat_map(|items| items.iter().cloned())
        .collect();
    let mut parts = vec![
        format!("node:{node}"),
        format!("self_tex:{}", hash_content(&tex_content)),
        format!("children:{}", direct_children.join(",")),
    ];
    for child in direct_children {
        let child_tex = extract_tex_statement(&read_text(
            &repo_path.join("Tablet").join(format!("{child}.tex")),
        ));
        if child == "Preamble" && child_tex.is_empty() {
            parts.push(format!("child_stmt:{child}:{}", hash_content("")));
            continue;
        }
        if child_tex.is_empty() {
            return String::new();
        }
        parts.push(format!("child_stmt:{child}:{}", hash_content(&child_tex)));
    }
    hash_content(&parts.join("|"))
}

fn current_repo_preamble_corr_fingerprint(repo_path: &Path) -> Fingerprint {
    let lean_content = read_text(&repo_path.join("Tablet/Preamble.lean"));
    let tex_content = read_text(&repo_path.join("Tablet/Preamble.tex"));
    if tex_content.trim().is_empty() {
        return String::new();
    }
    hash_content(
        &[
            "node:Preamble".to_string(),
            format!("preamble_lean:{}", hash_content(&lean_content)),
            format!("preamble_tex:{}", hash_content(tex_content.trim())),
        ]
        .join("|"),
    )
}

fn match_target_claims(
    node_obj: &serde_json::Map<String, Value>,
    configured: &[ConfiguredTarget],
) -> BTreeSet<TargetId> {
    let provenance = node_obj.get("paper_provenance").and_then(Value::as_object);
    let Some(provenance) = provenance else {
        return BTreeSet::new();
    };
    let label = {
        let raw = string_field(provenance, "tex_label");
        if raw.is_empty() {
            None
        } else {
            Some(raw)
        }
    };
    let start_line = i64_field(provenance, "start_line");
    let end_line = i64_field(provenance, "end_line");
    configured
        .iter()
        .filter(|target| match (&label, &target.label) {
            (Some(a), Some(b)) if a == b => true,
            _ => {
                start_line > 0
                    && end_line > 0
                    && start_line == target.start_line
                    && end_line == target.end_line
            }
        })
        .map(|target| target.id.clone())
        .collect()
}

fn coverage_from_claims(
    configured_targets: &BTreeSet<TargetId>,
    target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<TargetId, BTreeSet<NodeId>> {
    let mut coverage: BTreeMap<TargetId, BTreeSet<NodeId>> = configured_targets
        .iter()
        .cloned()
        .map(|target| (target, BTreeSet::new()))
        .collect();
    for node in present_nodes {
        if let Some(targets) = target_claims.get(node) {
            for target in targets {
                coverage
                    .entry(target.clone())
                    .or_default()
                    .insert(node.clone());
            }
        }
    }
    coverage
}

fn map_phase(raw: &str) -> Result<Phase, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "theorem_stating" => Ok(Phase::TheoremStating),
        "proof_formalization" => Ok(Phase::ProofFormalization),
        "cleanup" => Ok(Phase::Cleanup),
        "complete" => Ok(Phase::Complete),
        other => Err(format!("unsupported legacy phase for import: {other}")),
    }
}

fn map_target_mode(raw: &str) -> TargetEditMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "global" => TargetEditMode::Global,
        _ => TargetEditMode::Targeted,
    }
}

fn map_proof_mode(raw: &str) -> ProofEditMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "restructure" => ProofEditMode::Restructure,
        "coarse_restructure" => ProofEditMode::CoarseRestructure,
        _ => ProofEditMode::Local,
    }
}

fn import_approved_targets(
    legacy_state: &serde_json::Map<String, Value>,
    configured: &[ConfiguredTarget],
) -> ApprovedTargetSnapshot {
    let mut snapshot = ApprovedTargetSnapshot::default();
    let trusted = legacy_state
        .get("trusted_main_result_target_state")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if trusted.is_empty() {
        return snapshot;
    }
    let configured_by_label: BTreeMap<String, TargetId> = configured
        .iter()
        .filter_map(|target| {
            target
                .label
                .as_ref()
                .map(|label| (label.clone(), target.id.clone()))
        })
        .collect();
    for (label, entry) in trusted {
        let Some(target_id) = configured_by_label.get(&label).cloned() else {
            continue;
        };
        let Some(entry_obj) = entry.as_object() else {
            continue;
        };
        snapshot.configured_targets.insert(target_id.clone());
        let nodes = entry_obj
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|value| value.as_str().map(|s| NodeId::from(s.trim())))
            .filter(|s| !s.is_empty())
            .collect::<BTreeSet<_>>();
        snapshot.coverage.insert(target_id.clone(), nodes);
    }
    snapshot
}

pub fn import_legacy_project(
    config_path: &Path,
    legacy_state_path: Option<&Path>,
    legacy_tablet_path: Option<&Path>,
) -> Result<LegacyImportResult, String> {
    let config_path = config_path.canonicalize().map_err(|err| {
        format!(
            "failed to resolve config path {}: {err}",
            config_path.display()
        )
    })?;
    let config_raw = read_json(&config_path)?;
    let (repo_path, configured) = configured_targets(&config_path, &config_raw)?;
    let state_path = legacy_state_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_state_path(&config_path, &config_raw, &repo_path));
    let tablet_path = legacy_tablet_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_tablet_path(&config_path, &config_raw, &repo_path));
    let legacy_state = as_object(&read_json(&state_path)?, "legacy state")?.clone();
    let legacy_tablet = as_object(&read_json(&tablet_path)?, "legacy tablet")?.clone();
    let legacy_nodes = legacy_tablet
        .get("nodes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let configured_targets: BTreeSet<TargetId> =
        configured.iter().map(|target| target.id.clone()).collect();
    let present = present_nodes_from_repo(&repo_path)?;
    let node_kinds = node_kinds_from_repo(&repo_path, &present);
    let proof_nodes = proof_nodes_from_repo(&repo_path, &present);
    let deps = direct_deps_from_repo(&repo_path, &present);
    let ranks = compute_ranks(&deps);

    let mut open_nodes = BTreeSet::new();
    let mut node_difficulty = BTreeMap::new();
    let mut easy_attempts = BTreeMap::new();
    let mut corr_status = BTreeMap::new();
    let mut corr_approved_fingerprints = BTreeMap::new();
    let mut sound_status = BTreeMap::new();
    let mut sound_approved_fingerprints = BTreeMap::new();
    let mut node_target_fingerprints = BTreeMap::new();
    let mut corr_current_fingerprints = BTreeMap::new();
    let mut sound_current_fingerprints = BTreeMap::new();
    let mut target_claims = BTreeMap::new();

    for node in &present {
        let legacy_node = legacy_nodes.get(node.as_str()).and_then(Value::as_object);
        let status = legacy_node
            .map(|obj| string_field(obj, "status"))
            .unwrap_or_else(|| {
                if proof_nodes.contains(node) {
                    "open".to_string()
                } else {
                    "closed".to_string()
                }
            });
        if status != "closed" {
            open_nodes.insert(node.clone());
        }
        let difficulty = match legacy_node
            .map(|obj| string_field(obj, "difficulty"))
            .unwrap_or_else(|| "hard".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "easy" => NodeDifficulty::Easy,
            _ => NodeDifficulty::Hard,
        };
        node_difficulty.insert(node.clone(), difficulty);
        let attempts = legacy_node
            .and_then(|obj| obj.get("easy_attempts"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        easy_attempts.insert(node.clone(), attempts);

        let corr_fp = if node == "Preamble" {
            current_repo_preamble_corr_fingerprint(&repo_path)
        } else {
            legacy_node
                .map(current_corr_fingerprint)
                .unwrap_or_default()
        };
        let sound_fp = current_repo_sound_fingerprint(&repo_path, node, &deps);
        node_target_fingerprints.insert(node.clone(), corr_fp.clone());
        corr_current_fingerprints.insert(node.clone(), corr_fp.clone());
        sound_current_fingerprints.insert(node.clone(), sound_fp.clone());

        if node == "Preamble" && corr_fp.is_empty() {
            corr_status.insert(node.clone(), CorrStatus::Pass);
            corr_approved_fingerprints.insert(node.clone(), corr_fp.clone());
        }

        if let Some(obj) = legacy_node {
            match normalize_status(&string_field(obj, "correspondence_status")) {
                "pass" => {
                    corr_status.insert(node.clone(), CorrStatus::Pass);
                    corr_approved_fingerprints.insert(node.clone(), corr_fp.clone());
                }
                "fail" => {
                    corr_status.insert(node.clone(), CorrStatus::Fail);
                }
                _ => {}
            }
            match normalize_status(&string_field(obj, "soundness_status")) {
                "pass" => {
                    sound_status.insert(node.clone(), SoundStatus::Pass);
                    if !sound_fp.is_empty() {
                        sound_approved_fingerprints.insert(node.clone(), sound_fp.clone());
                    }
                }
                "fail" => {
                    sound_status.insert(node.clone(), SoundStatus::Fail);
                }
                "structural" => {
                    sound_status.insert(node.clone(), SoundStatus::Structural);
                }
                _ => {}
            }
            let claims = match_target_claims(obj, &configured);
            if !claims.is_empty() {
                target_claims.insert(node.clone(), claims);
            }
        }
    }

    let coverage = coverage_from_claims(&configured_targets, &target_claims, &present);
    // Legacy import doesn't have access to lake / Lean closure walk, so we
    // can't compute L_def per covering node. The empty L_def map signals
    // "couldn't determine", and `observe_paper_faithfulness_fingerprints`
    // returns empty fingerprints for covered targets (per its strict
    // completeness rule). Targets default to Unknown until the first
    // healthy in-process observation re-pins them. Acceptable: legacy
    // imports are one-shot bootstraps; subsequent operations reconstruct
    // the full fingerprints.
    let target_corr_current_fingerprints = observe_paper_faithfulness_fingerprints(
        &repo_path,
        &configured_targets,
        &target_claims,
        &present,
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let mut target_corr_status = BTreeMap::new();
    let mut target_corr_approved_fingerprints = BTreeMap::new();
    for target in &configured_targets {
        let nodes = coverage.get(target).cloned().unwrap_or_default();
        if nodes.is_empty() {
            continue;
        }
        let all_pass = nodes
            .iter()
            .all(|node| corr_status.get(node) == Some(&CorrStatus::Pass));
        let any_fail = nodes
            .iter()
            .any(|node| corr_status.get(node) == Some(&CorrStatus::Fail));
        if all_pass {
            target_corr_status.insert(target.clone(), CorrStatus::Pass);
            if let Some(fp) = target_corr_current_fingerprints.get(target) {
                if !fp.is_empty() {
                    target_corr_approved_fingerprints.insert(target.clone(), fp.clone());
                }
            }
        } else if any_fail {
            target_corr_status.insert(target.clone(), CorrStatus::Fail);
        }
    }

    let live = WorkingSnapshot {
        present_nodes: present.clone(),
        open_nodes: open_nodes.clone(),
        coverage: coverage.clone(),
        target_fingerprints: node_target_fingerprints.clone(),
        corr_current_fingerprints: corr_current_fingerprints.clone(),
        paper_current_fingerprints: target_corr_current_fingerprints.clone(),
        sound_current_fingerprints: sound_current_fingerprints.clone(),
        deviation_current_fingerprints: BTreeMap::new(),
        sound_current_fingerprint_parts: BTreeMap::new(),
        sketch_proof_nodes: BTreeSet::new(),
        // Substantiveness fingerprints aren't observable from
        // the legacy `paper_status` shape; legacy imports always start the
        // per-node lane fresh (every theorem-stating node Unknown), which
        // is the natural behaviour given the lane is brand-new in the
        // post-2026-04-29 protocol version. Empty here means
        // `current_substantiveness_state` returns Unknown for all present nodes
        // — exactly right for "legacy state freshly upgraded".
        substantiveness_current_fingerprints: BTreeMap::new(),
        // Narrow Lean type-surface closure isn't derivable from the
        // legacy paper_status shape either; left empty so the next
        // worker observation re-populates it from cached
        // `lean_semantic_payload` sidecars. AdvancePhase Approve from
        // a freshly-imported legacy state therefore snapshots an empty
        // `approved_targets.protected_closure_nodes` until the first
        // post-import worker burst hydrates the slot — matching the
        // covering-only protection legacy state already had.
        protected_closure_nodes_per_target: BTreeMap::new(),
    };

    let legacy_phase = string_field(&legacy_state, "phase");
    let phase = map_phase(&legacy_phase)?;
    let legacy_cycle = legacy_state
        .get("cycle")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let legacy_resume_from = string_field(&legacy_state, "resume_from");
    let awaiting_human_input = bool_field(&legacy_state, "awaiting_human_input");
    let mut notes = Vec::new();
    let stage = if phase == Phase::Complete {
        Stage::Complete
    } else if awaiting_human_input {
        notes.push("legacy awaiting_human_input normalized to HumanGate".to_string());
        Stage::HumanGate
    } else if !legacy_resume_from.is_empty() {
        if legacy_resume_from == "verification" {
            notes.push(
                "legacy resume_from=verification normalized to Reviewer; partial in-flight verification was discarded".to_string(),
            );
        } else if legacy_resume_from == "reviewer" {
            notes.push("legacy resume_from=reviewer normalized to Reviewer".to_string());
        }
        Stage::Reviewer
    } else {
        Stage::Reviewer
    };

    let mut state = ProtocolState::default();
    state.phase = phase;
    state.stage = stage;
    state.cycle = legacy_cycle;
    state.configured_targets = configured_targets.clone();
    state.approved_targets = import_approved_targets(&legacy_state, &configured);
    state.node_kinds = node_kinds.clone();
    state.committed_node_kinds = node_kinds;
    state.proof_nodes = proof_nodes.clone();
    state.committed_proof_nodes = proof_nodes;
    state.deps = deps.clone();
    state.committed_deps = deps;
    state.target_claims = target_claims.clone();
    state.committed_target_claims = target_claims;
    state.node_rank = ranks;
    state.live = live.clone();
    state.committed = live;
    state.corr_status = corr_status;
    state.corr_approved_fingerprints = corr_approved_fingerprints;
    state.paper_status = target_corr_status;
    state.paper_approved_fingerprints = target_corr_approved_fingerprints;
    state.sound_status = sound_status;
    state.sound_approved_fingerprints = sound_approved_fingerprints;
    state.node_difficulty = node_difficulty;
    state.easy_attempts = easy_attempts;
    state.human_input_outstanding = awaiting_human_input;
    state.invalid_attempt = legacy_state
        .get("last_worker_handoff")
        .and_then(Value::as_object)
        .map(|obj| string_field(obj, "status").eq_ignore_ascii_case("INVALID"))
        .unwrap_or(false);
    state.active_node = {
        let raw = string_field(&legacy_state, "active_node");
        if raw.is_empty() {
            None
        } else {
            Some(NodeId::from(raw))
        }
    };
    match phase {
        Phase::TheoremStating => {
            state.target_edit_mode =
                map_target_mode(&string_field(&legacy_state, "theorem_target_edit_mode"));
            state.proof_edit_mode = ProofEditMode::Local;
        }
        Phase::ProofFormalization => {
            state.proof_edit_mode =
                map_proof_mode(&string_field(&legacy_state, "proof_target_edit_mode"));
        }
        Phase::Cleanup | Phase::Complete => {}
    }
    state.gate_kind = if awaiting_human_input {
        let last_review = legacy_state
            .get("last_review")
            .and_then(Value::as_object)
            .map(|obj| string_field(obj, "decision"))
            .unwrap_or_default();
        if last_review.eq_ignore_ascii_case("ADVANCE_PHASE") {
            GateKind::Advance
        } else {
            GateKind::NeedInput
        }
    } else {
        GateKind::None
    };

    state.normalize_live_structural_state();
    state.commit_live();
    state.ensure_node_metadata();

    let legacy_active_node = state.active_node.clone();
    if state.phase == Phase::TheoremStating
        && !state.theorem_review_next_active_legal(state.active_node.as_ref())
    {
        if let Some(node) = &state.active_node {
            notes.push(format!(
                "legacy active_node `{node}` cleared because it is outside the support cone of the current blocked paper target set"
            ));
        }
        state.active_node = None;
    }
    if state.phase == Phase::TheoremStating && state.blocked_targets().is_empty() {
        state.held_target = state.select_theorem_held_target();
    }
    state.relegalize_active_fields();
    if state.in_flight_request.is_none() {
        if let Some(kind) = state.expected_request_kind() {
            let _ = state.issue_request(kind);
            notes.push(format!(
                "issued fresh {:?} request for normalized stage {:?}",
                kind, state.stage
            ));
        }
    }

    let summary = LegacyImportSummary {
        legacy_cycle,
        legacy_phase,
        legacy_resume_from,
        normalized_stage: state.stage,
        legacy_active_node,
        normalized_active_node: state.active_node.clone(),
        normalized_mode: state.current_mode(),
        held_target: state.held_target.clone(),
        blocked_targets: state.blocked_targets(),
        global_blockers: state.global_blockers(),
        notes,
    };

    Ok(LegacyImportResult {
        repo_path,
        state,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BlockerKind, BlockerObject};
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
    fn import_clears_stale_active_node_when_blocked_target_is_elsewhere() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\ntheorem A : True := by trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "import Tablet.Preamble\ntheorem B : True := by trivial\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{theorem}B\\end{theorem}\n",
        );

        let config = tmp.path().join("trellis.config.json");
        write(
            &config,
            &format!(
                "{{\"repo_path\":\"{}\",\"workflow\":{{\"main_result_targets\":[{{\"start_line\":10,\"end_line\":12,\"tex_label\":\"t.a\"}}]}}}}",
                repo.display()
            ),
        );
        let legacy_state = repo.join(".trellis/state.json");
        write(
            &legacy_state,
            r#"{
              "cycle": 8,
              "phase": "theorem_stating",
              "active_node": "B",
              "theorem_target_edit_mode": "repair",
              "resume_from": "verification"
            }"#,
        );
        let legacy_tablet = repo.join(".trellis/tablet.json");
        write(
            &legacy_tablet,
            r#"{
              "nodes": {
                "A": {
                  "status": "open",
                  "difficulty": "easy",
                  "paper_provenance": {"start_line":10,"end_line":12,"tex_label":"t.a"},
                  "verification_content_hash": "fp-A"
                },
                "B": {
                  "status": "open",
                  "difficulty": "easy",
                  "correspondence_status": "pass",
                  "correspondence_content_hash": "fp-B",
                  "soundness_status": "pass",
                  "soundness_content_hash": "snd-B"
                }
              }
            }"#,
        );

        let imported = import_legacy_project(&config, None, None).unwrap();
        assert_eq!(imported.state.stage, Stage::Reviewer);
        assert_eq!(imported.state.active_node, None);
        assert_eq!(
            imported.state.in_flight_request.as_ref().map(|r| r.kind),
            Some(crate::model::RequestKind::Review)
        );
        assert!(imported.summary.blocked_targets.contains("t.a"));
        assert!(imported
            .summary
            .notes
            .iter()
            .any(|note| note.contains("partial in-flight verification was discarded")));
        assert!(imported
            .summary
            .notes
            .iter()
            .any(|note| note.contains("active_node `B` cleared")));
    }

    #[test]
    fn import_reanchors_soundness_fingerprints_to_current_repo_state() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "import Tablet.Preamble\n\ntheorem B : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{theorem}\\label{t.b}B\\end{theorem}\n",
        );
        write(
            &repo.join("Tablet/A.lean"),
            "import Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}\\label{t.a}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
        );

        let config = tmp.path().join("trellis.config.json");
        write(
            &config,
            &format!(
                "{{\"repo_path\":\"{}\",\"workflow\":{{\"main_result_targets\":[{{\"start_line\":10,\"end_line\":12,\"tex_label\":\"t.a\"}}]}}}}",
                repo.display()
            ),
        );
        let legacy_state = repo.join(".trellis/state.json");
        write(
            &legacy_state,
            r#"{
              "cycle": 8,
              "phase": "theorem_stating",
              "resume_from": "reviewer"
            }"#,
        );
        let legacy_tablet = repo.join(".trellis/tablet.json");
        write(
            &legacy_tablet,
            r#"{
              "nodes": {
                "A": {
                  "status": "open",
                  "difficulty": "hard",
                  "paper_provenance": {"start_line":10,"end_line":12,"tex_label":"t.a"},
                  "correspondence_status": "pass",
                  "correspondence_content_hash": "corr-A",
                  "soundness_status": "pass",
                  "soundness_content_hash": "legacy-sound-A"
                },
                "B": {
                  "status": "open",
                  "difficulty": "hard",
                  "correspondence_status": "pass",
                  "correspondence_content_hash": "corr-B",
                  "soundness_status": "pass",
                  "soundness_content_hash": "legacy-sound-B"
                }
              }
            }"#,
        );

        let imported = import_legacy_project(&config, None, None).unwrap();
        let expected = current_repo_sound_fingerprint(&repo, "A", &imported.state.deps);
        assert!(!expected.is_empty());
        assert_eq!(
            imported.state.live.sound_current_fingerprints.get("A"),
            Some(&expected)
        );
        assert_eq!(
            imported.state.sound_approved_fingerprints.get("A"),
            Some(&expected)
        );
        assert_ne!(expected, "legacy-sound-A");
    }

    #[test]
    fn import_treats_preamble_as_present_non_proof_and_explicit_dependency() {
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
            "\\begin{theorem}\\label{t.a}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let config = tmp.path().join("trellis.config.json");
        write(
            &config,
            &format!(
                "{{\"repo_path\":\"{}\",\"workflow\":{{\"main_result_targets\":[{{\"start_line\":10,\"end_line\":12,\"tex_label\":\"t.a\"}}]}}}}",
                repo.display()
            ),
        );
        let legacy_state = repo.join(".trellis/state.json");
        write(
            &legacy_state,
            r#"{
              "cycle": 8,
              "phase": "theorem_stating",
              "resume_from": "reviewer"
            }"#,
        );
        let legacy_tablet = repo.join(".trellis/tablet.json");
        write(
            &legacy_tablet,
            r#"{
              "nodes": {
                "A": {
                  "status": "open",
                  "difficulty": "hard",
                  "paper_provenance": {"start_line":10,"end_line":12,"tex_label":"t.a"},
                  "correspondence_status": "pass",
                  "correspondence_content_hash": "corr-A"
                }
              }
            }"#,
        );

        let imported = import_legacy_project(&config, None, None).unwrap();
        assert!(imported.state.live.present_nodes.contains("Preamble"));
        assert!(!imported.state.proof_nodes.contains("Preamble"));
        assert!(!imported.state.live.open_nodes.contains("Preamble"));
        assert_eq!(
            imported.state.deps.get("A"),
            Some(&BTreeSet::from([NodeId::from("Preamble")]))
        );
        assert_eq!(
            imported
                .state
                .live
                .corr_current_fingerprints
                .get("Preamble"),
            Some(&String::new())
        );
        assert!(imported.state.current_corr_pass(&NodeId::from("Preamble")));
        assert!(!imported.state.global_blockers().contains(&Blocker {
            kind: BlockerKind::NodeCorr,
            object: BlockerObject::Node {
                node: NodeId::from("Preamble"),
            },
            fingerprint: String::new(),
            deferred: false,
        }));
    }

    #[test]
    fn import_resolves_legacy_state_dir_from_config() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join(".agent-supervisor")).unwrap();
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
            "\\begin{theorem}\\label{t.a}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let config = tmp.path().join("lagent.config.json");
        write(
            &config,
            &format!(
                "{{\"repo_path\":\"{}\",\"state_dir\":\".agent-supervisor\",\"workflow\":{{\"main_result_targets\":[{{\"start_line\":10,\"end_line\":12,\"tex_label\":\"t.a\"}}]}}}}",
                repo.display()
            ),
        );
        let legacy_state = repo.join(".agent-supervisor/state.json");
        write(
            &legacy_state,
            r#"{
              "cycle": 12,
              "phase": "theorem_stating",
              "resume_from": "reviewer"
            }"#,
        );
        let legacy_tablet = repo.join(".agent-supervisor/tablet.json");
        write(
            &legacy_tablet,
            r#"{
              "nodes": {
                "A": {
                  "status": "open",
                  "difficulty": "hard",
                  "paper_provenance": {"start_line":10,"end_line":12,"tex_label":"t.a"},
                  "correspondence_status": "pass",
                  "correspondence_content_hash": "corr-A"
                }
              }
            }"#,
        );

        let imported = import_legacy_project(&config, None, None).unwrap();
        assert_eq!(imported.state.cycle, 12);
        assert!(imported.state.live.present_nodes.contains("A"));
        assert_eq!(imported.summary.legacy_cycle, 12);
    }
}

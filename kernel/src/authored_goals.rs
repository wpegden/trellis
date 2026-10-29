//! Connect authored PV goals to the existing Decide/dormant-store lifecycle.
//! Positive statements retain ordinary authoring and semantic review. Only their
//! machine-derived negations are prescribed; neither side receives a review waiver.
use crate::model::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Authored statements are repository observations, so event replay records
/// their accepted registry separately from the pure protocol transition.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Registration {
    pub goals: BTreeMap<ChallengeTargetId, TargetId>,
    pub specs: BTreeMap<ChallengeTargetId, ChallengeTargetSpec>,
}

impl Registration {
    pub fn capture(state: &ProtocolState) -> Self {
        let specs = state.authored_decide_goals.keys()
            .flat_map(|id| [id.clone(), refutation_target_id(id)])
            .filter_map(|id| state.configured_challenge_targets.get(&id).cloned().map(|spec| (id, spec)))
            .collect();
        Self { goals: state.authored_decide_goals.clone(), specs }
    }

    pub fn restore(&self, state: &mut ProtocolState) {
        for id in state.authored_decide_goals.keys() {
            if self.specs.get(id) != state.configured_challenge_targets.get(id) {
                state.pv_refutation_findings.remove(id);
            }
            if !self.goals.contains_key(id) {
                state.pv_live_polarity.remove(id);
                state.pv_polarity_flip_count.remove(id);
            }
            state.configured_challenge_targets.remove(id);
            state.configured_challenge_targets.remove(&refutation_target_id(id));
        }
        state.authored_decide_goals = self.goals.clone();
        state.configured_challenge_targets.extend(self.specs.clone());
        state.normalize_all_structural_state();
        if let Some(request) = state.in_flight_request.clone() {
            state.in_flight_request = Some(Box::new(state.expected_request(request.id, request.kind)));
        }
    }
}

pub fn statement(repo: &Path, node: &NodeId) -> Result<ChallengeTargetSpec, String> {
    let spec = read_statement(repo, node)?;
    refutation_statement(&spec)?;
    Ok(spec)
}

/// Check a new or revised statement with the pinned Lean frontend. Unchanged
/// legacy registrations retain their exact historical refutation bytes.
pub fn checked_statement(repo: &Path, node: &NodeId, previous: Option<&ChallengeTargetSpec>) -> Result<ChallengeTargetSpec, String> {
    let mut spec = read_statement(repo, node)?;
    if let Some(old) = previous.filter(|old| old.statement_derivation.is_none()) {
        if *old == spec { return Ok(old.clone()); }
    }
    let input = crate::statement_transform::Input::new(&spec, None);
    let receipt = crate::statement_transform::observe(repo, &input);
    receipt.checked(&input)?;
    spec.statement_derivation = Some(receipt);
    Ok(spec)
}

fn read_statement(repo: &Path, node: &NodeId) -> Result<ChallengeTargetSpec, String> {
    let source = fs::read_to_string(repo.join("Tablet").join(format!("{node}.lean")))
        .map_err(|e| e.to_string())?;
    let region = crate::filespec_split::prescribed_region(&source, false)?;
    let start = region.split_inclusive('\n').scan(0usize, |offset, line| {
        let start = *offset; *offset += line.len(); Some((start, line))
    }).find(|(_, line)| line.starts_with("theorem ") || line.starts_with("lemma "))
        .map(|(offset, _)| offset).ok_or_else(|| format!("goal {node} needs a theorem or lemma statement"))?;
    let head = &region[start..];
    let lean = if let Some(tail) = head.strip_prefix("lemma") { format!("theorem{tail}") } else { head.to_string() };
    let tex = fs::read_to_string(repo.join("Tablet").join(format!("{node}.tex"))).map_err(|e| e.to_string())?;
    let parsed = crate::filespec::parse_top_level_envs(&tex);
    let env = parsed.envs.first().ok_or_else(|| format!("goal {node} has no NL statement"))?;
    let begin = format!("\\begin{{{env}}}");
    let end = format!("\\end{{{env}}}");
    let informal = tex.split_once(&begin).and_then(|(_, t)| t.split_once(&end))
        .map(|(t, _)| t.trim().to_string()).ok_or_else(|| format!("goal {node} has malformed NL"))?;
    let spec = ChallengeTargetSpec {
        kind: ChallengeTargetKind::Theorem, name: node.as_str().to_string(), lean,
        namespace_context: source.lines().take_while(|line| !line.trim_start().starts_with("-- [TABLET NODE:")).collect::<Vec<_>>().join("\n"), informal,
        resolution: ChallengeResolution::Decide, ..Default::default()
    };
    Ok(spec)
}

/// Refresh only at accepted repository boundaries, never over worker WIP.
/// This preserves the standard mutable-positive-goal workflow and resets a
/// dormant refutation when the proposition it negates changes.
pub fn sync(repo: &Path, state: &mut ProtocolState) -> Result<(), String> {
    if !state.authored_goals_decidable { return Ok(()); }
    let mut wanted = BTreeMap::new();
    for goal in &state.configured_targets {
        let nodes = state.live.coverage.get(goal).cloned().unwrap_or_default();
        if nodes.is_empty() { continue; }
        if nodes.len() != 1 {
            return Err(format!("goal {goal} needs one principal statement node (found {}); supporting nodes belong in its dependencies", nodes.len()));
        }
        let node = nodes.iter().next().unwrap();
        if !state.proof_nodes.contains(node) { return Err(format!("goal {goal} must be a proof-bearing statement")); }
        let primary = state.authored_decide_primary_for_node(node)
            .unwrap_or_else(|| ChallengeTargetId::from(format!("authored:{node}")));
        if wanted.insert(primary.clone(), goal.clone()).is_some() {
            return Err(format!("goal node {node} claims multiple supplied goals; each goal needs its own statement"));
        }
        if state.live_polarity(&primary) == ChallengePolarity::Disprove { continue; }
        let spec = if state.lean_statement_transforms_enabled {
            checked_statement(repo, node, state.configured_challenge_targets.get(&primary))?
        } else { statement(repo, node)? };
        let refutation_id = refutation_target_id(&primary);
        let refutation = ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem, name: refutation_node_name(&spec.name),
            lean: refutation_statement(&spec)?, namespace_context: spec.namespace_context.clone(),
            informal: refutation_informal(&spec), ..Default::default()
        };
        let changed = state.configured_challenge_targets.get(&primary).is_some_and(|old|
            old.lean != spec.lean || old.namespace_context != spec.namespace_context
                || state.configured_challenge_targets.get(&refutation_id).is_some_and(|old_ref| old_ref.lean != refutation.lean));
        if !state.authored_decide_goals.contains_key(&primary)
            && (state.configured_challenge_targets.contains_key(&primary)
                || state.live.present_nodes.contains(&NodeId::from(refutation.name.as_str()))) {
            return Err(format!("derived goal pair {primary} collides with an existing target or node"));
        }
        if changed {
            // The previous negative proof refers to the previous proposition.
            // Preserve it in git history; regenerate only the dormant derived side.
            for ext in ["lean", "tex"] {
                let path = repo.join("Dormant").join(format!("{}.{ext}", refutation.name));
                if path.exists() { fs::remove_file(path).map_err(|e| e.to_string())?; }
            }
            state.pv_refutation_findings.remove(&primary);
        }
        crate::dormant_store::seed_dormant_refutation_file(repo, node.as_str(), &refutation.name,
            &refutation.lean, &refutation.informal)?;
        state.configured_challenge_targets.insert(primary.clone(), spec);
        state.configured_challenge_targets.insert(refutation_id, refutation);
    }
    let retired: Vec<_> = state.authored_decide_goals.keys().filter(|id| !wanted.contains_key(*id)).cloned().collect();
    for id in retired {
        if state.live_polarity(&id) == ChallengePolarity::Disprove {
            return Err(format!("restore the positive direction before retiring goal pair {id}"));
        }
        if let Some(spec) = state.configured_challenge_targets.remove(&refutation_target_id(&id)) {
            for ext in ["lean", "tex"] {
                let path = repo.join("Dormant").join(format!("{}.{ext}", spec.name));
                if path.exists() { fs::remove_file(path).map_err(|e| e.to_string())?; }
            }
        }
        state.configured_challenge_targets.remove(&id);
        state.pv_live_polarity.remove(&id);
        state.pv_polarity_flip_count.remove(&id);
        state.pv_refutation_findings.remove(&id);
    }
    state.authored_decide_goals = wanted;
    state.normalize_all_structural_state();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authored_statement_revision_regenerates_exact_negation_without_waiving_reviews() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir(repo.join("Tablet")).unwrap();
        let node = NodeId::from("Goal");
        let goal = TargetId::from("input_goal");
        let path = repo.join("Tablet/Goal.lean");
        fs::write(&path, "-- [TABLET NODE: Goal]\ntheorem Goal (n : Nat) : n < 0 := by\n-- BODY\n  sorry\n").unwrap();
        fs::write(repo.join("Tablet/Goal.tex"), "\\begin{theorem}Every natural number is negative.\\end{theorem}\n\\begin{proof}SKETCH:\\end{proof}").unwrap();
        let mut state = ProtocolState::default();
        state.authored_goals_decidable = true;
        state.pv_tablet_configured = true;
        state.pv_goal_mode = PvGoalMode::Prose;
        state.configured_targets.insert(goal.clone());
        state.live.present_nodes.insert(node.clone());
        state.live.open_nodes.insert(node.clone());
        state.node_kinds.insert(node.clone(), NodeKind::Proof);
        state.deps.insert(node.clone(), Default::default());
        state.target_claims.insert(node.clone(), std::collections::BTreeSet::from([goal.clone()]));
        state.normalize_all_structural_state();
        state.ensure_node_metadata();
        sync(repo, &mut state).unwrap();
        state.validate().unwrap();
        let id = ChallengeTargetId::from("authored:Goal");
        assert!(state.is_decide_primary(&id));
        let registered = Registration::capture(&state);
        let mut replay = state.clone();
        replay.configured_challenge_targets.clear();
        replay.authored_decide_goals.clear();
        registered.restore(&mut replay);
        assert_eq!(state.configured_challenge_targets, replay.configured_challenge_targets);
        assert_eq!(state.live, replay.live);
        assert!(!state.substantiveness_waived(&node));
        assert!(!state.correspondence_waived(&node));
        assert_eq!(state.live.challenge_coverage[&id], std::collections::BTreeSet::from([node.clone()]));
        let neg = repo.join("Dormant/Goal__Refutation.lean");
        assert!(fs::read_to_string(&neg).unwrap().contains("¬ (∀ (n : Nat), n < 0)"));
        fs::write(&path, "-- [TABLET NODE: Goal]\ntheorem Goal (n : Nat) : n ≤ 0 := by\n-- BODY\n  sorry\n").unwrap();
        sync(repo, &mut state).unwrap();
        assert!(fs::read_to_string(&neg).unwrap().contains("¬ (∀ (n : Nat), n ≤ 0)"));
        let ref_node = NodeId::from("Goal__Refutation");
        state.challenge_claims.insert(ref_node.clone(), std::collections::BTreeSet::from([refutation_target_id(&id)]));
        assert!(!state.substantiveness_waived(&ref_node));
        assert!(!state.correspondence_waived(&ref_node));
    }
}

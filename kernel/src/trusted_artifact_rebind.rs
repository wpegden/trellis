//! Trusted, offline continuation of recorded judgments across a cold rebuild.
//! This module is pure: observers and certificate issuance belong to maintenance,
//! and replay consumes the recorded transition without contacting a provider.
use crate::model::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;

pub fn check_scope(state: &ProtocolState) -> Result<(), String> {
    if state.tablet_target != crate::backend::BackendId::Lean {
        return Err("remote resume supports Lean checkpoints only".into());
    }
    if state.is_pv()
        || state.trust_base != TrustBaseProtocolState::default()
        || !state.node_role.is_empty()
        || !state.configured_challenge_targets.is_empty()
        || !state.pv_live_polarity.is_empty()
        || !state.pv_authored_statements.is_empty()
        || state.assumption_authoring.is_some()
        || state.revision_context.is_some()
        || state.phase == Phase::RevisionStating
    {
        return Err("remote resume does not support PV, RequiredV1, challenge, or revision approval surfaces".into());
    }
    if state.corr_fingerprint_schema_version
        != crate::runtime_cli_observations::CORR_FINGERPRINT_SCHEMA_VERSION
        || state.sound_assessment_schema_version != SOUND_ASSESSMENT_SCHEMA_VERSION
    {
        return Err(
            "remote resume requires the current correspondence and Sound assessment schemas".into(),
        );
    }
    if state.in_flight_request.is_some() {
        return Err(
            "remote resume requires an idle checkpoint; the recorded request must not be cleared"
                .into(),
        );
    }
    if !matches!(
        state.stage,
        Stage::Start | Stage::HumanGate | Stage::Complete
    ) {
        return Err(format!(
            "unsupported remote resume boundary: {:?}",
            state.stage
        ));
    }
    Ok(())
}

/// Compare effective production lane predicates, including the *whole* Sound
/// category/origin/vote/action payload, and the scheduler's actual frontiers.
/// Fingerprints themselves are evidence, not scheduling intent.
pub fn scheduling_intent(state: &ProtocolState) -> Value {
    let blockers =
        |items: std::collections::BTreeSet<Blocker>| -> std::collections::BTreeSet<Blocker> {
            items
                .into_iter()
                .map(|mut b| {
                    b.fingerprint.clear();
                    b
                })
                .collect()
        };
    let nodes: BTreeMap<_, _> = state.live.present_nodes.iter().map(|node| {
        let mut sound = state.current_sound_assessment(node);
        sound.fingerprints = SoundFingerprintParts::default();
        (node, json!({"corr": format!("{:?}", state.current_corr_state(node)),
            "substantiveness": format!("{:?}", state.current_substantiveness_state(node)), "sound": sound}))
    }).collect();
    let targets: BTreeMap<_, _> = state
        .configured_targets
        .iter()
        .map(|target| (target, format!("{:?}", state.current_paper_state(target))))
        .collect();
    json!({"nodes": nodes, "targets": targets,
        "corr": state.corr_verify_nodes(), "paper": state.paper_verify_targets(),
        "substantiveness": state.substantiveness_verify_nodes(),
        "sound": state.sound_verify_nodes(), "deviation": state.deviation_verify_ids(),
        "requested_sound": state.reviewer_requested_sound_verify_nodes(),
        "blockers": blockers(state.global_blockers()), "failed_blockers": blockers(state.current_failed_blockers()),
        "theorem_next": state.theorem_start_request_kind(),
        "proof_next": state.proof_start_request_kind(),
        "worker_blockers": blockers(state.request_blockers(RequestKind::Worker)),
        "review_blockers": blockers(state.request_blockers(RequestKind::Review))})
}

/// A rebuild can change elaborated semantic representation. All text and
/// structural axes must still match; a missing paper must never create a new
/// approved baseline. Sound is source based and therefore stays byte-equal.
pub fn rebind_observed_snapshot(
    state: &mut ProtocolState,
    observed: WorkingSnapshot,
) -> Result<(), String> {
    let old = state.clone();
    let mut nonsemantic = observed.clone();
    nonsemantic.corr_current_fingerprints = old.live.corr_current_fingerprints.clone();
    nonsemantic.target_fingerprints = old.live.target_fingerprints.clone();
    if nonsemantic != old.live {
        return Err("rebuild changed source, text fingerprints, coverage, Sound parts, or protected closure membership; check paper/reference paths and launch policy".into());
    }
    if observed.target_fingerprints != observed.corr_current_fingerprints
        || observed
            .corr_current_fingerprints
            .keys()
            .ne(old.live.corr_current_fingerprints.keys())
    {
        return Err("rebuild correspondence owner coverage changed".into());
    }
    for (node, new) in &observed.corr_current_fingerprints {
        let prior = &old.live.corr_current_fingerprints[node];
        validate_corr_pair(node, prior, new)?;
        if prior == new {
            continue;
        }
        let approved = state.corr_approved_fingerprints.get_mut(node);
        if let Some(approved) = approved {
            if approved == prior
                && matches!(
                    old.corr_status.get(node),
                    Some(CorrStatus::Pass | CorrStatus::Fail)
                )
            {
                *approved = new.clone();
            } else if approved == new {
                return Err(format!(
                    "rebuild would make a stale correspondence binding current for {node}"
                ));
            }
        }
    }
    if let Some(task) = state.pending_task.as_mut() {
        task.task_blockers = task
            .task_blockers
            .iter()
            .cloned()
            .map(|mut blocker| {
                if blocker.kind == BlockerKind::NodeCorr {
                    if let BlockerObject::Node { node } = &blocker.object {
                        if old.live.corr_current_fingerprints.get(node)
                            == Some(&blocker.fingerprint)
                        {
                            if let Some(new) = observed.corr_current_fingerprints.get(node) {
                                blocker.fingerprint = new.clone();
                            }
                        }
                    }
                }
                blocker
            })
            .collect();
    }
    state.live = observed;
    let prior_intent = scheduling_intent(&old);
    let next_intent = scheduling_intent(state);
    if prior_intent != next_intent {
        let differences: BTreeMap<_, _> = prior_intent
            .as_object()
            .unwrap()
            .iter()
            .filter(|(key, value)| next_intent.get(*key) != Some(*value))
            .map(|(key, value)| {
                (
                    key.clone(),
                    json!({"before": value, "after": next_intent[key]}),
                )
            })
            .collect();
        *state = old;
        return Err(format!(
            "trusted rebuild changed effective verdicts or scheduling intent: {}",
            json!(differences)
        ));
    }
    Ok(())
}

fn validate_corr_pair(node: &NodeId, old: &str, new: &str) -> Result<(), String> {
    if node.as_str() == "Preamble" && old.is_empty() && new.is_empty() {
        return Ok(());
    }
    use crate::runtime_cli_observations::CorrespondenceFingerprint;
    // The production Preamble observer hashes its Lean/TeX interface but
    // deliberately has no principal declaration/semantic closure. Preserve
    // that exact structural fingerprint; never use this exception to rekey it.
    if node.as_str() == "Preamble" && old == new {
        if let Some(fp) = CorrespondenceFingerprint::from_storage_string(old) {
            if !fp.own_tex.is_empty() && !fp.preamble_tex.is_empty()
                && fp.lean_semantic_closure.is_empty() && fp.statement_type_hash.is_empty()
                && fp.lean_relevant_definition_descendants.is_empty() && fp.lean_relevant_dependencies.is_empty() {
                return Ok(());
            }
        }
    }
    let parse = |s| {
        CorrespondenceFingerprint::from_storage_string(s)
            .filter(|f| !f.lean_semantic_closure.is_empty())
            .ok_or_else(|| format!("malformed or empty correspondence fingerprint for {node}"))
    };
    let a = parse(old)?;
    let b = parse(new)?;
    if a.own_tex != b.own_tex
        || a.preamble_tex != b.preamble_tex
        || a.statement_type_hash != b.statement_type_hash
        || a.lean_relevant_definition_descendants != b.lean_relevant_definition_descendants
        || a.lean_relevant_dependencies != b.lean_relevant_dependencies
    {
        return Err(format!(
            "rebuild changed non-semantic correspondence axes for {node}"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldReplacement {
    pub before_sha256: String,
    pub after: Value,
}

/// Top-level field patches keep the event proportional to transformed evidence
/// and explicitly bind replay to its input generation. Unrelated history,
/// counters, pending tasks, reviewer decisions and provenance cannot be edited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedArtifactRebind {
    pub version: u32,
    pub input_generation: String,
    pub state_before_sha256: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, FieldReplacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_fields: Option<Value>,
}

pub fn digest(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("JSON value"))
    )
}

fn mutable_field(key: &str) -> bool {
    matches!(
        key,
        "live"
            | "committed"
            | "last_clean_live"
            | "corr_approved_fingerprints"
            | "last_clean_corr_approved_fingerprints"
            | "pending_task"
            | "local_closure_records"
            | "committed_local_closure_records"
            | "last_clean_local_closure_records"
            | "configured_reference_papers"
            | "boundary_statement_consumers"
            | "strict_dep_consumers"
    )
}

impl TrustedArtifactRebind {
    pub fn between(
        before: &ProtocolState,
        after: &ProtocolState,
        input_generation: String,
    ) -> Result<Self, String> {
        validate_transformation(before, after)?;
        let a = serde_json::to_value(before).map_err(|e| e.to_string())?;
        let b = serde_json::to_value(after).map_err(|e| e.to_string())?;
        let mut fields = BTreeMap::new();
        for (key, value) in b.as_object().ok_or("state is not an object")? {
            if a.get(key) != Some(value) {
                if !mutable_field(key) {
                    return Err(format!(
                        "rebuild attempted to change protected state field {key}"
                    ));
                }
                fields.insert(
                    key.clone(),
                    FieldReplacement {
                        before_sha256: digest(&a[key]),
                        after: value.clone(),
                    },
                );
            }
        }
        Ok(Self {
            version: VERSION,
            input_generation,
            state_before_sha256: digest(&a),
            fields,
            shared_fields: None,
        })
    }

    /// Version 2 carries the same field replacements through the existing
    /// structural-sharing codec; it never duplicates them in plain JSON.
    pub fn compact(mut self) -> Result<Self, String> {
        self.shared_fields = Some(crate::trusted_checkpoint_boundary::compact_document(
            &json!({"state": self.fields}))?);
        self.fields.clear();
        self.version = 2;
        Ok(self)
    }

    pub fn apply(&self, before: &ProtocolState) -> Result<ProtocolState, String> {
        if !matches!(self.version, VERSION | 2)
            || self.input_generation.len() != 64
            || !self.input_generation.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("unsupported or malformed trusted artifact rebind carrier".into());
        }
        let decoded_fields;
        let fields = if self.version == 2 {
            if !self.fields.is_empty() { return Err("compact rebind has duplicate plain fields".into()); }
            let encoded = self.shared_fields.as_ref().ok_or("compact rebind fields missing")?;
            if !crate::shared_state_codec::is_shared_state(encoded) { return Err("compact rebind fields are not shared".into()); }
            let mut document = crate::shared_state_codec::decode_shared_state(encoded.clone()).map_err(|e| e.to_string())?;
            decoded_fields = serde_json::from_value::<BTreeMap<String, FieldReplacement>>(document["state"].take()).map_err(|e| e.to_string())?;
            &decoded_fields
        } else {
            if self.shared_fields.is_some() { return Err("legacy rebind cannot carry compact fields".into()); }
            &self.fields
        };
        let mut value = serde_json::to_value(before).map_err(|e| e.to_string())?;
        if digest(&value) != self.state_before_sha256 {
            return Err("trusted artifact rebind state generation mismatch".into());
        }
        for (key, change) in fields {
            if !mutable_field(key)
                || value.get(key).map(digest).as_ref() != Some(&change.before_sha256)
            {
                return Err(format!(
                    "trusted artifact rebind input generation mismatch: {key}"
                ));
            }
            value[key] = change.after.clone();
        }
        let mut after: ProtocolState = serde_json::from_value(value).map_err(|e| e.to_string())?;
        recompute_local_closure_reverse_indices(&mut after);
        validate_transformation(before, &after)?;
        Ok(after)
    }
}

/// Validate live and actual activation projections. In particular LastClean has
/// its own legacy assessment mirrors; it must not receive live approvals.
pub fn validate_transformation(
    before: &ProtocolState,
    after: &ProtocolState,
) -> Result<(), String> {
    check_scope(before)?;
    check_scope(after)?;
    let mut references = before.configured_reference_papers.clone();
    for (id, spec) in &mut references {
        let next = after
            .configured_reference_papers
            .get(id)
            .ok_or("rebuild removed a reference paper")?;
        if spec.tex_path != next.tex_path {
            let relative = std::path::Path::new(&next.tex_path);
            if !std::path::Path::new(&spec.tex_path).is_absolute()
                || relative.is_absolute()
                || relative
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
                || next.tex_path.is_empty()
                || !std::path::Path::new(&spec.tex_path).ends_with(relative)
            {
                return Err(
                    "reference path rebind must retain its repository-relative suffix".into(),
                );
            }
            spec.tex_path = next.tex_path.clone();
        }
    }
    if references != after.configured_reference_papers {
        return Err("rebuild changed reference paper identity".into());
    }
    for tier in ["live", "committed", "last_clean"] {
        if tier == "last_clean" && !before.last_clean_local_closure_mirror_ready {
            continue;
        }
        let mut a = before.clone();
        let mut b = after.clone();
        if tier == "committed" {
            a.restore_committed();
            b.restore_committed();
        }
        if tier == "last_clean" {
            a.apply_last_clean_reset()?;
            b.apply_last_clean_reset()?;
        }
        let mut expected = a.clone();
        rebind_observed_snapshot(&mut expected, b.live.clone())?;
        if expected.corr_approved_fingerprints != b.corr_approved_fingerprints
            || (tier == "live" && expected.pending_task != b.pending_task)
            || scheduling_intent(&a) != scheduling_intent(&b)
        {
            return Err(format!(
                "trusted rebuild changes approvals or scheduled work in {tier}"
            ));
        }
    }
    if before.request_allowed_resets(RequestKind::Review)
        != after.request_allowed_resets(RequestKind::Review)
    {
        return Err("rebuild changes available reviewer rollback choices".into());
    }
    after.validate_local_closure_root_consistency()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{apply_event, ProtocolEvent};
    fn fp(semantic: &str) -> String {
        json!({"own_tex":"text", "lean_semantic_closure":semantic, "preamble_tex":"preamble"})
            .to_string()
    }
    #[test]
    fn structural_preamble_is_preserved_but_missing_proof_semantics_are_rejected() {
        let structural = fp("");
        assert!(validate_corr_pair(&"Preamble".into(), &structural, &structural).is_ok());
        assert!(validate_corr_pair(&"Proof".into(), &structural, &structural).is_err());
        assert!(validate_corr_pair(&"Preamble".into(), &structural, &fp("new")).is_err());
        assert!(validate_corr_pair(&"Preamble".into(), "{}", "{}").is_err());
        let changed = json!({"own_tex":"changed", "lean_semantic_closure":"", "preamble_tex":"preamble"}).to_string();
        assert!(validate_corr_pair(&"Preamble".into(), &structural, &changed).is_err());
        for axis in ["preamble_tex", "statement_type_hash", "lean_relevant_definition_descendants", "lean_relevant_dependencies"] {
            let mut changed: Value = serde_json::from_str(&structural).unwrap();
            changed[axis] = json!("changed");
            assert!(validate_corr_pair(&"Preamble".into(), &structural, &changed.to_string()).is_err());
        }
        for (status, approved) in [(CorrStatus::Pass, structural.clone()), (CorrStatus::Fail, structural.clone()),
            (CorrStatus::Pass, fp("stale")), (CorrStatus::Unknown, String::new())] {
            let mut before = ProtocolState::default();
            before.corr_fingerprint_schema_version = 4;
            let node: NodeId = "Preamble".into();
            before.live.present_nodes.insert(node.clone());
            before.node_kinds.insert(node.clone(), NodeKind::Preamble);
            before.live.corr_current_fingerprints.insert(node.clone(), structural.clone());
            before.live.target_fingerprints = before.live.corr_current_fingerprints.clone();
            before.corr_status.insert(node.clone(), status);
            before.corr_approved_fingerprints.insert(node, approved);
            before.committed = before.live.clone();
            before.committed_node_kinds = before.node_kinds.clone();
            let mut after = before.clone();
            rebind_observed_snapshot(&mut after, before.live.clone()).unwrap();
            assert_eq!(after, before);
            let event = TrustedArtifactRebind::between(&before, &after, "a".repeat(64)).unwrap().compact().unwrap();
            assert_eq!(event.apply(&before).unwrap(), before);
            assert_eq!(scheduling_intent(&after), scheduling_intent(&before));
        }
    }
    fn fixture() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.corr_fingerprint_schema_version = 4;
        state.phase = Phase::ProofFormalization;
        state.cycle = 31;
        for name in ["Aligned", "Stale", "Failed", "Accepted", "Pinned"] {
            let node: NodeId = name.into();
            state.live.present_nodes.insert(node.clone());
            state.live.open_nodes.insert(node.clone());
            state.node_kinds.insert(node.clone(), NodeKind::Proof);
            state.proof_nodes.insert(node.clone());
            state
                .live
                .corr_current_fingerprints
                .insert(node.clone(), fp("old"));
            state.corr_status.insert(
                node.clone(),
                if name == "Failed" {
                    CorrStatus::Fail
                } else {
                    CorrStatus::Pass
                },
            );
            state.corr_approved_fingerprints.insert(
                node.clone(),
                fp(if name == "Stale" { "stale" } else { "old" }),
            );
            let parts = SoundFingerprintParts {
                own_tex_hash: "sound text".into(),
                combined_sound_fp: "combined".into(),
                ..Default::default()
            };
            state
                .live
                .sound_current_fingerprints
                .insert(node.clone(), "combined".into());
            state
                .live
                .sound_current_fingerprint_parts
                .insert(node.clone(), parts.clone());
            state.sound_assessments.insert(
                node,
                SoundAssessment {
                    status: if name == "Accepted" {
                        SoundAssessmentStatus::ReviewerAcceptedPass
                    } else if name == "Pinned" {
                        SoundAssessmentStatus::ReviewerPinnedFail
                    } else {
                        SoundAssessmentStatus::VerifierPass
                    },
                    fingerprints: parts,
                    reviewer_action_id: Some(19),
                    ..Default::default()
                },
            );
        }
        state.live.target_fingerprints = state.live.corr_current_fingerprints.clone();
        state.committed = state.live.clone();
        state.committed_node_kinds = state.node_kinds.clone();
        state.committed_proof_nodes = state.proof_nodes.clone();
        state
            .reviewer_requested_sound_verifier_nodes
            .insert("Accepted".into());
        state
    }
    fn observed(state: &ProtocolState, value: &str) -> WorkingSnapshot {
        let mut next = state.live.clone();
        for current in next.corr_current_fingerprints.values_mut() {
            *current = fp(value);
        }
        next.target_fingerprints = next.corr_current_fingerprints.clone();
        next
    }
    #[test]
    fn representation_rebind_preserves_effective_lanes_and_replays_without_dispatch() {
        let before = fixture();
        let mut after = before.clone();
        rebind_observed_snapshot(&mut after, observed(&before, "new representation")).unwrap();
        after.committed = after.live.clone();
        assert_eq!(
            after.corr_approved_fingerprints[&NodeId::from("Stale")],
            fp("stale")
        );
        assert_eq!(
            after.current_corr_state(&"Failed".into()),
            CurrentCheckState::Fail
        );
        assert_eq!(
            after.current_sound_assessment(&"Accepted".into()).status,
            SoundAssessmentStatus::ReviewerAcceptedPass
        );
        assert_eq!(
            after.current_sound_assessment(&"Pinned".into()).status,
            SoundAssessmentStatus::ReviewerPinnedFail
        );
        assert_eq!(scheduling_intent(&before), scheduling_intent(&after));
        let payload = TrustedArtifactRebind::between(&before, &after, "a".repeat(64)).unwrap();
        let outcome = apply_event(
            before.clone(),
            ProtocolEvent::TrustedArtifactRebind { payload },
        )
        .unwrap();
        assert_eq!(outcome.state, after);
        assert!(outcome.commands.is_empty());
        assert_eq!(outcome.state.cycle, before.cycle);
        let again = observed(&after, "new representation");
        rebind_observed_snapshot(&mut after, again).unwrap();
        assert!(
            TrustedArtifactRebind::between(&outcome.state, &after, "a".repeat(64))
                .unwrap()
                .fields
                .is_empty()
        );
    }
    #[test]
    fn stale_collision_and_missing_text_fail_closed() {
        let before = fixture();
        let mut state = before.clone();
        assert!(
            rebind_observed_snapshot(&mut state, observed(&before, "stale"))
                .unwrap_err()
                .contains("stale")
        );
        let mut changed = observed(&before, "new");
        changed
            .substantiveness_current_fingerprints
            .insert("Aligned".into(), "missing paper baseline".into());
        assert!(rebind_observed_snapshot(&mut before.clone(), changed)
            .unwrap_err()
            .contains("paper/reference"));
        for malformed in ["", "{}", "legacy", "null"] {
            let mut changed = observed(&before, "new");
            changed
                .corr_current_fingerprints
                .insert("Aligned".into(), malformed.into());
            changed.target_fingerprints = changed.corr_current_fingerprints.clone();
            assert!(rebind_observed_snapshot(&mut before.clone(), changed).is_err());
        }
    }
    #[test]
    fn replay_cannot_change_provenance_counters_or_reviewer_requests() {
        let before = fixture();
        let mut after = before.clone();
        after.cycle += 1;
        assert!(TrustedArtifactRebind::between(&before, &after, "a".repeat(64)).is_err());
        let mut after = before.clone();
        after.reviewer_requested_sound_verifier_nodes.clear();
        assert!(TrustedArtifactRebind::between(&before, &after, "a".repeat(64)).is_err());
        let mut payload = TrustedArtifactRebind::between(&before, &before, "a".repeat(64)).unwrap();
        payload.fields.insert(
            "cycle".into(),
            FieldReplacement {
                before_sha256: digest(&json!(31)),
                after: json!(32),
            },
        );
        assert!(payload.apply(&before).is_err());
    }
    #[test]
    fn unsupported_schema_and_inflight_are_diagnostic() {
        let mut state = fixture();
        state.sound_assessment_schema_version = 0;
        assert!(check_scope(&state).unwrap_err().contains("schemas"));
        state = fixture();
        state.tablet_target = crate::backend::BackendId::IsabelleHol;
        assert!(check_scope(&state).unwrap_err().contains("Lean"));
        state = fixture();
        state.in_flight_request = Some(Box::new(state.expected_request(1, RequestKind::Worker)));
        assert!(check_scope(&state)
            .unwrap_err()
            .contains("must not be cleared"));
    }

    #[test]
    fn last_clean_rebind_uses_its_own_approvals_and_stale_sound_stays_stale() {
        let mut before = fixture();
        before
            .sound_assessments
            .get_mut(&NodeId::from("Aligned"))
            .unwrap()
            .fingerprints
            .own_tex_hash = "old text".into();
        before.last_clean_live = observed(&before, "historical");
        before.last_clean_node_kinds = before.node_kinds.clone();
        before.last_clean_proof_nodes = before.proof_nodes.clone();
        before.last_clean_corr_status = before.corr_status.clone();
        before.last_clean_corr_approved_fingerprints =
            before.last_clean_live.corr_current_fingerprints.clone();
        before
            .last_clean_corr_approved_fingerprints
            .insert("Stale".into(), fp("older stale"));
        before.last_clean_verifier_mirror_ready = true;
        before.last_clean_local_closure_mirror_ready = true;
        let mut after = before.clone();
        rebind_observed_snapshot(&mut after, observed(&before, "new live")).unwrap();
        after.committed = after.live.clone();
        let mut clean = before.clone();
        assert!(clean.apply_last_clean_reset().unwrap());
        let observed_clean = observed(&clean, "new historical");
        rebind_observed_snapshot(&mut clean, observed_clean).unwrap();
        after.last_clean_live = clean.live;
        after.last_clean_corr_approved_fingerprints = clean.corr_approved_fingerprints;
        let event = TrustedArtifactRebind::between(&before, &after, "b".repeat(64)).unwrap();
        assert_eq!(event.apply(&before).unwrap(), after);
        assert_eq!(
            after.current_sound_assessment(&"Aligned".into()),
            before.current_sound_assessment(&"Aligned".into())
        );
        assert_eq!(
            after.last_clean_corr_approved_fingerprints[&NodeId::from("Aligned")],
            fp("new historical")
        );
        assert_eq!(
            after.last_clean_corr_approved_fingerprints[&NodeId::from("Stale")],
            fp("older stale")
        );
        let mut wrong = before;
        wrong.cycle += 1;
        assert!(event.apply(&wrong).unwrap_err().contains("generation"));
    }

    #[test]
    fn different_artifact_bytes_keep_the_same_semantics_and_approvals() {
        use crate::node_certificate::*;
        use crate::trust_base::raw_sha256;
        let node: NodeId = "Preamble".into();
        let make = |bytes: &[u8]| {
            let parts = vec![ArtifactBundlePart {
                level: "exported".into(),
                sha256: raw_sha256(bytes),
                size_bytes: bytes.len() as u64,
            }];
            let certificate = issue_certificate(CertificateInputs {
                node: &node,
                module_name: "Tablet.Preamble",
                principal_declaration: "",
                closure_manifest: &[],
                replay_manifest: &[],
                visibility_manifests: &[VisibilityManifest {
                    level: "exported".into(),
                    declarations: vec![],
                }],
                artifact_bundle: &parts,
                source_sha256: raw_sha256(b"same source"),
                semantic_root: raw_sha256(b"same semantics"),
                dependency_certificate_roots: &BTreeMap::new(),
                axiom_policy_root: raw_sha256(b"policy"),
                toolchain_root: raw_sha256(b"toolchain"),
                observed_uses: &[],
                owner_certificates: &BTreeMap::new(),
            })
            .unwrap();
            assert!(certificate.roots_are_current());
            LocalClosureRecord {
                node: node.clone(),
                active_decl_hash: raw_sha256(b"same source").to_string(),
                node_certificate: Some(certificate),
                ..Default::default()
            }
        };
        let mut before = ProtocolState::default();
        before.corr_fingerprint_schema_version = 4;
        before.live.present_nodes.insert(node.clone());
        before.node_kinds.insert(node.clone(), NodeKind::Preamble);
        before
            .live
            .corr_current_fingerprints
            .insert(node.clone(), String::new());
        before.live.target_fingerprints = before.live.corr_current_fingerprints.clone();
        before.corr_approved_fingerprints = before.live.corr_current_fingerprints.clone();
        before.corr_status.insert(node.clone(), CorrStatus::Pass);
        before.committed = before.live.clone();
        before.committed_node_kinds = before.node_kinds.clone();
        before
            .local_closure_records
            .insert(node.clone(), make(b"old valid artifact bytes"));
        before.committed_local_closure_records = before.local_closure_records.clone();
        let mut after = before.clone();
        after.local_closure_records.insert(
            node.clone(),
            make(b"new valid artifact bytes of a different length"),
        );
        after.committed_local_closure_records = after.local_closure_records.clone();
        let event = TrustedArtifactRebind::between(&before, &after, "c".repeat(64)).unwrap();
        assert_eq!(event.apply(&before).unwrap(), after);
        assert_eq!(
            after.corr_approved_fingerprints,
            before.corr_approved_fingerprints
        );
        assert_eq!(scheduling_intent(&after), scheduling_intent(&before));
    }

    #[test]
    fn pending_task_keeps_routing_and_rebinds_only_its_current_blocker() {
        let mut before = fixture();
        let blocker = before
            .current_failed_blockers()
            .into_iter()
            .find(|b| b.kind == BlockerKind::NodeCorr)
            .unwrap();
        before.pending_task = Some(PendingTask {
            task_blockers: [blocker].into(),
            node: Some("Failed".into()),
            next_worker_context_mode: WorkerContextMode::Resume,
            ..Default::default()
        });
        let original_task = before.pending_task.clone().unwrap();
        let mut after = before.clone();
        rebind_observed_snapshot(&mut after, observed(&before, "new")).unwrap();
        after.committed = after.live.clone();
        let mut expected_task = original_task;
        expected_task.task_blockers = expected_task
            .task_blockers
            .into_iter()
            .map(|mut b| {
                b.fingerprint = fp("new");
                b
            })
            .collect();
        assert_eq!(after.pending_task.as_ref(), Some(&expected_task));
        assert_eq!(after.request_seq, before.request_seq);
        let event = TrustedArtifactRebind::between(&before, &after, "d".repeat(64)).unwrap();
        assert_eq!(event.apply(&before).unwrap(), after);
    }
}

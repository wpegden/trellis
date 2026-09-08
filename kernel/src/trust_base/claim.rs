//! Kernel-derived result rows for the gate and final archive.

use super::canonical::{canonical_json, raw_sha256, tagged_hash, DomainTag, Sha256Digest};
use super::records::TrustRecordSeedRoots;
use super::artifact::RustWitnessArtifactRecord;
use crate::model::{
    ChallengePolarity, ChallengeResolution, ChallengeTargetId, LocalClosureRecord, NodeId,
    ProtocolState, CANONICAL_APPROVED_AXIOMS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const CLAIM_ROWS_SCHEMA: &str = "trellis-claim-rows/v3";
pub const CLAIM_ROWS_SCHEMA_ID: &str = "trellis://schemas/claim-rows/v3";

/// Presence is explicit and remains non-authorizing result disclosure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RustArtifactSummary {
    Absent,
    Present { record: RustWitnessArtifactRecord },
}

/// The successful terminal lattice. An open target has no terminal outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalOutcome {
    Proved {
        proof_subject_sha256: Sha256Digest,
    },
    Disproved {
        proof_subject_sha256: Sha256Digest,
        artifact: RustArtifactSummary,
    },
    ConditionalTheorem {
        conditional_target_id: ChallengeTargetId,
        condition_lean: String,
        sealed_statement_lean: String,
        sealed_statement_sha256: Sha256Digest,
        approval_record_sha256: Sha256Digest,
        proof_subject_sha256: Sha256Digest,
    },
}

impl TerminalOutcome {
    pub fn rendered(&self) -> &'static str {
        match self {
            Self::Proved { .. } => "proved",
            Self::Disproved { .. } => "disproved",
            Self::ConditionalTheorem { .. } => "conditional theorem",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimEdition {
    PreGate,
    Finalization,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewableAuthoredDefinition {
    pub node: NodeId,
    pub logical_id: String,
    pub evidence_relative_path: String,
    pub raw_sha256: Sha256Digest,
    pub definition_utf8: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimHeader {
    pub edition: ClaimEdition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_episode_id: Option<String>,
    pub seed_roots: TrustRecordSeedRoots,
    pub launch_acknowledgment_sha256: Sha256Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_record_sha256: Option<Sha256Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_presentation_sha256: Option<Sha256Digest>,
    pub adaptation_ledger_sha256: Sha256Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase0: Option<crate::phase0::Phase0TrustRoots>,
    pub approved_axiom_floor: Vec<String>,
    pub authored_definitions: Vec<ReviewableAuthoredDefinition>,
    pub cycle: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureSummary {
    pub selected_target_id: String,
    pub selected_node: NodeId,
    pub fresh: bool,
    pub kernel_axioms: BTreeSet<String>,
    pub unapproved_axioms: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRow {
    pub target_id: String,
    pub informal: String,
    pub statement_name: String,
    pub statement_lean: String,
    pub statement_authority_sha256: Sha256Digest,
    pub statement_sha256: Sha256Digest,
    pub seeded_resolution: ChallengeResolution,
    pub selected_polarity: ChallengePolarity,
    pub terminal_outcome: TerminalOutcome,
    pub closure: ClosureSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRows {
    pub schema: String,
    pub header: ClaimHeader,
    pub rows: Vec<ClaimRow>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ClaimContext {
    pub edition_finalization: bool,
    pub approval_record_sha256: Option<Sha256Digest>,
    pub gate_presentation_sha256: Option<Sha256Digest>,
}

impl ClaimContext {
    pub fn pre_gate() -> Self {
        Self::default()
    }

    pub fn finalization(approval: Sha256Digest, presentation: Sha256Digest) -> Self {
        Self {
            edition_finalization: true,
            approval_record_sha256: Some(approval),
            gate_presentation_sha256: Some(presentation),
        }
    }
}

#[derive(Clone)]
struct ConditionalResult {
    conditional_target_id: ChallengeTargetId,
    condition_lean: String,
    sealed_statement_lean: String,
    sealed_statement_sha256: Sha256Digest,
    approval_record_sha256: Sha256Digest,
    proof_subject_sha256: Sha256Digest,
}

/// One closed precedence function makes result classification exclusive.
fn classify_success(
    positive: Option<Sha256Digest>,
    conditional: Option<ConditionalResult>,
    negative: Option<Sha256Digest>,
    artifact: RustArtifactSummary,
) -> Result<Option<TerminalOutcome>, String> {
    let closed_count = usize::from(positive.is_some())
        + usize::from(conditional.is_some())
        + usize::from(negative.is_some());
    if closed_count > 1 {
        return Err("NotBothSidesClosed: mutually exclusive terminal sides coexist".into());
    }
    if let Some(proof_subject_sha256) = positive {
        Ok(Some(TerminalOutcome::Proved { proof_subject_sha256 }))
    } else if let Some(item) = conditional {
        Ok(Some(TerminalOutcome::ConditionalTheorem {
            conditional_target_id: item.conditional_target_id,
            condition_lean: item.condition_lean,
            sealed_statement_lean: item.sealed_statement_lean,
            sealed_statement_sha256: item.sealed_statement_sha256,
            approval_record_sha256: item.approval_record_sha256,
            proof_subject_sha256: item.proof_subject_sha256,
        }))
    } else {
        Ok(negative.map(|proof_subject_sha256| TerminalOutcome::Disproved {
            proof_subject_sha256,
            artifact,
        }))
    }
}

pub(crate) fn checked_local_closure_subject_sha256(
    record: &LocalClosureRecord,
) -> Result<Sha256Digest, String> {
    let value = serde_json::to_value(record).map_err(|error| error.to_string())?;
    let bytes = super::canonical_json_value(&value).map_err(|error| error.to_string())?;
    Ok(tagged_hash(DomainTag::RawArtifact, &bytes))
}

fn closed_side(
    state: &ProtocolState,
    target: &ChallengeTargetId,
) -> Result<Option<(Sha256Digest, NodeId, BTreeSet<String>)>, String> {
    let Some(spec) = state.configured_challenge_targets.get(target) else {
        return Ok(None);
    };
    let node = NodeId::from(spec.name.as_str());
    if state.live.open_nodes.contains(&node) {
        return Ok(None);
    }
    let Some(record) = state.local_closure_records.get(&node) else {
        return Ok(None);
    };
    if record.node != node || !record.is_fresh_for_completion(state) || record.is_sentinel_hashed() {
        return Ok(None);
    }
    Ok(Some((
        checked_local_closure_subject_sha256(record)?,
        node,
        record.kernel_axioms.clone(),
    )))
}

pub fn terminal_outcome(
    state: &ProtocolState,
    target: &ChallengeTargetId,
) -> Result<TerminalOutcome, String> {
    let spec = state.configured_challenge_targets.get(target).ok_or_else(|| {
        format!("terminal_outcome: `{}` is not configured", target.as_str())
    })?;
    if state.decide_primary_of_refutation(target).is_some() {
        return Err("terminal_outcome: classify the Decide primary, not its refutation twin".into());
    }
    let active_conditional = state
        .trust_base
        .conditional_candidates
        .get(target)
        .filter(|candidate| candidate.disposition.is_none());
    let conditional = if let Some(candidate) = active_conditional {
        if candidate.stage != crate::trust_base::ConditionalStage::Closed {
            return Err(format!(
                "terminal_outcome: `{}` has a pending conditional generation",
                target.as_str()
            ));
        }
        let sealed = candidate
            .sealed
            .as_ref()
            .ok_or("terminal conditional generation lost its seal")?;
        let ratification = candidate
            .ratification
            .as_ref()
            .ok_or("terminal conditional generation lost its ratification")?;
        let mut obligations = vec![&sealed.proof, &sealed.inhabited];
        obligations.extend(sealed.counterexample_excluded.as_ref());
        let records: Vec<&LocalClosureRecord> = obligations
            .into_iter()
            .map(|obligation| {
                state
                    .local_closure_records
                    .get(&obligation.node)
                    .filter(|record| {
                        !state.live.open_nodes.contains(&obligation.node)
                            && record.node == obligation.node
                            && record.is_fresh_for_completion(state)
                            && !record.is_sentinel_hashed()
                    })
                    .ok_or("terminal conditional obligation is no longer freshly closed")
            })
            .collect::<Result<_, _>>()?;
        let proof_subject_sha256 = checked_local_closure_subject_sha256(records[0])?;
        Some(ConditionalResult {
            conditional_target_id: sealed.proof.target_id.clone(),
            condition_lean: candidate.stamped.proposal.condition_lean.clone(),
            sealed_statement_lean: sealed.proof.statement_lean.clone(),
            sealed_statement_sha256: sealed.proof.statement_sha256,
            approval_record_sha256: ratification
                .approval_record_sha256
                .ok_or("terminal conditional ratification lacks an approval digest")?,
            proof_subject_sha256,
        })
    } else {
        None
    };
    // A live candidate suppresses both unconditional arms. Rejection or
    // withdrawal is an orthogonal terminal disposition and unblocks them.
    let positive = if active_conditional.is_some() {
        None
    } else {
        closed_side(state, target)?.map(|row| row.0)
    };
    let negative = if active_conditional.is_none() && spec.resolution == ChallengeResolution::Decide {
        closed_side(state, &crate::model::refutation_target_id(target))?.map(|row| row.0)
    } else {
        None
    };
    let artifact = state
        .trust_base
        .rust_witness_artifact_records
        .get(target)
        .cloned()
        .map(|record| RustArtifactSummary::Present { record })
        .unwrap_or(RustArtifactSummary::Absent);
    classify_success(positive, conditional, negative, artifact)?.ok_or_else(|| {
        format!("terminal_outcome: `{}` has no checked successful result", target.as_str())
    })
}

fn reviewable_authored_definitions(
    state: &ProtocolState,
) -> Result<Vec<ReviewableAuthoredDefinition>, String> {
    state.trust_base.seed_support_definitions.iter().map(|(node, definition)| {
        if definition.definition_utf8.as_ref().is_some_and(|body| raw_sha256(body.as_bytes()) != definition.raw_sha256) {
            return Err(format!("authored definition `{}` does not match its digest", node.as_str()));
        }
        Ok(ReviewableAuthoredDefinition {
            node: node.clone(),
            logical_id: definition.logical_id.clone(),
            evidence_relative_path: definition.evidence_relative_path.clone(),
            raw_sha256: definition.raw_sha256,
            definition_utf8: definition.definition_utf8.clone(),
        })
    }).collect()
}

pub fn claim_rows_from_state(state: &ProtocolState, context: ClaimContext) -> Result<ClaimRows, String> {
    if !state.trust_base.required() {
        return Err("claim rows exist only for required-v1 state".into());
    }
    let seed_roots = TrustRecordSeedRoots {
        seed_manifest_sha256: state.trust_base.seed_manifest_sha256.ok_or("claim rows require seed manifest root")?,
        seed_definition_bundle_sha256: state.trust_base.seed_definition_bundle_sha256.ok_or("claim rows require seed bundle root")?,
        evidence_tool_manifest_sha256: state.trust_base.evidence_tool_manifest_sha256.ok_or("claim rows require evidence manifest root")?,
        authored_semantic_root: state.trust_base.authored_semantic_root.ok_or("claim rows require authored root")?,
        approved_evidence_tool_input_root: state.trust_base.approved_evidence_tool_input_root.ok_or("claim rows require evidence root")?,
    };
    let launch_acknowledgment_sha256 = state.trust_base.launch_acknowledgment_sha256.ok_or("claim rows require launch acknowledgment")?;
    let adaptation_ledger_sha256 = raw_sha256(&canonical_json(&state.trust_base.adaptation_ledger).map_err(|error| error.to_string())?);
    let mut approved: BTreeSet<String> = CANONICAL_APPROVED_AXIOMS
        .iter()
        .map(|value| (*value).to_owned())
        .collect();
    approved.extend(
        state
            .trust_base
            .conditional_candidates
            .values()
            .filter(|candidate| {
                candidate.disposition.is_none()
                    && candidate.stage == crate::trust_base::ConditionalStage::Closed
            })
            .flat_map(|candidate| candidate.stamped.assumption_snapshots.iter())
            .filter(|snapshot| snapshot.status == "approved")
            .map(|snapshot| snapshot.axiom_name.clone()),
    );
    let header = ClaimHeader {
        edition: if context.edition_finalization { ClaimEdition::Finalization } else { ClaimEdition::PreGate },
        gate_episode_id: state.trust_base.advance_gate_episode_id.clone(),
        seed_roots,
        launch_acknowledgment_sha256,
        approval_record_sha256: context.approval_record_sha256,
        gate_presentation_sha256: context.gate_presentation_sha256,
        adaptation_ledger_sha256,
        phase0: state.trust_base.phase0.clone(),
        approved_axiom_floor: approved.iter().cloned().collect(),
        authored_definitions: reviewable_authored_definitions(state)?,
        cycle: state.cycle,
    };
    let mut rows = Vec::new();
    for (target, spec) in &state.configured_challenge_targets {
        let conditional_obligation = state
            .trust_base
            .conditional_candidates
            .values()
            .chain(state.trust_base.retired_conditional_candidates.iter())
            .filter_map(|candidate| candidate.sealed.as_ref())
            .any(|sealed| {
                sealed.proof.target_id == *target
                    || sealed.inhabited.target_id == *target
                    || sealed
                        .counterexample_excluded
                        .as_ref()
                        .is_some_and(|obligation| obligation.target_id == *target)
            });
        if state.decide_primary_of_refutation(target).is_some() || conditional_obligation {
            continue;
        }
        let outcome = terminal_outcome(state, target)?;
        let selected_polarity = match &outcome {
            TerminalOutcome::Proved { .. } => ChallengePolarity::Prove,
            TerminalOutcome::Disproved { .. } => ChallengePolarity::Disprove,
            TerminalOutcome::ConditionalTheorem { .. } => ChallengePolarity::Prove,
        };
        let selected_id = match &outcome {
            TerminalOutcome::Disproved { .. } => crate::model::refutation_target_id(target),
            TerminalOutcome::ConditionalTheorem {
                conditional_target_id,
                ..
            } => conditional_target_id.clone(),
            TerminalOutcome::Proved { .. } => target.clone(),
        };
        let (_, selected_node, kernel_axioms) = closed_side(state, &selected_id)?
            .ok_or_else(|| format!("selected result for {} has no fresh closure", target.as_str()))?;
        let unapproved_axioms = kernel_axioms.difference(&approved).cloned().collect();
        let (statement_name, statement_lean, statement_sha256) = match &outcome {
            TerminalOutcome::ConditionalTheorem {
                sealed_statement_lean,
                sealed_statement_sha256,
                ..
            } => (
                selected_node.as_str().to_owned(),
                sealed_statement_lean.clone(),
                *sealed_statement_sha256,
            ),
            _ => (spec.name.clone(), spec.lean.clone(), raw_sha256(spec.lean.as_bytes())),
        };
        rows.push(ClaimRow {
            target_id: target.as_str().to_owned(),
            informal: spec.informal.clone(),
            statement_name,
            statement_lean,
            statement_authority_sha256: crate::model::registered_statement_sha256(state, &selected_id).ok_or("registered statement authority is absent")?,
            statement_sha256,
            seeded_resolution: spec.resolution,
            selected_polarity,
            terminal_outcome: outcome,
            closure: ClosureSummary {
                selected_target_id: selected_id.as_str().to_owned(),
                selected_node,
                fresh: true,
                kernel_axioms,
                unapproved_axioms,
            },
        });
    }
    rows.sort_by(|left, right| left.target_id.as_bytes().cmp(right.target_id.as_bytes()));
    Ok(ClaimRows { schema: CLAIM_ROWS_SCHEMA.into(), header, rows })
}

pub fn polarity_ledger_rows(state: &ProtocolState) -> Vec<Value> {
    state.configured_challenge_targets.iter().filter(|(target, _)| state.decide_primary_of_refutation(target).is_none()).map(|(target, spec)| json!({
        "target_id": target,
        "resolution": spec.resolution,
        "live_polarity": state.live_polarity(target),
        "flip_count": state.pv_polarity_flip_count.get(target).copied().unwrap_or(0),
    })).collect()
}

pub fn refutation_dossier_rows(state: &ProtocolState) -> Vec<Value> {
    state.configured_challenge_targets.iter().filter(|(target, spec)| spec.resolution == ChallengeResolution::Decide && state.decide_primary_of_refutation(target).is_none()).map(|(target, _)| json!({
        "target_id": target,
        "refutation_target_id": crate::model::refutation_target_id(target),
        "terminal_outcome": terminal_outcome(state, target).ok(),
    })).collect()
}

pub fn claim_rows_bytes(rows: &ClaimRows) -> Result<Vec<u8>, String> {
    canonical_json(rows).map_err(|error| error.to_string())
}

pub fn parse_claim_rows(bytes: &[u8]) -> Result<ClaimRows, String> {
    let value = super::parse_json_strict(bytes).map_err(|error| error.to_string())?;
    if super::canonical_json_value(&value).map_err(|error| error.to_string())? != bytes {
        return Err("claim rows are not canonical JSON".into());
    }
    let rows: ClaimRows = serde_json::from_value(value).map_err(|error| error.to_string())?;
    if rows.schema != CLAIM_ROWS_SCHEMA || rows.rows.is_empty() {
        return Err("claim rows schema is wrong or has no rows".into());
    }
    Ok(rows)
}

pub fn render_claim_document(rows: &ClaimRows) -> Vec<u8> {
    let mut output = format!("# Trellis checked results\n\nEdition: {:?}\n\n", rows.header.edition);
    for row in &rows.rows {
        output.push_str(&format!("## {}\n\nResult: {}\n", row.target_id, row.terminal_outcome.rendered()));
        if let TerminalOutcome::ConditionalTheorem { condition_lean, .. } = &row.terminal_outcome {
            output.push_str(&format!(
                "Condition C (exact Lean JSON string): {}\nSealed conditional statement (exact Lean JSON string): {}\n",
                serde_json::to_string(condition_lean).expect("string serialization cannot fail"),
                serde_json::to_string(&row.statement_lean).expect("string serialization cannot fail"),
            ));
        }
        let proof_subject = match &row.terminal_outcome {
            TerminalOutcome::Proved { proof_subject_sha256 }
            | TerminalOutcome::Disproved { proof_subject_sha256, .. }
            | TerminalOutcome::ConditionalTheorem { proof_subject_sha256, .. } => proof_subject_sha256,
        };
        output.push_str(&format!(
            "Proof subject: {proof_subject}\nSelected Lean node: {}\n\n",
            row.closure.selected_node.as_str()
        ));
    }
    output.into_bytes()
}

pub fn lint_claim_rows(rows: &ClaimRows, rendered: &[u8]) -> Result<(), String> {
    if rows.rows.is_empty() || rendered != render_claim_document(rows) {
        return Err("claim document is not the deterministic rendering of nonempty rows".into());
    }
    if rows.rows.iter().any(|row| !row.closure.fresh || !row.closure.unapproved_axioms.is_empty()) {
        return Err("claim row lacks fresh approved-axiom closure".into());
    }
    forbidden_phrase_reason(rendered).map_or(Ok(()), Err)
}

pub fn forbidden_phrase_reason(rendered: &[u8]) -> Option<String> {
    let lower = String::from_utf8_lossy(rendered).to_ascii_lowercase();
    ["verified in practice", "source confirmed", "machine-confirmed counterexample"]
        .into_iter()
        .find(|phrase| lower.contains(phrase))
        .map(|phrase| format!("claim document contains forbidden phrase {phrase:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> Sha256Digest {
        Sha256Digest::from_bytes([byte; 32])
    }

    #[test]
    fn exclusive_classifier_rejects_coexisting_closed_sides() {
        let result = classify_success(
            Some(digest(1)),
            Some(ConditionalResult { conditional_target_id: ChallengeTargetId::from("conditional:goal"), condition_lean: "x > 0".into(), sealed_statement_lean: "theorem c : True := by".into(), sealed_statement_sha256: digest(3), approval_record_sha256: digest(4), proof_subject_sha256: digest(5) }),
            Some(digest(6)),
            RustArtifactSummary::Absent,
        );
        assert!(result.unwrap_err().contains("NotBothSidesClosed"));
        assert!(matches!(classify_success(None, None, Some(digest(6)), RustArtifactSummary::Absent).unwrap(), Some(TerminalOutcome::Disproved { artifact: RustArtifactSummary::Absent, .. })));
        assert_eq!(classify_success(None, None, None, RustArtifactSummary::Absent).unwrap(), None);
    }

    fn artifact_with_correspondence(
        correspondence: super::super::artifact::RustWitnessCorrespondence,
    ) -> RustArtifactSummary {
        RustArtifactSummary::Present {
            record: RustWitnessArtifactRecord {
                schema: super::super::artifact::RUST_WITNESS_ARTIFACT_SCHEMA.into(),
                target_id: ChallengeTargetId::from("goal:generic"),
                relative_path: format!(
                    "reference/rust-witnesses/target-{}/witness.rs",
                    raw_sha256(b"goal:generic")
                ),
                artifact_sha256: digest(7),
                pinned_crate_tree_sha256: digest(8),
                execution: super::super::artifact::RustWitnessExecution::Receipt {
                    receipt_sha256: digest(9),
                    runner_sha256: digest(10),
                    timed_out: false,
                    exit_code: Some(101),
                },
                correspondence,
                freeze_episode_id: "advance:generic".into(),
                gate_episode_id: "advance:generic".into(),
            },
        }
    }

    #[test]
    fn failed_execution_and_pass_or_fail_correspondence_never_authorize_a_result() {
        let pass = artifact_with_correspondence(
            super::super::artifact::RustWitnessCorrespondence::Pass {
                request_sha256: digest(11),
                verdict_sha256: digest(12),
                reason: "lane pass".into(),
            },
        );
        let fail = artifact_with_correspondence(
            super::super::artifact::RustWitnessCorrespondence::Fail {
                request_sha256: digest(13),
                verdict_sha256: digest(14),
                reason: "lane fail".into(),
            },
        );
        assert_eq!(classify_success(None, None, None, pass.clone()), Ok(None));
        assert_eq!(classify_success(None, None, None, fail.clone()), Ok(None));
        for artifact in [pass, fail] {
            let result = classify_success(None, None, Some(digest(6)), artifact).unwrap();
            assert!(matches!(
                result,
                Some(TerminalOutcome::Disproved {
                    proof_subject_sha256,
                    ..
                }) if proof_subject_sha256 == digest(6)
            ));
        }
    }
}

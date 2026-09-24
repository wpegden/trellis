//! Generic, target-bound conditional theorem protocol.
//!
//! This module owns the only transformation from an already registered
//! theorem `T` and a proposed Lean proposition `C` to proof obligations. It
//! operates on registered bytes: callers cannot provide binders, declaration
//! names, target suffixes, or sealed bytes.

use super::{canonical_json, raw_sha256, tagged_hash, DomainTag, Sha256Digest};
use crate::model::{
    parse_decide_statement_parts, registered_statement_text_sha256, ChallengeResolution,
    ChallengeTargetId, NodeId, ProtocolState,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const CONDITIONAL_THEOREM_REACHABLE: bool = true;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionalTriggerClassification {
    ModelCounterexampleMismatch,
    AssumptionsModelGap,
    #[default]
    UnconditionalNotEstablished,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalEvidenceReferences {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disproof_sha256: Option<Sha256Digest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<Sha256Digest>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub assumption_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalTheoremProposal {
    pub target_id: ChallengeTargetId,
    /// A complete Lean `Prop` in the registered target's binder context.
    pub condition_lean: String,
    pub condition_informal: String,
    pub rationale: String,
    pub trigger: ConditionalTriggerClassification,
    pub evidence: ConditionalEvidenceReferences,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_under_model_assumption_id: Option<String>,
    /// Lean terms, in registered-binder order, for a concrete counterexample.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concrete_counterexample_arguments: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalAssumptionSnapshot {
    pub id: String,
    pub axiom_name: String,
    pub status: String,
    /// Complete canonical TCB disclosure presented to the dedicated
    /// correspondence verifier.
    pub record: serde_json::Value,
    pub record_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StampedConditionalTheoremProposal {
    pub generation: u32,
    pub proposal: ConditionalTheoremProposal,
    pub target_statement_sha256: Sha256Digest,
    pub condition_sha256: Sha256Digest,
    pub evidence_set_sha256: Sha256Digest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterexample_arguments_sha256: Option<Sha256Digest>,
    pub proposal_sha256: Sha256Digest,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assumption_snapshots: Vec<ConditionalAssumptionSnapshot>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalCorrespondenceRequest {
    pub proposal: StampedConditionalTheoremProposal,
    pub target_lean: String,
    pub relevant_rust_and_model_sources: serde_json::Value,
    pub artifact_dossier: serde_json::Value,
    pub request_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalCorrespondenceVerdict {
    pub request_sha256: Sha256Digest,
    pub proposal_sha256: Sha256Digest,
    pub boundary_expression_correct: bool,
    pub condition_relevant: bool,
    pub realizable_non_vacuous: bool,
    pub obligation_preserved_on_domain: bool,
    pub rust_axioms_backed_by_approved_assumptions: bool,
    pub findings: String,
}

impl ConditionalCorrespondenceVerdict {
    pub fn passed(&self) -> bool {
        self.boundary_expression_correct
            && self.condition_relevant
            && self.realizable_non_vacuous
            && self.obligation_preserved_on_domain
            && self.rust_axioms_backed_by_approved_assumptions
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalCorrespondenceApproval {
    pub request_sha256: Sha256Digest,
    pub response_sha256: Sha256Digest,
    pub proposal_sha256: Sha256Digest,
    pub target_statement_sha256: Sha256Digest,
    pub condition_sha256: Sha256Digest,
    pub evidence_set_sha256: Sha256Digest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterexample_arguments_sha256: Option<Sha256Digest>,
    pub verdict: ConditionalCorrespondenceVerdict,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionalObligationKind {
    #[default]
    ConditionalProof,
    ConditionInhabited,
    CounterexampleExcluded,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalObligation {
    pub kind: ConditionalObligationKind,
    pub target_id: ChallengeTargetId,
    pub node: NodeId,
    pub theorem_name: String,
    pub statement_lean: String,
    pub statement_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SealedConditionalTheorem {
    pub proposal_sha256: Sha256Digest,
    pub target_statement_sha256: Sha256Digest,
    pub condition_sha256: Sha256Digest,
    pub evidence_set_sha256: Sha256Digest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterexample_arguments_sha256: Option<Sha256Digest>,
    pub correspondence_sha256: Sha256Digest,
    pub proof: ConditionalObligation,
    pub inhabited: ConditionalObligation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterexample_excluded: Option<ConditionalObligation>,
    pub seal_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalRatification {
    pub generation: u32,
    pub target_id: ChallengeTargetId,
    pub proposal_sha256: Sha256Digest,
    pub target_statement_sha256: Sha256Digest,
    pub condition_sha256: Sha256Digest,
    pub evidence_set_sha256: Sha256Digest,
    pub correspondence_sha256: Sha256Digest,
    pub seal_sha256: Sha256Digest,
    pub gate_episode_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_record_sha256: Option<Sha256Digest>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionalStage {
    #[default]
    None,
    Proposed,
    CorrespondencePass,
    HumanApproved,
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionalDisposition {
    Rejected,
    Withdrawn,
    Superseded,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalCandidateGeneration {
    pub stamped: StampedConditionalTheoremProposal,
    pub stage: ConditionalStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<ConditionalDisposition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correspondence_request: Option<ConditionalCorrespondenceRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correspondence: Option<ConditionalCorrespondenceApproval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sealed: Option<SealedConditionalTheorem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratification_gate_episode_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratification: Option<ConditionalRatification>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionalActivationPayload {
    pub target_id: ChallengeTargetId,
    pub generation: u32,
    pub proposal_sha256: Sha256Digest,
    pub target_statement_sha256: Sha256Digest,
    pub condition_sha256: Sha256Digest,
    pub evidence_set_sha256: Sha256Digest,
    pub correspondence_sha256: Sha256Digest,
    pub seal_sha256: Sha256Digest,
    pub approval_record_sha256: Sha256Digest,
    pub obligations: Vec<ConditionalObligation>,
}

fn digest<T: Serialize>(value: &T) -> Result<Sha256Digest, String> {
    Ok(tagged_hash(
        DomainTag::ConditionalizationSchema,
        &canonical_json(value).map_err(|error| error.to_string())?,
    ))
}

fn syntactically_trivial_condition(text: &str) -> bool {
    let mut text = text.trim();
    while text.starts_with('(') && text.ends_with(')') {
        let inner = &text[1..text.len() - 1];
        if !valid_balanced_delimiters(inner) {
            break;
        }
        text = inner.trim();
    }
    if matches!(text, "True" | "False" | "Bool.true" | "Bool.false") {
        return true;
    }
    [" ↔ ", " = "].into_iter().any(|operator| {
        text.split_once(operator)
            .is_some_and(|(left, right)| !left.trim().is_empty() && left.trim() == right.trim())
    })
}

fn valid_balanced_delimiters(text: &str) -> bool {
    let mut stack = Vec::new();
    for c in text.chars() {
        match c {
            '(' => stack.push(')'),
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '⦃' => stack.push('⦄'),
            ')' | '}' | ']' | '⦄' if stack.pop() != Some(c) => return false,
            _ => {}
        }
    }
    stack.is_empty()
}

fn valid_lean_term(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty()
        || text.contains(":=")
        || text.contains("--")
        || text.contains("/-")
        || syntactically_trivial_condition(text)
    {
        return false;
    }
    valid_balanced_delimiters(text)
}

fn proposal_is_canonical(proposal: &ConditionalTheoremProposal) -> bool {
    proposal.condition_lean == proposal.condition_lean.trim()
        && proposal.condition_informal == proposal.condition_informal.trim()
        && proposal.rationale == proposal.rationale.trim()
        && proposal
            .existing_under_model_assumption_id
            .as_ref()
            .is_none_or(|id| id == id.trim())
        && proposal
            .concrete_counterexample_arguments
            .as_ref()
            .is_none_or(|arguments| arguments.iter().all(|term| term == term.trim()))
        && proposal
            .evidence
            .assumption_ids
            .iter()
            .all(|id| id == id.trim())
}

pub fn stamp_conditional_proposal(
    state: &ProtocolState,
    mut proposal: ConditionalTheoremProposal,
    generation: u32,
    mut assumption_snapshots: Vec<ConditionalAssumptionSnapshot>,
) -> Result<StampedConditionalTheoremProposal, String> {
    proposal.condition_lean = proposal.condition_lean.trim().to_owned();
    proposal.condition_informal = proposal.condition_informal.trim().to_owned();
    proposal.rationale = proposal.rationale.trim().to_owned();
    if let Some(id) = proposal.existing_under_model_assumption_id.as_mut() {
        *id = id.trim().to_owned();
    }
    if let Some(arguments) = proposal.concrete_counterexample_arguments.as_mut() {
        for term in arguments {
            *term = term.trim().to_owned();
        }
    }
    let normalized_assumption_ids: BTreeSet<String> = proposal
        .evidence
        .assumption_ids
        .iter()
        .map(|id| id.trim().to_owned())
        .collect();
    if normalized_assumption_ids.len() != proposal.evidence.assumption_ids.len() {
        return Err("conditional proposal contains duplicate canonical assumption ids".into());
    }
    proposal.evidence.assumption_ids = normalized_assumption_ids;
    assumption_snapshots.sort_by(|left, right| left.id.as_bytes().cmp(right.id.as_bytes()));
    let spec = state
        .configured_challenge_targets
        .get(&proposal.target_id)
        .ok_or("conditional proposal names an unknown target")?;
    if spec.resolution != ChallengeResolution::Decide
        || state.decide_primary_of_refutation(&proposal.target_id).is_some()
    {
        return Err("conditional proposal requires a Decide primary target".into());
    }
    if !valid_lean_term(&proposal.condition_lean)
        || proposal.condition_informal.trim().is_empty()
        || proposal.rationale.trim().is_empty()
    {
        return Err("conditional proposal has an invalid or undocumented condition".into());
    }
    if proposal.evidence.assumption_ids.iter().any(|id| id.trim().is_empty()) {
        return Err("conditional proposal contains an empty assumption id".into());
    }
    if let Some(id) = proposal.existing_under_model_assumption_id.as_deref() {
        if !proposal.evidence.assumption_ids.contains(id) {
            return Err("under-model assumption must be included in the evidence set".into());
        }
    }
    let snapshot_ids: BTreeSet<&str> = assumption_snapshots
        .iter()
        .map(|row| row.id.as_str())
        .collect();
    if snapshot_ids.len() != assumption_snapshots.len()
        || snapshot_ids
            != proposal
                .evidence
                .assumption_ids
                .iter()
                .map(String::as_str)
                .collect()
        || assumption_snapshots.iter().any(|row| {
            row.axiom_name.trim().is_empty()
                || row.status != "approved"
                || row.record.is_null()
                || row.record_sha256 == Sha256Digest::ZERO
                || canonical_json(&row.record)
                    .map(|bytes| raw_sha256(&bytes) != row.record_sha256)
                    .unwrap_or(true)
        })
    {
        return Err(
            "conditional proposal assumption evidence must exactly name frozen approved records"
                .into(),
        );
    }
    if let Some(artifact_sha256) = proposal.evidence.artifact_sha256 {
        if state
            .trust_base
            .rust_witness_artifact_records
            .get(&proposal.target_id)
            .is_none_or(|record| record.artifact_sha256 != artifact_sha256)
        {
            return Err("conditional proposal cites a stale artifact digest".into());
        }
    }
    if let Some(disproof_sha256) = proposal.evidence.disproof_sha256 {
        let twin = crate::model::refutation_target_id(&proposal.target_id);
        if registered_statement_text_sha256(state, &twin) != Some(disproof_sha256) {
            return Err("conditional proposal cites a stale disproof statement digest".into());
        }
    }
    let evidence_cites_counterexample = proposal.evidence.disproof_sha256.is_some()
        || proposal.evidence.artifact_sha256.is_some();
    if evidence_cites_counterexample
        && proposal
            .concrete_counterexample_arguments
            .as_ref()
            .is_none_or(Vec::is_empty)
    {
        return Err(
            "conditional proposal citing disproof/artifact evidence requires counterexample arguments"
                .into(),
        );
    }
    let target_statement_sha256 = registered_statement_text_sha256(state, &proposal.target_id)
        .ok_or("conditional proposal target has no registered statement")?;
    let condition_sha256 = raw_sha256(proposal.condition_lean.as_bytes());
    let evidence_set_sha256 = digest(&(&proposal.evidence, &assumption_snapshots))?;
    let counterexample_arguments_sha256 = proposal
        .concrete_counterexample_arguments
        .as_ref()
        .map(|arguments| digest(&(arguments, &proposal.evidence)))
        .transpose()?;
    let proposal_sha256 = digest(&(
        generation,
        &proposal,
        target_statement_sha256,
        condition_sha256,
        evidence_set_sha256,
        counterexample_arguments_sha256,
    ))?;
    Ok(StampedConditionalTheoremProposal {
        generation,
        proposal,
        target_statement_sha256,
        condition_sha256,
        evidence_set_sha256,
        counterexample_arguments_sha256,
        proposal_sha256,
        assumption_snapshots,
    })
}

pub fn build_conditional_correspondence_request(
    state: &ProtocolState,
    stamped: &StampedConditionalTheoremProposal,
    relevant_rust_and_model_sources: serde_json::Value,
    artifact_dossier: serde_json::Value,
) -> Result<ConditionalCorrespondenceRequest, String> {
    let spec = state
        .configured_challenge_targets
        .get(&stamped.proposal.target_id)
        .ok_or("conditional correspondence target is no longer registered")?;
    if raw_sha256(spec.lean.as_bytes()) != stamped.target_statement_sha256 {
        return Err("conditional correspondence target statement is stale".into());
    }
    let mut request = ConditionalCorrespondenceRequest {
        proposal: stamped.clone(),
        target_lean: spec.lean.clone(),
        relevant_rust_and_model_sources,
        artifact_dossier,
        request_sha256: Sha256Digest::ZERO,
    };
    request.request_sha256 = digest(&(
        &request.proposal,
        &request.target_lean,
        &request.relevant_rust_and_model_sources,
        &request.artifact_dossier,
    ))?;
    Ok(request)
}

#[derive(Clone, Debug)]
struct Binder {
    rendered: String,
    names: Vec<String>,
    ty: String,
    instance: bool,
}

fn matching_close(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '{' => Some('}'),
        '[' => Some(']'),
        '⦃' => Some('⦄'),
        _ => None,
    }
}

fn top_level_colon(text: &str) -> Option<usize> {
    let mut stack = Vec::new();
    for (idx, c) in text.char_indices() {
        if let Some(close) = matching_close(c) {
            stack.push(close);
        } else if matches!(c, ')' | '}' | ']' | '⦄') {
            if stack.pop() != Some(c) {
                return None;
            }
        } else if c == ':' && stack.is_empty() {
            return Some(idx);
        }
    }
    None
}

fn parse_binders(region: &str) -> Result<Vec<Binder>, String> {
    let chars: Vec<(usize, char)> = region.char_indices().collect();
    let mut out = Vec::new();
    let mut at = 0usize;
    let mut anonymous = 0usize;
    while at < chars.len() {
        while at < chars.len() && chars[at].1.is_whitespace() {
            at += 1;
        }
        if at == chars.len() {
            break;
        }
        let open = chars[at].1;
        let close = matching_close(open).ok_or("unsupported unbracketed theorem binder")?;
        let ascii_strict = open == '{' && chars.get(at + 1).is_some_and(|(_, c)| *c == '{');
        at += if ascii_strict { 2 } else { 1 };
        let inner_start = if at < chars.len() { chars[at].0 } else { region.len() };
        let mut stack = if ascii_strict {
            vec!['}', '}']
        } else {
            vec![close]
        };
        let mut end_byte = None;
        while at < chars.len() {
            let c = chars[at].1;
            if let Some(nested_close) = matching_close(c) {
                stack.push(nested_close);
            } else if matches!(c, ')' | '}' | ']' | '⦄') {
                if stack.pop() != Some(c) {
                    return Err("mismatched theorem binder brackets".into());
                }
                if stack.is_empty() {
                    end_byte = Some(if ascii_strict {
                        chars
                            .get(at.checked_sub(1).ok_or("malformed strict implicit binder")?)
                            .map(|(idx, _)| *idx)
                            .ok_or("malformed strict implicit binder")?
                    } else {
                        chars[at].0
                    });
                    at += 1;
                    break;
                }
            }
            at += 1;
        }
        let end_byte = end_byte.ok_or("unterminated theorem binder")?;
        let inner = region[inner_start..end_byte].trim();
        let colon = top_level_colon(inner).ok_or("theorem binder has no top-level type annotation")?;
        let names_text = inner[..colon].trim();
        let ty = inner[colon + 1..].trim().to_owned();
        if ty.is_empty() {
            return Err("theorem binder has an empty type".into());
        }
        let mut names: Vec<String> = names_text
            .split_whitespace()
            .filter(|name| *name != "_")
            .map(str::to_owned)
            .collect();
        if names.is_empty() {
            anonymous += 1;
            names.push(format!("_conditionalBinder{anonymous}"));
        }
        if names.iter().any(|name| {
            name.is_empty()
                || !name.chars().all(|c| c == '_' || c == '\'' || c.is_alphanumeric())
        }) {
            return Err("unsupported theorem binder name".into());
        }
        let rendered = if ascii_strict {
            format!("{{{{{} : {ty}}}}}", names.join(" "))
        } else {
            format!("{open}{} : {ty}{close}", names.join(" "))
        };
        out.push(Binder {
            rendered,
            names,
            ty,
            instance: open == '[',
        });
    }
    Ok(out)
}

fn lift_forall(mut proposition: String, mut binders: String) -> Result<(String, String), String> {
    loop {
        let trimmed = proposition.trim_start();
        let after = if let Some(rest) = trimmed.strip_prefix('∀') {
            Some(rest)
        } else if let Some(rest) = trimmed.strip_prefix("forall") {
            Some(rest)
        } else {
            None
        };
        let Some(after) = after else {
            return Ok((binders, proposition));
        };
        let mut stack = Vec::new();
        let mut comma = None;
        for (idx, c) in after.char_indices() {
            if let Some(close) = matching_close(c) {
                stack.push(close);
            } else if matches!(c, ')' | '}' | ']' | '⦄') {
                if stack.pop() != Some(c) {
                    return Err("malformed quantified theorem binders".into());
                }
            } else if c == ',' && stack.is_empty() {
                comma = Some(idx);
                break;
            }
        }
        let comma = comma.ok_or("quantified theorem has no binder/body comma")?;
        let lifted = after[..comma].trim();
        if lifted.is_empty() {
            return Err("quantified theorem has an empty binder group".into());
        }
        if !binders.trim().is_empty() {
            binders.push(' ');
        }
        binders.push_str(lifted);
        proposition = after[comma + 1..].trim().to_owned();
    }
}

fn inhabited_prop(binders: &[Binder], condition: &str) -> String {
    let mut tail = format!("({})", condition.trim());
    for binder in binders.iter().rev() {
        for name in binder.names.iter().rev() {
            if binder.instance {
                tail = format!("∃ ({name} : {}), letI := {name}; {tail}", binder.ty);
            } else {
                tail = format!("∃ ({name} : {}), {tail}", binder.ty);
            }
        }
    }
    tail
}

fn obligation(
    kind: ConditionalObligationKind,
    base_id: &str,
    theorem_name: String,
    statement_lean: String,
) -> ConditionalObligation {
    let suffix = match kind {
        ConditionalObligationKind::ConditionalProof => "proof",
        ConditionalObligationKind::ConditionInhabited => "inhabited",
        ConditionalObligationKind::CounterexampleExcluded => "counterexample_excluded",
    };
    ConditionalObligation {
        kind,
        target_id: ChallengeTargetId::from(format!("{base_id}__{suffix}")),
        node: NodeId::from(theorem_name.as_str()),
        theorem_name,
        statement_sha256: raw_sha256(statement_lean.as_bytes()),
        statement_lean,
    }
}

pub fn seal_conditional_theorem(
    state: &ProtocolState,
    stamped: &StampedConditionalTheoremProposal,
    request: &ConditionalCorrespondenceRequest,
    verdict: &ConditionalCorrespondenceVerdict,
) -> Result<(ConditionalCorrespondenceApproval, SealedConditionalTheorem), String> {
    let spec = state
        .configured_challenge_targets
        .get(&stamped.proposal.target_id)
        .ok_or("conditional target is no longer registered")?;
    if raw_sha256(spec.lean.as_bytes()) != stamped.target_statement_sha256
        || request.proposal != *stamped
        || request.target_lean != spec.lean
        || request.request_sha256
            != digest(&(
                &request.proposal,
                &request.target_lean,
                &request.relevant_rust_and_model_sources,
                &request.artifact_dossier,
            ))?
    {
        return Err("conditional correspondence request or target is stale".into());
    }
    if verdict.request_sha256 != request.request_sha256
        || verdict.proposal_sha256 != stamped.proposal_sha256
        || !verdict.passed()
        || verdict.findings.trim().is_empty()
    {
        return Err("conditional correspondence did not pass its exact bound request".into());
    }
    let response_sha256 = digest(verdict)?;
    let approval = ConditionalCorrespondenceApproval {
        request_sha256: request.request_sha256,
        response_sha256,
        proposal_sha256: stamped.proposal_sha256,
        target_statement_sha256: stamped.target_statement_sha256,
        condition_sha256: stamped.condition_sha256,
        evidence_set_sha256: stamped.evidence_set_sha256,
        counterexample_arguments_sha256: stamped.counterexample_arguments_sha256,
        verdict: verdict.clone(),
    };
    let correspondence_sha256 = digest(&approval)?;
    let (binders, proposition) = parse_decide_statement_parts(&spec.lean)?;
    let (binders, proposition) = lift_forall(proposition, binders)?;
    let parsed_binders = parse_binders(&binders)?;
    let binder_text = parsed_binders
        .iter()
        .map(|binder| binder.rendered.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let arg_count: usize = parsed_binders.iter().map(|binder| binder.names.len()).sum();
    let counterexample_arguments = stamped
        .proposal
        .concrete_counterexample_arguments
        .as_deref();
    if counterexample_arguments.is_some_and(|arguments| arguments.len() != arg_count) {
        return Err("counterexample arguments do not match the registered binder arity".into());
    }
    if counterexample_arguments
        .into_iter()
        .flatten()
        .any(|term| !valid_lean_term(term))
    {
        return Err("counterexample contains an invalid Lean argument term".into());
    }
    let short = &stamped.proposal_sha256.to_hex()[..16];
    let base_name = format!("TrellisConditional_{short}");
    let base_id = format!("{}__conditional_g{}", stamped.proposal.target_id.as_str(), stamped.generation);
    let separator = if binder_text.is_empty() { "" } else { " " };
    let proof_statement = format!(
        "theorem {base_name}{separator}{binder_text} : ({}) → ({}) := by",
        stamped.proposal.condition_lean.trim(),
        proposition.trim()
    );
    let proof = obligation(
        ConditionalObligationKind::ConditionalProof,
        &base_id,
        base_name.clone(),
        proof_statement,
    );
    let inhabited_name = format!("{base_name}__ConditionInhabited");
    let inhabited_statement = format!(
        "theorem {inhabited_name} : {} := by",
        inhabited_prop(&parsed_binders, &stamped.proposal.condition_lean)
    );
    let inhabited = obligation(
        ConditionalObligationKind::ConditionInhabited,
        &base_id,
        inhabited_name,
        inhabited_statement,
    );
    let counterexample_excluded = if let Some(arguments) = counterexample_arguments {
        let name = format!("{base_name}__CounterexampleExcluded");
        let predicate = if binder_text.is_empty() {
            format!("({})", stamped.proposal.condition_lean.trim())
        } else {
            format!(
                "(let predicate := fun {binder_text} => ({}); @predicate {})",
                stamped.proposal.condition_lean.trim(),
                arguments.join(" ")
            )
        };
        let statement = format!("theorem {name} : ¬ ({predicate}) := by");
        Some(obligation(
            ConditionalObligationKind::CounterexampleExcluded,
            &base_id,
            name,
            statement,
        ))
    } else {
        None
    };
    let mut sealed = SealedConditionalTheorem {
        proposal_sha256: stamped.proposal_sha256,
        target_statement_sha256: stamped.target_statement_sha256,
        condition_sha256: stamped.condition_sha256,
        evidence_set_sha256: stamped.evidence_set_sha256,
        counterexample_arguments_sha256: stamped.counterexample_arguments_sha256,
        correspondence_sha256,
        proof,
        inhabited,
        counterexample_excluded,
        seal_sha256: Sha256Digest::ZERO,
    };
    sealed.seal_sha256 = digest(&(
        sealed.proposal_sha256,
        sealed.target_statement_sha256,
        sealed.condition_sha256,
        sealed.evidence_set_sha256,
        sealed.counterexample_arguments_sha256,
        sealed.correspondence_sha256,
        &sealed.proof,
        &sealed.inhabited,
        &sealed.counterexample_excluded,
    ))?;
    Ok((approval, sealed))
}

pub fn conditional_activation_payload(
    candidate: &ConditionalCandidateGeneration,
) -> Result<ConditionalActivationPayload, String> {
    if candidate.stage != ConditionalStage::HumanApproved || candidate.disposition.is_some() {
        return Err("conditional candidate is not ratified and dormant".into());
    }
    let sealed = candidate.sealed.as_ref().ok_or("conditional candidate has no seal")?;
    let ratification = candidate.ratification.as_ref().ok_or("conditional candidate has no ratification")?;
    if ratification.proposal_sha256 != sealed.proposal_sha256
        || ratification.target_statement_sha256 != sealed.target_statement_sha256
        || ratification.condition_sha256 != sealed.condition_sha256
        || ratification.evidence_set_sha256 != sealed.evidence_set_sha256
        || ratification.correspondence_sha256 != sealed.correspondence_sha256
        || ratification.seal_sha256 != sealed.seal_sha256
        || ratification.approval_record_sha256.is_none()
    {
        return Err("conditional ratification does not bind the exact sealed packet".into());
    }
    let mut obligations = vec![sealed.proof.clone(), sealed.inhabited.clone()];
    obligations.extend(sealed.counterexample_excluded.clone());
    Ok(ConditionalActivationPayload {
        target_id: candidate.stamped.proposal.target_id.clone(),
        generation: candidate.stamped.generation,
        proposal_sha256: sealed.proposal_sha256,
        target_statement_sha256: sealed.target_statement_sha256,
        condition_sha256: sealed.condition_sha256,
        evidence_set_sha256: sealed.evidence_set_sha256,
        correspondence_sha256: sealed.correspondence_sha256,
        seal_sha256: sealed.seal_sha256,
        approval_record_sha256: ratification
            .approval_record_sha256
            .ok_or("conditional ratification lacks its approval record")?,
        obligations,
    })
}

pub fn validate_conditional_protocol_state(state: &ProtocolState) -> Result<(), String> {
    for retired in &state.trust_base.retired_conditional_candidates {
        if retired.disposition.is_none() {
            return Err("retired conditional generation has no irreversible disposition".into());
        }
    }
    for (target, candidate) in &state.trust_base.conditional_candidates {
        if target != &candidate.stamped.proposal.target_id || candidate.stamped.generation == 0 {
            return Err("conditional candidate key/generation is malformed".into());
        }
        let spec = state
            .configured_challenge_targets
            .get(target)
            .ok_or("conditional candidate parent target is absent")?;
        if raw_sha256(spec.lean.as_bytes()) != candidate.stamped.target_statement_sha256
            || !proposal_is_canonical(&candidate.stamped.proposal)
            || raw_sha256(candidate.stamped.proposal.condition_lean.as_bytes())
                != candidate.stamped.condition_sha256
            || candidate
                .stamped
                .assumption_snapshots
                .iter()
                .map(|snapshot| snapshot.id.as_str())
                .collect::<BTreeSet<_>>()
                != candidate
                    .stamped
                    .proposal
                    .evidence
                    .assumption_ids
                    .iter()
                    .map(String::as_str)
                    .collect()
            || candidate.stamped.assumption_snapshots.iter().any(|snapshot| {
                snapshot.id.trim().is_empty()
                    || snapshot.axiom_name.trim().is_empty()
                    || snapshot.status != "approved"
                    || snapshot.record.is_null()
                    || snapshot.record_sha256 == Sha256Digest::ZERO
                    || canonical_json(&snapshot.record)
                        .map(|bytes| raw_sha256(&bytes) != snapshot.record_sha256)
                        .unwrap_or(true)
            })
            || !candidate
                .stamped
                .assumption_snapshots
                .windows(2)
                .all(|pair| pair[0].id.as_bytes() < pair[1].id.as_bytes())
            || digest(&(
                &candidate.stamped.proposal.evidence,
                &candidate.stamped.assumption_snapshots,
            ))? != candidate.stamped.evidence_set_sha256
            || digest(&(
                candidate.stamped.generation,
                &candidate.stamped.proposal,
                candidate.stamped.target_statement_sha256,
                candidate.stamped.condition_sha256,
                candidate.stamped.evidence_set_sha256,
                candidate.stamped.counterexample_arguments_sha256,
            ))? != candidate.stamped.proposal_sha256
            || candidate.stamped.counterexample_arguments_sha256
                != candidate.stamped.proposal.concrete_counterexample_arguments.as_ref()
                    .map(|arguments| digest(&(arguments, &candidate.stamped.proposal.evidence)))
                    .transpose()?
        {
            return Err("conditional target/proposal/evidence identity is stale".into());
        }
        if candidate.disposition == Some(ConditionalDisposition::Superseded) {
            return Err("a current conditional generation cannot be superseded".into());
        }
        if candidate.stage == ConditionalStage::None {
            return Err("a stored conditional generation cannot be at the absent stage".into());
        }
        let request_required = candidate.stage >= ConditionalStage::Proposed;
        if request_required {
            let request = candidate
                .correspondence_request
                .as_ref()
                .ok_or("proposed conditional generation lacks its correspondence request")?;
            if request.proposal != candidate.stamped
                || request.target_lean != spec.lean
                || request.request_sha256
                    != digest(&(
                        &request.proposal,
                        &request.target_lean,
                        &request.relevant_rust_and_model_sources,
                        &request.artifact_dossier,
                    ))?
            {
                return Err("conditional correspondence request identity is stale".into());
            }
        }
        if candidate.stage >= ConditionalStage::CorrespondencePass {
            let approval = candidate
                .correspondence
                .as_ref()
                .ok_or("conditional correspondence-pass stage lacks approval")?;
            let sealed = candidate.sealed.as_ref().ok_or("conditional stage lacks seal")?;
            let request = candidate
                .correspondence_request
                .as_ref()
                .ok_or("conditional approval lost its request")?;
            if approval.request_sha256 != request.request_sha256
                || approval.response_sha256 != digest(&approval.verdict)?
                || approval.verdict.request_sha256 != request.request_sha256
                || approval.verdict.proposal_sha256 != candidate.stamped.proposal_sha256
                || !approval.verdict.passed()
                || approval.verdict.findings.trim().is_empty()
                || approval.proposal_sha256 != candidate.stamped.proposal_sha256
                || approval.target_statement_sha256 != candidate.stamped.target_statement_sha256
                || approval.condition_sha256 != candidate.stamped.condition_sha256
                || approval.evidence_set_sha256 != candidate.stamped.evidence_set_sha256
                || approval.counterexample_arguments_sha256
                    != candidate.stamped.counterexample_arguments_sha256
                || sealed.proposal_sha256 != approval.proposal_sha256
                || sealed.target_statement_sha256 != approval.target_statement_sha256
                || sealed.condition_sha256 != approval.condition_sha256
                || sealed.evidence_set_sha256 != approval.evidence_set_sha256
                || sealed.counterexample_arguments_sha256
                    != approval.counterexample_arguments_sha256
                || ((candidate.stamped.proposal.evidence.disproof_sha256.is_some()
                    || candidate.stamped.proposal.evidence.artifact_sha256.is_some())
                    && sealed.counterexample_excluded.is_none())
                || sealed.correspondence_sha256 != digest(approval)?
                || sealed.seal_sha256
                    != digest(&(
                        sealed.proposal_sha256,
                        sealed.target_statement_sha256,
                        sealed.condition_sha256,
                        sealed.evidence_set_sha256,
                        sealed.counterexample_arguments_sha256,
                        sealed.correspondence_sha256,
                        &sealed.proof,
                        &sealed.inhabited,
                        &sealed.counterexample_excluded,
                    ))?
            {
                return Err("conditional correspondence/seal identity is stale".into());
            }
        }
        if candidate.stage < ConditionalStage::CorrespondencePass
            && (candidate.correspondence.is_some() || candidate.sealed.is_some())
        {
            return Err("pre-correspondence conditional generation carries later-stage records".into());
        }
        if candidate.stage < ConditionalStage::HumanApproved && candidate.ratification.is_some() {
            return Err("pre-approval conditional generation carries a ratification".into());
        }
        if candidate.stage >= ConditionalStage::CorrespondencePass
            && candidate
                .ratification_gate_episode_id
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err("correspondence-passed conditional lacks its exact gate episode".into());
        }
        if candidate.stage < ConditionalStage::CorrespondencePass
            && candidate.ratification_gate_episode_id.is_some()
        {
            return Err("pre-correspondence conditional carries a gate episode".into());
        }
        if candidate.stage >= ConditionalStage::HumanApproved {
            let sealed = candidate.sealed.as_ref().ok_or("ratified conditional lacks seal")?;
            let ratification = candidate
                .ratification
                .as_ref()
                .ok_or("human-approved conditional lacks ratification")?;
            if ratification.target_id != *target
                || ratification.generation != candidate.stamped.generation
                || ratification.proposal_sha256 != sealed.proposal_sha256
                || ratification.target_statement_sha256 != sealed.target_statement_sha256
                || ratification.condition_sha256 != sealed.condition_sha256
                || ratification.evidence_set_sha256 != sealed.evidence_set_sha256
                || ratification.correspondence_sha256 != sealed.correspondence_sha256
                || ratification.seal_sha256 != sealed.seal_sha256
                || ratification.gate_episode_id.is_empty()
                || candidate.ratification_gate_episode_id.as_deref()
                    != Some(ratification.gate_episode_id.as_str())
                || ratification.approval_record_sha256.is_none()
            {
                return Err("conditional ratification does not bind the exact packet".into());
            }
        }
        if candidate.stage >= ConditionalStage::Open && candidate.disposition.is_none() {
            let sealed = candidate.sealed.as_ref().ok_or("open conditional lacks seal")?;
            let mut obligations = vec![&sealed.proof, &sealed.inhabited];
            obligations.extend(sealed.counterexample_excluded.as_ref());
            for obligation in obligations {
                let registered = state
                    .configured_challenge_targets
                    .get(&obligation.target_id)
                    .ok_or("open conditional obligation is not registered")?;
                if registered.lean != obligation.statement_lean
                    || registered.name != obligation.theorem_name
                    || !state.live.present_nodes.contains(&obligation.node)
                    || state
                        .challenge_claims
                        .get(&obligation.node)
                        != Some(&BTreeSet::from([obligation.target_id.clone()]))
                {
                    return Err("open conditional obligation differs from its seal".into());
                }
            }
        }
        if candidate.stage == ConditionalStage::Closed {
            let sealed = candidate.sealed.as_ref().ok_or("closed conditional lacks seal")?;
            let mut obligations = vec![&sealed.proof, &sealed.inhabited];
            obligations.extend(sealed.counterexample_excluded.as_ref());
            let closure_stale = obligations.into_iter().any(|obligation| {
                state.live.open_nodes.contains(&obligation.node)
                    || state.local_closure_records.get(&obligation.node).is_none_or(|record| {
                        record.node != obligation.node
                            || record.is_sentinel_hashed()
                            || !record.is_fresh_for_completion(state)
                    })
            });
            if candidate.disposition.is_none()
                && (state
                    .configured_challenge_targets
                    .get(target)
                    .is_some_and(|_| {
                        state
                            .live
                            .open_nodes
                            .contains(&NodeId::from(spec.name.as_str()))
                            == false
                    })
                    || state
                        .configured_challenge_targets
                        .get(&crate::model::refutation_target_id(target))
                        .is_some_and(|twin| {
                            !state.live.open_nodes.contains(&NodeId::from(twin.name.as_str()))
                        }))
            {
                return Err("closed conditional generation also closes an unconditional side".into());
            }
            if candidate.disposition.is_none() && closure_stale {
                return Err("closed conditional generation lacks fresh Lean closure".into());
            }
        }
    }
    Ok(())
}

pub fn conditional_ratification_packet(
    candidate: &ConditionalCandidateGeneration,
) -> Result<serde_json::Value, String> {
    if candidate.stage != ConditionalStage::CorrespondencePass
        || candidate.disposition.is_some()
    {
        return Err(
            "conditional ratification packet is not correspondence-passed and pending".into(),
        );
    }
    let correspondence = candidate
        .correspondence
        .as_ref()
        .ok_or("conditional ratification packet lacks correspondence approval")?;
    let sealed = candidate
        .sealed
        .as_ref()
        .ok_or("conditional ratification packet lacks mechanical seal")?;
    Ok(serde_json::json!({
        "generation": candidate.stamped.generation,
        "target_id": candidate.stamped.proposal.target_id,
        "target_statement_sha256": candidate.stamped.target_statement_sha256,
        "target_lean": candidate.correspondence_request.as_ref().map(|request| request.target_lean.clone()),
        "condition_lean": candidate.stamped.proposal.condition_lean,
        "condition_informal": candidate.stamped.proposal.condition_informal,
        "rationale": candidate.stamped.proposal.rationale,
        "trigger": candidate.stamped.proposal.trigger,
        "evidence": candidate.stamped.proposal.evidence,
        "assumption_status": candidate.stamped.assumption_snapshots,
        "proposal_sha256": candidate.stamped.proposal_sha256,
        "evidence_set_sha256": candidate.stamped.evidence_set_sha256,
        "correspondence": correspondence,
        "correspondence_sha256": sealed.correspondence_sha256,
        "sealed_statement": sealed.proof,
        "condition_inhabited_obligation": sealed.inhabited,
        "counterexample_exclusion_obligation": sealed.counterexample_excluded,
        "seal_sha256": sealed.seal_sha256,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_is_live() {
        assert!(CONDITIONAL_THEOREM_REACHABLE);
    }

    #[test]
    fn binder_parser_assigns_anonymous_instance_identity() {
        let got = parse_binders("{α : Type} [_ : Ord α] (x : α)").unwrap();
        assert_eq!(got[1].names, vec!["_conditionalBinder1"]);
        assert!(got[1].instance);
    }
}

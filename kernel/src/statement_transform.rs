//! Checked Lean statement derivations. I/O is confined to `observe`; validation
//! consumes the frozen receipt only, so event replay never executes Lean.
use crate::model::{ChallengeTargetSpec, ProtocolState, WrapperResponse};
use crate::trust_base::{canonical_json, raw_sha256, ConditionalTheoremProposal, Sha256Digest};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: String,
    pub statement: String,
    pub context: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<String>>,
}
impl Input {
    pub fn new(spec: &ChallengeTargetSpec, proposal: Option<&ConditionalTheoremProposal>) -> Self {
        Self {
            name: spec.name.clone(), statement: spec.lean.clone(), context: spec.statement_elaboration_context.as_ref().unwrap_or(&spec.namespace_context).clone(),
            condition: proposal.map(|p| p.condition_lean.trim().to_owned()),
            arguments: proposal.and_then(|p| p.concrete_counterexample_arguments.as_ref())
                .map(|args| args.iter().map(|s| s.trim().to_owned()).collect()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalTypes {
    pub proof_type: String,
    pub inhabited_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_type: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    pub refutation_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditional: Option<ConditionalTypes>,
}

/// Runtime-issued observation, never an agent assertion. Both successful and
/// refused elaboration are retained on the response, making correction replayable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Observation {
    pub version: u32,
    pub input_sha256: Sha256Digest,
    pub helper_sha256: Sha256Digest,
    pub environment: serde_json::Value,
    pub output: Option<Output>,
    pub error: String,
    pub receipt_sha256: Sha256Digest,
}
fn digest(value: &impl Serialize) -> Result<Sha256Digest, String> {
    Ok(raw_sha256(&canonical_json(value).map_err(|e| e.to_string())?))
}
impl Observation {
    fn digest(&self) -> Result<Sha256Digest, String> {
        digest(&(self.version, self.input_sha256, self.helper_sha256, &self.environment, &self.output, &self.error))
    }
    #[cfg(test)]
    pub(crate) fn fixture(input: &Input, output: Option<Output>, error: &str) -> Self {
        let mut receipt = Self {
            version: 1,
            input_sha256: digest(input).unwrap(),
            helper_sha256: raw_sha256(b"fixture helper"),
            environment: serde_json::json!({"fixture": true}),
            output,
            error: error.to_owned(),
            ..Default::default()
        };
        receipt.receipt_sha256 = receipt.digest().unwrap();
        receipt
    }
    pub fn checked<'a>(&'a self, input: &Input) -> Result<&'a Output, String> {
        if self.version != 1 || self.input_sha256 != digest(input)? || self.receipt_sha256 != self.digest()? {
            return Err("Lean statement derivation is absent, stale, or corrupt".into());
        }
        if !self.error.is_empty() { return Err(format!("Lean statement preflight: {}", self.error)); }
        if self.helper_sha256 == Sha256Digest::ZERO || !self.environment.is_object() {
            return Err("Lean derivation lacks helper/environment provenance".into());
        }
        let out = self.output.as_ref().ok_or("Lean derivation has no checked output")?;
        if out.refutation_type.trim().is_empty() || out.conditional.is_some() != input.condition.is_some() {
            return Err("Lean derivation has incomplete output".into());
        }
        if let Some(c) = &out.conditional {
            if c.proof_type.trim().is_empty() || c.inhabited_type.trim().is_empty()
                || c.excluded_type.is_some() != input.arguments.is_some()
                || c.excluded_type.as_ref().is_some_and(|s| s.trim().is_empty()) {
                return Err("Lean derivation has incomplete conditional obligations".into());
            }
        }
        Ok(out)
    }
}

pub fn observe(repo: &Path, input: &Input) -> Observation {
    let mut observation = Observation { version: 1, input_sha256: digest(input).unwrap_or_default(), ..Default::default() };
    let result = (|| {
        let raw = crate::runtime_cli_observations::run_repo_command_json(repo,
            "lean-statement-transform", &["--input-json".into(), serde_json::to_string(input).map_err(|e| e.to_string())?])?;
        if raw.get("status").and_then(|v| v.as_str()) != Some("ok") {
            return Err(raw.get("error").and_then(|v| v.as_str()).unwrap_or("Lean derivation failed").to_owned());
        }
        observation.helper_sha256 = serde_json::from_value(raw.get("helper_sha256").cloned().ok_or("missing helper digest")?).map_err(|e| e.to_string())?;
        observation.environment = raw.get("environment").cloned().ok_or("missing Lean environment")?;
        observation.output = Some(serde_json::from_value(raw.get("result").cloned().ok_or("missing Lean output")?).map_err(|e| e.to_string())?);
        Ok::<_, String>(())
    })();
    if let Err(error) = result { observation.error = error; observation.output = None; }
    observation.receipt_sha256 = observation.digest().unwrap_or_default();
    observation
}

/// Reuse the correspondence helper's typed semantic closure, through the same
/// confined checker transport. The signature's dummy proof is never evidence.
pub fn observe_signature_payload(repo: &Path, node: &str, input: &Input) -> Result<String, String> {
    let mut value = serde_json::to_value(input).map_err(|e| e.to_string())?;
    value["fingerprint_node"] = serde_json::json!(node);
    let raw = crate::runtime_cli_observations::run_repo_command_json(repo,
        "lean-statement-transform", &["--input-json".into(), value.to_string()])?;
    if raw.get("status").and_then(|v| v.as_str()) != Some("ok") {
        return Err(raw.get("error").and_then(|v| v.as_str()).unwrap_or("signature fingerprint unavailable").into());
    }
    raw.get("result").and_then(|v| v.get("payload")).and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && !s.contains("||missing|"))
        .map(str::to_owned).ok_or_else(|| "signature fingerprint missing or incomplete".into())
}

/// Overwrite any agent-supplied carrier, including on provided/injected responses.
/// The resulting response is the event that gets durably recorded.
pub fn hydrate_response(state: &ProtocolState, repo: Option<&Path>, response: &mut WrapperResponse) {
    let (proposal, slot) = match response {
        WrapperResponse::Worker(r) => (&r.conditional_theorem_proposal, &mut r.conditional_statement_derivation),
        WrapperResponse::StuckMathAudit(r) => (&r.conditional_theorem_proposal, &mut r.conditional_statement_derivation),
        _ => return,
    };
    *slot = None;
    if !state.lean_statement_transforms_enabled { return; }
    let Some(proposal) = proposal else { return };
    let Some(spec) = state.configured_challenge_targets.get(&proposal.target_id) else { return };
    let input = Input::new(spec, Some(proposal));
    if let Some(repo) = repo { *slot = Some(observe(repo, &input)); }
}

/// FILESPEC already owns the proof/declaration boundary; preserve its complete
/// preamble instead of reconstructing a reduced import/open context.
pub fn source_context(source: &str) -> String {
    source.lines().take_while(|line| !line.trim_start().starts_with("-- [TABLET NODE:"))
        .collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::trust_base::conditionalization::*;

    fn fixture() -> (ProtocolState, ConditionalTheoremProposal, Observation) {
        let mut state = ProtocolState::default();
        state.lean_statement_transforms_enabled = true;
        let id = ChallengeTargetId::from("goal:test");
        let spec = ChallengeTargetSpec {
            name: "Original".into(), lean: "theorem Original (n : Nat) : n = 0 := by".into(),
            namespace_context: "import Lean\n".into(), resolution: ChallengeResolution::Decide,
            ..Default::default()
        };
        let proposal = ConditionalTheoremProposal {
            target_id: id.clone(), condition_lean: "n < 3".into(),
            condition_informal: "n is below three".into(), rationale: "restrict the domain".into(),
            concrete_counterexample_arguments: Some(vec!["Nat.succ 2".into()]), ..Default::default()
        };
        let input = Input::new(&spec, Some(&proposal));
        let mut receipt = Observation {
            version: 1, input_sha256: digest(&input).unwrap(), helper_sha256: raw_sha256(b"fixture helper"),
            environment: serde_json::json!({"fixture":true}),
            output: Some(Output {
                refutation_type: "¬ (∀ n : Nat, n = 0)".into(),
                conditional: Some(ConditionalTypes {
                    proof_type: "∀ n : Nat, n < 3 → n = 0".into(),
                    inhabited_type: "∃ n : Nat, n < 3".into(),
                    excluded_type: Some("¬ (Nat.succ 2 < 3)".into()),
                }),
            }), ..Default::default()
        };
        receipt.receipt_sha256 = receipt.digest().unwrap();
        state.configured_challenge_targets.insert(id, spec);
        (state, proposal, receipt)
    }
    fn verdict(request: &ConditionalCorrespondenceRequest) -> ConditionalCorrespondenceVerdict {
        ConditionalCorrespondenceVerdict {
            request_sha256: request.request_sha256, proposal_sha256: request.proposal.proposal_sha256,
            boundary_expression_correct: true, condition_relevant: true, realizable_non_vacuous: true,
            obligation_preserved_on_domain: true, rust_axioms_backed_by_approved_assumptions: true,
            findings: "fixture correspondence".into(),
        }
    }

    #[test]
    fn checked_obligations_are_reviewed_sealed_and_replayed_without_io() {
        let (mut state, proposal, receipt) = fixture();
        let stamped = stamp_conditional_proposal(&state, proposal.clone(), 1, vec![]).unwrap();
        assert!(build_conditional_correspondence_request(&state, &stamped, serde_json::Value::Null, serde_json::Value::Null).is_err());
        let request = build_conditional_correspondence_request_with_derivation(&state, &stamped,
            serde_json::Value::Null, serde_json::Value::Null, Some(&receipt)).unwrap();
        let (approval, sealed) = seal_conditional_theorem(&state, &stamped, &request, &verdict(&request)).unwrap();
        assert!(sealed.proof.statement_lean.contains(&receipt.output.as_ref().unwrap().conditional.as_ref().unwrap().proof_type));
        state.trust_base.conditional_candidates.insert(proposal.target_id.clone(), ConditionalCandidateGeneration {
            stage: ConditionalStage::CorrespondencePass, stamped, correspondence_request: Some(request.clone()),
            correspondence: Some(approval), sealed: Some(sealed.clone()), ratification_gate_episode_id: Some("fixture-gate".into()),
            ..Default::default()
        });
        validate_conditional_protocol_state(&state).unwrap();
        let replay: ProtocolState = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        validate_conditional_protocol_state(&replay).unwrap();
        let candidate = &replay.trust_base.conditional_candidates[&proposal.target_id];
        assert_eq!(candidate.sealed.as_ref(), Some(&sealed));
        assert!(candidate.ratification.is_none());
        let mut changed = request.clone();
        changed.statement_derivation.as_mut().unwrap().output.as_mut().unwrap().conditional.as_mut().unwrap().proof_type = "True".into();
        assert!(seal_conditional_theorem(&state, &request.proposal, &changed, &verdict(&request)).is_err());
        let mut stale = state.clone();
        stale.configured_challenge_targets.get_mut(&proposal.target_id).unwrap().namespace_context.push_str("open Nat\n");
        assert!(validate_conditional_protocol_state(&stale).is_err());
    }

    #[test]
    fn activation_preserves_legacy_packets_and_rejects_missing_new_receipts() {
        let (mut state, proposal, _) = fixture();
        state.lean_statement_transforms_enabled = false;
        let stamped = stamp_conditional_proposal(&state, proposal, 1, vec![]).unwrap();
        let request = build_conditional_correspondence_request(&state, &stamped, serde_json::Value::Null, serde_json::Value::Null).unwrap();
        assert!(!serde_json::to_string(&request).unwrap().contains("statement_derivation"));
        let before = seal_conditional_theorem(&state, &stamped, &request, &verdict(&request)).unwrap();
        state.lean_statement_transforms_enabled = true;
        let after = seal_conditional_theorem(&state, &stamped, &request, &verdict(&request)).unwrap();
        assert_eq!(before, after);
        assert!(build_conditional_correspondence_request(&state, &stamped, serde_json::Value::Null, serde_json::Value::Null).is_err());
    }

    #[test]
    fn runtime_discards_agent_supplied_derivation_even_without_a_repository() {
        let (state, proposal, receipt) = fixture();
        let mut response = WrapperResponse::StuckMathAudit(StuckMathAuditResponse {
            conditional_theorem_proposal: Some(proposal), conditional_statement_derivation: Some(receipt), ..Default::default()
        });
        hydrate_response(&state, None, &mut response);
        let WrapperResponse::StuckMathAudit(response) = response else { unreachable!() };
        assert!(response.conditional_statement_derivation.is_none());
    }

    #[test]
    fn migration_boundary_refreshes_request_without_altering_registered_statements() {
        let (mut state, _, _) = fixture();
        state.lean_statement_transforms_enabled = false;
        state.in_flight_request = Some(Box::new(state.expected_request(1, RequestKind::Review)));
        let specs = state.configured_challenge_targets.clone();
        let metadata = crate::runtime::RuntimeMetadata {
            lean_statement_transforms_activation_event_count: Some(5),
            audited_polarity_binding_activation_event_count: Some(5), ..Default::default()
        };
        metadata.apply_audit_routing_boundary(&mut state, 4);
        assert!(!state.lean_statement_transforms_enabled && !state.audited_polarity_binding_enabled);
        metadata.apply_audit_routing_boundary(&mut state, 5);
        assert!(state.in_flight_request.as_ref().unwrap().lean_statement_transforms_enabled);
        assert!(state.audited_polarity_binding_enabled);
        assert_eq!(state.configured_challenge_targets, specs);
    }
}

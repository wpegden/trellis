//! Checked Lean proof ingestion for trust-v1 campaigns.
//!
//! Rust execution is deliberately outside this module. A checked positive or
//! negative Lean closure is the only unconditional result authority.

use super::canonical::{
    canonical_json_value, self_digest, tagged_hash, DomainTag, Sha256Digest, TrustError,
};
use super::closure::{VerifiedEvidenceClosure, VerifiedSeedDefinitionClosure};
use super::records::AuthoritativeRecord;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedPositiveProof {
    target_id: String,
    target_statement_sha256: Sha256Digest,
    generated_theorem_statement_sha256: Sha256Digest,
    checked_proof_artifact_sha256: Sha256Digest,
    checker_toolchain_sha256: Sha256Digest,
    approved_axiom_closure_sha256: Sha256Digest,
    semantic_definition_closure_sha256: Sha256Digest,
    proof_receipt: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedNegativeProof {
    target_id: String,
    target_statement_sha256: Sha256Digest,
    generated_not_theorem_statement_sha256: Sha256Digest,
    checked_not_proof_artifact_sha256: Sha256Digest,
    checker_toolchain_sha256: Sha256Digest,
    approved_axiom_closure_sha256: Sha256Digest,
    semantic_definition_closure_sha256: Sha256Digest,
    proof_receipt: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordedCampaignProof {
    Positive(RecordedPositiveProof),
    Negative(RecordedNegativeProof),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedPositiveProof {
    pub proof_subject_sha256: Sha256Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedNegativeProof {
    pub proof_subject_sha256: Sha256Digest,
}

pub struct TrustDerivationPipeline {
    seed: VerifiedSeedDefinitionClosure,
    evidence: VerifiedEvidenceClosure,
    launch_acknowledgment_sha256: Sha256Digest,
    positive_proofs: BTreeMap<String, Sha256Digest>,
    negative_proofs: BTreeMap<String, Sha256Digest>,
}

impl TrustDerivationPipeline {
    pub fn open(
        seed_manifest: &AuthoritativeRecord,
        seed: VerifiedSeedDefinitionClosure,
        evidence: VerifiedEvidenceClosure,
        launch_acknowledgment_sha256: Option<Sha256Digest>,
    ) -> Result<Self, TrustError> {
        if seed_manifest.contract().record_schema
            != "trellis-seed-authored-definition-manifest/v1"
            || seed_manifest.digest() != seed.seed_manifest_sha256
        {
            return Err(TrustError::new(
                "pipeline_seed_closure_wrong_manifest",
                "pipeline requires the exact verified seed and definition bundle",
            ));
        }
        let launch_acknowledgment_sha256 = launch_acknowledgment_sha256
            .filter(|digest| *digest != Sha256Digest::ZERO)
            .ok_or_else(|| {
                TrustError::new(
                    "pipeline_launch_acknowledgment_missing",
                    "proof ingestion requires the launch acknowledgment binding",
                )
            })?;
        Ok(Self {
            seed,
            evidence,
            launch_acknowledgment_sha256,
            positive_proofs: BTreeMap::new(),
            negative_proofs: BTreeMap::new(),
        })
    }

    pub fn target_has_formal_result(&self, target_id: &str) -> bool {
        self.positive_proofs.contains_key(target_id)
            || self.negative_proofs.contains_key(target_id)
    }

    pub fn record_campaign_local_closure_result(
        &mut self,
        state: &crate::model::ProtocolState,
        target_id: &str,
    ) -> Result<RecordedCampaignProof, TrustError> {
        if self.target_has_formal_result(target_id) {
            return Err(error("duplicate_or_conflicting_target_result", "target already has a result"));
        }
        self.require_seed_target(target_id)?;
        let primary = crate::model::ChallengeTargetId::from(target_id);
        let spec = state.configured_challenge_targets.get(&primary).ok_or_else(|| {
            error("campaign_proof_target_unconfigured", format!("target {target_id} is not configured"))
        })?;
        let polarity = state.live_polarity(&primary);
        if spec.resolution == crate::model::ChallengeResolution::Prove
            && polarity != crate::model::ChallengePolarity::Prove
        {
            return Err(error("campaign_non_decide_disproof_invalid", "prove-only target is on disprove polarity"));
        }
        let node_name = if spec.resolution == crate::model::ChallengeResolution::Decide {
            state.decide_pair_node_for_polarity(&primary, polarity).ok_or_else(|| {
                error("campaign_proof_node_unresolved", format!("cannot resolve live side for {target_id}"))
            })?
        } else {
            spec.name.clone()
        };
        let node = crate::model::NodeId::from(node_name.as_str());
        if state.live.open_nodes.contains(&node) {
            return Err(error("campaign_local_closure_owner_open", format!("{node_name} remains open")));
        }
        let record = state.local_closure_records.get(&node).ok_or_else(|| {
            error("campaign_local_closure_missing", format!("{node_name} lacks a local closure"))
        })?;
        record.is_consistent_with_state(state, true).map_err(|reason| {
            error("campaign_local_closure_inconsistent", format!("{target_id}: {reason}"))
        })?;
        if record.node != node || record.is_sentinel_hashed() {
            return Err(error("campaign_local_closure_identity_invalid", "closure identity is incomplete"));
        }
        self.require_local_closure_evidence(record)?;

        let target_statement_sha256 = crate::model::registered_statement_text_sha256(state, &primary)
            .ok_or_else(|| error("campaign_statement_missing", "registered target statement is absent"))?;
        let generated_statement_sha256: Sha256Digest = record.active_statement_hash.parse()?;
        if polarity == crate::model::ChallengePolarity::Prove
            && generated_statement_sha256 != target_statement_sha256
        {
            return Err(error("campaign_positive_statement_mismatch", "closed statement differs from registered target"));
        }
        let checker_toolchain_sha256: Sha256Digest = record.toolchain_hash.parse()?;
        let approved_axiom_closure_sha256: Sha256Digest = record.approved_axioms_hash.parse()?;
        for (name, value) in [
            ("active_decl_hash", record.active_decl_hash.as_str()),
            ("lake_manifest_hash", record.lake_manifest_hash.as_str()),
            ("preamble_hash", record.preamble_hash.as_str()),
        ] {
            if value.parse::<Sha256Digest>().map_err(|_| error("campaign_local_closure_hash_invalid", name))?
                == Sha256Digest::ZERO
            {
                return Err(error("campaign_local_closure_hash_zero", name));
            }
        }
        let semantic_definition_closure_sha256 = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&serde_json::json!({
                "node_certificate": record.node_certificate,
                "node_certificate_evidence": record.node_certificate_evidence,
                "active_decl_hash": record.active_decl_hash,
                "active_statement_hash": record.active_statement_hash,
            }))?,
        );
        let mut receipt = serde_json::json!({
            "schema": "trellis-local-closure-proof-receipt/v1",
            "target_id": target_id,
            "target_statement_sha256": target_statement_sha256,
            "polarity": match polarity { crate::model::ChallengePolarity::Prove => "prove", crate::model::ChallengePolarity::Disprove => "disprove" },
            "live_target_id": state.live_target_of_decide_pair(&primary).as_str(),
            "node_id": node.as_str(),
            "generated_statement_sha256": generated_statement_sha256,
            "checker_toolchain_sha256": checker_toolchain_sha256,
            "approved_axiom_closure_sha256": approved_axiom_closure_sha256,
            "semantic_definition_closure_sha256": semantic_definition_closure_sha256,
            "launch_acknowledgment_sha256": self.launch_acknowledgment_sha256,
            "local_closure_record": serde_json::to_value(record).map_err(|e| error("campaign_local_closure_encode_failed", e.to_string()))?,
            "proof_receipt_sha256": Sha256Digest::ZERO,
        });
        let artifact = self_digest(DomainTag::RawArtifact, &receipt, "proof_receipt_sha256")?;
        receipt["proof_receipt_sha256"] = Value::String(artifact.to_string());

        match polarity {
            crate::model::ChallengePolarity::Prove => self.record_positive_proof(CheckedPositiveProof {
                target_id: target_id.to_owned(),
                target_statement_sha256,
                generated_theorem_statement_sha256: generated_statement_sha256,
                checked_proof_artifact_sha256: artifact,
                checker_toolchain_sha256,
                approved_axiom_closure_sha256,
                semantic_definition_closure_sha256,
                proof_receipt: receipt,
            }).map(RecordedCampaignProof::Positive),
            crate::model::ChallengePolarity::Disprove => self.record_negative_proof(CheckedNegativeProof {
                target_id: target_id.to_owned(),
                target_statement_sha256,
                generated_not_theorem_statement_sha256: generated_statement_sha256,
                checked_not_proof_artifact_sha256: artifact,
                checker_toolchain_sha256,
                approved_axiom_closure_sha256,
                semantic_definition_closure_sha256,
                proof_receipt: receipt,
            }).map(RecordedCampaignProof::Negative),
        }
    }

    pub fn record_positive_proof(&mut self, proof: CheckedPositiveProof) -> Result<RecordedPositiveProof, TrustError> {
        self.validate_proof(&proof.target_id, proof.target_statement_sha256, proof.generated_theorem_statement_sha256, proof.checked_proof_artifact_sha256, proof.checker_toolchain_sha256, proof.approved_axiom_closure_sha256, proof.semantic_definition_closure_sha256, &proof.proof_receipt)?;
        let envelope = serde_json::json!({"schema":"trellis-checked-positive-proof/v1","target_id":proof.target_id,"target_statement_sha256":proof.target_statement_sha256,"generated_theorem_statement_sha256":proof.generated_theorem_statement_sha256,"checked_proof_artifact_sha256":proof.checked_proof_artifact_sha256,"checker_toolchain_sha256":proof.checker_toolchain_sha256,"approved_axiom_closure_sha256":proof.approved_axiom_closure_sha256,"semantic_definition_closure_sha256":proof.semantic_definition_closure_sha256,"proof_receipt":proof.proof_receipt});
        let subject = tagged_hash(DomainTag::RawArtifact, &canonical_json_value(&envelope)?);
        if self.negative_proofs.contains_key(&proof.target_id) || self.positive_proofs.insert(proof.target_id, subject).is_some() {
            return Err(error("duplicate_or_conflicting_target_result", "target already has a result"));
        }
        Ok(RecordedPositiveProof { proof_subject_sha256: subject })
    }

    pub fn record_negative_proof(&mut self, proof: CheckedNegativeProof) -> Result<RecordedNegativeProof, TrustError> {
        self.validate_proof(&proof.target_id, proof.target_statement_sha256, proof.generated_not_theorem_statement_sha256, proof.checked_not_proof_artifact_sha256, proof.checker_toolchain_sha256, proof.approved_axiom_closure_sha256, proof.semantic_definition_closure_sha256, &proof.proof_receipt)?;
        let envelope = serde_json::json!({"schema":"trellis-checked-negative-proof/v1","target_id":proof.target_id,"target_statement_sha256":proof.target_statement_sha256,"generated_not_theorem_statement_sha256":proof.generated_not_theorem_statement_sha256,"checked_not_proof_artifact_sha256":proof.checked_not_proof_artifact_sha256,"checker_toolchain_sha256":proof.checker_toolchain_sha256,"approved_axiom_closure_sha256":proof.approved_axiom_closure_sha256,"semantic_definition_closure_sha256":proof.semantic_definition_closure_sha256,"proof_receipt":proof.proof_receipt});
        let subject = tagged_hash(DomainTag::RawArtifact, &canonical_json_value(&envelope)?);
        if self.positive_proofs.contains_key(&proof.target_id) || self.negative_proofs.insert(proof.target_id, subject).is_some() {
            return Err(error("duplicate_or_conflicting_target_result", "target already has a result"));
        }
        Ok(RecordedNegativeProof { proof_subject_sha256: subject })
    }

    fn validate_proof(&self, target_id: &str, target_statement: Sha256Digest, generated: Sha256Digest, artifact: Sha256Digest, checker: Sha256Digest, axioms: Sha256Digest, semantics: Sha256Digest, receipt: &Value) -> Result<(), TrustError> {
        self.require_seed_target(target_id)?;
        if [target_statement, generated, artifact, checker, axioms, semantics].contains(&Sha256Digest::ZERO) {
            return Err(error("proof_zero_artifact", "proof identity cannot be zero"));
        }
        let embedded: Sha256Digest = receipt.get("proof_receipt_sha256").and_then(Value::as_str).ok_or_else(|| error("proof_receipt_digest_missing", "proof receipt lacks digest"))?.parse()?;
        if embedded != artifact {
            return Err(error("proof_receipt_digest_mismatch", "proof receipt digest differs from checked artifact"));
        }
        Ok(())
    }

    fn require_seed_target(&self, target_id: &str) -> Result<(), TrustError> {
        if self.seed.canonical_values_by_digest.values().any(|value| value.get("schema").and_then(Value::as_str) == Some("trellis-campaign-target-definition/v1") && value.get("target_id").and_then(Value::as_str) == Some(target_id)) {
            Ok(())
        } else {
            Err(error("formal_target_not_seed_frozen", "formal target is absent from the seed"))
        }
    }

    fn require_local_closure_evidence(&self, record: &crate::model::LocalClosureRecord) -> Result<(), TrustError> {
        for (logical_id, field, value) in [
            ("lean-toolchain", "toolchain_hash", record.toolchain_hash.as_str()),
            ("lake-manifest", "lake_manifest_hash", record.lake_manifest_hash.as_str()),
            ("aeneas-generated-preamble", "preamble_hash", record.preamble_hash.as_str()),
            ("lean-checker-executable", "lean_executable_hash", record.lean_executable_hash.as_str()),
            ("lake-driver-executable", "lake_executable_hash", record.lake_executable_hash.as_str()),
            ("local-closure-checker-script", "checker_script_hash", record.checker_script_hash.as_str()),
        ] {
            let digest: Sha256Digest = value.parse().map_err(|_| error("local_closure_platform_hash_invalid", field))?;
            let approved = self.evidence.leaves_by_logical_id.get(logical_id).ok_or_else(|| error("local_closure_platform_evidence_missing", logical_id))?;
            if digest == Sha256Digest::ZERO || approved.raw_sha256 != digest {
                return Err(error("local_closure_platform_evidence_mismatch", field));
            }
        }
        if self.evidence.leaves_by_logical_id.get("trusted-platform-boundary-v1").is_none_or(|leaf| leaf.raw_sha256 == Sha256Digest::ZERO) {
            return Err(error("local_closure_trusted_platform_boundary_missing", "trusted platform boundary is absent"));
        }
        Ok(())
    }
}

fn error(code: &'static str, message: impl Into<String>) -> TrustError {
    TrustError::new(code, message)
}

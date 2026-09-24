use super::canonical::{
    canonical_json_value, self_digest, tagged_hash, verify_self_digest, DomainTag, Sha256Digest,
    TrustError,
};
use super::schema::{RecordContract, SchemaRegistry};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthoritativeRecord {
    value: Value,
    contract: RecordContract,
    digest: Sha256Digest,
}

impl AuthoritativeRecord {
    pub fn parse(registry: &SchemaRegistry, value: Value) -> Result<Self, TrustError> {
        let contract = registry.validate_record(&value)?;
        let digest = if let Some(field) = contract.self_digest_field {
            verify_self_digest(contract.domain_tag, &value, field)?
        } else {
            tagged_hash(contract.domain_tag, &canonical_json_value(&value)?)
        };
        Ok(Self {
            value,
            contract,
            digest,
        })
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn into_value(self) -> Value {
        self.value
    }

    pub fn contract(&self) -> RecordContract {
        self.contract
    }

    pub fn digest(&self) -> Sha256Digest {
        self.digest
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, TrustError> {
        canonical_json_value(&self.value)
    }
}

/// Event-log payload vocabulary for required-v1 trust records (Q1, plan doc
/// 32 Stage 3).  These are NOT journal events: the sole carriers are the
/// supervisor event log (`EventLogRecord.trust_record`) and the git-tagged
/// checkpoint history.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SeedCommitted,
    AdvanceGateApproved,
    AdvanceGateFeedback,
    AuditAuthorization,
    ProtectedReapprovalApproved,
    ProtectedReapprovalFeedback,
}

impl EventKind {
    pub const ALL: [Self; 6] = [
        Self::SeedCommitted,
        Self::AdvanceGateApproved,
        Self::AdvanceGateFeedback,
        Self::AuditAuthorization,
        Self::ProtectedReapprovalApproved,
        Self::ProtectedReapprovalFeedback,
    ];

    /// The kinds whose commit rides the Stage-3 gate-decision transaction:
    /// record file + `supervisor2/trust-decision-*` tag written by the
    /// checkpoint hook, decision durable before state/log persist.
    pub const DECISION_KINDS: [Self; 5] = [
        Self::AdvanceGateApproved,
        Self::AdvanceGateFeedback,
        Self::AuditAuthorization,
        Self::ProtectedReapprovalApproved,
        Self::ProtectedReapprovalFeedback,
    ];

    pub fn is_decision_kind(self) -> bool {
        Self::DECISION_KINDS.contains(&self)
    }
}

/// The five named seed roots bound into every trust record (and rechecked
/// against state at Q7 finalization).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustRecordSeedRoots {
    pub seed_manifest_sha256: Sha256Digest,
    pub seed_definition_bundle_sha256: Sha256Digest,
    pub evidence_tool_manifest_sha256: Sha256Digest,
    pub authored_semantic_root: Sha256Digest,
    pub approved_evidence_tool_input_root: Sha256Digest,
}

/// One required-v1 trust record: the payload attached to the event-log line
/// of the step in which the decision (or station outcome) occurred, and the
/// content committed as the tracked decision-record file
/// `.trellis-history/trust-decisions/<digest>.json`.
///
/// `record_sha256` is a self digest under `DomainTag::TrustDecision`
/// (computed with the field omitted, then embedded), so the full digest
/// travels inside the record bytes themselves — the approve-arm digest
/// installed as `current_human_approval_event_hash` is exactly this value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustRecord {
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    pub gate_episode_id: String,
    pub presentation_sha256: Sha256Digest,
    pub seed_roots: TrustRecordSeedRoots,
    pub cycle: u32,
    /// The dense event-log index this record's line is appended at.  A
    /// surviving decision tag whose intended index differs from the current
    /// event count is historical-lookup-only (Codex R3-4a.2): it is never
    /// re-appended or enacted.
    pub intended_event_log_index: u64,
    pub record_sha256: Sha256Digest,
}

impl TrustRecord {
    pub fn compute_digest(&self) -> Result<Sha256Digest, TrustError> {
        let value = serde_json::to_value(self).map_err(|error| {
            TrustError::new("trust_record_serialization_failed", error.to_string())
        })?;
        self_digest(DomainTag::TrustDecision, &value, "record_sha256")
    }

    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.record_sha256 = self.compute_digest()?;
        Ok(self)
    }

    /// Recompute the self digest and require it to equal the embedded
    /// `record_sha256`.  Every consumer of a persisted/recovered record
    /// (event-log scan, tag lookup, archive embedding) verifies before use.
    pub fn verify(&self) -> Result<Sha256Digest, TrustError> {
        let computed = self.compute_digest()?;
        if computed != self.record_sha256 {
            return Err(TrustError::new(
                "trust_record_digest_mismatch",
                format!(
                    "embedded {}, computed {computed}",
                    self.record_sha256
                ),
            ));
        }
        Ok(computed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_record() -> TrustRecord {
        TrustRecord {
            kind: EventKind::AdvanceGateApproved,
            lane: None,
            gate_episode_id: "advance:fixture".into(),
            presentation_sha256: "11".repeat(32).parse().unwrap(),
            seed_roots: TrustRecordSeedRoots {
                seed_manifest_sha256: "22".repeat(32).parse().unwrap(),
                seed_definition_bundle_sha256: "33".repeat(32).parse().unwrap(),
                evidence_tool_manifest_sha256: "44".repeat(32).parse().unwrap(),
                authored_semantic_root: "55".repeat(32).parse().unwrap(),
                approved_evidence_tool_input_root: "66".repeat(32).parse().unwrap(),
            },
            cycle: 3,
            intended_event_log_index: 17,
            record_sha256: Sha256Digest::ZERO,
        }
        .seal()
        .unwrap()
    }

    #[test]
    fn trust_record_seals_and_verifies_self_digest() {
        let record = fixture_record();
        assert_ne!(record.record_sha256, Sha256Digest::ZERO);
        assert_eq!(record.verify().unwrap(), record.record_sha256);

        let mut tampered = record.clone();
        tampered.cycle = 4;
        let error = tampered.verify().unwrap_err();
        assert_eq!(error.code, "trust_record_digest_mismatch");
    }

    #[test]
    fn trust_record_round_trips_serde_and_math_lane_key_is_omitted() {
        let record = fixture_record();
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("\"lane\""), "None lane must be omitted: {json}");
        let reloaded: TrustRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded, record);
        assert_eq!(reloaded.verify().unwrap(), record.record_sha256);
    }

    #[test]
    fn event_kind_vocabulary_is_the_retained_six() {
        assert_eq!(EventKind::ALL.len(), 6);
        for kind in EventKind::DECISION_KINDS {
            assert!(EventKind::ALL.contains(&kind));
            assert!(kind.is_decision_kind());
        }
        assert!(!EventKind::SeedCommitted.is_decision_kind());
        // Serde names are the snake_case vocabulary the event log carries.
        assert_eq!(
            serde_json::to_string(&EventKind::AdvanceGateApproved).unwrap(),
            "\"advance_gate_approved\""
        );
    }
}

use super::canonical::{
    canonical_json_value, self_digest, tagged_hash, verify_self_digest, DomainTag, Sha256Digest,
    TrustError,
};
use super::schema::{RecordContract, SchemaRegistry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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

    /// Parse a schema-selected canonical record whose normative schema does
    /// not carry an in-band `schema` discriminator (qualification profiles
    /// are the v1 instance). The selector is closed in `RecordContract` and
    /// therefore cannot be supplied by an untrusted archive.
    pub fn parse_as(
        registry: &SchemaRegistry,
        record_schema: &str,
        value: Value,
    ) -> Result<Self, TrustError> {
        let contract = RecordContract::for_record_schema(record_schema)?;
        registry.validate(contract.schema_id, &value)?;
        let digest = if let Some(field) = contract.self_digest_field {
            verify_self_digest(contract.domain_tag, &value, field)?
        } else {
            tagged_hash(contract.domain_tag, &canonical_json_value(&value)?)
        };
        Ok(Self { value, contract, digest })
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    CanonicalRecord(AuthoritativeRecord),
    RawArtifact(Vec<u8>),
    ClosureTransition(AuthoritativeRecord),
}

impl Subject {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CanonicalRecord(_) => "canonical_record",
            Self::RawArtifact(_) => "raw_artifact",
            Self::ClosureTransition(_) => "closure_transition",
        }
    }

    pub fn domain_tag(&self) -> DomainTag {
        match self {
            Self::CanonicalRecord(record) | Self::ClosureTransition(record) => {
                record.contract.domain_tag
            }
            Self::RawArtifact(_) => DomainTag::RawArtifact,
        }
    }

    pub fn digest(&self) -> Result<Sha256Digest, TrustError> {
        match self {
            Self::CanonicalRecord(record) | Self::ClosureTransition(record) => {
                Ok(record.digest())
            }
            Self::RawArtifact(bytes) => Ok(tagged_hash(DomainTag::RawArtifact, bytes)),
        }
    }

    pub fn canonical_json(&self) -> Option<&Value> {
        match self {
            Self::CanonicalRecord(record) | Self::ClosureTransition(record) => {
                Some(record.value())
            }
            Self::RawArtifact(_) => None,
        }
    }

    pub fn raw_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::RawArtifact(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub fn reference_mode(&self) -> ReferenceMode {
        match self {
            Self::CanonicalRecord(record) | Self::ClosureTransition(record) => {
                if record.contract.self_digest_field.is_some() {
                    ReferenceMode::EmbeddedSelfDigest
                } else {
                    ReferenceMode::FullRecordHash
                }
            }
            Self::RawArtifact(_) => ReferenceMode::RawBytesHash,
        }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SeedCommitted,
    SourceClaimLineageRegistered,
    SourceClaimLineageResolutionRecorded,
    SourceModelMismatchResolved,
    AdvanceGateApproved,
    AdvanceGateFeedback,
    ProofChecked,
    WitnessRefutationChecked,
    UnrestrictedVerdictRecorded,
    SourceValidationClassified,
    SourceValidationAttemptRecorded,
    SourceValidationOutcomeRecorded,
    ReflectionValidationResultRecorded,
    SourceValidationHistorySummarized,
    ApprovedProfileSelected,
    ConditionalStatementGenerated,
    QualificationObligationsChecked,
    NoQualifiedResultEstablished,
    ApplicabilityClassified,
    ExternalClaimRowsGenerated,
    PackageAuthorized,
    AuditAuthorization,
    RevisionOpened,
    ProtectedReapprovalApproved,
    ProtectedReapprovalFeedback,
    Revoked,
}

impl EventKind {
    pub const ALL: [Self; 26] = [
        Self::SeedCommitted,
        Self::SourceClaimLineageRegistered,
        Self::SourceClaimLineageResolutionRecorded,
        Self::SourceModelMismatchResolved,
        Self::AdvanceGateApproved,
        Self::AdvanceGateFeedback,
        Self::ProofChecked,
        Self::WitnessRefutationChecked,
        Self::UnrestrictedVerdictRecorded,
        Self::SourceValidationClassified,
        Self::SourceValidationAttemptRecorded,
        Self::SourceValidationOutcomeRecorded,
        Self::ReflectionValidationResultRecorded,
        Self::SourceValidationHistorySummarized,
        Self::ApprovedProfileSelected,
        Self::ConditionalStatementGenerated,
        Self::QualificationObligationsChecked,
        Self::NoQualifiedResultEstablished,
        Self::ApplicabilityClassified,
        Self::ExternalClaimRowsGenerated,
        Self::PackageAuthorized,
        Self::AuditAuthorization,
        Self::RevisionOpened,
        Self::ProtectedReapprovalApproved,
        Self::ProtectedReapprovalFeedback,
        Self::Revoked,
    ];
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ActorRole {
    Kernel,
    Reviewer,
    AuditAuthority,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorAuthenticationMethod {
    KernelInternal,
    AuthenticatedGateReceipt,
    AuditSignatureV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceMode {
    EmbeddedSelfDigest,
    FullRecordHash,
    RawBytesHash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActorIdentityPolicy {
    TrellisKernel,
    PayloadReviewerIdentity,
    PayloadAuditIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationRequirement {
    None,
    RevisionLane,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalHead {
    pub journal_id: String,
    pub sequence_number: u64,
    pub event_hash: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEventPayload {
    pub schema: String,
    pub event_kind: EventKind,
    pub subject_id: String,
    pub subject_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_schema_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_schema_sha256: Option<Sha256Digest>,
    pub subject_hash_tag: String,
    pub subject_sha256: Sha256Digest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_identity: Option<String>,
    pub journal_predecessor_sha256: Sha256Digest,
    pub payload_sha256: Sha256Digest,
}

impl JournalEventPayload {
    pub fn compute_digest(&self) -> Result<Sha256Digest, TrustError> {
        let value = serde_json::to_value(self).map_err(|error| {
            TrustError::new("payload_serialization_failed", error.to_string())
        })?;
        self_digest(
            DomainTag::JournalEventPayload,
            &value,
            "payload_sha256",
        )
    }

    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.payload_sha256 = self.compute_digest()?;
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEvent {
    pub journal_schema: String,
    pub journal_id: String,
    pub run_id: String,
    pub sequence_number: u64,
    pub transaction_id: String,
    pub previous_event_hash: Sha256Digest,
    pub event_kind: EventKind,
    pub payload_schema_id: String,
    pub payload_schema_sha256: Sha256Digest,
    pub payload_hash: Sha256Digest,
    pub semantic_root_before: Sha256Digest,
    pub semantic_root_after: Sha256Digest,
    pub derived_result_root_before: Sha256Digest,
    pub derived_result_root_after: Sha256Digest,
    pub actor_role: ActorRole,
    pub actor_identity: String,
    pub actor_authentication_method: ActorAuthenticationMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_authentication_receipt_sha256: Option<Sha256Digest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_event_hash: Option<Sha256Digest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_lane_id: Option<String>,
    pub event_hash: Sha256Digest,
}

impl JournalEvent {
    pub fn compute_digest(&self) -> Result<Sha256Digest, TrustError> {
        let value = serde_json::to_value(self).map_err(|error| {
            TrustError::new("event_serialization_failed", error.to_string())
        })?;
        self_digest(DomainTag::JournalEvent, &value, "event_hash")
    }

    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.event_hash = self.compute_digest()?;
        Ok(self)
    }

    pub fn head(&self) -> JournalHead {
        JournalHead {
            journal_id: self.journal_id.clone(),
            sequence_number: self.sequence_number,
            event_hash: self.event_hash,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalPolicyDocument {
    schema: String,
    protocol_id: String,
    journal_event_schema_sha256: Sha256Digest,
    journal_payload_schema_sha256: Sha256Digest,
    authentication_key_manifest_schema_sha256: Sha256Digest,
    authentication_receipt_schema_sha256: Sha256Digest,
    entries: Vec<JournalPolicyEntry>,
    policy_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalPolicyEntry {
    pub event_kind: EventKind,
    pub envelope_actor_role: ActorRole,
    pub actor_identity_policy: ActorIdentityPolicy,
    pub authentication_method: ActorAuthenticationMethod,
    pub payload_schema_id: String,
    pub payload_schema_sha256: Sha256Digest,
    pub payload_hash_tag: String,
    pub payload_reference_mode: ReferenceMode,
    pub allowed_subject_hash_tags: Vec<String>,
    pub subject_reference_mode: ReferenceMode,
    pub authorization_requirement: AuthorizationRequirement,
}

#[derive(Clone, Debug)]
pub struct JournalPolicy {
    digest: Sha256Digest,
    entries: BTreeMap<EventKind, JournalPolicyEntry>,
}

impl JournalPolicy {
    pub fn embedded_v1(registry: &SchemaRegistry) -> Result<Self, TrustError> {
        let value: Value = serde_json::from_str(include_str!("schemas/JOURNAL_EVENT_POLICY.v1.json"))
        .map_err(|error| TrustError::new("journal_policy_parse_failed", error.to_string()))?;
        registry.validate_record(&value)?;
        let digest = verify_self_digest(
            DomainTag::JournalEventPolicy,
            &value,
            "policy_sha256",
        )?;
        let document: JournalPolicyDocument = serde_json::from_value(value).map_err(|error| {
            TrustError::new("journal_policy_decode_failed", error.to_string())
        })?;
        if document.schema != "trellis-journal-event-policy/v1"
            || document.protocol_id != "trellis-trust-v1"
        {
            return Err(TrustError::new(
                "journal_policy_identity_mismatch",
                "embedded journal policy has the wrong protocol identity",
            ));
        }
        if document.policy_sha256 != digest {
            return Err(TrustError::new(
                "journal_policy_digest_mismatch",
                "decoded policy digest differs from verified identity",
            ));
        }
        for (field, declared, schema_id) in [
            (
                "journal_event_schema_sha256",
                document.journal_event_schema_sha256,
                "trellis://schemas/journal-event/v1",
            ),
            (
                "journal_payload_schema_sha256",
                document.journal_payload_schema_sha256,
                "trellis://schemas/journal-event-payload/v1",
            ),
            (
                "authentication_key_manifest_schema_sha256",
                document.authentication_key_manifest_schema_sha256,
                "trellis://schemas/actor-authentication-key-manifest/v1",
            ),
            (
                "authentication_receipt_schema_sha256",
                document.authentication_receipt_schema_sha256,
                "trellis://schemas/actor-authentication-receipt/v1",
            ),
        ] {
            let actual = registry.schema_sha256(schema_id)?;
            if declared != actual {
                return Err(TrustError::new(
                    "journal_policy_schema_hash_mismatch",
                    format!("{field}: declared {declared}, embedded {actual}"),
                ));
            }
        }
        let mut entries = BTreeMap::new();
        for entry in document.entries {
            if DomainTag::parse_registered(&entry.payload_hash_tag)?
                != DomainTag::JournalEventPayload
                || entry.payload_reference_mode != ReferenceMode::EmbeddedSelfDigest
                || entry.payload_schema_id != "trellis://schemas/journal-event-payload/v1"
                || entry.payload_schema_sha256
                    != registry.schema_sha256("trellis://schemas/journal-event-payload/v1")?
            {
                return Err(TrustError::new(
                    "invalid_journal_policy_entry",
                    format!("payload policy is invalid for {:?}", entry.event_kind),
                ));
            }
            for tag in &entry.allowed_subject_hash_tags {
                DomainTag::parse_registered(tag)?;
            }
            let kind = entry.event_kind;
            if entries.insert(kind, entry).is_some() {
                return Err(TrustError::new(
                    "duplicate_journal_policy_event",
                    format!("duplicate policy for {kind:?}"),
                ));
            }
        }
        if entries.len() != EventKind::ALL.len()
            || EventKind::ALL.iter().any(|kind| !entries.contains_key(kind))
        {
            return Err(TrustError::new(
                "journal_policy_not_closed",
                "journal policy must contain every event kind exactly once",
            ));
        }
        Ok(Self { digest, entries })
    }

    pub fn digest(&self) -> Sha256Digest {
        self.digest
    }

    pub fn entry(&self, event_kind: EventKind) -> &JournalPolicyEntry {
        self.entries
            .get(&event_kind)
            .expect("embedded policy is checked as closed")
    }

    pub fn validate_subject(
        &self,
        event_kind: EventKind,
        subject: &Subject,
    ) -> Result<(), TrustError> {
        let entry = self.entry(event_kind);
        if !entry
            .allowed_subject_hash_tags
            .iter()
            .any(|tag| tag == subject.domain_tag().as_str())
        {
            return Err(TrustError::new(
                "journal_subject_tag_not_allowed",
                format!(
                    "{} is not permitted for {event_kind:?}",
                    subject.domain_tag().as_str()
                ),
            ));
        }
        if subject.reference_mode() != entry.subject_reference_mode {
            return Err(TrustError::new(
                "journal_subject_reference_mode_mismatch",
                format!(
                    "subject uses {:?}, policy requires {:?}",
                    subject.reference_mode(),
                    entry.subject_reference_mode
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_policy_is_closed_and_hash_bound_to_embedded_schemas() {
        let registry = SchemaRegistry::v1().unwrap();
        let policy = JournalPolicy::embedded_v1(&registry).unwrap();
        assert_ne!(policy.digest(), Sha256Digest::ZERO);
        assert_eq!(policy.entries.len(), 26);
    }

    #[test]
    fn registration_fixture_event_hashes_round_trip() {
        let fixture: Value = serde_json::from_str(include_str!("schemas/REGISTRATION_HASH_DAG_FIXTURES.v1.json"))
        .unwrap();
        for item in fixture["events"].as_array().unwrap() {
            let event: JournalEvent = serde_json::from_value(item["event"].clone()).unwrap();
            assert_eq!(event.compute_digest().unwrap(), event.event_hash);
            let payload: JournalEventPayload =
                serde_json::from_value(item["payload"].clone()).unwrap();
            assert_eq!(payload.compute_digest().unwrap(), payload.payload_sha256);
            assert_eq!(event.payload_hash, payload.payload_sha256);
            assert_eq!(event.event_kind, payload.event_kind);
            assert_eq!(event.previous_event_hash, payload.journal_predecessor_sha256);
        }
    }

    #[test]
    fn policy_rejects_full_hash_where_embedded_digest_is_required() {
        let registry = SchemaRegistry::v1().unwrap();
        let policy = JournalPolicy::embedded_v1(&registry).unwrap();
        let approval = AuthoritativeRecord::parse(
            &registry,
            serde_json::json!({
                "schema":"trellis-human-approval/v1",
                "authored_semantic_root":"00".repeat(32),
                "approved_evidence_tool_input_root":"11".repeat(32),
                "gate_presentation_sha256":"22".repeat(32),
                "reviewer_identity":"r",
                "approved_journal_head":{
                    "journal_id":"j","sequence_number":1,"event_hash":"33".repeat(32)
                }
            }),
        )
        .unwrap();
        let subject = Subject::CanonicalRecord(approval);
        policy
            .validate_subject(EventKind::AdvanceGateApproved, &subject)
            .unwrap();
        assert!(policy
            .validate_subject(EventKind::SeedCommitted, &subject)
            .is_err());
    }
}

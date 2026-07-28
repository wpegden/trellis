//! The sole public writer for post-gate semantic trust events.

use super::canonical::{
    canonical_json_value, parse_json_strict, self_digest, tagged_hash, DomainTag, Sha256Digest,
    TrustError,
};
use super::basis::validate_independent_basis;
use super::closure::{
    ResolvedEvidenceLeaf, VerifiedEvidenceClosure, VerifiedSeedDefinitionClosure,
};
use super::journal::{AppendRequest, JournalActor, TrustJournal};
use super::package::{
    AuthorizedPackage, PackageAuthorizationRequest, PackageReadiness,
};
use super::qualification::{
    evaluate_qualification, validate_qualification_prerequisites, QualificationEvidence,
    QualificationPrerequisites, QualifiedResult,
};
use super::records::{
    AuthoritativeRecord, EventKind, JournalEvent, JournalEventPayload, JournalHead, Subject,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use super::schema::SchemaRegistry;
use super::source_validation::{
    route_qualified_recovery, validate_attempt, validate_outcome, HistoryFacts,
    QualificationRoute, SourceValidationContractView, SourceValidationMethod, ValidatedOutcome,
    ValidationStatus,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub struct SourceToolInvocation<'a> {
    pub tool_logical_id: &'a str,
    pub runner_path: &'a Path,
    pub command_id: &'a str,
    pub working_directory: &'a Path,
    pub environment: &'a BTreeMap<String, String>,
    pub input: &'a Value,
    pub limits: super::execution::ExecutionLimits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedSourceExecution {
    pub record_sha256: Sha256Digest,
    pub event_hash: Sha256Digest,
    pub execution_receipt_sha256: Sha256Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedReflectionExecution {
    pub result_sha256: Sha256Digest,
    pub event_hash: Sha256Digest,
    pub checker_execution_receipt_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedTargetContract {
    pub target_id: String,
    pub contract_sha256: Sha256Digest,
    pub method: SourceValidationMethod,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedQualificationProfile {
    pub profile: AuthoritativeRecord,
    pub independent_basis: AuthoritativeRecord,
    pub conditional_theorem_candidate: AuthoritativeRecord,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CampaignTargetStatus {
    AwaitingFormalResult,
    PositiveAwaitingClaims,
    NonWitnessNegative {
        negative_proof_sha256: Sha256Digest,
        history_summary_sha256: Option<Sha256Digest>,
        source_outcome_recorded: bool,
        terminal_recorded: bool,
        method: SourceValidationMethod,
    },
    WitnessNegative {
        formal_refutation_sha256: Sha256Digest,
        history_summary_sha256: Option<Sha256Digest>,
        source_validation_recorded: bool,
        terminal_recorded: bool,
        qualification_selection: Option<QualificationSelection>,
        conditional_statement: Option<RecordedConditionalStatement>,
        qualification_bundle_sha256: Option<Sha256Digest>,
        applicability_recorded: bool,
        method: SourceValidationMethod,
    },
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedModelRefutation {
    pub witness_refutation_event_hash: Sha256Digest,
    pub unrestricted_verdict_event_hash: Sha256Digest,
    pub formal_refutation_sha256: Sha256Digest,
}

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

/// A checked proof of the negation of a frozen target that does not claim to
/// expose a source-language counterexample witness.  This is the general
/// negative result carrier; witness-specific refutations use
/// `record_model_refutation` and are the only results eligible for source
/// execution or qualified recovery.
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
    pub proof_event_hash: Sha256Digest,
    pub proof_subject_sha256: Sha256Digest,
    pub unrestricted_verdict_event_hash: Sha256Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedNegativeProof {
    pub proof_event_hash: Sha256Digest,
    pub proof_subject_sha256: Sha256Digest,
    pub unrestricted_verdict_event_hash: Sha256Digest,
}

#[derive(Clone, Debug)]
struct PositiveProofState {
    target_id: String,
    target_statement_sha256: Sha256Digest,
    proof_subject_sha256: Sha256Digest,
    proof_event_hash: Sha256Digest,
    unrestricted_verdict_event_hash: Sha256Digest,
}

#[derive(Clone, Debug)]
struct NegativeProofState {
    target_id: String,
    target_statement_sha256: Sha256Digest,
    proof_subject_sha256: Sha256Digest,
    proof_event_hash: Sha256Digest,
    unrestricted_verdict_event_hash: Sha256Digest,
    /// Exact journal subject bytes for the checked theorem-level refutation.
    /// Checked-reflection contracts consume this carrier directly; it is not
    /// reinterpreted as a finite source witness.
    proof_envelope: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QualificationSelection {
    pub profile_sha256: Sha256Digest,
    pub profile_selection_event_hash: Sha256Digest,
    pub history_summary_sha256: Sha256Digest,
    pub history_summary_event_hash: Sha256Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedConditionalStatement {
    pub statement_sha256: Sha256Digest,
    pub statement_event_hash: Sha256Digest,
    pub profile_selection_event_hash: Sha256Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordedQualification {
    pub qualification_bundle_sha256: Sha256Digest,
    pub qualification_event_hash: Sha256Digest,
    pub conditional_statement_sha256: Sha256Digest,
}

#[derive(Clone, Debug)]
struct FailedQualificationAttempt {
    pub independent_basis: AuthoritativeRecord,
    pub witness_resource_demand: AuthoritativeRecord,
    pub source_witness_admissibility: AuthoritativeRecord,
    pub profile: AuthoritativeRecord,
    pub checked_failure_execution_receipt: Value,
    pub checked_failure_receipt_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedQualificationAttempt {
    Qualified(RecordedQualification, QualifiedResult),
    NotEstablished { event_hash: Sha256Digest },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedExternalClaims {
    pub target_id: String,
    pub rendered: String,
    pub event_hash: Sha256Digest,
}

#[derive(Clone, Debug)]
struct SelectionState {
    selection: QualificationSelection,
    target_id: String,
    evidence: Option<QualificationInputRecords>,
}

#[derive(Clone, Debug)]
struct ConditionalState {
    record: RecordedConditionalStatement,
    target_id: String,
    evidence: QualificationInputRecords,
    statement_utf8: String,
}

#[derive(Clone, Debug)]
struct QualificationInputRecords {
    formal_refutation_sha256: Sha256Digest,
    independent_basis: AuthoritativeRecord,
    witness_resource_demand: AuthoritativeRecord,
    source_witness_admissibility: AuthoritativeRecord,
    profile: AuthoritativeRecord,
    conditional_theorem_candidate: AuthoritativeRecord,
    conditional_proof_receipt: Value,
}

#[derive(Clone, Debug)]
struct NoQualifiedState {
    result_kind: NegativeResultKind,
    negative_result_sha256: Sha256Digest,
    history_summary_sha256: Sha256Digest,
    event_hash: Sha256Digest,
    conditional_attempted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NegativeResultKind {
    FormalWitnessRefutation,
    CheckedNegativeProof,
}

#[derive(Clone, Debug)]
struct ExternalClaimState {
    terminal_event_hash: Sha256Digest,
    event_hash: Sha256Digest,
    event_sequence: u64,
    envelope: Value,
}

#[derive(Clone, Debug)]
struct OutcomeEntry {
    event: OutcomeEventRef,
    validated: ValidatedOutcome,
}

#[derive(Clone, Copy, Debug)]
struct OutcomeEventRef {
    sequence_number: u64,
    event_hash: Sha256Digest,
}

impl From<&JournalEvent> for OutcomeEventRef {
    fn from(event: &JournalEvent) -> Self {
        Self {
            sequence_number: event.sequence_number,
            event_hash: event.event_hash,
        }
    }
}

impl From<&JournalHead> for OutcomeEventRef {
    fn from(head: &JournalHead) -> Self {
        Self {
            sequence_number: head.sequence_number,
            event_hash: head.event_hash,
        }
    }
}

/// Register the exact initial lineage records already bound by the seed.  The
/// events are index receipts only and do not change the authored root.
pub fn register_seed_lineages(
    journal: &mut TrustJournal,
    closure: &VerifiedSeedDefinitionClosure,
) -> Result<(), TrustError> {
    let bundles = journal.committed_bundle_values()?;
    let (prefix_through_sequence, transaction_epoch) = match journal.current_approval() {
        None => (1, "seed".to_owned()),
        Some(approval) => {
            let revision = approval.revision_closure.ok_or_else(|| {
                TrustError::new(
                    "seed_lineage_registration_after_routine_gate",
                    "routine seed registrations must precede the one human gate",
                )
            })?;
            if revision.seed_manifest_sha256 != closure.seed_manifest_sha256
                || revision.seed_definition_bundle_sha256 != closure.bundle_sha256
            {
                return Err(TrustError::new(
                    "revision_lineage_closure_mismatch",
                    "lineage registration closure differs from protected reapproval",
                ));
            }
            let sequence = bundles
                .iter()
                .find_map(|bundle| {
                    let event: JournalEvent =
                        serde_json::from_value(bundle.get("event")?.clone()).ok()?;
                    (event.event_hash == approval.event_hash).then_some(event.sequence_number)
                })
                .ok_or_else(|| {
                    TrustError::new(
                        "revision_approval_event_missing",
                        "current protected approval is absent from its journal",
                    )
                })?;
            (sequence, format!("revision:{}", approval.event_hash))
        }
    };
    for bundle in bundles
        .iter()
        .filter(|bundle| {
            bundle
                .get("event")
                .and_then(|event| event.get("sequence_number"))
                .and_then(Value::as_u64)
                .is_some_and(|sequence| sequence > prefix_through_sequence)
        })
    {
        let event: JournalEvent = serde_json::from_value(bundle["event"].clone()).map_err(
            |error| TrustError::new("journal_event_decode_failed", error.to_string()),
        )?;
        if event.event_kind != EventKind::SourceClaimLineageRegistered {
            return Err(TrustError::new(
                "non_registration_event_in_seed_prefix",
                "fresh seed registration may not append after another event kind",
            ));
        }
        let value = bundle.get("subject_json").cloned().ok_or_else(|| {
            TrustError::new("lineage_subject_missing", "registration lacks subject JSON")
        })?;
        let record = AuthoritativeRecord::parse(&SchemaRegistry::v1()?, value)?;
        if closure.records_by_digest.get(&record.digest()) != Some(&record) {
            return Err(TrustError::new(
                "existing_lineage_registration_not_seed_frozen",
                "existing registration subject is not an exact seed lineage",
            ));
        }
        let lineage_id = string_field(record.value(), "lineage_id")?;
        if event.transaction_id
            != format!("{transaction_epoch}-lineage-registration:{lineage_id}")
        {
            return Err(TrustError::new(
                "existing_lineage_registration_transaction_mismatch",
                "existing seed registration has a non-deterministic transaction ID",
            ));
        }
    }
    let mut lineages: Vec<_> = closure
        .records_by_digest
        .values()
        .filter(|record| record.contract().record_schema == "trellis-source-claim-lineage/v1")
        .cloned()
        .collect();
    lineages.sort_by(|left, right| {
        string_field(left.value(), "lineage_id")
            .unwrap_or("")
            .as_bytes()
            .cmp(string_field(right.value(), "lineage_id").unwrap_or("").as_bytes())
    });
    let mut targets = BTreeSet::new();
    for lineage in lineages {
        if string_field(lineage.value(), "lineage_change_kind")? != "initial_seed"
            || string_field(lineage.value(), "registration_epoch")? != "seed"
            || string_field(lineage.value(), "registration_authority")? != "seed_contract_v1"
        {
            return Err(TrustError::new(
                "initial_lineage_not_seed_frozen",
                "initial lineage has revision authority or a non-seed epoch",
            ));
        }
        let target = string_field(lineage.value(), "target_id")?;
        if !targets.insert(target.to_owned()) {
            return Err(TrustError::new(
                "duplicate_initial_target_lineage",
                format!("multiple initial lineages for target {target}"),
            ));
        }
        let lineage_id = string_field(lineage.value(), "lineage_id")?.to_owned();
        let transaction_id = format!("{transaction_epoch}-lineage-registration:{lineage_id}");
        if journal
            .committed_transaction_event_hash(&transaction_id)
            .is_some()
        {
            continue;
        }
        journal.append(AppendRequest {
            transaction_id,
            event_kind: EventKind::SourceClaimLineageRegistered,
            subject_id: lineage_id,
            subject: Subject::CanonicalRecord(lineage),
            actor: JournalActor::Kernel,
            semantic_root_after: journal.semantic_root(),
            derived_result_root_after: journal.derived_result_root(),
            authorization: None,
        })?;
    }
    Ok(())
}

pub struct TrustDerivationPipeline {
    journal: TrustJournal,
    registry: SchemaRegistry,
    seed: VerifiedSeedDefinitionClosure,
    seed_authored_semantic_root: Sha256Digest,
    evidence: VerifiedEvidenceClosure,
    lineages: BTreeMap<Sha256Digest, AuthoritativeRecord>,
    lineage_registration: BTreeMap<Sha256Digest, JournalEvent>,
    contracts: BTreeMap<Sha256Digest, (AuthoritativeRecord, SourceValidationContractView)>,
    formal_refutations: BTreeMap<Sha256Digest, AuthoritativeRecord>,
    unrestricted_verdicts: BTreeMap<Sha256Digest, Sha256Digest>,
    positive_proofs: BTreeMap<String, PositiveProofState>,
    negative_proofs: BTreeMap<String, NegativeProofState>,
    reflection_results: BTreeMap<Sha256Digest, (AuthoritativeRecord, Sha256Digest)>,
    attempts: BTreeMap<Sha256Digest, AuthoritativeRecord>,
    outcomes_by_contract: BTreeMap<Sha256Digest, Vec<OutcomeEntry>>,
    histories: BTreeMap<Sha256Digest, (AuthoritativeRecord, Sha256Digest, HistoryFacts)>,
    active_selections: BTreeMap<Sha256Digest, SelectionState>,
    active_conditional_statements: BTreeMap<Sha256Digest, ConditionalState>,
    qualifications: BTreeMap<Sha256Digest, (AuthoritativeRecord, Sha256Digest)>,
    applicabilities: BTreeMap<Sha256Digest, (AuthoritativeRecord, Sha256Digest)>,
    no_qualified_results: BTreeMap<String, NoQualifiedState>,
    external_claim_rows: BTreeMap<String, ExternalClaimState>,
}

impl TrustDerivationPipeline {
    pub fn open(
        journal: TrustJournal,
        seed: VerifiedSeedDefinitionClosure,
        evidence: VerifiedEvidenceClosure,
    ) -> Result<Self, TrustError> {
        let seed_bundle = journal.committed_bundle_value(1)?;
        let journal_seed = AuthoritativeRecord::parse(
            &SchemaRegistry::v1()?,
            seed_bundle
                .get("subject_json")
                .cloned()
                .ok_or_else(|| {
                    TrustError::new(
                        "pipeline_journal_seed_missing",
                        "sequence one lacks its canonical seed manifest",
                    )
                })?,
        )?;
        let approval = journal.current_approval();
        let seed_authored_semantic_root = if let Some(revision) =
            approval.as_ref().and_then(|approval| approval.revision_closure)
        {
            if revision.seed_manifest_sha256 != seed.seed_manifest_sha256
                || revision.seed_definition_bundle_sha256 != seed.bundle_sha256
                || revision.evidence_manifest_sha256 != evidence.manifest_sha256
                || revision.evidence_tool_input_root != evidence.evidence_tool_input_root
            {
                return Err(TrustError::new(
                    "pipeline_revision_closure_mismatch",
                    "loaded seed/evidence closure differs from the protected reapproval",
                ));
            }
            revision.authored_semantic_root
        } else {
            if journal_seed.digest() != seed.seed_manifest_sha256 {
                return Err(TrustError::new(
                    "pipeline_seed_closure_wrong_journal",
                    "verified definition bundle belongs to a different seed manifest",
                ));
            }
            digest_field(journal_seed.value(), "authored_semantic_root")?
        };
        if let Some(approval) = approval {
            if approval.authored_semantic_root != seed_authored_semantic_root {
                return Err(TrustError::new(
                    "pipeline_semantic_closure_not_approved",
                    "loaded seed closure differs from the journal approval",
                ));
            }
            if approval.approved_evidence_tool_input_root != evidence.evidence_tool_input_root {
                return Err(TrustError::new(
                    "pipeline_evidence_closure_not_approved",
                    "verified evidence closure differs from the journal approval",
                ));
            }
        }
        let mut pipeline = Self {
            journal,
            registry: SchemaRegistry::v1()?,
            seed,
            seed_authored_semantic_root,
            evidence,
            lineages: BTreeMap::new(),
            lineage_registration: BTreeMap::new(),
            contracts: BTreeMap::new(),
            formal_refutations: BTreeMap::new(),
            unrestricted_verdicts: BTreeMap::new(),
            positive_proofs: BTreeMap::new(),
            negative_proofs: BTreeMap::new(),
            reflection_results: BTreeMap::new(),
            attempts: BTreeMap::new(),
            outcomes_by_contract: BTreeMap::new(),
            histories: BTreeMap::new(),
            active_selections: BTreeMap::new(),
            active_conditional_statements: BTreeMap::new(),
            qualifications: BTreeMap::new(),
            applicabilities: BTreeMap::new(),
            no_qualified_results: BTreeMap::new(),
            external_claim_rows: BTreeMap::new(),
        };
        pipeline.rebuild_from_journal()?;
        Ok(pipeline)
    }

    pub fn journal(&self) -> &TrustJournal {
        &self.journal
    }

    pub fn target_has_formal_result(&self, target_id: &str) -> bool {
        self.positive_proofs.contains_key(target_id)
            || self.negative_proofs.contains_key(target_id)
            || self.formal_refutations.values().any(|formal| {
                formal.value().get("target_id").and_then(Value::as_str) == Some(target_id)
            })
    }

    /// Replay-derived campaign progress used by the runtime to resume at the
    /// first uncommitted semantic transition after a crash.  The journal, not
    /// the runtime checkpoint, remains the authority for every flag here.
    pub fn campaign_target_status(
        &self,
        target_id: &str,
    ) -> Result<CampaignTargetStatus, TrustError> {
        if self.external_claim_rows.contains_key(target_id) {
            return Ok(CampaignTargetStatus::Complete);
        }
        if self.positive_proofs.contains_key(target_id) {
            return Ok(CampaignTargetStatus::PositiveAwaitingClaims);
        }
        let contract = self.contracts.values().find_map(|(_, contract)| {
            (contract.target_id == target_id).then_some(contract)
        }).ok_or_else(|| {
            TrustError::new(
                "campaign_target_contract_unclassified",
                "campaign target has no classified source-validation contract",
            )
        })?;
        let history = self
            .histories
            .iter()
            .filter(|(_, (record, _, _))| {
                record.value().get("target_id").and_then(Value::as_str) == Some(target_id)
            })
            .max_by_key(|(_, (record, _, _))| {
                record
                    .value()
                    .get("covered_through_sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            })
            .map(|(digest, _)| *digest);
        let source_outcome_recorded = self
            .outcomes_by_contract
            .get(&contract.digest)
            .is_some_and(|entries| !entries.is_empty());
        let qualification = self.qualifications.iter().find(|(_, (record, _))| {
                record.value().get("target_id").and_then(Value::as_str) == Some(target_id)
            });
        let qualification_bundle_sha256 = qualification.map(|(digest, _)| *digest);
        let applicability_recorded = qualification_bundle_sha256
            .is_some_and(|digest| self.applicabilities.contains_key(&digest));
        let terminal_recorded = self.no_qualified_results.contains_key(target_id)
            || (qualification_bundle_sha256.is_some() && applicability_recorded);
        if let Some(proof) = self.negative_proofs.get(target_id) {
            let source_validation_recorded = match contract.method {
                SourceValidationMethod::CheckedRefutationReflectionV1 => self
                    .reflection_results
                    .values()
                    .any(|(record, _)| {
                        record
                            .value()
                            .get("validation_contract_sha256")
                            .and_then(Value::as_str)
                            .and_then(|item| item.parse::<Sha256Digest>().ok())
                            == Some(contract.digest)
                    }),
                SourceValidationMethod::NotDefinedForClaimShapeV1 => {
                    source_outcome_recorded
                }
                // A contract that promises exact source execution must not
                // silently downgrade a witness-free theorem proof.
                SourceValidationMethod::ExactRustExecutionV1 => false,
            };
            return Ok(CampaignTargetStatus::NonWitnessNegative {
                negative_proof_sha256: proof.proof_subject_sha256,
                history_summary_sha256: history,
                source_outcome_recorded: source_validation_recorded,
                terminal_recorded,
                method: contract.method,
            });
        }
        let formals: Vec<_> = self
            .formal_refutations
            .iter()
            .filter(|(_, formal)| {
                formal.value().get("target_id").and_then(Value::as_str) == Some(target_id)
            })
            .collect();
        if formals.len() > 1 {
            return Err(TrustError::new(
                "campaign_target_multiple_formal_refutations",
                "campaign target has more than one witness refutation",
            ));
        }
        if let Some((digest, _)) = formals.first() {
            let source_validation_recorded = match contract.method {
                SourceValidationMethod::ExactRustExecutionV1 => source_outcome_recorded,
                SourceValidationMethod::CheckedRefutationReflectionV1 => self
                    .reflection_results
                    .values()
                    .any(|(record, _)| {
                        record
                            .value()
                            .get("validation_contract_sha256")
                            .and_then(Value::as_str)
                            .and_then(|item| item.parse::<Sha256Digest>().ok())
                            == Some(contract.digest)
                    }),
                SourceValidationMethod::NotDefinedForClaimShapeV1 => source_outcome_recorded,
            };
            return Ok(CampaignTargetStatus::WitnessNegative {
                formal_refutation_sha256: **digest,
                history_summary_sha256: history,
                source_validation_recorded,
                terminal_recorded,
                qualification_selection: self
                    .active_selections
                    .values()
                    .find(|state| state.target_id == target_id)
                    .map(|state| state.selection),
                conditional_statement: self
                    .active_conditional_statements
                    .values()
                    .find(|state| state.target_id == target_id)
                    .map(|state| state.record),
                qualification_bundle_sha256,
                applicability_recorded,
                method: contract.method,
            });
        }
        Ok(CampaignTargetStatus::AwaitingFormalResult)
    }

    pub fn source_contract_value(
        &self,
        contract_sha256: Sha256Digest,
    ) -> Result<Value, TrustError> {
        self.contracts
            .get(&contract_sha256)
            .map(|(record, _)| record.value().clone())
            .ok_or_else(|| {
                TrustError::new(
                    "campaign_source_contract_missing",
                    "source contract is not classified",
                )
            })
    }

    pub fn formal_refutation_value(
        &self,
        formal_refutation_sha256: Sha256Digest,
    ) -> Result<Value, TrustError> {
        self.formal_refutations
            .get(&formal_refutation_sha256)
            .map(|record| record.value().clone())
            .ok_or_else(|| {
                TrustError::new(
                    "campaign_formal_refutation_missing",
                    "formal witness refutation is not journaled",
                )
            })
    }

    /// Return the exact checked negative carrier named by a campaign status.
    /// This deliberately accepts both witness-specific formal refutations and
    /// theorem-level negative proofs without pretending the latter contains a
    /// source-executable witness.
    pub fn model_negative_evidence_value(
        &self,
        negative_result_sha256: Sha256Digest,
    ) -> Result<Value, TrustError> {
        if let Some(formal) = self.formal_refutations.get(&negative_result_sha256) {
            return Ok(formal.value().clone());
        }
        self.negative_proofs
            .values()
            .find(|proof| proof.proof_subject_sha256 == negative_result_sha256)
            .map(|proof| proof.proof_envelope.clone())
            .ok_or_else(|| {
                TrustError::new(
                    "campaign_model_negative_evidence_missing",
                    "negative result does not name a checked model refutation carrier",
                )
            })
    }

    fn checked_model_negative_binding(
        &self,
        negative_result_sha256: Sha256Digest,
    ) -> Result<(String, Sha256Digest, Sha256Digest), TrustError> {
        if let Some(formal) = self.formal_refutations.get(&negative_result_sha256) {
            return Ok((
                string_field(formal.value(), "target_id")?.to_owned(),
                digest_field(formal.value(), "target_statement_sha256")?,
                digest_field(formal.value(), "checked_not_t_proof_sha256")?,
            ));
        }
        let proof = self
            .negative_proofs
            .values()
            .find(|proof| proof.proof_subject_sha256 == negative_result_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "campaign_model_negative_evidence_missing",
                    "negative result does not name a checked model refutation carrier",
                )
            })?;
        if proof.unrestricted_verdict_event_hash == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "campaign_model_negative_verdict_missing",
                "checked theorem-level refutation lacks its unrestricted verdict",
            ));
        }
        Ok((
            proof.target_id.clone(),
            proof.target_statement_sha256,
            digest_field(&proof.proof_envelope, "checked_not_proof_artifact_sha256")?,
        ))
    }

    pub fn source_attempt_value(
        &self,
        attempt_sha256: Sha256Digest,
    ) -> Result<Value, TrustError> {
        self.attempts
            .get(&attempt_sha256)
            .map(|record| record.value().clone())
            .ok_or_else(|| {
                TrustError::new(
                    "campaign_source_attempt_missing",
                    "source attempt is not journaled",
                )
            })
    }

    pub fn source_history_value(
        &self,
        history_sha256: Sha256Digest,
    ) -> Result<Value, TrustError> {
        self.histories
            .get(&history_sha256)
            .map(|(record, _, _)| record.value().clone())
            .ok_or_else(|| {
                TrustError::new(
                    "campaign_source_history_missing",
                    "source-validation history is not journaled",
                )
            })
    }

    pub fn qualification_context_value(
        &self,
        selection: QualificationSelection,
    ) -> Result<Value, TrustError> {
        let matching: Vec<_> = self
            .active_conditional_statements
            .values()
            .filter(|conditional| {
                conditional.record.profile_selection_event_hash
                    == selection.profile_selection_event_hash
            })
            .collect();
        if matching.len() > 1 {
            return Err(TrustError::new(
                "qualification_conditional_ambiguous",
                "selection has more than one active conditional statement",
            ));
        }
        self.qualification_context_value_for(selection, matching.first().copied())
    }

    fn qualification_context_value_for(
        &self,
        selection: QualificationSelection,
        conditional: Option<&ConditionalState>,
    ) -> Result<Value, TrustError> {
        let state = self
            .active_selections
            .get(&selection.profile_selection_event_hash)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_selection_not_active",
                    "qualification context names no active selection",
                )
            })?;
        if state.selection != selection {
            return Err(TrustError::new(
                "qualification_selection_mismatch",
                "qualification context selection token differs from replay state",
            ));
        }
        let evidence = state.evidence.as_ref().ok_or_else(|| {
            TrustError::new(
                "qualification_selection_evidence_missing",
                "qualification context evidence has not been restored",
            )
        })?;
        self.qualification_context_from_evidence(selection, evidence, conditional)
    }

    fn qualification_context_from_evidence(
        &self,
        selection: QualificationSelection,
        evidence: &QualificationInputRecords,
        conditional: Option<&ConditionalState>,
    ) -> Result<Value, TrustError> {
        let formal = self
            .formal_refutations
            .get(&evidence.formal_refutation_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_context_formal_missing",
                    "qualification context formal refutation is absent",
                )
            })?;
        let history = self
            .histories
            .get(&selection.history_summary_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_context_history_missing",
                    "qualification context history is absent",
                )
            })?;
        if conditional.is_some_and(|conditional| {
            conditional.record.profile_selection_event_hash
                != selection.profile_selection_event_hash
        }) {
            return Err(TrustError::new(
                "qualification_conditional_selection_mismatch",
                "conditional statement belongs to another selection",
            ));
        }
        Ok(serde_json::json!({
            "schema": "trellis-qualification-checker-input/v1",
            "selection": selection,
            "formal_refutation_sha256": evidence.formal_refutation_sha256,
            "formal_refutation": formal.value(),
            "history_summary_sha256": selection.history_summary_sha256,
            "history_summary": history.0.value(),
            "independent_basis": evidence.independent_basis.value(),
            "witness_resource_demand": evidence.witness_resource_demand.value(),
            "source_witness_admissibility": evidence.source_witness_admissibility.value(),
            "profile": evidence.profile.value(),
            "conditional_theorem_candidate": evidence.conditional_theorem_candidate.value(),
            "conditional_proof_receipt": evidence.conditional_proof_receipt,
            "conditional_statement": conditional.map(|conditional| serde_json::json!({
                    "statement_sha256": conditional.record.statement_sha256,
                    "statement_event_hash": conditional.record.statement_event_hash,
                    "statement_utf8": conditional.statement_utf8,
                })),
        }))
    }

    pub fn seed_qualification_profiles(
        &self,
        target_id: &str,
        contract_sha256: Sha256Digest,
    ) -> Result<Vec<SeedQualificationProfile>, TrustError> {
        let mut profiles = Vec::new();
        for profile in self.seed.records_by_digest.values().filter(|record| {
            record.contract().record_schema == "trellis-qualification-profile/v1"
                && record.value().get("target_id").and_then(Value::as_str) == Some(target_id)
                && record
                    .value()
                    .get("validation_contract_sha256")
                    .and_then(Value::as_str)
                    .and_then(|item| item.parse::<Sha256Digest>().ok())
                    == Some(contract_sha256)
        }) {
            self.require_seed_profile(profile)?;
            let basis_digest = digest_field(profile.value(), "independent_basis_sha256")?;
            let basis = self.seed.records_by_digest.get(&basis_digest).cloned().ok_or_else(|| {
                TrustError::new(
                    "qualification_profile_basis_missing",
                    "seed qualification profile names no independent basis record",
                )
            })?;
            self.require_seed_record(&basis)?;
            validate_independent_basis(&self.seed, &basis)?;
            let conditional_theorem_candidate =
                self.seed_conditional_candidate_for_profile(profile)?;
            profiles.push(SeedQualificationProfile {
                profile: profile.clone(),
                independent_basis: basis,
                conditional_theorem_candidate,
            });
        }
        profiles.sort_by(|left, right| {
            let left_order = left
                .profile
                .value()
                .get("attempt_order")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            let right_order = right
                .profile
                .value()
                .get("attempt_order")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            left_order.cmp(&right_order).then_with(|| {
                left.profile.digest().cmp(&right.profile.digest())
            })
        });
        Ok(profiles)
    }

    /// Return the exact seed-frozen conditional theorem records authorized by
    /// current-epoch `ApprovedProfileSelected` events.  The seed catalog alone
    /// is not activation authority: callers use this projection for crash
    /// recovery and must keep every other catalog entry dormant.
    pub fn active_selected_conditional_candidates(
        &self,
    ) -> Result<Vec<AuthoritativeRecord>, TrustError> {
        let mut selected = BTreeMap::new();
        for state in self.active_selections.values() {
            let profile = self
                .seed
                .records_by_digest
                .get(&state.selection.profile_sha256)
                .ok_or_else(|| {
                    TrustError::new(
                        "active_selection_profile_missing",
                        "active profile selection names no seed-frozen profile",
                    )
                })?;
            self.require_seed_profile(profile)?;
            let candidate = self.seed_conditional_candidate_for_profile(profile)?;
            selected.insert(candidate.digest(), candidate);
        }
        Ok(selected.into_values().collect())
    }

    fn seed_conditional_candidate_for_profile(
        &self,
        profile: &AuthoritativeRecord,
    ) -> Result<AuthoritativeRecord, TrustError> {
        let matches = self
            .seed
            .records_by_digest
            .values()
            .filter(|record| {
                record.contract().record_schema
                    == "trellis-conditional-theorem-candidate/v1"
                    && record
                        .value()
                        .get("profile_definition_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(profile.digest())
            })
            .cloned()
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [candidate] => {
                self.require_seed_record(candidate)?;
                Ok(candidate.clone())
            }
            [] => Err(TrustError::new(
                "conditional_candidate_missing",
                "seed qualification profile has no conditional theorem candidate",
            )),
            _ => Err(TrustError::new(
                "conditional_candidate_ambiguous",
                "seed qualification profile has multiple conditional theorem candidates",
            )),
        }
    }

    pub fn campaign_qualification_route(
        &self,
        history_sha256: Sha256Digest,
    ) -> Result<QualificationRoute, TrustError> {
        let (history, _, facts) = self.histories.get(&history_sha256).ok_or_else(|| {
            TrustError::new(
                "campaign_qualification_history_missing",
                "qualification routing names no source history",
            )
        })?;
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "campaign_qualification_contract_missing",
                "qualification history names no classified contract",
            )
        })?;
        let profile_available = self
            .seed_qualification_profiles(&contract.target_id, contract.digest)?
            .is_empty()
            == false;
        Ok(route_qualified_recovery(
            contract,
            facts,
            profile_available,
            profile_available,
        ))
    }

    pub fn seed_target_contracts(&self) -> Result<Vec<SeedTargetContract>, TrustError> {
        let mut targets = Vec::new();
        for record in self.seed.records_by_digest.values().filter(|record| {
            record.contract().record_schema == "trellis-source-validation-contract/v1"
        }) {
            let view = SourceValidationContractView::from_record(record)?;
            targets.push(SeedTargetContract {
                target_id: view.target_id,
                contract_sha256: view.digest,
                method: view.method,
            });
        }
        targets.sort_by(|left, right| left.target_id.as_bytes().cmp(right.target_id.as_bytes()));
        Ok(targets)
    }

    pub fn approved_evidence_leaf_digest(
        &self,
        logical_id: &str,
    ) -> Result<Sha256Digest, TrustError> {
        self.evidence
            .leaves_by_logical_id
            .get(logical_id)
            .map(|leaf| leaf.raw_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "approved_evidence_leaf_missing",
                    format!("evidence closure lacks logical ID {logical_id}"),
                )
            })
    }

    pub fn approved_evidence_logical_id_for_digest(
        &self,
        digest: Sha256Digest,
    ) -> Result<String, TrustError> {
        let matches: Vec<_> = self
            .evidence
            .leaves_by_logical_id
            .iter()
            .filter(|(_, leaf)| leaf.raw_sha256 == digest)
            .map(|(logical_id, _)| logical_id.clone())
            .collect();
        match matches.as_slice() {
            [logical_id] => Ok(logical_id.clone()),
            [] => Err(TrustError::new(
                "approved_evidence_digest_missing",
                format!("evidence closure has no leaf with digest {digest}"),
            )),
            _ => Err(TrustError::new(
                "approved_evidence_digest_ambiguous",
                format!("more than one evidence leaf has digest {digest}"),
            )),
        }
    }

    /// Resolve and revalidate one approved evidence leaf for a subsequent
    /// pinned invocation. This is the safe bridge from a seed logical ID to a
    /// filesystem path; callers must not join manifest paths themselves.
    pub fn approved_evidence_leaf_path(
        &self,
        evidence_root: &Path,
        logical_id: &str,
    ) -> Result<PathBuf, TrustError> {
        self.evidence.resolve_leaf_path(evidence_root, logical_id)
    }

    /// Resolve a contract-pinned executable digest to the unique approved
    /// logical ID and its point-of-use-validated filesystem path.
    pub fn approved_evidence_leaf_by_digest(
        &self,
        evidence_root: &Path,
        digest: Sha256Digest,
    ) -> Result<ResolvedEvidenceLeaf, TrustError> {
        self.evidence.resolve_leaf_by_digest(evidence_root, digest)
    }

    pub fn ensure_seed_contract_classified(
        &mut self,
        transaction_id: &str,
        contract_digest: Sha256Digest,
    ) -> Result<(), TrustError> {
        if self.contracts.contains_key(&contract_digest) {
            return Ok(());
        }
        let contract = self
            .seed
            .records_by_digest
            .get(&contract_digest)
            .cloned()
            .ok_or_else(|| {
                TrustError::new(
                    "seed_contract_definition_missing",
                    "target contract digest is absent from the seed closure",
                )
            })?;
        self.classify_source_validation(transaction_id, contract)?;
        Ok(())
    }

    pub fn into_journal(self) -> TrustJournal {
        self.journal
    }

    /// Bind the seed-frozen conditional theorem candidate to the exact
    /// sorry-free Lean local-closure record produced by the worker.  The
    /// returned receipt is kernel-derived; qualification checkers may consume
    /// it but cannot author or replace its proof hashes.
    pub fn checked_conditional_candidate_proof_receipt(
        &self,
        state: &crate::model::ProtocolState,
        profile: &AuthoritativeRecord,
    ) -> Result<(AuthoritativeRecord, Value), TrustError> {
        self.require_seed_profile(profile)?;
        let candidate = self.seed_conditional_candidate_for_profile(profile)?;
        let node_name = string_field(candidate.value(), "node_id")?;
        let node = crate::model::NodeId::from(node_name);
        let record = state.local_closure_records.get(&node).ok_or_else(|| {
            TrustError::new(
                "conditional_candidate_local_closure_missing",
                format!("conditional theorem candidate node {node_name} lacks a local closure record"),
            )
        })?;
        record.is_consistent_with_state(state, true).map_err(|error| {
            TrustError::new(
                "conditional_candidate_local_closure_inconsistent",
                format!("conditional theorem candidate {node_name}: {error}"),
            )
        })?;
        if record.node != node || record.is_sentinel_hashed() {
            return Err(TrustError::new(
                "conditional_candidate_local_closure_identity_invalid",
                "conditional theorem local closure owner or finalized hashes are invalid",
            ));
        }

        let expected_active_statement =
            digest_field(candidate.value(), "active_statement_sha256")?;
        let active_statement: Sha256Digest = record.active_statement_hash.parse()?;
        if active_statement != expected_active_statement {
            return Err(TrustError::new(
                "conditional_candidate_local_closure_statement_mismatch",
                "checked Lean declaration does not exactly prove the seed-frozen candidate statement",
            ));
        }
        let checker_toolchain: Sha256Digest = record.toolchain_hash.parse()?;
        let approved_axioms: Sha256Digest = record.approved_axioms_hash.parse()?;
        self.require_local_closure_evidence(record, "conditional theorem candidate")?;
        self.require_approved_tool_digest(
            checker_toolchain,
            "conditional theorem candidate checker/toolchain",
        )?;
        for (field, value) in [
            ("active_decl_hash", record.active_decl_hash.as_str()),
            ("lake_manifest_hash", record.lake_manifest_hash.as_str()),
            ("preamble_hash", record.preamble_hash.as_str()),
            ("approved_axioms_hash", record.approved_axioms_hash.as_str()),
        ] {
            let digest: Sha256Digest = value.parse().map_err(|_| {
                TrustError::new(
                    "conditional_candidate_local_closure_hash_invalid",
                    format!("{field} is not a SHA-256 digest"),
                )
            })?;
            if digest == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "conditional_candidate_local_closure_hash_zero",
                    format!("{field} cannot be zero"),
                ));
            }
        }

        let local_closure_record = serde_json::to_value(record).map_err(|error| {
            TrustError::new(
                "conditional_candidate_local_closure_encode_failed",
                error.to_string(),
            )
        })?;
        let semantic_definition_closure_sha256 = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&serde_json::json!({
                "boundary_theorems": record.boundary_theorems,
                "strict_theorem_deps": record.strict_theorem_deps,
                "strict_definition_deps": record.strict_definition_deps,
                "kernel_semantic_hashes": record.kernel_semantic_hashes,
                "active_decl_hash": record.active_decl_hash,
                "active_statement_hash": record.active_statement_hash,
            }))?,
        );
        let checker_and_axiom_closure_sha256 = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&serde_json::json!({
                "checker_toolchain_sha256": checker_toolchain,
                "lean_executable_sha256": record.lean_executable_hash,
                "lake_executable_sha256": record.lake_executable_hash,
                "checker_script_sha256": record.checker_script_hash,
                "approved_axiom_closure_sha256": approved_axioms,
                "kernel_axioms": record.kernel_axioms,
            }))?,
        );
        let mut receipt = serde_json::json!({
            "schema": "trellis-conditional-local-closure-proof-receipt/v1",
            "candidate_definition_sha256": candidate.digest(),
            "profile_definition_sha256": profile.digest(),
            "target_id": string_field(candidate.value(), "target_id")?,
            "node_id": node_name,
            "conditional_statement_sha256": digest_field(candidate.value(), "conditional_statement_sha256")?,
            "active_statement_sha256": expected_active_statement,
            "checker_toolchain_sha256": checker_toolchain,
            "approved_axiom_closure_sha256": approved_axioms,
            "checker_and_axiom_closure_sha256": checker_and_axiom_closure_sha256,
            "semantic_definition_closure_sha256": semantic_definition_closure_sha256,
            "local_closure_record": local_closure_record,
            "proof_receipt_sha256": Sha256Digest::ZERO,
        });
        let proof_receipt_sha256 =
            self_digest(DomainTag::RawArtifact, &receipt, "proof_receipt_sha256")?;
        receipt["proof_receipt_sha256"] = Value::String(proof_receipt_sha256.to_string());
        validate_conditional_candidate_proof_receipt(&candidate, &receipt)?;
        Ok((candidate, receipt))
    }

    /// Convert the runtime's finalized Lean local-closure record into the
    /// opaque proof carrier used by the trust journal.  This is intentionally
    /// post-backfill: sentinel hashes, open nodes, stale dependencies, skipped
    /// axiom cross-checks, and target/polarity mismatches all fail closed.
    pub fn record_campaign_local_closure_result(
        &mut self,
        transaction_prefix: &str,
        state: &crate::model::ProtocolState,
        target_id: &str,
    ) -> Result<RecordedCampaignProof, TrustError> {
        self.require_approved()?;
        let expected_statement = self.seed.records_by_digest.values().find_map(|record| {
            (record.contract().record_schema == "trellis-source-validation-contract/v1"
                && record.value().get("target_id").and_then(Value::as_str) == Some(target_id))
                .then(|| digest_field(record.value(), "target_statement_sha256"))
        }).transpose()?.ok_or_else(|| {
            TrustError::new(
                "campaign_proof_target_not_seed_frozen",
                format!("target {target_id} has no seed-frozen validation contract"),
            )
        })?;
        let primary = crate::model::ChallengeTargetId::from(target_id);
        let polarity = state.live_polarity(&primary);
        let node_name = if state.is_decide_primary(&primary) {
            state
                .decide_pair_node_for_polarity(&primary, polarity)
                .ok_or_else(|| {
                    TrustError::new(
                        "campaign_proof_node_unresolved",
                        format!("cannot resolve live Decide node for {target_id}"),
                    )
                })?
        } else {
            if polarity != crate::model::ChallengePolarity::Prove {
                return Err(TrustError::new(
                    "campaign_non_decide_disproof_invalid",
                    "a non-Decide target cannot carry disprove polarity",
                ));
            }
            state
                .configured_challenge_targets
                .get(&primary)
                .map(|spec| spec.name.clone())
                .ok_or_else(|| {
                    TrustError::new(
                        "campaign_proof_target_unconfigured",
                        format!("target {target_id} is absent from runtime configuration"),
                    )
                })?
        };
        let node = crate::model::NodeId::from(node_name.as_str());
        let record = state.local_closure_records.get(&node).ok_or_else(|| {
            TrustError::new(
                "campaign_local_closure_missing",
                format!("target {target_id} node {node_name} lacks a local closure record"),
            )
        })?;
        record.is_consistent_with_state(state, true).map_err(|error| {
            TrustError::new(
                "campaign_local_closure_inconsistent",
                format!("target {target_id}: {error}"),
            )
        })?;
        if record.node != node || record.is_sentinel_hashed() {
            return Err(TrustError::new(
                "campaign_local_closure_identity_invalid",
                "local closure owner or finalized hashes are invalid",
            ));
        }
        let generated_statement: Sha256Digest = record.active_statement_hash.parse()?;
        let checker_toolchain: Sha256Digest = record.toolchain_hash.parse()?;
        let approved_axioms: Sha256Digest = record.approved_axioms_hash.parse()?;
        self.require_local_closure_evidence(record, "campaign proof")?;
        for (field, value) in [
            ("active_decl_hash", record.active_decl_hash.as_str()),
            ("lake_manifest_hash", record.lake_manifest_hash.as_str()),
            ("preamble_hash", record.preamble_hash.as_str()),
        ] {
            let digest: Sha256Digest = value.parse().map_err(|_| {
                TrustError::new(
                    "campaign_local_closure_hash_invalid",
                    format!("{field} is not a SHA-256 digest"),
                )
            })?;
            if digest == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "campaign_local_closure_hash_zero",
                    format!("{field} cannot be zero"),
                ));
            }
        }
        let record_value = serde_json::to_value(record).map_err(|error| {
            TrustError::new("campaign_local_closure_encode_failed", error.to_string())
        })?;
        let semantic_definition_closure = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&serde_json::json!({
                "boundary_theorems": record.boundary_theorems,
                "strict_theorem_deps": record.strict_theorem_deps,
                "strict_definition_deps": record.strict_definition_deps,
                "kernel_semantic_hashes": record.kernel_semantic_hashes,
                "active_decl_hash": record.active_decl_hash,
                "active_statement_hash": record.active_statement_hash,
            }))?,
        );
        let polarity_name = match polarity {
            crate::model::ChallengePolarity::Prove => "prove",
            crate::model::ChallengePolarity::Disprove => "disprove",
        };
        let mut receipt = serde_json::json!({
            "schema": "trellis-local-closure-proof-receipt/v1",
            "target_id": target_id,
            "target_statement_sha256": expected_statement,
            "polarity": polarity_name,
            "live_target_id": state.live_target_of_decide_pair(&primary).as_str(),
            "node_id": node.as_str(),
            "generated_statement_sha256": generated_statement,
            "checker_toolchain_sha256": checker_toolchain,
            "approved_axiom_closure_sha256": approved_axioms,
            "semantic_definition_closure_sha256": semantic_definition_closure,
            "local_closure_record": record_value,
            "proof_receipt_sha256": Sha256Digest::ZERO,
        });
        let proof_artifact = self_digest(
            DomainTag::RawArtifact,
            &receipt,
            "proof_receipt_sha256",
        )?;
        receipt["proof_receipt_sha256"] = Value::String(proof_artifact.to_string());
        match polarity {
            crate::model::ChallengePolarity::Prove => {
                if generated_statement != expected_statement {
                    return Err(TrustError::new(
                        "campaign_positive_statement_mismatch",
                        "proved declaration statement differs from the seed target",
                    ));
                }
                self.record_positive_proof(
                    transaction_prefix,
                    CheckedPositiveProof {
                        target_id: target_id.to_owned(),
                        target_statement_sha256: expected_statement,
                        generated_theorem_statement_sha256: generated_statement,
                        checked_proof_artifact_sha256: proof_artifact,
                        checker_toolchain_sha256: checker_toolchain,
                        approved_axiom_closure_sha256: approved_axioms,
                        semantic_definition_closure_sha256: semantic_definition_closure,
                        proof_receipt: receipt,
                    },
                )
                .map(RecordedCampaignProof::Positive)
            }
            crate::model::ChallengePolarity::Disprove => self
                .record_negative_proof(
                    transaction_prefix,
                    CheckedNegativeProof {
                        target_id: target_id.to_owned(),
                        target_statement_sha256: expected_statement,
                        generated_not_theorem_statement_sha256: generated_statement,
                        checked_not_proof_artifact_sha256: proof_artifact,
                        checker_toolchain_sha256: checker_toolchain,
                        approved_axiom_closure_sha256: approved_axioms,
                        semantic_definition_closure_sha256: semantic_definition_closure,
                        proof_receipt: receipt,
                    },
                )
                .map(RecordedCampaignProof::Negative),
        }
    }

    /// Convert the deliberately small worker-authored witness report into the
    /// fully hash-bound certificate.  Workers do not calculate or select any
    /// seed/proof identity: those fields come from the approved contract and
    /// the two checked local closures.
    pub fn record_campaign_witness_report(
        &mut self,
        transaction_prefix: &str,
        state: &crate::model::ProtocolState,
        target_id: &str,
        witness_report: Value,
    ) -> Result<RecordedModelRefutation, TrustError> {
        self.require_approved()?;
        self.registry.validate(
            "trellis://schemas/campaign-witness-report/v1",
            &witness_report,
        )?;
        if string_field(&witness_report, "target_id")? != target_id {
            return Err(TrustError::new(
                "campaign_witness_report_target_mismatch",
                "witness report belongs to another target",
            ));
        }
        let contract_record = self.seed.records_by_digest.values().find(|record| {
            record.contract().record_schema == "trellis-source-validation-contract/v1"
                && record.value().get("target_id").and_then(Value::as_str) == Some(target_id)
        }).ok_or_else(|| {
            TrustError::new(
                "campaign_witness_contract_missing",
                "witness target has no seed-frozen validation contract",
            )
        })?;
        let witness_node_name = string_field(&witness_report, "witness_refutation_node_id")?;
        let witness_node = crate::model::NodeId::from(witness_node_name);
        let witness_record = state.local_closure_records.get(&witness_node).ok_or_else(|| {
            TrustError::new(
                "campaign_witness_local_closure_missing",
                "witness theorem lacks a local closure",
            )
        })?;
        witness_record.is_consistent_with_state(state, true).map_err(|error| {
            TrustError::new(
                "campaign_witness_local_closure_inconsistent",
                error.to_string(),
            )
        })?;
        if witness_record.is_sentinel_hashed() {
            return Err(TrustError::new(
                "campaign_witness_local_closure_sentinel",
                "witness proof record is not fully backfilled",
            ));
        }
        let witness_term = witness_report.get("witness_term").cloned().ok_or_else(|| {
            TrustError::new(
                "campaign_witness_term_missing",
                "witness report lacks witness_term",
            )
        })?;
        let witness_term_sha256 = tagged_hash(
            DomainTag::RawArtifact,
            &canonical_json_value(&witness_term)?,
        );
        let witness_statement: Sha256Digest = witness_record.active_statement_hash.parse()?;
        let mut certificate = serde_json::json!({
            "schema": "trellis-formal-witness-sidecar/v1",
            "target_id": target_id,
            "target_statement_sha256": digest_field(contract_record.value(), "target_statement_sha256")?,
            "formal_predicate_sha256": digest_field(contract_record.value(), "normalized_claim_sha256")?,
            "witness_term": witness_term,
            "witness_term_sha256": witness_term_sha256,
            "witness_refutation_node_id": witness_node_name,
            "generated_witness_refutation_sha256": witness_statement,
            "source_encoding_descriptor": witness_report.get("source_encoding_descriptor").cloned().ok_or_else(|| {
                TrustError::new(
                    "campaign_source_descriptor_missing",
                    "witness report lacks source_encoding_descriptor",
                )
            })?,
            "sidecar_sha256": Sha256Digest::ZERO,
        });
        let certificate_digest = self_digest(
            DomainTag::RawArtifact,
            &certificate,
            "sidecar_sha256",
        )?;
        certificate["sidecar_sha256"] = Value::String(certificate_digest.to_string());
        self.record_campaign_witness_refutation(
            transaction_prefix,
            state,
            target_id,
            certificate,
        )
    }

    /// Join a machine-readable witness sidecar to two finalized Lean local
    /// closures: the witness-specific theorem and the unrestricted `not T`
    /// theorem that depends on it.  This is the only campaign route into the
    /// witness-specific refutation and Rust-validation lane.
    pub fn record_campaign_witness_refutation(
        &mut self,
        transaction_prefix: &str,
        state: &crate::model::ProtocolState,
        target_id: &str,
        witness_certificate: Value,
    ) -> Result<RecordedModelRefutation, TrustError> {
        self.require_approved()?;
        self.registry.validate(
            "trellis://schemas/formal-witness-sidecar/v1",
            &witness_certificate,
        )?;
        let certificate_digest = self_digest(
            DomainTag::RawArtifact,
            &witness_certificate,
            "sidecar_sha256",
        )?;
        require_digest(&witness_certificate, "sidecar_sha256", certificate_digest)?;
        if string_field(&witness_certificate, "target_id")? != target_id {
            return Err(TrustError::new(
                "campaign_witness_target_mismatch",
                "witness sidecar belongs to another target",
            ));
        }
        let contract_record = self.seed.records_by_digest.values().find(|record| {
            record.contract().record_schema == "trellis-source-validation-contract/v1"
                && record.value().get("target_id").and_then(Value::as_str) == Some(target_id)
        }).ok_or_else(|| {
            TrustError::new(
                "campaign_witness_contract_missing",
                "witness target has no seed-frozen validation contract",
            )
        })?;
        let contract = SourceValidationContractView::from_record(contract_record)?;
        if !matches!(
            contract.method,
            SourceValidationMethod::ExactRustExecutionV1
                | SourceValidationMethod::CheckedRefutationReflectionV1
        ) {
            return Err(TrustError::new(
                "campaign_witness_for_unsupported_contract",
                "witness-specific refutation is not authorized for this claim shape",
            ));
        }
        for (field, expected) in [
            ("target_statement_sha256", contract.target_statement_sha256),
            (
                "formal_predicate_sha256",
                digest_field(contract_record.value(), "normalized_claim_sha256")?,
            ),
        ] {
            require_digest(&witness_certificate, field, expected)?;
        }
        let witness_term = witness_certificate.get("witness_term").ok_or_else(|| {
            TrustError::new(
                "campaign_witness_term_missing",
                "witness sidecar lacks its canonical witness term",
            )
        })?;
        let witness_term_sha256 = tagged_hash(
            DomainTag::RawArtifact,
            &canonical_json_value(witness_term)?,
        );
        require_digest(
            &witness_certificate,
            "witness_term_sha256",
            witness_term_sha256,
        )?;
        let primary = crate::model::ChallengeTargetId::from(target_id);
        if state.live_polarity(&primary) != crate::model::ChallengePolarity::Disprove {
            return Err(TrustError::new(
                "campaign_witness_without_disprove_polarity",
                "witness refutation requires the checked Disprove branch",
            ));
        }
        let not_t_name = state
            .decide_pair_node_for_polarity(
                &primary,
                crate::model::ChallengePolarity::Disprove,
            )
            .ok_or_else(|| {
                TrustError::new(
                    "campaign_refutation_node_unresolved",
                    "cannot resolve the live refutation theorem",
                )
            })?;
        let not_t_node = crate::model::NodeId::from(not_t_name.as_str());
        let witness_node_name = string_field(
            &witness_certificate,
            "witness_refutation_node_id",
        )?;
        let witness_node = crate::model::NodeId::from(witness_node_name);
        let not_t_record = state.local_closure_records.get(&not_t_node).ok_or_else(|| {
            TrustError::new(
                "campaign_not_t_local_closure_missing",
                "unrestricted refutation theorem lacks a local closure",
            )
        })?;
        let witness_record = state.local_closure_records.get(&witness_node).ok_or_else(|| {
            TrustError::new(
                "campaign_witness_local_closure_missing",
                "witness theorem lacks a local closure",
            )
        })?;
        for record in [not_t_record, witness_record] {
            record.is_consistent_with_state(state, true).map_err(|error| {
                TrustError::new(
                    "campaign_witness_local_closure_inconsistent",
                    error.to_string(),
                )
            })?;
            if record.is_sentinel_hashed() {
                return Err(TrustError::new(
                    "campaign_witness_local_closure_sentinel",
                    "witness proof records are not fully backfilled",
                ));
            }
        }
        let witness_statement: Sha256Digest = witness_record.active_statement_hash.parse()?;
        require_digest(
            &witness_certificate,
            "generated_witness_refutation_sha256",
            witness_statement,
        )?;
        let not_t_statement: Sha256Digest = not_t_record.active_statement_hash.parse()?;
        let checker_toolchain: Sha256Digest = not_t_record.toolchain_hash.parse()?;
        let approved_axioms: Sha256Digest = not_t_record.approved_axioms_hash.parse()?;
        self.require_local_closure_evidence(not_t_record, "unrestricted refutation")?;
        self.require_local_closure_evidence(witness_record, "witness refutation")?;
        if witness_record.toolchain_hash != not_t_record.toolchain_hash
            || witness_record.approved_axioms_hash != not_t_record.approved_axioms_hash
        {
            return Err(TrustError::new(
                "campaign_witness_checker_closure_mismatch",
                "witness and unrestricted refutation used different checker closures",
            ));
        }
        self.require_approved_tool_digest(
            checker_toolchain,
            "formal refutation checker/toolchain",
        )?;
        let witness_record_value = serde_json::to_value(witness_record).map_err(|error| {
            TrustError::new("campaign_witness_record_encode_failed", error.to_string())
        })?;
        let not_t_record_value = serde_json::to_value(not_t_record).map_err(|error| {
            TrustError::new("campaign_not_t_record_encode_failed", error.to_string())
        })?;
        let checked_witness_proof = tagged_hash(
            DomainTag::RawArtifact,
            &canonical_json_value(&witness_record_value)?,
        );
        let mut proof_receipt = serde_json::json!({
            "schema": "trellis-witness-refutation-proof-receipt/v1",
            "target_id": target_id,
            "target_statement_sha256": contract.target_statement_sha256,
            "witness_certificate_sha256": certificate_digest,
            "checker_toolchain_sha256": checker_toolchain,
            "approved_axiom_closure_sha256": approved_axioms,
            "witness_local_closure_record": witness_record_value,
            "not_t_local_closure_record": not_t_record_value,
            "proof_receipt_sha256": Sha256Digest::ZERO,
        });
        let checked_not_t_proof = self_digest(
            DomainTag::RawArtifact,
            &proof_receipt,
            "proof_receipt_sha256",
        )?;
        proof_receipt["proof_receipt_sha256"] =
            Value::String(checked_not_t_proof.to_string());
        let mut formal_value = serde_json::json!({
            "schema": "trellis-formal-refutation/v1",
            "target_id": target_id,
            "target_statement_sha256": contract.target_statement_sha256,
            "witness_term_sha256": witness_term_sha256,
            "formal_predicate_sha256": digest_field(&witness_certificate, "formal_predicate_sha256")?,
            "generated_witness_refutation_sha256": witness_statement,
            "witness_generator_schema_id": "witness-refutation/v1",
            "witness_certificate_sha256": certificate_digest,
            "checked_witness_proof_sha256": checked_witness_proof,
            "unrestricted_generator_schema_id": "forall-counterexample/v1",
            "generated_not_t_statement_sha256": not_t_statement,
            "checked_not_t_proof_sha256": checked_not_t_proof,
            "checker_toolchain_sha256": checker_toolchain,
            "approved_axiom_closure_sha256": approved_axioms,
            "witness_certificate": witness_certificate,
            "formal_proof_receipt": proof_receipt,
            "formal_bundle_sha256": Sha256Digest::ZERO,
        });
        let formal_digest = self_digest(
            DomainTag::FormalRefutation,
            &formal_value,
            "formal_bundle_sha256",
        )?;
        formal_value["formal_bundle_sha256"] = Value::String(formal_digest.to_string());
        let formal = AuthoritativeRecord::parse(&self.registry, formal_value)?;
        self.record_model_refutation(transaction_prefix, formal)
    }

    /// Commit an independently checked positive theorem and its unrestricted
    /// extracted-model verdict. Positive proofs do not enter the disproof-only
    /// source-validation or qualified-recovery lane.
    pub fn record_positive_proof(
        &mut self,
        transaction_prefix: &str,
        proof: CheckedPositiveProof,
    ) -> Result<RecordedPositiveProof, TrustError> {
        self.require_approved()?;
        self.require_seed_target(&proof.target_id, proof.target_statement_sha256)?;
        if self.positive_proofs.contains_key(&proof.target_id)
            || self.negative_proofs.contains_key(&proof.target_id)
            || self.formal_refutations.values().any(|formal| {
                formal.value().get("target_id").and_then(Value::as_str)
                    == Some(proof.target_id.as_str())
            })
        {
            return Err(TrustError::new(
                "duplicate_or_conflicting_target_result",
                "target already has a positive or negative formal result",
            ));
        }
        for (field, digest) in [
            (
                "generated_theorem_statement_sha256",
                proof.generated_theorem_statement_sha256,
            ),
            (
                "checked_proof_artifact_sha256",
                proof.checked_proof_artifact_sha256,
            ),
            ("checker_toolchain_sha256", proof.checker_toolchain_sha256),
            (
                "approved_axiom_closure_sha256",
                proof.approved_axiom_closure_sha256,
            ),
            (
                "semantic_definition_closure_sha256",
                proof.semantic_definition_closure_sha256,
            ),
        ] {
            if digest == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "positive_proof_zero_artifact",
                    format!("{field} cannot use the zero sentinel"),
                ));
            }
        }
        self.require_approved_tool_digest(
            proof.checker_toolchain_sha256,
            "positive proof checker/toolchain",
        )?;
        validate_local_closure_proof_receipt(
            &proof.proof_receipt,
            &proof.target_id,
            "prove",
            proof.checked_proof_artifact_sha256,
            proof.generated_theorem_statement_sha256,
            proof.checker_toolchain_sha256,
            proof.approved_axiom_closure_sha256,
            proof.semantic_definition_closure_sha256,
        )?;
        let proof_envelope = serde_json::json!({
            "schema": "trellis-checked-positive-proof/v1",
            "target_id": proof.target_id,
            "target_statement_sha256": proof.target_statement_sha256,
            "generated_theorem_statement_sha256": proof.generated_theorem_statement_sha256,
            "checked_proof_artifact_sha256": proof.checked_proof_artifact_sha256,
            "checker_toolchain_sha256": proof.checker_toolchain_sha256,
            "approved_axiom_closure_sha256": proof.approved_axiom_closure_sha256,
            "semantic_definition_closure_sha256": proof.semantic_definition_closure_sha256,
            "proof_receipt": proof.proof_receipt,
            "human_approval_event_hash": self.journal.current_human_approval_event_hash(),
            "journal_predecessor_sha256": self.journal.head().event_hash,
        });
        let proof_subject_sha256 = tagged_hash(
            DomainTag::RawArtifact,
            &canonical_json_value(&proof_envelope)?,
        );
        let proof_head = self.append_raw_json(
            &format!("{transaction_prefix}:proof-checked"),
            EventKind::ProofChecked,
            &proof.target_id,
            &proof_envelope,
        )?;
        let verdict = serde_json::json!({
            "schema": "trellis-unrestricted-verdict/v1",
            "target_id": proof.target_id,
            "target_statement_sha256": proof.target_statement_sha256,
            "verdict": "proved_in_extracted_model",
            "checked_proof_subject_sha256": proof_subject_sha256,
            "checked_proof_event_hash": proof_head.event_hash,
        });
        let verdict_head = self.append_raw_json(
            &format!("{transaction_prefix}:unrestricted-verdict"),
            EventKind::UnrestrictedVerdictRecorded,
            &proof.target_id,
            &verdict,
        )?;
        self.positive_proofs.insert(
            proof.target_id.clone(),
            PositiveProofState {
                target_id: proof.target_id,
                target_statement_sha256: proof.target_statement_sha256,
                proof_subject_sha256,
                proof_event_hash: proof_head.event_hash,
                unrestricted_verdict_event_hash: verdict_head.event_hash,
            },
        );
        Ok(RecordedPositiveProof {
            proof_event_hash: proof_head.event_hash,
            proof_subject_sha256,
            unrestricted_verdict_event_hash: verdict_head.event_hash,
        })
    }

    /// Commit a checked theorem-level refutation that does not export a
    /// witness.  It preserves the unrestricted model verdict, but cannot be
    /// routed through exact Rust execution or qualified recovery.
    pub fn record_negative_proof(
        &mut self,
        transaction_prefix: &str,
        proof: CheckedNegativeProof,
    ) -> Result<RecordedNegativeProof, TrustError> {
        self.require_approved()?;
        self.require_seed_target(&proof.target_id, proof.target_statement_sha256)?;
        if self.positive_proofs.contains_key(&proof.target_id)
            || self.negative_proofs.contains_key(&proof.target_id)
            || self.formal_refutations.values().any(|formal| {
                formal.value().get("target_id").and_then(Value::as_str)
                    == Some(proof.target_id.as_str())
            })
        {
            return Err(TrustError::new(
                "duplicate_or_conflicting_target_result",
                "target already has a positive or negative formal result",
            ));
        }
        for (field, digest) in [
            (
                "generated_not_theorem_statement_sha256",
                proof.generated_not_theorem_statement_sha256,
            ),
            (
                "checked_not_proof_artifact_sha256",
                proof.checked_not_proof_artifact_sha256,
            ),
            ("checker_toolchain_sha256", proof.checker_toolchain_sha256),
            (
                "approved_axiom_closure_sha256",
                proof.approved_axiom_closure_sha256,
            ),
            (
                "semantic_definition_closure_sha256",
                proof.semantic_definition_closure_sha256,
            ),
        ] {
            if digest == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "negative_proof_zero_artifact",
                    format!("{field} cannot use the zero sentinel"),
                ));
            }
        }
        self.require_approved_tool_digest(
            proof.checker_toolchain_sha256,
            "negative proof checker/toolchain",
        )?;
        validate_local_closure_proof_receipt(
            &proof.proof_receipt,
            &proof.target_id,
            "disprove",
            proof.checked_not_proof_artifact_sha256,
            proof.generated_not_theorem_statement_sha256,
            proof.checker_toolchain_sha256,
            proof.approved_axiom_closure_sha256,
            proof.semantic_definition_closure_sha256,
        )?;
        let proof_envelope = serde_json::json!({
            "schema": "trellis-checked-negative-proof/v1",
            "target_id": proof.target_id,
            "target_statement_sha256": proof.target_statement_sha256,
            "generated_not_theorem_statement_sha256": proof.generated_not_theorem_statement_sha256,
            "checked_not_proof_artifact_sha256": proof.checked_not_proof_artifact_sha256,
            "checker_toolchain_sha256": proof.checker_toolchain_sha256,
            "approved_axiom_closure_sha256": proof.approved_axiom_closure_sha256,
            "semantic_definition_closure_sha256": proof.semantic_definition_closure_sha256,
            "proof_receipt": proof.proof_receipt,
            "human_approval_event_hash": self.journal.current_human_approval_event_hash(),
            "journal_predecessor_sha256": self.journal.head().event_hash,
        });
        let proof_subject_sha256 = tagged_hash(
            DomainTag::RawArtifact,
            &canonical_json_value(&proof_envelope)?,
        );
        let proof_head = self.append_raw_json(
            &format!("{transaction_prefix}:proof-checked"),
            EventKind::ProofChecked,
            &proof.target_id,
            &proof_envelope,
        )?;
        let verdict = serde_json::json!({
            "schema": "trellis-unrestricted-verdict/v1",
            "target_id": proof.target_id,
            "target_statement_sha256": proof.target_statement_sha256,
            "verdict": "refuted_in_extracted_model",
            "negative_proof_subject_sha256": proof_subject_sha256,
            "negative_proof_event_hash": proof_head.event_hash,
        });
        let verdict_head = self.append_raw_json(
            &format!("{transaction_prefix}:unrestricted-verdict"),
            EventKind::UnrestrictedVerdictRecorded,
            &proof.target_id,
            &verdict,
        )?;
        self.negative_proofs.insert(
            proof.target_id.clone(),
            NegativeProofState {
                target_id: proof.target_id,
                target_statement_sha256: proof.target_statement_sha256,
                proof_subject_sha256,
                proof_event_hash: proof_head.event_hash,
                unrestricted_verdict_event_hash: verdict_head.event_hash,
                proof_envelope,
            },
        );
        Ok(RecordedNegativeProof {
            proof_event_hash: proof_head.event_hash,
            proof_subject_sha256,
            unrestricted_verdict_event_hash: verdict_head.event_hash,
        })
    }

    /// Commit the checked witness-specific refutation and then a generated
    /// unrestricted verdict that names that exact certificate.  Runtime
    /// execution is deliberately absent from this derivation.
    pub fn record_model_refutation(
        &mut self,
        transaction_prefix: &str,
        formal: AuthoritativeRecord,
    ) -> Result<RecordedModelRefutation, TrustError> {
        self.require_approved()?;
        self.validate_formal_refutation(&formal)?;
        let target_id = string_field(formal.value(), "target_id")?.to_owned();
        let target_statement = digest_field(formal.value(), "target_statement_sha256")?;
        if self.positive_proofs.contains_key(&target_id)
            || self.negative_proofs.contains_key(&target_id)
            || self.formal_refutations.values().any(|prior| {
                prior.value().get("target_id").and_then(Value::as_str)
                    == Some(target_id.as_str())
            })
        {
            return Err(TrustError::new(
                "duplicate_or_conflicting_target_result",
                "target already has a positive or negative formal result",
            ));
        }
        let formal_digest = formal.digest();
        if self.formal_refutations.contains_key(&formal_digest) {
            return Err(TrustError::new(
                "formal_refutation_duplicate",
                "formal refutation is already recorded",
            ));
        }
        let witness_head = self.append_record(
            &format!("{transaction_prefix}:witness-refutation"),
            EventKind::WitnessRefutationChecked,
            &target_id,
            formal.clone(),
        )?;
        self.formal_refutations.insert(formal_digest, formal);
        let verdict = serde_json::json!({
            "schema": "trellis-unrestricted-verdict/v1",
            "target_id": target_id,
            "target_statement_sha256": target_statement,
            "verdict": "refuted_in_extracted_model",
            "formal_refutation_sha256": formal_digest,
            "witness_refutation_event_hash": witness_head.event_hash,
        });
        let verdict_head = self.append_raw_json(
            &format!("{transaction_prefix}:unrestricted-verdict"),
            EventKind::UnrestrictedVerdictRecorded,
            &target_id,
            &verdict,
        )?;
        self.unrestricted_verdicts
            .insert(formal_digest, verdict_head.event_hash);
        Ok(RecordedModelRefutation {
            witness_refutation_event_hash: witness_head.event_hash,
            unrestricted_verdict_event_hash: verdict_head.event_hash,
            formal_refutation_sha256: formal_digest,
        })
    }

    pub fn classify_source_validation(
        &mut self,
        transaction_id: &str,
        contract: AuthoritativeRecord,
    ) -> Result<Sha256Digest, TrustError> {
        self.require_approved()?;
        let view = self.validate_contract_classification(&contract)?;
        let head = self.append_record(
            transaction_id,
            EventKind::SourceValidationClassified,
            &view.contract_id,
            contract.clone(),
        )?;
        self.contracts.insert(contract.digest(), (contract, view));
        Ok(head.event_hash)
    }

    pub fn record_source_attempt(
        &mut self,
        transaction_id: &str,
        attempt: AuthoritativeRecord,
    ) -> Result<Sha256Digest, TrustError> {
        let contract_digest = digest_field(attempt.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "attempt_contract_not_classified",
                "attempt names no classified seed contract",
            )
        })?;
        self.validate_source_attempt_execution_receipt(
            contract,
            &attempt,
            self.journal.head().event_hash,
        )?;
        validate_attempt(contract, &attempt)?;
        let attempt_id = string_field(attempt.value(), "attempt_id")?.to_owned();
        let head = self.append_record(
            transaction_id,
            EventKind::SourceValidationAttemptRecorded,
            &attempt_id,
            attempt.clone(),
        )?;
        self.attempts.insert(attempt.digest(), attempt);
        Ok(head.event_hash)
    }

    /// Execute the contract-pinned Rust runner and commit the authoritative
    /// attempt obtained by deterministic kernel augmentation of its strict
    /// JSON output.
    pub fn execute_and_record_source_attempt(
        &mut self,
        transaction_id: &str,
        contract_digest: Sha256Digest,
        invocation: SourceToolInvocation<'_>,
    ) -> Result<RecordedSourceExecution, TrustError> {
        self.require_approved()?;
        let contract = self
            .contracts
            .get(&contract_digest)
            .map(|(_, contract)| contract.clone())
            .ok_or_else(|| {
                TrustError::new(
                    "source_execution_contract_not_classified",
                    "source execution names no classified contract",
                )
            })?;
        let runner_sha256 = contract
            .harness_cohort_basis
            .as_ref()
            .map(|basis| basis.runner_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "source_execution_not_authorized",
                    "contract does not authorize exact Rust execution",
                )
            })?;
        let predecessor = self.journal.head().event_hash;
        let receipt = super::execution::execute_approved_json(
            super::execution::ApprovedExecutionRequest {
                evidence: &self.evidence,
                tool_logical_id: invocation.tool_logical_id,
                purpose: "exact-rust-source-runner/v1",
                journal_predecessor_sha256: predecessor,
                execution: super::execution::PinnedExecutionRequest {
                    runner_path: invocation.runner_path,
                    expected_runner_sha256: runner_sha256,
                    command_id: invocation.command_id,
                    working_directory: invocation.working_directory,
                    environment: invocation.environment,
                    input: invocation.input,
                    limits: invocation.limits,
                },
            },
        )?;
        let mut value = receipt.parsed_output().cloned().ok_or_else(|| {
            TrustError::new(
                "source_runner_output_not_json",
                "source runner did not return one strict JSON attempt body",
            )
        })?;
        let object = value.as_object_mut().ok_or_else(|| {
            TrustError::new(
                "source_runner_output_not_object",
                "source runner output must be a JSON object",
            )
        })?;
        if object.contains_key("execution_receipt")
            || object.contains_key("raw_artifact_manifest_sha256")
            || object.contains_key("attempt_sha256")
            || object.contains_key("actual_environment_sha256")
            || object.contains_key("command_sha256")
            || object.contains_key("harness_cohort_sha256")
            || object.contains_key("full_tuple_sha256")
        {
            return Err(TrustError::new(
                "source_runner_output_claims_kernel_fields",
                "source runner cannot author kernel receipt or command-binding fields",
            ));
        }
        object.insert(
            "actual_environment_sha256".to_owned(),
            receipt.value()["actual_environment_sha256"].clone(),
        );
        object.insert(
            "command_sha256".to_owned(),
            receipt.value()["command_sha256"].clone(),
        );
        object.insert(
            "harness_cohort_sha256".to_owned(),
            Value::String(expected_harness_cohort_sha256(&contract)?.to_string()),
        );
        object.insert("execution_receipt".to_owned(), receipt.value().clone());
        object.insert(
            "raw_artifact_manifest_sha256".to_owned(),
            Value::String(receipt.digest().to_string()),
        );
        object.insert(
            "attempt_sha256".to_owned(),
            Value::String(Sha256Digest::ZERO.to_string()),
        );
        let full_tuple = expected_full_tuple_sha256(&contract, &value)?;
        value["full_tuple_sha256"] = Value::String(full_tuple.to_string());
        let digest = self_digest(
            DomainTag::SourceValidationAttempt,
            &value,
            "attempt_sha256",
        )?;
        value["attempt_sha256"] = Value::String(digest.to_string());
        let record = AuthoritativeRecord::parse(&self.registry, value)?;
        let event_hash = self.record_source_attempt(transaction_id, record)?;
        Ok(RecordedSourceExecution {
            record_sha256: digest,
            event_hash,
            execution_receipt_sha256: receipt.digest(),
        })
    }

    pub fn record_source_outcome(
        &mut self,
        transaction_id: &str,
        outcome: AuthoritativeRecord,
    ) -> Result<Sha256Digest, TrustError> {
        let contract_digest = digest_field(outcome.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "outcome_contract_not_classified",
                "outcome names no classified seed contract",
            )
        })?;
        require_digest(
            outcome.value(),
            "journal_predecessor_sha256",
            self.journal.head().event_hash,
        )?;
        if contract.method == SourceValidationMethod::ExactRustExecutionV1 {
            self.validate_source_outcome_execution_receipt(
                contract,
                &outcome,
                self.journal.head().event_hash,
            )?;
        }
        let attempt = outcome
            .value()
            .get("attempt_sha256")
            .and_then(Value::as_str)
            .map(str::parse::<Sha256Digest>)
            .transpose()?
            .and_then(|digest| self.attempts.get(&digest));
        let validated = validate_outcome(contract, &outcome, attempt)?;
        let result_id = string_field(outcome.value(), "result_id")?.to_owned();
        let head = self.append_record(
            transaction_id,
            EventKind::SourceValidationOutcomeRecorded,
            &result_id,
            outcome,
        )?;
        self.outcomes_by_contract
            .entry(contract_digest)
            .or_default()
            .push(OutcomeEntry {
                event: (&head).into(),
                validated,
            });
        Ok(head.event_hash)
    }

    /// Execute the pinned observation oracle over an already committed source
    /// attempt and commit its exact, receipt-bound classification.
    pub fn execute_and_record_source_outcome(
        &mut self,
        transaction_id: &str,
        contract_digest: Sha256Digest,
        invocation: SourceToolInvocation<'_>,
    ) -> Result<RecordedSourceExecution, TrustError> {
        self.require_approved()?;
        let contract = self
            .contracts
            .get(&contract_digest)
            .map(|(_, contract)| contract.clone())
            .ok_or_else(|| {
                TrustError::new(
                    "source_oracle_contract_not_classified",
                    "source oracle names no classified contract",
                )
            })?;
        let oracle_sha256 = contract.observation_oracle_sha256.ok_or_else(|| {
            TrustError::new(
                "source_oracle_not_authorized",
                "contract does not authorize an observation oracle",
            )
        })?;
        let predecessor = self.journal.head().event_hash;
        let receipt = super::execution::execute_approved_json(
            super::execution::ApprovedExecutionRequest {
                evidence: &self.evidence,
                tool_logical_id: invocation.tool_logical_id,
                purpose: "exact-rust-observation-oracle/v1",
                journal_predecessor_sha256: predecessor,
                execution: super::execution::PinnedExecutionRequest {
                    runner_path: invocation.runner_path,
                    expected_runner_sha256: oracle_sha256,
                    command_id: invocation.command_id,
                    working_directory: invocation.working_directory,
                    environment: invocation.environment,
                    input: invocation.input,
                    limits: invocation.limits,
                },
            },
        )?;
        require_successful_json_execution(
            receipt.value(),
            "source observation oracle",
        )?;
        let observation = receipt.parsed_output().cloned().ok_or_else(|| {
            TrustError::new(
                "source_oracle_output_not_json",
                "observation oracle did not return one strict JSON observation",
            )
        })?;
        self.registry.validate(
            "trellis://schemas/source-oracle-observation/v1",
            &observation,
        )?;
        let attempt_sha256 = digest_field(&observation, "attempt_sha256")?;
        let attempt = self.attempts.get(&attempt_sha256).ok_or_else(|| {
            TrustError::new(
                "source_oracle_attempt_unrecorded",
                "oracle observation names no committed exact attempt",
            )
        })?;
        if digest_field(attempt.value(), "validation_contract_sha256")? != contract.digest {
            return Err(TrustError::new(
                "source_oracle_attempt_contract_mismatch",
                "oracle observation names an attempt for another contract",
            ));
        }
        let role = string_field(attempt.value(), "role")?;
        let classification = string_field(&observation, "classification")?;
        let axes = centrally_derive_source_outcome_axes(role, classification)?;
        let source_validator = contract.source_validator_sha256.ok_or_else(|| {
            TrustError::new(
                "source_validator_not_authorized",
                "exact contract lacks a central source-validator identity",
            )
        })?;
        self.require_approved_tool_digest(source_validator, "source outcome validator")?;
        let mut value = serde_json::json!({
            "schema": "trellis-source-validation-outcome/v1",
            "result_id": format!("exact-{attempt_sha256}"),
            "attempt_sha256": attempt_sha256,
            "role": role,
            "harness_cohort_sha256": digest_field(attempt.value(), "harness_cohort_sha256")?,
            "target_id": contract.target_id,
            "validation_contract_id": contract.contract_id,
            "validation_contract_sha256": contract.digest,
            "source_claim_lineage_id": contract.lineage_id,
            "source_claim_lineage_sha256": contract.lineage_sha256,
            "validation_method": "exact_rust_execution_v1",
            "full_tuple_sha256": digest_field(attempt.value(), "full_tuple_sha256")?,
            "source_validator_sha256": source_validator,
            "oracle_observation": observation,
            "oracle_invocation_receipt": receipt.value(),
            "oracle_invocation_receipt_sha256": receipt.digest(),
            "status": axes.status,
            "realizability": axes.realizability,
            "observability": axes.observability,
            "reproducibility": axes.reproducibility,
            "decisiveness": axes.decisiveness,
            "derivation_receipt_sha256": receipt.digest(),
            "journal_predecessor_sha256": predecessor,
            "result_sha256": Sha256Digest::ZERO,
        });
        for field in [
            "raw_observation_sha256",
            "independent_basis_id",
            "independent_basis_sha256",
            "witness_resource_demand_sha256",
        ] {
            if let Some(item) = value["oracle_observation"].get(field).cloned() {
                value[field] = item;
            }
        }
        let digest = self_digest(
            DomainTag::SourceValidationOutcome,
            &value,
            "result_sha256",
        )?;
        value["result_sha256"] = Value::String(digest.to_string());
        let record = AuthoritativeRecord::parse(&self.registry, value)?;
        let event_hash = self.record_source_outcome(transaction_id, record)?;
        Ok(RecordedSourceExecution {
            record_sha256: digest,
            event_hash,
            execution_receipt_sha256: receipt.digest(),
        })
    }

    /// Generate the only legal v1 outcome for an unsupported claim shape.
    /// No runner, oracle, attempt, witness, or resource status is consulted.
    pub fn record_undefined_source_validation(
        &mut self,
        transaction_id: &str,
        contract_digest: Sha256Digest,
        result_id: &str,
    ) -> Result<Sha256Digest, TrustError> {
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "undefined_contract_not_classified",
                "undefined outcome names no classified contract",
            )
        })?;
        if contract.method != SourceValidationMethod::NotDefinedForClaimShapeV1 {
            return Err(TrustError::new(
                "undefined_outcome_for_supported_method",
                "kernel-generated undefined outcomes require not_defined_for_claim_shape_v1",
            ));
        }
        let predecessor = self.journal.head().event_hash;
        let derivation = tagged_hash(
            DomainTag::SemanticValidator,
            &canonical_json_value(&serde_json::json!({
                "schema": "trellis-not-defined-derivation/v1",
                "target_id": contract.target_id,
                "validation_contract_sha256": contract.digest,
                "source_claim_lineage_sha256": contract.lineage_sha256,
                "validation_method": "not_defined_for_claim_shape_v1",
                "journal_predecessor_sha256": predecessor,
            }))?,
        );
        let mut value = serde_json::json!({
            "schema": "trellis-source-validation-outcome/v1",
            "result_id": result_id,
            "target_id": contract.target_id,
            "validation_contract_id": contract.contract_id,
            "validation_contract_sha256": contract.digest,
            "source_claim_lineage_id": contract.lineage_id,
            "source_claim_lineage_sha256": contract.lineage_sha256,
            "validation_method": "not_defined_for_claim_shape_v1",
            "status": "not_defined_for_claim_shape",
            "realizability": "unestablished",
            "observability": "unmappable",
            "reproducibility": "not_attempted",
            "decisiveness": "unsupported_claim_class",
            "derivation_receipt_sha256": derivation,
            "journal_predecessor_sha256": predecessor,
            "result_sha256": Sha256Digest::ZERO,
        });
        let digest = self_digest(
            DomainTag::SourceValidationOutcome,
            &value,
            "result_sha256",
        )?;
        value["result_sha256"] = Value::String(digest.to_string());
        let record = AuthoritativeRecord::parse(&self.registry, value)?;
        self.record_source_outcome(transaction_id, record)
    }

    /// Run one approved checked-reflection tool and commit only the exact
    /// reflection result body it returned, augmented with kernel-owned
    /// journal and execution-receipt bindings.
    ///
    /// The checker executable is pinned by the seed contract; its selected
    /// evidence logical ID must resolve to that exact digest. The checker is
    /// not allowed to author its toolchain identity, the journal predecessor,
    /// approval closure, execution receipt, or result self digest.
    pub fn execute_and_record_reflection_validation_result(
        &mut self,
        transaction_id: &str,
        contract_digest: Sha256Digest,
        model_negative_result_sha256: Sha256Digest,
        invocation: SourceToolInvocation<'_>,
    ) -> Result<RecordedReflectionExecution, TrustError> {
        self.require_approved()?;
        let contract = self
            .contracts
            .get(&contract_digest)
            .map(|(_, contract)| contract.clone())
            .ok_or_else(|| {
                TrustError::new(
                    "reflection_execution_contract_not_classified",
                    "reflection execution names no classified contract",
                )
            })?;
        if contract.method != SourceValidationMethod::CheckedRefutationReflectionV1 {
            return Err(TrustError::new(
                "reflection_execution_not_authorized",
                "contract does not authorize checked theorem reflection",
            ));
        }
        let (negative_target, negative_statement, _) =
            self.checked_model_negative_binding(model_negative_result_sha256)?;
        if negative_target != contract.target_id
            || negative_statement != contract.target_statement_sha256
        {
            return Err(TrustError::new(
                "reflection_execution_model_refutation_mismatch",
                "checked model refutation belongs to another frozen target",
            ));
        }

        let checker_sha256 = contract.reflection_checker_sha256.ok_or_else(|| {
            TrustError::new(
                "reflection_contract_checker_missing",
                "reflection contract lacks its seed-pinned checker",
            )
        })?;
        let selected_leaf_sha256 =
            self.approved_evidence_leaf_digest(invocation.tool_logical_id)?;
        if selected_leaf_sha256 != checker_sha256 {
            return Err(TrustError::new(
                "reflection_checker_leaf_mismatch",
                "selected evidence leaf differs from the contract-pinned reflection checker",
            ));
        }
        if checker_sha256 == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "reflection_checker_placeholder_digest",
                "reflection checker cannot use the zero sentinel",
            ));
        }
        let predecessor = self.journal.head().event_hash;
        let approval_closure = self.journal.derived_result_root();
        let receipt = super::execution::execute_approved_json(
            super::execution::ApprovedExecutionRequest {
                evidence: &self.evidence,
                tool_logical_id: invocation.tool_logical_id,
                purpose: "checked-refutation-reflection/v1",
                journal_predecessor_sha256: predecessor,
                execution: super::execution::PinnedExecutionRequest {
                    runner_path: invocation.runner_path,
                    expected_runner_sha256: checker_sha256,
                    command_id: invocation.command_id,
                    working_directory: invocation.working_directory,
                    environment: invocation.environment,
                    input: invocation.input,
                    limits: invocation.limits,
                },
            },
        )?;
        let result = build_reflection_result_from_receipt(
            &self.registry,
            &receipt,
            checker_sha256,
            approval_closure,
            predecessor,
        )?;
        let result_sha256 = result.digest();
        let event_hash = self.record_reflection_validation_result(
            transaction_id,
            model_negative_result_sha256,
            result,
        )?;
        Ok(RecordedReflectionExecution {
            result_sha256,
            event_hash,
            checker_execution_receipt_sha256: receipt.digest(),
        })
    }

    /// Authenticate the theorem-level source reflection `T_R -> T_M` and the
    /// exact checked model refutation before recording the derived `not T_R`.
    /// Hash-shaped placeholders or a model verdict enum are not sufficient.
    pub fn record_reflection_validation_result(
        &mut self,
        transaction_id: &str,
        model_negative_result_sha256: Sha256Digest,
        result: AuthoritativeRecord,
    ) -> Result<Sha256Digest, TrustError> {
        self.require_approved()?;
        if result.contract().record_schema != "trellis-reflection-validation-result/v1" {
            return Err(TrustError::new(
                "reflection_result_wrong_schema",
                "expected a v1 reflection validation result",
            ));
        }
        let contract_digest = digest_field(result.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "reflection_contract_unclassified",
                "reflection result names no classified contract",
            )
        })?;
        if contract.method != SourceValidationMethod::CheckedRefutationReflectionV1 {
            return Err(TrustError::new(
                "reflection_result_for_wrong_method",
                "contract does not authorize checked theorem reflection",
            ));
        }
        let (negative_target, negative_statement, checked_model_proof) =
            self.checked_model_negative_binding(model_negative_result_sha256)?;
        if negative_target != contract.target_id
            || negative_statement != contract.target_statement_sha256
        {
            return Err(TrustError::new(
                "reflection_model_refutation_mismatch",
                "reflection must consume the checked negative result for its frozen target",
            ));
        }
        for (field, expected) in [
            ("source_claim_lineage_sha256", contract.lineage_sha256),
            ("validation_contract_sha256", contract.digest),
            (
                "rust_target_statement_sha256",
                contract.rust_target_statement_sha256,
            ),
            (
                "model_target_statement_sha256",
                contract.target_statement_sha256,
            ),
            (
                "generated_implication_statement_sha256",
                contract.reflection_theorem_sha256.ok_or_else(|| {
                    TrustError::new(
                        "reflection_contract_theorem_missing",
                        "reflection contract lacks its frozen implication",
                    )
                })?,
            ),
            (
                "checked_reflection_proof_sha256",
                contract.reflection_proof_artifact_sha256.ok_or_else(|| {
                    TrustError::new(
                        "reflection_contract_proof_missing",
                        "reflection contract lacks its frozen proof artifact",
                    )
                })?,
            ),
            (
                "checked_model_refutation_sha256",
                checked_model_proof,
            ),
            ("approval_closure_sha256", self.journal.derived_result_root()),
            ("journal_predecessor_sha256", self.journal.head().event_hash),
        ] {
            require_digest(result.value(), field, expected)?;
        }
        for (field, expected) in [
            ("target_id", contract.target_id.as_str()),
            ("source_claim_lineage_id", contract.lineage_id.as_str()),
        ] {
            if string_field(result.value(), field)? != expected {
                return Err(TrustError::new(
                    "reflection_result_identity_mismatch",
                    format!("reflection result differs at {field}"),
                ));
            }
        }
        for field in [
            "checker_toolchain_sha256",
            "approved_axiom_closure_sha256",
            "generated_source_refutation_statement_sha256",
            "checked_source_refutation_proof_sha256",
        ] {
            if digest_field(result.value(), field)? == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "reflection_zero_checked_artifact",
                    format!("{field} cannot be zero"),
                ));
            }
        }
        let validated_negative = self.validate_reflection_result_at(
            &result,
            self.journal.head().event_hash,
            self.journal.derived_result_root(),
        )?;
        if validated_negative != model_negative_result_sha256 {
            return Err(TrustError::new(
                "reflection_formal_refutation_mismatch",
                "reflection result resolves to a different checked model refutation",
            ));
        }
        let target_id = contract.target_id.clone();
        let digest = result.digest();
        let head = self.append_record(
            transaction_id,
            EventKind::ReflectionValidationResultRecorded,
            &target_id,
            result.clone(),
        )?;
        self.reflection_results
            .insert(digest, (result, head.event_hash));
        Ok(head.event_hash)
    }

    /// Generate the dominance summary from the complete current lineage
    /// history. No agent supplies blocker booleans or latest-result choices.
    pub fn summarize_source_history(
        &mut self,
        transaction_id: &str,
        contract_digest: Sha256Digest,
        summarizer_id: &str,
        summarizer_sha256: Sha256Digest,
    ) -> Result<AuthoritativeRecord, TrustError> {
        self.require_approved_tool_digest(summarizer_sha256, "source history summarizer")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new("history_contract_unknown", "history contract is not classified")
        })?;
        let outcomes = self
            .outcomes_by_contract
            .get(&contract_digest)
            .cloned()
            .unwrap_or_default();
        let validated: Vec<_> = outcomes.iter().map(|entry| entry.validated.clone()).collect();
        let facts = HistoryFacts::derive(&validated);
        let registration = self
            .lineage_registration
            .get(&contract.lineage_sha256)
            .ok_or_else(|| {
                TrustError::new("history_lineage_unregistered", "lineage receipt is missing")
            })?;
        let predecessor = self.journal.head();
        let mut latest: Vec<_> = facts.latest_by_role.values().copied().collect();
        latest.sort();
        latest.dedup();
        let cohort_blockers = derive_cohort_blockers(&outcomes);
        let coverage_events: Vec<_> = outcomes
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "sequence_number": entry.event.sequence_number,
                    "event_hash": entry.event.event_hash,
                    "result_sha256": entry.validated.digest,
                })
            })
            .collect();
        let coverage_root = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&Value::Array(coverage_events))?,
        );
        let journal_head_sha256 = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&serde_json::to_value(&predecessor).map_err(|error| {
                TrustError::new("journal_head_encode_failed", error.to_string())
            })?)?,
        );
        let mut value = serde_json::json!({
            "schema": "trellis-source-validation-history-summary/v1",
            "target_id": contract.target_id,
            "target_statement_sha256": contract.target_statement_sha256,
            "validation_contract_id": contract.contract_id,
            "validation_contract_sha256": contract.digest,
            "source_claim_lineage_id": contract.lineage_id,
            "source_claim_lineage_sha256": contract.lineage_sha256,
            "latest_result_hashes": latest,
            "decisive_source_refutation_present": facts.decisive_source_refutation_present,
            "source_model_mismatch_unresolved": facts.source_model_mismatch_unresolved,
            "cohort_blockers": cohort_blockers,
            "language_inadmissibility_unresolved": facts.language_inadmissibility_unresolved,
            "contract_invalid": facts.contract_invalid,
            "summarizer_id": summarizer_id,
            "summarizer_sha256": summarizer_sha256,
            "covered_journal_id": predecessor.journal_id,
            "lineage_registration_sequence": registration.sequence_number,
            "covered_from_sequence": registration.sequence_number,
            "covered_through_sequence": predecessor.sequence_number,
            "covered_through_event_hash": predecessor.event_hash,
            "coverage_root_sha256": coverage_root,
            "journal_head_sha256": journal_head_sha256,
            "summary_derivation_sha256": Sha256Digest::ZERO,
            "summary_sha256": Sha256Digest::ZERO,
        });
        let derivation = summary_derivation(&value)?;
        value["summary_derivation_sha256"] = Value::String(derivation.to_string());
        let digest = self_digest(
            DomainTag::SourceValidationHistorySummary,
            &value,
            "summary_sha256",
        )?;
        value["summary_sha256"] = Value::String(digest.to_string());
        let record = AuthoritativeRecord::parse(&self.registry, value)?;
        let target_id = contract.target_id.clone();
        let head = self.append_record(
            transaction_id,
            EventKind::SourceValidationHistorySummarized,
            &target_id,
            record.clone(),
        )?;
        self.histories
            .insert(record.digest(), (record.clone(), head.event_hash, facts));
        Ok(record)
    }

    /// Enter qualification only after every replay-independent prerequisite
    /// has been checked and while the journal is still exactly at the history
    /// summary event. This makes the summary tail closed by construction.
    #[allow(clippy::too_many_arguments)]
    pub fn select_qualification_profile(
        &mut self,
        transaction_id: &str,
        formal_refutation_sha256: Sha256Digest,
        history_summary_sha256: Sha256Digest,
        independent_basis: AuthoritativeRecord,
        witness_resource_demand: AuthoritativeRecord,
        source_witness_admissibility: AuthoritativeRecord,
        profile: AuthoritativeRecord,
    ) -> Result<QualificationSelection, TrustError> {
        self.require_approved()?;
        let formal = self
            .formal_refutations
            .get(&formal_refutation_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_formal_refutation_unrecorded",
                    "qualification must use a checked journaled formal refutation",
                )
            })?;
        let (history, history_event_hash, facts) = self
            .histories
            .get(&history_summary_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_history_unrecorded",
                    "qualification must use a generated journaled history summary",
                )
            })?;
        if self.journal.head().event_hash != *history_event_hash {
            return Err(TrustError::new(
                "qualification_history_tail_not_empty",
                "profile selection must immediately follow its complete history summary",
            ));
        }
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "qualification_contract_unclassified",
                "history names no classified validation contract",
            )
        })?;
        self.require_seed_profile(&profile)?;
        self.require_seed_record(&independent_basis)?;
        validate_independent_basis(&self.seed, &independent_basis)?;
        self.validate_qualification_input_execution_receipts(
            formal_refutation_sha256,
            history_summary_sha256,
            &independent_basis,
            &profile,
            &witness_resource_demand,
            &source_witness_admissibility,
            &[self.journal.head().event_hash],
        )?;
        let prerequisites = QualificationPrerequisites {
            contract,
            formal_refutation: formal,
            history_summary: history,
            history_summary_event_hash: *history_event_hash,
            history_facts: facts,
            independent_basis: &independent_basis,
            witness_resource_demand: &witness_resource_demand,
            source_witness_admissibility: &source_witness_admissibility,
            profile: &profile,
        };
        validate_qualification_prerequisites(&prerequisites)?;
        let history_event_hash = *history_event_hash;
        let target_id = contract.target_id.clone();
        let profile_digest = profile.digest();
        let head = self.append_record(
            transaction_id,
            EventKind::ApprovedProfileSelected,
            &target_id,
            profile,
        )?;
        let selection = QualificationSelection {
            profile_sha256: profile_digest,
            profile_selection_event_hash: head.event_hash,
            history_summary_sha256,
            history_summary_event_hash: history_event_hash,
        };
        self.active_selections.insert(
            head.event_hash,
            SelectionState {
                selection,
                target_id,
                // A profile selection authorizes the already gate-approved
                // conditional candidate; it does not assert that the proof
                // exists yet.  The runtime activates the selected node from
                // this durable event, and
                // `resume_qualification_selection_with_evidence` installs the
                // checked receipt only after ordinary PF closes it.
                evidence: None,
            },
        );
        Ok(selection)
    }

    /// Run the seed-pinned structural demand checker.  Its stdout supplies
    /// only the semantic record body; the kernel injects the execution receipt
    /// and record identity before validation.
    pub fn execute_witness_resource_demand(
        &self,
        formal_refutation_sha256: Sha256Digest,
        history_summary_sha256: Sha256Digest,
        independent_basis: &AuthoritativeRecord,
        profile: &AuthoritativeRecord,
        invocation: SourceToolInvocation<'_>,
    ) -> Result<AuthoritativeRecord, TrustError> {
        self.require_seed_profile(profile)?;
        let input = self.qualification_prerequisite_context(
            formal_refutation_sha256,
            history_summary_sha256,
            independent_basis,
            profile,
            None,
        )?;
        self.require_exact_tool_invocation(
            &invocation,
            digest_field(profile.value(), "witness_demand_checker_sha256")?,
            "trellis-witness-resource-demand-checker-v1",
            &input,
        )?;
        self.execute_qualification_record(
            profile,
            "witness_demand_checker_sha256",
            "witness-resource-demand-checker/v1",
            "trellis://schemas/witness-resource-demand/v1",
            "demand_certificate_sha256",
            DomainTag::WitnessResourceDemand,
            invocation,
        )
    }

    /// Run the seed-pinned source-admissibility checker over the exact formal
    /// witness, demand certificate, basis, contract, and profile.
    pub fn execute_source_witness_admissibility(
        &self,
        formal_refutation_sha256: Sha256Digest,
        history_summary_sha256: Sha256Digest,
        independent_basis: &AuthoritativeRecord,
        witness_resource_demand: &AuthoritativeRecord,
        profile: &AuthoritativeRecord,
        invocation: SourceToolInvocation<'_>,
    ) -> Result<AuthoritativeRecord, TrustError> {
        self.require_seed_profile(profile)?;
        let input = self.qualification_prerequisite_context(
            formal_refutation_sha256,
            history_summary_sha256,
            independent_basis,
            profile,
            Some(witness_resource_demand),
        )?;
        self.require_exact_tool_invocation(
            &invocation,
            digest_field(profile.value(), "source_admissibility_checker_sha256")?,
            "trellis-source-witness-admissibility-checker-v1",
            &input,
        )?;
        self.execute_qualification_record(
            profile,
            "source_admissibility_checker_sha256",
            "source-witness-admissibility-checker/v1",
            "trellis://schemas/source-witness-admissibility/v1",
            "certificate_sha256",
            DomainTag::SourceWitnessAdmissibility,
            invocation,
        )
    }

    fn qualification_prerequisite_context(
        &self,
        formal_refutation_sha256: Sha256Digest,
        history_summary_sha256: Sha256Digest,
        independent_basis: &AuthoritativeRecord,
        profile: &AuthoritativeRecord,
        witness_resource_demand: Option<&AuthoritativeRecord>,
    ) -> Result<Value, TrustError> {
        self.require_seed_profile(profile)?;
        self.require_seed_record(independent_basis)?;
        validate_independent_basis(&self.seed, independent_basis)?;
        let formal = self
            .formal_refutations
            .get(&formal_refutation_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_prerequisite_formal_missing",
                    "qualification checker context names no formal refutation",
                )
            })?;
        let (history, _, _) = self.histories.get(&history_summary_sha256).ok_or_else(|| {
            TrustError::new(
                "qualification_prerequisite_history_missing",
                "qualification checker context names no history summary",
            )
        })?;
        let contract_sha256 = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_sha256).ok_or_else(|| {
            TrustError::new(
                "qualification_prerequisite_contract_missing",
                "qualification checker context names no classified contract",
            )
        })?;
        let contract_record = self
            .seed
            .records_by_digest
            .get(&contract_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_prerequisite_contract_record_missing",
                    "classified contract is absent from the seed closure",
                )
            })?;
        if string_field(formal.value(), "target_id")? != contract.target_id
            || string_field(profile.value(), "target_id")? != contract.target_id
            || digest_field(profile.value(), "validation_contract_sha256")? != contract.digest
            || digest_field(profile.value(), "independent_basis_sha256")?
                != independent_basis.digest()
        {
            return Err(TrustError::new(
                "qualification_prerequisite_identity_mismatch",
                "qualification checker context mixes target, contract, profile, or basis identities",
            ));
        }
        let base = serde_json::json!({
            "schema": "trellis-witness-resource-demand-request/v1",
            "target_id": contract.target_id,
            "formal_refutation_sha256": formal_refutation_sha256,
            "formal_refutation": formal.value(),
            "history_summary_sha256": history_summary_sha256,
            "history_summary": history.value(),
            "validation_contract_sha256": contract.digest,
            "validation_contract": contract_record.value(),
            "independent_basis": independent_basis.value(),
            "profile": profile.value(),
        });
        let Some(demand) = witness_resource_demand else {
            return Ok(base);
        };
        let mut admissibility = base;
        admissibility["schema"] =
            Value::String("trellis-source-witness-admissibility-request/v1".to_owned());
        admissibility["witness_resource_demand"] = demand.value().clone();
        Ok(admissibility)
    }

    fn execute_qualification_record(
        &self,
        profile: &AuthoritativeRecord,
        profile_tool_field: &str,
        purpose: &str,
        schema_id: &str,
        self_digest_field: &str,
        domain: DomainTag,
        invocation: SourceToolInvocation<'_>,
    ) -> Result<AuthoritativeRecord, TrustError> {
        let expected_tool = digest_field(profile.value(), profile_tool_field)?;
        let receipt = super::execution::execute_approved_json(
            super::execution::ApprovedExecutionRequest {
                evidence: &self.evidence,
                tool_logical_id: invocation.tool_logical_id,
                purpose,
                journal_predecessor_sha256: self.journal.head().event_hash,
                execution: super::execution::PinnedExecutionRequest {
                    runner_path: invocation.runner_path,
                    expected_runner_sha256: expected_tool,
                    command_id: invocation.command_id,
                    working_directory: invocation.working_directory,
                    environment: invocation.environment,
                    input: invocation.input,
                    limits: invocation.limits,
                },
            },
        )?;
        require_successful_json_execution(receipt.value(), purpose)?;
        let mut value = receipt.parsed_output().cloned().ok_or_else(|| {
            TrustError::new(
                "qualification_checker_output_missing",
                format!("{purpose} did not return strict JSON"),
            )
        })?;
        let object = value.as_object_mut().ok_or_else(|| {
            TrustError::new(
                "qualification_checker_output_not_object",
                format!("{purpose} output must be an object"),
            )
        })?;
        for field in [
            "checker_execution_receipt",
            "checker_execution_receipt_sha256",
            self_digest_field,
        ] {
            if object.contains_key(field) {
                return Err(TrustError::new(
                    "qualification_checker_claims_kernel_field",
                    format!("{purpose} cannot author {field}"),
                ));
            }
        }
        object.insert(
            "checker_execution_receipt".to_owned(),
            receipt.value().clone(),
        );
        object.insert(
            "checker_execution_receipt_sha256".to_owned(),
            Value::String(receipt.digest().to_string()),
        );
        object.insert(
            self_digest_field.to_owned(),
            Value::String(Sha256Digest::ZERO.to_string()),
        );
        let digest = self_digest(domain, &value, self_digest_field)?;
        value[self_digest_field] = Value::String(digest.to_string());
        AuthoritativeRecord::parse_as(&self.registry, schema_id, value)
    }

    /// Restore the ephemeral selection evidence after a crash that occurred
    /// between the durable selection event and conditional generation.
    pub fn resume_qualification_selection_with_evidence(
        &mut self,
        selection: QualificationSelection,
        runtime_state: &crate::model::ProtocolState,
        formal_refutation_sha256: Sha256Digest,
        independent_basis: AuthoritativeRecord,
        witness_resource_demand: AuthoritativeRecord,
        source_witness_admissibility: AuthoritativeRecord,
        profile: AuthoritativeRecord,
    ) -> Result<(), TrustError> {
        let (conditional_theorem_candidate, conditional_proof_receipt) =
            self.checked_conditional_candidate_proof_receipt(runtime_state, &profile)?;
        let state = self
            .active_selections
            .get(&selection.profile_selection_event_hash)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_selection_not_active",
                    "cannot restore evidence for an inactive selection",
                )
            })?;
        if state.selection != selection
            || self.journal.head().event_hash != selection.profile_selection_event_hash
        {
            return Err(TrustError::new(
                "qualification_selection_not_current",
                "selection evidence can only be restored at the exact selection head",
            ));
        }
        let (history, history_event_hash, facts) = self
            .histories
            .get(&selection.history_summary_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_history_unrecorded",
                    "selection history is absent",
                )
            })?;
        let formal = self
            .formal_refutations
            .get(&formal_refutation_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_formal_refutation_unrecorded",
                    "selection formal refutation is absent",
                )
            })?;
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "qualification_contract_unclassified",
                "selection contract is absent",
            )
        })?;
        self.require_seed_profile(&profile)?;
        self.require_seed_record(&independent_basis)?;
        validate_independent_basis(&self.seed, &independent_basis)?;
        self.validate_qualification_input_execution_receipts(
            formal_refutation_sha256,
            selection.history_summary_sha256,
            &independent_basis,
            &profile,
            &witness_resource_demand,
            &source_witness_admissibility,
            &[*history_event_hash, selection.profile_selection_event_hash],
        )?;
        validate_qualification_prerequisites(&QualificationPrerequisites {
            contract,
            formal_refutation: formal,
            history_summary: history,
            history_summary_event_hash: *history_event_hash,
            history_facts: facts,
            independent_basis: &independent_basis,
            witness_resource_demand: &witness_resource_demand,
            source_witness_admissibility: &source_witness_admissibility,
            profile: &profile,
        })?;
        let target_id = state.target_id.clone();
        self.active_selections.insert(
            selection.profile_selection_event_hash,
            SelectionState {
                selection,
                target_id,
                evidence: Some(QualificationInputRecords {
                    formal_refutation_sha256,
                    independent_basis,
                    witness_resource_demand,
                    source_witness_admissibility,
                    profile,
                    conditional_theorem_candidate,
                    conditional_proof_receipt,
                }),
            },
        );
        Ok(())
    }

    pub fn execute_and_record_conditional_statement(
        &mut self,
        transaction_id: &str,
        selection: QualificationSelection,
        invocation: SourceToolInvocation<'_>,
    ) -> Result<RecordedConditionalStatement, TrustError> {
        let state = self
            .active_selections
            .get(&selection.profile_selection_event_hash)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_selection_not_active",
                    "conditional generator requires an active selection",
                )
            })?;
        let evidence = state.evidence.as_ref().ok_or_else(|| {
            TrustError::new(
                "qualification_selection_evidence_missing",
                "conditional generator requires restored prerequisite evidence",
            )
        })?;
        let generator = digest_field(
            evidence.profile.value(),
            "conditional_statement_generator_sha256",
        )?;
        let expected_input = self.qualification_context_value_for(selection, None)?;
        self.require_exact_tool_invocation(
            &invocation,
            generator,
            "trellis-conditional-statement-generator-v1",
            &expected_input,
        )?;
        let receipt = super::execution::execute_approved_json(
            super::execution::ApprovedExecutionRequest {
                evidence: &self.evidence,
                tool_logical_id: invocation.tool_logical_id,
                purpose: "conditional-statement-generator/v1",
                journal_predecessor_sha256: self.journal.head().event_hash,
                execution: super::execution::PinnedExecutionRequest {
                    runner_path: invocation.runner_path,
                    expected_runner_sha256: generator,
                    command_id: invocation.command_id,
                    working_directory: invocation.working_directory,
                    environment: invocation.environment,
                    input: invocation.input,
                    limits: invocation.limits,
                },
            },
        )?;
        require_successful_json_execution(receipt.value(), "conditional statement generator")?;
        let output = receipt.parsed_output().ok_or_else(|| {
            TrustError::new(
                "conditional_generator_output_missing",
                "conditional generator did not return strict JSON",
            )
        })?;
        let object = output.as_object().ok_or_else(|| {
            TrustError::new(
                "conditional_generator_output_not_object",
                "conditional generator output must be an object",
            )
        })?;
        if object.len() != 2
            || object.get("schema").and_then(Value::as_str)
                != Some("trellis-conditional-statement-output/v1")
        {
            return Err(TrustError::new(
                "conditional_generator_output_shape_invalid",
                "conditional generator may return only schema and statement_utf8",
            ));
        }
        let statement = object
            .get("statement_utf8")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TrustError::new(
                    "conditional_generator_statement_missing",
                    "conditional generator lacks statement_utf8",
                )
            })?
            .to_owned();
        self.record_conditional_statement(
            transaction_id,
            selection,
            generator,
            &statement,
            receipt.value().clone(),
        )
    }

    /// Record exact generated theorem bytes from a successful pinned generator
    /// receipt. The statement identity uses the conditionalization domain.
    fn record_conditional_statement(
        &mut self,
        transaction_id: &str,
        selection: QualificationSelection,
        generator_sha256: Sha256Digest,
        statement_utf8: &str,
        generator_execution_receipt: Value,
    ) -> Result<RecordedConditionalStatement, TrustError> {
        if generator_sha256 == Sha256Digest::ZERO || statement_utf8.is_empty() {
            return Err(TrustError::new(
                "conditional_statement_artifact_invalid",
                "generator digest and generated theorem bytes must be non-empty",
            ));
        }
        self.require_approved_tool_digest(
            generator_sha256,
            "conditional statement generator",
        )?;
        let receipt_digest = super::execution::validate_execution_receipt(
            &generator_execution_receipt,
            &self.evidence,
            "conditional-statement-generator/v1",
            selection.profile_selection_event_hash,
        )?;
        require_successful_json_execution(
            &generator_execution_receipt,
            "conditional statement generator",
        )?;
        require_digest(
            &generator_execution_receipt,
            "runner_sha256",
            generator_sha256,
        )?;
        let expected_output = serde_json::json!({
            "schema": "trellis-conditional-statement-output/v1",
            "statement_utf8": statement_utf8,
        });
        if generator_execution_receipt.get("parsed_stdout") != Some(&expected_output) {
            return Err(TrustError::new(
                "conditional_statement_not_exact_generator_output",
                "conditional statement differs from exact generator stdout",
            ));
        }
        let state = self
            .active_selections
            .get(&selection.profile_selection_event_hash)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_selection_not_active",
                    "conditional statement does not follow an in-process checked selection",
                )
            })?;
        if state.selection != selection
            || self.journal.head().event_hash != selection.profile_selection_event_hash
        {
            return Err(TrustError::new(
                "qualification_selection_stale_or_forged",
                "selection token differs from the current exact journal head",
            ));
        }
        let target_id = state.target_id.clone();
        let evidence = state.evidence.clone().ok_or_else(|| {
            TrustError::new(
                "qualification_selection_evidence_missing",
                "selection has no replayable prerequisite evidence",
            )
        })?;
        if digest_field(
            evidence.profile.value(),
            "conditional_statement_generator_sha256",
        )? != generator_sha256
        {
            return Err(TrustError::new(
                "conditional_generator_not_profile_pinned",
                "conditional generator differs from the selected profile",
            ));
        }
        if statement_utf8
            != string_field(
                evidence.conditional_theorem_candidate.value(),
                "statement_utf8",
            )?
        {
            return Err(TrustError::new(
                "conditional_statement_not_seed_candidate",
                "conditional generator output differs from the exact seed-frozen theorem candidate",
            ));
        }
        let statement_sha256 = tagged_hash(
            DomainTag::ConditionalizationSchema,
            statement_utf8.as_bytes(),
        );
        require_digest(
            evidence.conditional_theorem_candidate.value(),
            "conditional_statement_sha256",
            statement_sha256,
        )?;
        let envelope = serde_json::json!({
            "schema": "trellis-generated-conditional-statement/v1",
            "target_id": target_id,
            "profile_sha256": selection.profile_sha256,
            "profile_selection_event_hash": selection.profile_selection_event_hash,
            "history_summary_sha256": selection.history_summary_sha256,
            "history_summary_event_hash": selection.history_summary_event_hash,
            "generator_sha256": generator_sha256,
            "generator_execution_receipt_sha256": receipt_digest,
            "generator_execution_receipt": generator_execution_receipt,
            "statement_sha256": statement_sha256,
            "statement_utf8": statement_utf8,
            "qualification_inputs": {
                "formal_refutation_sha256": evidence.formal_refutation_sha256,
                "independent_basis": evidence.independent_basis.value(),
                "witness_resource_demand": evidence.witness_resource_demand.value(),
                "source_witness_admissibility": evidence.source_witness_admissibility.value(),
                "profile": evidence.profile.value(),
                "conditional_theorem_candidate": evidence.conditional_theorem_candidate.value(),
                "conditional_proof_receipt": evidence.conditional_proof_receipt,
            },
        });
        let head = self.append_raw_json(
            transaction_id,
            EventKind::ConditionalStatementGenerated,
            &target_id,
            &envelope,
        )?;
        let record = RecordedConditionalStatement {
            statement_sha256,
            statement_event_hash: head.event_hash,
            profile_selection_event_hash: selection.profile_selection_event_hash,
        };
        self.active_conditional_statements.insert(
            head.event_hash,
            ConditionalState {
                record,
                target_id,
                evidence,
                statement_utf8: statement_utf8.to_owned(),
            },
        );
        Ok(record)
    }

    /// Commit a checked conditional theorem without changing or replacing the
    /// already-journaled unrestricted refutation.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_and_record_qualification_bundle(
        &mut self,
        transaction_id: &str,
        selection: QualificationSelection,
        conditional: RecordedConditionalStatement,
        formal_refutation_sha256: Sha256Digest,
        independent_basis: AuthoritativeRecord,
        witness_resource_demand: AuthoritativeRecord,
        source_witness_admissibility: AuthoritativeRecord,
        profile: AuthoritativeRecord,
        invocation: SourceToolInvocation<'_>,
    ) -> Result<RecordedQualificationAttempt, TrustError> {
        self.require_seed_profile(&profile)?;
        let conditional_state = self
            .active_conditional_statements
            .get(&conditional.statement_event_hash)
            .ok_or_else(|| {
                TrustError::new(
                    "conditional_statement_not_active",
                    "qualification checker names no active conditional statement",
                )
            })?;
        if conditional_state.record != conditional
            || conditional.profile_selection_event_hash != selection.profile_selection_event_hash
            || self.journal.head().event_hash != conditional.statement_event_hash
        {
            return Err(TrustError::new(
                "conditional_statement_stale_or_forged",
                "qualification checker must immediately follow the exact active conditional statement",
            ));
        }
        let expected_input =
            self.qualification_context_value_for(selection, Some(conditional_state))?;
        let checker = digest_field(profile.value(), "conditional_proof_checker_sha256")?;
        self.require_exact_tool_invocation(
            &invocation,
            checker,
            "trellis-qualification-conditional-proof-checker-v1",
            &expected_input,
        )?;
        let receipt = super::execution::execute_approved_json(
            super::execution::ApprovedExecutionRequest {
                evidence: &self.evidence,
                tool_logical_id: invocation.tool_logical_id,
                purpose: "qualification-conditional-proof-checker/v1",
                journal_predecessor_sha256: self.journal.head().event_hash,
                execution: super::execution::PinnedExecutionRequest {
                    runner_path: invocation.runner_path,
                    expected_runner_sha256: checker,
                    command_id: invocation.command_id,
                    working_directory: invocation.working_directory,
                    environment: invocation.environment,
                    input: invocation.input,
                    limits: invocation.limits,
                },
            },
        )?;
        require_successful_json_execution(receipt.value(), "qualification proof checker")?;
        let mut value = receipt.parsed_output().cloned().ok_or_else(|| {
            TrustError::new(
                "qualification_checker_output_missing",
                "qualification proof checker did not return strict JSON",
            )
        })?;
        if value.get("schema").and_then(Value::as_str)
            == Some("trellis-qualification-proof-failure/v1")
        {
            self.registry.validate(
                "trellis://schemas/qualification-proof-failure/v1",
                &value,
            )?;
            if string_field(&value, "target_id")? != conditional_state.target_id
                || digest_field(&value, "generated_conditional_statement_sha256")?
                    != conditional.statement_sha256
            {
                return Err(TrustError::new(
                    "qualification_failure_output_binding_mismatch",
                    "checked failure output differs from the active target or conditional statement",
                ));
            }
            let event_hash = self.record_no_qualified_result_internal(
                transaction_id,
                formal_refutation_sha256,
                selection.history_summary_sha256,
                Some(FailedQualificationAttempt {
                    independent_basis,
                    witness_resource_demand,
                    source_witness_admissibility,
                    profile,
                    checked_failure_execution_receipt: receipt.value().clone(),
                    checked_failure_receipt_sha256: receipt.digest(),
                }),
            )?;
            return Ok(RecordedQualificationAttempt::NotEstablished { event_hash });
        }
        let object = value.as_object_mut().ok_or_else(|| {
            TrustError::new(
                "qualification_checker_output_not_object",
                "qualification proof checker output must be an object",
            )
        })?;
        for field in [
            "conditional_theorem_candidate_sha256",
            "conditional_proof_receipt_sha256",
            "checked_conditional_proof_sha256",
            "checker_and_axiom_closure_sha256",
            "checker_execution_receipt",
            "checker_execution_receipt_sha256",
            "journal_predecessor_sha256",
            "bundle_sha256",
        ] {
            if object.contains_key(field) {
                return Err(TrustError::new(
                    "qualification_checker_claims_kernel_field",
                    format!("qualification proof checker cannot author {field}"),
                ));
            }
        }
        let proof_receipt = &conditional_state.evidence.conditional_proof_receipt;
        validate_conditional_candidate_proof_receipt(
            &conditional_state.evidence.conditional_theorem_candidate,
            proof_receipt,
        )?;
        let proof_receipt_sha256 = digest_field(proof_receipt, "proof_receipt_sha256")?;
        object.insert(
            "conditional_theorem_candidate_sha256".to_owned(),
            Value::String(
                conditional_state
                    .evidence
                    .conditional_theorem_candidate
                    .digest()
                    .to_string(),
            ),
        );
        object.insert(
            "conditional_proof_receipt_sha256".to_owned(),
            Value::String(proof_receipt_sha256.to_string()),
        );
        object.insert(
            "checked_conditional_proof_sha256".to_owned(),
            Value::String(proof_receipt_sha256.to_string()),
        );
        object.insert(
            "checker_and_axiom_closure_sha256".to_owned(),
            Value::String(
                digest_field(proof_receipt, "checker_and_axiom_closure_sha256")?.to_string(),
            ),
        );
        object.insert(
            "checker_execution_receipt".to_owned(),
            receipt.value().clone(),
        );
        object.insert(
            "checker_execution_receipt_sha256".to_owned(),
            Value::String(receipt.digest().to_string()),
        );
        object.insert(
            "journal_predecessor_sha256".to_owned(),
            Value::String(self.journal.head().event_hash.to_string()),
        );
        object.insert(
            "bundle_sha256".to_owned(),
            Value::String(Sha256Digest::ZERO.to_string()),
        );
        let digest = self_digest(
            DomainTag::QualificationBundle,
            &value,
            "bundle_sha256",
        )?;
        value["bundle_sha256"] = Value::String(digest.to_string());
        let bundle = AuthoritativeRecord::parse(&self.registry, value)?;
        self.record_qualification_bundle(
            transaction_id,
            selection,
            conditional,
            formal_refutation_sha256,
            independent_basis,
            witness_resource_demand,
            source_witness_admissibility,
            profile,
            bundle,
        )
        .map(|(recorded, result)| RecordedQualificationAttempt::Qualified(recorded, result))
    }

    pub fn execute_and_record_active_qualification_bundle(
        &mut self,
        transaction_id: &str,
        conditional: RecordedConditionalStatement,
        invocation: SourceToolInvocation<'_>,
    ) -> Result<RecordedQualificationAttempt, TrustError> {
        let conditional_state = self
            .active_conditional_statements
            .get(&conditional.statement_event_hash)
            .cloned()
            .ok_or_else(|| {
                TrustError::new(
                    "conditional_statement_not_active",
                    "qualification checker names no active conditional statement",
                )
            })?;
        if conditional_state.record != conditional {
            return Err(TrustError::new(
                "conditional_statement_mismatch",
                "qualification checker conditional token differs from replay state",
            ));
        }
        let selection = self
            .active_selections
            .get(&conditional.profile_selection_event_hash)
            .map(|state| state.selection)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_selection_not_active",
                    "qualification checker lacks its active selection",
                )
            })?;
        let evidence = conditional_state.evidence;
        self.execute_and_record_qualification_bundle(
            transaction_id,
            selection,
            conditional,
            evidence.formal_refutation_sha256,
            evidence.independent_basis,
            evidence.witness_resource_demand,
            evidence.source_witness_admissibility,
            evidence.profile,
            invocation,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_qualification_bundle(
        &mut self,
        transaction_id: &str,
        selection: QualificationSelection,
        conditional: RecordedConditionalStatement,
        formal_refutation_sha256: Sha256Digest,
        independent_basis: AuthoritativeRecord,
        witness_resource_demand: AuthoritativeRecord,
        source_witness_admissibility: AuthoritativeRecord,
        profile: AuthoritativeRecord,
        bundle: AuthoritativeRecord,
    ) -> Result<(RecordedQualification, QualifiedResult), TrustError> {
        self.require_approved()?;
        let conditional_state = self
            .active_conditional_statements
            .get(&conditional.statement_event_hash)
            .ok_or_else(|| {
                TrustError::new(
                    "conditional_statement_not_active",
                    "qualification bundle does not follow an active generated statement",
                )
            })?;
        if conditional_state.record != conditional
            || conditional.profile_selection_event_hash != selection.profile_selection_event_hash
            || self.journal.head().event_hash != conditional.statement_event_hash
        {
            return Err(TrustError::new(
                "conditional_statement_stale_or_forged",
                "conditional statement is not the exact current journal predecessor",
            ));
        }
        let selected_evidence = &conditional_state.evidence;
        let expected_candidate = self.seed_conditional_candidate_for_profile(&profile)?;
        if selected_evidence.formal_refutation_sha256 != formal_refutation_sha256
            || selected_evidence.independent_basis != independent_basis
            || selected_evidence.witness_resource_demand != witness_resource_demand
            || selected_evidence.source_witness_admissibility != source_witness_admissibility
            || selected_evidence.profile != profile
            || selected_evidence.conditional_theorem_candidate != expected_candidate
        {
            return Err(TrustError::new(
                "qualification_inputs_changed_after_generation",
                "final qualification inputs differ from the canonical records bound by the generated statement",
            ));
        }
        let formal = self
            .formal_refutations
            .get(&formal_refutation_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_formal_refutation_unrecorded",
                    "qualification must use a checked journaled formal refutation",
                )
            })?;
        let (history, history_event_hash, facts) = self
            .histories
            .get(&selection.history_summary_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_history_unrecorded",
                    "qualification must use a generated journaled history summary",
                )
            })?;
        if *history_event_hash != selection.history_summary_event_hash
            || profile.digest() != selection.profile_sha256
        {
            return Err(TrustError::new(
                "qualification_selection_evidence_mismatch",
                "final evidence differs from the checked profile selection",
            ));
        }
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "qualification_contract_unclassified",
                "history names no classified validation contract",
            )
        })?;
        self.require_seed_profile(&profile)?;
        self.require_seed_record(&independent_basis)?;
        validate_independent_basis(&self.seed, &independent_basis)?;
        self.validate_conditional_proof_authority(selected_evidence, &bundle)?;
        self.validate_qualification_bundle_execution_receipt(
            &bundle,
            &profile,
            conditional.statement_event_hash,
            &self.qualification_context_value_for(selection, Some(conditional_state))?,
        )?;
        require_digest(
            bundle.value(),
            "journal_predecessor_sha256",
            conditional.statement_event_hash,
        )?;
        require_digest(
            bundle.value(),
            "generated_conditional_statement_sha256",
            conditional.statement_sha256,
        )?;
        let evidence = QualificationEvidence {
            contract,
            formal_refutation: formal,
            history_summary: history,
            history_summary_event_hash: *history_event_hash,
            history_facts: facts,
            independent_basis: &independent_basis,
            witness_resource_demand: &witness_resource_demand,
            source_witness_admissibility: &source_witness_admissibility,
            profile: &profile,
            bundle: &bundle,
        };
        let qualified = evaluate_qualification(&evidence)?;
        let bundle_digest = bundle.digest();
        let target_id = conditional_state.target_id.clone();
        if target_id != qualified.target_id {
            return Err(TrustError::new(
                "qualification_target_changed",
                "generated statement and checked bundle belong to different targets",
            ));
        }
        let head = self.append_record(
            transaction_id,
            EventKind::QualificationObligationsChecked,
            &target_id,
            bundle.clone(),
        )?;
        self.qualifications
            .insert(bundle_digest, (bundle, head.event_hash));
        Ok((
            RecordedQualification {
                qualification_bundle_sha256: bundle_digest,
                qualification_event_hash: head.event_hash,
                conditional_statement_sha256: conditional.statement_sha256,
            },
            qualified,
        ))
    }

    /// Classify actual-use coverage separately from conditional truth. The
    /// validator binary must be an exact leaf of the approved evidence/tool
    /// closure, and the selected profile must permit the requested status.
    pub fn record_applicability(
        &mut self,
        transaction_id: &str,
        result: AuthoritativeRecord,
    ) -> Result<Sha256Digest, TrustError> {
        self.require_approved()?;
        let predecessor = self.journal.head().event_hash;
        let (qualification_digest, target_id) =
            self.validate_applicability_record(&result, predecessor)?;
        let head = self.append_record(
            transaction_id,
            EventKind::ApplicabilityClassified,
            &target_id,
            result.clone(),
        )?;
        self.applicabilities
            .insert(qualification_digest, (result, head.event_hash));
        Ok(head.event_hash)
    }

    /// Record the conservative default when no deployment/configuration
    /// evidence has established that the conditional profile applies to an
    /// actual use.  This is a kernel derivation, not a positive validator
    /// claim, and therefore carries no `evidence_sha256`.
    pub fn record_unestablished_applicability(
        &mut self,
        transaction_id: &str,
        qualification_bundle_sha256: Sha256Digest,
    ) -> Result<Sha256Digest, TrustError> {
        self.require_approved()?;
        let (bundle, qualification_event_hash) = self
            .qualifications
            .get(&qualification_bundle_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "applicability_qualification_unrecorded",
                    "unestablished applicability names no qualification bundle",
                )
            })?;
        if self.journal.head().event_hash != *qualification_event_hash {
            return Err(TrustError::new(
                "applicability_qualification_not_current",
                "applicability must immediately follow qualification",
            ));
        }
        let profile_digest = digest_field(bundle.value(), "profile_definition_sha256")?;
        let profile = self.seed.records_by_digest.get(&profile_digest).ok_or_else(|| {
            TrustError::new(
                "applicability_profile_missing",
                "qualification profile is absent from the seed closure",
            )
        })?;
        self.require_seed_profile(profile)?;
        let validator_sha256 =
            digest_field(profile.value(), "applicability_validator_sha256")?;
        let validator_id = self.approved_evidence_logical_id_for_digest(validator_sha256)?;
        let mut value = serde_json::json!({
            "schema": "trellis-applicability-result/v1",
            "target_id": string_field(bundle.value(), "target_id")?,
            "target_statement_sha256": digest_field(bundle.value(), "target_statement_sha256")?,
            "source_claim_lineage_id": string_field(bundle.value(), "source_claim_lineage_id")?,
            "source_claim_lineage_sha256": digest_field(bundle.value(), "source_claim_lineage_sha256")?,
            "qualification_bundle_sha256": qualification_bundle_sha256,
            "condition_sha256": digest_field(bundle.value(), "condition_sha256")?,
            "status": "unestablished",
            "validator_id": validator_id,
            "validator_sha256": validator_sha256,
            "journal_predecessor_sha256": qualification_event_hash,
            "result_sha256": Sha256Digest::ZERO,
        });
        let digest = self_digest(
            DomainTag::ApplicabilityResult,
            &value,
            "result_sha256",
        )?;
        value["result_sha256"] = Value::String(digest.to_string());
        let record = AuthoritativeRecord::parse(&self.registry, value)?;
        self.record_applicability(transaction_id, record)
    }

    /// Close qualification honestly when no approved path applies or when a
    /// fully checked profile attempt failed to establish the conditional
    /// theorem. The unrestricted model refutation remains authoritative.
    pub fn record_no_qualified_result(
        &mut self,
        transaction_id: &str,
        formal_refutation_sha256: Sha256Digest,
        history_summary_sha256: Sha256Digest,
    ) -> Result<Sha256Digest, TrustError> {
        self.record_no_qualified_result_internal(
            transaction_id,
            formal_refutation_sha256,
            history_summary_sha256,
            None,
        )
    }

    fn record_no_qualified_result_internal(
        &mut self,
        transaction_id: &str,
        formal_refutation_sha256: Sha256Digest,
        history_summary_sha256: Sha256Digest,
        failed_attempt: Option<FailedQualificationAttempt>,
    ) -> Result<Sha256Digest, TrustError> {
        self.require_approved()?;
        let formal = self
            .formal_refutations
            .get(&formal_refutation_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "no_qualification_formal_refutation_unrecorded",
                    "no-qualified result must preserve a checked formal refutation",
                )
            })?;
        let (history, history_event_hash, facts) = self
            .histories
            .get(&history_summary_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "no_qualification_history_unrecorded",
                    "no-qualified result must use a generated history summary",
                )
            })?;
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "no_qualification_contract_unclassified",
                "history names no classified source-validation contract",
            )
        })?;
        let (route, reason, failed_receipt, profile_digest, failed_attempt_evidence) =
            match failed_attempt {
            Some(attempt) => {
                let conditional_predecessor = self.journal.head().event_hash;
                let conditional_state = self
                    .active_conditional_statements
                    .get(&conditional_predecessor)
                    .cloned()
                    .ok_or_else(|| {
                        TrustError::new(
                            "qualification_failure_not_after_conditional",
                            "a checked failure must immediately follow its active conditional statement",
                        )
                    })?;
                let selection_state = self
                    .active_selections
                    .get(&conditional_state.record.profile_selection_event_hash)
                    .ok_or_else(|| {
                        TrustError::new(
                            "qualification_failure_selection_missing",
                            "failed conditional proof attempt has no active checked selection",
                        )
                    })?;
                if conditional_state.target_id != contract.target_id
                    || conditional_state.evidence.formal_refutation_sha256
                        != formal_refutation_sha256
                    || conditional_state.evidence.independent_basis != attempt.independent_basis
                    || conditional_state.evidence.witness_resource_demand
                        != attempt.witness_resource_demand
                    || conditional_state.evidence.source_witness_admissibility
                        != attempt.source_witness_admissibility
                    || conditional_state.evidence.profile != attempt.profile
                    || selection_state.selection.history_summary_sha256
                        != history_summary_sha256
                    || selection_state.selection.history_summary_event_hash
                        != *history_event_hash
                {
                    return Err(TrustError::new(
                        "qualification_failure_evidence_changed",
                        "failed proof evidence differs from the exact active conditionalization inputs",
                    ));
                }
                self.require_seed_profile(&attempt.profile)?;
                self.require_seed_record(&attempt.independent_basis)?;
                validate_independent_basis(&self.seed, &attempt.independent_basis)?;
                validate_qualification_prerequisites(&QualificationPrerequisites {
                    contract,
                    formal_refutation: formal,
                    history_summary: history,
                    history_summary_event_hash: *history_event_hash,
                    history_facts: facts,
                    independent_basis: &attempt.independent_basis,
                    witness_resource_demand: &attempt.witness_resource_demand,
                    source_witness_admissibility: &attempt.source_witness_admissibility,
                    profile: &attempt.profile,
                })?;
                let failure_receipt = self
                    .validate_qualification_failure_execution_receipt(
                        &attempt.checked_failure_execution_receipt,
                        &attempt.profile,
                        conditional_predecessor,
                        &contract.target_id,
                        conditional_state.record.statement_sha256,
                        &self.qualification_context_value_for(
                            selection_state.selection,
                            Some(&conditional_state),
                        )?,
                    )?;
                if failure_receipt != attempt.checked_failure_receipt_sha256 {
                    return Err(TrustError::new(
                        "qualification_failure_receipt_digest_mismatch",
                        "failed proof receipt digest differs from its checked execution receipt",
                    ));
                }
                let evidence = serde_json::json!({
                    "independent_basis": attempt.independent_basis.value(),
                    "witness_resource_demand": attempt.witness_resource_demand.value(),
                    "source_witness_admissibility": attempt.source_witness_admissibility.value(),
                    "profile": attempt.profile.value(),
                    "conditional_statement_sha256": conditional_state.record.statement_sha256,
                    "conditional_statement_event_hash": conditional_state.record.statement_event_hash,
                    "checked_failure_execution_receipt": attempt.checked_failure_execution_receipt,
                    "checked_failure_receipt_sha256": attempt.checked_failure_receipt_sha256,
                });
                (
                    QualificationRoute::EligibleForProfileEvaluation,
                    "conditional_proof_not_established",
                    Some(attempt.checked_failure_receipt_sha256),
                    Some(attempt.profile.digest()),
                    Some(evidence),
                )
            }
            None => {
                if self.journal.head().event_hash != *history_event_hash {
                    return Err(TrustError::new(
                        "no_qualification_history_stale",
                        "a no-attempt terminal must immediately follow the complete history summary",
                    ));
                }
                let profile_available = self.seed.records_by_digest.values().any(|record| {
                    record.contract().record_schema == "trellis-qualification-profile/v1"
                        && record.value().get("target_id").and_then(Value::as_str)
                            == Some(contract.target_id.as_str())
                        && record
                            .value()
                            .get("validation_contract_sha256")
                            .and_then(Value::as_str)
                            .and_then(|value| value.parse::<Sha256Digest>().ok())
                            == Some(contract.digest)
                });
                let route = route_qualified_recovery(contract, facts, false, profile_available);
                let reason = match route {
                    QualificationRoute::ProhibitedCheckedReflection => {
                        "checked_reflection_decisively_refutes_source"
                    }
                    QualificationRoute::ProhibitedDecisiveSourceRefutation => {
                        "decisive_source_counterexample"
                    }
                    QualificationRoute::HaltSourceModelMismatch => "source_model_mismatch_halt",
                    QualificationRoute::CorrectBoundaryOrAdmissibility => {
                        "boundary_or_admissibility_correction_required"
                    }
                    QualificationRoute::ProhibitedUnsupportedClaimShape => {
                        "source_validation_not_defined_for_claim_shape"
                    }
                    QualificationRoute::ProhibitedInvalidContract => "invalid_source_contract",
                    QualificationRoute::NoIndependentQualificationBasis => {
                        "no_complete_independent_qualification_basis"
                    }
                    QualificationRoute::EligibleForProfileEvaluation => {
                        return Err(TrustError::new(
                            "eligible_qualification_closed_without_attempt",
                            "an eligible profile requires either qualification or a checked failure receipt",
                        ));
                    }
                };
                (route, reason, None, None, None)
            }
        };
        let target_id = contract.target_id.clone();
        if self.no_qualified_results.contains_key(&target_id) {
            return Err(TrustError::new(
                "duplicate_no_qualified_result",
                "target already has a terminal no-qualified result",
            ));
        }
        let value = serde_json::json!({
            "schema": "trellis-no-qualified-result/v1",
            "result_kind": "formal_refutation",
            "target_id": target_id,
            "target_statement_sha256": contract.target_statement_sha256,
            "formal_refutation_sha256": formal_refutation_sha256,
            "validation_contract_sha256": contract.digest,
            "source_claim_lineage_sha256": contract.lineage_sha256,
            "history_summary_sha256": history_summary_sha256,
            "history_summary_event_hash": history_event_hash,
            "qualification_route": format!("{route:?}"),
            "reason": reason,
            "profile_sha256": profile_digest,
            "checked_failure_receipt_sha256": failed_receipt,
            "failed_attempt_evidence": failed_attempt_evidence,
            "unrestricted_verdict": "refuted_in_extracted_model",
            "journal_predecessor_sha256": self.journal.head().event_hash,
        });
        let head = self.append_raw_json(
            transaction_id,
            EventKind::NoQualifiedResultEstablished,
            &target_id,
            &value,
        )?;
        self.no_qualified_results.insert(
            target_id,
            NoQualifiedState {
                result_kind: NegativeResultKind::FormalWitnessRefutation,
                negative_result_sha256: formal_refutation_sha256,
                history_summary_sha256,
                event_hash: head.event_hash,
                conditional_attempted: failed_receipt.is_some(),
            },
        );
        Ok(head.event_hash)
    }

    /// Close a theorem-level (non-witness) refutation after its contract's
    /// claim-specific source-validation method has reached a terminal result.
    /// Such a result can be checked reflection or an explicit unsupported
    /// classification; it can never enter witness/resource qualification.
    pub fn record_negative_proof_no_qualified_result(
        &mut self,
        transaction_id: &str,
        target_id: &str,
        history_summary_sha256: Sha256Digest,
    ) -> Result<Sha256Digest, TrustError> {
        self.require_approved()?;
        let proof = self.negative_proofs.get(target_id).ok_or_else(|| {
            TrustError::new(
                "negative_terminal_proof_missing",
                "general negative terminal requires a checked negative proof",
            )
        })?;
        if proof.unrestricted_verdict_event_hash == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "negative_terminal_verdict_missing",
                "general negative proof lacks its unrestricted verdict",
            ));
        }
        let (history, history_event_hash, facts) = self
            .histories
            .get(&history_summary_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "negative_terminal_history_missing",
                    "general negative terminal requires a checked history summary",
                )
            })?;
        if self.journal.head().event_hash != *history_event_hash {
            return Err(TrustError::new(
                "negative_terminal_history_stale",
                "general negative terminal must immediately follow its history summary",
            ));
        }
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "negative_terminal_contract_missing",
                "history names no classified source-validation contract",
            )
        })?;
        let route = route_qualified_recovery(contract, facts, false, false);
        let validation_complete = match contract.method {
            SourceValidationMethod::CheckedRefutationReflectionV1 => {
                route == QualificationRoute::ProhibitedCheckedReflection
                    && self.reflection_results.values().any(|(result, _)| {
                        result
                            .value()
                            .get("validation_contract_sha256")
                            .and_then(Value::as_str)
                            .and_then(|value| value.parse::<Sha256Digest>().ok())
                            == Some(contract.digest)
                    })
            }
            SourceValidationMethod::NotDefinedForClaimShapeV1 => {
                route == QualificationRoute::ProhibitedUnsupportedClaimShape
                    && self
                        .outcomes_by_contract
                        .get(&contract.digest)
                        .into_iter()
                        .flatten()
                        .any(|entry| {
                            entry.validated.status == ValidationStatus::NotDefinedForClaimShape
                        })
            }
            SourceValidationMethod::ExactRustExecutionV1 => false,
        };
        if contract.target_id != target_id
            || contract.target_statement_sha256 != proof.target_statement_sha256
            || !validation_complete
        {
            return Err(TrustError::new(
                "negative_terminal_source_validation_incomplete",
                "theorem-level negative proof lacks its contract-required source-validation terminal",
            ));
        }
        if self.no_qualified_results.contains_key(target_id) {
            return Err(TrustError::new(
                "duplicate_no_qualified_result",
                "target already has a terminal no-qualified result",
            ));
        }
        let proof_subject_sha256 = proof.proof_subject_sha256;
        let value = serde_json::json!({
            "schema": "trellis-no-qualified-result/v1",
            "result_kind": "checked_negative_proof",
            "target_id": target_id,
            "target_statement_sha256": contract.target_statement_sha256,
            "negative_proof_subject_sha256": proof_subject_sha256,
            "validation_contract_sha256": contract.digest,
            "source_claim_lineage_sha256": contract.lineage_sha256,
            "history_summary_sha256": history_summary_sha256,
            "history_summary_event_hash": history_event_hash,
            "qualification_route": format!("{route:?}"),
            "reason": qualification_route_reason(route)?,
            "profile_sha256": Value::Null,
            "checked_failure_receipt_sha256": Value::Null,
            "failed_attempt_evidence": Value::Null,
            "unrestricted_verdict": "refuted_in_extracted_model",
            "journal_predecessor_sha256": self.journal.head().event_hash,
        });
        let head = self.append_raw_json(
            transaction_id,
            EventKind::NoQualifiedResultEstablished,
            target_id,
            &value,
        )?;
        self.no_qualified_results.insert(
            target_id.to_owned(),
            NoQualifiedState {
                result_kind: NegativeResultKind::CheckedNegativeProof,
                negative_result_sha256: proof_subject_sha256,
                history_summary_sha256,
                event_hash: head.event_hash,
                conditional_attempted: false,
            },
        );
        Ok(head.event_hash)
    }

    /// Generate the normative four independent external claim lines from the
    /// committed records. No headline or caller-authored prose can collapse
    /// model truth, source counterevidence, conditional truth, and deployment
    /// applicability into one claim.
    pub fn generate_external_claim_rows(
        &mut self,
        transaction_id: &str,
        formal_refutation_sha256: Sha256Digest,
        history_summary_sha256: Sha256Digest,
    ) -> Result<GeneratedExternalClaims, TrustError> {
        self.require_approved()?;
        let formal = self
            .formal_refutations
            .get(&formal_refutation_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "claim_formal_refutation_unrecorded",
                    "claim rows require a checked unrestricted refutation",
                )
            })?;
        let (history, _, facts) = self
            .histories
            .get(&history_summary_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "claim_history_unrecorded",
                    "claim rows require a generated source-validation history",
                )
            })?;
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "claim_contract_unclassified",
                "history names no classified validation contract",
            )
        })?;
        if digest_field(formal.value(), "target_statement_sha256")?
            != contract.target_statement_sha256
            || string_field(formal.value(), "target_id")? != contract.target_id
        {
            return Err(TrustError::new(
                "claim_target_mismatch",
                "formal result and source history belong to different targets",
            ));
        }
        let target_id = contract.target_id.clone();
        if self.external_claim_rows.contains_key(&target_id) {
            return Err(TrustError::new(
                "duplicate_external_claim_rows",
                "target already has generated external claim rows",
            ));
        }

        let qualification = self.qualifications.values().find(|(bundle, _)| {
            bundle.value().get("target_id").and_then(Value::as_str)
                == Some(target_id.as_str())
                && bundle
                    .value()
                    .get("formal_refutation_sha256")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<Sha256Digest>().ok())
                    == Some(formal_refutation_sha256)
        });
        let (conditional_line, applicability_line, terminal_event_hash, terminal_digest) =
            if let Some((bundle, _qualification_event_hash)) = qualification {
                let applicability = self
                    .applicabilities
                    .get(&bundle.digest())
                    .ok_or_else(|| {
                        TrustError::new(
                            "qualified_claim_lacks_applicability",
                            "every qualification needs a separate applicability classification",
                        )
                    })?;
                if self.journal.head().event_hash != applicability.1 {
                    return Err(TrustError::new(
                        "qualified_claim_terminal_not_current",
                        "claim generation must immediately follow applicability classification",
                    ));
                }
                let profile_id = string_field(bundle.value(), "profile_id")?;
                let condition_sha256 = digest_field(bundle.value(), "condition_sha256")?;
                let conditional = format!(
                    "Conditional extracted-model claim under {profile_id}/{condition_sha256}: PROVED."
                );
                let status = string_field(applicability.0.value(), "status")?;
                let applicability_text = match status {
                    "formally_derived" => "FORMALLY DERIVED",
                    "mechanically_enforced" => "ENFORCED",
                    "externally_attested" => "ATTESTED",
                    "unestablished" => "UNESTABLISHED",
                    _ => {
                        return Err(TrustError::new(
                            "claim_applicability_status_invalid",
                            "unknown applicability status",
                        ))
                    }
                };
                (
                    conditional,
                    format!("Actual-use coverage of C: {applicability_text}."),
                    applicability.1,
                    bundle.digest(),
                )
            } else {
                let no_qualified = self.no_qualified_results.get(&target_id).ok_or_else(|| {
                    TrustError::new(
                        "claim_qualification_not_terminal",
                        "target has neither a qualification nor a no-qualified result",
                    )
                })?;
                if no_qualified.result_kind != NegativeResultKind::FormalWitnessRefutation
                    || no_qualified.negative_result_sha256 != formal_refutation_sha256
                    || self.journal.head().event_hash != no_qualified.event_hash
                {
                    return Err(TrustError::new(
                        "claim_no_qualification_stale",
                        "no-qualified result is stale or belongs to another refutation",
                    ));
                }
                let status = if no_qualified.conditional_attempted {
                    "NOT ESTABLISHED"
                } else {
                    "NOT ATTEMPTED"
                };
                (
                    format!(
                        "Conditional extracted-model claim under no-approved-profile/C: {status}."
                    ),
                    "Actual-use coverage of C: NOT APPLICABLE.".to_owned(),
                    no_qualified.event_hash,
                    history_summary_sha256,
                )
            };

        let source_line = match contract.method {
            SourceValidationMethod::CheckedRefutationReflectionV1 => {
                let checked = self.reflection_results.values().any(|(result, _)| {
                    result
                        .value()
                        .get("validation_contract_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(contract.digest)
                });
                if !checked {
                    return Err(TrustError::new(
                        "claim_reflection_result_missing",
                        "reflection contract has no authenticated checked result",
                    ));
                }
                "Source-counterevidence validation: checked_refutation_reflection_v1; source refutation checked."
                    .to_owned()
            }
            SourceValidationMethod::NotDefinedForClaimShapeV1 => {
                if !self
                    .outcomes_by_contract
                    .get(&contract.digest)
                    .into_iter()
                    .flatten()
                    .any(|entry| entry.validated.status == ValidationStatus::NotDefinedForClaimShape)
                {
                    return Err(TrustError::new(
                        "claim_not_defined_outcome_missing",
                        "undefined contract lacks its kernel-generated outcome",
                    ));
                }
                "Source-counterevidence validation: not_defined_for_claim_shape_v1; not defined for this claim shape."
                    .to_owned()
            }
            SourceValidationMethod::ExactRustExecutionV1 => {
                let status = self
                    .outcomes_by_contract
                    .get(&contract.digest)
                    .and_then(|entries| entries.last())
                    .map(|entry| validation_status_name(entry.validated.status))
                    .unwrap_or("no_exact_attempt_recorded");
                let dominance = if facts.decisive_source_refutation_present {
                    "qualification prohibited by decisive exact source counterexample"
                } else if facts.source_model_mismatch_unresolved {
                    "hard halt: source/model mismatch"
                } else {
                    "no decisive source counterexample recorded"
                };
                format!(
                    "Source-counterevidence validation: exact_rust_execution_v1; {status}; {dominance}."
                )
            }
        };
        let unrestricted_line = "Unrestricted extracted-model claim: REFUTED.";
        let rendered = format!(
            "{unrestricted_line}\n{source_line}\n{conditional_line}\n{applicability_line}\n"
        );
        let envelope = serde_json::json!({
            "schema": "trellis-external-claim-rows/v1",
            "result_kind": "formal_refutation",
            "target_id": target_id,
            "target_statement_sha256": contract.target_statement_sha256,
            "formal_refutation_sha256": formal_refutation_sha256,
            "history_summary_sha256": history_summary_sha256,
            "terminal_result_sha256": terminal_digest,
            "terminal_event_hash": terminal_event_hash,
            "unrestricted_extracted_model_claim": unrestricted_line,
            "source_counterevidence_validation": source_line,
            "conditional_extracted_model_claim": conditional_line,
            "actual_use_coverage": applicability_line,
            "rendered_utf8": rendered,
            "journal_predecessor_sha256": self.journal.head().event_hash,
        });
        self.validate_external_claim_envelope(
            &envelope,
            self.journal.head().event_hash,
        )?;
        let head = self.append_raw_json(
            transaction_id,
            EventKind::ExternalClaimRowsGenerated,
            &target_id,
            &envelope,
        )?;
        self.external_claim_rows.insert(
            target_id.clone(),
            ExternalClaimState {
                terminal_event_hash,
                event_hash: head.event_hash,
                event_sequence: head.sequence_number,
                envelope,
            },
        );
        Ok(GeneratedExternalClaims {
            target_id,
            rendered,
            event_hash: head.event_hash,
        })
    }

    /// Generate the four-line surface for a checked theorem-level negative
    /// proof. Source truth is reported strictly from its frozen validation
    /// method; the model proof is never described as a Rust execution.
    pub fn generate_negative_proof_external_claim_rows(
        &mut self,
        transaction_id: &str,
        target_id: &str,
        history_summary_sha256: Sha256Digest,
    ) -> Result<GeneratedExternalClaims, TrustError> {
        self.require_approved()?;
        let proof = self.negative_proofs.get(target_id).cloned().ok_or_else(|| {
            TrustError::new(
                "negative_claim_proof_missing",
                "negative claim rows require a checked negative proof",
            )
        })?;
        let (history, _, _) = self.histories.get(&history_summary_sha256).ok_or_else(|| {
            TrustError::new(
                "negative_claim_history_missing",
                "negative claim rows require a checked history summary",
            )
        })?;
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "negative_claim_contract_missing",
                "negative claim history names no classified contract",
            )
        })?;
        let terminal = self.no_qualified_results.get(target_id).ok_or_else(|| {
            TrustError::new(
                "negative_claim_terminal_missing",
                "negative claim rows require a no-qualified terminal",
            )
        })?;
        if contract.target_statement_sha256 != proof.target_statement_sha256
            || terminal.result_kind != NegativeResultKind::CheckedNegativeProof
            || terminal.negative_result_sha256 != proof.proof_subject_sha256
            || terminal.history_summary_sha256 != history_summary_sha256
            || self.journal.head().event_hash != terminal.event_hash
        {
            return Err(TrustError::new(
                "negative_claim_binding_mismatch",
                "negative claim inputs do not name one current theorem-level terminal",
            ));
        }
        if self.external_claim_rows.contains_key(target_id) {
            return Err(TrustError::new(
                "duplicate_external_claim_rows",
                "target already has generated external claim rows",
            ));
        }
        let terminal_event_hash = terminal.event_hash;
        let unrestricted = "Unrestricted extracted-model claim: REFUTED.";
        let source = match contract.method {
            SourceValidationMethod::CheckedRefutationReflectionV1 => {
                if !self.reflection_results.values().any(|(result, _)| {
                    result
                        .value()
                        .get("validation_contract_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(contract.digest)
                }) {
                    return Err(TrustError::new(
                        "negative_claim_reflection_missing",
                        "checked-reflection claim lacks its authenticated result",
                    ));
                }
                "Source-counterevidence validation: checked_refutation_reflection_v1; source refutation checked from the approved theorem-level bridge."
            }
            SourceValidationMethod::NotDefinedForClaimShapeV1 => {
                "Source-counterevidence validation: not_defined_for_claim_shape_v1; not defined for this claim shape."
            }
            SourceValidationMethod::ExactRustExecutionV1 => {
                return Err(TrustError::new(
                    "negative_claim_exact_execution_without_witness",
                    "exact Rust execution cannot consume a witness-free model proof",
                ));
            }
        };
        let conditional =
            "Conditional extracted-model claim under no-approved-profile/C: NOT ATTEMPTED.";
        let applicability = "Actual-use coverage of C: NOT APPLICABLE.";
        let rendered =
            format!("{unrestricted}\n{source}\n{conditional}\n{applicability}\n");
        let envelope = serde_json::json!({
            "schema": "trellis-external-claim-rows/v1",
            "result_kind": "checked_negative_proof",
            "target_id": target_id,
            "target_statement_sha256": proof.target_statement_sha256,
            "negative_proof_subject_sha256": proof.proof_subject_sha256,
            "history_summary_sha256": history_summary_sha256,
            "terminal_result_sha256": history_summary_sha256,
            "terminal_event_hash": terminal_event_hash,
            "unrestricted_extracted_model_claim": unrestricted,
            "source_counterevidence_validation": source,
            "conditional_extracted_model_claim": conditional,
            "actual_use_coverage": applicability,
            "rendered_utf8": rendered,
            "journal_predecessor_sha256": terminal_event_hash,
        });
        self.validate_external_claim_envelope(&envelope, terminal_event_hash)?;
        let head = self.append_raw_json(
            transaction_id,
            EventKind::ExternalClaimRowsGenerated,
            target_id,
            &envelope,
        )?;
        self.external_claim_rows.insert(
            target_id.to_owned(),
            ExternalClaimState {
                terminal_event_hash,
                event_hash: head.event_hash,
                event_sequence: head.sequence_number,
                envelope,
            },
        );
        Ok(GeneratedExternalClaims {
            target_id: target_id.to_owned(),
            rendered,
            event_hash: head.event_hash,
        })
    }

    /// Generate the same four-line external surface for a positive proof.
    /// Disproof-only source replay and qualification are explicitly marked as
    /// not applicable rather than fabricated for the opposite polarity.
    pub fn generate_positive_external_claim_rows(
        &mut self,
        transaction_id: &str,
        target_id: &str,
    ) -> Result<GeneratedExternalClaims, TrustError> {
        self.require_approved()?;
        let proof = self.positive_proofs.get(target_id).cloned().ok_or_else(|| {
            TrustError::new(
                "positive_claim_proof_missing",
                "positive claim rows require a checked positive proof",
            )
        })?;
        if proof.unrestricted_verdict_event_hash == Sha256Digest::ZERO
            || self.journal.head().event_hash != proof.unrestricted_verdict_event_hash
        {
            return Err(TrustError::new(
                "positive_claim_verdict_not_current",
                "positive claim rows must immediately follow the unrestricted verdict",
            ));
        }
        if self.external_claim_rows.contains_key(target_id) {
            return Err(TrustError::new(
                "duplicate_external_claim_rows",
                "target already has generated external claim rows",
            ));
        }
        let unrestricted = "Unrestricted extracted-model claim: PROVED.";
        let source = "Source-counterevidence validation: NOT APPLICABLE to a proved unrestricted result.";
        let conditional =
            "Conditional extracted-model claim under no-approved-profile/C: NOT ATTEMPTED.";
        let applicability = "Actual-use coverage of C: NOT APPLICABLE.";
        let rendered =
            format!("{unrestricted}\n{source}\n{conditional}\n{applicability}\n");
        let envelope = serde_json::json!({
            "schema": "trellis-external-claim-rows/v1",
            "result_kind": "positive_proof",
            "target_id": proof.target_id,
            "target_statement_sha256": proof.target_statement_sha256,
            "positive_proof_subject_sha256": proof.proof_subject_sha256,
            "terminal_result_sha256": proof.proof_subject_sha256,
            "terminal_event_hash": proof.unrestricted_verdict_event_hash,
            "unrestricted_extracted_model_claim": unrestricted,
            "source_counterevidence_validation": source,
            "conditional_extracted_model_claim": conditional,
            "actual_use_coverage": applicability,
            "rendered_utf8": rendered,
            "journal_predecessor_sha256": self.journal.head().event_hash,
        });
        self.validate_external_claim_envelope(
            &envelope,
            self.journal.head().event_hash,
        )?;
        let target = target_id.to_owned();
        let head = self.append_raw_json(
            transaction_id,
            EventKind::ExternalClaimRowsGenerated,
            &target,
            &envelope,
        )?;
        self.external_claim_rows.insert(
            target.clone(),
            ExternalClaimState {
                terminal_event_hash: proof.unrestricted_verdict_event_hash,
                event_hash: head.event_hash,
                event_sequence: head.sequence_number,
                envelope,
            },
        );
        Ok(GeneratedExternalClaims {
            target_id: target,
            rendered,
            event_hash: head.event_hash,
        })
    }

    /// Replay the semantic terminal conditions at the exact current head and
    /// produce an opaque certificate consumed by package signing.
    pub fn audit_package_readiness(&self) -> Result<PackageReadiness, TrustError> {
        self.require_approved()?;
        if self.journal.has_nonclosed_audit_authorization() {
            return Err(TrustError::new(
                "package_exceptional_revision_incomplete",
                "package authorization is forbidden while an audit authorization or revision lane is nonterminal",
            ));
        }
        let mut expected_targets = BTreeMap::new();
        for record in self.seed.records_by_digest.values().filter(|record| {
            record.contract().record_schema == "trellis-source-validation-contract/v1"
        }) {
            let contract = SourceValidationContractView::from_record(record)?;
            if expected_targets
                .insert(
                    contract.target_id.clone(),
                    (contract.target_statement_sha256, contract.digest),
                )
                .is_some()
            {
                return Err(TrustError::new(
                    "package_seed_target_contract_ambiguous",
                    format!(
                        "seed contains more than one source-validation contract for {}",
                        contract.target_id
                    ),
                ));
            }
        }
        if expected_targets.is_empty() {
            return Err(TrustError::new(
                "package_seed_has_no_targets",
                "seed closure contains no campaign target contracts",
            ));
        }
        if self
            .formal_refutations
            .len()
            .checked_add(self.positive_proofs.len())
            .and_then(|count| count.checked_add(self.negative_proofs.len()))
            != Some(expected_targets.len())
        {
            return Err(TrustError::new(
                "package_formal_target_set_incomplete",
                format!(
                    "seed requires {} target results, journal contains {} positive/refuted results",
                    expected_targets.len(),
                    self.formal_refutations.len()
                        + self.positive_proofs.len()
                        + self.negative_proofs.len()
                ),
            ));
        }
        let mut targets = BTreeSet::new();
        let mut claim_leaves = Vec::new();
        for (formal_digest, formal) in &self.formal_refutations {
            let target_id = string_field(formal.value(), "target_id")?.to_owned();
            let target_statement = digest_field(formal.value(), "target_statement_sha256")?;
            let Some((expected_statement, expected_contract_digest)) =
                expected_targets.get(&target_id)
            else {
                return Err(TrustError::new(
                    "package_formal_target_not_seed_frozen",
                    format!("target {target_id} is absent from the seed target set"),
                ));
            };
            if target_statement != *expected_statement {
                return Err(TrustError::new(
                    "package_formal_statement_not_seed_frozen",
                    format!("target {target_id} statement differs from the seed"),
                ));
            }
            if !self.unrestricted_verdicts.contains_key(formal_digest) {
                return Err(TrustError::new(
                    "package_unrestricted_verdict_missing",
                    format!("target {target_id} lacks its unrestricted verdict"),
                ));
            }
            if !targets.insert(target_id.clone()) {
                return Err(TrustError::new(
                    "package_duplicate_formal_target",
                    "v1 package must have one terminal formal refutation per target",
                ));
            }
            let contracts: Vec<_> = self
                .contracts
                .values()
                .filter(|(_, contract)| {
                    contract.target_id == target_id
                        && contract.target_statement_sha256 == target_statement
                })
                .collect();
            if contracts.len() != 1 {
                return Err(TrustError::new(
                    "package_target_contract_not_unique",
                    format!("target {target_id} has {} classified contracts", contracts.len()),
                ));
            }
            let (_, contract) = contracts[0];
            if contract.digest != *expected_contract_digest {
                return Err(TrustError::new(
                    "package_classified_contract_not_seed_target_contract",
                    format!("target {target_id} uses the wrong seed contract"),
                ));
            }
            let latest_history = self
                .histories
                .iter()
                .filter(|(_, (history, _, _))| {
                    history
                        .value()
                        .get("validation_contract_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(contract.digest)
                })
                .max_by_key(|(_, (history, _, _))| {
                    history
                        .value()
                        .get("covered_through_sequence")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                })
                .ok_or_else(|| {
                    TrustError::new(
                        "package_target_history_missing",
                        format!("target {target_id} lacks a history summary"),
                    )
                })?;
            let (history_digest, (history, _, facts)) = latest_history;
            if facts.source_model_mismatch_unresolved
                || facts.language_inadmissibility_unresolved
                || facts.contract_invalid
            {
                return Err(TrustError::new(
                    "package_target_has_unresolved_source_blocker",
                    format!("target {target_id} has a hard source-validation blocker"),
                ));
            }
            let covered_through = history
                .value()
                .get("covered_through_sequence")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    TrustError::new(
                        "package_history_coverage_invalid",
                        "history lacks covered_through_sequence",
                    )
                })?;
            if self
                .outcomes_by_contract
                .get(&contract.digest)
                .into_iter()
                .flatten()
                .any(|entry| entry.event.sequence_number > covered_through)
            {
                return Err(TrustError::new(
                    "package_history_stale",
                    format!("target {target_id} has an outcome after its summary coverage"),
                ));
            }
            match contract.method {
                SourceValidationMethod::CheckedRefutationReflectionV1
                    if !self.reflection_results.values().any(|(result, _)| {
                        result
                            .value()
                            .get("validation_contract_sha256")
                            .and_then(Value::as_str)
                            .and_then(|value| value.parse::<Sha256Digest>().ok())
                            == Some(contract.digest)
                    }) =>
                {
                    return Err(TrustError::new(
                        "package_reflection_result_missing",
                        format!("target {target_id} lacks its checked reflection result"),
                    ));
                }
                SourceValidationMethod::NotDefinedForClaimShapeV1
                    if !self
                        .outcomes_by_contract
                        .get(&contract.digest)
                        .into_iter()
                        .flatten()
                        .any(|entry| {
                            entry.validated.status == ValidationStatus::NotDefinedForClaimShape
                        }) =>
                {
                    return Err(TrustError::new(
                        "package_not_defined_result_missing",
                        format!("target {target_id} lacks its explicit undefined result"),
                    ));
                }
                _ => {}
            }
            let claim = self.external_claim_rows.get(&target_id).ok_or_else(|| {
                TrustError::new(
                    "package_external_claim_rows_missing",
                    format!("target {target_id} lacks generated claim rows"),
                )
            })?;
            if digest_field(&claim.envelope, "formal_refutation_sha256")?
                != *formal_digest
                || digest_field(&claim.envelope, "history_summary_sha256")?
                    != *history_digest
            {
                return Err(TrustError::new(
                    "package_external_claim_rows_stale",
                    format!("target {target_id} claim rows do not bind its latest records"),
                ));
            }
            self.validate_external_claim_envelope(
                &claim.envelope,
                claim.terminal_event_hash,
            )?;
            self.require_no_later_target_affecting_event(
                &target_id,
                claim.event_sequence,
            )?;
            let matching_qualifications: Vec<_> = self
                .qualifications
                .values()
                .filter(|(bundle, _)| {
                    bundle
                    .value()
                    .get("formal_refutation_sha256")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<Sha256Digest>().ok())
                    == Some(*formal_digest)
                    && bundle
                        .value()
                        .get("source_validation_history_summary_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(*history_digest)
                })
                .collect();
            let qualified_terminal = match matching_qualifications.as_slice() {
                [] => None,
                [(bundle, _)] => Some(
                    self.applicabilities
                        .get(&bundle.digest())
                        .ok_or_else(|| {
                            TrustError::new(
                                "package_qualification_lacks_applicability",
                                format!("target {target_id} qualification lacks applicability"),
                            )
                        })?
                        .1,
                ),
                _ => {
                    return Err(TrustError::new(
                        "package_duplicate_qualifications",
                        format!("target {target_id} has multiple qualification bundles"),
                    ))
                }
            };
            let no_qualified_terminal = self.no_qualified_results.get(&target_id).and_then(|state| {
                (state.result_kind == NegativeResultKind::FormalWitnessRefutation
                    && state.negative_result_sha256 == *formal_digest
                    && state.history_summary_sha256 == *history_digest)
                    .then_some(state.event_hash)
            });
            let terminal = match (qualified_terminal, no_qualified_terminal) {
                (Some(event), None) | (None, Some(event)) => event,
                _ => {
                    return Err(TrustError::new(
                        "package_qualification_terminal_ambiguous",
                        format!("target {target_id} must have exactly one qualification terminal"),
                    ))
                }
            };
            if claim.terminal_event_hash != terminal {
                return Err(TrustError::new(
                    "package_claim_terminal_mismatch",
                    format!("target {target_id} claim rows name a stale terminal"),
                ));
            }
            claim_leaves.push(serde_json::json!({
                "result_kind": "formal_refutation",
                "target_id": target_id,
                "formal_refutation_sha256": formal_digest,
                "history_summary_sha256": history_digest,
                "terminal_event_hash": terminal,
                "claim_rows_event_hash": claim.event_hash,
            }));
        }
        for (target_id, proof) in &self.negative_proofs {
            let Some((expected_statement, expected_contract_digest)) = expected_targets.get(target_id) else {
                return Err(TrustError::new(
                    "package_negative_target_not_seed_frozen",
                    format!("negative target {target_id} is absent from the seed"),
                ));
            };
            if proof.target_statement_sha256 != *expected_statement
                || proof.unrestricted_verdict_event_hash == Sha256Digest::ZERO
                || !targets.insert(target_id.clone())
            {
                return Err(TrustError::new(
                    "package_negative_result_invalid",
                    format!("negative target {target_id} is conflicting or incomplete"),
                ));
            }
            let (_, contract) = self.contracts.get(expected_contract_digest).ok_or_else(|| {
                TrustError::new(
                    "package_negative_contract_unclassified",
                    format!("negative target {target_id} lacks its classified contract"),
                )
            })?;
            if contract.method == SourceValidationMethod::ExactRustExecutionV1 {
                return Err(TrustError::new(
                    "package_negative_contract_executable",
                    format!(
                        "witness-free negative target {target_id} cannot satisfy an exact-execution contract"
                    ),
                ));
            }
            let (history_digest, (_history, _, facts)) = self
                .histories
                .iter()
                .filter(|(_, (history, _, _))| {
                    history
                        .value()
                        .get("validation_contract_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(contract.digest)
                })
                .max_by_key(|(_, (history, _, _))| {
                    history
                        .value()
                        .get("covered_through_sequence")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                })
                .ok_or_else(|| {
                    TrustError::new(
                        "package_negative_history_missing",
                        format!("negative target {target_id} lacks a history summary"),
                    )
                })?;
            let source_terminal_present = match contract.method {
                SourceValidationMethod::CheckedRefutationReflectionV1 => self
                    .reflection_results
                    .values()
                    .any(|(result, _)| {
                        result
                            .value()
                            .get("validation_contract_sha256")
                            .and_then(Value::as_str)
                            .and_then(|value| value.parse::<Sha256Digest>().ok())
                            == Some(contract.digest)
                    }),
                SourceValidationMethod::NotDefinedForClaimShapeV1 => self
                    .outcomes_by_contract
                    .get(&contract.digest)
                    .into_iter()
                    .flatten()
                    .any(|entry| {
                        entry.validated.status == ValidationStatus::NotDefinedForClaimShape
                    }),
                SourceValidationMethod::ExactRustExecutionV1 => false,
            };
            if facts.source_model_mismatch_unresolved
                || facts.language_inadmissibility_unresolved
                || facts.contract_invalid
                || !source_terminal_present
            {
                return Err(TrustError::new(
                    "package_negative_source_terminal_invalid",
                    format!("negative target {target_id} lacks its clean contract-required source-validation result"),
                ));
            }
            let terminal = self.no_qualified_results.get(target_id).ok_or_else(|| {
                TrustError::new(
                    "package_negative_terminal_missing",
                    format!("negative target {target_id} lacks its no-qualified terminal"),
                )
            })?;
            if terminal.result_kind != NegativeResultKind::CheckedNegativeProof
                || terminal.negative_result_sha256 != proof.proof_subject_sha256
                || terminal.history_summary_sha256 != *history_digest
                || terminal.conditional_attempted
            {
                return Err(TrustError::new(
                    "package_negative_terminal_stale",
                    format!("negative target {target_id} has a stale terminal"),
                ));
            }
            let claim = self.external_claim_rows.get(target_id).ok_or_else(|| {
                TrustError::new(
                    "package_negative_claim_rows_missing",
                    format!("negative target {target_id} lacks claim rows"),
                )
            })?;
            if string_field(&claim.envelope, "result_kind")? != "checked_negative_proof"
                || digest_field(&claim.envelope, "negative_proof_subject_sha256")?
                    != proof.proof_subject_sha256
                || digest_field(&claim.envelope, "history_summary_sha256")?
                    != *history_digest
                || claim.terminal_event_hash != terminal.event_hash
            {
                return Err(TrustError::new(
                    "package_negative_claim_rows_stale",
                    format!("negative target {target_id} claim rows are stale"),
                ));
            }
            self.validate_external_claim_envelope(&claim.envelope, claim.terminal_event_hash)?;
            self.require_no_later_target_affecting_event(target_id, claim.event_sequence)?;
            claim_leaves.push(serde_json::json!({
                "result_kind": "checked_negative_proof",
                "target_id": target_id,
                "negative_proof_subject_sha256": proof.proof_subject_sha256,
                "history_summary_sha256": history_digest,
                "terminal_event_hash": terminal.event_hash,
                "claim_rows_event_hash": claim.event_hash,
            }));
        }
        for (target_id, proof) in &self.positive_proofs {
            let Some((expected_statement, _)) = expected_targets.get(target_id) else {
                return Err(TrustError::new(
                    "package_positive_target_not_seed_frozen",
                    format!("positive target {target_id} is absent from the seed"),
                ));
            };
            if proof.target_statement_sha256 != *expected_statement
                || proof.unrestricted_verdict_event_hash == Sha256Digest::ZERO
                || !targets.insert(target_id.clone())
            {
                return Err(TrustError::new(
                    "package_positive_result_invalid",
                    format!("positive target {target_id} is conflicting or incomplete"),
                ));
            }
            let claim = self.external_claim_rows.get(target_id).ok_or_else(|| {
                TrustError::new(
                    "package_positive_claim_rows_missing",
                    format!("positive target {target_id} lacks claim rows"),
                )
            })?;
            if string_field(&claim.envelope, "result_kind")? != "positive_proof"
                || digest_field(&claim.envelope, "positive_proof_subject_sha256")?
                    != proof.proof_subject_sha256
                || claim.terminal_event_hash != proof.unrestricted_verdict_event_hash
            {
                return Err(TrustError::new(
                    "package_positive_claim_rows_stale",
                    format!("positive target {target_id} claim rows are stale"),
                ));
            }
            self.validate_external_claim_envelope(
                &claim.envelope,
                claim.terminal_event_hash,
            )?;
            self.require_no_later_target_affecting_event(
                target_id,
                claim.event_sequence,
            )?;
            claim_leaves.push(serde_json::json!({
                "result_kind": "positive_proof",
                "target_id": target_id,
                "positive_proof_subject_sha256": proof.proof_subject_sha256,
                "terminal_event_hash": proof.unrestricted_verdict_event_hash,
                "claim_rows_event_hash": claim.event_hash,
            }));
        }
        if targets.len() != expected_targets.len() {
            return Err(TrustError::new(
                "package_target_result_set_mismatch",
                "positive/refuted result target set differs from the seed target set",
            ));
        }
        if self.external_claim_rows.len() != targets.len() {
            return Err(TrustError::new(
                "package_has_extra_claim_rows",
                "claim-row target set differs from the formal result target set",
            ));
        }
        claim_leaves.sort_by(|left, right| {
            left["target_id"]
                .as_str()
                .unwrap_or("")
                .as_bytes()
                .cmp(right["target_id"].as_str().unwrap_or("").as_bytes())
        });
        let claim_rows_root = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&Value::Array(claim_leaves))?,
        );
        let claim_document_bytes = render_claim_document(&self.external_claim_rows)?;
        let approval = self.journal.current_approval().ok_or_else(|| {
            TrustError::new("package_approval_missing", "current approval disappeared")
        })?;
        let proof_requirements = super::package::derive_proof_requirements(
            &self.seed,
            &self.journal.committed_bundle_values()?,
        )?;
        Ok(PackageReadiness {
            journal_id: self.journal.journal_id().to_owned(),
            journal_head: self.journal.head(),
            human_approval_event_hash: approval.event_hash,
            semantic_root: self.journal.semantic_root(),
            derived_result_root: self.journal.derived_result_root(),
            approved_evidence_tool_input_root: approval.approved_evidence_tool_input_root,
            gate_presentation_sha256: approval.gate_presentation_sha256,
            target_count: targets.len(),
            claim_rows_root,
            claim_document_bytes,
            proof_requirements: Some(proof_requirements),
            expected_proof_tree_root: None,
        })
    }

    /// The only public online package-signing route. Readiness is recomputed
    /// immediately before the journal package event is committed.
    pub fn authorize_package(
        &mut self,
        archive_bytes: &[u8],
        request: PackageAuthorizationRequest<'_>,
    ) -> Result<AuthorizedPackage, TrustError> {
        let readiness = self.audit_package_readiness()?;
        super::package::authorize_package(
            &mut self.journal,
            archive_bytes,
            request,
            &readiness,
        )
    }

    pub fn build_package_archive(
        &self,
        artifacts: Vec<super::package::PackageArtifact>,
    ) -> Result<Vec<u8>, TrustError> {
        let readiness = self.audit_package_readiness()?;
        super::package::build_package_archive(
            &readiness,
            readiness.approved_evidence_tool_input_root,
            artifacts,
        )
    }

    /// Required-v1 packet builder. It obtains conditional-candidate receipts
    /// directly from the validated package-ready protocol checkpoint and
    /// reads only the exact Tablet modules named by those receipts. Mutable
    /// checker-state receipt files are intentionally outside this interface.
    pub fn build_package_archive_for_runtime(
        &self,
        state: &crate::model::ProtocolState,
        tablet_root: &Path,
        mut artifacts: Vec<super::package::PackageArtifact>,
    ) -> Result<Vec<u8>, TrustError> {
        let readiness = self.package_readiness_for_runtime(state, tablet_root)?;
        let requirements = readiness.proof_requirements.as_ref().ok_or_else(|| {
            TrustError::new(
                "package_proof_requirements_missing",
                "required-v1 readiness lacks its proof inventory",
            )
        })?;
        artifacts.extend(requirements.build_artifacts(tablet_root)?);
        super::package::build_package_archive(
            &readiness,
            readiness.approved_evidence_tool_input_root,
            artifacts,
        )
    }

    fn package_readiness_for_runtime(
        &self,
        state: &crate::model::ProtocolState,
        tablet_root: &Path,
    ) -> Result<PackageReadiness, TrustError> {
        if !state.trust_base.required()
            || !state.trust_base.package_ready
            || state.phase != crate::model::Phase::Cleanup
            || state.stage != crate::model::Stage::Start
            || state
                .trust_base
                .package_authorization_event_hash
                .is_some()
        {
            return Err(TrustError::new(
                "package_runtime_not_at_ready_barrier",
                "strict proof packaging requires the clean required-v1 package-ready barrier",
            ));
        }
        let journal_binding = self.journal.checkpoint_binding()?;
        if state.trust_base.journal_checkpoint.as_ref() != Some(&journal_binding) {
            return Err(TrustError::new(
                "package_runtime_journal_checkpoint_stale",
                "package-ready runtime checkpoint differs from the current journal head",
            ));
        }
        let mut readiness = self.audit_package_readiness()?;
        let requirements = readiness.proof_requirements.as_mut().ok_or_else(|| {
            TrustError::new(
                "package_proof_requirements_missing",
                "required-v1 readiness lacks its proof inventory",
            )
        })?;
        let mut conditional_receipts = BTreeMap::new();
        for candidate in requirements.conditional_candidates.values() {
            let profile_digest = digest_field(candidate.value(), "profile_definition_sha256")?;
            let profile = self.seed.records_by_digest.get(&profile_digest).ok_or_else(|| {
                TrustError::new(
                    "package_conditional_profile_missing",
                    "seed conditional candidate names no qualification profile",
                )
            })?;
            let (checked_candidate, receipt) =
                self.checked_conditional_candidate_proof_receipt(state, profile)?;
            if checked_candidate.digest() != candidate.digest()
                || conditional_receipts
                    .insert(candidate.digest(), receipt)
                    .is_some()
            {
                return Err(TrustError::new(
                    "package_conditional_receipt_ambiguous",
                    "runtime candidate proof differs from the seed proof inventory",
                ));
            }
        }
        requirements.set_conditional_receipts(conditional_receipts)?;
        readiness.expected_proof_tree_root = Some(requirements.current_tree_root(tablet_root)?);
        Ok(readiness)
    }

    /// Authorize only after recomputing the same candidate receipts from the
    /// current package-ready runtime checkpoint used by the strict builder.
    pub fn authorize_package_for_runtime(
        &mut self,
        state: &crate::model::ProtocolState,
        tablet_root: &Path,
        archive_bytes: &[u8],
        request: PackageAuthorizationRequest<'_>,
    ) -> Result<AuthorizedPackage, TrustError> {
        let readiness = self.package_readiness_for_runtime(state, tablet_root)?;
        super::package::authorize_package(
            &mut self.journal,
            archive_bytes,
            request,
            &readiness,
        )
    }

    fn rebuild_from_journal(&mut self) -> Result<(), TrustError> {
        let bundles = self.journal.committed_bundle_values()?;
        let cutover_sequence = match self
            .journal
            .current_approval()
            .filter(|approval| approval.revision_closure.is_some())
        {
            Some(approval) => bundles
                .iter()
                .find_map(|bundle| {
                    let event: JournalEvent =
                        serde_json::from_value(bundle.get("event")?.clone()).ok()?;
                    (event.event_hash == approval.event_hash).then_some(event.sequence_number)
                })
                .ok_or_else(|| {
                    TrustError::new(
                        "pipeline_revision_cutover_missing",
                        "current protected approval is absent from the journal",
                    )
                })?,
            None => 0,
        };
        for bundle in bundles {
            let event: JournalEvent = serde_json::from_value(bundle["event"].clone()).map_err(
                |error| TrustError::new("journal_event_decode_failed", error.to_string()),
            )?;
            if event.sequence_number <= cutover_sequence {
                continue;
            }
            if bundle.get("subject_encoding").and_then(Value::as_str) == Some("raw_base64") {
                let payload: JournalEventPayload = serde_json::from_value(
                    bundle
                        .get("payload")
                        .cloned()
                        .ok_or_else(|| {
                            TrustError::new("journal_payload_missing", "bundle lacks payload")
                        })?,
                )
                .map_err(|error| {
                    TrustError::new("journal_payload_decode_failed", error.to_string())
                })?;
                let raw = raw_json_subject(&bundle)?;
                match event.event_kind {
                    EventKind::ProofChecked => {
                        let proof_schema = string_field(&raw, "schema")?;
                        let positive = proof_schema == "trellis-checked-positive-proof/v1";
                        let negative = proof_schema == "trellis-checked-negative-proof/v1";
                        if (!positive && !negative)
                            || digest_field(&raw, "journal_predecessor_sha256")?
                                != event.previous_event_hash
                        {
                            return Err(TrustError::new(
                                "replayed_checked_proof_invalid",
                                "checked proof has an invalid schema or predecessor",
                            ));
                        }
                        let target_id = string_field(&raw, "target_id")?.to_owned();
                        let target_statement =
                            digest_field(&raw, "target_statement_sha256")?;
                        self.require_seed_target(&target_id, target_statement)?;
                        if payload.subject_id != target_id
                            || self.positive_proofs.contains_key(&target_id)
                            || self.negative_proofs.contains_key(&target_id)
                            || self.formal_refutations.values().any(|formal| {
                                formal.value().get("target_id").and_then(Value::as_str)
                                    == Some(target_id.as_str())
                            })
                        {
                            return Err(TrustError::new(
                                "replayed_checked_proof_target_conflict",
                                "checked proof subject conflicts with another target result",
                            ));
                        }
                        let polarity_fields = if positive {
                            [
                                "generated_theorem_statement_sha256",
                                "checked_proof_artifact_sha256",
                            ]
                        } else {
                            [
                                "generated_not_theorem_statement_sha256",
                                "checked_not_proof_artifact_sha256",
                            ]
                        };
                        for field in polarity_fields.into_iter().chain([
                            "checker_toolchain_sha256",
                            "approved_axiom_closure_sha256",
                            "semantic_definition_closure_sha256",
                        ]) {
                            if digest_field(&raw, field)? == Sha256Digest::ZERO {
                                return Err(TrustError::new(
                                    "replayed_checked_proof_zero_artifact",
                                    format!("{field} cannot be zero"),
                                ));
                            }
                        }
                        self.require_approved_tool_digest(
                            digest_field(&raw, "checker_toolchain_sha256")?,
                            "checked proof checker/toolchain",
                        )?;
                        let (polarity, artifact_field, statement_field) = if positive {
                            (
                                "prove",
                                "checked_proof_artifact_sha256",
                                "generated_theorem_statement_sha256",
                            )
                        } else {
                            (
                                "disprove",
                                "checked_not_proof_artifact_sha256",
                                "generated_not_theorem_statement_sha256",
                            )
                        };
                        validate_local_closure_proof_receipt(
                            raw.get("proof_receipt").ok_or_else(|| {
                                TrustError::new(
                                    "replayed_checked_proof_receipt_missing",
                                    "checked proof lacks its local-closure receipt",
                                )
                            })?,
                            &target_id,
                            polarity,
                            digest_field(&raw, artifact_field)?,
                            digest_field(&raw, statement_field)?,
                            digest_field(&raw, "checker_toolchain_sha256")?,
                            digest_field(&raw, "approved_axiom_closure_sha256")?,
                            digest_field(&raw, "semantic_definition_closure_sha256")?,
                        )?;
                        let approval = self.journal.current_human_approval_event_hash().ok_or_else(
                            || {
                                TrustError::new(
                                    "replayed_checked_proof_without_approval",
                                    "checked proof requires a current human approval",
                                )
                            },
                        )?;
                        require_digest(&raw, "human_approval_event_hash", approval)?;
                        if positive {
                            self.positive_proofs.insert(target_id.clone(), PositiveProofState {
                                target_id,
                                target_statement_sha256: target_statement,
                                proof_subject_sha256: payload.subject_sha256,
                                proof_event_hash: event.event_hash,
                                unrestricted_verdict_event_hash: Sha256Digest::ZERO,
                            });
                        } else {
                            self.negative_proofs.insert(target_id.clone(), NegativeProofState {
                                target_id,
                                target_statement_sha256: target_statement,
                                proof_subject_sha256: payload.subject_sha256,
                                proof_event_hash: event.event_hash,
                                unrestricted_verdict_event_hash: Sha256Digest::ZERO,
                                proof_envelope: raw.clone(),
                            });
                        }
                    }
                    EventKind::UnrestrictedVerdictRecorded => {
                        if string_field(&raw, "schema")?
                            != "trellis-unrestricted-verdict/v1"
                        {
                            return Err(TrustError::new(
                                "replayed_unrestricted_verdict_invalid",
                                "unrestricted verdict has an invalid schema",
                            ));
                        }
                        match string_field(&raw, "verdict")? {
                            "refuted_in_extracted_model" => {
                                if let Some(proof_digest) = optional_digest_field(
                                    &raw,
                                    "negative_proof_subject_sha256",
                                )? {
                                    let target_id = string_field(&raw, "target_id")?.to_owned();
                                    let state = self.negative_proofs.get_mut(&target_id).ok_or_else(|| {
                                        TrustError::new(
                                            "replayed_unrestricted_negative_missing",
                                            "negative verdict precedes its checked proof",
                                        )
                                    })?;
                                    if payload.subject_id != target_id
                                        || digest_field(&raw, "target_statement_sha256")?
                                            != state.target_statement_sha256
                                        || proof_digest != state.proof_subject_sha256
                                        || digest_field(&raw, "negative_proof_event_hash")?
                                            != state.proof_event_hash
                                        || event.previous_event_hash != state.proof_event_hash
                                        || state.unrestricted_verdict_event_hash
                                            != Sha256Digest::ZERO
                                    {
                                        return Err(TrustError::new(
                                            "replayed_negative_verdict_binding_mismatch",
                                            "negative verdict does not immediately bind its exact proof",
                                        ));
                                    }
                                    state.unrestricted_verdict_event_hash = event.event_hash;
                                    continue;
                                }
                                let formal_digest = digest_field(&raw, "formal_refutation_sha256")?;
                                let formal = self.formal_refutations.get(&formal_digest).ok_or_else(|| {
                                    TrustError::new(
                                        "replayed_unrestricted_formal_missing",
                                        "unrestricted verdict precedes its formal refutation",
                                    )
                                })?;
                                let target_id = string_field(formal.value(), "target_id")?;
                                if payload.subject_id != target_id
                                    || string_field(&raw, "target_id")? != target_id
                                    || digest_field(&raw, "target_statement_sha256")?
                                        != digest_field(formal.value(), "target_statement_sha256")?
                                    || digest_field(&raw, "witness_refutation_event_hash")?
                                        != event.previous_event_hash
                                {
                                    return Err(TrustError::new(
                                        "replayed_unrestricted_binding_mismatch",
                                        "unrestricted verdict is not the immediate exact consequence of its witness refutation",
                                    ));
                                }
                                if self
                                    .unrestricted_verdicts
                                    .insert(formal_digest, event.event_hash)
                                    .is_some()
                                {
                                    return Err(TrustError::new(
                                        "replayed_duplicate_unrestricted_verdict",
                                        "formal refutation has more than one unrestricted verdict",
                                    ));
                                }
                            }
                            "proved_in_extracted_model" => {
                                let target_id = string_field(&raw, "target_id")?.to_owned();
                                let state = self.positive_proofs.get_mut(&target_id).ok_or_else(|| {
                                    TrustError::new(
                                        "replayed_unrestricted_positive_missing",
                                        "positive verdict precedes its checked proof",
                                    )
                                })?;
                                if payload.subject_id != target_id
                                    || digest_field(&raw, "target_statement_sha256")?
                                        != state.target_statement_sha256
                                    || digest_field(&raw, "checked_proof_subject_sha256")?
                                        != state.proof_subject_sha256
                                    || digest_field(&raw, "checked_proof_event_hash")?
                                        != state.proof_event_hash
                                    || event.previous_event_hash != state.proof_event_hash
                                    || state.unrestricted_verdict_event_hash
                                        != Sha256Digest::ZERO
                                {
                                    return Err(TrustError::new(
                                        "replayed_positive_verdict_binding_mismatch",
                                        "positive verdict does not immediately bind its exact proof",
                                    ));
                                }
                                state.unrestricted_verdict_event_hash = event.event_hash;
                            }
                            _ => {
                                return Err(TrustError::new(
                                    "replayed_unrestricted_polarity_invalid",
                                    "unrestricted verdict has an unknown polarity",
                                ))
                            }
                        }
                    }
                    EventKind::ConditionalStatementGenerated => {
                        let state = self
                            .active_selections
                            .get(&event.previous_event_hash)
                            .cloned()
                            .ok_or_else(|| {
                                TrustError::new(
                                    "replayed_conditional_selection_missing",
                                    "generated statement does not immediately follow a checked profile selection",
                                )
                            })?;
                        if string_field(&raw, "schema")?
                            != "trellis-generated-conditional-statement/v1"
                            || payload.subject_id != state.target_id
                            || string_field(&raw, "target_id")? != state.target_id
                            || digest_field(&raw, "profile_sha256")?
                                != state.selection.profile_sha256
                            || digest_field(&raw, "profile_selection_event_hash")?
                                != state.selection.profile_selection_event_hash
                            || digest_field(&raw, "history_summary_sha256")?
                                != state.selection.history_summary_sha256
                            || digest_field(&raw, "history_summary_event_hash")?
                                != state.selection.history_summary_event_hash
                        {
                            return Err(TrustError::new(
                                "replayed_conditional_binding_mismatch",
                                "generated statement differs from its selected target, profile, or history",
                            ));
                        }
                        let generator = digest_field(&raw, "generator_sha256")?;
                        let statement = string_field(&raw, "statement_utf8")?;
                        let statement_digest = digest_field(&raw, "statement_sha256")?;
                        if generator == Sha256Digest::ZERO
                            || statement.is_empty()
                            || tagged_hash(
                                DomainTag::ConditionalizationSchema,
                                statement.as_bytes(),
                            ) != statement_digest
                        {
                            return Err(TrustError::new(
                                "replayed_conditional_artifact_invalid",
                                "generated statement bytes or generator identity are invalid",
                            ));
                        }
                        self.require_approved_tool_digest(
                            generator,
                            "conditional statement generator",
                        )?;
                        let evidence = self.parse_qualification_inputs(&raw)?;
                        self.validate_selection_evidence(&state.selection, &evidence)?;
                        if statement
                            != string_field(
                                evidence.conditional_theorem_candidate.value(),
                                "statement_utf8",
                            )?
                            || statement_digest
                                != digest_field(
                                    evidence.conditional_theorem_candidate.value(),
                                    "conditional_statement_sha256",
                                )?
                        {
                            return Err(TrustError::new(
                                "replayed_conditional_not_seed_candidate",
                                "generated statement differs from the exact seed-frozen theorem candidate",
                            ));
                        }
                        let expected_generator_input = self
                            .qualification_context_from_evidence(
                                state.selection,
                                &evidence,
                                None,
                            )?;
                        let generator_receipt = raw
                            .get("generator_execution_receipt")
                            .ok_or_else(|| {
                                TrustError::new(
                                    "replayed_conditional_generator_receipt_missing",
                                    "generated statement lacks its execution receipt",
                                )
                            })?;
                        let generator_receipt_digest =
                            super::execution::validate_execution_receipt(
                                generator_receipt,
                                &self.evidence,
                                "conditional-statement-generator/v1",
                                event.previous_event_hash,
                            )?;
                        require_successful_json_execution(
                            generator_receipt,
                            "conditional statement generator",
                        )?;
                        require_digest(generator_receipt, "runner_sha256", generator)?;
                        validate_receipt_invocation_policy(
                            generator_receipt,
                            "trellis-conditional-statement-generator-v1",
                            &expected_generator_input,
                        )?;
                        require_digest(
                            &raw,
                            "generator_execution_receipt_sha256",
                            generator_receipt_digest,
                        )?;
                        if generator_receipt.get("parsed_stdout")
                            != Some(&serde_json::json!({
                                "schema": "trellis-conditional-statement-output/v1",
                                "statement_utf8": statement,
                            }))
                        {
                            return Err(TrustError::new(
                                "replayed_conditional_not_exact_generator_output",
                                "conditional statement differs from exact generator stdout",
                            ));
                        }
                        let recorded = RecordedConditionalStatement {
                            statement_sha256: statement_digest,
                            statement_event_hash: event.event_hash,
                            profile_selection_event_hash: event.previous_event_hash,
                        };
                        self.active_conditional_statements.insert(
                            event.event_hash,
                            ConditionalState {
                                record: recorded,
                                target_id: state.target_id,
                                evidence,
                                statement_utf8: statement.to_owned(),
                            },
                        );
                    }
                    EventKind::NoQualifiedResultEstablished => {
                        if string_field(&raw, "schema")? != "trellis-no-qualified-result/v1"
                            || string_field(&raw, "unrestricted_verdict")?
                                != "refuted_in_extracted_model"
                            || digest_field(&raw, "journal_predecessor_sha256")?
                                != event.previous_event_hash
                        {
                            return Err(TrustError::new(
                                "replayed_no_qualified_result_invalid",
                                "raw no-qualified result has invalid identity or predecessor",
                            ));
                        }
                        let target_id = string_field(&raw, "target_id")?.to_owned();
                        if payload.subject_id != target_id {
                            return Err(TrustError::new(
                                "replayed_no_qualified_subject_mismatch",
                                "event subject ID differs from no-qualified target",
                            ));
                        }
                        if self.no_qualified_results.contains_key(&target_id) {
                            return Err(TrustError::new(
                                "replayed_duplicate_no_qualified_result",
                                "target has multiple no-qualified terminal results",
                            ));
                        }
                        let state = match string_field(&raw, "result_kind")? {
                            "formal_refutation" => {
                                let (formal, history, conditional_attempted) =
                                    self.validate_no_qualified_result_at(&raw, &event)?;
                                NoQualifiedState {
                                    result_kind: NegativeResultKind::FormalWitnessRefutation,
                                    negative_result_sha256: formal,
                                    history_summary_sha256: history,
                                    event_hash: event.event_hash,
                                    conditional_attempted,
                                }
                            }
                            "checked_negative_proof" => {
                                let (proof, history) = self
                                    .validate_negative_proof_no_qualified_result_at(&raw, &event)?;
                                NoQualifiedState {
                                    result_kind: NegativeResultKind::CheckedNegativeProof,
                                    negative_result_sha256: proof,
                                    history_summary_sha256: history,
                                    event_hash: event.event_hash,
                                    conditional_attempted: false,
                                }
                            }
                            _ => {
                                return Err(TrustError::new(
                                    "replayed_no_qualified_result_kind_invalid",
                                    "no-qualified result names an unknown result kind",
                                ))
                            }
                        };
                        self.no_qualified_results.insert(target_id, state);
                    }
                    EventKind::ExternalClaimRowsGenerated => {
                        if string_field(&raw, "schema")? != "trellis-external-claim-rows/v1"
                            || digest_field(&raw, "journal_predecessor_sha256")?
                                != event.previous_event_hash
                        {
                            return Err(TrustError::new(
                                "replayed_external_claim_rows_invalid",
                                "external claim rows have invalid identity or predecessor",
                            ));
                        }
                        let target_id = string_field(&raw, "target_id")?.to_owned();
                        if payload.subject_id != target_id
                            || raw
                                .get("rendered_utf8")
                                .and_then(Value::as_str)
                                .map(str::lines)
                                .map(Iterator::count)
                                != Some(4)
                        {
                            return Err(TrustError::new(
                                "replayed_external_claim_shape_invalid",
                                "claim rows must bind the subject and contain exactly four lines",
                            ));
                        }
                        self.validate_external_claim_envelope(
                            &raw,
                            event.previous_event_hash,
                        )?;
                        if self.external_claim_rows.contains_key(&target_id) {
                            return Err(TrustError::new(
                                "replayed_duplicate_external_claim_rows",
                                "target has multiple external claim-row events",
                            ));
                        }
                        self.external_claim_rows.insert(
                            target_id,
                            ExternalClaimState {
                                terminal_event_hash: digest_field(
                                    &raw,
                                    "terminal_event_hash",
                                )?,
                                event_hash: event.event_hash,
                                event_sequence: event.sequence_number,
                                envelope: raw,
                            },
                        );
                    }
                    _ => {}
                }
                continue;
            }
            let Some(value) = bundle.get("subject_json").cloned() else {
                continue;
            };
            let record = if event.event_kind == EventKind::ApprovedProfileSelected {
                AuthoritativeRecord::parse_as(
                    &self.registry,
                    "trellis-qualification-profile/v1",
                    value,
                )?
            } else {
                AuthoritativeRecord::parse(&self.registry, value)?
            };
            match event.event_kind {
                EventKind::SourceClaimLineageRegistered => {
                    let seed = self.seed.records_by_digest.get(&record.digest()).ok_or_else(|| {
                        TrustError::new(
                            "journal_lineage_not_seed_frozen",
                            "journal contains a non-seed initial lineage",
                        )
                    })?;
                    if seed != &record {
                        return Err(TrustError::new(
                            "journal_lineage_seed_mismatch",
                            "journal lineage bytes differ from seed",
                        ));
                    }
                    if payload_subject_id(&bundle)?
                        != string_field(record.value(), "lineage_id")?
                    {
                        return Err(TrustError::new(
                            "journal_lineage_subject_mismatch",
                            "lineage registration subject ID differs from the lineage record",
                        ));
                    }
                    if string_field(record.value(), "lineage_change_kind")? != "initial_seed"
                        || string_field(record.value(), "registration_epoch")? != "seed"
                        || string_field(record.value(), "registration_authority")?
                            != "seed_contract_v1"
                    {
                        return Err(TrustError::new(
                            "journal_initial_lineage_invalid",
                            "initial lineage registration is not a seed-frozen initial record",
                        ));
                    }
                    if self.lineages.contains_key(&record.digest()) {
                        return Err(TrustError::new(
                            "journal_duplicate_lineage_registration",
                            "lineage is registered more than once",
                        ));
                    }
                    self.lineage_registration.insert(record.digest(), event);
                    self.lineages.insert(record.digest(), record);
                }
                EventKind::SourceValidationClassified => {
                    let view = self.validate_contract_classification(&record)?;
                    if payload_subject_id(&bundle)? != view.contract_id {
                        return Err(TrustError::new(
                            "replayed_contract_subject_mismatch",
                            "classification subject ID differs from the contract ID",
                        ));
                    }
                    if self.contracts.contains_key(&record.digest()) {
                        return Err(TrustError::new(
                            "replayed_duplicate_contract_classification",
                            "source-validation contract is classified more than once",
                        ));
                    }
                    self.contracts.insert(record.digest(), (record, view));
                }
                EventKind::WitnessRefutationChecked => {
                    self.validate_formal_refutation(&record)?;
                    let target_id = string_field(record.value(), "target_id")?;
                    if payload_subject_id(&bundle)? != target_id
                    {
                        return Err(TrustError::new(
                            "replayed_formal_subject_mismatch",
                            "formal refutation event subject differs from its target",
                        ));
                    }
                    if self.formal_refutations.contains_key(&record.digest()) {
                        return Err(TrustError::new(
                            "replayed_duplicate_formal_refutation",
                            "formal refutation is recorded more than once",
                        ));
                    }
                    if self.positive_proofs.contains_key(target_id)
                        || self.negative_proofs.contains_key(target_id)
                        || self.formal_refutations.values().any(|prior| {
                            prior.value().get("target_id").and_then(Value::as_str)
                                == Some(target_id)
                        })
                    {
                        return Err(TrustError::new(
                            "replayed_formal_target_conflict",
                            "witness refutation conflicts with another target result",
                        ));
                    }
                    self.formal_refutations.insert(record.digest(), record);
                }
                EventKind::ReflectionValidationResultRecorded => {
                    self.validate_reflection_result_at(
                        &record,
                        event.previous_event_hash,
                        event.derived_result_root_before,
                    )?;
                    if payload_subject_id(&bundle)?
                        != string_field(record.value(), "target_id")?
                    {
                        return Err(TrustError::new(
                            "replayed_reflection_subject_mismatch",
                            "reflection event subject differs from its target",
                        ));
                    }
                    self.reflection_results
                        .insert(record.digest(), (record, event.event_hash));
                }
                EventKind::SourceValidationAttemptRecorded => {
                    let contract_digest =
                        digest_field(record.value(), "validation_contract_sha256")?;
                    let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
                        TrustError::new(
                            "replayed_attempt_contract_missing",
                            "attempt precedes its contract classification",
                        )
                    })?;
                    self.validate_source_attempt_execution_receipt(
                        contract,
                        &record,
                        event.previous_event_hash,
                    )?;
                    validate_attempt(contract, &record)?;
                    if payload_subject_id(&bundle)?
                        != string_field(record.value(), "attempt_id")?
                    {
                        return Err(TrustError::new(
                            "replayed_attempt_subject_mismatch",
                            "attempt event subject differs from its attempt ID",
                        ));
                    }
                    let digest = record.digest();
                    if self.attempts.contains_key(&digest) {
                        return Err(TrustError::new(
                            "replayed_duplicate_source_attempt",
                            "source-validation attempt is recorded more than once",
                        ));
                    }
                    self.attempts.insert(digest, record);
                }
                EventKind::SourceValidationOutcomeRecorded => {
                    let contract_digest = digest_field(record.value(), "validation_contract_sha256")?;
                    let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
                        TrustError::new(
                            "replayed_outcome_contract_missing",
                            "outcome precedes its contract classification",
                        )
                    })?;
                    let attempt = record
                        .value()
                        .get("attempt_sha256")
                        .and_then(Value::as_str)
                        .map(str::parse::<Sha256Digest>)
                        .transpose()?
                        .and_then(|digest| self.attempts.get(&digest));
                    require_digest(
                        record.value(),
                        "journal_predecessor_sha256",
                        event.previous_event_hash,
                    )?;
                    if contract.method == SourceValidationMethod::ExactRustExecutionV1 {
                        self.validate_source_outcome_execution_receipt(
                            contract,
                            &record,
                            event.previous_event_hash,
                        )?;
                    }
                    if payload_subject_id(&bundle)?
                        != string_field(record.value(), "result_id")?
                    {
                        return Err(TrustError::new(
                            "replayed_outcome_subject_mismatch",
                            "outcome event subject differs from its result ID",
                        ));
                    }
                    let validated = validate_outcome(contract, &record, attempt)?;
                    self.outcomes_by_contract
                        .entry(contract_digest)
                        .or_default()
                        .push(OutcomeEntry {
                            event: (&event).into(),
                            validated,
                        });
                }
                EventKind::SourceValidationHistorySummarized => {
                    // A new generated summary supersedes older summaries with
                    // the same digest only; freshness is checked again before
                    // qualification/package authorization.
                    let contract_digest = digest_field(record.value(), "validation_contract_sha256")?;
                    let outcomes = self
                        .outcomes_by_contract
                        .get(&contract_digest)
                        .cloned()
                        .unwrap_or_default();
                    let validated: Vec<_> =
                        outcomes.iter().map(|entry| entry.validated.clone()).collect();
                    let facts = HistoryFacts::derive(&validated);
                    if payload_subject_id(&bundle)?
                        != string_field(record.value(), "target_id")?
                    {
                        return Err(TrustError::new(
                            "replayed_history_subject_mismatch",
                            "history event subject differs from its target",
                        ));
                    }
                    self.validate_history_summary_at(&record, &event, &outcomes, &facts)?;
                    self.histories
                        .insert(record.digest(), (record, event.event_hash, facts));
                }
                EventKind::ApprovedProfileSelected => {
                    self.require_seed_profile(&record)?;
                    let (history_digest, (_, history_event_hash, _)) = self
                        .histories
                        .iter()
                        .find(|(_, (_, event_hash, _))| {
                            *event_hash == event.previous_event_hash
                        })
                        .ok_or_else(|| {
                            TrustError::new(
                                "replayed_profile_history_missing",
                                "profile selection does not immediately follow a checked history summary",
                            )
                        })?;
                    let target_id = string_field(record.value(), "target_id")?.to_owned();
                    if payload_subject_id(&bundle)? != target_id {
                        return Err(TrustError::new(
                            "replayed_profile_subject_mismatch",
                            "profile-selection subject differs from its target",
                        ));
                    }
                    let selection = QualificationSelection {
                        profile_sha256: record.digest(),
                        profile_selection_event_hash: event.event_hash,
                        history_summary_sha256: *history_digest,
                        history_summary_event_hash: *history_event_hash,
                    };
                    self.active_selections.insert(
                        event.event_hash,
                        SelectionState {
                            selection,
                            target_id,
                            evidence: None,
                        },
                    );
                }
                EventKind::QualificationObligationsChecked => {
                    self.validate_replayed_qualification(&record, &event, &bundle)?;
                    self.qualifications
                        .insert(record.digest(), (record, event.event_hash));
                }
                EventKind::ApplicabilityClassified => {
                    let (validated_qualification, target_id) = self
                        .validate_applicability_record(&record, event.previous_event_hash)?;
                    if payload_subject_id(&bundle)? != target_id {
                        return Err(TrustError::new(
                            "replayed_applicability_subject_mismatch",
                            "applicability event subject differs from its target",
                        ));
                    }
                    let qualification =
                        digest_field(record.value(), "qualification_bundle_sha256")?;
                    if qualification != validated_qualification {
                        return Err(TrustError::new(
                            "replayed_applicability_qualification_mismatch",
                            "validated applicability changed qualification identity",
                        ));
                    }
                    self.applicabilities
                        .insert(qualification, (record, event.event_hash));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn append_record(
        &mut self,
        transaction_id: &str,
        kind: EventKind,
        subject_id: &str,
        record: AuthoritativeRecord,
    ) -> Result<JournalHead, TrustError> {
        self.journal.append(AppendRequest {
            transaction_id: transaction_id.to_owned(),
            event_kind: kind,
            subject_id: subject_id.to_owned(),
            subject: Subject::CanonicalRecord(record),
            actor: JournalActor::Kernel,
            semantic_root_after: self.journal.semantic_root(),
            derived_result_root_after: Sha256Digest::ZERO,
            authorization: None,
        })
    }

    fn append_raw_json(
        &mut self,
        transaction_id: &str,
        kind: EventKind,
        subject_id: &str,
        value: &Value,
    ) -> Result<JournalHead, TrustError> {
        self.journal.append(AppendRequest {
            transaction_id: transaction_id.to_owned(),
            event_kind: kind,
            subject_id: subject_id.to_owned(),
            subject: Subject::RawArtifact(canonical_json_value(value)?),
            actor: JournalActor::Kernel,
            semantic_root_after: self.journal.semantic_root(),
            derived_result_root_after: Sha256Digest::ZERO,
            authorization: None,
        })
    }

    fn require_approved(&self) -> Result<(), TrustError> {
        let approval = self.journal.current_approval().ok_or_else(|| {
            TrustError::new(
                "derivation_before_human_approval",
                "post-gate trust derivations require the current human approval",
            )
        })?;
        if approval.approved_evidence_tool_input_root != self.evidence.evidence_tool_input_root {
            return Err(TrustError::new(
                "derivation_evidence_root_not_approved",
                "loaded evidence/tool closure differs from the current human approval",
            ));
        }
        if approval.authored_semantic_root != self.seed_authored_semantic_root {
            return Err(TrustError::new(
                "derivation_seed_root_not_currently_approved",
                "loaded seed closure differs from the current human-approved semantic root",
            ));
        }
        Ok(())
    }

    fn require_seed_target(
        &self,
        target_id: &str,
        statement: Sha256Digest,
    ) -> Result<(), TrustError> {
        let matched = self.seed.canonical_values_by_digest.values().any(|value| {
            value.get("target_id").and_then(Value::as_str) == Some(target_id)
                && value
                    .get("target_statement_sha256")
                    .and_then(Value::as_str)
                    .and_then(|item| item.parse::<Sha256Digest>().ok())
                    == Some(statement)
        });
        if !matched {
            return Err(TrustError::new(
                "formal_target_not_seed_frozen",
                "formal refutation target ID/statement is not an exact seed target",
            ));
        }
        Ok(())
    }

    fn validate_formal_refutation(
        &self,
        formal: &AuthoritativeRecord,
    ) -> Result<(), TrustError> {
        if formal.contract().record_schema != "trellis-formal-refutation/v1" {
            return Err(TrustError::new(
                "formal_refutation_wrong_schema",
                "expected a v1 formal refutation bundle",
            ));
        }
        self.require_seed_target(
            string_field(formal.value(), "target_id")?,
            digest_field(formal.value(), "target_statement_sha256")?,
        )?;
        for field in [
            "witness_term_sha256",
            "formal_predicate_sha256",
            "generated_witness_refutation_sha256",
            "witness_certificate_sha256",
            "checked_witness_proof_sha256",
            "generated_not_t_statement_sha256",
            "checked_not_t_proof_sha256",
            "checker_toolchain_sha256",
            "approved_axiom_closure_sha256",
        ] {
            if digest_field(formal.value(), field)? == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "formal_refutation_zero_artifact",
                    format!("{field} cannot use the zero sentinel"),
                ));
            }
        }
        self.require_approved_tool_digest(
            digest_field(formal.value(), "checker_toolchain_sha256")?,
            "formal refutation checker/toolchain",
        )?;
        let certificate = formal.value().get("witness_certificate").ok_or_else(|| {
            TrustError::new(
                "formal_witness_certificate_missing",
                "formal refutation lacks its full witness certificate",
            )
        })?;
        self.registry
            .validate("trellis://schemas/formal-witness-sidecar/v1", certificate)?;
        let certificate_digest = self_digest(
            DomainTag::RawArtifact,
            certificate,
            "sidecar_sha256",
        )?;
        for (field, expected) in [
            ("witness_certificate_sha256", certificate_digest),
            (
                "witness_term_sha256",
                tagged_hash(
                    DomainTag::RawArtifact,
                    &canonical_json_value(certificate.get("witness_term").ok_or_else(|| {
                        TrustError::new(
                            "formal_witness_term_missing",
                            "witness certificate lacks witness_term",
                        )
                    })?)?,
                ),
            ),
            (
                "formal_predicate_sha256",
                digest_field(certificate, "formal_predicate_sha256")?,
            ),
            (
                "generated_witness_refutation_sha256",
                digest_field(certificate, "generated_witness_refutation_sha256")?,
            ),
        ] {
            require_digest(formal.value(), field, expected)?;
        }
        require_digest(certificate, "sidecar_sha256", certificate_digest)?;
        require_digest(
            certificate,
            "target_statement_sha256",
            digest_field(formal.value(), "target_statement_sha256")?,
        )?;
        if string_field(certificate, "target_id")?
            != string_field(formal.value(), "target_id")?
        {
            return Err(TrustError::new(
                "formal_witness_target_mismatch",
                "witness certificate and formal refutation name different targets",
            ));
        }
        validate_witness_refutation_proof_receipt(formal.value())?;
        Ok(())
    }

    fn validate_contract_classification(
        &self,
        contract: &AuthoritativeRecord,
    ) -> Result<SourceValidationContractView, TrustError> {
        let seed_record = self
            .seed
            .records_by_digest
            .get(&contract.digest())
            .ok_or_else(|| {
                TrustError::new(
                    "source_contract_not_seed_frozen",
                    "source validation contract is not an exact seed definition",
                )
            })?;
        if seed_record != contract {
            return Err(TrustError::new(
                "source_contract_seed_bytes_mismatch",
                "contract bytes differ from the seed definition",
            ));
        }
        let view = SourceValidationContractView::from_record(contract)?;
        for (purpose, digest) in [
            ("source validator", view.source_validator_sha256),
            ("source observation oracle", view.observation_oracle_sha256),
            ("reflection checker", view.reflection_checker_sha256),
            (
                "source execution runner",
                view.harness_cohort_basis.as_ref().map(|basis| basis.runner_sha256),
            ),
        ] {
            if let Some(digest) = digest {
                self.require_approved_tool_digest(digest, purpose)?;
            }
        }
        for precondition in view.preconditions.values() {
            self.require_approved_tool_digest(
                precondition.checker_sha256,
                "source precondition checker",
            )?;
        }
        let lineage = self.lineages.get(&view.lineage_sha256).ok_or_else(|| {
            TrustError::new(
                "source_contract_lineage_unregistered",
                "contract's exact source lineage has no registration receipt",
            )
        })?;
        if string_field(lineage.value(), "target_id")? != view.target_id
            || digest_field(lineage.value(), "model_target_statement_sha256")?
                != view.target_statement_sha256
            || digest_field(lineage.value(), "rust_target_statement_sha256")?
                != view.rust_target_statement_sha256
        {
            return Err(TrustError::new(
                "source_contract_lineage_semantics_mismatch",
                "contract target statements differ from its registered lineage",
            ));
        }
        Ok(view)
    }

    fn require_approved_tool_digest(
        &self,
        digest: Sha256Digest,
        purpose: &str,
    ) -> Result<(), TrustError> {
        let approved = self
            .evidence
            .leaves_by_logical_id
            .values()
            .any(|leaf| leaf.raw_sha256 == digest);
        if !approved {
            return Err(TrustError::new(
                "tool_digest_not_in_approved_evidence_closure",
                format!("{purpose} digest {digest} is not in the approved closure"),
            ));
        }
        Ok(())
    }

    fn require_local_closure_evidence(
        &self,
        record: &crate::model::LocalClosureRecord,
        purpose: &str,
    ) -> Result<(), TrustError> {
        for (logical_id, field, value) in [
            ("lean-toolchain", "toolchain_hash", record.toolchain_hash.as_str()),
            (
                "lake-manifest",
                "lake_manifest_hash",
                record.lake_manifest_hash.as_str(),
            ),
            (
                "aeneas-generated-preamble",
                "preamble_hash",
                record.preamble_hash.as_str(),
            ),
            (
                "lean-checker-executable",
                "lean_executable_hash",
                record.lean_executable_hash.as_str(),
            ),
            (
                "lake-driver-executable",
                "lake_executable_hash",
                record.lake_executable_hash.as_str(),
            ),
            (
                "local-closure-checker-script",
                "checker_script_hash",
                record.checker_script_hash.as_str(),
            ),
        ] {
            let digest: Sha256Digest = value.parse().map_err(|_| {
                TrustError::new(
                    "local_closure_platform_hash_invalid",
                    format!("{purpose} {field} is not a SHA-256 digest"),
                )
            })?;
            if digest == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "local_closure_platform_hash_zero",
                    format!("{purpose} {field} cannot be zero"),
                ));
            }
            let leaf = self
                .evidence
                .leaves_by_logical_id
                .get(logical_id)
                .ok_or_else(|| {
                    TrustError::new(
                        "local_closure_platform_evidence_missing",
                        format!("approved evidence lacks {logical_id} for {purpose}"),
                    )
                })?;
            if leaf.raw_sha256 != digest {
                return Err(TrustError::new(
                    "local_closure_platform_evidence_mismatch",
                    format!(
                        "{purpose} {field} {digest} differs from approved {logical_id} {}",
                        leaf.raw_sha256
                    ),
                ));
            }
        }
        let boundary = self
            .evidence
            .leaves_by_logical_id
            .get("trusted-platform-boundary-v1")
            .ok_or_else(|| {
                TrustError::new(
                    "local_closure_trusted_platform_boundary_missing",
                    format!("approved evidence lacks the trusted-platform boundary for {purpose}"),
                )
            })?;
        if boundary.raw_sha256 == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "local_closure_trusted_platform_boundary_zero",
                "trusted-platform boundary cannot use the zero digest",
            ));
        }
        Ok(())
    }

    fn validate_source_attempt_execution_receipt(
        &self,
        contract: &SourceValidationContractView,
        attempt: &AuthoritativeRecord,
        predecessor: Sha256Digest,
    ) -> Result<(), TrustError> {
        if contract.method != SourceValidationMethod::ExactRustExecutionV1 {
            return Err(TrustError::new(
                "source_attempt_receipt_for_wrong_method",
                "only exact Rust execution can carry an execution receipt",
            ));
        }
        let receipt = attempt.value().get("execution_receipt").ok_or_else(|| {
            TrustError::new(
                "source_attempt_execution_receipt_missing",
                "exact source attempt lacks its embedded execution receipt",
            )
        })?;
        let digest = super::execution::validate_execution_receipt(
            receipt,
            &self.evidence,
            "exact-rust-source-runner/v1",
            predecessor,
        )?;
        require_successful_json_execution(receipt, "source observation oracle")?;
        let cohort = contract.harness_cohort_basis.as_ref().ok_or_else(|| {
            TrustError::new(
                "source_attempt_harness_basis_missing",
                "exact source contract lacks its harness cohort basis",
            )
        })?;
        require_digest(receipt, "runner_sha256", cohort.runner_sha256)?;
        require_digest(attempt.value(), "raw_artifact_manifest_sha256", digest)?;
        require_digest(
            attempt.value(),
            "actual_environment_sha256",
            digest_field(receipt, "actual_environment_sha256")?,
        )?;
        require_digest(
            attempt.value(),
            "command_sha256",
            digest_field(receipt, "command_sha256")?,
        )?;
        require_digest(
            attempt.value(),
            "harness_cohort_sha256",
            expected_harness_cohort_sha256(contract)?,
        )?;
        require_digest(
            attempt.value(),
            "full_tuple_sha256",
            expected_full_tuple_sha256(contract, attempt.value())?,
        )?;
        self.validate_attempt_formal_witness_binding(contract, attempt)?;
        let mut expected = attempt.value().clone();
        let object = expected.as_object_mut().ok_or_else(|| {
            TrustError::new("source_attempt_not_object", "attempt must be an object")
        })?;
        object.remove("execution_receipt");
        object.remove("raw_artifact_manifest_sha256");
        object.remove("actual_environment_sha256");
        object.remove("command_sha256");
        object.remove("harness_cohort_sha256");
        object.remove("full_tuple_sha256");
        object.remove("attempt_sha256");
        if receipt.get("parsed_stdout") != Some(&expected) {
            return Err(TrustError::new(
                "source_attempt_not_exact_runner_output",
                "attempt is not the exact kernel augmentation of runner stdout",
            ));
        }
        Ok(())
    }

    fn validate_attempt_formal_witness_binding(
        &self,
        contract: &SourceValidationContractView,
        attempt: &AuthoritativeRecord,
    ) -> Result<Sha256Digest, TrustError> {
        let value = attempt.value();
        let certificate_digest = digest_field(value, "formal_witness_certificate_sha256")?;
        let witness_term = digest_field(value, "witness_term_sha256")?;
        let predicate = digest_field(value, "formal_predicate_sha256")?;
        let matching: Vec<_> = self
            .formal_refutations
            .iter()
            .filter(|(digest, formal)| {
                self.unrestricted_verdicts.contains_key(digest)
                    && formal.value().get("target_id").and_then(Value::as_str)
                        == Some(contract.target_id.as_str())
                    && formal
                        .value()
                        .get("target_statement_sha256")
                        .and_then(Value::as_str)
                        .and_then(|item| item.parse::<Sha256Digest>().ok())
                        == Some(contract.target_statement_sha256)
                    && formal
                        .value()
                        .get("witness_certificate_sha256")
                        .and_then(Value::as_str)
                        .and_then(|item| item.parse::<Sha256Digest>().ok())
                        == Some(certificate_digest)
                    && formal
                        .value()
                        .get("witness_term_sha256")
                        .and_then(Value::as_str)
                        .and_then(|item| item.parse::<Sha256Digest>().ok())
                        == Some(witness_term)
                    && formal
                        .value()
                        .get("formal_predicate_sha256")
                        .and_then(Value::as_str)
                        .and_then(|item| item.parse::<Sha256Digest>().ok())
                        == Some(predicate)
            })
            .collect();
        if matching.len() != 1 {
            return Err(TrustError::new(
                "source_attempt_formal_witness_not_unique",
                "exact attempt must bind exactly one prior checked witness refutation",
            ));
        }
        let formal = matching[0].1;
        let certificate = formal.value().get("witness_certificate").ok_or_else(|| {
            TrustError::new(
                "source_attempt_formal_certificate_missing",
                "checked refutation lacks its embedded witness certificate",
            )
        })?;
        let descriptor = certificate
            .get("source_encoding_descriptor")
            .ok_or_else(|| {
                TrustError::new(
                    "source_attempt_descriptor_missing",
                    "witness certificate lacks a source encoding descriptor",
                )
            })?;
        require_digest(
            value,
            "encoded_input_or_descriptor_sha256",
            tagged_hash(DomainTag::RawArtifact, &canonical_json_value(descriptor)?),
        )?;
        let oracle = contract.observation_oracle_sha256.ok_or_else(|| {
            TrustError::new(
                "source_attempt_oracle_missing",
                "exact contract lacks its observation oracle",
            )
        })?;
        require_digest(value, "observation_oracle_sha256", oracle)?;
        for field in [
            "concretization_receipt_sha256",
            "erasure_receipt_sha256",
            "forbid_unsafe_compilation_receipt_sha256",
            "safe_construction_receipt_sha256",
        ] {
            if digest_field(value, field)? == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "source_attempt_zero_witness_receipt",
                    format!("{field} cannot be zero"),
                ));
            }
        }
        Ok(*matching[0].0)
    }

    fn validate_source_outcome_execution_receipt(
        &self,
        contract: &SourceValidationContractView,
        outcome: &AuthoritativeRecord,
        predecessor: Sha256Digest,
    ) -> Result<(), TrustError> {
        let receipt = outcome
            .value()
            .get("oracle_invocation_receipt")
            .ok_or_else(|| {
                TrustError::new(
                    "source_outcome_oracle_receipt_missing",
                    "exact source outcome lacks its embedded oracle receipt",
                )
            })?;
        let digest = super::execution::validate_execution_receipt(
            receipt,
            &self.evidence,
            "exact-rust-observation-oracle/v1",
            predecessor,
        )?;
        let oracle = contract.observation_oracle_sha256.ok_or_else(|| {
            TrustError::new(
                "source_outcome_oracle_unregistered",
                "exact source contract lacks its observation oracle",
            )
        })?;
        require_digest(receipt, "runner_sha256", oracle)?;
        require_digest(outcome.value(), "oracle_invocation_receipt_sha256", digest)?;
        require_digest(outcome.value(), "derivation_receipt_sha256", digest)?;
        let observation = outcome.value().get("oracle_observation").ok_or_else(|| {
            TrustError::new(
                "source_outcome_oracle_observation_missing",
                "exact outcome lacks the narrow oracle observation",
            )
        })?;
        self.registry.validate(
            "trellis://schemas/source-oracle-observation/v1",
            observation,
        )?;
        if receipt.get("parsed_stdout") != Some(observation) {
            return Err(TrustError::new(
                "source_outcome_not_exact_oracle_output",
                "embedded oracle observation is not exact checker stdout",
            ));
        }
        let attempt_digest = digest_field(observation, "attempt_sha256")?;
        require_digest(outcome.value(), "attempt_sha256", attempt_digest)?;
        let attempt = self.attempts.get(&attempt_digest).ok_or_else(|| {
            TrustError::new(
                "source_outcome_attempt_unrecorded",
                "oracle observation names no committed attempt",
            )
        })?;
        let axes = centrally_derive_source_outcome_axes(
            string_field(attempt.value(), "role")?,
            string_field(observation, "classification")?,
        )?;
        for (field, expected) in [
            ("status", axes.status),
            ("realizability", axes.realizability),
            ("observability", axes.observability),
            ("reproducibility", axes.reproducibility),
            ("decisiveness", axes.decisiveness),
        ] {
            if string_field(outcome.value(), field)? != expected {
                return Err(TrustError::new(
                    "source_outcome_axes_not_central",
                    format!("outcome differs from central derivation at {field}"),
                ));
            }
        }
        for field in [
            "raw_observation_sha256",
            "independent_basis_id",
            "independent_basis_sha256",
            "witness_resource_demand_sha256",
        ] {
            if outcome.value().get(field) != observation.get(field) {
                return Err(TrustError::new(
                    "source_outcome_observation_field_mismatch",
                    format!("outcome differs from oracle observation at {field}"),
                ));
            }
        }
        Ok(())
    }

    fn parse_qualification_inputs(
        &self,
        envelope: &Value,
    ) -> Result<QualificationInputRecords, TrustError> {
        let inputs = envelope
            .get("qualification_inputs")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_inputs_missing",
                    "generated statement lacks canonical qualification prerequisites",
                )
            })?;
        let parse = |field: &str| -> Result<AuthoritativeRecord, TrustError> {
            let value = inputs.get(field).cloned().ok_or_else(|| {
                TrustError::new(
                    "qualification_input_record_missing",
                    format!("qualification_inputs lacks {field}"),
                )
            })?;
            if field == "profile" {
                AuthoritativeRecord::parse_as(
                    &self.registry,
                    "trellis-qualification-profile/v1",
                    value,
                )
            } else {
                AuthoritativeRecord::parse(&self.registry, value)
            }
        };
        Ok(QualificationInputRecords {
            formal_refutation_sha256: digest_field(
                &Value::Object(inputs.clone()),
                "formal_refutation_sha256",
            )?,
            independent_basis: parse("independent_basis")?,
            witness_resource_demand: parse("witness_resource_demand")?,
            source_witness_admissibility: parse("source_witness_admissibility")?,
            profile: parse("profile")?,
            conditional_theorem_candidate: parse("conditional_theorem_candidate")?,
            conditional_proof_receipt: inputs
                .get("conditional_proof_receipt")
                .cloned()
                .ok_or_else(|| {
                    TrustError::new(
                        "conditional_candidate_proof_receipt_missing",
                        "qualification_inputs lacks conditional_proof_receipt",
                    )
                })?,
        })
    }

    fn validate_selection_evidence(
        &self,
        selection: &QualificationSelection,
        inputs: &QualificationInputRecords,
    ) -> Result<(), TrustError> {
        if inputs.profile.digest() != selection.profile_sha256 {
            return Err(TrustError::new(
                "qualification_input_profile_mismatch",
                "embedded profile differs from the selected profile",
            ));
        }
        let formal = self
            .formal_refutations
            .get(&inputs.formal_refutation_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_input_formal_missing",
                    "embedded inputs name no prior checked formal refutation",
                )
            })?;
        let (history, history_event_hash, facts) = self
            .histories
            .get(&selection.history_summary_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_input_history_missing",
                    "embedded inputs name no prior checked history summary",
                )
            })?;
        if *history_event_hash != selection.history_summary_event_hash {
            return Err(TrustError::new(
                "qualification_input_history_event_mismatch",
                "selected history digest and event do not belong together",
            ));
        }
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "qualification_input_contract_missing",
                "selected history names no classified contract",
            )
        })?;
        self.require_seed_profile(&inputs.profile)?;
        self.require_seed_record(&inputs.conditional_theorem_candidate)?;
        if digest_field(
            inputs.conditional_theorem_candidate.value(),
            "profile_definition_sha256",
        )? != inputs.profile.digest()
        {
            return Err(TrustError::new(
                "qualification_input_conditional_candidate_profile_mismatch",
                "embedded conditional theorem candidate differs from the selected profile",
            ));
        }
        let seed_candidate = self.seed_conditional_candidate_for_profile(&inputs.profile)?;
        if seed_candidate != inputs.conditional_theorem_candidate {
            return Err(TrustError::new(
                "qualification_input_conditional_candidate_not_seed_frozen",
                "embedded conditional theorem candidate differs from the seed closure",
            ));
        }
        validate_conditional_candidate_proof_receipt(
            &inputs.conditional_theorem_candidate,
            &inputs.conditional_proof_receipt,
        )?;
        self.require_approved_tool_digest(
            digest_field(
                &inputs.conditional_proof_receipt,
                "checker_toolchain_sha256",
            )?,
            "conditional theorem candidate checker/toolchain",
        )?;
        self.require_seed_record(&inputs.independent_basis)?;
        validate_independent_basis(&self.seed, &inputs.independent_basis)?;
        self.validate_qualification_input_execution_receipts(
            inputs.formal_refutation_sha256,
            selection.history_summary_sha256,
            &inputs.independent_basis,
            &inputs.profile,
            &inputs.witness_resource_demand,
            &inputs.source_witness_admissibility,
            &[
                selection.history_summary_event_hash,
                selection.profile_selection_event_hash,
            ],
        )?;
        validate_qualification_prerequisites(&QualificationPrerequisites {
            contract,
            formal_refutation: formal,
            history_summary: history,
            history_summary_event_hash: *history_event_hash,
            history_facts: facts,
            independent_basis: &inputs.independent_basis,
            witness_resource_demand: &inputs.witness_resource_demand,
            source_witness_admissibility: &inputs.source_witness_admissibility,
            profile: &inputs.profile,
        })
    }

    fn validate_applicability_record(
        &self,
        result: &AuthoritativeRecord,
        expected_predecessor: Sha256Digest,
    ) -> Result<(Sha256Digest, String), TrustError> {
        if result.contract().record_schema != "trellis-applicability-result/v1" {
            return Err(TrustError::new(
                "applicability_wrong_schema",
                "expected a v1 applicability result",
            ));
        }
        let qualification_digest =
            digest_field(result.value(), "qualification_bundle_sha256")?;
        let (bundle, qualification_event_hash) = self
            .qualifications
            .get(&qualification_digest)
            .ok_or_else(|| {
                TrustError::new(
                    "applicability_qualification_unrecorded",
                    "applicability must follow a checked qualification bundle",
                )
            })?;
        if expected_predecessor != *qualification_event_hash {
            return Err(TrustError::new(
                "applicability_not_immediate",
                "applicability classification must immediately follow qualification",
            ));
        }
        for field in [
            "target_id",
            "target_statement_sha256",
            "source_claim_lineage_id",
            "source_claim_lineage_sha256",
            "condition_sha256",
        ] {
            if result.value().get(field) != bundle.value().get(field) {
                return Err(TrustError::new(
                    "applicability_qualification_binding_mismatch",
                    format!("applicability differs from qualification at {field}"),
                ));
            }
        }
        require_digest(
            result.value(),
            "journal_predecessor_sha256",
            *qualification_event_hash,
        )?;
        let profile_digest = digest_field(bundle.value(), "profile_definition_sha256")?;
        let profile = self.seed.records_by_digest.get(&profile_digest).ok_or_else(|| {
            TrustError::new(
                "applicability_profile_missing",
                "qualification profile is absent from the seed closure",
            )
        })?;
        self.require_seed_profile(profile)?;
        let status = string_field(result.value(), "status")?;
        let permitted = profile
            .value()
            .get("permitted_applicability")
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(status)));
        if !permitted {
            return Err(TrustError::new(
                "applicability_status_not_profile_permitted",
                format!("profile does not permit {status}"),
            ));
        }
        if status == "externally_attested" {
            return Err(TrustError::new(
                "external_applicability_attestation_not_verified",
                "v1 requires a separately rooted attestation verifier before this status is usable",
            ));
        }
        let validator_id = string_field(result.value(), "validator_id")?;
        let validator_sha256 = digest_field(result.value(), "validator_sha256")?;
        let leaf = self
            .evidence
            .leaves_by_logical_id
            .get(validator_id)
            .ok_or_else(|| {
                TrustError::new(
                    "applicability_validator_not_approved",
                    "validator ID is absent from the approved evidence/tool closure",
                )
            })?;
        if leaf.raw_sha256 != validator_sha256 {
            return Err(TrustError::new(
                "applicability_validator_digest_mismatch",
                "validator bytes differ from the approved closure leaf",
            ));
        }
        match status {
            "unestablished" if result.value().get("evidence_sha256").is_some() => {
                return Err(TrustError::new(
                    "unestablished_applicability_has_evidence_claim",
                    "unestablished status cannot imply an evidence-backed guarantee",
                ));
            }
            "unestablished" => {}
            _ if digest_field(result.value(), "evidence_sha256")? == Sha256Digest::ZERO => {
                return Err(TrustError::new(
                    "applicability_evidence_zero",
                    "established applicability needs content-bound nonzero evidence",
                ));
            }
            _ => {}
        }
        Ok((
            qualification_digest,
            string_field(result.value(), "target_id")?.to_owned(),
        ))
    }

    fn validate_history_summary_at(
        &self,
        summary: &AuthoritativeRecord,
        event: &JournalEvent,
        outcomes: &[OutcomeEntry],
        facts: &HistoryFacts,
    ) -> Result<(), TrustError> {
        if summary.contract().record_schema
            != "trellis-source-validation-history-summary/v1"
        {
            return Err(TrustError::new(
                "history_summary_wrong_schema",
                "expected a v1 source-validation history summary",
            ));
        }
        let contract_digest =
            digest_field(summary.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "history_contract_unknown",
                "history summary names no prior classified contract",
            )
        })?;
        for (field, expected) in [
            ("target_id", contract.target_id.as_str()),
            ("validation_contract_id", contract.contract_id.as_str()),
            ("source_claim_lineage_id", contract.lineage_id.as_str()),
        ] {
            if string_field(summary.value(), field)? != expected {
                return Err(TrustError::new(
                    "history_summary_identity_mismatch",
                    format!("history summary differs from its contract at {field}"),
                ));
            }
        }
        for (field, expected) in [
            ("target_statement_sha256", contract.target_statement_sha256),
            ("validation_contract_sha256", contract.digest),
            ("source_claim_lineage_sha256", contract.lineage_sha256),
            ("covered_through_event_hash", event.previous_event_hash),
        ] {
            require_digest(summary.value(), field, expected)?;
        }
        let registration = self
            .lineage_registration
            .get(&contract.lineage_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "history_lineage_unregistered",
                    "history summary lineage has no registration receipt",
                )
            })?;
        let predecessor_sequence = event.sequence_number.checked_sub(1).ok_or_else(|| {
            TrustError::new(
                "history_event_sequence_invalid",
                "history summary cannot occur at genesis",
            )
        })?;
        for (field, expected) in [
            ("lineage_registration_sequence", registration.sequence_number),
            ("covered_from_sequence", registration.sequence_number),
            ("covered_through_sequence", predecessor_sequence),
        ] {
            if summary.value().get(field).and_then(Value::as_u64) != Some(expected) {
                return Err(TrustError::new(
                    "history_summary_coverage_mismatch",
                    format!("history summary differs at {field}"),
                ));
            }
        }
        if string_field(summary.value(), "covered_journal_id")? != event.journal_id {
            return Err(TrustError::new(
                "history_summary_journal_mismatch",
                "history summary belongs to another journal",
            ));
        }
        let mut latest: Vec<_> = facts.latest_by_role.values().copied().collect();
        latest.sort();
        latest.dedup();
        let declared_latest: Vec<_> = summary
            .value()
            .get("latest_result_hashes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TrustError::new(
                    "history_latest_results_missing",
                    "history summary lacks latest_result_hashes",
                )
            })?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| {
                        TrustError::new(
                            "history_latest_result_invalid",
                            "latest result digest must be a string",
                        )
                    })?
                    .parse::<Sha256Digest>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        if declared_latest != latest {
            return Err(TrustError::new(
                "history_latest_results_mismatch",
                "history summary does not contain the centrally derived latest results",
            ));
        }
        let expected_blockers = Value::Array(derive_cohort_blockers(outcomes));
        if summary.value().get("cohort_blockers") != Some(&expected_blockers) {
            return Err(TrustError::new(
                "history_cohort_blockers_mismatch",
                "history summary cohort blockers were not centrally derived",
            ));
        }
        validate_summary_flags(summary, facts)?;
        let coverage_events: Vec<_> = outcomes
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "sequence_number": entry.event.sequence_number,
                    "event_hash": entry.event.event_hash,
                    "result_sha256": entry.validated.digest,
                })
            })
            .collect();
        require_digest(
            summary.value(),
            "coverage_root_sha256",
            tagged_hash(
                DomainTag::ManifestNode,
                &canonical_json_value(&Value::Array(coverage_events))?,
            ),
        )?;
        let predecessor = JournalHead {
            journal_id: event.journal_id.clone(),
            sequence_number: predecessor_sequence,
            event_hash: event.previous_event_hash,
        };
        require_digest(
            summary.value(),
            "journal_head_sha256",
            tagged_hash(
                DomainTag::ManifestNode,
                &canonical_json_value(&serde_json::to_value(predecessor).map_err(|error| {
                    TrustError::new("journal_head_encode_failed", error.to_string())
                })?)?,
            ),
        )?;
        if string_field(summary.value(), "summarizer_id")?.is_empty()
            || digest_field(summary.value(), "summarizer_sha256")? == Sha256Digest::ZERO
            || digest_field(summary.value(), "summary_derivation_sha256")?
                != summary_derivation(summary.value())?
        {
            return Err(TrustError::new(
                "history_summary_derivation_invalid",
                "history summary lacks an exact non-placeholder central derivation",
            ));
        }
        self.require_approved_tool_digest(
            digest_field(summary.value(), "summarizer_sha256")?,
            "source history summarizer",
        )?;
        Ok(())
    }

    fn validate_reflection_result_at(
        &self,
        result: &AuthoritativeRecord,
        expected_predecessor: Sha256Digest,
        approval_closure: Sha256Digest,
    ) -> Result<Sha256Digest, TrustError> {
        if result.contract().record_schema != "trellis-reflection-validation-result/v1" {
            return Err(TrustError::new(
                "reflection_result_wrong_schema",
                "expected a v1 reflection validation result",
            ));
        }
        let contract_digest =
            digest_field(result.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "reflection_contract_unclassified",
                "reflection result names no classified contract",
            )
        })?;
        if contract.method != SourceValidationMethod::CheckedRefutationReflectionV1 {
            return Err(TrustError::new(
                "reflection_result_for_wrong_method",
                "contract does not authorize checked theorem reflection",
            ));
        }
        let checked_model = digest_field(result.value(), "checked_model_refutation_sha256")?;
        let mut matching_negative_results = self
            .formal_refutations
            .iter()
            .filter_map(|(digest, formal)| {
                (self.unrestricted_verdicts.contains_key(digest)
                    && formal.value().get("target_id").and_then(Value::as_str)
                        == Some(contract.target_id.as_str())
                    && formal
                        .value()
                        .get("target_statement_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(contract.target_statement_sha256)
                    && formal
                        .value()
                        .get("checked_not_t_proof_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(checked_model))
                .then_some(*digest)
            })
            .collect::<Vec<_>>();
        matching_negative_results.extend(self.negative_proofs.values().filter_map(|proof| {
            (proof.target_id == contract.target_id
                && proof.target_statement_sha256 == contract.target_statement_sha256
                && proof.unrestricted_verdict_event_hash != Sha256Digest::ZERO
                && proof
                    .proof_envelope
                    .get("checked_not_proof_artifact_sha256")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<Sha256Digest>().ok())
                    == Some(checked_model))
            .then_some(proof.proof_subject_sha256)
        }));
        if matching_negative_results.len() != 1 {
            return Err(TrustError::new(
                "reflection_model_refutation_not_unique",
                "reflection must bind exactly one prior unrestricted model negative result",
            ));
        }
        let model_negative_result_sha256 = matching_negative_results[0];
        for (field, expected) in [
            ("source_claim_lineage_sha256", contract.lineage_sha256),
            ("validation_contract_sha256", contract.digest),
            (
                "rust_target_statement_sha256",
                contract.rust_target_statement_sha256,
            ),
            (
                "model_target_statement_sha256",
                contract.target_statement_sha256,
            ),
            (
                "generated_implication_statement_sha256",
                contract.reflection_theorem_sha256.ok_or_else(|| {
                    TrustError::new(
                        "reflection_contract_theorem_missing",
                        "reflection contract lacks its frozen implication",
                    )
                })?,
            ),
            (
                "checked_reflection_proof_sha256",
                contract.reflection_proof_artifact_sha256.ok_or_else(|| {
                    TrustError::new(
                        "reflection_contract_proof_missing",
                        "reflection contract lacks its frozen proof artifact",
                    )
                })?,
            ),
            (
                "checker_toolchain_sha256",
                contract.reflection_checker_sha256.ok_or_else(|| {
                    TrustError::new(
                        "reflection_contract_checker_missing",
                        "reflection contract lacks its seed-pinned checker",
                    )
                })?,
            ),
            ("checked_model_refutation_sha256", checked_model),
            ("approval_closure_sha256", approval_closure),
            ("journal_predecessor_sha256", expected_predecessor),
        ] {
            require_digest(result.value(), field, expected)?;
        }
        for (field, expected) in [
            ("target_id", contract.target_id.as_str()),
            ("source_claim_lineage_id", contract.lineage_id.as_str()),
        ] {
            if string_field(result.value(), field)? != expected {
                return Err(TrustError::new(
                    "reflection_result_identity_mismatch",
                    format!("reflection result differs at {field}"),
                ));
            }
        }
        for field in [
            "checker_toolchain_sha256",
            "approved_axiom_closure_sha256",
            "generated_source_refutation_statement_sha256",
            "checked_source_refutation_proof_sha256",
        ] {
            if digest_field(result.value(), field)? == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "reflection_zero_checked_artifact",
                    format!("{field} cannot be zero"),
                ));
            }
        }
        self.require_approved_tool_digest(
            digest_field(result.value(), "checker_toolchain_sha256")?,
            "reflection checker/toolchain",
        )?;
        validate_reflection_execution_receipt(
            result.value(),
            &self.evidence,
            digest_field(result.value(), "checker_toolchain_sha256")?,
            expected_predecessor,
        )?;
        Ok(model_negative_result_sha256)
    }

    fn validate_replayed_qualification(
        &self,
        bundle_record: &AuthoritativeRecord,
        event: &JournalEvent,
        event_bundle: &Value,
    ) -> Result<(), TrustError> {
        if bundle_record.contract().record_schema != "trellis-qualification-bundle/v1" {
            return Err(TrustError::new(
                "replayed_qualification_wrong_schema",
                "qualification event must contain a v1 qualification bundle",
            ));
        }
        let conditional = self
            .active_conditional_statements
            .get(&event.previous_event_hash)
            .ok_or_else(|| {
                TrustError::new(
                    "replayed_qualification_conditional_missing",
                    "qualification does not immediately follow its generated statement",
                )
            })?;
        if payload_subject_id(event_bundle)? != conditional.target_id {
            return Err(TrustError::new(
                "replayed_qualification_subject_mismatch",
                "qualification event subject differs from the generated target",
            ));
        }
        require_digest(
            bundle_record.value(),
            "journal_predecessor_sha256",
            event.previous_event_hash,
        )?;
        require_digest(
            bundle_record.value(),
            "generated_conditional_statement_sha256",
            conditional.record.statement_sha256,
        )?;
        let selection = self
            .active_selections
            .get(&conditional.record.profile_selection_event_hash)
            .ok_or_else(|| {
                TrustError::new(
                    "replayed_qualification_selection_missing",
                    "qualification's generated statement names no checked selection",
                )
            })?;
        self.validate_selection_evidence(&selection.selection, &conditional.evidence)?;
        self.validate_conditional_proof_authority(
            &conditional.evidence,
            bundle_record,
        )?;
        self.validate_qualification_bundle_execution_receipt(
            bundle_record,
            &conditional.evidence.profile,
            event.previous_event_hash,
            &self.qualification_context_from_evidence(
                selection.selection,
                &conditional.evidence,
                Some(conditional),
            )?,
        )?;
        let formal = self
            .formal_refutations
            .get(&conditional.evidence.formal_refutation_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "replayed_qualification_formal_missing",
                    "qualification evidence names no formal refutation",
                )
            })?;
        let (history, history_event_hash, facts) = self
            .histories
            .get(&selection.selection.history_summary_sha256)
            .ok_or_else(|| {
                TrustError::new(
                    "replayed_qualification_history_missing",
                    "qualification evidence names no history summary",
                )
            })?;
        let contract_digest =
            digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "replayed_qualification_contract_missing",
                "qualification history names no classified contract",
            )
        })?;
        let qualified = evaluate_qualification(&QualificationEvidence {
            contract,
            formal_refutation: formal,
            history_summary: history,
            history_summary_event_hash: *history_event_hash,
            history_facts: facts,
            independent_basis: &conditional.evidence.independent_basis,
            witness_resource_demand: &conditional.evidence.witness_resource_demand,
            source_witness_admissibility: &conditional.evidence.source_witness_admissibility,
            profile: &conditional.evidence.profile,
            bundle: bundle_record,
        })?;
        if qualified.target_id != conditional.target_id {
            return Err(TrustError::new(
                "replayed_qualification_target_changed",
                "qualification result belongs to another target",
            ));
        }
        Ok(())
    }

    fn validate_no_qualified_result_at(
        &self,
        raw: &Value,
        event: &JournalEvent,
    ) -> Result<(Sha256Digest, Sha256Digest, bool), TrustError> {
        if string_field(raw, "result_kind")? != "formal_refutation" {
            return Err(TrustError::new(
                "replayed_no_qualified_result_kind_mismatch",
                "witness qualification terminal must name formal_refutation",
            ));
        }
        let formal_digest = digest_field(raw, "formal_refutation_sha256")?;
        let formal = self.formal_refutations.get(&formal_digest).ok_or_else(|| {
            TrustError::new(
                "replayed_no_qualified_formal_missing",
                "no-qualified result precedes its formal refutation",
            )
        })?;
        if !self.unrestricted_verdicts.contains_key(&formal_digest) {
            return Err(TrustError::new(
                "replayed_no_qualified_unrestricted_missing",
                "no-qualified result lacks its prior unrestricted verdict",
            ));
        }
        let history_digest = digest_field(raw, "history_summary_sha256")?;
        let (history, history_event_hash, facts) = self
            .histories
            .get(&history_digest)
            .ok_or_else(|| {
                TrustError::new(
                    "replayed_no_qualified_history_missing",
                    "no-qualified result precedes its history summary",
                )
            })?;
        require_digest(raw, "history_summary_event_hash", *history_event_hash)?;
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "replayed_no_qualified_contract_missing",
                "no-qualified history names no classified contract",
            )
        })?;
        for (field, expected) in [
            ("target_id", contract.target_id.as_str()),
            ("reason", string_field(raw, "reason")?),
        ] {
            if string_field(raw, field)? != expected {
                return Err(TrustError::new(
                    "replayed_no_qualified_identity_mismatch",
                    format!("no-qualified result differs at {field}"),
                ));
            }
        }
        for (field, expected) in [
            ("target_statement_sha256", contract.target_statement_sha256),
            ("validation_contract_sha256", contract.digest),
            ("source_claim_lineage_sha256", contract.lineage_sha256),
        ] {
            require_digest(raw, field, expected)?;
        }
        if string_field(formal.value(), "target_id")? != contract.target_id
            || digest_field(formal.value(), "target_statement_sha256")?
                != contract.target_statement_sha256
        {
            return Err(TrustError::new(
                "replayed_no_qualified_target_mismatch",
                "formal refutation and history belong to different targets",
            ));
        }
        let failed = raw
            .get("failed_attempt_evidence")
            .filter(|value| !value.is_null());
        if let Some(failed) = failed {
            let object = failed.as_object().ok_or_else(|| {
                TrustError::new(
                    "replayed_failed_qualification_invalid",
                    "failed qualification evidence must be an object",
                )
            })?;
            let parse = |field: &str| -> Result<AuthoritativeRecord, TrustError> {
                let value = object.get(field).cloned().ok_or_else(|| {
                    TrustError::new(
                        "replayed_failed_qualification_record_missing",
                        format!("failed qualification lacks {field}"),
                    )
                })?;
                if field == "profile" {
                    AuthoritativeRecord::parse_as(
                        &self.registry,
                        "trellis-qualification-profile/v1",
                        value,
                    )
                } else {
                    AuthoritativeRecord::parse(&self.registry, value)
                }
            };
            let basis = parse("independent_basis")?;
            let demand = parse("witness_resource_demand")?;
            let admissibility = parse("source_witness_admissibility")?;
            let profile = parse("profile")?;
            self.require_seed_profile(&profile)?;
            self.require_seed_record(&basis)?;
            validate_independent_basis(&self.seed, &basis)?;
            validate_qualification_prerequisites(&QualificationPrerequisites {
                contract,
                formal_refutation: formal,
                history_summary: history,
                history_summary_event_hash: *history_event_hash,
                history_facts: facts,
                independent_basis: &basis,
                witness_resource_demand: &demand,
                source_witness_admissibility: &admissibility,
                profile: &profile,
            })?;
            let conditional = self
                .active_conditional_statements
                .get(&event.previous_event_hash)
                .ok_or_else(|| {
                    TrustError::new(
                        "replayed_failed_qualification_conditional_missing",
                        "failed qualification does not immediately follow its generated conditional statement",
                    )
                })?;
            let selection = self
                .active_selections
                .get(&conditional.record.profile_selection_event_hash)
                .ok_or_else(|| {
                    TrustError::new(
                        "replayed_failed_qualification_selection_missing",
                        "failed qualification conditional has no checked selection",
                    )
                })?;
            if conditional.target_id != contract.target_id
                || conditional.evidence.formal_refutation_sha256 != formal_digest
                || conditional.evidence.independent_basis != basis
                || conditional.evidence.witness_resource_demand != demand
                || conditional.evidence.source_witness_admissibility != admissibility
                || conditional.evidence.profile != profile
                || selection.selection.history_summary_sha256 != history_digest
                || selection.selection.history_summary_event_hash != *history_event_hash
            {
                return Err(TrustError::new(
                    "replayed_failed_qualification_evidence_changed",
                    "failed qualification evidence differs from its exact conditionalization inputs",
                ));
            }
            let execution_receipt = object
                .get("checked_failure_execution_receipt")
                .ok_or_else(|| {
                    TrustError::new(
                        "replayed_failed_qualification_receipt_missing",
                        "failed qualification lacks its checked execution receipt",
                    )
                })?;
            let receipt = self.validate_qualification_failure_execution_receipt(
                execution_receipt,
                &profile,
                event.previous_event_hash,
                &contract.target_id,
                conditional.record.statement_sha256,
                &self.qualification_context_from_evidence(
                    selection.selection,
                    &conditional.evidence,
                    Some(conditional),
                )?,
            )?;
            if receipt == Sha256Digest::ZERO
                || object
                    .get("checked_failure_receipt_sha256")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<Sha256Digest>().ok())
                    != Some(receipt)
                || digest_field(failed, "conditional_statement_sha256")?
                    != conditional.record.statement_sha256
                || digest_field(failed, "conditional_statement_event_hash")?
                    != conditional.record.statement_event_hash
                || digest_field(raw, "profile_sha256")? != profile.digest()
                || digest_field(raw, "checked_failure_receipt_sha256")? != receipt
                || string_field(raw, "qualification_route")?
                    != format!("{:?}", QualificationRoute::EligibleForProfileEvaluation)
                || string_field(raw, "reason")? != "conditional_proof_not_established"
            {
                return Err(TrustError::new(
                    "replayed_failed_qualification_binding_mismatch",
                    "failed qualification terminal does not bind its exact checked attempt",
                ));
            }
            return Ok((formal_digest, history_digest, true));
        }
        if event.previous_event_hash != *history_event_hash {
            return Err(TrustError::new(
                "replayed_no_qualified_history_not_immediate",
                "a no-attempt terminal must immediately follow its complete history summary",
            ));
        }
        for field in [
            "profile_sha256",
            "checked_failure_receipt_sha256",
            "failed_attempt_evidence",
        ] {
            if !raw.get(field).is_some_and(Value::is_null) {
                return Err(TrustError::new(
                    "replayed_no_qualified_unexpected_attempt",
                    format!("no-attempt terminal must set {field} to null"),
                ));
            }
        }
        let profile_available = self.seed.records_by_digest.values().any(|record| {
            record.contract().record_schema == "trellis-qualification-profile/v1"
                && record.value().get("target_id").and_then(Value::as_str)
                    == Some(contract.target_id.as_str())
                && record
                    .value()
                    .get("validation_contract_sha256")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<Sha256Digest>().ok())
                    == Some(contract.digest)
        });
        let route = route_qualified_recovery(contract, facts, false, profile_available);
        if route == QualificationRoute::EligibleForProfileEvaluation {
            return Err(TrustError::new(
                "replayed_eligible_qualification_closed_without_attempt",
                "eligible qualification cannot be closed without a checked failure receipt",
            ));
        }
        let reason = qualification_route_reason(route)?;
        if string_field(raw, "qualification_route")? != format!("{route:?}")
            || string_field(raw, "reason")? != reason
        {
            return Err(TrustError::new(
                "replayed_no_qualified_route_mismatch",
                "no-qualified route or reason was not centrally derived",
            ));
        }
        Ok((formal_digest, history_digest, false))
    }

    fn validate_negative_proof_no_qualified_result_at(
        &self,
        raw: &Value,
        event: &JournalEvent,
    ) -> Result<(Sha256Digest, Sha256Digest), TrustError> {
        if string_field(raw, "result_kind")? != "checked_negative_proof" {
            return Err(TrustError::new(
                "replayed_negative_terminal_kind_mismatch",
                "general negative terminal has the wrong result kind",
            ));
        }
        let target_id = string_field(raw, "target_id")?;
        let proof = self.negative_proofs.get(target_id).ok_or_else(|| {
            TrustError::new(
                "replayed_negative_terminal_proof_missing",
                "general negative terminal precedes its checked proof",
            )
        })?;
        let proof_digest = digest_field(raw, "negative_proof_subject_sha256")?;
        if proof_digest != proof.proof_subject_sha256
            || proof.unrestricted_verdict_event_hash == Sha256Digest::ZERO
        {
            return Err(TrustError::new(
                "replayed_negative_terminal_proof_mismatch",
                "general negative terminal does not bind its unrestricted proof",
            ));
        }
        let history_digest = digest_field(raw, "history_summary_sha256")?;
        let (history, history_event_hash, facts) = self.histories.get(&history_digest).ok_or_else(|| {
            TrustError::new(
                "replayed_negative_terminal_history_missing",
                "general negative terminal precedes its history summary",
            )
        })?;
        if event.previous_event_hash != *history_event_hash {
            return Err(TrustError::new(
                "replayed_negative_terminal_history_not_immediate",
                "general negative terminal must immediately follow its history summary",
            ));
        }
        require_digest(raw, "history_summary_event_hash", *history_event_hash)?;
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "replayed_negative_terminal_contract_missing",
                "general negative history names no classified contract",
            )
        })?;
        let route = route_qualified_recovery(contract, facts, false, false);
        let validation_complete = match contract.method {
            SourceValidationMethod::CheckedRefutationReflectionV1 => {
                route == QualificationRoute::ProhibitedCheckedReflection
                    && self.reflection_results.values().any(|(result, _)| {
                        result
                            .value()
                            .get("validation_contract_sha256")
                            .and_then(Value::as_str)
                            .and_then(|value| value.parse::<Sha256Digest>().ok())
                            == Some(contract.digest)
                    })
            }
            SourceValidationMethod::NotDefinedForClaimShapeV1 => {
                route == QualificationRoute::ProhibitedUnsupportedClaimShape
                    && self
                        .outcomes_by_contract
                        .get(&contract.digest)
                        .into_iter()
                        .flatten()
                        .any(|entry| {
                            entry.validated.status == ValidationStatus::NotDefinedForClaimShape
                        })
            }
            SourceValidationMethod::ExactRustExecutionV1 => false,
        };
        if contract.target_id != target_id
            || contract.target_statement_sha256 != proof.target_statement_sha256
            || !validation_complete
        {
            return Err(TrustError::new(
                "replayed_negative_terminal_source_validation_incomplete",
                "theorem-level negative terminal lacks its contract-required source validation",
            ));
        }
        for (field, expected) in [
            ("target_statement_sha256", contract.target_statement_sha256),
            ("validation_contract_sha256", contract.digest),
            ("source_claim_lineage_sha256", contract.lineage_sha256),
        ] {
            require_digest(raw, field, expected)?;
        }
        if string_field(raw, "qualification_route")? != format!("{route:?}")
            || string_field(raw, "reason")? != qualification_route_reason(route)?
        {
            return Err(TrustError::new(
                "replayed_negative_terminal_route_mismatch",
                "general negative terminal route was not centrally derived",
            ));
        }
        for field in [
            "profile_sha256",
            "checked_failure_receipt_sha256",
            "failed_attempt_evidence",
        ] {
            if !raw.get(field).is_some_and(Value::is_null) {
                return Err(TrustError::new(
                    "replayed_negative_terminal_unexpected_attempt",
                    format!("general negative terminal must set {field} to null"),
                ));
            }
        }
        Ok((proof_digest, history_digest))
    }

    fn validate_external_claim_envelope(
        &self,
        envelope: &Value,
        event_predecessor: Sha256Digest,
    ) -> Result<(), TrustError> {
        match string_field(envelope, "result_kind")? {
            "positive_proof" => {
                return self
                    .validate_positive_claim_envelope(envelope, event_predecessor)
            }
            "checked_negative_proof" => {
                return self
                    .validate_negative_proof_claim_envelope(envelope, event_predecessor)
            }
            "formal_refutation" => {}
            _ => {
                return Err(TrustError::new(
                    "claim_result_kind_invalid",
                    "external claim rows name an unknown formal result kind",
                ))
            }
        }
        let target_id = string_field(envelope, "target_id")?;
        let formal_digest = digest_field(envelope, "formal_refutation_sha256")?;
        let formal = self.formal_refutations.get(&formal_digest).ok_or_else(|| {
            TrustError::new(
                "claim_formal_refutation_unrecorded",
                "claim rows require a checked formal refutation",
            )
        })?;
        if !self.unrestricted_verdicts.contains_key(&formal_digest) {
            return Err(TrustError::new(
                "claim_unrestricted_verdict_missing",
                "claim rows require the formal refutation's unrestricted verdict",
            ));
        }
        let history_digest = digest_field(envelope, "history_summary_sha256")?;
        let (history, _, facts) = self.histories.get(&history_digest).ok_or_else(|| {
            TrustError::new(
                "claim_history_unrecorded",
                "claim rows require a checked history summary",
            )
        })?;
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "claim_contract_unclassified",
                "claim history names no classified contract",
            )
        })?;
        if target_id != contract.target_id
            || string_field(formal.value(), "target_id")? != contract.target_id
            || digest_field(formal.value(), "target_statement_sha256")?
                != contract.target_statement_sha256
        {
            return Err(TrustError::new(
                "claim_target_mismatch",
                "claim, formal result, and source history belong to different targets",
            ));
        }
        require_digest(
            envelope,
            "target_statement_sha256",
            contract.target_statement_sha256,
        )?;
        let matching_qualifications: Vec<_> = self
            .qualifications
            .values()
            .filter(|(bundle, _)| {
                bundle.value().get("target_id").and_then(Value::as_str) == Some(target_id)
                    && bundle
                        .value()
                        .get("formal_refutation_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(formal_digest)
                    && bundle
                        .value()
                        .get("source_validation_history_summary_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(history_digest)
            })
            .collect();
        let no_qualified = self.no_qualified_results.get(target_id).filter(|state| {
            state.result_kind == NegativeResultKind::FormalWitnessRefutation
                && state.negative_result_sha256 == formal_digest
                && state.history_summary_sha256 == history_digest
        });
        let (conditional_line, applicability_line, terminal_event, terminal_digest) =
            match (matching_qualifications.as_slice(), no_qualified) {
                ([(bundle, _)], None) => {
                    let applicability = self
                        .applicabilities
                        .get(&bundle.digest())
                        .ok_or_else(|| {
                            TrustError::new(
                                "qualified_claim_lacks_applicability",
                                "qualified claim lacks separate applicability",
                            )
                        })?;
                    let status = string_field(applicability.0.value(), "status")?;
                    let applicability_text = match status {
                        "formally_derived" => "FORMALLY DERIVED",
                        "mechanically_enforced" => "ENFORCED",
                        "externally_attested" => "ATTESTED",
                        "unestablished" => "UNESTABLISHED",
                        _ => {
                            return Err(TrustError::new(
                                "claim_applicability_status_invalid",
                                "unknown applicability status",
                            ))
                        }
                    };
                    (
                        format!(
                            "Conditional extracted-model claim under {}/{}: PROVED.",
                            string_field(bundle.value(), "profile_id")?,
                            digest_field(bundle.value(), "condition_sha256")?
                        ),
                        format!("Actual-use coverage of C: {applicability_text}."),
                        applicability.1,
                        bundle.digest(),
                    )
                }
                ([], Some(state)) => {
                    let status = if state.conditional_attempted {
                        "NOT ESTABLISHED"
                    } else {
                        "NOT ATTEMPTED"
                    };
                    (
                        format!(
                            "Conditional extracted-model claim under no-approved-profile/C: {status}."
                        ),
                        "Actual-use coverage of C: NOT APPLICABLE.".to_owned(),
                        state.event_hash,
                        history_digest,
                    )
                }
                _ => {
                    return Err(TrustError::new(
                        "claim_qualification_terminal_ambiguous",
                        "claim target does not have exactly one qualification terminal",
                    ))
                }
            };
        let source_line = match contract.method {
            SourceValidationMethod::CheckedRefutationReflectionV1 => {
                if !self.reflection_results.values().any(|(result, _)| {
                    result
                        .value()
                        .get("validation_contract_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(contract.digest)
                }) {
                    return Err(TrustError::new(
                        "claim_reflection_result_missing",
                        "reflection claim lacks its checked result",
                    ));
                }
                "Source-counterevidence validation: checked_refutation_reflection_v1; source refutation checked."
                    .to_owned()
            }
            SourceValidationMethod::NotDefinedForClaimShapeV1 => {
                if !self
                    .outcomes_by_contract
                    .get(&contract.digest)
                    .into_iter()
                    .flatten()
                    .any(|entry| {
                        entry.validated.status == ValidationStatus::NotDefinedForClaimShape
                    })
                {
                    return Err(TrustError::new(
                        "claim_not_defined_outcome_missing",
                        "undefined claim lacks its kernel-generated outcome",
                    ));
                }
                "Source-counterevidence validation: not_defined_for_claim_shape_v1; not defined for this claim shape."
                    .to_owned()
            }
            SourceValidationMethod::ExactRustExecutionV1 => {
                let status = self
                    .outcomes_by_contract
                    .get(&contract.digest)
                    .and_then(|entries| entries.last())
                    .map(|entry| validation_status_name(entry.validated.status))
                    .unwrap_or("no_exact_attempt_recorded");
                let dominance = if facts.decisive_source_refutation_present {
                    "qualification prohibited by decisive exact source counterexample"
                } else if facts.source_model_mismatch_unresolved {
                    "hard halt: source/model mismatch"
                } else {
                    "no decisive source counterexample recorded"
                };
                format!(
                    "Source-counterevidence validation: exact_rust_execution_v1; {status}; {dominance}."
                )
            }
        };
        let unrestricted_line = "Unrestricted extracted-model claim: REFUTED.";
        let rendered = format!(
            "{unrestricted_line}\n{source_line}\n{conditional_line}\n{applicability_line}\n"
        );
        for (field, expected) in [
            ("unrestricted_extracted_model_claim", unrestricted_line),
            ("source_counterevidence_validation", source_line.as_str()),
            ("conditional_extracted_model_claim", conditional_line.as_str()),
            ("actual_use_coverage", applicability_line.as_str()),
            ("rendered_utf8", rendered.as_str()),
        ] {
            if string_field(envelope, field)? != expected {
                return Err(TrustError::new(
                    "claim_rendered_bytes_mismatch",
                    format!("claim generator output differs at {field}"),
                ));
            }
        }
        for (field, expected) in [
            ("terminal_result_sha256", terminal_digest),
            ("terminal_event_hash", terminal_event),
            ("journal_predecessor_sha256", terminal_event),
        ] {
            require_digest(envelope, field, expected)?;
        }
        if event_predecessor != terminal_event {
            return Err(TrustError::new(
                "claim_not_immediate_after_terminal",
                "external claim rows must immediately follow their terminal result",
            ));
        }
        Ok(())
    }

    fn validate_negative_proof_claim_envelope(
        &self,
        envelope: &Value,
        event_predecessor: Sha256Digest,
    ) -> Result<(), TrustError> {
        let target_id = string_field(envelope, "target_id")?;
        let proof = self.negative_proofs.get(target_id).ok_or_else(|| {
            TrustError::new(
                "negative_claim_proof_missing",
                "negative claim rows precede their checked proof",
            )
        })?;
        let history_digest = digest_field(envelope, "history_summary_sha256")?;
        let (history, _, _) = self.histories.get(&history_digest).ok_or_else(|| {
            TrustError::new(
                "negative_claim_history_missing",
                "negative claim rows name no checked history",
            )
        })?;
        let contract_digest = digest_field(history.value(), "validation_contract_sha256")?;
        let (_, contract) = self.contracts.get(&contract_digest).ok_or_else(|| {
            TrustError::new(
                "negative_claim_contract_missing",
                "negative claim rows name no classified contract",
            )
        })?;
        let terminal = self.no_qualified_results.get(target_id).ok_or_else(|| {
            TrustError::new(
                "negative_claim_terminal_missing",
                "negative claim rows precede their no-qualified terminal",
            )
        })?;
        if proof.target_id != target_id
            || proof.unrestricted_verdict_event_hash == Sha256Digest::ZERO
            || contract.target_id != target_id
            || contract.target_statement_sha256 != proof.target_statement_sha256
            || terminal.result_kind != NegativeResultKind::CheckedNegativeProof
            || terminal.negative_result_sha256 != proof.proof_subject_sha256
            || terminal.history_summary_sha256 != history_digest
            || terminal.conditional_attempted
            || terminal.event_hash != event_predecessor
        {
            return Err(TrustError::new(
                "negative_claim_binding_mismatch",
                "negative claim rows do not bind one theorem-level terminal",
            ));
        }
        for (field, expected) in [
            ("target_statement_sha256", proof.target_statement_sha256),
            ("negative_proof_subject_sha256", proof.proof_subject_sha256),
            ("terminal_result_sha256", history_digest),
            ("terminal_event_hash", terminal.event_hash),
            ("journal_predecessor_sha256", terminal.event_hash),
        ] {
            require_digest(envelope, field, expected)?;
        }
        let unrestricted = "Unrestricted extracted-model claim: REFUTED.";
        let source = match contract.method {
            SourceValidationMethod::CheckedRefutationReflectionV1 => {
                if !self.reflection_results.values().any(|(result, _)| {
                    result
                        .value()
                        .get("validation_contract_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(contract.digest)
                }) {
                    return Err(TrustError::new(
                        "negative_claim_reflection_missing",
                        "checked-reflection claim lacks its authenticated result",
                    ));
                }
                "Source-counterevidence validation: checked_refutation_reflection_v1; source refutation checked from the approved theorem-level bridge."
            }
            SourceValidationMethod::NotDefinedForClaimShapeV1 => {
                "Source-counterevidence validation: not_defined_for_claim_shape_v1; not defined for this claim shape."
            }
            SourceValidationMethod::ExactRustExecutionV1 => {
                return Err(TrustError::new(
                    "negative_claim_exact_execution_without_witness",
                    "exact Rust execution cannot consume a witness-free model proof",
                ));
            }
        };
        let conditional =
            "Conditional extracted-model claim under no-approved-profile/C: NOT ATTEMPTED.";
        let applicability = "Actual-use coverage of C: NOT APPLICABLE.";
        let rendered =
            format!("{unrestricted}\n{source}\n{conditional}\n{applicability}\n");
        for (field, expected) in [
            ("unrestricted_extracted_model_claim", unrestricted),
            ("source_counterevidence_validation", source),
            ("conditional_extracted_model_claim", conditional),
            ("actual_use_coverage", applicability),
            ("rendered_utf8", rendered.as_str()),
        ] {
            if string_field(envelope, field)? != expected {
                return Err(TrustError::new(
                    "negative_claim_rendered_bytes_mismatch",
                    format!("negative claim generator differs at {field}"),
                ));
            }
        }
        Ok(())
    }

    fn validate_positive_claim_envelope(
        &self,
        envelope: &Value,
        event_predecessor: Sha256Digest,
    ) -> Result<(), TrustError> {
        let target_id = string_field(envelope, "target_id")?;
        let proof = self.positive_proofs.get(target_id).ok_or_else(|| {
            TrustError::new(
                "positive_claim_proof_missing",
                "positive claim rows precede their checked proof",
            )
        })?;
        if proof.unrestricted_verdict_event_hash == Sha256Digest::ZERO
            || proof.target_id != target_id
            || event_predecessor != proof.unrestricted_verdict_event_hash
        {
            return Err(TrustError::new(
                "positive_claim_verdict_binding_mismatch",
                "positive claim rows do not immediately follow their unrestricted verdict",
            ));
        }
        for (field, expected) in [
            ("target_statement_sha256", proof.target_statement_sha256),
            (
                "positive_proof_subject_sha256",
                proof.proof_subject_sha256,
            ),
            ("terminal_result_sha256", proof.proof_subject_sha256),
            (
                "terminal_event_hash",
                proof.unrestricted_verdict_event_hash,
            ),
            (
                "journal_predecessor_sha256",
                proof.unrestricted_verdict_event_hash,
            ),
        ] {
            require_digest(envelope, field, expected)?;
        }
        let unrestricted = "Unrestricted extracted-model claim: PROVED.";
        let source = "Source-counterevidence validation: NOT APPLICABLE to a proved unrestricted result.";
        let conditional =
            "Conditional extracted-model claim under no-approved-profile/C: NOT ATTEMPTED.";
        let applicability = "Actual-use coverage of C: NOT APPLICABLE.";
        let rendered =
            format!("{unrestricted}\n{source}\n{conditional}\n{applicability}\n");
        for (field, expected) in [
            ("unrestricted_extracted_model_claim", unrestricted),
            ("source_counterevidence_validation", source),
            ("conditional_extracted_model_claim", conditional),
            ("actual_use_coverage", applicability),
            ("rendered_utf8", rendered.as_str()),
        ] {
            if string_field(envelope, field)? != expected {
                return Err(TrustError::new(
                    "positive_claim_rendered_bytes_mismatch",
                    format!("positive claim generator differs at {field}"),
                ));
            }
        }
        Ok(())
    }

    fn require_no_later_target_affecting_event(
        &self,
        target_id: &str,
        claim_sequence: u64,
    ) -> Result<(), TrustError> {
        for bundle in self.journal.committed_bundle_values()? {
            let event: JournalEvent = serde_json::from_value(
                bundle
                    .get("event")
                    .cloned()
                    .ok_or_else(|| {
                        TrustError::new(
                            "journal_event_missing",
                            "committed bundle lacks its event",
                        )
                    })?,
            )
            .map_err(|error| TrustError::new("journal_event_decode_failed", error.to_string()))?;
            if event.sequence_number <= claim_sequence
                || event.event_kind == EventKind::PackageAuthorized
            {
                continue;
            }
            if matches!(
                event.event_kind,
                EventKind::AdvanceGateApproved
                    | EventKind::AdvanceGateFeedback
                    | EventKind::AuditAuthorization
                    | EventKind::RevisionOpened
                    | EventKind::ProtectedReapprovalApproved
                    | EventKind::ProtectedReapprovalFeedback
                    | EventKind::Revoked
            ) {
                return Err(TrustError::new(
                    "package_claim_precedes_global_trust_change",
                    format!(
                        "target {target_id} claim rows precede later global event {:?}",
                        event.event_kind
                    ),
                ));
            }
            let affects_target = if bundle
                .get("subject_encoding")
                .and_then(Value::as_str)
                == Some("canonical_json")
            {
                bundle
                    .get("subject_json")
                    .and_then(|value| value.get("target_id"))
                    .and_then(Value::as_str)
                    == Some(target_id)
            } else if bundle
                .get("subject_encoding")
                .and_then(Value::as_str)
                == Some("raw_base64")
            {
                let value = raw_json_subject(&bundle)?;
                value
                    .get("target_id")
                    .and_then(Value::as_str)
                    .map(|value| value == target_id)
                    .unwrap_or(false)
            } else {
                false
            };
            if affects_target {
                return Err(TrustError::new(
                    "package_claim_rows_stale_after_target_event",
                    format!(
                        "target {target_id} has later affecting event {:?}",
                        event.event_kind
                    ),
                ));
            }
        }
        Ok(())
    }

    fn require_seed_record(&self, record: &AuthoritativeRecord) -> Result<(), TrustError> {
        if self.seed.records_by_digest.get(&record.digest()) != Some(record) {
            return Err(TrustError::new(
                "qualification_record_not_seed_frozen",
                "qualification definition is not byte-identical to the gate seed",
            ));
        }
        Ok(())
    }

    fn require_seed_profile(&self, profile: &AuthoritativeRecord) -> Result<(), TrustError> {
        if profile.contract().record_schema != "trellis-qualification-profile/v1" {
            return Err(TrustError::new(
                "qualification_profile_wrong_schema",
                "expected a v1 qualification profile",
            ));
        }
        self.require_seed_record(profile)?;
        let profile_value = profile.value();
        let matches = self
            .seed
            .records_by_digest
            .values()
            .filter(|record| {
                record.contract().record_schema == "trellis-qualification-profile-catalog/v1"
            })
            .flat_map(|catalog| {
                catalog
                    .value()
                    .get("profiles")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter(|candidate| *candidate == profile_value)
            .count();
        if matches != 1 {
            return Err(TrustError::new(
                "qualification_profile_not_uniquely_cataloged",
                "seed profile must occur byte-identically in exactly one approved catalog",
            ));
        }
        if profile_value.get("attempt_order").and_then(Value::as_u64) != Some(0)
            || profile_value.get("attempt_budget").and_then(Value::as_u64) != Some(1)
        {
            return Err(TrustError::new(
                "qualification_profile_attempt_policy_unsupported",
                "v1 permits exactly one ordered conditional-proof attempt",
            ));
        }
        let target_id = string_field(profile_value, "target_id")?;
        let contract_sha256 = digest_field(profile_value, "validation_contract_sha256")?;
        let profile_count = self
            .seed
            .records_by_digest
            .values()
            .filter(|candidate| {
                candidate.contract().record_schema == "trellis-qualification-profile/v1"
                    && candidate.value().get("target_id").and_then(Value::as_str)
                        == Some(target_id)
                    && candidate
                        .value()
                        .get("validation_contract_sha256")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<Sha256Digest>().ok())
                        == Some(contract_sha256)
            })
            .count();
        if profile_count != 1 {
            return Err(TrustError::new(
                "qualification_profile_multiplicity_unsupported",
                "v1 requires exactly one seed-frozen profile per qualified target",
            ));
        }
        for field in [
            "witness_demand_checker_sha256",
            "source_admissibility_checker_sha256",
            "conditional_statement_generator_sha256",
            "conditional_proof_checker_sha256",
            "applicability_validator_sha256",
        ] {
            self.require_approved_tool_digest(
                digest_field(profile_value, field)?,
                &format!("qualification profile {field}"),
            )?;
        }
        Ok(())
    }

    fn require_exact_tool_invocation(
        &self,
        invocation: &SourceToolInvocation<'_>,
        expected_runner_sha256: Sha256Digest,
        expected_command_id: &str,
        expected_input: &Value,
    ) -> Result<(), TrustError> {
        if invocation.command_id != expected_command_id
            || !invocation.environment.is_empty()
            || invocation.input != expected_input
        {
            return Err(TrustError::new(
                "approved_tool_invocation_policy_mismatch",
                "tool invocation must use the kernel-derived command, empty environment, and exact canonical input",
            ));
        }
        let leaf = self
            .evidence
            .leaves_by_logical_id
            .get(invocation.tool_logical_id)
            .ok_or_else(|| {
                TrustError::new(
                    "approved_tool_invocation_leaf_missing",
                    "tool invocation names no approved evidence leaf",
                )
            })?;
        if leaf.raw_sha256 != expected_runner_sha256 {
            return Err(TrustError::new(
                "approved_tool_invocation_runner_mismatch",
                "tool invocation leaf differs from the seed-pinned runner",
            ));
        }
        let root = std::fs::canonicalize(invocation.working_directory).map_err(|error| {
            TrustError::new(
                "approved_tool_working_directory_invalid",
                error.to_string(),
            )
        })?;
        let expected_runner = std::fs::canonicalize(root.join(&leaf.relative_path)).map_err(
            |error| {
                TrustError::new(
                    "approved_tool_manifest_path_invalid",
                    error.to_string(),
                )
            },
        )?;
        let actual_runner = std::fs::canonicalize(invocation.runner_path).map_err(|error| {
            TrustError::new("approved_tool_runner_path_invalid", error.to_string())
        })?;
        if expected_runner != actual_runner {
            return Err(TrustError::new(
                "approved_tool_working_root_mismatch",
                "runner must be the manifest-relative leaf under the declared evidence root",
            ));
        }
        Ok(())
    }

    fn validate_qualification_input_execution_receipts(
        &self,
        formal_refutation_sha256: Sha256Digest,
        history_summary_sha256: Sha256Digest,
        independent_basis: &AuthoritativeRecord,
        profile: &AuthoritativeRecord,
        demand: &AuthoritativeRecord,
        admissibility: &AuthoritativeRecord,
        allowed_predecessors: &[Sha256Digest],
    ) -> Result<(), TrustError> {
        let demand_input = self.qualification_prerequisite_context(
            formal_refutation_sha256,
            history_summary_sha256,
            independent_basis,
            profile,
            None,
        )?;
        let admissibility_input = self.qualification_prerequisite_context(
            formal_refutation_sha256,
            history_summary_sha256,
            independent_basis,
            profile,
            Some(demand),
        )?;
        for (record, purpose, profile_field, self_field, command_id, input) in [
            (
                demand,
                "witness-resource-demand-checker/v1",
                "witness_demand_checker_sha256",
                "demand_certificate_sha256",
                "trellis-witness-resource-demand-checker-v1",
                &demand_input,
            ),
            (
                admissibility,
                "source-witness-admissibility-checker/v1",
                "source_admissibility_checker_sha256",
                "certificate_sha256",
                "trellis-source-witness-admissibility-checker-v1",
                &admissibility_input,
            ),
        ] {
            let receipt = record
                .value()
                .get("checker_execution_receipt")
                .ok_or_else(|| {
                    TrustError::new(
                        "qualification_checker_receipt_missing",
                        format!("{purpose} record lacks its execution receipt"),
                    )
                })?;
            let predecessor = digest_field(receipt, "journal_predecessor_sha256")?;
            if !allowed_predecessors.contains(&predecessor) {
                return Err(TrustError::new(
                    "qualification_checker_receipt_predecessor_invalid",
                    format!("{purpose} receipt is not bound to the qualification frontier"),
                ));
            }
            let digest = super::execution::validate_execution_receipt(
                receipt,
                &self.evidence,
                purpose,
                predecessor,
            )?;
            require_successful_json_execution(receipt, purpose)?;
            require_digest(
                receipt,
                "runner_sha256",
                digest_field(profile.value(), profile_field)?,
            )?;
            validate_receipt_invocation_policy(receipt, command_id, input)?;
            require_digest(record.value(), "checker_execution_receipt_sha256", digest)?;
            let mut expected = record.value().clone();
            let object = expected.as_object_mut().ok_or_else(|| {
                TrustError::new(
                    "qualification_record_not_object",
                    "qualification checker record must be an object",
                )
            })?;
            object.remove("checker_execution_receipt");
            object.remove("checker_execution_receipt_sha256");
            object.remove(self_field);
            if receipt.get("parsed_stdout") != Some(&expected) {
                return Err(TrustError::new(
                    "qualification_record_not_exact_checker_output",
                    format!("{purpose} record differs from exact checker stdout"),
                ));
            }
        }
        Ok(())
    }

    fn validate_qualification_bundle_execution_receipt(
        &self,
        bundle: &AuthoritativeRecord,
        profile: &AuthoritativeRecord,
        expected_predecessor: Sha256Digest,
        expected_input: &Value,
    ) -> Result<(), TrustError> {
        let receipt = bundle
            .value()
            .get("checker_execution_receipt")
            .ok_or_else(|| {
                TrustError::new(
                    "qualification_bundle_checker_receipt_missing",
                    "qualification bundle lacks its proof-checker receipt",
                )
            })?;
        let digest = super::execution::validate_execution_receipt(
            receipt,
            &self.evidence,
            "qualification-conditional-proof-checker/v1",
            expected_predecessor,
        )?;
        require_successful_json_execution(receipt, "qualification proof checker")?;
        require_digest(
            receipt,
            "runner_sha256",
            digest_field(profile.value(), "conditional_proof_checker_sha256")?,
        )?;
        validate_receipt_invocation_policy(
            receipt,
            "trellis-qualification-conditional-proof-checker-v1",
            expected_input,
        )?;
        require_digest(bundle.value(), "checker_execution_receipt_sha256", digest)?;
        let mut expected = bundle.value().clone();
        let object = expected.as_object_mut().ok_or_else(|| {
            TrustError::new(
                "qualification_bundle_not_object",
                "qualification bundle must be an object",
            )
        })?;
        for field in [
            "conditional_theorem_candidate_sha256",
            "conditional_proof_receipt_sha256",
            "checked_conditional_proof_sha256",
            "checker_and_axiom_closure_sha256",
            "checker_execution_receipt",
            "checker_execution_receipt_sha256",
            "journal_predecessor_sha256",
            "bundle_sha256",
        ] {
            object.remove(field);
        }
        if receipt.get("parsed_stdout") != Some(&expected) {
            return Err(TrustError::new(
                "qualification_bundle_not_exact_checker_output",
                "qualification bundle differs from exact proof-checker stdout",
            ));
        }
        Ok(())
    }

    fn validate_conditional_proof_authority(
        &self,
        evidence: &QualificationInputRecords,
        bundle: &AuthoritativeRecord,
    ) -> Result<(), TrustError> {
        validate_conditional_candidate_proof_receipt(
            &evidence.conditional_theorem_candidate,
            &evidence.conditional_proof_receipt,
        )?;
        let proof_receipt_sha256 =
            digest_field(&evidence.conditional_proof_receipt, "proof_receipt_sha256")?;
        for (field, expected) in [
            (
                "conditional_theorem_candidate_sha256",
                evidence.conditional_theorem_candidate.digest(),
            ),
            ("conditional_proof_receipt_sha256", proof_receipt_sha256),
            ("checked_conditional_proof_sha256", proof_receipt_sha256),
            (
                "checker_and_axiom_closure_sha256",
                digest_field(
                    &evidence.conditional_proof_receipt,
                    "checker_and_axiom_closure_sha256",
                )?,
            ),
            (
                "generated_conditional_statement_sha256",
                digest_field(
                    evidence.conditional_theorem_candidate.value(),
                    "conditional_statement_sha256",
                )?,
            ),
        ] {
            require_digest(bundle.value(), field, expected)?;
        }
        Ok(())
    }

    fn validate_qualification_failure_execution_receipt(
        &self,
        receipt: &Value,
        profile: &AuthoritativeRecord,
        expected_predecessor: Sha256Digest,
        expected_target_id: &str,
        expected_statement_sha256: Sha256Digest,
        expected_input: &Value,
    ) -> Result<Sha256Digest, TrustError> {
        let digest = super::execution::validate_execution_receipt(
            receipt,
            &self.evidence,
            "qualification-conditional-proof-checker/v1",
            expected_predecessor,
        )?;
        require_successful_json_execution(receipt, "qualification proof checker")?;
        require_digest(
            receipt,
            "runner_sha256",
            digest_field(profile.value(), "conditional_proof_checker_sha256")?,
        )?;
        validate_receipt_invocation_policy(
            receipt,
            "trellis-qualification-conditional-proof-checker-v1",
            expected_input,
        )?;
        let output = receipt.get("parsed_stdout").ok_or_else(|| {
            TrustError::new(
                "qualification_failure_output_missing",
                "checked failure receipt lacks strict parsed output",
            )
        })?;
        self.registry.validate(
            "trellis://schemas/qualification-proof-failure/v1",
            output,
        )?;
        if string_field(output, "target_id")? != expected_target_id
            || digest_field(output, "generated_conditional_statement_sha256")?
                != expected_statement_sha256
        {
            return Err(TrustError::new(
                "qualification_failure_output_binding_mismatch",
                "checked failure output differs from the active target or conditional statement",
            ));
        }
        Ok(digest)
    }
}

fn raw_json_subject(bundle: &Value) -> Result<Value, TrustError> {
    let encoded = bundle
        .get("subject_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            TrustError::new("raw_subject_missing", "raw bundle lacks subject_base64")
        })?;
    let bytes = BASE64_STANDARD.decode(encoded).map_err(|error| {
        TrustError::new("raw_subject_base64_invalid", error.to_string())
    })?;
    let value = parse_json_strict(&bytes)
        .map_err(|error| TrustError::new("raw_subject_json_invalid", error.to_string()))?;
    if canonical_json_value(&value)? != bytes {
        return Err(TrustError::new(
            "raw_subject_json_not_canonical",
            "pipeline raw JSON subjects must be canonical",
        ));
    }
    Ok(value)
}

fn payload_subject_id(bundle: &Value) -> Result<&str, TrustError> {
    bundle
        .get("payload")
        .and_then(|payload| payload.get("subject_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            TrustError::new(
                "journal_payload_subject_missing",
                "event bundle payload lacks a string subject_id",
            )
        })
}

fn render_claim_document(
    claims: &BTreeMap<String, ExternalClaimState>,
) -> Result<Vec<u8>, TrustError> {
    let mut output = String::from(
        "# Trellis proof claims\n\nOnly the four independent lines under each target are normative.\n\n",
    );
    for (target_id, claim) in claims {
        let rendered = string_field(&claim.envelope, "rendered_utf8")?;
        output.push_str("## ");
        output.push_str(target_id);
        output.push_str("\n\n");
        output.push_str(rendered);
        output.push('\n');
    }
    Ok(output.into_bytes())
}

fn validate_local_closure_proof_receipt(
    receipt: &Value,
    target_id: &str,
    polarity: &str,
    proof_artifact_sha256: Sha256Digest,
    generated_statement_sha256: Sha256Digest,
    checker_toolchain_sha256: Sha256Digest,
    approved_axiom_closure_sha256: Sha256Digest,
    semantic_definition_closure_sha256: Sha256Digest,
) -> Result<(), TrustError> {
    if string_field(receipt, "schema")? != "trellis-local-closure-proof-receipt/v1"
        || string_field(receipt, "target_id")? != target_id
        || string_field(receipt, "polarity")? != polarity
    {
        return Err(TrustError::new(
            "local_closure_proof_receipt_identity_mismatch",
            "proof receipt has the wrong schema, target, or polarity",
        ));
    }
    let digest = self_digest(
        DomainTag::RawArtifact,
        receipt,
        "proof_receipt_sha256",
    )?;
    if digest != proof_artifact_sha256
        || digest_field(receipt, "proof_receipt_sha256")? != digest
    {
        return Err(TrustError::new(
            "local_closure_proof_receipt_digest_mismatch",
            "proof artifact is not the receipt's exact self digest",
        ));
    }
    for (field, expected) in [
        ("generated_statement_sha256", generated_statement_sha256),
        ("checker_toolchain_sha256", checker_toolchain_sha256),
        ("approved_axiom_closure_sha256", approved_axiom_closure_sha256),
        (
            "semantic_definition_closure_sha256",
            semantic_definition_closure_sha256,
        ),
    ] {
        require_digest(receipt, field, expected)?;
    }
    let local = receipt.get("local_closure_record").ok_or_else(|| {
        TrustError::new(
            "local_closure_proof_record_missing",
            "proof receipt lacks its full local closure record",
        )
    })?;
    for (receipt_field, local_field) in [
        ("generated_statement_sha256", "active_statement_hash"),
        ("checker_toolchain_sha256", "toolchain_hash"),
        ("approved_axiom_closure_sha256", "approved_axioms_hash"),
    ] {
        if digest_field(receipt, receipt_field)? != digest_field(local, local_field)? {
            return Err(TrustError::new(
                "local_closure_proof_record_binding_mismatch",
                format!("receipt differs from its local closure at {local_field}"),
            ));
        }
    }
    let expected_semantic = tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&serde_json::json!({
            "boundary_theorems": local.get("boundary_theorems").cloned().unwrap_or(Value::Null),
            "strict_theorem_deps": local.get("strict_theorem_deps").cloned().unwrap_or(Value::Null),
            "strict_definition_deps": local.get("strict_definition_deps").cloned().unwrap_or(Value::Null),
            "kernel_semantic_hashes": local.get("kernel_semantic_hashes").cloned().unwrap_or(Value::Null),
            "active_decl_hash": string_field(local, "active_decl_hash")?,
            "active_statement_hash": string_field(local, "active_statement_hash")?,
        }))?,
    );
    if expected_semantic != semantic_definition_closure_sha256 {
        return Err(TrustError::new(
            "local_closure_semantic_root_mismatch",
            "proof receipt semantic dependency closure is invalid",
        ));
    }
    Ok(())
}

pub(crate) fn validate_conditional_candidate_proof_receipt(
    candidate: &AuthoritativeRecord,
    receipt: &Value,
) -> Result<(), TrustError> {
    if candidate.contract().record_schema != "trellis-conditional-theorem-candidate/v1"
        || string_field(receipt, "schema")?
            != "trellis-conditional-local-closure-proof-receipt/v1"
        || digest_field(receipt, "candidate_definition_sha256")? != candidate.digest()
        || string_field(receipt, "target_id")?
            != string_field(candidate.value(), "target_id")?
        || string_field(receipt, "node_id")? != string_field(candidate.value(), "node_id")?
    {
        return Err(TrustError::new(
            "conditional_candidate_proof_receipt_identity_mismatch",
            "conditional proof receipt does not identify its exact seed candidate",
        ));
    }
    for (receipt_field, candidate_field) in [
        ("profile_definition_sha256", "profile_definition_sha256"),
        (
            "conditional_statement_sha256",
            "conditional_statement_sha256",
        ),
        ("active_statement_sha256", "active_statement_sha256"),
    ] {
        if digest_field(receipt, receipt_field)?
            != digest_field(candidate.value(), candidate_field)?
        {
            return Err(TrustError::new(
                "conditional_candidate_proof_receipt_binding_mismatch",
                format!("conditional proof receipt differs at {receipt_field}"),
            ));
        }
    }
    let receipt_digest = self_digest(
        DomainTag::RawArtifact,
        receipt,
        "proof_receipt_sha256",
    )?;
    require_digest(receipt, "proof_receipt_sha256", receipt_digest)?;
    let local = receipt.get("local_closure_record").ok_or_else(|| {
        TrustError::new(
            "conditional_candidate_proof_record_missing",
            "conditional proof receipt lacks its full local closure record",
        )
    })?;
    if string_field(local, "node")? != string_field(candidate.value(), "node_id")?
        || digest_field(local, "active_statement_hash")?
            != digest_field(candidate.value(), "active_statement_sha256")?
        || digest_field(local, "toolchain_hash")?
            != digest_field(receipt, "checker_toolchain_sha256")?
        || digest_field(local, "approved_axioms_hash")?
            != digest_field(receipt, "approved_axiom_closure_sha256")?
    {
        return Err(TrustError::new(
            "conditional_candidate_proof_record_binding_mismatch",
            "conditional proof receipt differs from its local closure record",
        ));
    }
    let expected_semantic = tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&serde_json::json!({
            "boundary_theorems": local.get("boundary_theorems").cloned().unwrap_or(Value::Null),
            "strict_theorem_deps": local.get("strict_theorem_deps").cloned().unwrap_or(Value::Null),
            "strict_definition_deps": local.get("strict_definition_deps").cloned().unwrap_or(Value::Null),
            "kernel_semantic_hashes": local.get("kernel_semantic_hashes").cloned().unwrap_or(Value::Null),
            "active_decl_hash": string_field(local, "active_decl_hash")?,
            "active_statement_hash": string_field(local, "active_statement_hash")?,
        }))?,
    );
    require_digest(
        receipt,
        "semantic_definition_closure_sha256",
        expected_semantic,
    )?;
    let expected_checker_axioms = tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&serde_json::json!({
            "checker_toolchain_sha256": digest_field(receipt, "checker_toolchain_sha256")?,
            "lean_executable_sha256": digest_field(local, "lean_executable_hash")?,
            "lake_executable_sha256": digest_field(local, "lake_executable_hash")?,
            "checker_script_sha256": digest_field(local, "checker_script_hash")?,
            "approved_axiom_closure_sha256": digest_field(receipt, "approved_axiom_closure_sha256")?,
            "kernel_axioms": local.get("kernel_axioms").cloned().unwrap_or(Value::Null),
        }))?,
    );
    require_digest(
        receipt,
        "checker_and_axiom_closure_sha256",
        expected_checker_axioms,
    )?;
    Ok(())
}

fn validate_witness_refutation_proof_receipt(formal: &Value) -> Result<(), TrustError> {
    let receipt = formal.get("formal_proof_receipt").ok_or_else(|| {
        TrustError::new(
            "formal_proof_receipt_missing",
            "formal witness refutation lacks its checked proof receipt",
        )
    })?;
    if string_field(receipt, "schema")?
        != "trellis-witness-refutation-proof-receipt/v1"
        || string_field(receipt, "target_id")? != string_field(formal, "target_id")?
    {
        return Err(TrustError::new(
            "formal_proof_receipt_identity_mismatch",
            "formal proof receipt has the wrong schema or target",
        ));
    }
    let receipt_digest = self_digest(
        DomainTag::RawArtifact,
        receipt,
        "proof_receipt_sha256",
    )?;
    require_digest(receipt, "proof_receipt_sha256", receipt_digest)?;
    require_digest(formal, "checked_not_t_proof_sha256", receipt_digest)?;
    for (field, expected) in [
        (
            "target_statement_sha256",
            digest_field(formal, "target_statement_sha256")?,
        ),
        (
            "witness_certificate_sha256",
            digest_field(formal, "witness_certificate_sha256")?,
        ),
        (
            "checker_toolchain_sha256",
            digest_field(formal, "checker_toolchain_sha256")?,
        ),
        (
            "approved_axiom_closure_sha256",
            digest_field(formal, "approved_axiom_closure_sha256")?,
        ),
    ] {
        require_digest(receipt, field, expected)?;
    }
    let witness_record = receipt
        .get("witness_local_closure_record")
        .ok_or_else(|| {
            TrustError::new(
                "formal_witness_local_closure_missing",
                "proof receipt lacks the witness theorem local closure",
            )
        })?;
    let not_t_record = receipt.get("not_t_local_closure_record").ok_or_else(|| {
        TrustError::new(
            "formal_not_t_local_closure_missing",
            "proof receipt lacks the unrestricted refutation local closure",
        )
    })?;
    for record in [witness_record, not_t_record] {
        if digest_field(record, "toolchain_hash")?
            != digest_field(formal, "checker_toolchain_sha256")?
            || digest_field(record, "approved_axioms_hash")?
                != digest_field(formal, "approved_axiom_closure_sha256")?
        {
            return Err(TrustError::new(
                "formal_local_closure_tool_mismatch",
                "formal proof local closures use different checker or axiom roots",
            ));
        }
    }
    require_digest(
        formal,
        "generated_witness_refutation_sha256",
        digest_field(witness_record, "active_statement_hash")?,
    )?;
    require_digest(
        formal,
        "generated_not_t_statement_sha256",
        digest_field(not_t_record, "active_statement_hash")?,
    )?;
    let witness_record_digest = tagged_hash(
        DomainTag::RawArtifact,
        &canonical_json_value(witness_record)?,
    );
    require_digest(
        formal,
        "checked_witness_proof_sha256",
        witness_record_digest,
    )?;
    let witness_node = string_field(
        formal
            .get("witness_certificate")
            .ok_or_else(|| TrustError::new("formal_witness_missing", "certificate missing"))?,
        "witness_refutation_node_id",
    )?;
    if string_field(witness_record, "node")? != witness_node {
        return Err(TrustError::new(
            "formal_witness_node_mismatch",
            "witness certificate names a different checked theorem node",
        ));
    }
    let witness_statement = digest_field(formal, "generated_witness_refutation_sha256")?;
    let dependency_matches = ["strict_theorem_deps", "boundary_theorems"]
        .into_iter()
        .any(|field| {
            not_t_record
                .get(field)
                .and_then(Value::as_object)
                .and_then(|dependencies| dependencies.get(witness_node))
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<Sha256Digest>().ok())
                == Some(witness_statement)
        });
    if !dependency_matches {
        return Err(TrustError::new(
            "formal_not_t_does_not_depend_on_witness",
            "unrestricted refutation closure does not contain the named witness theorem",
        ));
    }
    Ok(())
}

fn expected_harness_cohort_sha256(
    contract: &SourceValidationContractView,
) -> Result<Sha256Digest, TrustError> {
    let cohort = contract.harness_cohort_basis.as_ref().ok_or_else(|| {
        TrustError::new(
            "source_harness_basis_missing",
            "exact execution contract lacks a harness cohort basis",
        )
    })?;
    Ok(tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&serde_json::json!({
            "schema": "trellis-source-harness-cohort/v1",
            "validation_contract_sha256": contract.digest,
            "source_tree_sha256": cohort.source_tree_sha256,
            "toolchain_build_basis_sha256": cohort.toolchain_build_basis_sha256,
            "runner_sha256": cohort.runner_sha256,
            "environment_contract_sha256": cohort.environment_contract_sha256,
            "concretization_schema_sha256": cohort.concretization_schema_sha256,
            "erasure_relation_sha256": cohort.erasure_relation_sha256,
            "raw_observation_schema_sha256": cohort.raw_observation_schema_sha256,
            "observation_oracle_sha256": contract.observation_oracle_sha256,
            "source_validator_sha256": contract.source_validator_sha256,
        }))?,
    ))
}

fn expected_full_tuple_sha256(
    contract: &SourceValidationContractView,
    attempt: &Value,
) -> Result<Sha256Digest, TrustError> {
    let field = |name: &str| {
        attempt.get(name).cloned().ok_or_else(|| {
            TrustError::new(
                "source_attempt_tuple_field_missing",
                format!("exact source attempt lacks {name}"),
            )
        })
    };
    Ok(tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&serde_json::json!({
            "schema": "trellis-exact-source-tuple/v1",
            "validation_contract_sha256": contract.digest,
            "harness_cohort_sha256": expected_harness_cohort_sha256(contract)?,
            "formal_witness_certificate_sha256": field("formal_witness_certificate_sha256")?,
            "witness_term_sha256": field("witness_term_sha256")?,
            "formal_predicate_sha256": field("formal_predicate_sha256")?,
            "encoded_input_or_descriptor_sha256": field("encoded_input_or_descriptor_sha256")?,
            "concretization_receipt_sha256": field("concretization_receipt_sha256")?,
            "erasure_receipt_sha256": field("erasure_receipt_sha256")?,
            "precondition_results": field("precondition_results")?,
            "actual_environment_sha256": field("actual_environment_sha256")?,
            "command_sha256": field("command_sha256")?,
            "execution": field("execution")?,
        }))?,
    ))
}

fn derive_cohort_blockers(outcomes: &[OutcomeEntry]) -> Vec<Value> {
    let mut blockers: BTreeMap<Sha256Digest, (bool, bool, Sha256Digest)> = BTreeMap::new();
    for outcome in outcomes {
        let Some(cohort) = outcome.validated.harness_cohort_sha256 else {
            continue;
        };
        let entry = blockers
            .entry(cohort)
            .or_insert((false, false, outcome.validated.digest));
        match outcome.validated.status {
            ValidationStatus::ControlFailed => {
                entry.0 = true;
                entry.2 = outcome.validated.digest;
            }
            ValidationStatus::InvalidValidationEvidence => {
                entry.1 = true;
                entry.2 = outcome.validated.digest;
            }
            _ => {
                if !entry.0 && !entry.1 {
                    blockers.remove(&cohort);
                }
            }
        }
    }
    blockers
        .into_iter()
        .map(|(cohort, (control_failed, harness_invalid, latest))| {
            serde_json::json!({
                "harness_cohort_sha256": cohort,
                "control_failed": control_failed,
                "harness_invalid": harness_invalid,
                "latest_blocking_result_sha256": latest,
            })
        })
        .collect()
}

fn validation_status_name(status: ValidationStatus) -> &'static str {
    match status {
        ValidationStatus::ExactCounterexampleObserved => "exact_counterexample_observed",
        ValidationStatus::Reproduced => "reproduced",
        ValidationStatus::SourceModelMismatch => "source_model_mismatch",
        ValidationStatus::LanguageInadmissible => "language_inadmissible",
        ValidationStatus::IndependentScopeLimit => "independent_scope_limit",
        ValidationStatus::ValidationBudgetExceeded => "validation_budget_exceeded",
        ValidationStatus::HarnessPolicyRefusal => "harness_policy_refusal",
        ValidationStatus::ControlPassed => "control_passed",
        ValidationStatus::ControlFailed => "control_failed",
        ValidationStatus::ProxyExecuted => "proxy_executed",
        ValidationStatus::ValidationInconclusive => "validation_inconclusive",
        ValidationStatus::NotDefinedForClaimShape => "not_defined_for_claim_shape",
        ValidationStatus::InvalidValidationEvidence => "invalid_validation_evidence",
    }
}

fn qualification_route_reason(route: QualificationRoute) -> Result<&'static str, TrustError> {
    match route {
        QualificationRoute::ProhibitedCheckedReflection => {
            Ok("checked_reflection_decisively_refutes_source")
        }
        QualificationRoute::ProhibitedDecisiveSourceRefutation => {
            Ok("decisive_source_counterexample")
        }
        QualificationRoute::HaltSourceModelMismatch => Ok("source_model_mismatch_halt"),
        QualificationRoute::CorrectBoundaryOrAdmissibility => {
            Ok("boundary_or_admissibility_correction_required")
        }
        QualificationRoute::ProhibitedUnsupportedClaimShape => {
            Ok("source_validation_not_defined_for_claim_shape")
        }
        QualificationRoute::ProhibitedInvalidContract => Ok("invalid_source_contract"),
        QualificationRoute::NoIndependentQualificationBasis => {
            Ok("no_complete_independent_qualification_basis")
        }
        QualificationRoute::EligibleForProfileEvaluation => Err(TrustError::new(
            "eligible_qualification_has_no_terminal_reason",
            "eligible qualification needs a checked proof or failure receipt",
        )),
    }
}

fn validate_summary_flags(
    summary: &AuthoritativeRecord,
    facts: &HistoryFacts,
) -> Result<(), TrustError> {
    for (field, expected) in [
        (
            "decisive_source_refutation_present",
            facts.decisive_source_refutation_present,
        ),
        (
            "source_model_mismatch_unresolved",
            facts.source_model_mismatch_unresolved,
        ),
        (
            "language_inadmissibility_unresolved",
            facts.language_inadmissibility_unresolved,
        ),
        ("contract_invalid", facts.contract_invalid),
    ] {
        if summary.value().get(field).and_then(Value::as_bool) != Some(expected) {
            return Err(TrustError::new(
                "history_summary_fact_mismatch",
                format!("history summary differs at {field}"),
            ));
        }
    }
    Ok(())
}

fn summary_derivation(value: &Value) -> Result<Sha256Digest, TrustError> {
    let mut stripped = value.clone();
    let object = stripped.as_object_mut().ok_or_else(|| {
        TrustError::new("history_summary_not_object", "summary must be an object")
    })?;
    object.remove("summary_derivation_sha256");
    object.remove("summary_sha256");
    Ok(tagged_hash(
        DomainTag::SemanticValidator,
        &canonical_json_value(&stripped)?,
    ))
}

fn build_reflection_result_from_receipt(
    registry: &SchemaRegistry,
    receipt: &super::execution::CheckedExecutionReceipt,
    checker_sha256: Sha256Digest,
    approval_closure: Sha256Digest,
    predecessor: Sha256Digest,
) -> Result<AuthoritativeRecord, TrustError> {
    require_successful_json_execution(receipt.value(), "checked reflection")?;
    let mut value = receipt.parsed_output().cloned().ok_or_else(|| {
        TrustError::new(
            "reflection_checker_output_not_json",
            "reflection checker did not return one strict JSON result body",
        )
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        TrustError::new(
            "reflection_checker_output_not_object",
            "reflection checker output must be a JSON object",
        )
    })?;
    for field in [
        "checker_toolchain_sha256",
        "checker_execution_receipt",
        "checker_execution_receipt_sha256",
        "approval_closure_sha256",
        "journal_predecessor_sha256",
        "result_sha256",
    ] {
        if object.contains_key(field) {
            return Err(TrustError::new(
                "reflection_checker_output_claims_kernel_fields",
                format!("reflection checker cannot author kernel field {field}"),
            ));
        }
    }
    for field in [
        "source_claim_lineage_sha256",
        "validation_contract_sha256",
        "rust_target_statement_sha256",
        "model_target_statement_sha256",
        "generated_implication_statement_sha256",
        "checked_model_refutation_sha256",
        "checked_reflection_proof_sha256",
        "approved_axiom_closure_sha256",
        "generated_source_refutation_statement_sha256",
        "checked_source_refutation_proof_sha256",
    ] {
        let digest = object
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TrustError::new(
                    "reflection_checker_output_digest_missing",
                    format!("reflection checker output lacks digest field {field}"),
                )
            })?
            .parse::<Sha256Digest>()?;
        if digest == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "reflection_checker_output_placeholder_digest",
                format!("reflection checker output field {field} uses the zero sentinel"),
            ));
        }
    }
    object.insert(
        "checker_toolchain_sha256".to_owned(),
        Value::String(checker_sha256.to_string()),
    );
    object.insert(
        "checker_execution_receipt".to_owned(),
        receipt.value().clone(),
    );
    object.insert(
        "checker_execution_receipt_sha256".to_owned(),
        Value::String(receipt.digest().to_string()),
    );
    object.insert(
        "approval_closure_sha256".to_owned(),
        Value::String(approval_closure.to_string()),
    );
    object.insert(
        "journal_predecessor_sha256".to_owned(),
        Value::String(predecessor.to_string()),
    );
    object.insert(
        "result_sha256".to_owned(),
        Value::String(Sha256Digest::ZERO.to_string()),
    );
    let digest = self_digest(
        DomainTag::ReflectionValidationResult,
        &value,
        "result_sha256",
    )?;
    value["result_sha256"] = Value::String(digest.to_string());
    AuthoritativeRecord::parse(registry, value)
}

fn validate_reflection_execution_receipt(
    result: &Value,
    evidence: &VerifiedEvidenceClosure,
    expected_checker_sha256: Sha256Digest,
    expected_predecessor: Sha256Digest,
) -> Result<(), TrustError> {
    let receipt = result.get("checker_execution_receipt").ok_or_else(|| {
        TrustError::new(
            "reflection_checker_execution_receipt_missing",
            "checked reflection result lacks its checker execution receipt",
        )
    })?;
    let digest = super::execution::validate_execution_receipt(
        receipt,
        evidence,
        "checked-refutation-reflection/v1",
        expected_predecessor,
    )?;
    require_successful_json_execution(receipt, "checked reflection")?;
    require_digest(receipt, "runner_sha256", expected_checker_sha256)?;
    require_digest(result, "checker_execution_receipt_sha256", digest)?;

    let mut expected_output = result.clone();
    let object = expected_output.as_object_mut().ok_or_else(|| {
        TrustError::new(
            "reflection_result_not_object",
            "checked reflection result must be an object",
        )
    })?;
    for field in [
        "checker_toolchain_sha256",
        "checker_execution_receipt",
        "checker_execution_receipt_sha256",
        "approval_closure_sha256",
        "journal_predecessor_sha256",
        "result_sha256",
    ] {
        object.remove(field);
    }
    if receipt.get("parsed_stdout") != Some(&expected_output) {
        return Err(TrustError::new(
            "reflection_result_not_exact_checker_output",
            "reflection result is not the exact kernel augmentation of checker stdout",
        ));
    }
    Ok(())
}

fn require_successful_json_execution(
    receipt: &Value,
    purpose: &str,
) -> Result<(), TrustError> {
    let successful = receipt.get("timed_out").and_then(Value::as_bool) == Some(false)
        && receipt.get("stdout_truncated").and_then(Value::as_bool) == Some(false)
        && receipt.get("stderr_truncated").and_then(Value::as_bool) == Some(false)
        && receipt.get("exit_code").and_then(Value::as_i64) == Some(0)
        && receipt.get("parsed_stdout").is_some_and(|value| !value.is_null());
    if !successful {
        return Err(TrustError::new(
            "approved_json_execution_failed",
            format!("{purpose} requires exit zero and complete strict JSON output"),
        ));
    }
    Ok(())
}

fn validate_receipt_invocation_policy(
    receipt: &Value,
    expected_command_id: &str,
    expected_input: &Value,
) -> Result<(), TrustError> {
    let environment_is_empty = receipt
        .get("environment")
        .and_then(Value::as_object)
        .is_some_and(serde_json::Map::is_empty);
    let command_matches = receipt
        .get("command")
        .and_then(|command| command.get("command_id"))
        .and_then(Value::as_str)
        == Some(expected_command_id);
    if !environment_is_empty
        || !command_matches
        || receipt.get("input") != Some(expected_input)
    {
        return Err(TrustError::new(
            "execution_receipt_invocation_policy_mismatch",
            "receipt does not bind the kernel-derived command, empty environment, and exact canonical input",
        ));
    }
    Ok(())
}

struct SourceOutcomeAxes {
    status: &'static str,
    realizability: &'static str,
    observability: &'static str,
    reproducibility: &'static str,
    decisiveness: &'static str,
}

fn centrally_derive_source_outcome_axes(
    role: &str,
    classification: &str,
) -> Result<SourceOutcomeAxes, TrustError> {
    let axes = match (role, classification) {
        ("exact_witness", "exact_falsifying_observation_matched") => SourceOutcomeAxes {
            status: "exact_counterexample_observed",
            realizability: "exact_source_state_safely_constructed",
            observability: "exact_falsifying_source_observation_matched",
            reproducibility: "recorded_once",
            decisiveness: "decisive",
        },
        ("exact_witness", "exact_observation_did_not_match") => SourceOutcomeAxes {
            status: "source_model_mismatch",
            realizability: "exact_source_state_safely_constructed",
            observability: "exact_observation_did_not_match",
            reproducibility: "not_reproduced",
            decisiveness: "invalid",
        },
        ("exact_witness", "language_inadmissible") => SourceOutcomeAxes {
            status: "language_inadmissible",
            realizability: "contradicted",
            observability: "not_observed",
            reproducibility: "not_attempted",
            decisiveness: "invalid",
        },
        ("exact_witness", "independent_scope_limit") => SourceOutcomeAxes {
            status: "independent_scope_limit",
            realizability: "unestablished",
            observability: "not_observed",
            reproducibility: "not_attempted",
            decisiveness: "inconclusive",
        },
        ("exact_witness", "validation_budget_exceeded") => SourceOutcomeAxes {
            status: "validation_budget_exceeded",
            realizability: "unestablished",
            observability: "not_observed",
            reproducibility: "not_attempted",
            decisiveness: "inconclusive",
        },
        ("exact_witness", "harness_policy_refusal") => SourceOutcomeAxes {
            status: "harness_policy_refusal",
            realizability: "unestablished",
            observability: "not_observed",
            reproducibility: "not_attempted",
            decisiveness: "inconclusive",
        },
        ("exact_witness", "validation_inconclusive") => SourceOutcomeAxes {
            status: "validation_inconclusive",
            realizability: "unestablished",
            observability: "not_observed",
            reproducibility: "not_attempted",
            decisiveness: "inconclusive",
        },
        ("positive_control", "positive_control_passed") => SourceOutcomeAxes {
            status: "control_passed",
            realizability: "exact_source_state_safely_constructed",
            observability: "not_observed",
            reproducibility: "recorded_once",
            decisiveness: "corroborative",
        },
        ("positive_control", "positive_control_failed") => SourceOutcomeAxes {
            status: "control_failed",
            realizability: "exact_source_state_safely_constructed",
            observability: "not_observed",
            reproducibility: "recorded_once",
            decisiveness: "invalid",
        },
        ("corroborating_proxy", "corroborating_proxy_executed") => SourceOutcomeAxes {
            status: "proxy_executed",
            realizability: "exact_source_state_safely_constructed",
            observability: "not_observed",
            reproducibility: "recorded_once",
            decisiveness: "corroborative",
        },
        (_, "invalid_validation_evidence") => SourceOutcomeAxes {
            status: "invalid_validation_evidence",
            realizability: "invalid",
            observability: "not_observed",
            reproducibility: "not_attempted",
            decisiveness: "invalid",
        },
        _ => {
            return Err(TrustError::new(
                "source_oracle_role_classification_mismatch",
                format!("oracle classification {classification} is invalid for role {role}"),
            ))
        }
    };
    Ok(axes)
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        TrustError::new(
            "pipeline_field_missing_or_invalid",
            format!("{field} must be a string"),
        )
    })
}

fn digest_field(value: &Value, field: &str) -> Result<Sha256Digest, TrustError> {
    string_field(value, field)?.parse()
}

fn optional_digest_field(
    value: &Value,
    field: &str,
) -> Result<Option<Sha256Digest>, TrustError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => value.parse().map(Some),
        Some(_) => Err(TrustError::new(
            "pipeline_optional_digest_invalid",
            format!("{field} must be a digest string or null"),
        )),
    }
}

fn require_digest(value: &Value, field: &str, expected: Sha256Digest) -> Result<(), TrustError> {
    let actual = digest_field(value, field)?;
    if actual != expected {
        return Err(TrustError::new(
            "pipeline_digest_binding_mismatch",
            format!("{field}: expected {expected}, got {actual}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::closure::VerifiedEvidenceLeaf;
    use super::super::source_validation::Decisiveness;
    use std::fs;

    fn digest(byte: &str) -> String {
        byte.repeat(32)
    }

    fn fixture_conditional_candidate() -> AuthoritativeRecord {
        let fixture: Value = serde_json::from_str(include_str!("schemas/REGISTRATION_HASH_DAG_FIXTURES.v1.json"))
        .unwrap();
        AuthoritativeRecord::parse(
            &SchemaRegistry::v1().unwrap(),
            fixture["objects"]["conditional_theorem_candidate"].clone(),
        )
        .unwrap()
    }

    fn fixture_conditional_profile() -> AuthoritativeRecord {
        let fixture: Value = serde_json::from_str(include_str!("schemas/REGISTRATION_HASH_DAG_FIXTURES.v1.json"))
        .unwrap();
        AuthoritativeRecord::parse_as(
            &SchemaRegistry::v1().unwrap(),
            "trellis-qualification-profile/v1",
            fixture["objects"]["qualification_profile"].clone(),
        )
        .unwrap()
    }

    fn fixture_conditional_profile_catalog() -> AuthoritativeRecord {
        let fixture: Value = serde_json::from_str(include_str!("schemas/REGISTRATION_HASH_DAG_FIXTURES.v1.json"))
        .unwrap();
        AuthoritativeRecord::parse(
            &SchemaRegistry::v1().unwrap(),
            fixture["objects"]["qualification_profile_catalog"].clone(),
        )
        .unwrap()
    }

    fn fixture_undefined_source_records() -> (AuthoritativeRecord, AuthoritativeRecord) {
        let registry = SchemaRegistry::v1().unwrap();
        let target_statement = super::super::canonical::raw_sha256(b"unsupported target");
        let source_interpretation =
            super::super::canonical::raw_sha256(b"unsupported source interpretation");
        let normalized_claim =
            super::super::canonical::raw_sha256(b"unsupported normalized claim");
        let source_tree = super::super::canonical::raw_sha256(b"source tree");
        let preconditions = super::super::canonical::raw_sha256(b"no source preconditions");
        let refinement = super::super::canonical::raw_sha256(b"unestablished refinement");
        let build = super::super::canonical::raw_sha256(b"adapted-source build");
        let generator = super::super::canonical::raw_sha256(b"trellis trust kernel");
        let classifier = super::super::canonical::raw_sha256(b"unsupported classifier");
        let predecessor = super::super::canonical::raw_sha256(b"seed predecessor");

        let mut lineage = serde_json::json!({
            "schema": "trellis-source-claim-lineage/v1",
            "lineage_id": "unsupported-source-lineage",
            "target_id": "unsupported-target",
            "model_target_statement_sha256": target_statement,
            "rust_target_statement_sha256": source_interpretation,
            "source_claim_interpretation_sha256": source_interpretation,
            "normalized_claim_sha256": normalized_claim,
            "source_scope": "adapted_source",
            "source_tree_sha256": source_tree,
            "entry_point_semantics_sha256": source_interpretation,
            "precondition_manifest_sha256": preconditions,
            "refinement_semantics_sha256": refinement,
            "build_semantics_sha256": build,
            "concretization_erasure_semantics_sha256": refinement,
            "source_observation_semantics_sha256": source_interpretation,
            "lineage_change_kind": "initial_seed",
            "registration_authority": "seed_contract_v1",
            "registration_epoch": "seed",
            "lineage_generator_id": "trellis-trust-kernel",
            "lineage_generator_sha256": generator,
            "registration_predecessor_head_sha256": predecessor,
            "lineage_definition_sha256": Sha256Digest::ZERO,
        });
        let lineage_digest = self_digest(
            DomainTag::SourceClaimLineage,
            &lineage,
            "lineage_definition_sha256",
        )
        .unwrap();
        lineage["lineage_definition_sha256"] = Value::String(lineage_digest.to_string());
        let lineage = AuthoritativeRecord::parse(&registry, lineage).unwrap();

        let mut contract = serde_json::json!({
            "schema": "trellis-source-validation-contract/v1",
            "contract_id": "unsupported-source-validation",
            "target_id": "unsupported-target",
            "target_statement_sha256": target_statement,
            "source_claim_lineage_id": "unsupported-source-lineage",
            "source_claim_lineage_sha256": lineage.digest(),
            "rust_target_statement_sha256": source_interpretation,
            "source_claim_interpretation_sha256": source_interpretation,
            "claim_shape": "other",
            "normalized_claim_schema_id": "trellis://campaign/normalized-lean-claim/v1",
            "normalized_claim_sha256": normalized_claim,
            "negative_certificate_class": "unsupported_negative_certificate_v1",
            "formal_refutation_certificate_schema_id": "unsupported-negative-certificate/v1",
            "source_counterevidence_class": "no_approved_source_oracle",
            "validation_method": "not_defined_for_claim_shape_v1",
            "adequacy": "undefined",
            "contract_generator_id": "trellis-trust-kernel",
            "contract_generator_sha256": generator,
            "classification_proof_artifact_sha256": classifier,
            "source_scope": "adapted_source",
            "source_tree_sha256": source_tree,
            "refinement_closure_sha256": refinement,
            "build_tool_closure_sha256": build,
            "nondeterminism_environment_policy": "unsupported_in_v1",
            "qualification_permission": "prohibited",
            "registration_epoch": "seed",
            "registration_predecessor_head_sha256": predecessor,
            "contract_definition_sha256": Sha256Digest::ZERO,
        });
        let contract_digest = self_digest(
            DomainTag::SourceValidationContract,
            &contract,
            "contract_definition_sha256",
        )
        .unwrap();
        contract["contract_definition_sha256"] = Value::String(contract_digest.to_string());
        let contract = AuthoritativeRecord::parse(&registry, contract).unwrap();
        (lineage, contract)
    }

    fn fixture_undefined_source_pipeline(
        journal: TrustJournal,
        lineage: AuthoritativeRecord,
        contract: AuthoritativeRecord,
    ) -> TrustDerivationPipeline {
        let records = BTreeMap::from([
            (lineage.digest(), lineage.clone()),
            (contract.digest(), contract.clone()),
        ]);
        let values = records
            .iter()
            .map(|(digest, record)| (*digest, record.value().clone()))
            .collect();
        let contract_view = SourceValidationContractView::from_record(&contract).unwrap();
        TrustDerivationPipeline {
            journal,
            registry: SchemaRegistry::v1().unwrap(),
            seed: VerifiedSeedDefinitionClosure {
                seed_manifest_sha256: super::super::canonical::raw_sha256(b"seed manifest"),
                bundle_sha256: super::super::canonical::raw_sha256(b"seed bundle"),
                records_by_digest: records,
                canonical_values_by_digest: values,
            },
            seed_authored_semantic_root: super::super::canonical::raw_sha256(b"semantic root"),
            evidence: VerifiedEvidenceClosure {
                manifest_sha256: super::super::canonical::raw_sha256(b"evidence manifest"),
                evidence_tool_input_root: super::super::canonical::raw_sha256(
                    b"evidence input root",
                ),
                file_count: 0,
                leaves_by_logical_id: BTreeMap::new(),
            },
            lineages: BTreeMap::from([(lineage.digest(), lineage)]),
            lineage_registration: BTreeMap::new(),
            contracts: BTreeMap::from([(contract.digest(), (contract, contract_view))]),
            formal_refutations: BTreeMap::new(),
            unrestricted_verdicts: BTreeMap::new(),
            positive_proofs: BTreeMap::new(),
            negative_proofs: BTreeMap::new(),
            reflection_results: BTreeMap::new(),
            attempts: BTreeMap::new(),
            outcomes_by_contract: BTreeMap::new(),
            histories: BTreeMap::new(),
            active_selections: BTreeMap::new(),
            active_conditional_statements: BTreeMap::new(),
            qualifications: BTreeMap::new(),
            applicabilities: BTreeMap::new(),
            no_qualified_results: BTreeMap::new(),
            external_claim_rows: BTreeMap::new(),
        }
    }

    fn conditional_candidate_receipt(candidate: &AuthoritativeRecord) -> Value {
        let toolchain = super::super::canonical::raw_sha256(b"lean-toolchain");
        let lean_executable = super::super::canonical::raw_sha256(b"lean-executable");
        let lake_executable = super::super::canonical::raw_sha256(b"lake-executable");
        let checker_script = super::super::canonical::raw_sha256(b"checker-script");
        let axioms = super::super::canonical::raw_sha256(b"approved-axioms");
        let local = serde_json::json!({
            "node": string_field(candidate.value(), "node_id").unwrap(),
            "toolchain_hash": toolchain,
            "lean_executable_hash": lean_executable,
            "lake_executable_hash": lake_executable,
            "checker_script_hash": checker_script,
            "approved_axioms_hash": axioms,
            "active_decl_hash": super::super::canonical::raw_sha256(b"declaration-file"),
            "active_statement_hash": digest_field(candidate.value(), "active_statement_sha256").unwrap(),
            "boundary_theorems": {},
            "strict_theorem_deps": {},
            "strict_definition_deps": {},
            "kernel_semantic_hashes": {},
            "kernel_axioms": ["propext"],
        });
        let semantic = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&serde_json::json!({
                "boundary_theorems": {},
                "strict_theorem_deps": {},
                "strict_definition_deps": {},
                "kernel_semantic_hashes": {},
                "active_decl_hash": local["active_decl_hash"],
                "active_statement_hash": local["active_statement_hash"],
            }))
            .unwrap(),
        );
        let checker_axioms = tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&serde_json::json!({
                "checker_toolchain_sha256": toolchain,
                "lean_executable_sha256": lean_executable,
                "lake_executable_sha256": lake_executable,
                "checker_script_sha256": checker_script,
                "approved_axiom_closure_sha256": axioms,
                "kernel_axioms": ["propext"],
            }))
            .unwrap(),
        );
        let mut receipt = serde_json::json!({
            "schema": "trellis-conditional-local-closure-proof-receipt/v1",
            "candidate_definition_sha256": candidate.digest(),
            "profile_definition_sha256": digest_field(candidate.value(), "profile_definition_sha256").unwrap(),
            "target_id": string_field(candidate.value(), "target_id").unwrap(),
            "node_id": string_field(candidate.value(), "node_id").unwrap(),
            "conditional_statement_sha256": digest_field(candidate.value(), "conditional_statement_sha256").unwrap(),
            "active_statement_sha256": digest_field(candidate.value(), "active_statement_sha256").unwrap(),
            "checker_toolchain_sha256": toolchain,
            "approved_axiom_closure_sha256": axioms,
            "checker_and_axiom_closure_sha256": checker_axioms,
            "semantic_definition_closure_sha256": semantic,
            "local_closure_record": local,
            "proof_receipt_sha256": Sha256Digest::ZERO,
        });
        let digest = self_digest(
            DomainTag::RawArtifact,
            &receipt,
            "proof_receipt_sha256",
        )
        .unwrap();
        receipt["proof_receipt_sha256"] = Value::String(digest.to_string());
        receipt
    }

    #[test]
    fn conditional_candidate_receipt_binds_exact_checked_statement() {
        let candidate = fixture_conditional_candidate();
        let receipt = conditional_candidate_receipt(&candidate);
        validate_conditional_candidate_proof_receipt(&candidate, &receipt).unwrap();
    }

    #[test]
    fn conditional_candidate_receipt_rejects_rehashed_statement_substitution() {
        let candidate = fixture_conditional_candidate();
        let mut receipt = conditional_candidate_receipt(&candidate);
        receipt["local_closure_record"]["active_statement_hash"] =
            Value::String(
                super::super::canonical::raw_sha256(b"different theorem").to_string(),
            );
        receipt["proof_receipt_sha256"] = Value::String(Sha256Digest::ZERO.to_string());
        let digest = self_digest(
            DomainTag::RawArtifact,
            &receipt,
            "proof_receipt_sha256",
        )
        .unwrap();
        receipt["proof_receipt_sha256"] = Value::String(digest.to_string());
        let error =
            validate_conditional_candidate_proof_receipt(&candidate, &receipt).unwrap_err();
        assert_eq!(
            error.code,
            "conditional_candidate_proof_record_binding_mismatch"
        );
    }

    #[test]
    fn undefined_source_validation_is_committed_and_replayed_with_unsupported_decisiveness() {
        use crate::trust_base::journal::tests::create_exceptional_journal_fixture;

        let directory = tempfile::tempdir().unwrap();
        let journal_path = directory.path().join("journal");
        let journal_fixture = create_exceptional_journal_fixture(
            &journal_path,
            Some(EventKind::ProtectedReapprovalApproved),
        );
        let actor_manifest = journal_fixture.actor_manifest.clone();
        let journal = TrustJournal::open(&journal_path, actor_manifest.clone()).unwrap();
        let (lineage, contract) = fixture_undefined_source_records();
        let contract_digest = contract.digest();
        let mut pipeline =
            fixture_undefined_source_pipeline(journal, lineage.clone(), contract.clone());

        let event_hash = pipeline
            .record_undefined_source_validation(
                "unsupported-source-outcome",
                contract_digest,
                "unsupported-result",
            )
            .unwrap();
        let recorded = pipeline.outcomes_by_contract.get(&contract_digest).unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].event.event_hash, event_hash);
        assert_eq!(
            recorded[0].validated.status,
            ValidationStatus::NotDefinedForClaimShape
        );
        assert_eq!(
            recorded[0].validated.decisiveness,
            Decisiveness::UnsupportedClaimClass
        );
        assert_eq!(recorded[0].validated.role, None);
        drop(pipeline);

        let journal = TrustJournal::open(&journal_path, actor_manifest).unwrap();
        let mut replayed = fixture_undefined_source_pipeline(journal, lineage, contract);
        replayed.rebuild_from_journal().unwrap();
        let replayed_outcomes = replayed
            .outcomes_by_contract
            .get(&contract_digest)
            .unwrap();
        assert_eq!(replayed_outcomes.len(), 1);
        assert_eq!(replayed_outcomes[0].event.event_hash, event_hash);
        assert_eq!(
            replayed_outcomes[0].validated.status,
            ValidationStatus::NotDefinedForClaimShape
        );
        assert_eq!(
            replayed_outcomes[0].validated.decisiveness,
            Decisiveness::UnsupportedClaimClass
        );
        assert_eq!(replayed_outcomes[0].validated.role, None);
    }

    #[test]
    fn post_gate_candidate_consumes_the_exact_runtime_local_closure() {
        use crate::model::{AxcheckStatus, LocalClosureRecord, ProtocolState};
        use crate::trust_base::journal::tests::create_exceptional_journal_fixture;

        let directory = tempfile::tempdir().unwrap();
        let journal_path = directory.path().join("journal");
        let journal_fixture = create_exceptional_journal_fixture(&journal_path, None);
        let journal = TrustJournal::open(&journal_path, journal_fixture.actor_manifest).unwrap();
        let candidate = fixture_conditional_candidate();
        let profile = fixture_conditional_profile();
        let catalog = fixture_conditional_profile_catalog();
        let records = BTreeMap::from([
            (candidate.digest(), candidate.clone()),
            (profile.digest(), profile.clone()),
            (catalog.digest(), catalog),
        ]);
        let values = records
            .iter()
            .map(|(digest, record)| (*digest, record.value().clone()))
            .collect();
        let toolchain = super::super::canonical::raw_sha256(b"approved Lean toolchain");
        let lake_manifest = super::super::canonical::raw_sha256(b"lake");
        let preamble = super::super::canonical::raw_sha256(b"preamble");
        let lean_executable = super::super::canonical::raw_sha256(b"lean executable");
        let lake_executable = super::super::canonical::raw_sha256(b"lake executable");
        let checker_script = super::super::canonical::raw_sha256(b"checker script");
        let platform_boundary = super::super::canonical::raw_sha256(b"platform boundary");
        let qualification_tool = digest_field(
            profile.value(),
            "conditional_proof_checker_sha256",
        )
        .unwrap();
        let evidence = VerifiedEvidenceClosure {
            manifest_sha256: super::super::canonical::raw_sha256(b"manifest"),
            evidence_tool_input_root: super::super::canonical::raw_sha256(b"input root"),
            file_count: 8,
            leaves_by_logical_id: BTreeMap::from([
                (
                    "lean-toolchain".to_owned(),
                    VerifiedEvidenceLeaf {
                        kind: "toolchain".to_owned(),
                        relative_path: "lean-toolchain".to_owned(),
                        byte_length: 25,
                        raw_sha256: toolchain,
                        dependency_ids: Vec::new(),
                    },
                ),
                (
                    "lake-manifest".to_owned(),
                    VerifiedEvidenceLeaf {
                        kind: "toolchain".to_owned(),
                        relative_path: "lake-manifest.json".to_owned(),
                        byte_length: 4,
                        raw_sha256: lake_manifest,
                        dependency_ids: Vec::new(),
                    },
                ),
                (
                    "aeneas-generated-preamble".to_owned(),
                    VerifiedEvidenceLeaf {
                        kind: "model".to_owned(),
                        relative_path: "Preamble.lean".to_owned(),
                        byte_length: 8,
                        raw_sha256: preamble,
                        dependency_ids: Vec::new(),
                    },
                ),
                (
                    "lean-checker-executable".to_owned(),
                    VerifiedEvidenceLeaf {
                        kind: "tool".to_owned(),
                        relative_path: "lean".to_owned(),
                        byte_length: 1,
                        raw_sha256: lean_executable,
                        dependency_ids: Vec::new(),
                    },
                ),
                (
                    "lake-driver-executable".to_owned(),
                    VerifiedEvidenceLeaf {
                        kind: "tool".to_owned(),
                        relative_path: "lake".to_owned(),
                        byte_length: 1,
                        raw_sha256: lake_executable,
                        dependency_ids: Vec::new(),
                    },
                ),
                (
                    "local-closure-checker-script".to_owned(),
                    VerifiedEvidenceLeaf {
                        kind: "tool".to_owned(),
                        relative_path: "checker.lean".to_owned(),
                        byte_length: 1,
                        raw_sha256: checker_script,
                        dependency_ids: Vec::new(),
                    },
                ),
                (
                    "trusted-platform-boundary-v1".to_owned(),
                    VerifiedEvidenceLeaf {
                        kind: "boundary".to_owned(),
                        relative_path: "boundary.json".to_owned(),
                        byte_length: 1,
                        raw_sha256: platform_boundary,
                        dependency_ids: Vec::new(),
                    },
                ),
                (
                    "qualification-tool".to_owned(),
                    VerifiedEvidenceLeaf {
                        kind: "tool".to_owned(),
                        relative_path: "qualification-tool".to_owned(),
                        byte_length: 1,
                        raw_sha256: qualification_tool,
                        dependency_ids: Vec::new(),
                    },
                ),
            ]),
        };
        let pipeline = TrustDerivationPipeline {
            journal,
            registry: SchemaRegistry::v1().unwrap(),
            seed: VerifiedSeedDefinitionClosure {
                seed_manifest_sha256: super::super::canonical::raw_sha256(b"seed"),
                bundle_sha256: super::super::canonical::raw_sha256(b"bundle"),
                records_by_digest: records,
                canonical_values_by_digest: values,
            },
            seed_authored_semantic_root: super::super::canonical::raw_sha256(b"semantic"),
            evidence,
            lineages: BTreeMap::new(),
            lineage_registration: BTreeMap::new(),
            contracts: BTreeMap::new(),
            formal_refutations: BTreeMap::new(),
            unrestricted_verdicts: BTreeMap::new(),
            positive_proofs: BTreeMap::new(),
            negative_proofs: BTreeMap::new(),
            reflection_results: BTreeMap::new(),
            attempts: BTreeMap::new(),
            outcomes_by_contract: BTreeMap::new(),
            histories: BTreeMap::new(),
            active_selections: BTreeMap::new(),
            active_conditional_statements: BTreeMap::new(),
            qualifications: BTreeMap::new(),
            applicabilities: BTreeMap::new(),
            no_qualified_results: BTreeMap::new(),
            external_claim_rows: BTreeMap::new(),
        };

        let node = crate::model::NodeId::from(
            string_field(candidate.value(), "node_id").unwrap(),
        );
        let mut state = ProtocolState::default();
        state.live.present_nodes.insert(node.clone());
        state.committed.present_nodes.insert(node.clone());
        state.proof_nodes.insert(node.clone());
        state.committed_proof_nodes.insert(node.clone());
        let pending = pipeline
            .checked_conditional_candidate_proof_receipt(&state, &profile)
            .unwrap_err();
        assert_eq!(
            pending.code,
            "conditional_candidate_local_closure_missing",
            "an unproved seed candidate must remain distinguishable from a malformed proof binding"
        );
        let mut record = LocalClosureRecord {
            node: node.clone(),
            closure_version: "trellis-local-closure/v1".to_owned(),
            toolchain_hash: toolchain.to_string(),
            lean_executable_hash: lean_executable.to_string(),
            lake_executable_hash: lake_executable.to_string(),
            checker_script_hash: checker_script.to_string(),
            lake_manifest_hash: lake_manifest.to_string(),
            preamble_hash: preamble.to_string(),
            approved_axioms_hash: super::super::canonical::raw_sha256(b"axioms").to_string(),
            active_decl_hash: super::super::canonical::raw_sha256(b"candidate file").to_string(),
            active_statement_hash: digest_field(candidate.value(), "active_statement_sha256")
                .unwrap()
                .to_string(),
            kernel_axioms: BTreeSet::from(["propext".to_owned()]),
            boundary_theorems: BTreeMap::new(),
            strict_theorem_deps: BTreeMap::new(),
            strict_definition_deps: BTreeMap::new(),
            seed_support_definition_deps: BTreeMap::new(),
            seed_support_evidence_root: None,
            seed_support_file_hashes: BTreeMap::new(),
            kernel_semantic_hashes: BTreeMap::new(),
            accepted_at_snapshot_id: "post-gate-proof-snapshot".to_owned(),
            axcheck_status: AxcheckStatus::Agreed,
        };
        record.checker_script_hash = super::super::canonical::raw_sha256(b"drifted checker").to_string();
        state.local_closure_records.insert(node.clone(), record.clone());
        let drift = pipeline
            .checked_conditional_candidate_proof_receipt(&state, &profile)
            .unwrap_err();
        assert_eq!(drift.code, "local_closure_platform_evidence_mismatch");
        record.checker_script_hash = checker_script.to_string();
        state.local_closure_records.insert(node, record);

        let (bound_candidate, receipt) = pipeline
            .checked_conditional_candidate_proof_receipt(&state, &profile)
            .unwrap();
        assert_eq!(bound_candidate, candidate);
        assert_eq!(
            receipt["local_closure_record"]["accepted_at_snapshot_id"],
            "post-gate-proof-snapshot"
        );
        validate_conditional_candidate_proof_receipt(&bound_candidate, &receipt).unwrap();
    }

    fn reflection_body() -> Value {
        serde_json::json!({
            "schema": "trellis-reflection-validation-result/v1",
            "target_id": "target-1",
            "source_claim_lineage_id": "lineage-1",
            "source_claim_lineage_sha256": digest("11"),
            "validation_contract_sha256": digest("22"),
            "rust_target_statement_sha256": digest("33"),
            "model_target_statement_sha256": digest("44"),
            "generated_implication_statement_sha256": digest("55"),
            "checked_model_refutation_sha256": digest("66"),
            "checked_reflection_proof_sha256": digest("77"),
            "approved_axiom_closure_sha256": digest("88"),
            "generated_source_refutation_statement_sha256": digest("99"),
            "checked_source_refutation_proof_sha256": digest("aa"),
        })
    }

    #[cfg(unix)]
    #[test]
    fn reflection_result_requires_exact_successful_receipted_checker_output() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let runner = directory.path().join("reflection-checker");
        fs::write(&runner, b"#!/bin/sh\ncat\n").unwrap();
        let mut permissions = fs::metadata(&runner).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&runner, permissions).unwrap();
        let runner_bytes = fs::read(&runner).unwrap();
        let runner_sha256 = super::super::canonical::raw_sha256(&runner_bytes);
        let evidence = VerifiedEvidenceClosure {
            manifest_sha256: super::super::canonical::raw_sha256(b"manifest"),
            evidence_tool_input_root: super::super::canonical::raw_sha256(b"evidence-root"),
            file_count: 1,
            leaves_by_logical_id: BTreeMap::from([(
                "reflection-checker".to_owned(),
                VerifiedEvidenceLeaf {
                    kind: "tool".to_owned(),
                    relative_path: "reflection-checker".to_owned(),
                    byte_length: runner_bytes.len() as u64,
                    raw_sha256: runner_sha256,
                    dependency_ids: Vec::new(),
                },
            )]),
        };
        let predecessor = super::super::canonical::raw_sha256(b"predecessor");
        let approval = super::super::canonical::raw_sha256(b"approval");
        let body = reflection_body();
        let receipt = super::super::execution::execute_approved_json(
            super::super::execution::ApprovedExecutionRequest {
                evidence: &evidence,
                tool_logical_id: "reflection-checker",
                purpose: "checked-refutation-reflection/v1",
                journal_predecessor_sha256: predecessor,
                execution: super::super::execution::PinnedExecutionRequest {
                    runner_path: &runner,
                    expected_runner_sha256: runner_sha256,
                    command_id: "reflection-check",
                    working_directory: directory.path(),
                    environment: &BTreeMap::new(),
                    input: &body,
                    limits: super::super::execution::ExecutionLimits::default(),
                },
            },
        )
        .unwrap();
        let registry = SchemaRegistry::v1().unwrap();
        let record = build_reflection_result_from_receipt(
            &registry,
            &receipt,
            runner_sha256,
            approval,
            predecessor,
        )
        .unwrap();
        validate_reflection_execution_receipt(
            record.value(),
            &evidence,
            runner_sha256,
            predecessor,
        )
        .unwrap();
        assert_eq!(
            digest_field(record.value(), "checker_execution_receipt_sha256").unwrap(),
            receipt.digest()
        );

        let mut tampered = record.value().clone();
        tampered["checked_source_refutation_proof_sha256"] = Value::String(digest("bb"));
        let error = validate_reflection_execution_receipt(
            &tampered,
            &evidence,
            runner_sha256,
            predecessor,
        )
        .unwrap_err();
        assert_eq!(error.code, "reflection_result_not_exact_checker_output");
    }

    #[cfg(unix)]
    #[test]
    fn reflection_checker_placeholders_and_kernel_field_claims_fail_closed() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let runner = directory.path().join("reflection-checker");
        fs::write(&runner, b"#!/bin/sh\ncat\n").unwrap();
        let mut permissions = fs::metadata(&runner).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&runner, permissions).unwrap();
        let runner_bytes = fs::read(&runner).unwrap();
        let runner_sha256 = super::super::canonical::raw_sha256(&runner_bytes);
        let evidence = VerifiedEvidenceClosure {
            manifest_sha256: super::super::canonical::raw_sha256(b"manifest"),
            evidence_tool_input_root: super::super::canonical::raw_sha256(b"evidence-root"),
            file_count: 1,
            leaves_by_logical_id: BTreeMap::from([(
                "reflection-checker".to_owned(),
                VerifiedEvidenceLeaf {
                    kind: "tool".to_owned(),
                    relative_path: "reflection-checker".to_owned(),
                    byte_length: runner_bytes.len() as u64,
                    raw_sha256: runner_sha256,
                    dependency_ids: Vec::new(),
                },
            )]),
        };
        let predecessor = super::super::canonical::raw_sha256(b"predecessor");
        let approval = super::super::canonical::raw_sha256(b"approval");
        let registry = SchemaRegistry::v1().unwrap();
        let execute = |input: &Value| {
            super::super::execution::execute_approved_json(
                super::super::execution::ApprovedExecutionRequest {
                    evidence: &evidence,
                    tool_logical_id: "reflection-checker",
                    purpose: "checked-refutation-reflection/v1",
                    journal_predecessor_sha256: predecessor,
                    execution: super::super::execution::PinnedExecutionRequest {
                        runner_path: &runner,
                        expected_runner_sha256: runner_sha256,
                        command_id: "reflection-check",
                        working_directory: directory.path(),
                        environment: &BTreeMap::new(),
                        input,
                        limits: super::super::execution::ExecutionLimits::default(),
                    },
                },
            )
            .unwrap()
        };

        let mut placeholder = reflection_body();
        placeholder["checked_source_refutation_proof_sha256"] =
            Value::String(Sha256Digest::ZERO.to_string());
        let receipt = execute(&placeholder);
        let error = build_reflection_result_from_receipt(
            &registry,
            &receipt,
            runner_sha256,
            approval,
            predecessor,
        )
        .unwrap_err();
        assert_eq!(error.code, "reflection_checker_output_placeholder_digest");

        let mut claims_kernel_field = reflection_body();
        claims_kernel_field["result_sha256"] = Value::String(Sha256Digest::ZERO.to_string());
        let receipt = execute(&claims_kernel_field);
        let error = build_reflection_result_from_receipt(
            &registry,
            &receipt,
            runner_sha256,
            approval,
            predecessor,
        )
        .unwrap_err();
        assert_eq!(error.code, "reflection_checker_output_claims_kernel_fields");
    }
}

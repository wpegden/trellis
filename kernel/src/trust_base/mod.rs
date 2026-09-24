//! Auditable trust-boundary records and validation.
//!
//! This module implements the v1 contract in
//! `trust-base-redesign-dossier/03_TRUST_BASE_DESIGN_SPEC_draft-2.3.md`
//! (the design record; it lives beside the repo and is not committed), as
//! rebuilt by the converged PV-framework plan (dossier doc 32).  Everything
//! the build pins is vendored into `schemas/` next to this file — see
//! `kernel/TRUST_BASE_SCHEMAS.md`.  No include here may reach outside the
//! crate, and `schemas/` holds `*.schema.json` and nothing else.
//!
//! Q1 (Stage 3): trust decisions are recorded math-mode-style — ordinary
//! supervisor event log plus git-tagged checkpoint history — via
//! `TrustRecord`; the parallel journal authority and its ed25519 PKI were
//! deleted with the rebuild.

pub mod archive;
pub mod adaptation_ledger;
pub mod artifact;
pub mod bootstrap;
pub mod campaign_plan;
pub mod canonical;
pub mod claim;
pub mod closure;
pub mod conditionalization;
pub mod execution;
pub mod package;
pub mod pipeline;
pub mod records;
pub mod schema;
pub mod seed;
pub mod source_tree;

pub use canonical::{
    canonical_json, canonical_json_value, parse_json_strict, raw_sha256, self_digest, tagged_hash,
    DecimalNatural, DomainTag, Sha256Digest, TrustError,
};
pub use adaptation_ledger::{
    adaptation_diff_digest, parse_compatible_adaptation_ledger, phase0_ablated_tree_digest, phase0_entry_id,
    replay_phase0_adaptation, verify_phase0_adaptation, ByteSpan, CheckerStream, CompatibleAdaptationLedger,
    ExactBytePatch, Phase0AdaptationEntry, Phase0AdaptationLedger, Phase0FileOperation,
    Phase0LedgerStatus, QuotedCheckerError, PHASE0_ADAPTATION_LEDGER_SCHEMA,
};
pub use archive::{verify_package_archive, VerifiedArchiveManifest};
pub use artifact::{
    accept_rust_witness_correspondence, artifact_gate_dossiers, artifact_package_members,
    attach_execution_receipt, build_rust_witness_correspondence_request,
    close_rust_witness_correspondence_verdict, run_rust_witness_artifact,
    rust_witness_correspondence_verdict_digest, rust_witness_relative_path,
    rust_witness_execution_available, rust_witness_target_key, snapshot_rust_witness_source,
    validate_rust_witness_declaration, validate_rust_witness_protocol_bindings,
    validate_rust_witness_state,
    RustWitnessArtifactDeclaration, RustWitnessArtifactPayload,
    RustWitnessArtifactRecord, RustWitnessCorrespondence, RustWitnessCorrespondenceDecision,
    RustWitnessCorrespondenceLaneVerdict, RustWitnessCorrespondenceRequest,
    RustWitnessCorrespondenceVerdict, RustWitnessExecution, RustWitnessReviewedDigests,
    RustWitnessRunContext, MAX_RUST_WITNESS_SOURCE_BYTES,
    RUST_WITNESS_ARTIFACT_SCHEMA, RUST_WITNESS_ARTIFACT_SCHEMA_ID,
    RUST_WITNESS_CORRESPONDENCE_REQUEST_SCHEMA, RUST_WITNESS_CORRESPONDENCE_VERDICT_SCHEMA,
    RUST_WITNESS_EXECUTION_PURPOSE, RUST_WITNESS_RUNNER_LOGICAL_ID,
    RUST_WITNESS_RUNNER_OUTPUT_SCHEMA,
};
pub use claim::{
    claim_rows_bytes, claim_rows_from_state, forbidden_phrase_reason, lint_claim_rows,
    parse_claim_rows, polarity_ledger_rows, refutation_dossier_rows, render_claim_document,
    terminal_outcome, ClaimContext, ClaimEdition, ClaimHeader, ClaimRow, ClaimRows,
    ReviewableAuthoredDefinition, RustArtifactSummary, TerminalOutcome,
    CLAIM_ROWS_SCHEMA, CLAIM_ROWS_SCHEMA_ID,
};
pub use bootstrap::{
    build_campaign_seed, build_conservative_campaign_seed, build_evidence_manifest,
    validate_aeneas_model_refinement,
    CampaignSeedRequest, CampaignTargetInput, ConservativeSeedRequest, ConstructedSeed,
    EvidenceInput,
};
pub use source_tree::{
    canonical_source_tree_manifest, read_source_tree, source_tree_digest,
    source_tree_manifest, source_tree_manifest_from_files, source_tree_manifest_value,
    source_tree_root_from_entries, SourceTreeEntry, SourceTreeManifest,
    SOURCE_TREE_MANIFEST_SCHEMA,
};
pub use campaign_plan::{
    AdaptationCitation, AdaptationLedgerEntry, AdaptationLedgerStatus,
    AdaptationPathDelta, AdaptationSeamClass, CampaignTrustPlan, TargetTrustPlan,
    CAMPAIGN_TRUST_PLAN_SCHEMA_V4,
};
pub use closure::{
    hydrate_seed_support_definition_files, mask_trusted_platform_boundary,
    seed_adaptation_ledger_projection, seed_phase0_trust_roots_projection,
    seed_support_definition_projection,
    verify_evidence_tool_manifest, verify_seed_definition_bundle,
    verify_seed_support_definition_files, ResolvedEvidenceLeaf,
    VerifiedEvidenceClosure, VerifiedEvidenceLeaf, VerifiedSeedDefinitionClosure,
};
pub use conditionalization::{
    build_conditional_correspondence_request, conditional_activation_payload,
    conditional_ratification_packet,
    seal_conditional_theorem, stamp_conditional_proposal, validate_conditional_protocol_state,
    ConditionalActivationPayload,
    ConditionalAssumptionSnapshot, ConditionalCandidateGeneration,
    ConditionalCorrespondenceApproval, ConditionalCorrespondenceRequest,
    ConditionalCorrespondenceVerdict, ConditionalDisposition, ConditionalEvidenceReferences,
    ConditionalObligation, ConditionalObligationKind, ConditionalRatification, ConditionalStage,
    ConditionalTheoremProposal, ConditionalTriggerClassification, SealedConditionalTheorem,
    StampedConditionalTheoremProposal, CONDITIONAL_THEOREM_REACHABLE,
};
pub use execution::{
    execute_approved_json, execute_phase0_checked_json, execute_pinned_json,
    validate_execution_receipt, validate_phase0_execution_receipt,
    ApprovedExecutionRequest, CheckedExecutionReceipt, ExecutionLimits, PinnedExecution,
    Phase0ExecutionRequest, PinnedExecutionRequest, PHASE0_CHECKED_EXECUTION_RECEIPT_SCHEMA,
};
pub use package::{
    assemble_trust_finalization_archive, verify_trust_finalization_archive,
    EmbeddedApprovalRecord, TrustFinalizationArchiveRequest, TRUST_BINDING_PATH,
};
pub use schema::{RecordContract, SchemaRegistry};
pub use seed::{validate_seed_manifest_semantics, SeedRoots};
pub use records::{
    AuthoritativeRecord, EventKind, TrustRecord, TrustRecordSeedRoots,
};
pub use pipeline::{
    CheckedNegativeProof, CheckedPositiveProof, RecordedCampaignProof, RecordedNegativeProof,
    RecordedPositiveProof, TrustDerivationPipeline,
};

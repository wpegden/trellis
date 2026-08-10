//! Auditable trust-boundary records, validation, and append-only authority.
//!
//! This module implements the v1 contract in
//! `trust-base-redesign-dossier/03_TRUST_BASE_DESIGN_SPEC_draft-2.3.md`
//! (the design record; it lives beside the repo and is not committed).
//! Everything the build pins is vendored into `schemas/` next to this file —
//! see `schemas/README.md`. No include here may reach outside the crate.
//! It intentionally does not reuse the supervisor's operational event log:
//! that log is rewindable, while this journal is the sole trust authority.

pub mod auth;
pub mod archive;
pub mod basis;
pub mod bootstrap;
pub mod campaign_plan;
pub mod canonical;
pub mod closure;
pub mod execution;
pub mod journal;
pub mod package;
pub mod pipeline;
pub mod qualification;
pub mod records;
pub mod revision_store;
pub mod schema;
pub mod seed;
pub mod source_validation;

pub use canonical::{
    canonical_json, canonical_json_value, parse_json_strict, raw_sha256, self_digest, tagged_hash,
    DecimalNatural, DomainTag, Sha256Digest, TrustError,
};
pub use archive::{verify_package_archive, VerifiedArchiveManifest};
pub use basis::{validate_independent_basis, ValidatedIndependentBasis};
pub use bootstrap::{
    build_actor_key_manifest, build_campaign_seed, build_conservative_campaign_seed,
    build_evidence_manifest, source_tree_digest, validate_aeneas_model_refinement,
    ActorPublicKeySpec, CampaignSeedRequest,
    CampaignTargetInput, ConservativeSeedRequest, ConstructedSeed, EvidenceInput,
};
pub use campaign_plan::{
    BasisFactClass, CampaignTrustPlan, ClaimShapeName, IndependentBasisFactPlan,
    PermittedApplicability, ResourceComparison, ResourceEnforcement,
    ResourceQualificationPlan, ResourceScope, ResourceUnits, SourcePreconditionPlan,
    TargetSourceValidationPlan, TargetTrustPlan, UnsupportedEvidenceClass,
};
pub use closure::{
    seed_support_definition_projection, seed_worker_projections, verify_evidence_tool_manifest,
    verify_seed_definition_bundle, verify_seed_support_definition_files, ResolvedEvidenceLeaf,
    VerifiedEvidenceClosure, VerifiedEvidenceLeaf, VerifiedSeedDefinitionClosure,
};
pub use execution::{
    execute_approved_json, execute_pinned_json, validate_execution_receipt,
    ApprovedExecutionRequest, CheckedExecutionReceipt, ExecutionLimits, PinnedExecution,
    PinnedExecutionRequest,
};
pub use auth::{
    sign_actor_receipt, sign_journal_commit_receipt, verify_actor_receipt,
    verify_journal_commit_receipt, ActorKeyManifest, JournalCommitReceiptContext,
    ManifestAuthorityRoots, ReceiptContext, VerifiedActorReceipt, VerifiedJournalCommitReceipt,
};
pub use schema::{RecordContract, SchemaRegistry};
pub use seed::{validate_seed_manifest_semantics, SeedRoots};
pub use source_validation::{
    route_qualified_recovery, HistoryFacts, ModelNegativeCarrierRequirement,
    QualificationRoute, SourceValidationContractView, SourceValidationMethod,
};
pub use records::{
    ActorAuthenticationMethod, ActorRole, AuthoritativeRecord, EventKind, JournalEvent,
    JournalEventPayload, JournalHead, JournalPolicy, ReferenceMode, Subject,
};
pub use revision_store::{
    load_revision_closure, publish_revision_closure, revision_closure_address,
    revision_closure_paths, ResolvedRevisionClosure, RevisionClosurePaths,
};
pub(crate) use journal::{AppendRequest, JournalActor};
pub use journal::{
    AuthorizedRevisionChange, AuthorizedRevisionProposal, JournalApprovalProjection,
    JournalCheckpointBinding, JournalRoutineGateOutcome, RevisionClosureProjection, TrustJournal,
};
pub use qualification::{
    evaluate_qualification, validate_qualification_prerequisites, QualificationEvidence,
    QualificationPrerequisites, QualifiedResult,
};
pub use package::{
    verify_authorized_package, AuthorizedPackage, PackageArtifact, PackageAuthorizationRequest,
    VerifiedPackageAuthorization,
};
pub use pipeline::{
    register_seed_lineages, CampaignTargetStatus, CheckedNegativeProof, CheckedPositiveProof,
    GeneratedExternalClaims, QualificationSelection,
    RecordedConditionalStatement,
    RecordedCampaignProof, RecordedModelRefutation, RecordedNegativeProof, RecordedPositiveProof,
    RecordedQualification, RecordedQualificationAttempt, RecordedReflectionExecution, RecordedSourceExecution,
    SeedQualificationProfile, SeedTargetContract, SourceToolInvocation, TrustDerivationPipeline,
};

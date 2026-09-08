//! Pre-bootstrap Phase-0 source-adaptation protocol.
//!
//! The bridge transports bytes and agent judgments.  This module owns every
//! successful transition, revalidates the raw-to-candidate replay relation on
//! every load, and accepts semantic judgments only after their mechanical
//! receipt and generation bindings have been checked.

use crate::request_contracts::prompt_contract_version;
use crate::trust_base::{
    canonical_json, canonical_json_value, parse_json_strict, phase0_ablated_tree_digest,
    raw_sha256, read_source_tree, self_digest, source_tree_manifest, source_tree_root_from_entries,
    tagged_hash,
    validate_phase0_execution_receipt, verify_phase0_adaptation, DomainTag,
    AdaptationLedgerEntry, AdaptationLedgerStatus, AdaptationPathDelta, CampaignTrustPlan,
    Phase0AdaptationLedger, Phase0LedgerStatus, Sha256Digest, SourceTreeEntry,
    SourceTreeManifest, TrustError,
    PHASE0_CHECKED_EXECUTION_RECEIPT_SCHEMA, SOURCE_TREE_MANIFEST_SCHEMA,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PHASE0_GENESIS_SCHEMA: &str = "trellis-phase0-genesis/v1";
pub const PHASE0_STATE_SCHEMA: &str = "trellis-phase0-state/v1";
pub const PHASE0_CONTEXT_SCHEMA: &str = "trellis-phase0-validation-context/v1";
pub const PHASE0_WORKER_REQUEST_SCHEMA: &str = "trellis-phase0-worker-request/v1";
pub const PHASE0_WORKER_RESPONSE_SCHEMA: &str = "trellis-phase0-worker-response/v1";
pub const PHASE0_AUDIT_REQUEST_SCHEMA: &str = "trellis-phase0-audit-request/v1";
pub const PHASE0_AUDIT_RESULT_SCHEMA: &str = "trellis-phase0-audit-result/v1";
pub const PHASE0_GOAL_BINDING_REPORT_SCHEMA: &str = "trellis-phase0-goal-binding-report/v1";
/// One hop, not a closure: what the reporter followed from each target.
pub const GOAL_BINDING_BODILESS_SCOPE: &str = "direct_calls_only";
pub const PHASE0_SEMANTIC_BUNDLE_SCHEMA: &str = "trellis-phase0-semantic-bundle/v1";
pub const PHASE0_SEALED_GENERATION_SCHEMA: &str = "trellis-phase0-sealed-generation/v1";
pub const PHASE0_PRODUCTION_RESULT_SCHEMA: &str = "trellis-phase0-production-result/v1";
pub const PHASE0_SOURCE_PARTITION_MANIFEST_SCHEMA: &str =
    "trellis-phase0-source-partition-manifest/v1";
pub const MAX_PHASE0_HANDBACKS: u32 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase0Stage {
    Worker,
    Check,
    SeamAudit,
    CorrespondenceAudit,
    Sealed,
    Incomplete,
}

impl Phase0Stage {
    pub const fn tla_name(self) -> &'static str {
        match self {
            Self::Worker => "Worker",
            Self::Check => "Check",
            Self::SeamAudit => "SeamAudit",
            Self::CorrespondenceAudit => "CorrespondenceAudit",
            Self::Sealed => "Sealed",
            Self::Incomplete => "Incomplete",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0PinnedFile {
    pub path: String,
    pub sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0PinnedTree {
    pub path: String,
    pub source_tree_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0PinnedCheckoutTool {
    pub executable: Phase0PinnedFile,
    pub source: Phase0PinnedTree,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0ComponentClosure {
    /// Canonical checkout root mounted read-only and used as PYTHONPATH by the
    /// checked runner. The broker working directory is not import authority.
    pub repository_root: String,
    pub kernel: Phase0PinnedFile,
    pub controller: Phase0PinnedFile,
    pub bridge: Phase0PinnedFile,
    pub checker_broker: Phase0PinnedFile,
    pub checker_runner: Phase0PinnedFile,
    pub extractor: Phase0PinnedFile,
    pub filespec_checker: Phase0PinnedFile,
    pub prescribed_region_checker: Phase0PinnedFile,
    pub production_extraction_runner: Phase0PinnedFile,
    pub goal_binding_reporter: Phase0PinnedFile,
    /// The checker runner imports this to derive the model's bodiless
    /// declarations, a fact the gate reads, so it is pinned like the runner.
    pub model_opacity_scanner: Phase0PinnedFile,
    pub worker_fragment: Phase0PinnedFile,
    pub seam_audit_fragment: Phase0PinnedFile,
    pub correspondence_audit_fragment: Phase0PinnedFile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0ToolchainClosure {
    pub cargo: Phase0PinnedFile,
    pub cargo_identity: String,
    pub rustc: Phase0PinnedFile,
    pub rustc_identity: String,
    pub toolchain_root: String,
    /// Charon's selected compiler closure. It is put directly on PATH so the
    /// checker does not need the larger mutable rustup installation.
    pub charon_toolchain: Phase0PinnedTree,
    pub charon: Phase0PinnedCheckoutTool,
    pub aeneas: Phase0PinnedCheckoutTool,
    pub dependency_cache: Option<Phase0PinnedTree>,
    pub opam_switch_prefix: Phase0PinnedTree,
    pub opam_switch_environment: BTreeMap<String, String>,
    pub opam_switch: String,
    pub target_triple: String,
    pub extraction_profile: String,
    pub closed_environment: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0LaneBinding {
    pub lane_id: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub extra_args: Vec<String>,
    pub fallback_models: Vec<String>,
    pub binding_sha256: Sha256Digest,
}

impl Phase0LaneBinding {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.binding_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.binding_sha256 = self_digest(DomainTag::RawArtifact, &value, "binding_sha256")?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), TrustError> {
        validate_identifier(&self.lane_id, "lane id")?;
        validate_text(&self.provider, "provider")?;
        validate_text(&self.model, "model")?;
        validate_text(&self.effort, "effort")?;
        let value = serde_json::to_value(self).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "binding_sha256")?
            != self.binding_sha256
        {
            return Err(phase0_error("lane binding self digest is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0Bindings {
    pub worker: Phase0LaneBinding,
    pub seam_repair: Vec<Phase0LaneBinding>,
    pub source_correspondence: Vec<Phase0LaneBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0Policy {
    pub max_phase0_handbacks: u32,
    pub allow_same_model_lanes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase0SourcePartitionOrigin {
    Target,
    Supporting,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0SourcePartitionEntry {
    pub relative_path: String,
    pub origin: Phase0SourcePartitionOrigin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0SourcePartitionManifest {
    pub schema: String,
    pub files: Vec<Phase0SourcePartitionEntry>,
    pub target_source_tree_sha256: Sha256Digest,
    pub supporting_source_tree_sha256: Sha256Digest,
    pub union_source_tree_sha256: Sha256Digest,
    pub partition_manifest_sha256: Sha256Digest,
}

impl Phase0SourcePartitionManifest {
    pub fn target_only(union: &SourceTreeManifest) -> Result<Self, TrustError> {
        validate_source_manifest(union)?;
        let mut manifest = Self {
            schema: PHASE0_SOURCE_PARTITION_MANIFEST_SCHEMA.to_owned(),
            files: union
                .files
                .iter()
                .map(|entry| Phase0SourcePartitionEntry {
                    relative_path: entry.relative_path.clone(),
                    origin: Phase0SourcePartitionOrigin::Target,
                })
                .collect(),
            target_source_tree_sha256: union.source_tree_sha256,
            supporting_source_tree_sha256: partition_source_digest(&[])?,
            union_source_tree_sha256: union.source_tree_sha256,
            partition_manifest_sha256: Sha256Digest::ZERO,
        };
        let value = serde_json::to_value(&manifest).map_err(phase0_serde)?;
        manifest.partition_manifest_sha256 = self_digest(
            DomainTag::RawArtifact,
            &value,
            "partition_manifest_sha256",
        )?;
        manifest.validate(union)?;
        Ok(manifest)
    }

    pub fn validate(&self, union: &SourceTreeManifest) -> Result<(), TrustError> {
        validate_source_manifest(union)?;
        if self.schema != PHASE0_SOURCE_PARTITION_MANIFEST_SCHEMA
            || self.files.len() != union.files.len()
            || self.union_source_tree_sha256 != union.source_tree_sha256
        {
            return Err(phase0_error("source partition manifest does not bind the union"));
        }
        let mut target = Vec::<SourceTreeEntry>::new();
        let mut supporting = Vec::<SourceTreeEntry>::new();
        for (partition, source) in self.files.iter().zip(&union.files) {
            if partition.relative_path != source.relative_path {
                return Err(phase0_error(
                    "source partition paths are not in canonical union order",
                ));
            }
            match partition.origin {
                Phase0SourcePartitionOrigin::Target => target.push(source.clone()),
                Phase0SourcePartitionOrigin::Supporting => supporting.push(source.clone()),
            }
        }
        if target.is_empty()
            || partition_source_digest(&target)? != self.target_source_tree_sha256
            || partition_source_digest(&supporting)? != self.supporting_source_tree_sha256
        {
            return Err(phase0_error("source partition digests are invalid"));
        }
        let value = serde_json::to_value(self).map_err(phase0_serde)?;
        if self_digest(
            DomainTag::RawArtifact,
            &value,
            "partition_manifest_sha256",
        )? != self.partition_manifest_sha256
        {
            return Err(phase0_error("source partition manifest self digest is invalid"));
        }
        Ok(())
    }
}

fn partition_source_digest(entries: &[SourceTreeEntry]) -> Result<Sha256Digest, TrustError> {
    if entries.is_empty() {
        return Ok(tagged_hash(
            DomainTag::ManifestNode,
            &canonical_json_value(&Value::Array(Vec::new()))?,
        ));
    }
    source_tree_root_from_entries(entries)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0Genesis {
    pub schema: String,
    pub target_upload_sha256: Sha256Digest,
    pub supporting_upload_sha256: Option<Sha256Digest>,
    pub unadapted_source_manifest: SourceTreeManifest,
    pub source_partition_manifest: Phase0SourcePartitionManifest,
    pub goal_base64: String,
    pub goal_sha256: Sha256Digest,
    pub policy: Phase0Policy,
    pub bindings: Phase0Bindings,
    pub prompt_contract_version: u32,
    pub components: Phase0ComponentClosure,
    pub toolchain: Phase0ToolchainClosure,
    pub genesis_sha256: Sha256Digest,
}

impl Phase0Genesis {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.genesis_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.genesis_sha256 = self_digest(DomainTag::RawArtifact, &value, "genesis_sha256")?;
        Ok(self)
    }

    pub fn parse_and_validate(bytes: &[u8]) -> Result<Self, TrustError> {
        let genesis: Self = parse_canonical(bytes, "Phase-0 genesis")?;
        genesis.validate()?;
        Ok(genesis)
    }

    pub fn validate(&self) -> Result<(), TrustError> {
        if self.schema != PHASE0_GENESIS_SCHEMA {
            return Err(phase0_error("genesis has the wrong schema"));
        }
        if self.target_upload_sha256 == Sha256Digest::ZERO
            || self.supporting_upload_sha256 == Some(Sha256Digest::ZERO)
            || self.goal_sha256 == Sha256Digest::ZERO
        {
            return Err(phase0_error("genesis contains a zero input digest"));
        }
        validate_source_manifest(&self.unadapted_source_manifest)?;
        self.source_partition_manifest
            .validate(&self.unadapted_source_manifest)?;
        let goal = decode_canonical_base64(&self.goal_base64, "GOAL bytes")?;
        std::str::from_utf8(&goal)
            .map_err(|error| phase0_error(format!("GOAL is not UTF-8: {error}")))?;
        if raw_sha256(&goal) != self.goal_sha256 || goal.is_empty() {
            return Err(phase0_error("GOAL bytes do not match the genesis digest"));
        }
        if self.policy.max_phase0_handbacks != MAX_PHASE0_HANDBACKS {
            return Err(phase0_error("MaxPhase0Handbacks must be the framework value 32"));
        }
        if self.prompt_contract_version != 64 || prompt_contract_version() != 64 {
            return Err(phase0_error("Phase-0 genesis must pin prompt contract 64"));
        }
        validate_bindings(&self.bindings, self.policy.allow_same_model_lanes)?;
        validate_absolute_path(&self.components.repository_root, "repository root")?;
        let repository_root = Path::new(&self.components.repository_root);
        for file in component_files(&self.components) {
            validate_pinned_file_shape(file)?;
            if !Path::new(&file.path).starts_with(repository_root)
                && file.path != self.components.kernel.path
                && file.path != self.components.filespec_checker.path
                && file.path != self.components.prescribed_region_checker.path
            {
                return Err(phase0_error(
                    "Genesis component lies outside the pinned repository root",
                ));
            }
        }
        validate_toolchain_shape(&self.toolchain)?;
        let value = serde_json::to_value(self).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "genesis_sha256")?
            != self.genesis_sha256
        {
            return Err(phase0_error("genesis self digest is invalid"));
        }
        Ok(())
    }

    /// Validate all byte and tree pins while constructing the genesis.  Later
    /// checker receipts carry the same identities, so drift is also rejected
    /// when a checked execution is accepted.
    pub fn validate_filesystem(&self) -> Result<(), TrustError> {
        self.validate()?;
        for file in component_files(&self.components)
            .into_iter()
            .chain([&self.toolchain.cargo, &self.toolchain.rustc])
            .chain([
                &self.toolchain.charon.executable,
                &self.toolchain.aeneas.executable,
            ])
        {
            validate_pinned_file_bytes(file)?;
        }
        for checkout in [&self.toolchain.charon, &self.toolchain.aeneas] {
            validate_git_checkout_bytes(checkout)?;
        }
        for tree in self.toolchain.dependency_cache.iter() {
            validate_pinned_tree_bytes(tree)?;
        }
        validate_readonly_closure_bytes(&self.toolchain.charon_toolchain)?;
        validate_readonly_closure_bytes(&self.toolchain.opam_switch_prefix)?;
        validate_opam_switch_environment_filesystem(&self.toolchain)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0ValidationContext {
    pub schema: String,
    pub unadapted_tree_path: String,
    pub candidate_tree_path: String,
    pub goal_targets: Vec<String>,
    pub ledger_path: String,
    pub receipt_store_path: String,
    pub goal_binding_report_path: String,
}

impl Phase0ValidationContext {
    pub fn parse_and_validate(bytes: &[u8]) -> Result<Self, TrustError> {
        let context: Self = parse_canonical(bytes, "Phase-0 validation context")?;
        if context.schema != PHASE0_CONTEXT_SCHEMA {
            return Err(phase0_error("validation context has the wrong schema"));
        }
        context.goal_target_set()?;
        for value in [
            &context.unadapted_tree_path,
            &context.candidate_tree_path,
            &context.ledger_path,
            &context.receipt_store_path,
            &context.goal_binding_report_path,
        ] {
            validate_absolute_path(value, "validation-context path")?;
        }
        Ok(context)
    }

    pub fn goal_target_set(&self) -> Result<BTreeSet<String>, TrustError> {
        if self.goal_targets.is_empty() {
            return Err(phase0_error("validation context has no GOAL targets"));
        }
        let mut output = BTreeSet::new();
        let mut previous: Option<&str> = None;
        for target in &self.goal_targets {
            if !target.starts_with("goal:") {
                return Err(phase0_error("GOAL target ids must start with goal:"));
            }
            validate_identifier(target, "GOAL target id")?;
            if previous.is_some_and(|item| item.as_bytes() >= target.as_bytes()) {
                return Err(phase0_error("GOAL targets must be strictly sorted"));
            }
            output.insert(target.clone());
            previous = Some(target);
        }
        Ok(output)
    }

    fn audit_paths(&self) -> Phase0AuditReadOnlyPaths {
        Phase0AuditReadOnlyPaths {
            unadapted_tree: self.unadapted_tree_path.clone(),
            adapted_tree: self.candidate_tree_path.clone(),
            ledger: self.ledger_path.clone(),
            receipt_store: self.receipt_store_path.clone(),
            goal_binding_report: self.goal_binding_report_path.clone(),
            extracted_model: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalBindingStatus {
    Bound,
    Missing,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalBindingOrigin {
    Target,
    Supporting,
    Adaptation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalBindingMatch {
    pub adapted_item_path: String,
    pub model_node: String,
    pub source_sha256: Sha256Digest,
}

/// The head the Lean backend emits for a declaration with no definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodilessDeclarationKind {
    Axiom,
    Opaque,
}

/// Whether the extracted model was read for bodiless declarations at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodilessScanStatus {
    /// The generated Lean was read; `declarations` is what it holds.
    Read,
    /// A model exists but could not be read; an empty list means nothing here.
    Unavailable,
    /// The check produced no model, so there was nothing to read.
    NotExtracted,
}

/// One declaration the extracted model carries without a definition.
///
/// The computation it stands for is an assumption: the artifact says nothing
/// about it. This is derived from the emitted model, never from the ledger's
/// prose or from any list of source markings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0BodilessDeclaration {
    pub name: String,
    pub kind: BodilessDeclarationKind,
    /// The source item the model attributes it to, empty when the model gives
    /// none.
    pub item_path: String,
}

/// The checker's scan of the model an extraction produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0ModelBodilessDeclarations {
    pub status: BodilessScanStatus,
    pub read_from: String,
    pub detail: String,
    pub declarations: Vec<Phase0BodilessDeclaration>,
}

/// Shape of a bodiless-declaration list, wherever it is carried.
fn validate_bodiless_declarations(
    status: BodilessScanStatus,
    declarations: &[Phase0BodilessDeclaration],
) -> Result<(), TrustError> {
    if status != BodilessScanStatus::Read && !declarations.is_empty() {
        return Err(phase0_error(
            "an unread model cannot carry bodiless declarations",
        ));
    }
    let mut previous: Option<(&str, BodilessDeclarationKind)> = None;
    for declaration in declarations {
        validate_text(&declaration.name, "bodiless declaration name")?;
        if !declaration.item_path.is_empty() {
            validate_text(&declaration.item_path, "bodiless declaration item path")?;
        }
        let key = (declaration.name.as_str(), declaration.kind);
        if previous.is_some_and(|item| (item.0.as_bytes(), item.1) >= (key.0.as_bytes(), key.1)) {
            return Err(phase0_error("bodiless declarations are not canonical"));
        }
        previous = Some(key);
    }
    Ok(())
}

fn bodiless_names(declarations: &[Phase0BodilessDeclaration]) -> BTreeSet<&str> {
    declarations
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect()
}

impl Phase0ModelBodilessDeclarations {
    pub fn validate(&self) -> Result<(), TrustError> {
        if self.read_from.is_empty() {
            return Err(phase0_error("bodiless scan does not say what it read"));
        }
        validate_text(&self.read_from, "bodiless scan source")?;
        if !self.detail.is_empty() {
            validate_text(&self.detail, "bodiless scan detail")?;
        }
        // Anything but a successful read says why there is nothing to report;
        // a read one carries the declarations instead of a reason.
        if self.detail.is_empty() != (self.status == BodilessScanStatus::Read) {
            return Err(phase0_error(
                "a scan that read no model must say why, and a read one must not",
            ));
        }
        validate_bodiless_declarations(self.status, &self.declarations)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalBindingTarget {
    pub target_id: String,
    pub name: String,
    pub resolution: String,
    pub heading_line: u64,
    pub status: GoalBindingStatus,
    pub matches: Vec<GoalBindingMatch>,
    pub origin: GoalBindingOrigin,
    /// True for a supporting-upstream or adaptation-authored binding.
    pub goal_binding_outside_target: bool,
    /// True exactly when the bound Rust item is supplied by an added file or
    /// by bytes inserted by the adaptation ledger.
    pub goal_binding_in_adaptation: bool,
    /// True when the target's own declaration in the model has no definition.
    pub model_declaration_bodiless: bool,
    /// Bodiless declarations the target's own model text writes down. One hop
    /// only; see `Phase0GoalBindingReport::model_bodiless_scope`.
    pub model_bodiless_direct_calls: Vec<String>,
    /// True exactly when either of the two above says the target's computation
    /// is an assumption rather than something the model defines.
    pub model_computation_suppressed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0GoalBindingReport {
    pub schema: String,
    pub goal_base64: String,
    pub goal_sha256: Sha256Digest,
    pub adapted_tree_sha256: Sha256Digest,
    pub model_bodiless_status: BodilessScanStatus,
    pub model_bodiless_declarations: Vec<Phase0BodilessDeclaration>,
    /// Fixed: `direct_calls_only`. A target is reported against the bodiless
    /// declarations its own model text writes down; what those reach in turn is
    /// not followed.
    pub model_bodiless_scope: String,
    pub targets: Vec<GoalBindingTarget>,
    pub report_sha256: Sha256Digest,
}

impl Phase0GoalBindingReport {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.report_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.report_sha256 = self_digest(DomainTag::RawArtifact, &value, "report_sha256")?;
        Ok(self)
    }

    pub fn parse_and_validate(bytes: &[u8]) -> Result<Self, TrustError> {
        let report: Self = parse_canonical(bytes, "Phase-0 GOAL-binding report")?;
        report.validate()?;
        Ok(report)
    }

    pub fn validate(&self) -> Result<(), TrustError> {
        if self.schema != PHASE0_GOAL_BINDING_REPORT_SCHEMA {
            return Err(phase0_error("GOAL-binding report has the wrong schema"));
        }
        let goal = decode_canonical_base64(&self.goal_base64, "GOAL-binding GOAL bytes")?;
        if raw_sha256(&goal) != self.goal_sha256
            || self.goal_sha256 == Sha256Digest::ZERO
            || self.adapted_tree_sha256 == Sha256Digest::ZERO
        {
            return Err(phase0_error("GOAL-binding report input digests are invalid"));
        }
        if self.model_bodiless_scope != GOAL_BINDING_BODILESS_SCOPE {
            return Err(phase0_error("GOAL-binding report bodiless scope is not direct-only"));
        }
        validate_bodiless_declarations(
            self.model_bodiless_status,
            &self.model_bodiless_declarations,
        )?;
        let bodiless = bodiless_names(&self.model_bodiless_declarations);
        let mut previous: Option<&str> = None;
        for target in &self.targets {
            validate_identifier(&target.target_id, "binding target id")?;
            validate_identifier(&target.name, "binding target name")?;
            if target.target_id != format!("goal:{}", target.name)
                || !matches!(target.resolution.as_str(), "decide" | "prove")
                || target.heading_line == 0
            {
                return Err(phase0_error("GOAL-binding target metadata is invalid"));
            }
            if previous.is_some_and(|item| item.as_bytes() >= target.target_id.as_bytes()) {
                return Err(phase0_error("GOAL-binding targets are not strictly sorted"));
            }
            previous = Some(&target.target_id);
            let expected_len = match target.status {
                GoalBindingStatus::Bound => 1,
                GoalBindingStatus::Missing => 0,
                GoalBindingStatus::Ambiguous => {
                    if target.matches.len() < 2 {
                        return Err(phase0_error("ambiguous GOAL binding needs multiple matches"));
                    }
                    target.matches.len()
                }
            };
            if target.matches.len() != expected_len {
                return Err(phase0_error("GOAL binding status disagrees with its matches"));
            }
            let bound = target.status == GoalBindingStatus::Bound;
            if target.goal_binding_outside_target
                != (bound && target.origin != GoalBindingOrigin::Target)
                || target.goal_binding_in_adaptation
                    != (bound && target.origin == GoalBindingOrigin::Adaptation)
            {
                return Err(phase0_error("GOAL binding origin flags are inconsistent"));
            }
            let mut prior_call: Option<&str> = None;
            for call in &target.model_bodiless_direct_calls {
                if !bodiless.contains(call.as_str())
                    || prior_call.is_some_and(|item| item.as_bytes() >= call.as_bytes())
                {
                    return Err(phase0_error(
                        "GOAL binding direct bodiless calls are not canonical",
                    ));
                }
                prior_call = Some(call);
            }
            if target.model_computation_suppressed
                != (target.model_declaration_bodiless
                    || !target.model_bodiless_direct_calls.is_empty())
                || (!bound
                    && (target.model_declaration_bodiless
                        || !target.model_bodiless_direct_calls.is_empty()))
            {
                return Err(phase0_error("GOAL binding opacity facts are inconsistent"));
            }
            let mut prior_path: Option<&str> = None;
            for binding in &target.matches {
                validate_text(&binding.adapted_item_path, "adapted item path")?;
                validate_text(&binding.model_node, "model node")?;
                if binding.source_sha256 == Sha256Digest::ZERO
                    || prior_path.is_some_and(|item| item.as_bytes() >= binding.adapted_item_path.as_bytes())
                {
                    return Err(phase0_error("GOAL binding matches are not canonical"));
                }
                prior_path = Some(&binding.adapted_item_path);
            }
        }
        let value = serde_json::to_value(self).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "report_sha256")?
            != self.report_sha256
        {
            return Err(phase0_error("GOAL-binding report self digest is invalid"));
        }
        Ok(())
    }

    pub fn is_complete(&self) -> bool {
        !self.targets.is_empty()
            && self
                .targets
                .iter()
                .all(|target| target.status == GoalBindingStatus::Bound)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0ReceiptEvidence {
    pub receipt_sha256: Sha256Digest,
    pub receipt: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase0AuditScenario {
    SeamRepair,
    SourceCorrespondence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0AuditReadOnlyPaths {
    pub unadapted_tree: String,
    pub adapted_tree: String,
    pub ledger: String,
    pub receipt_store: String,
    pub goal_binding_report: String,
    /// The exact model directory produced by the final ratification replay.
    /// Its lifetime is owned by the Phase-0 driver through both audit lanes.
    pub extracted_model: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0AuditFinding {
    pub code: String,
    pub entry_id: Option<String>,
    pub path: Option<String>,
    pub span_start: Option<u64>,
    pub span_end: Option<u64>,
    pub judgment: String,
    pub required_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0GoalBindingAuditTarget {
    pub target_id: String,
    pub origin: GoalBindingOrigin,
    pub goal_binding_outside_target: bool,
    pub goal_binding_in_adaptation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0UnifiedDiff {
    pub entry_id: String,
    pub rendered: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0AuditRequest {
    pub schema: String,
    pub request_id: String,
    pub scenario: Phase0AuditScenario,
    pub generation: u64,
    pub genesis_sha256: Sha256Digest,
    pub goal_sha256: Sha256Digest,
    pub unadapted_manifest_sha256: Sha256Digest,
    pub adapted_manifest_sha256: Sha256Digest,
    pub ledger_entries_root: Sha256Digest,
    pub ledger_sha256: Sha256Digest,
    pub semantic_bundle_sha256: Sha256Digest,
    pub final_checker_success_receipt_sha256: Sha256Digest,
    pub entry_receipts: Vec<Phase0EntryReceiptBinding>,
    pub unified_diffs: Vec<Phase0UnifiedDiff>,
    pub goal_binding_report_sha256: Sha256Digest,
    pub goal_binding_targets: Vec<Phase0GoalBindingAuditTarget>,
    pub lane: Phase0LaneBinding,
    pub prior_findings: Vec<Phase0AuditFinding>,
    pub read_only_paths: Phase0AuditReadOnlyPaths,
    /// Kernel-authored role-local delivery contract.  Prompt prose does not
    /// duplicate wire fields and a lane cannot negotiate a weaker shape.
    pub result_contract: Value,
    pub request_sha256: Sha256Digest,
}

impl Phase0AuditRequest {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.request_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.request_sha256 = self_digest(DomainTag::RawArtifact, &value, "request_sha256")?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), TrustError> {
        if self.schema != PHASE0_AUDIT_REQUEST_SCHEMA {
            return Err(phase0_error("audit request has the wrong schema"));
        }
        validate_identifier(&self.request_id, "audit request id")?;
        self.lane.validate()?;
        validate_findings(&self.prior_findings)?;
        validate_entry_receipt_bindings(&self.entry_receipts)?;
        if self.unified_diffs.windows(2).any(|pair| {
            pair[0].entry_id.as_bytes() >= pair[1].entry_id.as_bytes()
        }) {
            return Err(phase0_error("audit unified diffs are not canonical"));
        }
        for diff in &self.unified_diffs {
            validate_identifier(&diff.entry_id, "audit unified-diff entry id")?;
            validate_text(&diff.rendered, "audit unified diff")?;
        }
        if self.goal_binding_targets.is_empty()
            || self.goal_binding_targets.windows(2).any(|pair| {
                pair[0].target_id.as_bytes() >= pair[1].target_id.as_bytes()
            })
        {
            return Err(phase0_error("audit request GOAL-binding targets are not canonical"));
        }
        for target in &self.goal_binding_targets {
            validate_identifier(&target.target_id, "audit GOAL-binding target")?;
            if target.goal_binding_outside_target
                != (target.origin != GoalBindingOrigin::Target)
                || target.goal_binding_in_adaptation
                    != (target.origin == GoalBindingOrigin::Adaptation)
            {
                return Err(phase0_error(
                    "audit request GOAL-binding origin flags are inconsistent",
                ));
            }
        }
        if self.result_contract != audit_result_contract(self.scenario) {
            return Err(phase0_error("audit request has the wrong result contract"));
        }
        for path in [
            &self.read_only_paths.unadapted_tree,
            &self.read_only_paths.adapted_tree,
            &self.read_only_paths.ledger,
            &self.read_only_paths.receipt_store,
            &self.read_only_paths.goal_binding_report,
            &self.read_only_paths.extracted_model,
        ] {
            validate_absolute_path(path, "audit read-only path")?;
        }
        let value = serde_json::to_value(self).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "request_sha256")?
            != self.request_sha256
        {
            return Err(phase0_error("audit request self digest is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase0AuditDecision {
    Pass,
    Findings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0EntryAuditVerdict {
    pub entry_id: String,
    pub decision: Phase0AuditDecision,
    pub judgment: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0WholeTreeVerdict {
    pub decision: Phase0AuditDecision,
    pub judgment: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0GoalBindingAuditVerdict {
    pub target_id: String,
    pub origin: GoalBindingOrigin,
    pub goal_binding_outside_target: bool,
    pub goal_binding_in_adaptation: bool,
    pub decision: Phase0AuditDecision,
    pub judgment: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0AuditResult {
    pub schema: String,
    pub request_id: String,
    pub request_sha256: Sha256Digest,
    pub scenario: Phase0AuditScenario,
    pub generation: u64,
    pub genesis_sha256: Sha256Digest,
    pub semantic_bundle_sha256: Sha256Digest,
    pub ledger_sha256: Sha256Digest,
    pub lane_id: String,
    pub lane_binding_sha256: Sha256Digest,
    pub decision: Phase0AuditDecision,
    pub entry_verdicts: Vec<Phase0EntryAuditVerdict>,
    pub whole_tree_verdict: Option<Phase0WholeTreeVerdict>,
    pub goal_binding_verdicts: Vec<Phase0GoalBindingAuditVerdict>,
    pub findings: Vec<Phase0AuditFinding>,
    pub result_sha256: Sha256Digest,
}

impl Phase0AuditResult {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.result_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.result_sha256 = self_digest(DomainTag::RawArtifact, &value, "result_sha256")?;
        Ok(self)
    }

    pub fn validate_for(
        &self,
        request: &Phase0AuditRequest,
        entry_ids: &[String],
    ) -> Result<(), TrustError> {
        if self.schema != PHASE0_AUDIT_RESULT_SCHEMA
            || self.request_id != request.request_id
            || self.request_sha256 != request.request_sha256
            || self.scenario != request.scenario
            || self.generation != request.generation
            || self.genesis_sha256 != request.genesis_sha256
            || self.semantic_bundle_sha256 != request.semantic_bundle_sha256
            || self.ledger_sha256 != request.ledger_sha256
            || self.lane_id != request.lane.lane_id
            || self.lane_binding_sha256 != request.lane.binding_sha256
        {
            return Err(phase0_error("audit result has a stale or wrong request binding"));
        }
        let verdict_ids: Vec<_> = self
            .entry_verdicts
            .iter()
            .map(|verdict| verdict.entry_id.clone())
            .collect();
        if verdict_ids != entry_ids {
            return Err(phase0_error("audit result must contain one verdict per exact entry"));
        }
        for verdict in &self.entry_verdicts {
            validate_text(&verdict.judgment, "entry audit judgment")?;
        }
        match self.scenario {
            Phase0AuditScenario::SeamRepair
                if self.whole_tree_verdict.is_some() || !self.goal_binding_verdicts.is_empty() => {
                return Err(phase0_error("seam-repair result must not add a whole-tree verdict"));
            }
            Phase0AuditScenario::SourceCorrespondence => {
                let whole = self.whole_tree_verdict.as_ref().ok_or_else(|| {
                    phase0_error("source-correspondence result needs a whole-tree verdict")
                })?;
                validate_text(&whole.judgment, "whole-tree audit judgment")?;
                if self.goal_binding_verdicts.len() != request.goal_binding_targets.len() {
                    return Err(phase0_error(
                        "source-correspondence result needs one GOAL-binding answer per target",
                    ));
                }
                for (answer, target) in self
                    .goal_binding_verdicts
                    .iter()
                    .zip(&request.goal_binding_targets)
                {
                    if answer.target_id != target.target_id
                        || answer.origin != target.origin
                        || answer.goal_binding_outside_target
                            != target.goal_binding_outside_target
                        || answer.goal_binding_in_adaptation
                            != target.goal_binding_in_adaptation
                    {
                        return Err(phase0_error(
                            "source-correspondence GOAL-binding answer is stale",
                        ));
                    }
                    validate_text(&answer.judgment, "GOAL-binding audit judgment")?;
                }
            }
            _ => {}
        }
        validate_findings(&self.findings)?;
        let verdict_has_findings = self
            .entry_verdicts
            .iter()
            .any(|verdict| verdict.decision == Phase0AuditDecision::Findings)
            || self
                .whole_tree_verdict
                .as_ref()
                .is_some_and(|verdict| verdict.decision == Phase0AuditDecision::Findings)
            || self
                .goal_binding_verdicts
                .iter()
                .any(|verdict| verdict.decision == Phase0AuditDecision::Findings);
        match self.decision {
            Phase0AuditDecision::Pass
                if verdict_has_findings || !self.findings.is_empty() =>
            {
                return Err(phase0_error("an audit pass cannot carry findings"));
            }
            Phase0AuditDecision::Findings
                if !verdict_has_findings || self.findings.is_empty() =>
            {
                return Err(phase0_error("a findings result needs verdicts and findings"));
            }
            _ => {}
        }
        let value = serde_json::to_value(self).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "result_sha256")?
            != self.result_sha256
        {
            return Err(phase0_error("audit result self digest is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0EntryReceiptBinding {
    pub entry_id: String,
    pub discovery_failure_receipt_sha256: Sha256Digest,
    pub ablation_failure_receipt_sha256: Sha256Digest,
    /// Candidate manifest root after omitting exactly this ledger row. It was
    /// rederived from the frozen unadapted tree before sealing.
    pub ablation_omitted_tree_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0SemanticBundle {
    pub schema: String,
    pub generation: u64,
    pub genesis_sha256: Sha256Digest,
    pub target_upload_sha256: Sha256Digest,
    pub supporting_upload_sha256: Option<Sha256Digest>,
    pub goal_sha256: Sha256Digest,
    pub unadapted_source_manifest: SourceTreeManifest,
    pub source_partition_manifest: Phase0SourcePartitionManifest,
    pub adapted_source_manifest: SourceTreeManifest,
    pub replayed_tree_sha256: Sha256Digest,
    pub ledger: Phase0AdaptationLedger,
    pub checker_receipts: Vec<Phase0ReceiptEvidence>,
    pub candidate_checker_success_receipt_sha256: Sha256Digest,
    pub final_checker_success_receipt_sha256: Sha256Digest,
    pub entry_receipts: Vec<Phase0EntryReceiptBinding>,
    pub goal_binding_report: Phase0GoalBindingReport,
    /// What the final clean check's model left without a definition. Carried
    /// here so bootstrap and the package see it without opening a receipt.
    pub model_bodiless_declarations: Phase0ModelBodilessDeclarations,
    pub semantic_bundle_sha256: Sha256Digest,
}

impl Phase0SemanticBundle {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.semantic_bundle_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.semantic_bundle_sha256 =
            self_digest(DomainTag::RawArtifact, &value, "semantic_bundle_sha256")?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), TrustError> {
        if self.schema != PHASE0_SEMANTIC_BUNDLE_SCHEMA {
            return Err(phase0_error("semantic bundle has the wrong schema"));
        }
        validate_source_manifest(&self.unadapted_source_manifest)?;
        self.source_partition_manifest
            .validate(&self.unadapted_source_manifest)?;
        validate_source_manifest(&self.adapted_source_manifest)?;
        self.ledger.validate(&goal_targets_from_report(&self.goal_binding_report))?;
        self.goal_binding_report.validate()?;
        self.model_bodiless_declarations.validate()?;
        if self.model_bodiless_declarations.status != self.goal_binding_report.model_bodiless_status
            || self.model_bodiless_declarations.declarations
                != self.goal_binding_report.model_bodiless_declarations
        {
            return Err(phase0_error(
                "semantic bundle and binding report disagree about the model's bodiless declarations",
            ));
        }
        if !self.goal_binding_report.is_complete()
            || self.goal_binding_report.adapted_tree_sha256
                != self.adapted_source_manifest.source_tree_sha256
            || self.replayed_tree_sha256 != self.adapted_source_manifest.source_tree_sha256
            || self.ledger.adapted_tree_sha256 != self.replayed_tree_sha256
            || self.ledger.unadapted_tree_sha256
                != self.unadapted_source_manifest.source_tree_sha256
            || self.ledger.generation != self.generation
            || self.target_upload_sha256 == Sha256Digest::ZERO
            || self.supporting_upload_sha256 == Some(Sha256Digest::ZERO)
        {
            return Err(phase0_error("semantic bundle roots or binding report disagree"));
        }
        validate_receipt_evidence_order(&self.checker_receipts)?;
        validate_entry_receipt_bindings(&self.entry_receipts)?;
        if self.entry_receipts.len() != self.ledger.entries.len()
            || self.ledger.entries.iter().any(|entry| {
                !self.entry_receipts.iter().any(|binding| {
                    binding.entry_id == entry.id
                        && binding.discovery_failure_receipt_sha256
                            == entry.discovery_failure_receipt_sha256
                        && binding.ablation_failure_receipt_sha256
                            == entry.ablation_failure_receipt_sha256
                })
            })
        {
            return Err(phase0_error("semantic bundle entry receipt bindings disagree"));
        }
        let value = serde_json::to_value(self).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "semantic_bundle_sha256")?
            != self.semantic_bundle_sha256
        {
            return Err(phase0_error("semantic bundle self digest is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0SealedGeneration {
    pub schema: String,
    pub generation: u64,
    pub genesis_sha256: Sha256Digest,
    pub semantic_bundle: Phase0SemanticBundle,
    pub seam_audit_requests: Vec<Phase0AuditRequest>,
    pub seam_audit_results: Vec<Phase0AuditResult>,
    pub correspondence_audit_requests: Vec<Phase0AuditRequest>,
    pub correspondence_audit_results: Vec<Phase0AuditResult>,
    pub sealed_generation_sha256: Sha256Digest,
}

impl Phase0SealedGeneration {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.sealed_generation_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.sealed_generation_sha256 =
            self_digest(DomainTag::RawArtifact, &value, "sealed_generation_sha256")?;
        Ok(self)
    }

    pub fn validate(&self, genesis: &Phase0Genesis) -> Result<(), TrustError> {
        if self.schema != PHASE0_SEALED_GENERATION_SCHEMA
            || self.generation != self.semantic_bundle.generation
            || self.genesis_sha256 != genesis.genesis_sha256
            || self.semantic_bundle.genesis_sha256 != genesis.genesis_sha256
            || self.semantic_bundle.target_upload_sha256 != genesis.target_upload_sha256
            || self.semantic_bundle.supporting_upload_sha256
                != genesis.supporting_upload_sha256
            || self.semantic_bundle.unadapted_source_manifest
                != genesis.unadapted_source_manifest
            || self.semantic_bundle.source_partition_manifest
                != genesis.source_partition_manifest
        {
            return Err(phase0_error("sealed-generation identity is invalid"));
        }
        self.semantic_bundle.validate()?;
        validate_sealed_receipt_closure(&self.semantic_bundle, genesis)?;
        validate_sealed_audit_pairs(
            &self.seam_audit_requests,
            &self.seam_audit_results,
            &self.semantic_bundle,
            Phase0AuditScenario::SeamRepair,
        )?;
        validate_complete_audit_set(
            &self.seam_audit_results,
            &genesis.bindings.seam_repair,
            Phase0AuditScenario::SeamRepair,
            self.generation,
        )?;
        validate_sealed_audit_pairs(
            &self.correspondence_audit_requests,
            &self.correspondence_audit_results,
            &self.semantic_bundle,
            Phase0AuditScenario::SourceCorrespondence,
        )?;
        validate_complete_audit_set(
            &self.correspondence_audit_results,
            &genesis.bindings.source_correspondence,
            Phase0AuditScenario::SourceCorrespondence,
            self.generation,
        )?;
        let value = serde_json::to_value(self).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "sealed_generation_sha256")?
            != self.sealed_generation_sha256
        {
            return Err(phase0_error("sealed-generation self digest is invalid"));
        }
        Ok(())
    }

    pub fn parse_and_validate(
        bytes: &[u8],
        genesis: &Phase0Genesis,
    ) -> Result<Self, TrustError> {
        let sealed: Self = parse_canonical(bytes, "Phase-0 sealed generation")?;
        sealed.validate(genesis)?;
        Ok(sealed)
    }
}

/// Seed/runtime-visible immutable identity of the complete Phase-0 handoff.
/// Verdict vectors are explicit because clean unanimity, rather than a single
/// aggregate bit, is the semantic audit authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0TrustRoots {
    pub allow_same_model_lanes: bool,
    pub sealed_generation_sha256: Sha256Digest,
    pub phase0_bundle_sha256: Sha256Digest,
    pub target_upload_sha256: Sha256Digest,
    pub supporting_upload_sha256: Option<Sha256Digest>,
    pub source_partition_manifest_sha256: Sha256Digest,
    pub target_source_tree_sha256: Sha256Digest,
    pub supporting_source_tree_sha256: Sha256Digest,
    pub unadapted_source_tree_sha256: Sha256Digest,
    pub adapted_source_tree_sha256: Sha256Digest,
    pub goal_sha256: Sha256Digest,
    pub goal_binding_report_sha256: Sha256Digest,
    pub adaptation_ledger_sha256: Sha256Digest,
    pub final_checker_receipt_sha256: Sha256Digest,
    pub seam_repair_verdict_sha256: Vec<Sha256Digest>,
    pub correspondence_verdict_sha256: Vec<Sha256Digest>,
    pub production_result_sha256: Sha256Digest,
}

impl Phase0TrustRoots {
    pub fn validate(&self) -> Result<(), TrustError> {
        if [
            self.sealed_generation_sha256,
            self.phase0_bundle_sha256,
            self.target_upload_sha256,
            self.source_partition_manifest_sha256,
            self.target_source_tree_sha256,
            self.supporting_source_tree_sha256,
            self.unadapted_source_tree_sha256,
            self.adapted_source_tree_sha256,
            self.goal_sha256,
            self.goal_binding_report_sha256,
            self.adaptation_ledger_sha256,
            self.final_checker_receipt_sha256,
            self.production_result_sha256,
        ]
        .contains(&Sha256Digest::ZERO)
            || self.supporting_upload_sha256 == Some(Sha256Digest::ZERO)
            || self.seam_repair_verdict_sha256.is_empty()
            || self.correspondence_verdict_sha256.is_empty()
            || self
                .seam_repair_verdict_sha256
                .iter()
                .chain(&self.correspondence_verdict_sha256)
                .any(|digest| *digest == Sha256Digest::ZERO)
        {
            return Err(phase0_error("Phase-0 trust roots are incomplete"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0ProductionResult {
    pub schema: String,
    pub allow_same_model_lanes: bool,
    pub sealed_generation_sha256: Sha256Digest,
    pub phase0_bundle_sha256: Sha256Digest,
    pub target_upload_sha256: Sha256Digest,
    pub supporting_upload_sha256: Option<Sha256Digest>,
    pub source_partition_manifest_sha256: Sha256Digest,
    pub target_source_tree_sha256: Sha256Digest,
    pub supporting_source_tree_sha256: Sha256Digest,
    pub unadapted_source_tree_sha256: Sha256Digest,
    pub adapted_source_tree_sha256: Sha256Digest,
    pub goal_sha256: Sha256Digest,
    pub goal_binding_report_sha256: Sha256Digest,
    pub adaptation_ledger_sha256: Sha256Digest,
    pub final_checker_receipt_sha256: Sha256Digest,
    pub seam_repair_verdict_sha256: Vec<Sha256Digest>,
    pub correspondence_verdict_sha256: Vec<Sha256Digest>,
    pub production_extraction_result_sha256: Sha256Digest,
    pub production_extraction_determinism_sha256: Sha256Digest,
    pub production_extractor_sha256: Sha256Digest,
    pub production_toolchain_sha256: Sha256Digest,
    pub production_generated_lean_sha256: Sha256Digest,
    pub production_tablet_root_sha256: Sha256Digest,
    pub production_result_sha256: Sha256Digest,
}

impl Phase0ProductionResult {
    fn seal(mut self) -> Result<Self, TrustError> {
        self.production_result_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.production_result_sha256 =
            self_digest(DomainTag::RawArtifact, &value, "production_result_sha256")?;
        Ok(self)
    }

    pub fn parse_and_validate(bytes: &[u8]) -> Result<Self, TrustError> {
        let result: Self = parse_canonical(bytes, "Phase-0 production result")?;
        if result.schema != PHASE0_PRODUCTION_RESULT_SCHEMA {
            return Err(phase0_error("Phase-0 production result has invalid roots"));
        }
        result.trust_roots().validate()?;
        let value = serde_json::to_value(&result).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "production_result_sha256")?
            != result.production_result_sha256
        {
            return Err(phase0_error("Phase-0 production-result self digest is invalid"));
        }
        Ok(result)
    }

    pub fn trust_roots(&self) -> Phase0TrustRoots {
        Phase0TrustRoots {
            allow_same_model_lanes: self.allow_same_model_lanes,
            sealed_generation_sha256: self.sealed_generation_sha256,
            phase0_bundle_sha256: self.phase0_bundle_sha256,
            target_upload_sha256: self.target_upload_sha256,
            supporting_upload_sha256: self.supporting_upload_sha256,
            source_partition_manifest_sha256: self.source_partition_manifest_sha256,
            target_source_tree_sha256: self.target_source_tree_sha256,
            supporting_source_tree_sha256: self.supporting_source_tree_sha256,
            unadapted_source_tree_sha256: self.unadapted_source_tree_sha256,
            adapted_source_tree_sha256: self.adapted_source_tree_sha256,
            goal_sha256: self.goal_sha256,
            goal_binding_report_sha256: self.goal_binding_report_sha256,
            adaptation_ledger_sha256: self.adaptation_ledger_sha256,
            final_checker_receipt_sha256: self.final_checker_receipt_sha256,
            seam_repair_verdict_sha256: self.seam_repair_verdict_sha256.clone(),
            correspondence_verdict_sha256: self.correspondence_verdict_sha256.clone(),
            production_result_sha256: self.production_result_sha256,
        }
    }
}

/// The only permitted plan projection. The full patch/receipt-bearing ledger
/// remains separately frozen; these compact rows exist for the established
/// later adaptation-ledger runtime surface.
pub fn phase0_campaign_ledger_projection(
    sealed: &Phase0SealedGeneration,
) -> Vec<AdaptationLedgerEntry> {
    sealed
        .semantic_bundle
        .ledger
        .entries
        .iter()
        .map(|entry| AdaptationLedgerEntry {
            id: entry.id.clone(),
            seam_class: entry.seam_class,
            paths: vec![AdaptationPathDelta {
                path: entry.file.clone(),
                before_sha256: entry.before_sha256,
                after_sha256: entry.after_sha256.unwrap_or_else(|| raw_sha256(b"")),
            }],
            citation: entry.citation.clone(),
            affected_targets: entry.affected_targets.clone(),
            meaning_change: entry.meaning_change,
            status: AdaptationLedgerStatus::Seed,
        })
        .collect()
}

pub fn verify_phase0_trust_plan(
    plan: &CampaignTrustPlan,
    sealed: &Phase0SealedGeneration,
) -> Result<(), TrustError> {
    plan.validate()?;
    if plan.adaptation_ledger != phase0_campaign_ledger_projection(sealed) {
        return Err(phase0_error(
            "campaign plan ledger differs from the sealed Phase-0 projection",
        ));
    }
    let expected: Vec<(String, crate::model::ChallengeResolution)> = sealed
        .semantic_bundle
        .goal_binding_report
        .targets
        .iter()
        .map(|target| {
            let resolution = match target.resolution.as_str() {
                "decide" => crate::model::ChallengeResolution::Decide,
                "prove" => crate::model::ChallengeResolution::Prove,
                _ => unreachable!("validated Phase-0 GOAL resolution"),
            };
            (target.target_id.clone(), resolution)
        })
        .collect();
    let actual: Vec<_> = plan
        .targets
        .iter()
        .map(|target| (target.target_id.clone(), target.resolution))
        .collect();
    if actual != expected {
        return Err(phase0_error(
            "campaign plan targets differ from the sealed GOAL-binding projection",
        ));
    }
    Ok(())
}

fn read_canonical_value(path: &Path, label: &str) -> Result<(Value, Vec<u8>), TrustError> {
    let bytes = fs::read(path)
        .map_err(|error| phase0_error(format!("cannot read {label} {}: {error}", path.display())))?;
    let value = parse_json_strict(&bytes)?;
    if canonical_json_value(&value)? != bytes {
        return Err(phase0_error(format!("{label} is not exact canonical JSON")));
    }
    Ok((value, bytes))
}

fn value_string<'a>(value: &'a Value, field: &str, label: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        phase0_error(format!("{label} lacks string field {field}"))
    })
}

fn digest_field(value: &Value, field: &str, label: &str) -> Result<Sha256Digest, TrustError> {
    value_string(value, field, label)?.parse()
}

fn production_tool_executable(command: &Value, tool: &str) -> Result<PathBuf, TrustError> {
    let runs = command
        .get("runs")
        .and_then(Value::as_array)
        .ok_or_else(|| phase0_error("production extraction command lacks runs"))?;
    let primary = runs
        .iter()
        .find(|run| run.get("run_role").and_then(Value::as_str) == Some("primary"))
        .ok_or_else(|| phase0_error("production extraction command lacks primary run"))?;
    let steps = primary
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| phase0_error("production extraction primary run lacks steps"))?;
    let step = steps
        .iter()
        .find(|step| step.get("tool").and_then(Value::as_str) == Some(tool))
        .ok_or_else(|| phase0_error(format!("production extraction lacks {tool} step")))?;
    let executable = step
        .get("argv")
        .and_then(Value::as_array)
        .and_then(|argv| argv.first())
        .and_then(Value::as_str)
        .ok_or_else(|| phase0_error(format!("production {tool} command lacks executable")))?;
    fs::canonicalize(executable)
        .map_err(|error| phase0_error(format!("cannot resolve production {tool}: {error}")))
}

pub fn verify_phase0_production(
    campaign_repo: &Path,
    genesis: &Phase0Genesis,
    sealed: &Phase0SealedGeneration,
) -> Result<Phase0ProductionResult, TrustError> {
    genesis.validate_filesystem()?;
    sealed.validate(genesis)?;
    let phase0 = campaign_repo.join(".trellis/phase0");
    let genesis_bytes = canonical_json_value(&serde_json::to_value(genesis).map_err(phase0_serde)?)?;
    let sealed_bytes = canonical_json_value(&serde_json::to_value(sealed).map_err(phase0_serde)?)?;
    if fs::read(phase0.join("GENESIS.json")).map_err(|error| {
        phase0_error(format!("cannot read copied Phase-0 genesis: {error}"))
    })? != genesis_bytes
        || fs::read(phase0.join("SEALED_GENERATION.json")).map_err(|error| {
            phase0_error(format!("cannot read copied sealed generation: {error}"))
        })? != sealed_bytes
    {
        return Err(phase0_error(
            "campaign Phase-0 genesis or sealed generation differs from authority bytes",
        ));
    }
    let bundle_bytes = canonical_json_value(
        &serde_json::to_value(&sealed.semantic_bundle).map_err(phase0_serde)?,
    )?;
    let ledger_bytes = canonical_json_value(
        &serde_json::to_value(&sealed.semantic_bundle.ledger).map_err(phase0_serde)?,
    )?;
    let report_bytes = canonical_json_value(
        &serde_json::to_value(&sealed.semantic_bundle.goal_binding_report).map_err(phase0_serde)?,
    )?;
    let partition_bytes = canonical_json_value(
        &serde_json::to_value(&sealed.semantic_bundle.source_partition_manifest)
            .map_err(phase0_serde)?,
    )?;
    for (name, expected) in [
        ("BUNDLE.json", bundle_bytes.as_slice()),
        ("ADAPTATION_LEDGER.json", ledger_bytes.as_slice()),
        ("GOAL_BINDING_REPORT.json", report_bytes.as_slice()),
        ("SOURCE_PARTITION_MANIFEST.json", partition_bytes.as_slice()),
    ] {
        if fs::read(phase0.join(name)).map_err(|error| {
            phase0_error(format!("cannot read campaign Phase-0 {name}: {error}"))
        })? != expected
        {
            return Err(phase0_error(format!(
                "campaign Phase-0 {name} differs from sealed bytes"
            )));
        }
    }
    let unadapted = source_tree_manifest(&campaign_repo.join("unadapted-crate"))?;
    let adapted = source_tree_manifest(&campaign_repo.join("crate"))?;
    if unadapted != sealed.semantic_bundle.unadapted_source_manifest
        || adapted != sealed.semantic_bundle.adapted_source_manifest
    {
        return Err(phase0_error(
            "campaign source trees differ from the sealed manifests and roots",
        ));
    }
    sealed
        .semantic_bundle
        .source_partition_manifest
        .validate(&unadapted)?;
    let goal_targets: BTreeSet<String> = sealed
        .semantic_bundle
        .goal_binding_report
        .targets
        .iter()
        .map(|target| target.target_id.clone())
        .collect();
    verify_phase0_adaptation(
        &campaign_repo.join("unadapted-crate"),
        &sealed.semantic_bundle.ledger,
        &goal_targets,
        &campaign_repo.join("crate"),
    )?;
    let goal = fs::read(campaign_repo.join("GOAL.md"))
        .map_err(|error| phase0_error(format!("cannot read campaign GOAL: {error}")))?;
    let genesis_goal = decode_canonical_base64(&genesis.goal_base64, "GOAL bytes")?;
    if goal != genesis_goal || raw_sha256(&goal) != sealed.semantic_bundle.goal_sha256 {
        return Err(phase0_error("campaign GOAL differs from the sealed exact bytes"));
    }

    let (command, _command_bytes) = read_canonical_value(
        &campaign_repo.join("extraction-evidence/COMMAND.json"),
        "production extraction command",
    )?;
    let (result, result_bytes) = read_canonical_value(
        &campaign_repo.join("extraction-evidence/RESULT.json"),
        "production extraction result",
    )?;
    let (determinism, determinism_bytes) = read_canonical_value(
        &campaign_repo.join("extraction-evidence/DETERMINISM.json"),
        "production extraction determinism",
    )?;
    if value_string(&result, "schema", "production extraction result")?
        != "trellis-extraction-result/v2"
        || value_string(&determinism, "schema", "production extraction determinism")?
            != "trellis-extraction-determinism/v2"
        || value_string(&determinism, "status", "production extraction determinism")?
            != "passed"
    {
        return Err(phase0_error("production extraction is not a deterministic pass"));
    }
    if digest_field(&determinism, "result_sha256", "production extraction determinism")?
        != raw_sha256(&result_bytes)
    {
        return Err(phase0_error("production determinism does not bind RESULT.json"));
    }
    if digest_field(&command, "extractor_script_sha256", "production extraction command")?
        != genesis.components.extractor.sha256
    {
        return Err(phase0_error(
            "production extractor bytes differ from the sealed checker extractor",
        ));
    }
    let gate_checkers = command
        .get("kernel_gate_checkers")
        .and_then(Value::as_array)
        .ok_or_else(|| phase0_error("production extraction lacks kernel gate checker pins"))?;
    let expected_gate_checkers = [
        (
            "kernel-scan-tablet-filespec",
            &genesis.components.filespec_checker,
        ),
        (
            "kernel-print-prescribed-region",
            &genesis.components.prescribed_region_checker,
        ),
    ];
    if gate_checkers.len() != expected_gate_checkers.len() {
        return Err(phase0_error(
            "production extraction has the wrong kernel gate checker closure",
        ));
    }
    for (record, (tool, expected)) in gate_checkers.iter().zip(expected_gate_checkers) {
        if value_string(record, "tool", "production gate checker")? != tool
            || value_string(record, "path", "production gate checker")? != expected.path
            || digest_field(record, "sha256", "production gate checker")? != expected.sha256
            || raw_sha256(&fs::read(&expected.path).map_err(|error| {
                phase0_error(format!("cannot read production {tool}: {error}"))
            })?) != expected.sha256
        {
            return Err(phase0_error(format!(
                "production {tool} differs from its Genesis pin"
            )));
        }
    }
    for (tool, expected) in [
        ("cargo-metadata", &genesis.toolchain.cargo),
        ("rustc-version", &genesis.toolchain.rustc),
        ("charon", &genesis.toolchain.charon.executable),
        ("aeneas", &genesis.toolchain.aeneas.executable),
    ] {
        let executable = production_tool_executable(&command, tool)?;
        if raw_sha256(&fs::read(&executable).map_err(|error| {
            phase0_error(format!("cannot read production {tool} executable: {error}"))
        })?) != expected.sha256
        {
            return Err(phase0_error(format!(
                "production {tool} bytes differ from the sealed checker tool"
            )));
        }
    }
    let checkout_records = command
        .get("tool_source_checkouts")
        .and_then(Value::as_array)
        .ok_or_else(|| phase0_error("production extraction lacks tool source checkouts"))?;
    if checkout_records.len() != 2 {
        return Err(phase0_error(
            "production extraction must bind exactly Charon and Aeneas checkouts",
        ));
    }
    for (tool, expected) in [
        ("charon", &genesis.toolchain.charon),
        ("aeneas", &genesis.toolchain.aeneas),
    ] {
        let record = checkout_records
            .iter()
            .find(|record| record.get("tool").and_then(Value::as_str) == Some(tool))
            .ok_or_else(|| phase0_error(format!("production extraction lacks {tool} checkout")))?;
        let root = fs::canonicalize(value_string(record, "repository_root", "tool checkout")?)
            .map_err(|error| phase0_error(format!("cannot resolve {tool} checkout: {error}")))?;
        let expected_root = fs::canonicalize(&expected.source.path)
            .map_err(|error| phase0_error(format!("cannot resolve sealed {tool} checkout: {error}")))?;
        let executable = fs::canonicalize(&expected.executable.path)
            .map_err(|error| phase0_error(format!("cannot resolve sealed {tool} executable: {error}")))?;
        let relative = executable
            .strip_prefix(&expected_root)
            .map_err(|_| phase0_error(format!("sealed {tool} executable is outside its checkout")))?
            .to_string_lossy()
            .replace('\\', "/");
        if root != expected_root
            || value_string(record, "commit", "tool checkout")? != expected.revision
            || value_string(record, "executable_relative_path", "tool checkout")? != relative
            || record.get("clean_worktree").and_then(Value::as_bool) != Some(true)
        {
            return Err(phase0_error(format!(
                "production {tool} checkout differs from the sealed checker tool root"
            )));
        }
    }
    let provenance = command.get("extraction_provenance").ok_or_else(|| {
        phase0_error("production extraction command lacks extraction provenance")
    })?;
    let extraction_toolchain = provenance
        .get("extraction_toolchain")
        .ok_or_else(|| phase0_error("production provenance lacks extraction toolchain"))?;
    if value_string(extraction_toolchain, "target_triple", "extraction toolchain")?
        != genesis.toolchain.target_triple
        || value_string(extraction_toolchain, "extraction_profile", "extraction toolchain")?
            != genesis.toolchain.extraction_profile
        || value_string(extraction_toolchain, "rustc_vv", "extraction toolchain")?.trim()
            != genesis.toolchain.rustc_identity
    {
        return Err(phase0_error(
            "production extraction toolchain facts differ from sealed checker facts",
        ));
    }
    let production_toolchain_sha256 = digest_field(
        provenance,
        "extractor_toolchain_sha256",
        "production extraction provenance",
    )?;
    let generated = determinism
        .get("generated_lean_sha256")
        .and_then(Value::as_array)
        .ok_or_else(|| phase0_error("production determinism lacks generated Lean roots"))?;
    let tablet = determinism
        .get("tablet_root_sha256")
        .and_then(Value::as_array)
        .ok_or_else(|| phase0_error("production determinism lacks Tablet roots"))?;
    if generated.len() != 2
        || tablet.len() != 2
        || generated[0] != generated[1]
        || tablet[0] != tablet[1]
    {
        return Err(phase0_error("production A/B extraction roots disagree"));
    }
    let production_generated_lean_sha256: Sha256Digest = generated[0]
        .as_str()
        .ok_or_else(|| phase0_error("production generated Lean root is invalid"))?
        .parse()?;
    let production_tablet_root_sha256: Sha256Digest = tablet[0]
        .as_str()
        .ok_or_else(|| phase0_error("production Tablet root is invalid"))?
        .parse()?;
    let final_receipt = sealed
        .semantic_bundle
        .checker_receipts
        .iter()
        .find(|receipt| {
            receipt.receipt_sha256
                == sealed.semantic_bundle.final_checker_success_receipt_sha256
        })
        .ok_or_else(|| phase0_error("sealed bundle lacks final checker receipt"))?;
    let checked = final_receipt
        .receipt
        .get("parsed_stdout")
        .ok_or_else(|| phase0_error("final checker receipt lacks parsed output"))?;
    if digest_field(checked, "candidate_tree_sha256", "final checker output")?
        != adapted.source_tree_sha256
        || digest_field(
            checked,
            "generated_lean_root_sha256",
            "final checker output",
        )? != production_generated_lean_sha256
        || digest_field(checked, "tablet_root_sha256", "final checker output")?
            != production_tablet_root_sha256
    {
        return Err(phase0_error(
            "production source/generated Lean/Tablet roots differ from final checker facts",
        ));
    }
    let expected_a_b_generated = checked
        .get("a_b_generated_lean_roots")
        .and_then(Value::as_array)
        .ok_or_else(|| phase0_error("final checker output lacks A/B generated roots"))?;
    let expected_a_b_tablet = checked
        .get("a_b_tablet_roots")
        .and_then(Value::as_array)
        .ok_or_else(|| phase0_error("final checker output lacks A/B Tablet roots"))?;
    if expected_a_b_generated != generated || expected_a_b_tablet != tablet {
        return Err(phase0_error(
            "production A/B roots differ from sealed final-checker A/B roots",
        ));
    }
    let seam_repair_verdict_sha256 = sealed
        .seam_audit_results
        .iter()
        .map(|result| result.result_sha256)
        .collect();
    let correspondence_verdict_sha256 = sealed
        .correspondence_audit_results
        .iter()
        .map(|result| result.result_sha256)
        .collect();
    Phase0ProductionResult {
        schema: PHASE0_PRODUCTION_RESULT_SCHEMA.to_owned(),
        allow_same_model_lanes: genesis.policy.allow_same_model_lanes,
        sealed_generation_sha256: sealed.sealed_generation_sha256,
        phase0_bundle_sha256: sealed.semantic_bundle.semantic_bundle_sha256,
        target_upload_sha256: sealed.semantic_bundle.target_upload_sha256,
        supporting_upload_sha256: sealed.semantic_bundle.supporting_upload_sha256,
        source_partition_manifest_sha256: sealed
            .semantic_bundle
            .source_partition_manifest
            .partition_manifest_sha256,
        target_source_tree_sha256: sealed
            .semantic_bundle
            .source_partition_manifest
            .target_source_tree_sha256,
        supporting_source_tree_sha256: sealed
            .semantic_bundle
            .source_partition_manifest
            .supporting_source_tree_sha256,
        unadapted_source_tree_sha256: unadapted.source_tree_sha256,
        adapted_source_tree_sha256: adapted.source_tree_sha256,
        goal_sha256: sealed.semantic_bundle.goal_sha256,
        goal_binding_report_sha256: sealed.semantic_bundle.goal_binding_report.report_sha256,
        adaptation_ledger_sha256: sealed.semantic_bundle.ledger.ledger_sha256,
        final_checker_receipt_sha256: final_receipt.receipt_sha256,
        seam_repair_verdict_sha256,
        correspondence_verdict_sha256,
        production_extraction_result_sha256: raw_sha256(&result_bytes),
        production_extraction_determinism_sha256: raw_sha256(&determinism_bytes),
        production_extractor_sha256: genesis.components.extractor.sha256,
        production_toolchain_sha256,
        production_generated_lean_sha256,
        production_tablet_root_sha256,
        production_result_sha256: Sha256Digest::ZERO,
    }
    .seal()
}

pub fn verify_phase0_repository(
    campaign_repo: &Path,
    expected_roots: &Phase0TrustRoots,
) -> Result<(Phase0Genesis, Phase0SealedGeneration, Phase0ProductionResult), TrustError> {
    let phase0 = campaign_repo.join(".trellis/phase0");
    let read = |path: &Path, label: &str| {
        fs::read(path).map_err(|error| {
            phase0_error(format!("cannot read {label} {}: {error}", path.display()))
        })
    };
    let genesis = Phase0Genesis::parse_and_validate(&read(
        &phase0.join("GENESIS.json"),
        "Phase-0 genesis",
    )?)?;
    let sealed = Phase0SealedGeneration::parse_and_validate(
        &read(
            &phase0.join("SEALED_GENERATION.json"),
            "sealed Phase-0 generation",
        )?,
        &genesis,
    )?;
    let recorded = Phase0ProductionResult::parse_and_validate(&read(
        &phase0.join("PRODUCTION_RESULT.json"),
        "Phase-0 production result",
    )?)?;
    let verified = verify_phase0_production(campaign_repo, &genesis, &sealed)?;
    if recorded != verified || recorded.trust_roots() != *expected_roots {
        return Err(phase0_error(
            "live Phase-0 repository, production result, and seeded roots differ",
        ));
    }
    Ok((genesis, sealed, recorded))
}

fn exact_patch_diff(entry: &crate::trust_base::Phase0AdaptationEntry) -> Value {
    serde_json::json!({
        "schema": "trellis-exact-byte-diff/v1",
        "file": entry.file,
        "operation": entry.operation,
        "before_span": entry.before_span,
        "after_span": entry.after_span,
        "removed_base64": entry.patch.removed_base64,
        "removed_sha256": entry.patch.removed_sha256,
        "inserted_base64": entry.patch.inserted_base64,
        "inserted_sha256": entry.patch.inserted_sha256,
        "diff_sha256": entry.diff_sha256,
    })
}

fn render_unified_diff(
    entry: &crate::trust_base::Phase0AdaptationEntry,
) -> Phase0UnifiedDiff {
    // Base64 payload rows keep this display lossless for arbitrary source
    // bytes while retaining familiar unified-diff headers and +/- semantics.
    let rendered = format!(
        "--- a/{file}\n+++ b/{file}\n@@ -byte {before_start},{before_len} +byte {after_start},{after_len} @@\n-base64:{removed}\n+base64:{inserted}\n",
        file = entry.file,
        before_start = entry.before_span.start,
        before_len = entry.before_span.end - entry.before_span.start,
        after_start = entry.after_span.start,
        after_len = entry.after_span.end - entry.after_span.start,
        removed = entry.patch.removed_base64,
        inserted = entry.patch.inserted_base64,
    );
    Phase0UnifiedDiff {
        entry_id: entry.id.clone(),
        rendered,
    }
}

fn render_unified_diffs(
    ledger: &Phase0AdaptationLedger,
) -> Vec<Phase0UnifiedDiff> {
    let mut diffs: Vec<_> = ledger.entries.iter().map(render_unified_diff).collect();
    diffs.sort_by(|left, right| left.entry_id.as_bytes().cmp(right.entry_id.as_bytes()));
    diffs
}

/// What the extracted model leaves without a definition, and which GOAL
/// targets that reaches, in words a reader does not have to decode.
///
/// A declaration with no body is a computation the artifact says nothing
/// about. Nothing here forbids one; it is shown so the person at the gate
/// decides with the fact in front of them instead of inferring it from an
/// entry's prose.
pub fn phase0_model_opacity_section(bundle: &Phase0SemanticBundle) -> Value {
    let scan = &bundle.model_bodiless_declarations;
    let affected: Vec<&GoalBindingTarget> = bundle
        .goal_binding_report
        .targets
        .iter()
        .filter(|target| target.model_computation_suppressed)
        .collect();
    let rendered_names: Vec<String> = scan
        .declarations
        .iter()
        .map(|declaration| {
            let kind = match declaration.kind {
                BodilessDeclarationKind::Axiom => "axiom",
                BodilessDeclarationKind::Opaque => "opaque",
            };
            if declaration.item_path.is_empty() {
                format!("{} ({kind})", declaration.name)
            } else {
                format!("{} ({kind}, from {})", declaration.name, declaration.item_path)
            }
        })
        .collect();
    let summary = match scan.status {
        BodilessScanStatus::NotExtracted => {
            "No extracted model was read, so nothing is recorded about declarations left \
             without a definition."
                .to_owned()
        }
        BodilessScanStatus::Unavailable => format!(
            "The extracted model could not be read ({}), so whether any computation was left \
             as an assumption is unknown.",
            scan.detail
        ),
        BodilessScanStatus::Read if scan.declarations.is_empty() => {
            "Every declaration in the extracted model carries a definition: no computation \
             was left as an assumption."
                .to_owned()
        }
        BodilessScanStatus::Read => {
            let targets = if affected.is_empty() {
                "No GOAL target's own declaration or direct calls are among them.".to_owned()
            } else {
                let rows: Vec<String> = affected
                    .iter()
                    .map(|target| {
                        if target.model_declaration_bodiless {
                            format!("{} (its own declaration)", target.target_id)
                        } else {
                            format!(
                                "{} (calls {})",
                                target.target_id,
                                target.model_bodiless_direct_calls.join(", ")
                            )
                        }
                    })
                    .collect();
                format!(
                    "GOAL targets whose computation is affected: {}.",
                    rows.join("; ")
                )
            };
            format!(
                "The extracted model leaves {} declaration(s) without a definition, so what \
                 they compute is assumed rather than modelled: {}. {} Direct calls only: what \
                 those declarations reach in turn was not followed.",
                scan.declarations.len(),
                rendered_names.join(", "),
                targets
            )
        }
    };
    serde_json::json!({
        "summary": summary,
        "scan_status": scan.status,
        "scan_read_from": scan.read_from,
        "scan_detail": scan.detail,
        "scope": bundle.goal_binding_report.model_bodiless_scope,
        "bodiless_declarations": scan.declarations,
        "affected_goal_targets": affected
            .iter()
            .map(|target| serde_json::json!({
                "target_id": target.target_id,
                "model_declaration_bodiless": target.model_declaration_bodiless,
                "model_bodiless_direct_calls": target.model_bodiless_direct_calls,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Full immutable source-adaptation dossier displayed at the existing human
/// advance gate. Every byte-bearing row is revalidated from the repository
/// before it is rendered; the exact-byte diff is derived only from the sealed
/// patch members, never reconstructed from the current filesystem.
pub fn phase0_advance_gate_section(
    campaign_repo: &Path,
    roots: &Phase0TrustRoots,
) -> Result<Value, TrustError> {
    let (_genesis, sealed, production) = verify_phase0_repository(campaign_repo, roots)?;
    let bundle = &sealed.semantic_bundle;
    let ledger_entries: Vec<Value> = bundle
        .ledger
        .entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "entry": entry,
                "exact_patch_diff": exact_patch_diff(entry),
                "unified_diff": render_unified_diff(entry),
                "seam_class": entry.seam_class,
                "affected_targets": entry.affected_targets,
                "quoted_checker_error_with_receipt": entry.quoted_error,
                "ablation_recurrence_receipt_sha256": entry.ablation_failure_receipt_sha256,
                "behavior_preservation_claim": entry.behavior_preservation_claim,
                "meaning_change": entry.meaning_change,
            })
        })
        .collect();
    let receipt = |digest: Sha256Digest| {
        bundle
            .checker_receipts
            .iter()
            .find(|receipt| receipt.receipt_sha256 == digest)
            .cloned()
    };
    let target_files: Vec<_> = bundle
        .source_partition_manifest
        .files
        .iter()
        .filter(|entry| entry.origin == Phase0SourcePartitionOrigin::Target)
        .map(|entry| entry.relative_path.clone())
        .collect();
    let supporting_files: Vec<_> = bundle
        .source_partition_manifest
        .files
        .iter()
        .filter(|entry| entry.origin == Phase0SourcePartitionOrigin::Supporting)
        .map(|entry| entry.relative_path.clone())
        .collect();
    let shim_items: Vec<_> = bundle
        .ledger
        .entries
        .iter()
        .filter(|entry| entry.operation == crate::trust_base::Phase0FileOperation::Add)
        .map(|entry| serde_json::json!({"entry_id": entry.id, "file": entry.file}))
        .collect();
    let goal_target_origins: Vec<_> = bundle
        .goal_binding_report
        .targets
        .iter()
        .map(|target| serde_json::json!({
            "target_id": target.target_id,
            "origin": target.origin,
            "goal_binding_outside_target": target.goal_binding_outside_target,
            "goal_binding_in_adaptation": target.goal_binding_in_adaptation,
        }))
        .collect();
    Ok(serde_json::json!({
        "section": "phase0_frozen_source_adaptation",
        "status": "sealed_no_separate_human_gate",
        "roots": roots,
        "unadapted_source_manifest": bundle.unadapted_source_manifest,
        "adapted_source_manifest": bundle.adapted_source_manifest,
        "target_upload_sha256": bundle.target_upload_sha256,
        "supporting_upload_sha256": bundle.supporting_upload_sha256,
        "source_partition_manifest": bundle.source_partition_manifest,
        "source_partitions": {
            "target_files": target_files,
            "supporting_files": supporting_files,
            "shim_items": shim_items,
            "goal_target_origins": goal_target_origins,
        },
        "ledger": {
            "ledger_sha256": bundle.ledger.ledger_sha256,
            "entries_root": bundle.ledger.ledger_entries_root,
            "entries": ledger_entries,
        },
        "candidate_clean_checker_receipt": receipt(bundle.candidate_checker_success_receipt_sha256),
        "final_clean_checker_receipt": receipt(bundle.final_checker_success_receipt_sha256),
        "entry_receipt_bindings": bundle.entry_receipts,
        "seam_repair_audit_requests": sealed.seam_audit_requests,
        "seam_repair_verdicts": sealed.seam_audit_results,
        "source_correspondence_audit_requests": sealed.correspondence_audit_requests,
        "source_correspondence_verdicts": sealed.correspondence_audit_results,
        "goal_binding_report": bundle.goal_binding_report,
        "model_opacity": phase0_model_opacity_section(bundle),
        "production_extraction": production,
    }))
}

fn phase0_source_snapshot(path: &Path, label: &str) -> Result<Vec<u8>, TrustError> {
    let manifest = source_tree_manifest(path)?;
    let files = read_source_tree(path)?;
    let rows: Vec<Value> = files
        .into_iter()
        .map(|(relative_path, bytes)| {
            serde_json::json!({
                "relative_path": relative_path,
                "byte_length": bytes.len(),
                "raw_sha256": raw_sha256(&bytes),
                "bytes_base64": BASE64_STANDARD.encode(bytes),
            })
        })
        .collect();
    canonical_json_value(&serde_json::json!({
        "schema": "trellis-phase0-source-snapshot/v1",
        "label": label,
        "manifest": manifest,
        "files": rows,
    }))
}

fn push_phase0_json_artifact(
    artifacts: &mut Vec<crate::trust_base::package::PackageArtifact>,
    role: &str,
    path: String,
    value: Value,
) -> Result<(), TrustError> {
    artifacts.push(crate::trust_base::package::PackageArtifact {
        role: role.to_owned(),
        path,
        bytes: canonical_json_value(&value)?,
    });
    Ok(())
}

/// Generic final-package artifacts for Phase 0. The ordinary package
/// assembler hashes and indexes these bytes and runtime finalization re-reads
/// every member after the atomic archive rename.
pub fn phase0_package_artifacts(
    campaign_repo: &Path,
    roots: &Phase0TrustRoots,
) -> Result<Vec<crate::trust_base::package::PackageArtifact>, TrustError> {
    let (genesis, sealed, production) = verify_phase0_repository(campaign_repo, roots)?;
    let mut artifacts = Vec::new();
    push_phase0_json_artifact(&mut artifacts, "phase0_genesis", "phase0/GENESIS.json".into(), serde_json::to_value(&genesis).map_err(phase0_serde)?)?;
    push_phase0_json_artifact(&mut artifacts, "phase0_sealed_generation", "phase0/SEALED_GENERATION.json".into(), serde_json::to_value(&sealed).map_err(phase0_serde)?)?;
    push_phase0_json_artifact(&mut artifacts, "phase0_semantic_bundle", "phase0/BUNDLE.json".into(), serde_json::to_value(&sealed.semantic_bundle).map_err(phase0_serde)?)?;
    push_phase0_json_artifact(&mut artifacts, "phase0_adaptation_ledger", "phase0/ADAPTATION_LEDGER.json".into(), serde_json::to_value(&sealed.semantic_bundle.ledger).map_err(phase0_serde)?)?;
    push_phase0_json_artifact(&mut artifacts, "phase0_goal_binding_report", "phase0/GOAL_BINDING_REPORT.json".into(), serde_json::to_value(&sealed.semantic_bundle.goal_binding_report).map_err(phase0_serde)?)?;
    push_phase0_json_artifact(&mut artifacts, "phase0_source_partition_manifest", "phase0/SOURCE_PARTITION_MANIFEST.json".into(), serde_json::to_value(&sealed.semantic_bundle.source_partition_manifest).map_err(phase0_serde)?)?;
    push_phase0_json_artifact(&mut artifacts, "phase0_model_opacity", "phase0/MODEL_OPACITY.json".into(), phase0_model_opacity_section(&sealed.semantic_bundle))?;
    push_phase0_json_artifact(&mut artifacts, "phase0_production_result", "phase0/PRODUCTION_RESULT.json".into(), serde_json::to_value(&production).map_err(phase0_serde)?)?;
    artifacts.push(crate::trust_base::package::PackageArtifact {
        role: "phase0_goal".into(),
        path: "phase0/GOAL.md".into(),
        bytes: fs::read(campaign_repo.join("GOAL.md")).map_err(|error| {
            phase0_error(format!("cannot read campaign GOAL for package: {error}"))
        })?,
    });
    artifacts.push(crate::trust_base::package::PackageArtifact {
        role: "phase0_unadapted_source_snapshot".into(),
        path: "phase0/source/unadapted.snapshot.json".into(),
        bytes: phase0_source_snapshot(&campaign_repo.join("unadapted-crate"), "unadapted")?,
    });
    artifacts.push(crate::trust_base::package::PackageArtifact {
        role: "phase0_adapted_source_snapshot".into(),
        path: "phase0/source/adapted.snapshot.json".into(),
        bytes: phase0_source_snapshot(&campaign_repo.join("crate"), "adapted")?,
    });
    for receipt in &sealed.semantic_bundle.checker_receipts {
        push_phase0_json_artifact(&mut artifacts,
            "phase0_checker_receipt",
            format!("phase0/receipts/{}.json", receipt.receipt_sha256),
            receipt.receipt.clone(),
        )?;
    }
    for (kind, requests, results) in [
        ("seam-repair", &sealed.seam_audit_requests, &sealed.seam_audit_results),
        ("source-correspondence", &sealed.correspondence_audit_requests, &sealed.correspondence_audit_results),
    ] {
        for request in requests {
            push_phase0_json_artifact(&mut artifacts,
                "phase0_audit_request",
                format!("phase0/audits/{kind}/requests/{}.json", request.request_sha256),
                serde_json::to_value(request).map_err(phase0_serde)?,
            )?;
        }
        for result in results {
            push_phase0_json_artifact(&mut artifacts,
                "phase0_audit_verdict",
                format!("phase0/audits/{kind}/verdicts/{}.json", result.result_sha256),
                serde_json::to_value(result).map_err(phase0_serde)?,
            )?;
        }
    }
    Ok(artifacts)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0WorkerRequest {
    pub schema: String,
    pub request_id: String,
    pub generation: u64,
    pub genesis_sha256: Sha256Digest,
    pub goal_sha256: Sha256Digest,
    pub goal_base64: String,
    /// The exact GOAL target ids (`goal:<heading>`, byte-sorted) that
    /// `affected_targets` may name. Without them the worker has to guess
    /// the id convention from the GOAL text and the kernel refuses the draft.
    pub goal_targets: Vec<String>,
    pub candidate_tree_sha256: Sha256Digest,
    pub candidate_tree_path: String,
    /// The uploaded (unadapted) tree, read-only: `before_span` offsets are
    /// measured in these files, never in the already-edited candidate.
    pub unadapted_tree_path: String,
    pub ledger: Phase0AdaptationLedger,
    pub ledger_sha256: Sha256Digest,
    pub worker_binding_sha256: Sha256Digest,
    pub checker_failure_receipt_sha256: Option<Sha256Digest>,
    pub checker_failure_receipt: Option<Phase0ReceiptEvidence>,
    pub goal_binding_report: Option<Phase0GoalBindingReport>,
    pub prior_findings: Vec<Phase0AuditFinding>,
    pub prompt_fragments: Vec<String>,
    pub response_contract: Value,
    pub request_sha256: Sha256Digest,
}

impl Phase0WorkerRequest {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.request_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.request_sha256 = self_digest(DomainTag::RawArtifact, &value, "request_sha256")?;
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0WorkerResponse {
    pub schema: String,
    pub request_id: String,
    pub request_sha256: Sha256Digest,
    pub base_generation: u64,
    pub worker_thread_id: String,
    pub candidate_manifest: SourceTreeManifest,
    pub ledger: Phase0AdaptationLedger,
    /// Failed candidate checks produced by the genesis-bound interactive
    /// broker during this worker burst, in execution order.
    pub discovery_receipts: Vec<Value>,
    pub response_sha256: Sha256Digest,
}

impl Phase0WorkerResponse {
    pub fn seal(mut self) -> Result<Self, TrustError> {
        self.response_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.response_sha256 = self_digest(DomainTag::RawArtifact, &value, "response_sha256")?;
        Ok(self)
    }

    pub fn parse_and_validate(bytes: &[u8]) -> Result<Self, TrustError> {
        let response: Self = parse_canonical(bytes, "Phase-0 worker response")?;
        let value = serde_json::to_value(&response).map_err(phase0_serde)?;
        if response.schema != PHASE0_WORKER_RESPONSE_SCHEMA
            || self_digest(DomainTag::RawArtifact, &value, "response_sha256")?
                != response.response_sha256
        {
            return Err(phase0_error("worker response schema or self digest is invalid"));
        }
        validate_identifier(&response.worker_thread_id, "worker thread id")?;
        validate_source_manifest(&response.candidate_manifest)?;
        Ok(response)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0StateEventRecord {
    pub sequence: u64,
    pub generation: u64,
    pub kind: String,
    pub evidence_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0RetiredGeneration {
    pub generation: u64,
    pub candidate_manifest: SourceTreeManifest,
    pub ledger: Phase0AdaptationLedger,
    pub semantic_bundle: Option<Phase0SemanticBundle>,
    pub audit_requests: Vec<Phase0AuditRequest>,
    pub audit_results: Vec<Phase0AuditResult>,
    pub terminal_findings: Vec<Phase0AuditFinding>,
    pub record_sha256: Sha256Digest,
}

impl Phase0RetiredGeneration {
    fn seal(mut self) -> Result<Self, TrustError> {
        self.record_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.record_sha256 = self_digest(DomainTag::RawArtifact, &value, "record_sha256")?;
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0State {
    pub schema: String,
    pub genesis: Phase0Genesis,
    pub stage: Phase0Stage,
    pub generation: u64,
    pub phase0_handbacks: u32,
    pub initial_diagnostic_returned: bool,
    pub worker_thread_id: Option<String>,
    pub candidate_manifest: SourceTreeManifest,
    pub ledger: Phase0AdaptationLedger,
    pub checker_receipts: Vec<Phase0ReceiptEvidence>,
    pub last_failure_receipt_sha256: Option<Sha256Digest>,
    pub candidate_success_receipt_sha256: Option<Sha256Digest>,
    pub final_success_receipt_sha256: Option<Sha256Digest>,
    pub extracted_model_path: Option<String>,
    /// The final clean check's scan of the model it produced. Set with the
    /// other check facts when the check completes, cleared with them.
    pub model_bodiless_declarations: Option<Phase0ModelBodilessDeclarations>,
    pub goal_binding_report: Option<Phase0GoalBindingReport>,
    pub semantic_bundle: Option<Phase0SemanticBundle>,
    pub audit_requests: Vec<Phase0AuditRequest>,
    pub audit_results: Vec<Phase0AuditResult>,
    pub pending_findings: Vec<Phase0AuditFinding>,
    pub sealed_generation: Option<Phase0SealedGeneration>,
    pub generation_history: Vec<Phase0RetiredGeneration>,
    pub event_history: Vec<Phase0StateEventRecord>,
    pub state_sha256: Sha256Digest,
}

/// The worker's inline copy of the failing checker receipt.
///
/// The full receipt is always on disk in the receipt store, and this kernel
/// keeps its own full copy in `checker_receipts`; every check the kernel and
/// the bridge run resolves against those, never against this view. The
/// projection bounds only what is rendered into the worker prompt, so a
/// receipt cannot drown out the rest of the worker's context. Measured on the
/// dec2flt run before this existed: a 355 KB worker prompt, 218 KB of which
/// was receipt the worker had no way to use.
///
/// Two things go, neither of them addressable by a citation:
///   * the top-level `stdout_base64`, which decodes to exactly `parsed_stdout`
///     and so is a byte-for-byte duplicate of it that a model cannot read;
///   * the captured streams of stages that PASSED. A `quoted_error` names a
///     `stage_index` and a `byte_offset` into that stage's stream, and only a
///     failing stage carries a diagnostic worth quoting.
///
/// The failing stage is left verbatim, so every quotable byte survives at its
/// original offset, and each stage keeps its digests and byte lengths so the
/// omission stays visible and checkable.
fn worker_receipt_view(
    evidence: &Phase0ReceiptEvidence,
    receipt_store_path: &str,
) -> Phase0ReceiptEvidence {
    let mut receipt = evidence.receipt.clone();
    let mut omitted: usize = 0;
    if let Some(object) = receipt.as_object_mut() {
        if let Some(duplicate) = object.remove("stdout_base64") {
            omitted += duplicate.as_str().map_or(0, str::len);
        }
        if let Some(stages) = object
            .get_mut("parsed_stdout")
            .and_then(|parsed| parsed.get_mut("stages"))
            .and_then(Value::as_array_mut)
        {
            for stage in stages.iter_mut() {
                let failed = stage.get("status").and_then(Value::as_str) == Some("failed");
                let Some(stage) = stage.as_object_mut() else {
                    continue;
                };
                // The sandbox wrapper goes from every stage, the failing one
                // included: `sandbox_argv` is the bwrap invocation, byte-identical
                // across stages and 8 KB of it each, and `environment` is the
                // runner's own env.  Neither is quotable and neither is anything a
                // worker can act on.  `command` stays, so the stage still says what
                // it ran.
                for boilerplate in ["sandbox_argv", "environment"] {
                    if let Some(value) = stage.remove(boilerplate) {
                        omitted += value.to_string().len();
                    }
                }
                if failed {
                    continue;
                }
                let mut dropped = false;
                for stream in ["stdout_base64", "stderr_base64"] {
                    if let Some(value) = stage.get_mut(stream) {
                        omitted += value.as_str().map_or(0, str::len);
                        *value = Value::String(String::new());
                        dropped = true;
                    }
                }
                if dropped {
                    stage.insert("streams_omitted_for_prompt".to_owned(), Value::Bool(true));
                }
            }
        }
        object.insert(
            "inline_view".to_owned(),
            Value::String(format!(
                "[TRUNCATED FOR PROMPT: {omitted} byte(s) omitted \u{2014} the top-level \
                 stdout duplicate and the captured streams of stages that passed. The failing \
                 stage is verbatim, so every quotable diagnostic is present at its original \
                 byte offset. Read the full receipt in the receipt store at {receipt_store_path}.]"
            )),
        );
    }
    Phase0ReceiptEvidence {
        receipt_sha256: evidence.receipt_sha256,
        receipt,
    }
}

impl Phase0State {
    pub fn initialize(
        genesis: Phase0Genesis,
        ledger: Phase0AdaptationLedger,
        context: &Phase0ValidationContext,
    ) -> Result<Self, TrustError> {
        genesis.validate_filesystem()?;
        let targets = context.goal_target_set()?;
        ledger.validate(&targets)?;
        if ledger.generation != 0
            || !ledger.entries.is_empty()
            || ledger.unadapted_tree_sha256
                != genesis.unadapted_source_manifest.source_tree_sha256
            || ledger.adapted_tree_sha256 != ledger.unadapted_tree_sha256
            || ledger.goal_sha256 != genesis.goal_sha256
        {
            return Err(phase0_error("initial ledger must be the generation-zero identity"));
        }
        let candidate_manifest = source_tree_manifest(Path::new(&context.candidate_tree_path))?;
        let unadapted_manifest = source_tree_manifest(Path::new(&context.unadapted_tree_path))?;
        if candidate_manifest != genesis.unadapted_source_manifest
            || unadapted_manifest != genesis.unadapted_source_manifest
        {
            return Err(phase0_error("initial candidate is not the frozen unadapted tree"));
        }
        let mut state = Self {
            schema: PHASE0_STATE_SCHEMA.to_owned(),
            genesis,
            stage: Phase0Stage::Check,
            generation: 0,
            phase0_handbacks: 0,
            initial_diagnostic_returned: false,
            worker_thread_id: None,
            candidate_manifest,
            ledger,
            checker_receipts: Vec::new(),
            last_failure_receipt_sha256: None,
            candidate_success_receipt_sha256: None,
            final_success_receipt_sha256: None,
            extracted_model_path: None,
            model_bodiless_declarations: None,
            goal_binding_report: None,
            semantic_bundle: None,
            audit_requests: Vec::new(),
            audit_results: Vec::new(),
            pending_findings: Vec::new(),
            sealed_generation: None,
            generation_history: Vec::new(),
            event_history: Vec::new(),
            state_sha256: Sha256Digest::ZERO,
        };
        state.reseal()?;
        state.validate(context)?;
        Ok(state)
    }

    pub fn parse_and_validate(
        bytes: &[u8],
        context: &Phase0ValidationContext,
    ) -> Result<Self, TrustError> {
        let state: Self = parse_canonical(bytes, "Phase-0 state")?;
        state.validate(context)?;
        Ok(state)
    }

    /// Parse a state for `apply` without validating it against the live
    /// candidate tree. `apply` performs the validation itself, against the
    /// supervisor's pre-burst snapshot for a worker revision (the live
    /// candidate already carries the proposed delta at that point) and
    /// against the live candidate for every other event. Validating here
    /// against the live tree first would reject every worker revision as
    /// "state source trees do not equal ledger replay".
    pub fn parse_for_apply(bytes: &[u8]) -> Result<Self, TrustError> {
        parse_canonical(bytes, "Phase-0 state")
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, TrustError> {
        canonical_json(self)
    }

    pub fn validate(&self, context: &Phase0ValidationContext) -> Result<(), TrustError> {
        self.validate_with_candidate(context, Path::new(&context.candidate_tree_path))
    }

    fn validate_with_candidate(
        &self,
        context: &Phase0ValidationContext,
        candidate_tree_path: &Path,
    ) -> Result<(), TrustError> {
        if self.schema != PHASE0_STATE_SCHEMA
            || self.generation != self.ledger.generation
            || self.phase0_handbacks > MAX_PHASE0_HANDBACKS
            || self.genesis.policy.max_phase0_handbacks != MAX_PHASE0_HANDBACKS
        {
            return Err(phase0_error("state schema, generation, or bound is invalid"));
        }
        self.genesis.validate_filesystem()?;
        let targets = context.goal_target_set()?;
        self.ledger.validate(&targets)?;
        validate_source_manifest(&self.candidate_manifest)?;
        let unadapted = source_tree_manifest(Path::new(&context.unadapted_tree_path))?;
        let candidate = source_tree_manifest(candidate_tree_path)?;
        if unadapted != self.genesis.unadapted_source_manifest
            || candidate != self.candidate_manifest
            || verify_phase0_adaptation(
                Path::new(&context.unadapted_tree_path),
                &self.ledger,
                &targets,
                candidate_tree_path,
            )? != self.candidate_manifest.source_tree_sha256
        {
            return Err(phase0_error("state source trees do not equal ledger replay"));
        }
        validate_receipt_evidence_order(&self.checker_receipts)?;
        let receipt_map = validate_all_receipts(self)?;
        validate_ledger_receipt_links(self, &receipt_map)?;
        validate_event_history(&self.event_history)?;
        validate_generation_history(self, context, &targets, &receipt_map)?;
        match (
            self.final_success_receipt_sha256,
            &self.model_bodiless_declarations,
        ) {
            (Some(digest), Some(scan)) => {
                scan.validate()?;
                let receipt = self
                    .checker_receipts
                    .iter()
                    .find(|evidence| evidence.receipt_sha256 == digest)
                    .ok_or_else(|| phase0_error("state lacks its final checker receipt"))?;
                if receipt_bodiless_declarations(&receipt.receipt)? != *scan {
                    return Err(phase0_error(
                        "state bodiless declarations are not the final checker's own scan",
                    ));
                }
            }
            (None, None) => {}
            _ => {
                return Err(phase0_error(
                    "bodiless declarations are recorded exactly with the final clean check",
                ))
            }
        }
        if let Some(report) = &self.goal_binding_report {
            report.validate()?;
            if report.goal_sha256 != self.genesis.goal_sha256
                || report.adapted_tree_sha256 != self.candidate_manifest.source_tree_sha256
                || goal_targets_from_report(report) != targets
            {
                return Err(phase0_error("GOAL-binding report belongs to another generation"));
            }
        }
        if let Some(bundle) = &self.semantic_bundle {
            bundle.validate()?;
            if bundle.generation != self.generation
                || bundle.genesis_sha256 != self.genesis.genesis_sha256
                || bundle.ledger != self.ledger
                || bundle.adapted_source_manifest != self.candidate_manifest
                || self.model_bodiless_declarations.as_ref()
                    != Some(&bundle.model_bodiless_declarations)
            {
                return Err(phase0_error("semantic bundle is stale"));
            }
        }
        validate_audit_history(self, context)?;
        match self.stage {
            Phase0Stage::SeamAudit | Phase0Stage::CorrespondenceAudit => {
                if self.semantic_bundle.is_none()
                    || self.extracted_model_path.is_none()
                    || !self.goal_binding_report.as_ref().is_some_and(|report| report.is_complete())
                {
                    return Err(phase0_error("audit stage lacks a complete checked bundle"));
                }
            }
            Phase0Stage::Sealed => {
                let sealed = self
                    .sealed_generation
                    .as_ref()
                    .ok_or_else(|| phase0_error("Sealed state lacks its sealed-generation record"))?;
                sealed.validate(&self.genesis)?;
                if self.semantic_bundle.as_ref() != Some(&sealed.semantic_bundle) {
                    return Err(phase0_error("state and sealed-generation bundles differ"));
                }
            }
            Phase0Stage::Incomplete => {
                if self.phase0_handbacks != MAX_PHASE0_HANDBACKS
                    || self.sealed_generation.is_some()
                {
                    return Err(phase0_error("Incomplete is legal only at the handback bound"));
                }
            }
            Phase0Stage::Worker | Phase0Stage::Check => {
                if self.sealed_generation.is_some() {
                    return Err(phase0_error("nonterminal state carries a sealed generation"));
                }
            }
        }
        let value = serde_json::to_value(self).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "state_sha256")? != self.state_sha256 {
            return Err(phase0_error("state self digest is invalid"));
        }
        Ok(())
    }

    pub fn next_request(
        &self,
        context: &Phase0ValidationContext,
    ) -> Result<Phase0NextRequest, TrustError> {
        self.validate(context)?;
        match self.stage {
            Phase0Stage::Check => Ok(Phase0NextRequest::Check(Phase0CheckRequest {
                schema: "trellis-phase0-check-request/v1".to_owned(),
                generation: self.generation,
                genesis_sha256: self.genesis.genesis_sha256,
                candidate_tree_sha256: self.candidate_manifest.source_tree_sha256,
                ledger_entries_root: self.ledger.ledger_entries_root,
                check_kind: "candidate_then_final_and_ablations".to_owned(),
            })),
            Phase0Stage::Worker => Ok(Phase0NextRequest::Worker(self.worker_request(context)?)),
            Phase0Stage::SeamAudit => Ok(Phase0NextRequest::Audit(
                self.next_audit_request(context, Phase0AuditScenario::SeamRepair)?,
            )),
            Phase0Stage::CorrespondenceAudit
                if self.scenario_complete(Phase0AuditScenario::SourceCorrespondence) => {
                    Ok(Phase0NextRequest::Seal(Phase0SealRequest {
                        schema: "trellis-phase0-seal-request/v1".to_owned(),
                        generation: self.generation,
                        genesis_sha256: self.genesis.genesis_sha256,
                        semantic_bundle_sha256: self
                            .semantic_bundle
                            .as_ref()
                            .ok_or_else(|| phase0_error("seal request lacks a semantic bundle"))?
                            .semantic_bundle_sha256,
                    }))
                }
            Phase0Stage::CorrespondenceAudit => Ok(Phase0NextRequest::Audit(
                self.next_audit_request(context, Phase0AuditScenario::SourceCorrespondence)?,
            )),
            Phase0Stage::Sealed => Ok(Phase0NextRequest::Terminal(Phase0TerminalRequest {
                schema: "trellis-phase0-terminal/v1".to_owned(),
                stage: Phase0Stage::Sealed,
                sealed_generation_sha256: self
                    .sealed_generation
                    .as_ref()
                    .map(|sealed| sealed.sealed_generation_sha256),
            })),
            Phase0Stage::Incomplete => Ok(Phase0NextRequest::Terminal(Phase0TerminalRequest {
                schema: "trellis-phase0-terminal/v1".to_owned(),
                stage: Phase0Stage::Incomplete,
                sealed_generation_sha256: None,
            })),
        }
    }

    pub fn apply(
        mut self,
        context: &Phase0ValidationContext,
        event: Phase0Event,
    ) -> Result<Self, TrustError> {
        // During a worker revision the live candidate already contains the
        // proposed delta.  Validate the pre-state against the supervisor's
        // immutable pre-burst snapshot, then validate the response against
        // the live candidate below.  No transition is allowed to silently
        // absorb an unaccounted write.
        if let Phase0Event::WorkerRevision {
            pre_burst_candidate_path,
            ..
        } = &event
        {
            validate_absolute_path(pre_burst_candidate_path, "pre-burst candidate snapshot")?;
            self.validate_with_candidate(context, Path::new(pre_burst_candidate_path))?;
        } else {
            self.validate(context)?;
        }
        match event {
            Phase0Event::CheckerFailure { receipt } => {
                if self.stage != Phase0Stage::Check {
                    return Err(phase0_error("checker failure is not legal in this stage"));
                }
                let digest = validate_receipt_for_state(&self, &receipt, false, None)?;
                insert_receipt(&mut self.checker_receipts, digest, receipt)?;
                self.last_failure_receipt_sha256 = Some(digest);
                let initial = self.generation == 0 && !self.initial_diagnostic_returned;
                if initial {
                    self.initial_diagnostic_returned = true;
                }
                // Checker failures are evidence, not semantic review
                // hand-backs. They return to the worker without consuming the
                // bounded audit/ablation/binding finding budget.
                self.stage = Phase0Stage::Worker;
                self.push_event("checker_failure", digest);
            }
            Phase0Event::WorkerRevision {
                response,
                pre_burst_candidate_path: _,
            } => {
                if self.stage != Phase0Stage::Worker {
                    return Err(phase0_error("worker revision is not legal in this stage"));
                }
                let request = self.worker_request(context)?;
                // Interactive self-check receipts are evidence the worker
                // gathered against its own edits inside this burst. They are
                // recorded, and a new entry may cite one of them, but they do
                // not displace the generation's authoritative candidate
                // failure: an entry that honestly quotes the diagnostic it
                // was written for must stay acceptable after the worker also
                // ran a self-check.
                let authoritative_failure = self.last_failure_receipt_sha256;
                let mut interactive_failures = BTreeSet::new();
                for receipt in &response.discovery_receipts {
                    let digest = validate_interactive_discovery_receipt(&self, receipt)?;
                    insert_receipt(&mut self.checker_receipts, digest, receipt.clone())?;
                    interactive_failures.insert(digest);
                }
                validate_worker_response(
                    &self,
                    &request,
                    &response,
                    context,
                    authoritative_failure,
                    &interactive_failures,
                )?;
                let retired = Phase0RetiredGeneration {
                    generation: self.generation,
                    candidate_manifest: self.candidate_manifest.clone(),
                    ledger: self.ledger.clone(),
                    semantic_bundle: self.semantic_bundle.clone(),
                    audit_requests: self.audit_requests.clone(),
                    audit_results: self.audit_results.clone(),
                    terminal_findings: self.pending_findings.clone(),
                    record_sha256: Sha256Digest::ZERO,
                }
                .seal()?;
                self.generation_history.push(retired);
                self.worker_thread_id = Some(response.worker_thread_id.clone());
                self.generation = response.ledger.generation;
                self.ledger = response.ledger;
                self.candidate_manifest = response.candidate_manifest;
                self.candidate_success_receipt_sha256 = None;
                self.final_success_receipt_sha256 = None;
                self.extracted_model_path = None;
                self.model_bodiless_declarations = None;
                self.goal_binding_report = None;
                self.semantic_bundle = None;
                self.audit_requests.clear();
                self.audit_results.clear();
                self.pending_findings.clear();
                self.stage = Phase0Stage::Check;
                self.push_event("worker_revision", response.response_sha256);
            }
            Phase0Event::CheckComplete {
                candidate_success_receipt,
                final_success_receipt,
                ablation_receipts,
                completed_ledger,
                goal_binding_report,
                extracted_model_path,
            } => {
                if self.stage != Phase0Stage::Check {
                    return Err(phase0_error("check completion is not legal in this stage"));
                }
                validate_completed_ledger(&self.ledger, &completed_ledger)?;
                let targets = context.goal_target_set()?;
                completed_ledger.validate(&targets)?;
                validate_absolute_path(&extracted_model_path, "extracted model path")?;
                let expected_extracted_model = checker_extracted_model_path(&final_success_receipt)?;
                if Path::new(&extracted_model_path) != expected_extracted_model {
                    return Err(phase0_error(
                        "check completion extracted model is not the final replay output",
                    ));
                }
                let candidate_digest = validate_receipt_for_state(
                    &self,
                    &candidate_success_receipt,
                    true,
                    Some("candidate"),
                )?;
                insert_receipt(
                    &mut self.checker_receipts,
                    candidate_digest,
                    candidate_success_receipt,
                )?;
                let final_digest = validate_receipt_for_state(
                    &self,
                    &final_success_receipt,
                    true,
                    Some("ratification_replay"),
                )?;
                let bodiless = receipt_bodiless_declarations(&final_success_receipt)?;
                insert_receipt(
                    &mut self.checker_receipts,
                    final_digest,
                    final_success_receipt,
                )?;
                if ablation_receipts.len() != completed_ledger.entries.len() {
                    return Err(phase0_error("check completion needs one ablation receipt per entry"));
                }
                let mut findings = Vec::new();
                let mut reverted_ids = BTreeSet::new();
                for receipt in ablation_receipts {
                    let reverted = checker_request_field(&receipt, "reverted_entry_id")?
                        .as_str()
                        .ok_or_else(|| phase0_error("ablation receipt lacks an entry id"))?
                        .to_owned();
                    let entry = completed_ledger
                        .entries
                        .iter()
                        .find(|entry| entry.id == reverted)
                        .ok_or_else(|| phase0_error("ablation receipt names an unknown entry"))?;
                    if !reverted_ids.insert(reverted.clone()) {
                        return Err(phase0_error("check completion repeats an ablation entry"));
                    }
                    let (digest, reproduces_failure) = validate_ablation_receipt_for_state(
                        &self,
                        &receipt,
                        entry,
                        context,
                        &targets,
                    )?;
                    if entry.ablation_failure_receipt_sha256 != digest {
                        return Err(phase0_error("ablation receipt does not match its ledger row"));
                    }
                    insert_receipt(&mut self.checker_receipts, digest, receipt)?;
                    if !reproduces_failure {
                        findings.push(Phase0AuditFinding {
                            code: "ablation_did_not_reproduce_diagnostic".to_owned(),
                            entry_id: Some(entry.id.clone()),
                            path: Some(entry.file.clone()),
                            span_start: Some(entry.before_span.start),
                            span_end: Some(entry.before_span.end),
                            judgment: "Omitting this entry did not reproduce its bound discovery failure."
                                .to_owned(),
                            required_revision: "Revise or remove the entry so its exact omission reproduces the cited failure."
                                .to_owned(),
                        });
                    }
                }
                goal_binding_report.validate()?;
                if goal_binding_report.goal_sha256 != self.genesis.goal_sha256
                    || goal_binding_report.adapted_tree_sha256
                        != self.candidate_manifest.source_tree_sha256
                    || goal_targets_from_report(&goal_binding_report) != targets
                {
                    return Err(phase0_error("final checker produced a stale GOAL-binding report"));
                }
                if goal_binding_report.model_bodiless_status != bodiless.status
                    || goal_binding_report.model_bodiless_declarations != bodiless.declarations
                {
                    return Err(phase0_error(
                        "GOAL-binding report and final checker disagree about the model's bodiless declarations",
                    ));
                }
                for target in &goal_binding_report.targets {
                    if target.status != GoalBindingStatus::Bound {
                        findings.push(Phase0AuditFinding {
                            code: "goal_binding_incomplete".to_owned(),
                            entry_id: None,
                            path: None,
                            span_start: None,
                            span_end: None,
                            judgment: format!(
                                "GOAL target {} is {:?} in the extracted model.",
                                target.target_id, target.status
                            ),
                            required_revision: "Revise the source adaptation so the target has one exact extracted counterpart."
                                .to_owned(),
                        });
                    }
                }
                if !findings.is_empty() {
                    findings.sort();
                    findings.dedup();
                    self.pending_findings = findings;
                    self.goal_binding_report = Some(goal_binding_report);
                    self.semantic_handback()?;
                    self.push_event("check_findings", final_digest);
                } else {
                    self.ledger = completed_ledger;
                    self.candidate_success_receipt_sha256 = Some(candidate_digest);
                    self.final_success_receipt_sha256 = Some(final_digest);
                    self.extracted_model_path = Some(extracted_model_path);
                    self.model_bodiless_declarations = Some(bodiless.clone());
                    self.goal_binding_report = Some(goal_binding_report.clone());
                    self.semantic_bundle = Some(self.build_semantic_bundle(
                        candidate_digest,
                        final_digest,
                        goal_binding_report,
                        bodiless,
                    )?);
                    self.stage = Phase0Stage::SeamAudit;
                    self.push_event("check_complete", final_digest);
                }
            }
            Phase0Event::AuditResult { request, result } => {
                let scenario = match self.stage {
                    Phase0Stage::SeamAudit => Phase0AuditScenario::SeamRepair,
                    Phase0Stage::CorrespondenceAudit => {
                        Phase0AuditScenario::SourceCorrespondence
                    }
                    _ => return Err(phase0_error("audit result is not legal in this stage")),
                };
                let expected = self.next_audit_request(context, scenario)?;
                if request != expected {
                    return Err(phase0_error("audit request is not the next kernel-generated request"));
                }
                let entry_ids = self.entry_ids();
                result.validate_for(&request, &entry_ids)?;
                self.audit_requests.push(request);
                self.audit_results.push(result.clone());
                sort_audit_history(&mut self.audit_requests, &mut self.audit_results);
                if result.decision == Phase0AuditDecision::Findings {
                    self.pending_findings = result.findings.clone();
                    self.semantic_handback()?;
                } else if self.scenario_complete(scenario) {
                    self.stage = match scenario {
                        Phase0AuditScenario::SeamRepair => Phase0Stage::CorrespondenceAudit,
                        Phase0AuditScenario::SourceCorrespondence => {
                            Phase0Stage::CorrespondenceAudit
                        }
                    };
                }
                self.push_event("audit_result", result.result_sha256);
            }
        }
        self.reseal()?;
        self.validate(context)?;
        Ok(self)
    }

    pub fn finalize(
        mut self,
        context: &Phase0ValidationContext,
    ) -> Result<(Self, Phase0SealedGeneration), TrustError> {
        self.validate(context)?;
        if self.stage != Phase0Stage::CorrespondenceAudit
            || !self.scenario_complete(Phase0AuditScenario::SeamRepair)
            || !self.scenario_complete(Phase0AuditScenario::SourceCorrespondence)
        {
            return Err(phase0_error("Phase 0 is not ready to seal"));
        }
        let bundle = self
            .semantic_bundle
            .clone()
            .ok_or_else(|| phase0_error("Phase 0 lacks its semantic bundle"))?;
        let seam = self.clean_results(Phase0AuditScenario::SeamRepair);
        let correspondence = self.clean_results(Phase0AuditScenario::SourceCorrespondence);
        let seam_requests = self.clean_requests(Phase0AuditScenario::SeamRepair);
        let correspondence_requests =
            self.clean_requests(Phase0AuditScenario::SourceCorrespondence);
        let sealed = Phase0SealedGeneration {
            schema: PHASE0_SEALED_GENERATION_SCHEMA.to_owned(),
            generation: self.generation,
            genesis_sha256: self.genesis.genesis_sha256,
            semantic_bundle: bundle,
            seam_audit_requests: seam_requests,
            seam_audit_results: seam,
            correspondence_audit_requests: correspondence_requests,
            correspondence_audit_results: correspondence,
            sealed_generation_sha256: Sha256Digest::ZERO,
        }
        .seal()?;
        sealed.validate(&self.genesis)?;
        self.stage = Phase0Stage::Sealed;
        self.sealed_generation = Some(sealed.clone());
        self.push_event("sealed", sealed.sealed_generation_sha256);
        self.reseal()?;
        self.validate(context)?;
        Ok((self, sealed))
    }

    fn worker_request(
        &self,
        context: &Phase0ValidationContext,
    ) -> Result<Phase0WorkerRequest, TrustError> {
        if self.last_failure_receipt_sha256.is_none() && self.pending_findings.is_empty() {
            return Err(phase0_error("worker stage lacks checker diagnostics or audit findings"));
        }
        Phase0WorkerRequest {
            schema: PHASE0_WORKER_REQUEST_SCHEMA.to_owned(),
            request_id: format!("phase0-worker-{}-{}", self.generation, &self.state_sha256.to_string()[..16]),
            generation: self.generation,
            genesis_sha256: self.genesis.genesis_sha256,
            goal_sha256: self.genesis.goal_sha256,
            goal_base64: self.genesis.goal_base64.clone(),
            goal_targets: context.goal_target_set()?.into_iter().collect(),
            candidate_tree_sha256: self.candidate_manifest.source_tree_sha256,
            candidate_tree_path: context.candidate_tree_path.clone(),
            unadapted_tree_path: context.unadapted_tree_path.clone(),
            ledger: self.ledger.clone(),
            ledger_sha256: self.ledger.ledger_sha256,
            worker_binding_sha256: self.genesis.bindings.worker.binding_sha256,
            checker_failure_receipt_sha256: self.last_failure_receipt_sha256,
            checker_failure_receipt: self.last_failure_receipt_sha256.and_then(|digest| {
                self.checker_receipts
                    .iter()
                    .find(|receipt| receipt.receipt_sha256 == digest)
                    .map(|evidence| worker_receipt_view(evidence, &context.receipt_store_path))
            }),
            goal_binding_report: self.goal_binding_report.clone(),
            prior_findings: self.all_prior_findings(),
            prompt_fragments: vec![
                "pv/source_adaptation/worker/05_role.md".to_owned(),
            ],
            response_contract: worker_draft_contract(),
            request_sha256: Sha256Digest::ZERO,
        }
        .seal()
    }

    fn next_audit_request(
        &self,
        context: &Phase0ValidationContext,
        scenario: Phase0AuditScenario,
    ) -> Result<Phase0AuditRequest, TrustError> {
        let bindings = match scenario {
            Phase0AuditScenario::SeamRepair => &self.genesis.bindings.seam_repair,
            Phase0AuditScenario::SourceCorrespondence => {
                &self.genesis.bindings.source_correspondence
            }
        };
        let lane = bindings
            .iter()
            .find(|binding| {
                !self.audit_results.iter().any(|result| {
                    result.generation == self.generation
                        && result.scenario == scenario
                        && result.lane_binding_sha256 == binding.binding_sha256
                        && result.decision == Phase0AuditDecision::Pass
                })
            })
            .ok_or_else(|| phase0_error("audit scenario has no pending lane"))?
            .clone();
        let bundle = self
            .semantic_bundle
            .as_ref()
            .ok_or_else(|| phase0_error("audit request lacks a semantic bundle"))?;
        let scenario_name = match scenario {
            Phase0AuditScenario::SeamRepair => "seam-repair",
            Phase0AuditScenario::SourceCorrespondence => "source-correspondence",
        };
        Phase0AuditRequest {
            schema: PHASE0_AUDIT_REQUEST_SCHEMA.to_owned(),
            request_id: format!(
                "phase0-audit-{}-{}-{}",
                self.generation, scenario_name, lane.lane_id
            ),
            scenario,
            generation: self.generation,
            genesis_sha256: self.genesis.genesis_sha256,
            goal_sha256: self.genesis.goal_sha256,
            unadapted_manifest_sha256: self
                .genesis
                .unadapted_source_manifest
                .source_tree_sha256,
            adapted_manifest_sha256: self.candidate_manifest.source_tree_sha256,
            ledger_entries_root: self.ledger.ledger_entries_root,
            ledger_sha256: self.ledger.ledger_sha256,
            semantic_bundle_sha256: bundle.semantic_bundle_sha256,
            final_checker_success_receipt_sha256: self
                .final_success_receipt_sha256
                .ok_or_else(|| phase0_error("audit request lacks final checker success"))?,
            entry_receipts: bundle.entry_receipts.clone(),
            unified_diffs: render_unified_diffs(&self.ledger),
            goal_binding_report_sha256: bundle.goal_binding_report.report_sha256,
            goal_binding_targets: bundle
                .goal_binding_report
                .targets
                .iter()
                .map(|target| Phase0GoalBindingAuditTarget {
                    target_id: target.target_id.clone(),
                    origin: target.origin,
                    goal_binding_outside_target: target.goal_binding_outside_target,
                    goal_binding_in_adaptation: target.goal_binding_in_adaptation,
                })
                .collect(),
            lane,
            prior_findings: self.all_prior_findings(),
            read_only_paths: {
                let mut paths = context.audit_paths();
                paths.extracted_model = self
                    .extracted_model_path
                    .clone()
                    .ok_or_else(|| phase0_error("audit request lacks the retained extracted model"))?;
                paths
            },
            result_contract: audit_result_contract(scenario),
            request_sha256: Sha256Digest::ZERO,
        }
        .seal()
    }

    fn build_semantic_bundle(
        &self,
        candidate_digest: Sha256Digest,
        final_digest: Sha256Digest,
        goal_binding_report: Phase0GoalBindingReport,
        model_bodiless_declarations: Phase0ModelBodilessDeclarations,
    ) -> Result<Phase0SemanticBundle, TrustError> {
        Phase0SemanticBundle {
            schema: PHASE0_SEMANTIC_BUNDLE_SCHEMA.to_owned(),
            generation: self.generation,
            genesis_sha256: self.genesis.genesis_sha256,
            target_upload_sha256: self.genesis.target_upload_sha256,
            supporting_upload_sha256: self.genesis.supporting_upload_sha256,
            goal_sha256: self.genesis.goal_sha256,
            unadapted_source_manifest: self.genesis.unadapted_source_manifest.clone(),
            source_partition_manifest: self.genesis.source_partition_manifest.clone(),
            adapted_source_manifest: self.candidate_manifest.clone(),
            replayed_tree_sha256: self.candidate_manifest.source_tree_sha256,
            ledger: self.ledger.clone(),
            checker_receipts: self.checker_receipts.clone(),
            candidate_checker_success_receipt_sha256: candidate_digest,
            final_checker_success_receipt_sha256: final_digest,
            entry_receipts: {
                let mut bindings: Vec<_> = self
                    .ledger
                    .entries
                    .iter()
                    .map(|entry| {
                        let receipt = self
                            .checker_receipts
                            .iter()
                            .find(|evidence| {
                                evidence.receipt_sha256
                                    == entry.ablation_failure_receipt_sha256
                            })
                            .ok_or_else(|| phase0_error("bundle lacks an ablation receipt"))?;
                        let omitted = checker_request_field(
                            &receipt.receipt,
                            "candidate_tree_sha256",
                        )?
                        .as_str()
                        .ok_or_else(|| phase0_error("ablation root is not a digest"))?
                        .parse()
                        .map_err(|_| phase0_error("ablation root is not a digest"))?;
                        Ok(Phase0EntryReceiptBinding {
                            entry_id: entry.id.clone(),
                            discovery_failure_receipt_sha256: entry
                                .discovery_failure_receipt_sha256,
                            ablation_failure_receipt_sha256: entry
                                .ablation_failure_receipt_sha256,
                            ablation_omitted_tree_sha256: omitted,
                        })
                    })
                    .collect::<Result<Vec<_>, TrustError>>()?;
                bindings.sort_by(|left, right| left.entry_id.as_bytes().cmp(right.entry_id.as_bytes()));
                bindings
            },
            goal_binding_report,
            model_bodiless_declarations,
            semantic_bundle_sha256: Sha256Digest::ZERO,
        }
        .seal()
    }

    fn scenario_complete(&self, scenario: Phase0AuditScenario) -> bool {
        let expected = match scenario {
            Phase0AuditScenario::SeamRepair => &self.genesis.bindings.seam_repair,
            Phase0AuditScenario::SourceCorrespondence => {
                &self.genesis.bindings.source_correspondence
            }
        };
        expected.iter().all(|binding| {
            self.audit_results.iter().any(|result| {
                result.generation == self.generation
                    && result.scenario == scenario
                    && result.lane_binding_sha256 == binding.binding_sha256
                    && result.decision == Phase0AuditDecision::Pass
            })
        })
    }

    fn all_prior_findings(&self) -> Vec<Phase0AuditFinding> {
        let mut findings: Vec<_> = self
            .generation_history
            .iter()
            .flat_map(|generation| generation.terminal_findings.iter().cloned())
            .chain(self.pending_findings.iter().cloned())
            .collect();
        findings.sort();
        findings.dedup();
        findings
    }

    fn clean_results(&self, scenario: Phase0AuditScenario) -> Vec<Phase0AuditResult> {
        let mut results: Vec<_> = self
            .audit_results
            .iter()
            .filter(|result| {
                result.generation == self.generation
                    && result.scenario == scenario
                    && result.decision == Phase0AuditDecision::Pass
            })
            .cloned()
            .collect();
        results.sort_by(|left, right| left.lane_id.as_bytes().cmp(right.lane_id.as_bytes()));
        results
    }

    fn clean_requests(&self, scenario: Phase0AuditScenario) -> Vec<Phase0AuditRequest> {
        let passing: BTreeSet<_> = self
            .audit_results
            .iter()
            .filter(|result| {
                result.generation == self.generation
                    && result.scenario == scenario
                    && result.decision == Phase0AuditDecision::Pass
            })
            .map(|result| result.request_sha256)
            .collect();
        let mut requests: Vec<_> = self
            .audit_requests
            .iter()
            .filter(|request| passing.contains(&request.request_sha256))
            .cloned()
            .collect();
        requests.sort_by(|left, right| left.lane.lane_id.as_bytes().cmp(right.lane.lane_id.as_bytes()));
        requests
    }

    fn entry_ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = self.ledger.entries.iter().map(|entry| entry.id.clone()).collect();
        ids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        ids
    }

    fn semantic_handback(&mut self) -> Result<(), TrustError> {
        self.phase0_handbacks = self
            .phase0_handbacks
            .checked_add(1)
            .ok_or_else(|| phase0_error("phase0_handbacks overflow"))?;
        if self.phase0_handbacks >= MAX_PHASE0_HANDBACKS {
            self.phase0_handbacks = MAX_PHASE0_HANDBACKS;
            self.stage = Phase0Stage::Incomplete;
        } else {
            self.stage = Phase0Stage::Worker;
        }
        Ok(())
    }

    fn push_event(&mut self, kind: &str, evidence_sha256: Sha256Digest) {
        self.event_history.push(Phase0StateEventRecord {
            sequence: self.event_history.len() as u64 + 1,
            generation: self.generation,
            kind: kind.to_owned(),
            evidence_sha256,
        });
    }

    fn reseal(&mut self) -> Result<(), TrustError> {
        self.checker_receipts
            .sort_by_key(|receipt| receipt.receipt_sha256);
        self.state_sha256 = Sha256Digest::ZERO;
        let value = serde_json::to_value(&self).map_err(phase0_serde)?;
        self.state_sha256 = self_digest(DomainTag::RawArtifact, &value, "state_sha256")?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Phase0Event {
    CheckerFailure {
        receipt: Value,
    },
    WorkerRevision {
        response: Phase0WorkerResponse,
        pre_burst_candidate_path: String,
    },
    CheckComplete {
        candidate_success_receipt: Value,
        final_success_receipt: Value,
        ablation_receipts: Vec<Value>,
        completed_ledger: Phase0AdaptationLedger,
        goal_binding_report: Phase0GoalBindingReport,
        extracted_model_path: String,
    },
    AuditResult {
        request: Phase0AuditRequest,
        result: Phase0AuditResult,
    },
}

impl Phase0Event {
    pub fn parse_canonical(bytes: &[u8]) -> Result<Self, TrustError> {
        parse_canonical(bytes, "Phase-0 event")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0CheckRequest {
    pub schema: String,
    pub generation: u64,
    pub genesis_sha256: Sha256Digest,
    pub candidate_tree_sha256: Sha256Digest,
    pub ledger_entries_root: Sha256Digest,
    pub check_kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0TerminalRequest {
    pub schema: String,
    pub stage: Phase0Stage,
    pub sealed_generation_sha256: Option<Sha256Digest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase0SealRequest {
    pub schema: String,
    pub generation: u64,
    pub genesis_sha256: Sha256Digest,
    pub semantic_bundle_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum Phase0NextRequest {
    Worker(Phase0WorkerRequest),
    Check(Phase0CheckRequest),
    Audit(Phase0AuditRequest),
    Seal(Phase0SealRequest),
    Terminal(Phase0TerminalRequest),
}

/// What the four seam classes mean. The adapting worker and the seam-repair
/// auditor read the identical bytes, so a class is not guessed on either side.
const SEAM_CLASS_SEMANTICS: &str = concat!(
    "i: a language or tool guarantee justifies the edit, cited as language_guarantee. ",
    "ii: upstream's own source or documentation justifies the edit, cited as ",
    "upstream_reference, and the edit re-attaches the executable computation. ",
    "iii: the edit deletes a computation; behavior_preservation_claim states what the ",
    "deleted computation denoted, with a harness receipt where one exists. ",
    "packaging_shim: a file or wiring the standalone package needs that upstream does not carry."
);

fn worker_draft_contract() -> Value {
    serde_json::json!({
        "schema": "trellis-phase0-response-contract/v1",
        "delivery_schema": "trellis-phase0-worker-draft/v1",
        "top_level_fields": ["schema", "request_id", "worker_thread_id", "entries"],
        "entry_fields": [
            "file", "operation", "before_span", "after_span", "before_sha256",
            "after_sha256", "patch", "quoted_error", "behavior_preservation_claim",
            "seam_class", "citation", "affected_targets", "meaning_change"
        ],
        "operation_values": ["add", "delete", "modify"],
        "patch_fields": ["removed_base64", "inserted_base64"],
        "quoted_error_fields": [
            "tool", "receipt_sha256", "stage_index", "stream", "byte_offset", "quote"
        ],
        "stream_values": ["stderr", "stdout"],
        "seam_class_values": ["i", "ii", "iii", "packaging_shim"],
        "seam_class_semantics": SEAM_CLASS_SEMANTICS,
        "model_opacity_semantics": "after every check the framework reads the extracted model and records every declaration the backend emitted without a body, with the GOAL targets whose own declaration or direct calls are among them; that record reaches the approval gate and the final package whatever an entry's seam class or behavior_preservation_claim says. An edit that keeps a computation in the extracted model is preferred to one that removes it from the model, and an entry whose edit removes a computation from the model states in behavior_preservation_claim what that computation denoted.",
        "citation_variants": ["language_guarantee", "upstream_reference"],
        "affected_targets_values": "a non-empty subset of this request's goal_targets, exactly as written there",
        "span_fields": ["start", "end"],
        "span_semantics": "before_span is the byte range of the UPLOADED file (read it under this request's unadapted_tree_path) that the patch replaces, even when the candidate already carries earlier edits to that file, start <= end; an add uses before_span {\"start\":0,\"end\":0}; several entries may modify one file as long as their before_spans do not overlap; after_span is computed by the bridge, send {\"start\":0,\"end\":0}",
        "sha256_semantics": "before_sha256/after_sha256 are whole-file digests computed by the bridge; send null",
        "citation_examples": [
            {"kind": "language_guarantee", "statement": "<what the language or tool guarantees>"},
            {"kind": "upstream_reference", "reference": "<where upstream documents it>"}
        ],
        "quoted_error_semantics": "quote is one complete line of the named stream of the receipt identified by receipt_sha256, starting at byte_offset (a line start); stage_index indexes that receipt's stages; the same line must recur when the entry is omitted, which the framework checks by re-running the tools without this entry and matching the quoted diagnostic in the same file, ignoring line and column numbers",
        "entries_semantics": "one entry per change this revision makes; every prior ledger entry is carried forward by the bridge unless an entry here names the same file and before_span, which replaces it",
        "candidate_tree_semantics": "every ledgered edit is already made in the candidate tree at this request's candidate_tree_path; the ledger only describes what that tree already holds, and the framework computes the candidate manifest from that tree",
        "worker_thread_id_semantics": "any non-empty string; the bridge binds the stable worker identity itself",
        "delivery_format": "one JSON object; any whitespace or key order is accepted and canonicalized by the bridge",
        "computed_by_framework": [
            "candidate_manifest", "entry_id", "patch_digests", "diff_sha256",
            "quote_length_and_digest", "ablation_receipt", "ledger_digests", "response_sha256",
            "worker_thread_id", "carried_forward_prior_entries", "before_sha256", "after_sha256", "after_span"
        ]
    })
}

fn audit_result_contract(scenario: Phase0AuditScenario) -> Value {
    let mut contract = serde_json::json!({
        "schema": "trellis-phase0-response-contract/v1",
        "delivery_schema": "trellis-phase0-audit-draft/v1",
        "top_level_fields": ["schema", "decision", "entry_verdicts", "whole_tree_verdict", "goal_binding_verdicts", "findings"],
        "decision_values": ["findings", "pass"],
        "entry_verdict_fields": ["entry_id", "decision", "judgment"],
        "whole_tree_verdict": match scenario {
            Phase0AuditScenario::SeamRepair => "must_be_null",
            Phase0AuditScenario::SourceCorrespondence => "required",
        },
        "goal_binding_verdict_fields": [
            "target_id", "origin", "goal_binding_outside_target",
            "goal_binding_in_adaptation", "decision", "judgment"
        ],
        "goal_binding_verdicts": match scenario {
            Phase0AuditScenario::SeamRepair => "must_be_empty",
            Phase0AuditScenario::SourceCorrespondence => "one_per_request_target",
        },
        "finding_fields": [
            "code", "entry_id", "path", "span_start", "span_end", "judgment",
            "required_revision"
        ],
        "computed_by_kernel_bridge": [
            "request_bindings", "generation", "lane_binding", "result_sha256"
        ]
    });
    if matches!(scenario, Phase0AuditScenario::SeamRepair) {
        contract["seam_class_semantics"] = Value::String(SEAM_CLASS_SEMANTICS.to_string());
    }
    contract
}

fn validate_worker_response(
    state: &Phase0State,
    request: &Phase0WorkerRequest,
    response: &Phase0WorkerResponse,
    context: &Phase0ValidationContext,
    authoritative_failure: Option<Sha256Digest>,
    interactive_failures: &BTreeSet<Sha256Digest>,
) -> Result<(), TrustError> {
    if response.schema != PHASE0_WORKER_RESPONSE_SCHEMA
        || response.request_id != request.request_id
        || response.request_sha256 != request.request_sha256
        || response.base_generation != state.generation
        || response.ledger.generation != state.generation + 1
        || response.ledger.goal_sha256 != state.genesis.goal_sha256
        || response.ledger.unadapted_tree_sha256
            != state.genesis.unadapted_source_manifest.source_tree_sha256
        || response.ledger.adapted_tree_sha256 != response.candidate_manifest.source_tree_sha256
        || state
            .worker_thread_id
            .as_ref()
            .is_some_and(|thread| thread != &response.worker_thread_id)
    {
        return Err(phase0_error("worker response is stale or belongs to another thread"));
    }
    let value = serde_json::to_value(response).map_err(phase0_serde)?;
    if self_digest(DomainTag::RawArtifact, &value, "response_sha256")?
        != response.response_sha256
    {
        return Err(phase0_error("worker response self digest is invalid"));
    }
    let targets = context.goal_target_set()?;
    response.ledger.validate(&targets)?;
    if response
        .ledger
        .entries
        .iter()
        .any(|entry| entry.status != Phase0LedgerStatus::Draft)
    {
        return Err(phase0_error("worker may submit only draft ledger entries"));
    }
    let actual = source_tree_manifest(Path::new(&context.candidate_tree_path))?;
    if actual != response.candidate_manifest
        || verify_phase0_adaptation(
            Path::new(&context.unadapted_tree_path),
            &response.ledger,
            &targets,
            Path::new(&context.candidate_tree_path),
        )? != actual.source_tree_sha256
    {
        return Err(phase0_error("worker ledger does not account for the actual tree delta"));
    }
    let receipt_map: BTreeSet<_> = state
        .checker_receipts
        .iter()
        .map(|receipt| receipt.receipt_sha256)
        .collect();
    let prior_diffs: BTreeSet<_> = state
        .ledger
        .entries
        .iter()
        .map(|entry| entry.diff_sha256)
        .collect();
    let has_new_entry = response
        .ledger
        .entries
        .iter()
        .any(|entry| !prior_diffs.contains(&entry.diff_sha256));
    let authoritative_failure = if has_new_entry {
        let digest = authoritative_failure
            .ok_or_else(|| phase0_error("worker revision lacks an authoritative candidate failure"))?;
        let authoritative_receipt = state
            .checker_receipts
            .iter()
            .find(|receipt| receipt.receipt_sha256 == digest)
            .ok_or_else(|| phase0_error("authoritative candidate failure is unavailable"))?;
        let authoritative_input = authoritative_receipt
            .receipt
            .get("input")
            .and_then(Value::as_object)
            .ok_or_else(|| phase0_error("authoritative candidate failure lacks its input"))?;
        if authoritative_input.get("check_kind").and_then(Value::as_str) != Some("candidate")
            || authoritative_input.get("ledger_generation").and_then(Value::as_u64)
                != Some(state.generation)
        {
            return Err(phase0_error(
                "worker revision is not based on this generation's authoritative candidate failure",
            ));
        }
        Some(digest)
    } else {
        None
    };
    // A new entry must be motivated by a failure observed against this
    // generation's candidate lineage: the authoritative checker failure the
    // worker was dispatched for, or an interactive self-check it ran and
    // delivered with this draft (already validated as a failed candidate
    // check of this lineage).
    if response.ledger.entries.iter().any(|entry| {
        let discovery = entry.discovery_failure_receipt_sha256;
        !receipt_map.contains(&discovery)
            || (!prior_diffs.contains(&entry.diff_sha256)
                && Some(discovery) != authoritative_failure
                && !interactive_failures.contains(&discovery))
            || entry.ablation_failure_receipt_sha256 != Sha256Digest::ZERO
    }) {
        return Err(phase0_error("worker ledger has unavailable discovery or forged ablation evidence"));
    }
    Ok(())
}

fn validate_completed_ledger(
    draft: &Phase0AdaptationLedger,
    completed: &Phase0AdaptationLedger,
) -> Result<(), TrustError> {
    if draft.generation != completed.generation
        || draft.ledger_entries_root != completed.ledger_entries_root
        || draft.unadapted_tree_sha256 != completed.unadapted_tree_sha256
        || draft.adapted_tree_sha256 != completed.adapted_tree_sha256
        || draft.goal_sha256 != completed.goal_sha256
        || draft.entries.len() != completed.entries.len()
    {
        return Err(phase0_error("completed ledger changes the checked edit generation"));
    }
    for (before, after) in draft.entries.iter().zip(&completed.entries) {
        let mut expected = before.clone();
        expected.ablation_failure_receipt_sha256 = after.ablation_failure_receipt_sha256;
        expected.status = Phase0LedgerStatus::Audited;
        if &expected != after || after.ablation_failure_receipt_sha256 == Sha256Digest::ZERO {
            return Err(phase0_error("completed ledger may add only ablation evidence and audited status"));
        }
    }
    Ok(())
}

fn validate_all_receipts<'a>(
    state: &'a Phase0State,
) -> Result<BTreeMap<Sha256Digest, &'a Value>, TrustError> {
    let mut output = BTreeMap::new();
    for evidence in &state.checker_receipts {
        let digest = validate_phase0_execution_receipt(
            &evidence.receipt,
            state.genesis.genesis_sha256,
            "source_adaptation_checker",
            state.genesis.components.checker_runner.sha256,
        )?;
        if digest != evidence.receipt_sha256 || output.insert(digest, &evidence.receipt).is_some() {
            return Err(phase0_error("checker receipt evidence is duplicated or mis-digested"));
        }
        validate_checker_output(&evidence.receipt, state)?;
    }
    Ok(output)
}

fn validate_receipt_for_state(
    state: &Phase0State,
    receipt: &Value,
    expect_success: bool,
    expected_kind: Option<&str>,
) -> Result<Sha256Digest, TrustError> {
    let digest = validate_phase0_execution_receipt(
        receipt,
        state.genesis.genesis_sha256,
        "source_adaptation_checker",
        state.genesis.components.checker_runner.sha256,
    )?;
    let output = validate_checker_output(receipt, state)?;
    let request = receipt
        .get("input")
        .and_then(Value::as_object)
        .ok_or_else(|| phase0_error("checker receipt input is absent"))?;
    if request.get("ledger_generation").and_then(Value::as_u64) != Some(state.generation)
        || request.get("ledger_root_sha256").and_then(Value::as_str)
            != Some(state.ledger.ledger_entries_root.to_string().as_str())
        || (expected_kind != Some("entry_ablation")
            && request.get("candidate_tree_sha256").and_then(Value::as_str)
                != Some(state.candidate_manifest.source_tree_sha256.to_string().as_str()))
    {
        return Err(phase0_error("checker receipt does not bind the current generation"));
    }
    if expected_kind.is_some_and(|kind| request.get("check_kind").and_then(Value::as_str) != Some(kind)) {
        return Err(phase0_error("checker receipt has the wrong check kind"));
    }
    let passed = output.get("status").and_then(Value::as_str) == Some("passed");
    if passed != expect_success {
        return Err(phase0_error("checker receipt has the wrong success status"));
    }
    if expect_success
        && expected_kind == Some("ratification_replay")
        && !checker_final_is_clean(output)
    {
        return Err(phase0_error("final checker receipt lacks clean A/B and filespec gates"));
    }
    Ok(digest)
}

fn validate_interactive_discovery_receipt(
    state: &Phase0State,
    receipt: &Value,
) -> Result<Sha256Digest, TrustError> {
    let digest = validate_phase0_execution_receipt(
        receipt,
        state.genesis.genesis_sha256,
        "source_adaptation_checker",
        state.genesis.components.checker_runner.sha256,
    )?;
    let output = validate_checker_output(receipt, state)?;
    let request = receipt
        .get("input")
        .and_then(Value::as_object)
        .ok_or_else(|| phase0_error("interactive checker receipt lacks input"))?;
    if request.get("check_kind").and_then(Value::as_str) != Some("candidate")
        || request.get("ledger_generation").and_then(Value::as_u64) != Some(state.generation)
        || request.get("ledger_root_sha256").and_then(Value::as_str)
            != Some(state.ledger.ledger_entries_root.to_string().as_str())
        || output.get("status").and_then(Value::as_str) != Some("failed")
    {
        return Err(phase0_error(
            "interactive discovery receipt is not a failed check for this candidate lineage",
        ));
    }
    Ok(digest)
}

fn validate_ablation_receipt_for_state(
    state: &Phase0State,
    receipt: &Value,
    entry: &crate::trust_base::Phase0AdaptationEntry,
    context: &Phase0ValidationContext,
    targets: &BTreeSet<String>,
) -> Result<(Sha256Digest, bool), TrustError> {
    let digest = validate_phase0_execution_receipt(
        receipt,
        state.genesis.genesis_sha256,
        "source_adaptation_checker",
        state.genesis.components.checker_runner.sha256,
    )?;
    let output = validate_checker_output(receipt, state)?;
    let input = receipt
        .get("input")
        .and_then(Value::as_object)
        .ok_or_else(|| phase0_error("ablation receipt input is absent"))?;
    let omitted_root = phase0_ablated_tree_digest(
        Path::new(&context.unadapted_tree_path),
        &state.ledger,
        targets,
        &entry.id,
    )?;
    if input.get("ledger_generation").and_then(Value::as_u64) != Some(state.generation)
        || input.get("ledger_root_sha256").and_then(Value::as_str)
            != Some(state.ledger.ledger_entries_root.to_string().as_str())
        || input.get("check_kind").and_then(Value::as_str) != Some("entry_ablation")
        || input.get("reverted_entry_id").and_then(Value::as_str) != Some(entry.id.as_str())
        || input.get("candidate_tree_sha256").and_then(Value::as_str)
            != Some(omitted_root.to_string().as_str())
    {
        return Err(phase0_error("ablation receipt does not bind the exact omitted-entry tree"));
    }
    if output.get("status").and_then(Value::as_str) != Some("failed") {
        return Ok((digest, false));
    }
    let discovery = state
        .checker_receipts
        .iter()
        .find(|evidence| evidence.receipt_sha256 == entry.discovery_failure_receipt_sha256)
        .ok_or_else(|| phase0_error("ablation entry has no discovery receipt"))?;
    let discovery_output = validate_checker_output(&discovery.receipt, state)?;
    Ok((
        digest,
        ablation_reproduces_quote(entry, discovery_output, output)?,
    ))
}

fn validate_checker_output<'a>(
    receipt: &'a Value,
    state: &Phase0State,
) -> Result<&'a serde_json::Map<String, Value>, TrustError> {
    if receipt.get("schema").and_then(Value::as_str)
        != Some(PHASE0_CHECKED_EXECUTION_RECEIPT_SCHEMA)
        || receipt.get("timed_out").and_then(Value::as_bool) != Some(false)
        || receipt.get("stdout_truncated").and_then(Value::as_bool) != Some(false)
        || receipt.get("stderr_truncated").and_then(Value::as_bool) != Some(false)
        || receipt.get("exit_code").and_then(Value::as_i64) != Some(0)
    {
        return Err(phase0_error("outer checker execution did not complete cleanly"));
    }
    let output = receipt
        .get("parsed_stdout")
        .and_then(Value::as_object)
        .ok_or_else(|| phase0_error("checker receipt lacks a parsed output"))?;
    let request = receipt
        .get("input")
        .and_then(Value::as_object)
        .ok_or_else(|| phase0_error("checker receipt lacks its request"))?;
    if output.get("schema").and_then(Value::as_str)
        != Some("trellis-pv-phase0-checker-output/v1")
        || output.get("phase0_genesis_sha256").and_then(Value::as_str)
            != Some(state.genesis.genesis_sha256.to_string().as_str())
        || output.get("goal_sha256").and_then(Value::as_str)
            != Some(state.genesis.goal_sha256.to_string().as_str())
        || output.get("ledger_generation").and_then(Value::as_u64)
            != request.get("ledger_generation").and_then(Value::as_u64)
        || output.get("ledger_root_sha256") != request.get("ledger_root_sha256")
        || output.get("candidate_tree_sha256") != request.get("candidate_tree_sha256")
        || output.get("check_kind") != request.get("check_kind")
        || output.get("check_id") != request.get("check_id")
        || output.get("reverted_entry_id") != request.get("reverted_entry_id")
    {
        return Err(phase0_error("checker output does not echo its exact request"));
    }
    let candidate_root = request
        .get("candidate_tree_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| phase0_error("checker request lacks candidate root"))?;
    let source_facts = output
        .get("source_facts")
        .and_then(Value::as_object)
        .ok_or_else(|| phase0_error("checker output lacks sealed source facts"))?;
    for field in [
        "candidate_tree_sha256",
        "compile_initial_tree_sha256",
        "compile_final_tree_sha256",
        "extraction_initial_tree_sha256",
        "extraction_final_tree_sha256",
    ] {
        if let Some(actual) = source_facts.get(field).and_then(Value::as_str) {
            if actual != candidate_root {
                return Err(phase0_error(format!(
                    "checker {field} differs from its sealed candidate root"
                )));
            }
        }
    }
    if request.get("phase0_genesis_sha256").and_then(Value::as_str)
        != Some(state.genesis.genesis_sha256.to_string().as_str())
        || request.get("goal_sha256").and_then(Value::as_str)
            != Some(state.genesis.goal_sha256.to_string().as_str())
        || request.get("runner_sha256").and_then(Value::as_str)
            != Some(state.genesis.components.checker_runner.sha256.to_string().as_str())
        || request.get("extractor_sha256").and_then(Value::as_str)
            != Some(state.genesis.components.extractor.sha256.to_string().as_str())
    {
        return Err(phase0_error("checker request differs from state/genesis pins"));
    }
    validate_checker_request_pins(request, &state.genesis)?;
    checker_bodiless_declarations(output)?;
    let stages = output
        .get("stages")
        .and_then(Value::as_array)
        .ok_or_else(|| phase0_error("checker output stages are absent"))?;
    if stages.is_empty() {
        return Err(phase0_error("checker output has no stages"));
    }
    for stage in stages {
        validate_checker_stage(stage)?;
    }
    let failed_stage = stages
        .iter()
        .find(|stage| stage.get("status").and_then(Value::as_str) == Some("failed"));
    match output.get("status").and_then(Value::as_str) {
        Some("passed")
            if failed_stage.is_none()
                && output.get("first_failing_stage").is_some_and(Value::is_null) => {}
        Some("failed")
            if failed_stage.and_then(|stage| stage.get("name")).and_then(Value::as_str)
                == output.get("first_failing_stage").and_then(Value::as_str) => {}
        _ => return Err(phase0_error("checker status disagrees with its ordered stage facts")),
    }
    Ok(output)
}

fn validate_checker_request_pins(
    request: &serde_json::Map<String, Value>,
    genesis: &Phase0Genesis,
) -> Result<(), TrustError> {
    let expected_strings = [
        ("runner_path", genesis.components.checker_runner.path.clone()),
        ("runner_sha256", genesis.components.checker_runner.sha256.to_string()),
        ("extractor_path", genesis.components.extractor.path.clone()),
        ("extractor_sha256", genesis.components.extractor.sha256.to_string()),
        (
            "filespec_checker_path",
            genesis.components.filespec_checker.path.clone(),
        ),
        (
            "filespec_checker_sha256",
            genesis.components.filespec_checker.sha256.to_string(),
        ),
        (
            "prescribed_region_checker_path",
            genesis.components.prescribed_region_checker.path.clone(),
        ),
        (
            "prescribed_region_checker_sha256",
            genesis.components.prescribed_region_checker.sha256.to_string(),
        ),
        ("repository_root", genesis.components.repository_root.clone()),
        ("cargo_path", genesis.toolchain.cargo.path.clone()),
        ("cargo_sha256", genesis.toolchain.cargo.sha256.to_string()),
        ("rustc_path", genesis.toolchain.rustc.path.clone()),
        ("rustc_sha256", genesis.toolchain.rustc.sha256.to_string()),
        ("toolchain_root", genesis.toolchain.toolchain_root.clone()),
        (
            "charon_toolchain_root",
            genesis.toolchain.charon_toolchain.path.clone(),
        ),
        (
            "charon_toolchain_tree_sha256",
            genesis.toolchain.charon_toolchain.source_tree_sha256.to_string(),
        ),
        ("charon_path", genesis.toolchain.charon.executable.path.clone()),
        ("charon_sha256", genesis.toolchain.charon.executable.sha256.to_string()),
        ("charon_source_root", genesis.toolchain.charon.source.path.clone()),
        (
            "charon_source_tree_sha256",
            genesis.toolchain.charon.source.source_tree_sha256.to_string(),
        ),
        ("aeneas_path", genesis.toolchain.aeneas.executable.path.clone()),
        ("aeneas_sha256", genesis.toolchain.aeneas.executable.sha256.to_string()),
        ("aeneas_source_root", genesis.toolchain.aeneas.source.path.clone()),
        (
            "aeneas_source_tree_sha256",
            genesis.toolchain.aeneas.source.source_tree_sha256.to_string(),
        ),
        (
            "opam_switch_prefix",
            genesis.toolchain.opam_switch_prefix.path.clone(),
        ),
        (
            "opam_switch_prefix_tree_sha256",
            genesis
                .toolchain
                .opam_switch_prefix
                .source_tree_sha256
                .to_string(),
        ),
        ("target_triple", genesis.toolchain.target_triple.clone()),
        ("extraction_profile", genesis.toolchain.extraction_profile.clone()),
        ("opam_switch", genesis.toolchain.opam_switch.clone()),
    ];
    for (field, expected) in expected_strings {
        if request.get(field).and_then(Value::as_str) != Some(expected.as_str()) {
            return Err(phase0_error(format!("checker request has stale {field}")));
        }
    }
    let expected_dependency_path = genesis
        .toolchain
        .dependency_cache
        .as_ref()
        .map(|tree| tree.path.as_str())
        .unwrap_or("");
    let expected_dependency_root = genesis
        .toolchain
        .dependency_cache
        .as_ref()
        .map(|tree| tree.source_tree_sha256.to_string())
        .unwrap_or_else(|| raw_sha256(b"").to_string());
    if request.get("dependency_cache_path").and_then(Value::as_str)
        != Some(expected_dependency_path)
        || request.get("dependency_cache_tree_sha256").and_then(Value::as_str)
            != Some(expected_dependency_root.as_str())
        || request.get("closed_environment")
            != Some(&serde_json::to_value(&genesis.toolchain.closed_environment).map_err(phase0_serde)?)
        || request.get("opam_switch_environment")
            != Some(
                &serde_json::to_value(&genesis.toolchain.opam_switch_environment)
                    .map_err(phase0_serde)?,
            )
    {
        return Err(phase0_error("checker request has stale cache or environment pins"));
    }
    Ok(())
}

fn validate_checker_stage(stage: &Value) -> Result<(), TrustError> {
    let object = stage
        .as_object()
        .ok_or_else(|| phase0_error("checker stage is not an object"))?;
    validate_text(
        object.get("name").and_then(Value::as_str).unwrap_or(""),
        "checker stage name",
    )?;
    validate_text(
        object.get("tool").and_then(Value::as_str).unwrap_or(""),
        "checker stage tool",
    )?;
    if !matches!(object.get("status").and_then(Value::as_str), Some("passed" | "failed"))
        || object.get("timed_out").and_then(Value::as_bool).is_none()
    {
        return Err(phase0_error("checker stage has invalid status facts"));
    }
    for stream in ["stdout", "stderr"] {
        let bytes = decode_canonical_base64(
            object
                .get(&format!("{stream}_base64"))
                .and_then(Value::as_str)
                .ok_or_else(|| phase0_error("checker stage stream is absent"))?,
            "checker stage stream",
        )?;
        if object
            .get(&format!("{stream}_byte_length"))
            .and_then(Value::as_u64)
            != Some(bytes.len() as u64)
            || object
                .get(&format!("{stream}_sha256"))
                .and_then(Value::as_str)
                != Some(raw_sha256(&bytes).to_string().as_str())
            || object
                .get(&format!("{stream}_truncated"))
                .and_then(Value::as_bool)
                .is_none()
        {
            return Err(phase0_error("checker stage stream digest/length is invalid"));
        }
    }
    Ok(())
}

fn checker_final_is_clean(output: &serde_json::Map<String, Value>) -> bool {
    let generated = output
        .get("a_b_generated_lean_roots")
        .and_then(Value::as_array);
    let tablet = output.get("a_b_tablet_roots").and_then(Value::as_array);
    let pair_clean = |values: Option<&Vec<Value>>| {
        values.is_some_and(|rows| {
            rows.len() == 2
                && rows[0] == rows[1]
                && rows[0]
                    .as_str()
                    .is_some_and(|value| value != Sha256Digest::ZERO.to_string())
        })
    };
    let stages_clean = output
        .get("stages")
        .and_then(Value::as_array)
        .is_some_and(|stages| {
            ["second_clean_extraction", "filespec_region_gates"]
                .iter()
                .all(|name| {
                    stages.iter().any(|stage| {
                        stage.get("name").and_then(Value::as_str) == Some(name)
                            && stage.get("status").and_then(Value::as_str) == Some("passed")
                    })
                })
        });
    pair_clean(generated) && pair_clean(tablet) && stages_clean
}

fn validate_ledger_receipt_links(
    state: &Phase0State,
    receipts: &BTreeMap<Sha256Digest, &Value>,
) -> Result<(), TrustError> {
    validate_ledger_receipt_links_for(state, &state.ledger, receipts)
}

fn validate_ledger_receipt_links_for(
    state: &Phase0State,
    ledger: &Phase0AdaptationLedger,
    receipts: &BTreeMap<Sha256Digest, &Value>,
) -> Result<(), TrustError> {
    for entry in &ledger.entries {
        let discovery = receipts
            .get(&entry.discovery_failure_receipt_sha256)
            .ok_or_else(|| phase0_error("ledger discovery receipt is unavailable"))?;
        let output = validate_checker_output(discovery, state)?;
        if output.get("status").and_then(Value::as_str) != Some("failed") {
            return Err(phase0_error("ledger discovery receipt is not a failure"));
        }
        validate_exact_quote(entry, output)?;
        if entry.status != Phase0LedgerStatus::Draft {
            let ablation = receipts
                .get(&entry.ablation_failure_receipt_sha256)
                .ok_or_else(|| phase0_error("ledger ablation receipt is unavailable"))?;
            let ablation_output = validate_checker_output(ablation, state)?;
            let input = ablation
                .get("input")
                .and_then(Value::as_object)
                .ok_or_else(|| phase0_error("ablation input is unavailable"))?;
            if input.get("check_kind").and_then(Value::as_str) != Some("entry_ablation")
                || input.get("reverted_entry_id").and_then(Value::as_str) != Some(&entry.id)
                || ablation_output.get("status").and_then(Value::as_str) != Some("failed")
                || !ablation_reproduces_quote(entry, output, ablation_output)?
            {
                return Err(phase0_error("entry ablation does not reproduce its bound failure"));
            }
        }
    }
    Ok(())
}

fn validate_ablation_roots(
    state: &Phase0State,
    context: &Phase0ValidationContext,
    targets: &BTreeSet<String>,
    receipts: &BTreeMap<Sha256Digest, &Value>,
) -> Result<(), TrustError> {
    validate_ledger_ablation_roots(
        &state.ledger,
        Path::new(&context.unadapted_tree_path),
        targets,
        receipts,
    )
}

fn validate_ledger_ablation_roots(
    ledger: &Phase0AdaptationLedger,
    unadapted_tree_path: &Path,
    targets: &BTreeSet<String>,
    receipts: &BTreeMap<Sha256Digest, &Value>,
) -> Result<(), TrustError> {
    for entry in &ledger.entries {
        if entry.status == Phase0LedgerStatus::Draft {
            continue;
        }
        let receipt = receipts[&entry.ablation_failure_receipt_sha256];
        let input = receipt.get("input").and_then(Value::as_object).unwrap();
        let expected = phase0_ablated_tree_digest(
            unadapted_tree_path,
            ledger,
            targets,
            &entry.id,
        )?;
        if input.get("candidate_tree_sha256").and_then(Value::as_str)
            != Some(expected.to_string().as_str())
            || input.get("ledger_generation").and_then(Value::as_u64) != Some(ledger.generation)
        {
            return Err(phase0_error("ablation receipt uses the wrong omitted-entry tree"));
        }
    }
    Ok(())
}

fn validate_exact_quote(
    entry: &crate::trust_base::Phase0AdaptationEntry,
    output: &serde_json::Map<String, Value>,
) -> Result<(), TrustError> {
    let stages = output.get("stages").and_then(Value::as_array).unwrap();
    let stage = stages
        .get(entry.quoted_error.stage_index as usize)
        .ok_or_else(|| phase0_error("quoted error stage index is outside its receipt"))?;
    if stage.get("tool").and_then(Value::as_str) != Some(&entry.quoted_error.tool)
        || stage.get("name").and_then(Value::as_str) != checker_failure_stage_name(output)
        || stage.get("status").and_then(Value::as_str) != Some("failed")
    {
        return Err(phase0_error("quoted error tool differs from its receipt stage"));
    }
    let stream = match entry.quoted_error.stream {
        crate::trust_base::CheckerStream::Stdout => "stdout_base64",
        crate::trust_base::CheckerStream::Stderr => "stderr_base64",
    };
    let bytes = decode_canonical_base64(
        stage.get(stream).and_then(Value::as_str).unwrap_or(""),
        "quoted checker stream",
    )?;
    let start = usize::try_from(entry.quoted_error.byte_offset)
        .map_err(|_| phase0_error("quoted error offset is too large"))?;
    let len = usize::try_from(entry.quoted_error.byte_length)
        .map_err(|_| phase0_error("quoted error length is too large"))?;
    let end = start
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| phase0_error("quoted error is outside its receipt stream"))?;
    if &bytes[start..end] != entry.quoted_error.quote.as_bytes() {
        return Err(phase0_error("quoted error bytes differ from the receipt stream"));
    }
    let line_start = bytes[..start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut line_end = bytes[end..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |index| end + index);
    if line_end > line_start && bytes[line_end - 1] == b'\r' {
        line_end -= 1;
    }
    if start != line_start
        || end != line_end
        || end - start < 8
        || bytes[start..end].iter().all(u8::is_ascii_whitespace)
    {
        return Err(phase0_error(
            "quoted error must be one complete non-whitespace diagnostic line of at least 8 bytes",
        ));
    }
    Ok(())
}

/// What identifies the quoted diagnostic beyond its own text: the source file
/// its location line names. Tool output pairs a message line with a location
/// line, so the same message can occur several times in one run for different
/// files, and a location line moves whenever another entry changes the line
/// count above it. Comparing the message together with its file, and never the
/// line and column numbers, is what one diagnostic recurring means here.
enum QuotedDiagnosticIdentity {
    /// The quote is a message line and the line below it names this file.
    Message { file: String },
    /// The quote is itself a location line naming this file, and the line above
    /// it, when the quote does not open the stream, carries this message.
    Location { file: String, message: Option<Vec<u8>> },
}

/// The file named by a location line, ignoring any line and column numbers.
/// Two shapes are recognised: `Source: 'file', lines 1:2-3:4` and
/// `--> file:1:2`. Anything else is not a location line.
fn diagnostic_location_file(line: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(line).ok()?.trim();
    if let Some(rest) = text.strip_prefix("Source:") {
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        let body = rest.strip_prefix(quote)?;
        let file = &body[..body.find(quote)?];
        if file.trim().is_empty() {
            return None;
        }
        return Some(file.to_owned());
    }
    let path = strip_line_and_column(text.strip_prefix("-->")?.trim())?;
    if path.trim().is_empty() {
        return None;
    }
    Some(path.to_owned())
}

/// Drop a trailing `:line` or `:line:column` from a location path. A path that
/// carries no such suffix is not recognised as a location.
fn strip_line_and_column(text: &str) -> Option<&str> {
    fn strip_one(text: &str) -> Option<&str> {
        let (head, digits) = text.rsplit_once(':')?;
        if head.is_empty() || digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Some(head)
    }
    let stripped = strip_one(text)?;
    Some(strip_one(stripped).unwrap_or(stripped))
}

fn diagnostic_lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect()
}

/// The identity of the quoted diagnostic as the discovery run printed it. Any
/// output shape this cannot read yields None, which compares the quote by its
/// text alone.
fn quoted_diagnostic_identity(
    entry: &crate::trust_base::Phase0AdaptationEntry,
    discovery_output: &serde_json::Map<String, Value>,
) -> Option<QuotedDiagnosticIdentity> {
    let stage = discovery_output
        .get("stages")
        .and_then(Value::as_array)?
        .get(usize::try_from(entry.quoted_error.stage_index).ok()?)?;
    let stream = match entry.quoted_error.stream {
        crate::trust_base::CheckerStream::Stdout => "stdout_base64",
        crate::trust_base::CheckerStream::Stderr => "stderr_base64",
    };
    let bytes = decode_canonical_base64(
        stage.get(stream).and_then(Value::as_str).unwrap_or(""),
        "quoted checker stream",
    )
    .ok()?;
    let offset = usize::try_from(entry.quoted_error.byte_offset).ok()?;
    let lines = diagnostic_lines(&bytes);
    let mut cursor = 0_usize;
    let mut quoted = None;
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if cursor == offset {
            quoted = Some(index);
            break;
        }
        cursor = cursor.checked_add(line.len())?.checked_add(1)?;
    }
    let index = quoted?;
    let line = *lines.get(index)?;
    if line != entry.quoted_error.quote.as_bytes() {
        return None;
    }
    if let Some(file) = diagnostic_location_file(line) {
        let message = index
            .checked_sub(1)
            .and_then(|above| lines.get(above))
            .map(|above| above.to_vec());
        return Some(QuotedDiagnosticIdentity::Location { file, message });
    }
    Some(QuotedDiagnosticIdentity::Message {
        file: diagnostic_location_file(lines.get(index + 1)?)?,
    })
}

/// Whether the ablated run printed the same diagnostic the entry quotes: the
/// quoted line for the same file the discovery run named, at whatever line and
/// column the ablated tree puts it. A quote the discovery output gives no
/// location for is compared by its text alone.
fn ablation_reproduces_quote(
    entry: &crate::trust_base::Phase0AdaptationEntry,
    discovery_output: &serde_json::Map<String, Value>,
    ablation_output: &serde_json::Map<String, Value>,
) -> Result<bool, TrustError> {
    let identity = quoted_diagnostic_identity(entry, discovery_output);
    let Some(stages) = ablation_output.get("stages").and_then(Value::as_array) else {
        return Ok(false);
    };
    let discovery_stage_name = checker_failure_stage_name(discovery_output).unwrap_or_default();
    for stage in stages {
        if stage.get("tool").and_then(Value::as_str) != Some(&entry.quoted_error.tool)
            || stage.get("name").and_then(Value::as_str) != Some(discovery_stage_name)
            || stage.get("status").and_then(Value::as_str) != Some("failed")
        {
            continue;
        }
        let stream = match entry.quoted_error.stream {
            crate::trust_base::CheckerStream::Stdout => "stdout_base64",
            crate::trust_base::CheckerStream::Stderr => "stderr_base64",
        };
        let bytes = decode_canonical_base64(
            stage.get(stream).and_then(Value::as_str).unwrap_or(""),
            "ablation checker stream",
        )?;
        let lines = diagnostic_lines(&bytes);
        for (index, line) in lines.iter().enumerate() {
            let recurs = match &identity {
                None => *line == entry.quoted_error.quote.as_bytes(),
                Some(QuotedDiagnosticIdentity::Message { file }) => {
                    *line == entry.quoted_error.quote.as_bytes()
                        && lines
                            .get(index + 1)
                            .and_then(|below| diagnostic_location_file(below))
                            .as_deref()
                            == Some(file.as_str())
                }
                Some(QuotedDiagnosticIdentity::Location { file, message }) => {
                    diagnostic_location_file(line).as_deref() == Some(file.as_str())
                        && index
                            .checked_sub(1)
                            .and_then(|above| lines.get(above))
                            .copied()
                            == message.as_deref()
                }
            };
            if recurs {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn checker_failure_stage_name(output: &serde_json::Map<String, Value>) -> Option<&str> {
    output.get("first_failing_stage").and_then(Value::as_str)
}

fn checker_request_field<'a>(receipt: &'a Value, field: &str) -> Result<&'a Value, TrustError> {
    receipt
        .get("input")
        .and_then(|input| input.get(field))
        .ok_or_else(|| phase0_error(format!("checker receipt input lacks {field}")))
}

/// The bodiless-declaration scan a checker output carries.
fn checker_bodiless_declarations(
    output: &serde_json::Map<String, Value>,
) -> Result<Phase0ModelBodilessDeclarations, TrustError> {
    let value = output
        .get("model_bodiless_declarations")
        .ok_or_else(|| phase0_error("checker output lacks its bodiless-declaration scan"))?;
    let scan: Phase0ModelBodilessDeclarations = serde_json::from_value(value.clone())
        .map_err(|error| phase0_error(format!("checker bodiless scan is malformed: {error}")))?;
    scan.validate()?;
    Ok(scan)
}

fn receipt_bodiless_declarations(
    receipt: &Value,
) -> Result<Phase0ModelBodilessDeclarations, TrustError> {
    checker_bodiless_declarations(
        receipt
            .get("parsed_stdout")
            .and_then(Value::as_object)
            .ok_or_else(|| phase0_error("checker receipt lacks a parsed output"))?,
    )
}

fn checker_extracted_model_path(receipt: &Value) -> Result<PathBuf, TrustError> {
    let output_root = checker_request_field(receipt, "output_root")?
        .as_str()
        .ok_or_else(|| phase0_error("checker output_root is not a string"))?;
    let run_id = checker_request_field(receipt, "run_id")?
        .as_str()
        .ok_or_else(|| phase0_error("checker run_id is not a string"))?;
    validate_absolute_path(output_root, "checker output root")?;
    validate_identifier(run_id, "checker run id")?;
    Ok(Path::new(output_root)
        .join(run_id)
        .join("extraction-writable")
        .join("extracted"))
}

fn insert_receipt(
    receipts: &mut Vec<Phase0ReceiptEvidence>,
    digest: Sha256Digest,
    receipt: Value,
) -> Result<(), TrustError> {
    if receipts.iter().any(|existing| existing.receipt_sha256 == digest) {
        return Err(phase0_error("checker receipt was already recorded"));
    }
    receipts.push(Phase0ReceiptEvidence {
        receipt_sha256: digest,
        receipt,
    });
    receipts.sort_by_key(|item| item.receipt_sha256);
    Ok(())
}

fn validate_audit_history(
    state: &Phase0State,
    context: &Phase0ValidationContext,
) -> Result<(), TrustError> {
    if state.audit_requests.len() != state.audit_results.len() {
        return Err(phase0_error("audit request/result history is incomplete"));
    }
    let entry_ids = state.entry_ids();
    let mut expected_paths = context.audit_paths();
    expected_paths.extracted_model = state.extracted_model_path.clone().unwrap_or_default();
    let mut prior: Option<(u64, Phase0AuditScenario, &str)> = None;
    for (request, result) in state.audit_requests.iter().zip(&state.audit_results) {
        request.validate()?;
        result.validate_for(request, &entry_ids)?;
        if request.generation != state.generation
            || request.genesis_sha256 != state.genesis.genesis_sha256
            || request.read_only_paths != expected_paths
            || request.semantic_bundle_sha256
                != state
                    .semantic_bundle
                    .as_ref()
                    .map(|bundle| bundle.semantic_bundle_sha256)
                    .unwrap_or(Sha256Digest::ZERO)
        {
            return Err(phase0_error("audit history contains a stale request"));
        }
        let key = (request.generation, request.scenario, request.lane.lane_id.as_str());
        if prior.is_some_and(|item| item >= key) {
            return Err(phase0_error("audit history is not canonical"));
        }
        prior = Some(key);
    }
    let targets = context.goal_target_set()?;
    let receipt_map = state
        .checker_receipts
        .iter()
        .map(|evidence| (evidence.receipt_sha256, &evidence.receipt))
        .collect();
    validate_ablation_roots(state, context, &targets, &receipt_map)
}

fn sort_audit_history(
    requests: &mut Vec<Phase0AuditRequest>,
    results: &mut Vec<Phase0AuditResult>,
) {
    let mut pairs: Vec<_> = requests.drain(..).zip(results.drain(..)).collect();
    pairs.sort_by(|(left, _), (right, _)| {
        (left.generation, left.scenario, left.lane.lane_id.as_bytes()).cmp(&(
            right.generation,
            right.scenario,
            right.lane.lane_id.as_bytes(),
        ))
    });
    for (request, result) in pairs {
        requests.push(request);
        results.push(result);
    }
}

fn validate_complete_audit_set(
    results: &[Phase0AuditResult],
    bindings: &[Phase0LaneBinding],
    scenario: Phase0AuditScenario,
    generation: u64,
) -> Result<(), TrustError> {
    if results.len() != bindings.len()
        || results.iter().any(|result| {
            result.scenario != scenario
                || result.generation != generation
                || result.decision != Phase0AuditDecision::Pass
        })
    {
        return Err(phase0_error("sealed generation lacks a clean complete audit scenario"));
    }
    let result_bindings: BTreeSet<_> = results
        .iter()
        .map(|result| result.lane_binding_sha256)
        .collect();
    let expected: BTreeSet<_> = bindings.iter().map(|binding| binding.binding_sha256).collect();
    if result_bindings != expected {
        return Err(phase0_error("sealed audit results have the wrong lane bindings"));
    }
    Ok(())
}

fn validate_sealed_audit_pairs(
    requests: &[Phase0AuditRequest],
    results: &[Phase0AuditResult],
    bundle: &Phase0SemanticBundle,
    scenario: Phase0AuditScenario,
) -> Result<(), TrustError> {
    if requests.len() != results.len() {
        return Err(phase0_error("sealed audit request/result cardinality differs"));
    }
    let mut entry_ids: Vec<_> = bundle
        .ledger
        .entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    entry_ids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let binding_targets: Vec<_> = bundle
        .goal_binding_report
        .targets
        .iter()
        .map(|target| Phase0GoalBindingAuditTarget {
            target_id: target.target_id.clone(),
            origin: target.origin,
            goal_binding_outside_target: target.goal_binding_outside_target,
            goal_binding_in_adaptation: target.goal_binding_in_adaptation,
        })
        .collect();
    for (request, result) in requests.iter().zip(results) {
        request.validate()?;
        result.validate_for(request, &entry_ids)?;
        if request.scenario != scenario
            || request.generation != bundle.generation
            || request.genesis_sha256 != bundle.genesis_sha256
            || request.goal_sha256 != bundle.goal_sha256
            || request.unadapted_manifest_sha256
                != bundle.unadapted_source_manifest.source_tree_sha256
            || request.adapted_manifest_sha256
                != bundle.adapted_source_manifest.source_tree_sha256
            || request.ledger_entries_root != bundle.ledger.ledger_entries_root
            || request.ledger_sha256 != bundle.ledger.ledger_sha256
            || request.semantic_bundle_sha256 != bundle.semantic_bundle_sha256
            || request.final_checker_success_receipt_sha256
                != bundle.final_checker_success_receipt_sha256
            || request.entry_receipts != bundle.entry_receipts
            || request.unified_diffs != render_unified_diffs(&bundle.ledger)
            || request.goal_binding_report_sha256 != bundle.goal_binding_report.report_sha256
            || request.goal_binding_targets != binding_targets
            || result.decision != Phase0AuditDecision::Pass
        {
            return Err(phase0_error("sealed audit pair is stale, dirty, or mis-bound"));
        }
    }
    Ok(())
}

fn validate_sealed_receipt_closure(
    bundle: &Phase0SemanticBundle,
    genesis: &Phase0Genesis,
) -> Result<(), TrustError> {
    let mut receipts = BTreeMap::new();
    for evidence in &bundle.checker_receipts {
        let digest = validate_phase0_execution_receipt(
            &evidence.receipt,
            genesis.genesis_sha256,
            "source_adaptation_checker",
            genesis.components.checker_runner.sha256,
        )?;
        if digest != evidence.receipt_sha256
            || receipts.insert(digest, &evidence.receipt).is_some()
        {
            return Err(phase0_error("sealed checker receipt is duplicated or mis-digested"));
        }
    }
    for digest in [
        bundle.candidate_checker_success_receipt_sha256,
        bundle.final_checker_success_receipt_sha256,
    ] {
        if !receipts.contains_key(&digest) {
            return Err(phase0_error("sealed bundle omits a success receipt"));
        }
    }
    for entry in &bundle.ledger.entries {
        let binding = bundle
            .entry_receipts
            .iter()
            .find(|binding| binding.entry_id == entry.id)
            .ok_or_else(|| phase0_error("sealed bundle omits an entry receipt binding"))?;
        let discovery = receipts
            .get(&entry.discovery_failure_receipt_sha256)
            .ok_or_else(|| phase0_error("sealed bundle omits discovery evidence"))?;
        let ablation = receipts
            .get(&entry.ablation_failure_receipt_sha256)
            .ok_or_else(|| phase0_error("sealed bundle omits ablation evidence"))?;
        let discovery_output = discovery
            .get("parsed_stdout")
            .and_then(Value::as_object)
            .ok_or_else(|| phase0_error("sealed discovery receipt lacks checker output"))?;
        let ablation_output = ablation
            .get("parsed_stdout")
            .and_then(Value::as_object)
            .ok_or_else(|| phase0_error("sealed ablation receipt lacks checker output"))?;
        if discovery_output.get("status").and_then(Value::as_str) != Some("failed")
            || ablation_output.get("status").and_then(Value::as_str) != Some("failed")
            || ablation
                .get("input")
                .and_then(|value| value.get("reverted_entry_id"))
                .and_then(Value::as_str)
                != Some(entry.id.as_str())
            || ablation
                .get("input")
                .and_then(|value| value.get("candidate_tree_sha256"))
                .and_then(Value::as_str)
                != Some(binding.ablation_omitted_tree_sha256.to_string().as_str())
        {
            return Err(phase0_error("sealed entry evidence is not a bound failure"));
        }
        validate_exact_quote(entry, discovery_output)?;
        if !ablation_reproduces_quote(entry, discovery_output, ablation_output)? {
            return Err(phase0_error("sealed ablation does not reproduce its discovery diagnostic"));
        }
    }
    Ok(())
}

fn validate_bindings(
    bindings: &Phase0Bindings,
    allow_same_model_lanes: bool,
) -> Result<(), TrustError> {
    bindings.worker.validate()?;
    for lanes in [&bindings.seam_repair, &bindings.source_correspondence] {
        if lanes.is_empty() {
            return Err(phase0_error("each Phase-0 audit scenario needs a lane"));
        }
        let mut prior: Option<&str> = None;
        for lane in lanes {
            lane.validate()?;
            if !allow_same_model_lanes
                && lane.provider == bindings.worker.provider
                && lane.model == bindings.worker.model
            {
                return Err(phase0_error(
                    "Phase-0 audit lane shares the worker provider/model without phase0.allow_same_model_lanes",
                ));
            }
            if prior.is_some_and(|item| item.as_bytes() >= lane.lane_id.as_bytes()) {
                return Err(phase0_error("Phase-0 lane bindings are not strictly sorted"));
            }
            prior = Some(&lane.lane_id);
        }
    }
    Ok(())
}

fn component_files(components: &Phase0ComponentClosure) -> Vec<&Phase0PinnedFile> {
    vec![
        &components.kernel,
        &components.controller,
        &components.bridge,
        &components.checker_broker,
        &components.checker_runner,
        &components.extractor,
        &components.filespec_checker,
        &components.prescribed_region_checker,
        &components.production_extraction_runner,
        &components.goal_binding_reporter,
        &components.model_opacity_scanner,
        &components.worker_fragment,
        &components.seam_audit_fragment,
        &components.correspondence_audit_fragment,
    ]
}

fn validate_toolchain_shape(toolchain: &Phase0ToolchainClosure) -> Result<(), TrustError> {
    validate_pinned_file_shape(&toolchain.cargo)?;
    validate_pinned_file_shape(&toolchain.rustc)?;
    validate_absolute_path(&toolchain.toolchain_root, "toolchain root")?;
    validate_pinned_tree_shape(&toolchain.charon_toolchain)?;
    validate_text(&toolchain.cargo_identity, "Cargo identity")?;
    validate_text(&toolchain.rustc_identity, "Rust identity")?;
    validate_checkout_shape(&toolchain.charon)?;
    validate_checkout_shape(&toolchain.aeneas)?;
    if let Some(tree) = &toolchain.dependency_cache {
        validate_pinned_tree_shape(tree)?;
    }
    validate_pinned_tree_shape(&toolchain.opam_switch_prefix)?;
    for value in [
        &toolchain.opam_switch,
        &toolchain.target_triple,
        &toolchain.extraction_profile,
    ] {
        validate_identifier(value, "toolchain identifier")?;
    }
    let allowed = BTreeSet::from(["LANG", "LC_ALL", "RUSTUP_TOOLCHAIN"]);
    if toolchain.closed_environment.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(phase0_error("closed checker environment contains an unapproved key"));
    }
    for (key, value) in &toolchain.closed_environment {
        validate_text(key, "environment key")?;
        if value.contains('\0') {
            return Err(phase0_error("closed checker environment contains NUL"));
        }
    }
    validate_opam_switch_environment_shape(toolchain)?;
    Ok(())
}

fn validate_opam_switch_environment_shape(
    toolchain: &Phase0ToolchainClosure,
) -> Result<(), TrustError> {
    let expected = BTreeSet::from([
        "CAML_LD_LIBRARY_PATH",
        "OCAMLTOP_INCLUDE_PATH",
        "OCAML_TOPLEVEL_PATH",
        "OPAMSWITCH",
        "OPAM_SWITCH_PREFIX",
    ]);
    let actual = toolchain
        .opam_switch_environment
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(phase0_error(
            "opam switch environment has missing or unapproved variables",
        ));
    }
    let prefix = &toolchain.opam_switch_prefix.path;
    if toolchain.opam_switch_environment.get("OPAM_SWITCH_PREFIX") != Some(prefix)
        || toolchain.opam_switch_environment.get("OPAMSWITCH")
            != Some(&toolchain.opam_switch)
    {
        return Err(phase0_error(
            "opam switch environment disagrees with the selected switch prefix",
        ));
    }
    let prefix_path = Path::new(prefix);
    for key in [
        "CAML_LD_LIBRARY_PATH",
        "OCAMLTOP_INCLUDE_PATH",
        "OCAML_TOPLEVEL_PATH",
        "OPAM_SWITCH_PREFIX",
    ] {
        let value = &toolchain.opam_switch_environment[key];
        let paths = std::env::split_paths(value).collect::<Vec<_>>();
        if paths.is_empty() {
            return Err(phase0_error(format!(
                "opam switch environment variable {key} has no paths"
            )));
        }
        for path in paths {
            let raw = path
                .to_str()
                .ok_or_else(|| phase0_error("opam switch environment path is not UTF-8"))?;
            validate_absolute_path(raw, "opam switch environment path")?;
            if !path.starts_with(prefix_path) {
                return Err(phase0_error(format!(
                    "opam switch environment variable {key} escapes the switch prefix"
                )));
            }
        }
    }
    Ok(())
}

fn validate_opam_switch_environment_filesystem(
    toolchain: &Phase0ToolchainClosure,
) -> Result<(), TrustError> {
    let prefix = fs::canonicalize(&toolchain.opam_switch_prefix.path)
        .map_err(|error| phase0_error(format!("cannot resolve opam switch prefix: {error}")))?;
    for key in [
        "CAML_LD_LIBRARY_PATH",
        "OCAMLTOP_INCLUDE_PATH",
        "OCAML_TOPLEVEL_PATH",
        "OPAM_SWITCH_PREFIX",
    ] {
        for path in std::env::split_paths(&toolchain.opam_switch_environment[key]) {
            let resolved = fs::canonicalize(&path).map_err(|error| {
                phase0_error(format!(
                    "cannot resolve opam switch environment path {}: {error}",
                    path.display()
                ))
            })?;
            if !resolved.starts_with(&prefix) {
                return Err(phase0_error(format!(
                    "opam switch environment variable {key} resolves outside the switch prefix"
                )));
            }
        }
    }
    Ok(())
}

fn validate_checkout_shape(tool: &Phase0PinnedCheckoutTool) -> Result<(), TrustError> {
    validate_pinned_file_shape(&tool.executable)?;
    validate_pinned_tree_shape(&tool.source)?;
    validate_identifier(&tool.revision, "checkout revision")
}

fn validate_pinned_file_shape(file: &Phase0PinnedFile) -> Result<(), TrustError> {
    validate_absolute_path(&file.path, "pinned file")?;
    if file.sha256 == Sha256Digest::ZERO {
        return Err(phase0_error("pinned file has a zero digest"));
    }
    Ok(())
}

fn validate_pinned_tree_shape(tree: &Phase0PinnedTree) -> Result<(), TrustError> {
    validate_absolute_path(&tree.path, "pinned tree")?;
    if tree.source_tree_sha256 == Sha256Digest::ZERO {
        return Err(phase0_error("pinned tree has a zero digest"));
    }
    Ok(())
}

fn validate_pinned_file_bytes(file: &Phase0PinnedFile) -> Result<(), TrustError> {
    validate_pinned_file_shape(file)?;
    let path = Path::new(&file.path);
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| phase0_error(format!("pinned file {}: {error}", file.path)))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(phase0_error("pinned component is not a non-symlink regular file"));
    }
    if raw_sha256(&fs::read(path).map_err(|error| phase0_error(error.to_string()))?)
        != file.sha256
    {
        return Err(phase0_error("pinned component bytes drifted"));
    }
    Ok(())
}

fn validate_pinned_tree_bytes(tree: &Phase0PinnedTree) -> Result<(), TrustError> {
    validate_pinned_tree_shape(tree)?;
    if source_tree_manifest(Path::new(&tree.path))?.source_tree_sha256 != tree.source_tree_sha256 {
        return Err(phase0_error("pinned tree bytes drifted"));
    }
    Ok(())
}

fn git_output(root: &Path, arguments: &[&str], label: &str) -> Result<Vec<u8>, TrustError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .map_err(|error| phase0_error(format!("cannot run Git for {label}: {error}")))?;
    if !output.status.success() {
        return Err(phase0_error(format!(
            "cannot inspect Git checkout for {label}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// Content identity for a clean tool checkout. Unlike a PV source manifest,
/// this deliberately admits tracked Git symlinks and hashes their link bytes.
pub fn git_checkout_source_digest(
    root: &Path,
    expected_revision: &str,
) -> Result<Sha256Digest, TrustError> {
    let root = fs::canonicalize(root)
        .map_err(|error| phase0_error(format!("cannot resolve Git checkout: {error}")))?;
    let top = String::from_utf8(git_output(
        &root,
        &["rev-parse", "--show-toplevel"],
        "checkout root",
    )?)
    .map_err(|_| phase0_error("Git checkout root is not UTF-8"))?;
    if fs::canonicalize(top.trim())
        .map_err(|error| phase0_error(format!("cannot resolve Git top level: {error}")))?
        != root
    {
        return Err(phase0_error("Git checkout root is not exact"));
    }
    let revision = String::from_utf8(git_output(&root, &["rev-parse", "HEAD"], "revision")?)
        .map_err(|_| phase0_error("Git revision is not UTF-8"))?;
    let revision = revision.trim();
    if revision != expected_revision
        || revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(phase0_error("Git checkout revision drifted or is not canonical"));
    }
    if !git_output(
        &root,
        &["status", "--porcelain", "--untracked-files=all"],
        "clean worktree",
    )?
    .is_empty()
    {
        return Err(phase0_error("Git checkout worktree is not clean"));
    }
    let index = git_output(&root, &["ls-files", "-z", "--stage"], "tracked files")?;
    let mut rows = Vec::new();
    for record in index.split(|byte| *byte == 0).filter(|record| !record.is_empty()) {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| phase0_error("Git checkout index row lacks a path"))?;
        let prefix = std::str::from_utf8(&record[..tab])
            .map_err(|_| phase0_error("Git checkout index metadata is not UTF-8"))?;
        let mut fields = prefix.split(' ');
        let mode = fields.next().unwrap_or("");
        let _object_id = fields.next().unwrap_or("");
        let stage = fields.next().unwrap_or("");
        if fields.next().is_some()
            || stage != "0"
            || !matches!(mode, "100644" | "100755" | "120000")
        {
            return Err(phase0_error(
                "Git checkout has an unsupported index mode or stage",
            ));
        }
        let relative = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| phase0_error("Git checkout path is not UTF-8"))?;
        crate::trust_base::source_tree::validate_relative_path(relative)?;
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| phase0_error(format!("cannot inspect tracked path: {error}")))?;
        let bytes = if mode == "120000" {
            if !metadata.file_type().is_symlink() {
                return Err(phase0_error("tracked symlink changed filesystem type"));
            }
            fs::read_link(&path)
                .map_err(|error| phase0_error(format!("cannot read tracked symlink: {error}")))?
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(phase0_error("tracked regular file changed filesystem type"));
            }
            fs::read(&path)
                .map_err(|error| phase0_error(format!("cannot read tracked file: {error}")))?
        };
        rows.push(serde_json::json!({
            "byte_length": bytes.len(),
            "git_mode": mode,
            "relative_path": relative,
            "sha256_of_raw_bytes": raw_sha256(&bytes),
        }));
    }
    rows.sort_by(|left, right| {
        left["relative_path"]
            .as_str()
            .unwrap_or("")
            .as_bytes()
            .cmp(right["relative_path"].as_str().unwrap_or("").as_bytes())
    });
    if rows.is_empty() {
        return Err(phase0_error("Git checkout contains no tracked files"));
    }
    let payload = canonical_json_value(&serde_json::json!({
        "files": rows,
        "revision": revision,
    }))?;
    Ok(tagged_hash(DomainTag::GitCheckoutRoot, &payload))
}

fn validate_git_checkout_bytes(checkout: &Phase0PinnedCheckoutTool) -> Result<(), TrustError> {
    // Unit fixtures created before the production launcher use a descriptive
    // non-SHA revision and the regular-file tree contract. Production
    // Genesis records are always emitted from a clean, full-revision Git root.
    if checkout.revision.len() == 40
        && checkout
            .revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        if git_checkout_source_digest(Path::new(&checkout.source.path), &checkout.revision)?
            != checkout.source.source_tree_sha256
        {
            return Err(phase0_error("pinned Git checkout bytes drifted"));
        }
        Ok(())
    } else {
        validate_pinned_tree_bytes(&checkout.source)
    }
}

/// Content identity of an exposed read-only dependency/cache directory.
/// Tracked tool checkouts use the stronger revision-aware function above;
/// this closure admits and hashes ordinary filesystem symlinks without ever
/// following them.
pub fn readonly_closure_digest(root: &Path) -> Result<Sha256Digest, TrustError> {
    fn walk(root: &Path, current: &Path, rows: &mut Vec<Value>) -> Result<(), TrustError> {
        let mut entries = fs::read_dir(current)
            .map_err(|error| phase0_error(format!("cannot enumerate read-only closure: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| phase0_error(format!("cannot enumerate read-only closure: {error}")))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                phase0_error(format!("cannot inspect read-only closure member: {error}"))
            })?;
            if metadata.is_dir() {
                walk(root, &path, rows)?;
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| phase0_error("read-only closure member escaped its root"))?
                .components()
                .map(|component| {
                    component
                        .as_os_str()
                        .to_str()
                        .ok_or_else(|| phase0_error("read-only closure path is not UTF-8"))
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("/");
            crate::trust_base::source_tree::validate_relative_path(&relative)?;
            let (entry_type, bytes) = if metadata.file_type().is_symlink() {
                (
                    "symlink",
                    fs::read_link(&path)
                        .map_err(|error| {
                            phase0_error(format!("cannot read closure symlink: {error}"))
                        })?
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                )
            } else if metadata.is_file() {
                (
                    "file",
                    fs::read(&path).map_err(|error| {
                        phase0_error(format!("cannot read closure file: {error}"))
                    })?,
                )
            } else {
                return Err(phase0_error(
                    "read-only closure contains a special filesystem member",
                ));
            };
            rows.push(serde_json::json!({
                "byte_length": bytes.len(),
                "entry_type": entry_type,
                "relative_path": relative,
                "sha256_of_raw_bytes": raw_sha256(&bytes),
            }));
        }
        Ok(())
    }

    let metadata = fs::symlink_metadata(root)
        .map_err(|error| phase0_error(format!("cannot inspect read-only closure: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(phase0_error(
            "read-only closure root is not a non-symlink directory",
        ));
    }
    let mut rows = Vec::new();
    walk(root, root, &mut rows)?;
    rows.sort_by(|left, right| {
        left["relative_path"]
            .as_str()
            .unwrap_or("")
            .as_bytes()
            .cmp(right["relative_path"].as_str().unwrap_or("").as_bytes())
    });
    if rows.is_empty() {
        return Err(phase0_error("read-only closure contains no files or links"));
    }
    Ok(tagged_hash(
        DomainTag::ReadonlyClosureRoot,
        &canonical_json_value(&Value::Array(rows))?,
    ))
}

fn validate_readonly_closure_bytes(tree: &Phase0PinnedTree) -> Result<(), TrustError> {
    validate_pinned_tree_shape(tree)?;
    if readonly_closure_digest(Path::new(&tree.path))? != tree.source_tree_sha256 {
        return Err(phase0_error("pinned read-only closure bytes drifted"));
    }
    Ok(())
}

fn validate_source_manifest(manifest: &SourceTreeManifest) -> Result<(), TrustError> {
    if manifest.schema != SOURCE_TREE_MANIFEST_SCHEMA
        || source_tree_root_from_entries(&manifest.files)? != manifest.source_tree_sha256
    {
        return Err(phase0_error("source-tree manifest is invalid"));
    }
    Ok(())
}

fn validate_entry_receipt_bindings(
    bindings: &[Phase0EntryReceiptBinding],
) -> Result<(), TrustError> {
    let mut previous: Option<&str> = None;
    for binding in bindings {
        validate_identifier(&binding.entry_id, "entry receipt id")?;
        if binding.discovery_failure_receipt_sha256 == Sha256Digest::ZERO
            || binding.ablation_failure_receipt_sha256 == Sha256Digest::ZERO
            || binding.ablation_omitted_tree_sha256 == Sha256Digest::ZERO
            || previous.is_some_and(|item| item.as_bytes() >= binding.entry_id.as_bytes())
        {
            return Err(phase0_error("entry receipt bindings are incomplete or unsorted"));
        }
        previous = Some(&binding.entry_id);
    }
    Ok(())
}

fn validate_receipt_evidence_order(receipts: &[Phase0ReceiptEvidence]) -> Result<(), TrustError> {
    let mut previous: Option<Sha256Digest> = None;
    for receipt in receipts {
        if receipt.receipt_sha256 == Sha256Digest::ZERO
            || previous.is_some_and(|digest| digest >= receipt.receipt_sha256)
        {
            return Err(phase0_error("checker receipts are not strictly digest-sorted"));
        }
        previous = Some(receipt.receipt_sha256);
    }
    Ok(())
}

fn validate_findings(findings: &[Phase0AuditFinding]) -> Result<(), TrustError> {
    let mut previous: Option<&Phase0AuditFinding> = None;
    for finding in findings {
        validate_identifier(&finding.code, "finding code")?;
        validate_text(&finding.judgment, "finding judgment")?;
        validate_text(&finding.required_revision, "required revision")?;
        if finding.span_start.is_some() != finding.span_end.is_some()
            || matches!((finding.span_start, finding.span_end), (Some(start), Some(end)) if start > end)
            || previous.is_some_and(|item| item >= finding)
        {
            return Err(phase0_error("audit findings are not canonical"));
        }
        previous = Some(finding);
    }
    Ok(())
}

fn validate_event_history(history: &[Phase0StateEventRecord]) -> Result<(), TrustError> {
    for (index, event) in history.iter().enumerate() {
        if event.sequence != index as u64 + 1 || event.evidence_sha256 == Sha256Digest::ZERO {
            return Err(phase0_error("state event history is not contiguous and bound"));
        }
        validate_identifier(&event.kind, "state event kind")?;
    }
    Ok(())
}

fn validate_generation_history(
    state: &Phase0State,
    context: &Phase0ValidationContext,
    targets: &BTreeSet<String>,
    receipts: &BTreeMap<Sha256Digest, &Value>,
) -> Result<(), TrustError> {
    let mut prior: Option<u64> = None;
    for retired in &state.generation_history {
        if retired.generation >= state.generation
            || prior.is_some_and(|generation| generation >= retired.generation)
            || retired.ledger.generation != retired.generation
            || retired.ledger.adapted_tree_sha256
                != retired.candidate_manifest.source_tree_sha256
        {
            return Err(phase0_error("retired generation history is stale or unordered"));
        }
        retired.ledger.validate(targets)?;
        validate_source_manifest(&retired.candidate_manifest)?;
        validate_ledger_receipt_links_for(state, &retired.ledger, receipts)?;
        validate_ledger_ablation_roots(
            &retired.ledger,
            Path::new(&context.unadapted_tree_path),
            targets,
            receipts,
        )?;
        if let Some(bundle) = &retired.semantic_bundle {
            bundle.validate()?;
            if bundle.generation != retired.generation
                || bundle.ledger != retired.ledger
                || bundle.adapted_source_manifest != retired.candidate_manifest
            {
                return Err(phase0_error("retired semantic bundle is mis-bound"));
            }
            validate_sealed_receipt_closure(bundle, &state.genesis)?;
        }
        if retired.audit_requests.len() != retired.audit_results.len() {
            return Err(phase0_error("retired audit history is incomplete"));
        }
        let mut entry_ids: Vec<_> = retired
            .ledger
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        entry_ids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        for (request, result) in retired.audit_requests.iter().zip(&retired.audit_results) {
            request.validate()?;
            result.validate_for(request, &entry_ids)?;
            let mut retired_paths = context.audit_paths();
            retired_paths.extracted_model = request.read_only_paths.extracted_model.clone();
            if request.generation != retired.generation
                || request.genesis_sha256 != state.genesis.genesis_sha256
                || request.read_only_paths != retired_paths
                || retired.semantic_bundle.as_ref().is_none_or(|bundle| {
                    request.semantic_bundle_sha256 != bundle.semantic_bundle_sha256
                        || request.ledger_sha256 != bundle.ledger.ledger_sha256
                        || request.ledger_entries_root != bundle.ledger.ledger_entries_root
                        || request.entry_receipts != bundle.entry_receipts
                        || request.unified_diffs != render_unified_diffs(&bundle.ledger)
                        || request.final_checker_success_receipt_sha256
                            != bundle.final_checker_success_receipt_sha256
                        || request.goal_binding_report_sha256
                            != bundle.goal_binding_report.report_sha256
                        || request.goal_binding_targets
                            != bundle.goal_binding_report.targets.iter().map(|target| {
                                Phase0GoalBindingAuditTarget {
                                    target_id: target.target_id.clone(),
                                    origin: target.origin,
                                    goal_binding_outside_target: target.goal_binding_outside_target,
                                    goal_binding_in_adaptation: target.goal_binding_in_adaptation,
                                }
                            }).collect::<Vec<_>>()
                })
            {
                return Err(phase0_error("retired audit has the wrong generation"));
            }
        }
        validate_findings(&retired.terminal_findings)?;
        if !retired.terminal_findings.is_empty()
            && !retired
                .audit_results
                .iter()
                .any(|result| result.findings == retired.terminal_findings)
        {
            return Err(phase0_error("retired findings lack their audit result"));
        }
        let value = serde_json::to_value(retired).map_err(phase0_serde)?;
        if self_digest(DomainTag::RawArtifact, &value, "record_sha256")?
            != retired.record_sha256
        {
            return Err(phase0_error("retired generation self digest is invalid"));
        }
        prior = Some(retired.generation);
    }
    Ok(())
}

fn goal_targets_from_report(report: &Phase0GoalBindingReport) -> BTreeSet<String> {
    report.targets.iter().map(|target| target.target_id.clone()).collect()
}

fn parse_canonical<T: DeserializeOwned>(bytes: &[u8], label: &str) -> Result<T, TrustError> {
    let value = parse_json_strict(bytes)?;
    if canonical_json_value(&value)? != bytes {
        return Err(phase0_error(format!("{label} must be exact canonical JSON")));
    }
    serde_json::from_value(value).map_err(|error| phase0_error(format!("{label}: {error}")))
}

fn decode_canonical_base64(value: &str, label: &str) -> Result<Vec<u8>, TrustError> {
    let bytes = BASE64_STANDARD
        .decode(value)
        .map_err(|error| phase0_error(format!("{label} is invalid base64: {error}")))?;
    if BASE64_STANDARD.encode(&bytes) != value {
        return Err(phase0_error(format!("{label} is not canonical base64")));
    }
    Ok(bytes)
}

fn validate_absolute_path(value: &str, label: &str) -> Result<(), TrustError> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || value.contains('\0')
        || value.chars().any(char::is_control)
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(phase0_error(format!("{label} is not a normalized absolute path")));
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), TrustError> {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(|character| character.is_control())
    {
        return Err(phase0_error(format!("{label} is invalid")));
    }
    Ok(())
}

fn validate_text(value: &str, label: &str) -> Result<(), TrustError> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(phase0_error(format!("{label} is invalid")));
    }
    Ok(())
}

fn phase0_serde(error: serde_json::Error) -> TrustError {
    phase0_error(error.to_string())
}

fn phase0_error(detail: impl Into<String>) -> TrustError {
    TrustError::new("phase0_protocol_invalid", detail)
}

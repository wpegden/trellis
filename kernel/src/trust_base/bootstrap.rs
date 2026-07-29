//! Production construction of an acyclic trust-v1 campaign seed.
//!
//! Bootstrap is deliberately a constructor, not a second verifier.  Every
//! value emitted here is immediately fed through the same schema, semantic,
//! definition-closure, and evidence-closure validators used by the runtime.

use super::auth::ActorKeyManifest;
use super::basis::validate_independent_basis;
use super::campaign_plan::{
    BasisFactClass, CampaignTrustPlan, ResourceQualificationPlan, TargetSourceValidationPlan,
};
use super::canonical::{
    canonical_json_value, raw_sha256, self_digest, tagged_hash, DomainTag, Sha256Digest,
    TrustError,
};
use super::closure::{
    verify_evidence_tool_manifest, verify_seed_definition_bundle, VerifiedEvidenceClosure,
};
use super::journal::canonical_genesis;
use super::records::{AuthoritativeRecord, JournalPolicy};
use super::schema::SchemaRegistry;
use super::source_validation::SourceValidationContractView;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ActorPublicKeySpec {
    pub key_id: String,
    pub actor_role: String,
    pub actor_identity: String,
    pub purpose: String,
    pub public_key_ed25519_hex: String,
}

#[derive(Clone, Debug)]
pub struct EvidenceInput {
    pub kind: String,
    pub logical_id: String,
    pub relative_path: String,
    pub dependency_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CampaignTargetInput {
    pub target_id: String,
    pub node_id: String,
    pub lean_declaration: String,
    pub informal: String,
    /// Exact checked generated-model boundary/call-path integration closure for
    /// source validation. This does not claim a proof of the Rust→Aeneas
    /// compiler simulation bridge, which is disclosed separately as semantic
    /// trust.
    pub model_refinement_sha256: Sha256Digest,
}

#[derive(Clone, Debug)]
pub struct ConservativeSeedRequest<'a> {
    pub journal_id: &'a str,
    pub run_id: &'a str,
    pub seed_plan_id: &'a str,
    pub actor_key_manifest: &'a ActorKeyManifest,
    pub evidence_tool_input_root: Sha256Digest,
    pub source_tree_sha256: Sha256Digest,
    pub contract_generator_sha256: Sha256Digest,
    pub targets: &'a [CampaignTargetInput],
}

#[derive(Clone, Debug)]
pub struct CampaignSeedRequest<'a> {
    pub journal_id: &'a str,
    pub run_id: &'a str,
    pub seed_plan_id: &'a str,
    pub actor_key_manifest: &'a ActorKeyManifest,
    pub evidence: &'a VerifiedEvidenceClosure,
    pub extraction_result: &'a Value,
    pub extraction_determinism: &'a Value,
    /// Exact canonical description of the intentionally unarchived platform
    /// surface.  It is an approved evidence leaf and is rendered verbatim at
    /// the sole human gate, rather than hidden behind only a digest.
    pub trusted_platform_boundary: &'a Value,
    pub basis_fact_artifacts: &'a BTreeMap<String, Value>,
    pub source_tree_sha256: Sha256Digest,
    pub targets: &'a [CampaignTargetInput],
    pub trust_plan: &'a CampaignTrustPlan,
}

#[derive(Clone, Debug)]
pub struct ConstructedSeed {
    pub seed_manifest: AuthoritativeRecord,
    pub seed_manifest_bytes: Vec<u8>,
    pub seed_definition_bundle: Value,
    pub seed_definition_bundle_bytes: Vec<u8>,
    pub gate_presentation_bytes: Vec<u8>,
}

pub fn build_actor_key_manifest(
    manifest_authority_id: &str,
    manifest_authority_key: &SigningKey,
    keys: &[ActorPublicKeySpec],
) -> Result<Value, TrustError> {
    if manifest_authority_id.is_empty() || keys.len() < 3 {
        return Err(TrustError::new(
            "bootstrap_actor_manifest_input_invalid",
            "manifest authority and all three actor roles are required",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut key_values = Vec::with_capacity(keys.len());
    for key in keys {
        if !seen.insert(key.key_id.clone()) {
            return Err(TrustError::new(
                "bootstrap_actor_key_duplicate",
                format!("duplicate actor key ID {:?}", key.key_id),
            ));
        }
        key_values.push(serde_json::json!({
            "key_id": key.key_id,
            "actor_role": key.actor_role,
            "actor_identity": key.actor_identity,
            "purpose": key.purpose,
            "algorithm": "Ed25519",
            "public_key_ed25519_hex": key.public_key_ed25519_hex,
            "valid_from_sequence": 0,
        }));
    }
    key_values.sort_by(|left, right| {
        left["key_id"]
            .as_str()
            .unwrap_or("")
            .as_bytes()
            .cmp(right["key_id"].as_str().unwrap_or("").as_bytes())
    });
    let authorization_value = serde_json::json!({
        "schema": "trellis-actor-authentication-key-manifest/v1",
        "protocol_id": "trellis-trust-v1",
        "key_history_policy": "seed_pinned_immutable_all_epochs_v1",
        "manifest_authority_id": manifest_authority_id,
        "keys": key_values,
    });
    let signing_digest = tagged_hash(
        DomainTag::ActorAuthenticationKeyManifestAuthorization,
        &canonical_json_value(&authorization_value)?,
    );
    let signature = manifest_authority_key.sign(signing_digest.as_bytes());
    let mut manifest = authorization_value;
    manifest["authorization_signing_digest_sha256"] =
        Value::String(signing_digest.to_string());
    manifest["authorization_signature_ed25519_hex"] =
        Value::String(hex(signature.to_bytes().as_ref()));
    manifest["manifest_sha256"] = Value::String(Sha256Digest::ZERO.to_string());
    let digest = self_digest(
        DomainTag::ActorAuthenticationKeyManifest,
        &manifest,
        "manifest_sha256",
    )?;
    manifest["manifest_sha256"] = Value::String(digest.to_string());
    Ok(manifest)
}

pub fn build_evidence_manifest(
    evidence_root: &Path,
    inputs: &[EvidenceInput],
) -> Result<(Value, Vec<u8>), TrustError> {
    let mut ordered = inputs.to_vec();
    for input in &mut ordered {
        input.dependency_ids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        input.dependency_ids.dedup();
    }
    ordered.sort_by(|left, right| {
        (&left.kind, &left.logical_id, &left.relative_path)
            .cmp(&(&right.kind, &right.logical_id, &right.relative_path))
    });
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut dependencies = BTreeMap::new();
    for input in &ordered {
        validate_relative_path(&input.relative_path)?;
        if !ids.insert(input.logical_id.clone()) || !paths.insert(input.relative_path.clone()) {
            return Err(TrustError::new(
                "bootstrap_evidence_duplicate",
                "evidence logical IDs and relative paths must be unique",
            ));
        }
        dependencies.insert(input.logical_id.clone(), input.dependency_ids.clone());
    }
    for (logical_id, dependency_ids) in &dependencies {
        for dependency in dependency_ids {
            if !ids.contains(dependency) {
                return Err(TrustError::new(
                    "bootstrap_evidence_dependency_missing",
                    format!("{logical_id} depends on missing {dependency}"),
                ));
            }
        }
    }
    reject_evidence_dependency_cycles(&dependencies)?;
    let mut leaves = Vec::with_capacity(ordered.len());
    for input in ordered {
        let bytes = read_regular_beneath(evidence_root, &input.relative_path)?;
        leaves.push(serde_json::json!({
            "kind": input.kind,
            "logical_id": input.logical_id,
            "relative_path": input.relative_path,
            "byte_length": bytes.len(),
            "sha256_of_raw_bytes": raw_sha256(&bytes),
            "dependency_ids": input.dependency_ids,
        }));
    }
    let evidence_root_sha256 = tagged_hash(
        DomainTag::EvidenceToolRoot,
        &canonical_json_value(&Value::Array(leaves.clone()))?,
    );
    let mut manifest = serde_json::json!({
        "schema": "trellis-evidence-tool-manifest/v1",
        "leaves": leaves,
        "evidence_tool_input_root": evidence_root_sha256,
        "manifest_sha256": Sha256Digest::ZERO,
    });
    let digest = self_digest(DomainTag::ManifestNode, &manifest, "manifest_sha256")?;
    manifest["manifest_sha256"] = Value::String(digest.to_string());
    let bytes = canonical_json_value(&manifest)?;
    verify_evidence_tool_manifest(evidence_root, &bytes)?;
    Ok((manifest, bytes))
}

fn reject_evidence_dependency_cycles(
    graph: &BTreeMap<String, Vec<String>>,
) -> Result<(), TrustError> {
    fn visit(
        logical_id: &str,
        graph: &BTreeMap<String, Vec<String>>,
        visiting: &mut BTreeSet<String>,
        complete: &mut BTreeSet<String>,
    ) -> Result<(), TrustError> {
        if complete.contains(logical_id) {
            return Ok(());
        }
        if !visiting.insert(logical_id.to_owned()) {
            return Err(TrustError::new(
                "bootstrap_evidence_dependency_cycle",
                format!("evidence dependency cycle reaches {logical_id}"),
            ));
        }
        for dependency in graph.get(logical_id).into_iter().flatten() {
            visit(dependency, graph, visiting, complete)?;
        }
        visiting.remove(logical_id);
        complete.insert(logical_id.to_owned());
        Ok(())
    }

    let mut complete = BTreeSet::new();
    for logical_id in graph.keys() {
        visit(logical_id, graph, &mut BTreeSet::new(), &mut complete)?;
    }
    Ok(())
}

fn read_regular_beneath(root: &Path, relative: &str) -> Result<Vec<u8>, TrustError> {
    let root_metadata = fs::symlink_metadata(root).map_err(|error| {
        TrustError::new(
            "bootstrap_evidence_root_unreadable",
            format!("{}: {error}", root.display()),
        )
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(TrustError::new(
            "bootstrap_evidence_root_invalid",
            format!("{} is not a non-symlink directory", root.display()),
        ));
    }
    let relative_path = Path::new(relative);
    let mut current = root.to_path_buf();
    let component_count = relative_path.components().count();
    for (index, component) in relative_path.components().enumerate() {
        let std::path::Component::Normal(component) = component else {
            return Err(TrustError::new(
                "bootstrap_relative_path_invalid",
                format!("invalid relative path {relative:?}"),
            ));
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            TrustError::new(
                "bootstrap_evidence_missing",
                format!("{}: {error}", current.display()),
            )
        })?;
        if metadata.file_type().is_symlink()
            || (index + 1 < component_count && !metadata.is_dir())
            || (index + 1 == component_count && !metadata.is_file())
        {
            return Err(TrustError::new(
                "bootstrap_evidence_not_regular",
                format!(
                    "{} traverses a symlink or is not a regular evidence file",
                    current.display()
                ),
            ));
        }
    }
    fs::read(&current)
        .map_err(|error| TrustError::new("bootstrap_evidence_unreadable", error.to_string()))
}

/// Build the honest baseline seed for targets without a checked source oracle.
/// Universal syntax is not enough to authorize Rust replay: every such target
/// is explicitly routed through `not_defined_for_claim_shape_v1` until a
/// claim-specific contract and refinement proof are added before the gate (or
/// through the exceptional audited-revision lane).
pub fn build_conservative_campaign_seed(
    request: ConservativeSeedRequest<'_>,
) -> Result<ConstructedSeed, TrustError> {
    if request.targets.is_empty() || request.contract_generator_sha256 == Sha256Digest::ZERO {
        return Err(TrustError::new(
            "bootstrap_seed_input_invalid",
            "at least one target and a nonzero generator identity are required",
        ));
    }
    let registry = SchemaRegistry::v1()?;
    let genesis = canonical_genesis(request.journal_id)?;
    let policy = JournalPolicy::embedded_v1(&registry)?;
    let mut identities = Vec::new();
    let mut bodies = Vec::new();
    let mut seen_targets = BTreeSet::new();
    let mut sorted_targets = request.targets.to_vec();
    sorted_targets.sort_by(|left, right| left.target_id.as_bytes().cmp(right.target_id.as_bytes()));

    struct PendingTarget {
        target: CampaignTargetInput,
        target_statement_sha256: Sha256Digest,
        normalized_claim_sha256: Sha256Digest,
        source_interpretation_sha256: Sha256Digest,
    }
    let mut pending = Vec::new();
    for target in sorted_targets {
        if !seen_targets.insert(target.target_id.clone()) {
            return Err(TrustError::new(
                "bootstrap_target_duplicate",
                format!("duplicate target {:?}", target.target_id),
            ));
        }
        let normalized_declaration = normalize_target_declaration(&target.lean_declaration)?;
        let target_statement_sha256 = raw_sha256(normalized_declaration.as_bytes());
        let target_body = serde_json::json!({
            "schema": "trellis-campaign-target-definition/v1",
            "target_id": target.target_id,
            "node_id": target.node_id,
            "normalized_lean_declaration_utf8": normalized_declaration,
            "informal": target.informal,
        });
        let target_digest = tagged_hash(
            DomainTag::TargetDefinition,
            &canonical_json_value(&target_body)?,
        );
        push_definition(
            &mut identities,
            &mut bodies,
            "target_definition",
            &target.target_id,
            "trellis://campaign/target-definition/v1",
            target_digest,
            DomainTag::TargetDefinition,
            target_body,
        );
        let interpretation_body = serde_json::json!({
            "schema": "trellis-campaign-source-interpretation/v1",
            "target_id": target.target_id,
            "model_target_statement_sha256": target_statement_sha256,
            "source_scope": "adapted_source",
            "source_validation_status": "not_defined_without_claim_specific_oracle",
            "reason": "logical shape alone does not define a finite Rust observation",
        });
        let interpretation_digest = tagged_hash(
            DomainTag::SourceInterpretation,
            &canonical_json_value(&interpretation_body)?,
        );
        push_definition(
            &mut identities,
            &mut bodies,
            "source_interpretation_definition",
            &format!("{}-source-interpretation", target.target_id),
            "trellis://campaign/source-interpretation/v1",
            interpretation_digest,
            DomainTag::SourceInterpretation,
            interpretation_body,
        );
        pending.push(PendingTarget {
            target,
            target_statement_sha256,
            normalized_claim_sha256: target_digest,
            source_interpretation_sha256: interpretation_digest,
        });
    }

    let foundation_specs = [
        (
            "precondition_definition",
            "no-source-precondition-closure",
            "trellis://campaign/precondition-definition/v1",
            DomainTag::PreconditionDefinition,
            serde_json::json!({
                "schema": "trellis-campaign-precondition-definition/v1",
                "status": "not_defined_for_unsupported_source_validation",
            }),
        ),
        (
            "carrier_refinement_definition",
            "unestablished-rust-aeneas-refinement",
            "trellis://campaign/carrier-refinement-definition/v1",
            DomainTag::CarrierRefinementDefinition,
            serde_json::json!({
                "schema": "trellis-campaign-carrier-refinement-definition/v1",
                "status": "unestablished",
                "authority": "none",
            }),
        ),
        (
            "build_definition",
            "adapted-source-build-semantics",
            "trellis://campaign/build-definition/v1",
            DomainTag::BuildDefinition,
            serde_json::json!({
                "schema": "trellis-campaign-build-definition/v1",
                "source_tree_sha256": request.source_tree_sha256,
                "source_scope": "adapted_source",
            }),
        ),
        (
            "semantic_validator",
            "unsupported-claim-classifier",
            "trellis://campaign/semantic-validator/v1",
            DomainTag::SemanticValidator,
            serde_json::json!({
                "schema": "trellis-campaign-semantic-validator/v1",
                "validator_sha256": request.contract_generator_sha256,
                "decision": "no_approved_source_oracle",
            }),
        ),
    ];
    let mut foundation = BTreeMap::new();
    for (kind, id, schema, tag, body) in foundation_specs {
        let digest = tagged_hash(tag, &canonical_json_value(&body)?);
        foundation.insert(id.to_owned(), digest);
        push_definition(
            &mut identities,
            &mut bodies,
            kind,
            id,
            schema,
            digest,
            tag,
            body,
        );
    }

    let preconditions = foundation["no-source-precondition-closure"];
    let refinement = foundation["unestablished-rust-aeneas-refinement"];
    let build = foundation["adapted-source-build-semantics"];
    let classifier = foundation["unsupported-claim-classifier"];
    for item in pending {
        let lineage_id = format!("{}-source-lineage", item.target.target_id);
        let contract_id = format!("{}-source-validation", item.target.target_id);
        let mut lineage = serde_json::json!({
            "schema": "trellis-source-claim-lineage/v1",
            "lineage_id": lineage_id,
            "target_id": item.target.target_id,
            "model_target_statement_sha256": item.target_statement_sha256,
            "rust_target_statement_sha256": item.source_interpretation_sha256,
            "source_claim_interpretation_sha256": item.source_interpretation_sha256,
            "normalized_claim_sha256": item.normalized_claim_sha256,
            "source_scope": "adapted_source",
            "source_tree_sha256": request.source_tree_sha256,
            "entry_point_semantics_sha256": item.source_interpretation_sha256,
            "precondition_manifest_sha256": preconditions,
            "refinement_semantics_sha256": refinement,
            "build_semantics_sha256": build,
            "concretization_erasure_semantics_sha256": refinement,
            "source_observation_semantics_sha256": item.source_interpretation_sha256,
            "lineage_change_kind": "initial_seed",
            "registration_authority": "seed_contract_v1",
            "registration_epoch": "seed",
            "lineage_generator_id": "trellis-trust-kernel",
            "lineage_generator_sha256": request.contract_generator_sha256,
            "registration_predecessor_head_sha256": genesis,
            "lineage_definition_sha256": Sha256Digest::ZERO,
        });
        let lineage_digest = self_digest(
            DomainTag::SourceClaimLineage,
            &lineage,
            "lineage_definition_sha256",
        )?;
        lineage["lineage_definition_sha256"] = Value::String(lineage_digest.to_string());
        let lineage_record = AuthoritativeRecord::parse(&registry, lineage.clone())?;
        push_definition(
            &mut identities,
            &mut bodies,
            "source_claim_lineage",
            &lineage_id,
            "trellis://schemas/source-claim-lineage/v1",
            lineage_record.digest(),
            DomainTag::SourceClaimLineage,
            lineage,
        );

        let mut contract = serde_json::json!({
            "schema": "trellis-source-validation-contract/v1",
            "contract_id": contract_id,
            "target_id": item.target.target_id,
            "target_statement_sha256": item.target_statement_sha256,
            "source_claim_lineage_id": lineage_id,
            "source_claim_lineage_sha256": lineage_record.digest(),
            "rust_target_statement_sha256": item.source_interpretation_sha256,
            "source_claim_interpretation_sha256": item.source_interpretation_sha256,
            "claim_shape": "other",
            "normalized_claim_schema_id": "trellis://campaign/normalized-lean-claim/v1",
            "normalized_claim_sha256": item.normalized_claim_sha256,
            "negative_certificate_class": "unsupported_negative_certificate_v1",
            "formal_refutation_certificate_schema_id": "unsupported-negative-certificate/v1",
            "source_counterevidence_class": "no_approved_source_oracle",
            "validation_method": "not_defined_for_claim_shape_v1",
            "adequacy": "undefined",
            "contract_generator_id": "trellis-trust-kernel",
            "contract_generator_sha256": request.contract_generator_sha256,
            "classification_proof_artifact_sha256": classifier,
            "source_scope": "adapted_source",
            "source_tree_sha256": request.source_tree_sha256,
            "refinement_closure_sha256": refinement,
            "build_tool_closure_sha256": build,
            "nondeterminism_environment_policy": "unsupported_in_v1",
            "qualification_permission": "prohibited",
            "registration_epoch": "seed",
            "registration_predecessor_head_sha256": genesis,
            "contract_definition_sha256": Sha256Digest::ZERO,
        });
        let contract_digest = self_digest(
            DomainTag::SourceValidationContract,
            &contract,
            "contract_definition_sha256",
        )?;
        contract["contract_definition_sha256"] = Value::String(contract_digest.to_string());
        let contract_record = AuthoritativeRecord::parse(&registry, contract.clone())?;
        push_definition(
            &mut identities,
            &mut bodies,
            "source_validation_contract",
            &contract_id,
            "trellis://schemas/source-validation-contract/v1",
            contract_record.digest(),
            DomainTag::SourceValidationContract,
            contract,
        );
    }

    let mut zipped: Vec<_> = identities.into_iter().zip(bodies).collect();
    zipped.sort_by(|(left, _), (right, _)| {
        seed_rank(left["record_kind"].as_str().unwrap_or(""))
            .cmp(&seed_rank(right["record_kind"].as_str().unwrap_or("")))
            .then_with(|| {
                left["record_id"]
                    .as_str()
                    .unwrap_or("")
                    .as_bytes()
                    .cmp(right["record_id"].as_str().unwrap_or("").as_bytes())
            })
    });
    let (identities, bodies): (Vec<_>, Vec<_>) = zipped.into_iter().unzip();
    let authored_root = tagged_hash(
        DomainTag::AuthoredSemanticRoot,
        &canonical_json_value(&Value::Array(identities.clone()))?,
    );
    let mut seed = serde_json::json!({
        "schema": "trellis-seed-authored-definition-manifest/v1",
        "protocol_id": "trellis-trust-v1",
        "journal_id": request.journal_id,
        "run_id": request.run_id,
        "seed_plan_id": request.seed_plan_id,
        "canonical_genesis_sha256": genesis,
        "definitions": identities,
        "authored_semantic_root": authored_root,
        "approved_evidence_tool_input_root": request.evidence_tool_input_root,
        "journal_event_policy_sha256": policy.digest(),
        "actor_key_manifest_sha256": request.actor_key_manifest.digest(),
        "manifest_sha256": Sha256Digest::ZERO,
    });
    let seed_digest = self_digest(
        DomainTag::SeedAuthoredDefinitionManifest,
        &seed,
        "manifest_sha256",
    )?;
    seed["manifest_sha256"] = Value::String(seed_digest.to_string());
    let seed_record = AuthoritativeRecord::parse(&registry, seed)?;
    let mut bundle = serde_json::json!({
        "schema": "trellis-seed-definition-bundle/v1",
        "definitions": bodies,
        "bundle_sha256": Sha256Digest::ZERO,
    });
    let bundle_digest = self_digest(DomainTag::ManifestNode, &bundle, "bundle_sha256")?;
    bundle["bundle_sha256"] = Value::String(bundle_digest.to_string());
    let seed_manifest_bytes = seed_record.canonical_bytes()?;
    let seed_definition_bundle_bytes = canonical_json_value(&bundle)?;
    verify_seed_definition_bundle(&seed_record, &seed_definition_bundle_bytes)?;
    let gate = render_gate_presentation(
        request,
        seed_record.digest(),
        bundle_digest,
        authored_root,
    );
    Ok(ConstructedSeed {
        seed_manifest: seed_record,
        seed_manifest_bytes,
        seed_definition_bundle: bundle,
        seed_definition_bundle_bytes,
        gate_presentation_bytes: gate.into_bytes(),
    })
}

/// Build a campaign seed from the explicit, evidence-bound trust plan.  The
/// plan must cover every configured target exactly once. Operational tool and
/// basis identities are resolved from the already verified pre-gate evidence
/// closure; no digest-shaped caller input can acquire authority here.
pub fn build_campaign_seed(
    request: CampaignSeedRequest<'_>,
) -> Result<ConstructedSeed, TrustError> {
    request.trust_plan.validate()?;
    if request.targets.is_empty()
        || request.evidence.evidence_tool_input_root == Sha256Digest::ZERO
    {
        return Err(TrustError::new(
            "bootstrap_seed_input_invalid",
            "campaign seed needs targets and a verified nonzero evidence closure",
        ));
    }
    let kernel_digest = evidence_leaf_digest(request.evidence, "trellis-trust-kernel")?;
    let evidence_tool_digest =
        evidence_leaf_digest(request.evidence, "campaign-evidence-tool")?;
    if request
        .trusted_platform_boundary
        .get("schema")
        .and_then(Value::as_str)
        != Some("trellis-trusted-platform-boundary/v1")
    {
        return Err(TrustError::new(
            "bootstrap_trusted_platform_boundary_invalid",
            "trusted-platform boundary has the wrong schema",
        ));
    }
    let boundary_leaf = request
        .evidence
        .leaves_by_logical_id
        .get("trusted-platform-boundary-v1")
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_trusted_platform_boundary_missing",
                "approved evidence lacks the trusted-platform boundary",
            )
        })?;
    if raw_sha256(&canonical_json_value(request.trusted_platform_boundary)?)
        != boundary_leaf.raw_sha256
    {
        return Err(TrustError::new(
            "bootstrap_trusted_platform_boundary_mismatch",
            "displayed trusted-platform boundary differs from its evidence leaf",
        ));
    }
    for (logical_id, schema, value) in [
        (
            "extraction-result",
            "trellis-extraction-result/v1",
            request.extraction_result,
        ),
        (
            "extraction-determinism",
            "trellis-extraction-determinism/v1",
            request.extraction_determinism,
        ),
    ] {
        if value.get("schema").and_then(Value::as_str) != Some(schema) {
            return Err(TrustError::new(
                "bootstrap_extraction_evidence_invalid",
                format!("{logical_id} has the wrong schema"),
            ));
        }
        let leaf = request
            .evidence
            .leaves_by_logical_id
            .get(logical_id)
            .ok_or_else(|| {
                TrustError::new(
                    "bootstrap_extraction_evidence_missing",
                    format!("approved evidence lacks {logical_id}"),
                )
            })?;
        if raw_sha256(&canonical_json_value(value)?) != leaf.raw_sha256 {
            return Err(TrustError::new(
                "bootstrap_extraction_evidence_mismatch",
                format!("displayed {logical_id} differs from its approved evidence leaf"),
            ));
        }
    }
    let mut plan_by_target = BTreeMap::new();
    for target in &request.trust_plan.targets {
        if plan_by_target.insert(target.target_id.as_str(), target).is_some() {
            return Err(TrustError::new(
                "bootstrap_trust_plan_target_duplicate",
                "trust plan repeats a target ID",
            ));
        }
    }
    let configured_ids: BTreeSet<_> = request
        .targets
        .iter()
        .map(|target| target.target_id.as_str())
        .collect();
    let planned_ids: BTreeSet<_> = plan_by_target.keys().copied().collect();
    if configured_ids.len() != request.targets.len() || configured_ids != planned_ids {
        return Err(TrustError::new(
            "bootstrap_trust_plan_target_mismatch",
            "trust plan target IDs must exactly cover the configured campaign targets",
        ));
    }

    let registry = SchemaRegistry::v1()?;
    let genesis = canonical_genesis(request.journal_id)?;
    let policy = JournalPolicy::embedded_v1(&registry)?;
    let mut identities = Vec::new();
    let mut bodies = Vec::new();
    let mut profiles = Vec::new();
    let mut sorted_targets = request.targets.to_vec();
    sorted_targets.sort_by(|left, right| left.target_id.as_bytes().cmp(right.target_id.as_bytes()));

    for target in &sorted_targets {
        let target_plan = plan_by_target
            .get(target.target_id.as_str())
            .copied()
            .expect("target coverage checked above");
        let normalized_declaration = normalize_target_declaration(&target.lean_declaration)?;
        let target_statement_sha256 = raw_sha256(normalized_declaration.as_bytes());
        let target_body = serde_json::json!({
            "schema": "trellis-campaign-target-definition/v1",
            "target_id": target.target_id,
            "node_id": target.node_id,
            "normalized_lean_declaration_utf8": normalized_declaration,
            "informal": target.informal,
        });
        let normalized_claim_sha256 = push_plain_definition(
            &mut identities,
            &mut bodies,
            "target_definition",
            &target.target_id,
            "trellis://campaign/target-definition/v1",
            DomainTag::TargetDefinition,
            target_body,
        )?;

        let semantics = build_target_semantics(
            target,
            &target_plan.source_validation,
            request.source_tree_sha256,
            evidence_tool_digest,
            &mut identities,
            &mut bodies,
        )?;
        let lineage_id = format!("{}-source-lineage", target.target_id);
        let contract_id = format!("{}-source-validation", target.target_id);
        let mut lineage = serde_json::json!({
            "schema": "trellis-source-claim-lineage/v1",
            "lineage_id": lineage_id,
            "target_id": target.target_id,
            "model_target_statement_sha256": target_statement_sha256,
            "rust_target_statement_sha256": semantics.rust_target_statement_sha256,
            "source_claim_interpretation_sha256": semantics.source_interpretation_sha256,
            "normalized_claim_sha256": normalized_claim_sha256,
            "source_scope": "adapted_source",
            "source_tree_sha256": request.source_tree_sha256,
            "entry_point_semantics_sha256": semantics.source_interpretation_sha256,
            "precondition_manifest_sha256": semantics.precondition_sha256,
            "refinement_semantics_sha256": semantics.refinement_sha256,
            "build_semantics_sha256": semantics.build_sha256,
            "concretization_erasure_semantics_sha256": semantics.refinement_sha256,
            "source_observation_semantics_sha256": semantics.source_interpretation_sha256,
            "lineage_change_kind": "initial_seed",
            "registration_authority": "seed_contract_v1",
            "registration_epoch": "seed",
            "lineage_generator_id": "trellis-trust-kernel",
            "lineage_generator_sha256": kernel_digest,
            "registration_predecessor_head_sha256": genesis,
            "lineage_definition_sha256": Sha256Digest::ZERO,
        });
        let lineage_record = seal_record(
            &registry,
            DomainTag::SourceClaimLineage,
            &mut lineage,
            "lineage_definition_sha256",
        )?;
        push_definition(
            &mut identities,
            &mut bodies,
            "source_claim_lineage",
            &lineage_id,
            "trellis://schemas/source-claim-lineage/v1",
            lineage_record.digest(),
            DomainTag::SourceClaimLineage,
            lineage,
        );

        let mut contract = match &target_plan.source_validation {
            TargetSourceValidationPlan::NotDefinedForClaimShapeV1 {
                claim_shape,
                source_counterevidence_class,
                ..
            } => serde_json::json!({
                "schema": "trellis-source-validation-contract/v1",
                "contract_id": contract_id,
                "target_id": target.target_id,
                "target_statement_sha256": target_statement_sha256,
                "source_claim_lineage_id": lineage_id,
                "source_claim_lineage_sha256": lineage_record.digest(),
                "rust_target_statement_sha256": semantics.rust_target_statement_sha256,
                "source_claim_interpretation_sha256": semantics.source_interpretation_sha256,
                "claim_shape": claim_shape.as_str(),
                "normalized_claim_schema_id": "trellis://campaign/normalized-lean-claim/v1",
                "normalized_claim_sha256": normalized_claim_sha256,
                "negative_certificate_class": "unsupported_negative_certificate_v1",
                "formal_refutation_certificate_schema_id": "unsupported-negative-certificate/v1",
                "source_counterevidence_class": source_counterevidence_class.as_str(),
                "validation_method": "not_defined_for_claim_shape_v1",
                "adequacy": "undefined",
                "contract_generator_id": "trellis-trust-kernel",
                "contract_generator_sha256": kernel_digest,
                "classification_proof_artifact_sha256": semantics.validator_sha256,
                "source_scope": "adapted_source",
                "source_tree_sha256": request.source_tree_sha256,
                "refinement_closure_sha256": semantics.refinement_sha256,
                "build_tool_closure_sha256": semantics.build_sha256,
                "nondeterminism_environment_policy": "unsupported_in_v1",
                "qualification_permission": "prohibited",
                "registration_epoch": "seed",
                "registration_predecessor_head_sha256": genesis,
                "contract_definition_sha256": Sha256Digest::ZERO,
            }),
            TargetSourceValidationPlan::ExactRustExecutionV1 {
                public_entry_point,
                preconditions,
                qualification,
                ..
            } => {
                let precondition_values: Vec<_> = preconditions
                    .iter()
                    .map(|precondition| {
                        serde_json::json!({
                            "precondition_id": precondition.precondition_id,
                            "normalized_statement_sha256": raw_sha256(
                                precondition.normalized_statement_utf8.as_bytes()
                            ),
                            "checker_sha256": evidence_tool_digest,
                        })
                    })
                    .collect();
                let mut contract = serde_json::json!({
                    "schema": "trellis-source-validation-contract/v1",
                    "contract_id": contract_id,
                    "target_id": target.target_id,
                    "target_statement_sha256": target_statement_sha256,
                    "source_claim_lineage_id": lineage_id,
                    "source_claim_lineage_sha256": lineage_record.digest(),
                    "rust_target_statement_sha256": semantics.rust_target_statement_sha256,
                    "source_claim_interpretation_sha256": semantics.source_interpretation_sha256,
                    "claim_shape": "universal",
                    "normalized_claim_schema_id": "forall-executable-input-postcondition/v1",
                    "normalized_claim_sha256": normalized_claim_sha256,
                    "negative_certificate_class": "forall_executable_input_postcondition_v1",
                    "formal_refutation_certificate_schema_id": "finite-input-counterexample/v1",
                    "source_counterevidence_class": "finite_executable_counterexample",
                    "validation_method": "exact_rust_execution_v1",
                    "adequacy": "decisive",
                });
                let operational = serde_json::json!({
                    "contract_generator_id": "trellis-trust-kernel",
                    "contract_generator_sha256": kernel_digest,
                    "classification_proof_artifact_sha256": semantics.validator_sha256,
                    "source_scope": "adapted_source",
                    "source_tree_sha256": request.source_tree_sha256,
                    "refinement_closure_sha256": semantics.refinement_sha256,
                    "build_tool_closure_sha256": semantics.build_sha256,
                    "nondeterminism_environment_policy": "closed_deterministic_v1",
                    "qualification_permission": if qualification.is_some() {
                        "resource_profiles_if_independent_scope_limit"
                    } else {
                        "prohibited"
                    },
                    "registration_epoch": "seed",
                    "registration_predecessor_head_sha256": genesis,
                    "contract_definition_sha256": Sha256Digest::ZERO,
                });
                let exact = serde_json::json!({
                    "public_entry_point": public_entry_point,
                    "binder_domain_schema_sha256": semantics.source_interpretation_sha256,
                    "entry_point_type_sha256": semantics.build_sha256,
                    "environment_contract_sha256": semantics.build_sha256,
                    "concretization_schema_sha256": semantics.refinement_sha256,
                    "erasure_relation_sha256": semantics.refinement_sha256,
                    "raw_observation_schema_sha256": semantics.source_interpretation_sha256,
                    "negative_evidence_soundness_statement_sha256": semantics.validator_sha256,
                    "negative_evidence_soundness_proof_sha256": evidence_tool_digest,
                    "preconditions": precondition_values,
                    "formal_predicate_generator_sha256": evidence_tool_digest,
                    "source_validator_sha256": evidence_tool_digest,
                    "observation_oracle_generator_sha256": evidence_tool_digest,
                    "observation_oracle_sha256": evidence_tool_digest,
                    "concretization_validator_sha256": evidence_tool_digest,
                    "determinism_audit_sha256": evidence_tool_digest,
                    "toolchain_build_basis_sha256": evidence_tool_digest,
                    "runner_sha256": evidence_tool_digest,
                });
                contract
                    .as_object_mut()
                    .expect("contract literal is an object")
                    .extend(
                        operational
                            .as_object()
                            .expect("operational literal is an object")
                            .clone(),
                    );
                contract
                    .as_object_mut()
                    .expect("contract literal is an object")
                    .extend(
                        exact
                            .as_object()
                            .expect("exact contract literal is an object")
                            .clone(),
                    );
                contract
            }
        };
        let contract_record = seal_record(
            &registry,
            DomainTag::SourceValidationContract,
            &mut contract,
            "contract_definition_sha256",
        )?;
        SourceValidationContractView::from_record(&contract_record)?;
        push_definition(
            &mut identities,
            &mut bodies,
            "source_validation_contract",
            &contract_id,
            "trellis://schemas/source-validation-contract/v1",
            contract_record.digest(),
            DomainTag::SourceValidationContract,
            contract,
        );

        if let TargetSourceValidationPlan::ExactRustExecutionV1 {
            qualification: Some(qualification),
            ..
        } = &target_plan.source_validation
        {
            profiles.push(build_qualification_definitions(
                &request,
                &registry,
                genesis,
                target,
                target_statement_sha256,
                &lineage_id,
                &lineage_record,
                &contract_id,
                &contract_record,
                qualification,
                evidence_tool_digest,
                &mut identities,
                &mut bodies,
            )?);
        }
    }

    if !profiles.is_empty() {
        profiles.sort_by(|left, right| {
            left["profile_id"]
                .as_str()
                .unwrap_or("")
                .as_bytes()
                .cmp(right["profile_id"].as_str().unwrap_or("").as_bytes())
        });
        let mut catalog = serde_json::json!({
            "schema": "trellis-qualification-profile-catalog/v1",
            "profiles": profiles,
            "catalog_definition_sha256": Sha256Digest::ZERO,
        });
        let catalog_record = seal_record(
            &registry,
            DomainTag::QualificationProfileCatalog,
            &mut catalog,
            "catalog_definition_sha256",
        )?;
        push_definition(
            &mut identities,
            &mut bodies,
            "qualification_profile_catalog",
            "campaign-qualification-profile-catalog",
            "trellis://schemas/qualification-profile-catalog/v1",
            catalog_record.digest(),
            DomainTag::QualificationProfileCatalog,
            catalog,
        );
    }

    let mut zipped: Vec<_> = identities.into_iter().zip(bodies).collect();
    zipped.sort_by(|(left, _), (right, _)| {
        seed_rank(left["record_kind"].as_str().unwrap_or(""))
            .cmp(&seed_rank(right["record_kind"].as_str().unwrap_or("")))
            .then_with(|| {
                left["record_id"]
                    .as_str()
                    .unwrap_or("")
                    .as_bytes()
                    .cmp(right["record_id"].as_str().unwrap_or("").as_bytes())
            })
    });
    let (identities, bodies): (Vec<_>, Vec<_>) = zipped.into_iter().unzip();
    let authored_root = tagged_hash(
        DomainTag::AuthoredSemanticRoot,
        &canonical_json_value(&Value::Array(identities.clone()))?,
    );
    let mut seed = serde_json::json!({
        "schema": "trellis-seed-authored-definition-manifest/v1",
        "protocol_id": "trellis-trust-v1",
        "journal_id": request.journal_id,
        "run_id": request.run_id,
        "seed_plan_id": request.seed_plan_id,
        "canonical_genesis_sha256": genesis,
        "definitions": identities,
        "authored_semantic_root": authored_root,
        "approved_evidence_tool_input_root": request.evidence.evidence_tool_input_root,
        "journal_event_policy_sha256": policy.digest(),
        "actor_key_manifest_sha256": request.actor_key_manifest.digest(),
        "manifest_sha256": Sha256Digest::ZERO,
    });
    let seed_digest = self_digest(
        DomainTag::SeedAuthoredDefinitionManifest,
        &seed,
        "manifest_sha256",
    )?;
    seed["manifest_sha256"] = Value::String(seed_digest.to_string());
    let seed_record = AuthoritativeRecord::parse(&registry, seed)?;
    let mut bundle = serde_json::json!({
        "schema": "trellis-seed-definition-bundle/v1",
        "definitions": bodies,
        "bundle_sha256": Sha256Digest::ZERO,
    });
    let bundle_digest = self_digest(DomainTag::ManifestNode, &bundle, "bundle_sha256")?;
    bundle["bundle_sha256"] = Value::String(bundle_digest.to_string());
    let seed_manifest_bytes = seed_record.canonical_bytes()?;
    let seed_definition_bundle_bytes = canonical_json_value(&bundle)?;
    let verified_seed = verify_seed_definition_bundle(&seed_record, &seed_definition_bundle_bytes)?;
    for basis in verified_seed.records_by_digest.values().filter(|record| {
        record.contract().record_schema == "trellis-independent-basis/v1"
    }) {
        validate_independent_basis(&verified_seed, basis)?;
    }
    for contract in verified_seed.records_by_digest.values().filter(|record| {
        record.contract().record_schema == "trellis-source-validation-contract/v1"
    }) {
        SourceValidationContractView::from_record(contract)?;
    }
    let gate = render_campaign_gate_presentation(
        &request,
        seed_record.digest(),
        bundle_digest,
        authored_root,
    );
    Ok(ConstructedSeed {
        seed_manifest: seed_record,
        seed_manifest_bytes,
        seed_definition_bundle: bundle,
        seed_definition_bundle_bytes,
        gate_presentation_bytes: gate.into_bytes(),
    })
}

struct TargetSemantics {
    rust_target_statement_sha256: Sha256Digest,
    source_interpretation_sha256: Sha256Digest,
    precondition_sha256: Sha256Digest,
    refinement_sha256: Sha256Digest,
    build_sha256: Sha256Digest,
    validator_sha256: Sha256Digest,
}

fn build_target_semantics(
    target: &CampaignTargetInput,
    plan: &TargetSourceValidationPlan,
    source_tree_sha256: Sha256Digest,
    evidence_tool_sha256: Sha256Digest,
    identities: &mut Vec<Value>,
    bodies: &mut Vec<Value>,
) -> Result<TargetSemantics, TrustError> {
    let prefix = &target.target_id;
    let (interpretation, precondition, refinement, build, validator, rust_statement) = match plan {
        TargetSourceValidationPlan::NotDefinedForClaimShapeV1 { reason, .. } => (
            serde_json::json!({
                "schema": "trellis-campaign-source-interpretation/v1",
                "target_id": target.target_id,
                "source_scope": "adapted_source",
                "source_validation_status": "not_defined_without_claim_specific_oracle",
                "reason": reason,
            }),
            serde_json::json!({
                "schema": "trellis-campaign-precondition-definition/v1",
                "target_id": target.target_id,
                "status": "not_defined_for_unsupported_source_validation",
            }),
            serde_json::json!({
                "schema": "trellis-campaign-carrier-refinement-definition/v1",
                "target_id": target.target_id,
                "status": "unestablished",
                "authority": "none",
            }),
            serde_json::json!({
                "schema": "trellis-campaign-build-definition/v1",
                "target_id": target.target_id,
                "source_tree_sha256": source_tree_sha256,
                "source_scope": "adapted_source",
            }),
            serde_json::json!({
                "schema": "trellis-campaign-semantic-validator/v1",
                "target_id": target.target_id,
                "validator_sha256": evidence_tool_sha256,
                "decision": "no_approved_source_oracle",
            }),
            None,
        ),
        TargetSourceValidationPlan::ExactRustExecutionV1 {
            public_entry_point,
            rust_target_statement_utf8,
            source_claim_interpretation_utf8,
            binder_domain_schema_utf8,
            preconditions,
            entry_point_type_utf8,
            environment_contract_utf8,
            concretization_schema_utf8,
            erasure_relation_utf8,
            raw_observation_schema_utf8,
            negative_evidence_soundness_statement_utf8,
            ..
        } => {
            if target.model_refinement_sha256 == Sha256Digest::ZERO {
                return Err(TrustError::new(
                    "bootstrap_model_refinement_unchecked",
                    "exact Rust validation requires a checked generated-model call-path/boundary-integration closure",
                ));
            }
            (
            serde_json::json!({
                "schema": "trellis-campaign-source-interpretation/v1",
                "target_id": target.target_id,
                "source_scope": "adapted_source",
                "source_validation_status": "exact_rust_execution_v1",
                "public_entry_point": public_entry_point,
                "rust_target_statement_utf8": rust_target_statement_utf8,
                "source_claim_interpretation_utf8": source_claim_interpretation_utf8,
                "binder_domain_schema_utf8": binder_domain_schema_utf8,
                "raw_observation_schema_utf8": raw_observation_schema_utf8,
            }),
            serde_json::json!({
                "schema": "trellis-campaign-precondition-definition/v1",
                "target_id": target.target_id,
                "preconditions": preconditions.iter().map(|item| serde_json::json!({
                    "precondition_id": item.precondition_id,
                    "normalized_statement_utf8": item.normalized_statement_utf8,
                    "checker_sha256": evidence_tool_sha256,
                })).collect::<Vec<_>>(),
            }),
            serde_json::json!({
                "schema": "trellis-campaign-carrier-refinement-definition/v1",
                "target_id": target.target_id,
                "concretization_schema_utf8": concretization_schema_utf8,
                "erasure_relation_utf8": erasure_relation_utf8,
                "relation_statement_sha256": raw_sha256(erasure_relation_utf8.as_bytes()),
                "integrated_model_call_path_closure_sha256": target.model_refinement_sha256,
                "simulation_assurance": "explicit_semantic_trust_item",
                "explicit_trust_item_id": "trellis-rust-charon-aeneas-simulation-boundary-v1",
                "explicit_trust_statement_utf8": concat!(
                    "For the seed-pinned Rust source, rustc/build semantics, Charon revision, ",
                    "Aeneas revision, extracted entry point, and reachable reviewed boundary ",
                    "definitions, a valid Rust execution related by erasure_relation_utf8 is ",
                    "represented by the generated Aeneas execution with corresponding result ",
                    "or failure behavior. This simulation bridge is explicitly trusted; the ",
                    "integrated Lean call path and absence of reachable opaque stubs are checked."
                ),
                "reference_erasure_limit_utf8": concat!(
                    "The Aeneas value does not retain Rust address, allocation identity, lifetime, ",
                    "or provenance, and no result may reconstruct those facts from the model value."
                ),
            }),
            serde_json::json!({
                "schema": "trellis-campaign-build-definition/v1",
                "target_id": target.target_id,
                "source_tree_sha256": source_tree_sha256,
                "source_scope": "adapted_source",
                "public_entry_point": public_entry_point,
                "entry_point_type_utf8": entry_point_type_utf8,
                "environment_contract_utf8": environment_contract_utf8,
            }),
            serde_json::json!({
                "schema": "trellis-campaign-semantic-validator/v1",
                "target_id": target.target_id,
                "validator_sha256": evidence_tool_sha256,
                "negative_evidence_soundness_statement_utf8": negative_evidence_soundness_statement_utf8,
            }),
            Some(rust_target_statement_utf8.as_str()),
        )},
    };
    let source_interpretation_sha256 = push_plain_definition(
        identities,
        bodies,
        "source_interpretation_definition",
        &format!("{prefix}-source-interpretation"),
        "trellis://campaign/source-interpretation/v1",
        DomainTag::SourceInterpretation,
        interpretation,
    )?;
    let precondition_sha256 = push_plain_definition(
        identities,
        bodies,
        "precondition_definition",
        &format!("{prefix}-precondition-closure"),
        "trellis://campaign/precondition-definition/v1",
        DomainTag::PreconditionDefinition,
        precondition,
    )?;
    let refinement_sha256 = push_plain_definition(
        identities,
        bodies,
        "carrier_refinement_definition",
        &format!("{prefix}-carrier-refinement"),
        "trellis://campaign/carrier-refinement-definition/v1",
        DomainTag::CarrierRefinementDefinition,
        refinement,
    )?;
    let build_sha256 = push_plain_definition(
        identities,
        bodies,
        "build_definition",
        &format!("{prefix}-build-semantics"),
        "trellis://campaign/build-definition/v1",
        DomainTag::BuildDefinition,
        build,
    )?;
    let validator_sha256 = push_plain_definition(
        identities,
        bodies,
        "semantic_validator",
        &format!("{prefix}-semantic-validator"),
        "trellis://campaign/semantic-validator/v1",
        DomainTag::SemanticValidator,
        validator,
    )?;
    Ok(TargetSemantics {
        rust_target_statement_sha256: rust_statement
            .map(|statement| raw_sha256(statement.as_bytes()))
            .unwrap_or(source_interpretation_sha256),
        source_interpretation_sha256,
        precondition_sha256,
        refinement_sha256,
        build_sha256,
        validator_sha256,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_qualification_definitions(
    request: &CampaignSeedRequest<'_>,
    registry: &SchemaRegistry,
    genesis: Sha256Digest,
    target: &CampaignTargetInput,
    target_statement_sha256: Sha256Digest,
    lineage_id: &str,
    lineage_record: &AuthoritativeRecord,
    contract_id: &str,
    contract_record: &AuthoritativeRecord,
    plan: &ResourceQualificationPlan,
    evidence_tool_sha256: Sha256Digest,
    identities: &mut Vec<Value>,
    bodies: &mut Vec<Value>,
) -> Result<Value, TrustError> {
    let profile_id = &plan.profile_id;
    let registry_id = format!("{profile_id}-basis-fact-class-registry");
    let mut entries = Vec::new();
    for class in [
        "configuration_limit",
        "deployment_limit",
        "external_attestation_limit",
        "host_capability_limit",
        "source_limit",
    ] {
        let selected = class == plan.basis_fact.fact_class.as_str();
        let fact_schema_id = if selected {
            plan.basis_fact.fact_schema_id.clone()
        } else {
            format!("trellis://facts/{class}/v1")
        };
        let (allowed_scopes, allowed_enforcements) = match class {
            "configuration_limit" => (vec!["configuration"], vec!["configuration_enforced"]),
            "deployment_limit" => (vec!["deployment"], vec!["deployment_enforced"]),
            "external_attestation_limit" => {
                (vec!["deployment", "host"], vec!["deployment_enforced", "observed"])
            }
            "host_capability_limit" => (vec!["host"], vec!["observed"]),
            "source_limit" => (vec!["source"], vec!["source_enforced"]),
            _ => unreachable!(),
        };
        let mut entry = serde_json::json!({
            "fact_class": class,
            "fact_schema_id": fact_schema_id,
            "fact_schema_sha256": raw_sha256(fact_schema_id.as_bytes()),
            "validator_id": "campaign-evidence-tool",
            "validator_sha256": evidence_tool_sha256,
            "allowed_scopes": allowed_scopes,
            "allowed_enforcements": allowed_enforcements,
            "entry_sha256": Sha256Digest::ZERO,
        });
        if class == "external_attestation_limit" {
            entry["attestation_authority_manifest_sha256"] =
                Value::String(evidence_tool_sha256.to_string());
            entry["attestation_authorization_policy_sha256"] =
                Value::String(evidence_tool_sha256.to_string());
        }
        let digest = self_digest(
            DomainTag::BasisFactClassRegistryEntry,
            &entry,
            "entry_sha256",
        )?;
        entry["entry_sha256"] = Value::String(digest.to_string());
        entries.push(entry);
    }
    let mut fact_registry = serde_json::json!({
        "schema": "trellis-basis-fact-class-registry/v1",
        "registry_id": registry_id,
        "entries": entries,
        "registration_epoch": "seed",
        "registration_predecessor_head_sha256": genesis,
        "registry_sha256": Sha256Digest::ZERO,
    });
    let fact_registry_record = seal_record(
        registry,
        DomainTag::BasisFactClassRegistry,
        &mut fact_registry,
        "registry_sha256",
    )?;
    push_definition(
        identities,
        bodies,
        "basis_fact_class_registry",
        &registry_id,
        "trellis://schemas/basis-fact-class-registry/v1",
        fact_registry_record.digest(),
        DomainTag::BasisFactClassRegistry,
        fact_registry.clone(),
    );

    push_plain_definition(
        identities,
        bodies,
        "conditionalization_schema",
        &format!("{profile_id}-conditionalization-schema"),
        "trellis://campaign/conditionalization-schema/v1",
        DomainTag::ConditionalizationSchema,
        serde_json::json!({
            "schema": "trellis-campaign-conditionalization-schema/v1",
            "conditionalization_schema_id": plan.conditionalization_schema_id,
            "kind": "resource_bound_implication_v1",
        }),
    )?;

    let mut measure = serde_json::json!({
        "measure_id": plan.measure_id,
        "binder": plan.binder,
        "binder_type": plan.binder_type,
        "units": plan.units.as_str(),
        "expression_ast": {
            "kind": "structural_input_byte_length_v1",
            "binder": plan.binder,
        },
        "measure_definition_sha256": Sha256Digest::ZERO,
    });
    let measure_sha256 = self_digest(
        DomainTag::MeasureDefinition,
        &measure,
        "measure_definition_sha256",
    )?;
    measure["measure_definition_sha256"] = Value::String(measure_sha256.to_string());
    let measure_catalog_id = format!("{profile_id}-measure-catalog");
    let mut measure_catalog = serde_json::json!({
        "schema": "trellis-measure-catalog/v1",
        "catalog_id": measure_catalog_id,
        "measures": [measure],
        "registration_epoch": "seed",
        "registration_predecessor_head_sha256": genesis,
        "catalog_sha256": Sha256Digest::ZERO,
    });
    let measure_catalog_record = seal_record(
        registry,
        DomainTag::MeasureCatalog,
        &mut measure_catalog,
        "catalog_sha256",
    )?;
    push_definition(
        identities,
        bodies,
        "measure_catalog",
        &measure_catalog_id,
        "trellis://schemas/measure-catalog/v1",
        measure_catalog_record.digest(),
        DomainTag::MeasureCatalog,
        measure_catalog,
    );

    let evidence_leaf = request
        .evidence
        .leaves_by_logical_id
        .get(&plan.basis_fact.evidence_logical_id)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_basis_evidence_missing",
                format!(
                    "basis logical ID {:?} is absent from the pre-gate evidence closure",
                    plan.basis_fact.evidence_logical_id
                ),
            )
        })?;
    let fact_artifact = request
        .basis_fact_artifacts
        .get(&plan.basis_fact.evidence_logical_id)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_basis_artifact_missing",
                "basis evidence lacks its exact parsed canonical fact artifact",
            )
        })?;
    let fact_bytes = canonical_json_value(fact_artifact)?;
    if raw_sha256(&fact_bytes) != evidence_leaf.raw_sha256
        || fact_artifact.get("schema").and_then(Value::as_str)
            != Some(plan.basis_fact.fact_schema_id.as_str())
    {
        return Err(TrustError::new(
            "bootstrap_basis_artifact_identity_mismatch",
            "basis fact schema or canonical bytes differ from the approved evidence leaf",
        ));
    }
    if plan.basis_fact.fact_class == BasisFactClass::HostCapabilityLimit {
        validate_captured_host_fact_for_seed(fact_artifact, evidence_leaf_digest(
            request.evidence,
            "trellis-trust-kernel",
        )?)?;
    }
    let artifact_bound = ["addressable_bytes_upper_bound", "bound", "limit"]
        .into_iter()
        .find_map(|field| fact_artifact.get(field).and_then(Value::as_str));
    if artifact_bound != Some(plan.bound.as_str()) {
        return Err(TrustError::new(
            "bootstrap_basis_bound_mismatch",
            "qualification bound must be derived from the exact pre-gate basis fact artifact",
        ));
    }
    let selected_entry = fact_registry["entries"]
        .as_array()
        .and_then(|entries| {
            entries.iter().find(|entry| {
                entry.get("fact_class").and_then(Value::as_str)
                    == Some(plan.basis_fact.fact_class.as_str())
            })
        })
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_basis_registry_entry_missing",
                "closed registry lacks selected fact class",
            )
        })?;
    let fact_node_id = format!("{profile_id}-registered-fact");
    let validity_interval_sha256 =
        raw_sha256(plan.basis_fact.validity_interval_utf8.as_bytes());
    let mut fact_node = serde_json::json!({
        "node_id": fact_node_id,
        "node_kind": "registered_fact",
        "dependency_node_ids": [],
        "fact_class": plan.basis_fact.fact_class.as_str(),
        "fact_schema_id": plan.basis_fact.fact_schema_id,
        "fact_schema_sha256": selected_entry["fact_schema_sha256"],
        "fact_class_registry_entry_sha256": selected_entry["entry_sha256"],
        "fact_content_sha256": raw_sha256(&fact_bytes),
        "fact_value": plan.bound,
        "units": plan.units.as_str(),
        "comparison": plan.comparison.as_str(),
        "scope": plan.scope.as_str(),
        "enforcement": plan.enforcement.as_str(),
        "producer_identity": plan.basis_fact.producer_identity,
        "evidence_artifact_sha256": evidence_leaf.raw_sha256,
        "fact_validator_sha256": evidence_tool_sha256,
        "validity_interval_sha256": validity_interval_sha256,
        "registration_epoch": "seed",
        "node_sha256": Sha256Digest::ZERO,
    });
    let fact_node_sha256 = self_digest(
        DomainTag::IndependentBasisDerivationNode,
        &fact_node,
        "node_sha256",
    )?;
    fact_node["node_sha256"] = Value::String(fact_node_sha256.to_string());
    let derivation_id = format!("{profile_id}-basis-derivation");
    let mut derivation = serde_json::json!({
        "schema": "trellis-independent-basis-derivation/v1",
        "derivation_id": derivation_id,
        "binder": plan.binder,
        "binder_type": plan.binder_type,
        "measure_id": plan.measure_id,
        "measure_definition_sha256": measure_sha256,
        "fact_class_registry_id": registry_id,
        "fact_class_registry_sha256": fact_registry_record.digest(),
        "units": plan.units.as_str(),
        "comparison": plan.comparison.as_str(),
        "scope": plan.scope.as_str(),
        "enforcement": plan.enforcement.as_str(),
        "derived_limit": plan.bound,
        "root_node_id": fact_node_id,
        "nodes": [fact_node],
        "registration_epoch": "seed",
        "registration_predecessor_head_sha256": genesis,
        "derivation_sha256": Sha256Digest::ZERO,
    });
    let derivation_record = seal_record(
        registry,
        DomainTag::IndependentBasisDerivation,
        &mut derivation,
        "derivation_sha256",
    )?;
    push_definition(
        identities,
        bodies,
        "independent_basis_derivation",
        &derivation_id,
        "trellis://schemas/independent-basis-derivation/v1",
        derivation_record.digest(),
        DomainTag::IndependentBasisDerivation,
        derivation,
    );

    let basis_id = format!("{profile_id}-independent-basis");
    let mut basis = serde_json::json!({
        "schema": "trellis-independent-basis/v1",
        "basis_id": basis_id,
        "target_id": target.target_id,
        "target_statement_sha256": target_statement_sha256,
        "source_claim_lineage_id": lineage_id,
        "source_claim_lineage_sha256": lineage_record.digest(),
        "validation_contract_id": contract_id,
        "validation_contract_sha256": contract_record.digest(),
        "binder": plan.binder,
        "binder_type": plan.binder_type,
        "measure_id": plan.measure_id,
        "measure_definition_sha256": measure_sha256,
        "units": plan.units.as_str(),
        "comparison": plan.comparison.as_str(),
        "limit": plan.bound,
        "scope": plan.scope.as_str(),
        "enforcement": plan.enforcement.as_str(),
        "basis_derivation_id": derivation_id,
        "basis_derivation_sha256": derivation_record.digest(),
        "validity_interval_sha256": validity_interval_sha256,
        "registration_epoch": "seed",
        "registration_predecessor_head_sha256": genesis,
        "basis_definition_sha256": Sha256Digest::ZERO,
    });
    let basis_record = seal_record(
        registry,
        DomainTag::IndependentBasis,
        &mut basis,
        "basis_definition_sha256",
    )?;
    push_definition(
        identities,
        bodies,
        "independent_basis",
        &basis_id,
        "trellis://schemas/independent-basis/v1",
        basis_record.digest(),
        DomainTag::IndependentBasis,
        basis,
    );

    let condition = serde_json::json!({
        "kind": "atom",
        "binder": plan.binder,
        "binder_type": plan.binder_type,
        "measure_id": plan.measure_id,
        "units": plan.units.as_str(),
        "comparison": plan.comparison.as_str(),
        "bound": plan.bound,
        "scope": plan.scope.as_str(),
        "enforcement": plan.enforcement.as_str(),
        "independent_basis_id": basis_id,
        "independent_basis_sha256": basis_record.digest(),
    });
    let permitted: Vec<_> = plan
        .permitted_applicability
        .iter()
        .map(|item| item.as_str())
        .collect();
    let mut profile = serde_json::json!({
        "profile_id": profile_id,
        "target_id": target.target_id,
        "target_statement_sha256": target_statement_sha256,
        "source_claim_lineage_id": lineage_id,
        "source_claim_lineage_sha256": lineage_record.digest(),
        "validation_contract_id": contract_id,
        "validation_contract_sha256": contract_record.digest(),
        "conditionalization_schema_id": plan.conditionalization_schema_id,
        "registration_epoch": "seed",
        "condition": condition,
        "measure_closure_sha256": measure_sha256,
        "independent_basis_id": basis_id,
        "independent_basis_sha256": basis_record.digest(),
        "permitted_applicability": permitted,
        "attempt_order": 0,
        "attempt_budget": 1,
        "witness_demand_checker_sha256": evidence_tool_sha256,
        "source_admissibility_checker_sha256": evidence_tool_sha256,
        "conditional_statement_generator_sha256": evidence_tool_sha256,
        "conditional_proof_checker_sha256": evidence_tool_sha256,
        "applicability_validator_sha256": evidence_tool_sha256,
        "profile_definition_sha256": Sha256Digest::ZERO,
    });
    let profile_digest = self_digest(
        DomainTag::QualificationProfile,
        &profile,
        "profile_definition_sha256",
    )?;
    profile["profile_definition_sha256"] = Value::String(profile_digest.to_string());
    let profile_record = AuthoritativeRecord::parse_as(
        registry,
        "trellis-qualification-profile/v1",
        profile.clone(),
    )?;
    push_definition(
        identities,
        bodies,
        "qualification_profile",
        profile_id,
        "trellis://schemas/qualification-profile/v1",
        profile_record.digest(),
        DomainTag::QualificationProfile,
        profile.clone(),
    );

    let condition_sha256 = tagged_hash(
        DomainTag::ConditionalizationSchema,
        &canonical_json_value(&condition)?,
    );
    let mut candidate = serde_json::json!({
        "schema": "trellis-conditional-theorem-candidate/v1",
        "candidate_id": plan.conditional_candidate_id,
        "profile_id": profile_id,
        "profile_definition_sha256": profile_record.digest(),
        "target_id": target.target_id,
        "target_statement_sha256": target_statement_sha256,
        "conditionalization_schema_id": plan.conditionalization_schema_id,
        "condition_sha256": condition_sha256,
        "node_id": plan.conditional_candidate_node_id,
        "statement_normalization": "trellis-find-declaration-v1",
        "statement_utf8": plan.conditional_statement_utf8,
        "conditional_statement_sha256": tagged_hash(
            DomainTag::ConditionalizationSchema,
            plan.conditional_statement_utf8.as_bytes(),
        ),
        "active_statement_sha256": raw_sha256(plan.conditional_statement_utf8.as_bytes()),
        "registration_epoch": "seed",
        "candidate_definition_sha256": Sha256Digest::ZERO,
    });
    let candidate_record = seal_record(
        registry,
        DomainTag::ConditionalTheoremCandidate,
        &mut candidate,
        "candidate_definition_sha256",
    )?;
    push_definition(
        identities,
        bodies,
        "conditional_theorem_candidate",
        &plan.conditional_candidate_id,
        "trellis://schemas/conditional-theorem-candidate/v1",
        candidate_record.digest(),
        DomainTag::ConditionalTheoremCandidate,
        candidate,
    );
    Ok(profile)
}

fn evidence_leaf_digest(
    evidence: &VerifiedEvidenceClosure,
    logical_id: &str,
) -> Result<Sha256Digest, TrustError> {
    evidence
        .leaves_by_logical_id
        .get(logical_id)
        .map(|leaf| leaf.raw_sha256)
        .filter(|digest| *digest != Sha256Digest::ZERO)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_required_evidence_missing",
                format!("approved evidence lacks required logical ID {logical_id}"),
            )
        })
}

#[cfg(target_os = "linux")]
fn validate_captured_host_fact_for_seed(
    fact: &Value,
    capture_tool_sha256: Sha256Digest,
) -> Result<(), TrustError> {
    let cpuinfo = fs::read("/proc/cpuinfo").map_err(|error| {
        TrustError::new(
            "bootstrap_host_fact_capture_unavailable",
            format!("cannot read /proc/cpuinfo: {error}"),
        )
    })?;
    validate_captured_host_fact_against(fact, capture_tool_sha256, &cpuinfo)
}

#[cfg(not(target_os = "linux"))]
fn validate_captured_host_fact_for_seed(
    _fact: &Value,
    _capture_tool_sha256: Sha256Digest,
) -> Result<(), TrustError> {
    Err(TrustError::new(
        "bootstrap_host_fact_capture_unavailable",
        "host-capability basis validation is Linux-only and requires /proc/cpuinfo",
    ))
}

fn validate_captured_host_fact_against(
    fact: &Value,
    capture_tool_sha256: Sha256Digest,
    cpuinfo: &[u8],
) -> Result<(), TrustError> {
    let (physical_bits, virtual_bits, raw_lines) = parse_host_address_lines(cpuinfo)?;
    let bound = host_power_of_two_decimal(virtual_bits)?;
    let address_size_attestation = serde_json::json!({
        "schema": "trellis-proc-cpuinfo-address-size-attestation/v1",
        "architecture": std::env::consts::ARCH,
        "processor_count": raw_lines.len(),
        "raw_address_size_lines": raw_lines,
    });
    let attestation_sha256 = tagged_hash(
        DomainTag::RawArtifact,
        &canonical_json_value(&address_size_attestation)?,
    );
    let receipt = serde_json::json!({
        "schema": "trellis-host-address-width-capture-receipt/v1",
        "architecture": std::env::consts::ARCH,
        "processor_count": raw_lines.len(),
        "physical_address_bits": physical_bits,
        "virtual_address_bits": virtual_bits,
        "addressable_bytes_upper_bound": bound,
        "capture_tool_sha256": capture_tool_sha256,
        "address_size_attestation_sha256": attestation_sha256,
        "raw_address_size_lines": raw_lines,
    });
    let capture_receipt_sha256 = tagged_hash(
        DomainTag::RawArtifact,
        &canonical_json_value(&receipt)?,
    );
    let expected = serde_json::json!({
        "schema": "trellis-captured-host-address-width-fact/v1",
        "fact_id": "captured-host-address-width-v1",
        "architecture": std::env::consts::ARCH,
        "physical_address_bits": physical_bits,
        "virtual_address_bits": virtual_bits,
        "addressable_bytes_upper_bound": bound,
        "capture_tool_sha256": capture_tool_sha256,
        "capture_receipt_sha256": capture_receipt_sha256,
        "attestation_sha256": attestation_sha256,
    });
    if *fact != expected {
        return Err(TrustError::new(
            "bootstrap_host_fact_reproduction_failed",
            "host fact does not reproduce from the pinned kernel and every live processor address-size line",
        ));
    }
    Ok(())
}

fn parse_host_address_lines(
    cpuinfo: &[u8],
) -> Result<(u64, u64, Vec<String>), TrustError> {
    let text = std::str::from_utf8(cpuinfo).map_err(|_| {
        TrustError::new(
            "bootstrap_host_fact_capture_invalid",
            "/proc/cpuinfo is not UTF-8",
        )
    })?;
    if text.contains('\r') {
        return Err(TrustError::new(
            "bootstrap_host_fact_capture_invalid",
            "/proc/cpuinfo contains unsupported carriage returns",
        ));
    }
    let mut expected = None;
    let mut raw_lines = Vec::new();
    for stanza in text.split("\n\n").filter(|stanza| !stanza.trim().is_empty()) {
        let lines: Vec<_> = stanza.lines().collect();
        if !lines.iter().any(|line| {
            line.split_once(':')
                .is_some_and(|(key, value)| key.trim() == "processor" && !value.trim().is_empty())
        }) {
            continue;
        }
        let address_lines: Vec<_> = lines
            .iter()
            .filter(|line| {
                line.split_once(':')
                    .is_some_and(|(key, _)| key.trim() == "address sizes")
            })
            .copied()
            .collect();
        let [line] = address_lines.as_slice() else {
            return Err(TrustError::new(
                "bootstrap_host_fact_capture_invalid",
                "every processor must have exactly one address sizes line",
            ));
        };
        let (key, widths) = line.split_once(':').ok_or_else(|| {
            TrustError::new(
                "bootstrap_host_fact_capture_invalid",
                "address sizes line lacks its colon",
            )
        })?;
        if key.trim() != "address sizes" {
            return Err(TrustError::new(
                "bootstrap_host_fact_capture_invalid",
                "address sizes line has the wrong key",
            ));
        }
        let (physical, virtual_bits) = widths
            .trim()
            .split_once(" bits physical, ")
            .ok_or_else(|| {
                TrustError::new(
                    "bootstrap_host_fact_capture_invalid",
                    "address sizes line has the wrong physical-width syntax",
                )
            })?;
        let virtual_bits = virtual_bits.strip_suffix(" bits virtual").ok_or_else(|| {
            TrustError::new(
                "bootstrap_host_fact_capture_invalid",
                "address sizes line has the wrong virtual-width syntax",
            )
        })?;
        if physical.is_empty()
            || virtual_bits.is_empty()
            || !physical.bytes().all(|byte| byte.is_ascii_digit())
            || !virtual_bits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(TrustError::new(
                "bootstrap_host_fact_capture_invalid",
                "address sizes must be decimal integers",
            ));
        }
        let pair = (
            physical.parse::<u64>().map_err(|_| {
                TrustError::new(
                    "bootstrap_host_fact_capture_invalid",
                    "physical address width is out of range",
                )
            })?,
            virtual_bits.parse::<u64>().map_err(|_| {
                TrustError::new(
                    "bootstrap_host_fact_capture_invalid",
                    "virtual address width is out of range",
                )
            })?,
        );
        if expected.is_some_and(|prior| prior != pair) {
            return Err(TrustError::new(
                "bootstrap_host_fact_capture_invalid",
                "processor address sizes are inconsistent",
            ));
        }
        expected = Some(pair);
        raw_lines.push(line.trim().to_owned());
    }
    let Some((physical_bits, virtual_bits)) = expected else {
        return Err(TrustError::new(
            "bootstrap_host_fact_capture_invalid",
            "/proc/cpuinfo contains no processor address sizes",
        ));
    };
    if physical_bits == 0 || physical_bits > virtual_bits || virtual_bits > 128 {
        return Err(TrustError::new(
            "bootstrap_host_fact_capture_invalid",
            "address widths must satisfy 0 < physical <= virtual <= 128",
        ));
    }
    Ok((physical_bits, virtual_bits, raw_lines))
}

fn host_power_of_two_decimal(exponent: u64) -> Result<String, TrustError> {
    if exponent > 128 {
        return Err(TrustError::new(
            "bootstrap_host_fact_capture_invalid",
            "address width exceeds the v1 128-bit limit",
        ));
    }
    let mut digits = vec![1_u8];
    for _ in 0..exponent {
        let mut carry = 0_u8;
        for digit in &mut digits {
            let next = *digit * 2 + carry;
            *digit = next % 10;
            carry = next / 10;
        }
        if carry != 0 {
            digits.push(carry);
        }
    }
    Ok(digits
        .into_iter()
        .rev()
        .map(|digit| char::from(b'0' + digit))
        .collect())
}

fn push_plain_definition(
    identities: &mut Vec<Value>,
    bodies: &mut Vec<Value>,
    kind: &str,
    id: &str,
    schema_id: &str,
    tag: DomainTag,
    body: Value,
) -> Result<Sha256Digest, TrustError> {
    let digest = tagged_hash(tag, &canonical_json_value(&body)?);
    push_definition(
        identities,
        bodies,
        kind,
        id,
        schema_id,
        digest,
        tag,
        body,
    );
    Ok(digest)
}

fn seal_record(
    registry: &SchemaRegistry,
    tag: DomainTag,
    value: &mut Value,
    digest_field: &str,
) -> Result<AuthoritativeRecord, TrustError> {
    let digest = self_digest(tag, value, digest_field)?;
    value[digest_field] = Value::String(digest.to_string());
    AuthoritativeRecord::parse(registry, value.clone())
}

#[allow(clippy::too_many_arguments)]
fn render_campaign_gate_presentation(
    request: &CampaignSeedRequest<'_>,
    seed_digest: Sha256Digest,
    bundle_digest: Sha256Digest,
    authored_root: Sha256Digest,
) -> String {
    let mut output = format!(
        "Trellis trust-v1 advance gate\n\nJournal: {}\nRun: {}\nSeed plan: {}\nSeed manifest: {seed_digest}\nSeed definition bundle: {bundle_digest}\nAuthored semantic root: {authored_root}\nEvidence/tool root: {}\nSource tree: {}\n",
        request.journal_id,
        request.run_id,
        request.seed_plan_id,
        request.evidence.evidence_tool_input_root,
        request.source_tree_sha256,
    );
    output.push_str("\nPinned pre-gate inputs\n");
    for (label, logical_id) in [
        ("campaign config", "campaign-config"),
        ("campaign trust plan", "campaign-trust-plan"),
        ("trust kernel", "trellis-trust-kernel"),
        ("evidence/semantic tool", "campaign-evidence-tool"),
        ("Lean toolchain", "lean-toolchain"),
        ("Lake package definition", "lakefile"),
        ("Lake dependency manifest", "lake-manifest"),
        ("approved axiom policy", "approved-axioms"),
        ("actual Lean checker executable", "lean-checker-executable"),
        ("actual Lake driver executable", "lake-driver-executable"),
        ("Elan toolchain proxy executable", "elan-toolchain-proxy-executable"),
        ("local-closure checker source", "local-closure-checker-script"),
        (
            "campaign evidence-tool reproducible build receipt",
            "campaign-evidence-tool-build-receipt",
        ),
        ("trusted-platform boundary", "trusted-platform-boundary-v1"),
        ("Charon extraction executable", "extraction-tool:charon"),
        ("Aeneas extraction executable", "extraction-tool:aeneas"),
        (
            "Cargo kernel-gate build executable",
            "extraction-tool:cargo-build-kernel-example",
        ),
        (
            "FILESPEC gate executable",
            "extraction-tool:kernel-scan-tablet-filespec",
        ),
        (
            "prescribed-region gate executable",
            "extraction-tool:kernel-print-prescribed-region",
        ),
        ("extraction generator script", "extraction-generator-script"),
        ("extraction command and closed environment", "extraction-command"),
        ("extraction result/log closure", "extraction-result"),
        ("extraction A/B determinism receipt", "extraction-determinism"),
    ] {
        match request.evidence.leaves_by_logical_id.get(logical_id) {
            Some(leaf) => output.push_str(&format!(
                "- {label}: logical_id={logical_id}; raw_sha256={}; path={}\n",
                leaf.raw_sha256, leaf.relative_path
            )),
            None => output.push_str(&format!(
                "- {label}: logical_id={logical_id}; NOT PRESENT (approval must be refused)\n"
            )),
        }
    }
    output.push_str("\nExplicit trusted-platform boundary (canonical JSON)\n");
    match canonical_json_value(request.trusted_platform_boundary) {
        Ok(bytes) => output.push_str(&format!("{}\n", String::from_utf8_lossy(&bytes))),
        Err(error) => output.push_str(&format!(
            "NOT CANONICAL ({error}); approval must be refused\n"
        )),
    }
    output.push_str("\nExtraction result and A/B determinism receipts (canonical JSON)\n");
    for (label, value) in [
        ("RESULT.json", request.extraction_result),
        ("DETERMINISM.json", request.extraction_determinism),
    ] {
        match canonical_json_value(value) {
            Ok(bytes) => output.push_str(&format!(
                "- {label}: {}\n",
                String::from_utf8_lossy(&bytes)
            )),
            Err(error) => output.push_str(&format!(
                "- {label}: NOT CANONICAL ({error}); approval must be refused\n"
            )),
        }
    }

    output.push_str("\nSource-validation and qualification inventory\n");
    let plan_by_target: BTreeMap<_, _> = request
        .trust_plan
        .targets
        .iter()
        .map(|target| (target.target_id.as_str(), target))
        .collect();
    let mut targets: Vec<_> = request.targets.iter().collect();
    targets.sort_by(|left, right| left.target_id.as_bytes().cmp(right.target_id.as_bytes()));
    for target in targets {
        let statement = normalize_target_declaration(&target.lean_declaration)
            .unwrap_or_else(|_| target.lean_declaration.clone());
        output.push_str(&format!(
            "\nTarget {} (node {})\n- model statement: {}\n- model statement raw_sha256: {}\n",
            gate_string(&target.target_id),
            gate_string(&target.node_id),
            gate_string(&statement),
            raw_sha256(statement.as_bytes()),
        ));
        let target_plan = plan_by_target
            .get(target.target_id.as_str())
            .expect("trust-plan coverage was validated");
        match &target_plan.source_validation {
            TargetSourceValidationPlan::NotDefinedForClaimShapeV1 {
                claim_shape,
                source_counterevidence_class,
                reason,
            } => {
                output.push_str(&format!(
                    "- validation method: not_defined_for_claim_shape_v1\n- unsupported claim shape: {}\n- source counterevidence class: {}\n- qualification: prohibited\n- reason: {}\n",
                    claim_shape.as_str(),
                    source_counterevidence_class.as_str(),
                    gate_string(reason),
                ));
            }
            TargetSourceValidationPlan::ExactRustExecutionV1 {
                public_entry_point,
                rust_target_statement_utf8,
                source_claim_interpretation_utf8,
                binder_domain_schema_utf8,
                preconditions,
                entry_point_type_utf8,
                environment_contract_utf8,
                concretization_schema_utf8,
                erasure_relation_utf8,
                raw_observation_schema_utf8,
                negative_evidence_soundness_statement_utf8,
                qualification,
            } => {
                output.push_str("- validation method: exact_rust_execution_v1\n");
                output.push_str(&format!(
                    "- checked generated call-path/boundary-integration closure: {}\n",
                    target.model_refinement_sha256
                ));
                output.push_str(
                    "- Rust→Aeneas execution simulation assurance: explicit semantic trust item\n\
- explicit trust item: trellis-rust-charon-aeneas-simulation-boundary-v1\n\
- scope: the bridge itself is trusted; generated Lean compilation, the exact reachable call path, and absence of reachable opaque boundary stubs are mechanically checked\n",
                );
                for (label, value) in [
                    ("public entry point", public_entry_point.as_str()),
                    ("Rust target statement", rust_target_statement_utf8.as_str()),
                    ("source claim interpretation", source_claim_interpretation_utf8.as_str()),
                    ("binder-domain schema", binder_domain_schema_utf8.as_str()),
                    ("entry-point type", entry_point_type_utf8.as_str()),
                    ("environment contract", environment_contract_utf8.as_str()),
                    ("concretization schema", concretization_schema_utf8.as_str()),
                    ("erasure relation", erasure_relation_utf8.as_str()),
                    ("raw observation schema", raw_observation_schema_utf8.as_str()),
                    (
                        "negative-evidence soundness statement",
                        negative_evidence_soundness_statement_utf8.as_str(),
                    ),
                ] {
                    output.push_str(&format!(
                        "- {label}: {}\n- {label} raw_sha256: {}\n",
                        gate_string(value),
                        raw_sha256(value.as_bytes()),
                    ));
                }
                output.push_str("- exact preconditions:\n");
                for precondition in preconditions {
                    output.push_str(&format!(
                        "  - id={}; statement={}; raw_sha256={}\n",
                        gate_string(&precondition.precondition_id),
                        gate_string(&precondition.normalized_statement_utf8),
                        raw_sha256(precondition.normalized_statement_utf8.as_bytes()),
                    ));
                }
                match qualification {
                    None => output.push_str("- qualification: prohibited\n"),
                    Some(profile) => {
                        let evidence_digest = request
                            .evidence
                            .leaves_by_logical_id
                            .get(&profile.basis_fact.evidence_logical_id)
                            .map(|leaf| leaf.raw_sha256.to_string())
                            .unwrap_or_else(|| "NOT PRESENT".to_owned());
                        let permitted = profile
                            .permitted_applicability
                            .iter()
                            .map(|item| item.as_str())
                            .collect::<Vec<_>>()
                            .join(",");
                        output.push_str(&format!(
                            "- qualification profile: {}\n- bound: measure={} comparison={} value={} units={}\n- scope/enforcement: {}/{}\n- basis: class={} logical_id={} raw_evidence_sha256={} producer={} validity={}\n- permitted applicability: [{}]\n- conditional candidate: id={} node={}\n- conditional statement: {}\n- conditional statement raw_sha256: {}\n",
                            gate_string(&profile.profile_id),
                            gate_string(&profile.measure_id),
                            profile.comparison.as_str(),
                            profile.bound,
                            profile.units.as_str(),
                            profile.scope.as_str(),
                            profile.enforcement.as_str(),
                            profile.basis_fact.fact_class.as_str(),
                            gate_string(&profile.basis_fact.evidence_logical_id),
                            evidence_digest,
                            gate_string(&profile.basis_fact.producer_identity),
                            gate_string(&profile.basis_fact.validity_interval_utf8),
                            permitted,
                            gate_string(&profile.conditional_candidate_id),
                            gate_string(&profile.conditional_candidate_node_id),
                            gate_string(&profile.conditional_statement_utf8),
                            raw_sha256(profile.conditional_statement_utf8.as_bytes()),
                        ));
                    }
                }
            }
        }
    }
    output.push_str(
        "\nApproval authorizes exactly the displayed methods, texts, bounds, evidence bytes, candidate statements, and closure roots. It does not assert that any theorem is true, that the adapted source equals upstream Rust, or that extracted-model truth applies outside the explicitly registered source-validation and qualification scope.\n",
    );
    output
}

fn gate_string(value: &str) -> String {
    serde_json::to_string(value).expect("a Rust string is always JSON serializable")
}

pub fn validate_aeneas_model_refinement(
    manifest: &Value,
    public_entry_point: &str,
    tablet_root: &Path,
    tcb_manifest: &Value,
) -> Result<Sha256Digest, TrustError> {
    if manifest.get("schema").and_then(Value::as_str)
        != Some("trellis-aeneas-model-refinement/v1")
        || manifest.get("model_semantics").and_then(Value::as_str)
            != Some("trellis-aeneas-boundary-definitions/v1")
    {
        return Err(TrustError::new(
            "bootstrap_model_refinement_schema_invalid",
            "generated model refinement schema or semantics is not the closed v1 value",
        ));
    }
    let embedded: Sha256Digest = manifest
        .get("manifest_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_digest_missing",
                "model refinement manifest lacks manifest_sha256",
            )
        })?
        .parse()?;
    let mut unhashed = manifest.clone();
    unhashed
        .as_object_mut()
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_not_object",
                "model refinement manifest must be an object",
            )
        })?
        .remove("manifest_sha256");
    if raw_sha256(&canonical_json_value(&unhashed)?) != embedded {
        return Err(TrustError::new(
            "bootstrap_model_refinement_digest_mismatch",
            "model refinement manifest digest does not match its exact canonical content",
        ));
    }

    let definition_values = manifest
        .get("boundary_definitions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_definitions_missing",
                "model refinement manifest lacks boundary_definitions",
            )
        })?;
    let mut definitions = BTreeSet::new();
    for definition in definition_values {
        let name = definition
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TrustError::new(
                    "bootstrap_model_refinement_definition_invalid",
                    "boundary definition lacks a name",
                )
            })?;
        if definition.get("classification").and_then(Value::as_str)
            != Some("reviewed-executable-boundary-definition")
            || definition.get("model_semantics").and_then(Value::as_str)
                != Some("trellis-aeneas-boundary-definitions/v1")
            || definition
                .get("definition_sha256")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<Sha256Digest>().ok())
                .is_none()
            || !definitions.insert(name.to_owned())
        {
            return Err(TrustError::new(
                "bootstrap_model_refinement_definition_invalid",
                "boundary definitions must be unique, hash-bound reviewed v1 definitions",
            ));
        }
        let expected_instantiations: BTreeSet<String> = match name {
            "core.option.Option.Insts.CoreOpsTry_traitTry.branch" => {
                ["Option::<i64>::branch".to_owned()]
                    .into_iter()
                    .collect()
            }
            "core.option.Option.Insts.CoreOpsTry_traitFromResidualOptionInfallible.from_residual" => [
                "Option::<(dec2flt_full_integer::Number, usize)>::from_residual(Option<core::convert::Infallible>)".to_owned(),
            ]
            .into_iter()
            .collect(),
            "core.slice.Slice.first" => ["Slice.first::<u8>".to_owned()]
                .into_iter()
                .collect(),
            "core.slice.Slice.split_first" => {
                ["Slice.split_first::<u8>".to_owned()]
                    .into_iter()
                    .collect()
            }
            _ => {
                return Err(TrustError::new(
                    "bootstrap_model_refinement_definition_unregistered",
                    format!("boundary definition {name} is not registered in v1"),
                ))
            }
        };
        if string_array_field(definition, "approved_rust_instantiations")?
            != expected_instantiations
        {
            return Err(TrustError::new(
                "bootstrap_model_refinement_instantiation_scope_mismatch",
                format!(
                    "boundary definition {name} does not carry its exact v1 Rust instantiation scope"
                ),
            ));
        }
    }
    let unresolved_values = manifest
        .get("unresolved_boundary_axioms")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_axioms_missing",
                "model refinement manifest lacks unresolved_boundary_axioms",
            )
        })?;
    let mut unresolved = BTreeSet::new();
    for axiom in unresolved_values {
        let name = axiom.get("name").and_then(Value::as_str).ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_axiom_invalid",
                "unresolved boundary axiom lacks a name",
            )
        })?;
        if !unresolved.insert(name.to_owned()) || definitions.contains(name) {
            return Err(TrustError::new(
                "bootstrap_model_refinement_boundary_overlap",
                "boundary names must be unique and cannot be both defined and opaque",
            ));
        }
    }

    let validity_values = manifest
        .get("validity_definitions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_validity_missing",
                "model refinement manifest lacks validity_definitions",
            )
        })?;
    if validity_values.len() != 1
        || validity_values[0].get("name").and_then(Value::as_str)
            != Some("RustValidSliceU8")
        || validity_values[0]
            .get("definition_sha256")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<Sha256Digest>().ok())
            .is_none()
    {
        return Err(TrustError::new(
            "bootstrap_model_refinement_validity_invalid",
            "the closed v1 refinement requires one hash-bound RustValidSliceU8 definition",
        ));
    }

    let nodes = manifest.get("nodes").and_then(Value::as_array).ok_or_else(|| {
        TrustError::new(
            "bootstrap_model_refinement_nodes_missing",
            "model refinement manifest lacks its generated node graph",
        )
    })?;
    let mut by_node = BTreeMap::new();
    for node in nodes {
        let node_id = node.get("node_id").and_then(Value::as_str).ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_node_invalid",
                "model refinement graph node lacks node_id",
            )
        })?;
        if by_node.insert(node_id.to_owned(), node).is_some() {
            return Err(TrustError::new(
                "bootstrap_model_refinement_node_duplicate",
                format!("model refinement graph repeats {node_id}"),
            ));
        }
    }
    if !by_node.contains_key(public_entry_point) {
        return Err(TrustError::new(
            "bootstrap_model_refinement_entry_missing",
            format!("generated model graph lacks entry point {public_entry_point}"),
        ));
    }

    let mut pending = vec![public_entry_point.to_owned()];
    let mut reachable = BTreeSet::new();
    while let Some(node_id) = pending.pop() {
        if !reachable.insert(node_id.clone()) {
            continue;
        }
        let node = by_node.get(&node_id).ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_dependency_missing",
                format!("model refinement dependency {node_id} is absent"),
            )
        })?;
        let source = read_regular_model_file(tablet_root, &format!("{node_id}.lean"))?;
        let source = std::str::from_utf8(&source).map_err(|_| {
            TrustError::new(
                "bootstrap_model_refinement_source_not_utf8",
                format!("Tablet/{node_id}.lean is not UTF-8"),
            )
        })?;
        let declared_dependencies = string_array_field(node, "dependency_node_ids")?;
        let declared_defined_calls = string_array_field(node, "reviewed_boundary_calls")?;
        let declared_opaque_calls = string_array_field(node, "unresolved_boundary_calls")?;
        let actual_dependencies: BTreeSet<_> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("import Tablet."))
            .filter(|dependency| by_node.contains_key(*dependency))
            .map(str::to_owned)
            .collect();
        let actual_defined_calls: BTreeSet<_> = definitions
            .iter()
            .filter(|name| source.contains(name.as_str()))
            .cloned()
            .collect();
        let actual_opaque_calls: BTreeSet<_> = unresolved
            .iter()
            .filter(|name| source.contains(name.as_str()))
            .cloned()
            .collect();
        if declared_dependencies != actual_dependencies
            || declared_defined_calls != actual_defined_calls
            || declared_opaque_calls != actual_opaque_calls
        {
            return Err(TrustError::new(
                "bootstrap_model_refinement_call_graph_mismatch",
                format!("generated call graph differs from Tablet/{node_id}.lean"),
            ));
        }
        if !actual_opaque_calls.is_empty() {
            return Err(TrustError::new(
                "bootstrap_model_refinement_opaque_reachable",
                format!(
                    "entry point {public_entry_point} reaches opaque externals through {node_id}: {actual_opaque_calls:?}"
                ),
            ));
        }
        pending.extend(actual_dependencies);
    }

    let preamble = read_regular_model_file(tablet_root, "Preamble.lean")?;
    let assumptions = read_regular_model_file(tablet_root, "Assumptions.lean")?;
    let flattened_preamble = String::from_utf8(preamble.clone())
        .map_err(|_| {
            TrustError::new(
                "bootstrap_model_refinement_preamble_not_utf8",
                "Tablet/Preamble.lean is not UTF-8",
            )
        })?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for name in &definitions {
        if !flattened_preamble.contains(&format!("def {name}"))
            || flattened_preamble.contains(&format!("axiom {name}"))
        {
            return Err(TrustError::new(
                "bootstrap_model_refinement_preamble_mismatch",
                format!("Preamble does not install reviewed definition {name}"),
            ));
        }
    }
    let flattened_assumptions = String::from_utf8(assumptions.clone())
        .map_err(|_| {
            TrustError::new(
                "bootstrap_model_refinement_assumptions_not_utf8",
                "Tablet/Assumptions.lean is not UTF-8",
            )
        })?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !flattened_assumptions.contains("def RustValidSliceU8")
        || flattened_assumptions.contains("axiom RustValidSliceU8")
    {
        return Err(TrustError::new(
            "bootstrap_model_refinement_validity_not_defined",
            "Assumptions.lean must define, not axiomatize, RustValidSliceU8",
        ));
    }
    if tcb_manifest.get("boundary_definitions") != manifest.get("boundary_definitions") {
        return Err(TrustError::new(
            "bootstrap_model_refinement_tcb_mismatch",
            "TCB manifest boundary definitions differ from the model refinement manifest",
        ));
    }
    let approved_axioms: BTreeSet<_> = tcb_manifest
        .get("global")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_tcb_invalid",
                "TCB manifest global allowlist is missing",
            )
        })?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if approved_axioms.contains("RustValidSliceU8")
        || definitions
            .iter()
            .any(|name| approved_axioms.contains(name.as_str()))
    {
        return Err(TrustError::new(
            "bootstrap_model_refinement_definition_in_tcb",
            "defined model boundaries cannot remain in the approved axiom allowlist",
        ));
    }

    let closure = serde_json::json!({
        "schema": "trellis-aeneas-model-refinement-closure/v1",
        "manifest_sha256": embedded,
        "public_entry_point": public_entry_point,
        "reachable_node_ids": reachable,
        "preamble_sha256": raw_sha256(&preamble),
        "assumptions_sha256": raw_sha256(&assumptions),
        "tcb_manifest_sha256": raw_sha256(&canonical_json_value(tcb_manifest)?),
    });
    Ok(tagged_hash(
        DomainTag::CarrierRefinementDefinition,
        &canonical_json_value(&closure)?,
    ))
}

fn string_array_field(value: &Value, field: &str) -> Result<BTreeSet<String>, TrustError> {
    let values = value.get(field).and_then(Value::as_array).ok_or_else(|| {
        TrustError::new(
            "bootstrap_model_refinement_array_missing",
            format!("model refinement node lacks {field}"),
        )
    })?;
    let mut output = BTreeSet::new();
    for item in values {
        let item = item.as_str().ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_array_invalid",
                format!("model refinement {field} entries must be strings"),
            )
        })?;
        if !output.insert(item.to_owned()) {
            return Err(TrustError::new(
                "bootstrap_model_refinement_array_duplicate",
                format!("model refinement {field} repeats {item}"),
            ));
        }
    }
    Ok(output)
}

fn read_regular_model_file(root: &Path, relative: &str) -> Result<Vec<u8>, TrustError> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        TrustError::new(
            "bootstrap_model_refinement_file_missing",
            format!("{}: {error}", path.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(TrustError::new(
            "bootstrap_model_refinement_file_invalid",
            format!("{} must be a non-symlink regular file", path.display()),
        ));
    }
    fs::read(&path).map_err(|error| {
        TrustError::new(
            "bootstrap_model_refinement_file_unreadable",
            format!("{}: {error}", path.display()),
        )
    })
}

pub fn source_tree_digest(root: &Path) -> Result<Sha256Digest, TrustError> {
    let mut entries = Vec::new();
    for (relative, bytes) in regular_files(root)? {
        entries.push(serde_json::json!({
            "relative_path": relative,
            "byte_length": bytes.len(),
            "sha256_of_raw_bytes": raw_sha256(&bytes),
        }));
    }
    if entries.is_empty() {
        return Err(TrustError::new(
            "bootstrap_source_tree_empty",
            "source tree contains no regular files",
        ));
    }
    Ok(tagged_hash(
        DomainTag::ManifestNode,
        &canonical_json_value(&Value::Array(entries))?,
    ))
}

fn push_definition(
    identities: &mut Vec<Value>,
    bodies: &mut Vec<Value>,
    kind: &str,
    id: &str,
    schema_id: &str,
    digest: Sha256Digest,
    tag: DomainTag,
    body: Value,
) {
    let identity = serde_json::json!({
        "record_kind": kind,
        "record_id": id,
        "record_schema_id": schema_id,
        "record_sha256": digest,
        "domain_tag": tag.as_str(),
    });
    let mut bundled = identity.clone();
    bundled["canonical_value"] = body;
    identities.push(identity);
    bodies.push(bundled);
}

fn seed_rank(kind: &str) -> u8 {
    match kind {
        "source_claim_lineage" => 1,
        "source_validation_contract" => 2,
        "measure_catalog" => 3,
        "independent_basis_derivation" => 4,
        "independent_basis" => 5,
        "qualification_profile" => 6,
        "conditional_theorem_candidate" => 7,
        "qualification_profile_catalog" => 8,
        _ => 0,
    }
}

fn normalize_target_declaration(value: &str) -> Result<String, TrustError> {
    let mut lines = Vec::new();
    for line in value.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() && lines.is_empty() {
            continue;
        }
        lines.push(trimmed.to_owned());
        if line.contains(":=") {
            return Ok(lines.join(" "));
        }
    }
    Err(TrustError::new(
        "bootstrap_target_declaration_invalid",
        "target declaration does not contain its := proof boundary",
    ))
}

fn render_gate_presentation(
    request: ConservativeSeedRequest<'_>,
    seed_digest: Sha256Digest,
    bundle_digest: Sha256Digest,
    authored_root: Sha256Digest,
) -> String {
    let mut output = format!(
        "Trellis trust-v1 advance gate\n\nJournal: {}\nRun: {}\nSeed manifest: {}\nSeed definition bundle: {}\nAuthored semantic root: {}\nEvidence/tool root: {}\nSource tree: {}\n\nSource-validation inventory\n",
        request.journal_id,
        request.run_id,
        seed_digest,
        bundle_digest,
        authored_root,
        request.evidence_tool_input_root,
        request.source_tree_sha256,
    );
    let mut targets: Vec<_> = request.targets.iter().collect();
    targets.sort_by(|left, right| left.target_id.as_bytes().cmp(right.target_id.as_bytes()));
    for target in targets {
        output.push_str(&format!(
            "- {}: extracted-model theorem is seed-pinned; Rust validation is NOT DEFINED because no claim-specific checked source oracle/refinement is registered; qualified recovery is prohibited.\n",
            target.target_id
        ));
    }
    output.push_str(
        "\nApproval authorizes exactly these bytes and roots. It does not assert that any theorem is true, that the adapted source equals upstream Rust, or that extracted-model truth applies to deployed uses.\n",
    );
    output
}

fn regular_files(root: &Path) -> Result<Vec<(String, Vec<u8>)>, TrustError> {
    fn walk(
        root: &Path,
        current: &Path,
        output: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<(), TrustError> {
        let mut entries: Vec<_> = fs::read_dir(current)
            .map_err(|error| TrustError::new("bootstrap_tree_unreadable", error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| TrustError::new("bootstrap_tree_unreadable", error.to_string()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| TrustError::new("bootstrap_tree_unreadable", error.to_string()))?;
            if metadata.file_type().is_symlink() {
                return Err(TrustError::new(
                    "bootstrap_tree_symlink_forbidden",
                    format!("{} is a symlink", path.display()),
                ));
            }
            if metadata.is_dir() {
                walk(root, &path, output)?;
            } else if metadata.is_file() {
                let relative = path.strip_prefix(root).map_err(|_| {
                    TrustError::new("bootstrap_tree_escape", "tree member escapes root")
                })?;
                let relative = slash_path(relative)?;
                output.push((
                    relative,
                    fs::read(&path).map_err(|error| {
                        TrustError::new("bootstrap_tree_unreadable", error.to_string())
                    })?,
                ));
            } else {
                return Err(TrustError::new(
                    "bootstrap_tree_special_file",
                    format!("{} is not a regular file or directory", path.display()),
                ));
            }
        }
        Ok(())
    }
    let root_metadata = fs::symlink_metadata(root).map_err(|error| {
        TrustError::new("bootstrap_tree_missing", format!("{}: {error}", root.display()))
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(TrustError::new(
            "bootstrap_tree_missing",
            format!("{} is not a non-symlink directory", root.display()),
        ));
    }
    let mut output = Vec::new();
    walk(root, root, &mut output)?;
    output.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    Ok(output)
}

fn slash_path(path: &Path) -> Result<String, TrustError> {
    let mut pieces = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(piece) = component else {
            return Err(TrustError::new(
                "bootstrap_path_invalid",
                format!("{} is not normalized", path.display()),
            ));
        };
        pieces.push(piece.to_str().ok_or_else(|| {
            TrustError::new("bootstrap_path_not_utf8", "tree path is not UTF-8")
        })?);
    }
    Ok(pieces.join("/"))
}

fn validate_relative_path(value: &str) -> Result<(), TrustError> {
    let path = PathBuf::from(value);
    if value.is_empty()
        || value.contains('\\')
        || value.contains('\0')
        || value.chars().any(char::is_control)
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, std::path::Component::Normal(_))
        })
    {
        return Err(TrustError::new(
            "bootstrap_relative_path_invalid",
            format!("invalid relative path {value:?}"),
        ));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust_base::ManifestAuthorityRoots;

    fn evidence_input(
        logical_id: &str,
        relative_path: &str,
        dependency_ids: &[&str],
    ) -> EvidenceInput {
        EvidenceInput {
            kind: "fixture".into(),
            logical_id: logical_id.into(),
            relative_path: relative_path.into(),
            dependency_ids: dependency_ids
                .iter()
                .map(|dependency| (*dependency).to_owned())
                .collect(),
        }
    }

    #[test]
    fn conservative_seed_is_a_complete_acyclic_verified_closure() {
        let registry = SchemaRegistry::v1().unwrap();
        let root = SigningKey::from_bytes(&[9_u8; 32]);
        let reviewer = SigningKey::from_bytes(&[10_u8; 32]);
        let audit = SigningKey::from_bytes(&[11_u8; 32]);
        let journal = SigningKey::from_bytes(&[12_u8; 32]);
        let actor_value = build_actor_key_manifest(
            "fixture-root",
            &root,
            &[
                ActorPublicKeySpec {
                    key_id: "reviewer".into(),
                    actor_role: "reviewer".into(),
                    actor_identity: "reviewer".into(),
                    purpose: "gate_review".into(),
                    public_key_ed25519_hex: hex(&reviewer.verifying_key().to_bytes()),
                },
                ActorPublicKeySpec {
                    key_id: "audit".into(),
                    actor_role: "audit_authority".into(),
                    actor_identity: "audit".into(),
                    purpose: "audit_authorization".into(),
                    public_key_ed25519_hex: hex(&audit.verifying_key().to_bytes()),
                },
                ActorPublicKeySpec {
                    key_id: "journal".into(),
                    actor_role: "journal_authority".into(),
                    actor_identity: "journal".into(),
                    purpose: "journal_commit".into(),
                    public_key_ed25519_hex: hex(&journal.verifying_key().to_bytes()),
                },
            ],
        )
        .unwrap();
        let mut roots = ManifestAuthorityRoots::default();
        roots
            .insert_hex("fixture-root", &hex(&root.verifying_key().to_bytes()))
            .unwrap();
        let actor = ActorKeyManifest::verify(&registry, actor_value, &roots).unwrap();
        let seed = build_conservative_campaign_seed(ConservativeSeedRequest {
            journal_id: "fixture-journal",
            run_id: "fixture-run",
            seed_plan_id: "fixture-plan",
            actor_key_manifest: &actor,
            evidence_tool_input_root: raw_sha256(b"evidence"),
            source_tree_sha256: raw_sha256(b"source"),
            contract_generator_sha256: raw_sha256(b"kernel"),
            targets: &[CampaignTargetInput {
                target_id: "claim".into(),
                node_id: "claim".into(),
                lean_declaration: "theorem claim :\n  True := by".into(),
                informal: "fixture".into(),
                model_refinement_sha256: Sha256Digest::ZERO,
            }],
        })
        .unwrap();
        verify_seed_definition_bundle(
            &seed.seed_manifest,
            &seed.seed_definition_bundle_bytes,
        )
        .unwrap();
        assert!(String::from_utf8(seed.gate_presentation_bytes)
            .unwrap()
            .contains("Rust validation is NOT DEFINED"));
    }

    fn seal_refinement_manifest(mut value: Value) -> Value {
        value["manifest_sha256"] = Value::String(
            raw_sha256(&canonical_json_value(&value).unwrap()).to_string(),
        );
        value
    }

    #[test]
    fn model_refinement_recomputes_call_paths_and_rejects_reachable_opaque_stubs() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("Preamble.lean"),
            b"def core.slice.Slice.first : True := True.intro\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("Assumptions.lean"),
            b"def RustValidSliceU8 : Prop := True\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("entry.lean"),
            b"def entry : True := by\n  have := core.slice.Slice.first\n  trivial\n",
        )
        .unwrap();
        let definition = serde_json::json!({
            "name": "core.slice.Slice.first",
            "classification": "reviewed-executable-boundary-definition",
            "replaces_stubs": "fixture",
            "definition_sha256": raw_sha256(b"definition"),
            "model_semantics": "trellis-aeneas-boundary-definitions/v1",
            "approved_rust_instantiations": ["Slice.first::<u8>"],
        });
        let tcb = serde_json::json!({
            "boundary_definitions": [definition.clone()],
            "global": [],
        });
        let manifest = seal_refinement_manifest(serde_json::json!({
            "schema": "trellis-aeneas-model-refinement/v1",
            "model_semantics": "trellis-aeneas-boundary-definitions/v1",
            "validity_definitions": [{
                "name": "RustValidSliceU8",
                "definition_sha256": raw_sha256(b"validity"),
            }],
            "boundary_definitions": [definition],
            "unresolved_boundary_axioms": [],
            "nodes": [{
                "node_id": "entry",
                "dependency_node_ids": [],
                "reviewed_boundary_calls": ["core.slice.Slice.first"],
                "unresolved_boundary_calls": [],
            }],
        }));
        assert_ne!(
            validate_aeneas_model_refinement(&manifest, "entry", directory.path(), &tcb)
                .unwrap(),
            Sha256Digest::ZERO
        );
        let mut widened = manifest.clone();
        widened
            .as_object_mut()
            .unwrap()
            .remove("manifest_sha256");
        widened["boundary_definitions"][0]["approved_rust_instantiations"] =
            serde_json::json!(["Slice.first::<u16>"]);
        let widened = seal_refinement_manifest(widened);
        let widened_tcb = serde_json::json!({
            "boundary_definitions": widened["boundary_definitions"].clone(),
            "global": [],
        });
        let error = validate_aeneas_model_refinement(
            &widened,
            "entry",
            directory.path(),
            &widened_tcb,
        )
        .unwrap_err();
        assert_eq!(
            error.code,
            "bootstrap_model_refinement_instantiation_scope_mismatch"
        );

        fs::write(
            directory.path().join("entry.lean"),
            b"def entry : True := by\n  have := opaque.external\n  trivial\n",
        )
        .unwrap();
        let opaque_manifest = seal_refinement_manifest(serde_json::json!({
            "schema": "trellis-aeneas-model-refinement/v1",
            "model_semantics": "trellis-aeneas-boundary-definitions/v1",
            "validity_definitions": [{
                "name": "RustValidSliceU8",
                "definition_sha256": raw_sha256(b"validity"),
            }],
            "boundary_definitions": [],
            "unresolved_boundary_axioms": [{
                "name": "opaque.external",
                "stubs": "True",
                "classification": "aeneas-stdlib-prim",
            }],
            "nodes": [{
                "node_id": "entry",
                "dependency_node_ids": [],
                "reviewed_boundary_calls": [],
                "unresolved_boundary_calls": ["opaque.external"],
            }],
        }));
        let error = validate_aeneas_model_refinement(
            &opaque_manifest,
            "entry",
            directory.path(),
            &serde_json::json!({"boundary_definitions": [], "global": ["opaque.external"]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "bootstrap_model_refinement_opaque_reachable");
    }

    #[test]
    fn evidence_manifest_normalizes_and_verifies_dependency_order() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a"), b"a").unwrap();
        fs::write(directory.path().join("b"), b"b").unwrap();
        fs::write(directory.path().join("c"), b"c").unwrap();
        let (manifest, bytes) = build_evidence_manifest(
            directory.path(),
            &[
                evidence_input("c", "c", &["b", "a", "b"]),
                evidence_input("a", "a", &[]),
                evidence_input("b", "b", &["a"]),
            ],
        )
        .unwrap();

        assert_eq!(manifest["leaves"][2]["logical_id"], "c");
        assert_eq!(manifest["leaves"][2]["dependency_ids"], serde_json::json!(["a", "b"]));
        verify_evidence_tool_manifest(directory.path(), &bytes).unwrap();
    }

    #[test]
    fn evidence_manifest_rejects_missing_and_cyclic_dependencies_before_file_reads() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a"), b"a").unwrap();
        fs::write(directory.path().join("b"), b"b").unwrap();

        let error = build_evidence_manifest(
            directory.path(),
            &[evidence_input("a", "does-not-exist", &["missing"])],
        )
        .unwrap_err();
        assert_eq!(error.code, "bootstrap_evidence_dependency_missing");

        let error = build_evidence_manifest(
            directory.path(),
            &[
                evidence_input("a", "does-not-exist", &["b"]),
                evidence_input("b", "also-does-not-exist", &["a"]),
            ],
        )
        .unwrap_err();
        assert_eq!(error.code, "bootstrap_evidence_dependency_cycle");
    }

    #[cfg(unix)]
    #[test]
    fn evidence_manifest_never_reads_through_a_symlinked_path_component() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"outside").unwrap();
        symlink(outside.path(), directory.path().join("linked")).unwrap();

        let error = build_evidence_manifest(
            directory.path(),
            &[evidence_input("secret", "linked/secret", &[])],
        )
        .unwrap_err();
        assert_eq!(error.code, "bootstrap_evidence_not_regular");
    }

    #[cfg(unix)]
    #[test]
    fn source_tree_digest_rejects_a_symlinked_root() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("lib.rs"), b"pub fn outside() {}\n").unwrap();
        let linked_root = directory.path().join("crate");
        symlink(outside.path(), &linked_root).unwrap();

        let error = source_tree_digest(&linked_root).unwrap_err();
        assert_eq!(error.code, "bootstrap_tree_missing");
    }
}

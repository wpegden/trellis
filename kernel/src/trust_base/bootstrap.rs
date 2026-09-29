//! Production construction of an acyclic trust-v1 campaign seed.
//!
//! Bootstrap is deliberately a constructor, not a second verifier.  Every
//! value emitted here is immediately fed through the same schema, semantic,
//! definition-closure, and evidence-closure validators used by the runtime.

use super::campaign_plan::CampaignTrustPlan;
use super::canonical::{
    canonical_json_value, raw_sha256, self_digest, tagged_hash, DomainTag, Sha256Digest,
    TrustError,
};
use super::closure::{
    seed_adaptation_ledger_projection, verify_evidence_tool_manifest,
    verify_seed_definition_bundle, VerifiedEvidenceClosure, ADAPTATION_LEDGER_ROW_SCHEMA,
};
use super::records::AuthoritativeRecord;
use super::schema::SchemaRegistry;
use crate::phase0::Phase0TrustRoots;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

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
    pub resolution: crate::model::ChallengeResolution,
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
    pub run_id: &'a str,
    pub seed_plan_id: &'a str,
    pub evidence_tool_input_root: Sha256Digest,
    pub source_tree_sha256: Sha256Digest,
    pub contract_generator_sha256: Sha256Digest,
    pub targets: &'a [CampaignTargetInput],
    /// W9: the prose GOAL referent bytes for a prose campaign — embedded
    /// (digest + bytes) as a member of the authoritative seed record so
    /// the launch acknowledgment covers the prose exactly as it covers
    /// mode-B's pinned statements. `None` for mode-B/math seeds.
    pub pv_goal_prose_utf8: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub struct CampaignSeedRequest<'a> {
    pub run_id: &'a str,
    pub seed_plan_id: &'a str,
    pub evidence: &'a VerifiedEvidenceClosure,
    pub extraction_result: &'a Value,
    pub extraction_determinism: &'a Value,
    /// Exact canonical description of the intentionally unarchived platform
    /// surface.  It is an approved evidence leaf and is rendered verbatim at
    /// the sole human gate, rather than hidden behind only a digest.
    pub trusted_platform_boundary: &'a Value,
    pub source_tree_sha256: Sha256Digest,
    pub targets: &'a [CampaignTargetInput],
    pub trust_plan: &'a CampaignTrustPlan,
    pub phase0: &'a Phase0TrustRoots,
    /// W9: the prose GOAL referent bytes (see `ConservativeSeedRequest`).
    pub pv_goal_prose_utf8: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub struct ConstructedSeed {
    pub seed_manifest: AuthoritativeRecord,
    pub seed_manifest_bytes: Vec<u8>,
    pub seed_definition_bundle: Value,
    pub seed_definition_bundle_bytes: Vec<u8>,
    /// Bootstrap-time seed-sight artifact; it is not a verdict.
    pub launch_acknowledgment_bytes: Vec<u8>,
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

/// Build a seed without a plan-backed evidence closure. This remains useful
/// for the generic math-mode bootstrap; prose campaigns use
/// [`build_campaign_seed`] so GOAL-owned resolutions can be checked against
/// the plan before any authoritative bytes are written.
pub fn build_conservative_campaign_seed(
    request: ConservativeSeedRequest<'_>,
) -> Result<ConstructedSeed, TrustError> {
    if request.targets.is_empty() || request.contract_generator_sha256 == Sha256Digest::ZERO {
        return Err(TrustError::new(
            "bootstrap_seed_input_invalid",
            "at least one target and a nonzero generator identity are required",
        ));
    }
    let (identities, bodies) = build_target_definitions(request.targets, None)?;
    finish_seed(
        request.run_id,
        request.seed_plan_id,
        request.evidence_tool_input_root,
        identities,
        bodies,
        request.pv_goal_prose_utf8,
        render_conservative_launch_acknowledgment(&request),
    )
}

/// Build a prose campaign seed from the closed target-resolution plan.
/// Resolution is copied into each target definition only after exact
/// agreement with the independently parsed campaign configuration.
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
    validate_displayed_evidence(&request)?;

    let configured: BTreeMap<_, _> = request
        .targets
        .iter()
        .map(|target| (target.target_id.as_str(), target.resolution))
        .collect();
    let planned: BTreeMap<_, _> = request
        .trust_plan
        .targets
        .iter()
        .map(|target| (target.target_id.as_str(), target.resolution))
        .collect();
    if configured.len() != request.targets.len() || configured != planned {
        return Err(TrustError::new(
            "bootstrap_target_resolution_mismatch",
            "trust plan target IDs and resolutions must exactly match the campaign configuration",
        ));
    }

    let (mut identities, mut bodies) =
        build_target_definitions(request.targets, request.pv_goal_prose_utf8)?;
    push_plain_definition(
        &mut identities,
        &mut bodies,
        "phase0_source_adaptation",
        "phase0-source-adaptation-v1",
        "trellis://campaign/phase0-source-adaptation/v1",
        DomainTag::RawArtifact,
        serde_json::json!({
            "schema": super::closure::PHASE0_SOURCE_ADAPTATION_DEFINITION_SCHEMA,
            "roots": request.phase0,
        }),
    )?;
    for row in &request.trust_plan.adaptation_ledger {
        push_plain_definition(
            &mut identities,
            &mut bodies,
            "adaptation_ledger_row",
            &format!("adaptation-{}", row.id),
            "trellis://campaign/adaptation-ledger-row/v1",
            DomainTag::AdaptationLedgerRow,
            serde_json::json!({
                "schema": ADAPTATION_LEDGER_ROW_SCHEMA,
                "row": serde_json::to_value(row).map_err(|error| {
                    TrustError::new("adaptation_ledger_not_serializable", error.to_string())
                })?,
            }),
        )?;
    }
    let acknowledgment = render_campaign_launch_acknowledgment(&request)?;
    let seed = finish_seed(
        request.run_id,
        request.seed_plan_id,
        request.evidence.evidence_tool_input_root,
        identities,
        bodies,
        request.pv_goal_prose_utf8,
        acknowledgment,
    )?;
    let verified = verify_seed_definition_bundle(
        &seed.seed_manifest,
        &seed.seed_definition_bundle_bytes,
    )?;
    let projected = super::closure::seed_phase0_trust_roots_projection(&verified)?;
    if &projected != request.phase0 {
        return Err(TrustError::new(
            "bootstrap_phase0_projection_mismatch",
            "seed Phase-0 roots differ from the verified source-adaptation projection",
        ));
    }
    Ok(seed)
}

fn validate_displayed_evidence(request: &CampaignSeedRequest<'_>) -> Result<(), TrustError> {
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
    for (logical_id, schema, value) in [
        (
            "trusted-platform-boundary-v1",
            "trellis-trusted-platform-boundary/v1",
            request.trusted_platform_boundary,
        ),
        (
            "extraction-result",
            "trellis-extraction-result/v2",
            request.extraction_result,
        ),
        (
            "extraction-determinism",
            "trellis-extraction-determinism/v2",
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
    Ok(())
}

fn build_target_definitions(
    targets: &[CampaignTargetInput],
    prose_goal: Option<&str>,
) -> Result<(Vec<Value>, Vec<Value>), TrustError> {
    let mut identities = Vec::new();
    let mut bodies = Vec::new();
    let mut seen = BTreeSet::new();
    let mut sorted = targets.to_vec();
    sorted.sort_by(|left, right| left.target_id.as_bytes().cmp(right.target_id.as_bytes()));
    for target in sorted {
        if !seen.insert(target.target_id.clone()) {
            return Err(TrustError::new(
                "bootstrap_target_duplicate",
                format!("duplicate target {:?}", target.target_id),
            ));
        }
        let deferred = prose_goal.is_some();
        let declaration = if deferred {
            if !target.lean_declaration.trim().is_empty() {
                return Err(TrustError::new(
                    "bootstrap_target_declaration_invalid",
                    format!("prose target {} must bind its statement mid-run", target.target_id),
                ));
            }
            String::new()
        } else {
            normalize_target_declaration(&target.lean_declaration)?
        };
        let mut body = serde_json::json!({
            "schema": "trellis-campaign-target-definition/v1",
            "target_id": target.target_id,
            "node_id": target.node_id,
            "resolution": target.resolution,
            "normalized_lean_declaration_utf8": declaration,
            "informal": target.informal,
        });
        if deferred {
            body["statement_deferred"] = Value::Bool(true);
        }
        push_plain_definition(
            &mut identities,
            &mut bodies,
            "target_definition",
            &target.target_id,
            "trellis://campaign/target-definition/v1",
            DomainTag::TargetDefinition,
            body,
        )?;
    }
    Ok((identities, bodies))
}

fn finish_seed(
    run_id: &str,
    seed_plan_id: &str,
    evidence_root: Sha256Digest,
    mut identities: Vec<Value>,
    mut bodies: Vec<Value>,
    prose_goal: Option<&str>,
    acknowledgment: String,
) -> Result<ConstructedSeed, TrustError> {
    let registry = SchemaRegistry::v1()?;
    let mut zipped: Vec<_> = identities.drain(..).zip(bodies.drain(..)).collect();
    zipped.sort_by(|(left, _), (right, _)| {
        left["record_kind"]
            .as_str()
            .unwrap_or("")
            .as_bytes()
            .cmp(right["record_kind"].as_str().unwrap_or("").as_bytes())
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
        "run_id": run_id,
        "seed_plan_id": seed_plan_id,
        "definitions": identities,
        "authored_semantic_root": authored_root,
        "approved_evidence_tool_input_root": evidence_root,
        "manifest_sha256": Sha256Digest::ZERO,
    });
    if let Some(goal) = prose_goal {
        if goal.trim().is_empty() {
            return Err(TrustError::new(
                "seed_goal_prose_invalid",
                "the prose GOAL referent cannot be empty",
            ));
        }
        seed["pv_goal_prose_sha256"] = Value::String(raw_sha256(goal.as_bytes()).to_string());
        seed["pv_goal_prose_utf8"] = Value::String(goal.to_owned());
    }
    let digest = self_digest(
        DomainTag::SeedAuthoredDefinitionManifest,
        &seed,
        "manifest_sha256",
    )?;
    seed["manifest_sha256"] = Value::String(digest.to_string());
    let seed_manifest = AuthoritativeRecord::parse(&registry, seed)?;
    let mut bundle = serde_json::json!({
        "schema": "trellis-seed-definition-bundle/v1",
        "definitions": bodies,
        "bundle_sha256": Sha256Digest::ZERO,
    });
    let bundle_digest = self_digest(DomainTag::ManifestNode, &bundle, "bundle_sha256")?;
    bundle["bundle_sha256"] = Value::String(bundle_digest.to_string());
    let seed_manifest_bytes = seed_manifest.canonical_bytes()?;
    let seed_definition_bundle_bytes = canonical_json_value(&bundle)?;
    let verified = verify_seed_definition_bundle(&seed_manifest, &seed_definition_bundle_bytes)?;
    let _ = seed_adaptation_ledger_projection(&verified)?;
    Ok(ConstructedSeed {
        seed_manifest,
        seed_manifest_bytes,
        seed_definition_bundle: bundle,
        seed_definition_bundle_bytes,
        launch_acknowledgment_bytes: acknowledgment.into_bytes(),
    })
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
    push_definition(identities, bodies, kind, id, schema_id, digest, tag, body);
    Ok(digest)
}

fn render_campaign_launch_acknowledgment(
    request: &CampaignSeedRequest<'_>,
) -> Result<String, TrustError> {
    let mut output = format!(
        "Trellis trust-v1 launch acknowledgment (seed sight, no verdict)\n\nRun: {}\nSeed plan: {}\nEvidence/tool root: {}\nSource tree: {}\n",
        request.run_id,
        request.seed_plan_id,
        request.evidence.evidence_tool_input_root,
        request.source_tree_sha256,
    );
    output.push_str(&format!(
        "Phase-0 sealed generation: {}\nPhase-0 semantic bundle: {}\nTarget upload archive: {}\nSupporting upload archive: {}\nSource partition manifest: {}\nTarget source partition: {}\nSupporting source partition: {}\nUnadapted source union: {}\nAdapted source tree: {}\nGOAL binding report: {}\nPhase-0 production result: {}\nSame-model Phase-0 audit lanes explicitly allowed: {}\nSeam-repair verdicts: {}\nSource-correspondence verdicts: {}\n",
        request.phase0.sealed_generation_sha256,
        request.phase0.phase0_bundle_sha256,
        request.phase0.target_upload_sha256,
        request.phase0.supporting_upload_sha256.map(|digest| digest.to_string()).unwrap_or_else(|| "absent".into()),
        request.phase0.source_partition_manifest_sha256,
        request.phase0.target_source_tree_sha256,
        request.phase0.supporting_source_tree_sha256,
        request.phase0.unadapted_source_tree_sha256,
        request.phase0.adapted_source_tree_sha256,
        request.phase0.goal_binding_report_sha256,
        request.phase0.production_result_sha256,
        request.phase0.allow_same_model_lanes,
        request.phase0.seam_repair_verdict_sha256.iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
        request.phase0.correspondence_verdict_sha256.iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
    ));
    if let Some(goal) = request.pv_goal_prose_utf8 {
        output.push_str(&format!(
            "Prose GOAL referent: {}\n\nProse GOAL bytes (verbatim)\n{goal}\n--- end of prose GOAL ---\n",
            raw_sha256(goal.as_bytes()),
        ));
    }
    output.push_str("\nPinned evidence inputs\n");
    for (logical_id, leaf) in &request.evidence.leaves_by_logical_id {
        output.push_str(&format!(
            "- logical_id={}; raw_sha256={}; path={}\n",
            gate_string(logical_id),
            leaf.raw_sha256,
            gate_string(&leaf.relative_path),
        ));
    }
    output.push_str("\nRegistered targets\n");
    let mut targets = request.targets.to_vec();
    targets.sort_by(|left, right| left.target_id.as_bytes().cmp(right.target_id.as_bytes()));
    for target in targets {
        output.push_str(&format!(
            "- target={}; node={}; resolution={:?}; model_refinement_sha256={}\n",
            gate_string(&target.target_id),
            gate_string(&target.node_id),
            target.resolution,
            target.model_refinement_sha256,
        ));
    }
    output.push_str("\nSeed adaptation ledger\n");
    for row in &request.trust_plan.adaptation_ledger {
        output.push_str(&format!(
            "- {}\n",
            String::from_utf8_lossy(&canonical_json_value(&serde_json::to_value(row).map_err(
                |error| TrustError::new("adaptation_ledger_not_serializable", error.to_string()),
            )?)?),
        ));
    }
    output.push_str(
        "\nApproval authorizes exactly the displayed methods, texts, evidence bytes, target resolutions, and closure roots. It does not assert that any theorem is true or that adapted source equals upstream Rust.\n",
    );
    Ok(output)
}

fn render_conservative_launch_acknowledgment(request: &ConservativeSeedRequest<'_>) -> String {
    let mut output = format!(
        "Trellis trust-v1 launch acknowledgment (seed sight, no verdict)\n\nRun: {}\nEvidence/tool root: {}\nSource tree: {}\n",
        request.run_id, request.evidence_tool_input_root, request.source_tree_sha256,
    );
    if let Some(goal) = request.pv_goal_prose_utf8 {
        output.push_str(&format!(
            "Prose GOAL referent: {}\n\nProse GOAL bytes (verbatim)\n{goal}\n--- end of prose GOAL ---\n",
            raw_sha256(goal.as_bytes()),
        ));
    }
    output.push_str("\nRegistered targets\n");
    for target in request.targets {
        output.push_str(&format!(
            "- target={}; resolution={:?}\n",
            gate_string(&target.target_id), target.resolution,
        ));
    }
    output.push_str(
        "\nApproval authorizes exactly these bytes and roots. It does not assert that any theorem is true or that adapted source equals upstream Rust.\n",
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
    approved_axioms_manifest: &Value,
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

    let boundary_values = manifest
        .get("boundary_definitions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_definitions_missing",
                "model refinement manifest lacks boundary_definitions",
            )
        })?;
    if !boundary_values.is_empty() {
        return Err(TrustError::new(
            "bootstrap_model_refinement_definition_invalid",
            "the extracted model must not pre-install boundary definitions",
        ));
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
    let mut unresolved_signatures = BTreeMap::new();
    for axiom in unresolved_values {
        let name = axiom.get("name").and_then(Value::as_str).ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_axiom_invalid",
                "unresolved boundary axiom lacks a name",
            )
        })?;
        let signature = axiom.get("stubs").and_then(Value::as_str).ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_axiom_invalid",
                format!("unresolved boundary axiom {name} lacks its exact Lean signature"),
            )
        })?;
        if name.is_empty()
            || signature.is_empty()
            || !matches!(
                axiom.get("classification").and_then(Value::as_str),
                Some("aeneas-stdlib-prim" | "auto-derived-method")
            )
        {
            return Err(TrustError::new(
                "bootstrap_model_refinement_axiom_invalid",
                "unresolved boundary axioms require a name, exact signature, and closed classification",
            ));
        }
        if !unresolved.insert(name.to_owned()) {
            return Err(TrustError::new(
                "bootstrap_model_refinement_boundary_overlap",
                "unresolved boundary names must be unique",
            ));
        }
        unresolved_signatures.insert(name.to_owned(), signature.to_owned());
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
    if !validity_values.is_empty() {
        return Err(TrustError::new(
            "bootstrap_model_refinement_validity_invalid",
            "the extracted model must not pre-install validity definitions",
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
        let actual_opaque_calls: BTreeSet<_> = unresolved
            .iter()
            .filter(|name| source.contains(name.as_str()))
            .cloned()
            .collect();
        if declared_dependencies != actual_dependencies
            || !declared_defined_calls.is_empty()
            || declared_opaque_calls != actual_opaque_calls
        {
            return Err(TrustError::new(
                "bootstrap_model_refinement_call_graph_mismatch",
                format!("generated call graph differs from Tablet/{node_id}.lean"),
            ));
        }
        pending.extend(actual_dependencies);
    }

    let preamble = read_regular_model_file(tablet_root, "Preamble.lean")?;
    let assumptions = read_regular_model_file(tablet_root, "Assumptions.lean")?;
    let preamble_utf8 = std::str::from_utf8(&preamble).map_err(|_| {
        TrustError::new(
            "bootstrap_model_refinement_preamble_not_utf8",
            "Tablet/Preamble.lean is not UTF-8",
        )
    })?;
    let assumptions_utf8 = std::str::from_utf8(&assumptions).map_err(|_| {
        TrustError::new(
            "bootstrap_model_refinement_assumptions_not_utf8",
            "Tablet/Assumptions.lean is not UTF-8",
        )
    })?;
    reject_forbidden_lean_declarations(
        preamble_utf8,
        &[
            "def", "abbrev", "instance", "theorem", "lemma", "macro", "notation", "opaque",
        ],
        "bootstrap_model_refinement_preamble_forbidden_declaration",
        "Preamble",
    )?;
    reject_forbidden_lean_declarations(
        assumptions_utf8,
        &[
            "axiom",
            "def",
            "abbrev",
            "instance",
            "theorem",
            "lemma",
            "macro",
            "macro_rules",
            "notation",
            "opaque",
            "structure",
            "inductive",
            "class",
            "constant",
            "example",
            "syntax",
        ],
        "bootstrap_model_refinement_assumptions_declaration",
        "Assumptions",
    )?;
    let actual_axioms = preamble_axiom_signatures(preamble_utf8)?;
    if actual_axioms != unresolved_signatures {
        return Err(TrustError::new(
            "bootstrap_model_refinement_preamble_mismatch",
            "Preamble opaque axiom names and exact signatures differ from the disclosed inventory",
        ));
    }
    if tcb_manifest.get("boundary_definitions") != manifest.get("boundary_definitions") {
        return Err(TrustError::new(
            "bootstrap_model_refinement_tcb_mismatch",
            "TCB manifest boundary definitions differ from the model refinement manifest",
        ));
    }
    let approved_axioms = tcb_manifest
        .get("global")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_tcb_invalid",
                "TCB manifest global allowlist is missing or is not an array",
            )
        })?;
    let approved_axioms: BTreeSet<_> = approved_axioms
        .iter()
        .map(|item| {
            item.as_str().map(str::to_owned).ok_or_else(|| {
                TrustError::new(
                    "bootstrap_model_refinement_tcb_invalid",
                    "TCB manifest global allowlist entries must be strings",
                )
            })
        })
        .collect::<Result<_, _>>()?;
    if approved_axioms != unresolved {
        return Err(TrustError::new(
            "bootstrap_model_refinement_tcb_mismatch",
            "TCB manifest global allowlist differs from the disclosed opaque axiom inventory",
        ));
    }

    let namespace = match tcb_manifest.get("namespace") {
        None => "",
        Some(Value::String(namespace)) => namespace.trim(),
        Some(_) => {
            return Err(TrustError::new(
                "bootstrap_model_refinement_tcb_invalid",
                "TCB manifest namespace must be a string when present",
            ));
        }
    };
    let qualified_tcb_axioms: BTreeSet<_> = approved_axioms
        .iter()
        .map(|name| {
            if namespace.is_empty() || name.starts_with(&format!("{namespace}.")) {
                name.clone()
            } else {
                format!("{namespace}.{name}")
            }
        })
        .collect();
    let operative_approved_axioms = approved_axioms_manifest
        .get("global")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TrustError::new(
                "bootstrap_model_refinement_approved_axioms_invalid",
                "APPROVED_AXIOMS.json global allowlist is missing or is not an array",
            )
        })?
        .iter()
        .map(|item| {
            item.as_str().map(str::to_owned).ok_or_else(|| {
                TrustError::new(
                    "bootstrap_model_refinement_approved_axioms_invalid",
                    "APPROVED_AXIOMS.json global allowlist entries must be strings",
                )
            })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if operative_approved_axioms != qualified_tcb_axioms {
        return Err(TrustError::new(
            "bootstrap_model_refinement_approved_axioms_mismatch",
            "APPROVED_AXIOMS.json global allowlist differs from the namespace-qualified TCB manifest projection",
        ));
    }

    aeneas_model_refinement_closure_digest(
        embedded,
        public_entry_point,
        &reachable,
        raw_sha256(&preamble),
        raw_sha256(&assumptions),
        raw_sha256(&canonical_json_value(tcb_manifest)?),
    )
}

fn aeneas_model_refinement_closure_digest(
    manifest_sha256: Sha256Digest,
    public_entry_point: &str,
    reachable_node_ids: &BTreeSet<String>,
    preamble_sha256: Sha256Digest,
    assumptions_sha256: Sha256Digest,
    tcb_manifest_sha256: Sha256Digest,
) -> Result<Sha256Digest, TrustError> {
    let closure = serde_json::json!({
        "schema": "trellis-aeneas-model-refinement-closure/v1",
        "manifest_sha256": manifest_sha256,
        "public_entry_point": public_entry_point,
        "reachable_node_ids": reachable_node_ids,
        "preamble_sha256": preamble_sha256,
        "assumptions_sha256": assumptions_sha256,
        "tcb_manifest_sha256": tcb_manifest_sha256,
    });
    Ok(tagged_hash(
        DomainTag::CarrierRefinementDefinition,
        &canonical_json_value(&closure)?,
    ))
}

/// Recompute the extractor's disclosed opaque declarations from the generated
/// Preamble. Aeneas emits each axiom in a blank-line-delimited declaration
/// block; the extractor preserves those blocks verbatim and stores the
/// whitespace-normalized signature (including every binder head) in `stubs`.
fn preamble_axiom_signatures(source: &str) -> Result<BTreeMap<String, String>, TrustError> {
    let uncommented = strip_lean_comments_preserving_lines(source);
    let mut blocks = Vec::new();
    let mut block = Vec::new();
    for line in uncommented.lines() {
        if line.trim().is_empty() {
            if !block.is_empty() {
                blocks.push(std::mem::take(&mut block));
            }
        } else {
            block.push(line);
        }
    }
    if !block.is_empty() {
        blocks.push(block);
    }
    let mut output = BTreeMap::new();
    for block in blocks {
        let flat = block
            .into_iter()
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>()
            .join(" ");
        let Some(axiom_offset) = lean_keyword_offset(&flat, "axiom") else {
            continue;
        };
        let declaration = flat[axiom_offset + "axiom".len()..].trim();
        let Some(name) = declaration.split_whitespace().next() else {
            return Err(TrustError::new(
                "bootstrap_model_refinement_preamble_mismatch",
                "Preamble contains an axiom command without a name",
            ));
        };
        let name = name.trim_end_matches(':');
        let mut signature = declaration[name.len()..].trim();
        if signature.starts_with(':') {
            signature = signature[1..].trim();
        }
        if name.is_empty()
            || signature.is_empty()
            || output
                .insert(name.to_owned(), signature.to_owned())
                .is_some()
        {
            return Err(TrustError::new(
                "bootstrap_model_refinement_preamble_mismatch",
                "Preamble opaque axiom declarations must have unique names and exact signatures",
            ));
        }
    }
    Ok(output)
}

fn lean_keyword_offset(source: &str, keyword: &str) -> Option<usize> {
    let code_only = mask_lean_comments_and_literals_preserving_bytes(source, true);
    code_only.match_indices(keyword).find_map(|(offset, _)| {
        let before = code_only[..offset].chars().next_back();
        let after = code_only[offset + keyword.len()..].chars().next();
        let boundary = |character: Option<char>| {
            character.is_none_or(|character| {
                !(character.is_alphanumeric()
                    || matches!(character, '_' | '\'' | '.' | '«' | '»'))
            })
        };
        (boundary(before) && boundary(after)).then_some(offset)
    })
}

/// Whether executable Lean source declares exactly `name` with `def` or
/// `abbrev`. Comments and all supported literal forms are masked by the same
/// scanner used for bootstrap declaration checks. Both the keyword and name
/// require Lean-identifier boundaries, so `def RustValidSliceU8Extended`
/// cannot stand in for `RustValidSliceU8`.
pub(crate) fn lean_declares_definition(source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let code_only = mask_lean_comments_and_literals_preserving_bytes(source, true);
    let is_identifier_character = |character: char| {
        character.is_alphanumeric() || matches!(character, '_' | '\'' | '.' | '«' | '»')
    };
    for keyword in ["def", "abbrev"] {
        for (offset, _) in code_only.match_indices(keyword) {
            let before = code_only[..offset].chars().next_back();
            let after_keyword = offset + keyword.len();
            let after = code_only[after_keyword..].chars().next();
            if before.is_some_and(is_identifier_character)
                || after.is_some_and(is_identifier_character)
            {
                continue;
            }
            let declaration = code_only[after_keyword..].trim_start();
            let Some(rest) = declaration.strip_prefix(name) else {
                continue;
            };
            if rest
                .chars()
                .next()
                .is_none_or(|character| !is_identifier_character(character))
            {
                return true;
            }
        }
    }
    false
}

fn strip_lean_comments_preserving_lines(source: &str) -> String {
    mask_lean_comments_and_literals_preserving_bytes(source, false)
}

fn reject_forbidden_lean_declarations(
    source: &str,
    keywords: &[&str],
    error_code: &'static str,
    file_stem: &str,
) -> Result<(), TrustError> {
    if let Some(keyword) = keywords
        .iter()
        .find(|keyword| lean_keyword_offset(source, keyword).is_some())
    {
        return Err(TrustError::new(
            error_code,
            format!("Tablet/{file_stem}.lean contains forbidden `{keyword}` declaration syntax"),
        ));
    }
    Ok(())
}

fn mask_lean_comments_and_literals_preserving_bytes(source: &str, mask_literals: bool) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        LineComment,
        BlockComment(u32),
        String,
        RawString(usize),
    }

    fn blank(output: &mut [u8], source: &[u8], index: usize) {
        if source[index] != b'\n' {
            output[index] = b' ';
        }
    }

    fn raw_string_start(source: &[u8], index: usize) -> Option<(usize, usize)> {
        if source.get(index) != Some(&b'r') {
            return None;
        }
        let mut cursor = index + 1;
        while source.get(cursor) == Some(&b'#') {
            cursor += 1;
        }
        (source.get(cursor) == Some(&b'"')).then_some((cursor - index - 1, cursor + 1))
    }

    fn ordinary_string_end(source: &[u8], opening_quote: usize) -> Option<usize> {
        let mut cursor = opening_quote + 1;
        while cursor < source.len() {
            if source[cursor] == b'\\' && cursor + 1 < source.len() {
                cursor += 2;
            } else if source[cursor] == b'"' {
                return Some(cursor + 1);
            } else {
                cursor += 1;
            }
        }
        None
    }

    fn raw_string_end(source: &[u8], opening: usize) -> Option<usize> {
        let (hashes, mut cursor) = raw_string_start(source, opening)?;
        while cursor < source.len() {
            if source[cursor] == b'"'
                && source
                    .get(cursor + 1..cursor + 1 + hashes)
                    .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
            {
                return Some(cursor + 1 + hashes);
            }
            cursor += 1;
        }
        None
    }

    fn is_interpolated_string_start(source: &[u8], opening_quote: usize) -> bool {
        let mut cursor = opening_quote;
        while cursor > 0 && source[cursor - 1].is_ascii_whitespace() {
            cursor -= 1;
        }
        cursor > 0 && source[cursor - 1] == b'!'
    }

    fn interpolated_string_end(source: &str, opening_quote: usize) -> Option<usize> {
        let bytes = source.as_bytes();
        let mut cursor = opening_quote + 1;
        let mut brace_depth = 0_u32;
        while cursor < bytes.len() {
            if brace_depth == 0 {
                if bytes[cursor] == b'\\' && cursor + 1 < bytes.len() {
                    cursor += 2;
                } else if bytes[cursor] == b'"' {
                    return Some(cursor + 1);
                } else if bytes[cursor] == b'{' {
                    brace_depth = 1;
                    cursor += 1;
                } else {
                    cursor += 1;
                }
                continue;
            }

            let next = bytes.get(cursor + 1).copied();
            if bytes[cursor] == b'-' && next == Some(b'-') {
                cursor += 2;
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            } else if bytes[cursor] == b'/' && next == Some(b'-') {
                cursor += 2;
                let mut comment_depth = 1_u32;
                while cursor < bytes.len() && comment_depth > 0 {
                    let comment_next = bytes.get(cursor + 1).copied();
                    if bytes[cursor] == b'/' && comment_next == Some(b'-') {
                        comment_depth += 1;
                        cursor += 2;
                    } else if bytes[cursor] == b'-' && comment_next == Some(b'/') {
                        comment_depth -= 1;
                        cursor += 2;
                    } else {
                        cursor += 1;
                    }
                }
            } else if let Some((_, _)) = raw_string_start(bytes, cursor) {
                cursor = raw_string_end(bytes, cursor)?;
            } else if bytes[cursor] == b'"' {
                cursor = if is_interpolated_string_start(bytes, cursor) {
                    interpolated_string_end(source, cursor)?
                } else {
                    ordinary_string_end(bytes, cursor)?
                };
            } else if bytes[cursor] == b'\'' {
                cursor = character_literal_end(source, cursor).unwrap_or(cursor + 1);
            } else if bytes[cursor] == b'{' {
                brace_depth += 1;
                cursor += 1;
            } else if bytes[cursor] == b'}' {
                brace_depth -= 1;
                cursor += 1;
            } else {
                cursor += 1;
            }
        }
        None
    }

    fn character_literal_end(source: &str, index: usize) -> Option<usize> {
        let rest = source.get(index + 1..)?;
        let mut characters = rest.char_indices();
        let (_, first) = characters.next()?;
        if first == '\'' {
            return None;
        }
        let content_bytes = if first == '\\' {
            let (_, escaped) = characters.next()?;
            match escaped {
                'x' => 4,
                'u' => 6,
                _ => 2,
            }
        } else {
            first.len_utf8()
        };
        let closing = index + 1 + content_bytes;
        (source.as_bytes().get(closing) == Some(&b'\'')).then_some(closing + 1)
    }

    let bytes = source.as_bytes();
    let mut output = bytes.to_vec();
    let mut index = 0;
    let mut state = State::Code;
    while index < bytes.len() {
        let next = bytes.get(index + 1).copied();
        match state {
            State::Code if bytes[index] == b'-' && next == Some(b'-') => {
                blank(&mut output, bytes, index);
                blank(&mut output, bytes, index + 1);
                index += 2;
                state = State::LineComment;
            }
            State::Code if bytes[index] == b'/' && next == Some(b'-') => {
                blank(&mut output, bytes, index);
                blank(&mut output, bytes, index + 1);
                index += 2;
                state = State::BlockComment(1);
            }
            State::Code => {
                if let Some((hashes, after_opening)) = raw_string_start(bytes, index) {
                    if mask_literals {
                        for cursor in index..after_opening {
                            blank(&mut output, bytes, cursor);
                        }
                    }
                    index = after_opening;
                    state = State::RawString(hashes);
                } else if bytes[index] == b'"' && is_interpolated_string_start(bytes, index) {
                    let end = interpolated_string_end(source, index).unwrap_or(bytes.len());
                    if mask_literals {
                        for cursor in index..end {
                            blank(&mut output, bytes, cursor);
                        }
                    }
                    index = end;
                } else if bytes[index] == b'"' {
                    if mask_literals {
                        blank(&mut output, bytes, index);
                    }
                    index += 1;
                    state = State::String;
                } else if bytes[index] == b'\'' {
                    if let Some(end) = character_literal_end(source, index) {
                        if mask_literals {
                            for cursor in index..end {
                                blank(&mut output, bytes, cursor);
                            }
                        }
                        index = end;
                    } else {
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            State::LineComment => {
                blank(&mut output, bytes, index);
                if bytes[index] == b'\n' {
                    state = State::Code;
                }
                index += 1;
            }
            State::BlockComment(depth) => {
                blank(&mut output, bytes, index);
                if bytes[index] == b'/' && next == Some(b'-') {
                    blank(&mut output, bytes, index + 1);
                    index += 2;
                    state = State::BlockComment(depth + 1);
                } else if bytes[index] == b'-' && next == Some(b'/') {
                    blank(&mut output, bytes, index + 1);
                    index += 2;
                    state = if depth == 1 {
                        State::Code
                    } else {
                        State::BlockComment(depth - 1)
                    };
                } else {
                    index += 1;
                }
            }
            State::String => {
                if mask_literals {
                    blank(&mut output, bytes, index);
                }
                if bytes[index] == b'\\' && next.is_some() {
                    if mask_literals {
                        blank(&mut output, bytes, index + 1);
                    }
                    index += 2;
                } else if bytes[index] == b'"' {
                    index += 1;
                    state = State::Code;
                } else {
                    index += 1;
                }
            }
            State::RawString(hashes) => {
                if bytes[index] == b'"'
                    && bytes
                        .get(index + 1..index + 1 + hashes)
                        .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
                {
                    if mask_literals {
                        for cursor in index..index + 1 + hashes {
                            blank(&mut output, bytes, cursor);
                        }
                    }
                    index += 1 + hashes;
                    state = State::Code;
                } else {
                    if mask_literals {
                        blank(&mut output, bytes, index);
                    }
                    index += 1;
                }
            }
        }
    }
    String::from_utf8(output).expect("masking UTF-8 with ASCII spaces preserves UTF-8")
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

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::closure::VerifiedEvidenceLeaf;

    fn displayed_evidence() -> (VerifiedEvidenceClosure, Value, Value, Value) {
        let boundary = serde_json::json!({
            "schema": "trellis-trusted-platform-boundary/v1",
            "surface": "fixture"
        });
        let extraction = serde_json::json!({
            "schema": "trellis-extraction-result/v2",
            "runs": []
        });
        let determinism = serde_json::json!({
            "schema": "trellis-extraction-determinism/v2",
            "status": "passed"
        });
        let leaf = |kind: &str, relative_path: &str, value: &Value| VerifiedEvidenceLeaf {
            kind: kind.to_owned(),
            relative_path: relative_path.to_owned(),
            byte_length: canonical_json_value(value).unwrap().len() as u64,
            raw_sha256: raw_sha256(&canonical_json_value(value).unwrap()),
            dependency_ids: Vec::new(),
        };
        let evidence = VerifiedEvidenceClosure {
            manifest_sha256: raw_sha256(b"manifest"),
            evidence_tool_input_root: raw_sha256(b"evidence-root"),
            file_count: 3,
            leaves_by_logical_id: BTreeMap::from([
                (
                    "trusted-platform-boundary-v1".to_owned(),
                    leaf("boundary", "boundary.json", &boundary),
                ),
                (
                    "extraction-result".to_owned(),
                    leaf("extraction", "extraction.json", &extraction),
                ),
                (
                    "extraction-determinism".to_owned(),
                    leaf("determinism", "determinism.json", &determinism),
                ),
            ]),
        };
        (evidence, boundary, extraction, determinism)
    }

    fn plan(resolution: &str) -> CampaignTrustPlan {
        CampaignTrustPlan::parse_and_validate(
            &serde_json::to_vec(&serde_json::json!({
                "schema": "trellis-campaign-trust-plan/v4",
                "adaptation_ledger": [],
                "targets": [{"target_id": "goal:claim", "resolution": resolution}]
            }))
            .unwrap(),
        )
        .unwrap()
    }

    fn target(resolution: crate::model::ChallengeResolution) -> CampaignTargetInput {
        CampaignTargetInput {
            target_id: "goal:claim".into(),
            resolution,
            node_id: "Claim".into(),
            lean_declaration: String::new(),
            informal: "fixture claim".into(),
            model_refinement_sha256: raw_sha256(b"model-refinement"),
        }
    }

    fn phase0_roots() -> Phase0TrustRoots {
        Phase0TrustRoots {
            allow_same_model_lanes: false,
            sealed_generation_sha256: raw_sha256(b"phase0-seal"),
            phase0_bundle_sha256: raw_sha256(b"phase0-bundle"),
            target_upload_sha256: raw_sha256(b"phase0-target-upload"),
            supporting_upload_sha256: Some(raw_sha256(b"phase0-support-upload")),
            source_partition_manifest_sha256: raw_sha256(b"phase0-partition-manifest"),
            target_source_tree_sha256: raw_sha256(b"phase0-target-tree"),
            supporting_source_tree_sha256: raw_sha256(b"phase0-support-tree"),
            unadapted_source_tree_sha256: raw_sha256(b"phase0-unadapted"),
            adapted_source_tree_sha256: raw_sha256(b"phase0-adapted"),
            goal_sha256: raw_sha256(b"phase0-goal"),
            goal_binding_report_sha256: raw_sha256(b"phase0-binding"),
            adaptation_ledger_sha256: raw_sha256(b"phase0-ledger"),
            final_checker_receipt_sha256: raw_sha256(b"phase0-checker"),
            seam_repair_verdict_sha256: vec![raw_sha256(b"phase0-seam")],
            correspondence_verdict_sha256: vec![raw_sha256(b"phase0-correspondence")],
            production_result_sha256: raw_sha256(b"phase0-production"),
        }
    }

    #[test]
    fn bootstrap_refuses_plan_target_resolution_disagreement() {
        let (evidence, boundary, extraction, determinism) = displayed_evidence();
        let targets = [target(crate::model::ChallengeResolution::Decide)];
        let trust_plan = plan("prove");
        let phase0 = phase0_roots();
        let error = build_campaign_seed(CampaignSeedRequest {
            run_id: "run",
            seed_plan_id: "plan",
            evidence: &evidence,
            extraction_result: &extraction,
            extraction_determinism: &determinism,
            trusted_platform_boundary: &boundary,
            source_tree_sha256: raw_sha256(b"source"),
            targets: &targets,
            trust_plan: &trust_plan,
            phase0: &phase0,
            pv_goal_prose_utf8: Some("# Verification targets\n\n- claim\n"),
        })
        .unwrap_err();
        assert_eq!(error.code, "bootstrap_target_resolution_mismatch");
    }

    #[test]
    fn bootstrap_copies_agreed_resolution_into_target_definition() {
        let (evidence, boundary, extraction, determinism) = displayed_evidence();
        let targets = [target(crate::model::ChallengeResolution::Decide)];
        let trust_plan = plan("decide");
        let phase0 = phase0_roots();
        let seed = build_campaign_seed(CampaignSeedRequest {
            run_id: "run",
            seed_plan_id: "plan",
            evidence: &evidence,
            extraction_result: &extraction,
            extraction_determinism: &determinism,
            trusted_platform_boundary: &boundary,
            source_tree_sha256: raw_sha256(b"source"),
            targets: &targets,
            trust_plan: &trust_plan,
            phase0: &phase0,
            pv_goal_prose_utf8: Some("# Verification targets\n\n- claim\n"),
        })
        .unwrap();
        let bundle = String::from_utf8(seed.seed_definition_bundle_bytes).unwrap();
        assert!(bundle.contains("\"resolution\":\"decide\""));
        assert!(bundle.contains("\"statement_deferred\":true"));
    }

    #[test]
    fn conservative_seed_retains_verified_definition_closure() {
        let targets = [CampaignTargetInput {
            target_id: "claim".into(),
            resolution: crate::model::ChallengeResolution::Prove,
            node_id: "claim".into(),
            lean_declaration: "theorem claim : True := by".into(),
            informal: "fixture".into(),
            model_refinement_sha256: raw_sha256(b"model-refinement"),
        }];
        let seed = build_conservative_campaign_seed(ConservativeSeedRequest {
            run_id: "fixture-run",
            seed_plan_id: "fixture-plan",
            evidence_tool_input_root: raw_sha256(b"evidence"),
            source_tree_sha256: raw_sha256(b"source"),
            contract_generator_sha256: raw_sha256(b"kernel"),
            targets: &targets,
            pv_goal_prose_utf8: None,
        })
        .unwrap();
        verify_seed_definition_bundle(
            &seed.seed_manifest,
            &seed.seed_definition_bundle_bytes,
        )
        .unwrap();
    }
}

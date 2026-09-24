//! Optional Rust witness artifacts attached to checked Lean disproofs.
//!
//! This module records provenance and correspondence review.  Neither an
//! execution receipt nor a correspondence verdict is proof authority.

use super::canonical::{
    canonical_json, canonical_json_value, raw_sha256, self_digest, DomainTag, Sha256Digest,
    TrustError,
};
use super::closure::VerifiedEvidenceClosure;
use super::execution::{validate_execution_receipt, CheckedExecutionReceipt};
use super::execution::{
    execute_approved_json, ApprovedExecutionRequest, ExecutionLimits, PinnedExecutionRequest,
};
use crate::model::{
    ChallengePolarity, ChallengeResolution, ChallengeTargetId, LocalClosureRecord, ProtocolState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const RUST_WITNESS_ARTIFACT_SCHEMA: &str = "trellis-rust-witness-artifact/v1";
pub const RUST_WITNESS_ARTIFACT_SCHEMA_ID: &str =
    "trellis://schemas/rust-witness-artifact/v1";
pub const RUST_WITNESS_RUNNER_LOGICAL_ID: &str = "pv-rust-witness-artifact-runner";
pub const RUST_WITNESS_EXECUTION_PURPOSE: &str = "pv-rust-witness-artifact-run/v1";
pub const RUST_WITNESS_RUNNER_OUTPUT_SCHEMA: &str =
    "trellis-pv-rust-witness-runner-output/v1";
pub const RUST_WITNESS_CORRESPONDENCE_REQUEST_SCHEMA: &str =
    "trellis-rust-witness-correspondence-request/v1";
pub const RUST_WITNESS_CORRESPONDENCE_VERDICT_SCHEMA: &str =
    "trellis-rust-witness-correspondence-verdict/v1";
pub const MAX_RUST_WITNESS_SOURCE_BYTES: usize = 1024 * 1024;

pub struct RustWitnessRunContext<'a> {
    pub repo_root: &'a Path,
    pub evidence_root: &'a Path,
    pub runtime_root: &'a Path,
    pub evidence: &'a VerifiedEvidenceClosure,
    pub launch_acknowledgment_sha256: Sha256Digest,
    pub pinned_crate_tree_sha256: Sha256Digest,
    pub cargo_lock_sha256: Sha256Digest,
    pub rustc_path: &'a Path,
    pub cargo_path: &'a Path,
    pub toolchain_root: &'a Path,
    pub dependency_cache_path: Option<&'a Path>,
    pub rustc_vv: &'a str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RustWitnessArtifactDeclaration {
    pub target_id: ChallengeTargetId,
    pub relative_path: String,
}

/// The receipt summary deliberately does not classify the Rust observation.
/// Build and test status remains visible in the complete receipt payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RustWitnessExecution {
    NotAttempted,
    Receipt {
        receipt_sha256: Sha256Digest,
        runner_sha256: Sha256Digest,
        timed_out: bool,
        exit_code: Option<i32>,
    },
}

impl Default for RustWitnessExecution {
    fn default() -> Self {
        Self::NotAttempted
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RustWitnessCorrespondence {
    NotReviewed,
    Pass {
        request_sha256: Sha256Digest,
        verdict_sha256: Sha256Digest,
        reason: String,
    },
    Fail {
        request_sha256: Sha256Digest,
        verdict_sha256: Sha256Digest,
        reason: String,
    },
}

impl Default for RustWitnessCorrespondence {
    fn default() -> Self {
        Self::NotReviewed
    }
}

/// The one normative state/claim/gate/archive record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustWitnessArtifactRecord {
    pub schema: String,
    pub target_id: ChallengeTargetId,
    pub relative_path: String,
    pub artifact_sha256: Sha256Digest,
    pub pinned_crate_tree_sha256: Sha256Digest,
    pub execution: RustWitnessExecution,
    pub correspondence: RustWitnessCorrespondence,
    pub freeze_episode_id: String,
    pub gate_episode_id: String,
}

/// Frozen bytes needed to render the correspondence request and gate dossier.
/// It is kept separate so the normative record remains provenance/status only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustWitnessArtifactPayload {
    pub source_utf8: String,
    pub runner_request: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_receipt: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correspondence_request: Option<RustWitnessCorrespondenceRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correspondence_verdict: Option<RustWitnessCorrespondenceVerdict>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustWitnessReviewedDigests {
    pub target_statement_sha256: Sha256Digest,
    pub negated_statement_sha256: Sha256Digest,
    pub negative_closure_sha256: Sha256Digest,
    pub artifact_sha256: Sha256Digest,
    pub receipt_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustWitnessCorrespondenceRequest {
    pub schema: String,
    pub target_id: ChallengeTargetId,
    pub goal_target_prose_utf8: String,
    pub target_lean_utf8: String,
    pub negated_target_lean_utf8: String,
    pub checked_negative_proof_closure: LocalClosureRecord,
    pub artifact_source_utf8: String,
    pub execution_receipt: Value,
    pub reviewed_digests: RustWitnessReviewedDigests,
    pub request_sha256: Sha256Digest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RustWitnessCorrespondenceDecision {
    Pass,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustWitnessCorrespondenceVerdict {
    pub schema: String,
    pub request_sha256: Sha256Digest,
    pub same_witness_as_lean_disproof: bool,
    pub invokes_pinned_crate_operation: bool,
    pub observation_contradicts_goal_obligation: bool,
    pub reviewed_digests: RustWitnessReviewedDigests,
    pub decision: RustWitnessCorrespondenceDecision,
    pub reason: String,
    pub verdict_sha256: Sha256Digest,
}

/// Agent-authored portion of the Corr response. The kernel adds the schema
/// and self digest after checking its request and reviewed-digest echoes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustWitnessCorrespondenceLaneVerdict {
    pub request_sha256: Sha256Digest,
    pub same_witness_as_lean_disproof: bool,
    pub invokes_pinned_crate_operation: bool,
    pub observation_contradicts_goal_obligation: bool,
    pub reviewed_digests: RustWitnessReviewedDigests,
    pub decision: RustWitnessCorrespondenceDecision,
    pub reason: String,
}

pub fn close_rust_witness_correspondence_verdict(
    lane: &RustWitnessCorrespondenceLaneVerdict,
) -> Result<RustWitnessCorrespondenceVerdict, TrustError> {
    let mut verdict = RustWitnessCorrespondenceVerdict {
        schema: RUST_WITNESS_CORRESPONDENCE_VERDICT_SCHEMA.into(),
        request_sha256: lane.request_sha256,
        same_witness_as_lean_disproof: lane.same_witness_as_lean_disproof,
        invokes_pinned_crate_operation: lane.invokes_pinned_crate_operation,
        observation_contradicts_goal_obligation: lane.observation_contradicts_goal_obligation,
        reviewed_digests: lane.reviewed_digests.clone(),
        decision: lane.decision,
        reason: lane.reason.trim().to_owned(),
        verdict_sha256: Sha256Digest::ZERO,
    };
    verdict.verdict_sha256 = rust_witness_correspondence_verdict_digest(&verdict)?;
    Ok(verdict)
}

pub fn rust_witness_target_key(target: &ChallengeTargetId) -> String {
    format!("target-{}", raw_sha256(target.as_str().as_bytes()))
}

pub fn rust_witness_relative_path(target: &ChallengeTargetId) -> String {
    format!(
        "reference/rust-witnesses/{}/witness.rs",
        rust_witness_target_key(target)
    )
}

fn relative_components(relative: &str) -> Option<Vec<&str>> {
    if relative.is_empty() || relative.contains('\\') {
        return None;
    }
    let components: Vec<_> = Path::new(relative).components().collect();
    if components.iter().any(|component| !matches!(component, Component::Normal(_))) {
        return None;
    }
    relative.split('/').all(|part| !part.is_empty()).then_some(
        relative.split('/').collect(),
    )
}

fn regular_file_beneath_without_symlinks(
    root: &Path,
    relative: &str,
) -> Result<PathBuf, String> {
    let components = relative_components(relative)
        .ok_or_else(|| "Rust witness path is not a normalized relative path".to_owned())?;
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("canonicalize Rust witness root: {error}"))?;
    let mut current = canonical_root.clone();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("Rust witness path is unavailable: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("Rust witness path contains a symlink component".into());
        }
        let final_component = index + 1 == components.len();
        if (final_component && !metadata.is_file()) || (!final_component && !metadata.is_dir()) {
            return Err("Rust witness path has the wrong filesystem type".into());
        }
    }
    let canonical_file = current
        .canonicalize()
        .map_err(|error| format!("canonicalize Rust witness source: {error}"))?;
    if !canonical_file.starts_with(&canonical_root) {
        return Err("Rust witness source resolves outside the repository".into());
    }
    Ok(canonical_file)
}

pub fn validate_rust_witness_declaration(
    state: &ProtocolState,
    declaration: &RustWitnessArtifactDeclaration,
) -> Result<(), String> {
    let target = &declaration.target_id;
    let Some(spec) = state.configured_challenge_targets.get(target) else {
        return Err("Rust witness declaration names an unconfigured target".into());
    };
    if state.decide_primary_of_refutation(target).is_some()
        || spec.resolution != ChallengeResolution::Decide
    {
        return Err("Rust witness declaration must name a Decide primary target".into());
    }
    if state.live_polarity(target) != ChallengePolarity::Disprove {
        return Err("Rust witness declaration requires the target's live Disprove side".into());
    }
    let expected = rust_witness_relative_path(target);
    if declaration.relative_path != expected || relative_components(&expected).is_none() {
        return Err(format!(
            "Rust witness declaration path must equal the kernel-derived path {expected}"
        ));
    }
    Ok(())
}

pub fn snapshot_rust_witness_source(
    state: &ProtocolState,
    repo_root: &Path,
    declaration: &RustWitnessArtifactDeclaration,
    pinned_crate_tree_sha256: Sha256Digest,
    runner_request: Value,
) -> Result<(RustWitnessArtifactRecord, RustWitnessArtifactPayload), String> {
    validate_rust_witness_declaration(state, declaration)?;
    if pinned_crate_tree_sha256 == Sha256Digest::ZERO {
        return Err("Rust witness declaration lacks a pinned crate-tree identity".into());
    }
    let path = regular_file_beneath_without_symlinks(repo_root, &declaration.relative_path)?;
    let bytes = fs::read(&path).map_err(|error| format!("read Rust witness source: {error}"))?;
    if bytes.is_empty() || bytes.len() > MAX_RUST_WITNESS_SOURCE_BYTES {
        return Err(format!(
            "Rust witness source must contain 1..={MAX_RUST_WITNESS_SOURCE_BYTES} bytes"
        ));
    }
    let source_utf8 = String::from_utf8(bytes.clone())
        .map_err(|_| "Rust witness source must be UTF-8".to_owned())?;
    let artifact_sha256 = raw_sha256(&bytes);
    let episode = state
        .trust_base
        .advance_gate_episode_id
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Rust witness snapshot lacks its freeze/gate episode identity".to_owned())?;
    Ok((
        RustWitnessArtifactRecord {
            schema: RUST_WITNESS_ARTIFACT_SCHEMA.into(),
            target_id: declaration.target_id.clone(),
            relative_path: declaration.relative_path.clone(),
            artifact_sha256,
            pinned_crate_tree_sha256,
            execution: RustWitnessExecution::NotAttempted,
            correspondence: RustWitnessCorrespondence::NotReviewed,
            freeze_episode_id: episode.clone(),
            gate_episode_id: episode,
        },
        RustWitnessArtifactPayload {
            source_utf8,
            runner_request,
            execution_receipt: None,
            correspondence_request: None,
            correspondence_verdict: None,
        },
    ))
}

/// Freeze and attempt the optional corroborating source.  A runner/sandbox
/// launch that cannot produce strict JSON leaves the closed record at
/// `not_attempted`; an actual Cargo build/test failure is a checked receipt
/// whose purpose-specific output is `unavailable`.
pub fn run_rust_witness_artifact(
    state: &ProtocolState,
    declaration: &RustWitnessArtifactDeclaration,
    context: RustWitnessRunContext<'_>,
) -> Result<(RustWitnessArtifactRecord, RustWitnessArtifactPayload), TrustError> {
    validate_rust_witness_declaration(state, declaration)
        .map_err(|reason| TrustError::new("rust_witness_declaration_invalid", reason))?;
    let source_path = regular_file_beneath_without_symlinks(
        context.repo_root,
        &declaration.relative_path,
    )
    .map_err(|reason| TrustError::new("rust_witness_source_invalid", reason))?;
    let source = fs::read(&source_path)
        .map_err(|error| TrustError::new("rust_witness_source_unreadable", error.to_string()))?;
    if source.is_empty() || source.len() > MAX_RUST_WITNESS_SOURCE_BYTES {
        return Err(TrustError::new(
            "rust_witness_source_size_invalid",
            "Rust witness source is empty or exceeds the closed limit",
        ));
    }
    std::str::from_utf8(&source).map_err(|_| {
        TrustError::new("rust_witness_source_encoding_invalid", "Rust witness source must be UTF-8")
    })?;
    let goal_path = context.repo_root.join("GOAL.md");
    let goal_bytes = fs::read(&goal_path)
        .map_err(|error| TrustError::new("rust_witness_goal_unavailable", error.to_string()))?;
    let run_id = format!(
        "{}-{}",
        rust_witness_target_key(&declaration.target_id),
        &raw_sha256(&source).to_hex()[..16]
    );
    let request = json!({
        "schema": "trellis-pv-rust-witness-runner-request/v1",
        "target_id": declaration.target_id,
        "relative_path": declaration.relative_path,
        "artifact_source_base64": BASE64_STANDARD.encode(&source),
        "artifact_sha256": raw_sha256(&source),
        "worker_source_path": source_path.to_string_lossy(),
        "pinned_crate_root": context.evidence_root.join("source/crate").to_string_lossy(),
        "pinned_crate_tree_sha256": context.pinned_crate_tree_sha256,
        "goal_path": goal_path.to_string_lossy(),
        "goal_sha256": raw_sha256(&goal_bytes),
        "cargo_lock_sha256": context.cargo_lock_sha256,
        "rustc_path": context.rustc_path.to_string_lossy(),
        "rustc_vv": context.rustc_vv,
        "cargo_path": context.cargo_path.to_string_lossy(),
        "toolchain_root": context.toolchain_root.to_string_lossy(),
        "dependency_cache_path": context.dependency_cache_path.map(Path::to_string_lossy).unwrap_or_default(),
        "runtime_root": context.runtime_root.to_string_lossy(),
        "run_id": run_id,
        "limits": {
            "timeout_seconds": 300,
            "cpu_seconds": 240,
            "address_space_bytes": 8_589_934_592_u64,
            "file_size_bytes": 268_435_456_u64,
            "process_count": 4096,
            "stdout_bytes": 8_388_608,
            "stderr_bytes": 8_388_608,
        },
    });
    let (mut record, mut payload) = snapshot_rust_witness_source(
        state,
        context.repo_root,
        declaration,
        context.pinned_crate_tree_sha256,
        request,
    )
    .map_err(|reason| TrustError::new("rust_witness_snapshot_failed", reason))?;
    let leaf = context
        .evidence
        .leaves_by_logical_id
        .get(RUST_WITNESS_RUNNER_LOGICAL_ID)
        .ok_or_else(|| {
            TrustError::new(
                "rust_witness_runner_not_approved",
                "Rust witness runner is absent from the evidence closure",
            )
        })?;
    let runner_path = context.evidence_root.join(&leaf.relative_path);
    let environment = BTreeMap::from([
        (
            "PYTHONPATH".to_owned(),
            context.evidence_root.join("runtime/source").to_string_lossy().into_owned(),
        ),
        ("PYTHONDONTWRITEBYTECODE".to_owned(), "1".to_owned()),
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
    ]);
    let receipt = execute_approved_json(ApprovedExecutionRequest {
        evidence: context.evidence,
        tool_logical_id: RUST_WITNESS_RUNNER_LOGICAL_ID,
        purpose: RUST_WITNESS_EXECUTION_PURPOSE,
        launch_acknowledgment_sha256: context.launch_acknowledgment_sha256,
        execution: PinnedExecutionRequest {
            runner_path: &runner_path,
            expected_runner_sha256: leaf.raw_sha256,
            command_id: "pv-rust-witness-artifact-runner",
            working_directory: context.runtime_root,
            environment: &environment,
            input: &payload.runner_request,
            limits: ExecutionLimits {
                timeout: Duration::from_secs(330),
                max_stdin_bytes: 2 * 1024 * 1024,
                max_stdout_bytes: 32 * 1024 * 1024,
                max_stderr_bytes: 8 * 1024 * 1024,
            },
        },
    })?;
    // A sandbox that never starts deliberately produces no parsed output and
    // therefore no durable receipt. The frozen source remains optional and
    // the Lean disproof remains closed.
    if receipt.parsed_output().is_some() {
        attach_execution_receipt(
            &mut record,
            &mut payload,
            &receipt,
            context.evidence,
            context.launch_acknowledgment_sha256,
        )?;
    }
    Ok((record, payload))
}

fn digest_local_closure(record: &LocalClosureRecord) -> Result<Sha256Digest, TrustError> {
    Ok(raw_sha256(&canonical_json(record)?))
}

pub fn attach_execution_receipt(
    record: &mut RustWitnessArtifactRecord,
    payload: &mut RustWitnessArtifactPayload,
    receipt: &CheckedExecutionReceipt,
    evidence: &VerifiedEvidenceClosure,
    launch_acknowledgment_sha256: Sha256Digest,
) -> Result<(), TrustError> {
    validate_execution_receipt(
        receipt.value(),
        evidence,
        RUST_WITNESS_EXECUTION_PURPOSE,
        launch_acknowledgment_sha256,
    )?;
    let input = receipt.value().get("input").ok_or_else(|| {
        TrustError::new("rust_witness_receipt_input_missing", "receipt lacks runner input")
    })?;
    if input != &payload.runner_request {
        return Err(TrustError::new(
            "rust_witness_receipt_request_mismatch",
            "receipt input differs from the frozen runner request",
        ));
    }
    let target_id = input.get("target_id").and_then(Value::as_str);
    let relative_path = input.get("relative_path").and_then(Value::as_str);
    let artifact_sha256 = input.get("artifact_sha256").and_then(Value::as_str);
    let crate_tree_sha256 = input.get("pinned_crate_tree_sha256").and_then(Value::as_str);
    if target_id != Some(record.target_id.as_str())
        || relative_path != Some(record.relative_path.as_str())
        || artifact_sha256 != Some(record.artifact_sha256.to_hex().as_str())
        || crate_tree_sha256 != Some(record.pinned_crate_tree_sha256.to_hex().as_str())
    {
        return Err(TrustError::new(
            "rust_witness_receipt_identity_mismatch",
            "receipt input does not bind the record target, path, artifact, and crate tree",
        ));
    }
    let parsed = receipt.parsed_output().ok_or_else(|| {
        TrustError::new(
            "rust_witness_runner_output_missing",
            "runner receipt has no complete JSON output",
        )
    })?;
    if parsed.get("schema").and_then(Value::as_str) != Some(RUST_WITNESS_RUNNER_OUTPUT_SCHEMA) {
        return Err(TrustError::new(
            "rust_witness_runner_output_invalid",
            "runner output has the wrong schema",
        ));
    }
    validate_runner_output(parsed, input)?;
    let runner_sha256: Sha256Digest = receipt
        .value()
        .get("runner_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| TrustError::new("rust_witness_runner_digest_missing", "missing runner digest"))?
        .parse()?;
    let timed_out = parsed
        .get("timed_out")
        .and_then(Value::as_bool)
        .ok_or_else(|| TrustError::new("rust_witness_receipt_status_missing", "missing timeout status"))?;
    let exit_code = match parsed.get("exit_code") {
        Some(Value::Number(number)) => number.as_i64().and_then(|value| i32::try_from(value).ok()),
        Some(Value::Null) | None => None,
        _ => None,
    };
    record.execution = RustWitnessExecution::Receipt {
        receipt_sha256: receipt.digest(),
        runner_sha256,
        timed_out,
        exit_code,
    };
    payload.execution_receipt = Some(receipt.value().clone());
    Ok(())
}

fn validate_runner_output(output: &Value, input: &Value) -> Result<(), TrustError> {
    let same_string = |field: &str| {
        output.get(field).and_then(Value::as_str) == input.get(field).and_then(Value::as_str)
    };
    if output.get("attempted").and_then(Value::as_bool) != Some(true)
        || !matches!(output.get("status").and_then(Value::as_str), Some("available" | "unavailable"))
        || ![
            "target_id",
            "relative_path",
            "artifact_sha256",
            "pinned_crate_tree_sha256",
            "cargo_lock_sha256",
            "goal_sha256",
            "rustc_vv",
        ]
        .iter()
        .all(|field| same_string(field))
    {
        return Err(TrustError::new(
            "rust_witness_runner_binding_mismatch",
            "runner output does not bind every frozen request identity",
        ));
    }
    let stdout = output
        .get("stdout_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| TrustError::new("rust_witness_runner_capture_invalid", "missing stdout"))?;
    let stderr = output
        .get("stderr_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| TrustError::new("rust_witness_runner_capture_invalid", "missing stderr"))?;
    let stdout = BASE64_STANDARD.decode(stdout).map_err(|error| {
        TrustError::new("rust_witness_runner_capture_invalid", error.to_string())
    })?;
    let stderr = BASE64_STANDARD.decode(stderr).map_err(|error| {
        TrustError::new("rust_witness_runner_capture_invalid", error.to_string())
    })?;
    if output.get("stdout_sha256").and_then(Value::as_str)
        != Some(raw_sha256(&stdout).to_hex().as_str())
        || output.get("stderr_sha256").and_then(Value::as_str)
            != Some(raw_sha256(&stderr).to_hex().as_str())
        || output.get("stdout_truncated").and_then(Value::as_bool).is_none()
        || output.get("stderr_truncated").and_then(Value::as_bool).is_none()
        || output.get("timed_out").and_then(Value::as_bool).is_none()
    {
        return Err(TrustError::new(
            "rust_witness_runner_capture_invalid",
            "runner stdout/stderr/status fields fail byte-level integrity checks",
        ));
    }
    let status = output.get("status").and_then(Value::as_str);
    let exit_code = output.get("exit_code").and_then(Value::as_i64);
    let timed_out = output.get("timed_out").and_then(Value::as_bool).unwrap_or(true);
    if status == Some("available") && (exit_code != Some(0) || timed_out) {
        return Err(TrustError::new(
            "rust_witness_runner_status_invalid",
            "available status requires a successful non-timeout Cargo test",
        ));
    }
    if status == Some("unavailable") && exit_code == Some(0) && !timed_out {
        return Err(TrustError::new(
            "rust_witness_runner_status_invalid",
            "unavailable status cannot carry a successful non-timeout Cargo test",
        ));
    }
    Ok(())
}

pub fn build_rust_witness_correspondence_request(
    record: &RustWitnessArtifactRecord,
    payload: &RustWitnessArtifactPayload,
    goal_target_prose_utf8: String,
    target_lean_utf8: String,
    negated_target_lean_utf8: String,
    negative_closure: LocalClosureRecord,
) -> Result<RustWitnessCorrespondenceRequest, TrustError> {
    if !rust_witness_execution_available(payload) {
        return Err(TrustError::new(
            "rust_witness_correspondence_execution_unavailable",
            "correspondence requires a successful Rust witness execution",
        ));
    }
    let execution_receipt = payload.execution_receipt.clone().ok_or_else(|| {
        TrustError::new(
            "rust_witness_correspondence_receipt_missing",
            "correspondence requires a complete execution receipt",
        )
    })?;
    let receipt_sha256: Sha256Digest = execution_receipt
        .get("receipt_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| TrustError::new("rust_witness_correspondence_receipt_invalid", "receipt lacks digest"))?
        .parse()?;
    let reviewed_digests = RustWitnessReviewedDigests {
        target_statement_sha256: raw_sha256(target_lean_utf8.as_bytes()),
        negated_statement_sha256: raw_sha256(negated_target_lean_utf8.as_bytes()),
        negative_closure_sha256: digest_local_closure(&negative_closure)?,
        artifact_sha256: record.artifact_sha256,
        receipt_sha256,
    };
    let mut request = RustWitnessCorrespondenceRequest {
        schema: RUST_WITNESS_CORRESPONDENCE_REQUEST_SCHEMA.into(),
        target_id: record.target_id.clone(),
        goal_target_prose_utf8,
        target_lean_utf8,
        negated_target_lean_utf8,
        checked_negative_proof_closure: negative_closure,
        artifact_source_utf8: payload.source_utf8.clone(),
        execution_receipt,
        reviewed_digests,
        request_sha256: Sha256Digest::ZERO,
    };
    let value = serde_json::to_value(&request).map_err(|error| {
        TrustError::new("rust_witness_correspondence_request_invalid", error.to_string())
    })?;
    request.request_sha256 = self_digest(DomainTag::RawArtifact, &value, "request_sha256")?;
    Ok(request)
}

/// Whether the frozen checked receipt contains the runner's successful result.
/// Failed builds/runs remain recorded evidence, but never enter Corr routing.
pub fn rust_witness_execution_available(payload: &RustWitnessArtifactPayload) -> bool {
    payload
        .execution_receipt
        .as_ref()
        .and_then(|receipt| receipt.get("parsed_stdout"))
        .is_some_and(|output| {
            output.get("status").and_then(Value::as_str) == Some("available")
                && output.get("exit_code").and_then(Value::as_i64) == Some(0)
                && output.get("timed_out").and_then(Value::as_bool) == Some(false)
        })
}

pub fn rust_witness_correspondence_verdict_digest(
    verdict: &RustWitnessCorrespondenceVerdict,
) -> Result<Sha256Digest, TrustError> {
    let value = serde_json::to_value(verdict).map_err(|error| {
        TrustError::new("rust_witness_correspondence_verdict_invalid", error.to_string())
    })?;
    self_digest(DomainTag::RawArtifact, &value, "verdict_sha256")
}

pub fn accept_rust_witness_correspondence(
    record: &mut RustWitnessArtifactRecord,
    payload: &mut RustWitnessArtifactPayload,
    request: &RustWitnessCorrespondenceRequest,
    verdict: RustWitnessCorrespondenceVerdict,
) -> Result<(), TrustError> {
    if !rust_witness_execution_available(payload) {
        return Err(TrustError::new(
            "rust_witness_correspondence_execution_unavailable",
            "correspondence cannot review an unavailable Rust witness execution",
        ));
    }
    if payload.correspondence_request.as_ref() != Some(request) {
        return Err(TrustError::new(
            "rust_witness_correspondence_request_mismatch",
            "verdict does not answer the frozen artifact correspondence request",
        ));
    }
    validate_correspondence_request_binding(record, payload, request)?;
    let request_value = serde_json::to_value(request).map_err(|error| {
        TrustError::new("rust_witness_correspondence_request_invalid", error.to_string())
    })?;
    let request_digest = self_digest(DomainTag::RawArtifact, &request_value, "request_sha256")?;
    if request.schema != RUST_WITNESS_CORRESPONDENCE_REQUEST_SCHEMA
        || request_digest != request.request_sha256
        || request.target_id != record.target_id
        || verdict.schema != RUST_WITNESS_CORRESPONDENCE_VERDICT_SCHEMA
        || verdict.request_sha256 != request.request_sha256
        || verdict.reviewed_digests != request.reviewed_digests
    {
        return Err(TrustError::new(
            "rust_witness_correspondence_binding_mismatch",
            "verdict is not bound to the exact issued artifact correspondence request",
        ));
    }
    let verdict_digest = rust_witness_correspondence_verdict_digest(&verdict)?;
    if verdict_digest != verdict.verdict_sha256 || verdict.reason.trim().is_empty() {
        return Err(TrustError::new(
            "rust_witness_correspondence_verdict_invalid",
            "verdict digest or reason is invalid",
        ));
    }
    record.correspondence = match verdict.decision {
        RustWitnessCorrespondenceDecision::Pass => RustWitnessCorrespondence::Pass {
            request_sha256: request.request_sha256,
            verdict_sha256: verdict.verdict_sha256,
            reason: verdict.reason.clone(),
        },
        RustWitnessCorrespondenceDecision::Fail => RustWitnessCorrespondence::Fail {
            request_sha256: request.request_sha256,
            verdict_sha256: verdict.verdict_sha256,
            reason: verdict.reason.clone(),
        },
    };
    payload.correspondence_request = Some(request.clone());
    payload.correspondence_verdict = Some(verdict);
    Ok(())
}

fn validate_correspondence_request_binding(
    record: &RustWitnessArtifactRecord,
    payload: &RustWitnessArtifactPayload,
    request: &RustWitnessCorrespondenceRequest,
) -> Result<(), TrustError> {
    let receipt = payload.execution_receipt.as_ref().ok_or_else(|| {
        TrustError::new(
            "rust_witness_correspondence_receipt_missing",
            "correspondence request lacks the frozen execution receipt",
        )
    })?;
    let receipt_sha256: Sha256Digest = receipt
        .get("receipt_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            TrustError::new(
                "rust_witness_correspondence_receipt_invalid",
                "frozen execution receipt lacks its digest",
            )
        })?
        .parse()?;
    let request_value = serde_json::to_value(request).map_err(|error| {
        TrustError::new("rust_witness_correspondence_request_invalid", error.to_string())
    })?;
    let request_digest = self_digest(DomainTag::RawArtifact, &request_value, "request_sha256")?;
    let expected_digests = RustWitnessReviewedDigests {
        target_statement_sha256: raw_sha256(request.target_lean_utf8.as_bytes()),
        negated_statement_sha256: raw_sha256(request.negated_target_lean_utf8.as_bytes()),
        negative_closure_sha256: digest_local_closure(
            &request.checked_negative_proof_closure,
        )?,
        artifact_sha256: record.artifact_sha256,
        receipt_sha256,
    };
    if request.schema != RUST_WITNESS_CORRESPONDENCE_REQUEST_SCHEMA
        || request.request_sha256 != request_digest
        || request.target_id != record.target_id
        || request.artifact_source_utf8 != payload.source_utf8
        || &request.execution_receipt != receipt
        || request.reviewed_digests != expected_digests
    {
        return Err(TrustError::new(
            "rust_witness_correspondence_request_binding_invalid",
            "correspondence request differs from its frozen target, closure, artifact, or receipt",
        ));
    }
    Ok(())
}

pub fn validate_rust_witness_state(
    records: &BTreeMap<ChallengeTargetId, RustWitnessArtifactRecord>,
    payloads: &BTreeMap<ChallengeTargetId, RustWitnessArtifactPayload>,
) -> Result<(), String> {
    if records.keys().ne(payloads.keys()) {
        return Err("Rust witness record and payload target sets differ".into());
    }
    for (target, record) in records {
        if record.schema != RUST_WITNESS_ARTIFACT_SCHEMA
            || &record.target_id != target
            || record.relative_path != rust_witness_relative_path(target)
            || record.artifact_sha256 == Sha256Digest::ZERO
            || record.pinned_crate_tree_sha256 == Sha256Digest::ZERO
            || record.freeze_episode_id.is_empty()
            || record.gate_episode_id.is_empty()
        {
            return Err(format!("Rust witness record for {} is invalid", target.as_str()));
        }
        if matches!(
            record.execution,
            RustWitnessExecution::Receipt {
                receipt_sha256: Sha256Digest::ZERO,
                ..
            } | RustWitnessExecution::Receipt {
                runner_sha256: Sha256Digest::ZERO,
                ..
            }
        ) {
            return Err(format!(
                "Rust witness execution identity for {} is empty",
                target.as_str()
            ));
        }
        let payload = &payloads[target];
        if raw_sha256(payload.source_utf8.as_bytes()) != record.artifact_sha256 {
            return Err(format!("Rust witness source for {} changed", target.as_str()));
        }
        if let RustWitnessExecution::Receipt {
            receipt_sha256,
            runner_sha256,
            timed_out,
            exit_code,
        } = record.execution
        {
            let receipt = payload.execution_receipt.as_ref().ok_or_else(|| {
                format!("Rust witness receipt for {} is absent", target.as_str())
            })?;
            let recomputed = self_digest(DomainTag::RawArtifact, receipt, "receipt_sha256")
                .map_err(|error| error.to_string())?;
            let parsed = receipt.get("parsed_stdout").ok_or_else(|| {
                format!("Rust witness receipt for {} has no runner output", target.as_str())
            })?;
            let parsed_exit_code = match parsed.get("exit_code") {
                Some(Value::Number(number)) => {
                    number.as_i64().and_then(|value| i32::try_from(value).ok())
                }
                Some(Value::Null) => None,
                _ => {
                    return Err(format!(
                        "Rust witness receipt for {} has an invalid exit code",
                        target.as_str()
                    ))
                }
            };
            if receipt.get("receipt_sha256").and_then(Value::as_str)
                != Some(receipt_sha256.to_hex().as_str())
                || recomputed != receipt_sha256
                || receipt.get("purpose").and_then(Value::as_str)
                    != Some(RUST_WITNESS_EXECUTION_PURPOSE)
                || receipt.get("tool_logical_id").and_then(Value::as_str)
                    != Some(RUST_WITNESS_RUNNER_LOGICAL_ID)
                || receipt.get("runner_sha256").and_then(Value::as_str)
                    != Some(runner_sha256.to_hex().as_str())
                || receipt.get("input") != Some(&payload.runner_request)
                || parsed.get("timed_out").and_then(Value::as_bool) != Some(timed_out)
                || parsed_exit_code != exit_code
            {
                return Err(format!("Rust witness receipt for {} changed", target.as_str()));
            }
            validate_runner_output(parsed, &payload.runner_request).map_err(|error| {
                format!(
                    "Rust witness runner output for {} is invalid: {error}",
                    target.as_str()
                )
            })?;
        } else if payload.execution_receipt.is_some() {
            return Err(format!("Rust witness payload for {} has an unrecorded receipt", target.as_str()));
        }
        match (&record.correspondence, &payload.correspondence_request, &payload.correspondence_verdict) {
            (RustWitnessCorrespondence::NotReviewed, None, None) => {}
            (RustWitnessCorrespondence::NotReviewed, Some(request), None) => {
                if !rust_witness_execution_available(payload) {
                    return Err(format!(
                        "Rust witness correspondence for {} has no successful execution",
                        target.as_str()
                    ));
                }
                validate_correspondence_request_binding(record, payload, request)
                    .map_err(|error| format!("Rust witness correspondence for {} is invalid: {error}", target.as_str()))?;
            }
            (RustWitnessCorrespondence::Pass { request_sha256, verdict_sha256, reason }
            | RustWitnessCorrespondence::Fail { request_sha256, verdict_sha256, reason }, Some(request), Some(verdict))
                if request.request_sha256 == *request_sha256
                    && verdict.verdict_sha256 == *verdict_sha256
                    && verdict.reason == *reason => {
                validate_correspondence_request_binding(record, payload, request)
                    .map_err(|error| format!("Rust witness correspondence for {} is invalid: {error}", target.as_str()))?;
                if rust_witness_correspondence_verdict_digest(verdict)
                    .map_err(|error| error.to_string())?
                    != verdict.verdict_sha256
                    || verdict.request_sha256 != request.request_sha256
                    || verdict.reviewed_digests != request.reviewed_digests
                    || reason.trim().is_empty()
                {
                    return Err(format!("Rust witness verdict for {} is not request-bound", target.as_str()));
                }
                if !rust_witness_execution_available(payload) {
                    return Err(format!(
                        "Rust witness correspondence for {} has no successful execution",
                        target.as_str()
                    ));
                }
            }
            _ => return Err(format!("Rust witness correspondence for {} is incoherent", target.as_str())),
        }
    }
    Ok(())
}

/// Rebind persisted artifact requests to the exact protocol registry and the
/// checked negative closure. This is called on every state validation/reload.
pub fn validate_rust_witness_protocol_bindings(state: &ProtocolState) -> Result<(), String> {
    validate_rust_witness_state(
        &state.trust_base.rust_witness_artifact_records,
        &state.trust_base.rust_witness_artifact_payloads,
    )?;
    for (target, record) in &state.trust_base.rust_witness_artifact_records {
        validate_rust_witness_declaration(
            state,
            &RustWitnessArtifactDeclaration {
                target_id: target.clone(),
                relative_path: record.relative_path.clone(),
            },
        )?;
        let payload = &state.trust_base.rust_witness_artifact_payloads[target];
        let Some(request) = &payload.correspondence_request else {
            continue;
        };
        let primary = state
            .configured_challenge_targets
            .get(target)
            .ok_or_else(|| format!("Rust witness primary {} is absent", target.as_str()))?;
        let refutation_id = crate::model::refutation_target_id(target);
        let refutation = state
            .configured_challenge_targets
            .get(&refutation_id)
            .ok_or_else(|| format!("Rust witness refutation {} is absent", refutation_id.as_str()))?;
        let closure = state
            .local_closure_records
            .get(&crate::model::NodeId::from(refutation.name.as_str()))
            .ok_or_else(|| {
                format!(
                    "Rust witness correspondence for {} lacks its checked negative closure",
                    target.as_str()
                )
            })?;
        if request.goal_target_prose_utf8 != primary.informal
            || request.target_lean_utf8 != primary.lean
            || request.negated_target_lean_utf8 != refutation.lean
            || &request.checked_negative_proof_closure != closure
        {
            return Err(format!(
                "Rust witness correspondence for {} differs from the exact GOAL target, T, negated T, or checked closure",
                target.as_str()
            ));
        }
    }
    Ok(())
}

pub fn artifact_package_members(
    records: &BTreeMap<ChallengeTargetId, RustWitnessArtifactRecord>,
    payloads: &BTreeMap<ChallengeTargetId, RustWitnessArtifactPayload>,
) -> Result<Vec<super::package::PackageArtifact>, TrustError> {
    validate_rust_witness_state(records, payloads)
        .map_err(|reason| TrustError::new("rust_witness_state_invalid", reason))?;
    let mut artifacts = Vec::new();
    for (target, record) in records {
        let key = rust_witness_target_key(target);
        let payload = &payloads[target];
        artifacts.push(super::package::PackageArtifact {
            role: "corroborating_rust_witness_record".into(),
            path: format!("artifacts/rust-witnesses/{key}/record.json"),
            bytes: canonical_json(record)?,
        });
        artifacts.push(super::package::PackageArtifact {
            role: "corroborating_rust_witness_source".into(),
            path: format!("artifacts/rust-witnesses/{key}/witness.rs"),
            bytes: payload.source_utf8.as_bytes().to_vec(),
        });
        artifacts.push(super::package::PackageArtifact {
            role: "corroborating_rust_witness_runner_request".into(),
            path: format!("artifacts/rust-witnesses/{key}/runner-request.json"),
            bytes: canonical_json_value(&payload.runner_request)?,
        });
        if let Some(receipt) = &payload.execution_receipt {
            artifacts.push(super::package::PackageArtifact {
                role: "corroborating_rust_witness_execution_receipt".into(),
                path: format!("artifacts/rust-witnesses/{key}/execution-receipt.json"),
                bytes: canonical_json_value(receipt)?,
            });
        }
        if let Some(request) = &payload.correspondence_request {
            artifacts.push(super::package::PackageArtifact {
                role: "corroborating_rust_witness_correspondence_request".into(),
                path: format!("artifacts/rust-witnesses/{key}/correspondence-request.json"),
                bytes: canonical_json(request)?,
            });
        }
        if let Some(verdict) = &payload.correspondence_verdict {
            artifacts.push(super::package::PackageArtifact {
                role: "corroborating_rust_witness_correspondence_verdict".into(),
                path: format!("artifacts/rust-witnesses/{key}/correspondence-verdict.json"),
                bytes: canonical_json(verdict)?,
            });
        }
    }
    Ok(artifacts)
}

pub fn artifact_gate_dossiers(
    records: &BTreeMap<ChallengeTargetId, RustWitnessArtifactRecord>,
    payloads: &BTreeMap<ChallengeTargetId, RustWitnessArtifactPayload>,
) -> Result<Vec<Value>, TrustError> {
    validate_rust_witness_state(records, payloads)
        .map_err(|reason| TrustError::new("rust_witness_state_invalid", reason))?;
    records
        .iter()
        .map(|(target, record)| {
            let payload = &payloads[target];
            Ok(json!({
                "label": "corroborating_evidence",
                "record": record,
                "source_utf8": payload.source_utf8,
                "fixed_command": "cargo test --manifest-path <copy>/Cargo.toml --test trellis_witness --locked --offline --target-dir <run-root>/target -- --nocapture",
                "runner_request": payload.runner_request,
                "execution_receipt": payload.execution_receipt,
                "correspondence_request": payload.correspondence_request,
                "correspondence_verdict": payload.correspondence_verdict,
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        refutation_target_id, ChallengeTargetKind, ChallengeTargetProvenance,
        ChallengeTargetSpec, StatementProvenance,
    };

    fn target_state() -> (ProtocolState, ChallengeTargetId) {
        let target = ChallengeTargetId::from("goal:generic_add_offset");
        let primary = ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: "add_offset_spec".into(),
            lean: "theorem add_offset_spec (v o : Int) : v + o = v + o := by".into(),
            informal: "bounded addition agrees with the source operation".into(),
            provenance: ChallengeTargetProvenance::default(),
            resolution: ChallengeResolution::Decide,
            statement_provenance: StatementProvenance::SeedPinned,
            ..ChallengeTargetSpec::default()
        };
        let twin_id = refutation_target_id(&target);
        let twin = ChallengeTargetSpec {
            kind: ChallengeTargetKind::Theorem,
            name: crate::model::refutation_node_name(&primary.name),
            lean: crate::model::refutation_statement(&primary).unwrap(),
            informal: crate::model::refutation_informal(&primary),
            provenance: ChallengeTargetProvenance::default(),
            resolution: ChallengeResolution::Prove,
            statement_provenance: StatementProvenance::KernelDerived,
            ..ChallengeTargetSpec::default()
        };
        let mut state = ProtocolState::default();
        state
            .configured_challenge_targets
            .insert(target.clone(), primary);
        state.configured_challenge_targets.insert(twin_id, twin);
        state
            .pv_live_polarity
            .insert(target.clone(), ChallengePolarity::Disprove);
        state.trust_base.advance_gate_episode_id = Some("episode-generic".into());
        (state, target)
    }

    #[test]
    fn target_path_is_derived_and_declaration_acceptance_is_live_disprove_only() {
        let (mut state, target) = target_state();
        let declaration = RustWitnessArtifactDeclaration {
            target_id: target.clone(),
            relative_path: rust_witness_relative_path(&target),
        };
        assert_eq!(
            declaration.relative_path,
            format!(
                "reference/rust-witnesses/target-{}/witness.rs",
                raw_sha256(target.as_str().as_bytes())
            )
        );
        validate_rust_witness_declaration(&state, &declaration).unwrap();
        let mut wrong = declaration.clone();
        wrong.relative_path.push_str(".other");
        assert!(validate_rust_witness_declaration(&state, &wrong).is_err());
        state.pv_live_polarity.clear();
        assert!(validate_rust_witness_declaration(&state, &declaration).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn witness_path_rejects_a_symlinked_parent_component() {
        use std::os::unix::fs::symlink;

        let (state, target) = target_state();
        let declaration = RustWitnessArtifactDeclaration {
            target_id: target.clone(),
            relative_path: rust_witness_relative_path(&target),
        };
        let work = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/rust-witness-path-test")
            .join(std::process::id().to_string());
        if work.exists() {
            fs::remove_dir_all(&work).unwrap();
        }
        let repo = work.join("repo");
        let outside = work.join("outside");
        fs::create_dir_all(repo.join("reference/rust-witnesses")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("witness.rs"), b"#[test] fn witness() {}\n").unwrap();
        symlink(
            &outside,
            repo.join("reference/rust-witnesses")
                .join(rust_witness_target_key(&target)),
        )
        .unwrap();
        let error = snapshot_rust_witness_source(
            &state,
            &repo,
            &declaration,
            raw_sha256(b"generic crate"),
            json!({}),
        )
        .unwrap_err();
        assert!(error.contains("symlink component"));
        fs::remove_dir_all(&work).unwrap();
    }

    fn reviewed_fixture(
    ) -> (
        RustWitnessArtifactRecord,
        RustWitnessArtifactPayload,
        RustWitnessCorrespondenceRequest,
    ) {
        let (state, target) = target_state();
        let relative_path = rust_witness_relative_path(&target);
        let mut record = RustWitnessArtifactRecord {
            schema: RUST_WITNESS_ARTIFACT_SCHEMA.into(),
            target_id: target.clone(),
            relative_path,
            artifact_sha256: raw_sha256(b"#[test] fn witness() {}\n"),
            pinned_crate_tree_sha256: raw_sha256(b"crate"),
            execution: RustWitnessExecution::Receipt {
                receipt_sha256: Sha256Digest::ZERO,
                runner_sha256: raw_sha256(b"runner"),
                timed_out: false,
                exit_code: Some(0),
            },
            correspondence: RustWitnessCorrespondence::NotReviewed,
            freeze_episode_id: "episode-generic".into(),
            gate_episode_id: "episode-generic".into(),
        };
        let (mut runner_request, mut runner_output) = runner_binding_fixture();
        for field in [
            "target_id",
            "relative_path",
            "artifact_sha256",
            "pinned_crate_tree_sha256",
        ] {
            let value = match field {
                "target_id" => json!(record.target_id),
                "relative_path" => json!(record.relative_path),
                "artifact_sha256" => json!(record.artifact_sha256),
                "pinned_crate_tree_sha256" => json!(record.pinned_crate_tree_sha256),
                _ => unreachable!(),
            };
            runner_request[field] = value.clone();
            runner_output[field] = value;
        }
        let mut receipt = json!({
            "schema": "trellis-checked-execution-receipt/v1",
            "purpose": RUST_WITNESS_EXECUTION_PURPOSE,
            "tool_logical_id": RUST_WITNESS_RUNNER_LOGICAL_ID,
            "runner_sha256": raw_sha256(b"runner"),
            "input": runner_request,
            "parsed_stdout": runner_output,
            "receipt_sha256": Sha256Digest::ZERO,
        });
        let receipt_digest = self_digest(DomainTag::RawArtifact, &receipt, "receipt_sha256").unwrap();
        receipt["receipt_sha256"] = json!(receipt_digest);
        if let RustWitnessExecution::Receipt { receipt_sha256, .. } = &mut record.execution {
            *receipt_sha256 = receipt_digest;
        }
        let mut payload = RustWitnessArtifactPayload {
            source_utf8: "#[test] fn witness() {}\n".into(),
            runner_request: receipt["input"].clone(),
            execution_receipt: Some(receipt),
            correspondence_request: None,
            correspondence_verdict: None,
        };
        let request = build_rust_witness_correspondence_request(
            &record,
            &payload,
            state.configured_challenge_targets[&target].informal.clone(),
            state.configured_challenge_targets[&target].lean.clone(),
            state.configured_challenge_targets[&refutation_target_id(&target)]
                .lean
                .clone(),
            LocalClosureRecord {
                node: crate::model::NodeId::from(
                    state.configured_challenge_targets[&refutation_target_id(&target)]
                        .name
                        .as_str(),
                ),
                ..LocalClosureRecord::default()
            },
        )
        .unwrap();
        payload.correspondence_request = Some(request.clone());
        (record, payload, request)
    }

    #[test]
    fn correspondence_checks_request_binding_but_not_lane_judgment() {
        let (mut record, mut payload, request) = reviewed_fixture();
        let lane = RustWitnessCorrespondenceLaneVerdict {
            request_sha256: request.request_sha256,
            same_witness_as_lean_disproof: false,
            invokes_pinned_crate_operation: false,
            observation_contradicts_goal_obligation: false,
            reviewed_digests: request.reviewed_digests.clone(),
            decision: RustWitnessCorrespondenceDecision::Pass,
            reason: "lane independently judged correspondence".into(),
        };
        let verdict = close_rust_witness_correspondence_verdict(&lane).unwrap();
        accept_rust_witness_correspondence(
            &mut record,
            &mut payload,
            &request,
            verdict,
        )
        .unwrap();
        assert!(matches!(record.correspondence, RustWitnessCorrespondence::Pass { .. }));

        let (mut other_record, mut other_payload, other_request) = reviewed_fixture();
        let mut mismatched = lane;
        mismatched.request_sha256 = raw_sha256(b"another request");
        let verdict = close_rust_witness_correspondence_verdict(&mismatched).unwrap();
        assert!(accept_rust_witness_correspondence(
            &mut other_record,
            &mut other_payload,
            &other_request,
            verdict,
        )
        .is_err());
    }

    fn runner_binding_fixture() -> (Value, Value) {
        let stdout = b"observed=32767\n";
        let stderr = b"Finished test\n";
        let input = json!({
            "target_id": "goal:generic_add_offset",
            "relative_path": "reference/rust-witnesses/target-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/witness.rs",
            "artifact_sha256": raw_sha256(b"source"),
            "pinned_crate_tree_sha256": raw_sha256(b"crate"),
            "cargo_lock_sha256": raw_sha256(b"lock"),
            "goal_sha256": raw_sha256(b"goal"),
            "rustc_vv": "rustc fixture\n",
        });
        let output = json!({
            "schema": RUST_WITNESS_RUNNER_OUTPUT_SCHEMA,
            "status": "available",
            "attempted": true,
            "target_id": input["target_id"],
            "relative_path": input["relative_path"],
            "artifact_sha256": input["artifact_sha256"],
            "pinned_crate_tree_sha256": input["pinned_crate_tree_sha256"],
            "cargo_lock_sha256": input["cargo_lock_sha256"],
            "goal_sha256": input["goal_sha256"],
            "rustc_vv": input["rustc_vv"],
            "timed_out": false,
            "exit_code": 0,
            "stdout_base64": BASE64_STANDARD.encode(stdout),
            "stderr_base64": BASE64_STANDARD.encode(stderr),
            "stdout_sha256": raw_sha256(stdout),
            "stderr_sha256": raw_sha256(stderr),
            "stdout_truncated": false,
            "stderr_truncated": false,
        });
        (input, output)
    }

    #[test]
    fn nested_capture_exit_and_truncation_fields_are_integrity_checked() {
        let (input, output) = runner_binding_fixture();
        validate_runner_output(&output, &input).unwrap();
        for (pointer, replacement) in [
            ("/stdout_base64", json!(BASE64_STANDARD.encode(b"changed"))),
            ("/stderr_sha256", json!(raw_sha256(b"changed"))),
            ("/stdout_truncated", json!("false")),
            ("/timed_out", json!(true)),
            ("/exit_code", json!(101)),
        ] {
            let mut changed = output.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                validate_runner_output(&changed, &input).is_err(),
                "mutation at {pointer} escaped validation"
            );
        }
    }

    #[test]
    fn pending_correspondence_survives_state_replay_byte_for_byte() {
        let (mut state, target) = target_state();
        let (record, payload, request) = reviewed_fixture();
        state.local_closure_records.insert(
            request.checked_negative_proof_closure.node.clone(),
            request.checked_negative_proof_closure.clone(),
        );
        state
            .trust_base
            .rust_witness_artifact_records
            .insert(target.clone(), record);
        state
            .trust_base
            .rust_witness_artifact_payloads
            .insert(target.clone(), payload);
        state.trust_base.pending_rust_witness_correspondence_target = Some(target.clone());
        validate_rust_witness_protocol_bindings(&state).unwrap();
        let bytes = canonical_json(&state).unwrap();
        let restored: ProtocolState = serde_json::from_slice(&bytes).unwrap();
        validate_rust_witness_protocol_bindings(&restored).unwrap();
        let replayed = restored.expected_request(7, crate::model::RequestKind::Corr);
        assert_eq!(
            replayed.rust_witness_artifact_correspondence,
            Some(request)
        );

        let mut drifted = restored;
        let payload = drifted
            .trust_base
            .rust_witness_artifact_payloads
            .get_mut(&target)
            .unwrap();
        let changed_request = payload.correspondence_request.as_mut().unwrap();
        changed_request.goal_target_prose_utf8.push_str(" drift");
        changed_request.request_sha256 = Sha256Digest::ZERO;
        changed_request.request_sha256 = self_digest(
            DomainTag::RawArtifact,
            &serde_json::to_value(&*changed_request).unwrap(),
            "request_sha256",
        )
        .unwrap();
        validate_rust_witness_state(
            &drifted.trust_base.rust_witness_artifact_records,
            &drifted.trust_base.rust_witness_artifact_payloads,
        )
        .unwrap();
        assert!(validate_rust_witness_protocol_bindings(&drifted).is_err());
    }

    #[test]
    fn gate_dossier_and_archive_projection_contain_all_frozen_payloads() {
        let (mut record, mut payload, request) = reviewed_fixture();
        let lane = RustWitnessCorrespondenceLaneVerdict {
            request_sha256: request.request_sha256,
            same_witness_as_lean_disproof: true,
            invokes_pinned_crate_operation: true,
            observation_contradicts_goal_obligation: true,
            reviewed_digests: request.reviewed_digests.clone(),
            decision: RustWitnessCorrespondenceDecision::Fail,
            reason: "the captured operation is not the GOAL operation".into(),
        };
        accept_rust_witness_correspondence(
            &mut record,
            &mut payload,
            &request,
            close_rust_witness_correspondence_verdict(&lane).unwrap(),
        )
        .unwrap();
        let target = record.target_id.clone();
        let records = BTreeMap::from([(target.clone(), record.clone())]);
        let payloads = BTreeMap::from([(target, payload.clone())]);
        let dossier = artifact_gate_dossiers(&records, &payloads).unwrap().remove(0);
        assert_eq!(dossier["record"], json!(&record));
        assert_eq!(dossier["source_utf8"], json!(&payload.source_utf8));
        assert_eq!(dossier["runner_request"], payload.runner_request);
        assert_eq!(dossier["execution_receipt"], json!(&payload.execution_receipt));
        assert_eq!(
            dossier["correspondence_request"],
            json!(&payload.correspondence_request)
        );
        assert_eq!(
            dossier["correspondence_verdict"],
            json!(&payload.correspondence_verdict)
        );
        let members = artifact_package_members(&records, &payloads).unwrap();
        assert_eq!(members.len(), 6);
        assert!(members.iter().any(|member| member.bytes == canonical_json(&record).unwrap()));
        assert!(members
            .iter()
            .any(|member| member.bytes == payload.source_utf8.as_bytes()));
    }
}

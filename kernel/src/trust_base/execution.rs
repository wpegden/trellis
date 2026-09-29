//! Pinned, shell-free execution for the exact-source validation class.

use super::canonical::{
    canonical_json_value, parse_json_strict, raw_sha256, self_digest, tagged_hash, DomainTag,
    Sha256Digest, TrustError,
};
use super::closure::VerifiedEvidenceClosure;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct ExecutionLimits {
    pub timeout: Duration,
    pub max_stdin_bytes: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(300),
            max_stdin_bytes: 64 * 1024 * 1024,
            max_stdout_bytes: 64 * 1024 * 1024,
            max_stderr_bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct PinnedExecutionRequest<'a> {
    pub runner_path: &'a Path,
    pub expected_runner_sha256: Sha256Digest,
    pub command_id: &'a str,
    pub working_directory: &'a Path,
    pub environment: &'a BTreeMap<String, String>,
    pub input: &'a Value,
    pub limits: ExecutionLimits,
}

#[derive(Clone, Debug)]
pub struct PinnedExecution {
    pub command: Value,
    pub command_sha256: Sha256Digest,
    pub actual_environment_sha256: Sha256Digest,
    pub runner_sha256: Sha256Digest,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_sha256: Sha256Digest,
    pub stderr_sha256: Sha256Digest,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub parsed_stdout: Option<Value>,
}

/// Opaque, self-digested evidence that one exact approved executable was run
/// over one canonical input.  Callers cannot construct this type without the
/// validating constructor below.
#[derive(Clone, Debug)]
pub struct CheckedExecutionReceipt {
    value: Value,
    digest: Sha256Digest,
}

impl CheckedExecutionReceipt {
    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn digest(&self) -> Sha256Digest {
        self.digest
    }

    pub fn parsed_output(&self) -> Option<&Value> {
        self.value.get("parsed_stdout").filter(|value| !value.is_null())
    }

    pub fn assessment_report(&self) -> Option<&Value> {
        self.value
            .get("assessment_report")
            .filter(|value| !value.is_null())
    }
}

/// Join the two independently checked generated-harness executions without
/// inventing a new evidence record kind. The adapted execution remains the
/// receipt's primary/pinned tool identity; the complete unadapted receipt and
/// the kernel-composed assessment report become self-digested members of that
/// same checked-execution receipt.
pub fn combine_assessment_receipts(
    mut adapted: CheckedExecutionReceipt,
    unadapted: Option<CheckedExecutionReceipt>,
    unadapted_leg: &str,
    assessment_report: Value,
) -> Result<CheckedExecutionReceipt, TrustError> {
    let object = adapted.value.as_object_mut().ok_or_else(|| {
        TrustError::new(
            "assessment_receipt_not_object",
            "adapted execution receipt is not an object",
        )
    })?;
    match (unadapted_leg, unadapted) {
        ("identity", None) => {}
        ("separate", Some(unadapted)) => {
            object.insert(
                "unadapted_execution_receipt".to_owned(),
                unadapted.value,
            );
        }
        _ => {
            return Err(TrustError::new(
                "assessment_unadapted_leg_invalid",
                "identity assessment must have no second receipt and separate assessment must have one",
            ));
        }
    }
    object.insert(
        "unadapted_leg".to_owned(),
        Value::String(unadapted_leg.to_owned()),
    );
    object.insert("assessment_report".to_owned(), assessment_report);
    object.insert(
        "receipt_sha256".to_owned(),
        Value::String(Sha256Digest::ZERO.to_string()),
    );
    let digest = self_digest(DomainTag::RawArtifact, &adapted.value, "receipt_sha256")?;
    adapted.value["receipt_sha256"] = Value::String(digest.to_string());
    adapted.digest = digest;
    Ok(adapted)
}

pub struct ApprovedExecutionRequest<'a> {
    pub evidence: &'a VerifiedEvidenceClosure,
    pub tool_logical_id: &'a str,
    pub purpose: &'a str,
    /// The surviving campaign binding (Q1, plan doc 32 Stage 3): the
    /// launch-acknowledgment digest replaces the retired journal-head
    /// predecessor.  The state field and its bootstrap writer land in
    /// Stage 4; a required-v1 pipeline construction with no binding
    /// refuses execution (fail-closed).
    pub launch_acknowledgment_sha256: Sha256Digest,
    pub execution: PinnedExecutionRequest<'a>,
}

pub const PHASE0_CHECKED_EXECUTION_RECEIPT_SCHEMA: &str =
    "trellis-phase0-checked-execution-receipt/v1";

pub struct Phase0ExecutionRequest<'a> {
    pub phase0_genesis_sha256: Sha256Digest,
    pub purpose: &'a str,
    pub execution: PinnedExecutionRequest<'a>,
}

/// Pre-bootstrap checked execution. It deliberately binds the nonzero Phase-0
/// genesis rather than fabricating a campaign evidence root or launch ack.
pub fn execute_phase0_checked_json(
    request: Phase0ExecutionRequest<'_>,
) -> Result<CheckedExecutionReceipt, TrustError> {
    if request.phase0_genesis_sha256 == Sha256Digest::ZERO {
        return Err(TrustError::new(
            "phase0_execution_genesis_invalid",
            "Phase-0 execution requires a nonzero genesis digest",
        ));
    }
    if request.purpose.is_empty() || request.purpose.chars().any(char::is_control) {
        return Err(TrustError::new(
            "phase0_execution_purpose_invalid",
            "Phase-0 execution purpose must be nonempty and control-free",
        ));
    }
    let input = request.execution.input.clone();
    let environment = Value::Object(
        request
            .execution
            .environment
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    );
    let execution = execute_pinned_json(request.execution)?;
    let mut value = serde_json::json!({
        "schema": PHASE0_CHECKED_EXECUTION_RECEIPT_SCHEMA,
        "purpose": request.purpose,
        "phase0_genesis_sha256": request.phase0_genesis_sha256,
        "runner_sha256": execution.runner_sha256,
        "command": execution.command,
        "command_sha256": execution.command_sha256,
        "environment": environment,
        "actual_environment_sha256": execution.actual_environment_sha256,
        "input": input,
        "timed_out": execution.timed_out,
        "exit_code": execution.exit_code,
        "stdout_base64": BASE64_STANDARD.encode(&execution.stdout),
        "stdout_byte_length": execution.stdout.len(),
        "stdout_sha256": execution.stdout_sha256,
        "stderr_base64": BASE64_STANDARD.encode(&execution.stderr),
        "stderr_byte_length": execution.stderr.len(),
        "stderr_sha256": execution.stderr_sha256,
        "stdout_truncated": execution.stdout_truncated,
        "stderr_truncated": execution.stderr_truncated,
        "parsed_stdout": if execution.timed_out
            || execution.stdout_truncated
            || execution.stderr_truncated
        {
            None
        } else {
            execution.parsed_stdout
        },
        "receipt_sha256": Sha256Digest::ZERO,
    });
    let digest = self_digest(DomainTag::RawArtifact, &value, "receipt_sha256")?;
    value["receipt_sha256"] = Value::String(digest.to_string());
    validate_phase0_execution_receipt(
        &value,
        request.phase0_genesis_sha256,
        request.purpose,
        execution.runner_sha256,
    )?;
    Ok(CheckedExecutionReceipt { value, digest })
}

pub fn validate_phase0_execution_receipt(
    value: &Value,
    expected_genesis: Sha256Digest,
    expected_purpose: &str,
    expected_runner: Sha256Digest,
) -> Result<Sha256Digest, TrustError> {
    if expected_genesis == Sha256Digest::ZERO || expected_runner == Sha256Digest::ZERO {
        return Err(TrustError::new(
            "phase0_execution_expected_binding_invalid",
            "expected genesis and runner digests must be nonzero",
        ));
    }
    let expected_fields = BTreeMap::from([
        ("actual_environment_sha256", ()),
        ("command", ()),
        ("command_sha256", ()),
        ("environment", ()),
        ("exit_code", ()),
        ("input", ()),
        ("parsed_stdout", ()),
        ("phase0_genesis_sha256", ()),
        ("purpose", ()),
        ("receipt_sha256", ()),
        ("runner_sha256", ()),
        ("schema", ()),
        ("stderr_base64", ()),
        ("stderr_byte_length", ()),
        ("stderr_sha256", ()),
        ("stderr_truncated", ()),
        ("stdout_base64", ()),
        ("stdout_byte_length", ()),
        ("stdout_sha256", ()),
        ("stdout_truncated", ()),
        ("timed_out", ()),
    ]);
    let object = value.as_object().ok_or_else(|| {
        TrustError::new("phase0_execution_receipt_invalid", "receipt must be an object")
    })?;
    if object.len() != expected_fields.len()
        || object.keys().any(|field| !expected_fields.contains_key(field.as_str()))
    {
        return Err(TrustError::new(
            "phase0_execution_receipt_invalid",
            "receipt has missing or unknown fields",
        ));
    }
    if value.get("schema").and_then(Value::as_str)
        != Some(PHASE0_CHECKED_EXECUTION_RECEIPT_SCHEMA)
        || value.get("purpose").and_then(Value::as_str) != Some(expected_purpose)
    {
        return Err(TrustError::new(
            "phase0_execution_receipt_identity_mismatch",
            "receipt schema or purpose differs",
        ));
    }
    let digest = self_digest(DomainTag::RawArtifact, value, "receipt_sha256")?;
    if string_field(value, "receipt_sha256")?.parse::<Sha256Digest>()? != digest {
        return Err(TrustError::new(
            "phase0_execution_receipt_digest_mismatch",
            "receipt self digest is invalid",
        ));
    }
    let genesis: Sha256Digest = string_field(value, "phase0_genesis_sha256")?.parse()?;
    let runner: Sha256Digest = string_field(value, "runner_sha256")?.parse()?;
    if genesis != expected_genesis || runner != expected_runner {
        return Err(TrustError::new(
            "phase0_execution_receipt_binding_mismatch",
            "receipt belongs to another genesis or runner",
        ));
    }
    for field in [
        "actual_environment_sha256",
        "command_sha256",
        "phase0_genesis_sha256",
        "receipt_sha256",
        "runner_sha256",
        "stderr_sha256",
        "stdout_sha256",
    ] {
        if string_field(value, field)?.parse::<Sha256Digest>()? == Sha256Digest::ZERO {
            return Err(TrustError::new(
                "phase0_execution_receipt_zero_digest",
                format!("receipt field {field} must not be the zero digest"),
            ));
        }
    }
    let environment = value.get("environment").ok_or_else(|| {
        TrustError::new("phase0_execution_receipt_invalid", "environment is absent")
    })?;
    let environment_digest = tagged_hash(
        DomainTag::EvidenceToolInput,
        &canonical_json_value(environment)?,
    );
    if string_field(value, "actual_environment_sha256")?.parse::<Sha256Digest>()?
        != environment_digest
    {
        return Err(TrustError::new(
            "phase0_execution_receipt_environment_mismatch",
            "environment digest is invalid",
        ));
    }
    let input = value.get("input").ok_or_else(|| {
        TrustError::new("phase0_execution_receipt_invalid", "input is absent")
    })?;
    let command = value.get("command").ok_or_else(|| {
        TrustError::new("phase0_execution_receipt_invalid", "command is absent")
    })?;
    if string_field(command, "runner_sha256")?.parse::<Sha256Digest>()? != runner
        || string_field(command, "environment_sha256")?.parse::<Sha256Digest>()?
            != environment_digest
        || string_field(command, "stdin_sha256")?.parse::<Sha256Digest>()?
            != raw_sha256(&canonical_json_value(input)?)
    {
        return Err(TrustError::new(
            "phase0_execution_receipt_command_binding_mismatch",
            "command does not bind runner, environment, and input",
        ));
    }
    let command_digest = tagged_hash(
        DomainTag::EvidenceToolInput,
        &canonical_json_value(command)?,
    );
    if string_field(value, "command_sha256")?.parse::<Sha256Digest>()? != command_digest {
        return Err(TrustError::new(
            "phase0_execution_receipt_command_digest_mismatch",
            "command digest is invalid",
        ));
    }
    let stdout = BASE64_STANDARD
        .decode(string_field(value, "stdout_base64")?)
        .map_err(|error| TrustError::new("phase0_execution_receipt_stdout_invalid", error.to_string()))?;
    let stderr = BASE64_STANDARD
        .decode(string_field(value, "stderr_base64")?)
        .map_err(|error| TrustError::new("phase0_execution_receipt_stderr_invalid", error.to_string()))?;
    let stdout_length = value.get("stdout_byte_length").and_then(Value::as_u64);
    let stderr_length = value.get("stderr_byte_length").and_then(Value::as_u64);
    if stdout_length != Some(stdout.len() as u64)
        || stderr_length != Some(stderr.len() as u64)
        || string_field(value, "stdout_sha256")?.parse::<Sha256Digest>()? != raw_sha256(&stdout)
        || string_field(value, "stderr_sha256")?.parse::<Sha256Digest>()? != raw_sha256(&stderr)
    {
        return Err(TrustError::new(
            "phase0_execution_receipt_output_mismatch",
            "captured output length or digest is invalid",
        ));
    }
    let timed_out = bool_field(value, "timed_out")?;
    let stdout_truncated = bool_field(value, "stdout_truncated")?;
    let stderr_truncated = bool_field(value, "stderr_truncated")?;
    let expected_parsed = if !timed_out && !stdout_truncated && !stderr_truncated {
        parse_json_strict(&stdout).ok()
    } else {
        None
    };
    if value.get("parsed_stdout") != Some(expected_parsed.as_ref().unwrap_or(&Value::Null)) {
        return Err(TrustError::new(
            "phase0_execution_receipt_parsed_output_mismatch",
            "parsed output is not the strict parse of captured stdout",
        ));
    }
    Ok(digest)
}

/// Execute one seed-pinned validator binary directly (never through a shell),
/// with a closed environment and canonical JSON stdin.  Timeout and output
/// limits are harness facts only: callers must not convert them into an
/// independent resource limit or qualification basis.
pub fn execute_pinned_json(
    request: PinnedExecutionRequest<'_>,
) -> Result<PinnedExecution, TrustError> {
    if request.command_id.is_empty() || request.command_id.chars().any(char::is_control) {
        return Err(TrustError::new(
            "execution_command_id_invalid",
            "command_id must be non-empty and contain no controls",
        ));
    }
    if request.limits.timeout.is_zero()
        || request.limits.max_stdout_bytes == 0
        || request.limits.max_stderr_bytes == 0
        || request.limits.max_stdin_bytes == 0
    {
        return Err(TrustError::new(
            "execution_limits_invalid",
            "timeout and capture limits must be positive",
        ));
    }
    let metadata = fs::symlink_metadata(request.runner_path).map_err(|error| {
        TrustError::new(
            "runner_unavailable",
            format!("{}: {error}", request.runner_path.display()),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(TrustError::new(
            "runner_not_regular_file",
            "pinned runner must be a non-symlink regular file",
        ));
    }
    let runner_bytes = fs::read(request.runner_path).map_err(|error| {
        TrustError::new("runner_unreadable", format!("runner read failed: {error}"))
    })?;
    let runner_sha256 = raw_sha256(&runner_bytes);
    if runner_sha256 != request.expected_runner_sha256 {
        return Err(TrustError::new(
            "runner_digest_mismatch",
            format!("runner hashes to {runner_sha256}"),
        ));
    }
    let working = fs::canonicalize(request.working_directory).map_err(|error| {
        TrustError::new(
            "runner_working_directory_invalid",
            format!("{}: {error}", request.working_directory.display()),
        )
    })?;
    let input_bytes = canonical_json_value(request.input)?;
    if input_bytes.len() > request.limits.max_stdin_bytes {
        return Err(TrustError::new(
            "runner_input_too_large",
            "canonical runner request exceeds the configured harness input limit",
        ));
    }
    let environment_value = Value::Object(
        request
            .environment
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    );
    for (key, value) in request.environment {
        if key.is_empty()
            || key.contains('=')
            || key.contains('\0')
            || value.contains('\0')
            || key.chars().any(char::is_control)
        {
            return Err(TrustError::new(
                "runner_environment_invalid",
                format!("invalid environment entry {key:?}"),
            ));
        }
    }
    let actual_environment_sha256 = tagged_hash(
        DomainTag::EvidenceToolInput,
        &canonical_json_value(&environment_value)?,
    );
    let working_utf8 = working.to_str().ok_or_else(|| {
        TrustError::new(
            "runner_working_directory_not_utf8",
            "canonical working directory must be UTF-8 for an auditable receipt",
        )
    })?;
    let timeout_millis = u64::try_from(request.limits.timeout.as_millis()).map_err(|_| {
        TrustError::new("execution_timeout_too_large", "timeout does not fit in u64 milliseconds")
    })?;
    let command_value = serde_json::json!({
        "schema": "trellis-pinned-command/v1",
        "command_id": request.command_id,
        "runner_sha256": runner_sha256,
        "arguments": [],
        "working_directory_utf8": working_utf8,
        "environment_sha256": actual_environment_sha256,
        "stdin_sha256": raw_sha256(&input_bytes),
        "timeout_millis": timeout_millis,
        "max_stdin_bytes": request.limits.max_stdin_bytes,
        "max_stdout_bytes": request.limits.max_stdout_bytes,
        "max_stderr_bytes": request.limits.max_stderr_bytes,
    });
    let command_sha256 = tagged_hash(
        DomainTag::EvidenceToolInput,
        &canonical_json_value(&command_value)?,
    );

    let mut child = Command::new(request.runner_path)
        .current_dir(working)
        .env_clear()
        .envs(request.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| TrustError::new("runner_spawn_failed", error.to_string()))?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        TrustError::new("runner_stdin_unavailable", "failed to open runner stdin")
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        TrustError::new("runner_stdout_unavailable", "failed to capture stdout")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        TrustError::new("runner_stderr_unavailable", "failed to capture stderr")
    })?;
    let stdout_limit = request.limits.max_stdout_bytes;
    let stderr_limit = request.limits.max_stderr_bytes;
    let stdin_thread = thread::spawn(move || {
        stdin.write_all(&input_bytes)?;
        drop(stdin);
        Ok::<_, std::io::Error>(())
    });
    let stdout_thread = thread::spawn(move || read_capped(stdout, stdout_limit));
    let stderr_thread = thread::spawn(move || read_capped(stderr, stderr_limit));
    let started = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (Some(status), false),
            Ok(None) if started.elapsed() < request.limits.timeout => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                child.kill().map_err(|error| {
                    TrustError::new("runner_timeout_kill_failed", error.to_string())
                })?;
                let status = child.wait().map_err(|error| {
                    TrustError::new("runner_timeout_wait_failed", error.to_string())
                })?;
                break (Some(status), true);
            }
            Err(error) => {
                let _ = child.kill();
                return Err(TrustError::new("runner_wait_failed", error.to_string()));
            }
        }
    };
    let (stdout, stdout_truncated) = stdout_thread
        .join()
        .map_err(|_| TrustError::new("runner_capture_panicked", "stdout reader panicked"))?
        .map_err(|error| TrustError::new("runner_stdout_read_failed", error.to_string()))?;
    stdin_thread
        .join()
        .map_err(|_| TrustError::new("runner_input_panicked", "stdin writer panicked"))?
        .map_err(|error| TrustError::new("runner_stdin_write_failed", error.to_string()))?;
    let (stderr, stderr_truncated) = stderr_thread
        .join()
        .map_err(|_| TrustError::new("runner_capture_panicked", "stderr reader panicked"))?
        .map_err(|error| TrustError::new("runner_stderr_read_failed", error.to_string()))?;
    let parsed_stdout = if !timed_out && !stdout_truncated {
        parse_json_strict(&stdout).ok()
    } else {
        None
    };
    Ok(PinnedExecution {
        command: command_value,
        command_sha256,
        actual_environment_sha256,
        runner_sha256,
        timed_out,
        exit_code: status.and_then(|status| status.code()),
        stdout_sha256: raw_sha256(&stdout),
        stderr_sha256: raw_sha256(&stderr),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        parsed_stdout,
    })
}

/// Run an evidence-manifest member and construct a receipt that embeds every
/// value needed for deterministic offline validation of the invocation and
/// captured result.
pub fn execute_approved_json(
    request: ApprovedExecutionRequest<'_>,
) -> Result<CheckedExecutionReceipt, TrustError> {
    if request.purpose.is_empty() || request.purpose.chars().any(char::is_control) {
        return Err(TrustError::new(
            "execution_purpose_invalid",
            "receipt purpose must be non-empty and contain no controls",
        ));
    }
    let leaf = request
        .evidence
        .leaves_by_logical_id
        .get(request.tool_logical_id)
        .ok_or_else(|| {
            TrustError::new(
                "execution_tool_not_approved",
                format!("{} is absent from the evidence closure", request.tool_logical_id),
            )
        })?;
    if leaf.raw_sha256 != request.execution.expected_runner_sha256 {
        return Err(TrustError::new(
            "execution_tool_digest_not_approved",
            "runner digest differs from its approved evidence leaf",
        ));
    }
    let input = request.execution.input.clone();
    let environment = Value::Object(
        request
            .execution
            .environment
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    );
    let execution = execute_pinned_json(request.execution)?;
    let mut value = serde_json::json!({
        "schema": "trellis-checked-execution-receipt/v1",
        "purpose": request.purpose,
        "tool_logical_id": request.tool_logical_id,
        "tool_kind": leaf.kind,
        "tool_relative_path": leaf.relative_path,
        "runner_sha256": execution.runner_sha256,
        "evidence_tool_input_root": request.evidence.evidence_tool_input_root,
        "launch_acknowledgment_sha256": request.launch_acknowledgment_sha256,
        "command": execution.command,
        "command_sha256": execution.command_sha256,
        "environment": environment,
        "actual_environment_sha256": execution.actual_environment_sha256,
        "input": input,
        "timed_out": execution.timed_out,
        "exit_code": execution.exit_code,
        "stdout_base64": BASE64_STANDARD.encode(&execution.stdout),
        "stderr_base64": BASE64_STANDARD.encode(&execution.stderr),
        "stdout_sha256": execution.stdout_sha256,
        "stderr_sha256": execution.stderr_sha256,
        "stdout_truncated": execution.stdout_truncated,
        "stderr_truncated": execution.stderr_truncated,
        "parsed_stdout": execution.parsed_stdout,
        "receipt_sha256": Sha256Digest::ZERO,
    });
    let digest = self_digest(DomainTag::RawArtifact, &value, "receipt_sha256")?;
    value["receipt_sha256"] = Value::String(digest.to_string());
    validate_execution_receipt(
        &value,
        request.evidence,
        request.purpose,
        request.launch_acknowledgment_sha256,
    )?;
    Ok(CheckedExecutionReceipt { value, digest })
}

/// Recompute an execution receipt without trusting any declared hash or parsed
/// output.  This validates evidence membership and byte-level I/O closure; the
/// purpose-specific semantic validator remains responsible for interpreting
/// the output.
pub fn validate_execution_receipt(
    value: &Value,
    evidence: &VerifiedEvidenceClosure,
    expected_purpose: &str,
    expected_launch_acknowledgment: Sha256Digest,
) -> Result<Sha256Digest, TrustError> {
    if value.get("schema").and_then(Value::as_str)
        != Some("trellis-checked-execution-receipt/v1")
        || value.get("purpose").and_then(Value::as_str) != Some(expected_purpose)
    {
        return Err(TrustError::new(
            "execution_receipt_identity_mismatch",
            "execution receipt has the wrong schema or purpose",
        ));
    }
    let digest = self_digest(DomainTag::RawArtifact, value, "receipt_sha256")?;
    let declared: Sha256Digest = string_field(value, "receipt_sha256")?.parse()?;
    if digest != declared {
        return Err(TrustError::new(
            "execution_receipt_digest_mismatch",
            "execution receipt self digest is invalid",
        ));
    }
    let launch_acknowledgment: Sha256Digest =
        string_field(value, "launch_acknowledgment_sha256")?.parse()?;
    if launch_acknowledgment != expected_launch_acknowledgment {
        return Err(TrustError::new(
            "execution_receipt_launch_binding_mismatch",
            "execution receipt belongs to another campaign launch acknowledgment",
        ));
    }
    let evidence_root: Sha256Digest =
        string_field(value, "evidence_tool_input_root")?.parse()?;
    if evidence_root != evidence.evidence_tool_input_root {
        return Err(TrustError::new(
            "execution_receipt_evidence_root_mismatch",
            "execution receipt belongs to another evidence closure",
        ));
    }
    let tool_id = string_field(value, "tool_logical_id")?;
    let leaf = evidence.leaves_by_logical_id.get(tool_id).ok_or_else(|| {
        TrustError::new(
            "execution_receipt_tool_not_approved",
            format!("{tool_id} is absent from the evidence closure"),
        )
    })?;
    let runner: Sha256Digest = string_field(value, "runner_sha256")?.parse()?;
    if runner != leaf.raw_sha256
        || value.get("tool_kind").and_then(Value::as_str) != Some(leaf.kind.as_str())
        || value.get("tool_relative_path").and_then(Value::as_str)
            != Some(leaf.relative_path.as_str())
    {
        return Err(TrustError::new(
            "execution_receipt_tool_binding_mismatch",
            "receipt tool identity differs from the approved evidence leaf",
        ));
    }
    let environment = value.get("environment").ok_or_else(|| {
        TrustError::new("execution_receipt_environment_missing", "receipt lacks environment")
    })?;
    let environment_digest = tagged_hash(
        DomainTag::EvidenceToolInput,
        &canonical_json_value(environment)?,
    );
    let declared_environment: Sha256Digest =
        string_field(value, "actual_environment_sha256")?.parse()?;
    if environment_digest != declared_environment {
        return Err(TrustError::new(
            "execution_receipt_environment_mismatch",
            "receipt environment digest is invalid",
        ));
    }
    let input = value.get("input").ok_or_else(|| {
        TrustError::new("execution_receipt_input_missing", "receipt lacks canonical input")
    })?;
    let command = value.get("command").ok_or_else(|| {
        TrustError::new("execution_receipt_command_missing", "receipt lacks command")
    })?;
    let command_runner: Sha256Digest = string_field(command, "runner_sha256")?.parse()?;
    let command_environment: Sha256Digest =
        string_field(command, "environment_sha256")?.parse()?;
    let command_input: Sha256Digest = string_field(command, "stdin_sha256")?.parse()?;
    if command_runner != runner
        || command_environment != environment_digest
        || command_input != raw_sha256(&canonical_json_value(input)?)
    {
        return Err(TrustError::new(
            "execution_receipt_command_binding_mismatch",
            "receipt command does not bind its runner, environment, and input",
        ));
    }
    let command_digest = tagged_hash(
        DomainTag::EvidenceToolInput,
        &canonical_json_value(command)?,
    );
    let declared_command: Sha256Digest = string_field(value, "command_sha256")?.parse()?;
    if command_digest != declared_command {
        return Err(TrustError::new(
            "execution_receipt_command_digest_mismatch",
            "receipt command digest is invalid",
        ));
    }
    let stdout = BASE64_STANDARD.decode(string_field(value, "stdout_base64")?).map_err(|error| {
        TrustError::new("execution_receipt_stdout_invalid", error.to_string())
    })?;
    let stderr = BASE64_STANDARD.decode(string_field(value, "stderr_base64")?).map_err(|error| {
        TrustError::new("execution_receipt_stderr_invalid", error.to_string())
    })?;
    if raw_sha256(&stdout) != string_field(value, "stdout_sha256")?.parse()?
        || raw_sha256(&stderr) != string_field(value, "stderr_sha256")?.parse()?
    {
        return Err(TrustError::new(
            "execution_receipt_output_digest_mismatch",
            "receipt stdout or stderr digest is invalid",
        ));
    }
    let parsed = value.get("parsed_stdout").ok_or_else(|| {
        TrustError::new("execution_receipt_parsed_output_missing", "parsed_stdout is absent")
    })?;
    let timed_out = bool_field(value, "timed_out")?;
    let truncated = bool_field(value, "stdout_truncated")?;
    let expected_parsed = if !timed_out && !truncated {
        parse_json_strict(&stdout).ok()
    } else {
        None
    };
    if parsed != expected_parsed.as_ref().unwrap_or(&Value::Null) {
        return Err(TrustError::new(
            "execution_receipt_parsed_output_mismatch",
            "parsed_stdout is not the exact strict parse of captured stdout",
        ));
    }
    Ok(digest)
}

fn read_capped(
    mut reader: impl Read,
    limit: usize,
) -> Result<(Vec<u8>, bool), std::io::Error> {
    let mut captured = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        let retain = remaining.min(count);
        captured.extend_from_slice(&buffer[..retain]);
        truncated |= retain < count;
    }
    Ok((captured, truncated))
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, TrustError> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        TrustError::new(
            "execution_receipt_field_invalid",
            format!("{field} must be a string"),
        )
    })
}

fn bool_field(value: &Value, field: &str) -> Result<bool, TrustError> {
    value.get(field).and_then(Value::as_bool).ok_or_else(|| {
        TrustError::new(
            "execution_receipt_field_invalid",
            format!("{field} must be a boolean"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::closure::VerifiedEvidenceLeaf;

    #[test]
    fn runner_digest_is_checked_before_execution() {
        let directory = tempfile::tempdir().unwrap();
        let runner = directory.path().join("not-a-runner");
        fs::write(&runner, b"not executable").unwrap();
        let error = execute_pinned_json(PinnedExecutionRequest {
            runner_path: &runner,
            expected_runner_sha256: "11".repeat(32).parse().unwrap(),
            command_id: "test",
            working_directory: directory.path(),
            environment: &BTreeMap::new(),
            input: &serde_json::json!({"test": true}),
            limits: ExecutionLimits::default(),
        })
        .unwrap_err();
        assert_eq!(error.code, "runner_digest_mismatch");
    }

    #[cfg(unix)]
    #[test]
    fn approved_receipt_recomputes_captured_output() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let runner = directory.path().join("runner");
        fs::write(
            &runner,
            b"#!/bin/sh\ninput=$(cat)\nprintf '{\"ok\":true}'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&runner).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&runner, permissions).unwrap();
        let runner_sha256 = raw_sha256(&fs::read(&runner).unwrap());
        let evidence_root: Sha256Digest = "11".repeat(32).parse().unwrap();
        let evidence = VerifiedEvidenceClosure {
            manifest_sha256: "22".repeat(32).parse().unwrap(),
            evidence_tool_input_root: evidence_root,
            file_count: 1,
            leaves_by_logical_id: BTreeMap::from([(
                "runner".to_owned(),
                VerifiedEvidenceLeaf {
                    kind: "executable".to_owned(),
                    relative_path: "runner".to_owned(),
                    byte_length: fs::metadata(&runner).unwrap().len(),
                    raw_sha256: runner_sha256,
                    dependency_ids: Vec::new(),
                },
            )]),
        };
        let launch_acknowledgment: Sha256Digest = "33".repeat(32).parse().unwrap();
        let receipt = execute_approved_json(ApprovedExecutionRequest {
            evidence: &evidence,
            tool_logical_id: "runner",
            purpose: "unit-test",
            launch_acknowledgment_sha256: launch_acknowledgment,
            execution: PinnedExecutionRequest {
                runner_path: &runner,
                expected_runner_sha256: runner_sha256,
                command_id: "unit-test",
                working_directory: directory.path(),
                environment: &BTreeMap::new(),
                input: &serde_json::json!({"input": 1}),
                limits: ExecutionLimits::default(),
            },
        })
        .unwrap();
        assert_eq!(receipt.parsed_output(), Some(&serde_json::json!({"ok": true})));

        let mut tampered = receipt.value().clone();
        tampered["stdout_sha256"] = Value::String("44".repeat(32));
        let digest = self_digest(DomainTag::RawArtifact, &tampered, "receipt_sha256").unwrap();
        tampered["receipt_sha256"] = Value::String(digest.to_string());
        let error = validate_execution_receipt(
            &tampered,
            &evidence,
            "unit-test",
            launch_acknowledgment,
        )
        .unwrap_err();
        assert_eq!(error.code, "execution_receipt_output_digest_mismatch");
    }
}

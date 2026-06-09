#[path = "../runtime_cli_observations.rs"]
mod runtime_cli_observations;

use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use runtime_cli_observations::{
    cleanup_preserving_step_result, current_tablet_node_names,
    ensure_worker_checker_support_available, evaluate_node_observation,
    evaluate_tablet_observation, final_cleanup_preserving_step_result, find_declaration,
    load_approved_axioms, observe_correspondence_fingerprints, observe_deviation_fingerprints,
    observe_node, observe_sketch_proof_nodes, observe_soundness_fingerprint_parts,
    observe_soundness_fingerprints, observe_tablet, observe_tablet_nodes,
    proof_worker_delta_step_result, relevant_new_errors, run_local_closure_axioms,
    scoped_tablet_step_result, theorem_target_edit_scope_step_result, validate_probe_present_nodes,
};
use trellis_kernel::{
    accept_worker_response, blocker_choice_ids, blocker_choices, diff_node_sets,
    direct_deps_from_repo, extract_tex_statement_items, normalize_audit_response,
    normalize_corr_response, normalize_node_lean_imports_on_disk, normalize_paper_response,
    normalize_review_response, normalize_sound_response, observe_paper_faithfulness_fingerprints,
    resolve_main_result_targets, snapshot_tablet_dir, sync_tablet_render_support_from_repo,
    validate_correspondence_result_data, validate_deviation_authorization_result_data,
    validate_paper_faithfulness_result_data, validate_soundness_result_data,
    validate_substantiveness_result_data, validate_trellis_audit_result_data,
    validate_trellis_reviewer_result_data, validate_trellis_stuck_math_audit_result_data,
    validate_trellis_worker_result_data, validate_trellis_worker_result_data_with_allowed_outcomes,
    ArtifactValidationOutput, AuditNormalizationInput, AxcheckStatus, CheckpointHookPayload,
    CheckpointSink, CorrNormalizationInput, CorrNormalizationOutput, CorrStatus, DeviationRequest,
    ErrorSummary, HumanChoice, HumanGateResponse, HydrateWorkerResponseInput, LegacyImportSummary,
    LocalClosureProbeOutput, LocalClosureRecord, NodeDifficulty, NodeId, NodeKind,
    NoopCheckpointSink, PaperNormalizationInput, PaperNormalizationOutput, ProtocolState,
    RawReviewPayload, RequestKind, ResolvedMainResultTargetsOutput, ResponseStatus,
    RetryOutcomeKind, RevalidationBatch, ReviewNormalizationInput, ReviewNormalizationOutput,
    ReviewResponse, RuntimeCheckpoint, RuntimeMetadata, RuntimePaths, RuntimeStepOutcome,
    SoundNormalizationInput, SoundNormalizationOutput, StuckMathAuditResponse, SupervisorRuntime,
    TabletSupportSyncOutput, TargetId, Update, WorkerAcceptanceInput, WorkerAcceptanceOutput,
    WorkerGateObservationInput, WorkerNormalizationInput, WorkerNormalizationOutput, WorkerOutcome,
    WorkerResponse, WorkerValidationExecutionPlanStep, WorkerValidationKind,
    WorkerValidationStepResult, WrapperAdapter, WrapperRequest, WrapperResponse,
    WrapperResponseMeta,
};

#[cfg(test)]
static KERNEL_CACHE_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn kernel_cache_env_test_guard() -> std::sync::MutexGuard<'static, ()> {
    KERNEL_CACHE_ENV_TEST_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

#[cfg(not(test))]
fn kernel_cache_env_test_guard() {}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum RuntimeCliRequest {
    Init {
        root: PathBuf,
        state: ProtocolState,
        metadata: Option<RuntimeMetadata>,
    },
    InitFromConfig {
        root: PathBuf,
        config_path: PathBuf,
    },
    ImportLegacy {
        root: PathBuf,
        config_path: PathBuf,
        state_path: Option<PathBuf>,
        tablet_path: Option<PathBuf>,
    },
    ResolveMainResultTargets {
        paper_path: Option<PathBuf>,
        raw_targets: Option<serde_json::Value>,
        raw_labels: Option<serde_json::Value>,
    },
    NormalizeWorker {
        input: WorkerNormalizationInput,
    },
    ValidateTrellisWorkerResult {
        raw_payload: serde_json::Value,
        #[serde(default)]
        acceptance_context: Option<serde_json::Value>,
    },
    ValidateTrellisReviewerResult {
        raw_payload: serde_json::Value,
    },
    /// Cleanup-v2 (audit Finding 1): shape-validate a `cleanup_audit_result_v1`
    /// artifact (the audit-burst JSON envelope).
    ValidateTrellisAuditResult {
        raw_payload: serde_json::Value,
    },
    ValidateTrellisStuckMathAuditResult {
        raw_payload: serde_json::Value,
    },
    BuildMalformedResponse {
        kind: RequestKind,
        request_id: u32,
        cycle: u32,
    },
    ValidatePaperFaithfulnessResult {
        raw_payload: serde_json::Value,
    },
    ValidateDeviationAuthorizationResult {
        raw_payload: serde_json::Value,
    },
    ValidateSubstantivenessResult {
        raw_payload: serde_json::Value,
    },
    ValidateCorrespondenceResult {
        raw_payload: serde_json::Value,
    },
    ValidateSoundnessResult {
        raw_payload: serde_json::Value,
        node_name: String,
    },
    CheckTrellisWorkerResult {
        repo_path: PathBuf,
        acceptance_context: serde_json::Value,
        raw_payload: serde_json::Value,
    },
    HydrateWorkerResponse {
        input: HydrateWorkerResponseInput,
    },
    CheckTrellisReviewerResult {
        review_request: serde_json::Value,
        raw_payload: serde_json::Value,
    },
    /// Cleanup-v2 (audit Finding 1): one-shot validate+normalize for the
    /// audit-burst artifact, parallel to `CheckTrellisReviewerResult`.
    CheckTrellisAuditResult {
        audit_request: serde_json::Value,
        raw_payload: serde_json::Value,
    },
    CheckTrellisStuckMathAuditResult {
        audit_request: serde_json::Value,
        raw_payload: serde_json::Value,
    },
    CheckNode {
        repo_path: PathBuf,
        node_name: String,
        expected_hash: Option<String>,
    },
    CheckTablet {
        repo_path: PathBuf,
    },
    SyncTabletSupport {
        repo_path: PathBuf,
    },
    ObserveSoundnessFingerprints {
        repo_path: PathBuf,
        #[serde(default)]
        nodes: BTreeSet<NodeId>,
    },
    CheckTabletScoped {
        repo_path: PathBuf,
        #[serde(default)]
        baseline_errors: Vec<String>,
        #[serde(default)]
        allowed_nodes: BTreeSet<NodeId>,
    },
    PrepareWorkerGate {
        repo_path: PathBuf,
        request: WrapperRequest,
        collect_observations: Option<bool>,
        /// Path to the configured paper.tex (relative to repo_path or
        /// absolute). Drives the substantiveness fingerprint's
        /// `paper_source_sha` field. When unset, the lane remains active
        /// via the own_tex + node_kind reopen triggers, but paper edits
        /// will not reopen substantiveness on any node — i.e. the
        /// paper-edit reopen defence is a no-op for that hydration cycle.
        #[serde(default)]
        paper_source_path: Option<PathBuf>,
    },
    ExecuteWorkerValidationPlan {
        input: ExecuteWorkerValidationPlanInput,
    },
    NormalizeCorr {
        input: CorrNormalizationInput,
    },
    NormalizePaper {
        input: PaperNormalizationInput,
    },
    NormalizeSound {
        input: SoundNormalizationInput,
    },
    NormalizeReview {
        input: ReviewNormalizationInput,
    },
    NormalizeHumanGate {
        request_id: u32,
        cycle: u32,
        raw_payload_text: String,
    },
    /// Pure-action snapshot probe for the kernel-rendered
    /// `worker_blocker_status_block`. Echoes the rendered Markdown body and
    /// the structured sidecar payload (when overflow) without writing the
    /// sidecar to disk — see `request_contracts::worker_blocker_status_block`.
    WorkerBlockerStatusBlock {
        request: WrapperRequest,
    },
    /// Pure-action snapshot probe for the kernel-rendered
    /// `review_blocker_choices_block`. Echoes the rendered Markdown body and
    /// the structured sidecar payload (when overflow); bridge owns sidecar
    /// IO and placeholder substitution — see
    /// `request_contracts::review_blocker_choices_block`.
    ReviewBlockerChoicesBlock {
        request: WrapperRequest,
    },
    AcceptWorker {
        input: WorkerAcceptanceInput,
    },
    CurrentRequest {
        root: PathBuf,
    },
    Show {
        root: PathBuf,
    },
    Step {
        root: PathBuf,
        response: Option<WrapperResponse>,
    },
    Run {
        root: PathBuf,
        max_steps: Option<u32>,
    },
    BridgeRequestPayload {
        repo_path: PathBuf,
        request: WrapperRequest,
    },
    ReplayToEventCount {
        root: PathBuf,
        stop_after_event_count: u64,
        #[serde(default)]
        dry_run_state_path: Option<PathBuf>,
        #[serde(default)]
        seed_checkpoint_path: Option<PathBuf>,
    },
    /// Audit followup #2 (Problem B): bridge-side relaunch after a
    /// supervisor restart needs to restore the worker repo's `Tablet/`
    /// to the captured `active_worker_base` snapshot before rebuilding
    /// the acceptance context. The in-flight request determines whether
    /// a worker is in flight; the snapshot directory determines whether
    /// there's anything to restore. No-op when either is absent.
    RestoreActiveWorkerBase {
        root: PathBuf,
    },
    /// Audit M-3 — controlled clear path for the checker-disagreement
    /// halt marker. Previously, the only way to clear was `rm` the JSON
    /// file directly; this command adds an auditable workflow with three
    /// modes:
    ///   * No flags + no probe → refused; operator must supply a probe
    ///     result or use --force.
    ///   * Probe supplied with `axcheck.agreed=true` → cleared (the
    ///     disagreement is resolved per re-observation).
    ///   * `force=true` → cleared unconditionally (operator-asserted
    ///     override; logged as such in the history file).
    /// Every attempt is logged to `<runtime_root>/halt_history/ack_log.jsonl`
    /// with timestamp + reason so an operator audit trail survives.
    AckHaltMarker {
        root: PathBuf,
        /// Operator-supplied free-text reason for the ack attempt; written
        /// verbatim to the history line.
        reason: String,
        /// Operator override flag; clears the marker unconditionally.
        #[serde(default)]
        force: bool,
        /// Optional re-observed probe result. When supplied, the kernel
        /// inspects `axcheck.agreed` + `status` + `errors` to decide
        /// whether the disagreement is resolved.
        #[serde(default)]
        probe_result: Option<LocalClosureProbeOutput>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum RuntimeCliResponse {
    Ok {
        state: ProtocolState,
        metadata: RuntimeMetadata,
        outcome: Option<RuntimeStepOutcome>,
        checkpoint: Option<RuntimeCheckpoint>,
        event_count: u64,
        steps_executed: u32,
        import_summary: Option<LegacyImportSummary>,
    },
    ResolveMainResultTargetsOk {
        output: ResolvedMainResultTargetsOutput,
    },
    NormalizeWorkerOk {
        output: WorkerNormalizationOutput,
    },
    ValidateTrellisWorkerResultOk {
        output: ArtifactValidationOutput,
    },
    ValidateTrellisReviewerResultOk {
        output: ArtifactValidationOutput,
    },
    /// Cleanup-v2 (audit Finding 1): response variant for
    /// `ValidateTrellisAuditResult`.
    ValidateTrellisAuditResultOk {
        output: ArtifactValidationOutput,
    },
    ValidateTrellisStuckMathAuditResultOk {
        output: ArtifactValidationOutput,
    },
    BuildMalformedResponseOk {
        output: WrapperResponse,
    },
    ValidatePaperFaithfulnessResultOk {
        output: ArtifactValidationOutput,
    },
    ValidateDeviationAuthorizationResultOk {
        output: ArtifactValidationOutput,
    },
    ValidateSubstantivenessResultOk {
        output: ArtifactValidationOutput,
    },
    ValidateCorrespondenceResultOk {
        output: ArtifactValidationOutput,
    },
    ValidateSoundnessResultOk {
        output: ArtifactValidationOutput,
    },
    CheckTrellisWorkerResultOk {
        output: CheckedTrellisWorkerResultOutput,
    },
    HydrateWorkerResponseOk {
        output: HydratedWorkerResponseOutput,
    },
    CheckTrellisReviewerResultOk {
        output: CheckedTrellisReviewerResultOutput,
    },
    /// Cleanup-v2 (audit Finding 1): response variant for
    /// `CheckTrellisAuditResult`.
    CheckTrellisAuditResultOk {
        output: CheckedTrellisAuditResultOutput,
    },
    CheckTrellisStuckMathAuditResultOk {
        output: CheckedTrellisAuditResultOutput,
    },
    CheckNodeOk {
        output: runtime_cli_observations::EvaluatedNode,
    },
    CheckTabletOk {
        output: runtime_cli_observations::EvaluatedTablet,
    },
    SyncTabletSupportOk {
        output: TabletSupportSyncOutput,
    },
    ObserveSoundnessFingerprintsOk {
        output: BTreeMap<NodeId, String>,
    },
    CheckTabletScopedOk {
        output: ScopedTabletCheckOutput,
    },
    PrepareWorkerGateOk {
        output: PreparedWorkerGateOutput,
    },
    ExecuteWorkerValidationPlanOk {
        output: ExecutedWorkerValidationPlanOutput,
    },
    NormalizeCorrOk {
        output: CorrNormalizationOutput,
    },
    NormalizePaperOk {
        output: PaperNormalizationOutput,
    },
    NormalizeSoundOk {
        output: SoundNormalizationOutput,
    },
    NormalizeReviewOk {
        output: ReviewNormalizationOutput,
    },
    NormalizeHumanGateOk {
        output: WrapperResponse,
    },
    /// Pure-action snapshot probe response for
    /// `RuntimeCliRequest::WorkerBlockerStatusBlock`.
    WorkerBlockerStatusBlockOk {
        output: trellis_kernel::WorkerBlockerStatusBlock,
    },
    /// Pure-action snapshot probe response for
    /// `RuntimeCliRequest::ReviewBlockerChoicesBlock`.
    ReviewBlockerChoicesBlockOk {
        output: trellis_kernel::ReviewBlockerChoicesBlock,
    },
    AcceptWorkerOk {
        output: WorkerAcceptanceOutput,
    },
    BridgeRequestPayloadOk {
        payload: serde_json::Value,
    },
    CurrentRequestOk {
        request: serde_json::Value,
        metadata: RuntimeMetadata,
    },
    ReplayToEventCountOk {
        event_count_applied: u64,
        cycle: u32,
        stage: String,
        in_flight_kind: Option<String>,
        in_flight_id: Option<u32>,
        state_path: PathBuf,
        log_truncated: bool,
        repo_reset_to_tag: Option<String>,
        repo_reset_error: Option<String>,
    },
    RestoreActiveWorkerBaseOk {
        restored: bool,
    },
    /// Audit M-3 — response for `RuntimeCliRequest::AckHaltMarker`.
    /// Surfaces the structured outcome so callers (operator CLIs,
    /// supervisor scripts) can branch on whether the marker was cleared
    /// or refused.
    AckHaltMarkerOk {
        outcome: trellis_kernel::runtime_cli_observations_halt::HaltMarkerAckOutcome,
    },
    Error {
        message: String,
    },
    InvalidRequest {
        message: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct PreparedWorkerGateOutput {
    request: serde_json::Value,
    validation_kind: String,
    worker_acceptance: serde_json::Value,
    active_node: String,
    held_target: String,
    authorized_nodes: std::collections::BTreeSet<NodeId>,
    configured_targets: std::collections::BTreeSet<TargetId>,
    current_present_nodes: std::collections::BTreeSet<NodeId>,
    current_proof_nodes: std::collections::BTreeSet<NodeId>,
    current_deps: std::collections::BTreeMap<NodeId, std::collections::BTreeSet<NodeId>>,
    current_target_claims: std::collections::BTreeMap<NodeId, std::collections::BTreeSet<TargetId>>,
    #[serde(default)]
    current_deviation_files: std::collections::BTreeMap<trellis_kernel::DeviationId, String>,
    #[serde(default)]
    current_node_deviation_claims:
        std::collections::BTreeMap<NodeId, std::collections::BTreeSet<trellis_kernel::DeviationId>>,
    #[serde(default)]
    current_paper_approved_fingerprints: std::collections::BTreeMap<TargetId, String>,
    // Paper-target-covering node set + their approved correspondence
    // fingerprints (JSON-encoded CorrespondenceFingerprint), feeding the
    // commit-time paper_target_corr_reopen_guard_errors check. See
    // `ExecuteWorkerValidationPlanInput` for details.
    #[serde(default)]
    approved_target_nodes: std::collections::BTreeSet<NodeId>,
    #[serde(default)]
    approved_corr_fingerprints: std::collections::BTreeMap<NodeId, String>,
    #[serde(default)]
    coarse_dag_nodes: std::collections::BTreeSet<NodeId>,
    #[serde(default)]
    current_coverage: std::collections::BTreeMap<TargetId, std::collections::BTreeSet<NodeId>>,
    #[serde(default)]
    current_paper_current_fingerprints: std::collections::BTreeMap<TargetId, String>,
    repo_path: String,
    before_snapshot: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    before_tablet_contents: std::collections::BTreeMap<String, String>,
    baseline_errors: Vec<String>,
    imports_before: Vec<String>,
    expected_active_hash: String,
    baseline_declaration_hashes: std::collections::BTreeMap<NodeId, String>,
    baseline_correspondence_hashes: std::collections::BTreeMap<NodeId, String>,
    /// Optional path to the configured paper file (relative to
    /// `repo_path` or absolute). Drives the substantiveness
    /// fingerprint's `paper_source_sha` field. When unset, the lane
    /// remains active via own_tex + node_kind reopen triggers, but
    /// paper edits will not reopen substantiveness on any node — the
    /// paper-edit reopen defence is a no-op for the cycle.
    #[serde(default)]
    paper_source_path: Option<PathBuf>,
    /// Node kinds at the time the gate was prepared. Used to populate
    /// the `node_kind` field in the substantiveness fingerprint during
    /// post-worker hydration.
    #[serde(default)]
    current_node_kinds: std::collections::BTreeMap<NodeId, trellis_kernel::NodeKind>,
    /// Patch C-R: pre-delta `live.open_nodes` snapshot captured by
    /// `prepare_worker_gate_output` via `open_nodes_from_repo`. Threaded
    /// into `ExecuteWorkerValidationPlanInput` so the helper-probe loop
    /// in `proof_worker_delta_step_result` can detect sorryd→sorry-free
    /// transitions for non-active proof_nodes. Empty default keeps
    /// pre-Patch-C-R replays compatible — empty causes the helper-probe
    /// loop to fire on new births only (the existing MCA-active-node
    /// coverage is unchanged either way).
    #[serde(default)]
    current_open_nodes: std::collections::BTreeSet<NodeId>,
}

#[derive(Debug, Deserialize)]
struct ExecuteWorkerValidationPlanInput {
    repo_path: PathBuf,
    active_node: Option<NodeId>,
    #[serde(default)]
    before_snapshot: BTreeMap<String, String>,
    #[serde(default)]
    before_tablet_contents: BTreeMap<String, String>,
    #[serde(default)]
    baseline_errors: Vec<String>,
    #[serde(default)]
    expected_active_hash: String,
    #[serde(default)]
    baseline_declaration_hashes: BTreeMap<NodeId, String>,
    #[serde(default)]
    baseline_correspondence_hashes: BTreeMap<NodeId, String>,
    #[serde(default)]
    current_present_nodes: BTreeSet<NodeId>,
    /// Patch C-N item 1: node-kind map (NodeId → NodeKind) used by the
    /// local-closure probe dep-kind validator inside
    /// `proof_worker_delta_step_result`. Defaults to empty for
    /// back-compat with standalone `ExecuteWorkerValidationPlan`
    /// requests that don't supply kinds; an empty map causes the
    /// validator to skip the kind refinement (membership-only check
    /// still fires), matching the pre-Patch-C-N behavior. The
    /// acceptance path always populates this from
    /// `WorkerAcceptanceContext.current_node_kinds`.
    #[serde(default)]
    current_node_kinds: BTreeMap<NodeId, trellis_kernel::NodeKind>,
    /// Patch C-R: pre-delta `live.open_nodes` snapshot (the kernel's
    /// authoritative sorryd set BEFORE this worker burst). Threaded
    /// through to `proof_worker_delta_step_result` so the helper-probe
    /// loop can identify sorryd→sorry-free transitions for non-active
    /// proof_nodes (new helper births + restructure-mode helper
    /// closes). `prepare_worker_gate_output` populates this from disk
    /// via `open_nodes_from_repo`. Empty default is back-compat with
    /// pre-Patch-C-R serialized payloads — empty means "no pre-delta
    /// open set known", which causes the helper-probe loop to fire on
    /// new births only (a strict subset of the post-patch behaviour;
    /// existing MCA-active-node coverage is unaffected).
    #[serde(default)]
    current_open_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    configured_targets: BTreeSet<TargetId>,
    #[serde(default)]
    current_deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    #[serde(default)]
    current_target_claims: BTreeMap<NodeId, BTreeSet<TargetId>>,
    // New fields feeding `paper_target_corr_reopen_guard_errors`. Populated
    // from the kernel's `state.approved_target_nodes()` and the subset of
    // `state.corr_approved_fingerprints` keyed by those nodes. When empty
    // the guard is a no-op (no covering nodes → nothing to protect).
    #[serde(default)]
    approved_target_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    approved_corr_fingerprints: BTreeMap<NodeId, String>,
    // Coarse DAG snapshot from the end of theorem-stating. Drives the
    // Restructure-vs-CoarseRestructure gate on active-node signature edits:
    // nodes in this set need coarse_restructure to change signatures;
    // proof-phase helpers added later can have signatures revised under
    // plain restructure.
    #[serde(default)]
    coarse_dag_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    authorized_nodes: BTreeSet<NodeId>,
    #[serde(default)]
    validation_execution_plan: Vec<WorkerValidationExecutionPlanStep>,
}

#[derive(Debug, Serialize)]
struct ExecutedWorkerValidationPlanOutput {
    step_results: Vec<WorkerValidationStepResult>,
    protected_semantic_change_nodes: BTreeSet<NodeId>,
}

#[derive(Debug, Serialize)]
struct ScopedTabletCheckOutput {
    ok: bool,
    errors: Vec<String>,
    warnings: Vec<String>,
    all_errors: Vec<String>,
    error_records: Vec<runtime_cli_observations::ErrorRecord>,
    allowed_nodes: Vec<NodeId>,
    build_output: String,
}

#[derive(Debug, Serialize)]
struct CheckedTrellisWorkerResultOutput {
    ok: bool,
    errors: Vec<String>,
    data: Option<serde_json::Value>,
    response: Option<serde_json::Value>,
    validation_step_results: Vec<WorkerValidationStepResult>,
    contract_errors: Vec<String>,
    validation_errors: Vec<String>,
    final_outcome: String,
}

#[derive(Debug, Serialize)]
struct CheckedTrellisReviewerResultOutput {
    ok: bool,
    errors: Vec<String>,
    data: Option<serde_json::Value>,
    response: Option<serde_json::Value>,
}

/// Cleanup-v2 (audit Finding 1): output for `CheckTrellisAuditResult`,
/// the one-shot validate+normalize path for audit-burst artifacts.
#[derive(Debug, Serialize)]
struct CheckedTrellisAuditResultOutput {
    ok: bool,
    errors: Vec<String>,
    data: Option<serde_json::Value>,
    response: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct HydratedWorkerResponseOutput {
    response: WorkerResponse,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct CheckedWorkerPayload {
    outcome: String,
    summary: String,
    comments: String,
    semantic_dep_updates: BTreeMap<String, BTreeSet<String>>,
    target_claim_updates: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    deviation_requests: BTreeMap<String, DeviationRequest>,
    #[serde(default)]
    node_deviation_claims: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    deviation_deletions: BTreeSet<String>,
    difficulty_updates: BTreeMap<String, String>,
    // Required+validated by `validate_trellis_worker_result_data` for
    // `outcome=needs_restructure`. Without this field on the deserialized
    // payload, the validator's extracted value was silently dropped here,
    // `accept_worker_response` built a default WorkerResponse with the
    // suggested set empty, and the reviewer's
    // `latest_worker_needs_restructure_suggested_nodes` snapshot was always
    // `[]` — defeating the whole point of the field (let the reviewer widen
    // scope concretely instead of guessing what the worker meant by "needs
    // broader repair"). Same allowlist-strip pattern as the recently-fixed
    // reviewer-side `request_sound_verifier_node_ids` (commit 78bc2b8).
    #[serde(default)]
    needs_restructure_suggested_nodes: Vec<String>,
}

impl Default for CheckedWorkerPayload {
    fn default() -> Self {
        Self {
            outcome: String::new(),
            summary: String::new(),
            comments: String::new(),
            semantic_dep_updates: BTreeMap::new(),
            target_claim_updates: BTreeMap::new(),
            deviation_requests: BTreeMap::new(),
            node_deviation_claims: BTreeMap::new(),
            deviation_deletions: BTreeSet::new(),
            difficulty_updates: BTreeMap::new(),
            needs_restructure_suggested_nodes: Vec::new(),
        }
    }
}

struct ProvidedResponseAdapter {
    response: Option<WrapperResponse>,
}

impl WrapperAdapter for ProvidedResponseAdapter {
    fn dispatch(&mut self, request: &WrapperRequest) -> Result<WrapperResponse, String> {
        let response = self
            .response
            .take()
            .ok_or_else(|| format!("missing response for in-flight request {:?}", request.kind))?;
        if response.request_id() != request.id || response.cycle() != request.cycle {
            return Err(format!(
                "provided response does not match in-flight request id={} cycle={}",
                request.id, request.cycle
            ));
        }
        Ok(response)
    }
}

struct ProcessCheckpointHook {
    command: PathBuf,
}

struct ProcessBridgeAdapter {
    command: PathBuf,
    config_path: PathBuf,
    repo_path: Option<PathBuf>,
    runtime_root: PathBuf,
}

enum RuntimeAdapter {
    Provided(ProvidedResponseAdapter),
    Process(ProcessBridgeAdapter),
}

impl WrapperAdapter for RuntimeAdapter {
    fn dispatch(&mut self, request: &WrapperRequest) -> Result<WrapperResponse, String> {
        match self {
            Self::Provided(adapter) => adapter.dispatch(request),
            Self::Process(adapter) => adapter.dispatch(request),
        }
    }
}

impl WrapperAdapter for ProcessBridgeAdapter {
    fn dispatch(&mut self, request: &WrapperRequest) -> Result<WrapperResponse, String> {
        let kernel_cmd = std::env::current_exe()
            .map_err(|err| format!("failed to resolve current kernel binary: {err}"))?;
        let input = json!({
            "config_path": self.config_path,
            "runtime_root": self.runtime_root,
            "request": bridge_request_payload(
                request,
                Some(&self.config_path),
                self.repo_path.as_deref(),
            )?,
        });
        let output = Command::new(&self.command)
            .env("TRELLIS_TRELLIS_KERNEL_CMD", &kernel_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                if let Some(stdin) = child.stdin.as_mut() {
                    let payload = serde_json::to_vec_pretty(&input)?;
                    stdin.write_all(&payload)?;
                }
                child.wait_with_output()
            })
            .map_err(|err| format!("failed to run bridge {}: {err}", self.command.display()))?;
        if output.status.success() {
            return serde_json::from_slice::<WrapperResponse>(&output.stdout)
                .map_err(|err| format!("failed to parse bridge response: {err}"));
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
            if let Some(message) = value.get("error").and_then(|v| v.as_str()) {
                return Err(format!(
                    "bridge {} failed: {}",
                    self.command.display(),
                    message
                ));
            }
        }
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit status {}", output.status)
        };
        Err(format!(
            "bridge {} failed: {}",
            self.command.display(),
            detail
        ))
    }
}

impl CheckpointSink for ProcessCheckpointHook {
    fn commit(&mut self, payload: &CheckpointHookPayload) -> Result<(), String> {
        let input = serde_json::to_vec_pretty(payload)
            .map_err(|err| format!("failed to serialize checkpoint payload: {err}"))?;
        let output = Command::new(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(&input)?;
                }
                child.wait_with_output()
            })
            .map_err(|err| {
                format!(
                    "failed to run checkpoint hook {}: {err}",
                    self.command.display()
                )
            })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("exit status {}", output.status)
        };
        Err(format!(
            "checkpoint hook {} failed: {}",
            self.command.display(),
            detail
        ))
    }
}

fn checkpoint_sink_from_env() -> Result<Option<ProcessCheckpointHook>, String> {
    let raw = std::env::var("TRELLIS_RUNTIME_CHECKPOINT_HOOK").unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(trimmed);
    if !path.exists() {
        return Err(format!(
            "checkpoint hook does not exist: {}",
            path.display()
        ));
    }
    Ok(Some(ProcessCheckpointHook { command: path }))
}

fn bridge_command_from_env() -> Result<Option<PathBuf>, String> {
    let raw = std::env::var("TRELLIS_RUNTIME_BRIDGE_CMD").unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(trimmed);
    if !path.exists() {
        return Err(format!("bridge command does not exist: {}", path.display()));
    }
    Ok(Some(path))
}

/// Generate a `fn(value) -> &'static str` that maps each enum variant
/// to its snake_case JSON tag. Used for serializing kernel enums into
/// the runtime_cli's hand-rolled JSON envelopes.
macro_rules! snake_name_fn {
    ($fn_name:ident, $type:ty, $( $variant:ident => $name:literal ),* $(,)?) => {
        fn $fn_name(value: $type) -> &'static str {
            match value {
                $( <$type>::$variant => $name, )*
            }
        }
    };
}

snake_name_fn!(request_kind_name, trellis_kernel::RequestKind,
    Worker => "worker",
    Paper => "paper",
    Corr => "corr",
    Sound => "sound",
    Review => "review",
    HumanGate => "human_gate",
    Audit => "audit",
    StuckMathAudit => "stuck_math_audit",
);

snake_name_fn!(phase_name, trellis_kernel::Phase,
    TheoremStating => "theorem_stating",
    ProofFormalization => "proof_formalization",
    Cleanup => "cleanup",
    Complete => "complete",
);

snake_name_fn!(task_mode_name, trellis_kernel::TaskMode,
    Global => "global",
    Targeted => "targeted",
    Local => "local",
    Restructure => "restructure",
    CoarseRestructure => "coarse_restructure",
    Cleanup => "cleanup",
);

snake_name_fn!(review_decision_name, trellis_kernel::ReviewDecisionKind,
    Continue => "continue",
    AdvancePhase => "advance_phase",
    NeedInput => "need_input",
    Done => "done",
);

snake_name_fn!(gate_kind_name, trellis_kernel::GateKind,
    None => "none",
    Advance => "advance",
    NeedInput => "need_input",
    ProtectedReapproval => "protected_reapproval",
);

snake_name_fn!(reset_choice_name, trellis_kernel::ResetChoice,
    None => "none",
    LastCommit => "last_commit",
    LastClean => "last_clean",
    TheoremStatingNode => "theorem_stating_node",
);

snake_name_fn!(difficulty_name, trellis_kernel::NodeDifficulty,
    Easy => "easy",
    Hard => "hard",
);

snake_name_fn!(worker_profile_name, trellis_kernel::WorkerProfile,
    None => "none",
    Theorem => "theorem",
    ProofEasy => "proof_easy",
    ProofHard => "proof_hard",
    Cleanup => "cleanup",
    FinalCleanup => "final_cleanup",
);

snake_name_fn!(node_kind_name, trellis_kernel::NodeKind,
    Preamble => "preamble",
    Definition => "definition",
    Proof => "proof",
);

snake_name_fn!(worker_validation_kind_name, trellis_kernel::WorkerValidationKind,
    None => "none",
    TheoremGlobal => "theorem_global",
    TheoremTargeted => "theorem_targeted",
    ProofEasy => "proof_easy",
    ProofLocal => "proof_local",
    ProofRestructure => "proof_restructure",
    ProofCoarseRestructure => "proof_coarse_restructure",
    Cleanup => "cleanup",
    FinalCleanup => "final_cleanup",
);

snake_name_fn!(worker_baseline_scope_name, trellis_kernel::WorkerBaselineScope,
    None => "none",
    AuthorizedNodes => "authorized_nodes",
    AllPresent => "all_present",
);

snake_name_fn!(worker_proof_delta_mode_name, trellis_kernel::WorkerProofDeltaMode,
    None => "none",
    Easy => "easy",
    Local => "local",
    Restructure => "restructure",
    CoarseRestructure => "coarse_restructure",
);

snake_name_fn!(scoped_tablet_allowed_nodes_mode_name, trellis_kernel::ScopedTabletAllowedNodesMode,
    Explicit => "explicit",
    AllPresent => "all_present",
    PreviousOrExplicit => "previous_or_explicit",
);

fn worker_validation_execution_plan_json(
    steps: &[trellis_kernel::WorkerValidationExecutionPlanStep],
) -> Vec<serde_json::Value> {
    steps
        .iter()
        .map(|step| match step {
            trellis_kernel::WorkerValidationExecutionPlanStep::TheoremTargetEditScope {
                target,
                initial_scope,
            } => json!({
                "kind": "theorem_target_edit_scope",
                "target": target,
                "initial_scope": initial_scope,
            }),
            trellis_kernel::WorkerValidationExecutionPlanStep::ScopedTablet {
                allowed_nodes_mode,
                explicit_nodes,
            } => json!({
                "kind": "scoped_tablet",
                "allowed_nodes_mode": scoped_tablet_allowed_nodes_mode_name(*allowed_nodes_mode),
                "explicit_nodes": explicit_nodes,
            }),
            trellis_kernel::WorkerValidationExecutionPlanStep::ProofEasyScope { active_node } => {
                json!({
                    "kind": "proof_easy_scope",
                    "active_node": active_node,
                })
            }
            trellis_kernel::WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                active_node,
                mode,
                authorized_nodes,
                protected_semantic_change_nodes,
                allow_new_obligations,
                must_close_active,
            } => json!({
                "kind": "proof_worker_delta",
                "active_node": active_node,
                "mode": worker_proof_delta_mode_name(*mode),
                "authorized_nodes": authorized_nodes,
                "protected_semantic_change_nodes": protected_semantic_change_nodes,
                "allow_new_obligations": allow_new_obligations,
                "must_close_active": must_close_active,
            }),
            trellis_kernel::WorkerValidationExecutionPlanStep::CleanupPreserving {} => json!({
                "kind": "cleanup_preserving",
            }),
            trellis_kernel::WorkerValidationExecutionPlanStep::FinalCleanupPreserving {
                task_kind,
                target_node,
                authorized_nodes,
                protected_statement_node_set,
            } => {
                // Cleanup-v2 Step 8: surface task-aware payload for the
                // runtime validator. Legacy lint-only mode encodes as
                // task_kind=null, target_node=null, both sets empty.
                json!({
                    "kind": "final_cleanup_preserving",
                    "task_kind": task_kind,
                    "target_node": target_node,
                    "authorized_nodes": authorized_nodes,
                    "protected_statement_node_set": protected_statement_node_set,
                })
            }
        })
        .collect()
}

/// Emit a top-level acceptance-progress phase header to stderr.
///
/// The `[acceptance]` prefix is for humans (and the calling agent) reading
/// the tool's stderr stream while `check_trellis_worker_result` runs;
/// stderr is unbuffered by default in Rust so each `eprintln!` flushes
/// immediately and shows up in the live tool-output stream.
fn acceptance_progress_phase(phase: usize, total: usize, name: &str) {
    eprintln!("[acceptance] phase {phase}/{total}: {name}");
}

/// Emit a sub-counter line under a top-level phase. `parent` is the
/// "phase {k}/{total}" prefix string (e.g. "2/6") so sub-events thread
/// back to the right parent phase.
fn acceptance_progress_sub(parent: &str, k: usize, sub_total: usize, detail: &str) {
    eprintln!("[acceptance]   {parent} sub {k}/{sub_total}: {detail}");
}

/// Phase name for a worker-validation execution-plan step variant.
fn validation_step_progress_name(step: &WorkerValidationExecutionPlanStep) -> &'static str {
    match step {
        WorkerValidationExecutionPlanStep::TheoremTargetEditScope { .. } => {
            "theorem_target_edit_scope"
        }
        WorkerValidationExecutionPlanStep::ScopedTablet { .. } => "scoped_tablet",
        WorkerValidationExecutionPlanStep::ProofEasyScope { .. } => "proof_easy_scope",
        WorkerValidationExecutionPlanStep::ProofWorkerDelta { .. } => "proof_worker_delta",
        WorkerValidationExecutionPlanStep::CleanupPreserving {} => "cleanup_preserving",
        WorkerValidationExecutionPlanStep::FinalCleanupPreserving { .. } => {
            "final_cleanup_preserving"
        }
    }
}

fn execute_worker_validation_plan(
    input: &ExecuteWorkerValidationPlanInput,
) -> Result<ExecutedWorkerValidationPlanOutput, String> {
    execute_worker_validation_plan_with_progress(input, None)
}

/// Variant that emits per-step progress sub-counters under the supplied
/// parent phase tag (e.g. "2/6"). Used by `check_trellis_worker_result_output`
/// so the caller agent sees `[acceptance]   2/6 sub k/n: <kind>` lines as
/// each plan step runs. Pass `None` for callers that don't want progress
/// (e.g. the standalone `ExecuteWorkerValidationPlan` request).
fn execute_worker_validation_plan_with_progress(
    input: &ExecuteWorkerValidationPlanInput,
    progress_parent: Option<&str>,
) -> Result<ExecutedWorkerValidationPlanOutput, String> {
    fn execution_failure_step_result(
        kind: &str,
        error: String,
        allowed_nodes: BTreeSet<NodeId>,
    ) -> WorkerValidationStepResult {
        WorkerValidationStepResult {
            kind: kind.to_string(),
            ok: false,
            detail: error.clone(),
            errors: vec![error],
            build_output: String::new(),
            allowed_nodes,
            local_closure_results: BTreeMap::new(),
        }
    }

    let total_steps = input.validation_execution_plan.len();
    let mut previous_allowed_nodes: BTreeSet<NodeId> = BTreeSet::new();
    let mut step_results: Vec<WorkerValidationStepResult> = Vec::new();
    let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
    for (step_idx, step) in input.validation_execution_plan.iter().enumerate() {
        if let Some(parent) = progress_parent {
            acceptance_progress_sub(
                parent,
                step_idx + 1,
                total_steps,
                validation_step_progress_name(step),
            );
        }
        match step {
            WorkerValidationExecutionPlanStep::TheoremTargetEditScope {
                target,
                initial_scope,
            } => {
                let resolved_target = target
                    .clone()
                    .or_else(|| input.active_node.clone())
                    .ok_or_else(|| {
                        "theorem_target_edit_scope step is missing target".to_string()
                    })?;
                let step_result = theorem_target_edit_scope_step_result(
                    &input.repo_path,
                    &resolved_target,
                    &input.before_snapshot,
                    initial_scope,
                );
                previous_allowed_nodes = step_result.allowed_nodes.clone();
                step_results.push(step_result);
            }
            WorkerValidationExecutionPlanStep::ScopedTablet {
                allowed_nodes_mode,
                explicit_nodes,
            } => {
                let allowed_nodes = match allowed_nodes_mode {
                    trellis_kernel::ScopedTabletAllowedNodesMode::AllPresent => {
                        let mut nodes = input.current_present_nodes.clone();
                        nodes.extend(current_tablet_node_names(&input.repo_path));
                        nodes
                    }
                    trellis_kernel::ScopedTabletAllowedNodesMode::PreviousOrExplicit => {
                        if previous_allowed_nodes.is_empty() {
                            explicit_nodes.clone()
                        } else {
                            previous_allowed_nodes.clone()
                        }
                    }
                    trellis_kernel::ScopedTabletAllowedNodesMode::Explicit => {
                        explicit_nodes.clone()
                    }
                };
                let observe_all_present = matches!(
                    allowed_nodes_mode,
                    trellis_kernel::ScopedTabletAllowedNodesMode::AllPresent
                );
                let step_result = scoped_tablet_step_result(
                    &input.repo_path,
                    &input.baseline_errors,
                    &allowed_nodes,
                    observe_all_present,
                )
                .unwrap_or_else(|err| {
                    execution_failure_step_result("scoped_tablet", err, allowed_nodes.clone())
                });
                step_results.push(step_result);
            }
            WorkerValidationExecutionPlanStep::ProofEasyScope { active_node } => {
                let resolved_active = active_node.as_ref().or(input.active_node.as_ref());
                let detail = format!(
                    "Legacy proof_easy_scope validation steps are retired; regenerate the worker request so the kernel emits proof_worker_delta with explicit allow_new_obligations and must_close_active gates{}.",
                    resolved_active
                        .map(|node| format!(" for active_node={node}"))
                        .unwrap_or_default()
                );
                step_results.push(WorkerValidationStepResult {
                    kind: "proof_easy_scope".to_string(),
                    ok: false,
                    detail: detail.clone(),
                    errors: vec![detail],
                    build_output: String::new(),
                    allowed_nodes: BTreeSet::new(),
                    local_closure_results: BTreeMap::new(),
                });
            }
            WorkerValidationExecutionPlanStep::ProofWorkerDelta {
                active_node,
                mode,
                authorized_nodes,
                protected_semantic_change_nodes: step_protected_semantic_change_nodes,
                allow_new_obligations,
                must_close_active,
            } => {
                let resolved_active = active_node
                    .clone()
                    .or_else(|| input.active_node.clone())
                    .unwrap_or_default();
                let resolved_authorized_nodes = if authorized_nodes.is_empty() {
                    input.authorized_nodes.clone()
                } else {
                    authorized_nodes.clone()
                };
                let step_result = proof_worker_delta_step_result(
                    &input.repo_path,
                    &resolved_active,
                    &input.before_snapshot,
                    &input.current_present_nodes,
                    &input.current_node_kinds,
                    // Patch C-R: pre-delta `live.open_nodes` so the helper-
                    // probe loop can detect sorryd→sorry-free transitions
                    // on non-active proof_nodes. `prepare_worker_gate_output`
                    // captures this from disk via `open_nodes_from_repo`
                    // before the worker burst runs.
                    &input.current_open_nodes,
                    &input.expected_active_hash,
                    *mode,
                    &input.approved_target_nodes,
                    &input.approved_corr_fingerprints,
                    &input.coarse_dag_nodes,
                    &resolved_authorized_nodes,
                    step_protected_semantic_change_nodes,
                    &mut protected_semantic_change_nodes,
                    *allow_new_obligations,
                    *must_close_active,
                )
                .unwrap_or_else(|err| {
                    execution_failure_step_result(
                        "proof_worker_delta",
                        err,
                        resolved_authorized_nodes.clone(),
                    )
                });
                step_results.push(step_result);
            }
            WorkerValidationExecutionPlanStep::CleanupPreserving {} => {
                let step_result = cleanup_preserving_step_result(
                    &input.repo_path,
                    &input.before_snapshot,
                    &input.before_tablet_contents,
                    &input.baseline_declaration_hashes,
                    &input.baseline_correspondence_hashes,
                    &input.configured_targets,
                    &input.current_deps,
                    &input.current_target_claims,
                    &input.current_present_nodes,
                )
                .unwrap_or_else(|err| {
                    execution_failure_step_result("cleanup_preserving", err, BTreeSet::new())
                });
                step_results.push(step_result);
            }
            WorkerValidationExecutionPlanStep::FinalCleanupPreserving {
                task_kind,
                target_node,
                authorized_nodes,
                protected_statement_node_set,
            } => {
                // Cleanup-v2 Step 9: task-aware validator dispatch. The
                // payload's `task_kind` selects between legacy lint-only
                // (None), LintFix (single-node), and Substitution
                // (target deletion + authorized importer edits)
                // semantics. See `final_cleanup_preserving_step_result`
                // for the per-mode constraint list.
                let step_result = final_cleanup_preserving_step_result(
                    &input.repo_path,
                    &input.before_snapshot,
                    &input.baseline_declaration_hashes,
                    &input.baseline_correspondence_hashes,
                    &input.current_present_nodes,
                    task_kind.as_ref(),
                    target_node.as_ref(),
                    authorized_nodes,
                    protected_statement_node_set,
                )
                .unwrap_or_else(|err| {
                    execution_failure_step_result("final_cleanup_preserving", err, BTreeSet::new())
                });
                step_results.push(step_result);
            }
        }
    }
    Ok(ExecutedWorkerValidationPlanOutput {
        step_results,
        protected_semantic_change_nodes,
    })
}

fn bridge_request_payload(
    request: &WrapperRequest,
    _config_path: Option<&std::path::Path>,
    repo_path: Option<&std::path::Path>,
) -> Result<serde_json::Value, String> {
    let mut request_owned = request.clone();
    trellis_kernel::populate_request_prompt_contracts(&mut request_owned, repo_path);
    let request = &request_owned;

    // Structural emit: every WrapperRequest field is rendered via serde's
    // Serialize derive. This eliminates the recurring bug class where a
    // newly-added WrapperRequest field is consumed downstream (validator,
    // legality check, prompt template) but silently absent from the bridge
    // JSON, deserializing back to its serde default and producing wrong
    // legality verdicts. Fields that have previously hit this class:
    // `substantiveness_verify_nodes`, `next_active_coarse` (commit 78bc2b8 prior bug),
    // `request_sound_verifier_node_ids` (commit 78bc2b8), and the
    // sibling cluster `sound_verifier_requestable_nodes`,
    // `sound_repair_ready_nodes`, `kernel_hinted_next_active_coarse_nodes`,
    // `proof_active_node_base_legal_candidates`,
    // `coarse_repair_blocker_carriers`, `resettable_theorem_stating_nodes`,
    // `cleanup_force_done_view`, etc. that this fix collectively addresses.
    //
    // The overlays below replace fields where the prompt expects a shape
    // serde doesn't produce by default: lowercase snake_case enum names
    // instead of PascalCase variants, sub-objects with computed `enabled`
    // overrides, helper-derived fields not on the struct. Adding a new
    // WrapperRequest field requires no change here — its serde rendering
    // flows through automatically.
    let mut payload =
        serde_json::to_value(request).map_err(|err| format!("serialize WrapperRequest: {err}"))?;
    let obj = payload
        .as_object_mut()
        .ok_or_else(|| "WrapperRequest must serialize to a JSON object".to_string())?;

    // Top-level enum-typed fields: prompt expects friendly snake_case.
    obj.insert("kind".into(), json!(request_kind_name(request.kind)));
    obj.insert("phase".into(), json!(phase_name(request.phase)));
    obj.insert("mode".into(), json!(task_mode_name(request.mode)));
    obj.insert("gate_kind".into(), json!(gate_kind_name(request.gate_kind)));
    obj.insert(
        "retry_outcome_kind".into(),
        json!(match request.retry_outcome_kind {
            RetryOutcomeKind::None => "None",
            RetryOutcomeKind::Invalid => "Invalid",
            RetryOutcomeKind::Stuck => "Stuck",
            RetryOutcomeKind::NeedsRestructure => "NeedsRestructure",
            RetryOutcomeKind::Transport => "Transport",
        }),
    );
    obj.insert(
        "allowed_decisions".into(),
        json!(request
            .allowed_decisions
            .iter()
            .map(|item| review_decision_name(*item))
            .collect::<Vec<_>>()),
    );
    obj.insert(
        "allowed_next_modes".into(),
        json!(request
            .allowed_next_modes
            .iter()
            .map(|item| task_mode_name(*item))
            .collect::<Vec<_>>()),
    );
    obj.insert(
        "allowed_resets".into(),
        json!(request
            .allowed_resets
            .iter()
            .map(|item| reset_choice_name(*item))
            .collect::<Vec<_>>()),
    );
    obj.insert(
        "current_node_kinds".into(),
        json!(request
            .current_node_kinds
            .iter()
            .map(|(node, kind)| (node.clone(), node_kind_name(*kind)))
            .collect::<std::collections::BTreeMap<_, _>>()),
    );

    // worker_context: prompt-side `enabled` is the kernel value OR the
    // request kind itself being Worker (legacy convenience); enum fields
    // need snake_case rendering. Rebuild the whole sub-object.
    obj.insert(
        "worker_context".into(),
        json!({
            "enabled": request.worker_context.enabled || request.kind == RequestKind::Worker,
            "active_difficulty": difficulty_name(request.worker_context.active_difficulty),
            "active_easy_attempts": request.worker_context.active_easy_attempts,
            "worker_profile": worker_profile_name(request.worker_context.worker_profile),
            "validation_kind": worker_validation_kind_name(request.worker_context.validation_kind),
            "authorized_nodes": request.worker_context.authorized_nodes,
            "protected_semantic_change_nodes": request.worker_context.protected_semantic_change_nodes,
            "next_context_mode": match request.worker_context.next_context_mode {
                trellis_kernel::WorkerContextMode::Resume => "resume",
                trellis_kernel::WorkerContextMode::Fresh => "fresh",
            },
            "paper_focus_ranges": request.worker_context.paper_focus_ranges,
            "work_style_hint": match request.worker_context.work_style_hint {
                trellis_kernel::WorkerWorkStyleHint::None => "none",
                trellis_kernel::WorkerWorkStyleHint::Restructure => "restructure",
            },
        }),
    );

    // worker_acceptance: same shape — computed `enabled` + enum renames.
    obj.insert(
        "worker_acceptance".into(),
        json!({
            "enabled": request.worker_acceptance.enabled || request.kind == RequestKind::Worker,
            "validation_kind": worker_validation_kind_name(request.worker_acceptance.validation_kind),
            "authorized_nodes": request.worker_acceptance.authorized_nodes,
            "protected_semantic_change_nodes": request.worker_acceptance.protected_semantic_change_nodes,
            "validation_execution_plan": worker_validation_execution_plan_json(
                &request.worker_acceptance.validation_execution_plan
            ),
            "require_explicit_target_claims_for_new_nodes": request.worker_acceptance.require_explicit_target_claims_for_new_nodes,
            "forbid_tablet_changes_when_stuck": request.worker_acceptance.forbid_tablet_changes_when_stuck,
            "observation_plan": {
                "capture_before_snapshot": request.worker_acceptance.observation_plan.capture_before_snapshot,
                "capture_before_tablet_contents": request.worker_acceptance.observation_plan.capture_before_tablet_contents,
                "capture_scoped_tablet_baseline_errors": request.worker_acceptance.observation_plan.capture_scoped_tablet_baseline_errors,
                "scoped_tablet_baseline_scope": worker_baseline_scope_name(request.worker_acceptance.observation_plan.scoped_tablet_baseline_scope),
                "capture_imports_before": request.worker_acceptance.observation_plan.capture_imports_before,
                "capture_expected_active_hash": request.worker_acceptance.observation_plan.capture_expected_active_hash,
                "capture_baseline_declaration_hashes": request.worker_acceptance.observation_plan.capture_baseline_declaration_hashes,
                "capture_baseline_correspondence_hashes": request.worker_acceptance.observation_plan.capture_baseline_correspondence_hashes,
            },
        }),
    );

    // Legacy alias kept for prompt-template compatibility — `audit_latch`
    // is the older name for what's now `stuck_math_audit`.
    obj.insert("audit_latch".into(), json!(request.stuck_math_audit));

    // Helper-derived fields not stored on WrapperRequest.
    obj.insert(
        "review_blocker_choices".into(),
        json!(blocker_choices(&request.blockers)),
    );
    obj.insert(
        "allowed_reset_blocker_ids".into(),
        json!(blocker_choice_ids(&request.allowed_reset_blockers)),
    );

    Ok(payload)
}

fn config_path_for_repo(repo_path: &Path) -> Result<PathBuf, String> {
    let trellis = repo_path.join("trellis.config.json");
    if trellis.is_file() {
        return Ok(trellis);
    }
    let legacy = repo_path.join("lagent.config.json");
    if legacy.is_file() {
        return Ok(legacy);
    }
    Err(format!(
        "no trellis.config.json or lagent.config.json found under {}",
        repo_path.display()
    ))
}

fn local_closure_axcheck_required_for_repo(repo_path: &Path) -> bool {
    match config_path_for_repo(repo_path) {
        Ok(config_path) => {
            trellis_kernel::resolve_local_closure_axcheck_enabled(&config_path).unwrap_or(true)
        }
        Err(_) => true,
    }
}

fn hydrated_bridge_request_payload(
    repo_path: &Path,
    request: &WrapperRequest,
) -> Result<serde_json::Value, String> {
    let config_path = config_path_for_repo(repo_path)?;
    let mut request_owned = request.clone();
    trellis_kernel::populate_request_prompt_contracts(&mut request_owned, Some(repo_path));
    if matches!(
        request_owned.kind,
        RequestKind::Paper | RequestKind::Corr | RequestKind::Sound
    ) {
        let bindings =
            trellis_kernel::resolve_request_verifier_bindings(&config_path, &request_owned)?;
        request_owned.paper_verify_lane_bindings = bindings.paper_verify_lane_bindings;
        request_owned.corr_verify_lane_bindings = bindings.corr_verify_lane_bindings;
        request_owned.sound_verify_lane_bindings = bindings.sound_verify_lane_bindings;
    } else {
        request_owned.paper_verify_lane_bindings.clear();
        request_owned.corr_verify_lane_bindings.clear();
        request_owned.sound_verify_lane_bindings.clear();
    }
    if matches!(
        request_owned.kind,
        RequestKind::Worker
            | RequestKind::Review
            | RequestKind::Audit
            | RequestKind::StuckMathAudit
    ) {
        let bindings =
            trellis_kernel::resolve_request_actor_bindings(&config_path, &request_owned)?;
        request_owned.worker_binding = bindings.worker_binding;
        request_owned.reviewer_binding = bindings.reviewer_binding;
        request_owned.stuck_math_audit_binding = bindings.stuck_math_audit_binding;
    } else {
        request_owned.worker_binding = trellis_kernel::BridgeActorBinding::default();
        request_owned.reviewer_binding = trellis_kernel::BridgeActorBinding::default();
        request_owned.stuck_math_audit_binding = trellis_kernel::BridgeActorBinding::default();
    }
    bridge_request_payload(&request_owned, Some(&config_path), Some(repo_path))
}

fn prepare_worker_gate_output(
    repo_path: &std::path::Path,
    request: &WrapperRequest,
    collect_observations: bool,
    paper_source_path: Option<&std::path::Path>,
) -> Result<PreparedWorkerGateOutput, String> {
    if collect_observations {
        sync_tablet_render_support_from_repo(repo_path)?;
    }
    let request_payload = bridge_request_payload(request, None, Some(repo_path))?;
    let worker_acceptance = request_payload
        .get("worker_acceptance")
        .cloned()
        .unwrap_or_else(|| json!({}));
    // Hash capture (`capture_expected_active_hash`,
    // `capture_baseline_declaration_hashes`) used to route through the
    // parser-based splitter and required oleans for the target nodes
    // to be materialised so Lean elaboration saw a consistent import
    // graph. Since 2026-05-12 those hashes are computed by
    // `filespec_split::declaration_hash_strict`, a pure-text scan with
    // no Lean / lake / olean access. The materialise step is therefore
    // unnecessary work; the other `ensure_worker_checker_support_available`
    // call-sites below (baseline lake-build errors, correspondence
    // fingerprints, paper-faithfulness fingerprints) genuinely need
    // olean state and stay.
    let observations =
        trellis_kernel::prepare_worker_gate_observations(&WorkerGateObservationInput {
            repo_path: repo_path.to_path_buf(),
            current_present_nodes: request.current_present_nodes.clone(),
            active_node: request.active_node.clone(),
            observation_plan: request.worker_acceptance.observation_plan.clone(),
            collect_observations,
        })?;
    let baseline_errors = if collect_observations
        && request
            .worker_acceptance
            .observation_plan
            .capture_scoped_tablet_baseline_errors
    {
        let baseline_allowed_nodes = match request
            .worker_acceptance
            .observation_plan
            .scoped_tablet_baseline_scope
        {
            trellis_kernel::WorkerBaselineScope::AllPresent => {
                request.current_present_nodes.clone()
            }
            trellis_kernel::WorkerBaselineScope::AuthorizedNodes => {
                request.worker_acceptance.authorized_nodes.clone()
            }
            trellis_kernel::WorkerBaselineScope::None => BTreeSet::new(),
        };
        ensure_worker_checker_support_available(repo_path, &baseline_allowed_nodes)?;
        let tablet_observation = match request
            .worker_acceptance
            .observation_plan
            .scoped_tablet_baseline_scope
        {
            trellis_kernel::WorkerBaselineScope::AllPresent => observe_tablet(repo_path)?,
            trellis_kernel::WorkerBaselineScope::AuthorizedNodes => {
                observe_tablet_nodes(repo_path, &baseline_allowed_nodes)?
            }
            trellis_kernel::WorkerBaselineScope::None => {
                observe_tablet_nodes(repo_path, &baseline_allowed_nodes)?
            }
        };
        let evaluated = evaluate_tablet_observation(repo_path, &tablet_observation);
        relevant_new_errors(&evaluated, &[], &baseline_allowed_nodes)
    } else {
        Vec::new()
    };
    let baseline_correspondence_hashes = if collect_observations
        && request
            .worker_acceptance
            .observation_plan
            .capture_baseline_correspondence_hashes
    {
        ensure_worker_checker_support_available(repo_path, &request.current_present_nodes)?;
        observe_correspondence_fingerprints(repo_path, &request.current_present_nodes)?
    } else {
        BTreeMap::new()
    };
    let current_coverage = coverage_from_target_claims(
        &request.configured_targets,
        &request.current_target_claims,
        &request.current_present_nodes,
    );
    let proof_validation_kind = matches!(
        request.worker_acceptance.validation_kind,
        WorkerValidationKind::ProofEasy
            | WorkerValidationKind::ProofLocal
            | WorkerValidationKind::ProofRestructure
            | WorkerValidationKind::ProofCoarseRestructure
    );
    // `current_protected_fingerprints` used to be computed here from
    // `request.protected_snapshot.keys()` for the post-hoc worker-honesty
    // loop that ran after worker response ingest. Both have been removed —
    // covering-node protection now lands at worker-commit time via the
    // `paper_target_corr_reopen_guard_errors` check, so no pre-worker
    // snapshot is needed.
    let current_paper_current_fingerprints = if collect_observations && proof_validation_kind {
        ensure_worker_checker_support_available(repo_path, &request.current_present_nodes)?;
        let covering_union: BTreeSet<NodeId> =
            current_coverage.values().flatten().cloned().collect();
        let lean_relevant_per_covering =
            runtime_cli_observations::observe_lean_relevant_definition_descendants_per_node(
                repo_path,
                &covering_union,
            )?;
        observe_paper_faithfulness_fingerprints(
            repo_path,
            &request.configured_targets,
            &request.current_target_claims,
            &request.current_present_nodes,
            &request.current_paper_approved_fingerprints,
            &lean_relevant_per_covering,
        )
    } else {
        BTreeMap::new()
    };
    // Patch C-R: capture pre-delta `open_nodes` (the kernel's
    // authoritative sorryd set BEFORE the worker burst). Sourced from
    // disk via `open_nodes_from_repo` — `has_sorry` over each present
    // node's `.lean` content. Matches the kernel's own `live.open_nodes`
    // computation (worker_normalization::normalize_worker_response uses
    // the same helper post-delta). Empty when observations are skipped
    // (collect_observations=false, e.g. replay paths) to preserve the
    // existing back-compat surface.
    let current_open_nodes = if collect_observations {
        trellis_kernel::open_nodes_from_repo(repo_path, &request.current_present_nodes)
    } else {
        BTreeSet::new()
    };
    Ok(PreparedWorkerGateOutput {
        request: request_payload,
        validation_kind: worker_validation_kind_name(request.worker_acceptance.validation_kind)
            .to_string(),
        worker_acceptance,
        active_node: request
            .active_node
            .as_ref()
            .map(|n| n.to_string())
            .unwrap_or_default(),
        held_target: request
            .held_target
            .as_ref()
            .map(|n| n.to_string())
            .unwrap_or_default(),
        authorized_nodes: request.worker_acceptance.authorized_nodes.clone(),
        configured_targets: request.configured_targets.clone(),
        current_present_nodes: request.current_present_nodes.clone(),
        current_proof_nodes: request.current_proof_nodes.clone(),
        current_deps: request.current_deps.clone(),
        current_target_claims: request.current_target_claims.clone(),
        current_deviation_files: request.current_deviation_files.clone(),
        current_node_deviation_claims: request.node_deviation_claims.clone(),
        current_paper_approved_fingerprints: request.current_paper_approved_fingerprints.clone(),
        approved_target_nodes: request.approved_target_nodes.clone(),
        approved_corr_fingerprints: request.approved_corr_fingerprints.clone(),
        coarse_dag_nodes: request.coarse_dag_nodes.clone(),
        current_coverage,
        current_paper_current_fingerprints,
        repo_path: repo_path.display().to_string(),
        before_snapshot: observations.before_snapshot,
        before_tablet_contents: observations.before_tablet_contents,
        baseline_errors,
        imports_before: observations.imports_before,
        expected_active_hash: observations.expected_active_hash,
        baseline_declaration_hashes: observations.baseline_declaration_hashes,
        baseline_correspondence_hashes,
        paper_source_path: paper_source_path.map(|p| p.to_path_buf()),
        current_node_kinds: request.current_node_kinds.clone(),
        current_open_nodes,
    })
}

fn check_node_output(
    repo_path: &std::path::Path,
    node_name: &str,
    expected_hash: Option<&str>,
) -> Result<runtime_cli_observations::EvaluatedNode, String> {
    let requested_nodes = BTreeSet::from([NodeId::from(node_name)]);
    ensure_worker_checker_support_available(repo_path, &requested_nodes)?;
    let observation = observe_node(repo_path, node_name)?;
    Ok(evaluate_node_observation(
        repo_path,
        &observation,
        expected_hash,
    ))
}

fn check_tablet_output(
    repo_path: &std::path::Path,
) -> Result<runtime_cli_observations::EvaluatedTablet, String> {
    ensure_worker_checker_support_available(repo_path, &BTreeSet::new())?;
    let observation = observe_tablet(repo_path)?;
    Ok(evaluate_tablet_observation(repo_path, &observation))
}

fn check_tablet_scoped_output(
    repo_path: &std::path::Path,
    baseline_errors: &[String],
    allowed_nodes: &BTreeSet<NodeId>,
) -> Result<ScopedTabletCheckOutput, String> {
    ensure_worker_checker_support_available(repo_path, allowed_nodes)?;
    let observation = observe_tablet_nodes(repo_path, allowed_nodes)?;
    let evaluated = evaluate_tablet_observation(repo_path, &observation);
    let errors = relevant_new_errors(&evaluated, baseline_errors, allowed_nodes);
    Ok(ScopedTabletCheckOutput {
        ok: errors.is_empty(),
        errors,
        warnings: evaluated.warnings,
        all_errors: evaluated.errors,
        error_records: evaluated.error_records,
        allowed_nodes: allowed_nodes.iter().cloned().collect(),
        build_output: evaluated.build_output,
    })
}

fn worker_outcome_from_checked_payload(raw: &str) -> Result<trellis_kernel::WorkerOutcome, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "valid" => Ok(trellis_kernel::WorkerOutcome::Valid),
        "invalid" => Ok(trellis_kernel::WorkerOutcome::Invalid),
        "stuck" => Ok(trellis_kernel::WorkerOutcome::Stuck),
        "needs_restructure" => Ok(trellis_kernel::WorkerOutcome::NeedsRestructure),
        _ => Err(
            "worker outcome must be one of ['valid', 'invalid', 'stuck', 'needs_restructure']"
                .to_string(),
        ),
    }
}

fn difficulty_updates_from_checked_payload(
    raw: &BTreeMap<String, String>,
) -> BTreeMap<NodeId, Update<NodeDifficulty>> {
    raw.iter()
        .map(|(node, value)| {
            let update = if value.eq_ignore_ascii_case("easy") {
                Update::Set(NodeDifficulty::Easy)
            } else {
                Update::Set(NodeDifficulty::Hard)
            };
            (NodeId::from(node), update)
        })
        .collect()
}

fn request_id_from_value(request: &serde_json::Value) -> u32 {
    request
        .get("id")
        .and_then(|value| value.as_u64())
        .map(|value| value as u32)
        .unwrap_or_default()
}

fn cycle_from_value(request: &serde_json::Value) -> u32 {
    request
        .get("cycle")
        .and_then(|value| value.as_u64())
        .map(|value| value as u32)
        .unwrap_or_default()
}

fn node_kinds_from_request_value(
    request: &serde_json::Value,
) -> Result<BTreeMap<NodeId, trellis_kernel::NodeKind>, String> {
    serde_json::from_value(
        request
            .get("current_node_kinds")
            .cloned()
            .unwrap_or_else(|| json!({})),
    )
    .map_err(|err| format!("worker acceptance context has invalid current_node_kinds: {err}"))
}

fn target_claims_after_updates(
    configured_targets: &BTreeSet<TargetId>,
    base: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    updates: &BTreeMap<NodeId, Update<BTreeSet<TargetId>>>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<NodeId, BTreeSet<TargetId>> {
    present_nodes
        .iter()
        .map(|node| {
            let next = match updates.get(node) {
                Some(Update::Set(targets)) => targets.clone(),
                Some(Update::Same) | None => base
                    .get(node)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|target| configured_targets.contains(target))
                    .collect(),
            };
            (node.clone(), next)
        })
        .collect()
}

fn deviation_files_after_updates(
    base: &BTreeMap<trellis_kernel::DeviationId, String>,
    updates: &BTreeMap<trellis_kernel::DeviationId, DeviationRequest>,
) -> BTreeMap<trellis_kernel::DeviationId, String> {
    let mut deviation_files = base.clone();
    for (id, request) in updates {
        if !request.path.trim().is_empty() {
            deviation_files.insert(id.clone(), request.path.clone());
        }
    }
    deviation_files
}

fn node_deviation_claims_after_updates(
    base: &BTreeMap<NodeId, BTreeSet<trellis_kernel::DeviationId>>,
    deviation_requests: &BTreeMap<trellis_kernel::DeviationId, trellis_kernel::DeviationRequest>,
    updates: &BTreeMap<NodeId, BTreeSet<trellis_kernel::DeviationId>>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<NodeId, BTreeSet<trellis_kernel::DeviationId>> {
    // Mirror `apply_worker_structure_updates` (model.rs:5617-5650):
    // first seed claims from `deviation_requests[id].affected_nodes`
    // intersected with `present_nodes`, then apply the explicit
    // `node_deviation_claims` overrides (which may clear a node's
    // claim set entirely). The two computations must agree so the
    // hydrator's `substantiveness_current_fingerprints` snapshot
    // matches the state that lands on the next kernel apply.
    let mut combined: BTreeMap<NodeId, BTreeSet<trellis_kernel::DeviationId>> = base.clone();
    for (id, request) in deviation_requests {
        if request.path.trim().is_empty() {
            continue;
        }
        for node in &request.affected_nodes {
            if present_nodes.contains(node) {
                combined.entry(node.clone()).or_default().insert(id.clone());
            }
        }
    }
    for (node, claims) in updates {
        if claims.is_empty() {
            combined.remove(node);
        } else {
            combined.insert(node.clone(), claims.clone());
        }
    }
    present_nodes
        .iter()
        .filter_map(|node| {
            let claims = combined.get(node).cloned().unwrap_or_default();
            if claims.is_empty() {
                None
            } else {
                Some((node.clone(), claims))
            }
        })
        .collect()
}

fn coverage_from_target_claims(
    configured_targets: &BTreeSet<TargetId>,
    target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<TargetId, BTreeSet<NodeId>> {
    configured_targets
        .iter()
        .map(|target| {
            let covered = present_nodes
                .iter()
                .filter(|node| {
                    target_claims
                        .get(*node)
                        .is_some_and(|targets| targets.contains(target))
                })
                .cloned()
                .collect();
            (target.clone(), covered)
        })
        .collect()
}

fn proof_validation_kind_requires_protected_package_check(validation_kind: &str) -> bool {
    matches!(
        validation_kind,
        "proof_easy" | "proof_local" | "proof_restructure" | "proof_coarse_restructure"
    )
}

fn proof_protected_package_legality_error(
    validation_kind: &str,
    acceptance_context: &PreparedWorkerGateOutput,
    response: &trellis_kernel::WorkerResponse,
) -> Option<String> {
    if !proof_validation_kind_requires_protected_package_check(validation_kind)
        || validation_kind == "proof_coarse_restructure"
    {
        return None;
    }
    if response.snapshot.coverage != acceptance_context.current_coverage {
        return Some("proof worker changed protected package coverage".to_string());
    }
    // The paper-fingerprint descendant axis is now Lean-relevance-filtered
    // (paper_fingerprints.rs `lean_relevant_definition_descendants`). A
    // worker adding a helper that no covering node's `lean_semantic_closure`
    // walk consumes does not change this axis; only Lean-relevant additions
    // or modifications surface here, which is exactly the protection
    // intent. No descendant strip is required; compare directly.
    if response.snapshot.paper_current_fingerprints
        != acceptance_context.current_paper_current_fingerprints
    {
        return Some("proof worker changed protected package paper fingerprints".to_string());
    }
    // The old per-node protected_snapshot post-hoc honesty check lived here.
    // Deleted: under the new design, commit-time protection is a single
    // `paper_target_corr_reopen_guard_errors` check restricted to
    // paper-target-covering nodes (wired in
    // `proof_worker_delta_step_result`). Non-covering nodes are
    // intentionally NOT post-hoc-honesty-checked against a pre-worker
    // fingerprint snapshot — they flow through normal correspondence
    // reopen → verify → reviewer adjudication on meaning changes.
    None
}

fn populate_response_fingerprints(
    repo_path: &std::path::Path,
    configured_targets: &std::collections::BTreeSet<TargetId>,
    current_target_claims: &std::collections::BTreeMap<
        NodeId,
        std::collections::BTreeSet<TargetId>,
    >,
    current_deviation_files: &std::collections::BTreeMap<trellis_kernel::DeviationId, String>,
    current_node_deviation_claims: &std::collections::BTreeMap<
        NodeId,
        std::collections::BTreeSet<trellis_kernel::DeviationId>,
    >,
    approved_paper_fingerprints: &std::collections::BTreeMap<TargetId, String>,
    paper_source_path: Option<&std::path::Path>,
    current_node_kinds: &std::collections::BTreeMap<NodeId, trellis_kernel::NodeKind>,
    response: &mut trellis_kernel::WorkerResponse,
) -> Result<(), String> {
    let present_nodes = response.snapshot.present_nodes.clone();
    let node_kinds = node_kinds_after_updates(
        current_node_kinds,
        &response.node_kind_updates,
        &present_nodes,
    );
    let target_claims = target_claims_after_updates(
        configured_targets,
        current_target_claims,
        &response.target_claim_updates,
        &present_nodes,
    );
    let deviation_files =
        deviation_files_after_updates(current_deviation_files, &response.deviation_requests);
    let node_deviation_claims = node_deviation_claims_after_updates(
        current_node_deviation_claims,
        &response.deviation_requests,
        &response.node_deviation_claims,
        &present_nodes,
    );
    let target_fingerprints = observe_correspondence_fingerprints(repo_path, &present_nodes)?;
    let deviation_current_fingerprints =
        observe_deviation_fingerprints(repo_path, &deviation_files)?;
    let sound_current_fingerprints = observe_soundness_fingerprints(repo_path, &present_nodes)?;
    let sound_current_fingerprint_parts =
        observe_soundness_fingerprint_parts(repo_path, &present_nodes)?;
    let sketch_proof_nodes = observe_sketch_proof_nodes(repo_path, &present_nodes);
    let coverage_now =
        coverage_from_target_claims(configured_targets, &target_claims, &present_nodes);
    let covering_union: BTreeSet<NodeId> = coverage_now.values().flatten().cloned().collect();
    let lean_relevant_per_covering =
        runtime_cli_observations::observe_lean_relevant_definition_descendants_per_node(
            repo_path,
            &covering_union,
        )?;
    let paper_current_fingerprints = observe_paper_faithfulness_fingerprints(
        repo_path,
        configured_targets,
        &target_claims,
        &present_nodes,
        approved_paper_fingerprints,
        &lean_relevant_per_covering,
    );
    // Substantiveness fingerprints describe the post-worker snapshot.
    // `node_kind_updates` are produced by kernel normalization from disk
    // before hydration, so apply them before hashing. Otherwise a newly
    // introduced proof/helper node is fingerprinted as the default
    // Definition on the acceptance cycle, then as Proof on the next cycle,
    // creating a spurious substantiveness reopen.
    let substantiveness_fingerprints =
        runtime_cli_observations::observe_substantiveness_fingerprints(
            repo_path,
            &present_nodes,
            paper_source_path,
            &node_kinds,
            &node_deviation_claims,
            &deviation_current_fingerprints,
        )?;
    response.snapshot.coverage =
        coverage_from_target_claims(configured_targets, &target_claims, &present_nodes);
    response.snapshot.target_fingerprints = target_fingerprints.clone();
    response.snapshot.corr_current_fingerprints = target_fingerprints;
    response.snapshot.paper_current_fingerprints = paper_current_fingerprints;
    response.snapshot.deviation_current_fingerprints = deviation_current_fingerprints;
    response.snapshot.substantiveness_current_fingerprints = substantiveness_fingerprints;
    response.snapshot.sound_current_fingerprints = sound_current_fingerprints;
    response.snapshot.sound_current_fingerprint_parts = sound_current_fingerprint_parts;
    response.snapshot.sketch_proof_nodes = sketch_proof_nodes;
    // Narrow Lean type-surface closure per target. Snapshotted at the
    // next AdvancePhase Approve (engine.rs `apply_human_gate_response`
    // GateKind::Advance / HumanChoice::Approve branch) into
    // `approved_targets.protected_closure_nodes`, which extends
    // `approved_target_nodes()` and therefore the worker-acceptance
    // protection set in `proof_worker_protected_package_legal`. Cheap
    // to observe on every burst because `observe_lean_semantic_payloads`
    // memoises per-node payloads in-process and on disk.
    response.snapshot.protected_closure_nodes_per_target =
        runtime_cli_observations::observe_protected_closure_nodes(
            repo_path,
            &response.snapshot.coverage,
            &response.snapshot.present_nodes,
        )?;
    Ok(())
}

fn node_kinds_after_updates(
    current_node_kinds: &BTreeMap<NodeId, NodeKind>,
    updates: &BTreeMap<NodeId, Update<NodeKind>>,
    present_nodes: &BTreeSet<NodeId>,
) -> BTreeMap<NodeId, NodeKind> {
    let mut node_kinds = current_node_kinds.clone();
    for (node, update) in updates {
        match update {
            Update::Same => {}
            Update::Set(kind) => {
                node_kinds.insert(node.clone(), *kind);
            }
        }
    }
    node_kinds.retain(|node, _| present_nodes.contains(node));
    node_kinds
}

fn hydrate_worker_response_output(
    input: &HydrateWorkerResponseInput,
) -> Result<HydratedWorkerResponseOutput, String> {
    let mut response = input.response.clone();
    let present_nodes = response.snapshot.present_nodes.clone();
    ensure_worker_checker_support_available(&input.repo_path, &present_nodes)?;
    populate_response_fingerprints(
        &input.repo_path,
        &input.configured_targets,
        &input.current_target_claims,
        &input.current_deviation_files,
        &input.current_node_deviation_claims,
        &input.approved_paper_fingerprints,
        input.paper_source_path.as_deref(),
        &input.current_node_kinds,
        &mut response,
    )?;
    Ok(HydratedWorkerResponseOutput { response })
}

fn compute_changed_node_stems_for_autofix(
    before_snapshot: &BTreeMap<String, String>,
    repo_path: &Path,
) -> BTreeSet<String> {
    let current = snapshot_tablet_dir(repo_path);
    let mut stems = BTreeSet::new();
    for (filename, current_hash) in &current {
        if !filename.ends_with(".lean") {
            continue;
        }
        if before_snapshot.get(filename) != Some(current_hash) {
            if let Some(stem) = filename.strip_suffix(".lean") {
                stems.insert(stem.to_string());
            }
        }
    }
    stems
}

fn check_trellis_worker_result_output(
    repo_path: &std::path::Path,
    acceptance_context: serde_json::Value,
    raw_payload: serde_json::Value,
) -> Result<CheckedTrellisWorkerResultOutput, String> {
    // Stable enumeration: each of these seven top-level phases is emitted to
    // stderr with `[acceptance] phase k/7: ...` before its work runs (or
    // `(skipped: ...)` when control flow takes a path that bypasses the
    // phase). The Python `run_kernel_cli` wrapper streams stderr line-by-line
    // so the calling agent sees progress in real time during the 5-30 minute
    // disk-bound checks, instead of a silent gap until the JSON response.
    //
    // Phase 5 (FILESPEC validation) was added 2026-05-12 to surface
    // body-marker / declaration-shape violations as worker-time
    // deterministic_rejection_reasons. The marker rule (every ordinary
    // Tablet `.lean` file has exactly one line whose trimmed content is
    // `-- BODY`) lets the kernel locate the statement/proof boundary
    // via a sub-millisecond text scan instead of the previous
    // parser-based path, which had a long tail of false-positive
    // failure modes (e.g. Mathlib's `scoped prefix:arg "#" => Finset.card`
    // on `FiberAndDegreeMixedLiftedIntersectionUniformBound` — the
    // burn-loop incident this phase was originally added for).
    const ACCEPTANCE_PHASES: usize = 7;

    let allowed_outcomes = worker_allowed_outcomes_for_validation(&acceptance_context)?;
    let validated =
        validate_trellis_worker_result_data_with_allowed_outcomes(&raw_payload, &allowed_outcomes);
    if !validated.ok {
        return Ok(CheckedTrellisWorkerResultOutput {
            ok: false,
            errors: validated.errors,
            data: None,
            response: None,
            validation_step_results: Vec::new(),
            contract_errors: Vec::new(),
            validation_errors: Vec::new(),
            final_outcome: String::new(),
        });
    }

    acceptance_progress_phase(1, ACCEPTANCE_PHASES, "parse acceptance context");
    let validated_data = validated
        .data
        .clone()
        .ok_or_else(|| "validated worker payload is missing data".to_string())?;
    let payload: CheckedWorkerPayload = serde_json::from_value(validated_data.clone())
        .map_err(|err| format!("validated worker payload had unexpected shape: {err}"))?;
    let acceptance_context: PreparedWorkerGateOutput =
        serde_json::from_value(acceptance_context)
            .map_err(|err| format!("worker acceptance context has unexpected shape: {err}"))?;

    let validation_execution_plan: Vec<WorkerValidationExecutionPlanStep> = serde_json::from_value(
        acceptance_context
            .worker_acceptance
            .get("validation_execution_plan")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|err| {
        format!("worker acceptance context has invalid validation_execution_plan: {err}")
    })?;

    let is_non_progress_outcome = matches!(payload.outcome.as_str(), "stuck" | "needs_restructure");

    // Auto-fix orphan-import injection runs INSIDE this CLI subcommand,
    // AFTER `execute_worker_validation_plan` returns step results but
    // BEFORE `populate_response_fingerprints` reads disk for fingerprints.
    //
    // This ordering eliminates the bug (#55) where kernel state stored
    // stale pre-auto-fix fingerprints while disk had post-auto-fix
    // content. Validators on both sides still observe pre-auto-fix disk
    // (matching the worker's check.py); only fingerprints see the
    // post-auto-fix state, ensuring kernel state and disk agree.
    //
    // Historical context: an earlier design ran auto-fix BEFORE
    // validation, which produced "authoritative checker mismatch"
    // rejections (worker check.py saw pre-fix, supervisor's check.py
    // saw post-fix, the two disagreed and the burst was rejected).
    // The current ordering avoids that because validation is scoped to
    // pre-fix disk on both sides; only fingerprints see the post-fix
    // state. Bridge.py no longer calls auto_fix; the polish step is
    // kernel-authored. Worker repo sync still runs in bridge.py via
    // `propagate_tablet_back_to_worker` after this CLI returns.

    let (validation_step_results, protected_semantic_change_nodes) = if is_non_progress_outcome {
        eprintln!(
            "[acceptance] phase 2/{ACCEPTANCE_PHASES}: validation execution plan (skipped: outcome={})",
            payload.outcome
        );
        (Vec::new(), BTreeSet::new())
    } else {
        acceptance_progress_phase(2, ACCEPTANCE_PHASES, "validation execution plan");
        let executed = execute_worker_validation_plan_with_progress(
            &ExecuteWorkerValidationPlanInput {
                repo_path: repo_path.to_path_buf(),
                active_node: if acceptance_context.active_node.is_empty() {
                    None
                } else {
                    Some(NodeId::from(acceptance_context.active_node.clone()))
                },
                before_snapshot: acceptance_context.before_snapshot.clone(),
                before_tablet_contents: acceptance_context.before_tablet_contents.clone(),
                baseline_errors: acceptance_context.baseline_errors.clone(),
                expected_active_hash: acceptance_context.expected_active_hash.clone(),
                baseline_declaration_hashes: acceptance_context.baseline_declaration_hashes.clone(),
                baseline_correspondence_hashes: acceptance_context
                    .baseline_correspondence_hashes
                    .clone(),
                current_present_nodes: acceptance_context.current_present_nodes.clone(),
                // Patch C-N item 1: forward kinds so the local-closure
                // probe dep-kind validator inside
                // `proof_worker_delta_step_result` has the map it needs
                // to reject kind-confused deps (boundary listed as
                // definition, etc.). On the acceptance path this is
                // always populated from the request's
                // `current_node_kinds`.
                current_node_kinds: acceptance_context.current_node_kinds.clone(),
                // Patch C-R: forward the pre-delta open_nodes snapshot
                // captured at `prepare_worker_gate_output` time so the
                // helper-probe loop in `proof_worker_delta_step_result`
                // can detect sorryd→sorry-free transitions for non-
                // active proof_nodes.
                current_open_nodes: acceptance_context.current_open_nodes.clone(),
                configured_targets: acceptance_context.configured_targets.clone(),
                current_deps: acceptance_context.current_deps.clone(),
                current_target_claims: acceptance_context.current_target_claims.clone(),
                approved_target_nodes: acceptance_context.approved_target_nodes.clone(),
                approved_corr_fingerprints: acceptance_context.approved_corr_fingerprints.clone(),
                coarse_dag_nodes: acceptance_context.coarse_dag_nodes.clone(),
                authorized_nodes: acceptance_context.authorized_nodes.clone(),
                validation_execution_plan: validation_execution_plan.clone(),
            },
            Some("2/6"),
        )?;
        (
            executed.step_results,
            executed.protected_semantic_change_nodes,
        )
    };

    acceptance_progress_phase(3, ACCEPTANCE_PHASES, "finalize worker acceptance");
    let current_node_kinds = node_kinds_from_request_value(&acceptance_context.request)?;

    let mut output = accept_worker_response(&WorkerAcceptanceInput {
        request_id: request_id_from_value(&acceptance_context.request),
        cycle: cycle_from_value(&acceptance_context.request),
        payload_outcome: worker_outcome_from_checked_payload(&payload.outcome)?,
        difficulty_updates: difficulty_updates_from_checked_payload(&payload.difficulty_updates),
        deviation_requests: payload
            .deviation_requests
            .iter()
            .map(|(id, request)| (trellis_kernel::DeviationId::from(id), request.clone()))
            .collect(),
        node_deviation_claims: payload
            .node_deviation_claims
            .iter()
            .map(|(node, claims)| {
                (
                    NodeId::from(node),
                    claims
                        .iter()
                        .map(trellis_kernel::DeviationId::from)
                        .collect(),
                )
            })
            .collect(),
        deviation_deletions: payload
            .deviation_deletions
            .iter()
            .map(trellis_kernel::DeviationId::from)
            .collect(),
        current_node_deviation_claims: acceptance_context.current_node_deviation_claims.clone(),
        current_deviation_files: acceptance_context.current_deviation_files.clone(),
        before_snapshot: acceptance_context.before_snapshot.clone(),
        forbid_tablet_changes_when_stuck: acceptance_context
            .worker_acceptance
            .get("forbid_tablet_changes_when_stuck")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        validation_execution_plan,
        validation_step_results: validation_step_results.clone(),
        protected_semantic_change_nodes,
        normalization: WorkerNormalizationInput {
            repo_path: repo_path.to_path_buf(),
            configured_targets: acceptance_context.configured_targets.clone(),
            current_present_nodes: acceptance_context.current_present_nodes.clone(),
            current_proof_nodes: acceptance_context.current_proof_nodes.clone(),
            current_node_kinds,
            current_deps: acceptance_context.current_deps.clone(),
            current_target_claims: acceptance_context.current_target_claims.clone(),
            approved_paper_fingerprints: acceptance_context
                .current_paper_approved_fingerprints
                .clone(),
            target_claim_updates: payload
                .target_claim_updates
                .iter()
                .map(|(node, targets)| {
                    (
                        NodeId::from(node),
                        targets.iter().map(TargetId::from).collect(),
                    )
                })
                .collect(),
            target_fingerprints: BTreeMap::new(),
            sound_current_fingerprints: BTreeMap::new(),
        },
    })?;

    output.response.summary = payload.summary.trim().to_string();
    output.response.comments = payload.comments.trim().to_string();
    output.response.needs_restructure_suggested_nodes = payload
        .needs_restructure_suggested_nodes
        .iter()
        .map(|name| NodeId::from(name.trim()))
        .collect();

    // #55: kernel-authored auto-fix runs HERE, after acceptance is
    // finalized but before fingerprints are populated. Acceptance
    // already consumed the pre-auto-fix `before_snapshot`; fingerprints
    // get re-read post-auto-fix below, so kernel state and disk agree.
    //
    // `changed_node_stems` is hoisted here (was: scoped inside the
    // auto-fix block) so phase 5 can re-use it without recomputing
    // against the same before_snapshot. The check is cheap (disk diff
    // over a small set of files) but routing through the same source
    // of truth avoids any chance of phase 5 acting on a different node
    // set than phase 4.
    // Computed unconditionally so phase 5 (FILESPEC) can validate the
    // worker's file shape even when prior phases set outcome=Invalid.
    // Phase 4 (auto-fix) still skips on Invalid below — auto-fix mutates
    // disk and should not run against a rejected response — but the
    // FILESPEC string scan is cheap and surfaces the actual shape-error
    // cause (e.g. `Declaration name is ""`) before phase 6's Lean
    // compilation has to re-discover the same diagnostic.
    let changed_node_stems: std::collections::BTreeSet<String> =
        compute_changed_node_stems_for_autofix(&acceptance_context.before_snapshot, repo_path);
    if output.final_outcome == WorkerOutcome::Valid {
        acceptance_progress_phase(4, ACCEPTANCE_PHASES, "auto-fix orphan imports");
        let total_stems = changed_node_stems.len();
        for (idx, stem) in changed_node_stems.iter().enumerate() {
            if stem == "Preamble" || stem == "Axioms" {
                continue;
            }
            acceptance_progress_sub("4/7", idx + 1, total_stems, stem);
            let _ = normalize_node_lean_imports_on_disk(repo_path, stem);
        }
        // Re-extract deps from disk so dep_updates in the response
        // reflect the post-auto-fix import surface. accept_worker_response
        // already populated dep_updates from a pre-auto-fix disk read
        // (normalize_worker_response → direct_deps_from_repo), and
        // auto-fix may have just added `import Tablet.Preamble` lines
        // that the pre-fix read missed. Without this re-extraction,
        // kernel state.deps would diverge from disk until the next
        // worker burst's normalize self-heals it. Today the divergence
        // is benign (Preamble is excluded from orphan logic and is a
        // dep-graph leaf), but principled state/disk parity is cheap
        // here and protects against future logic that consults state.deps
        // (e.g. reviewer prompts include current_deps).
        //
        // Why this targeted re-extraction is sufficient (audit follow-up
        // — full normalize was the audit's recommendation, but it would
        // be redundant work given what auto-fix actually does):
        //
        // `normalize_node_lean_imports_on_disk` ONLY adds an
        // `import Tablet.Preamble` line to a node's `.lean` file when
        // it has zero existing Tablet imports. That can affect at most:
        //   - imports → deps              (covered: re-extract below)
        //   - file content hash → fingerprints
        //                                  (covered: populate_response_fingerprints
        //                                   re-reads disk below)
        //
        // What auto-fix CANNOT affect, and why:
        //   - present_nodes / open_nodes  : doesn't add or remove `.lean` files
        //                                   or sorrys; only edits import lines
        //   - node_kinds                  : derived from declaration heads,
        //                                   not imports
        //   - proof_nodes                 : derived from sorry presence,
        //                                   not imports
        //   - target_claims               : derived from `.tex`, not `.lean`
        //                                   imports
        //
        // If `normalize_node_lean_imports_on_disk` ever grows beyond
        // import-line edits (e.g. starts adding declarations or
        // touching sorrys), this targeted re-extraction will silently
        // miss the new effects — at which point the right move is to
        // replace this block with a full normalize_worker_response
        // re-run (the audit's recommendation).
        let post_fix_deps =
            direct_deps_from_repo(repo_path, &output.response.snapshot.present_nodes);
        output.response.dep_updates =
            diff_node_sets(&acceptance_context.current_deps, &post_fix_deps);
    } else {
        eprintln!(
            "[acceptance] phase 4/{ACCEPTANCE_PHASES}: auto-fix orphan imports (skipped: outcome={:?})",
            output.final_outcome
        );
    }

    // Phase 5: FILESPEC validation. For each node added or modified
    // by the worker, verify the file matches the FILESPEC marker rule
    // — exactly one line whose trimmed content is `-- BODY`, with the
    // principal declaration named after the file stem appearing before
    // the marker. Pure-text via `filespec_split::validate_filespec`;
    // no Lean dependency, sub-millisecond per file.
    //
    // Surfacing this here gives the worker a deterministic rejection
    // reason in its own Shell output during the burst, so the worker
    // can repair the file in-place rather than producing a kernel-side
    // failure mode no later request can recover from. (Cf. the 2026-05-12
    // burn loop on `FiberAndDegreeMixedLiftedIntersectionUniformBound`,
    // where Mathlib's `scoped prefix:arg "#" => Finset.card` notation
    // confused the previous parser-based splitter; the FILESPEC marker
    // approach is parser-independent and indentation-inert, eliminating
    // that entire failure class.)
    // FILESPEC validation runs regardless of prior outcome: it is a
    // sub-millisecond pure-text check whose role is to surface the
    // actual shape error (e.g. missing `-- BODY` marker, principal
    // declaration named "" instead of the expected stem) BEFORE phase
    // 6's Lean compilation re-discovers the same diagnostic the
    // expensive way. When prior phases already set Invalid, phase 5
    // can only confirm Invalid (or add additional shape errors); it
    // cannot promote back to Valid.
    acceptance_progress_phase(5, ACCEPTANCE_PHASES, "FILESPEC validation");
    let total_stems = changed_node_stems.len();
    for (idx, stem) in changed_node_stems.iter().enumerate() {
        if stem == "Preamble" || stem == "Axioms" {
            continue;
        }
        acceptance_progress_sub("5/7", idx + 1, total_stems, stem);
        // FILESPEC rule: each ordinary `Tablet/<Node>.lean` must
        // contain exactly one line whose trimmed content is
        // `-- BODY`, with the principal declaration named after the
        // file stem appearing before the marker. Pure-text check;
        // no Lean dependency. Replaces the prior Lean-parser-based
        // `decl_split` parsability gate (which had a long tail of
        // false-positive failure modes on scoped notation, set_option
        // wrappers, multi-line let-in-signature, etc.).
        let content_result =
            trellis_kernel::filespec_split::read_node_file(repo_path, stem).map(|(c, _)| c);
        let content = match content_result {
            Ok(c) => c,
            Err(err) => {
                let msg = format!("FILESPEC read failed for {stem}: {err}");
                output.validation_errors.push(msg.clone());
                output.errors.push(msg);
                output.final_outcome = trellis_kernel::WorkerOutcome::Invalid;
                output.ok = false;
                output.response.outcome = trellis_kernel::WorkerOutcome::Invalid;
                continue;
            }
        };
        if let Err(err) = trellis_kernel::filespec_split::validate_filespec(&content, stem) {
            let msg = format!(
                "FILESPEC validation failed for {stem}: {err} \
                 (every ordinary Tablet `.lean` file must contain \
                 exactly one line whose trimmed content is `-- BODY`, \
                 placed between the principal declaration and its \
                 proof body; the marker is a Lean line comment, so \
                 it has no parser interaction and any indentation \
                 is fine)"
            );
            output.validation_errors.push(msg.clone());
            output.errors.push(msg);
            output.final_outcome = trellis_kernel::WorkerOutcome::Invalid;
            output.ok = false;
            output.response.outcome = trellis_kernel::WorkerOutcome::Invalid;
        }
    }

    acceptance_progress_phase(6, ACCEPTANCE_PHASES, "observe response fingerprints");
    // Populate response fingerprints on every outcome so the engine's
    // worker_semantic_delta check sees real fingerprints and does not
    // spuriously flag NeedsRestructure / Stuck submissions as a snapshot
    // delta against the prior (hydrated) state.live. Valid paths additionally
    // run hydrate_worker_response_output below for its checker-support
    // materialization side effect (fingerprint population is idempotent).
    if let Err(err) = populate_response_fingerprints(
        repo_path,
        &acceptance_context.configured_targets,
        &acceptance_context.current_target_claims,
        &acceptance_context.current_deviation_files,
        &acceptance_context.current_node_deviation_claims,
        &acceptance_context.current_paper_approved_fingerprints,
        acceptance_context.paper_source_path.as_deref(),
        &acceptance_context.current_node_kinds,
        &mut output.response,
    ) {
        if output.final_outcome == trellis_kernel::WorkerOutcome::Valid {
            output.validation_errors.push(err.clone());
            output.errors.push(err);
            output.final_outcome = trellis_kernel::WorkerOutcome::Invalid;
            output.ok = false;
            output.response.outcome = trellis_kernel::WorkerOutcome::Invalid;
        } else {
            output.validation_errors.push(err);
        }
    }

    if output.final_outcome == trellis_kernel::WorkerOutcome::Valid {
        acceptance_progress_phase(7, ACCEPTANCE_PHASES, "hydrate response and legality check");
        match hydrate_worker_response_output(&HydrateWorkerResponseInput {
            repo_path: repo_path.to_path_buf(),
            configured_targets: acceptance_context.configured_targets.clone(),
            current_target_claims: acceptance_context.current_target_claims.clone(),
            current_deviation_files: acceptance_context.current_deviation_files.clone(),
            current_node_deviation_claims: acceptance_context.current_node_deviation_claims.clone(),
            approved_paper_fingerprints: acceptance_context
                .current_paper_approved_fingerprints
                .clone(),
            paper_source_path: acceptance_context.paper_source_path.clone(),
            current_node_kinds: acceptance_context.current_node_kinds.clone(),
            response: output.response.clone(),
        }) {
            Ok(hydrated) => {
                output.response = hydrated.response;
            }
            Err(err) => {
                output.validation_errors.push(err.clone());
                output.errors.push(err);
                output.final_outcome = trellis_kernel::WorkerOutcome::Invalid;
                output.ok = false;
                output.response.outcome = trellis_kernel::WorkerOutcome::Invalid;
            }
        }
    } else {
        eprintln!(
            "[acceptance] phase 7/{ACCEPTANCE_PHASES}: hydrate response and legality check (skipped: outcome={:?})",
            output.final_outcome
        );
    }

    if output.final_outcome == trellis_kernel::WorkerOutcome::Valid {
        if let Some(err) = proof_protected_package_legality_error(
            &acceptance_context.validation_kind,
            &acceptance_context,
            &output.response,
        ) {
            output.validation_errors.push(err.clone());
            output.errors.push(err);
            output.final_outcome = trellis_kernel::WorkerOutcome::Invalid;
            output.ok = false;
            output.response.outcome = trellis_kernel::WorkerOutcome::Invalid;
        }
    }

    output.response.deterministic_rejection_reasons =
        if output.final_outcome == trellis_kernel::WorkerOutcome::Invalid {
            output.errors.clone()
        } else {
            Vec::new()
        };

    let mut response_json = serde_json::to_value(&output.response)
        .map_err(|err| format!("failed to serialize worker response: {err}"))?;
    if let Some(obj) = response_json.as_object_mut() {
        obj.insert("kind".to_string(), json!("worker"));
    }

    Ok(CheckedTrellisWorkerResultOutput {
        ok: output.ok,
        errors: output.errors.clone(),
        data: Some(validated_data),
        response: Some(response_json),
        validation_step_results,
        contract_errors: output.contract_errors,
        validation_errors: output.validation_errors,
        final_outcome: format!("{:?}", output.final_outcome).to_ascii_lowercase(),
    })
}

fn worker_allowed_outcomes_for_validation(
    acceptance_context: &serde_json::Value,
) -> Result<Vec<String>, String> {
    let validation_kind_value = acceptance_context
        .get("worker_acceptance")
        .and_then(|value| value.get("validation_kind"))
        .cloned()
        .or_else(|| acceptance_context.get("validation_kind").cloned())
        .ok_or_else(|| "worker acceptance context is missing validation_kind".to_string())?;
    let validation_kind: WorkerValidationKind = serde_json::from_value(validation_kind_value)
        .map_err(|err| format!("worker acceptance context has invalid validation_kind: {err}"))?;
    let cleanup_like = matches!(
        validation_kind,
        WorkerValidationKind::Cleanup | WorkerValidationKind::FinalCleanup
    );
    Ok(if cleanup_like {
        vec!["valid".to_string(), "invalid".to_string()]
    } else {
        vec![
            "valid".to_string(),
            "invalid".to_string(),
            "stuck".to_string(),
            "needs_restructure".to_string(),
        ]
    })
}

fn build_malformed_response_output(
    kind: RequestKind,
    request_id: u32,
    cycle: u32,
) -> Result<WrapperResponse, String> {
    match kind {
        RequestKind::Worker => Ok(WrapperResponse::Worker(WorkerResponse {
            request_id,
            cycle,
            status: ResponseStatus::Malformed,
            outcome: WorkerOutcome::Invalid,
            ..WorkerResponse::default()
        })),
        RequestKind::Review => Ok(WrapperResponse::Review(ReviewResponse {
            request_id,
            cycle,
            status: ResponseStatus::Malformed,
            ..ReviewResponse::default()
        })),
        // Cleanup-v2 (audit Finding 1): malformed audit responses route
        // through the kernel's audit-burst retry path (one retry per
        // burst, then force AuditDone).
        RequestKind::Audit => Ok(WrapperResponse::Audit(trellis_kernel::AuditResponse {
            request_id,
            cycle,
            status: ResponseStatus::Malformed,
            ..trellis_kernel::AuditResponse::default()
        })),
        RequestKind::StuckMathAudit => {
            Ok(WrapperResponse::StuckMathAudit(StuckMathAuditResponse {
                request_id,
                cycle,
                status: ResponseStatus::Malformed,
                ..StuckMathAuditResponse::default()
            }))
        }
        _ => Err(format!(
            "build_malformed_response only supports worker/review/audit/stuck_math_audit, got {:?}",
            kind
        )),
    }
}

fn normalize_human_gate_output(
    request_id: u32,
    cycle: u32,
    raw_payload_text: &str,
) -> WrapperResponse {
    let malformed = || {
        WrapperResponse::HumanGate(HumanGateResponse {
            request_id,
            cycle,
            status: ResponseStatus::Malformed,
            choice: HumanChoice::Approve,
        })
    };
    let payload: Value = match serde_json::from_str(raw_payload_text) {
        Ok(value) => value,
        Err(_) => return malformed(),
    };
    let obj = match payload.as_object() {
        Some(obj) => obj,
        None => return malformed(),
    };
    let choice = match obj.get("choice").and_then(Value::as_str) {
        Some(raw) if raw.eq_ignore_ascii_case("approve") => HumanChoice::Approve,
        Some(raw) if raw.eq_ignore_ascii_case("feedback") => HumanChoice::Feedback,
        _ => return malformed(),
    };
    WrapperResponse::HumanGate(HumanGateResponse {
        request_id,
        cycle,
        status: ResponseStatus::Ok,
        choice,
    })
}

fn check_trellis_reviewer_result_output(
    review_request: serde_json::Value,
    raw_payload: serde_json::Value,
) -> Result<CheckedTrellisReviewerResultOutput, String> {
    let validated = validate_trellis_reviewer_result_data(&raw_payload);
    if !validated.ok {
        return Ok(CheckedTrellisReviewerResultOutput {
            ok: false,
            errors: validated.errors,
            data: None,
            response: None,
        });
    }

    let validated_data = validated
        .data
        .clone()
        .ok_or_else(|| "validated reviewer payload is missing data".to_string())?;
    let request: WrapperRequest = serde_json::from_value(review_request)
        .map_err(|err| format!("review request has unexpected shape: {err}"))?;
    let raw_payload: RawReviewPayload = serde_json::from_value(validated_data.clone())
        .map_err(|err| format!("validated reviewer payload had unexpected shape: {err}"))?;
    let output = normalize_review_response(&ReviewNormalizationInput {
        request,
        raw_payload,
    })?;

    let mut response_json = serde_json::to_value(&output.response)
        .map_err(|err| format!("failed to serialize review response: {err}"))?;
    if let Some(obj) = response_json.as_object_mut() {
        obj.insert("kind".to_string(), json!("review"));
    }

    Ok(CheckedTrellisReviewerResultOutput {
        ok: true,
        errors: Vec::new(),
        data: Some(validated_data),
        response: Some(response_json),
    })
}

/// Cleanup-v2 (audit Finding 1): one-shot validate+normalize for an
/// audit-burst artifact. Mirrors `check_trellis_reviewer_result_output`.
fn check_trellis_audit_result_output(
    audit_request: serde_json::Value,
    raw_payload: serde_json::Value,
) -> Result<CheckedTrellisAuditResultOutput, String> {
    let validated = validate_trellis_audit_result_data(&raw_payload);
    if !validated.ok {
        return Ok(CheckedTrellisAuditResultOutput {
            ok: false,
            errors: validated.errors,
            data: None,
            response: None,
        });
    }
    let validated_data = validated
        .data
        .clone()
        .ok_or_else(|| "validated audit payload is missing data".to_string())?;
    let request: WrapperRequest = serde_json::from_value(audit_request)
        .map_err(|err| format!("audit request has unexpected shape: {err}"))?;
    let raw_payload: trellis_kernel::RawAuditPayload =
        serde_json::from_value(validated_data.clone())
            .map_err(|err| format!("validated audit payload had unexpected shape: {err}"))?;
    let output = normalize_audit_response(&AuditNormalizationInput {
        request,
        raw_payload,
    })?;
    let mut response_json = serde_json::to_value(&output.response)
        .map_err(|err| format!("failed to serialize audit response: {err}"))?;
    if let Some(obj) = response_json.as_object_mut() {
        obj.insert("kind".to_string(), json!("audit"));
    }
    Ok(CheckedTrellisAuditResultOutput {
        ok: true,
        errors: Vec::new(),
        data: Some(validated_data),
        response: Some(response_json),
    })
}

fn check_trellis_stuck_math_audit_result_output(
    audit_request: serde_json::Value,
    raw_payload: serde_json::Value,
) -> Result<CheckedTrellisAuditResultOutput, String> {
    let validated = validate_trellis_stuck_math_audit_result_data(&raw_payload);
    if !validated.ok {
        return Ok(CheckedTrellisAuditResultOutput {
            ok: false,
            errors: validated.errors,
            data: None,
            response: None,
        });
    }
    let validated_data = validated
        .data
        .clone()
        .ok_or_else(|| "validated stuck math audit payload is missing data".to_string())?;
    let request: WrapperRequest = serde_json::from_value(audit_request)
        .map_err(|err| format!("stuck math audit request has unexpected shape: {err}"))?;
    if request.kind != RequestKind::StuckMathAudit {
        return Err(format!(
            "stuck math audit checker expected RequestKind::StuckMathAudit, got {:?}",
            request.kind
        ));
    }
    let report = validated_data
        .get("report")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let tasks: Vec<trellis_kernel::AuditTask> = serde_json::from_value(
        validated_data
            .get("tasks")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|err| format!("validated stuck math audit tasks had unexpected shape: {err}"))?;
    let probe_paths: Vec<String> = serde_json::from_value(
        validated_data
            .get("probe_paths")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|err| format!("validated stuck math audit probe_paths had unexpected shape: {err}"))?;
    let cone_clean_node = validated_data
        .get("cone_clean_node")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(NodeId::from);
    let confirm_need_input = validated_data
        .get("confirm_need_input")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let global_repair_approve = validated_data
        .get("global_repair_approve")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let global_repair_approved_extension_node_ids: Vec<String> = serde_json::from_value(
        validated_data
            .get("global_repair_approved_extension_node_ids")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|err| {
        format!("validated global_repair_approved_extension_node_ids had unexpected shape: {err}")
    })?;
    let global_repair_auditor_reason = validated_data
        .get("global_repair_auditor_reason")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let response = WrapperResponse::StuckMathAudit(StuckMathAuditResponse {
        request_id: request.id,
        cycle: request.cycle,
        status: ResponseStatus::Ok,
        confirm_need_input,
        report,
        tasks,
        probe_paths,
        cone_clean_node,
        global_repair_approve,
        global_repair_approved_extension_node_ids,
        global_repair_auditor_reason,
    });
    let mut response_json = serde_json::to_value(&response)
        .map_err(|err| format!("failed to serialize stuck math audit response: {err}"))?;
    if let Some(obj) = response_json.as_object_mut() {
        obj.insert("kind".to_string(), json!("stuck_math_audit"));
    }
    Ok(CheckedTrellisAuditResultOutput {
        ok: true,
        errors: Vec::new(),
        data: Some(validated_data),
        response: Some(response_json),
    })
}

fn read_request() -> Result<RuntimeCliRequest, String> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|err| format!("failed to read stdin: {err}"))?;
    serde_json::from_str(&input).map_err(|err| format!("invalid runtime request JSON: {err}"))
}

fn checkpoint_from_paths(paths: &RuntimePaths) -> Result<Option<RuntimeCheckpoint>, String> {
    if !paths.checkpoint_path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&paths.checkpoint_path)
        .map_err(|err| format!("failed to read checkpoint: {err}"))?;
    let checkpoint =
        serde_json::from_str(&text).map_err(|err| format!("failed to parse checkpoint: {err}"))?;
    Ok(Some(checkpoint))
}

fn success_response(
    runtime: &SupervisorRuntime,
    outcome: Option<RuntimeStepOutcome>,
    steps_executed: u32,
    import_summary: Option<LegacyImportSummary>,
) -> Result<RuntimeCliResponse, String> {
    Ok(RuntimeCliResponse::Ok {
        state: runtime.state().clone(),
        metadata: runtime.metadata().clone(),
        outcome,
        checkpoint: checkpoint_from_paths(runtime.paths())?,
        event_count: runtime.event_count(),
        steps_executed,
        import_summary,
    })
}

fn make_adapter(
    runtime: &SupervisorRuntime,
    response: Option<WrapperResponse>,
) -> Result<RuntimeAdapter, String> {
    if let Some(response) = response {
        return Ok(RuntimeAdapter::Provided(ProvidedResponseAdapter {
            response: Some(response),
        }));
    }
    if let Some(command) = bridge_command_from_env()? {
        let config_path = runtime
            .metadata()
            .config_path
            .clone()
            .ok_or_else(|| "runtime metadata is missing config_path".to_string())?;
        return Ok(RuntimeAdapter::Process(ProcessBridgeAdapter {
            command,
            config_path,
            repo_path: runtime.metadata().repo_path.clone(),
            runtime_root: runtime.paths().root.clone(),
        }));
    }
    Ok(RuntimeAdapter::Provided(ProvidedResponseAdapter {
        response: None,
    }))
}

/// Bug B: load + recompute corr/sound fingerprints from disk, fail hard
/// on any mismatch with the kernel's recorded `state.live.{corr,sound}_current_fingerprints`.
/// Drift means the worktree was mutated outside the protocol since the
/// last verifier panel ran (manual git surgery, partial restore, mid-write
/// crash). Continuing risks compounding the corruption — refuse to start
/// so the operator must explicitly recover.
///
/// Skipped when:
/// - metadata.repo_path is unset (test fixtures, init flow).
/// - Tablet/ doesn't exist (uninitialized repo).
/// - There's an in-flight Worker request — the worker burst is mutating
///   disk and divergence is the expected condition until the burst
///   result is normalized.
///
/// Paper fingerprints (target-keyed via paper.tex line-range claims)
/// and baseline_declaration_hashes (per-request snapshots, not per-state)
/// are NOT validated here — different shape, less drift-prone.
fn load_runtime_with_fingerprint_validation(
    paths: RuntimePaths,
) -> Result<SupervisorRuntime, String> {
    let mut runtime =
        SupervisorRuntime::load(paths).map_err(|err| format!("runtime load failed: {err}"))?;

    // One-shot fingerprint-schema migration (lean-relevance refactor). Runs
    // before validation so subsequent byte-equality checks at
    // `current_corr_state` / `current_paper_state` see the migrated shape on
    // both sides. Refuses to run during in-flight worker (audit point —
    // could otherwise bless unaccepted WIP into the approval baseline).
    // Safe no-op if already at the current schema version.
    if let Some(repo_path) = runtime.metadata().repo_path.clone() {
        if repo_path.join("Tablet").is_dir() {
            runtime
                .try_post_load_state_migration(|state| {
                    runtime_cli_observations::migrate_corr_fingerprint_schema(state, &repo_path)
                })
                .map_err(|err| format!("corr fingerprint schema migration failed: {err}"))?;
            runtime
                .try_post_load_state_migration(|state| {
                    runtime_cli_observations::migrate_soundness_fingerprint_schema_if_enabled(
                        state, &repo_path,
                    )
                })
                .map_err(|err| format!("soundness fingerprint schema migration failed: {err}"))?;
        }
        // Burst-history backfill: if `<repo>/.trellis/logs/burst-history.jsonl`
        // doesn't exist yet, walk `<runtime_root>/event_log.jsonl` once and
        // emit one summary row per `wrapper_response` event. Best-effort
        // (errors logged to stderr, never blocks startup). Idempotent
        // (no-op if the ledger already exists).
        let event_log_path = runtime.paths().event_log_path.clone();
        trellis_kernel::burst_history::backfill_if_missing(&repo_path, &event_log_path);
    }

    let Some(repo_path) = runtime.metadata().repo_path.as_deref() else {
        return Ok(runtime);
    };
    if !repo_path.join("Tablet").is_dir() {
        return Ok(runtime);
    }
    if let Some(req) = runtime.state().in_flight_request.as_ref() {
        if req.kind == trellis_kernel::RequestKind::Worker {
            return Ok(runtime);
        }
    }
    let recorded_corr = &runtime.state().live.corr_current_fingerprints;
    let recorded_sound = &runtime.state().live.sound_current_fingerprints;
    let nodes_to_check: BTreeSet<NodeId> = recorded_corr
        .keys()
        .chain(recorded_sound.keys())
        .filter(|n| n.as_str() != "Preamble")
        .cloned()
        .collect();
    if nodes_to_check.is_empty() {
        return Ok(runtime);
    }
    let observed_corr = observe_correspondence_fingerprints(repo_path, &nodes_to_check)
        .map_err(|err| format!("Bug B fingerprint validation: corr observe failed: {err}"))?;
    let observed_sound = observe_soundness_fingerprints(repo_path, &nodes_to_check)
        .map_err(|err| format!("Bug B fingerprint validation: sound observe failed: {err}"))?;
    let mut mismatches: Vec<String> = Vec::new();
    for node in &nodes_to_check {
        if let Some(expected) = recorded_corr.get(node) {
            let actual = observed_corr.get(node).cloned().unwrap_or_default();
            if &actual != expected {
                mismatches.push(format!("corr[{node}]: state={expected} disk={actual}"));
            }
        }
        if let Some(expected) = recorded_sound.get(node) {
            let actual = observed_sound.get(node).cloned().unwrap_or_default();
            if &actual != expected {
                mismatches.push(format!("sound[{node}]: state={expected} disk={actual}"));
            }
        }
    }
    if !mismatches.is_empty() {
        return Err(format!(
            "kernel state diverges from disk on {} fingerprint(s); \
             worktree was mutated outside the protocol. Restore the repo \
             to a checkpointed state and re-run, or rewind the supervisor \
             state. Mismatches:\n  {}",
            mismatches.len(),
            mismatches.join("\n  ")
        ));
    }
    Ok(runtime)
}

// ────────────────────────────────────────────────────────────────────
// Patch C-D: local-closure runtime CLI orchestration.
//
// The runtime CLI is the I/O boundary for local-closure probes:
// - Computes real per-record input hashes (toolchain, manifest, preamble,
//   approved-axioms, active-decl, active-statement) at install time.
// - Runs the deterministic-revalidation pass before reviewer round-trips,
//   in the cleanup-burst response pipeline, and during first-deploy
//   migration.
// - Persists migration progress under
//   `<runtime_root>/checker-state/local-closure-records/<node>.json` so
//   supervisor kills mid-migration carry forward.
//
// Engine functions stay pure-state. Probe I/O happens here. Hash
// computation is centralized in `compute_local_closure_record_inputs`
// so every install site uses identical input semantics.
// ────────────────────────────────────────────────────────────────────

/// Closure-record schema version. Bumped when traversal semantics or the
/// hash-input set evolves; existing records' `closure_version` mismatch
/// triggers re-probe via deterministic revalidation. C-B writes
/// `"TODO_PATCH_C_D_VERSION"` as a sentinel for records awaiting hash
/// backfill; the backfill pass replaces it with this constant.
const CLOSURE_VERSION: &str = "patch_c_v1";

/// C-B hash sentinel — replaced by `compute_local_closure_record_inputs`
/// at backfill time. Records carrying this string are pending real-hash
/// computation.
const CLOSURE_HASH_SENTINEL: &str = "TODO_PATCH_C_D_HASH";

/// C-B closure-version sentinel — paired with `CLOSURE_HASH_SENTINEL` so
/// the backfill pass can detect placeholder records by either field.
const CLOSURE_VERSION_SENTINEL: &str = "TODO_PATCH_C_D_VERSION";

/// Number of transport-error retries before `retry_exhausted=true`.
/// After exhaustion, deterministic revalidation skips the node and the
/// failure surfaces to the operator as an infrastructure diagnostic.
/// Plan §7.4.1.
const TRANSPORT_RETRY_BUDGET: u32 = 5;

/// Cap on exponential-backoff window (cycles). Plan §7.4.1.
const TRANSPORT_BACKOFF_MAX_CYCLES: u64 = 64;

/// Subdirectory under `runtime_root` for persisted local-closure records
/// (one JSON file per node). Plan §7.10.
const LOCAL_CLOSURE_RECORDS_DIR: &str = "checker-state/local-closure-records";

/// Filename suffix for persisted records.
const LOCAL_CLOSURE_RECORDS_EXT: &str = "json";

fn local_closure_records_dir(runtime_root: &Path) -> PathBuf {
    runtime_root.join(LOCAL_CLOSURE_RECORDS_DIR)
}

fn remove_persisted_local_closure_record_files(
    runtime_root: &Path,
    nodes: &[NodeId],
    log_label: &str,
) {
    if nodes.is_empty() {
        return;
    }
    let records_dir = local_closure_records_dir(runtime_root);
    for node in nodes {
        let path = records_dir.join(trellis_kernel::runtime::persisted_record_file_name(node));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!(
                    "[{log_label}] failed to remove demoted record {}: {err}",
                    path.display()
                );
            }
        }
    }
}

fn hash_bytes(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

fn hash_text(content: &str) -> String {
    hash_bytes(content.as_bytes())
}

fn hash_file_or_empty(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => hash_bytes(&bytes),
        Err(_) => String::new(),
    }
}

/// Hash the per-node approved-kernel-axioms set. Plan §7.1: a per-node
/// hash means a per-node waiver does NOT touch other records.
///
/// Audit MEDIUM (approved-axioms load errors): returns `Err(_)` on I/O
/// or parse failure so callers can surface an `internal_error` rather
/// than silently substituting an empty approved set. The previous
/// `.unwrap_or_default()` call site collapsed every load failure into
/// "hash of empty list", which would mislabel an infrastructure /
/// config error as a clean record.
fn hash_approved_axioms_for_node(repo: &Path, node: &str) -> Result<String, String> {
    let approved = load_approved_axioms(repo, node)?;
    let serialized: Vec<&str> = approved.iter().map(String::as_str).collect();
    let blob = serde_json::to_string(&serialized).unwrap_or_default();
    Ok(hash_text(&blob))
}

/// Read the active node's `<repo>/Tablet/<node>.lean` content and hash
/// the whole file.
fn active_decl_hash_for_node(repo: &Path, node: &str) -> String {
    let path = repo.join("Tablet").join(format!("{node}.lean"));
    hash_file_or_empty(&path)
}

/// Hash just the declaration statement for `node`, normalized via
/// `find_declaration` (existing helper from runtime_cli_observations).
/// Empty-string fallback matches existing behavior in
/// `runtime_cli_observations::declaration_hash`.
fn active_statement_hash_for_node(repo: &Path, node: &str) -> String {
    let path = repo.join("Tablet").join(format!("{node}.lean"));
    let lean_content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => return String::new(),
    };
    let decl = find_declaration(&lean_content, node);
    if decl.is_empty() {
        String::new()
    } else {
        hash_text(&decl)
    }
}

/// Plan §7.0/§7.1 — compute a fully populated `LocalClosureRecord` from
/// disk inputs and probe content. The `kernel_axioms` /
/// `boundary_theorems` / `strict_*_deps` fields come from the probe (the
/// engine already attaches these via `apply_local_closure_acceptance_bookkeeping`).
/// Hash fields are read fresh per call.
///
/// Used at three sites:
/// 1. Backfill of post-engine-apply C-B sentinel records.
/// 2. Deterministic-revalidation pass.
/// 3. Migration probe install.
fn compute_local_closure_record_inputs(
    repo: &Path,
    node: &NodeId,
    kernel_axioms: &BTreeSet<String>,
    boundary_theorems: &BTreeMap<NodeId, String>,
    strict_theorem_deps: &BTreeMap<NodeId, String>,
    strict_definition_deps: &BTreeMap<NodeId, String>,
    accepted_at_snapshot_id: String,
    axcheck_status: AxcheckStatus,
) -> Result<LocalClosureRecord, String> {
    let approved_axioms_hash = hash_approved_axioms_for_node(repo, node.as_str())?;
    Ok(LocalClosureRecord {
        node: node.clone(),
        closure_version: CLOSURE_VERSION.to_string(),
        toolchain_hash: hash_file_or_empty(&repo.join("lean-toolchain")),
        lake_manifest_hash: hash_file_or_empty(&repo.join("lake-manifest.json")),
        preamble_hash: hash_file_or_empty(&repo.join("Tablet/Preamble.lean")),
        approved_axioms_hash,
        active_decl_hash: active_decl_hash_for_node(repo, node.as_str()),
        active_statement_hash: active_statement_hash_for_node(repo, node.as_str()),
        kernel_axioms: kernel_axioms.clone(),
        boundary_theorems: boundary_theorems.clone(),
        strict_theorem_deps: strict_theorem_deps.clone(),
        strict_definition_deps: strict_definition_deps.clone(),
        // Patch C-P HIGH 1 (b) — populated by callers that have access
        // to live `corr_current_fingerprints` (engine probe path /
        // deterministic revalidation / migration). Left empty here so
        // pre-Patch-C-P callsites and tests that don't care continue
        // to compile; the migration-time check treats an empty map as
        // "no kernel-hash invariants to enforce" (additive layer atop
        // the strict-signal + cross-record-evidence checks).
        kernel_semantic_hashes: BTreeMap::new(),
        accepted_at_snapshot_id,
        // Audit H-4 — caller supplies the axcheck status derived
        // from probe envelope. Backfill / dep-hash sweep callers
        // that don't have probe access pass the prior record's
        // status (preservation) or `AxcheckStatus::Skipped` (defensive
        // default for synthetic / probeless callers).
        axcheck_status,
    })
}

/// Patch C-P HIGH 1 (b) — populate `kernel_semantic_hashes` on a record
/// from the current `state.live.corr_current_fingerprints`. Covers every
/// dep across all three categories. Empty string for deps the kernel
/// has not yet fingerprinted (rare but possible during early bursts).
///
/// Called by sites that own a fresh `LocalClosureRecord` and have access
/// to live state: the deterministic-revalidation pass and any future
/// site that synthesizes a record from probe output. The engine itself
/// populates the field inline at record-creation time (see
/// `apply_local_closure_acceptance_bookkeeping` in engine.rs).
///
/// Patch C-Q Q11 — delegates to `trellis_kernel::model::populate_kernel_semantic_hashes`
/// so the engine and runtime-CLI sites share one loop.
fn populate_kernel_semantic_hashes_from_state(
    record: &mut LocalClosureRecord,
    state: &ProtocolState,
) {
    trellis_kernel::model::populate_kernel_semantic_hashes(record, state);
}

/// Detect a record that still carries C-B sentinel hash inputs and
/// therefore needs a backfill pass.
fn record_needs_hash_backfill(record: &LocalClosureRecord) -> bool {
    record.closure_version == CLOSURE_VERSION_SENTINEL
        || record.toolchain_hash == CLOSURE_HASH_SENTINEL
        || record.lake_manifest_hash == CLOSURE_HASH_SENTINEL
        || record.preamble_hash == CLOSURE_HASH_SENTINEL
        || record.approved_axioms_hash == CLOSURE_HASH_SENTINEL
        || record.active_decl_hash == CLOSURE_HASH_SENTINEL
        || record.active_statement_hash == CLOSURE_HASH_SENTINEL
}

/// Walk `state.local_closure_records` and replace any C-B sentinel hash
/// fields with computed ones. Returns true iff at least one record was
/// rewritten (so the caller knows whether to persist).
///
/// Trigger: after every `step_with_checkpoint_sink` that may have
/// installed sentinel records via `apply_local_closure_acceptance_bookkeeping`'s
/// sorryd→sorry-free arm. Idempotent: a fully-populated record is left
/// alone.
/// Patch C-Q Q6 — outcome of a backfill pass. `mutated` is true iff at
/// least one record was rewritten OR demoted. `demoted_nodes` lists the
/// nodes whose sentinel record was demoted to an internal_error failure
/// (hash backfill failed). The caller is responsible for deleting the
/// corresponding persisted JSON file under
/// `<runtime_root>/checker-state/local-closure-records/`. Mirrors the
/// `ProtocolCommand::DeleteLocalClosureRecord` semantic the engine emits
/// for in-band invalidation, but expressed as a return value because
/// backfill runs at the runtime-CLI layer (outside the engine).
#[derive(Debug, Default)]
struct BackfillOutcome {
    mutated: bool,
    demoted_nodes: Vec<NodeId>,
}

fn backfill_local_closure_record_hashes(
    state: &mut ProtocolState,
    repo: &Path,
    current_cycle: u64,
) -> BackfillOutcome {
    let nodes_to_backfill: Vec<NodeId> = state
        .local_closure_records
        .iter()
        .filter_map(|(node, record)| {
            if record_needs_hash_backfill(record) {
                Some(node.clone())
            } else {
                None
            }
        })
        .collect();
    if nodes_to_backfill.is_empty() {
        return BackfillOutcome::default();
    }
    let mut outcome = BackfillOutcome::default();
    for node in nodes_to_backfill {
        if let Some(record) = state.local_closure_records.get(&node).cloned() {
            // Patch C-O MEDIUM 1 — fail closed on hash-compute failure
            // for sentinel records. Previously, if backfill failed
            // (e.g. corrupted APPROVED_AXIOMS.json), the sentinel record
            // remained in `local_closure_records` and satisfied
            // `formalization_complete` (which only checks record
            // presence) until the next restart rejected it. Now we
            // demote the sentinel to an `internal_error` failure +
            // unverified entry so the operator sees the problem and the
            // completion gate stays closed.
            // Audit H-4 — backfill preserves the original record's
            // `axcheck_status`; backfill is purely about replacing
            // sentinel hash fields with disk-fresh ones and must not
            // upgrade a `Skipped` record to `Agreed`. The probe-time
            // status is the source of truth.
            match compute_local_closure_record_inputs(
                repo,
                &node,
                &record.kernel_axioms,
                &record.boundary_theorems,
                &record.strict_theorem_deps,
                &record.strict_definition_deps,
                record.accepted_at_snapshot_id.clone(),
                record.axcheck_status,
            ) {
                Ok(mut refreshed) => {
                    // Patch C-P HIGH 1 (b) — preserve the
                    // probe-time-captured `kernel_semantic_hashes`
                    // through backfill. The field is set at probe time
                    // by the engine and is independent of the on-disk
                    // hash inputs that backfill is replacing; copy it
                    // forward so migration-time drift detection
                    // continues to work for the refreshed record.
                    refreshed.kernel_semantic_hashes = record.kernel_semantic_hashes.clone();
                    state.local_closure_records.insert(node, refreshed);
                    outcome.mutated = true;
                }
                Err(err) => {
                    eprintln!(
                        "[local-closure backfill] demoting {} to internal_error: {err}",
                        node.as_str()
                    );
                    state.local_closure_records.remove(&node);
                    let summary = ErrorSummary {
                        status: "internal_error".to_string(),
                        returncode: -1,
                        timed_out: false,
                        stderr_excerpt: {
                            let raw = format!("backfill failed: {err}");
                            if raw.len() > 1024 {
                                raw[..1024].to_string()
                            } else {
                                raw
                            }
                        },
                        axiom_violations: Vec::new(),
                        strict_errors: Vec::new(),
                        captured_at_cycle: current_cycle,
                        retry_count: 0,
                        last_attempt_cycle: current_cycle,
                        next_retry_cycle: 0,
                        retry_exhausted: false,
                    };
                    state.local_closure_failures.insert(node.clone(), summary);
                    state.local_closure_unverified_nodes.insert(node.clone());
                    outcome.mutated = true;
                    // Patch C-Q Q6 — surface the demote so the caller
                    // can remove the stale persisted JSON. Without
                    // this, a rewind or state-file loss could reload
                    // the sentinel from disk and clobber the
                    // internal_error failure we just installed.
                    outcome.demoted_nodes.push(node);
                }
            }
        }
    }
    outcome
}

/// Plan §7.0 / §7.4.1 — build an `ErrorSummary` from a transport-layer
/// error. Increments the retry counter relative to any prior summary and
/// computes the next retry cycle via exponential backoff capped at
/// `TRANSPORT_BACKOFF_MAX_CYCLES`. Sets `retry_exhausted=true` when the
/// post-increment count exceeds `TRANSPORT_RETRY_BUDGET`.
fn build_transport_error_summary(
    err: &str,
    prior: Option<&ErrorSummary>,
    current_cycle: u64,
) -> ErrorSummary {
    let retry_count = prior
        .filter(|s| s.status == "transport_error")
        .map(|s| s.retry_count.saturating_add(1))
        .unwrap_or(0);
    let backoff = (1u64.checked_shl(retry_count.min(63)).unwrap_or(u64::MAX))
        .min(TRANSPORT_BACKOFF_MAX_CYCLES);
    let next_retry_cycle = current_cycle.saturating_add(backoff);
    ErrorSummary {
        status: "transport_error".to_string(),
        returncode: -1,
        timed_out: false,
        stderr_excerpt: if err.len() > 1024 {
            err[..1024].to_string()
        } else {
            err.to_string()
        },
        axiom_violations: Vec::new(),
        strict_errors: Vec::new(),
        captured_at_cycle: current_cycle,
        retry_count,
        last_attempt_cycle: current_cycle,
        next_retry_cycle,
        retry_exhausted: retry_count > TRANSPORT_RETRY_BUDGET,
    }
}

/// Plan §7.0 — build an `ErrorSummary` from a probe that completed but
/// rejected (proof-shape failure, axiom violation, strict error, etc.).
/// Non-transport probes retry on every revalidation pass; the backoff
/// fields stay at their `Default` zeros.
fn build_failure_summary(
    probe: &LocalClosureProbeOutput,
    approved: &BTreeSet<String>,
    current_cycle: u64,
) -> ErrorSummary {
    let stderr_excerpt = if probe.raw_stderr.len() > 1024 {
        probe.raw_stderr[..1024].to_string()
    } else {
        probe.raw_stderr.clone()
    };
    let axiom_violations: Vec<String> = probe
        .kernel_axioms
        .iter()
        .filter(|a| !approved.contains(a.as_str()))
        .cloned()
        .collect();
    ErrorSummary {
        status: if probe.status.is_empty() {
            "internal_error".to_string()
        } else {
            probe.status.clone()
        },
        returncode: probe.returncode,
        timed_out: probe.timed_out,
        stderr_excerpt,
        axiom_violations,
        strict_errors: probe.errors.clone(),
        captured_at_cycle: current_cycle,
        retry_count: 0,
        last_attempt_cycle: current_cycle,
        next_retry_cycle: 0,
        retry_exhausted: false,
    }
}

/// Audit MEDIUM (approved-axioms load errors) — build an
/// `internal_error` summary for an approved-axioms file that failed to
/// load (I/O or parse). The error message is preserved in
/// `stderr_excerpt` (truncated to the standard 1024-byte cap) so the
/// operator sees the load failure as a diagnostic rather than a silent
/// "empty approved set" hash that would mislabel real axiom violations.
fn build_approved_axioms_load_error_summary(load_err: &str, current_cycle: u64) -> ErrorSummary {
    let stderr_excerpt = if load_err.len() > 1024 {
        load_err[..1024].to_string()
    } else {
        load_err.to_string()
    };
    ErrorSummary {
        status: "internal_error".to_string(),
        returncode: -1,
        timed_out: false,
        stderr_excerpt,
        axiom_violations: Vec::new(),
        strict_errors: Vec::new(),
        captured_at_cycle: current_cycle,
        retry_count: 0,
        last_attempt_cycle: current_cycle,
        next_retry_cycle: 0,
        retry_exhausted: false,
    }
}

/// Plan §7.5 — deterministic-revalidation pass. Iterates the entire
/// unverified set and runs the local-closure probe for each (subject to
/// the transport-error backoff gate), producing a `RevalidationBatch`
/// for the engine's `apply_revalidation_batch` API.
///
/// Patch C-M: the per-pass chunking cap was removed. Operator decision:
/// deferring probe work across cycles only blocks `Cleanup` longer for
/// no benefit; the total probing work is identical either way, so drain
/// the full set in one call.
///
/// The probe runner is injected as a closure so tests can substitute a
/// canned response set without depending on the checker socket. The
/// production wrapper (`deterministic_revalidate_at_cli`) wires this to
/// `run_local_closure_axioms`.
fn deterministic_revalidate_at_cli_with_probe<F>(
    state: &ProtocolState,
    repo: &Path,
    current_cycle: u64,
    mut probe: F,
) -> RevalidationBatch
where
    F: FnMut(&Path, &str) -> Result<LocalClosureProbeOutput, String>,
{
    let mut batch = RevalidationBatch::default();
    let nodes: Vec<NodeId> = state
        .local_closure_unverified_nodes
        .iter()
        .cloned()
        .collect();
    for node in nodes {
        // Transport-error backoff gate (plan §7.0/§7.4.1).
        if let Some(prior) = state.local_closure_failures.get(&node) {
            if prior.status == "transport_error" {
                if prior.retry_exhausted {
                    continue;
                }
                if current_cycle < prior.next_retry_cycle {
                    continue;
                }
            }
        }
        let mut result = match probe(repo, node.as_str()) {
            Ok(r) => r,
            Err(transport_err) => {
                let summary = build_transport_error_summary(
                    &transport_err,
                    state.local_closure_failures.get(&node),
                    current_cycle,
                );
                batch.still_unverified.push((node.clone(), summary));
                continue;
            }
        };
        // Patch C-Q Q1 — dep name/kind validation BEFORE record
        // construction. The worker MCA path called this in
        // `proof_worker_delta_step_result` (Patch C-K/C-N); this
        // deterministic path was missing it. A probe whose dep map
        // contains a node absent from `live.present_nodes` (or with a
        // mismatched kind) would otherwise produce a refreshed record
        // whose dep key is not tied to the kernel's lifecycle. The
        // validator flips `status` to `internal_error` on failure;
        // the existing `result.status == "ok"` gate then routes to
        // `build_failure_summary` (still_unverified arm) and no record
        // is persisted.
        validate_probe_present_nodes(&mut result, &state.live.present_nodes, &state.node_kinds);
        // Audit MEDIUM (approved-axioms load errors): a load failure
        // is an infrastructure/config error, not a probe outcome. Skip
        // any subset-check / record install and write an
        // `internal_error` summary so the operator sees the load
        // failure as a diagnostic rather than a silent "empty approved
        // set" hash that would mislabel real axiom violations.
        let approved = match load_approved_axioms(repo, node.as_str()) {
            Ok(a) => a,
            Err(load_err) => {
                let summary = build_approved_axioms_load_error_summary(&load_err, current_cycle);
                batch.still_unverified.push((node.clone(), summary));
                continue;
            }
        };
        let kernel_subset = result.kernel_axioms.is_subset(&approved);
        if result.status == "ok" && kernel_subset && result.errors.is_empty() {
            // Audit H-4 — derive axcheck status from the probe's
            // `axiomization_check` sub-object so revalidation records
            // carry the same telemetry as engine-installed records.
            // Mirrors the engine derivation in
            // `apply_local_closure_acceptance_bookkeeping`.
            let axcheck_status = match &result.axiomization_check {
                Some(ax) if ax.skipped => AxcheckStatus::Skipped,
                Some(ax) if ax.agreed => AxcheckStatus::Agreed,
                Some(_) => AxcheckStatus::Disagreed,
                None => AxcheckStatus::Skipped,
            };
            match compute_local_closure_record_inputs(
                repo,
                &node,
                &result.kernel_axioms,
                &result.boundary_theorems,
                &result.strict_theorem_deps,
                &result.strict_definition_deps,
                format!("revalidate-cycle-{}", current_cycle),
                axcheck_status,
            ) {
                Ok(mut record) => {
                    // Patch C-P HIGH 1 (b) — stamp kernel_semantic_hashes
                    // from live state so migration-time drift detection
                    // works against any future stale persisted copy.
                    populate_kernel_semantic_hashes_from_state(&mut record, state);
                    batch.refreshed.push((node, record));
                }
                Err(load_err) => {
                    // approved-axioms load failed between the subset
                    // check and the record build — surface the load
                    // error rather than installing a record with a
                    // wrong hash.
                    let summary =
                        build_approved_axioms_load_error_summary(&load_err, current_cycle);
                    batch.still_unverified.push((node, summary));
                }
            }
        } else {
            let summary = build_failure_summary(&result, &approved, current_cycle);
            batch.still_unverified.push((node, summary));
        }
    }
    batch
}

/// Production wrapper for `deterministic_revalidate_at_cli_with_probe`.
/// Wires the probe runner to `run_local_closure_axioms`.
fn deterministic_revalidate_at_cli(
    state: &ProtocolState,
    repo: &Path,
    current_cycle: u64,
) -> RevalidationBatch {
    deterministic_revalidate_at_cli_with_probe(state, repo, current_cycle, run_local_closure_axioms)
}

/// Plan §7.10 — persist a fresh record to disk so a supervisor kill
/// mid-migration loses no progress. JSON includes a `_persisted_at_cycle`
/// diagnostic alongside the record fields.
fn persist_record_to_disk(
    records_dir: &Path,
    record: &LocalClosureRecord,
    cycle: u64,
) -> Result<(), String> {
    fs::create_dir_all(records_dir).map_err(|err| {
        format!(
            "failed to create local-closure records dir {}: {err}",
            records_dir.display()
        )
    })?;
    let mut value = serde_json::to_value(record)
        .map_err(|err| format!("failed to serialize local-closure record: {err}"))?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "_persisted_at_cycle".to_string(),
            serde_json::Value::from(cycle),
        );
    }
    // Patch C-Q Q5 — use the shared filename helper so the
    // persistence side and `delete_persisted_local_closure_record`
    // (runtime.rs) stay in lockstep. The helper escapes `/` in node
    // IDs identically on both sides; even though current `NodeId`s
    // don't contain `/`, centralizing the rule future-proofs the
    // save/delete pair.
    let path = records_dir.join(trellis_kernel::runtime::persisted_record_file_name(
        &record.node,
    ));
    // Defensive belt-and-suspenders: keep the suffix constant in sync
    // for visibility — the helper hard-codes `.json` (see runtime.rs);
    // this assertion would fire if `LOCAL_CLOSURE_RECORDS_EXT` drifts
    // away from "json" without updating the helper.
    debug_assert_eq!(LOCAL_CLOSURE_RECORDS_EXT, "json");
    let text = serde_json::to_string_pretty(&value)
        .map_err(|err| format!("failed to serialize local-closure record JSON: {err}"))?;
    fs::write(&path, text).map_err(|err| {
        format!(
            "failed to write local-closure record {}: {err}",
            path.display()
        )
    })?;
    Ok(())
}

/// Load a persisted record from disk. Strips the diagnostic
/// `_persisted_at_cycle` field (if present) before deserializing.
fn load_persisted_record(path: &Path) -> Result<LocalClosureRecord, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("_persisted_at_cycle");
    }
    serde_json::from_value::<LocalClosureRecord>(value)
        .map_err(|err| format!("failed to deserialize record at {}: {err}", path.display()))
}

/// Plan §7.10 step 3 — install-time hash revalidation. A persisted
/// record is only installed if every input hash matches current state.
///
/// Audit HIGH 1 fix: also validates each stored dep hash
/// (`boundary_theorems`, `strict_theorem_deps`, `strict_definition_deps`)
/// against any cross-record evidence in `state`. The probe-derived hash
/// formats (statement_hash / value_hash / semantic_hash) are produced by
/// `scripts/lean_local_closure.lean` and cannot be recomputed Rust-side
/// without re-running the probe; we therefore use the strongest check
/// available without re-probing: if any OTHER in-state record references
/// the same dep node under the same dep kind with a different hash, at
/// least one of those records is stale, so we conservatively reject the
/// candidate. This catches the audit's scenario where a boundary helper /
/// strict dep changed and another consumer's record (refreshed via the
/// engine's invalidation walk or a deterministic-revalidation pass) now
/// disagrees on the dep's hash. Records without cross-references cannot
/// be cross-checked this way; the deterministic-revalidation pass and
/// engine invalidation walk remain the primary defenses there.
fn record_hashes_match_current(
    record: &LocalClosureRecord,
    repo: &Path,
    state: &ProtocolState,
) -> bool {
    if record.closure_version != CLOSURE_VERSION {
        return false;
    }
    let toolchain = hash_file_or_empty(&repo.join("lean-toolchain"));
    if record.toolchain_hash != toolchain {
        return false;
    }
    let manifest = hash_file_or_empty(&repo.join("lake-manifest.json"));
    if record.lake_manifest_hash != manifest {
        return false;
    }
    let preamble = hash_file_or_empty(&repo.join("Tablet/Preamble.lean"));
    if record.preamble_hash != preamble {
        return false;
    }
    // Audit MEDIUM (approved-axioms load errors): a load failure means
    // we cannot validate the record's `approved_axioms_hash`; fail
    // closed (reject) rather than silently accepting against a wrong
    // "empty approved set" hash.
    let approved = match hash_approved_axioms_for_node(repo, record.node.as_str()) {
        Ok(h) => h,
        Err(_) => return false,
    };
    if record.approved_axioms_hash != approved {
        return false;
    }
    // Audit H-4 — startup/migration side of axcheck-policy rescission.
    // If current config requires the secondary axcheck collector, a
    // persisted record generated while axcheck was skipped must not
    // reinstall as verified.
    if local_closure_axcheck_required_for_repo(repo)
        && record.axcheck_status != AxcheckStatus::Agreed
    {
        return false;
    }
    let active_decl = active_decl_hash_for_node(repo, record.node.as_str());
    if record.active_decl_hash != active_decl {
        return false;
    }
    let active_stmt = active_statement_hash_for_node(repo, record.node.as_str());
    if record.active_statement_hash != active_stmt {
        return false;
    }
    // Audit HIGH 1: validate every stored dep hash against any
    // cross-record evidence currently in state.
    if !record_dep_hashes_consistent_with_state(record, state) {
        return false;
    }
    true
}

/// Audit HIGH 1 + Patch C-O HIGH 1 (b) + Patch C-P HIGH 1 (b) helper —
/// true iff every stored dep on the record is corroborated by current
/// state. Layered checks, in order of strictness:
///
///   * The dep MUST be in `state.live.present_nodes`. A ghost dep
///     (referenced in the record but no longer present) is a stale
///     record.
///   * Patch C-P HIGH 1 (b): if the record carries a probe-time
///     `kernel_semantic_hash` for the dep, it MUST equal the current
///     `state.live.corr_current_fingerprints[dep]`. A mismatch (or a
///     missing-from-state entry on a present dep) means the dep's
///     meaning surface drifted since the record was written; reject.
///     This is the canonical drift check: it catches silent dep drift
///     where neither record's invalidation flag was set, mutual-stale
///     pairs that would cross-validate under the old check, off-protocol
///     edits between supervisor stops, and iteration-order dependent
///     migrations.
///   * Patch C-O strict-signal fallback (retained as a belt-and-
///     suspenders layer for records persisted before Patch C-P added
///     `kernel_semantic_hashes`): if the dep itself is in
///     `state.local_closure_unverified_nodes` or
///     `state.local_closure_failures`, the dep is in flux. Pre-Patch-C-P
///     records have an empty `kernel_semantic_hashes` map (per
///     `#[serde(default)]`), so the kernel-hash check above doesn't fire
///     for them; this fallback still gives those records the C-O
///     guarantee.
///   * The existing cross-record agreement check is retained: if
///     another record in state names the same dep under the same kind
///     with a different hash, the candidate is stale.
///
/// Skips `record.node` itself so a record can be re-installed against
/// an earlier in-state copy of itself.
fn record_dep_hashes_consistent_with_state(
    record: &LocalClosureRecord,
    state: &ProtocolState,
) -> bool {
    let dep_groups: [&BTreeMap<NodeId, String>; 3] = [
        &record.boundary_theorems,
        &record.strict_theorem_deps,
        &record.strict_definition_deps,
    ];
    for group in dep_groups {
        for dep in group.keys() {
            if !state.live.present_nodes.contains(dep) {
                return false;
            }
        }
    }
    // Patch C-P HIGH 1 (b) — kernel `semantic_hash` drift check. Any
    // dep whose recorded kernel hash disagrees with the current
    // `corr_current_fingerprints` value (or whose entry has been
    // deleted from current state) means the record is stale. Empty
    // strings (rare: dep not yet fingerprinted at record-creation
    // time) match against a current empty/missing entry.
    for (dep, recorded_hash) in &record.kernel_semantic_hashes {
        let current_hash = state.live.corr_current_fingerprints.get(dep);
        match current_hash {
            Some(current) if current == recorded_hash => continue,
            _ => return false,
        }
    }
    // Patch C-O HIGH 1 (b) strict-signal fallback — only fires for
    // pre-Patch-C-P records (those with an empty `kernel_semantic_hashes`
    // map). Post-Patch-C-P records carry hashes for every dep, so the
    // check above is authoritative. Kept for back-compat with persisted
    // records written before this deploy.
    if record.kernel_semantic_hashes.is_empty() {
        for group in dep_groups {
            for dep in group.keys() {
                if state.local_closure_unverified_nodes.contains(dep) {
                    return false;
                }
                if state.local_closure_failures.contains_key(dep) {
                    return false;
                }
            }
        }
    }
    for (dep, recorded_hash) in &record.boundary_theorems {
        for (other_node, other) in &state.local_closure_records {
            if other_node == &record.node {
                continue;
            }
            if let Some(other_hash) = other.boundary_theorems.get(dep) {
                if other_hash != recorded_hash {
                    return false;
                }
            }
        }
    }
    for (dep, recorded_hash) in &record.strict_theorem_deps {
        for (other_node, other) in &state.local_closure_records {
            if other_node == &record.node {
                continue;
            }
            if let Some(other_hash) = other.strict_theorem_deps.get(dep) {
                if other_hash != recorded_hash {
                    return false;
                }
            }
        }
    }
    for (dep, recorded_hash) in &record.strict_definition_deps {
        for (other_node, other) in &state.local_closure_records {
            if other_node == &record.node {
                continue;
            }
            if let Some(other_hash) = other.strict_definition_deps.get(dep) {
                if other_hash != recorded_hash {
                    return false;
                }
            }
        }
    }
    true
}

/// Plan §7.10 — first-deploy migration. Idempotent: returns false on
/// subsequent calls when no work remains, so safe to run at every
/// supervisor startup. Returns true if any state mutation occurred.
///
/// Steps:
/// 1. Scan persisted records, install those whose hashes match current state.
/// 2. Identify sorry-free proof_nodes lacking a record; insert into unverified set.
/// 3. Run a deterministic-revalidation pass; persist refreshed records.
/// 4. Recompute reverse indices.
///
/// The probe runner is injected for testability; production calls pass
/// `run_local_closure_axioms`.
fn run_migration_if_needed_with_probe<F>(
    state: &mut ProtocolState,
    repo: &Path,
    runtime_root: &Path,
    current_cycle: u64,
    probe: F,
) -> Result<bool, String>
where
    F: FnMut(&Path, &str) -> Result<LocalClosureProbeOutput, String>,
{
    // Audit NR-1 — sentinel-record persistence window. The engine's
    // `apply_local_closure_acceptance_bookkeeping` installs
    // sentinel-hashed records into in-memory state at the sorry-free
    // arm; the runtime CLI's post-step `backfill_local_closure_record_hashes`
    // replaces sentinels with real hashes BEFORE the next step. But
    // there's a persistence window: `step_with_checkpoint_sink` calls
    // `persist_state` (writing state.json with the sentinel record)
    // BEFORE `backfill_local_closure_record_hashes` runs. If the
    // process dies in that window, the persisted state.json has a
    // sentinel record. On restart, migration's `record-load` loop at
    // line 3828 skips re-loading the disk record because
    // `state.local_closure_records.contains_key` is true; the
    // `needs_probe` filter at line 3870 also excludes the node. The
    // sentinel record survives until the NEXT step's backfill — and
    // during that window `formalization_complete()` sees a
    // present-but-sentinel record and may incorrectly allow phase
    // advancement.
    //
    // Fix: at migration entry, sweep `state.local_closure_records`
    // and demote any sentinel-shaped record into the
    // `local_closure_unverified_nodes` set. This forces the
    // deterministic-revalidation pass below to re-probe the node,
    // producing a real-hashed record.
    let sentinel_demotion_count = {
        let sentinels: Vec<NodeId> = state
            .local_closure_records
            .iter()
            .filter_map(|(node, record)| {
                if record_needs_hash_backfill(record) {
                    Some(node.clone())
                } else {
                    None
                }
            })
            .collect();
        let count = sentinels.len();
        for node in sentinels {
            state.local_closure_records.remove(&node);
            state.local_closure_unverified_nodes.insert(node);
        }
        count
    };
    if sentinel_demotion_count > 0 {
        eprintln!(
            "[local-closure migration] NR-1 sentinel sweep: demoted {} sentinel record(s) to \
             unverified so deterministic revalidation re-probes them",
            sentinel_demotion_count
        );
    }
    let records_dir = local_closure_records_dir(runtime_root);
    let mut mutated_by_load = sentinel_demotion_count > 0;
    if records_dir.exists() {
        if let Ok(entries) = fs::read_dir(&records_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Ok(record) = load_persisted_record(&path) {
                        if state.local_closure_records.contains_key(&record.node) {
                            continue;
                        }
                        // Patch C-O HIGH 1 (a): tombstone-respect. The
                        // in-memory `local_closure_unverified_nodes` and
                        // `local_closure_failures` are authoritative
                        // tombstones — they explicitly mark the prior
                        // record as invalidated and force a re-probe.
                        // A persisted disk record must not override
                        // those tombstones; let the probe pass decide.
                        if state.local_closure_unverified_nodes.contains(&record.node) {
                            continue;
                        }
                        if state.local_closure_failures.contains_key(&record.node) {
                            continue;
                        }
                        // Audit NR-1 — reject sentinel-shaped persisted
                        // records at disk-load time too. Belt-and-braces
                        // with the in-memory sweep at the top of this
                        // function: in the typical case the per-node
                        // disk file is written by `step_runtime`'s
                        // post-step backfill sweep AFTER the engine has
                        // already replaced sentinels with real hashes,
                        // so sentinels on disk are rare. But a future
                        // refactor that persists records earlier (or a
                        // hand-edited state file used for testing) could
                        // surface them; reject here too so the migration
                        // forces a re-probe rather than blessing
                        // sentinel hashes as legitimate.
                        if record_needs_hash_backfill(&record) {
                            eprintln!(
                                "[local-closure migration] NR-1: rejecting persisted sentinel \
                                 record for {} ({}); deterministic revalidation will re-probe",
                                record.node.as_str(),
                                path.display(),
                            );
                            state
                                .local_closure_unverified_nodes
                                .insert(record.node.clone());
                            mutated_by_load = true;
                            continue;
                        }
                        if !record_hashes_match_current(&record, repo, state) {
                            continue;
                        }
                        let node = record.node.clone();
                        state.local_closure_records.insert(node.clone(), record);
                        state.local_closure_unverified_nodes.remove(&node);
                        state.local_closure_failures.remove(&node);
                        mutated_by_load = true;
                    }
                }
            }
        }
    }
    // Patch C-Q Q4 (defense in depth): require `present_nodes`
    // membership too. Without this check, a node listed in
    // `proof_nodes` but no longer in `live.present_nodes` would
    // transiently land in `local_closure_unverified_nodes` (the insert
    // below) before `apply_revalidation_batch` later drops the
    // resulting batch entry via the present-only filter. That window
    // breaks the §7.0 invariant `unverified ⊆ present_nodes`; this
    // filter prevents the insert in the first place.
    let needs_probe: Vec<NodeId> = state
        .proof_nodes
        .iter()
        .filter(|n| state.live.present_nodes.contains(n.as_str()))
        .filter(|n| !state.live.open_nodes.contains(n.as_str()))
        .filter(|n| !state.local_closure_records.contains_key(n.as_str()))
        .cloned()
        .collect();
    let mut mutated = mutated_by_load || !needs_probe.is_empty();
    for n in needs_probe {
        state.local_closure_unverified_nodes.insert(n);
    }
    if !state.local_closure_unverified_nodes.is_empty() {
        let batch = deterministic_revalidate_at_cli_with_probe(state, repo, current_cycle, probe);
        for (_, record) in &batch.refreshed {
            let _ = persist_record_to_disk(&records_dir, record, current_cycle);
        }
        if !batch.refreshed.is_empty() || !batch.still_unverified.is_empty() {
            mutated = true;
        }
        trellis_kernel::engine::apply_revalidation_batch(state, batch);
    }
    if mutated {
        trellis_kernel::model::recompute_local_closure_reverse_indices(state);
    }
    Ok(mutated)
}

/// Production migration wrapper — wires the probe runner to
/// `run_local_closure_axioms`.
fn run_migration_if_needed(
    state: &mut ProtocolState,
    repo: &Path,
    runtime_root: &Path,
    current_cycle: u64,
) -> Result<bool, String> {
    run_migration_if_needed_with_probe(
        state,
        repo,
        runtime_root,
        current_cycle,
        run_local_closure_axioms,
    )
}

/// Audit guard — true iff the current state is at a safe lifecycle
/// point for running the local-closure migration. Returns
/// `Some(skip_reason)` when the migration should NOT run.
///
/// Skip when (post-Patch-C-O tightening):
///   1. `phase == Cleanup`: migration introduces unverified nodes that
///      would block Cleanup completion (`formalization_complete`'s
///      records_present clause); defer until the next phase transition.
///   2. **Any** request is in flight (Worker / Review / Corr / Paper /
///      Sound). Worker-in-flight has the original "disk may contain
///      unaccepted edits" hazard. Review/Corr/Paper/Sound in flight
///      means the kernel already dispatched a prompt to an external
///      agent; mutating local-closure state behind that prompt would
///      drift the agent's view from the post-state the response will
///      be checked against (the audit's prompt/legality-drift hazard).
///
/// The earlier C-H pass only skipped on Worker; C-O tightened to any
/// in-flight kind. See `CLAUDES_NOTES_deploy_playbook.md` §8 for the
/// operator-visible consequence: startup migration only runs when the
/// supervisor was deliberately stopped at a no-in-flight-request
/// boundary. Naturally-occurring no-request gaps don't exist between
/// cycles in a normal run.
///
/// Returns `None` when migration is safe to run: no Cleanup, no
/// in-flight request of any kind.
fn local_closure_migration_skip_reason(state: &ProtocolState) -> Option<String> {
    if state.phase == trellis_kernel::Phase::Cleanup {
        return Some(
            "skipping migration in Cleanup phase; defer until next phase transition.".to_string(),
        );
    }
    if let Some(req) = state.in_flight_request.as_ref() {
        // Patch C-O MEDIUM 2: tighten from "Worker only" to "any in-flight
        // request." A Review/Corr/Sound prompt already references the
        // current blocker/legality snapshot; silently mutating state under
        // it (e.g. via migration installing new records or clearing
        // unverified entries) drifts the prompt away from the post-state
        // the response will be checked against.
        return Some(format!(
            "in-flight {:?} request; defer migration until next request boundary.",
            req.kind
        ));
    }
    None
}

/// Run the migration once on supervisor startup. Errors propagate as
/// transient diagnostics — the supervisor must be able to make progress
/// even when migration fails to capture some nodes (e.g. a probe
/// transport hiccup leaves them in `local_closure_unverified_nodes`,
/// which the per-step revalidator drains on subsequent steps).
///
/// Patch C-M: the per-pass chunking cap was removed — the revalidator
/// now drains the entire unverified set in a single call, so nothing is
/// deferred between cycles.
///
/// Audit HIGH 5: gated to safe request-lifecycle points only; see
/// `local_closure_migration_skip_reason`.
fn run_local_closure_migration_if_configured(
    runtime: &mut SupervisorRuntime,
) -> Result<(), String> {
    let Some(repo) = runtime.metadata().repo_path.clone() else {
        return Ok(());
    };
    if !repo.join("Tablet").is_dir() {
        return Ok(());
    }
    // Audit HIGH 5: skip migration at unsafe times (Cleanup phase or
    // pending Worker response). Avoids capturing unaccepted-edit disk
    // state in persisted records and prevents migration from blocking
    // Cleanup completion via newly-introduced unverified nodes.
    if let Some(reason) = local_closure_migration_skip_reason(runtime.state()) {
        eprintln!("[local-closure migration] {reason}");
        return Ok(());
    }
    let runtime_root = runtime.paths().root.clone();
    let current_cycle = runtime.state().cycle as u64;
    runtime
        .try_post_load_state_migration(|state| {
            run_migration_if_needed(state, &repo, &runtime_root, current_cycle)
        })
        .map_err(|err| format!("local-closure migration failed: {err}"))?;
    Ok(())
}

/// Test/production split of the pre-step revalidation flow.
///
/// Patch C-Q Q8 (doc refresh): this hook is gated. It fires only when
/// (a) the unverified set is non-empty AND (b) either no request is in
/// flight at all, or the in-flight request is `Review`. All other
/// in-flight request kinds (Worker/Paper/Corr/Sound/HumanGate/...) skip
/// the hook outright — see the explicit `if req.kind != RequestKind::Review`
/// short-circuit below for the reasoning. Returns `None` when the gate
/// blocks; otherwise mutates `state` in place via
/// `apply_revalidation_batch` and returns the batch for the caller to
/// persist/observe. Persistence to disk happens at the caller for the
/// production path; tests can elide that.
///
/// History (kept short on purpose):
/// * Patch C-F generalized the trigger from Review-only to "every
///   step where unverified is non-empty" so cold-start probes don't
///   wait for the next Review.
/// * Patch C-O HIGH 2 re-tightened that to "no in-flight OR Review"
///   so a Worker/Paper/Corr/Sound prompt-in-flight can't probe against
///   unaccepted WIP on disk.
/// * Patch C-M: the per-pass chunking cap was removed — every naked
///   unverified node is probed in a single call.
fn run_pre_step_revalidation_if_needed_pure<F>(
    state: &mut ProtocolState,
    repo: &Path,
    runtime_root: &Path,
    current_cycle: u64,
    probe: F,
) -> Option<RevalidationBatch>
where
    F: FnMut(&Path, &str) -> Result<LocalClosureProbeOutput, String>,
{
    if state.local_closure_unverified_nodes.is_empty() {
        return None;
    }
    // Patch C-O HIGH 2 — fire only when the next request to be
    // generated is a Review (or no request is in flight at all). The
    // unverified set is only consulted for reviewer decisions; workers
    // don't care, and the auto-scheduler post-C-F only schedules nodes
    // with failure records, not naked-unverified. Firing during a
    // Worker/Paper/Corr/Sound request in flight has two hazards:
    //   1. The repo may contain unaccepted Worker WIP; probing/persisting
    //      records against that disk state would capture a snapshot the
    //      kernel may later reject.
    //   2. Mutating an already-dispatched request's
    //      `local_closure_unverified` (Patch C-J regeneration path) drifts
    //      legality context from what the agent actually saw.
    //
    // Wrapper-level gate (vs. engine-level injection just before Review
    // prompt construction): keep the hook in `step_runtime` and skip
    // when the in-flight request is a non-Review kind. Patch C-J's
    // in-flight regeneration is no longer needed because we never run
    // under a Worker/Corr/Paper/Sound prompt, so this version drops it.
    if let Some(req) = state.in_flight_request.as_ref() {
        if req.kind != RequestKind::Review {
            return None;
        }
    }
    let batch = deterministic_revalidate_at_cli_with_probe(state, repo, current_cycle, probe);
    let records_dir = local_closure_records_dir(runtime_root);
    for (_, record) in &batch.refreshed {
        let _ = persist_record_to_disk(&records_dir, record, current_cycle);
    }
    let batch_clone = batch.clone();
    let batch_mutated_unverified =
        !batch_clone.refreshed.is_empty() || !batch_clone.still_unverified.is_empty();
    trellis_kernel::engine::apply_revalidation_batch(state, batch);
    // Patch C-Q Q3 — limited regeneration scope. If the in-flight
    // request is `Review`, the request was constructed (carrying a
    // snapshot of `local_closure_unverified` / blocker context) before
    // this hook fired. `apply_revalidation_batch` may have mutated the
    // unverified set; without regeneration, the dispatched Review
    // prompt would reference state that no longer matches the kernel.
    // `apply_request_dispatch_hints` (runtime.rs) does not rebuild
    // state-derived request fields, so we have to call
    // `expected_request` here.
    //
    // Scope is intentionally narrow:
    //   * Only regenerate for `Review` (the case the auditor flagged).
    //   * Skip if the batch didn't actually refresh / fail anything
    //     (the hook ran but produced no state delta).
    //   * Skip when no request is in flight (nothing to regenerate).
    //
    // This is the limited form of C-J's universal regeneration. Other
    // request kinds (Worker/Paper/Corr/Sound) cannot reach this code:
    // the gate above returned early.
    if batch_mutated_unverified {
        if let Some(prev) = state.in_flight_request.clone() {
            if prev.kind == RequestKind::Review {
                let refreshed_request = state.expected_request(prev.id, RequestKind::Review);
                state.in_flight_request = Some(refreshed_request);
            }
        }
    }
    Some(batch_clone)
}

/// Pre-step hook (plan §7.5 trigger 1, generalized by Patch C-F, gated
/// by Patch C-O HIGH 2). When the unverified set is non-empty AND
/// either no request is in flight or the in-flight request is Review,
/// runs a deterministic pass and applies the batch via
/// `apply_revalidation_batch`. The gate prevents probing/persisting
/// records while a Worker/Corr/Paper/Sound prompt is in flight — those
/// don't consult the unverified set anyway, and Worker in particular
/// may have unaccepted WIP on disk. Persists any refreshed records so
/// migration progress carries across restarts.
fn run_pre_step_revalidation_if_needed(runtime: &mut SupervisorRuntime) -> Result<(), String> {
    let Some(repo) = runtime.metadata().repo_path.clone() else {
        return Ok(());
    };
    if !repo.join("Tablet").is_dir() {
        return Ok(());
    }
    let runtime_root = runtime.paths().root.clone();
    let current_cycle = runtime.state().cycle as u64;
    runtime
        .try_post_load_state_migration(|state| {
            let result = run_pre_step_revalidation_if_needed_pure(
                state,
                &repo,
                &runtime_root,
                current_cycle,
                run_local_closure_axioms,
            );
            Ok(result.is_some())
        })
        .map_err(|err| format!("pre-step revalidation failed: {err}"))?;
    Ok(())
}

/// Audit H-2 — approved-axiom rescission. Recompute each closure
/// record's `approved_axioms_hash` against current
/// `APPROVED_AXIOMS.json` and demote any record whose hash no longer
/// matches. The migration-time hash check (`record_hashes_match_current`)
/// catches drift on supervisor restart; this per-step hook catches
/// operators who flip the policy mid-run without restarting.
///
/// Demote = drop from `local_closure_records`, push the node into
/// `local_closure_unverified_nodes`, write a synthetic `axiom_violation`
/// failure summary. The next deterministic-revalidation pass will
/// re-probe with the new policy.
///
/// Skipped when no Tablet repo exists (synthetic / minimal runs).
/// Note: this hook is intentionally NOT gated on `in_flight_request`.
/// The policy change is an environmental fact that survives in-flight
/// prompts; defending against a worker prompt drifting under a policy
/// shift is the rescission's job, not a reason to defer it.
/// Audit H-2 — pure-state inner of the approved-axiom rescission
/// hook. Returns a list of nodes that were demoted (so the caller
/// can delete their persisted disk JSON in lockstep). Read-only on
/// disk; all state mutations land on the supplied `&mut ProtocolState`.
fn rescind_records_with_stale_approved_axioms_hash_pure(
    state: &mut ProtocolState,
    repo: &Path,
    current_cycle: u64,
) -> Vec<NodeId> {
    let mut to_demote: Vec<NodeId> = Vec::new();
    for (node, record) in &state.local_closure_records {
        let current = match hash_approved_axioms_for_node(repo, node.as_str()) {
            Ok(h) => h,
            Err(_) => {
                // Treat policy-file load failure as a
                // policy-shift indication: we cannot prove the
                // record's hash matches current policy, so
                // demote defensively.
                to_demote.push(node.clone());
                continue;
            }
        };
        if record.approved_axioms_hash != current {
            to_demote.push(node.clone());
        }
    }
    if to_demote.is_empty() {
        return to_demote;
    }
    for node in &to_demote {
        let summary = ErrorSummary {
            status: "axiom_violation".to_string(),
            returncode: -1,
            timed_out: false,
            stderr_excerpt: "approved_axioms_hash drift: APPROVED_AXIOMS.json changed since record install — record demoted, re-probe pending".to_string(),
            axiom_violations: Vec::new(),
            strict_errors: Vec::new(),
            captured_at_cycle: current_cycle,
            retry_count: 0,
            last_attempt_cycle: current_cycle,
            next_retry_cycle: 0,
            retry_exhausted: false,
        };
        state.local_closure_records.remove(node);
        state.local_closure_failures.insert(node.clone(), summary);
        state.local_closure_unverified_nodes.insert(node.clone());
    }
    trellis_kernel::model::recompute_local_closure_reverse_indices(state);
    state.ensure_local_closure_coverage();
    to_demote
}

fn rescind_records_with_stale_approved_axioms_hash(
    runtime: &mut SupervisorRuntime,
) -> Result<(), String> {
    let Some(repo) = runtime.metadata().repo_path.clone() else {
        return Ok(());
    };
    if !repo.join("Tablet").is_dir() {
        return Ok(());
    }
    let runtime_root = runtime.paths().root.clone();
    let current_cycle = runtime.state().cycle as u64;
    let mut demoted_nodes: Vec<NodeId> = Vec::new();
    runtime
        .try_post_load_state_migration(|state| {
            let demoted =
                rescind_records_with_stale_approved_axioms_hash_pure(state, &repo, current_cycle);
            let mutated = !demoted.is_empty();
            demoted_nodes = demoted;
            Ok(mutated)
        })
        .map_err(|err| format!("approved-axiom rescission failed: {err}"))?;
    // Delete persisted JSON for demoted records so disk state matches
    // the in-memory tombstone (same lockstep pattern as the cleanup-
    // batch persistence sweep and the hash backfill).
    if !demoted_nodes.is_empty() {
        remove_persisted_local_closure_record_files(
            &runtime_root,
            &demoted_nodes,
            "approved-axiom rescission",
        );
    }
    Ok(())
}

/// Audit H-4 — axcheck policy rescission. If current runtime policy
/// requires the secondary axcheck collector, any record captured while
/// axcheck was skipped or disagreed is stale. Demote it so the next
/// deterministic revalidation pass reruns the local-closure probe under
/// the current policy.
fn rescind_records_with_stale_axcheck_status_pure(
    state: &mut ProtocolState,
    axcheck_required: bool,
    current_cycle: u64,
) -> Vec<NodeId> {
    if !axcheck_required {
        return Vec::new();
    }
    let to_demote: Vec<NodeId> = state
        .local_closure_records
        .iter()
        .filter_map(|(node, record)| {
            (record.axcheck_status != AxcheckStatus::Agreed).then(|| node.clone())
        })
        .collect();
    if to_demote.is_empty() {
        return to_demote;
    }
    for node in &to_demote {
        let summary = ErrorSummary {
            status: "internal_error".to_string(),
            returncode: -1,
            timed_out: false,
            stderr_excerpt: "axcheck_status is not Agreed while local_closure_axcheck_enabled requires secondary axcheck — record demoted, re-probe pending".to_string(),
            axiom_violations: Vec::new(),
            strict_errors: Vec::new(),
            captured_at_cycle: current_cycle,
            retry_count: 0,
            last_attempt_cycle: current_cycle,
            next_retry_cycle: 0,
            retry_exhausted: false,
        };
        state.local_closure_records.remove(node);
        state.local_closure_failures.insert(node.clone(), summary);
        state.local_closure_unverified_nodes.insert(node.clone());
    }
    trellis_kernel::model::recompute_local_closure_reverse_indices(state);
    state.ensure_local_closure_coverage();
    to_demote
}

fn rescind_records_with_stale_axcheck_status(
    runtime: &mut SupervisorRuntime,
) -> Result<(), String> {
    let Some(repo) = runtime.metadata().repo_path.clone() else {
        return Ok(());
    };
    if !repo.join("Tablet").is_dir() {
        return Ok(());
    }
    let axcheck_required = local_closure_axcheck_required_for_repo(&repo);
    if !axcheck_required {
        return Ok(());
    }
    let runtime_root = runtime.paths().root.clone();
    let current_cycle = runtime.state().cycle as u64;
    let mut demoted_nodes: Vec<NodeId> = Vec::new();
    runtime
        .try_post_load_state_migration(|state| {
            let demoted = rescind_records_with_stale_axcheck_status_pure(
                state,
                axcheck_required,
                current_cycle,
            );
            let mutated = !demoted.is_empty();
            demoted_nodes = demoted;
            Ok(mutated)
        })
        .map_err(|err| format!("axcheck-status rescission failed: {err}"))?;
    if !demoted_nodes.is_empty() {
        remove_persisted_local_closure_record_files(
            &runtime_root,
            &demoted_nodes,
            "axcheck-status rescission",
        );
    }
    Ok(())
}

/// Adapter wrapper that injects `local_closure_revalidation` into a
/// cleanup-flavored Worker response BEFORE the engine processes it.
///
/// Plan §7.7: the cleanup-burst engine path checks
/// `formalization_complete()` after applying bookkeeping. Records
/// invalidated by this delta need fresh probes BEFORE that check; the
/// engine consumes `WorkerResponse.local_closure_revalidation` inside
/// `apply_local_closure_acceptance_bookkeeping`.
///
/// Patch C-Q Q2 — the adapter does NOT persist records to disk inside
/// the dispatch. If the engine subsequently rejects the cleanup response
/// (cleanup invariant violation, validation failure, ...), the records
/// must not survive on disk. Instead, the batch is stashed in
/// `WorkerResponse.local_closure_revalidation` and a shared cell
/// (`shared_batch`); after the engine returns Ok, `step_runtime`
/// persists the refreshed entries that survived the engine's eligibility
/// filter (i.e. that are still in `state.local_closure_records`).
struct CleanupRevalidationAdapter<'a, A: WrapperAdapter> {
    inner: A,
    state: &'a ProtocolState,
    repo: PathBuf,
    current_cycle: u64,
    /// Patch C-Q Q2 — shared with `step_runtime` so the post-acceptance
    /// persistence sweep can see the batch the adapter built. `None`
    /// means no cleanup batch was produced (either the request wasn't a
    /// cleanup kind, or the unverified set was empty).
    shared_batch: std::rc::Rc<std::cell::RefCell<Option<RevalidationBatch>>>,
}

impl<'a, A: WrapperAdapter> WrapperAdapter for CleanupRevalidationAdapter<'a, A> {
    fn dispatch(&mut self, request: &WrapperRequest) -> Result<WrapperResponse, String> {
        let mut response = self.inner.dispatch(request)?;
        let is_cleanup_request = matches!(
            request.worker_context.validation_kind,
            WorkerValidationKind::Cleanup | WorkerValidationKind::FinalCleanup
        );
        if is_cleanup_request {
            if let WrapperResponse::Worker(ref mut worker_response) = response {
                if worker_response.local_closure_revalidation.is_none()
                    && !self.state.local_closure_unverified_nodes.is_empty()
                {
                    let batch =
                        deterministic_revalidate_at_cli(self.state, &self.repo, self.current_cycle);
                    // Patch C-Q Q2 — stash a clone for the post-
                    // acceptance persistence sweep. The disk write is
                    // deferred until after `step_with_checkpoint_sink`
                    // returns Ok, so a subsequent engine rejection
                    // leaves no orphan persisted records.
                    *self.shared_batch.borrow_mut() = Some(batch.clone());
                    worker_response.local_closure_revalidation = Some(batch);
                }
            }
        }
        Ok(response)
    }
}

/// Helper: invoke `step_with_checkpoint_sink` with an adapter, picking
/// up the checkpoint sink from env. Centralizes the "match on env-sink"
/// pattern so the cleanup-wrap and non-wrap step paths share one body.
fn run_step_with_sink<A: WrapperAdapter>(
    runtime: &mut SupervisorRuntime,
    adapter: &mut A,
) -> Result<RuntimeStepOutcome, String> {
    match checkpoint_sink_from_env()? {
        Some(mut sink) => runtime
            .step_with_checkpoint_sink(adapter, &mut sink)
            .map_err(|err| format!("runtime step failed: {err}")),
        None => {
            let mut sink = NoopCheckpointSink;
            runtime
                .step_with_checkpoint_sink(adapter, &mut sink)
                .map_err(|err| format!("runtime step failed: {err}"))
        }
    }
}

fn step_runtime(
    runtime: &mut SupervisorRuntime,
    response: Option<WrapperResponse>,
) -> Result<RuntimeStepOutcome, String> {
    // Audit H-2 — approved-axiom rescission hook. Recompute each
    // record's `approved_axioms_hash` against current
    // `APPROVED_AXIOMS.json` and demote any record whose hash no
    // longer matches. Idempotent and cheap: hashing the policy file
    // and comparing strings, no probes. The migration-time check
    // (`record_hashes_match_current`) catches this on supervisor
    // restart; this hook catches operators who edit the policy
    // file mid-run without restarting.
    //
    // Skipped when no Tablet repo present (synthetic / minimal
    // test runs); skipped also when the in-flight request is
    // mid-cycle (per the same prompt/drift contract as the
    // pre-step revalidator) so we don't shift records under an
    // in-flight verifier prompt.
    rescind_records_with_stale_approved_axioms_hash(runtime)?;
    // Audit H-4 — paired policy hook for records captured while
    // local-closure axcheck was disabled/skipped. The H-2 hook above
    // handles approved-axiom hash drift; this one handles axcheck policy
    // drift.
    rescind_records_with_stale_axcheck_status(runtime)?;
    // Plan §7.5 trigger 1 — deterministic-revalidation pass before the
    // step when the unverified set is non-empty.
    //
    // Patch C-Q Q8 (doc refresh): the hook is gated. It fires only when
    // there's no in-flight request OR the in-flight request is
    // `Review`. Worker/Paper/Corr/Sound prompts in flight skip it so we
    // don't probe against unaccepted WIP on disk. See
    // `run_pre_step_revalidation_if_needed_pure` for the full gate
    // (C-F generalized the trigger; C-O HIGH 2 re-tightened to the
    // current Review-or-idle shape).
    run_pre_step_revalidation_if_needed(runtime)?;
    let inner_adapter = make_adapter(runtime, response)?;
    let cleanup_wrap = runtime
        .metadata()
        .repo_path
        .as_deref()
        .is_some_and(|p| p.join("Tablet").is_dir());
    // Patch C-Q Q2 — shared cell for the cleanup adapter's batch. The
    // adapter populates this before returning the WorkerResponse; the
    // post-acceptance persistence sweep below reads it and writes only
    // the refreshed entries that survived the engine's eligibility
    // filter (i.e. that are now in `state.local_closure_records`).
    let shared_cleanup_batch: std::rc::Rc<std::cell::RefCell<Option<RevalidationBatch>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let outcome = if cleanup_wrap {
        // The wrapper holds an immutable reference to a cloned snapshot of
        // `runtime.state()` for the duration of the dispatch, since the
        // mutable `step_with_checkpoint_sink` call below needs sole
        // ownership of `runtime`.
        let repo = runtime.metadata().repo_path.clone().unwrap();
        let current_cycle = runtime.state().cycle as u64;
        let state_snapshot = runtime.state().clone();
        let mut wrapper = CleanupRevalidationAdapter {
            inner: inner_adapter,
            state: &state_snapshot,
            repo,
            current_cycle,
            shared_batch: shared_cleanup_batch.clone(),
        };
        run_step_with_sink(runtime, &mut wrapper)
    } else {
        let mut adapter = inner_adapter;
        run_step_with_sink(runtime, &mut adapter)
    };
    let outcome = outcome?;
    // Patch C-Q Q2 — post-acceptance persistence sweep for the cleanup
    // adapter's batch. Disk writes are deferred until after the engine
    // accepts the cleanup response so a rejection doesn't leave orphan
    // persisted records on disk. By this point, `step_with_checkpoint_sink`
    // returned Ok, so the engine accepted; `apply_local_closure_acceptance_bookkeeping`
    // routed the batch through `apply_revalidation_batch` (filters by
    // present + proof + not-open). We only persist entries that survived
    // that filter — `state.local_closure_records.contains_key(node)`
    // means the engine kept the entry, so the on-disk copy is justified.
    if let Some(repo) = runtime.metadata().repo_path.clone() {
        if repo.join("Tablet").is_dir() {
            if let Some(batch) = shared_cleanup_batch.borrow_mut().take() {
                let runtime_root = runtime.paths().root.clone();
                let current_cycle = runtime.state().cycle as u64;
                let records_dir = local_closure_records_dir(&runtime_root);
                let state = runtime.state();
                for (node, record) in &batch.refreshed {
                    // Engine acceptance filter: only persist entries
                    // that the engine kept in
                    // `state.local_closure_records`. If
                    // `apply_revalidation_batch` dropped the entry
                    // (e.g. node opened during the cleanup delta), do
                    // NOT write a persisted file — the on-disk state
                    // would otherwise diverge from the kernel's view.
                    if let Some(accepted) = state.local_closure_records.get(node) {
                        // Persist the engine's accepted record, not
                        // the adapter's pre-acceptance candidate. They
                        // should match for the typical case, but the
                        // engine may have substituted (e.g. via
                        // backfill running concurrently). Defer to
                        // the in-memory canonical copy.
                        let _ = persist_record_to_disk(&records_dir, accepted, current_cycle);
                        let _ = record; // mute unused-binding warning
                    }
                }
            }
        }
    }
    // C-D hash backfill (plan §7.0): replace any C-B sentinel hashes the
    // engine wrote during this step with real on-disk hashes. Records
    // also persisted to disk so migration progress carries forward.
    //
    // Patch C-Q Q6: track demoted nodes so we can delete their stale
    // persisted JSON. Without this, a future rewind or state-file loss
    // could reload the sentinel from disk and clobber the
    // internal_error failure we just installed.
    if let Some(repo) = runtime.metadata().repo_path.clone() {
        if repo.join("Tablet").is_dir() {
            let runtime_root = runtime.paths().root.clone();
            let current_cycle = runtime.state().cycle as u64;
            // Capture demoted_nodes out of the migration closure so we
            // can delete their persisted JSON files after the closure
            // returns (the closure mutates `state` and must return
            // `Result<bool, String>`).
            let mut demoted_nodes: Vec<NodeId> = Vec::new();
            runtime
                .try_post_load_state_migration(|state| {
                    let result = backfill_local_closure_record_hashes(state, &repo, current_cycle);
                    demoted_nodes = result.demoted_nodes;
                    if result.mutated {
                        let records_dir = local_closure_records_dir(&runtime_root);
                        for (_, record) in state.local_closure_records.iter() {
                            let _ = persist_record_to_disk(&records_dir, record, current_cycle);
                        }
                    }
                    Ok(result.mutated)
                })
                .map_err(|err| format!("local-closure hash backfill failed: {err}"))?;
            // Q6: delete the persisted JSON for any demoted record so
            // the on-disk state matches the in-memory tombstone.
            // Uses the shared filename helper for save/delete
            // lockstep (Patch C-Q Q5).
            let records_dir = local_closure_records_dir(&runtime_root);
            for node in &demoted_nodes {
                let path =
                    records_dir.join(trellis_kernel::runtime::persisted_record_file_name(node));
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        eprintln!(
                            "[local-closure backfill] failed to remove demoted record {}: {err}",
                            path.display()
                        );
                    }
                }
            }
        }
    }
    Ok(outcome)
}

const HUMAN_GATE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const HUMAN_GATE_MISSING_RESPONSE_DETAIL: &str = "missing human gate response file";

fn should_poll_for_human_gate_response(runtime: &SupervisorRuntime, err: &str) -> bool {
    if runtime.state().stage != trellis_kernel::Stage::HumanGate {
        return false;
    }
    let Some(request) = runtime.state().in_flight_request.as_ref() else {
        return false;
    };
    request.kind == RequestKind::HumanGate && err.contains(HUMAN_GATE_MISSING_RESPONSE_DETAIL)
}

fn configured_targets_from_config(config_path: &PathBuf) -> Result<BTreeSet<TargetId>, String> {
    let text = fs::read_to_string(config_path)
        .map_err(|err| format!("failed to read config {}: {err}", config_path.display()))?;
    let raw: Value = serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse config {}: {err}", config_path.display()))?;
    let workflow = raw
        .as_object()
        .and_then(|obj| obj.get("workflow"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            format!(
                "config.workflow must be an object in {}",
                config_path.display()
            )
        })?;
    let raw_targets = workflow
        .get("main_result_targets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut targets: BTreeSet<TargetId> = BTreeSet::new();
    for item in raw_targets {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let start_line = obj.get("start_line").and_then(Value::as_i64).unwrap_or(0);
        let end_line = obj.get("end_line").and_then(Value::as_i64).unwrap_or(0);
        if start_line <= 0 || end_line <= 0 {
            continue;
        }
        let (start_line, end_line) = if start_line <= end_line {
            (start_line, end_line)
        } else {
            (end_line, start_line)
        };
        let label = obj
            .get("tex_label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        targets.insert(TargetId::from(
            label.unwrap_or_else(|| format!("lines:{start_line}-{end_line}")),
        ));
    }
    Ok(targets)
}

fn repo_path_from_config(config_path: &PathBuf) -> Result<PathBuf, String> {
    let text = fs::read_to_string(config_path)
        .map_err(|err| format!("failed to read config {}: {err}", config_path.display()))?;
    let raw: Value = serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse config {}: {err}", config_path.display()))?;
    let repo_raw = raw
        .as_object()
        .and_then(|obj| obj.get("repo_path"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "config.repo_path must be a non-empty string in {}",
                config_path.display()
            )
        })?;
    let candidate = PathBuf::from(repo_raw);
    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(candidate)
    };
    Ok(fs::canonicalize(&resolved).unwrap_or(resolved))
}

fn replay_to_event_count(
    root: PathBuf,
    stop_after_event_count: u64,
    dry_run_state_path: Option<PathBuf>,
    seed_checkpoint_path: Option<PathBuf>,
) -> Result<RuntimeCliResponse, String> {
    let paths = RuntimePaths::new(root);
    let metadata_raw = fs::read_to_string(&paths.metadata_path).map_err(|err| {
        format!(
            "failed to read metadata {}: {err}",
            paths.metadata_path.display()
        )
    })?;
    let metadata: RuntimeMetadata = serde_json::from_str(&metadata_raw)
        .map_err(|err| format!("failed to parse runtime metadata: {err}"))?;
    let repo_path = metadata
        .repo_path
        .clone()
        .ok_or_else(|| "runtime metadata has no repo_path".to_string())?;
    let log_raw = fs::read_to_string(&paths.event_log_path).map_err(|err| {
        format!(
            "failed to read event log {}: {err}",
            paths.event_log_path.display()
        )
    })?;
    let log_lines: Vec<&str> = log_raw.lines().collect();
    let total = log_lines.len() as u64;
    if stop_after_event_count > total {
        return Err(format!(
            "stop_after_event_count={} exceeds log length {}",
            stop_after_event_count, total
        ));
    }
    // Seed either from a supervisor_state.json checkpoint (skip replay up to its
    // event_count) or from config (replay from the very start).
    let (mut state, skip_count) = if let Some(ref ckpt_path) = seed_checkpoint_path {
        let ckpt_raw = fs::read_to_string(ckpt_path)
            .map_err(|err| format!("failed to read checkpoint {}: {err}", ckpt_path.display()))?;
        let ckpt_value: serde_json::Value = serde_json::from_str(&ckpt_raw)
            .map_err(|err| format!("failed to parse checkpoint JSON: {err}"))?;
        let state_value = ckpt_value
            .get("state")
            .ok_or_else(|| "checkpoint missing 'state' field".to_string())?
            .clone();
        let ckpt_state: ProtocolState = serde_json::from_value(state_value)
            .map_err(|err| format!("failed to deserialize checkpoint state: {err}"))?;
        let ckpt_count = ckpt_value
            .get("event_count")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "checkpoint missing integer 'event_count'".to_string())?;
        if ckpt_count > stop_after_event_count {
            return Err(format!(
                "checkpoint event_count={} exceeds stop_after_event_count={}",
                ckpt_count, stop_after_event_count
            ));
        }
        (ckpt_state, ckpt_count as usize)
    } else {
        let mut state = ProtocolState::default();
        let seed_metadata = RuntimeMetadata {
            repo_path: Some(repo_path.clone()),
            config_path: metadata.config_path.clone(),
            native_history_kinds: metadata.native_history_kinds.clone(),
        };
        seed_state_from_config(&mut state, &seed_metadata)?;
        state.normalize_all_structural_state();
        (state, 0usize)
    };
    let mut skipped: Vec<String> = Vec::new();
    for (idx, line) in log_lines
        .iter()
        .enumerate()
        .skip(skip_count)
        .take((stop_after_event_count as usize).saturating_sub(skip_count))
    {
        let record: trellis_kernel::EventLogRecord = serde_json::from_str(line)
            .map_err(|err| format!("failed to parse event_log line {}: {err}", idx + 1))?;
        match trellis_kernel::apply_event(state.clone(), record.event.clone()) {
            Ok(outcome) => {
                state = outcome.state;
            }
            Err(trellis_kernel::TransitionError::InvalidStage { .. })
            | Err(trellis_kernel::TransitionError::InvalidPhase { .. }) => {
                skipped.push(format!("line {}: skipped stale event", idx + 1));
            }
            Err(err) => {
                return Err(format!(
                    "apply_event failed at log line {}: {err:?}. Skipped so far: {}",
                    idx + 1,
                    skipped.len()
                ));
            }
        }
    }
    eprintln!(
        "replay_to_event_count: skipped {} stale events",
        skipped.len()
    );
    let target_state_path = dry_run_state_path
        .clone()
        .unwrap_or(paths.state_path.clone());
    let serialized = serde_json::to_string_pretty(&state)
        .map_err(|err| format!("failed to serialize replayed state: {err}"))?;
    fs::write(&target_state_path, serialized).map_err(|err| {
        format!(
            "failed to write state {}: {err}",
            target_state_path.display()
        )
    })?;
    let mut log_truncated = false;
    if dry_run_state_path.is_none() && stop_after_event_count < total {
        let kept = log_lines
            .iter()
            .take(stop_after_event_count as usize)
            .copied()
            .collect::<Vec<&str>>()
            .join("\n");
        let mut new_log = kept;
        if !new_log.is_empty() {
            new_log.push('\n');
        }
        fs::write(&paths.event_log_path, new_log).map_err(|err| {
            format!(
                "failed to truncate event log {}: {err}",
                paths.event_log_path.display()
            )
        })?;
        log_truncated = true;
    }
    let in_flight_kind = state
        .in_flight_request
        .as_ref()
        .map(|req| format!("{:?}", req.kind));
    let in_flight_id = state.in_flight_request.as_ref().map(|req| req.id);
    // Fix A: make the repo worktree consistent with the rewound state by
    // hard-resetting to the supervisor2 checkpoint tag at or before the
    // target event_count, and cleaning untracked files under Tablet/. This
    // prevents the "ghost baseline" problem where rewinding kernel state
    // alone leaves disk holding work from events that were discarded, which
    // the next worker's checker then locks in as its `before_snapshot`.
    // Skip on dry-run since we didn't truncate the log either.
    let (repo_reset_to_tag, repo_reset_error) = if dry_run_state_path.is_none() {
        match reset_repo_worktree_to_checkpoint(&repo_path, stop_after_event_count) {
            Ok(tag) => (Some(tag), None),
            Err(err) => {
                eprintln!("replay_to_event_count: repo reset failed: {err}");
                (None, Some(err))
            }
        }
    } else {
        (None, None)
    };
    Ok(RuntimeCliResponse::ReplayToEventCountOk {
        event_count_applied: stop_after_event_count,
        cycle: state.cycle,
        stage: format!("{:?}", state.stage),
        in_flight_kind,
        in_flight_id,
        state_path: target_state_path,
        log_truncated,
        repo_reset_to_tag,
        repo_reset_error,
    })
}

/// Reset the repo worktree so it matches the state we rewound to.
/// Finds the largest `supervisor2/checkpoint-NNNNNN` tag with NNNNNN <=
/// `event_count`, runs `git reset --hard` to it, then `git clean -fd Tablet/`
/// to drop any untracked helper files that later bursts may have added.
fn reset_repo_worktree_to_checkpoint(repo_path: &Path, event_count: u64) -> Result<String, String> {
    use std::process::Command;
    let repo_str = repo_path
        .to_str()
        .ok_or_else(|| "repo_path is not valid UTF-8".to_string())?;
    let tags_out = Command::new("git")
        .args(["-C", repo_str, "tag", "-l", "supervisor2/checkpoint-*"])
        .output()
        .map_err(|err| format!("git tag list failed: {err}"))?;
    if !tags_out.status.success() {
        return Err(format!(
            "git tag list exited {}: {}",
            tags_out.status,
            String::from_utf8_lossy(&tags_out.stderr)
        ));
    }
    let tags_raw = String::from_utf8_lossy(&tags_out.stdout);
    let best = tags_raw
        .lines()
        .filter_map(|line| {
            line.strip_prefix("supervisor2/checkpoint-")
                .and_then(|suffix| suffix.trim().parse::<u64>().ok())
        })
        .filter(|&n| n <= event_count)
        .max()
        .ok_or_else(|| {
            format!("no supervisor2/checkpoint-* tag at or before event_count={event_count}")
        })?;
    let tag = format!("supervisor2/checkpoint-{best:06}");
    let reset_out = Command::new("git")
        .args(["-C", repo_str, "reset", "--hard", &tag])
        .output()
        .map_err(|err| format!("git reset --hard {tag} failed: {err}"))?;
    if !reset_out.status.success() {
        return Err(format!(
            "git reset --hard {tag} exited {}: {}",
            reset_out.status,
            String::from_utf8_lossy(&reset_out.stderr)
        ));
    }
    // Clean untracked files under Tablet/ only. Other dirs (.lake, .trellis,
    // paper/, etc.) are preserved — we only want to drop worker-authored
    // node files that weren't part of the checkpoint.
    let clean_out = Command::new("git")
        .args(["-C", repo_str, "clean", "-fd", "Tablet/"])
        .output()
        .map_err(|err| format!("git clean -fd Tablet/ failed: {err}"))?;
    if !clean_out.status.success() {
        return Err(format!(
            "git clean -fd Tablet/ exited {}: {}",
            clean_out.status,
            String::from_utf8_lossy(&clean_out.stderr)
        ));
    }
    eprintln!(
        "replay_to_event_count: reset repo worktree to {tag} (for event_count={event_count})"
    );
    Ok(tag)
}

fn ensure_initial_preamble(repo_path: &Path) -> Result<(), String> {
    let tablet_dir = repo_path.join("Tablet");
    fs::create_dir_all(&tablet_dir).map_err(|err| {
        format!(
            "failed to create Tablet dir {}: {err}",
            tablet_dir.display()
        )
    })?;
    let preamble_lean = tablet_dir.join("Preamble.lean");
    if !preamble_lean.exists() {
        fs::write(&preamble_lean, "")
            .map_err(|err| format!("failed to seed {}: {err}", preamble_lean.display()))?;
    }
    let preamble_tex = tablet_dir.join("Preamble.tex");
    if !preamble_tex.exists() {
        fs::write(&preamble_tex, "")
            .map_err(|err| format!("failed to seed {}: {err}", preamble_tex.display()))?;
    }
    Ok(())
}

fn seed_state_from_config(
    state: &mut ProtocolState,
    metadata: &RuntimeMetadata,
) -> Result<(), String> {
    if let Some(repo_path) = metadata.repo_path.as_ref() {
        if state.live.present_nodes.is_empty() && state.committed.present_nodes.is_empty() {
            ensure_initial_preamble(repo_path)?;
            let preamble = NodeId::from("Preamble");
            state.live.present_nodes.insert(preamble.clone());
            state.committed.present_nodes.insert(preamble.clone());
            state
                .node_kinds
                .insert(preamble.clone(), NodeKind::Preamble);
            state
                .committed_node_kinds
                .insert(preamble.clone(), NodeKind::Preamble);
            state.deps.insert(preamble.clone(), BTreeSet::new());
            state
                .committed_deps
                .insert(preamble.clone(), BTreeSet::new());
            state
                .target_claims
                .insert(preamble.clone(), BTreeSet::new());
            state
                .committed_target_claims
                .insert(preamble.clone(), BTreeSet::new());
            let preamble_tex = fs::read_to_string(repo_path.join("Tablet").join("Preamble.tex"))
                .unwrap_or_default();
            if extract_tex_statement_items(&preamble_tex, true).is_empty() {
                state.corr_status.insert(preamble.clone(), CorrStatus::Pass);
                state
                    .corr_approved_fingerprints
                    .insert(preamble.clone(), String::new());
                state
                    .live
                    .corr_current_fingerprints
                    .insert(preamble.clone(), String::new());
                state
                    .committed
                    .corr_current_fingerprints
                    .insert(preamble.clone(), String::new());
                state
                    .live
                    .target_fingerprints
                    .insert(preamble.clone(), String::new());
                state
                    .committed
                    .target_fingerprints
                    .insert(preamble, String::new());
            }
        }
    }

    // Verifier-lane count: derive from the operator's config + policy so that
    // single-agent-per-panel setups produce single-lane verification (one API
    // call per check) instead of crashing with "not enough configured X agents
    // for requested lanes". Existing 2-agent setups still get 2 lanes because
    // both `*_agents` and `*_agent_selectors` lists have len() == 2, which
    // `resolve_verifier_lane_count` reports as 2. Backwards compat: when the
    // config_path is missing or unreadable we keep the protocol default
    // (`default_verifier_lanes`) — matches pre-K-2 behavior. K-2 fix.
    if let Some(config_path) = metadata.config_path.as_ref() {
        if config_path.exists() {
            let lane_count =
                trellis_kernel::resolve_verifier_lane_count(config_path).map_err(|err| {
                    format!("failed to resolve verifier lane count from config: {err}")
                })?;
            state.verifier_lanes = trellis_kernel::build_verifier_lanes(lane_count);
        }
    }

    if !state.configured_targets.is_empty() {
        return Ok(());
    }
    let Some(config_path) = metadata.config_path.as_ref() else {
        return Ok(());
    };
    if !config_path.exists() {
        return Ok(());
    }
    let configured_targets = configured_targets_from_config(config_path)?;
    if configured_targets.is_empty() {
        return Ok(());
    }
    let empty_coverage: BTreeMap<TargetId, BTreeSet<NodeId>> = configured_targets
        .iter()
        .cloned()
        .map(|target| (target, BTreeSet::new()))
        .collect();
    state.configured_targets = configured_targets.clone();
    state.live.coverage = empty_coverage.clone();
    state.committed.coverage = empty_coverage.clone();
    Ok(())
}

fn main() -> std::process::ExitCode {
    let request = match read_request() {
        Ok(request) => request,
        Err(message) => {
            let _ = serde_json::to_writer_pretty(
                io::stdout(),
                &RuntimeCliResponse::InvalidRequest {
                    message: message.clone(),
                },
            );
            println!();
            eprintln!("{message}");
            return std::process::ExitCode::from(2);
        }
    };

    let response = match request {
        RuntimeCliRequest::Init {
            root,
            mut state,
            metadata,
        } => {
            let metadata = metadata.unwrap_or_default();
            let runtime = seed_state_from_config(&mut state, &metadata).and_then(|_| {
                SupervisorRuntime::initialize_with_metadata(
                    RuntimePaths::new(root),
                    state,
                    metadata,
                )
                .map_err(|err| format!("runtime init failed: {err}"))
            });
            runtime.and_then(|runtime| success_response(&runtime, None, 0, None))
        }
        RuntimeCliRequest::InitFromConfig { root, config_path } => {
            repo_path_from_config(&config_path).and_then(|repo_path| {
                let metadata = RuntimeMetadata {
                    repo_path: Some(repo_path.clone()),
                    config_path: Some(config_path),
                    native_history_kinds: Default::default(),
                };
                let mut state = ProtocolState::default();
                seed_state_from_config(&mut state, &metadata)?;
                trellis_kernel::sync_tablet_support_from_repo(&repo_path)?;
                SupervisorRuntime::initialize_with_metadata(
                    RuntimePaths::new(root),
                    state,
                    metadata,
                )
                .map_err(|err| format!("runtime init failed: {err}"))
                .and_then(|runtime| success_response(&runtime, None, 0, None))
            })
        }
        RuntimeCliRequest::ImportLegacy {
            root,
            config_path,
            state_path,
            tablet_path,
        } => {
            let imported = trellis_kernel::import_legacy_project(
                &config_path,
                state_path.as_deref(),
                tablet_path.as_deref(),
            );
            imported.and_then(|imported| {
                let runtime = SupervisorRuntime::initialize_with_metadata(
                    RuntimePaths::new(root),
                    imported.state,
                    RuntimeMetadata {
                        repo_path: Some(imported.repo_path),
                        config_path: Some(config_path),
                        native_history_kinds: Default::default(),
                    },
                )
                .map_err(|err| format!("runtime init failed: {err}"))?;
                success_response(&runtime, None, 0, Some(imported.summary))
            })
        }
        RuntimeCliRequest::ResolveMainResultTargets {
            paper_path,
            raw_targets,
            raw_labels,
        } => resolve_main_result_targets(
            paper_path.as_deref(),
            raw_targets.as_ref(),
            raw_labels.as_ref(),
        )
        .map(|output| RuntimeCliResponse::ResolveMainResultTargetsOk { output }),
        RuntimeCliRequest::BridgeRequestPayload { repo_path, request } => {
            hydrated_bridge_request_payload(&repo_path, &request)
                .map(|payload| RuntimeCliResponse::BridgeRequestPayloadOk { payload })
        }
        RuntimeCliRequest::ReplayToEventCount {
            root,
            stop_after_event_count,
            dry_run_state_path,
            seed_checkpoint_path,
        } => replay_to_event_count(
            root,
            stop_after_event_count,
            dry_run_state_path,
            seed_checkpoint_path,
        ),
        RuntimeCliRequest::Show { root } => {
            let runtime = SupervisorRuntime::load(RuntimePaths::new(root))
                .map_err(|err| format!("runtime load failed: {err}"));
            runtime.and_then(|runtime| success_response(&runtime, None, 0, None))
        }
        RuntimeCliRequest::CurrentRequest { root } => {
            let runtime = SupervisorRuntime::load(RuntimePaths::new(root))
                .map_err(|err| format!("runtime load failed: {err}"));
            runtime.and_then(|runtime| {
                let request = runtime
                    .state()
                    .in_flight_request
                    .as_ref()
                    .ok_or_else(|| "runtime has no in-flight request".to_string())?;
                Ok(RuntimeCliResponse::CurrentRequestOk {
                    request: bridge_request_payload(
                        request,
                        runtime.metadata().config_path.as_deref(),
                        runtime.metadata().repo_path.as_deref(),
                    )?,
                    metadata: runtime.metadata().clone(),
                })
            })
        }
        RuntimeCliRequest::NormalizeWorker { input } => {
            trellis_kernel::normalize_worker_response(&input)
                .map(|output| RuntimeCliResponse::NormalizeWorkerOk { output })
        }
        RuntimeCliRequest::ValidateTrellisWorkerResult {
            raw_payload,
            acceptance_context,
        } => {
            if let Some(acceptance_context) = acceptance_context {
                worker_allowed_outcomes_for_validation(&acceptance_context).map(
                    |allowed_outcomes| RuntimeCliResponse::ValidateTrellisWorkerResultOk {
                        output: validate_trellis_worker_result_data_with_allowed_outcomes(
                            &raw_payload,
                            &allowed_outcomes,
                        ),
                    },
                )
            } else {
                Ok(RuntimeCliResponse::ValidateTrellisWorkerResultOk {
                    output: validate_trellis_worker_result_data(&raw_payload),
                })
            }
        }
        RuntimeCliRequest::ValidateTrellisReviewerResult { raw_payload } => {
            Ok(RuntimeCliResponse::ValidateTrellisReviewerResultOk {
                output: validate_trellis_reviewer_result_data(&raw_payload),
            })
        }
        // Cleanup-v2 (audit Finding 1): shape-validate an audit-burst
        // artifact. Domain legality (target ∈ present, replacement
        // validity, etc.) is enforced by `apply_audit_response` against
        // the live ProtocolState — this branch only checks shape.
        RuntimeCliRequest::ValidateTrellisAuditResult { raw_payload } => {
            Ok(RuntimeCliResponse::ValidateTrellisAuditResultOk {
                output: validate_trellis_audit_result_data(&raw_payload),
            })
        }
        RuntimeCliRequest::ValidateTrellisStuckMathAuditResult { raw_payload } => {
            Ok(RuntimeCliResponse::ValidateTrellisStuckMathAuditResultOk {
                output: validate_trellis_stuck_math_audit_result_data(&raw_payload),
            })
        }
        RuntimeCliRequest::BuildMalformedResponse {
            kind,
            request_id,
            cycle,
        } => build_malformed_response_output(kind, request_id, cycle)
            .map(|output| RuntimeCliResponse::BuildMalformedResponseOk { output }),
        RuntimeCliRequest::ValidatePaperFaithfulnessResult { raw_payload } => {
            Ok(RuntimeCliResponse::ValidatePaperFaithfulnessResultOk {
                output: validate_paper_faithfulness_result_data(&raw_payload),
            })
        }
        RuntimeCliRequest::ValidateDeviationAuthorizationResult { raw_payload } => {
            Ok(RuntimeCliResponse::ValidateDeviationAuthorizationResultOk {
                output: validate_deviation_authorization_result_data(&raw_payload),
            })
        }
        RuntimeCliRequest::ValidateSubstantivenessResult { raw_payload } => {
            Ok(RuntimeCliResponse::ValidateSubstantivenessResultOk {
                output: validate_substantiveness_result_data(&raw_payload),
            })
        }
        RuntimeCliRequest::ValidateCorrespondenceResult { raw_payload } => {
            Ok(RuntimeCliResponse::ValidateCorrespondenceResultOk {
                output: validate_correspondence_result_data(&raw_payload),
            })
        }
        RuntimeCliRequest::ValidateSoundnessResult {
            raw_payload,
            node_name,
        } => Ok(RuntimeCliResponse::ValidateSoundnessResultOk {
            output: validate_soundness_result_data(&raw_payload, &node_name),
        }),
        RuntimeCliRequest::CheckTrellisWorkerResult {
            repo_path,
            acceptance_context,
            raw_payload,
        } => check_trellis_worker_result_output(&repo_path, acceptance_context, raw_payload)
            .map(|output| RuntimeCliResponse::CheckTrellisWorkerResultOk { output }),
        RuntimeCliRequest::HydrateWorkerResponse { input } => {
            hydrate_worker_response_output(&input)
                .map(|output| RuntimeCliResponse::HydrateWorkerResponseOk { output })
        }
        RuntimeCliRequest::CheckTrellisReviewerResult {
            review_request,
            raw_payload,
        } => check_trellis_reviewer_result_output(review_request, raw_payload)
            .map(|output| RuntimeCliResponse::CheckTrellisReviewerResultOk { output }),
        // Cleanup-v2 (audit Finding 1): one-shot validate+normalize for
        // the audit-burst artifact, mirroring the reviewer path.
        RuntimeCliRequest::CheckTrellisAuditResult {
            audit_request,
            raw_payload,
        } => check_trellis_audit_result_output(audit_request, raw_payload)
            .map(|output| RuntimeCliResponse::CheckTrellisAuditResultOk { output }),
        RuntimeCliRequest::CheckTrellisStuckMathAuditResult {
            audit_request,
            raw_payload,
        } => check_trellis_stuck_math_audit_result_output(audit_request, raw_payload)
            .map(|output| RuntimeCliResponse::CheckTrellisStuckMathAuditResultOk { output }),
        RuntimeCliRequest::CheckNode {
            repo_path,
            node_name,
            expected_hash,
        } => check_node_output(&repo_path, &node_name, expected_hash.as_deref())
            .map(|output| RuntimeCliResponse::CheckNodeOk { output }),
        RuntimeCliRequest::CheckTablet { repo_path } => check_tablet_output(&repo_path)
            .map(|output| RuntimeCliResponse::CheckTabletOk { output }),
        RuntimeCliRequest::SyncTabletSupport { repo_path } => {
            trellis_kernel::sync_tablet_support_from_repo(&repo_path)
                .map(|output| RuntimeCliResponse::SyncTabletSupportOk { output })
        }
        RuntimeCliRequest::ObserveSoundnessFingerprints { repo_path, nodes } => {
            ensure_worker_checker_support_available(&repo_path, &nodes).and_then(|_| {
                observe_soundness_fingerprints(&repo_path, &nodes)
                    .map(|output| RuntimeCliResponse::ObserveSoundnessFingerprintsOk { output })
            })
        }
        RuntimeCliRequest::CheckTabletScoped {
            repo_path,
            baseline_errors,
            allowed_nodes,
        } => check_tablet_scoped_output(&repo_path, &baseline_errors, &allowed_nodes)
            .map(|output| RuntimeCliResponse::CheckTabletScopedOk { output }),
        RuntimeCliRequest::PrepareWorkerGate {
            repo_path,
            request,
            collect_observations,
            paper_source_path,
        } => prepare_worker_gate_output(
            &repo_path,
            &request,
            collect_observations.unwrap_or(true),
            paper_source_path.as_deref(),
        )
        .map(|output| RuntimeCliResponse::PrepareWorkerGateOk { output }),
        RuntimeCliRequest::ExecuteWorkerValidationPlan { input } => {
            execute_worker_validation_plan(&input)
                .map(|output| RuntimeCliResponse::ExecuteWorkerValidationPlanOk { output })
        }
        RuntimeCliRequest::NormalizeCorr { input } => normalize_corr_response(&input)
            .map(|output| RuntimeCliResponse::NormalizeCorrOk { output }),
        RuntimeCliRequest::NormalizePaper { input } => normalize_paper_response(&input)
            .map(|output| RuntimeCliResponse::NormalizePaperOk { output }),
        RuntimeCliRequest::NormalizeSound { input } => normalize_sound_response(&input)
            .map(|output| RuntimeCliResponse::NormalizeSoundOk { output }),
        RuntimeCliRequest::NormalizeReview { input } => normalize_review_response(&input)
            .map(|output| RuntimeCliResponse::NormalizeReviewOk { output }),
        RuntimeCliRequest::NormalizeHumanGate {
            request_id,
            cycle,
            raw_payload_text,
        } => Ok(RuntimeCliResponse::NormalizeHumanGateOk {
            output: normalize_human_gate_output(request_id, cycle, &raw_payload_text),
        }),
        RuntimeCliRequest::WorkerBlockerStatusBlock { request } => {
            Ok(RuntimeCliResponse::WorkerBlockerStatusBlockOk {
                output: trellis_kernel::worker_blocker_status_block(&request),
            })
        }
        RuntimeCliRequest::ReviewBlockerChoicesBlock { request } => {
            Ok(RuntimeCliResponse::ReviewBlockerChoicesBlockOk {
                output: trellis_kernel::review_blocker_choices_block(&request),
            })
        }
        RuntimeCliRequest::AcceptWorker { input } => accept_worker_response(&input)
            .map(|output| RuntimeCliResponse::AcceptWorkerOk { output }),
        RuntimeCliRequest::Step { root, response } => {
            let runtime = load_runtime_with_fingerprint_validation(RuntimePaths::new(root));
            runtime.and_then(|mut runtime| {
                run_local_closure_migration_if_configured(&mut runtime)?;
                let outcome = step_runtime(&mut runtime, response)?;
                success_response(&runtime, Some(outcome), 1, None)
            })
        }
        RuntimeCliRequest::RestoreActiveWorkerBase { root } => {
            SupervisorRuntime::load(RuntimePaths::new(root))
                .map_err(|err| format!("runtime load failed: {err}"))
                .and_then(|runtime| {
                    runtime
                        .restore_active_worker_base_for_inflight()
                        .map_err(|err| format!("restore active_worker_base failed: {err}"))
                        .map(|restored| RuntimeCliResponse::RestoreActiveWorkerBaseOk { restored })
                })
        }
        RuntimeCliRequest::AckHaltMarker {
            root,
            reason,
            force,
            probe_result,
        } => {
            // Audit M-3 — operator-driven controlled clear path for the
            // checker-disagreement halt marker. The kernel routes
            // through `TRELLIS_KERNEL_CACHE_ROOT` to resolve the marker
            // path; export it from the supplied `root` so this command
            // works without requiring the caller to pre-export the env
            // var. Use unsafe set_var per the same single-threaded
            // rationale as the `Run` command above.
            unsafe {
                std::env::set_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, &root);
            }
            trellis_kernel::runtime_cli_observations_halt::acknowledge_checker_disagreement_halt_marker(
                &reason,
                force,
                probe_result.as_ref(),
            )
            .map(|outcome| RuntimeCliResponse::AckHaltMarkerOk { outcome })
        }
        RuntimeCliRequest::Run { root, max_steps } => {
            let _kernel_cache_env_guard = kernel_cache_env_test_guard();
            // Export the kernel cache root so every subprocess this
            // supervisor spawns (bridge → Python wrapper → child kernel
            // CLI) inherits it via env. Disk-persistent cache files
            // live at `<root>/checker-state/kernel-cache/<namespace>/`;
            // see `trellis_kernel::disk_cache` for the file layout.
            //
            // Setting an env var on this process is unsafe in edition
            // 2024+ (concurrent reads from other threads can race) but
            // we're single-threaded here and the env var is read-only
            // from this point onward.
            //
            // Trust-boundary defense: explicitly UNSET the readonly
            // fallback var, so an operator shell that happens to export
            // it (typo, debug leftover) can't push a worker-writable
            // path into the supervisor's lookup chain. The two-cache
            // split's invariant — supervisor never reads from anywhere
            // a worker can write — relies on this var staying unset for
            // the supervisor process; only `sandbox.py:wrap_command`
            // sets it, and only inside a worker bwrap (not on
            // supervisor-side spawns).
            unsafe {
                std::env::set_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, &root);
                std::env::remove_var(trellis_kernel::disk_cache::KERNEL_CACHE_READONLY_ROOT_ENV);
            }
            // Fail-fast startup invariant: `TRELLIS_CHECKER_SOCKET` must be
            // set AND point to an existing path. Without it, every
            // `local-closure-axioms` request silently fails with an
            // `internal_error` status whose error text the wire format
            // doesn't surface — the worker sees only
            // `local-closure probe status=internal_error:` and is forced
            // to downgrade valid artifacts. Detect at supervisor startup
            // so the operator knows to start the checker server / re-export
            // the env var BEFORE any worker burst runs.
            let env_check: Result<(), String> = match std::env::var("TRELLIS_CHECKER_SOCKET") {
                Ok(path) if !path.trim().is_empty() => {
                    let socket_path = std::path::Path::new(path.trim());
                    if socket_path.exists() {
                        Ok(())
                    } else {
                        Err(format!(
                            "TRELLIS_CHECKER_SOCKET points to a non-existent \
                             path: {path}. Start the checker server (e.g. \
                             scripts/trellis_checker_server.sh) before \
                             launching the supervisor, or correct the env var."
                        ))
                    }
                }
                _ => Err("TRELLIS_CHECKER_SOCKET is unset (or empty). The \
                     supervisor requires it to route local-closure-axioms \
                     requests to the checker server. Set it before launch: \
                     TRELLIS_CHECKER_SOCKET=<runtime>/sockets/checker.sock"
                    .to_string()),
            };
            env_check.and_then(|_| {
            let runtime = load_runtime_with_fingerprint_validation(RuntimePaths::new(root));
            runtime.and_then(|mut runtime| {
                run_local_closure_migration_if_configured(&mut runtime)?;
                let mut steps_executed = 0;
                let limit = max_steps.unwrap_or(u32::MAX);
                let mut last_outcome: Option<RuntimeStepOutcome> = None;
                while steps_executed < limit
                    && runtime.state().stage != trellis_kernel::Stage::Complete
                {
                    // Graceful-stop sentinel. Checked at the top of each
                    // loop iteration so it fires for both successful-step
                    // and human-gate-poll iterations (the latter `continue`
                    // past the rest of the body). State is guaranteed
                    // consistent at this point: the previous iteration's
                    // step, if any, persisted fully before returning; on
                    // the first iteration the runtime was just loaded
                    // from disk. Atomicity invariant from d060508 holds —
                    // we never observe the sentinel between sink-commit
                    // and state-rollback.
                    if let Some(repo_path) = runtime.metadata().repo_path.as_deref() {
                        let stop_file = repo_path.join(".trellis-stop-after-checkpoint");
                        if stop_file.exists() {
                            let _ = std::fs::remove_file(&stop_file);
                            eprintln!(
                                "trellis: stop-after-checkpoint sentinel detected; halting cleanly after {steps_executed} step(s)."
                            );
                            break;
                        }
                    }
                    let checker_halt_marker = runtime.paths().root.join(
                        trellis_kernel::runtime_cli_observations_halt::CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME,
                    );
                    if checker_halt_marker.exists() {
                        eprintln!(
                            "==============================================================\n\
                             trellis: HALTED — checker-disagreement marker present at\n\
                               {}\n\
                             Inspect the JSON for diagnostics + clear instructions.\n\
                             No new bursts will be dispatched until the marker is\n\
                             removed (operator-only).\n\
                             ==============================================================",
                            checker_halt_marker.display()
                        );
                        break;
                    }
                    // Per fail-loudly policy: every system_feedback emission pauses the run.
                    let system_feedback_halt_marker = runtime.paths().root.join(
                        trellis_kernel::runtime_cli_observations_halt::SYSTEM_FEEDBACK_HALT_MARKER_FILENAME,
                    );
                    if system_feedback_halt_marker.exists() {
                        eprintln!(
                            "==============================================================\n\
                             trellis: HALTED — system_feedback marker present at\n\
                               {}\n\
                             An agent burst returned a non-empty system_feedback string.\n\
                             Inspect the JSON for diagnostics + clear instructions.\n\
                             No new bursts will be dispatched until the marker is\n\
                             removed (operator-only).\n\
                             ==============================================================",
                            system_feedback_halt_marker.display()
                        );
                        break;
                    }
                    let outcome = match step_runtime(&mut runtime, None) {
                        Ok(outcome) => outcome,
                        Err(message) if should_poll_for_human_gate_response(&runtime, &message) => {
                            std::thread::sleep(HUMAN_GATE_POLL_INTERVAL);
                            continue;
                        }
                        Err(message) => return Err(message),
                    };
                    steps_executed += 1;
                    let done =
                        matches!(outcome.status, trellis_kernel::RuntimeStepStatus::Complete);
                    last_outcome = Some(outcome);
                    if done {
                        break;
                    }
                }
                success_response(&runtime, last_outcome, steps_executed, None)
            })
            })
        }
    };

    match response {
        Ok(response) => {
            let _ = serde_json::to_writer_pretty(io::stdout(), &response);
            println!();
            std::process::ExitCode::SUCCESS
        }
        Err(message) => {
            let _ = serde_json::to_writer_pretty(
                io::stdout(),
                &RuntimeCliResponse::Error {
                    message: message.clone(),
                },
            );
            println!();
            eprintln!("{message}");
            std::process::ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bridge_request_payload, check_trellis_worker_result_output,
        node_deviation_claims_after_updates, populate_response_fingerprints,
        prepare_worker_gate_output, proof_protected_package_legality_error,
        should_poll_for_human_gate_response, CheckedWorkerPayload, PreparedWorkerGateOutput,
        RuntimePaths, SupervisorRuntime,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use tempfile::tempdir;
    use trellis_kernel::{
        DeviationId, DeviationRequest, GateKind, NodeId, Phase, ProtocolState, RequestKind,
        ResetChoice, RetryOutcomeKind, ReviewDecisionKind, Stage, TargetId, TaskMode,
        WorkerOutcome, WorkerResponse, WorkerValidationKind, WorkingSnapshot,
    };

    #[test]
    fn human_gate_poll_helper_only_waits_for_missing_human_input() {
        let dir = tempdir().expect("tempdir");
        let mut state = ProtocolState::default();
        state.phase = Phase::TheoremStating;
        state.stage = Stage::HumanGate;
        state.cycle = 3;
        state.request_seq = 1;
        state.in_flight_request = Some(state.expected_request(1, RequestKind::HumanGate));
        let runtime = SupervisorRuntime::initialize(RuntimePaths::new(dir.path()), state)
            .expect("initialize runtime");

        assert!(should_poll_for_human_gate_response(
            &runtime,
            "runtime step failed: adapter error: bridge /tmp/bridge failed: missing human gate response file: /tmp/runtime/human_gate_response.json"
        ));
        assert!(!should_poll_for_human_gate_response(
            &runtime,
            "runtime step failed: adapter error: bridge /tmp/bridge failed: failed to read human gate response file: permission denied"
        ));
    }

    // (deleted: proof_protected_package_check_rejects_protected_fingerprint_drift
    //  exercised the per-node post-hoc honesty loop that iterated
    //  `protected_snapshot.keys()`. That loop is gone; drift-on-covering-nodes
    //  is now caught by `paper_target_corr_reopen_guard_errors` at commit time.
    //  The test's assertion — that a "protected target fingerprint" mismatch
    //  yields a rejection at this legality-error helper — no longer applies.)

    #[test]
    fn proof_local_allows_new_lean_irrelevant_helper_definition() {
        // Adding a new helper that is NOT in any covering node's L_def
        // (Lean-relevance set) does not appear in the paper fingerprint's
        // `lean_relevant_definition_descendants` axis, so the post-worker
        // fingerprint byte-equals the pre-worker fingerprint and the
        // protection check passes.
        let acceptance_context = serde_json::json!({
            "request": {},
            "validation_kind": "proof_local",
            "worker_acceptance": {},
            "active_node": "ThmConn",
            "held_target": "",
            "authorized_nodes": [],
            "configured_targets": ["thm:conn"],
            "current_present_nodes": ["Preamble", "ThmConn", "ExistingDef"],
            "current_proof_nodes": ["ThmConn"],
            "current_deps": {
                "Preamble": [],
                "ThmConn": ["Preamble", "ExistingDef"],
                "ExistingDef": ["Preamble"]
            },
            "current_target_claims": {"ThmConn": ["thm:conn"], "ExistingDef": []},
            "current_paper_approved_fingerprints": {"thm:conn": "paper-approved"},
            "current_coverage": {"thm:conn": ["ThmConn"]},
            "current_paper_current_fingerprints": {
                "thm:conn": serde_json::json!({
                    "target": "thm:conn",
                    "covering_nodes": {"ThmConn": "protected-fp"},
                    "preamble_definition_hashes": []
                }).to_string()
            },
            "repo_path": "/tmp/repo",
            "before_snapshot": {},
            "baseline_errors": [],
            "imports_before": [],
            "expected_active_hash": "",
            "baseline_declaration_hashes": {},
            "baseline_correspondence_hashes": {}
        });
        let acceptance_context: PreparedWorkerGateOutput =
            serde_json::from_value(acceptance_context).expect("parse acceptance context");
        let response = WorkerResponse {
            outcome: WorkerOutcome::Valid,
            snapshot: WorkingSnapshot {
                present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    NodeId::from("ThmConn"),
                    NodeId::from("ExistingDef"),
                    NodeId::from("NewHelperDef"),
                ]),
                open_nodes: BTreeSet::new(),
                coverage: BTreeMap::from([(
                    TargetId::from("thm:conn"),
                    BTreeSet::from([NodeId::from("ThmConn")]),
                )]),
                target_fingerprints: BTreeMap::from([
                    (NodeId::from("ThmConn"), "protected-fp".to_string()),
                    (
                        NodeId::from("ExistingDef"),
                        "existing-def-target".to_string(),
                    ),
                    (
                        NodeId::from("NewHelperDef"),
                        "helper-def-target".to_string(),
                    ),
                ]),
                corr_current_fingerprints: BTreeMap::from([
                    (NodeId::from("ThmConn"), "protected-fp".to_string()),
                    (
                        NodeId::from("ExistingDef"),
                        "existing-def-target".to_string(),
                    ),
                    (
                        NodeId::from("NewHelperDef"),
                        "helper-def-target".to_string(),
                    ),
                ]),
                paper_current_fingerprints: BTreeMap::from([(
                    TargetId::from("thm:conn"),
                    serde_json::json!({
                        "target": "thm:conn",
                        "covering_nodes": {"ThmConn": "protected-fp"},
                        "preamble_definition_hashes": []
                    })
                    .to_string(),
                )]),
                sound_current_fingerprints: BTreeMap::new(),
                deviation_current_fingerprints: BTreeMap::new(),
                sound_current_fingerprint_parts: BTreeMap::new(),
                sketch_proof_nodes: BTreeSet::new(),
                substantiveness_current_fingerprints: BTreeMap::new(),
                protected_closure_nodes_per_target: BTreeMap::new(),
            },
            ..WorkerResponse::default()
        };

        let err =
            proof_protected_package_legality_error("proof_local", &acceptance_context, &response);

        assert_eq!(err, None);
    }

    #[test]
    fn proof_restructure_allows_non_lean_relevant_def_tex_edit() {
        // A TeX-only edit to a definition descendant whose Lean meaning
        // is not consumed by any covering node's `lean_semantic_closure`
        // walk does not appear in the paper fingerprint's
        // `lean_relevant_definition_descendants` axis, so the post-worker
        // fingerprint byte-equals the pre-worker fingerprint and the
        // protection check passes.
        let acceptance_context = serde_json::json!({
            "request": {},
            "validation_kind": "proof_restructure",
            "worker_acceptance": {},
            "active_node": "ThmConn",
            "held_target": "",
            "authorized_nodes": [],
            "configured_targets": ["thm:conn"],
            "current_present_nodes": ["Preamble", "ThmConn", "PostStateDef"],
            "current_proof_nodes": ["ThmConn"],
            "current_deps": {
                "Preamble": [],
                "ThmConn": ["Preamble", "PostStateDef"],
                "PostStateDef": ["Preamble"]
            },
            "current_target_claims": {"ThmConn": ["thm:conn"], "PostStateDef": []},
            "current_paper_approved_fingerprints": {"thm:conn": "paper-approved"},
            "current_coverage": {"thm:conn": ["ThmConn"]},
            "current_paper_current_fingerprints": {
                "thm:conn": serde_json::json!({
                    "target": "thm:conn",
                    "covering_nodes": {"ThmConn": "protected-fp"},
                    "preamble_definition_hashes": []
                }).to_string()
            },
            "repo_path": "/tmp/repo",
            "before_snapshot": {},
            "baseline_errors": [],
            "imports_before": [],
            "expected_active_hash": "",
            "baseline_declaration_hashes": {},
            "baseline_correspondence_hashes": {}
        });
        let acceptance_context: PreparedWorkerGateOutput =
            serde_json::from_value(acceptance_context).expect("parse acceptance context");
        let response = WorkerResponse {
            outcome: WorkerOutcome::Valid,
            snapshot: WorkingSnapshot {
                present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    NodeId::from("ThmConn"),
                    NodeId::from("PostStateDef"),
                ]),
                open_nodes: BTreeSet::new(),
                coverage: BTreeMap::from([(
                    TargetId::from("thm:conn"),
                    BTreeSet::from([NodeId::from("ThmConn")]),
                )]),
                target_fingerprints: BTreeMap::new(),
                corr_current_fingerprints: BTreeMap::new(),
                paper_current_fingerprints: BTreeMap::from([(
                    TargetId::from("thm:conn"),
                    serde_json::json!({
                        "target": "thm:conn",
                        "covering_nodes": {"ThmConn": "protected-fp"},
                        "preamble_definition_hashes": []
                    })
                    .to_string(),
                )]),
                sound_current_fingerprints: BTreeMap::new(),
                deviation_current_fingerprints: BTreeMap::new(),
                sound_current_fingerprint_parts: BTreeMap::new(),
                sketch_proof_nodes: BTreeSet::new(),
                substantiveness_current_fingerprints: BTreeMap::new(),
                protected_closure_nodes_per_target: BTreeMap::new(),
            },
            ..WorkerResponse::default()
        };

        let err = proof_protected_package_legality_error(
            "proof_restructure",
            &acceptance_context,
            &response,
        );

        assert_eq!(
            err, None,
            "non-Lean-relevant def TeX edits must pass under restructure"
        );
    }

    #[test]
    fn proof_restructure_rejects_lean_relevant_descendant_change() {
        // A change to a Lean-relevant descendant's TeX shows up in the
        // paper fingerprint's `lean_relevant_definition_descendants` axis,
        // so the post-worker fingerprint byte-differs from the pre-worker
        // fingerprint and the protection check fires.
        let acceptance_context = serde_json::json!({
            "request": {},
            "validation_kind": "proof_restructure",
            "worker_acceptance": {},
            "active_node": "ThmConn",
            "held_target": "",
            "authorized_nodes": [],
            "configured_targets": ["thm:conn"],
            "current_present_nodes": ["Preamble", "ThmConn", "RelevantDef"],
            "current_proof_nodes": ["ThmConn"],
            "current_deps": {
                "Preamble": [],
                "ThmConn": ["Preamble", "RelevantDef"],
                "RelevantDef": ["Preamble"]
            },
            "current_target_claims": {"ThmConn": ["thm:conn"], "RelevantDef": []},
            "current_paper_approved_fingerprints": {"thm:conn": "paper-approved"},
            "current_coverage": {"thm:conn": ["ThmConn"]},
            "current_paper_current_fingerprints": {
                "thm:conn": serde_json::json!({
                    "target": "thm:conn",
                    "covering_nodes": {"ThmConn": "protected-fp"},
                    "lean_relevant_definition_descendants": {"RelevantDef": "relevant-fp-old"},
                    "preamble_definition_hashes": []
                }).to_string()
            },
            "repo_path": "/tmp/repo",
            "before_snapshot": {},
            "baseline_errors": [],
            "imports_before": [],
            "expected_active_hash": "",
            "baseline_declaration_hashes": {},
            "baseline_correspondence_hashes": {}
        });
        let acceptance_context: PreparedWorkerGateOutput =
            serde_json::from_value(acceptance_context).expect("parse acceptance context");
        let response = WorkerResponse {
            outcome: WorkerOutcome::Valid,
            snapshot: WorkingSnapshot {
                present_nodes: BTreeSet::from([
                    NodeId::from("Preamble"),
                    NodeId::from("ThmConn"),
                    NodeId::from("RelevantDef"),
                ]),
                open_nodes: BTreeSet::new(),
                coverage: BTreeMap::from([(
                    TargetId::from("thm:conn"),
                    BTreeSet::from([NodeId::from("ThmConn")]),
                )]),
                target_fingerprints: BTreeMap::new(),
                corr_current_fingerprints: BTreeMap::new(),
                paper_current_fingerprints: BTreeMap::from([(
                    TargetId::from("thm:conn"),
                    serde_json::json!({
                        "target": "thm:conn",
                        "covering_nodes": {"ThmConn": "protected-fp"},
                        "lean_relevant_definition_descendants": {"RelevantDef": "relevant-fp-NEW"},
                        "preamble_definition_hashes": []
                    })
                    .to_string(),
                )]),
                sound_current_fingerprints: BTreeMap::new(),
                deviation_current_fingerprints: BTreeMap::new(),
                sound_current_fingerprint_parts: BTreeMap::new(),
                sketch_proof_nodes: BTreeSet::new(),
                substantiveness_current_fingerprints: BTreeMap::new(),
                protected_closure_nodes_per_target: BTreeMap::new(),
            },
            ..WorkerResponse::default()
        };

        let err = proof_protected_package_legality_error(
            "proof_restructure",
            &acceptance_context,
            &response,
        );

        assert!(
            err.is_some(),
            "Lean-relevant descendant change must trigger paper-fingerprint error"
        );
        assert!(
            err.as_deref().unwrap_or("").contains("paper fingerprints"),
            "expected paper-fingerprint error, got: {:?}",
            err
        );
    }

    #[test]
    fn cleanup_worker_result_check_rejects_stuck_at_raw_validation_floor() {
        let tmp = tempdir().expect("tempdir");
        let acceptance_context = serde_json::json!({
            "worker_acceptance": {
                "validation_kind": "cleanup",
            }
        });
        let raw_payload = serde_json::json!({
            "outcome": "stuck",
            "summary": "cannot finish",
            "comments": "",
            "semantic_dep_updates": {},
            "target_claim_updates": {},
            "difficulty_updates": {},
        });

        let output =
            check_trellis_worker_result_output(tmp.path(), acceptance_context, raw_payload)
                .expect("check output");

        assert!(!output.ok);
        assert!(output
            .errors
            .iter()
            .any(|err| err.contains("outcome must be one of ['valid', 'invalid']")));
        assert!(output.data.is_none());
        assert!(output.response.is_none());
    }

    #[test]
    fn bridge_request_payload_preserves_retry_fields() {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::Worker;
        state.cycle = 3;
        state.retry_outcome_kind = RetryOutcomeKind::Invalid;
        state.attempt = 2;
        state.invalid_attempt = true;
        let request = state.expected_request(15, RequestKind::Worker);

        let payload = bridge_request_payload(&request, None, None).expect("bridge request payload");

        assert_eq!(payload["invalid_attempt"], serde_json::json!(true));
        assert_eq!(payload["retry_outcome_kind"], serde_json::json!("Invalid"));
        assert_eq!(payload["retry_attempt"], serde_json::json!(2));
    }

    #[test]
    fn bridge_request_payload_forwards_substantiveness_verify_nodes() {
        // Regression: bridge_request_payload missed `substantiveness_verify_nodes`
        // in the JSON, so the Python bridge's `_handle_paper` saw an empty
        // per-node frontier (`request.get("substantiveness_verify_nodes", [])`
        // returned []), took the no-op short-circuit, and returned
        // `member_responses: []`. The kernel's substantiveness drain then
        // treated the no-progress response as a verifier-stuck signal, and
        // after `SUBSTANTIVENESS_MAX_CONSECUTIVE_NO_PROGRESS = 5` rounds,
        // escalated to Reviewer with all blockers Unknown — surfacing a
        // K-1-shaped verifier-starvation collateral.
        let mut request = trellis_kernel::WrapperRequest::default();
        request.id = 4;
        request.cycle = 1;
        request.kind = RequestKind::Paper;
        request.phase = Phase::TheoremStating;
        request.substantiveness_verify_nodes =
            BTreeSet::from([NodeId::from("Foo"), NodeId::from("Bar")]);
        request.verify_lanes = BTreeSet::from(["v1".to_string(), "v2".to_string()]);

        let payload = bridge_request_payload(&request, None, None).expect("bridge request payload");

        assert_eq!(
            payload["substantiveness_verify_nodes"],
            serde_json::json!(["Bar", "Foo"])
        );
    }

    #[test]
    fn bridge_request_payload_emits_every_wrapper_request_field() {
        // Structural regression test for the recurring bug class where a
        // newly-added WrapperRequest field is silently absent from the
        // bridge JSON, causing the on-disk request to deserialize back
        // with the field at serde default and the validator to reject
        // otherwise-legal reviewer responses. Fields that have previously
        // hit this class: `substantiveness_verify_nodes`, the new-soundness
        // cluster (`sound_verifier_requestable_nodes`,
        // `sound_repair_ready_nodes`, `kernel_hinted_next_active_coarse_nodes`,
        // `proof_active_node_base_legal_candidates`, etc.).
        //
        // The fix is structural — `bridge_request_payload` now seeds from
        // `serde_json::to_value(request)` rather than a hand-built JSON
        // whitelist. This test pins that property: every public field on
        // WrapperRequest must appear as a top-level key in the payload.
        let request = trellis_kernel::WrapperRequest::default();
        let payload = bridge_request_payload(&request, None, None).expect("bridge request payload");
        let payload_obj = payload
            .as_object()
            .expect("payload must serialize to a JSON object");

        // Reflect over the WrapperRequest serde JSON to discover its
        // full field set, then assert each appears in the payload. The
        // overlays (kind/phase/mode/etc.) are tested implicitly because
        // those field names exist in both serde-emit and overlay form.
        let direct = serde_json::to_value(&request).expect("serialize WrapperRequest");
        let direct_obj = direct
            .as_object()
            .expect("WrapperRequest must serialize to an object");
        let mut missing: Vec<&String> = direct_obj
            .keys()
            .filter(|key| !payload_obj.contains_key(*key))
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "bridge_request_payload dropped {} WrapperRequest field(s) the serde renderer emits: {missing:?}. \
             If you added a field to WrapperRequest, the structural serde-seeded payload should pick it up \
             automatically — this assertion failing means the seeding logic itself regressed.",
            missing.len()
        );

        // Spot-check the previously-stripped new-soundness fields are
        // present (would fail under the pre-fix manual whitelist).
        for key in [
            "sound_verifier_requestable_nodes",
            "sound_repair_ready_nodes",
            "sound_assessment_statuses",
            "previous_sound_lane_findings",
            "kernel_hinted_next_active_coarse_nodes",
            "proof_active_node_base_legal_candidates",
            "coarse_repair_blocker_carriers",
            "resettable_theorem_stating_nodes",
            "cleanup_force_done_view",
            "cycles_since_clean",
            "last_clean_rewind_count",
            "latest_worker_summary",
            "latest_worker_needs_restructure_suggested_nodes",
        ] {
            assert!(
                payload_obj.contains_key(key),
                "bridge_request_payload must emit `{key}` (would have failed under the pre-fix manual whitelist)",
            );
        }
    }

    #[test]
    fn bridge_request_payload_round_trips_sound_verifier_requestable_nodes() {
        // Tighter regression: actual values round-trip through serde back
        // into a WrapperRequest with the field populated, not just present
        // as a key. Mirrors the failure mode where
        // `sound_verifier_requestable_nodes` deserialized
        // back as an empty BTreeSet and the validator rejected
        // `request_sound_verifier_node_ids=[LocalDecoderLemma]` as
        // "legal Sound verifier targets are {}".
        let mut request = trellis_kernel::WrapperRequest::default();
        request.id = 53;
        request.cycle = 12;
        request.kind = RequestKind::Review;
        request.phase = Phase::TheoremStating;
        request.sound_verifier_requestable_nodes =
            BTreeSet::from([NodeId::from("LocalDecoderLemma")]);
        request.sound_repair_ready_nodes = BTreeSet::from([
            NodeId::from("LocalDecoderLemma"),
            NodeId::from("CoverLemma"),
        ]);

        let payload = bridge_request_payload(&request, None, None).expect("bridge request payload");
        let round_tripped: trellis_kernel::WrapperRequest =
            serde_json::from_value(payload).expect("round-trip payload back into WrapperRequest");

        assert_eq!(
            round_tripped.sound_verifier_requestable_nodes,
            BTreeSet::from([NodeId::from("LocalDecoderLemma")]),
            "sound_verifier_requestable_nodes must survive the JSON round-trip"
        );
        assert_eq!(
            round_tripped.sound_repair_ready_nodes,
            BTreeSet::from([
                NodeId::from("LocalDecoderLemma"),
                NodeId::from("CoverLemma")
            ]),
            "sound_repair_ready_nodes must survive the JSON round-trip"
        );
    }

    #[test]
    fn checked_worker_payload_preserves_needs_restructure_suggested_nodes() {
        // Regression: validate_trellis_worker_result_data requires + extracts
        // needs_restructure_suggested_nodes when outcome=needs_restructure
        // (and re-emits it in the success payload). But CheckedWorkerPayload
        // had no field for it, so serde-default dropped the array during
        // deserialization and accept_worker_response built a WorkerResponse
        // with an empty suggested set. The reviewer's
        // latest_worker_needs_restructure_suggested_nodes snapshot was
        // therefore always [], defeating the whole point of the field
        // (let reviewer widen scope concretely instead of guessing what
        // "needs broader repair" means).
        let raw_payload = serde_json::json!({
            "outcome": "needs_restructure",
            "summary": "needs broader scope",
            "comments": "active node alone cannot close; need HelperA and HelperB included",
            "needs_restructure_suggested_nodes": ["HelperA", "HelperB"],
        });
        let validated = trellis_kernel::validate_trellis_worker_result_data(&raw_payload);
        assert!(validated.ok, "validator errors={:?}", validated.errors);
        let validated_data = validated.data.expect("validator must emit data");
        assert_eq!(
            validated_data["needs_restructure_suggested_nodes"],
            serde_json::json!(["HelperA", "HelperB"]),
            "validator must re-emit the suggested-nodes field"
        );

        let payload: CheckedWorkerPayload = serde_json::from_value(validated_data)
            .expect("validator output must deserialize into CheckedWorkerPayload");
        assert_eq!(
            payload.needs_restructure_suggested_nodes,
            vec!["HelperA".to_string(), "HelperB".to_string()],
            "CheckedWorkerPayload must preserve the suggested-nodes field; \
             a missing field here means accept_worker_response will build a \
             WorkerResponse with the suggested set empty and the reviewer \
             snapshot will always be []"
        );
    }

    #[test]
    fn bridge_request_payload_preserves_protected_review_scope_fields() {
        let mut request = trellis_kernel::WrapperRequest::default();
        request.id = 12;
        request.cycle = 4;
        request.kind = RequestKind::Review;
        request.phase = Phase::ProofFormalization;
        request.approved_target_nodes =
            BTreeSet::from([NodeId::from("ProtectedA"), NodeId::from("ProtectedB")]);
        request.protected_semantic_change_confirmation =
            Some(trellis_kernel::ProtectedSemanticChangeConfirmation {
                nodes: BTreeSet::from([NodeId::from("ProtectedA")]),
                next_active: Some(NodeId::from("Active")),
                next_mode: TaskMode::CoarseRestructure,
                allow_new_obligations: true,
                must_close_active: false,
            });

        let payload = bridge_request_payload(&request, None, None).expect("bridge request payload");

        assert_eq!(
            payload["approved_target_nodes"],
            serde_json::json!(["ProtectedA", "ProtectedB"])
        );
        assert_eq!(
            payload["protected_semantic_change_confirmation"]["nodes"],
            serde_json::json!(["ProtectedA"])
        );
    }

    #[test]
    fn bridge_worker_payload_preserves_coarse_dag_nodes_for_acceptance_context() {
        let tmp = tempdir().expect("tempdir");
        let mut request = trellis_kernel::WrapperRequest::default();
        request.id = 16;
        request.cycle = 6;
        request.kind = RequestKind::Worker;
        request.phase = Phase::ProofFormalization;
        request.mode = TaskMode::Restructure;
        request.active_node = Some(NodeId::from("ProofPhaseHelper"));
        request.current_present_nodes = BTreeSet::from([
            NodeId::from("CoarseA"),
            NodeId::from("CoarseB"),
            NodeId::from("ProofPhaseHelper"),
        ]);
        request.current_paper_approved_fingerprints.insert(
            TargetId::from("target_main"),
            "paper-approved-fp".to_string(),
        );
        request.coarse_dag_nodes =
            BTreeSet::from([NodeId::from("CoarseA"), NodeId::from("CoarseB")]);
        request.worker_acceptance.enabled = true;
        request.worker_acceptance.validation_kind = WorkerValidationKind::ProofRestructure;

        let payload = bridge_request_payload(&request, None, None).expect("bridge request payload");

        assert_eq!(
            payload["coarse_dag_nodes"],
            serde_json::json!(["CoarseA", "CoarseB"])
        );
        assert_eq!(
            payload["worker_contract"]["scope_contract"]["coarse_dag_nodes"],
            serde_json::json!(["CoarseA", "CoarseB"])
        );
        assert_eq!(
            payload["current_paper_approved_fingerprints"],
            serde_json::json!({"target_main": "paper-approved-fp"})
        );

        let round_tripped: trellis_kernel::WrapperRequest =
            serde_json::from_value(payload).expect("round-tripped wrapper request");
        let acceptance = prepare_worker_gate_output(tmp.path(), &round_tripped, false, None)
            .expect("prepared worker gate output");

        assert_eq!(
            acceptance.coarse_dag_nodes,
            BTreeSet::from([NodeId::from("CoarseA"), NodeId::from("CoarseB")])
        );
        assert_eq!(
            acceptance.request["worker_contract"]["scope_contract"]["coarse_dag_nodes"],
            serde_json::json!(["CoarseA", "CoarseB"])
        );
        assert_eq!(
            acceptance.current_paper_approved_fingerprints,
            BTreeMap::from([(
                TargetId::from("target_main"),
                "paper-approved-fp".to_string(),
            )])
        );
    }

    #[test]
    fn bridge_review_normalization_accepts_protected_scope_from_bridge_payload() {
        let mut request = trellis_kernel::WrapperRequest::default();
        request.id = 13;
        request.cycle = 5;
        request.kind = RequestKind::Review;
        request.phase = Phase::ProofFormalization;
        request.allowed_decisions = BTreeSet::from([ReviewDecisionKind::Continue]);
        request.allowed_next_modes = BTreeSet::from([TaskMode::CoarseRestructure]);
        request.kernel_hinted_next_active_nodes = BTreeSet::from([NodeId::from("Active")]);
        request.allowed_resets = BTreeSet::from([ResetChoice::None]);
        request.approved_target_nodes = BTreeSet::from([NodeId::from("ProtectedA")]);
        request.current_present_nodes =
            BTreeSet::from([NodeId::from("Active"), NodeId::from("ProtectedA")]);
        let review_request =
            bridge_request_payload(&request, None, None).expect("bridge request payload");
        let raw_payload = serde_json::json!({
            "decision": "continue",
            "reason": "protected change is required",
            "comments": "",
            "task_blocker_ids": [],
            "override_blocker_ids": [],
            "reset_blocker_ids": [],
            "next_active": "Active",
            "next_mode": "coarse_restructure",
            "reset": "none",
            "difficulty_updates": {},
            "allow_new_obligations": true,
            "must_close_active": false,
            "next_worker_context_mode": "resume",
            "paper_focus_ranges": [],
            "work_style_hint": "none",
            "protected_semantic_change_node_ids": ["ProtectedA"],
            "confirm_protected_semantic_change_scope": false,
            "authorized_node_ids": ["Active", "ProtectedA"],
        });

        let output = super::check_trellis_reviewer_result_output(review_request, raw_payload)
            .expect("review normalization output");

        assert!(output.ok, "errors: {:?}", output.errors);
        let response = output.response.expect("normalized response");
        assert_eq!(
            response["protected_semantic_change_nodes"],
            serde_json::json!(["ProtectedA"])
        );
    }

    #[test]
    fn human_gate_payload_surfaces_protected_reapproval_nodes() {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        state.stage = Stage::HumanGate;
        state.gate_kind = GateKind::ProtectedReapproval;
        state.pending_protected_reapproval_nodes = BTreeSet::from([NodeId::from("ProtectedA")]);
        let request = state.expected_request(21, RequestKind::HumanGate);

        let payload = bridge_request_payload(&request, None, None).expect("bridge request payload");

        assert_eq!(
            payload["gate_kind"],
            serde_json::json!("protected_reapproval")
        );
        assert_eq!(
            payload["protected_reapproval_nodes"],
            serde_json::json!(["ProtectedA"])
        );
    }

    #[test]
    fn node_deviation_claims_after_updates_seeds_from_affected_nodes() {
        // P2(b) parity: hydrator must mirror `apply_worker_structure_updates`
        // semantics: when a worker response sets
        // `deviation_requests[id].affected_nodes = {N}` and leaves
        // `node_deviation_claims` untouched, the post-response view
        // must claim `id` for node `N`. Otherwise the hydrator's
        // substantiveness fingerprint snapshot and the kernel's apply
        // result will disagree.
        let dev_id = DeviationId::from("dev:a");
        let node = NodeId::from("N");
        let other = NodeId::from("M");
        let present = BTreeSet::from([node.clone(), other.clone()]);
        let base: BTreeMap<NodeId, BTreeSet<DeviationId>> = BTreeMap::new();
        let requests = BTreeMap::from([(
            dev_id.clone(),
            DeviationRequest {
                path: "reference/dev_a.tex".to_string(),
                summary: "a deviation".to_string(),
                affected_nodes: BTreeSet::from([node.clone()]),
            },
        )]);
        let explicit_updates: BTreeMap<NodeId, BTreeSet<DeviationId>> = BTreeMap::new();

        let claims =
            node_deviation_claims_after_updates(&base, &requests, &explicit_updates, &present);

        assert_eq!(
            claims.get(&node),
            Some(&BTreeSet::from([dev_id.clone()])),
            "affected_node N must inherit deviation `dev:a`",
        );
        assert!(
            !claims.contains_key(&other),
            "non-affected node M must not be claimed",
        );
    }

    #[test]
    fn node_deviation_claims_after_updates_lets_explicit_clears_override() {
        // Explicit `node_deviation_claims[N] = {}` must override the
        // seed from `deviation_requests.affected_nodes`. This matches
        // `apply_worker_structure_updates` ordering (affected_nodes
        // first, explicit overrides second).
        let dev_id = DeviationId::from("dev:a");
        let node = NodeId::from("N");
        let present = BTreeSet::from([node.clone()]);
        let base: BTreeMap<NodeId, BTreeSet<DeviationId>> = BTreeMap::new();
        let requests = BTreeMap::from([(
            dev_id.clone(),
            DeviationRequest {
                path: "reference/dev_a.tex".to_string(),
                summary: "a deviation".to_string(),
                affected_nodes: BTreeSet::from([node.clone()]),
            },
        )]);
        let explicit_updates = BTreeMap::from([(node.clone(), BTreeSet::new())]);

        let claims =
            node_deviation_claims_after_updates(&base, &requests, &explicit_updates, &present);

        assert!(
            !claims.contains_key(&node),
            "explicit empty-set clear must win over affected_nodes seed; got {:?}",
            claims,
        );
    }

    #[test]
    fn populate_response_fingerprints_seeds_substantiveness_from_affected_nodes_only() {
        // End-to-end parity test: a worker response that registers a
        // new deviation only via `deviation_requests.affected_nodes`
        // (no explicit `node_deviation_claims` entry) must produce a
        // `substantiveness_current_fingerprints[N]` that already embeds
        // the deviation fingerprint. This is what
        // `apply_worker_structure_updates` would produce on the kernel
        // side, so the two views must agree.
        use trellis_kernel::NodeKind;
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().to_path_buf();
        fs::create_dir_all(repo.join("Tablet")).expect("create Tablet");
        fs::create_dir_all(repo.join("reference")).expect("create reference");
        fs::write(repo.join("Tablet/N.tex"), "\\begin{theorem}N\\end{theorem}")
            .expect("write N.tex");
        fs::write(repo.join("Tablet/Preamble.tex"), "").expect("write Preamble.tex");
        fs::write(repo.join("paper.tex"), "paper").expect("write paper.tex");
        fs::write(
            repo.join("reference/dev_a.tex"),
            "\\section*{dev_a}\nA deviation\n",
        )
        .expect("write dev_a.tex");

        let node = NodeId::from("N");
        let dev_id = DeviationId::from("dev:a");
        let configured_targets: BTreeSet<TargetId> = BTreeSet::new();
        let current_target_claims: BTreeMap<NodeId, BTreeSet<TargetId>> = BTreeMap::new();
        let current_deviation_files: BTreeMap<DeviationId, String> = BTreeMap::new();
        let current_node_deviation_claims: BTreeMap<NodeId, BTreeSet<DeviationId>> =
            BTreeMap::new();
        let approved_paper_fingerprints: BTreeMap<TargetId, String> = BTreeMap::new();
        let current_node_kinds = BTreeMap::from([
            (NodeId::from("Preamble"), NodeKind::Preamble),
            (node.clone(), NodeKind::Definition),
        ]);
        let paper_source = repo.join("paper.tex");

        let mut response = WorkerResponse {
            snapshot: WorkingSnapshot {
                present_nodes: BTreeSet::from([NodeId::from("Preamble"), node.clone()]),
                ..WorkingSnapshot::default()
            },
            deviation_requests: BTreeMap::from([(
                dev_id.clone(),
                DeviationRequest {
                    path: "reference/dev_a.tex".to_string(),
                    summary: "a deviation".to_string(),
                    affected_nodes: BTreeSet::from([node.clone()]),
                },
            )]),
            // INTENTIONALLY empty explicit claims: only affected_nodes
            // is providing the claim.
            node_deviation_claims: BTreeMap::new(),
            ..WorkerResponse::default()
        };

        populate_response_fingerprints(
            &repo,
            &configured_targets,
            &current_target_claims,
            &current_deviation_files,
            &current_node_deviation_claims,
            &approved_paper_fingerprints,
            Some(paper_source.as_path()),
            &current_node_kinds,
            &mut response,
        )
        .expect("populate_response_fingerprints");

        let dev_fp = response
            .snapshot
            .deviation_current_fingerprints
            .get(&dev_id)
            .expect("deviation fingerprint must be observed");
        assert!(
            !dev_fp.is_empty(),
            "deviation fingerprint must be non-empty for a file that exists",
        );
        let subst_storage = response
            .snapshot
            .substantiveness_current_fingerprints
            .get(&node)
            .expect("substantiveness fingerprint must be populated for N");
        let parsed =
            super::runtime_cli_observations::SubstantivenessFingerprint::from_storage_string(
                subst_storage,
            )
            .expect("parse substantiveness fingerprint");
        assert_eq!(
            parsed.claimed_deviation_fingerprints.get(&dev_id),
            Some(dev_fp),
            "substantiveness fingerprint must embed the deviation fingerprint for N \
             when only affected_nodes was provided; got {:?}",
            parsed.claimed_deviation_fingerprints,
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Patch C-D local-closure runtime orchestration tests (plan §7.5/§7.10).
    // ────────────────────────────────────────────────────────────────

    mod local_closure {
        use super::super::{
            backfill_local_closure_record_hashes, build_failure_summary,
            build_transport_error_summary, compute_local_closure_record_inputs,
            deterministic_revalidate_at_cli_with_probe, hash_approved_axioms_for_node, hash_text,
            load_persisted_record, local_closure_axcheck_required_for_repo,
            local_closure_migration_skip_reason, local_closure_records_dir, persist_record_to_disk,
            record_hashes_match_current, record_needs_hash_backfill,
            rescind_records_with_stale_approved_axioms_hash_pure,
            rescind_records_with_stale_axcheck_status_pure, run_migration_if_needed_with_probe,
            run_pre_step_revalidation_if_needed_pure, CleanupRevalidationAdapter,
            CLOSURE_HASH_SENTINEL, CLOSURE_VERSION, CLOSURE_VERSION_SENTINEL,
            TRANSPORT_BACKOFF_MAX_CYCLES, TRANSPORT_RETRY_BUDGET,
        };
        use std::collections::{BTreeMap, BTreeSet};
        use std::fs;
        use std::path::Path;
        use tempfile::tempdir;
        use trellis_kernel::{
            AxcheckStatus, AxiomizationCheckOutput, ErrorSummary, LocalClosureProbeOutput,
            LocalClosureRecord, NodeId, Phase, ProtocolState, RequestKind, RevalidationBatch,
            WorkerValidationKind, WrapperAdapter, WrapperRequest, WrapperResponse,
        };

        fn seed_repo(repo: &Path) {
            fs::create_dir_all(repo.join("Tablet")).expect("create Tablet dir");
            fs::write(repo.join("lean-toolchain"), "leanprover/lean4:v4.x.x\n")
                .expect("write toolchain");
            fs::write(repo.join("lake-manifest.json"), "{\"version\": 0}\n")
                .expect("write manifest");
            fs::write(repo.join("Tablet/Preamble.lean"), "-- preamble\n").expect("write preamble");
        }

        fn write_node(repo: &Path, name: &str, body: &str) {
            fs::write(
                repo.join("Tablet").join(format!("{name}.lean")),
                format!("import Tablet.Preamble\ntheorem {name} : True := {body}\n"),
            )
            .expect("write node");
        }

        fn ok_probe(node: &NodeId) -> LocalClosureProbeOutput {
            let _ = node;
            LocalClosureProbeOutput {
                status: "ok".to_string(),
                kernel_axioms: BTreeSet::new(),
                boundary_theorems: BTreeMap::new(),
                strict_theorem_deps: BTreeMap::new(),
                strict_definition_deps: BTreeMap::new(),
                errors: Vec::new(),
                raw_stdout: String::new(),
                raw_stderr: String::new(),
                returncode: 0,
                timed_out: false,
                axiomization_check: None,
            }
        }

        fn ok_probe_axcheck_agreed(node: &NodeId) -> LocalClosureProbeOutput {
            let mut probe = ok_probe(node);
            probe.axiomization_check = Some(AxiomizationCheckOutput {
                agreed: true,
                skipped: false,
                ..AxiomizationCheckOutput::default()
            });
            probe
        }

        fn fail_probe() -> LocalClosureProbeOutput {
            LocalClosureProbeOutput {
                status: "axiom_violation".to_string(),
                kernel_axioms: BTreeSet::from(["UnapprovedAx".to_string()]),
                boundary_theorems: BTreeMap::new(),
                strict_theorem_deps: BTreeMap::new(),
                strict_definition_deps: BTreeMap::new(),
                errors: vec!["[axiom] active proof uses unapproved kernel axiom".to_string()],
                raw_stdout: String::new(),
                raw_stderr: String::new(),
                returncode: 1,
                timed_out: false,
                axiomization_check: None,
            }
        }

        #[test]
        fn compute_record_inputs_produces_stable_hashes_for_stable_inputs() {
            // Plan §7.10 / test 8 — hash computation is deterministic.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let node = NodeId::from("Foo");
            let r1 = compute_local_closure_record_inputs(
                repo,
                &node,
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap1".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            let r2 = compute_local_closure_record_inputs(
                repo,
                &node,
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap1".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            assert_eq!(r1.toolchain_hash, r2.toolchain_hash);
            assert_eq!(r1.lake_manifest_hash, r2.lake_manifest_hash);
            assert_eq!(r1.preamble_hash, r2.preamble_hash);
            assert_eq!(r1.active_decl_hash, r2.active_decl_hash);
            assert_eq!(r1.active_statement_hash, r2.active_statement_hash);
            assert_eq!(r1.approved_axioms_hash, r2.approved_axioms_hash);
            assert_eq!(r1.closure_version, CLOSURE_VERSION);

            // Mutating .lean must change the hashes that depend on it.
            write_node(repo, "Foo", "by trivial");
            let r3 = compute_local_closure_record_inputs(
                repo,
                &node,
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap1".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            assert_ne!(r1.active_decl_hash, r3.active_decl_hash);
        }

        #[test]
        fn backfill_replaces_sentinel_hashes() {
            // Plan §7.0 — C-B writes sentinel placeholders; the C-D backfill
            // pass replaces them with real hashes.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            let mut record = LocalClosureRecord::default();
            record.node = NodeId::from("Foo");
            record.closure_version = CLOSURE_VERSION_SENTINEL.to_string();
            record.toolchain_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.lake_manifest_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.preamble_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.approved_axioms_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.active_decl_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.active_statement_hash = CLOSURE_HASH_SENTINEL.to_string();
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record);

            let outcome = backfill_local_closure_record_hashes(&mut state, repo, 0);
            assert!(outcome.mutated, "backfill must report state mutation");
            assert!(
                outcome.demoted_nodes.is_empty(),
                "successful backfill must not demote any node; got {:?}",
                outcome.demoted_nodes
            );
            let refreshed = &state.local_closure_records[&NodeId::from("Foo")];
            assert_eq!(refreshed.closure_version, CLOSURE_VERSION);
            assert_ne!(refreshed.toolchain_hash, CLOSURE_HASH_SENTINEL);
            assert_ne!(refreshed.active_decl_hash, CLOSURE_HASH_SENTINEL);
            assert_ne!(refreshed.active_statement_hash, CLOSURE_HASH_SENTINEL);
        }

        #[test]
        fn backfill_is_idempotent_on_real_records() {
            // Calling backfill twice must not change anything on the
            // second pass — supports running the backfill at every step.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap1".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record);
            let outcome_first = backfill_local_closure_record_hashes(&mut state, repo, 0);
            assert!(
                !outcome_first.mutated,
                "real records should not be touched by backfill"
            );
            assert!(
                outcome_first.demoted_nodes.is_empty(),
                "idempotent backfill must not demote anything",
            );
        }

        // ────────────────────────────────────────────────────────────
        // Audit H-2 — approved-axiom rescission tests.
        // ────────────────────────────────────────────────────────────

        #[test]
        fn rescission_demotes_record_when_approved_axioms_file_changes() {
            // Operator installs a record with current
            // APPROVED_AXIOMS.json hash. Later rewrites
            // APPROVED_AXIOMS.json to broaden axioms. The H-2 hook
            // must detect the hash drift and demote the record to
            // unverified.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            // Write APPROVED_AXIOMS.json baseline.
            fs::write(
                repo.join("APPROVED_AXIOMS.json"),
                r#"{"global":["propext"],"nodes":{}}"#,
            )
            .expect("write baseline approved");
            let baseline_hash = hash_approved_axioms_for_node(repo, "Foo").expect("hash baseline");

            // Install a record with baseline hash.
            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            let mut record = LocalClosureRecord::default();
            record.node = NodeId::from("Foo");
            record.closure_version = CLOSURE_VERSION.to_string();
            record.approved_axioms_hash = baseline_hash.clone();
            record.toolchain_hash = "tc".to_string();
            record.lake_manifest_hash = "lk".to_string();
            record.preamble_hash = "pr".to_string();
            record.active_decl_hash = "decl".to_string();
            record.active_statement_hash = "stmt".to_string();
            record.axcheck_status = AxcheckStatus::Agreed;
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record);

            // Drift: rewrite APPROVED_AXIOMS.json.
            fs::write(
                repo.join("APPROVED_AXIOMS.json"),
                r#"{"global":["propext","funext","Lean.ofReduceBool"],"nodes":{}}"#,
            )
            .expect("write drifted approved");

            let demoted =
                rescind_records_with_stale_approved_axioms_hash_pure(&mut state, repo, 17);
            assert_eq!(
                demoted,
                vec![NodeId::from("Foo")],
                "rescission must demote Foo"
            );
            assert!(
                !state
                    .local_closure_records
                    .contains_key(&NodeId::from("Foo")),
                "demoted record must leave records map"
            );
            assert!(
                state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Foo")),
                "demoted node must enter unverified set"
            );
            assert!(
                state
                    .local_closure_failures
                    .contains_key(&NodeId::from("Foo")),
                "demoted node must carry failure summary"
            );
            let summary = &state.local_closure_failures[&NodeId::from("Foo")];
            assert_eq!(summary.status, "axiom_violation");
            assert_eq!(summary.captured_at_cycle, 17);
        }

        #[test]
        fn rescission_is_idempotent_when_hashes_match_current_policy() {
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            fs::write(
                repo.join("APPROVED_AXIOMS.json"),
                r#"{"global":["propext"],"nodes":{}}"#,
            )
            .expect("write approved");
            let current_hash = hash_approved_axioms_for_node(repo, "Foo").expect("hash");

            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            let mut record = LocalClosureRecord::default();
            record.node = NodeId::from("Foo");
            record.closure_version = CLOSURE_VERSION.to_string();
            record.approved_axioms_hash = current_hash;
            record.axcheck_status = AxcheckStatus::Agreed;
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record.clone());

            let demoted = rescind_records_with_stale_approved_axioms_hash_pure(&mut state, repo, 1);
            assert!(
                demoted.is_empty(),
                "matching-hash record must not be demoted; got {:?}",
                demoted
            );
            assert!(state
                .local_closure_records
                .contains_key(&NodeId::from("Foo")));
        }

        #[test]
        fn rescission_demotes_when_approved_axioms_load_fails() {
            // Corrupt APPROVED_AXIOMS.json: the rescission must
            // defensively demote (we can't prove the record's hash
            // matches current policy).
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            fs::write(repo.join("APPROVED_AXIOMS.json"), "{ not valid json")
                .expect("write corrupt");

            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            let mut record = LocalClosureRecord::default();
            record.node = NodeId::from("Foo");
            record.closure_version = CLOSURE_VERSION.to_string();
            record.approved_axioms_hash = "any-old-hash".to_string();
            record.axcheck_status = AxcheckStatus::Agreed;
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record);

            let demoted = rescind_records_with_stale_approved_axioms_hash_pure(&mut state, repo, 2);
            assert_eq!(
                demoted,
                vec![NodeId::from("Foo")],
                "corrupt-policy must demote defensively"
            );
            assert!(
                state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Foo")),
                "defensively-demoted node must enter unverified"
            );
        }

        // ────────────────────────────────────────────────────────────
        // Audit H-4 — axcheck-status rescission tests.
        // ────────────────────────────────────────────────────────────

        #[test]
        fn axcheck_rescission_demotes_skipped_record_when_policy_requires_axcheck() {
            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            let mut record = LocalClosureRecord::default();
            record.node = NodeId::from("Foo");
            record.axcheck_status = AxcheckStatus::Skipped;
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record);

            let demoted = rescind_records_with_stale_axcheck_status_pure(&mut state, true, 23);
            assert_eq!(demoted, vec![NodeId::from("Foo")]);
            assert!(
                !state
                    .local_closure_records
                    .contains_key(&NodeId::from("Foo")),
                "skipped-axcheck record must leave records map when axcheck is required"
            );
            assert!(
                state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Foo")),
                "demoted node must enter unverified set"
            );
            let summary = &state.local_closure_failures[&NodeId::from("Foo")];
            assert_eq!(summary.status, "internal_error");
            assert_eq!(summary.captured_at_cycle, 23);
        }

        #[test]
        fn axcheck_rescission_is_noop_when_policy_disables_axcheck() {
            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            let mut record = LocalClosureRecord::default();
            record.node = NodeId::from("Foo");
            record.axcheck_status = AxcheckStatus::Skipped;
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record);

            let demoted = rescind_records_with_stale_axcheck_status_pure(&mut state, false, 23);
            assert!(demoted.is_empty());
            assert!(state
                .local_closure_records
                .contains_key(&NodeId::from("Foo")));
        }

        #[test]
        fn record_hashes_match_current_rejects_skipped_axcheck_when_policy_requires_it() {
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            assert!(
                local_closure_axcheck_required_for_repo(repo),
                "missing config defaults to axcheck required"
            );
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap-skipped".to_string(),
                AxcheckStatus::Skipped,
            )
            .expect("compute record inputs");
            let state = ProtocolState::default();
            assert!(
                !record_hashes_match_current(&record, repo, &state),
                "skipped-axcheck persisted record must not reinstall when policy requires axcheck"
            );
        }

        #[test]
        fn record_hashes_match_current_accepts_skipped_axcheck_when_policy_disables_it() {
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            fs::write(
                repo.join("trellis.config.json"),
                r#"{"local_closure_axcheck_enabled": false}"#,
            )
            .expect("write config");
            assert!(
                !local_closure_axcheck_required_for_repo(repo),
                "explicit false disables axcheck requirement"
            );
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap-skipped".to_string(),
                AxcheckStatus::Skipped,
            )
            .expect("compute record inputs");
            let state = ProtocolState::default();
            assert!(
                record_hashes_match_current(&record, repo, &state),
                "skipped-axcheck record remains valid only while policy disables axcheck"
            );
        }

        #[test]
        fn sentinel_record_backfill_failure_demotes_record_to_internal_error_failure() {
            // Patch C-O MEDIUM 1 — when backfill cannot compute real
            // hashes for a sentinel record (e.g. APPROVED_AXIOMS.json is
            // corrupt), the record must NOT stay in the live set: that
            // would let `formalization_complete` pass on a stale
            // placeholder until the next restart. Fail closed: drop the
            // record, install an `internal_error` failure summary, and
            // re-add the node to the unverified set so it is reprobed.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            // Corrupt the approved-axioms file so hash backfill fails.
            fs::write(repo.join("APPROVED_AXIOMS.json"), "{ not valid json")
                .expect("write corrupt approved");

            let mut state = ProtocolState::default();
            let mut record = LocalClosureRecord::default();
            record.node = NodeId::from("Foo");
            record.closure_version = CLOSURE_VERSION_SENTINEL.to_string();
            record.toolchain_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.lake_manifest_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.preamble_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.approved_axioms_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.active_decl_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.active_statement_hash = CLOSURE_HASH_SENTINEL.to_string();
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record);

            let outcome = backfill_local_closure_record_hashes(&mut state, repo, 42);
            assert!(
                outcome.mutated,
                "backfill must report state mutation when it demotes a failed record"
            );

            // Record was removed.
            assert!(
                !state
                    .local_closure_records
                    .contains_key(&NodeId::from("Foo")),
                "sentinel record must be removed when backfill cannot complete it"
            );

            // Internal-error failure summary installed.
            let summary = state
                .local_closure_failures
                .get(&NodeId::from("Foo"))
                .expect("failure summary must be installed");
            assert_eq!(
                summary.status, "internal_error",
                "demoted record must surface as internal_error; got {}",
                summary.status
            );
            assert!(
                summary.stderr_excerpt.contains("backfill failed"),
                "stderr_excerpt must mention backfill; got {:?}",
                summary.stderr_excerpt
            );
            assert_eq!(summary.captured_at_cycle, 42);

            // Node returns to the unverified set so a future probe will
            // surface the real outcome.
            assert!(
                state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Foo")),
                "demoted node must be re-added to the unverified set"
            );

            // Patch C-Q Q6 — demote must also surface a
            // `demoted_nodes` entry so the caller deletes the stale
            // persisted JSON file under
            // `<runtime_root>/checker-state/local-closure-records/`.
            // The caller in `step_runtime` uses
            // `persisted_record_file_name` to compute the path and
            // unlinks it; this assertion guarantees the data needed
            // for that cleanup is surfaced.
            assert_eq!(
                outcome.demoted_nodes,
                vec![NodeId::from("Foo")],
                "demote path must surface the node so the caller can delete its persisted JSON"
            );
        }

        #[test]
        fn backfill_demote_emits_delete_command_for_persisted_record() {
            // Patch C-Q Q6 — when backfill demotes a sentinel record
            // (e.g. APPROVED_AXIOMS.json corruption) the caller must
            // delete the persisted JSON file too. Otherwise a future
            // rewind or state-file loss can reload the sentinel from
            // disk and clobber the `internal_error` failure we just
            // installed. This test exercises the full demote path
            // end-to-end: persist a sentinel record, corrupt the
            // approved-axioms file so backfill must fail, run
            // backfill, then manually invoke the cleanup the caller
            // would perform (deleting via `persisted_record_file_name`)
            // and assert the file is gone.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");

            let runtime_root_dir = tempdir().expect("runtime root");
            let runtime_root = runtime_root_dir.path();
            let records_dir = local_closure_records_dir(runtime_root);

            // Persist the sentinel record to disk so the demote has
            // something to delete.
            fs::create_dir_all(&records_dir).expect("records dir");
            let mut record = LocalClosureRecord::default();
            record.node = NodeId::from("Foo");
            record.closure_version = CLOSURE_VERSION_SENTINEL.to_string();
            record.toolchain_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.lake_manifest_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.preamble_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.approved_axioms_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.active_decl_hash = CLOSURE_HASH_SENTINEL.to_string();
            record.active_statement_hash = CLOSURE_HASH_SENTINEL.to_string();
            persist_record_to_disk(&records_dir, &record, 1).expect("persist sentinel");
            let on_disk = records_dir.join(trellis_kernel::runtime::persisted_record_file_name(
                &NodeId::from("Foo"),
            ));
            assert!(on_disk.exists(), "precondition: sentinel JSON exists");

            // Corrupt approved-axioms so backfill fails.
            fs::write(repo.join("APPROVED_AXIOMS.json"), "{ not valid json")
                .expect("write corrupt approved");

            let mut state = ProtocolState::default();
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record);

            let outcome = backfill_local_closure_record_hashes(&mut state, repo, 42);
            assert!(outcome.mutated, "demote mutates state");
            assert_eq!(outcome.demoted_nodes, vec![NodeId::from("Foo")]);

            // Mirror the cleanup the production caller performs in
            // `step_runtime` after `try_post_load_state_migration`.
            for node in &outcome.demoted_nodes {
                let path =
                    records_dir.join(trellis_kernel::runtime::persisted_record_file_name(node));
                let _ = fs::remove_file(&path);
            }
            assert!(
                !on_disk.exists(),
                "demote must result in the persisted JSON being unlinked"
            );
        }

        #[test]
        fn deterministic_revalidation_refreshes_passing_node() {
            // Plan §7.5 / test 3 — a node that now probes "ok" is moved
            // from unverified to refreshed.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            let batch =
                deterministic_revalidate_at_cli_with_probe(&state, repo, 10, |_repo, node| {
                    Ok(ok_probe(&NodeId::from(node)))
                });
            assert_eq!(batch.refreshed.len(), 1, "Foo must be refreshed");
            assert_eq!(batch.still_unverified.len(), 0);
            let (refreshed_node, refreshed_record) = &batch.refreshed[0];
            assert_eq!(refreshed_node.as_str(), "Foo");
            assert_eq!(refreshed_record.closure_version, CLOSURE_VERSION);
        }

        #[test]
        fn deterministic_revalidation_rejects_probe_with_unmappable_boundary_dep() {
            // Patch C-Q Q1 — `deterministic_revalidate_at_cli_with_probe`
            // must call `validate_probe_present_nodes` before record
            // construction. A probe whose `boundary_theorems` map
            // contains a dep absent from `live.present_nodes` is a
            // dep-name drift; the worker-side gate already rejects this
            // (Patch C-K), and the deterministic path must do the same.
            // The validator flips status to `internal_error`, which
            // forces the still_unverified arm rather than installing
            // a refreshed record with an unmappable dep key.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Foo"));
            // Note: "Ghost" is NOT in present_nodes; the probe reports
            // it as a boundary_theorems dep.
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            let batch =
                deterministic_revalidate_at_cli_with_probe(&state, repo, 10, |_repo, node| {
                    let mut probe = ok_probe(&NodeId::from(node));
                    probe
                        .boundary_theorems
                        .insert(NodeId::from("Ghost"), "h-ghost".to_string());
                    Ok(probe)
                });
            assert!(
                batch.refreshed.is_empty(),
                "probe with unmappable boundary dep must NOT refresh a record"
            );
            assert_eq!(
                batch.still_unverified.len(),
                1,
                "validator flip routes the entry to still_unverified",
            );
            let (still_node, summary) = &batch.still_unverified[0];
            assert_eq!(still_node.as_str(), "Foo");
            assert_eq!(
                summary.status, "internal_error",
                "validator must flip status to internal_error",
            );
            assert!(
                summary.strict_errors.iter().any(|e| e.contains(
                    "local-closure probe contains dep names not in kernel present_nodes"
                ) || e.contains("Ghost")),
                "stderr_excerpt/strict_errors must surface the unmappable dep; got {:?}",
                summary.strict_errors,
            );
        }

        #[test]
        fn deterministic_revalidation_rejects_probe_with_kind_confused_dep() {
            // Patch C-Q Q1 — additionally validate dep kinds.
            // `boundary_theorems` and `strict_theorem_deps` must point
            // at Proof-kind nodes; `strict_definition_deps` at
            // Definition-kind nodes. A probe that places a Definition
            // node under `boundary_theorems` is kind-confused; the
            // worker-side gate rejects this (Patch C-N), and the
            // deterministic path must too.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.live.present_nodes.insert(NodeId::from("HelperDef"));
            state
                .node_kinds
                .insert(NodeId::from("Foo"), trellis_kernel::NodeKind::Proof);
            state.node_kinds.insert(
                NodeId::from("HelperDef"),
                trellis_kernel::NodeKind::Definition,
            );
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            let batch =
                deterministic_revalidate_at_cli_with_probe(&state, repo, 10, |_repo, node| {
                    let mut probe = ok_probe(&NodeId::from(node));
                    // HelperDef is Definition-kind but placed under
                    // boundary_theorems (which expects Proof-kind).
                    probe
                        .boundary_theorems
                        .insert(NodeId::from("HelperDef"), "h-def".to_string());
                    Ok(probe)
                });
            assert!(
                batch.refreshed.is_empty(),
                "probe with kind-confused dep must NOT refresh a record"
            );
            assert_eq!(
                batch.still_unverified.len(),
                1,
                "validator flip routes the entry to still_unverified",
            );
            let (_node, summary) = &batch.still_unverified[0];
            assert_eq!(summary.status, "internal_error");
            assert!(
                summary
                    .strict_errors
                    .iter()
                    .any(|e| e.contains("kind does not match") || e.contains("HelperDef")),
                "stderr_excerpt/strict_errors must surface the kind mismatch; got {:?}",
                summary.strict_errors,
            );
        }

        #[test]
        fn deterministic_revalidation_keeps_failing_node_unverified() {
            // Plan §7.5 / test 4 — a probe failure produces an
            // ErrorSummary, node stays unverified.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            let batch =
                deterministic_revalidate_at_cli_with_probe(&state, repo, 10, |_repo, _node| {
                    Ok(fail_probe())
                });
            assert_eq!(batch.refreshed.len(), 0);
            assert_eq!(batch.still_unverified.len(), 1);
            let (still_node, summary) = &batch.still_unverified[0];
            assert_eq!(still_node.as_str(), "Foo");
            assert_eq!(summary.status, "axiom_violation");
            assert!(
                summary.axiom_violations.iter().any(|a| a == "UnapprovedAx"),
                "axiom_violations must include the unapproved axiom"
            );
        }

        #[test]
        fn transport_error_backoff_skips_before_next_retry_cycle() {
            // Plan §7.0 / test 5 — when prior failure status is
            // transport_error and current_cycle < next_retry_cycle, the
            // pass skips the node entirely (no probe call).
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            let mut prior = ErrorSummary::default();
            prior.status = "transport_error".to_string();
            prior.retry_count = 1;
            prior.next_retry_cycle = 100;
            state
                .local_closure_failures
                .insert(NodeId::from("Foo"), prior);

            let mut calls = 0usize;
            let batch =
                deterministic_revalidate_at_cli_with_probe(&state, repo, 50, |_repo, _node| {
                    calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                });
            assert_eq!(calls, 0, "probe must not run while in backoff window");
            assert_eq!(batch.refreshed.len(), 0);
            assert_eq!(batch.still_unverified.len(), 0);
        }

        #[test]
        fn transport_error_retry_exhausted_skips_node() {
            // Plan §7.0 / test 6 — after retry_exhausted, the pass
            // permanently skips the node until operator intervention.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            let mut prior = ErrorSummary::default();
            prior.status = "transport_error".to_string();
            prior.retry_count = TRANSPORT_RETRY_BUDGET + 1;
            prior.retry_exhausted = true;
            prior.next_retry_cycle = 0;
            state
                .local_closure_failures
                .insert(NodeId::from("Foo"), prior);

            let mut calls = 0usize;
            let _batch = deterministic_revalidate_at_cli_with_probe(
                &state,
                repo,
                1_000_000,
                |_repo, _node| {
                    calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            );
            assert_eq!(calls, 0, "probe must not run when retry_exhausted=true");
        }

        #[test]
        fn transport_error_summary_increments_and_caps_backoff() {
            // Plan §7.4.1 — exponential backoff capped at
            // TRANSPORT_BACKOFF_MAX_CYCLES; retry_exhausted set when count
            // exceeds the budget.
            let summary0 = build_transport_error_summary("socket down", None, 10);
            assert_eq!(summary0.status, "transport_error");
            assert_eq!(summary0.retry_count, 0);
            assert_eq!(summary0.next_retry_cycle, 11); // 10 + 2^0
            assert!(!summary0.retry_exhausted);

            let summary1 = build_transport_error_summary("socket down", Some(&summary0), 11);
            assert_eq!(summary1.retry_count, 1);
            assert_eq!(summary1.next_retry_cycle, 13); // 11 + 2^1

            // Beyond budget — retry_exhausted=true.
            let mut prior = ErrorSummary::default();
            prior.status = "transport_error".to_string();
            prior.retry_count = TRANSPORT_RETRY_BUDGET;
            let exhausted = build_transport_error_summary("socket down", Some(&prior), 50);
            assert_eq!(exhausted.retry_count, TRANSPORT_RETRY_BUDGET + 1);
            assert!(exhausted.retry_exhausted);

            // Backoff cap.
            let mut huge_prior = ErrorSummary::default();
            huge_prior.status = "transport_error".to_string();
            huge_prior.retry_count = 60; // 2^61 would overflow without cap.
            let capped = build_transport_error_summary("socket down", Some(&huge_prior), 100);
            assert_eq!(
                capped.next_retry_cycle,
                100 + TRANSPORT_BACKOFF_MAX_CYCLES,
                "exponential backoff must be capped at TRANSPORT_BACKOFF_MAX_CYCLES"
            );
        }

        #[test]
        fn revalidation_pass_drains_full_unverified_set_in_one_call() {
            // Patch C-M — chunking removed. Every node in the unverified
            // set is probed in a single call. Operator decision:
            // deferring probe work across cycles only blocks `Cleanup`
            // longer for no benefit; the total probing work is identical
            // either way.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            const N: usize = 15;
            for i in 0..N {
                write_node(repo, &format!("Foo{i}"), "trivial");
            }
            let mut state = ProtocolState::default();
            for i in 0..N {
                state
                    .local_closure_unverified_nodes
                    .insert(NodeId::from(format!("Foo{i}").as_str()));
            }
            let mut calls = 0usize;
            let batch =
                deterministic_revalidate_at_cli_with_probe(&state, repo, 10, |_repo, node| {
                    calls += 1;
                    Ok(ok_probe(&NodeId::from(node)))
                });
            assert_eq!(calls, N, "every node in the unverified set must be probed");
            assert_eq!(
                batch.refreshed.len(),
                N,
                "every passing probe must enter `refreshed`"
            );
        }

        #[test]
        fn pre_step_revalidation_drains_all_unverified_in_single_call() {
            // Patch C-M — a 50+ node unverified set is fully drained in a
            // single call to the pre-step hook. All nodes whose probe
            // returns cleanly exit the unverified set via
            // `apply_revalidation_batch` and land in
            // `local_closure_records`. None remain naked-unverified after
            // the call.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            const N: usize = 50;
            for i in 0..N {
                write_node(repo, &format!("Foo{i}"), "trivial");
            }
            let mut state = ProtocolState::default();
            for i in 0..N {
                let node = NodeId::from(format!("Foo{i}").as_str());
                // Seed live/proof/present so `apply_revalidation_batch`
                // installs the refreshed records (filters drop entries
                // for absent or non-proof nodes).
                state.live.present_nodes.insert(node.clone());
                state.proof_nodes.insert(node.clone());
                state.local_closure_unverified_nodes.insert(node);
            }
            state.cycle = 5;
            let runtime_root = tempdir().expect("rt");
            let mut probe_calls = 0usize;
            let batch = run_pre_step_revalidation_if_needed_pure(
                &mut state,
                repo,
                runtime_root.path(),
                5,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            );
            let batch = batch.expect("pre-step hook must fire when set non-empty");
            assert_eq!(
                probe_calls, N,
                "all {N} nodes must be probed in a single call"
            );
            assert_eq!(
                batch.refreshed.len(),
                N,
                "all passing probes must enter `refreshed`"
            );
            assert!(
                state.local_closure_unverified_nodes.is_empty(),
                "no node may remain naked-unverified after the drain; got {:?}",
                state.local_closure_unverified_nodes
            );
            assert_eq!(
                state.local_closure_records.len(),
                N,
                "every refreshed record must be installed in `local_closure_records`"
            );
        }

        #[test]
        fn pre_step_revalidation_transport_errors_keep_node_unverified_but_not_naked() {
            // Patch C-M — a node whose probe stub returns a transport
            // error after the chunking cap is removed stays in
            // `local_closure_unverified_nodes` BUT also has a
            // `transport_error` failure entry, so it is "failed-unverified"
            // (skipped by the auto-scheduler per C-F's logic) rather than
            // "naked-unverified". The drain loop must not infinite-loop
            // even when no probe succeeds; one call is one full pass over
            // the set.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            let node = NodeId::from("Foo");
            state.live.present_nodes.insert(node.clone());
            state.proof_nodes.insert(node.clone());
            state.local_closure_unverified_nodes.insert(node.clone());
            state.cycle = 5;
            let runtime_root = tempdir().expect("rt");
            let mut probe_calls = 0usize;
            let batch = run_pre_step_revalidation_if_needed_pure(
                &mut state,
                repo,
                runtime_root.path(),
                5,
                |_repo, _node| -> Result<LocalClosureProbeOutput, String> {
                    probe_calls += 1;
                    Err("socket closed".to_string())
                },
            );
            let batch = batch.expect("pre-step hook must fire");
            assert_eq!(probe_calls, 1, "transport error path probes once");
            assert_eq!(
                batch.still_unverified.len(),
                1,
                "transport error pushes to `still_unverified`"
            );
            assert!(
                state.local_closure_unverified_nodes.contains(&node),
                "transport error keeps node in the unverified set"
            );
            let summary = state
                .local_closure_failures
                .get(&node)
                .expect("transport error must install a failure entry — not naked");
            assert_eq!(
                summary.status, "transport_error",
                "failure entry must carry transport_error status"
            );
        }

        #[test]
        fn failure_summary_segregates_unapproved_axioms() {
            // build_failure_summary populates axiom_violations only with
            // axioms NOT in the approved set, so the diagnostic only
            // surfaces the actual offenders.
            let probe = LocalClosureProbeOutput {
                status: "axiom_violation".to_string(),
                kernel_axioms: BTreeSet::from([
                    "Classical.choice".to_string(),
                    "UnapprovedAx".to_string(),
                ]),
                ..LocalClosureProbeOutput::default()
            };
            let approved = BTreeSet::from(["Classical.choice".to_string()]);
            let summary = build_failure_summary(&probe, &approved, 7);
            assert_eq!(summary.axiom_violations, vec!["UnapprovedAx".to_string()]);
            assert_eq!(summary.captured_at_cycle, 7);
            assert!(!summary.retry_exhausted);
        }

        #[test]
        fn persisted_record_roundtrip() {
            // Plan §7.10 / test 9 — persist + load is identity, with the
            // `_persisted_at_cycle` diagnostic stripped on load so it
            // doesn't pollute hash recomputation.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap-rt".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            let runtime_root = tempdir().expect("runtime root");
            let records_dir = local_closure_records_dir(runtime_root.path());
            persist_record_to_disk(&records_dir, &record, 42).expect("persist");
            let path = records_dir.join("Foo.json");
            assert!(path.exists());
            let loaded = load_persisted_record(&path).expect("load");
            assert_eq!(loaded, record);
        }

        #[test]
        fn migration_loads_record_with_matching_hashes() {
            // Plan §7.10 / test 1 — persisted records that match current
            // hashes get installed into state.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let runtime_root = tempdir().expect("runtime root");
            let records_dir = local_closure_records_dir(runtime_root.path());
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap-m".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            persist_record_to_disk(&records_dir, &record, 1).expect("persist");

            let mut state = ProtocolState::default();
            // Foo is a sorry-free proof_node lacking a record — without
            // disk-load, it would enter unverified.
            state.proof_nodes.insert(NodeId::from("Foo"));
            // Plus the live snapshot says Foo is closed (sorry-free).
            // (open_nodes empty by default.)
            let mut probe_calls = 0usize;
            let _ = run_migration_if_needed_with_probe(
                &mut state,
                repo,
                runtime_root.path(),
                100,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            )
            .expect("migration");
            assert!(state
                .local_closure_records
                .contains_key(&NodeId::from("Foo")));
            assert!(!state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Foo")));
            assert_eq!(
                probe_calls, 0,
                "matching persisted record must skip the probe"
            );
        }

        #[test]
        fn migration_discards_record_with_mismatched_hashes() {
            // Plan §7.10 / test 1 (negative half) — persisted record
            // with stale hashes is discarded; node stays in unverified.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let runtime_root = tempdir().expect("runtime root");
            let records_dir = local_closure_records_dir(runtime_root.path());
            // Persist a record with a wrong toolchain hash (forced).
            let mut record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap-stale".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            record.toolchain_hash = hash_text("WRONG");
            persist_record_to_disk(&records_dir, &record, 1).expect("persist");

            let mut state = ProtocolState::default();
            state.proof_nodes.insert(NodeId::from("Foo"));
            // Patch C-Q Q4: needs_probe filter requires present_nodes
            // membership; without this seed, the stale-record path
            // wouldn't trigger the re-probe.
            state.live.present_nodes.insert(NodeId::from("Foo"));
            let mut probe_calls = 0usize;
            let _ = run_migration_if_needed_with_probe(
                &mut state,
                repo,
                runtime_root.path(),
                100,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(fail_probe())
                },
            )
            .expect("migration");
            assert!(!state
                .local_closure_records
                .contains_key(&NodeId::from("Foo")));
            assert!(state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Foo")));
            assert_eq!(probe_calls, 1, "stale record forces re-probe");
        }

        #[test]
        fn migration_does_not_install_record_for_node_in_unverified_set() {
            // Patch C-O HIGH 1 (a) — when a node is currently in
            // `local_closure_unverified_nodes`, that membership is an
            // explicit in-memory tombstone saying "the prior record was
            // invalidated and must be re-probed." A stale persisted
            // JSON file MUST NOT override the tombstone; migration must
            // skip the disk record and force a probe.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let runtime_root = tempdir().expect("runtime root");
            let records_dir = local_closure_records_dir(runtime_root.path());
            // The persisted record has matching hashes — pre-fix it
            // would be installed.
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap-tombstoned".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            persist_record_to_disk(&records_dir, &record, 1).expect("persist");

            let mut state = ProtocolState::default();
            state.proof_nodes.insert(NodeId::from("Foo"));
            // The tombstone: Foo is invalidated and awaiting re-probe.
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));

            let mut probe_calls = 0usize;
            let _ = run_migration_if_needed_with_probe(
                &mut state,
                repo,
                runtime_root.path(),
                100,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(fail_probe())
                },
            )
            .expect("migration");

            assert!(
                !state
                    .local_closure_records
                    .contains_key(&NodeId::from("Foo")),
                "tombstoned node must NOT receive a record from disk"
            );
            assert!(
                state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Foo")),
                "tombstone must persist (migration may add a failure summary, but the unverified entry stays)"
            );
            assert_eq!(
                probe_calls, 1,
                "tombstone forces a re-probe rather than disk install"
            );
        }

        #[test]
        fn migration_does_not_install_record_for_node_with_failure_entry() {
            // Patch C-O HIGH 1 (a) — same as the unverified-set check
            // but for `local_closure_failures`. Either tombstone (or
            // both) signals "do not install a disk record."
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let runtime_root = tempdir().expect("runtime root");
            let records_dir = local_closure_records_dir(runtime_root.path());
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap-failure".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            persist_record_to_disk(&records_dir, &record, 1).expect("persist");

            let mut state = ProtocolState::default();
            state.proof_nodes.insert(NodeId::from("Foo"));
            // The tombstone: Foo has a recorded failure summary.
            let mut summary = ErrorSummary::default();
            summary.status = "axiom_violation".to_string();
            state
                .local_closure_failures
                .insert(NodeId::from("Foo"), summary);

            // Note: a node can be in failures without being in unverified
            // if it was previously closed but the engine marked it bad.
            // Either signal must block disk-install.
            let mut probe_calls = 0usize;
            let _ = run_migration_if_needed_with_probe(
                &mut state,
                repo,
                runtime_root.path(),
                100,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(fail_probe())
                },
            )
            .expect("migration");

            assert!(
                !state
                    .local_closure_records
                    .contains_key(&NodeId::from("Foo")),
                "node with a failure tombstone must NOT receive a record from disk"
            );
        }

        #[test]
        fn migration_demotes_sentinel_record_in_memory_to_unverified() {
            // Audit NR-1 — sentinel record persistence window. Scenario:
            //   1. Engine installs a sentinel-hashed record via
            //      `apply_local_closure_acceptance_bookkeeping` at the
            //      sorry-free arm.
            //   2. `step_with_checkpoint_sink` persists state.json with
            //      the sentinel record.
            //   3. Process dies BEFORE `step_runtime`'s post-step
            //      backfill replaces the sentinel with real hashes.
            //   4. On restart, state.local_closure_records.contains_key
            //      is true → migration's record-load loop skips the
            //      disk reload AND `needs_probe` filter excludes the
            //      node.
            //   5. `formalization_complete()` sees a present-but-
            //      sentinel record and may incorrectly bless phase
            //      advancement.
            //
            // Fix: at migration entry, sweep `state.local_closure_records`
            // and demote sentinel records to `local_closure_unverified_nodes`
            // so deterministic revalidation re-probes them.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let runtime_root = tempdir().expect("runtime root");

            // Simulate the state file that survived a crash between
            // engine acceptance (sentinel install) and backfill: an
            // in-memory record carrying sentinel hash placeholders.
            let mut sentinel_record = LocalClosureRecord::default();
            sentinel_record.node = NodeId::from("Foo");
            sentinel_record.closure_version = CLOSURE_VERSION_SENTINEL.to_string();
            sentinel_record.toolchain_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.lake_manifest_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.preamble_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.approved_axioms_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.active_decl_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.active_statement_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.accepted_at_snapshot_id = "snap-pre-crash".to_string();

            let mut state = ProtocolState::default();
            state.proof_nodes.insert(NodeId::from("Foo"));
            state.live.present_nodes.insert(NodeId::from("Foo"));
            // Inject the sentinel record (simulating a state.json that
            // was persisted with engine-emitted sentinel hashes before
            // backfill ran).
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), sentinel_record);

            // Pre-condition: record exists, no unverified entry, no
            // failure. Without the fix, migration would skip Foo
            // entirely — needs_probe filter excludes nodes with records.
            assert!(state
                .local_closure_records
                .contains_key(&NodeId::from("Foo")));
            assert!(!state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Foo")));

            let mut probe_calls = 0usize;
            let _ = run_migration_if_needed_with_probe(
                &mut state,
                repo,
                runtime_root.path(),
                100,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            )
            .expect("migration");

            // Post-condition: deterministic revalidation ran (because
            // the sentinel sweep demoted Foo to unverified, the
            // revalidation pass then probed it). The re-probe produced
            // an `ok` result so Foo now has a real-hashed record.
            assert_eq!(
                probe_calls, 1,
                "sentinel record must trigger re-probe via deterministic revalidation"
            );
            assert!(
                state
                    .local_closure_records
                    .contains_key(&NodeId::from("Foo")),
                "successful re-probe must install a real-hashed record"
            );
            let new_record = state
                .local_closure_records
                .get(&NodeId::from("Foo"))
                .unwrap();
            assert!(
                !record_needs_hash_backfill(new_record),
                "post-migration record must NOT carry sentinel hashes; got: closure_version={:?}, \
                 toolchain_hash={:?}",
                new_record.closure_version,
                new_record.toolchain_hash
            );
            // The unverified entry was cleared by the successful
            // revalidation.
            assert!(
                !state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Foo")),
                "unverified entry must be cleared after successful re-probe"
            );
        }

        #[test]
        fn migration_rejects_sentinel_record_on_disk_load() {
            // Audit NR-1 — belt-and-braces: a persisted disk record
            // carrying sentinel hashes (rare but possible in synthetic
            // tests or future refactors that persist records earlier)
            // must be rejected at disk-load time. The migration
            // installs the node in `local_closure_unverified_nodes`
            // and lets deterministic revalidation re-probe.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let runtime_root = tempdir().expect("runtime root");
            let records_dir = local_closure_records_dir(runtime_root.path());

            // Hand-write a sentinel-shaped persisted record.
            let mut sentinel_record = LocalClosureRecord::default();
            sentinel_record.node = NodeId::from("Foo");
            sentinel_record.closure_version = CLOSURE_VERSION_SENTINEL.to_string();
            sentinel_record.toolchain_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.lake_manifest_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.preamble_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.approved_axioms_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.active_decl_hash = CLOSURE_HASH_SENTINEL.to_string();
            sentinel_record.active_statement_hash = CLOSURE_HASH_SENTINEL.to_string();
            persist_record_to_disk(&records_dir, &sentinel_record, 1).expect("persist sentinel");

            let mut state = ProtocolState::default();
            state.proof_nodes.insert(NodeId::from("Foo"));
            state.live.present_nodes.insert(NodeId::from("Foo"));
            // Pre-condition: no in-memory record yet (only disk).
            assert!(!state
                .local_closure_records
                .contains_key(&NodeId::from("Foo")));

            let mut probe_calls = 0usize;
            let _ = run_migration_if_needed_with_probe(
                &mut state,
                repo,
                runtime_root.path(),
                100,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            )
            .expect("migration");

            // Post-condition: re-probe happened (instead of disk load
            // installing the sentinel).
            assert_eq!(
                probe_calls, 1,
                "disk sentinel record must trigger re-probe instead of install"
            );
            assert!(
                state
                    .local_closure_records
                    .contains_key(&NodeId::from("Foo")),
                "re-probe must install a real-hashed record"
            );
            let new_record = state
                .local_closure_records
                .get(&NodeId::from("Foo"))
                .unwrap();
            assert!(
                !record_needs_hash_backfill(new_record),
                "post-migration record must NOT carry sentinel hashes"
            );
        }

        #[test]
        fn record_hashes_match_current_rejects_record_with_changed_boundary_dep_fingerprint_no_other_consumer(
        ) {
            // Patch C-P HIGH 1 (b) — single-consumer staleness via
            // kernel `semantic_hash` mismatch. The record's
            // `kernel_semantic_hashes` map carries the dep's hash as it
            // was at probe time; current `corr_current_fingerprints`
            // disagrees → reject. Replaces C-O's strict-signal trigger
            // (dep in unverified set) with the canonical drift check.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");

            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Helper"));
            // Current kernel hash for Helper is the post-edit value.
            state
                .live
                .corr_current_fingerprints
                .insert(NodeId::from("Helper"), "kernel-helper-new".to_string());

            let mut foo_boundary = BTreeMap::new();
            foo_boundary.insert(NodeId::from("Helper"), "any-statement-hash".to_string());
            let mut foo_record =
                record_for_dep_test(repo, "Foo", foo_boundary, BTreeMap::new(), BTreeMap::new());
            // The record was written when Helper's kernel hash was the
            // OLD value — stale relative to current state.
            foo_record
                .kernel_semantic_hashes
                .insert(NodeId::from("Helper"), "kernel-helper-old".to_string());

            assert!(
                !record_hashes_match_current(&foo_record, repo, &state),
                "single-consumer record whose boundary dep's kernel hash drifted must reject"
            );
        }

        #[test]
        fn record_hashes_match_current_rejects_record_with_changed_strict_theorem_dep_fingerprint_no_other_consumer(
        ) {
            // Patch C-P HIGH 1 (b) — single-consumer staleness via
            // kernel `semantic_hash` mismatch for `strict_theorem_deps`.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");

            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("ThmT"));
            state
                .live
                .corr_current_fingerprints
                .insert(NodeId::from("ThmT"), "kernel-thmt-new".to_string());

            let mut foo_strict_thm = BTreeMap::new();
            foo_strict_thm.insert(NodeId::from("ThmT"), "any-val".to_string());
            let mut foo_record = record_for_dep_test(
                repo,
                "Foo",
                BTreeMap::new(),
                foo_strict_thm,
                BTreeMap::new(),
            );
            foo_record
                .kernel_semantic_hashes
                .insert(NodeId::from("ThmT"), "kernel-thmt-old".to_string());

            assert!(
                !record_hashes_match_current(&foo_record, repo, &state),
                "single-consumer record whose strict_theorem_deps dep's kernel hash drifted must reject"
            );
        }

        #[test]
        fn record_hashes_match_current_rejects_record_with_changed_strict_definition_dep_fingerprint_no_other_consumer(
        ) {
            // Patch C-P HIGH 1 (b) — single-consumer staleness via
            // kernel `semantic_hash` mismatch for `strict_definition_deps`.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");

            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("DefD"));
            state
                .live
                .corr_current_fingerprints
                .insert(NodeId::from("DefD"), "kernel-defd-new".to_string());

            let mut foo_strict_def = BTreeMap::new();
            foo_strict_def.insert(NodeId::from("DefD"), "any-sem".to_string());
            let mut foo_record = record_for_dep_test(
                repo,
                "Foo",
                BTreeMap::new(),
                BTreeMap::new(),
                foo_strict_def,
            );
            foo_record
                .kernel_semantic_hashes
                .insert(NodeId::from("DefD"), "kernel-defd-old".to_string());

            assert!(
                !record_hashes_match_current(&foo_record, repo, &state),
                "single-consumer record whose strict_definition_deps dep's kernel hash drifted must reject"
            );
        }

        #[test]
        fn record_hashes_match_current_rejects_record_with_silently_changed_dep_hash_no_flag() {
            // Patch C-P HIGH 1 (b) — the case C-O's strict-signal
            // approach could NOT catch. A dep's content silently
            // drifted (e.g. an off-protocol edit between supervisor
            // stops; or the engine's invalidation propagation has a
            // bug). The dep is NOT in unverified/failures (no
            // tombstone). The record's recorded `kernel_semantic_hash`
            // disagrees with current state → reject.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");

            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Helper"));
            // Current kernel hash for Helper has drifted, but no
            // tombstone (Helper is NOT in unverified or failures).
            state
                .live
                .corr_current_fingerprints
                .insert(NodeId::from("Helper"), "kernel-helper-drifted".to_string());
            assert!(!state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Helper")));
            assert!(!state
                .local_closure_failures
                .contains_key(&NodeId::from("Helper")));

            let mut foo_boundary = BTreeMap::new();
            foo_boundary.insert(NodeId::from("Helper"), "any-stmt".to_string());
            let mut foo_record =
                record_for_dep_test(repo, "Foo", foo_boundary, BTreeMap::new(), BTreeMap::new());
            foo_record
                .kernel_semantic_hashes
                .insert(NodeId::from("Helper"), "kernel-helper-original".to_string());

            assert!(
                !record_hashes_match_current(&foo_record, repo, &state),
                "silent dep drift (no tombstone) must still reject — kernel hash is authoritative"
            );
        }

        #[test]
        fn record_hashes_match_current_rejects_two_stale_records_mutually_referencing_each_other() {
            // Patch C-P HIGH 1 (b) — mutual-stale scenario that C-O's
            // strict signals + cross-record evidence couldn't catch.
            // Two persisted records A and H reference each other; both
            // are stale (their recorded kernel hashes don't match
            // current). Under C-O, because neither dep is in unverified
            // and cross-record evidence comes from the OTHER stale
            // record, they would mutually validate. Under C-P, the
            // kernel hash check rejects each independently.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "A", "trivial");
            write_node(repo, "H", "trivial");

            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("A"));
            state.live.present_nodes.insert(NodeId::from("H"));
            // Current kernel hashes — both deps drifted from their
            // record-write-time values.
            state
                .live
                .corr_current_fingerprints
                .insert(NodeId::from("A"), "kernel-A-new".to_string());
            state
                .live
                .corr_current_fingerprints
                .insert(NodeId::from("H"), "kernel-H-new".to_string());

            // A's record names H as a boundary dep with a stale stmt-hash;
            // A's kernel_semantic_hashes records H's OLD kernel hash.
            let mut a_boundary = BTreeMap::new();
            a_boundary.insert(NodeId::from("H"), "stale-stmt-hash".to_string());
            let mut a_record =
                record_for_dep_test(repo, "A", a_boundary, BTreeMap::new(), BTreeMap::new());
            a_record
                .kernel_semantic_hashes
                .insert(NodeId::from("H"), "kernel-H-old".to_string());

            // H's record names A as a boundary dep with a stale stmt-hash;
            // H's kernel_semantic_hashes records A's OLD kernel hash.
            let mut h_boundary = BTreeMap::new();
            h_boundary.insert(NodeId::from("A"), "stale-stmt-hash".to_string());
            let mut h_record =
                record_for_dep_test(repo, "H", h_boundary, BTreeMap::new(), BTreeMap::new());
            h_record
                .kernel_semantic_hashes
                .insert(NodeId::from("A"), "kernel-A-old".to_string());

            // Put both records into state (mimicking a partial-restart
            // where one was installed and the migration is about to
            // consider the other). Under C-O, mutual cross-record
            // agreement (stale-stmt-hash matches on both sides) would
            // not surface a disagreement; only kernel-hash drift catches
            // this.
            state
                .local_closure_records
                .insert(NodeId::from("A"), a_record.clone());

            assert!(
                !record_hashes_match_current(&h_record, repo, &state),
                "H's record must reject: its recorded kernel hash for A drifted"
            );
            // And vice versa.
            state.local_closure_records.remove(&NodeId::from("A"));
            state
                .local_closure_records
                .insert(NodeId::from("H"), h_record);
            assert!(
                !record_hashes_match_current(&a_record, repo, &state),
                "A's record must reject: its recorded kernel hash for H drifted"
            );
        }

        #[test]
        fn record_hashes_match_current_passes_when_all_dep_hashes_match_current() {
            // Patch C-P HIGH 1 (b) — positive control. With kernel
            // hashes recorded and matching current state, the record
            // installs cleanly.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");

            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Helper"));
            state.live.present_nodes.insert(NodeId::from("ThmT"));
            state.live.present_nodes.insert(NodeId::from("DefD"));
            state
                .live
                .corr_current_fingerprints
                .insert(NodeId::from("Helper"), "k-helper".to_string());
            state
                .live
                .corr_current_fingerprints
                .insert(NodeId::from("ThmT"), "k-thmt".to_string());
            state
                .live
                .corr_current_fingerprints
                .insert(NodeId::from("DefD"), "k-defd".to_string());

            let mut foo_boundary = BTreeMap::new();
            foo_boundary.insert(NodeId::from("Helper"), "stmt-helper".to_string());
            let mut foo_strict_thm = BTreeMap::new();
            foo_strict_thm.insert(NodeId::from("ThmT"), "val-thmt".to_string());
            let mut foo_strict_def = BTreeMap::new();
            foo_strict_def.insert(NodeId::from("DefD"), "sem-defd".to_string());
            let mut foo_record =
                record_for_dep_test(repo, "Foo", foo_boundary, foo_strict_thm, foo_strict_def);
            foo_record
                .kernel_semantic_hashes
                .insert(NodeId::from("Helper"), "k-helper".to_string());
            foo_record
                .kernel_semantic_hashes
                .insert(NodeId::from("ThmT"), "k-thmt".to_string());
            foo_record
                .kernel_semantic_hashes
                .insert(NodeId::from("DefD"), "k-defd".to_string());

            assert!(
                record_hashes_match_current(&foo_record, repo, &state),
                "record with all dep kernel hashes matching current must accept"
            );
        }

        #[test]
        fn record_hashes_match_current_rejects_record_with_dep_missing_from_current_fingerprints() {
            // Patch C-P HIGH 1 (b) — a recorded `kernel_semantic_hash`
            // for a dep whose `corr_current_fingerprints` entry has
            // been deleted (dep removed from kernel state since the
            // record was written) must reject. Note: this is distinct
            // from the present_nodes check at the top of the function
            // (the dep can still be in present_nodes but have its
            // fingerprint pruned, e.g. during a corr-invalidation pass).
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");

            let mut state = ProtocolState::default();
            state.live.present_nodes.insert(NodeId::from("Helper"));
            // No entry in corr_current_fingerprints — Helper's
            // fingerprint was removed (e.g. live state lost it during
            // a partial restore).
            assert!(!state
                .live
                .corr_current_fingerprints
                .contains_key(&NodeId::from("Helper")));

            let mut foo_boundary = BTreeMap::new();
            foo_boundary.insert(NodeId::from("Helper"), "stmt".to_string());
            let mut foo_record =
                record_for_dep_test(repo, "Foo", foo_boundary, BTreeMap::new(), BTreeMap::new());
            foo_record
                .kernel_semantic_hashes
                .insert(NodeId::from("Helper"), "k-helper-original".to_string());

            assert!(
                !record_hashes_match_current(&foo_record, repo, &state),
                "record whose dep is missing from current fingerprints must reject"
            );
        }

        #[test]
        fn migration_enters_sorry_free_proof_nodes_into_unverified_set() {
            // Plan §7.10 / test 2 — sorry-free proof_nodes lacking a
            // record enter unverified; sorryd ones do not.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Free", "trivial");
            write_node(repo, "Sorryd", "sorry");
            let runtime_root = tempdir().expect("runtime root");

            let mut state = ProtocolState::default();
            state.proof_nodes.insert(NodeId::from("Free"));
            state.proof_nodes.insert(NodeId::from("Sorryd"));
            state.live.present_nodes.insert(NodeId::from("Free"));
            state.live.present_nodes.insert(NodeId::from("Sorryd"));
            state.live.open_nodes.insert(NodeId::from("Sorryd"));
            let _ = run_migration_if_needed_with_probe(
                &mut state,
                repo,
                runtime_root.path(),
                10,
                // Make the probe fail so Free stays unverified.
                |_repo, _node| Ok(fail_probe()),
            )
            .expect("migration");
            assert!(state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Free")));
            assert!(!state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Sorryd")));
        }

        #[test]
        fn migration_skips_non_present_node() {
            // Patch C-Q Q4 — `needs_probe` filter must require
            // `live.present_nodes` membership. A node listed in
            // `proof_nodes` but absent from `present_nodes` (e.g. a
            // node that's been removed from the live tablet but not
            // pruned from `proof_nodes`) used to slip through and land
            // in `local_closure_unverified_nodes`, even though
            // `apply_revalidation_batch` would later drop the batch
            // entry. The unverified-set insert violates the §7.0
            // invariant `unverified ⊆ present_nodes`; the C-Q filter
            // adds a `present_nodes` membership check to prevent it.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Present", "trivial");
            // No write_node for "Absent" — it's in proof_nodes but not
            // on disk and not in present_nodes.
            let runtime_root = tempdir().expect("runtime root");

            let mut state = ProtocolState::default();
            state.proof_nodes.insert(NodeId::from("Present"));
            state.proof_nodes.insert(NodeId::from("Absent"));
            state.live.present_nodes.insert(NodeId::from("Present"));
            // Deliberately omit "Absent" from `live.present_nodes`.
            let _ = run_migration_if_needed_with_probe(
                &mut state,
                repo,
                runtime_root.path(),
                10,
                |_repo, _node| Ok(fail_probe()),
            )
            .expect("migration");
            assert!(
                state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Present")),
                "present sorry-free proof node enters unverified set",
            );
            assert!(
                !state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Absent")),
                "absent proof node must NOT enter unverified set — \
                 violates §7.0 invariant `unverified ⊆ present_nodes`",
            );
        }

        #[test]
        fn record_hashes_match_current_detects_drift() {
            // The install-time guard (plan §7.10 step 3) compares every
            // hash field; any drift returns false.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap-match".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs");
            let empty_state = ProtocolState::default();
            assert!(record_hashes_match_current(&record, repo, &empty_state));
            // Mutate the active-decl file.
            write_node(repo, "Foo", "by trivial");
            assert!(!record_hashes_match_current(&record, repo, &empty_state));
        }

        struct StubAdapter {
            responses: Vec<WrapperResponse>,
        }
        impl WrapperAdapter for StubAdapter {
            fn dispatch(&mut self, _request: &WrapperRequest) -> Result<WrapperResponse, String> {
                Ok(self.responses.remove(0))
            }
        }

        #[test]
        fn cleanup_revalidation_adapter_injects_batch_into_worker_response() {
            // Plan §7.7 — the cleanup adapter wrapper must populate the
            // outgoing `WorkerResponse.local_closure_revalidation` so the
            // engine's `formalization_complete` check sees the refreshed
            // records before deciding whether to accept/reject the burst.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.cycle = 9;
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));

            let mut worker_response = trellis_kernel::WorkerResponse::default();
            worker_response.local_closure_revalidation = None;
            let responses = vec![WrapperResponse::Worker(worker_response)];
            let inner = StubAdapter { responses };

            let _runtime_root = tempdir().expect("runtime root");
            let shared: std::rc::Rc<std::cell::RefCell<Option<RevalidationBatch>>> =
                std::rc::Rc::new(std::cell::RefCell::new(None));
            let mut wrapper = CleanupRevalidationAdapter {
                inner,
                state: &state,
                repo: repo.to_path_buf(),
                current_cycle: 9,
                shared_batch: shared.clone(),
            };

            // Build a synthetic cleanup-validation Worker request.
            let mut request = WrapperRequest::default();
            request.kind = RequestKind::Worker;
            request.worker_context.validation_kind = WorkerValidationKind::Cleanup;

            // Because we can't easily invoke the real probe in unit tests,
            // verify the wrapper at least invokes the pass (it tries to
            // call run_local_closure_axioms; on the empty repo this will
            // produce a transport_error which still proves the wrapper
            // wired the pass — `local_closure_revalidation` becomes
            // `Some(...)`).
            let response = wrapper.dispatch(&request).expect("dispatch");
            let WrapperResponse::Worker(worker) = response else {
                panic!("expected Worker response");
            };
            assert!(
                worker.local_closure_revalidation.is_some(),
                "cleanup wrapper must populate local_closure_revalidation"
            );
            // Patch C-Q Q2 — the shared cell also gets a clone of the
            // batch (used by `step_runtime`'s post-acceptance
            // persistence sweep).
            assert!(
                shared.borrow().is_some(),
                "Q2: cleanup adapter must populate the shared batch cell",
            );
        }

        #[test]
        fn pre_review_hook_fires_when_generating_review_prompt_with_unverified_nonempty() {
            // Patch C-O HIGH 2 (was C-F's
            // pre_step_revalidation_fires_on_review_request_...): the
            // pre-review hook fires when (i) the unverified set is
            // non-empty AND (ii) no request is in flight OR the
            // in-flight request is Review. Here we exercise the Review
            // case: a Review prompt is about to be re-generated and the
            // hook must drain the unverified set first.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.in_flight_request = Some(state.expected_request(1, RequestKind::Review));
            state.cycle = 5;
            // C-G's HIGH 6 batch filter requires every revalidation-batch
            // entry's node to be present + proof-bearing + not-open.
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            let runtime_root = tempdir().expect("rt");
            let mut probe_calls = 0usize;
            let batch = run_pre_step_revalidation_if_needed_pure(
                &mut state,
                repo,
                runtime_root.path(),
                5,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            );
            let batch = batch.expect("pre-review hook must produce a batch");
            assert_eq!(batch.refreshed.len(), 1);
            assert_eq!(batch.still_unverified.len(), 0);
            assert!(state
                .local_closure_records
                .contains_key(&NodeId::from("Foo")));
            assert!(!state
                .local_closure_unverified_nodes
                .contains(&NodeId::from("Foo")));
            assert_eq!(probe_calls, 1);
        }

        #[test]
        fn pre_review_hook_does_not_fire_for_worker_or_paper_or_corr_or_sound_request_kinds() {
            // Patch C-O HIGH 2 — the pre-review hook MUST NOT fire when
            // a non-Review request is in flight: those don't consult
            // the unverified set (workers don't care, and the
            // auto-scheduler post-C-F only schedules on failure
            // records, not naked-unverified). Probing during a
            // Worker/Paper/Corr/Sound burst risks capturing unaccepted
            // disk state AND drifts the dispatched prompt's legality
            // context.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");

            for kind in [
                RequestKind::Worker,
                RequestKind::Paper,
                RequestKind::Corr,
                RequestKind::Sound,
            ] {
                let mut state = ProtocolState::default();
                state.in_flight_request = Some(state.expected_request(1, kind));
                state.cycle = 5;
                state.live.present_nodes.insert(NodeId::from("Foo"));
                state.proof_nodes.insert(NodeId::from("Foo"));
                state
                    .local_closure_unverified_nodes
                    .insert(NodeId::from("Foo"));
                let runtime_root = tempdir().expect("rt");
                let mut probe_calls = 0usize;
                let batch = run_pre_step_revalidation_if_needed_pure(
                    &mut state,
                    repo,
                    runtime_root.path(),
                    5,
                    |_repo, _node| {
                        probe_calls += 1;
                        Ok(ok_probe(&NodeId::from("Foo")))
                    },
                );
                assert!(
                    batch.is_none(),
                    "pre-review hook must NOT fire with {kind:?} in flight; got Some(batch)"
                );
                assert_eq!(probe_calls, 0, "probe must not run with {kind:?} in flight");
                // Unverified set must remain untouched.
                assert!(
                    state
                        .local_closure_unverified_nodes
                        .contains(&NodeId::from("Foo")),
                    "unverified node must remain in set when {kind:?} blocks hook"
                );
            }
        }

        #[test]
        fn worker_burst_does_not_trigger_pre_review_revalidation_during_in_flight_request() {
            // Patch C-O HIGH 2 — sanity check that the WIP-hazard never
            // materializes: with a Worker in flight (carrying potentially
            // unaccepted edits on disk), the hook does NOT probe / persist
            // records. The Worker response handler will run bookkeeping
            // when consumed, and any unverified entries either get cleared
            // there or wait for the next request-boundary Review hook.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.in_flight_request = Some(state.expected_request(7, RequestKind::Worker));
            state.cycle = 5;
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            // Add a real failure so we'd otherwise want to re-probe.
            state.local_closure_failures.insert(
                NodeId::from("Foo"),
                ErrorSummary {
                    status: "axiom_violation".to_string(),
                    ..Default::default()
                },
            );
            let runtime_root = tempdir().expect("rt");
            let records_dir = runtime_root
                .path()
                .join("checker-state/local-closure-records");
            let mut probe_calls = 0usize;
            let batch = run_pre_step_revalidation_if_needed_pure(
                &mut state,
                repo,
                runtime_root.path(),
                5,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            );
            assert!(
                batch.is_none(),
                "WIP-hazard guard must prevent the hook from firing during in-flight Worker"
            );
            assert_eq!(probe_calls, 0, "probe must not run during Worker WIP");
            // No persisted record file must have been written (we
            // shouldn't capture WIP disk state).
            assert!(
                !records_dir.exists()
                    || fs::read_dir(&records_dir).map(|d| d.count()).unwrap_or(0) == 0,
                "no record file may be persisted under WIP-hazard guard"
            );
        }

        #[test]
        fn pre_step_revalidation_no_op_when_unverified_set_empty() {
            // Patch C-F — the pre-step hook is a no-op when the unverified
            // set is empty: nothing to probe, no batch produced, no probe
            // closure invoked. This is the fast-path that avoids
            // gratuitous I/O on clean steps.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.in_flight_request = Some(state.expected_request(1, RequestKind::Worker));
            // local_closure_unverified_nodes intentionally empty.
            let runtime_root = tempdir().expect("rt");
            let mut probe_calls = 0usize;
            let batch = run_pre_step_revalidation_if_needed_pure(
                &mut state,
                repo,
                runtime_root.path(),
                5,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            );
            assert!(batch.is_none(), "empty unverified set must skip the hook");
            assert_eq!(probe_calls, 0);
        }

        #[test]
        fn naked_unverified_node_exits_set_via_revalidation_not_worker_dispatch() {
            // Patch C-F integration — a naked unverified node (in the
            // unverified set but WITH NO failure record) passes through
            // the pre-step revalidation hook and exits the set via the
            // cheap server-side probe, never reaching the auto-scheduler.
            // This is the migration-cold-start / dep-invalidated scenario
            // that the C-F fix targets: such nodes must NOT trigger
            // worker dispatch (~30-60s) when a probe (~2-15s) suffices.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            // Patch C-O HIGH 2: hook only fires when no request is in
            // flight or a Review is in flight. The original C-F test
            // used Worker in flight; this is now disallowed. The
            // naked-unverified-cleared semantic is unchanged for the
            // no-in-flight scenario, which is the canonical path.
            state.cycle = 5;
            // C-G's HIGH 6 batch filter requires every revalidation-batch
            // entry's node to be present + proof-bearing + not-open.
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            // Naked: in unverified set but NO entry in
            // local_closure_failures.
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            assert!(state.local_closure_failures.is_empty());

            let runtime_root = tempdir().expect("rt");
            let mut probe_calls = 0usize;
            let batch = run_pre_step_revalidation_if_needed_pure(
                &mut state,
                repo,
                runtime_root.path(),
                5,
                |_repo, _node| {
                    probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            );
            let _batch = batch.expect("hook fires on naked unverified too");
            // The probe succeeded; the node has exited the unverified
            // set via revalidation, NOT via worker dispatch.
            assert_eq!(probe_calls, 1);
            assert!(
                !state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Foo")),
                "naked unverified must exit set via revalidation"
            );
            assert!(
                state
                    .local_closure_records
                    .contains_key(&NodeId::from("Foo")),
                "successful probe installs a fresh record"
            );
        }

        // Patch C-O HIGH 2 — DELETED:
        //   - `pre_step_revalidation_regenerates_in_flight_request_after_batch_apply`
        //     (regeneration is no longer needed: hook only fires when
        //     in-flight is None or Review, and a Review prompt is built
        //     fresh against post-hook state by the engine, so there's
        //     nothing to silently rewrite.)
        //   - `worker_request_in_flight_gets_regenerated_just_like_review`
        //     (was asserting the unsafe behavior; the hook never fires
        //     during in-flight Worker post-C-O.)
        //
        // The `no_regenerate_when_no_in_flight_request` test is kept
        // below — under the new gate, no-in-flight is one of the two
        // permitted firing conditions.

        #[test]
        fn pre_review_hook_regenerates_in_flight_review_request_after_batch_mutates_unverified_set()
        {
            // Patch C-Q Q3 — when the in-flight request is `Review` AND
            // the batch actually refreshed/failed something, the hook
            // must regenerate `state.in_flight_request` via
            // `expected_request(prev.id, Review)`. Otherwise the
            // already-materialized Review prompt's `local_closure_unverified`
            // map references state that no longer matches the kernel
            // (the reviewer would see a stale failure-context snapshot).
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.cycle = 5;
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            // Stage 1: Foo is in unverified with a stale failure
            // entry; the original Review request carries that snapshot.
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            let mut stale_failure = ErrorSummary::default();
            stale_failure.status = "axiom_violation".to_string();
            stale_failure.stderr_excerpt = "STALE".to_string();
            state
                .local_closure_failures
                .insert(NodeId::from("Foo"), stale_failure);
            let original = state.expected_request(17, RequestKind::Review);
            state.in_flight_request = Some(original.clone());
            // Snapshot the original's `local_closure_unverified` map
            // for after-comparison.
            assert!(
                original
                    .local_closure_unverified
                    .contains_key(&NodeId::from("Foo")),
                "original Review request must carry the stale Foo entry",
            );

            // Run the hook with a probe that returns ok → Foo exits
            // unverified, fresh record installed, no failure entry.
            let runtime_root = tempdir().expect("rt");
            let batch = run_pre_step_revalidation_if_needed_pure(
                &mut state,
                repo,
                runtime_root.path(),
                5,
                |_repo, _node| Ok(ok_probe(&NodeId::from("Foo"))),
            );
            let batch = batch.expect("hook fires under Review-in-flight");
            assert!(
                !batch.refreshed.is_empty(),
                "batch must have refreshed Foo (probe ok)"
            );

            // Foo is no longer in unverified.
            assert!(
                !state
                    .local_closure_unverified_nodes
                    .contains(&NodeId::from("Foo")),
                "post-batch state must not have Foo in unverified"
            );

            // The in-flight Review request must have been regenerated:
            // its `local_closure_unverified` must be empty now (Foo
            // exited the set and its failure entry was cleared).
            let regenerated = state
                .in_flight_request
                .as_ref()
                .expect("Review must still be in flight");
            assert_eq!(
                regenerated.kind,
                RequestKind::Review,
                "regenerated request stays Review-kind"
            );
            assert_eq!(
                regenerated.id, 17,
                "regenerated request keeps the original request id"
            );
            assert!(
                regenerated.local_closure_unverified.is_empty(),
                "Q3: regenerated Review must reflect post-batch state — \
                 Foo's stale failure entry must be gone; got {:?}",
                regenerated.local_closure_unverified,
            );
        }

        #[test]
        fn pre_review_hook_does_not_regenerate_worker_or_other_request_kinds() {
            // Patch C-Q Q3 — regeneration is intentionally Review-only.
            // The gate blocks the hook entirely for Worker/Paper/Corr/Sound
            // (see `pre_review_hook_does_not_fire_for_worker_or_paper_or_corr_or_sound_request_kinds`),
            // so by definition no regeneration runs for those kinds.
            // This test is the explicit pin: if a future edit relaxes
            // the gate to also fire under, say, Worker, regeneration
            // must NOT happen for Worker (it would silently rewrite
            // the worker's already-dispatched prompt).
            //
            // We exercise the contract directly: with a Worker in
            // flight, the hook returns None (gate blocks), and the
            // in-flight request must be exactly what we put there —
            // no clone, no mutation.
            for kind in [
                RequestKind::Worker,
                RequestKind::Paper,
                RequestKind::Corr,
                RequestKind::Sound,
            ] {
                let dir = tempdir().expect("tempdir");
                let repo = dir.path();
                seed_repo(repo);
                write_node(repo, "Foo", "trivial");
                let mut state = ProtocolState::default();
                state.cycle = 5;
                state.live.present_nodes.insert(NodeId::from("Foo"));
                state.proof_nodes.insert(NodeId::from("Foo"));
                state
                    .local_closure_unverified_nodes
                    .insert(NodeId::from("Foo"));
                let original = state.expected_request(99, kind);
                state.in_flight_request = Some(original.clone());
                let runtime_root = tempdir().expect("rt");
                let _ = run_pre_step_revalidation_if_needed_pure(
                    &mut state,
                    repo,
                    runtime_root.path(),
                    5,
                    |_repo, _node| Ok(ok_probe(&NodeId::from("Foo"))),
                );
                assert_eq!(
                    state.in_flight_request.as_ref().expect("still in flight"),
                    &original,
                    "Q3: in-flight {kind:?} request must NOT be regenerated by the pre-review hook",
                );
            }
        }

        #[test]
        fn pre_step_revalidation_no_regenerate_when_no_in_flight_request() {
            // Patch C-O HIGH 2 — when no request is in flight, the hook
            // still fires (one of the two permitted conditions) and
            // mutates state. Regeneration is no longer attempted; the
            // test stays as a sanity check that no panic occurs.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.cycle = 5;
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            // No in_flight_request.
            assert!(state.in_flight_request.is_none());
            let runtime_root = tempdir().expect("rt");
            let batch = run_pre_step_revalidation_if_needed_pure(
                &mut state,
                repo,
                runtime_root.path(),
                5,
                |_repo, _node| Ok(ok_probe(&NodeId::from("Foo"))),
            );
            assert!(batch.is_some(), "hook fires when no request is in flight");
            assert!(
                state.in_flight_request.is_none(),
                "missing in-flight request must stay None — no spurious creation"
            );
        }

        #[test]
        fn cleanup_revalidation_adapter_passes_non_cleanup_through() {
            // The wrapper must not modify non-cleanup responses.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            let mut state = ProtocolState::default();
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));
            let worker_response = trellis_kernel::WorkerResponse::default();
            let responses = vec![WrapperResponse::Worker(worker_response)];
            let inner = StubAdapter { responses };
            let _runtime_root = tempdir().expect("runtime root");
            let shared: std::rc::Rc<std::cell::RefCell<Option<RevalidationBatch>>> =
                std::rc::Rc::new(std::cell::RefCell::new(None));
            let mut wrapper = CleanupRevalidationAdapter {
                inner,
                state: &state,
                repo: repo.to_path_buf(),
                current_cycle: 9,
                shared_batch: shared.clone(),
            };
            let mut request = WrapperRequest::default();
            request.kind = RequestKind::Worker;
            request.worker_context.validation_kind = WorkerValidationKind::ProofLocal;
            let response = wrapper.dispatch(&request).expect("dispatch");
            let WrapperResponse::Worker(worker) = response else {
                panic!("expected Worker response");
            };
            assert!(
                worker.local_closure_revalidation.is_none(),
                "non-cleanup wrapper must NOT populate local_closure_revalidation"
            );
            assert!(
                shared.borrow().is_none(),
                "Q2: non-cleanup must NOT populate the shared batch cell",
            );
        }

        #[test]
        fn cleanup_revalidation_does_not_persist_records_before_engine_acceptance() {
            // Patch C-Q Q2 — the adapter must NOT touch
            // `<runtime_root>/checker-state/local-closure-records/`
            // during dispatch. Persistence is deferred to after the
            // engine accepts the cleanup response (post-`step_with_checkpoint_sink`).
            // This test exercises the adapter directly and asserts the
            // records dir stays empty regardless of what the adapter
            // would otherwise persist.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.cycle = 9;
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));

            let worker_response = trellis_kernel::WorkerResponse::default();
            let responses = vec![WrapperResponse::Worker(worker_response)];
            let inner = StubAdapter { responses };
            let runtime_root_dir = tempdir().expect("runtime root");
            let runtime_root = runtime_root_dir.path();
            let records_dir = local_closure_records_dir(runtime_root);
            let shared: std::rc::Rc<std::cell::RefCell<Option<RevalidationBatch>>> =
                std::rc::Rc::new(std::cell::RefCell::new(None));
            let mut wrapper = CleanupRevalidationAdapter {
                inner,
                state: &state,
                repo: repo.to_path_buf(),
                current_cycle: 9,
                shared_batch: shared.clone(),
            };
            let mut request = WrapperRequest::default();
            request.kind = RequestKind::Worker;
            request.worker_context.validation_kind = WorkerValidationKind::Cleanup;

            let _ = wrapper.dispatch(&request).expect("dispatch");

            // The records dir must NOT have been created or populated
            // by the adapter. Even if the dispatch failed (transport
            // error on the empty repo), the adapter must not have
            // written any persisted JSON.
            let records_present = records_dir.exists()
                && fs::read_dir(&records_dir).map(|d| d.count()).unwrap_or(0) > 0;
            assert!(
                !records_present,
                "Q2: cleanup adapter must NOT persist records to disk during dispatch — \
                 persistence is deferred to post-acceptance. Records dir state: \
                 exists={}, entry count={}",
                records_dir.exists(),
                if records_dir.exists() {
                    fs::read_dir(&records_dir).map(|d| d.count()).unwrap_or(0)
                } else {
                    0
                },
            );
            // The batch is stashed in the shared cell for the post-
            // acceptance sweep (`step_runtime` reads it).
            assert!(
                shared.borrow().is_some(),
                "Q2: cleanup adapter must populate the shared batch cell for the post-acceptance sweep",
            );
        }

        #[test]
        fn cleanup_revalidation_persists_records_only_after_engine_acceptance() {
            // Patch C-Q Q2 — partner test: the records dir gets
            // populated only by `step_runtime`'s post-acceptance
            // persistence sweep, which reads the shared cell. This
            // test simulates that sweep manually (the runtime/engine
            // wiring is exercised in the audit-flagged
            // integration-style harness; here we pin the unit-level
            // contract that the shared cell carries the batch and
            // that the sweep semantics are "persist iff the entry is
            // still in `state.local_closure_records`").
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.cycle = 9;
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));

            // Pretend the engine accepted the batch and routed it
            // through `apply_revalidation_batch`, which inserted Foo
            // into `state.local_closure_records`. We mock that by
            // synthesizing a record directly.
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record");
            state
                .local_closure_records
                .insert(NodeId::from("Foo"), record.clone());
            state
                .local_closure_unverified_nodes
                .remove(&NodeId::from("Foo"));

            // Build the batch the adapter would have produced.
            let mut batch = RevalidationBatch::default();
            batch.refreshed.push((NodeId::from("Foo"), record.clone()));

            // Manually emulate the `step_runtime` post-acceptance sweep.
            let runtime_root_dir = tempdir().expect("rt root");
            let runtime_root = runtime_root_dir.path();
            let records_dir = local_closure_records_dir(runtime_root);
            for (node, _r) in &batch.refreshed {
                if let Some(accepted) = state.local_closure_records.get(node) {
                    persist_record_to_disk(&records_dir, accepted, 9).expect("persist");
                }
            }
            let on_disk = records_dir.join(trellis_kernel::runtime::persisted_record_file_name(
                &NodeId::from("Foo"),
            ));
            assert!(
                on_disk.exists(),
                "Q2: after engine acceptance, the record must land on disk via the post-acceptance sweep",
            );
        }

        #[test]
        fn cleanup_revalidation_post_acceptance_sweep_skips_engine_rejected_entries() {
            // Patch C-Q Q2 — defense in depth: the post-acceptance
            // sweep filters by `state.local_closure_records.contains_key`.
            // If the engine dropped a batch entry (e.g.
            // `apply_revalidation_batch`'s eligibility filter rejected
            // it because the node became `Open` during the cleanup
            // delta), the sweep must NOT persist that entry — on-disk
            // state must mirror the kernel's view.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let mut state = ProtocolState::default();
            state.cycle = 9;
            // Foo is in batch.refreshed but engine rejected
            // (not present in state.local_closure_records).
            // (Don't insert into records.)
            let record = compute_local_closure_record_inputs(
                repo,
                &NodeId::from("Foo"),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                "snap".to_string(),
                AxcheckStatus::Agreed,
            )
            .expect("compute record");
            let mut batch = RevalidationBatch::default();
            batch.refreshed.push((NodeId::from("Foo"), record));

            let runtime_root_dir = tempdir().expect("rt root");
            let runtime_root = runtime_root_dir.path();
            let records_dir = local_closure_records_dir(runtime_root);
            for (node, _r) in &batch.refreshed {
                if let Some(accepted) = state.local_closure_records.get(node) {
                    persist_record_to_disk(&records_dir, accepted, 9).expect("persist");
                }
            }
            let on_disk = records_dir.join(trellis_kernel::runtime::persisted_record_file_name(
                &NodeId::from("Foo"),
            ));
            assert!(
                !on_disk.exists(),
                "Q2: post-acceptance sweep must NOT persist an entry that the engine dropped",
            );
        }

        // ---- Patch C-E gap-fill tests ----------------------------------

        #[test]
        fn migration_persists_successful_records_for_resumability() {
            // Plan §7.10 / test 25 — when a migration probe succeeds, the
            // refreshed record is persisted to disk; a subsequent restart
            // loads it back without re-probing. This is the load-bearing
            // resumability invariant.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            let runtime_root = tempdir().expect("runtime root");

            // First migration: probes once, persists.
            let mut state = ProtocolState::default();
            // C-G's HIGH 6 batch filter requires nodes to be present +
            // proof-bearing + not-open; populate present_nodes alongside
            // proof_nodes so the migration's batch entries survive the
            // filter.
            state.live.present_nodes.insert(NodeId::from("Foo"));
            state.proof_nodes.insert(NodeId::from("Foo"));
            let mut first_probe_calls = 0usize;
            let _ = run_migration_if_needed_with_probe(
                &mut state,
                repo,
                runtime_root.path(),
                10,
                |_repo, _node| {
                    first_probe_calls += 1;
                    Ok(ok_probe_axcheck_agreed(&NodeId::from("Foo")))
                },
            )
            .expect("first migration");
            assert_eq!(first_probe_calls, 1, "first run probes Foo once");
            assert!(state
                .local_closure_records
                .contains_key(&NodeId::from("Foo")));
            // Verify the on-disk record exists.
            let records_dir = local_closure_records_dir(runtime_root.path());
            assert!(records_dir.join("Foo.json").exists());

            // Simulate restart: drop the in-memory state and rerun the
            // migration. With the persisted record present, the second
            // run loads it and skips the probe entirely.
            let mut state2 = ProtocolState::default();
            // Same filter setup as state above.
            state2.live.present_nodes.insert(NodeId::from("Foo"));
            state2.proof_nodes.insert(NodeId::from("Foo"));
            let mut second_probe_calls = 0usize;
            let _ = run_migration_if_needed_with_probe(
                &mut state2,
                repo,
                runtime_root.path(),
                20,
                |_repo, _node| {
                    second_probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                },
            )
            .expect("second migration");
            assert_eq!(
                second_probe_calls, 0,
                "resumed migration must skip probe when persisted record is fresh"
            );
            assert!(state2
                .local_closure_records
                .contains_key(&NodeId::from("Foo")));
        }

        #[test]
        fn deterministic_revalidation_returns_empty_batch_when_no_unverified_nodes() {
            // Plan §7.5 — when `local_closure_unverified_nodes` is empty,
            // the deterministic pass is a no-op: no probes called, empty
            // batch returned. This is the "phase-complete" fast-path.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            let state = ProtocolState::default(); // unverified set is empty
            let mut probe_calls = 0usize;
            let batch =
                deterministic_revalidate_at_cli_with_probe(&state, repo, 10, |_repo, _node| {
                    probe_calls += 1;
                    Ok(ok_probe(&NodeId::from("Foo")))
                });
            assert_eq!(probe_calls, 0, "no nodes to revalidate → no probe calls");
            assert!(batch.refreshed.is_empty());
            assert!(batch.still_unverified.is_empty());
        }

        // ────────────────────────────────────────────────────────────
        // Audit HIGH 1 — persisted record dep-hash validation.
        // ────────────────────────────────────────────────────────────

        /// Helper: build a sorry-free record for `node` with given dep
        /// hashes against `repo`. Caller adds the dep-hash payload after.
        fn record_for_dep_test(
            repo: &Path,
            node: &str,
            boundary_theorems: BTreeMap<NodeId, String>,
            strict_theorem_deps: BTreeMap<NodeId, String>,
            strict_definition_deps: BTreeMap<NodeId, String>,
        ) -> LocalClosureRecord {
            compute_local_closure_record_inputs(
                repo,
                &NodeId::from(node),
                &BTreeSet::new(),
                &boundary_theorems,
                &strict_theorem_deps,
                &strict_definition_deps,
                format!("snap-{node}"),
                AxcheckStatus::Agreed,
            )
            .expect("compute record inputs")
        }

        #[test]
        fn record_hashes_match_current_rejects_record_with_stale_boundary_hash() {
            // Audit HIGH 1 — a persisted record's `boundary_theorems` dep
            // hash must agree with any other in-state record's hash for
            // the same dep. Disagreement implies one of the records is
            // stale; the candidate is rejected (return false).
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            write_node(repo, "Bar", "trivial");

            let mut state = ProtocolState::default();
            // Patch C-O HIGH 1 (b): dep must be present in live for the
            // sanity-check arm to pass after the strict-signal filter.
            state.live.present_nodes.insert(NodeId::from("Helper"));
            // Existing in-state record Bar references Helper with the
            // NEW hash (post-edit). The candidate Foo also references
            // Helper but recorded the OLD hash — disagreement → stale.
            let mut bar_boundary = BTreeMap::new();
            bar_boundary.insert(NodeId::from("Helper"), "new-hash".to_string());
            let bar_record =
                record_for_dep_test(repo, "Bar", bar_boundary, BTreeMap::new(), BTreeMap::new());
            state
                .local_closure_records
                .insert(NodeId::from("Bar"), bar_record);

            let mut foo_boundary = BTreeMap::new();
            foo_boundary.insert(NodeId::from("Helper"), "old-hash".to_string());
            let foo_record =
                record_for_dep_test(repo, "Foo", foo_boundary, BTreeMap::new(), BTreeMap::new());

            assert!(
                !record_hashes_match_current(&foo_record, repo, &state),
                "stale boundary hash must reject the candidate"
            );

            // Sanity check: agreement passes.
            let mut foo_boundary_agree = BTreeMap::new();
            foo_boundary_agree.insert(NodeId::from("Helper"), "new-hash".to_string());
            let foo_record_agree = record_for_dep_test(
                repo,
                "Foo",
                foo_boundary_agree,
                BTreeMap::new(),
                BTreeMap::new(),
            );
            assert!(
                record_hashes_match_current(&foo_record_agree, repo, &state),
                "matching boundary hash must accept the candidate"
            );
        }

        #[test]
        fn record_hashes_match_current_rejects_record_with_stale_strict_theorem_hash() {
            // Audit HIGH 1 — same as boundary check but for
            // `strict_theorem_deps`. Disagreement → reject.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            write_node(repo, "Bar", "trivial");

            let mut state = ProtocolState::default();
            // Patch C-O HIGH 1 (b): dep must be present in live so the
            // strict-signal filter doesn't reject before we even hit the
            // cross-record disagreement check.
            state.live.present_nodes.insert(NodeId::from("ThmT"));
            let mut bar_strict_thm = BTreeMap::new();
            bar_strict_thm.insert(NodeId::from("ThmT"), "new-val".to_string());
            let bar_record = record_for_dep_test(
                repo,
                "Bar",
                BTreeMap::new(),
                bar_strict_thm,
                BTreeMap::new(),
            );
            state
                .local_closure_records
                .insert(NodeId::from("Bar"), bar_record);

            let mut foo_strict_thm = BTreeMap::new();
            foo_strict_thm.insert(NodeId::from("ThmT"), "old-val".to_string());
            let foo_record = record_for_dep_test(
                repo,
                "Foo",
                BTreeMap::new(),
                foo_strict_thm,
                BTreeMap::new(),
            );

            assert!(
                !record_hashes_match_current(&foo_record, repo, &state),
                "stale strict_theorem_deps hash must reject the candidate"
            );
        }

        #[test]
        fn record_hashes_match_current_rejects_record_with_stale_strict_definition_hash() {
            // Audit HIGH 1 — same as boundary check but for
            // `strict_definition_deps`. Disagreement → reject.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            write_node(repo, "Bar", "trivial");

            let mut state = ProtocolState::default();
            // Patch C-O HIGH 1 (b): dep must be present in live so the
            // strict-signal filter doesn't reject before we even hit the
            // cross-record disagreement check.
            state.live.present_nodes.insert(NodeId::from("DefD"));
            let mut bar_strict_def = BTreeMap::new();
            bar_strict_def.insert(NodeId::from("DefD"), "new-sem".to_string());
            let bar_record = record_for_dep_test(
                repo,
                "Bar",
                BTreeMap::new(),
                BTreeMap::new(),
                bar_strict_def,
            );
            state
                .local_closure_records
                .insert(NodeId::from("Bar"), bar_record);

            let mut foo_strict_def = BTreeMap::new();
            foo_strict_def.insert(NodeId::from("DefD"), "old-sem".to_string());
            let foo_record = record_for_dep_test(
                repo,
                "Foo",
                BTreeMap::new(),
                BTreeMap::new(),
                foo_strict_def,
            );

            assert!(
                !record_hashes_match_current(&foo_record, repo, &state),
                "stale strict_definition_deps hash must reject the candidate"
            );
        }

        // ────────────────────────────────────────────────────────────
        // Audit HIGH 5 — migration runs at unsafe times.
        // ────────────────────────────────────────────────────────────

        #[test]
        fn migration_skips_when_phase_is_cleanup() {
            // Audit HIGH 5 — migration must not run in Cleanup phase:
            // introducing unverified nodes there would block
            // `formalization_complete` and prevent Cleanup `Done`.
            let mut state = ProtocolState::default();
            state.phase = Phase::Cleanup;
            // No in_flight_request — only the phase should trigger skip.
            let reason = local_closure_migration_skip_reason(&state)
                .expect("Cleanup phase must produce a skip reason");
            assert!(
                reason.contains("Cleanup"),
                "skip reason must mention Cleanup phase; got {reason}"
            );
        }

        #[test]
        fn migration_skips_when_worker_response_is_in_flight() {
            // Audit HIGH 5 / Patch C-O MEDIUM 2 — migration must not run
            // while a Worker request is in flight: the repo may contain
            // unaccepted edits that would be captured in persisted
            // records. Patch C-O generalized this to "any in-flight
            // request kind"; the Worker case is preserved here.
            let mut state = ProtocolState::default();
            state.phase = Phase::ProofFormalization;
            state.in_flight_request = Some(state.expected_request(7, RequestKind::Worker));
            let reason = local_closure_migration_skip_reason(&state)
                .expect("Worker in-flight must produce a skip reason");
            assert!(
                reason.contains("Worker"),
                "skip reason must mention Worker kind; got {reason}"
            );
        }

        #[test]
        fn migration_runs_when_phase_is_proof_formalization_with_no_in_flight_request() {
            // Audit HIGH 5 — safe path: ProofFormalization phase with no
            // in-flight Worker request → migration runs (skip reason is
            // None).
            let mut state = ProtocolState::default();
            state.phase = Phase::ProofFormalization;
            // No in_flight_request.
            assert!(
                local_closure_migration_skip_reason(&state).is_none(),
                "ProofFormalization with no in-flight request must allow migration"
            );
        }

        #[test]
        fn migration_skips_when_review_is_in_flight() {
            // Patch C-O MEDIUM 2 — tightened from "Worker only" to "any
            // in-flight request": a Review prompt already references a
            // particular blocker/legality snapshot and silently mutating
            // state via migration drifts the prompt from what the
            // response is checked against.
            let mut state = ProtocolState::default();
            state.phase = Phase::ProofFormalization;
            state.in_flight_request = Some(state.expected_request(1, RequestKind::Review));
            let reason = local_closure_migration_skip_reason(&state)
                .expect("Review in-flight must produce a skip reason after Patch C-O");
            assert!(
                reason.contains("Review"),
                "skip reason must mention Review kind; got {reason}"
            );
        }

        // ────────────────────────────────────────────────────────────
        // Audit MEDIUM — approved-axioms load errors propagate.
        // ────────────────────────────────────────────────────────────

        #[test]
        fn approved_axioms_load_error_propagates_as_internal_error_failure() {
            // Audit MEDIUM — when `APPROVED_AXIOMS.json` is corrupted,
            // the deterministic revalidation pass must NOT silently
            // substitute an empty approved set and "successfully" bless
            // the node. Instead it must record an `internal_error`
            // failure summary so the operator sees the load failure.
            let dir = tempdir().expect("tempdir");
            let repo = dir.path();
            seed_repo(repo);
            write_node(repo, "Foo", "trivial");
            // Write a corrupted approved-axioms file.
            fs::write(repo.join("APPROVED_AXIOMS.json"), "{ not valid json")
                .expect("write corrupt approved");

            let mut state = ProtocolState::default();
            state
                .local_closure_unverified_nodes
                .insert(NodeId::from("Foo"));

            let batch =
                deterministic_revalidate_at_cli_with_probe(&state, repo, 10, |_repo, _node| {
                    Ok(ok_probe(&NodeId::from("Foo")))
                });

            // Probe ok, but approved-axioms load failed: no refreshed
            // entry, an internal_error failure summary instead.
            assert!(
                batch.refreshed.is_empty(),
                "load error must NOT install a record; got {} refreshed",
                batch.refreshed.len()
            );
            assert_eq!(
                batch.still_unverified.len(),
                1,
                "load error must surface as a still_unverified entry"
            );
            let (node, summary) = &batch.still_unverified[0];
            assert_eq!(*node, NodeId::from("Foo"));
            assert_eq!(
                summary.status, "internal_error",
                "load error must be categorized as internal_error, not as a clean record"
            );
            assert!(
                summary.stderr_excerpt.contains("approved")
                    || summary.stderr_excerpt.contains("APPROVED"),
                "stderr_excerpt must mention the approved-axioms load failure; got {:?}",
                summary.stderr_excerpt
            );
        }
    }
}

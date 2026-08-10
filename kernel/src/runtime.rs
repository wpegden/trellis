use crate::engine::{apply_event, ProtocolCommand, ProtocolEvent, TransitionError};
use crate::model::{
    GateKind, HumanChoice, NodeId, Phase, ProtocolState, ResponseStatus, WorkerOutcome, WorkingSnapshot,
    WrapperRequest, WrapperResponse, SOUND_ASSESSMENT_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use crate::trust_base::{
    load_revision_closure, parse_json_strict, tagged_hash, ActorKeyManifest, ActorRole, AppendRequest,
    AuthoritativeRecord, DomainTag, EventKind, JournalActor, JournalEvent,
    JournalRoutineGateOutcome, ManifestAuthorityRoots, SchemaRegistry, Subject, TrustJournal,
    seed_support_definition_projection, seed_worker_projections, verify_evidence_tool_manifest,
    verify_seed_definition_bundle, verify_seed_support_definition_files,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct RuntimePaths {
    pub root: PathBuf,
    pub state_path: PathBuf,
    pub checkpoint_path: PathBuf,
    pub metadata_path: PathBuf,
}

impl RuntimePaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            state_path: root.join("protocol_state.json"),
            checkpoint_path: root.join("checkpoint.json"),
            metadata_path: root.join("runtime_metadata.json"),
            root,
        }
    }
}

/// Resolve the per-cycle event-log directory.
///
/// The log lives inside the TRACKED repo tree at
/// `<repo>/.trellis-history/event-log/` so checkpoint commits (`git add -A`)
/// version it alongside `supervisor_state.json`. When `repo_path` is unset
/// (headless tests, imports run before a repo is attached) fall back to a
/// runtime-local `<root>/.event-log-fallback/` so the runtime still has a
/// place to append.
pub fn event_log_dir_for(root: &Path, metadata: &RuntimeMetadata) -> PathBuf {
    match metadata.repo_path.as_deref() {
        Some(repo) => repo.join(".trellis-history").join("event-log"),
        None => root.join(".event-log-fallback"),
    }
}

/// The per-cycle event-log file for `cycle` inside `dir`. Lexical name order
/// (zero-padded width 6) equals cycle order equals global index order.
pub fn event_log_cycle_file(dir: &Path, cycle: u32) -> PathBuf {
    dir.join(format!("cycle-{cycle:06}.jsonl"))
}

/// Lexically-sorted absolute paths of the per-cycle event-log files
/// (`cycle-NNNNNN.jsonl`) present in `dir`. Returns an empty vec when the
/// directory does not yet exist.
pub fn event_log_cycle_files(dir: &Path) -> Result<Vec<PathBuf>, RuntimeError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_cycle_file = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|name| name.starts_with("cycle-") && name.ends_with(".jsonl"))
            .unwrap_or(false);
        if is_cycle_file {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeMetadata {
    pub repo_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub native_history_kinds: BTreeSet<String>,
    /// Replay determinism gate for the fresh-run initial planner. `true` on
    /// runs initialized after the feature landed (stamped at Init /
    /// InitFromConfig); `false` (serde default) on pre-feature metadata.
    /// `seed_state_from_config` seeds `initial_planning` only when this is
    /// set, so `replay_to_event_count` over a pre-feature event log rebuilds
    /// the seed state byte-identically to the original run.
    #[serde(default)]
    pub initial_planning_seeded: bool,
    /// Trust-base v1 paths are installed by trusted run configuration. The
    /// journal must be outside `repo_path`; the actor manifest is root-signed
    /// under one of the independently configured authority keys below.
    #[serde(default)]
    pub trust_journal_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_actor_key_manifest_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_gate_presentation_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_seed_manifest_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_seed_definition_bundle_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_evidence_tool_manifest_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_evidence_tool_root_path: Option<PathBuf>,
    #[serde(default)]
    pub trust_seed_transaction_id: Option<String>,
    #[serde(default)]
    pub trust_advance_gate_episode_id: Option<String>,
    #[serde(default)]
    pub trust_manifest_authority_roots: BTreeMap<String, String>,
    /// Replay determinism gate for periodic coverage re-planning, the exact
    /// `initial_planning_seeded` pattern (a separate flag: initial-planner-era
    /// logs must NOT arm the coverage trigger at replay, or replay would
    /// dispatch a planner where the original run dispatched a worker). `true`
    /// on runs initialized after the feature landed (stamped at Init /
    /// InitFromConfig; imports never fresh-plan, so ImportLegacy /
    /// ImportRevisionProject stamp `false`); `seed_state_from_config` seeds
    /// `ProtocolState.coverage_replanning_source` only when this is set.
    #[serde(default)]
    pub coverage_replanning_seeded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCheckpoint {
    pub cycle: u32,
    pub phase: Phase,
    pub gate_kind: GateKind,
    pub active_node: Option<NodeId>,
    pub committed: WorkingSnapshot,
    /// Binding to the external, non-rewindable trust journal.  A legacy or
    /// non-PV checkpoint has no binding; RequiredV1 checkpoints must match or
    /// trail the journal and are reconciled from the journal on restart.
    #[serde(default)]
    pub trust_journal_checkpoint: Option<crate::trust_base::JournalCheckpointBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointHookPayload {
    pub root: PathBuf,
    pub state_path: PathBuf,
    /// Per-cycle event-log directory (`<repo>/.trellis-history/event-log/`).
    /// Informational only — the checkpoint hook stages everything via
    /// `git add -A` and never reads this path.
    pub event_log_dir: PathBuf,
    pub checkpoint_path: PathBuf,
    pub metadata_path: PathBuf,
    pub metadata: RuntimeMetadata,
    pub state: ProtocolState,
    pub checkpoint: RuntimeCheckpoint,
    pub commands: Vec<ProtocolCommand>,
    pub event_count: u64,
    /// True iff `state.global_blockers().is_empty()` at emission time.
    /// Checkpoint hook uses this to write an additional
    /// `supervisor2/clean-NNNNNN` tag so reviewer-driven
    /// `ResetChoice::LastClean` has something to rewind to.
    #[serde(default)]
    pub is_clean: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventLogRecord {
    pub index: u64,
    pub event: ProtocolEvent,
    pub commands: Vec<ProtocolCommand>,
    pub phase: Phase,
    pub stage: crate::model::Stage,
    pub cycle: u32,
    /// Wall-clock timestamp when this record was appended to the log, in
    /// milliseconds since the Unix epoch. `#[serde(default)]` keeps older
    /// event logs (without the field) parseable — they'll read as 0.
    #[serde(default)]
    pub ts_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeStepStatus {
    Transitioned,
    /// Required-v1 has committed final cleanup and is waiting only for the
    /// external deterministic package authorization. The supervisor must stop
    /// dispatching agents; no human response is requested.
    PackageReady,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStepOutcome {
    pub status: RuntimeStepStatus,
    pub event: Option<ProtocolEvent>,
    pub commands: Vec<ProtocolCommand>,
}

pub trait WrapperAdapter {
    fn dispatch(&mut self, request: &WrapperRequest) -> Result<WrapperResponse, String>;
}

pub trait CheckpointSink {
    fn commit(&mut self, payload: &CheckpointHookPayload) -> Result<(), String>;
}

#[derive(Default)]
pub struct NoopCheckpointSink;

impl CheckpointSink for NoopCheckpointSink {
    fn commit(&mut self, _payload: &CheckpointHookPayload) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Io(std::io::Error),
    Serde(serde_json::Error),
    Kernel(TransitionError),
    Adapter(String),
    CheckpointSink(String),
    InvalidRuntimeState(String),
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Serde(err) => write!(f, "serde error: {err}"),
            Self::Kernel(err) => write!(f, "kernel error: {:?}", err),
            Self::Adapter(err) => write!(f, "adapter error: {err}"),
            Self::CheckpointSink(err) => write!(f, "checkpoint sink error: {err}"),
            Self::InvalidRuntimeState(err) => write!(f, "invalid runtime state: {err}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<std::io::Error> for RuntimeError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value)
    }
}

impl From<TransitionError> for RuntimeError {
    fn from(value: TransitionError) -> Self {
        Self::Kernel(value)
    }
}

pub struct SupervisorRuntime {
    paths: RuntimePaths,
    state: ProtocolState,
    metadata: RuntimeMetadata,
    event_count: u64,
}

const ACTIVE_WORKER_BASE_SCHEMA_VERSION: u32 = 1;

/// Presence is part of the snapshot: in particular, an absent `reference/`
/// at dispatch must be absent again after rollback even though entering the
/// worker sandbox creates that writable directory on demand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveWorkerBaseManifest {
    schema_version: u32,
    tablet_present: bool,
    reference_present: bool,
}

impl SupervisorRuntime {
    fn active_worker_base_dir(&self) -> PathBuf {
        self.paths.root.join("active_worker_base")
    }

    fn active_worker_base_manifest_path(&self) -> PathBuf {
        self.active_worker_base_dir().join("worker_surfaces.json")
    }

    fn active_worker_base_tablet_dir(&self) -> PathBuf {
        self.active_worker_base_dir().join("Tablet")
    }

    fn active_worker_base_reference_dir(&self) -> PathBuf {
        self.active_worker_base_dir().join("reference")
    }

    fn capture_active_worker_base_for_request(
        &self,
        request: &crate::model::WrapperRequest,
    ) -> Result<(), RuntimeError> {
        if request.kind != crate::model::RequestKind::Worker {
            return Ok(());
        }
        let repo_path = match self.metadata.repo_path.as_deref() {
            Some(path) => path,
            None => return Ok(()),
        };
        let tablet_dir = repo_path.join("Tablet");
        let reference_dir = repo_path.join("reference");
        let tablet_present = worker_surface_directory_present(&tablet_dir)?;
        let reference_present = worker_surface_directory_present(&reference_dir)?;
        let capture_root = self.active_worker_base_dir();
        if capture_root.exists() {
            fs::remove_dir_all(&capture_root)?;
        }
        fs::create_dir_all(&capture_root)?;
        if tablet_present {
            copy_dir_recursive(&tablet_dir, &self.active_worker_base_tablet_dir())?;
        }
        if reference_present {
            copy_dir_recursive(&reference_dir, &self.active_worker_base_reference_dir())?;
        }
        let manifest = ActiveWorkerBaseManifest {
            schema_version: ACTIVE_WORKER_BASE_SCHEMA_VERSION,
            tablet_present,
            reference_present,
        };
        fs::write(
            self.active_worker_base_manifest_path(),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        Ok(())
    }

    pub fn initialize(paths: RuntimePaths, state: ProtocolState) -> Result<Self, RuntimeError> {
        Self::initialize_with_metadata(paths, state, RuntimeMetadata::default())
    }

    pub fn initialize_with_metadata(
        paths: RuntimePaths,
        mut state: ProtocolState,
        metadata: RuntimeMetadata,
    ) -> Result<Self, RuntimeError> {
        fs::create_dir_all(&paths.root)?;
        state.normalize_all_structural_state();
        let mut runtime = Self {
            paths,
            state,
            metadata,
            event_count: 0,
        };
        runtime.reconcile_trust_journal_projection()?;
        // Fresh CLI initialization seeds configured Decide definitions and
        // materializes their files as separate lifecycle operations. Do not
        // inspect the intermediate worktree here. The first load (before any
        // dispatch), every post-step state, and active-worker restoration all
        // validate the complete disk layout fail-closed.
        if runtime.state.trust_base.required() {
            runtime
                .state
                .validate()
                .map_err(RuntimeError::InvalidRuntimeState)?;
        }
        runtime.persist_state()?;
        runtime.persist_metadata()?;
        Ok(runtime)
    }

    pub fn load(paths: RuntimePaths) -> Result<Self, RuntimeError> {
        let state: ProtocolState = serde_json::from_str(&fs::read_to_string(&paths.state_path)?)?;
        let metadata = read_metadata(&paths.metadata_path)?;
        let event_count = read_event_count(&event_log_dir_for(&paths.root, &metadata))?;
        // Misorder guard (segmentation migration): an ABSENT/empty
        // event-log dir is indistinguishable from a cold start by count
        // alone. If the loaded state is non-initial (cycle >= 1 implies
        // at least the start_cycle event was appended), an empty dir
        // means the operator launched a segmentation-aware binary
        // before running `segment_event_log` — appending would restart
        // the dense index at 0 and corrupt the log. Fail loud instead.
        if event_count == 0 && state.cycle >= 1 {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "event-log dir is absent/empty but the loaded state is non-initial \
                 (cycle {}); run `segment_event_log` (or restore the per-cycle \
                 files) before launching this binary",
                state.cycle
            )));
        }
        let mut runtime = Self {
            paths,
            state,
            metadata,
            event_count,
        };
        runtime.state.normalize_all_structural_state();
        // Decide-pair registration backfill (audit round 2 B2; ORDERING fix,
        // round-2 follow-up 1). This has to run HERE, not from the CLI's
        // post-load migration block, because on a `TrustBaseMode::RequiredV1`
        // run `reconcile_trust_journal_projection` below ends in
        // `ProtocolState::validate()` — the first gate a trust-required load
        // hits. Run afterwards, the migration's `node_kinds` repair arm (and
        // the `open_nodes` re-derivation that guards it) was unreachable on
        // exactly the class of run it was written for: validate() rejects a
        // wrong/absent Decide-pair `node_kinds` value before the repair can
        // touch it. Running it first also puts it ahead of
        // `migrate_corr_fingerprint_schema` /
        // `migrate_soundness_fingerprint_schema_if_enabled`, both of which key
        // off `node_kinds` and would otherwise recompute against a kind this
        // migration is about to correct.
        //
        // `validate()` is NOT weakened: it still runs, unchanged, immediately
        // after — this only gives a REPAIRABLE state the chance to be repaired
        // before it is judged. Anything the migration cannot re-derive
        // (unconfigured target, second claimant, a shape outside its scope)
        // still fails loud, now with the migration's own diagnostic printed
        // first.
        //
        // MATH-MODE NEUTRALITY: the migration's body is a loop over
        // `configured_challenge_targets` filtered by `is_decide_primary`, so a
        // non-PV state returns `Ok(false)` before touching a field and nothing
        // is persisted. The `repo_path` + `Tablet/` guard is carried over
        // verbatim from the CLI call site, so the set of loads on which it does
        // any work is unchanged — only its position in the load order moved.
        //
        // Paired safety change: the migration now defers under ANY in-flight
        // request, not just a Worker burst. `validate()` pins the persisted
        // request against `expected_request` recomputed from the state, and
        // that projection reads the facts this migration writes — so repairing
        // ahead of validate() while a request is in flight would desync the
        // pair and hard-fail the load. Deferring costs nothing: the repair is
        // idempotent and lands at the next idle load.
        //
        // Known ordering nuance: this now precedes
        // `recover_interrupted_configured_decide_flips` below, so on a load
        // that catches a TORN Decide file move the `open_nodes` re-derivation
        // reads disk before the layout is repaired and may under-state
        // openness. That combination (torn move AND a stranded kind AND an
        // active closure tier) still fails loud in validate() — i.e. no worse
        // than today, where such a state is rejected outright.
        let mut persist_after_load = match runtime.metadata.repo_path.clone() {
            Some(repo_path) if repo_path.join("Tablet").is_dir() => {
                crate::runtime_cli_observations::migrate_stranded_decide_registration(
                    &mut runtime.state,
                    &repo_path,
                )
                .map_err(RuntimeError::InvalidRuntimeState)?
            }
            _ => false,
        };
        // Reverse indices are `#[serde(skip)]` — rebuild them from the
        // freshly-loaded `local_closure_records` before any code path can
        // run `validate()`, otherwise the new H-1 reverse-index assert
        // will fire on the first event after restart.
        crate::model::recompute_local_closure_reverse_indices(&mut runtime.state);
        // The persisted in-flight request includes dispatch-only execution
        // hints attached immediately before a burst: actor/verifier bindings,
        // prompt contracts, and `fresh_context`.  `ProtocolState::validate`,
        // however, compares the request with the semantic projection produced
        // by `expected_request` (whose execution hints are deliberately
        // unresolved).  Trust-v1 reconciliation validates the state below, so
        // normalize ONLY those execution-hint fields before entering the trust
        // boundary.  Replacing the whole request here would erase semantic or
        // seed-bound tampering before validation could reject it.
        //
        // After the external journal and seed projection are verified, the
        // ordinary full refresh below reflects any journal-selected revision;
        // execution hints are then reattached from the pinned runtime config.
        runtime.normalize_in_flight_request_execution_hints_for_validation();
        persist_after_load |= runtime.reconcile_trust_journal_projection()?;
        persist_after_load |= runtime.apply_sound_assessment_schema_cutover()?;
        let pre_heal_coarse_count = runtime.state.coarse_dag_nodes.len();
        runtime.heal_coarse_dag_from_git_if_needed();
        // Persist if the heal actually changed something. Without this the
        // heal lives only in memory until the next step writes — and step
        // can take many minutes (post-restart materialize-tablet-oleans is
        // typically 5-15 min before the first step write). Persisting here
        // makes the heal durable: a supervisor crash mid-materialize won't
        // require the next operator restart to re-discover the empty field.
        if runtime.state.coarse_dag_nodes.len() != pre_heal_coarse_count {
            persist_after_load = true;
        }
        if persist_after_load {
            runtime.persist_state()?;
        }
        runtime.refresh_in_flight_request_from_state();
        runtime.apply_request_dispatch_hints()?;
        // A crashed Worker may have left its writable Tablet/reference trees
        // in a partial state. Loading must remain possible so the bridge can
        // invoke `restore_active_worker_base_for_inflight`; that restore
        // validates the Decide layout before relaunch. Every non-Worker load
        // must already have an authoritative disk layout and validates here.
        let worker_restore_pending = runtime
            .state
            .in_flight_request
            .as_ref()
            .is_some_and(|request| request.kind == crate::model::RequestKind::Worker);
        if !worker_restore_pending {
            if let Some(repo_path) = runtime.metadata.repo_path.as_deref() {
                crate::dormant_store::recover_interrupted_configured_decide_flips(
                    repo_path,
                    &runtime.state,
                )
                .map_err(RuntimeError::InvalidRuntimeState)?;
                crate::dormant_store::validate_configured_decide_layout(repo_path, &runtime.state)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
            }
        }
        // Atomicity (audit, Option C): refuse to start if the loaded
        // state's last_clean readiness is internally inconsistent with
        // git. Fail loud here so a downstream reviewer-driven LastClean
        // doesn't either fail mid-step or silently rewind to a stale
        // tag describing a different state.
        runtime.validate_last_clean_tag_consistency()?;
        Ok(runtime)
    }

    /// Reconcile the non-rewindable trust authority for an already loaded,
    /// idle runtime.  Exceptional authorization and protected terminal events
    /// are committed by the separate trust CLI, so a long-lived supervisor
    /// must not require a process restart to notice that the journal advanced.
    pub fn reconcile_external_trust_authority(&mut self) -> Result<(), RuntimeError> {
        if self.state.in_flight_request.is_some() {
            return Err(RuntimeError::InvalidRuntimeState(
                "external trust authority may be reconciled only at an idle request boundary"
                    .into(),
            ));
        }
        if self.reconcile_trust_journal_projection()? {
            self.persist_state()?;
        }
        Ok(())
    }

    /// Rebuild the trust projection from the external journal before any
    /// rewindable checkpoint is trusted.  A journal-ahead state is accepted
    /// only for the exact crash window at the advance gate, or after the
    /// routine gate when the operational checkpoint merely trails later
    /// mechanical trust events.  A revoked approval never gets resurrected.
    fn reconcile_trust_journal_projection(&mut self) -> Result<bool, RuntimeError> {
        if !self.state.trust_base.required() {
            return Ok(false);
        }
        ensure_external_trust_journal_path(&self.metadata)?;
        let actor_keys = load_verified_actor_keys(&self.metadata)?;
        let journal_path = self.metadata.trust_journal_path.as_deref().ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires runtime_metadata.trust_journal_path".into(),
            )
        })?;
        let journal = TrustJournal::open(journal_path, actor_keys)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        let old_binding = self.state.trust_base.journal_checkpoint.clone();
        let old_approval = self.state.trust_base.current_human_approval_event_hash;
        let old_gate_state = self.state.trust_base.routine_gate_state;
        let old_revision_lane = self.state.trust_base.active_revision_lane_id.clone();
        let old_package = self.state.trust_base.package_authorization_event_hash;

        if let Some(binding) = old_binding.as_ref() {
            journal
                .verify_checkpoint_binding(binding)
                .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        }

        match journal.routine_gate_outcome() {
            JournalRoutineGateOutcome::NotPresented => {
                if self.state.trust_base.routine_gate_state
                    != crate::model::TrustRoutineGateState::NotPresented
                    || self
                        .state
                        .trust_base
                        .current_human_approval_event_hash
                        .is_some()
                {
                    return Err(RuntimeError::InvalidRuntimeState(
                        "checkpoint claims a trust-gate result absent from the sole-authority journal"
                            .into(),
                    ));
                }
            }
            JournalRoutineGateOutcome::Approved(_) => {
                let approval = journal.current_approval().ok_or_else(|| {
                    RuntimeError::InvalidRuntimeState(
                        "routine advance approval was later revoked or has an incomplete projection"
                            .into(),
                    )
                })?;
                if let Some(revision) = approval.revision_closure {
                    let stored = load_revision_closure(journal_path, &revision)
                        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
                    let seed_closure = stored.seed;
                    self.state.trust_base.seed_manifest_sha256 =
                        Some(seed_closure.seed_manifest_sha256);
                    self.state.trust_base.seed_definition_bundle_sha256 =
                        Some(seed_closure.bundle_sha256);
                    self.state.trust_base.evidence_tool_manifest_sha256 =
                        Some(stored.evidence.manifest_sha256);
                    self.state.trust_base.seed_support_definitions =
                        seed_support_definition_projection(&stored.evidence)
                            .map_err(|error| {
                                RuntimeError::InvalidRuntimeState(error.to_string())
                            })?;
                    let (candidates, source_guidance) =
                        seed_worker_projections(&seed_closure).map_err(|error| {
                            RuntimeError::InvalidRuntimeState(error.to_string())
                        })?;
                    self.state.trust_base.conditional_theorem_candidates = candidates;
                    self.state.trust_base.source_validation_guidance = source_guidance;
                }
                if self.state.trust_base.routine_gate_state
                    != crate::model::TrustRoutineGateState::Approved
                {
                    crate::engine::recover_trust_advance_approval(&mut self.state)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                self.state.trust_base.routine_gate_state =
                    crate::model::TrustRoutineGateState::Approved;
                self.state.trust_base.current_human_approval_event_hash =
                    Some(approval.event_hash);
                self.state.trust_base.authored_semantic_root =
                    Some(approval.authored_semantic_root);
                self.state.trust_base.approved_evidence_tool_input_root =
                    Some(approval.approved_evidence_tool_input_root);
                self.state.trust_base.gate_commit_pending = false;
            }
            JournalRoutineGateOutcome::Feedback(_) => {
                if self.state.trust_base.routine_gate_state
                    != crate::model::TrustRoutineGateState::FeedbackTerminated
                {
                    crate::engine::recover_trust_advance_feedback(&mut self.state)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                self.state.trust_base.routine_gate_state =
                    crate::model::TrustRoutineGateState::FeedbackTerminated;
                self.state.trust_base.current_human_approval_event_hash = None;
                self.state.trust_base.gate_commit_pending = false;
            }
        }
        let journal_revision_lane = journal.active_revision_lane_id().map(str::to_owned);
        match (old_revision_lane.as_deref(), journal_revision_lane.as_deref()) {
            (None, Some(_)) => {
                crate::engine::recover_trust_revision_open(&mut self.state)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
            }
            (Some(checkpoint_lane), None) => {
                let terminal = revision_terminal_kind(&journal, checkpoint_lane)?
                    .ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "checkpoint revision lane disappeared without a protected terminal"
                                .into(),
                        )
                    })?;
                if terminal == EventKind::ProtectedReapprovalFeedback
                    && self.metadata.repo_path.is_some()
                {
                    return Err(RuntimeError::InvalidRuntimeState(
                        "protected-reapproval feedback preserved the prior trust basis, but the runtime cannot prove that provisional revision worktree bytes were rolled back; restore the pre-revision checkpoint before resuming"
                            .into(),
                    ));
                }
                crate::engine::recover_trust_revision_terminal(&mut self.state)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
            }
            (Some(checkpoint_lane), Some(journal_lane)) if checkpoint_lane != journal_lane => {
                return Err(RuntimeError::InvalidRuntimeState(format!(
                    "checkpoint revision lane {checkpoint_lane} differs from journal lane {journal_lane}"
                )));
            }
            _ => {}
        }
        self.state.trust_base.active_revision_lane_id = journal_revision_lane;
        self.state.trust_base.package_authorization_event_hash =
            journal.current_package_authorization_event_hash();
        self.state.trust_base.journal_checkpoint = Some(
            journal
                .checkpoint_binding()
                .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?,
        );
        self.state.trust_base.last_fail_closed_reason = None;
        let seed_support_projection_migrated =
            verify_runtime_trust_seed_projection(&mut self.state, &self.metadata, &journal)?;
        self.state
            .validate()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        Ok(old_binding != self.state.trust_base.journal_checkpoint
            || old_approval != self.state.trust_base.current_human_approval_event_hash
            || old_gate_state != self.state.trust_base.routine_gate_state
            || old_revision_lane != self.state.trust_base.active_revision_lane_id
            || old_package != self.state.trust_base.package_authorization_event_hash
            || seed_support_projection_migrated)
    }

    fn apply_sound_assessment_schema_cutover(&mut self) -> Result<bool, RuntimeError> {
        if self.state.sound_assessment_schema_version >= SOUND_ASSESSMENT_SCHEMA_VERSION {
            return Ok(false);
        }
        if self.state.sound_assessment_cutover_requires_rewind() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "soundness assessment schema cutover requires a rewind: this state predates \
                 sound_assessment_schema_version={} but already contains Sound verifier lane \
                 evidence. Rewind the run to just before any Soundness lanes were dispatched \
                 (no in-flight Sound request, no sound_status / sound_approved_fingerprints, \
                 and no latest/previous Sound lane evidence), then restart.",
                SOUND_ASSESSMENT_SCHEMA_VERSION
            )));
        }
        self.state.sound_assessment_schema_version = SOUND_ASSESSMENT_SCHEMA_VERSION;
        Ok(true)
    }

    /// Recover `coarse_dag_nodes` from the supervisor's git history when the
    /// loaded state is in (or past) ProofFormalization but the field is
    /// empty — typically because of a manual rewind across the
    /// TheoremStating → ProofFormalization phase boundary, or a state file
    /// imported from a system version that didn't track the field.
    ///
    /// `coarse_dag_nodes` is normally captured ONCE at the phase
    /// transition (engine.rs around the
    /// `state.coarse_dag_nodes = state.live.present_nodes.clone()` line)
    /// and never re-derived. If lost, signature-protection in Restructure
    /// mode silently degrades (the legacy fallback in
    /// `runtime_cli_observations.rs` treats every node as coarse — safe but
    /// over-restrictive: helpers added later under Restructure can never
    /// have their signatures revised), and the reviewer prompt + viewer
    /// can't surface which nodes are actually coarse-protected.
    ///
    /// We could heal by snapshotting current `live.present_nodes`, but that
    /// would over-include helpers added during proof-formalization (they
    /// would be incorrectly marked as coarse forever). Instead, recover the
    /// authentic value by walking git log of the configured repo:
    /// checkpoint commits write `.trellis-history/supervisor_state.json`
    /// containing the live state, including `coarse_dag_nodes`. The most
    /// recent commit with a populated value is the authoritative snapshot.
    ///
    /// Fails soft: if `repo_path` is unset, the repo isn't a git repo, no
    /// historical commit had a populated value, or any git invocation
    /// errors, this is a no-op (the field stays empty and the legacy
    /// fallback takes over).
    fn heal_coarse_dag_from_git_if_needed(&mut self) {
        if !self.state.coarse_dag_nodes.is_empty() {
            return;
        }
        if self.state.phase.is_theorem_stating_like() {
            return;
        }
        let Some(repo_path) = self.metadata.repo_path.as_deref() else {
            return;
        };
        if let Some(recovered) = recover_coarse_dag_from_git(repo_path) {
            if !recovered.is_empty() {
                self.state.coarse_dag_nodes = recovered;
            }
        }
    }

    pub fn load_or_initialize(
        paths: RuntimePaths,
        initial_state: ProtocolState,
    ) -> Result<Self, RuntimeError> {
        if paths.state_path.exists() {
            Self::load(paths)
        } else {
            Self::initialize(paths, initial_state)
        }
    }

    pub fn state(&self) -> &ProtocolState {
        &self.state
    }

    pub fn metadata(&self) -> &RuntimeMetadata {
        &self.metadata
    }

    /// Run a one-shot post-load state migration. The closure receives a
    /// mutable reference to the loaded `ProtocolState`; it must be
    /// idempotent (running twice is a no-op). When the closure returns
    /// `Ok(true)`, this method persists the mutated state to disk so the
    /// migration is durable across restarts. Returns `Ok(false)` if the
    /// closure reports no mutation. Errors from the closure are surfaced
    /// as `RuntimeError::InvalidRuntimeState`.
    ///
    /// Used by `bin/runtime_cli.rs` to run schema migrations after `load`
    /// but before the kernel begins servicing requests. The closure runs
    /// before the first dispatch, so any in-memory mutations it makes are
    /// visible to all subsequent state queries.
    pub fn try_post_load_state_migration<F>(&mut self, migrate: F) -> Result<bool, RuntimeError>
    where
        F: FnOnce(&mut ProtocolState) -> Result<bool, String>,
    {
        let mutated = migrate(&mut self.state).map_err(RuntimeError::InvalidRuntimeState)?;
        if mutated {
            self.persist_state()?;
        }
        Ok(mutated)
    }

    /// Rebuild a request that the pure engine issued immediately before a
    /// post-step, journal-authorized state migration.  Trust-v1 conditional
    /// candidate activation is the motivating case: the next request has not
    /// left the runtime yet, but its graph/blocker projection and Worker base
    /// snapshot were captured before the journal-selected node existed.
    ///
    /// Recomputing the same request id/kind, reapplying execution hints, and
    /// recapturing `active_worker_base` keeps the persisted request, returned
    /// `IssueRequest`, and authoritative repository bytes atomic at the
    /// dispatch boundary.
    pub fn refresh_in_flight_after_external_state_change(
        &mut self,
    ) -> Result<Option<crate::model::WrapperRequest>, RuntimeError> {
        if self.state.in_flight_request.is_none() {
            self.state
                .validate()
                .map_err(RuntimeError::InvalidRuntimeState)?;
            self.persist_state()?;
            return Ok(None);
        }
        self.refresh_in_flight_request_from_state();
        let mut next_state = self.state.clone();
        self.apply_request_execution_hints_to_state(&mut next_state, true)?;
        next_state
            .validate()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        self.state = next_state;
        self.persist_state()?;
        Ok(self.state.in_flight_request.clone())
    }

    pub fn paths(&self) -> &RuntimePaths {
        &self.paths
    }

    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    /// Per-cycle event-log directory for this runtime, resolved lazily from
    /// `metadata.repo_path` (fallback `<root>/.event-log-fallback`).
    pub fn event_log_dir(&self) -> PathBuf {
        event_log_dir_for(&self.paths.root, &self.metadata)
    }

    pub fn step<A: WrapperAdapter>(
        &mut self,
        adapter: &mut A,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        let mut sink = NoopCheckpointSink;
        self.step_with_checkpoint_sink(adapter, &mut sink)
    }

    pub fn step_with_checkpoint_sink<A: WrapperAdapter, C: CheckpointSink>(
        &mut self,
        adapter: &mut A,
        checkpoint_sink: &mut C,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        if self.state.trust_base.required()
            && self.state.trust_base.package_ready
            && self
                .state
                .trust_base
                .package_authorization_event_hash
                .is_none()
        {
            return Ok(RuntimeStepOutcome {
                status: RuntimeStepStatus::PackageReady,
                event: None,
                commands: vec![],
            });
        }
        if self.state.phase == Phase::Complete || self.state.stage == crate::model::Stage::Complete
        {
            return Ok(RuntimeStepOutcome {
                status: RuntimeStepStatus::Complete,
                event: None,
                commands: vec![],
            });
        }

        // Snapshot pre-step in-memory state for atomicity rollback (used
        // only on checkpoint_sink failure below — see the comment block
        // at the bottom of this function). Captured before any step
        // mutations so a sink failure restores `self.state` and
        // `self.metadata` to the exact "before this step" snapshot,
        // leaving the persisted state file (which has not been
        // overwritten yet) consistent with the unchanged git repo.
        //
        // metadata is included because `record_native_history` and
        // `maybe_clear_worker_history_for_checker_mismatch` mutate
        // `metadata.native_history_kinds` between pre-step capture
        // and the sink call. Without snapshotting metadata, a rolled-
        // back step would leave history-key mutations in place and
        // a re-step's `request_requires_fresh_context` decision could
        // diverge from a fresh-process startup. event_count is NOT
        // snapshotted because `append_event_log` runs after the sink,
        // so a sink failure leaves event_count unchanged.
        let pre_step_state = self.state.clone();
        let pre_step_metadata = self.metadata.clone();

        // Re-run the coarse-DAG heal at every step boundary. It's a no-op
        // once the field is populated (the early-return on
        // `!coarse_dag_nodes.is_empty()` skips the git scan), but acts as
        // a continuous self-heal: if anything ever clears the field
        // mid-run (a future rewind path, a hand-edited state file,
        // whatever), the next step recovers it from git history without
        // needing a supervisor restart.
        self.heal_coarse_dag_from_git_if_needed();

        self.apply_request_dispatch_hints()?;
        let prior_request = self
            .state
            .in_flight_request
            .as_ref()
            .map(|req| (req.kind, req.phase));
        // burst-history ledger: snapshot the full dispatch-time
        // WrapperRequest so we can pair it with the upcoming response.
        // Cloning is cheap relative to the response wait that follows.
        let burst_history_request_snapshot = self.state.in_flight_request.clone();
        let event = self.next_event(adapter)?;
        self.step_committed_event_with_checkpoint_sink(
            event,
            checkpoint_sink,
            pre_step_state,
            pre_step_metadata,
            prior_request,
            burst_history_request_snapshot,
        )
    }

    /// Parallel-closure sidecar: drive an externally-constructed event
    /// (the boundary hook's `SidecarClosure`) through the FULL step
    /// machinery — sink-first durability ordering, rollback on sink
    /// failure, state/metadata persist, event-log append. A mechanical
    /// twin of `step_with_checkpoint_sink` with the adapter-derived
    /// `next_event` replaced by the injected event; both share
    /// `step_committed_event_with_checkpoint_sink`, so the existing
    /// path is byte-identical.
    pub fn step_injected_event_with_checkpoint_sink<C: CheckpointSink>(
        &mut self,
        event: ProtocolEvent,
        checkpoint_sink: &mut C,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        if self.state.phase == Phase::Complete || self.state.stage == crate::model::Stage::Complete
        {
            return Ok(RuntimeStepOutcome {
                status: RuntimeStepStatus::Complete,
                event: None,
                commands: vec![],
            });
        }
        let pre_step_state = self.state.clone();
        let pre_step_metadata = self.metadata.clone();
        self.heal_coarse_dag_from_git_if_needed();
        self.apply_request_dispatch_hints()?;
        self.step_committed_event_with_checkpoint_sink(
            event,
            checkpoint_sink,
            pre_step_state,
            pre_step_metadata,
            None,
            None,
        )
    }

    /// Shared tail of `step_with_checkpoint_sink` /
    /// `step_injected_event_with_checkpoint_sink`: applies `event`,
    /// honors commands, runs the sink-first durability barrier, and
    /// persists state + metadata + the event-log line. Extracted
    /// verbatim (mechanical refactor, no behavior change).
    fn step_committed_event_with_checkpoint_sink<C: CheckpointSink>(
        &mut self,
        event: ProtocolEvent,
        checkpoint_sink: &mut C,
        pre_step_state: ProtocolState,
        pre_step_metadata: RuntimeMetadata,
        prior_request: Option<(crate::model::RequestKind, Phase)>,
        burst_history_request_snapshot: Option<crate::model::WrapperRequest>,
    ) -> Result<RuntimeStepOutcome, RuntimeError> {
        let captured_last_invalid = self.capture_last_invalid_snapshot_for_event(&event)?;
        let outcome = apply_event(self.state.clone(), event.clone())?;
        let mut next_state = outcome.state;
        // Audit L-1 — pending side-effect deletes deferred past the
        // checkpoint durability barrier. Engine emits
        // `ProtocolCommand::DeleteLocalClosureRecord` to drop the
        // persisted JSON for an invalidated record; doing the disk
        // delete inline (before sink commit + persist_state) leaves a
        // window where a sink failure rolls back in-memory state to
        // pre_step_state (which holds the record) but the disk file is
        // already gone. Buffering the deletes and flushing only on
        // success closes that window — failed steps leave both memory
        // and disk consistent.
        let mut pending_local_closure_disk_deletes: Vec<NodeId> = Vec::new();
        // #54: kernel-emitted RestoreWorktree* commands replace the
        // event-shape-driven restore. Each variant maps to its own runtime
        // method; commands are processed in order so any restore happens
        // before subsequent commands (CommitCheckpoint, IssueRequest).
        for command in &outcome.commands {
            match command {
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase => {
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "repo worktree restore required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    // This rollback is intentionally limited to the exact
                    // semantic source surfaces writable by a Worker burst.
                    // Falling back to a repo-wide HEAD reset can erase
                    // accepted, uncheckpointed kernel writes (Dormant flips,
                    // assumption registries, manifests, and similar state).
                    // A missing/corrupt snapshot therefore fails closed.
                    self.restore_repo_worktree_to_active_worker_base(repo_path)?;
                }
                ProtocolCommand::RestoreWorktreeToHead => {
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "repo worktree restore required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    self.restore_repo_worktree_to_head(repo_path)?;
                }
                ProtocolCommand::RestoreWorktreeToLastClean => {
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "repo worktree restore required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    // Process memory (spec §7): the reviewer decision that
                    // triggered this LastClean carries the carry-forward
                    // flag. Non-review triggers keep the spec default
                    // (preserve).
                    let preserve_process_memory = match &event {
                        ProtocolEvent::WrapperResponse {
                            response: WrapperResponse::Review(review),
                        } => review.preserve_process_memory,
                        _ => true,
                    };
                    self.restore_repo_worktree_to_last_clean(repo_path, preserve_process_memory)?;
                }
                ProtocolCommand::RestoreTheoremStatingNodeAndPruneOrphans { node } => {
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "theorem-stating node reset required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    if let Err(err) = self.restore_theorem_stating_node_and_prune_orphans(
                        repo_path,
                        &mut next_state,
                        node,
                    ) {
                        if let Err(rollback_err) = self.restore_repo_worktree_to_head(repo_path) {
                            eprintln!(
                                "trellis: theorem-stating node reset failed ({err}); rollback to HEAD also failed: {rollback_err}"
                            );
                        }
                        return Err(err);
                    }
                }
                ProtocolCommand::DeleteLocalClosureRecord { node } => {
                    // Audit L-1 (disk durability ordering): defer the
                    // disk delete until AFTER the checkpoint sink
                    // commits + state.json persists. If the sink fails
                    // we restore in-memory state from pre_step_state,
                    // which still holds the record; deleting the disk
                    // file early would leave state.json (or its
                    // rollback) carrying a record whose persisted JSON
                    // is gone, forcing the next migration to re-probe.
                    // Buffering preserves the original semantic ("the
                    // engine wants this record's disk file gone") but
                    // gates it on the durability barrier so a failed
                    // step is fully rolled back.
                    pending_local_closure_disk_deletes.push(node.clone());
                }
                ProtocolCommand::WriteHaltSentinel { reason } => {
                    // Circuit-breaker: write `.trellis-stop-after-checkpoint`
                    // to the supervisor repo so the outer driver halts at
                    // the next checkpoint boundary. We surface the reason
                    // both inside the sentinel and to stderr so an operator
                    // diagnosing the halt doesn't have to scrape logs.
                    // Timestamp uses SystemTime so we don't pull in a new
                    // chrono dependency for a single timestamp.
                    if let Some(repo_path) = self.metadata.repo_path.as_deref() {
                        let stop_file = repo_path.join(".trellis-stop-after-checkpoint");
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs().to_string())
                            .unwrap_or_else(|_| "unknown".to_string());
                        let payload = format!(
                            "[kernel circuit-breaker] {reason}\n\
                             Written by trellis_runtime_cli at unix_ts={ts}.\n",
                        );
                        if let Err(err) = std::fs::write(&stop_file, &payload) {
                            eprintln!(
                                "trellis: failed to write halt sentinel at {}: {err}",
                                stop_file.display()
                            );
                        } else {
                            eprintln!(
                                "trellis: circuit-breaker halt sentinel written to {}; supervisor will exit at next checkpoint boundary.",
                                stop_file.display()
                            );
                        }
                    } else {
                        eprintln!(
                            "trellis: circuit-breaker tripped ({reason}) but runtime metadata is missing repo_path; cannot write halt sentinel."
                        );
                    }
                }
                ProtocolCommand::SyncTabletRootForPaperTargets { node_names } => {
                    // Paper-target umbrella sync at PF→Cleanup (2026-05-29):
                    // rewrite `<repo>/Tablet.lean` to import the resolved
                    // covering-node set ∪ {Preamble}. The legacy
                    // `sync_tablet_root_from_repo` API is retained for
                    // setup_repo.sh + TheoremStating-reset hot paths;
                    // this command honors the supervisor's PF→Cleanup
                    // boundary specifically.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "paper-target tablet root sync required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::tablet_root::sync_tablet_root(repo_path, node_names)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::FlipDormantDecideFiles {
                    newly_live,
                    newly_dormant,
                } => {
                    // PV dormant store: the engine flipped a `Decide` pair's
                    // polarity. Move the now-live side `Dormant/ → Tablet/` and
                    // the now-dormant side `Tablet/ → Dormant/` so the supervisor's
                    // next observation re-derives `present_nodes` from `Tablet/`
                    // with the new live node present and the old one absent
                    // (the `present_nodes = scan(Tablet/)` chokepoint propagates
                    // the change to every lane). Atomic + crash-safe: see
                    // `dormant_store::flip_decide_pair_on_disk`.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "dormant decide flip required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::dormant_store::flip_decide_pair_on_disk(
                        repo_path,
                        newly_live,
                        newly_dormant,
                    )
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::RecordProposedAssumption { record } => {
                    // PV under-model (Slice 2): the assumptions lane passed a
                    // worker-authored `C` — record it `status:"pending"` in
                    // `PROPOSED_ASSUMPTIONS.json`. The Lean/NL blocks already
                    // live in `Tablet/Assumptions.{lean,tex}` and have passed
                    // ordinary NodeCorr. While pending it is NOT in
                    // `APPROVED_AXIOMS.json`, so the closure gate fails closed
                    // on any proof reaching it.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "record proposed assumption required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::assumptions_registry::record_proposed(repo_path, record.clone())
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::RejectStagedAssumption {
                    assumption_id,
                    reason: _,
                } => {
                    // PV under-model (Slice 2): the assumptions lane rejected a
                    // staged candidate before it became a pending proposal.
                    // Remove its marked Lean/NL blocks from
                    // `Tablet/Assumptions.{lean,tex}`.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "reject staged assumption required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::assumptions_registry::remove_staged_assumption_blocks(
                        repo_path,
                        assumption_id,
                    )
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::ProjectAllPendingAssumptions => {
                    // PV under-model (Slice 2): the operator ratified the batch
                    // at the AssumptionReview gate — project every pending `C`
                    // into the trust sinks (APPROVED_AXIOMS.json global and
                    // tcb_manifest.json disclosure) so it becomes usable, then
                    // clear it from the proposed file. The staged Lean/NL
                    // blocks remain in Tablet/Assumptions.{lean,tex}.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "project approved assumptions required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::assumptions_registry::project_all_pending(repo_path)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::RejectAllPendingAssumptions { reason } => {
                    // PV under-model (Slice 2): the operator declined the batch —
                    // mark every pending `C` rejected and remove its staged
                    // blocks (the parked targets route via ordinary proving on
                    // the reviewer's next turn).
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "reject pending assumptions required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::assumptions_registry::reject_all_pending(repo_path, reason)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::RenderAssumptionsReview => {
                    // PV under-model (Slice 2): (re)render `ASSUMPTIONS_REVIEW.md`
                    // from the pending entries so the operator sees the verbatim
                    // batch at the AssumptionReview gate.
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "render assumptions review required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::assumptions_registry::render_review(repo_path)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::ApplyProcessMemoryOperations { ops } => {
                    // Process memory (spec §6): materialize the accepted
                    // audit's entry files + regenerate INDEX.md. Runs
                    // BEFORE the durability barrier below, so the
                    // mutations land in the same checkpoint commit as the
                    // state they were derived from (the checkpoint hook's
                    // `git add -A` picks up the tracked directory).
                    let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "process-memory apply required but runtime metadata is missing repo_path".into(),
                        )
                    })?;
                    crate::process_memory::apply_file_ops(repo_path, ops)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                }
                ProtocolCommand::CommitTrustGateDecision {
                    gate_episode_id,
                    choice,
                    actor_authentication_receipt,
                } => {
                    self.commit_trust_gate_decision(
                        &mut next_state,
                        gate_episode_id,
                        *choice,
                        actor_authentication_receipt,
                    )?;
                }
                ProtocolCommand::IssueRequest { .. } | ProtocolCommand::CommitCheckpoint => {}
            }
        }
        self.maybe_clear_worker_history_for_checker_mismatch(&event);
        self.apply_request_execution_hints_to_state(&mut next_state, true)?;
        if let Some(repo_path) = self.metadata.repo_path.as_deref() {
            crate::dormant_store::validate_configured_decide_layout(repo_path, &next_state)
                .map_err(RuntimeError::InvalidRuntimeState)?;
        }
        self.state = next_state;
        self.update_last_invalid_for_event(&event, captured_last_invalid.as_deref())?;
        if matches!(event, ProtocolEvent::WrapperResponse { .. }) {
            if let Some((kind, phase)) = prior_request {
                if self.should_record_native_history_for_event(&event, kind) {
                    self.record_native_history(kind, phase);
                }
            }
            // Burst-history ledger append is deferred until after the
            // checkpoint sink and the durable persist_state /
            // append_event_log calls below — see the post-persistence
            // hook for the actual append. Rationale: if the checkpoint
            // sink fails (lines below), in-memory state rolls back to
            // pre_step_state, and we don't want burst-history.jsonl to
            // carry a row for a response the runtime didn't durably
            // commit. The persist_state / append_event_log calls below
            // are the durability barrier; the append happens after.
        }
        // Atomicity (audit): for steps that emit CommitCheckpoint, the
        // engine has already called `commit_live()` which mutated
        // `state.committed_*`, `state.last_clean_*`, `has_ever_been_clean`,
        // and `last_clean_verifier_mirror_ready`. Persisting state BEFORE
        // running the checkpoint sink (which performs the git commit + tag
        // creation) leaves a hazard: if the sink fails, the on-disk state
        // file claims a checkpoint exists but git has no corresponding
        // commit/tag. On the next load:
        //   - LastCommit's `git reset --hard HEAD` lands on the OLD commit
        //     (the new one was supposed to be created by the failed sink).
        //   - LastClean's `git reset --hard supervisor2/clean-N` picks the
        //     PREVIOUS clean tag; the `last_clean_*` mirrors point at a
        //     state that doesn't match the tag.
        // Fix: run the sink FIRST. On success → persist state/event log
        // (everything consistent). On failure → restore in-memory state
        // from the pre-step clone and propagate the error; state file
        // remains at the prior generation, so next startup loads a state
        // consistent with the unchanged git.
        let has_checkpoint = outcome
            .commands
            .iter()
            .any(|command| matches!(command, ProtocolCommand::CommitCheckpoint));
        if has_checkpoint {
            // persist_checkpoint writes a derived/cache file
            // (paths.checkpoint_path) that the runtime never reads back on
            // load — no rollback needed. Sink failure is the failure to
            // worry about.
            let checkpoint = match self.persist_checkpoint() {
                Ok(c) => c,
                Err(e) => {
                    self.state = pre_step_state;
                    self.metadata = pre_step_metadata;
                    return Err(e);
                }
            };
            let payload = self.checkpoint_hook_payload(checkpoint, &outcome.commands);
            let is_clean_checkpoint = payload.is_clean;
            if let Err(sink_err) = checkpoint_sink.commit(&payload) {
                self.state = pre_step_state;
                self.metadata = pre_step_metadata;
                return Err(RuntimeError::CheckpointSink(sink_err));
            }
            // Bug 2: record the durable commit pointer for the LastClean
            // rewind target. The checkpoint hook (sink) has now committed
            // the clean checkpoint and written its `supervisor2/clean-*`
            // tag, so HEAD is the exact commit the just-snapshotted
            // `last_clean_*` mirrors correspond to. Persisting the SHA in
            // state lets `restore_repo_worktree_to_last_clean` rewind by
            // commit pointer instead of the non-monotonic lexical-max tag.
            // Best-effort: a rev-parse failure leaves the pointer at its
            // prior value and the rewind falls back to ancestor-of-HEAD
            // tag selection. Set BEFORE persist_state so it lands durably.
            if is_clean_checkpoint {
                if let Some(repo_path) = self.metadata.repo_path.as_deref() {
                    if let Some(sha) = git_head_sha(repo_path) {
                        self.state.last_clean_commit = Some(sha);
                    }
                }
            }
        }
        self.persist_state()?;
        self.persist_metadata()?;
        self.append_event_log(&event, &outcome.commands)?;
        // Audit L-1 — flush deferred local-closure record disk deletes
        // now that the state file durably reflects the in-memory
        // tombstones. Earlier in the step the engine emitted
        // `ProtocolCommand::DeleteLocalClosureRecord` for each
        // invalidated record; the inline buffer ensures we never delete
        // a JSON file whose corresponding record is still referenced by
        // the previous-generation state.json. Idempotent: re-running a
        // delete on an absent file is a no-op (handled by
        // `delete_persisted_local_closure_record`).
        for node in &pending_local_closure_disk_deletes {
            delete_persisted_local_closure_record(&self.paths.root, node);
        }
        // Burst-history ledger append (deferred to here so the ledger
        // never gets a row for a response the runtime didn't durably
        // commit). At this point: checkpoint sink (if any) succeeded,
        // persist_state succeeded, append_event_log succeeded. Any
        // failure above this point either rolled back in-memory state
        // (checkpoint branch) or propagated an error before reaching
        // here. Best-effort: errors inside `append` are swallowed so
        // a telemetry I/O hiccup never masks a successful step.
        if matches!(event, ProtocolEvent::WrapperResponse { .. }) {
            if let (Some(repo_path), Some(request), ProtocolEvent::WrapperResponse { response }) = (
                self.metadata.repo_path.as_deref(),
                burst_history_request_snapshot.as_ref(),
                &event,
            ) {
                crate::burst_history::append(repo_path, request, response);
            }
        }
        Ok(RuntimeStepOutcome {
            status: RuntimeStepStatus::Transitioned,
            event: Some(event),
            commands: outcome.commands,
        })
    }

    fn next_event<A: WrapperAdapter>(
        &self,
        adapter: &mut A,
    ) -> Result<ProtocolEvent, RuntimeError> {
        if self.state.trust_base.required()
            && self.state.trust_base.package_ready
            && self
                .state
                .trust_base
                .package_authorization_event_hash
                .is_some()
        {
            return Ok(ProtocolEvent::FinalizeAuthorizedPackage);
        }
        if self.state.stage == crate::model::Stage::Start && self.state.in_flight_request.is_none()
        {
            return Ok(ProtocolEvent::StartCycle);
        }
        let Some(request) = self.state.in_flight_request.as_ref() else {
            return Err(RuntimeError::InvalidRuntimeState(
                "no in-flight request available for current stage".into(),
            ));
        };
        let response = adapter.dispatch(request).map_err(RuntimeError::Adapter)?;
        Ok(ProtocolEvent::WrapperResponse { response })
    }

    fn persist_state(&self) -> Result<(), RuntimeError> {
        fs::create_dir_all(&self.paths.root)?;
        let data = serde_json::to_string_pretty(&self.state)?;
        fs::write(&self.paths.state_path, data)?;
        Ok(())
    }

    fn persist_metadata(&self) -> Result<(), RuntimeError> {
        fs::create_dir_all(&self.paths.root)?;
        let data = serde_json::to_string_pretty(&self.metadata)?;
        fs::write(&self.paths.metadata_path, data)?;
        Ok(())
    }

    fn persist_checkpoint(&self) -> Result<RuntimeCheckpoint, RuntimeError> {
        let checkpoint = RuntimeCheckpoint {
            cycle: self.state.cycle,
            phase: self.state.phase,
            gate_kind: self.state.gate_kind,
            active_node: self.state.active_node.clone(),
            committed: self.state.committed.clone(),
            trust_journal_checkpoint: self.state.trust_base.journal_checkpoint.clone(),
        };
        let data = serde_json::to_string_pretty(&checkpoint)?;
        fs::write(&self.paths.checkpoint_path, data)?;
        Ok(checkpoint)
    }

    fn commit_trust_gate_decision(
        &self,
        next_state: &mut ProtocolState,
        gate_episode_id: &str,
        choice: HumanChoice,
        receipt: &serde_json::Value,
    ) -> Result<(), RuntimeError> {
        if !next_state.trust_base.required() || !next_state.trust_base.gate_commit_pending {
            return Err(RuntimeError::InvalidRuntimeState(
                "trust gate commit command emitted outside a pending required-v1 gate".into(),
            ));
        }
        ensure_external_trust_journal_path(&self.metadata)?;
        let journal_path = self
            .metadata
            .trust_journal_path
            .as_deref()
            .ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(
                    "trust-base v1 requires runtime_metadata.trust_journal_path".into(),
                )
            })?;
        let registry = SchemaRegistry::v1()
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        let actor_keys = load_verified_actor_keys(&self.metadata)?;
        let mut journal = TrustJournal::open(journal_path, actor_keys)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        let transaction_id = receipt
            .get("transaction_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(
                    "trust actor receipt lacks transaction_id".into(),
                )
            })?;
        if let Some(committed_hash) = journal.committed_transaction_event_hash(transaction_id) {
            if committed_hash != journal.head().event_hash
                || (choice == HumanChoice::Approve
                    && journal.current_human_approval_event_hash() != Some(committed_hash))
            {
                return Err(RuntimeError::InvalidRuntimeState(
                    "gate transaction was previously committed in a different journal position"
                        .into(),
                ));
            }
            next_state.trust_base.journal_checkpoint = Some(
                journal
                    .checkpoint_binding()
                    .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?,
            );
            next_state.trust_base.current_human_approval_event_hash =
                journal.current_human_approval_event_hash();
            next_state.trust_base.gate_commit_pending = false;
            next_state.trust_base.routine_gate_state = match choice {
                HumanChoice::Approve => crate::model::TrustRoutineGateState::Approved,
                HumanChoice::Feedback => crate::model::TrustRoutineGateState::FeedbackTerminated,
            };
            next_state.trust_base.last_fail_closed_reason = None;
            return Ok(());
        }
        let presentation_path = self
            .metadata
            .trust_gate_presentation_path
            .as_deref()
            .ok_or_else(|| {
                RuntimeError::InvalidRuntimeState(
                    "trust-base v1 requires exact gate presentation bytes".into(),
                )
            })?;
        let presentation_bytes = fs::read(presentation_path).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "failed to read gate presentation {}: {error}",
                presentation_path.display()
            ))
        })?;
        let presentation_hash = tagged_hash(DomainTag::GatePresentation, &presentation_bytes);
        persist_trust_gate_presentation(
            journal_path,
            presentation_hash,
            &presentation_bytes,
        )?;
        let actor_identity = receipt
            .get("actor_identity")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                RuntimeError::InvalidRuntimeState("trust actor receipt lacks actor_identity".into())
            })?
            .to_owned();
        let predecessor = journal.head();
        let subject = match choice {
            HumanChoice::Approve => {
                let authored_semantic_root = next_state
                    .trust_base
                    .authored_semantic_root
                    .ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "trust seed has no authored semantic root".into(),
                        )
                    })?;
                let approved_evidence_tool_input_root = next_state
                    .trust_base
                    .approved_evidence_tool_input_root
                    .ok_or_else(|| {
                        RuntimeError::InvalidRuntimeState(
                            "trust seed has no approved evidence/tool input root".into(),
                        )
                    })?;
                let approval = serde_json::json!({
                    "schema": "trellis-human-approval/v1",
                    "authored_semantic_root": authored_semantic_root,
                    "approved_evidence_tool_input_root": approved_evidence_tool_input_root,
                    "gate_presentation_sha256": presentation_hash,
                    "reviewer_identity": actor_identity,
                    "approved_journal_head": predecessor,
                });
                Subject::CanonicalRecord(
                    AuthoritativeRecord::parse(&registry, approval)
                        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?,
                )
            }
            HumanChoice::Feedback => {
                let feedback = serde_json::json!({
                    "schema": "trellis-advance-gate-feedback/v1",
                    "gate_episode_id": gate_episode_id,
                    "gate_presentation_sha256": presentation_hash,
                    "choice": "feedback",
                });
                let bytes = crate::trust_base::canonical_json_value(&feedback)
                    .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
                Subject::RawArtifact(bytes)
            }
        };
        let event_kind = match choice {
            HumanChoice::Approve => crate::trust_base::EventKind::AdvanceGateApproved,
            HumanChoice::Feedback => crate::trust_base::EventKind::AdvanceGateFeedback,
        };
        let head = journal
            .append(AppendRequest {
                transaction_id: transaction_id.to_owned(),
                event_kind,
                subject_id: gate_episode_id.to_owned(),
                subject,
                actor: JournalActor::Authenticated {
                    role: ActorRole::Reviewer,
                    identity: actor_identity,
                    gate_or_revision_lane_id: gate_episode_id.to_owned(),
                    receipt: receipt.clone(),
                },
                semantic_root_after: journal.semantic_root(),
                derived_result_root_after: journal.derived_result_root(),
                authorization: None,
            })
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        next_state.trust_base.journal_checkpoint = Some(
            journal
                .checkpoint_binding()
                .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?,
        );
        next_state.trust_base.current_human_approval_event_hash =
            journal.current_human_approval_event_hash();
        if choice == HumanChoice::Approve
            && next_state.trust_base.current_human_approval_event_hash != Some(head.event_hash)
        {
            return Err(RuntimeError::InvalidRuntimeState(
                "advance approval did not become the journal's current approval".into(),
            ));
        }
        next_state.trust_base.gate_commit_pending = false;
        next_state.trust_base.routine_gate_state = match choice {
            HumanChoice::Approve => crate::model::TrustRoutineGateState::Approved,
            HumanChoice::Feedback => crate::model::TrustRoutineGateState::FeedbackTerminated,
        };
        next_state.trust_base.last_fail_closed_reason = None;
        Ok(())
    }

    fn append_event_log(
        &mut self,
        event: &ProtocolEvent,
        commands: &[ProtocolCommand],
    ) -> Result<(), RuntimeError> {
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let cycle = self.state.cycle;
        // Defensive: every appended record carries `state.cycle`, and no
        // cycle-0 / pre-cycle events exist (start_cycle increments the cycle
        // before the first append). A cycle==0 here would mean the writer
        // got ahead of the engine's cycle bump — fail loud rather than write
        // a `cycle-000000.jsonl` file that breaks the dense-index invariant.
        if cycle == 0 {
            return Err(RuntimeError::InvalidRuntimeState(
                "refusing to append event log record with cycle==0".into(),
            ));
        }
        let record = EventLogRecord {
            index: self.event_count,
            event: event.clone(),
            commands: commands.to_vec(),
            phase: self.state.phase,
            stage: self.state.stage,
            cycle,
            ts_ms,
        };
        let dir = self.event_log_dir();
        fs::create_dir_all(&dir)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(event_log_cycle_file(&dir, cycle))?;
        serde_json::to_writer(&mut file, &record)?;
        file.write_all(b"\n")?;
        self.event_count += 1;
        Ok(())
    }

    fn checkpoint_hook_payload(
        &self,
        checkpoint: RuntimeCheckpoint,
        commands: &[ProtocolCommand],
    ) -> CheckpointHookPayload {
        let is_clean = self.state.clean_checkpoint_ready();
        CheckpointHookPayload {
            root: self.paths.root.clone(),
            state_path: self.paths.state_path.clone(),
            event_log_dir: self.event_log_dir(),
            checkpoint_path: self.paths.checkpoint_path.clone(),
            metadata_path: self.paths.metadata_path.clone(),
            metadata: self.metadata.clone(),
            state: self.state.clone(),
            checkpoint,
            commands: commands.to_vec(),
            event_count: self.event_count,
            is_clean,
        }
    }

    fn apply_request_dispatch_hints(&mut self) -> Result<(), RuntimeError> {
        let mut next_state = self.state.clone();
        self.apply_request_execution_hints_to_state(&mut next_state, false)?;
        self.state = next_state;
        Ok(())
    }

    #[cfg(test)]
    fn apply_request_execution_hints(&mut self) -> Result<(), RuntimeError> {
        let mut next_state = self.state.clone();
        self.apply_request_execution_hints_to_state(&mut next_state, true)?;
        self.state = next_state;
        Ok(())
    }

    fn apply_request_execution_hints_to_state(
        &self,
        state: &mut ProtocolState,
        prepare_support: bool,
    ) -> Result<(), RuntimeError> {
        let fresh = match state.in_flight_request.as_ref() {
            Some(request) => {
                self.request_requires_fresh_context(request.kind)
                    || matches!(
                        request.worker_context.next_context_mode,
                        crate::model::WorkerContextMode::Fresh
                    )
            }
            None => false,
        };
        if let Some(request) = state.in_flight_request.as_mut() {
            request.fresh_context = fresh;
            // Sidecar queue redesign (Q6/A4): resolve the prompt
            // ADVERTISEMENT flag from the config block, BEFORE the
            // prompt-contract population below renders the payload /
            // fragment list. The verifier-bindings precedent:
            // `metadata.config_path` is read fresh on every
            // issue/reissue, so enablement is a config edit — with the
            // same accepted caveat that a mid-flight config flip
            // changes advertisement between issue and reissue (the
            // VALIDATED queue fields stay state-derived and
            // byte-stable regardless).
            if request.kind == crate::model::RequestKind::Review {
                if let Some(config_path) = self.metadata.config_path.as_deref() {
                    let cfg = crate::sidecar::load_sidecar_runtime_config(config_path)
                        .map_err(RuntimeError::InvalidRuntimeState)?;
                    request.sidecar_advertise_queue_fields = cfg.is_some();
                } else {
                    request.sidecar_advertise_queue_fields = false;
                }
            } else {
                request.sidecar_advertise_queue_fields = false;
            }
            // Process memory (spec §5): same pass, same precedent —
            // resolve the `memory_challenges` ADVERTISEMENT flag from
            // `process-memory/` on disk, which only the runtime can
            // read. Worker and Review are the two contracts that carry
            // the channel; every other kind stays off the wire so
            // memory-less runs keep byte-identical requests.
            request.process_memory_active = matches!(
                request.kind,
                crate::model::RequestKind::Worker | crate::model::RequestKind::Review
            ) && self
                .metadata
                .repo_path
                .as_deref()
                .is_some_and(crate::process_memory::has_active_entries);
            crate::populate_request_prompt_contracts(request, self.metadata.repo_path.as_deref());
            if matches!(
                request.kind,
                crate::model::RequestKind::Paper
                    | crate::model::RequestKind::Corr
                    | crate::model::RequestKind::Sound
            ) {
                let config_path = self.metadata.config_path.as_deref().ok_or_else(|| {
                    RuntimeError::InvalidRuntimeState(
                        "runtime is missing config_path for verifier lane binding resolution"
                            .into(),
                    )
                })?;
                let bindings = crate::resolve_request_verifier_bindings(config_path, request)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                request.paper_verify_lane_bindings = bindings.paper_verify_lane_bindings;
                request.corr_verify_lane_bindings = bindings.corr_verify_lane_bindings;
                request.sound_verify_lane_bindings = bindings.sound_verify_lane_bindings;
            } else {
                request.paper_verify_lane_bindings.clear();
                request.corr_verify_lane_bindings.clear();
                request.sound_verify_lane_bindings.clear();
            }
            if matches!(
                request.kind,
                crate::model::RequestKind::Worker
                    | crate::model::RequestKind::Review
                    | crate::model::RequestKind::Audit
                    | crate::model::RequestKind::StuckMathAudit
            ) {
                let config_path = self.metadata.config_path.as_deref().ok_or_else(|| {
                    RuntimeError::InvalidRuntimeState(
                        "runtime is missing config_path for actor binding resolution".into(),
                    )
                })?;
                let bindings = crate::resolve_request_actor_bindings(config_path, request)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
                request.worker_binding = bindings.worker_binding;
                request.reviewer_binding = bindings.reviewer_binding;
                request.stuck_math_audit_binding = bindings.stuck_math_audit_binding;
            } else {
                request.worker_binding = crate::BridgeActorBinding::default();
                request.reviewer_binding = crate::BridgeActorBinding::default();
                request.stuck_math_audit_binding = crate::BridgeActorBinding::default();
            }
            if prepare_support && request.runtime_support_required {
                let Some(repo_path) = self.metadata.repo_path.as_deref() else {
                    return Err(RuntimeError::InvalidRuntimeState(
                        "support-required request missing repo_path metadata".into(),
                    ));
                };
                crate::ensure_tablet_support_available(repo_path, &request.current_present_nodes)
                    .map_err(RuntimeError::InvalidRuntimeState)?;
            }
            if prepare_support {
                self.capture_active_worker_base_for_request(request)?;
            }
        }
        Ok(())
    }

    fn restore_repo_worktree_to_head(&self, repo_path: &Path) -> Result<(), RuntimeError> {
        restore_worktree_to_head(repo_path)
    }

    /// List `supervisor2/clean-*` tags in the given repo, sorted
    /// newest-first. Shared between `validate_last_clean_tag_consistency`
    /// (load-time atomicity check, audit Option C) and
    /// `restore_repo_worktree_to_last_clean` (runtime LastClean apply).
    ///
    /// Returns:
    /// - `Ok(vec)` — git ran successfully (`status.success()`); `vec`
    ///   contains the trimmed non-empty tag names in newest-first order.
    ///   May be empty if the repo legitimately has no clean tags.
    /// - `Err(reason)` — git invocation failed entirely (binary missing,
    ///   spawn error) OR git exited non-zero (repo not a git repo,
    ///   permission error, etc.). `reason` captures stderr + exit code
    ///   (or the io error message) for operator triage. Callers must
    ///   distinguish "git unavailable" from "tags listed cleanly with
    ///   empty result" — the validator soft-no-ops on `Err` (defers to
    ///   downstream paths that need git for proper context), the
    ///   LastClean apply errs hard on `Err` and includes `reason` in
    ///   the surfaced message.
    fn list_supervisor_clean_tags(repo_path: &Path) -> Result<Vec<String>, String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["tag", "--list", "supervisor2/clean-*", "--sort=-refname"])
            .output()
            .map_err(|err| format!("git tag spawn failed: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "git tag exited with code {:?}; stderr={:?}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Resolve the commit-ish a LastClean reset should rewind to.
    ///
    /// Bug 2 (designs incident 2026-06-26): selection MUST be
    /// commit-pinned / ancestor-of-HEAD — never the lexically-highest
    /// `supervisor2/clean-{event_count}` tag. `event_count` (the tag
    /// suffix) is the per-cycle event-log line count, which is
    /// NON-monotonic across event-log segmentation and prior rewinds,
    /// so the old `--sort=-refname` + `.first()` could pick an ancient
    /// checkpoint (the incident's 226-cycle catastrophic rollback).
    ///
    /// Selection (both candidates must be ANCESTORS of HEAD; pick the one
    /// NEAREST to HEAD = fewest commits behind = most recent):
    /// 1. `state.last_clean_commit` (the durable commit pointer recorded
    ///    when the clean checkpoint was written) — the exact commit the
    ///    `last_clean_*` logical mirrors correspond to.
    /// 2. The `supervisor2/clean-*` tag that is an ANCESTOR of HEAD and
    ///    NEAREST to HEAD. On the linear checkpoint history this is the
    ///    greatest-cycle clean checkpoint reachable from HEAD.
    ///
    /// We take the MORE-RECENT (nearer-HEAD) of the two rather than
    /// unconditionally preferring the commit pointer (round-2 audit
    /// fold-in): if a `rev-parse HEAD` failed during a later clean
    /// checkpoint, `last_clean_commit` can lag behind a newer clean tag,
    /// and the newer tag is the correct (less-lossy) target. Ties go to
    /// the commit pointer (it is the precise mirror match). If neither
    /// yields a HEAD-ancestor target → `Err`. We never fall back to a
    /// non-ancestor or a stale higher-event_count tag.
    fn resolve_last_clean_commitish(
        &self,
        repo_path: &Path,
        tags: &[String],
    ) -> Result<String, RuntimeError> {
        // Track the nearest-HEAD candidate (smallest commits-behind wins).
        let mut best: Option<(u64, String)> = None;
        let mut consider = |behind: u64, commitish: String| {
            let take = match &best {
                Some((best_behind, _)) => behind < *best_behind,
                None => true,
            };
            if take {
                best = Some((behind, commitish));
            }
        };
        // (1) Commit pointer recorded in state.
        if let Some(commit) = self.state.last_clean_commit.as_deref() {
            let commit = commit.trim();
            if !commit.is_empty() && git_is_ancestor_of_head(repo_path, commit) {
                // When the distance is computable we compare it against the
                // tags; when it is not (rev-list error) we fall back to 0 so a
                // resolvable HEAD-ancestor pointer still wins — the round-1
                // "prefer the pointer" behaviour, retained only for the
                // unknown-distance case.
                let behind = git_commits_behind_head(repo_path, commit).unwrap_or(0);
                consider(behind, commit.to_string());
            } else if !commit.is_empty() {
                eprintln!(
                    "trellis: recorded last_clean_commit={commit} is not an ancestor of HEAD \
                     in {} — falling back to ancestor-of-HEAD clean-tag selection.",
                    repo_path.display()
                );
            }
        }
        // (2) Nearest HEAD-ancestor clean tag.
        for tag in tags {
            let tag = tag.as_str();
            if !git_is_ancestor_of_head(repo_path, tag) {
                continue;
            }
            let Some(behind) = git_commits_behind_head(repo_path, tag) else {
                continue;
            };
            consider(behind, tag.to_string());
        }
        if let Some((_, commitish)) = best {
            return Ok(commitish);
        }
        // (3) No safe target.
        Err(RuntimeError::InvalidRuntimeState(format!(
            "LastClean reset requested but no safe rewind target found in {}: \
             state.last_clean_commit is {} and none of the {} supervisor2/clean-* \
             tag(s) is an ancestor of HEAD. Refusing to rewind to a non-ancestor / \
             stale tag (Bug 2 guard). Investigate the checkpoint history and resolve \
             manually.",
            repo_path.display(),
            self.state
                .last_clean_commit
                .as_deref()
                .map(|c| format!("set to `{c}` (not a HEAD ancestor)"))
                .unwrap_or_else(|| "absent".to_string()),
            tags.len(),
        )))
    }

    /// Atomicity validator (audit, Option C): on `load()`, verify that
    /// the loaded state's claim "last_clean mirrors are ready" is
    /// consistent with the git repo actually having at least one
    /// `supervisor2/clean-*` tag. The two can diverge if the
    /// checkpoint sink succeeded at producing the in-memory commit but
    /// failed before writing the clean tag (or if a process crash
    /// landed between the sink's commit and the kernel's
    /// `persist_state` write — the post-A reorder narrows that window
    /// from "any sink failure" to "process kill in microseconds").
    /// Without this check, the divergence would surface only on a
    /// reviewer-driven LastClean rewind — at which point
    /// `restore_repo_worktree_to_last_clean` would either fail loudly
    /// (no tag) or silently rewind to a STALE tag whose state doesn't
    /// match the loaded `last_clean_*` mirrors. Better to fail at
    /// load with an actionable error.
    ///
    /// Returns Err(InvalidRuntimeState) when the state is internally
    /// inconsistent — specifically when git ran cleanly AND the repo
    /// has zero `supervisor2/clean-*` tags despite state claiming
    /// readiness. Returns Ok(()) for any benign case (no repo_path,
    /// mirrors not ready, OR git invocation failed entirely so we
    /// can't tell — bridge's existing error paths surface real git
    /// corruption with proper context when they actually need git).
    fn validate_last_clean_tag_consistency(&self) -> Result<(), RuntimeError> {
        if !self.state.last_clean_verifier_mirror_ready {
            return Ok(());
        }
        let Some(repo_path) = self.metadata.repo_path.as_deref() else {
            return Ok(());
        };
        // Soft no-op when git is unavailable (helper returns Err for
        // binary-missing, repo-not-git, permission errors, etc.) —
        // bridge's existing runtime paths surface real corruption when
        // they actually need git, with proper context. The validator's
        // job is the narrower one: catch the specific divergence where
        // git ran cleanly AND the repo has zero clean tags.
        let tags = match Self::list_supervisor_clean_tags(repo_path) {
            Ok(t) => t,
            Err(_) => return Ok(()),
        };
        if !tags.is_empty() {
            return Ok(());
        }
        Err(RuntimeError::InvalidRuntimeState(format!(
            "loaded state at cycle={} has last_clean_verifier_mirror_ready=true \
             (mirror fields populated, has_ever_been_clean={}) but the git repo \
             at {} has zero `supervisor2/clean-*` tags. The state file is ahead \
             of git — most likely a checkpoint sink failure or process crash \
             between sink success and state persistence. LastClean reset cannot \
             land safely (no tag to rewind to). Investigate {}/.trellis-history \
             for the most recent successful checkpoint and either roll back the \
             state file or recreate the missing tag(s).",
            self.state.cycle,
            self.state.has_ever_been_clean,
            repo_path.display(),
            repo_path.display(),
        )))
    }

    /// Rewind the repo worktree to the most recent `supervisor2/clean-*`
    /// tag written by `git_checkpoint_hook.py`. These tags mark checkpoints
    /// where `state.global_blockers().is_empty()` at emission time. Returns
    /// an error if no such tag exists — the reviewer should only send
    /// `ResetChoice::LastClean` when `cycles_since_clean >= 1`, and the
    /// allowed-resets gate enforces that, so in practice at least one
    /// clean tag should always exist when this is called.
    ///
    /// Process memory (spec §7): when `preserve_process_memory` is true
    /// (the reviewer default), the pre-rewind HEAD's `process-memory/`
    /// directory is restored after the reset — a LastClean rewind usually
    /// means "this line failed", which is when its refuted-route entries
    /// are most valuable. Not-yet-checkpointed entries (untracked at
    /// rewind time) are additionally spared by a `git clean` exclusion and
    /// join the carry-forward, with `INDEX.md` regenerated over the union.
    /// The §4 monotonicity invariant (files only added or status-flipped
    /// forward) makes this file-level restore the correct union merge.
    /// The next checkpoint commits the carry-forward. When false (poisoned
    /// memory), tracked entries revert with every other tracked file and
    /// untracked ones are swept by the clean; the abandoned committed
    /// entries remain reachable on the `trellis-rewound/*` branch.
    fn restore_repo_worktree_to_last_clean(
        &self,
        repo_path: &Path,
        preserve_process_memory: bool,
    ) -> Result<(), RuntimeError> {
        let start = std::time::Instant::now();
        let tags_result = Self::list_supervisor_clean_tags(repo_path);
        let duration = start.elapsed().as_secs_f64();
        // Telemetry: `ok = git invocation succeeded` (matches pre-fix
        // semantics — Err means the subprocess didn't run cleanly).
        // `stdout_len` is the sum of returned tag bytes + 1 each;
        // off-by-N from raw git stdout bytes but the consumer at
        // trellis/usage_report.py:135-164 only aggregates counts +
        // `ok`/duration, not byte sums for control flow.
        let git_ran = tags_result.is_ok();
        let tags_vec: Vec<String> = match &tags_result {
            Ok(v) => v.clone(),
            Err(_) => Vec::new(),
        };
        crate::check_ledger::append_kind(
            repo_path,
            "git",
            "tag",
            duration,
            git_ran,
            tags_vec.iter().map(|t| t.len() + 1).sum(),
            0,
        );
        let tags_vec = tags_result.map_err(|reason| {
            // Propagate the helper's captured stderr/exit/io error so
            // operators triaging a failed LastClean apply have the
            // actual git failure context, not just a generic message.
            RuntimeError::InvalidRuntimeState(format!(
                "list supervisor2/clean-* tags failed: {reason}"
            ))
        })?;
        if tags_vec.is_empty() && self.state.last_clean_commit.is_none() {
            return Err(RuntimeError::InvalidRuntimeState(
                "LastClean reset requested but no supervisor2/clean-* tag found in repo".into(),
            ));
        }
        // Bug 2: select by commit pointer / nearest HEAD-ancestor tag,
        // never the lexically-highest (potentially stale) tag.
        let target = self.resolve_last_clean_commitish(repo_path, &tags_vec)?;
        let tag = target.as_str();
        // Process memory (spec §7): capture the pre-reset HEAD so the
        // carry-forward below can restore `process-memory/` from the
        // abandoned line. Best-effort capture: without a resolvable HEAD
        // there is nothing to carry forward.
        let pre_rewind_head = if preserve_process_memory {
            git_head_sha(repo_path)
        } else {
            None
        };
        // Preserve the abandoned lineage as a `trellis-rewound/...` branch
        // BEFORE the destructive reset. The branch ref keeps the commits
        // reachable so they can be inspected later (and pushed to the
        // archive remote, which then carries the full history of what was
        // attempted, not just the surviving line). Best-effort: branch
        // creation failure is logged but does not block the reset.
        self.preserve_abandoned_branch_for_rewind(repo_path, tag);
        // Process memory (spec §7): entries materialized since the last
        // checkpoint are still UNTRACKED, so it is the `git clean` — not
        // the reset — that would delete them. With preserve=true (the
        // reviewer default) the sweep spares `process-memory/` and the
        // survivors join the committed carry-forward below; with
        // preserve=false (poisoned memory) the exclusion is intentionally
        // omitted so memory reverts fully to the clean tag (the reset
        // reverts tracked entries, the unexcluded clean removes the
        // untracked ones).
        // Anchored (`/`-prefixed) so only the repo-root process-memory dir
        // is spared; a stray nested `Tablet/process-memory/` is still swept.
        let pm_exclude = format!("/{}", crate::process_memory::PROCESS_MEMORY_DIR);
        let mut clean_command = vec![
            "clean",
            "-fd",
            "-e",
            ".trellis-history/event-log.restore-shield",
        ];
        if preserve_process_memory {
            clean_command.push("-e");
            clean_command.push(pm_exclude.as_str());
        }
        with_event_log_shielded(repo_path, || {
            for command in [vec!["reset", "--hard", tag], clean_command] {
                let start = std::time::Instant::now();
                let output = Command::new("git")
                    .arg("-C")
                    .arg(repo_path)
                    .args(&command)
                    .output();
                let duration = start.elapsed().as_secs_f64();
                let output = match output {
                    Ok(o) => {
                        crate::check_ledger::append_kind(
                            repo_path,
                            "git",
                            command[0],
                            duration,
                            o.status.success(),
                            o.stdout.len(),
                            o.stderr.len(),
                        );
                        o
                    }
                    Err(err) => {
                        crate::check_ledger::append_kind(
                            repo_path, "git", command[0], duration, false, 0, 0,
                        );
                        return Err(err.into());
                    }
                };
                if !output.status.success() {
                    return Err(RuntimeError::InvalidRuntimeState(format!(
                    "restore last-clean worktree failed for `git {}` with exit code {:?}; stdout={:?}; stderr={:?}",
                    command.join(" "),
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                )));
                }
            }
            Ok(())
        })?;
        // Process memory (spec §7): restore `process-memory/` from the
        // pre-rewind HEAD. Tolerates the directory not existing in that
        // commit (pre-migration runs / no entries yet) by probing with
        // `git ls-tree` first; an actual checkout failure is a real
        // error and aborts the step.
        if let Some(sha) = pre_rewind_head.as_deref() {
            restore_process_memory_from_commit(repo_path, sha)?;
        }
        if preserve_process_memory {
            // The carry-forward checkout restores the pre-rewind COMMITTED
            // INDEX.md, which does not list the untracked survivors spared
            // by the clean exclusion above. INDEX.md is derived state;
            // regenerate it from the surviving union of entry files.
            crate::process_memory::regenerate_index(repo_path).map_err(|err| {
                RuntimeError::InvalidRuntimeState(format!(
                    "process-memory index regeneration after LastClean restore failed: {err}"
                ))
            })?;
        }
        // Purge stale .lake/build/lib/lean/Tablet/ artifacts for nodes whose
        // source `.lean` file no longer exists on disk after the rewind. Without
        // this, deleted node oleans persist and Lean resolves imports for nodes
        // that have no current source — i.e. "ghost" imports that pollute the
        // worker/audit semantic view of the tablet (probe.lean compiles against
        // dead code, reviewer/worker reason about deleted declarations). The
        // git clean above doesn't touch .lake/build because it's gitignored.
        purge_stale_tablet_build_artifacts(repo_path);
        Ok(())
    }

    fn restore_theorem_stating_node_and_prune_orphans(
        &self,
        repo_path: &Path,
        state: &mut ProtocolState,
        node: &NodeId,
    ) -> Result<(), RuntimeError> {
        if !state.resettable_theorem_stating_nodes().contains(node) {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "theorem-stating reset requested for non-resettable node `{}`",
                node.as_str()
            )));
        }
        let baseline = recover_theorem_stating_baseline_from_git(repo_path).ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "could not recover theorem-stating baseline checkpoint from git history".into(),
            )
        })?;
        if !baseline.state.live.present_nodes.contains(node) {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "theorem-stating baseline commit {} does not contain node `{}`",
                baseline.commit,
                node.as_str()
            )));
        }

        restore_repo_path_from_git(
            repo_path,
            &baseline.commit,
            &format!("Tablet/{}.lean", node.as_str()),
        )?;
        let restored_lean = fs::read_to_string(
            repo_path
                .join("Tablet")
                .join(format!("{}.lean", node.as_str())),
        )?;
        crate::filespec_split::validate_filespec(&restored_lean, node.as_str()).map_err(|err| {
            RuntimeError::InvalidRuntimeState(format!(
                "theorem-stating baseline commit {} restored Tablet/{}.lean, but it does not satisfy current FILESPEC: {}",
                baseline.commit,
                node.as_str(),
                err
            ))
        })?;
        restore_repo_path_from_git(
            repo_path,
            &baseline.commit,
            &format!("Tablet/{}.tex", node.as_str()),
        )?;

        let present_after_restore = crate::worker_normalization::present_nodes_from_repo(repo_path)
            .map_err(RuntimeError::InvalidRuntimeState)?;
        let deps_after_restore =
            crate::worker_normalization::direct_deps_from_repo(repo_path, &present_after_restore);
        let mut target_claims =
            target_claims_after_theorem_stating_node_restore(state, &baseline.state, node);
        retain_target_claims_for_present(
            &mut target_claims,
            &present_after_restore,
            &state.configured_targets,
        );
        let coverage_after_restore = crate::worker_normalization::coverage_from_claims(
            &state.configured_targets,
            &target_claims,
            &present_after_restore,
        );
        // Challenge-covering nodes root support exactly like
        // paper-covering nodes; without them a cone-clean on a
        // challenge run would sweep the covering declarations (and
        // their support cones) as orphans.
        let mut orphan_roots: std::collections::BTreeSet<NodeId> = coverage_after_restore
            .values()
            .flat_map(|nodes| nodes.iter().cloned())
            .collect();
        let configured_challenge_ids: std::collections::BTreeSet<_> =
            state.configured_challenge_targets.keys().cloned().collect();
        let challenge_coverage_after_restore =
            crate::worker_normalization::challenge_coverage_from_claims(
                &configured_challenge_ids,
                &state.challenge_claims,
                &present_after_restore,
            );
        orphan_roots.extend(
            challenge_coverage_after_restore
                .values()
                .flat_map(|nodes| nodes.iter().cloned()),
        );
        let orphans = ProtocolState::orphan_nodes_for_roots(
            &present_after_restore,
            &orphan_roots,
            &deps_after_restore,
        );
        if orphans.contains(node) {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "theorem-stating reset would make reset node `{}` orphaned; refusing to delete the selected node",
                node.as_str()
            )));
        }
        for orphan in &orphans {
            remove_tablet_node_files(repo_path, orphan)?;
        }

        crate::tablet_support::sync_tablet_support_from_repo(repo_path)
            .map_err(RuntimeError::InvalidRuntimeState)?;
        purge_stale_tablet_build_artifacts(repo_path);

        let paper_approved_for_observation =
            paper_approved_after_theorem_stating_node_restore(state, &baseline.state, node);
        let observed = observe_live_tablet_state_from_repo(
            repo_path,
            state,
            target_claims,
            &paper_approved_for_observation,
            paper_source_path_from_config(self.metadata.config_path.as_deref()).as_deref(),
        )?;
        let mut changed_nodes = orphans.clone();
        changed_nodes.insert(node.clone());
        for old in state
            .live
            .present_nodes
            .difference(&observed.live.present_nodes)
        {
            changed_nodes.insert(old.clone());
        }
        for new in observed
            .live
            .present_nodes
            .difference(&state.live.present_nodes)
        {
            changed_nodes.insert(new.clone());
        }
        state.install_observed_live_tablet_state(
            observed.live,
            observed.node_kinds,
            observed.proof_nodes,
            observed.deps,
            observed.target_claims,
        );
        state.restore_theorem_stating_baseline_for_node(node, &baseline.state);
        let deleted_records = state.prune_local_closure_after_runtime_tablet_reset(&changed_nodes);
        for deleted in deleted_records {
            delete_persisted_local_closure_record(&self.paths.root, &deleted);
        }
        state.commit_live();
        // Sidecar queue redesign §1.2: the deterministic tail prune
        // normally runs inside `apply_event` immediately before
        // `validate()`, but this runtime sweep mutates the live tablet
        // (orphan deletion) OUTSIDE the transition function — a queued
        // node deleted by the orphan sweep would otherwise leave a stale
        // queue entry and fail the queue invariant below (reason
        // `deleted` in the prune log, same as the apply_event path).
        state.prune_sidecar_queue();
        // Same rationale for closure provenance: the orphan sweep
        // deletes live tablet nodes outside the transition function, so
        // a provenance entry for a swept node would outlive the node
        // and fail the provenance invariant below.
        state.prune_closure_provenance();
        state
            .validate()
            .map_err(RuntimeError::InvalidRuntimeState)?;
        Ok(())
    }

    /// Create a `trellis-rewound/{YYYYMMDD-HHMMSS}-to-{tag-suffix}` branch
    /// pointing at the current HEAD, so the soon-to-be-abandoned line stays
    /// reachable after a `git reset --hard` rewind. Quiet best-effort: any
    /// failure is recorded in the check ledger and otherwise swallowed —
    /// preserving history is a nice-to-have, not a precondition for the
    /// rewind itself.
    fn preserve_abandoned_branch_for_rewind(&self, repo_path: &Path, target_tag: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let secs = now as i64;
        // Crude UTC formatter — avoids pulling in chrono. We only need a
        // monotonic-ish, human-readable suffix; precision is irrelevant.
        let day = secs / 86400;
        let day_secs = secs % 86400;
        let hh = day_secs / 3600;
        let mm = (day_secs % 3600) / 60;
        let ss = day_secs % 60;
        // Days since 1970-01-01 → naive Y/M/D split. Good enough for a label.
        let mut year = 1970i64;
        let mut days_left = day;
        loop {
            let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            let in_year = if leap { 366 } else { 365 };
            if days_left < in_year {
                break;
            }
            days_left -= in_year;
            year += 1;
        }
        let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let mdays = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        let mut month = 1i64;
        for &dm in mdays.iter() {
            if days_left < dm {
                break;
            }
            days_left -= dm;
            month += 1;
        }
        let day_of_month = days_left + 1;
        let ts_label = format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            year, month, day_of_month, hh, mm, ss,
        );
        let tag_suffix = target_tag
            .strip_prefix("supervisor2/clean-")
            .unwrap_or(target_tag)
            .replace('/', "-");
        // Disambiguate concurrent rewinds with the abandoned HEAD's short SHA.
        let mut suffix = String::new();
        let head_proc = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["rev-parse", "--short=8", "HEAD"])
            .output();
        if let Ok(o) = head_proc {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !s.is_empty() {
                    suffix = format!("-{}", s);
                }
            }
        }
        let branch_name = format!("trellis-rewound/{}-to-{}{}", ts_label, tag_suffix, suffix);

        // `git branch <name> HEAD` is non-destructive: fails harmlessly if a
        // branch with this exact name already exists. We don't `--force` it
        // because two rewinds at the same second from the same HEAD would
        // produce identical lineage anyway — first writer wins.
        let start = std::time::Instant::now();
        let res = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["branch", &branch_name, "HEAD"])
            .output();
        let duration = start.elapsed().as_secs_f64();
        match res {
            Ok(o) => {
                crate::check_ledger::append_kind(
                    repo_path,
                    "git",
                    "branch",
                    duration,
                    o.status.success(),
                    o.stdout.len(),
                    o.stderr.len(),
                );
            }
            Err(_) => {
                crate::check_ledger::append_kind(repo_path, "git", "branch", duration, false, 0, 0);
            }
        }
    }

    fn restore_repo_worktree_to_active_worker_base(
        &self,
        repo_path: &Path,
    ) -> Result<(), RuntimeError> {
        let manifest_path = self.active_worker_base_manifest_path();
        let manifest: ActiveWorkerBaseManifest = serde_json::from_slice(
            &fs::read(&manifest_path).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "active worker base manifest {} is unavailable: {error}",
                    manifest_path.display()
                ))
            })?,
        )
        .map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "active worker base manifest {} is malformed: {error}",
                manifest_path.display()
            ))
        })?;
        if manifest.schema_version != ACTIVE_WORKER_BASE_SCHEMA_VERSION {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "active worker base manifest {} has unsupported schema_version {}; expected {}",
                manifest_path.display(),
                manifest.schema_version,
                ACTIVE_WORKER_BASE_SCHEMA_VERSION,
            )));
        }

        // Validate the complete pair of snapshots before deleting either live
        // destination. A corrupt nested entry or manifest contradiction must
        // be a non-mutating failure, never a destructive half-restore.
        validate_worker_surface_snapshot(
            &self.active_worker_base_tablet_dir(),
            manifest.tablet_present,
            "Tablet",
        )?;
        validate_worker_surface_snapshot(
            &self.active_worker_base_reference_dir(),
            manifest.reference_present,
            "reference",
        )?;
        restore_worker_surface(
            &self.active_worker_base_tablet_dir(),
            &repo_path.join("Tablet"),
            manifest.tablet_present,
        )?;
        restore_worker_surface(
            &self.active_worker_base_reference_dir(),
            &repo_path.join("reference"),
            manifest.reference_present,
        )?;
        crate::dormant_store::validate_configured_decide_layout(repo_path, &self.state)
            .map_err(RuntimeError::InvalidRuntimeState)?;
        Ok(())
    }

    /// Audit followup #2 (Problem B): SIGHUP-style restart leaves the
    /// worker repo dirty if a partial worker burst mutated `Tablet/`
    /// before the supervisor was killed. The next bridge reissue must
    /// restore the worker repo to the captured `active_worker_base`
    /// snapshot BEFORE rebuilding the acceptance context — otherwise
    /// `before_snapshot` is captured against the post-mutation disk and
    /// the unauthorized edits become baseline rather than candidate
    /// changes. Exposed via the `RestoreActiveWorkerBase` CLI subcommand
    /// for the Python bridge to invoke at the top of `_handle_worker`
    /// when no `.done` artifact is present (i.e., we're about to
    /// relaunch the worker, possibly after a crash).
    ///
    /// Returns `Ok(false)` and is a no-op when:
    ///   - runtime metadata has no `repo_path` (legacy / dry-run state),
    ///   - no in-flight request exists (bridge dispatching a fresh request),
    ///   - the in-flight request is not a Worker request (no Tablet baseline
    ///     to restore for non-worker burst kinds).
    /// Returns `Ok(true)` after a successful restore.
    ///
    /// Returns `Err(InvalidRuntimeState(...))` when the in-flight request
    /// IS a Worker but the `active_worker_base/worker_surfaces.json` snapshot
    /// manifest is missing. This is the dirty-disk-relaunch hazard: the bridge calls
    /// this from `_handle_worker` precisely because a previous worker
    /// burst may have crashed mid-write; if the snapshot it would
    /// rewind to is also gone (interrupted earlier step, manual cleanup,
    /// migration), the bridge cannot establish a clean baseline before
    /// rebuilding `before_snapshot`. Failing loudly here lets the bridge
    /// route via its existing exception handler to a transport_failure
    /// classification, which the kernel then handles via its
    /// transport-attempt budget — rather than silently absorbing dirty
    /// Tablet/ writes into the new acceptance baseline.
    pub fn restore_active_worker_base_for_inflight(&self) -> Result<bool, RuntimeError> {
        let Some(repo_path) = self.metadata.repo_path.as_deref() else {
            return Ok(false);
        };
        let Some(request) = self.state.in_flight_request.as_ref() else {
            return Ok(false);
        };
        if request.kind != crate::model::RequestKind::Worker {
            return Ok(false);
        }
        if !self.active_worker_base_manifest_path().is_file() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "restore_active_worker_base_for_inflight: in-flight Worker \
                 request id={} cycle={} but active_worker_base/worker_surfaces.json \
                 snapshot manifest is missing — cannot establish clean baseline for \
                 worker relaunch",
                request.id, request.cycle,
            )));
        }
        self.restore_repo_worktree_to_active_worker_base(repo_path)?;
        Ok(true)
    }

    /// True for any non-Valid worker response. Three call sites use it:
    ///   1. `event_requires_repo_worktree_restore` /
    ///      `restore_repo_worktree_for_event` → triggers worktree
    ///      rollback to `active_worker_base` so out-of-scope and
    ///      contract-violating disk effects don't leak between attempts.
    ///   2. `capture_last_invalid_snapshot_for_event` → snapshots
    ///      `Tablet/` to a sidecar directory before rollback so the
    ///      worker's WIP is preserved.
    ///   3. `update_last_invalid_for_event` → persists the snapshot +
    ///      metadata to `.trellis-history/worker_state/last_invalid/`
    ///      for the next worker's prompt context.
    ///
    /// Stuck and NeedsRestructure used to be excluded from the rollback
    /// + snapshot paths under the assumption that the worker had
    /// reverted its tablet changes before returning, but that assumption
    /// was never enforced and let a corruption (a worker editing a
    /// sibling file outside its Easy-mode scope) survive across worker
    /// bursts and pollute the next baseline. Treat them the same as
    /// Invalid: capture the WIP, then restore disk to baseline.
    fn worker_response_should_preserve_attempt(response: &crate::model::WorkerResponse) -> bool {
        response.status == ResponseStatus::Malformed
            || matches!(
                response.outcome,
                WorkerOutcome::Invalid
                    | WorkerOutcome::Stuck
                    | WorkerOutcome::NeedsRestructure
                    // PV under-model (Slice 1): a non-progress verdict with no
                    // committed tablet edit — capture WIP, restore baseline,
                    // same as Stuck/NR.
                    | WorkerOutcome::TargetFalseUnderModel
            )
    }

    fn worker_response_has_checker_mismatch(response: &crate::model::WorkerResponse) -> bool {
        response
            .deterministic_rejection_reasons
            .iter()
            .any(|reason| reason.starts_with("authoritative checker mismatch:"))
    }

    fn maybe_clear_worker_history_for_checker_mismatch(&mut self, event: &ProtocolEvent) {
        let ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(response),
        } = event
        else {
            return;
        };
        if !Self::worker_response_has_checker_mismatch(response) {
            return;
        }
        self.metadata
            .native_history_kinds
            .remove(&request_history_key(
                crate::model::RequestKind::Worker,
                self.state.phase,
            ));
    }

    fn should_record_native_history_for_event(
        &self,
        event: &ProtocolEvent,
        kind: crate::model::RequestKind,
    ) -> bool {
        match event {
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(response),
            } if kind == crate::model::RequestKind::Worker => {
                !Self::worker_response_has_checker_mismatch(response)
            }
            _ => true,
        }
    }

    fn capture_last_invalid_snapshot_for_event(
        &self,
        event: &ProtocolEvent,
    ) -> Result<Option<PathBuf>, RuntimeError> {
        let ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(response),
        } = event
        else {
            return Ok(None);
        };
        // Capture the Tablet/ snapshot for any non-Valid outcome — the
        // worker's WIP (whether rejected edits, stuck mid-progress, or
        // needs-restructure abandonment) is on disk and the next worker
        // benefits from seeing it. The kernel will roll the worktree
        // back to active_worker_base after this capture so the WIP
        // doesn't pollute the next worker's baseline; preserving it as
        // a sidecar snapshot is what makes the rollback non-destructive.
        //
        // ALSO capture for a Valid outcome: a Valid response can still be
        // rejected deterministically at apply time (e.g. the live-orphan
        // rule), and that decision isn't known until after apply — by
        // which point the rollback has already destroyed the WIP unless
        // this capture exists (dec2flt request 938, 2026-07-03: a
        // checker-passing restructure was rolled back with the retry
        // prompt pointing at a snapshot that was never written). An
        // ACCEPTED Valid response's capture is discarded in
        // `update_last_invalid_for_event`.
        if !Self::worker_response_should_preserve_attempt(response)
            && !(response.status == ResponseStatus::Ok
                && response.outcome == WorkerOutcome::Valid)
        {
            return Ok(None);
        }
        let repo_path = self.metadata.repo_path.as_deref().ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "invalid worker snapshot capture requires repo_path metadata".into(),
            )
        })?;
        let tablet_dir = repo_path.join("Tablet");
        if !tablet_dir.is_dir() {
            return Ok(None);
        }
        let capture_root = self.paths.root.join("last_invalid_capture");
        if capture_root.exists() {
            fs::remove_dir_all(&capture_root)?;
        }
        let capture_tablet = capture_root.join("Tablet");
        copy_dir_recursive(&tablet_dir, &capture_tablet)?;
        Ok(Some(capture_root))
    }

    fn update_last_invalid_for_event(
        &self,
        event: &ProtocolEvent,
        captured_snapshot_root: Option<&Path>,
    ) -> Result<(), RuntimeError> {
        let repo_path = match self.metadata.repo_path.as_deref() {
            Some(path) => path,
            None => return Ok(()),
        };
        let last_invalid_dir = repo_path
            .join(".trellis-history")
            .join("worker_state")
            .join("last_invalid");
        let last_invalid_tablet = last_invalid_dir.join("Tablet");
        let last_invalid_metadata = last_invalid_dir.join("metadata.json");
        match event {
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(response),
            } => {
                // Runs AFTER `self.state = next_state`, so the engine's
                // apply decision is visible here. A Valid response the
                // engine rejected deterministically (live-orphan rule,
                // etc.) leaves `deterministic_worker_rejection_reasons`
                // non-empty (an accept clears it via
                // `clear_retry_context`) — preserve its WIP exactly like
                // the non-Valid exits.
                let kernel_rejected_valid = response.outcome == WorkerOutcome::Valid
                    && !self.state.deterministic_worker_rejection_reasons.is_empty();
                if Self::worker_response_should_preserve_attempt(response)
                    || kernel_rejected_valid
                {
                    if last_invalid_dir.exists() {
                        fs::remove_dir_all(&last_invalid_dir)?;
                    }
                    if let Some(snapshot_root) = captured_snapshot_root {
                        let captured_tablet = snapshot_root.join("Tablet");
                        if captured_tablet.is_dir() {
                            copy_dir_recursive(&captured_tablet, &last_invalid_tablet)?;
                        }
                    }
                    fs::create_dir_all(&last_invalid_dir)?;
                    let rejection_reasons = if response.deterministic_rejection_reasons.is_empty()
                    {
                        &self.state.deterministic_worker_rejection_reasons
                    } else {
                        &response.deterministic_rejection_reasons
                    };
                    let metadata = json!({
                        "request_id": response.request_id,
                        "cycle": response.cycle,
                        "status": format!("{:?}", response.status),
                        "outcome": format!("{:?}", response.outcome),
                        "summary": response.summary,
                        "comments": response.comments,
                        "deterministic_rejection_reasons": crate::model::prompt_safe_deterministic_worker_rejection_reasons(
                            rejection_reasons,
                        ),
                        "present_nodes": response.snapshot.present_nodes,
                        "open_nodes": response.snapshot.open_nodes,
                        "coverage": response.snapshot.coverage,
                    });
                    fs::write(
                        last_invalid_metadata,
                        serde_json::to_string_pretty(&metadata)? + "\n",
                    )?;
                } else if last_invalid_dir.exists() {
                    fs::remove_dir_all(&last_invalid_dir)?;
                }
            }
            _ => {}
        }
        if let Some(snapshot_root) = captured_snapshot_root {
            if snapshot_root.exists() {
                fs::remove_dir_all(snapshot_root)?;
            }
        }
        Ok(())
    }

    fn refresh_in_flight_request_from_state(&mut self) {
        let Some(request) = self.state.in_flight_request.as_ref().cloned() else {
            return;
        };
        self.state.in_flight_request = Some(self.state.expected_request(request.id, request.kind));
    }

    /// Strip only the fields added by `apply_request_execution_hints_to_state`
    /// so the semantic in-flight-request invariant can be checked on reload.
    /// Every other request field remains byte-for-byte as persisted and is
    /// therefore still covered by `ProtocolState::validate`.
    fn normalize_in_flight_request_execution_hints_for_validation(&mut self) {
        let Some(persisted) = self.state.in_flight_request.as_ref() else {
            return;
        };
        let expected = self.state.expected_request(persisted.id, persisted.kind);
        let persisted = self.state.in_flight_request.as_mut().unwrap();
        persisted.fresh_context = expected.fresh_context;
        persisted.prompt_contract_version = expected.prompt_contract_version;
        persisted.project_invariants = expected.project_invariants;
        persisted.paper_contract = expected.paper_contract;
        persisted.corr_contract = expected.corr_contract;
        persisted.sound_contract = expected.sound_contract;
        persisted.worker_contract = expected.worker_contract;
        persisted.review_contract = expected.review_contract;
        persisted.audit_contract = expected.audit_contract;
        persisted.stuck_math_audit_contract = expected.stuck_math_audit_contract;
        persisted.paper_verify_lane_bindings = expected.paper_verify_lane_bindings;
        persisted.corr_verify_lane_bindings = expected.corr_verify_lane_bindings;
        persisted.sound_verify_lane_bindings = expected.sound_verify_lane_bindings;
        persisted.worker_binding = expected.worker_binding;
        persisted.reviewer_binding = expected.reviewer_binding;
        persisted.stuck_math_audit_binding = expected.stuck_math_audit_binding;
    }

    fn request_requires_fresh_context(&self, kind: crate::model::RequestKind) -> bool {
        match kind {
            crate::model::RequestKind::Paper
            | crate::model::RequestKind::Corr
            | crate::model::RequestKind::Sound => true,
            crate::model::RequestKind::Worker | crate::model::RequestKind::Review => !self
                .metadata
                .native_history_kinds
                .contains(&request_history_key(kind, self.state.phase)),
            crate::model::RequestKind::HumanGate => false,
            // Cleanup-v2 audit is a single-burst structured-output role
            // with its own prompt-fragment family. Always treat it as
            // requiring a fresh context until/unless the bridge gains
            // audit-specific history tracking. Continuation bursts within
            // a single audit round carry their state via the scratchpad
            // surfaced in the prompt, not via bridge history.
            crate::model::RequestKind::Audit | crate::model::RequestKind::StuckMathAudit => true,
        }
    }

    fn record_native_history(&mut self, kind: crate::model::RequestKind, phase: Phase) {
        self.metadata
            .native_history_kinds
            .insert(request_history_key(kind, phase));
    }
}

fn persist_trust_gate_presentation(
    journal_root: &Path,
    digest: crate::trust_base::Sha256Digest,
    bytes: &[u8],
) -> Result<(), RuntimeError> {
    let directory = journal_root.join("presentations");
    fs::create_dir_all(&directory).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "failed to create immutable presentation store {}: {error}",
            directory.display()
        ))
    })?;
    let path = directory.join(format!("{digest}.bin"));
    match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(mut file) => {
            file.write_all(bytes).map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "failed to write immutable gate presentation {}: {error}",
                    path.display()
                ))
            })?;
            file.sync_all().map_err(|error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "failed to sync immutable gate presentation {}: {error}",
                    path.display()
                ))
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&path).map_err(|read_error| {
                RuntimeError::InvalidRuntimeState(format!(
                    "failed to read immutable gate presentation {}: {read_error}",
                    path.display()
                ))
            })?;
            if existing != bytes
                || tagged_hash(DomainTag::GatePresentation, &existing) != digest
            {
                return Err(RuntimeError::InvalidRuntimeState(
                    "immutable gate-presentation digest collision or tampering detected".into(),
                ));
            }
        }
        Err(error) => {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "failed to create immutable gate presentation {}: {error}",
                path.display()
            )))
        }
    }
    Ok(())
}

/// Restore the worker repo's working tree to git HEAD via
/// `git reset --hard HEAD` then `git clean -fd`. Used by the
/// runtime's `restore_repo_worktree_for_event` so partial filesystem
/// mutations from a rejected event don't leak into the next attempt's
/// `before_snapshot`. Free function (not `&self`-bound) so it can be
/// reused without constructing a `SupervisorRuntime`. Bug X principled
/// fix (Phase 1-4) made the prior `RollbackWorkerAttempt` CLI variant
/// dead — the kernel-driven `RestoreWorktreeToActiveWorkerBase` is the
/// only restore path the bridge needs; transport failures are conveyed
/// via `transport_failure=true` Malformed responses and the kernel
/// handles the rest.
/// The per-cycle event log (`.trellis-history/event-log/`) is append-only
/// history: the in-memory `event_count` never rewinds, so a worktree restore
/// that reverted or deleted cycle files would tear a hole in the dense global
/// index (caught fail-loud at the next load, with the torn events
/// unrecoverable). Shield the directory across destructive git commands by
/// renaming it aside and moving it back afterwards, replacing whatever git
/// materialized at the path. The shield lives inside `.trellis-history/` so
/// `git clean -fd -e .trellis-history...` invocations skip it.
fn with_event_log_shielded<F>(repo_path: &Path, f: F) -> Result<(), RuntimeError>
where
    F: FnOnce() -> Result<(), RuntimeError>,
{
    let dir = repo_path.join(".trellis-history").join("event-log");
    if !dir.is_dir() {
        return f();
    }
    let shield = repo_path
        .join(".trellis-history")
        .join("event-log.restore-shield");
    if shield.exists() {
        fs::remove_dir_all(&shield)?;
    }
    fs::rename(&dir, &shield)?;
    let result = f();
    if dir.exists() {
        // Whatever the git restore materialized at the path (e.g. the
        // clean tag's older cycle files) is superseded by the shielded
        // live log.
        let _ = fs::remove_dir_all(&dir);
    }
    if let Err(err) = fs::rename(&shield, &dir) {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "event-log shield restore failed: {err}; the live event log is at \
             {} and MUST be moved back to {} before relaunch",
            shield.display(),
            dir.display()
        )));
    }
    result
}

pub fn restore_worktree_to_head(repo_path: &Path) -> Result<(), RuntimeError> {
    with_event_log_shielded(repo_path, || restore_worktree_to_head_inner(repo_path))
}

/// Full HEAD commit SHA, or `None` on any git error. Used to record the
/// durable LastClean commit pointer (Bug 2) after a clean checkpoint.
/// Process memory (spec §7): restore `process-memory/` (worktree + index)
/// from `commit` after a LastClean reset. No-op when the commit carries no
/// `process-memory/` tree (pre-migration runs); loud error when the
/// checkout itself fails.
fn restore_process_memory_from_commit(
    repo_path: &Path,
    commit: &str,
) -> Result<(), RuntimeError> {
    let probe = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "ls-tree",
            "-d",
            commit,
            "--",
            crate::process_memory::PROCESS_MEMORY_DIR,
        ])
        .output()
        .map_err(RuntimeError::from)?;
    if !probe.status.success()
        || String::from_utf8_lossy(&probe.stdout).trim().is_empty()
    {
        return Ok(());
    }
    let checkout = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "checkout",
            commit,
            "--",
            crate::process_memory::PROCESS_MEMORY_DIR,
        ])
        .output()
        .map_err(RuntimeError::from)?;
    if !checkout.status.success() {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "process-memory carry-forward failed for `git checkout {commit} -- {}`: exit code {:?}; stderr={:?}",
            crate::process_memory::PROCESS_MEMORY_DIR,
            checkout.status.code(),
            String::from_utf8_lossy(&checkout.stderr),
        )));
    }
    Ok(())
}

fn git_head_sha(repo_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// `true` iff `commit` resolves and is an ancestor of (or equal to) HEAD.
/// Uses `git merge-base --is-ancestor <commit> HEAD` (exit 0 = ancestor,
/// exit 1 = not, other = error). On any spawn/resolution error we return
/// `false` (treat as "not a safe target") so the Bug-2 selection never
/// rewinds to something it cannot confirm is on the current line.
fn git_is_ancestor_of_head(repo_path: &Path, commit: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["merge-base", "--is-ancestor", commit, "HEAD"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Number of commits in `commit..HEAD` (how far `commit` is behind HEAD).
/// `Some(0)` means `commit == HEAD`. Returns `None` on any git error
/// (e.g. unrelated histories, unresolvable ref). On the linear checkpoint
/// history the ancestor with the SMALLEST value is the nearest / greatest-
/// cycle clean checkpoint.
fn git_commits_behind_head(repo_path: &Path, commit: &str) -> Option<u64> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-list", "--count", &format!("{commit}..HEAD")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn restore_worktree_to_head_inner(repo_path: &Path) -> Result<(), RuntimeError> {
    // Anchored so only the repo-root process-memory dir is spared; a stray
    // nested `Tablet/process-memory/` is still swept.
    let pm_exclude = format!("/{}", crate::process_memory::PROCESS_MEMORY_DIR);
    for command in [
        vec!["reset", "--hard", "HEAD"],
        vec![
            "clean",
            "-fd",
            "-e",
            ".trellis-history",
            "-e",
            ".trellis-stop-after-checkpoint",
            // Process memory (spec §7): entries materialized at audit
            // acceptance stay untracked until the next cycle-Start
            // checkpoint commits them; `process-memory/` is kernel-owned
            // durable state (like `.trellis-history/`), so the sweep must
            // spare it. Before this exclusion, the rejection-cycle
            // worker-retry restore silently deleted the entry files and
            // INDEX.md while `process_memory_seq` kept its bumped value.
            "-e",
            pm_exclude.as_str(),
        ],
    ] {
        let start = std::time::Instant::now();
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(&command)
            .output();
        let duration = start.elapsed().as_secs_f64();
        let output = match output {
            Ok(o) => {
                crate::check_ledger::append_kind(
                    repo_path,
                    "git",
                    command[0],
                    duration,
                    o.status.success(),
                    o.stdout.len(),
                    o.stderr.len(),
                );
                o
            }
            Err(err) => {
                crate::check_ledger::append_kind(
                    repo_path, "git", command[0], duration, false, 0, 0,
                );
                return Err(err.into());
            }
        };
        if !output.status.success() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "restore committed worktree failed for `git {}` with exit code {:?}; stdout={:?}; stderr={:?}",
                command.join(" "),
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )));
        }
    }
    // Purge stale Tablet/*.olean (and friends) whose source file was deleted
    // in this rewind. Same reasoning as `restore_repo_worktree_to_last_clean`:
    // .lake/build is gitignored, so git clean misses it; without this, Lean
    // resolves imports for nodes whose sources are gone, polluting downstream
    // probes and worker reasoning with ghost declarations.
    purge_stale_tablet_build_artifacts(repo_path);
    // INDEX.md is usually TRACKED (committed at prior checkpoints), so the
    // `reset --hard` above reverts an uncommitted audit-acceptance rewrite of
    // it even though the entry files themselves survive the clean exclusion.
    // INDEX.md is derived state; regenerate it from the surviving entry files
    // (no-op for repos without a process-memory dir).
    crate::process_memory::regenerate_index(repo_path).map_err(|err| {
        RuntimeError::InvalidRuntimeState(format!(
            "process-memory index regeneration after worktree restore failed: {err}"
        ))
    })?;
    Ok(())
}

/// Delete stale Lake build artifacts for Tablet nodes whose
/// `Tablet/<stem>.lean` source file is no longer present after a worktree
/// rewind or cone-clean prune. Two artifact classes are purged, in both
/// `.lake/build/lib/lean/Tablet/` and `.lake/build/ir/Tablet/`:
///
///   1. The deleted modules' OWN artifacts (`<stem>.{olean,ilean,olean.hash,
///      ilean.hash,c,c.hash,ll,trace,setup.json,...}`). The artifacts are
///      gitignored so neither `git reset --hard` nor `git clean -fd` touches
///      them; without this purge, Lean's import resolver happily finds the
///      orphaned olean and consumers (probes, workers, reviewers) end up
///      reasoning about declarations whose source no longer exists.
///
///   2. DEPENDENT modules' artifacts — any surviving module whose cached
///      Lake import graph (`.lake/build/ir/Tablet/<m>.setup.json`) still
///      references a deleted `Tablet.<stem>` module. Lake trusts the cached
///      setup rather than re-resolving imports from the (unchanged) source,
///      so `lake build` hard-fails with "object file '.../<stem>.olean' of
///      module Tablet.<stem> does not exist" even when no current source
///      imports the deleted node (observed: unitdistance cycle 694, cone
///      clean of `BigonCorridorSideCopy`). Dropping the dependents'
///      artifacts is safe — their source survives and rebuilds cleanly.
///
/// Best-effort: ignores I/O errors (any individual deletion failure is
/// surfaced via stderr but does not abort the rewind). The set of "live"
/// stems is derived from the current on-disk Tablet/*.lean listing, so
/// multiple deletions in one burst are handled uniformly. Only files
/// directly under the repo's own `.lake/build/{lib/lean,ir}/Tablet/` are
/// ever removed; source files are never touched.
fn purge_stale_tablet_build_artifacts(repo_path: &Path) {
    purge_invalidated_tablet_build_artifacts(repo_path, &std::collections::BTreeSet::new());
}

/// Generalized entry point shared by the deletion trigger
/// (`purge_stale_tablet_build_artifacts`, invalidated set empty) and the
/// edit trigger (worker-burst acceptance in `bin/runtime_cli.rs`, which
/// passes the stems of every Tablet `.lean` file the burst modified or
/// added). A stem's artifacts are invalidated when its source is missing
/// OR it appears in `invalidated_stems`; dependents whose cached
/// `setup.json` import graph references any invalidated stem are swept in
/// the same pass.
///
/// The edit trigger exists because a bare probe (`lake env lean`, which
/// never builds) resolves imports through whatever olean is on disk: after
/// an accepted signature-changing edit, the pre-edit olean is a "phantom"
/// that shows the OLD signature (observed: unitdistance cycle 696,
/// reviewer scratch probe of `EndpointSidePrefixConstruction` displayed
/// the pre-edit signature an hour after the accepted edit). Deleting the
/// artifacts converts that silent phantom into either a fresh rebuild
/// (every support-required dispatch and the acceptance hydrate phase run
/// `materialize-tablet-oleans` = `lake build`, whose caches gate on olean
/// PRESENCE and therefore force a real dispatch once the files are gone)
/// or an unambiguous missing-olean error for a probe that races ahead of
/// the rebuild — strictly better than reasoning about dead declarations.
///
/// Idempotent: deleting already-missing files is a tolerated no-op, so a
/// delete+edit in one burst (acceptance purge, then a later rewind-path
/// purge over the same stems) never errors.
pub fn purge_invalidated_tablet_build_artifacts(
    repo_path: &Path,
    invalidated_stems: &std::collections::BTreeSet<String>,
) {
    use std::collections::BTreeSet;
    let tablet_dir = repo_path.join("Tablet");
    let lib_dir = repo_path.join(".lake/build/lib/lean/Tablet");
    let ir_dir = repo_path.join(".lake/build/ir/Tablet");
    if !lib_dir.is_dir() && !ir_dir.is_dir() {
        return;
    }
    let live_stems: BTreeSet<String> = match std::fs::read_dir(&tablet_dir) {
        Ok(iter) => iter
            .filter_map(|e| e.ok())
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("lean") {
                    path.file_stem().and_then(|s| s.to_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => return,
    };
    let directly_invalidated =
        |stem: &str| !live_stems.contains(stem) || invalidated_stems.contains(stem);
    // Identify surviving dependents whose cached import graph references an
    // invalidated module BEFORE purging, so the scan sees every setup.json.
    let stale_dependents = tablet_dependents_of_invalidated_modules(&ir_dir, &directly_invalidated);
    let mut purged = 0usize;
    for dir in [&lib_dir, &ir_dir] {
        purged += purge_tablet_artifact_dir_entries(dir, |stem| {
            directly_invalidated(stem) || stale_dependents.contains(stem)
        });
    }
    if purged > 0 {
        eprintln!(
            "trellis: purged {purged} stale .lake/build/{{lib/lean,ir}}/Tablet/ \
             entr{plural} for deleted/edited-source nodes and their cached-import \
             dependents (post-rewind/cone-clean/acceptance cleanup).",
            plural = if purged == 1 { "y" } else { "ies" }
        );
    }
}

/// Delete every regular file directly under `dir` whose leading stem (text
/// before the first '.' — handles .olean, .olean.hash, .ilean, .ilean.hash,
/// .c, .c.hash, .ll, .trace, .setup.json) satisfies `is_stale`. Returns the
/// number of files removed. Missing directory or unreadable entries are
/// skipped; individual deletion failures are surfaced via stderr but never
/// abort the purge (missing files are fine — another pass may already have
/// removed them).
fn purge_tablet_artifact_dir_entries(dir: &Path, is_stale: impl Fn(&str) -> bool) -> usize {
    let entries = match std::fs::read_dir(dir) {
        Ok(iter) => iter,
        Err(_) => return 0,
    };
    let mut purged = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let stem = match name.split('.').next() {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        if !is_stale(stem) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => purged += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!(
                    "trellis: failed to purge stale Tablet build artifact {}: {err}",
                    path.display()
                );
            }
        }
    }
    purged
}

/// Scan `<ir_dir>/*.setup.json` for surviving Tablet modules whose cached
/// Lake import graph still references an invalidated `Tablet.<stem>`
/// module (source deleted, or content edited this burst), returning their
/// stems. Matching is exact-module-name — a JSON object key or string
/// value equal to `Tablet.<stem>` with exactly one segment after
/// `Tablet.` — never substring, so an invalidated `Tablet.Foo` does not
/// flag a dependent that only imports `Tablet.FooBar`. Walking keys AND
/// string values keeps the check robust across Lake setup-file schema
/// variations (`importArts` keys today; plain import-name arrays in other
/// versions). An unparseable setup.json is conservatively treated as
/// stale — invalidating it only costs a rebuild from source, whereas
/// trusting it risks the hard `lake build` failure this purge exists to
/// prevent.
fn tablet_dependents_of_invalidated_modules(
    ir_dir: &Path,
    is_invalidated: &dyn Fn(&str) -> bool,
) -> std::collections::BTreeSet<String> {
    let mut stale = std::collections::BTreeSet::new();
    let entries = match std::fs::read_dir(ir_dir) {
        Ok(iter) => iter,
        Err(_) => return stale,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        let stem = match name.strip_suffix(".setup.json") {
            Some(s) if !s.is_empty() && !s.contains('.') => s,
            _ => continue,
        };
        if is_invalidated(stem) {
            // The invalidated module's own setup.json; the direct pass
            // already removes it, and its references are moot.
            continue;
        }
        let references_deleted = match std::fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|text| {
                serde_json::from_str::<serde_json::Value>(&text).map_err(|err| err.to_string())
            }) {
            Ok(value) => json_references_invalidated_tablet_module(&value, is_invalidated),
            Err(err) => {
                eprintln!(
                    "trellis: unreadable Lake setup file {} ({err}); \
                     conservatively invalidating module {stem}'s build artifacts.",
                    path.display()
                );
                true
            }
        };
        if references_deleted {
            stale.insert(stem.to_string());
        }
    }
    stale
}

/// True iff any JSON object key or string value anywhere in `value` is an
/// exact `Tablet.<stem>` module name whose `<stem>` satisfies
/// `is_invalidated` (deleted source or edited-this-burst).
fn json_references_invalidated_tablet_module(
    value: &serde_json::Value,
    is_invalidated: &dyn Fn(&str) -> bool,
) -> bool {
    match value {
        serde_json::Value::String(s) => is_invalidated_tablet_module_name(s, is_invalidated),
        serde_json::Value::Array(items) => items
            .iter()
            .any(|item| json_references_invalidated_tablet_module(item, is_invalidated)),
        serde_json::Value::Object(map) => map.iter().any(|(key, item)| {
            is_invalidated_tablet_module_name(key, is_invalidated)
                || json_references_invalidated_tablet_module(item, is_invalidated)
        }),
        _ => false,
    }
}

/// True iff `candidate` is exactly `Tablet.<stem>` (one segment, non-empty)
/// with `<stem>` satisfying `is_invalidated`. Tablet node modules are flat,
/// so multi-segment names (`Tablet.Foo.Bar`) are never node references.
fn is_invalidated_tablet_module_name(
    candidate: &str,
    is_invalidated: &dyn Fn(&str) -> bool,
) -> bool {
    match candidate.strip_prefix("Tablet.") {
        Some(stem) if !stem.is_empty() && !stem.contains('.') => is_invalidated(stem),
        _ => false,
    }
}

/// Patch C-Q Q5 — canonical filesystem path for a persisted local-closure
/// record under `<runtime_root>/checker-state/local-closure-records/`.
/// Escapes `/` in node IDs to `_` so deletion and persistence stay in
/// lockstep (the persistence path in `bin/runtime_cli.rs` does the same
/// substitution, and the audit flagged the mismatch as a future-proofing
/// risk even though current `NodeId`s don't contain `/`). Centralizing
/// the construction here means any future filename-mapping change has
/// exactly one site to update.
pub fn persisted_record_path(runtime_root: &Path, node: &NodeId) -> PathBuf {
    let safe_name = node.as_str().replace('/', "_");
    runtime_root
        .join("checker-state")
        .join("local-closure-records")
        .join(format!("{}.json", safe_name))
}

/// Patch C-Q Q5 — filename component (without parent directory) for a
/// persisted local-closure record. Used by `persist_record_to_disk` in
/// `bin/runtime_cli.rs`, which already owns the `records_dir`. Keeps
/// the same escape logic as `persisted_record_path`.
pub fn persisted_record_file_name(node: &NodeId) -> String {
    let safe_name = node.as_str().replace('/', "_");
    format!("{}.json", safe_name)
}

/// Patch C-O HIGH 1 (c) — remove the persisted local-closure record
/// file at `<runtime_root>/checker-state/local-closure-records/<node>.json`.
/// Called by the runtime when the engine emits
/// `ProtocolCommand::DeleteLocalClosureRecord`. Missing-file is not an
/// error (no probe has persisted a record yet for that node). Other
/// I/O failures are logged to stderr — the engine's in-memory tombstone
/// (Patch C-O HIGH 1 (a)) is the load-bearing guard; the disk delete is
/// hygiene to avoid stale files accumulating.
///
/// Patch C-Q Q5 — uses `persisted_record_path` so the filename escape
/// matches the persistence side.
///
/// Audit L-1 — surfaced as `pub` so integration tests
/// (`kernel/tests/local_closure_disk_durability.rs`) can pin the
/// per-file delete primitive that the L-1 flush loop in
/// `step_with_checkpoint_sink` iterates. The internal callers are still
/// the only paths that DRIVE the delete (engine emits a command, the
/// runtime processes it); test-side direct calls verify the primitive's
/// idempotency contract.
pub fn delete_persisted_local_closure_record(runtime_root: &Path, node: &NodeId) {
    let file = persisted_record_path(runtime_root, node);
    match fs::remove_file(&file) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            eprintln!(
                "[local-closure delete] failed to remove {}: {err}",
                file.display()
            );
        }
    }
}

fn worker_surface_directory_present(path: &Path) -> Result<bool, RuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(true),
        Ok(_) => Err(RuntimeError::InvalidRuntimeState(format!(
            "worker-writable semantic surface {} must be a directory when present",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(RuntimeError::Io(error)),
    }
}

fn remove_path_without_following_symlinks(path: &Path) -> Result<(), RuntimeError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(RuntimeError::Io(error)),
    }
    Ok(())
}

/// Restore one worker-writable semantic source tree without touching any
/// sibling path. `expected_present` records absence as well as presence, so a
/// worker-created tree cannot survive a rollback merely because no baseline
/// directory exists to copy over it.
fn restore_worker_surface(
    snapshot: &Path,
    destination: &Path,
    expected_present: bool,
) -> Result<(), RuntimeError> {
    remove_path_without_following_symlinks(destination)?;
    if expected_present {
        copy_dir_recursive(snapshot, destination)?;
    }
    Ok(())
}

fn validate_worker_surface_snapshot(
    snapshot: &Path,
    expected_present: bool,
    surface_name: &str,
) -> Result<(), RuntimeError> {
    let snapshot_present = worker_surface_directory_present(snapshot)?;
    if snapshot_present != expected_present {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "active worker base surface `{surface_name}` disagrees with its manifest: \
             manifest present={expected_present}, snapshot directory present={snapshot_present}"
        )));
    }
    if expected_present {
        validate_worker_surface_tree(snapshot)?;
    }
    Ok(())
}

fn validate_worker_surface_tree(root: &Path) -> Result<(), RuntimeError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            validate_worker_surface_tree(&path)?;
        } else if !file_type.is_file() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "refusing to copy non-regular worker-surface entry {} \
                 (symlinks, FIFOs, sockets, and device nodes are forbidden)",
                path.display()
            )));
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), RuntimeError> {
    fs::create_dir_all(dst)?;
    // Normalize directory mode to group-writable (0o2775 keeps the setgid bit
    // so children inherit the parent group). The rollback path
    // (`restore_repo_worktree_to_active_worker_base`) writes into the worker
    // repo's `Tablet/` as the supervisor user; the next worker
    // burst runs inside a bwrap as the burst user, which is in the
    // supervisor's group. Without this, dirs inherit the supervisor's umask (0o775
    // typically), which is fine, but we re-assert it explicitly so the
    // invariant is local to this helper rather than scattered across
    // shell-level umask + bwrap config.
    set_dir_mode_group_writable(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src_path, &dst_path)?;
            // 2026-04-28 fix: `fs::copy` preserves the source's mode bits.
            // When an agent writes Tablet files via tools that default to
            // 0o600 (e.g. `tempfile.mkstemp` followed by atomic rename, or
            // codex's internal write path), those 0o600 modes get captured
            // into `active_worker_base/Tablet/` and then restored back to
            // the worker repo on the rollback path. The next worker burst
            // — running as the burst user, in the supervisor's group
            // — cannot read or modify a file owned by the supervisor user
            // with mode 0o600 (no group access). The dir-level lock is
            // group-writable so the worker can `rm` and re-create the file,
            // but that wastes a retry cycle on a self-inflicted permission
            // detour and surfaces as a transport_failure on the deterministic
            // checker (`sync_tablet_support` writing `Tablet/README.md`).
            // Normalize to 0o664 so any group member can read/write
            // restored content.
            set_file_mode_group_writable(&dst_path, &src_path)?;
        } else {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "refusing to copy non-regular worker-surface entry {} \
                 (symlinks, FIFOs, sockets, and device nodes are forbidden)",
                src_path.display()
            )));
        }
    }
    Ok(())
}

fn set_dir_mode_group_writable(path: &Path) -> Result<(), RuntimeError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    // 0o2775 = setgid + rwx for owner & group, rx for other. Setgid keeps
    // newly-created children in the parent's group (Tablet/ is owned by
    // the supervisor's group on the live runtime, dir mode 2775).
    perms.set_mode(0o2775);
    fs::set_permissions(path, perms)?;
    Ok(())
}

fn set_file_mode_group_writable(dst: &Path, src: &Path) -> Result<(), RuntimeError> {
    use std::os::unix::fs::PermissionsExt;
    // Preserve the executable bit if the source had it (Tablet/ files are
    // never executable, but this helper is shared by all `copy_dir_recursive`
    // callers, including ones that may handle scripts in the future). Apply
    // 0o664 base + 0o111 mask if any execute bit was set on the source.
    let src_mode = fs::metadata(src)?.permissions().mode() & 0o777;
    let any_exec = src_mode & 0o111 != 0;
    let target_mode = if any_exec { 0o775 } else { 0o664 };
    let mut perms = fs::metadata(dst)?.permissions();
    perms.set_mode(target_mode);
    fs::set_permissions(dst, perms)?;
    Ok(())
}

fn request_kind_key(kind: crate::model::RequestKind) -> &'static str {
    match kind {
        crate::model::RequestKind::Worker => "worker",
        crate::model::RequestKind::Paper => "paper",
        crate::model::RequestKind::Corr => "corr",
        crate::model::RequestKind::Sound => "sound",
        crate::model::RequestKind::Review => "review",
        crate::model::RequestKind::HumanGate => "human_gate",
        crate::model::RequestKind::Audit => "audit",
        crate::model::RequestKind::StuckMathAudit => "stuck_math_audit",
    }
}

fn phase_key(phase: Phase) -> &'static str {
    match phase {
        Phase::TheoremStating => "theorem_stating",
        Phase::RevisionStating => "revision_stating",
        Phase::ProofFormalization => "proof_formalization",
        Phase::Cleanup => "cleanup",
        Phase::Complete => "complete",
    }
}

fn request_history_key(kind: crate::model::RequestKind, phase: Phase) -> String {
    match kind {
        crate::model::RequestKind::Worker | crate::model::RequestKind::Review => {
            format!("{}:{}", request_kind_key(kind), phase_key(phase))
        }
        _ => request_kind_key(kind).to_string(),
    }
}

/// Derive `event_count` by summing non-blank lines across the per-cycle
/// event-log files, with a fail-loud reconciliation: the highest record's
/// `index` must equal `sum - 1` (dense 0..N-1 global index). A mismatch means
/// a gap, a duplicate, or a non-contiguous cycle file — refuse to start so a
/// downstream resume can't compute the wrong event_count.
fn read_event_count(dir: &Path) -> Result<u64, RuntimeError> {
    let files = event_log_cycle_files(dir)?;
    if files.is_empty() {
        return Ok(0);
    }
    let mut sum: u64 = 0;
    let mut max_index: Option<u64> = None;
    for path in &files {
        let text = fs::read_to_string(path)?;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            sum += 1;
            // The highest file (last in lexical order) carries the highest
            // index, but parse every record's index so a gap anywhere is
            // caught by the max-vs-sum reconciliation below.
            if let Ok(record) = serde_json::from_str::<EventLogRecord>(line) {
                max_index = Some(max_index.map_or(record.index, |m| m.max(record.index)));
            }
        }
    }
    if sum == 0 {
        return Ok(0);
    }
    match max_index {
        Some(max_index) if max_index + 1 == sum => Ok(sum),
        Some(max_index) => Err(RuntimeError::InvalidRuntimeState(format!(
            "event-log index density violated in {}: highest record index={max_index} but \
             {sum} non-blank records present (expected {} for a dense 0..N-1 index). A gap, \
             duplicate, or non-contiguous cycle file is present; refusing to start.",
            dir.display(),
            max_index + 1
        ))),
        None => Err(RuntimeError::InvalidRuntimeState(format!(
            "event-log directory {} has {sum} non-blank line(s) but no parseable record to \
             reconcile the global index against; refusing to start.",
            dir.display()
        ))),
    }
}

fn read_metadata(path: &Path) -> Result<RuntimeMetadata, RuntimeError> {
    if !path.exists() {
        return Ok(RuntimeMetadata::default());
    }
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn ensure_external_trust_journal_path(metadata: &RuntimeMetadata) -> Result<(), RuntimeError> {
    let journal_path = metadata.trust_journal_path.as_deref().ok_or_else(|| {
        RuntimeError::InvalidRuntimeState(
            "trust-base v1 requires runtime_metadata.trust_journal_path".into(),
        )
    })?;
    let journal_abs = fs::canonicalize(journal_path).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "cannot resolve trust journal {}: {error}",
            journal_path.display()
        ))
    })?;
    if let Some(repo_path) = metadata.repo_path.as_deref() {
        let repo_abs = fs::canonicalize(repo_path).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "cannot resolve run repository {}: {error}",
                repo_path.display()
            ))
        })?;
        if journal_abs.starts_with(&repo_abs) {
            return Err(RuntimeError::InvalidRuntimeState(
                "trust journal must be outside the rewindable run repository".into(),
            ));
        }
    }
    Ok(())
}

/// Checkpoint copies of seed-derived worker guidance are never authoritative.
/// Re-read the externally configured seed closure on every trust-runtime load
/// (and after protected-revision recovery), then require exact projection
/// equality before any request can be refreshed or dispatched.
fn verify_runtime_trust_seed_projection(
    state: &mut ProtocolState,
    metadata: &RuntimeMetadata,
    journal: &TrustJournal,
) -> Result<bool, RuntimeError> {
    if let Some(revision) = journal
        .current_approval()
        .and_then(|approval| approval.revision_closure)
    {
        let journal_path = metadata.trust_journal_path.as_deref().ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires the external journal path on runtime load".into(),
            )
        })?;
        let stored = load_revision_closure(journal_path, &revision)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        let (candidates, source_guidance) = seed_worker_projections(&stored.seed)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        let support_definitions = seed_support_definition_projection(&stored.evidence)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        // Headless authority-recovery runtimes have no attached worktree.
        // A runtime that can dispatch workers always has `repo_path`, and its
        // probe path rechecks the same bytes immediately before use.
        if let Some(repo_path) = metadata.repo_path.as_deref() {
            verify_seed_support_definition_files(&repo_path.join("Tablet"), &support_definitions)
                .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        }
        let migrated = migrate_legacy_seed_support_projection(
            state,
            &support_definitions,
            revision.evidence_tool_input_root,
        )?;
        if state.trust_base.seed_manifest_sha256 != Some(stored.seed.seed_manifest_sha256)
            || state.trust_base.seed_definition_bundle_sha256 != Some(stored.seed.bundle_sha256)
            || state.trust_base.evidence_tool_manifest_sha256
                != Some(stored.evidence.manifest_sha256)
            || state.trust_base.authored_semantic_root
                != Some(revision.authored_semantic_root)
            || state.trust_base.approved_evidence_tool_input_root
                != Some(revision.evidence_tool_input_root)
            || state.trust_base.conditional_theorem_candidates != candidates
            || state.trust_base.source_validation_guidance != source_guidance
            || state.trust_base.seed_support_definitions != support_definitions
        {
            return Err(RuntimeError::InvalidRuntimeState(
                "persisted trust worker guidance differs from the journal-selected revision closure"
                    .into(),
            ));
        }
        return Ok(migrated);
    }
    let seed_path = metadata
        .trust_seed_manifest_path
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires the seed manifest path on runtime load".into(),
            )
        })?;
    let seed_value = parse_json_strict(&fs::read(seed_path).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "failed to read seed manifest {} on runtime load: {error}",
            seed_path.display()
        ))
    })?)
    .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let registry = SchemaRegistry::v1()
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let seed = AuthoritativeRecord::parse(&registry, seed_value)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let bundle_path = metadata
        .trust_seed_definition_bundle_path
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires the seed definition bundle path on runtime load".into(),
            )
        })?;
    let closure = verify_seed_definition_bundle(
        &seed,
        &fs::read(bundle_path).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "failed to read seed definition bundle {} on runtime load: {error}",
                bundle_path.display()
            ))
        })?,
    )
    .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let evidence_manifest_path = metadata
        .trust_evidence_tool_manifest_path
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires the evidence/tool manifest on runtime load".into(),
            )
        })?;
    let evidence_root_path = metadata
        .trust_evidence_tool_root_path
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires the evidence/tool root on runtime load".into(),
            )
        })?;
    let evidence = verify_evidence_tool_manifest(
        evidence_root_path,
        &fs::read(evidence_manifest_path).map_err(|error| {
            RuntimeError::InvalidRuntimeState(format!(
                "failed to read evidence/tool manifest {} on runtime load: {error}",
                evidence_manifest_path.display()
            ))
        })?,
    )
    .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let support_definitions = seed_support_definition_projection(&evidence)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    // Headless authority-recovery runtimes have no attached worktree.  The
    // worker path requires a repository and rechecks these bytes before use.
    if let Some(repo_path) = metadata.repo_path.as_deref() {
        verify_seed_support_definition_files(&repo_path.join("Tablet"), &support_definitions)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    }
    let migrated = migrate_legacy_seed_support_projection(
        state,
        &support_definitions,
        evidence.evidence_tool_input_root,
    )?;
    let (candidates, source_guidance) = seed_worker_projections(&closure)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let mut mismatches = Vec::new();
    if state.trust_base.seed_manifest_sha256 != Some(seed.digest()) {
        mismatches.push("seed manifest");
    }
    if state.trust_base.seed_definition_bundle_sha256 != Some(closure.bundle_sha256) {
        mismatches.push("seed definition bundle");
    }
    if state.trust_base.evidence_tool_manifest_sha256 != Some(evidence.manifest_sha256) {
        mismatches.push("evidence manifest");
    }
    // Do not equate this configured closure's root with the currently
    // approved root.  During a protected revision the configured closure is
    // the staged proposal, while the journal deliberately keeps the prior
    // approval authoritative until protected reapproval commits.  The CLI
    // refuses to use support definitions while that revision lane is open.
    if state.trust_base.conditional_theorem_candidates != candidates {
        mismatches.push("conditional candidates");
    }
    if state.trust_base.source_validation_guidance != source_guidance {
        mismatches.push("source-validation guidance");
    }
    if state.trust_base.seed_support_definitions != support_definitions {
        mismatches.push("seed support definitions");
    }
    if !mismatches.is_empty() {
        return Err(RuntimeError::InvalidRuntimeState(
            format!(
                "persisted trust worker guidance differs from the verified seed closure: {}",
                mismatches.join(", ")
            ),
        ));
    }
    Ok(migrated)
}

/// Older required-v1 checkpoints predate the explicit non-node support
/// projection.  It is a derived cache, not authority, so reconstruct an empty
/// legacy field only from the exact currently approved evidence closure.  No
/// closure result is promoted by this migration; affected nodes still enter
/// deterministic revalidation and must earn a newly bound record.
fn migrate_legacy_seed_support_projection(
    state: &mut ProtocolState,
    projection: &BTreeMap<NodeId, crate::model::TrustSeedSupportDefinition>,
    projection_evidence_root: crate::trust_base::Sha256Digest,
) -> Result<bool, RuntimeError> {
    if !state.trust_base.seed_support_definitions.is_empty() {
        return Ok(false);
    }
    if !state.trust_base.required() {
        return Err(RuntimeError::InvalidRuntimeState(
            "seed support projection migration is valid only in required-v1 mode".into(),
        ));
    }
    if state.trust_base.approved_evidence_tool_input_root != Some(projection_evidence_root) {
        return Err(RuntimeError::InvalidRuntimeState(
            "cannot migrate seed support projection from an unapproved evidence root".into(),
        ));
    }
    if state.local_closure_records.values().any(|record| {
        !record.seed_support_definition_deps.is_empty()
            || record.seed_support_evidence_root.is_some()
            || !record.seed_support_file_hashes.is_empty()
    }) {
        return Err(RuntimeError::InvalidRuntimeState(
            "checkpoint has support-bound local-closure records but no seed support projection"
                .into(),
        ));
    }
    state.trust_base.seed_support_definitions = projection.clone();
    Ok(true)
}

fn load_verified_actor_keys(metadata: &RuntimeMetadata) -> Result<ActorKeyManifest, RuntimeError> {
    let manifest_path = metadata
        .trust_actor_key_manifest_path
        .as_deref()
        .ok_or_else(|| {
            RuntimeError::InvalidRuntimeState(
                "trust-base v1 requires a root-authorized actor key manifest".into(),
            )
        })?;
    let manifest_bytes = fs::read(manifest_path).map_err(|error| {
        RuntimeError::InvalidRuntimeState(format!(
            "failed to read actor key manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest_value = parse_json_strict(&manifest_bytes)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    let registry = SchemaRegistry::v1()
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    if metadata.trust_manifest_authority_roots.is_empty() {
        return Err(RuntimeError::InvalidRuntimeState(
            "trust-base v1 has no independently installed manifest-authority roots".into(),
        ));
    }
    let mut roots = ManifestAuthorityRoots::default();
    for (authority_id, public_key) in &metadata.trust_manifest_authority_roots {
        roots
            .insert_hex(authority_id.clone(), public_key)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
    }
    ActorKeyManifest::verify(&registry, manifest_value, &roots)
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))
}

fn revision_terminal_kind(
    journal: &TrustJournal,
    revision_lane_id: &str,
) -> Result<Option<EventKind>, RuntimeError> {
    for sequence in (1..=journal.head().sequence_number).rev() {
        let bundle = journal
            .committed_bundle_value(sequence)
            .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        let event: JournalEvent = serde_json::from_value(
            bundle
                .get("event")
                .cloned()
                .ok_or_else(|| {
                    RuntimeError::InvalidRuntimeState(
                        "committed journal bundle lacks its event".into(),
                    )
                })?,
        )
        .map_err(|error| RuntimeError::InvalidRuntimeState(error.to_string()))?;
        if event.revision_lane_id.as_deref() == Some(revision_lane_id)
            && matches!(
                event.event_kind,
                EventKind::ProtectedReapprovalApproved
                    | EventKind::ProtectedReapprovalFeedback
            )
        {
            return Ok(Some(event.event_kind));
        }
    }
    Ok(None)
}

struct TheoremStatingBaseline {
    commit: String,
    state: ProtocolState,
}

struct ObservedLiveTabletState {
    live: WorkingSnapshot,
    node_kinds: BTreeMap<NodeId, crate::model::NodeKind>,
    proof_nodes: BTreeSet<NodeId>,
    deps: BTreeMap<NodeId, BTreeSet<NodeId>>,
    target_claims: BTreeMap<NodeId, BTreeSet<crate::model::TargetId>>,
}

fn restore_repo_path_from_git(
    repo_path: &Path,
    commit: &str,
    rel_path: &str,
) -> Result<(), RuntimeError> {
    let show_arg = format!("{commit}:{rel_path}");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["show", &show_arg])
        .output()?;
    if !output.status.success() {
        return Err(RuntimeError::InvalidRuntimeState(format!(
            "failed to restore {rel_path} from {commit}; exit={:?}; stderr={:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        )));
    }
    let dest = repo_path.join(rel_path);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, output.stdout)?;
    Ok(())
}

fn remove_tablet_node_files(repo_path: &Path, node: &NodeId) -> Result<(), RuntimeError> {
    for ext in ["lean", "tex"] {
        let path = repo_path
            .join("Tablet")
            .join(format!("{}.{}", node.as_str(), ext));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(RuntimeError::Io(err)),
        }
    }
    Ok(())
}

fn target_claims_after_theorem_stating_node_restore(
    state: &ProtocolState,
    baseline: &ProtocolState,
    node: &NodeId,
) -> BTreeMap<NodeId, BTreeSet<crate::model::TargetId>> {
    // Cone clean restores one node. Other surviving nodes keep live claims;
    // orphan pruning and verifier fingerprints reconcile the mixed state.
    let mut target_claims = state.target_claims.clone();
    match baseline.target_claims.get(node) {
        Some(targets) => {
            target_claims.insert(node.clone(), targets.clone());
        }
        None => {
            target_claims.remove(node);
        }
    }
    target_claims
}

fn paper_approved_after_theorem_stating_node_restore(
    state: &ProtocolState,
    baseline: &ProtocolState,
    node: &NodeId,
) -> BTreeMap<crate::model::TargetId, crate::model::Fingerprint> {
    let mut approved = state.paper_approved_fingerprints.clone();
    for target in baseline.target_claims.get(node).into_iter().flatten() {
        if let Some(fp) = baseline.paper_approved_fingerprints.get(target) {
            approved.insert(target.clone(), fp.clone());
        }
    }
    approved
}

fn retain_target_claims_for_present(
    target_claims: &mut BTreeMap<NodeId, BTreeSet<crate::model::TargetId>>,
    present_nodes: &BTreeSet<NodeId>,
    configured_targets: &BTreeSet<crate::model::TargetId>,
) {
    target_claims.retain(|node, targets| {
        if !present_nodes.contains(node) {
            return false;
        }
        targets.retain(|target| configured_targets.contains(target));
        !targets.is_empty()
    });
}

fn paper_source_path_from_config(config_path: Option<&Path>) -> Option<PathBuf> {
    let config_path = config_path?;
    let text = fs::read_to_string(config_path).ok()?;
    let raw: serde_json::Value = serde_json::from_str(&text).ok()?;
    let paper = raw
        .as_object()
        .and_then(|obj| obj.get("workflow"))
        .and_then(|workflow| workflow.as_object())
        .and_then(|workflow| workflow.get("paper_tex_path"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(PathBuf::from(paper))
}

fn observe_live_tablet_state_from_repo(
    repo_path: &Path,
    state: &ProtocolState,
    mut target_claims: BTreeMap<NodeId, BTreeSet<crate::model::TargetId>>,
    approved_paper_fingerprints: &BTreeMap<crate::model::TargetId, crate::model::Fingerprint>,
    paper_source_path: Option<&Path>,
) -> Result<ObservedLiveTabletState, RuntimeError> {
    let present_nodes = crate::worker_normalization::present_nodes_from_repo(repo_path)
        .map_err(RuntimeError::InvalidRuntimeState)?;
    retain_target_claims_for_present(
        &mut target_claims,
        &present_nodes,
        &state.configured_targets,
    );
    let open_nodes = crate::worker_normalization::open_nodes_from_repo(repo_path, &present_nodes);
    let node_kinds = crate::worker_normalization::node_kinds_from_repo(repo_path, &present_nodes);
    let proof_nodes =
        crate::worker_normalization::proof_nodes_from_kinds(&node_kinds, &present_nodes);
    let deps = crate::worker_normalization::direct_deps_from_repo(repo_path, &present_nodes);
    let coverage = crate::worker_normalization::coverage_from_claims(
        &state.configured_targets,
        &target_claims,
        &present_nodes,
    );
    let under_model_assumption_nodes =
        crate::runtime_cli_observations::under_model_assumption_nodes_from_state(state);
    let target_fingerprints =
        crate::runtime_cli_observations::observe_correspondence_fingerprints_with_under_model_assumptions(
            repo_path,
            &present_nodes,
            &under_model_assumption_nodes,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let sound_current_fingerprints =
        crate::runtime_cli_observations::observe_soundness_fingerprints(
            repo_path,
            &present_nodes,
            &node_kinds,
            &state.node_role,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let sound_current_fingerprint_parts =
        crate::runtime_cli_observations::observe_soundness_fingerprint_parts(
            repo_path,
            &present_nodes,
            &node_kinds,
            &state.node_role,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let sketch_proof_nodes =
        crate::runtime_cli_observations::observe_sketch_proof_nodes(repo_path, &present_nodes);
    let placeholder_definition_nodes =
        crate::runtime_cli_observations::observe_placeholder_definition_nodes(
            repo_path,
            &present_nodes,
            &node_kinds,
        );
    let covering_union: BTreeSet<NodeId> = coverage.values().flatten().cloned().collect();
    let lean_relevant_per_covering =
        crate::runtime_cli_observations::observe_lean_relevant_definition_descendants_per_node(
            repo_path,
            &covering_union,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let paper_current_fingerprints = crate::observe_paper_faithfulness_fingerprints(
        repo_path,
        &state.configured_targets,
        &target_claims,
        &present_nodes,
        approved_paper_fingerprints,
        &lean_relevant_per_covering,
    );
    let deviation_current_fingerprints =
        crate::runtime_cli_observations::observe_deviation_fingerprints(
            repo_path,
            &state.deviation_files,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let substantiveness_current_fingerprints =
        crate::runtime_cli_observations::observe_substantiveness_fingerprints(
            repo_path,
            &present_nodes,
            paper_source_path,
            &node_kinds,
            &state.node_deviation_claims,
            &deviation_current_fingerprints,
            &state.configured_reference_papers,
            &state.node_reference_grounds,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let protected_closure_nodes_per_target =
        crate::runtime_cli_observations::observe_protected_closure_nodes(
            repo_path,
            &coverage,
            &present_nodes,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    Ok(ObservedLiveTabletState {
        live: WorkingSnapshot {
            present_nodes,
            open_nodes,
            coverage,
            target_fingerprints: target_fingerprints.clone(),
            corr_current_fingerprints: target_fingerprints,
            paper_current_fingerprints,
            sound_current_fingerprints,
            deviation_current_fingerprints,
            sound_current_fingerprint_parts,
            sketch_proof_nodes,
            placeholder_definition_nodes,
            substantiveness_current_fingerprints,
            protected_closure_nodes_per_target,
            // Challenge coverage is kernel state derived from
            // `challenge_claims`; the observation layer doesn't read it
            // from the repo. `normalize_live_structural_state` recomputes
            // it right after this snapshot is installed.
            challenge_coverage: BTreeMap::new(),
        },
        node_kinds,
        proof_nodes,
        deps,
        target_claims,
    })
}

/// Walk the configured repo's git history for the most recent commit whose
/// `.trellis-history/supervisor_state.json` carried a populated
/// `coarse_dag_nodes`. Used by
/// [`SupervisorRuntime::heal_coarse_dag_from_git_if_needed`] to recover
/// from a state that lost the field.
///
/// Bounded: scans at most [`COARSE_DAG_GIT_SCAN_LIMIT`] commits. Returns
/// `None` if the repo isn't a git repo, no historical commit had a
/// populated value, or any git invocation errors.
fn recover_coarse_dag_from_git(repo_path: &Path) -> Option<BTreeSet<NodeId>> {
    let log_output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "log",
            "--format=%H",
            &format!("--max-count={COARSE_DAG_GIT_SCAN_LIMIT}"),
            "--",
            COARSE_DAG_HISTORY_PATH,
        ])
        .output()
        .ok()?;
    if !log_output.status.success() {
        return None;
    }
    let log_text = String::from_utf8(log_output.stdout).ok()?;
    for sha in log_text.lines() {
        let sha = sha.trim();
        if sha.is_empty() {
            continue;
        }
        let show_arg = format!("{sha}:{COARSE_DAG_HISTORY_PATH}");
        let show_output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["show", &show_arg])
            .output()
            .ok()?;
        if !show_output.status.success() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_slice(&show_output.stdout) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(arr) = parsed
            .get("state")
            .and_then(|s| s.get("coarse_dag_nodes"))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        if arr.is_empty() {
            continue;
        }
        let nodes: BTreeSet<NodeId> = arr
            .iter()
            .filter_map(|v| v.as_str().map(NodeId::from))
            .collect();
        if !nodes.is_empty() {
            return Some(nodes);
        }
    }
    None
}

fn recover_theorem_stating_baseline_from_git(repo_path: &Path) -> Option<TheoremStatingBaseline> {
    let log_output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "log",
            "--format=%H",
            &format!("--max-count={THEOREM_STATING_BASELINE_GIT_SCAN_LIMIT}"),
            "--",
            COARSE_DAG_HISTORY_PATH,
        ])
        .output()
        .ok()?;
    if !log_output.status.success() {
        return None;
    }
    let log_text = String::from_utf8(log_output.stdout).ok()?;
    let mut candidate: Option<TheoremStatingBaseline> = None;
    for sha in log_text
        .lines()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
    {
        let show_arg = format!("{sha}:{COARSE_DAG_HISTORY_PATH}");
        let show_output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["show", &show_arg])
            .output()
            .ok()?;
        if !show_output.status.success() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_slice(&show_output.stdout) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(state_value) = parsed.get("state").cloned() else {
            continue;
        };
        let mut parsed_state: ProtocolState = match serde_json::from_value(state_value) {
            Ok(state) => state,
            Err(_) => continue,
        };
        parsed_state.normalize_all_structural_state();
        parsed_state.ensure_node_metadata();
        if parsed_state.phase == Phase::ProofFormalization
            && !parsed_state.coarse_dag_nodes.is_empty()
        {
            candidate = Some(TheoremStatingBaseline {
                commit: sha.to_string(),
                state: parsed_state,
            });
            continue;
        }
        if candidate.is_some() && parsed_state.phase.is_theorem_stating_like() {
            break;
        }
    }
    candidate
}

/// Path inside the repo where the supervisor's git checkpoint hook writes
/// a snapshot of the live `ProtocolState`. Each `supervisor2/checkpoint-*`
/// commit updates this file (see `trellis/runtime/git_checkpoint_hook.py`),
/// so historical revisions are the canonical source for recovering the
/// authentic `coarse_dag_nodes` value.
const COARSE_DAG_HISTORY_PATH: &str = ".trellis-history/supervisor_state.json";

/// Cap the number of historical commits scanned during the heal. Each
/// commit needs one `git show` subprocess. 500 is well past any realistic
/// rewind distance and bounds worst-case load latency.
const COARSE_DAG_GIT_SCAN_LIMIT: u32 = 500;

/// The theorem-stating baseline can be far behind a long proof run. This
/// scan only happens when the reviewer confirms the targeted reset, so a
/// higher bound is preferable to failing on mature runs.
const THEOREM_STATING_BASELINE_GIT_SCAN_LIMIT: u32 = 5000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CorrResponse, CorrStatus, HumanChoice, HumanGateResponse, PaperResponse, RequestKind,
        ResponseStatus, ReviewDecisionKind, ReviewResponse, SoundResponse, SoundStatus, TargetId,
        TaskMode, TrustBaseMode, TrustRoutineGateState, WorkerOutcome, WorkerResponse,
    };
    use crate::trust_base::{canonical_json_value, publish_revision_closure};
    use crate::trust_base::journal::tests::{
        create_exceptional_journal_fixture, ExceptionalJournalFixture,
        EXCEPTIONAL_SUPPORT_BYTES,
    };
    use ed25519_dalek::SigningKey;
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir_in;

    fn set<T: From<String> + Ord>(items: &[&str]) -> BTreeSet<T> {
        items.iter().map(|s| T::from((*s).to_string())).collect()
    }

    fn empty_corr_node_lanes(
        lanes: &BTreeSet<String>,
    ) -> BTreeMap<String, BTreeMap<NodeId, crate::model::Update<CorrStatus>>> {
        lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect()
    }

    fn empty_corr_target_lanes(
        lanes: &BTreeSet<String>,
    ) -> BTreeMap<String, BTreeMap<TargetId, crate::model::Update<CorrStatus>>> {
        lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect()
    }

    fn empty_sound_lanes(
        lanes: &BTreeSet<String>,
    ) -> BTreeMap<String, BTreeMap<NodeId, crate::model::Update<SoundStatus>>> {
        lanes
            .iter()
            .map(|lane| (lane.clone(), BTreeMap::new()))
            .collect()
    }

    fn mark_substantiveness_pass(state: &mut ProtocolState, node: &str, fp: &str) {
        state
            .substantiveness_status
            .insert(node.into(), CorrStatus::Pass);
        state
            .substantiveness_approved_fingerprints
            .insert(node.into(), fp.into());
        state
            .live
            .substantiveness_current_fingerprints
            .insert(node.into(), fp.into());
    }

    fn base_state() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.configured_targets = set(&["t"]);
        state.proof_nodes = set(&["a"]);
        state.target_claims.insert("a".into(), set(&["t"]));
        state.live.present_nodes = set(&["a", "b"]);
        state.live.open_nodes = set(&["a", "b"]);
        state.live.coverage.insert("t".into(), set(&["a"]));
        state
            .live
            .paper_current_fingerprints
            .insert("t".into(), "a=ta".into());
        state
            .live
            .target_fingerprints
            .insert("a".into(), "ta".into());
        state
            .live
            .corr_current_fingerprints
            .insert("a".into(), "ca".into());
        state
            .live
            .corr_current_fingerprints
            .insert("b".into(), "cb".into());
        state
            .live
            .sound_current_fingerprints
            .insert("a".into(), "sa".into());
        mark_substantiveness_pass(&mut state, "a", "sub-a");
        mark_substantiveness_pass(&mut state, "b", "sub-b");
        state.committed = state.live.clone();
        state.corr_status.insert("a".into(), CorrStatus::Pass);
        state.corr_status.insert("b".into(), CorrStatus::Pass);
        state.paper_status.insert("t".into(), CorrStatus::Pass);
        state
            .corr_approved_fingerprints
            .insert("a".into(), "ca".into());
        state
            .corr_approved_fingerprints
            .insert("b".into(), "cb".into());
        state
            .paper_approved_fingerprints
            .insert("t".into(), "a=ta".into());
        state.sound_status.insert("a".into(), SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert("a".into(), "sa".into());
        state.committed_proof_nodes = state.proof_nodes.clone();
        state.committed_deps = state.deps.clone();
        state.committed_target_claims = state.target_claims.clone();
        state
    }

    fn exceptional_runtime_metadata(
        directory: &Path,
        journal_path: &Path,
        fixture: &ExceptionalJournalFixture,
    ) -> RuntimeMetadata {
        let actor_manifest_path = directory.join("actor-key-manifest.json");
        fs::write(
            &actor_manifest_path,
            fixture.actor_manifest.canonical_bytes().unwrap(),
        )
        .unwrap();
        let seed_manifest_path = directory.join("revised-seed-manifest.json");
        fs::write(
            &seed_manifest_path,
            canonical_json_value(&fixture.revised_seed_manifest).unwrap(),
        )
        .unwrap();
        let seed_bundle_path = directory.join("revised-seed-definition-bundle.json");
        fs::write(
            &seed_bundle_path,
            canonical_json_value(&fixture.revised_seed_definition_bundle).unwrap(),
        )
        .unwrap();
        let evidence_manifest_path = directory.join("revised-evidence-manifest.json");
        fs::write(
            &evidence_manifest_path,
            canonical_json_value(&fixture.revised_evidence_manifest).unwrap(),
        )
        .unwrap();
        let evidence_root_path = directory.join("revised-evidence-root");
        fs::create_dir(&evidence_root_path).unwrap();
        fs::create_dir(evidence_root_path.join("model")).unwrap();
        fs::write(
            evidence_root_path.join("model/Assumptions.lean"),
            EXCEPTIONAL_SUPPORT_BYTES,
        )
        .unwrap();
        publish_revision_closure(
            journal_path,
            &canonical_json_value(&fixture.revised_seed_manifest).unwrap(),
            &canonical_json_value(&fixture.revised_seed_definition_bundle).unwrap(),
            &evidence_root_path,
            &canonical_json_value(&fixture.revised_evidence_manifest).unwrap(),
            &fixture.revision_closure,
        )
        .unwrap();
        let manifest_root = SigningKey::from_bytes(&[3_u8; 32]).verifying_key();
        RuntimeMetadata {
            trust_journal_path: Some(journal_path.to_owned()),
            trust_actor_key_manifest_path: Some(actor_manifest_path),
            trust_seed_manifest_path: Some(seed_manifest_path),
            trust_seed_definition_bundle_path: Some(seed_bundle_path),
            trust_evidence_tool_manifest_path: Some(evidence_manifest_path),
            trust_evidence_tool_root_path: Some(evidence_root_path),
            trust_manifest_authority_roots: BTreeMap::from([(
                "fixture-v1-manifest-root".into(),
                manifest_root
                    .to_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            )]),
            ..RuntimeMetadata::default()
        }
    }

    fn stale_required_v1_state(
        fixture: &ExceptionalJournalFixture,
        phase: Phase,
        active_revision_lane_id: Option<&str>,
    ) -> ProtocolState {
        let mut state = base_state();
        state.pv_tablet_configured = true;
        for node in state.live.present_nodes.clone() {
            state
                .node_difficulty
                .insert(node.clone(), crate::model::NodeDifficulty::Hard);
            state.easy_attempts.insert(node, 0);
        }
        state.phase = phase;
        state.stage = crate::model::Stage::Start;
        state.trust_base.mode = TrustBaseMode::RequiredV1;
        state.trust_base.seed_manifest_sha256 = Some(fixture.revision_closure.seed_manifest_sha256);
        state.trust_base.seed_definition_bundle_sha256 =
            Some(fixture.revision_closure.seed_definition_bundle_sha256);
        state.trust_base.evidence_tool_manifest_sha256 =
            Some(fixture.revision_closure.evidence_manifest_sha256);
        state.trust_base.authored_semantic_root =
            Some(fixture.revision_closure.authored_semantic_root);
        state.trust_base.approved_evidence_tool_input_root =
            Some(fixture.revision_closure.evidence_tool_input_root);
        state.trust_base.seed_support_definitions.insert(
            NodeId::from(crate::assumptions_registry::ASSUMPTIONS_NODE),
            crate::model::TrustSeedSupportDefinition {
                logical_id: "aeneas-validity-definitions".into(),
                evidence_relative_path: "model/Assumptions.lean".into(),
                raw_sha256: crate::trust_base::raw_sha256(EXCEPTIONAL_SUPPORT_BYTES),
            },
        );
        state.trust_base.advance_gate_episode_id = Some("fixture-routine-gate".into());
        state.trust_base.current_human_approval_event_hash =
            Some(fixture.routine_approval_event_hash);
        state.trust_base.active_revision_lane_id = active_revision_lane_id.map(str::to_owned);
        state.trust_base.routine_gate_state = TrustRoutineGateState::Approved;
        state
    }

    #[test]
    fn legacy_required_v1_checkpoint_derives_missing_seed_support_projection() {
        let evidence_root = crate::trust_base::raw_sha256(b"approved evidence root");
        let file_hash = crate::trust_base::raw_sha256(EXCEPTIONAL_SUPPORT_BYTES);
        let projection = BTreeMap::from([(
            NodeId::from(crate::assumptions_registry::ASSUMPTIONS_NODE),
            crate::model::TrustSeedSupportDefinition {
                logical_id: "aeneas-validity-definitions".to_owned(),
                evidence_relative_path: "model/Assumptions.lean".to_owned(),
                raw_sha256: file_hash,
            },
        )]);
        let mut state = ProtocolState::default();
        state.trust_base.mode = TrustBaseMode::RequiredV1;
        state.trust_base.approved_evidence_tool_input_root = Some(evidence_root);

        assert!(migrate_legacy_seed_support_projection(
            &mut state,
            &projection,
            evidence_root,
        )
        .unwrap());
        assert_eq!(state.trust_base.seed_support_definitions, projection);
        assert!(!migrate_legacy_seed_support_projection(
            &mut state,
            &projection,
            evidence_root,
        )
        .unwrap());
    }

    #[test]
    fn legacy_seed_support_projection_migration_rejects_wrong_root() {
        let mut state = ProtocolState::default();
        state.trust_base.mode = TrustBaseMode::RequiredV1;
        state.trust_base.approved_evidence_tool_input_root =
            Some(crate::trust_base::raw_sha256(b"approved"));
        let projection = BTreeMap::from([(
            NodeId::from(crate::assumptions_registry::ASSUMPTIONS_NODE),
            crate::model::TrustSeedSupportDefinition {
                logical_id: "aeneas-validity-definitions".to_owned(),
                evidence_relative_path: "model/Assumptions.lean".to_owned(),
                raw_sha256: crate::trust_base::raw_sha256(EXCEPTIONAL_SUPPORT_BYTES),
            },
        )]);

        let error = migrate_legacy_seed_support_projection(
            &mut state,
            &projection,
            crate::trust_base::raw_sha256(b"unapproved"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unapproved evidence root"));
        assert!(state.trust_base.seed_support_definitions.is_empty());
    }

    #[test]
    fn reconcile_journal_ahead_revision_open_enters_restricted_revision_stating() {
        let directory = local_tempdir();
        let journal_path = directory.path().join("trust-journal");
        let fixture = create_exceptional_journal_fixture(&journal_path, None);
        let metadata = exceptional_runtime_metadata(directory.path(), &journal_path, &fixture);
        let state = stale_required_v1_state(&fixture, Phase::ProofFormalization, None);

        let runtime = SupervisorRuntime::initialize_with_metadata(
            RuntimePaths::new(directory.path().join("runtime")),
            state,
            metadata,
        )
        .unwrap();
        assert_eq!(runtime.state.phase, Phase::RevisionStating);
        assert_eq!(runtime.state.stage, crate::model::Stage::Start);
        assert_eq!(
            runtime.state.trust_base.active_revision_lane_id.as_deref(),
            Some("fixture-revision-lane")
        );
        assert_eq!(
            runtime.state.trust_base.routine_gate_state,
            TrustRoutineGateState::Approved
        );
        assert_eq!(
            runtime.state.trust_base.current_human_approval_event_hash,
            Some(fixture.routine_approval_event_hash)
        );
    }

    #[test]
    fn reconcile_journal_ahead_protected_approval_resumes_pf_with_revised_basis() {
        let directory = local_tempdir();
        let journal_path = directory.path().join("trust-journal");
        let fixture = create_exceptional_journal_fixture(
            &journal_path,
            Some(crate::trust_base::EventKind::ProtectedReapprovalApproved),
        );
        let metadata = exceptional_runtime_metadata(directory.path(), &journal_path, &fixture);
        // The initial config paths are deliberately stale/corrupt.  A
        // protected approval must resolve only the immutable digest-addressed
        // closure selected by the journal, never a mutable path rewrite.
        fs::write(
            metadata.trust_seed_manifest_path.as_ref().unwrap(),
            b"not the approved revised seed",
        )
        .unwrap();
        let mut state = stale_required_v1_state(
            &fixture,
            Phase::RevisionStating,
            Some("fixture-revision-lane"),
        );
        state
            .trust_base
            .seed_support_definitions
            .get_mut(&NodeId::from(crate::assumptions_registry::ASSUMPTIONS_NODE))
            .unwrap()
            .raw_sha256 = crate::trust_base::raw_sha256(b"prior support definition");

        let runtime_root = directory.path().join("runtime");
        let runtime = SupervisorRuntime::initialize_with_metadata(
            RuntimePaths::new(&runtime_root),
            state,
            metadata,
        )
        .unwrap();
        assert_eq!(runtime.state.phase, Phase::ProofFormalization);
        assert_eq!(runtime.state.stage, crate::model::Stage::Start);
        assert_eq!(runtime.state.trust_base.active_revision_lane_id, None);
        assert_eq!(
            runtime.state.trust_base.current_human_approval_event_hash,
            fixture.terminal_event_hash
        );
        assert_eq!(
            runtime.state.trust_base.authored_semantic_root,
            Some(fixture.revision_closure.authored_semantic_root)
        );
        assert_eq!(
            runtime.state.trust_base.routine_gate_state,
            TrustRoutineGateState::Approved
        );
        assert_eq!(
            runtime
                .state
                .trust_base
                .seed_support_definitions
                .get(&NodeId::from(crate::assumptions_registry::ASSUMPTIONS_NODE))
                .unwrap()
                .raw_sha256,
            crate::trust_base::raw_sha256(EXCEPTIONAL_SUPPORT_BYTES)
        );

        drop(runtime);
        let restarted = SupervisorRuntime::load(RuntimePaths::new(runtime_root)).unwrap();
        assert_eq!(restarted.state.phase, Phase::ProofFormalization);
        assert_eq!(
            restarted.state.trust_base.current_human_approval_event_hash,
            fixture.terminal_event_hash
        );
        assert_eq!(
            restarted.state.trust_base.seed_manifest_sha256,
            Some(fixture.revision_closure.seed_manifest_sha256)
        );
    }

    #[test]
    fn trust_runtime_reload_normalizes_dispatch_hints_before_semantic_validation() {
        let directory = local_tempdir();
        let journal_path = directory.path().join("trust-journal");
        let fixture = create_exceptional_journal_fixture(
            &journal_path,
            Some(crate::trust_base::EventKind::ProtectedReapprovalApproved),
        );
        let mut metadata =
            exceptional_runtime_metadata(directory.path(), &journal_path, &fixture);
        let repo_path = directory.path().join("repo");
        seed_test_support_repo(&repo_path);
        let config_path = write_test_config(&repo_path);
        metadata.repo_path = Some(repo_path);
        metadata.config_path = Some(config_path);
        let state = stale_required_v1_state(
            &fixture,
            Phase::RevisionStating,
            Some("fixture-revision-lane"),
        );
        let runtime_root = directory.path().join("runtime");
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            RuntimePaths::new(&runtime_root),
            state,
            metadata,
        )
        .unwrap();

        runtime.state.stage = crate::model::Stage::Worker;
        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(7, RequestKind::Worker));
        runtime
            .apply_request_execution_hints()
            .expect("attach dispatch-only worker hints");
        let hinted = runtime.state.in_flight_request.as_ref().unwrap();
        assert_eq!(hinted.worker_binding.model.as_deref(), Some("worker-a"));
        assert_ne!(hinted.prompt_contract_version, 0);
        runtime.persist_state().unwrap();
        drop(runtime);

        let mut restarted = SupervisorRuntime::load(RuntimePaths::new(&runtime_root))
            .expect("trust-v1 reload must normalize hints before semantic validation");
        let request = restarted.state.in_flight_request.as_ref().unwrap();
        assert_eq!(request.id, 7);
        assert_eq!(request.kind, RequestKind::Worker);
        assert_eq!(request.worker_binding.model.as_deref(), Some("worker-a"));
        assert_ne!(request.prompt_contract_version, 0);
        assert!(restarted.active_worker_base_tablet_dir().is_dir());

        // Dispatch-only normalization must not turn into a whole-request
        // overwrite: a seed-bound semantic field remains covered by the
        // in-flight equality invariant and fails closed on the next reload.
        restarted
            .state
            .in_flight_request
            .as_mut()
            .unwrap()
            .trust_base_required_v1 = false;
        restarted.persist_state().unwrap();
        drop(restarted);
        let error = match SupervisorRuntime::load(RuntimePaths::new(runtime_root)) {
            Ok(_) => panic!("seed-bound request tampering must fail closed"),
            Err(error) => error,
        };
        let RuntimeError::InvalidRuntimeState(message) = error else {
            panic!("expected invalid runtime state, got {error:?}");
        };
        assert!(
            message.contains("in-flight request payload does not match derived state"),
            "{message}"
        );
    }

    #[test]
    fn protected_approval_restart_rejects_tampered_content_addressed_closure() {
        let directory = local_tempdir();
        let journal_path = directory.path().join("trust-journal");
        let fixture = create_exceptional_journal_fixture(
            &journal_path,
            Some(crate::trust_base::EventKind::ProtectedReapprovalApproved),
        );
        let metadata = exceptional_runtime_metadata(directory.path(), &journal_path, &fixture);
        let paths = crate::trust_base::revision_closure_paths(
            &journal_path,
            &fixture.revision_closure,
        )
        .unwrap();
        let marker = paths.root.join("CLOSURE.json");
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&marker, b"{}").unwrap();
        let state = stale_required_v1_state(
            &fixture,
            Phase::RevisionStating,
            Some("fixture-revision-lane"),
        );

        let error = match SupervisorRuntime::initialize_with_metadata(
            RuntimePaths::new(directory.path().join("runtime")),
            state,
            metadata,
        ) {
            Ok(_) => panic!("tampered revision closure must fail closed"),
            Err(error) => error,
        };
        let RuntimeError::InvalidRuntimeState(message) = error else {
            panic!("expected invalid runtime state, got {error:?}");
        };
        assert!(message.contains("revision_closure_marker_mismatch"), "{message}");
    }

    #[test]
    fn reconcile_journal_ahead_protected_feedback_resumes_pf_with_prior_basis() {
        let directory = local_tempdir();
        let journal_path = directory.path().join("trust-journal");
        let fixture = create_exceptional_journal_fixture(
            &journal_path,
            Some(crate::trust_base::EventKind::ProtectedReapprovalFeedback),
        );
        let metadata = exceptional_runtime_metadata(directory.path(), &journal_path, &fixture);
        let state = stale_required_v1_state(
            &fixture,
            Phase::RevisionStating,
            Some("fixture-revision-lane"),
        );

        let runtime = SupervisorRuntime::initialize_with_metadata(
            RuntimePaths::new(directory.path().join("runtime")),
            state,
            metadata,
        )
        .unwrap();
        assert_eq!(runtime.state.phase, Phase::ProofFormalization);
        assert_eq!(runtime.state.stage, crate::model::Stage::Start);
        assert_eq!(runtime.state.trust_base.active_revision_lane_id, None);
        assert_eq!(
            runtime.state.trust_base.current_human_approval_event_hash,
            Some(fixture.routine_approval_event_hash)
        );
        assert_eq!(
            runtime.state.trust_base.routine_gate_state,
            TrustRoutineGateState::Approved
        );
    }

    #[test]
    fn reconcile_protected_feedback_fails_closed_without_worktree_rollback_proof() {
        let directory = local_tempdir();
        let journal_path = directory.path().join("trust-journal");
        let fixture = create_exceptional_journal_fixture(
            &journal_path,
            Some(crate::trust_base::EventKind::ProtectedReapprovalFeedback),
        );
        let mut metadata = exceptional_runtime_metadata(directory.path(), &journal_path, &fixture);
        let repo_path = directory.path().join("repo");
        fs::create_dir(&repo_path).unwrap();
        metadata.repo_path = Some(repo_path);
        let state = stale_required_v1_state(
            &fixture,
            Phase::RevisionStating,
            Some("fixture-revision-lane"),
        );

        let error = match SupervisorRuntime::initialize_with_metadata(
            RuntimePaths::new(directory.path().join("runtime")),
            state,
            metadata,
        ) {
            Ok(_) => panic!("feedback recovery without rollback proof must fail closed"),
            Err(error) => error,
        };
        let RuntimeError::InvalidRuntimeState(message) = error else {
            panic!("expected fail-closed invalid state, got {error:?}");
        };
        assert!(message.contains("cannot prove"), "{message}");
        assert!(message.contains("rolled back"), "{message}");
    }

    #[test]
    fn initialize_normalizes_total_target_corr_fingerprints() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let mut state = ProtocolState::default();
        state.configured_targets = set(&["t"]);
        state.live.present_nodes = set(&["Preamble"]);
        state.committed.present_nodes = set(&["Preamble"]);
        state
            .live
            .corr_current_fingerprints
            .insert("Preamble".into(), "".into());
        state
            .live
            .target_fingerprints
            .insert("Preamble".into(), "".into());
        state
            .committed
            .corr_current_fingerprints
            .insert("Preamble".into(), "".into());
        state
            .committed
            .target_fingerprints
            .insert("Preamble".into(), "".into());

        let runtime =
            SupervisorRuntime::initialize_with_metadata(paths, state, RuntimeMetadata::default())
                .expect("initialize runtime");

        assert_eq!(
            runtime.state.live.paper_current_fingerprints.get("t"),
            Some(&"".to_string())
        );
        assert_eq!(
            runtime.state.committed.paper_current_fingerprints.get("t"),
            Some(&"".to_string())
        );
    }

    fn local_tempdir() -> tempfile::TempDir {
        let tmp_root = std::env::current_dir()
            .expect("current dir")
            .join(".tmp-tests");
        fs::create_dir_all(&tmp_root).expect("tmp root");
        tempdir_in(&tmp_root).expect("tempdir")
    }

    /// Cone-clean artifact invalidation (unitdistance cycle 694): pruning a
    /// node's source must also drop (a) the pruned module's own Lake build
    /// artifacts in BOTH `lib/lean/Tablet/` and `ir/Tablet/`, and (b) the
    /// artifacts of surviving dependents whose cached `ir/Tablet/<m>.setup.json`
    /// import graph still references a pruned module — otherwise `lake build`
    /// hard-fails on the missing olean even though no current source imports
    /// the pruned node. Unrelated modules' artifacts must survive, including
    /// a dependent that imports a LIVE module whose name has a pruned module's
    /// name as a strict prefix (substring matching would wrongly flag it).
    #[test]
    fn purge_stale_tablet_build_artifacts_invalidates_cached_import_dependents() {
        let dir = local_tempdir();
        let repo = dir.path();
        let tablet = repo.join("Tablet");
        let lib = repo.join(".lake/build/lib/lean/Tablet");
        let ir = repo.join(".lake/build/ir/Tablet");
        fs::create_dir_all(&tablet).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&ir).unwrap();

        // Live sources. `Pruned` and `AlsoPruned` were cone-cleaned (no
        // source), exercising multiple deletions in one burst. `PrunedExtra`
        // is live and has `Pruned` as a strict name prefix.
        for stem in ["Preamble", "Dep", "DepTwo", "Bystander", "PrunedExtra", "Broken"] {
            fs::write(tablet.join(format!("{stem}.lean")), "-- source\n").unwrap();
        }

        let write_artifacts = |stem: &str| {
            fs::write(lib.join(format!("{stem}.olean")), b"olean").unwrap();
            fs::write(lib.join(format!("{stem}.ilean")), b"ilean").unwrap();
            fs::write(lib.join(format!("{stem}.olean.hash")), b"hash").unwrap();
            fs::write(ir.join(format!("{stem}.c")), b"c").unwrap();
        };
        for stem in [
            "Preamble",
            "Dep",
            "DepTwo",
            "Bystander",
            "PrunedExtra",
            "Broken",
            "Pruned",
            "AlsoPruned",
        ] {
            write_artifacts(stem);
        }
        // Cached import graphs. `Dep` references pruned `Tablet.Pruned` via
        // an `importArts`-style object key; `DepTwo` references pruned
        // `Tablet.AlsoPruned` via a plain string array (schema variation).
        // `Bystander` imports only live modules — including `Tablet.PrunedExtra`,
        // the substring trap. `Broken` has an unparseable setup.json and is
        // conservatively invalidated.
        fs::write(
            ir.join("Dep.setup.json"),
            r#"{"name":"Tablet.Dep","importArts":{"Tablet.Pruned":["x"],"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("DepTwo.setup.json"),
            r#"{"name":"Tablet.DepTwo","imports":["Tablet.AlsoPruned","Tablet.Preamble"]}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Bystander.setup.json"),
            r#"{"name":"Tablet.Bystander","importArts":{"Tablet.PrunedExtra":["x"],"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("PrunedExtra.setup.json"),
            r#"{"name":"Tablet.PrunedExtra","importArts":{"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Preamble.setup.json"),
            r#"{"name":"Tablet.Preamble","importArts":{}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Pruned.setup.json"),
            r#"{"name":"Tablet.Pruned","importArts":{"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(ir.join("Broken.setup.json"), "{not json").unwrap();

        purge_stale_tablet_build_artifacts(repo);

        // Pruned modules' own artifacts are gone from both build dirs.
        for stem in ["Pruned", "AlsoPruned"] {
            assert!(!lib.join(format!("{stem}.olean")).exists(), "{stem} olean");
            assert!(!lib.join(format!("{stem}.ilean")).exists(), "{stem} ilean");
            assert!(
                !lib.join(format!("{stem}.olean.hash")).exists(),
                "{stem} olean.hash"
            );
            assert!(!ir.join(format!("{stem}.c")).exists(), "{stem} ir .c");
        }
        assert!(!ir.join("Pruned.setup.json").exists(), "Pruned setup.json");
        // Dependents with a stale cached import graph are invalidated (they
        // rebuild from their surviving source), as is the unparseable one.
        for stem in ["Dep", "DepTwo", "Broken"] {
            assert!(
                !lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should be invalidated"
            );
            assert!(
                !ir.join(format!("{stem}.setup.json")).exists(),
                "{stem} setup.json should be invalidated"
            );
            assert!(
                !ir.join(format!("{stem}.c")).exists(),
                "{stem} ir .c should be invalidated"
            );
            assert!(
                tablet.join(format!("{stem}.lean")).exists(),
                "{stem} source must never be touched"
            );
        }
        // Unrelated modules survive untouched — including the substring trap.
        for stem in ["Preamble", "Bystander", "PrunedExtra"] {
            assert!(
                lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should survive"
            );
            assert!(
                ir.join(format!("{stem}.setup.json")).exists(),
                "{stem} setup.json should survive"
            );
            assert!(
                ir.join(format!("{stem}.c")).exists(),
                "{stem} ir .c should survive"
            );
        }
    }

    /// Edit-driven artifact invalidation (unitdistance cycle 696,
    /// reviewer-3116): an ACCEPTED worker edit to a Tablet source must drop
    /// (a) the edited module's own Lake build artifacts in BOTH
    /// `lib/lean/Tablet/` and `ir/Tablet/` (the pre-edit olean is a phantom
    /// that bare `lake env lean` probes would import, displaying the
    /// pre-edit signature), and (b) the artifacts of dependents whose cached
    /// `ir/Tablet/<m>.setup.json` import graph references an edited module.
    /// Unrelated modules' artifacts must survive, including a dependent that
    /// imports a live untouched module whose name has an edited module's
    /// name as a strict prefix (substring matching would wrongly flag it).
    /// A deleted-source stem in the same burst is swept by the same call
    /// (delete+edit in one burst), and re-running the purge is an idempotent
    /// no-op on the already-removed files.
    #[test]
    fn purge_invalidated_tablet_build_artifacts_invalidates_edited_stems_and_dependents() {
        let dir = local_tempdir();
        let repo = dir.path();
        let tablet = repo.join("Tablet");
        let lib = repo.join(".lake/build/lib/lean/Tablet");
        let ir = repo.join(".lake/build/ir/Tablet");
        fs::create_dir_all(&tablet).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&ir).unwrap();

        // Live sources. `Edited` was modified by the accepted burst (source
        // still on disk — unlike the deletion trigger). `EditedExtra` is
        // live, untouched, and has `Edited` as a strict name prefix.
        // `Gone` was deleted by the same burst (no source on disk).
        for stem in [
            "Preamble",
            "Edited",
            "Dep",
            "DepTwo",
            "Bystander",
            "EditedExtra",
        ] {
            fs::write(tablet.join(format!("{stem}.lean")), "-- source\n").unwrap();
        }

        let write_artifacts = |stem: &str| {
            fs::write(lib.join(format!("{stem}.olean")), b"olean").unwrap();
            fs::write(lib.join(format!("{stem}.ilean")), b"ilean").unwrap();
            fs::write(lib.join(format!("{stem}.olean.hash")), b"hash").unwrap();
            fs::write(ir.join(format!("{stem}.c")), b"c").unwrap();
        };
        for stem in [
            "Preamble",
            "Edited",
            "Dep",
            "DepTwo",
            "Bystander",
            "EditedExtra",
            "Gone",
        ] {
            write_artifacts(stem);
        }
        // Cached import graphs. `Dep` references edited `Tablet.Edited` via
        // an `importArts`-style object key; `DepTwo` references it via a
        // plain string array (schema variation). `Bystander` imports only
        // live untouched modules — including `Tablet.EditedExtra`, the
        // substring trap.
        fs::write(
            ir.join("Dep.setup.json"),
            r#"{"name":"Tablet.Dep","importArts":{"Tablet.Edited":["x"],"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("DepTwo.setup.json"),
            r#"{"name":"Tablet.DepTwo","imports":["Tablet.Edited","Tablet.Preamble"]}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Bystander.setup.json"),
            r#"{"name":"Tablet.Bystander","importArts":{"Tablet.EditedExtra":["x"],"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("EditedExtra.setup.json"),
            r#"{"name":"Tablet.EditedExtra","importArts":{"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();
        fs::write(
            ir.join("Preamble.setup.json"),
            r#"{"name":"Tablet.Preamble","importArts":{}}"#,
        )
        .unwrap();
        // The edited module's own setup.json references only Preamble; it
        // is purged via the direct invalidated-stem clause, not the
        // dependent scan.
        fs::write(
            ir.join("Edited.setup.json"),
            r#"{"name":"Tablet.Edited","importArts":{"Tablet.Preamble":["y"]}}"#,
        )
        .unwrap();

        let edited: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::from(["Edited".to_string()]);
        purge_invalidated_tablet_build_artifacts(repo, &edited);

        // The edited module's own artifacts are gone from both build dirs,
        // even though its source is still live; the same-burst deleted
        // stem's artifacts are swept by the same call.
        for stem in ["Edited", "Gone"] {
            assert!(!lib.join(format!("{stem}.olean")).exists(), "{stem} olean");
            assert!(!lib.join(format!("{stem}.ilean")).exists(), "{stem} ilean");
            assert!(
                !lib.join(format!("{stem}.olean.hash")).exists(),
                "{stem} olean.hash"
            );
            assert!(!ir.join(format!("{stem}.c")).exists(), "{stem} ir .c");
        }
        assert!(!ir.join("Edited.setup.json").exists(), "Edited setup.json");
        // Dependents whose cached import graph references the edited module
        // are invalidated (they rebuild from their surviving source).
        for stem in ["Dep", "DepTwo"] {
            assert!(
                !lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should be invalidated"
            );
            assert!(
                !ir.join(format!("{stem}.setup.json")).exists(),
                "{stem} setup.json should be invalidated"
            );
            assert!(
                !ir.join(format!("{stem}.c")).exists(),
                "{stem} ir .c should be invalidated"
            );
            assert!(
                tablet.join(format!("{stem}.lean")).exists(),
                "{stem} source must never be touched"
            );
        }
        // The edited module's SOURCE is never touched.
        assert!(
            tablet.join("Edited.lean").exists(),
            "edited source must never be touched"
        );
        // Unrelated modules survive untouched — including the substring trap.
        for stem in ["Preamble", "Bystander", "EditedExtra"] {
            assert!(
                lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should survive"
            );
            assert!(
                ir.join(format!("{stem}.setup.json")).exists(),
                "{stem} setup.json should survive"
            );
            assert!(
                ir.join(format!("{stem}.c")).exists(),
                "{stem} ir .c should survive"
            );
        }

        // Idempotence (delete+edit interplay with the part-1 rewind-path
        // purge): a second pass over the same stems — or the deletion-keyed
        // wrapper that a later rewind would run — must be a quiet no-op and
        // must not disturb the survivors.
        purge_invalidated_tablet_build_artifacts(repo, &edited);
        purge_stale_tablet_build_artifacts(repo);
        for stem in ["Preamble", "Bystander", "EditedExtra"] {
            assert!(
                lib.join(format!("{stem}.olean")).exists(),
                "{stem} olean should survive repeated purges"
            );
        }
    }

    fn seed_test_support_repo(repo: &Path) {
        fs::create_dir_all(repo.join(".trellis/scripts")).expect("script dir");
        fs::create_dir_all(repo.join("Tablet")).expect("tablet dir");
        fs::write(
            repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        )
        .expect("write preamble lean");
        fs::write(
            repo.join("Tablet/Assumptions.lean"),
            EXCEPTIONAL_SUPPORT_BYTES,
        )
        .expect("write trust support lean");
        fs::write(repo.join("Tablet/Preamble.tex"), "").expect("write preamble tex");
        fs::write(
            repo.join("Tablet/a.lean"),
            "import Tablet.Preamble\n\ntheorem a : True := by\n  sorry\n",
        )
        .expect("write a lean");
        fs::write(
            repo.join("Tablet/a.tex"),
            "\\begin{theorem}a\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        )
        .expect("write a tex");
        fs::write(
            repo.join("Tablet/b.lean"),
            "import Tablet.Preamble\n\ndef b : Nat := by\n  sorry\n",
        )
        .expect("write b lean");
        fs::write(
            repo.join("Tablet/b.tex"),
            "\\begin{definition}b\\end{definition}\n",
        )
        .expect("write b tex");
        let check_path = repo.join(".trellis/scripts/check.py");
        fs::write(
            &check_path,
            "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("write check script");
        let mut perms = fs::metadata(&check_path)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&check_path, perms).expect("chmod script");
    }

    fn write_test_config(repo: &Path) -> PathBuf {
        let config_path = repo.join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "repo_path": repo,
                "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
                "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
                "workflow": {}
            })
            .to_string(),
        )
        .expect("write config");
        config_path
    }

    fn write_test_config_with_verifiers(repo: &Path) -> PathBuf {
        let config_path = repo.join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "repo_path": repo,
                "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
                "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
                "workflow": {},
                "verification": {
                    "correspondence_agents": [
                        {"provider": "claude", "model": "corr-a", "label": "corr-a"},
                        {"provider": "gemini", "model": "corr-b", "label": "corr-b"}
                    ],
                    "soundness_agents": [
                        {"provider": "claude", "model": "sound-a", "label": "sound-a"},
                        {"provider": "gemini", "model": "sound-b", "label": "sound-b"}
                    ]
                }
            })
            .to_string(),
        )
        .expect("write config");
        config_path
    }

    fn init_git_repo(repo: &Path) {
        let commands = [
            vec!["init".to_string()],
            vec![
                "config".to_string(),
                "user.name".to_string(),
                "trellis-test".to_string(),
            ],
            vec![
                "config".to_string(),
                "user.email".to_string(),
                "trellis-test@example.com".to_string(),
            ],
            vec!["add".to_string(), "-A".to_string()],
            vec![
                "commit".to_string(),
                "-m".to_string(),
                "Initial commit".to_string(),
            ],
        ];
        for command in commands {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(&command)
                .status()
                .expect("run git command");
            assert!(
                status.success(),
                "git command failed: git -C {} {}",
                repo.display(),
                command.join(" ")
            );
        }
    }

    fn commit_all(repo: &Path, message: &str) {
        for command in [
            vec!["add".to_string(), "-A".to_string()],
            vec!["commit".to_string(), "-m".to_string(), message.to_string()],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(&command)
                .status()
                .expect("run git command");
            assert!(
                status.success(),
                "git command failed: git -C {} {}",
                repo.display(),
                command.join(" ")
            );
        }
    }

    struct QueueAdapter {
        responses: VecDeque<WrapperResponse>,
    }

    impl QueueAdapter {
        fn new(responses: Vec<WrapperResponse>) -> Self {
            Self {
                responses: responses.into(),
            }
        }
    }

    impl WrapperAdapter for QueueAdapter {
        fn dispatch(&mut self, request: &WrapperRequest) -> Result<WrapperResponse, String> {
            let response = self
                .responses
                .pop_front()
                .ok_or_else(|| format!("no response queued for request {:?}", request))?;
            Ok(response)
        }
    }

    fn minimal_package_ready_state() -> ProtocolState {
        let digest = crate::trust_base::raw_sha256(b"runtime-package-ready-fixture");
        let mut state = ProtocolState::default();
        state.pv_tablet_configured = true;
        state.cycle = 1;
        state.phase = Phase::Cleanup;
        state.stage = crate::model::Stage::Start;
        state.trust_base.mode = TrustBaseMode::RequiredV1;
        state.trust_base.seed_manifest_sha256 = Some(digest);
        state.trust_base.seed_definition_bundle_sha256 = Some(digest);
        state.trust_base.evidence_tool_manifest_sha256 = Some(digest);
        state.trust_base.authored_semantic_root = Some(digest);
        state.trust_base.approved_evidence_tool_input_root = Some(digest);
        state.trust_base.seed_support_definitions.insert(
            NodeId::from(crate::assumptions_registry::ASSUMPTIONS_NODE),
            crate::model::TrustSeedSupportDefinition {
                logical_id: "aeneas-validity-definitions".into(),
                evidence_relative_path: "model/Assumptions.lean".into(),
                raw_sha256: digest,
            },
        );
        state.trust_base.journal_checkpoint = Some(
            crate::trust_base::JournalCheckpointBinding {
                journal_id: "runtime-package-ready-fixture".into(),
                sequence_number: 7,
                event_hash: digest,
                projection_root: digest,
            },
        );
        state.trust_base.advance_gate_episode_id = Some("sole-advance-gate".into());
        state.trust_base.current_human_approval_event_hash = Some(digest);
        state.trust_base.routine_gate_state = TrustRoutineGateState::Approved;
        state.trust_base.package_ready = true;
        state
    }

    #[test]
    fn package_ready_runtime_stops_then_finalizes_only_after_authorization() {
        let directory = local_tempdir();
        let paths = RuntimePaths::new(directory.path().join("runtime"));
        fs::create_dir_all(&paths.root).unwrap();
        let state = minimal_package_ready_state();
        state.validate().expect("package-ready fixture must validate");
        let mut runtime = SupervisorRuntime {
            paths,
            state,
            metadata: RuntimeMetadata::default(),
            event_count: 0,
        };
        let mut adapter = QueueAdapter::new(vec![]);

        let waiting = runtime.step(&mut adapter).unwrap();
        assert_eq!(waiting.status, RuntimeStepStatus::PackageReady);
        assert!(waiting.event.is_none());
        assert_eq!(runtime.event_count, 0);
        assert_eq!(runtime.state.phase, Phase::Cleanup);

        runtime.state.trust_base.package_authorization_event_hash =
            Some(crate::trust_base::raw_sha256(b"authorized-package"));
        let finalized = runtime.step(&mut adapter).unwrap();
        assert_eq!(finalized.status, RuntimeStepStatus::Transitioned);
        assert_eq!(
            finalized.event,
            Some(ProtocolEvent::FinalizeAuthorizedPackage)
        );
        assert_eq!(runtime.state.phase, Phase::Complete);
        assert_eq!(runtime.state.stage, crate::model::Stage::Complete);
        assert!(!runtime.state.trust_base.package_ready);

        let terminal = runtime.step(&mut adapter).unwrap();
        assert_eq!(terminal.status, RuntimeStepStatus::Complete);
        assert!(terminal.event.is_none());
    }

    #[derive(Default)]
    struct RecordingCheckpointSink {
        payloads: Vec<CheckpointHookPayload>,
        fail_with: Option<String>,
    }

    impl CheckpointSink for RecordingCheckpointSink {
        fn commit(&mut self, payload: &CheckpointHookPayload) -> Result<(), String> {
            self.payloads.push(payload.clone());
            if let Some(message) = self.fail_with.clone() {
                return Err(message);
            }
            Ok(())
        }
    }

    #[test]
    fn parity_request_fresh_context_tracks_tla_native_history_policy() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("repo dir");
        seed_test_support_repo(&repo);
        let config_path = repo.join("trellis.config.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "repo_path": repo,
                "policy_path": "trellis.policy.json",
                "worker": {"provider": "codex", "model": "worker-a", "label": "worker-a"},
                "reviewer": {"provider": "codex", "model": "reviewer-a", "label": "reviewer-a"},
                "workflow": {},
                "verification": {
                    "correspondence_agents": [
                        {"provider": "claude", "model": "corr-a", "label": "claude-a"},
                        {"provider": "gemini", "model": "corr-b", "label": "gemini-b"}
                    ],
                    "soundness_agents": [
                        {"provider": "claude", "model": "snd-a", "label": "claude-a"},
                        {"provider": "gemini", "model": "snd-b", "label": "gemini-b"}
                    ]
                }
            })
            .to_string(),
        )
        .expect("write config");
        fs::write(repo.join("trellis.policy.json"), "{}").expect("write policy");
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(1, RequestKind::Worker));
        runtime
            .apply_request_execution_hints()
            .expect("apply request execution hints");
        assert!(
            runtime
                .state
                .in_flight_request
                .as_ref()
                .expect("worker request")
                .fresh_context
        );

        runtime.record_native_history(RequestKind::Worker, Phase::TheoremStating);
        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(2, RequestKind::Worker));
        runtime
            .apply_request_execution_hints()
            .expect("apply request execution hints");
        assert!(
            !runtime
                .state
                .in_flight_request
                .as_ref()
                .expect("repeat worker request")
                .fresh_context
        );

        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(3, RequestKind::Review));
        runtime
            .apply_request_execution_hints()
            .expect("apply request execution hints");
        assert!(
            runtime
                .state
                .in_flight_request
                .as_ref()
                .expect("first review request")
                .fresh_context
        );

        runtime.record_native_history(RequestKind::Review, Phase::TheoremStating);
        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(4, RequestKind::Review));
        runtime
            .apply_request_execution_hints()
            .expect("apply request execution hints");
        assert!(
            !runtime
                .state
                .in_flight_request
                .as_ref()
                .expect("repeat review request")
                .fresh_context
        );

        runtime.state.phase = Phase::ProofFormalization;
        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(4, RequestKind::Worker));
        runtime
            .apply_request_execution_hints()
            .expect("apply request execution hints");
        assert!(
            runtime
                .state
                .in_flight_request
                .as_ref()
                .expect("first proof worker request")
                .fresh_context
        );

        runtime.record_native_history(RequestKind::Worker, Phase::ProofFormalization);
        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(5, RequestKind::Worker));
        runtime
            .apply_request_execution_hints()
            .expect("apply request execution hints");
        assert!(
            !runtime
                .state
                .in_flight_request
                .as_ref()
                .expect("repeat proof worker request")
                .fresh_context
        );

        for (request_id, kind) in [(6, RequestKind::Corr), (7, RequestKind::Sound)] {
            runtime.state.in_flight_request =
                Some(runtime.state.expected_request(request_id, kind));
            runtime
                .apply_request_execution_hints()
                .expect("apply request execution hints");
            assert!(
                runtime
                    .state
                    .in_flight_request
                    .as_ref()
                    .expect("verification or gate request")
                    .fresh_context
            );
        }

        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(9, RequestKind::HumanGate));
        runtime
            .apply_request_execution_hints()
            .expect("apply request execution hints");
        assert!(
            !runtime
                .state
                .in_flight_request
                .as_ref()
                .expect("human gate request")
                .fresh_context
        );
    }

    #[test]
    fn support_required_request_without_repo_metadata_is_invalid() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: None,
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(1, RequestKind::Worker));
        let err = runtime
            .apply_request_execution_hints()
            .expect_err("missing repo metadata should fail");
        assert!(
            err.to_string()
                .contains("support-required request missing repo_path metadata"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parity_request_execution_hints_sync_support_when_repo_check_script_exists() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).expect("tablet dir");
        fs::create_dir_all(repo.join(".trellis/scripts")).expect("script dir");
        let command_log = repo.join("support-commands.log");
        fs::write(
            repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        )
        .expect("write preamble lean");
        fs::write(repo.join("Tablet/Preamble.tex"), "").expect("write preamble tex");
        let check_script = r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
cmd = sys.argv[1]
with Path("__COMMAND_LOG__").open("a", encoding="utf-8") as handle:
    handle.write(cmd + "\n")
if cmd == "sync-tablet-support":
    print(json.dumps({
        "updated_paths": ["Tablet/README.md", "Tablet/INDEX.md", "Tablet/header.tex"],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }))
elif cmd == "prepare-compiled-support":
    print(json.dumps({
        "returncode": 0,
        "stdout": "prepared",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "materialize-tablet-oleans":
    print(json.dumps({
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
else:
    raise SystemExit(f"unexpected subcommand: {cmd}")
"#
        .replace("__COMMAND_LOG__", &command_log.display().to_string());
        fs::write(repo.join(".trellis/scripts/check.py"), check_script)
            .expect("write check script");
        let config_path = write_test_config_with_verifiers(&repo);
        let paths = RuntimePaths::new(dir.path().join("runtime"));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(1, RequestKind::Review));

        runtime
            .apply_request_execution_hints()
            .expect("apply request execution hints");
        assert!(repo.join("Tablet.lean").exists());
        assert_eq!(
            fs::read_to_string(command_log).expect("read command log"),
            "sync-supervisor-workspace\nsync-tablet-support\nprepare-compiled-support\nmaterialize-tablet-oleans\n"
        );
    }

    #[test]
    fn parity_request_execution_hints_populate_persisted_prompt_contracts() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let paths = RuntimePaths::new(dir.path().join("runtime"));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        runtime.state.in_flight_request =
            Some(runtime.state.expected_request(1, RequestKind::Review));

        runtime
            .apply_request_execution_hints()
            .expect("apply request execution hints");
        let request = runtime
            .state
            .in_flight_request
            .as_ref()
            .expect("review request");
        assert_eq!(
            request.prompt_contract_version,
            crate::prompt_contract_version()
        );
        assert!(request.project_invariants.is_object());
        assert!(request.corr_contract.is_object());
        assert!(request.sound_contract.is_object());
        assert!(request.worker_contract.is_object());
        assert!(request.review_contract.is_object());
    }

    #[test]
    fn step_persists_start_cycle_and_request() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![]);
        let outcome = runtime.step(&mut adapter).unwrap();
        assert_eq!(outcome.status, RuntimeStepStatus::Transitioned);
        assert_eq!(runtime.state().stage, crate::model::Stage::Worker);
        assert!(runtime.state().in_flight_request.is_some());
        assert!(paths.state_path.exists());
        // After one step the runtime is in cycle 1; its per-cycle event-log
        // file must exist under the repo's `.trellis-history/event-log/` dir.
        let event_log_dir = event_log_dir_for(&paths.root, runtime.metadata());
        assert!(event_log_cycle_file(&event_log_dir, runtime.state().cycle).exists());
    }

    #[test]
    fn reload_resumes_pending_request() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        // Keep a real worktree in the resume fixture; active-worker restore is
        // nevertheless driven solely by its captured worker-surface manifest.
        init_git_repo(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 3;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Worker));
        let mut seeded = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        // The misorder guard rejects non-initial states with an empty
        // event log; seed one record so reload sees a consistent log.
        seeded
            .append_event_log(&ProtocolEvent::StartCycle, &[])
            .unwrap();
        let active_request = seeded.state.in_flight_request.as_ref().unwrap().clone();
        seeded
            .capture_active_worker_base_for_request(&active_request)
            .expect("capture pending worker baseline before restart");
        drop(seeded);

        let mut runtime = SupervisorRuntime::load(paths.clone()).unwrap();
        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Worker(WorkerResponse {
            request_id: 1,
            cycle: 3,
            status: ResponseStatus::Ok,
            outcome: WorkerOutcome::Stuck,
            snapshot: runtime.state().live.clone(),
            difficulty_updates: BTreeMap::new(),
            ..WorkerResponse::default()
        })]);
        let outcome = runtime.step(&mut adapter).unwrap();
        // #54: Stuck worker now triggers a [RestoreWorktreeToActiveWorkerBase,
        // IssueRequest{Worker}] sequence instead of bare [IssueRequest{Worker}].
        assert!(matches!(
            outcome.commands.as_slice(),
            [
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase,
                ProtocolCommand::IssueRequest { request },
            ] if request.kind == RequestKind::Worker
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::Worker);
    }

    #[test]
    fn reload_refreshes_derived_in_flight_request_fields() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 3;
        initial.request_seq = 1;
        initial.target_edit_mode = crate::model::TargetEditMode::Targeted;
        initial.active_node = Some("a".into());
        initial.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Worker,
            cycle: 3,
            worker_context: crate::model::WorkerContext {
                enabled: true,
                validation_kind: crate::model::WorkerValidationKind::TheoremTargeted,
                authorized_nodes: set(&["a"]),
                ..crate::model::WorkerContext::default()
            },
            worker_acceptance: crate::model::WorkerAcceptanceContract::default(),
            current_present_nodes: BTreeSet::new(),
            current_node_kinds: BTreeMap::new(),
            ..WrapperRequest::default()
        });
        let mut seeded = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        // The misorder guard rejects non-initial states with an empty
        // event log; seed one record so reload sees a consistent log.
        seeded
            .append_event_log(&ProtocolEvent::StartCycle, &[])
            .unwrap();
        drop(seeded);

        let runtime = SupervisorRuntime::load(paths).unwrap();
        let request = runtime
            .state()
            .in_flight_request
            .as_ref()
            .expect("reloaded worker request");
        assert_eq!(
            request.worker_acceptance.validation_kind,
            crate::model::WorkerValidationKind::TheoremTargeted
        );
        assert_eq!(
            request.worker_acceptance.validation_execution_plan,
            vec![
                crate::model::WorkerValidationExecutionPlanStep::TheoremTargetEditScope {
                    target: Some("a".into()),
                    initial_scope: set(&["a"]),
                },
                crate::model::WorkerValidationExecutionPlanStep::ScopedTablet {
                    allowed_nodes_mode:
                        crate::model::ScopedTabletAllowedNodesMode::PreviousOrExplicit,
                    explicit_nodes: set(&["a"]),
                },
            ]
        );
        assert_eq!(request.current_present_nodes, set(&["a", "b"]));
        assert_eq!(
            request.current_node_kinds.get("a"),
            Some(&crate::model::NodeKind::Proof)
        );
        assert_eq!(
            request.current_node_kinds.get("b"),
            Some(&crate::model::NodeKind::Definition)
        );
    }

    #[test]
    fn reload_sanitizes_persisted_checker_mismatch_rejection_reasons() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let raw_reason = format!(
            "{} worker={{\"snapshot\":\"{}\"}} supervisor={{\"errors\":[\"{}\"]}}",
            crate::model::CHECKER_MISMATCH_REJECTION_PREFIX,
            "w".repeat(600_000),
            "s".repeat(600_000)
        );
        let mut initial = base_state();
        initial.phase = Phase::ProofFormalization;
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 215;
        initial.request_seq = 1;
        initial.active_node = Some("a".into());
        initial.deterministic_worker_rejection_reasons = vec![raw_reason.clone()];
        initial.in_flight_request = Some(WrapperRequest {
            id: 1,
            kind: RequestKind::Review,
            deterministic_worker_rejection_reasons: vec![raw_reason.clone()],
            review_contract: serde_json::json!({
                "request_summary": {
                    "deterministic_worker_rejection_reasons": [raw_reason.clone()],
                },
            }),
            ..WrapperRequest::default()
        });
        let mut seeded = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        // The misorder guard rejects non-initial states with an empty
        // event log; seed one record so reload sees a consistent log.
        seeded
            .append_event_log(&ProtocolEvent::StartCycle, &[])
            .unwrap();
        drop(seeded);

        let runtime = SupervisorRuntime::load(paths).unwrap();
        let request = runtime
            .state()
            .in_flight_request
            .as_ref()
            .expect("reloaded review request");
        let reason = request
            .deterministic_worker_rejection_reasons
            .first()
            .expect("sanitized reason");

        assert_eq!(request.kind, RequestKind::Review);
        assert_eq!(
            runtime.state().deterministic_worker_rejection_reasons[0],
            raw_reason
        );
        assert!(reason.starts_with(crate::model::CHECKER_MISMATCH_REJECTION_PREFIX));
        assert!(!reason.contains("worker={"));
        assert!(!reason.contains("supervisor={"));
        assert!(reason.len() < 600);
        assert_eq!(
            request.review_contract["request_summary"]["deterministic_worker_rejection_reasons"],
            serde_json::json!(request.deterministic_worker_rejection_reasons.clone())
        );
    }

    #[test]
    fn invalid_worker_retry_restores_repo_worktree_before_next_support_prep() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        let check_path = repo.join(".trellis/scripts/check.py");
        fs::write(
            &check_path,
            "#!/usr/bin/env python3\nimport json, pathlib, sys\nrepo = pathlib.Path(__file__).resolve().parents[2]\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    preamble = (repo / 'Tablet/Preamble.lean').read_text()\n    if 'BROKEN_IMPORT' in preamble:\n        json.dump({'returncode': 1, 'stdout': '', 'stderr': 'broken preamble', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n        sys.exit(0)\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("rewrite check script");
        let mut perms = fs::metadata(&check_path)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&check_path, perms).expect("chmod script");
        let original_preamble =
            fs::read_to_string(repo.join("Tablet/Preamble.lean")).expect("read original preamble");
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 1;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Worker));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        let active_request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime
            .capture_active_worker_base_for_request(&active_request)
            .expect("capture pre-worker baseline");
        fs::write(repo.join("Tablet/Preamble.lean"), "import BROKEN_IMPORT\n")
            .expect("write broken preamble");
        fs::write(
            repo.join("Tablet/orphan.lean"),
            "def orphan : True := True.intro\n",
        )
        .expect("write untracked orphan");
        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Worker(WorkerResponse {
            request_id: 1,
            cycle: 1,
            status: ResponseStatus::Ok,
            outcome: WorkerOutcome::Invalid,
            snapshot: runtime.state().live.clone(),
            difficulty_updates: BTreeMap::new(),
            ..WorkerResponse::default()
        })]);

        let outcome = runtime.step(&mut adapter).expect("retry should not fail");
        assert!(matches!(
            outcome.commands.as_slice(),
            [
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase,
                ProtocolCommand::IssueRequest { request },
            ] if request.kind == RequestKind::Worker
        ));
        assert_eq!(
            fs::read_to_string(repo.join("Tablet/Preamble.lean")).expect("restored preamble"),
            original_preamble
        );
        assert!(!repo.join("Tablet/orphan.lean").exists());
    }

    #[test]
    fn invalid_cleanup_retry_restores_pre_request_worker_base_before_next_support_prep() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        fs::write(
            repo.join("Tablet/c.lean"),
            "-- [TABLET NODE: c]\nimport Tablet.Preamble\n\ntheorem c : True := by\n  trivial\n",
        )
        .expect("write accepted c lean");
        fs::write(
            repo.join("Tablet/c.tex"),
            "\\begin{theorem}Synthetic accepted node c.\\end{theorem}\n",
        )
        .expect("write accepted c tex");
        // Commit c.lean/c.tex so they survive the pre-snapshot HEAD reset
        // that capture_active_worker_base_for_request now performs. Without
        // this commit, the new HEAD reset would wipe them as untracked
        // files before the next snapshot captures them.
        commit_all(&repo, "add c node");
        let check_path = repo.join(".trellis/scripts/check.py");
        fs::write(
            &check_path,
            "#!/usr/bin/env python3\nimport json, pathlib, sys\nrepo = pathlib.Path(__file__).resolve().parents[2]\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    node = repo / 'Tablet/c.lean'\n    if not node.exists():\n        json.dump({'returncode': 1, 'stdout': '', 'stderr': '[c]\\nno such file or directory\\n  file: Tablet/c.lean', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n        sys.exit(0)\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("rewrite check script");
        let mut perms = fs::metadata(&check_path)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&check_path, perms).expect("chmod script");

        let mut initial = base_state();
        initial.phase = Phase::TheoremStating;
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 1;
        initial.request_seq = 1;
        initial.live.present_nodes = set(&["Preamble", "a", "b", "c"]);
        initial.live.open_nodes = set(&["a", "b", "c"]);
        initial
            .node_kinds
            .insert("c".into(), crate::model::NodeKind::Proof);
        initial.deps.insert("c".into(), set(&["Preamble"]));
        initial.target_claims.insert("c".into(), BTreeSet::new());
        initial.normalize_all_structural_state();
        // b67ccbf: `validation_kind == Cleanup` is derived from an active
        // orphan-cleanup task (`orphan_cleanup_nodes` non-empty), not from the
        // in-flight request label alone. Park a task over the live orphans
        // (`b`, `c` are unsupported here) so the cleanup-validation pass — and
        // thus the Cleanup-labelled retry this test asserts — is reachable and
        // survives the reject_cleanup Leave path.
        initial.pending_task = Some(crate::model::PendingTask {
            task_blockers: BTreeSet::new(),
            node: initial.active_node.clone(),
            mode: initial.current_mode(),
            orphan_cleanup_nodes: set(&["b", "c"]),
            protected_semantic_change_nodes: BTreeSet::new(),
            authorized_nodes: BTreeSet::new(),
            allow_new_obligations: true,
            must_close_active: false,
            next_worker_context_mode: crate::model::WorkerContextMode::Resume,
            paper_focus_ranges: Vec::new(),
            work_style_hint: crate::model::WorkerWorkStyleHint::Restructure,
            consumed_global_repair_grant: false,
            node_retirement: None,
        });
        let mut request = initial.expected_request(1, RequestKind::Worker);
        request.worker_context.validation_kind = crate::model::WorkerValidationKind::Cleanup;
        request.current_present_nodes = initial.live.present_nodes.clone();
        initial.in_flight_request = Some(request);

        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial.clone(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        // Simulate the active_worker_base capture that would have happened at
        // the end of the prior step() when the in-flight worker request was
        // issued. Under #54, kernel emits RestoreWorktreeToActiveWorkerBase
        // unconditionally on cleanup-retry rejection (see implementation
        // note in reject_cleanup_worker_response).
        let active_request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime
            .capture_active_worker_base_for_request(&active_request)
            .expect("seed active worker base");

        struct DirtyInvalidCleanupAdapter {
            repo: PathBuf,
            snapshot: WorkingSnapshot,
        }

        impl WrapperAdapter for DirtyInvalidCleanupAdapter {
            fn dispatch(&mut self, _request: &WrapperRequest) -> Result<WrapperResponse, String> {
                fs::remove_file(self.repo.join("Tablet/c.lean")).map_err(|err| err.to_string())?;
                fs::remove_file(self.repo.join("Tablet/c.tex")).map_err(|err| err.to_string())?;
                Ok(WrapperResponse::Worker(WorkerResponse {
                    request_id: 1,
                    cycle: 1,
                    status: ResponseStatus::Ok,
                    outcome: WorkerOutcome::Invalid,
                    snapshot: self.snapshot.clone(),
                    ..WorkerResponse::default()
                }))
            }
        }

        let mut adapter = DirtyInvalidCleanupAdapter {
            repo: repo.clone(),
            snapshot: initial.live.clone(),
        };

        let outcome = runtime
            .step(&mut adapter)
            .expect("cleanup retry should not fail");
        // #54: cleanup-retry rejection emits [RestoreWorktreeToActiveWorkerBase,
        // IssueRequest{Worker}]. Disk gets restored so worker's destructive
        // delete doesn't leave state.live (still has `c`) and disk (lacks `c`)
        // out of sync.
        assert!(matches!(
            outcome.commands.as_slice(),
            [
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase,
                ProtocolCommand::IssueRequest { request },
            ] if request.kind == RequestKind::Worker
                && request.worker_context.validation_kind == crate::model::WorkerValidationKind::Cleanup
                && request.current_present_nodes.contains("c")
        ));
        assert!(repo.join("Tablet/c.lean").exists());
        assert!(repo.join("Tablet/c.tex").exists());
    }

    #[test]
    fn stuck_worker_retry_restores_repo_worktree_and_captures_snapshot() {
        // A Stuck worker that left dirty state on disk (out-of-scope edits,
        // partial proofs, whatever) used to leak its modifications across
        // bursts because the kernel only rolled back on Invalid/Malformed.
        // After the predicate broadening, Stuck triggers the same rollback +
        // last_invalid snapshot capture as Invalid does.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        let check_path = repo.join(".trellis/scripts/check.py");
        fs::write(
            &check_path,
            "#!/usr/bin/env python3\nimport json, sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("rewrite check script");
        let mut perms = fs::metadata(&check_path)
            .expect("script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&check_path, perms).expect("chmod script");
        // The original (HEAD-committed) preamble that the worker request
        // baseline will restore to.
        let original_preamble =
            fs::read_to_string(repo.join("Tablet/Preamble.lean")).expect("read original preamble");
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 1;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Worker));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        let active_request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime
            .capture_active_worker_base_for_request(&active_request)
            .expect("capture pre-worker baseline");
        // Simulate the failure mode that motivated this fix: the worker burst
        // left a sibling file modified out-of-scope after its baseline was
        // captured. The retry must discard both writes.
        fs::write(
            repo.join("Tablet/Preamble.lean"),
            "import OUT_OF_SCOPE_MODIFICATION\n",
        )
        .expect("write contract-violating preamble edit");
        fs::write(
            repo.join("Tablet/leftover_orphan.lean"),
            "def leftover : True := True.intro\n",
        )
        .expect("write untracked leftover");
        // The worker reports Stuck. Under the OLD contract the kernel
        // assumed the worker had reverted its changes; under the new
        // contract the kernel snapshots and rolls back unconditionally.
        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Worker(WorkerResponse {
            request_id: 1,
            cycle: 1,
            status: ResponseStatus::Ok,
            outcome: WorkerOutcome::Stuck,
            snapshot: runtime.state().live.clone(),
            difficulty_updates: BTreeMap::new(),
            ..WorkerResponse::default()
        })]);

        let outcome = runtime
            .step(&mut adapter)
            .expect("stuck step should not fail");
        // Stuck routes through a Worker retry first (continue_worker_retry
        // returns true while stuck-retries remain); only when retries are
        // exhausted does it begin_retry_review and emit Reviewer. With a
        // fresh state it's the retry path. Under #54 the kernel emits
        // [RestoreWorktreeToActiveWorkerBase, IssueRequest{Worker}].
        assert!(matches!(
            outcome.commands.as_slice(),
            [
                ProtocolCommand::RestoreWorktreeToActiveWorkerBase,
                ProtocolCommand::IssueRequest { request },
            ] if request.kind == RequestKind::Worker
        ));
        // Disk MUST be back to baseline — the out-of-scope modification
        // was discarded, the leftover untracked file was cleaned.
        assert_eq!(
            fs::read_to_string(repo.join("Tablet/Preamble.lean")).expect("restored preamble"),
            original_preamble,
            "Stuck worker's out-of-scope Preamble edit should be rolled back"
        );
        assert!(
            !repo.join("Tablet/leftover_orphan.lean").exists(),
            "Stuck worker's untracked leftover should be cleaned"
        );
        // The pre-rollback Tablet snapshot MUST be preserved at the
        // last_invalid sidecar so the next worker's prompt can show
        // the prior attempt's WIP.
        let last_invalid_preamble =
            repo.join(".trellis-history/worker_state/last_invalid/Tablet/Preamble.lean");
        assert!(
            last_invalid_preamble.exists(),
            "Stuck snapshot should be captured to last_invalid sidecar"
        );
        assert_eq!(
            fs::read_to_string(&last_invalid_preamble).expect("read sidecar preamble"),
            "import OUT_OF_SCOPE_MODIFICATION\n",
            "sidecar should contain the worker's WIP, not the rolled-back baseline"
        );
        let last_invalid_metadata =
            repo.join(".trellis-history/worker_state/last_invalid/metadata.json");
        let metadata_text =
            fs::read_to_string(&last_invalid_metadata).expect("read sidecar metadata");
        assert!(
            metadata_text.contains("\"outcome\": \"Stuck\""),
            "metadata.json should record outcome=Stuck; got {metadata_text}"
        );
    }

    #[test]
    fn valid_response_rejected_by_kernel_rule_preserves_wip_snapshot() {
        // dec2flt request 938 (2026-07-03): a Valid, checker-passing
        // response was rejected post-hoc by the engine's live-orphan rule.
        // The rollback ran but no WIP snapshot was written (the old capture
        // gate keyed on non-Valid outcomes only), so the retry prompt
        // pointed at a nonexistent `last_invalid` and the work was lost.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 1;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Worker));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        fs::write(
            repo.join("Tablet/Preamble.lean"),
            "import VALID_BUT_REJECTED_WIP\n",
        )
        .expect("write wip");
        let response = WorkerResponse {
            request_id: 1,
            cycle: 1,
            status: ResponseStatus::Ok,
            outcome: WorkerOutcome::Valid,
            snapshot: runtime.state().live.clone(),
            ..WorkerResponse::default()
        };
        let event = ProtocolEvent::WrapperResponse {
            response: WrapperResponse::Worker(response),
        };
        let captured = runtime
            .capture_last_invalid_snapshot_for_event(&event)
            .expect("capture ok");
        assert!(
            captured.is_some(),
            "a Valid response must be captured pre-apply — the engine may still reject it"
        );
        // Simulate the engine's apply outcome: deterministic rejection of
        // the Valid response leaves the reasons non-empty (an accept would
        // clear them via clear_retry_context).
        runtime.state.deterministic_worker_rejection_reasons = vec![
            "valid worker response leaves live orphan nodes; delete same-burst or reattach \
             support"
                .into(),
        ];
        runtime
            .update_last_invalid_for_event(&event, captured.as_deref())
            .expect("update ok");
        let sidecar = repo.join(".trellis-history/worker_state/last_invalid/Tablet/Preamble.lean");
        assert!(
            sidecar.exists(),
            "kernel-rejected Valid WIP must survive in the last_invalid sidecar"
        );
        assert_eq!(
            fs::read_to_string(&sidecar).expect("read sidecar"),
            "import VALID_BUT_REJECTED_WIP\n"
        );
        let metadata = fs::read_to_string(
            repo.join(".trellis-history/worker_state/last_invalid/metadata.json"),
        )
        .expect("read metadata");
        assert!(metadata.contains("live orphan nodes"), "{metadata}");
        assert!(metadata.contains("\"outcome\": \"Valid\""), "{metadata}");

        // An ACCEPTED Valid response (reasons cleared by the engine)
        // discards the capture and removes the stale sidecar.
        runtime.state.deterministic_worker_rejection_reasons.clear();
        let captured2 = runtime
            .capture_last_invalid_snapshot_for_event(&event)
            .expect("capture ok");
        runtime
            .update_last_invalid_for_event(&event, captured2.as_deref())
            .expect("update ok");
        assert!(
            !repo
                .join(".trellis-history/worker_state/last_invalid")
                .exists(),
            "an accepted Valid response must clear the sidecar"
        );
    }

    #[test]
    fn illegal_reset_review_response_does_not_modify_repo_disk() {
        // #54: under the new ProtocolCommand-driven restore, the runtime
        // only mutates disk when the kernel emits a RestoreWorktree*
        // command. A reviewer response with `reset: LastCommit` against
        // a request whose `allowed_resets` is `{None}` is rejected as
        // illegal by `review_response_legal`; the kernel reissues Review
        // and emits NO restore command. Disk MUST be left untouched.
        // (Pre-#54 the runtime restored disk anyway via the
        // event-shape-based `restore_repo_worktree_for_event`, leading
        // to silent state-vs-disk divergence — the bug #54 fixes.)
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        init_git_repo(&repo);
        fs::write(
            repo.join("Tablet/a.tex"),
            "\\begin{theorem}changed\\end{theorem}\n",
        )
        .expect("dirty tracked tex");
        fs::write(repo.join("Tablet/temp.tex"), "temporary\n").expect("write untracked temp");
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Review));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::LastCommit,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);

        let outcome = runtime.step(&mut adapter).expect("review should succeed");
        // Kernel rejected the response as illegal → reissues Review.
        // No RestoreWorktree* command anywhere in the vec.
        assert!(matches!(
            outcome.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
        // Disk MUST be untouched — the worker's WIP is preserved.
        assert_eq!(
            fs::read_to_string(repo.join("Tablet/a.tex")).expect("read tex"),
            "\\begin{theorem}changed\\end{theorem}\n",
        );
        assert!(repo.join("Tablet/temp.tex").exists());
    }

    #[test]
    fn malformed_review_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Review));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::Review(ReviewResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                ..ReviewResponse::default()
            }),
            WrapperResponse::Review(ReviewResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                decision: ReviewDecisionKind::Continue,
                comments: String::new(),
                task_blockers: BTreeSet::new(),
                override_blockers: BTreeSet::new(),
                reset_blockers: BTreeSet::new(),
                next_active: Some("a".into()),
                reset: crate::model::ResetChoice::None,
                next_mode: TaskMode::Global,
                difficulty_updates: BTreeMap::new(),
                clear_human_input: false,
                ..ReviewResponse::default()
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed review should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued review request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued review should succeed");
        assert!(second
            .commands
            .iter()
            .any(|command| matches!(command, ProtocolCommand::CommitCheckpoint)));
        assert_eq!(runtime.state().stage, crate::model::Stage::Start);
    }

    #[test]
    fn malformed_paper_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::VerifyPaper;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Paper));
        let verifier_lanes = initial.verifier_lanes.clone();
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::Paper(PaperResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                ..PaperResponse::default()
            }),
            WrapperResponse::Paper(PaperResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                target_lane_updates: empty_corr_target_lanes(&verifier_lanes),
                node_lane_updates: BTreeMap::new(),
                reviewer_evidence: BTreeMap::new(),
                node_reviewer_evidence: BTreeMap::new(),
                ..PaperResponse::default()
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed paper should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Paper && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::VerifyPaper);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued paper request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued paper should succeed");
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert!(matches!(
            second.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn malformed_corr_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::VerifyCorr;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Corr));
        let verifier_lanes = initial.verifier_lanes.clone();
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::Corr(CorrResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                ..CorrResponse::default()
            }),
            WrapperResponse::Corr(CorrResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                node_lane_updates: empty_corr_node_lanes(&verifier_lanes),
                target_lane_updates: empty_corr_target_lanes(&verifier_lanes),
                reviewer_evidence: BTreeMap::new(),
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed corr should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Corr && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::VerifyCorr);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued corr request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued corr should succeed");
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert!(matches!(
            second.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn malformed_sound_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::VerifySound;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.held_target = Some("a".into());
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Sound));
        let verifier_lanes = initial.verifier_lanes.clone();
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::Sound(SoundResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                ..SoundResponse::default()
            }),
            WrapperResponse::Sound(SoundResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                lane_updates: empty_sound_lanes(&verifier_lanes),
                reviewer_evidence: BTreeMap::new(),
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed sound should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Sound && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::VerifySound);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued sound request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued sound should succeed");
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert!(matches!(
            second.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
    }

    #[test]
    fn malformed_human_gate_reissues_request_and_runtime_can_continue() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::HumanGate;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.gate_kind = GateKind::NeedInput;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::HumanGate));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::HumanGate(HumanGateResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                choice: HumanChoice::Approve,
            trust_actor_authentication_receipt: None,
            }),
            WrapperResponse::HumanGate(HumanGateResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                choice: HumanChoice::Approve,
            trust_actor_authentication_receipt: None,
            }),
        ]);

        let first = runtime
            .step(&mut adapter)
            .expect("malformed human gate should reissue");
        assert!(matches!(
            first.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::HumanGate && request.id == 2
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::HumanGate);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("reissued human gate request")
                .id,
            2
        );

        let second = runtime
            .step(&mut adapter)
            .expect("reissued human gate should succeed");
        assert!(matches!(
            second.commands.as_slice(),
            [ProtocolCommand::IssueRequest { request }] if request.kind == RequestKind::Review
        ));
        assert_eq!(runtime.state().stage, crate::model::Stage::Reviewer);
        assert_eq!(
            runtime
                .state()
                .in_flight_request
                .as_ref()
                .expect("review request after human gate")
                .kind,
            RequestKind::Review
        );
    }

    #[test]
    fn checkpoint_written_on_commit_command() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Review));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        let outcome = runtime.step(&mut adapter).unwrap();
        assert!(outcome
            .commands
            .iter()
            .any(|command| matches!(command, ProtocolCommand::CommitCheckpoint)));
        assert!(paths.checkpoint_path.exists());
        let checkpoint: RuntimeCheckpoint =
            serde_json::from_str(&fs::read_to_string(paths.checkpoint_path).unwrap()).unwrap();
        assert_eq!(checkpoint.cycle, 4);
        assert_eq!(checkpoint.phase, Phase::TheoremStating);
    }

    #[test]
    fn checkpoint_sink_called_on_commit_command() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Review));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        let mut sink = RecordingCheckpointSink::default();

        runtime
            .step_with_checkpoint_sink(&mut adapter, &mut sink)
            .unwrap();
        assert_eq!(sink.payloads.len(), 1);
        assert_eq!(sink.payloads[0].checkpoint.cycle, 4);
        assert_eq!(
            sink.payloads[0].commands,
            vec![ProtocolCommand::CommitCheckpoint]
        );
    }

    fn git_in(repo: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Worktree restores must never rewind the append-only event log:
    /// `reset --hard HEAD` used to revert the dirty current-cycle file,
    /// tearing a hole in the dense index.
    #[test]
    fn restore_worktree_to_head_shields_event_log() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".trellis-history/event-log")).unwrap();
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "committed").unwrap();
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000001.jsonl"),
            "{\"index\":0}\n",
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c1"]);
        // Dirty both: Tablet edit must be reverted, event-log tail must survive.
        std::fs::write(repo.join("Tablet/A.lean"), "dirty").unwrap();
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000001.jsonl"),
            "{\"index\":0}\n{\"index\":1}\n",
        )
        .unwrap();
        restore_worktree_to_head(&repo).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("Tablet/A.lean")).unwrap(),
            "committed"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join(".trellis-history/event-log/cycle-000001.jsonl"))
                .unwrap(),
            "{\"index\":0}\n{\"index\":1}\n",
            "event-log appends must survive a HEAD restore"
        );
        assert!(
            !repo
                .join(".trellis-history/event-log.restore-shield")
                .exists(),
            "shield must be moved back, not left behind"
        );
    }

    /// Regression for the live loss (designs run, 2026-07-04): entries
    /// materialized at audit acceptance are UNTRACKED until the next
    /// cycle-Start checkpoint commits them. In a rejection cycle the
    /// worker-retry restore (`RestoreWorktreeToActiveWorkerBase` /
    /// `RestoreWorktreeToHead` → `restore_worktree_to_head`) ran an
    /// unexcluded repo-root `git clean -fd`, deleting the entry files and
    /// INDEX.md while `process_memory_seq` kept its bumped value.
    #[test]
    fn restore_worktree_to_head_spares_untracked_process_memory() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "committed").unwrap();
        // A pre-existing COMMITTED entry + INDEX: the audit-acceptance
        // rewrite of the tracked INDEX.md is an uncommitted modification
        // that `reset --hard` reverts, so the restore must regenerate it.
        crate::process_memory::apply_file_ops(
            &repo,
            &[crate::process_memory::ProcessMemoryFileOp::Add {
                entry_id: "pm-0001-old".into(),
                entry_type: "constraint".into(),
                coarse_node: "global".into(),
                title: "old".into(),
                body: "Committed-era constraint.".into(),
                cycle: 4,
                request_id: 17,
            }],
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c1"]);
        // Materialize an audit-authored entry via the same code path the
        // runtime's ApplyProcessMemoryOperations handler uses. Nothing
        // commits it yet — exactly the acceptance-to-checkpoint window.
        crate::process_memory::apply_file_ops(
            &repo,
            &[crate::process_memory::ProcessMemoryFileOp::Add {
                entry_id: "pm-0003-route-y".into(),
                entry_type: "refuted-route".into(),
                coarse_node: "ConeB".into(),
                title: "t".into(),
                body: "Route Y refuted; see cycle 5 audit.".into(),
                cycle: 5,
                request_id: 21,
            }],
        )
        .unwrap();
        // A rejected burst's stray mutations must still be swept — including
        // a stray NESTED process-memory dir (the exclusion is anchored to
        // the repo root).
        std::fs::write(repo.join("Tablet/A.lean"), "dirty").unwrap();
        std::fs::write(repo.join("Tablet/Stray.lean"), "junk").unwrap();
        std::fs::create_dir_all(repo.join("Tablet/process-memory")).unwrap();
        std::fs::write(repo.join("Tablet/process-memory/forged.md"), "x").unwrap();

        restore_worktree_to_head(&repo).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("Tablet/A.lean")).unwrap(),
            "committed",
            "tracked files must be restored to HEAD"
        );
        assert!(
            !repo.join("Tablet/Stray.lean").exists(),
            "untracked non-memory files must still be cleaned"
        );
        let entry = repo.join("process-memory/ConeB/pm-0003-route-y.md");
        assert!(
            entry.is_file(),
            "not-yet-checkpointed process-memory entries must survive the restore"
        );
        assert!(
            !repo.join("Tablet/process-memory").exists(),
            "nested stray process-memory dirs must still be swept (anchored exclusion)"
        );
        let index = std::fs::read_to_string(repo.join("process-memory/INDEX.md")).unwrap();
        assert!(
            index.contains("pm-0003-route-y") && index.contains("pm-0001-old"),
            "INDEX.md must be regenerated after the restore to list both the \
             committed entry and the untracked survivor (reset --hard reverts \
             the tracked INDEX to its committed content): {index}"
        );

        // A checkpoint-style commit (`git add -A`, see commit_checkpoint)
        // must pick the survivors up.
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "checkpoint"]);
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["ls-files", "process-memory"])
            .output()
            .unwrap();
        let tracked = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(tracked.contains("process-memory/ConeB/pm-0003-route-y.md"));
        assert!(tracked.contains("process-memory/INDEX.md"));
    }

    /// LastClean's `reset --hard <clean-tag>` + `clean -fd` must spare the
    /// event log: post-clean cycle files would otherwise be deleted outright.
    #[test]
    fn restore_last_clean_shields_event_log() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join(".trellis-history/event-log")).unwrap();
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "clean").unwrap();
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000001.jsonl"),
            "{\"index\":0}\n",
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "clean point"]);
        git_in(&repo, &["tag", "supervisor2/clean-000001"]);
        // Advance past the clean point: new committed cycle file + edits.
        std::fs::write(repo.join("Tablet/A.lean"), "post-clean").unwrap();
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000002.jsonl"),
            "{\"index\":1}\n",
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "post clean"]);
        std::fs::write(
            repo.join(".trellis-history/event-log/cycle-000002.jsonl"),
            "{\"index\":1}\n{\"index\":2}\n",
        )
        .unwrap();

        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        runtime.restore_repo_worktree_to_last_clean(&repo, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.join("Tablet/A.lean")).unwrap(),
            "clean",
            "non-event-log files must be at the clean tag"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join(".trellis-history/event-log/cycle-000002.jsonl"))
                .unwrap(),
            "{\"index\":1}\n{\"index\":2}\n",
            "post-clean cycle files and dirty appends must survive LastClean"
        );
        assert!(!repo
            .join(".trellis-history/event-log.restore-shield")
            .exists());
    }

    /// Process memory (spec §7): shared harness — a clean-tagged commit
    /// WITHOUT `process-memory/`, then a later commit that adds an entry.
    fn build_process_memory_rewind_repo(dir: &tempfile::TempDir) -> (SupervisorRuntime, PathBuf) {
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "clean").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "clean point"]);
        git_in(&repo, &["tag", "supervisor2/clean-000001"]);
        // Advance: an audit materialized a process-memory entry.
        std::fs::write(repo.join("Tablet/A.lean"), "post-clean").unwrap();
        crate::process_memory::apply_file_ops(
            &repo,
            &[crate::process_memory::ProcessMemoryFileOp::Add {
                entry_id: "pm-0001-route-x".into(),
                entry_type: "refuted-route".into(),
                coarse_node: "ConeA".into(),
                title: "t".into(),
                body: "Route X refuted; counterexample inline.".into(),
                cycle: 3,
                request_id: 9,
            }],
        )
        .unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "post clean with memory"]);
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        (runtime, repo)
    }

    /// Process memory (spec §7): `preserve_process_memory=true` restores
    /// `process-memory/` from the pre-rewind HEAD after a LastClean reset.
    #[test]
    fn restore_last_clean_carries_process_memory_forward_when_preserving() {
        let dir = local_tempdir();
        let (runtime, repo) = build_process_memory_rewind_repo(&dir);
        runtime
            .restore_repo_worktree_to_last_clean(&repo, true)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("Tablet/A.lean")).unwrap(),
            "clean",
            "tracked files must be at the clean tag"
        );
        let entry = repo.join("process-memory/ConeA/pm-0001-route-x.md");
        assert!(
            entry.is_file(),
            "process-memory entry must be carried forward across LastClean"
        );
        assert!(repo.join("process-memory/INDEX.md").is_file());
    }

    /// Process memory (spec §7): `preserve_process_memory=false` (the
    /// poisoned-memory case) keeps existing behavior — the reset reverts
    /// memory with every other tracked file.
    #[test]
    fn restore_last_clean_drops_process_memory_when_not_preserving() {
        let dir = local_tempdir();
        let (runtime, repo) = build_process_memory_rewind_repo(&dir);
        runtime
            .restore_repo_worktree_to_last_clean(&repo, false)
            .unwrap();
        assert!(
            !repo.join("process-memory").exists(),
            "poisoned-memory rewind must revert process-memory/ to the clean tag"
        );
    }

    /// Extend the shared harness with a SECOND entry that is still
    /// untracked (materialized after the last checkpoint commit).
    fn add_untracked_entry(repo: &Path) {
        crate::process_memory::apply_file_ops(
            repo,
            &[crate::process_memory::ProcessMemoryFileOp::Add {
                entry_id: "pm-0002-route-y".into(),
                entry_type: "constraint".into(),
                coarse_node: "ConeA".into(),
                title: "t".into(),
                body: "Bound must stay below n/3.".into(),
                cycle: 4,
                request_id: 11,
            }],
        )
        .unwrap();
    }

    /// Process memory (spec §7): with `preserve_process_memory=true`, a
    /// not-yet-checkpointed (untracked) entry survives the LastClean
    /// rewind alongside the committed carry-forward, and INDEX.md is
    /// regenerated to list the union.
    #[test]
    fn restore_last_clean_keeps_untracked_process_memory_when_preserving() {
        let dir = local_tempdir();
        let (runtime, repo) = build_process_memory_rewind_repo(&dir);
        add_untracked_entry(&repo);
        runtime
            .restore_repo_worktree_to_last_clean(&repo, true)
            .unwrap();
        assert!(
            repo.join("process-memory/ConeA/pm-0001-route-x.md").is_file(),
            "committed entry must be carried forward"
        );
        assert!(
            repo.join("process-memory/ConeA/pm-0002-route-y.md").is_file(),
            "untracked entry must survive the LastClean clean sweep"
        );
        let index =
            std::fs::read_to_string(repo.join("process-memory/INDEX.md")).unwrap();
        assert!(index.contains("pm-0001-route-x]"));
        assert!(
            index.contains("pm-0002-route-y]"),
            "INDEX.md must be regenerated over the carried-forward + untracked union"
        );
    }

    /// Process memory (spec §7): with `preserve_process_memory=false`
    /// (poisoned memory) the rewind reverts memory to the clean tag —
    /// tracked entries via the reset, untracked ones removed by the sweep.
    #[test]
    fn restore_last_clean_removes_untracked_process_memory_when_not_preserving() {
        let dir = local_tempdir();
        let (runtime, repo) = build_process_memory_rewind_repo(&dir);
        add_untracked_entry(&repo);
        runtime
            .restore_repo_worktree_to_last_clean(&repo, false)
            .unwrap();
        assert!(
            !repo.join("process-memory").exists(),
            "poisoned-memory rewind must also remove untracked process-memory files"
        );
    }

    /// Bug 2 selection harness: build a LINEAR three-commit repo where the
    /// clean-tag suffixes are NON-MONOTONIC in real run time (a stale
    /// pre-segmentation `clean-003334` on an OLDER commit, a live
    /// post-segmentation `clean-001799` on a NEWER commit). Returns the
    /// runtime + repo path + the live (newer) commit SHA.
    fn build_nonmonotonic_clean_repo(
        dir: &tempfile::TempDir,
    ) -> (SupervisorRuntime, PathBuf, String) {
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        // c1: STALE clean checkpoint, high event_count suffix (lexical max).
        std::fs::write(repo.join("Tablet/A.lean"), "v1").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c1 stale clean"]);
        git_in(&repo, &["tag", "supervisor2/clean-003334"]);
        // c2: LIVE clean checkpoint, lower event_count suffix (post-segmentation).
        std::fs::write(repo.join("Tablet/A.lean"), "v2").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c2 live clean"]);
        git_in(&repo, &["tag", "supervisor2/clean-001799"]);
        let live_sha = {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        // c3: current HEAD (dirty work advanced past the live clean point).
        std::fs::write(repo.join("Tablet/A.lean"), "v3").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c3 head"]);
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        (runtime, repo, live_sha)
    }

    /// Bug 2 root-cause #1/#3: with no commit pointer in state, the
    /// rewind target must be the NEAREST HEAD-ancestor clean tag (the
    /// live `clean-001799`), NOT the lexically-highest stale
    /// `clean-003334`. The old `--sort=-refname` + `.first()` picked the
    /// stale tag — a 226-cycle catastrophic rollback.
    #[test]
    fn last_clean_target_picks_nearest_ancestor_not_lexical_max() {
        let dir = local_tempdir();
        let (runtime, repo, _live_sha) = build_nonmonotonic_clean_repo(&dir);
        assert!(runtime.state.last_clean_commit.is_none());
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        // git --sort=-refname yields the stale tag first (the old bug).
        assert_eq!(
            tags.first().map(String::as_str),
            Some("supervisor2/clean-003334")
        );
        let target = runtime.resolve_last_clean_commitish(&repo, &tags).unwrap();
        assert_eq!(
            target, "supervisor2/clean-001799",
            "must pick the nearest HEAD-ancestor clean tag, not the lexical-max stale tag"
        );
    }

    /// Bug 2 fix #1: a recorded `last_clean_commit` pointer that is an
    /// ancestor of HEAD is used verbatim, overriding tag selection.
    #[test]
    fn last_clean_target_prefers_commit_pointer() {
        let dir = local_tempdir();
        let (mut runtime, repo, live_sha) = build_nonmonotonic_clean_repo(&dir);
        runtime.state.last_clean_commit = Some(live_sha.clone());
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        let target = runtime.resolve_last_clean_commitish(&repo, &tags).unwrap();
        assert_eq!(target, live_sha, "commit pointer (HEAD ancestor) must win");
    }

    /// Round-2 fold-in: when the recorded `last_clean_commit` pointer is a
    /// HEAD ancestor but STALE relative to a newer clean tag (e.g. a later
    /// `rev-parse HEAD` failed so the pointer lagged), selection must take the
    /// MORE-RECENT (nearer-HEAD) target — the newer tag — not the stale
    /// pointer. This is the "prefer more-recent of {pointer, tag}" change over
    /// the round-1 "unconditionally prefer the pointer" behaviour.
    #[test]
    fn last_clean_target_prefers_more_recent_tag_over_stale_pointer() {
        let dir = local_tempdir();
        let (mut runtime, repo, _live_sha) = build_nonmonotonic_clean_repo(&dir);
        // Stale pointer = the OLDER clean checkpoint c1 (HEAD-ancestor, but two
        // commits behind); the newer clean tag c2 is one commit behind.
        let stale_sha = {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "supervisor2/clean-003334"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        runtime.state.last_clean_commit = Some(stale_sha);
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        let target = runtime.resolve_last_clean_commitish(&repo, &tags).unwrap();
        assert_eq!(
            target, "supervisor2/clean-001799",
            "must prefer the more-recent HEAD-ancestor target over the stale commit pointer"
        );
    }

    /// Bug 2 fix: a stale `last_clean_commit` that is NOT an ancestor of
    /// HEAD is ignored; selection falls back to the nearest ancestor tag.
    #[test]
    fn last_clean_target_ignores_non_ancestor_commit_pointer() {
        let dir = local_tempdir();
        let (mut runtime, repo, _live_sha) = build_nonmonotonic_clean_repo(&dir);
        runtime.state.last_clean_commit = Some("0000000000000000000000000000000000000000".into());
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        let target = runtime.resolve_last_clean_commitish(&repo, &tags).unwrap();
        assert_eq!(target, "supervisor2/clean-001799");
    }

    /// Bug 2 fix #2: when the only clean tag is on a SIBLING line (not an
    /// ancestor of HEAD) and there is no commit pointer, refuse to rewind
    /// (fail loud) rather than reset to a non-ancestor stale tag.
    #[test]
    fn last_clean_target_fails_loud_when_no_ancestor_tag() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "base").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "base"]);
        // Sibling branch carries the (stale) clean tag, NOT reachable from main.
        git_in(&repo, &["checkout", "-b", "sibling"]);
        std::fs::write(repo.join("Tablet/A.lean"), "sibling").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "sibling clean"]);
        git_in(&repo, &["tag", "supervisor2/clean-009999"]);
        git_in(&repo, &["checkout", "main"]);
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        let tags = SupervisorRuntime::list_supervisor_clean_tags(&repo).unwrap();
        assert_eq!(tags, vec!["supervisor2/clean-009999".to_string()]);
        let err = runtime
            .resolve_last_clean_commitish(&repo, &tags)
            .unwrap_err();
        match err {
            RuntimeError::InvalidRuntimeState(msg) => {
                assert!(msg.contains("no safe rewind target"), "got: {msg}");
            }
            other => panic!("expected InvalidRuntimeState, got {other:?}"),
        }
    }

    /// Operator directive (a): a STATE-INTEGRITY fault (the loaded
    /// `last_clean_*` mirrors claim readiness but git has zero clean tags)
    /// must FAIL LOUD at load — it must NOT silently rewind. This is the
    /// "state-fault path does NOT pick LastClean" guard.
    #[test]
    fn state_integrity_fault_fails_loud_does_not_rewind() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        git_in(&repo, &["init", "--initial-branch=main"]);
        std::fs::write(repo.join("Tablet/A.lean"), "x").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-m", "c1"]);
        // No supervisor2/clean-* tag exists, but state claims mirror readiness.
        let mut state = base_state();
        state.last_clean_verifier_mirror_ready = true;
        state.has_ever_been_clean = true;
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        let err = runtime.validate_last_clean_tag_consistency().unwrap_err();
        match err {
            RuntimeError::InvalidRuntimeState(msg) => {
                assert!(
                    msg.contains("zero `supervisor2/clean-*` tags"),
                    "got: {msg}"
                );
            }
            other => panic!("expected fail-loud InvalidRuntimeState, got {other:?}"),
        }
    }

    #[test]
    fn checkpoint_sink_failure_is_reported() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Review));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        let mut sink = RecordingCheckpointSink {
            payloads: Vec::new(),
            fail_with: Some("hook failed".into()),
        };

        let error = runtime
            .step_with_checkpoint_sink(&mut adapter, &mut sink)
            .expect_err("sink failure should bubble");
        assert!(matches!(error, RuntimeError::CheckpointSink(message) if message == "hook failed"));
    }

    #[test]
    fn load_rejects_state_claiming_clean_mirror_ready_when_git_has_no_clean_tag() {
        // Atomicity (audit, Option C): a state file with
        // last_clean_verifier_mirror_ready=true must be backed by at
        // least one supervisor2/clean-* tag in git. Otherwise a
        // future LastClean reset has nothing to rewind to. Fail at
        // load with an actionable error.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        init_git_repo(&repo);
        // No supervisor2/clean-* tag is created by init_git_repo —
        // the synthetic seed only produces a single root commit.
        let mut initial = base_state();
        // Simulate a state file that thinks a clean checkpoint exists.
        initial.last_clean_verifier_mirror_ready = true;
        initial.has_ever_been_clean = true;
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize_with_metadata should succeed (no validation there)");

        // load() runs the validator and should refuse.
        let result = SupervisorRuntime::load(paths);
        let err = match result {
            Ok(_) => panic!("load should refuse when state expects a clean tag git lacks"),
            Err(e) => e,
        };
        let RuntimeError::InvalidRuntimeState(msg) = err else {
            panic!("expected InvalidRuntimeState; got {err:?}");
        };
        assert!(msg.contains("supervisor2/clean-"), "msg={msg}");
        assert!(msg.contains("zero"), "msg={msg}");
    }

    #[test]
    fn load_accepts_state_with_clean_mirror_ready_when_git_has_clean_tag() {
        // Sanity counterpart: when git DOES have a clean tag, load
        // accepts the state. (Without this counterpart, the validator
        // could have a bug that always rejects.)
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        init_git_repo(&repo);
        // Create a fake clean tag.
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["tag", "supervisor2/clean-000001", "HEAD"])
            .output()
            .expect("git tag");
        let mut initial = base_state();
        initial.last_clean_verifier_mirror_ready = true;
        initial.has_ever_been_clean = true;
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        let _runtime = SupervisorRuntime::load(paths).expect("load should accept");
    }

    #[test]
    fn load_rejects_pre_cutover_state_with_sound_lane_evidence() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let mut initial = base_state();
        initial.sound_assessment_schema_version = 0;
        SupervisorRuntime::initialize(paths.clone(), initial)
            .expect("initial persisted state can predate cutover");

        let err = match SupervisorRuntime::load(paths) {
            Ok(_) => panic!("legacy Sound evidence must reject"),
            Err(err) => err,
        };
        let RuntimeError::InvalidRuntimeState(msg) = err else {
            panic!("expected InvalidRuntimeState; got {err:?}");
        };
        assert!(
            msg.contains("soundness assessment schema cutover"),
            "msg={msg}"
        );
        assert!(msg.contains("Rewind"), "msg={msg}");
        assert!(msg.contains("Soundness lanes"), "msg={msg}");
    }

    #[test]
    fn load_stamps_pre_cutover_state_without_sound_lane_evidence() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let mut initial = ProtocolState::default();
        initial.sound_assessment_schema_version = 0;
        SupervisorRuntime::initialize(paths.clone(), initial)
            .expect("initial persisted state can predate cutover");

        let runtime = SupervisorRuntime::load(paths.clone())
            .expect("pre-cutover state with no Sound evidence should load");
        assert_eq!(
            runtime.state().sound_assessment_schema_version,
            SOUND_ASSESSMENT_SCHEMA_VERSION
        );
        let persisted: ProtocolState =
            serde_json::from_str(&fs::read_to_string(paths.state_path).expect("read state"))
                .expect("parse persisted state");
        assert_eq!(
            persisted.sound_assessment_schema_version,
            SOUND_ASSESSMENT_SCHEMA_VERSION
        );
    }

    #[test]
    fn checkpoint_sink_failure_rolls_back_in_memory_state_and_state_file() {
        // Atomicity (audit): checkpoint sink failure must not advance
        // either in-memory state OR the persisted state file. Otherwise
        // a subsequent process start (with state file ahead of git)
        // would see LastCommit pointing at an OLD commit and LastClean
        // pointing at a clean tag that the sink never created, with
        // `last_clean_*` mirrors describing a state git doesn't hold.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Review));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        // Snapshot pre-step in-memory state and the on-disk state file.
        let pre_step_state = runtime.state().clone();
        let pre_step_state_file = fs::read_to_string(&paths.state_path)
            .expect("state file should exist after initialize");

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        let mut sink = RecordingCheckpointSink {
            payloads: Vec::new(),
            fail_with: Some("hook failed for atomicity test".into()),
        };

        let error = runtime
            .step_with_checkpoint_sink(&mut adapter, &mut sink)
            .expect_err("sink failure should bubble");
        assert!(matches!(error, RuntimeError::CheckpointSink(_)));

        // In-memory state restored to pre-step.
        assert_eq!(
            runtime.state(),
            &pre_step_state,
            "in-memory state must roll back to pre-step on sink failure",
        );
        // metadata.native_history_kinds also restored to pre-step.
        // record_native_history may have inserted (Review, phase) before
        // the sink ran; the rollback restores metadata to its pre-step
        // shape. Without this assertion, a regression that drops the
        // self.metadata = pre_step_metadata line wouldn't be caught.
        assert!(
            runtime.metadata.native_history_kinds.is_empty(),
            "metadata.native_history_kinds must roll back to pre-step \
             (was empty); got {:?}",
            runtime.metadata.native_history_kinds,
        );
        // State file untouched (the new persist_state runs AFTER sink success).
        let post_step_state_file =
            fs::read_to_string(&paths.state_path).expect("state file still readable");
        assert_eq!(
            post_step_state_file, pre_step_state_file,
            "state file must not be advanced when checkpoint sink fails",
        );
    }

    #[test]
    fn load_validator_soft_no_ops_when_git_invocation_fails() {
        // Audit follow-up regression: the validator must NOT reject when
        // git is unavailable (binary missing, repo path can't be opened
        // by git, etc.). Prior to this fix, the helper collapsed
        // git-unavailable into "empty Vec" and the validator treated
        // that as "zero clean tags exist" → spurious rejection on
        // hosts/repos where git can't run.
        //
        // Use a path that's GUARANTEED not to exist as a directory.
        // git -C <nonexistent> exits with "fatal: cannot change to
        // '...': No such file or directory" (status 128) BEFORE any
        // ancestor .git discovery walks the filesystem. This avoids
        // the fragility of relying on /tmp being a separate filesystem
        // mount — on hosts where /tmp shares a filesystem with a
        // parent .git, git -C /tmp/<name> would succeed by walking up.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let nonexistent_repo = dir
            .path()
            .join("definitely-does-not-exist")
            .join("nor-does-this");
        assert!(
            !nonexistent_repo.exists(),
            "test precondition: repo path must not exist on disk so \
             git -C errors with 'cannot change to dir' before any \
             ancestor .git discovery",
        );
        let mut initial = base_state();
        initial.last_clean_verifier_mirror_ready = true;
        initial.has_ever_been_clean = true;
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(nonexistent_repo),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();
        SupervisorRuntime::load(paths).expect(
            "load must succeed when git is unavailable (helper returns Err) — \
             the validator soft-no-ops, not blame the state file",
        );
    }

    #[test]
    fn checkpoint_persist_failure_also_rolls_back_state_and_metadata() {
        // Audit follow-up: the prior rollback test only exercised the
        // sink.commit failure path. The persist_checkpoint failure
        // path's `self.state = pre_step_state; self.metadata =
        // pre_step_metadata;` lines were uncovered. Inject a failure
        // by pointing checkpoint_path at a path inside a nonexistent
        // directory — fs::write returns Err(NotFound) because the
        // parent doesn't exist.
        let dir = local_tempdir();
        let mut paths = RuntimePaths::new(dir.path());
        paths.checkpoint_path = dir
            .path()
            .join("nonexistent-parent-dir")
            .join("checkpoint.json");
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Reviewer;
        initial.cycle = 4;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Review));
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let pre_step_state = runtime.state().clone();
        let pre_step_state_file = fs::read_to_string(&paths.state_path).unwrap();

        let mut adapter = QueueAdapter::new(vec![WrapperResponse::Review(ReviewResponse {
            request_id: 1,
            cycle: 4,
            status: ResponseStatus::Ok,
            decision: ReviewDecisionKind::Continue,
            comments: String::new(),
            task_blockers: BTreeSet::new(),
            override_blockers: BTreeSet::new(),
            reset_blockers: BTreeSet::new(),
            next_active: Some("a".into()),
            reset: crate::model::ResetChoice::None,
            next_mode: TaskMode::Global,
            difficulty_updates: BTreeMap::new(),
            clear_human_input: false,
            ..ReviewResponse::default()
        })]);
        // NoopCheckpointSink — the failure must come from
        // persist_checkpoint, not from the sink.
        let mut sink = NoopCheckpointSink;

        let error = runtime
            .step_with_checkpoint_sink(&mut adapter, &mut sink)
            .expect_err("persist_checkpoint failure should bubble");
        assert!(
            matches!(error, RuntimeError::Io(_)),
            "expected Io error from fs::write to nonexistent parent; got {error:?}",
        );

        // Both state and metadata rolled back from this distinct
        // failure path (separate from the sink-commit failure path).
        assert_eq!(
            runtime.state(),
            &pre_step_state,
            "state must roll back on persist_checkpoint failure",
        );
        assert!(
            runtime.metadata.native_history_kinds.is_empty(),
            "metadata.native_history_kinds must roll back on persist_checkpoint \
             failure (was empty); got {:?}",
            runtime.metadata.native_history_kinds,
        );
        assert_eq!(
            fs::read_to_string(&paths.state_path).unwrap(),
            pre_step_state_file,
            "state file must not advance on persist_checkpoint failure",
        );
    }

    /// Build a `.trellis-history/supervisor_state.json` payload mirroring
    /// the supervisor's git checkpoint hook output. Only the
    /// `state.coarse_dag_nodes` field is consumed by the heal, but we mirror
    /// the surrounding shape so a future change to the recovery logic
    /// (e.g., reading metadata too) doesn't quietly break.
    fn write_history_state(repo: &Path, coarse_dag_nodes: &[&str]) {
        let history_dir = repo.join(".trellis-history");
        fs::create_dir_all(&history_dir).expect("create .trellis-history dir");
        let payload = serde_json::json!({
            "event_count": 0,
            "metadata": {},
            "checkpoint": {},
            "state": {
                "phase": "ProofFormalization",
                "coarse_dag_nodes": coarse_dag_nodes,
            },
            "commands": [],
        });
        fs::write(
            history_dir.join("supervisor_state.json"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .expect("write history supervisor_state.json");
    }

    fn git_commit_all(repo: &Path, message: &str) {
        let add = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["add", "-A"])
            .status()
            .expect("git add");
        assert!(add.success(), "git add failed");
        let commit = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["commit", "-m", message])
            .status()
            .expect("git commit");
        assert!(commit.success(), "git commit failed");
    }

    #[test]
    fn load_recovers_coarse_dag_from_git_history_when_state_field_is_empty() {
        // Mirrors the production failure mode: a manual rewind landed us in
        // ProofFormalization with empty coarse_dag_nodes, but a prior
        // checkpoint commit in git history still has the authentic value.
        // SupervisorRuntime::load must transparently recover.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        init_git_repo(&repo);

        // Commit 2: history captures coarse_dag_nodes populated.
        write_history_state(&repo, &["Preamble", "MainProof", "DepLemma"]);
        git_commit_all(&repo, "supervisor2 checkpoint with populated coarse_dag");

        // Commit 3: a later checkpoint that LOST the field (mirrors the
        // post-rewind state). The heal must still pick up the populated
        // value from commit 2.
        write_history_state(&repo, &[]);
        git_commit_all(&repo, "supervisor2 checkpoint after rewind (empty)");

        // Initialize runtime with empty coarse_dag_nodes in protocol_state
        // and phase=ProofFormalization (heal precondition).
        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.coarse_dag_nodes.clear();
        let metadata = RuntimeMetadata {
            repo_path: Some(repo.clone()),
            config_path: Some(repo.join("trellis.config.json")),
            native_history_kinds: BTreeSet::new(),
            initial_planning_seeded: false,
        ..RuntimeMetadata::default()
        };
        let runtime = SupervisorRuntime::initialize_with_metadata(paths.clone(), state, metadata)
            .expect("initialize runtime");
        // initialize doesn't run the heal; load does.
        drop(runtime);

        let healed = SupervisorRuntime::load(paths).expect("load runtime");
        assert_eq!(
            healed.state.coarse_dag_nodes,
            BTreeSet::from([
                NodeId::from("Preamble"),
                NodeId::from("MainProof"),
                NodeId::from("DepLemma"),
            ]),
            "expected git heal to recover the populated coarse_dag_nodes from history",
        );
    }

    #[test]
    fn load_does_not_overwrite_already_populated_coarse_dag() {
        // Heal must be a no-op if the loaded state already has a value —
        // even if git history disagrees. The on-disk state is authoritative
        // when present.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        init_git_repo(&repo);
        write_history_state(&repo, &["DifferentNode"]);
        git_commit_all(&repo, "history with different coarse_dag");

        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.coarse_dag_nodes =
            BTreeSet::from([NodeId::from("OnDiskNode1"), NodeId::from("OnDiskNode2")]);
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(repo.join("trellis.config.json")),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        drop(runtime);

        let loaded = SupervisorRuntime::load(paths).expect("load runtime");
        assert_eq!(
            loaded.state.coarse_dag_nodes,
            BTreeSet::from([NodeId::from("OnDiskNode1"), NodeId::from("OnDiskNode2")]),
            "heal must not touch an already-populated coarse_dag_nodes",
        );
    }

    #[test]
    fn step_re_heals_coarse_dag_if_field_clears_mid_run() {
        // Defensive: if anything clears coarse_dag_nodes after load (a
        // future state-mutation path, a manual edit between steps), the
        // step boundary heal must recover it without needing a restart.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        init_git_repo(&repo);
        write_history_state(&repo, &["Preamble", "MainProof"]);
        git_commit_all(&repo, "supervisor2 checkpoint with populated coarse_dag");

        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        // Populate so load() leaves it alone.
        state.coarse_dag_nodes =
            BTreeSet::from([NodeId::from("Preamble"), NodeId::from("MainProof")]);
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(repo.join("trellis.config.json")),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        // Simulate the field being cleared mid-run (the failure mode this
        // hook exists to defend against).
        runtime.state.coarse_dag_nodes.clear();

        // step() must transparently re-heal before doing anything else.
        let mut adapter = QueueAdapter::new(vec![]);
        let _ = runtime.step(&mut adapter);
        assert_eq!(
            runtime.state.coarse_dag_nodes,
            BTreeSet::from([NodeId::from("Preamble"), NodeId::from("MainProof")]),
            "step boundary must re-heal coarse_dag_nodes if it gets cleared mid-run",
        );
    }

    #[test]
    fn load_heal_is_noop_when_no_git_history_available() {
        // Repo isn't a git repo (or has no checkpoint history). Heal must
        // fail soft — the field stays empty and the legacy
        // "treat all as coarse" fallback in runtime_cli_observations.rs
        // takes over.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        write_test_config(&repo);
        // NB: deliberately do NOT init_git_repo — git invocations will fail.

        let mut state = base_state();
        state.phase = Phase::ProofFormalization;
        state.coarse_dag_nodes.clear();
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(repo.join("trellis.config.json")),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        drop(runtime);

        let loaded = SupervisorRuntime::load(paths).expect("load runtime");
        assert!(
            loaded.state.coarse_dag_nodes.is_empty(),
            "no git history → heal must be a no-op, not crash and not populate from anywhere",
        );
    }

    #[test]
    fn event_log_appends_one_record_per_step() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![]);
        runtime.step(&mut adapter).unwrap();
        // The single step's start_cycle record lands in the NEW cycle's
        // per-cycle file (cycle 1), and the global event_count reconciles.
        let event_log_dir = runtime.event_log_dir();
        let cycle_file = event_log_cycle_file(&event_log_dir, runtime.state().cycle);
        let lines = fs::read_to_string(&cycle_file).unwrap();
        assert_eq!(lines.lines().count(), 1);
        assert_eq!(read_event_count(&event_log_dir).unwrap(), 1);
    }

    #[test]
    fn append_event_log_keys_files_on_cycle_and_reconciles_count() {
        // Direct writer/loader unit test (no engine drive): append records
        // spanning two cycles, assert per-cycle membership, dense in-order
        // concatenation, and that read_event_count sums across files.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        let mut runtime = SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            base_state(),
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .unwrap();

        // Two records in cycle 1, one in cycle 2. `append_event_log` keys on
        // `state.cycle`, so set it before each append.
        runtime.state.cycle = 1;
        runtime
            .append_event_log(&ProtocolEvent::StartCycle, &[])
            .unwrap();
        runtime
            .append_event_log(&ProtocolEvent::StartCycle, &[])
            .unwrap();
        runtime.state.cycle = 2;
        runtime
            .append_event_log(&ProtocolEvent::StartCycle, &[])
            .unwrap();

        let event_log_dir = runtime.event_log_dir();
        let files = event_log_cycle_files(&event_log_dir).unwrap();
        assert_eq!(files.len(), 2, "one file per cycle");
        let c1 = fs::read_to_string(event_log_cycle_file(&event_log_dir, 1)).unwrap();
        let c2 = fs::read_to_string(event_log_cycle_file(&event_log_dir, 2)).unwrap();
        assert_eq!(c1.lines().count(), 2);
        assert_eq!(c2.lines().count(), 1);

        // Dense, in-order concatenation: indices 0,1,2 across the sorted files.
        let mut indices: Vec<u64> = Vec::new();
        for path in &files {
            for line in fs::read_to_string(path).unwrap().lines() {
                let record: EventLogRecord = serde_json::from_str(line).unwrap();
                indices.push(record.index);
            }
        }
        assert_eq!(indices, vec![0, 1, 2]);
        assert_eq!(read_event_count(&event_log_dir).unwrap(), 3);
    }

    #[test]
    fn read_event_count_fails_loud_on_index_gap() {
        // A gap in the global index (missing index 1) must fail loud rather
        // than silently returning a count that disagrees with max_index+1.
        let dir = local_tempdir();
        let event_log_dir = dir.path().join("event-log");
        fs::create_dir_all(&event_log_dir).unwrap();
        let mk = |index: u64, cycle: u32| {
            let record = EventLogRecord {
                index,
                event: ProtocolEvent::StartCycle,
                commands: vec![],
                phase: Phase::TheoremStating,
                stage: crate::model::Stage::Start,
                cycle,
                ts_ms: 0,
            };
            format!("{}\n", serde_json::to_string(&record).unwrap())
        };
        // indices 0 and 2 present, 1 missing → sum=2 but max_index=2.
        fs::write(
            event_log_cycle_file(&event_log_dir, 1),
            format!("{}{}", mk(0, 1), mk(2, 1)),
        )
        .unwrap();
        let err = read_event_count(&event_log_dir).unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidRuntimeState(_)),
            "index gap must surface as InvalidRuntimeState, got {err:?}"
        );
    }

    #[test]
    fn read_event_count_is_zero_for_absent_dir() {
        let dir = local_tempdir();
        let absent = dir.path().join("nope");
        assert_eq!(read_event_count(&absent).unwrap(), 0);
    }

    #[test]
    fn load_rejects_non_initial_state_with_absent_event_log() {
        // Segmentation misorder guard: a non-initial state (cycle >= 1)
        // with an absent/empty event-log dir means the binary was
        // launched before `segment_event_log` ran; appending would
        // restart the dense index at 0. Must fail loud, not cold-start.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let mut initial = base_state();
        initial.cycle = 490;
        SupervisorRuntime::initialize(paths.clone(), initial).unwrap();

        let err = match SupervisorRuntime::load(paths) {
            Ok(_) => panic!("absent event log with non-initial state must reject"),
            Err(err) => err,
        };
        let RuntimeError::InvalidRuntimeState(msg) = err else {
            panic!("expected InvalidRuntimeState; got {err:?}");
        };
        assert!(msg.contains("segment_event_log"), "msg={msg}");
    }

    fn seed_runtime_decide_pair(state: &mut ProtocolState, disprove_live: bool) {
        let primary = crate::model::ChallengeTargetId::from("correct");
        state.configured_challenge_targets.insert(
            primary.clone(),
            crate::model::ChallengeTargetSpec {
                name: "Correct".into(),
                resolution: crate::model::ChallengeResolution::Decide,
                ..crate::model::ChallengeTargetSpec::default()
            },
        );
        state.configured_challenge_targets.insert(
            crate::model::refutation_target_id(&primary),
            crate::model::ChallengeTargetSpec {
                name: "Correct__Refutation".into(),
                ..crate::model::ChallengeTargetSpec::default()
            },
        );
        if disprove_live {
            state
                .pv_live_polarity
                .insert(primary, crate::model::ChallengePolarity::Disprove);
        }
    }

    fn worker_inflight_runtime(
        runtime_root: &Path,
        repo: &Path,
        mut state: ProtocolState,
    ) -> SupervisorRuntime {
        state.stage = crate::model::Stage::Worker;
        // Cycle zero keeps these direct-runtime fixtures independent of the
        // event-log segmentation guard when a test exercises crash reload.
        state.cycle = 0;
        state.request_seq = 1;
        state.in_flight_request = Some(state.expected_request(1, RequestKind::Worker));
        let config_path = repo.join("trellis.config.json");
        SupervisorRuntime::initialize_with_metadata(
            RuntimePaths::new(runtime_root),
            state,
            RuntimeMetadata {
                repo_path: Some(repo.to_path_buf()),
                config_path: config_path.is_file().then_some(config_path),
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize worker runtime")
    }

    #[test]
    fn decide_layout_allows_config_only_load_but_rejects_partial_materialization() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        let mut state = base_state();
        seed_runtime_decide_pair(&mut state, false);
        let paths = RuntimePaths::new(dir.path().join("runtime"));

        // Definition seeding may precede source-file materialization during a
        // single fresh-init operation, so initialization accepts this
        // intermediate state.
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo),
                ..RuntimeMetadata::default()
            },
        )
        .expect("fresh init permits the pre-materialization intermediate");

        // Config-only state remains loadable so a worker can author the live
        // primary. No state snapshot claims either side and all eight pair
        // paths are absent.
        let runtime = SupervisorRuntime::load(paths.clone())
            .expect("wholly unmaterialized config-only Decide pair may load");
        drop(runtime);

        // The exemption ends as soon as any pair path appears: one lone file
        // is partial materialization and must fail before dispatch.
        fs::write(
            dir.path().join("repo/Tablet/Correct.lean"),
            "partial primary",
        )
        .unwrap();
        let error = match SupervisorRuntime::load(paths) {
            Ok(_) => panic!("post-init load must reject a partial Decide pair"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("invalid disk layout"),
            "unexpected load error: {error}"
        );
    }

    #[test]
    fn active_worker_restore_preserves_precheckpoint_decide_flip_on_first_relaunch() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("Dormant")).unwrap();
        for (dir_name, node, body) in [
            ("Tablet", "Correct", "POSITIVE"),
            ("Dormant", "Correct__Refutation", "REFUTATION"),
        ] {
            fs::write(repo.join(format!("{dir_name}/{node}.lean")), body).unwrap();
            fs::write(repo.join(format!("{dir_name}/{node}.tex")), body).unwrap();
        }
        // HEAD intentionally records the pre-flip layout. The kernel flip is
        // accepted before the next checkpoint, exactly as in the regression.
        init_git_repo(&repo);
        crate::dormant_store::flip_decide_pair_on_disk(
            &repo,
            &NodeId::from("Correct__Refutation"),
            &NodeId::from("Correct"),
        )
        .unwrap();
        write_test_config(&repo);

        let mut state = base_state();
        seed_runtime_decide_pair(&mut state, true);
        let runtime_root = dir.path().join("runtime");
        let runtime = worker_inflight_runtime(&runtime_root, &repo, state);
        let request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime.capture_active_worker_base_for_request(&request).unwrap();

        // Simulate a partial worker attempt plus sandbox-created reference/.
        fs::write(repo.join("Tablet/Correct__Refutation.lean"), "DIRTY").unwrap();
        fs::remove_file(repo.join("Tablet/Correct__Refutation.tex")).unwrap();
        fs::write(repo.join("Tablet/worker_orphan.lean"), "DIRTY").unwrap();
        fs::create_dir_all(repo.join("reference")).unwrap();
        fs::write(repo.join("reference/worker.tex"), "DIRTY").unwrap();

        // Loading must tolerate a partial in-flight Worker surface so the
        // pre-dispatch restore can repair it; non-Worker loads validate the
        // same malformed Decide layout immediately.
        drop(runtime);
        let runtime = SupervisorRuntime::load(RuntimePaths::new(&runtime_root))
            .expect("reload dirty in-flight Worker for fail-closed restore");
        assert!(runtime.restore_active_worker_base_for_inflight().unwrap());
        assert_eq!(
            fs::read_to_string(repo.join("Tablet/Correct__Refutation.lean")).unwrap(),
            "REFUTATION"
        );
        assert!(repo.join("Dormant/Correct.lean").is_file());
        assert!(!repo.join("Dormant/Correct__Refutation.lean").exists());
        assert!(!repo.join("Tablet/Correct.lean").exists());
        assert!(!repo.join("Tablet/worker_orphan.lean").exists());
        assert!(
            !repo.join("reference").exists(),
            "absence of a worker source surface is part of its baseline"
        );
    }

    #[test]
    fn load_recovers_interrupted_decide_flip_before_validation() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("Dormant")).unwrap();
        // Persisted polarity is Prove: primary live in Tablet/, refutation
        // dormant in Dormant/. Seed that valid layout, then tear the FIRST
        // forward rename toward Disprove — the new-live refutation `.lean` is
        // promoted to Tablet/ while its `.tex` is still dormant (an admitted
        // recovery prefix relative to the persisted polarity).
        for (dir_name, node, body) in [
            ("Tablet", "Correct", "PRIMARY"),
            ("Dormant", "Correct__Refutation", "REFUTATION"),
        ] {
            fs::write(repo.join(format!("{dir_name}/{node}.lean")), body).unwrap();
            fs::write(repo.join(format!("{dir_name}/{node}.tex")), body).unwrap();
        }
        fs::rename(
            repo.join("Dormant/Correct__Refutation.lean"),
            repo.join("Tablet/Correct__Refutation.lean"),
        )
        .unwrap();

        let mut state = base_state();
        seed_runtime_decide_pair(&mut state, false);
        let paths = RuntimePaths::new(dir.path().join("runtime"));
        // Init does not inspect the worktree; it just persists state + metadata.
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                ..RuntimeMetadata::default()
            },
        )
        .expect("init persists a Prove-live Decide pair");

        // NO in-flight Worker request, so load runs the recover→validate pair
        // at runtime.rs:449-455 end-to-end. Recovery repairs the torn forward
        // prefix back to the persisted polarity, then validation passes — so
        // load succeeds instead of failing on the transient layout.
        let runtime =
            SupervisorRuntime::load(paths).expect("torn forward prefix is recovered at load");

        let primary = NodeId::from("Correct");
        let refutation = NodeId::from("Correct__Refutation");
        // Disk matches the persisted Prove polarity, every file in exactly one
        // location.
        assert!(crate::dormant_store::node_in_tablet(&repo, &primary));
        assert!(!crate::dormant_store::node_in_dormant(&repo, &primary));
        assert!(crate::dormant_store::node_in_dormant(&repo, &refutation));
        assert!(!crate::dormant_store::node_in_tablet(&repo, &refutation));
        crate::dormant_store::validate_configured_decide_layout(&repo, &runtime.state)
            .expect("recovered layout validates against the persisted polarity");
    }

    /// Audit round 2, follow-up 1 — ORDERING. On a `TrustBaseMode::RequiredV1`
    /// run, `reconcile_trust_journal_projection` ends in
    /// `ProtocolState::validate()`, and that is the FIRST gate a load reaches.
    /// While the stranded-Decide migration was invoked from the CLI *after*
    /// `SupervisorRuntime::load` returned, its `node_kinds` repair arm (and the
    /// `open_nodes` re-derivation guarding it) was unreachable on exactly the
    /// class of run it was written for: validate()'s Decide-pair value clause
    /// rejected the wrong kind before the repair could touch it. The migration
    /// now runs inside `load`, ahead of that validate(), so a REPAIRABLE
    /// checkpoint is repaired rather than refused.
    #[test]
    fn trust_required_load_repairs_stranded_decide_registration_before_validation() {
        let directory = local_tempdir();
        let journal_path = directory.path().join("trust-journal");
        let fixture = create_exceptional_journal_fixture(
            &journal_path,
            Some(crate::trust_base::EventKind::ProtectedReapprovalApproved),
        );
        let mut metadata =
            exceptional_runtime_metadata(directory.path(), &journal_path, &fixture);
        let repo = directory.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config(&repo);
        metadata.repo_path = Some(repo.clone());
        metadata.config_path = Some(config_path);

        // Disprove-live Decide layout on disk: the refutation in `Tablet/` with
        // a `sorry` body, the primary dormant.
        fs::create_dir_all(repo.join("Dormant")).unwrap();
        fs::write(
            repo.join("Tablet/Correct__Refutation.lean"),
            "import Tablet.Preamble\n\ntheorem Correct__Refutation : ¬ True := by\n  sorry\n",
        )
        .unwrap();
        fs::write(
            repo.join("Tablet/Correct__Refutation.tex"),
            "\\begin{theorem}not true\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        )
        .unwrap();
        fs::write(repo.join("Dormant/Correct.lean"), "PRIMARY").unwrap();
        fs::write(repo.join("Dormant/Correct.tex"), "PRIMARY").unwrap();

        let primary = crate::model::ChallengeTargetId::from("correct");
        let refutation_target = crate::model::refutation_target_id(&primary);
        let node = NodeId::from("Correct__Refutation");
        let mut state = stale_required_v1_state(
            &fixture,
            Phase::RevisionStating,
            Some("fixture-revision-lane"),
        );
        seed_runtime_decide_pair(&mut state, true);
        // Initialize from a CORRECTLY registered pair — `initialize_with_metadata`
        // validates a trust-required state, so the stranded shape has to be
        // introduced afterwards, exactly as a pre-fix binary would have left it
        // in a checkpoint.
        state.live.present_nodes.insert(node.clone());
        state.node_kinds.insert(node.clone(), crate::model::NodeKind::Proof);
        state.deps.insert(node.clone(), BTreeSet::new());
        state
            .challenge_claims
            .insert(node.clone(), BTreeSet::from([refutation_target.clone()]));
        state.node_difficulty.insert(node.clone(), crate::model::NodeDifficulty::Hard);
        state.easy_attempts.insert(node.clone(), 0);

        let runtime_root = directory.path().join("runtime");
        let runtime = SupervisorRuntime::initialize_with_metadata(
            RuntimePaths::new(&runtime_root),
            state,
            metadata,
        )
        .expect("a correctly registered Disprove-live pair initializes under trust-v1");
        assert!(runtime.state.trust_base.required());
        let state_path = runtime.paths.state_path.clone();
        drop(runtime);

        // Strand the registration in the persisted checkpoint: the `Definition`
        // default `effective_node_kinds` materialises for an unregistered
        // present node, and no challenge claim at all.
        let mut persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        persisted["node_kinds"]["Correct__Refutation"] = serde_json::json!("Definition");
        persisted["proof_nodes"] = serde_json::json!(persisted["proof_nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry.as_str() != Some("Correct__Refutation"))
            .cloned()
            .collect::<Vec<_>>());
        persisted["challenge_claims"]
            .as_object_mut()
            .unwrap()
            .remove("Correct__Refutation");
        fs::write(&state_path, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();

        // The load-time repair now precedes validate(), so this loads instead of
        // being rejected with "present decide-pair node ... has node kind
        // Definition".
        let repaired = SupervisorRuntime::load(RuntimePaths::new(&runtime_root))
            .expect("a repairable stranded Decide registration must be healed, not refused");
        assert_eq!(
            repaired.state.node_kinds.get(&node),
            Some(&crate::model::NodeKind::Proof),
            "the kind must be re-derived from the configured spec"
        );
        assert!(repaired.state.proof_nodes.contains(&node));
        assert_eq!(
            repaired.state.challenge_claims.get(&node),
            Some(&BTreeSet::from([refutation_target])),
            "and the stranded claim backfilled"
        );
        assert!(
            repaired.state.live.open_nodes.contains(&node),
            "the repaired node's body is `sorry`; openness is re-derived from disk"
        );
        // Durable: the repair was persisted, and a second load is a no-op.
        drop(repaired);
        let reloaded = SupervisorRuntime::load(RuntimePaths::new(&runtime_root))
            .expect("the healed checkpoint reloads");
        assert_eq!(
            reloaded.state.node_kinds.get(&node),
            Some(&crate::model::NodeKind::Proof)
        );
    }

    #[test]
    fn active_worker_restore_rolls_back_reference_tree_exactly() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("reference/nested")).unwrap();
        fs::write(repo.join("Tablet/a.lean"), "BASE").unwrap();
        fs::write(repo.join("reference/deviation.tex"), "BASE-REF").unwrap();
        fs::write(repo.join("reference/nested/note.tex"), "BASE-NESTED").unwrap();
        let runtime = worker_inflight_runtime(
            &dir.path().join("runtime"),
            &repo,
            base_state(),
        );
        let request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime.capture_active_worker_base_for_request(&request).unwrap();

        fs::write(repo.join("reference/deviation.tex"), "DIRTY").unwrap();
        fs::remove_file(repo.join("reference/nested/note.tex")).unwrap();
        fs::write(repo.join("reference/orphan.tex"), "DIRTY").unwrap();
        runtime.restore_active_worker_base_for_inflight().unwrap();

        assert_eq!(
            fs::read_to_string(repo.join("reference/deviation.tex")).unwrap(),
            "BASE-REF"
        );
        assert_eq!(
            fs::read_to_string(repo.join("reference/nested/note.tex")).unwrap(),
            "BASE-NESTED"
        );
        assert!(!repo.join("reference/orphan.tex").exists());
    }

    #[test]
    fn active_worker_restore_leaves_all_nonworker_kernel_surfaces_byte_identical() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("Dormant")).unwrap();
        fs::write(repo.join("Tablet/a.lean"), "BASE").unwrap();
        fs::write(repo.join("Tablet.lean"), "OLD ROOT").unwrap();
        fs::write(repo.join("PROPOSED_ASSUMPTIONS.json"), "OLD REGISTRY").unwrap();
        fs::write(repo.join("Dormant/canary.lean"), "OLD DORMANT").unwrap();
        init_git_repo(&repo);

        // Accepted kernel writes newer than HEAD and older than the worker
        // request must survive any worker retry/relaunch rollback.
        fs::write(repo.join("Tablet.lean"), "KERNEL ROOT").unwrap();
        fs::write(
            repo.join("PROPOSED_ASSUMPTIONS.json"),
            "KERNEL PENDING REGISTRY",
        )
        .unwrap();
        fs::write(repo.join("Dormant/canary.lean"), "KERNEL DORMANT").unwrap();
        fs::write(repo.join("KERNEL_MANIFEST.json"), "KERNEL MANIFEST").unwrap();

        let runtime = worker_inflight_runtime(
            &dir.path().join("runtime"),
            &repo,
            base_state(),
        );
        let request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime.capture_active_worker_base_for_request(&request).unwrap();
        fs::write(repo.join("Tablet/a.lean"), "WORKER DIRTY").unwrap();
        runtime.restore_active_worker_base_for_inflight().unwrap();

        assert_eq!(fs::read_to_string(repo.join("Tablet.lean")).unwrap(), "KERNEL ROOT");
        assert_eq!(
            fs::read_to_string(repo.join("PROPOSED_ASSUMPTIONS.json")).unwrap(),
            "KERNEL PENDING REGISTRY"
        );
        assert_eq!(
            fs::read_to_string(repo.join("Dormant/canary.lean")).unwrap(),
            "KERNEL DORMANT"
        );
        assert_eq!(
            fs::read_to_string(repo.join("KERNEL_MANIFEST.json")).unwrap(),
            "KERNEL MANIFEST"
        );
    }

    #[test]
    fn active_worker_capture_rejects_nested_symlink_and_fifo_entries() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet/nested")).unwrap();
        fs::create_dir_all(repo.join("reference")).unwrap();
        fs::write(repo.join("Tablet/a.lean"), "BASE").unwrap();
        fs::write(repo.join("reference/base.tex"), "REF-BASE").unwrap();
        fs::write(repo.join("outside"), "OUTSIDE").unwrap();
        let runtime = worker_inflight_runtime(
            &dir.path().join("runtime"),
            &repo,
            base_state(),
        );
        let request = runtime.state.in_flight_request.as_ref().unwrap().clone();

        std::os::unix::fs::symlink(
            repo.join("outside"),
            repo.join("Tablet/nested/escape"),
        )
        .unwrap();
        let error = runtime
            .capture_active_worker_base_for_request(&request)
            .unwrap_err();
        assert!(error.to_string().contains("non-regular worker-surface entry"));
        fs::remove_file(repo.join("Tablet/nested/escape")).unwrap();

        let fifo = repo.join("Tablet/nested/pipe");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let error = runtime
            .capture_active_worker_base_for_request(&request)
            .unwrap_err();
        assert!(error.to_string().contains("non-regular worker-surface entry"));
    }

    #[test]
    fn active_worker_restore_rejects_corrupt_manifest_and_snapshot_shapes() {
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("Tablet/nested")).unwrap();
        fs::create_dir_all(repo.join("reference")).unwrap();
        fs::write(repo.join("Tablet/a.lean"), "BASE").unwrap();
        fs::write(repo.join("reference/base.tex"), "REF-BASE").unwrap();
        let runtime = worker_inflight_runtime(
            &dir.path().join("runtime"),
            &repo,
            base_state(),
        );
        let request = runtime.state.in_flight_request.as_ref().unwrap().clone();
        runtime.capture_active_worker_base_for_request(&request).unwrap();

        fs::write(runtime.active_worker_base_manifest_path(), b"{bad json").unwrap();
        assert!(runtime
            .restore_active_worker_base_for_inflight()
            .unwrap_err()
            .to_string()
            .contains("is malformed"));

        fs::write(
            runtime.active_worker_base_manifest_path(),
            br#"{"schema_version":99,"tablet_present":true,"reference_present":true}"#,
        )
        .unwrap();
        assert!(runtime
            .restore_active_worker_base_for_inflight()
            .unwrap_err()
            .to_string()
            .contains("unsupported schema_version 99"));

        fs::write(
            runtime.active_worker_base_manifest_path(),
            br#"{"schema_version":1,"tablet_present":false,"reference_present":true}"#,
        )
        .unwrap();
        assert!(runtime
            .restore_active_worker_base_for_inflight()
            .unwrap_err()
            .to_string()
            .contains("disagrees with its manifest"));

        fs::write(
            runtime.active_worker_base_manifest_path(),
            br#"{"schema_version":1,"tablet_present":true,"reference_present":true}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(
            repo.join("Tablet/a.lean"),
            runtime.active_worker_base_tablet_dir().join("nested/escape"),
        )
        .unwrap();
        fs::write(repo.join("Tablet/a.lean"), "LIVE-TABLET-CANARY").unwrap();
        fs::write(repo.join("reference/base.tex"), "LIVE-REFERENCE-CANARY").unwrap();
        assert!(runtime
            .restore_active_worker_base_for_inflight()
            .unwrap_err()
            .to_string()
            .contains("non-regular worker-surface entry"));
        assert_eq!(
            fs::read_to_string(repo.join("Tablet/a.lean")).unwrap(),
            "LIVE-TABLET-CANARY"
        );
        assert_eq!(
            fs::read_to_string(repo.join("reference/base.tex")).unwrap(),
            "LIVE-REFERENCE-CANARY"
        );
    }

    #[test]
    fn restore_active_worker_base_for_inflight_errs_when_snapshot_missing_for_worker() {
        // Audit followup: previously this returned Ok(false) silently when
        // the in-flight request was a Worker but no active-worker manifest
        // snapshot existed. The bridge discarded the boolean and proceeded
        // to rebuild `before_snapshot` against dirty disk — exactly the
        // baseline-poisoning hazard the restore call was supposed to
        // prevent. Now Errs so the bridge's KernelCliError handler routes
        // to a transport_failure classification.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("repo dir");
        let mut state = base_state();
        state.stage = crate::model::Stage::Worker;
        state.cycle = 1;
        state.request_seq = 1;
        state.in_flight_request = Some(state.expected_request(1, RequestKind::Worker));
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        // Deliberately do NOT seed active_worker_base/worker_surfaces.json.
        let result = runtime.restore_active_worker_base_for_inflight();
        let Err(err) = result else {
            panic!(
                "expected Err when in-flight Worker has no snapshot dir; \
                 got Ok({:?})",
                result.unwrap()
            );
        };
        let RuntimeError::InvalidRuntimeState(msg) = err else {
            panic!("expected InvalidRuntimeState; got {:?}", err);
        };
        assert!(msg.contains("active_worker_base/worker_surfaces.json"), "msg={msg}");
        assert!(msg.contains("snapshot manifest is missing"), "msg={msg}");
    }

    #[test]
    fn restore_active_worker_base_for_inflight_returns_false_for_benign_no_inflight() {
        // The Ok(false) path should still apply for the benign cases
        // (no in-flight request, non-Worker request, no metadata) — only
        // the in-flight-Worker + missing-snapshot case errs.
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("repo dir");
        let state = base_state(); // in_flight_request = None
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: None,
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
            ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");
        assert_eq!(
            runtime.restore_active_worker_base_for_inflight().unwrap(),
            false,
            "no in-flight request → Ok(false) (nothing to restore)",
        );
    }

    #[test]
    fn delete_persisted_local_closure_record_removes_existing_file() {
        // Patch C-O HIGH 1 (c) — the engine emits
        // `ProtocolCommand::DeleteLocalClosureRecord` after invalidating
        // a record. The runtime CLI handler removes the file under
        // `<runtime_root>/checker-state/local-closure-records/<node>.json`.
        // Verify the helper does that.
        let dir = local_tempdir();
        let runtime_root = dir.path();
        let records_dir = runtime_root
            .join("checker-state")
            .join("local-closure-records");
        fs::create_dir_all(&records_dir).expect("records dir");
        let file = records_dir.join("FooNode.json");
        fs::write(&file, r#"{"node":"FooNode"}"#).expect("write record");
        assert!(file.exists(), "precondition: record file must exist");

        delete_persisted_local_closure_record(runtime_root, &NodeId::from("FooNode"));

        assert!(
            !file.exists(),
            "DeleteLocalClosureRecord command must remove the persisted file"
        );
    }

    #[test]
    fn delete_persisted_local_closure_record_is_noop_when_file_missing() {
        // Patch C-O HIGH 1 (c) — missing file is not an error; the
        // engine emits the command at the moment of in-memory
        // invalidation, but no probe may have persisted a record yet.
        let dir = local_tempdir();
        let runtime_root = dir.path();
        // No records-dir created; the helper must NOT panic.
        delete_persisted_local_closure_record(runtime_root, &NodeId::from("Ghost"));
    }

    #[test]
    fn persisted_record_path_escapes_slash_consistently() {
        // Patch C-Q Q5 — both save (`bin/runtime_cli.rs:persist_record_to_disk`)
        // and delete (`delete_persisted_local_closure_record`) must use
        // the same on-disk filename mapping. The audit flagged a
        // pre-Q5 drift where save escaped `/` but delete did not — even
        // though current `NodeId`s don't contain `/`, the helper future-
        // proofs both sites. Verify the helper's escape behavior so a
        // future drift surfaces here.
        let dir = local_tempdir();
        let runtime_root = dir.path();
        let plain = NodeId::from("FooNode");
        let with_slash = NodeId::from("Group/Inner");
        let plain_path = persisted_record_path(runtime_root, &plain);
        let slash_path = persisted_record_path(runtime_root, &with_slash);
        assert_eq!(
            plain_path.file_name().and_then(|s| s.to_str()),
            Some("FooNode.json"),
            "plain node id keeps its name + .json suffix",
        );
        assert_eq!(
            slash_path.file_name().and_then(|s| s.to_str()),
            Some("Group_Inner.json"),
            "slash in node id is replaced with `_` for filesystem safety",
        );
        // File-name helper must match the path helper's last segment.
        assert_eq!(persisted_record_file_name(&plain), "FooNode.json",);
        assert_eq!(persisted_record_file_name(&with_slash), "Group_Inner.json",);
        // And the delete site must agree with the path: write a file
        // whose name matches `persisted_record_file_name`, ask the
        // delete helper to remove it, and confirm it actually went.
        let records_dir = runtime_root
            .join("checker-state")
            .join("local-closure-records");
        fs::create_dir_all(&records_dir).expect("records dir");
        let file = records_dir.join(persisted_record_file_name(&with_slash));
        fs::write(&file, r#"{"node":"Group/Inner"}"#).expect("write record");
        assert!(file.exists(), "precondition");
        delete_persisted_local_closure_record(runtime_root, &with_slash);
        assert!(
            !file.exists(),
            "delete helper must agree with persisted_record_file_name's escape",
        );
    }

    #[test]
    fn theorem_stating_node_reset_prunes_orphan_deleted_sidecar_queue_entry() {
        // Sidecar delta audit F1: this runtime sweep deletes orphan node
        // files and installs observed state OUTSIDE `apply_event`, then
        // calls `validate()` directly — so the deterministic queue prune
        // must run here too. Without it, a queued node orphan-deleted by
        // the sweep leaves a stale queue entry, `validate()` fails the
        // queue invariant, and a legitimate reviewer theorem_stating_node
        // reset downs the whole step.
        let dir = local_tempdir();
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        for stale in ["a.lean", "a.tex", "b.lean", "b.tex"] {
            fs::remove_file(repo.join("Tablet").join(stale)).expect("drop seeded node file");
        }
        // The observation pass after the sweep needs `lean-semantic-payloads`
        // too; extend the seeded stub script with it.
        fs::write(
            repo.join(".trellis/scripts/check.py"),
            "#!/usr/bin/env python3\nimport json,sys\ncmd = sys.argv[1]\nif cmd == 'sync-tablet-support':\n    json.dump({'updated_paths': ['Tablet/INDEX.md', 'Tablet/README.md'], 'header_tex_path': 'Tablet/header.tex', 'index_md_path': 'Tablet/INDEX.md', 'readme_md_path': 'Tablet/README.md'}, sys.stdout)\n    sys.exit(0)\nif cmd == 'prepare-compiled-support':\n    json.dump({'returncode': 0, 'stdout': 'prepared', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'materialize-tablet-oleans':\n    json.dump({'returncode': 0, 'stdout': 'materialized', 'stderr': '', 'timed_out': False, 'spawn_error': ''}, sys.stdout)\n    sys.exit(0)\nif cmd == 'lean-semantic-payloads':\n    json.dump({'A': {'ok': True, 'payload': 'root|A||const|Tablet.A|theorem|type=(const True)', 'error': ''}, 'X': {'ok': True, 'payload': 'root|X||const|Tablet.X|theorem|type=(const True)', 'error': ''}, 'Preamble': {'ok': False, 'payload': '', 'error': ''}}, sys.stdout)\n    sys.exit(0)\nraise SystemExit(f'unexpected command: {cmd}')\n",
        )
        .expect("extend check script");
        let a_baseline = "import Tablet.Preamble\n-- [TABLET NODE: A]\ntheorem A : True := by\n-- BODY\n  sorry\n";
        fs::write(repo.join("Tablet/A.lean"), a_baseline).expect("write baseline A lean");
        fs::write(
            repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        )
        .expect("write A tex");
        // Theorem-stating baseline snapshot: ProofFormalization + a
        // non-empty coarse DAG containing the reset node.
        let mut baseline = ProtocolState::default();
        baseline.phase = crate::model::Phase::ProofFormalization;
        baseline.coarse_dag_nodes = set(&["A"]);
        baseline.configured_targets = set(&["t"]);
        baseline.proof_nodes = set(&["A"]);
        baseline.target_claims.insert("A".into(), set(&["t"]));
        baseline.live.present_nodes = set(&["Preamble", "A"]);
        baseline.live.open_nodes = set(&["A"]);
        baseline.live.coverage.insert("t".into(), set(&["A"]));
        baseline.committed = baseline.live.clone();
        fs::create_dir_all(repo.join(".trellis-history")).expect("history dir");
        fs::write(
            repo.join(".trellis-history/supervisor_state.json"),
            serde_json::json!({ "state": baseline }).to_string(),
        )
        .expect("write baseline snapshot");
        init_git_repo(&repo); // commit 1 = the theorem-stating baseline
        // Live edit after the baseline: A leans on new helper X.
        let a_live = "import Tablet.Preamble\nimport Tablet.X\n-- [TABLET NODE: A]\ntheorem A : True := by\n-- BODY\n  exact X\n";
        fs::write(repo.join("Tablet/A.lean"), a_live).expect("write live A lean");
        fs::write(
            repo.join("Tablet/X.lean"),
            "import Tablet.Preamble\n-- [TABLET NODE: X]\ntheorem X : True := by\n-- BODY\n  sorry\n",
        )
        .expect("write X lean");
        fs::write(
            repo.join("Tablet/X.tex"),
            "\\begin{theorem}X\\end{theorem}\n\\begin{proof}TODO\\end{proof}\n",
        )
        .expect("write X tex");
        commit_all(&repo, "live: A leans on helper X");

        let config_path = write_test_config(&repo);
        let paths = RuntimePaths::new(dir.path().join("runtime"));
        let mut state = ProtocolState::default();
        state.phase = crate::model::Phase::ProofFormalization;
        state.coarse_dag_nodes = set(&["A"]);
        state.configured_targets = set(&["t"]);
        state.proof_nodes = set(&["A", "X"]);
        state.target_claims.insert("A".into(), set(&["t"]));
        state.deps.insert("A".into(), set(&["X"]));
        state.live.present_nodes = set(&["Preamble", "A", "X"]);
        state.live.open_nodes = set(&["X"]);
        state.live.coverage.insert("t".into(), set(&["A"]));
        state.committed = state.live.clone();
        // The reviewer queued X for a sidecar grunt.
        state.sidecar_queue.push(crate::model::SidecarQueueEntry {
            node: "X".into(),
            entry_seq: 7,
            queued_at_cycle: 3,
            origin: Default::default(),
        });
        let runtime = SupervisorRuntime::initialize_with_metadata(
            paths,
            state,
            RuntimeMetadata {
                repo_path: Some(repo.clone()),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
                initial_planning_seeded: false,
                coverage_replanning_seeded: false,
                ..RuntimeMetadata::default()
            },
        )
        .expect("initialize runtime");

        let mut next_state = runtime.state.clone();
        runtime
            .restore_theorem_stating_node_and_prune_orphans(&repo, &mut next_state, &"A".into())
            .expect("reset must succeed: the orphaned queue entry is pruned, not fatal");

        assert!(
            !next_state.live.present_nodes.contains(&NodeId::from("X")),
            "the orphan sweep must have deleted X"
        );
        assert!(
            next_state.sidecar_queue.is_empty(),
            "X's stale queue entry must be pruned before validate()"
        );
        let prune = next_state
            .sidecar_queue_prune_log
            .last()
            .expect("prune log must record the removal");
        assert_eq!(prune.node.as_str(), "X");
        assert_eq!(prune.entry_seq, 7);
        assert_eq!(prune.reason, "deleted");
    }
}

use crate::engine::{apply_event, ProtocolCommand, ProtocolEvent, TransitionError};
use crate::model::{
    GateKind, NodeId, Phase, ProtocolState, ResponseStatus, WorkerOutcome, WorkingSnapshot,
    WrapperRequest, WrapperResponse, SOUND_ASSESSMENT_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
    pub event_log_path: PathBuf,
    pub checkpoint_path: PathBuf,
    pub metadata_path: PathBuf,
}

impl RuntimePaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            state_path: root.join("protocol_state.json"),
            event_log_path: root.join("event_log.jsonl"),
            checkpoint_path: root.join("checkpoint.json"),
            metadata_path: root.join("runtime_metadata.json"),
            root,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeMetadata {
    pub repo_path: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub native_history_kinds: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCheckpoint {
    pub cycle: u32,
    pub phase: Phase,
    pub gate_kind: GateKind,
    pub active_node: Option<NodeId>,
    pub committed: WorkingSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointHookPayload {
    pub root: PathBuf,
    pub state_path: PathBuf,
    pub event_log_path: PathBuf,
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

impl SupervisorRuntime {
    fn active_worker_base_dir(&self) -> PathBuf {
        self.paths.root.join("active_worker_base")
    }

    fn active_worker_base_tablet_dir(&self) -> PathBuf {
        self.active_worker_base_dir().join("Tablet")
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
        if !tablet_dir.is_dir() {
            return Ok(());
        }
        let capture_root = self.active_worker_base_dir();
        if capture_root.exists() {
            fs::remove_dir_all(&capture_root)?;
        }
        copy_dir_recursive(&tablet_dir, &self.active_worker_base_tablet_dir())?;
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
        let runtime = Self {
            paths,
            state,
            metadata,
            event_count: 0,
        };
        runtime.persist_state()?;
        runtime.persist_metadata()?;
        Ok(runtime)
    }

    pub fn load(paths: RuntimePaths) -> Result<Self, RuntimeError> {
        let state: ProtocolState = serde_json::from_str(&fs::read_to_string(&paths.state_path)?)?;
        let event_count = read_event_count(&paths.event_log_path)?;
        let metadata = read_metadata(&paths.metadata_path)?;
        let mut runtime = Self {
            paths,
            state,
            metadata,
            event_count,
        };
        runtime.state.normalize_all_structural_state();
        // Reverse indices are `#[serde(skip)]` — rebuild them from the
        // freshly-loaded `local_closure_records` before any code path can
        // run `validate()`, otherwise the new H-1 reverse-index assert
        // will fire on the first event after restart.
        crate::model::recompute_local_closure_reverse_indices(&mut runtime.state);
        let mut persist_after_load = runtime.apply_sound_assessment_schema_cutover()?;
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
        // Atomicity (audit, Option C): refuse to start if the loaded
        // state's last_clean readiness is internally inconsistent with
        // git. Fail loud here so a downstream reviewer-driven LastClean
        // doesn't either fail mid-step or silently rewind to a stale
        // tag describing a different state.
        runtime.validate_last_clean_tag_consistency()?;
        Ok(runtime)
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
        if matches!(self.state.phase, Phase::TheoremStating) {
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

    pub fn paths(&self) -> &RuntimePaths {
        &self.paths
    }

    pub fn event_count(&self) -> u64 {
        self.event_count
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
                    if self.active_worker_base_tablet_dir().is_dir() {
                        self.restore_repo_worktree_to_active_worker_base(repo_path)?;
                    } else {
                        self.restore_repo_worktree_to_head(repo_path)?;
                    }
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
                    self.restore_repo_worktree_to_last_clean(repo_path)?;
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
                ProtocolCommand::IssueRequest { .. } | ProtocolCommand::CommitCheckpoint => {}
            }
        }
        self.maybe_clear_worker_history_for_checker_mismatch(&event);
        self.apply_request_execution_hints_to_state(&mut next_state, true)?;
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
            if let Err(sink_err) = checkpoint_sink.commit(&payload) {
                self.state = pre_step_state;
                self.metadata = pre_step_metadata;
                return Err(RuntimeError::CheckpointSink(sink_err));
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
        };
        let data = serde_json::to_string_pretty(&checkpoint)?;
        fs::write(&self.paths.checkpoint_path, data)?;
        Ok(checkpoint)
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
        let record = EventLogRecord {
            index: self.event_count,
            event: event.clone(),
            commands: commands.to_vec(),
            phase: self.state.phase,
            stage: self.state.stage,
            cycle: self.state.cycle,
            ts_ms,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.paths.event_log_path)?;
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
            event_log_path: self.paths.event_log_path.clone(),
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
    fn restore_repo_worktree_to_last_clean(&self, repo_path: &Path) -> Result<(), RuntimeError> {
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
        let tag = match tags_vec.first() {
            Some(t) => t.as_str(),
            None => {
                return Err(RuntimeError::InvalidRuntimeState(
                    "LastClean reset requested but no supervisor2/clean-* tag found in repo".into(),
                ));
            }
        };
        // Preserve the abandoned lineage as a `trellis-rewound/...` branch
        // BEFORE the destructive reset. The branch ref keeps the commits
        // reachable so they can be inspected later (and pushed to the
        // archive remote, which then carries the full history of what was
        // attempted, not just the surviving line). Best-effort: branch
        // creation failure is logged but does not block the reset.
        self.preserve_abandoned_branch_for_rewind(repo_path, tag);
        for command in [vec!["reset", "--hard", tag], vec!["clean", "-fd"]] {
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
        let orphans = ProtocolState::orphan_nodes_for_graph(
            &present_after_restore,
            &coverage_after_restore,
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
        self.restore_repo_worktree_to_head(repo_path)?;
        let repo_tablet = repo_path.join("Tablet");
        if repo_tablet.exists() {
            fs::remove_dir_all(&repo_tablet)?;
        }
        copy_dir_recursive(&self.active_worker_base_tablet_dir(), &repo_tablet)?;
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
    /// IS a Worker but the `active_worker_base/Tablet/` snapshot is
    /// missing. This is the dirty-disk-relaunch hazard: the bridge calls
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
        if !self.active_worker_base_tablet_dir().is_dir() {
            return Err(RuntimeError::InvalidRuntimeState(format!(
                "restore_active_worker_base_for_inflight: in-flight Worker \
                 request id={} cycle={} but active_worker_base/Tablet/ \
                 snapshot is missing — cannot establish clean baseline for \
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
                WorkerOutcome::Invalid | WorkerOutcome::Stuck | WorkerOutcome::NeedsRestructure
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
        if !Self::worker_response_should_preserve_attempt(response) {
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
            } if Self::worker_response_should_preserve_attempt(response) => {
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
                let metadata = json!({
                    "request_id": response.request_id,
                    "cycle": response.cycle,
                    "status": format!("{:?}", response.status),
                    "outcome": format!("{:?}", response.outcome),
                    "summary": response.summary,
                    "comments": response.comments,
                    "deterministic_rejection_reasons": crate::model::prompt_safe_deterministic_worker_rejection_reasons(
                        &response.deterministic_rejection_reasons,
                    ),
                    "present_nodes": response.snapshot.present_nodes,
                    "open_nodes": response.snapshot.open_nodes,
                    "coverage": response.snapshot.coverage,
                });
                fs::write(
                    last_invalid_metadata,
                    serde_json::to_string_pretty(&metadata)? + "\n",
                )?;
            }
            ProtocolEvent::WrapperResponse {
                response: WrapperResponse::Worker(_),
            } => {
                if last_invalid_dir.exists() {
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
pub fn restore_worktree_to_head(repo_path: &Path) -> Result<(), RuntimeError> {
    for command in [
        vec!["reset", "--hard", "HEAD"],
        vec![
            "clean",
            "-fd",
            "-e",
            ".trellis-history",
            "-e",
            ".trellis-stop-after-checkpoint",
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
    Ok(())
}

/// Delete `.lake/build/lib/lean/Tablet/<stem>.{olean,ilean,olean.hash,
/// ilean.hash,c,c.hash,ll,trace}` for every `<stem>` whose Tablet/<stem>.lean
/// source file is no longer present after a worktree rewind. The artifacts
/// are gitignored so neither `git reset --hard` nor `git clean -fd` touches
/// them; without this purge, Lean's import resolver happily finds the
/// orphaned olean and consumers (probes, workers, reviewers) end up
/// reasoning about declarations whose source no longer exists.
///
/// Best-effort: ignores I/O errors (any individual deletion failure is
/// surfaced via stderr but does not abort the rewind). The set of "live"
/// stems is derived from the current on-disk Tablet/*.lean listing.
fn purge_stale_tablet_build_artifacts(repo_path: &Path) {
    use std::collections::BTreeSet;
    let tablet_dir = repo_path.join("Tablet");
    let build_dir = repo_path.join(".lake/build/lib/lean/Tablet");
    if !build_dir.is_dir() {
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
    let entries = match std::fs::read_dir(&build_dir) {
        Ok(iter) => iter,
        Err(_) => return,
    };
    let mut purged = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Stem is text before the first '.'  — handles .olean, .olean.hash,
        // .ilean, .ilean.hash, .c, .c.hash, .ll, .trace.
        let stem = match name.split('.').next() {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        if live_stems.contains(stem) {
            continue;
        }
        if let Err(err) = std::fs::remove_file(&path) {
            eprintln!(
                "trellis: failed to purge stale Tablet build artifact {}: {err}",
                path.display()
            );
            continue;
        }
        purged += 1;
    }
    if purged > 0 {
        eprintln!(
            "trellis: purged {purged} stale .lake/build/lib/lean/Tablet/ \
             entr{plural} for deleted-source nodes (post-rewind cleanup).",
            plural = if purged == 1 { "y" } else { "ies" }
        );
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

fn read_event_count(path: &Path) -> Result<u64, RuntimeError> {
    if !path.exists() {
        return Ok(0);
    }
    let text = fs::read_to_string(path)?;
    Ok(text.lines().filter(|line| !line.trim().is_empty()).count() as u64)
}

fn read_metadata(path: &Path) -> Result<RuntimeMetadata, RuntimeError> {
    if !path.exists() {
        return Ok(RuntimeMetadata::default());
    }
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
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
    let target_fingerprints = crate::runtime_cli_observations::observe_correspondence_fingerprints(
        repo_path,
        &present_nodes,
    )
    .map_err(RuntimeError::InvalidRuntimeState)?;
    let sound_current_fingerprints =
        crate::runtime_cli_observations::observe_soundness_fingerprints(repo_path, &present_nodes)
            .map_err(RuntimeError::InvalidRuntimeState)?;
    let sound_current_fingerprint_parts =
        crate::runtime_cli_observations::observe_soundness_fingerprint_parts(
            repo_path,
            &present_nodes,
        )
        .map_err(RuntimeError::InvalidRuntimeState)?;
    let sketch_proof_nodes =
        crate::runtime_cli_observations::observe_sketch_proof_nodes(repo_path, &present_nodes);
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
            substantiveness_current_fingerprints,
            protected_closure_nodes_per_target,
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
        if candidate.is_some() && parsed_state.phase == Phase::TheoremStating {
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
        TaskMode, WorkerOutcome, WorkerResponse,
    };
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

    fn seed_test_support_repo(repo: &Path) {
        fs::create_dir_all(repo.join(".trellis/scripts")).expect("script dir");
        fs::create_dir_all(repo.join("Tablet")).expect("tablet dir");
        fs::write(
            repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        )
        .expect("write preamble lean");
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
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![]);
        let outcome = runtime.step(&mut adapter).unwrap();
        assert_eq!(outcome.status, RuntimeStepStatus::Transitioned);
        assert_eq!(runtime.state().stage, crate::model::Stage::Worker);
        assert!(runtime.state().in_flight_request.is_some());
        assert!(paths.state_path.exists());
        assert!(paths.event_log_path.exists());
    }

    #[test]
    fn reload_resumes_pending_request() {
        let dir = local_tempdir();
        let paths = RuntimePaths::new(dir.path());
        let repo = dir.path().join("repo");
        seed_test_support_repo(&repo);
        let config_path = write_test_config_with_verifiers(&repo);
        // Stuck now triggers worktree restore, which calls `git reset` —
        // initialize the repo as a git worktree so the restore succeeds.
        init_git_repo(&repo);
        let mut initial = base_state();
        initial.stage = crate::model::Stage::Worker;
        initial.cycle = 3;
        initial.request_seq = 1;
        initial.in_flight_request = Some(initial.expected_request(1, RequestKind::Worker));
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
            },
        )
        .unwrap();

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
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
            },
        )
        .unwrap();

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
        SupervisorRuntime::initialize_with_metadata(
            paths.clone(),
            initial,
            RuntimeMetadata {
                repo_path: Some(repo),
                config_path: Some(config_path),
                native_history_kinds: BTreeSet::new(),
            },
        )
        .unwrap();

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
        fs::write(repo.join("Tablet/Preamble.lean"), "import BROKEN_IMPORT\n")
            .expect("write broken preamble");
        fs::write(
            repo.join("Tablet/orphan.lean"),
            "def orphan : True := True.intro\n",
        )
        .expect("write untracked orphan");
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
            },
        )
        .expect("initialize runtime");
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
            },
        )
        .expect("initialize runtime");
        // Simulate the active_worker_base capture that would have happened at
        // the end of the prior step() when the in-flight worker request was
        // issued. Under #54, kernel emits RestoreWorktreeToActiveWorkerBase
        // unconditionally on cleanup-retry rejection (see implementation
        // note in reject_cleanup_worker_response).
        copy_dir_recursive(
            &repo.join("Tablet"),
            &runtime.active_worker_base_tablet_dir(),
        )
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
        // Simulate the failure mode that motivated this fix: the prior
        // worker burst left a sibling file modified out-of-scope. With the
        // OLD predicate, this modification would survive the rollback and
        // pollute the next worker's baseline.
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
            },
        )
        .expect("initialize runtime");
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
            },
        )
        .expect("initialize runtime");

        let mut adapter = QueueAdapter::new(vec![
            WrapperResponse::HumanGate(HumanGateResponse {
                request_id: 1,
                cycle: 4,
                status: ResponseStatus::Malformed,
                choice: HumanChoice::Approve,
            }),
            WrapperResponse::HumanGate(HumanGateResponse {
                request_id: 2,
                cycle: 4,
                status: ResponseStatus::Ok,
                choice: HumanChoice::Approve,
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
            },
        )
        .unwrap();

        let mut adapter = QueueAdapter::new(vec![]);
        runtime.step(&mut adapter).unwrap();
        let lines = fs::read_to_string(paths.event_log_path).unwrap();
        assert_eq!(lines.lines().count(), 1);
    }

    #[test]
    fn restore_active_worker_base_for_inflight_errs_when_snapshot_missing_for_worker() {
        // Audit followup: previously this returned Ok(false) silently when
        // the in-flight request was a Worker but no active_worker_base/Tablet/
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
            },
        )
        .expect("initialize runtime");
        // Deliberately do NOT seed active_worker_base/Tablet/.
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
        assert!(msg.contains("active_worker_base/Tablet/"), "msg={msg}");
        assert!(msg.contains("snapshot is missing"), "msg={msg}");
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
}

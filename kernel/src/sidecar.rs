//! Parallel-closure sidecar — kernel-side surface (SIDECAR plan §1, §3.1).
//!
//! This module owns the kernel half of the sidecar interface contract:
//!   * the lazy `sidecar` config-block parse (inert when absent — the
//!     challenge-targets precedent: no block ⇒ the boundary hook is a
//!     no-op and creates no directories);
//!   * the `candidates.json` export (tmp-file + rename in the same
//!     directory — atomic on one filesystem), produced from the SAME
//!     `ProtocolState::sidecar_eligible` predicate the apply gate uses,
//!     so daemon/kernel predicate drift is structurally impossible;
//!   * (commit 5) the spool claim protocol, write-ahead journal, and
//!     apply-gate sequencing helpers.
//!
//! House style: pure decision functions separated from IO. Everything
//! filesystem-touching takes explicit paths; everything state-derived
//! is a pure function of `&ProtocolState`.

use crate::filespec_split;
use crate::model::{
    NodeId, ProtocolState, SidecarAttemptOutcome, SidecarAttemptOutcomeSource,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Directory under the runtime root that holds the whole sidecar
/// interface surface (`candidates.json`, `spool/`, `apply-journal.json`).
/// Lives OUTSIDE the repo worktree so the checkpoint hook's
/// `git add -A` can never sweep it and rewinds never touch it.
pub const SIDECAR_DIR_NAME: &str = "sidecar";
pub const SIDECAR_CANDIDATES_FILENAME: &str = "candidates.json";
pub const SIDECAR_APPLY_JOURNAL_FILENAME: &str = "apply-journal.json";
/// `candidates.json` / attempt-record schema version. Schema 2 = the
/// reviewer-managed queue redesign: the export's primary payload is the
/// kernel queue (`queue` rows with per-entry status + splice hashes),
/// plus the reviewer-advisory `eligible_now` feed and the
/// `pruned_recent` mirror of the state prune log. The file KEEPS the
/// `candidates.json` path/name (Q4 — one constant, one atomic writer;
/// the name is mildly historical).
pub const SIDECAR_SCHEMA_VERSION: u32 = 2;

pub fn sidecar_dir(runtime_root: &Path) -> PathBuf {
    runtime_root.join(SIDECAR_DIR_NAME)
}

pub fn sidecar_candidates_path(runtime_root: &Path) -> PathBuf {
    sidecar_dir(runtime_root).join(SIDECAR_CANDIDATES_FILENAME)
}

pub fn sidecar_apply_journal_path(runtime_root: &Path) -> PathBuf {
    sidecar_dir(runtime_root).join(SIDECAR_APPLY_JOURNAL_FILENAME)
}

// ====================================================================
// Config (kernel-relevant subset of the `sidecar` block)
// ====================================================================

/// The kernel-relevant subset of the `trellis.config.json` `sidecar`
/// block (§3.3): `enabled`, `apply.*`, `phases.*`. The daemon reads the
/// rest (model/budgets) with its own parser — the kernel never touches
/// those keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarRuntimeConfig {
    pub enabled: bool,
    /// Wall-clock budget for one boundary apply attempt (gates checked
    /// before each stage; trip ⇒ restore + defer per settled D2).
    pub apply_budget_seconds: u64,
    /// Max spooled attempts claimed per boundary (settled: 1).
    pub max_applies_per_boundary: u32,
    /// Max spooled attempt OUTCOMES (spent generations) claimed per
    /// boundary. Outcome ingest is pure state bookkeeping — no checker,
    /// no disk write beyond the spool moves — so the bound is only
    /// there to keep one boundary's work finite; the remainder waits in
    /// `outcomes/` for the next boundary.
    pub max_outcomes_per_boundary: u32,
    /// Phase toggles (§3.3 `phases`): both default true.
    pub phases_proof_formalization: bool,
    pub phases_stating_after_coverage: bool,
}

impl Default for SidecarRuntimeConfig {
    fn default() -> Self {
        SidecarRuntimeConfig {
            enabled: true,
            apply_budget_seconds: 300,
            max_applies_per_boundary: 1,
            max_outcomes_per_boundary: 16,
            phases_proof_formalization: true,
            phases_stating_after_coverage: true,
        }
    }
}

/// Lazily parse the `sidecar` block from `trellis.config.json`.
///
/// Returns:
///   * `Ok(None)` — file absent, or file present but no `sidecar` key,
///     or `enabled: false`: the feature is INERT (byte-identical
///     kernel; the boundary hook must not run and must not create any
///     directory).
///   * `Ok(Some(cfg))` — block present and enabled.
///   * `Err(_)` — file/block present but unreadable/malformed: fail
///     loud (an operator config error must halt, not silently disable
///     the sidecar).
pub fn load_sidecar_runtime_config(
    config_path: &Path,
) -> Result<Option<SidecarRuntimeConfig>, String> {
    if !config_path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(config_path)
        .map_err(|err| format!("failed to read config {}: {err}", config_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse config {}: {err}", config_path.display()))?;
    let Some(block) = value.get("sidecar") else {
        return Ok(None);
    };
    let obj = block.as_object().ok_or_else(|| {
        format!(
            "config {}: `sidecar` block must be a JSON object",
            config_path.display()
        )
    })?;
    let mut cfg = SidecarRuntimeConfig::default();
    if let Some(enabled) = obj.get("enabled") {
        cfg.enabled = enabled.as_bool().ok_or_else(|| {
            format!(
                "config {}: `sidecar.enabled` must be a boolean",
                config_path.display()
            )
        })?;
    }
    if let Some(apply) = obj.get("apply") {
        let apply = apply.as_object().ok_or_else(|| {
            format!(
                "config {}: `sidecar.apply` must be a JSON object",
                config_path.display()
            )
        })?;
        if let Some(v) = apply.get("budget_seconds") {
            cfg.apply_budget_seconds = v.as_u64().ok_or_else(|| {
                format!(
                    "config {}: `sidecar.apply.budget_seconds` must be a non-negative integer",
                    config_path.display()
                )
            })?;
        }
        if let Some(v) = apply.get("max_applies_per_boundary") {
            cfg.max_applies_per_boundary = u32::try_from(v.as_u64().unwrap_or(u64::MAX))
                .map_err(|_| {
                    format!(
                        "config {}: `sidecar.apply.max_applies_per_boundary` out of range",
                        config_path.display()
                    )
                })?;
        }
        if let Some(v) = apply.get("max_outcomes_per_boundary") {
            cfg.max_outcomes_per_boundary = u32::try_from(v.as_u64().unwrap_or(u64::MAX))
                .map_err(|_| {
                    format!(
                        "config {}: `sidecar.apply.max_outcomes_per_boundary` out of range",
                        config_path.display()
                    )
                })?;
        }
    }
    if let Some(phases) = obj.get("phases") {
        let phases = phases.as_object().ok_or_else(|| {
            format!(
                "config {}: `sidecar.phases` must be a JSON object",
                config_path.display()
            )
        })?;
        if let Some(v) = phases.get("proof_formalization") {
            cfg.phases_proof_formalization = v.as_bool().ok_or_else(|| {
                format!(
                    "config {}: `sidecar.phases.proof_formalization` must be a boolean",
                    config_path.display()
                )
            })?;
        }
        if let Some(v) = phases.get("stating_after_coverage") {
            cfg.phases_stating_after_coverage = v.as_bool().ok_or_else(|| {
                format!(
                    "config {}: `sidecar.phases.stating_after_coverage` must be a boolean",
                    config_path.display()
                )
            })?;
        }
    }
    if !cfg.enabled {
        return Ok(None);
    }
    Ok(Some(cfg))
}

/// Config-aware window: the state-level `sidecar_window_open` predicate
/// AND the operator's per-phase toggles. The engine's apply gate uses
/// the state-only predicate (config is not replay state); this
/// config-aware form gates the runtime hook + the export flag, so an
/// operator toggling a phase off can never be contradicted by replay.
pub fn sidecar_window_open_with_config(
    state: &ProtocolState,
    cfg: &SidecarRuntimeConfig,
) -> bool {
    use crate::model::Phase;
    match state.phase {
        Phase::ProofFormalization => {
            cfg.phases_proof_formalization && state.sidecar_window_open(&state.live)
        }
        Phase::TheoremStating | Phase::RevisionStating => {
            cfg.phases_stating_after_coverage && state.sidecar_window_open(&state.live)
        }
        _ => false,
    }
}

// ====================================================================
// candidates.json export
// ====================================================================

/// One schema-2 queue row — the daemon's work list, kernel order =
/// reviewer submission order (Q3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarQueueExportRow {
    pub node: NodeId,
    pub entry_seq: u64,
    pub queued_at_cycle: u32,
    /// `"ready"` or `"blocked:<reason>"` — the transient Q2 conditions
    /// (`active_node` / `held_target` / `pending_task` / `window_shut`)
    /// plus `blocked:unsplittable` when the node file is unreadable or
    /// fails the FILESPEC split (today's skip-with-stderr, made
    /// visible). Blocked entries are never assigned by the manager and
    /// never pruned by the kernel.
    pub status: String,
    /// SHA-256 of the full node-file pre-image (the attempt `base`;
    /// HARD gate at apply). Empty on `blocked:unsplittable`.
    #[serde(default)]
    pub node_file_sha256: String,
    /// SHA-256 of the bytes through the `-- BODY` line (splice anchor).
    /// Empty on `blocked:unsplittable`.
    #[serde(default)]
    pub statement_prefix_sha256: String,
}

/// One `eligible_now` row: the kernel-computed ADVISORY feed for the
/// reviewer's queueing decision (rendered in the grunt status table).
/// The daemon ignores it. `tier` is the sound-lane preference
/// annotation (1 = sound Pass ∪ Structural) — preference information
/// only, never a gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarEligibleNowRow {
    pub node: NodeId,
    pub tier: u8,
}

/// Recent-window bound on the `recent_closures` feed: only closures
/// landed within the last N cycles are surfaced.
pub const SIDECAR_RECENT_CLOSURES_WINDOW_CYCLES: u32 = 40;
/// Hard cap on the number of `recent_closures` rows in one export.
pub const SIDECAR_RECENT_CLOSURES_MAX: usize = 25;

/// One `recent_closures` row: a landed grunt closure's provenance
/// (attribution + when), sourced from the `ClosedBy::Sidecar` entries
/// of `ProtocolState::closure_provenance`.
/// PURE provenance — a record of completed verified work, never a
/// scheduling input (the grunt-scheduling-never-bears-on-primary
/// principle). Surfaced so the reviewer (the grunt dispatcher) sees the
/// success side of the ledger, mirroring the failure side already shown
/// via the per-node attempt digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarRecentClosureRow {
    pub node: NodeId,
    pub cycle: u32,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarCandidatesExport {
    pub schema: u32,
    /// HEAD at the just-committed checkpoint.
    pub snapshot_sha: String,
    pub generated_at_ms: u64,
    pub cycle: u32,
    pub phase: String,
    /// False in gated-off phases so the daemon can distinguish "window
    /// shut" from "export broken".
    pub sidecar_window_open: bool,
    /// The reviewer-managed queue (schema 2): per-entry status + the
    /// splice hashes, computed ONLY for queued nodes.
    #[serde(default)]
    pub queue: Vec<SidecarQueueExportRow>,
    /// Reviewer-advisory add candidates (sorted tier-then-name;
    /// already-queued nodes excluded).
    #[serde(default)]
    pub eligible_now: Vec<SidecarEligibleNowRow>,
    /// Mirror of the state prune log (newest last) — the reviewer's
    /// "where did my entry go" answer, also rendered in the status
    /// table.
    #[serde(default)]
    pub pruned_recent: Vec<crate::model::SidecarQueuePrune>,
    /// Recent landed grunt closures (the `ClosedBy::Sidecar` entries of
    /// `state.closure_provenance`), newest-first, windowed to the last
    /// `SIDECAR_RECENT_CLOSURES_WINDOW_CYCLES` cycles and capped at
    /// `SIDECAR_RECENT_CLOSURES_MAX`. The success side of the grunt
    /// ledger for the reviewer/dispatcher. Skip-when-empty so exports
    /// with no closures stay byte-identical to the pre-field format.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_closures: Vec<SidecarRecentClosureRow>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn hash_file_or_empty(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => sha256_hex(&bytes),
        Err(_) => String::new(),
    }
}

/// Pure: the eligible node list (with tiers) from the single-source
/// predicate. Ordering: BTreeSet iteration (lexicographic) — stable,
/// carrying NO scheduling meaning (random pick, tiers, tried-set are
/// all daemon-side).
pub fn sidecar_candidate_nodes(state: &ProtocolState) -> Vec<(NodeId, u8)> {
    state
        .live
        .open_nodes
        .iter()
        .filter(|node| state.sidecar_eligible(node, &state.live))
        .map(|node| (node.clone(), state.sidecar_tier(node)))
        .collect()
}

fn git_head_sha(repo_path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
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

/// Build the full schema-2 export for the current boundary. Disk
/// reads: each QUEUED node's `Tablet/<node>.lean` for the splice
/// hashes — cheaper than the schema-1 hash-every-eligible-node loop
/// (an unreadable/unsplittable file becomes a visible
/// `blocked:unsplittable` row instead of a silent skip). The
/// `eligible_now` advisory feed carries bare `{node, tier}`, no disk
/// reads. `snapshot_sha` is HEAD (the just-committed checkpoint
/// commit).
pub fn build_candidates_export(
    repo_path: &Path,
    state: &ProtocolState,
    cfg: &SidecarRuntimeConfig,
) -> SidecarCandidatesExport {
    let window_open = sidecar_window_open_with_config(state, cfg);
    let mut queue = Vec::new();
    for entry in &state.sidecar_queue {
        let node = &entry.node;
        // Transient-block classifier first (state-only), then the
        // config-aware window (an operator phase toggle can shut the
        // window while the state-only predicate says open), then the
        // disk reads.
        let mut status = match state.sidecar_queue_blocked_reason(node, &state.live) {
            Some(reason) => format!("blocked:{reason}"),
            None if !window_open => "blocked:window_shut".to_string(),
            None => "ready".to_string(),
        };
        let mut node_file_sha256 = String::new();
        let mut statement_prefix_sha256 = String::new();
        match filespec_split::read_node_file(repo_path, node.as_str()) {
            Ok((content, sha)) => match filespec_split::split(&content, node.as_str()) {
                Ok(split) => {
                    node_file_sha256 = sha;
                    statement_prefix_sha256 =
                        sha256_hex(content[..split.body_marker_end_byte].as_bytes());
                }
                Err(err) => {
                    eprintln!(
                        "trellis sidecar: queued node {} unsplittable: {err}",
                        node.as_str()
                    );
                    status = "blocked:unsplittable".to_string();
                }
            },
            Err(err) => {
                eprintln!(
                    "trellis sidecar: queued node {} unreadable: {err}",
                    node.as_str()
                );
                status = "blocked:unsplittable".to_string();
            }
        }
        queue.push(SidecarQueueExportRow {
            node: node.clone(),
            entry_seq: entry.entry_seq,
            queued_at_cycle: entry.queued_at_cycle,
            status,
            node_file_sha256,
            statement_prefix_sha256,
        });
    }
    // Advisory add-candidates: eligible now, not already queued; sorted
    // tier-then-name so the bridge's head-truncation keeps the
    // strongest sound-lane candidates inline (amendment A6).
    let mut eligible_now: Vec<SidecarEligibleNowRow> = if window_open {
        sidecar_candidate_nodes(state)
            .into_iter()
            .filter(|(node, _)| !state.sidecar_queue_contains(node))
            .map(|(node, tier)| SidecarEligibleNowRow { node, tier })
            .collect()
    } else {
        Vec::new()
    };
    eligible_now.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| a.node.cmp(&b.node)));
    // Success side of the grunt ledger: landed closures from the recent
    // cycle window, newest-first, capped. Pure provenance read of
    // `closure_provenance` — no disk, no scheduling effect. Restricted
    // to `ClosedBy::Sidecar` entries: this feed is the GRUNT ledger, and
    // the map now also carries the primary loop's worker closures. A
    // grunt closure whose node later reopened has already been pruned
    // out of the map, so a reopened node no longer shows up here as a
    // landed success.
    let min_cycle = state
        .cycle
        .saturating_sub(SIDECAR_RECENT_CLOSURES_WINDOW_CYCLES);
    let mut recent_closures: Vec<SidecarRecentClosureRow> = state
        .closure_provenance
        .iter()
        .filter(|(_, prov)| {
            prov.closed_by == crate::model::ClosedBy::Sidecar && prov.cycle >= min_cycle
        })
        .map(|(node, prov)| SidecarRecentClosureRow {
            node: node.clone(),
            cycle: prov.cycle,
            model: prov
                .sidecar
                .as_ref()
                .map(|meta| meta.model.clone())
                .unwrap_or_default(),
        })
        .collect();
    // Newest-first; node name as a stable tie-break within a cycle.
    recent_closures.sort_by(|a, b| b.cycle.cmp(&a.cycle).then_with(|| a.node.cmp(&b.node)));
    recent_closures.truncate(SIDECAR_RECENT_CLOSURES_MAX);
    SidecarCandidatesExport {
        schema: SIDECAR_SCHEMA_VERSION,
        snapshot_sha: git_head_sha(repo_path).unwrap_or_default(),
        generated_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        cycle: state.cycle,
        phase: format!("{:?}", state.phase),
        sidecar_window_open: window_open,
        queue,
        eligible_now,
        pruned_recent: state.sidecar_queue_prune_log.clone(),
        recent_closures,
    }
}

/// Write the export via tmp-file + rename in the same directory
/// (atomic on one filesystem). Creates `<runtime>/sidecar/` — callers
/// must gate on the config block being present (inertness contract).
pub fn write_candidates_export(
    runtime_root: &Path,
    export: &SidecarCandidatesExport,
) -> Result<(), String> {
    let dir = sidecar_dir(runtime_root);
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("sidecar: create {} failed: {err}", dir.display()))?;
    let final_path = sidecar_candidates_path(runtime_root);
    let tmp_path = dir.join(format!("{SIDECAR_CANDIDATES_FILENAME}.tmp"));
    let data = serde_json::to_string_pretty(export)
        .map_err(|err| format!("sidecar: serialize candidates export failed: {err}"))?;
    std::fs::write(&tmp_path, data)
        .map_err(|err| format!("sidecar: write {} failed: {err}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &final_path).map_err(|err| {
        format!(
            "sidecar: rename {} -> {} failed: {err}",
            tmp_path.display(),
            final_path.display()
        )
    })?;
    Ok(())
}

// ====================================================================
// Spool (pending/ claimed/ applied/ rejected/) — the entire IPC surface
// ====================================================================

pub const SIDECAR_SPOOL_DIR: &str = "spool";
pub const SPOOL_PENDING: &str = "pending";
pub const SPOOL_CLAIMED: &str = "claimed";
pub const SPOOL_APPLIED: &str = "applied";
pub const SPOOL_REJECTED: &str = "rejected";
/// Spent-generation lane, the exact mirror of `pending/` for the
/// outcome half: the daemon writes only INTO `outcomes/` (by rename
/// from its private `inflight/`), the kernel alone moves files out.
pub const SPOOL_OUTCOMES: &str = "outcomes";
/// Kernel-private mid-boundary processing for the outcome lane (the
/// `claimed/` analog; swept back by `sweep_orphaned_outcome_claims`).
pub const SPOOL_CLAIMED_OUTCOMES: &str = "claimed_outcomes";
/// Terminal outcome lane, verdict appended (the `applied/`+`rejected/`
/// analog — one dir, because an outcome has no apply/reject axis: it
/// either expired a generation or was dropped with a named reason).
pub const SPOOL_OUTCOMES_CONSUMED: &str = "outcomes_consumed";

/// Ownership protocol (risk 16): the daemon writes only INTO `pending/`
/// and `outcomes/` (by rename from its private `inflight/`) and only
/// READS the terminal dirs; the kernel alone moves files out of
/// `pending/` / `outcomes/`.
#[derive(Clone, Debug)]
pub struct SidecarSpool {
    pub pending: PathBuf,
    pub claimed: PathBuf,
    pub applied: PathBuf,
    pub rejected: PathBuf,
    pub outcomes: PathBuf,
    pub claimed_outcomes: PathBuf,
    pub outcomes_consumed: PathBuf,
}

pub fn spool_dirs(runtime_root: &Path) -> SidecarSpool {
    let spool = sidecar_dir(runtime_root).join(SIDECAR_SPOOL_DIR);
    SidecarSpool {
        pending: spool.join(SPOOL_PENDING),
        claimed: spool.join(SPOOL_CLAIMED),
        applied: spool.join(SPOOL_APPLIED),
        rejected: spool.join(SPOOL_REJECTED),
        outcomes: spool.join(SPOOL_OUTCOMES),
        claimed_outcomes: spool.join(SPOOL_CLAIMED_OUTCOMES),
        outcomes_consumed: spool.join(SPOOL_OUTCOMES_CONSUMED),
    }
}

pub fn ensure_spool_dirs(spool: &SidecarSpool) -> Result<(), String> {
    for dir in [
        &spool.pending,
        &spool.claimed,
        &spool.applied,
        &spool.rejected,
        &spool.outcomes,
        &spool.claimed_outcomes,
        &spool.outcomes_consumed,
    ] {
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("sidecar: create {} failed: {err}", dir.display()))?;
    }
    Ok(())
}

/// Kernel-read subset of the daemon's attempt record (§1.3 schema).
/// Serde-lenient: unknown fields (workspace / daemon_validation /
/// tokens) are preserved on disk by the Value-editing finalizers below
/// — this struct is a read view, never a rewrite vehicle.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SidecarAttemptRecord {
    pub schema: u32,
    pub attempt_id: String,
    pub node: NodeId,
    pub snapshot_sha: String,
    pub base: SidecarAttemptBase,
    pub artifact: SidecarAttemptArtifact,
    pub status: String,
    pub provenance: SidecarAttemptProvenance,
    /// Defer-then-reject counter (settled D2 + amendment A4).
    pub deferrals: u32,
    /// Queue-entry generation this attempt was assigned for (queue
    /// redesign amendment A3). The preflight rejects a mismatch with
    /// the node's CURRENT queue entry as `stale_generation`, closing
    /// the remove+re-add race: an attempt spawned for a superseded
    /// generation can never land on the re-added entry. Serde default 0
    /// (never a live generation — the counter starts at 1) so a record
    /// missing the field is stale by construction.
    pub entry_seq: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SidecarAttemptBase {
    pub node_file_sha256: String,
    pub statement_prefix_sha256: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SidecarAttemptArtifact {
    pub proof_body: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SidecarAttemptProvenance {
    pub provider: String,
    pub model: String,
    pub iterations: u32,
    pub wall_secs: f64,
    pub driver_version: String,
}

pub fn parse_attempt_record(text: &str) -> Result<SidecarAttemptRecord, String> {
    serde_json::from_str(text).map_err(|err| format!("sidecar: malformed attempt record: {err}"))
}

/// Lexicographically-first pending attempt (attempt ids are
/// timestamp-prefixed, so this is oldest-first; NO scheduling meaning —
/// ordering among simultaneously pending attempts is arbitrary).
pub fn next_pending_attempt(spool: &SidecarSpool) -> Option<PathBuf> {
    let entries = std::fs::read_dir(&spool.pending).ok()?;
    let mut names: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Claim by rename `pending/ → claimed/` (K§10.3). A rename failure
/// (daemon race, fs error) is a skip, not an abort.
pub fn claim_attempt(spool: &SidecarSpool, pending_path: &Path) -> Result<PathBuf, String> {
    let file_name = pending_path
        .file_name()
        .ok_or_else(|| "sidecar: pending path has no file name".to_string())?;
    let claimed_path = spool.claimed.join(file_name);
    std::fs::rename(pending_path, &claimed_path).map_err(|err| {
        format!(
            "sidecar: claim rename {} -> {} failed: {err}",
            pending_path.display(),
            claimed_path.display()
        )
    })?;
    Ok(claimed_path)
}

fn read_attempt_value(path: &Path) -> Result<serde_json::Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("sidecar: read {} failed: {err}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|err| format!("sidecar: parse {} failed: {err}", path.display()))
}

fn move_attempt_with_edit(
    src: &Path,
    dest_dir: &Path,
    edit: impl FnOnce(&mut serde_json::Value),
) -> Result<PathBuf, String> {
    // Malformed JSON still gets moved (wrapped) so a poison file can
    // never wedge the spool.
    let mut value = read_attempt_value(src).unwrap_or_else(|err| {
        serde_json::json!({ "malformed": true, "error": err })
    });
    edit(&mut value);
    std::fs::create_dir_all(dest_dir)
        .map_err(|err| format!("sidecar: create {} failed: {err}", dest_dir.display()))?;
    let file_name = src
        .file_name()
        .ok_or_else(|| "sidecar: attempt path has no file name".to_string())?;
    let dest = dest_dir.join(file_name);
    let tmp = dest_dir.join(format!(
        "{}.tmp",
        file_name.to_string_lossy()
    ));
    let data = serde_json::to_string_pretty(&value)
        .map_err(|err| format!("sidecar: serialize attempt failed: {err}"))?;
    std::fs::write(&tmp, data)
        .map_err(|err| format!("sidecar: write {} failed: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &dest)
        .map_err(|err| format!("sidecar: rename {} failed: {err}", tmp.display()))?;
    std::fs::remove_file(src)
        .map_err(|err| format!("sidecar: remove {} failed: {err}", src.display()))?;
    Ok(dest)
}

/// Terminal move `claimed/ → applied/ | rejected/`, appending the §1.3
/// `verdict` object. Unknown daemon fields survive (Value edit).
pub fn finalize_attempt(
    claimed_path: &Path,
    spool: &SidecarSpool,
    outcome: &str,
    reason: &str,
    cycle: u32,
    wall_ms: u64,
) -> Result<PathBuf, String> {
    let dest_dir = if outcome == "applied" {
        &spool.applied
    } else {
        &spool.rejected
    };
    move_attempt_with_edit(claimed_path, dest_dir, |value| {
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "verdict".to_string(),
                serde_json::json!({
                    "outcome": outcome,
                    "reason": reason,
                    "cycle": cycle,
                    "wall_ms": wall_ms,
                }),
            );
        }
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeferOutcome {
    /// Moved back to `pending/` with `deferrals` incremented.
    Deferred { deferrals: u32 },
    /// Deferral ceiling hit (settled D2: defer once, then reject) —
    /// moved to `rejected/` with reason `apply_timeout`.
    Rejected,
}

/// D2 mechanics + amendment A4's reuse: increment `deferrals`; a file
/// that had already been deferred once is rejected (`apply_timeout`)
/// instead of bouncing forever.
pub fn defer_attempt(
    claimed_path: &Path,
    spool: &SidecarSpool,
    cycle: u32,
    context: &str,
) -> Result<DeferOutcome, String> {
    let value = read_attempt_value(claimed_path).unwrap_or_default();
    let prior_deferrals = value
        .get("deferrals")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    if prior_deferrals >= 1 {
        finalize_attempt(
            claimed_path,
            spool,
            "rejected",
            &format!("apply_timeout ({context})"),
            cycle,
            0,
        )?;
        return Ok(DeferOutcome::Rejected);
    }
    let new_deferrals = prior_deferrals + 1;
    move_attempt_with_edit(claimed_path, &spool.pending, |value| {
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "deferrals".to_string(),
                serde_json::json!(new_deferrals),
            );
        }
    })?;
    Ok(DeferOutcome::Deferred {
        deferrals: new_deferrals,
    })
}

/// Amendment A4 — boundary/startup recovery sweep: any file still in
/// `claimed/` was orphaned by a crash mid-boundary (the kernel is the
/// only mover out of `pending/`, and it always finalizes or defers
/// within the same boundary). Sweep each back through the D2
/// defer-then-reject mechanics. No-op (and no dir creation) when the
/// spool doesn't exist.
pub fn sweep_orphaned_claims(
    spool: &SidecarSpool,
    cycle: u32,
) -> Result<Vec<(String, DeferOutcome)>, String> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&spool.claimed) {
        Ok(entries) => entries,
        Err(_) => return Ok(out),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let outcome = defer_attempt(&path, spool, cycle, "orphaned_claim")?;
        out.push((name, outcome));
    }
    Ok(out)
}

// ====================================================================
// Outcome lane (spent generations) — the auto-prune input
// ====================================================================

/// Kernel-read view of a daemon-published OUTCOME record. Serde-lenient
/// exactly like `SidecarAttemptRecord`: unknown daemon fields survive
/// the Value-editing finalizers.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SidecarOutcomeRecord {
    pub schema: u32,
    pub attempt_id: String,
    pub node: NodeId,
    /// Queue-entry generation the spent attempt was assigned for.
    /// Serde default 0 is never a live generation (the counter starts
    /// at 1), so a record missing the field can only no-op.
    pub entry_seq: u64,
    /// Daemon outcome status (`failed`, `budget_exhausted`,
    /// `skipped_giant`, `crashed`, `error`, `cancelled`).
    pub status: String,
    pub detail: String,
    /// Export `cycle` the assignment was made from. The post-rewind
    /// gate drops any outcome from a cycle the kernel has since rewound
    /// past (`export_cycle > state.cycle`).
    pub export_cycle: u32,
}

pub fn parse_outcome_record(text: &str) -> Result<SidecarOutcomeRecord, String> {
    serde_json::from_str(text).map_err(|err| format!("sidecar: malformed outcome record: {err}"))
}

/// Outcome files awaiting ingest, oldest-first by name (attempt ids are
/// timestamp-prefixed). No scheduling meaning — the apply is
/// order-insensitive; sorted purely so a boundary is deterministic.
pub fn pending_outcome_files(spool: &SidecarSpool) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(&spool.outcomes) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    names.sort();
    names
}

/// Claim by rename `outcomes/ → claimed_outcomes/` (the `claim_attempt`
/// protocol for the outcome lane).
pub fn claim_outcome(spool: &SidecarSpool, outcome_path: &Path) -> Result<PathBuf, String> {
    let file_name = outcome_path
        .file_name()
        .ok_or_else(|| "sidecar: outcome path has no file name".to_string())?;
    let claimed_path = spool.claimed_outcomes.join(file_name);
    std::fs::rename(outcome_path, &claimed_path).map_err(|err| {
        format!(
            "sidecar: outcome claim rename {} -> {} failed: {err}",
            outcome_path.display(),
            claimed_path.display()
        )
    })?;
    Ok(claimed_path)
}

/// Terminal move `claimed_outcomes/ → outcomes_consumed/`, appending a
/// `verdict` block. `disposition` is either `expired` (the batch
/// applied and this generation's entry left the queue, or was already
/// gone) or `dropped:<reason>`.
pub fn finalize_outcome(
    claimed_path: &Path,
    spool: &SidecarSpool,
    disposition: &str,
    cycle: u32,
) -> Result<PathBuf, String> {
    move_attempt_with_edit(claimed_path, &spool.outcomes_consumed, |value| {
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "verdict".to_string(),
                serde_json::json!({
                    "outcome": disposition,
                    "cycle": cycle,
                }),
            );
        }
    })
}

/// Put a claimed outcome BACK in `outcomes/` untouched — the
/// `closure_awaiting_ingest` belt. A node with a closure still waiting
/// in `pending/` / `claimed/` must never have its queue entry expired
/// (the closure apply re-asserts queue membership and would die
/// `not_queued`, destroying completed grunt work), and the outcome is
/// not consumed either: it is re-evaluated at the next boundary, by
/// which time the closure has applied (and the entry is gone, so the
/// outcome no-ops) or been rejected (and the entry is genuinely spent).
pub fn return_outcome_to_lane(claimed_path: &Path, spool: &SidecarSpool) -> Result<PathBuf, String> {
    let file_name = claimed_path
        .file_name()
        .ok_or_else(|| "sidecar: claimed outcome path has no file name".to_string())?;
    std::fs::create_dir_all(&spool.outcomes)
        .map_err(|err| format!("sidecar: create {} failed: {err}", spool.outcomes.display()))?;
    let dest = spool.outcomes.join(file_name);
    std::fs::rename(claimed_path, &dest).map_err(|err| {
        format!(
            "sidecar: outcome return rename {} -> {} failed: {err}",
            claimed_path.display(),
            dest.display()
        )
    })?;
    Ok(dest)
}

/// Nodes with a closure record still awaiting kernel ingest — anything
/// in `pending/` or `claimed/`. The `closure_awaiting_ingest` filter
/// reads this; malformed / node-less records are skipped (they cannot
/// name a node to protect).
pub fn nodes_awaiting_closure_ingest(spool: &SidecarSpool) -> std::collections::BTreeSet<NodeId> {
    let mut out = std::collections::BTreeSet::new();
    for dir in [&spool.pending, &spool.claimed] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.extension().is_some_and(|ext| ext == "json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(record) = parse_attempt_record(&text) {
                if !record.node.as_str().is_empty() {
                    out.insert(record.node);
                }
            }
        }
    }
    out
}

/// What the boundary does with one claimed outcome record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarOutcomeDisposition {
    /// Survives the pre-filter: joins this boundary's batch, and its
    /// file is consumed once the event applies.
    Expire,
    /// Consumed with a named `dropped:<reason>` verdict; contributes
    /// nothing to the batch.
    Drop { reason: String },
    /// Returned to `outcomes/` UNCONSUMED — a closure for this node is
    /// still awaiting ingest and its queue entry must survive.
    Requeue,
}

/// Pure pre-filter for one outcome record against CURRENT state (the
/// `preflight_claimed_attempt` analog for the outcome lane).
///
/// Ordering is deliberate:
///   1. post-rewind — an outcome minted from a cycle the kernel has
///      since rewound past describes a generation that no longer
///      exists; the daemon wipes its attempted-set on the same signal;
///   2. closure-awaiting-ingest — the risk-1 belt, checked BEFORE any
///      queue reasoning so a node with completed grunt work in flight
///      can never lose its entry;
///   3. the `(node, entry_seq)` generation match, which is where a
///      genuinely spent entry is identified.
pub fn classify_outcome(
    state: &ProtocolState,
    record: &SidecarOutcomeRecord,
    nodes_awaiting_closure: &std::collections::BTreeSet<NodeId>,
) -> SidecarOutcomeDisposition {
    if record.node.as_str().is_empty() {
        return SidecarOutcomeDisposition::Drop {
            reason: "malformed".to_string(),
        };
    }
    if record.export_cycle > state.cycle {
        return SidecarOutcomeDisposition::Drop {
            reason: "post_rewind".to_string(),
        };
    }
    if nodes_awaiting_closure.contains(&record.node) {
        return SidecarOutcomeDisposition::Requeue;
    }
    let Some(entry) = state
        .sidecar_queue
        .iter()
        .find(|entry| entry.node == record.node)
    else {
        return SidecarOutcomeDisposition::Drop {
            reason: "not_queued".to_string(),
        };
    };
    if entry.entry_seq != record.entry_seq {
        return SidecarOutcomeDisposition::Drop {
            reason: "stale_generation".to_string(),
        };
    }
    SidecarOutcomeDisposition::Expire
}

/// One boundary's spent-generation batch, split by the LATE
/// closure-awaiting-ingest check (see `partition_batch_against_awaiting`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidecarOutcomeBatch {
    /// Outcomes to carry into the event.
    pub outcomes: Vec<SidecarAttemptOutcome>,
    /// Claimed files to finalize into `outcomes_consumed/` once the
    /// event applies.
    pub consumable: Vec<PathBuf>,
    /// Withheld: `(node, claimed file)`. A daemon record goes back to
    /// `outcomes/` UNCONSUMED; a kernel rejection has no file and is
    /// simply not carried this boundary.
    pub withheld: Vec<(NodeId, Option<PathBuf>)>,
}

/// The LATE `closure_awaiting_ingest` check, run immediately before the
/// event is stepped.
///
/// The snapshot taken at the top of the ingest is already stale by the
/// time a batch is assembled, and the staleness is not theoretical:
/// `publish_attempt` runs in the ATTEMPT CHILD, so a closure lands in
/// `pending/` before the child's result file exists. A stop-sentinel
/// cancel or a crash-streak burn can therefore publish a spent-
/// generation report for a node whose PROOF is arriving in exactly the
/// window the claim loop spans.
///
/// Neither of the other two risk-1 defenses covers that race —
/// success-never-publishes does not apply (the daemon never saw a
/// success) and the generation match does not either (the closure is
/// for that same generation) — so this check is the whole defense, and
/// it has to read the lane as late as possible. Withheld records are
/// RETURNED, never consumed: the next boundary re-evaluates them
/// against a settled lane.
pub fn partition_batch_against_awaiting(
    batch: Vec<(SidecarAttemptOutcome, Option<PathBuf>)>,
    awaiting: &std::collections::BTreeSet<NodeId>,
) -> SidecarOutcomeBatch {
    let mut split = SidecarOutcomeBatch::default();
    for (outcome, claimed) in batch {
        if awaiting.contains(&outcome.node) {
            split.withheld.push((outcome.node, claimed));
            continue;
        }
        if let Some(claimed) = claimed {
            split.consumable.push(claimed);
        }
        split.outcomes.push(outcome);
    }
    split
}

/// What a DEFER contributes to the boundary's spent-generation batch.
///
/// The distinction is load-bearing and is exactly the D2 ceiling:
///   * `Deferred` — the attempt went BACK to `pending/` and may still
///     land on the very next boundary. Nothing is spent, so it
///     contributes NOTHING. (This is the common case: a transiently
///     busy checker, a budget trip mid-gates.)
///   * `Rejected` — the ceiling tripped (`apply_timeout`): the attempt
///     is terminally dead in `rejected/`, and since the daemon wrote
///     its `success` row before publishing, the generation is spent
///     with no daemon outcome coming. Without this arm the entry would
///     sit queued and SPENT forever — the exact failure this feature
///     exists to remove, reached by a rarer path.
///
/// `identity` is `None` for any attempt whose record did not parse or
/// did not claim `success`; such an attempt names no generation, so it
/// contributes nothing either way.
pub fn defer_outcome_contribution(
    outcome: &DeferOutcome,
    identity: Option<(NodeId, u64, String)>,
    context: &str,
) -> Option<SidecarAttemptOutcome> {
    match outcome {
        DeferOutcome::Deferred { .. } => None,
        DeferOutcome::Rejected => {
            let (node, entry_seq, attempt_id) = identity?;
            Some(SidecarAttemptOutcome {
                node,
                entry_seq,
                attempt_id,
                status: "rejected:defer_ceiling".to_string(),
                detail: format!("apply_timeout ({context})").chars().take(200).collect(),
                source: SidecarAttemptOutcomeSource::KernelReject,
            })
        }
    }
}

/// Boundary/startup recovery sweep for the outcome lane (the
/// `sweep_orphaned_claims` twin, risk 8): anything still in
/// `claimed_outcomes/` was orphaned by a crash mid-boundary. Return it
/// to `outcomes/` so the next boundary re-evaluates it — bounded by a
/// `sweep_count` so a record that somehow wedges the boundary every
/// time is finalized as `dropped:orphaned_claim` instead of bouncing
/// forever. No-op (and no dir creation) when the spool doesn't exist.
pub fn sweep_orphaned_outcome_claims(
    spool: &SidecarSpool,
    cycle: u32,
) -> Result<Vec<(String, bool)>, String> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&spool.claimed_outcomes) {
        Ok(entries) => entries,
        Err(_) => return Ok(out),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let sweeps = read_attempt_value(&path)
            .ok()
            .and_then(|value| value.get("sweep_count").and_then(|v| v.as_u64()))
            .unwrap_or(0);
        if sweeps >= 1 {
            finalize_outcome(&path, spool, "dropped:orphaned_claim", cycle)?;
            out.push((name, false));
            continue;
        }
        move_attempt_with_edit(&path, &spool.outcomes, |value| {
            if let Some(map) = value.as_object_mut() {
                map.insert("sweep_count".to_string(), serde_json::json!(sweeps + 1));
            }
        })?;
        out.push((name, true));
    }
    Ok(out)
}

// ====================================================================
// Write-ahead apply journal (§1.4) + recovery (amendments A5/A7)
// ====================================================================

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarApplyJournal {
    pub attempt_id: String,
    pub node: NodeId,
    /// Repo-relative path of the file the apply writes.
    pub file: String,
    /// SHA-256 of the pre-image. DIAGNOSTIC ONLY at recovery (§1.4
    /// refinement): after a LastClean rewind HEAD may legitimately
    /// differ from the recorded pre-image; the recovery invariant is
    /// "the file matches HEAD", never "the file matches pre_image_sha".
    pub pre_image_sha: String,
}

pub fn write_apply_journal(
    runtime_root: &Path,
    journal: &SidecarApplyJournal,
) -> Result<(), String> {
    let dir = sidecar_dir(runtime_root);
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("sidecar: create {} failed: {err}", dir.display()))?;
    let path = sidecar_apply_journal_path(runtime_root);
    let tmp = dir.join(format!("{SIDECAR_APPLY_JOURNAL_FILENAME}.tmp"));
    let data = serde_json::to_string_pretty(journal)
        .map_err(|err| format!("sidecar: serialize journal failed: {err}"))?;
    std::fs::write(&tmp, data)
        .map_err(|err| format!("sidecar: write {} failed: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|err| format!("sidecar: rename journal failed: {err}"))?;
    Ok(())
}

pub fn clear_apply_journal(runtime_root: &Path) {
    let path = sidecar_apply_journal_path(runtime_root);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => eprintln!(
            "trellis sidecar: failed to clear apply journal {}: {err}",
            path.display()
        ),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalRecoveryOutcome {
    /// No journal on disk — nothing to recover.
    NoJournal,
    /// File already matches HEAD ⇒ either the apply fully completed
    /// (event + commit landed) or the commit landed without the event
    /// persist (amendment A7's crash window). Both clear the journal:
    /// in the A7 case the disk-closed / state-open divergence
    /// self-heals via `open_nodes_from_repo` →
    /// `local_closure_unverified_nodes` → one synthesized
    /// re-verification. Never restored, never an error.
    ClearedFileMatchesHead,
    /// Un-evented dirty write found ⇒ restored the file to HEAD
    /// (`git checkout -- <file>`). `restored_sha_matches_pre_image`
    /// is diagnostic only (false after a LastClean rewind moved HEAD
    /// under the journal — logged, never failed on).
    RestoredDirtyFile { restored_sha_matches_pre_image: bool },
}

fn git_file_matches_head(repo_path: &Path, rel_file: &str) -> Result<bool, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["diff", "--quiet", "HEAD", "--"])
        .arg(rel_file)
        .output()
        .map_err(|err| format!("sidecar: git diff failed: {err}"))?;
    Ok(output.status.success())
}

fn git_checkout_file(repo_path: &Path, rel_file: &str) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["checkout", "--"])
        .arg(rel_file)
        .output()
        .map_err(|err| format!("sidecar: git checkout failed: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "sidecar: git checkout -- {rel_file} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

/// Journal recovery (§1.4). Amendment A5: runs UNCONDITIONALLY whenever
/// `apply-journal.json` exists — NOT gated on the sidecar config block
/// (a crash after the operator removed the block must still be
/// recovered). Creates nothing when there is no journal.
pub fn run_journal_recovery(
    runtime_root: &Path,
    repo_path: &Path,
) -> Result<JournalRecoveryOutcome, String> {
    let path = sidecar_apply_journal_path(runtime_root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalRecoveryOutcome::NoJournal)
        }
        Err(err) => return Err(format!("sidecar: read journal failed: {err}")),
    };
    let journal: SidecarApplyJournal = serde_json::from_str(&text)
        .map_err(|err| format!("sidecar: malformed apply journal {}: {err}", path.display()))?;
    if git_file_matches_head(repo_path, &journal.file)? {
        eprintln!(
            "trellis sidecar: journal recovery — {} matches HEAD (apply completed or committed-before-event-persist); clearing journal for attempt {}",
            journal.file, journal.attempt_id
        );
        clear_apply_journal(runtime_root);
        return Ok(JournalRecoveryOutcome::ClearedFileMatchesHead);
    }
    git_checkout_file(repo_path, &journal.file)?;
    let restored_sha = hash_file_or_empty(&repo_path.join(&journal.file));
    let matches = restored_sha == journal.pre_image_sha;
    if !matches {
        eprintln!(
            "trellis sidecar: journal recovery — restored {} to HEAD but sha {} != journalled pre_image_sha {} (expected after a LastClean rewind; diagnostic only)",
            journal.file, restored_sha, journal.pre_image_sha
        );
    } else {
        eprintln!(
            "trellis sidecar: journal recovery — restored un-evented dirty write to {} (attempt {})",
            journal.file, journal.attempt_id
        );
    }
    // Keep the supervisor-workspace mirror coherent with the restore.
    if let Some(tablet) = supervisor_workspace_tablet_dir(repo_path) {
        let src = repo_path.join(&journal.file);
        if let (Ok(bytes), Some(name)) = (std::fs::read(&src), Path::new(&journal.file).file_name()) {
            let _ = std::fs::write(tablet.join(name), bytes);
        }
    }
    clear_apply_journal(runtime_root);
    Ok(JournalRecoveryOutcome::RestoredDirtyFile {
        restored_sha_matches_pre_image: matches,
    })
}

// ====================================================================
// Splice + sidecar-specific ban scan (amendment A3)
// ====================================================================

/// Sidecar-specific banned tokens BEYOND `LEAN_FORBIDDEN_KEYWORDS`.
/// `admit` / the syntax-extension forms come from the exec research
/// (E§5.3); `attribute`, `deriving`, `export` are amendment A3 —
/// command forms that slip past the declaration-shape gate and can
/// globally mutate typeclass / reducibility / name resolution for
/// OTHER nodes (e.g. `attribute [simp] foo`, `export Foo (bar)`).
pub const SIDECAR_EXTRA_BANNED_TOKENS: &[&str] = &[
    "admit",
    "macro_rules",
    "elab_rules",
    "declare_syntax_cat",
    "binder_predicate",
    "attribute",
    "deriving",
    "export",
];

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '\''
}

fn text_contains_token(text: &str, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let mut search_start = 0usize;
    while let Some(found) = text[search_start..].find(token) {
        let start = search_start + found;
        let end = start + token.len();
        let prev = text[..start].chars().next_back();
        let next = text[end..].chars().next();
        if !prev.is_some_and(is_token_char) && !next.is_some_and(is_token_char) {
            return true;
        }
        search_start = end;
    }
    false
}

/// Kernel-side sidecar body ban scan: token-boundary scan of the RAW
/// body (comments and strings INCLUDED — deliberately stricter than the
/// primary `keyword_clean` scan's comment masking: a sidecar body has
/// no legitimate use for any of these tokens anywhere, and the driver
/// prompt states the same rule). Returns the first hit.
pub fn sidecar_body_ban_scan(proof_body: &str) -> Option<String> {
    for token in crate::backend::LEAN_FORBIDDEN_KEYWORDS {
        if text_contains_token(proof_body, token) {
            return Some((*token).to_string());
        }
    }
    for token in SIDECAR_EXTRA_BANNED_TOKENS {
        if text_contains_token(proof_body, token) {
            return Some((*token).to_string());
        }
    }
    None
}

/// The one legal sidecar mutation: replace everything after the
/// `-- BODY` marker line of the pre-image with `proof_body`, keeping
/// the prefix (through the marker line, inclusive) byte-frozen. A body
/// smuggling its own `-- BODY` line yields a two-marker file that the
/// gate-6 `validate_filespec` rejects — asserted by tests, not trusted.
pub fn splice_proof_body(
    pre_image: &str,
    node: &str,
    proof_body: &str,
) -> Result<String, String> {
    let split = filespec_split::split(pre_image, node)?;
    let prefix = &pre_image[..split.body_marker_end_byte];
    Ok(format!("{prefix}{proof_body}"))
}

/// Detect the two-repo production layout: the kernel's `repo_path` is
/// the run (worker) repo, and the CHECKER compiles in the supervisor
/// workspace at `<repo>/.trellis/supervisor/repo` (the
/// `sync_supervisor_workspace` layout). When present, the spliced (or
/// restored) node file must be mirrored INTO the supervisor workspace
/// before any checker-mediated gate runs, or gates 7–8 would compile
/// the STALE pre-splice bytes (the primary path gets this from
/// `sync_supervisor_workspace` at acceptance; the boundary hook owns
/// its own one-file sync). Exposed by the A6 real-checker E2E rig.
pub fn supervisor_workspace_tablet_dir(repo_path: &Path) -> Option<PathBuf> {
    let tablet = repo_path
        .join(".trellis")
        .join("supervisor")
        .join("repo")
        .join("Tablet");
    if tablet.is_dir() {
        Some(tablet)
    } else {
        None
    }
}

/// Mirror one node file's bytes into the supervisor workspace when the
/// two-repo layout is present. Returns whether a mirror write happened.
pub fn mirror_node_file_to_supervisor_workspace(
    repo_path: &Path,
    node: &str,
    content: &str,
) -> Result<bool, String> {
    let Some(tablet) = supervisor_workspace_tablet_dir(repo_path) else {
        return Ok(false);
    };
    let dst = tablet.join(format!("{node}.lean"));
    std::fs::write(&dst, content)
        .map_err(|err| format!("sidecar: mirror write {} failed: {err}", dst.display()))?;
    Ok(true)
}

// ====================================================================
// Pure preflight decision (gates 2–3b) — the drift-disposition surface
// ====================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarPreflight {
    Proceed,
    Reject { reason: String },
}

/// Pure gates 2–3b over explicit inputs: eligibility recheck against
/// CURRENT state (window included, config-aware), the HARD pre-image
/// content gate, and the A3 ban scan. Everything after (journal,
/// splice, filespec, compile probe, closure probe, corr no-drift) is
/// IO-bound and lives with the runtime hook. Content-addressed, not
/// commit-addressed: `snapshot_sha` drift alone never rejects.
pub fn preflight_claimed_attempt(
    state: &ProtocolState,
    cfg: &SidecarRuntimeConfig,
    record: &SidecarAttemptRecord,
    current_node_file: Option<&str>,
) -> SidecarPreflight {
    if record.status != "success" {
        return SidecarPreflight::Reject {
            reason: format!("not_success (status={})", record.status),
        };
    }
    if record.node.is_empty() {
        return SidecarPreflight::Reject {
            reason: "malformed (empty node)".to_string(),
        };
    }
    if !sidecar_window_open_with_config(state, cfg)
        || !state.sidecar_eligible(&record.node, &state.live)
    {
        return SidecarPreflight::Reject {
            reason: "ineligible".to_string(),
        };
    }
    // Queue-membership gate (queue redesign §1.5): the reviewer-
    // authority loop's daemon-misbehavior guard — a spooled result for
    // any node the reviewer has not (still) queued dies in
    // `rejected/not_queued` with the worktree untouched.
    let Some(queue_entry) = state
        .sidecar_queue
        .iter()
        .find(|entry| entry.node == record.node)
    else {
        return SidecarPreflight::Reject {
            reason: "not_queued".to_string(),
        };
    };
    // Generation gate (amendment A3): the attempt must have been
    // spawned for the CURRENT queue entry, not a removed-then-re-added
    // predecessor. Rejections are event-invisible (settled D8), so no
    // event-schema change rides on this.
    if record.entry_seq != queue_entry.entry_seq {
        return SidecarPreflight::Reject {
            reason: format!(
                "stale_generation (record entry_seq {} != queued entry_seq {})",
                record.entry_seq, queue_entry.entry_seq
            ),
        };
    }
    let Some(content) = current_node_file else {
        return SidecarPreflight::Reject {
            reason: "stale_content (node file unreadable)".to_string(),
        };
    };
    if sha256_hex(content.as_bytes()) != record.base.node_file_sha256 {
        return SidecarPreflight::Reject {
            reason: "stale_content".to_string(),
        };
    }
    if let Some(token) = sidecar_body_ban_scan(&record.artifact.proof_body) {
        return SidecarPreflight::Reject {
            reason: format!("banned_token ({token})"),
        };
    }
    SidecarPreflight::Proceed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CorrStatus, NodeKind, Phase};

    fn write_config(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("trellis.config.json");
        std::fs::write(&path, contents).expect("write config");
        path
    }

    fn tempdir() -> tempfile::TempDir {
        // Local .tmp-tests root (never /tmp): mirrors runtime.rs's
        // `local_tempdir` convention.
        let tmp_root = std::env::current_dir()
            .expect("current dir")
            .join(".tmp-tests");
        std::fs::create_dir_all(&tmp_root).expect("tmp root");
        tempfile::tempdir_in(&tmp_root).expect("tempdir")
    }

    #[test]
    fn config_absent_file_is_inert() {
        let dir = tempdir();
        let missing = dir.path().join("no-such-config.json");
        assert_eq!(load_sidecar_runtime_config(&missing), Ok(None));
    }

    #[test]
    fn config_absent_block_is_inert() {
        let dir = tempdir();
        let path = write_config(dir.path(), r#"{"worker": {"provider": "codex"}}"#);
        assert_eq!(load_sidecar_runtime_config(&path), Ok(None));
    }

    #[test]
    fn config_disabled_block_is_inert() {
        let dir = tempdir();
        let path = write_config(dir.path(), r#"{"sidecar": {"enabled": false}}"#);
        assert_eq!(load_sidecar_runtime_config(&path), Ok(None));
    }

    #[test]
    fn config_parses_apply_and_phase_knobs() {
        let dir = tempdir();
        let path = write_config(
            dir.path(),
            r#"{"sidecar": {"enabled": true,
                 "apply": {"budget_seconds": 120, "max_applies_per_boundary": 2},
                 "phases": {"proof_formalization": true, "stating_after_coverage": false}}}"#,
        );
        let cfg = load_sidecar_runtime_config(&path)
            .expect("parse ok")
            .expect("block present");
        assert_eq!(cfg.apply_budget_seconds, 120);
        assert_eq!(cfg.max_applies_per_boundary, 2);
        assert!(cfg.phases_proof_formalization);
        assert!(!cfg.phases_stating_after_coverage);
    }

    #[test]
    fn config_malformed_block_fails_loud() {
        let dir = tempdir();
        let path = write_config(dir.path(), r#"{"sidecar": {"enabled": "yes"}}"#);
        assert!(load_sidecar_runtime_config(&path).is_err());
        let path2 = write_config(dir.path(), r#"{"sidecar": []}"#);
        assert!(load_sidecar_runtime_config(&path2).is_err());
    }

    fn eligible_state() -> ProtocolState {
        let mut state = ProtocolState::default();
        state.phase = Phase::ProofFormalization;
        let n = NodeId::from("Rung");
        state.live.present_nodes.insert(n.clone());
        state.live.open_nodes.insert(n.clone());
        state.node_kinds.insert(n.clone(), NodeKind::Proof);
        state.proof_nodes.insert(n.clone());
        state.corr_status.insert(n.clone(), CorrStatus::Pass);
        state
            .live
            .corr_current_fingerprints
            .insert(n.clone(), "c1".to_string());
        state
            .corr_approved_fingerprints
            .insert(n.clone(), "c1".to_string());
        state.substantiveness_status.insert(n.clone(), CorrStatus::Pass);
        state
            .live
            .substantiveness_current_fingerprints
            .insert(n.clone(), "s1".to_string());
        state
            .substantiveness_approved_fingerprints
            .insert(n.clone(), "s1".to_string());
        state
    }

    fn seed_node_file(repo: &Path) {
        let tablet = repo.join("Tablet");
        std::fs::create_dir_all(&tablet).expect("mkdir Tablet");
        std::fs::write(
            tablet.join("Rung.lean"),
            "import Tablet.Preamble\n\n-- [TABLET NODE: Rung]\ntheorem Rung : True := by\n-- BODY\n  sorry\n",
        )
        .expect("write node");
    }

    fn queue_state(entries: &[(&str, u64)]) -> ProtocolState {
        let mut state = eligible_state();
        for (name, seq) in entries {
            state.sidecar_queue.push(crate::model::SidecarQueueEntry {
                node: NodeId::from(*name),
                entry_seq: *seq,
                queued_at_cycle: 3,
            });
            state.sidecar_queue_seq = state.sidecar_queue_seq.max(*seq);
        }
        state
    }

    #[test]
    fn export_schema2_unqueued_eligible_node_feeds_eligible_now_only() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let state = eligible_state();
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        assert!(export.sidecar_window_open);
        assert_eq!(export.schema, SIDECAR_SCHEMA_VERSION);
        assert!(export.queue.is_empty(), "nothing queued");
        assert_eq!(export.eligible_now.len(), 1);
        assert_eq!(export.eligible_now[0].node.as_str(), "Rung");
        assert_eq!(export.eligible_now[0].tier, 2);
        assert!(export.pruned_recent.is_empty());
    }

    #[test]
    fn export_schema2_queued_ready_row_carries_hashes_and_leaves_add_feed() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let state = queue_state(&[("Rung", 7)]);
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        assert_eq!(export.queue.len(), 1);
        let row = &export.queue[0];
        assert_eq!(row.node.as_str(), "Rung");
        assert_eq!(row.entry_seq, 7);
        assert_eq!(row.queued_at_cycle, 3);
        assert_eq!(row.status, "ready");
        // The prefix hash covers bytes through the `-- BODY` line: a
        // body-only change must NOT move it, a statement change must.
        let content = std::fs::read_to_string(repo.join("Tablet/Rung.lean")).unwrap();
        let split = filespec_split::split(&content, "Rung").unwrap();
        assert_eq!(
            row.statement_prefix_sha256,
            sha256_hex(content[..split.body_marker_end_byte].as_bytes())
        );
        assert_eq!(row.node_file_sha256, sha256_hex(content.as_bytes()));
        // A queued node leaves the advisory add feed.
        assert!(export.eligible_now.is_empty());
    }

    #[test]
    fn export_schema2_blocked_rows_and_unsplittable() {
        // Transient block: routed active node.
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = queue_state(&[("Rung", 7)]);
        state.active_node = Some(NodeId::from("Rung"));
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        assert_eq!(export.queue[0].status, "blocked:active_node");
        // Blocked rows still carry the hashes (the file is fine).
        assert!(!export.queue[0].node_file_sha256.is_empty());

        // Unsplittable: missing node file.
        let state = queue_state(&[("Ghost", 8)]);
        let export = build_candidates_export(&repo, &state, &cfg);
        let ghost = export
            .queue
            .iter()
            .find(|row| row.node.as_str() == "Ghost")
            .expect("queued row exported even when unsplittable");
        assert_eq!(ghost.status, "blocked:unsplittable");
        assert!(ghost.node_file_sha256.is_empty());
    }

    #[test]
    fn export_schema2_pruned_recent_mirrors_state_log() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = eligible_state();
        state.sidecar_queue_prune_log.push(crate::model::SidecarQueuePrune {
            node: NodeId::from("Old"),
            entry_seq: 3,
            cycle: 2,
            reason: "closed".to_string(),
        });
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        assert_eq!(export.pruned_recent, state.sidecar_queue_prune_log);
    }

    fn insert_closure(state: &mut ProtocolState, node: &str, cycle: u32, model: &str) {
        state.closure_provenance.insert(
            NodeId::from(node),
            crate::model::NodeClosureProvenance {
                closed_by: crate::model::ClosedBy::Sidecar,
                cycle,
                sidecar: Some(crate::model::SidecarClosureMeta {
                    attempt_id: format!("sc-{node}"),
                    provider: "prov".to_string(),
                    model: model.to_string(),
                    wall_ms: 1234,
                }),
            },
        );
    }

    fn insert_worker_closure(state: &mut ProtocolState, node: &str, cycle: u32) {
        state.closure_provenance.insert(
            NodeId::from(node),
            crate::model::NodeClosureProvenance {
                closed_by: crate::model::ClosedBy::Worker,
                cycle,
                sidecar: None,
            },
        );
    }

    #[test]
    fn export_recent_closures_windowed_and_newest_first() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = eligible_state();
        state.cycle = 300;
        // Window is the last SIDECAR_RECENT_CLOSURES_WINDOW_CYCLES (40)
        // cycles, so min_cycle = 260 (boundary inclusive).
        insert_closure(&mut state, "InWindowA", 290, "mistral");
        insert_closure(&mut state, "InWindowB", 295, "codex");
        insert_closure(&mut state, "Boundary", 260, "gemini");
        insert_closure(&mut state, "TooOld", 259, "old");
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        let rows: Vec<(&str, u32, &str)> = export
            .recent_closures
            .iter()
            .map(|r| (r.node.as_str(), r.cycle, r.model.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("InWindowB", 295, "codex"),
                ("InWindowA", 290, "mistral"),
                ("Boundary", 260, "gemini"),
            ],
            "newest-first, boundary included, TooOld dropped"
        );
    }

    /// `recent_closures` is the GRUNT ledger. Now that
    /// `closure_provenance` also holds the primary loop's worker
    /// closures, the export must filter to `ClosedBy::Sidecar` — a
    /// worker closure in the same window is not a grunt success.
    #[test]
    fn export_recent_closures_excludes_worker_closures() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = eligible_state();
        state.cycle = 300;
        insert_closure(&mut state, "GruntClosed", 290, "mistral");
        insert_worker_closure(&mut state, "WorkerClosed", 291);
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        let rows: Vec<&str> = export
            .recent_closures
            .iter()
            .map(|r| r.node.as_str())
            .collect();
        assert_eq!(rows, vec!["GruntClosed"]);
    }

    #[test]
    fn export_recent_closures_capped_to_max() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = eligible_state();
        state.cycle = 100;
        // 30 closures, all inside the 40-cycle window (cycles 71..=100).
        for i in 0..30u32 {
            insert_closure(&mut state, &format!("N{i:02}"), 71 + i, "m");
        }
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        assert_eq!(export.recent_closures.len(), SIDECAR_RECENT_CLOSURES_MAX);
        // Newest kept: highest cycle first, oldest (cycle 76 and below)
        // dropped by the cap.
        assert_eq!(export.recent_closures[0].cycle, 100);
        assert_eq!(
            export.recent_closures.last().unwrap().cycle,
            100 - (SIDECAR_RECENT_CLOSURES_MAX as u32 - 1)
        );
    }

    #[test]
    fn export_recent_closures_empty_skips_serialization() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let state = eligible_state();
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        assert!(export.recent_closures.is_empty());
        // Skip-when-empty keeps the pre-field export byte-stable.
        let json = serde_json::to_value(&export).unwrap();
        assert!(
            json.get("recent_closures").is_none(),
            "empty recent_closures must be omitted from serialized export"
        );
        // Additive default: an export doc without the field deserializes.
        let mut obj = json.as_object().unwrap().clone();
        obj.remove("recent_closures");
        let round: SidecarCandidatesExport =
            serde_json::from_value(serde_json::Value::Object(obj)).unwrap();
        assert!(round.recent_closures.is_empty());
    }

    #[test]
    fn export_eligible_now_sorted_tier_then_name() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = eligible_state();
        // Two more eligible nodes; make "Zeta" tier 1 (sound VerifierPass).
        for name in ["Alpha", "Zeta"] {
            let n = NodeId::from(name);
            state.live.present_nodes.insert(n.clone());
            state.live.open_nodes.insert(n.clone());
            state.node_kinds.insert(n.clone(), NodeKind::Proof);
            state.proof_nodes.insert(n.clone());
            state.corr_status.insert(n.clone(), CorrStatus::Pass);
            state
                .live
                .corr_current_fingerprints
                .insert(n.clone(), "c1".to_string());
            state
                .corr_approved_fingerprints
                .insert(n.clone(), "c1".to_string());
            state
                .substantiveness_status
                .insert(n.clone(), CorrStatus::Pass);
            state
                .live
                .substantiveness_current_fingerprints
                .insert(n.clone(), "s1".to_string());
            state
                .substantiveness_approved_fingerprints
                .insert(n.clone(), "s1".to_string());
        }
        state.sound_assessments.insert(
            NodeId::from("Zeta"),
            crate::model::SoundAssessment {
                status: crate::model::SoundAssessmentStatus::VerifierPass,
                origin: crate::model::AssessmentOrigin::VerifierPanel,
                fingerprints: Default::default(),
                lane_votes: Default::default(),
                reviewer_action_id: None,
            },
        );
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        let rows: Vec<(&str, u8)> = export
            .eligible_now
            .iter()
            .map(|row| (row.node.as_str(), row.tier))
            .collect();
        assert_eq!(
            rows,
            vec![("Zeta", 1), ("Alpha", 2), ("Rung", 2)],
            "tier-1 first, then name order (A6 head-truncation keeps tier 1 inline)"
        );
    }

    #[test]
    fn export_window_shut_in_cleanup_blocks_queue_and_empties_eligible_now() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = queue_state(&[("Rung", 7)]);
        state.phase = Phase::Cleanup;
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg);
        assert!(!export.sidecar_window_open);
        assert!(export.eligible_now.is_empty());
        // The queued entry stays visible — blocked, never silently gone
        // (Q2: transient window conditions block, only durable
        // conditions prune).
        assert_eq!(export.queue[0].status, "blocked:window_shut");
    }

    #[test]
    fn export_respects_config_phase_toggle() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let state = queue_state(&[("Rung", 7)]);
        let cfg = SidecarRuntimeConfig {
            phases_proof_formalization: false,
            ..SidecarRuntimeConfig::default()
        };
        let export = build_candidates_export(&repo, &state, &cfg);
        assert!(!export.sidecar_window_open);
        assert!(export.eligible_now.is_empty());
        assert_eq!(
            export.queue[0].status, "blocked:window_shut",
            "config-off phase toggle surfaces as a blocked row, exactly like the state window"
        );
    }

    #[test]
    fn boundary_export_components_inert_without_config_block() {
        // The Run-loop boundary hook (`run_sidecar_boundary_hook` in
        // runtime_cli) gates on `load_sidecar_runtime_config` before
        // calling `build_candidates_export` + `write_candidates_export`;
        // pin each component of that composition.
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let runtime_root = dir.path().join("runtime");
        std::fs::create_dir_all(&runtime_root).unwrap();
        let config_path = write_config(dir.path(), r#"{"worker": {"provider": "codex"}}"#);
        let state = eligible_state();

        // Config without the block: the gate yields None with NO
        // filesystem side effects, so the hook never reaches the
        // export half and <runtime>/sidecar/ is never created.
        let cfg = load_sidecar_runtime_config(&config_path).unwrap();
        assert!(cfg.is_none());
        assert!(
            !sidecar_dir(&runtime_root).exists(),
            "inert config gate must not create <runtime>/sidecar/"
        );

        // With the block: export lands via tmp+rename, no tmp residue.
        let config_path = write_config(dir.path(), r#"{"sidecar": {"enabled": true}}"#);
        let cfg = load_sidecar_runtime_config(&config_path)
            .unwrap()
            .expect("enabled block parses");
        let export = build_candidates_export(&repo, &state, &cfg);
        write_candidates_export(&runtime_root, &export).unwrap();
        let out = sidecar_candidates_path(&runtime_root);
        assert!(out.exists());
        assert!(!sidecar_dir(&runtime_root)
            .join(format!("{SIDECAR_CANDIDATES_FILENAME}.tmp"))
            .exists());
        let parsed: SidecarCandidatesExport =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(parsed.schema, SIDECAR_SCHEMA_VERSION);
        assert_eq!(parsed.eligible_now.len(), 1);
        assert!(parsed.queue.is_empty());
    }
}

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
use crate::model::{NodeId, ProtocolState, SidecarAttemptOutcome};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Directory under the runtime root that holds the whole sidecar
/// interface surface (`candidates.json`, `spool/`, `apply-journal.json`).
/// Lives OUTSIDE the repo worktree so the checkpoint hook's
/// `git add -A` can never sweep it and rewinds never touch it.
pub const SIDECAR_DIR_NAME: &str = "sidecar";
pub const SIDECAR_CANDIDATES_FILENAME: &str = "candidates.json";
/// The REVIEWER's copy of the export: byte-for-byte the same document
/// minus `kernel_queue`.
///
/// Two files rather than one, because the two readers have opposite
/// requirements. The daemon needs BOTH lanes in ONE atomic read (a
/// cancel-watch that unions keys across two independently renamed
/// files would cancel live attempts on a torn read). The reviewer must
/// not see the kernel lane at all — and `candidates.json` is
/// ro-bind-mounted into the reviewer's sandbox
/// (`trellis.sandbox._reviewer_sidecar_readonly_files`), so hiding a
/// field from the rendered prompt block would hide nothing. Redacting
/// at the writer, into a path the sandbox binds instead, is the only
/// place the guarantee actually holds.
pub const SIDECAR_REVIEWER_CANDIDATES_FILENAME: &str = "reviewer_candidates.json";
pub const SIDECAR_APPLY_JOURNAL_FILENAME: &str = "apply-journal.json";
pub const SIDECAR_FEEDBACK_DIR: &str = "feedback";
/// `candidates.json` / attempt-record schema version.
///
/// Schema 2 = the reviewer-managed queue redesign: `queue` rows with
/// per-entry status + splice hashes, the reviewer-advisory
/// `eligible_now` feed, the `pruned_recent` mirror.
///
/// Schema 3 = the kernel-queue split: the same document gains
/// `kernel_queue`, a fully assignable ranked row list covering every
/// eligible node, and `queue` narrows to the REVIEWER lane. Both
/// directions of the version skew are safe: an old daemon reading a
/// schema-3 export ignores the unknown field and drains `queue` exactly
/// as before, and a new daemon reading a schema-2 export finds no
/// `kernel_queue` and falls back to reviewer-lane-only dispatch.
///
/// The file KEEPS the `candidates.json` path/name (Q4 — one constant,
/// one atomic writer; the name is mildly historical).
pub const SIDECAR_SCHEMA_VERSION: u32 = 3;

pub fn sidecar_dir(runtime_root: &Path) -> PathBuf {
    runtime_root.join(SIDECAR_DIR_NAME)
}

pub fn sidecar_candidates_path(runtime_root: &Path) -> PathBuf {
    sidecar_dir(runtime_root).join(SIDECAR_CANDIDATES_FILENAME)
}

pub fn sidecar_reviewer_candidates_path(runtime_root: &Path) -> PathBuf {
    sidecar_dir(runtime_root).join(SIDECAR_REVIEWER_CANDIDATES_FILENAME)
}

pub fn sidecar_apply_journal_path(runtime_root: &Path) -> PathBuf {
    sidecar_dir(runtime_root).join(SIDECAR_APPLY_JOURNAL_FILENAME)
}

pub fn sidecar_feedback_dir(runtime_root: &Path) -> PathBuf {
    sidecar_dir(runtime_root).join(SIDECAR_FEEDBACK_DIR)
}

/// Publish a kernel disposition back to the attempt that produced the
/// closure.  The dedicated feedback file is the durable handoff to the
/// daemon (and to a later grunt prompt); the best-effort result rewrite makes
/// the common case visible immediately.  Keeping both closes the publish race:
/// a closure can reach the kernel just before the attempt child writes its
/// `result-*.json`, in which case the daemon applies the durable feedback on
/// its next pass.
pub fn publish_grunt_feedback(
    runtime_root: &Path,
    node: &NodeId,
    attempt_id: &str,
    status: &str,
    detail: &str,
    cycle: u32,
) -> Result<PathBuf, String> {
    let feedback_dir = sidecar_feedback_dir(runtime_root);
    std::fs::create_dir_all(&feedback_dir)
        .map_err(|err| format!("sidecar: create {} failed: {err}", feedback_dir.display()))?;
    let feedback_id = {
        let mut hasher = Sha256::new();
        hasher.update(attempt_id.as_bytes());
        hasher.update([0]);
        hasher.update(status.as_bytes());
        hasher.update([0]);
        hasher.update(detail.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    let feedback = serde_json::json!({
        "schema": 1,
        "feedback_id": feedback_id,
        "attempt_id": attempt_id,
        "node": node.as_str(),
        "status": status,
        "detail": detail.chars().take(500).collect::<String>(),
        "cycle": cycle,
    });
    let path = feedback_dir.join(format!("feedback-{feedback_id}.json"));
    let tmp = feedback_dir.join(format!("feedback-{feedback_id}.json.tmp"));
    let bytes = serde_json::to_vec_pretty(&feedback)
        .map_err(|err| format!("sidecar: serialize grunt feedback failed: {err}"))?;
    std::fs::write(&tmp, bytes)
        .map_err(|err| format!("sidecar: write {} failed: {err}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|err| format!("sidecar: rename {} failed: {err}", tmp.display()))?;

    // Best effort only: absence means the attempt child has not emitted its
    // result yet.  The durable feedback above remains for daemon delivery.
    let grunts = sidecar_dir(runtime_root).join("grunts");
    if let Ok(entries) = std::fs::read_dir(&grunts) {
        let expected = format!("result-{attempt_id}.json");
        if Path::new(&expected).components().count() != 1 {
            return Err("sidecar: feedback attempt_id is not a filename component".to_string());
        }
        for entry in entries.flatten() {
            let result_path = entry.path().join(&expected);
            let Ok(text) = std::fs::read_to_string(&result_path) else {
                continue;
            };
            let Ok(mut result) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let Some(map) = result.as_object_mut() else {
                continue;
            };
            let already_delivered = map
                .get("kernel_feedback")
                .and_then(|value| value.as_array())
                .is_some_and(|rows| {
                    rows.iter().any(|row| {
                        row.get("feedback_id").and_then(|value| value.as_str())
                            == feedback.get("feedback_id").and_then(|value| value.as_str())
                    })
                });
            if already_delivered {
                continue;
            }
            if !map.contains_key("grunt_outcome") {
                map.insert(
                    "grunt_outcome".to_string(),
                    serde_json::json!({
                        "status": map.get("status").cloned().unwrap_or_default(),
                        "detail": map.get("detail").cloned().unwrap_or_default(),
                    }),
                );
            }
            map.insert("node".to_string(), serde_json::json!(node.as_str()));
            map.insert("status".to_string(), serde_json::json!(status));
            map.insert("detail".to_string(), serde_json::json!(detail));
            map.entry("kernel_feedback".to_string())
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .expect("kernel_feedback was just initialized as an array")
                .push(feedback.clone());
            let result_tmp = result_path.with_extension("json.kernel-feedback.tmp");
            if let Ok(bytes) = serde_json::to_vec_pretty(&result) {
                if std::fs::write(&result_tmp, bytes).is_ok() {
                    if std::fs::rename(&result_tmp, &result_path).is_ok() {
                        let log_path = entry.path().join(format!("attempt-{attempt_id}.log"));
                        if let Ok(mut log) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(log_path)
                        {
                            use std::io::Write as _;
                            let _ = writeln!(
                                log,
                                "sidecar[{attempt_id}]: kernel {status}: {detail} [{feedback_id}]"
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(path)
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
    /// before each stage; trip ⇒ restore + infrastructure retry).
    pub apply_budget_seconds: u64,
    /// Max spooled attempts claimed per boundary (settled: 1).
    pub max_applies_per_boundary: u32,
    /// Max spooled attempt OUTCOMES (spent generations) claimed per
    /// boundary. Outcome ingest is pure state bookkeeping — no checker,
    /// no disk write beyond the spool moves — so the bound is only
    /// there to keep one boundary's work finite; the remainder waits in
    /// `outcomes/` for the next boundary.
    pub max_outcomes_per_boundary: u32,
    /// Phase controls (§3.3 `phases`). Proof formalization defaults on;
    /// `stating_after_coverage` defaults false (stating is open throughout)
    /// and restores the original coverage gate when true.
    pub phases_proof_formalization: bool,
    pub phases_stating_after_coverage: bool,
    /// Kernel-queue refill (`sidecar.auto_dispatch.enabled`, default
    /// true): at every boundary the hook tops the queue's KERNEL lane
    /// back up to a full ranked list of every eligible node, so the
    /// pool's utilisation stops being a function of how often
    /// boundaries happen. The operator's off switch is a config edit,
    /// never a binary swap (the `phases` precedent).
    pub auto_dispatch_enabled: bool,
    /// Safety bound on resident KERNEL-lane entries
    /// (`sidecar.auto_dispatch.kernel_queue_max`, default 512). NOT a
    /// scheduling parameter: it exists so a pathological tablet cannot
    /// grow the queue without limit, and the default is chosen well
    /// above any real run's eligible population (~108 on a live run) so
    /// that in practice the lane IS the complete list.
    pub auto_dispatch_kernel_queue_max: usize,
    /// Per-node attempt ceiling (`sidecar.auto_dispatch.max_attempts`;
    /// legacy spelling `max_attempts_per_node` still accepted, the new
    /// key wins when both are present). `0` = unlimited, but UNLIKE
    /// `max_nl_proof_chars` the DEFAULT is
    /// `DEFAULT_AUTO_DISPATCH_MAX_ATTEMPTS` (3), not off: no closure the
    /// arm has ever made landed above effective attempt index 2, over 723
    /// attempts spanning the uncapped era and the first capped one. The
    /// first measurement set the ceiling at 5, which deleted a tail of 230
    /// tries for 0 closures; re-measured after that cap had been running,
    /// indices 3–5 were a further 33% of attempts and 30% of spend for 0
    /// closures. 3 keeps one attempt of margin past every success on
    /// record, so it is on unless the operator turns it off.
    ///
    /// KERNEL auto-dispatch lane only — the reviewer lane is never
    /// filtered by it. A SUPPLY gate on new mints, never an evictor:
    /// entries already resident keep their `entry_seq` and in-flight
    /// attempts are untouched. Attempts are counted per CONTENT: only
    /// recorded attempts whose `node_file_sha256` matches the node's
    /// current file count toward the cap (a repaired node is a new
    /// problem), with hash-less legacy rows always counting — see
    /// `collect_auto_dispatch_candidates`. Starvation of OTHER nodes is
    /// already impossible — attempt count is the primary sort key —
    /// so this knob is purely "when to stop paying".
    pub auto_dispatch_max_attempts: u32,
    /// Longest NL proof (`Tablet/<node>.tex` chars) the pool will be
    /// offered (`sidecar.auto_dispatch.max_nl_proof_chars`, default 0 =
    /// off). A SUPPLY filter, not a ranking hint.
    ///
    /// Measured on a live run: the grunt model's lifetime close rate by
    /// prose length is 46% under 1.5k chars, 35% to 3k, 8% to 5k, and
    /// 0 of 113 above 5k. With the small-node backlog exhausted, the
    /// ranking kept handing out 15k-33k-char nodes and the pool burned
    /// ~1.4M tokens per attempt for 40 consecutive failures. Ordering
    /// alone cannot express "not worth attempting at all"; shortest-first
    /// still dispatches the shortest of a hopeless population.
    pub auto_dispatch_max_nl_proof_chars: usize,
}

impl Default for SidecarRuntimeConfig {
    fn default() -> Self {
        SidecarRuntimeConfig {
            enabled: true,
            // Drain rate. These defaults were 300/1, which starved the
            // queue by construction: the SAME boundary hook refills the
            // grunt lane (`auto_dispatch_kernel_queue_max`, 512) and then
            // drains at most one closure, so a pool of grunts grows the
            // backlog by several per cycle and removes one. Claims are
            // oldest-first (`next_pending_attempt`), and a claim the gates
            // reject as `stale_content` still SPENDS its slot — so on a
            // long run the backlog outruns the drain and most completed
            // grunt work never lands. Observed live on 2026-08-27: 9 valid
            // closures queued in ~20 minutes against 1 applied per ~45
            // minute cycle.
            //
            // Leaving a closure unapplied is not merely slow, it is
            // wasteful downstream: an applied closure closes the node, and
            // a closed node does not draw the NL soundness work that an
            // open one does. The queue is the wrong place for a finished
            // proof to sit.
            //
            // `budget_seconds` is a WALL, not a target: a slot will not
            // START with less than `APPLY_SLOT_FLOOR` (240s) remaining, so
            // the budget bounds boundary latency rather than reserving it.
            // Measured apply cost on a real tablet node is ~20s (gate 7
            // compile + gate 8 local-closure probe), so ten applies
            // typically cost ~200s and the wall is only reached when
            // individual applies are pathologically slow — exactly when
            // stopping early is right.
            apply_budget_seconds: 2700,
            max_applies_per_boundary: 10,
            max_outcomes_per_boundary: 16,
            phases_proof_formalization: true,
            phases_stating_after_coverage: false,
            auto_dispatch_enabled: true,
            auto_dispatch_kernel_queue_max: DEFAULT_KERNEL_QUEUE_MAX,
            auto_dispatch_max_attempts: DEFAULT_AUTO_DISPATCH_MAX_ATTEMPTS,
            auto_dispatch_max_nl_proof_chars: 0,
        }
    }
}

/// Default for `SidecarRuntimeConfig::auto_dispatch_kernel_queue_max`.
pub const DEFAULT_KERNEL_QUEUE_MAX: usize = 512;

/// Default for `SidecarRuntimeConfig::auto_dispatch_max_attempts` —
/// see that field's doc for the measurement behind 3.
pub const DEFAULT_AUTO_DISPATCH_MAX_ATTEMPTS: u32 = 3;

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
            cfg.max_applies_per_boundary =
                u32::try_from(v.as_u64().unwrap_or(u64::MAX)).map_err(|_| {
                    format!(
                        "config {}: `sidecar.apply.max_applies_per_boundary` out of range",
                        config_path.display()
                    )
                })?;
        }
        if let Some(v) = apply.get("max_outcomes_per_boundary") {
            cfg.max_outcomes_per_boundary =
                u32::try_from(v.as_u64().unwrap_or(u64::MAX)).map_err(|_| {
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
    if let Some(auto) = obj.get("auto_dispatch") {
        let auto = auto.as_object().ok_or_else(|| {
            format!(
                "config {}: `sidecar.auto_dispatch` must be a JSON object",
                config_path.display()
            )
        })?;
        if let Some(v) = auto.get("enabled") {
            cfg.auto_dispatch_enabled = v.as_bool().ok_or_else(|| {
                format!(
                    "config {}: `sidecar.auto_dispatch.enabled` must be a boolean",
                    config_path.display()
                )
            })?;
        }
        if let Some(v) = auto.get("kernel_queue_max") {
            let raw = v.as_u64().ok_or_else(|| {
                format!(
                    "config {}: `sidecar.auto_dispatch.kernel_queue_max` must be a \
                     non-negative integer",
                    config_path.display()
                )
            })?;
            cfg.auto_dispatch_kernel_queue_max = usize::try_from(raw).map_err(|_| {
                format!(
                    "config {}: `sidecar.auto_dispatch.kernel_queue_max` out of range",
                    config_path.display()
                )
            })?;
        }
        if let Some(v) = auto.get("max_nl_proof_chars") {
            let raw = v.as_u64().ok_or_else(|| {
                format!(
                    "config {}: `sidecar.auto_dispatch.max_nl_proof_chars` must be a \
                     non-negative integer",
                    config_path.display()
                )
            })?;
            cfg.auto_dispatch_max_nl_proof_chars = usize::try_from(raw).map_err(|_| {
                format!(
                    "config {}: `sidecar.auto_dispatch.max_nl_proof_chars` out of range",
                    config_path.display()
                )
            })?;
        }
        // `max_attempts` (canonical) / `max_attempts_per_node` (legacy
        // spelling of the same knob): 0 = unlimited; absent = the
        // default ceiling of `DEFAULT_AUTO_DISPATCH_MAX_ATTEMPTS`. The
        // canonical key wins when both are present.
        for key in ["max_attempts_per_node", "max_attempts"] {
            if let Some(v) = auto.get(key) {
                cfg.auto_dispatch_max_attempts = u32::try_from(v.as_u64().unwrap_or(u64::MAX))
                    .map_err(|_| {
                        format!(
                            "config {}: `sidecar.auto_dispatch.{key}` \
                             must be a non-negative integer in range",
                            config_path.display()
                        )
                    })?;
            }
        }
    }
    if !cfg.enabled {
        return Ok(None);
    }
    Ok(Some(cfg))
}

/// Config-aware window. The engine's apply gate uses the permissive
/// state-only predicate (config is not replay state); this runtime form may
/// narrow stating back to the original after-coverage window.
pub fn sidecar_window_open_with_config(state: &ProtocolState, cfg: &SidecarRuntimeConfig) -> bool {
    use crate::model::Phase;
    match state.phase {
        Phase::ProofFormalization => {
            cfg.phases_proof_formalization && state.sidecar_window_open(&state.live)
        }
        Phase::TheoremStating | Phase::RevisionStating => {
            state.sidecar_window_open(&state.live)
                && (!cfg.phases_stating_after_coverage
                    || !state.orphan_construction_window_open(&state.live))
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
    /// The REVIEWER lane (schema 2 shape, schema 3 scope): per-entry
    /// status + the splice hashes, in reviewer submission order. This
    /// is the only lane the reviewer is shown and the only one it
    /// manages.
    #[serde(default)]
    pub queue: Vec<SidecarQueueExportRow>,
    /// The KERNEL lane (schema 3): the same fully assignable row shape,
    /// covering every eligible node the kernel holds a generation for,
    /// in the boundary's MINT-BATCH dispatch ranking, FIFO-rotated
    /// thereafter — see the ordering note in `build_candidates_export`
    /// (rank: fewest prior attempts, then
    /// non-sketch, then shortest NL proof, then node id).
    ///
    /// The daemon walks `queue` first and falls back to this; the
    /// reviewer never sees it (redacted out of
    /// `reviewer_candidates.json`). Skip-when-empty so a run with the
    /// refill disabled produces a byte-identical schema-2-shaped
    /// document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kernel_queue: Vec<SidecarQueueExportRow>,
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
    attempt_counts: &std::collections::BTreeMap<NodeId, u32>,
) -> SidecarCandidatesExport {
    let window_open = sidecar_window_open_with_config(state, cfg);
    let mut queue = Vec::new();
    let mut kernel_queue = Vec::new();
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
        let row = SidecarQueueExportRow {
            node: node.clone(),
            entry_seq: entry.entry_seq,
            queued_at_cycle: entry.queued_at_cycle,
            status,
            node_file_sha256,
            statement_prefix_sha256,
        };
        if entry.origin.is_kernel() {
            kernel_queue.push(row);
        } else {
            queue.push(row);
        }
    }
    // RE-RANK THE KERNEL LANE, every boundary, by the operator's stated
    // priority — fewest prior grunt attempts, then non-sketch before
    // sketch, then shortest NL proof, then node id. Same
    // `AutoDispatchCandidate::sort_key` the refill ranks its mint batch
    // with, so one definition of "priority" serves both.
    //
    // Walking `state.sidecar_queue` in insertion order gave the rank only
    // over each boundary's mint batch, leaving the lane FIFO thereafter:
    // entries are RESIDENT, so once every eligible node is queued nothing
    // new is minted and the order froze. Clearing `attempted.json` to
    // "start the ranking fresh" then had no visible effect at all, which
    // is not what the queue is for.
    //
    // ORDER ONLY. This reorders EXPORT ROWS; it does not touch
    // `state.sidecar_queue` membership, `entry_seq`, or `queued_at_cycle`.
    // The daemon cancels an attempt when its `(node, entry_seq)` key
    // LEAVES the export (`_cancel_removed`), and every key is still
    // present after a permutation — so re-ranking can never cancel an
    // in-flight attempt. Re-minting the state queue would have.
    //
    // The reviewer lane is deliberately NOT sorted: its order IS the
    // reviewer's expressed priority, worked front to back.
    //
    // Cost: one sort plus a `.tex` read per kernel-lane row (~108 on the
    // live run, ~10 KB each, page-cache warm) on the boundary path.
    let kernel_queue = {
        let ranked = order_auto_dispatch_candidates(
            kernel_queue
                .iter()
                .map(|row| AutoDispatchCandidate {
                    attempts: attempt_counts.get(&row.node).copied().unwrap_or(0),
                    sketch: state.live.sketch_proof_nodes.contains(&row.node),
                    proof_length: nl_proof_length(repo_path, &row.node),
                    node: row.node.clone(),
                })
                .collect(),
        );
        let mut by_node: std::collections::BTreeMap<&NodeId, SidecarQueueExportRow> = kernel_queue
            .iter()
            .map(|row| (&row.node, row.clone()))
            .collect();
        ranked
            .iter()
            .filter_map(|candidate| by_node.remove(&candidate.node))
            .collect::<Vec<_>>()
    };

    // Advisory add-candidates: eligible now, not already in the
    // REVIEWER lane; sorted tier-then-name so the bridge's
    // head-truncation keeps the strongest sound-lane candidates inline
    // (amendment A6).
    //
    // Reviewer-lane membership, NOT `sidecar_queue_contains`: the
    // kernel lane holds nearly every eligible node, so filtering on
    // whole-queue membership would empty this feed and leave the
    // reviewer with nothing it is allowed to add — the one surface it
    // has for expressing priority.
    let mut eligible_now: Vec<SidecarEligibleNowRow> = if window_open {
        sidecar_candidate_nodes(state)
            .into_iter()
            .filter(|(node, _)| !state.sidecar_queue_reviewer_contains(node))
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
        kernel_queue,
        eligible_now,
        // REVIEWER-lane prunes only. The kernel lane retires and
        // re-mints generations every boundary by design; letting that
        // churn into the bounded 32-row log would evict the reviewer's
        // own "where did my entry go" answers within a cycle.
        pruned_recent: state
            .sidecar_queue_prune_log
            .iter()
            .filter(|row| row.origin.is_reviewer())
            .cloned()
            .collect(),
        recent_closures,
    }
}

/// The reviewer's redacted view: everything except the kernel lane.
///
/// Consumed by `write_reviewer_candidates_export`; separated so the
/// redaction is one named, testable function rather than a field the
/// writer happens to skip.
pub fn redact_export_for_reviewer(export: &SidecarCandidatesExport) -> SidecarCandidatesExport {
    SidecarCandidatesExport {
        kernel_queue: Vec::new(),
        ..export.clone()
    }
}

/// Write the export via tmp-file + rename in the same directory
/// (atomic on one filesystem). Creates `<runtime>/sidecar/` — callers
/// must gate on the config block being present (inertness contract).
pub fn write_candidates_export(
    runtime_root: &Path,
    export: &SidecarCandidatesExport,
) -> Result<(), String> {
    write_export_to(runtime_root, SIDECAR_CANDIDATES_FILENAME, export)
}

/// Write the reviewer's redacted copy (`reviewer_candidates.json`) —
/// the file the reviewer sandbox ro-binds and the reviewer prompt
/// block renders from.
///
/// Written AFTER `candidates.json` deliberately. The daemon's
/// staleness clock reads `candidates.json`'s mtime, so the daemon's
/// file must never be the older of the two; the reviewer's copy lagging
/// by microseconds costs nothing, because a reviewer burst is minutes
/// away from any boundary.
pub fn write_reviewer_candidates_export(
    runtime_root: &Path,
    export: &SidecarCandidatesExport,
) -> Result<(), String> {
    write_export_to(
        runtime_root,
        SIDECAR_REVIEWER_CANDIDATES_FILENAME,
        &redact_export_for_reviewer(export),
    )
}

fn write_export_to(
    runtime_root: &Path,
    filename: &str,
    export: &SidecarCandidatesExport,
) -> Result<(), String> {
    let dir = sidecar_dir(runtime_root);
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("sidecar: create {} failed: {err}", dir.display()))?;
    let final_path = dir.join(filename);
    let tmp_path = dir.join(format!("{filename}.tmp"));
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
// Kernel-queue refill (the pool's standing supply of ranked work)
// ====================================================================
//
// MEASURED PROBLEM. The grunt pool ran at 5.1% utilisation over 13.2 h
// (50 attempts, 2.7 of 52.9 available grunt-hours, twelve idle gaps
// over 20 minutes, one of 150). Median attempt: 169 s. The bottleneck
// was never throughput — it was REFILL. The reviewer queue holds ~4
// entries, the pool has 3-4 slots, and each entry is worth exactly one
// attempt (`(node, entry_seq)` dedupe), so the pool drains a whole
// cycle's supply in ~3 minutes and then idles until the next boundary,
// 20 to 150 minutes later. Sizing the top-up to the instant (the old
// `idle - 1` rule) cannot fix that: it refills to the pool's width, and
// the pool empties again in one attempt-length.
//
// THE FIX. Keep a full ranked list of every eligible node RESIDENT in
// the queue's kernel lane, so the supply outlives the boundary that
// produced it. Utilisation then depends on how fast grunts work, not on
// how often the supervisor reaches a boundary.
//
// GENERATION MINTING. Entries persist across boundaries, so a boundary
// mints generations only for nodes the lane does not already hold —
// in steady state, roughly the handful spent since the previous
// boundary, not ~108 every time. This is what keeps the daemon's
// one-attempt-per-`(node, entry_seq)` dedupe meaningful: a resident
// entry keeps its generation until that generation is SPENT, and the
// spend is what retires it (`apply_sidecar_attempt_outcomes`).
//
// STARVATION. Rank is `AutoDispatchCandidate::sort_key`, whose primary
// key is prior attempt count. A node that fails is re-minted a cycle
// later carrying attempts+1, which sorts it below every node tried less
// often — so the lane rotates through the whole eligible population
// before it comes back to anything. `spent_this_cycle` additionally
// forbids re-minting a generation at the very boundary that retired it,
// which bounds the degenerate one-eligible-node case to one attempt per
// cycle. On top of the rotation sits a hard stop,
// `auto_dispatch.max_attempts` (default 3, 0 = unlimited), counted per
// node CONTENT so a repaired node becomes eligible again.
//
// ONE daemon-owned file feeds the decision now:
//   * `attempted.json` — the daemon's durable per-node attempt history,
//     the primary ranking key. Unreadable ⇒ the boundary refills
//     nothing, because substituting zeros would turn "spread attempts
//     around" into "hammer whatever sorts first".
//
// The `status.json` heartbeat is deliberately NO LONGER consulted. It
// was load-bearing only while the top-up was sized off `idle - 1`,
// where a dead daemon's fossil `in_flight` could size a dispatch off a
// reading of a pool that no longer exists. A refill to a fixed lane
// depth reads nothing about the pool, so there is nothing for a fossil
// to corrupt: while the daemon is down the lane simply sits full, and
// the pool drains it when it comes back.

pub const SIDECAR_ATTEMPTED_FILENAME: &str = "attempted.json";

pub fn sidecar_attempted_path(runtime_root: &Path) -> PathBuf {
    sidecar_dir(runtime_root).join(SIDECAR_ATTEMPTED_FILENAME)
}

/// One ranked auto-dispatch candidate. Built from state (which nodes
/// are eligible) plus two disk reads (the daemon's attempt history, the
/// node's `.tex` proof block).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoDispatchCandidate {
    pub node: NodeId,
    /// Prior grunt attempts recorded against the node, ALL generations
    /// (the daemon's `attempted.json` history, not the 5-row-per-node
    /// window `status.json` mirrors).
    pub attempts: u32,
    /// The node's NL proof is a `SKETCH:` placeholder.
    pub sketch: bool,
    /// NL proof length; `None` when it cannot be measured (no `.tex`,
    /// no `\begin{proof}` block).
    pub proof_length: Option<usize>,
}

impl AutoDispatchCandidate {
    /// Total order, in the owner's stated priority:
    ///   1. fewest prior grunt attempts;
    ///   2. non-sketch before sketch — a SKETCH proof counts as LONGER
    ///      than every non-sketch proof, so sketches sort last among
    ///      otherwise-equal candidates;
    ///   3. shortest NL proof, with an unmeasurable proof sorting after
    ///      every measured one (an absent proof block is not evidence
    ///      of a short proof);
    ///   4. node id — the deterministic final tie-break, so the same
    ///      state always produces the same dispatch.
    fn sort_key(&self) -> (u32, bool, usize, &str) {
        (
            self.attempts,
            self.sketch,
            self.proof_length.unwrap_or(usize::MAX),
            self.node.as_str(),
        )
    }
}

/// Order candidates by the priority above (pure).
pub fn order_auto_dispatch_candidates(
    mut candidates: Vec<AutoDispatchCandidate>,
) -> Vec<AutoDispatchCandidate> {
    candidates.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    candidates
}

/// The whole refill decision, pure: which nodes this boundary adds to
/// the kernel lane, in rank order.
///
/// `resident` is how many kernel-lane entries the queue already holds;
/// `cap` is `auto_dispatch_kernel_queue_max`. `candidates` are the
/// nodes NOT currently queued in either lane (`collect_auto_dispatch_
/// candidates` applies that filter, plus the `spent_this_cycle`
/// deferral).
///
/// Re-entrancy: the boundary hook can run several times per boundary
/// (the human-gate poll loop re-enters it). The second pass sees the
/// first pass's adds as `resident` and as queue members, so it plans
/// nothing — the headroom subtraction IS the churn guard that the old
/// queue-empty trigger used to be.
pub fn plan_kernel_queue_refill(
    resident: usize,
    cap: usize,
    candidates: Vec<AutoDispatchCandidate>,
) -> Vec<NodeId> {
    let headroom = cap.saturating_sub(resident);
    if headroom == 0 {
        return Vec::new();
    }
    order_auto_dispatch_candidates(candidates)
        .into_iter()
        .take(headroom)
        .map(|candidate| candidate.node)
        .collect()
}

/// Per-node prior-attempt counts from the daemon's `attempted.json`, less
/// kernel-declared infrastructure/authority discards.
///
/// `Ok(empty)` when the file is ABSENT — the honest reading of "no
/// attempt has been recorded yet" (a fresh run, or a post-rewind wipe:
/// the daemon deletes the file when it rewinds, which is exactly when
/// the counts should restart).
/// `Err` when the file exists but cannot be read or parsed: the counts
/// are the PRIMARY sort key, and silently substituting zeros would turn
/// "spread attempts around" into "hammer whatever sorts first", so the
/// boundary skips auto-dispatch instead.
pub fn read_attempt_counts(
    runtime_root: &Path,
) -> Result<std::collections::BTreeMap<NodeId, u32>, String> {
    Ok(read_attempt_history(runtime_root)?
        .into_iter()
        .filter_map(|(node, rows)| u32::try_from(rows.len()).ok().map(|count| (node, count)))
        .collect())
}

/// Per-node, per-attempt recorded content hashes from the daemon's
/// `attempted.json` — the same chargeable rows `read_attempt_counts` counts,
/// kept individually so the attempt cap can count per CONTENT. Raw rows stay
/// in `attempted.json` for generation dedupe and forensics; the daemon marks a
/// row `chargeable: false` after consuming a kernel-owned
/// `feedback/rejected/not_queued` disposition. The exemption affects only
/// dispatch ranking and the mathematical-attempt ceiling.
///
/// Each row yields the `node_file_sha256` the daemon stamped at
/// assignment time, or `""` for rows that predate the stamping (or
/// whose assignment row carried no hash). Absence semantics match
/// `read_attempt_counts`: absent file ⇒ empty map, unreadable ⇒ `Err`.
pub fn read_attempt_history(
    runtime_root: &Path,
) -> Result<std::collections::BTreeMap<NodeId, Vec<String>>, String> {
    let path = sidecar_attempted_path(runtime_root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(std::collections::BTreeMap::new())
        }
        Err(err) => return Err(format!("sidecar: read {} failed: {err}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| format!("sidecar: parse {} failed: {err}", path.display()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| format!("sidecar: {} is not a JSON object", path.display()))?;
    let mut history = std::collections::BTreeMap::new();
    for (node, rows) in obj {
        let hashes: Vec<String> = rows
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|row| {
                        row.get("chargeable").and_then(|v| v.as_bool()) != Some(false)
                    })
                    .map(|row| {
                        row.get("node_file_sha256")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default();
        history.insert(NodeId::from(node.as_str()), hashes);
    }
    Ok(history)
}

/// NL proof length for the ordering, measured over the node's
/// `Tablet/<node>.tex` `\begin{proof}` … `\end{proof}` block (the same
/// block `tex_proof_starts_with_sketch_marker` reads — one extractor,
/// not a second parser).
///
/// "Length" = NON-WHITESPACE CHARACTERS. Bytes would make the order
/// depend on line wrapping and indentation, so a reflow of an untouched
/// proof could reshuffle the queue; counting non-whitespace characters
/// is stable under reflow while staying a pure function of the text.
/// `None` (unmeasurable) when the `.tex` is absent/unreadable or has no
/// proof block.
pub fn nl_proof_length(repo_path: &Path, node: &NodeId) -> Option<usize> {
    let path = repo_path
        .join("Tablet")
        .join(format!("{}.tex", node.as_str()));
    let text = std::fs::read_to_string(path).ok()?;
    let block = crate::runtime_cli_observations::extract_tex_proof_block(&text);
    if block.is_empty() {
        return None;
    }
    Some(block.chars().filter(|c| !c.is_whitespace()).count())
}

/// Assemble the ranked candidate list for a boundary: every node that
/// is sidecar-eligible RIGHT NOW and not already queued, annotated with
/// its attempt count, sketch flag, and proof length.
///
/// Eligibility is `sidecar_candidate_nodes` — the single
/// `sidecar_eligible` predicate the export and the apply gate use. The
/// auto-dispatcher deliberately does not own a second definition of
/// "eligible": if a node cannot be applied it must not be queued.
///
/// One scheduling filter sits on top of eligibility: a node whose
/// generation was declared SPENT during the current cycle is held back
/// until the next one (`spent_this_cycle`). Without it the degenerate
/// case bites immediately — when the spent entry was the last one in
/// the queue and its node is the only eligible candidate, the very same
/// boundary that retires the generation mints a fresh one and the pool
/// re-attempts the node it just failed, back to back, forever. The
/// attempt-count ordering cannot break that tie because there is
/// nothing to rotate to. Deferring by a cycle bounds a hopeless node to
/// one auto-attempt per cycle (the rate the reviewer could queue it by
/// hand) and gives the reviewer a turn in between.
pub fn collect_auto_dispatch_candidates(
    repo_path: &Path,
    state: &ProtocolState,
    attempt_history: &std::collections::BTreeMap<NodeId, Vec<String>>,
    max_attempts: u32,
    max_nl_proof_chars: usize,
) -> AutoDispatchSupply {
    let spent_this_cycle: std::collections::BTreeSet<&NodeId> = state
        .sidecar_queue_prune_log
        .iter()
        .filter(|row| row.cycle == state.cycle && row.reason.starts_with("attempt_spent:"))
        .map(|row| &row.node)
        .collect();
    // Attempt ceiling (default 3, 0 = unlimited): stop refilling a node
    // the pool has already failed this many times. Counted per CONTENT:
    // only recorded attempts whose stamped `node_file_sha256` matches
    // the node's current file count, so a statement/file repair resets
    // the allowance — a repaired node is a new problem. Two deliberate
    // conservatisms, both in the "spend less" direction:
    //   * hash-less rows (attempts recorded before the daemon stamped
    //     hashes, or whose assignment row carried none) ALWAYS count —
    //     a node ground down under the old records stays capped until
    //     the reviewer queues it by hand or a rewind wipes
    //     `attempted.json`;
    //   * an unreadable node file counts every row (it could not be
    //     attempted anyway — it would export `blocked:unsplittable`).
    // Cancelled attempts count too, ON PURPOSE: a cancel still spent a
    // generation (`record_attempted(publish=True)`), and the cap
    // budgets generations, not verdicts — do not "fix" that.
    let capped_by_attempts = |node: &NodeId| -> bool {
        if max_attempts == 0 {
            return false;
        }
        let Some(rows) = attempt_history.get(node) else {
            return false;
        };
        if rows.len() < max_attempts as usize {
            // Cheap short-circuit: even if every row counted, the node
            // is under the ceiling — skip the file read.
            return false;
        }
        let current = filespec_split::read_node_file(repo_path, node.as_str())
            .ok()
            .map(|(_, sha)| sha);
        let effective = rows
            .iter()
            .filter(|hash| {
                hash.is_empty() || current.is_none() || current.as_deref() == Some(hash.as_str())
            })
            .count();
        effective >= max_attempts as usize
    };
    let mut capped = Vec::new();
    let candidates = sidecar_candidate_nodes(state)
        .into_iter()
        .map(|(node, _tier)| node)
        .filter(|node| !state.sidecar_queue_contains(node))
        .filter(|node| !spent_this_cycle.contains(node))
        .filter(|node| {
            if capped_by_attempts(node) {
                capped.push(node.clone());
                return false;
            }
            true
        })
        .map(|node| AutoDispatchCandidate {
            attempts: u32::try_from(attempt_history.get(&node).map_or(0, Vec::len))
                .unwrap_or(u32::MAX),
            // `sidecar_eligible` already excludes sketch nodes, so this
            // is FALSE for every candidate that reaches here today. It
            // is computed anyway: the ordering rule is specified
            // independently of the eligibility rule, and if eligibility
            // ever admits sketches the order is already right.
            sketch: state.live.sketch_proof_nodes.contains(&node),
            proof_length: nl_proof_length(repo_path, &node),
            node,
        })
        // SUPPLY cutoff (0 = off). Applied AFTER construction because it
        // reads `proof_length`, computed there. Ordering is not a substitute:
        // shortest-first still dispatches the shortest node of a population
        // where nothing is closable.
        .filter(|c| {
            max_nl_proof_chars == 0 || c.proof_length.map_or(true, |n| n <= max_nl_proof_chars)
        })
        .collect();
    AutoDispatchSupply { candidates, capped }
}

/// What one boundary's supply pass produced: the assignable candidates,
/// plus the nodes the attempt cap withheld (a LOG feed for the caller —
/// the cap is a supply gate on new mints, never a queue mutation, so
/// `capped` drives one observability line and nothing else).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoDispatchSupply {
    pub candidates: Vec<AutoDispatchCandidate>,
    pub capped: Vec<NodeId>,
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
    /// Infrastructure retry counter (diagnostic only; never an attempt cap).
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
    let mut value = read_attempt_value(src)
        .unwrap_or_else(|err| serde_json::json!({ "malformed": true, "error": err }));
    edit(&mut value);
    std::fs::create_dir_all(dest_dir)
        .map_err(|err| format!("sidecar: create {} failed: {err}", dest_dir.display()))?;
    let file_name = src
        .file_name()
        .ok_or_else(|| "sidecar: attempt path has no file name".to_string())?;
    let dest = dest_dir.join(file_name);
    let tmp = dest_dir.join(format!("{}.tmp", file_name.to_string_lossy()));
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
    /// Moved back to `pending/` with `deferrals` incremented. There is no
    /// terminal infrastructure ceiling: availability is not a mathematical
    /// attempt outcome and cannot consume the generation.
    Deferred { deferrals: u32 },
}

/// Return an infrastructure-blocked closure to `pending/` and retain it until
/// it can be applied. `deferrals` is diagnostic only, not an attempt budget.
pub fn defer_attempt(
    claimed_path: &Path,
    spool: &SidecarSpool,
    cycle: u32,
    context: &str,
) -> Result<DeferOutcome, String> {
    let value = read_attempt_value(claimed_path).unwrap_or_default();
    let prior_deferrals = value.get("deferrals").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let new_deferrals = prior_deferrals.saturating_add(1);
    move_attempt_with_edit(claimed_path, &spool.pending, |value| {
        if let Some(map) = value.as_object_mut() {
            map.insert("deferrals".to_string(), serde_json::json!(new_deferrals));
            map.insert(
                "last_infrastructure_retry".to_string(),
                serde_json::json!({ "cycle": cycle, "context": context }),
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
/// within the same boundary). Sweep each back through the infrastructure-retry
/// mechanics. No-op (and no dir creation) when the spool
/// doesn't exist.
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
pub fn return_outcome_to_lane(
    claimed_path: &Path,
    spool: &SidecarSpool,
) -> Result<PathBuf, String> {
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
    std::fs::rename(&tmp, &path).map_err(|err| format!("sidecar: rename journal failed: {err}"))?;
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
    RestoredDirtyFile {
        restored_sha_matches_pre_image: bool,
    },
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
        if let (Ok(bytes), Some(name)) = (std::fs::read(&src), Path::new(&journal.file).file_name())
        {
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
    // Confirmed compiling bypass of gate 6b (2026-08-01 audit): the
    // ANONYMOUS form -- initialize followed by a parenthesised IO action --
    // the declared-name delta has nothing to compare and cannot see it.
    // It runs at IMPORT -- which gate 7's axiom audit performs, and which
    // every later build of a dependent node performs -- inside the compile
    // sandbox. A ban is the only instrument that reaches a nameless
    // command. Zero occurrences across the 626 live node bodies.
    "initialize",
    "builtin_initialize",
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
pub fn splice_proof_body(pre_image: &str, node: &str, proof_body: &str) -> Result<String, String> {
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
    //
    // One exact state-carried exception repairs the lane's own race. A
    // generation pruned THIS cycle as `lane_drift` may already have its
    // completed closure in `pending/` / `claimed/`; the current
    // eligibility and content gates below re-check the now-settled
    // statement. The engine re-asserts this exact generation from the
    // prune row immediately before its unchanged membership gate. An
    // arbitrary unqueued node and a reviewer-removed entry have no such
    // row and still die here.
    if let Some(queue_entry) = state.sidecar_queue_entry(&record.node) {
        // Generation gate (amendment A3): the attempt must have been
        // spawned for the CURRENT queue entry, not a removed-then-re-added
        // predecessor.
        if record.entry_seq != queue_entry.entry_seq {
            return SidecarPreflight::Reject {
                reason: format!(
                    "stale_generation (record entry_seq {} != queued entry_seq {})",
                    record.entry_seq, queue_entry.entry_seq
                ),
            };
        }
    } else if state
        .recoverable_sidecar_lane_drift_prune(&record.node, record.entry_seq)
        .is_none()
    {
        return SidecarPreflight::Reject {
            reason: "not_queued".to_string(),
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
    use crate::model::{CorrStatus, NodeKind, Phase, SubstantivenessStatus};

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
    fn stating_window_defaults_open_and_config_can_restore_coverage_gate() {
        let mut state = eligible_state();
        state.phase = Phase::TheoremStating;
        let target = crate::model::TargetId::from("t");
        state.configured_targets.insert(target.clone());
        assert!(state.orphan_construction_window_open(&state.live));

        let default_cfg = SidecarRuntimeConfig::default();
        assert!(!default_cfg.phases_stating_after_coverage);
        assert!(sidecar_window_open_with_config(&state, &default_cfg));

        let gated_cfg = SidecarRuntimeConfig {
            phases_stating_after_coverage: true,
            ..default_cfg
        };
        assert!(!sidecar_window_open_with_config(&state, &gated_cfg));

        state
            .live
            .coverage
            .insert(target, [NodeId::from("Rung")].into_iter().collect());
        assert!(sidecar_window_open_with_config(&state, &gated_cfg));
    }

    #[test]
    fn apply_defaults_drain_faster_than_one_per_boundary() {
        // Regression: the shipped defaults were 300s / 1 apply, while the
        // same boundary hook refills the grunt lane. One drained per
        // boundary against a lane that refills in bulk means the backlog
        // grows forever and finished proofs rot in `pending/` — and a
        // closure left unapplied keeps its node open, which keeps drawing
        // NL soundness work the closure would have made unnecessary.
        let cfg = SidecarRuntimeConfig::default();
        assert!(
            cfg.max_applies_per_boundary > 1,
            "a default of {} applies per boundary cannot keep up with a lane \
             that refills in bulk",
            cfg.max_applies_per_boundary
        );
        // The budget must actually permit the slots the cap allows, or the
        // cap is decorative: a slot needs APPLY_SLOT_FLOOR (240s) remaining
        // to start, so a budget below that admits exactly one.
        const APPLY_SLOT_FLOOR_SECS: u64 = 240;
        assert!(
            cfg.apply_budget_seconds > APPLY_SLOT_FLOOR_SECS,
            "budget {}s leaves room for a single slot; raising the cap alone \
             changes nothing",
            cfg.apply_budget_seconds
        );
        // At the measured ~20s per apply, the budget must not be the
        // binding constraint before the cap is reached.
        const OBSERVED_APPLY_SECS: u64 = 20;
        let affordable =
            (cfg.apply_budget_seconds - APPLY_SLOT_FLOOR_SECS) / OBSERVED_APPLY_SECS + 1;
        assert!(
            affordable >= u64::from(cfg.max_applies_per_boundary),
            "budget affords only {affordable} applies at {OBSERVED_APPLY_SECS}s each, \
             below the cap of {}",
            cfg.max_applies_per_boundary
        );
    }

    #[test]
    fn config_auto_dispatch_defaults_on_and_takes_an_off_switch() {
        let dir = tempdir();
        // Absent block: idle-pool top-up is ON, so an existing run gets
        // it from the binary swap alone.
        let path = write_config(dir.path(), r#"{"sidecar": {"enabled": true}}"#);
        assert!(
            load_sidecar_runtime_config(&path)
                .expect("parse ok")
                .expect("block present")
                .auto_dispatch_enabled
        );
        let path = write_config(
            dir.path(),
            r#"{"sidecar": {"enabled": true, "auto_dispatch": {"enabled": false}}}"#,
        );
        assert!(
            !load_sidecar_runtime_config(&path)
                .expect("parse ok")
                .expect("block present")
                .auto_dispatch_enabled
        );
        // Operator typo halts, like every other knob in the block.
        let path = write_config(
            dir.path(),
            r#"{"sidecar": {"enabled": true, "auto_dispatch": {"enabled": "no"}}}"#,
        );
        assert!(load_sidecar_runtime_config(&path).is_err());
        let path = write_config(
            dir.path(),
            r#"{"sidecar": {"enabled": true, "auto_dispatch": true}}"#,
        );
        assert!(load_sidecar_runtime_config(&path).is_err());
    }

    #[test]
    fn config_kernel_queue_bounds_default_and_parse() {
        let dir = tempdir();
        let path = write_config(dir.path(), r#"{"sidecar": {"enabled": true}}"#);
        let cfg = load_sidecar_runtime_config(&path)
            .expect("parse ok")
            .expect("block present");
        assert_eq!(cfg.auto_dispatch_kernel_queue_max, DEFAULT_KERNEL_QUEUE_MAX);
        assert_eq!(
            cfg.auto_dispatch_max_attempts, DEFAULT_AUTO_DISPATCH_MAX_ATTEMPTS,
            "the attempt ceiling is ON by default (unlike max_nl_proof_chars)"
        );
        let path = write_config(
            dir.path(),
            r#"{"sidecar": {"enabled": true, "auto_dispatch":
                {"kernel_queue_max": 40, "max_attempts": 3}}}"#,
        );
        let cfg = load_sidecar_runtime_config(&path)
            .expect("parse ok")
            .expect("block present");
        assert_eq!(cfg.auto_dispatch_kernel_queue_max, 40);
        assert_eq!(cfg.auto_dispatch_max_attempts, 3);
        // 0 = unlimited (the shared off convention), and the legacy
        // spelling still lands on the same knob — with the canonical
        // key winning when both are present.
        let path = write_config(
            dir.path(),
            r#"{"sidecar": {"enabled": true, "auto_dispatch": {"max_attempts": 0}}}"#,
        );
        let cfg = load_sidecar_runtime_config(&path)
            .expect("parse ok")
            .expect("block present");
        assert_eq!(cfg.auto_dispatch_max_attempts, 0);
        let path = write_config(
            dir.path(),
            r#"{"sidecar": {"enabled": true, "auto_dispatch":
                {"max_attempts_per_node": 7}}}"#,
        );
        let cfg = load_sidecar_runtime_config(&path)
            .expect("parse ok")
            .expect("block present");
        assert_eq!(cfg.auto_dispatch_max_attempts, 7);
        let path = write_config(
            dir.path(),
            r#"{"sidecar": {"enabled": true, "auto_dispatch":
                {"max_attempts_per_node": 7, "max_attempts": 4}}}"#,
        );
        let cfg = load_sidecar_runtime_config(&path)
            .expect("parse ok")
            .expect("block present");
        assert_eq!(cfg.auto_dispatch_max_attempts, 4);
        // Operator typo halts, like every other knob in the block.
        let path = write_config(
            dir.path(),
            r#"{"sidecar": {"enabled": true, "auto_dispatch": {"kernel_queue_max": "lots"}}}"#,
        );
        assert!(load_sidecar_runtime_config(&path).is_err());
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
        state.substantiveness_status.insert(n.clone(), SubstantivenessStatus::Pass);
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
                origin: crate::model::SidecarQueueOrigin::Reviewer,
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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

    /// The two lanes are separate LISTS over one state queue: same row
    /// shape, same hashes, same status classifier — the only thing that
    /// decides which list a row lands in is `origin`.
    #[test]
    fn export_splits_the_state_queue_into_reviewer_and_kernel_lanes() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = queue_state(&[("Rung", 7)]);
        state.sidecar_queue.push(crate::model::SidecarQueueEntry {
            node: NodeId::from("Rung"),
            entry_seq: 8,
            queued_at_cycle: 4,
            origin: crate::model::SidecarQueueOrigin::Kernel,
        });
        // (A single node cannot really be in both lanes; this state is
        // built by hand purely to compare the two rows field by field.)
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
        assert_eq!(export.schema, 3);
        assert_eq!(export.queue.len(), 1);
        assert_eq!(export.kernel_queue.len(), 1);
        let reviewer = &export.queue[0];
        let kernel = &export.kernel_queue[0];
        assert_eq!((reviewer.entry_seq, kernel.entry_seq), (7, 8));
        assert_eq!(reviewer.status, "ready");
        assert_eq!(kernel.status, "ready");
        assert_eq!(reviewer.node_file_sha256, kernel.node_file_sha256);
        assert_eq!(
            reviewer.statement_prefix_sha256,
            kernel.statement_prefix_sha256
        );
    }

    /// The advisory add-feed filters on REVIEWER-lane membership. The
    /// kernel lane holds nearly every eligible node, so filtering on
    /// whole-queue membership would empty the feed and leave the
    /// reviewer with nothing it is allowed to add.
    #[test]
    fn eligible_now_survives_a_node_the_kernel_lane_holds() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = eligible_state();
        state.sidecar_queue.push(crate::model::SidecarQueueEntry {
            node: NodeId::from("Rung"),
            entry_seq: 1,
            queued_at_cycle: 3,
            origin: crate::model::SidecarQueueOrigin::Kernel,
        });
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
        assert_eq!(export.queue.len(), 0);
        assert_eq!(export.kernel_queue.len(), 1);
        assert_eq!(
            export
                .eligible_now
                .iter()
                .map(|row| row.node.as_str())
                .collect::<Vec<_>>(),
            vec!["Rung"],
        );
        // A REVIEWER-lane entry still removes it from the add feed.
        state.sidecar_queue[0].origin = crate::model::SidecarQueueOrigin::Reviewer;
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
        assert!(export.eligible_now.is_empty());
    }

    /// The kernel lane retires and re-mints generations every boundary.
    /// Letting that churn into the bounded 32-row prune log would evict
    /// the reviewer's own "where did my entry go" answers within a
    /// cycle, so the export shows the reviewer only its own prunes.
    #[test]
    fn pruned_recent_hides_kernel_lane_churn() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        seed_node_file(&repo);
        let mut state = eligible_state();
        for (node, origin) in [
            ("Mine", crate::model::SidecarQueueOrigin::Reviewer),
            ("Theirs", crate::model::SidecarQueueOrigin::Kernel),
        ] {
            state
                .sidecar_queue_prune_log
                .push(crate::model::SidecarQueuePrune {
                    node: NodeId::from(node),
                    entry_seq: 1,
                    cycle: 2,
                    reason: "attempt_spent:failed".to_string(),
                    origin,
                });
        }
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
        assert_eq!(
            export
                .pruned_recent
                .iter()
                .map(|row| row.node.as_str())
                .collect::<Vec<_>>(),
            vec!["Mine"],
        );
    }

    /// The reviewer's copy is the daemon's document minus the kernel
    /// lane — and the redaction happens at the WRITER, because
    /// `reviewer_candidates.json` is what the reviewer sandbox binds.
    #[test]
    fn reviewer_copy_is_the_export_without_the_kernel_lane() {
        let dir = tempdir();
        let repo = dir.path().join("repo");
        let runtime_root = dir.path().join("runtime");
        seed_node_file(&repo);
        let mut state = queue_state(&[("Rung", 7)]);
        state.sidecar_queue.push(crate::model::SidecarQueueEntry {
            node: NodeId::from("Lift"),
            entry_seq: 8,
            queued_at_cycle: 4,
            origin: crate::model::SidecarQueueOrigin::Kernel,
        });
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
        assert_eq!(export.kernel_queue.len(), 1);
        write_candidates_export(&runtime_root, &export).expect("daemon copy");
        write_reviewer_candidates_export(&runtime_root, &export).expect("reviewer copy");

        let daemon_doc: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(sidecar_candidates_path(&runtime_root)).unwrap(),
        )
        .unwrap();
        let reviewer_doc: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(sidecar_reviewer_candidates_path(&runtime_root)).unwrap(),
        )
        .unwrap();
        assert!(daemon_doc.get("kernel_queue").is_some());
        assert!(
            reviewer_doc.get("kernel_queue").is_none(),
            "skip-when-empty must keep the field off the reviewer's copy entirely"
        );
        assert_eq!(daemon_doc["queue"], reviewer_doc["queue"]);
        assert_eq!(daemon_doc["eligible_now"], reviewer_doc["eligible_now"]);
        // And the node names in the kernel lane appear nowhere in it.
        assert!(!reviewer_doc.to_string().contains("Lift"));
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
        assert_eq!(export.queue[0].status, "blocked:active_node");
        // Blocked rows still carry the hashes (the file is fine).
        assert!(!export.queue[0].node_file_sha256.is_empty());

        // Unsplittable: missing node file.
        let state = queue_state(&[("Ghost", 8)]);
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
        state
            .sidecar_queue_prune_log
            .push(crate::model::SidecarQueuePrune {
                node: NodeId::from("Old"),
                entry_seq: 3,
                cycle: 2,
                reason: "closed".to_string(),
                origin: crate::model::SidecarQueueOrigin::Reviewer,
            });
        let cfg = SidecarRuntimeConfig::default();
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
                .insert(n.clone(), SubstantivenessStatus::Pass);
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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
        let export = build_candidates_export(&repo, &state, &cfg, &Default::default());
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

// ====================================================================
// Grunt-path declared-name delta (2026-08-01 audit)
// ====================================================================

/// Every top-level declaration name a Lean source appears to author,
/// collected PERMISSIVELY.
///
/// This exists for ONE caller: the grunt apply path compares the set
/// before and after the body splice and refuses any difference. A grunt
/// supplies a proof body and has no legitimate reason to introduce a new
/// top-level constant, so the honest statement of the rule is "the
/// declared names did not change", not "the body matched an allowlist".
///
/// Being a DELTA is what lets this be aggressive. The failure that
/// killed the text-level fix to `filespec::parse_decl_line` — 77
/// `private` auxiliaries in `GreenEdgesSixEndNormalization` and
/// `PrismJumpK4RootConstruction` becoming visible and rejecting two
/// closed baseline nodes — cannot happen here: those declarations live
/// in the byte-frozen prefix, so they appear on BOTH sides and cancel.
/// Over-detection costs nothing; only UNDER-detection matters. That is
/// the opposite of the tradeoff `parse_decl_line` faces, which is why
/// this is a separate function and not an edit to that one.
///
/// Deliberately NOT in `filespec.rs`: that module's parser is on the
/// shared WORKER acceptance path, where a new rejection halts the run.
/// Nothing here is reachable from a worker burst.
///
/// Known blind spots, stated rather than papered over: a declaration
/// synthesized by a macro (the macro-defining commands are already
/// banned tokens), and an anonymous `instance`, whose name the
/// elaborator invents with no `declId` in source. Closing those needs
/// the elaborated environment, which is gate 8's territory.
pub fn declared_names_permissive(content: &str) -> std::collections::BTreeSet<String> {
    const KEYWORDS: &[&str] = &[
        "theorem",
        "lemma",
        "def",
        "abbrev",
        "example",
        "structure",
        "inductive",
        "class",
        "instance",
        "axiom",
        "opaque",
        "initialize",
        "builtin_initialize",
    ];
    // Modifiers Lean allows before a declaration keyword. Order-free on
    // purpose: we only need to reach the keyword, not validate the form.
    const MODIFIERS: &[&str] = &[
        "private",
        "protected",
        "scoped",
        "local",
        "noncomputable",
        "unsafe",
        "partial",
        "nonrec",
        "meta",
    ];
    let stripped = strip_comments_and_attributes(content);
    let mut names = std::collections::BTreeSet::new();
    let mut namespaces: Vec<String> = Vec::new();
    // A declaration keyword may sit alone on its line with the name on the
    // next — one of the two evasions that needs no modifier at all.
    let mut pending_keyword = false;
    for raw in stripped.lines() {
        let mut line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if pending_keyword {
            pending_keyword = false;
            if let Some(name) = line.split_whitespace().next() {
                if is_plausible_decl_name(name) {
                    names.insert(qualify(&namespaces, name));
                    continue;
                }
            }
        }
        // `set_option X in`, `open X in`, `universe X in`, ... — the
        // wrapper form. Peel every leading `... in` prefix.
        loop {
            let head = line.split_whitespace().next().unwrap_or("");
            if !matches!(
                head,
                "set_option" | "open" | "universe" | "attribute" | "variable"
            ) {
                break;
            }
            match line.find(" in ") {
                Some(idx) => line = line[idx + 4..].trim_start(),
                None => break,
            }
        }
        let mut tokens = line.split_whitespace().peekable();
        // Namespace/section tracking, so `namespace N / theorem P / end`
        // yields `N.P` and cannot be confused with a top-level `P`.
        match tokens.peek().copied() {
            Some("namespace") => {
                let mut it = line.split_whitespace();
                it.next();
                if let Some(n) = it.next() {
                    namespaces.push(n.to_string());
                }
                continue;
            }
            Some("end") => {
                namespaces.pop();
                continue;
            }
            _ => {}
        }
        while let Some(tok) = tokens.peek().copied() {
            if MODIFIERS.contains(&tok) {
                tokens.next();
                continue;
            }
            break;
        }
        // `let rec <name>` lifts to a real top-level constant
        // `<Principal>.<name>`. Zero live bodies use it, so closing it is
        // free. Its sibling `where` is deliberately NOT closed: one live
        // body uses it and it is an ordinary proof idiom, so rejecting it
        // would cost real work. Both produce a constant under the
        // principal's OWN namespace, which the owner module certificate
        // covers (the closure walk audits value as well as type, so
        // axioms are still recorded) — they are an invariant leak, not a
        // soundness one. The residual risk is an auxiliary named exactly
        // like the node, which `tabletNodeId?` canonicalizes onto the real
        // node id; hardening that canonicalizer is the follow-up.
        if kw_is_let_rec(line) {
            if let Some(name) = line
                .split_whitespace()
                .nth(2)
                .filter(|t| is_plausible_decl_name(t))
            {
                names.insert(qualify(&namespaces, name));
            }
            continue;
        }
        let Some(kw) = tokens.next() else { continue };
        if !KEYWORDS.contains(&kw) {
            continue;
        }
        match tokens.next() {
            Some(name) if is_plausible_decl_name(name) => {
                names.insert(qualify(&namespaces, name));
            }
            // Keyword alone on the line: the name is on the next one.
            None => pending_keyword = true,
            // A declaration keyword whose next token is NOT a name — the
            // anonymous form, e.g. `instance : Inhabited Bool := ...`, whose
            // name the elaborator invents. Over-detection is free in a delta,
            // so record a positional sentinel rather than skipping: the body
            // still cannot introduce one without moving the set.
            Some(_) => {
                let n = names.len();
                names.insert(qualify(&namespaces, &format!("<anonymous {kw} #{n}>")));
            }
        }
    }
    names
}

/// `let rec <name>` — the binder lifts to a top-level constant.
fn kw_is_let_rec(line: &str) -> bool {
    let mut it = line.split_whitespace();
    it.next() == Some("let") && it.next() == Some("rec")
}

fn qualify(namespaces: &[String], name: &str) -> String {
    if namespaces.is_empty() {
        name.to_string()
    } else {
        format!("{}.{}", namespaces.join("."), name)
    }
}

/// A token that could be a declaration name: not punctuation, not a
/// binder opener. Keeps `instance : Foo` (anonymous) from registering
/// `:` as a name.
fn is_plausible_decl_name(token: &str) -> bool {
    let first = token.chars().next().unwrap_or(':');
    // `«` (U+00AB) opens a guillemet-quoted identifier — `theorem «Evil»` is
    // a legal declaration whose name starts with a punctuation char, and it
    // slipped the gate on all 626 live prefixes before this.
    (first.is_alphabetic() || first == '_' || first == '\u{00AB}') && !token.starts_with("--")
}

/// Remove block/line comments and `@[...]` attribute groups, including
/// the MULTI-LINE forms that `strip_leading_attribute_groups` gives up
/// on. Replaces them with spaces so line structure is preserved.
fn strip_comments_and_attributes(content: &str) -> String {
    let bytes: Vec<char> = content.chars().collect();
    let mut out = String::with_capacity(content.len());
    let mut i = 0usize;
    let mut block_depth = 0usize;
    let mut attr_depth = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied().unwrap_or('\0');
        if block_depth > 0 {
            if c == '-' && next == '/' {
                block_depth -= 1;
                out.push(' ');
                out.push(' ');
                i += 2;
                continue;
            }
            if c == '/' && next == '-' {
                block_depth += 1;
                out.push(' ');
                out.push(' ');
                i += 2;
                continue;
            }
            out.push(if c == '\n' { '\n' } else { ' ' });
            i += 1;
            continue;
        }
        if attr_depth > 0 {
            if c == '[' {
                attr_depth += 1;
            } else if c == ']' {
                attr_depth -= 1;
            }
            out.push(if c == '\n' { '\n' } else { ' ' });
            i += 1;
            continue;
        }
        // String literals must be skipped BEFORE comment/attribute openers:
        // a body may legitimately contain those two-char sequences inside a
        // string, and treating them as openers swallowed the remainder of the
        // file. Confirmed bypass (2026-08-01 audit): a `have` binding a string
        // containing a block-comment opener hid an appended theorem on all 626
        // live prefixes. Handles the `s!` / `r` prefixes and backslash escapes.
        if c == '"' {
            out.push(' ');
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i];
                if ch == '\\' {
                    out.push(' ');
                    if i + 1 < bytes.len() {
                        out.push(if bytes[i + 1] == '\n' { '\n' } else { ' ' });
                    }
                    i += 2;
                    continue;
                }
                out.push(if ch == '\n' { '\n' } else { ' ' });
                i += 1;
                if ch == '"' {
                    break;
                }
            }
            continue;
        }
        if c == '/' && next == '-' {
            block_depth = 1;
            out.push(' ');
            out.push(' ');
            i += 2;
            continue;
        }
        if c == '-' && next == '-' {
            while i < bytes.len() && bytes[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '@' && next == '[' {
            attr_depth = 1;
            out.push(' ');
            out.push(' ');
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod declared_names_delta_tests {
    use super::declared_names_permissive;

    /// NOTE: anonymous `initialize (...)` is NOT covered here — it declares
    /// no name, so a name delta cannot see it. It is closed by the banned
    /// token list. An earlier version of this test asserted the NAMED form,
    /// which is not the reachable one, and so gave false assurance.
    ///
    /// The gate is a DELTA, so the property is "a body cannot add a name".
    /// Each of these compiles under the tablet toolchain and passes every
    /// other gate; `private lemma` additionally defeats both the
    /// first-token text scan and the Init-only Lean parse.
    #[test]
    fn a_body_cannot_introduce_a_top_level_declaration() {
        let prefix = "-- [TABLET NODE: Foo]\ntheorem Foo : True := by\n-- BODY\n";
        let base = declared_names_permissive(&format!("{prefix}  sorry\n"));
        let attacks = [
            "  trivial\n\ntheorem P : True := trivial\n",
            "  trivial\n\nprivate theorem P : True := trivial\n",
            "  trivial\n\nprivate lemma P : True := trivial\n",
            "  trivial\n\nnonrec theorem P : True := trivial\n",
            "  trivial\n\nlocal instance P : Inhabited Bool := trivial\n",
            "  trivial\n\nset_option maxHeartbeats 1 in theorem P : True := trivial\n",
            "  trivial\n\nopen Nat in theorem P : True := trivial\n",
            "  trivial\n\nnamespace NS\nprotected theorem P : True := trivial\nend NS\n",
            // The escape an allowlist rule could not close: `namespace <Node>`
            // makes every injected name look like a legitimate auxiliary.
            "  trivial\n\nnamespace Foo\ntheorem P : True := trivial\nend Foo\n",
            "  trivial\n\ntheorem\n  P : True := trivial\n",
            "  trivial\n\n/-- doc -/ theorem P : True := trivial\n",
            "  trivial\n\n@[simp,\n  reducible] def P : Nat := 0\n",
            "  trivial\n\ntheorem «Evil» : True := trivial\n",
            "  have _s : String := \"/-\"\n  trivial\n\ntheorem E2 : True := trivial\n",
            "  trivial\n\ninstance : Inhabited Bool := trivial\n",
            "  let rec helper : True := trivial\n  exact helper\n",
        ];
        for body in attacks {
            let got = declared_names_permissive(&format!("{prefix}{body}"));
            assert_ne!(
                got, base,
                "body slipped a declaration past the delta: {body:?}"
            );
        }
    }

    /// The other half: legitimate bodies must not move the set. Validated
    /// against all 626 marker-bearing nodes of one live run's tablet
    /// with zero false positives; these pin the shapes that matter.
    #[test]
    fn ordinary_proof_bodies_move_nothing() {
        let prefix = "-- [TABLET NODE: Foo]\ntheorem Foo : True := by\n-- BODY\n";
        let base = declared_names_permissive(&format!("{prefix}  sorry\n"));
        let ok = [
            "  trivial\n",
            // 8 live nodes raise heartbeats this way.
            "  set_option maxHeartbeats 400000 in\n  trivial\n",
            "  have h : True := trivial\n  let k := 3\n  trivial\n",
            "  -- this theorem P is only prose\n  trivial\n",
        ];
        for body in ok {
            assert_eq!(
                declared_names_permissive(&format!("{prefix}{body}")),
                base,
                "legitimate body was flagged: {body:?}"
            );
        }
    }
}

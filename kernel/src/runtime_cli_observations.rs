#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use trellis_kernel::{
    declaration_heads,
    disk_cache::{
        cache_dir_for_namespace, cache_lookup_dirs, disk_cache_get_first, disk_cache_put,
    },
    extract_tex_statement_items, is_proof_bearing_statement_environment,
    resolve_local_closure_axcheck_enabled, tex_statement_environment,
    validate_declaration_kind_shape, validate_tex_format, AxiomizationCheckOutput, DeviationId,
    LocalClosureProbeOutput, NodeId, NodeKind, RefPaperId, ReferencePaperSpec,
    SoundFingerprintParts, TargetId,
    WorkerProofDeltaMode, WorkerValidationStepResult, SORRY_AX_REJECTION_REMINDER,
};

/// Status value assigned to `LocalClosureProbeOutput.status` when the
/// dual-collector cross-check produced a real disagreement (non-crash,
/// non-skipped, `agreed=false`). Distinct from `internal_error` so the
/// upstream caller can branch to fail-loudly halt semantics rather than
/// the transient-retry path.
pub(crate) const CHECKER_DISAGREEMENT_STATUS: &str = "checker_disagreement";

/// Filename of the persisted halt marker dropped at runtime-root when a
/// dual-collector disagreement is observed. Self-documenting JSON; the
/// supervisor `run` loop checks for its presence at every iteration and
/// refuses to dispatch a new burst while it exists. Operator clears the
/// halt by deleting the file (the JSON itself carries instructions).
pub const CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME: &str = "checker_disagreement_halt.json";

/// Filename of the persisted halt marker dropped at runtime-root when an
/// agent burst returns a non-empty `system_feedback` string AND
/// system-feedback halting is enabled (top-level `system_feedback_halt`
/// in `trellis.config.json`, or `TRELLIS_SYSTEM_FEEDBACK_HALT=1` in the
/// environment — see [`system_feedback_halt_enabled`]). Same fail-loudly
/// machinery as `CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME` but distinct
/// path so the operator can tell the two halt causes apart. The run-loop
/// and bridge checks on this marker are UNCONDITIONAL: a marker already
/// on disk (e.g. written under an earlier halt-enabled configuration)
/// halts the run regardless of the current knob value.
pub const SYSTEM_FEEDBACK_HALT_MARKER_FILENAME: &str = "system_feedback_halt.json";

/// Filename of the persisted, restart-surviving set of acknowledged
/// `system_feedback` fingerprints. A human adds a fingerprint here (via
/// the `acknowledge-system-feedback` CLI subcommand) to opt a specific
/// design-gap CLASS out of the opt-in fail-loudly halt: recurrences of
/// that exact fingerprint then log-and-continue instead of freezing the
/// run even when halting is enabled. Absent / unparseable file ⇒ empty
/// set ⇒ every emission still halts when halting is enabled
/// (fail-closed default — the ack policy is a pure superset).
pub const SYSTEM_FEEDBACK_ACK_STORE_FILENAME: &str = "system_feedback_acks.json";

/// Filename of the append-only unified log recording EVERY non-empty
/// `system_feedback` emission — halted, acked, or log-and-continue — as
/// one durable JSONL stream regardless of the halt knob. Each row
/// carries kind/cycle/request_id/request_kind/burst_role/lane/artifact/
/// system_feedback/fingerprint/unix_ts plus `acked` and `halted` flags.
/// (Supersedes the acked-only `system_feedback_design_gap_log.jsonl`;
/// files under the old name on existing runtimes remain as historical
/// artifacts.)
pub const SYSTEM_FEEDBACK_LOG_FILENAME: &str = "system_feedback_log.jsonl";

/// Environment override for the `system_feedback_halt` config knob.
/// `1`/`true` forces halting on, `0`/`false` forces it off, taking
/// precedence over `trellis.config.json` in both directions
/// (launcher-friendly). Unset/empty defers to the config.
pub const SYSTEM_FEEDBACK_HALT_ENV: &str = "TRELLIS_SYSTEM_FEEDBACK_HALT";

/// Aliased to the canonical backend-descriptor home so the Lean
/// import-policy literal lives in exactly one place (mirrors how
/// `DEFAULT_APPROVED_AXIOMS` aliases `CANONICAL_APPROVED_AXIOMS`).
const ALLOWED_IMPORT_PREFIXES: &[&str] = trellis_kernel::backend::LEAN_ALLOWED_IMPORT_PREFIXES;
const PREAMBLE_NAME: &str = "Preamble";
const AXIOMS_NAME: &str = "Axioms";
const HEADER_NAME: &str = "header";
const ASSUMPTIONS_NAME: &str = trellis_kernel::assumptions_registry::ASSUMPTIONS_NODE;
const WHOLE_FILE_DECLARATION_HASH_PREFIX: &str = "whole-file-v1:";
// v4 (2026-07-04): the semantic-payload format changed — the fingerprint
// script now resolves namespaced seeds (previously every PV payload was the
// degenerate `root|X||missing|X`) and emits owner-node-tagged const lines.
// Every node's `lean_semantic_closure` axis therefore changes value; the
// one-shot axis migration below re-runs once to recompute live fingerprints
// and re-pin clean approved baselines under the new payloads.
pub(crate) const CORR_FINGERPRINT_SCHEMA_VERSION: u32 = 4;
/// Auto-managed Tablet-dir filenames that legitimately appear in
/// `changes.modified` without being kernel-ratified nodes (regen flows,
/// header/preamble materialization). Kept as full filenames (with
/// extensions) because they're matched against `changes.modified`
/// entries — distinct from the bare-name `PREAMBLE_NAME`/`HEADER_NAME`
/// constants above which name nodes.
const STRUCTURAL_FILENAMES: &[&str] = &["header.tex", "Preamble.tex", "INDEX.md", "README.md"];
/// Aliased to the canonical backend-descriptor home (the 16-entry Lean
/// lexical-policy keyword list, copied there verbatim). Single source of
/// truth; `backend::tests::descriptor_constants_match_legacy_literals`
/// pins the exact sequence.
const FORBIDDEN_KEYWORDS: &[&str] = trellis_kernel::backend::LEAN_FORBIDDEN_KEYWORDS;
/// Audit M-2 — runtime-CLI baseline approved-axiom set. Aliased to the
/// kernel-wide canonical constant so engine accept ceiling, runtime CLI
/// default, and the public-viewer export script never drift. The
/// per-node `APPROVED_AXIOMS.json` can WIDEN this set (per-node
/// waivers); the canonical four is the floor.
const DEFAULT_APPROVED_AXIOMS: &[&str] = trellis_kernel::model::CANONICAL_APPROVED_AXIOMS;

pub(crate) fn under_model_assumption_nodes_from_roles(
    is_pv: bool,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> BTreeSet<NodeId> {
    if !is_pv {
        return BTreeSet::new();
    }
    node_role
        .iter()
        .filter(|(node, role)| {
            node.as_str() == ASSUMPTIONS_NAME
                && **role == trellis_kernel::PvRole::UnderModelAssumptions
        })
        .map(|(node, _)| node.clone())
        .collect()
}

pub(crate) fn under_model_assumption_nodes_from_state(
    state: &crate::ProtocolState,
) -> BTreeSet<NodeId> {
    under_model_assumption_nodes_from_roles(state.is_pv(), &state.node_role)
}

fn is_under_model_assumption_observation_node(
    node_name: &str,
    under_model_assumption_nodes: &BTreeSet<NodeId>,
) -> bool {
    node_name == ASSUMPTIONS_NAME
        && under_model_assumption_nodes
            .iter()
            .any(|node| node.as_str() == node_name)
}

fn is_assumption_authoring_observation_node(
    node_name: &str,
    assumption_authoring_node: Option<&NodeId>,
) -> bool {
    node_name == ASSUMPTIONS_NAME
        && assumption_authoring_node.is_some_and(|node| node.as_str() == node_name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabletNodeValidationSemantics {
    Ordinary,
    ExtractionModel,
    UnderModelAssumptions,
}

fn observation_node_role(
    node_name: &str,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> Option<trellis_kernel::PvRole> {
    node_role.get(node_name).copied()
}

fn is_extraction_model_observation_node(
    node_name: &str,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> bool {
    matches!(
        observation_node_role(node_name, node_role),
        Some(trellis_kernel::PvRole::ExtractionModel)
    )
}

fn tablet_node_validation_semantics(
    node_name: &str,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
    assumption_authoring_node: Option<&NodeId>,
) -> TabletNodeValidationSemantics {
    let is_under_model_assumptions = node_name == ASSUMPTIONS_NAME
        && matches!(
            observation_node_role(node_name, node_role),
            Some(trellis_kernel::PvRole::UnderModelAssumptions)
        );
    if is_assumption_authoring_observation_node(node_name, assumption_authoring_node)
        || is_under_model_assumptions
    {
        return TabletNodeValidationSemantics::UnderModelAssumptions;
    }
    if is_extraction_model_observation_node(node_name, node_role) {
        return TabletNodeValidationSemantics::ExtractionModel;
    }
    TabletNodeValidationSemantics::Ordinary
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeywordHit {
    pub keyword: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceLineHit {
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalCommandObservation {
    pub returncode: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub spawn_error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeObservation {
    pub node: String,
    pub lean_path: String,
    pub tex_path: String,
    pub lean_exists: bool,
    pub tex_exists: bool,
    pub lean_content: String,
    pub tex_content: String,
    pub compile: ExternalCommandObservation,
    pub print_axioms: ExternalCommandObservation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreambleObservation {
    pub lean_exists: bool,
    pub tex_exists: bool,
    pub lean_content: String,
    pub tex_content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabletObservation {
    pub tablet_exists: bool,
    pub preamble: PreambleObservation,
    pub node_names: Vec<String>,
    pub invalid_node_names: Vec<String>,
    pub missing_tex_for_lean: Vec<String>,
    pub orphan_tex_nodes: Vec<String>,
    pub nodes: BTreeMap<String, NodeObservation>,
    pub build: ExternalCommandObservation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeanSemanticPayloadObservation {
    pub ok: bool,
    pub payload: String,
    pub error: String,
}

#[derive(Debug, Clone, Default)]
struct CorrespondenceFingerprintObservation {
    fingerprints: BTreeMap<NodeId, String>,
    unavailable_reasons: BTreeMap<NodeId, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorRecord {
    pub message: String,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvaluatedNode {
    pub ok: bool,
    /// Shallow companion to `ok`: drops the transitive checks
    /// (`sorry_warnings.is_empty()` and `axioms_valid`) so the result
    /// reflects "the node's own .lean compiles cleanly with no
    /// `sorry` literal in its file" — matching the kernel's
    /// authoritative `committed.open_nodes` semantics
    /// (`worker_normalization::open_nodes_from_repo`, which uses a
    /// pure textual `has_sorry` check).
    ///
    /// Used by the `must_close_active` enforcement so the contract
    /// that asks the worker to "close THIS node" is satisfied when the
    /// active node's own file is sorry-free, regardless of whether
    /// helpers it imports carry `sorry` (those helpers are governed by
    /// `allow_new_obligations` and tracked separately in
    /// `committed.open_nodes`).
    pub shallow_ok: bool,
    /// Shallow textual `sorry` presence in the node's own .lean file.
    /// Mirrors the check used by
    /// `worker_normalization::open_nodes_from_repo`. Exposed so the
    /// `must_close_active` enforcement's error-message dispatcher can
    /// distinguish "your file has sorry" from "your file is fine but
    /// some other gate failed" without re-scanning forbidden hits.
    pub sorry_in_source: bool,
    pub compiles: bool,
    pub sorry_free: bool,
    pub keyword_clean: bool,
    pub imports_valid: bool,
    pub declaration_intact: bool,
    pub marker_valid: bool,
    pub declaration_name_matches: bool,
    pub tex_format_valid: bool,
    pub axioms_valid: bool,
    pub audited_axioms: Vec<String>,
    pub axiom_violations: Vec<String>,
    pub import_violations: Vec<String>,
    pub forbidden_hits: Vec<KeywordHit>,
    pub sorry_warnings: Vec<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub build_output: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvaluatedTablet {
    pub ok: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub error_records: Vec<ErrorRecord>,
    pub build_output: String,
    pub nodes: BTreeMap<String, EvaluatedNode>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SnapshotChanges {
    pub created: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProofWorkerDeltaScope {
    active_changed: bool,
    /// True when the active node's `.lean` or `.tex` was modified but
    /// the active node is not in the reviewer's `authorized_nodes`
    /// list and the proof_edit_mode requires explicit authorization
    /// (Restructure / CoarseRestructure). In Local / Easy modes the
    /// active node is implicitly editable as the proof body and this
    /// flag is never set.
    unauthorized_active_change: bool,
    new_lean_files: Vec<String>,
    stray_new_tex: Vec<String>,
    authorized_extra_changed_existing_nodes: Vec<String>,
    unauthorized_extra_changed_existing_nodes: Vec<String>,
    deleted_existing_nodes: Vec<String>,
    /// Files in `changes.modified` whose stems are not in the kernel's
    /// `current_present_nodes` and are not the active node. These
    /// indicate a prior burst left filesystem state behind that the
    /// kernel never ratified — typically the result of a silent
    /// retry that bypassed `restore_repo_worktree_for_event`.
    /// Reject loudly: see Bug X (task #52) for the upstream cause.
    ghost_node_files: Vec<String>,
}

fn repo_check_script_path(repo_path: &Path) -> PathBuf {
    repo_path.join(".trellis").join("scripts").join("check.py")
}

fn node_name_from_tablet_file(name: &str) -> Option<String> {
    Path::new(name)
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::to_string)
}

/// Patch C-R-e: when a worker burst introduces new helpers AND the
/// active node's probe references those helpers as `boundary_theorems`,
/// the C-K fail-closed `validate_probe_present_nodes` check must admit
/// the new helpers — they are valid deps post-delta even though they
/// weren't in `current_present_nodes` at burst-emit time. This helper
/// returns a fresh `BTreeSet<NodeId>` containing every entry in
/// `base_present` plus every `new_lean_files` entry (each converted
/// from a Tablet-stem string into a `NodeId`).
fn augment_present_nodes_with_burst_new(
    base_present: &BTreeSet<NodeId>,
    new_lean_files: &[String],
) -> BTreeSet<NodeId> {
    let mut augmented: BTreeSet<NodeId> = base_present.clone();
    for name in new_lean_files {
        augmented.insert(NodeId::from(name.as_str()));
    }
    augmented
}

/// Patch C-R-e companion: same idea for the node-kind map. For each new
/// helper, infer its kind from the post-delta `.tex` environment:
/// theorem-shaped → `Proof`, otherwise `Definition`. Missing or
/// unreadable `.tex` defaults to `Definition` (the safer choice — a
/// definition-kind boundary dep is rejected by `validate_probe_present_nodes`
/// if listed under `boundary_theorems`, surfacing the issue rather than
/// silently letting it through).
fn augment_node_kinds_for_burst_new(
    base_kinds: &BTreeMap<NodeId, NodeKind>,
    new_lean_files: &[String],
    repo_path: &Path,
) -> BTreeMap<NodeId, NodeKind> {
    let mut augmented = base_kinds.clone();
    for name in new_lean_files {
        let tex_path = repo_path.join("Tablet").join(format!("{name}.tex"));
        let tex_content = std::fs::read_to_string(&tex_path).unwrap_or_default();
        let env = tex_statement_environment(&tex_content);
        let kind = if is_proof_bearing_statement_environment(&env) {
            NodeKind::Proof
        } else {
            NodeKind::Definition
        };
        augmented.insert(NodeId::from(name.as_str()), kind);
    }
    augmented
}

fn modified_existing_node_names(
    changes: &SnapshotChanges,
    current_present_nodes: &BTreeSet<NodeId>,
    target: trellis_kernel::backend::BackendId,
) -> BTreeSet<String> {
    // R4 B5: the node-source suffix is the target's (`.lean` ⇒ byte-identical on
    // the live run; `.thy` for IsabelleHol). The `.tex` arm is backend-agnostic.
    let source_suffix = format!(
        ".{}",
        trellis_kernel::backend::descriptor_for(target).node_source_ext
    );
    changes
        .modified
        .iter()
        .filter(|name| {
            (name.ends_with(&source_suffix) || name.ends_with(".tex"))
                && *name != "header.tex"
                && *name != "Preamble.tex"
        })
        .filter_map(|name| node_name_from_tablet_file(name))
        .filter(|name| current_present_nodes.contains(name.as_str()))
        .collect()
}

fn deleted_existing_node_names(
    changes: &SnapshotChanges,
    current_present_nodes: &BTreeSet<NodeId>,
    target: trellis_kernel::backend::BackendId,
) -> BTreeSet<String> {
    // R4 B5: see `modified_existing_node_names`.
    let source_suffix = format!(
        ".{}",
        trellis_kernel::backend::descriptor_for(target).node_source_ext
    );
    changes
        .deleted
        .iter()
        .filter(|name| {
            (name.ends_with(&source_suffix) || name.ends_with(".tex"))
                && *name != "header.tex"
                && *name != "Preamble.tex"
        })
        .filter_map(|name| node_name_from_tablet_file(name))
        .filter(|name| current_present_nodes.contains(name.as_str()))
        .collect()
}

fn proof_worker_delta_scope(
    changes: &SnapshotChanges,
    active_node: &str,
    current_present_nodes: &BTreeSet<NodeId>,
    proof_edit_mode: WorkerProofDeltaMode,
    authorized_nodes: &BTreeSet<NodeId>,
    target: trellis_kernel::backend::BackendId,
) -> ProofWorkerDeltaScope {
    // R4 B1–B5: the node-SOURCE filename suffix is the target's (`.lean` on the
    // all-Lean live run ⇒ every `ends_with` filter is byte-identical to the
    // historical hardcoded `.lean`; `.thy` for IsabelleHol so a `.thy` create /
    // modify / delete is recognized as a node-source change). The `.tex`
    // statement file is backend-agnostic (both backends carry it), so `.tex`
    // arms are unchanged. `node_name_from_tablet_file` is file-stem based, so
    // only the suffix predicates here need routing.
    let source_suffix = format!(
        ".{}",
        trellis_kernel::backend::descriptor_for(target).node_source_ext
    );
    let active_lean = format!("{active_node}{source_suffix}");
    let active_tex = format!("{active_node}.tex");
    let active_changed = changes
        .modified
        .iter()
        .any(|name| name == &active_lean || name == &active_tex);
    let mut new_lean_files: Vec<String> = changes
        .created
        .iter()
        .filter(|name| name.ends_with(&source_suffix))
        .filter_map(|name| node_name_from_tablet_file(name))
        .collect();
    new_lean_files.sort();
    let mut new_tex_files: Vec<String> = changes
        .created
        .iter()
        .filter(|name| name.ends_with(".tex") && *name != "header.tex" && *name != "Preamble.tex")
        .filter_map(|name| node_name_from_tablet_file(name))
        .collect();
    new_tex_files.sort();
    let mut stray_new_tex: Vec<String> = new_tex_files
        .iter()
        .filter(|name| !new_lean_files.contains(*name))
        .cloned()
        .collect();
    stray_new_tex.sort();

    let mut extra_changed_existing_nodes: Vec<String> =
        modified_existing_node_names(changes, current_present_nodes, target)
            .into_iter()
            .filter(|name| name != active_node)
            .collect();
    extra_changed_existing_nodes.sort();

    let allows_authorized_existing = matches!(
        proof_edit_mode,
        WorkerProofDeltaMode::Restructure | WorkerProofDeltaMode::CoarseRestructure
    );
    let mut authorized_extra_changed_existing_nodes = Vec::new();
    let mut unauthorized_extra_changed_existing_nodes = Vec::new();
    for name in extra_changed_existing_nodes {
        if allows_authorized_existing && authorized_nodes.contains(name.as_str()) {
            authorized_extra_changed_existing_nodes.push(name);
        } else {
            unauthorized_extra_changed_existing_nodes.push(name);
        }
    }

    // Active-node edit authorization: in Restructure / CoarseRestructure,
    // the active node is editable only if explicitly listed in the
    // reviewer's `authorized_nodes` set — `next_active` alone is a
    // scope anchor, not edit permission. Local / Easy modes are
    // unaffected: the active node IS the proof body those modes
    // operate on.
    let unauthorized_active_change =
        active_changed && allows_authorized_existing && !authorized_nodes.contains(active_node);

    let mut deleted_existing_nodes: Vec<String> =
        deleted_existing_node_names(changes, current_present_nodes, target)
            .into_iter()
            .collect();
    deleted_existing_nodes.sort();

    // Ghost files: tablet-dir entries in `changes.modified` whose node
    // stems are not in the kernel's authoritative present-nodes set
    // and are not the active node (which is always allowed). These
    // signal a prior burst's filesystem mutations that were never
    // rolled back; see Bug X / task #52 for the upstream cause and
    // Bug Y / task #51 for this defense.
    let mut ghost_node_files: Vec<String> = changes
        .modified
        .iter()
        .filter(|name| {
            if !name.ends_with(&source_suffix) && !name.ends_with(".tex") {
                return false;
            }
            // Auto-managed structural files are regenerated each prep —
            // never ghosts. (Their bare names like "header" / "Preamble"
            // would also be excluded by the stem-level filter below;
            // this is a defense-in-depth filename pre-filter.)
            !STRUCTURAL_FILENAMES.contains(&name.as_str())
        })
        .filter_map(|name| node_name_from_tablet_file(name))
        .filter(|stem| {
            stem != PREAMBLE_NAME
                && stem != AXIOMS_NAME
                && stem != HEADER_NAME
                && stem != active_node
                && !current_present_nodes.contains(stem.as_str())
        })
        .collect();
    ghost_node_files.sort();
    ghost_node_files.dedup();

    ProofWorkerDeltaScope {
        active_changed,
        unauthorized_active_change,
        new_lean_files,
        stray_new_tex,
        authorized_extra_changed_existing_nodes,
        unauthorized_extra_changed_existing_nodes,
        deleted_existing_nodes,
        ghost_node_files,
    }
}

/// Optionally attach a child process spawned via `Command` to the
/// cgroup-v2 subtree named by `TRELLIS_CHECK_CGROUP`. Each child
/// writes its own PID to `<cgroup>/cgroup.procs` after fork but
/// before exec, so the freshly-exec'd `python3` (and every grandchild
/// it forks — lake, lean, etc.) inherits the cgroup's memory ceiling.
///
/// Scoped via the `IN_PARALLEL_CHECK_BATCH` thread-local: cgroup attach
/// only fires from inside `observe_nodes_parallel`'s spawned worker
/// threads, where bounding aggregate memory across N concurrent lean
/// processes is the explicit goal. Other callers — single-shot
/// `lean-semantic-payloads`, `materialize-tablet-oleans`,
/// `sync-tablet-support`, the OOM-rescue serial retry — run on the
/// main thread with the flag false, so they're never cgroup-bounded
/// and can't be terminated by the 24 GB cap. This is the audit fix
/// for the cardinal-rule violation where a single-call subcommand
/// OOM'd inside the cgroup with no rescue path.
///
/// No-op when the env var is unset (current behavior, no ceiling).
/// Errors during attach are non-fatal — if the cgroup write fails the
/// child still runs, just without the memory cap. Quota/cgroup
/// tracking must never break a run; checks proceed without ceiling.
fn apply_check_cgroup_attach(cmd: &mut Command) {
    if !IN_PARALLEL_CHECK_BATCH.with(|c| c.get()) {
        return;
    }
    let cgroup_dir = match std::env::var("TRELLIS_CHECK_CGROUP") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return,
    };
    let procs_path = std::path::PathBuf::from(&cgroup_dir).join("cgroup.procs");
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(move || {
            // libc::getpid is async-signal-safe and the only sane way
            // to read our own PID from inside a post-fork pre-exec hook.
            let pid = libc::getpid();
            // Best-effort write. Errors here are silently ignored so
            // the spawn never fails on cgroup misconfig.
            let _ = std::fs::write(&procs_path, pid.to_string());
            Ok(())
        });
    }
}

thread_local! {
    /// True only inside `observe_nodes_parallel`'s spawned worker
    /// threads. Gates `apply_check_cgroup_attach` so the cgroup memory
    /// ceiling applies ONLY to the parallel per-node lean-compile-node
    /// batch — not to single-call subcommands like lean-semantic-payloads
    /// or to the OOM-rescue serial retry, both of which need full system
    /// memory available.
    static IN_PARALLEL_CHECK_BATCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Read the cgroup-v2 OOM-kill counter for `TRELLIS_CHECK_CGROUP`.
/// Returns 0 when the env var is unset or the file is unreadable —
/// this is a passive diagnostic only.
fn read_check_cgroup_oom_kill() -> u64 {
    let cgroup_dir = match std::env::var("TRELLIS_CHECK_CGROUP") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => return 0,
    };
    // memory.events has lines like "oom_kill 0". We want the local
    // counter to attribute kills inside this cgroup specifically;
    // memory.events.local is preferred but fall back to memory.events.
    for fname in &["memory.events.local", "memory.events"] {
        let path = std::path::PathBuf::from(&cgroup_dir).join(fname);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            if parts.next() == Some("oom_kill") {
                if let Some(v) = parts.next().and_then(|s| s.parse::<u64>().ok()) {
                    return v;
                }
            }
        }
        return 0;
    }
    0
}

pub(crate) fn run_repo_command_json(
    repo_path: &Path,
    subcommand: &str,
    args: &[String],
) -> Result<serde_json::Value, String> {
    let start = std::time::Instant::now();
    // Snapshot the cgroup OOM counter BEFORE spawn so we can attribute
    // any in-flight OOM to this child specifically. With parallel
    // checks the counter could be incremented by a sibling, but the
    // exit-signal check below is the primary OOM indicator; the
    // counter is a corroborating signal.
    let oom_pre = read_check_cgroup_oom_kill();
    let mut cmd = Command::new("python3");
    cmd.arg(repo_check_script_path(repo_path))
        .arg(subcommand)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_check_cgroup_attach(&mut cmd);
    let output = cmd.output();
    let duration = start.elapsed().as_secs_f64();
    let output = match output {
        Ok(o) => o,
        Err(err) => {
            trellis_kernel::check_ledger::append(repo_path, subcommand, duration, false, 0, 0);
            return Err(format!("spawn {subcommand} failed: {err}"));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed = serde_json::from_str::<serde_json::Value>(stdout.trim());
    // OOM classification: distinguishes "wrapper killed our process"
    // (cgroup OOM-killer fired because we exceeded TRELLIS_CHECK_CGROUP's
    // memory.max) from "lean rejected the build" (legitimate compile
    // error or sorry warning, exit code != 0 but the process ran to
    // completion). The decision-maker that chooses retry vs. escalate
    // needs this distinction: OOM should retry (possibly serially or
    // with more memory); legitimate failure should NOT retry blindly.
    //
    // Detection: SIGKILL exit signal is the unambiguous per-child signal
    // (the cgroup OOM-killer always uses SIGKILL on the offending child).
    // The cgroup oom_kill counter is process-wide, so under parallelism a
    // sibling's OOM would advance it for every concurrent child — gating
    // on counter-advance would produce false-positive OOMs on successful
    // siblings. We snapshot the counter pre/post anyway and surface its
    // delta in the diagnostic Err message for operator context.
    use std::os::unix::process::ExitStatusExt;
    let killed_by_sigkill = output.status.signal() == Some(libc::SIGKILL);
    let oom_post = read_check_cgroup_oom_kill();
    let oom_detected = killed_by_sigkill;
    trellis_kernel::check_ledger::append_full(
        repo_path,
        "check",
        subcommand,
        duration,
        parsed.is_ok() && output.status.success() && !oom_detected,
        stdout.len(),
        stderr.len(),
        oom_detected,
    );
    if oom_detected {
        return Err(format!(
            "[OOM] {subcommand}: cgroup OOM-killer terminated the check process \
             (signal={:?}, oom_kill counter {} -> {}); cgroup memory.max may need \
             to be raised or TRELLIS_LEAN_PARALLELISM reduced. stderr={:?}",
            output.status.signal(),
            oom_pre,
            oom_post,
            stderr.trim(),
        ));
    }
    parsed.map_err(|err| {
        format!(
            "{subcommand} returned invalid JSON: {err}; stdout={:?}; stderr={:?}",
            stdout.trim(),
            stderr.trim()
        )
    })
}

fn read_text_if_exists(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn external_command_from_value(
    raw: serde_json::Value,
    subcommand: &str,
) -> Result<ExternalCommandObservation, String> {
    serde_json::from_value(raw).map_err(|err| format!("parse {subcommand} output failed: {err}"))
}

/// Two-tier caches for the per-node Lean ops issued from `observe_node`:
///   Tier 1 (in-memory): `LazyLock<Mutex<HashMap>>` static.
///   Tier 2 (disk):      `<runtime_root>/checker-state/kernel-cache/`
///                       under namespace `lean-compile-node` /
///                       `print-axioms` (resolved via
///                       `crate::disk_cache::cache_dir_for_namespace`).
///
/// The kernel CLI binary is short-lived (Popen'd once per
/// `RuntimeCliRequest` by the Python wrapper). The in-memory tier alone
/// cannot persist across cycles or across separate `run_kernel_cli`
/// calls within one cycle (e.g. `prepare_worker_gate` followed by
/// `check_trellis_worker_result` are two distinct child kernel
/// processes). Disk persistence closes that gap.
///
/// Why each is cached:
///   - Each `lean-compile-node` subprocess call dispatches into the
///     checker socket which acquires the lake lock; live runs show ~17s
///     per call (mostly lock-wait + ~3s lake work). A 90-node closure
///     ⇒ ~26 minutes of pure dispatch overhead even when every olean is
///     already on disk and the build is a no-op.
///   - `print-axioms` is per-node and runs after `lean-compile-node`;
///     ~5-15ms each on cache hits, but with hundreds of nodes it adds
///     up and crucially still requires a Python subprocess fork.
///
/// Same correctness contract as the lean-semantic-payloads cache:
/// content-hash key (no mtimes), conservative on key-construction
/// failure (skip cache → live call), only `returncode == Some(0)`
/// successes are memoised (a transient build failure must not be
/// pinned), olean-presence guard before any cache hit (memory or disk)
/// is served — the cached observation reports a successful build but if
/// the olean is gone, downstream ops would fail; force a fresh
/// dispatch instead.
type CompileNodeCacheKey = String;
static COMPILE_NODE_CACHE: LazyLock<
    Mutex<HashMap<(PathBuf, String), (CompileNodeCacheKey, ExternalCommandObservation)>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));
type PrintAxiomsCacheKey = String;
static PRINT_AXIOMS_CACHE: LazyLock<
    Mutex<HashMap<(PathBuf, String), (PrintAxiomsCacheKey, ExternalCommandObservation)>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Disk-cache namespaces. One subdir per cache; same parent dir layout
/// as documented in `crate::disk_cache`.
const COMPILE_NODE_DISK_NAMESPACE: &str = "lean-compile-node";
const PRINT_AXIOMS_DISK_NAMESPACE: &str = "print-axioms";
const LEAN_SEMANTIC_PAYLOADS_DISK_NAMESPACE: &str = "lean-semantic-payloads";

/// Build the disk-cache lookup key for a node.
///
/// PATH-INDEPENDENT: the lookup key intentionally omits `canon_repo`.
/// Including the path was a bug: each "view" of the same Lean content
/// (the supervisor's bwrap'd repo `<runtime>/.trellis/supervisor/repo`,
/// the live tablet `<runtime>/`, a worker's bwrap mount, etc.) has the
/// same content but a different path, so a path-keyed lookup produced
/// distinct cache files for byte-identical inputs. Cache writes
/// performed against one path were unreachable from another path —
/// observed in production where ~99 entries cached against the
/// supervisor-bwrap path stopped being reachable when the post-restart
/// supervisor read from a different path view.
///
/// Correctness still rests on the closure-content `value_key`: a hit
/// only fires when the stored value_key matches the current value_key
/// (computed from the Lean source contents). The value_key already
/// captures everything that could make the stored result wrong; the
/// path was redundant *and* harmful for sharing.
///
/// `_canon_repo` is kept in the signature to keep call sites stable;
/// it's intentionally unused. Bumping the cache version isn't required
/// because the value_key carries `cache_v=2`; old path-keyed files
/// just sit unreferenced under their old shard locations.
fn per_node_disk_lookup_key(_canon_repo: &Path, node: &str) -> String {
    format!("node={node}")
}

/// Conservative on-disk verification before serving a cached
/// compile-node success. The cached result reports "lake produced an
/// olean for this closure"; if the olean has been deleted between the
/// original observation and now (worker hygiene cleanup, manual rm,
/// `.lake` wipe), the next op that needs the olean (`materialize_oleans`,
/// `lean-semantic-payloads`) would fail. Falling back to the live call
/// in that case re-builds the missing artefact.
fn olean_present(repo_path: &Path, node: &str) -> bool {
    repo_path
        .join(".lake/build/lib/lean/Tablet")
        .join(format!("{node}.olean"))
        .exists()
}

fn run_compile_node(repo_path: &Path, node: &str) -> Result<ExternalCommandObservation, String> {
    let canon_repo = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let key = lean_closure_cache_key(repo_path, node);

    // Tier 1 (in-memory).
    if let Some(ref k) = key {
        let cache = COMPILE_NODE_CACHE.lock().unwrap();
        if let Some((stored_key, stored_value)) = cache.get(&(canon_repo.clone(), node.to_string()))
        {
            // The cached observation was a successful compile; reuse
            // it only if the resulting olean is still on disk. This
            // guards against worker-side `.lake` cleanups silently
            // invalidating the cache between observations within one
            // kernel invocation.
            if stored_key == k && olean_present(repo_path, node) {
                return Ok(stored_value.clone());
            }
        }
    }

    // Tier 2 (disk). Same olean-presence guard as Tier 1.
    //
    // `cache_lookup_dirs` returns up to two directories: the writable
    // cache (this process's own — supervisor-only entries on the
    // supervisor side; worker-only entries on the worker side) and an
    // optional read-only fallback (set in worker contexts to point at
    // the supervisor's cache, which the worker can read but not write).
    // Writes always go to the writable directory only — see the `put`
    // call below.
    let disk_key = per_node_disk_lookup_key(&canon_repo, node);
    if let Some(ref k) = key {
        let dirs = cache_lookup_dirs(COMPILE_NODE_DISK_NAMESPACE);
        if let Some(stored_value) =
            disk_cache_get_first::<ExternalCommandObservation>(&dirs, &disk_key, k)
        {
            if olean_present(repo_path, node) {
                // Promote to Tier 1 to avoid re-reading the disk
                // entry on subsequent calls in this process.
                COMPILE_NODE_CACHE.lock().unwrap().insert(
                    (canon_repo.clone(), node.to_string()),
                    (k.clone(), stored_value.clone()),
                );
                return Ok(stored_value);
            }
        }
    }

    // Route per-target: `tablet_target_for_repo` resolves to `Lean` for the
    // all-Lean live run (config absent or `default_target` != isabelle_hol),
    // so the Lean driver + `[node, repo]` arg shape is byte-identical. The
    // Isabelle `isabelle-check-node` op takes the same `[node, repo]` shape.
    let driver = trellis_kernel::backend::checker_driver_for(
        trellis_kernel::tablet_target_for_repo(repo_path),
    );
    let raw = run_repo_command_json(
        repo_path,
        driver.compile_node_op(),
        &[node.to_string(), repo_path.display().to_string()],
    )?;
    let observation = external_command_from_value(raw, driver.compile_node_op())?;

    // Only cache successful compiles. A non-zero returncode is a
    // legitimate compile error that may resolve on the next call once
    // the worker fixes it; pinning it would suppress the recovery path.
    // `timed_out` and `spawn_error` are also exclusion criteria —
    // transient infrastructure failures must not be pinned.
    if observation.returncode == Some(0)
        && !observation.timed_out
        && observation.spawn_error.is_empty()
    {
        // Recompute the key against the FRESHLY-BUILT olean. Unlike
        // print-axioms / semantic-payloads (read-only ops run after
        // materialize, so their olean is already stable), `lean-compile-node`
        // PRODUCES the node's olean: the pre-build `key` was computed against
        // the stale/absent olean. Storing under the post-build olean key is
        // what lets the next call hit, and is what makes a content change in
        // the olean self-heal (the stored key tracks the olean it describes).
        if let Some(k) = lean_closure_cache_key(repo_path, node) {
            COMPILE_NODE_CACHE.lock().unwrap().insert(
                (canon_repo.clone(), node.to_string()),
                (k.clone(), observation.clone()),
            );
            // Disk write is fire-and-forget; failures are silent and
            // the slow path runs unchanged on next miss.
            if let Some(disk_dir) = cache_dir_for_namespace(COMPILE_NODE_DISK_NAMESPACE) {
                disk_cache_put(&disk_dir, &disk_key, &k, &observation);
            }
        }
    }
    Ok(observation)
}

pub(crate) fn ensure_worker_checker_support_available(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
) -> Result<trellis_kernel::TabletSupportObservation, String> {
    trellis_kernel::ensure_worker_checker_support_available(repo_path, requested_nodes)
}

pub(crate) fn ensure_worker_checker_oleans_materialized(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
) -> Result<(), String> {
    trellis_kernel::ensure_worker_checker_oleans_materialized(repo_path, requested_nodes)
}

pub(crate) fn sync_tablet_render_support_from_repo(
    repo_path: &Path,
) -> Result<trellis_kernel::TabletSupportObservation, String> {
    trellis_kernel::sync_tablet_render_support_from_repo(repo_path)
}

fn run_print_axioms(repo_path: &Path, node: &str) -> Result<ExternalCommandObservation, String> {
    let canon_repo = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let key = lean_closure_cache_key(repo_path, node);

    // Tier 1 (in-memory).
    if let Some(ref k) = key {
        let cache = PRINT_AXIOMS_CACHE.lock().unwrap();
        if let Some((stored_key, stored_value)) = cache.get(&(canon_repo.clone(), node.to_string()))
        {
            // Same olean-presence guard as `run_compile_node`: only
            // serve from cache when the artefact print-axioms reads
            // (`<node>.olean`) is still on disk.
            if stored_key == k && olean_present(repo_path, node) {
                return Ok(stored_value.clone());
            }
        }
    }

    // Tier 2 (disk). See `run_compile_node` for the multi-dir lookup
    // rationale (writable + optional readonly fallback).
    let disk_key = per_node_disk_lookup_key(&canon_repo, node);
    if let Some(ref k) = key {
        let dirs = cache_lookup_dirs(PRINT_AXIOMS_DISK_NAMESPACE);
        if let Some(stored_value) =
            disk_cache_get_first::<ExternalCommandObservation>(&dirs, &disk_key, k)
        {
            if olean_present(repo_path, node) {
                PRINT_AXIOMS_CACHE.lock().unwrap().insert(
                    (canon_repo.clone(), node.to_string()),
                    (k.clone(), stored_value.clone()),
                );
                return Ok(stored_value);
            }
        }
    }

    // Route per-target (see `run_compile_node`): `Lean` for the live run ⇒
    // byte-identical `[node, repo]` Lean shape; Isabelle `isabelle-thm-oracles`
    // takes the same `[node, repo]` shape.
    let driver = trellis_kernel::backend::checker_driver_for(
        trellis_kernel::tablet_target_for_repo(repo_path),
    );
    let raw = run_repo_command_json(
        repo_path,
        driver.print_axioms_op(),
        &[node.to_string(), repo_path.display().to_string()],
    )?;
    let observation = external_command_from_value(raw, driver.print_axioms_op())?;

    if observation.returncode == Some(0)
        && !observation.timed_out
        && observation.spawn_error.is_empty()
    {
        if let Some(k) = key {
            PRINT_AXIOMS_CACHE.lock().unwrap().insert(
                (canon_repo.clone(), node.to_string()),
                (k.clone(), observation.clone()),
            );
            if let Some(disk_dir) = cache_dir_for_namespace(PRINT_AXIOMS_DISK_NAMESPACE) {
                disk_cache_put(&disk_dir, &disk_key, &k, &observation);
            }
        }
    }
    Ok(observation)
}

/// Patch A wrapper for the local-closure probe (LOCAL_CLOSURE_IMPL_PLAN.md
/// §5.8). Materializes oleans for `node` (and its transitive Tablet
/// imports), invokes `local-closure-axioms` against the checker socket
/// via the existing `run_repo_command_json` transport, and parses the
/// envelope into `LocalClosureProbeOutput`.
///
/// Patch A is purely observational: this function is the entry point
/// that Patch B's pre-auto-fix gate (plan §6.1) and Patch C's recording
/// pass (plan §7.0) will call. It does NOT consult `ProtocolState` and
/// therefore does NOT validate that the Lean constants emitted by the
/// script map to nodes in `present_nodes` (per plan §4.5 the validation
/// "fail closed if a Lean constant cannot be mapped" is Patch C's
/// responsibility, since only Patch C has access to state). Patch A
/// strips the `Tablet.` prefix from each Lean name to obtain the bare
/// Tablet node identifier; non-Tablet names (which the script should
/// never emit for boundary_theorems / strict_*_deps but which we accept
/// defensively) are stored verbatim. The Patch B/C wiring will refine
/// this with present_nodes-aware validation.
///
/// Materialization mirrors the `print_axioms` flow (see `observe_node`
/// at the call site on line ~873): the kernel-side caller invokes
/// `ensure_worker_checker_support_available` immediately before this
/// wrapper. The server-side `_handle_local_closure_axioms` does NOT
/// itself materialize (matching `_handle_print_axioms`), so the
/// precondition is the caller's contract here as well.
///
/// On parse failure or transport error the wrapper still returns a
/// well-formed `LocalClosureProbeOutput` whenever possible (carrying
/// the script's structured error envelope); a transport failure in
/// `run_repo_command_json` itself bubbles up as `Err(String)` for the
/// caller to handle.
pub(crate) fn run_local_closure_axioms(
    repo_path: &Path,
    node: &str,
) -> Result<LocalClosureProbeOutput, String> {
    // Precondition (plan §5.2): oleans for the requested node and its
    // transitive Tablet imports must be on disk before the Lean script
    // can `importModules` them. Mirrors the `print_axioms` callsite
    // pattern in `observe_node`.
    let requested_nodes = BTreeSet::from([NodeId::from(node)]);
    ensure_worker_checker_support_available(repo_path, &requested_nodes)?;

    // Plan §4.6.1 kill-switch: when the bridge config flag
    // `local_closure_axcheck_enabled` is `false`, append `--no-axcheck`
    // to the CLI args so the Lean script skips the secondary collector.
    // The wrapper then accepts the (skipped) cross-check trivially.
    // Default is `true` (run both collectors). Config-read errors are
    // non-fatal: we treat them as "use the default" so a missing config
    // doesn't break the probe. The bridge config lookup uses
    // `repo_path.join("trellis.config.json")` to mirror the existing
    // convention in `bin/runtime_cli.rs:config_path_for_repo`.
    let axcheck_enabled = local_closure_axcheck_enabled_for_repo(repo_path);
    // Route per-target (see `run_compile_node`): `Lean` for the live run ⇒
    // byte-identical Lean shape (`[node]`, +`--no-axcheck` when axcheck off);
    // Isabelle `isabelle-thm-deps` takes `[node]` (axcheck inert — its second
    // collector is the oracle/dep pair, not an axiomatization walk).
    let driver = trellis_kernel::backend::checker_driver_for(
        trellis_kernel::tablet_target_for_repo(repo_path),
    );
    let args = driver.local_closure_args(node, axcheck_enabled);

    // Invoke the checker-socket op via the standard subcommand
    // transport. The CLI dispatches `local-closure-axioms` to the
    // server (see `trellis/atomic_actions/cli.py` line 106-136); the
    // server runs `lean --run scripts/lean_local_closure.lean <node>`
    // under bwrap'd lake (`_handle_local_closure_axioms` in
    // `trellis/checker/server.py`). The response envelope contains
    // both the script's parsed JSON fields and the transport-level
    // `returncode`/`timed_out`/`stdout`/`stderr`.
    let raw = run_repo_command_json(repo_path, driver.local_closure_op(), &args)?;

    parse_local_closure_response(node, raw)
}

/// FIX 2 coverage: run ONLY the owner-file authored-name scan for `node`,
/// regardless of node kind. This is the universal authoring gate that
/// closes the residual where the full closure probe never runs on a node
/// (e.g. a pure Definition-kind node, which `proof_worker_delta_step_result`
/// excludes from `probe_candidates`): such a node could author a
/// `where`/`let rec`-bound reserved-shaped auxiliary (`def Foo … where
/// eq_1 := …` ⇒ `Foo.eq_1`) that the full probe never scans, the Rust
/// `reserved_shaped_authored_components` text backstop cannot see (it does
/// not parse `where` binders), and a consumer's transparent-walk then
/// hides.
///
/// It reuses the EXACT same `where`-aware `ownerFileScanRejection` scan the
/// full probe runs (via the Lean script's `--scan-only` mode), but is
/// decoupled from the closure-record machinery: scan-only emits no
/// dep/axiom keys, so it never enters `validate_probe_present_nodes` or the
/// engine's `classify_record_eligibility` record-install path. A clean node
/// yields `status: "ok"`; a node authoring a reserved-shaped declaration
/// yields `status: "internal_error"` with the diagnostic in `errors[]`.
///
/// Unlike `run_local_closure_axioms`, this does NOT call
/// `ensure_worker_checker_support_available`: the scan parses the node's
/// source against an `Init`-only environment (it never imports the node's
/// own module), so it needs no built oleans and stays cheap (parse-only).
pub(crate) fn run_local_closure_owner_scan(
    repo_path: &Path,
    node: &str,
) -> Result<LocalClosureProbeOutput, String> {
    // `--scan-only` is plumbed through cli.py → checker_client →
    // server.py, which appends it to the Lean script CLI and bypasses the
    // olean-keyed sidecar cache (the scan is parse-only and not
    // olean-keyed). `--no-axcheck` is irrelevant in scan-only mode (no
    // closure walk runs) but harmless; we omit it for clarity.
    // Route per-target (see `run_compile_node`): `Lean` for the live run ⇒
    // byte-identical `[node, --scan-only]` Lean shape; Isabelle
    // `isabelle-thm-deps` accepts+ignores `--scan-only` (whole-theorem cert,
    // no parse-only mode).
    let driver = trellis_kernel::backend::checker_driver_for(
        trellis_kernel::tablet_target_for_repo(repo_path),
    );
    let args = driver.local_closure_scan_args(node);
    let raw = run_repo_command_json(repo_path, driver.local_closure_op(), &args)?;
    parse_local_closure_response(node, raw)
}

// Production declaration-hash callers reach the FILESPEC-marker text
// splitter via `crate::filespec_split::declaration_hash_strict`.

/// Resolve the `local_closure_axcheck_enabled` bridge-config flag for
/// the given repo. Plan §4.6.1: default `true` (run both collectors).
/// Config-read errors (missing file, malformed JSON) fall through to the
/// default so the probe still runs — a missing config is an operator
/// concern but must not silently disable the safety invariant. The
/// repo's `trellis.config.json` is the canonical location; we mirror
/// `config_path_for_repo` in `bin/runtime_cli.rs:1234` for legacy
/// `lagent.config.json` fallback.
fn local_closure_axcheck_enabled_for_repo(repo_path: &Path) -> bool {
    let trellis_path = repo_path.join("trellis.config.json");
    let candidate = if trellis_path.is_file() {
        trellis_path
    } else {
        let legacy = repo_path.join("lagent.config.json");
        if legacy.is_file() {
            legacy
        } else {
            return true;
        }
    };
    resolve_local_closure_axcheck_enabled(&candidate).unwrap_or(true)
}

/// Convert one of the Lean-side name strings (e.g. `"Tablet.Foo"` or
/// just `"Foo"`) into a `NodeId`. Patch A does not have access to
/// `present_nodes` and therefore does not enforce that the result is
/// a kernel-ratified node — that validation lands in Patch C per plan
/// §4.5. The rule here is the simplest correct one for an additive
/// observation: strip a leading `Tablet.` segment if present, otherwise
/// pass the name through verbatim. The script's `isTabletConst` filter
/// already guarantees that boundary_theorems / strict_*_deps entries
/// originate from `Tablet.*` modules (see `lean_local_closure.lean`
/// `isTabletConst`); the verbatim fallback is defensive in case the
/// fail-closed-to-Tablet branch in the script emits a bare name.
fn local_closure_lean_name_to_node_id(name: &str) -> NodeId {
    if let Some(stripped) = name.strip_prefix("Tablet.") {
        NodeId::from(stripped)
    } else {
        NodeId::from(name)
    }
}

/// Parse a single `{"name": "<Lean name>", "<hash field>": "<hex>"}`
/// envelope from the script's per-list output. Returns the `(NodeId,
/// hash)` pair, or `None` if either field is missing / not a string.
/// Caller is responsible for surfacing the dropped entry as an error
/// in the returned `LocalClosureProbeOutput.errors`.
fn parse_local_closure_pair(
    entry: &serde_json::Value,
    hash_field: &str,
) -> Option<(NodeId, String)> {
    let obj = entry.as_object()?;
    let name = obj.get("name").and_then(serde_json::Value::as_str)?;
    let hash = obj.get(hash_field).and_then(serde_json::Value::as_str)?;
    Some((local_closure_lean_name_to_node_id(name), hash.to_string()))
}

fn parse_local_closure_pairs(
    raw: Option<&serde_json::Value>,
    hash_field: &str,
    parse_errors: &mut Vec<String>,
    label: &str,
) -> BTreeMap<NodeId, String> {
    let mut out: BTreeMap<NodeId, String> = BTreeMap::new();
    let Some(value) = raw else {
        return out;
    };
    let Some(arr) = value.as_array() else {
        parse_errors.push(format!(
            "local-closure {label} is not a JSON array (got {:?})",
            value,
        ));
        return out;
    };
    for entry in arr {
        match parse_local_closure_pair(entry, hash_field) {
            Some((node_id, hash)) => {
                out.insert(node_id, hash);
            }
            None => parse_errors.push(format!(
                "local-closure {label} entry malformed (expected \
                 {{\"name\": ..., \"{hash_field}\": ...}}): {entry}",
            )),
        }
    }
    out
}

fn parse_local_closure_response(
    node: &str,
    raw: serde_json::Value,
) -> Result<LocalClosureProbeOutput, String> {
    let obj = raw.as_object().ok_or_else(|| {
        format!("local-closure-axioms response for {node} was not a JSON object: {raw}")
    })?;

    let mut parse_errors: Vec<String> = Vec::new();

    let mut status = obj
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("internal_error")
        .to_string();

    let kernel_axioms: BTreeSet<String> = match obj.get("kernel_axioms") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(other) => {
            parse_errors.push(format!(
                "local-closure kernel_axioms is not a JSON array (got {other:?})"
            ));
            BTreeSet::new()
        }
        None => BTreeSet::new(),
    };

    // Proof-assistant oracles (trusted external decision procedures,
    // distinct from kernel axioms). The Lean checker omits this key, so
    // the `None` arm (empty set) is the only branch Lean ever takes —
    // no-op for Lean, load-bearing for a future Isabelle/HOL checker.
    // Mirror `kernel_axioms`: array of strings; a non-array shape pushes
    // a soft parse-error and yields an empty set.
    let oracles_used: BTreeSet<String> = match obj.get("oracles_used") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(other) => {
            parse_errors.push(format!(
                "local-closure oracles_used is not a JSON array (got {other:?})"
            ));
            BTreeSet::new()
        }
        None => BTreeSet::new(),
    };

    // Isabelle server-injected cert fields (B2c-gate Slice 1). The Lean
    // checker omits all three keys, so the `None` arms (empty set / empty
    // string / `false`) are the only branches Lean ever takes — no-op for
    // Lean, load-bearing for the IsabelleHol accept clauses (Slice 2).
    // `extra_shyps` mirrors `oracles_used`: array of strings; a non-array
    // shape pushes a soft parse-error and yields an empty set.
    let extra_shyps: BTreeSet<String> = match obj.get("extra_shyps") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(other) => {
            parse_errors.push(format!(
                "local-closure extra_shyps is not a JSON array (got {other:?})"
            ));
            BTreeSet::new()
        }
        None => BTreeSet::new(),
    };
    let statement_hash: String = match obj.get("statement_hash") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(other) => {
            parse_errors.push(format!(
                "local-closure statement_hash is not a JSON string (got {other:?})"
            ));
            String::new()
        }
    };
    let theorem_exists: bool = match obj.get("theorem_exists") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Null) | None => false,
        Some(other) => {
            parse_errors.push(format!(
                "local-closure theorem_exists is not a JSON bool (got {other:?})"
            ));
            false
        }
    };

    let boundary_theorems = parse_local_closure_pairs(
        obj.get("boundary_theorems"),
        "statement_hash",
        &mut parse_errors,
        "boundary_theorems",
    );
    let strict_theorem_deps = parse_local_closure_pairs(
        obj.get("strict_theorem_deps"),
        "value_hash",
        &mut parse_errors,
        "strict_theorem_deps",
    );
    let strict_definition_deps = parse_local_closure_pairs(
        obj.get("strict_definition_deps"),
        "semantic_hash",
        &mut parse_errors,
        "strict_definition_deps",
    );

    let mut errors: Vec<String> = match obj.get("errors") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(other) => {
            parse_errors.push(format!(
                "local-closure errors is not a JSON array (got {other:?})"
            ));
            Vec::new()
        }
        None => Vec::new(),
    };
    errors.append(&mut parse_errors);

    let raw_stdout = obj
        .get("stdout")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let raw_stderr = obj
        .get("stderr")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let timed_out = obj
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // `returncode` may be `null` when a transport-level failure means no
    // child process exit was observed (e.g. spawn error). Coerce to 0
    // so `LocalClosureProbeOutput.returncode != 0` remains a reliable
    // signal: callers that need to distinguish must consult the
    // `errors` array, which carries the spawn-error message in such
    // cases (server.py composes it).
    let returncode = obj
        .get("returncode")
        .and_then(serde_json::Value::as_i64)
        .map(|v| v as i32)
        .unwrap_or(0);

    // Plan §4.6.1 axiomization cross-check parsing. `None` indicates the
    // script did not emit the sub-object (pre-merge state file or
    // upstream parse failure); `Some(...)` carries the comparison
    // verdict. On parse failure of the sub-object itself, surface a
    // soft-error and leave the field as `None` — the wrapper enforces
    // the invariant only when the field is `Some` and not `skipped`.
    let axiomization_check = parse_axiomization_check(obj.get("axiomization_check"), &mut errors);

    // Patch C-K Fix 3 (audit MEDIUM): if the Lean script's secondary
    // axcheck collector crashed, the script now emits `axiomization_check
    // { agreed: false, skipped: false, error: "<msg>" }` AND a top-level
    // `errors: ["axiomization_check_crash: ..."]` (replacing the previous
    // silent `skipped: true` degradation). The Rust parser detects the
    // crash by inspecting (a) the typed `error` field on the parsed
    // sub-object OR (b) the script's top-level errors for the
    // `axiomization_check_crash:` prefix. Operator-disabled skip (CLI
    // flag / env var / bridge config kill-switch) still emits
    // `skipped: true` and remains a legitimate pass-through. Implementation
    // crashes are now LOUD: status → internal_error with a distinct
    // diagnostic so the gate's "crash" vs "disagree" arms see different
    // failure classes.
    //
    // Patch C-N item 4: read the typed `axiomization_check.error` field
    // off `axiomization_check` instead of re-keying into the raw JSON
    // — the sub-object parser now populates the typed field, so the
    // raw JSON detour is no longer needed and old fixtures lacking the
    // field deserialize to `None`.
    let axcheck_crash_message: Option<String> = axiomization_check
        .as_ref()
        .and_then(|ax| ax.error.clone())
        .or_else(|| {
            errors.iter().find_map(|e| {
                e.strip_prefix("axiomization_check_crash:")
                    .map(|m| m.trim().to_string())
            })
        });

    // Runtime-invariant enforcement (plan §4.6.1 / §6.2). When the
    // secondary collector ran (`!skipped`) and disagreed with the
    // primary, flip the status to `internal_error` and append a
    // structured error describing the diff. Patch C-Q Q9 removed the
    // MCA gate's dedicated `[internal] axiomization disagrees` arm
    // (it duplicated this parser-side flip); the generic
    // `local.status != "ok"` arm now surfaces the structured `errors[0]`
    // payload to the operator.
    //
    // Patch C-K Fix 3: when `axcheck_crash_message` is `Some`, the
    // `agreed: false, skipped: false` shape is from a collector crash
    // rather than a real disagreement — emit a distinct diagnostic.
    // Per `feedback_fail_loudly_on_dual_check`: "Per-request
    // `internal_error` is for transient failures (provider timeout, JSON
    // parse error). A checker-vs-checker disagreement is the opposite of
    // transient — it's a structural property of the code that will
    // reproduce on every retry." Classify true disagreements as
    // `checker_disagreement` so the caller can halt the run instead of
    // letting the worker bounce through the transient-retry path.
    let mut emit_halt_marker_for_disagreement = false;
    if let Some(ax) = &axiomization_check {
        if !ax.skipped && !ax.agreed {
            if let Some(msg) = &axcheck_crash_message {
                let diag = format!(
                    "axiomization cross-check collector crashed (not a disagreement): {msg}"
                );
                errors.push(diag);
                status = "internal_error".to_string();
            } else {
                let diag = format!(
                    "axiomization cross-check disagrees with primary collector: \
                     primary_only_axioms={:?}, axcheck_only_axioms={:?}, \
                     primary_only_boundaries={:?}, axcheck_only_boundaries={:?}",
                    ax.primary_only_axioms,
                    ax.axcheck_only_axioms,
                    ax.primary_only_boundaries,
                    ax.axcheck_only_boundaries,
                );
                errors.push(diag);
                status = CHECKER_DISAGREEMENT_STATUS.to_string();
                emit_halt_marker_for_disagreement = true;
            }
        }
    }

    let probe = LocalClosureProbeOutput {
        status,
        kernel_axioms,
        oracles_used,
        extra_shyps,
        statement_hash,
        theorem_exists,
        boundary_theorems,
        strict_theorem_deps,
        strict_definition_deps,
        errors,
        raw_stdout,
        raw_stderr,
        returncode,
        timed_out,
        axiomization_check,
    };
    if emit_halt_marker_for_disagreement {
        write_checker_disagreement_halt_marker(node, 0, 0, &probe);
    }
    Ok(probe)
}

/// Resolve the runtime-root directory the supervisor is currently
/// operating against. The `Run` action in `bin/runtime_cli.rs` exports
/// `TRELLIS_KERNEL_CACHE_ROOT` to the runtime root before entering the
/// loop, so every kernel-side subprocess in this run inherits a stable
/// pointer back to it. Returns `None` when the env var is unset (test
/// fixtures, replay tools); callers degrade to in-memory diagnostics.
pub fn checker_disagreement_halt_marker_path() -> Option<PathBuf> {
    let raw = std::env::var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed).join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME))
}

/// True iff a checker-disagreement halt marker is present at the
/// configured runtime-root. Cheap fs::metadata check; safe to invoke
/// every supervisor loop iteration. Returns `false` when no runtime
/// root is configured (replay / test paths).
pub fn checker_disagreement_halt_marker_present() -> bool {
    checker_disagreement_halt_marker_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Persist a halt marker on disagreement. The marker is intentionally
/// self-documenting: an operator opening the JSON sees the per-incident
/// diagnostics AND the `clear_instructions` field that explains how to
/// resume (delete this file).
///
/// Best-effort write: if the runtime root cannot be resolved or the
/// write itself fails, we log to stderr but do NOT propagate the error.
/// The probe's `status = checker_disagreement` is the authoritative halt
/// signal; the marker is a durability / forensics layer on top.
pub fn write_checker_disagreement_halt_marker(
    active_node: &str,
    cycle: u32,
    request_id: u32,
    probe: &LocalClosureProbeOutput,
) {
    let Some(path) = checker_disagreement_halt_marker_path() else {
        eprintln!(
            "trellis: checker disagreement detected for node={active_node} cycle={cycle} \
             request_id={request_id} but TRELLIS_KERNEL_CACHE_ROOT is unset — halt \
             marker not persisted. Probe status: {}",
            probe.status
        );
        return;
    };
    // If a marker already exists from an earlier disagreement, do NOT
    // overwrite — the first disagreement is the load-bearing one and
    // overwriting would lose its diagnostic.
    if path.exists() {
        eprintln!(
            "trellis: checker disagreement halt marker already present at {}; \
             preserving original diagnostic (current node={active_node}).",
            path.display()
        );
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ax = probe.axiomization_check.as_ref();
    let payload = serde_json::json!({
        "kind": "checker_disagreement",
        "schema_version": 1,
        "active_node": active_node,
        "cycle": cycle,
        "request_id": request_id,
        "unix_ts": ts,
        "primary_only_axioms": ax.map(|a| a.primary_only_axioms.clone()).unwrap_or_default(),
        "axcheck_only_axioms": ax.map(|a| a.axcheck_only_axioms.clone()).unwrap_or_default(),
        "primary_only_boundaries": ax.map(|a| a.primary_only_boundaries.clone()).unwrap_or_default(),
        "axcheck_only_boundaries": ax.map(|a| a.axcheck_only_boundaries.clone()).unwrap_or_default(),
        "primary_kernel_axioms": probe.kernel_axioms.iter().cloned().collect::<Vec<_>>(),
        "primary_boundary_theorems": probe
            .boundary_theorems
            .keys()
            .map(|k| k.as_ref().to_string())
            .collect::<Vec<_>>(),
        "axcheck_kernel_axioms": ax
            .map(|a| a.kernel_axioms.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default(),
        "axcheck_boundary_theorems": ax
            .map(|a| a.boundary_theorems.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default(),
        "probe_errors": probe.errors.clone(),
        "probe_status": probe.status.clone(),
        "raw_stdout": probe.raw_stdout.clone(),
        "raw_stderr": probe.raw_stderr.clone(),
        "clear_instructions": format!(
            "The trellis supervisor is HALTED because its dual-collector axiom \
             cross-check reported disagreement on node `{active_node}` at cycle \
             {cycle}. The disagreement is a STRUCTURAL property — retries will \
             reproduce it. Investigate the diff fields above, decide whether the \
             primary or the axcheck collector is correct, fix the underlying \
             Lean / kernel bug, then DELETE this file to resume: \
             `rm {}`. The supervisor will refuse to dispatch new bursts until \
             then.",
            path.display()
        ),
    });
    let body = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, body) {
        Ok(()) => {
            eprintln!(
                "==============================================================\n\
                 trellis: CHECKER DISAGREEMENT — supervisor will HALT.\n\
                 Halt marker persisted at: {}\n\
                 Active node: {active_node}  cycle={cycle}  request_id={request_id}\n\
                 Resume only after operator review + deletion of the marker file.\n\
                 ==============================================================",
                path.display()
            );
        }
        Err(err) => {
            eprintln!(
                "trellis: failed to write checker-disagreement halt marker at {}: \
                 {err}. The probe still rejects (status=checker_disagreement) but \
                 the supervisor's startup halt check has nothing to read.",
                path.display()
            );
        }
    }
}

/// Audit M-3 — outcome of an operator-driven halt-marker
/// acknowledgement. Surfaced from `acknowledge_checker_disagreement_halt_marker`
/// so the runtime CLI can report structured status back to the operator.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum HaltMarkerAckOutcome {
    /// Marker was cleared. `mode` identifies the path:
    ///   * `probe_reagree` — re-ran the probe; both collectors now agree;
    ///     marker safely cleared.
    ///   * `force` — operator passed --force; cleared without verification.
    ///   * `no_marker_present` — nothing to clear; logged the ack anyway.
    Cleared { mode: String, history_path: PathBuf },
    /// Marker was NOT cleared. Operator must investigate further or use
    /// `--force` to override.
    Refused {
        reason: String,
        history_path: PathBuf,
    },
}

/// Audit M-3 — runtime-root path for the structured halt-acknowledgement
/// history file. One line per ack attempt (cleared OR refused), so an
/// operator can audit who/when forced past a halt.
pub fn halt_history_path() -> Option<PathBuf> {
    let raw = std::env::var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(trimmed)
            .join("halt_history")
            .join("ack_log.jsonl"),
    )
}

/// Audit M-3 — controlled clear path for the checker-disagreement halt
/// marker. Operators previously had only `rm` available; this helper
/// adds an auditable workflow:
///   * Logs every ack attempt (success or refuse) to
///     `<runtime_root>/halt_history/ack_log.jsonl` with timestamp +
///     operator-supplied reason.
///   * Without `--force`, requires the operator to either supply a
///     `probe_result` whose `axiomization_check.agreed == true` (the
///     supervisor re-ran the probe and it now agrees) OR explicitly
///     opt out via `--force`.
///   * With `--force`, clears unconditionally; the history line records
///     `mode = "force"` so the operator action is auditable.
///   * Refuses if a probe was supplied AND it still disagrees.
///
/// Concurrent ack attempts are serialized only insofar as the
/// filesystem operations are: the marker file's `rename` is atomic
/// (we move it to a `.acked` suffix rather than `rm` so the diagnostic
/// content survives in a separate file an operator can grep later).
pub fn acknowledge_checker_disagreement_halt_marker(
    reason: &str,
    force: bool,
    probe_result: Option<&LocalClosureProbeOutput>,
) -> Result<HaltMarkerAckOutcome, String> {
    let Some(marker_path) = checker_disagreement_halt_marker_path() else {
        return Err(
            "TRELLIS_KERNEL_CACHE_ROOT is unset — cannot resolve halt marker path".to_string(),
        );
    };
    let Some(history_path) = halt_history_path() else {
        return Err(
            "TRELLIS_KERNEL_CACHE_ROOT is unset — cannot resolve halt history path".to_string(),
        );
    };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Determine the ack mode and whether to proceed with clear.
    let (mode, cleared, refusal_reason) = if !marker_path.exists() {
        ("no_marker_present".to_string(), true, None)
    } else if force {
        ("force".to_string(), true, None)
    } else if let Some(probe) = probe_result {
        // Re-ran the probe; check if it now agrees.
        let ax_agrees = match probe.axiomization_check.as_ref() {
            Some(ax) => ax.agreed && !ax.skipped,
            None => false,
        };
        let probe_ok = probe.status == "ok" && probe.errors.is_empty();
        if ax_agrees && probe_ok {
            ("probe_reagree".to_string(), true, None)
        } else {
            (
                "probe_still_disagrees".to_string(),
                false,
                Some(format!(
                    "supplied probe result still disagrees (axcheck.agreed={}, axcheck.skipped={}, \
                     probe.status={:?}, probe.errors={:?})",
                    probe.axiomization_check.as_ref().map(|a| a.agreed).unwrap_or(false),
                    probe.axiomization_check.as_ref().map(|a| a.skipped).unwrap_or(false),
                    probe.status,
                    probe.errors,
                )),
            )
        }
    } else {
        // No probe supplied AND not forcing — refuse and tell the operator
        // their options.
        (
            "no_evidence".to_string(),
            false,
            Some(
                "no evidence supplied to clear the halt: re-run the probe (via the trellis CLI's \
                 local-closure-axioms subcommand) and pass the result, or use --force to override"
                    .to_string(),
            ),
        )
    };

    // Always log the ack attempt to history. Best-effort: if log write
    // fails, surface a stderr warning but proceed with the clear/refuse
    // decision (the history layer is forensic, not authoritative).
    let history_record = serde_json::json!({
        "schema_version": 1,
        "kind": "checker_disagreement_ack",
        "unix_ts": ts,
        "reason": reason,
        "mode": mode,
        "marker_path": marker_path.display().to_string(),
        "force": force,
        "probe_supplied": probe_result.is_some(),
        "cleared": cleared,
        "refusal_reason": refusal_reason,
    });
    let history_line =
        serde_json::to_string(&history_record).unwrap_or_else(|_| history_record.to_string());
    if let Some(parent) = history_path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            eprintln!(
                "trellis: failed to create halt history dir {}: {err}",
                parent.display()
            );
        }
    }
    if let Err(err) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{}", history_line)
        })
    {
        eprintln!(
            "trellis: failed to append halt history at {}: {err}",
            history_path.display()
        );
    }

    if cleared {
        if marker_path.exists() {
            // Preserve the diagnostic content by renaming the marker
            // to `.acked-<ts>.json` rather than `rm`. Operators can
            // recover the original payload from disk if needed.
            let acked_name = format!(
                "{}.acked-{}.json",
                marker_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("checker_disagreement_halt"),
                ts
            );
            let acked_path = marker_path
                .parent()
                .map(|p| p.join(acked_name))
                .unwrap_or_else(|| PathBuf::from("checker_disagreement_halt.acked.json"));
            if let Err(err) = std::fs::rename(&marker_path, &acked_path) {
                // Renaming failed; fall back to a plain remove so the
                // marker doesn't permanently block the supervisor.
                eprintln!(
                    "trellis: failed to rename halt marker {} → {}: {err}; falling back to rm",
                    marker_path.display(),
                    acked_path.display(),
                );
                if let Err(rm_err) = std::fs::remove_file(&marker_path) {
                    if rm_err.kind() != std::io::ErrorKind::NotFound {
                        return Err(format!(
                            "failed to clear halt marker at {}: {rm_err}",
                            marker_path.display()
                        ));
                    }
                }
            } else {
                eprintln!(
                    "trellis: halt marker acknowledged (mode={mode}); preserved as {}",
                    acked_path.display()
                );
            }
        }
        Ok(HaltMarkerAckOutcome::Cleared { mode, history_path })
    } else {
        Ok(HaltMarkerAckOutcome::Refused {
            reason: refusal_reason.unwrap_or_else(|| "no specific reason recorded".to_string()),
            history_path,
        })
    }
}

/// Resolve the runtime-root path for the system-feedback halt marker.
/// Same env-var lookup pattern as `checker_disagreement_halt_marker_path`;
/// see that helper's docstring for the rationale.
pub fn system_feedback_halt_marker_path() -> Option<PathBuf> {
    let raw = std::env::var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed).join(SYSTEM_FEEDBACK_HALT_MARKER_FILENAME))
}

/// True iff a system-feedback halt marker is present at the configured
/// runtime-root. Cheap fs::metadata check; safe to invoke every
/// supervisor loop iteration. Returns `false` when no runtime root is
/// configured (replay / test paths).
pub fn system_feedback_halt_marker_present() -> bool {
    system_feedback_halt_marker_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// True iff EITHER halt marker (checker-disagreement OR system-feedback)
/// is present. Hot-path helper used by the supervisor `Run` loop to gate
/// dispatch on a single check.
pub fn any_halt_marker_present() -> bool {
    checker_disagreement_halt_marker_present() || system_feedback_halt_marker_present()
}

/// Normalize a raw `system_feedback` string into a cycle-STABLE form for
/// fingerprinting. Reviewer/agent feedback that flags a recurring design
/// gap re-references the specific node, artifact, symbol, and counts of
/// the cycle that produced it — all of which vary cycle-to-cycle for the
/// SAME underlying gap-class, so a raw-text hash would never dedup. We
/// strip exactly the structurally-variable tokens:
///   * backtick-delimited spans (`` `Node` ``, `` `path/to/x.json` ``,
///     `` `lemma_foo` ``) → the sentinel `<id>`. Trellis prose quotes
///     every node / artifact / symbol identifier in backticks, so this
///     removes the per-cycle identifiers without touching the English
///     description of the gap.
///   * runs of ASCII digits → the sentinel `<n>` — cycle numbers,
///     request ids, counts ("3 nodes", "cycle 809", "12 targets").
/// then lowercases and collapses whitespace. The residue is the stable
/// prose describing the gap-class, identical across recurrences.
fn normalize_system_feedback(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            // Consume through the closing backtick (or end-of-string).
            while let Some(nc) = chars.next() {
                if nc == '`' {
                    break;
                }
            }
            out.push_str(" <id> ");
        } else if c.is_ascii_digit() {
            while let Some(&nc) = chars.peek() {
                if nc.is_ascii_digit() {
                    chars.next();
                } else {
                    break;
                }
            }
            out.push_str(" <n> ");
        } else {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
        }
    }
    // Collapse all whitespace runs to single spaces and trim.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Compute the STABLE fingerprint for a `system_feedback` emission. This
/// is the key the ack store is indexed by, so it MUST be identical across
/// cycles for the same gap-class and differ for genuinely different gaps.
///
/// Fingerprint definition (versioned `v1`):
///   `sha256( "sffp:v1" ␟ request_kind ␟ burst_role ␟ normalize(text) )`
/// where `␟` is the ASCII unit separator (`0x1F`) and `normalize` is
/// [`normalize_system_feedback`] (backtick-spans → `<id>`, digit-runs →
/// `<n>`, lowercased, whitespace-collapsed).
///
/// `request_kind` + `burst_role` are structural, cycle-stable emission
/// attributes; folding them in means "the same words from a reviewer vs.
/// a worker" are distinct ack classes without over-collapsing. `cycle`,
/// `request_id`, `active_node`, `lane`, and `artifact` are deliberately
/// EXCLUDED — they change every cycle for the same gap and would defeat
/// dedup.
pub fn system_feedback_fingerprint(
    request_kind: &str,
    burst_role: &str,
    system_feedback: &str,
) -> String {
    let normalized = normalize_system_feedback(system_feedback);
    let material = format!(
        "sffp:v1\u{1f}{request_kind}\u{1f}{burst_role}\u{1f}{normalized}"
    );
    let digest = Sha256::digest(material.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A single operator-recorded acknowledgment of a `system_feedback`
/// fingerprint. Persisted inside [`SystemFeedbackAckStore`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemFeedbackAck {
    /// The stable fingerprint (see [`system_feedback_fingerprint`]).
    pub fingerprint: String,
    /// Free-text operator justification for the ack.
    pub reason: String,
    /// Unix seconds when the ack was recorded.
    pub unix_ts: u64,
    /// A verbatim sample of the feedback text at ack time, so a later
    /// reviewer can see what class of gap this fingerprint stands for.
    pub sample_feedback: String,
}

/// Restart-surviving set of acknowledged `system_feedback` fingerprints.
/// Serialized as pretty JSON at
/// `<runtime_root>/system_feedback_acks.json`. Mirrors the
/// runtime-root-relative, JSON-on-disk persistence convention the
/// checker-disagreement halt machinery uses. Keyed by fingerprint so
/// membership is O(log n) and acks are strictly PER-FINGERPRINT (never
/// global).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemFeedbackAckStore {
    #[serde(default = "system_feedback_ack_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub acknowledged: BTreeMap<String, SystemFeedbackAck>,
}

fn system_feedback_ack_schema_version() -> u32 {
    1
}

impl Default for SystemFeedbackAckStore {
    fn default() -> Self {
        Self {
            schema_version: system_feedback_ack_schema_version(),
            acknowledged: BTreeMap::new(),
        }
    }
}

impl SystemFeedbackAckStore {
    /// True iff `fingerprint` has been acknowledged by an operator.
    pub fn is_acknowledged(&self, fingerprint: &str) -> bool {
        self.acknowledged.contains_key(fingerprint)
    }
}

/// Runtime-root path of the acknowledged-fingerprint store. Same env-var
/// lookup pattern as [`system_feedback_halt_marker_path`]; `None` when no
/// runtime root is configured (replay / test fixtures) — in which case
/// callers degrade to an empty (fail-closed) store.
pub fn system_feedback_ack_store_path() -> Option<PathBuf> {
    let raw = std::env::var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed).join(SYSTEM_FEEDBACK_ACK_STORE_FILENAME))
}

/// Runtime-root path of the unified system-feedback audit log (JSONL).
pub fn system_feedback_log_path() -> Option<PathBuf> {
    let raw = std::env::var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed).join(SYSTEM_FEEDBACK_LOG_FILENAME))
}

/// Resolve whether a non-empty `system_feedback` emission should HALT the
/// run (write the sticky halt marker) or log-and-continue.
///
/// Resolution order:
/// 1. Env `TRELLIS_SYSTEM_FEEDBACK_HALT` — `1`/`true` ⇒ halt, `0`/`false`
///    ⇒ no halt (case-insensitive). Takes precedence over config in BOTH
///    directions so launchers can flip a run without editing its config.
///    Any other non-empty value is malformed ⇒ loud stderr diagnostic and
///    HALT-ENABLED (conservative: unreadable operator intent resolves to
///    the fail-loudly behavior, which is itself the loud failure).
/// 2. Top-level `system_feedback_halt` bool in `trellis.config.json`
///    (lazy-parsed per call, mirroring the `*_from_config` readers).
///    Missing file / absent key ⇒ FALSE (the shipped default is
///    log-and-continue). Unparseable file or a non-bool value ⇒ loud
///    stderr diagnostic and HALT-ENABLED, same rationale as above.
///
/// NOTE the asymmetry with the marker CHECKS: the run loop and bridge
/// halt on a marker that is already on disk regardless of this knob — a
/// pre-existing marker means an operator-era halt that only operator
/// deletion clears.
pub fn system_feedback_halt_enabled(config_path: Option<&Path>) -> bool {
    if let Ok(raw) = std::env::var(SYSTEM_FEEDBACK_HALT_ENV) {
        let value = raw.trim().to_ascii_lowercase();
        match value.as_str() {
            "" => {}
            "1" | "true" => return true,
            "0" | "false" => return false,
            other => {
                eprintln!(
                    "trellis: malformed {SYSTEM_FEEDBACK_HALT_ENV}={other:?} (expected 1/0/true/false); \
                     treating system_feedback halting as ENABLED (conservative)."
                );
                return true;
            }
        }
    }
    let Some(config_path) = config_path else {
        return false;
    };
    if !config_path.exists() {
        return false;
    }
    let text = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!(
                "trellis: failed to read config {} while resolving system_feedback_halt: {err}; \
                 treating system_feedback halting as ENABLED (conservative).",
                config_path.display()
            );
            return true;
        }
    };
    let raw: serde_json::Value = match serde_json::from_str(&text) {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!(
                "trellis: failed to parse config {} while resolving system_feedback_halt: {err}; \
                 treating system_feedback halting as ENABLED (conservative).",
                config_path.display()
            );
            return true;
        }
    };
    match raw.as_object().and_then(|obj| obj.get("system_feedback_halt")) {
        None => false,
        Some(serde_json::Value::Bool(value)) => *value,
        Some(other) => {
            eprintln!(
                "trellis: config {} has non-bool system_feedback_halt {other:?}; \
                 treating system_feedback halting as ENABLED (conservative).",
                config_path.display()
            );
            true
        }
    }
}

/// Load the acknowledged-fingerprint store from disk. FAIL-CLOSED: any
/// resolution / read / parse failure yields the empty default store, so
/// the ack policy can only ever SUPPRESS a halt for a fingerprint an
/// operator explicitly and parseably recorded — never accidentally widen
/// suppression. With no store on disk (the default state of every run),
/// this returns the empty set and every emission still halts exactly as
/// today.
pub fn load_system_feedback_ack_store() -> SystemFeedbackAckStore {
    let Some(path) = system_feedback_ack_store_path() else {
        return SystemFeedbackAckStore::default();
    };
    if !path.exists() {
        return SystemFeedbackAckStore::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<SystemFeedbackAckStore>(&raw).unwrap_or_else(|err| {
            eprintln!(
                "trellis: failed to parse system_feedback ack store at {}: {err}; \
                 treating as EMPTY (fail-closed — emissions still halt).",
                path.display()
            );
            SystemFeedbackAckStore::default()
        }),
        Err(err) => {
            eprintln!(
                "trellis: failed to read system_feedback ack store at {}: {err}; \
                 treating as EMPTY (fail-closed — emissions still halt).",
                path.display()
            );
            SystemFeedbackAckStore::default()
        }
    }
}

/// Append one JSONL record to the unified system-feedback log. Called
/// for EVERY non-empty emission — `acked` marks operator-acknowledged
/// (halt-suppressed) recurrences, `halted` marks emissions that occurred
/// under halt-enabled mode and left the run halting. Best-effort: a
/// failed write is logged to stderr but never propagated (the log is a
/// forensic layer, not an authoritative one).
#[allow(clippy::too_many_arguments)]
fn append_system_feedback_log(
    active_node: &str,
    active_coarse_node: &str,
    cycle: u32,
    request_id: u32,
    request_kind: &str,
    burst_role: &str,
    lane: &str,
    artifact: &str,
    system_feedback: &str,
    fingerprint: &str,
    acked: bool,
    halted: bool,
) {
    let Some(path) = system_feedback_log_path() else {
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = serde_json::json!({
        "schema_version": 1,
        "kind": "system_feedback",
        "unix_ts": ts,
        "fingerprint": fingerprint,
        "active_node": active_node,
        "active_coarse_node": active_coarse_node,
        "cycle": cycle,
        "request_id": request_id,
        "request_kind": request_kind,
        "burst_role": burst_role,
        "lane": lane,
        "artifact": artifact,
        "system_feedback": system_feedback,
        "acked": acked,
        "halted": halted,
    });
    let line = serde_json::to_string(&record).unwrap_or_else(|_| record.to_string());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{line}")
        })
    {
        eprintln!(
            "trellis: failed to append system_feedback log at {}: {err}",
            path.display()
        );
    }
}

/// Structured outcome of an operator acknowledging a `system_feedback`
/// fingerprint. Surfaced through the runtime CLI so scripts / operators
/// can confirm the ack landed.
#[derive(Debug, Clone, Serialize)]
pub struct SystemFeedbackAckResult {
    pub fingerprint: String,
    /// `true` if this fingerprint was not previously in the store.
    pub newly_added: bool,
    /// Total acknowledged fingerprints after this ack.
    pub total_acknowledged: usize,
    pub store_path: PathBuf,
    /// `true` if a currently-present halt marker matched this fingerprint
    /// and was cleared (renamed to `.acked-<ts>.json`) so the run resumes.
    pub cleared_active_halt: bool,
}

/// Operator affordance — acknowledge a `system_feedback` fingerprint so
/// its RECURRENCES log-and-continue instead of halting. NEVER auto-called;
/// only ever reached from the `acknowledge-system-feedback` runtime CLI
/// subcommand, i.e. a human action. Mirrors
/// [`acknowledge_checker_disagreement_halt_marker`]: it persists under the
/// runtime root, logs the attempt to the shared halt-history audit log,
/// and, when the fingerprint matches the currently-present halt marker,
/// preserves that marker as `.acked-<ts>.json` (rather than `rm`) so the
/// diagnostic survives while the run resumes.
pub fn acknowledge_system_feedback_fingerprint(
    fingerprint: &str,
    reason: &str,
    sample_feedback: &str,
) -> Result<SystemFeedbackAckResult, String> {
    let fingerprint = fingerprint.trim();
    if fingerprint.is_empty() {
        return Err("refusing to acknowledge an empty fingerprint".to_string());
    }
    let Some(store_path) = system_feedback_ack_store_path() else {
        return Err(
            "TRELLIS_KERNEL_CACHE_ROOT is unset — cannot resolve system_feedback ack store path"
                .to_string(),
        );
    };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut store = load_system_feedback_ack_store();
    let newly_added = store
        .acknowledged
        .insert(
            fingerprint.to_string(),
            SystemFeedbackAck {
                fingerprint: fingerprint.to_string(),
                reason: reason.to_string(),
                unix_ts: ts,
                sample_feedback: sample_feedback.to_string(),
            },
        )
        .is_none();

    let body = serde_json::to_string_pretty(&store)
        .map_err(|err| format!("failed to serialize system_feedback ack store: {err}"))?;
    if let Some(parent) = store_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&store_path, body)
        .map_err(|err| format!("failed to persist system_feedback ack store at {}: {err}", store_path.display()))?;

    // Audit: append to the shared halt-history log (mirrors the
    // checker-disagreement ack). Best-effort.
    if let Some(history_path) = halt_history_path() {
        let history_record = serde_json::json!({
            "schema_version": 1,
            "kind": "system_feedback_ack",
            "unix_ts": ts,
            "fingerprint": fingerprint,
            "reason": reason,
            "newly_added": newly_added,
            "store_path": store_path.display().to_string(),
        });
        let history_line =
            serde_json::to_string(&history_record).unwrap_or_else(|_| history_record.to_string());
        if let Some(parent) = history_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(err) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&history_path)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "{history_line}")
            })
        {
            eprintln!(
                "trellis: failed to append halt history at {}: {err}",
                history_path.display()
            );
        }
    }

    // If the currently-present halt marker was raised by THIS fingerprint,
    // clear it (preserve-as-`.acked`) so the operator's ack also resumes
    // the in-progress halt. Only clears on an exact fingerprint match —
    // an unrelated halt is left untouched.
    let mut cleared_active_halt = false;
    if let Some(marker_path) = system_feedback_halt_marker_path() {
        if marker_path.exists() {
            let marker_fp = std::fs::read_to_string(&marker_path)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|v| {
                    v.get("fingerprint")
                        .and_then(|f| f.as_str())
                        .map(str::to_string)
                });
            if marker_fp.as_deref() == Some(fingerprint) {
                let acked_name = format!(
                    "{}.acked-{}.json",
                    marker_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("system_feedback_halt"),
                    ts
                );
                let acked_path = marker_path
                    .parent()
                    .map(|p| p.join(acked_name))
                    .unwrap_or_else(|| PathBuf::from("system_feedback_halt.acked.json"));
                match std::fs::rename(&marker_path, &acked_path) {
                    Ok(()) => {
                        cleared_active_halt = true;
                        eprintln!(
                            "trellis: system_feedback halt marker acknowledged (fingerprint={fingerprint}); \
                             preserved as {}",
                            acked_path.display()
                        );
                    }
                    Err(err) => {
                        eprintln!(
                            "trellis: failed to rename system_feedback halt marker {} → {}: {err}",
                            marker_path.display(),
                            acked_path.display()
                        );
                    }
                }
            }
        }
    }

    let total_acknowledged = store.acknowledged.len();
    Ok(SystemFeedbackAckResult {
        fingerprint: fingerprint.to_string(),
        newly_added,
        total_acknowledged,
        store_path,
        cleared_active_halt,
    })
}

/// Record a non-empty `system_feedback` emission: ALWAYS append one row
/// to the unified `system_feedback_log.jsonl`, and additionally persist
/// the sticky halt marker when halting is enabled (opt-in via config
/// `system_feedback_halt` / env `TRELLIS_SYSTEM_FEEDBACK_HALT` — see
/// [`system_feedback_halt_enabled`]; `config_path` is the run's
/// `trellis.config.json`, `None` ⇒ env/default resolution only). Marker
/// shape + preservation semantics mirror
/// `write_checker_disagreement_halt_marker` but at a distinct path so
/// the two halt causes don't clobber each other.
///
/// Default (knob absent): log-and-continue — no marker, a clearly
/// visible stderr notice, and the run proceeds. Opt-in (knob true): the
/// historical fail-loudly behavior, byte-identical marker + banner, plus
/// the log row. An operator-acknowledged fingerprint log-and-continues
/// in BOTH modes.
///
/// Best-effort write: if the runtime root cannot be resolved or the
/// write itself fails, log to stderr but do NOT propagate the error.
/// The supervisor's bridge-side halt check also reads the same path, so
/// once the marker lands the next dispatch is refused.
#[allow(clippy::too_many_arguments)]
pub fn write_system_feedback_halt_marker(
    config_path: Option<&Path>,
    active_node: &str,
    active_coarse_node: &str,
    cycle: u32,
    request_id: u32,
    request_kind: &str,
    burst_role: &str,
    lane: &str,
    artifact: &str,
    system_feedback: &str,
    reason: &str,
) {
    // Stable fingerprint for this emission's gap-class (see
    // `system_feedback_fingerprint`). Computed unconditionally so it can
    // be recorded in the halt marker (novel case) AND matched against the
    // ack store (recurrence case).
    let fingerprint = system_feedback_fingerprint(request_kind, burst_role, system_feedback);

    // CONSERVATIVE ack policy — can only fire when an operator has
    // explicitly acknowledged THIS fingerprint. With the default (no ack
    // store on disk / empty store), `is_acknowledged` always returns
    // false. An acked fingerprint log-and-continues in BOTH halt modes:
    // one unified log row, no marker.
    let ack_store = load_system_feedback_ack_store();
    if ack_store.is_acknowledged(&fingerprint) {
        append_system_feedback_log(
            active_node,
            active_coarse_node,
            cycle,
            request_id,
            request_kind,
            burst_role,
            lane,
            artifact,
            system_feedback,
            &fingerprint,
            true,
            false,
        );
        eprintln!(
            "trellis: system_feedback fingerprint {fingerprint} is ACKNOWLEDGED — \
             recording to {SYSTEM_FEEDBACK_LOG_FILENAME} and CONTINUING (no halt). \
             node={active_node} cycle={cycle} request_id={request_id} kind={request_kind}."
        );
        return;
    }

    let halt_enabled = system_feedback_halt_enabled(config_path);
    // One durable stream regardless of mode: the unified log row lands
    // before any marker bookkeeping, so halt-mode re-emissions (marker
    // already present) are recorded too.
    append_system_feedback_log(
        active_node,
        active_coarse_node,
        cycle,
        request_id,
        request_kind,
        burst_role,
        lane,
        artifact,
        system_feedback,
        &fingerprint,
        false,
        halt_enabled,
    );
    if !halt_enabled {
        eprintln!(
            "trellis: SYSTEM_FEEDBACK EMITTED (log-and-continue): an agent burst returned a \
             non-empty system_feedback string on request_id={request_id} (kind={request_kind}, \
             role={burst_role}, node={active_node}, cycle={cycle}). The full text and stable \
             fingerprint {fingerprint} were appended to <runtime>/{SYSTEM_FEEDBACK_LOG_FILENAME}; \
             the run CONTINUES. system_feedback usually signals a design gap or harness bug worth \
             operator review; to make future emissions halt the run instead, set top-level \
             \"system_feedback_halt\": true in trellis.config.json (or {SYSTEM_FEEDBACK_HALT_ENV}=1). \
             Feedback text: {system_feedback}"
        );
        return;
    }

    let Some(path) = system_feedback_halt_marker_path() else {
        eprintln!(
            "trellis: system_feedback emitted (node={active_node} cycle={cycle} \
             request_id={request_id} kind={request_kind}) but \
             TRELLIS_KERNEL_CACHE_ROOT is unset — halt marker not persisted. \
             Feedback text: {system_feedback} (fingerprint={fingerprint})"
        );
        return;
    };
    if path.exists() {
        eprintln!(
            "trellis: system_feedback halt marker already present at {}; \
             preserving original diagnostic (current node={active_node}).",
            path.display()
        );
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let payload = serde_json::json!({
        "kind": "system_feedback",
        "schema_version": 1,
        "active_node": active_node,
        "active_coarse_node": active_coarse_node,
        "cycle": cycle,
        "request_id": request_id,
        "request_kind": request_kind,
        "burst_role": burst_role,
        "lane": lane,
        "artifact": artifact,
        "system_feedback": system_feedback,
        "reason": reason,
        "unix_ts": ts,
        "fingerprint": fingerprint,
        "clear_instructions": format!(
            "The trellis supervisor is HALTED because an agent burst returned \
             a non-empty `system_feedback` string on request_id={request_id} \
             (kind={request_kind}, node={active_node}, cycle={cycle}). Every \
             system_feedback emission is treated as a design-gap signal that \
             requires human inspection — the supervisor will not dispatch new \
             bursts until you review the `system_feedback` field above and \
             then DELETE this file to resume: `rm {}`. \
             If this is a KNOWN design gap you want the supervisor to log-and-continue \
             on future recurrences (instead of halting each cycle), acknowledge its \
             fingerprint `{fingerprint}` by piping a JSON request to the runtime CLI \
             on stdin: `{{\"action\":\"ack_system_feedback\",\"root\":\"<runtime_root>\",\"fingerprint\":\"{fingerprint}\",\"reason\":\"<why>\"}}` \
             (records it in `{}` and resumes this halt). Acks are PER-FINGERPRINT; any NOVEL \
             feedback still halts. Halting on system_feedback is OPT-IN: this run enabled it via \
             the top-level `system_feedback_halt` knob in trellis.config.json (or \
             {SYSTEM_FEEDBACK_HALT_ENV}=1); with the knob off (the default) emissions are \
             appended to `{SYSTEM_FEEDBACK_LOG_FILENAME}` and the run continues.",
            path.display(),
            SYSTEM_FEEDBACK_ACK_STORE_FILENAME,
        ),
    });
    let body = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, body) {
        Ok(()) => {
            eprintln!(
                "==============================================================\n\
                 trellis: SYSTEM_FEEDBACK EMITTED — supervisor will HALT.\n\
                 Halt marker persisted at: {}\n\
                 Active node: {active_node}  cycle={cycle}  request_id={request_id}  kind={request_kind}\n\
                 Resume only after operator review + deletion of the marker file.\n\
                 ==============================================================",
                path.display()
            );
        }
        Err(err) => {
            eprintln!(
                "trellis: failed to write system-feedback halt marker at {}: {err}.",
                path.display()
            );
        }
    }
}

/// Parse the script's `axiomization_check` sub-object into an
/// `AxiomizationCheckOutput`. Returns `None` when the field is absent
/// (pre-merge state files). Returns `Some` even when the parse partial
/// fails — the wrapper's invariant enforcement only fires on
/// `Some(agreed: false, skipped: false)` so a malformed sub-object
/// degrades gracefully (the primary's verdict stands and the parse
/// errors are surfaced in the outer `errors` array). Plan §4.6.1.
fn parse_axiomization_check(
    raw: Option<&serde_json::Value>,
    errors: &mut Vec<String>,
) -> Option<AxiomizationCheckOutput> {
    let value = raw?;
    let obj = match value.as_object() {
        Some(obj) => obj,
        None => {
            errors.push(format!(
                "local-closure axiomization_check is not a JSON object (got {value:?})"
            ));
            return None;
        }
    };

    let parse_str_set = |field: &str, errs: &mut Vec<String>| -> BTreeSet<String> {
        match obj.get(field) {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(other) => {
                errs.push(format!(
                    "local-closure axiomization_check.{field} is not a JSON array (got {other:?})"
                ));
                BTreeSet::new()
            }
            None => BTreeSet::new(),
        }
    };
    let parse_str_vec = |field: &str, errs: &mut Vec<String>| -> Vec<String> {
        match obj.get(field) {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            Some(other) => {
                errs.push(format!(
                    "local-closure axiomization_check.{field} is not a JSON array (got {other:?})"
                ));
                Vec::new()
            }
            None => Vec::new(),
        }
    };

    let kernel_axioms = parse_str_set("kernel_axioms", errors);
    let boundary_theorems = parse_str_set("boundary_theorems", errors);
    // `agreed` defaults to `true` only when paired with `skipped: true`;
    // for a populated sub-object without an explicit `agreed` we default
    // to `false` so a malformed envelope does not silently bypass the
    // invariant. The script always emits `agreed` explicitly, so this
    // default-false fires only on a malformed payload.
    let skipped = obj
        .get("skipped")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let agreed = obj
        .get("agreed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(skipped);
    let primary_only_axioms = parse_str_vec("primary_only_axioms", errors);
    let axcheck_only_axioms = parse_str_vec("axcheck_only_axioms", errors);
    let primary_only_boundaries = parse_str_vec("primary_only_boundaries", errors);
    let axcheck_only_boundaries = parse_str_vec("axcheck_only_boundaries", errors);
    // Patch C-N item 4: parse the typed `error` field. When the
    // secondary collector crashed, the Lean script emits the exception
    // message here so the Rust wrapper can distinguish crash from real
    // disagreement WITHOUT keying off the raw JSON. Empty string and
    // missing both deserialize to `None` (no crash signal).
    let error = obj
        .get("error")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Some(AxiomizationCheckOutput {
        kernel_axioms,
        boundary_theorems,
        agreed,
        skipped,
        primary_only_axioms,
        axcheck_only_axioms,
        primary_only_boundaries,
        axcheck_only_boundaries,
        error,
    })
}

/// Patch C-K Fix 1 (audit MEDIUM-HIGH): validate that every dep name
/// emitted by the Lean local-closure probe maps to a kernel-ratified
/// `present_node`. The probe parser strips `Tablet.` from dep names but
/// does not validate that the resulting NodeId is actually present in
/// the kernel state — so a Lean constant the script tagged as Tablet
/// but the kernel never ratified (e.g. a Preamble-internal helper, a
/// stale Tablet declaration name, or any unmappable artifact) could
/// become a record dependency key. Such a key is not tied to kernel
/// node lifecycle / invalidation semantics, so the record could be
/// falsely treated as fresh.
///
/// The fix: AFTER parsing, scan every `boundary_theorems` /
/// `strict_theorem_deps` / `strict_definition_deps` key. If ANY key is
/// not in `present_nodes`, fail closed: flip `probe.status` to
/// `internal_error` and append a structured diagnostic to `probe.errors`
/// naming the unmappable dep(s). Mutates `probe` in place so the caller
/// can re-use the parsed payload (the engine's
/// `apply_local_closure_acceptance_bookkeeping` checks
/// `probe.status == "ok"` before installing a record; an internal_error
/// status blocks the install).
///
/// Patch C-N item 1: extended to also validate dep KIND against
/// `node_kinds`. Each dep map has an expected kind:
///   * `boundary_theorems`       → `NodeKind::Proof`
///   * `strict_theorem_deps`     → `NodeKind::Proof`
///   * `strict_definition_deps`  → `NodeKind::Definition`
///     (plus the import-root `Preamble` when recorded as
///     `NodeKind::Preamble`)
/// A dep whose recorded kind in `node_kinds` doesn't match its expected
/// kind is treated the same as the unmappable-present-node case: fail
/// closed, flip status to `internal_error`, append a diagnostic. Deps
/// that are present-node-mapped but absent from `node_kinds` are
/// considered a kind mismatch (deps without a kind can't be validated
/// and would otherwise let a kind-confused entry through). Membership
/// is validated first; kind validation runs only over deps that ARE in
/// `present_nodes` (avoids double-counting in the same diagnostic).
pub(crate) fn validate_probe_present_nodes(
    probe: &mut LocalClosureProbeOutput,
    present_nodes: &BTreeSet<NodeId>,
    node_kinds: &BTreeMap<NodeId, crate::NodeKind>,
) {
    fn collect_unmappable(
        map: &BTreeMap<NodeId, String>,
        label: &str,
        present_nodes: &BTreeSet<NodeId>,
        out: &mut Vec<String>,
        private_aux: &mut Vec<String>,
    ) {
        for key in map.keys() {
            if !present_nodes.contains(key) {
                // A dotted dep whose leading component IS a present node
                // is a reference to that node's namespaced auxiliary
                // declaration — a contract violation by the proof, not a
                // probe artifact. Surface it with the rule and remedy so
                // the worker repairs the dependency instead of treating
                // the rejection as checker breakage (the 2145 incident:
                // the worker inlined the auxiliary's whole proof).
                let owner = key.as_str().split('.').next().unwrap_or_default();
                // Compiler-generated children (`Foo.match_1`,
                // `Foo._proof_2`, `Foo.congr_simp`, ...) leaking into a
                // dep map are probe artifacts, not worker contract
                // violations — keep those on the generic diagnostic. The
                // classification only picks the message; both paths fail
                // closed identically.
                let child = key.as_str().split('.').nth(1).unwrap_or_default();
                let generated_child = child.starts_with('_')
                    || [
                        "match_",
                        "congr_",
                        "eq_",
                        "proof_",
                        "casesOn",
                        "rec",
                        "below",
                        "brecOn",
                        "noConfusion",
                        "injEq",
                        "sizeOf",
                    ]
                    .iter()
                    .any(|p| child.starts_with(p));
                if key.as_str().contains('.')
                    && !generated_child
                    && present_nodes.contains(&NodeId::new(owner))
                {
                    private_aux.push(format!(
                        "`{}` is a private auxiliary declaration of node `{owner}`; \
                         node proofs depend on registered nodes' principal \
                         declarations. Depend on `{owner}` itself, or factor the \
                         auxiliary out: move its statement and proof into a new \
                         Lean-closed helper node and point `{owner}`'s proof at it",
                        key.as_str()
                    ));
                } else {
                    out.push(format!("{label}={}", key.as_str()));
                }
            }
        }
    }
    let mut unmappable: Vec<String> = Vec::new();
    let mut private_aux: Vec<String> = Vec::new();
    collect_unmappable(
        &probe.boundary_theorems,
        "boundary_theorems",
        present_nodes,
        &mut unmappable,
        &mut private_aux,
    );
    collect_unmappable(
        &probe.strict_theorem_deps,
        "strict_theorem_deps",
        present_nodes,
        &mut unmappable,
        &mut private_aux,
    );
    collect_unmappable(
        &probe.strict_definition_deps,
        "strict_definition_deps",
        present_nodes,
        &mut unmappable,
        &mut private_aux,
    );
    if !private_aux.is_empty() {
        probe.errors.push(format!(
            "local-closure probe rejected private-auxiliary dependencies: {}",
            private_aux.join("; ")
        ));
        probe.status = "internal_error".to_string();
    }
    if !unmappable.is_empty() {
        probe.errors.push(format!(
            "local-closure probe contains dep names not in kernel present_nodes \
             (Patch C-K fail-closed validation): {unmappable:?}"
        ));
        probe.status = "internal_error".to_string();
    }

    // Patch C-N item 1: kind validation. Run only over deps that ARE in
    // `present_nodes` so a single dep doesn't surface twice in the
    // diagnostic. A dep present in `present_nodes` but absent from
    // `node_kinds` counts as a kind mismatch — the caller is responsible
    // for keeping `present_nodes` and `node_kinds` in lockstep (kernel
    // state invariant); if they diverge here, fail closed.
    //
    // Skip kind validation entirely when `node_kinds` is empty: that's
    // the standalone-CLI / pre-Patch-C-N back-compat path where kinds
    // simply weren't supplied. The membership check still fires; only
    // the additional kind refinement is suppressed.
    if node_kinds.is_empty() {
        return;
    }
    fn collect_kind_mismatches(
        map: &BTreeMap<NodeId, String>,
        label: &str,
        expected: crate::NodeKind,
        present_nodes: &BTreeSet<NodeId>,
        node_kinds: &BTreeMap<NodeId, crate::NodeKind>,
        out: &mut Vec<String>,
    ) {
        for key in map.keys() {
            if !present_nodes.contains(key) {
                // Already surfaced by the membership pass.
                continue;
            }
            match node_kinds.get(key) {
                Some(actual) if *actual == expected => {}
                Some(actual)
                    if *actual == crate::NodeKind::Preamble
                        && label == "strict_definition_deps"
                        && key.as_str() == PREAMBLE_NAME => {}
                Some(actual) => {
                    out.push(format!(
                        "{label}={} (expected={:?}, actual={:?})",
                        key.as_str(),
                        expected,
                        actual
                    ));
                }
                None => {
                    out.push(format!(
                        "{label}={} (expected={:?}, actual=<missing>)",
                        key.as_str(),
                        expected
                    ));
                }
            }
        }
    }
    let mut kind_mismatches: Vec<String> = Vec::new();
    collect_kind_mismatches(
        &probe.boundary_theorems,
        "boundary_theorems",
        crate::NodeKind::Proof,
        present_nodes,
        node_kinds,
        &mut kind_mismatches,
    );
    collect_kind_mismatches(
        &probe.strict_theorem_deps,
        "strict_theorem_deps",
        crate::NodeKind::Proof,
        present_nodes,
        node_kinds,
        &mut kind_mismatches,
    );
    collect_kind_mismatches(
        &probe.strict_definition_deps,
        "strict_definition_deps",
        crate::NodeKind::Definition,
        present_nodes,
        node_kinds,
        &mut kind_mismatches,
    );
    if !kind_mismatches.is_empty() {
        probe.errors.push(format!(
            "local-closure probe contains dep names whose kind does not \
             match the expected kind for the dep category \
             (Patch C-N kind validation): {kind_mismatches:?}"
        ));
        probe.status = "internal_error".to_string();
    }
}

fn empty_external_command_observation() -> ExternalCommandObservation {
    ExternalCommandObservation {
        returncode: None,
        stdout: String::new(),
        stderr: String::new(),
        timed_out: false,
        spawn_error: String::new(),
    }
}

fn should_run_axiom_audit(repo_path: &Path, observation: &NodeObservation) -> bool {
    // R4 C9 (decision #3): the `#print axioms` audit is Lean-only — IsabelleHol
    // soundness is the server cert (`isabelle_cert_gate_violation`), and the
    // Lean-shaped marker / declaration-name / `parse_print_axioms_output` checks
    // below would all false-fire on a `.thy`. Resolving the target here also
    // keeps `observe_node` from issuing a Lean `lean-print-axioms` op against an
    // Isabelle repo. `Lean` on the all-Lean live run ⇒ byte-identical (the
    // historical body runs unchanged).
    if trellis_kernel::tablet_target_for_repo(repo_path) != trellis_kernel::backend::BackendId::Lean
    {
        return false;
    }
    if !observation.lean_exists || !observation.tex_exists {
        return false;
    }

    let compile_output = external_output(&observation.compile);
    let mut compiles = command_ok(&observation.compile);
    if !compiles && is_lake_package_error(&compile_output) {
        compiles = true;
    }
    if !compiles {
        return false;
    }

    let marker_name = extract_marker_name(&observation.lean_content);
    if marker_name != observation.node {
        return false;
    }
    let declaration_name = declaration_name(&observation.lean_content);
    if !trellis_kernel::filespec::decl_name_matches_node(&declaration_name, &observation.node) {
        return false;
    }
    // B2c-gate Slice 3 / S6: the shape + import + forbidden gate is
    // backend-parameterized (resolves to Lean for the live run ⇒ byte-identical).
    let shape_target = trellis_kernel::tablet_target_for_repo(repo_path);
    if !trellis_kernel::backend::source_model_for(shape_target)
        .validate_node_shape(&observation.lean_content, &observation.node)
        .is_empty()
    {
        return false;
    }
    if !validate_tex_format(&observation.tex_content, false).is_empty() {
        return false;
    }
    if !validate_imports_for_target(shape_target, &observation.lean_content).is_empty() {
        return false;
    }
    let forbidden_hits =
        scan_forbidden_keywords_for_target(shape_target, &observation.lean_content);
    if forbidden_hits.iter().any(|hit| hit.keyword != "sorry") {
        return false;
    }
    if !scan_sorry_in_definitions(&observation.lean_content).is_empty() {
        return false;
    }

    let tex_environment = tex_statement_environment(&observation.tex_content);
    let declaration_kind = declaration_kind(&observation.lean_content, &observation.node);
    if declaration_kind == "definition" && is_proof_bearing_statement_environment(&tex_environment)
    {
        return false;
    }
    if declaration_kind == "theorem_like" && tex_environment == "definition" {
        return false;
    }

    let sorry_in_source = forbidden_hits_include_textual_sorry(&forbidden_hits);
    let sorry_warning = compile_output.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("sorry") && (lower.contains("warning") || lower.contains("declaration uses"))
    });
    !sorry_in_source && !sorry_warning
}

pub(crate) fn observe_node(repo_path: &Path, node: &str) -> Result<NodeObservation, String> {
    // R4 A1: route the node-source path per target. `tablet_target_for_repo`
    // resolves to `Lean` on the all-Lean live run (config absent / not
    // `isabelle_hol`), so `node_source_path` returns `Tablet/<node>.lean`
    // byte-for-byte — identical to the historical hardcoded path. For an
    // IsabelleHol tablet it returns `Tablet/<node>.thy`, so a `.thy` node is
    // observed (`lean_exists==true`, `lean_content` = the `.thy`) and flows
    // PAST the `evaluate_node_observation` early `.lean`-not-found reject to
    // the soundness cert gate. `target` is computed here (not threaded) from
    // the `repo_path` this function already holds — every caller would resolve
    // the identical tablet-wide target (no per-node override exists), so the
    // internal resolution is exactly the threaded value, with a far smaller
    // hot-path edit surface. The `lean_*` field NAMES are retained; only the
    // path they point at is target-selected.
    let target = trellis_kernel::tablet_target_for_repo(repo_path);
    let lean_path = trellis_kernel::node_source_path(repo_path, node, target);
    let tex_path = repo_path.join("Tablet").join(format!("{node}.tex"));
    let lean_exists = lean_path.exists();
    let tex_exists = tex_path.exists();
    let compile = if lean_exists {
        run_compile_node(repo_path, node)?
    } else {
        empty_external_command_observation()
    };
    let mut observation = NodeObservation {
        node: node.to_string(),
        lean_path: lean_path.display().to_string(),
        tex_path: tex_path.display().to_string(),
        lean_exists,
        tex_exists,
        lean_content: if lean_exists {
            read_text_if_exists(&lean_path)
        } else {
            String::new()
        },
        tex_content: if tex_exists {
            read_text_if_exists(&tex_path)
        } else {
            String::new()
        },
        compile,
        print_axioms: empty_external_command_observation(),
    };
    if should_run_axiom_audit(repo_path, &observation) {
        // Sync of Tablet/{README,INDEX,header}.md was hoisted to
        // observe_nodes_parallel's setup (see comment there). Here we only
        // need the per-node olean materialization. Previously this called
        // ensure_worker_checker_support_available, which re-ran the sync
        // for every node — producing 6-way concurrent sync-tablet-support
        // subprocesses (parallelism=6) that raced on Tablet/README.md and
        // intermittently broke acceptance.
        let requested_nodes = BTreeSet::from([NodeId::from(node)]);
        ensure_worker_checker_oleans_materialized(repo_path, &requested_nodes)?;
        observation.print_axioms = run_print_axioms(repo_path, node)?;
    }
    Ok(observation)
}

/// Run `observe_node` on each (idx, name) pair in `to_observe` with the
/// requested parallelism. Returns a Vec of (name, result) preserving the
/// input order. Each `observe_node` call shells out to `python3 check.py
/// lean-compile-node <node>`, which is the heavy step (10-30s per node);
/// with parallelism=N we can run N at once, capped by the cgroup memory
/// ceiling. Errors are propagated per-node — caller decides what to do.
fn observe_nodes_parallel(
    repo_path: &Path,
    to_observe: &[(usize, String)],
    total: usize,
    parallelism: usize,
) -> Vec<(String, Result<NodeObservation, String>)> {
    // One-shot sync of Tablet/{README,INDEX,header}.md for this batch. The
    // render output is a pure function of the repo tree (see
    // build_tablet_support_render_output in kernel/src/tablet_support.rs),
    // so it suffices to sync once before observing any nodes — the per-node
    // calls inside observe_node would write identical bytes anyway. Hoisting
    // also eliminates the parallelism=6 race that previously broke
    // acceptance via "sync-tablet-support returned invalid JSON".
    if let Err(err) = sync_tablet_render_support_from_repo(repo_path) {
        eprintln!(
            "[acceptance] observe_nodes_parallel: pre-batch sync-tablet-support failed: {err}"
        );
    }
    if parallelism <= 1 || to_observe.len() <= 1 {
        // Serial path: simpler, no thread overhead, matches prior behavior.
        return to_observe
            .iter()
            .map(|(idx, name)| {
                eprintln!(
                    "[acceptance]   observe_tablet {}/{}: {}",
                    idx + 1,
                    total,
                    name
                );
                let result = observe_node(repo_path, name);
                (name.clone(), result)
            })
            .collect();
    }
    // Parallel path: bounded thread pool with std lib only.
    use std::sync::{Arc, Mutex};
    let queue: Arc<Mutex<Vec<(usize, String)>>> =
        Arc::new(Mutex::new(to_observe.iter().rev().cloned().collect()));
    let results: Arc<Mutex<Vec<(usize, String, Result<NodeObservation, String>)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let repo_path_owned = repo_path.to_path_buf();
    std::thread::scope(|scope| {
        for _ in 0..parallelism {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            let repo_path = repo_path_owned.clone();
            scope.spawn(move || {
                // Mark this worker thread as "in the parallel check
                // batch" so per-child cgroup attach fires for the
                // lean-compile-node calls inside observe_node. Reset
                // when the thread exits (the closure returns).
                IN_PARALLEL_CHECK_BATCH.with(|c| c.set(true));
                loop {
                    let next = {
                        let mut q = queue.lock().unwrap();
                        q.pop()
                    };
                    let Some((idx, name)) = next else { break };
                    eprintln!(
                        "[acceptance]   observe_tablet {}/{}: {}",
                        idx + 1,
                        total,
                        name
                    );
                    let result = observe_node(&repo_path, &name);
                    results.lock().unwrap().push((idx, name, result));
                }
                IN_PARALLEL_CHECK_BATCH.with(|c| c.set(false));
            });
        }
    });
    // Restore original input order in the returned vec.
    let mut out = results.lock().unwrap().clone();
    out.sort_by_key(|(idx, _, _)| *idx);
    out.into_iter().map(|(_, n, r)| (n, r)).collect()
}

pub(crate) fn observe_tablet(repo_path: &Path) -> Result<TabletObservation, String> {
    observe_tablet_with_roles(repo_path, &BTreeMap::new())
}

pub(crate) fn observe_tablet_with_roles(
    repo_path: &Path,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> Result<TabletObservation, String> {
    let tablet_dir = repo_path.join("Tablet");
    let tablet_exists = tablet_dir.exists();
    let preamble_lean = tablet_dir.join("Preamble.lean");
    let preamble_tex = tablet_dir.join("Preamble.tex");
    let mut node_names = Vec::new();
    let mut invalid_node_names = Vec::new();
    let mut missing_tex_for_lean = Vec::new();
    let mut orphan_tex_nodes = Vec::new();
    let mut nodes = BTreeMap::new();

    if tablet_exists {
        // Collect eligible (.lean / non-Preamble / non-Axioms) names up
        // front so we can emit `[acceptance]   observe_tablet k/N: <node>`
        // per-node progress to stderr — `observe_node` shells out to lake
        // and can take many seconds per node, so the agent driving this
        // CLI through the Python wrapper sees forward progress instead
        // of a long silent gap inside phase 2/scoped_tablet (or phase 5
        // fingerprints, depending on the caller). The line-by-line
        // forwarder in `trellis.runtime.kernel_cli` streams these to
        // the host's stderr in real time. No-op when no `.lean` files
        // exist; cheap (one collect + sort) when they do.
        // R4 A6: the node-source extension is the target's (`lean` on the
        // all-Lean live run ⇒ byte-identical discovery; `thy` for IsabelleHol),
        // so an Isabelle tablet's `Tablet/<Node>.thy` files are enumerated.
        let source_ext = trellis_kernel::backend::descriptor_for(
            trellis_kernel::tablet_target_for_repo(repo_path),
        )
        .node_source_ext;
        let mut eligible: Vec<String> = Vec::new();
        for lean_path in fs::read_dir(&tablet_dir)
            .map_err(|err| format!("read Tablet dir failed: {err}"))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some(source_ext))
        {
            let Some(name) = lean_path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if name == "Preamble" || name == "Axioms" {
                continue;
            }
            eligible.push(name.to_string());
        }
        eligible.sort();

        let total = eligible.len();
        // Pre-classify nodes serially (fast file existence + name checks)
        // so the heavy parallel pass below only runs `observe_node` on
        // nodes that actually need the lean shell-out.
        let mut to_observe: Vec<(usize, String)> = Vec::new();
        for (idx, name) in eligible.into_iter().enumerate() {
            node_names.push(name.clone());
            let is_extraction_model = is_extraction_model_observation_node(&name, node_role);
            if !is_extraction_model && !is_valid_node_name(&name) {
                invalid_node_names.push(name);
                continue;
            }
            let tex_path = tablet_dir.join(format!("{name}.tex"));
            if !is_extraction_model && !tex_path.exists() {
                missing_tex_for_lean.push(name);
                continue;
            }
            to_observe.push((idx, name));
        }

        // Parallelism for the per-node observe pass. Default is 1 (serial,
        // matches prior behavior). Set TRELLIS_LEAN_PARALLELISM=N (with
        // TRELLIS_CHECK_CGROUP also set so OOM is bounded) to enable
        // concurrent per-node lean compile checks.
        let parallelism = std::env::var("TRELLIS_LEAN_PARALLELISM")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(1);

        let parallel_results = observe_nodes_parallel(repo_path, &to_observe, total, parallelism);

        // OOM rescue: any node that hit "[OOM]" in the parallel pass gets
        // ONE retry serially. The retry runs on the main thread where
        // IN_PARALLEL_CHECK_BATCH is false, so apply_check_cgroup_attach
        // is a no-op — the retry has full system memory available
        // (~62 GB on the current host, vs. the 24 GB cgroup cap that
        // bounded the parallel batch). A node that genuinely needs
        // >24 GB will succeed under retry if the host has the memory;
        // if the uncapped retry STILL fails, the error surfaces.
        for (name, result) in parallel_results {
            match result {
                Ok(observation) => {
                    nodes.insert(name, observation);
                }
                Err(message) if message.starts_with("[OOM]") => {
                    eprintln!(
                        "[acceptance]   observe_tablet OOM on {}; retrying serially without cgroup cap.",
                        name
                    );
                    let observation = observe_node(repo_path, &name)?;
                    nodes.insert(name, observation);
                }
                Err(message) => return Err(message),
            }
        }

        // R4 A6: orphan-tex detection compares against the node-SOURCE set,
        // which is the target's extension (`lean` ⇒ byte-identical on the live
        // run; `thy` for IsabelleHol so a `.tex` with a sibling `.thy` is not
        // flagged orphan). `source_ext` is computed above.
        let lean_node_names: BTreeSet<String> = fs::read_dir(&tablet_dir)
            .map_err(|err| format!("read Tablet dir failed: {err}"))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some(source_ext))
            .filter_map(|path| {
                path.file_stem()
                    .and_then(|value| value.to_str())
                    .map(str::to_string)
            })
            .filter(|name| name != "Preamble" && name != "Axioms")
            .collect();
        let tex_node_names: BTreeSet<String> = fs::read_dir(&tablet_dir)
            .map_err(|err| format!("read Tablet dir failed: {err}"))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("tex"))
            .filter_map(|path| {
                path.file_stem()
                    .and_then(|value| value.to_str())
                    .map(str::to_string)
            })
            .filter(|name| name != "header" && name != "Preamble")
            .collect();
        orphan_tex_nodes = tex_node_names
            .difference(&lean_node_names)
            .cloned()
            .collect();
    }

    Ok(TabletObservation {
        tablet_exists,
        preamble: PreambleObservation {
            lean_exists: preamble_lean.exists(),
            tex_exists: preamble_tex.exists(),
            lean_content: if preamble_lean.exists() {
                read_text_if_exists(&preamble_lean)
            } else {
                String::new()
            },
            tex_content: if preamble_tex.exists() {
                read_text_if_exists(&preamble_tex)
            } else {
                String::new()
            },
        },
        node_names,
        invalid_node_names,
        missing_tex_for_lean,
        orphan_tex_nodes,
        nodes,
        build: ExternalCommandObservation {
            returncode: if tablet_exists { Some(0) } else { None },
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            spawn_error: String::new(),
        },
    })
}

pub(crate) fn observe_tablet_nodes(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
) -> Result<TabletObservation, String> {
    observe_tablet_nodes_with_roles(repo_path, requested_nodes, &BTreeMap::new())
}

pub(crate) fn observe_tablet_nodes_with_roles(
    repo_path: &Path,
    requested_nodes: &BTreeSet<NodeId>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> Result<TabletObservation, String> {
    let tablet_dir = repo_path.join("Tablet");
    let tablet_exists = tablet_dir.exists();
    let preamble_lean = tablet_dir.join("Preamble.lean");
    let preamble_tex = tablet_dir.join("Preamble.tex");
    let mut node_names = Vec::new();
    let mut invalid_node_names = Vec::new();
    let mut missing_tex_for_lean = Vec::new();
    let mut nodes = BTreeMap::new();

    if tablet_exists {
        let mut requested: Vec<String> = requested_nodes
            .iter()
            .map(|node| node.as_str().to_string())
            .filter(|name| name != "Preamble" && name != "Axioms" && name != "header")
            .collect();
        requested.sort();
        requested.dedup();

        let total = requested.len();
        let mut to_observe: Vec<(usize, String)> = Vec::new();
        for (idx, name) in requested.into_iter().enumerate() {
            node_names.push(name.clone());
            let is_extraction_model = is_extraction_model_observation_node(&name, node_role);
            if !is_extraction_model && !is_valid_node_name(&name) {
                invalid_node_names.push(name);
                continue;
            }
            let lean_path = tablet_dir.join(format!("{name}.lean"));
            let tex_path = tablet_dir.join(format!("{name}.tex"));
            if !is_extraction_model && lean_path.exists() && !tex_path.exists() {
                missing_tex_for_lean.push(name);
                continue;
            }
            to_observe.push((idx, name));
        }

        let parallelism = std::env::var("TRELLIS_LEAN_PARALLELISM")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(1);

        let parallel_results = observe_nodes_parallel(repo_path, &to_observe, total, parallelism);
        for (name, result) in parallel_results {
            match result {
                Ok(observation) => {
                    nodes.insert(name, observation);
                }
                Err(message) if message.starts_with("[OOM]") => {
                    eprintln!(
                        "[acceptance]   observe_tablet OOM on {}; retrying serially without cgroup cap.",
                        name
                    );
                    let observation = observe_node(repo_path, &name)?;
                    nodes.insert(name, observation);
                }
                Err(message) => return Err(message),
            }
        }
    }

    Ok(TabletObservation {
        tablet_exists,
        preamble: PreambleObservation {
            lean_exists: preamble_lean.exists(),
            tex_exists: preamble_tex.exists(),
            lean_content: if preamble_lean.exists() {
                read_text_if_exists(&preamble_lean)
            } else {
                String::new()
            },
            tex_content: if preamble_tex.exists() {
                read_text_if_exists(&preamble_tex)
            } else {
                String::new()
            },
        },
        node_names,
        invalid_node_names,
        missing_tex_for_lean,
        orphan_tex_nodes: Vec::new(),
        nodes,
        build: ExternalCommandObservation {
            returncode: if tablet_exists { Some(0) } else { None },
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            spawn_error: String::new(),
        },
    })
}

pub(crate) fn observe_correspondence_fingerprints(
    repo_path: &Path,
    nodes: &BTreeSet<NodeId>,
) -> Result<BTreeMap<NodeId, String>, String> {
    observe_correspondence_fingerprints_with_under_model_assumptions(
        repo_path,
        nodes,
        &BTreeSet::new(),
    )
}

pub(crate) fn observe_correspondence_fingerprints_with_under_model_assumptions(
    repo_path: &Path,
    nodes: &BTreeSet<NodeId>,
    under_model_assumption_nodes: &BTreeSet<NodeId>,
) -> Result<BTreeMap<NodeId, String>, String> {
    Ok(observe_correspondence_fingerprints_detailed(
        repo_path,
        nodes,
        under_model_assumption_nodes,
    )?
    .fingerprints)
}

fn observe_correspondence_fingerprints_detailed(
    repo_path: &Path,
    nodes: &BTreeSet<NodeId>,
    under_model_assumption_nodes: &BTreeSet<NodeId>,
) -> Result<CorrespondenceFingerprintObservation, String> {
    if nodes.is_empty() {
        return Ok(CorrespondenceFingerprintObservation::default());
    }
    if !has_lake_project(repo_path) {
        let mut output = BTreeMap::new();
        for node in nodes {
            output.insert(
                node.clone(),
                legacy_correspondence_fingerprint(repo_path, node, under_model_assumption_nodes),
            );
        }
        return Ok(CorrespondenceFingerprintObservation {
            fingerprints: output,
            unavailable_reasons: BTreeMap::new(),
        });
    }
    let payload_nodes: BTreeSet<NodeId> = nodes
        .iter()
        .filter(|node| {
            node.as_str() != PREAMBLE_NAME
                && !is_under_model_assumption_observation_node(
                    node.as_str(),
                    under_model_assumption_nodes,
                )
        })
        .cloned()
        .collect();
    let payloads = observe_lean_semantic_payloads(repo_path, &payload_nodes)?;
    let mut output = BTreeMap::new();
    let mut unavailable_reasons = BTreeMap::new();
    for node in nodes {
        let payload = payloads.get(node.as_str());
        let fingerprint =
            correspondence_fingerprint(repo_path, node, payload, under_model_assumption_nodes);
        if fingerprint.trim().is_empty() {
            if let Some(reason) = correspondence_fingerprint_unavailable_reason(
                repo_path,
                node,
                payload,
                under_model_assumption_nodes,
            ) {
                unavailable_reasons.insert(node.clone(), reason);
            }
        }
        output.insert(node.clone(), fingerprint);
    }
    Ok(CorrespondenceFingerprintObservation {
        fingerprints: output,
        unavailable_reasons,
    })
}

pub(crate) fn observe_soundness_fingerprints(
    repo_path: &Path,
    nodes: &BTreeSet<NodeId>,
    node_kinds: &BTreeMap<NodeId, NodeKind>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> Result<BTreeMap<NodeId, String>, String> {
    if nodes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut output = BTreeMap::new();
    for node in nodes {
        let fingerprint =
            soundness_fingerprint_with_context(repo_path, node, node_kinds, node_role);
        if !fingerprint.trim().is_empty() {
            output.insert(node.clone(), fingerprint);
        }
    }
    Ok(output)
}

pub(crate) fn observe_soundness_fingerprint_parts(
    repo_path: &Path,
    nodes: &BTreeSet<NodeId>,
    node_kinds: &BTreeMap<NodeId, NodeKind>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> Result<BTreeMap<NodeId, SoundFingerprintParts>, String> {
    if nodes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut output = BTreeMap::new();
    for node in nodes {
        let parts =
            soundness_fingerprint_parts_with_context(repo_path, node, node_kinds, node_role);
        if !parts.combined_sound_fp.trim().is_empty() {
            output.insert(node.clone(), parts);
        }
    }
    Ok(output)
}

pub(crate) fn observe_sketch_proof_nodes(
    repo_path: &Path,
    nodes: &BTreeSet<NodeId>,
) -> BTreeSet<NodeId> {
    nodes
        .iter()
        .filter(|node| {
            let tex_content =
                read_text_if_exists(&repo_path.join("Tablet").join(format!("{node}.tex")));
            tex_proof_starts_with_sketch_marker(&tex_content)
        })
        .cloned()
        .collect()
}

/// Definition nodes whose Lean body is the bare placeholder `True` — the
/// definition-analogue of a SKETCH proof. A worker writes `... := True`
/// (e.g. `def Foo (...) : Prop := True`) to defer the real correspondence
/// work on a hard definition; the deferral is accepted into the Tablet but
/// must never reach correspondence Pass. The kernel detects it here and
/// `current_corr_state` forces a deterministic Fail (no verifier round),
/// mirroring `observe_sketch_proof_nodes` + `SketchAutoFail`.
///
/// Conservative scope (intentional, mirrors SKETCH's exact-marker rule):
/// only the body region after the `-- BODY` marker, with whitespace and a
/// single trailing `--` line comment stripped, equal to exactly the bare
/// token `True`, is matched. `fun _ => True`, `True ∧ True`, `by trivial`,
/// or any non-trivial body is NOT a placeholder. Only `Definition` nodes
/// are considered — a theorem body is a proof, never the bare type `True`,
/// and a real definition never has body `True`, so the exact match cannot
/// false-positive a genuine definition. Nodes that fail to split (no/bad
/// `-- BODY` marker, unreadable file) are conservatively excluded.
pub(crate) fn observe_placeholder_definition_nodes(
    repo_path: &Path,
    present_nodes: &BTreeSet<NodeId>,
    node_kinds: &BTreeMap<NodeId, NodeKind>,
) -> BTreeSet<NodeId> {
    present_nodes
        .iter()
        .filter(|node| {
            if node_kinds.get(*node) != Some(&NodeKind::Definition) {
                return false;
            }
            lean_definition_body_is_true_placeholder(repo_path, node.as_str())
        })
        .cloned()
        .collect()
}

/// True iff `<repo>/Tablet/<node>.lean` splits cleanly at its `-- BODY`
/// marker and its body region is one of the two definition-placeholder
/// shapes:
///   * the bare token `True` (the `def`/`abbrev` placeholder), or
///   * a contentless body under a `where`-form data declaration
///     (`structure Foo where` / `inductive Foo where` / `class Foo …
///     where` with no fields or constructors below the marker — the
///     structural analog of the `True` body: `structure … where` ≅ Unit,
///     `inductive … where` ≅ Empty, both of which compile).
/// For the `True` shape an inline `--` note is stripped before the
/// comparison; for the `where` shape the body counts as empty only when
/// every line is blank or a whole-line `--` comment, so a real field or
/// constructor below a leading comment keeps the declaration out of the
/// net. See `observe_placeholder_definition_nodes` for the conservative-
/// scope rationale. Returns false on any read/split failure (fail-closed:
/// an unparseable file is never treated as a placeholder).
fn lean_definition_body_is_true_placeholder(repo_path: &Path, node: &str) -> bool {
    // R4 A7: split via the target's source model. On the all-Lean live run
    // `target` resolves to `Lean` ⇒ the Lean `-- BODY`-marker split runs
    // byte-for-byte. The `read_node_file` read below is `.lean`-keyed; for an
    // IsabelleHol tablet that read returns `Err` and this conservatively
    // reports `false` (not a placeholder) — the `True` / empty-`where` shapes
    // are Lean-FILESPEC syntax with no Isabelle analogue, and a `.thy`
    // definition's substantiveness is governed elsewhere. So the detector is
    // vacuously-false for Isabelle, never false-positively flagging a `.thy`.
    let target = trellis_kernel::tablet_target_for_repo(repo_path);
    let Ok((content, _hash)) = trellis_kernel::filespec_split::read_node_file(repo_path, node)
    else {
        return false;
    };
    let Ok(split) = trellis_kernel::backend::source_model_for(target).split(&content, node) else {
        return false;
    };
    let body = &content[split.body_marker_end_byte..];
    // `def`/`abbrev` placeholder: the body is the single token `True`,
    // optionally followed by a worker's trailing note. Strip an inline
    // `--` comment (everything from the first marker), then trim. `True`
    // always precedes any such note, so truncation cannot lose content.
    let body_first_comment_stripped = match body.find("--") {
        Some(idx) => &body[..idx],
        None => body,
    };
    if body_first_comment_stripped.trim() == "True" {
        return true;
    }
    // Empty `where`-body placeholder: the declaration head above the
    // marker ends with `where` (a data declaration in where-form) and
    // no field/constructor line remains below the marker. The emptiness
    // test strips whole-line `--` comments and blank lines *line by line*
    // (NOT first-`--`-truncate, which would erase real fields sitting
    // below a leading comment) so a genuine `structure`/`inductive`/
    // `class` with a leading comment plus real fields is never flagged.
    // Note: the kernel's placeholder net intentionally covers only the
    // literally-empty `where`-body; a contentless-but-nonempty body
    // (e.g. a single dummy field) is left to the correspondence verifier,
    // which owns the rest.
    let where_body_is_empty = body.lines().all(|line| {
        let t = line.trim();
        t.is_empty() || t.starts_with("--")
    });
    where_body_is_empty && declaration_head_ends_with_where(&content, &split)
}

/// True iff the line immediately above the `-- BODY` marker (trailing
/// whitespace ignored) ends with the standalone keyword `where`. Mirrors
/// the body-delimiter recognition in
/// `filespec_split::validate_body_marker_position`.
fn declaration_head_ends_with_where(
    content: &str,
    split: &trellis_kernel::filespec_split::FilespecSplit,
) -> bool {
    let pre_body = &content[..split.body_marker_start_byte];
    let pre_body_no_eol = pre_body
        .strip_suffix("\r\n")
        .or_else(|| pre_body.strip_suffix('\n'))
        .unwrap_or(pre_body);
    let prior_line = pre_body_no_eol
        .rsplit_once('\n')
        .map(|(_, last)| last.trim_end_matches('\r'))
        .unwrap_or(pre_body_no_eol);
    let trimmed = prior_line.trim_end();
    if trimmed == "where" {
        return true;
    }
    trimmed.strip_suffix("where").is_some_and(|stripped| {
        !stripped.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    })
}

/// Active soundness fingerprint scheme, resolved from environment.
///
/// * `V1` — legacy text path: hashes `self_tex` + the full direct-imports
///   list + each child's statement block. Reopens whenever the import set
///   changes (even adding a Lean-closed helper unrelated to the NL proof
///   blocks the parent's soundness baseline — the bug that motivated v2).
/// * `V2Strict` — hash includes only `\noderef`-cited statements + own
///   `.tex`. Strictly rejects (returns empty fingerprint) any node whose
///   `\noderef`s aren't all in its recursive Lean import closure. Use
///   once the corpus has been fully migrated to `\noderef` coverage.
/// * `V2Permissive` — same hash structure as `V2Strict` but skips both
///   the import-closure enforcement and the bail-on-missing-noderef-tex
///   guard. Intended for a controlled rollout where workers are starting
///   to add `\noderef` citations but the corpus isn't yet fully covered;
///   captures the desired reopening semantics (own .tex or `\noderef`-
///   cited dep statement changed) without breaking existing nodes that
///   pre-date the citation discipline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundnessFingerprintMode {
    V1,
    V2Strict,
    V2Permissive,
}

pub(crate) fn soundness_fingerprint_mode() -> SoundnessFingerprintMode {
    // V2Strict is the default: the `\noderef`-within-import-closure
    // citation discipline is corpus-wide on current projects, and strict
    // and permissive hashes are identical wherever strict's gates pass,
    // so the strict default reopens nothing on a compliant corpus.
    // Legacy corpora opt out explicitly via env var: `v2_permissive` for
    // partial `\noderef` coverage, `v1` for stored fingerprints that
    // pre-date the V2 schema.
    let raw = std::env::var("TRELLIS_SOUNDNESS_FINGERPRINT_V2")
        .or_else(|_| std::env::var("TRELLIS_SOUNDNESS_FINGERPRINT_MODE"))
        .unwrap_or_default();
    match raw.trim().to_ascii_lowercase().as_str() {
        "v2_permissive" | "v2-permissive" | "permissive" => SoundnessFingerprintMode::V2Permissive,
        "1" | "true" | "yes" | "on" | "v2" | "sound-v2" | "v2_strict" | "v2-strict" | "strict" => {
            SoundnessFingerprintMode::V2Strict
        }
        "0" | "false" | "no" | "off" | "v1" | "sound-v1" => SoundnessFingerprintMode::V1,
        // Unset, empty, or unrecognized: the default.
        _ => SoundnessFingerprintMode::V2Strict,
    }
}

/// Back-compat: existing call sites that just want "is some v2 mode on".
pub(crate) fn soundness_fingerprint_v2_enabled() -> bool {
    !matches!(soundness_fingerprint_mode(), SoundnessFingerprintMode::V1)
}

/// Substantiveness fingerprint structure (JSON-encoded for
/// storage in `WorkingSnapshot.substantiveness_current_fingerprints`).
///
/// Reopens whenever any of:
///   - `own_tex` (the node's `.tex` statement block) changes,
///   - `paper_source_sha` (the configured paper file hash) changes,
///   - `node_kind` (preamble / definition / proof) changes.
///   - claimed deviation ids or their current file fingerprints change.
///   - claimed reference-paper ids or their file content hashes change.
///
/// Conservative by design: hashing the entire paper.tex means any paper
/// edit reopens every node's substantiveness status. Wes confirmed
/// (2026-04-29) that the paper isn't edited mid-run, so this is in
/// practice constant; the field stays in the design as defence against
/// future revisions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstantivenessFingerprint {
    pub own_tex: String,
    pub paper_source_sha: String,
    pub node_kind: String,
    #[serde(default)]
    pub claimed_deviation_fingerprints: BTreeMap<DeviationId, String>,
    /// Content hashes of the reference papers this node claims as
    /// grounding authority (`node_reference_grounds`), keyed by id.
    /// LAST field, and skipped when empty — a claim-free fingerprint
    /// serializes byte-identically to the pre-feature shape, so no
    /// stored substantiveness baseline reopens on deploy.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub claimed_reference_shas: BTreeMap<RefPaperId, String>,
}

impl SubstantivenessFingerprint {
    pub fn to_storage_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_storage_string(raw: &str) -> Option<Self> {
        if raw.trim().is_empty() {
            return None;
        }
        serde_json::from_str(raw).ok()
    }
}

/// Compute substantiveness fingerprints for the supplied node
/// set. Mirrors the `observe_correspondence_fingerprints` shape: returns
/// a `BTreeMap<NodeId, JSON-encoded fingerprint>`. Excludes the
/// `Preamble` node (per-node lane delegates Preamble to the target-level
/// paper lane).
///
/// `paper_source_path` is the path to the configured paper file (e.g.
/// `paper/arXiv_v3.tex`). Empty / missing path produces an empty
/// `paper_source_sha` — fingerprint still serialises but is conservatively
/// considered "no baseline established yet" by reopen semantics.
pub fn observe_substantiveness_fingerprints(
    repo_path: &Path,
    nodes: &BTreeSet<NodeId>,
    paper_source_path: Option<&Path>,
    node_kinds: &BTreeMap<NodeId, crate::NodeKind>,
    node_deviation_claims: &BTreeMap<NodeId, BTreeSet<DeviationId>>,
    deviation_current_fingerprints: &BTreeMap<DeviationId, String>,
    configured_reference_papers: &BTreeMap<RefPaperId, ReferencePaperSpec>,
    node_reference_grounds: &BTreeMap<NodeId, BTreeSet<RefPaperId>>,
) -> Result<BTreeMap<NodeId, String>, String> {
    if nodes.is_empty() {
        return Ok(BTreeMap::new());
    }
    // Content hash per CLAIMED reference paper (same `hash_text`
    // discipline as `paper_source_sha`). Computed once per id across the
    // node loop; a missing/empty file yields an empty sha (conservative
    // "no baseline" convention).
    let claimed_ids: BTreeSet<&RefPaperId> = node_reference_grounds
        .iter()
        .filter(|(node, _)| nodes.contains(*node))
        .flat_map(|(_, ids)| ids.iter())
        .collect();
    let mut reference_shas: BTreeMap<RefPaperId, String> = BTreeMap::new();
    for id in claimed_ids {
        let sha = configured_reference_papers
            .get(id)
            .map(|spec| {
                let candidate = Path::new(&spec.tex_path);
                let resolved = if candidate.is_absolute() {
                    candidate.to_path_buf()
                } else {
                    repo_path.join(candidate)
                };
                let content = read_text_if_exists(&resolved);
                if content.is_empty() {
                    String::new()
                } else {
                    hash_text(&content)
                }
            })
            .unwrap_or_default();
        reference_shas.insert(id.clone(), sha);
    }
    let paper_source_sha = paper_source_path
        .map(|path| {
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                repo_path.join(path)
            };
            let content = read_text_if_exists(&resolved);
            if content.is_empty() {
                String::new()
            } else {
                hash_text(&content)
            }
        })
        .unwrap_or_default();
    let mut output = BTreeMap::new();
    for node in nodes {
        if node.as_str() == PREAMBLE_NAME {
            // Preamble is delegated to the target-level paper lane; do
            // not produce a per-node fingerprint here. (Empty string
            // matches the convention used by the corr lane for "no
            // baseline" — `current_substantiveness_state` short-circuits
            // Preamble to Pass anyway.)
            output.insert(node.clone(), String::new());
            continue;
        }
        let tex_statement = extract_tex_statement_block(&read_text_if_exists(
            &repo_path
                .join("Tablet")
                .join(format!("{}.tex", node.as_str())),
        ));
        if tex_statement.is_empty() {
            output.insert(node.clone(), String::new());
            continue;
        }
        let kind = node_kinds
            .get(node)
            .copied()
            .unwrap_or(crate::NodeKind::Definition);
        let kind_label = match kind {
            crate::NodeKind::Preamble => "preamble",
            crate::NodeKind::Definition => "definition",
            crate::NodeKind::Proof => "proof",
        };
        let fingerprint = SubstantivenessFingerprint {
            own_tex: hash_text(&tex_statement),
            paper_source_sha: paper_source_sha.clone(),
            node_kind: kind_label.to_string(),
            claimed_deviation_fingerprints: node_deviation_claims
                .get(node)
                .into_iter()
                .flatten()
                .map(|id| {
                    (
                        id.clone(),
                        deviation_current_fingerprints
                            .get(id)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect(),
            claimed_reference_shas: node_reference_grounds
                .get(node)
                .into_iter()
                .flatten()
                .map(|id| {
                    (
                        id.clone(),
                        reference_shas.get(id).cloned().unwrap_or_default(),
                    )
                })
                .collect(),
        };
        output.insert(node.clone(), fingerprint.to_storage_string());
    }
    Ok(output)
}

pub fn observe_deviation_fingerprints(
    repo_path: &Path,
    deviation_files: &BTreeMap<DeviationId, String>,
) -> Result<BTreeMap<DeviationId, String>, String> {
    let mut output = BTreeMap::new();
    for (id, path) in deviation_files {
        let relative = Path::new(path);
        if relative.is_absolute() {
            return Err(format!("deviation {id} path must be relative"));
        }
        let components: Vec<Component<'_>> = relative.components().collect();
        if components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(format!(
                "deviation {id} path must not contain '.', '..', root, or prefix components"
            ));
        }
        let under_reference = components.first().and_then(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        }) == Some("reference");
        if !under_reference || !path.ends_with(".tex") {
            return Err(format!(
                "deviation {id} path must be a .tex file under reference/"
            ));
        }
        let resolved = repo_path.join(relative);
        if let Ok(canonical) = resolved.canonicalize() {
            let repo_canonical = repo_path
                .canonicalize()
                .map_err(|err| format!("canonicalize repo path: {err}"))?;
            if !canonical.starts_with(&repo_canonical) {
                return Err(format!("deviation {id} path escapes repository"));
            }
        }
        let fingerprint = fs::read_to_string(&resolved)
            .map(|content| hash_text(&content))
            .unwrap_or_default();
        output.insert(id.clone(), fingerprint);
    }
    Ok(output)
}

pub(crate) fn snapshot_tablet_dir(repo_path: &Path) -> BTreeMap<String, String> {
    let tablet_dir = repo_path.join("Tablet");
    if !tablet_dir.exists() {
        return BTreeMap::new();
    }
    let Ok(entries) = fs::read_dir(&tablet_dir) else {
        return BTreeMap::new();
    };
    let mut snapshot = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Ok(content) = fs::read(&path) else {
            continue;
        };
        snapshot.insert(name.to_string(), hash_bytes(&content));
    }
    snapshot
}

fn normalize_snapshot_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(name)
        .to_string()
}

pub(crate) fn detect_snapshot_changes(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> SnapshotChanges {
    let before_norm: BTreeMap<String, String> = before
        .iter()
        .map(|(name, hash)| (normalize_snapshot_name(name), hash.clone()))
        .collect();
    let after_norm: BTreeMap<String, String> = after
        .iter()
        .map(|(name, hash)| (normalize_snapshot_name(name), hash.clone()))
        .collect();
    let all_names: BTreeSet<String> = before_norm
        .keys()
        .chain(after_norm.keys())
        .cloned()
        .collect();
    let mut changes = SnapshotChanges::default();
    for name in all_names {
        match (before_norm.get(&name), after_norm.get(&name)) {
            (None, Some(_)) => changes.created.push(name),
            (Some(_), None) => changes.deleted.push(name),
            (Some(before_hash), Some(after_hash)) if before_hash != after_hash => {
                changes.modified.push(name)
            }
            _ => {}
        }
    }
    changes
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

fn is_structural_declaration_hash_node(node_name: &str) -> bool {
    node_name == PREAMBLE_NAME || node_name == AXIOMS_NAME
}

fn whole_file_declaration_hash(content: &str) -> String {
    format!("{WHOLE_FILE_DECLARATION_HASH_PREFIX}{}", hash_text(content))
}

/// Structured per-node correspondence fingerprint.
///
/// Stored in `ProtocolState::live.corr_current_fingerprints[node]` and
/// `ProtocolState::corr_approved_fingerprints[node]` as a JSON string (keeping
/// the surrounding `BTreeMap<NodeId, String>` schema unchanged). A node's
/// correspondence is considered "reopened" whenever a new prospective
/// `CorrespondenceFingerprint` would diverge from the approved baseline under
/// the [`corr_reopen_triggered`] predicate below.
///
/// The fingerprint captures everything needed to answer "has the expressed
/// mathematical content of this node's statement changed?":
///
/// - `own_tex`: hash of the node's own `.tex` statement block.
/// - `lean_semantic_closure`: hash of the Lean semantic payload produced by
///   `scripts/lean_semantic_fingerprint.lean` — the transitive serialization
///   of every `const` referenced in the node's Lean declaration type (and
///   for definitions, values). Proof bodies are excluded, so reorganizing a
///   proof in a dependency does NOT change this hash; changing a
///   definition's value, or a referenced theorem's type, DOES.
/// - `lean_relevant_definition_descendants`: a map from each definition-kind
///   node consumed by this node's Lean type-surface closure walk to the
///   hash of that descendant's `.tex` statement. Captured at the moment
///   this fingerprint was produced; treated by [`corr_reopen_triggered`]
///   as the "definitions present at last correspondence check" baseline.
///   Strictly narrower than the textual-import closure: descendants reached
///   only via proof bodies do not appear here. **Used for TeX-hash
///   propagation only.** Theorems / propositions / axioms in the closure
///   walk are NOT in this set because their TeX-statement content is not
///   part of the parent's verifier basis (the parent uses the theorem's
///   *type*, captured by `lean_semantic_closure`).
/// - `lean_relevant_dependencies`: the **full** set of project-defined
///   Tablet nodes consumed by the closure walk, regardless of TeX
///   environment. Includes definitions, theorems, propositions, axioms.
///   **Used for topological dispatch eligibility** — the parent's
///   verifier interprets each of these dependencies' Lean meanings, so
///   the parent's corr cannot be soundly dispatched until each
///   dependency has `corr_status == Pass`. Stored as a name set (no
///   per-name hash) since dispatch only consults `corr_status`.
/// - `preamble_tex`: structured hash of the preamble's tex content.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrespondenceFingerprint {
    pub own_tex: String,
    pub lean_semantic_closure: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lean_relevant_definition_descendants: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub lean_relevant_dependencies: BTreeSet<String>,
    #[serde(default)]
    pub preamble_tex: String,
}

impl CorrespondenceFingerprint {
    /// Serialize for storage in the `corr_current_fingerprints` /
    /// `corr_approved_fingerprints` String slots. Deterministic thanks to
    /// `BTreeMap` key ordering.
    pub fn to_storage_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse from a storage string. Returns `None` for empty strings and
    /// legacy single-hash strings (not parseable as JSON); callers should
    /// treat those as "no baseline yet" for reopen semantics.
    pub fn from_storage_string(raw: &str) -> Option<Self> {
        if raw.trim().is_empty() {
            return None;
        }
        serde_json::from_str(raw).ok()
    }
}

/// Does swapping `approved` for `prospective` constitute a correspondence
/// reopen?
///
/// Semantics:
/// - `own_tex`, `lean_semantic_closure`, `preamble_tex` must match
///   byte-for-byte.
/// - Every descendant name present in `approved.lean_relevant_definition_descendants`
///   must also be present in `prospective.lean_relevant_definition_descendants`
///   with a matching hash. This is the "frozen at last correspondence
///   check" behavior over the Lean-relevance-filtered axis: definitions
///   whose Lean meaning is consumed by the parent's type-surface closure
///   and that have since had their tex changed or been removed from the
///   Lean-relevant set count as reopens.
/// - Extra descendants in `prospective` not present in `approved` are
///   **ignored**. A worker adding a fresh def to the Lean-relevant set
///   post-baseline does not cause a reopen. The new descendant will
///   become part of the frozen baseline at the next correspondence
///   check (when the node is re-verified).
///
/// If either side cannot be parsed (empty string / legacy single-hash
/// format / malformed JSON), we conservatively treat that as a reopen so
/// that stale storage is forced through re-approval rather than silently
/// trusted.
pub fn corr_reopen_triggered(approved: &str, prospective: &str) -> bool {
    let Some(approved) = CorrespondenceFingerprint::from_storage_string(approved) else {
        return true;
    };
    let Some(prospective) = CorrespondenceFingerprint::from_storage_string(prospective) else {
        return true;
    };
    if approved.own_tex != prospective.own_tex {
        return true;
    }
    if approved.lean_semantic_closure != prospective.lean_semantic_closure {
        return true;
    }
    if approved.preamble_tex != prospective.preamble_tex {
        return true;
    }
    // Lean-relevance-filtered descendant axis: only definitions whose Lean
    // declaration is consumed by the parent's `lean_semantic_closure` walk
    // contribute to reopen on TeX-statement changes. Definitions outside
    // the parent's Lean basis (e.g., descendants reached only via proof
    // bodies) are caught by their own per-node corr lanes. Legacy storage
    // (schema version 0) deserialises with this map empty, which the
    // load-time migration recomputes for aligned (Pass-and-byte-equal)
    // entries; drifted entries keep legacy storage so byte-mismatch
    // continues to drive Unknown until re-verified.
    for (name, approved_hash) in &approved.lean_relevant_definition_descendants {
        match prospective.lean_relevant_definition_descendants.get(name) {
            Some(p_hash) if p_hash == approved_hash => {}
            _ => return true,
        }
    }
    false
}

/// Enumerate which axes of a correspondence fingerprint would differ between
/// `approved` and `prospective`, producing a list of human-readable bullet
/// lines. Used to build the in-burst diagnostic when a commit would reopen
/// correspondence on a paper-target-covering node.
///
/// Returns an empty vec if both parse AND are equivalent under
/// [`corr_reopen_triggered`]. For unparseable inputs, returns a single-entry
/// vec noting the stale/missing baseline.
pub(crate) fn diff_corr_fingerprint_axes(approved: &str, prospective: &str) -> Vec<String> {
    let approved_fp = CorrespondenceFingerprint::from_storage_string(approved);
    let prospective_fp = CorrespondenceFingerprint::from_storage_string(prospective);
    match (approved_fp, prospective_fp) {
        (None, _) => vec![
            "Approved baseline for this node is missing or in a legacy format; \
             re-approval is required before modifications."
                .to_string(),
        ],
        (_, None) => vec![
            "Could not produce a prospective correspondence fingerprint for this \
             node (missing `.tex` statement, missing Lean declaration, or Lean \
             semantic-payload extraction failed)."
                .to_string(),
        ],
        (Some(a), Some(p)) => {
            let mut out = Vec::new();
            if a.own_tex != p.own_tex {
                out.push(
                    "The node's own `.tex` statement block has changed from the \
                     approval baseline."
                        .to_string(),
                );
            }
            if a.lean_semantic_closure != p.lean_semantic_closure {
                out.push(
                    "The node's Lean semantic closure has changed. Something in \
                     the transitive Lean-level surface of this node's declaration \
                     (theorem types, definition values, inductive/constructor \
                     shapes, axiom types) is no longer the same as at approval \
                     time. Proof bodies are NOT part of this closure — only \
                     meaning-changes of declarations it transitively references."
                        .to_string(),
                );
            }
            if a.preamble_tex != p.preamble_tex {
                out.push(
                    "The preamble's structured `.tex` definition content has \
                     changed from the approval baseline."
                        .to_string(),
                );
            }
            let mut missing = Vec::new();
            let mut changed = Vec::new();
            for (name, a_hash) in &a.lean_relevant_definition_descendants {
                match p.lean_relevant_definition_descendants.get(name) {
                    None => missing.push(name.clone()),
                    Some(p_hash) if p_hash != a_hash => changed.push(name.clone()),
                    Some(_) => {}
                }
            }
            if !changed.is_empty() {
                out.push(format!(
                    "The following definition-kind nodes — which were already \
                     in this node's Lean-import closure at approval time — have \
                     had their `.tex` statement changed: {:?}. Their statements \
                     are part of the baselined meaning of this node's target.",
                    changed
                ));
            }
            if !missing.is_empty() {
                out.push(format!(
                    "The following definition-kind nodes were present in this \
                     node's Lean-import closure at approval time but are no \
                     longer reachable: {:?}. The approved support surface of \
                     this node's target has changed.",
                    missing
                ));
            }
            out
        }
    }
}

/// Guard run at worker-delta commit time on every paper-target-covering
/// node. If the prospective post-commit correspondence fingerprint of any
/// covering node would diverge from `corr_approved_fingerprints[node]` under
/// the reopen predicate, emit a rich prose error enumerating which axes
/// differ, so the agent can correct course in-burst (the error flows back
/// through the `trellis-worker-result` deterministic check).
///
/// In `CoarseRestructure` mode the guard permits only reviewer-scoped
/// protected semantic changes. Any actual reopen in that approved scope is
/// returned to the engine so it can drain verifiers and require human
/// reapproval before normal proof-formalization routing resumes.
///
/// `repo_path`: Tablet tree to observe prospective fingerprints from.
/// `covering_nodes`: the `state.approved_target_nodes()` set snapshotted at
///     the last advance-gate approval.
/// `approved_corr_fingerprints`: a subset of `state.corr_approved_fingerprints`
///     containing at least the covering-node entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CorrReopenGuardReport {
    pub errors: Vec<String>,
    pub reopened_nodes: BTreeSet<NodeId>,
}

pub(crate) fn paper_target_corr_reopen_guard_report(
    repo_path: &Path,
    covering_nodes: &BTreeSet<NodeId>,
    approved_corr_fingerprints: &BTreeMap<NodeId, String>,
    proof_edit_mode: WorkerProofDeltaMode,
) -> Result<CorrReopenGuardReport, String> {
    paper_target_corr_reopen_guard_report_with_scope(
        repo_path,
        covering_nodes,
        approved_corr_fingerprints,
        proof_edit_mode,
        &BTreeSet::new(),
    )
}

pub(crate) fn paper_target_corr_reopen_guard_report_with_scope(
    repo_path: &Path,
    covering_nodes: &BTreeSet<NodeId>,
    approved_corr_fingerprints: &BTreeMap<NodeId, String>,
    proof_edit_mode: WorkerProofDeltaMode,
    allowed_protected_semantic_change_nodes: &BTreeSet<NodeId>,
) -> Result<CorrReopenGuardReport, String> {
    if covering_nodes.is_empty() {
        return Ok(CorrReopenGuardReport::default());
    }
    // Narrow to covering nodes that have an approved baseline. Nodes that
    // appear in coverage but have never been corr-approved yet aren't
    // guarded — they'll flow through normal correspondence verification
    // the first time around.
    let to_check: BTreeSet<NodeId> = covering_nodes
        .iter()
        .filter(|node| {
            approved_corr_fingerprints
                .get(node.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if to_check.is_empty() {
        return Ok(CorrReopenGuardReport::default());
    }
    let mut prospective_observation =
        observe_correspondence_fingerprints_detailed(repo_path, &to_check, &BTreeSet::new())?;
    let recoverable_unavailable: BTreeSet<NodeId> = to_check
        .iter()
        .filter(|node| {
            let approved = approved_corr_fingerprints
                .get(node.as_str())
                .cloned()
                .unwrap_or_default();
            let prospective_fp = prospective_observation
                .fingerprints
                .get(node.as_str())
                .cloned()
                .unwrap_or_default();
            CorrespondenceFingerprint::from_storage_string(&approved).is_some()
                && CorrespondenceFingerprint::from_storage_string(&prospective_fp).is_none()
                && prospective_observation
                    .unavailable_reasons
                    .get(node.as_str())
                    .map(|reason| corr_unavailable_reason_is_recoverable(reason))
                    .unwrap_or(false)
        })
        .cloned()
        .collect();
    let mut recovery_errors: BTreeMap<NodeId, String> = BTreeMap::new();
    if !recoverable_unavailable.is_empty() && has_lake_project(repo_path) {
        // Observation-only recovery: refresh checker artifacts so the guard can
        // compare the prospective fingerprint it already intended to compare.
        // This must not update approved baselines or make a changed fingerprint legal.
        let recovery_error =
            ensure_worker_checker_support_available(repo_path, &recoverable_unavailable).err();
        let retry = observe_correspondence_fingerprints_detailed(
            repo_path,
            &recoverable_unavailable,
            &BTreeSet::new(),
        )?;
        for node in &recoverable_unavailable {
            if let Some(fp) = retry.fingerprints.get(node.as_str()) {
                prospective_observation
                    .fingerprints
                    .insert(node.clone(), fp.clone());
            }
            match retry.unavailable_reasons.get(node.as_str()) {
                Some(reason) => {
                    prospective_observation
                        .unavailable_reasons
                        .insert(node.clone(), reason.clone());
                }
                None => {
                    prospective_observation.unavailable_reasons.remove(node);
                }
            }
            if let Some(err) = &recovery_error {
                let still_unavailable = prospective_observation
                    .fingerprints
                    .get(node.as_str())
                    .and_then(|fp| CorrespondenceFingerprint::from_storage_string(fp))
                    .is_none();
                if still_unavailable {
                    recovery_errors.insert(node.clone(), err.clone());
                }
            }
        }
    }
    let mut report = CorrReopenGuardReport::default();
    let mut errors = Vec::new();
    for node in &to_check {
        let approved = approved_corr_fingerprints
            .get(node.as_str())
            .cloned()
            .unwrap_or_default();
        let prospective_fp = prospective_observation
            .fingerprints
            .get(node.as_str())
            .cloned()
            .unwrap_or_default();
        if !corr_reopen_triggered(&approved, &prospective_fp) {
            continue;
        }
        if proof_edit_mode == WorkerProofDeltaMode::CoarseRestructure
            && allowed_protected_semantic_change_nodes.contains(node)
        {
            report.reopened_nodes.insert(node.clone());
            continue;
        }
        let mut axis_bullets = diff_corr_fingerprint_axes(&approved, &prospective_fp);
        if CorrespondenceFingerprint::from_storage_string(&prospective_fp).is_none() {
            if let Some(reason) = prospective_observation
                .unavailable_reasons
                .get(node.as_str())
            {
                let recovery_suffix = if recoverable_unavailable.contains(node) {
                    " after a narrow checker-support recovery retry"
                } else {
                    ""
                };
                let mut detail = format!(
                    "Could not produce a prospective correspondence fingerprint for this node\
                     {recovery_suffix}. Last unavailable reason: {reason}"
                );
                if let Some(recovery_error) = recovery_errors.get(node.as_str()) {
                    detail.push_str(&format!(
                        " Recovery materialization/checker-support error: {recovery_error}"
                    ));
                }
                axis_bullets = vec![detail];
            }
        }
        let bullet_lines: String = if axis_bullets.is_empty() {
            "  - (fingerprint differs but no specific axis could be identified)".to_string()
        } else {
            axis_bullets
                .iter()
                .map(|line| format!("  - {}", line))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let recourse = if proof_edit_mode == WorkerProofDeltaMode::CoarseRestructure {
            "This coarse_restructure worker was not explicitly authorized to change this \
             protected semantic node. The reviewer must name the node in \
             protected_semantic_change_node_ids and confirm that scope before the worker may \
             make this change; the accepted change will then require verifier drain and human \
             reapproval."
        } else {
            "If an expert-approved change is genuinely required, request \
             coarse_restructure mode from the reviewer and re-submit with explicit protected \
             semantic scope."
        };
        errors.push(format!(
            "Protected approved-target/protected-closure node `{node}` would have its \
             correspondence reopened by this change, which may change the meaning of a paper \
             target that was expert-approved at the last advance-gate. Specifically:\n\
             {bullet_lines}\n\n\
             Preserve: the covering node's `.tex` statement; the Lean semantic \
             meaning of its declaration (dependency proof bodies may change, but \
             changes that alter the meaning of referenced definitions / theorems \
             / axioms propagate here); and the `.tex` statements of all \
             definitions that were already in this node's Lean-import closure at \
             approval time. {recourse}"
        ));
    }
    report.errors = errors;
    Ok(report)
}

pub(crate) fn paper_target_corr_reopen_guard_errors(
    repo_path: &Path,
    covering_nodes: &BTreeSet<NodeId>,
    approved_corr_fingerprints: &BTreeMap<NodeId, String>,
    proof_edit_mode: WorkerProofDeltaMode,
) -> Result<Vec<String>, String> {
    Ok(paper_target_corr_reopen_guard_report(
        repo_path,
        covering_nodes,
        approved_corr_fingerprints,
        proof_edit_mode,
    )?
    .errors)
}

fn has_lake_project(repo_path: &Path) -> bool {
    repo_path.join("lakefile.lean").exists() || repo_path.join("lakefile.toml").exists()
}

/// Process-local short-circuit cache for `lean-semantic-payloads` results.
///
/// Keyed by a content-hash (NOT mtime) of every input that affects the
/// Lean side of a node's correspondence fingerprint:
///   - `Preamble.lean` (used by every node's import chain),
///   - the node's own `Tablet/<node>.lean`,
///   - every transitive `Tablet.<dep>.lean` reachable via `import Tablet.X`,
///   - lake state files (`lakefile.lean`, `lakefile.toml`,
///     `lake-manifest.json`, `lean-toolchain`) — a mathlib upgrade or
///     lake-toml change must invalidate every node's payload,
///   - the dispatch script (`.trellis/scripts/check.py`) — schema
///     changes invalidate everything.
///
/// When the cache key for a node matches a prior successful observation,
/// we return the stored `LeanSemanticPayloadObservation` directly and
/// skip the entire `python3 .trellis/scripts/check.py
/// lean-semantic-payloads` round trip — the single most expensive op in
/// the fingerprint walk (3-30s per call even on Python-side cache hits,
/// because the script still has to walk closures, hash oleans, and read
/// sidecar files).
///
/// Conservative-by-design:
///   - mtime is never read; only file-content SHA-256s.
///   - any I/O failure during key construction returns `None` ⇒ cache
///     skip ⇒ live-call fallback ⇒ slow path runs unchanged.
///   - we cache only `ok && !payload.is_empty()` results. A non-ok or
///     empty payload would otherwise pin a transient Lean error.
///   - cache is purely additive: identical inputs always yield identical
///     cached output. No eviction needed; size is bounded by the closure
///     hash space (one entry per (node, distinct closure-state) seen in
///     this kernel-binary process).
///
/// Two-tier (in-memory + disk): the kernel binary is short-lived (one
/// invocation per `RuntimeCliRequest` from the Python wrapper), so the
/// in-memory tier alone cannot persist across cycles or across separate
/// `run_kernel_cli` calls within one cycle. The disk tier closes that
/// gap, persisting under
/// `<runtime_root>/checker-state/kernel-cache/lean-semantic-payloads/`.
type LeanSemanticPayloadCacheKey = String;
type LeanSemanticPayloadCacheValue = LeanSemanticPayloadObservation;
static LEAN_SEMANTIC_PAYLOAD_CACHE: LazyLock<
    Mutex<HashMap<(PathBuf, String), (LeanSemanticPayloadCacheKey, LeanSemanticPayloadCacheValue)>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Compute the per-node Lean-state cache key for the result caches.
///
/// Delegates to `trellis_kernel::cache_key::lean_closure_cache_key_with_oleans`
/// — the source-closure content hash PLUS the content hash of every closure
/// member's `.olean`. The olean shas are load-bearing for these RESULT caches:
/// `print-axioms`, `lean-compile-node`, and `lean-semantic-payloads` all store
/// the output of a Lean op that READS oleans, so a result computed against a
/// content-stale olean must be invalidated when the olean is rebuilt — even
/// though the source (and hence the source-only key) is unchanged. Folding the
/// olean content in makes these caches self-healing and, via the changed blob
/// format, retroactively invalidates any pre-fix poisoned entries. The
/// materialize-oleans cache in `tablet_support` deliberately keeps the
/// source-only `lean_closure_cache_key` (it produces oleans rather than reading
/// them) plus its own sidecar content guard.
fn lean_closure_cache_key(repo_path: &Path, node_name: &str) -> Option<String> {
    trellis_kernel::cache_key::lean_closure_cache_key_with_oleans(repo_path, node_name)
}

/// Kernel-side payload-format salt. The kernel's payload caches are keyed by
/// SOURCE + OLEAN content (`lean_closure_cache_key_with_oleans`), which does
/// NOT see the fingerprint SCRIPT (it lives in the trellis source tree, not
/// the repo) — so a script/output-format change would silently serve stale
/// pre-format payloads as cache hits (verified on the 2026-07-04 dry run:
/// the schema-v4 migration recomputed every fingerprint against the OLD
/// degenerate payloads). Bump this salt with every payload-format change,
/// in lockstep with `SEMANTIC_PAYLOAD_CACHE_VERSION` in
/// `trellis/checker/sync.py` (the server-side sidecar's version).
const LEAN_SEMANTIC_PAYLOAD_FORMAT_SALT: &str = "payload-fmt-v2";

fn lean_semantic_payload_cache_key(repo_path: &Path, node_name: &str) -> Option<String> {
    lean_closure_cache_key(repo_path, node_name)
        .map(|k| format!("{LEAN_SEMANTIC_PAYLOAD_FORMAT_SALT}:{k}"))
}

fn observe_lean_semantic_payloads(
    repo_path: &Path,
    nodes: &BTreeSet<NodeId>,
) -> Result<BTreeMap<String, LeanSemanticPayloadObservation>, String> {
    if nodes.is_empty() {
        return Ok(BTreeMap::new());
    }
    // Canonicalise once for cache-key stability across symlink-different
    // but content-equal paths. Fall back to the raw path if canonicalize
    // fails (still correct: keys just won't deduplicate across views).
    let canon_repo = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    // Read-side: walk writable + optional readonly fallback. Write-side:
    // only to the writable directory (`cache_dir_for_namespace`).
    let read_dirs = cache_lookup_dirs(LEAN_SEMANTIC_PAYLOADS_DISK_NAMESPACE);
    let writable_disk_dir = cache_dir_for_namespace(LEAN_SEMANTIC_PAYLOADS_DISK_NAMESPACE);

    // Per-node lookup: Tier 1 (in-memory) first, then Tier 2 (disk).
    // Anything still missing falls through to a live dispatch.
    let mut cached: BTreeMap<String, LeanSemanticPayloadObservation> = BTreeMap::new();
    let mut missing: BTreeSet<NodeId> = BTreeSet::new();
    let mut planned_keys: BTreeMap<String, String> = BTreeMap::new();
    {
        let cache = LEAN_SEMANTIC_PAYLOAD_CACHE.lock().unwrap();
        for node in nodes {
            let key = match lean_semantic_payload_cache_key(repo_path, node.as_str()) {
                Some(k) => k,
                None => {
                    // Can't construct a sound key; play it safe by
                    // running the live observation for this node.
                    missing.insert(node.clone());
                    continue;
                }
            };
            match cache.get(&(canon_repo.clone(), node.to_string())) {
                Some((stored_key, stored_value)) if stored_key == &key => {
                    cached.insert(node.to_string(), stored_value.clone());
                }
                _ => {
                    planned_keys.insert(node.to_string(), key);
                    missing.insert(node.clone());
                }
            }
        }
    }

    // Tier 2: disk cache. Promote any disk hits into Tier 1 for
    // subsequent calls in this process. Walks `read_dirs` (writable +
    // optional readonly fallback); skipped cleanly when both are empty.
    if !read_dirs.is_empty() {
        let mut promote: Vec<(NodeId, String, LeanSemanticPayloadObservation)> = Vec::new();
        let mut still_missing: BTreeSet<NodeId> = BTreeSet::new();
        for node in &missing {
            let Some(k) = planned_keys.get(node.as_str()).cloned() else {
                still_missing.insert(node.clone());
                continue;
            };
            let disk_key = per_node_disk_lookup_key(&canon_repo, node.as_str());
            match disk_cache_get_first::<LeanSemanticPayloadObservation>(&read_dirs, &disk_key, &k)
            {
                Some(value) => {
                    cached.insert(node.to_string(), value.clone());
                    promote.push((node.clone(), k, value));
                }
                None => {
                    still_missing.insert(node.clone());
                }
            }
        }
        if !promote.is_empty() {
            let mut cache = LEAN_SEMANTIC_PAYLOAD_CACHE.lock().unwrap();
            for (node, k, value) in promote {
                cache.insert((canon_repo.clone(), node.to_string()), (k, value));
            }
        }
        missing = still_missing;
    }

    if missing.is_empty() {
        // Walk-level short-circuit: every requested node served from
        // cache. Zero `lean-semantic-payloads` round trips.
        return Ok(cached);
    }

    let mut args = vec![repo_path.display().to_string()];
    for node in &missing {
        args.push("--node".to_string());
        args.push(node.to_string());
    }
    // Route per-target for driver-selection consistency. `Lean` for the live
    // run ⇒ the `lean-semantic-payloads` op + `[repo, (--node N)*]` batch shape
    // is byte-identical. The Isabelle Tier-2 semantic-payload format and its
    // per-node `isabelle-thm-deps` arg shape are a LATER increment (not B2b);
    // the IsabelleHol arm is unreachable on the all-Lean live path, so the
    // shared `[repo, (--node N)*]` argv below is exercised only by Lean.
    let driver = trellis_kernel::backend::checker_driver_for(
        trellis_kernel::tablet_target_for_repo(repo_path),
    );
    let raw = run_repo_command_json(repo_path, driver.lean_semantic_payloads_op(), &args)?;
    let observed: BTreeMap<String, LeanSemanticPayloadObservation> = serde_json::from_value(raw)
        .map_err(|err| {
            format!(
                "parse {} output failed: {err}",
                driver.lean_semantic_payloads_op()
            )
        })?;

    // Populate both cache tiers for this round's misses, then merge
    // cached + observed into the response.
    {
        let mut cache = LEAN_SEMANTIC_PAYLOAD_CACHE.lock().unwrap();
        for (node, payload) in &observed {
            // Only memoise successful, non-empty payloads. A failed or
            // empty payload typically means the Lean elaboration
            // genuinely couldn't proceed (missing olean, syntax error
            // shadowed by build state, etc.); pinning that would
            // suppress future recovery attempts.
            if !payload.ok || payload.payload.is_empty() {
                continue;
            }
            // Re-derive the key here in case the caller's `planned_keys`
            // didn't have it (defensive: a node enters `missing` either
            // because key construction failed, or because it was a cache
            // miss with a known key). For the former path, we compute
            // the key now — if it succeeds, we cache; if it fails, we
            // skip caching (next call will recompute live).
            let key = planned_keys
                .get(node)
                .cloned()
                .or_else(|| lean_semantic_payload_cache_key(repo_path, node.as_str()));
            let Some(key) = key else { continue };
            cache.insert(
                (canon_repo.clone(), node.clone()),
                (key.clone(), payload.clone()),
            );
            if let Some(ref disk_dir) = writable_disk_dir {
                let disk_key = per_node_disk_lookup_key(&canon_repo, node);
                disk_cache_put(disk_dir, &disk_key, &key, payload);
            }
        }
    }

    let mut output = cached;
    for (node, payload) in observed {
        output.insert(node, payload);
    }
    Ok(output)
}

/// Per-target narrow Lean type-surface closure observation.
///
/// For each `(target, covering_nodes)` pair, runs each covering node's
/// `lean_semantic_payload` through the parser below and unions the
/// resulting *project-defined* (Tablet-side) const names — top-level
/// Tablet name only; auto-generated Lean elaborator artefacts like
/// `Foo._proof_1`, `Foo._cstage_*`, `Foo.match_*` aggregate under their
/// parent `Foo`. Covering nodes themselves are excluded (they're already
/// in `coverage`); Preamble is excluded (it sits below the
/// `isTabletConst` carveout in the closure).
///
/// The closure policy is the one from
/// `scripts/lean_semantic_fingerprint.lean` (header section: theorem →
/// walk type only; def → walk type and value; stop at the `Tablet.*`
/// boundary, where every reference becomes an `extern|<name>` line we
/// drop here). That means proof bodies do not enter the closure: a
/// lemma used only inside a covering theorem's proof will not appear.
/// This is the reviewer-facing "type-surface" set the human-review zip
/// describes as the protected meaning surface.
///
/// Reuses the existing per-node payload cache in
/// `observe_lean_semantic_payloads`, so steady-state cost is one
/// in-memory lookup per covering node (no disk reads, no Lean
/// re-elaboration).
/// Pure parser for one `lean_semantic_fingerprint.lean` payload's
/// `const|<name>|...` lines, producing the set of project-defined
/// top-level Tablet names referenced by the seed (with auto-generated
/// elaborator artefacts collapsed under their parent and Preamble +
/// the seed itself + non-present names filtered out). Extracted from
/// `observe_protected_closure_nodes` for unit testability.
fn parse_lean_payload_into_closure_names(
    payload: &str,
    covering_strs: &BTreeSet<&str>,
    present_strs: &BTreeSet<&str>,
) -> BTreeSet<NodeId> {
    let mut out = BTreeSet::new();
    for chunk in payload.split("||") {
        let chunk = chunk.trim();
        let Some(after) = chunk.strip_prefix("const|") else {
            continue;
        };
        let mut fields = after.split('|');
        let name = fields.next().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        // The fingerprint script emits the owning NODE ID on every const
        // line (`const|<full-name>|node=<owner>|...`), computed by the
        // declaring-module rule (`Tablet.<node>` minus prefix) — the same
        // attribution `lean_local_closure.lean::tabletDefDepNodeId?` uses.
        // The old first-component derivation (`name.split('.').next()`)
        // misattributes namespaced corpora (`dec2flt_full_integer.Spec.…`
        // → not a node; `FiniteDecimal.value` → the WRONG node
        // `FiniteDecimal`), so a const line without the owner field is
        // skipped rather than guessed at.
        let Some(owner) = fields.next().and_then(|f| f.strip_prefix("node=")) else {
            continue;
        };
        if owner.is_empty() || owner == "Preamble" {
            continue;
        }
        if covering_strs.contains(owner) {
            continue;
        }
        // Defensive (audit follow-up): only include names the kernel
        // knows as present nodes, so a stray declaration inside a
        // non-node module can never leak a non-NodeId into
        // `protected_closure_nodes_per_target`, where the downstream
        // `paper_target_corr_reopen_guard_errors` check would silently
        // no-op (no `corr_approved_fingerprints` entry), masking the
        // true protection contract.
        if !present_strs.contains(owner) {
            continue;
        }
        out.insert(NodeId::from(owner.to_string()));
    }
    out
}

pub(crate) fn observe_protected_closure_nodes(
    repo_path: &Path,
    coverage: &BTreeMap<TargetId, BTreeSet<NodeId>>,
    present_nodes: &BTreeSet<NodeId>,
) -> Result<BTreeMap<TargetId, BTreeSet<NodeId>>, String> {
    let present_strs: BTreeSet<&str> = present_nodes.iter().map(|n| n.as_str()).collect();
    let mut out: BTreeMap<TargetId, BTreeSet<NodeId>> = BTreeMap::new();
    for (target, covering) in coverage {
        if covering.is_empty() {
            out.insert(target.clone(), BTreeSet::new());
            continue;
        }
        let payloads = observe_lean_semantic_payloads(repo_path, covering)?;
        let mut union: BTreeSet<NodeId> = BTreeSet::new();
        let covering_strs: BTreeSet<&str> = covering.iter().map(|n| n.as_str()).collect();
        for payload in payloads.values() {
            if !payload.ok || payload.payload.is_empty() {
                continue;
            }
            union.extend(parse_lean_payload_into_closure_names(
                &payload.payload,
                &covering_strs,
                &present_strs,
            ));
        }
        out.insert(target.clone(), union);
    }
    Ok(out)
}

/// Test-only: clear all three process-local Lean-state caches for one repo.
/// Tests that mutate filesystem state in a single process need to
/// explicitly drop cache state because the static caches otherwise
/// persist across test cases. Keep this repo-scoped so cargo's parallel
/// test runner cannot clear another test's temp-repo cache mid-assertion.
#[cfg(test)]
fn clear_lean_semantic_payload_cache_for_tests(repo_path: &Path) {
    let canon_repo = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    LEAN_SEMANTIC_PAYLOAD_CACHE
        .lock()
        .unwrap()
        .retain(|(repo, _), _| repo != &canon_repo);
    COMPILE_NODE_CACHE
        .lock()
        .unwrap()
        .retain(|(repo, _), _| repo != &canon_repo);
    PRINT_AXIOMS_CACHE
        .lock()
        .unwrap()
        .retain(|(repo, _), _| repo != &canon_repo);
}

fn extract_tex_statement_block(tex_content: &str) -> String {
    tex_content
        .split("\\begin{proof}")
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn extract_declaration_with_imports(lean_content: &str) -> String {
    let mut out = Vec::new();
    for line in lean_content.lines() {
        out.push(line);
        let stripped = line.trim();
        if stripped.contains(":= sorry") || stripped.contains(":= by") || stripped.ends_with(":=") {
            break;
        }
    }
    out.join("\n")
}

fn direct_imports(
    repo_path: &Path,
    node_name: &str,
    target: trellis_kernel::backend::BackendId,
) -> BTreeSet<String> {
    // R4 C5 / A5 (R-NODEREF): read the node SOURCE and parse its intra-tablet
    // import edges per target. Without this every Isabelle `\noderef` falsely
    // rejects (the closure walk would be empty over a `.thy`).
    //
    // The LEAN arm reads `Tablet/<node>.lean` and uses the LOCAL
    // `extract_tablet_imports` — byte-for-byte the historical path. (It is NOT
    // routed through `SourceModel::tablet_imports`, whose Lean impl —
    // `worker_normalization::extract_tablet_imports` — differs from the local
    // parser on a whitespace-only `import Tablet.` suffix: the local one trims
    // THEN drops empties, so `import Tablet.   ` yields nothing, whereas the
    // worker parser would emit an empty NodeId. Preserving the local parser
    // keeps `direct_imports` Lean-byte-identical.) The ISABELLE arm reads
    // `Tablet/<node>.thy` and parses `imports Tablet_X` via
    // `isabelle_filespec::tablet_imports`.
    let source = read_text_if_exists(&trellis_kernel::node_source_path(
        repo_path, node_name, target,
    ));
    match target {
        trellis_kernel::backend::BackendId::Lean => extract_tablet_imports(&source),
        trellis_kernel::backend::BackendId::IsabelleHol => {
            trellis_kernel::isabelle_filespec::tablet_imports(&source)
                .into_iter()
                .map(|n| n.into_string())
                .collect()
        }
    }
}

fn recursive_imports(
    repo_path: &Path,
    node_name: &str,
    visited: &mut BTreeSet<String>,
    target: trellis_kernel::backend::BackendId,
) {
    if node_name.is_empty() || node_name == "Preamble" || visited.contains(node_name) {
        return;
    }
    visited.insert(node_name.to_string());
    for dep in direct_imports(repo_path, node_name, target) {
        if dep != "Preamble" {
            recursive_imports(repo_path, &dep, visited, target);
        }
    }
}

fn preamble_structured_items(tex_content: &str) -> Vec<trellis_kernel::TexStatementItem> {
    extract_tex_statement_items(tex_content, true)
}

fn preamble_structured_hash(tex_content: &str) -> Option<String> {
    let items = preamble_structured_items(tex_content);
    if items.is_empty() {
        return None;
    }
    serde_json::to_string(&items)
        .ok()
        .map(|json| hash_text(&json))
}

/// Fingerprint-schema migration entry point. Performs two distinct
/// steps with different lifecycles (see `corr_fingerprint_schema_version`
/// in model.rs for the full doc):
///
/// 1. **One-shot axis migration** (gated on `version < 2`): recomputes
///    corr and paper fingerprints to use the Lean-relevance-filtered
///    `lean_relevant_definition_descendants` axis on both
///    `CorrespondenceFingerprint` and `PaperTargetFingerprint`.
///    Refuses to run while an in-flight Worker request exists (would
///    otherwise bless unaccepted worker WIP into the approval baseline).
///    Other request kinds (Verifier/Review/HumanGate) don't mutate disk,
///    so the migration runs safely during them — and must, since the
///    matching `load_runtime_with_fingerprint_validation` drift check
///    only skips on Worker.
///
/// 2. **Every-load schema-equivalent repair** (NOT gated on version):
///    `repair_schema_equivalent_fingerprint_baselines` runs unconditionally
///    on every load. It re-pins legacy-shape approved fingerprints to
///    match current live fingerprints when the two are schema-equivalent
///    (own_tex / lean_semantic_closure / preamble_tex match AND the new-
///    shape descendants are a subset of the legacy-shape descendants
///    with the same hashes). Catches mid-run drift introduced by
///    `apply_*_worker_response` Valid arms doing `state.live = snapshot`
///    against an OLD-shape approved baseline. Pure in-memory and
///    idempotent, so it is safe to re-run on every load.
///
/// On success, bumps the version to `CORR_FINGERPRINT_SCHEMA_VERSION`.
/// Returns `Ok(false)` when nothing changed (e.g. already-current state
/// with no schema-equivalent drift).
///
/// Three invariants the one-shot axis migration preserves uniformly
/// across both lanes (audit findings M1-M3):
///
/// - **M1 (no drift-blessing):** approved is re-pinned only for entries
///   that were `live == approved` byte-equal pre-migration. Drifted
///   entries get `live` recomputed but `approved` left in legacy storage,
///   so the byte-mismatch driving Unknown status persists across the
///   migration boundary.
/// - **M2 (no last_clean corruption):** `last_clean_*` mirrors are NOT
///   recomputed. They snapshot an older clean tablet state; recomputing
///   from current disk would corrupt the baseline used by
///   `apply_last_clean_reset`. Legacy storage remains; serde-default
///   handles the missing new-axis field on deserialization.
/// - **M3 (no empty-fingerprint blessing):** if the recomputed fingerprint
///   is empty (e.g. transient lake outage), skip the per-entry copy and
///   preserve existing legacy storage so the entry retains a baseline.
pub(crate) fn migrate_corr_fingerprint_schema(
    state: &mut crate::ProtocolState,
    repo_path: &Path,
) -> Result<bool, String> {
    let original_version = state.corr_fingerprint_schema_version;
    // Skip only on in-flight Worker. Workers are the only request kind
    // that mutates disk, so they're the one case where a mid-flight
    // migration could bless unaccepted WIP. Verifier / Review / HumanGate
    // requests don't mutate disk; migration is safe to run during them
    // and we MUST run it — otherwise `load_runtime_with_fingerprint_validation`
    // (which only skips drift validation for in-flight Worker) would
    // observe v2-shape fingerprints from disk and fail equality against
    // the still-legacy stored live fingerprints.
    if let Some(req) = state.in_flight_request.as_ref() {
        if req.kind == crate::RequestKind::Worker {
            return Ok(false);
        }
    }

    let mut changed = false;

    // One-shot axis migrations. `< 2`: the original Lean-relevance axis
    // change. `< 4`: the 2026-07-04 semantic-payload format change (seed
    // resolution + owner-tagged const lines) — same recompute + re-pin
    // logic, same M1-M3 invariants, re-run once so the stored fingerprints
    // align with what the new script observes from disk.
    if state.corr_fingerprint_schema_version < 4 {
        migrate_corr_axis(state, repo_path)?;
        migrate_paper_axis(state, repo_path)?;
        changed = true;
    }

    // Schema-equivalent baseline repair runs on every load, NOT only when
    // schema_version < 3. The reason: live fingerprints can drift mid-run
    // when `apply_*_worker_response` Valid arms do `state.live = snapshot`
    // (the snapshot's fingerprints are computed by the post-5a99009 binary
    // in NEW shape `lean_relevant_definition_descendants`, but
    // `corr_approved_fingerprints` retain their OLD shape baseline from a
    // prior checkpointed state). Without this repair re-running on every
    // load, an OLD-shape approved + NEW-shape live combo persists across
    // restarts as `cur != approved → Unknown`, falsely reopening
    // verification on protected paper-target covering nodes. The
    // function is idempotent
    // and pure-in-memory (no lake/disk I/O), so running it unconditionally
    // is cheap; it's a runtime invariant restorer, not a one-shot
    // migration step.
    changed |= repair_schema_equivalent_fingerprint_baselines(state);

    state.corr_fingerprint_schema_version = CORR_FINGERPRINT_SCHEMA_VERSION;
    Ok(changed || original_version != CORR_FINGERPRINT_SCHEMA_VERSION)
}

fn repair_schema_equivalent_fingerprint_baselines(state: &mut crate::ProtocolState) -> bool {
    let mut changed = false;

    for (node, current) in state.live.corr_current_fingerprints.clone() {
        let Some(approved) = state.corr_approved_fingerprints.get(&node) else {
            continue;
        };
        if current != *approved && corr_fingerprint_schema_equivalent(approved, &current) {
            state
                .corr_approved_fingerprints
                .insert(node.clone(), current.clone());
            changed = true;
        }
        if let Some(committed) = state.committed.corr_current_fingerprints.get(&node) {
            if *committed != current && corr_fingerprint_schema_equivalent(committed, &current) {
                state
                    .committed
                    .corr_current_fingerprints
                    .insert(node.clone(), current.clone());
                changed = true;
            }
        }
    }

    for (target, current) in state.live.paper_current_fingerprints.clone() {
        let Some(approved) = state.paper_approved_fingerprints.get(&target) else {
            continue;
        };
        if current != *approved && paper_fingerprint_schema_equivalent(approved, &current) {
            state
                .paper_approved_fingerprints
                .insert(target.clone(), current.clone());
            changed = true;
        }
        if let Some(committed) = state.committed.paper_current_fingerprints.get(&target) {
            if *committed != current && paper_fingerprint_schema_equivalent(committed, &current) {
                state
                    .committed
                    .paper_current_fingerprints
                    .insert(target.clone(), current.clone());
                changed = true;
            }
        }
    }

    changed
}

fn corr_fingerprint_schema_equivalent(approved: &str, current: &str) -> bool {
    let Some(approved) = parse_json_object(approved) else {
        return false;
    };
    let Some(current) = parse_json_object(current) else {
        return false;
    };

    for key in ["own_tex", "lean_semantic_closure", "preamble_tex"] {
        if approved.get(key).and_then(|v| v.as_str()) != current.get(key).and_then(|v| v.as_str()) {
            return false;
        }
    }

    legacy_compatible_map_subset(
        &current,
        &approved,
        "lean_relevant_definition_descendants",
        "definition_descendants",
    )
}

fn paper_fingerprint_schema_equivalent(approved: &str, current: &str) -> bool {
    let Some(approved) = parse_json_object(approved) else {
        return false;
    };
    let Some(current) = parse_json_object(current) else {
        return false;
    };

    for key in ["target", "covering_nodes", "preamble_definition_hashes"] {
        if approved.get(key) != current.get(key) {
            return false;
        }
    }

    legacy_compatible_map_subset(
        &current,
        &approved,
        "lean_relevant_definition_descendants",
        "definition_nodes",
    )
}

fn parse_json_object(raw: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(raw).ok()? {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    }
}

fn legacy_compatible_map_subset(
    current: &serde_json::Map<String, serde_json::Value>,
    approved: &serde_json::Map<String, serde_json::Value>,
    current_key: &str,
    legacy_key: &str,
) -> bool {
    let current_map = current
        .get(current_key)
        .or_else(|| current.get(legacy_key))
        .and_then(|v| v.as_object());
    let approved_map = approved
        .get(current_key)
        .or_else(|| approved.get(legacy_key))
        .and_then(|v| v.as_object());

    let Some(current_map) = current_map else {
        return approved_map.map(|m| m.is_empty()).unwrap_or(true);
    };
    let Some(approved_map) = approved_map else {
        return current_map.is_empty();
    };

    current_map
        .iter()
        .all(|(key, value)| approved_map.get(key) == Some(value))
}

fn migrate_corr_axis(state: &mut crate::ProtocolState, repo_path: &Path) -> Result<(), String> {
    // M1: snapshot pre-migration alignment. Only nodes whose live and
    // approved fingerprints byte-equal pre-migration may have approved
    // re-pinned post-recompute. Drifted nodes (status Unknown due to
    // prior worker activity) leave approved unchanged so the
    // byte-mismatch survives the schema change.
    let aligned_corr_nodes: BTreeSet<NodeId> = state
        .corr_approved_fingerprints
        .iter()
        .filter(|(node, approved)| {
            state.live.corr_current_fingerprints.get(*node) == Some(*approved)
        })
        .map(|(node, _)| node.clone())
        .collect();
    let live_corr_nodes: BTreeSet<NodeId> = state
        .live
        .corr_current_fingerprints
        .keys()
        .cloned()
        .collect();
    let committed_corr_nodes: BTreeSet<NodeId> = state
        .committed
        .corr_current_fingerprints
        .keys()
        .cloned()
        .collect();
    let recompute_set: BTreeSet<NodeId> = aligned_corr_nodes
        .iter()
        .chain(live_corr_nodes.iter())
        .chain(committed_corr_nodes.iter())
        .cloned()
        .collect();
    if recompute_set.is_empty() {
        return Ok(());
    }
    let under_model_assumption_nodes = under_model_assumption_nodes_from_state(state);
    let fresh = observe_correspondence_fingerprints_with_under_model_assumptions(
        repo_path,
        &recompute_set,
        &under_model_assumption_nodes,
    )?;
    for (node, fp) in &fresh {
        // M3: skip empty recomputes (failed payload extraction).
        if fp.trim().is_empty() {
            continue;
        }
        // M1: re-pin approved only for nodes that were aligned
        // pre-migration. Drifted nodes keep legacy approved.
        if aligned_corr_nodes.contains(node) {
            state
                .corr_approved_fingerprints
                .insert(node.clone(), fp.clone());
        }
        // Live can always be refreshed; it's a current-disk view and
        // re-pinning it just normalises the schema for present nodes.
        if live_corr_nodes.contains(node) {
            state
                .live
                .corr_current_fingerprints
                .insert(node.clone(), fp.clone());
        }
        // Committed snapshot semantic: `restore_committed()` (worker
        // rejection path) reverts `live` to `committed` and pairs with
        // `RestoreWorktreeToActiveWorkerBase`, which restores disk to the
        // `active_worker_base` recorded when the worker was dispatched.
        // The no-Worker-in-flight gate at the top of this migration
        // guarantees no worker is currently dispatched, so the next
        // worker's `active_worker_base` will be the current disk content
        // — i.e. the same content we recompute fingerprints against
        // here. Pinning committed to this fresh recompute means
        // restore_committed will leave live and disk in agreement.
        // Without this, `restore_committed` after migration would revert
        // live to legacy shape and trip a spurious Unknown until the
        // next observation re-pins.
        //
        // (Note: committed != live is a normal mid-cycle state — most
        // worker accept paths do not call `commit_live`. The relevant
        // safety condition is "no Worker in-flight", not committed/live
        // equality.)
        if committed_corr_nodes.contains(node) {
            state
                .committed
                .corr_current_fingerprints
                .insert(node.clone(), fp.clone());
        }
    }
    // M2: last_clean_corr_approved_fingerprints and
    // last_clean_live.corr_current_fingerprints intentionally untouched.
    Ok(())
}

pub(crate) fn migrate_soundness_fingerprint_schema_if_enabled(
    state: &mut crate::ProtocolState,
    repo_path: &Path,
) -> Result<bool, String> {
    let mode = soundness_fingerprint_mode();
    if matches!(mode, SoundnessFingerprintMode::V1) {
        return Ok(false);
    }
    migrate_soundness_fingerprint_schema(state, repo_path, mode)
}

/// Re-aligns soundness fingerprints to the active schema mode. NOT a
/// one-shot migration: this runs on every `SupervisorRuntime::load` and
/// recomputes fingerprints from the CURRENT on-disk Tablet text, then
/// rewrites only the live/committed/approved/mirror entries whose stored
/// value differs from the freshly computed one (under the content-undrift
/// guards documented at each write site). Also self-heals any Pass mirror
/// whose approved baseline was poisoned by an earlier unguarded write.
fn migrate_soundness_fingerprint_schema(
    state: &mut crate::ProtocolState,
    repo_path: &Path,
    mode: SoundnessFingerprintMode,
) -> Result<bool, String> {
    if let Some(req) = state.in_flight_request.as_ref() {
        if req.kind == crate::RequestKind::Worker {
            return Ok(false);
        }
    }

    let old_live = state.live.sound_current_fingerprints.clone();
    let old_committed = state.committed.sound_current_fingerprints.clone();
    let old_last_clean = state.last_clean_live.sound_current_fingerprints.clone();
    let node_kinds = state.node_kinds.clone();
    let node_role = state.node_role.clone();
    let aligned_nodes: BTreeSet<NodeId> = state
        .sound_approved_fingerprints
        .iter()
        .filter(|(node, approved)| old_live.get(*node) == Some(*approved))
        .map(|(node, _)| node.clone())
        .collect();
    let aligned_last_clean_nodes: BTreeSet<NodeId> = state
        .last_clean_sound_approved_fingerprints
        .iter()
        .filter(|(node, approved)| old_last_clean.get(*node) == Some(*approved))
        .map(|(node, _)| node.clone())
        .collect();

    // Schema-bridge rescue: nodes where `approved` is the legacy
    // (pre-40e678b) hash of the CURRENT .tex content. Such approvals
    // were blessed under the prior schema_tag-included payload on
    // content that hasn't drifted since; re-bless them under the new
    // payload so the verdict carries across the schema migration.
    // Without this, a mid-run binary swap (which silently upgrades the
    // worker-acceptance subprocess's hash format while leaving the
    // supervisor on the old in-memory copy) drifts `live` to the new
    // format while `approved` stays on the legacy format, and the
    // aligned_nodes filter alone misses those nodes' approvals.
    let bridgeable_approved_nodes: BTreeSet<NodeId> = state
        .sound_approved_fingerprints
        .iter()
        .filter(|(node, approved)| {
            if aligned_nodes.contains(*node) {
                return false;
            }
            approved_matches_legacy_payload(repo_path, node.as_str(), approved.as_str())
        })
        .map(|(node, _)| node.clone())
        .collect();
    let bridgeable_last_clean_nodes: BTreeSet<NodeId> = state
        .last_clean_sound_approved_fingerprints
        .iter()
        .filter(|(node, approved)| {
            if aligned_last_clean_nodes.contains(*node) {
                return false;
            }
            approved_matches_legacy_payload(repo_path, node.as_str(), approved.as_str())
        })
        .map(|(node, _)| node.clone())
        .collect();
    let empty_verifier_pass_assessment_nodes: BTreeSet<NodeId> = state
        .sound_assessments
        .iter()
        .filter(|(_, assessment)| {
            assessment.status == trellis_kernel::SoundAssessmentStatus::VerifierPass
                && assessment.fingerprints.combined_sound_fp.trim().is_empty()
        })
        .map(|(node, _)| node.clone())
        .collect();

    let recompute_set: BTreeSet<NodeId> = old_live
        .keys()
        .chain(old_committed.keys())
        .chain(old_last_clean.keys())
        .chain(aligned_nodes.iter())
        .chain(aligned_last_clean_nodes.iter())
        .chain(bridgeable_approved_nodes.iter())
        .chain(bridgeable_last_clean_nodes.iter())
        .chain(empty_verifier_pass_assessment_nodes.iter())
        .cloned()
        .collect();
    if recompute_set.is_empty() {
        return Ok(false);
    }

    // Migration must use the SAME mode that the live runtime will use
    // going forward, otherwise alignment checks compare apples to oranges
    // (v2-strict-computed approved baseline vs v2-permissive-computed
    // live fingerprint would never match). The mode is passed in by the
    // caller (`_if_enabled` wrapper resolves it from env at supervisor
    // start; tests pass it explicitly).
    let fresh: BTreeMap<NodeId, SoundFingerprintParts> = recompute_set
        .iter()
        .map(|node| {
            (
                node.clone(),
                match mode {
                    // V1 case is unreachable in production (caller filters)
                    // but keep the match exhaustive for safety.
                    SoundnessFingerprintMode::V1 => SoundFingerprintParts::default(),
                    SoundnessFingerprintMode::V2Strict => {
                        soundness_fingerprint_v2_parts_impl_with_context(
                            repo_path,
                            node,
                            /* permissive = */ false,
                            &node_kinds,
                            &node_role,
                        )
                    }
                    SoundnessFingerprintMode::V2Permissive => {
                        soundness_fingerprint_v2_parts_impl_with_context(
                            repo_path,
                            node,
                            /* permissive = */ true,
                            &node_kinds,
                            &node_role,
                        )
                    }
                },
            )
        })
        .collect();
    let mut changed = false;
    for (node, parts) in fresh {
        let fp = parts.combined_sound_fp.clone();
        if fp.trim().is_empty() {
            if empty_verifier_pass_assessment_nodes.contains(&node) {
                if state.sound_assessments.remove(&node).is_some() {
                    changed = true;
                }
                if state.sound_status.remove(&node).is_some() {
                    changed = true;
                }
                if state.sound_approved_fingerprints.remove(&node).is_some() {
                    changed = true;
                }
                if state
                    .live
                    .sound_current_fingerprints
                    .remove(&node)
                    .is_some()
                {
                    changed = true;
                }
                if state
                    .live
                    .sound_current_fingerprint_parts
                    .remove(&node)
                    .is_some()
                {
                    changed = true;
                }
                if state
                    .committed
                    .sound_current_fingerprints
                    .remove(&node)
                    .is_some()
                {
                    changed = true;
                }
                if state
                    .committed
                    .sound_current_fingerprint_parts
                    .remove(&node)
                    .is_some()
                {
                    changed = true;
                }
                let last_clean_pass = matches!(
                    state.last_clean_sound_status.get(&node),
                    Some(trellis_kernel::SoundStatus::Pass)
                );
                let last_clean_current_missing = state
                    .last_clean_live
                    .sound_current_fingerprints
                    .get(&node)
                    .map(|stored| stored.trim().is_empty())
                    .unwrap_or(true);
                if last_clean_pass && last_clean_current_missing {
                    if state.last_clean_sound_status.remove(&node).is_some() {
                        changed = true;
                    }
                    if state
                        .last_clean_sound_approved_fingerprints
                        .remove(&node)
                        .is_some()
                    {
                        changed = true;
                    }
                    if state
                        .last_clean_live
                        .sound_current_fingerprints
                        .remove(&node)
                        .is_some()
                    {
                        changed = true;
                    }
                    if state
                        .last_clean_live
                        .sound_current_fingerprint_parts
                        .remove(&node)
                        .is_some()
                    {
                        changed = true;
                    }
                }
            }
            continue;
        }
        if (old_live.contains_key(&node) || empty_verifier_pass_assessment_nodes.contains(&node))
            && old_live.get(&node) != Some(&fp)
        {
            state
                .live
                .sound_current_fingerprints
                .insert(node.clone(), fp.clone());
            changed = true;
        }
        if (old_live.contains_key(&node) || empty_verifier_pass_assessment_nodes.contains(&node))
            && state.live.sound_current_fingerprint_parts.get(&node) != Some(&parts)
        {
            state
                .live
                .sound_current_fingerprint_parts
                .insert(node.clone(), parts.clone());
            changed = true;
        }
        if (old_committed.contains_key(&node)
            || empty_verifier_pass_assessment_nodes.contains(&node))
            && old_committed.get(&node) != Some(&fp)
        {
            state
                .committed
                .sound_current_fingerprints
                .insert(node.clone(), fp.clone());
            changed = true;
        }
        if (old_committed.contains_key(&node)
            || empty_verifier_pass_assessment_nodes.contains(&node))
            && state.committed.sound_current_fingerprint_parts.get(&node) != Some(&parts)
        {
            state
                .committed
                .sound_current_fingerprint_parts
                .insert(node.clone(), parts.clone());
            changed = true;
        }
        if (aligned_nodes.contains(&node) || bridgeable_approved_nodes.contains(&node))
            && state.sound_approved_fingerprints.get(&node) != Some(&fp)
        {
            state
                .sound_approved_fingerprints
                .insert(node.clone(), fp.clone());
            changed = true;
        }
        if empty_verifier_pass_assessment_nodes.contains(&node) {
            let may_rebless_empty_pass = state
                .sound_approved_fingerprints
                .get(&node)
                .map(|approved| approved.trim().is_empty() || approved == &fp)
                .unwrap_or(true);
            if may_rebless_empty_pass {
                if state.sound_approved_fingerprints.get(&node) != Some(&fp) {
                    state
                        .sound_approved_fingerprints
                        .insert(node.clone(), fp.clone());
                    changed = true;
                }
                if let Some(assessment) = state.sound_assessments.get_mut(&node) {
                    if assessment.fingerprints != parts {
                        assessment.fingerprints = parts.clone();
                        changed = true;
                    }
                }
            }
        }

        // Safe LastClean mirror repair: only migrate entries whose old
        // LastClean sound fingerprint already matched the current live
        // fingerprint before migration. If LastClean points at older content,
        // leave the mirror untouched; a later load after an actual LastClean
        // reset can migrate against that restored disk state.
        if old_last_clean.get(&node).is_some()
            && old_last_clean.get(&node) == old_live.get(&node)
            && old_last_clean.get(&node) != Some(&fp)
        {
            state
                .last_clean_live
                .sound_current_fingerprints
                .insert(node.clone(), fp.clone());
            changed = true;
        }
        if old_last_clean.get(&node).is_some()
            && old_last_clean.get(&node) == old_live.get(&node)
            && state
                .last_clean_live
                .sound_current_fingerprint_parts
                .get(&node)
                != Some(&parts)
        {
            state
                .last_clean_live
                .sound_current_fingerprint_parts
                .insert(node.clone(), parts.clone());
            changed = true;
        }
        // Safe LastClean mirror-approved repair: re-bless the mirror's
        // approved baseline to the new format ONLY when the node's content
        // hasn't drifted since the clean checkpoint (mirror-current ==
        // live). The aligned/bridgeable membership alone is insufficient:
        // it establishes the mirror is internally consistent (or its
        // approved is the legacy hash of current disk text), but not that
        // mirror-current still tracks live. For a node edited since the
        // clean checkpoint, `fp` is the hash of CURRENT disk text, so
        // writing it here would poison mirror-approved with post-clean
        // content while the guarded mirror-current write above correctly
        // keeps the clean-text hash — a permanent mirror mismatch that
        // breaks the LastClean-lands-clean invariant. Gate on the same
        // content-undrifted condition the mirror-current write uses.
        if (aligned_last_clean_nodes.contains(&node) || bridgeable_last_clean_nodes.contains(&node))
            && old_last_clean.get(&node).is_some()
            && old_last_clean.get(&node) == old_live.get(&node)
            && state.last_clean_sound_approved_fingerprints.get(&node) != Some(&fp)
        {
            state
                .last_clean_sound_approved_fingerprints
                .insert(node, fp);
            changed = true;
        }
    }

    // Mirror self-heal for already-poisoned checkpoints. A Pass mirror
    // entry was captured with current == approved by the LastClean
    // cleanliness invariant (a node only enters the clean mirror at
    // Pass when its current fingerprint matched its approved baseline),
    // so any Pass-with-mismatch mirror is by construction corrupted —
    // the pre-guard write path above poisoned mirror-approved with
    // post-clean disk text while mirror-current correctly kept the
    // clean-text hash. Restore mirror-approved from mirror-current,
    // which is the value the guarded write path preserves. This runs on
    // every load so existing poisoned checkpoints self-correct on the
    // next supervisor start, independent of whether the recompute loop
    // above touched the node.
    let poisoned_pass_nodes: Vec<NodeId> = state
        .last_clean_sound_status
        .iter()
        .filter(|(node, status)| {
            **status == trellis_kernel::SoundStatus::Pass
                && match (
                    state.last_clean_live.sound_current_fingerprints.get(*node),
                    state.last_clean_sound_approved_fingerprints.get(*node),
                ) {
                    (Some(current), Some(approved)) => current != approved,
                    _ => false,
                }
        })
        .map(|(node, _)| node.clone())
        .collect();
    for node in poisoned_pass_nodes {
        if let Some(current) = state
            .last_clean_live
            .sound_current_fingerprints
            .get(&node)
            .cloned()
        {
            state
                .last_clean_sound_approved_fingerprints
                .insert(node, current);
            changed = true;
        }
    }

    Ok(changed)
}

/// Returns true iff `approved_hash` is the legacy-payload hash of the
/// node's CURRENT content under EITHER schema_tag variant (v2-permissive
/// or v2-strict). Used by the schema-bridge rescue: if an approval was
/// blessed under the pre-40e678b payload on content that hasn't drifted
/// since, the verdict still applies — we just need to re-bless it under
/// the new format.
fn approved_matches_legacy_payload(repo_path: &Path, node_name: &str, approved_hash: &str) -> bool {
    if approved_hash.trim().is_empty() {
        return false;
    }
    let legacy_perm = legacy_v2_payload_hash(repo_path, node_name, "schema:sound-v2-permissive");
    if !legacy_perm.is_empty() && legacy_perm == approved_hash {
        return true;
    }
    let legacy_strict = legacy_v2_payload_hash(repo_path, node_name, "schema:sound-v2");
    !legacy_strict.is_empty() && legacy_strict == approved_hash
}

/// One-time schema-bridge helper: compute what the pre-40e678b v2 hash
/// payload would produce for the node's CURRENT content using the given
/// schema_tag string. This is the OLD format — for forward-going
/// fingerprinting use `soundness_fingerprint_v2` / `_v2_permissive`,
/// which are schema_tag-free post-40e678b. This function is gate-free
/// (no import-closure check): the migration runs in both modes and
/// rescuing an approval under one variant is correct regardless of the
/// strict gate's current opinion of the node.
fn legacy_v2_payload_hash(repo_path: &Path, node_name: &str, schema_tag: &str) -> String {
    let tex_content =
        read_text_if_exists(&repo_path.join("Tablet").join(format!("{node_name}.tex")));
    if tex_content.trim().is_empty() {
        return String::new();
    }
    let refs = extract_tex_proof_noderefs(&tex_content);
    let mut parts = vec![
        schema_tag.to_string(),
        format!("node:{node_name}"),
        format!("self_tex:{}", hash_text(&tex_content)),
        format!(
            "noderefs:{}",
            refs.iter().cloned().collect::<Vec<_>>().join(",")
        ),
    ];
    for reference in refs {
        let ref_tex = extract_tex_statement_block(&read_text_if_exists(
            &repo_path.join("Tablet").join(format!("{reference}.tex")),
        ));
        parts.push(format!("noderef_stmt:{reference}:{}", hash_text(&ref_tex)));
    }
    format!("sound-v2:{}", hash_text(&parts.join("|")))
}

fn migrate_paper_axis(state: &mut crate::ProtocolState, repo_path: &Path) -> Result<(), String> {
    // M1: per-target alignment.
    let aligned_targets: BTreeSet<crate::TargetId> = state
        .paper_approved_fingerprints
        .iter()
        .filter(|(target, approved)| {
            state.live.paper_current_fingerprints.get(*target) == Some(*approved)
        })
        .map(|(target, _)| target.clone())
        .collect();
    let live_targets: BTreeSet<crate::TargetId> = state
        .live
        .paper_current_fingerprints
        .keys()
        .cloned()
        .collect();
    let committed_targets: BTreeSet<crate::TargetId> = state
        .committed
        .paper_current_fingerprints
        .keys()
        .cloned()
        .collect();
    if aligned_targets.is_empty() && live_targets.is_empty() && committed_targets.is_empty() {
        return Ok(());
    }

    // L_def per covering node, computed via the bin's closure walker.
    let covering_union: BTreeSet<NodeId> =
        state.live.coverage.values().flatten().cloned().collect();
    let l_def_per_covering = if covering_union.is_empty() {
        BTreeMap::new()
    } else {
        observe_lean_relevant_definition_descendants_per_node(repo_path, &covering_union)?
    };

    let fresh = crate::observe_paper_faithfulness_fingerprints(
        repo_path,
        &state.configured_targets,
        &state.target_claims,
        &state.live.present_nodes,
        &state.paper_approved_fingerprints,
        &l_def_per_covering,
    );
    for (target, fp) in &fresh {
        // M3: skip empty recomputes (covers both payload failure and the
        // lib's strict-completeness check — `observe_paper_faithfulness_fingerprints`
        // emits empty when any covering node is missing from the L_def
        // map, so we get partial-baseline protection for free here).
        if fp.trim().is_empty() {
            continue;
        }
        // M1: re-pin approved only for aligned targets.
        if aligned_targets.contains(target) {
            state
                .paper_approved_fingerprints
                .insert(target.clone(), fp.clone());
        }
        if live_targets.contains(target) {
            state
                .live
                .paper_current_fingerprints
                .insert(target.clone(), fp.clone());
        }
        // Committed mirror — see migrate_corr_axis for rationale.
        if committed_targets.contains(target) {
            state
                .committed
                .paper_current_fingerprints
                .insert(target.clone(), fp.clone());
        }
    }
    // M2: last_clean_paper_approved_fingerprints and
    // last_clean_live.paper_current_fingerprints intentionally untouched.
    Ok(())
}

/// List names of all nodes with a `.lean` file in `Tablet/`. Used to derive
/// the `present_strs` filter for `parse_lean_payload_into_closure_names`
/// without plumbing the kernel's `present_nodes` set through every fingerprint
/// computation site. Disk-derived listing is correct for fingerprint
/// computation: the kernel's `present_nodes` is itself a function of the
/// Tablet/ directory contents, modulo a brief transient window during commits.
fn list_tablet_node_names_from_disk(repo_path: &Path) -> BTreeSet<NodeId> {
    let mut out = BTreeSet::new();
    let dir = repo_path.join("Tablet");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("lean") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if !stem.is_empty() {
                out.insert(NodeId::from(stem.to_string()));
            }
        }
    }
    out
}

/// Compute `.tex` hashes of definition-kind descendants whose Lean meaning
/// is consumed by `node_name`'s lean_semantic_closure walk. Returns a map
/// `<descendant_name> -> hash(descendant's .tex statement block)`.
///
/// Relevance is derived directly from the Lean closure payload: if a const
/// from descendant `D` was visited while walking `node_name`'s declaration
/// (theorem types, definition values, inductive shapes, axiom types — proof
/// bodies excluded), then `D` is in the parent's Lean-meaning basis. The
/// existing `parse_lean_payload_into_closure_names` parser already does this
/// extraction (used by `observe_protected_closure_nodes`).
///
/// After Lean-relevance filtering, this further restricts to definition-kind
/// nodes (i.e., `tex_statement_environment` returns `"definition"`). Theorem-
/// kind descendants are intentionally excluded from the TeX-hash propagation
/// axis: their NL prose isn't part of the parent's verifier basis (the
/// parent's verifier consumes their TYPE via `lean_semantic_closure`, not
/// their TeX statement). Definition-kind descendants are different — their
/// TeX names a piece of mathematical content the parent's NL relies on by
/// reference, so changes to their TeX statement matter.
///
/// Returns an empty map if the payload is unavailable or unsuccessful;
/// callers that need a fallback to legacy behavior should detect payload
/// unavailability themselves rather than relying on the empty-set value.
/// Names of all project-defined Tablet nodes consumed by `node_name`'s
/// `lean_semantic_closure` walk, regardless of TeX environment. Filtered
/// only by `list_tablet_node_names_from_disk` (project membership) and
/// excluding the node itself / `Preamble`. Returns an empty set if the
/// payload is missing or not ok.
///
/// This is the **dispatch-eligibility primitive**: every node here is a
/// dependency whose Lean meaning the parent's verifier consults, so the
/// parent's corr cannot be soundly dispatched until each entry has
/// `corr_status == Pass`. Includes def, theorem, proposition, axiom kinds.
fn lean_relevant_dependency_names(
    repo_path: &Path,
    node_name: &str,
    payload: &LeanSemanticPayloadObservation,
) -> BTreeSet<NodeId> {
    if !payload.ok || payload.payload.is_empty() {
        return BTreeSet::new();
    }
    let self_set: BTreeSet<&str> = std::iter::once(node_name).collect();
    let present_nodes = list_tablet_node_names_from_disk(repo_path);
    let present_strs: BTreeSet<&str> = present_nodes.iter().map(|n| n.as_str()).collect();
    parse_lean_payload_into_closure_names(&payload.payload, &self_set, &present_strs)
}

/// Names of **definition-kind** nodes from `lean_relevant_dependency_names`,
/// filtered by `tex_statement_environment == "definition"`. This is the
/// L_def(N) primitive used for the TeX-hash propagation axis on the corr
/// and paper fingerprints. Theorems / propositions / axioms are dropped
/// because their TeX prose isn't part of the parent's verifier basis (the
/// parent uses their *type* via `lean_semantic_closure`).
fn lean_relevant_definition_descendant_names(
    repo_path: &Path,
    node_name: &str,
    payload: &LeanSemanticPayloadObservation,
) -> BTreeSet<NodeId> {
    let candidates = lean_relevant_dependency_names(repo_path, node_name, payload);
    let mut out = BTreeSet::new();
    for cand in candidates {
        let dep_tex = extract_tex_statement_block(&read_text_if_exists(
            &repo_path
                .join("Tablet")
                .join(format!("{}.tex", cand.as_str())),
        ));
        if dep_tex.is_empty() {
            continue;
        }
        if tex_statement_environment(&dep_tex) != "definition" {
            continue;
        }
        out.insert(cand);
    }
    out
}

/// For each input node N (typically a target's covering node), compute
/// L_def(N) — the set of definition-kind project nodes consumed by N's
/// Lean type-surface closure. Uses one `observe_lean_semantic_payloads`
/// call to amortise lake startup across the input set.
///
/// Map semantics:
/// - Entry **present**, possibly empty: L_def(N) was determined (an empty
///   set means N has no Lean-relevant definition descendants — legitimate
///   for leaf definitions). For `Preamble`, always an empty entry.
/// - Entry **absent**: payload extraction failed for N (lake hiccup,
///   missing `.lean`, etc.). Callers should treat this as "couldn't
///   determine"; in particular, paper-migration must NOT pin a fingerprint
///   covering N because its `lean_relevant_definition_descendants` axis
///   would be incomplete and silently disagree with future healthy
///   observations.
pub(crate) fn observe_lean_relevant_definition_descendants_per_node(
    repo_path: &Path,
    nodes: &BTreeSet<NodeId>,
) -> Result<BTreeMap<NodeId, BTreeSet<NodeId>>, String> {
    if nodes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let payload_nodes: BTreeSet<NodeId> = nodes
        .iter()
        .filter(|n| n.as_str() != "Preamble")
        .cloned()
        .collect();
    let payloads = observe_lean_semantic_payloads(repo_path, &payload_nodes)?;
    let mut out: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
    for node in nodes {
        if node.as_str() == "Preamble" {
            // Preamble has no closure walk — record a determined-empty set.
            out.insert(node.clone(), BTreeSet::new());
            continue;
        }
        match payloads.get(node.as_str()) {
            Some(payload) if payload.ok && !payload.payload.is_empty() => {
                out.insert(
                    node.clone(),
                    lean_relevant_definition_descendant_names(repo_path, node.as_str(), payload),
                );
            }
            // Payload missing or not-ok: omit from output so callers can
            // distinguish "determined empty" from "couldn't determine".
            _ => {}
        }
    }
    Ok(out)
}

/// Preamble-as-a-node fingerprint: preamble has no `.tex` statement block
/// per se, so `own_tex` is the hash of its full `.lean` body (proxy for the
/// declared preamble interface), and `preamble_tex` is the structured hash
/// of `Preamble.tex`. No Lean semantic closure (preamble is terminal in the
/// import graph and its role is environment setup, not a declared theorem).
fn preamble_correspondence_fingerprint(repo_path: &Path) -> String {
    let preamble_lean = read_text_if_exists(&repo_path.join("Tablet").join("Preamble.lean"));
    let preamble_tex = read_text_if_exists(&repo_path.join("Tablet").join("Preamble.tex"));
    let Some(structured_hash) = preamble_structured_hash(&preamble_tex) else {
        return String::new();
    };
    CorrespondenceFingerprint {
        own_tex: hash_text(&preamble_lean),
        lean_semantic_closure: String::new(),
        lean_relevant_definition_descendants: BTreeMap::new(),
        lean_relevant_dependencies: BTreeSet::new(),
        preamble_tex: structured_hash,
    }
    .to_storage_string()
}

fn assumptions_correspondence_fingerprint(repo_path: &Path) -> String {
    let assumptions_lean = read_text_if_exists(
        &repo_path
            .join("Tablet")
            .join(format!("{ASSUMPTIONS_NAME}.lean")),
    );
    let assumptions_tex = read_text_if_exists(
        &repo_path
            .join("Tablet")
            .join(format!("{ASSUMPTIONS_NAME}.tex")),
    );
    let has_staged_block =
        trellis_kernel::assumptions_registry::has_worker_authored_staged_assumption(repo_path)
            .unwrap_or(false);
    if !has_staged_block {
        return String::new();
    }
    let preamble_tex = read_text_if_exists(&repo_path.join("Tablet").join("Preamble.tex"));
    let preamble_tex_hash = preamble_structured_hash(&preamble_tex).unwrap_or_default();
    CorrespondenceFingerprint {
        own_tex: hash_text(&assumptions_tex),
        lean_semantic_closure: hash_text(&assumptions_lean),
        lean_relevant_definition_descendants: BTreeMap::new(),
        lean_relevant_dependencies: BTreeSet::new(),
        preamble_tex: preamble_tex_hash,
    }
    .to_storage_string()
}

/// Fingerprint used when the repo lacks a Lake project (so we cannot run
/// the Lean semantic-payload extractor). Still produces a CorrespondenceFingerprint-
/// shaped JSON string; the `lean_semantic_closure` field is a stable hash
/// derived from the `.lean` file's declaration-with-imports so that
/// lean-level changes still trigger a reopen on repos without a lake
/// project (used in unit tests + lightweight configs).
fn legacy_correspondence_fingerprint(
    repo_path: &Path,
    node_name: &str,
    under_model_assumption_nodes: &BTreeSet<NodeId>,
) -> String {
    if node_name == PREAMBLE_NAME {
        return preamble_correspondence_fingerprint(repo_path);
    }
    if is_under_model_assumption_observation_node(node_name, under_model_assumption_nodes) {
        return assumptions_correspondence_fingerprint(repo_path);
    }
    let tex_statement = extract_tex_statement_block(&read_text_if_exists(
        &repo_path.join("Tablet").join(format!("{node_name}.tex")),
    ));
    let lean_content =
        read_text_if_exists(&repo_path.join("Tablet").join(format!("{node_name}.lean")));
    if tex_statement.is_empty() || lean_content.trim().is_empty() {
        return String::new();
    }

    // Surrogate "lean semantic closure": hash the declaration-with-imports of
    // the node + each non-definition dependency's declaration-with-imports.
    // Not as tight as the real `lean-semantic-payloads` output, but adequate
    // for reopen detection in lake-less configs.
    let mut lean_parts = vec![
        format!("node:{node_name}"),
        hash_text(&extract_declaration_with_imports(&lean_content)),
    ];
    let mut deps = BTreeSet::new();
    // R4: this is the soundness/correspondence fingerprint machinery (Tier-2),
    // NOT the node-eval lane R4 routes. Pass `Lean` to preserve its exact
    // current behavior byte-for-byte; the fingerprint body still reads `.lean`
    // below. Isabelle routing of the fingerprint lane is the separate corr-plan
    // Phase M/I (unstarted), not R4.
    recursive_imports(
        repo_path,
        node_name,
        &mut deps,
        trellis_kernel::backend::BackendId::Lean,
    );
    deps.remove(node_name);
    for dep in &deps {
        let dep_lean = read_text_if_exists(&repo_path.join("Tablet").join(format!("{dep}.lean")));
        if !dep_lean.trim().is_empty() {
            lean_parts.push(format!(
                "dep_lean:{dep}:{}",
                hash_text(&extract_declaration_with_imports(&dep_lean))
            ));
        }
    }
    let preamble_lean = read_text_if_exists(&repo_path.join("Tablet").join("Preamble.lean"));
    if !preamble_lean.trim().is_empty() {
        lean_parts.push(format!("preamble_lean:{}", hash_text(&preamble_lean)));
    }

    let preamble_tex = read_text_if_exists(&repo_path.join("Tablet").join("Preamble.tex"));
    let preamble_tex_hash = preamble_structured_hash(&preamble_tex).unwrap_or_default();

    CorrespondenceFingerprint {
        own_tex: hash_text(&tex_statement),
        lean_semantic_closure: hash_text(&lean_parts.join("|")),
        lean_relevant_definition_descendants: BTreeMap::new(),
        lean_relevant_dependencies: BTreeSet::new(),
        preamble_tex: preamble_tex_hash,
    }
    .to_storage_string()
}

fn soundness_fingerprint(repo_path: &Path, node_name: &str) -> String {
    soundness_fingerprint_with_context(repo_path, node_name, &BTreeMap::new(), &BTreeMap::new())
}

fn soundness_fingerprint_with_context(
    repo_path: &Path,
    node_name: &str,
    node_kinds: &BTreeMap<NodeId, NodeKind>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> String {
    match soundness_fingerprint_mode() {
        SoundnessFingerprintMode::V1 => soundness_fingerprint_v1(repo_path, node_name),
        SoundnessFingerprintMode::V2Strict => {
            soundness_fingerprint_v2_parts_impl_with_context(
                repo_path, node_name, /* permissive = */ false, node_kinds, node_role,
            )
            .combined_sound_fp
        }
        SoundnessFingerprintMode::V2Permissive => soundness_fingerprint_v2_permissive_with_context(
            repo_path, node_name, node_kinds, node_role,
        ),
    }
}

fn soundness_fingerprint_parts(repo_path: &Path, node_name: &str) -> SoundFingerprintParts {
    soundness_fingerprint_parts_with_context(
        repo_path,
        node_name,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
}

fn soundness_fingerprint_parts_with_context(
    repo_path: &Path,
    node_name: &str,
    node_kinds: &BTreeMap<NodeId, NodeKind>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> SoundFingerprintParts {
    match soundness_fingerprint_mode() {
        SoundnessFingerprintMode::V1 => SoundFingerprintParts {
            own_tex_hash: String::new(),
            dep_statement_hashes: BTreeMap::new(),
            combined_sound_fp: soundness_fingerprint_v1(repo_path, node_name),
        },
        SoundnessFingerprintMode::V2Strict => soundness_fingerprint_v2_parts_impl_with_context(
            repo_path, node_name, /* permissive = */ false, node_kinds, node_role,
        ),
        SoundnessFingerprintMode::V2Permissive => soundness_fingerprint_v2_parts_impl_with_context(
            repo_path, node_name, /* permissive = */ true, node_kinds, node_role,
        ),
    }
}

fn soundness_fingerprint_v1(repo_path: &Path, node_name: &str) -> String {
    let tex_content =
        read_text_if_exists(&repo_path.join("Tablet").join(format!("{node_name}.tex")));
    if tex_content.trim().is_empty() {
        return String::new();
    }
    // R4: soundness fingerprint (Tier-2) — pass `Lean` to keep its current
    // behavior exact; Isabelle fingerprint routing is the corr plan, not R4.
    let direct_children = direct_imports(
        repo_path,
        node_name,
        trellis_kernel::backend::BackendId::Lean,
    );
    let mut parts = vec![
        format!("node:{node_name}"),
        format!("self_tex:{}", hash_text(&tex_content)),
        format!(
            "children:{}",
            direct_children
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        ),
    ];
    for child in direct_children {
        let child_tex = extract_tex_statement_block(&read_text_if_exists(
            &repo_path.join("Tablet").join(format!("{child}.tex")),
        ));
        if child == "Preamble" && child_tex.is_empty() {
            parts.push(format!("child_stmt:{child}:{}", hash_text("")));
            continue;
        }
        if child_tex.is_empty() {
            return String::new();
        }
        parts.push(format!("child_stmt:{child}:{}", hash_text(&child_tex)));
    }
    hash_text(&parts.join("|"))
}

pub fn soundness_fingerprint_v2(repo_path: &Path, node_name: &str) -> String {
    soundness_fingerprint_v2_impl(repo_path, node_name, /* permissive = */ false)
}

/// Permissive variant of v2: same hash structure but does not enforce the
/// `\noderef`-in-import-closure rule, and does not bail when a cited
/// node's `.tex` is missing (its statement contributes the empty-string
/// hash instead).
///
/// Whenever both modes WOULD succeed (i.e. v2-strict's import-closure
/// gate is satisfied and every cited statement's `.tex` is readable),
/// v2-permissive and v2-strict produce IDENTICAL hashes. This makes a
/// mode switch a no-op for nodes whose v2-permissive approval already
/// satisfied the strict rule — only nodes that v2-strict would reject
/// outright (return empty-string) are reopened on a switch to strict.
fn soundness_fingerprint_v2_permissive(repo_path: &Path, node_name: &str) -> String {
    soundness_fingerprint_v2_permissive_with_context(
        repo_path,
        node_name,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
}

fn soundness_fingerprint_v2_permissive_with_context(
    repo_path: &Path,
    node_name: &str,
    node_kinds: &BTreeMap<NodeId, NodeKind>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> String {
    soundness_fingerprint_v2_parts_impl_with_context(
        repo_path, node_name, /* permissive = */ true, node_kinds, node_role,
    )
    .combined_sound_fp
}

/// Shared implementation of the v2 hashing payload. The single parameter
/// `permissive` toggles the two strict checks that distinguish v2-strict
/// from v2-permissive: (1) `\noderef`-in-import-closure enforcement,
/// (2) bail-on-missing-noderef-tex. Both checks act as gates BEFORE the
/// hash payload is built — they return empty-string instead of a hash.
/// The hash payload itself is mode-independent, so a v2-permissive hash
/// equals the v2-strict hash on the same content.
///
/// The hashing reads from the per-node `.tex`:
/// * `self_tex` — the full `.tex` content (statement block + proof block).
///   The intended reopen semantics are "node X's `.tex` statement or
///   proof changed", which is implied by any byte change in `self_tex`.
/// * For each `\noderef`-cited reference: hash the reference's `.tex`
///   statement block when nonempty. If the cited node is definition-like
///   and has no TeX statement, hash its Lean source under a separate
///   domain. Proof-bearing cited nodes still require a nonempty TeX
///   statement in strict mode.
fn soundness_fingerprint_v2_impl(repo_path: &Path, node_name: &str, permissive: bool) -> String {
    soundness_fingerprint_v2_parts_impl(repo_path, node_name, permissive).combined_sound_fp
}

fn soundness_fingerprint_v2_parts_impl(
    repo_path: &Path,
    node_name: &str,
    permissive: bool,
) -> SoundFingerprintParts {
    soundness_fingerprint_v2_parts_impl_with_context(
        repo_path,
        node_name,
        permissive,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
}

fn soundness_fingerprint_v2_parts_impl_with_context(
    repo_path: &Path,
    node_name: &str,
    permissive: bool,
    node_kinds: &BTreeMap<NodeId, NodeKind>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> SoundFingerprintParts {
    let tex_content =
        read_text_if_exists(&repo_path.join("Tablet").join(format!("{node_name}.tex")));
    if tex_content.trim().is_empty() {
        return SoundFingerprintParts::default();
    }
    let refs = extract_tex_proof_noderefs(&tex_content);
    if !permissive && !soundness_noderefs_are_in_import_closure(repo_path, node_name, &refs) {
        return SoundFingerprintParts::default();
    }

    let own_tex_hash = hash_text(&tex_content);
    let mut parts = vec![
        format!("node:{node_name}"),
        format!("self_tex:{}", own_tex_hash),
        format!(
            "noderefs:{}",
            refs.iter().cloned().collect::<Vec<_>>().join(",")
        ),
    ];
    let mut dep_statement_hashes = BTreeMap::new();
    for reference in refs {
        let Some((dep_part, ref_hash)) =
            soundness_noderef_dependency_fingerprint(repo_path, &reference, node_kinds, node_role)
        else {
            if permissive {
                let ref_hash = hash_text("");
                parts.push(format!("noderef_stmt:{reference}:{}", ref_hash));
                dep_statement_hashes.insert(NodeId::from(reference), ref_hash);
                continue;
            }
            return SoundFingerprintParts::default();
        };
        parts.push(format!("noderef_{dep_part}:{reference}:{}", ref_hash));
        dep_statement_hashes.insert(NodeId::from(reference), ref_hash);
    }
    SoundFingerprintParts {
        own_tex_hash,
        dep_statement_hashes,
        combined_sound_fp: format!("sound-v2:{}", hash_text(&parts.join("|"))),
    }
}

fn soundness_noderef_dependency_fingerprint(
    repo_path: &Path,
    reference: &str,
    node_kinds: &BTreeMap<NodeId, NodeKind>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> Option<(String, String)> {
    let ref_tex = extract_tex_statement_block(&read_text_if_exists(
        &repo_path.join("Tablet").join(format!("{reference}.tex")),
    ));
    if !ref_tex.is_empty() {
        return Some(("stmt".to_string(), hash_text(&ref_tex)));
    }
    if !empty_tex_sound_noderef_may_use_lean(reference, node_kinds, node_role) {
        return None;
    }
    let ref_lean = read_text_if_exists(&repo_path.join("Tablet").join(format!("{reference}.lean")));
    if ref_lean.trim().is_empty() || !lean_node_is_definition_like(&ref_lean, reference) {
        return None;
    }
    Some((
        "lean_def".to_string(),
        hash_text(&format!("sound-noderef-lean-source-v1\0{ref_lean}")),
    ))
}

fn empty_tex_sound_noderef_may_use_lean(
    reference: &str,
    node_kinds: &BTreeMap<NodeId, NodeKind>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
) -> bool {
    let node = NodeId::from(reference);
    if matches!(
        node_role.get(&node),
        Some(trellis_kernel::PvRole::ExtractionModel)
    ) {
        return true;
    }
    match node_kinds.get(&node) {
        Some(NodeKind::Proof) => false,
        Some(NodeKind::Definition | NodeKind::Preamble) => true,
        None => true,
    }
}

fn lean_node_is_definition_like(lean_content: &str, node_name: &str) -> bool {
    let heads = trellis_kernel::filespec::declaration_heads(lean_content);
    let unqualified_node = node_name.rsplit('.').next().unwrap_or(node_name);
    heads.iter().any(|head| {
        let head_name = head
            .name
            .trim_end_matches(|c: char| c == ':' || c == '(' || c == '{' || c == '[' || c == ',');
        let unqualified_head = head_name.rsplit('.').next().unwrap_or(head_name);
        // Node file stems sanitize namespace dots to underscores (decl
        // `BiasedFp.zero_pow2` lives in node `BiasedFp_zero_pow2`), so the
        // sanitized head must match too. Without it, every model def with a
        // dotted declaration name failed this fallback, and the soundness
        // fingerprint of ANY proof citing one silently computed empty — the
        // node then vanished from `sound_current_fingerprint_parts`, so a
        // pinned Sound Fail on it could never self-clear on edit (verified
        // on dec2flt: parse_number_faithful and
        // parse_long_mantissa_suffix_correct, the two goal-critical nodes).
        (head_name == node_name
            || unqualified_head == unqualified_node
            || head_name.replace('.', "_") == node_name)
            && trellis_kernel::filespec::is_definition_kind(&head.kind)
    }) || lean_node_has_constant_declaration(lean_content, node_name)
}

fn lean_node_has_constant_declaration(lean_content: &str, node_name: &str) -> bool {
    let unqualified_node = node_name.rsplit('.').next().unwrap_or(node_name);
    lean_content.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with("--") {
            return false;
        }
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.len() < 2 || tokens[0] != "constant" {
            return false;
        }
        let name = tokens[1]
            .trim_end_matches(|c: char| c == ':' || c == '(' || c == '{' || c == '[' || c == ',');
        let unqualified_name = name.rsplit('.').next().unwrap_or(name);
        name == node_name || unqualified_name == unqualified_node
    })
}

fn extract_tex_proof_block(tex_content: &str) -> String {
    let Some((_, after_begin)) = tex_content.split_once("\\begin{proof}") else {
        return String::new();
    };
    after_begin
        .split("\\end{proof}")
        .next()
        .unwrap_or(after_begin)
        .trim()
        .to_string()
}

pub fn tex_proof_starts_with_sketch_marker(tex_content: &str) -> bool {
    for line in extract_tex_proof_block(tex_content).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return trimmed == "SKETCH:";
    }
    false
}

fn extract_tex_proof_noderefs(tex_content: &str) -> BTreeSet<String> {
    let proof = extract_tex_proof_block(tex_content);
    let mut out = BTreeSet::new();
    let mut rest = proof.as_str();
    while let Some(idx) = rest.find("\\noderef") {
        let after = &rest[idx + "\\noderef".len()..];
        let after_ws = after.trim_start();
        if let Some(after_open) = after_ws.strip_prefix('{') {
            if let Some(end) = after_open.find('}') {
                let target = after_open[..end].trim();
                if !target.is_empty() {
                    out.insert(target.to_string());
                }
                rest = &after_open[end + 1..];
                continue;
            }
        }
        rest = after;
    }
    out
}

fn soundness_noderefs_are_in_import_closure(
    repo_path: &Path,
    node_name: &str,
    refs: &BTreeSet<String>,
) -> bool {
    if refs.is_empty() {
        return true;
    }
    let mut closure = BTreeSet::new();
    // R4: this predicate gates the STRICT SOUNDNESS FINGERPRINT (defense in
    // depth alongside `noderef_closure_errors`), part of the Tier-2 corr/sound
    // lane — not the node-eval lane R4 routes. Pass `Lean` to preserve exact
    // current behavior; the leaf `.lean`/`.tex` checks below stay `.lean`-keyed.
    // Isabelle routing of this lane is the corr plan, not R4.
    recursive_imports(
        repo_path,
        node_name,
        &mut closure,
        trellis_kernel::backend::BackendId::Lean,
    );
    closure.remove(node_name);
    refs.iter().all(|reference| {
        let tablet_dir = repo_path.join("Tablet");
        closure.contains(reference)
            && tablet_dir.join(format!("{reference}.lean")).is_file()
            && tablet_dir.join(format!("{reference}.tex")).is_file()
    })
}

/// Per-bad-`\noderef{X}` human-readable rejection messages for the
/// citing node's `.tex` proof block. A `\noderef{X}` is rejected when
/// X is not in `node_name`'s Lean import closure, or when X's `.lean`
/// or `.tex` file is missing from the tablet. Returns an empty vector
/// when every reference is in-closure (or there are no references).
/// Used by `evaluate_node_observation` to hard-reject artifacts at
/// worker acceptance; mirrors the predicate
/// `soundness_noderefs_are_in_import_closure` (which still gates the
/// strict soundness fingerprint as defense in depth).
fn noderef_closure_errors(repo_path: &Path, node_name: &str, tex_content: &str) -> Vec<String> {
    let refs = extract_tex_proof_noderefs(tex_content);
    if refs.is_empty() {
        return Vec::new();
    }
    // R4 C5 (R-NODEREF): the import-closure walk + the cited-node leaf
    // existence checks route per target. `Lean` on the all-Lean live run ⇒
    // `Tablet/<X>.lean` + the Lean `import Tablet.X` parser, byte-for-byte; for
    // IsabelleHol the walk parses `imports Tablet_X` from `.thy` files and the
    // leaf check requires `Tablet/<X>.thy`. The diagnostic PROSE is left
    // unchanged (it names `.lean` / `import Tablet.X` — a known cosmetic
    // residual for Isabelle, called out in the report; the gate LOGIC is
    // correct for both backends).
    let target = trellis_kernel::tablet_target_for_repo(repo_path);
    let mut closure = BTreeSet::new();
    recursive_imports(repo_path, node_name, &mut closure, target);
    closure.remove(node_name);
    let mut errors = Vec::new();
    for reference in &refs {
        let in_closure = closure.contains(reference);
        let lean_exists = trellis_kernel::node_source_path(repo_path, reference, target).is_file();
        let tex_exists = repo_path
            .join("Tablet")
            .join(format!("{reference}.tex"))
            .is_file();
        if in_closure && lean_exists && tex_exists {
            continue;
        }
        // When the cited node transitively imports the citing node,
        // "add the import" is unsatisfiable advice — it would create an
        // import cycle. Name the cycle and drop the import suggestion.
        let mut reference_closure = BTreeSet::new();
        recursive_imports(repo_path, reference, &mut reference_closure, target);
        if !in_closure && lean_exists && reference_closure.contains(node_name) {
            errors.push(format!(
                "\\noderef{{{reference}}} in Tablet/{node_name}.tex proof block is not in {node_name}'s Lean import closure, and {reference} transitively imports {node_name}, so citing it would create an import cycle. Remove the noderef or restructure so the cited statement lives below {node_name}."
            ));
            continue;
        }
        errors.push(format!(
            "\\noderef{{{reference}}} in Tablet/{node_name}.tex proof block is not in {node_name}'s Lean import closure (or missing .tex/.lean). Add `import Tablet.{reference}` to Tablet/{node_name}.lean or remove the noderef."
        ));
    }
    errors
}

/// Compute the structured correspondence fingerprint for a node, returned as
/// a JSON-encoded `CorrespondenceFingerprint` string. Empty string is returned
/// when preconditions fail (no tex statement, no Lean semantic payload, or
/// Preamble hash preconditions unmet), matching the prior convention.
fn correspondence_fingerprint(
    repo_path: &Path,
    node_name: &str,
    payload: Option<&LeanSemanticPayloadObservation>,
    under_model_assumption_nodes: &BTreeSet<NodeId>,
) -> String {
    if node_name == PREAMBLE_NAME {
        return preamble_correspondence_fingerprint(repo_path);
    }
    if is_under_model_assumption_observation_node(node_name, under_model_assumption_nodes) {
        return assumptions_correspondence_fingerprint(repo_path);
    }
    let tex_statement = extract_tex_statement_block(&read_text_if_exists(
        &repo_path.join("Tablet").join(format!("{node_name}.tex")),
    ));
    if tex_statement.is_empty() {
        return String::new();
    }
    let Some(payload) = payload else {
        return String::new();
    };
    if !payload.ok || payload.payload.is_empty() {
        return String::new();
    }
    let preamble_tex = read_text_if_exists(&repo_path.join("Tablet").join("Preamble.tex"));
    let preamble_tex_hash = preamble_structured_hash(&preamble_tex).unwrap_or_default();
    // Single parse of the Lean semantic closure produces both axes:
    //   - lean_relevant_dependencies: all project-defined Tablet nodes the
    //     parent's verifier consults (used for topological dispatch).
    //   - lean_relevant_definition_descendants: subset filtered to def-kind
    //     nodes, with TeX-statement hashes (used for TeX-hash propagation).
    let dep_names = lean_relevant_dependency_names(repo_path, node_name, payload);
    let mut def_descendant_hashes: BTreeMap<String, String> = BTreeMap::new();
    for cand in &dep_names {
        let dep_tex = extract_tex_statement_block(&read_text_if_exists(
            &repo_path
                .join("Tablet")
                .join(format!("{}.tex", cand.as_str())),
        ));
        if dep_tex.is_empty() {
            continue;
        }
        if tex_statement_environment(&dep_tex) != "definition" {
            continue;
        }
        def_descendant_hashes.insert(cand.as_str().to_string(), hash_text(&dep_tex));
    }
    CorrespondenceFingerprint {
        own_tex: hash_text(&tex_statement),
        lean_semantic_closure: hash_text(&payload.payload),
        lean_relevant_definition_descendants: def_descendant_hashes,
        lean_relevant_dependencies: dep_names.into_iter().map(|n| n.into_string()).collect(),
        preamble_tex: preamble_tex_hash,
    }
    .to_storage_string()
}

fn correspondence_fingerprint_unavailable_reason(
    repo_path: &Path,
    node_name: &str,
    payload: Option<&LeanSemanticPayloadObservation>,
    under_model_assumption_nodes: &BTreeSet<NodeId>,
) -> Option<String> {
    if node_name == PREAMBLE_NAME
        || is_under_model_assumption_observation_node(node_name, under_model_assumption_nodes)
    {
        return None;
    }
    let tex_statement = extract_tex_statement_block(&read_text_if_exists(
        &repo_path.join("Tablet").join(format!("{node_name}.tex")),
    ));
    if tex_statement.is_empty() {
        return Some("missing `.tex` statement block".to_string());
    }
    match payload {
        None => Some("Lean semantic-payload response was missing for this node".to_string()),
        Some(payload) if !payload.ok => {
            let detail = payload.error.trim();
            if detail.is_empty() {
                Some("Lean semantic-payload extraction failed without a diagnostic".to_string())
            } else {
                Some(format!("Lean semantic-payload extraction failed: {detail}"))
            }
        }
        Some(payload) if payload.payload.is_empty() => {
            let detail = payload.error.trim();
            if detail.is_empty() {
                Some("Lean semantic-payload extraction returned an empty payload".to_string())
            } else {
                Some(format!(
                    "Lean semantic-payload extraction returned an empty payload: {detail}"
                ))
            }
        }
        Some(_) => None,
    }
}

fn corr_unavailable_reason_is_recoverable(reason: &str) -> bool {
    reason.starts_with("Lean semantic-payload ")
}

fn external_output(observation: &ExternalCommandObservation) -> String {
    let mut parts = Vec::new();
    if !observation.stdout.trim().is_empty() {
        parts.push(observation.stdout.trim().to_string());
    }
    if !observation.stderr.trim().is_empty() {
        parts.push(observation.stderr.trim().to_string());
    }
    if !observation.spawn_error.trim().is_empty() {
        parts.push(observation.spawn_error.trim().to_string());
    }
    if observation.timed_out {
        parts.push("timed out".to_string());
    }
    parts.join("\n")
}

fn command_ok(observation: &ExternalCommandObservation) -> bool {
    observation.returncode == Some(0)
        && !observation.timed_out
        && observation.spawn_error.is_empty()
}

fn is_valid_node_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && name != "Preamble"
        && name != "Axioms"
}

fn extract_marker_name(lean_content: &str) -> String {
    for line in lean_content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("-- [TABLET NODE: ") {
            if let Some(name) = rest.strip_suffix(']') {
                return name.trim().to_string();
            }
        }
    }
    String::new()
}

fn declaration_name(lean_content: &str) -> String {
    declaration_heads(lean_content)
        .into_iter()
        .next()
        .map(|head| head.name)
        .unwrap_or_default()
}

fn declaration_kind(lean_content: &str, node_name: &str) -> String {
    for head in declaration_heads(lean_content) {
        if head.name == node_name {
            // structure/inductive/class join def/abbrev as definition
            // (non-proof-bearing) kinds; instance is proof-bearing
            // (theorem_like), discharging the class's law fields.
            if trellis_kernel::filespec::is_definition_kind(&head.kind) {
                return "definition".to_string();
            }
            return "theorem_like".to_string();
        }
    }
    String::new()
}

pub(crate) fn find_declaration(lean_content: &str, node_name: &str) -> String {
    let mut decl_lines = Vec::new();
    let mut found = false;
    for line in lean_content.lines() {
        let trimmed = line.trim();
        if declaration_heads(trimmed)
            .into_iter()
            .next()
            .is_some_and(|head| head.name == node_name)
        {
            found = true;
            decl_lines.clear();
            decl_lines.push(trimmed.to_string());
            if line.contains(":=") {
                return decl_lines.join(" ");
            }
            continue;
        }
        if found {
            decl_lines.push(line.trim().to_string());
            if line.contains(":=") {
                return decl_lines.join(" ");
            }
        }
    }
    decl_lines.join(" ")
}

fn normalize_declaration(decl: &str) -> String {
    // Strip the body binding (`:=` and everything after). Use rfind so a
    // default-argument `:=` inside the signature stays intact — only the
    // trailing binding `:=` is removed. This keeps the hash stable across
    // proof-body edits regardless of what follows `:=` (by / sorry /
    // term-mode / `by sorry` placeholder / tactic blocks / etc.).
    let mut d = decl.trim().to_string();
    if let Some(pos) = d.rfind(":=") {
        d.truncate(pos);
        d = d.trim().to_string();
    }
    for prefix in [
        "Filter.",
        "Real.",
        "Nat.",
        "Int.",
        "Set.",
        "Finset.",
        "MeasureTheory.",
        "Topology.",
        "ENNReal.",
        "NNReal.",
    ] {
        d = d.replace(prefix, "");
    }
    d.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Legacy text-based declaration_hash — used by the cfg(test)
// `declaration_hash_for_gate` below and by the test fixtures in
// `proof_worker_delta_step_result`. Production builds route through
// `filespec_split::declaration_hash_strict` (FILESPEC-marker text scan).
#[cfg(test)]
fn declaration_hash(lean_content: &str, node_name: &str) -> String {
    let decl = find_declaration(lean_content, node_name);
    if decl.is_empty() {
        return String::new();
    }
    hash_bytes(normalize_declaration(&decl).as_bytes())
}

/// Consumer-side hash entry point for `evaluate_node_observation` and
/// the cleanup-step validators.
///
/// Production builds: routes through
/// `filespec_split::declaration_hash_strict` (FILESPEC-marker text scan
/// + matched normalisation) so the validity gate sees the strict hash
/// — catching `let X := …` signature drift that the legacy
/// find-the-first-`:=` text path missed on 79 / 377 live tablet nodes.
/// Errors on FILESPEC violations propagate so callers fail closed.
///
/// Test builds (`cfg(test)`): uses the local legacy `declaration_hash`
/// text path — keeps the existing test fixtures (synthetic repos lacking
/// the FILESPEC marker) green. Compile-time excluded from release
/// builds.
#[cfg(test)]
fn declaration_hash_for_gate(
    repo_path: &Path,
    lean_content: &str,
    node_name: &str,
) -> Result<String, String> {
    if is_structural_declaration_hash_node(node_name) {
        return Ok(whole_file_declaration_hash(lean_content));
    }
    // R4 C3 (decision #4): route the Tier-1 signature hash on target. Lean
    // fixtures keep the legacy text-based `declaration_hash` (the historical
    // test shim — byte-identical); IsabelleHol fixtures get the real
    // `isabelle_filespec::signature_hash` (the normalized principal-command
    // span hash) so a `.thy` declaration-lock check compares like-for-like.
    match trellis_kernel::tablet_target_for_repo(repo_path) {
        trellis_kernel::backend::BackendId::Lean => Ok(declaration_hash(lean_content, node_name)),
        target => trellis_kernel::backend::source_model_for(target).signature_hash(
            repo_path,
            lean_content,
            node_name,
        ),
    }
}

#[cfg(not(test))]
fn declaration_hash_for_gate(
    repo_path: &Path,
    lean_content: &str,
    node_name: &str,
) -> Result<String, String> {
    if is_structural_declaration_hash_node(node_name) {
        return Ok(whole_file_declaration_hash(lean_content));
    }
    // R4 C3 (Tier-1 source signature hash): route on target. `Lean` on the
    // all-Lean live run ⇒ `filespec_split::declaration_hash_strict` byte-for-
    // byte (the FILESPEC `-- BODY`-marker text scan); IsabelleHol ⇒
    // `isabelle_filespec::signature_hash`.
    trellis_kernel::backend::source_model_for(trellis_kernel::tablet_target_for_repo(repo_path))
        .signature_hash(repo_path, lean_content, node_name)
}

fn mask_comments_and_strings(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(chars.len());
    let mut i = 0usize;
    let mut block_depth = 0usize;
    while i < chars.len() {
        if block_depth > 0 {
            if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '-' {
                block_depth += 1;
                result.push(' ');
                result.push(' ');
                i += 2;
            } else if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1] == '/' {
                block_depth -= 1;
                result.push(' ');
                result.push(' ');
                i += 2;
            } else if chars[i] == '\n' {
                result.push('\n');
                i += 1;
            } else {
                result.push(' ');
                i += 1;
            }
        } else if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '-' {
            block_depth = 1;
            result.push(' ');
            result.push(' ');
            i += 2;
        } else if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1] == '-' {
            result.push(' ');
            result.push(' ');
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                result.push(' ');
                i += 1;
            }
        } else if chars[i] == '"' {
            result.push(' ');
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                result.push(if chars[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
            if i < chars.len() {
                result.push(' ');
                i += 1;
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

fn is_keyword_char(ch: Option<char>) -> bool {
    matches!(ch, Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '\'')
}

fn line_contains_keyword(line: &str, keyword: &str) -> bool {
    if keyword.is_empty() {
        return false;
    }
    let mut search_start = 0usize;
    while let Some(found) = line[search_start..].find(keyword) {
        let start = search_start + found;
        let end = start + keyword.len();
        let prev = line[..start].chars().next_back();
        let next = line[end..].chars().next();
        let valid = if keyword
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '\'')
        {
            !is_keyword_char(prev) && !is_keyword_char(next)
        } else {
            !is_keyword_char(prev) && !is_keyword_char(next)
        };
        if valid {
            return true;
        }
        search_start = end;
    }
    false
}

fn scan_forbidden_keywords(lean_content: &str) -> Vec<KeywordHit> {
    let masked = mask_comments_and_strings(lean_content);
    masked
        .lines()
        .zip(lean_content.lines())
        .enumerate()
        .flat_map(|(idx, (masked_line, original_line))| {
            FORBIDDEN_KEYWORDS
                .iter()
                .filter(move |keyword| line_contains_keyword(masked_line, keyword))
                .map(move |keyword| KeywordHit {
                    keyword: (*keyword).to_string(),
                    line: (idx + 1) as u32,
                    text: original_line.trim().to_string(),
                })
        })
        .collect()
}

fn forbidden_hits_include_textual_sorry(hits: &[KeywordHit]) -> bool {
    // This intentionally means the Lean `sorry` token in this node's own source,
    // not substrings such as `sorryAx` and not warnings emitted while compiling
    // imported nodes. A closed source must still be axiom-audited for transitive
    // `sorryAx` dependencies.
    hits.iter().any(|hit| hit.keyword == "sorry")
}

fn scan_sorry_in_definitions(lean_content: &str) -> Vec<SourceLineHit> {
    let masked = mask_comments_and_strings(lean_content);
    let mut hits = Vec::new();
    let mut in_def = false;
    for (idx, (masked_line, original_line)) in masked.lines().zip(lean_content.lines()).enumerate()
    {
        // Strip a leading inline `@[...]` attribute group so an attribute-
        // prefixed declaration head (e.g. the PV extraction-model
        // `@[global_simps, irreducible] def FIELD_MODULUS …`) is still
        // recognized as entering definition-mode. Parsing-only.
        let trimmed = trellis_kernel::filespec::strip_leading_attribute_groups(masked_line.trim());
        if trimmed.starts_with("def ")
            || trimmed.starts_with("noncomputable def ")
            || trimmed.starts_with("structure ")
            || trimmed.starts_with("inductive ")
            || trimmed.starts_with("class ")
        {
            in_def = true;
        } else if trimmed.starts_with("theorem ")
            || trimmed.starts_with("lemma ")
            || trimmed.starts_with("example ")
            || trimmed.starts_with("instance ")
        {
            in_def = false;
        }
        if in_def && line_contains_keyword(masked_line, "sorry") {
            hits.push(SourceLineHit {
                line: (idx + 1) as u32,
                text: original_line.trim().to_string(),
            });
        }
        if trimmed.is_empty() {
            in_def = false;
        }
    }
    hits
}

fn scan_preamble_definitions(lean_content: &str) -> Vec<SourceLineHit> {
    let masked = mask_comments_and_strings(lean_content);
    masked
        .lines()
        .zip(lean_content.lines())
        .enumerate()
        .filter_map(|(idx, (masked_line, original_line))| {
            // Attribute-aware: an inline `@[...]`-prefixed `def` in the
            // preamble is still a definition (which the preamble may not
            // contain). Parsing-only.
            let trimmed =
                trellis_kernel::filespec::strip_leading_attribute_groups(masked_line.trim());
            if trimmed.starts_with("def ") || trimmed.starts_with("noncomputable def ") {
                Some(SourceLineHit {
                    line: (idx + 1) as u32,
                    text: original_line.trim().to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn extract_imports(lean_content: &str) -> Vec<String> {
    lean_content
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import ").map(str::trim))
        .filter(|target| !target.is_empty())
        .map(str::to_string)
        .collect()
}

fn validate_imports(lean_content: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for imp in extract_imports(lean_content) {
        if imp.starts_with("Tablet.") {
            continue;
        }
        if ALLOWED_IMPORT_PREFIXES.iter().any(|prefix| imp == *prefix) {
            violations.push(format!(
                "{imp} (bare import not allowed -- use specific submodules like {imp}.SomeModule)"
            ));
            continue;
        }
        if ALLOWED_IMPORT_PREFIXES
            .iter()
            .any(|prefix| imp.starts_with(&format!("{prefix}.")))
        {
            continue;
        }
        violations.push(imp);
    }
    violations
}

/// B2c-gate Slice 3 / S6: backend-parameterized import-allowlist gate. For
/// `Lean` this calls the existing Lean `validate_imports` byte-for-byte (so the
/// live run is unchanged); for `IsabelleHol` it dispatches to
/// `isabelle_filespec::validate_imports` (the `Tablet_<Dep>` / `HOL`/`Main`
/// allowlist over the `.thy` `imports` clause).
fn validate_imports_for_target(
    target: trellis_kernel::backend::BackendId,
    source_content: &str,
) -> Vec<String> {
    match target {
        trellis_kernel::backend::BackendId::Lean => validate_imports(source_content),
        trellis_kernel::backend::BackendId::IsabelleHol => {
            trellis_kernel::isabelle_filespec::validate_imports(source_content)
        }
    }
}

/// B2c-gate Slice 3 / S6: backend-parameterized forbidden-keyword scan. For
/// `Lean` this calls the existing Lean `scan_forbidden_keywords` byte-for-byte;
/// for `IsabelleHol` it maps `isabelle_filespec::forbidden_keyword_line_hits`
/// (Isabelle-tokenizer-based, so comment/string/cartouche masking is intrinsic
/// and the S4 codegen-method / cert-diagnostic bans are included) into the
/// shared `KeywordHit` shape — including the synthetic `sorry` hit so the
/// caller's `sorry_free` derivation is correct for `.thy`.
fn scan_forbidden_keywords_for_target(
    target: trellis_kernel::backend::BackendId,
    source_content: &str,
) -> Vec<KeywordHit> {
    match target {
        trellis_kernel::backend::BackendId::Lean => scan_forbidden_keywords(source_content),
        trellis_kernel::backend::BackendId::IsabelleHol => {
            trellis_kernel::isabelle_filespec::forbidden_keyword_line_hits(source_content)
                .into_iter()
                .map(|(keyword, line, text)| KeywordHit {
                    keyword,
                    line,
                    text,
                })
                .collect()
        }
    }
}

fn is_lake_package_error(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    let pkg = [
        "url has changed",
        "permission denied",
        "cloning again",
        "deleting",
    ];
    let code = [
        "type mismatch",
        "unknown identifier",
        "unexpected token",
        "expected",
        "unsolved goals",
        "declaration uses",
    ];
    pkg.iter().any(|item| lowered.contains(item)) && !code.iter().any(|item| lowered.contains(item))
}

fn parse_print_axioms_output(output: &str) -> Option<Vec<String>> {
    let normalized = output.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.contains("does not depend on any axioms") {
        return Some(Vec::new());
    }
    let marker = "depends on axioms:";
    let start = normalized.find(marker)?;
    let after = normalized[start + marker.len()..].trim();
    let body = after.strip_prefix('[')?.split(']').next()?.trim();
    if body.is_empty() {
        return Some(Vec::new());
    }
    Some(
        body.split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// B2c-gate Slice 2 — the IsabelleHol SOUNDNESS cert gate. Consumes the
/// server-authored cert fields (`oracles_used` / `extra_shyps` /
/// `theorem_exists`) the checker injects into the `LocalClosureProbeOutput`
/// from a CHECKER-owned probe theory (Slice 1) and returns `Some(detail)` for
/// the FIRST violation, `None` when clean.
///
/// The arms, in deterministic order:
///   * `[oracle]` — ANY non-empty `oracles_used` not ⊆ the descriptor's
///     `approved_oracles_floor` (= ∅ for IsabelleHol). This is the PRIMARY
///     mechanical soundness catcher: it rejects BOTH `sorry`
///     (`oracle = skip_proof`) AND `smt`/`z3`/external-solver oracles. Confirmed
///     live (Slice-1 audit) that a `sorry` of `False` builds RC0 over
///     `use_theories` and surfaces ONLY here — the per-node build-RC alone is
///     NOT a `sorry` defense.
///   * `[shyps]` — non-empty `extra_shyps` (dangling sort hypotheses): the
///     empty-sort / inconsistent-type-class vacuous-`False` channel, invisible
///     to the build and the oracle gate.
///   * `[theorem]` — `!theorem_exists`: the cert could not resolve the node's
///     principal theorem by its fully-qualified name (`oops`/elision/shadow).
///
/// Caller-gated on `tablet_target_for_repo(repo) == IsabelleHol` — on the Lean
/// path this is never invoked (and would be vacuous anyway: Lean's probe carries
/// the three fields empty/false, so every arm is a no-op), keeping Lean
/// acceptance byte-identical. The `[statement]` (record-to-record statement-hash
/// stability) clause is NOT here: the gate site has no prior-record access; the
/// probe's `statement_hash` is stashed and recorded into the
/// `LocalClosureRecord.active_statement_hash` at the engine accept path (R1 —
/// recorded for diagnostics; drift-from-approved enforced by the record-
/// stability machinery, not by an equality-vs-source-hash check).
fn isabelle_cert_gate_violation(local: &LocalClosureProbeOutput, label: &str) -> Option<String> {
    let floor: BTreeSet<&str> =
        trellis_kernel::backend::descriptor_for(trellis_kernel::backend::BackendId::IsabelleHol)
            .approved_oracles_floor
            .iter()
            .copied()
            .collect();
    let offending_oracles: Vec<String> = local
        .oracles_used
        .iter()
        .filter(|o| !floor.contains(o.as_str()))
        .cloned()
        .collect();
    if !offending_oracles.is_empty() {
        return Some(format!(
            "[oracle] {label} proof reached the unapproved Isabelle oracle(s): {:?}. No oracle is approvable (`skip_proof`=`sorry`/`\\<proof>` and every external-solver oracle `smt`/`z3`/… are rejected); the proof must close under the kernel without an oracle.",
            offending_oracles
        ));
    }
    if !local.extra_shyps.is_empty() {
        return Some(format!(
            "[shyps] {label} proof carries dangling sort hypotheses {:?} (empty-sort / inconsistent-type-class): the theorem is vacuously derivable and is rejected. Discharge or remove the offending sort class.",
            local.extra_shyps.iter().cloned().collect::<Vec<_>>()
        ));
    }
    if !local.theorem_exists {
        return Some(format!(
            "[theorem] {label} principal theorem does not exist: the checker could not resolve the node's fact by its fully-qualified name (`oops`/elided/shadowed). The proof must produce a theorem named for the node."
        ));
    }
    None
}

pub(crate) fn load_approved_axioms(
    repo_path: &Path,
    node_name: &str,
) -> Result<BTreeSet<String>, String> {
    let mut approved: BTreeSet<String> = DEFAULT_APPROVED_AXIOMS
        .iter()
        .map(|item| (*item).to_string())
        .collect();
    // Provisional admission (operator decision, 2026-07-02): lane-passed
    // `status:"pending"` under-model assumptions count for closure
    // immediately; the end-of-TheoremStating AssumptionReview gate then
    // confirms permanently (→ APPROVED_AXIOMS.json) or revokes (→ status
    // "rejected", which shrinks this set — the effective-allowlist hash
    // derived from this function changes, and
    // `rescind_records_with_stale_approved_axioms_hash` rescinds every
    // closure record blessed under the revoked axiom). See
    // `assumptions_registry::lane_passed_pending_axioms` for the design
    // note. A registry parse error propagates (fail-closed: an unreadable
    // registry must never widen the allowlist).
    approved.extend(trellis_kernel::assumptions_registry::lane_passed_pending_axioms(repo_path)?);
    let approved_path = repo_path.join("APPROVED_AXIOMS.json");
    if !approved_path.exists() {
        return Ok(approved);
    }
    let raw = fs::read_to_string(&approved_path).map_err(|err| {
        format!(
            "Failed to load approved axioms from {}: {err}",
            approved_path.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|err| {
        format!(
            "Failed to parse approved axioms from {}: {err}",
            approved_path.display()
        )
    })?;
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(text) = item.as_str() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        approved.insert(trimmed.to_string());
                    }
                }
            }
            Ok(approved)
        }
        serde_json::Value::Object(obj) => {
            if let Some(global) = obj.get("global").and_then(|value| value.as_array()) {
                for item in global {
                    if let Some(text) = item.as_str() {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            approved.insert(trimmed.to_string());
                        }
                    }
                }
            }
            if let Some(nodes) = obj.get("nodes").and_then(|value| value.as_object()) {
                if let Some(node_items) = nodes.get(node_name).and_then(|value| value.as_array()) {
                    for item in node_items {
                        if let Some(text) = item.as_str() {
                            let trimmed = text.trim();
                            if !trimmed.is_empty() {
                                approved.insert(trimmed.to_string());
                            }
                        }
                    }
                }
            }
            Ok(approved)
        }
        _ => Err(format!(
            "Approved axioms file must be a JSON list or object: {}",
            approved_path.display()
        )),
    }
}

pub(crate) fn current_tablet_node_names(repo_path: &Path) -> BTreeSet<NodeId> {
    let tablet_dir = repo_path.join("Tablet");
    if !tablet_dir.exists() {
        return BTreeSet::new();
    }
    let Ok(entries) = fs::read_dir(&tablet_dir) else {
        return BTreeSet::new();
    };
    let mut lean_names: BTreeSet<NodeId> = BTreeSet::new();
    let mut tex_names: BTreeSet<NodeId> = BTreeSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        match path.extension().and_then(|value| value.to_str()) {
            Some("lean") if stem != "Axioms" => {
                lean_names.insert(NodeId::from(stem));
            }
            Some("tex") if stem != "header" => {
                tex_names.insert(NodeId::from(stem));
            }
            _ => {}
        }
    }
    lean_names.union(&tex_names).cloned().collect()
}

fn tablet_node_hash(repo_path: &Path, node: &str) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    let lean_path = repo_path.join("Tablet").join(format!("{node}.lean"));
    let tex_path = repo_path.join("Tablet").join(format!("{node}.tex"));
    hasher.update(fs::read(&lean_path).unwrap_or_default());
    hasher.update(b"\0");
    hasher.update(fs::read(&tex_path).unwrap_or_default());
    format!("{:x}", hasher.finalize())
}

fn extract_tablet_imports(lean_content: &str) -> BTreeSet<String> {
    lean_content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("import Tablet.")
                .map(str::trim)
                .filter(|suffix| !suffix.is_empty())
                .map(str::to_string)
        })
        .collect()
}

fn current_deps_from_repo(repo_path: &Path) -> BTreeMap<String, BTreeSet<String>> {
    current_tablet_node_names(repo_path)
        .into_iter()
        .map(|node| {
            let lean_path = repo_path.join("Tablet").join(format!("{node}.lean"));
            let deps = fs::read_to_string(&lean_path)
                .map(|content| extract_tablet_imports(&content))
                .unwrap_or_default();
            (node.into_string(), deps)
        })
        .collect()
}

fn import_closure(deps: &BTreeMap<String, BTreeSet<String>>, start: &str) -> BTreeSet<String> {
    let mut visited = BTreeSet::new();
    let mut stack = vec![start.to_string()];
    while let Some(node) = stack.pop() {
        if let Some(children) = deps.get(&node) {
            for child in children {
                if visited.insert(child.clone()) {
                    stack.push(child.clone());
                }
            }
        }
    }
    visited
}

fn reverse_import_closure(
    deps: &BTreeMap<String, BTreeSet<String>>,
    start: &str,
) -> BTreeSet<String> {
    let mut reverse: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (node, children) in deps {
        for child in children {
            reverse
                .entry(child.clone())
                .or_default()
                .insert(node.clone());
        }
    }
    let mut visited = BTreeSet::new();
    let mut stack = vec![start.to_string()];
    while let Some(node) = stack.pop() {
        if let Some(parents) = reverse.get(&node) {
            for parent in parents {
                if visited.insert(parent.clone()) {
                    stack.push(parent.clone());
                }
            }
        }
    }
    visited
}

fn active_support_cone_missing_new_nodes(
    repo_path: &Path,
    active_node: &str,
    new_nodes: &[String],
) -> Vec<String> {
    if new_nodes.is_empty() {
        return Vec::new();
    }
    let deps = current_deps_from_repo(repo_path);
    let supported = import_closure(&deps, active_node);
    let mut missing: Vec<String> = new_nodes
        .iter()
        .filter(|node| !supported.contains(*node))
        .cloned()
        .collect();
    missing.sort();
    missing
}

pub(crate) fn compute_target_impact_region(repo_path: &Path, node: &str) -> BTreeSet<String> {
    let current_names = current_tablet_node_names(repo_path);
    if !current_names.contains(node) {
        return BTreeSet::new();
    }
    let deps = current_deps_from_repo(repo_path);
    let mut region = BTreeSet::from([node.to_string()]);
    region.extend(import_closure(&deps, node));
    region.extend(reverse_import_closure(&deps, node));
    region
}

fn build_error_lines(output: &str, limit: usize) -> Vec<String> {
    output
        .lines()
        .filter(|line| line.to_ascii_lowercase().contains("error"))
        .take(limit)
        .map(str::to_string)
        .collect()
}

fn evaluate_assumptions_node_observation(observation: &NodeObservation) -> EvaluatedNode {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let compile_output = external_output(&observation.compile);

    if !observation.lean_exists {
        errors.push(format!("{} not found", observation.lean_path));
    }
    if !observation.tex_exists {
        errors.push(format!(
            "{} not found (Assumptions needs a paired .tex file)",
            observation.tex_path
        ));
    }

    let forbidden_hits = scan_forbidden_keywords_for_target(
        trellis_kernel::backend::BackendId::Lean,
        &observation.lean_content,
    );
    let non_sorry_hits: Vec<_> = forbidden_hits
        .iter()
        .filter(|hit| hit.keyword != "sorry" && hit.keyword != "axiom")
        .collect();
    let sorry_in_source = forbidden_hits_include_textual_sorry(&forbidden_hits);
    if sorry_in_source {
        errors.push("Assumptions.lean may not contain `sorry`".to_string());
    }
    if !non_sorry_hits.is_empty() {
        let keywords: Vec<_> = non_sorry_hits
            .iter()
            .map(|hit| hit.keyword.clone())
            .collect();
        errors.push(format!(
            "Forbidden keywords in Assumptions.lean: {:?}",
            keywords
        ));
    }

    let mut compiles = command_ok(&observation.compile);
    if !compiles && is_lake_package_error(&compile_output) {
        compiles = true;
        warnings.push("Lake package warning ignored".to_string());
    }
    if !compiles {
        let err_lines = build_error_lines(&compile_output, 20);
        errors.push(format!("Compilation failed:\n{}", err_lines.join("\n")));
    }
    let ok = errors.is_empty() && compiles && !sorry_in_source;
    EvaluatedNode {
        ok,
        shallow_ok: ok,
        compiles,
        sorry_free: !sorry_in_source,
        sorry_in_source,
        sorry_warnings: Vec::new(),
        imports_valid: true,
        keyword_clean: non_sorry_hits.is_empty() && !sorry_in_source,
        marker_valid: true,
        declaration_name_matches: true,
        declaration_intact: true,
        tex_format_valid: true,
        axioms_valid: true,
        audited_axioms: Vec::new(),
        axiom_violations: Vec::new(),
        import_violations: Vec::new(),
        warnings,
        errors,
        forbidden_hits,
        build_output: compile_output,
    }
}

fn evaluate_extraction_model_node_observation(
    repo_path: &Path,
    observation: &NodeObservation,
) -> EvaluatedNode {
    // PV ExtractionModel nodes are generated/challenge-pinned artifacts, not
    // ordinary authored Tablet nodes. Keep source/compile/import/keyword gates,
    // but do not impose ordinary marker, declaration-name, or `.tex` shape.
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let shape_target = trellis_kernel::tablet_target_for_repo(repo_path);
    let import_violations = validate_imports_for_target(shape_target, &observation.lean_content);
    let forbidden_hits =
        scan_forbidden_keywords_for_target(shape_target, &observation.lean_content);
    let definition_sorry_hits = scan_sorry_in_definitions(&observation.lean_content);
    let compile_output = external_output(&observation.compile);

    if !observation.lean_exists {
        errors.push(format!("{} not found", observation.lean_path));
    }
    if !import_violations.is_empty() {
        errors.push(format!("Unauthorized imports: {:?}", import_violations));
    }

    let non_sorry_hits: Vec<_> = forbidden_hits
        .iter()
        .filter(|hit| hit.keyword != "sorry")
        .collect();
    let sorry_ax_hits: Vec<_> = non_sorry_hits
        .iter()
        .filter(|hit| hit.keyword == "sorryAx")
        .collect();
    for hit in sorry_ax_hits {
        errors.push(format!(
            "sorryAx is forbidden, use sorry instead at line {}: {}",
            hit.line, hit.text
        ));
    }
    let other_non_sorry_hits: Vec<_> = non_sorry_hits
        .iter()
        .filter(|hit| hit.keyword != "sorryAx")
        .collect();
    if !other_non_sorry_hits.is_empty() {
        let keywords: Vec<_> = other_non_sorry_hits
            .iter()
            .map(|hit| hit.keyword.clone())
            .collect();
        errors.push(format!("Forbidden keywords: {:?}", keywords));
    }
    for hit in &definition_sorry_hits {
        errors.push(format!(
            "sorry in definition at line {}: {}",
            hit.line, hit.text
        ));
    }

    let mut compiles = command_ok(&observation.compile);
    if !compiles && is_lake_package_error(&compile_output) {
        compiles = true;
        warnings.push("Lake package warning ignored".to_string());
    }
    if !compiles {
        let err_lines = build_error_lines(&compile_output, 20);
        errors.push(format!("Compilation failed:\n{}", err_lines.join("\n")));
    }

    let sorry_in_source = forbidden_hits_include_textual_sorry(&forbidden_hits);
    let sorry_warnings: Vec<String> = compile_output
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("sorry")
                && (lower.contains("warning") || lower.contains("declaration uses"))
        })
        .map(str::to_string)
        .collect();
    let sorry_free = !sorry_in_source && sorry_warnings.is_empty();
    if !sorry_free {
        warnings.push("Node has sorry (open)".to_string());
    }

    let imports_valid = import_violations.is_empty();
    let keyword_clean = non_sorry_hits.is_empty() && definition_sorry_hits.is_empty();
    let ok = compiles && sorry_free && keyword_clean && imports_valid && observation.lean_exists;
    EvaluatedNode {
        ok,
        shallow_ok: ok,
        sorry_in_source,
        compiles,
        sorry_free,
        keyword_clean,
        imports_valid,
        declaration_intact: true,
        marker_valid: true,
        declaration_name_matches: true,
        tex_format_valid: true,
        axioms_valid: true,
        audited_axioms: Vec::new(),
        axiom_violations: Vec::new(),
        import_violations,
        forbidden_hits,
        sorry_warnings,
        errors,
        warnings,
        build_output: compile_output,
    }
}

pub(crate) fn evaluate_node_observation(
    repo_path: &Path,
    observation: &NodeObservation,
    expected_hash: Option<&str>,
) -> EvaluatedNode {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let expected_hash = expected_hash.unwrap_or("").trim();
    // R4: resolve the tablet-wide target once. `Lean` on the all-Lean live run ⇒
    // every routed helper below selects its Lean implementation byte-for-byte.
    let shape_target = trellis_kernel::tablet_target_for_repo(repo_path);
    let is_lean_target = shape_target == trellis_kernel::backend::BackendId::Lean;
    // R4 C1/C2 (decision #1): node-identity for an IsabelleHol `.thy` is `theory
    // Tablet_<Node>` + the principal-command name, enforced by
    // `validate_node_shape` (the `.lean shape errors:` branch below) — NOT by a
    // `-- [TABLET NODE: …]` marker (a `.thy` has none) nor by a Lean
    // declaration-head scan. So the marker / declaration-name identity checks
    // VACUOUS-PASS for Isabelle (return `node`, making `marker_valid` /
    // `declaration_name_matches` true). For Lean the existing scans run
    // byte-for-byte.
    let marker_name = if is_lean_target {
        extract_marker_name(&observation.lean_content)
    } else {
        observation.node.clone()
    };
    let declaration_name = if is_lean_target {
        declaration_name(&observation.lean_content)
    } else {
        observation.node.clone()
    };
    // Only compute the hash when there's something to compare against —
    // saves a socket round-trip per node for observations with no
    // baseline (e.g. first-cycle gates, test fixtures passing `None`).
    let declaration_hash = if expected_hash.is_empty() {
        String::new()
    } else {
        match declaration_hash_for_gate(repo_path, &observation.lean_content, &observation.node) {
            Ok(h) => h,
            Err(e) => {
                errors.push(format!("Declaration hash extraction failed: {e}"));
                String::new()
            }
        }
    };
    // R4 C4 (decision #2): the def/theorem kind classifier, via the
    // `SourceModel::declaration_kind` seam (parallel `classify_declaration`).
    // Lean: the declaration-head scan, byte-identical to the local
    // `declaration_kind`. IsabelleHol: the principal-command keyword class. The
    // result feeds the two declaration-kind ↔ tex-environment consistency
    // checks below.
    let declaration_kind = trellis_kernel::backend::source_model_for(shape_target)
        .declaration_kind(&observation.lean_content, &observation.node);
    // B2c-gate Slice 3 / S6: the shape + import gate is backend-parameterized.
    // `shape_target` resolves to `Lean` for the live run ⇒ the Lean shape
    // validator / `validate_imports` / forbidden scan are selected byte-for-
    // byte; for an IsabelleHol tablet the IsabelleHol descriptor-driven gate
    // runs. `shape_target` / `is_lean_target` are computed at the top of this
    // function (R4) and reused here.
    let lean_shape_errors = trellis_kernel::backend::source_model_for(shape_target)
        .validate_node_shape(&observation.lean_content, &observation.node);
    // R4 C8 (decision implied by Group C site map): the declaration-KIND shape
    // rules (anonymous-`instance` ban, `inductive` `where`-form) are Lean
    // FILESPEC syntax with no Isabelle analogue, so they are vacuous-empty for
    // Isabelle (the Isabelle shape constraints live in `validate_node_shape`).
    let declaration_kind_shape_errors = if is_lean_target {
        validate_declaration_kind_shape(&observation.lean_content, &observation.node)
    } else {
        Vec::new()
    };
    let tex_environment = tex_statement_environment(&observation.tex_content);
    let tex_format_errors = validate_tex_format(&observation.tex_content, false);
    let import_violations = validate_imports_for_target(shape_target, &observation.lean_content);
    let forbidden_hits =
        scan_forbidden_keywords_for_target(shape_target, &observation.lean_content);
    let definition_sorry_hits = scan_sorry_in_definitions(&observation.lean_content);
    let definition_with_proof_env = declaration_kind == "definition"
        && is_proof_bearing_statement_environment(&tex_environment);
    let theorem_like_with_definition_env =
        declaration_kind == "theorem_like" && tex_environment == "definition";
    let compile_output = external_output(&observation.compile);

    if !observation.lean_exists {
        errors.push(format!("{} not found", observation.lean_path));
    }
    if !observation.tex_exists {
        errors.push(format!(
            "{} not found (every node needs a .tex file)",
            observation.tex_path
        ));
    }

    let mut declaration_intact = true;
    if !expected_hash.is_empty() && declaration_hash != expected_hash {
        declaration_intact = false;
        errors.push(format!(
            "Declaration signature changed (expected {}... got {}...)",
            &expected_hash[..expected_hash.len().min(16)],
            &declaration_hash[..declaration_hash.len().min(16)]
        ));
        errors.push(
            "Only the proof body (after :=) may be modified, not the theorem statement; adding or retuning a `set_option` at the declaration is a common trigger — FILESPEC's heartbeat-placement section gives the legal placements."
                .to_string(),
        );
    }

    if !import_violations.is_empty() {
        errors.push(format!("Unauthorized imports: {:?}", import_violations));
    }
    if marker_name != observation.node {
        errors.push(format!(
            "Marker says {:?}, expected {:?}",
            marker_name, observation.node
        ));
    }
    if !trellis_kernel::filespec::decl_name_matches_node(&declaration_name, &observation.node) {
        errors.push(format!(
            "Declaration name is {:?}, expected {:?}",
            declaration_name, observation.node
        ));
    }
    if !lean_shape_errors.is_empty() {
        errors.push(format!(".lean shape errors: {:?}", lean_shape_errors));
    }
    for kind_shape_error in &declaration_kind_shape_errors {
        errors.push(kind_shape_error.clone());
    }
    if !tex_format_errors.is_empty() {
        errors.push(format!(".tex format errors: {:?}", tex_format_errors));
    }
    // Hard-reject `\noderef{X}` references whose target isn't in the
    // citing node's Lean import closure. Previously enforced only
    // indirectly via `soundness_fingerprint_v2_impl` returning an
    // empty fingerprint; now a deterministic acceptance-time error so
    // such artifacts never land and Sound verifier prompts can stop
    // re-deriving the rule.
    errors.extend(noderef_closure_errors(
        repo_path,
        &observation.node,
        &observation.tex_content,
    ));
    if definition_with_proof_env {
        errors.push(format!(
            "Lean declaration is definition-like but .tex uses proof-bearing env `{}`; proof-bearing nodes must use theorem-like Lean declarations.",
            tex_environment
        ));
    } else if theorem_like_with_definition_env {
        errors.push("The .tex statement uses definition but the Lean declaration is theorem-like; do not use theorem/lemma/corollary/helper nodes as disguised definitions.".to_string());
    }

    let non_sorry_hits: Vec<_> = forbidden_hits
        .iter()
        .filter(|hit| hit.keyword != "sorry")
        .collect();
    let sorry_ax_hits: Vec<_> = non_sorry_hits
        .iter()
        .filter(|hit| hit.keyword == "sorryAx")
        .collect();
    for hit in sorry_ax_hits {
        errors.push(format!(
            "sorryAx is forbidden, use sorry instead at line {}: {}",
            hit.line, hit.text
        ));
    }
    let other_non_sorry_hits: Vec<_> = non_sorry_hits
        .iter()
        .filter(|hit| hit.keyword != "sorryAx")
        .collect();
    if !other_non_sorry_hits.is_empty() {
        let keywords: Vec<_> = other_non_sorry_hits
            .iter()
            .map(|hit| hit.keyword.clone())
            .collect();
        errors.push(format!("Forbidden keywords: {:?}", keywords));
    }
    for hit in &definition_sorry_hits {
        errors.push(format!(
            "sorry in definition at line {}: {}",
            hit.line, hit.text
        ));
    }

    let mut compiles = command_ok(&observation.compile);
    if !compiles && is_lake_package_error(&compile_output) {
        compiles = true;
        warnings.push("Lake package warning ignored".to_string());
    }
    if !compiles {
        let err_lines = build_error_lines(&compile_output, 20);
        errors.push(format!("Compilation failed:\n{}", err_lines.join("\n")));
    }

    let sorry_in_source = forbidden_hits_include_textual_sorry(&forbidden_hits);
    let sorry_warnings: Vec<String> = compile_output
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("sorry")
                && (lower.contains("warning") || lower.contains("declaration uses"))
        })
        .map(str::to_string)
        .collect();
    let sorry_free = !sorry_in_source && sorry_warnings.is_empty();
    if !sorry_free {
        warnings.push("Node has sorry (open)".to_string());
    }

    let imports_valid = import_violations.is_empty();
    let keyword_clean = non_sorry_hits.is_empty() && definition_sorry_hits.is_empty();
    let marker_valid = marker_name == observation.node;
    let declaration_name_matches =
        trellis_kernel::filespec::decl_name_matches_node(&declaration_name, &observation.node);
    let tex_format_valid = tex_format_errors.is_empty();
    let mut axioms_valid = true;
    let mut audited_axioms = Vec::new();
    let mut axiom_violations = Vec::new();
    // R4 C9 (decision #3): the `#print axioms` audit is the Lean axiom-closure
    // soundness model. For IsabelleHol the soundness gate is the server-injected
    // ORACLE/SHYPS/THEOREM cert (`isabelle_cert_gate_violation`, B2c Slice 2),
    // NOT a Lean `#print axioms` parse — and `should_run_axiom_audit` never
    // populates `print_axioms` for Isabelle anyway. So this block is Lean-only;
    // `axioms_valid` stays vacuously `true` for Isabelle (matching the Slice-2
    // axiom-ceiling drop). Running the Lean-shaped `parse_print_axioms_output`
    // over an Isabelle node would false-error.
    if is_lean_target
        && compiles
        && sorry_free
        && keyword_clean
        && imports_valid
        && declaration_intact
        && marker_valid
        && declaration_name_matches
        && tex_format_valid
    {
        if command_ok(&observation.print_axioms) {
            let raw_axioms = external_output(&observation.print_axioms);
            match parse_print_axioms_output(&raw_axioms) {
                Some(axioms) => {
                    audited_axioms = axioms;
                    match load_approved_axioms(
                        Path::new(&observation.lean_path)
                            .parent()
                            .and_then(Path::parent)
                            .unwrap_or_else(|| Path::new(".")),
                        &observation.node,
                    ) {
                        Ok(approved) => {
                            axiom_violations = audited_axioms
                                .iter()
                                .filter(|axiom| !approved.contains(*axiom))
                                .cloned()
                                .collect();
                            if !axiom_violations.is_empty() {
                                axioms_valid = false;
                                let mut message = format!(
                                    "Axiom audit failed: Unapproved axioms: {:?}",
                                    axiom_violations
                                );
                                if axiom_violations.iter().any(|axiom| axiom == "sorryAx") {
                                    message.push(' ');
                                    message.push_str(SORRY_AX_REJECTION_REMINDER);
                                }
                                errors.push(message);
                            }
                        }
                        Err(err) => {
                            axioms_valid = false;
                            errors.push(format!("Axiom audit failed: {err}"));
                        }
                    }
                }
                None => {
                    axioms_valid = false;
                    errors.push(format!(
                        "Axiom audit failed: Could not parse `#print axioms` output for {}",
                        observation.node
                    ));
                }
            }
        } else {
            axioms_valid = false;
            errors.push(format!(
                "Axiom audit failed: {}",
                external_output(&observation.print_axioms)
            ));
        }
    }

    EvaluatedNode {
        ok: compiles
            && sorry_free
            && keyword_clean
            && imports_valid
            && declaration_intact
            && marker_valid
            && declaration_name_matches
            && tex_format_valid
            && axioms_valid,
        // Shallow ok: drops sorry_warnings (transitive) and axioms_valid
        // (transitive). Matches `open_nodes_from_repo`'s shallow textual
        // check on the node's own .lean file.
        shallow_ok: compiles
            && !sorry_in_source
            && keyword_clean
            && imports_valid
            && declaration_intact
            && marker_valid
            && declaration_name_matches
            && tex_format_valid,
        sorry_in_source,
        compiles,
        sorry_free,
        keyword_clean,
        imports_valid,
        declaration_intact,
        marker_valid,
        declaration_name_matches,
        tex_format_valid,
        axioms_valid,
        audited_axioms,
        axiom_violations,
        import_violations,
        forbidden_hits,
        sorry_warnings,
        errors,
        warnings,
        build_output: compile_output,
    }
}

fn append_error(
    errors: &mut Vec<String>,
    records: &mut Vec<ErrorRecord>,
    message: String,
    owner: Option<String>,
) {
    errors.push(message.clone());
    records.push(ErrorRecord { message, owner });
}

fn evaluate_tablet_observation_impl(
    repo_path: &Path,
    observation: &TabletObservation,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
    assumption_authoring_node: Option<&NodeId>,
) -> EvaluatedTablet {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut error_records = Vec::new();
    let mut nodes = BTreeMap::new();

    if !observation.tablet_exists {
        append_error(
            &mut errors,
            &mut error_records,
            format!("{} not found", repo_path.join("Tablet").display()),
            None,
        );
        return EvaluatedTablet {
            ok: false,
            errors,
            warnings,
            error_records,
            build_output: external_output(&observation.build),
            nodes,
        };
    }

    if !observation.preamble.lean_exists {
        append_error(
            &mut errors,
            &mut error_records,
            format!(
                "{} not found",
                repo_path.join("Tablet").join("Preamble.lean").display()
            ),
            None,
        );
    } else {
        if !scan_preamble_definitions(&observation.preamble.lean_content).is_empty() {
            append_error(
                &mut errors,
                &mut error_records,
                "Preamble.lean may only contain imports".to_string(),
                None,
            );
        }
        let preamble_import_violations = validate_imports(&observation.preamble.lean_content);
        if !preamble_import_violations.is_empty() {
            append_error(
                &mut errors,
                &mut error_records,
                format!(
                    "Preamble has unauthorized imports: {:?}",
                    preamble_import_violations
                ),
                None,
            );
        }
    }
    let preamble_tex_errors = validate_tex_format(&observation.preamble.tex_content, true);
    if observation.preamble.tex_exists && !preamble_tex_errors.is_empty() {
        append_error(
            &mut errors,
            &mut error_records,
            format!("Preamble: .tex format errors: {:?}", preamble_tex_errors),
            None,
        );
    }

    for name in &observation.invalid_node_names {
        append_error(
            &mut errors,
            &mut error_records,
            format!("Invalid node name: {}", name),
            Some(name.clone()),
        );
    }
    for name in &observation.missing_tex_for_lean {
        append_error(
            &mut errors,
            &mut error_records,
            format!(
                "{} not found",
                repo_path
                    .join("Tablet")
                    .join(format!("{name}.tex"))
                    .display()
            ),
            Some(name.clone()),
        );
    }

    for (name, node_observation) in &observation.nodes {
        let semantics =
            tablet_node_validation_semantics(name, node_role, assumption_authoring_node);
        if semantics == TabletNodeValidationSemantics::Ordinary {
            let marker_name = extract_marker_name(&node_observation.lean_content);
            if marker_name != *name {
                append_error(
                    &mut errors,
                    &mut error_records,
                    format!(
                        "{}: marker says {:?}, expected {:?}",
                        name, marker_name, name
                    ),
                    Some(name.clone()),
                );
            }
            let declared_name = declaration_name(&node_observation.lean_content);
            if !trellis_kernel::filespec::decl_name_matches_node(&declared_name, name) {
                append_error(
                    &mut errors,
                    &mut error_records,
                    format!(
                        "{}: declaration name is {:?}, expected {:?}",
                        name, declared_name, name
                    ),
                    Some(name.clone()),
                );
            }
            let tex_errors = validate_tex_format(&node_observation.tex_content, false);
            if !tex_errors.is_empty() {
                append_error(
                    &mut errors,
                    &mut error_records,
                    format!("{}: .tex format errors: {:?}", name, tex_errors),
                    Some(name.clone()),
                );
            }
        }
        let evaluated = match semantics {
            TabletNodeValidationSemantics::Ordinary => {
                evaluate_node_observation(repo_path, node_observation, None)
            }
            TabletNodeValidationSemantics::ExtractionModel => {
                evaluate_extraction_model_node_observation(repo_path, node_observation)
            }
            TabletNodeValidationSemantics::UnderModelAssumptions => {
                evaluate_assumptions_node_observation(node_observation)
            }
        };
        nodes.insert(name.clone(), evaluated.clone());
        for err in evaluated.errors {
            append_error(
                &mut errors,
                &mut error_records,
                format!("{name}: {err}"),
                Some(name.clone()),
            );
        }
        warnings.extend(
            evaluated
                .warnings
                .into_iter()
                .map(|warning| format!("{name}: {warning}")),
        );
    }

    for name in &observation.orphan_tex_nodes {
        append_error(
            &mut errors,
            &mut error_records,
            format!(
                "{} not found (every .tex node needs a matching .lean file)",
                repo_path
                    .join("Tablet")
                    .join(format!("{name}.lean"))
                    .display()
            ),
            None,
        );
    }

    let build_output = external_output(&observation.build);
    if !command_ok(&observation.build) && !is_lake_package_error(&build_output) {
        let err_lines = build_error_lines(&build_output, 10);
        append_error(
            &mut errors,
            &mut error_records,
            format!(
                "lake build Tablet failed{}",
                if err_lines.is_empty() {
                    String::new()
                } else {
                    format!(": {}", err_lines.join(" | "))
                }
            ),
            None,
        );
    } else if !command_ok(&observation.build) {
        warnings.push("lake build Tablet reported Lake package noise".to_string());
    }

    EvaluatedTablet {
        ok: errors.is_empty(),
        errors,
        warnings,
        error_records,
        build_output,
        nodes,
    }
}

pub(crate) fn evaluate_tablet_observation(
    repo_path: &Path,
    observation: &TabletObservation,
) -> EvaluatedTablet {
    evaluate_tablet_observation_impl(repo_path, observation, &BTreeMap::new(), None)
}

pub(crate) fn evaluate_tablet_observation_with_assumption_authoring(
    repo_path: &Path,
    observation: &TabletObservation,
    assumption_authoring_node: Option<&NodeId>,
) -> EvaluatedTablet {
    evaluate_tablet_observation_impl(
        repo_path,
        observation,
        &BTreeMap::new(),
        assumption_authoring_node,
    )
}

pub(crate) fn evaluate_tablet_observation_with_roles(
    repo_path: &Path,
    observation: &TabletObservation,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
    assumption_authoring_node: Option<&NodeId>,
) -> EvaluatedTablet {
    evaluate_tablet_observation_impl(repo_path, observation, node_role, assumption_authoring_node)
}

pub(crate) fn relevant_new_errors(
    evaluated: &EvaluatedTablet,
    baseline_errors: &[String],
    allowed_nodes: &BTreeSet<NodeId>,
) -> Vec<String> {
    let baseline: BTreeSet<String> = baseline_errors.iter().cloned().collect();
    evaluated
        .error_records
        .iter()
        .filter(|record| {
            !baseline.contains(&record.message)
                && match record.owner.as_deref() {
                    None => true,
                    Some(owner) => allowed_nodes.contains(owner),
                }
        })
        .map(|record| record.message.clone())
        .collect()
}

fn evaluation_errors_for_nodes(
    evaluated: &EvaluatedTablet,
    allowed_nodes: &BTreeSet<NodeId>,
) -> Vec<String> {
    evaluated
        .error_records
        .iter()
        .filter(|record| {
            record
                .owner
                .as_deref()
                .is_some_and(|owner| allowed_nodes.contains(owner))
        })
        .map(|record| record.message.clone())
        .collect()
}

pub(crate) fn proof_easy_scope_step_result(
    repo_path: &Path,
    active_node: &str,
    before_snapshot: &BTreeMap<String, String>,
) -> WorkerValidationStepResult {
    let changes = detect_snapshot_changes(before_snapshot, &snapshot_tablet_dir(repo_path));
    let active_lean = format!("{active_node}.lean");
    let mut errors = Vec::new();
    if !changes.deleted.is_empty() {
        errors.push(format!(
            "Easy mode does not allow deleting files: {:?}",
            changes.deleted
        ));
    }
    // Carve-out: created `.lean`+`.tex` pairs that are Lean-closed and
    // inside the active node's support cone are allowed in Easy. Workers
    // may extract a clean lemma into its own helper without escalating to
    // hard local, as long as the helper introduces zero new open
    // obligations. Helpers carrying `sorry` still require hard local.
    let new_lean_nodes: BTreeSet<String> = changes
        .created
        .iter()
        .filter(|n| n.ends_with(".lean"))
        .filter_map(|n| node_name_from_tablet_file(n))
        .collect();
    let new_tex_nodes: BTreeSet<String> = changes
        .created
        .iter()
        .filter(|n| n.ends_with(".tex") && *n != "header.tex" && *n != "Preamble.tex")
        .filter_map(|n| node_name_from_tablet_file(n))
        .collect();
    let paired: Vec<String> = new_lean_nodes
        .intersection(&new_tex_nodes)
        .cloned()
        .collect();
    let cone_missing: BTreeSet<String> =
        active_support_cone_missing_new_nodes(repo_path, active_node, &paired)
            .into_iter()
            .collect();
    let allowed_helpers: BTreeSet<String> = paired
        .into_iter()
        .filter(|n| !cone_missing.contains(n))
        .filter(|n| {
            observe_node(repo_path, n)
                .map(|obs| evaluate_node_observation(repo_path, &obs, None).sorry_free)
                .unwrap_or(false)
        })
        .collect();
    let created_content_files: Vec<_> = changes
        .created
        .iter()
        .filter(|name| name.ends_with(".lean") || name.ends_with(".tex"))
        .filter(|name| {
            node_name_from_tablet_file(name)
                .map(|n| !allowed_helpers.contains(&n))
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if !created_content_files.is_empty() {
        errors.push(format!(
            "Easy mode only allows editing `{active_lean}` and adding Lean-closed helpers in the active support cone. Disallowed created: {:?}",
            created_content_files
        ));
    }
    let unexpected_modified: Vec<_> = changes
        .modified
        .iter()
        .filter(|name| **name != active_lean)
        .cloned()
        .collect();
    if !unexpected_modified.is_empty() {
        errors.push(format!(
            "Easy mode only allows editing `{active_lean}`. Modified: {:?}",
            unexpected_modified
        ));
    }
    match observe_node(repo_path, active_node) {
        Ok(observation) => {
            let evaluated = evaluate_node_observation(repo_path, &observation, None);
            errors.extend(evaluated.errors);
            if !evaluated.ok {
                if !evaluated.sorry_free {
                    errors.push(
                        "Easy mode requires the active node to be Lean-closed; if it or an imported dependency still uses `sorry`, use hard Local mode."
                            .to_string(),
                    );
                } else {
                    errors.push(
                        "Easy mode requires the active node to be a fully accepted Lean-closed Tablet node."
                            .to_string(),
                    );
                }
            }
        }
        Err(err) => errors.push(err),
    }
    WorkerValidationStepResult {
        kind: "proof_easy_scope".to_string(),
        ok: errors.is_empty(),
        detail: errors.first().cloned().unwrap_or_default(),
        errors,
        build_output: String::new(),
        allowed_nodes: BTreeSet::new(),
        local_closure_results: BTreeMap::new(),
    }
}

/// Revision-mode statement-edit scope validator (`revision_plan.md` §10,
/// step 11). Layers the frozen/editable/removed-target restrictions on top of
/// the ordinary theorem-stating scope/build steps that run alongside it.
///
/// Rejects, with node-naming errors:
///  - deletion of a frozen node;
///  - modification (`.lean`/`.tex` edit) of a frozen node;
///  - modification of an existing node outside `authorized_editable_nodes`
///    (= reviewer `authorized_nodes ∩ (present − revision_context.frozen_nodes)`,
///    the dynamic editable rule — nodes born during RevisionStating are
///    editable);
///  - creation of a new node when `allow_new_obligations` is false;
///  - a post-burst target claim naming a `removed` target.
///
/// New nodes (created files for a node absent from the before-snapshot) are
/// permitted through the existing theorem-stating conventions and are not
/// scope-checked here (unless new obligations are forbidden).
#[allow(clippy::too_many_arguments)]
pub(crate) fn revision_statement_edit_scope_step_result(
    repo_path: &Path,
    before_snapshot: &BTreeMap<String, String>,
    authorized_editable_nodes: &BTreeSet<NodeId>,
    frozen_nodes: &BTreeSet<NodeId>,
    frozen_node_reasons: &BTreeMap<NodeId, String>,
    removed_targets: &BTreeSet<TargetId>,
    allow_new_obligations: bool,
    current_target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    node_reference_ground_updates: &BTreeMap<NodeId, BTreeSet<RefPaperId>>,
    current_node_reference_grounds: &BTreeMap<NodeId, BTreeSet<RefPaperId>>,
) -> WorkerValidationStepResult {
    let kind = "revision_statement_edit_scope";
    let changes = detect_snapshot_changes(before_snapshot, &snapshot_tablet_dir(repo_path));
    let node_of = |name: &str| -> Option<NodeId> {
        if name.ends_with(".lean") || name.ends_with(".tex") {
            Path::new(name)
                .file_stem()
                .and_then(|value| value.to_str())
                .map(NodeId::from)
        } else {
            None
        }
    };
    let created_nodes: BTreeSet<NodeId> =
        changes.created.iter().filter_map(|n| node_of(n)).collect();
    let modified_nodes: BTreeSet<NodeId> =
        changes.modified.iter().filter_map(|n| node_of(n)).collect();
    let deleted_nodes: BTreeSet<NodeId> =
        changes.deleted.iter().filter_map(|n| node_of(n)).collect();

    // A node is "new" only if it did not exist before this burst at all
    // (no before-snapshot `.lean`/`.tex` for it). A created `.tex` paired with
    // an already-present `.lean` is an edit to an existing node, not a new node.
    let before_nodes: BTreeSet<NodeId> =
        before_snapshot.keys().filter_map(|n| node_of(n)).collect();
    let new_nodes: BTreeSet<NodeId> = created_nodes
        .iter()
        .filter(|node| !before_nodes.contains(*node))
        .cloned()
        .collect();
    // Existing nodes that were modified or whose files were created as an
    // addition to an already-present node.
    let touched_existing: BTreeSet<NodeId> = modified_nodes
        .iter()
        .chain(created_nodes.iter())
        .filter(|node| before_nodes.contains(*node))
        .cloned()
        .collect();

    let mut errors: Vec<String> = Vec::new();

    let frozen_reason = |node: &NodeId| -> String {
        frozen_node_reasons
            .get(node)
            .cloned()
            .unwrap_or_else(|| "frozen by revision plan".to_string())
    };

    // 1. Deleting a frozen node.
    for node in &deleted_nodes {
        if frozen_nodes.contains(node) {
            errors.push(format!(
                "Revision edit deletes frozen node `{node}`: {}. Frozen nodes are carried from the prior formalization and must not be deleted.",
                frozen_reason(node)
            ));
        }
    }
    // 2. Editing a frozen node.
    for node in modified_nodes.iter().chain(created_nodes.iter()) {
        if frozen_nodes.contains(node) {
            errors.push(format!(
                "Revision edit modifies frozen node `{node}`: {}. Frozen nodes are carried from the prior formalization and must not be edited.",
                frozen_reason(node)
            ));
        }
    }
    // 3. Existing-node edits must stay within authorized ∩ editable.
    let mut out_of_scope: Vec<NodeId> = touched_existing
        .iter()
        .chain(deleted_nodes.iter())
        .filter(|node| !frozen_nodes.contains(*node))
        .filter(|node| !authorized_editable_nodes.contains(*node))
        .cloned()
        .collect();
    out_of_scope.sort();
    out_of_scope.dedup();
    if !out_of_scope.is_empty() {
        let mut allowed_preview = authorized_editable_nodes
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if authorized_editable_nodes.len() > 12 {
            allowed_preview.push_str(", ...");
        }
        errors.push(format!(
            "Out-of-scope revision edits to existing nodes: {}. In RevisionStating a modified existing node must be in the reviewer-authorized editable set (authorized_nodes ∩ (present − frozen_nodes)): {}.",
            out_of_scope.join(", "),
            if allowed_preview.is_empty() { "(empty)" } else { allowed_preview.as_str() }
        ));
    }
    // 3b. Reference-paper claim DELTAS on frozen nodes (Amendment B1).
    // `node_reference_grounds` is full-set-replace, so a byte-identical
    // re-send of a frozen node's unchanged claim set passes; only a set
    // that actually differs from the current kernel state is a frozen
    // edit. This exclusion applies exactly while this step is in the
    // validation plan (RevisionStating); it LAPSES after the Advance
    // gate, when an ex-frozen node's claim legally reopens its
    // substantiveness through the ordinary claim path.
    for (node, claims) in node_reference_ground_updates {
        if !frozen_nodes.contains(node) {
            continue;
        }
        let current = current_node_reference_grounds
            .get(node)
            .cloned()
            .unwrap_or_default();
        if *claims != current {
            errors.push(format!(
                "Revision edit changes node_reference_grounds of frozen node `{node}`: {}. Frozen nodes are carried from the prior formalization; their reference-paper claims may not change during RevisionStating (an unchanged re-send of the same set is fine).",
                frozen_reason(node)
            ));
        }
    }
    // 4. New nodes only when new obligations are permitted.
    if !allow_new_obligations && !new_nodes.is_empty() {
        let mut listed = new_nodes.iter().cloned().collect::<Vec<_>>();
        listed.sort();
        errors.push(format!(
            "Revision edit creates new node(s) {} but this request forbids new obligations (allow_new_obligations=false).",
            listed.join(", ")
        ));
    }
    // 5. Target claims naming a removed target.
    if !removed_targets.is_empty() {
        for (node, claims) in current_target_claims {
            let bad: Vec<&TargetId> = claims
                .iter()
                .filter(|t| removed_targets.contains(*t))
                .collect();
            for target in bad {
                errors.push(format!(
                    "Node `{node}` claims target `{target}`, which the paper revision removed. Removed targets must not be claimed by any covering node.",
                ));
            }
        }
    }

    WorkerValidationStepResult {
        kind: kind.to_string(),
        ok: errors.is_empty(),
        detail: errors.first().cloned().unwrap_or_default(),
        errors,
        build_output: String::new(),
        allowed_nodes: authorized_editable_nodes.clone(),
        local_closure_results: BTreeMap::new(),
    }
}

pub(crate) fn theorem_target_edit_scope_step_result(
    repo_path: &Path,
    target: &str,
    before_snapshot: &BTreeMap<String, String>,
    initial_scope: &BTreeSet<NodeId>,
    declared_deleted_nodes: &BTreeSet<NodeId>,
) -> WorkerValidationStepResult {
    theorem_target_edit_scope_step_result_with_assumption_authoring(
        repo_path,
        target,
        before_snapshot,
        initial_scope,
        None,
        declared_deleted_nodes,
    )
}

pub(crate) fn theorem_target_edit_scope_step_result_with_assumption_authoring(
    repo_path: &Path,
    target: &str,
    before_snapshot: &BTreeMap<String, String>,
    initial_scope: &BTreeSet<NodeId>,
    assumption_authoring_node: Option<&NodeId>,
    declared_deleted_nodes: &BTreeSet<NodeId>,
) -> WorkerValidationStepResult {
    if target.is_empty() {
        return WorkerValidationStepResult {
            kind: "theorem_target_edit_scope".to_string(),
            ok: true,
            detail: String::new(),
            errors: Vec::new(),
            build_output: String::new(),
            allowed_nodes: BTreeSet::new(),
            local_closure_results: BTreeMap::new(),
        };
    }
    let final_names = current_tablet_node_names(repo_path);
    if !final_names.contains(target) {
        return WorkerValidationStepResult {
            kind: "theorem_target_edit_scope".to_string(),
            ok: false,
            detail: format!(
                "Current soundness target `{target}` must remain present in the tablet."
            ),
            errors: vec![format!(
                "Current soundness target `{target}` must remain present in the tablet."
            )],
            build_output: String::new(),
            allowed_nodes: BTreeSet::new(),
            local_closure_results: BTreeMap::new(),
        };
    }
    let changes = detect_snapshot_changes(before_snapshot, &snapshot_tablet_dir(repo_path));
    let changed_nodes: BTreeSet<NodeId> = changes
        .created
        .into_iter()
        .chain(changes.modified)
        .chain(changes.deleted)
        .filter_map(|name| {
            if name.ends_with(".lean") || name.ends_with(".tex") {
                Path::new(&name)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .map(NodeId::from)
            } else {
                None
            }
        })
        .filter(|node| !declared_deleted_nodes.contains(node))
        .collect();
    if changed_nodes.is_empty() {
        return WorkerValidationStepResult {
            kind: "theorem_target_edit_scope".to_string(),
            ok: true,
            detail: String::new(),
            errors: Vec::new(),
            build_output: String::new(),
            allowed_nodes: BTreeSet::new(),
            local_closure_results: BTreeMap::new(),
        };
    }
    let allowed: BTreeSet<NodeId> =
        if is_assumption_authoring_observation_node(target, assumption_authoring_node) {
            BTreeSet::from([NodeId::from(target)])
        } else {
            let after_scope: BTreeSet<NodeId> = compute_target_impact_region(repo_path, target)
                .into_iter()
                .map(NodeId::from)
                .collect();
            initial_scope.union(&after_scope).cloned().collect()
        };
    let out_of_scope: Vec<_> = changed_nodes
        .iter()
        .filter(|node| !allowed.contains(*node))
        .cloned()
        .collect();
    let mut errors = Vec::new();
    if !out_of_scope.is_empty() {
        let mut allowed_preview = allowed
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if allowed.len() > 12 {
            allowed_preview.push_str(", ...");
        }
        errors.push(format!(
            "Out-of-scope theorem-stating edits for target `{target}`: {}. When a current soundness target is set, changes must stay within that target's authorized impact region (target, prerequisites, and downstream consumers, before or after the cycle). Allowed scope: {}.",
            out_of_scope.join(", "),
            if allowed_preview.is_empty() { "(empty)" } else { allowed_preview.as_str() }
        ));
    }
    WorkerValidationStepResult {
        kind: "theorem_target_edit_scope".to_string(),
        ok: errors.is_empty(),
        detail: errors.first().cloned().unwrap_or_default(),
        errors,
        build_output: String::new(),
        allowed_nodes: allowed,
        local_closure_results: BTreeMap::new(),
    }
}

pub(crate) fn scoped_tablet_step_result(
    repo_path: &Path,
    baseline_errors: &[String],
    allowed_nodes: &BTreeSet<NodeId>,
    observe_all_present: bool,
) -> Result<WorkerValidationStepResult, String> {
    scoped_tablet_step_result_with_assumption_authoring(
        repo_path,
        baseline_errors,
        allowed_nodes,
        observe_all_present,
        &BTreeMap::new(),
        None,
        &BTreeSet::new(),
    )
}

pub(crate) fn scoped_tablet_step_result_with_assumption_authoring(
    repo_path: &Path,
    baseline_errors: &[String],
    allowed_nodes: &BTreeSet<NodeId>,
    observe_all_present: bool,
    before_snapshot: &BTreeMap<String, String>,
    assumption_authoring_node: Option<&NodeId>,
    declared_deleted_nodes: &BTreeSet<NodeId>,
) -> Result<WorkerValidationStepResult, String> {
    scoped_tablet_step_result_with_roles(
        repo_path,
        baseline_errors,
        allowed_nodes,
        observe_all_present,
        before_snapshot,
        &BTreeMap::new(),
        assumption_authoring_node,
        declared_deleted_nodes,
    )
}

fn under_model_assumptions_edit_errors(
    repo_path: &Path,
    before_snapshot: &BTreeMap<String, String>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
    assumption_authoring_node: Option<&NodeId>,
) -> Vec<String> {
    if before_snapshot.is_empty()
        || !matches!(
            node_role.get(ASSUMPTIONS_NAME),
            Some(trellis_kernel::PvRole::UnderModelAssumptions)
        )
        || is_assumption_authoring_observation_node(ASSUMPTIONS_NAME, assumption_authoring_node)
    {
        return Vec::new();
    }
    let changes = detect_snapshot_changes(before_snapshot, &snapshot_tablet_dir(repo_path));
    let edited_assumptions = changes
        .created
        .iter()
        .chain(changes.modified.iter())
        .chain(changes.deleted.iter())
        .any(|name| matches!(name.as_str(), "Assumptions.lean" | "Assumptions.tex"));
    if edited_assumptions {
        vec![
            "Under-model Assumptions node edits are only legal on a PV assumption-authoring worker request."
                .to_string(),
        ]
    } else {
        Vec::new()
    }
}

pub(crate) fn scoped_tablet_step_result_with_roles(
    repo_path: &Path,
    baseline_errors: &[String],
    allowed_nodes: &BTreeSet<NodeId>,
    observe_all_present: bool,
    before_snapshot: &BTreeMap<String, String>,
    node_role: &BTreeMap<NodeId, trellis_kernel::PvRole>,
    assumption_authoring_node: Option<&NodeId>,
    declared_deleted_nodes: &BTreeSet<NodeId>,
) -> Result<WorkerValidationStepResult, String> {
    let effective_allowed_nodes: BTreeSet<NodeId> = allowed_nodes
        .difference(declared_deleted_nodes)
        .cloned()
        .collect();
    let observation = if observe_all_present {
        observe_tablet_with_roles(repo_path, node_role)?
    } else {
        observe_tablet_nodes_with_roles(repo_path, &effective_allowed_nodes, node_role)?
    };
    let evaluated = evaluate_tablet_observation_with_roles(
        repo_path,
        &observation,
        node_role,
        assumption_authoring_node,
    );
    let mut new_relevant_errors =
        relevant_new_errors(&evaluated, baseline_errors, &effective_allowed_nodes);
    new_relevant_errors.extend(under_model_assumptions_edit_errors(
        repo_path,
        before_snapshot,
        node_role,
        assumption_authoring_node,
    ));
    Ok(WorkerValidationStepResult {
        kind: "scoped_tablet".to_string(),
        ok: new_relevant_errors.is_empty(),
        detail: new_relevant_errors.first().cloned().unwrap_or_default(),
        errors: new_relevant_errors,
        build_output: evaluated.build_output,
        allowed_nodes: effective_allowed_nodes,
        local_closure_results: BTreeMap::new(),
    })
}

pub(crate) fn cleanup_preserving_step_result(
    repo_path: &Path,
    before_snapshot: &BTreeMap<String, String>,
    before_tablet_contents: &BTreeMap<String, String>,
    _baseline_declaration_hashes: &BTreeMap<NodeId, String>,
    _baseline_correspondence_hashes: &BTreeMap<NodeId, String>,
    configured_targets: &BTreeSet<TargetId>,
    current_deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    current_target_claims: &BTreeMap<NodeId, BTreeSet<TargetId>>,
    current_present_nodes: &BTreeSet<NodeId>,
) -> Result<WorkerValidationStepResult, String> {
    fn tablet_imports(lean_content: &str) -> BTreeSet<NodeId> {
        lean_content
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                let suffix = trimmed.strip_prefix("import Tablet.")?;
                let name = suffix.trim();
                if name.is_empty() || name == AXIOMS_NAME {
                    None
                } else {
                    Some(NodeId::from(name))
                }
            })
            .collect()
    }

    fn content_without_current_orphan_imports(
        lean_content: &str,
        orphan_nodes: &BTreeSet<NodeId>,
    ) -> String {
        let mut retained = Vec::new();
        for line in lean_content.lines() {
            let trimmed = line.trim();
            let Some(dep) = trimmed.strip_prefix("import Tablet.") else {
                retained.push(line);
                continue;
            };
            if !orphan_nodes.contains(&NodeId::from(dep.trim())) {
                retained.push(line);
            }
        }
        retained.join("\n")
    }

    fn before_tablet_content<'a>(
        before_tablet_contents: &'a BTreeMap<String, String>,
        filename: &str,
    ) -> Option<&'a str> {
        if let Some(content) = before_tablet_contents.get(filename) {
            return Some(content.as_str());
        }
        before_tablet_contents
            .iter()
            .find(|(name, _)| normalize_snapshot_name(name) == filename)
            .map(|(_, content)| content.as_str())
    }

    fn dep_closure(
        seed: &BTreeSet<NodeId>,
        live_present: &BTreeSet<NodeId>,
        deps: &BTreeMap<NodeId, BTreeSet<NodeId>>,
    ) -> BTreeSet<NodeId> {
        let mut closure: BTreeSet<NodeId> = seed
            .iter()
            .filter(|node| live_present.contains(*node))
            .cloned()
            .collect();
        let mut frontier: Vec<NodeId> = closure.iter().cloned().collect();
        while let Some(node) = frontier.pop() {
            for dep in deps.get(&node).into_iter().flatten() {
                if !live_present.contains(dep) {
                    continue;
                }
                if closure.insert(dep.clone()) {
                    frontier.push(dep.clone());
                }
            }
        }
        closure
    }

    let orphan_nodes: BTreeSet<NodeId> = {
        let roots: BTreeSet<NodeId> = current_target_claims
            .iter()
            .filter(|(node, targets)| {
                current_present_nodes.contains(*node)
                    && targets
                        .iter()
                        .any(|target| configured_targets.contains(target))
            })
            .map(|(node, _)| node.clone())
            .collect();
        let supported = dep_closure(&roots, current_present_nodes, current_deps);
        current_present_nodes
            .iter()
            .filter(|node| node.as_str() != PREAMBLE_NAME && !supported.contains(*node))
            .cloned()
            .collect()
    };
    let changes = detect_snapshot_changes(before_snapshot, &snapshot_tablet_dir(repo_path));
    let mut errors = Vec::new();
    if !changes.created.is_empty() {
        errors.push(format!(
            "cleanup may not create Tablet files: {:?}",
            changes.created
        ));
    }
    let deleted_nodes: BTreeSet<NodeId> = changes
        .deleted
        .iter()
        .filter_map(|name| {
            Path::new(name)
                .file_stem()
                .and_then(|value| value.to_str())
                .map(NodeId::from)
        })
        .filter(|name| {
            name.as_str() != PREAMBLE_NAME
                && name.as_str() != AXIOMS_NAME
                && name.as_str() != HEADER_NAME
        })
        .collect();
    let illegal_deleted_nodes: Vec<_> = deleted_nodes.difference(&orphan_nodes).cloned().collect();
    if !illegal_deleted_nodes.is_empty() {
        errors.push(format!(
            "cleanup may only delete current orphan nodes: {:?}",
            illegal_deleted_nodes
        ));
    }
    let tex_modified: Vec<_> = changes
        .modified
        .iter()
        .filter(|name| name.ends_with(".tex"))
        .cloned()
        .collect();
    if !tex_modified.is_empty() {
        errors.push(format!(
            "cleanup may not modify .tex files: {:?}",
            tex_modified
        ));
    }

    let changed_nodes: BTreeSet<NodeId> = changes
        .modified
        .iter()
        .filter(|name| name.ends_with(".lean"))
        .filter_map(|name| {
            Path::new(name)
                .file_stem()
                .and_then(|value| value.to_str())
                .map(NodeId::from)
        })
        .filter(|name| name.as_str() != "Preamble" && name.as_str() != "Axioms")
        .collect();
    for node in &changed_nodes {
        if orphan_nodes.contains(node) {
            errors.push(format!(
                "{node}: cleanup may not edit orphan nodes in place; delete them or attach them from another node"
            ));
            continue;
        }
        let after_content =
            read_text_if_exists(&repo_path.join("Tablet").join(format!("{node}.lean")));
        if after_content.is_empty() {
            continue;
        }
        let before_tablet = current_deps.get(node).cloned().unwrap_or_default();
        let after_tablet = tablet_imports(&after_content);
        let illegal_import_delta: Vec<_> = before_tablet
            .symmetric_difference(&after_tablet)
            .filter(|dep| !orphan_nodes.contains(*dep))
            .cloned()
            .collect();
        if !illegal_import_delta.is_empty() {
            errors.push(format!(
                "{node}: cleanup may only add or remove Tablet imports that touch current orphan nodes: {:?}",
                illegal_import_delta
            ));
        }
    }

    let mut build_output = String::new();
    if before_tablet_contents.is_empty() {
        errors.push(
            "cleanup validation context is stale/missing before_tablet_contents; refusing whole-tablet fallback. Regenerate and retry the cleanup request with fresh context.".to_string(),
        );
    }

    if !before_tablet_contents.is_empty() {
        for node in &changed_nodes {
            if orphan_nodes.contains(node) {
                continue;
            }
            let filename = format!("{node}.lean");
            let Some(before_content) = before_tablet_content(before_tablet_contents, &filename)
            else {
                errors.push(format!(
                    "{node}: cleanup validation is missing the pre-worker Lean text snapshot"
                ));
                continue;
            };
            let after_content =
                read_text_if_exists(&repo_path.join("Tablet").join(format!("{node}.lean")));
            if content_without_current_orphan_imports(before_content, &orphan_nodes)
                != content_without_current_orphan_imports(&after_content, &orphan_nodes)
            {
                errors.push(format!(
                    "{node}: orphan cleanup may only add or remove Tablet import lines for current orphan nodes"
                ));
                continue;
            }

            match observe_node(repo_path, node) {
                Ok(observation) => {
                    let evaluated = evaluate_node_observation(repo_path, &observation, None);
                    if build_output.is_empty() {
                        build_output = evaluated.build_output.clone();
                    }
                    for err in evaluated.errors {
                        errors.push(format!("{node}: {err}"));
                    }
                }
                Err(err) => errors.push(format!("{node}: cleanup observation failed: {err}")),
            }
        }
    }

    Ok(WorkerValidationStepResult {
        kind: "cleanup_preserving".to_string(),
        ok: errors.is_empty(),
        detail: errors.first().cloned().unwrap_or_default(),
        errors,
        build_output,
        allowed_nodes: BTreeSet::new(),
        local_closure_results: BTreeMap::new(),
    })
}

/// Cleanup-v2 Step 9 (2026-05-14). Task-aware final-cleanup acceptance
/// validator. Three modes, keyed by `task_kind`:
///
/// 1. **None / legacy lint-only** (`task_kind = None`): same constraints
///    as the pre-v2 validator — no creates, no deletes, no `.tex`
///    modifications, decl-hash + corr-fingerprint invariant for every
///    changed `.lean` node. Used by the legacy cleanup flow until the
///    audit lane lights up.
///
/// 2. **LintFix** (`task_kind = LintFix { .. }`): same as legacy lint-only
///    but tightened to a single-node scope. `target_node` is the only
///    legal `.lean` edit; no `.tex`; no creates; no deletes; decl-hash
///    + corr-fingerprint invariant for the changed `.lean` node.
///
/// 3. **Substitution** (`task_kind = Substitution { .. }`):
///    - `target_node` deletion is the only legal Tablet-file deletion
///      (both `.lean` and `.tex`). Other deletes rejected.
///    - Changed `.lean` nodes must be a subset of
///      `authorized_nodes ∪ {target_node}`.
///    - `.tex` edits are allowed only for `authorized_nodes ∪ {target_node}`.
///    - Protected-statement nodes (`protected_statement_node_set`):
///      Lean signature (decl_hash) and `.tex` statement
///      (corr_reopen_guard fingerprint) are immutable. The narrowed
///      decl-hash invariant runs against
///      `protected_statement_node_set ∪ (authorized_nodes \ {target_node})`
///      — `target_node` is exempt because it is being deleted, and
///      `authorized_nodes \ {target_node}` are importers whose proof
///      bodies may be rewritten but whose signatures must not drift.
///    - Calls `paper_target_corr_reopen_guard_report_with_scope` to
///      catch fingerprint-changing `.tex` edits to protected nodes.
///
/// Legacy mode still evaluates the whole tablet. Task-scoped modes
/// evaluate only the acceptance scope selected by the task: LintFix checks
/// the target node, and Substitution checks authorized/protected nodes plus
/// edited nodes that remain present.
///
/// In every mode, errors are accumulated and returned as a single
/// `WorkerValidationStepResult` (no early returns — the reviewer
/// surfaces the full error list).
pub(crate) fn final_cleanup_preserving_step_result(
    repo_path: &Path,
    before_snapshot: &BTreeMap<String, String>,
    baseline_declaration_hashes: &BTreeMap<NodeId, String>,
    baseline_correspondence_hashes: &BTreeMap<NodeId, String>,
    current_present_nodes: &BTreeSet<NodeId>,
    task_kind: Option<&trellis_kernel::CleanupTaskKind>,
    target_node: Option<&NodeId>,
    authorized_nodes: &BTreeSet<NodeId>,
    protected_statement_node_set: &BTreeSet<NodeId>,
) -> Result<WorkerValidationStepResult, String> {
    let changes = detect_snapshot_changes(before_snapshot, &snapshot_tablet_dir(repo_path));
    let mut errors = Vec::new();

    // Mode dispatch. The three task modes share much of the same
    // observation pipeline (tablet build, decl-hash check, corr
    // fingerprint check) but differ in which file edits are legal.
    let is_substitution = matches!(
        task_kind,
        Some(trellis_kernel::CleanupTaskKind::Substitution { .. })
    );
    let is_lintfix = matches!(
        task_kind,
        Some(trellis_kernel::CleanupTaskKind::LintFix { .. })
    );
    // Legacy lint-only mode: task_kind = None. Same as today's
    // pre-v2 validator. Used when `cleanup_audit_enabled = false`
    // or when no audit-task has been dispatched yet.

    // ---------- Create / delete validation ----------
    if !changes.created.is_empty() {
        errors.push(format!(
            "final cleanup may not create Tablet files: {:?}",
            changes.created
        ));
    }

    // Deletes: legacy + LintFix forbid all; Substitution allows
    // exactly the target_node's .lean and .tex, AND requires both.
    if is_substitution {
        let target = target_node.cloned().unwrap_or_default();
        let allowed_deletes: BTreeSet<String> = if !target.as_str().is_empty() {
            BTreeSet::from([format!("{target}.lean"), format!("{target}.tex")])
        } else {
            BTreeSet::new()
        };
        let illegal_deletes: Vec<_> = changes
            .deleted
            .iter()
            .filter(|name| !allowed_deletes.contains(*name))
            .cloned()
            .collect();
        if !illegal_deletes.is_empty() {
            errors.push(format!(
                "cleanup substitution may only delete target_node's files (.lean + .tex); illegal deletes: {:?}",
                illegal_deletes
            ));
        }
        // Cleanup-v2 (audit Finding 7): substitution must actually
        // substitute — both `target_node.lean` AND `target_node.tex`
        // must appear in `deleted_files`. A "substitution" that leaves
        // the target node in place defeats the entire point of the
        // task.
        if !target.as_str().is_empty() {
            let lean_name = format!("{target}.lean");
            let tex_name = format!("{target}.tex");
            let mut missing: Vec<String> = Vec::new();
            if !changes.deleted.contains(&lean_name) {
                missing.push(lean_name);
            }
            if !changes.deleted.contains(&tex_name) {
                missing.push(tex_name);
            }
            if !missing.is_empty() {
                errors.push(format!(
                    "cleanup substitution must delete BOTH target_node's .lean and .tex files; missing deletions: {:?}",
                    missing
                ));
            }
        }
    } else if !changes.deleted.is_empty() {
        errors.push(format!(
            "final cleanup may not delete Tablet files: {:?}",
            changes.deleted
        ));
    }

    // ---------- .tex edit validation ----------
    let tex_modified: Vec<_> = changes
        .modified
        .iter()
        .filter(|name| name.ends_with(".tex"))
        .cloned()
        .collect();
    if is_substitution {
        // Substitution: .tex edits allowed for authorized_nodes ∪ {target_node}.
        let mut allowed_tex_nodes: BTreeSet<NodeId> = authorized_nodes.clone();
        if let Some(target) = target_node {
            allowed_tex_nodes.insert(target.clone());
        }
        let illegal_tex: Vec<_> = tex_modified
            .iter()
            .filter_map(|name| {
                Path::new(name)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(NodeId::from)
            })
            .filter(|node_id| {
                // Cleanup-v2 (audit Finding 8 fix): structural .tex
                // files (`header.tex`, `Preamble.tex`) are NEVER editable
                // in cleanup — they must appear in the illegal list.
                // Previously the filter was inverted: it excluded
                // structural files from being flagged. The corrected
                // predicate marks a node as illegal when EITHER it is a
                // structural file OR it is not in the allowed set.
                node_id.as_str() == PREAMBLE_NAME
                    || node_id.as_str() == "header"
                    || !allowed_tex_nodes.contains(node_id)
            })
            .collect();
        if !illegal_tex.is_empty() {
            errors.push(format!(
                "cleanup substitution may only modify .tex files for authorized_nodes \u{222a} \
                 {{target_node}} (and never structural files like header.tex / Preamble.tex); \
                 illegal .tex edits: {:?}",
                illegal_tex
            ));
        }
    } else if !tex_modified.is_empty() {
        errors.push(format!(
            "final cleanup may not modify .tex files: {:?}",
            tex_modified
        ));
    }

    // ---------- .lean edit scope ----------
    let changed_nodes: BTreeSet<NodeId> = changes
        .modified
        .iter()
        .filter(|name| name.ends_with(".lean"))
        .filter_map(|name| {
            Path::new(name)
                .file_stem()
                .and_then(|value| value.to_str())
                .map(NodeId::from)
        })
        .collect();
    let changed_content_nodes: BTreeSet<NodeId> = changes
        .modified
        .iter()
        .filter(|name| name.ends_with(".lean") || name.ends_with(".tex"))
        .filter_map(|name| {
            Path::new(name)
                .file_stem()
                .and_then(|value| value.to_str())
                .map(NodeId::from)
        })
        .collect();
    let changed_structural_lean_nodes: Vec<NodeId> = changed_nodes
        .iter()
        .filter(|node| is_structural_declaration_hash_node(node.as_str()))
        .cloned()
        .collect();

    if !changed_structural_lean_nodes.is_empty() {
        errors.push(format!(
            "final cleanup may not edit structural .lean files: {:?}",
            changed_structural_lean_nodes
        ));
    }

    if is_lintfix {
        // LintFix scope: `changed_nodes ⊆ authorized_nodes ∪ {target_node}`.
        //
        // Single-task dispatch supplies `target_node = Some(node)` and an
        // empty `authorized_nodes`, so the allowed set collapses to exactly
        // `{target_node}` — byte-for-byte the historical single-node scope.
        //
        // Cleanup-v2 batch dispatch supplies `target_node = None` and
        // `authorized_nodes` = the batch's target-node set, so the allowed
        // set is exactly the batched nodes. This is a scope generalization
        // only: the decl-hash + correspondence-fingerprint invariance below
        // still runs over EVERY changed node, so each batched node is fully
        // re-verified — batching changes dispatch granularity, not the
        // acceptance-time verification.
        let mut allowed_nodes: BTreeSet<NodeId> = authorized_nodes.clone();
        if let Some(target) = target_node {
            allowed_nodes.insert(target.clone());
        }
        let illegal: Vec<_> = changed_nodes
            .iter()
            .filter(|node| !allowed_nodes.contains(*node))
            .cloned()
            .collect();
        if !illegal.is_empty() {
            errors.push(format!(
                "cleanup lintfix may only edit its authorized target node(s) ({:?}); illegal .lean edits: {:?}",
                allowed_nodes, illegal
            ));
        }
    } else if is_substitution {
        // Substitution: changed nodes ⊆ authorized_nodes ∪ {target_node}.
        let mut allowed_nodes: BTreeSet<NodeId> = authorized_nodes.clone();
        if let Some(target) = target_node {
            allowed_nodes.insert(target.clone());
        }
        let illegal: Vec<_> = changed_nodes
            .iter()
            .filter(|node| !allowed_nodes.contains(*node))
            .cloned()
            .collect();
        if !illegal.is_empty() {
            errors.push(format!(
                "cleanup substitution may only edit authorized_nodes \u{222a} {{target_node}} \
                 . Structural .lean files are never editable in cleanup task modes. \
                 Illegal .lean edits: {:?}",
                illegal
            ));
        }
    } else {
        // Legacy: any present node may be edited.
        let illegal_changed_nodes: Vec<_> = changed_nodes
            .iter()
            .filter(|name| {
                name.as_str() != PREAMBLE_NAME
                    && name.as_str() != AXIOMS_NAME
                    && !current_present_nodes.contains(*name)
            })
            .cloned()
            .collect();
        if !illegal_changed_nodes.is_empty() {
            errors.push(format!(
                "final cleanup may only edit existing retained nodes: {:?}",
                illegal_changed_nodes
            ));
        }
    }

    // ---------- Build observation ----------
    let scoped_observation_nodes = if is_lintfix {
        let mut nodes = changed_content_nodes.clone();
        if let Some(target) = target_node {
            nodes.insert(target.clone());
        }
        Some(nodes)
    } else if is_substitution {
        let mut nodes: BTreeSet<NodeId> = authorized_nodes.clone();
        nodes.extend(protected_statement_node_set.iter().cloned());
        nodes.extend(changed_content_nodes.iter().cloned());
        if let Some(target) = target_node {
            let target_lean = format!("{target}.lean");
            let target_tex = format!("{target}.tex");
            let target_fully_deleted =
                changes.deleted.contains(&target_lean) && changes.deleted.contains(&target_tex);
            if !target_fully_deleted {
                nodes.insert(target.clone());
            } else {
                nodes.remove(target);
            }
        }
        Some(nodes)
    } else {
        None
    };
    let observation = if let Some(nodes) = scoped_observation_nodes.as_ref() {
        observe_tablet_nodes(repo_path, nodes)?
    } else {
        observe_tablet(repo_path)?
    };
    let evaluated = evaluate_tablet_observation(repo_path, &observation);
    if let Some(nodes) = scoped_observation_nodes.as_ref() {
        errors.extend(evaluation_errors_for_nodes(&evaluated, nodes));
    } else {
        errors.extend(evaluated.errors.clone());
    }

    // ---------- Decl-hash + corr fingerprint invariant ----------
    // Scope:
    //   Legacy / LintFix → every changed `.lean` node
    //   Substitution     → protected_statement_node_set
    //                      ∪ (authorized_nodes \ {target_node})
    //
    // For Substitution, `target_node` is exempt because it is being
    // deleted (its `.lean`/`.tex` won't exist after the burst).
    // Importers in `authorized_nodes \ {target_node}` must keep their
    // signatures invariant: proof bodies are editable but the Lean
    // signature and corr `.tex` statement are not.
    let mut invariant_nodes: BTreeSet<NodeId> = if is_substitution {
        let target = target_node.cloned();
        let mut nodes: BTreeSet<NodeId> = protected_statement_node_set.clone();
        for node in authorized_nodes {
            if Some(node) != target.as_ref() {
                nodes.insert(node.clone());
            }
        }
        // Intersect with the actually-changed nodes so we don't probe
        // nodes the worker didn't touch — same shape as legacy.
        nodes.intersection(&changed_nodes).cloned().collect()
    } else {
        changed_nodes.clone()
    };
    invariant_nodes.extend(changed_structural_lean_nodes.iter().cloned());

    let current_corr = observe_correspondence_fingerprints(repo_path, &invariant_nodes)?;
    for node in &invariant_nodes {
        let structural_lean_content;
        let observed_lean_content =
            if let Some(node_observation) = observation.nodes.get(node.as_str()) {
                Some(node_observation.lean_content.as_str())
            } else if is_structural_declaration_hash_node(node.as_str()) {
                structural_lean_content = read_text_if_exists(
                    &repo_path
                        .join("Tablet")
                        .join(format!("{}.lean", node.as_str())),
                );
                Some(structural_lean_content.as_str())
            } else {
                None
            };
        if let Some(lean_content) = observed_lean_content {
            let baseline_decl = baseline_declaration_hashes
                .get(node.as_str())
                .cloned()
                .unwrap_or_default();
            // Lazy-fetch: skip the strict (socket) path when nothing to
            // compare against. Mirrors `cleanup_preserving_step_result`.
            let current_decl = if baseline_decl.is_empty() {
                String::new()
            } else {
                match declaration_hash_for_gate(repo_path, lean_content, node) {
                    Ok(h) => h,
                    Err(e) => {
                        errors.push(format!(
                            "{node}: declaration hash extraction failed during final cleanup: {e}"
                        ));
                        String::new()
                    }
                }
            };
            if !baseline_decl.is_empty() && current_decl != baseline_decl {
                errors.push(format!(
                    "{node}: declaration hash changed during final cleanup"
                ));
            }
            let baseline_corr = baseline_correspondence_hashes
                .get(node.as_str())
                .cloned()
                .unwrap_or_default();
            let current_corr_fp = current_corr.get(node).cloned().unwrap_or_default();
            if !baseline_corr.is_empty() && current_corr_fp != baseline_corr {
                errors.push(format!(
                    "{node}: correspondence fingerprint changed during final cleanup"
                ));
            }
        }
    }

    // ---------- Substitution: protected-statement guard (extra safety) ----------
    // The Substitution branch additionally invokes the standard paper-
    // target reopen guard scoped to the protected-statement set. This
    // catches `\noderef`-sweep `.tex` edits that mechanically alter the
    // statement-level fingerprint of a protected node — the decl-hash
    // check covers Lean-side signature drift; the reopen guard covers
    // NL-side statement drift.
    if is_substitution && !protected_statement_node_set.is_empty() {
        // Pass the protected set as both `covering_nodes` and (empty)
        // `allowed_protected_semantic_change_nodes` so any reopen on
        // the protected set is fatal.
        match paper_target_corr_reopen_guard_report_with_scope(
            repo_path,
            protected_statement_node_set,
            baseline_correspondence_hashes,
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
        ) {
            Ok(report) => {
                for err in report.errors {
                    errors.push(format!(
                        "protected-statement corr reopen during cleanup substitution: {err}"
                    ));
                }
                if !report.reopened_nodes.is_empty() {
                    errors.push(format!(
                        "cleanup substitution may not modify the .tex statement of \
                         protected-statement nodes; reopened: {:?}",
                        report.reopened_nodes
                    ));
                }
            }
            Err(err) => {
                errors.push(format!(
                    "protected-statement reopen guard failed during cleanup substitution: {err}"
                ));
            }
        }
    }

    Ok(WorkerValidationStepResult {
        kind: "final_cleanup_preserving".to_string(),
        ok: errors.is_empty(),
        detail: errors.first().cloned().unwrap_or_default(),
        errors,
        build_output: evaluated.build_output,
        allowed_nodes: BTreeSet::new(),
        local_closure_results: BTreeMap::new(),
    })
}

pub(crate) fn proof_worker_delta_step_result(
    repo_path: &Path,
    active_node: &str,
    before_snapshot: &BTreeMap<String, String>,
    current_present_nodes: &BTreeSet<NodeId>,
    declared_deleted_nodes: &BTreeSet<NodeId>,
    current_node_kinds: &BTreeMap<NodeId, crate::NodeKind>,
    // Patch C-R: pre-delta `live.open_nodes` (the kernel's authoritative
    // sorryd set BEFORE the worker burst). Used to detect sorryd→sorry-
    // free transitions for non-active helpers so the local-closure probe
    // runs on every newly-sorry-free proof_node, not just the MCA-gated
    // active node. May be empty in legacy callers / tests that don't
    // need the new helper-probe behaviour; empty means "treat every
    // candidate's pre-delta state as unknown — only new births trigger".
    current_open_nodes: &BTreeSet<NodeId>,
    expected_active_hash: &str,
    proof_edit_mode: WorkerProofDeltaMode,
    approved_target_nodes: &BTreeSet<NodeId>,
    approved_corr_fingerprints: &BTreeMap<NodeId, String>,
    coarse_dag_nodes: &BTreeSet<NodeId>,
    authorized_nodes: &BTreeSet<NodeId>,
    allowed_protected_semantic_change_nodes: &BTreeSet<NodeId>,
    protected_semantic_change_nodes: &mut BTreeSet<NodeId>,
    allow_new_obligations: bool,
    must_close_active: bool,
) -> Result<WorkerValidationStepResult, String> {
    proof_worker_delta_step_result_with_under_model_assumptions(
        repo_path,
        active_node,
        before_snapshot,
        current_present_nodes,
        declared_deleted_nodes,
        current_node_kinds,
        current_open_nodes,
        expected_active_hash,
        proof_edit_mode,
        approved_target_nodes,
        approved_corr_fingerprints,
        coarse_dag_nodes,
        authorized_nodes,
        allowed_protected_semantic_change_nodes,
        protected_semantic_change_nodes,
        &BTreeSet::new(),
        None,
        allow_new_obligations,
        must_close_active,
    )
}

pub(crate) fn proof_worker_delta_step_result_with_under_model_assumptions(
    repo_path: &Path,
    active_node: &str,
    before_snapshot: &BTreeMap<String, String>,
    current_present_nodes: &BTreeSet<NodeId>,
    declared_deleted_nodes: &BTreeSet<NodeId>,
    current_node_kinds: &BTreeMap<NodeId, crate::NodeKind>,
    current_open_nodes: &BTreeSet<NodeId>,
    expected_active_hash: &str,
    proof_edit_mode: WorkerProofDeltaMode,
    approved_target_nodes: &BTreeSet<NodeId>,
    approved_corr_fingerprints: &BTreeMap<NodeId, String>,
    coarse_dag_nodes: &BTreeSet<NodeId>,
    authorized_nodes: &BTreeSet<NodeId>,
    allowed_protected_semantic_change_nodes: &BTreeSet<NodeId>,
    protected_semantic_change_nodes: &mut BTreeSet<NodeId>,
    under_model_assumption_nodes: &BTreeSet<NodeId>,
    assumption_authoring_node: Option<&NodeId>,
    allow_new_obligations: bool,
    must_close_active: bool,
) -> Result<WorkerValidationStepResult, String> {
    let changes = detect_snapshot_changes(before_snapshot, &snapshot_tablet_dir(repo_path));
    let mut build_output = String::new();
    // R4: resolve the tablet-wide target once for every filename-filter / probe
    // re-read site in this function. `Lean` on the all-Lean live run ⇒ every
    // routed site is byte-identical to the historical hardcoded `.lean`.
    let target = trellis_kernel::tablet_target_for_repo(repo_path);
    let mut delta_scope = proof_worker_delta_scope(
        &changes,
        active_node,
        current_present_nodes,
        proof_edit_mode,
        authorized_nodes,
        target,
    );
    delta_scope
        .deleted_existing_nodes
        .retain(|node| !declared_deleted_nodes.contains(node.as_str()));
    // Patch C-R-e: probes can legitimately reference new helpers added
    // in this same burst as boundary deps. The C-K fail-closed
    // `validate_probe_present_nodes` check uses the pre-burst snapshot
    // of `current_present_nodes` / `current_node_kinds`; that's correct
    // for catching stale-Lean-environment artifacts, but it would
    // wrongly reject a probe whose `boundary_theorems` lists a freshly-
    // added helper. The reviewer/worker prompts allow introducing
    // Lean-closed helpers (per `scope_local.md:5` /
    // `scope_restructure.md:5` / `20_helper_decomposition.md:5`); we
    // mirror that by admitting same-burst new helpers as valid dep
    // names + assigning them their post-delta kind (Proof if
    // proof-bearing, else Definition) inferred from the worker's .tex.
    let augmented_present_nodes =
        augment_present_nodes_with_burst_new(current_present_nodes, &delta_scope.new_lean_files);
    let augmented_node_kinds = augment_node_kinds_for_burst_new(
        current_node_kinds,
        &delta_scope.new_lean_files,
        repo_path,
    );
    if !delta_scope.stray_new_tex.is_empty() {
        return Ok(WorkerValidationStepResult {
            kind: "proof_worker_delta".to_string(),
            ok: false,
            detail: format!(
                "New .tex files without matching .lean files: {:?}",
                delta_scope.stray_new_tex
            ),
            errors: vec![format!(
                "Unpaired .tex files created: {:?}",
                delta_scope.stray_new_tex
            )],
            build_output: String::new(),
            allowed_nodes: authorized_nodes.clone(),
            local_closure_results: BTreeMap::new(),
        });
    }

    if !delta_scope.ghost_node_files.is_empty() {
        return Ok(WorkerValidationStepResult {
            kind: "proof_worker_delta".to_string(),
            ok: false,
            detail: format!(
                "Tablet/ contains files the kernel did not ratify: {:?}",
                delta_scope.ghost_node_files
            ),
            errors: vec![format!(
                "Found Tablet/ files modified that are not in the kernel's present_nodes set: {:?}. \
                 This indicates a prior burst's filesystem mutations were not rolled back through \
                 the runtime event path (see Bug X / task #52). Manual cleanup required: either \
                 commit a fresh worker burst that declares these via node_kind_updates, or delete \
                 the orphan files.",
                delta_scope.ghost_node_files
            )],
            build_output: String::new(),
            allowed_nodes: authorized_nodes.clone(),
            local_closure_results: BTreeMap::new(),
        });
    }

    let _ = coarse_dag_nodes; // Lean-relevance refactor: descendant filtering is
                              // now baked into the corr fingerprint shape itself
                              // (`lean_relevant_definition_descendants`); the
                              // explicit `coarse_dag_descendant_filter` parameter
                              // is no longer needed at the guard layer.
    let corr_reopen_report = paper_target_corr_reopen_guard_report_with_scope(
        repo_path,
        approved_target_nodes,
        approved_corr_fingerprints,
        proof_edit_mode,
        allowed_protected_semantic_change_nodes,
    )?;
    let corr_reopened_nodes = corr_reopen_report.reopened_nodes;
    let corr_reopen_errors = corr_reopen_report.errors;
    if !corr_reopen_errors.is_empty() {
        return Ok(WorkerValidationStepResult {
            kind: "proof_worker_delta".to_string(),
            ok: false,
            detail: corr_reopen_errors[0]
                .lines()
                .next()
                .unwrap_or("paper-target correspondence reopened")
                .to_string(),
            errors: corr_reopen_errors,
            build_output: String::new(),
            allowed_nodes: authorized_nodes.clone(),
            local_closure_results: BTreeMap::new(),
        });
    }

    if !delta_scope.deleted_existing_nodes.is_empty() {
        return Ok(WorkerValidationStepResult {
            kind: "proof_worker_delta".to_string(),
            ok: false,
            detail: format!(
                "Existing proof-formalization nodes may not be deleted on this path: {:?}",
                delta_scope.deleted_existing_nodes
            ),
            errors: vec![format!(
                "Deleted existing node files: {:?}",
                delta_scope.deleted_existing_nodes
            )],
            build_output: String::new(),
            allowed_nodes: authorized_nodes.clone(),
            local_closure_results: BTreeMap::new(),
        });
    }

    if !delta_scope
        .unauthorized_extra_changed_existing_nodes
        .is_empty()
    {
        return Ok(WorkerValidationStepResult {
            kind: "proof_worker_delta".to_string(),
            ok: false,
            detail: format!(
                "Out-of-scope existing nodes were modified: {:?}",
                delta_scope.unauthorized_extra_changed_existing_nodes
            ),
            errors: vec![format!(
                "Out-of-scope existing node changes: {:?}",
                delta_scope.unauthorized_extra_changed_existing_nodes
            )],
            build_output: String::new(),
            allowed_nodes: authorized_nodes.clone(),
            local_closure_results: BTreeMap::new(),
        });
    }

    if delta_scope.unauthorized_active_change {
        return Ok(WorkerValidationStepResult {
            kind: "proof_worker_delta".to_string(),
            ok: false,
            detail: format!(
                "Active node `{active_node}` was changed but is not listed in authorized_node_ids; next_active is a scope anchor, not edit permission.",
            ),
            errors: vec![format!(
                "Unauthorized active-node change: `{active_node}` is not in authorized_nodes={authorized_nodes:?}",
            )],
            build_output: String::new(),
            allowed_nodes: authorized_nodes.clone(),
            local_closure_results: BTreeMap::new(),
        });
    }

    if !must_close_active
        && !delta_scope.active_changed
        && delta_scope.new_lean_files.is_empty()
        && delta_scope
            .authorized_extra_changed_existing_nodes
            .is_empty()
    {
        protected_semantic_change_nodes.extend(corr_reopened_nodes);
        return Ok(WorkerValidationStepResult {
            kind: "proof_worker_delta".to_string(),
            ok: true,
            detail: "No files were changed.".to_string(),
            errors: Vec::new(),
            build_output: String::new(),
            allowed_nodes: authorized_nodes.clone(),
            local_closure_results: BTreeMap::new(),
        });
    }

    for name in &delta_scope.new_lean_files {
        if name == PREAMBLE_NAME
            || name == AXIOMS_NAME
            || is_under_model_assumption_observation_node(name, under_model_assumption_nodes)
        {
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: format!("Proof work may not create special node `{name}`"),
                errors: vec![format!("Proof work may not create special node `{name}`")],
                build_output: String::new(),
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        if current_present_nodes.contains(name.as_str()) {
            continue;
        }
        let valid_name = name
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
            && name
                .chars()
                .all(|ch| ch == '_' || ch == '\'' || ch.is_ascii_alphanumeric());
        if !valid_name {
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: format!("Invalid node name: {:?}", name),
                errors: vec![format!("Invalid node name: {:?}", name)],
                build_output: String::new(),
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        let observation = observe_node(repo_path, name)?;
        if build_output.is_empty() {
            build_output = external_output(&observation.compile);
        }
        if !observation.tex_exists {
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: format!("New node {name} has .lean but no .tex file"),
                errors: vec![format!("New node {name} has .lean but no .tex file")],
                build_output: String::new(),
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        let evaluated = evaluate_node_observation(repo_path, &observation, None);
        if build_output.is_empty() {
            build_output = evaluated.build_output.clone();
        }
        let mut node_errors = evaluated.errors.clone();
        if !allow_new_obligations && !evaluated.ok {
            if !evaluated.sorry_free {
                node_errors.push(
                    "This reviewer scope does not allow new open obligations: every new helper node must be Lean-closed with no `sorry`."
                        .to_string(),
                );
            } else if node_errors.is_empty() {
                node_errors.push(
                    "This reviewer scope does not allow new open obligations: every new helper node must be a fully accepted Lean-closed Tablet node."
                        .to_string(),
                );
            }
        }
        if !node_errors.is_empty() {
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: format!("New node {name}: {}", node_errors[0]),
                errors: node_errors
                    .into_iter()
                    .map(|err| format!("New node {name}: {err}"))
                    .collect(),
                build_output: evaluated.build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
    }

    for name in &delta_scope.authorized_extra_changed_existing_nodes {
        let observation = observe_node(repo_path, name)?;
        let evaluated = if is_assumption_authoring_observation_node(name, assumption_authoring_node)
        {
            evaluate_assumptions_node_observation(&observation)
        } else {
            evaluate_node_observation(repo_path, &observation, None)
        };
        if build_output.is_empty() {
            build_output = evaluated.build_output.clone();
        }
        if !evaluated.errors.is_empty() {
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: format!("{name}: {}", evaluated.errors[0]),
                errors: evaluated
                    .errors
                    .into_iter()
                    .map(|err| format!("{name}: {err}"))
                    .collect(),
                build_output: evaluated.build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
    }

    if delta_scope.active_changed || must_close_active {
        let observation = observe_node(repo_path, active_node)?;
        if is_assumption_authoring_observation_node(active_node, assumption_authoring_node) {
            let evaluated = evaluate_assumptions_node_observation(&observation);
            if build_output.is_empty() {
                build_output = evaluated.build_output.clone();
            }
            if !evaluated.errors.is_empty() {
                return Ok(WorkerValidationStepResult {
                    kind: "proof_worker_delta".to_string(),
                    ok: false,
                    detail: evaluated.errors[0].clone(),
                    errors: evaluated.errors,
                    build_output: evaluated.build_output,
                    allowed_nodes: authorized_nodes.clone(),
                    local_closure_results: BTreeMap::new(),
                });
            }
            protected_semantic_change_nodes.extend(corr_reopened_nodes);
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: true,
                detail: "Assumptions node staged edit accepted.".to_string(),
                errors: Vec::new(),
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        // Lock the active node's signature (declaration head hash) unless the
        // worker is in a mode that's allowed to change it.
        //   - `coarse_restructure`: always permitted (escape hatch for expert-
        //     approved package moves, including coarse-DAG node signatures).
        //   - `restructure`: permitted only when the active node is NOT part
        //     of the coarse DAG captured at the theorem-stating → proof-
        //     formalization transition. Later-added proof-phase helpers can
        //     have their signatures revised in this mode; coarse-DAG nodes
        //     still require coarse_restructure.
        //   - Legacy runs (empty `coarse_dag_nodes`): treat every node as
        //     coarse to preserve prior behavior until the field is populated
        //     at the next phase advance.
        let active_is_coarse = if coarse_dag_nodes.is_empty() {
            true
        } else {
            coarse_dag_nodes.contains(active_node)
        };
        let signature_editable = proof_edit_mode == WorkerProofDeltaMode::CoarseRestructure
            || (proof_edit_mode == WorkerProofDeltaMode::Restructure && !active_is_coarse);
        let expected = if signature_editable {
            None
        } else {
            Some(expected_active_hash)
        };
        let evaluated = evaluate_node_observation(repo_path, &observation, expected);
        if build_output.is_empty() {
            build_output = evaluated.build_output.clone();
        }
        if !evaluated.errors.is_empty() {
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: evaluated.errors[0].clone(),
                errors: evaluated.errors,
                build_output: evaluated.build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        if must_close_active && !evaluated.shallow_ok {
            // Shallow-closure semantics, matching the kernel's authoritative
            // `committed.open_nodes` tracking
            // (`worker_normalization::open_nodes_from_repo` — pure textual
            // `has_sorry` check on the node's own .lean file). The active
            // node is "closed for the worker's task" when its own file
            // compiles cleanly and contains no `sorry` literal, regardless
            // of whether imported helpers carry `sorry` (those helpers are
            // governed by `allow_new_obligations` and remain in
            // `committed.open_nodes` until they themselves are closed).
            //
            // The transitive checks (`sorry_warnings.is_empty()` and
            // `axioms_valid` from `#print axioms` showing sorryAx) are
            // intentionally excluded — they fail when ANY transitive dep
            // carries sorry, which is the wrong semantic for "did the
            // worker complete THIS node's task" — a worker that closed
            // its own node should not be flagged because a transitive
            // dependency still carries `sorry`.
            let detail = if evaluated.sorry_in_source {
                "This reviewer scope requires the active node to be Lean-closed with no `sorry` in its own file."
                    .to_string()
            } else {
                "This reviewer scope requires the active node to be a fully accepted Lean-closed Tablet node."
                    .to_string()
            };
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output: evaluated.build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
    }

    // Kind-aware must_close_active enforcement (operator-settled design,
    // cycle-699 `EndpointSidePrefixAttachment` incident): the strict-
    // closure probe below is a THEOREM-root machine — the Lean script
    // rejects structure/inductive/class roots with "unsupported root
    // kind" — so it only defines "finished" for Proof-kind actives. For
    // a Definition-kind active node, "finished" is: the delta is
    // complete and accepted AND the node compiles with no `sorry` in
    // its own file — exactly the deterministic delta checks plus the
    // `[shallow]` no-sorry gate above (`evaluate_node_observation`'s
    // `shallow_ok`, enforced at the `must_close_active &&
    // !evaluated.shallow_ok` arm). We therefore skip ONLY the strict-
    // closure root probe (and its `[internal]`/`[axiom]`/`[strict]`
    // rejection arms) for Definition-kind actives; every other check is
    // unchanged. No probe payload also means no `LocalClosureRecord`
    // for the definition active — records are scoped to proof-bearing
    // nodes (engine `classify_record_eligibility` would drop it anyway).
    //
    // Kind resolution is the kernel's committed protocol kind
    // (`current_node_kinds`, the same pre-burst snapshot the reviewer
    // saw when setting must_close_active). Fail closed: a missing entry
    // (legacy callers / tests that pass an empty map) or any non-
    // Definition kind keeps the full Proof-kind strict behavior.
    let active_is_definition_kind = matches!(
        current_node_kinds.get(active_node),
        Some(crate::NodeKind::Definition)
    );

    // Patch B (plan §6.1): local-closure probe gate. Runs only when
    // `must_close_active=true` (i.e., reviewer pinned the worker to
    // close THIS proof and the gate is rejecting otherwise) AND the
    // active node is Proof-kind (see kind-aware note above). The probe
    // walks the elaborated environment from `active_node` outward,
    // emitting the kernel-axiom set, boundary theorems (open helpers
    // the proof acknowledges), and strict-context dependencies. The
    // gate rejects in five categories:
    //   - [internal] probe transport / script error (timeout, non-zero
    //     returncode, status != "ok")
    //   - [internal] axiomization cross-check disagrees with primary
    //                collector (plan §4.6.1 dual-collector runtime
    //                invariant; populated by the merged Lean script's
    //                secondary `axCheckRoot` pass)
    //   - [axiom]    kernel_axioms ⊄ approved_kernel_axioms_for(active)
    //   - [strict]   probe.errors non-empty (strict-context dep carries
    //                an unapproved axiom or other violation surfaced
    //                inside the script)
    // The shallow [shallow] rejection is the existing check above.
    //
    // On accept, the probe payload is stashed in
    // `local_closure_results` for downstream consumption (Patch C will
    // persist a `LocalClosureRecord`).
    let mca_local_closure_result: Option<LocalClosureProbeOutput> = if must_close_active
        && !active_is_definition_kind
    {
        let mut local = run_local_closure_axioms(repo_path, active_node).map_err(|err| {
            // Transport / IPC failures bubble up to the caller; the
            // caller (`execute_worker_validation_plan_with_progress`)
            // converts the Err into a fail-closed step result. Patch C
            // will introduce a synthesized "transport_error" failure
            // summary so the engine can record it; for Patch B we
            // preserve the existing Err propagation contract.
            err
        })?;
        // Patch C-K Fix 1 (audit MEDIUM-HIGH): validate dep names against
        // current present_nodes BEFORE the probe payload is consumed
        // downstream (engine's record install path / Rust normalization).
        // An unmappable Lean constant must NOT become a record dep key,
        // since such a key is not tied to kernel node lifecycle. Fail
        // closed by flipping status to `internal_error` — the existing
        // `local.status != "ok"` arm below handles the rejection.
        //
        // Patch C-N item 1: ALSO validate dep kinds (boundary_theorems
        // must be Proof-kind, strict_definition_deps must be Definition-
        // kind, etc.) so a kind-confused dep can't sneak through into a
        // persisted record either.
        //
        // Patch C-R-e: see top-of-function comment. Same-burst new
        // helpers are admitted as valid boundary deps.
        validate_probe_present_nodes(&mut local, &augmented_present_nodes, &augmented_node_kinds);
        if local.timed_out {
            let detail = "[internal] local-closure probe timed out".to_string();
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        if local.returncode != 0 {
            // Trim stderr to keep operator-facing messages compact.
            // The full envelope is preserved in the LocalClosureProbeOutput
            // we discard here; Patch C will retain it on rejection.
            let stderr_excerpt: String = local
                .raw_stderr
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(3)
                .collect::<Vec<_>>()
                .join(" | ");
            let detail = format!(
                "[internal] local-closure probe exited with returncode={}: {}",
                local.returncode, stderr_excerpt
            );
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        // Patch C-Q Q9: the duplicate axcheck-disagreement arm that
        // used to live here was redundant. `parse_local_closure_response`
        // already flips `local.status` to `internal_error` and pushes a
        // primary_only_axioms / axcheck_only_axioms / primary_only_boundaries
        // / axcheck_only_boundaries diagnostic into `local.errors[0]`
        // when the cross-check disagrees. The fallthrough
        // `local.status != "ok"` arm below surfaces that same diagnostic
        // (status + errors[0]) to the operator, so the dedicated branch
        // here added no information the structured payload doesn't
        // already carry. Removing the duplication keeps the operator-
        // facing format in lockstep with whatever
        // `parse_local_closure_response` emits.
        if local.status != "ok" {
            let first_error = local.errors.first().cloned().unwrap_or_default();
            let detail = format!(
                "[internal] local-closure probe status={}: {}",
                local.status, first_error
            );
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        // B2c-gate Slice 2 — backend split. The build-RC0 / timeout /
        // status arms above are backend-agnostic and already applied. The
        // axiom-CEILING vs the ORACLE/SHYPS/THEOREM cert gate diverge by
        // target:
        //   * Lean: the mathlib axiom-closure model — `kernel_axioms ⊄
        //     approved` rejects `[axiom]` (the `sorryAx`-in-closure catcher).
        //   * IsabelleHol: DROP the axiom ceiling (definitional axioms are
        //     legitimate; the `Main` inventory is ~3451). The cheat signal is
        //     the server-injected ORACLE (the always-present `sorry`/`smt`
        //     catcher — the build does NOT catch a `sorry` over `use_theories`),
        //     plus the SHYPS (inconsistent-class) and THEOREM (oops/elision)
        //     channels. The node-level `axiomatization` ban (S6 shape gate)
        //     covers worker-authored axioms.
        if trellis_kernel::tablet_target_for_repo(repo_path)
            == trellis_kernel::backend::BackendId::IsabelleHol
        {
            if let Some(detail) = isabelle_cert_gate_violation(&local, "active") {
                return Ok(WorkerValidationStepResult {
                    kind: "proof_worker_delta".to_string(),
                    ok: false,
                    detail: detail.clone(),
                    errors: vec![detail],
                    build_output,
                    allowed_nodes: authorized_nodes.clone(),
                    local_closure_results: BTreeMap::new(),
                });
            }
        } else {
            // Lean approved-set assembly (plan §8): canonical four are
            // seeded by `load_approved_axioms` itself (DEFAULT_APPROVED_AXIOMS
            // at the top of this file); we just call the per-node loader and
            // use its return as the approved set. No need to re-union the
            // canonical four here.
            let approved = load_approved_axioms(repo_path, active_node)?;
            let violations: Vec<String> = local
                .kernel_axioms
                .iter()
                .filter(|a| !approved.contains(a.as_str()))
                .cloned()
                .collect();
            if !violations.is_empty() {
                let detail = format!(
                    "[axiom] active proof uses unapproved kernel axiom(s): {:?}",
                    violations
                );
                return Ok(WorkerValidationStepResult {
                    kind: "proof_worker_delta".to_string(),
                    ok: false,
                    detail: detail.clone(),
                    errors: vec![detail],
                    build_output,
                    allowed_nodes: authorized_nodes.clone(),
                    local_closure_results: BTreeMap::new(),
                });
            }
        }
        if !local.errors.is_empty() {
            let first = local.errors.first().cloned().unwrap_or_default();
            let detail = format!(
                "[strict] active proof's strict-context dependency carries an unapproved axiom or other violation: {}",
                first
            );
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        // Probe accepted; stash for downstream consumption.
        Some(local)
    } else {
        None
    };

    if matches!(
        proof_edit_mode,
        WorkerProofDeltaMode::Easy | WorkerProofDeltaMode::Local
    ) {
        // Easy and Local authorize only the active node's proof body (after
        // `:=`) — no .tex edits, no signature changes (signature handled
        // above via expected_active_hash). The deterministic-gate must
        // reject active.tex modifications to match the prompt contract
        // (prompt_fragments/review/common/37_restructure_strategy.md +
        // prompt_fragments/worker/proof_formalization/05_scope_local.md).
        // Without this check the worker silently expands its scope and
        // forces a corr-blocker re-open it can't resolve in these modes.
        let active_tex = format!("{active_node}.tex");
        if changes.modified.contains(&active_tex) {
            let mode_label = if proof_edit_mode == WorkerProofDeltaMode::Easy {
                "Easy"
            } else {
                "Local"
            };
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: format!(
                    "{mode_label} mode does not authorize editing the active node's .tex file: {active_tex}"
                ),
                errors: vec![format!(
                    "{mode_label} mode authorizes only proof-body edits to `{active_node}.lean` plus permitted new helpers. Modified .tex: {active_tex}"
                )],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        let missing_new_support = active_support_cone_missing_new_nodes(
            repo_path,
            active_node,
            &delta_scope.new_lean_files,
        );
        if !missing_new_support.is_empty() {
            let mode_label = if proof_edit_mode == WorkerProofDeltaMode::Easy {
                "Easy"
            } else {
                "proof-local"
            };
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: format!(
                    "New {mode_label} helper nodes must end inside the active node's support cone: {:?}",
                    missing_new_support
                ),
                errors: vec![format!(
                    "New {mode_label} helper nodes are not imported by the active node's final support cone: {:?}",
                    missing_new_support
                )],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
    }

    protected_semantic_change_nodes.extend(corr_reopened_nodes);
    // Patch B accept path: when must_close_active=true and the local-
    // closure probe accepted, attach the probe payload keyed by the
    // active node so the bridge can forward it onto
    // WorkerResponse.local_closure_results. Patch C will use this to
    // write a durable LocalClosureRecord. Empty when must_close_active
    // is false (the gate didn't fire) or when the probe never ran.
    let mut local_closure_results: BTreeMap<NodeId, LocalClosureProbeOutput> = BTreeMap::new();
    let active_node_id = NodeId::from(active_node);
    if let Some(probe) = mca_local_closure_result {
        local_closure_results.insert(active_node_id.clone(), probe);
    }

    // Patch C-R (plan §7.0): run the local-closure probe on every helper
    // that becomes sorry-free in this delta — not just the MCA-gated
    // active node. The §527 invariant ("sorry-free + no fresh record ⇒
    // in unverified set") is only enforced if the engine actually
    // receives a probe result (or a synthesized failure) for every
    // sorryd→sorry-free transition; before this patch, new helper
    // births and non-active sorry-free transitions silently bypassed
    // the local-closure gate. Cf. worker 2707's `FixedEmbeddingGraph
    // EventProbability` add (cycle TBD): sorry-free helper joined
    // `proof_nodes` with no probe, no record, no unverified entry.
    //
    // Candidate set construction (Patch C-R-c: broadened to enforce
    // "any node touched in this burst that ends up sorry-free must
    // pass the local check before outcome=valid"):
    //
    //   (a) `delta_scope.new_lean_files` — every new proof_node birth.
    //   (b) Every `.lean` file in `changes.modified` whose node ends up
    //       sorry-free post-delta. This covers two sub-cases:
    //       - sorryd → sorry-free transition (pre-delta in
    //         `current_open_nodes`, post-delta no `sorry` token);
    //       - already-sorry-free node whose proof body was edited
    //         (pre-delta NOT in `current_open_nodes`, post-delta still
    //         no `sorry` token, but the proof body changed and could
    //         have introduced an unapproved-axiom or sorryAx-via-dep
    //         regression).
    //   Pre-delta sorry status is no longer used to derive (b) — the
    //   post-candidate "still sorry-free" filter (below) is what gates
    //   record-eligibility. Restricting (b) to pre-delta sorryd nodes
    //   created a class-(c) loophole: a worker could modify the proof
    //   of an already-sorry-free node, the modified proof would silently
    //   bypass the local check at gate time, and the engine's content-
    //   change invalidation (Patch C-Q) would only catch it on the next
    //   revalidation pass. The invariant requires check.py to enforce
    //   the local check for every touched sorry-free node, not defer.
    //
    // Definitions are excluded (closure records only exist for proof-
    // bearing nodes; engine `classify_record_eligibility` would drop
    // their probe results anyway, so save the IPC). Active node is
    // skipped when the MCA gate already populated a probe result for
    // it — no double-probing, no overwrite of the MCA payload.
    //
    // Probes are I/O-heavy and run serially (one IPC per node); even
    // for restructure bursts adding multiple helpers, serial is fast
    // enough — DO NOT introduce parallelism.
    //
    // Failure path: any probe with `status != "ok"`, non-empty
    // `errors`, OR kernel_axioms ⊄ approved_axioms_for(N) rejects the
    // entire burst. The engine's step (e) loop tolerates failed probes
    // (synthesizes a failure summary, marks unverified), but at this
    // gate we want the worker to see the failure and retry, matching
    // the MCA gate's stricter contract.
    // R4 B4: node-source suffix is the target's (`.lean` ⇒ byte-identical on the
    // live run; `.thy` for IsabelleHol).
    let source_suffix = format!(
        ".{}",
        trellis_kernel::backend::descriptor_for(target).node_source_ext
    );
    let modified_lean_node_files: BTreeSet<String> = changes
        .modified
        .iter()
        .filter(|name| name.ends_with(&source_suffix))
        .filter_map(|name| node_name_from_tablet_file(name))
        .collect();

    // FIX 2 coverage (universal owner-file authoring gate): run the
    // `where`-aware authored-name scan on EVERY new/modified `.lean` node
    // file, regardless of node kind, BEFORE the proof-only closure-probe
    // loop below. This is the residual fix: the closure probe (and its
    // embedded owner-file scan) runs only on proof-bearing
    // `probe_candidates`, so a pure Definition-kind node could author a
    // `where`/`let rec`-bound reserved-shaped auxiliary (`def Foo … where
    // eq_1 := …` ⇒ `Foo.eq_1`) that the probe never scans, the Rust text
    // backstop (`reserved_shaped_authored_components`) cannot see (no
    // `where`-binder parse), and a consumer then transparent-walks/hides.
    //
    // The scan is decoupled from the closure-record machinery: scan-only
    // emits no dep/axiom keys, so it never enters
    // `validate_probe_present_nodes` / `classify_record_eligibility`. It is
    // a distinct HARD authoring invariant — a reserved-shaped authored
    // declaration in ANY node file rejects the burst. The proof-only
    // probe's embedded scan and the Rust text backstop stay in place as
    // defense-in-depth (the redundant scan on proof nodes is cheap —
    // scan-only is parse-only, no olean import, no closure walk).
    //
    // Cost: one cheap (parse-only) IPC per changed/new node file. Probes
    // run serially by design — DO NOT introduce parallelism.
    let mut owner_scan_nodes: BTreeSet<NodeId> = BTreeSet::new();
    for name in &delta_scope.new_lean_files {
        owner_scan_nodes.insert(NodeId::from(name.as_str()));
    }
    for name in &modified_lean_node_files {
        owner_scan_nodes.insert(NodeId::from(name.as_str()));
    }
    for candidate in &owner_scan_nodes {
        let node_name = candidate.as_str();
        // The node source may have vanished after the snapshot was taken (the
        // node is no longer a present-node candidate); skip — the
        // deterministic-revalidation pass catches any residual drift. R4 A4:
        // the source path is the target's (`.lean` ⇒ byte-identical on the live
        // run; `.thy` for IsabelleHol). The owner-scan op itself is already
        // backend-routed in `run_local_closure_owner_scan`.
        let lean_path = trellis_kernel::node_source_path(repo_path, node_name, target);
        if !lean_path.exists() {
            continue;
        }
        let scan = run_local_closure_owner_scan(repo_path, node_name)?;
        if scan.status != "ok" {
            // The Lean scan-only envelope carries the reserved-shape (or
            // unreadable-source, fail-closed) diagnostic in `errors[0]`.
            let first_error = scan.errors.first().cloned().unwrap_or_else(|| {
                format!(
                    "owner-file authoring scan failed for `{node_name}` (status={})",
                    scan.status
                )
            });
            let detail = format!(
                "[authoring] node `{node_name}` failed the owner-file authored-name scan: {first_error}"
            );
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
    }

    let mut probe_candidates: BTreeSet<NodeId> = BTreeSet::new();
    for name in &delta_scope.new_lean_files {
        probe_candidates.insert(NodeId::from(name.as_str()));
    }
    for name in &modified_lean_node_files {
        probe_candidates.insert(NodeId::from(name.as_str()));
    }
    // Patch C-R-c: `current_open_nodes` is no longer required to derive
    // the candidate set — the broader trigger covers both class-(b)
    // transitions and class-(c) already-sorry-free edits. The parameter
    // is retained on `proof_worker_delta_step_result` for plumbing
    // stability and as a pre-burst snapshot that future patches may
    // need (e.g. for distinguishing transition errors from edit-
    // regressions in error messages).
    let _ = current_open_nodes;
    // Don't double-probe the active node when the MCA gate already
    // attached a result above. The MCA payload uses the same approved-
    // axioms gate this loop would, so coalescing on NodeId is safe.
    if local_closure_results.contains_key(&active_node_id) {
        probe_candidates.remove(&active_node_id);
    }

    // Patch C-R-d: how much of `raw_stderr` we surface in rejection
    // messages. Workers iterate via `lake build` (which gives full
    // output) but only see the gate's rejection text when the probe
    // disagrees with their iteration. Surface a generous excerpt so
    // workers can act on the rejection without re-running `lake build`
    // a second time; cap by lines and bytes to keep prompt size
    // bounded if Lean produced an enormous error stream.
    const STDERR_MAX_LINES: usize = 200;
    const STDERR_MAX_BYTES: usize = 30_000;
    let format_stderr_excerpt = |raw: &str| -> String {
        let non_empty: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
        let total_lines = non_empty.len();
        let mut excerpt = String::new();
        let mut included = 0usize;
        for line in non_empty.iter().take(STDERR_MAX_LINES) {
            if excerpt.len() + line.len() + 1 > STDERR_MAX_BYTES {
                break;
            }
            if !excerpt.is_empty() {
                excerpt.push('\n');
            }
            excerpt.push_str(line);
            included += 1;
        }
        if included < total_lines {
            excerpt.push_str(&format!(
                "\n... [truncated; {} more line(s) omitted]",
                total_lines - included
            ));
        }
        excerpt
    };

    for candidate in &probe_candidates {
        let node_name = candidate.as_str();
        // Patch C-R-d: class-aware wording for operator diagnostics.
        // (a) "new sorry-free node" — fresh helper birth in this burst.
        // (b/c) "modified sorry-free node" — pre-existing node whose
        // .lean was edited and remains sorry-free post-delta.
        let class_label = if delta_scope
            .new_lean_files
            .iter()
            .any(|name| name == node_name)
        {
            "new sorry-free node"
        } else {
            "modified sorry-free node"
        };
        // Re-observe to determine post-delta sorry-free / proof-
        // bearing status. New nodes were already observed in the
        // new_lean_files loop above; this re-read pulls from cache via
        // the bwrap'd lake checker socket and is cheap (source file
        // read + .tex environment parse — no compile). R4 A3: the source
        // path is the target's (`.lean` ⇒ byte-identical on the live run;
        // `.thy` for IsabelleHol).
        let lean_path = trellis_kernel::node_source_path(repo_path, node_name, target);
        let tex_path = repo_path.join("Tablet").join(format!("{node_name}.tex"));
        let lean_content = if lean_path.exists() {
            std::fs::read_to_string(&lean_path).unwrap_or_default()
        } else {
            // Node source vanished after the snapshot was taken — skip the
            // probe (the node is no longer a present_node candidate).
            // The deterministic-revalidation pass will catch any state
            // drift at the next checkpoint.
            continue;
        };
        let tex_content = if tex_path.exists() {
            std::fs::read_to_string(&tex_path).unwrap_or_default()
        } else {
            String::new()
        };
        // Sorry-free is the textual `has_sorry` check (matches
        // `open_nodes_from_repo` / `EvaluatedNode::sorry_in_source`).
        // Transitive sorry_warnings are intentionally excluded — a
        // helper with a still-sorryd dep is its own concern; this
        // gate is "does THIS node's own file have a sorry token". R4 A3:
        // backend-parameterized scan (`Lean` ⇒ byte-identical; IsabelleHol
        // uses the tokenizer-based forbidden-command scan incl. the `sorry`
        // synthetic hit so this `sorry_in_source` derivation is correct).
        let forbidden_hits = scan_forbidden_keywords_for_target(target, &lean_content);
        let sorry_in_source = forbidden_hits_include_textual_sorry(&forbidden_hits);
        if sorry_in_source {
            // Worker left sorry in this helper — no record-eligible
            // transition. Skip; engine's step (a)/(b)/(c) bookkeeping
            // handles open-node state.
            continue;
        }
        // Skip Definition-kind nodes — they don't get closure records
        // (plan §7.2: records are scoped to proof-bearing nodes). The
        // engine's `classify_record_eligibility` returns NotProof for
        // these anyway, but short-circuiting here saves one IPC per
        // definition.
        let tex_env = tex_statement_environment(&tex_content);
        if !is_proof_bearing_statement_environment(&tex_env) {
            continue;
        }
        // Run the probe. Same machinery the MCA gate uses (plan §6.1
        // wiring), threaded through `present_nodes` / `node_kinds`
        // validation so kind-confused deps can't sneak into a record.
        // I/O failure bubbles up as Err — same contract as MCA.
        let mut local = run_local_closure_axioms(repo_path, node_name)?;
        // Patch C-R-e: same fix as MCA path. The probe for a new/
        // modified sorry-free node may reference other new helpers
        // added in this same burst (e.g. a multi-helper restructure
        // burst); admit them as valid deps.
        validate_probe_present_nodes(&mut local, &augmented_present_nodes, &augmented_node_kinds);
        if local.timed_out {
            let detail =
                format!("[internal] local-closure probe timed out for {class_label} `{node_name}`");
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        if local.returncode != 0 {
            // Patch C-R-d: surface the full stderr stream (cap-limited)
            // so the worker has actionable Lean diagnostics in the
            // rejection text, not just a 3-line teaser.
            let stderr_excerpt = format_stderr_excerpt(&local.raw_stderr);
            let detail = format!(
                "[internal] local-closure probe for {class_label} `{node_name}` exited with returncode={}:\n{}",
                local.returncode, stderr_excerpt
            );
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        if local.status != "ok" {
            let first_error = local.errors.first().cloned().unwrap_or_default();
            // Patch C-R-d: include raw_stderr alongside the structured
            // first-error so workers see Lean diagnostics here too. The
            // probe wrapper sometimes flips status to internal_error
            // because of an axcheck disagreement (sub-process succeeded
            // but disagrees with the primary collector) — in that case
            // raw_stderr may be empty or contain only the wrapper's
            // own logs, which is fine.
            let stderr_excerpt = format_stderr_excerpt(&local.raw_stderr);
            let mut detail = format!(
                "[internal] local-closure probe for {class_label} `{node_name}` status={}: {}",
                local.status, first_error
            );
            if !stderr_excerpt.is_empty() {
                detail.push_str("\nstderr:\n");
                detail.push_str(&stderr_excerpt);
            }
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        // B2c-gate Slice 2 — same backend split as the MCA path. For
        // IsabelleHol drop the mathlib axiom ceiling and run the server-cert
        // ORACLE/SHYPS/THEOREM gate; for Lean keep the `kernel_axioms ⊄
        // approved` rejection.
        if trellis_kernel::tablet_target_for_repo(repo_path)
            == trellis_kernel::backend::BackendId::IsabelleHol
        {
            if let Some(detail) =
                isabelle_cert_gate_violation(&local, &format!("{class_label} `{node_name}`"))
            {
                return Ok(WorkerValidationStepResult {
                    kind: "proof_worker_delta".to_string(),
                    ok: false,
                    detail: detail.clone(),
                    errors: vec![detail],
                    build_output,
                    allowed_nodes: authorized_nodes.clone(),
                    local_closure_results: BTreeMap::new(),
                });
            }
        } else {
            // Per-node approved set (plan §8) — `load_approved_axioms`
            // unions the canonical four with the per-node APPROVED_AXIOMS
            // entries. Same gate as the MCA path above.
            let approved = load_approved_axioms(repo_path, node_name)?;
            let violations: Vec<String> = local
                .kernel_axioms
                .iter()
                .filter(|a| !approved.contains(a.as_str()))
                .cloned()
                .collect();
            if !violations.is_empty() {
                // Patch C-R-d: append SORRY_AX_REJECTION_REMINDER when the
                // violations include `sorryAx`, matching the existing per-
                // node `#print axioms` audit's pedagogy at line 4259-4262.
                // Workers hitting this often introduced a sorry-free node
                // that transitively depends on a sorryd helper; the
                // reminder explains the fix.
                let mut detail = format!(
                    "[axiom] {class_label} `{node_name}` uses unapproved kernel axiom(s): {:?}",
                    violations
                );
                if violations.iter().any(|axiom| axiom == "sorryAx") {
                    detail.push(' ');
                    detail.push_str(SORRY_AX_REJECTION_REMINDER);
                }
                return Ok(WorkerValidationStepResult {
                    kind: "proof_worker_delta".to_string(),
                    ok: false,
                    detail: detail.clone(),
                    errors: vec![detail],
                    build_output,
                    allowed_nodes: authorized_nodes.clone(),
                    local_closure_results: BTreeMap::new(),
                });
            }
        }
        if !local.errors.is_empty() {
            let first = local.errors.first().cloned().unwrap_or_default();
            let detail = format!(
                "[strict] {class_label} `{node_name}` carries an unapproved-axiom or strict-context violation: {}",
                first
            );
            return Ok(WorkerValidationStepResult {
                kind: "proof_worker_delta".to_string(),
                ok: false,
                detail: detail.clone(),
                errors: vec![detail],
                build_output,
                allowed_nodes: authorized_nodes.clone(),
                local_closure_results: BTreeMap::new(),
            });
        }
        local_closure_results.insert(candidate.clone(), local);
    }

    Ok(WorkerValidationStepResult {
        kind: "proof_worker_delta".to_string(),
        ok: true,
        detail: String::new(),
        errors: Vec::new(),
        build_output,
        allowed_nodes: authorized_nodes.clone(),
        local_closure_results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    // ------ parse_lean_payload_into_closure_names ---------------------------
    //
    // Tests the pure parser extracted from `observe_protected_closure_nodes`.
    // The parser is the only piece of new logic that doesn't rely on disk
    // state, so it gets direct unit coverage; the surrounding wrapper is
    // exercised end-to-end through the synthetic harness.

    fn ns(items: &[&str]) -> BTreeSet<NodeId> {
        items
            .iter()
            .map(|s| NodeId::from((*s).to_string()))
            .collect()
    }

    fn strs<'a>(items: &'a [&'a str]) -> BTreeSet<&'a str> {
        items.iter().copied().collect()
    }

    /// Audit M-2 — single-source-of-truth regression. The runtime-CLI's
    /// `DEFAULT_APPROVED_AXIOMS` must equal the kernel-wide canonical
    /// constant as a set. Adding a platform-blessed axiom in one place
    /// without the other would create asymmetric acceptance between the
    /// engine accept-time ceiling (which uses the model constant via
    /// `ENGINE_CANONICAL_APPROVED_AXIOMS`) and the runtime-CLI defaults.
    /// Test fails loudly if the constants ever drift.
    #[test]
    fn default_approved_axioms_matches_canonical_constant() {
        let runtime_set: BTreeSet<&str> = DEFAULT_APPROVED_AXIOMS.iter().copied().collect();
        let canonical_set: BTreeSet<&str> = trellis_kernel::model::CANONICAL_APPROVED_AXIOMS
            .iter()
            .copied()
            .collect();
        assert_eq!(
            runtime_set, canonical_set,
            "DEFAULT_APPROVED_AXIOMS and CANONICAL_APPROVED_AXIOMS must agree; \
             update model.rs::CANONICAL_APPROVED_AXIOMS as the single source of truth"
        );
        // Defensive cross-check: both must contain at least the
        // mathlib-blessed four (propext, funext, Classical.choice,
        // Quot.sound). Catches a future PR that accidentally REMOVES
        // an axiom from the canonical list (set-equality alone would
        // pass against an empty list).
        for required in &["propext", "funext", "Classical.choice", "Quot.sound"] {
            assert!(
                canonical_set.contains(*required),
                "canonical approved-axioms set lost required axiom: {required}"
            );
        }
    }

    #[test]
    fn pending_under_model_assumption_is_provisionally_usable_and_revocable() {
        // Provisional admission (operator decision, 2026-07-02): a lane-passed
        // `status:"pending"` assumption IS in `load_approved_axioms`
        // (provisional, revocable); a gate REJECT removes it; the human-gate
        // projection makes it permanent. End-to-end through the same loader
        // the closure gate uses.
        use trellis_kernel::assumptions_registry as reg;
        let dir = tempdir().unwrap();
        let repo = dir.path();
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        let axiom = "dec2flt.slice_len_le_isize_max";
        let lean_statement = format!("axiom {axiom} : True");
        let nl_statement =
            "\\begin{definition}Every slice allocation length is at most isize::MAX.\\end{definition}";
        fs::write(
            repo.join("Tablet")
                .join(format!("{}.lean", reg::ASSUMPTIONS_NODE)),
            format!(
                "{}slice_len\n{}\n{}slice_len\n",
                reg::LEAN_ASSUMPTION_BEGIN_PREFIX,
                lean_statement,
                reg::LEAN_ASSUMPTION_END_PREFIX,
            ),
        )
        .unwrap();
        fs::write(
            repo.join("Tablet")
                .join(format!("{}.tex", reg::ASSUMPTIONS_NODE)),
            format!(
                "{}slice_len\n{}\n{}slice_len\n",
                reg::TEX_ASSUMPTION_BEGIN_PREFIX,
                nl_statement,
                reg::TEX_ASSUMPTION_END_PREFIX,
            ),
        )
        .unwrap();
        let record = reg::ProposedAssumption {
            id: "slice_len".into(),
            axiom_name: axiom.into(),
            lean_statement,
            nl_statement: nl_statement.into(),
            citation_locator: "Rust Reference: alloc <= isize::MAX".into(),
            rust_justification: "the allocator never produces a larger allocation".into(),
            needed_by: vec!["parse_number_faithful".into()],
            claim_class: String::new(),
            adversarial_hunt_result: "failed to refute over 10k runs".into(),
            relativization_probe: String::new(),
            status: "pending".into(),
            rejected_reason: String::new(),
        };
        reg::record_proposed(repo, record).unwrap();

        // While PENDING: the axiom IS in the effective set — provisional
        // admission so dependent closures are not quarantined until the gate.
        let approved = load_approved_axioms(repo, "Correct").unwrap();
        assert!(
            approved.contains(axiom),
            "a lane-passed pending under-model assumption must be provisionally \
             in load_approved_axioms"
        );

        // Human ratifies -> project (permanent, via APPROVED_AXIOMS.json).
        reg::project_all_pending(repo).unwrap();
        let approved = load_approved_axioms(repo, "Correct").unwrap();
        assert!(
            approved.contains(axiom),
            "an approved under-model assumption must be in load_approved_axioms"
        );
        // The four-axiom floor is still present alongside it.
        for required in &["propext", "funext", "Classical.choice", "Quot.sound"] {
            assert!(approved.contains(*required));
        }

        // Gate REJECT on a second pending entry: revoked — the axiom drops
        // out of the effective set (and only ever sat there provisionally).
        let axiom_b = "dec2flt.slice_len_le_isize_max_b";
        let lean_b = format!("axiom {axiom_b} : True");
        fs::write(
            repo.join("Tablet")
                .join(format!("{}.lean", reg::ASSUMPTIONS_NODE)),
            format!(
                "{}slice_len_b\n{}\n{}slice_len_b\n",
                reg::LEAN_ASSUMPTION_BEGIN_PREFIX,
                lean_b,
                reg::LEAN_ASSUMPTION_END_PREFIX,
            ),
        )
        .unwrap();
        fs::write(
            repo.join("Tablet")
                .join(format!("{}.tex", reg::ASSUMPTIONS_NODE)),
            format!(
                "{}slice_len_b\n{}\n{}slice_len_b\n",
                reg::TEX_ASSUMPTION_BEGIN_PREFIX,
                nl_statement,
                reg::TEX_ASSUMPTION_END_PREFIX,
            ),
        )
        .unwrap();
        let record_b = reg::ProposedAssumption {
            id: "slice_len_b".into(),
            axiom_name: axiom_b.into(),
            lean_statement: lean_b,
            nl_statement: nl_statement.into(),
            citation_locator: "Rust Reference: alloc <= isize::MAX".into(),
            rust_justification: "the allocator never produces a larger allocation".into(),
            needed_by: vec!["parse_number_faithful".into()],
            claim_class: String::new(),
            adversarial_hunt_result: "failed to refute over 10k runs".into(),
            relativization_probe: String::new(),
            status: "pending".into(),
            rejected_reason: String::new(),
        };
        reg::record_proposed(repo, record_b).unwrap();
        let approved = load_approved_axioms(repo, "Correct").unwrap();
        assert!(approved.contains(axiom_b), "pending entry b must be provisional");
        reg::reject(repo, "slice_len_b", "operator rejected in test").unwrap();
        let approved = load_approved_axioms(repo, "Correct").unwrap();
        assert!(
            !approved.contains(axiom_b),
            "a rejected under-model assumption must NOT be in load_approved_axioms"
        );
        // The permanently projected first axiom is unaffected by the reject.
        assert!(approved.contains(axiom));
    }

    #[test]
    fn deviation_fingerprints_track_file_content_and_missing_files() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        fs::create_dir_all(repo.join("reference")).unwrap();
        let id = DeviationId::from("dev:a");
        let files = BTreeMap::from([(id.clone(), "reference/dev_a.tex".to_string())]);

        fs::write(repo.join("reference/dev_a.tex"), "").unwrap();
        let empty_existing = observe_deviation_fingerprints(repo, &files).unwrap();
        assert_ne!(empty_existing.get(&id), Some(&String::new()));

        fs::write(repo.join("reference/dev_a.tex"), "first").unwrap();
        let first = observe_deviation_fingerprints(repo, &files).unwrap();
        fs::write(repo.join("reference/dev_a.tex"), "second").unwrap();
        let second = observe_deviation_fingerprints(repo, &files).unwrap();
        fs::remove_file(repo.join("reference/dev_a.tex")).unwrap();
        let missing = observe_deviation_fingerprints(repo, &files).unwrap();

        assert_ne!(first.get(&id), second.get(&id));
        assert_eq!(missing.get(&id), Some(&String::new()));
    }

    #[test]
    fn deviation_fingerprint_observation_rejects_paths_outside_reference() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        let id = DeviationId::from("dev:a");

        for path in [
            "/tmp/dev_a.tex",
            "../dev_a.tex",
            "reference/../dev_a.tex",
            "Tablet/dev_a.tex",
            "reference/dev_a.lean",
        ] {
            let files = BTreeMap::from([(id.clone(), path.to_string())]);
            assert!(
                observe_deviation_fingerprints(repo, &files).is_err(),
                "path should be rejected: {path}"
            );
        }
    }

    #[test]
    fn substantiveness_fingerprint_includes_claimed_deviation_fingerprints() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::write(repo.join("Tablet/N.tex"), "\\begin{theorem}N\\end{theorem}").unwrap();
        fs::write(repo.join("paper.tex"), "paper").unwrap();
        let node = NodeId::from("N");
        let deviation = DeviationId::from("dev:a");
        let nodes = BTreeSet::from([node.clone()]);
        let kinds = BTreeMap::from([(node.clone(), crate::NodeKind::Definition)]);

        let none = observe_substantiveness_fingerprints(
            repo,
            &nodes,
            Some(Path::new("paper.tex")),
            &kinds,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let claimed = observe_substantiveness_fingerprints(
            repo,
            &nodes,
            Some(Path::new("paper.tex")),
            &kinds,
            &BTreeMap::from([(node.clone(), BTreeSet::from([deviation.clone()]))]),
            &BTreeMap::from([(deviation, "dev-fp".to_string())]),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();

        assert_ne!(none.get(&node), claimed.get(&node));
        let parsed = SubstantivenessFingerprint::from_storage_string(
            claimed.get(&node).expect("fingerprint"),
        )
        .expect("parse fingerprint");
        assert_eq!(
            parsed.claimed_deviation_fingerprints,
            BTreeMap::from([(DeviationId::from("dev:a"), "dev-fp".to_string())])
        );
    }

    #[test]
    fn claim_free_substantiveness_fingerprint_is_byte_identical_to_pre_feature_shape() {
        // Golden string: a fingerprint with no reference-paper claims
        // must serialize EXACTLY as the pre-feature (master) shape —
        // `claimed_reference_shas` is skipped when empty, so no stored
        // substantiveness baseline reopens when the new binary deploys.
        let fp = SubstantivenessFingerprint {
            own_tex: "OT".to_string(),
            paper_source_sha: "PS".to_string(),
            node_kind: "proof".to_string(),
            claimed_deviation_fingerprints: BTreeMap::new(),
            claimed_reference_shas: BTreeMap::new(),
        };
        assert_eq!(
            fp.to_storage_string(),
            r#"{"own_tex":"OT","paper_source_sha":"PS","node_kind":"proof","claimed_deviation_fingerprints":{}}"#
        );
        // And a legacy-serialized fingerprint parses with empty claims.
        let parsed = SubstantivenessFingerprint::from_storage_string(
            r#"{"own_tex":"OT","paper_source_sha":"PS","node_kind":"proof","claimed_deviation_fingerprints":{}}"#,
        )
        .expect("legacy fingerprint parses");
        assert_eq!(parsed, fp);
    }

    #[test]
    fn substantiveness_fingerprint_includes_claimed_reference_shas() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::create_dir_all(repo.join("paper/refs")).unwrap();
        fs::write(repo.join("Tablet/N.tex"), "\\begin{theorem}N\\end{theorem}").unwrap();
        fs::write(repo.join("paper.tex"), "paper").unwrap();
        fs::write(repo.join("paper/refs/smith2020.tex"), "reference content v1").unwrap();
        let node = NodeId::from("N");
        let ref_id = RefPaperId::from("smith2020");
        let nodes = BTreeSet::from([node.clone()]);
        let kinds = BTreeMap::from([(node.clone(), crate::NodeKind::Definition)]);
        let registry = BTreeMap::from([(
            ref_id.clone(),
            ReferencePaperSpec {
                tex_path: "paper/refs/smith2020.tex".to_string(),
                source_id: "Smith 2020".to_string(),
            },
        )]);
        let grounds = BTreeMap::from([(node.clone(), BTreeSet::from([ref_id.clone()]))]);

        let none = observe_substantiveness_fingerprints(
            repo,
            &nodes,
            Some(Path::new("paper.tex")),
            &kinds,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &registry,
            &BTreeMap::new(),
        )
        .unwrap();
        let claimed = observe_substantiveness_fingerprints(
            repo,
            &nodes,
            Some(Path::new("paper.tex")),
            &kinds,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &registry,
            &grounds,
        )
        .unwrap();
        // Claiming reopens (fingerprint changes) …
        assert_ne!(none.get(&node), claimed.get(&node));
        let parsed =
            SubstantivenessFingerprint::from_storage_string(claimed.get(&node).unwrap())
                .expect("parse fingerprint");
        let sha_v1 = parsed
            .claimed_reference_shas
            .get(&ref_id)
            .expect("claimed sha present")
            .clone();
        assert!(!sha_v1.is_empty());

        // … and so does editing the claimed reference file.
        fs::write(repo.join("paper/refs/smith2020.tex"), "reference content v2").unwrap();
        let reclaimed = observe_substantiveness_fingerprints(
            repo,
            &nodes,
            Some(Path::new("paper.tex")),
            &kinds,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &registry,
            &grounds,
        )
        .unwrap();
        assert_ne!(claimed.get(&node), reclaimed.get(&node));
    }

    #[test]
    fn parse_lean_payload_collapses_proof_match_artefacts_under_parent() {
        // Lean elaborator emits Foo._proof_1, Foo._cstage_*, Foo.match_1
        // for any def with a by-block; the fingerprint script owner-tags
        // each with the declaring module's node id, so all of them
        // aggregate under the user-authored Tablet node `Foo` (and then
        // the present_nodes filter applies).
        let payload = "root|Seed|decl=crate.Seed||\
            const|crate.Foo._proof_1|node=Foo|thm|lvls=|typehash=1||\
            const|crate.Foo.match_1|node=Foo|def|lvls=|hash=2||\
            const|crate.Foo._cstage_2|node=Foo|def|lvls=|hash=3||\
            const|crate.Foo|node=Foo|def|lvls=|hash=4||\
            extern|Mathlib.Whatever";
        let out = parse_lean_payload_into_closure_names(
            payload,
            &strs(&["Seed"]),
            &strs(&["Foo", "Seed"]),
        );
        assert_eq!(out, ns(&["Foo"]), "all Foo-owned artefacts collapse to Foo");
    }

    #[test]
    fn parse_lean_payload_drops_seed_extern_and_preamble() {
        // The seed itself is in `coverage` already, so it must not
        // re-enter as a closure descendant. Externs are mathlib-side
        // and never enter. Preamble is structurally excluded.
        let payload = "root|Seed|decl=crate.Seed||\
            const|crate.Seed|node=Seed|thm|lvls=|typehash=1||\
            const|crate.Helper|node=Helper|def|lvls=|hash=2||\
            const|crate.BiasedFp|node=Preamble|def|lvls=|hash=3||\
            extern|Real.exp||\
            extern|SimpleGraph.Adj";
        let out = parse_lean_payload_into_closure_names(
            payload,
            &strs(&["Seed"]),
            &strs(&["Seed", "Helper", "Preamble"]),
        );
        assert_eq!(
            out,
            ns(&["Helper"]),
            "Seed (covering), Preamble-owned (filtered), externs (Mathlib) are all dropped"
        );
    }

    #[test]
    fn parse_lean_payload_filters_names_not_in_present_nodes() {
        // Defensive filter: an owner id that is not a kernel NodeId
        // (e.g. a module outside the tablet scan) must not leak into
        // the protected closure.
        let payload = "root|Seed|decl=crate.Seed||\
            const|crate.HelperInPreamble|node=NotANode|def|lvls=|hash=1||\
            const|crate.RealHelper|node=RealHelper|def|lvls=|hash=2";
        let out = parse_lean_payload_into_closure_names(
            payload,
            &strs(&["Seed"]),
            // NotANode is intentionally NOT in present_nodes; RealHelper
            // IS a kernel node and survives.
            &strs(&["Seed", "RealHelper"]),
        );
        assert_eq!(
            out,
            ns(&["RealHelper"]),
            "owners not in present_nodes must not leak into the protected closure"
        );
    }

    #[test]
    fn parse_lean_payload_handles_empty_root_only_and_ownerless_lines() {
        let empty = parse_lean_payload_into_closure_names("", &strs(&[]), &strs(&[]));
        assert!(empty.is_empty(), "empty payload yields empty closure");
        let root_only =
            parse_lean_payload_into_closure_names("root|Seed", &strs(&["Seed"]), &strs(&["Seed"]));
        assert!(
            root_only.is_empty(),
            "a payload with only the root line yields empty closure"
        );
        // A pre-format const line (no node= owner field) is skipped, never
        // first-component-guessed: `FiniteDecimal.value` must NOT attribute
        // to the (distinct, existing) node `FiniteDecimal`.
        let legacy = parse_lean_payload_into_closure_names(
            "root|Seed||const|FiniteDecimal.value|def|lvls=|hash=1",
            &strs(&["Seed"]),
            &strs(&["Seed", "FiniteDecimal", "FiniteDecimal_value"]),
        );
        assert!(
            legacy.is_empty(),
            "ownerless const lines are skipped, not misattributed by first component"
        );
        // With the owner field, the same const attributes correctly.
        let owned = parse_lean_payload_into_closure_names(
            "root|Seed||const|crate.FiniteDecimal.value|node=FiniteDecimal_value|def|lvls=|hash=1",
            &strs(&["Seed"]),
            &strs(&["Seed", "FiniteDecimal", "FiniteDecimal_value"]),
        );
        assert_eq!(owned, ns(&["FiniteDecimal_value"]));
    }

    // ------ CorrespondenceFingerprint + corr_reopen_triggered --------------

    fn cf(
        own_tex: &str,
        lean: &str,
        preamble: &str,
        defs: &[(&str, &str)],
    ) -> CorrespondenceFingerprint {
        // Test helper populates `lean_relevant_dependencies` from the same
        // names as `lean_relevant_definition_descendants` (the def names are
        // a subset of all dependencies). Tests that need a non-def dependency
        // can construct the fingerprint directly.
        CorrespondenceFingerprint {
            own_tex: own_tex.to_string(),
            lean_semantic_closure: lean.to_string(),
            lean_relevant_definition_descendants: defs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            lean_relevant_dependencies: defs.iter().map(|(k, _)| (*k).to_string()).collect(),
            preamble_tex: preamble.to_string(),
        }
    }

    #[test]
    fn corr_reopen_identical_fingerprints_is_false() {
        let fp = cf("ot", "lc", "pre", &[("D1", "d1h"), ("D2", "d2h")]);
        let s = fp.to_storage_string();
        assert!(!corr_reopen_triggered(&s, &s));
    }

    #[test]
    fn schema_repair_repins_corr_legacy_approved_when_semantically_equivalent() {
        let mut state = crate::ProtocolState::default();
        let node = NodeId::from("N");
        let current = serde_json::json!({
            "own_tex": "own",
            "lean_semantic_closure": "lean",
            "lean_relevant_definition_descendants": {"D": "dh"},
            "lean_relevant_dependencies": ["D", "T"],
            "preamble_tex": "pre",
        })
        .to_string();
        let legacy_approved = serde_json::json!({
            "own_tex": "own",
            "lean_semantic_closure": "lean",
            "definition_descendants": {"D": "dh", "IrrelevantOldDef": "ih"},
            "preamble_tex": "pre",
        })
        .to_string();

        state
            .live
            .corr_current_fingerprints
            .insert(node.clone(), current.clone());
        state
            .corr_approved_fingerprints
            .insert(node.clone(), legacy_approved.clone());
        state
            .committed
            .corr_current_fingerprints
            .insert(node.clone(), legacy_approved);

        assert!(repair_schema_equivalent_fingerprint_baselines(&mut state));
        assert_eq!(state.corr_approved_fingerprints.get(&node), Some(&current));
        assert_eq!(
            state.committed.corr_current_fingerprints.get(&node),
            Some(&current)
        );
    }

    #[test]
    fn schema_repair_does_not_repin_corr_real_drift() {
        let mut state = crate::ProtocolState::default();
        let node = NodeId::from("N");
        let current = serde_json::json!({
            "own_tex": "own",
            "lean_semantic_closure": "lean",
            "lean_relevant_definition_descendants": {"D": "NEW"},
            "preamble_tex": "pre",
        })
        .to_string();
        let legacy_approved = serde_json::json!({
            "own_tex": "own",
            "lean_semantic_closure": "lean",
            "definition_descendants": {"D": "old"},
            "preamble_tex": "pre",
        })
        .to_string();

        state
            .live
            .corr_current_fingerprints
            .insert(node.clone(), current);
        state
            .corr_approved_fingerprints
            .insert(node.clone(), legacy_approved.clone());

        assert!(!repair_schema_equivalent_fingerprint_baselines(&mut state));
        assert_eq!(
            state.corr_approved_fingerprints.get(&node),
            Some(&legacy_approved)
        );
    }

    #[test]
    fn schema_repair_repins_paper_legacy_approved_when_semantically_equivalent() {
        let mut state = crate::ProtocolState::default();
        let target = TargetId::from("main");
        let current = serde_json::json!({
            "target": "main",
            "covering_nodes": {"Main": "mh"},
            "lean_relevant_definition_descendants": {"D": "dh"},
            "preamble_definition_hashes": ["pre"],
        })
        .to_string();
        let legacy_approved = serde_json::json!({
            "target": "main",
            "covering_nodes": {"Main": "mh"},
            "definition_nodes": {"D": "dh", "IrrelevantOldDef": "ih"},
            "preamble_definition_hashes": ["pre"],
        })
        .to_string();

        state
            .live
            .paper_current_fingerprints
            .insert(target.clone(), current.clone());
        state
            .paper_approved_fingerprints
            .insert(target.clone(), legacy_approved.clone());
        state
            .committed
            .paper_current_fingerprints
            .insert(target.clone(), legacy_approved);

        assert!(repair_schema_equivalent_fingerprint_baselines(&mut state));
        assert_eq!(
            state.paper_approved_fingerprints.get(&target),
            Some(&current)
        );
        assert_eq!(
            state.committed.paper_current_fingerprints.get(&target),
            Some(&current)
        );
    }

    #[test]
    fn migration_at_current_schema_reruns_repair_when_drift_emerges() {
        // Regression: repair_schema_equivalent_fingerprint_baselines used to
        // be gated on the schema version, which meant a state checkpointed
        // at the current version with OLD-shape approved + NEW-shape live
        // (from a mid-run hydration write) would never get re-paired and the
        // protected paper-target covering nodes would silently report DRIFT
        // / Unknown. Run at the CURRENT version so only the every-load
        // repair path fires (the one-shot axis migration is version-gated).
        let tmp = tempfile::tempdir().unwrap();
        let mut state = crate::ProtocolState::default();
        state.corr_fingerprint_schema_version = CORR_FINGERPRINT_SCHEMA_VERSION;
        let node = NodeId::from("N");
        let current = serde_json::json!({
            "own_tex": "own",
            "lean_semantic_closure": "lean",
            "lean_relevant_definition_descendants": {"D": "dh"},
            "lean_relevant_dependencies": ["D"],
            "preamble_tex": "pre",
        })
        .to_string();
        let legacy_approved = serde_json::json!({
            "own_tex": "own",
            "lean_semantic_closure": "lean",
            "definition_descendants": {"D": "dh", "IrrelevantOldDef": "ih"},
            "preamble_tex": "pre",
        })
        .to_string();
        state
            .live
            .corr_current_fingerprints
            .insert(node.clone(), current.clone());
        state
            .corr_approved_fingerprints
            .insert(node.clone(), legacy_approved);

        let changed =
            migrate_corr_fingerprint_schema(&mut state, tmp.path()).expect("migration ok");
        assert!(changed, "repair must fire and report change");
        assert_eq!(
            state.corr_approved_fingerprints.get(&node),
            Some(&current),
            "approved should be re-pinned to NEW shape"
        );
        assert_eq!(
            state.corr_fingerprint_schema_version,
            CORR_FINGERPRINT_SCHEMA_VERSION
        );
    }

    #[test]
    fn migration_at_schema_3_preserves_real_drift() {
        // Companion to the above: schema=3 + REAL semantic drift (different
        // descendant hash, not just legacy schema shape) must NOT be
        // auto-pinned. The verifier still needs to adjudicate.
        let tmp = tempfile::tempdir().unwrap();
        let mut state = crate::ProtocolState::default();
        state.corr_fingerprint_schema_version = 3;
        let node = NodeId::from("N");
        let current = serde_json::json!({
            "own_tex": "own",
            "lean_semantic_closure": "lean",
            "lean_relevant_definition_descendants": {"D": "NEW_HASH"},
            "preamble_tex": "pre",
        })
        .to_string();
        let approved = serde_json::json!({
            "own_tex": "own",
            "lean_semantic_closure": "lean",
            "definition_descendants": {"D": "OLD_HASH"},
            "preamble_tex": "pre",
        })
        .to_string();
        state
            .live
            .corr_current_fingerprints
            .insert(node.clone(), current);
        state
            .corr_approved_fingerprints
            .insert(node.clone(), approved.clone());

        migrate_corr_fingerprint_schema(&mut state, tmp.path()).expect("migration ok");
        assert_eq!(
            state.corr_approved_fingerprints.get(&node),
            Some(&approved),
            "approved should be unchanged for real drift (verifier must adjudicate)"
        );
    }

    // ---------- normalize_declaration: signature hash stability -----------

    #[test]
    fn normalize_declaration_strips_body_binding_uniformly() {
        // These all share the same signature; the body varies. They must
        // all normalize identically so the declaration hash doesn't flag
        // a proof-body edit as a "signature changed" error.
        let sig = "theorem T : Nat";
        let variants = [
            "theorem T : Nat := by sorry",        // placeholder
            "theorem T : Nat := by",              // body continued on next line
            "theorem T : Nat := sorry",           // direct sorry
            "theorem T : Nat := by exact 0",      // by + term
            "theorem T : Nat := fun _ => 0",      // term-mode
            "theorem T : Nat := show Nat from 0", // show..from
            "theorem T : Nat :=",                 // trailing :=
            "theorem T : Nat",                    // no binding at all
        ];
        let expected = normalize_declaration(sig);
        for decl in variants {
            assert_eq!(
                normalize_declaration(decl),
                expected,
                "variant {decl:?} normalized differently"
            );
        }
    }

    #[test]
    fn normalize_declaration_preserves_default_argument_binding() {
        // Default-argument `:=` inside the signature must survive; only the
        // trailing body `:=` is stripped (`rfind` semantics).
        let with_default_sorry = "theorem T (x : Nat := 0) : Nat := by sorry";
        let with_default_proof = "theorem T (x : Nat := 0) : Nat := by exact x";
        assert_eq!(
            normalize_declaration(with_default_sorry),
            normalize_declaration(with_default_proof)
        );
        // And the default value itself is retained in the normalized form,
        // so changing it is still detected.
        let with_different_default = "theorem T (x : Nat := 1) : Nat := by sorry";
        assert_ne!(
            normalize_declaration(with_default_sorry),
            normalize_declaration(with_different_default)
        );
    }

    #[test]
    fn forbidden_keyword_scan_distinguishes_textual_sorry_from_sorry_ax() {
        let sorry_ax_only = "-- [TABLET NODE: T]\ntheorem T : True := by\n  exact sorryAx\n";
        let hits = scan_forbidden_keywords(sorry_ax_only);
        assert!(
            hits.iter().any(|hit| hit.keyword == "sorryAx"),
            "`sorryAx` must be caught by the forbidden-keyword scan"
        );
        assert!(
            !forbidden_hits_include_textual_sorry(&hits),
            "`sorryAx` must not make the node textually open"
        );

        let textual_sorry = "-- [TABLET NODE: T]\ntheorem T : True := by\n  sorry\n";
        let hits = scan_forbidden_keywords(textual_sorry);
        assert!(
            forbidden_hits_include_textual_sorry(&hits),
            "a real `sorry` token must still make the node open"
        );
    }

    #[test]
    fn corr_reopen_own_tex_change_triggers() {
        let a = cf("a", "lc", "pre", &[("D1", "d1h")]).to_storage_string();
        let p = cf("b", "lc", "pre", &[("D1", "d1h")]).to_storage_string();
        assert!(corr_reopen_triggered(&a, &p));
    }

    #[test]
    fn corr_reopen_lean_semantic_change_triggers() {
        let a = cf("ot", "lc1", "pre", &[]).to_storage_string();
        let p = cf("ot", "lc2", "pre", &[]).to_storage_string();
        assert!(corr_reopen_triggered(&a, &p));
    }

    #[test]
    fn corr_reopen_preamble_change_triggers() {
        let a = cf("ot", "lc", "pre1", &[]).to_storage_string();
        let p = cf("ot", "lc", "pre2", &[]).to_storage_string();
        assert!(corr_reopen_triggered(&a, &p));
    }

    #[test]
    fn corr_reopen_baselined_descendant_tex_change_triggers() {
        let a = cf("ot", "lc", "pre", &[("D1", "d1h")]).to_storage_string();
        let p = cf("ot", "lc", "pre", &[("D1", "d1h_v2")]).to_storage_string();
        assert!(corr_reopen_triggered(&a, &p));
    }

    #[test]
    fn corr_reopen_baselined_descendant_missing_triggers() {
        let a = cf("ot", "lc", "pre", &[("D1", "d1h")]).to_storage_string();
        let p = cf("ot", "lc", "pre", &[]).to_storage_string();
        assert!(corr_reopen_triggered(&a, &p));
    }

    #[test]
    fn corr_reopen_new_descendant_not_in_baseline_is_ignored() {
        let a = cf("ot", "lc", "pre", &[("D1", "d1h")]).to_storage_string();
        let p = cf("ot", "lc", "pre", &[("D1", "d1h"), ("D_NEW", "h_new")]).to_storage_string();
        assert!(!corr_reopen_triggered(&a, &p));
    }

    #[test]
    fn corr_reopen_empty_approved_storage_counts_as_reopen() {
        // Legacy single-hash or empty storage. Forces re-approval rather than
        // silently trusting an ambiguous baseline.
        let p = cf("ot", "lc", "pre", &[]).to_storage_string();
        assert!(corr_reopen_triggered("", &p));
        assert!(corr_reopen_triggered("legacy-single-hash-abcd", &p));
    }

    #[test]
    fn corr_reopen_empty_prospective_counts_as_reopen() {
        let a = cf("ot", "lc", "pre", &[]).to_storage_string();
        assert!(corr_reopen_triggered(&a, ""));
    }

    // (The `corr_reopen_triggered_with_descendant_filter` band-aid has
    // been removed; its tests are deleted with it. The Lean-relevance
    // refactor narrows the descendant axis at fingerprint construction
    // time instead of via a runtime filter.)

    #[test]
    fn corr_fingerprint_storage_string_is_deterministic() {
        let fp1 = cf(
            "ot",
            "lc",
            "pre",
            &[("D2", "h2"), ("D1", "h1"), ("D3", "h3")],
        );
        let fp2 = cf(
            "ot",
            "lc",
            "pre",
            &[("D1", "h1"), ("D2", "h2"), ("D3", "h3")],
        );
        assert_eq!(fp1.to_storage_string(), fp2.to_storage_string());
    }

    // ------ diff_corr_fingerprint_axes -------------------------------------

    #[test]
    fn diff_axes_detects_own_tex_change() {
        let a = cf("own_a", "lc", "pre", &[]).to_storage_string();
        let p = cf("own_b", "lc", "pre", &[]).to_storage_string();
        let bullets = diff_corr_fingerprint_axes(&a, &p);
        assert!(bullets.iter().any(|b| b.contains("`.tex` statement block")));
    }

    #[test]
    fn diff_axes_detects_lean_change() {
        let a = cf("ot", "lc1", "pre", &[]).to_storage_string();
        let p = cf("ot", "lc2", "pre", &[]).to_storage_string();
        let bullets = diff_corr_fingerprint_axes(&a, &p);
        assert!(bullets.iter().any(|b| b.contains("Lean semantic closure")));
    }

    #[test]
    fn diff_axes_detects_baselined_descendant_tex_change_and_missing() {
        let a = cf("ot", "lc", "pre", &[("D1", "h1"), ("D2", "h2")]).to_storage_string();
        let p = cf("ot", "lc", "pre", &[("D1", "h1_new")]).to_storage_string();
        let bullets = diff_corr_fingerprint_axes(&a, &p);
        let combined: String = bullets.join("\n");
        assert!(combined.contains("D1"));
        assert!(combined.contains("D2"));
    }

    #[test]
    fn diff_axes_empty_when_equal() {
        let fp = cf("ot", "lc", "pre", &[("D1", "h1")]).to_storage_string();
        assert!(diff_corr_fingerprint_axes(&fp, &fp).is_empty());
    }

    #[test]
    fn diff_axes_legacy_baseline_reports_missing() {
        let p = cf("ot", "lc", "pre", &[]).to_storage_string();
        let bullets = diff_corr_fingerprint_axes("legacy-hash", &p);
        assert_eq!(bullets.len(), 1);
        assert!(bullets[0].contains("Approved baseline"));
    }

    // ------ paper_target_corr_reopen_guard_errors (unit level, w/o Lake) ---
    //
    // These tests use legacy_correspondence_fingerprint via
    // observe_correspondence_fingerprints on repos without a lakefile — so
    // lean_semantic_closure in the prospective fingerprint is the
    // declaration-with-imports surrogate. That's sufficient to exercise the
    // reopen-guard logic.

    fn write_node(repo: &Path, name: &str, tex: &str, lean: &str) {
        write(&repo.join("Tablet").join(format!("{name}.tex")), tex);
        write(&repo.join("Tablet").join(format!("{name}.lean")), lean);
    }

    #[test]
    fn soundness_fingerprint_v2_hashes_noderef_statements_not_all_imports() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\nimport Tablet.C\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B statement v1\\end{theorem}\n\\begin{proof}B proof v1.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );
        write_node(
            repo,
            "C",
            "\\begin{theorem}C statement v1\\end{theorem}\n\\begin{proof}C proof v1.\\end{proof}\n",
            "-- [TABLET NODE: C]\nimport Tablet.Preamble\n\ntheorem C : True := sorry\n",
        );

        let baseline = soundness_fingerprint_v2(repo, "A");
        assert!(baseline.starts_with("sound-v2:"));

        write_node(
            repo,
            "C",
            "\\begin{theorem}C statement v2\\end{theorem}\n\\begin{proof}C proof v1.\\end{proof}\n",
            "-- [TABLET NODE: C]\nimport Tablet.Preamble\n\ntheorem C : True := sorry\n",
        );
        assert_eq!(
            soundness_fingerprint_v2(repo, "A"),
            baseline,
            "changing an imported but uncited node must not reopen soundness"
        );

        write_node(
            repo,
            "B",
            "\\begin{theorem}B statement v1\\end{theorem}\n\\begin{proof}B proof v2.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );
        assert_eq!(
            soundness_fingerprint_v2(repo, "A"),
            baseline,
            "changing a cited node's proof text must not reopen the consumer"
        );

        write_node(
            repo,
            "B",
            "\\begin{theorem}B statement v2\\end{theorem}\n\\begin{proof}B proof v2.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );
        assert_ne!(
            soundness_fingerprint_v2(repo, "A"),
            baseline,
            "changing a cited node's statement must reopen the consumer"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_rejects_noderef_outside_import_closure() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        assert_eq!(
            soundness_fingerprint_v2(repo, "A"),
            "",
            "noderef targets outside the Lean import closure must be flagged"
        );
    }

    /// Build a minimal `NodeObservation` directly from on-disk node files,
    /// bypassing the `observe_node` path (which shells out to the
    /// checker's `lean-compile-node` script). The noderef-closure check
    /// inside `evaluate_node_observation` only reads
    /// `observation.tex_content` plus the repo filesystem — no Lean
    /// build is required.
    fn observation_from_disk_for_test(repo: &Path, node: &str) -> NodeObservation {
        let lean_path = repo.join("Tablet").join(format!("{node}.lean"));
        let tex_path = repo.join("Tablet").join(format!("{node}.tex"));
        let lean_exists = lean_path.is_file();
        let tex_exists = tex_path.is_file();
        NodeObservation {
            node: node.to_string(),
            lean_path: lean_path.display().to_string(),
            tex_path: tex_path.display().to_string(),
            lean_exists,
            tex_exists,
            lean_content: if lean_exists {
                std::fs::read_to_string(&lean_path).unwrap_or_default()
            } else {
                String::new()
            },
            tex_content: if tex_exists {
                std::fs::read_to_string(&tex_path).unwrap_or_default()
            } else {
                String::new()
            },
            compile: empty_external_command_observation(),
            print_axioms: empty_external_command_observation(),
        }
    }

    /// Positive case: when every `\noderef{X}` in a node's `.tex` proof
    /// block points to a node in its Lean import closure (and X.lean +
    /// X.tex are both present), `evaluate_node_observation` MUST NOT
    /// emit any noderef-closure errors.
    #[test]
    fn evaluate_node_observation_accepts_noderef_in_import_closure() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        let observation = observation_from_disk_for_test(repo, "A");
        let evaluated = evaluate_node_observation(repo, &observation, None);
        assert!(
            !evaluated
                .errors
                .iter()
                .any(|e| e.contains("Lean import closure")),
            "in-closure noderef must not produce a closure rejection error: {:?}",
            evaluated.errors
        );
    }

    /// Negative case: when a node's `.tex` proof block cites
    /// `\noderef{X}` and X is NOT in the citing node's Lean import
    /// closure, `evaluate_node_observation` MUST emit one (and only
    /// one) error of the expected shape, naming the bad reference and
    /// pointing the worker at the fix. This is the deterministic
    /// hard-rejection that replaces the old indirect fingerprint gate.
    #[test]
    fn evaluate_node_observation_rejects_noderef_outside_import_closure() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        let observation = observation_from_disk_for_test(repo, "A");
        let evaluated = evaluate_node_observation(repo, &observation, None);
        let closure_errors: Vec<&String> = evaluated
            .errors
            .iter()
            .filter(|e| e.contains("Lean import closure"))
            .collect();
        assert_eq!(
            closure_errors.len(),
            1,
            "expected exactly one closure rejection error, got: {:?}",
            evaluated.errors
        );
        let msg = closure_errors[0];
        assert!(
            msg.contains("\\noderef{B}"),
            "error must name the bad reference verbatim: {msg}"
        );
        assert!(
            msg.contains("Tablet/A.tex"),
            "error must name the citing node's tex path: {msg}"
        );
        assert!(
            msg.contains("import Tablet.B"),
            "error must suggest the import-line fix: {msg}"
        );
    }

    /// Cycle case (friction sweep, request 2109): when the cited node
    /// transitively imports the citing node, "add the import" is
    /// unsatisfiable advice — the message must name the cycle and drop
    /// the import suggestion.
    #[test]
    fn evaluate_node_observation_noderef_cycle_drops_import_suggestion() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // A cites B, but B already imports A — adding `import Tablet.B`
        // to A.lean would create an import cycle.
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\nimport Tablet.A\n\ntheorem B : True := sorry\n",
        );

        let observation = observation_from_disk_for_test(repo, "A");
        let evaluated = evaluate_node_observation(repo, &observation, None);
        let closure_errors: Vec<&String> = evaluated
            .errors
            .iter()
            .filter(|e| e.contains("Lean import closure"))
            .collect();
        assert_eq!(
            closure_errors.len(),
            1,
            "expected exactly one closure rejection error, got: {:?}",
            evaluated.errors
        );
        let msg = closure_errors[0];
        assert!(
            msg.contains("would create an import cycle"),
            "error must name the cycle: {msg}"
        );
        assert!(
            msg.contains("\\noderef{B}"),
            "error must name the bad reference verbatim: {msg}"
        );
        assert!(
            !msg.contains("Add `import"),
            "error must not suggest the unsatisfiable import fix: {msg}"
        );
    }

    /// Signature-drift FAIL hint (friction sweep, requests 2038/2113/2145):
    /// the drift rejection must point at the common `set_option` trigger
    /// and at FILESPEC's heartbeat-placement section.
    #[test]
    fn evaluate_node_observation_signature_drift_names_set_option_trigger() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );

        let observation = observation_from_disk_for_test(repo, "A");
        let evaluated = evaluate_node_observation(repo, &observation, Some("baselinehash0000"));
        assert!(
            evaluated
                .errors
                .iter()
                .any(|e| e.contains("Declaration signature changed")),
            "mismatched baseline hash must report signature drift: {:?}",
            evaluated.errors
        );
        assert!(
            evaluated.errors.iter().any(|e| e.contains("set_option")
                && e.contains("heartbeat-placement")),
            "drift hint must name the set_option trigger and FILESPEC's heartbeat-placement section: {:?}",
            evaluated.errors
        );
    }

    /// Permissive mode is the v2 hash WITHOUT the strict closure check.
    /// A noderef that isn't in the import closure should still yield a
    /// non-empty fingerprint — the user wants the citation-driven reopen
    /// semantics during rollout even when the corpus isn't fully
    /// migrated to `\noderef` discipline.
    #[test]
    fn soundness_fingerprint_v2_permissive_accepts_noderef_outside_closure() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        // Strict mode rejects (returns empty).
        assert_eq!(soundness_fingerprint_v2(repo, "A"), "");
        // Permissive mode accepts (non-empty `sound-v2:…`).
        let fp = soundness_fingerprint_v2_permissive(repo, "A");
        assert!(
            !fp.is_empty(),
            "permissive should not bail on out-of-closure noderef"
        );
        assert!(
            fp.starts_with("sound-v2:"),
            "permissive must still use sound-v2 prefix"
        );
    }

    /// When both v2-strict and v2-permissive successfully hash the same
    /// node, the resulting hashes must be IDENTICAL. The schema_tag string
    /// is intentionally NOT part of the hash payload — otherwise switching
    /// `TRELLIS_SOUNDNESS_FINGERPRINT_MODE` between strict and permissive
    /// would invalidate every existing approval that was already satisfying
    /// the strict rule.
    #[test]
    fn soundness_fingerprint_v2_strict_and_permissive_agree_when_both_succeed() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // Node A cites B; B IS in A's import closure -> strict accepts.
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        let strict = soundness_fingerprint_v2(repo, "A");
        let permissive = soundness_fingerprint_v2_permissive(repo, "A");
        assert!(!strict.is_empty(), "strict must succeed on in-closure ref");
        assert!(
            !permissive.is_empty(),
            "permissive must succeed on in-closure ref"
        );
        assert_eq!(
            strict, permissive,
            "strict and permissive hashes must agree when both succeed; \
             otherwise mode switches reopen approvals unnecessarily"
        );
    }

    /// Permissive mode must still REOPEN when a `\noderef`-cited node's
    /// `.tex` statement changes — that's the whole reopen-on-citation
    /// semantics the rollout is enabling.
    #[test]
    fn soundness_fingerprint_v2_permissive_reopens_on_cited_statement_change() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // A imports B (in closure), cites B via \noderef.
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B v1\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );
        let before = soundness_fingerprint_v2_permissive(repo, "A");
        assert!(!before.is_empty());

        // Mutate B's statement only (keep proof unchanged).
        write_node(
            repo,
            "B",
            "\\begin{theorem}B v2 (changed statement)\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );
        let after = soundness_fingerprint_v2_permissive(repo, "A");
        assert_ne!(
            before, after,
            "permissive hash MUST reopen on a cited node's statement change"
        );
    }

    /// Conversely, permissive mode should NOT reopen on a non-cited
    /// import's change (the v1 bug — adding a Lean-closed helper to the
    /// import set used to invalidate the parent's soundness baseline).
    #[test]
    fn soundness_fingerprint_v2_permissive_stable_under_non_cited_import_change() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // A cites only B; imports B and C.
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\nimport Tablet.C\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );
        write_node(
            repo,
            "C",
            "\\begin{theorem}C v1\\end{theorem}\n\\begin{proof}C proof v1.\\end{proof}\n",
            "-- [TABLET NODE: C]\nimport Tablet.Preamble\n\ntheorem C : True := sorry\n",
        );
        let before = soundness_fingerprint_v2_permissive(repo, "A");

        // Mutate C (statement AND proof) — C is imported by A but not
        // cited via \noderef in A's proof. A's fingerprint must not change.
        write_node(
            repo,
            "C",
            "\\begin{theorem}C v2\\end{theorem}\n\\begin{proof}C proof v2 rewritten.\\end{proof}\n",
            "-- [TABLET NODE: C]\nimport Tablet.Preamble\n\ntheorem C : True := sorry\n",
        );
        let after = soundness_fingerprint_v2_permissive(repo, "A");
        assert_eq!(
            before, after,
            "permissive hash must NOT reopen on a non-cited import's change"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_falls_back_to_lean_for_empty_tex_definition_noderef() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 1\n",
        );

        let before = soundness_fingerprint_v2(repo, "A");
        assert!(
            before.starts_with("sound-v2:"),
            "empty-tex definition noderef must still produce a strict fingerprint"
        );

        write_node(
            repo,
            "B",
            "",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 2\n",
        );
        let after = soundness_fingerprint_v2(repo, "A");
        assert_ne!(
            before, after,
            "changing the cited definition's Lean source must reopen the consumer"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_rejects_empty_own_tex() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := sorry\n",
        );

        assert_eq!(
            soundness_fingerprint_v2(repo, "A"),
            "",
            "the proof node's own .tex must be nonempty"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_context_rejects_empty_tex_proof_kind_noderef() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 1\n",
        );
        let node_kinds = BTreeMap::from([(NodeId::from("B"), NodeKind::Proof)]);

        let parts = soundness_fingerprint_v2_parts_impl_with_context(
            repo,
            "A",
            /* permissive = */ false,
            &node_kinds,
            &BTreeMap::new(),
        );
        assert_eq!(
            parts,
            SoundFingerprintParts::default(),
            "state Proof kind must reject empty .tex even when Lean looks definition-like"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_rejects_empty_tex_proof_bearing_noderef() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        assert_eq!(
            soundness_fingerprint_v2(repo, "A"),
            "",
            "proof-bearing cited nodes still require a nonempty .tex statement"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_rejects_empty_tex_axiom_noderef() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\naxiom B : True\n",
        );

        assert_eq!(
            soundness_fingerprint_v2(repo, "A"),
            "",
            "empty-tex axiom noderefs must not use the Lean fallback"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_falls_back_to_lean_for_empty_tex_constant_noderef() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\nconstant B : Nat\n",
        );

        assert!(
            soundness_fingerprint_v2(repo, "A").starts_with("sound-v2:"),
            "empty-tex constant noderef must use the Lean fallback"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_nonempty_definition_tex_wins_over_lean_change() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{definition}B is a fixed natural.\\end{definition}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 1\n",
        );
        let before = soundness_fingerprint_v2(repo, "A");
        assert!(!before.is_empty());

        write_node(
            repo,
            "B",
            "\\begin{definition}B is a fixed natural.\\end{definition}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 2\n",
        );
        assert_eq!(
            soundness_fingerprint_v2(repo, "A"),
            before,
            "nonempty definition TeX remains the sound dependency source"
        );
    }

    /// Schema-bridge rescue: when `approved` holds the LEGACY (pre-40e678b)
    /// payload hash of the node's current content (i.e. the worker
    /// acceptance subprocess was upgraded mid-run and rewrote `live` to
    /// the new format, but `approved` was set under the prior payload),
    /// the migration must re-bless `approved` under the new format.
    /// Without this rescue, nodes can end up Pass-status with `live` in
    /// the new format and `approved` still in the old format, leaving the
    /// two baselines permanently mismatched.
    #[test]
    fn soundness_fingerprint_v2_migration_bridges_legacy_approved_on_unchanged_content() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        // Simulate the mid-run binary swap: `live` was already rewritten
        // to the new (schema_tag-free) payload by a fresh kernel-CLI
        // subprocess, but `approved` is the legacy v2-permissive hash.
        let new_fp = soundness_fingerprint_v2(repo, "A");
        let legacy_approved = legacy_v2_payload_hash(repo, "A", "schema:sound-v2-permissive");
        assert!(!new_fp.is_empty());
        assert!(!legacy_approved.is_empty());
        assert_ne!(
            new_fp, legacy_approved,
            "legacy and new payloads must differ"
        );

        let mut state = crate::ProtocolState::default();
        let node = NodeId::from("A");
        state
            .live
            .sound_current_fingerprints
            .insert(node.clone(), new_fp.clone());
        state
            .committed
            .sound_current_fingerprints
            .insert(node.clone(), new_fp.clone());
        state
            .sound_approved_fingerprints
            .insert(node.clone(), legacy_approved.clone());

        // Sanity: pre-migration not aligned (this is the production
        // residue we're rescuing).
        assert_ne!(
            state.live.sound_current_fingerprints.get(&node),
            state.sound_approved_fingerprints.get(&node)
        );

        let changed = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Strict,
        )
        .unwrap();
        assert!(changed, "bridge must mutate state");
        assert_eq!(
            state.live.sound_current_fingerprints.get(&node),
            Some(&new_fp),
            "live stays at new format"
        );
        assert_eq!(
            state.sound_approved_fingerprints.get(&node),
            Some(&new_fp),
            "approved must be re-blessed to the new format (bridge)"
        );
    }

    /// Counter-test: if content HAS drifted since approval (so neither
    /// legacy hash variant matches stored approved), the migration must
    /// NOT bridge. The verdict needs to be re-issued by the verifier.
    #[test]
    fn soundness_fingerprint_v2_migration_does_not_bridge_real_content_drift() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A revised\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        let new_fp = soundness_fingerprint_v2(repo, "A");
        // Approved was blessed for some OTHER content; doesn't match
        // either legacy variant on current content.
        let bogus_approved = "sound-v2:deadbeef".to_string();

        let mut state = crate::ProtocolState::default();
        let node = NodeId::from("A");
        state
            .live
            .sound_current_fingerprints
            .insert(node.clone(), new_fp.clone());
        state
            .sound_approved_fingerprints
            .insert(node.clone(), bogus_approved.clone());

        let _ = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Strict,
        )
        .unwrap();
        assert_eq!(
            state.sound_approved_fingerprints.get(&node),
            Some(&bogus_approved),
            "approved must NOT be rewritten when content has genuinely drifted"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_migration_repins_only_aligned_baselines() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        let old = soundness_fingerprint_v1(repo, "A");
        let fresh = soundness_fingerprint_v2(repo, "A");
        assert_ne!(old, fresh);

        let mut state = crate::ProtocolState::default();
        let node = NodeId::from("A");
        state
            .live
            .sound_current_fingerprints
            .insert(node.clone(), old.clone());
        state
            .committed
            .sound_current_fingerprints
            .insert(node.clone(), old.clone());
        state
            .sound_approved_fingerprints
            .insert(node.clone(), old.clone());
        state
            .last_clean_live
            .sound_current_fingerprints
            .insert(node.clone(), old.clone());
        state
            .last_clean_sound_approved_fingerprints
            .insert(node.clone(), old);

        assert!(migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Strict
        )
        .unwrap());
        assert_eq!(
            state.live.sound_current_fingerprints.get(&node),
            Some(&fresh)
        );
        assert_eq!(
            state.committed.sound_current_fingerprints.get(&node),
            Some(&fresh)
        );
        assert_eq!(state.sound_approved_fingerprints.get(&node), Some(&fresh));
        assert_eq!(
            state.last_clean_live.sound_current_fingerprints.get(&node),
            Some(&fresh)
        );
        assert_eq!(
            state.last_clean_sound_approved_fingerprints.get(&node),
            Some(&fresh)
        );

        let drifted = NodeId::from("Drifted");
        state
            .live
            .sound_current_fingerprints
            .insert(drifted.clone(), "old-live".to_string());
        state
            .sound_approved_fingerprints
            .insert(drifted.clone(), "old-approved".to_string());
        let _ = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Strict,
        )
        .unwrap();
        assert_eq!(
            state.sound_approved_fingerprints.get(&drifted),
            Some(&"old-approved".to_string()),
            "drifted approved baselines must not be blessed by migration"
        );
    }

    #[test]
    fn soundness_fingerprint_v2_migration_backfills_empty_verifier_pass_assessment() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 1\n",
        );

        let node = NodeId::from("A");
        let fresh_parts =
            soundness_fingerprint_v2_parts_impl(repo, &node, /* permissive = */ false);
        let fresh = fresh_parts.combined_sound_fp.clone();
        assert!(!fresh.is_empty());

        let mut state = crate::ProtocolState::default();
        state
            .sound_approved_fingerprints
            .insert(node.clone(), String::new());
        state.sound_assessments.insert(
            node.clone(),
            trellis_kernel::SoundAssessment {
                status: trellis_kernel::SoundAssessmentStatus::VerifierPass,
                origin: trellis_kernel::AssessmentOrigin::VerifierPanel,
                fingerprints: SoundFingerprintParts::default(),
                lane_votes: BTreeMap::new(),
                reviewer_action_id: None,
            },
        );

        let changed = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Strict,
        )
        .unwrap();
        assert!(changed, "migration must backfill the newly computable pass");
        assert_eq!(state.sound_approved_fingerprints.get(&node), Some(&fresh));
        assert_eq!(
            state.live.sound_current_fingerprints.get(&node),
            Some(&fresh)
        );
        assert_eq!(
            state.live.sound_current_fingerprint_parts.get(&node),
            Some(&fresh_parts)
        );
        assert_eq!(
            state.committed.sound_current_fingerprints.get(&node),
            Some(&fresh)
        );
        assert_eq!(
            state.committed.sound_current_fingerprint_parts.get(&node),
            Some(&fresh_parts)
        );
        assert_eq!(
            state.sound_assessments.get(&node).map(|a| &a.fingerprints),
            Some(&fresh_parts)
        );
    }

    #[test]
    fn soundness_fingerprint_v2_migration_reopens_uncomputable_empty_pass() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        let node = NodeId::from("A");
        assert!(
            soundness_fingerprint_v2(repo, &node).is_empty(),
            "the current fingerprint is uncomputable because the cited theorem has empty TeX"
        );

        let mut state = crate::ProtocolState::default();
        state
            .sound_status
            .insert(node.clone(), trellis_kernel::SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert(node.clone(), "sound-v2:stale".to_string());
        state
            .live
            .sound_current_fingerprints
            .insert(node.clone(), "sound-v2:stale".to_string());
        state
            .committed
            .sound_current_fingerprints
            .insert(node.clone(), "sound-v2:stale".to_string());
        state
            .last_clean_sound_status
            .insert(node.clone(), trellis_kernel::SoundStatus::Pass);
        state.sound_assessments.insert(
            node.clone(),
            trellis_kernel::SoundAssessment {
                status: trellis_kernel::SoundAssessmentStatus::VerifierPass,
                origin: trellis_kernel::AssessmentOrigin::VerifierPanel,
                fingerprints: SoundFingerprintParts::default(),
                lane_votes: BTreeMap::new(),
                reviewer_action_id: None,
            },
        );

        let changed = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Strict,
        )
        .unwrap();
        assert!(changed, "stale empty verifier pass must be reopened");
        assert!(!state.sound_assessments.contains_key(&node));
        assert!(!state.sound_status.contains_key(&node));
        assert!(!state.sound_approved_fingerprints.contains_key(&node));
        assert!(!state.live.sound_current_fingerprints.contains_key(&node));
        assert!(!state
            .committed
            .sound_current_fingerprints
            .contains_key(&node));
        assert!(!state.last_clean_sound_status.contains_key(&node));
    }

    #[test]
    fn soundness_fingerprint_v2_migration_does_not_rebless_drifted_empty_pass() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A changed\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 1\n",
        );

        let node = NodeId::from("A");
        let fresh_parts =
            soundness_fingerprint_v2_parts_impl(repo, &node, /* permissive = */ false);
        let fresh = fresh_parts.combined_sound_fp.clone();
        assert!(!fresh.is_empty());

        let mut state = crate::ProtocolState::default();
        let old_approved = "sound-v2:old-approved".to_string();
        state
            .sound_status
            .insert(node.clone(), trellis_kernel::SoundStatus::Pass);
        state
            .sound_approved_fingerprints
            .insert(node.clone(), old_approved.clone());
        state.sound_assessments.insert(
            node.clone(),
            trellis_kernel::SoundAssessment {
                status: trellis_kernel::SoundAssessmentStatus::VerifierPass,
                origin: trellis_kernel::AssessmentOrigin::VerifierPanel,
                fingerprints: SoundFingerprintParts::default(),
                lane_votes: BTreeMap::new(),
                reviewer_action_id: None,
            },
        );

        let changed = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Strict,
        )
        .unwrap();
        assert!(changed, "current fingerprints should still be repaired");
        assert_eq!(
            state.sound_approved_fingerprints.get(&node),
            Some(&old_approved),
            "nonempty approved baselines must not be overwritten"
        );
        assert_eq!(
            state.sound_assessments.get(&node).map(|a| &a.fingerprints),
            Some(&SoundFingerprintParts::default()),
            "drifted empty-pass assessments must not be pinned to current text"
        );
        assert_eq!(
            state.live.sound_current_fingerprints.get(&node),
            Some(&fresh)
        );
        assert_eq!(
            state.committed.sound_current_fingerprints.get(&node),
            Some(&fresh)
        );
    }

    #[test]
    fn soundness_fingerprint_v2_migration_does_not_backfill_empty_fail_assessments() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "FailNode",
            "\\begin{theorem}FailNode\\end{theorem}\n\\begin{proof}Use \\noderef{DefNode}.\\end{proof}\n",
            "-- [TABLET NODE: FailNode]\nimport Tablet.Preamble\nimport Tablet.DefNode\n\ntheorem FailNode : True := sorry\n",
        );
        write_node(
            repo,
            "PinnedNode",
            "\\begin{theorem}PinnedNode\\end{theorem}\n\\begin{proof}Use \\noderef{DefNode}.\\end{proof}\n",
            "-- [TABLET NODE: PinnedNode]\nimport Tablet.Preamble\nimport Tablet.DefNode\n\ntheorem PinnedNode : True := sorry\n",
        );
        write_node(
            repo,
            "DefNode",
            "",
            "-- [TABLET NODE: DefNode]\nimport Tablet.Preamble\n\ndef DefNode : Nat := 1\n",
        );

        let fail_node = NodeId::from("FailNode");
        let pinned_node = NodeId::from("PinnedNode");
        let mut state = crate::ProtocolState::default();
        for (node, status) in [
            (
                fail_node.clone(),
                trellis_kernel::SoundAssessmentStatus::VerifierFail,
            ),
            (
                pinned_node.clone(),
                trellis_kernel::SoundAssessmentStatus::ReviewerPinnedFail,
            ),
        ] {
            state
                .sound_approved_fingerprints
                .insert(node.clone(), String::new());
            state.sound_assessments.insert(
                node,
                trellis_kernel::SoundAssessment {
                    status,
                    origin: trellis_kernel::AssessmentOrigin::VerifierPanel,
                    fingerprints: SoundFingerprintParts::default(),
                    lane_votes: BTreeMap::new(),
                    reviewer_action_id: None,
                },
            );
        }

        let changed = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Strict,
        )
        .unwrap();
        assert!(
            !changed,
            "migration must not blindly bless stale fail assessments"
        );
        for node in [fail_node, pinned_node] {
            assert_eq!(
                state.sound_approved_fingerprints.get(&node),
                Some(&String::new())
            );
            assert_eq!(
                state.sound_assessments.get(&node).map(|a| &a.fingerprints),
                Some(&SoundFingerprintParts::default())
            );
        }
    }

    /// Regression for the mirror-approved poisoning bug. A node whose
    /// content drifted since the clean checkpoint (mirror current ==
    /// mirror approved == clean-text hash C, but live == on-disk hash E
    /// ≠ C) must NOT have its mirror-approved rewritten to the
    /// current-disk hash. Before the content-undrift guard, the
    /// `aligned_last_clean_nodes` membership alone (mirror internally
    /// consistent) let the write fire and poisoned mirror-approved with
    /// post-clean text. This test FAILS without the guard and PASSES
    /// with it.
    #[test]
    fn soundness_fingerprint_v2_migration_does_not_poison_drifted_last_clean_mirror() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A current\\end{theorem}\n\\begin{proof}Use \\noderef{B}.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := sorry\n",
        );
        write_node(
            repo,
            "B",
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}B proof.\\end{proof}\n",
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := sorry\n",
        );

        let node = NodeId::from("A");
        // C: the clean-checkpoint fingerprint the mirror was captured at.
        // The migration recomputes the CURRENT disk text and only rewrites
        // when nothing drifted — emulate the clean-checkpoint hash with a
        // stand-in distinct from what the current disk text would produce.
        let clean = "sound-v2:cleancheckpointhash".to_string();
        // E: live drifted to some other (also non-current) value, so live
        // != mirror-current → content has drifted since clean.
        let live_drifted = "sound-v2:livedriftedhash".to_string();
        let current_disk = soundness_fingerprint_v2_permissive(repo, &node);
        assert!(!current_disk.is_empty());
        assert_ne!(current_disk, clean);
        assert_ne!(current_disk, live_drifted);

        let mut state = crate::ProtocolState::default();
        state
            .live
            .sound_current_fingerprints
            .insert(node.clone(), live_drifted.clone());
        state
            .last_clean_live
            .sound_current_fingerprints
            .insert(node.clone(), clean.clone());
        // Mirror internally consistent (current == approved == C): this is
        // every healthy mirror entry, so aligned_last_clean_nodes contains
        // the node, but content HAS drifted (mirror-current C != live E).
        state
            .last_clean_sound_approved_fingerprints
            .insert(node.clone(), clean.clone());

        let _ = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Permissive,
        )
        .unwrap();

        assert_eq!(
            state.last_clean_sound_approved_fingerprints.get(&node),
            Some(&clean),
            "mirror-approved must NOT be rewritten when content drifted since the clean checkpoint"
        );
    }

    /// Mirror self-heal: an already-poisoned checkpoint (status Pass, mirror
    /// current C, mirror approved X ≠ C) is corrupted by construction — a
    /// Pass mirror entry is captured with current == approved by the
    /// cleanliness invariant. The migration restores mirror-approved from
    /// mirror-current on every load.
    #[test]
    fn soundness_fingerprint_v2_migration_self_heals_poisoned_pass_mirror() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}A proof.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := sorry\n",
        );

        let node = NodeId::from("A");
        let clean = "sound-v2:cleantexthash".to_string();
        let poison = "sound-v2:poisonedapprovedhash".to_string();
        assert_ne!(clean, poison);

        let mut state = crate::ProtocolState::default();
        state
            .last_clean_sound_status
            .insert(node.clone(), trellis_kernel::SoundStatus::Pass);
        state
            .last_clean_live
            .sound_current_fingerprints
            .insert(node.clone(), clean.clone());
        state
            .last_clean_sound_approved_fingerprints
            .insert(node.clone(), poison.clone());

        let changed = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Permissive,
        )
        .unwrap();
        assert!(changed, "self-heal must mutate state");
        assert_eq!(
            state.last_clean_sound_approved_fingerprints.get(&node),
            Some(&clean),
            "poisoned Pass mirror must heal mirror-approved back to mirror-current"
        );
    }

    /// End-to-end: a poisoned mirror that the migration heals, followed by
    /// `apply_last_clean_reset`, lands the node at Sound Pass with no
    /// spurious Soundness blocker. Without the heal the restored mirrors
    /// would disagree → `legacy_sound_assessment` returns None →
    /// FreshUnknown → a Soundness blocker on a supposedly-clean restore.
    #[test]
    fn poisoned_mirror_heals_then_last_clean_reset_reads_sound_pass() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "A",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}A proof.\\end{proof}\n",
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := sorry\n",
        );

        let node = NodeId::from("A");
        let clean = "sound-v2:cleantexthash".to_string();
        let poison = "sound-v2:poisonedapprovedhash".to_string();

        let mut state = crate::ProtocolState::default();
        // Populate the LastClean mirrors so `apply_last_clean_reset` takes
        // the full-restore path: the node is present/open/proof at the
        // clean checkpoint and sound-Pass, with a poisoned mirror-approved.
        state.last_clean_live.present_nodes.insert(node.clone());
        state.last_clean_live.open_nodes.insert(node.clone());
        state.last_clean_proof_nodes.insert(node.clone());
        state
            .last_clean_live
            .sound_current_fingerprints
            .insert(node.clone(), clean.clone());
        state
            .last_clean_sound_status
            .insert(node.clone(), trellis_kernel::SoundStatus::Pass);
        state
            .last_clean_sound_approved_fingerprints
            .insert(node.clone(), poison.clone());
        state.last_clean_verifier_mirror_ready = true;
        state.last_clean_local_closure_mirror_ready = true;

        // Heal the poisoned mirror (runs on every load).
        let _ = migrate_soundness_fingerprint_schema(
            &mut state,
            repo,
            SoundnessFingerprintMode::V2Permissive,
        )
        .unwrap();
        assert_eq!(
            state.last_clean_sound_approved_fingerprints.get(&node),
            Some(&clean),
            "precondition: mirror healed before reset"
        );

        assert_eq!(state.apply_last_clean_reset(), Ok(true));

        // legacy_sound_assessment sees current == approved == C → Pass.
        assert!(state.needs_sound(&node));
        assert_eq!(
            state.current_sound_state(&node),
            trellis_kernel::CurrentCheckState::Pass,
            "post-reset Sound state must read Pass, not a spurious Unknown blocker"
        );
    }

    #[test]
    fn guard_skips_when_no_covering_nodes() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        let errors = paper_target_corr_reopen_guard_errors(
            repo,
            &BTreeSet::new(),
            &BTreeMap::new(),
            WorkerProofDeltaMode::Local,
        )
        .unwrap();
        assert!(errors.is_empty());
    }

    #[test]
    fn guard_requires_explicit_scope_in_coarse_restructure_mode() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "Main",
            r#"\begin{theorem}stmt\end{theorem}\begin{proof}p\end{proof}"#,
            "theorem Main : True := sorry",
        );
        let baseline = cf("stale", "stale", "stale", &[]).to_storage_string();
        let approved = BTreeMap::from([(NodeId::from("Main"), baseline)]);
        let covering: BTreeSet<NodeId> = [NodeId::from("Main")].into_iter().collect();
        let errors = paper_target_corr_reopen_guard_errors(
            repo,
            &covering,
            &approved,
            WorkerProofDeltaMode::CoarseRestructure,
        )
        .unwrap();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("not explicitly authorized"),
            "msg={:?}",
            errors[0]
        );

        let report = paper_target_corr_reopen_guard_report_with_scope(
            repo,
            &covering,
            &approved,
            WorkerProofDeltaMode::CoarseRestructure,
            &BTreeSet::from([NodeId::from("Main")]),
        )
        .unwrap();
        assert!(report.errors.is_empty());
        assert_eq!(
            report.reopened_nodes,
            BTreeSet::from([NodeId::from("Main")])
        );
    }

    #[test]
    fn guard_skips_when_no_baseline_yet() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "Main",
            r#"\begin{theorem}stmt\end{theorem}\begin{proof}p\end{proof}"#,
            "theorem Main : True := sorry",
        );
        // No approved fingerprint → no guard (first corr check will establish baseline).
        let errors = paper_target_corr_reopen_guard_errors(
            repo,
            &[NodeId::from("Main")].into_iter().collect(),
            &BTreeMap::new(),
            WorkerProofDeltaMode::Local,
        )
        .unwrap();
        assert!(errors.is_empty());
    }

    #[test]
    fn guard_fires_on_own_tex_change_with_prose_error() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "Main",
            r#"\begin{theorem}changed statement\end{theorem}\begin{proof}p\end{proof}"#,
            "theorem Main : True := sorry",
        );
        // Build a fake approved fingerprint that differs from the current.
        let stale = cf("different_own_hash", "any_lean", "", &[]).to_storage_string();
        let approved = BTreeMap::from([(NodeId::from("Main"), stale)]);
        let errors = paper_target_corr_reopen_guard_errors(
            repo,
            &[NodeId::from("Main")].into_iter().collect(),
            &approved,
            WorkerProofDeltaMode::Local,
        )
        .unwrap();
        assert_eq!(errors.len(), 1);
        let msg = &errors[0];
        assert!(
            msg.contains("Protected approved-target/protected-closure node `Main`"),
            "msg={msg:?}"
        );
        assert!(
            msg.contains("would have its correspondence reopened"),
            "msg={msg:?}"
        );
        assert!(msg.contains("coarse_restructure"), "msg={msg:?}");
    }
    // -----------------------------------------------------------------------

    fn set<T: From<String> + Ord>(items: &[&str]) -> BTreeSet<T> {
        items
            .iter()
            .map(|item| T::from((*item).to_string()))
            .collect()
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn scoped_tablet_exempts_extraction_model_impact_region_from_ordinary_shape() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path();
        write_stub_check_script(repo);
        write(&repo.join("Tablet/Preamble.lean"), "");
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.PV.Generated.Model\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/PV.Generated.Model.lean"),
            "import Tablet.Preamble\n\nnamespace PV.Generated\n\ndef Model.value : Nat := 0\n\nend PV.Generated\n",
        );
        write(&repo.join("Tablet/PV.Generated.Model.tex"), "");
        let before_snapshot = snapshot_tablet_dir(repo);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.PV.Generated.Model\n\ntheorem A : True := by\n  trivial\n-- ordinary target edit\n",
        );

        let scope = theorem_target_edit_scope_step_result(
            repo,
            "A",
            &before_snapshot,
            &set(&["A"]),
            &BTreeSet::new(),
        );
        assert!(scope.ok, "{:?}", scope.errors);
        assert!(
            scope
                .allowed_nodes
                .contains(&NodeId::from("PV.Generated.Model")),
            "target impact region should include the imported generated model: {:?}",
            scope.allowed_nodes
        );

        let ordinary = scoped_tablet_step_result(repo, &[], &scope.allowed_nodes, false).unwrap();
        assert!(
            !ordinary.ok,
            "without a PV role, the dotted/empty generated node must still fail ordinary validation"
        );
        assert!(
            ordinary
                .errors
                .iter()
                .any(|err| err.contains("Invalid node name: PV.Generated.Model")
                    || err.contains("PV.Generated.Model: marker says")
                    || err.contains("PV.Generated.Model: .tex format errors")),
            "expected ordinary FILESPEC-shape rejection for the generated node, got {:?}",
            ordinary.errors
        );

        let role_map = BTreeMap::from([(
            NodeId::from("PV.Generated.Model"),
            trellis_kernel::PvRole::ExtractionModel,
        )]);
        let pv = scoped_tablet_step_result_with_roles(
            repo,
            &[],
            &scope.allowed_nodes,
            false,
            &BTreeMap::new(),
            &role_map,
            None,
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(
            pv.ok,
            "ExtractionModel role must suppress ordinary node-shape FILESPEC errors: {:?}",
            pv.errors
        );
    }

    #[test]
    fn scoped_tablet_allows_staged_assumption_only_for_authoring_node() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path();
        write_stub_check_script(repo);
        write(&repo.join("Tablet/Preamble.lean"), "");
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/Assumptions.lean"),
            &format!(
                "{}\n{}slice_len_le_isize_max\naxiom dec2flt.slice_len_le_isize_max : True\n{}slice_len_le_isize_max\n",
                trellis_kernel::assumptions_registry::empty_assumptions_lean_scaffold(),
                trellis_kernel::assumptions_registry::LEAN_ASSUMPTION_BEGIN_PREFIX,
                trellis_kernel::assumptions_registry::LEAN_ASSUMPTION_END_PREFIX,
            ),
        );
        write(
            &repo.join("Tablet/Assumptions.tex"),
            &format!(
                "{}\n{}slice_len_le_isize_max\n\\begin{{definition}}Every Rust slice length is at most isize::MAX.\\end{{definition}}\n{}slice_len_le_isize_max\n",
                trellis_kernel::assumptions_registry::empty_assumptions_tex_scaffold(),
                trellis_kernel::assumptions_registry::TEX_ASSUMPTION_BEGIN_PREFIX,
                trellis_kernel::assumptions_registry::TEX_ASSUMPTION_END_PREFIX,
            ),
        );
        let allowed = set::<NodeId>(&["Assumptions"]);
        let assumptions = NodeId::from("Assumptions");

        let ordinary = scoped_tablet_step_result(repo, &[], &allowed, false).unwrap();
        assert!(
            !ordinary.ok,
            "ordinary scoped-tablet validation must reject the staged axiom shape"
        );
        assert!(
            ordinary
                .errors
                .iter()
                .any(|err| err.contains("Forbidden keywords") && err.contains("axiom")),
            "expected ordinary forbidden-axiom rejection, got {:?}",
            ordinary.errors
        );

        let authoring = scoped_tablet_step_result_with_assumption_authoring(
            repo,
            &[],
            &allowed,
            false,
            &BTreeMap::new(),
            Some(&assumptions),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(
            authoring.ok,
            "assumption-authoring scoped-tablet validation must accept staged Assumptions: {:?}",
            authoring.errors
        );

        let role_map = BTreeMap::from([(
            assumptions.clone(),
            trellis_kernel::PvRole::UnderModelAssumptions,
        )]);
        let under_model = scoped_tablet_step_result_with_roles(
            repo,
            &[],
            &allowed,
            false,
            &BTreeMap::new(),
            &role_map,
            None,
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(
            under_model.ok,
            "under-model Assumptions role must use the assumptions-specific contract outside authoring: {:?}",
            under_model.errors
        );
    }

    #[test]
    fn assumption_authoring_target_scope_rejects_collateral_edits() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path();
        write(&repo.join("Tablet/Preamble.lean"), "");
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/Assumptions.lean"),
            &trellis_kernel::assumptions_registry::empty_assumptions_lean_scaffold(),
        );
        write(
            &repo.join("Tablet/Assumptions.tex"),
            &trellis_kernel::assumptions_registry::empty_assumptions_tex_scaffold(),
        );
        write(
            &repo.join("Tablet/Consumer.lean"),
            "import Tablet.Assumptions\n-- [TABLET NODE: Consumer]\ntheorem Consumer : True := by\n-- BODY\n  trivial\n",
        );
        write(
            &repo.join("Tablet/Consumer.tex"),
            "\\begin{theorem}Consumer.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(repo);
        write(
            &repo.join("Tablet/Assumptions.lean"),
            &format!(
                "{}\n{}slice_len_le_isize_max\naxiom dec2flt.slice_len_le_isize_max : True\n{}slice_len_le_isize_max\n",
                trellis_kernel::assumptions_registry::empty_assumptions_lean_scaffold(),
                trellis_kernel::assumptions_registry::LEAN_ASSUMPTION_BEGIN_PREFIX,
                trellis_kernel::assumptions_registry::LEAN_ASSUMPTION_END_PREFIX,
            ),
        );
        write(
            &repo.join("Tablet/Consumer.tex"),
            "\\begin{theorem}Consumer edited.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let assumptions = NodeId::from("Assumptions");

        let result = theorem_target_edit_scope_step_result_with_assumption_authoring(
            repo,
            "Assumptions",
            &before_snapshot,
            &set(&["Assumptions", "Consumer"]),
            Some(&assumptions),
            &BTreeSet::new(),
        );

        assert!(!result.ok, "{:?}", result.errors);
        assert!(
            result.errors.iter().any(|err| err.contains("Consumer")),
            "collateral Consumer edit must be rejected, got {:?}",
            result.errors
        );
    }

    #[test]
    fn assumptions_named_node_uses_ordinary_corr_without_pv_role() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path();
        write(&repo.join("Tablet/Preamble.lean"), "");
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/Assumptions.lean"),
            "-- [TABLET NODE: Assumptions]\nimport Tablet.Preamble\n\ntheorem OrdinaryAssumptions : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/Assumptions.tex"),
            "\\begin{theorem}Ordinary assumptions node.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let nodes: BTreeSet<NodeId> = set(&["Assumptions"]);

        let ordinary = observe_correspondence_fingerprints(repo, &nodes).unwrap();
        assert!(
            !ordinary
                .get(&NodeId::from("Assumptions"))
                .unwrap()
                .trim()
                .is_empty(),
            "a non-PV node named Assumptions must be fingerprinted as an ordinary node"
        );

        let pv =
            observe_correspondence_fingerprints_with_under_model_assumptions(repo, &nodes, &nodes)
                .unwrap();
        assert_eq!(
            pv.get(&NodeId::from("Assumptions")).unwrap(),
            "",
            "the under-model assumptions fingerprint is selected only when the PV role is supplied"
        );
    }

    #[test]
    fn under_model_assumptions_scaffold_template_has_empty_corr_fingerprint() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path();
        write(&repo.join("Tablet/Preamble.lean"), "");
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/Assumptions.lean"),
            &trellis_kernel::assumptions_registry::empty_assumptions_lean_scaffold(),
        );
        write(
            &repo.join("Tablet/Assumptions.tex"),
            &trellis_kernel::assumptions_registry::empty_assumptions_tex_scaffold(),
        );
        let nodes: BTreeSet<NodeId> = set(&["Assumptions"]);

        let scaffold =
            observe_correspondence_fingerprints_with_under_model_assumptions(repo, &nodes, &nodes)
                .unwrap();
        assert_eq!(
            scaffold.get(&NodeId::from("Assumptions")).unwrap(),
            "",
            "template marker comments must not create a staged Assumptions fingerprint"
        );

        write(
            &repo.join("Tablet/Assumptions.lean"),
            &format!(
                "{}\n{}a1\naxiom dec2flt.slice_len_le_isize_max : True\n{}a1\n",
                trellis_kernel::assumptions_registry::empty_assumptions_lean_scaffold(),
                trellis_kernel::assumptions_registry::LEAN_ASSUMPTION_BEGIN_PREFIX,
                trellis_kernel::assumptions_registry::LEAN_ASSUMPTION_END_PREFIX,
            ),
        );
        write(
            &repo.join("Tablet/Assumptions.tex"),
            &format!(
                "{}\n{}a1\nEvery Rust slice length is at most isize::MAX.\n{}a1\n",
                trellis_kernel::assumptions_registry::empty_assumptions_tex_scaffold(),
                trellis_kernel::assumptions_registry::TEX_ASSUMPTION_BEGIN_PREFIX,
                trellis_kernel::assumptions_registry::TEX_ASSUMPTION_END_PREFIX,
            ),
        );

        let staged =
            observe_correspondence_fingerprints_with_under_model_assumptions(repo, &nodes, &nodes)
                .unwrap();
        assert!(
            !staged
                .get(&NodeId::from("Assumptions"))
                .unwrap()
                .trim()
                .is_empty(),
            "a real staged marker must produce an ordinary Assumptions fingerprint"
        );
    }

    #[test]
    fn proof_delta_treats_assumptions_as_ordinary_without_pv_role() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path();
        write_stub_check_script(repo);
        write(&repo.join("Tablet/Preamble.lean"), "");
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/Assumptions.lean"),
            "-- [TABLET NODE: Assumptions]\nimport Tablet.Preamble\n\ntheorem OrdinaryAssumptions : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/Assumptions.tex"),
            "\\begin{theorem}Ordinary assumptions node.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(repo);
        write(
            &repo.join("Tablet/Assumptions.lean"),
            "-- [TABLET NODE: Assumptions]\nimport Tablet.Preamble\n\naxiom ordinary_assumption : True\n",
        );

        let mut protected_semantic_change_nodes = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A", "Assumptions"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            "",
            WorkerProofDeltaMode::Restructure,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &set(&["Assumptions"]),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        assert!(!result.ok, "{:?}", result.errors);
        assert!(
            result
                .errors
                .iter()
                .any(|err| err.contains("Forbidden keywords")),
            "without the PV under-model role, Assumptions must use ordinary node validation: {:?}",
            result.errors
        );
    }

    fn snapshot_tablet_contents(repo: &Path) -> BTreeMap<String, String> {
        trellis_kernel::snapshot_tablet_file_contents(repo)
    }

    fn write_stub_check_script(repo: &Path) {
        let script = r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]

if cmd == "lean-compile-node":
    print(json.dumps({
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "print-axioms":
    node = sys.argv[2]
    print(json.dumps({
        "returncode": 0,
        "stdout": f"{node} does not depend on any axioms\\n",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "local-closure-axioms":
    # Patch B stub: clean local-closure probe (empty axiom set, empty
    # boundary/strict deps, no errors). Tests that want to exercise the
    # rejection paths override this stub with the dedicated
    # `write_local_closure_stub_check_script` variant.
    node = sys.argv[2]
    print(json.dumps({
        "request_id": 0,
        "node_name": node,
        "returncode": 0,
        "timed_out": False,
        "stdout": "",
        "stderr": "",
        "status": "ok",
        "kernel_axioms": [],
        "boundary_theorems": [],
        "strict_theorem_deps": [],
        "strict_definition_deps": [],
        "errors": [],
    }))
elif cmd == "lean-semantic-payloads":
    repo = Path(sys.argv[2])
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    payloads = {}
    for node in nodes:
        lean_path = repo / "Tablet" / f"{node}.lean"
        payload = ""
        for line in lean_path.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if stripped.startswith(("theorem ", "lemma ", "def ", "abbrev ", "instance ", "structure ", "inductive ", "class ")):
                payload = stripped
                break
        payloads[node] = {
            "ok": True,
            "payload": payload,
            "error": "",
        }
    print(json.dumps(payloads))
elif cmd == "sync-tablet-support":
    print(json.dumps({
        "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }))
elif cmd == "materialize-tablet-oleans":
    print(json.dumps({
        "returncode": 0,
        "stdout": "materialized",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "prepare-compiled-support":
    print(json.dumps({
        "returncode": 0,
        "stdout": "prepared",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
else:
    raise SystemExit(f"unexpected subcommand: {cmd}")
"#;
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }

    fn write_theorem_restructure_same_burst_orphan_fixture(repo: &Path) {
        write(&repo.join("Tablet/Preamble.lean"), "");
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{definition}B.\\end{definition}\n",
        );
    }

    fn delete_same_burst_orphan_b(repo: &Path) {
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        std::fs::remove_file(repo.join("Tablet/B.lean")).unwrap();
        std::fs::remove_file(repo.join("Tablet/B.tex")).unwrap();
    }

    #[test]
    fn theorem_restructure_scoped_tablet_ignores_declared_deleted_explicit_node() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path();
        write_stub_check_script(repo);
        write_theorem_restructure_same_burst_orphan_fixture(repo);
        let before_snapshot = snapshot_tablet_dir(repo);
        delete_same_burst_orphan_b(repo);

        let declared_deleted_nodes = set::<NodeId>(&["B"]);
        let initial_scope = set::<NodeId>(&["A", "B"]);
        let scope = theorem_target_edit_scope_step_result(
            repo,
            "A",
            &before_snapshot,
            &initial_scope,
            &declared_deleted_nodes,
        );
        assert!(scope.ok, "{:?}", scope.errors);
        assert!(
            scope.allowed_nodes.contains(&NodeId::from("B")),
            "the scoped-tablet explicit/previous set must still exercise the deletion filter"
        );

        let scoped = scoped_tablet_step_result_with_roles(
            repo,
            &[],
            &scope.allowed_nodes,
            false,
            &before_snapshot,
            &BTreeMap::new(),
            None,
            &declared_deleted_nodes,
        )
        .unwrap();
        assert!(scoped.ok, "{:?}", scoped.errors);
        assert!(
            !scoped.allowed_nodes.contains(&NodeId::from("B")),
            "declared deleted nodes should be removed only from scoped-tablet observation"
        );
    }

    #[test]
    fn theorem_restructure_scoped_tablet_rejects_undeclared_deleted_explicit_node() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path();
        write_stub_check_script(repo);
        write_theorem_restructure_same_burst_orphan_fixture(repo);
        let before_snapshot = snapshot_tablet_dir(repo);
        delete_same_burst_orphan_b(repo);

        let initial_scope = set::<NodeId>(&["A", "B"]);
        let scope = theorem_target_edit_scope_step_result(
            repo,
            "A",
            &before_snapshot,
            &initial_scope,
            &BTreeSet::new(),
        );
        assert!(scope.ok, "{:?}", scope.errors);

        let scoped = scoped_tablet_step_result_with_roles(
            repo,
            &[],
            &scope.allowed_nodes,
            false,
            &before_snapshot,
            &BTreeMap::new(),
            None,
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(!scoped.ok, "undeclared deleted B should remain observed");
        assert!(
            scoped.errors.iter().any(|err| err.contains("B")),
            "expected B-specific missing-node validation error, got {:?}",
            scoped.errors
        );
    }

    fn write_import_sensitive_stub_check_script(repo: &Path) {
        let script = r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]

def direct_imports(repo: Path, node: str):
    lean_path = repo / "Tablet" / f"{node}.lean"
    if not lean_path.exists():
        return []
    deps = []
    for line in lean_path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("import Tablet."):
            dep = stripped.removeprefix("import Tablet.").strip()
            if dep and dep != "Preamble":
                deps.append(dep)
    return deps

def olean_path(repo: Path, node: str) -> Path:
    return repo / ".lake" / "build" / "lib" / "lean" / "Tablet" / f"{node}.olean"

def materialize(repo: Path, nodes: list[str]):
    seen = set()
    order = []
    def visit(node: str):
        if not node or node in seen:
            return
        seen.add(node)
        for dep in direct_imports(repo, node):
            visit(dep)
        order.append(node)
    for node in nodes:
        visit(node)
    for node in order:
        out = olean_path(repo, node)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(f"olean:{node}\n", encoding="utf-8")
    return order

if cmd == "sync-tablet-support":
    print(json.dumps({
        "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }))
elif cmd == "materialize-tablet-oleans":
    repo = Path(sys.argv[2])
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    built = materialize(repo, nodes)
    print(json.dumps({
        "requested_nodes": nodes,
        "materialized_nodes": built,
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "lean-compile-node":
    node = sys.argv[2]
    repo = Path(sys.argv[3]) if len(sys.argv) > 3 else Path(".")
    for dep in direct_imports(repo, node):
        if not olean_path(repo, dep).exists():
            print(json.dumps({
                "returncode": 1,
                "stdout": "",
                "stderr": f"Tablet/{node}.lean:2:0: error: object file '{olean_path(repo, dep)}' of module Tablet.{dep} does not exist",
                "timed_out": False,
                "spawn_error": "",
            }))
            break
    else:
        print(json.dumps({
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "timed_out": False,
            "spawn_error": "",
        }))
elif cmd == "print-axioms":
    node = sys.argv[2]
    print(json.dumps({
        "returncode": 0,
        "stdout": f"{node} does not depend on any axioms\\n",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "local-closure-axioms":
    node = sys.argv[2]
    print(json.dumps({
        "request_id": 0,
        "node_name": node,
        "returncode": 0,
        "timed_out": False,
        "stdout": "",
        "stderr": "",
        "status": "ok",
        "kernel_axioms": [],
        "boundary_theorems": [],
        "strict_theorem_deps": [],
        "strict_definition_deps": [],
        "errors": [],
    }))
elif cmd == "lean-semantic-payloads":
    repo = Path(sys.argv[2])
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    payloads = {}
    for node in nodes:
        lean_path = repo / "Tablet" / f"{node}.lean"
        payload = ""
        for line in lean_path.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if stripped.startswith(("theorem ", "lemma ", "def ", "abbrev ", "instance ", "structure ", "inductive ", "class ")):
                payload = stripped
                break
        payloads[node] = {"ok": True, "payload": payload, "error": ""}
    print(json.dumps(payloads))
else:
    raise SystemExit(f"unexpected subcommand: {cmd}")
"#;
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }

    #[test]
    fn cleanup_preserving_allows_current_orphan_deletion() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{definition}B\\end{definition}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let before_tablet_contents = snapshot_tablet_contents(&repo);
        let baseline_declaration_hashes = BTreeMap::from([
            (
                NodeId::from("A"),
                declaration_hash(&read_text_if_exists(&repo.join("Tablet/A.lean")), "A"),
            ),
            (
                NodeId::from("B"),
                declaration_hash(&read_text_if_exists(&repo.join("Tablet/B.lean")), "B"),
            ),
        ]);
        let baseline_correspondence_hashes =
            observe_correspondence_fingerprints(&repo, &set(&["A", "B"])).unwrap();

        std::fs::remove_file(repo.join("Tablet/B.lean")).unwrap();
        std::fs::remove_file(repo.join("Tablet/B.tex")).unwrap();

        let result = cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &before_tablet_contents,
            &baseline_declaration_hashes,
            &baseline_correspondence_hashes,
            &set(&["t"]),
            &BTreeMap::from([
                (NodeId::from("A"), set(&["Preamble"])),
                (NodeId::from("B"), set(&["Preamble"])),
            ]),
            &BTreeMap::from([(NodeId::from("A"), set(&["t"]))]),
            &set(&["Preamble", "A", "B"]),
        )
        .unwrap();

        assert!(result.ok, "{:?}", result.errors);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn cleanup_preserving_allows_import_only_orphan_attachment() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{definition}B\\end{definition}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let before_tablet_contents = snapshot_tablet_contents(&repo);
        let baseline_declaration_hashes = BTreeMap::from([
            (
                NodeId::from("A"),
                declaration_hash(&read_text_if_exists(&repo.join("Tablet/A.lean")), "A"),
            ),
            (
                NodeId::from("B"),
                declaration_hash(&read_text_if_exists(&repo.join("Tablet/B.lean")), "B"),
            ),
        ]);
        let baseline_correspondence_hashes =
            observe_correspondence_fingerprints(&repo, &set(&["A", "B"])).unwrap();

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := by\n  trivial\n",
        );

        let result = cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &before_tablet_contents,
            &baseline_declaration_hashes,
            &baseline_correspondence_hashes,
            &set(&["t"]),
            &BTreeMap::from([
                (NodeId::from("A"), set(&["Preamble"])),
                (NodeId::from("B"), set(&["Preamble"])),
            ]),
            &BTreeMap::from([(NodeId::from("A"), set(&["t"]))]),
            &set(&["Preamble", "A", "B"]),
        )
        .unwrap();

        assert!(result.ok, "{:?}", result.errors);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn cleanup_preserving_rejects_retained_node_non_import_edits() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{definition}B\\end{definition}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let before_tablet_contents = snapshot_tablet_contents(&repo);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.B\n\ntheorem A : True := by\n  exact True.intro\n",
        );

        let result = cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &before_tablet_contents,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &set(&["t"]),
            &BTreeMap::from([
                (NodeId::from("A"), set(&["Preamble"])),
                (NodeId::from("B"), set(&["Preamble"])),
            ]),
            &BTreeMap::from([(NodeId::from("A"), set(&["t"]))]),
            &set(&["Preamble", "A", "B"]),
        )
        .unwrap();

        assert!(!result.ok);
        assert!(result.errors.iter().any(|err| {
            err.contains(
                "orphan cleanup may only add or remove Tablet import lines for current orphan nodes",
            )
        }));
    }

    #[test]
    fn cleanup_preserving_rejects_non_import_edits_on_retained_nodes() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{definition}B\\end{definition}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let before_tablet_contents = snapshot_tablet_contents(&repo);
        let baseline_declaration_hashes = BTreeMap::from([
            (
                NodeId::from("A"),
                declaration_hash(&read_text_if_exists(&repo.join("Tablet/A.lean")), "A"),
            ),
            (
                NodeId::from("B"),
                declaration_hash(&read_text_if_exists(&repo.join("Tablet/B.lean")), "B"),
            ),
        ]);
        let baseline_correspondence_hashes =
            observe_correspondence_fingerprints(&repo, &set(&["A", "B"])).unwrap();

        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 1\n",
        );

        let result = cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &before_tablet_contents,
            &baseline_declaration_hashes,
            &baseline_correspondence_hashes,
            &set(&["t"]),
            &BTreeMap::from([
                (NodeId::from("A"), BTreeSet::new()),
                (NodeId::from("B"), BTreeSet::new()),
            ]),
            &BTreeMap::from([(NodeId::from("A"), set(&["t"]))]),
            &set(&["Preamble", "A", "B"]),
        )
        .unwrap();

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("cleanup may not edit orphan nodes in place")));
    }

    #[test]
    fn cleanup_preserving_rejects_stale_context_without_whole_tablet_fallback() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{definition}B\\end{definition}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        std::fs::remove_file(repo.join("Tablet/B.lean")).unwrap();
        std::fs::remove_file(repo.join("Tablet/B.tex")).unwrap();

        let result = cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &BTreeMap::new(),
            &BTreeMap::from([(NodeId::from("A"), "decl-baseline".to_string())]),
            &BTreeMap::from([(NodeId::from("A"), "corr-baseline".to_string())]),
            &set(&["t"]),
            &BTreeMap::from([
                (NodeId::from("A"), set(&["Preamble"])),
                (NodeId::from("B"), set(&["Preamble"])),
            ]),
            &BTreeMap::from([(NodeId::from("A"), set(&["t"]))]),
            &set(&["Preamble", "A", "B"]),
        )
        .expect("stale cleanup context should fail validation, not observe the whole tablet");

        assert!(!result.ok);
        assert!(result.errors.iter().any(|err| {
            err.contains("stale/missing before_tablet_contents")
                && err.contains("refusing whole-tablet fallback")
        }));
    }

    #[test]
    fn final_cleanup_preserving_allows_existing_lean_hygiene_edits() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Mathlib.Data.Bool.Basic\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let baseline_declaration_hashes = BTreeMap::from([(
            NodeId::from("A"),
            declaration_hash(&read_text_if_exists(&repo.join("Tablet/A.lean")), "A"),
        )]);
        let baseline_correspondence_hashes =
            observe_correspondence_fingerprints(&repo, &set(&["A"])).unwrap();

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );

        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &baseline_declaration_hashes,
            &baseline_correspondence_hashes,
            &set(&["Preamble", "A"]),
            None,
            None,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(result.ok, "{:?}", result.errors);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn final_cleanup_preserving_rejects_correspondence_drift() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let baseline_declaration_hashes = BTreeMap::from([(
            NodeId::from("A"),
            declaration_hash(&read_text_if_exists(&repo.join("Tablet/A.lean")), "A"),
        )]);
        let baseline_correspondence_hashes =
            observe_correspondence_fingerprints(&repo, &set(&["A"])).unwrap();

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : False := by\n  contradiction\n",
        );

        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &baseline_declaration_hashes,
            &baseline_correspondence_hashes,
            &set(&["Preamble", "A"]),
            None,
            None,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(!result.ok);
        assert!(result.errors.iter().any(|err| {
            err.contains("correspondence fingerprint changed during final cleanup")
        }));
    }

    #[test]
    fn final_cleanup_preserving_rejects_tex_edits() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let baseline_declaration_hashes = BTreeMap::from([(
            NodeId::from("A"),
            declaration_hash(&read_text_if_exists(&repo.join("Tablet/A.lean")), "A"),
        )]);
        let baseline_correspondence_hashes =
            observe_correspondence_fingerprints(&repo, &set(&["A"])).unwrap();

        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A changed\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &baseline_declaration_hashes,
            &baseline_correspondence_hashes,
            &set(&["Preamble", "A"]),
            None,
            None,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("final cleanup may not modify .tex files")));
    }

    #[test]
    fn final_cleanup_lintfix_rejects_preamble_lean_modification() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package \"stub\"\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let baseline_declaration_hashes = BTreeMap::from([(
            NodeId::from("Preamble"),
            declaration_hash_for_gate(
                &repo,
                &read_text_if_exists(&repo.join("Tablet/Preamble.lean")),
                "Preamble",
            )
            .unwrap(),
        )]);
        let task_kind = trellis_kernel::CleanupTaskKind::LintFix {
            warning_text: "unused import".to_string(),
        };
        let target = NodeId::from("A");

        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\nimport Mathlib.Data.Set.Basic\n",
        );

        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &baseline_declaration_hashes,
            &BTreeMap::new(),
            &set(&["Preamble", "A"]),
            Some(&task_kind),
            Some(&target),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|err| { err.contains("structural .lean files") && err.contains("Preamble") }),
            "expected structural .lean scope rejection; got: {:?}",
            result.errors
        );
        assert!(
            result
                .errors
                .iter()
                .any(|err| err.contains("Preamble: declaration hash changed during final cleanup")),
            "expected tagged whole-file hash guard rejection; got: {:?}",
            result.errors
        );
    }

    /// Cleanup-v2 batch dispatch: the LintFix scope check permits edits to
    /// EVERY authorized batch node (target_node=None, authorized_nodes =
    /// the batch's target set) and rejects an edit to a node outside that
    /// set. Confirms batching widens dispatch scope only — the per-node
    /// verification (decl-hash, corr) still runs over every changed node.
    #[test]
    fn final_cleanup_lintfix_batch_allows_all_authorized_nodes_rejects_others() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package \"stub\"\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        for node in ["A", "B", "C"] {
            write(
                &repo.join(format!("Tablet/{node}.lean")),
                &format!(
                    "-- [TABLET NODE: {node}]\nimport Tablet.Preamble\n\ntheorem {node} : True := by\n  trivial\n"
                ),
            );
            write(
                &repo.join(format!("Tablet/{node}.tex")),
                &format!("\\begin{{theorem}}{node}\\end{{theorem}}\n\\begin{{proof}}Trivial.\\end{{proof}}\n"),
            );
        }

        let before_snapshot = snapshot_tablet_dir(&repo);
        let task_kind = trellis_kernel::CleanupTaskKind::LintFix {
            warning_text: String::new(),
        };
        // Batch authorizes A and B (target_node=None).
        let authorized: BTreeSet<NodeId> = set(&["A", "B"]);

        // Case 1: editing both authorized nodes A and B is in-scope.
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  exact trivial\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := by\n  exact trivial\n",
        );
        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &set(&["Preamble", "A", "B", "C"]),
            Some(&task_kind),
            None,
            &authorized,
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(
            !result
                .errors
                .iter()
                .any(|err| err.contains("may only edit")),
            "batched edits to authorized nodes A and B must be in-scope; got: {:?}",
            result.errors
        );

        // Case 2: editing an unauthorized node C is rejected.
        write(
            &repo.join("Tablet/C.lean"),
            "-- [TABLET NODE: C]\nimport Tablet.Preamble\n\ntheorem C : True := by\n  exact trivial\n",
        );
        let result_c = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &set(&["Preamble", "A", "B", "C"]),
            Some(&task_kind),
            None,
            &authorized,
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(
            result_c
                .errors
                .iter()
                .any(|err| err.contains("may only edit") && err.contains("C")),
            "an edit to unauthorized node C must be rejected; got: {:?}",
            result_c.errors
        );
    }

    #[test]
    fn final_cleanup_lintfix_ignores_unrelated_out_of_scope_noderef_error() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package \"stub\"\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write_node(
            &repo,
            "Target",
            "\\begin{theorem}Target\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
            "-- [TABLET NODE: Target]\nimport Tablet.Preamble\n\ntheorem Target : True := by\n  trivial\n",
        );
        write_node(
            &repo,
            "BadRef",
            "\\begin{theorem}BadRef\\end{theorem}\n\\begin{proof}Use \\noderef{Helper}.\\end{proof}\n",
            "-- [TABLET NODE: BadRef]\nimport Tablet.Preamble\n\ntheorem BadRef : True := by\n  trivial\n",
        );
        write_node(
            &repo,
            "Helper",
            "\\begin{theorem}Helper\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
            "-- [TABLET NODE: Helper]\nimport Tablet.Preamble\n\ntheorem Helper : True := by\n  trivial\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let baseline_declaration_hashes = BTreeMap::from([(
            NodeId::from("Target"),
            declaration_hash(
                &read_text_if_exists(&repo.join("Tablet/Target.lean")),
                "Target",
            ),
        )]);
        let baseline_correspondence_hashes =
            observe_correspondence_fingerprints(&repo, &set(&["Target"])).unwrap();

        write(
            &repo.join("Tablet/Target.lean"),
            "-- [TABLET NODE: Target]\nimport Tablet.Preamble\n\ntheorem Target : True := by\n  exact True.intro\n",
        );
        let task_kind = trellis_kernel::CleanupTaskKind::LintFix {
            warning_text: "proof cleanup".to_string(),
        };
        let target = NodeId::from("Target");

        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &baseline_declaration_hashes,
            &baseline_correspondence_hashes,
            &set(&["Preamble", "Target", "BadRef", "Helper"]),
            Some(&task_kind),
            Some(&target),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(result.ok, "{:?}", result.errors);
        assert!(
            !result
                .errors
                .iter()
                .any(|err| err.contains("BadRef") || err.contains("noderef")),
            "out-of-scope noderef error should not reject Target cleanup: {:?}",
            result.errors
        );
    }

    #[test]
    fn final_cleanup_lintfix_rejects_target_noderef_error() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package \"stub\"\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write_node(
            &repo,
            "Target",
            "\\begin{theorem}Target\\end{theorem}\n\\begin{proof}Use \\noderef{Helper}.\\end{proof}\n",
            "-- [TABLET NODE: Target]\nimport Tablet.Preamble\nimport Tablet.Helper\n\ntheorem Target : True := by\n  trivial\n",
        );
        write_node(
            &repo,
            "Helper",
            "\\begin{theorem}Helper\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
            "-- [TABLET NODE: Helper]\nimport Tablet.Preamble\n\ntheorem Helper : True := by\n  trivial\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let baseline_declaration_hashes = BTreeMap::from([(
            NodeId::from("Target"),
            declaration_hash(
                &read_text_if_exists(&repo.join("Tablet/Target.lean")),
                "Target",
            ),
        )]);
        let baseline_correspondence_hashes =
            observe_correspondence_fingerprints(&repo, &set(&["Target"])).unwrap();

        write(
            &repo.join("Tablet/Target.lean"),
            "-- [TABLET NODE: Target]\nimport Tablet.Preamble\n\ntheorem Target : True := by\n  trivial\n",
        );
        let task_kind = trellis_kernel::CleanupTaskKind::LintFix {
            warning_text: "unused import".to_string(),
        };
        let target = NodeId::from("Target");

        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &baseline_declaration_hashes,
            &baseline_correspondence_hashes,
            &set(&["Preamble", "Target", "Helper"]),
            Some(&task_kind),
            Some(&target),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();

        assert!(!result.ok);
        assert!(
            result.errors.iter().any(|err| {
                err.contains("Target")
                    && err.contains("\\noderef{Helper}")
                    && err.contains("not in Target's Lean import closure")
            }),
            "expected target noderef closure rejection; got: {:?}",
            result.errors
        );
    }

    /// Cleanup-v2 (audit Finding 7): a Substitution task that fails to
    /// delete BOTH `target_node.lean` and `target_node.tex` is rejected.
    /// Pre-fix the validator only checked deleted files were in the
    /// allowed set; deleting only `.lean` (leaving `.tex` orphan) or
    /// deleting nothing at all would pass.
    #[test]
    fn final_cleanup_substitution_must_delete_both_target_files() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package \u{ab}stub\u{bb}\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);
        let baseline_decl: BTreeMap<NodeId, String> = BTreeMap::new();
        let baseline_corr: BTreeMap<NodeId, String> = BTreeMap::new();
        // Delete only A.lean (leave A.tex).
        std::fs::remove_file(repo.join("Tablet/A.lean")).unwrap();
        let task_kind = trellis_kernel::CleanupTaskKind::Substitution {
            replacement: trellis_kernel::CleanupReplacement::Mathlib {
                citation: "Nat.add_comm".into(),
            },
        };
        let target = NodeId::from("A");
        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &baseline_decl,
            &baseline_corr,
            &set(&["Preamble", "A"]),
            Some(&task_kind),
            Some(&target),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|err| err.contains("must delete BOTH") || err.contains("missing deletions")),
            "expected substitution missing-deletion error; got: {:?}",
            result.errors
        );
    }

    /// Cleanup-v2 (audit Finding 8): editing a structural .tex file
    /// (`header.tex` / `Preamble.tex`) during a Substitution burst must
    /// be rejected. Pre-fix the predicate was inverted: structural files
    /// were EXCLUDED from the illegal list rather than included.
    #[test]
    fn final_cleanup_substitution_rejects_preamble_tex_edit() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package \u{ab}stub\u{bb}\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);
        let baseline_decl: BTreeMap<NodeId, String> = BTreeMap::new();
        let baseline_corr: BTreeMap<NodeId, String> = BTreeMap::new();
        // Worker substitutes target A and ALSO modifies Preamble.tex —
        // this should be rejected even though Preamble is "structural"
        // and was previously excluded from the illegal list.
        std::fs::remove_file(repo.join("Tablet/A.lean")).unwrap();
        std::fs::remove_file(repo.join("Tablet/A.tex")).unwrap();
        write(
            &repo.join("Tablet/Preamble.tex"),
            "% Worker attempted to modify the preamble — illegal\n",
        );
        let task_kind = trellis_kernel::CleanupTaskKind::Substitution {
            replacement: trellis_kernel::CleanupReplacement::Mathlib {
                citation: "Nat.add_comm".into(),
            },
        };
        let target = NodeId::from("A");
        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &baseline_decl,
            &baseline_corr,
            &set(&["Preamble", "A"]),
            Some(&task_kind),
            Some(&target),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|err| err.contains("illegal .tex edits") && err.contains("Preamble")),
            "expected structural .tex rejection containing 'Preamble'; got: {:?}",
            result.errors
        );
    }

    /// Cleanup-v2 (audit Finding 7): a properly-shaped Substitution
    /// (deletes both target files, edits an authorized importer) is
    /// accepted.
    #[test]
    fn final_cleanup_substitution_accepts_proper_both_file_deletion() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package \u{ab}stub\u{bb}\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/Target.lean"),
            "-- [TABLET NODE: Target]\nimport Tablet.Preamble\n\ntheorem Target : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/Target.tex"),
            "\\begin{theorem}Target\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);
        let baseline_decl: BTreeMap<NodeId, String> = BTreeMap::new();
        let baseline_corr: BTreeMap<NodeId, String> = BTreeMap::new();
        // Delete both target files (proper substitution).
        std::fs::remove_file(repo.join("Tablet/Target.lean")).unwrap();
        std::fs::remove_file(repo.join("Tablet/Target.tex")).unwrap();
        let task_kind = trellis_kernel::CleanupTaskKind::Substitution {
            replacement: trellis_kernel::CleanupReplacement::Mathlib {
                citation: "Nat.add_comm".into(),
            },
        };
        let target = NodeId::from("Target");
        let result = final_cleanup_preserving_step_result(
            &repo,
            &before_snapshot,
            &baseline_decl,
            &baseline_corr,
            &set(&["Preamble", "Target"]),
            Some(&task_kind),
            Some(&target),
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        // We don't check the build status here (the stub build may fail
        // for other reasons in this synthetic harness). The targeted
        // assertion is: NO substitution-deletion-rule errors fire.
        assert!(
            !result
                .errors
                .iter()
                .any(|err| err.contains("must delete BOTH") || err.contains("missing deletions")),
            "proper substitution should not produce a missing-deletion error; got: {:?}",
            result.errors
        );
        assert!(
            !result
                .errors
                .iter()
                .any(|err| err.contains("illegal .tex edits")),
            "proper substitution should not produce an illegal-tex error; got: {:?}",
            result.errors
        );
    }

    #[test]
    fn proof_easy_scope_rejects_editing_other_node_lean() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{definition}B\\end{definition}\n",
        );
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 1\n",
        );

        let result = proof_easy_scope_step_result(&repo, "A", &before_snapshot);

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("Easy mode only allows editing `A.lean`. Modified:")));
    }

    #[test]
    fn proof_easy_scope_allows_new_lean_closed_helper_in_active_support_cone() {
        // Easy mode permits a new helper node when (a) it ships as a paired
        // .lean+.tex, (b) the .lean is Lean-closed (no sorry), and (c) the
        // active node imports it (so it lands inside the active support
        // cone). This lets workers extract a clean lemma without escalating
        // to hard local, while preserving Easy's "no new open obligations"
        // invariant.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        // Worker writes a Lean-closed helper plus its .tex, and imports
        // the helper from the active node. Active proof body now closes.
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.helper\n\ntheorem A (n : Nat) : n + 0 = n := helper n\n",
        );
        write(
            &repo.join("Tablet/helper.lean"),
            "-- [TABLET NODE: helper]\nimport Tablet.Preamble\n\ntheorem helper (n : Nat) : n + 0 = n := Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/helper.tex"),
            "\\begin{theorem}For every natural $n$, $n + 0 = n$.\\end{theorem}\n\\begin{proof}Right-zero law.\\end{proof}\n",
        );

        let result = proof_easy_scope_step_result(&repo, "A", &before_snapshot);

        assert!(result.ok, "{:?}", result.errors);
    }

    #[test]
    fn proof_easy_scope_rejects_open_active_node() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        let result = proof_easy_scope_step_result(&repo, "A", &before_snapshot);

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("requires the active node to be Lean-closed")));
    }

    #[test]
    fn proof_easy_scope_rejects_new_helper_with_sorry() {
        // A new helper carrying `sorry` would introduce a new open
        // obligation — that's hard local territory. Easy must reject so
        // the reviewer escalates.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.helper\n\ntheorem A (n : Nat) : n + 0 = n := helper n\n",
        );
        write(
            &repo.join("Tablet/helper.lean"),
            "-- [TABLET NODE: helper]\nimport Tablet.Preamble\n\ntheorem helper (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/helper.tex"),
            "\\begin{theorem}helper\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let result = proof_easy_scope_step_result(&repo, "A", &before_snapshot);

        assert!(!result.ok);
        assert!(result.errors.iter().any(|err| err
            .contains("Lean-closed helpers in the active support cone. Disallowed created")));
    }

    #[test]
    fn proof_easy_scope_rejects_new_helper_outside_active_support_cone() {
        // A Lean-closed helper that the active node does not import is an
        // orphan — the worker hasn't actually used it to close anything.
        // Easy rejects so that the kernel doesn't silently accept dead
        // weight nodes.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        // Helper is Lean-closed but the active node does not import it.
        write(
            &repo.join("Tablet/orphan_helper.lean"),
            "-- [TABLET NODE: orphan_helper]\nimport Tablet.Preamble\n\ntheorem orphan_helper (n : Nat) : n + 0 = n := Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/orphan_helper.tex"),
            "\\begin{theorem}orphan\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let result = proof_easy_scope_step_result(&repo, "A", &before_snapshot);

        assert!(!result.ok);
        assert!(result.errors.iter().any(|err| err
            .contains("Lean-closed helpers in the active support cone. Disallowed created")));
    }

    #[test]
    fn proof_easy_delta_allows_new_lean_closed_helper_in_active_support_cone() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let original_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := by\n  sorry\n";
        write(&repo.join("Tablet/A.lean"), original_active);
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.helper\n\ntheorem A (n : Nat) : n + 0 = n := helper n\n",
        );
        write(
            &repo.join("Tablet/helper.lean"),
            "-- [TABLET NODE: helper]\nimport Tablet.Preamble\n\ntheorem helper (n : Nat) : n + 0 = n := Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/helper.tex"),
            "\\begin{theorem}For every natural $n$, $n + 0 = n$.\\end{theorem}\n\\begin{proof}Right-zero law.\\end{proof}\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(original_active, "A"),
            WorkerProofDeltaMode::Easy,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            false,
            true,
        )
        .unwrap();

        assert!(result.ok, "{:?}", result.errors);
    }

    #[test]
    fn proof_easy_delta_rejects_open_active_node_even_with_closed_helper() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let original_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := by\n  sorry\n";
        write(&repo.join("Tablet/A.lean"), original_active);
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.helper\n\ntheorem A (n : Nat) : n + 0 = n := by\n  have h := helper n\n  sorry\n",
        );
        write(
            &repo.join("Tablet/helper.lean"),
            "-- [TABLET NODE: helper]\nimport Tablet.Preamble\n\ntheorem helper (n : Nat) : n + 0 = n := Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/helper.tex"),
            "\\begin{theorem}For every natural $n$, $n + 0 = n$.\\end{theorem}\n\\begin{proof}Right-zero law.\\end{proof}\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(original_active, "A"),
            WorkerProofDeltaMode::Easy,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            false,
            true,
        )
        .unwrap();

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("requires the active node to be Lean-closed")));
    }

    #[test]
    fn proof_easy_delta_rejects_new_helper_with_sorry() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let original_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := by\n  sorry\n";
        write(&repo.join("Tablet/A.lean"), original_active);
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.helper\n\ntheorem A (n : Nat) : n + 0 = n := helper n\n",
        );
        write(
            &repo.join("Tablet/helper.lean"),
            "-- [TABLET NODE: helper]\nimport Tablet.Preamble\n\ntheorem helper (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/helper.tex"),
            "\\begin{theorem}helper\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(original_active, "A"),
            WorkerProofDeltaMode::Easy,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            false,
            false,
        )
        .unwrap();

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("does not allow new open obligations")));
    }

    #[test]
    fn proof_local_delta_still_allows_new_helper_with_sorry() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let original_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := by\n  sorry\n";
        write(&repo.join("Tablet/A.lean"), original_active);
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.helper\n\ntheorem A (n : Nat) : n + 0 = n := helper n\n",
        );
        write(
            &repo.join("Tablet/helper.lean"),
            "-- [TABLET NODE: helper]\nimport Tablet.Preamble\n\ntheorem helper (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/helper.tex"),
            "\\begin{theorem}helper\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(original_active, "A"),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        assert!(result.ok, "{:?}", result.errors);
    }

    /// Audit-ordered node retirement contract pin (operator 2026-07-08):
    /// `allow_new_obligations=false` gates only NEWLY-CREATED nodes. A
    /// retirement-shaped Restructure delta that deletes the named node
    /// and re-opens an EXISTING survivor's proof body to `sorry` is
    /// ACCEPTED under that flag; the same delta additionally creating a
    /// new OPEN helper is REJECTED by the new-obligations gate. The two
    /// semantics are disjoint; the retirement feature relies on that.
    #[test]
    fn node_retirement_shape_delta_rejects_new_open_helper_but_allows_survivor_reopen() {
        fn seed(repo: &Path) -> (BTreeMap<String, String>, String) {
            write_stub_check_script(repo);
            std::fs::create_dir_all(repo.join("Tablet")).unwrap();
            write(&repo.join("lakefile.lean"), "package «stub»\n");
            write(
                &repo.join("Tablet/Preamble.lean"),
                "import Mathlib.Data.Nat.Basic\n",
            );
            write(&repo.join("Tablet/Preamble.tex"), "");
            let original_active = "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n";
            write(&repo.join("Tablet/A.lean"), original_active);
            write(
                &repo.join("Tablet/A.tex"),
                "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
            );
            write(
                &repo.join("Tablet/R.lean"),
                "-- [TABLET NODE: R]\nimport Tablet.Preamble\n\ntheorem R (n : Nat) : n + 0 = n := Nat.add_zero n\n",
            );
            write(
                &repo.join("Tablet/R.tex"),
                "\\begin{theorem}R\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
            );
            write(
                &repo.join("Tablet/S.lean"),
                "-- [TABLET NODE: S]\nimport Tablet.Preamble\nimport Tablet.R\n\ntheorem S (n : Nat) : n + 0 = n := R n\n",
            );
            write(
                &repo.join("Tablet/S.tex"),
                "\\begin{theorem}S\\end{theorem}\n\\begin{proof}Via R.\\end{proof}\n",
            );
            let before_snapshot = snapshot_tablet_dir(repo);
            // Retirement delta: delete R; re-open survivor S to sorry.
            std::fs::remove_file(repo.join("Tablet/R.lean")).unwrap();
            std::fs::remove_file(repo.join("Tablet/R.tex")).unwrap();
            write(
                &repo.join("Tablet/S.lean"),
                "-- [TABLET NODE: S]\nimport Tablet.Preamble\n\ntheorem S (n : Nat) : n + 0 = n := by\n  sorry\n",
            );
            (before_snapshot, original_active.to_string())
        }
        fn run(repo: &Path, before: &BTreeMap<String, String>, active: &str) -> WorkerValidationStepResult {
            let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
            proof_worker_delta_step_result(
                repo,
                "A",
                before,
                &set(&["Preamble", "A", "R", "S"]),
                &set(&["R"]), // declared_deleted_nodes
                &BTreeMap::new(),
                &BTreeSet::new(),
                &declaration_hash(active, "A"),
                WorkerProofDeltaMode::Restructure,
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeSet::new(),
                &set(&["R", "S"]), // authorized_nodes
                &BTreeSet::new(),
                &mut protected_semantic_change_nodes,
                false, // allow_new_obligations — the flag under test
                false, // must_close_active
            )
            .unwrap()
        }

        // Accepted half: deletion + survivor reopen only.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before, active) = seed(&repo);
        let accepted = run(&repo, &before, &active);
        assert!(
            accepted.ok,
            "survivor reopen to sorry under allow_new_obligations=false must be accepted: {:?}",
            accepted.errors
        );

        // Rejected half: the same delta plus a NEW open helper node.
        let tmp2 = tempdir().unwrap();
        let repo2 = tmp2.path().join("repo");
        let (before2, active2) = seed(&repo2);
        write(
            &repo2.join("Tablet/NewHelper.lean"),
            "-- [TABLET NODE: NewHelper]\nimport Tablet.Preamble\n\ntheorem NewHelper (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo2.join("Tablet/NewHelper.tex"),
            "\\begin{theorem}NewHelper\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let rejected = run(&repo2, &before2, &active2);
        assert!(!rejected.ok);
        assert!(
            rejected
                .errors
                .iter()
                .any(|err| err.contains("does not allow new open obligations")),
            "new open helper must be rejected by the new-obligations gate: {:?}",
            rejected.errors
        );
    }

    #[test]
    fn proof_easy_scope_rejects_deleting_files() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ndef B : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{definition}B\\end{definition}\n",
        );
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);

        std::fs::remove_file(&repo.join("Tablet/B.lean")).unwrap();

        let result = proof_easy_scope_step_result(&repo, "A", &before_snapshot);

        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|err| err.contains("Easy mode does not allow deleting files")));
    }

    #[test]
    fn proof_local_flags_extra_existing_lean_and_tex_changes_as_out_of_scope() {
        let changes = SnapshotChanges {
            created: Vec::new(),
            modified: vec![
                "main.lean".to_string(),
                "neighbor.lean".to_string(),
                "neighbor.tex".to_string(),
            ],
            deleted: Vec::new(),
        };

        let scope = proof_worker_delta_scope(
            &changes,
            "main",
            &set(&["main", "neighbor"]),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            trellis_kernel::backend::BackendId::Lean,
        );

        assert!(scope.active_changed);
        assert_eq!(
            scope.authorized_extra_changed_existing_nodes,
            Vec::<String>::new()
        );
        assert_eq!(
            scope.unauthorized_extra_changed_existing_nodes,
            vec!["neighbor".to_string()]
        );
    }

    #[test]
    fn guard_retries_unavailable_target_payload_without_blessing_changed_fingerprint() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let recovered = tmp.path().join("recovered");
        write_minimal_lake_repo(&repo);
        clear_lean_semantic_payload_cache_for_tests(&repo);
        write_node(
            &repo,
            "Main",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
            "-- [TABLET NODE: Main]\nimport Tablet.Preamble\n\ntheorem Main : True := trivial\n",
        );
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path

cmd = sys.argv[1]
log_path = Path({log_path:?})
recovered_path = Path({recovered_path:?})
with log_path.open("a", encoding="utf-8") as h:
    h.write(cmd + "\n")

if cmd == "sync-tablet-support":
    print(json.dumps({{
        "updated_paths": [],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }}))
elif cmd == "materialize-tablet-oleans":
    recovered_path.write_text("yes", encoding="utf-8")
    print(json.dumps({{
        "returncode": 0,
        "stdout": "recovered",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "lean-semantic-payloads":
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    if not recovered_path.exists():
        payloads = {{n: {{"ok": False, "payload": "", "error": "missing olean"}} for n in nodes}}
    else:
        payloads = {{n: {{"ok": True, "payload": f"{{n}}-changed", "error": ""}} for n in nodes}}
    print(json.dumps(payloads))
else:
    raise SystemExit(f"unexpected: {{cmd}}")
"#,
            log_path = log.display().to_string(),
            recovered_path = recovered.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        let own_tex = hash_text(&extract_tex_statement_block(&read_text_if_exists(
            &repo.join("Tablet/Main.tex"),
        )));
        let approved = BTreeMap::from([(
            NodeId::from("Main"),
            cf(&own_tex, &hash_text("Main-approved"), "", &[]).to_storage_string(),
        )]);
        let errors = paper_target_corr_reopen_guard_errors(
            &repo,
            &[NodeId::from("Main")].into_iter().collect(),
            &approved,
            WorkerProofDeltaMode::Local,
        )
        .unwrap();

        assert_eq!(count_invocations(&log, "lean-semantic-payloads"), 2);
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);
        assert_eq!(errors.len(), 1);
        let msg = &errors[0];
        assert!(
            msg.contains("Lean semantic closure has changed"),
            "retry must expose the real fingerprint diff instead of blessing it: {msg:?}"
        );
        assert!(
            !msg.contains("after a narrow checker-support recovery retry"),
            "successful recovery should not leave the generic unavailable diagnostic: {msg:?}"
        );
    }

    #[test]
    fn guard_reports_semantic_payload_error_when_recovery_still_unavailable() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        clear_lean_semantic_payload_cache_for_tests(&repo);
        write_node(
            &repo,
            "Main",
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
            "-- [TABLET NODE: Main]\nimport Tablet.Preamble\n\ntheorem Main : True := trivial\n",
        );
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path

cmd = sys.argv[1]
log_path = Path({log_path:?})
with log_path.open("a", encoding="utf-8") as h:
    h.write(cmd + "\n")

if cmd == "sync-tablet-support":
    print(json.dumps({{
        "updated_paths": [],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }}))
elif cmd == "materialize-tablet-oleans":
    print(json.dumps({{
        "returncode": 1,
        "stdout": "",
        "stderr": "target build failed",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "lean-semantic-payloads":
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    payloads = {{n: {{"ok": False, "payload": "", "error": "still missing olean"}} for n in nodes}}
    print(json.dumps(payloads))
else:
    raise SystemExit(f"unexpected: {{cmd}}")
"#,
            log_path = log.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        let own_tex = hash_text(&extract_tex_statement_block(&read_text_if_exists(
            &repo.join("Tablet/Main.tex"),
        )));
        let approved = BTreeMap::from([(
            NodeId::from("Main"),
            cf(&own_tex, &hash_text("Main-approved"), "", &[]).to_storage_string(),
        )]);
        let errors = paper_target_corr_reopen_guard_errors(
            &repo,
            &[NodeId::from("Main")].into_iter().collect(),
            &approved,
            WorkerProofDeltaMode::Local,
        )
        .unwrap();

        assert_eq!(count_invocations(&log, "lean-semantic-payloads"), 2);
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 1);
        assert_eq!(errors.len(), 1);
        let msg = &errors[0];
        assert!(
            msg.contains("still missing olean") && msg.contains("target build failed"),
            "unavailable diagnostic should preserve payload and recovery errors: {msg:?}"
        );
    }

    #[test]
    fn guard_does_not_retry_non_semantic_fingerprint_unavailability() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        clear_lean_semantic_payload_cache_for_tests(&repo);
        write(
            &repo.join("Tablet/Main.lean"),
            "-- [TABLET NODE: Main]\nimport Tablet.Preamble\n\ntheorem Main : True := trivial\n",
        );
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path

cmd = sys.argv[1]
log_path = Path({log_path:?})
with log_path.open("a", encoding="utf-8") as h:
    h.write(cmd + "\n")

if cmd == "sync-tablet-support":
    print(json.dumps({{
        "updated_paths": [],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }}))
elif cmd == "materialize-tablet-oleans":
    print(json.dumps({{
        "returncode": 0,
        "stdout": "should not run",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "lean-semantic-payloads":
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    print(json.dumps({{n: {{"ok": True, "payload": f"{{n}}-payload", "error": ""}} for n in nodes}}))
else:
    raise SystemExit(f"unexpected: {{cmd}}")
"#,
            log_path = log.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        let approved = BTreeMap::from([(
            NodeId::from("Main"),
            cf("approved-tex", &hash_text("Main-approved"), "", &[]).to_storage_string(),
        )]);
        let errors = paper_target_corr_reopen_guard_errors(
            &repo,
            &[NodeId::from("Main")].into_iter().collect(),
            &approved,
            WorkerProofDeltaMode::Local,
        )
        .unwrap();

        assert_eq!(count_invocations(&log, "lean-semantic-payloads"), 1);
        assert_eq!(count_invocations(&log, "materialize-tablet-oleans"), 0);
        assert_eq!(errors.len(), 1);
        assert!(
            !errors[0].contains("recovery retry"),
            "non-recoverable fingerprint gaps should not claim a recovery retry ran: {:?}",
            errors[0]
        );
    }

    #[test]
    fn proof_delta_rejects_active_change_when_active_not_explicitly_authorized() {
        // CoarseRestructure: active node `main` was edited but is NOT in
        // authorized_nodes. Expected: unauthorized_active_change=true.
        let changes = SnapshotChanges {
            created: Vec::new(),
            modified: vec!["main.lean".to_string()],
            deleted: Vec::new(),
        };
        let scope = proof_worker_delta_scope(
            &changes,
            "main",
            &set(&["main", "support"]),
            WorkerProofDeltaMode::CoarseRestructure,
            &set(&["support"]),
            trellis_kernel::backend::BackendId::Lean,
        );
        assert!(scope.active_changed);
        assert!(scope.unauthorized_active_change);
    }

    #[test]
    fn proof_delta_allows_active_change_when_explicitly_authorized() {
        // CoarseRestructure: active=main, authorized={main, support}.
        // The active edit is authorized → unauthorized_active_change=false.
        let changes = SnapshotChanges {
            created: Vec::new(),
            modified: vec!["main.lean".to_string(), "main.tex".to_string()],
            deleted: Vec::new(),
        };
        let scope = proof_worker_delta_scope(
            &changes,
            "main",
            &set(&["main", "support"]),
            WorkerProofDeltaMode::CoarseRestructure,
            &set(&["main", "support"]),
            trellis_kernel::backend::BackendId::Lean,
        );
        assert!(scope.active_changed);
        assert!(!scope.unauthorized_active_change);
    }

    #[test]
    fn proof_delta_local_mode_implicitly_authorizes_active_change() {
        // Local mode: active is the proof body the worker IS supposed
        // to edit. authorized_nodes is empty by Local's contract; the
        // active-node guard must NOT fire here.
        let changes = SnapshotChanges {
            created: Vec::new(),
            modified: vec!["main.lean".to_string()],
            deleted: Vec::new(),
        };
        let scope = proof_worker_delta_scope(
            &changes,
            "main",
            &set(&["main"]),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            trellis_kernel::backend::BackendId::Lean,
        );
        assert!(scope.active_changed);
        assert!(!scope.unauthorized_active_change);
    }

    #[test]
    fn proof_delta_existing_node_change_obeys_explicit_authorized_list() {
        // CoarseRestructure: active=main, authorized={support}.
        // Editing `other` (existing, not in authorized) → unauthorized.
        let changes = SnapshotChanges {
            created: Vec::new(),
            modified: vec!["other.lean".to_string()],
            deleted: Vec::new(),
        };
        let scope = proof_worker_delta_scope(
            &changes,
            "main",
            &set(&["main", "support", "other"]),
            WorkerProofDeltaMode::CoarseRestructure,
            &set(&["support"]),
            trellis_kernel::backend::BackendId::Lean,
        );
        assert!(!scope.active_changed);
        assert_eq!(
            scope.unauthorized_extra_changed_existing_nodes,
            vec!["other".to_string()]
        );
        assert_eq!(
            scope.authorized_extra_changed_existing_nodes,
            Vec::<String>::new()
        );
    }

    #[test]
    fn proof_restructure_validates_authorized_tex_only_existing_changes() {
        let changes = SnapshotChanges {
            created: Vec::new(),
            modified: vec!["support.tex".to_string()],
            deleted: Vec::new(),
        };

        let scope = proof_worker_delta_scope(
            &changes,
            "main",
            &set(&["main", "support"]),
            WorkerProofDeltaMode::Restructure,
            &set(&["support"]),
            trellis_kernel::backend::BackendId::Lean,
        );

        assert!(!scope.active_changed);
        assert_eq!(
            scope.authorized_extra_changed_existing_nodes,
            vec!["support".to_string()]
        );
        assert_eq!(
            scope.unauthorized_extra_changed_existing_nodes,
            Vec::<String>::new()
        );
    }

    #[test]
    fn proof_delta_scope_rejects_deleted_existing_nodes() {
        let changes = SnapshotChanges {
            created: Vec::new(),
            modified: vec!["main.lean".to_string()],
            deleted: vec!["support.tex".to_string()],
        };

        let scope = proof_worker_delta_scope(
            &changes,
            "main",
            &set(&["main", "support"]),
            WorkerProofDeltaMode::CoarseRestructure,
            &set(&["support"]),
            trellis_kernel::backend::BackendId::Lean,
        );

        assert_eq!(scope.deleted_existing_nodes, vec!["support".to_string()]);
    }

    #[test]
    fn proof_delta_step_allows_declared_existing_node_deletion() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let main_lean =
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := by\n  trivial\n";
        write(&repo.join("Tablet/main.lean"), main_lean);
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/support.lean"),
            "-- [TABLET NODE: support]\nimport Tablet.Preamble\n\ndef support : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/support.tex"),
            "\\begin{definition}support\\end{definition}\n",
        );
        let before_snapshot = snapshot_tablet_dir(&repo);
        std::fs::remove_file(repo.join("Tablet/support.lean")).unwrap();
        std::fs::remove_file(repo.join("Tablet/support.tex")).unwrap();

        let mut protected_semantic_change_nodes = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "main",
            &before_snapshot,
            &set(&["Preamble", "main", "support"]),
            &set(&["support"]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &declaration_hash(main_lean, "main"),
            WorkerProofDeltaMode::CoarseRestructure,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        assert!(result.ok, "{:?}", result.errors);
        assert_eq!(result.detail, "No files were changed.");
    }

    #[test]
    fn proof_local_allows_new_closed_helper_nodes_inside_active_support_cone() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}For every natural number $n$, we have $n + 0 = n$.\\end{theorem}\n\\begin{proof}Proof omitted.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let _protected_snapshot = BTreeMap::from([(
            "main".to_string(),
            observe_correspondence_fingerprints(&repo, &BTreeSet::from([NodeId::from("main")]))
                .unwrap()
                .remove("main")
                .unwrap_or_default(),
        )]);
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\nimport Tablet.helper_thm\n\ntheorem main (n : Nat) : n + 0 = n := by\n  simpa [helper_def] using helper_thm n\n",
        );
        // (#45) Local mode forbids modifying the active node's .tex. The
        // pre-#45 version of this test wrote main.tex here; that's been
        // dropped so the test exercises the support-cone / helper logic
        // it intends to. Dedicated .tex-rejection coverage lives in
        // proof_local_rejects_active_tex_modification below.
        write(
            &repo.join("Tablet/helper_thm.lean"),
            "-- [TABLET NODE: helper_thm]\nimport Tablet.Preamble\nimport Tablet.helper_def\n\ntheorem helper_thm (n : Nat) : helper_def n = n := by\n  simpa [helper_def] using Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/helper_thm.tex"),
            "\\begin{helper}For every natural number $n$, helper\\_def$(n) = n$.\\end{helper}\n\\begin{proof}Expand the definition and apply the right-zero law.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/helper_def.lean"),
            "-- [TABLET NODE: helper_def]\nimport Tablet.Preamble\n\ndef helper_def (n : Nat) : Nat := n + 0\n",
        );
        write(
            &repo.join("Tablet/helper_def.tex"),
            "\\begin{definition}Define helper\\_def$(n)$ to be $n + 0$.\\end{definition}\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "main",
            &before_snapshot,
            &set(&["Preamble", "main"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(
                "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main (n : Nat) : n + 0 = n := by\n  sorry\n",
                "main",
            ),
            WorkerProofDeltaMode::Local,
            // Post-refactor: test fixtures use empty covering-node set +
            // empty approved_corr_fingerprints. Since the new guard
            // (paper_target_corr_reopen_guard_errors) short-circuits when
            // covering_nodes is empty, these tests no longer exercise the
            // coarse_package_guard path — they exercise the remaining
            // delta-scope checks in proof_worker_delta_step_result.
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),  // coarse_dag_nodes (legacy test: empty → treat as all-coarse)
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        assert!(result.ok, "{:?}", result.errors);
    }

    #[test]
    fn proof_local_materializes_new_helpers_before_rechecking_active_node() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_import_sensitive_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}For every natural number $n$, we have $n + 0 = n$.\\end{theorem}\n\\begin{proof}Proof omitted.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let _protected_snapshot = BTreeMap::from([(
            "main".to_string(),
            observe_correspondence_fingerprints(&repo, &BTreeSet::from([NodeId::from("main")]))
                .unwrap()
                .remove("main")
                .unwrap_or_default(),
        )]);

        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\nimport Tablet.helper_thm\n\ntheorem main (n : Nat) : n + 0 = n := by\n  simpa [helper_def] using helper_thm n\n",
        );
        // (#45) Local mode forbids modifying the active node's .tex.
        // See proof_local_rejects_active_tex_modification for that path.
        write(
            &repo.join("Tablet/helper_def.lean"),
            "-- [TABLET NODE: helper_def]\nimport Tablet.Preamble\n\ndef helper_def (n : Nat) : Nat := n + 0\n",
        );
        write(
            &repo.join("Tablet/helper_def.tex"),
            "\\begin{definition}Define helper\\_def$(n)$ to be $n + 0$.\\end{definition}\n",
        );
        write(
            &repo.join("Tablet/helper_thm.lean"),
            "-- [TABLET NODE: helper_thm]\nimport Tablet.Preamble\nimport Tablet.helper_def\n\ntheorem helper_thm (n : Nat) : helper_def n = n := by\n  simpa [helper_def] using Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/helper_thm.tex"),
            "\\begin{helper}For every natural number $n$, helper\\_def$(n) = n$.\\end{helper}\n\\begin{proof}Expand the definition and apply the right-zero law.\\end{proof}\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "main",
            &before_snapshot,
            &set(&["Preamble", "main"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(
                "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main (n : Nat) : n + 0 = n := by\n  sorry\n",
                "main",
            ),
            WorkerProofDeltaMode::Local,
            // Post-refactor: test fixtures use empty covering-node set +
            // empty approved_corr_fingerprints. Since the new guard
            // (paper_target_corr_reopen_guard_errors) short-circuits when
            // covering_nodes is empty, these tests no longer exercise the
            // coarse_package_guard path — they exercise the remaining
            // delta-scope checks in proof_worker_delta_step_result.
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),  // coarse_dag_nodes (legacy test: empty → treat as all-coarse)
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        assert!(result.ok, "{:?}", result.errors);
        assert!(repo
            .join(".lake/build/lib/lean/Tablet/helper_thm.olean")
            .exists());
        assert!(repo
            .join(".lake/build/lib/lean/Tablet/helper_def.olean")
            .exists());
    }

    #[test]
    fn proof_local_rejects_new_nodes_outside_active_support_cone() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}For every natural number $n$, we have $n + 0 = n$.\\end{theorem}\n\\begin{proof}Proof omitted.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        let _protected_snapshot = BTreeMap::from([(
            "main".to_string(),
            observe_correspondence_fingerprints(&repo, &BTreeSet::from([NodeId::from("main")]))
                .unwrap()
                .remove("main")
                .unwrap_or_default(),
        )]);
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main (n : Nat) : n + 0 = n := by\n  simpa using Nat.add_zero n\n",
        );
        // (#45) Local mode forbids modifying the active node's .tex.
        // See proof_local_rejects_active_tex_modification for that path.
        write(
            &repo.join("Tablet/helper_def.lean"),
            "-- [TABLET NODE: helper_def]\nimport Tablet.Preamble\n\ndef helper_def (n : Nat) : Nat := n + 0\n",
        );
        write(
            &repo.join("Tablet/helper_def.tex"),
            "\\begin{definition}Define helper\\_def$(n)$ to be $n + 0$.\\end{definition}\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "main",
            &before_snapshot,
            &set(&["Preamble", "main"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(
                "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main (n : Nat) : n + 0 = n := by\n  sorry\n",
                "main",
            ),
            WorkerProofDeltaMode::Local,
            // Post-refactor: test fixtures use empty covering-node set +
            // empty approved_corr_fingerprints. Since the new guard
            // (paper_target_corr_reopen_guard_errors) short-circuits when
            // covering_nodes is empty, these tests no longer exercise the
            // coarse_package_guard path — they exercise the remaining
            // delta-scope checks in proof_worker_delta_step_result.
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),  // coarse_dag_nodes (legacy test: empty → treat as all-coarse)
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        assert!(!result.ok);
        assert!(result.errors.iter().any(|err| err.contains("support cone")));
    }

    #[test]
    fn proof_local_rejects_active_tex_modification() {
        // (#45 / commit 60321e2) Local mode authorizes only proof-body
        // edits to the active node's .lean. Modifying the active node's
        // .tex must be rejected by the deterministic gate. This was
        // previously implicitly exercised (and broken) by the three
        // proof_local_* tests above which incidentally wrote main.tex;
        // this test is the dedicated regression covering the
        // rejection path.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main (n : Nat) : n + 0 = n := by\n  sorry\n",
        );
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}For every natural number $n$, we have $n + 0 = n$.\\end{theorem}\n\\begin{proof}Proof omitted.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);
        // Modify ONLY main.tex — the proof-body .lean stays as-is. This
        // isolates the .tex-rejection path; no helpers, no support-cone
        // logic interferes.
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}A modified theorem statement.\\end{theorem}\n\\begin{proof}Modified proof.\\end{proof}\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "main",
            &before_snapshot,
            &set(&["Preamble", "main"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(
                "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main (n : Nat) : n + 0 = n := by\n  sorry\n",
                "main",
            ),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        assert!(
            !result.ok,
            "Local-mode .tex modification should be rejected"
        );
        assert!(
            result
                .errors
                .iter()
                .any(|err| err.contains("Local mode authorizes only proof-body edits")),
            "expected Local-mode .tex-rejection error, got: {:?}",
            result.errors
        );
    }

    // ---------------------------------------------------------------
    // Bug Y / task #51: ghost-file detection in proof_worker_delta_step_result.
    // A "ghost file" is a Tablet/ entry present in `before_snapshot`
    // (on disk pre-burst) whose stem is NOT in `current_present_nodes`
    // (kernel state). These signal a prior burst's filesystem
    // mutations that never got rolled back. The validator must
    // reject them loudly.
    // ---------------------------------------------------------------

    /// Setup: minimal repo with `Preamble` + `main` ratified, then
    /// inject `Foo.lean` + `Foo.tex` on disk WITHOUT adding `Foo` to
    /// `current_present_nodes`. Snapshot the polluted state as
    /// `before_snapshot`, then have the worker modify `Foo.lean`
    /// (so it lands in `changes.modified`). The ghost gate must fire.
    #[test]
    fn proof_worker_delta_step_result_rejects_ghost_lean_files() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        // Pre-burst pollution: ghost file Foo on disk, NOT in kernel state.
        write(
            &repo.join("Tablet/Foo.lean"),
            "-- [TABLET NODE: Foo]\nimport Tablet.Preamble\n\ntheorem Foo : True := trivial\n",
        );
        write(
            &repo.join("Tablet/Foo.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);

        // Worker modifies Foo.lean (puts it in changes.modified).
        write(
            &repo.join("Tablet/Foo.lean"),
            "-- [TABLET NODE: Foo]\nimport Tablet.Preamble\n\ntheorem Foo : True := by trivial\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "main",
            &before_snapshot,
            &set(&["Preamble", "main"]), // Foo NOT present
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(
                "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := by\n  trivial\n",
                "main",
            ),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        assert!(!result.ok, "ghost file should be rejected");
        assert!(
            result.errors.iter().any(|err| err.contains("Foo")),
            "error should name the ghost file: {:?}",
            result.errors
        );
        assert!(
            result
                .errors
                .iter()
                .any(|err| err.contains("present_nodes") || err.contains("ratify")),
            "error should explain the ghost-file mechanism: {:?}",
            result.errors
        );
    }

    /// `header.tex` is regenerated by `sync_tablet_render_support_from_repo`
    /// every burst — it's auto-managed structural state, not a node, and
    /// must NOT trip the ghost gate when modified.
    #[test]
    fn proof_worker_delta_step_result_excludes_header_tex() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        write(&repo.join("Tablet/header.tex"), "% header v1\n");

        let before_snapshot = snapshot_tablet_dir(&repo);

        // Regen header.tex (puts it in changes.modified).
        write(&repo.join("Tablet/header.tex"), "% header v2\n");

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "main",
            &before_snapshot,
            &set(&["Preamble", "main"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(
                "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := by\n  trivial\n",
                "main",
            ),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        // header.tex regen must not trigger ghost rejection. (Other
        // checks may still produce errors; we only assert no error
        // mentions "header" as a ghost.)
        assert!(
            !result
                .errors
                .iter()
                .any(|err| err.contains("header") && err.contains("ratify")),
            "header.tex should not be flagged as a ghost: {:?}",
            result.errors
        );
    }

    /// Modifying the active node's `.lean` is normal worker behavior
    /// and must not trip the ghost gate even when the active node is
    /// momentarily not in `current_present_nodes` (defensive: kernel
    /// state could be transiently inconsistent).
    #[test]
    fn proof_worker_delta_step_result_excludes_active_node_even_if_not_in_present() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);

        // Worker modifies the active node's .lean.
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := trivial\n",
        );

        // Pass an EMPTY present_nodes set — defensive case.
        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "main",
            &before_snapshot,
            &BTreeSet::new(),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(
                "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := by\n  trivial\n",
                "main",
            ),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        // The active node must not be flagged as a ghost regardless
        // of present_nodes membership.
        assert!(
            !result
                .errors
                .iter()
                .any(|err| err.contains("main") && err.contains("ratify")),
            "active node should not be flagged as a ghost: {:?}",
            result.errors
        );
    }

    /// A worker that creates a `.tex` without a matching `.lean` (a
    /// stray) should be rejected with the existing stray_new_tex
    /// error, not the ghost error — the stray check fires first by
    /// design (preserves error precedence relative to existing
    /// callers/expectations).
    #[test]
    fn proof_worker_delta_step_result_ghost_rejection_precedence() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        write_stub_check_script(&repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        write(
            &repo.join("Tablet/main.lean"),
            "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := by\n  trivial\n",
        );
        write(
            &repo.join("Tablet/main.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        // Pre-burst ghost.
        write(
            &repo.join("Tablet/Foo.lean"),
            "-- [TABLET NODE: Foo]\nimport Tablet.Preamble\n\ntheorem Foo : True := trivial\n",
        );
        write(
            &repo.join("Tablet/Foo.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let before_snapshot = snapshot_tablet_dir(&repo);

        // Worker modifies the ghost AND creates a stray new .tex
        // (no matching .lean for Bar).
        write(
            &repo.join("Tablet/Foo.lean"),
            "-- [TABLET NODE: Foo]\nimport Tablet.Preamble\n\ntheorem Foo : True := by trivial\n",
        );
        write(
            &repo.join("Tablet/Bar.tex"),
            "\\begin{theorem}True.\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "main",
            &before_snapshot,
            &set(&["Preamble", "main"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            &declaration_hash(
                "-- [TABLET NODE: main]\nimport Tablet.Preamble\n\ntheorem main : True := by\n  trivial\n",
                "main",
            ),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            false,
        )
        .unwrap();

        assert!(!result.ok);
        // Precedence: stray_new_tex error fires first.
        assert!(
            result
                .errors
                .iter()
                .any(|err| err.contains("Bar") || err.contains("Unpaired")),
            "stray_new_tex error should fire first: {:?}",
            result.errors
        );
    }

    // ------ Patch B local-closure MCA gate -------------------------------
    //
    // Tests for the local-closure probe wiring in
    // `proof_worker_delta_step_result` when `must_close_active=true`.
    // The plan §6.1 contract is:
    //
    //   if must_close_active:
    //     if !evaluated.shallow_ok:                            reject [shallow]
    //     if kind(active) == Definition: accept (no probe)     [kind-aware]
    //     let local = run_local_closure_axioms(repo, active)?
    //     if local.timed_out:                                  reject [internal]
    //     if local.returncode != 0:                            reject [internal]
    //     if local.status != "ok":                             reject [internal]
    //     let approved = load_approved_axioms(repo, active)
    //     let violations = local.kernel_axioms − approved
    //     if !violations.is_empty():                           reject [axiom]
    //     if !local.errors.is_empty():                         reject [strict]
    //     accept (probe payload threaded into result)
    //
    // These tests use a configurable check.py stub that returns a
    // fixed local-closure payload (kernel_axioms / errors / status)
    // so we can drive each branch deterministically without depending
    // on the real Lean script. Patch A's smoke tests cover the script
    // itself end-to-end (under `tests/local_closure_smoke.rs`).
    //
    // The fixture shape mirrors the existing
    // `write_stub_check_script` pattern: the stub dispatches on
    // `argv[1]` and returns canned JSON. The local-closure variant
    // also reads its desired payload from a JSON file in the repo
    // (so tests can mutate it without re-writing the .py).

    fn write_local_closure_stub_check_script(repo: &Path) {
        // Stub reads `.trellis/scripts/local_closure_response.json`
        // when `local-closure-axioms` is requested. Tests pre-write
        // that file with the desired payload. Other subcommands use
        // the same canned responses as `write_stub_check_script`.
        let script = r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]
script_dir = Path(__file__).resolve().parent

if cmd == "lean-compile-node":
    print(json.dumps({
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "print-axioms":
    node = sys.argv[2]
    print(json.dumps({
        "returncode": 0,
        "stdout": f"{node} does not depend on any axioms\n",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "local-closure-axioms":
    node = sys.argv[2]
    # FIX 2 coverage: the `--scan-only` owner-file authoring gate is a
    # DIFFERENT op than the full closure probe (it runs only the scan,
    # never the closure walk / axcheck), so it reads its own canned
    # files (`local_closure_scan_response[_<node>].json`) and defaults
    # to a clean `ok`. This keeps the full-probe canned responses
    # (which model axiom/strict/closure outcomes) from being served
    # for a scan-only call, and vice versa.
    scan_only = "--scan-only" in sys.argv[3:]
    if scan_only:
        per_node_file = script_dir / f"local_closure_scan_response_{node}.json"
        response_file = script_dir / "local_closure_scan_response.json"
    else:
        # Patch C-R-c: check for a per-node response first, falling back
        # to the shared response. The broadened helper-probe trigger
        # (class a/b/c) probes both the active node and new helpers in
        # the same burst, so tests need to vary the canned response by
        # node to assert on which-node-failed.
        per_node_file = script_dir / f"local_closure_response_{node}.json"
        response_file = script_dir / "local_closure_response.json"
    chosen = per_node_file if per_node_file.exists() else response_file
    if chosen.exists():
        canned = json.loads(chosen.read_text(encoding="utf-8"))
        canned.setdefault("request_id", 0)
        canned.setdefault("node_name", node)
        canned.setdefault("returncode", 0)
        canned.setdefault("timed_out", False)
        canned.setdefault("stdout", "")
        canned.setdefault("stderr", "")
        canned.setdefault("status", "ok")
        canned.setdefault("kernel_axioms", [])
        canned.setdefault("boundary_theorems", [])
        canned.setdefault("strict_theorem_deps", [])
        canned.setdefault("strict_definition_deps", [])
        canned.setdefault("errors", [])
        print(json.dumps(canned))
    else:
        # Clean default: no axioms, no boundaries, no errors.
        print(json.dumps({
            "request_id": 0,
            "node_name": node,
            "returncode": 0,
            "timed_out": False,
            "stdout": "",
            "stderr": "",
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
        }))
elif cmd == "lean-semantic-payloads":
    repo = Path(sys.argv[2])
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    payloads = {}
    for node in nodes:
        lean_path = repo / "Tablet" / f"{node}.lean"
        payload = ""
        for line in lean_path.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if stripped.startswith(("theorem ", "lemma ", "def ", "abbrev ", "instance ", "structure ", "inductive ", "class ")):
                payload = stripped
                break
        payloads[node] = {"ok": True, "payload": payload, "error": ""}
    print(json.dumps(payloads))
elif cmd == "sync-tablet-support":
    print(json.dumps({
        "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }))
elif cmd == "materialize-tablet-oleans":
    print(json.dumps({
        "returncode": 0,
        "stdout": "materialized",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "prepare-compiled-support":
    print(json.dumps({
        "returncode": 0,
        "stdout": "prepared",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
else:
    raise SystemExit(f"unexpected subcommand: {cmd}")
"#;
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }

    /// Configure the canned local-closure response served by the stub
    /// check script. Pass `None` to clear the canned file (revert to
    /// the stub's "clean default").
    fn set_local_closure_canned_response(repo: &Path, canned: Option<&str>) {
        let path = repo.join(".trellis/scripts/local_closure_response.json");
        match canned {
            Some(json_text) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&path, json_text).unwrap();
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Patch C-R-c: per-node canned response. The stub script checks
    /// `local_closure_response_<node>.json` BEFORE the shared
    /// `local_closure_response.json`, so tests can target a specific
    /// node's probe without affecting other probes in the same burst.
    /// Necessary because the broadened helper-probe trigger probes
    /// both the active node and new helpers in many test fixtures.
    fn set_local_closure_canned_response_for_node(repo: &Path, node: &str, canned: Option<&str>) {
        let path = repo
            .join(".trellis/scripts")
            .join(format!("local_closure_response_{node}.json"));
        match canned {
            Some(json_text) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&path, json_text).unwrap();
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// FIX 2 coverage: per-node canned response for the `--scan-only`
    /// owner-file authoring gate. Distinct from the full-probe canned files
    /// so a scan-only response (reserved-shape rejection or clean `ok`)
    /// never leaks into the closure-probe path. The stub defaults scan-only
    /// to a clean `ok` when no scan file exists.
    fn set_local_closure_scan_response_for_node(repo: &Path, node: &str, canned: Option<&str>) {
        let path = repo
            .join(".trellis/scripts")
            .join(format!("local_closure_scan_response_{node}.json"));
        match canned {
            Some(json_text) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&path, json_text).unwrap();
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Set up a minimal lake repo with a single sorry-free active node A
    /// and a Preamble. Returns `(repo, before_snapshot, original_active_source)`.
    fn setup_closed_active(repo: &Path) -> (BTreeMap<String, String>, String) {
        write_local_closure_stub_check_script(repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let original_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n";
        write(&repo.join("Tablet/A.lean"), original_active);
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        let before_snapshot = snapshot_tablet_dir(repo);
        // Touch A.lean (same content) so the worker delta machinery
        // sees an "active changed" event — `must_close_active` runs the
        // probe only when the active node actually changes (or when
        // explicitly forced). Rewriting identical content still bumps
        // the snapshot hash via mtime semantics, but content is stable
        // for the probe and gate.
        write(&repo.join("Tablet/A.lean"), original_active);
        (before_snapshot, original_active.to_string())
    }

    /// Modify A.lean to a guaranteed-different content so the delta
    /// scope recognizes the active node as changed. Returns the new
    /// source so the test can recompute `expected_active_hash`. Same
    /// declaration signature so the lock check accepts it.
    fn touch_active_lean(repo: &Path, content: &str) {
        write(&repo.join("Tablet/A.lean"), content);
    }

    /// Rewrite the canonical Lean `A.lean` with a trailing `-- {tag}` comment
    /// (signature-stable) so the delta scope sees the active node as changed.
    /// The Lean analogue of `touch_active_isabelle`, for the Lean cross-check
    /// MCA tests (which set up a Lean repo via `setup_closed_active`).
    fn touch_active_lean_tagged(repo: &Path, tag: &str) {
        let new_active = format!(
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- {tag}\n"
        );
        touch_active_lean(repo, &new_active);
    }

    /// Call `proof_worker_delta_step_result` with the
    /// must_close_active=true preset commonly needed in these tests.
    ///
    /// The present_nodes set includes `Preamble`, `A` plus every Tablet
    /// dep name that any test's canned probe response references
    /// (`Helper`, `SmuggleDef`, `StrictThm`). Patch C-K Fix 1's
    /// present-node validation rejects probes whose dep keys aren't in
    /// `present_nodes`; existing tests crafted canned responses without
    /// the kernel-state setup, so the helper bakes the necessary nodes
    /// in here. Tests that want to exercise the new fail-closed path
    /// instead call `proof_worker_delta_step_result` directly with a
    /// custom present_nodes set.
    fn run_mca_gate(
        repo: &Path,
        before_snapshot: &BTreeMap<String, String>,
        original_active: &str,
    ) -> WorkerValidationStepResult {
        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        proof_worker_delta_step_result(
            repo,
            "A",
            before_snapshot,
            &set(&["Preamble", "A", "Helper", "SmuggleDef", "StrictThm"]),
            &BTreeSet::new(), // declared_deleted_nodes
            &BTreeMap::new(), // current_node_kinds (Patch C-N item 1) — empty skips kind validation
            &BTreeSet::new(), // current_open_nodes (Patch C-R) — empty: no pre-delta sorryd info, helper probe fires on new births only
            // R4: route the expected active-node signature hash on target so the
            // declaration-lock compares like-for-like. For Lean this is the test
            // shim's `declaration_hash` (byte-identical to the prior call); for
            // an IsabelleHol repo it is `isabelle_filespec::signature_hash`.
            &declaration_hash_for_gate(repo, original_active, "A").unwrap_or_default(),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true, // allow_new_obligations: irrelevant when no new helpers
            true, // must_close_active (the gate under test)
        )
        .unwrap()
    }

    /// Test 1 (plan §6.4): sorryd helper allowed.
    /// The active node imports an open helper; the boundary-cut
    /// semantics treat the helper as a boundary theorem and do not
    /// propagate its `sorryAx`. The probe reports `kernel_axioms=[]`,
    /// `boundary_theorems=[Helper]`, `errors=[]`. The gate accepts and
    /// stashes the probe payload in `local_closure_results`.
    #[test]
    fn patch_b_mca_accepts_when_helper_is_sorryd_but_local_closure_clean() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        // Mutate A.lean so the worker delta gets a real change to gate.
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- minor edit\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":[],"boundary_theorems":[{"name":"Tablet.Helper","statement_hash":"abc"}],"strict_theorem_deps":[],"strict_definition_deps":[],"errors":[]}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);

        assert!(
            result.ok,
            "MCA gate must accept when boundary-cut keeps kernel_axioms clean: {:?}",
            result.errors,
        );
        assert!(
            result.local_closure_results.contains_key("A"),
            "accept path must stash probe payload keyed by active node, got: {:?}",
            result.local_closure_results.keys().collect::<Vec<_>>(),
        );
        let probe = &result.local_closure_results["A"];
        assert_eq!(probe.status, "ok");
        assert!(probe.kernel_axioms.is_empty());
        assert!(
            probe.boundary_theorems.contains_key("Helper"),
            "boundary_theorems must contain Helper after Tablet. prefix strip; got {:?}",
            probe.boundary_theorems.keys().collect::<Vec<_>>(),
        );
    }

    /// Test 2 (plan §6.4): active sorry rejected with `[shallow]`.
    /// The MCA gate's shallow check fires before the probe is ever
    /// invoked; the resulting rejection carries the existing "no sorry
    /// in its own file" message and `local_closure_results` stays empty.
    #[test]
    fn patch_b_mca_rejects_active_sorry_with_shallow_error() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        // Reintroduce sorry into the active node.
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := by\n  sorry\n";
        touch_active_lean(&repo, new_active);

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);

        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("no `sorry` in its own file")),
            "shallow rejection must mention the no-sorry contract: {:?}",
            result.errors,
        );
        assert!(
            result.local_closure_results.is_empty(),
            "shallow rejection must not stash a probe payload",
        );
    }

    /// Test 3 (plan §6.4 + variant on hidden sorryAx). Construct the
    /// scenario via the stub: probe reports `kernel_axioms=["sorryAx"]`.
    /// Gate rejects with `[axiom]`.
    #[test]
    fn patch_b_mca_rejects_hidden_sorry_ax_with_axiom_error() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- v2\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response(
            &repo,
            Some(r#"{"status":"ok","kernel_axioms":["sorryAx"]}"#),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);

        assert!(!result.ok);
        assert!(
            result.errors.iter().any(|e| e.starts_with("[axiom]")),
            "[axiom] rejection expected, got: {:?}",
            result.errors,
        );
        assert!(
            result.errors.iter().any(|e| e.contains("sorryAx")),
            "rejection message must name the offending axiom: {:?}",
            result.errors,
        );
    }

    /// Test 4 (plan §6.4): definition smuggling.
    /// `def D := Classical.choose openHelper`; A's proof uses D; the
    /// probe walks D.value strictly and surfaces `sorryAx`. The gate
    /// rejects with `[axiom]` listing `sorryAx`. The stub replicates
    /// the probe's would-be observation.
    #[test]
    fn patch_b_mca_rejects_definition_smuggling_via_axiom_path() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.SmuggleDef\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n";
        touch_active_lean(&repo, new_active);
        // Simulate a SmuggleDef whose body Strict-walk would surface
        // sorryAx. Real Lean script would walk D.value Strict; we
        // model the observation.
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":["sorryAx"],"strict_definition_deps":[{"name":"Tablet.SmuggleDef","semantic_hash":"def"}]}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);

        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[axiom]") && e.contains("sorryAx")),
            "definition smuggling must reject via [axiom]/sorryAx: {:?}",
            result.errors,
        );
    }

    /// Test 5 (plan §6.4): theorem-statement smuggling.
    /// `A`'s statement embeds `Classical.choose openHelper` etc; the
    /// probe walks A.type Strict and surfaces sorryAx via the type.
    /// Stub replicates the observation; gate rejects with `[axiom]`.
    ///
    /// Implementation note: the deterministic gate locks the active
    /// node's declaration head hash against `expected_active_hash`
    /// in non-restructure modes, so the synthetic test reuses the
    /// original statement and lets the stub assert the would-be
    /// observation. A real statement-smuggling burst is a Restructure
    /// or CoarseRestructure flow — that mode-specific path is covered
    /// by the existing signature-lock tests. What we're proving here
    /// is that when the probe surfaces sorryAx, the gate emits the
    /// expected `[axiom]/sorryAx` shape regardless of whether the
    /// surface was a body change or a type change.
    #[test]
    fn patch_b_mca_rejects_theorem_statement_smuggling_via_axiom_path() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        // Keep the signature stable (signature-lock is a separate
        // contract). Mutate the body comment so the delta machinery
        // sees an active change; the probe's stubbed kernel_axioms
        // simulates what the script would emit if A.type had embedded
        // sorryAx-bearing constants.
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- statement smuggling synthetic v5\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response(
            &repo,
            Some(r#"{"status":"ok","kernel_axioms":["sorryAx"]}"#),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);

        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[axiom]") && e.contains("sorryAx")),
            "statement smuggling must reject via [axiom]/sorryAx: {:?}",
            result.errors,
        );
    }

    /// Test 6 (plan §6.4): `native_decide` rejected.
    /// `native_decide` introduces `Lean.ofReduceBool`. The probe
    /// observation is simulated; gate rejects with `[axiom]` listing
    /// `Lean.ofReduceBool`.
    ///
    /// Implementation note: writing literal `native_decide` into the
    /// source triggers the existing `FORBIDDEN_KEYWORDS` keyword scan
    /// (`runtime_cli_observations.rs:31`), which fires before the probe
    /// in `evaluate_node_observation`. The keyword scan is a separate
    /// pre-existing contract that already covers the `native_decide`
    /// source path; this test exercises the *probe-driven* `[axiom]`
    /// path by simulating what the Lean script would emit if the
    /// `native_decide` was hidden behind macro expansion or a helper
    /// whose source the keyword scan can't see.
    #[test]
    fn patch_b_mca_rejects_native_decide_via_axiom_path() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        // Same signature; benign-looking body. The stub simulates
        // the probe surfacing `Lean.ofReduceBool` (e.g., because a
        // helper used `native_decide` and the kernel-axiom collector
        // walked through it).
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- ofReduceBool surfaced via dep v6\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response(
            &repo,
            Some(r#"{"status":"ok","kernel_axioms":["Lean.ofReduceBool"]}"#),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);

        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[axiom]") && e.contains("Lean.ofReduceBool")),
            "native_decide-induced axiom must reject via [axiom]/Lean.ofReduceBool: {:?}",
            result.errors,
        );
    }

    /// Test 7 (plan §6.4): approved kernel axioms accepted.
    /// Proof uses only the canonical four (`propext`, `funext`,
    /// `Classical.choice`, `Quot.sound`); these are seeded into
    /// `load_approved_axioms`'s default set. Gate accepts.
    #[test]
    fn patch_b_mca_accepts_proof_using_only_canonical_four_axioms() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- v3\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":["propext","funext","Classical.choice","Quot.sound"]}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);

        assert!(
            result.ok,
            "canonical four must be admitted by load_approved_axioms's default set: {:?}",
            result.errors,
        );
        assert_eq!(result.local_closure_results["A"].kernel_axioms.len(), 4,);
    }

    /// Test 8 (plan §6.4): project axiom rejected without waiver,
    /// accepted with per-node waiver.
    /// First half: probe reports `kernel_axioms=["UnapprovedProjectAxiom"]`;
    /// no waiver in APPROVED_AXIOMS.json → `[axiom]` rejection.
    /// Second half: add per-node waiver to APPROVED_AXIOMS.json under
    /// `nodes.A` → accept.
    #[test]
    fn patch_b_mca_rejects_project_axiom_then_accepts_with_per_node_waiver() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- v4\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response(
            &repo,
            Some(r#"{"status":"ok","kernel_axioms":["UnapprovedProjectAxiom"]}"#),
        );

        // First call: no waiver → reject.
        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[axiom]") && e.contains("UnapprovedProjectAxiom")),
            "must list the unapproved axiom: {:?}",
            result.errors,
        );

        // Add a per-node waiver — only for "A".
        write(
            &repo.join("APPROVED_AXIOMS.json"),
            r#"{"global":[],"nodes":{"A":["UnapprovedProjectAxiom"]}}"#,
        );
        let result_with_waiver = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(
            result_with_waiver.ok,
            "per-node waiver for A must admit the axiom: {:?}",
            result_with_waiver.errors,
        );
        // Sanity: verify the waiver did NOT bleed into another node's
        // approved set. `load_approved_axioms(repo, "B")` should NOT
        // include UnapprovedProjectAxiom.
        let approved_b = load_approved_axioms(&repo, "B").unwrap();
        assert!(
            !approved_b.contains("UnapprovedProjectAxiom"),
            "per-node waiver for A must not bleed into B's approved set: {:?}",
            approved_b,
        );
        let approved_a = load_approved_axioms(&repo, "A").unwrap();
        assert!(
            approved_a.contains("UnapprovedProjectAxiom"),
            "per-node waiver must be in A's approved set: {:?}",
            approved_a,
        );
    }

    // ===================================================================
    // B2c-gate Slice 2 — IsabelleHol accept clauses (adversarial)
    // ===================================================================
    //
    // These mirror the `[axiom]`/`sorryAx` MCA stub tests above but for an
    // IsabelleHol-targeted repo (a `trellis.config.json` selecting
    // `default_target = isabelle_hol` ⇒ `tablet_target_for_repo` resolves to
    // IsabelleHol, activating the server-cert reject arms). The probe payload
    // carries the Slice-1 cert fields (`oracles_used`/`extra_shyps`/
    // `theorem_exists`). The active-node delta machinery is `.lean`-keyed
    // (Slice-3/R4 territory), so these reuse the Lean-style `.lean` repo for
    // change detection; the gate under test reads the PROBE, not the source.

    /// An IsabelleHol-routing stub `check.py`: dispatches the `isabelle-*`
    /// subcommands (`checker_driver_for(IsabelleHol)` op strings) so the
    /// upstream `observe_node` compile (`isabelle-check-node`), the session
    /// materialization (`isabelle-build-session`/`isabelle-sync-session`), and
    /// the probe (`isabelle-thm-deps`) all resolve. The probe reads the SAME
    /// canned `local_closure_response.json` the Lean stub uses, so the
    /// existing `set_local_closure_canned_response` helper drives the
    /// server-cert payload. `isabelle-check-node` returns RC0 (clean compile)
    /// so the upstream shallow/shape evaluation passes through to the gate.
    fn write_isabelle_stub_check_script(repo: &Path) {
        let script = r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]
script_dir = Path(__file__).resolve().parent

if cmd in ("isabelle-check-node",):
    print(json.dumps({
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd in ("isabelle-build-session", "isabelle-sync-session"):
    print(json.dumps({
        "returncode": 0,
        "stdout": "ok",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
        "updated_paths": [],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }))
elif cmd == "isabelle-thm-oracles":
    node = sys.argv[2]
    print(json.dumps({
        "returncode": 0,
        "stdout": f"{node} does not depend on any axioms\n",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }))
elif cmd == "isabelle-thm-deps":
    node = sys.argv[2]
    scan_only = "--scan-only" in sys.argv[3:]
    if scan_only:
        per_node_file = script_dir / f"local_closure_scan_response_{node}.json"
        response_file = script_dir / "local_closure_scan_response.json"
    else:
        per_node_file = script_dir / f"local_closure_response_{node}.json"
        response_file = script_dir / "local_closure_response.json"
    chosen = per_node_file if per_node_file.exists() else response_file
    if chosen.exists():
        canned = json.loads(chosen.read_text(encoding="utf-8"))
        canned.setdefault("request_id", 0)
        canned.setdefault("node_name", node)
        canned.setdefault("returncode", 0)
        canned.setdefault("timed_out", False)
        canned.setdefault("stdout", "")
        canned.setdefault("stderr", "")
        canned.setdefault("status", "ok")
        canned.setdefault("kernel_axioms", [])
        canned.setdefault("oracles_used", [])
        canned.setdefault("extra_shyps", [])
        canned.setdefault("theorem_exists", True)
        canned.setdefault("boundary_theorems", [])
        canned.setdefault("strict_theorem_deps", [])
        canned.setdefault("strict_definition_deps", [])
        canned.setdefault("errors", [])
        print(json.dumps(canned))
    else:
        print(json.dumps({
            "request_id": 0,
            "node_name": node,
            "returncode": 0,
            "timed_out": False,
            "stdout": "",
            "stderr": "",
            "status": "ok",
            "kernel_axioms": [],
            "oracles_used": [],
            "extra_shyps": [],
            "theorem_exists": True,
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
        }))
else:
    raise SystemExit(f"unexpected subcommand: {cmd}")
"#;
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }

    /// The canonical sorry-free Isabelle active node `A`: a real
    /// `Tablet/A.thy` (`theory Tablet_A imports Tablet_Preamble begin theorem
    /// A: … proof … qed end`) plus its `.tex`. `tag` is an Isabelle `(* … *)`
    /// comment appended AFTER `qed` (before `end`) — i.e. OUTSIDE the
    /// `[theorem A: .. proof]` signature span — so the `signature_hash`
    /// declaration-lock stays stable across edits.
    fn isabelle_active_thy(tag: &str) -> String {
        format!(
            "theory Tablet_A\n  imports Tablet_Preamble\nbegin\n\ntheorem A:\n  fixes n :: nat\n  shows \"n + 0 = n\"\nproof -\n  show \"n + 0 = n\" by simp\nqed\n(* {tag} *)\n\nend\n"
        )
    }

    /// A real-`.thy` Isabelle fixture (R4): the `default_target=isabelle_hol`
    /// config so the gate takes the IsabelleHol branch (drop the axiom ceiling;
    /// run the cert gate), the IsabelleHol-routing stub check script so the
    /// per-node ops resolve to the `isabelle-*` subcommands, and the active node
    /// as a genuine `Tablet/A.thy` (NO `.lean` — `observe_node` reads the `.thy`
    /// per the R4 A1 routing). Returns `(before_snapshot, original_active_thy)`.
    fn setup_closed_active_isabelle(repo: &Path) -> (BTreeMap<String, String>, String) {
        write_isabelle_stub_check_script(repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(
            &repo.join("trellis.config.json"),
            r#"{"workflow":{"default_target":"isabelle_hol"}}"#,
        );
        // Isabelle Preamble theory + its `.tex` (backend-agnostic statement).
        write(
            &repo.join("Tablet/Preamble.thy"),
            "theory Tablet_Preamble\n  imports Main\nbegin\n\nend\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let original_active = isabelle_active_thy("orig");
        write(&repo.join("Tablet/A.thy"), &original_active);
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Trivial.\\end{proof}\n",
        );
        // Snapshot AFTER the `.thy` exists so the worker delta sees the active
        // node only as "changed" when `touch_active_isabelle` rewrites it.
        let before_snapshot = snapshot_tablet_dir(repo);
        (before_snapshot, original_active)
    }

    /// Rewrite `Tablet/A.thy` with a changed `(* tag *)` (signature-stable, see
    /// `isabelle_active_thy`) so the delta scope recognizes the active node as
    /// changed. Returns the new `.thy` source.
    fn touch_active_isabelle(repo: &Path, tag: &str) -> String {
        let new_active = isabelle_active_thy(tag);
        write(&repo.join("Tablet/A.thy"), &new_active);
        new_active
    }

    // ========================================================================
    // R4 Lean BYTE-IDENTITY pins (R-HOT). The node-eval routing selects every
    // site via `*_for(tablet_target_for_repo(repo))`, which is `Lean` on the
    // all-Lean live run. These pin that the routed sites behave EXACTLY as the
    // historical hardcoded `.lean` path for Lean. `contract_baseline` is the
    // wire-level zero-diff guard; these are the direct-call guards.
    // ========================================================================

    /// `node_source_path(repo, node, Lean)` is byte-identical to the historical
    /// hardcoded `Tablet/<node>.lean`. The A1 / A3 / A4 / C5 re-reads all route
    /// through this; if the Lean extension ever drifted, every Lean read would
    /// move off `.lean`.
    #[test]
    fn r4_node_source_path_lean_equals_lean_path() {
        let repo = Path::new("/some/repo");
        assert_eq!(
            trellis_kernel::node_source_path(repo, "Foo", trellis_kernel::backend::BackendId::Lean),
            repo.join("Tablet").join("Foo.lean"),
        );
    }

    /// `proof_worker_delta_scope` with `Lean` produces the exact historical
    /// scope: `.lean` create/modify/delete and `.tex` arms classify as before.
    /// Pins the B1–B5 filename-filter routing for Lean (R-SCOPE-SIG round-trip).
    #[test]
    fn r4_proof_worker_delta_scope_lean_roundtrip_identical() {
        let changes = SnapshotChanges {
            created: vec!["NewHelper.lean".to_string(), "NewHelper.tex".to_string()],
            modified: vec![
                "main.lean".to_string(),
                "neighbor.lean".to_string(),
                "ghost.lean".to_string(),
            ],
            deleted: vec!["gone.lean".to_string()],
        };
        let present = set(&["main", "neighbor", "gone"]);
        let scope = proof_worker_delta_scope(
            &changes,
            "main",
            &present,
            WorkerProofDeltaMode::CoarseRestructure,
            &set(&["neighbor"]),
            trellis_kernel::backend::BackendId::Lean,
        );
        // active main.lean modified ⇒ active_changed.
        assert!(scope.active_changed);
        // NewHelper.{lean,tex} paired ⇒ a new lean file, no stray tex.
        assert_eq!(scope.new_lean_files, vec!["NewHelper".to_string()]);
        assert!(scope.stray_new_tex.is_empty());
        // neighbor authorized, ghost not present ⇒ ghost is a ghost-file.
        assert_eq!(
            scope.authorized_extra_changed_existing_nodes,
            vec!["neighbor".to_string()]
        );
        assert_eq!(scope.ghost_node_files, vec!["ghost".to_string()]);
        // gone.lean deleted and was present ⇒ deleted_existing_nodes.
        assert_eq!(scope.deleted_existing_nodes, vec!["gone".to_string()]);

        // Cross-check: the SAME change-set under IsabelleHol sees NONE of the
        // `.lean` files as node-source changes (its source ext is `.thy`), so
        // the Lean and Isabelle classifications are provably distinct — the
        // routing is live, and the Lean arm above is the unchanged historical
        // behavior.
        let iso = proof_worker_delta_scope(
            &changes,
            "main",
            &present,
            WorkerProofDeltaMode::CoarseRestructure,
            &set(&["neighbor"]),
            trellis_kernel::backend::BackendId::IsabelleHol,
        );
        assert!(!iso.active_changed, "main.lean is not a .thy node-source");
        assert!(iso.new_lean_files.is_empty());
        // NewHelper.tex with no matching .thy ⇒ a stray tex under Isabelle.
        assert_eq!(iso.stray_new_tex, vec!["NewHelper".to_string()]);
        assert!(iso.deleted_existing_nodes.is_empty());
    }

    /// `evaluate_node_observation` on a Lean `setup_closed_active` node yields
    /// the unchanged Lean-derived fields (the pre/post-refactor pin the plan
    /// calls for). The marker / declaration-name / declaration-kind / noderef
    /// helpers are all routed now; for Lean they must produce the historical
    /// values. (`compile`/`print_axioms` are left as the default empty
    /// observation — so `compiles` is false and the axiom audit is skipped,
    /// exactly as the historical body does for an un-populated observation;
    /// this isolates the ROUTED text-derived fields from the I/O-derived ones.)
    #[test]
    fn r4_evaluate_node_observation_lean_fields_pinned() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (_before, _original) = setup_closed_active(&repo);
        let observation = observation_from_disk_for_test(&repo, "A");
        let evaluated = evaluate_node_observation(&repo, &observation, None);
        // C1/C2 (Lean arm): the `-- [TABLET NODE: A]` marker + the `theorem A`
        // declaration-head scan both resolve to "A".
        assert!(
            evaluated.marker_valid,
            "Lean marker scan must match: {:?}",
            evaluated.errors
        );
        assert!(evaluated.declaration_name_matches);
        // Routed text gates: imports clean, no forbidden keyword, no .tex
        // format error, declaration intact (no expected-hash).
        assert!(evaluated.imports_valid);
        assert!(evaluated.keyword_clean);
        assert!(evaluated.tex_format_valid);
        assert!(evaluated.declaration_intact);
        // C5 noderef: the `.tex` cites no `\noderef`, so no closure error.
        assert!(
            !evaluated
                .errors
                .iter()
                .any(|e| e.contains("Lean import closure")),
            "no spurious noderef error: {:?}",
            evaluated.errors
        );
        // The ONLY error on this lake-less fixture is the (expected) compile
        // failure from the empty compile observation — proving the routed text
        // helpers contributed NO errors of their own.
        assert!(
            evaluated
                .errors
                .iter()
                .all(|e| e.starts_with("Compilation failed")),
            "the routed text helpers must contribute no errors for a clean Lean node; got: {:?}",
            evaluated.errors
        );
        // Serialization round-trips with the routed booleans true.
        let json = serde_json::to_string(&evaluated).unwrap();
        assert!(json.contains("\"marker_valid\":true"));
        assert!(json.contains("\"declaration_name_matches\":true"));
    }

    /// (i) `oracles_used:["z3"]` ⇒ `[oracle]` reject. The PRIMARY soundness
    /// catcher — an external-solver oracle the build does NOT catch.
    #[test]
    fn isabelle_mca_rejects_z3_oracle_with_oracle_error() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active_isabelle(&repo);
        touch_active_isabelle(&repo, "iz3");
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":[],"oracles_used":["z3"],"theorem_exists":true}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(!result.ok, "z3 oracle must be rejected");
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[oracle]") && e.contains("z3")),
            "[oracle] rejection listing z3 expected, got: {:?}",
            result.errors,
        );
        assert!(
            result.local_closure_results.is_empty(),
            "oracle rejection must not stash a probe payload",
        );
    }

    /// (ii) `oracles_used:["skip_proof"]` ⇒ `[oracle]` reject. `skip_proof`
    /// is the `sorry`/`\<proof>` oracle; the build returns RC0 on a `sorry`
    /// over `use_theories`, so the oracle gate is the ONLY catcher.
    #[test]
    fn isabelle_mca_rejects_skip_proof_oracle_with_oracle_error() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active_isabelle(&repo);
        touch_active_isabelle(&repo, "iskip");
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":[],"oracles_used":["skip_proof"],"theorem_exists":true}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(!result.ok, "skip_proof oracle must be rejected");
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[oracle]") && e.contains("skip_proof")),
            "[oracle] rejection listing skip_proof expected, got: {:?}",
            result.errors,
        );
    }

    /// (iii) `extra_shyps:["{}"]` ⇒ `[shyps]` reject. The empty-sort /
    /// inconsistent-class vacuous-`False` channel (oracle-blind, build-clean).
    #[test]
    fn isabelle_mca_rejects_extra_shyps_with_shyps_error() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active_isabelle(&repo);
        touch_active_isabelle(&repo, "ishyps");
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":[],"oracles_used":[],"extra_shyps":["{}"],"theorem_exists":true}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(!result.ok, "non-empty extra_shyps must be rejected");
        assert!(
            result.errors.iter().any(|e| e.starts_with("[shyps]")),
            "[shyps] rejection expected, got: {:?}",
            result.errors,
        );
    }

    /// (iv) `theorem_exists:false` ⇒ `[theorem]` reject. Catches oops/elision
    /// / name-shadow — the server cert could not resolve the pinned theorem.
    #[test]
    fn isabelle_mca_rejects_missing_theorem_with_theorem_error() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active_isabelle(&repo);
        touch_active_isabelle(&repo, "ithm");
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":[],"oracles_used":[],"extra_shyps":[],"theorem_exists":false}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(!result.ok, "theorem_exists=false must be rejected");
        assert!(
            result.errors.iter().any(|e| e.starts_with("[theorem]")),
            "[theorem] rejection expected, got: {:?}",
            result.errors,
        );
    }

    /// (v) CLEAN accept proving the AXIOM-CEILING DROP: empty oracles/shyps,
    /// `theorem_exists:true`, and a BIG `kernel_axioms` set that is NOT a
    /// subset of the canonical four (definitional axioms — `*_def`,
    /// `type_definition_*`, the foundational HOL primitives). For a Lean
    /// target this exact payload would `[axiom]`-reject; for IsabelleHol the
    /// ceiling is gated off, so it ACCEPTS and stashes the probe.
    #[test]
    fn isabelle_mca_accepts_clean_with_big_definitional_axiom_set() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active_isabelle(&repo);
        touch_active_isabelle(&repo, "iclean");
        // A big definitional axiom set, none of which are the Lean canonical
        // four (so it would trip the Lean `[axiom]` ceiling).
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":["HOL.refl","HOL.ext","Nat.Suc_Rep_inject","List.list.size_def","Groups.plus_class.plus_def","type_definition_prod","Hilbert_Choice.someI"],"oracles_used":[],"extra_shyps":[],"theorem_exists":true,"statement_hash":"abc123"}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(
            result.ok,
            "IsabelleHol must ACCEPT a clean cert with a big definitional axiom set (axiom ceiling dropped): {:?}",
            result.errors,
        );
        assert!(
            result.local_closure_results.contains_key("A"),
            "accept path must stash the probe payload keyed by the active node",
        );
        let probe = &result.local_closure_results["A"];
        assert_eq!(
            probe.kernel_axioms.len(),
            7,
            "the big definitional axiom set must survive into the stashed probe (ceiling not applied)",
        );
        assert!(probe.oracles_used.is_empty());
        assert!(probe.extra_shyps.is_empty());
        assert!(probe.theorem_exists);
        assert_eq!(probe.statement_hash, "abc123");
    }

    /// Cross-check the SAME big-axiom payload on a LEAN repo `[axiom]`-rejects
    /// — pins that the ceiling drop is IsabelleHol-only (Lean unchanged).
    #[test]
    fn lean_mca_rejects_same_big_axiom_set_proving_ceiling_is_isabelle_only() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        // setup_closed_active WITHOUT the isabelle config ⇒ Lean target.
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        touch_active_lean_tagged(&repo, "leanbig");
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":["HOL.refl","HOL.ext","Nat.Suc_Rep_inject"],"oracles_used":[],"extra_shyps":[],"theorem_exists":true}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(
            !result.ok,
            "Lean must still apply the axiom ceiling to a non-canonical axiom set",
        );
        assert!(
            result.errors.iter().any(|e| e.starts_with("[axiom]")),
            "[axiom] rejection expected on Lean, got: {:?}",
            result.errors,
        );
    }

    /// Lean carries the new cert fields EMPTY (never emitted), so a Lean
    /// probe with `oracles_used:["z3"]` is impossible — but to PIN that the
    /// oracle arm is IsabelleHol-gated and does not leak to Lean, feed a Lean
    /// repo a probe that DOES carry `oracles_used:["z3"]` and confirm it does
    /// NOT produce an `[oracle]` reject (it is accepted on the axiom path —
    /// the oracle field is inert for Lean).
    #[test]
    fn lean_mca_ignores_oracles_used_field_no_oracle_gate() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        touch_active_lean_tagged(&repo, "leanorc");
        set_local_closure_canned_response(
            &repo,
            // Lean target: empty kernel_axioms (passes the ceiling), but a
            // z3 oracle in the field. The Lean path must NOT consult it.
            Some(r#"{"status":"ok","kernel_axioms":[],"oracles_used":["z3"]}"#),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(
            result.ok,
            "Lean must ignore the oracles_used field (no oracle gate for Lean): {:?}",
            result.errors,
        );
        assert!(
            !result.errors.iter().any(|e| e.starts_with("[oracle]")),
            "Lean must NOT emit an [oracle] reject: {:?}",
            result.errors,
        );
    }

    // ========================================================================
    // R4.4 END-TO-END: a real `.thy` node flows through observe_node →
    // evaluate_node_observation → the B2c cert gate. The whole point of R4 —
    // a `.thy` with no `.lean` is observed, passes the node-eval early-reject,
    // and reaches the soundness gate.
    // ========================================================================

    /// (1) `observe_node` on a real-`.thy` Isabelle repo yields `lean_exists ==
    /// true` with the `.thy` content, and (2) `evaluate_node_observation` flows
    /// PAST the `!errors.is_empty()` early-reject (no `.thy`-not-found, no
    /// spurious marker / declaration-name / axiom-audit error).
    #[test]
    fn r4_thy_node_observed_and_flows_past_early_reject() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (_before, original_active) = setup_closed_active_isabelle(&repo);

        // (1) observed: lean_exists true, content is the `.thy`.
        let observation = observe_node(&repo, "A").unwrap();
        assert!(
            observation.lean_exists,
            "the `.thy` active node must be observed as existing (R4 A1 routing)",
        );
        assert!(
            observation.lean_path.ends_with("A.thy"),
            "observe_node must point at the `.thy` source, got {}",
            observation.lean_path,
        );
        assert_eq!(
            observation.lean_content, original_active,
            "observed content must be the `.thy` source",
        );
        assert!(observation.tex_exists);

        // (2) flows past the early-reject: evaluate_node_observation produces
        // NO node-eval errors (no `.thy not found`, no marker / decl-name /
        // shape / axiom-audit false-rejection). `compiles` is true via the
        // `isabelle-check-node` RC0 stub.
        let evaluated = evaluate_node_observation(&repo, &observation, None);
        assert!(
            evaluated.errors.is_empty(),
            "a well-formed `.thy` must produce no node-eval errors (flows to the cert gate); got: {:?}",
            evaluated.errors,
        );
        assert!(
            evaluated.marker_valid,
            "C1: marker vacuous-pass for Isabelle"
        );
        assert!(
            evaluated.declaration_name_matches,
            "C2: decl-name vacuous-pass"
        );
        assert!(
            evaluated.axioms_valid,
            "C9: axiom audit vacuous-true for Isabelle"
        );
        assert!(evaluated.compiles, "isabelle-check-node stub returns RC0");
        assert!(evaluated.sorry_free, "the canonical `.thy` has no sorry");
        assert!(
            evaluated.ok,
            "the `.thy` node must be accepted by evaluate_node_observation"
        );
    }

    /// (3) The full gate: a clean cert (`oracles_used:[]`, `extra_shyps:[]`,
    /// `theorem_exists:true`) over a real `.thy` ⇒ `result.ok`, with the probe
    /// stashed. (This is the end-to-end accept the cert gate guards — reached
    /// only because R4 routed the `.thy` to it.)
    #[test]
    fn r4_thy_clean_cert_accepts_end_to_end() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active_isabelle(&repo);
        touch_active_isabelle(&repo, "clean");
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":["HOL.refl"],"oracles_used":[],"extra_shyps":[],"theorem_exists":true,"statement_hash":"sh"}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(
            result.ok,
            "a clean cert over a real `.thy` must accept end-to-end: {:?}",
            result.errors,
        );
        assert!(
            result.local_closure_results.contains_key("A"),
            "the accept path must stash the probe payload keyed by the active node",
        );
    }

    /// (4) The oracle gate over a real `.thy`. The `.thy` is TEXTUALLY sorry-
    /// free (so it passes node-eval and the shallow-closure check) but the
    /// server cert reports `oracles_used:["skip_proof"]` — the `sorry`/`\<proof>`
    /// oracle that the build returns RC0 on and the textual scan CANNOT see. The
    /// cert's ORACLE channel is the only catcher, and it fires over a genuine
    /// `.thy` that flowed all the way through node-eval to the gate. (A LITERAL
    /// `sorry` token in the `.thy` would instead be caught earlier by the
    /// textual sorry-free check — that is the shallow gate, not this one.)
    #[test]
    fn r4_thy_skip_proof_oracle_rejected_end_to_end() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active_isabelle(&repo);
        // Sorry-free `.thy` (the canonical proof body) — passes the shallow
        // textual check so the flow reaches the cert gate.
        touch_active_isabelle(&repo, "skipproof");
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":[],"oracles_used":["skip_proof"],"extra_shyps":[],"theorem_exists":true}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);
        assert!(!result.ok, "a skip_proof-oracle `.thy` must be rejected");
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[oracle]") && e.contains("skip_proof")),
            "the skip_proof oracle must be caught by the [oracle] gate over a real `.thy`, got: {:?}",
            result.errors,
        );
        // The rejection is the SOUNDNESS gate, not an early node-eval reject:
        // no `.thy not found` / shape error / shallow-closure message.
        assert!(
            !result.errors.iter().any(|e| {
                e.contains("not found")
                    || e.contains("shape errors")
                    || e.contains("Lean-closed with no `sorry`")
            }),
            "rejection must be the cert ORACLE gate, not a node-eval / shallow early-reject: {:?}",
            result.errors,
        );
    }

    /// (5) Byte-identity sibling: the SAME fixture shape with the isabelle
    /// config ABSENT and a `.lean` node behaves exactly as the historical Lean
    /// path — `observe_node` reads `.lean`, and a `.thy` placed in the repo is
    /// IGNORED (the Lean target's source ext is `.lean`). Proves the routing is
    /// config-gated and the live (config-absent) run is untouched.
    #[test]
    fn r4_lean_sibling_ignores_thy_and_reads_lean() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        // Lean fixture (NO isabelle config) — the live-run shape.
        let (_before, lean_active) = setup_closed_active(&repo);
        // Drop a `.thy` alongside the `.lean`; the Lean target must ignore it.
        write(
            &repo.join("Tablet/A.thy"),
            "theory Tablet_A\n  imports Tablet_Preamble\nbegin\ntheorem A: \"True\" by simp\nend\n",
        );

        let observation = observe_node(&repo, "A").unwrap();
        assert!(observation.lean_exists);
        assert!(
            observation.lean_path.ends_with("A.lean"),
            "Lean target must read `.lean`, not the sibling `.thy`: {}",
            observation.lean_path,
        );
        assert_eq!(
            observation.lean_content, lean_active,
            "Lean target must observe the `.lean` content, ignoring the `.thy`",
        );
        // `node_source_path(Lean)` is the `.lean` path — the routing is
        // config-gated; the stray `.thy` does not perturb it.
        assert_eq!(
            trellis_kernel::node_source_path(&repo, "A", trellis_kernel::backend::BackendId::Lean),
            repo.join("Tablet").join("A.lean"),
        );
    }

    /// The LEAN-BYTE-IDENTICAL pin: `LocalClosureProbeOutput::default()`
    /// serializes WITHOUT the three new Slice-2 keys (`extra_shyps`,
    /// `statement_hash`, `theorem_exists`) — they are `skip_serializing_if`
    /// empty/empty/false. The Lean checker never emits them, so the default
    /// (empty) shape stays off the wire, exactly like `oracles_used`. A
    /// regression here would change the Lean JSON and break `contract_baseline`.
    #[test]
    fn local_closure_probe_default_has_no_new_keys_on_wire() {
        let json = serde_json::to_string(&LocalClosureProbeOutput::default()).unwrap();
        for key in [
            "extra_shyps",
            "statement_hash",
            "theorem_exists",
            "oracles_used",
        ] {
            assert!(
                !json.contains(key),
                "default LocalClosureProbeOutput must not serialize `{key}` (Lean byte-identical invariant); got {json}",
            );
        }
        // A NON-default value (Isabelle wire) DOES carry them — confirms the
        // fields are live, not dead.
        let probe = LocalClosureProbeOutput {
            extra_shyps: ["{}".to_string()].into_iter().collect(),
            statement_hash: "h".to_string(),
            theorem_exists: true,
            oracles_used: ["z3".to_string()].into_iter().collect(),
            ..LocalClosureProbeOutput::default()
        };
        let json2 = serde_json::to_string(&probe).unwrap();
        for key in [
            "extra_shyps",
            "statement_hash",
            "theorem_exists",
            "oracles_used",
        ] {
            assert!(
                json2.contains(key),
                "non-empty {key} must serialize on the Isabelle wire; got {json2}",
            );
        }
        // Round-trip equality (deserialize back).
        let back: LocalClosureProbeOutput = serde_json::from_str(&json2).unwrap();
        assert_eq!(back, probe);
    }

    /// Bonus coverage: the [internal] / [strict] paths exist alongside
    /// [shallow] and [axiom]. The plan §6.2 message-shape contract is
    /// the test surface; reject with these prefixes when the probe
    /// reports timeout/non-zero/non-ok status (internal) or non-empty
    /// errors (strict). Two minimal smoke tests to lock the prefix
    /// shapes.
    #[test]
    fn patch_b_mca_rejects_with_internal_when_probe_status_not_ok() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- v5\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response(
            &repo,
            Some(r#"{"status":"elaboration_error","errors":["could not elaborate active node"]}"#),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);

        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[internal]") && e.contains("status=elaboration_error")),
            "non-ok status must emit [internal]: {:?}",
            result.errors,
        );
    }

    #[test]
    fn patch_b_mca_rejects_with_strict_when_probe_errors_non_empty() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- v6\n";
        touch_active_lean(&repo, new_active);
        // status="ok", no axiom violations, but errors is non-empty.
        // Plan §6.1 says we still reject — this is the [strict] arm.
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","errors":["strict-context dep Helper carries unapproved axiom Foo"]}"#,
            ),
        );

        let result = run_mca_gate(&repo, &before_snapshot, &original_active);

        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[strict]") && e.contains("Helper")),
            "[strict] rejection expected when probe.errors is non-empty: {:?}",
            result.errors,
        );
    }

    // ------ kind-aware must_close_active (cycle-699 incident) -------------
    //
    // `must_close_active=true` on a Definition-kind active node (structure
    // / plain def) must NOT run the theorem-root strict-closure probe —
    // the Lean script rejects structure roots with "unsupported root
    // kind" (`scripts/lean_local_closure.lean`), which forced valid
    // definition-repair bursts Invalid via the `[strict]`/`[internal]`
    // arms. For Definition actives "finished" == deterministic delta
    // checks + the `[shallow]` no-sorry gate; no probe, no
    // LocalClosureRecord. Proof-kind actives (and unknown/absent kinds,
    // fail-closed) keep the full probe behavior byte-for-byte.

    /// Set up a minimal lake repo whose active node A is a Definition-kind
    /// structure. Mirrors `setup_closed_active` but with a `structure`
    /// declaration and a `[definition]` .tex block. The default field
    /// value keeps a `:=` in the declaration so `find_declaration` /
    /// `declaration_hash` stay stable across body-value edits.
    fn setup_definition_active(repo: &Path) -> (BTreeMap<String, String>, String) {
        write_local_closure_stub_check_script(repo);
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
        let original_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\nstructure A where\n  side : Nat := 0\n";
        write(&repo.join("Tablet/A.lean"), original_active);
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{definition}A structure.\\end{definition}\n",
        );
        let before_snapshot = snapshot_tablet_dir(repo);
        write(&repo.join("Tablet/A.lean"), original_active);
        (before_snapshot, original_active.to_string())
    }

    /// `run_mca_gate` variant that threads an explicit
    /// `current_node_kinds` map (the kind-aware gate's input). Everything
    /// else matches `run_mca_gate`'s must_close_active=true preset.
    fn run_mca_gate_with_kinds(
        repo: &Path,
        before_snapshot: &BTreeMap<String, String>,
        original_active: &str,
        node_kinds: &BTreeMap<NodeId, crate::NodeKind>,
    ) -> WorkerValidationStepResult {
        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        proof_worker_delta_step_result(
            repo,
            "A",
            before_snapshot,
            &set(&["Preamble", "A", "Helper", "SmuggleDef", "StrictThm"]),
            &BTreeSet::new(), // declared_deleted_nodes
            node_kinds,
            &BTreeSet::new(), // current_open_nodes
            &declaration_hash(original_active, "A"),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true, // allow_new_obligations: irrelevant when no new helpers
            true, // must_close_active (the gate under test)
        )
        .unwrap()
    }

    /// (a) Definition-kind active + must_close_active=true + clean
    /// structure edit → ACCEPTED, and the strict-closure probe is never
    /// invoked for the root: the canned probe response is poisoned
    /// (status=internal_error), so any probe invocation would reject the
    /// burst. No probe payload also means no closure-record input.
    #[test]
    fn kind_aware_mca_accepts_clean_definition_active_without_closure_probe() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_definition_active(&repo);
        // The cycle-699 shape: a surgical one-field repair on a structure.
        // Same declaration head (hash-stable), different field default.
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\nstructure A where\n  side : Nat := 1\n";
        touch_active_lean(&repo, new_active);
        // Poison BOTH canned probe responses (shared + per-node): if any
        // closure probe runs for A, the gate rejects and this test fails.
        let poison = r#"{"status":"internal_error","errors":["closure probe must not run for a Definition-kind active"]}"#;
        set_local_closure_canned_response(&repo, Some(poison));
        set_local_closure_canned_response_for_node(&repo, "A", Some(poison));
        let kinds = BTreeMap::from([
            (NodeId::from("Preamble"), crate::NodeKind::Preamble),
            (NodeId::from("A"), crate::NodeKind::Definition),
        ]);

        let result = run_mca_gate_with_kinds(&repo, &before_snapshot, &original_active, &kinds);

        assert!(
            result.ok,
            "Definition-kind active under must_close_active must be accepted by the \
             deterministic delta checks + no-sorry gate alone: {:?}",
            result.errors,
        );
        assert!(
            result.local_closure_results.is_empty(),
            "Definition-kind active must not produce a probe payload (no closure \
             record for definition nodes), got: {:?}",
            result.local_closure_results.keys().collect::<Vec<_>>(),
        );
    }

    /// (b) Definition-kind active + must_close_active=true but the
    /// structure carries `sorry` (in a default field value) → REJECTED.
    /// The no-sorry requirement for definition actives lives OUTSIDE the
    /// skipped probe (keyword scan / `[shallow]` shallow_ok gate), so
    /// skipping the probe must not admit a sorryd definition.
    #[test]
    fn kind_aware_mca_rejects_definition_active_with_sorry() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_definition_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\nstructure A where\n  side : Nat := sorry\n";
        touch_active_lean(&repo, new_active);
        let kinds = BTreeMap::from([(NodeId::from("A"), crate::NodeKind::Definition)]);

        let result = run_mca_gate_with_kinds(&repo, &before_snapshot, &original_active, &kinds);

        assert!(
            !result.ok,
            "a Definition-kind active with `sorry` must still be rejected under \
             must_close_active",
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.to_ascii_lowercase().contains("sorry")),
            "rejection must name the sorry violation: {:?}",
            result.errors,
        );
        assert!(
            result.local_closure_results.is_empty(),
            "rejection must not stash a probe payload",
        );
    }

    /// (c) Proof-kind active + must_close_active=true → behavior is
    /// unchanged: the strict-closure probe still runs and its `[strict]`
    /// arm still rejects. Mirrors
    /// `patch_b_mca_rejects_with_strict_when_probe_errors_non_empty` but
    /// with the active node's kind EXPLICITLY Proof in
    /// `current_node_kinds` (the existing MCA tests pass an empty kinds
    /// map, which exercises the fail-closed missing-kind default).
    #[test]
    fn kind_aware_mca_keeps_strict_closure_for_explicit_proof_kind_active() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- kind-aware v1\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","errors":["strict-context dep Helper carries unapproved axiom Foo"]}"#,
            ),
        );
        let kinds = BTreeMap::from([(NodeId::from("A"), crate::NodeKind::Proof)]);

        let result = run_mca_gate_with_kinds(&repo, &before_snapshot, &original_active, &kinds);

        assert!(!result.ok);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.starts_with("[strict]") && e.contains("Helper")),
            "Proof-kind active must keep the [strict] probe rejection: {:?}",
            result.errors,
        );
    }

    // ------ lean-semantic-payloads short-circuit cache --------------------
    //
    // These tests exercise `observe_lean_semantic_payloads`'s
    // process-local content-hash cache via the public entry point
    // `observe_correspondence_fingerprints`. They write a counting stub
    // `check.py` that appends to a log file every time it's invoked,
    // then call the observer twice with various inter-call file edits.
    //
    // Each test clears the cache up front because `cargo test`'s default
    // parallel runner shares the static `LEAN_SEMANTIC_PAYLOAD_CACHE`
    // across tests; the per-tempdir canonical repo path keeps tests
    // isolated by key, but explicit clear keeps assertions independent
    // of run order in case of future test additions.

    fn write_counting_stub_check_script(repo: &Path, log_path: &Path) {
        let script = format!(
            r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]
log_path = Path({log_path:?})
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(cmd + "\n")

if cmd == "sync-tablet-support":
    print(json.dumps({{
        "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }}))
elif cmd == "materialize-tablet-oleans":
    print(json.dumps({{
        "returncode": 0,
        "stdout": "materialized",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "prepare-compiled-support":
    print(json.dumps({{
        "returncode": 0,
        "stdout": "prepared",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "lean-compile-node":
    print(json.dumps({{
        "returncode": 0,
        "stdout": "",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "print-axioms":
    node = sys.argv[2]
    print(json.dumps({{
        "returncode": 0,
        "stdout": f"{{node}} does not depend on any axioms\n",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "local-closure-axioms":
    node = sys.argv[2]
    print(json.dumps({{
        "request_id": 0,
        "node_name": node,
        "returncode": 0,
        "timed_out": False,
        "stdout": "",
        "stderr": "",
        "status": "ok",
        "kernel_axioms": [],
        "boundary_theorems": [],
        "strict_theorem_deps": [],
        "strict_definition_deps": [],
        "errors": [],
    }}))
elif cmd == "lean-semantic-payloads":
    repo = Path(sys.argv[2])
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    payloads = {{}}
    for node in nodes:
        lean_path = repo / "Tablet" / f"{{node}}.lean"
        payload = ""
        for line in lean_path.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if stripped.startswith(("theorem ", "lemma ", "def ", "abbrev ", "instance ", "structure ", "inductive ", "class ")):
                payload = stripped
                break
        payloads[node] = {{"ok": True, "payload": payload, "error": ""}}
    print(json.dumps(payloads))
else:
    raise SystemExit(f"unexpected subcommand: {{cmd}}")
"#,
            log_path = log_path.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }

    fn write_imported_sorry_sensitive_stub_check_script(repo: &Path, log_path: &Path) {
        let script = format!(
            r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

cmd = sys.argv[1]
log_path = Path({log_path:?})
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(cmd + "\n")

def helper_is_open(repo):
    helper = repo / "Tablet" / "Helper.lean"
    return helper.exists() and "sorry" in helper.read_text(encoding="utf-8")

if cmd == "sync-tablet-support":
    print(json.dumps({{
        "updated_paths": ["Tablet/INDEX.md", "Tablet/README.md"],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }}))
elif cmd == "materialize-tablet-oleans":
    print(json.dumps({{
        "returncode": 0,
        "stdout": "materialized",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "prepare-compiled-support":
    print(json.dumps({{
        "returncode": 0,
        "stdout": "prepared",
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "lean-compile-node":
    repo = Path(sys.argv[3])
    stdout = "warning: Tablet/Helper.lean:3:8: declaration uses 'sorry'\n" if helper_is_open(repo) else ""
    print(json.dumps({{
        "returncode": 0,
        "stdout": stdout,
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "print-axioms":
    node = sys.argv[2]
    repo = Path(sys.argv[3])
    stdout = f"{{node}} depends on axioms: [sorryAx]\n" if helper_is_open(repo) else f"{{node}} does not depend on any axioms\n"
    print(json.dumps({{
        "returncode": 0,
        "stdout": stdout,
        "stderr": "",
        "timed_out": False,
        "spawn_error": "",
    }}))
elif cmd == "lean-semantic-payloads":
    print(json.dumps({{}}))
else:
    raise SystemExit(f"unexpected subcommand: {{cmd}}")
"#,
            log_path = log_path.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }

    fn count_invocations(log_path: &Path, subcommand: &str) -> usize {
        let raw = std::fs::read_to_string(log_path).unwrap_or_default();
        raw.lines().filter(|line| line.trim() == subcommand).count()
    }

    fn write_minimal_lake_repo(repo: &Path) {
        std::fs::create_dir_all(repo.join("Tablet")).unwrap();
        // `has_lake_project` flips true with either of these present.
        write(&repo.join("lakefile.lean"), "package «stub»\n");
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\n",
        );
        write(&repo.join("Tablet/Preamble.tex"), "");
    }

    #[test]
    fn cache_skips_lean_semantic_payloads_on_repeat_call_with_no_edits() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        // First call: cold cache, must dispatch to the Lean script.
        let first = observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        let first_count = count_invocations(&log, "lean-semantic-payloads");
        assert_eq!(
            first_count, 1,
            "cold cache must dispatch lean-semantic-payloads exactly once"
        );

        // Second call with no filesystem edits: must hit cache and skip
        // the Lean dispatch entirely.
        let second = observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        let second_count = count_invocations(&log, "lean-semantic-payloads");
        assert_eq!(
            second_count, first_count,
            "warm cache must not re-dispatch lean-semantic-payloads"
        );
        assert_eq!(
            first, second,
            "fingerprints must match between cold and warm calls"
        );
    }

    #[test]
    fn cache_skips_lean_dispatch_when_only_node_tex_changes() {
        // NL-only worker burst: only the .tex statement was touched.
        // The Lean closure is byte-for-byte identical, so the cache
        // hits and zero `lean-semantic-payloads` calls fire on the
        // second walk. (`own_tex` axis still recomputes from the new
        // .tex content — that's a cheap text hash, never a Lean dispatch.)
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}Statement v1\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        let first = observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        let first_count = count_invocations(&log, "lean-semantic-payloads");
        assert_eq!(first_count, 1);

        // .tex change only — Lean side untouched.
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}Statement v2 with new prose\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );

        let second = observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        let second_count = count_invocations(&log, "lean-semantic-payloads");
        assert_eq!(
            second_count, 1,
            "tex-only edit must not trigger another lean-semantic-payloads dispatch"
        );

        // Sanity: own_tex differs between the two snapshots because
        // .tex changed; lean_semantic_closure is identical because it
        // came from the cache.
        let parse = |fp: &str| CorrespondenceFingerprint::from_storage_string(fp).unwrap();
        let first_fp = parse(first.get(&NodeId::from("A")).unwrap());
        let second_fp = parse(second.get(&NodeId::from("A")).unwrap());
        assert_ne!(
            first_fp.own_tex, second_fp.own_tex,
            "own_tex must reflect the .tex edit"
        );
        assert_eq!(
            first_fp.lean_semantic_closure, second_fp.lean_semantic_closure,
            "lean_semantic_closure must be served from the cache and unchanged"
        );
    }

    #[test]
    fn cache_invalidates_on_node_lean_change() {
        // A .lean edit on the node itself must invalidate the cache for
        // that node; the second call dispatches lean-semantic-payloads
        // again because the content hash of `A.lean` shifted.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        let first_count = count_invocations(&log, "lean-semantic-payloads");
        assert_eq!(first_count, 1);

        // Worker changed the Lean side: declaration value differs.
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by trivial\n",
        );

        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        let second_count = count_invocations(&log, "lean-semantic-payloads");
        assert_eq!(
            second_count, 2,
            "Lean edit must force a fresh lean-semantic-payloads dispatch"
        );
    }

    #[test]
    fn cache_skips_lean_dispatch_when_only_preamble_tex_changes() {
        // Preamble.tex change: structurally distinct from Preamble.lean.
        // The cache key only includes Preamble.lean, so a Preamble.tex
        // edit must NOT invalidate the per-node cache. The
        // `preamble_tex` axis still gets recomputed (it's a cheap text
        // hash), but Lean dispatch is skipped.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );
        // Seed Preamble.tex with a structured definition so
        // preamble_structured_hash returns something non-empty (and
        // non-default) — this exercises the preamble_tex axis end to end.
        write(
            &repo.join("Tablet/Preamble.tex"),
            "\\begin{definition}[A foo]Foo means bar.\\end{definition}\n",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        let first = observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        let first_count = count_invocations(&log, "lean-semantic-payloads");
        assert_eq!(first_count, 1);

        // Preamble.tex-only edit (no .lean change anywhere).
        write(
            &repo.join("Tablet/Preamble.tex"),
            "\\begin{definition}[A foo]Foo means bar (revised).\\end{definition}\n",
        );

        let second = observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        let second_count = count_invocations(&log, "lean-semantic-payloads");
        assert_eq!(
            second_count, 1,
            "Preamble.tex-only edit must not trigger a lean-semantic-payloads dispatch"
        );

        // Confirm the preamble_tex axis still differs (the tex change
        // was observed) while lean_semantic_closure was served from
        // the cache.
        let parse = |fp: &str| CorrespondenceFingerprint::from_storage_string(fp).unwrap();
        let first_fp = parse(first.get(&NodeId::from("A")).unwrap());
        let second_fp = parse(second.get(&NodeId::from("A")).unwrap());
        assert_ne!(
            first_fp.preamble_tex, second_fp.preamble_tex,
            "preamble_tex axis must reflect the Preamble.tex edit"
        );
        assert_eq!(
            first_fp.lean_semantic_closure, second_fp.lean_semantic_closure,
            "lean_semantic_closure must be served from the cache"
        );
    }

    #[test]
    fn cache_invalidates_on_preamble_lean_change() {
        // Preamble.lean changes affect every node's import chain, so the
        // cache key shifts and a fresh lean-semantic-payloads dispatch
        // must fire. Conservative: even a comment-only change to
        // Preamble.lean invalidates (we use byte-identical content
        // hashing — the script's behavior cannot be assumed insensitive
        // to comment trivia).
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "lean-semantic-payloads"), 1);

        // Preamble.lean changes (e.g., new mathlib import).
        write(
            &repo.join("Tablet/Preamble.lean"),
            "import Mathlib.Data.Nat.Basic\nimport Mathlib.Tactic.Linarith\n",
        );

        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "lean-semantic-payloads"),
            2,
            "Preamble.lean change must invalidate every node's payload cache"
        );
    }

    #[test]
    fn cache_invalidates_on_lake_manifest_change() {
        // Mathlib upgrade: lake-manifest.json changes even when no
        // .lean file changed. Conservative: must invalidate the cache.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("lake-manifest.json"),
            r#"{"version": 7, "packages": []}"#,
        );
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "lean-semantic-payloads"), 1);

        // Manifest pin bump.
        write(
            &repo.join("lake-manifest.json"),
            r#"{"version": 7, "packages": [{"name": "mathlib", "rev": "abcdef"}]}"#,
        );

        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "lean-semantic-payloads"),
            2,
            "lake-manifest.json change must invalidate cache (mathlib upgrade case)"
        );
    }

    #[test]
    fn cache_partial_hit_dispatches_only_for_missed_nodes() {
        // Two nodes A and B; A's cache hits while B's misses (we edit
        // only B's .lean between calls). The second dispatch must
        // request only B, not A — the per-node short-circuit, not just
        // the walk-level one.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let arglog = tmp.path().join("invocation_args.log");
        write_minimal_lake_repo(&repo);
        // Counting stub that ALSO logs the --node args, so the test
        // can assert the second dispatch contained only B.
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path

cmd = sys.argv[1]
log_path = Path({log_path:?})
arg_log = Path({arg_log_path:?})
with log_path.open("a", encoding="utf-8") as h:
    h.write(cmd + "\n")

if cmd == "sync-tablet-support":
    print(json.dumps({{
        "updated_paths": [],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }}))
elif cmd == "materialize-tablet-oleans":
    print(json.dumps({{"returncode": 0, "stdout": "", "stderr": "", "timed_out": False, "spawn_error": ""}}))
elif cmd == "prepare-compiled-support":
    print(json.dumps({{"returncode": 0, "stdout": "", "stderr": "", "timed_out": False, "spawn_error": ""}}))
elif cmd == "lean-semantic-payloads":
    repo = Path(sys.argv[2])
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    with arg_log.open("a", encoding="utf-8") as h:
        h.write(",".join(sorted(nodes)) + "\n")
    payloads = {{}}
    for node in nodes:
        lean_path = repo / "Tablet" / f"{{node}}.lean"
        body = lean_path.read_text(encoding="utf-8") if lean_path.exists() else ""
        payloads[node] = {{"ok": True, "payload": f"{{node}}|{{body}}", "error": ""}}
    print(json.dumps(payloads))
else:
    raise SystemExit(f"unexpected: {{cmd}}")
"#,
            log_path = log.display().to_string(),
            arg_log_path = arglog.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := trivial\n",
        );
        write(
            &repo.join("Tablet/B.tex"),
            "\\begin{theorem}B\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("A"), NodeId::from("B")].into_iter().collect();

        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "lean-semantic-payloads"), 1);
        // First dispatch should have asked for both A and B.
        let first_args = std::fs::read_to_string(&arglog).unwrap();
        let first_line = first_args.lines().next().unwrap();
        assert_eq!(first_line, "A,B", "first call must request both nodes");

        // Edit B's .lean only.
        write(
            &repo.join("Tablet/B.lean"),
            "-- [TABLET NODE: B]\nimport Tablet.Preamble\n\ntheorem B : True := by trivial\n",
        );

        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "lean-semantic-payloads"),
            2,
            "the second call still dispatches because B's cache key shifted"
        );
        let second_args = std::fs::read_to_string(&arglog).unwrap();
        let second_line = second_args.lines().nth(1).unwrap();
        assert_eq!(
            second_line, "B",
            "second dispatch must ask only for B; A's payload comes from cache"
        );
    }

    #[test]
    fn cache_does_not_pin_failed_payloads() {
        // A first-call failure (non-ok payload) must not be cached;
        // otherwise a transient build error would persist forever.
        // Test by stubbing a script that returns ok=False on the first
        // call and ok=True on the second call (toggled via a sentinel
        // file). Both calls must dispatch.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let toggle = tmp.path().join("first_call_done");
        write_minimal_lake_repo(&repo);
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path

cmd = sys.argv[1]
log_path = Path({log_path:?})
toggle_path = Path({toggle_path:?})
with log_path.open("a", encoding="utf-8") as h:
    h.write(cmd + "\n")

if cmd == "sync-tablet-support":
    print(json.dumps({{
        "updated_paths": [],
        "header_tex_path": "Tablet/header.tex",
        "index_md_path": "Tablet/INDEX.md",
        "readme_md_path": "Tablet/README.md",
    }}))
elif cmd == "materialize-tablet-oleans":
    print(json.dumps({{"returncode": 0, "stdout": "", "stderr": "", "timed_out": False, "spawn_error": ""}}))
elif cmd == "prepare-compiled-support":
    print(json.dumps({{"returncode": 0, "stdout": "", "stderr": "", "timed_out": False, "spawn_error": ""}}))
elif cmd == "lean-semantic-payloads":
    nodes = []
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--node" and i + 1 < len(sys.argv):
            nodes.append(sys.argv[i + 1])
            i += 2
        else:
            i += 1
    if not toggle_path.exists():
        toggle_path.write_text("done")
        payloads = {{n: {{"ok": False, "payload": "", "error": "transient"}} for n in nodes}}
    else:
        payloads = {{n: {{"ok": True, "payload": f"{{n}}-good", "error": ""}} for n in nodes}}
    print(json.dumps(payloads))
else:
    raise SystemExit(f"unexpected: {{cmd}}")
"#,
            log_path = log.display().to_string(),
            toggle_path = toggle.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join("Tablet/A.tex"),
            "\\begin{theorem}A\\end{theorem}\n\\begin{proof}Done.\\end{proof}\n",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);
        let nodes: BTreeSet<NodeId> = [NodeId::from("A")].into_iter().collect();

        // First call: ok=False payload. The fingerprint comes back
        // empty (payload check inside `correspondence_fingerprint`),
        // and crucially the cache is NOT populated.
        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        assert_eq!(count_invocations(&log, "lean-semantic-payloads"), 1);

        // Second call: must dispatch again because the failure wasn't
        // pinned in the cache.
        observe_correspondence_fingerprints(&repo, &nodes).unwrap();
        assert_eq!(
            count_invocations(&log, "lean-semantic-payloads"),
            2,
            "transient ok=False results must not be cached"
        );
    }

    // ------ run_compile_node cache short-circuit -----------------------
    //
    // These tests exercise the per-node `lean-compile-node` cache via
    // `run_compile_node` directly. The same closure-content-hash key
    // used for `lean-semantic-payloads` gates this cache; the safety
    // guard is the `<node>.olean` presence check.

    fn write_node_olean(repo: &Path, node: &str) {
        let path = repo
            .join(".lake/build/lib/lean/Tablet")
            .join(format!("{node}.olean"));
        write(&path, &format!("olean-stub-for-{node}"));
    }

    #[test]
    fn compile_node_cache_skips_dispatch_on_unchanged_closure_with_olean_present() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        clear_lean_semantic_payload_cache_for_tests(&repo);

        // First call: cold cache, dispatches.
        run_compile_node(&repo, "A").unwrap();
        assert_eq!(count_invocations(&log, "lean-compile-node"), 1);

        // Second call: warm cache + olean still on disk → skip.
        run_compile_node(&repo, "A").unwrap();
        assert_eq!(
            count_invocations(&log, "lean-compile-node"),
            1,
            "warm cache hit must skip lean-compile-node dispatch"
        );
    }

    #[test]
    fn compile_node_cache_falls_back_when_olean_missing() {
        // Conservative-by-design: even when the closure key matches,
        // a missing olean must trigger a fresh compile so the artefact
        // gets rebuilt. Otherwise downstream `materialize_oleans` /
        // `lean-semantic-payloads` ops break.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        clear_lean_semantic_payload_cache_for_tests(&repo);

        run_compile_node(&repo, "A").unwrap();
        assert_eq!(count_invocations(&log, "lean-compile-node"), 1);

        // Worker hygiene cleanup wipes the olean even though .lean
        // didn't change.
        std::fs::remove_file(repo.join(".lake/build/lib/lean/Tablet/A.olean")).unwrap();

        run_compile_node(&repo, "A").unwrap();
        assert_eq!(
            count_invocations(&log, "lean-compile-node"),
            2,
            "missing olean must force a fresh compile-node dispatch"
        );
    }

    #[test]
    fn compile_node_cache_invalidates_on_lean_edit() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        clear_lean_semantic_payload_cache_for_tests(&repo);

        run_compile_node(&repo, "A").unwrap();
        assert_eq!(count_invocations(&log, "lean-compile-node"), 1);

        // Worker edits .lean.
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := by trivial\n",
        );
        run_compile_node(&repo, "A").unwrap();
        assert_eq!(
            count_invocations(&log, "lean-compile-node"),
            2,
            ".lean edit must invalidate the compile-node cache"
        );
    }

    #[test]
    fn compile_node_cache_does_not_pin_failed_returncode() {
        // A non-zero returncode is a legitimate compile error. The
        // worker may fix the source on the next pass; if we cached the
        // failure, the recovery path would be locked out.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        let toggle = tmp.path().join("toggle");
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
cmd = sys.argv[1]
with Path({log:?}).open("a", encoding="utf-8") as h:
    h.write(cmd + "\n")
toggle = Path({toggle:?})
if cmd == "sync-tablet-support":
    print(json.dumps({{"updated_paths": [], "header_tex_path": "", "index_md_path": "", "readme_md_path": ""}}))
elif cmd == "lean-compile-node":
    if not toggle.exists():
        toggle.write_text("on")
        print(json.dumps({{"returncode": 1, "stdout": "", "stderr": "type error", "timed_out": False, "spawn_error": ""}}))
    else:
        print(json.dumps({{"returncode": 0, "stdout": "", "stderr": "", "timed_out": False, "spawn_error": ""}}))
else:
    raise SystemExit(f"unexpected: {{cmd}}")
"#,
            log = log.display().to_string(),
            toggle = toggle.display().to_string(),
        );
        let path = repo.join(".trellis/scripts/check.py");
        write(&path, &script);
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        clear_lean_semantic_payload_cache_for_tests(&repo);

        let first = run_compile_node(&repo, "A").unwrap();
        assert_eq!(first.returncode, Some(1));
        assert_eq!(count_invocations(&log, "lean-compile-node"), 1);

        // Second call must dispatch again because returncode=1 wasn't
        // pinned; this time the script reports success.
        let second = run_compile_node(&repo, "A").unwrap();
        assert_eq!(second.returncode, Some(0));
        assert_eq!(
            count_invocations(&log, "lean-compile-node"),
            2,
            "failed compile must not be cached"
        );
    }

    // ------ run_print_axioms cache short-circuit ------------------------

    #[test]
    fn print_axioms_cache_skips_dispatch_on_unchanged_closure() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write_node_olean(&repo, "A");

        clear_lean_semantic_payload_cache_for_tests(&repo);

        run_print_axioms(&repo, "A").unwrap();
        assert_eq!(count_invocations(&log, "print-axioms"), 1);

        run_print_axioms(&repo, "A").unwrap();
        assert_eq!(
            count_invocations(&log, "print-axioms"),
            1,
            "warm print-axioms cache hit must skip dispatch"
        );
    }

    #[test]
    fn print_axioms_cache_self_heals_when_olean_rebuilt() {
        // DEFECT 1 (olean-staleness fix, end-to-end): a print-axioms result
        // cached against a content-stale olean (the `sorryAx`-bearing build
        // an operator `touch` could freeze) must NOT be re-served once the
        // olean is rebuilt — even though the SOURCE (and thus the source-only
        // key) is unchanged. The cache key folds in the olean content hash, so
        // a rebuilt olean changes the key ⇒ miss ⇒ fresh dispatch that reads
        // the rebuilt (closed) olean. With the old source-only key this second
        // call would have hit and re-served the stale `sorryAx` result.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        // Stale olean stands in for a `sorryAx`-bearing build.
        write(
            &repo.join(".lake/build/lib/lean/Tablet/A.olean"),
            "stale-sorry-olean",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);

        run_print_axioms(&repo, "A").unwrap();
        assert_eq!(count_invocations(&log, "print-axioms"), 1);

        // Rebuild the olean to a different (closed) build WITHOUT editing the
        // source — exactly the post-fix-deploy / re-close shape.
        write(
            &repo.join(".lake/build/lib/lean/Tablet/A.olean"),
            "closed-olean-rebuilt",
        );

        run_print_axioms(&repo, "A").unwrap();
        assert_eq!(
            count_invocations(&log, "print-axioms"),
            2,
            "rebuilt olean must change the key ⇒ cache miss ⇒ fresh dispatch (no stale serve)",
        );
    }

    #[test]
    fn compile_node_cache_self_heals_when_olean_rebuilt() {
        // DEFECT 1 companion for `lean-compile-node`: same self-heal property.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/A.lean"),
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A : True := trivial\n",
        );
        write(
            &repo.join(".lake/build/lib/lean/Tablet/A.olean"),
            "stale-olean",
        );

        clear_lean_semantic_payload_cache_for_tests(&repo);

        run_compile_node(&repo, "A").unwrap();
        assert_eq!(count_invocations(&log, "lean-compile-node"), 1);

        write(
            &repo.join(".lake/build/lib/lean/Tablet/A.olean"),
            "rebuilt-olean",
        );

        run_compile_node(&repo, "A").unwrap();
        assert_eq!(
            count_invocations(&log, "lean-compile-node"),
            2,
            "rebuilt olean must change the key ⇒ cache miss ⇒ fresh dispatch",
        );
    }

    // ------ disk-cache short-circuits ------------------------------------
    //
    // These tests simulate the production process shape (the Python
    // wrapper `Popen`s a fresh kernel CLI process per `RuntimeCliRequest`,
    // so each invocation starts with a cold in-memory cache). We
    // approximate the "process boundary" by clearing the in-memory
    // statics between calls; the disk cache underneath the env var
    // `TRELLIS_KERNEL_CACHE_ROOT` survives.

    /// Serialise tests that mutate `TRELLIS_KERNEL_CACHE_ROOT`. The
    /// env var is process-global; cargo's parallel test runner would
    /// otherwise let disk-cache tests and runtime `Run` tests observe
    /// each other's tempdir.
    fn with_disk_cache_root<R>(cache_root: &Path, body: impl FnOnce() -> R) -> R {
        let _guard = crate::kernel_cache_env_test_guard();
        let prev = std::env::var_os(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
        // SAFETY: see runtime_cli's `Run` handler — the test binary is
        // single-threaded WRT env mutation while the lock is held.
        unsafe {
            std::env::set_var(
                trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV,
                cache_root,
            );
        }
        let result = body();
        match prev {
            Some(value) => unsafe {
                std::env::set_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, value);
            },
            None => unsafe {
                std::env::remove_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
            },
        }
        result
    }

    #[test]
    fn compile_node_disk_cache_skips_dispatch_after_in_memory_cleared() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let cache_root = tmp.path().join("runtime-root");
        let node = "DiskCompileHit";
        std::fs::create_dir_all(&cache_root).unwrap();
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join(format!("Tablet/{node}.lean")),
            &format!(
                "-- [TABLET NODE: {node}]\nimport Tablet.Preamble\n\ntheorem {node} : True := trivial\n"
            ),
        );
        write_node_olean(&repo, node);

        with_disk_cache_root(&cache_root, || {
            // "Process 1": cold, dispatches once.
            clear_lean_semantic_payload_cache_for_tests(&repo);
            run_compile_node(&repo, node).unwrap();
            assert_eq!(count_invocations(&log, "lean-compile-node"), 1);

            // "Process 2": clear in-memory tier; disk persists.
            clear_lean_semantic_payload_cache_for_tests(&repo);
            run_compile_node(&repo, node).unwrap();
            assert_eq!(
                count_invocations(&log, "lean-compile-node"),
                1,
                "warm disk cache (after cold in-memory) must skip dispatch"
            );
        });
    }

    #[test]
    fn compile_node_disk_cache_falls_back_when_olean_missing() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let cache_root = tmp.path().join("runtime-root");
        let node = "DiskCompileMissing";
        std::fs::create_dir_all(&cache_root).unwrap();
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join(format!("Tablet/{node}.lean")),
            &format!(
                "-- [TABLET NODE: {node}]\nimport Tablet.Preamble\n\ntheorem {node} : True := trivial\n"
            ),
        );
        write_node_olean(&repo, node);

        with_disk_cache_root(&cache_root, || {
            clear_lean_semantic_payload_cache_for_tests(&repo);
            run_compile_node(&repo, node).unwrap();
            assert_eq!(count_invocations(&log, "lean-compile-node"), 1);

            // Worker hygiene wipes the olean. Disk-cache hit must
            // still verify olean presence and re-dispatch.
            std::fs::remove_file(
                repo.join(".lake/build/lib/lean/Tablet")
                    .join(format!("{node}.olean")),
            )
            .unwrap();
            clear_lean_semantic_payload_cache_for_tests(&repo);
            run_compile_node(&repo, node).unwrap();
            assert_eq!(
                count_invocations(&log, "lean-compile-node"),
                2,
                "missing olean must force a fresh compile-node dispatch even with disk hit"
            );
        });
    }

    #[test]
    fn print_axioms_disk_cache_skips_dispatch_after_in_memory_cleared() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let cache_root = tmp.path().join("runtime-root");
        let node = "DiskPrintHit";
        std::fs::create_dir_all(&cache_root).unwrap();
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join(format!("Tablet/{node}.lean")),
            &format!(
                "-- [TABLET NODE: {node}]\nimport Tablet.Preamble\n\ntheorem {node} : True := trivial\n"
            ),
        );
        write_node_olean(&repo, node);

        with_disk_cache_root(&cache_root, || {
            clear_lean_semantic_payload_cache_for_tests(&repo);
            run_print_axioms(&repo, node).unwrap();
            assert_eq!(count_invocations(&log, "print-axioms"), 1);

            clear_lean_semantic_payload_cache_for_tests(&repo);
            run_print_axioms(&repo, node).unwrap();
            assert_eq!(
                count_invocations(&log, "print-axioms"),
                1,
                "warm disk cache (after cold in-memory) must skip print-axioms dispatch"
            );
        });
    }

    #[test]
    fn imported_helper_sorry_invalidates_disk_cache_and_suppresses_axiom_audit() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let cache_root = tmp.path().join("runtime-root");
        let active = "DiskSorryActive";
        std::fs::create_dir_all(&cache_root).unwrap();
        write_minimal_lake_repo(&repo);
        write_imported_sorry_sensitive_stub_check_script(&repo, &log);
        write(
            &repo.join("Tablet/Helper.lean"),
            "-- [TABLET NODE: Helper]\nimport Tablet.Preamble\n\ntheorem Helper : True := trivial\n",
        );
        write(
            &repo.join("Tablet/Helper.tex"),
            "\\begin{helper}Helper.\\end{helper}\n\\begin{proof}Done.\\end{proof}\n",
        );
        write(
            &repo.join(format!("Tablet/{active}.lean")),
            &format!(
                "-- [TABLET NODE: {active}]\nimport Tablet.Preamble\nimport Tablet.Helper\n\ntheorem {active} : True := Helper\n"
            ),
        );
        write(
            &repo.join(format!("Tablet/{active}.tex")),
            "\\begin{theorem}Disk sorry active.\\end{theorem}\n\\begin{proof}Use Helper.\\end{proof}\n",
        );
        write_node_olean(&repo, active);

        with_disk_cache_root(&cache_root, || {
            clear_lean_semantic_payload_cache_for_tests(&repo);
            let closed_observation = observe_node(&repo, active).unwrap();
            let closed_eval = evaluate_node_observation(&repo, &closed_observation, None);
            assert!(closed_eval.ok, "{:?}", closed_eval.errors);
            assert_eq!(count_invocations(&log, "lean-compile-node"), 1);
            assert_eq!(count_invocations(&log, "print-axioms"), 1);

            write(
                &repo.join("Tablet/Helper.lean"),
                "-- [TABLET NODE: Helper]\nimport Tablet.Preamble\n\ntheorem Helper : True := by\n  sorry\n",
            );

            // Simulate the next worker/supervisor checker process. The active
            // node's own file and stale olean are unchanged, so only hashing the
            // imported helper can prevent the old closed observation from being
            // reused. The correct result is the pre-regression supervisor
            // result: the active node is open because an imported helper uses
            // `sorry`, and therefore `#print axioms` is not run.
            clear_lean_semantic_payload_cache_for_tests(&repo);
            let open_observation = observe_node(&repo, active).unwrap();
            let open_eval = evaluate_node_observation(&repo, &open_observation, None);
            assert!(
                !open_eval.ok,
                "imported helper sorry should leave {active} open"
            );
            assert!(
                !open_eval.sorry_free,
                "imported sorry warning must mark {active} open"
            );
            assert!(
                open_eval
                    .errors
                    .iter()
                    .all(|err| !err.contains("Axiom audit failed")),
                "imported sorry must not be converted into a sorryAx audit failure: {:?}",
                open_eval.errors
            );
            assert_eq!(
                count_invocations(&log, "lean-compile-node"),
                2,
                "helper .lean edit must invalidate the compile-node disk cache"
            );
            assert_eq!(
                count_invocations(&log, "print-axioms"),
                1,
                "imported sorry warning must suppress print-axioms instead of reusing stale closed output"
            );
        });
    }

    #[test]
    fn lean_semantic_payloads_disk_cache_skips_dispatch_after_in_memory_cleared() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let log = tmp.path().join("invocations.log");
        let cache_root = tmp.path().join("runtime-root");
        let node = "DiskPayloadHit";
        std::fs::create_dir_all(&cache_root).unwrap();
        write_minimal_lake_repo(&repo);
        write_counting_stub_check_script(&repo, &log);
        write(
            &repo.join(format!("Tablet/{node}.lean")),
            &format!(
                "-- [TABLET NODE: {node}]\nimport Tablet.Preamble\n\ntheorem {node} : True := trivial\n"
            ),
        );

        let nodes: BTreeSet<NodeId> = [NodeId::from(node)].into_iter().collect();

        with_disk_cache_root(&cache_root, || {
            // "Process 1": cold, dispatches once.
            clear_lean_semantic_payload_cache_for_tests(&repo);
            observe_lean_semantic_payloads(&repo, &nodes).unwrap();
            assert_eq!(count_invocations(&log, "lean-semantic-payloads"), 1);

            // "Process 2": clear in-memory tier; disk persists.
            clear_lean_semantic_payload_cache_for_tests(&repo);
            observe_lean_semantic_payloads(&repo, &nodes).unwrap();
            assert_eq!(
                count_invocations(&log, "lean-semantic-payloads"),
                1,
                "warm disk cache (after cold in-memory) must skip lean-semantic-payloads dispatch"
            );
        });
    }

    // ------ Patch A: local-closure probe wrapper unit tests -----------------
    //
    // These cover plan §5.9 Tier 1 (Rust unit). The wrapper itself
    // (`run_local_closure_axioms`) shells out to `run_repo_command_json`
    // and cannot be exercised here without the live checker socket; the
    // tests below cover the deterministic pieces that the wrapper composes:
    // the DTO's serde round-trip, the lean-name → NodeId stripper, and
    // the response-envelope parser's diagnostics on malformed input.
    // End-to-end coverage lives in the Tier 3 fixture under
    // `kernel/tests/fixtures/local_closure_smoke/`, which is gated
    // `#[ignore]` because it requires an operator-built fixture .lean
    // tree and a live checker server.

    #[test]
    fn local_closure_probe_output_json_roundtrip() {
        // Plan §5.9 Tier 1: construct a fully-populated probe output,
        // serialize, deserialize, assert equality. Pins the serde
        // contract so future field additions can't silently drop data
        // on the wire between the runtime CLI and the engine event.
        let mut boundary = BTreeMap::new();
        boundary.insert(NodeId::from("Helper"), "stmt-hash-helper".to_string());
        boundary.insert(NodeId::from("OtherHelper"), "stmt-hash-other".to_string());

        let mut strict_thm = BTreeMap::new();
        strict_thm.insert(NodeId::from("StrictThm"), "value-hash-thm".to_string());

        let mut strict_def = BTreeMap::new();
        strict_def.insert(NodeId::from("StrictDef"), "semantic-hash-def".to_string());

        let original = LocalClosureProbeOutput {
            status: "ok".to_string(),
            kernel_axioms: BTreeSet::from([
                "Classical.choice".to_string(),
                "Quot.sound".to_string(),
            ]),
            oracles_used: BTreeSet::new(),
            boundary_theorems: boundary,
            strict_theorem_deps: strict_thm,
            strict_definition_deps: strict_def,
            errors: vec!["soft-warning: foo".to_string()],
            raw_stdout: "{...}\n".to_string(),
            raw_stderr: "minor noise".to_string(),
            returncode: 0,
            timed_out: false,
            axiomization_check: None,
            ..LocalClosureProbeOutput::default()
        };

        let encoded = serde_json::to_string(&original).expect("serialize probe output");
        let decoded: LocalClosureProbeOutput =
            serde_json::from_str(&encoded).expect("deserialize probe output");
        assert_eq!(decoded, original);
    }

    #[test]
    fn local_closure_probe_output_round_trips_with_axiomization_check() {
        // Plan §4.6.1: the new `axiomization_check` sub-object must
        // round-trip cleanly through serde so engine events carrying a
        // probe payload preserve the cross-check verdict across the
        // bridge / replay boundaries. Mirrors the round-trip test above
        // but exercises the `Some(AxiomizationCheckOutput)` path.
        let boundary = BTreeMap::from([(NodeId::from("Helper"), "h1".to_string())]);
        let original = LocalClosureProbeOutput {
            status: "ok".to_string(),
            kernel_axioms: BTreeSet::from(["Classical.choice".to_string()]),
            oracles_used: BTreeSet::new(),
            boundary_theorems: boundary,
            strict_theorem_deps: BTreeMap::new(),
            strict_definition_deps: BTreeMap::new(),
            errors: Vec::new(),
            raw_stdout: String::new(),
            raw_stderr: String::new(),
            returncode: 0,
            timed_out: false,
            axiomization_check: Some(AxiomizationCheckOutput {
                kernel_axioms: BTreeSet::from(["Classical.choice".to_string()]),
                boundary_theorems: BTreeSet::from(["Helper".to_string()]),
                agreed: true,
                skipped: false,
                primary_only_axioms: Vec::new(),
                axcheck_only_axioms: Vec::new(),
                primary_only_boundaries: Vec::new(),
                axcheck_only_boundaries: Vec::new(),
                // Patch C-N item 4: typed crash-error field; None on
                // agreed/skipped/disagree paths (only crashes populate).
                error: None,
            }),
            ..LocalClosureProbeOutput::default()
        };
        let encoded = serde_json::to_string(&original).expect("serialize axcheck output");
        let decoded: LocalClosureProbeOutput =
            serde_json::from_str(&encoded).expect("deserialize axcheck output");
        assert_eq!(decoded, original);
    }

    #[test]
    fn parse_local_closure_response_flips_status_on_axcheck_disagreement() {
        // Plan §4.6.1 runtime invariant: when the script reports
        // `axiomization_check.agreed: false` (and not skipped), the
        // wrapper flips the top-level status to the dedicated
        // `checker_disagreement` value and appends a structured
        // diagnostic to `errors`. Callers that gate on `status != "ok"`
        // still reject; the distinct status name lets the supervisor's
        // halt path classify the failure as structural rather than
        // transient (`feedback_fail_loudly_on_dual_check`).
        let _env = HaltMarkerEnvOverride::pointing_into(tempdir().unwrap().path());
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["Classical.choice"],
            "boundary_theorems": [{"name": "Tablet.Helper", "statement_hash": "h1"}],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "axiomization_check": {
                "kernel_axioms": ["Classical.choice", "sorryAx"],
                "boundary_theorems": ["Helper"],
                "agreed": false,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": ["sorryAx"],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": []
            }
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(
            parsed.status, CHECKER_DISAGREEMENT_STATUS,
            "disagreement must flip status to checker_disagreement",
        );
        assert!(
            parsed
                .errors
                .iter()
                .any(|e| e.contains("axiomization cross-check disagrees") && e.contains("sorryAx")),
            "diagnostic must surface the divergent axiom, got: {:?}",
            parsed.errors,
        );
        // Sub-object preserved for downstream callers. (Patch C-Q Q9
        // removed the MCA gate's dedicated `[internal] axiomization
        // disagrees` arm — the generic `local.status != "ok"` arm now
        // surfaces the structured `errors[0]` payload — but parser-
        // side `axiomization_check` round-trip is still required for
        // any future structured consumer.)
        let ax = parsed
            .axiomization_check
            .expect("axiomization_check populated");
        assert!(!ax.agreed);
        assert!(!ax.skipped);
        assert_eq!(ax.axcheck_only_axioms, vec!["sorryAx".to_string()]);
    }

    #[test]
    fn parse_local_closure_response_passes_through_when_axcheck_skipped() {
        // Plan §4.6.1 disable flag: `skipped: true` means the operator
        // turned off the cross-check (env var / CLI flag / config
        // kill-switch). The wrapper must accept the primary's verdict
        // unchanged — no status flip, no appended error.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["propext"],
            "boundary_theorems": [{"name": "Tablet.Helper", "statement_hash": "h1"}],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "axiomization_check": {
                "kernel_axioms": [],
                "boundary_theorems": [],
                "agreed": true,
                "skipped": true,
                "primary_only_axioms": [],
                "axcheck_only_axioms": [],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": []
            }
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(parsed.status, "ok", "skipped axcheck must not flip status");
        assert!(
            parsed.errors.is_empty(),
            "skipped axcheck must not append errors, got: {:?}",
            parsed.errors,
        );
        let ax = parsed
            .axiomization_check
            .expect("axiomization_check populated");
        assert!(ax.skipped);
        assert!(ax.agreed);
    }

    #[test]
    fn parse_local_closure_response_passes_through_when_axcheck_absent() {
        // Pre-merge state files (Patch A + initial Patch B) carry no
        // `axiomization_check` sub-object. The wrapper must accept the
        // primary's verdict unchanged — the field deserializes as
        // `None` and the invariant only fires on `Some(...)`.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["Classical.choice"],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": []
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(parsed.status, "ok");
        assert!(parsed.axiomization_check.is_none());
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn local_closure_axcheck_enabled_for_repo_defaults_true_without_config() {
        // Plan §4.6.1 default behavior: when no trellis.config.json /
        // lagent.config.json is on disk, the wrapper defaults to
        // `true` (run both collectors). A missing config is an operator
        // concern but must not silently disable the safety invariant.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(local_closure_axcheck_enabled_for_repo(&repo));
    }

    #[test]
    fn local_closure_axcheck_enabled_for_repo_reads_explicit_false_from_config() {
        // Plan §4.6.1 kill-switch: bridge config flag
        // `local_closure_axcheck_enabled: false` flips the wrapper to
        // pass `--no-axcheck` to the Lean script.
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("trellis.config.json"),
            r#"{"local_closure_axcheck_enabled": false}"#,
        )
        .unwrap();
        assert!(!local_closure_axcheck_enabled_for_repo(&repo));
    }

    #[test]
    fn local_closure_axcheck_enabled_for_repo_defaults_true_when_field_absent() {
        // Plan §4.6.1 default: explicit `true` is the operator's
        // affirmative consent to run both collectors; a config that
        // simply omits the field also gets `true` (the serde default).
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("trellis.config.json"), r#"{}"#).unwrap();
        assert!(local_closure_axcheck_enabled_for_repo(&repo));
    }

    #[test]
    fn parse_local_closure_response_passes_through_when_axcheck_agreed() {
        // Happy path: `agreed: true && skipped: false` → wrapper passes
        // through unchanged. Cross-check is the runtime-invariant
        // safety net; on agreement the primary's verdict drives every
        // downstream decision.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["Classical.choice"],
            "boundary_theorems": [{"name": "Tablet.Helper", "statement_hash": "h1"}],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "axiomization_check": {
                "kernel_axioms": ["Classical.choice"],
                "boundary_theorems": ["Helper"],
                "agreed": true,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": [],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": []
            }
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(parsed.status, "ok");
        assert!(parsed.errors.is_empty());
        let ax = parsed
            .axiomization_check
            .expect("axiomization_check populated");
        assert!(ax.agreed);
        assert!(!ax.skipped);
    }

    #[test]
    fn local_closure_probe_output_default_handles_missing_fields() {
        // Forward-compat regression: pre-Patch-A traces / replayed
        // events may carry a probe output payload that lacks fields
        // added in later patches. The DTO's `#[serde(default)]` plus
        // per-field `#[serde(default)]` must let a minimal envelope
        // deserialize cleanly. This pins plan §5.8's "every field
        // carries `#[serde(default)]`" contract.
        let minimal = serde_json::json!({"status": "ok"});
        let decoded: LocalClosureProbeOutput =
            serde_json::from_value(minimal).expect("deserialize minimal envelope");
        assert_eq!(decoded.status, "ok");
        assert!(decoded.kernel_axioms.is_empty());
        assert!(decoded.boundary_theorems.is_empty());
        assert!(decoded.strict_theorem_deps.is_empty());
        assert!(decoded.strict_definition_deps.is_empty());
        assert!(decoded.errors.is_empty());
        assert_eq!(decoded.raw_stdout, "");
        assert_eq!(decoded.raw_stderr, "");
        assert_eq!(decoded.returncode, 0);
        assert!(!decoded.timed_out);
        // Pre-merge state files lack the `axiomization_check`
        // sub-object; serde_default should yield `None` so the wrapper
        // treats it as "trust the primary's verdict" (plan §4.6.1).
        assert!(decoded.axiomization_check.is_none());

        // Truly empty object (no `status`) also deserializes; status
        // defaults to the empty string. Patch B/C will treat
        // `status != "ok"` as fail-closed, so an empty status string is
        // still an unambiguous failure signal at the gate.
        let empty: LocalClosureProbeOutput =
            serde_json::from_value(serde_json::json!({})).expect("deserialize empty object");
        assert_eq!(empty.status, "");
        assert!(empty.kernel_axioms.is_empty());
        assert!(empty.axiomization_check.is_none());
    }

    #[test]
    fn local_closure_lean_name_to_node_id_strips_prefix() {
        // Plan §4.5 NodeId-normalization rule (Patch A's
        // verbatim-fallback variant): `Tablet.Foo` → `Foo`, bare
        // `Foo` → `Foo`, non-Tablet names are passed through verbatim
        // (Patch C will refine this with present_nodes-aware
        // validation; Patch A intentionally keeps the unmappable case
        // visible by not silently dropping it).
        assert_eq!(
            local_closure_lean_name_to_node_id("Tablet.Foo"),
            NodeId::from("Foo"),
        );
        assert_eq!(
            local_closure_lean_name_to_node_id("Foo"),
            NodeId::from("Foo"),
        );
        // Non-Tablet name (e.g. a Mathlib const that the Lean script's
        // `isTabletConst` filter accidentally let through, or a
        // qualified Tablet sub-name like `Tablet.Foo.aux` which the
        // Patch A wrapper does NOT collapse): stored verbatim. The
        // strip rule only fires once, against the leading `Tablet.`.
        assert_eq!(
            local_closure_lean_name_to_node_id("Bar.Baz"),
            NodeId::from("Bar.Baz"),
        );
        assert_eq!(
            local_closure_lean_name_to_node_id("Tablet.Foo.aux"),
            NodeId::from("Foo.aux"),
        );
        assert_eq!(
            local_closure_lean_name_to_node_id("Mathlib.Topology.Foo"),
            NodeId::from("Mathlib.Topology.Foo"),
        );
        // Empty string: pass through. The wrapper's caller is
        // responsible for refusing empty bare names; the stripper
        // itself is a pure mapping.
        assert_eq!(local_closure_lean_name_to_node_id(""), NodeId::from(""),);
    }

    #[test]
    fn parse_local_closure_response_accepts_well_formed_envelope() {
        // Mirrors the server's `_handle_local_closure_axioms` happy
        // path: the script emits structured JSON and the Python handler
        // forwards it verbatim plus transport envelope fields.
        let envelope = serde_json::json!({
            "request_id": 42,
            "node": "Foo",
            "status": "ok",
            "root_kind": "theorem",
            "kernel_axioms": ["Classical.choice", "propext"],
            "boundary_theorems": [
                {"name": "Tablet.Helper", "statement_hash": "h1"},
                {"name": "Tablet.OtherHelper", "statement_hash": "h2"},
            ],
            "strict_theorem_deps": [
                {"name": "Tablet.StrictThm", "value_hash": "v1"},
            ],
            "strict_definition_deps": [
                {"name": "Tablet.StrictDef", "semantic_hash": "s1"},
            ],
            "errors": [],
            "stdout": "{...}",
            "stderr": "",
            "timed_out": false,
            "returncode": 0,
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(parsed.status, "ok");
        assert_eq!(
            parsed.kernel_axioms,
            BTreeSet::from(["Classical.choice".to_string(), "propext".to_string()]),
        );
        // Names are normalized via `local_closure_lean_name_to_node_id`
        // — `Tablet.` prefix stripped, NodeId values kept verbatim
        // beyond that point.
        assert_eq!(
            parsed.boundary_theorems,
            BTreeMap::from([
                (NodeId::from("Helper"), "h1".to_string()),
                (NodeId::from("OtherHelper"), "h2".to_string()),
            ]),
        );
        assert_eq!(
            parsed.strict_theorem_deps,
            BTreeMap::from([(NodeId::from("StrictThm"), "v1".to_string())]),
        );
        assert_eq!(
            parsed.strict_definition_deps,
            BTreeMap::from([(NodeId::from("StrictDef"), "s1".to_string())]),
        );
        assert!(parsed.errors.is_empty());
        assert_eq!(parsed.returncode, 0);
        assert!(!parsed.timed_out);
    }

    // ----- Phase II step 5: `oracles_used` field pins -----

    #[test]
    fn lean_probe_serializes_without_oracles_used_key() {
        // Byte-identity pin: a Lean-shaped probe leaves `oracles_used`
        // empty, and `skip_serializing_if = "BTreeSet::is_empty"` must
        // keep the key off the wire entirely (so the serialized JSON is
        // byte-identical to the pre-field shape). Control: `kernel_axioms`
        // (which has no skip_serializing_if) is still emitted even when
        // empty, proving the absence of `oracles_used` is the skip
        // attribute and not a generic empty-collection elision.
        let mut probe = LocalClosureProbeOutput::default();
        probe.status = "ok".to_string();
        // kernel_axioms left empty on purpose for the control.
        assert!(probe.oracles_used.is_empty());

        let value = serde_json::to_value(&probe).expect("serialize probe");
        let obj = value.as_object().expect("probe serializes as object");
        assert!(
            !obj.contains_key("oracles_used"),
            "empty `oracles_used` must be skip-serialized (load-bearing for byte-identity); \
             got keys {:?}",
            obj.keys().collect::<Vec<_>>(),
        );
        // Control: `kernel_axioms` has no skip_serializing_if, so the key
        // is present (as `[]`) even when empty. Confirms the elision is
        // specific to `oracles_used`.
        assert!(
            obj.contains_key("kernel_axioms"),
            "control: `kernel_axioms` (no skip attr) must still serialize when empty",
        );
    }

    #[test]
    fn oracles_used_bearing_payload_survives_parse_not_stripped() {
        // F3 pin: the field-stripping site is `parse_local_closure_response`
        // (the manual `obj.get(...)` field-picker), NOT `validate_trellis_*`.
        // A checker JSON carrying `oracles_used` must thread through the
        // parser into the struct (no silent drop), and a non-empty set
        // DOES re-emit the key on serialize (proving the skip is empty-only).
        let envelope = serde_json::json!({
            "node": "Foo",
            "status": "ok",
            "kernel_axioms": ["propext"],
            "oracles_used": ["z3", "metis"],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "stdout": "",
            "stderr": "",
            "timed_out": false,
            "returncode": 0,
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(
            parsed.oracles_used,
            BTreeSet::from(["metis".to_string(), "z3".to_string()]),
            "oracles_used must survive the parser, not be silently dropped",
        );
        // No spurious parse errors from threading the new field.
        assert!(
            parsed.errors.is_empty(),
            "a well-formed oracles_used array must not surface parse errors; got {:?}",
            parsed.errors,
        );
        // Non-empty oracles_used DOES re-emit the key (skip is empty-only).
        let value = serde_json::to_value(&parsed).expect("serialize probe");
        let obj = value.as_object().expect("probe serializes as object");
        assert!(
            obj.contains_key("oracles_used"),
            "non-empty `oracles_used` must serialize the key",
        );
        assert_eq!(
            obj.get("oracles_used"),
            Some(&serde_json::json!(["metis", "z3"])),
            "serialized oracles_used must carry the parsed values (BTreeSet order)",
        );

        // A non-array `oracles_used` is a soft parse-error → empty set,
        // mirroring the `kernel_axioms` non-array arm.
        let bad = serde_json::json!({
            "node": "Foo",
            "status": "ok",
            "kernel_axioms": [],
            "oracles_used": "z3",
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "stdout": "",
            "stderr": "",
            "timed_out": false,
            "returncode": 0,
        });
        let parsed_bad = parse_local_closure_response("Foo", bad).expect("parse envelope");
        assert!(parsed_bad.oracles_used.is_empty());
        assert!(
            parsed_bad
                .errors
                .iter()
                .any(|e| e.contains("oracles_used is not a JSON array")),
            "non-array oracles_used must push a parse error; got {:?}",
            parsed_bad.errors,
        );
    }

    #[test]
    fn probe_roundtrips_with_oracles_used() {
        // Serde round-trip pin for a probe carrying a non-empty
        // `oracles_used`: `from_value(to_value(v)) == v`.
        let mut probe = LocalClosureProbeOutput::default();
        probe.status = "ok".to_string();
        probe.kernel_axioms.insert("propext".to_string());
        probe.oracles_used.insert("z3".to_string());
        probe.oracles_used.insert("cvc5".to_string());

        let value = serde_json::to_value(&probe).expect("serialize probe");
        let back: LocalClosureProbeOutput =
            serde_json::from_value(value).expect("deserialize probe");
        assert_eq!(
            back, probe,
            "probe with oracles_used must survive round-trip"
        );
        assert_eq!(
            back.oracles_used,
            BTreeSet::from(["cvc5".to_string(), "z3".to_string()]),
        );
    }

    #[test]
    fn parse_local_closure_response_preserves_owner_scan_internal_error() {
        // FIX 2 regression: the owner-file authoring scan in
        // `lean_local_closure.lean` emits a fail-closed `internal_error`
        // envelope (empty dep/axiom arrays, the diagnostic in `errors[]`,
        // `axiomization_check.skipped=true`). The Rust parser must pass that
        // status through UNCHANGED — the gate sites treat any non-`ok` status
        // as fail-closed, so the forge is rejected at the gate. This pins the
        // fail-closed-at-the-gate contract.
        let envelope = serde_json::json!({
            "node": "ForgeProtectedEq",
            "status": "internal_error",
            "root_kind": "other",
            "kernel_axioms": [],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [
                "node authors reserved-shaped private auxiliary declaration(s) [eq_1]: ..."
            ],
            "axiomization_check": {
                "kernel_axioms": [],
                "boundary_theorems": [],
                "agreed": true,
                "skipped": true,
                "primary_only_axioms": [],
                "axcheck_only_axioms": [],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": [],
            },
            "stdout": "{...}",
            "stderr": "",
            "timed_out": false,
            "returncode": 0,
        });
        let parsed =
            parse_local_closure_response("ForgeProtectedEq", envelope).expect("parse envelope");
        assert_eq!(
            parsed.status, "internal_error",
            "owner-scan rejection status must survive parsing unchanged",
        );
        assert!(
            parsed
                .errors
                .iter()
                .any(|e| e.contains("reserved-shaped") && e.contains("eq_1")),
            "the reserved-shape diagnostic must survive in errors[]; got {:?}",
            parsed.errors,
        );
        // No dep/axiom keys leaked from the fail-closed envelope.
        assert!(parsed.kernel_axioms.is_empty());
        assert!(parsed.boundary_theorems.is_empty());
        assert!(parsed.strict_theorem_deps.is_empty());
        assert!(parsed.strict_definition_deps.is_empty());
    }

    #[test]
    fn parse_local_closure_response_rejects_non_object_root() {
        // The server should never emit a non-object envelope, but the
        // parser must surface a clean `Err` rather than panicking. Pins
        // the contract documented on `run_local_closure_axioms`: a
        // structurally-malformed response is an `Err(String)` with a
        // useful diagnostic.
        let arr = serde_json::json!([1, 2, 3]);
        let err =
            parse_local_closure_response("Foo", arr).expect_err("non-object envelope must error");
        assert!(
            err.contains("not a JSON object"),
            "diagnostic should explain shape mismatch, got: {err}",
        );
        assert!(
            err.contains("Foo"),
            "diagnostic should name the offending node, got: {err}",
        );
    }

    #[test]
    fn parse_local_closure_response_surfaces_field_shape_mismatches() {
        // When the script's per-list fields are not JSON arrays, the
        // parser emits a soft error inside `errors[]` and continues
        // (so partial data is still surfaced). This mirrors the Patch
        // A "graceful degradation" contract: a malformed `kernel_axioms`
        // shouldn't blow up the whole probe, it should be visible in
        // `errors` so the gate can still fail closed.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": "not-an-array",
            "boundary_theorems": {"oops": "object-not-array"},
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": ["pre-existing soft warning"],
        });
        let parsed = parse_local_closure_response("Bar", envelope).expect("parse with soft errors");
        // The original `errors` array is preserved, with parser-level
        // diagnostics appended.
        assert!(parsed
            .errors
            .iter()
            .any(|e| e == "pre-existing soft warning"));
        assert!(
            parsed
                .errors
                .iter()
                .any(|e| e.contains("kernel_axioms is not a JSON array")),
            "expected kernel_axioms shape diagnostic, got: {:?}",
            parsed.errors,
        );
        assert!(
            parsed
                .errors
                .iter()
                .any(|e| e.contains("boundary_theorems is not a JSON array")),
            "expected boundary_theorems shape diagnostic, got: {:?}",
            parsed.errors,
        );
        // Non-array fields contribute nothing to the parsed maps —
        // graceful degradation, not silent acceptance.
        assert!(parsed.kernel_axioms.is_empty());
        assert!(parsed.boundary_theorems.is_empty());
    }

    #[test]
    fn parse_local_closure_response_surfaces_pair_entry_malformations() {
        // A list entry missing one of the expected hash fields should
        // be skipped, but the parser must record an `errors` entry so
        // the gate can refuse to accept on a partial payload.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [
                {"name": "Tablet.Helper", "statement_hash": "h1"},
                // Malformed: missing `statement_hash`.
                {"name": "Tablet.NoHash"},
                // Malformed: missing `name`.
                {"statement_hash": "orphan"},
            ],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
        });
        let parsed =
            parse_local_closure_response("Baz", envelope).expect("parse with malformed pairs");
        // Well-formed entry survives.
        assert_eq!(
            parsed.boundary_theorems,
            BTreeMap::from([(NodeId::from("Helper"), "h1".to_string())]),
        );
        // Both malformed entries surfaced as errors.
        let malformed_count = parsed
            .errors
            .iter()
            .filter(|e| e.contains("boundary_theorems entry malformed"))
            .count();
        assert_eq!(
            malformed_count, 2,
            "two malformed entries should yield two diagnostics, got errors: {:?}",
            parsed.errors,
        );
    }

    #[test]
    fn parse_local_closure_response_handles_returncode_null() {
        // Documented contract on `parse_local_closure_response`:
        // `returncode` may be `null` for transport-level failures
        // (e.g. spawn error before the child exited). It should
        // coerce to 0; callers that need to distinguish must consult
        // `errors`. This guards against the gate accidentally
        // treating a transport failure as success because `returncode
        // != 0` was the only signal it consulted.
        let envelope = serde_json::json!({
            "status": "internal_error",
            "returncode": null,
            "errors": ["spawn failed: bwrap setup error"],
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse null returncode");
        assert_eq!(parsed.returncode, 0);
        assert_eq!(parsed.status, "internal_error");
        assert!(
            parsed.errors.iter().any(|e| e.contains("spawn failed")),
            "spawn-error message must survive into errors, got: {:?}",
            parsed.errors,
        );
    }

    // ===========================================================
    // Patch C-K Fix 1 — validate_probe_present_nodes
    // ===========================================================

    #[test]
    fn parse_local_closure_probe_output_rejects_unmappable_boundary_dep() {
        // Audit MEDIUM-HIGH: a Lean constant the script tagged as
        // Tablet but the kernel never ratified (e.g. a stale Tablet
        // declaration name, or a Preamble-internal helper that the
        // script's `isTabletConst` filter let through) must NOT become
        // a record dependency key. The wrapper validates dep keys
        // against `present_nodes` and fails closed by flipping status.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["propext"],
            "boundary_theorems": [
                {"name": "Tablet.RealHelper", "statement_hash": "h1"},
                {"name": "Tablet.StaleOrPreambleInternal", "statement_hash": "h2"},
            ],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        assert_eq!(probe.status, "ok", "pre-validation status is ok");
        // Only RealHelper is present; StaleOrPreambleInternal is not.
        let present_nodes: BTreeSet<NodeId> = [NodeId::from("Active"), NodeId::from("RealHelper")]
            .into_iter()
            .collect();
        // Patch C-N item 1: tag RealHelper as Proof so the kind validator
        // (boundary_theorems must be Proof-kind) doesn't double-fire on
        // the only legitimately-present dep.
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            (NodeId::from("RealHelper"), crate::NodeKind::Proof),
        ]
        .into_iter()
        .collect();
        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(
            probe.status, "internal_error",
            "unmappable dep must flip status to internal_error",
        );
        assert!(
            probe.errors.iter().any(|e| {
                e.contains("StaleOrPreambleInternal")
                    && e.contains("boundary_theorems")
                    && e.contains("Patch C-K")
            }),
            "diagnostic must surface the unmappable dep, got: {:?}",
            probe.errors,
        );
        // Mappable dep is unchanged (still in the map; the validator
        // does NOT prune entries — it just refuses the whole probe).
        assert!(
            probe
                .boundary_theorems
                .contains_key(&NodeId::from("RealHelper")),
            "validator must not mutate the dep map; it only flips status",
        );
    }

    #[test]
    fn validate_probe_present_nodes_rejects_generated_child_if_it_leaks() {
        // The Lean collector transparent-walks reserved generated
        // artifacts like `Foo.congr_simp`, but the Rust C-K guard must
        // not rely on that filter for soundness. If such a child leaks
        // into a dep map, it has no kernel lifecycle hook and must not be
        // parent-accepted just because `Foo` itself is present.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [
                {"name": "Tablet.SomePresent.congr_simp", "statement_hash": "h1"},
            ],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        assert_eq!(probe.status, "ok", "pre-validation status is ok");

        let present_nodes: BTreeSet<NodeId> = [NodeId::from("Active"), NodeId::from("SomePresent")]
            .into_iter()
            .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            (NodeId::from("SomePresent"), crate::NodeKind::Proof),
        ]
        .into_iter()
        .collect();

        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(
            probe.status, "internal_error",
            "generated-looking child dep must not be accepted via its parent node",
        );
        let diag = probe
            .errors
            .iter()
            .find(|e| e.contains("Patch C-K"))
            .expect("validation diagnostic appended");
        assert!(
            diag.contains("boundary_theorems=SomePresent.congr_simp"),
            "diagnostic must name the leaked child dep: {diag}",
        );
    }

    #[test]
    fn validate_probe_present_nodes_rejects_handwritten_owner_aux() {
        // Over-admission guard for the generated-member local-closure fix.
        // The Lean probe transparent-walks genuine generated members
        // (structure field projections, inductive constructors, class
        // methods, `extends`-diamond parent coercions, aux recursors), so
        // those never reach this validator as `Owner.child` dep keys. But a
        // HAND-WRITTEN `theorem Owner.realAux` is a `thmInfo` — not a
        // generated member — so it IS recorded as a dep key and must still
        // be rejected as a private auxiliary of node `Owner`. This is the
        // reliable pure-Rust form of fix case (d): a synthetic
        // `Owner.realAux` dep, whose `child` ("realAux") matches none of the
        // `generated_child` prefixes, falls through to the private-aux
        // branch and flips status to `internal_error`.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [
                {"name": "Tablet.Owner.realAux", "statement_hash": "h1"},
            ],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        assert_eq!(probe.status, "ok", "pre-validation status is ok");

        let present_nodes: BTreeSet<NodeId> = [NodeId::from("Active"), NodeId::from("Owner")]
            .into_iter()
            .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            // `Owner` is a structure (definition-kind), but the
            // private-aux classification keys only off the dotted dep
            // shape, not the kind.
            (NodeId::from("Owner"), crate::NodeKind::Definition),
        ]
        .into_iter()
        .collect();

        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(
            probe.status, "internal_error",
            "hand-written Owner.realAux must still be rejected (not over-admitted by the generated-member fix)",
        );
        let diag = probe
            .errors
            .iter()
            .find(|e| e.contains("private auxiliary"))
            .expect("private-auxiliary diagnostic appended");
        assert!(
            diag.contains("Owner.realAux") && diag.contains("`Owner`"),
            "diagnostic must name the hand-written aux and its owner node: {diag}",
        );
    }

    #[test]
    fn parse_local_closure_probe_output_accepts_when_all_deps_mappable() {
        // Negative: when every dep key is in present_nodes, the
        // validator is a no-op — status stays "ok" and no diagnostic
        // is appended. Covers all three dep categories (boundary,
        // strict_theorem, strict_definition).
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["propext"],
            "boundary_theorems": [
                {"name": "Tablet.Helper", "statement_hash": "h1"},
            ],
            "strict_theorem_deps": [
                {"name": "Tablet.StrictThm", "value_hash": "v1"},
            ],
            "strict_definition_deps": [
                {"name": "Tablet.StrictDef", "semantic_hash": "s1"},
            ],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        let present_nodes: BTreeSet<NodeId> = [
            NodeId::from("Active"),
            NodeId::from("Helper"),
            NodeId::from("StrictThm"),
            NodeId::from("StrictDef"),
        ]
        .into_iter()
        .collect();
        // Patch C-N item 1: kind-tag each present node so the kind
        // validator doesn't fire (boundary/strict_theorem deps are
        // Proof-kind; strict_definition deps are Definition-kind).
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            (NodeId::from("Helper"), crate::NodeKind::Proof),
            (NodeId::from("StrictThm"), crate::NodeKind::Proof),
            (NodeId::from("StrictDef"), crate::NodeKind::Definition),
        ]
        .into_iter()
        .collect();
        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(probe.status, "ok", "all-mappable case is a no-op");
        assert!(
            probe.errors.is_empty(),
            "no diagnostic should be appended; got: {:?}",
            probe.errors,
        );
    }

    #[test]
    fn validate_probe_present_nodes_catches_strict_theorem_and_definition_deps() {
        // Coverage: unmappable strict_theorem_dep and strict_definition_dep
        // also trigger fail-closed (not just boundaries). The diagnostic
        // includes labels distinguishing the dep category.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [],
            "strict_theorem_deps": [
                {"name": "Tablet.UnknownStrictThm", "value_hash": "v1"},
            ],
            "strict_definition_deps": [
                {"name": "Tablet.UnknownStrictDef", "semantic_hash": "s1"},
            ],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        let present_nodes: BTreeSet<NodeId> = [NodeId::from("Active")].into_iter().collect();
        // Patch C-N item 1: only Active is in present_nodes, so kinds
        // are irrelevant — the unmappable test fires on membership.
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> =
            [(NodeId::from("Active"), crate::NodeKind::Proof)]
                .into_iter()
                .collect();
        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(probe.status, "internal_error");
        let diag = probe
            .errors
            .iter()
            .find(|e| e.contains("Patch C-K"))
            .expect("validation diagnostic appended");
        assert!(
            diag.contains("strict_theorem_deps=UnknownStrictThm"),
            "strict_theorem_dep must be labeled in the diagnostic: {diag}",
        );
        assert!(
            diag.contains("strict_definition_deps=UnknownStrictDef"),
            "strict_definition_dep must be labeled in the diagnostic: {diag}",
        );
    }

    // ===========================================================
    // Patch C-K end-to-end — real Lean probe → parser → validator
    // ===========================================================
    //
    // These close the seam the smoke test cannot reach: they run the ACTUAL
    // `scripts/lean_local_closure.lean` on the namespaced fixtures, parse the
    // output through the REAL kernel parser (`parse_local_closure_response`),
    // and feed it through the REAL `validate_probe_present_nodes`. They are
    // skip-guarded (not `#[ignore]`d) on `lake` availability + a built fixture
    // tree, mirroring `kernel/tests/local_closure_smoke.rs`, so they execute
    // wherever the fixture is built and pass vacuously elsewhere.

    /// `(repo_root, fixture_root, script_path)` for the local-closure fixture.
    fn e2e_fixture_paths() -> (PathBuf, PathBuf, PathBuf) {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest
            .parent()
            .expect("kernel has a parent")
            .to_path_buf();
        let fixture_root = manifest
            .join("tests")
            .join("fixtures")
            .join("local_closure_smoke");
        let script = repo_root.join("scripts").join("lean_local_closure.lean");
        (repo_root, fixture_root, script)
    }

    /// Skip reason when the real probe cannot run (no `lake`, missing fixture
    /// tree, or the specific node's olean not built). `None` ⇒ runnable.
    fn e2e_skip_reason(required_oleans: &[&str]) -> Option<String> {
        let on_path = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("lake").is_file()))
            .unwrap_or(false);
        if !on_path {
            return Some("`lake` not on $PATH".to_string());
        }
        let (_, fixture_root, script) = e2e_fixture_paths();
        if !script.exists() {
            return Some(format!("probe script missing: {}", script.display()));
        }
        let lib = fixture_root
            .join(".lake")
            .join("build")
            .join("lib")
            .join("lean");
        for o in required_oleans {
            if !lib.join(o).exists() {
                return Some(format!(
                    "fixture not built ({} missing); run `cd {} && lake build`",
                    lib.join(o).display(),
                    fixture_root.display(),
                ));
            }
        }
        None
    }

    /// Run the real probe on `node` and parse its last stdout line through the
    /// production parser. Panics on spawn/parse failure (a hard error, not a
    /// skip — callers gate on `e2e_skip_reason` first).
    fn run_real_probe(node: &str) -> LocalClosureProbeOutput {
        let (_, fixture_root, script) = e2e_fixture_paths();
        let out = Command::new("lake")
            .arg("env")
            .arg("lean")
            .arg("--run")
            .arg(&script)
            .arg(node)
            .current_dir(&fixture_root)
            .output()
            .unwrap_or_else(|e| panic!("spawn lake env lean for {node}: {e}"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_else(|| {
                panic!(
                    "no JSON line for {node}; stdout=<<<{stdout}>>> stderr=<<<{}>>>",
                    String::from_utf8_lossy(&out.stderr)
                )
            });
        let json: serde_json::Value = serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("parse JSON for {node}: {e}; line={line}"));
        parse_local_closure_response(node, json)
            .unwrap_or_else(|e| panic!("parse_local_closure_response for {node}: {e}"))
    }

    #[test]
    fn validate_probe_present_nodes_accepts_real_namespaced_dep_probe() {
        // ITEM 2 (genuine end-to-end POSITIVE): run the REAL probe on the
        // namespaced consumer, parse via the REAL parser, then run the REAL
        // `validate_probe_present_nodes`. With the three namespaced deps
        // ratified as present nodes (their bare ids), the Patch C-K validator
        // must NOT flip status — proving the dep-fix's bare node ids are
        // exactly the present-node keys the kernel expects. Pre-fix the probe
        // emitted `crate_ns.<Node>` keys, which this validator rejected.
        let required = &[
            "Tablet/NamespacedConsumer.olean",
            "Tablet/NamespacedThm.olean",
            "Tablet/NamespacedStrictThm.olean",
            "Tablet/NamespacedDef.olean",
        ];
        if let Some(reason) = e2e_skip_reason(required) {
            eprintln!(
                "SKIP validate_probe_present_nodes_accepts_real_namespaced_dep_probe: {reason}"
            );
            return;
        }
        let mut probe = run_real_probe("NamespacedConsumer");
        assert_eq!(
            probe.status, "ok",
            "real namespaced-consumer probe must be ok; errors: {:?}",
            probe.errors
        );
        // Sanity: the parser yielded the bare node ids as keys.
        assert!(
            probe
                .boundary_theorems
                .contains_key(&NodeId::from("NamespacedThm")),
            "boundary key must be bare `NamespacedThm`; got {:?}",
            probe.boundary_theorems.keys().collect::<Vec<_>>(),
        );
        assert!(
            probe
                .strict_definition_deps
                .contains_key(&NodeId::from("NamespacedDef")),
            "strict_def key must be bare `NamespacedDef`; got {:?}",
            probe.strict_definition_deps.keys().collect::<Vec<_>>(),
        );
        assert!(
            probe
                .strict_theorem_deps
                .contains_key(&NodeId::from("NamespacedStrictThm")),
            "strict_thm key must be bare `NamespacedStrictThm`; got {:?}",
            probe.strict_theorem_deps.keys().collect::<Vec<_>>(),
        );

        // Ratify the consumer + its three deps with their expected kinds.
        let present_nodes: BTreeSet<NodeId> = [
            NodeId::from("NamespacedConsumer"),
            NodeId::from("NamespacedThm"),
            NodeId::from("NamespacedStrictThm"),
            NodeId::from("NamespacedDef"),
        ]
        .into_iter()
        .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("NamespacedConsumer"), crate::NodeKind::Proof),
            (NodeId::from("NamespacedThm"), crate::NodeKind::Proof),
            (NodeId::from("NamespacedStrictThm"), crate::NodeKind::Proof),
            (NodeId::from("NamespacedDef"), crate::NodeKind::Definition),
        ]
        .into_iter()
        .collect();

        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(
            probe.status, "ok",
            "Patch C-K must ACCEPT the real probe's bare namespaced-dep ids; errors: {:?}",
            probe.errors,
        );
        assert!(
            !probe.errors.iter().any(|e| e.contains("Patch C-K")),
            "no Patch C-K diagnostic expected; errors: {:?}",
            probe.errors,
        );
    }

    #[test]
    fn validate_probe_present_nodes_rejects_real_namespaced_authored_aux() {
        // ITEM 2 (genuine end-to-end NEGATIVE / SOUNDNESS): run the REAL probe
        // on the namespaced consumer of a namespaced AUTHORED auxiliary
        // (`crate_ns.NamespacedForgeAux.realAux`). The dep-fix's
        // principal-declaration guard leaves that key DOTTED (its final
        // component `realAux` ≠ the module-suffix node id `NamespacedForgeAux`),
        // so even with `NamespacedForgeAux` ratified as a present node the
        // REAL `validate_probe_present_nodes` must REJECT (fail-closed): the
        // dotted aux key is not a present node. Were the guard absent, the key
        // would be the present id `NamespacedForgeAux` and wrongly pass — the
        // soundness hole this test locks shut.
        let required = &[
            "Tablet/UsesNamespacedForgeAux.olean",
            "Tablet/NamespacedForgeAux.olean",
        ];
        if let Some(reason) = e2e_skip_reason(required) {
            eprintln!(
                "SKIP validate_probe_present_nodes_rejects_real_namespaced_authored_aux: {reason}"
            );
            return;
        }
        let mut probe = run_real_probe("UsesNamespacedForgeAux");
        assert_eq!(
            probe.status, "ok",
            "probe itself records the dep (rejection is the kernel's job); errors: {:?}",
            probe.errors,
        );
        // The aux dep key is DOTTED and is NOT the bare present-node id.
        let aux_key = probe
            .boundary_theorems
            .keys()
            .find(|k| k.as_str().ends_with(".realAux"))
            .unwrap_or_else(|| {
                panic!(
                    "aux dep must be a dotted key ending `.realAux`; got {:?}",
                    probe.boundary_theorems.keys().collect::<Vec<_>>()
                )
            })
            .clone();
        assert!(
            aux_key.as_str().contains('.'),
            "aux dep key `{}` must stay dotted (not collapsed to a bare node id)",
            aux_key.as_str(),
        );
        assert!(
            !probe
                .boundary_theorems
                .contains_key(&NodeId::from("NamespacedForgeAux")),
            "aux must NOT be re-keyed onto the bare present node id `NamespacedForgeAux`",
        );

        // Ratify the consumer AND the aux's owner node as present (the
        // realistic state): the dotted aux key is STILL not a present node, so
        // the validator must fail closed.
        let present_nodes: BTreeSet<NodeId> = [
            NodeId::from("UsesNamespacedForgeAux"),
            NodeId::from("NamespacedForgeAux"),
        ]
        .into_iter()
        .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (
                NodeId::from("UsesNamespacedForgeAux"),
                crate::NodeKind::Proof,
            ),
            (NodeId::from("NamespacedForgeAux"), crate::NodeKind::Proof),
        ]
        .into_iter()
        .collect();

        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(
            probe.status, "internal_error",
            "Patch C-K must REJECT the dotted namespaced authored-aux dep; errors: {:?}",
            probe.errors,
        );
        assert!(
            probe
                .errors
                .iter()
                .any(|e| e.contains("Patch C-K") || e.contains("private-auxiliary")),
            "rejection must carry a fail-closed diagnostic; errors: {:?}",
            probe.errors,
        );
    }

    #[test]
    fn validate_probe_present_nodes_accepts_real_definition_dep_module_attribution() {
        // DEFINITION-DEP MODULE-ATTRIBUTION (the dec2flt `strict_definition_deps`
        // fix), genuine end-to-end POSITIVE: run the REAL probe on two PV-shape
        // `def` consumers and assert each `strict_definition_dep` is keyed by its
        // dep's DECLARING-MODULE node id, with NO principal `declMatchesStem`
        // gate — so the kernel's Patch C-K present-node validator accepts them.
        //
        // Two categories of NON-principal definition dep that pre-fix fell through
        // to the raw dotted Lean `Name` (which Patch C-K fail-closed):
        //   (a) `UsesPreambleStruct` -> struct `crate_ns.PreambleBiasedFp` declared
        //       in the shared `Tablet.Preamble` module (node id `Preamble`); the
        //       decl's final component does NOT sanitize to the stem `Preamble`.
        //   (b) `UsesLoopHelper` -> non-principal Aeneas-shape co-generated helper
        //       `crate_ns.LoopHost_loop0` living inside the `Tablet.LoopHost`
        //       module (node id `LoopHost`); the helper is not name-shaped as a
        //       generated artifact and its final component does NOT sanitize to
        //       the stem `LoopHost`.
        // Post-fix `depDefKeyName` attributes both to the bare host-node ids
        // `Preamble` / `LoopHost`.
        let required = &[
            "Tablet/UsesPreambleStruct.olean",
            "Tablet/UsesLoopHelper.olean",
            "Tablet/LoopHost.olean",
            "Tablet/Preamble.olean",
        ];
        if let Some(reason) = e2e_skip_reason(required) {
            eprintln!(
                "SKIP validate_probe_present_nodes_accepts_real_definition_dep_module_attribution: {reason}"
            );
            return;
        }

        // Category (a): Preamble-shared structure.
        let mut probe_a = run_real_probe("UsesPreambleStruct");
        assert_eq!(
            probe_a.status, "ok",
            "real Preamble-struct consumer probe must be ok; errors: {:?}",
            probe_a.errors
        );
        assert!(
            probe_a
                .strict_definition_deps
                .contains_key(&NodeId::from("Preamble")),
            "Preamble-shared struct dep must be keyed by host-node id `Preamble`; got {:?}",
            probe_a.strict_definition_deps.keys().collect::<Vec<_>>(),
        );
        assert!(
            !probe_a
                .strict_definition_deps
                .keys()
                .any(|k| k.as_str().contains('.')),
            "no definition dep may stay a dotted Lean Name; got {:?}",
            probe_a.strict_definition_deps.keys().collect::<Vec<_>>(),
        );

        // Category (b): non-principal Aeneas-shape co-generated `_loop` helper.
        let mut probe_b = run_real_probe("UsesLoopHelper");
        assert_eq!(
            probe_b.status, "ok",
            "real loop-helper consumer probe must be ok; errors: {:?}",
            probe_b.errors
        );
        assert!(
            probe_b
                .strict_definition_deps
                .contains_key(&NodeId::from("LoopHost")),
            "co-generated loop helper dep must be keyed by host-node id `LoopHost`; got {:?}",
            probe_b.strict_definition_deps.keys().collect::<Vec<_>>(),
        );
        assert!(
            !probe_b
                .strict_definition_deps
                .keys()
                .any(|k| k.as_str().contains('.')),
            "no definition dep may stay a dotted Lean Name; got {:?}",
            probe_b.strict_definition_deps.keys().collect::<Vec<_>>(),
        );

        // The kernel's Patch C-K present-node validation must ACCEPT both with
        // the host nodes ratified as present. `Preamble` is the shared import
        // root, not a Definition node, but it legitimately hosts strict
        // definition deps for shared structures/axioms.
        let present_nodes: BTreeSet<NodeId> = [
            NodeId::from("UsesPreambleStruct"),
            NodeId::from("UsesLoopHelper"),
            NodeId::from("Preamble"),
            NodeId::from("LoopHost"),
        ]
        .into_iter()
        .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (
                NodeId::from("UsesPreambleStruct"),
                crate::NodeKind::Definition,
            ),
            (NodeId::from("UsesLoopHelper"), crate::NodeKind::Definition),
            (NodeId::from("Preamble"), crate::NodeKind::Preamble),
            (NodeId::from("LoopHost"), crate::NodeKind::Definition),
        ]
        .into_iter()
        .collect();

        for (label, probe) in [
            ("UsesPreambleStruct", &mut probe_a),
            ("UsesLoopHelper", &mut probe_b),
        ] {
            validate_probe_present_nodes(probe, &present_nodes, &node_kinds);
            assert_eq!(
                probe.status, "ok",
                "Patch C-K must ACCEPT {label}'s declaring-module def-dep id; errors: {:?}",
                probe.errors,
            );
            assert!(
                !probe.errors.iter().any(|e| e.contains("Patch C-K")),
                "no Patch C-K diagnostic expected for {label}; errors: {:?}",
                probe.errors,
            );
        }
    }

    // ===========================================================
    // Patch C-N item 1 — dep KIND validation
    // ===========================================================

    #[test]
    fn validate_probe_kinds_accepts_preamble_strict_definition_dep() {
        // Preamble is the import root for shared structures/axioms. The
        // local-closure probe can legitimately emit it under
        // `strict_definition_deps`, while live PV state records it as
        // `NodeKind::Preamble` rather than `NodeKind::Definition`.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [
                {"name": "Tablet.Preamble", "semantic_hash": "s1"},
            ],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        let present_nodes: BTreeSet<NodeId> = [NodeId::from("Active"), NodeId::from("Preamble")]
            .into_iter()
            .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            (NodeId::from("Preamble"), crate::NodeKind::Preamble),
        ]
        .into_iter()
        .collect();

        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);

        assert_eq!(
            probe.status, "ok",
            "Preamble strict_definition_deps entry must be accepted; errors: {:?}",
            probe.errors,
        );
        assert!(
            probe.errors.is_empty(),
            "Preamble allowance must not append diagnostics; got: {:?}",
            probe.errors,
        );
    }

    #[test]
    fn validate_probe_kinds_rejects_preamble_kind_outside_preamble_strict_def() {
        // The Preamble allowance is intentionally narrow: only the key
        // `Preamble` under `strict_definition_deps` may be accepted with
        // `NodeKind::Preamble`. A different key with Preamble kind, or
        // Preamble in a theorem dep category, is still a kind mismatch.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [
                {"name": "Tablet.Preamble", "statement_hash": "h1"},
            ],
            "strict_theorem_deps": [],
            "strict_definition_deps": [
                {"name": "Tablet.NotPreamble", "semantic_hash": "s1"},
            ],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        let present_nodes: BTreeSet<NodeId> = [
            NodeId::from("Active"),
            NodeId::from("Preamble"),
            NodeId::from("NotPreamble"),
        ]
        .into_iter()
        .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            (NodeId::from("Preamble"), crate::NodeKind::Preamble),
            (NodeId::from("NotPreamble"), crate::NodeKind::Preamble),
        ]
        .into_iter()
        .collect();

        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);

        assert_eq!(probe.status, "internal_error");
        let diag = probe
            .errors
            .iter()
            .find(|e| e.contains("Patch C-N"))
            .expect("kind-validation diagnostic appended");
        assert!(
            diag.contains("boundary_theorems=Preamble"),
            "Preamble theorem-category mismatch must be rejected: {diag}",
        );
        assert!(
            diag.contains("strict_definition_deps=NotPreamble"),
            "non-Preamble key with Preamble kind must be rejected: {diag}",
        );
    }

    #[test]
    fn validate_probe_kinds_rejects_theorem_dep_listed_as_definition() {
        // A dep emitted under `strict_definition_deps` whose recorded
        // kind is `Proof` (theorem) must trigger fail-closed: kinds
        // disagree with the dep category. Mirrors the unmappable-dep
        // pattern: flip status to internal_error and append a structured
        // diagnostic naming the offending dep.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [
                {"name": "Tablet.MisLabeledThm", "semantic_hash": "s1"},
            ],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        let present_nodes: BTreeSet<NodeId> =
            [NodeId::from("Active"), NodeId::from("MisLabeledThm")]
                .into_iter()
                .collect();
        // MisLabeledThm is recorded as a theorem (Proof kind) in
        // node_kinds — but the probe emitted it as a strict_definition
        // dep. That's the kind-confusion we're catching.
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            (NodeId::from("MisLabeledThm"), crate::NodeKind::Proof),
        ]
        .into_iter()
        .collect();
        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(
            probe.status, "internal_error",
            "kind mismatch must flip status to internal_error",
        );
        let diag = probe
            .errors
            .iter()
            .find(|e| e.contains("Patch C-N"))
            .expect("kind-validation diagnostic appended");
        assert!(
            diag.contains("strict_definition_deps=MisLabeledThm"),
            "diagnostic must label the dep and category: {diag}",
        );
    }

    /// The 2145 incident: a proof depended on another node's namespaced
    /// auxiliary theorem (`Owner.Aux`). The probe must still fail closed,
    /// but with the private-auxiliary rule and remedy — the generic
    /// "not in present_nodes" internal-error text sent the worker into
    /// inlining the auxiliary's whole proof.
    #[test]
    fn probe_rejects_private_auxiliary_dep_with_remedy() {
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [
                {"name": "Tablet.OwnerNode.WithMembership",
                 "statement_hash": "h1"},
            ],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        let present_nodes: BTreeSet<NodeId> = [NodeId::from("Active"), NodeId::from("OwnerNode")]
            .into_iter()
            .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            (NodeId::from("OwnerNode"), crate::NodeKind::Proof),
        ]
        .into_iter()
        .collect();
        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(
            probe.status, "internal_error",
            "private-aux dep must still fail closed",
        );
        let diag = probe
            .errors
            .iter()
            .find(|e| e.contains("private auxiliary declaration"))
            .expect("private-aux diagnostic appended");
        assert!(
            diag.contains("node `OwnerNode`") && diag.contains("factor the auxiliary out"),
            "diagnostic must name the owner and the remedy: {diag}",
        );
        assert!(
            !probe.errors.iter().any(|e| e.contains("Patch C-K")),
            "private-aux case must not fall through to the generic \
             unmappable diagnostic: {:?}",
            probe.errors,
        );
    }

    #[test]
    fn validate_probe_kinds_rejects_definition_dep_listed_as_theorem() {
        // Mirror: a dep emitted under `strict_theorem_deps` whose
        // recorded kind is `Definition` must also fail closed. The
        // diagnostic must label the dep + category so an operator can
        // tell whether the probe or the kernel kind map is wrong. Also
        // covers `boundary_theorems` in the same probe (a definition
        // listed as a boundary should be rejected too).
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [
                {"name": "Tablet.MisLabeledBoundary", "statement_hash": "h1"},
            ],
            "strict_theorem_deps": [
                {"name": "Tablet.MisLabeledDef", "value_hash": "v1"},
            ],
            "strict_definition_deps": [],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        let present_nodes: BTreeSet<NodeId> = [
            NodeId::from("Active"),
            NodeId::from("MisLabeledBoundary"),
            NodeId::from("MisLabeledDef"),
        ]
        .into_iter()
        .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            (
                NodeId::from("MisLabeledBoundary"),
                crate::NodeKind::Definition,
            ),
            (NodeId::from("MisLabeledDef"), crate::NodeKind::Definition),
        ]
        .into_iter()
        .collect();
        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(probe.status, "internal_error");
        let diag = probe
            .errors
            .iter()
            .find(|e| e.contains("Patch C-N"))
            .expect("kind-validation diagnostic appended");
        assert!(
            diag.contains("strict_theorem_deps=MisLabeledDef"),
            "strict_theorem dep must be labeled: {diag}",
        );
        assert!(
            diag.contains("boundary_theorems=MisLabeledBoundary"),
            "boundary_theorems dep must be labeled: {diag}",
        );
    }

    #[test]
    fn validate_probe_kinds_accepts_matched_kinds() {
        // Negative: when every dep's recorded kind matches the dep
        // category's expected kind (boundary/strict_theorem → Proof;
        // strict_definition → Definition), the kind validator is a
        // no-op. Together with the prior all-mappable case this guards
        // against false positives on the happy path.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["propext"],
            "boundary_theorems": [
                {"name": "Tablet.HelperA", "statement_hash": "h1"},
            ],
            "strict_theorem_deps": [
                {"name": "Tablet.ThmB", "value_hash": "v1"},
            ],
            "strict_definition_deps": [
                {"name": "Tablet.DefC", "semantic_hash": "s1"},
            ],
            "errors": [],
        });
        let mut probe = parse_local_closure_response("Active", envelope).expect("parse envelope");
        let present_nodes: BTreeSet<NodeId> = [
            NodeId::from("Active"),
            NodeId::from("HelperA"),
            NodeId::from("ThmB"),
            NodeId::from("DefC"),
        ]
        .into_iter()
        .collect();
        let node_kinds: BTreeMap<NodeId, crate::NodeKind> = [
            (NodeId::from("Active"), crate::NodeKind::Proof),
            (NodeId::from("HelperA"), crate::NodeKind::Proof),
            (NodeId::from("ThmB"), crate::NodeKind::Proof),
            (NodeId::from("DefC"), crate::NodeKind::Definition),
        ]
        .into_iter()
        .collect();
        validate_probe_present_nodes(&mut probe, &present_nodes, &node_kinds);
        assert_eq!(probe.status, "ok", "matched kinds must keep status ok");
        assert!(
            probe.errors.is_empty(),
            "no diagnostic should fire on matched kinds; got: {:?}",
            probe.errors,
        );
    }

    // ===========================================================
    // Patch C-K Fix 3 — axcheck collector crash → loud internal_error
    // ===========================================================

    #[test]
    fn parse_axiomization_check_crash_propagates_as_internal_error() {
        // Audit MEDIUM: when the Lean script's secondary axcheck
        // collector throws, the script previously emitted
        // `axiomization_check { skipped: true }` with no error
        // indication. That silently disabled the cross-check. The new
        // Lean-side behavior emits `agreed: false, skipped: false`
        // plus `axiomization_check.error: "<msg>"` and a top-level
        // `errors: ["axiomization_check_crash: ..."]`. The Rust parser
        // recognizes the crash via the top-level prefix (or the
        // sub-object's `error` field) and surfaces a distinct
        // diagnostic, separate from the "disagrees with primary"
        // diagnostic that fires on a real cross-check disagreement.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["Classical.choice"],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": ["axiomization_check_crash: simulated traversal failure"],
            "axiomization_check": {
                "kernel_axioms": [],
                "boundary_theorems": [],
                "agreed": false,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": [],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": [],
                "error": "simulated traversal failure"
            }
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(
            parsed.status, "internal_error",
            "axcheck crash must flip status to internal_error",
        );
        // The distinct "collector crashed" diagnostic must fire,
        // NOT the generic "disagrees" diagnostic.
        assert!(
            parsed.errors.iter().any(|e| {
                e.contains("axiomization cross-check collector crashed")
                    && e.contains("simulated traversal failure")
            }),
            "crash diagnostic must surface the exception message, got: {:?}",
            parsed.errors,
        );
        assert!(
            !parsed
                .errors
                .iter()
                .any(|e| e.contains("axiomization cross-check disagrees")),
            "must NOT emit the disagreement diagnostic on a crash, got: {:?}",
            parsed.errors,
        );
    }

    #[test]
    fn axiomization_check_typed_error_field_propagates_as_internal_error() {
        // Patch C-N item 4: confirm the typed `AxiomizationCheckOutput.error`
        // field, NOT the raw-JSON detour, drives the crash diagnostic.
        // Envelope mirrors the crash case but OMITS the top-level
        // `axiomization_check_crash:` prefix in `errors` — only the
        // typed sub-object field carries the crash signal. The Rust
        // parser's fall-through (typed field first, then top-level
        // prefix) means this still flips status and surfaces the
        // collector-crashed diagnostic, proving the typed field alone
        // is sufficient. Guards against regressions where someone
        // removes the typed-field branch and accidentally relies on
        // the top-level fallback that was tested by C-K's test.
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["Classical.choice"],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            // No `axiomization_check_crash:` prefix in top-level errors.
            "errors": [],
            "axiomization_check": {
                "kernel_axioms": [],
                "boundary_theorems": [],
                "agreed": false,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": [],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": [],
                "error": "typed-field-only crash payload"
            }
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(
            parsed.status, "internal_error",
            "typed error field alone must flip status to internal_error",
        );
        assert!(
            parsed.errors.iter().any(|e| {
                e.contains("axiomization cross-check collector crashed")
                    && e.contains("typed-field-only crash payload")
            }),
            "crash diagnostic must surface the typed field's message: {:?}",
            parsed.errors,
        );
        assert!(
            !parsed
                .errors
                .iter()
                .any(|e| e.contains("axiomization cross-check disagrees")),
            "must NOT emit the disagreement diagnostic when typed error is set: {:?}",
            parsed.errors,
        );
        // The typed field round-trips into the parsed output so
        // downstream consumers don't need to re-parse JSON.
        let ax = parsed
            .axiomization_check
            .expect("axiomization_check populated");
        assert_eq!(
            ax.error.as_deref(),
            Some("typed-field-only crash payload"),
            "typed error field must round-trip into AxiomizationCheckOutput",
        );
    }

    #[test]
    fn parse_axiomization_check_disagreement_still_emits_disagreement_diagnostic() {
        // Regression: a true disagreement (no `error` field present
        // and no top-level `axiomization_check_crash:` prefix) must
        // still emit the existing "disagrees" diagnostic — Fix 3
        // only carves out the crash path. The status is now
        // `checker_disagreement` (fail-loudly halt classification);
        // the diagnostic text itself is unchanged.
        let _env = HaltMarkerEnvOverride::pointing_into(tempdir().unwrap().path());
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["Classical.choice"],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "axiomization_check": {
                "kernel_axioms": ["Classical.choice", "sorryAx"],
                "boundary_theorems": [],
                "agreed": false,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": ["sorryAx"],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": []
            }
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(parsed.status, CHECKER_DISAGREEMENT_STATUS);
        assert!(
            parsed
                .errors
                .iter()
                .any(|e| e.contains("axiomization cross-check disagrees") && e.contains("sorryAx")),
            "disagreement diagnostic must still fire when there is no crash signal: {:?}",
            parsed.errors,
        );
        assert!(
            !parsed
                .errors
                .iter()
                .any(|e| e.contains("collector crashed")),
            "must NOT emit the crash diagnostic when no crash signal present, got: {:?}",
            parsed.errors,
        );
    }

    // -------------------- Patch C-R helper-probe tests --------------------
    //
    // The runtime CLI's `proof_worker_delta_step_result` now runs the
    // local-closure probe on every sorry-free helper that becomes
    // record-eligible in a delta (new births + sorryd→sorry-free
    // transitions on non-active proof_nodes), not just the MCA-gated
    // active node. These tests pin the new probe-invocation contract
    // and the rejection paths layered on top.

    /// Worker burst that adds a new sorry-free helper with no active
    /// edit and no MCA gate. The helper-probe loop must run on the new
    /// helper, accept its probe payload, and stash the result keyed by
    /// the helper's NodeId in `local_closure_results`. Before Patch
    /// C-R, no probe ran at all here — the helper joined `proof_nodes`
    /// silently, satisfying neither the §527 invariant nor the §1.1
    /// safety story.
    #[test]
    fn patch_c_r_probes_newly_added_sorry_free_helper() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        // Touch A.lean (identical content, snapshot-hash flip) so the
        // delta has _something_ to gate; the helper-probe path doesn't
        // depend on the active edit itself.
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.NewHelper\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- patch_c_r v1\n";
        touch_active_lean(&repo, new_active);
        // Worker authors a sorry-free helper. Its TeX env is
        // theorem-bearing → kind=Proof → record-eligible.
        write(
            &repo.join("Tablet/NewHelper.lean"),
            "-- [TABLET NODE: NewHelper]\nimport Tablet.Preamble\n\ntheorem NewHelper (n : Nat) : n + 0 = n := Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/NewHelper.tex"),
            "\\begin{theorem}For every natural $n$, $n + 0 = n$.\\end{theorem}\n\\begin{proof}Right-zero law.\\end{proof}\n",
        );
        // Probe response (shared by all `local-closure-axioms` calls).
        // Boundary keys must round-trip the Tablet. prefix strip.
        set_local_closure_canned_response(
            &repo,
            Some(
                r#"{"status":"ok","kernel_axioms":["propext"],"boundary_theorems":[],"strict_theorem_deps":[],"strict_definition_deps":[],"errors":[]}"#,
            ),
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            // Pre-delta present_nodes: just A + Preamble. NewHelper is
            // not present yet — it's the worker's addition.
            &set(&["Preamble", "A"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            // No pre-delta sorryd info needed for the new-birth case;
            // the helper-probe loop fires on `delta_scope.new_lean_files`
            // unconditionally.
            &BTreeSet::new(),
            &declaration_hash(&original_active, "A"),
            // Restructure mode: lets the active node be edited (it's
            // not authorized but `active_changed=false` since the
            // signature is unchanged; the active edit here is just a
            // comment-level body tweak that gets through under Easy/
            // Local rules). Use Easy for the simplest possible setup.
            WorkerProofDeltaMode::Easy,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            // allow_new_obligations: the helper is sorry-free, but the
            // gate computes `evaluated.ok` strictly; on the stub'd
            // check.py the axiom audit defaults to "no axioms" which
            // satisfies the per-node approved set, so `evaluated.ok`
            // succeeds and we can use `false` here.
            false,
            // must_close_active: false → no MCA gate. The active node
            // is already sorry-free pre-delta, so this is fine.
            false,
        )
        .unwrap();

        // Probe ran and accepted. The helper's payload should be
        // stashed under its NodeId.
        assert!(
            result.ok,
            "Patch C-R must accept a clean new-helper probe: {:?}",
            result.errors,
        );
        assert!(
            result
                .local_closure_results
                .contains_key(&NodeId::from("NewHelper")),
            "helper-probe loop must stash probe payload keyed by the \
             new helper's NodeId; got keys: {:?}",
            result.local_closure_results.keys().collect::<Vec<_>>(),
        );
        let helper_probe = result
            .local_closure_results
            .get(&NodeId::from("NewHelper"))
            .unwrap();
        assert_eq!(helper_probe.status, "ok");
        assert!(
            helper_probe.kernel_axioms.contains("propext"),
            "stashed probe must carry kernel_axioms from canned response",
        );
    }

    /// New helper whose probe returns an unapproved kernel axiom must
    /// reject the burst with an `[axiom]` error mentioning the helper's
    /// node name (so the operator can locate the offending source).
    #[test]
    fn patch_c_r_rejects_new_helper_with_unapproved_axiom() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.NewHelper\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- patch_c_r v2\n";
        touch_active_lean(&repo, new_active);
        write(
            &repo.join("Tablet/NewHelper.lean"),
            "-- [TABLET NODE: NewHelper]\nimport Tablet.Preamble\n\ntheorem NewHelper (n : Nat) : n + 0 = n := Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/NewHelper.tex"),
            "\\begin{theorem}For every natural $n$, $n + 0 = n$.\\end{theorem}\n\\begin{proof}Right-zero law.\\end{proof}\n",
        );
        // Probe surfaces `Lean.ofReduceBool` — an unapproved kernel
        // axiom (per the canonical four).
        set_local_closure_canned_response(
            &repo,
            Some(r#"{"status":"ok","kernel_axioms":["Lean.ofReduceBool"]}"#),
        );
        // Patch C-R-c: A is also probed under the broader trigger
        // (class-c: pre-delta sorry-free + .lean modified). Give A a
        // clean per-node response so the assertion below isolates the
        // helper-rejection path.
        set_local_closure_canned_response_for_node(
            &repo,
            "A",
            Some(r#"{"status":"ok","kernel_axioms":["propext"]}"#),
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &declaration_hash(&original_active, "A"),
            WorkerProofDeltaMode::Easy,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            false,
            false,
        )
        .unwrap();

        assert!(
            !result.ok,
            "Patch C-R must reject when a new helper carries an \
             unapproved kernel axiom",
        );
        assert!(
            result.errors.iter().any(|e| e.starts_with("[axiom]")
                && e.contains("NewHelper")
                && e.contains("Lean.ofReduceBool")),
            "rejection must name both the offending helper and axiom: {:?}",
            result.errors,
        );
        // On rejection, `local_closure_results` must be empty —
        // matching the MCA rejection contract.
        assert!(
            result.local_closure_results.is_empty(),
            "rejection must not stash probe payloads",
        );
    }

    /// Probe failure (`status != "ok"`) on a new helper must reject the
    /// burst with an `[internal]` error that names the helper.
    #[test]
    fn patch_c_r_rejects_new_helper_with_probe_internal_error() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.NewHelper\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- patch_c_r v3\n";
        touch_active_lean(&repo, new_active);
        write(
            &repo.join("Tablet/NewHelper.lean"),
            "-- [TABLET NODE: NewHelper]\nimport Tablet.Preamble\n\ntheorem NewHelper (n : Nat) : n + 0 = n := Nat.add_zero n\n",
        );
        write(
            &repo.join("Tablet/NewHelper.tex"),
            "\\begin{theorem}For every natural $n$, $n + 0 = n$.\\end{theorem}\n\\begin{proof}Right-zero law.\\end{proof}\n",
        );
        set_local_closure_canned_response(
            &repo,
            Some(r#"{"status":"internal_error","errors":["elaborator collapsed"]}"#),
        );
        // Patch C-R-c: A is also probed under the broader trigger
        // (class-c). Clean per-node response for A isolates the
        // assertion on the helper.
        set_local_closure_canned_response_for_node(
            &repo,
            "A",
            Some(r#"{"status":"ok","kernel_axioms":["propext"]}"#),
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &declaration_hash(&original_active, "A"),
            WorkerProofDeltaMode::Easy,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            false,
            false,
        )
        .unwrap();

        assert!(
            !result.ok,
            "Patch C-R must reject when a new helper's probe reports \
             status != ok",
        );
        assert!(
            result.errors.iter().any(|e| e.starts_with("[internal]")
                && e.contains("NewHelper")
                && e.contains("internal_error")),
            "rejection must surface the [internal] probe failure for \
             the helper: {:?}",
            result.errors,
        );
    }

    /// Definition-kind helpers (TeX `\begin{definition}`) must NOT
    /// trigger a probe — closure records are scoped to proof-bearing
    /// nodes only (plan §7.2). This test verifies the short-circuit
    /// by setting a canned response that would have rejected the
    /// burst if the probe HAD run, and asserting that it didn't.
    #[test]
    fn patch_c_r_skips_definition_kind_new_helper() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.MyDef\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- patch_c_r v4\n";
        touch_active_lean(&repo, new_active);
        write(
            &repo.join("Tablet/MyDef.lean"),
            "-- [TABLET NODE: MyDef]\nimport Tablet.Preamble\n\ndef MyDef (n : Nat) : Nat := n + 0\n",
        );
        // Definition-kind .tex (NOT theorem-bearing).
        write(
            &repo.join("Tablet/MyDef.tex"),
            "\\begin{definition}Define $\\mathrm{MyDef}(n)$ to be $n + 0$.\\end{definition}\n",
        );
        // Canned response would reject if it ran (axiom violation).
        set_local_closure_canned_response(
            &repo,
            Some(r#"{"status":"ok","kernel_axioms":["Lean.ofReduceBool"]}"#),
        );
        // Patch C-R-c: A is probed under the broader trigger. Give A a
        // clean per-node response so its probe passes; only MyDef (which
        // is definition-kind and should be skipped before the probe runs)
        // would have hit the rejection arm.
        set_local_closure_canned_response_for_node(
            &repo,
            "A",
            Some(r#"{"status":"ok","kernel_axioms":["propext"]}"#),
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &declaration_hash(&original_active, "A"),
            WorkerProofDeltaMode::Easy,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            false,
            false,
        )
        .unwrap();

        // The burst should be accepted; the helper-probe loop must
        // have short-circuited on the definition kind. `local_closure_
        // results` must NOT contain MyDef (no probe ran).
        assert!(
            result.ok,
            "Patch C-R must skip definition-kind helpers; canned-axiom \
             violation rejection should not fire: {:?}",
            result.errors,
        );
        assert!(
            !result
                .local_closure_results
                .contains_key(&NodeId::from("MyDef")),
            "definition-kind helpers must not produce a probe payload \
             (records are proof-bearing-only per §7.2)",
        );
    }

    /// Patch C-R-c: a node that was ALREADY sorry-free pre-delta and
    /// whose `.lean` proof body is modified must be probed at gate time
    /// — even though it never went through a sorryd→sorry-free
    /// transition. This is the class (c) trigger: pre-C-R-c, only new
    /// births (a) and sorryd→sorry-free transitions (b) were probed,
    /// leaving a loophole where a worker could silently regress an
    /// already-closed node's proof body and have the gate accept the
    /// burst until the next deterministic-revalidation pass caught it.
    ///
    /// Setup: H exists pre-delta, sorry-free with its own clean proof.
    /// Worker modifies H's body in this burst (still sorry-free post-
    /// delta). The helper-probe loop must run on H and stash a payload
    /// keyed by H's NodeId.
    #[test]
    fn patch_c_r_probes_modified_already_sorry_free_helper() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (mut before_snapshot, original_active) = setup_closed_active(&repo);
        // Pre-existing sorry-free helper H — author original body and
        // record its pre-delta hash in before_snapshot so the worker
        // delta machinery sees H as MODIFIED (not added).
        let h_original =
            "-- [TABLET NODE: H]\nimport Tablet.Preamble\n\ntheorem H (n : Nat) : n + 0 = n := Nat.add_zero n\n";
        write(&repo.join("Tablet/H.lean"), h_original);
        write(
            &repo.join("Tablet/H.tex"),
            "\\begin{theorem}For every natural $n$, $n + 0 = n$.\\end{theorem}\n\\begin{proof}Right-zero law.\\end{proof}\n",
        );
        // Snapshot H at its pre-delta state — this becomes part of
        // `before_snapshot` so subsequent modifications surface in
        // `changes.modified`.
        let h_snapshot = snapshot_tablet_dir(&repo);
        for (k, v) in h_snapshot {
            before_snapshot.insert(k, v);
        }
        // Worker modifies H's proof body (still sorry-free, different
        // proof term). Class (c): pre-delta H ∉ current_open_nodes, but
        // H.lean ∈ changes.modified.
        let h_modified =
            "-- [TABLET NODE: H]\nimport Tablet.Preamble\n\ntheorem H (n : Nat) : n + 0 = n := by rw [Nat.add_zero]\n";
        write(&repo.join("Tablet/H.lean"), h_modified);
        // A is also touched (typical burst shape) so the delta has the
        // standard active-edit footprint. Give A a clean per-node
        // response so its probe passes — we want the assertion to
        // isolate H.
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.H\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- patch_c_r_c v1\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response_for_node(
            &repo,
            "A",
            Some(r#"{"status":"ok","kernel_axioms":["propext"]}"#),
        );
        // H's probe returns clean — `propext` is canonical-approved.
        set_local_closure_canned_response_for_node(
            &repo,
            "H",
            Some(r#"{"status":"ok","kernel_axioms":["propext"]}"#),
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            // Pre-delta present_nodes: Preamble + A + H. H is NOT in
            // current_open_nodes (it was sorry-free pre-delta).
            &set(&["Preamble", "A", "H"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            // current_open_nodes is empty — H is NOT pre-delta sorryd.
            // This is the load-bearing precondition for the class (c)
            // case: without C-R-c, H would NOT be in probe_candidates.
            &BTreeSet::new(),
            &declaration_hash(&original_active, "A"),
            // Restructure mode is required to authorize edits to a
            // non-active existing node (H). The `authorized_nodes` set
            // is consulted only in Restructure/CoarseRestructure modes
            // (runtime_cli_observations.rs:296-299).
            WorkerProofDeltaMode::Restructure,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            // Active node A and non-active H both authorized.
            &set(&["A", "H"]),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            false,
            false,
        )
        .unwrap();

        assert!(
            result.ok,
            "Patch C-R-c must accept a clean class-(c) modified-but-still-sorry-free probe: {:?}",
            result.errors,
        );
        assert!(
            result
                .local_closure_results
                .contains_key(&NodeId::from("H")),
            "class (c) trigger must probe modified pre-existing sorry-free \
             nodes; got keys: {:?}",
            result.local_closure_results.keys().collect::<Vec<_>>(),
        );
        let h_probe = result
            .local_closure_results
            .get(&NodeId::from("H"))
            .unwrap();
        assert_eq!(h_probe.status, "ok");
        assert!(
            h_probe.kernel_axioms.contains("propext"),
            "class-(c) probe payload must carry kernel_axioms from canned response",
        );
    }

    /// Patch C-R-c safety: when `must_close_active=false` AND the
    /// worker LEAVES sorry in the active body, the helper-probe loop
    /// MUST NOT probe the active. The worker is exercising the
    /// "active may remain open" prompt contract — probing here would
    /// erroneously reject a legal burst. The `sorry_in_source` filter
    /// at line ~5790 is what protects this case; this test pins it
    /// against regression.
    ///
    /// This is the load-bearing common case: under must_close_active=
    /// false, workers typically iterate on the active body while
    /// leaving sorry placeholders for sub-obligations. The gate must
    /// stay out of their way.
    #[test]
    fn patch_c_r_must_close_active_false_with_sorryd_active_skips_probe() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, _original_active) = setup_closed_active(&repo);
        // Worker modifies A.lean and leaves `sorry` in the body. The
        // current_open_nodes parameter is empty here (A was sorry-free
        // pre-delta — `setup_closed_active` authored a complete proof),
        // but post-delta A's .lean has a `sorry` token, so the
        // `sorry_in_source` check at the probe-candidate filter must
        // fire and skip the probe.
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\n\ntheorem A (n : Nat) : n + 0 = n := by sorry\n";
        touch_active_lean(&repo, new_active);
        // A canned axiom-violation response that would reject IF the
        // probe ran — the test passes iff the probe is skipped.
        set_local_closure_canned_response(
            &repo,
            Some(r#"{"status":"ok","kernel_axioms":["Lean.ofReduceBool"]}"#),
        );
        // Adjust the expected_active_hash to the new (sorryd) body, so
        // the active-hash-stability check at line ~5340 doesn't reject
        // first. The point of this test is the probe gate, not the
        // signature gate.
        let new_active_hash = declaration_hash(new_active, "A");

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &new_active_hash,
            WorkerProofDeltaMode::Easy,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            false, // allow_new_obligations
            false, // must_close_active — the load-bearing flag for this test
        )
        .unwrap();

        // The burst MUST be accepted: probe must not have run on the
        // sorryd active, so the canned Lean.ofReduceBool rejection must
        // NOT fire. local_closure_results must NOT contain A (probe
        // skipped → no payload).
        assert!(
            result.ok,
            "must_close_active=false with sorryd active must NOT trigger \
             the helper-probe loop's axiom check: {:?}",
            result.errors,
        );
        assert!(
            !result
                .local_closure_results
                .contains_key(&NodeId::from("A")),
            "sorryd active must NOT produce a probe payload (sorry_in_source \
             filter must short-circuit)",
        );
    }

    /// Patch C-R-c rejection: class (c) modified-but-still-sorry-free
    /// helper whose probe surfaces an unapproved axiom must reject the
    /// burst with an `[axiom]` error naming the helper.
    #[test]
    fn patch_c_r_rejects_modified_helper_with_unapproved_axiom() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (mut before_snapshot, original_active) = setup_closed_active(&repo);
        let h_original =
            "-- [TABLET NODE: H]\nimport Tablet.Preamble\n\ntheorem H (n : Nat) : n + 0 = n := Nat.add_zero n\n";
        write(&repo.join("Tablet/H.lean"), h_original);
        write(
            &repo.join("Tablet/H.tex"),
            "\\begin{theorem}For every natural $n$, $n + 0 = n$.\\end{theorem}\n\\begin{proof}Right-zero law.\\end{proof}\n",
        );
        let h_snapshot = snapshot_tablet_dir(&repo);
        for (k, v) in h_snapshot {
            before_snapshot.insert(k, v);
        }
        let h_modified =
            "-- [TABLET NODE: H]\nimport Tablet.Preamble\n\ntheorem H (n : Nat) : n + 0 = n := by rw [Nat.add_zero]\n";
        write(&repo.join("Tablet/H.lean"), h_modified);
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.H\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n-- patch_c_r_c v2\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response_for_node(
            &repo,
            "A",
            Some(r#"{"status":"ok","kernel_axioms":["propext"]}"#),
        );
        // H's modified proof transitively uses an unapproved axiom.
        set_local_closure_canned_response_for_node(
            &repo,
            "H",
            Some(r#"{"status":"ok","kernel_axioms":["Lean.ofReduceBool"]}"#),
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A", "H"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &declaration_hash(&original_active, "A"),
            // Restructure mode (same rationale as the happy-path test).
            WorkerProofDeltaMode::Restructure,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            // Active A + non-active H both authorized.
            &set(&["A", "H"]),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            false,
            false,
        )
        .unwrap();

        assert!(
            !result.ok,
            "Patch C-R-c must reject when a class-(c) modified helper \
             carries an unapproved kernel axiom",
        );
        assert!(
            result.errors.iter().any(|e| e.starts_with("[axiom]")
                && e.contains("H")
                && e.contains("Lean.ofReduceBool")),
            "rejection must name the offending class-(c) helper and axiom: {:?}",
            result.errors,
        );
    }

    /// Patch C-R-e regression: worker 2718 contract bug. A burst where
    /// the reviewer asked for a helper-based decomposition, the worker
    /// added a new helper, and the active node's MCA probe references
    /// the new helper as a `boundary_theorems` entry. Pre-C-R-e, the
    /// C-K `validate_probe_present_nodes` check rejected this with
    /// status=internal_error because the new helper wasn't in
    /// `current_present_nodes` (which is the pre-burst snapshot).
    /// Post-C-R-e, the helper-probe path admits same-burst new helpers
    /// via `augment_present_nodes_with_burst_new` so the active probe
    /// accepts boundary references to them.
    #[test]
    fn patch_c_r_admits_same_burst_helper_as_boundary_theorem() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        // Worker authors a new helper NewHelper (sorry-free, proof kind).
        write(
            &repo.join("Tablet/NewHelper.lean"),
            "-- [TABLET NODE: NewHelper]\nimport Tablet.Preamble\n\ntheorem NewHelper : True := trivial\n",
        );
        write(
            &repo.join("Tablet/NewHelper.tex"),
            "\\begin{theorem}A trivial helper.\\end{theorem}\n\\begin{proof}By definition.\\end{proof}\n",
        );
        // Modify A.lean to import the new helper and use it.
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.NewHelper\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n";
        touch_active_lean(&repo, new_active);
        // Active node's probe (MCA) references NewHelper as a boundary
        // theorem. This is the load-bearing setup: NewHelper is NOT in
        // pre-burst current_present_nodes, but the probe emits it. Pre-
        // C-R-e: rejected as "dep names not in kernel present_nodes".
        set_local_closure_canned_response_for_node(
            &repo,
            "A",
            Some(
                r#"{"status":"ok","kernel_axioms":[],"boundary_theorems":[{"name":"Tablet.NewHelper","statement_hash":"deadbeef"}],"strict_theorem_deps":[],"strict_definition_deps":[],"errors":[]}"#,
            ),
        );
        // NewHelper's own probe (class a) — clean.
        set_local_closure_canned_response_for_node(
            &repo,
            "NewHelper",
            Some(r#"{"status":"ok","kernel_axioms":["propext"]}"#),
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            // Pre-burst present_nodes do NOT contain NewHelper.
            &set(&["Preamble", "A"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &declaration_hash(&original_active, "A"),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true, // allow_new_obligations: lets the worker introduce helpers
            true, // must_close_active: MCA fires, which is where the bug surfaced
        )
        .unwrap();

        assert!(
            result.ok,
            "Patch C-R-e: active probe referencing a same-burst helper \
             as boundary_theorems must NOT be rejected by \
             validate_probe_present_nodes; got errors: {:?}",
            result.errors,
        );
        assert!(
            result
                .local_closure_results
                .contains_key(&NodeId::from("A")),
            "MCA accept path must stash A's probe payload, got: {:?}",
            result.local_closure_results.keys().collect::<Vec<_>>(),
        );
        assert!(
            result
                .local_closure_results
                .contains_key(&NodeId::from("NewHelper")),
            "C-R helper-probe path must also stash NewHelper's probe, got: {:?}",
            result.local_closure_results.keys().collect::<Vec<_>>(),
        );
    }

    /// FIX 2 COVERAGE (the residual closed by this change): the universal
    /// owner-file authored-name scan must run for a Definition-kind node at
    /// its own acceptance, even though Definition nodes are excluded from the
    /// proof-only `probe_candidates` closure-probe set. This pins the
    /// COVERAGE — that the kernel actually RUNS `run_local_closure_owner_scan`
    /// on a Definition node and propagates its rejection — distinct from the
    /// `local_closure_smoke.rs` tests that pin the scan LOGIC.
    ///
    /// Setup: a NEW Definition-kind node `D` (a `def`, paired with a
    /// `definition` `.tex`). Its `.lean` is shape-valid (so the new-node
    /// `evaluate_node_observation` pass accepts it), but its per-node canned
    /// `local-closure-axioms` response is an `internal_error` carrying the
    /// reserved-shape diagnostic — simulating the authoritative Lean
    /// scan-only finding a `where`/`let rec`-bound forge the Rust text
    /// backstop cannot see. The burst must be rejected with the authoring
    /// diagnostic.
    ///
    /// Load-bearing: a Definition node is in NEITHER `probe_candidates` (the
    /// proof-only loop filters it out via `is_proof_bearing_statement_environment`)
    /// NOR — absent this change — the universal `owner_scan_nodes` loop. So
    /// the ONLY thing that probes `D` is the new owner-scan loop. With the
    /// `owner_scan_nodes` loop removed, `D`'s canned `internal_error` is
    /// never consulted and the burst passes — i.e. this test fails. Verified
    /// by reverting: deleting the loop makes `result.ok == true`.
    #[test]
    fn owner_scan_rejects_definition_node_forge() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        // A NEW Definition-kind node D: a `def` paired with a `definition`
        // .tex. Shape-valid so `evaluate_node_observation` (which runs
        // before the scan loop) accepts it. The body is a benign def; the
        // forge is simulated via the canned scan response below (the stub
        // serves the canned JSON without parsing the .lean, so this models
        // an authoritative Lean scan that found a `where`-bound forge the
        // Rust text backstop could not see).
        write(
            &repo.join("Tablet/D.lean"),
            "-- [TABLET NODE: D]\nimport Tablet.Preamble\n\ndef D : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/D.tex"),
            "\\begin{definition}A definition node.\\end{definition}\n",
        );
        // Active node A imports D (so D is inside A's support cone — the
        // proof-local mode gate requires new helper nodes to be reachable
        // from the active node's imports) and probes clean so the assertion
        // isolates D's owner-scan rejection.
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.D\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response_for_node(
            &repo,
            "A",
            Some(r#"{"status":"ok","kernel_axioms":["propext"]}"#),
        );
        // D's owner-file SCAN (the `--scan-only` op) returns a
        // reserved-shape rejection. This is the response the `--scan-only`
        // Lean probe emits for a Definition node authoring `def D … where
        // _helper := …` ⇒ `D._helper`. Set on the scan-specific canned
        // file so it targets the owner-scan loop (the full-probe path never
        // runs on D, a Definition node).
        set_local_closure_scan_response_for_node(
            &repo,
            "D",
            Some(
                r#"{"status":"internal_error","kernel_axioms":[],"boundary_theorems":[],"strict_theorem_deps":[],"strict_definition_deps":[],"errors":["node authors reserved-shaped private auxiliary declaration(s) [_helper]: a declaration whose final name component matches a compiler-internal shape (leading `_`, or `eq_`/`match_`/`proof_`/`omega_` + digits) is an unregistered cross-node dependency hidden from the local-closure check. Move the auxiliary into its own registered node, or rename it to a non-reserved shape."]}"#,
            ),
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &declaration_hash(&original_active, "A"),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true, // allow_new_obligations: lets the worker introduce the new node
            true, // must_close_active
        )
        .unwrap();

        assert!(
            !result.ok,
            "the universal owner-file scan must REJECT a Definition-kind node \
             whose authored-name scan flags a reserved-shaped auxiliary; got ok \
             result: {result:?}",
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("owner-file authored-name scan")
                    && e.contains("_helper")
                    && e.contains("D")),
            "rejection must come from the owner-file scan and name node `D` + the \
             offending component `_helper`; errors: {:?}",
            result.errors,
        );
        // The scan is decoupled from the closure-record machinery: a
        // fail-closed scan rejection emits no closure results.
        assert!(
            result.local_closure_results.is_empty(),
            "owner-scan rejection must not stash any closure record; got: {:?}",
            result.local_closure_results.keys().collect::<Vec<_>>(),
        );
    }

    /// FIX 2 COVERAGE companion: a clean Definition-kind node must PASS the
    /// universal owner-scan loop (no false rejection), confirming the new
    /// loop does not break legitimate definitions. With the canned scan
    /// response `ok`, the burst accepts; the definition is NOT stashed as a
    /// closure record (the scan loop is decoupled and never writes records —
    /// records stay proof-node-only via the separate `probe_candidates`
    /// loop, which excludes definitions).
    #[test]
    fn owner_scan_passes_clean_definition_node() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (before_snapshot, original_active) = setup_closed_active(&repo);
        write(
            &repo.join("Tablet/D.lean"),
            "-- [TABLET NODE: D]\nimport Tablet.Preamble\n\ndef D : Nat := 0\n",
        );
        write(
            &repo.join("Tablet/D.tex"),
            "\\begin{definition}A definition node.\\end{definition}\n",
        );
        // A imports D so D is inside A's support cone (proof-local gate).
        let new_active =
            "-- [TABLET NODE: A]\nimport Tablet.Preamble\nimport Tablet.D\n\ntheorem A (n : Nat) : n + 0 = n := Nat.add_zero n\n";
        touch_active_lean(&repo, new_active);
        set_local_closure_canned_response_for_node(
            &repo,
            "A",
            Some(r#"{"status":"ok","kernel_axioms":["propext"]}"#),
        );
        // D's owner-file scan is clean (scan-specific canned file; the stub
        // would also default scan-only to `ok`, but we set it explicitly).
        set_local_closure_scan_response_for_node(
            &repo,
            "D",
            Some(
                r#"{"status":"ok","kernel_axioms":[],"boundary_theorems":[],"strict_theorem_deps":[],"strict_definition_deps":[],"errors":[]}"#,
            ),
        );

        let mut protected_semantic_change_nodes: BTreeSet<NodeId> = BTreeSet::new();
        let result = proof_worker_delta_step_result(
            &repo,
            "A",
            &before_snapshot,
            &set(&["Preamble", "A"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &declaration_hash(&original_active, "A"),
            WorkerProofDeltaMode::Local,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &mut protected_semantic_change_nodes,
            true,
            true,
        )
        .unwrap();

        assert!(
            result.ok,
            "a clean Definition-kind node must pass the universal owner-scan \
             loop; got errors: {:?}",
            result.errors,
        );
        // Decoupled: the definition is NOT stashed as a closure record (the
        // owner-scan loop never writes records; the proof-only probe loop
        // excludes definitions).
        assert!(
            !result
                .local_closure_results
                .contains_key(&NodeId::from("D")),
            "Definition-kind node must NOT get a closure record from the \
             owner-scan loop (records stay proof-node-only); got: {:?}",
            result.local_closure_results.keys().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn axcheck_disagreement_surfaces_as_internal_error_via_structured_errors_field() {
        // Patch C-Q Q9 + fail-loudly halt: the
        // `parse_local_closure_response` path flips status to
        // `checker_disagreement` (was `internal_error` pre-halt-loud)
        // and pushes a structured diagnostic into `errors[0]`. The
        // generic `local.status != "ok"` arm downstream is responsible
        // for surfacing the message to the operator. The dedicated
        // status name lets the supervisor halt path distinguish
        // structural (`checker_disagreement`) from transient
        // (`internal_error`) failures (`feedback_fail_loudly_on_dual_check`).
        let _env = HaltMarkerEnvOverride::pointing_into(tempdir().unwrap().path());
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["Classical.choice"],
            "boundary_theorems": [{"name": "Tablet.Helper", "statement_hash": "h1"}],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "axiomization_check": {
                "kernel_axioms": ["Classical.choice", "sorryAx"],
                "boundary_theorems": ["Helper"],
                "agreed": false,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": ["sorryAx"],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": []
            }
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        // (1) Status flip — the generic `local.status != "ok"` arm in
        // `proof_worker_delta_step_result` reaches this and surfaces
        // the diagnostic.
        assert_eq!(
            parsed.status, CHECKER_DISAGREEMENT_STATUS,
            "axcheck disagreement must flip top-level status",
        );
        // (2) Structured diagnostic in errors[0] — operator sees the
        // full primary/axcheck diff via `local.errors.first()`, which
        // the downstream arm formats into the `[internal] local-closure
        // probe status=internal_error: ...` message.
        assert!(
            !parsed.errors.is_empty(),
            "must have at least one structured error",
        );
        let first = &parsed.errors[0];
        assert!(
            first.contains("axiomization cross-check disagrees") && first.contains("sorryAx"),
            "errors[0] must carry the full diff payload that the \
             downstream arm surfaces, got: {first}",
        );
    }

    // ------ fail-loudly halt on dual-collector disagreement ----------
    //
    // `feedback_fail_loudly_on_dual_check`: a checker-vs-checker
    // disagreement is structural (will reproduce on retry), not
    // transient. The parser flips status to `checker_disagreement` and
    // persists a halt marker so the supervisor refuses to dispatch new
    // bursts. These tests pin the classification + marker behavior.

    /// Test-only RAII guard that points
    /// `TRELLIS_KERNEL_CACHE_ROOT` at a caller-controlled directory and
    /// restores the previous value on drop. Holds the global env-mutation
    /// lock so concurrent tests don't observe each other's tempdirs.
    struct HaltMarkerEnvOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
    }

    impl HaltMarkerEnvOverride {
        fn pointing_into(dir: &Path) -> Self {
            let guard = crate::kernel_cache_env_test_guard();
            let prev = std::env::var_os(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
            // SAFETY: env mutation is serialized by the lock guard.
            unsafe {
                std::env::set_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, dir);
            }
            // Pre-clear any leftover marker so subsequent emit attempts
            // are not no-op'd by the "already exists" guard.
            let marker = dir.join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME);
            let _ = std::fs::remove_file(&marker);
            Self {
                _guard: guard,
                prev,
            }
        }
    }

    impl Drop for HaltMarkerEnvOverride {
        fn drop(&mut self) {
            // SAFETY: env mutation is serialized by the lock guard we hold.
            match self.prev.take() {
                Some(v) => unsafe {
                    std::env::set_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, v);
                },
                None => unsafe {
                    std::env::remove_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
                },
            }
        }
    }

    #[test]
    fn checker_disagreement_writes_halt_marker_with_required_fields() {
        let tmp = tempdir().unwrap();
        let runtime_root = tmp.path();
        let _env = HaltMarkerEnvOverride::pointing_into(runtime_root);
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["Classical.choice"],
            "boundary_theorems": [{"name": "Tablet.Foo", "statement_hash": "h1"}],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "axiomization_check": {
                "kernel_axioms": ["Classical.choice", "sorryAx"],
                "boundary_theorems": ["Foo"],
                "agreed": false,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": ["sorryAx"],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": []
            }
        });
        let parsed = parse_local_closure_response("Foo", envelope).expect("parse envelope");
        assert_eq!(parsed.status, CHECKER_DISAGREEMENT_STATUS);

        let marker = runtime_root.join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME);
        assert!(
            marker.exists(),
            "halt marker must be persisted at runtime-root"
        );
        let raw = std::fs::read_to_string(&marker).expect("read halt marker");
        let parsed_marker: serde_json::Value =
            serde_json::from_str(&raw).expect("halt marker must be valid JSON");
        assert_eq!(parsed_marker["kind"], "checker_disagreement");
        assert_eq!(parsed_marker["active_node"], "Foo");
        assert!(parsed_marker["unix_ts"].as_u64().is_some());
        assert_eq!(parsed_marker["axcheck_only_axioms"][0], "sorryAx");
        assert_eq!(parsed_marker["probe_status"], CHECKER_DISAGREEMENT_STATUS);
        assert!(
            parsed_marker["clear_instructions"]
                .as_str()
                .map(|s| s.contains("DELETE this file")
                    && s.contains("checker_disagreement_halt.json"))
                .unwrap_or(false),
            "clear_instructions must be self-documenting, got: {:?}",
            parsed_marker["clear_instructions"],
        );
        assert!(checker_disagreement_halt_marker_present());
    }

    #[test]
    fn checker_disagreement_with_empty_diffs_still_classified_and_halted() {
        // Defensive: agreed=false with empty diff arrays shouldn't
        // happen (the Lean script populates diffs alongside the flip)
        // but if it ever did, we still treat it as a real disagreement
        // and halt — better safe than silent.
        let tmp = tempdir().unwrap();
        let _env = HaltMarkerEnvOverride::pointing_into(tmp.path());
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "axiomization_check": {
                "kernel_axioms": [],
                "boundary_theorems": [],
                "agreed": false,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": [],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": []
            }
        });
        let parsed = parse_local_closure_response("EmptyDiff", envelope).expect("parse");
        assert_eq!(parsed.status, CHECKER_DISAGREEMENT_STATUS);
        let marker = tmp.path().join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME);
        assert!(marker.exists(), "empty-diff disagreement still halts");
    }

    #[test]
    fn axcheck_collector_crash_preserves_internal_error_and_no_halt_marker() {
        // Collector crashes are transient (a new Lean toolchain glitch,
        // a one-off OOM) — we must preserve the pre-existing
        // `internal_error` classification so the worker-retry path
        // applies, and we MUST NOT write a halt marker.
        let tmp = tempdir().unwrap();
        let _env = HaltMarkerEnvOverride::pointing_into(tmp.path());
        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": ["Classical.choice"],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": ["axiomization_check_crash: oom in collector"],
            "axiomization_check": {
                "kernel_axioms": [],
                "boundary_theorems": [],
                "agreed": false,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": [],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": [],
                "error": "oom in collector"
            }
        });
        let parsed = parse_local_closure_response("CrashNode", envelope).expect("parse");
        assert_eq!(
            parsed.status, "internal_error",
            "collector crash MUST stay internal_error so retry path applies",
        );
        assert_ne!(parsed.status, CHECKER_DISAGREEMENT_STATUS);
        let marker = tmp.path().join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME);
        assert!(
            !marker.exists(),
            "collector crash MUST NOT trigger a halt marker (it's transient)",
        );
        assert!(!checker_disagreement_halt_marker_present());
    }

    #[test]
    fn parse_internal_error_unrelated_to_axcheck_does_not_trigger_halt_marker() {
        // A regular probe-side internal_error (parse failure, missing
        // status field, etc.) must NOT be conflated with a disagreement.
        let tmp = tempdir().unwrap();
        let _env = HaltMarkerEnvOverride::pointing_into(tmp.path());
        let envelope = serde_json::json!({
            "status": "internal_error",
            "kernel_axioms": [],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": ["transport-level failure"]
        });
        let parsed = parse_local_closure_response("X", envelope).expect("parse");
        assert_eq!(parsed.status, "internal_error");
        let marker = tmp.path().join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME);
        assert!(
            !marker.exists(),
            "vanilla internal_error must not write halt marker",
        );
    }

    #[test]
    fn existing_halt_marker_is_preserved_not_overwritten() {
        // First disagreement is load-bearing; subsequent disagreements
        // (e.g., from retried probes before the operator notices) must
        // not clobber the original diagnostic.
        let tmp = tempdir().unwrap();
        let _env = HaltMarkerEnvOverride::pointing_into(tmp.path());
        let marker = tmp.path().join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME);
        std::fs::write(
            &marker,
            "{\"kind\":\"checker_disagreement\",\"active_node\":\"FirstNode\"}",
        )
        .unwrap();

        let envelope = serde_json::json!({
            "status": "ok",
            "kernel_axioms": [],
            "boundary_theorems": [],
            "strict_theorem_deps": [],
            "strict_definition_deps": [],
            "errors": [],
            "axiomization_check": {
                "kernel_axioms": [],
                "boundary_theorems": [],
                "agreed": false,
                "skipped": false,
                "primary_only_axioms": [],
                "axcheck_only_axioms": ["second"],
                "primary_only_boundaries": [],
                "axcheck_only_boundaries": []
            }
        });
        let _ = parse_local_closure_response("SecondNode", envelope).expect("parse");
        let body = std::fs::read_to_string(&marker).unwrap();
        assert!(
            body.contains("FirstNode"),
            "original marker MUST be preserved across re-disagreement, got: {body}",
        );
        assert!(!body.contains("SecondNode"));
    }

    // ------ fail-loudly halt on system_feedback emission -------------
    //
    // Per fail-loudly policy: every system_feedback emission pauses the
    // run. Distinct marker file from checker-disagreement so the
    // operator can tell the two halt causes apart at a glance. These
    // tests pin the marker shape + preservation behavior and the
    // either-marker-blocks-loop semantics.

    /// Test-only RAII guard for the system_feedback halt marker path.
    /// Same env-mutation lock as `HaltMarkerEnvOverride` so concurrent
    /// tests don't race on `TRELLIS_KERNEL_CACHE_ROOT`. Pre-clears any
    /// leftover marker AND `TRELLIS_SYSTEM_FEEDBACK_HALT` (so writer
    /// tests see the documented default unless they opt in via
    /// `set_halt_env`); both are restored on drop.
    struct SystemFeedbackHaltMarkerEnvOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
        prev_halt_env: Option<std::ffi::OsString>,
    }

    impl SystemFeedbackHaltMarkerEnvOverride {
        fn pointing_into(dir: &Path) -> Self {
            let guard = crate::kernel_cache_env_test_guard();
            let prev = std::env::var_os(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
            let prev_halt_env = std::env::var_os(SYSTEM_FEEDBACK_HALT_ENV);
            // SAFETY: env mutation is serialized by the lock guard.
            unsafe {
                std::env::set_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, dir);
                std::env::remove_var(SYSTEM_FEEDBACK_HALT_ENV);
            }
            let marker = dir.join(SYSTEM_FEEDBACK_HALT_MARKER_FILENAME);
            let _ = std::fs::remove_file(&marker);
            let checker = dir.join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME);
            let _ = std::fs::remove_file(&checker);
            Self {
                _guard: guard,
                prev,
                prev_halt_env,
            }
        }

        /// Set `TRELLIS_SYSTEM_FEEDBACK_HALT` for the guard's lifetime
        /// (restored on drop). Safe because the guard's mutex serializes
        /// every test that touches this env var.
        fn set_halt_env(&self, value: &str) {
            // SAFETY: env mutation is serialized by the lock guard we hold.
            unsafe {
                std::env::set_var(SYSTEM_FEEDBACK_HALT_ENV, value);
            }
        }
    }

    impl Drop for SystemFeedbackHaltMarkerEnvOverride {
        fn drop(&mut self) {
            // SAFETY: env mutation is serialized by the lock guard we hold.
            match self.prev.take() {
                Some(v) => unsafe {
                    std::env::set_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, v);
                },
                None => unsafe {
                    std::env::remove_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
                },
            }
            match self.prev_halt_env.take() {
                Some(v) => unsafe {
                    std::env::set_var(SYSTEM_FEEDBACK_HALT_ENV, v);
                },
                None => unsafe {
                    std::env::remove_var(SYSTEM_FEEDBACK_HALT_ENV);
                },
            }
        }
    }

    /// Write a minimal `trellis.config.json` with the given top-level
    /// `system_feedback_halt` JSON fragment (raw text, e.g. `true`) and
    /// return its path.
    fn write_halt_knob_config(dir: &Path, knob_json: &str) -> PathBuf {
        let path = dir.join("trellis.config.json");
        std::fs::write(
            &path,
            format!("{{\"system_feedback_halt\": {knob_json}}}"),
        )
        .unwrap();
        path
    }

    /// Read + parse the single-row (or last-row) unified system-feedback
    /// log under `dir`.
    fn read_system_feedback_log_rows(dir: &Path) -> Vec<serde_json::Value> {
        let raw = std::fs::read_to_string(dir.join(SYSTEM_FEEDBACK_LOG_FILENAME)).unwrap();
        raw.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn system_feedback_emission_writes_halt_marker_with_required_fields() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        let config = write_halt_knob_config(tmp.path(), "true");
        write_system_feedback_halt_marker(
            Some(&config),
            "Foo",
            "CoarseFoo",
            42,
            123,
            "worker",
            "worker",
            "worker:0:Foo",
            "artifact.json",
            "tool X mis-parsed argument Y",
            "agent burst returned non-empty system_feedback string",
        );
        let marker = tmp.path().join(SYSTEM_FEEDBACK_HALT_MARKER_FILENAME);
        assert!(
            marker.exists(),
            "halt marker must be persisted at runtime-root"
        );
        let raw = std::fs::read_to_string(&marker).expect("read halt marker");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("halt marker must be valid JSON");
        assert_eq!(parsed["kind"], "system_feedback");
        assert_eq!(parsed["active_node"], "Foo");
        assert_eq!(parsed["active_coarse_node"], "CoarseFoo");
        assert_eq!(parsed["cycle"], 42);
        assert_eq!(parsed["request_id"], 123);
        assert_eq!(parsed["request_kind"], "worker");
        assert_eq!(parsed["burst_role"], "worker");
        assert_eq!(parsed["lane"], "worker:0:Foo");
        assert_eq!(parsed["artifact"], "artifact.json");
        assert_eq!(parsed["system_feedback"], "tool X mis-parsed argument Y");
        assert!(parsed["unix_ts"].as_u64().is_some());
        assert!(
            parsed["clear_instructions"]
                .as_str()
                .map(|s| s.contains("DELETE this file") && s.contains("system_feedback_halt.json"))
                .unwrap_or(false),
            "clear_instructions must be self-documenting, got: {:?}",
            parsed["clear_instructions"],
        );
        assert!(system_feedback_halt_marker_present());
        // Halt mode ALSO appends the unified log row (one durable stream
        // regardless of mode).
        let rows = read_system_feedback_log_rows(tmp.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "system_feedback");
        assert_eq!(rows[0]["acked"], false);
        assert_eq!(rows[0]["halted"], true);
        assert_eq!(rows[0]["system_feedback"], "tool X mis-parsed argument Y");
    }

    #[test]
    fn system_feedback_existing_marker_is_preserved_not_overwritten() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        let config = write_halt_knob_config(tmp.path(), "true");
        let marker = tmp.path().join(SYSTEM_FEEDBACK_HALT_MARKER_FILENAME);
        std::fs::write(
            &marker,
            "{\"kind\":\"system_feedback\",\"active_node\":\"FirstNode\"}",
        )
        .unwrap();

        write_system_feedback_halt_marker(
            Some(&config),
            "SecondNode",
            "",
            1,
            2,
            "reviewer",
            "reviewer",
            "reviewer:0",
            "review.json",
            "second feedback",
            "second emission",
        );
        let body = std::fs::read_to_string(&marker).unwrap();
        assert!(
            body.contains("FirstNode"),
            "original marker MUST be preserved across re-emission, got: {body}",
        );
        assert!(!body.contains("SecondNode"));
        assert!(!body.contains("second feedback"));
    }

    #[test]
    fn any_halt_marker_present_reports_either_marker() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        assert!(!any_halt_marker_present());

        let checker = tmp.path().join(CHECKER_DISAGREEMENT_HALT_MARKER_FILENAME);
        std::fs::write(&checker, "{}").unwrap();
        assert!(any_halt_marker_present());
        assert!(checker_disagreement_halt_marker_present());
        assert!(!system_feedback_halt_marker_present());
        std::fs::remove_file(&checker).unwrap();
        assert!(!any_halt_marker_present());

        let feedback = tmp.path().join(SYSTEM_FEEDBACK_HALT_MARKER_FILENAME);
        std::fs::write(&feedback, "{}").unwrap();
        assert!(any_halt_marker_present());
        assert!(system_feedback_halt_marker_present());
        assert!(!checker_disagreement_halt_marker_present());
    }

    #[test]
    fn system_feedback_halt_marker_path_unset_env_returns_none() {
        // Mirror the checker-disagreement helper's degraded-mode
        // behavior: with `TRELLIS_KERNEL_CACHE_ROOT` unset, the marker
        // path resolves to None and `_present` returns false (rather
        // than crashing or falsely halting).
        let guard = crate::kernel_cache_env_test_guard();
        let prev = std::env::var_os(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
        // SAFETY: env mutation is serialized by the lock guard.
        unsafe {
            std::env::remove_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
        }
        assert!(system_feedback_halt_marker_path().is_none());
        assert!(!system_feedback_halt_marker_present());
        assert!(!any_halt_marker_present());
        // Restore.
        // SAFETY: env mutation is serialized by the lock guard we hold.
        if let Some(v) = prev {
            unsafe {
                std::env::set_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, v);
            }
        }
        drop(guard);
    }

    // ------ conservative fingerprint-ack policy for system_feedback -----
    //
    // The default (no ack store) MUST be a pure superset of the historical
    // fail-loudly behavior: every emission still halts, and the marker now
    // additionally carries the `fingerprint` an operator can ack. Only a
    // fingerprint an operator explicitly recorded is suppressed, and its
    // suppressed recurrences are still logged to the design-gap log.

    /// Default path: with an EMPTY ack store the emission still writes the
    /// halt marker (behavior unchanged) AND the marker carries the stable
    /// `fingerprint` field so the operator knows what to ack.
    #[test]
    fn system_feedback_unacked_writes_marker_with_fingerprint() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        let config = write_halt_knob_config(tmp.path(), "true");
        write_system_feedback_halt_marker(
            Some(&config),
            "NodeA",
            "CoarseA",
            809,
            4242,
            "reviewer",
            "reviewer",
            "reviewer:0:NodeA",
            "review.json",
            "reviewer flags a known gap in `NodeA` after 3 targets",
            "agent burst returned non-empty system_feedback string",
        );
        let marker = tmp.path().join(SYSTEM_FEEDBACK_HALT_MARKER_FILENAME);
        assert!(marker.exists(), "unacked feedback MUST still halt (write marker)");
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&marker).unwrap()).unwrap();
        let expected_fp = system_feedback_fingerprint(
            "reviewer",
            "reviewer",
            "reviewer flags a known gap in `NodeA` after 3 targets",
        );
        assert_eq!(
            parsed["fingerprint"].as_str(),
            Some(expected_fp.as_str()),
            "halt marker MUST carry the computed fingerprint for the operator to ack",
        );
        // The unified log records the unacked halting emission too.
        let rows = read_system_feedback_log_rows(tmp.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["acked"], false);
        assert_eq!(rows[0]["halted"], true);
        assert_eq!(rows[0]["fingerprint"].as_str(), Some(expected_fp.as_str()));
    }

    /// Acked path (halt mode): a fingerprint present in the store
    /// suppresses the halt marker, appends the full feedback to the
    /// unified system-feedback log (row only), and the function returns
    /// without freezing the run.
    #[test]
    fn system_feedback_acked_suppresses_marker_and_logs_gap() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        let config = write_halt_knob_config(tmp.path(), "true");

        let feedback = "reviewer flags a known gap in `NodeA` after 3 targets";
        let fingerprint = system_feedback_fingerprint("reviewer", "reviewer", feedback);
        let result = acknowledge_system_feedback_fingerprint(
            &fingerprint,
            "known design gap tracked in ISSUE-123",
            feedback,
        )
        .expect("ack must succeed");
        assert!(result.newly_added);
        assert_eq!(result.total_acknowledged, 1);

        write_system_feedback_halt_marker(
            Some(&config),
            "NodeA",
            "CoarseA",
            810,
            4243,
            "reviewer",
            "reviewer",
            "reviewer:0:NodeA",
            "review.json",
            feedback,
            "agent burst returned non-empty system_feedback string",
        );

        let marker = tmp.path().join(SYSTEM_FEEDBACK_HALT_MARKER_FILENAME);
        assert!(!marker.exists(), "acked feedback MUST NOT write a halt marker");
        assert!(!system_feedback_halt_marker_present());

        let rows = read_system_feedback_log_rows(tmp.path());
        assert_eq!(rows.len(), 1, "acked feedback MUST be recorded in the unified log");
        let rec = &rows[0];
        assert_eq!(rec["kind"], "system_feedback");
        assert_eq!(rec["acked"], true);
        assert_eq!(rec["halted"], false);
        assert_eq!(rec["fingerprint"].as_str(), Some(fingerprint.as_str()));
        assert_eq!(rec["cycle"], 810);
        assert_eq!(rec["request_id"], 4243);
        assert_eq!(rec["system_feedback"].as_str(), Some(feedback));
    }

    /// DEFAULT path (knob absent everywhere): NO halt marker; the
    /// emission is appended to the unified log with the full field set
    /// and the writer returns without freezing the run. Pins the shipped
    /// public-release behavior.
    #[test]
    fn system_feedback_default_logs_and_continues_without_marker() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        write_system_feedback_halt_marker(
            None,
            "NodeA",
            "CoarseA",
            42,
            123,
            "worker",
            "worker",
            "worker:0:NodeA",
            "artifact.json",
            "tool X mis-parsed argument Y",
            "agent burst returned non-empty system_feedback string",
        );
        assert!(
            !tmp.path().join(SYSTEM_FEEDBACK_HALT_MARKER_FILENAME).exists(),
            "default (knob absent) MUST NOT write a halt marker"
        );
        assert!(!system_feedback_halt_marker_present());
        let rows = read_system_feedback_log_rows(tmp.path());
        assert_eq!(rows.len(), 1);
        let rec = &rows[0];
        assert_eq!(rec["kind"], "system_feedback");
        assert_eq!(rec["active_node"], "NodeA");
        assert_eq!(rec["active_coarse_node"], "CoarseA");
        assert_eq!(rec["cycle"], 42);
        assert_eq!(rec["request_id"], 123);
        assert_eq!(rec["request_kind"], "worker");
        assert_eq!(rec["burst_role"], "worker");
        assert_eq!(rec["lane"], "worker:0:NodeA");
        assert_eq!(rec["artifact"], "artifact.json");
        assert_eq!(rec["system_feedback"], "tool X mis-parsed argument Y");
        assert_eq!(rec["acked"], false);
        assert_eq!(rec["halted"], false);
        assert!(rec["unix_ts"].as_u64().is_some());
        let expected_fp =
            system_feedback_fingerprint("worker", "worker", "tool X mis-parsed argument Y");
        assert_eq!(rec["fingerprint"].as_str(), Some(expected_fp.as_str()));
        // A config that simply omits the knob behaves the same.
        let config = tmp.path().join("trellis.config.json");
        std::fs::write(&config, "{}").unwrap();
        write_system_feedback_halt_marker(
            Some(&config),
            "NodeA", "CoarseA", 43, 124, "worker", "worker", "worker:0:NodeA",
            "artifact.json", "tool X mis-parsed argument Y", "reason",
        );
        assert!(!system_feedback_halt_marker_present());
        assert_eq!(read_system_feedback_log_rows(tmp.path()).len(), 2);
    }

    /// Explicit `system_feedback_halt: false` is the same as absent.
    #[test]
    fn system_feedback_config_false_logs_and_continues() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        let config = write_halt_knob_config(tmp.path(), "false");
        write_system_feedback_halt_marker(
            Some(&config),
            "NodeA", "CoarseA", 1, 2, "reviewer", "reviewer", "reviewer:0",
            "review.json", "some feedback text", "reason",
        );
        assert!(!system_feedback_halt_marker_present());
        let rows = read_system_feedback_log_rows(tmp.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["halted"], false);
    }

    /// Env `TRELLIS_SYSTEM_FEEDBACK_HALT` beats the config knob in BOTH
    /// directions (launcher-friendly override).
    #[test]
    fn system_feedback_env_override_beats_config_both_directions() {
        let tmp = tempdir().unwrap();
        let env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());

        // env=1 + config false ⇒ HALT.
        let config = write_halt_knob_config(tmp.path(), "false");
        env.set_halt_env("1");
        write_system_feedback_halt_marker(
            Some(&config),
            "NodeA", "CoarseA", 1, 2, "reviewer", "reviewer", "reviewer:0",
            "review.json", "env forces halting on", "reason",
        );
        assert!(
            system_feedback_halt_marker_present(),
            "env=1 must override config false"
        );
        std::fs::remove_file(tmp.path().join(SYSTEM_FEEDBACK_HALT_MARKER_FILENAME)).unwrap();

        // env=0 + config true ⇒ NO halt, log row only.
        let config = write_halt_knob_config(tmp.path(), "true");
        env.set_halt_env("0");
        write_system_feedback_halt_marker(
            Some(&config),
            "NodeA", "CoarseA", 3, 4, "reviewer", "reviewer", "reviewer:0",
            "review.json", "env forces halting off", "reason",
        );
        assert!(
            !system_feedback_halt_marker_present(),
            "env=0 must override config true"
        );
        let rows = read_system_feedback_log_rows(tmp.path());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["halted"], true);
        assert_eq!(rows[1]["halted"], false);
    }

    /// Malformed knob (unparseable config JSON, or a non-bool value)
    /// resolves LOUDLY toward halting — unreadable operator intent
    /// falls back to the fail-loudly behavior, never silently disables
    /// it. Documented lenient behavior per the sidecar-config precedent.
    #[test]
    fn system_feedback_malformed_config_fails_loud_toward_halt() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());

        // Non-bool knob value.
        let config = write_halt_knob_config(tmp.path(), "\"yes\"");
        assert!(system_feedback_halt_enabled(Some(&config)));

        // Unparseable config file.
        std::fs::write(&config, "{ this is not valid json").unwrap();
        assert!(system_feedback_halt_enabled(Some(&config)));

        // Malformed env value (with a well-formed config false).
        let config = write_halt_knob_config(tmp.path(), "false");
        let env2 = &_env;
        env2.set_halt_env("maybe");
        assert!(system_feedback_halt_enabled(Some(&config)));
    }

    /// Resolution table for `system_feedback_halt_enabled` with no env
    /// override in play: default FALSE for None / missing file / absent
    /// key; the bool value verbatim when present.
    #[test]
    fn system_feedback_halt_enabled_default_resolution() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        assert!(!system_feedback_halt_enabled(None));
        let missing = tmp.path().join("does_not_exist.config.json");
        assert!(!system_feedback_halt_enabled(Some(&missing)));
        let config = tmp.path().join("trellis.config.json");
        std::fs::write(&config, "{}").unwrap();
        assert!(!system_feedback_halt_enabled(Some(&config)));
        let config = write_halt_knob_config(tmp.path(), "true");
        assert!(system_feedback_halt_enabled(Some(&config)));
        let config = write_halt_knob_config(tmp.path(), "false");
        assert!(!system_feedback_halt_enabled(Some(&config)));
        // env "true"/"false" spellings.
        let guard = &_env;
        guard.set_halt_env("true");
        assert!(system_feedback_halt_enabled(None));
        guard.set_halt_env("false");
        let config = write_halt_knob_config(tmp.path(), "true");
        assert!(!system_feedback_halt_enabled(Some(&config)));
    }

    /// Fingerprint stability — the crux. Two emissions of the SAME gap that
    /// differ only in embedded node-identifiers (backtick spans), cycle
    /// numbers, and counts (digit runs) MUST collapse to ONE fingerprint
    /// (so a single ack suppresses every recurrence). Two GENUINELY
    /// different gaps MUST produce different fingerprints (novel still
    /// halts).
    #[test]
    fn system_feedback_fingerprint_is_stable_across_cycle_variation() {
        // Same gap-class, cycle 809 vs 934, node NodeA vs NodeB, 3 vs 17.
        let a = "reviewer flags a known gap in `NodeA` after 3 targets at cycle 809";
        let b = "reviewer flags a known gap in `NodeB` after 17 targets at cycle 934";
        let fp_a = system_feedback_fingerprint("reviewer", "reviewer", a);
        let fp_b = system_feedback_fingerprint("reviewer", "reviewer", b);
        assert_eq!(
            fp_a, fp_b,
            "same gap-class differing only in node-ids/counts/cycles MUST share a fingerprint\n a={a}\n b={b}",
        );

        // A genuinely different gap text → different fingerprint.
        let c = "worker cannot dispatch because the checker socket is unreachable";
        let fp_c = system_feedback_fingerprint("reviewer", "reviewer", c);
        assert_ne!(fp_a, fp_c, "different gap-class MUST produce a different fingerprint");

        // request_kind / burst_role are part of the key: same text, different
        // origin → different fingerprint (distinct ack classes).
        let fp_a_worker = system_feedback_fingerprint("worker", "worker", a);
        assert_ne!(
            fp_a, fp_a_worker,
            "same text from a different request_kind/burst_role is a distinct ack class",
        );
    }

    /// Empty (default) store never suppresses — a pure superset guarantee at
    /// the store level.
    #[test]
    fn system_feedback_empty_store_acknowledges_nothing() {
        let store = SystemFeedbackAckStore::default();
        assert!(store.acknowledged.is_empty());
        assert!(!store.is_acknowledged(&system_feedback_fingerprint("reviewer", "reviewer", "any")));
    }

    /// Missing / unset runtime root ⇒ empty store (fail-closed): the ack
    /// machinery can never accidentally widen suppression when it cannot
    /// resolve its own storage.
    #[test]
    fn system_feedback_ack_store_fail_closed_without_root() {
        let guard = crate::kernel_cache_env_test_guard();
        let prev = std::env::var_os(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
        // SAFETY: env mutation serialized by the lock guard.
        unsafe {
            std::env::remove_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV);
        }
        assert!(system_feedback_ack_store_path().is_none());
        assert!(load_system_feedback_ack_store().acknowledged.is_empty());
        if let Some(v) = prev {
            // SAFETY: env mutation serialized by the lock guard we hold.
            unsafe {
                std::env::set_var(trellis_kernel::disk_cache::KERNEL_CACHE_ROOT_ENV, v);
            }
        }
        drop(guard);
    }

    /// A corrupt ack-store file is treated as EMPTY (fail-closed) — a
    /// tampered / truncated store can never suppress a halt.
    #[test]
    fn system_feedback_corrupt_store_fails_closed() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        std::fs::write(
            tmp.path().join(SYSTEM_FEEDBACK_ACK_STORE_FILENAME),
            "{ this is not valid json",
        )
        .unwrap();
        assert!(load_system_feedback_ack_store().acknowledged.is_empty());
    }

    /// Acking a fingerprint that raised the current halt also CLEARS that
    /// halt (preserve-as-`.acked`) so the operator's ack resumes the run,
    /// mirroring the checker-disagreement ack's marker preservation.
    #[test]
    fn system_feedback_ack_clears_matching_active_halt() {
        let tmp = tempdir().unwrap();
        let _env = SystemFeedbackHaltMarkerEnvOverride::pointing_into(tmp.path());
        let feedback = "reviewer flags a known gap in `NodeA` after 3 targets";
        let config = write_halt_knob_config(tmp.path(), "true");
        // Raise the halt (unacked).
        write_system_feedback_halt_marker(
            Some(&config),
            "NodeA", "CoarseA", 809, 4242, "reviewer", "reviewer", "reviewer:0:NodeA",
            "review.json", feedback, "reason",
        );
        assert!(system_feedback_halt_marker_present());
        let fingerprint = system_feedback_fingerprint("reviewer", "reviewer", feedback);
        let result =
            acknowledge_system_feedback_fingerprint(&fingerprint, "ack it", feedback).unwrap();
        assert!(result.cleared_active_halt, "matching ack must clear the active halt");
        assert!(!system_feedback_halt_marker_present());
        // The diagnostic survives as `.acked-*.json`.
        let acked = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("system_feedback_halt.acked-")
            });
        assert!(acked, "acked marker must be preserved on disk");
    }

    // ------ observe_placeholder_definition_nodes ---------------------------
    //
    // Detection unit tests for the `True`-placeholder correspondence
    // auto-fail (definition-analogue of SKETCH). FILESPEC v2 files carry a
    // `-- BODY` marker; the body region is everything after it.

    /// Write a FILESPEC-v2 definition node whose body region is `body`.
    fn write_def_node(repo: &Path, name: &str, signature: &str, body: &str) {
        let lean = format!(
            "-- [TABLET NODE: {name}]\nimport Tablet.Preamble\n\n{signature} :=\n-- BODY\n{body}\n"
        );
        write(&repo.join("Tablet").join(format!("{name}.lean")), &lean);
    }

    #[test]
    fn placeholder_def_detects_bare_true_body() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_def_node(repo, "Foo", "def Foo (n : Nat) : Prop", "True");
        let present: BTreeSet<NodeId> = set(&["Foo"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Foo"), NodeKind::Definition)]);
        let detected = observe_placeholder_definition_nodes(repo, &present, &kinds);
        assert_eq!(detected, set::<NodeId>(&["Foo"]));
    }

    #[test]
    fn placeholder_def_detects_true_with_surrounding_whitespace_and_trailing_comment() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // Body region has leading/trailing whitespace and a trailing `--`
        // note; the stripped body is exactly `True`.
        write_def_node(
            repo,
            "Bar",
            "def Bar : Prop",
            "  True  -- TODO: real definition deferred",
        );
        let present: BTreeSet<NodeId> = set(&["Bar"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Bar"), NodeKind::Definition)]);
        assert_eq!(
            observe_placeholder_definition_nodes(repo, &present, &kinds),
            set::<NodeId>(&["Bar"])
        );
    }

    #[test]
    fn placeholder_def_rejects_real_definition_body() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_def_node(
            repo,
            "Card",
            "def Card (s : Finset Nat) : Nat",
            "Finset.card s",
        );
        let present: BTreeSet<NodeId> = set(&["Card"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Card"), NodeKind::Definition)]);
        assert!(observe_placeholder_definition_nodes(repo, &present, &kinds).is_empty());
    }

    #[test]
    fn placeholder_def_never_matches_non_definition_kind() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // Even a contrived theorem whose body region is the bare `True`
        // token is never a placeholder definition — only Definition kind
        // is gated.
        write_def_node(repo, "Thm", "theorem Thm : True", "True");
        let present: BTreeSet<NodeId> = set(&["Thm"]);
        let proof_kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Thm"), NodeKind::Proof)]);
        assert!(
            observe_placeholder_definition_nodes(repo, &present, &proof_kinds).is_empty(),
            "Proof-kind node must never be a placeholder definition"
        );
        // The same file IS detected when the kind map says Definition,
        // proving the kind gate (not the body) is what excludes it above.
        let def_kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Thm"), NodeKind::Definition)]);
        assert_eq!(
            observe_placeholder_definition_nodes(repo, &present, &def_kinds),
            set::<NodeId>(&["Thm"])
        );
    }

    #[test]
    fn placeholder_def_conservative_scope_rejects_near_true_bodies() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        for (name, body) in [
            ("FunTrue", "fun _ => True"),
            ("AndTrue", "True ∧ True"),
            ("ByTrivial", "by trivial"),
        ] {
            write_def_node(repo, name, &format!("def {name} : Prop"), body);
        }
        let present: BTreeSet<NodeId> = set(&["FunTrue", "AndTrue", "ByTrivial"]);
        let kinds: BTreeMap<NodeId, NodeKind> = present
            .iter()
            .map(|n| (n.clone(), NodeKind::Definition))
            .collect();
        assert!(
            observe_placeholder_definition_nodes(repo, &present, &kinds).is_empty(),
            "near-`True` bodies must not be treated as the bare-`True` placeholder"
        );
    }

    #[test]
    fn placeholder_def_fails_closed_on_missing_body_marker() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // No `-- BODY` marker: split() errors, detection excludes the node.
        write(
            &repo.join("Tablet").join("NoMarker.lean"),
            "-- [TABLET NODE: NoMarker]\nimport Tablet.Preamble\n\ndef NoMarker : Prop := True\n",
        );
        let present: BTreeSet<NodeId> = set(&["NoMarker"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("NoMarker"), NodeKind::Definition)]);
        assert!(observe_placeholder_definition_nodes(repo, &present, &kinds).is_empty());
    }

    /// Write a FILESPEC-v2 where-form data-declaration node: `head` is the
    /// declaration line (ending in `where`), `body` is the fields /
    /// constructors region below the marker.
    fn write_where_node(repo: &Path, name: &str, head: &str, body: &str) {
        let lean = format!(
            "-- [TABLET NODE: {name}]\nimport Tablet.Preamble\n\n{head}\n-- BODY\n{body}\n"
        );
        write(&repo.join("Tablet").join(format!("{name}.lean")), &lean);
    }

    #[test]
    fn placeholder_def_detects_empty_where_body_structure() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // No fields below the marker -> `structure Foo where` ≅ Unit, the
        // structural placeholder.
        write_where_node(repo, "Foo", "structure Foo where", "");
        let present: BTreeSet<NodeId> = set(&["Foo"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Foo"), NodeKind::Definition)]);
        assert_eq!(
            observe_placeholder_definition_nodes(repo, &present, &kinds),
            set::<NodeId>(&["Foo"])
        );
    }

    #[test]
    fn placeholder_def_detects_empty_where_body_inductive() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_where_node(repo, "Foo", "inductive Foo : Type where", "  -- deferred");
        let present: BTreeSet<NodeId> = set(&["Foo"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Foo"), NodeKind::Definition)]);
        assert_eq!(
            observe_placeholder_definition_nodes(repo, &present, &kinds),
            set::<NodeId>(&["Foo"])
        );
    }

    #[test]
    fn placeholder_def_rejects_populated_where_body() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_where_node(repo, "Foo", "structure Foo where", "  x : Nat");
        let present: BTreeSet<NodeId> = set(&["Foo"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Foo"), NodeKind::Definition)]);
        assert!(observe_placeholder_definition_nodes(repo, &present, &kinds).is_empty());
    }

    #[test]
    fn placeholder_def_rejects_where_body_with_leading_comment_and_fields() {
        // Regression: a genuine `structure` with a leading body comment
        // above its real fields must NOT be flagged. The pre-fix
        // `find("--")`-truncate emptiness test erased the fields and
        // force-flagged it as the placeholder, deterministically failing
        // its correspondence.
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_where_node(
            repo,
            "Pt",
            "structure Pt where",
            "  -- TODO: flesh out\n  x : Nat\n  y : Nat",
        );
        let present: BTreeSet<NodeId> = set(&["Pt"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Pt"), NodeKind::Definition)]);
        assert!(
            observe_placeholder_definition_nodes(repo, &present, &kinds).is_empty(),
            "structure with a leading comment + real fields is not a placeholder"
        );
    }

    #[test]
    fn placeholder_def_rejects_where_body_inductive_class_with_leading_comment() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_where_node(
            repo,
            "Color",
            "inductive Color where",
            "  -- the primaries\n  | red\n  | green\n  | blue",
        );
        write_where_node(
            repo,
            "Mon",
            "class Mon (α : Type) where",
            "  -- the carrier op\n  op : α → α → α",
        );
        let present: BTreeSet<NodeId> = set(&["Color", "Mon"]);
        let kinds: BTreeMap<NodeId, NodeKind> = present
            .iter()
            .map(|n| (n.clone(), NodeKind::Definition))
            .collect();
        assert!(
            observe_placeholder_definition_nodes(repo, &present, &kinds).is_empty(),
            "inductive/class with a leading comment + real constructors/fields \
             are not placeholders"
        );
    }

    #[test]
    fn placeholder_def_detects_where_body_with_only_comments_and_blanks() {
        // A literally-empty where-body — only comments and blank lines, no
        // field/constructor — IS still the deferral placeholder.
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_where_node(
            repo,
            "Foo",
            "structure Foo where",
            "  -- deferred\n\n  -- fill in later\n",
        );
        let present: BTreeSet<NodeId> = set(&["Foo"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Foo"), NodeKind::Definition)]);
        assert_eq!(
            observe_placeholder_definition_nodes(repo, &present, &kinds),
            set::<NodeId>(&["Foo"])
        );
    }

    #[test]
    fn placeholder_def_true_body_path_unchanged_by_where_fix() {
        // The `def`/`True` placeholder path is untouched by the where-branch
        // rewrite: a `True` body (with a trailing note) still flags, and a
        // real def body still does not.
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_def_node(repo, "P", "def P : Prop", "True  -- deferred");
        write_def_node(repo, "Q", "def Q (n : Nat) : Nat", "n + 1");
        let present: BTreeSet<NodeId> = set(&["P", "Q"]);
        let kinds: BTreeMap<NodeId, NodeKind> = present
            .iter()
            .map(|n| (n.clone(), NodeKind::Definition))
            .collect();
        assert_eq!(
            observe_placeholder_definition_nodes(repo, &present, &kinds),
            set::<NodeId>(&["P"]),
            "True-body def flags; real-body def does not"
        );
    }

    #[test]
    fn placeholder_def_empty_body_without_where_is_not_a_placeholder() {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        // `def Foo : Nat :=` with an empty body region is malformed, not a
        // placeholder — only the empty-`where` shape qualifies.
        write_def_node(repo, "Foo", "def Foo : Nat", "");
        let present: BTreeSet<NodeId> = set(&["Foo"]);
        let kinds: BTreeMap<NodeId, NodeKind> =
            BTreeMap::from([(NodeId::from("Foo"), NodeKind::Definition)]);
        assert!(observe_placeholder_definition_nodes(repo, &present, &kinds).is_empty());
    }

    // ------ declaration_kind classification --------------------------------

    #[test]
    fn declaration_kind_classifies_new_kinds() {
        for (head, name) in [
            ("structure Foo where", "Foo"),
            ("inductive Foo : Type where", "Foo"),
            ("class Foo where", "Foo"),
        ] {
            let content = format!("-- [TABLET NODE: {name}]\n{head}\n-- BODY\n  x : Nat\n");
            assert_eq!(
                super::declaration_kind(&content, name),
                "definition",
                "{head} should classify as definition"
            );
        }
        let inst = "-- [TABLET NODE: Foo]\ninstance Foo : Group T := by\n-- BODY\n  sorry\n";
        assert_eq!(super::declaration_kind(inst, "Foo"), "theorem_like");
    }

    #[test]
    fn scan_sorry_in_definitions_catches_sorry_in_data_declarations() {
        let s = "structure Foo where\n  x : Nat := sorry\n";
        assert!(!super::scan_sorry_in_definitions(s).is_empty());
        // An instance is proof-bearing: `sorry` in its body is allowed by
        // this scan (soundness owns it), so it is not flagged here.
        let inst = "instance Foo : Group T where\n  mul := sorry\n";
        assert!(super::scan_sorry_in_definitions(inst).is_empty());
    }

    // ---- revision_statement_edit_scope_step_result (step 11) --------------

    /// Seed a 3-node tablet (Main editable, Frozen frozen, Aux editable) and
    /// return the repo dir + the before-snapshot the validator diffs against.
    fn seed_revision_tablet() -> (tempfile::TempDir, BTreeMap<String, String>) {
        let dir = tempdir().unwrap();
        let repo = dir.path();
        write_node(
            repo,
            "Main",
            "old main tex\n",
            "theorem Main : True := sorry\n",
        );
        write_node(
            repo,
            "Frozen",
            "frozen tex\n",
            "theorem Frozen : True := trivial\n",
        );
        write_node(repo, "Aux", "aux tex\n", "theorem Aux : True := trivial\n");
        let before = snapshot_tablet_dir(repo);
        (dir, before)
    }

    #[test]
    fn revision_scope_authorized_editable_restatement_succeeds() {
        let (dir, before) = seed_revision_tablet();
        // Restate Main (authorized + editable).
        write_node(
            dir.path(),
            "Main",
            "new strengthened tex\n",
            "theorem Main : True := sorry\n",
        );
        let result = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main", "Aux"]),
            &ns(&["Frozen"]),
            &BTreeMap::from([("Frozen".into(), "frozen: preamble".to_string())]),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(result.ok, "errors: {:?}", result.errors);
    }

    #[test]
    fn revision_scope_unauthorized_edit_fails() {
        let (dir, before) = seed_revision_tablet();
        // Aux is editable but NOT in authorized_editable_nodes (reviewer only
        // authorized Main), so editing it is out of scope.
        write_node(
            dir.path(),
            "Aux",
            "aux tex changed\n",
            "theorem Aux : True := trivial\n",
        );
        let result = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main"]),
            &ns(&["Frozen"]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("Aux") && e.contains("Out-of-scope")));
    }

    #[test]
    fn revision_scope_frozen_edit_fails() {
        let (dir, before) = seed_revision_tablet();
        write_node(
            dir.path(),
            "Frozen",
            "frozen tex tampered\n",
            "theorem Frozen : True := trivial\n",
        );
        let result = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main", "Aux"]),
            &ns(&["Frozen"]),
            &BTreeMap::from([("Frozen".into(), "frozen: axioms".to_string())]),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("Frozen") && e.contains("frozen") && e.contains("frozen: axioms")));
    }

    #[test]
    fn revision_scope_frozen_deletion_fails() {
        let (dir, before) = seed_revision_tablet();
        fs::remove_file(dir.path().join("Tablet").join("Frozen.lean")).unwrap();
        fs::remove_file(dir.path().join("Tablet").join("Frozen.tex")).unwrap();
        let result = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main", "Aux"]),
            &ns(&["Frozen"]),
            &BTreeMap::from([("Frozen".into(), "frozen: carried target".to_string())]),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("deletes frozen node `Frozen`")));
    }

    /// Newborn (born in a prior RevisionStating burst, so present in the
    /// before-snapshot but never in the synthesis-time editable snapshot):
    /// with the dynamic present − frozen rule the builder places it in
    /// `authorized_editable_nodes`, and both an edit and a deletion pass this
    /// scope gate. Deleting an orphan newborn is legal; a SUPPORTED newborn's
    /// deletion legality is owned by the orphan/support clauses in
    /// `worker_normalization` (narrowed clause 1), not by this step.
    #[test]
    fn revision_scope_newborn_edit_and_deletion_within_dynamic_envelope_succeed() {
        let (dir, _seed_before) = seed_revision_tablet();
        let repo = dir.path();
        // Burst N: the newborn is stated.
        write_node(
            repo,
            "Newborn",
            "newborn tex\n",
            "theorem Newborn : True := trivial\n",
        );
        // Burst N+1 baseline: the newborn is an EXISTING node now.
        let before = snapshot_tablet_dir(repo);

        // Edit the newborn (restate it).
        write_node(
            repo,
            "Newborn",
            "newborn tex restated\n",
            "theorem Newborn : True := sorry\n",
        );
        let edited = revision_statement_edit_scope_step_result(
            repo,
            &before,
            &ns(&["Newborn"]),
            &ns(&["Frozen"]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(
            edited.ok,
            "newborn edit must pass the revision scope gate: {:?}",
            edited.errors
        );

        // Delete the newborn.
        fs::remove_file(repo.join("Tablet").join("Newborn.lean")).unwrap();
        fs::remove_file(repo.join("Tablet").join("Newborn.tex")).unwrap();
        let deleted = revision_statement_edit_scope_step_result(
            repo,
            &before,
            &ns(&["Newborn"]),
            &ns(&["Frozen"]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(
            deleted.ok,
            "newborn deletion must pass the revision scope gate: {:?}",
            deleted.errors
        );
    }

    /// The dynamic-editability fix must leave frozen enforcement
    /// byte-identical: pin the exact rejection strings for a frozen edit and
    /// a frozen deletion.
    #[test]
    fn revision_scope_frozen_rejections_are_byte_identical() {
        let (dir, before) = seed_revision_tablet();
        let reasons = BTreeMap::from([("Frozen".into(), "frozen: preamble".to_string())]);

        write_node(
            dir.path(),
            "Frozen",
            "frozen tex tampered\n",
            "theorem Frozen : True := trivial\n",
        );
        let edited = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main", "Aux"]),
            &ns(&["Frozen"]),
            &reasons,
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(
            edited.errors,
            vec![
                "Revision edit modifies frozen node `Frozen`: frozen: preamble. Frozen nodes are carried from the prior formalization and must not be edited."
                    .to_string()
            ]
        );

        fs::remove_file(dir.path().join("Tablet").join("Frozen.lean")).unwrap();
        fs::remove_file(dir.path().join("Tablet").join("Frozen.tex")).unwrap();
        let deleted = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main", "Aux"]),
            &ns(&["Frozen"]),
            &reasons,
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(
            deleted.errors,
            vec![
                "Revision edit deletes frozen node `Frozen`: frozen: preamble. Frozen nodes are carried from the prior formalization and must not be deleted."
                    .to_string()
            ]
        );
    }

    #[test]
    fn revision_scope_new_node_follows_theorem_stating_rules() {
        let (dir, before) = seed_revision_tablet();
        // Create a genuinely new node, no entry in before-snapshot.
        write_node(
            dir.path(),
            "Helper",
            "helper tex\n",
            "theorem Helper : True := trivial\n",
        );
        // allow_new_obligations=true: new node is permitted.
        let ok = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main"]),
            &ns(&["Frozen"]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(
            ok.ok,
            "new node should pass when new obligations allowed: {:?}",
            ok.errors
        );
        // allow_new_obligations=false: new node is rejected.
        let blocked = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main"]),
            &ns(&["Frozen"]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            false,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(!blocked.ok);
        assert!(blocked
            .errors
            .iter()
            .any(|e| e.contains("Helper") && e.contains("new obligations")));
    }

    #[test]
    fn revision_scope_removed_target_claim_fails() {
        let (dir, before) = seed_revision_tablet();
        // No file changes; a covering node claims a removed target.
        let claims: BTreeMap<NodeId, BTreeSet<TargetId>> =
            BTreeMap::from([("Main".into(), bts(&["thm:gone"]))]);
        let result = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main"]),
            &ns(&["Frozen"]),
            &BTreeMap::new(),
            &bts(&["thm:gone"]),
            true,
            &claims,
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(!result.ok);
        assert!(result
            .errors
            .iter()
            .any(|e| e.contains("Main") && e.contains("thm:gone") && e.contains("removed")));
    }

    #[test]
    fn revision_scope_frozen_reference_ground_delta_fails_unchanged_resend_passes() {
        // Amendment B1: a node_reference_grounds DELTA on a frozen node
        // is rejected during RevisionStating; a byte-identical re-send
        // of the unchanged set passes (full-set-replace semantics).
        let (dir, before) = seed_revision_tablet();
        let ref_id = RefPaperId::from("smith2020");
        let current: BTreeMap<NodeId, BTreeSet<RefPaperId>> =
            BTreeMap::from([("Frozen".into(), BTreeSet::from([ref_id.clone()]))]);

        // Unchanged re-send of the frozen node's exact current set: OK.
        let resend = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main", "Aux"]),
            &ns(&["Frozen"]),
            &BTreeMap::from([("Frozen".into(), "frozen: carried".to_string())]),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &current,
            &current,
        );
        assert!(resend.ok, "unchanged re-send must pass: {:?}", resend.errors);

        // Dropping the claim (empty replacement set) is a delta: reject.
        let dropped = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main", "Aux"]),
            &ns(&["Frozen"]),
            &BTreeMap::from([("Frozen".into(), "frozen: carried".to_string())]),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::from([("Frozen".into(), BTreeSet::new())]),
            &current,
        );
        assert!(!dropped.ok);
        assert!(dropped.errors.iter().any(|e| {
            e.contains("node_reference_grounds")
                && e.contains("Frozen")
                && e.contains("frozen: carried")
        }));

        // Adding a claim to a claim-free frozen node is likewise a delta.
        let added = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main", "Aux"]),
            &ns(&["Frozen"]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::from([("Frozen".into(), BTreeSet::from([ref_id.clone()]))]),
            &BTreeMap::new(),
        );
        assert!(!added.ok);

        // Deltas on NON-frozen nodes are not this step's business.
        let non_frozen = revision_statement_edit_scope_step_result(
            dir.path(),
            &before,
            &ns(&["Main", "Aux"]),
            &ns(&["Frozen"]),
            &BTreeMap::new(),
            &BTreeSet::new(),
            true,
            &BTreeMap::new(),
            &BTreeMap::from([("Main".into(), BTreeSet::from([ref_id]))]),
            &BTreeMap::new(),
        );
        assert!(non_frozen.ok, "errors: {:?}", non_frozen.errors);
    }

    fn bts(items: &[&str]) -> BTreeSet<TargetId> {
        items
            .iter()
            .map(|s| TargetId::from((*s).to_string()))
            .collect()
    }
}

#[cfg(test)]
mod dotted_decl_fingerprint_tests {
    use super::*;

    #[test]
    fn dotted_declaration_node_is_definition_like_under_sanitized_stem() {
        // Node stems sanitize namespace dots to underscores; the empty-tex
        // Lean fallback must match `BiasedFp.zero_pow2` against node
        // `BiasedFp_zero_pow2`. Regression for the dec2flt fingerprint
        // zeroing of proofs citing dotted-decl model constants.
        let lean = "namespace dec2flt\n\ndef BiasedFp.zero_pow2 (p : I32) : Result BiasedFp :=\n-- BODY\n  ok {}\n";
        assert!(lean_node_is_definition_like(lean, "BiasedFp_zero_pow2"));
        assert!(lean_node_is_definition_like(lean, "BiasedFp.zero_pow2"));
        assert!(!lean_node_is_definition_like(lean, "SomeOtherNode"));
    }

    #[test]
    fn dotted_declaration_name_passes_observation_name_checks() {
        // dec2flt requests 952/953 (2026-07-04): the acceptance walk's
        // declaration-name checks rejected baseline node
        // `FiniteDecimal_value` because its principal declaration is the
        // dotted `FiniteDecimal.value`. Every name-equality site must use
        // the sanitized-stem predicate the name-parity rule documents.
        let lean = "-- [TABLET NODE: FiniteDecimal_value]\n\
                    noncomputable def FiniteDecimal.value (d : FiniteDecimal) : ℝ :=\n-- BODY\n  0\n";
        let declared = declaration_name(lean);
        assert_eq!(declared, "FiniteDecimal.value");
        assert!(trellis_kernel::filespec::decl_name_matches_node(
            &declared,
            "FiniteDecimal_value"
        ));
        assert!(!trellis_kernel::filespec::decl_name_matches_node(
            &declared,
            "FiniteDecimal"
        ));
    }
}
